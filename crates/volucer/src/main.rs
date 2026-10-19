//! The `volucer` command line interface.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use clap::{Parser, Subcommand};

use volucer_core::config::{resolve_access_log_path, AccessLogFormat, Config, MinTlsVersion};
use volucer_core::php;
use volucer_core::sites::Sites;
use volucer_fcgi::cgi::Routing;
use volucer_http::{HttpLimits, RunningHttp, RunningStatus};
use volucer_server::{
    Consumers, Contention, Dispatcher, FastCgiListener, Limits, ServerError, WorkerPool,
};

#[derive(Parser, Debug)]
#[command(
    name = "volucer",
    version,
    about = "Cross-platform PHP application server: persistent workers and FastCGI",
    long_about = None
)]
struct Cli {
    /// Increase log verbosity (`-v`, `-vv`).
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    verbose: u8,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Run the configured listeners and backends.
    Serve {
        /// Path to `volucer.toml`.
        #[arg(short, long, default_value = "volucer.toml")]
        config: PathBuf,

        /// Document root that PHP scripts are resolved against.
        ///
        /// Overrides `server.document_root`; without either, `.`.
        #[arg(long)]
        document_root: Option<PathBuf>,

        /// Front controller, relative to the document root.
        ///
        /// Overrides `server.front_controller`; without either, `index.php`.
        /// Set it empty to serve only actual files, matching nginx's
        /// `location ~ \.php$`.
        #[arg(long)]
        front_controller: Option<String>,

        /// Maximum concurrent client connections.
        #[arg(long, default_value_t = 512)]
        max_connections: usize,
    },

    /// Validate the configuration and report what was found on this host.
    Check {
        /// Path to `volucer.toml`.
        #[arg(short, long, default_value = "volucer.toml")]
        config: PathBuf,

        /// Document root to check, overriding `server.document_root`.
        #[arg(long)]
        document_root: Option<PathBuf>,

        /// Extra directories to search for PHP binaries.
        #[arg(long = "php-dir")]
        php_dirs: Vec<PathBuf>,
    },

    /// Print the version, the PHP floor, and the protocols this build speaks.
    Version,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    init_tracing(cli.verbose);

    let result = match cli.command {
        Command::Serve {
            config,
            document_root,
            front_controller,
            max_connections,
        } => serve(
            &config,
            document_root.as_deref(),
            front_controller.as_deref(),
            max_connections,
        ),
        Command::Check {
            config,
            document_root,
            php_dirs,
        } => check(&config, document_root.as_deref(), &php_dirs).map_err(ServerError::from),
        Command::Version => {
            version();
            Ok(())
        }
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            // The subscriber is initialised by now, and `main` still holds the only `Err` a run can
            // produce. `error!` puts a fatal message where a log pipeline expects one; the chain is
            // written as a single event with the causes folded in, because a per-cause event would
            // be several lines pretending to be one error.
            let mut causes = String::new();
            let mut source = std::error::Error::source(&err);
            while let Some(cause) = source {
                causes.push_str(&format!("\n  caused by: {cause}"));
                source = cause.source();
            }
            tracing::error!("volucer: {err}{causes}");
            ExitCode::FAILURE
        }
    }
}

/// Build the process's observability handle from configuration.
///
/// **This is where "off by default" is decided, and it is the only place.** No `[server.access_log]`
/// table produces [`Observability::disabled`], which writes nothing and — because the front-end
/// checks [`Observability::is_enabled`] before building a record — does not allocate a request id's
/// hex form either. A deployment that has not asked for logging therefore pays one branch per
/// request, which is what makes the promise checkable rather than merely stated.
///
/// The path resolves against the config file's directory, the same rule `[server.tls]`'s
/// `certificate` follows. A warning about a missing directory is **not** emitted here: `AccessLog`
/// reports an unopenable file once at runtime, from the only place that knows whether the file could
/// actually be created, and a second weaker check here would be a warning that fires on a directory
/// that exists but is not writable — which teaches an operator to ignore it.
/// The exporter handle, kept alive by the caller for as long as the server serves.
///
/// Dropping it stops the thread and flushes the last batch, so it must **not** be dropped at the end
/// of a builder — which is exactly what returning it here rather than storing it in the
/// `Observability` prevents, since that would put an `Arc` cycle or a hidden lifetime in a type whose
/// whole job is identity.
pub struct Telemetry {
    exporter: Option<Arc<volucer_otlp::Exporter>>,
}

impl Telemetry {
    /// Whether spans are being exported.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.exporter.is_some()
    }

    /// The endpoint, for the startup line and the shutdown summary.
    #[must_use]
    pub fn endpoint(&self) -> Option<&str> {
        self.exporter.as_ref().map(|e| e.endpoint())
    }

    /// What the exporter did, at shutdown.
    #[must_use]
    pub fn counts(&self) -> Option<volucer_otlp::ExportCounts> {
        self.exporter.as_ref().map(|e| e.counts())
    }

    /// Stop the exporter and flush the last batch.
    ///
    /// Called at shutdown *before* the summary is printed, so the summary can report what the
    /// exporter did — the numbers are only final once the final flush has finished.
    pub fn stop(&self) {
        if let Some(exporter) = self.exporter.as_ref() {
            exporter.stop();
        }
    }
}

/// Build the process's observability handle **and** its trace exporter.
///
/// One function rather than two, because the two halves have to agree on one thing: the exporter's
/// recorder must be installed in the same `Observability` the front-ends are handed. Building them
/// separately would make "spans are configured but nothing records them" a state that compiles.
fn build_observability_with_exporter(
    config: &Config,
    config_path: &Path,
) -> (volucer_observe::Observability, Telemetry) {
    let mut observe = match &config.server.access_log {
        Some(log) => {
            let config_dir = config_path.parent().unwrap_or_else(|| Path::new(""));
            let path = resolve_access_log_path(&log.path, config_dir);
            let format = match log.format {
                AccessLogFormat::Combined => volucer_observe::access_log::Format::Combined,
                AccessLogFormat::Json => volucer_observe::access_log::Format::Json,
            };
            // An empty header means "never adopt", which is nginx's `$request_id` behaviour and is
            // reachable by emptying one string rather than by a second boolean that could disagree
            // with this one.
            let header = match log.request_id_header.trim() {
                "" => None,
                name => Some(name.to_string()),
            };
            tracing::info!(
                path = %path.display(),
                format = ?log.format,
                adopt = header.as_deref().unwrap_or("(never)"),
                "access logging enabled"
            );
            volucer_observe::Observability::new(
                volucer_observe::access_log::AccessLog::open(&path, format),
                header,
            )
        }
        // Not `disabled()` and then patched: a deployment with spans and no access log is a
        // supported shape, and this is where that is expressed.
        None => volucer_observe::Observability::disabled(),
    };

    let Some(telemetry) = &config.telemetry else {
        return (observe, Telemetry { exporter: None });
    };

    match volucer_otlp::Exporter::start(telemetry) {
        Ok(exporter) => {
            let exporter = std::sync::Arc::new(exporter);
            tracing::info!(
                endpoint = exporter.endpoint(),
                service = %telemetry.service_name,
                batch_size = telemetry.batch_size,
                interval_ms = telemetry.interval_ms,
                "trace export enabled"
            );
            observe = observe.with_spans(exporter.recorder());
            (
                observe,
                Telemetry {
                    // Kept as an `Arc` rather than unwrapped: the recorder the front-ends hold is a
                    // second reference to this same allocation, so `try_unwrap` would always fail.
                    // The `Arc` is also what lets `Telemetry::stop(&self)` reach the thread.
                    exporter: Some(exporter),
                },
            )
        }
        // **A bad endpoint is reported, not fatal.** The alternative is a server that refuses to
        // start because its telemetry collector is misconfigured — which inverts the priority the
        // access log already states: a deployment whose log directory is missing logs that fact and
        // serves anyway. Traces are the least important thing this process does.
        Err(err) => {
            tracing::error!(
                endpoint = %telemetry.endpoint,
                %err,
                "trace export could not start, so no spans will be exported; the server is \
                 unaffected"
            );
            (observe, Telemetry { exporter: None })
        }
    }
}

/// `version` prints what is running, and what that build can do.
///
/// The PHP floor is here because it is a build fact an operator has to be able
/// to check from a shell: a worker below it starts and then cannot be asked for
/// a per-request peak. See the README in `sailantis/volucer-php-sdk`.
fn version() {
    println!("volucer {}", env!("CARGO_PKG_VERSION"));
    println!("  front-ends:       HTTP/1.1, HTTP/2 (cleartext), FastCGI, TLS (rustls)");
    println!(
        "  worker protocol:  {}",
        volucer_server::WORKER_PROTOCOL_VERSION
    );
    println!(
        "  php floor:        {}",
        volucer_server::MINIMUM_PHP_VERSION
    );
    println!("  backends:         worker, external fpm, spawn_cgi, upstream");
    // ACME is advertised only as *available*, because whether it is in use is a property of the
    // configuration rather than of the build — `volucer check` is where a deployment's own answer
    // is printed. Saying "TLS-ALPN-01" rather than "ACME" matters: it is the one challenge this
    // build can perform, and an operator comparing against another server needs to know that.
    println!("  certificates:     ACME (RFC 8555), the tls-alpn-01 challenge");
    // The access log and trace propagation are advertised as *available*, exactly as ACME is:
    // whether either is in use is a property of the configuration, and `check` is where a
    // deployment's own answer is printed. Saying "traceparent" by name matters because it is the
    // W3C header a client or an nginx in front would send, and an operator comparing against
    // another server needs to know which spelling this one reads.
    println!("  observability:    access log (combined or json), W3C tracecontext");
    // OTLP is advertised as *available* for the same reason ACME and the access log are: whether it
    // is in use is a property of the configuration. The endpoint is named because the port is the
    // one fact an operator needs to compare against their collector's own configuration.
    println!("  traces:           OTLP/HTTP export (POST /v1/traces, default port 4318)");
}

fn init_tracing(verbosity: u8) {
    use tracing_subscriber::EnvFilter;

    let fallback = match verbosity {
        0 => "volucer=info",
        1 => "volucer=debug",
        _ => "volucer=trace",
    };
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(fallback));
    // **Diagnostics go to STDERR, and that split is deliberate.** `fmt`'s default writer is
    // `io::stdout` (verified in `tracing-subscriber-0.3.23/src/fmt/fmt_layer.rs:749`), which would
    // put log lines on the same stream as the banner and `check`'s report — so an operator
    // grepping stdout would be reading logs, and a machine parsing `check` output would be reading
    // both at once. `stderr` is the stream for diagnostics; stdout stays the program's own output.
    //
    // It also makes the three sinks this server will have *separately* silenceable: stdout for
    // program output, stderr for diagnostics, and the access log's own file — which is why a
    // single `RUST_LOG` filter must not be able to quiet the access log as well.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(false)
        .try_init();
}

fn load_and_validate(path: &Path) -> Result<Config, ServerError> {
    let config = Config::load(path)?;
    config.validate()?;
    tracing::info!(path = %path.display(), "configuration loaded");
    Ok(config)
}

/// Build the routing for a site from the site table.
///
/// One place rather than a literal per call site, because the `script_extensions` a site resolves
/// has to reach both the router and the dispatcher: a `Routing` rebuilt from `[server]` at one of
/// them is how a widened list would be honoured by one and not the other.
fn routing_of(site: &volucer_core::ResolvedSite) -> Routing {
    Routing {
        document_root: site.document_root.clone(),
        front_controller: site.front_controller.clone(),
        script_extensions: site.script_extensions.clone(),
        ..Routing::default()
    }
}

/// The document root in effect: the flag, else the file, else the working
/// directory.
///
/// The order matters and is stated once, here, so `serve` and `check` cannot
/// disagree about which root will be used — `check` reports on the deployment
/// `serve` would actually start.
fn resolve_document_root(flag: Option<&Path>, config: &Config) -> PathBuf {
    flag.map(Path::to_path_buf)
        .or_else(|| config.server.document_root.clone())
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Absolutise the document root, because a relative one silently breaks PHP.
///
/// `SCRIPT_FILENAME` is built by joining this root, and `php-cgi` runs in a
/// persistent pool: it `chdir()`s into the directory of the script it executed.
/// A relative root therefore resolves against the *previous* request's
/// directory from the second request onwards, and the answer is `No input file
/// specified` — verified against php-cgi 8.3.31, where the same path succeeds
/// once and fails every time after. An absolute root has no such dependency.
///
/// Best-effort: if the working directory cannot be read there is nothing better
/// to offer than the path as written, and the preflight warning for a root that
/// does not exist already covers the case where something is wrong.
fn absolute_document_root(root: &Path) -> PathBuf {
    if root.is_absolute() {
        return root.to_path_buf();
    }
    std::env::current_dir()
        .map(|cwd| cwd.join(root))
        .unwrap_or_else(|_| root.to_path_buf())
}

/// Resolve a TLS path against the directory holding the config file.
///
/// Deliberately **not** the document-root rule, which is relative to the process's working
/// directory. A certificate beside `volucer.toml` is the common case, and resolving it
/// against wherever the process happened to start is a footgun with a security consequence:
/// the server would start with whatever certificate it found, or fail to start at all, for a
/// reason invisible in the file. An absolute path is used as written.
fn resolve_tls_path(path: &Path, config_path: &Path) -> PathBuf {
    if path.is_absolute() {
        return path.to_path_buf();
    }
    match config_path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir.join(path),
        // A bare filename such as `volucer.toml` has an empty parent, which means the
        // working directory — the same answer `Path::parent` gives and the one a reader
        // expects.
        _ => path.to_path_buf(),
    }
}

/// One ACME table: the deployment's, or a site's.
///
/// Collected before any engine is built, because the engines and the resolver that carries their
/// challenge sources are mutually dependent: every engine needs the cell the composed resolver will
/// be installed into, and the composed resolver needs every engine's challenge source. Gathering the
/// scopes first is what breaks that cycle without a lock on any path that matters.
#[derive(Debug, Clone)]
struct AcmeScope {
    /// How this table is named in logs — `server.acme`, or the site name.
    label: String,
    /// The names to issue for, already resolved.
    domains: Vec<String>,
    /// Which authority.
    authority: volucer_core::config::Authority,
    /// The cache directory, already absolute.
    cache: PathBuf,
    /// `Some(site)` when this is a site's own table. The site name is what scopes its challenge
    /// source, since a validation handshake carries only an SNI name.
    site: Option<String>,
    /// The contact list this scope registers, already resolved against the deployment's.
    ///
    /// Empty is allowed and reported: a CA with no contact has no way to warn that this account's
    /// renewals are failing, which is the failure this feature is most prone to. What it is *not* is
    /// invisible — it is also what the account file is named after, so two scopes that share it
    /// share an account and two that differ do not.
    contact: Vec<String>,
    /// Whether the deployment's contact list is empty, for the one warning worth emitting.
    contact_missing: bool,
}

/// A cache directory, absolute.
///
/// Relative to the **config file's** directory, like `certificate` and `key` and deliberately unlike
/// `document_root`. Resolved once here rather than at each use so `check` and `serve` cannot disagree
/// about where the account and certificates live.
fn acme_cache(config_dir: &Path, cache: &Path) -> PathBuf {
    match cache.is_absolute() {
        true => cache.to_path_buf(),
        false => config_dir.join(cache),
    }
}

/// The ACME engine's view of which authority to talk to.
fn acme_authority(authority: volucer_core::config::Authority) -> volucer_acme::Authority {
    match authority {
        volucer_core::config::Authority::Staging => volucer_acme::Authority::Staging,
        volucer_core::config::Authority::Production => volucer_acme::Authority::Production,
    }
}

/// Absolute certificate and key for the server's own TLS settings.
///
/// `None` when the pair is stated only partly, which `Config::validate` refuses, or when it is left
/// out entirely so a placeholder is generated — in which case there is no path to resolve and
/// `check` reports the generation rather than a file.
fn resolved_server_tls(config: &Config, config_path: &Path) -> Option<TlsPaths> {
    let tls = config.server.tls.as_ref()?;
    let certificate = tls.certificate.as_ref()?;
    let key = tls.key.as_ref()?;
    Some(TlsPaths {
        certificate: resolve_tls_path(certificate, config_path),
        key: resolve_tls_path(key, config_path),
    })
}

/// Absolute certificate and key for the server's own TLS settings, when they are generated.
///
/// The distinction `check` has to make: a pair that is absent is not a mistake, it is a
/// placeholder — and saying "certificate: (generated)" is the difference between an operator
/// knowing issuance is wired up and wondering where their certificate went.
fn server_tls_is_generated(config: &Config) -> bool {
    config
        .server
        .tls
        .as_ref()
        .is_some_and(|tls| tls.certificate.is_none() && tls.key.is_none())
}

/// Resolved certificate and key paths.
struct TlsPaths {
    certificate: PathBuf,
    key: PathBuf,
}

/// Every `(site name, paths)` that states its own certificate, resolved.
fn resolved_site_tls(config: &Config, config_path: &Path) -> Vec<(String, TlsPaths)> {
    config
        .sites
        .iter()
        .filter_map(|(name, site)| {
            let tls = site.tls.as_ref()?;
            let certificate = tls.certificate.as_ref()?;
            let key = tls.key.as_ref()?;
            Some((
                name.clone(),
                TlsPaths {
                    certificate: resolve_tls_path(certificate, config_path),
                    key: resolve_tls_path(key, config_path),
                },
            ))
        })
        .collect()
}

/// How long a certificate is valid for, and whether that is a problem.
///
/// The direct mitigation for ACME's failure mode, which is **silence**: issuance that never
/// succeeds is indistinguishable from issuance that is working, so the only symptom a deployment
/// gets is a certificate that expires. Reading the days left and saying so is what turns weeks of
/// quiet failure into one line an operator sees.
///
/// A day count rather than a timestamp because that is the unit the decision is made in: 41 days
/// means "renewals are working", 3 means "something has been failing for a while".
fn certificate_lifetime(path: &Path) -> Option<CertificateLifetime> {
    let pem = std::fs::read(path).ok()?;
    let (_, pem) = x509_parser::pem::parse_x509_pem(&pem).ok()?;
    let certificate = pem.parse_x509().ok()?;
    let validity = certificate.validity();
    let expiry = validity.not_after.timestamp();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    Some(CertificateLifetime {
        days_left: (expiry - now).div_euclid(86_400),
        expired: expiry <= now,
    })
}

/// What `check` reports about one certificate's lifetime.
struct CertificateLifetime {
    /// Whole days until `not_after`. Negative when it has already passed.
    days_left: i64,
    /// Whether `not_after` is in the past.
    expired: bool,
}

impl CertificateLifetime {
    /// The report, with the warning it deserves.
    ///
    /// The thresholds are chosen against how automation actually fails rather than against a
    /// round number. A renewal is attempted at a third of the way through validity, so anything
    /// under a third means *at least one* attempt has not taken effect yet — worth knowing, not yet
    /// an emergency. Under a week means several have failed in a row.
    fn report(&self, label: &str) -> String {
        if self.expired {
            return format!(
                "{label}: EXPIRED — every client is being shown a certificate warning, and \
                 no ACME renewal has succeeded"
            );
        }
        match self.days_left {
            0..=7 => format!(
                "{label}: {} day(s) left — several renewals in a row have failed; the CA will \
                 also be warning the account contact",
                self.days_left
            ),
            8..=30 => format!(
                "{label}: {} day(s) left — an ACME renewal should have happened by now",
                self.days_left
            ),
            _ => format!("{label}: {} day(s) left", self.days_left),
        }
    }
}

/// Install an ACME-issued certificate into the process's serving table.
///
/// The engine's sink, and the only bridge from issuance to what a handshake resolves against.
///
/// It reaches the **process's one resolver**, not a listener's, for the reason `reload-shared`
/// already established one layer down: an `:80`/`:443` deployment has two TLS-speaking listeners
/// sharing one table, so a certificate installed per listener would appear on one port and not the
/// other — the same one-protocol-only failure, one layer up.
///
/// The resolver arrives through a cell rather than as a field because the two are mutually
/// dependent and only one order can be written: the resolver is composed with the engine's
/// challenge source, so the engine has to exist first, and the engine needs somewhere to install
/// what it issues. A cell breaks the cycle without a lock on any path that matters — nothing writes
/// it after startup, and the only read is on an issuance, which happens seconds or weeks later.
struct ResolverSink {
    resolver: Arc<std::sync::OnceLock<Arc<volucer_http::SniResolver>>>,
    /// Whether what this sink installs should become the **default** certificate.
    ///
    /// True for the deployment's own table, false for a site's — see
    /// [`SniResolver::install_issued`]. A site's engine must not make its certificate answer for
    /// names it was not issued for, which the default entry does.
    takes_default: bool,
}

impl volucer_acme::CertificateSink for ResolverSink {
    fn install(&self, pem: &[u8], domains: &[String]) -> Result<(), volucer_acme::AcmeError> {
        let Some(resolver) = self.resolver.get() else {
            // Only reachable if an issuance completed between the engine starting and the
            // resolver being installed, which `serve` does not allow — but a missing resolver is a
            // certificate installed nowhere, and that is the failure this whole path exists to
            // prevent, so it is an error rather than a silent success.
            tracing::error!(
                "an issued certificate arrived before the serving resolver was installed"
            );
            return Err(volucer_acme::AcmeError::State);
        };

        // `rustls-acme` hands over the certificate and key as *one* PEM blob — the shape its cache
        // stores — so the split is by PEM label rather than by position: a chain may have any
        // number of certificates followed by exactly one key, and "first is the cert, second is
        // the key" would break the moment a chain had an intermediate.
        let (cert_pem, key_pem) = split_pem_pair(pem);
        match resolver.install_issued(cert_pem, key_pem, domains, self.takes_default) {
            Ok(replaced) => {
                tracing::info!(
                    domains = ?domains,
                    named_before = replaced.named(),
                    "the issued certificate is now being served"
                );
                Ok(())
            }
            Err(err) => {
                tracing::error!(%err, "an issued certificate could not be installed");
                Err(volucer_acme::AcmeError::State)
            }
        }
    }
}

impl std::fmt::Debug for ResolverSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolverSink")
            .field("installed", &self.resolver.get().is_some())
            .finish_non_exhaustive()
    }
}

/// Split one PEM blob into its certificate chain and its private key.
///
/// By PEM *label* rather than by position, because that is what the two actually are and a chain is
/// not a fixed length: the blob is the chain (leaf, then any intermediates) followed by the key, so
/// a reader that took "the second block" would hand an intermediate over as the key.
///
/// A blob with no key yields an empty key half rather than a guess: it cannot be installed either
/// way, and the loader names the reason (`KeyError::Empty`) instead of a parser failing on an
/// intermediate.
fn split_pem_pair(pem: &[u8]) -> (&[u8], &[u8]) {
    const KEY_MARKERS: [&str; 4] = [
        "-----BEGIN PRIVATE KEY-----",
        "-----BEGIN RSA PRIVATE KEY-----",
        "-----BEGIN EC PRIVATE KEY-----",
        "-----BEGIN ENCRYPTED PRIVATE KEY-----",
    ];

    match KEY_MARKERS
        .iter()
        .filter_map(|marker| find_bytes(pem, marker.as_bytes()))
        .min()
    {
        Some(at) => (&pem[..at], &pem[at..]),
        // No key marker: the whole blob is the chain, and the key half is empty so the loader
        // reports a missing key rather than the chain being silently truncated.
        None => (pem, &[]),
    }
}

/// The offset of `needle` in `haystack`, or `None`.
///
/// `windows` rather than a dependency: this runs once per issuance, on a few kilobytes.
fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Run the configured listeners until interrupted.
///
/// Both front-ends share one dispatcher, so a request gets the same backend
/// whichever protocol carried it. The FastCGI listener runs on this thread; the
/// HTTP listener needs a `tokio` runtime, so it runs on one of its own. That is
/// the async boundary made concrete: the runtime exists for the HTTP front-end,
/// and nothing else in the program depends on it.
fn serve(
    path: &Path,
    document_root_flag: Option<&Path>,
    front_controller_flag: Option<&str>,
    max_connections: usize,
) -> Result<(), ServerError> {
    let config = load_and_validate(path)?;
    let document_root = absolute_document_root(&resolve_document_root(document_root_flag, &config));
    let front_controller = config
        .server
        .resolve_front_controller(front_controller_flag);

    // The command line describes the *default* site. A site states its own root, so a
    // flag that named one would silently mean "the default site only" — the kind of
    // setting this file already refuses to give two meanings (see the empty-string rule
    // on `front_controller`).
    if !config.sites.is_empty() {
        if document_root_flag.is_some() {
            return Err(ServerError::Misconfigured {
                backend: "<cli>".to_string(),
                reason: "--document-root cannot be combined with [sites]: each site states its \
                         own document_root"
                    .to_string(),
            });
        }
        if front_controller_flag.is_some() {
            return Err(ServerError::Misconfigured {
                backend: "<cli>".to_string(),
                reason: "--front-controller cannot be combined with [sites]: each site states its \
                         own front_controller"
                    .to_string(),
            });
        }
    }

    // One routing, derived once, from the same table the sites are. The command line's
    // controller is folded in here rather than afterwards so the default site is
    // described in one place.
    let sites = Arc::new(Sites::from_config_with(
        &config,
        &document_root,
        front_controller.as_deref(),
    ));
    let http_address = config.server.http.clone();
    let fastcgi_address = config.server.fastcgi.clone();
    if http_address.is_none() && fastcgi_address.is_none() {
        return Err(ServerError::NoListener);
    }

    // Backends start before a listener binds: a misconfigured backend should be
    // reported before a port is occupied. One dispatcher serves both front-ends,
    // shared without a lock because each backend slot synchronises internally —
    // so two requests to two workers run at the same time.
    let dispatcher = Dispatcher::from_config_in(
        &config,
        // The default site's routing, taken from the table rather than rebuilt: a widened
        // `[server]` list has to reach the dispatch path, and rebuilding the value here is how
        // one of the two would quietly get the default list instead.
        routing_of(sites.default_site()),
        path.parent().unwrap_or_else(|| Path::new("")),
    )?;
    for name in dispatcher.backend_names() {
        tracing::info!(backend = %name, "backend ready");
    }
    // The same rule `check` prints, enforced where it matters. `serve` builds the backend — so a
    // bad origin or an unreadable CA is already a startup failure — but until the HTTP front-end
    // gains its branch for an upstream there is no request path for one, and a server that started
    // and then 500'd every request for that backend would be a configuration error wearing an
    // application bug's clothes.
    // A static-only deployment answers a path that names no file with a 500 and
    // nothing else in the log, so the reason is stated once here rather than left
    // to be inferred from a request that failed for no visible cause.
    if !dispatcher.has_backends() {
        tracing::warn!(
            "no backends are configured: only paths naming a real file will be answered, and \
             every other path will be a 500"
        );
    }
    let dispatcher = Arc::new(dispatcher);

    println!("volucer");
    println!("  document root:    {}", document_root.display());
    println!(
        "  front controller: {}",
        front_controller
            .as_deref()
            .unwrap_or("(none: only real .php files are served)")
    );
    println!("  backends:         {}", config.backends.len());
    if !config.routes.is_empty() {
        println!("  routes:           {}", config.routes.len());
    }
    if !config.sites.is_empty() {
        println!("  sites:            {}", config.sites.len());
        for (name, site) in &config.sites {
            println!("    {name}: {}", site.hosts.join(", "));
        }
    }
    // The table is built just above, so this reports what will actually serve rather than what
    // the file says. That matters for exactly one setting: a static layer turned off because the
    // document root has no asset to serve. Printing "serving from the document root" there would
    // be the silent version of the change.
    let suppressed = sites.suppressed_static_layer();
    let suppressed_default = suppressed.iter().any(|site| site.name == "<default>");
    if suppressed_default {
        println!(
            "  static files:     off (the document root holds no asset a file could be served \
             from)"
        );
    } else if config.server.statics().enabled {
        println!("  static files:     serving from the document root");
    }
    for site in suppressed.iter().filter(|site| site.name != "<default>") {
        println!(
            "    {}: off (no asset in {} a file could be served from)",
            site.name,
            site.document_root.display()
        );
    }

    // Reported before the listeners start, not after: a document root that is
    // not there is the mistake that produces a 404 for every static file and a
    // front controller PHP will not run, and noticing it in the log is worth
    // more than the process refusing to start. Only in the log, so a deployment
    // that creates the root during startup still works.
    for warning in volucer_core::preflight::inspect_with(&config, &document_root, Some(path)) {
        tracing::warn!("{}", warning.message);
        if let Some(hint) = &warning.hint {
            tracing::warn!("  {hint}");
        }
    }

    // A report on shutdown needs numbers that only exist at shutdown, so the
    // dispatcher stays reachable instead of being dropped with `Arc`'s last
    // strong count below.
    let reporting = Arc::clone(&dispatcher);

    // One handle, cloned into every listener, so a reload moves all of them or none. It must be
    // built here rather than per listener: a listener that built its own would keep serving the
    // old configuration after a reload, which takes effect on one protocol and reads as the
    // other front-end being right.
    //
    // The registry is what a *trigger* holds: it knows the one shared site table and every
    // front-end's snapshot-holder, so `apply` cannot half-succeed. Building it here rather than
    // inside a listener is what makes the process's own `SIGHUP` path able to reach a FastCGI
    // listener at all.
    let registry = Arc::new(volucer_http::Registry::new(volucer_http::SharedState::new(
        (*sites).clone(),
    )));

    // **One observability handle for the whole process**, built before any listener binds.
    //
    // One, not one per listener, and for the reason the site table is one: a deployment behind nginx
    // runs both front-ends, so two access logs would put one application's requests in two files
    // depending on which protocol received them, and two id sources would make the same request
    // carry different ids on the two hops. Building it here is also what makes "off by default"
    // true — `Observability::disabled` is what a config with neither table produces, and every path
    // below then costs one `is_active` check per request.
    //
    // The exporter comes back with it and is held for the rest of the run: dropping it stops the
    // thread, and its last flush is what puts the final seconds of a deployment's traffic into its
    // traces. `telemetry` is therefore deliberately *not* a local of a helper.
    let (observe, telemetry) = build_observability_with_exporter(&config, path);
    let observe = Arc::new(observe);
    if let Some(endpoint) = telemetry.endpoint() {
        println!("  telemetry:        {endpoint}");
    }

    // The rate limit, when the deployment asked for one. Built here — before either front-end —
    // because one instance has to reach **both**. Two would give a client twice the configured
    // allowance depending on which protocol received its request, which no single-front-end test can
    // see. Restart-only, and the buckets are why: replacing the policy on a `SIGHUP` would reset every
    // caller's allowance, which is a limit that can be escaped by asking the operator to reload.
    let rate_limit = config
        .server
        .rate_limiter()
        .map_err(|reason| ServerError::Listener { reason })?
        .map(Arc::new);

    // The FastCGI listener binds first, because binding is synchronous here while
    // the HTTP listener needs a runtime — and an occupied port should be reported
    // before the runtime is built.
    let fastcgi = match &fastcgi_address {
        Some(address) => {
            let listener = FastCgiListener::bind_registered(
                address,
                Arc::clone(&dispatcher),
                Arc::clone(&registry),
                Limits {
                    max_connections,
                    max_body_bytes: config.server.max_body_bytes(),
                },
            )?
            .with_observability(Arc::clone(&observe));
            let listener = match rate_limit.as_ref() {
                Some(limit) => listener.with_rate_limit(Arc::clone(limit)),
                None => listener,
            };
            println!("  fastcgi:          {}", listener.local_addr()?);
            Some(listener)
        }
        None => {
            println!("  fastcgi:          (disabled)");
            None
        }
    };

    // TLS applies to `http` unless a dedicated `https` listener was configured, in which case
    // `http` stays plaintext and `https` carries it. Validation has already refused every
    // combination outside that matrix, so this only picks between the two shapes.
    let config_dir = path.parent().unwrap_or_else(|| Path::new(""));
    let tls_active = config.server.tls.is_some();
    let https_address = config.server.https.clone();
    let tls_on_http = tls_active && https_address.is_none();
    let wants_redirect = config.server.redirect_to_https;

    // The redirect lives on whichever listener serves cleartext. With a split :80/:443 that is
    // `http` (which has no TLS of its own), and with TLS sharing the port it is the cleartext
    // half of that one listener. Validation has already ensured some plaintext listener exists.
    //
    // Only a TCP listener has a port. A unix socket cannot be reached over a redirect at all,
    // so it yields `None` and the redirect falls back to the listener's own port.
    let https_port = match &https_address {
        Some(address) => match volucer_core::config::parse_listener(address) {
            Ok(volucer_core::config::Listener::Tcp(addr)) => Some(addr.port()),
            _ => None,
        },
        None => None,
    };

    // `tls` and the redirect are separate settings on purpose: the plaintext listener in a
    // split deployment carries the redirect with *no* acceptor, and giving it one would turn it
    // into a TLS port that no cleartext client could reach.
    let http_settings = volucer_http::ListenerSettings {
        tls: if tls_on_http {
            Some(volucer_http::TlsSettings {
                config: &config,
                config_dir,
            })
        } else {
            None
        },
        redirect_to_https: wants_redirect && config.server.has_plaintext_listener(),
        https_port,
    };

    // The certificate table is built **once for the process**, before any port is bound, and
    // handed to every TLS-speaking listener.
    //
    // One resolver rather than one per listener, because a renewal has to reach all of them: an
    // `:80`/`:443` deployment has two TLS-speaking listeners, so two resolvers would mean a
    // renewed certificate appeared on one port and not the other — the same one-protocol-only
    // failure the shared site table exists to prevent, one layer down. A plaintext deployment
    // builds none at all, which is the whole of "TLS costs nothing when it is off".
    //
    // Loading it here rather than inside the listener is also the ordering hazard 56 was about:
    // a certificate that cannot be read must fail *before* a port is taken, not after `serve` has
    // printed that the port came up.
    let tls_state = match tls_active {
        true => Some(Arc::new(
            volucer_http::SniResolver::from_config(&config, config_dir).map_err(|err| {
                ServerError::Listener {
                    reason: err.to_string(),
                }
            })?,
        )),
        false => None,
    };

    // The ACME engines, when configured: one for the deployment's table and one per site that has
    // its own. All built *before* the listeners bind, because a challenge handshake can arrive as
    // soon as a port is open, so every challenge source has to be composed into the resolver already.
    //
    // One engine per table rather than one with several domain sets, because each has its **own
    // order and its own account** — and because `rustls-acme` keeps one challenge at a time per
    // state, so one state for two orders would have the second challenge overwrite the first.
    //
    // Each engine needs somewhere to install what it issues, and every one of those places has to be
    // the **one** table the listeners serve from — otherwise a certificate reaches one port and not
    // another. So all the cells are collected first and filled once the composed resolver exists;
    // and the composition happens once, at the end, because building it per site would drop the
    // earlier sites' challenge sources.
    let mut scopes: Vec<AcmeScope> = Vec::new();
    if let Some(acme) = &config.server.acme {
        if acme.enabled {
            let contact_missing = acme.contact.is_empty();
            scopes.push(AcmeScope {
                label: "server.acme".to_string(),
                domains: acme.domains.clone(),
                authority: acme.authority,
                cache: acme_cache(config_dir, &acme.cache),
                site: None,
                contact: acme.contact.clone(),
                contact_missing,
            });
        }
    }
    for (name, site) in &config.sites {
        let Some(site_acme) = &site.acme else {
            continue;
        };
        // A table that is turned off, or that sits beside a certificate the site names itself,
        // orders nothing — and must not, because either one means the site is served by the
        // deployment's certificate or by the pair it named. Building an engine anyway would start
        // an order nobody asked for, against a per-week rate limit.
        if !site_acme.issues() {
            continue;
        }
        let (names, _dropped) = site_acme.resolve_domains(&site.hosts);
        let contact = site_acme.resolved_contact(&config.server.acme).to_vec();
        scopes.push(AcmeScope {
            label: name.to_string(),
            domains: names,
            authority: site_acme.resolved_authority(&config.server.acme),
            cache: acme_cache(config_dir, &site_acme.resolved_cache(&config.server.acme)),
            site: Some(name.clone()),
            contact_missing: contact.is_empty(),
            contact,
        });
    }

    let mut cells: Vec<Arc<std::sync::OnceLock<Arc<volucer_http::SniResolver>>>> = Vec::new();
    let mut acme: Vec<volucer_acme::Engine> = Vec::new();
    for scope in &scopes {
        let cell = Arc::new(std::sync::OnceLock::new());
        let engine = volucer_acme::Engine::new(
            scope.domains.clone(),
            acme_authority(scope.authority),
            scope.cache.clone(),
            scope.contact.clone(),
            Arc::new(ResolverSink {
                resolver: Arc::clone(&cell),
                takes_default: scope.site.is_none(),
            }),
        )
        .map_err(|err| ServerError::Listener {
            reason: err.to_string(),
        })?;
        cells.push(cell);
        acme.push(engine);
    }

    // The challenge sources are composed in **before** any port is bound, and each only answers a
    // challenge handshake — `SniResolver::resolve` consults them behind an exact `acme-tls/1` ALPN
    // match, so a normal handshake is answered by the site table exactly as it was.
    //
    // The resolver object is rebuilt rather than mutated, because the ACME field is set at
    // construction. The *table* is shared into the new resolver, so an installation through a cell
    // and a reload through the old handle both reach the same place — which is why one cell per
    // engine still means one table.
    let tls_state = match tls_state {
        Some(resolver) if !acme.is_empty() => {
            let mut composed =
                volucer_http::SniResolver::from_shared(resolver.table(), resolver.config_dir());
            for (scope, engine) in scopes.iter().zip(&acme) {
                composed = match &scope.site {
                    None => composed.with_acme(Arc::new(engine.challenge_source())),
                    Some(site) => composed
                        .with_site_acme(site.clone(), Arc::new(engine.site_challenge_source(site))),
                };
                println!(
                    "  acme:             ({}) {}",
                    scope.label,
                    scope.domains.join(", ")
                );
                println!("    authority:      {}", scope.authority.as_str());
            }
            let composed = Arc::new(composed);
            for cell in &cells {
                if cell.set(Arc::clone(&composed)).is_err() {
                    return Err(ServerError::Listener {
                        reason: "the ACME sink could not be wired to the serving resolver"
                            .to_string(),
                    });
                }
            }
            for scope in &scopes {
                if scope.site.is_none() && scope.contact_missing {
                    tracing::warn!(
                        "no server.acme.contact is set, so the CA has no way to warn this \
                         deployment that renewals are failing; `volucer check` reports the days \
                         left instead"
                    );
                }
            }
            Some(composed)
        }
        // ACME configured and no TLS: validation refuses this, so reaching it means the two rules
        // have come apart.
        other if !acme.is_empty() => {
            let _ = other;
            return Err(ServerError::Listener {
                reason: "[server.acme] needs [server.tls]: the TLS-ALPN-01 challenge is validated \
                         on the TLS port"
                    .to_string(),
            });
        }
        other => other,
    };

    // The cached certificate is installed before anything is bound, so a restart serves it from the
    // first handshake rather than from whenever the state's own first poll completes. Blocking here
    // is deliberate: it is one file read, and a listener that accepted a handshake before it
    // finished would be presenting the placeholder to a client that should have had the issued one.
    //
    // `run` installs it too, and the two are not redundant: a renewal has to reach the table months
    // after startup, and that is the event path. This one is what makes the *window before the first
    // poll* serve the issued certificate — the engine's own `load_cert` step is asynchronous, and
    // `serve` does not wait for it.
    //
    // Every engine is drained in turn, and the order does not matter: each installs for **its own**
    // names, and `install_issued` is a partial update that leaves every other entry alone. So a
    // deployment with a table per site ends up serving all of them, whatever the order.
    for engine in &acme {
        if futures::executor::block_on(engine.install_cached()) {
            tracing::info!(
                domains = ?engine.domains(),
                "the cached certificate is serving before any port was bound"
            );
        }
    }

    // The handles one listener needs: the shared table, the one registry, and TLS when it speaks
    // it. Built per listener because the resolver is `Some` for a TLS listener and `None` for a
    // plaintext one, which is what keeps a plaintext listener from carrying TLS state at all.
    //
    // The client-address policy rides here rather than being a second argument to each binder, so a
    // deployment behind nginx and one that is not differ in the configuration alone. It is built
    // once, before the closure, because `[server.proxy]` is restart-only and so cannot move on a
    // reload — `restart_only` is the list an operator is shown, and it names this table.
    let client_ip = Arc::new(
        config
            .server
            .client_ip()
            .map_err(|reason| ServerError::Listener { reason })?,
    );
    let handles_for = |speaks_tls: bool| {
        let handles = volucer_http::ReloadHandles::new(Arc::clone(&registry))
            .with_observability(Arc::clone(&observe))
            .with_client_ip(Arc::clone(&client_ip));
        let handles = match rate_limit.as_ref() {
            Some(limit) => handles.with_rate_limit(Arc::clone(limit)),
            None => handles,
        };
        match (speaks_tls, &tls_state) {
            (true, Some(resolver)) => handles.with_tls(Arc::clone(resolver)),
            _ => handles,
        }
    };

    // The metrics and status listener, when the configuration asks for one. Built here — after the
    // dispatcher exists, because readiness reads it, and **before** the request listeners bind, so a
    // bad address on a metrics table is reported before a serving port is taken.
    //
    // It is not one of the `http`/`https` listeners and deliberately shares none of their settings:
    // those face the public and may be TLS-only, while this one is meant to be reachable by a
    // scraper and an orchestrator and by nothing else.
    let status = start_status(&config, &dispatcher)?;

    // The consumer side of the queues, when the deployment consumes anything. **Started before the
    // listeners**, so a consumer pool that cannot boot its processes is a startup failure rather than a
    // queue that fills up while the server serves 200s: the pool's own constructor boots every worker and
    // completes its handshake, which is the same guarantee a backend gets and for the same reason.
    //
    // `None` in three cases, and they are three different answers: no `[jobs]` table at all, a `[jobs]`
    // table with nothing in `consume` (a producer-only node, which is a documented deployment), and a
    // `[jobs]` table whose `pool` is absent. The last is refused by `Config::validate` when `consume` is
    // non-empty, so it cannot arrive here as a silent no-consumer — that is the failure that would look
    // like a working server.
    let consumers = start_consumers(&config, &dispatcher)?;
    if let Some(consumers) = &consumers {
        println!(
            "  consumers:        {} consumer(s) on {:?}",
            consumers.len(),
            consumers.pipelines()
        );
    }

    let http = match &http_address {
        Some(address) => {
            let running = RunningHttp::start_with_tls(
                address,
                Arc::clone(&dispatcher),
                handles_for(tls_on_http),
                HttpLimits {
                    max_connections,
                    max_body_bytes: config.server.max_body_bytes(),
                },
                http_settings,
            )
            .map_err(|err| ServerError::Listener {
                reason: err.to_string(),
            })?;
            if tls_on_http {
                println!("  http:             {} (https)", running.local_addr());
            } else {
                println!("  http:             {}", running.local_addr());
            }
            Some(running)
        }
        None => {
            println!("  http:             (disabled)");
            None
        }
    };

    let https = match &https_address {
        Some(address) => {
            let running = RunningHttp::start_with_tls(
                address,
                Arc::clone(&dispatcher),
                handles_for(true),
                HttpLimits {
                    max_connections,
                    max_body_bytes: config.server.max_body_bytes(),
                },
                // TLS only: this listener is already the https endpoint, so a redirect here
                // would send a client to where it came from.
                volucer_http::ListenerSettings {
                    tls: Some(volucer_http::TlsSettings {
                        config: &config,
                        config_dir,
                    }),
                    redirect_to_https: false,
                    https_port,
                },
            )
            .map_err(|err| ServerError::Listener {
                reason: err.to_string(),
            })?;
            println!("  https:            {} (https)", running.local_addr());
            Some(running)
        }
        None => {
            if tls_active {
                println!("  https:            (disabled)");
            }
            None
        }
    };

    println!("press Ctrl-C to stop");

    // Each engine is moved **into** a thread of its own, which is what keeps `Arc<Engine>` — and
    // therefore `AcmeState`'s lack of `Sync` — out of the signal handler entirely. The reloader needs
    // no handle to any of them: what it asks is the *resolver*, which is `Sync` because it has to be.
    for engine in acme {
        std::thread::Builder::new()
            .name("acme".to_string())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(err) => {
                        tracing::error!(
                            %err,
                            "no runtime for ACME issuance; certificates will not be renewed"
                        );
                        return;
                    }
                };
                runtime.block_on(engine.run());
            })
            .map_err(|err| ServerError::Listener {
                reason: format!("the ACME task could not be started: {err}"),
            })?;
    }

    install_shutdown_handler(
        fastcgi.as_ref().map(|l| l.shutdown_handle()),
        Arc::clone(&registry),
        tls_state.clone(),
        Arc::clone(&observe),
        path.to_path_buf(),
        document_root_flag.map(Path::to_path_buf),
        front_controller_flag.map(str::to_string),
    );

    // The poller, on its own thread: the pool is synchronous by construction, and the shutdown flag this
    // checks between jobs is the same flag the signal handler sets, so Ctrl-C stops the reserving before
    // the listeners go and a job already handed to a consumer is waited for rather than abandoned.
    //
    // Joined below and never detached, because the thread holds the pool: leaving it running past `serve`
    // would leave it holding a slot and reading a pipe while the process tried to reap the consumers.
    let poller = match &consumers {
        Some(consumers) => {
            let running = Arc::clone(consumers);
            let stop = running.stopper();
            Some((
                stop,
                std::thread::spawn(move || running.run()),
                Arc::clone(consumers),
            ))
        }
        None => None,
    };

    // The FastCGI listener owns this thread while the HTTP listener runs on its
    // own, so both are served concurrently. With no FastCGI listener there is
    // nothing to drive, so the process waits on the shared shutdown flag instead.
    match fastcgi {
        Some(listener) => listener.run()?,
        None => wait_for_shutdown(),
    }

    // Stopping the HTTP listener joins its thread, which drops the dispatcher on
    // the other side when both shared handles are gone — and with it the worker
    // pool, which reaps every PHP process.
    if let Some(http) = http {
        let stats = Arc::clone(&http.stats());
        http.stop();
        let (_, requests, _, failed) = stats.snapshot();
        println!("http: {requests} request(s), {failed} failed");
        report_tls(&stats);
    }
    if let Some(https) = https {
        let stats = Arc::clone(&https.stats());
        https.stop();
        let (_, requests, _, failed) = stats.snapshot();
        println!("https: {requests} request(s), {failed} failed");
        report_tls(&stats);
    }

    // Stopped last, after the request listeners have gone: a scraper that reads a pool count while
    // the pools are being reaped would see it fall to zero for a reason that has nothing to do with
    // the deployment's health, and the last scrape of a shutdown is exactly the one an operator
    // looks at.
    if let Some(status) = status {
        status.stop();
    }

    // And the trace exporter last of all, **before** the summary is printed below. That order is the
    // point: stopping it joins the thread and flushes the final batch, so the numbers a reader is
    // about to see are final. A summary that reported the counts before the flush would understate
    // every run by up to one batch — and understate exactly the runs that ended cleanly.
    telemetry.stop();
    if let Some(counts) = telemetry.counts() {
        println!(
            "telemetry: {} span(s) exported, {} lost to the collector, {} dropped at the queue",
            counts.exported, counts.failed, counts.dropped
        );
    }

    // Printed rather than traced: a load test wants this line in the same log as
    // the run it describes, and a filter that could silence it is the last thing a
    // diagnostic should depend on.
    for contention in reporting.contention() {
        println!("{}", contention_line(&contention));
        // **Printed only when something was lost**, and separately from the line above because it is a
        // different kind of fact: the contention figures are diagnostics that are always interesting,
        // while a drop count of zero is noise. A deployment with no `[metrics]` table has no other way
        // to learn that its worker-emitted samples were refused, which is why this is here rather than
        // only in the registry.
        if contention.dropped_metrics > 0 {
            println!(
                "metrics: {} worker-emitted sample(s) dropped for the backend {:?} — its queue was \
                 full, and a scrape is what drains it",
                contention.dropped_metrics, contention.backend
            );
        }
    }

    // The consumers go last, after the listeners and after the last scrape. Stopping them earlier would
    // make the final metrics scrape — the one an operator reads to find out what the run did — report a
    // pool of zero consumers for a reason that has nothing to do with the deployment.
    //
    // The stop is requested, then the thread is joined, and the join is the graceful part: the flag is
    // checked *between* jobs, so a consumer already working finishes and its answer is settled before the
    // pool below drops. Killing the thread instead would leave a reserved job to wait out its lease, which
    // is the exact cost the lease exists to bound.
    if let Some((stop, poller, consumers)) = poller {
        stop.store(true, Ordering::Relaxed);
        if poller.join().is_err() {
            // The poller thread panicked. Nothing to do about it here — the process is stopping, and the
            // panic has already been reported by the default hook — but it is *reported* rather than
            // silently joined, because a poller that died mid-job is the difference between "the queue
            // drained" and "the queue stopped being read".
            tracing::error!("the job poller panicked");
        }
        let (acked, requeued, rejected) = consumers.counts();
        println!("jobs: {acked} acknowledged, {requeued} requeued, {rejected} rejected");
        // Dropped explicitly so the consumer processes are reaped *here*, in the shutdown report, rather
        // than by `Arc`'s last strong count somewhere below — the same reason the dispatcher is kept in a
        // `reporting` handle above.
        drop(consumers);
    }

    println!("stopped");
    Ok(())
}

/// Print the TLS counters, when the listener had TLS at all.
///
/// Printed rather than traced for the same reason the request counts are: a failed handshake is
/// invisible from the client's side — the client sees a handshake error and never reaches this
/// log — so the number is the only signal that a port is refusing everyone.
fn report_tls(stats: &volucer_http::HttpStats) {
    let (handshakes, failures, plaintext) = stats.tls_snapshot();
    if handshakes == 0 && failures == 0 && plaintext == 0 {
        return;
    }
    println!("tls: {handshakes} handshake(s), {failures} failed");
    if plaintext > 0 {
        println!("tls: {plaintext} cleartext connection(s) accepted on the TLS port");
    }
}

/// The one line of slot-contention report, as a string.
///
/// Its own function because it is a format, not a side effect: the numbers are what a
/// measurement is read from, so the text they land in is worth a test that does not need
/// a server, a signal or a terminal.
fn contention_line(contention: &Contention) -> String {
    format!(
        "slots: {} worker(s), {} request(s), wait mean {} us, wait p50 {} us, wait p99 {} us, \
         busiest slot {} at {} us",
        contention.workers,
        contention.served,
        contention.mean_us,
        contention.p50_us,
        contention.p99_us,
        contention.busiest,
        contention.busiest_us,
    )
}

/// Readiness, answered from the dispatcher.
///
/// A struct rather than a closure because the endpoint holds it for the process's life and both
/// methods are needed; a pair of closures would be two things to keep in step.
struct DispatcherReadiness {
    dispatcher: Arc<Dispatcher>,
}

impl volucer_http::Readiness for DispatcherReadiness {
    fn is_ready(&self) -> bool {
        self.dispatcher.is_ready()
    }

    fn reasons(&self) -> Vec<String> {
        self.dispatcher.readiness_reasons()
    }
}

/// The exposition, read from the dispatcher's pools at the moment of the scrape.
///
/// This is where `/metrics` stops being a static page and starts describing the process. The
/// registry is the process's one, the `refresh` walks every worker pool, and the render is what the
/// registry already does — so the ordering that matters (read, then render) lives in one place
/// instead of being a rule the listener has to remember.
struct DispatcherMetrics {
    dispatcher: Arc<Dispatcher>,
    registry: &'static volucer_metrics::Registry,
    path: String,
    prefix: Option<String>,
}

impl volucer_http::MetricsSource for DispatcherMetrics {
    fn refresh(&self) {
        self.dispatcher.publish_metrics(self.registry);
    }

    fn render(&self) -> String {
        self.registry.render()
    }

    fn path(&self) -> &str {
        &self.path
    }

    fn prefix(&self) -> Option<&str> {
        self.prefix.as_deref()
    }
}

/// The process's metric registry, with the `rr_http_*` families already described in it.
///
/// **Process-wide, because one `/metrics` page has to describe the whole process.** Two registries
/// would be two scrapes with half the numbers in each, and a worker pool that published into one
/// while the listener rendered the other is exactly the class of bug this step exists to avoid.
///
/// It is initialised on first use, which is the first scrape — so a deployment with no `[metrics]`
/// table never builds it at all, and the `describe` calls (idempotent) cost nothing.
///
/// **The application's own names come from the configuration, and they arrive through
/// [`describe_collected`] rather than here**, because the `LazyLock` initialiser cannot see a
/// `Config`. Both are idempotent, so the order the two run in does not matter — which is what lets
/// the registry keep its "no global configuration" shape while still being process-wide.
static METRICS: std::sync::LazyLock<volucer_metrics::Registry> = std::sync::LazyLock::new(|| {
    let registry = volucer_metrics::Registry::new();
    volucer_metrics::http::describe(&registry);
    registry
});

/// Declare a deployment's `[metrics.collect]` names in the process registry.
///
/// A one-line forwarder, and the commentary lives on [`volucer_server::describe_collected`] — which
/// is where the function can be tested rather than copied. What this adds is the process's own
/// registry, which a test cannot reach.
fn describe_collected(metrics: &volucer_core::config::MetricsConfig) {
    volucer_server::describe_collected(&METRICS, metrics);
}

/// Build the metrics and status listener, when the configuration asks for one.
///
/// **One listener for both tables**, because a deployment that wants a scraper and an orchestrator
/// wants two paths on one port, and two ports would be two things to firewall. Which endpoints exist
/// is decided by which tables are present: `[metrics]` without `[status]` serves the exposition and
/// answers 404 to `/health`, and the reverse.
///
/// Where the two name different addresses it is a **refusal**, not a silent choice: two listeners
/// would double the thread, the runtime and the shutdown path to serve two paths that no deployment
/// needs on two ports, and quietly honouring one of the two settings would be a configuration that
/// does not mean what it says.
fn build_status(
    config: &Config,
    dispatcher: &Arc<Dispatcher>,
) -> Result<Option<RunningStatus>, ServerError> {
    if config.metrics.is_none() && config.status.is_none() {
        return Ok(None);
    }

    let readiness = || -> Arc<dyn volucer_http::Readiness> {
        Arc::new(DispatcherReadiness {
            dispatcher: Arc::clone(dispatcher),
        })
    };

    let address = match (&config.metrics, &config.status) {
        (Some(metrics), Some(status)) => {
            if metrics.address != status.address {
                return Err(ServerError::Misconfigured {
                    backend: "<status>".to_string(),
                    reason: format!(
                        "[metrics] listens on {} and [status] on {}; they share one listener, so \
                         give them the same address or configure only one",
                        metrics.address, status.address
                    ),
                });
            }
            status.address.clone()
        }
        (Some(metrics), None) => metrics.address.clone(),
        (None, Some(status)) => status.address.clone(),
        (None, None) => unreachable!("checked above"),
    };

    let metrics = config.metrics.as_ref().map(|metrics| {
        // Registered before the listener exists, so the first scrape already knows the application's
        // names. Doing it on scrape would mean the first few scrapes of a fresh process answered
        // "undeclared" for a name the deployment had declared — a race whose symptom is an empty
        // graph that fills in by itself.
        describe_collected(metrics);

        let source: Arc<dyn volucer_http::MetricsSource> = Arc::new(DispatcherMetrics {
            dispatcher: Arc::clone(dispatcher),
            registry: &METRICS,
            path: metrics.path.clone(),
            prefix: metrics.prefix.clone(),
        });
        source
    });

    let services = volucer_http::StatusServices {
        metrics,
        readiness: config.status.as_ref().map(|_| readiness()),
        unavailable_status: config
            .status
            .as_ref()
            .map_or(503, |status| status.unavailable_status_code),
        // The worker routes exist only when the deployment asked, and the implementation is the
        // dispatcher's own pool directory — the same one `workers.list` reads, so a `GET /workers`
        // and a worker's own `workers.list` cannot disagree about what a worker is.
        workers: workers_control(config, dispatcher),
    };

    RunningStatus::start(&address, services)
        .map(Some)
        .map_err(|err| ServerError::Listener {
            reason: err.to_string(),
        })
}

/// The worker-control routes, when `[status] allow_worker_reset` asks for them.
///
/// `None` in two cases, and they are the same answer for different reasons: the deployment did not
/// ask, or it asked and configured no worker pool at all — in which case there is nothing to list and
/// `/workers` would be an empty array that looks like a pool with no workers. A static-only or
/// upstream-only deployment gets no route rather than a misleading one.
fn workers_control(
    config: &Config,
    dispatcher: &Arc<Dispatcher>,
) -> Option<Arc<dyn volucer_http::WorkerControl>> {
    if !config
        .status
        .as_ref()
        .is_some_and(|status| status.allow_worker_reset)
    {
        return None;
    }
    let pools = dispatcher.pools()?;
    Some(Arc::new(PoolControl { pools }))
}

/// [`volucer_http::WorkerControl`] over the pool directory the call plane already reads.
///
/// A newtype rather than implementing the trait on `PoolDirectory`, so the HTTP crate depends on a
/// trait it declares and `volucer-server` never learns that a status listener exists.
struct PoolControl {
    pools: Arc<volucer_server::PoolDirectory>,
}

impl volucer_http::WorkerControl for PoolControl {
    fn workers(&self) -> Vec<volucer_server::WorkerStatus> {
        // Through the trait, so this is the *same* implementation `workers.list` answers from rather
        // than a second rendering of the pools that could drift from it.
        volucer_server::PoolLookup::workers(&*self.pools)
    }

    fn restart(&self, index: usize) -> std::result::Result<(), volucer_server::ResetRefusal> {
        volucer_server::PoolLookup::restart(&*self.pools, index)
    }
}

/// Start the metrics and status listener, when the configuration asks for one, and say where it
/// came up.
///
/// The address is printed rather than traced for the same reason the other listeners' are: an
/// operator looking for the port a scraper should poll wants it in the same block as `http` and
/// `fastcgi`, not behind a log filter.
fn start_status(
    config: &Config,
    dispatcher: &Arc<Dispatcher>,
) -> Result<Option<RunningStatus>, ServerError> {
    let running = build_status(config, dispatcher)?;
    if let Some(listener) = &running {
        println!("  metrics:          {}", listener.local_addr());
    }
    Ok(running)
}

/// Start the consumer side of the queues, when the configuration has any.
///
/// Three cases answer `None`, and they are three different things: no `[jobs]` table, a table with nothing
/// in `consume` (a producer-only node — a documented deployment, not a mistake), and a table whose `pool`
/// is absent. The last is refused by `Config::validate` whenever `consume` is non-empty, so it cannot
/// become a server that silently consumes nothing.
///
/// The pool is booted here, which is the point of doing this at startup: a consumer command that does not
/// exist, or that cannot complete its handshake, is reported before a port is bound rather than as a queue
/// that fills up while every request is answered 200.
fn start_consumers(
    config: &Config,
    dispatcher: &Arc<Dispatcher>,
) -> Result<Option<Arc<Consumers>>, ServerError> {
    let (Some(jobs), Some(plane)) = (&config.jobs, dispatcher.queues()) else {
        return Ok(None);
    };
    let Some(pool_config) = &jobs.pool else {
        return Ok(None);
    };
    // `consume` is checked against `pipelines` by `Config::validate`, and `pool` is required whenever
    // `consume` is non-empty — so by here a pool config means there is something to consume. The empty
    // check is what turns that into a statement rather than an assumption: without it, a configuration
    // that named a pool and consumed nothing would boot a set of consumer processes that poll nothing.
    let consumers = Consumers::new(
        &plane,
        WorkerPool::start_consumers("jobs", pool_config)?,
        Duration::from_secs(pool_config.job_lease_seconds),
        pool_config.prefetch,
    );
    if consumers.is_empty() {
        tracing::info!(
            workers = consumers.len(),
            "a jobs pool is configured but no pipeline is consumed, so it will not poll"
        );
    }
    Ok(Some(Arc::new(consumers)))
}

/// Wait until the shutdown flag is set.
///
/// Used when only the HTTP listener is configured: its thread owns the listener,
/// so this thread has nothing to drive and blocks on the same flag the Ctrl-C
/// handler sets.
fn wait_for_shutdown() {
    while !SHUTDOWN.load(Ordering::Relaxed) {
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}
/// Set once a Ctrl-C has been seen, so a listener that cannot be told directly
/// can still be waited on.
static SHUTDOWN: AtomicBool = AtomicBool::new(false);

/// Everything a reload needs to re-read the configuration and apply it.
///
/// One value rather than a handful of arguments, because that is what the handler closure has to
/// capture and a caller that supplied half of them would be a reload that reported success and
/// changed nothing.
///
/// **Unused on Windows**, where there is no `SIGHUP` and therefore nothing that could reload. It is
/// built and passed there anyway rather than `#[cfg]`-ed away, so the two platforms' handler
/// signatures stay the same and the difference is stated in one place — the `signals` module —
/// instead of in every call site.
#[cfg_attr(windows, allow(dead_code))]
struct Reloader {
    /// The path named on the command line. Re-read as-is, which is open decision 9.1's answer:
    /// what an operator expects from `SIGHUP`, and no new concept such as a marker file.
    config_path: PathBuf,
    /// The document root and front controller **the command line resolved**.
    ///
    /// Replayed on a reload because they are part of what the configuration *means*: a deployment
    /// started with `--document-root` on a site-less config takes its root from the flag, and a
    /// reload that dropped it would move the root to `.` — a working server that starts serving
    /// 404s, with the change attributed to whatever the operator edited.
    document_root_flag: Option<PathBuf>,
    front_controller_flag: Option<String>,
    /// The one registry: the shared table and every front-end.
    registry: Arc<volucer_http::Registry>,
    /// The process's certificate table, when TLS is on, so a renewed PEM is picked up too.
    ///
    /// Also the reload's answer to "does ACME manage these certificates?": the composed resolver is
    /// the thing that actually answers handshakes, so asking *it* rather than a second flag is what
    /// keeps the reload's decision from drifting away from the serving path. The engine itself
    /// cannot be held here — `AcmeState` is not `Sync`, so it cannot move into the signal thread.
    tls: Option<Arc<volucer_http::SniResolver>>,
    /// Request identity and the access log, so a `SIGHUP` closes and reopens the log.
    ///
    /// **Held as the same `Arc` both front-ends carry**, which is what makes the reopen visible to
    /// them: the sink lives behind a `Mutex` inside this value, so reopening moves the file every
    /// front-end is writing through rather than installing a second log they do not read.
    ///
    /// A `SIGHUP` action rather than a separate signal because that is nginx's convention and the
    /// one `logrotate`'s default `postrotate` script sends. See
    /// [`volucer_observe::access_log::AccessLog::reopen`].
    observe: Arc<volucer_observe::Observability>,
}

impl Reloader {
    /// Re-read the configuration file and apply it, whole or not at all.
    ///
    /// The two failure modes are reported and *nothing is changed*: a file that does not parse, and
    /// a file that parses and does not validate. That is the only safe direction — a reload that
    /// installed half of a file would leave the operator unable to tell which half took effect —
    /// and it is why this returns early rather than applying what it could.
    ///
    /// A reload that succeeds still may not have changed anything the implementation can honour;
    /// the settings that are **restart-only** are named in [`restart_only`] and printed, so an
    /// operator who edited one learns it here rather than from behaviour.
    #[cfg_attr(windows, allow(dead_code))]
    fn reload(&self) {
        // **The access log is reopened first, and unconditionally — before the file is even read.**
        //
        // Before, because the two are independent: a rotation is a change to the *filesystem*, and a
        // configuration that fails to parse must not leave the log writing into a renamed inode for
        // another day. An operator who sends a `SIGHUP` after `logrotate` and edits the file in the
        // same breath would otherwise get the error they can act on and not the repair they needed.
        //
        // Unconditionally, because a reopen is idempotent and cheap (one close, one open): asking
        // "did the path change" would need the old configuration, and the answer this must give for
        // an *unchanged* path is exactly the one `logrotate` requires.
        //
        // No branch on the result here: `false` means "no file is open", which is either a
        // deployment with no access log — ordinary, and not worth a line on every reload — or a
        // reopen that failed, which `AccessLog::reopen` has already reported at `error` with the
        // reason. A summary here would be the second line saying the same thing, or a misleading
        // one.
        if self.observe.reopen_access_log() {
            tracing::info!("access log reopened for a rotation");
        }

        let config = match Config::load(&self.config_path) {
            Ok(config) => config,
            Err(err) => {
                tracing::error!(%err, "reload refused: the configuration could not be read");
                return;
            }
        };
        if let Err(err) = config.validate() {
            tracing::error!(%err, "reload refused: the configuration is not valid");
            return;
        }

        // The site table, with the command line replayed, exactly as `serve` built the first one.
        // Built before anything is installed, so a table that cannot be built is a refusal rather
        // than a half-applied reload.
        let document_root = match &self.document_root_flag {
            Some(root) => absolute_document_root(root),
            None => absolute_document_root(&resolve_document_root(None, &config)),
        };
        let front_controller = config
            .server
            .resolve_front_controller(self.front_controller_flag.as_deref());
        let sites = Sites::from_config_with(&config, &document_root, front_controller.as_deref());

        let generation = self.registry.generation() + 1;
        let _previous = self.registry.apply(sites);
        tracing::info!(
            generation,
            file = %self.config_path.display(),
            document_root = %document_root.display(),
            sites = config.sites.len(),
            "configuration reloaded"
        );

        // Certificates are a second, independent table. Refreshed after the sites so that a
        // certificate that cannot be read leaves the *sites* reloaded and the certificates as they
        // were — one table refusing must not take the other with it, and both are whole-or-nothing
        // on their own.
        //
        // This runs even when ACME manages some of the deployment's certificates, and the **merge
        // inside `refresh`** is what makes that safe. It used to be skipped, because a rebuild from
        // the configuration alone would replace an issued certificate with a placeholder (hazard 71);
        // now the rebuild keeps what was issued and re-reads what is named. Skipping would have been
        // simpler and wrong in the other direction: a site's own `[sites.x.tls]` PEM *is* in the
        // configuration, and a renewal of it has to be picked up.
        if let Some(resolver) = &self.tls {
            match resolver.refresh(&volucer_http::CertificateSet::from_config(&config)) {
                Ok(_) => tracing::info!("certificate table refreshed"),
                Err(err) => {
                    tracing::error!(%err, "the certificate table was not replaced; the previous one is still serving")
                }
            }
        }

        for line in restart_only() {
            tracing::info!("{line}");
        }
    }
}

/// The settings a reload cannot honour, and the reason.
///
/// Named rather than discovered, which is the lesson of the TLS work: a setting the implementation
/// cannot apply has to be *stated*, or the operator finds out from behaviour instead — and a
/// reload that appears to have worked is worse than one that refused.
///
/// Returned as lines so the same text is what `serve` logs and what a test can assert on, rather
/// than a message assembled at the call site.
pub fn restart_only() -> Vec<&'static str> {
    vec![
        "a reload does not change: listener addresses (the sockets are already bound)",
        "a reload does not change: backends, their commands and worker counts (the processes are running)",
        "a reload does not change: [server.tls] min_version and alpn (the rustls ServerConfig is built once)",
        "a reload does not change: [server.proxy] (whose forwarded headers are believed is a statement about this deployment's topology, not about a request)",
        "a reload does not change: [server.rate_limit] (the buckets are live state, so replacing the policy would reset every caller's allowance)",
        "a reload does not change: [server.access_log] path, format and request_id_header (SIGHUP closes and reopens the log a rotation moved, but the destination is chosen at startup)",
        "a reload does not change: [status] address and allow_worker_reset (the listener is bound, and a route that mutates must not appear because a file was edited)",
    ]
}

/// Stop the listeners on the first Ctrl-C or SIGTERM, and **reload** on SIGHUP.
///
/// A second signal forces an exit, so a stuck shutdown cannot trap the operator.
///
/// **SIGTERM is what a supervisor actually sends**, and until this handled it the
/// process died on the spot with no chance to run a destructor — so `systemctl stop`,
/// `docker stop` and a plain `kill` all left the php-cgi children of a `spawn_cgi`
/// pool running. They accumulated: a benchmark host reached 68 orphans, and each one
/// held its port, its PHP state and its share of the CPU.
///
/// **SIGHUP reloads**, which is the conventional meaning on a Unix server and the reason the
/// handler has to distinguish the signals at all. That is what the platform split below is about,
/// and it is not merely a `#[cfg]` for tidiness: `ctrlc` registers `SIGHUP` under its `termination`
/// feature and runs the *shutdown* callback for it, so before this change `kill -HUP volucer`
/// stopped the server. Reproduced against the previous binary, not inferred: the process was gone
/// and the log read "shutting down".
///
/// Forcing is safe now, and was not before: the lifecycle guard reaps the PHP
/// processes when the process dies, *provided* the death is one it can observe.
///
/// The FastCGI listener takes a [`ShutdownHandle`], which is a shared flag. The
/// HTTP listener runs on another thread and is stopped by *dropping* its handle,
/// which is what `serve` does after this — so the only thing this handler has to
/// do for it is set the static flag that `wait_for_shutdown` polls.
fn install_shutdown_handler(
    shutdown: Option<volucer_server::ShutdownHandle>,
    registry: Arc<volucer_http::Registry>,
    tls: Option<Arc<volucer_http::SniResolver>>,
    observe: Arc<volucer_observe::Observability>,
    config_path: PathBuf,
    document_root_flag: Option<PathBuf>,
    front_controller_flag: Option<String>,
) {
    let forced = Arc::new(AtomicBool::new(false));
    let stop = {
        let forced = Arc::clone(&forced);
        move || {
            if forced.swap(true, Ordering::SeqCst) {
                // `error!`, not `warn!`: a second signal is the operator telling the process it has
                // run out of chances, and the exit code says the shutdown did not complete.
                tracing::error!("forced exit");
                std::process::exit(130);
            }
            // Through `tracing` rather than `eprintln!`, and this is not tidiness. These lines
            // used to bypass the subscriber entirely, so they could not be filtered, redirected,
            // captured or shipped — the one pair of messages an operator reads *while something is
            // wrong* was the one pair no log pipeline could see. The exit-on-second-signal and the
            // flag are unchanged.
            tracing::warn!("shutting down; signal again to force");
            SHUTDOWN.store(true, Ordering::Relaxed);
            if let Some(shutdown) = &shutdown {
                shutdown.shutdown();
            }
        }
    };

    let reloader = Reloader {
        config_path,
        document_root_flag,
        front_controller_flag,
        registry,
        tls,
        observe,
    };

    if let Err(err) = signals::install(stop, reloader) {
        // Not fatal, but it means a SIGTERM will leak the pools: say so at a level
        // an operator sees, because the symptom (surviving PHP processes) appears
        // long after the cause.
        tracing::warn!(%err, "no signal handler installed; stopping this process may leave its PHP children running, and SIGHUP cannot reload it");
    }
}

/// Signal handling, split by platform because the *mechanism* is — the behaviour is not.
///
/// The two implementations are deliberately one call apiece with the same meaning: stop on
/// SIGINT/SIGTERM, and reload on whatever this platform calls "reload". Windows has no `SIGHUP`
/// and no equivalent, so there the only signal is a stop, and reload is unavailable — which is
/// stated here rather than left for an operator on Windows to discover.
#[cfg(unix)]
mod signals {
    use super::Reloader;
    use signal_hook::consts::signal::{SIGHUP, SIGINT, SIGTERM};
    use signal_hook::iterator::Signals;
    use std::sync::Arc;

    /// Stop on SIGINT/SIGTERM, reload on SIGHUP.
    ///
    /// The signal number is what makes this possible, and it is the whole reason `signal-hook` is
    /// used here rather than the `ctrlc` this crate already depends on for Windows: `ctrlc` calls
    /// one handler for every signal it registered and cannot say which arrived, so a handler that
    /// had to tell a reload from a shutdown could not be written against it. Its `termination`
    /// feature also maps SIGHUP to the shutdown callback, which is what made `kill -HUP` stop the
    /// server.
    pub fn install(
        stop: impl Fn() + Send + 'static,
        reloader: Reloader,
    ) -> Result<(), std::io::Error> {
        let mut signals = Signals::new([SIGINT, SIGTERM, SIGHUP])?;
        let reloader = Arc::new(reloader);
        std::thread::Builder::new()
            .name("signals".into())
            .spawn(move || {
                for signal in signals.forever() {
                    match signal {
                        SIGHUP => {
                            tracing::info!("SIGHUP: reloading configuration");
                            reloader.reload();
                        }
                        SIGINT | SIGTERM => stop(),
                        // Registered signals only, so nothing else can arrive; named rather than
                        // silently ignored so that adding one to the set above without deciding
                        // what it means is visible here.
                        other => tracing::warn!(signal = other, "ignoring an unhandled signal"),
                    }
                }
            })
            .map(|_| ())
    }
}

#[cfg(windows)]
mod signals {
    use super::Reloader;
    use std::sync::Arc;

    /// Stop on Ctrl-C, Ctrl-Break and the console-close event.
    ///
    /// **There is no reload here, because Windows has no `SIGHUP`.** That is a property of the
    /// platform rather than a gap to work around, and it is why the reloader is dropped rather than
    /// stored: a handler that pretended to reload would be a signal nobody can send. A Windows
    /// deployment picks up a configuration change by restarting, exactly as it did before this
    /// phase.
    pub fn install(
        stop: impl Fn() + Send + 'static,
        _reloader: Reloader,
    ) -> Result<(), std::io::Error> {
        let _ = Arc::new(());
        ctrlc::set_handler(stop).map_err(std::io::Error::other)
    }
}

/// `check` validates configuration and reports the PHP binaries found here.
fn check(
    path: &Path,
    document_root_flag: Option<&Path>,
    extra_php_dirs: &[PathBuf],
) -> Result<(), volucer_core::Error> {
    let config = Config::load(path)?;
    config.validate()?;

    // The same resolution `serve` performs, so this reports the deployment that
    // would actually start rather than the one written in the file.
    let document_root = absolute_document_root(&resolve_document_root(document_root_flag, &config));
    let front_controller = config.server.resolve_front_controller(None);

    println!("configuration: {}", path.display());
    println!("  listeners:");
    match &config.server.http {
        Some(http) => println!("    http:    {http}"),
        None => println!("    http:    (disabled)"),
    }
    match &config.server.https {
        Some(https) => println!("    https:   {https}"),
        None => println!("    https:   (disabled)"),
    }
    match &config.server.fastcgi {
        Some(fcgi) => println!("    fastcgi: {fcgi}"),
        None => println!("    fastcgi: (disabled)"),
    }
    // Printed beside the listeners because it is a property of them rather than of a backend, and
    // because the value an operator most needs to see is the one they did *not* write: a deployment
    // with no key has a 32 MiB limit that appears nowhere in its file.
    println!(
        "    max body: {} ({} MiB; bounds both front-ends, not an upstream)",
        config.server.max_body_bytes(),
        config.server.max_body_bytes() / (1024 * 1024)
    );

    // The rate limit, or the absence of one stated out loud. The *absence* is what this prints in
    // the off case rather than printing nothing, because "no limit" and "a limit I configured
    // wrongly" look identical from a deployment's behaviour and only one of them is intended.
    match config.server.rate_limit.as_ref() {
        Some(limit) => {
            let limiter = config
                .server
                .rate_limiter()
                .map_err(volucer_core::Error::Config)?
                .expect("a stated table builds a policy");
            println!("    rate limit:");
            println!(
                "      key:      {}",
                match limiter.key() {
                    volucer_core::KeyMode::Ip => {
                        "ip (the address [server.proxy] resolves)".to_string()
                    }
                    volucer_core::KeyMode::Global =>
                        "global (one bucket for the process)".to_string(),
                    volucer_core::KeyMode::Header => format!(
                        "header {:?} -- not an authenticated identity; a client chooses it",
                        limiter.header().unwrap_or_default()
                    ),
                }
            );
            println!(
                "      rate:     {:.4} request(s)/second ({} per {} ms), burst {}",
                limiter.per_second(),
                limit.rate,
                limit.interval_ms,
                limiter.burst()
            );
            println!(
                "      entries:  {} (a full table of depleted buckets refuses new callers with a \
                 429)",
                limiter.max_entries()
            );
        }
        None => println!("    rate limit: none (server.rate_limit is unset; nothing is throttled)"),
    }

    // TLS, and which paths were actually resolved. The resolution is printed rather than
    // assumed because a certificate path is the one setting whose *file* the reader cannot
    // see from the config: `certs/a.crt` is relative to this file, not to wherever the
    // process started.
    //
    // A pair left out prints `(generated)` rather than nothing: it is a configuration an operator
    // chose, and the difference between "I forgot the certificate" and "it will be generated until
    // ACME issues" is the difference between a bug report and the intended behaviour.
    let generated_tls = server_tls_is_generated(&config);
    match (resolved_server_tls(&config, path), generated_tls) {
        (Some(server_tls), _) => {
            let tls = config
                .server
                .tls
                .as_ref()
                .expect("resolved implies present");
            println!("  tls:");
            println!("    certificate:      {}", server_tls.certificate.display());
            println!("    key:              {}", server_tls.key.display());
            println!(
                "    min version:      {}",
                match tls.min_version {
                    MinTlsVersion::V1_2 => "1.2",
                    MinTlsVersion::V1_3 => "1.3",
                }
            );
            println!("    alpn:             {}", tls.alpn.join(", "));
            if tls.plain {
                println!(
                    "    plain:            true (cleartext and TLS share the http port — \
                     accept it knowingly)"
                );
            } else {
                println!("    plain:            false");
            }
            if config.server.redirect_to_https {
                println!("    redirect_to_https: true");
            }
        }
        (None, true) => {
            let tls = config
                .server
                .tls
                .as_ref()
                .expect("generated implies present");
            println!("  tls:");
            println!(
                "    certificate:      (generated in memory: no pair is named, so a self-signed \
                 placeholder is made for the names this deployment answers to and replaced when \
                 ACME issues)"
            );
            println!(
                "    min version:      {}",
                match tls.min_version {
                    MinTlsVersion::V1_2 => "1.2",
                    MinTlsVersion::V1_3 => "1.3",
                }
            );
            println!("    alpn:             {}", tls.alpn.join(", "));
            if config.server.redirect_to_https {
                println!("    redirect_to_https: true");
            }
        }
        (None, false) => println!("  tls:              (disabled)"),
    }
    // A site's certificate is printed only when the site states one, for the same reason the
    // site table prints only stated keys: listing the inherited value per site would read as
    // though every site had been configured with it.
    for (name, site_tls) in resolved_site_tls(&config, path) {
        println!("    site {name}:        {}", site_tls.certificate.display());
    }

    // ACME, when configured. Printed even when the cache is empty, because that is a state an
    // operator has to be able to tell apart from a deployment whose issuance is working: silence
    // is the failure mode here, so what `check` can say is which authority, which names, and
    // whether a certificate for them is already cached.
    match &config.server.acme {
        Some(acme) => {
            println!("  acme:");
            println!("    enabled:          {}", acme.enabled);
            if !acme.enabled {
                println!(
                    "      DISABLED:       settings only; nothing is ordered. Every site that \
                     inherits them still issues."
                );
            }
            println!("    authority:        {}", acme.authority.as_str());
            if acme.authority == volucer_core::config::Authority::Production {
                println!("      PRODUCTION:     real certificates, against per-week rate limits");
            }
            match (acme.domains.is_empty(), acme.enabled) {
                // The empty list is legal only here, so it is explained rather than printed as a
                // blank — an operator who turned the table off and one who forgot `domains` would
                // otherwise see the same output for opposite situations.
                (true, false) => {
                    println!("    domains:          (none: a disabled table issues for nothing)")
                }
                (true, true) => println!("    domains:          (none)"),
                (false, _) => println!("    domains:          {}", acme.domains.join(", ")),
            }
            match acme.contact.is_empty() {
                true => println!(
                    "    contact:          (none: the CA has no way to warn you that renewals are \
                     failing)"
                ),
                false => println!("    contact:          {}", acme.contact.join(", ")),
            }
            let cache = resolve_tls_path(&acme.cache, path);
            println!("    cache:            {}", cache.display());
            // The cached certificate is *hashed* by the crate, so the file name is not derivable
            // here — what can be said is whether the directory has anything in it, which is the
            // question an operator actually has: has this deployment ever issued?
            let cached = std::fs::read_dir(&cache)
                .map(|entries| {
                    entries.filter_map(Result::ok).any(|entry| {
                        entry
                            .file_name()
                            .to_string_lossy()
                            .starts_with("cached_cert_")
                    })
                })
                .unwrap_or(false);
            println!(
                "      issued before:  {}",
                match cached {
                    true => "yes (a cached certificate exists, so a restart will not re-issue)",
                    false => "no (the first start will place an order)",
                }
            );

            // The days left on each certificate this deployment serves. Reported whenever ACME is
            // configured, because the *question* — is anything about to expire — is the same
            // whether or not issuance is working, and expiry is this feature's only symptom.
            if let Some(server_tls) = resolved_server_tls(&config, path) {
                if let Some(lifetime) = certificate_lifetime(&server_tls.certificate) {
                    println!("      {}", lifetime.report("serving certificate"));
                }
            }
            for (name, site_tls) in resolved_site_tls(&config, path) {
                if let Some(lifetime) = certificate_lifetime(&site_tls.certificate) {
                    println!(
                        "      {}",
                        lifetime.report(&format!("site {name} certificate"))
                    );
                }
            }
        }
        None => println!("  acme:             (disabled)"),
    }

    // Each site's own table, reported with everything it *inherits* made explicit. A site that omits
    // `authority`, `contact` or `cache` gets the deployment's, and printing only what the file says
    // would leave the reader unable to tell "inherited staging" from "defaulted staging" — the same
    // reason the site table prints only keys a site states, one layer down.
    for (name, site) in &config.sites {
        let Some(site_acme) = &site.acme else {
            continue;
        };
        let (names, dropped) = site_acme.resolve_domains(&site.hosts);
        let authority = site_acme.resolved_authority(&config.server.acme);
        println!("  acme ({name}):");
        println!("    enabled:          {}", site_acme.issues());
        if !site_acme.issues() {
            println!(
                "      DISABLED:       this site orders nothing and is served by the \
                 deployment's certificate"
            );
        }
        println!("    authority:        {}", authority.as_str());
        if authority == volucer_core::config::Authority::Production {
            println!("      PRODUCTION:     real certificates, against per-week rate limits");
        }
        println!(
            "    domains:          {}{}",
            names.join(", "),
            match site_acme.domains.is_empty() {
                true => " (derived from this site's hosts)",
                false => "",
            }
        );
        if site_acme.domains.is_empty() && !dropped.is_empty() {
            println!(
                "      wildcards left out: {} — a certificate name cannot be one, so this site \
                 answers to {} that the certificate will not cover",
                dropped.join(", "),
                match dropped.len() {
                    1 => "a name",
                    _ => "names",
                }
            );
        }
        let contact = site_acme.resolved_contact(&config.server.acme);
        match contact.is_empty() {
            true => println!(
                "    contact:          (none: the CA has no way to warn you that renewals are failing)"
            ),
            false => println!("    contact:          {}", contact.join(", ")),
        }
        let cache = resolve_tls_path(&site_acme.resolved_cache(&config.server.acme), path);
        println!("    cache:            {}", cache.display());
    }

    println!("  document root: {}", document_root.display());
    println!(
        "    exists:           {}",
        match (document_root.exists(), document_root.is_dir()) {
            (false, _) => "no",
            (true, true) => "yes",
            (true, false) => "no (it is a file, not a directory)",
        }
    );
    match &front_controller {
        Some(front) => {
            println!("    front controller: {front}");
            let script = document_root.join(front);
            println!(
                "      present:        {}",
                if script.is_file() { "yes" } else { "NO" }
            );
        }
        None => println!("    front controller: (none: only real .php files)"),
    }
    // Printed only when it was configured, and as the normalised list: an operator who wrote
    // `".py"` should see that the server compares `py`, and one who wrote nothing should not have
    // to read a line describing the default. The list decides three things at once — routing, the
    // static layer's source guard, and `skip_stat` — so it is worth being able to see.
    let server_extensions = config.server.script_extensions();
    if !server_extensions.is_default() {
        println!(
            "    script extensions: {}",
            server_extensions.as_slice().join(", ")
        );
    }

    println!("  backends:");
    if config.backends.is_empty() {
        println!("    (none)");
    }
    for (name, backend) in &config.backends {
        let default = if config.default_backend.as_deref() == Some(name.as_str()) {
            " (default)"
        } else {
            ""
        };
        println!("    {name}: {}{default}", backend.kind().replace('_', "-"));
        println!(
            "      per-request reset: {}",
            backend.request_scope().describe()
        );
        // The relay is the one thing an operator has to look at for an `attach` backend, and it
        // is the one thing volucer does not own: nothing in this process started it, so "is it
        // there" is a question for `preflight` rather than for `check`. Printing the address is
        // what makes the two processes' relationship visible in one line.
        if let volucer_core::config::BackendConfig::Attach(attach) = backend {
            println!("      relay:             {}", attach.relay.trim());
            println!("      connections:       {}", attach.workers);
        }
        // A `kind = "upstream"` backend was reported here as "not served yet" for exactly one
        // commit — the one that built the client before any listener could reach it — and that line
        // went away with the same commit that added the request path below it. Both existed so that
        // `check` could not answer "configuration OK" for a kind `serve` refused, and both were
        // removed together for the same reason.
    }

    // The KV storages, with a file store's *resolved* directory. The resolved value is the one that
    // matters: `path = "kv"` is relative to the configuration file, and an operator reading `check`
    // needs to see where the files will actually be before a directory appears in the wrong place.
    if !config.kv.is_empty() {
        println!("  kv:");
        for (name, storage) in &config.kv {
            match storage {
                volucer_core::config::KvStorageConfig::File(file) => {
                    let resolved = volucer_core::config::resolve_kv_path(
                        &file.path,
                        path.parent().unwrap_or_else(|| Path::new("")),
                    );
                    println!("    {name}: file, {}", resolved.display());
                    println!(
                        "      bound:            {} MiB, counted from what is already there",
                        file.max_bytes / (1024 * 1024)
                    );
                }
                other => println!("    {name}: {}", other.driver()),
            }
        }
    }

    // A route is printed as the three matchers it can state, with `skip_stat` where it
    // applies — the saving is real and an operator should be able to see where the
    // router is still checking the filesystem and where it has stopped.
    let print_route = |route: &volucer_core::config::RouteConfig,
                       interprets_path: bool,
                       extensions: &volucer_core::ScriptExtensions,
                       indent: &str| {
        let host = route.host.as_deref().unwrap_or("*");
        let prefix = route.path_prefix.as_deref().unwrap_or("*");
        let suffix = route.path_suffix.as_deref().unwrap_or("*");
        // A route with no backend is a files-only location, and saying so is the point: it
        // explains why a missing file there is a 404 rather than a dispatch.
        match &route.backend {
            Some(backend) => println!("{indent}{host}{prefix}{suffix} -> {backend}"),
            None => println!("{indent}{host}{prefix}{suffix} -> (files only: no backend)"),
        }
        // Only what the route *states*, which is the same rule the site table uses: printing
        // `all` under every route would make the table read as though every route had been
        // configured with it.
        if route.policy != volucer_core::config::RoutePolicy::All {
            println!("{indent}  policy: {}", route.policy.as_str());
        }
        if volucer_core::config::skip_stat(route, interprets_path, extensions) {
            println!("{indent}  stat: skipped (the pattern proves the extension)");
        }
    };
    // Whether the backend reads the path as a file to execute. A worker does not, and neither does
    // an upstream — so both skip the router's `stat`, which is why this asks the question that way
    // round rather than asking "is it a worker". A name that selects nothing is treated as
    // interpreting the path: a backend that is not there cannot vouch for the safety of skipping.
    let interprets_path = |name: &str| {
        config
            .backends
            .get(name)
            .is_none_or(volucer_core::config::BackendConfig::interprets_path)
    };
    // The list a route's `skip_stat` is judged against. `check` asks the site table for it rather
    // than reading `[server]` directly, so a site that widened the list is reported against the
    // list that site will actually route with.
    let sites =
        volucer_core::Sites::from_config_with(&config, &document_root, front_controller.as_deref());

    println!("  routes:");
    if config.routes.is_empty() {
        println!("    (none: every request goes to the default backend)");
    }
    let default_extensions = sites.default_site().script_extensions.clone();
    for route in &config.routes {
        print_route(
            route,
            route.backend.as_deref().is_some_and(interprets_path),
            &default_extensions,
            "    ",
        );
    }

    println!("  sites:");
    if config.sites.is_empty() {
        println!("    (none: [server] describes the only site)");
    }
    // Only what a site *states* is printed here. A key it does not state is the server's,
    // already printed above, and repeating it per site would make the table read as
    // though every site had been configured with the same values.
    for (name, site) in &config.sites {
        println!("    {name}: {}", site.hosts.join(", "));
        println!("      document root:    {}", site.document_root.display());
        if let Some(controller) = &site.front_controller {
            match controller.as_path() {
                Some(path) => println!("      front controller: {path}"),
                None => println!("      front controller: (none: only real .php files are served)"),
            }
        }
        if let Some(serve_only) = site.serve_only {
            println!("      serve only:       {serve_only}");
        }
        if let Some(extensions) = sites.named().get(name) {
            if !extensions.script_extensions.is_default() {
                println!(
                    "      script extensions: {}",
                    extensions.script_extensions.as_slice().join(", ")
                );
            }
        }
        if let Some(routes) = &site.routes {
            println!("      routes:");
            // The site's own list, resolved from the same table `serve` builds, so `check`
            // reports the `stat` decision the request path will actually make.
            let extensions = sites
                .named()
                .get(name)
                .map(|resolved| &resolved.script_extensions)
                .unwrap_or(&default_extensions);
            for route in routes {
                print_route(
                    route,
                    route.backend.as_deref().is_some_and(interprets_path),
                    extensions,
                    "        ",
                );
            }
        }
        if let Some(backend) = &site.default_backend {
            println!("      default backend:  {backend}");
        }
    }

    println!("  static files:");
    println!("    {}", config.server.serve_only);
    if !config.sites.is_empty() {
        println!(
            "    this is the default site's setting; a site that states serve_only overrides it"
        );
    }
    // The layer the file configured, and then the one the document roots can use. Both are
    // printed when they differ, because a setting that is not in force is exactly what an
    // operator runs `check` to find out.
    let suppressed = sites.suppressed_static_layer();
    if config.server.statics().enabled {
        println!("    enabled for the HTTP listener");
        println!(
            "      precompressed:   {}",
            if config.server.statics().precompressed {
                "yes (.br, then .gz)"
            } else {
                "no"
            }
        );
        println!(
            "      directory index: {}",
            if config.server.statics().directory_index {
                "index.html, with a redirect from the bare path"
            } else {
                "no"
            }
        );
        println!(
            "      dotfiles:        {:?}",
            config.server.statics().dotfiles
        );
        println!(
            "      cache-control:   {}",
            config.server.statics().cache_control
        );
    } else {
        println!("    (disabled: every request goes to a backend)");
    }
    // After the settings it overrides, so the block reads top to bottom: what the file says,
    // and then where it does not apply.
    for site in &suppressed {
        println!(
            "    {}: off, though the file enables it — {} holds no asset a file could be served \
             from",
            site.name,
            site.document_root.display()
        );
    }

    let mut dirs = extra_php_dirs.to_vec();
    dirs.extend(php::default_search_dirs());
    let found = php::discover(&dirs)?;

    // The access log, reported as the *path this deployment will actually use* rather than the one
    // written in the file — the same rule the certificate paths follow, and the reason
    // `resolve_access_log_path` is shared with `serve` instead of reimplemented here. The
    // silent-reader rule from the upstream step applies: a key a deployment sets that no command
    // reports is a key an operator cannot confirm.
    match &config.server.access_log {
        Some(log) => {
            let resolved =
                resolve_access_log_path(&log.path, path.parent().unwrap_or_else(|| Path::new("")));
            println!("  access log:");
            println!("    path:   {}", resolved.display());
            println!(
                "    format: {}",
                match log.format {
                    AccessLogFormat::Combined => "combined (nginx's shape, plus two timings)",
                    AccessLogFormat::Json => "json (one object per line)",
                }
            );
            println!(
                "    request id: {}",
                match log.request_id_header.trim() {
                    "" => "always generated (adoption off)".to_string(),
                    name => format!("adopt and echo {name}"),
                }
            );
            // Reported rather than warned about, because serving without an access log is a valid
            // outcome and the runtime already says so once — this is the *pre-start* answer, which
            // is what `check` is for.
            if let Some(parent) = resolved.parent() {
                if !parent.as_os_str().is_empty() && !parent.is_dir() {
                    println!(
                        "    note:   {} does not exist; requests will be served without an access \
                         log until it does",
                        parent.display()
                    );
                }
            }
            // Printed because it is the one thing a deployment must do *after* configuring a log,
            // and because the alternative is finding out during an incident that `logrotate` detached
            // the log silently. The path and format above are restart-only; the reopen is not.
            println!(
                "    rotate: send SIGHUP (or `systemctl reload`) after logrotate renames the file — \
                 volucer closes and reopens it, so the new file is the one written"
            );
        }
        None => println!("  access log: none (server.access_log is unset)"),
    }
    // Trace propagation has no config key of its own and needs none: an inbound `traceparent` is
    // always parsed and continued with a fresh span, and a missing or malformed one starts a new
    // unsampled trace rather than failing. It is also always forwarded — `to_cgi` maps it to
    // `HTTP_TRACEPARENT` and no strip list contains it — so PHP sees it without any key to set.
    // Printed so an operator can confirm that without reading the source.
    println!("  trace context: W3C traceparent is read, continued, and forwarded to backends");
    // The exporter, and the endpoint as written, which is the key a misconfiguration hides in — the
    // silent-reader rule again. Note it is reported even when the endpoint cannot be dialled:
    // `check` reports what was configured, and `serve` reports whether the exporter started.
    match &config.telemetry {
        Some(telemetry) => {
            println!("  telemetry:");
            println!("    endpoint:     {}", telemetry.endpoint);
            println!("    service.name: {}", telemetry.service_name);
            println!(
                "    batching:     {} span(s), or every {} ms",
                telemetry.batch_size, telemetry.interval_ms
            );
            println!("    timeout:      {} ms per export", telemetry.timeout_ms);
            if !telemetry.headers.is_empty() {
                // Names only. A value here is a credential — the same rule the Redis URLs follow —
                // and printing one would put a secret in a diagnostic an operator pastes into a
                // ticket.
                let names: Vec<&str> = telemetry.headers.keys().map(String::as_str).collect();
                println!("    headers:      {} (values redacted)", names.join(", "));
            }
        }
        None => println!("  telemetry: none (telemetry is unset)"),
    }

    // The status listener and the one route on it that mutates. Printed in both cases rather than
    // only when configured: the *absence* of a mutation surface is the fact an operator auditing
    // this deployment wants, and a report that printed nothing when `allow_worker_reset` was off
    // would leave them unable to tell "not configured" from "not reported".
    match (&config.status, &config.metrics) {
        (Some(status), _) => {
            println!("  status: {}", status.address);
            println!(
                "    worker routes:    {}",
                if status.allow_worker_reset {
                    "GET /workers and POST /workers/{index}/reset (no authentication)"
                } else {
                    "disabled (allow_worker_reset is false; the routes do not exist)"
                }
            );
        }
        (None, Some(metrics)) => {
            println!("  metrics: {}", metrics.address);
            println!("    worker routes:    disabled (they live on [status], which is unset)");
        }
        (None, None) => {
            println!("  status:           (disabled: no /health, no /ready, and no worker routes)")
        }
    }

    println!("  PHP binaries:");
    if found.is_empty() {
        println!("    (none found)");
    }
    for entry in &found {
        let binary = &entry.binary;
        let below_floor = binary.version.as_str() < volucer_server::MINIMUM_PHP_VERSION;
        println!(
            "    {:<4} PHP {} {:<8} {}{}",
            entry.role,
            binary.version,
            binary.flavour(),
            binary.path.display(),
            if below_floor {
                "  (below the worker floor; worker mode needs 8.2+)"
            } else {
                ""
            }
        );
    }

    // Warnings, not errors: every one of these is a normal intermediate state on
    // the way to a working deployment, and refusing to report anything else
    // because the root is not there yet would be the wrong trade.
    let warnings = volucer_core::preflight::inspect_with(&config, &document_root, Some(path));
    if !warnings.is_empty() {
        println!("  warnings:");
        for warning in &warnings {
            println!("    - {}", warning.message);
            if let Some(hint) = &warning.hint {
                println!("      {hint}");
            }
        }
    }

    println!("configuration OK");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_contention_line_carries_every_number_it_is_given() {
        let reported = Contention {
            backend: "app".to_string(),
            workers: 12,
            served: 40_000,
            mean_us: 812,
            p50_us: 1,
            p99_us: 16_384,
            busiest: 7,
            busiest_us: 41_233,
            dropped_metrics: 0,
        };
        let line = contention_line(&reported);
        // Whitespace-collapsed comparison: the format is what matters, not the width of
        // the separators, and a test that pins the spacing fails on a harmless edit.
        let flat: String = line.split_whitespace().collect::<Vec<_>>().join(" ");
        assert_eq!(
            flat,
            "slots: 12 worker(s), 40000 request(s), wait mean 812 us, wait p50 1 us, \
             wait p99 16384 us, busiest slot 7 at 41233 us"
        );
    }

    #[test]
    fn routing_uses_a_front_controller_by_default() {
        let site = volucer_core::ResolvedSite {
            name: "<default>".to_string(),
            document_root: PathBuf::from("/srv/app"),
            front_controller: Some("index.php".to_string()),
            script_extensions: volucer_core::ScriptExtensions::default(),
            serve_only: volucer_core::config::ServeOnly::All,
            static_files: volucer_core::config::StaticConfig::default(),
            static_files_suppressed: false,
            compression: volucer_core::config::CompressionSetting::default(),
            routes: Vec::new(),
            default_backend: None,
        };
        let routing = routing_of(&site);
        assert_eq!(routing.front_controller.as_deref(), Some("index.php"));
        assert_eq!(routing.document_root, PathBuf::from("/srv/app"));
        assert_eq!(
            routing.script_extensions,
            volucer_core::ScriptExtensions::default()
        );
    }

    #[test]
    fn resolve_document_root_prefers_the_flags_that_are_set() {
        let text = r#"
[server]
http = "127.0.0.1:8080"
document_root = "/from/file"
"#;
        let config = Config::from_toml(text).expect("config parses");

        assert_eq!(
            resolve_document_root(Some(Path::new("/from/flag")), &config),
            PathBuf::from("/from/flag")
        );
        assert_eq!(
            resolve_document_root(None, &config),
            PathBuf::from("/from/file")
        );

        let bare = Config::from_toml("[server]\nhttp = \"127.0.0.1:8080\"\n").expect("parses");
        assert_eq!(resolve_document_root(None, &bare), PathBuf::from("."));
    }

    #[test]
    fn an_empty_front_controller_means_files_only() {
        let site = volucer_core::ResolvedSite {
            name: "<default>".to_string(),
            document_root: PathBuf::from("."),
            front_controller: None,
            script_extensions: volucer_core::ScriptExtensions::default(),
            serve_only: volucer_core::config::ServeOnly::All,
            static_files: volucer_core::config::StaticConfig::default(),
            static_files_suppressed: false,
            compression: volucer_core::config::CompressionSetting::default(),
            routes: Vec::new(),
            default_backend: None,
        };
        assert_eq!(routing_of(&site).front_controller, None);
    }

    /// Days left are reported in the units the decision is made in, and each band says something
    /// different.
    ///
    /// The thresholds are not round numbers chosen for looks: a renewal is attempted a third of the
    /// way through validity, so **under a third** means a renewal that should have happened has
    /// not, and **under a week** means several have failed in a row. Both are what an operator
    /// needs told, and the difference between them is the difference between "look at this" and
    /// "this has been broken for a while".
    ///
    /// Asserted per band rather than by containing a substring, because a single over-broad message
    /// would satisfy "contains days" for all of them.
    #[test]
    fn a_certificates_days_left_are_reported_with_the_warning_they_deserve() {
        let report = |days_left: i64| {
            CertificateLifetime {
                days_left,
                expired: days_left < 0,
            }
            .report("cert")
        };

        // Healthy: a bare count, no alarm.
        let healthy = report(75);
        assert_eq!(healthy, "cert: 75 day(s) left", "{healthy}");
        assert!(!healthy.contains("failed"), "{healthy}");

        // Under a third of 90 days: a renewal should have happened by now.
        let late = report(20);
        assert!(late.contains("20 day(s) left"), "{late}");
        assert!(late.contains("should have happened"), "{late}");

        // Under a week: several in a row have failed, and the CA is already warning.
        let urgent = report(3);
        assert!(urgent.contains("3 day(s) left"), "{urgent}");
        assert!(urgent.contains("several renewals"), "{urgent}");
        assert!(urgent.contains("contact"), "{urgent}");

        // Expired is its own message, because the consequence is different in kind: clients are
        // already being shown warnings.
        let expired = report(-1);
        assert!(expired.contains("EXPIRED"), "{expired}");
        assert!(expired.contains("client"), "{expired}");
    }

    /// A certificate file that is not a certificate is reported as *nothing*, not as a failure.
    ///
    /// `check` runs against a deployment that may not have its certificate yet — that is the state
    /// an ACME-managed first start is in — so a missing or unreadable file must not make `check`
    /// fail. The preflight warnings already report an unreadable certificate by name; this is the
    /// extra information when there *is* one.
    #[test]
    fn a_certificate_that_cannot_be_read_reports_no_lifetime_rather_than_failing() {
        let missing = std::env::temp_dir().join("volucer-no-such-cert-1.crt");
        assert!(
            certificate_lifetime(&missing).is_none(),
            "a missing certificate must report nothing"
        );

        let not_a_cert =
            std::env::temp_dir().join(format!("volucer-not-a-cert-{}.crt", std::process::id()));
        std::fs::write(&not_a_cert, b"this is not a certificate").expect("write the fixture");
        assert!(
            certificate_lifetime(&not_a_cert).is_none(),
            "a file that is not a certificate must report nothing"
        );
        let _ = std::fs::remove_file(&not_a_cert);
    }

    /// A real certificate's expiry is actually read, so the report is not always absent.
    ///
    /// The counterpart to the test above, and the one that would catch a parser wired up wrongly:
    /// "reports nothing for a bad file" passes just as well against code that reports nothing for
    /// every file. A certificate generated here has a real `not_after`, so a lifetime must come
    /// back — and its sign is checked rather than its value, because the fixture's validity window
    /// is not this test's business.
    #[test]
    fn a_real_certificates_expiry_is_read() {
        let generated = rcgen::generate_simple_self_signed(vec!["example.test".to_string()])
            .expect("generate a certificate");
        let path =
            std::env::temp_dir().join(format!("volucer-real-cert-{}.crt", std::process::id()));
        std::fs::write(&path, generated.cert.pem()).expect("write the fixture");

        let lifetime =
            certificate_lifetime(&path).expect("a real certificate must have a readable lifetime");
        assert!(
            !lifetime.expired,
            "a freshly generated certificate must not read as expired"
        );
        assert!(
            lifetime.days_left > 0,
            "a freshly generated certificate must have days left: {}",
            lifetime.days_left
        );

        let _ = std::fs::remove_file(&path);
    }

    /// The restart-only list names the three classes that genuinely cannot be replaced, and
    /// **not** the two that an earlier draft wrongly listed.
    ///
    /// This is the text an operator reads on every reload, so it is the thing that tells them a
    /// setting they edited is not in effect. It is asserted rather than trusted for two reasons.
    /// First, the list is a claim about the implementation, and the implementation moved: a site's
    /// `routes` and `[server.statics]` on/off were both listed as restart-only here and are both
    /// read per request out of the swapped table, so both *are* reloadable — an operator told
    /// otherwise would restart a server for nothing.
    /// Second, this list is what the README's reloadable/restart-only split is copied from, so a
    /// silent drift between the two is a documentation bug in a file no test reads.
    ///
    /// The assertion is deliberately about class rather than wording: what matters is that a bound
    /// socket, a running process and a built `rustls::ServerConfig` are named, and that nothing
    /// the reload *does* apply appears.
    #[test]
    fn the_restart_only_list_names_what_cannot_be_replaced_and_nothing_else() {
        let lines = restart_only();
        let joined = lines.join("\n");

        for required in [
            "listener addresses",
            "backends",
            "min_version",
            "server.proxy",
            "server.rate_limit",
            "server.access_log",
        ] {
            assert!(
                joined.contains(required),
                "the restart-only list must name {required:?}, or an operator who edited it gets \
                 silence: {joined}"
            );
        }

        // The two an earlier draft got wrong, in the code's favour. Both are read per request out
        // of the swapped table, so telling an operator they are restart-only is telling them to
        // restart for nothing.
        for wrong in ["routes", "statics", "static_files"] {
            assert!(
                !joined.contains(wrong),
                "{wrong:?} is read per request out of the swapped table and is therefore \
                 reloadable; listing it as restart-only is the half-reload claim in reverse: \
                 {joined}"
            );
        }
    }

    /// A PEM blob of a chain plus a key splits at the key, not at the second block.
    ///
    /// `rustls-acme` hands the issued certificate over as one blob, and the shape is a *chain*
    /// followed by a key. The tempting implementation — "the first block is the certificate, the
    /// second is the key" — is wrong the moment a chain has an intermediate, and it fails in a way
    /// that reads as a key problem rather than a parsing one. So the split is by PEM label, and this
    /// pins that: a three-block blob must give a two-certificate chain.
    #[test]
    fn a_pem_blob_splits_at_the_key_not_at_the_second_block() {
        let chain_then_key = "-----BEGIN CERTIFICATE-----\nleaf\n-----END CERTIFICATE-----\n\
                              -----BEGIN CERTIFICATE-----\nintermediate\n-----END CERTIFICATE-----\n\
                              -----BEGIN PRIVATE KEY-----\nkey\n-----END PRIVATE KEY-----\n";
        let (cert, key) = split_pem_pair(chain_then_key.as_bytes());

        let cert = String::from_utf8_lossy(cert);
        let key = String::from_utf8_lossy(key);
        assert!(
            cert.contains("leaf") && cert.contains("intermediate"),
            "the certificate half must be the whole chain, intermediates included: {cert}"
        );
        assert!(
            !cert.contains("BEGIN PRIVATE KEY"),
            "the chain must not carry the key: {cert}"
        );
        assert!(
            key.starts_with("-----BEGIN PRIVATE KEY-----"),
            "the key half must begin at the key's own marker: {key}"
        );
        assert!(
            !key.contains("intermediate"),
            "the key half must not carry a certificate: {key}"
        );

        // The single-certificate shape, which is the common one, and the shapes of key marker an
        // `openssl` invocation actually produces.
        for marker in [
            "-----BEGIN PRIVATE KEY-----",
            "-----BEGIN RSA PRIVATE KEY-----",
            "-----BEGIN EC PRIVATE KEY-----",
        ] {
            let blob = format!(
                "-----BEGIN CERTIFICATE-----\nonly\n-----END CERTIFICATE-----\n{marker}\nk\n"
            );
            let (cert, key) = split_pem_pair(blob.as_bytes());
            assert_eq!(
                String::from_utf8_lossy(cert).trim_end(),
                "-----BEGIN CERTIFICATE-----\nonly\n-----END CERTIFICATE-----",
                "one certificate must be the whole chain half ({marker})"
            );
            assert!(
                String::from_utf8_lossy(key).starts_with(marker),
                "the key half must begin at {marker}"
            );
        }

        // No key at all: the whole blob is the chain and the key half is empty, so the loader
        // reports a missing key rather than the chain being silently truncated.
        let (cert, key) = split_pem_pair(b"-----BEGIN CERTIFICATE-----\nonly\n");
        assert_eq!(cert, b"-----BEGIN CERTIFICATE-----\nonly\n");
        assert!(
            key.is_empty(),
            "a blob with no key must give an empty key half, not a truncated chain"
        );
    }
}
