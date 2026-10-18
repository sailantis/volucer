//! Rate limiting: one token bucket per caller.
//!
//! # Why this exists at all
//!
//! Everything else in this server is something nginx could do instead, and §9 of `docs/plugins.md`
//! says so per row. A rate limit is the exception: nginx's `limit_req` is a fixed-window counter
//! that cannot be keyed on a header, cannot be keyed on a value another module resolved, and
//! cannot be changed without a reload. Behind a reverse proxy, this is the only lever an
//! unauthenticated endpoint has.
//!
//! # The algorithm, and the one it is not
//!
//! A **token bucket that refills continuously**: a bucket holds up to `burst` tokens, gains
//! `rate / interval` tokens per second, and a request costs one. This is what `golang.org/x/time/rate`
//! implements, which is what RoadRunner's plugin uses.
//!
//! The alternative — a counter that resets at a window boundary — is what nginx's `limit_req` does,
//! and it has a defect that only appears under a limit a client is trying to defeat: a client can
//! spend a whole window's allowance at `t = interval - 1ms` and the next whole allowance at
//! `t = interval + 1ms`, so the *instantaneous* rate is twice the configured one. A bucket has no
//! boundary to exploit: it cannot hold more than `burst`, and it refills at a fixed rate.
//!
//! # Why the key space is an enum of three values
//!
//! `Key` is not a `String`. Two of its three variants exist because the alternative allocates on a
//! path an unauthenticated client drives:
//!
//! - `Address(IpAddr)` — no `to_string()` per request, which the `clientip` module already refuses
//!   to do for the same reason.
//! - `Header(u64)` — **hashed, not stored.** A header value is attacker-chosen and unbounded, so
//!   storing it verbatim would let an unauthenticated client make the process hold an arbitrary
//!   amount of memory per bucket. RoadRunner hashes with SHA-256; this uses `std`'s
//!   [`RandomState`], which is a seeded SipHash and therefore *also* not precomputable by a client
//!   trying to fill the table deliberately. At [`DEFAULT_MAX_ENTRIES`] the chance that two of
//!   10 000 64-bit digests collide is on the order of 10⁻¹¹, and a collision merges two callers'
//!   buckets rather than corrupting anything — a cost worth paying for bounded memory with no
//!   dependency.
//!
//! # What this deliberately does not do
//!
//! - **No timer, and no background thread.** The sweep that reclaims idle buckets runs when the
//!   table is *full*, which is the only moment it changes an answer. RoadRunner sweeps on a
//!   one-minute timer instead; a timer needs a shutdown handle and a thread, and it does work at
//!   moments when the work cannot affect anything.
//! - **No eviction of a bucket that is not full.** A full bucket is indistinguishable from an
//!   absent one — both hand out `burst` tokens from now — so reclaiming it is lossless. A depleted
//!   bucket is not: dropping it would hand the caller a fresh allowance, which is exactly what the
//!   limit exists to refuse. So a full table of *depleted* buckets refuses the newcomer rather than
//!   admitting it, and the refusal is a 429 rather than a 503 (see [`Reason`]).

use std::collections::hash_map::RandomState;
use std::collections::HashMap;
use std::hash::BuildHasher;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// How many identities one process tracks before it starts refusing new ones.
pub const DEFAULT_MAX_ENTRIES: usize = 10_000;

/// How a caller is identified.
///
/// The three names are RoadRunner's (`global`, `ip`, `header`), because an operator migrating an
/// `http.rate_limiter` block already has them in mind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum KeyMode {
    /// One bucket for the whole process. Useful as a global circuit breaker, and useless for
    /// telling two clients apart — which is the whole point of the other two.
    Global,
    /// One bucket per client address, **as `[server.proxy]` resolves it**.
    ///
    /// That resolution is not incidental: behind nginx every connection arrives from nginx, so a
    /// limit keyed on the *peer* throttles the proxy and cannot tell two clients apart at all. This
    /// is the reason `[server.proxy]` was built before this.
    ///
    /// **The default**, because "limit each caller" is what a rate limit means when nothing else is
    /// said.
    #[default]
    Ip,
    /// One bucket per value of a configured header.
    ///
    /// **Not an authenticated identity.** A client chooses the value, so a client that varies it
    /// gets a fresh bucket per request until the table is full. Its use is a value the *proxy*
    /// sets — an API key it vouched for, a tenant it routed — not one the client asserts.
    Header,
}

impl KeyMode {
    /// The word this mode is written as in a log line, so a report cannot spell it differently.
    pub fn as_str(self) -> &'static str {
        match self {
            KeyMode::Global => "global",
            KeyMode::Ip => "ip",
            KeyMode::Header => "header",
        }
    }
}

/// The same word [`KeyMode::as_str`] returns.
///
/// Implemented so a `thiserror` message can name the mode (`the {key} rate limit …`) without the
/// error type holding a `&'static str` that a caller had to remember to fill in.
impl std::fmt::Display for KeyMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Who a request is, for the purpose of spending a token.
///
/// Borrowed rather than owned so the caller does not build a `String` for the policy to look up:
/// the two variants that carry data carry it in the shape the key map is indexed by.
#[derive(Debug, Clone, Copy)]
pub enum Identity<'a> {
    Global,
    Address(IpAddr),
    /// The header's value, trimmed. Empty is the caller's problem — it is a missing identity, not
    /// an identity, and `check` refuses it rather than bucketing every such request together.
    Header(&'a str),
}

/// Why a request was refused.
///
/// The distinction is for the operator, and it decides nothing about the status code: both are a
/// 429, because in both cases the truth is "this caller has no allowance right now". Reporting the
/// second as a 503 — which is what RoadRunner does — would tell a load balancer the *server* is
/// unhealthy and take the node out of rotation for a condition one misbehaving client caused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// This identity's own bucket is empty.
    Exhausted,
    /// The table is full of buckets that are not full, so a new identity cannot be tracked.
    TableFull,
}

/// What a request costs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Allowed,
    /// Refused, and how long until this identity could try again.
    ///
    /// **A whole number of seconds, at least one**, because that is what `Retry-After` means (RFC
    /// 9110 §10.2.3 allows a delay of zero but advises against it, and a zero here would invite an
    /// immediate retry against a bucket that is still empty).
    Limited {
        retry_after: Duration,
        reason: Reason,
    },
}

/// One identity's bucket.
#[derive(Debug, Clone, Copy)]
struct Bucket {
    /// Tokens available at `last`. Fractional, because a refill is continuous.
    tokens: f64,
    /// When `tokens` was last brought up to date.
    last: Instant,
}

impl Bucket {
    /// This bucket's tokens at `now`, capped at `burst`.
    ///
    /// A capped bucket is *not* written back with its capped value, because that would throw away
    /// the time it has been idle: a bucket left alone for an hour would then refill only from the
    /// moment it was read. Capping on read, and storing the uncapped arithmetic's result, is why
    /// `last` is only moved when a token is actually spent.
    fn tokens_at(&self, now: Instant, per_second: f64, burst: f64) -> f64 {
        let elapsed = now.saturating_duration_since(self.last).as_secs_f64();
        (self.tokens + elapsed * per_second).min(burst)
    }
}

/// A policy: the shape of the limit, and the buckets that carry it out.
///
/// One per process, shared by every front-end through an `Arc`, for the reason the site table is:
/// a deployment behind nginx runs both front-ends, so two limiters would give a client twice the
/// configured allowance depending on which protocol received the request.
#[derive(Debug)]
pub struct RateLimitPolicy {
    key: KeyMode,
    /// The canonical header name, when `key` is [`KeyMode::Header`].
    header: Option<String>,
    /// Tokens added per second. `rate / interval`, resolved once.
    per_second: f64,
    /// The capacity of one bucket, and the number of tokens a new one starts with.
    burst: f64,
    max_entries: usize,
    /// The hasher for header-mode keys. One instance for the policy's life, because
    /// [`RandomState`] guarantees only that *the same instance* produces the same digests — a
    /// fresh one per request would give one client a new bucket every time.
    hasher: RandomState,
    buckets: Mutex<HashMap<Key, Bucket>>,
}

/// The key a bucket is stored under.
///
/// Deliberately not `String`: see the module documentation. `Global` is a unit variant so the
/// global case still has one map rather than a special-cased field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Key {
    Global,
    Address(IpAddr),
    Header(u64),
}

impl RateLimitPolicy {
    /// Build a policy from configuration, or say why it is not usable.
    ///
    /// Returns the reason on the first invalid value rather than clamping it. A rate limit is one of
    /// the few settings where a silent correction is worse than a refusal: a `rate` read as `1` when
    /// the file said `0` would either throttle every user or none, and nobody would know which.
    ///
    /// Every limit is checked here rather than in `Config::validate` alone so that a policy built by
    /// a test or by a future caller is subject to the same rules — the one place, following
    /// [`crate::clientip::ClientIpPolicy::new`].
    pub fn new(
        key: KeyMode,
        header: Option<&str>,
        rate: u32,
        interval: Duration,
        burst: Option<u32>,
        max_entries: Option<usize>,
    ) -> Result<Self, String> {
        if rate == 0 {
            return Err("rate must be at least 1".to_string());
        }
        if interval.is_zero() {
            return Err("interval must be greater than zero".to_string());
        }

        // **`burst` defaults to `rate`, not to 1.** RoadRunner defaults it to one, which makes
        // `rate = 60, interval = 1m` mean "one request, then one per second" — a limit nobody
        // writing that line meant. Defaulting to `rate` is the ordinary reading of "60 requests per
        // minute": the whole allowance may be spent at once, and it comes back over the minute.
        let burst = burst.unwrap_or(rate);
        if burst == 0 {
            return Err(
                "burst must be at least 1; a bucket of zero capacity refuses every request, \
                 including the first"
                    .to_string(),
            );
        }

        let max_entries = max_entries.unwrap_or(DEFAULT_MAX_ENTRIES);
        if max_entries == 0 {
            return Err(
                "max_entries must be at least 1; the table starts empty, so zero would refuse \
                 every request that is not global"
                    .to_string(),
            );
        }

        let header = match key {
            KeyMode::Header => {
                let name = header.map(str::trim).unwrap_or_default();
                if name.is_empty() {
                    return Err(
                        "key = \"header\" needs a header name in `header`, because the name is \
                         the identity"
                            .to_string(),
                    );
                }
                if !is_token(name) {
                    return Err(format!(
                        "{name:?} is not a valid header name: a header name is a run of \
                         !#$%&'*+-.^_`|~ and alphanumerics"
                    ));
                }
                Some(canonical_header_name(name))
            }
            // A header named beside a key mode that does not read one is an error rather than
            // ignored: RoadRunner's schema makes the same call, and for the same reason — a
            // deployment that wrote it believes it is limiting that header.
            KeyMode::Global | KeyMode::Ip => {
                if header.is_some_and(|name| !name.trim().is_empty()) {
                    return Err(
                        "`header` is only read when key = \"header\"; beside another key it would \
                         appear to limit something it does not"
                            .to_string(),
                    );
                }
                None
            }
        };

        Ok(RateLimitPolicy {
            key,
            header,
            per_second: f64::from(rate) / interval.as_secs_f64(),
            burst: f64::from(burst),
            max_entries,
            hasher: RandomState::new(),
            buckets: Mutex::new(HashMap::new()),
        })
    }

    /// The key mode, for reporting.
    pub fn key(&self) -> KeyMode {
        self.key
    }

    /// The header consulted in [`KeyMode::Header`], for reporting.
    pub fn header(&self) -> Option<&str> {
        self.header.as_deref()
    }

    /// Tokens per second, for reporting.
    pub fn per_second(&self) -> f64 {
        self.per_second
    }

    /// One bucket's capacity, for reporting.
    pub fn burst(&self) -> u32 {
        // Lossless: `burst` was a `u32` in `new`.
        self.burst as u32
    }

    /// The most identities this process tracks, for reporting.
    pub fn max_entries(&self) -> usize {
        self.max_entries
    }

    /// How many buckets are live, for tests and for a shutdown report.
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// Whether no bucket has been created yet.
    pub fn is_empty(&self) -> bool {
        self.lock().is_empty()
    }

    /// Spend a token, or say how long to wait.
    ///
    /// `now` is a parameter rather than read here so a test can drive the arithmetic without
    /// sleeping, which is the difference between a suite that finishes and one that takes a minute.
    ///
    /// The lock is held for the length of one map operation. Nothing awaits inside it — the callers
    /// are a blocking listener and a check made before the first `await` on the HTTP path — so the
    /// `std` mutex is the right tool and not an async one.
    pub fn check(&self, identity: Identity<'_>, now: Instant) -> Verdict {
        let mut buckets = self.lock();

        // A missing or blank header value is **a new identity per request**, and bucketing them
        // together under one empty key would make the deployment's limit a global one that
        // silently looked per-caller. Refusing is what RoadRunner does (a 400) and it is the
        // honest answer: the configuration said this header names the caller, and it does not.
        let key = match identity {
            Identity::Global => Key::Global,
            Identity::Address(address) => Key::Address(unmap(address)),
            Identity::Header(value) => {
                let value = value.trim();
                if value.is_empty() {
                    // `burst` is finite, so reporting the token interval is the truthful wait.
                    return Verdict::Limited {
                        retry_after: self.retry_after(0.0),
                        reason: Reason::Exhausted,
                    };
                }
                Key::Header(self.digest(value))
            }
        };

        if let Some(bucket) = buckets.get_mut(&key) {
            let tokens = bucket.tokens_at(now, self.per_second, self.burst);
            if tokens >= 1.0 {
                // Written back as spent-and-dated, which is the only write that matters: it is
                // what stops a bucket being refilled twice from the same elapsed time.
                *bucket = Bucket {
                    tokens: tokens - 1.0,
                    last: now,
                };
                return Verdict::Allowed;
            }

            // **Not written back**, so the bucket keeps counting from `last` and the wait below is
            // computed from the same arithmetic the next request will use.
            return Verdict::Limited {
                retry_after: self.retry_after(tokens),
                reason: Reason::Exhausted,
            };
        }

        // A new identity, and no room for it.
        if buckets.len() >= self.max_entries {
            // Reclaim the buckets that cost nothing to lose: a full bucket hands out `burst`
            // tokens from now whether it exists or not, so deleting it is indistinguishable from
            // never having created it. RoadRunner's timer-driven sweep uses exactly this test;
            // the difference is that this one runs when it can matter.
            buckets.retain(|_, bucket| {
                bucket.tokens_at(now, self.per_second, self.burst) < self.burst
            });
            if buckets.len() >= self.max_entries {
                // Everything left is depleted, so there is nothing lossless to evict and admitting
                // this caller would mean handing it an allowance while the callers already tracked
                // wait. Refusing is the truthful answer, and it is a 429 rather than a 503 because
                // the condition is "this process is at its tracking limit", not "this server is
                // unhealthy".
                return Verdict::Limited {
                    retry_after: self.retry_after(0.0),
                    reason: Reason::TableFull,
                };
            }
        }

        // A new bucket starts **full**, which is what makes the sweep above lossless.
        buckets.insert(
            key,
            Bucket {
                tokens: self.burst - 1.0,
                last: now,
            },
        );
        Verdict::Allowed
    }

    /// The `Retry-After` a bucket holding `tokens` earns.
    ///
    /// Rounded up to whole seconds and floored at one: a client that is told "0" retries
    /// immediately against a bucket that is still empty, and a sub-second delay is a number
    /// `Retry-After` cannot carry. The token is **not** reserved by the refusal, so several clients
    /// told the same thing race for it — which is correct, because only one can have it.
    fn retry_after(&self, tokens: f64) -> Duration {
        let missing = (1.0 - tokens).max(0.0);
        let seconds = (missing / self.per_second).ceil();
        // `per_second` is positive and finite, so this is a finite non-negative number; the
        // `.max(1.0)` is the floor, and the clamp keeps a pathological configuration (rate 1 per
        // year) from overflowing the conversion.
        Duration::from_secs(seconds.max(1.0).min(f64::from(u32::MAX)) as u64)
    }

    /// The map this policy indexes, locked.
    ///
    /// A poisoned mutex is recovered rather than propagated: the map is a cache of counters with
    /// no invariant that a panicking thread could have half-updated — every operation on it is one
    /// insert or one in-place assignment — so refusing every future request because one thread
    /// panicked elsewhere in the process would convert an unrelated failure into a total one.
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<Key, Bucket>> {
        self.buckets.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The 64-bit digest a header value is stored under.
    fn digest(&self, value: &str) -> u64 {
        self.hasher.hash_one(value)
    }
}

/// A v4-mapped v6 address as the v4 address it names.
///
/// Without this, one client arrives as two identities: a connection over a dual-stack socket
/// reports `::ffff:203.0.113.7` while a forwarded header names `203.0.113.7`, so the same caller
/// would get one bucket per protocol — twice the allowance, and a table filling twice as fast.
///
/// RoadRunner does the same (`ip.Unmap()`) for the same reason. It is not the `clientip` module's
/// job because that module answers "which of these headers is the client", and this is "which key
/// is this address"; the mapping is a property of the identifier, not of the resolution.
fn unmap(address: IpAddr) -> IpAddr {
    match address {
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None => address,
        },
        IpAddr::V4(_) => address,
    }
}

/// Whether `name` is a valid HTTP header name (RFC 9110 §5.1 `token`).
///
/// Checked rather than passed through, because the name is looked up in a header map that compares
/// case-insensitively: a name with a space in it would silently match nothing and the deployment
/// would limit every request under one empty key.
fn is_token(name: &str) -> bool {
    !name.is_empty()
        && name.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'!' | b'#'
                        | b'$'
                        | b'%'
                        | b'&'
                        | b'\''
                        | b'*'
                        | b'+'
                        | b'-'
                        | b'.'
                        | b'^'
                        | b'_'
                        | b'`'
                        | b'|'
                        | b'~'
                )
        })
}

/// `x-client-id` and `X-Client-Id` are one header, so the policy stores one spelling.
///
/// The same canonicalisation `clientip` uses, for the same reason: the two front-ends look the name
/// up in different structures (`hyper`'s header map on the HTTP side, the `HTTP_`-prefixed CGI
/// environment on the FastCGI side), so the policy holds the form both build their key from.
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

    fn ip(spec: &str) -> IpAddr {
        spec.parse().expect("the test names a valid address")
    }

    /// A policy of `rate` tokens per `interval`, with an explicit burst.
    fn policy(rate: u32, interval_secs: u64, burst: Option<u32>) -> RateLimitPolicy {
        RateLimitPolicy::new(
            KeyMode::Ip,
            None,
            rate,
            Duration::from_secs(interval_secs),
            burst,
            None,
        )
        .expect("the test's configuration is valid")
    }

    fn limited(verdict: Verdict) -> (Duration, Reason) {
        match verdict {
            Verdict::Limited {
                retry_after,
                reason,
            } => (retry_after, reason),
            Verdict::Allowed => panic!("expected a refusal, got an allowed request"),
        }
    }

    #[test]
    fn a_bucket_admits_its_burst_and_then_refuses() {
        // The property everything else rests on: `burst` requests succeed from a cold start, and
        // the next one does not. A test that only asserted the first half would pass against a
        // limiter that never refuses anything.
        let limiter = policy(1, 60, Some(3));
        let now = Instant::now();
        let client = Identity::Address(ip("203.0.113.7"));

        for n in 0..3 {
            assert_eq!(
                limiter.check(client, now),
                Verdict::Allowed,
                "request {n} is inside the burst"
            );
        }
        let (retry_after, reason) = limited(limiter.check(client, now));
        assert_eq!(reason, Reason::Exhausted);
        // One token per 60s, so the wait is a whole minute rather than a second.
        assert_eq!(retry_after, Duration::from_secs(60));
    }

    #[test]
    fn a_token_comes_back_at_the_configured_rate_and_not_before() {
        let limiter = policy(2, 1, Some(1));
        let start = Instant::now();
        let client = Identity::Address(ip("203.0.113.7"));

        assert_eq!(limiter.check(client, start), Verdict::Allowed);
        assert_eq!(
            limited(limiter.check(client, start)).0,
            Duration::from_secs(1),
            "the wait is one interval's worth of tokens"
        );

        // Half a token is not a token. Checking a moment short of the refill must still refuse,
        // which is the assertion a limiter built on `>` rather than `>=` would fail.
        assert!(matches!(
            limiter.check(client, start + Duration::from_millis(499)),
            Verdict::Limited { .. }
        ));
        assert_eq!(
            limiter.check(client, start + Duration::from_millis(500)),
            Verdict::Allowed
        );
    }

    #[test]
    fn refilling_is_capped_at_the_burst_rather_than_banked() {
        // A bucket left alone for an hour must not hold an hour's tokens: the whole reason `burst`
        // exists is to bound how much of a limit can be spent at once. The test spends the burst,
        // waits far longer than a full refill, and asserts the answer is `burst` and not more.
        let limiter = policy(1, 1, Some(2));
        let start = Instant::now();
        let client = Identity::Address(ip("203.0.113.7"));

        assert_eq!(limiter.check(client, start), Verdict::Allowed);
        assert_eq!(limiter.check(client, start), Verdict::Allowed);
        assert!(matches!(
            limiter.check(client, start),
            Verdict::Limited { .. }
        ));

        let much_later = start + Duration::from_secs(3600);
        assert_eq!(limiter.check(client, much_later), Verdict::Allowed);
        assert_eq!(limiter.check(client, much_later), Verdict::Allowed);
        assert!(
            matches!(limiter.check(client, much_later), Verdict::Limited { .. }),
            "an hour idle must not buy a third request"
        );
    }

    #[test]
    fn two_clients_do_not_share_a_bucket() {
        // The distinction the whole feature exists for. A limiter keyed on the peer — which is
        // what this server saw before `[server.proxy]` — would fail this by throttling both.
        let limiter = policy(1, 60, Some(1));
        let now = Instant::now();

        assert_eq!(
            limiter.check(Identity::Address(ip("203.0.113.7")), now),
            Verdict::Allowed
        );
        assert!(matches!(
            limiter.check(Identity::Address(ip("203.0.113.7")), now),
            Verdict::Limited { .. }
        ));
        assert_eq!(
            limiter.check(Identity::Address(ip("203.0.113.8")), now),
            Verdict::Allowed,
            "a second client has its own bucket"
        );
        assert_eq!(limiter.len(), 2);
    }

    #[test]
    fn a_v4_mapped_address_shares_the_v4_bucket() {
        // One client, one allowance. Without `unmap`, a dual-stack socket and a forwarded header
        // would describe the same caller as two identities.
        let limiter = policy(1, 60, Some(1));
        let now = Instant::now();

        assert_eq!(
            limiter.check(Identity::Address(ip("203.0.113.7")), now),
            Verdict::Allowed
        );
        assert!(
            matches!(
                limiter.check(Identity::Address(ip("::ffff:203.0.113.7")), now),
                Verdict::Limited { .. }
            ),
            "the mapped spelling is the same client"
        );
        assert_eq!(limiter.len(), 1);
    }

    #[test]
    fn a_full_table_reclaims_only_the_buckets_that_cost_nothing_to_lose() {
        // The two halves of the sweep, and the second is the one worth having: a bucket that has
        // refilled to `burst` is indistinguishable from an absent one, so reclaiming it loses
        // nothing. A bucket that is still depleted is a caller who must keep waiting.
        let limiter = RateLimitPolicy::new(
            KeyMode::Ip,
            None,
            1,
            Duration::from_secs(1),
            Some(1),
            Some(2),
        )
        .expect("valid");
        let start = Instant::now();

        let spent = Identity::Address(ip("203.0.113.7"));
        assert_eq!(limiter.check(spent, start), Verdict::Allowed);
        assert!(matches!(
            limiter.check(spent, start),
            Verdict::Limited { .. }
        ));

        // The second identity arrives while the first is depleted: there is room for it, so it is
        // tracked, and the table is now full of two buckets of which one is depleted.
        let other = Identity::Address(ip("203.0.113.8"));
        assert_eq!(limiter.check(other, start), Verdict::Allowed);
        assert_eq!(limiter.len(), 2);

        // A third identity now finds a full table. `spent` has not refilled and `other` has spent
        // its only token, so neither is reclaimable — and the answer must be a refusal rather than
        // a silent eviction that would hand one of them a fresh allowance.
        let (_, reason) = limited(limiter.check(Identity::Address(ip("203.0.113.9")), start));
        assert_eq!(reason, Reason::TableFull);
        assert_eq!(limiter.len(), 2, "a refusal must not grow the table");

        // An hour later both have refilled, so the same arrival reclaims one of them and succeeds.
        let later = start + Duration::from_secs(3600);
        assert_eq!(
            limiter.check(Identity::Address(ip("203.0.113.9")), later),
            Verdict::Allowed,
            "the sweep runs when the table is full, and a full bucket is free to reclaim"
        );
    }

    #[test]
    fn a_blank_header_value_is_refused_rather_than_bucketed_together() {
        // The failure this prevents is a limit that looks per-caller and is global: every request
        // with an empty header would share one bucket, so the first client to arrive would spend
        // everyone's allowance.
        let limiter = RateLimitPolicy::new(
            KeyMode::Header,
            Some("X-Client-Id"),
            u32::MAX,
            Duration::from_secs(1),
            None,
            None,
        )
        .expect("valid");

        let now = Instant::now();
        assert!(matches!(
            limiter.check(Identity::Header("   "), now),
            Verdict::Limited { .. }
        ));
        assert!(
            limiter.is_empty(),
            "nothing may be stored for a blank value"
        );
    }

    #[test]
    fn header_mode_keys_on_the_value_rather_than_storing_it() {
        let limiter = RateLimitPolicy::new(
            KeyMode::Header,
            Some("x-client-id"),
            1,
            Duration::from_secs(60),
            Some(1),
            None,
        )
        .expect("valid");
        let now = Instant::now();

        assert_eq!(limiter.header(), Some("X-Client-Id"), "canonicalised once");
        assert_eq!(
            limiter.check(Identity::Header("tenant-a"), now),
            Verdict::Allowed
        );
        assert!(matches!(
            limiter.check(Identity::Header("tenant-a"), now),
            Verdict::Limited { .. }
        ));
        // Case-sensitive values, so a differently-spelled value is a different identity. That is
        // RoadRunner's rule too, and it is the right one: a proxy that sets this header sets one
        // spelling.
        assert_eq!(
            limiter.check(Identity::Header("Tenant-A"), now),
            Verdict::Allowed
        );
        // And a long value costs one entry, not its own length: the digest is what is stored.
        let long = "x".repeat(1_000_000);
        assert_eq!(
            limiter.check(Identity::Header(&long), now),
            Verdict::Allowed
        );
        assert_eq!(limiter.len(), 3);
    }

    #[test]
    fn a_global_limit_is_one_bucket_for_everyone() {
        let limiter = RateLimitPolicy::new(
            KeyMode::Global,
            None,
            1,
            Duration::from_secs(60),
            Some(1),
            None,
        )
        .expect("valid");
        let now = Instant::now();

        assert_eq!(limiter.check(Identity::Global, now), Verdict::Allowed);
        assert!(matches!(
            limiter.check(Identity::Global, now),
            Verdict::Limited { .. }
        ));
        assert_eq!(limiter.len(), 1);
    }

    #[test]
    fn the_default_burst_is_the_rate_rather_than_one() {
        // Pinned because it is a deliberate departure from RoadRunner, whose default of 1 turns
        // "60 per minute" into "one, then one per second" — a limit nobody writing that line meant.
        let limiter = policy(5, 1, None);
        assert_eq!(limiter.burst(), 5);

        let now = Instant::now();
        for n in 0..5 {
            assert_eq!(
                limiter.check(Identity::Global, now),
                Verdict::Allowed,
                "request {n} is inside the default burst"
            );
        }
        assert!(matches!(
            limiter.check(Identity::Global, now),
            Verdict::Limited { .. }
        ));
    }

    #[test]
    fn an_unusable_configuration_is_refused_with_a_reason() {
        let cases: [(u32, u64, Option<u32>, Option<usize>); 4] = [
            (0, 1, None, None),
            (1, 0, None, None),
            (1, 1, Some(0), None),
            (1, 1, None, Some(0)),
        ];
        for (rate, interval, burst, max_entries) in cases {
            let err = RateLimitPolicy::new(
                KeyMode::Ip,
                None,
                rate,
                Duration::from_secs(interval),
                burst,
                max_entries,
            )
            .expect_err("must be refused");
            assert!(!err.is_empty(), "a refusal has to say what to fix");
        }

        // A header key with no name, and a name with no header key, are both mistakes rather than
        // defaults — each is a line that believes it limits something.
        assert!(
            RateLimitPolicy::new(KeyMode::Header, None, 1, Duration::from_secs(1), None, None)
                .is_err()
        );
        assert!(RateLimitPolicy::new(
            KeyMode::Header,
            Some("not a header"),
            1,
            Duration::from_secs(1),
            None,
            None
        )
        .is_err());
        assert!(RateLimitPolicy::new(
            KeyMode::Ip,
            Some("X-Client-Id"),
            1,
            Duration::from_secs(1),
            None,
            None
        )
        .is_err());
    }

    #[test]
    fn a_retry_after_is_never_zero_and_never_fractional() {
        // `Retry-After` is whole seconds (RFC 9110 §10.2.3). A zero would let a client retry
        // immediately against the same empty bucket, so the floor is one.
        let limiter = policy(1000, 1, Some(1));
        let now = Instant::now();
        let client = Identity::Address(ip("203.0.113.7"));

        assert_eq!(limiter.check(client, now), Verdict::Allowed);
        // A thousand tokens a second means the next one is a millisecond away, which must still be
        // reported as a whole second rather than as zero.
        let (retry_after, _) = limited(limiter.check(client, now));
        assert_eq!(retry_after, Duration::from_secs(1));
        assert!(retry_after.as_secs() >= 1);
    }
}
