//! Supervising the process behind an `upstream` origin.
//!
//! Everything else in this tree either *is* the server or is reached by it. An `upstream` backend
//! is the one kind that reaches somebody else's process — and, since the `upstream` backend was
//! given the ability to *start* it,
//! the one kind that can be asked to.
//!
//! Why this exists rather than a `kind = "python"` backend: every Python and Ruby application
//! server already implements "boot once, serve many" and already speaks HTTP over a unix socket
//! (`gunicorn --preload --bind unix:…`, Puma's `preload_app!`, uvicorn, granian). volucer already
//! reaches one over `unix:`. What was missing was not a protocol, it was **supervision**: starting
//! the process, waiting until it answers, restarting it if it dies, and making sure it does not
//! outlive us when we do.
//!
//! Four things this has to get right, each of which is a way to be quietly wrong:
//!
//! 1. **A missing binary is a startup failure, not a 502 later.** The same rule `CgiPool::start`
//!    and `Backend::from_config` already follow: a typo in `command` is reported where the file is
//!    read, naming the key, rather than becoming an origin that never answers.
//! 2. **The first request must not race the boot.** A `gunicorn` that has just been spawned is not
//!    listening yet; it imports, it opens its database connection. `ready_timeout_ms` is the gate,
//!    and without it a deploy produces a burst of 502s from an origin that is merely still starting.
//! 3. **A command that cannot start must not become a busy loop.** An origin whose command exits in
//!    milliseconds — a missing virtualenv, a typo in a module name — plus an unbounded restarter is
//!    a core burned and a log filled while the backend answers nothing. Hence `restart_limit` and a
//!    doubling backoff, both of which `Config::validate` refuses to see set to a value that would
//!    spin.
//! 4. **An orphan is worse than a crash.** The process is adopted into the same job object / process
//!    group the CGI pool uses, and every path out of `start` that does not return a `Supervisor`
//!    kills the child it started — including the readiness timeout, where the process *is* running
//!    and would otherwise hold the socket for ever. On Unix the lifecycle guard is a no-op (the
//!    process group only helps when *we* are signalled), so relying on it there would be relying on
//!    nothing.
//!
//! The watcher polls `try_wait` on a short interval rather than blocking in `wait`, and that is
//! deliberate: `Child::wait` borrows the child exclusively for as long as the process lives, which
//! is exactly as long as shutdown needs it in order to kill it. Polling leaves shutdown a lock it
//! can actually acquire.

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use volucer_core::config::{
    OriginSpec, UpstreamConfig, UpstreamRestart, UPSTREAM_RESTART_BACKOFF_CAP_MS,
};
use volucer_core::program::{resolve_program, split_command};
use volucer_fcgi::Endpoint;

use crate::error::{Hint, Result, ServerError};
use crate::lifecycle::{configure_command, LifecycleGuard};

/// How often the watcher looks at the child.
///
/// Short enough that a restart is not noticeably late, long enough that an idle supervisor costs
/// nothing measurable. See the module doc for why this is not a blocking `wait`.
const WATCH_INTERVAL: Duration = Duration::from_millis(200);

/// How long a single readiness probe waits before it is retried.
const PROBE_TIMEOUT: Duration = Duration::from_millis(250);

/// How long a run has to last to count as *stable*.
///
/// `restart_limit` is a limit on **consecutive** failures, and without this it would be a limit on
/// a service's *total lifetime* restarts: an origin that crashes once a week would be given up on
/// after three weeks of otherwise perfect service. A process that stayed up at least this long has
/// proved it can run, so a crash after it is the first of a new streak.
///
/// A minute rather than a few seconds, because the failure this guards against — a bad command
/// line — is detectable in milliseconds, while a genuine intermittent crash usually comes with
/// minutes of service between occurrences. Public because the test that verifies the reset cannot
/// wait a minute for it.
pub const STABLE_RUN: Duration = Duration::from_secs(60);

/// What to run, resolved once.
///
/// Built before anything is spawned, so a command that cannot be split or a program that does not
/// exist is a startup failure rather than a partially started supervisor.
#[derive(Debug)]
struct Spec {
    binary: PathBuf,
    args: Vec<String>,
    working_dir: Option<PathBuf>,
    env: std::collections::BTreeMap<String, String>,
}

/// How the child is restarted.
#[derive(Debug, Clone, Copy)]
struct Policy {
    restart: UpstreamRestart,
    limit: u32,
    backoff_ms: u64,
    ready_timeout: Duration,
}

impl Policy {
    /// Whether an exit with this status should be followed by another start.
    fn should_restart(&self, success: bool) -> bool {
        match self.restart {
            UpstreamRestart::Never => false,
            UpstreamRestart::Always => true,
            // A clean exit is somebody having asked it to stop — `SIGTERM` from a deploy, a manual
            // kill. Starting it again would fight the operator.
            UpstreamRestart::OnFailure => !success,
        }
    }

    /// The delay before the *n*-th consecutive restart.
    ///
    /// Doubling from `backoff_ms`, capped. The cap is what keeps a genuinely broken origin from
    /// becoming an outage nobody connects to a two-minute delay.
    fn backoff(&self, attempt: u32) -> Duration {
        let shift = attempt.saturating_sub(1).min(16);
        let millis = self
            .backoff_ms
            .saturating_mul(1u64 << shift)
            .min(UPSTREAM_RESTART_BACKOFF_CAP_MS);
        Duration::from_millis(millis)
    }

    /// Whether a run that lasted this long counts as stable.
    fn is_stable(&self, lived: Duration) -> bool {
        lived >= STABLE_RUN
    }
}

/// The live child and when it was started.
///
/// One lock over both, so the two cannot disagree: the watcher asks "did this exit, and had it run
/// long enough to count as stable", and a pair of independent mutexes would let a restart interleave
/// between the two reads and answer about two different processes.
#[derive(Debug)]
struct Process {
    child: Child,
    started_at: Instant,
}

/// The state the watcher and the owning `Supervisor` share.
#[derive(Debug)]
struct Shared {
    process: Mutex<Process>,
    spec: Spec,
    policy: Policy,
    /// Set before the child is killed, so the watcher does not resurrect a deliberate stop.
    stopping: AtomicBool,
    name: String,
}

impl Shared {
    /// Start the configured program, with its output drained.
    fn spawn(&self) -> Result<Child> {
        let mut command = Command::new(&self.spec.binary);
        command.args(&self.spec.args);
        if let Some(dir) = &self.spec.working_dir {
            command.current_dir(dir);
        }
        for (key, value) in &self.spec.env {
            command.env(key, value);
        }
        // Last, so a configured `env` cannot shadow it — see `VERSION_ENV`.
        command.env(volucer_core::config::VERSION_ENV, crate::build::VERSION);
        // Both pipes drained rather than nulled, the same courtesy the worker pool extends to PHP
        // and for the same reason: an application's own diagnostics are the only thing that
        // explains a 502 it caused, and a process whose pipe fills up blocks for ever on its next
        // write. `gunicorn` puts its access log on stdout and its tracebacks on stderr, so dropping
        // either would drop the half of the story that matters.
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // The process group / job object membership that makes it die with us.
        configure_command(&mut command);

        let mut child = command.spawn().map_err(|source| ServerError::Spawn {
            binary: self.spec.binary.clone(),
            source,
        })?;

        if let Some(stdout) = child.stdout.take() {
            drain(self.name.clone(), child.id(), stdout, "origin stdout");
        }
        if let Some(stderr) = child.stderr.take() {
            drain(self.name.clone(), child.id(), stderr, "origin stderr");
        }

        Ok(child)
    }

    /// Kill the child and reap it, so nothing is left running or unreaped.
    fn kill(&self) {
        let mut process = self.process.lock().unwrap_or_else(|p| p.into_inner());
        // The group, for the reason every other kill site does it: an origin started through a
        // shell wrapper is a tree, and the process we spawned is only its root.
        let _ = crate::lifecycle::kill(&mut process.child);
        let _ = process.child.wait();
    }
}

/// A supervised `upstream` origin.
#[derive(Debug)]
pub struct Supervisor {
    shared: Arc<Shared>,
    /// Held for the life of the supervisor; dropping it reaps the child even if nothing else ran.
    _guard: LifecycleGuard,
    watcher: Option<std::thread::JoinHandle<()>>,
}

impl Supervisor {
    /// Start the configured origin and wait until it answers.
    ///
    /// The readiness gate is the last thing here, so a failure to start is reported as itself
    /// rather than as a timeout: a command that exits immediately is a `StartupExited` naming the
    /// binary, not a thirty-second wait ending in `StartupTimeout`.
    pub fn start(name: &str, config: &UpstreamConfig, origin: &OriginSpec) -> Result<Self> {
        let Some(command) = config.command.as_deref() else {
            // Not reachable from the dispatcher, which builds a supervisor only when a command is
            // configured — but a supervisor with nothing to supervise would be a silent no-op, and
            // naming the reason costs less than a panic nobody expects.
            return Err(ServerError::Misconfigured {
                backend: name.to_string(),
                reason: "no `command` is configured, so there is no origin process to supervise"
                    .to_string(),
            });
        };
        let (program, args) =
            split_command(command).map_err(|reason| ServerError::Misconfigured {
                backend: name.to_string(),
                reason,
            })?;
        let binary = resolve_program(&program);

        // Reported by name, before anything is spawned: the same rule `CgiPool::start` follows for
        // `php-cgi`, and the reason a mistyped `command` is a startup failure rather than a pool of
        // 502s.
        if !binary.is_file() {
            return Err(ServerError::Misconfigured {
                backend: name.to_string(),
                reason: format!(
                    "the origin command {} does not exist (from command {:?})",
                    binary.display(),
                    command
                ),
            });
        }
        if let Some(dir) = &config.working_dir {
            if !dir.is_dir() {
                return Err(ServerError::Misconfigured {
                    backend: name.to_string(),
                    reason: format!("working_dir {} is not a directory", dir.display()),
                });
            }
        }

        let guard = LifecycleGuard::new(&format!("volucer-{name}")).map_err(|source| {
            ServerError::Misconfigured {
                backend: name.to_string(),
                reason: format!("could not create the process lifecycle guard: {source}"),
            }
        })?;

        let shared = Arc::new(Shared {
            // A placeholder replaced below, before anything can observe it: the child has to exist
            // before it can be adopted into the guard, and `Shared` is what spawns it. The
            // alternative is a `Mutex<Option<Child>>` and an unwrap at every reader.
            process: Mutex::new(Process {
                child: no_child()?,
                started_at: Instant::now(),
            }),
            spec: Spec {
                binary,
                args,
                working_dir: config.working_dir.clone(),
                env: config.env.clone(),
            },
            policy: Policy {
                restart: config.restart_policy(),
                limit: config.restart_limit,
                backoff_ms: config.restart_backoff_ms,
                ready_timeout: Duration::from_millis(config.ready_timeout_ms),
            },
            stopping: AtomicBool::new(false),
            name: name.to_string(),
        });

        let mut child = shared.spawn()?;
        if let Err(source) = guard.adopt(&child) {
            // Not adopted, so nothing else will reap it. A process that already exited is not a
            // lifecycle failure, and saying "job object" would hide the real reason.
            let exit = child.try_wait();
            let _ = crate::lifecycle::kill(&mut child);
            let _ = child.wait();
            return Err(match exit {
                Ok(Some(status)) => ServerError::StartupExited {
                    binary: shared.spec.binary.clone(),
                    status: status.to_string(),
                    hint: Hint::default(),
                },
                _ => ServerError::Spawn {
                    binary: shared.spec.binary.clone(),
                    source,
                },
            });
        }
        {
            let mut process = shared.process.lock().unwrap_or_else(|p| p.into_inner());
            process.child = child;
            process.started_at = Instant::now();
        }

        // The one path where the child is running and this function does not return a `Supervisor`,
        // so it is the one path that has to kill it explicitly — on Unix the guard would not.
        if let Err(err) = wait_until_ready(&shared, origin) {
            shared.kill();
            return Err(err);
        }

        let watcher = {
            let shared = Arc::clone(&shared);
            std::thread::Builder::new()
                .name(format!("volucer-upstream-{name}"))
                .spawn(move || watch(shared))
                .ok()
        };

        tracing::info!(
            backend = %name,
            binary = %shared.spec.binary.display(),
            origin = %config.origin,
            restart = %config.restart_policy(),
            "supervised origin started"
        );

        Ok(Supervisor {
            shared,
            _guard: guard,
            watcher,
        })
    }

    /// The pid of the live origin process, for diagnostics and tests.
    pub fn pid(&self) -> u32 {
        self.shared
            .process
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .child
            .id()
    }

    /// Stop the origin.
    ///
    /// Order matters: the flag first, so the watcher cannot restart what is about to be killed;
    /// then the kill, so the watcher's next poll sees a deliberate stop rather than an exit; then
    /// the join, so no thread outlives the `Arc` it holds.
    pub fn shutdown(&mut self) {
        self.shared.stopping.store(true, Ordering::SeqCst);
        self.shared.kill();
        if let Some(watcher) = self.watcher.take() {
            let _ = watcher.join();
        }
    }
}

impl Drop for Supervisor {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// A process that has already exited, to hold a child-shaped hole in `Shared`.
///
/// `std::process::Child` has no default constructor and this is the cheapest way to build `Shared`
/// before the real child exists. Nothing observes it: it is spawned with no configuration that can
/// fail, it is replaced before `start` returns or spawns anything else, and it is already reaped by
/// the time it is overwritten.
fn no_child() -> Result<Child> {
    #[cfg(windows)]
    let mut command = {
        let mut command = Command::new("cmd");
        command.args(["/C", "exit 0"]);
        command
    };
    #[cfg(not(windows))]
    let mut command = Command::new("true");
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = command.spawn().map_err(|source| ServerError::Spawn {
        binary: PathBuf::from(if cfg!(windows) { "cmd" } else { "true" }),
        source,
    })?;
    let _ = child.wait();
    Ok(child)
}

/// Wait for the origin to accept a connection, or report why it did not.
///
/// Probes by connecting to the origin's own address rather than by assuming a port, so this works
/// the same for `http://`, `https://` and `unix:` — and it proves the *client* can reach the address,
/// rather than that something is listening somewhere. No TLS handshake is attempted: an origin
/// listening for TLS answers a TCP connection, and a readiness gate that verified a certificate
/// would be testing `[backends.<n>.tls]` rather than the origin's start.
fn wait_until_ready(shared: &Shared, origin: &OriginSpec) -> Result<()> {
    let endpoint = Endpoint::parse(&probe_address(origin)).ok();

    // The two failure modes are checked together in one loop, so a process that exits during
    // startup is reported at once rather than after the full timeout.
    let deadline = Instant::now() + shared.policy.ready_timeout;
    while Instant::now() < deadline {
        {
            let mut process = shared.process.lock().unwrap_or_else(|p| p.into_inner());
            match process.child.try_wait() {
                Ok(Some(status)) => {
                    return Err(ServerError::StartupExited {
                        binary: shared.spec.binary.clone(),
                        status: status.to_string(),
                        hint: Hint::default(),
                    })
                }
                Ok(None) => {}
                Err(source) => return Err(ServerError::Io(source)),
            }
        }
        if endpoint
            .as_ref()
            .is_some_and(|endpoint| endpoint.connect(Some(PROBE_TIMEOUT)).is_ok())
        {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(25));
    }

    Err(ServerError::StartupTimeout {
        binary: shared.spec.binary.clone(),
        address: probe_address(origin),
        timeout: shared.policy.ready_timeout,
        hint: Hint::default(),
    })
}

/// How an origin is addressed by the readiness probe.
fn probe_address(origin: &OriginSpec) -> String {
    match origin {
        OriginSpec::Network { host, port, .. } => format!("{host}:{port}"),
        #[cfg(unix)]
        OriginSpec::Socket { path, .. } => format!("unix:{}", path.display()),
    }
}

/// Watch the child and restart it per the policy until asked to stop.
fn watch(shared: Arc<Shared>) {
    let mut attempt: u32 = 0;
    loop {
        std::thread::sleep(WATCH_INTERVAL);
        if shared.stopping.load(Ordering::SeqCst) {
            return;
        }

        let exited = {
            let mut process = shared.process.lock().unwrap_or_else(|p| p.into_inner());
            match process.child.try_wait() {
                Ok(Some(status)) => Some((status.success(), process.started_at.elapsed())),
                Ok(None) => None,
                // A `try_wait` that fails leaves the state of the process unknown, and starting a
                // second one beside a possibly-live first is worse than reporting that we cannot
                // tell. The watcher stops rather than guessing.
                Err(err) => {
                    tracing::warn!(
                        backend = %shared.name,
                        %err,
                        "could not check the supervised origin, so it will not be restarted"
                    );
                    return;
                }
            }
        };

        let Some((success, lived)) = exited else {
            continue;
        };

        if !shared.policy.should_restart(success) {
            tracing::info!(
                backend = %shared.name,
                success,
                "the supervised origin exited and will not be restarted"
            );
            return;
        }

        // A run that lasted counts as a fresh streak, so `restart_limit` is a limit on
        // *consecutive* failures rather than on a service's total lifetime restarts.
        if shared.policy.is_stable(lived) {
            attempt = 0;
        }

        attempt += 1;
        if attempt > shared.policy.limit {
            tracing::error!(
                backend = %shared.name,
                limit = shared.policy.limit,
                "the supervised origin keeps exiting; giving up instead of restarting it again"
            );
            return;
        }

        let delay = shared.policy.backoff(attempt);
        tracing::warn!(
            backend = %shared.name,
            attempt,
            delay_ms = delay.as_millis() as u64,
            "restarting the supervised origin"
        );
        // Slept in short steps so a shutdown during a long backoff is not held up by it.
        let wake = Instant::now() + delay;
        while Instant::now() < wake {
            if shared.stopping.load(Ordering::SeqCst) {
                return;
            }
            let remaining = wake.saturating_duration_since(Instant::now());
            std::thread::sleep(WATCH_INTERVAL.min(remaining));
        }
        if shared.stopping.load(Ordering::SeqCst) {
            return;
        }

        match shared.spawn() {
            Ok(child) => {
                let mut process = shared.process.lock().unwrap_or_else(|p| p.into_inner());
                process.child = child;
                process.started_at = Instant::now();
                tracing::info!(
                    backend = %shared.name,
                    attempt,
                    pid = process.child.id(),
                    "the supervised origin was restarted"
                );
            }
            Err(err) => {
                // Counted like any other failure, so a program deleted from under a running server
                // does not restart for ever either.
                tracing::error!(
                    backend = %shared.name,
                    %err,
                    "could not restart the supervised origin"
                );
            }
        }
    }
}

/// Forward an origin's output to the log.
///
/// The same courtesy the worker pool extends to PHP: an application's diagnostics are the only
/// thing that explains a 502 it caused, and a process whose pipe fills up blocks.
fn drain<R: std::io::Read + Send + 'static>(
    backend: String,
    pid: u32,
    source: R,
    stream: &'static str,
) {
    use std::io::BufRead;
    std::thread::spawn(move || {
        let mut reader = std::io::BufReader::new(source);
        let mut line = Vec::new();
        loop {
            line.clear();
            match reader.read_until(b'\n', &mut line) {
                Ok(0) => break,
                Ok(_) => {
                    let text = String::from_utf8_lossy(&line);
                    let text = text.trim_end();
                    if !text.is_empty() {
                        tracing::debug!(backend = %backend, pid, stream, line = %text, "origin output");
                    }
                }
                Err(err) => {
                    tracing::debug!(backend = %backend, pid, stream, %err, "origin output ended");
                    break;
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A command that starts and stays up — the shape of an origin that boots but never listens.
    ///
    /// Written out rather than inlined because both this and the test fixtures need the same
    /// thing: a program that is listening when the readiness probe arrives.
    fn sleeper() -> &'static str {
        if cfg!(windows) {
            "cmd /C ping -n 30 127.0.0.1 > NUL"
        } else {
            "sleep 30"
        }
    }

    /// The interpreter to start an HTTP origin with, or `None` when the machine has none that works.
    ///
    /// **Two bugs are fixed by this one function, and the second was only visible on Windows.** The
    /// first: the names must be *tried*, because the interpreter on Linux is `python3` and a
    /// hardcoded `python` made the test fail with "the origin command python does not exist" *after*
    /// the guard had already decided the machine had one.
    ///
    /// The second is subtler and worth more: **on Windows `python3.exe` exists as a Microsoft Store
    /// stub** — an App Execution Alias that is a real file, passes `is_file()`, and exits `9009`
    /// without running anything. A guard that asks "is there a file with this name" therefore
    /// *passes* and the test fails one layer deeper with a `StartupExited`. So the check is that the
    /// interpreter runs, which is the property the test actually needs and the only one that cannot
    /// be faked by a file's existence.
    fn python() -> Option<&'static str> {
        ["python3", "python"].into_iter().find(|name| {
            let binary = volucer_core::program::resolve_program(name);
            if !binary.is_file() {
                return false;
            }
            std::process::Command::new(&binary)
                .arg("--version")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|status| status.success())
        })
    }

    /// A listener that accepts connections and answers, so the readiness probe succeeds.
    ///
    /// `python -m http.server` binds the port and answers `GET`; the probe only needs a TCP connection
    /// to succeed. Chosen over a shell trick because it has no quoting to get wrong and because a
    /// *real* server is what this test is about — a supervisor that starts a command and waits for it
    /// to answer needs a command that behaves like the thing a deployment would name.
    /// A listener that accepts connections and answers, so the readiness probe succeeds.
    ///
    /// **Not `python -m http.server`, which loses a race with the probe.** `HTTPServer.server_bind`
    /// binds and *then* calls `socket.getfqdn(host)` before `server_activate()` gets to `listen()`
    /// (`socketserver.TCPServer.__init__` -> `server_bind()` -> `server_activate()`). On macOS that
    /// lookup goes through mDNSResponder and takes seconds, so for that window the port is bound but
    /// refusing connections: every `connect()` in the readiness loop fails and the supervisor reports
    /// `StartupTimeout` for an origin that was about to be ready. It was macOS-only and intermittent,
    /// which is how it read in CI: one red run in four, only on the runner with a slow resolver.
    ///
    /// This fixture inverts that order: `listen()` is the first thing it does, and the name lookup
    /// `http.server` would have done is never performed. It still exercises everything the test is
    /// about (spawn a command, poll a process, probe an address, kill it on shutdown), because the
    /// probe only ever needed a TCP connection. What it stops measuring is macOS's resolver.
    fn listener_command(interpreter: &str, port: u16) -> String {
        let bind = format!("s.bind(('127.0.0.1', {port}))");
        let script = [
            "import socket, time".to_string(),
            "s = socket.socket()".to_string(),
            "s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)".to_string(),
            bind,
            "s.listen(8)".to_string(),
            "time.sleep(30)".to_string(),
        ]
        .join("; ");

        format!("{interpreter} -c \"{script}\"")
    }

    fn free_port() -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind a probe port");
        let port = listener.local_addr().expect("address").port();
        drop(listener);
        port
    }

    fn config(command: &str) -> UpstreamConfig {
        let mut config = UpstreamConfig::new("http://127.0.0.1:0");
        config.command = Some(command.to_string());
        config
    }

    fn origin(port: u16) -> OriginSpec {
        OriginSpec::parse(&format!("http://127.0.0.1:{port}"), None).expect("a loopback origin")
    }

    #[test]
    fn a_missing_command_program_is_refused_by_name_not_spawned() {
        let port = free_port();
        let err = Supervisor::start(
            "app",
            &config("definitely-not-a-program --serve"),
            &origin(port),
        )
        .unwrap_err();
        assert!(matches!(err, ServerError::Misconfigured { .. }), "{err}");
        assert!(err.to_string().contains("does not exist"), "{err}");
    }

    #[test]
    fn a_command_that_exits_at_once_is_a_startup_failure_not_a_timeout() {
        // The distinction that matters: an origin that could not boot at all must say so at once
        // rather than after `ready_timeout_ms`, which is what an operator reads as "slow".
        let port = free_port();
        let command = if cfg!(windows) {
            "cmd /C exit 3"
        } else {
            "sh -c \"exit 3\""
        };
        let err = Supervisor::start("app", &config(command), &origin(port)).unwrap_err();
        assert!(matches!(err, ServerError::StartupExited { .. }), "{err}");
    }

    #[test]
    fn a_working_directory_that_is_not_a_directory_is_refused() {
        let port = free_port();
        let mut config = config(sleeper());
        config.working_dir = Some(PathBuf::from("definitely-not-a-directory"));
        let err = Supervisor::start("app", &config, &origin(port)).unwrap_err();
        assert!(matches!(err, ServerError::Misconfigured { .. }), "{err}");
        assert!(err.to_string().contains("is not a directory"), "{err}");
    }

    #[test]
    fn an_origin_that_never_listens_is_a_timeout_and_does_not_leave_a_process_behind() {
        // The orphan hazard, and the reason it is asserted here rather than trusted: the process
        // *is* running when the timeout fires, so nothing but this path can kill it. On Unix the
        // lifecycle guard is a no-op, so without the explicit kill the `sleep` outlives the test.
        let port = free_port();
        let mut config = config(sleeper());
        config.ready_timeout_ms = 400;
        let err = Supervisor::start("app", &config, &origin(port)).unwrap_err();
        assert!(matches!(err, ServerError::StartupTimeout { .. }), "{err}");

        // Nothing was left holding the port, so the next bind succeeds. A leaked process is not
        // listening here, so the stronger statement — that the process is gone — is asserted by
        // the port being free rather than by counting processes, which would be platform-specific.
        std::net::TcpListener::bind(("127.0.0.1", port))
            .expect("the failed supervisor left nothing behind");
    }

    #[test]
    fn a_supervised_origin_that_answers_is_ready_and_is_killed_on_shutdown() {
        let Some(interpreter) = python() else {
            eprintln!("skipping: no python binary on PATH, so there is no origin to supervise");
            return;
        };
        let port = free_port();
        let mut supervisor = Supervisor::start(
            "app",
            &config(&listener_command(interpreter, port)),
            &origin(port),
        )
        .expect("the origin starts and answers");
        let pid = supervisor.pid();
        assert!(pid > 0);
        supervisor.shutdown();

        // Shut down rather than merely dropped: the port is free without waiting for the OS to
        // reap anything, which is what `shutdown`'s kill-then-wait is for.
        std::net::TcpListener::bind(("127.0.0.1", port))
            .expect("the origin released its port when it was stopped");
    }

    #[test]
    fn the_backoff_doubles_from_the_configured_delay_and_stops_at_the_cap() {
        let policy = Policy {
            restart: UpstreamRestart::OnFailure,
            limit: 10,
            backoff_ms: 100,
            ready_timeout: Duration::from_secs(1),
        };
        assert_eq!(policy.backoff(1), Duration::from_millis(100));
        assert_eq!(policy.backoff(2), Duration::from_millis(200));
        assert_eq!(policy.backoff(3), Duration::from_millis(400));
        assert_eq!(policy.backoff(4), Duration::from_millis(800));
        // And it stops, rather than growing without bound: a delay that reached hours would turn a
        // transient failure into an outage nobody attributes to it.
        assert_eq!(
            policy.backoff(30),
            Duration::from_millis(UPSTREAM_RESTART_BACKOFF_CAP_MS)
        );
    }

    #[test]
    fn only_a_failure_is_restarted_by_the_default_policy() {
        let policy = Policy {
            restart: UpstreamRestart::OnFailure,
            limit: 3,
            backoff_ms: 50,
            ready_timeout: Duration::from_secs(1),
        };
        assert!(
            policy.should_restart(false),
            "an uncaught exception is a failure"
        );
        assert!(
            !policy.should_restart(true),
            "a clean exit is somebody having asked it to stop, and restarting would fight them"
        );

        let never = Policy {
            restart: UpstreamRestart::Never,
            ..policy
        };
        assert!(!never.should_restart(false));

        let always = Policy {
            restart: UpstreamRestart::Always,
            ..policy
        };
        assert!(always.should_restart(true));
    }

    #[test]
    fn a_plain_upstream_config_is_not_refused_by_the_new_supervisor_rule() {
        // The regression this exists for: `restart` defaulting to a value rather than staying
        // absent made every *unsupervised* upstream — the common case, somebody else's process —
        // look as though it had set a supervisor key, and `validate` refused it. A serde default
        // and a validator that checks "was this key set" are not compatible, and only a test that
        // parses the ordinary configuration catches it.
        let text = r#"
[server]
default_backend = "node"
http = "127.0.0.1:8080"
[backends.node]
kind = "upstream"
origin = "http://127.0.0.1:3000"
"#;
        volucer_core::config::Config::from_toml(text)
            .expect("an upstream with no command and no supervisor keys is still valid");
    }

    #[test]
    fn a_run_that_lasted_counts_as_a_fresh_streak() {
        // Without this, `restart_limit` would be a limit on a service's total lifetime restarts:
        // an origin that crashes once a week would be abandoned after three weeks.
        let policy = Policy {
            restart: UpstreamRestart::OnFailure,
            limit: 3,
            backoff_ms: 50,
            ready_timeout: Duration::from_secs(1),
        };
        assert!(!policy.is_stable(Duration::from_millis(10)));
        assert!(policy.is_stable(STABLE_RUN));
        assert!(policy.is_stable(STABLE_RUN + Duration::from_secs(1)));
    }
}
