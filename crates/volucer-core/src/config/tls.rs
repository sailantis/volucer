//! TLS, and the ACME orders that can replace a configured certificate.

use super::*;

/// TLS settings.
///
/// **The presence of this table is what turns TLS on.** There is deliberately no `enabled`
/// key: a flag and a certificate are two statements about the same thing, and two statements
/// can disagree — so a table without a usable certificate is an error instead.
///
/// A relative `certificate`/`key` is resolved against **the directory holding the config
/// file**, which is a deliberate exception to `document_root`'s rule (that one is relative to
/// the process's working directory). A certificate beside `volucer.toml` is the common
/// case, and resolving it against wherever the process happened to start would be a footgun
/// with a security consequence. `volucer check` prints the path it resolved.
///
/// **Both keys may be left out when an ACME authority will issue** — see [`AcmeConfig`]. The server
/// then generates a self-signed pair in memory for the first handshakes, and issuance replaces it.
/// The pair is not written to disk: it is a placeholder, so there is nothing to clean up and no
/// private key lying around for a certificate nothing ever trusted.
///
/// Absent is *only* allowed together with ACME, because otherwise the server would have nothing to
/// present and every handshake that is not an ACME challenge would fail — the mistake the
/// certificate-checked-after-the-port-was-taken defect was
/// about, and the reason this table exists rather than an `enabled` flag.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct TlsConfig {
    /// PEM certificate chain, leaf first.
    ///
    /// Absent means "generate one", which requires an ACME authority to replace it.
    #[serde(default)]
    pub certificate: Option<PathBuf>,

    /// PEM private key (PKCS#8, PKCS#1 or SEC1).
    ///
    /// Absent means "generate one", which requires an ACME authority to replace it.
    #[serde(default)]
    pub key: Option<PathBuf>,

    /// Lowest protocol version accepted: `"1.2"` (the default) or `"1.3"`.
    ///
    /// Stated as a string rather than a number because `1.2` and `1.3` are names of
    /// protocol versions, not decimals, and `1.10` must never be expressible.
    #[serde(default)]
    pub min_version: MinTlsVersion,

    /// ALPN protocols to advertise, in preference order.
    ///
    /// `http/1.1` must not be dropped: a client that cannot do `h2` must still connect, and
    /// a listener that advertises only `h2` fails that handshake rather than downgrading.
    #[serde(default = "default_alpn")]
    pub alpn: Vec<String>,

    /// Also answer cleartext HTTP on the same port as TLS.
    ///
    /// Off by default, and it should stay off unless something needs it. It exists for a
    /// deployment behind an upstream proxy that forwards cleartext to `:443`, and for a
    /// migration period — not as a convenience. Accepting cleartext on a port that clients
    /// believe is encrypted is a security decision, so it is explicit, it is logged loudly at
    /// startup, and it is an error beside `https` (that port is already dedicated, so the
    /// sniff would have nothing to decide).
    ///
    /// A **detection, not a downgrade**: the first byte picks which of two paths a connection
    /// takes (a TLS record always begins `0x16`; a plaintext request begins an ASCII method
    /// letter). A client that offers TLS and fails the handshake still fails — there is no
    /// fallback to cleartext on a handshake error.
    #[serde(default)]
    pub plain: bool,
}

/// The `[sites.<name>.tls]` override.
///
/// Only the certificate and key can differ per site, because SNI is the whole reason a
/// per-site table exists: the name is claimed before any request is read, so a per-site
/// setting here can only be about *which certificate is presented*. `min_version`, `alpn`
/// and `plain` are properties of the socket the connection arrived on, so a per-site value
/// would describe something the socket does not do — setting one is an error rather than a
/// silently ignored key.
///
/// Both keys may be left out when the site has its own `[sites.<name>.acme]`, for the same reason
/// [`TlsConfig`] allows it: what is generated in memory is replaced by what is issued.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SiteTlsConfig {
    /// PEM certificate chain for this site's hosts, leaf first.
    #[serde(default)]
    pub certificate: Option<PathBuf>,

    /// PEM private key for this site's hosts.
    #[serde(default)]
    pub key: Option<PathBuf>,
}

/// `[server.acme]`: obtain this deployment's certificates automatically (RFC 8555).
///
/// # Why this is a table and not a boolean
///
/// Every key here has a value a deployment has to decide, and there is no safe default for any of
/// them except the authority — where the safe default is *staging*, spelled as an enum rather than
/// a `bool` so that `true` cannot quietly mean "issue a real certificate for a name somebody owns"
/// at a call site a reader skims. Presence of the table is what turns ACME on, the same rule
/// `[server.tls]` follows: a flag and a set of names are two statements about one thing, and they
/// can disagree.
///
/// # What it needs from `[server.tls]`
///
/// A **certificate to serve until an order completes** — and `[server.tls].certificate`/`.key` may
/// be left out for that, in which case one is generated in memory and never written down. This
/// replaces the earlier answer, which required the *operator* to name a
/// self-signed pair. The invariant — a TLS listener needs something to present — is
/// what matters, and generating satisfies it without asking for work that is thrown away moments
/// later. The pair is a placeholder, so making it durable would leave a private key on disk for a
/// certificate nothing ever trusted.
///
/// So ACME does not contradict "a TLS listener needs a certificate" — it changes *which*
/// certificate, which is precisely what the reload phase made replaceable.
///
/// # Why this type is separate from [`SiteAcmeConfig`]
///
/// The global table's `domains` is **required** and a site's is **optional**, and that difference is
/// the whole reason there are two types. Deriving names from a site's `hosts` is what an operator
/// expects and is safe within one site; deriving across a whole deployment is exactly where a
/// wildcard host bites, because a wildcard cannot be a certificate name. One type with a
/// conditional rule would make "is `domains` allowed to be empty?" depend on where the table
/// appeared, which is the kind of rule a reader has to be told rather than see.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct AcmeConfig {
    /// Whether this table issues at all.
    ///
    /// Defaults to `true`, so a table that does not mention it means what it always meant. `false`
    /// makes the table **defaults only**: its `authority`, `contact` and `cache` still apply to
    /// every site that inherits them, and nothing is ordered for `domains`.
    ///
    /// # Why a key rather than deleting the table
    ///
    /// Removing the table is how a deployment turns ACME off, and it is not always what an operator
    /// wants: staging a contact and an authority, or parking an order during an incident, would mean
    /// deleting settings that have to be typed back correctly afterwards. A key that says "not now"
    /// leaves them in place.
    ///
    /// # Why it is not inherited
    ///
    /// A site's `enabled` is its own. If a site inherited `false`, turning the deployment's orders
    /// off would silently stop every site's too — and the sites are the ones with unrelated
    /// hostnames, which is exactly the deployment that has more than one order. So an operator who
    /// wants silence everywhere says so wherever it is wanted.
    ///
    /// A **disabled server table still issues for a site that asks**, because a site's table is its
    /// own order and its own account; the deployment's `enabled` is a statement about the
    /// deployment's names, not about the deployment's settings.
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// Which authority to talk to.
    ///
    /// Defaults to staging, and production must be named. The reason is a rate limit as much as a
    /// safety one: a production account has weekly limits, and a first deployment that reached
    /// production by omission would spend them on its own mistakes.
    #[serde(default)]
    pub authority: Authority,

    /// The names to issue for.
    ///
    /// **Required here, and stated rather than derived.** The deployment-wide table covers hosts
    /// from several sites, and a site's hosts may be a wildcard pattern, which cannot be a name on a
    /// certificate — so a derivation that looked safe for one site would be wrong exactly for the
    /// deployments that need issuance most. A site's own table *may* omit this and derive from its
    /// own `hosts`; see [`SiteAcmeConfig::domains`].
    ///
    /// An empty list is an error rather than "derive from the sites": a mistake that would
    /// otherwise become an ACME request for an empty identifier set.
    #[serde(default)]
    pub domains: Vec<String>,

    /// A contact address for the account, e.g. `mailto:ops@example.com`.
    ///
    /// Optional in the protocol — Let's Encrypt does not require one — and **strongly recommended**
    /// here, because it is the only channel a CA has for warning that renewals are failing for this
    /// account. A deployment with no contact finds out from an expired certificate.
    ///
    /// This is also what the account is keyed by: `rustls-acme`'s directory cache names the account
    /// file after a hash of the contact list and the directory URL, so two tables that list the same
    /// contacts on the same authority **share one account**, and two that differ create two. The
    /// second is invisible in the configuration, so it is reported at startup.
    #[serde(default)]
    pub contact: Vec<String>,

    /// Where the account and issued certificates are cached.
    ///
    /// Relative paths resolve against the config file's directory, the same rule `certificate` and
    /// `key` follow (and deliberately **not** `document_root`'s rule, which is relative to the
    /// process's working directory).
    ///
    /// Not optional in effect: the engine requires a cache, because without one every restart
    /// re-issues and the rate limits are per-week. The default sits beside the configuration file,
    /// which is where an operator looks for it.
    #[serde(default = "default_acme_cache")]
    pub cache: PathBuf,
}

/// `[sites.<name>.acme]`: this site's certificates, obtained automatically.
///
/// The same table as [`AcmeConfig`] with one difference and one addition:
///
/// - `authority`, `contact` and `cache` are all **optional**, so a site inherits the deployment's
///   values and states only what is its own. Inheriting the cache is what makes the account shared:
///   the account file is keyed by contact and authority, not by site.
/// - `domains` may be **omitted**, in which case it is derived from the site's own
///   [`hosts`](SiteConfig::hosts), dropping any wildcard pattern.
///
/// The site's certificate **replaces** the deployment's for the hosts it names. A site that states
/// its own [`tls`](SiteConfig::tls) is left alone entirely — naming a certificate is a decision not
/// to have one issued.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct SiteAcmeConfig {
    /// Whether this site's table issues; absent means `true`.
    ///
    /// **Not inherited from `[server.acme]`**, which is the one place this table departs from the
    /// usual rule; see [`AcmeConfig::enabled`]. `false` keeps the table and its settings and issues
    /// nothing for this site, so the site falls back to the deployment's certificate.
    #[serde(default)]
    pub enabled: Option<bool>,

    /// Which authority to talk to; absent inherits `[server.acme]`.
    #[serde(default)]
    pub authority: Option<Authority>,

    /// The names to issue for; absent or empty derives them from this site's `hosts`.
    ///
    /// Deriving drops every wildcard pattern, because `*.example.com` cannot appear on a
    /// certificate — the CA offers `dns-01` for that and this server does not speak it. What is
    /// dropped is **logged at startup**, since a certificate that does not cover a name the site
    /// answers to is a browser warning waiting for someone to notice.
    ///
    /// A derivation that leaves nothing is an **error** rather than an empty order, and the error
    /// quotes the wildcards and says to list the real names.
    #[serde(default)]
    pub domains: Vec<String>,

    /// A contact address for the account; absent inherits `[server.acme]`.
    #[serde(default)]
    pub contact: Option<Vec<String>>,

    /// Where the account and issued certificates are cached; absent inherits `[server.acme]`.
    #[serde(default)]
    pub cache: Option<PathBuf>,
}

impl SiteAcmeConfig {
    /// Whether this table's settings are honoured at all.
    ///
    /// Separate from [`SiteAcmeConfig::issues`] because the two are different questions: a disabled
    /// site still **inherits** the deployment's `authority` and `cache` for its own use, and only
    /// stops ordering. Conflating them would make `enabled = false` mean "ignore this table", which
    /// is what deleting it means.
    pub fn is_enabled(&self) -> bool {
        self.enabled.unwrap_or(true)
    }

    /// Whether this site orders a certificate of its own.
    ///
    /// Also false when the site names its own certificate — see [`SiteConfig::tls`] — because naming
    /// one is a decision not to have one issued. `Config::validate` refuses the two together, so a
    /// deployment cannot reach that state, but the readers downstream do not have to know that.
    pub fn issues(&self) -> bool {
        self.is_enabled()
    }

    /// The names to issue for: the site's own, or derived from its hosts.
    ///
    /// Returns the names and the wildcard patterns that were dropped, so a caller can report them
    /// rather than let the omission pass unmentioned.
    pub fn resolve_domains(&self, hosts: &[String]) -> (Vec<String>, Vec<String>) {
        if !self.domains.is_empty() {
            return (self.domains.clone(), Vec::new());
        }
        let mut names = Vec::new();
        let mut dropped = Vec::new();
        for host in hosts {
            // A wildcard is a *pattern*, not a name, and `validate_acme_domain` refuses it. Split
            // rather than filtered, so the caller can say which ones were left out.
            if host.contains('*') {
                dropped.push(host.clone());
                continue;
            }
            if !names.contains(host) {
                names.push(host.clone());
            }
        }
        (names, dropped)
    }

    /// This site's contact list: its own, or the deployment's.
    ///
    /// Inheriting rather than defaulting is deliberate, and it is what makes the **account**
    /// shared: `rustls-acme` names the account file after a hash of the contact list and the
    /// directory URL, so a site that inherits creates no second account.
    pub fn resolved_contact<'a>(&'a self, global: &'a Option<AcmeConfig>) -> &'a [String] {
        match &self.contact {
            Some(contact) => contact,
            None => global.as_ref().map_or(&[], |acme| acme.contact.as_slice()),
        }
    }

    /// This site's cache directory: its own, or the deployment's.
    ///
    /// Inheriting is what lets several sites' certificates live in one directory. The file names
    /// are hashes of `(domains, directory_url)` for a certificate and `(contact, directory_url)`
    /// for the account, so two sites never collide on a certificate and *do* share an account when
    /// their contacts match.
    pub fn resolved_cache(&self, global: &Option<AcmeConfig>) -> PathBuf {
        match &self.cache {
            Some(cache) => cache.clone(),
            None => global
                .as_ref()
                .map_or_else(default_acme_cache, |acme| acme.cache.clone()),
        }
    }

    /// This site's authority: its own, or the deployment's.
    pub fn resolved_authority(&self, global: &Option<AcmeConfig>) -> Authority {
        match self.authority {
            Some(authority) => authority,
            None => global
                .as_ref()
                .map_or(Authority::default(), |acme| acme.authority),
        }
    }
}

/// The rules every ACME table shares, whoever wrote it.
///
/// Factored out so the deployment's table and a site's table cannot drift: the names, the contact
/// format and the cache are the same questions in both, and the errors name the table they belong
/// to through `label`.
pub(super) fn validate_acme_table(
    label: &str,
    domains: &[String],
    contact: &[String],
    cache: &Path,
) -> Result<()> {
    for domain in domains {
        validate_acme_domain(domain).map_err(|err| match err {
            Error::Config(message) => Error::Config(message.replace("server.acme", label)),
            other => other,
        })?;
    }

    // Duplicates are not harmful to the order — the CA ignores them — but they are always a
    // mistake, and the mistake they usually are is a copy-paste that left one name out. Saying so
    // is cheaper than the operator discovering the missing name from a browser warning.
    let mut seen: BTreeMap<&str, usize> = BTreeMap::new();
    for (index, domain) in domains.iter().enumerate() {
        if let Some(first) = seen.insert(domain.as_str(), index) {
            return Err(Error::Config(format!(
                "{label}.domains lists {domain:?} twice (positions {first} and {index}); the CA \
                 would ignore the repeat, so this is nearly always a name that was meant to be \
                 here instead"
            )));
        }
    }

    for address in contact {
        // `mailto:` is the only scheme Let's Encrypt accepts, and a bare address — the mistake an
        // operator makes because every other tool takes one — would be sent as an unknown URI
        // scheme and rejected by the CA. Catching it here means the error names the fix.
        if !address.starts_with("mailto:") {
            return Err(Error::Config(format!(
                "{label}.contact {address:?} must be a `mailto:` URI, e.g. \
                 \"mailto:ops@example.com\""
            )));
        }
        if address.len() <= "mailto:".len() {
            return Err(Error::Config(format!(
                "{label}.contact is an empty `mailto:` URI; give an address"
            )));
        }
    }

    if cache.as_os_str().is_empty() {
        return Err(Error::Config(format!(
            "{label}.cache is empty; name a directory, or remove the key to get the default \
             beside this file"
        )));
    }

    Ok(())
}

/// Which ACME authority to use.
///
/// A named enum rather than a `bool`, and the reason is the consequence of getting it wrong: a real
/// certificate for a name somebody owns, at a rate limit that is per-week and per-account.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Authority {
    /// Let's Encrypt staging: the real protocol, certificates no browser trusts, and generous
    /// limits. The default.
    #[default]
    Staging,
    /// Let's Encrypt production. Reached only by naming it.
    Production,
}

impl Authority {
    /// The name as it is written in configuration, for logs and diagnostics.
    pub fn as_str(self) -> &'static str {
        match self {
            Authority::Staging => "staging",
            Authority::Production => "production",
        }
    }
}

/// The default ACME cache directory, beside the configuration file.
fn default_acme_cache() -> PathBuf {
    PathBuf::from("acme-cache")
}

/// Validate one ACME domain name.
///
/// Stricter than `validate_host_pattern` on purpose, because the two rules are about different
/// things. A *host pattern* may be a wildcard (`*.example.com`) and is matched against a `Host`
/// header, port and all. An ACME *identifier* is a name that goes on a certificate, so:
///
/// - a wildcard is refused, because TLS-ALPN-01 cannot validate one (RFC 8737 requires the
///   identifier to equal the SNI name, and `*.example.com` is not an SNI name a client sends) and
///   the crate's flat domain list could not express one anyway;
/// - a port is refused, because it is not part of a name;
/// - an empty label is refused, because `a..b` is a typo that a CA rejects after a round trip.
///
/// The last is the reason this exists at all: every rule here is something the CA would reject
/// *after* an order had been started, and an error that arrives before the network is free.
pub(super) fn validate_acme_domain(domain: &str) -> Result<()> {
    if domain.is_empty() {
        return Err(Error::Config(
            "server.acme.domains contains an empty name".to_string(),
        ));
    }
    if domain.starts_with("*.") || domain.contains('*') {
        return Err(Error::Config(format!(
            "server.acme.domains {domain:?} is a wildcard; the TLS-ALPN-01 challenge proves control \
             of one exact name, so a wildcard cannot be validated this way — list the names \
             individually, or use a DNS-01 client instead"
        )));
    }
    if domain.contains(':') {
        return Err(Error::Config(format!(
            "server.acme.domains {domain:?} contains a port; a certificate names a host, not an \
             address"
        )));
    }
    if domain.starts_with('.') || domain.ends_with('.') || domain.contains("..") {
        return Err(Error::Config(format!(
            "server.acme.domains {domain:?} has an empty label; check the name"
        )));
    }
    if !domain.contains('.') {
        // Not fatal for a CA — a single-label name is valid in some setups — but it is a mistake
        // in every deployment shape this server is for, and the failure it leads to (the CA cannot
        // resolve it) arrives after a round trip.
        return Err(Error::Config(format!(
            "server.acme.domains {domain:?} has no dot; a certificate cannot be issued for a \
             single-label name, so this is probably a typo"
        )));
    }
    Ok(())
}

/// The lowest TLS protocol version accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum MinTlsVersion {
    /// TLS 1.2 — the default. TLS 1.3 clients still negotiate 1.3; this is a floor.
    #[default]
    #[serde(rename = "1.2")]
    V1_2,
    /// TLS 1.3 only.
    #[serde(rename = "1.3")]
    V1_3,
}

/// ALPN default: HTTP/2 preferred, HTTP/1.1 always offered.
fn default_alpn() -> Vec<String> {
    vec!["h2".to_string(), "http/1.1".to_string()]
}
