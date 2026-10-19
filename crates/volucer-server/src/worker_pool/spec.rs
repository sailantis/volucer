//! What a worker is started from — or, for an attached pool, dialled at. The resolved
//! program, its arguments, its environment, and the checks that make a typo a startup error
//! by name rather than a confusing `CreateProcess` / `execvp` failure later. **No process is
//! created and no socket is opened here** — this module only decides *what* to reach.

use crate::error::{Result, ServerError};
use std::collections::BTreeMap;
use std::path::PathBuf;
use volucer_core::config::{AttachConfig, ChecksumMode, WorkerConfig, WorkerTransport};
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
    /// The gRPC services this pool's workers were granted, captured from the configuration.
    ///
    /// **Captured rather than read from the configuration per request**, for the reason the
    /// registry is built per backend and not per process: the list is a decision made once, at
    /// startup, about what this pool may be asked to do. Reaching into a live configuration would
    /// also make the answer depend on a reload landing mid-request, which is the ordering hazard
    /// the listener's snapshot exists to prevent.
    ///
    /// Beside `checksum` because it is the same *kind* of fact — a handshake-time property of
    /// this pool — and because a replaced worker must be granted what the worker it replaces was
    /// granted, which is what putting it on the spec gives for free.
    pub(super) grpc_services: Vec<String>,
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
            },
            checksum: checksum_of(config.checksum),
            grpc_services: config.grpc_services.clone(),
        })
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

        Ok(Spec {
            launch: Launch::Attach(AttachSpec {
                relay: config.relay.trim().to_string(),
                workers: config.workers,
            }),
            checksum: checksum_of(config.checksum),
            grpc_services: config.grpc_services.clone(),
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
    pub(super) fn describe(&self) -> String {
        match &self.launch {
            Launch::Spawn { binary, args, .. } => {
                let mut line = binary.display().to_string();
                for arg in args {
                    line.push(' ');
                    line.push_str(arg);
                }
                line
            }
            Launch::Attach(spec) => format!("{} (attached)", spec.relay),
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
            env: Default::default(),
            checksum: Default::default(),
            services: Vec::new(),
            grpc_services: Vec::new(),
            transport: Default::default(),
        }
    }

    fn attach_config(workers: u32) -> AttachConfig {
        AttachConfig {
            relay: "unix:/run/app/workers.sock".to_string(),
            workers,
            max_requests: None,
            checksum: Default::default(),
            services: Vec::new(),
            grpc_services: Vec::new(),
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
