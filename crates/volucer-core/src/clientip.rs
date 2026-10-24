//! Resolving a client's address when a reverse proxy is in front of the server.
//!
//! # The defect this exists to fix
//!
//! Behind nginx every connection arrives from nginx. So the server's peer address is the *proxy's*
//! address, and two things that read it are then wrong in a way that looks like correct behaviour:
//!
//! - The access log records nginx as the client, so "who requested this" has no answer in the one
//!   place an operator looks for it.
//! - Anything keyed on the address — a rate limiter, an IP allow-list, an abuse heuristic — keys on
//!   nginx, which means it limits *nginx* rather than the client and cannot tell two clients apart
//!   at all.
//!
//! Neither is visible without the other. A single-machine test passes, the log looks plausible, and
//! the failure only appears as a limit that throttles every user at once.
//!
//! # Why this is a trust decision and not a header read
//!
//! `X-Forwarded-For` is a request header. Any client can send one, and a client that sends
//! `X-Forwarded-For: 10.0.0.1` is asserting an address it has no business asserting. So the rule is
//! not "read the header" — it is **"read the header only if the peer is a proxy this deployment
//! named"**:
//!
//! 1. If the peer's address is not inside a configured `trusted_subnets` entry, the peer address is
//!    the answer and no header is consulted at all. An untrusted client's claim is not a weaker
//!    answer, it is no answer.
//! 2. Only if the peer *is* trusted are the configured headers read, in order, and the **first one
//!    that yields a parsable address** wins.
//!
//! The empty list is therefore the safe default: with no `trusted_subnets`, every request resolves
//! to its own peer, which is exactly what the server does today. Turning the feature on is a
//! statement that a specific proxy is in front, and that statement is the *only* thing that makes a
//! forwarded header mean anything.
//!
//! # Order matters, and the default order is the conservative one
//!
//! `X-Forwarded-For` is *appended to* by every hop, so its first element is the original client
//! **only when exactly one proxy is in the chain**. Behind a CDN in front of nginx, the first
//! element is whatever the client sent and the CDN appended to. The default chain tries `Forwarded`
//! (RFC 7239, the standardized spelling) and then the `X-Forwarded-For` / `X-Real-Ip` family in the
//! order RoadRunner uses, and a deployment that knows its own topology should say so explicitly
//! with `trusted_headers`.
//!
//! # What this does not do
//!
//! It does not walk the list *backwards* from the peer, which is what a fully correct
//! rightmost-untrusted algorithm does. That algorithm is the one you need when the chain mixes
//! trusted and untrusted hops, and it needs every hop's address rather than one. This resolves the
//! single case a deployment actually configures — one layer of proxies whose addresses are known —
//! and stops rather than guessing when the chain is longer.

use std::net::IpAddr;

/// One header to consult for a client address, and how to read a value out of it.
///
/// The parse is chosen **once, from the header's name, at configuration time** rather than on the
/// request path. A `match` on the name per request would be the same decision made repeatedly for
/// an answer that cannot change, and it would put a string comparison in front of the address a
/// rate limiter needs.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Source {
    /// RFC 7239 `Forwarded: for=...`, which may carry more parameters and quoting than the
    /// `X-Forwarded-*` family.
    Forwarded,
    /// A comma-separated list, appended to by each hop. The left-most element is what a
    /// single-proxy deployment means.
    XForwardedFor,
    /// A single address, taken as written. `X-Real-Ip`, `True-Client-Ip`, `Cf-Connecting-Ip`, and
    /// any custom header an operator names.
    Verbatim,
}

impl Source {
    /// The parser a header's name calls for.
    ///
    /// Canonicalization already happened, so this compares like with like. The two structured
    /// headers are matched by their standard spelling; **everything else is taken verbatim**, which
    /// is what makes a custom header usable without a code change.
    fn for_header(canonical: &str) -> Self {
        match canonical.to_ascii_lowercase().as_str() {
            "forwarded" => Source::Forwarded,
            "x-forwarded-for" => Source::XForwardedFor,
            _ => Source::Verbatim,
        }
    }
}

/// A header to consult, resolved to the shape its value has.
#[derive(Debug, Clone, PartialEq, Eq)]
struct HeaderSource {
    /// The name to look up, as configured. Canonicalization is the front-end's business: the HTTP
    /// side has a case-insensitive map, and the FastCGI side has `HTTP_`-prefixed keys.
    name: String,
    source: Source,
}

/// The header chain used when a deployment names none.
///
/// RoadRunner's order, and it is a *default* rather than a recommendation: it puts `Forwarded`
/// first because it is the standardized spelling and the least ambiguous, then the
/// `X-Forwarded-For` family. `X-Forwarded-For` is second rather than first deliberately — it is the
/// one header a CDN will append the client's own claim to, so a deployment behind one should set
/// `trusted_headers` explicitly rather than rely on this.
pub const DEFAULT_TRUSTED_HEADERS: &[&str] = &[
    "Forwarded",
    "X-Forwarded-For",
    "X-Real-Ip",
    "True-Client-Ip",
    "Cf-Connecting-Ip",
];

/// An address range, the unit a trust decision is made in.
///
/// Hand-rolled rather than taken from a crate: the whole of it is one mask and one comparison per
/// address family, and it is the *security-relevant* comparison in this module. A dependency here
/// would be a dependency whose absence from `Cargo.lock` is currently a feature, and it would put
/// the trust check behind a version bump.
///
/// **Public because a site's address binding is the same comparison.** "Whose address is this" and
/// "which local address did this connection arrive on" are both `contains` against a list of ranges,
/// and a second implementation of it would be a second chance to get the masking wrong in the one
/// place where getting it wrong serves an internal site to the public internet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cidr {
    base: IpAddr,
    /// Prefix length: how many leading bits must match.
    bits: u8,
}

impl Cidr {
    /// Parse `10.0.0.0/8`, `127.0.0.1`, `::1`, or `fd00::/8`.
    ///
    /// A bare address is a range of one, which is how a deployment names a single proxy without
    /// writing `/32` or `/128`.
    ///
    /// **Host bits beyond the prefix are accepted and masked.** `10.1.2.3/8` means `10.0.0.0/8`,
    /// which is what the operator wrote the `/8` for. Refusing it would be defensible, but the
    /// masking direction is the one the prefix itself asks for, and the alternative — trusting
    /// `10.1.2.3` while appearing to trust a `/8` — is the failure that would actually be dangerous.
    pub fn parse(spec: &str) -> Result<Self, String> {
        let spec = spec.trim();
        if spec.is_empty() {
            return Err("an empty address range".to_string());
        }

        let (address, bits) = match spec.split_once('/') {
            Some((address, length)) => {
                let length: u8 = length
                    .trim()
                    .parse()
                    .map_err(|_| format!("{spec:?} has a prefix length that is not a number"))?;
                (address.trim(), Some(length))
            }
            None => (spec, None),
        };

        let base: IpAddr = address
            .parse()
            .map_err(|_| format!("{spec:?} does not begin with an IP address"))?;

        let max = if base.is_ipv4() { 32 } else { 128 };
        let bits = bits.unwrap_or(max);
        if bits > max {
            return Err(format!(
                "{spec:?} has a prefix length of {bits}, but an {} address has at most {max} bits",
                if base.is_ipv4() { "IPv4" } else { "IPv6" }
            ));
        }

        Ok(Cidr { base, bits })
    }

    /// The prefix length this range was written with.
    ///
    /// Exposed for the one caller that needs to compare two *matching* ranges rather than to
    /// test membership: an address-only site is chosen by the narrowest range that admits the
    /// connection, which is the same "most specific wins" rule the host table uses for wildcards.
    pub fn bits(&self) -> u8 {
        self.bits
    }

    /// Whether `address` is inside this range.
    ///
    /// A v4 range never contains a v6 address and vice versa, and the match says so rather than
    /// silently comparing a masked `u32` against a masked `u128`.
    ///
    /// **A v4-mapped v6 address is not a v4 address here**, and that is deliberate rather than an
    /// oversight: `::ffff:10.0.0.1` is a v6 peer, so a `10.0.0.0/8` binding does not admit it. A
    /// socket bound to `::` accepts v4 clients as mapped addresses on some platforms, so a
    /// deployment that binds an internal site to a v4 range and listens on `[::]` gets a refusal it
    /// can see rather than a site it did not mean to expose. Binding the listener to `0.0.0.0` (or
    /// naming the mapped range explicitly) is the fix, and the documentation says so.
    pub fn contains(&self, address: IpAddr) -> bool {
        match (self.base, address) {
            (IpAddr::V4(base), IpAddr::V4(address)) => {
                let mask = v4_mask(self.bits);
                u32::from(base) & mask == u32::from(address) & mask
            }
            (IpAddr::V6(base), IpAddr::V6(address)) => {
                let mask = v6_mask(self.bits);
                u128::from(base) & mask == u128::from(address) & mask
            }
            _ => false,
        }
    }
}

/// A bitmask with the leading `bits` set.
///
/// The `bits == 0` arm is not defensive: `u32::MAX << 32` is undefined for a shift of the full
/// width, and a `/0` is a range a deployment can legitimately write (`0.0.0.0/0`).
fn v4_mask(bits: u8) -> u32 {
    if bits == 0 {
        0
    } else {
        u32::MAX << (32 - bits)
    }
}

fn v6_mask(bits: u8) -> u128 {
    if bits == 0 {
        0
    } else {
        u128::MAX << (128 - bits)
    }
}

/// Where a front-end looks up a request header.
///
/// A trait rather than a concrete type so the two front-ends can each answer in their own shape —
/// `hyper`'s case-insensitive map on the HTTP side, the `HTTP_`-prefixed CGI environment on the
/// FastCGI side — while **the resolution rule itself exists once**, with one set of tests. The
/// alternative is two implementations of a trust check, which is two chances to get it wrong in a
/// way only one protocol shows.
pub trait HeaderLookup {
    /// The first value of `name`, if the request has one and it is valid text.
    ///
    /// A header with an invalid byte is treated as absent, which is the same rule request ids
    /// follow: it cannot be an address, so "absent" is the honest answer rather than a refusal.
    fn header(&self, name: &str) -> Option<&str>;
}

/// How this deployment decides which address is the client's.
///
/// Cheap to clone — two small vectors — because a front-end holds one per generation and the request
/// path reads it through an `Arc`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClientIpPolicy {
    /// Ranges whose peers may set a client address through a header. **Empty means the feature is
    /// off**, and that is the default.
    trusted: Vec<Cidr>,
    /// The header chain, in order, resolved once at configuration time.
    ///
    /// **Materialised here rather than at lookup.** An earlier version built the default chain inside
    /// `resolve`, which meant five `String` allocations per request on the path a rate limiter
    /// depends on — for an answer that cannot change. The chain is fixed for the life of the policy,
    /// so it is built where a policy is built.
    ///
    /// An empty `trusted` makes this irrelevant, so it is left empty there rather than filled in.
    headers: Vec<HeaderSource>,
}

impl ClientIpPolicy {
    /// Build a policy from configured ranges and header names.
    ///
    /// Returns the reason on the first malformed entry rather than skipping it. A trust list is one
    /// of the few settings where a typo is a security decision, and a range that was meant to be
    /// narrowed but did not parse must not leave the server running with it absent.
    pub fn new(subnets: &[String], headers: &[String]) -> Result<Self, String> {
        let mut trusted = Vec::with_capacity(subnets.len());
        for spec in subnets {
            trusted.push(Cidr::parse(spec)?);
        }

        // A blank or duplicated name is dropped rather than refused: neither changes which header
        // is consulted, so refusing would be a startup failure for a file that means what it says.
        // Names are canonicalized so that `x-real-ip` and `X-Real-Ip` are one entry.
        let mut resolved: Vec<HeaderSource> = Vec::with_capacity(headers.len());
        for name in headers {
            let name = name.trim();
            if name.is_empty() {
                continue;
            }
            let canonical = canonical_header_name(name);
            if resolved.iter().any(|source| source.name == canonical) {
                continue;
            }
            let source = Source::for_header(&canonical);
            resolved.push(HeaderSource {
                name: canonical,
                source,
            });
        }

        // **An empty list is not "no headers", it is the default chain**, and a deployment that named
        // none has said nothing about which to use. Built unconditionally rather than lazily: it is
        // five strings once, and the alternative is paying for them per request.
        if resolved.is_empty() {
            resolved = DEFAULT_TRUSTED_HEADERS
                .iter()
                .map(|name| HeaderSource {
                    name: (*name).to_string(),
                    source: Source::for_header(name),
                })
                .collect();
        }

        Ok(ClientIpPolicy {
            trusted,
            headers: resolved,
        })
    }

    /// A policy that consults nothing, which is what an unconfigured deployment gets.
    pub fn off() -> Self {
        ClientIpPolicy::default()
    }

    /// Whether any range is trusted. False means every request resolves to its own peer.
    pub fn is_enabled(&self) -> bool {
        !self.trusted.is_empty()
    }

    /// Whether `address` is a peer whose forwarded headers are believed.
    pub fn trusts(&self, address: IpAddr) -> bool {
        self.trusted.iter().any(|range| range.contains(address))
    }

    /// The header names that would be consulted, for reporting.
    pub fn header_names(&self) -> Vec<&str> {
        self.headers
            .iter()
            .map(|source| source.name.as_str())
            .collect()
    }

    /// The address to treat as the client's.
    ///
    /// Three steps, in this order, and the order is the whole of the security property:
    ///
    /// 1. An untrusted peer resolves to itself, **and no header is read**. A client that sends
    ///    `X-Forwarded-For: 10.0.0.1` to a server with no trust list is not spoofing anything,
    ///    because nothing looks.
    /// 2. A trusted peer with the feature on walks the chain and takes the first header that parses.
    /// 3. A trusted peer whose headers are all absent or unparsable resolves to itself. Falling back
    ///    to the peer is the safe direction: it is a real address this server saw, where a guessed
    ///    one is an attacker's.
    pub fn resolve<H: HeaderLookup>(&self, peer: IpAddr, headers: &H) -> IpAddr {
        if !self.is_enabled() || !self.trusts(peer) {
            return peer;
        }

        for source in &self.headers {
            let Some(value) = headers.header(&source.name) else {
                continue;
            };
            if let Some(address) = source.source.extract(value) {
                return address;
            }
        }

        peer
    }
}

impl Source {
    /// The address this header's value names, if it names one.
    fn extract(&self, value: &str) -> Option<IpAddr> {
        match self {
            Source::Forwarded => forwarded_address(value),
            Source::XForwardedFor => value.split(',').next().and_then(parse_address),
            Source::Verbatim => parse_address(value),
        }
    }
}

/// The `for=` target of an RFC 7239 `Forwarded` value.
///
/// The grammar allows several comma-separated elements and several `;`-separated parameters each.
/// Only the first element is read, for the same reason `X-Forwarded-For` takes its left-most
/// address: with one proxy in front, that is the client. The `for=` token is matched
/// case-insensitively, and an IPv6 address arrives quoted because the grammar requires it.
fn forwarded_address(value: &str) -> Option<IpAddr> {
    let first = value.split(',').next()?;
    for parameter in first.split(';') {
        let (name, target) = parameter.split_once('=')?;
        if name.trim().eq_ignore_ascii_case("for") {
            return parse_address(target);
        }
    }

    None
}

/// Parse an address as a forwarded header writes one.
///
/// Four spellings appear in practice and all four are accepted, because refusing a valid one would
/// silently fall back to the proxy's address — the defect this module exists to fix:
///
/// - `203.0.113.7` — the bare IPv4 case.
/// - `2001:db8::1` — the bare IPv6 case, which the `Forwarded` grammar forbids but nginx's
///   `$remote_addr` produces.
/// - `203.0.113.7:51234` — with a port, which `$remote_addr` never has but an operator's custom
///   header may.
/// - `[2001:db8::1]:51234` and `[2001:db8::1]` — RFC 7239's quoted forms, brackets included.
fn parse_address(value: &str) -> Option<IpAddr> {
    let value = value.trim().trim_matches('"').trim();
    if value.is_empty() {
        return None;
    }

    if let Ok(address) = value.parse::<IpAddr>() {
        return Some(address);
    }

    // `[v6]` with no port: the brackets are not part of the address.
    if let Some(inner) = value
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
    {
        if let Ok(address) = inner.parse::<IpAddr>() {
            return Some(address);
        }
    }

    // Anything with a port, including `[v6]:port`. Tried last because it is the most permissive:
    // a bare address has already been handled, so reaching here means the value carries a port.
    value.parse::<std::net::SocketAddr>().ok().map(|sa| sa.ip())
}

/// `x-real-ip` and `X-Real-Ip` are one header, so a configured list holds one entry for them.
///
/// `hyper`'s header map already compares case-insensitively, so the canonical form is not strictly
/// needed for the lookup. It is needed for the *deduplication* above and for the FastCGI side, where
/// the environment key is built by uppercasing the name with `-` replaced by `_`.
fn canonical_header_name(name: &str) -> String {
    name.split('-')
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => first.to_ascii_uppercase().to_string() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join("-")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// A header lookup over a list, for tests.
    struct Headers(HashMap<String, String>);

    impl Headers {
        fn new(pairs: &[(&str, &str)]) -> Self {
            Headers(
                pairs
                    .iter()
                    .map(|(name, value)| (name.to_ascii_lowercase(), (*value).to_string()))
                    .collect(),
            )
        }
    }

    impl HeaderLookup for Headers {
        fn header(&self, name: &str) -> Option<&str> {
            self.0.get(&name.to_ascii_lowercase()).map(String::as_str)
        }
    }

    fn ip(spec: &str) -> IpAddr {
        spec.parse().expect("the test names a valid address")
    }

    fn policy(subnets: &[&str], headers: &[&str]) -> ClientIpPolicy {
        ClientIpPolicy::new(
            &subnets.iter().map(|s| (*s).to_string()).collect::<Vec<_>>(),
            &headers.iter().map(|s| (*s).to_string()).collect::<Vec<_>>(),
        )
        .expect("the test's configuration is valid")
    }

    #[test]
    fn a_range_contains_its_own_addresses_and_not_its_neighbours() {
        let range = Cidr::parse("10.0.0.0/8").unwrap();
        assert!(range.contains(ip("10.0.0.1")));
        assert!(range.contains(ip("10.255.255.255")));
        assert!(!range.contains(ip("11.0.0.1")));
        assert!(!range.contains(ip("9.255.255.255")));
        // A v4 range must not contain a v6 address, however the masks would compare.
        assert!(!range.contains(ip("::1")));
        assert!(!range.contains(ip("::ffff:10.0.0.1")));
    }

    #[test]
    fn a_full_width_range_does_not_shift_by_its_whole_width() {
        // `u32::MAX << 32` is undefined, so `/0` and `/32` are the two ends that would panic or
        // silently match everything if the mask were computed naively.
        let everything = Cidr::parse("0.0.0.0/0").unwrap();
        assert!(everything.contains(ip("1.2.3.4")));
        assert!(everything.contains(ip("255.255.255.255")));

        let one = Cidr::parse("192.0.2.7/32").unwrap();
        assert!(one.contains(ip("192.0.2.7")));
        assert!(!one.contains(ip("192.0.2.8")));

        let v6_everything = Cidr::parse("::/0").unwrap();
        assert!(v6_everything.contains(ip("2001:db8::1")));
    }

    #[test]
    fn a_bare_address_is_a_range_of_one() {
        let one = Cidr::parse("127.0.0.1").unwrap();
        assert!(one.contains(ip("127.0.0.1")));
        assert!(!one.contains(ip("127.0.0.2")));

        let one_v6 = Cidr::parse("::1").unwrap();
        assert!(one_v6.contains(ip("::1")));
        assert!(!one_v6.contains(ip("::2")));
    }

    #[test]
    fn host_bits_past_the_prefix_are_masked_rather_than_kept() {
        // The value `10.1.2.3/8` must trust the whole of `10.0.0.0/8`. Trusting only `10.1.2.3`
        // while the file says `/8` is the failure that would be dangerous, so this pins the
        // direction of the rounding.
        let range = Cidr::parse("10.1.2.3/8").unwrap();
        assert!(
            range.contains(ip("10.0.0.1")),
            "the /8 was meant to be a /8"
        );
        assert!(range.contains(ip("10.1.2.3")));
    }

    #[test]
    fn a_malformed_range_is_refused_with_a_reason() {
        for bad in ["", "   ", "not-an-address", "10.0.0.0/", "10.0.0.0/abc"] {
            assert!(Cidr::parse(bad).is_err(), "{bad:?} must be refused");
        }

        // A prefix length that is wider than the address family is its own mistake, and the
        // message has to say which family, because that is what the operator got wrong.
        let too_wide = Cidr::parse("10.0.0.0/33").unwrap_err();
        assert!(too_wide.contains("IPv4"), "got: {too_wide}");
        let too_wide_v6 = Cidr::parse("::1/129").unwrap_err();
        assert!(too_wide_v6.contains("IPv6"), "got: {too_wide_v6}");
    }

    #[test]
    fn an_untrusted_peer_is_never_re_derived_from_a_header() {
        // The security property, and the reason the feature is off by default. A client that
        // sends its own `X-Forwarded-For` is asserting an address, and the assertion must be
        // ignored because nothing about the connection says it is a proxy.
        let headers = Headers::new(&[("X-Forwarded-For", "10.0.0.1")]);

        let off = ClientIpPolicy::off();
        assert_eq!(off.resolve(ip("203.0.113.7"), &headers), ip("203.0.113.7"));

        let on = policy(&["10.0.0.0/8"], &[]);
        assert_eq!(on.resolve(ip("203.0.113.7"), &headers), ip("203.0.113.7"));

        // And the control: the same header from a *trusted* peer is the answer, so the two
        // assertions above are about trust rather than about the header being unreadable.
        assert_eq!(on.resolve(ip("10.1.2.3"), &headers), ip("10.0.0.1"));
    }

    #[test]
    fn the_chain_is_walked_in_order_and_the_first_parsable_value_wins() {
        let headers = Headers::new(&[
            // `Forwarded` present but with no `for=`, so it must not stop the walk.
            ("Forwarded", "proto=https;host=example.test"),
            ("X-Forwarded-For", "203.0.113.7, 10.0.0.1"),
        ]);
        let resolved = policy(&["10.0.0.0/8"], &[]).resolve(ip("10.0.0.5"), &headers);
        assert_eq!(
            resolved,
            ip("203.0.113.7"),
            "with no `for=`, the next header is read"
        );

        // Order is the configured order, not the canonical one: naming only the second header
        // must skip the first entirely.
        let only_xff = policy(&["10.0.0.0/8"], &["X-Forwarded-For"]);
        assert_eq!(
            only_xff.resolve(ip("10.0.0.5"), &headers),
            ip("203.0.113.7")
        );

        let only_forwarded = policy(&["10.0.0.0/8"], &["Forwarded"]);
        assert_eq!(
            only_forwarded.resolve(ip("10.0.0.5"), &headers),
            ip("10.0.0.5"),
            "a chain of one must not fall through to a header that was not configured"
        );
    }

    #[test]
    fn a_trusted_peer_with_no_usable_header_stays_itself() {
        // The fallback direction matters: an absent or unparsable header must leave a real address
        // this server saw, never a guess.
        let empty = Headers::new(&[]);
        let policy = policy(&["10.0.0.0/8"], &[]);
        assert_eq!(policy.resolve(ip("10.0.0.5"), &empty), ip("10.0.0.5"));

        let junk = Headers::new(&[("X-Forwarded-For", "unknown, 10.0.0.9")]);
        assert_eq!(
            policy.resolve(ip("10.0.0.5"), &junk),
            ip("10.0.0.5"),
            "a value that is not an address is absent, not a claim"
        );
    }

    #[test]
    fn the_four_spellings_a_forwarded_address_arrives_in_are_all_read() {
        // Each of these is a shape a real proxy produces. A refusal here is silent, because the
        // server falls back to the proxy's address — which is the defect this module exists for.
        let cases = [
            ("203.0.113.7", "203.0.113.7"),
            ("2001:db8::1", "2001:db8::1"),
            ("203.0.113.7:51234", "203.0.113.7"),
            ("[2001:db8::1]:51234", "2001:db8::1"),
            ("[2001:db8::1]", "2001:db8::1"),
            ("  203.0.113.7  ", "203.0.113.7"),
            ("\"203.0.113.7\"", "203.0.113.7"),
        ];

        for (written, expected) in cases {
            let headers = Headers::new(&[("X-Real-Ip", written)]);
            let resolved =
                policy(&["10.0.0.0/8"], &["X-Real-Ip"]).resolve(ip("10.0.0.5"), &headers);
            assert_eq!(resolved, ip(expected), "{written:?}");
        }
    }

    #[test]
    fn an_rfc7239_forwarded_value_yields_its_for_target() {
        let cases = [
            ("for=203.0.113.7", "203.0.113.7"),
            ("for=203.0.113.7;proto=https", "203.0.113.7"),
            // The grammar requires the address to be quoted when it carries a port or is IPv6.
            ("for=\"[2001:db8::1]:51234\"", "2001:db8::1"),
            ("for=\"203.0.113.7:51234\";proto=http", "203.0.113.7"),
            // Case-insensitive, because the parameter name is a token.
            ("FOR=203.0.113.7", "203.0.113.7"),
            // Several elements: the first is the one a single-proxy deployment means.
            ("for=203.0.113.7, for=10.0.0.1", "203.0.113.7"),
        ];

        for (written, expected) in cases {
            let headers = Headers::new(&[("Forwarded", written)]);
            let resolved =
                policy(&["10.0.0.0/8"], &["Forwarded"]).resolve(ip("10.0.0.5"), &headers);
            assert_eq!(resolved, ip(expected), "{written:?}");
        }

        // `by=` is a different parameter and must not be mistaken for `for=`.
        let headers = Headers::new(&[("Forwarded", "by=10.0.0.5;for=203.0.113.7")]);
        let resolved = policy(&["10.0.0.0/8"], &["Forwarded"]).resolve(ip("10.0.0.5"), &headers);
        assert_eq!(resolved, ip("203.0.113.7"));
    }

    #[test]
    fn a_blank_or_duplicated_header_name_is_dropped_rather_than_refused() {
        let named = policy(
            &["10.0.0.0/8"],
            &["", "  ", "x-real-ip", "X-Real-Ip", "X-Forwarded-For"],
        );
        // Deduplicated case-insensitively and in the order given, with the blanks gone.
        assert_eq!(
            named.header_names(),
            vec!["X-Real-Ip", "X-Forwarded-For"],
            "a file that means what it says must not fail to start"
        );

        // An empty list is not "no headers" — it is "the default chain", because a deployment that
        // named none has said nothing about which to use.
        let defaults = policy(&["10.0.0.0/8"], &[]);
        assert_eq!(defaults.header_names(), DEFAULT_TRUSTED_HEADERS.to_vec());
    }

    #[test]
    fn a_custom_header_is_read_verbatim() {
        // The escape hatch: a CDN with its own spelling needs no code change, and its value is a
        // single address rather than a list.
        let headers = Headers::new(&[("X-Cdn-Client", "203.0.113.7")]);
        let resolved = policy(&["10.0.0.0/8"], &["X-Cdn-Client"]).resolve(ip("10.0.0.5"), &headers);
        assert_eq!(resolved, ip("203.0.113.7"));
    }

    #[test]
    fn the_feature_is_off_until_a_range_is_named() {
        assert!(!ClientIpPolicy::off().is_enabled());
        assert!(!policy(&[], &["X-Forwarded-For"]).is_enabled());
        assert!(policy(&["10.0.0.0/8"], &[]).is_enabled());

        // And naming headers with no ranges changes nothing, which is the property that makes the
        // default safe: a half-configured deployment is an off one rather than a trusting one.
        let headers = Headers::new(&[("X-Forwarded-For", "10.0.0.1")]);
        let half = policy(&[], &["X-Forwarded-For"]);
        assert_eq!(half.resolve(ip("10.0.0.5"), &headers), ip("10.0.0.5"));
    }
}
