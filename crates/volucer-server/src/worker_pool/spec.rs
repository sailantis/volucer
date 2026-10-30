//! What a worker is started from — or, for an attached pool, dialled at. The resolved
//! program, its arguments, its environment, and the checks that make a typo a startup error
//! by name rather than a confusing `CreateProcess` / `execvp` failure later. **No process is
//! created and no socket is opened here** — this module only decides *what* to reach.

use crate::error::{Result, ServerError};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;
use volucer_core::config::{
    AttachConfig, ChecksumMode, JobsPoolConfig, WorkerConfig, WorkerTransport,
};
use volucer_core::program::{resolve_program, split_command};
use volucer_worker::Checksum;

/// How a pool's slots are obtained: a process per slot, or a connection per slot.
///
/// The two are one type rather than two pools because everything above them is identical —
/// handshake, exchange thread, slot locking, recycling, metrics, calls, shutdown — and only the
/// first and last step differ: who creates the far end, and what "kill it" means. An attached
/// slot has no process to kill: closing its connection is the whole of the teardown, and the
/// supervisor that forked it decides what happens next.
#[derive(Debug, Clone)]
pub(super) enum Launch {
    /// volucer starts the process itself.
    Spawn {
        /// The resolved program, so a missing interpreter is a startup error rather than a
        /// confusing `CreateProcess` / `execvp` failure later.
        binary: PathBuf,
        args: Vec<String>,
        working_dir: Option<PathBuf>,
        env: BTreeMap<String, String>,
        /// How to connect to the worker. See [`WorkerTransport`].
        transport: WorkerTransport,
        /// A `unix:` path **prefix** when the socket transport is on a unix socket.
        ///
        /// `None` means the loopback ephemeral port, which is what every configuration written
        /// before this key existed means. The slot index is appended to make one address per
        /// worker; the pool does that, not this struct, because the index is the pool's knowledge.
        ///
        /// On the spec rather than passed to `boot` for the same reason `checksum` is: a
        /// *replaced* worker must be reached the same way as the one it replaces, and a slot that
        /// respawned onto a different address would be a slot whose connection had moved.
        socket: Option<String>,
    },
    /// Somebody else started the process; volucer connects to it.
    Attach(AttachSpec),
}

/// The relay an `attach` backend connects to.
#[derive(Debug, Clone)]
pub(super) struct AttachSpec {
    /// `unix:/path` or `host:port`, already validated by the configuration model.
    pub(super) relay: String,
    /// How many connections to hold, which is how many requests run at once.
    pub(super) workers: u32,
}

/// What a worker pool reaches: how to launch or attach, and the handshake-time policy every
/// slot must share.
#[derive(Debug, Clone)]
pub(super) struct Spec {
    pub(super) launch: Launch,
    /// The checksum policy to offer at handshake.
    ///
    /// On the spec rather than passed to `boot` because a *replaced* worker must be handshaken
    /// on the same terms as the one it replaces: a pool that negotiated `optional` at startup
    /// and then respawned a worker under `required` would have one worker verifying and the rest
    /// not, which is a difference nothing would report.
    pub(super) checksum: Checksum,
    /// How long one request may take before the slot's worker is replaced.
    ///
    /// On the spec rather than read from the configuration at each request, for the same reason
    /// `checksum` is: the value is a property of the pool this process built, and a reload does not
    /// change it — the deadlines a running pool enforces are the ones it started with, which is what
    /// `restart_only` says out loud.
    pub(super) request_timeout: Duration,
}

impl Spec {
    /// Split, resolve and check a configured worker command.
    ///
    /// Checked here rather than at spawn time so that a typo in `command` or `working_dir` is
    /// reported by name, before any port is bound and before a pool half-exists.
    pub(super) fn from_config(name: &str, config: &WorkerConfig) -> Result<Self> {
        let (program, args) =
            split_command(&config.command).map_err(|reason| ServerError::Misconfigured {
                backend: name.to_string(),
                reason,
            })?;
        let binary = resolve_program(&program);

        if !binary.is_file() {
            return Err(ServerError::Misconfigured {
                backend: name.to_string(),
                reason: format!(
                    "the worker program {} does not exist (from command {:?})",
                    binary.display(),
                    config.command
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

        Ok(Spec {
            launch: Launch::Spawn {
                binary,
                args,
                working_dir: config.working_dir.clone(),
                env: config.env.clone(),
                transport: config.transport,
                socket: config.socket.clone(),
            },
            checksum: checksum_of(config.checksum),
            request_timeout: request_timeout_of(config.request_timeout_ms, name)?,
        })
    }

    /// Read a `[jobs.pool]` table into the same spec.
    ///
    /// A consumer pool is the same pool — boot, handshake, lease, recycle — so it is described by
    /// the same [`Spec`] rather than by a second one. The translation is the whole of the
    /// function, and it exists here rather than in `mod.rs` so that the fields which *are* a
    /// property of reaching a process (`transport`, `socket`) stay next to the code that acts on
    /// them.
    pub(super) fn from_jobs_config(name: &str, config: &JobsPoolConfig) -> Result<Self> {
        Self::from_config(
            name,
            &WorkerConfig {
                command: config.command.clone(),
                workers: config.workers,
                memory_limit_mb: config.memory_limit_mb,
                max_requests: config.max_jobs,
                working_dir: config.working_dir.clone(),
                request_timeout_ms: None,
                env: config.env.clone(),
                services: Vec::new(),
                grpc_services: Vec::new(),
                grpc_serve_any: false,
                grpc_proto: Vec::new(),
                grpc_proto_services: Vec::new(),
                checksum: config.checksum,
                transport: config.transport,
                socket: config.socket.clone(),
            },
        )
    }

    /// Read an `attach` backend's table.
    ///
    /// Only the two things volucer owns are checked, and [`AttachConfig::validate`] has already
    /// refused the rest at configuration load. There is no program to resolve and no working
    /// directory to stat, because volucer does not start anything: a supervisor that forked its
    /// children decided all of it, and re-checking it here would be a second, weaker answer.
    pub(super) fn from_attach_config(name: &str, config: &AttachConfig) -> Result<Self> {
        config
            .validate(name)
            .map_err(|reason| ServerError::Misconfigured {
                backend: name.to_string(),
                reason,
            })?;

        let relay = config
            .relay()
            .map_err(|reason| ServerError::Misconfigured {
                backend: name.to_string(),
                reason,
            })?;

        Ok(Spec {
            launch: Launch::Attach(AttachSpec {
                relay: relay.into_owned(),
                workers: config.workers,
            }),
            checksum: checksum_of(config.checksum),
            // An attached pool's deadline is the pool's own: there is no process to kill, so a
            // replacement is a reconnect, and the supervisor on the far side has its own timeouts.
            // Configurable on the backend it belongs to (`[backends.<n>]`), not on `attach`.
            request_timeout: crate::worker_pool::REQUEST_TIMEOUT,
        })
    }

    /// The program that is started, for the error paths that name one.
    ///
    /// `None` for an attached pool, which starts nothing: a failure there names the relay
    /// instead, through [`ServerError::Attach`].
    pub(super) fn binary(&self) -> Option<&PathBuf> {
        match &self.launch {
            Launch::Spawn { binary, .. } => Some(binary),
            Launch::Attach(_) => None,
        }
    }

    /// What this pool reaches, as an operator would name it: a command or a relay address.
    ///
    /// A socket-transport pool appends where it listens, because the transport is invisible on
    /// the wire and the address is the one thing an operator has to check when a worker will not
    /// connect. The prefix is shown rather than a slot's finished path: it is what the file says.
    pub(super) fn describe(&self) -> String {
        match &self.launch {
            Launch::Spawn {
                binary,
                args,
                transport,
                socket,
                ..
            } => {
                let mut line = binary.display().to_string();
                for arg in args {
                    line.push(' ');
                    line.push_str(arg);
                }
                if !transport.is_pipe() {
                    match socket {
                        Some(socket) => {
                            line.push_str(&format!(" ({} on {socket}.<slot>.sock)", transport));
                        }
                        // The loopback port is picked at bind time, so there is nothing to name
                        // here — saying "loopback" is still more than the transport alone says.
                        None => line.push_str(&format!(" ({transport} on a loopback port)")),
                    }
                }
                line
            }
            Launch::Attach(spec) => format!("{} (attached)", spec.relay),
        }
    }

    /// The worker socket bound by slot `index`, when this pool listens on unix sockets.
    ///
    /// `None` for a pipe, for an attached pool, and for a socket-transport pool whose address is
    /// a loopback port — the three cases in which volucer either binds nothing or lets the kernel
    /// choose.
    pub(super) fn worker_socket(&self, index: u32) -> Option<PathBuf> {
        match &self.launch {
            Launch::Spawn {
                transport, socket, ..
            } if !transport.is_pipe() => socket
                .as_deref()
                .map(|prefix| volucer_core::config::worker_socket_path(prefix, index)),
            _ => None,
        }
    }

    /// How many slots this spec asks for, when the spec itself decides.
    ///
    /// A spawned pool's `workers` lives on the configuration and is read by the caller, because
    /// `0` there means "one per CPU" and resolving that needs `worker_count`. An attached pool
    /// has no such convention — the supervisor already forked a specific number — so the count
    /// is part of the spec and there is nothing to resolve.
    pub(super) fn attached_workers(&self) -> Option<u32> {
        match &self.launch {
            Launch::Attach(spec) => Some(spec.workers),
            Launch::Spawn { .. } => None,
        }
    }
}

fn checksum_of(mode: ChecksumMode) -> Checksum {
    match mode {
        ChecksumMode::Required => Checksum::Required,
        ChecksumMode::Optional => Checksum::Optional,
    }
}

/// A configured `request_timeout_ms`, resolved or refused.
///
/// `None` means the pool's own default, which is the thirty seconds the constant always was. `0` is
/// refused rather than read as "no limit": a worker with no deadline can be lost for ever with the
/// slot it holds, and the pool has no way to tell that from a slow request — the two would be
/// indistinguishable until the pool ran out of slots.
fn request_timeout_of(configured: Option<u64>, backend: &str) -> Result<Duration> {
    match configured {
        None => Ok(crate::worker_pool::REQUEST_TIMEOUT),
        Some(0) => Err(ServerError::Misconfigured {
            backend: backend.to_string(),
            reason: "request_timeout_ms = 0 would give a worker no deadline at all, so a hung \
                     request could hold its slot for ever; write the number of milliseconds you \
                     mean instead"
                .to_string(),
        }),
        Some(millis) => Ok(Duration::from_millis(millis)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn worker_config(command: &str) -> WorkerConfig {
        WorkerConfig {
            command: command.to_string(),
            workers: 1,
            memory_limit_mb: None,
            max_requests: None,
            working_dir: None,
            request_timeout_ms: None,
            env: Default::default(),
            checksum: Default::default(),
            services: Vec::new(),
            grpc_services: Vec::new(),
            grpc_serve_any: false,
            grpc_proto: Vec::new(),
            grpc_proto_services: Vec::new(),
            transport: Default::default(),
            socket: Default::default(),
        }
    }

    fn attach_config(workers: u32) -> AttachConfig {
        AttachConfig {
            relay: Some("unix:/run/app/workers.sock".to_string()),
            workers,
            max_requests: None,
            checksum: Default::default(),
            services: Vec::new(),
            grpc_services: Vec::new(),
            grpc_serve_any: false,
            grpc_proto: Vec::new(),
            grpc_proto_services: Vec::new(),
        }
    }

    #[test]
    fn a_missing_program_is_a_misconfiguration_not_a_panic() {
        let config = worker_config("definitely-not-a-php-binary worker.php");
        let err = Spec::from_config("test", &config).unwrap_err();
        assert!(matches!(err, ServerError::Misconfigured { .. }), "{err}");
        assert!(err.to_string().contains("does not exist"), "{err}");
    }

    #[test]
    fn a_working_directory_that_is_not_a_directory_is_refused() {
        // A path that certainly exists but is not a directory: the test binary itself.
        let binary = std::env::current_exe().expect("the test binary path");
        let mut config = worker_config(&binary.display().to_string());
        config.working_dir = Some(binary);
        let err = Spec::from_config("test", &config).unwrap_err();
        assert!(err.to_string().contains("is not a directory"), "{err}");
    }

    #[test]
    fn an_empty_command_is_a_misconfiguration() {
        let err = Spec::from_config("test", &worker_config("")).unwrap_err();
        assert!(err.to_string().contains("empty"), "{err}");
    }

    #[test]
    fn an_attach_spec_carries_the_relay_and_its_connection_count() {
        let spec = Spec::from_attach_config("app", &attach_config(16)).expect("a valid relay");

        assert_eq!(Some(16), spec.attached_workers());
        assert!(spec.binary().is_none(), "an attached pool starts nothing");
        assert_eq!(
            "unix:/run/app/workers.sock (attached)",
            spec.describe(),
            "the relay is what an error should name"
        );
    }

    #[test]
    fn an_attach_spec_refuses_zero_workers_rather_than_resolving_it() {
        // `0` on a spawned pool means "one per CPU". Here that would silently disconnect every
        // child the supervisor forked, so the same number has to be refused instead.
        let err = Spec::from_attach_config("app", &attach_config(0)).unwrap_err();
        assert!(matches!(err, ServerError::Misconfigured { .. }), "{err}");
        assert!(err.to_string().contains("at least 1"), "{err}");
    }
}
