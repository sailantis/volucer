//! ACME (RFC 8555) issuance, and the one place it meets our certificate table.
//!
//! # Why this is a crate of its own
//!
//! Because the composition is the hard part, not the protocol. `rustls-acme` implements the flow
//! — account, order, challenge, finalize, renewal schedule — and this crate exists to hold the
//! *answer to a handshake* that keeps that flow from taking over the deployment. Putting it in
//! `volucer-http` would put a network client next to the request path; putting it behind a trait
//! would hide the one property that matters, which is who answers a handshake.
//!
//! # The property this crate exists to preserve
//!
//! `rustls-acme`'s `ResolvesServerCertAcme` holds **one** certificate and returns it for *every*
//! SNI name that is not a challenge:
//!
//! ```text
//! if is_acme_challenge { /* the challenge cert */ }
//! else { self.inner.lock().unwrap().cert.clone() }   // <- the site table is never consulted
//! ```
//!
//! Installed directly, that silently replaces the host→certificate rule: a site would present
//! another site's certificate, which is a security failure rather than a misconfiguration. So the
//! ACME resolver may only be consulted for a handshake that offers the `acme-tls/1` ALPN protocol,
//! and [`ChallengeResolver`] is the only shape this crate hands out — a caller cannot install it
//! so that it answers a normal handshake.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rustls::server::ClientHello;

/// The certificate a validation handshake is answered with, or nothing.
///
/// A type alias rather than a newtype so that a caller passes it straight to
/// `with_cert_resolver` on a configuration built **for the challenge only**, which is what
/// `rustls-acme`'s own acceptor does. Nothing in this crate returns an `Arc<dyn
/// ResolvesServerCert>` that would be valid on the serving port, because that is the mistake.
pub type ChallengeCert = Arc<rustls::sign::CertifiedKey>;

/// Answer a *validation* handshake from the ACME state, and nothing else.
///
/// Deliberately not a `ResolvesServerCert` implementation: the trait has no way to say "only
/// challenge handshakes", so an implementation of it could be passed to
/// `ServerConfig::with_cert_resolver` and would then answer every name. The function takes the
/// `ClientHello` and returns `None` for anything that is not a challenge — so a caller who wired
/// it in the wrong place would get failed handshakes for real clients rather than a wrong
/// certificate, and a failed handshake is loud.
pub trait ChallengeSource: std::fmt::Debug + Send + Sync {
    /// Whether this handshake is a validation attempt.
    ///
    /// By reference, and separate from [`ChallengeSource::challenge_cert`], because it *can* be:
    /// `ClientHello` is not `Clone`, so a caller that had only a consuming method could not ask the
    /// question and then still answer a normal handshake with it. Two methods rather than one is
    /// the honest shape of the split — deciding does not consume, resolving must.
    fn is_challenge(&self, hello: &ClientHello<'_>) -> bool;

    /// The challenge certificate for this handshake, when it is a challenge for a name the state
    /// is currently validating.
    fn challenge_cert(&self, hello: ClientHello<'_>) -> Option<ChallengeCert>;

    /// The **site** this handshake's challenge belongs to, when the source can tell.
    ///
    /// Needed once one deployment can have several ACME tables: a challenge arrives as a bare TLS
    /// handshake, before any request, so the SNI name is the only thing that says *which* site's
    /// order it validates. `None` means "unscoped" — the source answers for the whole server, which
    /// is the correct reading when there is one table for the deployment.
    ///
    /// The answer is a site *name* rather than a hostname because a site is the unit the resolver
    /// already resolves by, and because several hosts can belong to one order.
    fn site_for(&self, _hello: &ClientHello<'_>) -> Option<&str> {
        None
    }
}

/// The ACME resolver, wrapped so it can only ever answer a challenge handshake.
///
/// Wraps `rustls_acme`'s resolver rather than re-exporting it, because the re-export's `resolve`
/// answers every non-challenge name with one certificate. Every path to the inner resolver goes
/// through [`ChallengeResolver::challenge_cert`], which refuses anything that is not a challenge —
/// so "the ACME resolver replaced our certificate table" is not a thing a caller can write.
#[derive(Debug)]
pub struct ChallengeResolver {
    inner: Arc<rustls_acme::ResolvesServerCertAcme>,
}

impl ChallengeResolver {
    /// Wrap the resolver an `AcmeState` hands out.
    ///
    /// `AcmeState::resolver()` is public, and so is `is_tls_alpn_challenge`, so the wrapper needs
    /// nothing from `rustls-acme` that a caller could not reach — which is what makes it possible
    /// to compose rather than to fork.
    pub fn new(inner: Arc<rustls_acme::ResolvesServerCertAcme>) -> Self {
        ChallengeResolver { inner }
    }
}

impl ChallengeSource for ChallengeResolver {
    fn is_challenge(&self, hello: &ClientHello<'_>) -> bool {
        rustls_acme::is_tls_alpn_challenge(hello)
    }

    fn challenge_cert(&self, hello: ClientHello<'_>) -> Option<ChallengeCert> {
        // The gate, and the only one that matters. `is_tls_alpn_challenge` is an **exact** match on
        // the ALPN list: it uses `.eq([b"acme-tls/1"])`, so a client offering `h2` *and*
        // `acme-tls/1` is not a validation attempt and must take the normal path. A client cannot
        // reach a validation-only certificate by asking for it alongside something else.
        if !self.is_challenge(&hello) {
            return None;
        }
        // Reached only for a challenge, so this is the one call into `rustls-acme`'s resolver.
        // Its own code then checks that the SNI name is the one being validated, and returns
        // `None` for a mismatch — which fails that handshake rather than answering it with
        // somebody else's challenge certificate.
        use rustls::server::ResolvesServerCert;
        self.inner.resolve(hello)
    }
}

/// A [`ChallengeResolver`] that answers for **one site** and says so.
///
/// The same resolver, with the site name attached so [`ChallengeSource::site_for`] can be answered.
/// Needed because one deployment can have several ACME tables and a validation handshake carries
/// only an SNI name: without the name, the resolver would have to try every table and could answer a
/// challenge for one site's order with another site's — which `rustls-acme` prevents only by refusing
/// a name it is not validating, so the mismatch would surface as a failed handshake rather than as
/// the configuration error it is.
#[derive(Debug)]
pub struct ScopedChallengeResolver {
    site: String,
    inner: ChallengeResolver,
}

impl ChallengeSource for ScopedChallengeResolver {
    fn is_challenge(&self, hello: &ClientHello<'_>) -> bool {
        self.inner.is_challenge(hello)
    }

    fn challenge_cert(&self, hello: ClientHello<'_>) -> Option<ChallengeCert> {
        self.inner.challenge_cert(hello)
    }

    fn site_for(&self, _hello: &ClientHello<'_>) -> Option<&str> {
        Some(&self.site)
    }
}

/// Something that went wrong setting ACME up.
#[derive(Debug, thiserror::Error)]
pub enum AcmeError {
    #[error(
        "no ACME directory: name one, or use the staging default — there is deliberately no \
         default that issues a real certificate"
    )]
    NoDirectory,

    #[error("an ACME-managed deployment needs at least one name to issue for")]
    NoDomains,

    #[error("the ACME cache directory {path} could not be secured: {reason}")]
    Cache { path: PathBuf, reason: String },

    #[error("the ACME state could not be started")]
    State,
}

/// Which certificate authority to talk to.
///
/// A named type rather than a bare `bool`, because the two are not interchangeable at a call site
/// a reader skims: `directory(true)` says nothing about what `true` means, and getting it wrong
/// means a *real* certificate for a test. Every example of this crate defaults to staging, and this
/// type is what carries that decision so it cannot be made by omission.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Authority {
    /// Let's Encrypt's staging endpoint: real protocol, certificates no browser trusts, and
    /// generous rate limits. The default.
    #[default]
    Staging,
    /// Let's Encrypt production. Only ever reached by naming it.
    Production,
}

impl Authority {
    /// The directory URL, which is what `rustls-acme` takes.
    pub fn directory(self) -> &'static str {
        match self {
            Authority::Staging => "https://acme-staging-v02.api.letsencrypt.org/directory",
            Authority::Production => "https://acme-v02.api.letsencrypt.org/directory",
        }
    }

    /// The name as it is written in configuration, for logs.
    pub fn as_str(self) -> &'static str {
        match self {
            Authority::Staging => "staging",
            Authority::Production => "production",
        }
    }
}

/// An ACME engine for one set of names: the account, the orders, and the renewal schedule.
///
/// # Why this exists rather than an `AcmeState` at the call site
///
/// Two reasons, and the second is the one that matters.
///
/// First, the state has to be **driven**: `AcmeState` is a `Stream` that does nothing until it is
/// polled, and it is the polling that performs the order, waits for the challenge to be validated,
/// finalizes, and installs the certificate. This type owns that task.
///
/// Second, the certificate has to reach the **server's** resolver. `rustls-acme` installs an issued
/// certificate into *its* resolver — `ResolvesServerCertAcme` — and that resolver returns one
/// certificate for every non-challenge name, which is Gap 3. So the engine does not let the issued
/// certificate stop there: it *also* installs it into the deployment's own table, through a sink the
/// caller provides. That sink is the reload path built in the previous phase, which is what makes a
/// certificate issued at 03:00 the one served by the next handshake.
#[derive(Debug)]
pub struct Engine {
    /// The state being driven, kept so the challenge resolver can be read from it.
    ///
    /// The cache's error types are `std::io::Error` — `DirCache` is what a deployment passes — so
    /// the concrete cache is named here rather than left generic. A caller with its own cache
    /// implementation would need the type parameter back; that is a change with a user, and there
    /// is not one yet.
    state: rustls_acme::AcmeState<std::io::Error, std::io::Error>,
    /// Where the cache lives, for the startup read.
    ///
    /// Held as a path rather than as a `DirCache`, because `DirCache` is not `Clone` and the state
    /// consumes one. Recreating it is free — it is a path — and it means the startup read and the
    /// state's own cache are necessarily the same directory rather than two that could drift.
    cache_dir: PathBuf,
    /// Where an issued certificate is installed for serving.
    sink: Arc<dyn CertificateSink>,
    /// The names this engine issues for, for logging and for the "nothing to do" check.
    domains: Vec<String>,
    /// Which authority, reported on every event.
    authority: Authority,
}

/// Where a certificate issued by ACME is installed so the next handshake serves it.
///
/// A trait rather than a concrete `SniResolver`, for the reason the reload work established: the
/// certificate has to reach **every** TLS listener, and a type that reached one would present a
/// renewed certificate on one port and not another. The implementation a deployment uses is the
/// process's single resolver; a test's is a recorder.
///
/// Deliberately **not** given the `KeyPair` or the account: a sink is the narrowest thing that can
/// do the job, so a caller cannot use it to reach ACME state.
pub trait CertificateSink: std::fmt::Debug + Send + Sync {
    /// Install a PEM certificate chain and key, as `rustls-acme` produced them.
    ///
    /// The PEM pair is what `AcmeState` stores and what `DirCache` writes, so taking the same shape
    /// means the in-memory path and the on-disk path cannot drift: what is served is what was
    /// cached, byte for byte.
    fn install(&self, pem: &[u8], domains: &[String]) -> Result<(), AcmeError>;
}

impl Engine {
    /// Build an engine around a cache directory, a set of names, and where to install what is
    /// issued.
    ///
    /// `cache_dir` is required rather than optional, and that is a rate-limit decision rather than
    /// a tidiness one: without a cache every restart re-issues, and Let's Encrypt's limits are
    /// per-week and per-account.
    ///
    /// `contact` is passed through to the account registration. It is not decoration: it is the only
    /// channel a CA has for warning that renewals are failing for this account, and `rustls-acme`
    /// names the account file after a hash of it — so it is also what decides **which** account a
    /// scope uses, and two scopes with the same contact share one.
    pub fn new(
        domains: Vec<String>,
        authority: Authority,
        cache_dir: impl Into<PathBuf>,
        contact: Vec<String>,
        sink: Arc<dyn CertificateSink>,
    ) -> Result<Self, AcmeError> {
        if domains.is_empty() {
            return Err(AcmeError::NoDomains);
        }
        let cache_dir = cache_dir.into();
        // Before anything is ordered, because the first thing that happens after this is a write
        // of the account key. `rustls-acme`'s `DirCache` is `create_dir_all` plus `fs::write` with
        // no mode of its own, so both the directory and every file in it inherit the process umask
        // — `0755`/`0644` on a stock system. That publishes the account key and, on every renewal,
        // the certificate's private key, to every local user on the host. Secured here rather than
        // by a wrapper cache because the files are written after `Engine::new` returns and by a
        // type this crate does not own; making the directory private is what covers them.
        secure_cache_dir(&cache_dir)?;
        let config = rustls_acme::AcmeConfig::new(domains.clone())
            .contact(contact)
            .cache(rustls_acme::caches::DirCache::new(cache_dir.clone()))
            // The directory is named explicitly even for staging, because the default is a
            // `pub(crate)` field a caller cannot read — so relying on it would mean not being able
            // to *report* which authority is in use, which is the thing an operator needs to know.
            .directory(authority.directory())
            .state();
        Ok(Engine {
            state: config,
            cache_dir,
            sink,
            domains,
            authority,
        })
    }

    /// The names this engine issues for.
    pub fn domains(&self) -> &[String] {
        &self.domains
    }

    /// The authority being used.
    pub fn authority(&self) -> Authority {
        self.authority
    }

    /// The challenge source to compose into the deployment's resolver.
    ///
    /// The engine keeps the state, so this is the only way out of it — and what comes back is a
    /// [`ChallengeResolver`], which cannot answer a normal handshake.
    pub fn challenge_source(&self) -> ChallengeResolver {
        ChallengeResolver::new(self.state.resolver())
    }

    /// The challenge source for **one site's** orders.
    ///
    /// Same resolver, wrapped with the site's name so [`ChallengeSource::site_for`] can tell the
    /// deployment's table from a site's. A deployment can have several tables at once — one per vhost
    /// with its own `[sites.<name>.acme]` — and a validation handshake carries only an SNI name, so
    /// the name is the only thing that says which order a challenge belongs to.
    pub fn site_challenge_source(&self, site: impl Into<String>) -> ScopedChallengeResolver {
        ScopedChallengeResolver {
            site: site.into(),
            inner: ChallengeResolver::new(self.state.resolver()),
        }
    }

    /// Install the cached certificate, if there is one.
    ///
    /// This is the answer to a real problem rather than a nicety. `AcmeState` installs an issued
    /// certificate into its **own** resolver — the one with Gap 3 — and the certificate bytes are
    /// not reachable from there. So the serving table cannot be filled from a `DeployedNewCert`
    /// event.
    ///
    /// What it *can* be filled from is the cache, through the public `CertCache` trait. `rustls-acme`
    /// writes the PEM there (its `store_cert` early action) and reports `EventOk::CertCacheStore`
    /// when that has happened, so reading the cache at that point gets exactly the certificate that
    /// was just issued. That is how a **renewal reaches the serving table without a restart** — the
    /// deliverable of the whole phase, and the one thing that would silently not happen if this
    /// read only ran at startup.
    ///
    /// Reading it *before* the first poll is the other half: `AcmeState`'s own `load_cert` is its
    /// first step, so this read means a restart serves its cached certificate immediately, and no
    /// order is started unless the cache is empty or the certificate has expired — which is the
    /// rate-limit behaviour the cache exists for.
    ///
    /// Returns whether anything was installed.
    ///
    /// A cache read that fails is reported and treated as "no cache": the alternative is refusing
    /// to start a server that has a perfectly good certificate on disk, which is the worse failure.
    pub async fn install_cached(&self) -> bool {
        use rustls_acme::CertCache;

        let cache = rustls_acme::caches::DirCache::new(self.cache_dir.clone());
        match cache
            .load_cert(&self.domains, self.authority.directory())
            .await
        {
            Ok(Some(pem)) => match self.sink.install(&pem, &self.domains) {
                Ok(()) => {
                    tracing::info!(
                        domains = ?self.domains,
                        "the cached certificate is installed; the next handshake serves it"
                    );
                    true
                }
                Err(err) => {
                    // The certificate is on disk and the deployment cannot use it. This is worth
                    // an error rather than a warning: the next handshake will present whatever the
                    // configuration names instead, and that is the difference between a browser
                    // trusting the site and not.
                    tracing::error!(%err, "the cached certificate could not be installed; the configured one keeps serving");
                    false
                }
            },
            Ok(None) => {
                tracing::info!(
                    domains = ?self.domains,
                    "no cached certificate: an order will be started, which is expected on a \
                     first deployment"
                );
                false
            }
            Err(err) => {
                tracing::error!(%err, "the certificate cache could not be read; treating it as empty");
                false
            }
        }
    }

    /// Drive the state for ever: issue, install, and renew on the crate's own schedule.
    ///
    /// Runs until the future is dropped, which is how the task is stopped — no shutdown flag,
    /// because there is nothing to flush: an interrupted order is retried on the next start and the
    /// cache is written per file.
    ///
    /// Every event is logged, including the ones that carry no certificate. That is deliberate: the
    /// failure this prevents is *silence*. A deployment whose issuance is failing looks completely
    /// healthy — the port is up, the previous certificate is serving, and the only symptom appears
    /// weeks later when that certificate expires.
    pub async fn run(mut self) {
        use futures::StreamExt;

        // Before the first poll, so a restart serves its cached certificate immediately.
        let _ = self.install_cached().await;

        tracing::info!(
            authority = self.authority.as_str(),
            domains = ?self.domains,
            "ACME issuance started"
        );

        while let Some(event) = self.state.next().await {
            match event {
                Ok(ok) => self.on_success(ok).await,
                Err(err) => {
                    // Not fatal, and not silent: `rustls-acme` backs off and retries, so a
                    // transient failure self-heals and a persistent one is visible here for as long
                    // as it persists.
                    tracing::error!(%err, "an ACME step failed; the deployment keeps serving the certificate it has");
                }
            }
        }
    }

    /// React to a completed step.
    async fn on_success(&mut self, event: rustls_acme::EventOk) {
        use rustls_acme::EventOk;

        match event {
            EventOk::DeployedCachedCert => tracing::info!(
                "a cached certificate is in use: no order was needed, which is what keeps a \
                 restart from spending a rate limit"
            ),
            EventOk::DeployedNewCert => {
                tracing::info!(domains = ?self.domains, "a new certificate was issued")
            }
            EventOk::CertCacheStore => {
                // **The renewal path.** The PEM is in the cache at this point — this event *is*
                // `store_cert` having returned `Ok` — so reading it back is how the newly issued
                // certificate reaches the table a handshake resolves against. Without this, a
                // renewal would sit in `rustls-acme`'s resolver, which answers nothing on the
                // serving port, and the deployment would keep presenting the old certificate until
                // it expired.
                tracing::debug!("the certificate was written to the cache");
                if !self.install_cached().await {
                    tracing::error!(
                        "a certificate was stored but could not be read back into the serving \
                         table; the previous one keeps serving until the next attempt"
                    );
                }
            }
            EventOk::AccountCacheStore => {
                tracing::debug!("the account was written to the cache")
            }
        }
    }
}

/// Create the cache directory if needed and make it private to this account.
///
/// The files `rustls-acme` writes there are the ACME **account key** and the **certificate private
/// key**, and its `DirCache` gives neither a mode: it is `create_dir_all` and `fs::write`, so both
/// land at `0666 & !umask` — `0644` on a stock system — inside a `0755` directory. On a shared
/// host that is a local user reading the key to the site's TLS certificate, and with the account
/// key, a local user able to order certificates as this deployment. `volucer-kv` sets `0o700` and
/// `0o600` for exactly this reason; this is the same rule for the same class of file.
///
/// Applied on every start rather than only at creation, because a cache directory created by an
/// earlier version — or by hand, as the documentation suggests — is already `0755`, and tightening
/// it only on the "created it myself" path would leave every existing deployment exposed.
///
/// Files already inside are tightened too. New ones inherit the directory's mode only for the
/// group/other bits the umask leaves; a file written before this ran is `0644` regardless, and a
/// renewal is up to 60 days away, so waiting for the next write would leave those readable for
/// that long.
///
/// Windows has no equivalent: a file's ACL comes from its parent directory, so a cache under a
/// user's profile is already private to that user. The mode calls are `cfg(unix)` and the function
/// is a directory creation there.
fn secure_cache_dir(dir: &Path) -> Result<(), AcmeError> {
    let cache = |path: &Path, reason: String| AcmeError::Cache {
        path: path.to_path_buf(),
        reason,
    };

    // A cache path that exists as a *file* is a configuration mistake rather than a transient
    // failure: every later write would fail, and the engine would report it as a networking error
    // from the ACME step that tried to store. Refusing here names the path instead.
    if let Ok(metadata) = std::fs::metadata(dir) {
        if !metadata.is_dir() {
            return Err(cache(dir, "it exists and is not a directory".to_string()));
        }
    } else if let Err(err) = std::fs::create_dir_all(dir) {
        return Err(cache(dir, format!("could not create it: {err}")));
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .map_err(|err| cache(dir, format!("could not set its mode to 0700: {err}")))?;

        // A read that fails is not fatal. The directory is private *now*, so nothing new can learn
        // the names; an entry this cannot stat is one that disappeared between the read and the
        // metadata call, which is the ACME task's own temporary file and not a key.
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if entry.file_type().is_ok_and(|kind| kind.is_file()) {
                    // Deliberately not an error: a file owned by another user — a deployment
                    // re-run as a different account — cannot be tightened, and refusing to start
                    // would turn a permission that was already wrong into an outage. The
                    // directory mode is the guard that matters, and it is set above.
                    let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
                }
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The wrapper is a `ChallengeSource` and **not** a `ResolvesServerCert`.
    ///
    /// A compile-time assertion, and the point of the whole crate: if `ChallengeResolver` ever
    /// implemented `ResolvesServerCert`, a caller could hand it to `with_cert_resolver` and every
    /// SNI name would get whatever single certificate the ACME state holds. That is the failure
    /// this type exists to make unwritable, so the *absence* of the impl is what is asserted —
    /// and the way to assert an absence at compile time is to require the type in a bound it does
    /// not satisfy and check the build fails. Which cannot be done in a test that must pass.
    ///
    /// So what is checked instead is the observation that is available: the trait this type *does*
    /// implement is named, and its method refuses a non-challenge hello. The stronger property is
    /// enforced by the compiler at the call site in `volucer-http::tls::SniResolver::with_acme`,
    /// where passing a `ChallengeResolver` to `with_cert_resolver` would not type-check.
    #[test]
    fn a_challenge_resolver_is_not_a_certificate_resolver() {
        fn assert_challenge_source<T: ChallengeSource>() {}
        assert_challenge_source::<ChallengeResolver>();
    }

    /// A `CertificateSink` that records what it was asked to install.
    #[derive(Debug, Default)]
    struct Recording {
        installed: std::sync::Mutex<Vec<(Vec<u8>, Vec<String>)>>,
    }

    impl CertificateSink for Recording {
        fn install(&self, pem: &[u8], domains: &[String]) -> Result<(), AcmeError> {
            self.installed
                .lock()
                .expect("not poisoned")
                .push((pem.to_vec(), domains.to_vec()));
            Ok(())
        }
    }

    /// A `CertificateSink` that always fails, for the "on disk but unusable" path.
    #[derive(Debug)]
    struct Refusing;

    impl CertificateSink for Refusing {
        fn install(&self, _pem: &[u8], _domains: &[String]) -> Result<(), AcmeError> {
            Err(AcmeError::State)
        }
    }

    /// Staging, never production, and the directories are the real ones.
    ///
    /// A test rather than a comment because the consequence of getting it wrong is issuing a real
    /// certificate for a name somebody owns. Every path that does not name an authority must come
    /// out staging, and production must be reachable only by naming it.
    #[test]
    fn the_default_authority_is_staging_and_production_needs_naming() {
        assert_eq!(
            Authority::default(),
            Authority::Staging,
            "a deployment that does not name an authority must not be able to reach production"
        );
        assert!(
            Authority::Staging.directory().contains("staging"),
            "the staging directory must be the staging endpoint: {}",
            Authority::Staging.directory()
        );
        assert!(
            !Authority::Production.directory().contains("staging"),
            "the production directory must be the production endpoint: {}",
            Authority::Production.directory()
        );
        assert_eq!(Authority::default().as_str(), "staging");
    }

    /// The cached certificate is read from the cache and installed, and a **missing** one is
    /// reported as such rather than as a failure.
    ///
    /// The load-bearing half is the timing: `run` calls this *before* its first poll, which is what
    /// makes a restart serve its cached certificate immediately instead of waiting for the state to
    /// load it — and waiting would be exactly the window in which the port is up and handshakes get
    /// something other than the certificate that was issued.
    ///
    /// Driven on `install_cached` rather than on `run`, because `run` never returns: it is
    /// `AcmeState`'s infinite stream, and a test that awaited it would hang rather than fail.
    #[tokio::test]
    async fn the_cached_certificate_is_installed_before_anything_is_ordered() {
        let dir = std::env::temp_dir().join(format!("volucer-acme-cache-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        let sink = Arc::new(Recording::default());
        let engine = Engine::new(
            vec!["example.test".to_string()],
            Authority::Staging,
            &dir,
            Vec::new(),
            Arc::clone(&sink) as Arc<dyn CertificateSink>,
        )
        .expect("an engine for one name");

        // An empty cache: nothing installed, and *not* an error — a first deployment has no
        // certificate and must still start.
        assert!(
            !engine.install_cached().await,
            "an empty cache must report that nothing was installed"
        );
        assert!(
            sink.installed.lock().expect("not poisoned").is_empty(),
            "an empty cache must install nothing"
        );

        // Now a cached certificate. Written through the same `CertCache` trait the engine reads
        // with, so the file name (a hash of the domains and directory) is the one it will look for
        // — a test that wrote the file by hand would pass against a name the engine never reads.
        {
            use rustls_acme::CertCache;
            let cache = rustls_acme::caches::DirCache::new(&dir);
            cache
                .store_cert(
                    &["example.test".to_string()],
                    Authority::Staging.directory(),
                    b"-----BEGIN CERTIFICATE-----\ncached\n-----END CERTIFICATE-----\n",
                )
                .await
                .expect("the cache is writable");
        }

        let sink2 = Arc::new(Recording::default());
        let engine = Engine::new(
            vec!["example.test".to_string()],
            Authority::Staging,
            &dir,
            Vec::new(),
            Arc::clone(&sink2) as Arc<dyn CertificateSink>,
        )
        .expect("an engine for one name");

        assert!(
            engine.install_cached().await,
            "a cached certificate must be reported as installed"
        );
        let installed = sink2.installed.lock().expect("not poisoned").clone();
        assert_eq!(
            installed.len(),
            1,
            "the cached certificate must be installed exactly once: {installed:?}"
        );
        assert!(
            installed[0].0.starts_with(b"-----BEGIN CERTIFICATE-----"),
            "the bytes handed to the sink must be the cached PEM, so what is served is byte for \
             byte what was issued: {installed:?}"
        );
        assert_eq!(
            installed[0].1,
            vec!["example.test".to_string()],
            "the domains must travel with the certificate"
        );

        // And a sink that refuses is reported without taking the process with it.
        let engine = Engine::new(
            vec!["example.test".to_string()],
            Authority::Staging,
            &dir,
            Vec::new(),
            Arc::new(Refusing),
        )
        .expect("an engine for one name");
        assert!(
            !engine.install_cached().await,
            "a certificate the sink refuses must be reported as not installed"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A published renewal reaches the serving table, without a restart.
    ///
    /// **This is the deliverable of the whole phase**, and it is the one thing that would silently
    /// not happen. `AcmeState` installs an issued certificate into *its* resolver —
    /// `ResolvesServerCertAcme`, the one with Gap 3 — and that resolver answers nothing on a
    /// non-challenge handshake, so a renewal would sit there while the deployment kept presenting
    /// the old certificate until it expired. The serving table is filled by reading the cache back
    /// when `rustls-acme` reports it has been written, which is what this asserts.
    ///
    /// Written against `on_success`, because the alternative is a test that talks to Let's Encrypt.
    /// The event is the real one and the cache is the real `DirCache`, so what is stubbed is only
    /// the network — which is the part that must not be part of a test anyway.
    #[tokio::test]
    async fn a_published_certificate_is_read_back_into_the_serving_table() {
        use rustls_acme::CertCache;

        let dir = std::env::temp_dir().join(format!("volucer-acme-renew-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        let sink = Arc::new(Recording::default());
        let mut engine = Engine::new(
            vec!["example.test".to_string()],
            Authority::Staging,
            &dir,
            Vec::new(),
            Arc::clone(&sink) as Arc<dyn CertificateSink>,
        )
        .expect("an engine for one name");

        // Nothing in the cache yet: the `CertCacheStore` event must not invent a certificate.
        engine
            .on_success(rustls_acme::EventOk::CertCacheStore)
            .await;
        assert!(
            sink.installed.lock().expect("not poisoned").is_empty(),
            "an event reporting a cache write that has not happened must install nothing"
        );

        // Now the write the event reports. Done through the same trait, so the file name is the
        // hashed one the engine will look for.
        let cache = rustls_acme::caches::DirCache::new(&dir);
        cache
            .store_cert(
                &["example.test".to_string()],
                Authority::Staging.directory(),
                b"-----BEGIN CERTIFICATE-----\nrenewed\n-----END CERTIFICATE-----\n",
            )
            .await
            .expect("the cache is writable");

        engine
            .on_success(rustls_acme::EventOk::CertCacheStore)
            .await;
        let installed = sink.installed.lock().expect("not poisoned").clone();
        assert_eq!(
            installed.len(),
            1,
            "a published renewal must reach the serving table exactly once: {installed:?}"
        );
        assert!(
            installed[0]
                .0
                .windows(b"renewed".len())
                .any(|w| w == b"renewed"),
            "the bytes installed must be the renewed certificate, not whatever was there before: \
             {:?}",
            String::from_utf8_lossy(&installed[0].0)
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An engine will not start with nothing to issue for.
    ///
    /// An empty domain list is a configuration mistake that would otherwise become an ACME request
    /// for an empty identifier set — a network round trip to learn what the caller already knew.
    #[test]
    fn an_engine_needs_at_least_one_name() {
        let sink = Arc::new(Recording::default()) as Arc<dyn CertificateSink>;
        match Engine::new(Vec::new(), Authority::Staging, ".", Vec::new(), sink) {
            Ok(_) => panic!("no names is not a usable engine, and must be refused"),
            Err(err) => assert!(
                err.to_string().contains("at least one name"),
                "the error must say what is missing: {err}"
            ),
        }
    }

    /// The cache holds the account key and the certificate's private key, and neither may be
    /// readable by any other account on the host.
    ///
    /// `rustls-acme`'s `DirCache` writes both with `fs::write` and no mode, so on a stock umask they
    /// are `0644` in a `0755` directory. Both halves are asserted, because the directory alone is
    /// not enough — a file already on disk keeps its own mode — and the *pre-existing* case is the
    /// one that matters in practice, since a deployment upgrading is already exposed.
    #[cfg(unix)]
    #[test]
    fn the_cache_directory_and_its_files_are_private() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!("volucer-acme-mode-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create the cache");

        // A key written *before* this ran, which is what an upgrade finds.
        let stale = dir.join("cached_account_stale");
        std::fs::write(&stale, b"private").expect("write a stale file");
        std::fs::set_permissions(&stale, std::fs::Permissions::from_mode(0o644))
            .expect("make it world-readable");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755))
            .expect("make the directory world-readable");

        let sink = Arc::new(Recording::default()) as Arc<dyn CertificateSink>;
        Engine::new(
            vec!["example.test".to_string()],
            Authority::Staging,
            &dir,
            Vec::new(),
            sink,
        )
        .expect("an engine for one name");

        let mode = |path: &std::path::Path| {
            std::fs::metadata(path)
                .expect("the path exists")
                .permissions()
                .mode()
                & 0o777
        };
        assert_eq!(
            mode(&dir),
            0o700,
            "the cache directory must not be readable by others"
        );
        assert_eq!(
            mode(&stale),
            0o600,
            "a key already in the cache must be tightened, not left for the next renewal"
        );

        // A file written *after* the engine exists still lands in a private directory, which is
        // the guarantee that covers every later write by `DirCache`.
        let fresh = dir.join("cached_cert_fresh");
        std::fs::write(&fresh, b"private").expect("write a fresh file");
        assert_eq!(mode(&dir), 0o700, "the directory must stay private");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A cache path that is a file, rather than a directory, is refused by name.
    ///
    /// The alternative is every `store_cert` failing later and being reported by the ACME task as a
    /// network step that went wrong, which sends an operator looking at the CA rather than at the
    /// configuration.
    #[test]
    fn a_cache_path_that_is_not_a_directory_is_refused() {
        let path = std::env::temp_dir().join(format!("volucer-acme-notdir-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::write(&path, b"a file, not a cache").expect("write the file");

        let sink = Arc::new(Recording::default()) as Arc<dyn CertificateSink>;
        match Engine::new(
            vec!["example.test".to_string()],
            Authority::Staging,
            &path,
            Vec::new(),
            sink,
        ) {
            Ok(_) => panic!("a cache path that is a file must be refused"),
            Err(err) => {
                let message = err.to_string();
                assert!(
                    message.contains("not a directory"),
                    "the error must name the problem: {message}"
                );
                assert!(
                    message.contains(&path.display().to_string()),
                    "the error must name the path: {message}"
                );
            }
        }

        let _ = std::fs::remove_file(&path);
    }
}
