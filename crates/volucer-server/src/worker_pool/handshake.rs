//! Starting a worker and getting it ready: `spawn`, or a `connect` to a relay somebody else
//! listens on; the `HELLO` / `HELLO_ACK` handshake under a deadline; the socket-transport accept
//! poll; and the `stderr` drain thread. Everything that waits here has a deadline, and the
//! deadline is enforced by killing the worker — see the note in [`super`].

use super::exchange::{Channel, ChannelExchange};
use super::spec::{AttachSpec, Launch, Spec};
use crate::calls::MethodRegistry;
use crate::error::{Hint, Result, ServerError, SOCKET_WORKER_HINT};
use crate::lifecycle::{configure_command, LifecycleGuard};
use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::net::TcpListener;
use std::process::{Child, ChildStderr, Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Arc;
use std::time::{Duration, Instant};
use volucer_core::config::WORKER_SOCKET_ENV;
use volucer_fcgi::{Endpoint, FcgiError, Stream};
use volucer_worker::{BoundedChannel, Checksum, HelloAck, WorkerError};

#[cfg(unix)]
use std::os::unix::net::UnixListener;
#[cfg(unix)]
use std::path::PathBuf;

/// How long a worker gets to complete the `HELLO` / `HELLO_ACK` handshake.
///
/// Generous, because it covers loading the application: a framework boot costs
/// real time, and a worker that boots once should not be killed for taking a
/// while at the one moment where being slow is expected.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a connect to an `attach` relay gets before startup fails.
///
/// Short, because there is nothing to wait for: a supervisor that is listening accepts
/// immediately, and one that is not will not become so because we waited. Thirty seconds would
/// turn a missing socket into a startup that looks like it is doing something.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// How often a socket-transport pool looks for a worker that has not connected yet.
///
/// The connection is made by a process this server has just spawned, so it arrives within a
/// millisecond of being made and the poll interval is the whole cost of finding out. It is
/// short for that reason: this delay is paid once per worker at startup, not per request.
const ACCEPT_POLL_INTERVAL: Duration = Duration::from_millis(2);

/// A worker that has been started and has completed its handshake.
#[derive(Debug)]
pub(super) struct Ready {
    pub(super) link: Link,
    pub(super) exchange: ChannelExchange,
    pub(super) ack: HelloAck,
}

/// The far end of a slot: a process volucer started, or a connection it dialled.
///
/// **One type, because everything above it is the same.** A slot needs four things from its
/// worker: an identity for logs and metrics, an answer to "is it still there", a way to kill it,
/// and a way to wait for it to be gone. For a spawned worker all four are
/// [`std::process::Child`]. For an attached one they are the connection itself: closing it is
/// the kill, dialling again is the respawn, and the supervisor on the other end decides what its
/// child does.
///
/// The alternative — a second pool — was rejected because the *interesting* part is identical
/// (handshake, exchange thread, slot locking, recycling, metrics, calls) and duplicating it
/// would mean every future fix to one of them being made twice, or once.
///
/// An attached slot deliberately has **no waiting**: there is no `waitpid` for a process we did
/// not fork, so [`Link::wait`] closes nothing and returns. That is the honest answer rather than
/// a simulated one — the process's own exit is the supervisor's to observe, and it is the
/// supervisor that will replace it.
#[derive(Debug)]
pub(super) enum Link {
    /// volucer started this process and is responsible for reaping it.
    Process(Child),
    /// volucer connected to this worker and is responsible only for closing the connection.
    ///
    /// The pid is what the worker reported in its `HELLO_ACK`, which is the *child's* pid on
    /// the far side — a process this one cannot signal. It is kept because a pid is the only
    /// handle an operator has on a worker, and because `workers.list` reports one.
    Attached {
        /// The worker's self-reported pid, or 0 if it did not report one.
        pid: u32,
    },
}

impl Link {
    /// The worker's pid, for logs, metrics and `workers.list`.
    pub(super) fn id(&self) -> u32 {
        match self {
            Link::Process(child) => child.id(),
            Link::Attached { pid } => *pid,
        }
    }

    /// Whether the worker is still there, without blocking.
    ///
    /// For a process, `try_wait`. For a connection, `true` until a request fails: a closed socket
    /// is discovered by writing to it, and there is no equivalent of `try_wait` for a peer that
    /// may be mid-request. Polling with a zero-byte read would be the wrong question — a healthy
    /// worker is deliberately silent between requests — so this answers the optimistic thing and
    /// lets the exchange thread's error be the real signal, which is the same path a pipe worker
    /// takes when it dies between `try_wait` and the write.
    pub(super) fn is_alive(&mut self) -> bool {
        match self {
            Link::Process(child) => matches!(child.try_wait(), Ok(None)),
            Link::Attached { .. } => true,
        }
    }

    /// Terminate the worker.
    ///
    /// A process is killed by group (see [`crate::lifecycle::kill`]). An attached worker is
    /// released by closing its connection — the exchange thread owns the socket and drops it when
    /// the slot does — which is how this protocol says "stop": the worker's read loop returns and
    /// it exits on its own. That is also the graceful half of `Slot::stop`; for `terminate` there
    /// is nothing more, because a process we did not start has no signal we may send.
    pub(super) fn kill(&mut self) -> std::io::Result<()> {
        match self {
            Link::Process(child) => crate::lifecycle::kill(child),
            // Nothing to signal: the connection closes when the exchange is dropped.
            Link::Attached { .. } => Ok(()),
        }
    }

    /// Wait for the worker to be gone.
    ///
    /// A process is reaped. An attached one is not waited for at all: `waitpid` only works on
    /// children, and the supervisor that owns this worker notices its exit before we could.
    /// Returning immediately rather than sleeping is deliberate — a pool that blocked here would
    /// make shutdown slower for a process it does not own.
    pub(super) fn wait(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
        match self {
            Link::Process(child) => child.wait().map(Some),
            Link::Attached { .. } => Ok(None),
        }
    }
}

/// The writer half of a relay connection.
///
/// A newtype rather than passing `Stream` twice, because the two halves are handed to
/// `BoundedChannel::new` as two different trait objects and naming them keeps the type of each
/// obvious at the call site.
#[derive(Debug)]
struct RelayWriter(Stream);

impl Write for RelayWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}

/// The reader half of a relay connection.
///
/// Reading through `Stream` is what arms `TCP_QUICKACK` on the TCP arm — see
/// [`volucer_fcgi::transport`] — which matters here for the same reason it does for FastCGI: the
/// protocol is a ping-pong of small frames, and a delayed ACK is a delayed answer.
#[derive(Debug)]
struct RelayReader(Stream);

impl Read for RelayReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.0.read(buf)
    }
}

/// The listener a socket-transport worker connects back to.
///
/// One type rather than two code paths, because everything below the bind is identical: an
/// `accept` with a deadline, a poll interval, and the same two error shapes. The address is
/// either an ephemeral loopback port — which the kernel chooses, so it has to be read back after
/// binding — or a unix socket path from the configuration.
#[derive(Debug)]
enum WorkerListener {
    Tcp(TcpListener),
    #[cfg(unix)]
    Unix(UnixListener, PathBuf),
}

impl WorkerListener {
    /// Bind the listener a spawned worker will connect back to.
    ///
    /// The unix arm unlinks a stale path first, for the reason the SDK supervisors document: a
    /// socket path is a *file*, and binding one that already exists fails with `EADDRINUSE`, so a
    /// server restarted after a crash could never come up. It is safe here because this process is
    /// meant to be the only listener on that path — a live server would hold the socket and the
    /// bind would fail rather than two processes sharing one address.
    fn bind(spec: &Spec, index: u32) -> Result<Option<Self>> {
        let Launch::Spawn { transport, .. } = &spec.launch else {
            return Ok(None);
        };
        if transport.is_pipe() {
            return Ok(None);
        }
        let binary = spec
            .binary()
            .expect("a socket-transport listener only exists for a spawned pool");
        let spawn_err = |source| ServerError::Spawn {
            binary: binary.clone(),
            source,
        };

        match spec.worker_socket(index) {
            None => TcpListener::bind(("127.0.0.1", 0))
                .map(|listener| Some(WorkerListener::Tcp(listener)))
                .map_err(spawn_err),
            Some(path) => {
                #[cfg(unix)]
                {
                    match std::fs::remove_file(&path) {
                        Ok(()) => {}
                        Err(err) if err.kind() == ErrorKind::NotFound => {}
                        // Not fatal by itself — the bind below is the authority — but it means
                        // something is at the path that is not ours, and the bind failure would
                        // report only "address in use".
                        Err(err) => {
                            return Err(ServerError::Misconfigured {
                                backend: "worker".to_string(),
                                reason: format!(
                                    "could not remove the stale worker socket {}: {err}",
                                    path.display()
                                ),
                            })
                        }
                    }
                    UnixListener::bind(&path)
                        .map(|listener| Some(WorkerListener::Unix(listener, path.clone())))
                        .map_err(spawn_err)
                }
                #[cfg(not(unix))]
                {
                    let _ = path;
                    Err(ServerError::Misconfigured {
                        backend: "worker".to_string(),
                        reason: "unix sockets are not available on this platform".to_string(),
                    })
                }
            }
        }
    }

    /// The address to hand the worker, in the form `VOLUCER_WORKER_SOCKET` carries.
    ///
    /// A unix socket keeps the `unix:` prefix, which is the same convention the configuration
    /// uses for `relay` and `external.socket`. The SDKs already treat this variable's value as
    /// opaque, so the prefix is what tells them which wrapper to use.
    fn address(&self) -> Result<String> {
        match self {
            WorkerListener::Tcp(listener) => listener
                .local_addr()
                .map(|address| address.to_string())
                .map_err(ServerError::Io),
            #[cfg(unix)]
            WorkerListener::Unix(_, path) => Ok(format!("unix:{}", path.display())),
        }
    }

    fn set_nonblocking(&self, nonblocking: bool) -> std::io::Result<()> {
        match self {
            WorkerListener::Tcp(listener) => listener.set_nonblocking(nonblocking),
            #[cfg(unix)]
            WorkerListener::Unix(listener, _) => listener.set_nonblocking(nonblocking),
        }
    }

    /// Accept one connection, or report that none is waiting yet.
    fn accept(&self) -> std::io::Result<(Stream, bool)> {
        match self {
            WorkerListener::Tcp(listener) => listener
                .accept()
                .map(|(stream, _)| (Stream::Tcp(stream), true)),
            #[cfg(unix)]
            WorkerListener::Unix(listener, _) => listener
                .accept()
                .map(|(stream, _)| (Stream::Unix(stream), false)),
        }
    }

    /// A human-readable address for an error message, when one can be produced.
    fn describe(&self) -> String {
        self.address()
            .unwrap_or_else(|_| "the worker socket".to_string())
    }
}

/// Wait for a socket-transport worker to connect back, and give up rather than hang.
///
/// A blocking `accept` here has no deadline, and the failure it waits for is a quiet one: a
/// worker that starts but connects to the wrong address, or does not connect at all, produces
/// no error anywhere — the accept simply never returns. That turns a misconfiguration into a
/// server that hangs at startup instead of one that says what is wrong, which is the same
/// failure the named-pipe transport had to solve for reads.
///
/// The listener is made non-blocking and polled, rather than blocking in `accept` with the
/// deadline enforced elsewhere: there is no portable `accept` timeout, and this runs once
/// per worker at startup, so the 2 ms granularity is not on any request path. The accepted
/// stream is put back to blocking, which is what the exchange thread wants.
fn accept_with_deadline(
    listener: &WorkerListener,
    spec: &Spec,
    child: &mut Child,
    timeout: Duration,
) -> Result<Stream> {
    let binary = spec
        .binary()
        .expect("a socket-transport listener only exists for a spawned pool");
    let deadline = Instant::now() + timeout;
    listener
        .set_nonblocking(true)
        .map_err(|source| ServerError::Spawn {
            binary: binary.clone(),
            source,
        })?;

    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                // Restore blocking before the stream is handed to the exchange thread.
                match &stream {
                    Stream::Tcp(tcp) => {
                        let _ = tcp.set_nonblocking(false);
                    }
                    #[cfg(unix)]
                    Stream::Unix(unix) => {
                        let _ = unix.set_nonblocking(false);
                    }
                    #[cfg(windows)]
                    Stream::Pipe(_) => {}
                }
                return Ok(stream);
            }
            Err(err) if err.kind() == ErrorKind::WouldBlock => {
                // A worker that has already exited will never connect. Saying so is far more
                // useful than waiting out the full handshake budget, and the exit status is
                // usually the reason: a fatal error, a missing extension, a bad path.
                if let Ok(Some(status)) = child.try_wait() {
                    return Err(ServerError::StartupExited {
                        binary: binary.clone(),
                        status: status.to_string(),
                        hint: Hint(Some(SOCKET_WORKER_HINT)),
                    });
                }
                if Instant::now() >= deadline {
                    return Err(ServerError::StartupTimeout {
                        binary: binary.clone(),
                        address: listener.describe(),
                        timeout,
                        hint: Hint(Some(SOCKET_WORKER_HINT)),
                    });
                }
                std::thread::sleep(ACCEPT_POLL_INTERVAL);
            }
            Err(source) => {
                return Err(ServerError::Spawn {
                    binary: binary.clone(),
                    source,
                })
            }
        }
    }
}

/// Install what answers a worker's call, if anything does.
///
/// `handler` is `None` for a worker whose backend granted no `services`, and the channel then refuses
/// every call by name. That refusal is deliberate and is the per-backend capability
/// boundary: a worker facing the public reaches no namespace at all, and the only thing saying so is
/// the absence of a handler rather than a list that has to be consulted correctly on every call.
///
/// The `Arc` is cloned per worker and moved into the closure, so the read loop holds one reference
/// rather than a borrow of the dispatcher — which is what makes the channel independent of the
/// dispatcher's lifetime, and what lets a pool outlive the caller that built it.
fn install_call_handler(channel: &mut Channel, handler: Option<&Arc<MethodRegistry>>) {
    let Some(registry) = handler else {
        return;
    };
    let registry = Arc::clone(registry);
    channel.set_call_handler(Box::new(move |call| registry.serve(call)));
}

/// The hint for a startup failure, given how the worker was going to be reached.
/// A pipe-transport worker that exits during its handshake is an ordinary application failure
/// — a syntax error, a missing extension, a bad path — and `StartupExited` already says
/// everything useful about it. A socket-transport worker has one far more likely explanation
/// that the status alone cannot express, which is the whole reason the hint exists.
fn transport_hint(spec: &Spec) -> Option<&'static str> {
    match &spec.launch {
        Launch::Spawn { transport, .. } if !transport.is_pipe() => Some(SOCKET_WORKER_HINT),
        // An attached pool never sets `VOLUCER_WORKER_SOCKET`, so the hint would be about a
        // transport it is not using.
        _ => None,
    }
}

/// Start, or connect to, one worker, and complete its handshake.
///
/// Everything that can go wrong with a worker does so here, which is the point:
/// a worker that cannot boot is a startup error, not a mystery on the first
/// request that happens to reach it.
///
/// The two halves below — `boot_spawned` and [`boot_attached`] — differ in exactly two steps,
/// which is why they are two functions rather than two pools: how the far end comes to exist, and
/// what a failure means. Everything between them — installing the call handler, the handshake
/// under its deadline, the `Ready` — is shared, and is what [`finish_boot`] is.
///
/// `handler` is the method registry this backend's workers may call, or `None` to grant nothing.
/// It is installed on the channel before the handshake; see [`install_call_handler`].
///
/// `index` is the slot's position, used only for the per-slot suffix on a unix socket path.
pub(super) fn boot(
    name: &str,
    spec: &Spec,
    index: u32,
    guard: &LifecycleGuard,
    handler: Option<&Arc<MethodRegistry>>,
) -> Result<Ready> {
    match &spec.launch {
        Launch::Spawn { .. } => boot_spawned(name, spec, index, guard, handler),
        Launch::Attach(attach) => boot_attached(name, spec, attach, handler),
    }
}

/// Connect to a worker somebody else started, and complete its handshake.
///
/// **volucer dials; it does not listen.** That direction is the whole reason this backend can
/// exist: a PHP prefork supervisor boots the application once and forks its children, and those
/// children inherit the booted heap — measured at 89% less resident memory for the same 16
/// workers, because each `exec`ed worker maps its own OPcache segment and each `fork`ed one
/// shares the original. But a PHP process cannot hand a file descriptor over, so the side that
/// owns the socket has to be the side that owns the fork. It listens; we connect.
///
/// One connection is one slot: it carries one request at a time, which is exactly the contract a
/// spawned worker's pipes have. That makes `attach.workers` the concurrency of the pool, and it
/// has to match the number of children the supervisor forked — a mismatch is either idle
/// children or requests queued behind a connection with no child to answer them.
///
/// A failed connect is a startup failure rather than a lazily-retried condition, for the same
/// reason a worker that cannot boot is: a deployment that starts and then 502s is worse than one
/// that does not start.
fn boot_attached(
    name: &str,
    spec: &Spec,
    attach: &AttachSpec,
    handler: Option<&Arc<MethodRegistry>>,
) -> Result<Ready> {
    let (writer, reader) = connect_relay(name, attach)?;
    let channel = BoundedChannel::new(writer, reader);

    finish_boot(name, spec, Link::Attached { pid: 0 }, channel, handler)
}

/// One connection to the relay, as a writer and a reader.
///
/// A unix socket is **not** shared through one handle: the exchange thread needs a reader and the
/// request path a writer, and `BoundedChannel` takes them as two objects. `try_clone_stream` is
/// the `dup`, and a failure to make one is a startup error rather than a silent half-duplex
/// channel.
///
/// The relay is parsed by the same `Endpoint` the FastCGI client uses, so `unix:` and `host:port`
/// mean the same thing here as everywhere else in the configuration.
fn connect_relay(name: &str, attach: &AttachSpec) -> Result<(RelayWriter, RelayReader)> {
    let fail = |source: FcgiError| ServerError::Attach {
        backend: name.to_string(),
        relay: attach.relay.clone(),
        source,
    };

    let endpoint = Endpoint::parse(&attach.relay).map_err(fail)?;
    let stream = endpoint.connect(Some(CONNECT_TIMEOUT)).map_err(fail)?;
    let reader = stream
        .try_clone_stream()
        .map_err(|source| fail(FcgiError::Io(source)))?;

    tracing::debug!(
        backend = %name,
        relay = %attach.relay,
        "connected to an attached worker"
    );

    Ok((RelayWriter(stream), RelayReader(reader)))
}

/// The half of a boot that is the same whether the worker was spawned or attached.
///
/// Split out because it holds the *interesting* rules — install the call handler before the
/// handshake, enforce the deadline, and turn a failure into the error that names the real cause —
/// and two copies of them is exactly how a fix lands in one and not the other.
fn finish_boot(
    name: &str,
    spec: &Spec,
    mut link: Link,
    mut channel: Channel,
    handler: Option<&Arc<MethodRegistry>>,
) -> Result<Ready> {
    // The call handler goes on **before the handshake**, and the order matters. A worker may send
    // a `CALL` at any point in its life, and the first thing it does after connecting is boot the
    // application — which is exactly when a framework wants to ask the server something.
    // Installing the handler after `HELLO_ACK` would leave that window answering "no call handler
    // is installed", which is a refusal the worker cannot tell from a server that does not
    // support calls at all.
    install_call_handler(&mut channel, handler);

    match handshake_with_timeout(channel, HANDSHAKE_TIMEOUT, spec.checksum) {
        Ok((channel, ack)) => {
            // An attached worker's pid arrives with its handshake, and is the only handle an
            // operator has on a process this server did not fork.
            if let Link::Attached { pid } = &mut link {
                *pid = ack.pid;
            }
            tracing::debug!(
                backend = %name,
                pid = link.id(),
                php = %ack.php_version,
                sapi = %ack.sapi,
                target = %spec.describe(),
                "worker ready"
            );
            Ok(Ready {
                link,
                exchange: ChannelExchange::start(channel),
                ack,
            })
        }
        Err(failure) => {
            if matches!(failure, HandshakeFailure::TimedOut) {
                // The reader thread is still blocked in a read; ending the process — or dropping
                // the connection — is what ends it.
                let _ = link.kill();
            }
            let status = link.wait().ok().flatten();

            let source = match failure {
                HandshakeFailure::TimedOut => WorkerError::Timeout(HANDSHAKE_TIMEOUT),
                HandshakeFailure::Refused(err) => err,
            };

            Err(match (status, spec.binary()) {
                // The worker exited instead of answering, so its own exit is the useful part of
                // the story.
                (Some(status), Some(binary)) => ServerError::StartupExited {
                    binary: binary.clone(),
                    status: status.to_string(),
                    hint: Hint(transport_hint(spec)),
                },
                // An attached worker has no exit status this process can see; only its silence
                // on the wire.
                _ => ServerError::WorkerProtocol {
                    backend: name.to_string(),
                    source,
                },
            })
        }
    }
}

/// Start a worker and complete its handshake.
///
/// `index` is the slot's position in the pool, and it is needed for exactly one thing: the
/// per-slot suffix on a unix socket path, which is what gives each worker an address of its own
/// rather than putting them all behind one queue.
fn boot_spawned(
    name: &str,
    spec: &Spec,
    index: u32,
    guard: &LifecycleGuard,
    handler: Option<&Arc<MethodRegistry>>,
) -> Result<Ready> {
    let Launch::Spawn {
        binary,
        args,
        working_dir,
        env,
        transport,
        ..
    } = &spec.launch
    else {
        unreachable!("boot_spawned is only reached for a spawned pool")
    };

    // A socket-transport worker is connected to a listener that has to exist *before* the
    // process starts, because the address is handed to it as an environment variable
    // rather than discovered by it. So the listener is bound first and the worker spawns
    // into a connection that is already waiting.
    let listener = WorkerListener::bind(spec, index)?;
    let socket_address = match &listener {
        Some(listener) => Some(listener.address()?),
        None => None,
    };

    let mut command = Command::new(binary);
    command
        .stdin(if transport.is_pipe() {
            Stdio::piped()
        } else {
            // With the protocol on a socket, stdio is free to be nothing. Leaving stdin
            // piped would also work, but a worker that reads the wrong stream should fail
            // loudly rather than block, and `null` gives it immediate end-of-file.
            Stdio::null()
        })
        .stdout(if transport.is_pipe() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        // Piped, never nulled: PHP writes its diagnostics here, and a worker
        // whose stderr pipe fills up blocks for ever on its next write.
        .stderr(Stdio::piped());
    command.args(args);
    if let Some(dir) = working_dir {
        command.current_dir(dir);
    }
    for (key, value) in env {
        command.env(key, value);
    }
    // Set after the configured `env`, so it cannot be shadowed — the same ordering the socket
    // address below uses, and for the same reason: a value an operator could overwrite would be a
    // variable that sometimes reports the wrong server.
    command.env(volucer_core::config::VERSION_ENV, crate::build::VERSION);
    if let Some(address) = &socket_address {
        // Set *after* the configured `env`, so the address always wins. A configuration
        // that carries its own `env` map would otherwise be able to point the worker at a
        // different address than the listener we are about to accept on, and the failure
        // would be a worker that boots and never connects.
        //
        // The value is the transport's own spelling — `host:port` or `unix:/path` — and it is
        // the SDK that decides which wrapper to open. That keeps one variable with one meaning
        // rather than a second variable whose absence would mean "TCP".
        command.env(WORKER_SOCKET_ENV, address);
    }
    configure_command(&mut command);

    let mut child = command.spawn().map_err(|source| ServerError::Spawn {
        binary: binary.clone(),
        source,
    })?;

    // Adopt before waiting on anything, so a failure below cannot leave an
    // orphan holding a pipe.
    if let Err(source) = guard.adopt(&child) {
        // A process that exited before it could be adopted is not a lifecycle
        // failure, and saying "job object" would hide the real reason.
        return Err(match child.try_wait() {
            Ok(Some(status)) => ServerError::StartupExited {
                binary: binary.clone(),
                status: status.to_string(),
                hint: Hint(transport_hint(spec)),
            },
            _ => ServerError::Spawn {
                binary: binary.clone(),
                source,
            },
        });
    }

    if let Some(stderr) = child.stderr.take() {
        drain_stderr(name.to_string(), child.id(), stderr);
    }

    let channel = match listener {
        // The worker connects back to the listener bound above. The timeout matters: a
        // worker that never connects must fail here rather than hold startup for ever, and
        // it is bounded by the same budget the handshake gets.
        Some(listener) => {
            let accepted =
                match accept_with_deadline(&listener, spec, &mut child, HANDSHAKE_TIMEOUT) {
                    Ok(stream) => stream,
                    Err(source) => {
                        let _ = crate::lifecycle::kill(&mut child);
                        let _ = child.wait();
                        return Err(source);
                    }
                };
            // The protocol is latency-sensitive and its frames are small; Nagle would
            // only add delay to a request that is written frame by frame. A unix socket has
            // no Nagle to disable — `set_nodelay` on one is `ENOPROTOOPT`, which is why this
            // is only done on the TCP arm rather than ignored on both.
            if let Stream::Tcp(tcp) = &accepted {
                let _ = tcp.set_nodelay(true);
            }
            let reader = accepted
                .try_clone_stream()
                .map_err(|source| ServerError::Spawn {
                    binary: binary.clone(),
                    source,
                })?;
            BoundedChannel::new(accepted, reader)
        }
        None => {
            let stdin = child.stdin.take().expect("stdin was piped");
            let stdout = child.stdout.take().expect("stdout was piped");
            BoundedChannel::new(stdin, stdout)
        }
    };

    finish_boot(name, spec, Link::Process(child), channel, handler)
}

/// Why a boot handshake did not produce a `HELLO_ACK`.
#[derive(Debug)]
enum HandshakeFailure {
    /// The deadline expired. The reader thread is still blocked and the caller
    /// must kill the worker to end it.
    TimedOut,
    /// The worker answered with something unusable, or its pipe ended first.
    Refused(WorkerError),
}

/// Run the handshake on its own thread, so that it can have a deadline.
/// A pipe read has no portable timeout, so the deadline is enforced from the
/// outside: this gives up, the caller kills the worker, and the pending read then
/// fails — which is how the reader thread gets to exit. Nothing is closed from
/// here, because closing the pipes would need a handle this function deliberately
/// does not have.
fn handshake_with_timeout(
    mut channel: Channel,
    timeout: Duration,
    policy: Checksum,
) -> std::result::Result<(Channel, HelloAck), HandshakeFailure> {
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        // The offer is recorded *before* the handshake because that is the frame it travels in:
        // `false` means no `HELLO` is ever sent offering an unchecked frame, `true` means one is
        // sent and the option is adopted only if the worker answers with it accepted.
        channel.offer_no_checksum(policy == Checksum::Optional);
        let result = channel.handshake();
        // Hand the channel back so the winner can keep using it. A send failure
        // means the receiver gave up, and then the channel is dropped with this
        // thread, closing the pipes.
        let _ = sender.send((channel, result));
    });

    match receiver.recv_timeout(timeout) {
        Ok((channel, Ok(ack))) => Ok((channel, ack)),
        Ok((_channel, Err(err))) => Err(HandshakeFailure::Refused(err)),
        Err(RecvTimeoutError::Timeout) => Err(HandshakeFailure::TimedOut),
        Err(RecvTimeoutError::Disconnected) => Err(HandshakeFailure::Refused(WorkerError::Closed(
            "the handshake thread ended without answering".to_string(),
        ))),
    }
}

/// Log a worker's `stderr` until it ends.
///
/// This thread exists so the pipe cannot fill up, and it logs because for PHP
/// this is where errors go: the SDK points `display_errors` at `stderr` precisely
/// so that diagnostics cannot corrupt the protocol stream. For a worker written
/// without the SDK it is simply free-form output.
///
/// It must run on its own thread, and that is not incidental: reading `stderr` to
/// end-of-file blocks until the worker exits, so doing it inline would deadlock
/// the boot — the worker waits for `HELLO` while we wait for a line that will
/// never come. This is the hazard "drain `STDERR` for every
/// backend, always", seen from the other side: draining it on the caller's thread
/// is the same bug with a different shape.
fn drain_stderr(backend: String, pid: u32, stderr: ChildStderr) {
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stderr);
        let mut line = Vec::new();
        loop {
            line.clear();
            match reader.read_until(b'\n', &mut line) {
                Ok(0) => break,
                Ok(_) => {
                    let text = String::from_utf8_lossy(&line);
                    let text = text.trim_end();
                    if !text.is_empty() {
                        tracing::debug!(backend = %backend, pid, line = %text, "worker stderr");
                    }
                }
                Err(err) => {
                    tracing::debug!(backend = %backend, pid, %err, "worker stderr ended");
                    break;
                }
            }
        }
    });
}

#[cfg(test)]
use std::collections::BTreeMap;
#[cfg(test)]
#[cfg(test)]
use volucer_core::config::WorkerTransport;

/// The pieces of a worker specification `boot` does not read.
///
/// `boot` resolves no program and parses no command — those checks already happened in
/// `Spec::from_config` — so a test driving startup failures can leave the binary as a path
/// that does not exist.
#[cfg(test)]
fn bare_spec(transport: WorkerTransport) -> Spec {
    Spec {
        launch: Launch::Spawn {
            binary: std::path::PathBuf::from("php"),
            args: Vec::new(),
            working_dir: None,
            env: BTreeMap::new(),
            transport,
            socket: None,
        },
        checksum: Checksum::Optional,
        grpc_services: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use volucer_core::config::WorkerTransport;

    /// A socket-transport worker that never connects back fails startup, promptly.
    ///
    /// `boot` originally called a bare `listener.accept()`, which has no deadline. A worker
    /// that connects to the wrong address therefore hung startup for ever with nothing
    /// logged, because no step in the sequence reports a *missing* connection. The test
    /// asserts the two things that make the difference visible: the error names the cause,
    /// and it arrives in about a second rather than never.
    ///
    /// A short timeout is passed instead of the real 30 s handshake budget so the test stays
    /// fast; the deadline arithmetic is the same either way.
    #[test]
    fn a_socket_worker_that_never_connects_times_out_rather_than_hanging() {
        let listener = WorkerListener::Tcp(TcpListener::bind(("127.0.0.1", 0)).unwrap());
        // A process that has already exited, standing in for the worker that failed to
        // connect — this is the case where waiting out the deadline would be the wrong
        // answer, and the one `boot` must recognise by exit status.
        let mut child = Command::new(if cfg!(windows) { "cmd" } else { "sh" })
            .args(if cfg!(windows) {
                vec!["/C", "exit 3"]
            } else {
                vec!["-c", "exit 3"]
            })
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();

        let started = Instant::now();
        let err = accept_with_deadline(
            &listener,
            &bare_spec(WorkerTransport::Socket),
            &mut child,
            Duration::from_secs(5),
        )
        .expect_err("a listener nobody connects to must not succeed");
        let elapsed = started.elapsed();

        let _ = crate::lifecycle::kill(&mut child);
        let _ = child.wait();

        assert!(
            elapsed < Duration::from_secs(3),
            "the failure must be reported as soon as the worker is known to be gone, \
         not after the full budget: took {elapsed:?}"
        );
        assert!(
            matches!(err, ServerError::StartupExited { .. }),
            "a worker that exited cannot connect, so its exit status is the useful part: {err:?}"
        );
        assert!(
            err.to_string().contains("php"),
            "the error names the binary that failed: {err}"
        );
    }

    /// The timeout path itself: a live worker that never dials back.
    #[test]
    fn a_live_socket_worker_that_never_connects_hits_the_startup_timeout() {
        let tcp = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = tcp.local_addr().unwrap();
        let listener = WorkerListener::Tcp(tcp);

        // Long enough to outlive the deadline below, and silent on both streams.
        let mut child = Command::new(if cfg!(windows) { "cmd" } else { "sleep" })
            .args(if cfg!(windows) {
                vec!["/C", "ping -n 6 127.0.0.1 >NUL"]
            } else {
                vec!["5"]
            })
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();

        let err = accept_with_deadline(
            &listener,
            &bare_spec(WorkerTransport::Socket),
            &mut child,
            Duration::from_millis(200),
        )
        .expect_err("a live worker that never connects must not hold startup for ever");

        let _ = crate::lifecycle::kill(&mut child);
        let _ = child.wait();

        match err {
            ServerError::StartupTimeout {
                address: found,
                timeout,
                ..
            } => {
                assert_eq!(
                    found,
                    address.to_string(),
                    "the error names the address waited on"
                );
                assert_eq!(timeout, Duration::from_millis(200));
            }
            other => panic!("expected a startup timeout, got {other:?}"),
        }
    }
}
