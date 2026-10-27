//! The `volucer` command line interface.

use std::io::{IsTerminal, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
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
    Consumers, Contention, Dispatcher, FastCgiListener, Limits, RpcListener, ServerError,
    WorkerPool,
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
    #[command(group(
        clap::ArgGroup::new("backend")
            .multiple(false)
            .args(["php_cgi", "worker", "fcgi"])
    ))]
    Serve {
        /// Path to `volucer.toml`.
        ///
        /// Defaults to `volucer.toml` in the working directory when that file exists. Without a
        /// file — no `-c`, and no `volucer.toml` beside the working directory — the configuration
        /// comes from the command line alone, so at least one listener must be given with
        /// `--listen`, `--fastcgi` or `-o server.http=…`.
        #[arg(short, long)]
        config: Option<PathBuf>,

        /// HTTP listener, e.g. `127.0.0.1:8080`.
        ///
        /// Shorthand for `-o server.http=<ADDR>`, and applied **before** every `-o` pair, so an
        /// explicit `-o server.http=…` wins over this flag rather than depending on argument order.
        #[arg(long, value_name = "ADDR")]
        listen: Option<String>,

        /// FastCGI listener, e.g. `127.0.0.1:9000` or `unix:/run/volucer.sock`.
        ///
        /// Shorthand for `-o server.fastcgi=<ADDR>`, with the same precedence rule as `--listen`.
        #[arg(long, value_name = "ADDR")]
        fastcgi: Option<String>,

        /// Set one configuration key, in `dot.notation=value` form. Repeatable.
        ///
        /// The value is read as a TOML literal, so `-o backends.app.workers=8` is a number and
        /// `-o server.http=127.0.0.1:8080` is the string it looks like. Quote it to force a
        /// string: `-o 'server.http="0.0.0.0:8080"'`. `${NAME}` and `${NAME:-default}` are
        /// expanded first, as in the file.
        ///
        /// An **array of tables** takes an index: `-o server.routes[0].backend=app` amends the
        /// first route, and `-o server.routes[2].path_prefix=/x` appends when the array holds two
        /// (a larger index is refused rather than padded). A key that itself contains a dot is
        /// quoted: `-o 'server.env."APP.NAME"=x'`.
        ///
        /// **A key that does not exist is an error**, exactly as a typo in `volucer.toml` is —
        /// the strictness is the feature. Applied in the order given, after `--listen` and
        /// `--fastcgi`, so a later pair wins over an earlier one.
        #[arg(short = 'o', long = "override", value_name = "KEY=VALUE")]
        overrides: Vec<String>,

        /// Document root that PHP scripts are resolved against.
        ///
        /// Overrides `server.document_root`; without either, `.`.
        #[arg(long, visible_alias = "root")]
        document_root: Option<PathBuf>,

        /// Front controller, relative to the document root.
        ///
        /// Overrides `server.front_controller`; without either, `index.php`.
        /// Set it empty to serve only actual files, matching nginx's
        /// `location ~ \.php$`.
        #[arg(long)]
        front_controller: Option<String>,

        /// Serve a `php-cgi` pool as the default backend.
        ///
        /// The quick-start flag: `volucer serve --root public --php-cgi` is a complete
        /// deployment, with no `volucer.toml` to write. It sets `backends.cgi.kind=spawn_cgi`,
        /// `backends.cgi.binary`, and points `server.default_backend` at it — three statements
        /// that this flag *makes* rather than infers, which is why nothing is detected without it.
        ///
        /// A bare `--php-cgi` uses `php-cgi`, resolved in the PHP search directories (`PATH`
        /// first), so the name works wherever `volucer check` finds a binary. Give a value to
        /// name one explicitly: `--php-cgi=C:/php-8.3.31/php-cgi.exe`.
        #[arg(
            long = "php-cgi",
            value_name = "PATH",
            num_args = 0..=1,
            default_missing_value = "php-cgi"
        )]
        php_cgi: Option<String>,

        /// Serve a resident worker as the default backend, e.g. `--worker "php worker.php"`.
        ///
        /// Sets `backends.app.kind=worker`, its `command`, and `server.default_backend`.
        /// `--workers` sizes the pool. The value is one command line, split without a shell —
        /// the same tokenizer `[backends.app].command` uses — so quote it as a whole.
        #[arg(long, value_name = "COMMAND")]
        worker: Option<String>,

        /// Serve any FastCGI server as the default backend, e.g.
        /// `--fcgi "python fcgi_app.py --bind {address}"`.
        ///
        /// The non-PHP sibling of `--php-cgi`: it sets `backends.app.kind=spawn_cgi` with a
        /// `command`, and `server.default_backend`. **`{address}` is required** — the pool owns
        /// the address of every slot, so a program that binds one of its own is never reached.
        ///
        /// It also sets `front_controller` to `""` when you have not, because a WSGI or Rack
        /// program is not `index.php`: without that, a path naming no file resolves to
        /// `index.php` and answers 404. Set `--front-controller` yourself to choose one.
        #[arg(long, value_name = "COMMAND")]
        fcgi: Option<String>,

        /// How many slots the `--php-cgi`, `--worker` or `--fcgi` backend starts.
        ///
        /// Shorthand for that backend's `workers`, and an error without one of those flags —
        /// there would be no backend of that name to size. Omitted, a slot is started per
        /// logical CPU, except for `--fcgi`, whose slots are `1` by default because the program
        /// decides its own concurrency through `concurrency`.
        #[arg(long, value_name = "N", requires = "backend")]
        workers: Option<u32>,

        /// How many requests one `--fcgi` slot may serve at once.
        ///
        /// Shorthand for `backends.app.concurrency`, and it means something different from
        /// `--workers`: `--workers` is how many slots, `--concurrency` is how many requests one
        /// slot answers at once. Since `--fcgi` runs its command once per slot with a distinct
        /// `{address}`, this is the app server's own worker count *for that one address* —
        /// `gunicorn --workers 4` in the command means `--concurrency 4`, and the pool then runs
        /// that command `--workers` times. Workers times threads if the server threads.
        ///
        /// Only meaningful with `--fcgi`, because a `worker` decides its own concurrency one
        /// request at a time and `php-cgi` expresses it as `children`; giving it to either is an
        /// error naming that.
        #[arg(long, value_name = "N", alias = "capacity")]
        concurrency: Option<u32>,

        /// Maximum concurrent client connections.
        #[arg(long, default_value_t = 512)]
        max_connections: usize,
    },

    /// Validate the configuration and report what was found on this host.
    #[command(group(
        clap::ArgGroup::new("backend")
            .multiple(false)
            .args(["php_cgi", "worker", "fcgi"])
    ))]
    Check {
        /// Path to `volucer.toml`. Optional, with the same rule as `serve`.
        #[arg(short, long)]
        config: Option<PathBuf>,

        /// HTTP listener to check, overriding `server.http`. See `serve --listen`.
        #[arg(long, value_name = "ADDR")]
        listen: Option<String>,

        /// FastCGI listener to check, overriding `server.fastcgi`.
        #[arg(long, value_name = "ADDR")]
        fastcgi: Option<String>,

        /// Set one configuration key, in `dot.notation=value` form. Repeatable.
        ///
        /// The same keys, typing and expansion as `serve --override`, and the same refusal of an
        /// unknown one — which is what makes `check` a rehearsal of `serve` rather than a second
        /// opinion about the file.
        #[arg(short = 'o', long = "override", value_name = "KEY=VALUE")]
        overrides: Vec<String>,

        /// Document root to check, overriding `server.document_root`.
        #[arg(long, visible_alias = "root")]
        document_root: Option<PathBuf>,

        /// Check a `php-cgi` pool as the default backend.
        ///
        /// The same shorthand `serve` takes, so a one-line deployment can be rehearsed with
        /// `check` before it is started. A bare `--php-cgi` names `php-cgi`, resolved in the PHP
        /// search directories.
        #[arg(
            long = "php-cgi",
            value_name = "PATH",
            num_args = 0..=1,
            default_missing_value = "php-cgi"
        )]
        php_cgi: Option<String>,

        /// Check a resident worker as the default backend. See `serve --worker`.
        #[arg(long, value_name = "COMMAND")]
        worker: Option<String>,

        /// Check a generic FastCGI server as the default backend. See `serve --fcgi`.
        #[arg(long, value_name = "COMMAND")]
        fcgi: Option<String>,

        /// Front controller to check, overriding `server.front_controller`. See
        /// `serve --front-controller`.
        #[arg(long)]
        front_controller: Option<String>,

        /// How many slots the backend would start. See `serve --workers`.
        #[arg(long, value_name = "N", requires = "backend")]
        workers: Option<u32>,

        /// How many requests one `--fcgi` slot may serve at once. See `serve --concurrency`.
        #[arg(long, value_name = "N", alias = "capacity")]
        concurrency: Option<u32>,

        /// Extra directories to search for PHP binaries.
        #[arg(long = "php-dir")]
        php_dirs: Vec<PathBuf>,
    },

    /// Ask a running server whether it is healthy, or ready for traffic.
    ///
    /// A subcommand rather than a `curl` in a `HEALTHCHECK` line because the `scratch` image has
    /// no shell and no `curl` — a container health check that cannot execute is a check that
    /// silently never runs, and Docker reads "never ran" as "start-period never elapsed" rather
    /// than as an error. This also makes the probe work in a `distroless` deployment, and makes
    /// the check assert the two things this server distinguishes: `/health` asks whether the
    /// process is alive, `/ready` whether it should be sent traffic.
    Health {
        /// The URL to probe, e.g. `http://127.0.0.1:2114/health`.
        ///
        /// A full URL rather than an address, so the probe does not have to know which listener
        /// serves the route. Wins over `--config` and over `VOLUCER_HEALTH_URL`.
        #[arg(long, conflicts_with = "config")]
        url: Option<String>,

        /// Probe the listener this configuration file names.
        ///
        /// The address comes from `[status].listen`, which is the only one there is
        /// present, because the two share one listener. An unspecified host (`0.0.0.0`, `::`) is
        /// probed on loopback, which is what a container's own address means from inside it.
        ///
        /// Defaults to `VOLUCER_HEALTH_CONFIG`, and then to `volucer.toml` beside the working
        /// directory — so a `HEALTHCHECK` written for the documented `/etc/volucer/volucer.toml`
        /// keeps working when a deployment overrides `CMD` to point somewhere else, without the
        /// image having to guess. A file that cannot be read is a *failed* probe rather than a
        /// skipped one: a container with no configuration cannot serve, and reporting it healthy
        /// because the check could not read its file is the failure a health check exists to
        /// prevent.
        #[arg(long, conflicts_with = "url")]
        config: Option<PathBuf>,

        /// Probe `/ready` instead of `/health`.
        ///
        /// The two answer different questions, and a container is normally kept alive by `/health`
        /// and kept out of a load balancer by `/ready`. Use this one for the second.
        #[arg(long)]
        ready: bool,

        /// How long to wait for an answer, in milliseconds.
        ///
        /// Defaults lower than Docker's own 30 s health-check timeout: a probe that waits longer
        /// than the orchestrator does is a probe whose result is never read.
        #[arg(long, default_value_t = 2000)]
        timeout_ms: u64,
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
            listen,
            fastcgi,
            overrides,
            document_root,
            front_controller,
            php_cgi,
            worker,
            fcgi,
            workers,
            concurrency,
            max_connections,
        } => {
            let backend = Backends::from_flags(php_cgi, worker, fcgi);
            Sizing::from_flags(&backend, workers, concurrency).and_then(|sizing| {
                // An explicit `--front-controller` wins over what `--fcgi` implies; both end up on
                // the one flag path, which is the only spelling the empty value has.
                let front_controller = front_controller
                    .or_else(|| implied_front_controller(&backend).map(str::to_string));
                serve(
                    &Source::resolve_with_backends(
                        config, listen, fastcgi, overrides, backend, sizing,
                    ),
                    document_root.as_deref(),
                    front_controller.as_deref(),
                    max_connections,
                )
            })
        }
        Command::Check {
            config,
            listen,
            fastcgi,
            overrides,
            document_root,
            front_controller,
            php_cgi,
            worker,
            fcgi,
            workers,
            concurrency,
            php_dirs,
        } => {
            let backend = Backends::from_flags(php_cgi, worker, fcgi);
            Sizing::from_flags(&backend, workers, concurrency).and_then(|sizing| {
                let front_controller = front_controller
                    .or_else(|| implied_front_controller(&backend).map(str::to_string));
                check(
                    &Source::resolve_with_backends(
                        config, listen, fastcgi, overrides, backend, sizing,
                    ),
                    document_root.as_deref(),
                    front_controller.as_deref(),
                    &php_dirs,
                )
            })
        }
        Command::Health {
            url,
            config,
            ready,
            timeout_ms,
        } => health(url.as_deref(), config.as_deref(), ready, timeout_ms).map_err(|err| {
            ServerError::Probe {
                reason: err.to_string(),
            }
        }),
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
    config_dir: &Path,
) -> (volucer_observe::Observability, Telemetry) {
    let mut observe = match &config.server.access_log {
        Some(log) => {
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
    // Set before the exporter is attached, because they are independent and the *earlier* one is
    // the one a deployment without `[telemetry]` still needs: `propagate` writes the header a
    // backend's spans hang from, and that is a request-path fact whether or not this process
    // exports anything.
    observe = observe.with_propagation(propagation_of(config.server.trace_context));
    // And the sampler, for the same reason and with the same "earlier one matters" argument: under
    // `propagate` the `sampled` flag on the header a backend receives *is* this decision, so a
    // deployment that exports nothing here but asks for `always` still has to write `-01`.
    observe = observe.with_sampling(
        config
            .telemetry
            .as_ref()
            .map_or_else(volucer_observe::Sampler::default, sampler_of),
    );

    let Some(telemetry) = &config.telemetry else {
        return (observe, Telemetry { exporter: None });
    };

    match volucer_otlp::Exporter::start(telemetry) {
        Ok(exporter) => {
            let exporter = std::sync::Arc::new(exporter);
            // Attached *before* the exporter so the sampler is in place even if the exporter
            // fails, which is the same argument as above: the flag on the outbound header is a
            // request-path fact and does not depend on whether an export can be delivered.
            tracing::info!(
                endpoint = exporter.endpoint(),
                service = %telemetry.service_name,
                batch_size = telemetry.batch_size,
                interval_ms = telemetry.interval_ms,
                sample = telemetry.sample.as_str(),
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

/// The configuration enum as the behaviour enum.
///
/// **The one conversion**, called from the one place the process builds its `Observability`. A
/// second conversion would be a second answer to "what does `propagate` mean", which is exactly the
/// drift the mirror type exists to make impossible: `volucer-core` is a leaf and cannot depend on
/// `volucer-observe`, so the two enums are separate and this is the seam between them.
fn propagation_of(
    mode: volucer_core::config::PropagationMode,
) -> volucer_observe::TracePropagation {
    use volucer_core::config::PropagationMode as From;
    use volucer_observe::TracePropagation as To;
    match mode {
        From::Ignore => To::Ignore,
        From::Extract => To::Extract,
        From::Inject => To::Inject,
        From::Propagate => To::Propagate,
    }
}

/// The configuration's sampling policy as the behaviour type.
///
/// The second seam between `volucer-core` and `volucer-observe`, and the second reason the config
/// model carries a mirror enum. The ratio is taken from the table rather than the enum because it
/// is a value the configuration validates on its own — `sample_ratio` beside `sample = "ratio"` — so
/// the conversion can never see a `Ratio` without one. The `unwrap_or_default` is therefore not a
/// fallback for a missing key: validation has already refused that shape, and a reload cannot reach
/// here with one. It is `parent_based` if it ever did, which is the safe direction — exporting less
/// rather than more.
fn sampler_of(telemetry: &volucer_core::config::TelemetryConfig) -> volucer_observe::Sampler {
    use volucer_core::config::SampleMode;
    match telemetry.sample {
        SampleMode::ParentBased => volucer_observe::Sampler::ParentBased,
        SampleMode::Always => volucer_observe::Sampler::Always,
        SampleMode::Ratio => telemetry
            .sample_ratio
            .and_then(volucer_observe::Sampler::ratio)
            .unwrap_or_default(),
    }
}

/// Why a `health` probe failed, kept separate from [`volucer_core::Error`] because none of its
/// causes are configuration: the probe is a client, and the file it might otherwise complain about
/// is not one it reads.
#[derive(Debug, thiserror::Error)]
enum HealthError {
    #[error(
        "the configuration has no [status] table, so this server serves no \
         /health or /ready to probe; pass --url instead"
    )]
    NoProbeListener,

    #[error("{url} is not an http:// URL; `health` speaks plain HTTP, because the [status] listener has no TLS of its own")]
    NotHttp { url: String },

    #[error("{url} does not say where to connect: expected http://host:port/path")]
    NoAuthority { url: String },

    #[error("could not connect to {address}: {source}")]
    Connect {
        address: String,
        #[source]
        source: std::io::Error,
    },

    #[error("could not send the probe to {address}: {source}")]
    Write {
        address: String,
        #[source]
        source: std::io::Error,
    },

    #[error("could not read the answer from {address}: {source}")]
    Read {
        address: String,
        #[source]
        source: std::io::Error,
    },

    #[error("{address} did not answer within {timeout_ms} ms")]
    Timeout { address: String, timeout_ms: u64 },

    #[error("{address} answered {status}: the probe failed. The body was: {body}")]
    Unhealthy {
        address: String,
        status: u16,
        body: String,
    },

    #[error("{address} answered something that is not HTTP/1.x: {head}")]
    NotHttpResponse { address: String, head: String },

    #[error(transparent)]
    Config(Box<volucer_core::Error>),
}

/// `health` asks a running server whether it is healthy, so a container image with no shell and no
/// `curl` can still be probed.
///
/// **The exit status is the whole interface**, because that is all a `HEALTHCHECK` reads: `0` for
/// healthy, `1` for anything else. The reason goes to stderr, which Docker keeps in the container's
/// health log — so `docker inspect` answers "why is this unhealthy" with the server's own words
/// (`not ready\nworker pool: 3 of 8 slots booting`) instead of a bare timeout.
///
/// One HTTP/1.1 request over `std::net::TcpStream` rather than a client crate: the request is a
/// literal and the response is reduced to a status code, so a TLS stack, a connection pool and a
/// URL parser would all be dependency weight for a program whose entire job is one `GET`. The
/// `Connection: close` framing means the read ends on EOF, which is why nothing here parses
/// `Content-Length` — a probe that fails because the framing changed would be a false alarm.
fn health(
    url: Option<&str>,
    config: Option<&Path>,
    ready: bool,
    timeout_ms: u64,
) -> Result<(), HealthError> {
    let path = if ready { "/ready" } else { "/health" };
    // `VOLUCER_HEALTH_URL` is what a `HEALTHCHECK` in an image can set without rewriting its
    // command: Docker's `--health-cmd` and Compose's `healthcheck.test` are fixed at build time,
    // while the environment is the one part a deployment overrides. The flag wins, so an operator
    // debugging by hand is never surprised by a variable they forgot.
    let url = url.map(str::to_string).or_else(|| {
        std::env::var("VOLUCER_HEALTH_URL")
            .ok()
            .filter(|value| !value.trim().is_empty())
    });
    let url = match (url.as_deref(), config) {
        (Some(url), _) => url.to_string(),
        (None, Some(config)) => probe_url_for(config, path)?,
        (None, None) => {
            // Last resort, and deliberately the same default `serve` has: a `HEALTHCHECK` that
            // passes nothing at all should probe the file the `CMD` would have served.
            let default = std::env::var_os("VOLUCER_HEALTH_CONFIG")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("volucer.toml"));
            probe_url_for(&default, path)?
        }
    };
    probe(&url, timeout_ms)
}

/// The URL a configuration file's own listeners are reachable on.
fn probe_url_for(config: &Path, path: &str) -> Result<String, HealthError> {
    let loaded = Config::load(config).map_err(|err| HealthError::Config(Box::new(err)))?;
    // One listener, so one address: the readiness probe, the exposition and the worker-control
    // routes are paths on the same socket, and there is nothing to reconcile.
    let address = loaded
        .status
        .as_ref()
        .map(|status| status.listen.clone())
        .ok_or(HealthError::NoProbeListener)?;
    Ok(format!("http://{}{path}", probe_host(&address)))
}

/// The address to probe a listener on, from the address it binds.
///
/// A wildcard bind has to be turned into something connectable: `0.0.0.0:2114` and `[::]:2114` are
/// not destinations, and a `HEALTHCHECK` that tried to connect to them would fail on the one
/// configuration a container actually uses — the default, where the listener has to be reachable
/// from outside the container. Loopback is the address the process itself answers on.
fn probe_host(address: &str) -> String {
    let (host, port) = match address.rsplit_once(':') {
        Some((host, port)) => (host, port),
        None => return address.to_string(),
    };
    let host = host.trim_start_matches('[').trim_end_matches(']');
    match host {
        "0.0.0.0" | "" => format!("127.0.0.1:{port}"),
        "::" => format!("[::1]:{port}"),
        _ if address.starts_with('[') => format!("[{host}]:{port}"),
        _ => format!("{host}:{port}"),
    }
}

fn probe(url: &str, timeout_ms: u64) -> Result<(), HealthError> {
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| HealthError::NotHttp {
            url: url.to_string(),
        })?;
    let (authority, path) = match rest.find('/') {
        Some(at) => (&rest[..at], &rest[at..]),
        None => (rest, "/"),
    };
    if authority.is_empty() {
        return Err(HealthError::NoAuthority {
            url: url.to_string(),
        });
    }

    let timeout = Duration::from_millis(timeout_ms);
    // `to_socket_addrs` rather than `format!("{authority}")` into a `TcpStream`: `[::1]:2114` is a
    // valid authority that a `&str` split on `:` cannot parse, and a probe that only works for
    // IPv4 would fail on the one deployment that set `[status] address` to a v6 literal.
    let addresses: Vec<_> = authority
        .to_socket_addrs()
        .map_err(|source| HealthError::Connect {
            address: authority.to_string(),
            source,
        })?
        .collect();
    // Every resolved address is tried, because a `localhost` that resolves to `::1` first on a
    // server bound only to `127.0.0.1` is the ordinary case, not the exotic one — and a probe that
    // gave up after the first failure would report a healthy server as unhealthy.
    let mut last: Option<(std::net::SocketAddr, std::io::Error)> = None;
    let mut stream = None;
    for address in addresses {
        match TcpStream::connect_timeout(&address, timeout) {
            Ok(connected) => {
                stream = Some(connected);
                break;
            }
            Err(source) => last = Some((address, source)),
        }
    }
    let Some(mut stream) = stream else {
        // A name that resolved to nothing is not a connection failure the OS can report, so it is
        // described as what it is rather than as a fabricated "connection refused".
        let (address, source) = match last {
            Some(pair) => pair,
            None => {
                return Err(HealthError::Connect {
                    address: authority.to_string(),
                    source: std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        "the name resolved to no address",
                    ),
                });
            }
        };
        return Err(HealthError::Connect {
            address: address.to_string(),
            source,
        });
    };

    stream
        .set_read_timeout(Some(timeout))
        .and_then(|()| stream.set_write_timeout(Some(timeout)))
        .map_err(|source| HealthError::Connect {
            address: authority.to_string(),
            source,
        })?;

    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: {authority}\r\nUser-Agent: volucer-healthcheck\r\nConnection: close\r\n\r\n"
    );
    stream
        .write_all(request.as_bytes())
        .and_then(|()| stream.flush())
        .map_err(|source| HealthError::Write {
            address: authority.to_string(),
            source,
        })?;

    let mut raw = Vec::new();
    stream
        .read_to_end(&mut raw)
        .map_err(|source| match source.kind() {
            // A read that timed out is the one failure worth its own variant: "the server is
            // alive but wedged" is a different operator action from "nothing is listening".
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => HealthError::Timeout {
                address: authority.to_string(),
                timeout_ms,
            },
            _ => HealthError::Read {
                address: authority.to_string(),
                source,
            },
        })?;

    let text = String::from_utf8_lossy(&raw);
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| HealthError::NotHttpResponse {
            address: authority.to_string(),
            head: head.lines().next().unwrap_or("").to_string(),
        })?;

    if (200..300).contains(&status) {
        Ok(())
    } else {
        Err(HealthError::Unhealthy {
            address: authority.to_string(),
            status,
            body: body.trim().to_string(),
        })
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
    // All five `kind` values, in the order `BackendConfig` declares them. `attach` belongs
    // here even though it is the least-used: it is a shape the configuration accepts, and a
    // capability list that omits one is a list an operator cannot check a config against.
    println!("  backends:         worker, attach, external fpm, spawn_cgi, upstream");
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
    //
    // Colour follows whether stderr is a terminal. `fmt`'s default is to emit ANSI escapes
    // unconditionally, and every line ends up somewhere that keeps them: a container's log, a
    // `docker inspect` health entry, a systemd journal, a file. `\u{1b}[31mERROR\u{1b}[0m` is the
    // same information as `ERROR` with seven bytes of noise in front of it, and a health check that
    // fails on a misconfiguration writes the operator's only explanation there.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(std::io::stderr().is_terminal())
        .with_target(false)
        .try_init();
}

/// Where a run's configuration comes from: a file, the command line, or both.
///
/// **Two sources, one deserializer.** The file is read into a `toml::Table`, every override is laid
/// on top of it in order, and the result goes through `Config::from_table` — so a file, an `-o`
/// pair and a `SIGHUP` reload are defaulted, validated and refused by exactly one piece of code.
/// The alternative, a `serde`-only path for flags, would be a second answer to "what does this
/// configuration say", which is the drift `check` exists to prevent.
///
/// The pairs are kept as written rather than as parsed values because [`Reloader`] has to replay
/// them: a reload that re-parsed a *different* representation of the same flags would be a second
/// reading of them, and the one thing a reload must never do is change the meaning of a running
/// deployment.
#[derive(Clone)]
struct Source {
    /// The file, when there is one. `Some` for an explicit `-c` — which is then a hard error if it
    /// cannot be read, so a typo cannot silently become a default server — and for the default
    /// `volucer.toml` when that file exists.
    path: Option<PathBuf>,
    /// Every `-o` pair in the order it applies: `--listen` and `--fastcgi` first, then the
    /// `-o/--override` pairs as given.
    pairs: Vec<String>,
}

impl Source {
    /// Assemble the source from the parsed command line, without the quick-start flags.
    ///
    /// The flags without extra arguments, for a caller that has none.
    /// [`Source::resolve_with_backends`] is the one the command line uses.
    #[cfg(test)]
    fn resolve(
        config: Option<PathBuf>,
        listen: Option<String>,
        fastcgi: Option<String>,
        overrides: Vec<String>,
    ) -> Self {
        Self::resolve_with_backends(
            config,
            listen,
            fastcgi,
            overrides,
            Backends::None,
            Sizing::default(),
        )
    }

    /// [`Source::resolve`] with the quick-start backend flags.
    ///
    /// Six positional arguments is a lot for an in-process API, but this is the shape of the
    /// command line and the whole point of the type: every one of these is a flag, and none of
    /// them has a meaning apart from the others. Kept beside [`Source::resolve`] so the two
    /// callers that pass none of them — every test, and any future subcommand — say what they
    /// mean instead of spelling `None, None`.
    fn resolve_with_backends(
        config: Option<PathBuf>,
        listen: Option<String>,
        fastcgi: Option<String>,
        overrides: Vec<String>,
        backend: Backends,
        sizing: Sizing,
    ) -> Self {
        let mut pairs = Vec::with_capacity(overrides.len() + 2);
        for (key, address) in [("server.http", listen), ("server.fastcgi", fastcgi)] {
            if let Some(address) = address {
                pairs.push(string_pair(key, &address));
            }
        }

        // Every backend flag is the same *kind* of statement, made here rather than inferred
        // anywhere else: a backend exists, of this kind, with these settings, and it is the
        // default. A deployment that wants a route to reach it instead sets `default_backend`
        // itself, which these pairs are written *before* so the file or an `-o` still wins.
        match backend {
            Backends::None => {}
            Backends::PhpCgi(binary) => {
                pairs.push(string_pair("server.default_backend", "cgi"));
                pairs.push(string_pair("backends.cgi.kind", "spawn_cgi"));
                pairs.push(string_pair("backends.cgi.binary", &binary));
                if let Some(workers) = sizing.workers {
                    pairs.push(count_pair("backends.cgi.workers", workers));
                }
            }
            Backends::Worker(command) => {
                pairs.push(string_pair("server.default_backend", "app"));
                pairs.push(string_pair("backends.app.kind", "worker"));
                pairs.push(string_pair("backends.app.command", &command));
                if let Some(workers) = sizing.workers {
                    pairs.push(count_pair("backends.app.workers", workers));
                }
            }
            Backends::Fcgi(command) => {
                pairs.push(string_pair("server.default_backend", "app"));
                pairs.push(string_pair("backends.app.kind", "spawn_cgi"));
                pairs.push(string_pair("backends.app.command", &command));
                // `concurrency` rather than `children` is how many requests one slot answers,
                // and `workers` is how many slots there are. One slot by default, because a
                // FastCGI application server usually forks for itself; `--concurrency` says how
                // wide that fork is, so the pool holds enough connections to reach every child
                // instead of queueing behind one.
                pairs.push(count_pair(
                    "backends.app.workers",
                    sizing.workers.unwrap_or(1),
                ));
                if let Some(concurrency) = sizing.concurrency {
                    pairs.push(count_pair("backends.app.concurrency", concurrency));
                }
                // The front controller is *not* written here. "Only real files" is expressible
                // only as a flag — `Config::validate` refuses an empty table key on purpose — so
                // [`implied_front_controller`] supplies it through the flag instead.
            }
        }

        pairs.extend(overrides);

        // The default file is used only when it is *there*. A missing `volucer.toml` and no `-c`
        // means the command line is the whole configuration; an explicit `-c` naming a missing file
        // stays an error, because that is the difference between "I said nothing" and "I named a
        // file that is not there".
        let path = match config {
            Some(path) => Some(path),
            None => {
                let default = PathBuf::from(DEFAULT_CONFIG);
                default.exists().then_some(default)
            }
        };

        Self { path, pairs }
    }

    /// Read, merge and validate. The only way `serve`, `check` and a reload build a `Config`.
    ///
    /// **`grpc_proto` is expanded here**, between validating and returning, and that placement is
    /// the whole of how the key stays honest. It cannot happen in `Config::validate`, which is a
    /// pure function of the file — every one of its rules is checkable without a disk, and `check`
    /// depends on that to report a file it would not start from. It must not happen later either: a
    /// backend whose grants were still unresolved when the dispatcher was built would answer
    /// `UNIMPLEMENTED` to everything until the next reload. So the read goes where the file's
    /// directory is already known, and what comes out is written back into `grpc_services`, which
    /// every consumer already reads.
    ///
    /// The two refusals are `serve`'s and the reload's alike: a `.proto` that cannot be read, and a
    /// set that resolves to nothing. Both leave the running configuration untouched on a reload,
    /// which is the same whole-or-nothing rule the rest of them follow.
    fn load(&self) -> Result<Config, ServerError> {
        let mut table = match &self.path {
            Some(path) => Config::table_from_path(path)?,
            None => toml::Table::new(),
        };
        for pair in &self.pairs {
            apply_override(&mut table, pair)?;
        }
        let mut config = Config::from_table(table)?;
        config.validate()?;
        config.expand_grpc_proto(&self.dir())?;
        tracing::info!(source = %self.describe(), "configuration loaded");
        Ok(config)
    }

    /// The directory relative paths resolve against.
    ///
    /// The file's own directory, which is the rule `[server.tls]`'s `certificate`, `[server.acme]`'s
    /// `cache` and the access log already follow — and **the working directory when there is no
    /// file**, the only answer left once the file is gone. One function so `serve`, `check` and the
    /// preflight cannot disagree, and so a fileless deployment's certificate is looked for where the
    /// process was started rather than in a directory that does not exist.
    fn dir(&self) -> PathBuf {
        match &self.path {
            Some(path) => path
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| PathBuf::from("")),
            None => std::env::current_dir().unwrap_or_else(|_| PathBuf::from("")),
        }
    }

    /// How this configuration is named in `serve`'s banner and in `check`'s first line.
    fn describe(&self) -> String {
        match &self.path {
            Some(path) => path.display().to_string(),
            None => format!("command line ({} override(s), no file)", self.pairs.len()),
        }
    }
}

/// The file used when `-c` is not given and this exists.
const DEFAULT_CONFIG: &str = "volucer.toml";

/// One override's text as a TOML value.
///
/// **Parsed as a value, not as a document.** `str::parse::<toml::Value>` reads a whole
/// configuration, so `8` and `127.0.0.1:8080` both fail there and a caller that fell back to a
/// string would make every override a string — including `workers=8`, which then fails the type
/// check with a message about the *value* rather than about the number it was. The wrapping below
/// is what asks the grammar the value question, so quoting, integers, floats, booleans, arrays and
/// datetimes all behave exactly as they do in the file.
///
/// A value that still does not parse is a **string**, which is the answer a listener needs:
/// `127.0.0.1:8080` and `unix:/run/app.sock` are not TOML and are the two most common things to
/// pass.
fn literal(text: &str) -> toml::Value {
    format!("value = {text}")
        .parse::<toml::Table>()
        .ok()
        .and_then(|mut table| table.remove("value"))
        .unwrap_or_else(|| toml::Value::String(text.to_string()))
}

/// One segment of an override's key: a table key, or an index into an array.
///
/// The bracket form exists because the value side cannot reach into an array that is already
/// there: `-o 'server.routes=[…]'` *replaces* the file's routes, so amending one route of twenty
/// had no spelling at all. `-o server.routes[3].backend=staging` is that spelling, and it reads
/// the way the same thing reads in PHP.
#[derive(Debug, PartialEq)]
enum KeySegment {
    /// A table key, and the key a value is finally inserted under.
    Key(String),
    /// An index into an array. Only `[N]` has a form here: `[]` would have to mean "append", which
    /// would make the result depend on how many pairs were written before it.
    Index(usize),
}

/// Split a `KEY=VALUE` pair at the first `=` that is **not inside a quoted key**.
///
/// `split_once('=')` is nearly right, but quoting is what lets a map key hold a dot
/// (`server.env."APP.NAME"`), and a key that may hold a dot may hold an `=` too. Scanning with the
/// quote state means the value keeps every `=` it has, which is also what a query string in a
/// value needs.
fn split_pair(pair: &str) -> Option<(&str, &str)> {
    let mut quoted = false;
    let mut escaped = false;
    for (index, ch) in pair.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '\\' if quoted => escaped = true,
            '"' => quoted = !quoted,
            '=' if !quoted => return Some((&pair[..index], &pair[index + 1..])),
            _ => {}
        }
    }
    None
}

/// Split a dotted key into segments, accepting `name[N]` and `"quoted name"` on the way.
///
/// Quoting is TOML's own, and it is what makes the two things a plain dot-split cannot express
/// possible: a map key that contains a dot (`server.env."APP.NAME"=x`) and a key that only looks
/// like an index (`server.env."routes[0]"=x`). Inside a quoted key every character is literal, so
/// the two syntaxes cannot be mistaken for one another.
fn key_segments(key: &str) -> Result<Vec<KeySegment>, ServerError> {
    let problem = |reason: &str| {
        invalid_override(format!(
            "--override {key:?} {reason}; a dotted key is `section.key`, an element of an array of \
             tables is `section.array[0].key`, and a key that itself holds a dot is \
             `section.\"quoted.key\"`"
        ))
    };

    let chars: Vec<char> = key.chars().collect();
    let mut segments = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '"' {
            let (name, next) = quoted_key(&chars, i).ok_or_else(|| {
                problem("has a quoted key that is unterminated (`\\\"` and `\\\\` are the escapes)")
            })?;
            segments.push(KeySegment::Key(name));
            i = next;
        } else if chars[i] == '[' {
            let (index, next) = index_segment(&chars, i)
                .ok_or_else(|| problem("has a bracket that is not an index; write `[0]`"))?;
            segments.push(KeySegment::Index(index));
            i = next;
        } else {
            let start = i;
            while i < chars.len() && chars[i] != '.' && chars[i] != '[' {
                i += 1;
            }
            if start == i {
                return Err(problem("has an empty segment"));
            }
            let segment: String = chars[start..i].iter().collect();
            if segment.contains('"') {
                return Err(problem(
                    "has a quote inside an unquoted key; quote the whole key",
                ));
            }
            // A `]` here is an index that lost its `[`, which is a typo worth naming rather than
            // turning into a table key called `http]`.
            if segment.contains(']') {
                return Err(problem(
                    "has a `]` without a matching `[`; an index is written `[0]`",
                ));
            }
            segments.push(KeySegment::Key(segment));
            // `routes[0]` is one key in PHP and two segments here, and either way no dot sits
            // between the name and the bracket, so the bracket is allowed to abut the name.
            if i < chars.len() && chars[i] == '[' {
                continue;
            }
        }

        if i == chars.len() {
            break;
        }
        if chars[i] != '.' {
            return Err(problem(
                "has something after a segment that is neither `.` nor `[`",
            ));
        }
        i += 1;
        if i == chars.len() {
            return Err(problem("ends with a dot"));
        }
    }
    Ok(segments)
}

/// Read `[N]` starting at `chars[open]`, returning the index and the position after the `]`.
///
/// `[01]` is accepted as `1`; a negative or empty index is not, because neither names an element
/// of an array that has a length.
fn index_segment(chars: &[char], open: usize) -> Option<(usize, usize)> {
    let mut index = 0usize;
    let mut i = open + 1;
    let start = i;
    while i < chars.len() {
        let Some(digit) = chars[i].to_digit(10) else {
            break;
        };
        index = index.checked_mul(10)?.checked_add(digit as usize)?;
        i += 1;
    }
    if i == start || chars.get(i) != Some(&']') {
        return None;
    }
    Some((index, i + 1))
}

/// Read a `"quoted key"` starting at `chars[open]`, returning the key and the position after it.
fn quoted_key(chars: &[char], open: usize) -> Option<(String, usize)> {
    let mut name = String::new();
    let mut i = open + 1;
    while i < chars.len() {
        match chars[i] {
            '\\' => {
                i += 1;
                match *chars.get(i)? {
                    '"' => name.push('"'),
                    '\\' => name.push('\\'),
                    _ => return None,
                }
                i += 1;
            }
            '"' => return Some((name, i + 1)),
            other => {
                name.push(other);
                i += 1;
            }
        }
    }
    None
}

/// Whether raw text opens like a TOML array or table.
fn opens_structured(text: &str) -> bool {
    let text = text.trim_start();
    text.starts_with('[') || text.starts_with('{')
}

/// Lay one `KEY=VALUE` pair onto the table.
///
/// The value is read by [`literal`], which is what gives `-o backends.app.workers=8` an integer and
/// `-o server.http=127.0.0.1:8080` the string it looks like — no per-key knowledge here, and no way
/// for this function to disagree with the file's own typing.
fn apply_override(table: &mut toml::Table, pair: &str) -> Result<(), ServerError> {
    let (key, raw) = split_pair(pair).ok_or_else(|| {
        invalid_override(format!(
            "--override {pair:?} is not KEY=VALUE; write -o server.http=127.0.0.1:8080 \
             (quote the whole pair in a shell)"
        ))
    })?;
    let key = key.trim();
    if key.is_empty() {
        return Err(invalid_override(format!(
            "--override {pair:?} names no key; write it as KEY=VALUE"
        )));
    }

    // Expanded before it is parsed, so `${PORT:-8080}` works in a listener position exactly as it
    // does in the file — where the substitution is on the *text* for the same reason.
    let expanded = volucer_core::env::expand(raw.trim())
        .map_err(|reason| invalid_override(format!("--override {key}: {reason}")))?;
    let value = literal(&expanded);

    // `literal` answers "a string" for anything it cannot parse, which is right for a listener and
    // wrong for an array: `['a' => 'b']` — PHP, or JSON with `:` — would become a *string* and be
    // refused later for being the wrong type, a message that names neither the real mistake nor
    // the file format. Only a value that opens like TOML and did not parse is caught here, so a
    // quoted `"[time]"` is still an ordinary string.
    if opens_structured(&expanded) && matches!(value, toml::Value::String(_)) {
        return Err(invalid_override(format!(
            "--override {key}: {} is not a TOML array or table; PHP and JSON syntax are not read \
             here, so write TOML inline syntax — `[{{ key = \"value\" }}]` for an array of tables — \
             or quote the value to force a string",
            expanded.trim()
        )));
    }

    place(table, &key_segments(key)?, value, key)
}

/// Walk `segments` from `cursor`, inserting `value` at the end.
///
/// A table or an array is created only where a segment needs one, so `-o server.http=…` never
/// grows an empty `routes` array. An existing container of the *wrong* shape is refused rather
/// than replaced, because both `-o server.routes[0].backend=…` and `-o server.routes=[…]` are
/// writable and replacing would make the result depend on the order the pairs were written in —
/// a precedence an operator cannot see in the file they are reasoning about.
fn place(
    cursor: &mut toml::Table,
    segments: &[KeySegment],
    value: toml::Value,
    key: &str,
) -> Result<(), ServerError> {
    let Some((head, rest)) = segments.split_first() else {
        return Err(invalid_override(format!(
            "--override {key:?} names no key; write it as KEY=VALUE"
        )));
    };
    let KeySegment::Key(name) = head else {
        return Err(invalid_override(format!(
            "--override {key:?} indexes a value that is not there; an index needs an array to \
             index, so write `section.array[0]`"
        )));
    };
    if rest.is_empty() {
        cursor.insert(name.clone(), value);
        return Ok(());
    }

    let wants_array = matches!(rest[0], KeySegment::Index(_));
    let entry = cursor.entry(name.clone()).or_insert_with(|| {
        if wants_array {
            toml::Value::Array(Vec::new())
        } else {
            toml::Value::Table(toml::Table::new())
        }
    });
    match entry {
        toml::Value::Table(table) if !wants_array => place(table, rest, value, key),
        toml::Value::Array(items) if wants_array => place_in_array(items, rest, value, key),
        other => {
            let hint = if wants_array {
                format!("it is not an array, so write `{name}.key` or replace it with `{name}=[…]`")
            } else {
                format!("it is not a table, so write `{name}[0].key`")
            };
            Err(invalid_override(format!(
                "--override {key:?} passes through {name:?}, which already holds a {}; {hint}",
                other.type_str()
            )))
        }
    }
}

/// Take `segments` (which begin with an index) inside `items` and insert `value` where they end.
///
/// An index may name an existing element or **one past the end**, which appends and is the only
/// way to add an entry without writing the whole array — the reason this syntax exists. Anything
/// further is refused rather than padded: an element invented in the middle would be a route or a
/// backend that nobody wrote and that no default can guess.
fn place_in_array(
    items: &mut Vec<toml::Value>,
    segments: &[KeySegment],
    value: toml::Value,
    key: &str,
) -> Result<(), ServerError> {
    let Some((head, rest)) = segments.split_first() else {
        return Err(invalid_override(format!(
            "--override {key:?} names no key; write it as KEY=VALUE"
        )));
    };
    let KeySegment::Index(index) = *head else {
        return Err(invalid_override(format!(
            "--override {key:?} names an array but no element; write `[…]` or `[0]`"
        )));
    };
    if index > items.len() {
        return Err(invalid_override(format!(
            "--override {key:?} names element {index} of an array that holds {}; an index may add \
             one element at the end, not leave a gap",
            items.len()
        )));
    }
    if index == items.len() {
        if rest.is_empty() {
            items.push(value);
            return Ok(());
        }
        items.push(toml::Value::Table(toml::Table::new()));
    }
    if rest.is_empty() {
        items[index] = value;
        return Ok(());
    }
    match &mut items[index] {
        toml::Value::Table(table) => place(table, rest, value, key),
        other => Err(invalid_override(format!(
            "--override {key:?} passes through element {index}, which already holds a {}; name a \
             key inside it, or replace the element with `[{index}]=…`",
            other.type_str()
        ))),
    }
}

/// One `-o` pair that cannot be applied.
///
/// `volucer_core::Error::Config` rather than a new variant: a bad override *is* a configuration
/// error, and it reaches the operator through the same `configuration error: …` line as a bad file.
fn invalid_override(reason: String) -> ServerError {
    ServerError::from(volucer_core::Error::Config(reason))
}

/// One `KEY=VALUE` pair whose value is forced to be a TOML **string**.
///
/// Quoted rather than passed to [`literal`]: a listener may be `unix:/run/app.sock`, which no TOML
/// grammar accepts unquoted, and a bare `8080` would otherwise be read as a number and refused by
/// the type check. A `default_missing_value = "8080"` on a string-typed key is exactly that trap.
/// Backslashes and quotes are escaped, so a Windows path survives as one value.
fn string_pair(key: &str, value: &str) -> String {
    format!(
        "{key}=\"{}\"",
        value.replace('\\', "\\\\").replace('"', "\\\"")
    )
}

/// One `KEY=<N>` pair whose value is a TOML **integer**.
///
/// A number, not a string: [`literal`] reads it as TOML, which is what makes the type check accept
/// it without a per-key rule.
fn count_pair(key: &str, count: u32) -> String {
    format!("{key}={count}")
}

/// Which backend a quick-start flag asked for.
///
/// One value rather than three `Option`s threaded separately, because the three flags are one
/// choice — `clap`'s group already refuses them together — and the pairs each one writes are the
/// same three statements with different values.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Backends {
    /// No backend flag: the file, the `-o` pairs, or nothing (a static-only deployment).
    None,
    /// `--php-cgi [PATH]`: a `spawn_cgi` pool driven by the `php-cgi` argv.
    PhpCgi(String),
    /// `--worker COMMAND`: a resident process speaking the worker protocol.
    Worker(String),
    /// `--fcgi COMMAND`: any FastCGI server with `{address}`.
    Fcgi(String),
}

impl Backends {
    /// Read the three flags as one choice. `clap`'s group makes at most one of them `Some`.
    fn from_flags(php_cgi: Option<String>, worker: Option<String>, fcgi: Option<String>) -> Self {
        match (php_cgi, worker, fcgi) {
            (Some(binary), _, _) => Backends::PhpCgi(binary),
            (_, Some(command), _) => Backends::Worker(command),
            (_, _, Some(command)) => Backends::Fcgi(command),
            _ => Backends::None,
        }
    }
}

/// The sizing a quick-start flag asked for: how many slots, and how wide each one is.
///
/// A struct rather than two more positional arguments, because the two mean different things and
/// only one of them applies to each backend: `workers` is slots, `concurrency` is requests per
/// slot. `clap` keeps `--workers` to any backend and `--concurrency` to `--fcgi`, so a `Backends`
/// plus this is a complete statement of what the command line asked for.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Sizing {
    workers: Option<u32>,
    concurrency: Option<u32>,
}

impl Sizing {
    /// Read the two flags, refusing `--concurrency` for a backend it cannot describe.
    ///
    /// **Checked here rather than by `requires = "fcgi"` on the argument**, because clap's
    /// `requires` is satisfied by *presence*: `--php-cgi` takes an optional value, so clap reads it
    /// as never missing, and `--php-cgi --concurrency 4` would pass a `requires` check while
    /// meaning nothing. `backends.cgi.concurrency` is a configuration error for that driver
    /// anyway, so the only honest thing to do is say so before the configuration is built.
    fn from_flags(
        backend: &Backends,
        workers: Option<u32>,
        concurrency: Option<u32>,
    ) -> Result<Self, ServerError> {
        if concurrency.is_some() && !matches!(backend, Backends::Fcgi(_)) {
            return Err(invalid_override(
                "--concurrency sets how many requests one --fcgi slot serves at once, so it needs \
                 --fcgi: a worker answers one request at a time by construction, and a php-cgi \
                 pool expresses the same number as `children`"
                    .to_string(),
            ));
        }
        Ok(Self {
            workers,
            concurrency,
        })
    }
}

/// The front controller `--fcgi` implies, when the operator named none.
///
/// A WSGI or Rack program is not `index.php`: leaving the PHP-shaped default in place means every
/// path naming no file resolves to `index.php`, which does not exist, and answers 404. So `--fcgi`
/// means "serve only real files" unless told otherwise — the same meaning `--front-controller ''`
/// has.
///
/// **It is returned as a flag rather than written as a configuration key**, and that is not
/// cosmetic: `Config::validate` refuses an empty `server.front_controller` in the table, because
/// TOML cannot tell `front_controller = ""` from an absent key and the meaning is reserved for the
/// flag. Returning it here puts it on the same path an explicit `--front-controller` takes, so the
/// reserved spelling has exactly one implementation.
fn implied_front_controller(backend: &Backends) -> Option<&'static str> {
    matches!(backend, Backends::Fcgi(_)).then_some("")
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

/// Resolve a TLS path against the directory the configuration was read from.
///
/// Deliberately **not** the document-root rule, which is relative to the process's working
/// directory. A certificate beside `volucer.toml` is the common case, and resolving it
/// against wherever the process happened to start is a footgun with a security consequence:
/// the server would start with whatever certificate it found, or fail to start at all, for a
/// reason invisible in the file. An absolute path is used as written.
///
/// `config_dir` is the file's directory — or the working directory when the deployment has no
/// file at all, which [`Source::dir`] resolves once and every caller shares, so a fileless `check`
/// and a fileless `serve` name the same certificate.
fn resolve_tls_path(path: &Path, config_dir: &Path) -> PathBuf {
    if path.is_absolute() {
        return path.to_path_buf();
    }
    // A bare filename such as `volucer.toml` has an empty parent, which means the working
    // directory — the same answer `Path::parent` gives and the one a reader expects. Joining an
    // empty directory is exactly that, so one arm covers both.
    config_dir.join(path)
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
fn resolved_server_tls(config: &Config, config_dir: &Path) -> Option<TlsPaths> {
    let tls = config.server.tls.as_ref()?;
    let certificate = tls.certificate.as_ref()?;
    let key = tls.key.as_ref()?;
    Some(TlsPaths {
        certificate: resolve_tls_path(certificate, config_dir),
        key: resolve_tls_path(key, config_dir),
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
fn resolved_site_tls(config: &Config, config_dir: &Path) -> Vec<(String, TlsPaths)> {
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
                    certificate: resolve_tls_path(certificate, config_dir),
                    key: resolve_tls_path(key, config_dir),
                },
            ))
        })
        .collect()
}

/// Report the `Strict-Transport-Security` policy each site will actually send.
///
/// Printed per site rather than once, because the whole point of the per-site key is that one site
/// can differ — and an operator reading `check` needs to see the header a given name's clients
/// receive. `(none)` for a site is a fact worth printing: it is what a browser will store for that
/// name, and it distinguishes "no policy" from "the deployment's policy, inherited".
fn print_hsts(config: &Config, sites: &Sites) {
    let default = sites.default_site();
    match (&default.hsts, &config.server.hsts) {
        (Some(policy), _) => println!("  hsts:             {policy}"),
        (None, Some(_)) => println!("  hsts:             (disabled)"),
        (None, None) => {}
    }
    // Only sites that state a table of their own, plus any whose resolved policy differs from the
    // deployment's. A site that inherits silently is the ordinary case, and repeating the same
    // header under every name would bury the one that differs.
    for site in sites.named().values() {
        let differs = site.hsts != default.hsts;
        if !differs {
            continue;
        }
        match &site.hsts {
            Some(policy) => println!("    site {}:        {policy}", site.name),
            None => println!(
                "    site {}:        (none: emits no header, though the deployment has one)",
                site.name
            ),
        }
    }
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
    source: &Source,
    document_root_flag: Option<&Path>,
    front_controller_flag: Option<&str>,
    max_connections: usize,
) -> Result<(), ServerError> {
    let config = source.load()?;
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
    // `https` counts: it is an independently bound TLS listener, so a configuration that
    // states only `https` has a listener. Checking `http` and `fastcgi` alone refused a
    // deployment whose whole point was to answer on :443 only — and `validate` accepts it,
    // so the refusal arrived here, after `check` had already said OK.
    if http_address.is_none() && fastcgi_address.is_none() && config.server.https.is_none() {
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
        &source.dir(),
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

    // **A backend that serves any gRPC service says so at warning level.** The default grants
    // nothing, so this can only be a decision somebody wrote down — but it is a decision that gives
    // up the pre-body refusal, and a worker facing the public that opted out should be visible in
    // the log rather than only in the file. Reported here, once, rather than per reload: the flag
    // itself is restart-only, and a warning repeated on every `SIGHUP` is a warning people filter.
    for (name, grants) in config.grpc_grants_all() {
        if grants.is_any() {
            tracing::warn!(
                backend = %name,
                "grpc_serve_any is set, so every well-formed gRPC method path is delivered to this \
                 backend's workers; the application answers UNIMPLEMENTED for services it does not \
                 implement, and an unknown service therefore costs a worker slot"
            );
        }
    }

    println!("volucer");
    println!("  document root:    {}", document_root.display());
    println!(
        "  front controller: {}",
        front_controller
            .as_deref()
            .unwrap_or("(none: only real .php files are served)")
    );
    println!("  backends:         {}", config.backends.len());
    if !config.server.routes.is_empty() {
        println!("  routes:           {}", config.server.routes.len());
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
    for warning in volucer_core::preflight::inspect_with_front_controller(
        &config,
        &document_root,
        source.path.as_deref(),
        front_controller_flag,
    ) {
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
    let (observe, telemetry) = build_observability_with_exporter(&config, &source.dir());
    let observe = Arc::new(observe);
    if let Some(endpoint) = telemetry.endpoint() {
        println!("  telemetry:        {endpoint}");
    }

    // The rate limit, when the deployment asked for one. Built here — before either front-end —
    // because one instance has to reach **both**. Two would give a client twice the configured
    // allowance depending on which protocol received its request, which no single-front-end test can
    // see.
    //
    // **Reloadable**, as of the `RateLimitCell` work: the cell is built once and cloned into both
    // front-ends and into the reloader, so a `SIGHUP` replaces the policy both of them consult. The
    // buckets survive a change of rate (`RateLimitPolicy::adopt_buckets`) and are replaced only when
    // the *identity* changes — which is the honest answer, because there is no way to map one
    // caller's bucket onto another's.
    let rate_limit = Arc::new(volucer_http::RateLimitCell::new(
        config
            .server
            .rate_limiter()
            .map_err(|reason| ServerError::Listener { reason })?,
    ));

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
            .with_observability(Arc::clone(&observe))
            .with_rate_limit(Arc::clone(&rate_limit));
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
    let config_dir = &source.dir();
    let tls_active = config.server.tls.is_some();
    let https_address = config.server.https.clone();
    let tls_on_http = tls_active && https_address.is_none();

    // Which sites redirect is the site table's answer — a site's own key, or `[server]`'s for the
    // sites that inherit. What this function supplies is only the *target*: the port that actually
    // speaks TLS, which no site can know.
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
    // split deployment names the `https` port with *no* acceptor, and giving it one would turn it
    // into a TLS port that no cleartext client could reach. Which sites actually redirect comes
    // from the site table, so this key is only the target's port.
    let http_settings = volucer_http::ListenerSettings {
        tls: if tls_on_http {
            Some(volucer_http::TlsSettings {
                config: &config,
                config_dir,
            })
        } else {
            None
        },
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
    // Loading it here rather than inside the listener is also the ordering that matters:
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
            .with_client_ip(Arc::clone(&client_ip))
            .with_rate_limit(Arc::clone(&rate_limit));
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
    // **One gate for the process**, built before the status listener so the listener and the reloader
    // hold the same cell. Replacing it on a reload is what makes `[status.workers] allow` changeable
    // without restarting: a method that is not granted does not exist, which is the same answer a
    // deployment that never asked for it gives.
    let reset_gate = Arc::new(match &config.status {
        Some(status) => volucer_http::WorkerResetGate::open(worker_grant(status)),
        None => volucer_http::WorkerResetGate::closed(),
    });

    let status = start_status(&config, &dispatcher, &reset_gate)?;

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

    // The call plane, when the deployment asked for one. Bound **after** the request listeners, so
    // a request that arrives during startup reaches a backend that is already up, and **before**
    // the startup line below, so an occupied address is a message rather than a socket that
    // silently is not there.
    //
    // Refused rather than skipped when `[rpc]` is configured and the dispatcher has no call plane —
    // which is the no-entropy case in `rpc::new_secret`. A deployment that asked for a call plane
    // and did not get one must not be told the server started.
    let rpc_listener = match (&config.rpc, dispatcher.rpc()) {
        (Some(rpc_config), Some(grants)) => {
            let listener = RpcListener::bind(rpc_config, Arc::clone(grants)).map_err(|err| {
                ServerError::Listener {
                    reason: err.to_string(),
                }
            })?;
            println!(
                "  rpc:              {} (call plane)",
                listener.configured_address()
            );
            Some(listener)
        }
        (Some(_), None) => {
            return Err(ServerError::Listener {
                reason:
                    "[rpc] is configured but the call plane could not be built; see the errors \
                         above, which name the reason"
                        .to_string(),
            })
        }
        (None, _) => None,
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

    // On a thread of its own, so the call plane is available from the first request rather than only
    // after the request listeners stop — and because the FastCGI listener owns this thread. The
    // handle is kept for the shutdown sequence, which stops it *first*: a call that arrives while
    // the stores are being torn down should be refused by a closed socket rather than served against
    // half-live state.
    let rpc_thread = match rpc_listener {
        Some(listener) => {
            let listener = Arc::new(listener);
            let stop = listener.stop_handle();
            let running = Arc::clone(&listener);
            let handle = std::thread::Builder::new()
                .name("rpc".to_string())
                .spawn(move || {
                    if let Err(err) = running.run() {
                        tracing::error!(%err, "the [rpc] listener stopped with an error");
                    }
                })
                .map_err(|err| ServerError::Listener {
                    reason: format!("the [rpc] listener could not be started: {err}"),
                })?;
            Some((stop, handle, listener))
        }
        None => None,
    };

    install_shutdown_handler(
        fastcgi.as_ref().map(|l| l.shutdown_handle()),
        Reloader {
            source: source.clone(),
            document_root_flag: document_root_flag.map(Path::to_path_buf),
            front_controller_flag: front_controller_flag.map(str::to_string),
            registry,
            tls: tls_state.clone(),
            observe,
            rate_limit,
            reset_gate,
        },
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

    // Stopped **first**, before the request listeners and before anything is reaped: a call that
    // arrives while the stores are being torn down should meet a closed socket rather than be served
    // against half-live state. The summary follows, and it is printed even at zero because the
    // interesting case is the deployment whose application never learned to call.
    if let Some((stop, handle, listener)) = rpc_thread {
        stop.stop();
        let _ = handle.join();
        let stats = listener.stats();
        println!(
            "rpc: {} call(s), {} refused, {} connection(s)",
            stats.calls, stats.refused, stats.connections
        );
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
        // while a drop count of zero is noise. A deployment with no `[status.metrics]` table has no other way
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
/// It is initialised on first use, which is the first scrape — so a deployment with no `[status.metrics]`
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

/// Declare a deployment's `[status.metrics.collect]` names in the process registry.
///
/// A one-line forwarder, and the commentary lives on [`volucer_server::describe_collected`] — which
/// is where the function can be tested rather than copied. What this adds is the process's own
/// registry, which a test cannot reach.
fn describe_collected(metrics: &volucer_core::config::StatusMetrics) {
    volucer_server::describe_collected(&METRICS, metrics);
}

/// Build the metrics and status listener, when the configuration asks for one.
///
/// **One listener for one table**, because a deployment that wants a scraper and an orchestrator
/// wants two paths on one port, and two ports would be two things to firewall. Which endpoints exist
/// is decided by which sub-tables are present: `[status.metrics]` absent serves no exposition,
/// `[status.workers]` absent serves no worker-control route, and the probes are always there.
fn build_status(
    config: &Config,
    dispatcher: &Arc<Dispatcher>,
    reset_gate: &Arc<volucer_http::WorkerResetGate>,
) -> Result<Option<RunningStatus>, ServerError> {
    let Some(status) = &config.status else {
        return Ok(None);
    };

    let readiness = || -> Arc<dyn volucer_http::Readiness> {
        Arc::new(DispatcherReadiness {
            dispatcher: Arc::clone(dispatcher),
        })
    };

    let metrics = status.metrics.as_ref().map(|metrics| {
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
        // Always present: `[status]` without probes would be an address that answers nothing, and
        // the operator who wrote one is configuring a probe rather than an empty socket.
        readiness: Some(readiness()),
        unavailable_status: status.health.unavailable_status,
        // The worker routes exist only when the deployment asked, and the implementation is the
        // dispatcher's own pool directory — the same one `workers.list` reads, so a `GET /workers`
        // and a worker's own `workers.list` cannot disagree about what a worker is.
        //
        // The gate is a second question and deliberately separate: `workers` says whether there is a
        // pool to control, the gate says whether a caller may reach it *now*. Splitting them is what
        // lets `[status.workers] allow` be changed by a reload without dropping the pool handle — and
        // an ungranted route is a 404 rather than a 403, which is the property that stops an attacker
        // enumerating them.
        workers: workers_control(dispatcher),
        reset_gate: Some(Arc::clone(reset_gate)),
    };

    RunningStatus::start(&status.listen, services)
        .map(Some)
        .map_err(|err| ServerError::Listener {
            reason: err.to_string(),
        })
}

/// The methods a deployment's `[status.workers]` grants, as the listener's own type.
///
/// The third seam between `volucer-core` and `volucer-http`, after the sampling policy and the
/// readiness trait, and for the same reason as those: the configuration model stays free of the
/// crate that serves, so `volucer-core` remains the leaf every other crate depends on.
///
/// An unknown name is ignored with an error rather than asserted on: `Config::validate` refuses one
/// before a configuration is ever served, so the arm is unreachable from a running server — but this
/// is also reachable from a `Config` built in a test and never validated, and a panic in a startup
/// path is a worse answer there than the route simply not existing.
fn worker_grant(status: &volucer_core::config::StatusConfig) -> volucer_http::WorkerGrant {
    let mut grant = volucer_http::WorkerGrant::NONE;
    let Some(workers) = &status.workers else {
        return grant;
    };
    for method in &workers.allow {
        match method.as_str() {
            "workers.list" => grant.list = true,
            "workers.reset" => grant.reset = true,
            other => tracing::error!(
                method = other,
                "[status.workers] names a method this listener does not serve; validation refuses \
                 one, so this configuration was never checked. The route is not granted"
            ),
        }
    }
    grant
}

/// The worker-control routes' implementation, when this deployment has a worker pool at all.
///
/// **Not gated on the grant**, and that is what a reloadable gate forces: the pool handle has to be
/// there *before* a method can be granted, because a reload must not have to rebuild a listener to
/// start answering `/workers`. Whether a caller may reach a route is
/// [`WorkerResetGate`](volucer_http::WorkerResetGate)'s answer, asked per request.
///
/// `None` for a deployment with no worker pool: there is nothing to list, and `/workers` would be an
/// empty array that looks like a pool with no workers. A static-only or upstream-only deployment gets
/// no route rather than a misleading one, whatever the grant says.
fn workers_control(dispatcher: &Arc<Dispatcher>) -> Option<Arc<dyn volucer_http::WorkerControl>> {
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
    reset_gate: &Arc<volucer_http::WorkerResetGate>,
) -> Result<Option<RunningStatus>, ServerError> {
    let running = build_status(config, dispatcher, reset_gate)?;
    if let Some(listener) = &running {
        println!("  status:           {}", listener.local_addr());
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
    //
    // The consumer's `env` is merged over `[server.env]` here, for the same reason the dispatcher
    // merges a backend's: `[jobs.pool]` is not a backend, so it does not pass through
    // `BackendConfig::with_server_env`, and a baseline that reached a web worker but not a consumer
    // would be a deployment whose queue talked to the wrong database.
    let mut pool_config = pool_config.clone();
    pool_config.env = config.server.env_for(&pool_config.env);
    let consumers = Consumers::new(
        &plane,
        WorkerPool::start_consumers("jobs", &pool_config)?,
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
    /// Where the configuration came from, replayed exactly as it was given: the file (if any) with
    /// the same overrides in the same order. Open decision 9.1's answer — what an operator expects
    /// from `SIGHUP`, and no new concept such as a marker file.
    ///
    /// **A `Source` rather than a path**, because a reload that dropped the overrides would install
    /// a configuration nobody wrote: a deployment started with `-o server.document_root=public`
    /// would move its root to `.` and start serving 404s, with the change attributed to whatever
    /// the operator edited.
    source: Source,
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
    /// The rate-limit cell both front-ends consult, so a `SIGHUP` can change the limit.
    ///
    /// **Held as the same `Arc` both front-ends carry**, which is what makes the replacement visible
    /// to them: the policy lives behind an [`Swappable`](volucer_watch::Swappable) inside this value,
    /// so replacing it moves the limit every front-end consults rather than installing a second one
    /// they do not read.
    ///
    /// The buckets are carried across by
    /// [`volucer_core::RateLimitPolicy::adopt_buckets`], so a reload that tightens the rate throttles
    /// a caller from where they already were. That is the difference between a limit and a limit that
    /// can be escaped by asking the operator to reload.
    rate_limit: Arc<volucer_http::RateLimitCell>,
    /// The worker grant, so `[status.workers] allow` can be changed by a
    /// reload.
    ///
    /// The **same cell the status listener reads**, which is what makes the change visible: the
    /// listener consults it per request, so a shut gate makes `/workers` a 404 and a `POST` a 405 on
    /// the very next request rather than at the next restart. Validation has already refused a
    /// wildcard `[status] address` beside the flag, so the *open* direction is as safe through a
    /// reload as it was at startup.
    reset_gate: Arc<volucer_http::WorkerResetGate>,
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
    ///
    /// **A fileless deployment reloads to the same configuration it started with**, because the
    /// overrides are replayed rather than re-read. That is the honest answer to a `SIGHUP`: flags
    /// are not a file, so there is nothing new to pick up, and a reload that changed them would have
    /// to invent a source. The pair is still re-applied, so a reload that *refuses* (say, because a
    /// certificate the overrides name has gone) leaves the previous one serving rather than
    /// half-installing one.
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

        let config = match self.source.load() {
            Ok(config) => config,
            // `Source::load` reports the same three failures `serve` refuses at startup: an
            // unreadable file, a bad override, and a configuration that parses and does not
            // validate. All of them are *refusals* here — nothing is installed — which is the only
            // safe direction. A reload that half-applied a file would leave the operator unable to
            // tell which half took effect.
            Err(err) => {
                tracing::error!(%err, "reload refused: the configuration was not accepted");
                return;
            }
        };

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
        // The gRPC grants are rebuilt from the same file and installed in the same swap as the site
        // table: a route names a backend and a grant says whether that backend may serve a service,
        // so a reload that installed one without the other would be half-applied by definition.
        let grpc_grants = Arc::new(config.grpc_grants_all());
        let _previous = self.registry.apply(sites, grpc_grants);
        tracing::info!(
            generation,
            source = %self.source.describe(),
            document_root = %document_root.display(),
            sites = config.sites.len(),
            "configuration reloaded"
        );

        // The rate limit, third and independent of both tables above — the same ordering rule the
        // certificates get, and for the same reason: a limit that cannot be built must not take the
        // sites with it, and it is reported rather than silently kept.
        match config.server.rate_limiter() {
            Ok(policy) => {
                let carried = self.rate_limit.replace(policy);
                if carried {
                    tracing::info!(
                        "the rate limit was replaced; every tracked caller keeps the tokens it had"
                    );
                } else {
                    tracing::info!(
                        "the rate limit was replaced and starts from empty buckets — the key mode or \
                         the header changed, so no existing caller's bucket could be carried over"
                    );
                }
            }
            Err(reason) => tracing::error!(
                %reason,
                "the rate limit was not replaced; the previous one is still limiting"
            ),
        }

        // Certificates are a second, independent table. Refreshed after the sites so that a
        // certificate that cannot be read leaves the *sites* reloaded and the certificates as they
        // were — one table refusing must not take the other with it, and both are whole-or-nothing
        // on their own.
        //
        // This runs even when ACME manages some of the deployment's certificates, and the **merge
        // inside `refresh`** is what makes that safe. It used to be skipped, because a rebuild from
        // the configuration alone would replace an issued certificate with a placeholder;
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

        // The worker-control grant, fourth and independent of the three above. An ungranted method
        // makes its route 404 on the next request; a granted one makes it answer. The validation that
        // refuses a wildcard `[status] listen` beside `workers.reset` ran when the file was read, so
        // this cannot open a mutation surface at an address the deployment would not have been
        // allowed to start with.
        //
        // A **removed** `[status]` table closes the gate, because the grant it carried is gone — the
        // routes are the grant's, and the grant is the table's. `listen` is restart-only, which is why
        // the listener itself survives; only the surface it used to expose does not.
        let granted = match &config.status {
            Some(status) => worker_grant(status),
            None => volucer_http::WorkerGrant::NONE,
        };
        let was = self.reset_gate.set(granted);
        if was != granted {
            tracing::info!(
                list = granted.list,
                reset = granted.reset,
                "this reload changed the worker-control routes: {} list, {} reset",
                if granted.list { "granted" } else { "removed" },
                if granted.reset { "granted" } else { "removed" }
            );
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
        "a reload does not change: [server.env] and a backend's env (the processes are already running with the old values, so a reload would leave a replacement worker on the new ones and its siblings on the old)",
        "a reload does not change: [server.tls] min_version and alpn (the rustls ServerConfig is built once)",
        "a reload does not change: [server.proxy] (whose forwarded headers are believed is a statement about this deployment's topology, not about a request)",
        "a reload does not change: [server.access_log] path, format and request_id_header (SIGHUP closes and reopens the log a rotation moved, but the destination is chosen at startup)",
        "a reload does not change: [status] listen (the listener is bound, so a new address is a new socket)",
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
fn install_shutdown_handler(shutdown: Option<volucer_server::ShutdownHandle>, reloader: Reloader) {
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
    source: &Source,
    document_root_flag: Option<&Path>,
    front_controller_flag: Option<&str>,
    extra_php_dirs: &[PathBuf],
) -> Result<(), ServerError> {
    let config = source.load()?;

    // The same resolution `serve` performs, so this reports the deployment that
    // would actually start rather than the one written in the file.
    let document_root = absolute_document_root(&resolve_document_root(document_root_flag, &config));
    let front_controller = config
        .server
        .resolve_front_controller(front_controller_flag);
    // Where relative paths resolve: the file's directory, or the working directory for a fileless
    // deployment. Taken once, from the same helper `serve` uses, so the two cannot disagree about
    // which certificate `check` inspected.
    let config_dir = source.dir();

    // The *source* rather than a path, because a deployment with no file has no path to print —
    // and "which of these two is my server running" is the first question a `check` report has to
    // answer when the same binary can be started either way.
    println!("configuration: {}", source.describe());
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
    match (resolved_server_tls(&config, &config_dir), generated_tls) {
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
    for (name, site_tls) in resolved_site_tls(&config, &config_dir) {
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
            let cache = resolve_tls_path(&acme.cache, &config_dir);
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
            if let Some(server_tls) = resolved_server_tls(&config, &config_dir) {
                if let Some(lifetime) = certificate_lifetime(&server_tls.certificate) {
                    println!("      {}", lifetime.report("serving certificate"));
                }
            }
            for (name, site_tls) in resolved_site_tls(&config, &config_dir) {
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
        let cache = resolve_tls_path(&site_acme.resolved_cache(&config.server.acme), &config_dir);
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
        let default = if config.server.default_backend.as_deref() == Some(name.as_str()) {
            " (default)"
        } else {
            ""
        };
        println!("    {name}: {}{default}", backend.kind().replace('_', "-"));
        println!(
            "      per-request reset: {}",
            backend.request_scope().describe()
        );
        // Concurrency, because it is the first thing an operator tunes and the one place where
        // the answer differs between kinds for a reason that is invisible in the configuration.
        // `external` has no `workers` key at all, and saying so is more useful than leaving the
        // absence to be noticed.
        println!("      concurrency:       {}", backend.concurrency());
        // The relay is the one thing an operator has to look at for an `attach` backend, and it
        // is the one thing volucer does not own: nothing in this process started it, so "is it
        // there" is a question for `preflight` rather than for `check`. Printing the address is
        // what makes the two processes' relationship visible in one line.
        if let volucer_core::config::BackendConfig::Attach(attach) = backend {
            // `relay()` is what the pool will actually dial — the configuration's value, or the
            // one `VOLUCER_RELAY_SOCKET` supplies — and printing the *resolved* address is the
            // point of this line: an omission plus a variable in the operator's shell is exactly
            // the case where "what the file says" and "what will be dialled" differ.
            let relay = attach
                .relay()
                .map(|relay| relay.into_owned())
                .unwrap_or_else(|reason| format!("<unresolved: {reason}>"));
            println!("      relay:             {}", relay.trim());
            println!("      connections:       {}", attach.workers);
        }
        // And for `external`, the address, for the same reason: it is the other process's socket,
        // nobody in this one created it, and an operator reading `check` should be able to see
        // which php-fpm this backend is actually pointed at — plus the two things that decide
        // whether the far side will keep up.
        if let volucer_core::config::BackendConfig::External(external) = backend {
            println!("      socket:            {}", external.socket.trim());
            println!(
                "      max connections:   {}",
                match external.max_connections {
                    Some(max) => format!("{max} (requests beyond it wait)"),
                    None => "none (one connection per request in flight)".to_string(),
                }
            );
            // What the far side says about itself, when it says anything. This is the only
            // place volucer asks: a human is waiting, a round trip to a socket the operator
            // just named is what they asked for, and the answer is printed rather than acted
            // on. `serve` never queries it — see `ExternalConfig` for why a gauge that
            // php-fpm answers with an empty result is not something to trust on the accept path.
            print_advertised_capacity(external);
        }
        // A socket-transport worker backend prints *where*, because the transport is invisible on
        // the wire: a pool on the wrong one answers every request with a 200, only slower. The
        // resolved form is printed — `unix:…<slot>.sock` — rather than the prefix, because the
        // prefix is what the file says and the operator already has the file.
        if let volucer_core::config::BackendConfig::Worker(worker) = backend {
            if !worker.transport.is_pipe() {
                println!(
                    "      transport:         {} on {}",
                    worker.transport,
                    match &worker.socket {
                        Some(prefix) => {
                            format!("{}<slot>.sock", prefix.trim_end_matches(".sock"))
                        }
                        None => "a loopback ephemeral port".to_string(),
                    }
                );
            }
        }
        // And a `spawn_cgi` backend prints how its slots are started and how wide each is.
        //
        // Both lines answer questions the file alone cannot. `driver` says which of the two
        // spawn shapes is in use, because a reader cannot tell from `binary`/`command` whether
        // `children` or `concurrency` is the one in force. The prefork line exists because
        // `workers` alone stopped being the process count when `children` did: `workers ×
        // (children + 1)` is what the machine will run, and a reader tuning memory needs that
        // number rather than the two factors to multiply themselves.
        if let volucer_core::config::BackendConfig::SpawnCgi(cgi) = backend {
            if cgi.is_php_cgi() {
                println!("      driver:            php-cgi (binary)");
                // The resolved path, not the configured string: with a bare `php-cgi` the two
                // differ, and "which php-cgi will run" is the question this line answers — the
                // same rule the `command` driver's `program` line follows.
                if let Some(configured) = &cgi.binary {
                    let resolved = volucer_core::php::resolve_cgi_binary(configured);
                    if resolved == *configured {
                        println!("      program:           {}", resolved.display());
                    } else {
                        println!(
                            "      program:           {} (from binary {:?})",
                            resolved.display(),
                            configured
                        );
                    }
                }
            } else {
                println!("      driver:            command");
                println!(
                    "      program:           {}",
                    cgi.command.as_deref().unwrap_or("(none)").trim()
                );
                println!(
                    "      concurrency:       {} per slot",
                    cgi.slot_concurrency()
                );
                // What the program will actually be told, when the command asks to be told.
                //
                // The line above states the number the pool admits; this one states the number
                // the *program* receives, and they are the same by construction — which is the
                // whole reason the placeholder exists. Printing it is what makes the `0` case
                // visible: a file with no `concurrency` key and a command with `{concurrency}`
                // resolves to `1`, not to `0`, and an operator who wrote neither number should
                // be able to see that from `check` rather than from the program's behaviour.
                // `{address}` is left as written because it is the one value the pool decides
                // per slot, and inventing a slot number here would be a different address from
                // the one `serve` binds.
                if let Some(command) = &cgi.command {
                    if command.contains(volucer_core::config::FCGI_CONCURRENCY_PLACEHOLDER) {
                        println!(
                            "      command resolves:  {}",
                            command.replace(
                                volucer_core::config::FCGI_CONCURRENCY_PLACEHOLDER,
                                &cgi.slot_concurrency().to_string()
                            )
                        );
                    }
                }
                if let Some(dir) = &cgi.working_dir {
                    println!("      working dir:       {}", dir.display());
                }
            }
            // The host total, spelled as the multiplication that produces it.
            //
            // This is the line the file cannot answer. `workers` and `concurrency` are two
            // numbers one row apart, and the process count is their product — for a `command`
            // backend because the pool starts that program once per slot and the program forks
            // its own concurrency, for php-cgi because `children` is added to the master. An
            // operator sizing memory, file descriptors or a container's `pids_limit` needs the
            // product, so printing it (and the working) turns a fact that lived only in the
            // documentation into something `check` states about *this* deployment.
            //
            // It also replaces the separate prefork line that used to sit here. That line said
            // the same arithmetic for php-cgi only, which left the `command` driver — the one
            // whose `concurrency` is easiest to misread as a total — with no total at all.
            let (processes, working) = cgi.process_count();
            println!("      processes:         {processes} ({working})");
        }
        // A `kind = "upstream"` backend was reported here as "not served yet" for exactly one
        // commit — the one that built the client before any listener could reach it — and that line
        // went away with the same commit that added the request path below it. Both existed so that
        // `check` could not answer "configuration OK" for a kind `serve` refused, and both were
        // removed together for the same reason.
        //
        // What *is* printed is the merged environment's variable **names**, because they are the
        // half an operator can act on and the values are the half that must not reach a terminal
        // they later paste into a ticket. Printing nothing would hide the case this feature exists
        // for: a key set once in `[server.env]` and inherited by four processes, where "did it
        // reach this backend" is a question the file alone cannot answer.
        let names = backend_env_names(&config.server, backend);
        if !names.is_empty() {
            println!(
                "      env:               {} (values not printed)",
                names.join(", ")
            );
        }
        // The gRPC grants, so the one setting that decides whether a call reaches a worker at all is
        // visible without reading the file. `any` is spelled out rather than shown as an empty list,
        // because the two are very different answers that would otherwise look identical here.
        if let Some(grants) = config.grpc_grants(name) {
            if grants.is_any() {
                println!("      grpc:              any service (grpc_serve_any)");
            } else if !grants.is_empty() {
                println!("      grpc:              {}", grants.names().join(", "));
                // Where the names came from, when they were not written in this file. A directory
                // is the one entry that can grant services nobody meant to grant — a vendored SDK
                // or another team's `.proto` sharing the tree — so the count is printed beside the
                // source and an operator can compare it against what they expect to implement.
                let entries = config.grpc_proto(name);
                if !entries.is_empty() {
                    println!(
                        "      grpc from:         {} service(s) from grpc_proto: {}",
                        grants.names().len(),
                        entries
                            .iter()
                            .map(|entry| entry.display().to_string())
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                }
            }
        }
    }

    // The KV storages, with a file store's *resolved* directory. The resolved value is the one that
    // matters: `path = "kv"` is relative to the configuration file, and an operator reading `check`
    // needs to see where the files will actually be before a directory appears in the wrong place.
    if !config.kv.is_empty() {
        println!("  kv:");
        for (name, storage) in &config.kv {
            match storage {
                volucer_core::config::KvStorageConfig::File(file) => {
                    let resolved = volucer_core::config::resolve_kv_path(&file.path, &config_dir);
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

    // The `Strict-Transport-Security` policy, and where it applies. The *resolved* value is printed
    // rather than the keys as written, because a site that inherits is the ordinary case and an
    // operator asking "what header do my names' clients actually get" cannot answer it from the
    // file alone — the same reason the site table prints what it inherits one layer down.
    print_hsts(&config, &sites);

    println!("  routes:");
    if config.server.routes.is_empty() {
        println!("    (none: every request goes to the default backend)");
    }
    let default_extensions = sites.default_site().script_extensions.clone();
    for route in &config.server.routes {
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
        // The name, and what the site is reachable by. An address-only site has no hosts, and
        // printing an empty list would read as a mistake rather than as the nginx `default_server`
        // shape it is — so the two halves are printed together.
        let mut reachable = Vec::new();
        if !site.hosts.is_empty() {
            reachable.push(site.hosts.join(", "));
        }
        if !site.addresses.is_empty() {
            reachable.push(format!("addresses {}", site.addresses.join(", ")));
        }
        println!("    {name}: {}", reachable.join(" @ "));
        println!("      document root:    {}", site.document_root.display());
        if let Some(protocols) = &site.protocols {
            println!(
                "      protocols:        {}",
                protocols
                    .iter()
                    .map(|protocol| protocol.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        if let Some(controller) = &site.front_controller {
            match controller.as_path() {
                Some(path) => println!("      front controller: {path}"),
                None => println!("      front controller: (none: only real .php files are served)"),
            }
        }
        if let Some(serve_only) = site.serve_only {
            println!("      serve only:       {serve_only}");
        }
        // Only a site that *states* the key is printed here, following this block's rule. The
        // resolved value for every site is printed by `print_hsts` and by the redirect line
        // below it, which is where "what will actually happen" is answered.
        if let Some(redirect) = site.redirect_to_https {
            println!("      redirect to https: {redirect}");
        }
        if let Some(hsts) = &site.hsts {
            // The **resolved** value, not this table alone: a site that states one directive
            // keeps the server's others, so printing the table as written would understate what
            // a client actually receives — and this line exists to answer exactly that.
            let resolved = sites.named().get(name).and_then(|site| site.hsts.as_ref());
            println!(
                "      hsts:             {}",
                match resolved {
                    Some(policy) => policy.header_value(),
                    None if !hsts.is_enabled() => "off".to_string(),
                    None => "(no policy: max_age_s is unset)".to_string(),
                }
            );
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
            let resolved = resolve_access_log_path(&log.path, &config_dir);
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
    // Trace propagation, printed from the configured value rather than from a constant, so an
    // operator can confirm the decision without reading the source. `extract` and `propagate` both
    // read an inbound `traceparent` and continue it; `inject` and `propagate` both replace what the
    // backend receives with this server's context. A missing or malformed inbound header starts a
    // new unsampled trace rather than failing the request, under every value.
    println!(
        "  trace context: {} (inbound {})",
        config.server.trace_context.as_str(),
        match config.server.trace_context.extracts() {
            true => "traceparent is read and continued",
            false => "traceparent is ignored",
        }
    );
    println!(
        "    backend:     {}",
        match config.server.trace_context.injects() {
            true => "receives this server's traceparent, so its spans are children of its span",
            false => "receives the client's traceparent unchanged, so its spans are siblings",
        }
    );
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
            // Printed from the configured value rather than from a constant, so an operator can
            // confirm the decision without reading the source. Sampling is the key that decides
            // *what leaves this process*, and a deployment that thinks it is tracing everything
            // while exporting a tenth of it is the mistake this line exists to prevent.
            println!(
                "    sampling:     {} — exports {}",
                telemetry.sample.as_str(),
                match (telemetry.sample, telemetry.sample_ratio) {
                    (volucer_core::config::SampleMode::Ratio, Some(ratio)) =>
                        format!("{ratio} of traces, chosen by the trace id"),
                    _ => sampler_of(telemetry).describe(),
                }
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

    // The call plane. Printed in both cases, for the reason the status block below is: a deployment
    // whose FastCGI application "cannot reach the cache" is a question about whether this listener
    // exists and what each backend was granted, and a report that printed nothing when `[rpc]` was
    // absent would leave the operator unable to tell "not configured" from "not reported".
    match &config.rpc {
        Some(rpc) => {
            println!("  rpc: {}", rpc.listen);
            println!(
                "    idle timeout:     {} s, {} s per call",
                rpc.idle_timeout_seconds, rpc.call_timeout_seconds
            );
            println!(
                "    connections:      {}",
                match rpc.max_connections {
                    Some(limit) => format!("at most {limit}"),
                    None => "uncapped".to_string(),
                }
            );
            // What each backend may reach, which is the same list its workers get. By name rather
            // than as a count, because "3 services" is not something an operator can compare with a
            // file, and the whole failure mode this line prevents is a grant that disagrees with
            // what the application tries to call.
            for (backend, services) in config.rpc_grants() {
                println!("    {backend}: {}", services.join(", "));
            }
        }
        None if config.serves_rpc() => {
            // Unreachable in a validated configuration — `Config::validate` refuses a grant with no
            // listener — but printed rather than assumed, because `check` also runs against a file
            // that failed validation, and there the line is the explanation.
            println!(
                "  rpc: none, but {} grants services (validation refuses this)",
                config
                    .rpc_grants()
                    .keys()
                    .map(|name| format!("backends.{name}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        None => println!("  rpc: none (no FastCGI application can reach the call plane)"),
    }

    // The status listener and the routes on it that mutate. Printed in both cases rather than only
    // when configured: the *absence* of a mutation surface is the fact an operator auditing this
    // deployment wants, and a report that printed nothing when `workers.reset` was ungranted would
    // leave them unable to tell "not configured" from "not reported".
    match &config.status {
        Some(status) => {
            println!("  status:           {}", status.listen);
            let has = |method: &str| {
                status
                    .workers
                    .as_ref()
                    .is_some_and(|workers| workers.allow.iter().any(|m| m == method))
            };
            let routes = match (has("workers.list"), has("workers.reset")) {
                (true, true) => {
                    "GET /workers and POST /workers/{index}/reset (no authentication)".to_string()
                }
                (true, false) => {
                    "GET /workers only (no authentication; a reset is not granted)".to_string()
                }
                // Worth its own line: the list is how a caller learns an index, so a grant of the
                // reset alone is a route whose documentation cannot be followed.
                (false, true) => {
                    "POST /workers/{index}/reset only (no authentication, and the list that names an \
                     index is not served)"
                        .to_string()
                }
                (false, false) => "none ([status.workers] is absent, so the routes do not exist)"
                    .to_string(),
            };
            println!("    worker routes:    {routes}");
            println!(
                "    metrics:          {}",
                match &status.metrics {
                    Some(metrics) =>
                        format!("{} on this listener at {}", metrics.path, status.listen),
                    None => "disabled ([status.metrics] is absent)".to_string(),
                }
            );
        }
        None => {
            println!("  status:           (disabled: no /health, no /ready, no /metrics, and no worker routes)")
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
    let warnings = volucer_core::preflight::inspect_with_front_controller(
        &config,
        &document_root,
        source.path.as_deref(),
        front_controller_flag,
    );
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

/// Ask an already-running FastCGI server what it says about its own capacity.
///
/// `FCGI_GET_VALUES` is the protocol's own channel for this, and the two PHP servers behave
/// differently enough that the answer is worth printing rather than assuming — measured:
/// a preforking `php-cgi` answers `FCGI_MAX_CONNS=4` for `PHP_FCGI_CHILDREN=4`, and `php-fpm`
/// answers with an empty result even though `fpm_php.c` fills the same variables in.
///
/// **Only called from `check`.** It is a round trip to a socket the operator named, with a
/// short timeout, made once while a human waits. `serve` never calls it: php-fpm only answers
/// after a child has been forked to serve the connection (1.5 s after start on the bench host),
/// so on the accept path this would trade a reliable queue for an unreliable gauge. The two
/// numbers are reported, never acted on.
/// The variable names a backend's processes receive: `[server.env]` merged with its own `env`.
///
/// Names only, deliberately. The values are the half that must not be printed — an operator pastes
/// `check` output into a ticket — and the names are the half that answers the question the file
/// cannot: *which* processes inherited the baseline. Sorted, because a `BTreeMap` already is and
/// because a stable order is what makes two runs diffable.
fn backend_env_names(
    server: &volucer_core::config::ServerConfig,
    backend: &volucer_core::config::BackendConfig,
) -> Vec<String> {
    let own = match backend {
        volucer_core::config::BackendConfig::Worker(worker) => &worker.env,
        volucer_core::config::BackendConfig::SpawnCgi(cgi) => &cgi.env,
        volucer_core::config::BackendConfig::Upstream(upstream) => &upstream.env,
        // Neither starts a process here, so neither receives the baseline — see
        // `ServerConfig::env`.
        volucer_core::config::BackendConfig::Attach(_)
        | volucer_core::config::BackendConfig::External(_) => return Vec::new(),
    };
    // Owned rather than borrowed: the merge produces a new map, so a `&str` into it could not
    // outlive this call.
    server.env_for(own).into_keys().collect()
}

fn print_advertised_capacity(external: &volucer_core::config::ExternalConfig) {
    use std::time::Duration;

    let Ok(endpoint) = volucer_fcgi::Endpoint::parse(&external.socket) else {
        return;
    };
    let mut client = match volucer_fcgi::Client::connect(&endpoint, Some(Duration::from_secs(2))) {
        Ok(client) => client,
        // Not a warning: `preflight` already reports an unreachable socket, and `check` is not
        // the place to say it twice in two different ways.
        Err(_) => return,
    };

    let mut reported = Vec::new();
    for name in [&b"FCGI_MAX_CONNS"[..], &b"FCGI_MAX_REQS"[..]] {
        match client.get_values([name]) {
            Ok(values) => {
                for (key, value) in values {
                    if !value.is_empty() {
                        reported.push(format!(
                            "{}={}",
                            String::from_utf8_lossy(&key),
                            String::from_utf8_lossy(&value)
                        ));
                    }
                }
            }
            Err(_) => continue,
        }
    }

    if reported.is_empty() {
        println!(
            "      advertised:        nothing (the server answers GET_VALUES with an empty result)"
        );
    } else {
        println!("      advertised:        {}", reported.join(", "));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use volucer_core::config::BackendConfig;

    /// An override is typed by TOML, and a listener that is not TOML is still a string.
    ///
    /// The pair this pins is the one that broke: `str::parse::<toml::Value>` reads a whole
    /// *document*, so `8` and `127.0.0.1:8080` both failed there and every override became a string
    /// — which surfaced as `http: "\"127.0.0.1:8080\""` in the error, a quoted listener, and as
    /// `workers = "8"` failing the type check. Both halves are asserted because a fix for the first
    /// that broke the second would be the same bug with the sign flipped.
    #[test]
    fn an_override_is_typed_like_the_file_it_lands_in() {
        assert_eq!(literal("8"), toml::Value::Integer(8));
        assert_eq!(literal("true"), toml::Value::Boolean(true));
        assert_eq!(literal("0.5"), toml::Value::Float(0.5));
        assert_eq!(literal("\"quoted\""), toml::Value::String("quoted".into()));
        assert_eq!(
            literal("127.0.0.1:8080"),
            toml::Value::String("127.0.0.1:8080".into()),
            "a listener is not TOML, so it is the string it looks like"
        );
        assert_eq!(
            literal("unix:/run/app.sock"),
            toml::Value::String("unix:/run/app.sock".into())
        );
    }

    /// A dotted pair builds the tables it needs, and a pair that contradicts an earlier one is
    /// refused rather than silently ordered.
    ///
    /// The refusal matters because both forms are *writable*: `-o server=1 -o server.http=…` would
    /// otherwise choose by argument order, which is the kind of precedence an operator cannot see in
    /// the file they are reasoning about.
    #[test]
    fn an_override_builds_tables_and_refuses_to_replace_a_value() {
        let mut table = toml::Table::new();
        apply_override(&mut table, "backends.app.kind=worker").expect("a nested key is created");
        apply_override(&mut table, "backends.app.command=\"php worker.php\"")
            .expect("a quoted value is a string");
        apply_override(&mut table, "backends.app.workers=3").expect("a sibling is added");

        let config = Config::from_table(table.clone()).expect("the table deserializes");
        match config.backends.get("app").expect("the backend exists") {
            volucer_core::BackendConfig::Worker(worker) => {
                assert_eq!(
                    worker.workers, 3,
                    "`workers=3` must be a number, not a string"
                );
                assert_eq!(worker.command, "php worker.php");
            }
            other => panic!("`kind = worker` must build a worker: {other:?}"),
        }

        let mut table = toml::Table::new();
        apply_override(&mut table, "server=1").expect("a scalar is a valid override");
        let err = apply_override(&mut table, "server.http=127.0.0.1:8080")
            .expect_err("a value cannot be walked through");
        assert!(
            err.to_string().contains("server"),
            "the refusal must name the key it could not pass through: {err}"
        );
    }

    /// A bracketed index reaches into an array of tables, which is what a dotted key cannot do.
    ///
    /// The case that motivated it: `-o 'server.routes=[…]'` replaces the whole array, so amending
    /// one route of twenty had no spelling. Every assertion is on the *table*, before
    /// deserialization, because the grammar is the part being pinned — and the deserialization at
    /// the end is what proves the segments landed where a file's `[[server.routes]]` would have.
    #[test]
    fn an_index_reaches_into_an_array_of_tables() {
        let mut table = toml::Table::new();
        apply_override(&mut table, "server.http=127.0.0.1:8080")
            .expect("a listener is not an array");
        apply_override(&mut table, r#"server.routes[0].path_prefix="/api""#)
            .expect("the empty array it has to create is its own");
        apply_override(&mut table, r#"server.routes[0].backend="app""#).expect("a sibling");
        apply_override(&mut table, r#"server.routes[1].path_prefix="/admin""#)
            .expect("one past the end appends");

        let config = Config::from_table(table.clone()).expect("the table deserializes");
        assert_eq!(config.server.http.as_deref(), Some("127.0.0.1:8080"));
        assert_eq!(config.server.routes.len(), 2, "one appended, one amended");
        assert_eq!(config.server.routes[0].path_prefix.as_deref(), Some("/api"));
        assert_eq!(
            config.server.routes[1].path_prefix.as_deref(),
            Some("/admin")
        );
        assert_eq!(
            config.server.routes[0].backend.as_deref(),
            Some("app"),
            "the second pair must amend element 0, not replace it"
        );

        // `[1]` on a fresh table is a gap, and a gap is refused: an element invented in the middle
        // would be a route nobody wrote.
        let mut table = toml::Table::new();
        let err = apply_override(&mut table, "server.routes[1].backend=app")
            .expect_err("an index past the end plus one is not an append");
        assert!(err.to_string().contains("gap"), "{err}");

        // And a table cannot be indexed, nor an array walked without one. Both are refused by the
        // schema rather than here — this function is deliberately schema-blind, so it builds what
        // it is told and the one strict deserializer says the type is wrong.
        let mut table = toml::Table::new();
        apply_override(&mut table, "server[0].http=127.0.0.1:8080").expect("the grammar allows it");
        assert!(
            Config::from_table(table).is_err(),
            "`server` is a table, so an index into it must fail validation"
        );
        let mut table = toml::Table::new();
        apply_override(&mut table, r#"server.routes={path_prefix="/api"}"#).expect("a table value");
        assert!(
            Config::from_table(table).is_err(),
            "`routes` is a sequence, so a table of one must fail validation"
        );
    }

    /// A quoted segment is how a map key that holds a dot is written, and it is the one place a
    /// bracket is a literal character rather than an index.
    #[test]
    fn a_quoted_segment_is_literal() {
        let mut table = toml::Table::new();
        apply_override(&mut table, r#"server.env."APP.NAME"=x"#).expect("a quoted key");
        apply_override(&mut table, r#"server.env."routes[0]"=literal"#)
            .expect("a bracket in a quoted key is a character");
        apply_override(&mut table, r#"server.env.APP_ENV=prod"#).expect("a plain key still works");

        let env = table["server"]["env"].as_table().expect("env is a table");
        assert_eq!(env["APP.NAME"].as_str(), Some("x"));
        assert_eq!(env["routes[0]"].as_str(), Some("literal"));
        assert_eq!(env["APP_ENV"].as_str(), Some("prod"));
        assert!(
            table["server"].get("routes").is_none(),
            "a quoted bracket must not invent an array"
        );
    }

    /// The key grammar says which spellings it takes and names the one that is wrong.
    ///
    /// `[01]` and a trailing dot are here so that the accepted set is stated rather than assumed:
    /// leading zeros are just a number, and a dot promises a segment that is not there.
    #[test]
    fn a_key_segment_is_a_name_or_an_index() {
        let cases: &[(&str, &[KeySegment])] = &[
            (
                "server.http",
                &[
                    KeySegment::Key("server".into()),
                    KeySegment::Key("http".into()),
                ],
            ),
            (
                "server.routes[02].backend",
                &[
                    KeySegment::Key("server".into()),
                    KeySegment::Key("routes".into()),
                    KeySegment::Index(2),
                    KeySegment::Key("backend".into()),
                ],
            ),
            (
                r#"server.env."a.b""#,
                &[
                    KeySegment::Key("server".into()),
                    KeySegment::Key("env".into()),
                    KeySegment::Key("a.b".into()),
                ],
            ),
            (
                r#"server.env."a\"b""#,
                &[
                    KeySegment::Key("server".into()),
                    KeySegment::Key("env".into()),
                    KeySegment::Key(r#"a"b"#.into()),
                ],
            ),
        ];
        for (key, expected) in cases {
            let segments = key_segments(key).unwrap_or_else(|err| panic!("key {key:?}: {err}"));
            assert_eq!(segments, *expected, "key {key:?}");
        }

        for key in [
            "server..http",
            "server.",
            ".server",
            "server.http]",
            "server.routes[]",
            "server.routes[-1]",
            "server.routes[a]",
            r#"server.env.a"b""#,
            r#"server.env."unterminated"#,
        ] {
            assert!(key_segments(key).is_err(), "key {key:?} must be refused");
        }
    }

    /// A value that opens like TOML and did not parse is refused as a *syntax* error, not silently
    /// read as a string.
    ///
    /// `['a' => 'b']` is the PHP spelling and `[{"a": "b"}]` the JSON one; both would otherwise
    /// become a string and fail later with "invalid type: string" — a message that names neither
    /// the mistake nor the fix. A quoted value is still a string, so `-o 'x="[time]"'` keeps
    /// working.
    #[test]
    fn a_structured_value_that_is_not_toml_names_the_syntax_it_expected() {
        let mut table = toml::Table::new();
        for pair in [
            r#"server.routes=['a' => 'b']"#,
            r#"server.routes=[{"a": "b"}]"#,
            "server.routes=[{path_prefix=/api}]",
            "server.routes={",
        ] {
            let err = apply_override(&mut table, pair).expect_err("PHP and JSON are not read");
            let message = err.to_string();
            assert!(message.contains("TOML"), "{pair}: {message}");
        }

        let mut table = toml::Table::new();
        apply_override(&mut table, r#"server.env.LITERAL="[time]""#).expect("quoted is a string");
        assert_eq!(
            table["server"]["env"]["LITERAL"].as_str(),
            Some("[time]"),
            "a quoted value must reach the table as text, not be refused as malformed TOML"
        );
    }

    /// A pair that is not `KEY=VALUE`, or names no key, is an error rather than a no-op.
    #[test]
    fn a_malformed_override_names_what_is_wrong() {
        let mut table = toml::Table::new();
        for pair in ["server.http", "=127.0.0.1:8080", "server..http=8080"] {
            let err = apply_override(&mut table, pair).expect_err("a malformed pair is refused");
            assert!(
                err.to_string().contains("override"),
                "the message must say it is an override that is wrong: {err}"
            );
        }
    }

    /// `--listen` becomes a quoted `server.http` pair, placed before every `-o`.
    ///
    /// Asserted on the assembled pairs rather than on a running server: the precedence rule is one
    /// ordering decision, and a test that started a process to see it would fail for a port reason
    /// and say nothing about the order.
    #[test]
    fn the_listen_sugar_precedes_the_overrides() {
        let source = Source::resolve(
            None,
            Some("127.0.0.1:8080".to_string()),
            Some("unix:/run/app.sock".to_string()),
            vec!["server.http=127.0.0.1:9090".to_string()],
        );

        assert_eq!(
            source.pairs,
            vec![
                "server.http=\"127.0.0.1:8080\"".to_string(),
                "server.fastcgi=\"unix:/run/app.sock\"".to_string(),
                "server.http=127.0.0.1:9090".to_string(),
            ],
            "the sugar is applied first, so an explicit -o wins"
        );
        // The last pair is the one a table keeps, which is the whole of the precedence rule.
        let mut table = toml::Table::new();
        for pair in &source.pairs {
            apply_override(&mut table, pair).expect("each pair applies");
        }
        let config = Config::from_table(table).expect("the table deserializes");
        assert_eq!(config.server.http.as_deref(), Some("127.0.0.1:9090"));
        assert_eq!(
            config.server.fastcgi.as_deref(),
            Some("unix:/run/app.sock"),
            "the fastcgi sugar is untouched by the http override"
        );
    }

    /// The quick-start group is three statements plus a count, written before every `-o`.
    ///
    /// Asserted on the assembled pairs, like the `--listen` test: the ordering *is* the precedence
    /// rule, and a process-based test would fail for a port reason and say nothing about it.
    #[test]
    fn the_php_cgi_sugar_names_a_default_backend_and_its_size() {
        let source = Source::resolve_with_backends(
            None,
            Some("127.0.0.1:8080".to_string()),
            None,
            vec!["backends.cgi.workers=9".to_string()],
            Backends::PhpCgi("C:/php-8.3.31/php-cgi.exe".to_string()),
            Sizing {
                workers: Some(5),
                concurrency: None,
            },
        );

        assert_eq!(
            source.pairs,
            vec![
                "server.http=\"127.0.0.1:8080\"".to_string(),
                "server.default_backend=\"cgi\"".to_string(),
                "backends.cgi.kind=\"spawn_cgi\"".to_string(),
                "backends.cgi.binary=\"C:/php-8.3.31/php-cgi.exe\"".to_string(),
                "backends.cgi.workers=5".to_string(),
                "backends.cgi.workers=9".to_string(),
            ],
            "the sugar precedes the overrides, so an explicit -o wins"
        );
    }

    /// The whole point of the short command: the pairs alone describe a deployment `serve` will
    /// accept, with no file and no further flags.
    #[test]
    fn the_php_cgi_sugar_alone_is_a_valid_configuration() {
        let source = Source::resolve_with_backends(
            None,
            Some("127.0.0.1:8080".to_string()),
            None,
            Vec::new(),
            Backends::PhpCgi("php-cgi".to_string()),
            Sizing {
                workers: Some(5),
                concurrency: None,
            },
        );

        let mut table = toml::Table::new();
        for pair in &source.pairs {
            apply_override(&mut table, pair).expect("each pair applies");
        }
        let config = Config::from_table(table).expect("the table deserializes");
        config.validate().expect("and validates");

        assert_eq!(config.server.default_backend.as_deref(), Some("cgi"));
        match config.backends.get("cgi").expect("the backend exists") {
            BackendConfig::SpawnCgi(cgi) => {
                assert_eq!(cgi.binary.as_deref(), Some(Path::new("php-cgi")));
                assert_eq!(cgi.workers, 5);
                assert!(cgi.command.is_none(), "binary and command are exclusive");
            }
            other => panic!("expected a spawn_cgi backend, got {other:?}"),
        }
    }

    /// Worker mode is the same three statements, with `worker` where `spawn_cgi` was.
    #[test]
    fn the_worker_sugar_produces_a_worker_backend_with_its_command() {
        let source = Source::resolve_with_backends(
            None,
            Some("127.0.0.1:8080".to_string()),
            None,
            Vec::new(),
            Backends::Worker("php worker.php".to_string()),
            Sizing {
                workers: Some(5),
                concurrency: None,
            },
        );

        let mut table = toml::Table::new();
        for pair in &source.pairs {
            apply_override(&mut table, pair).expect("each pair applies");
        }
        let config = Config::from_table(table).expect("the table deserializes");
        config.validate().expect("and validates");

        assert_eq!(config.server.default_backend.as_deref(), Some("app"));
        match config.backends.get("app").expect("the backend exists") {
            BackendConfig::Worker(worker) => {
                assert_eq!(worker.command, "php worker.php");
                assert_eq!(worker.workers, 5);
            }
            other => panic!("expected a worker backend, got {other:?}"),
        }
    }

    /// The non-PHP FastCGI flag: a `command` slot with `{address}`, and one slot by default —
    /// plus the front controller it has to clear, because a WSGI program is not `index.php`.
    #[test]
    fn the_fcgi_sugar_sets_a_command_slot_and_clears_the_front_controller() {
        let source = Source::resolve_with_backends(
            None,
            Some("127.0.0.1:8080".to_string()),
            None,
            Vec::new(),
            Backends::Fcgi("python fcgi_app.py --bind {address}".to_string()),
            Sizing::default(),
        );

        let mut table = toml::Table::new();
        for pair in &source.pairs {
            apply_override(&mut table, pair).expect("each pair applies");
        }
        let config = Config::from_table(table).expect("the table deserializes");
        config.validate().expect("and validates");

        assert_eq!(config.server.default_backend.as_deref(), Some("app"));
        match config.backends.get("app").expect("the backend exists") {
            BackendConfig::SpawnCgi(cgi) => {
                assert_eq!(
                    cgi.command.as_deref(),
                    Some("python fcgi_app.py --bind {address}")
                );
                assert!(cgi.binary.is_none(), "binary and command are exclusive");
                assert_eq!(cgi.workers, 1, "one slot until concurrency is widened");
            }
            other => panic!("expected a spawn_cgi backend, got {other:?}"),
        }
        // The empty string is the documented spelling of "no front controller", and it is what
        // keeps a WSGI deployment from resolving every unknown path to a missing `index.php`.
        // It cannot live in the table — `Config::validate` refuses that — so it is supplied as a
        // flag, which is the same path an explicit `--front-controller` takes.
        assert_eq!(
            implied_front_controller(&Backends::Fcgi(
                "python fcgi_app.py --bind {address}".to_string()
            )),
            Some("")
        );
        assert_eq!(
            implied_front_controller(&Backends::PhpCgi("php-cgi".to_string())),
            None,
            "a PHP deployment keeps index.php"
        );
        assert_eq!(implied_front_controller(&Backends::None), None);
        assert_eq!(
            config.server.resolve_front_controller(None).as_deref(),
            Some("index.php")
        );
    }

    /// `--concurrency` widens a `--fcgi` slot without changing how many slots there are.
    ///
    /// The two are different numbers, and that is the whole reason the flag exists: `--workers` is
    /// slots, `--concurrency` is requests held open to each one. A FastCGI server that forks four
    /// children behind one slot needs `concurrency = 4` for the pool to reach all four.
    #[test]
    fn the_concurrency_flag_widens_an_fcgi_slot() {
        let source = Source::resolve_with_backends(
            None,
            Some("127.0.0.1:8080".to_string()),
            None,
            Vec::new(),
            Backends::Fcgi("python fcgi_app.py --bind {address}".to_string()),
            Sizing {
                workers: Some(2),
                concurrency: Some(4),
            },
        );

        let mut table = toml::Table::new();
        for pair in &source.pairs {
            apply_override(&mut table, pair).expect("each pair applies");
        }
        let config = Config::from_table(table).expect("the table deserializes");
        config.validate().expect("and validates");

        match config.backends.get("app").expect("the backend exists") {
            BackendConfig::SpawnCgi(cgi) => {
                assert_eq!(cgi.workers, 2, "two slots");
                assert_eq!(cgi.concurrency, 4, "four requests each");
            }
            other => panic!("expected a spawn_cgi backend, got {other:?}"),
        }
    }

    /// `--concurrency` describes a `spawn_cgi` slot, so it is refused for the other two backends.
    ///
    /// A `worker` answers one request at a time by construction and `php-cgi` spells the same
    /// number `children`, so accepting the flag there would either do nothing or mean something
    /// else — and `backends.cgi.concurrency` beside `binary` is a configuration error anyway. The
    /// refusal is ours rather than clap's, because `requires` is satisfied by *presence* and
    /// `--php-cgi`'s optional value counts as present.
    #[test]
    fn concurrency_without_fcgi_is_refused() {
        for (backend, argv) in [
            (
                Backends::PhpCgi("php-cgi".to_string()),
                vec!["volucer", "serve", "--php-cgi", "--concurrency", "4"],
            ),
            (
                Backends::Worker("php worker.php".to_string()),
                vec![
                    "volucer",
                    "serve",
                    "--worker",
                    "php worker.php",
                    "--concurrency",
                    "4",
                ],
            ),
            (
                Backends::None,
                vec!["volucer", "serve", "--concurrency", "4"],
            ),
        ] {
            // Parsed, so the flag itself is accepted — the refusal is what the value *means*,
            // which only the backend choice can answer.
            let cli = Cli::try_parse_from(&argv).expect("--concurrency parses on its own");
            let Command::Serve {
                workers,
                concurrency,
                ..
            } = cli.command
            else {
                panic!("expected serve for {argv:?}")
            };
            let err = Sizing::from_flags(&backend, workers, concurrency)
                .expect_err("concurrency without --fcgi is refused");
            assert!(err.to_string().contains("--concurrency"), "{err}");
            assert!(err.to_string().contains("--fcgi"), "{err}");
        }

        // And with `--fcgi` it is accepted.
        assert!(Sizing::from_flags(
            &Backends::Fcgi("python fcgi_app.py --bind {address}".to_string()),
            None,
            Some(4)
        )
        .is_ok());
    }

    /// The three backend flags are one choice, and clap refuses two at once rather than letting
    /// one silently win over another.
    #[test]
    fn two_backend_flags_are_a_usage_error() {
        for pair in [
            vec!["--php-cgi", "--worker", "php worker.php"],
            vec!["--php-cgi", "--fcgi", "flup-fcgi {address}"],
            vec![
                "--worker",
                "php worker.php",
                "--fcgi",
                "flup-fcgi {address}",
            ],
        ] {
            let mut argv = vec!["volucer", "serve", "--listen", "127.0.0.1:8080"];
            argv.extend(pair.iter().copied());
            let error = Cli::try_parse_from(&argv)
                .expect_err("clap refuses two backends in one deployment");
            assert_eq!(
                error.kind(),
                clap::error::ErrorKind::ArgumentConflict,
                "{pair:?}"
            );
        }
    }

    /// `--workers` without any backend flag has nothing to size, and clap refuses it rather than
    /// writing a pair for a backend nobody asked for.
    #[test]
    fn workers_without_a_backend_is_a_usage_error() {
        let error = Cli::try_parse_from([
            "volucer",
            "serve",
            "--listen",
            "127.0.0.1:8080",
            "--workers",
            "5",
        ])
        .expect_err("clap refuses a dependent flag on its own");
        assert_eq!(
            error.kind(),
            clap::error::ErrorKind::MissingRequiredArgument
        );
    }

    /// A bare `--php-cgi` means the name `php-cgi`, which the resolver then looks up; a value
    /// means exactly that value. Both are the same flag, which is why `num_args = 0..=1` is
    /// load-bearing.
    #[test]
    fn a_bare_php_cgi_flag_names_php_cgi_and_a_value_overrides_it() {
        for (argv, expected) in [
            (vec!["volucer", "serve", "--php-cgi"], "php-cgi"),
            (
                vec!["volucer", "serve", "--php-cgi=C:/php-8.3.31/php-cgi.exe"],
                "C:/php-8.3.31/php-cgi.exe",
            ),
        ] {
            let cli = Cli::try_parse_from(argv.clone()).expect("parses");
            match cli.command {
                Command::Serve { php_cgi, .. } => {
                    assert_eq!(php_cgi.as_deref(), Some(expected), "{argv:?}")
                }
                other => panic!("expected serve, got {other:?}"),
            }
        }
    }

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
            binding: volucer_core::sites::SiteBinding::Any,
            protocols: volucer_core::sites::SiteProtocols::default(),
            redirect_to_https: false,
            hsts: None,
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
            binding: volucer_core::sites::SiteBinding::Any,
            protocols: volucer_core::sites::SiteProtocols::default(),
            redirect_to_https: false,
            hsts: None,
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
            "server.env",
            "min_version",
            "server.proxy",
            "server.access_log",
            "status] listen",
        ] {
            assert!(
                joined.contains(required),
                "the restart-only list must name {required:?}, or an operator who edited it gets \
                 silence: {joined}"
            );
        }

        // The ones an earlier draft got wrong, in the code's favour. Each is now read per request
        // out of swapped state — `routes` and `static_files` from the site snapshot,
        // `server.rate_limit` from the shared `RateLimitCell`, the worker grant from the
        // `WorkerResetGate`, and a backend's `grpc_services` from `Reloaded::grpc_grants` — so
        // telling an operator they are restart-only is telling them to restart for nothing.
        for wrong in [
            "routes",
            "statics",
            "static_files",
            "server.rate_limit",
            "[status.workers]",
            "grpc_services",
        ] {
            assert!(
                !joined.contains(wrong),
                "{wrong:?} is read per request out of swapped state and is therefore \
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
