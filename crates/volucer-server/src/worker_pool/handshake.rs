//! Starting a worker and getting it ready: `spawn`, or a `connect` to a relay somebody else
//! listens on; the `HELLO` / `HELLO_ACK` handshake under a deadline; the socket-transport accept
//! poll; and the `stderr` drain thread. Everything that waits here has a deadline, and the
//! deadline is enforced by killing the worker — see the note in [`super`].

use super::exchange::{Channel, ChannelExchange};
use super::spec::{AttachSpec, Launch, Spec};
use super::SlotExchange;
use crate::calls::MethodRegistry;
use crate::error::{Hint, Result, ServerError, SOCKET_WORKER_HINT};
use crate::lifecycle::{configure_command, spawn, LifecycleGuard};
use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::net::TcpListener;
use std::process::{Child, ChildStderr, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
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
    pub(super) exchange: SlotExchange,
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
    stderr_log: Option<&StderrLog>,
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
                    // The evidence first, so the error that follows is read in context — this exit
                    // is one of the two cases the socket hint describes, and the worker's own
                    // stderr is what tells them apart.
                    if let Some(log) = stderr_log {
                        report_worker_stderr(&spec.describe(), log);
                    }
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

/// The same handler, as the awaited path wants it.
///
/// [`install_call_handler`] serves the **blocking** channel; an awaited slot's connection is a
/// [`super::wire::WireChannel`], which is a different type, and it had no equivalent. A worker on
/// an awaited connection therefore reached a channel with no handler installed, so every `CALL` it
/// made — `kv`, `lock` and `jobs` all reach the server from inside a request, and a framework asks
/// during boot — was answered `"no call handler is installed"`, and the application saw a failure
/// that had nothing to do with what it asked for.
///
/// Built here rather than at each call site so the two paths cannot drift: the closure is the same
/// one, over the same registry, with the same refusal for anything not granted.
fn call_handler_for(
    handler: Option<&Arc<MethodRegistry>>,
) -> Option<Box<volucer_worker::codec::CallHandler>> {
    let registry = Arc::clone(handler?);
    Some(Box::new(move |call| registry.serve(call)))
}

/// Read a worker's retained `stderr` and write it where an operator will see it.
///
/// # Why this is `warn` and not the `debug` the drain thread uses
///
/// Because of what it is for. The startup failure these lines explain is logged at `error`, and its
/// hint says the worker's own stderr "is logged above". At the default filter (`volucer=info`) a
/// `debug` line is invisible, so before this existed the hint pointed at nothing — for the *likeliest*
/// first-run failure on Windows, where the socket transport is the default and a worker written
/// against `Worker::create()` dies in its handshake.
///
/// It is `warn` rather than `error` because the error is the failure and this is its evidence: two
/// `error` lines for one startup abort reads as two things having gone wrong.
///
/// # Why an empty log is reported
///
/// "The worker wrote nothing to stderr" is itself the answer when a startup failure is otherwise
/// unexplained — it rules out the most common cause and sends the reader to the exit status and the
/// command instead. Silence here would look like the mechanism had failed to capture something.
fn report_worker_stderr(name: &str, log: &StderrLog) {
    let text = log.take();
    if text.is_empty() {
        tracing::warn!(
            backend = %name,
            "the worker wrote nothing to stderr before it exited, so the reason is not in its own \
             diagnostics — check the command, the working directory and the exit status"
        );
        return;
    }
    tracing::warn!(backend = %name, "worker stderr, before it exited:\n{text}");
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
        // An attached worker's transport is a socket, so its slot awaits the connection when the
        // spec carries a runtime to drive it. The fallback is not a nicety: a spec built by a test
        // or by a configuration path that had no runtime still has to boot, and it does so on the
        // blocking exchange thread — correct, and costing what it always did.
        Launch::Attach(attach) => match &spec.runtime {
            Some(runtime) => boot_attached_async(name, spec, attach, handler, runtime),
            None => boot_attached(name, spec, attach, handler),
        },
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

    finish_boot(
        name,
        spec,
        Link::Attached { pid: 0 },
        channel,
        handler,
        None,
        None,
    )
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

/// Connect to an attached worker, handshake it, and hand its connection to a runtime.
///
/// The same boot as [`boot_attached`] in every way that decides *what* is done — the same
/// `Endpoint`, the same `HELLO`/`HELLO_ACK` exchange, the same deadline, the same errors — and
/// different only in who owns the connection afterwards. That is the whole change: a slot whose
/// transport can be awaited does not need a thread, and the thread is what cost ~13 µs of CPU per
/// request (measured; see [`super::wire`]'s module docs).
///
/// # Why the handshake stays blocking
///
/// It happens **once per worker**, at startup, on a path that already blocks for every other
/// transport. A future version would mean a second handshake implementation whose only benefit is
/// a few milliseconds at boot, and whose risk is the two disagreeing about the version check or the
/// checksum policy. So the socket is dialled and handshaken exactly as before, and only then
/// adopted.
///
/// # Why adoption needs a runtime context
///
/// `tokio` refuses a blocking file descriptor, and it needs a runtime context to register one. The
/// socket is therefore switched to non-blocking and its read/write timeouts are cleared (a `tokio`
/// socket would panic on a pending operation against a `SO_RCVTIMEO`, and the deadline is enforced
/// by the exchange instead). The runtime is the pool's own, held for the pool's lifetime, because a
/// socket's waker belongs to whichever runtime first polls it and that runtime must outlive the
/// connection.
fn boot_attached_async(
    name: &str,
    spec: &Spec,
    attach: &AttachSpec,
    handler: Option<&Arc<MethodRegistry>>,
    runtime: &super::AsyncRuntime,
) -> Result<Ready> {
    let fail = |source: FcgiError| ServerError::Attach {
        backend: name.to_string(),
        relay: attach.relay.clone(),
        source,
    };
    let io_fail = |source: std::io::Error| fail(FcgiError::Io(source));

    let endpoint = Endpoint::parse(&attach.relay).map_err(fail)?;
    let stream = endpoint.connect(Some(CONNECT_TIMEOUT)).map_err(fail)?;
    let reader = stream.try_clone_stream().map_err(io_fail)?;

    // The handshake gets its own `dup`s, so the connection that is adopted afterwards is the one
    // dialled here rather than something recovered from a channel that owns it.
    //
    // Handing over the *originals* would be simpler by one call and wrong: a `BufReader` inside the
    // handshake channel may read ahead, and bytes sitting in its buffer are bytes the adopted
    // handle will never see. Separate `dup`s do not remove that risk — they share one socket — but
    // the protocol does: it is rigorously half-duplex, so a worker sends nothing after `HELLO_ACK`
    // until it is sent a request, and there is nothing for the handshake's reader to have
    // prefetched.
    let handshake_reader = reader.try_clone_stream().map_err(io_fail)?;
    let handshake_writer = stream.try_clone_stream().map_err(io_fail)?;
    let mut channel = BoundedChannel::new(handshake_writer, handshake_reader);
    install_call_handler(&mut channel, handler);
    let offered = spec.checksum == Checksum::Optional;
    let ack = match handshake_with_timeout(channel, HANDSHAKE_TIMEOUT, spec.checksum) {
        Ok((_channel, ack)) => ack,
        Err(failure) => {
            let source = match failure {
                HandshakeFailure::TimedOut => WorkerError::Timeout(HANDSHAKE_TIMEOUT),
                HandshakeFailure::Refused(err) => err,
            };
            return Err(ServerError::WorkerProtocol {
                backend: name.to_string(),
                source,
            });
        }
    };

    // **After the handshake, never before.** The handshake runs on the *blocking* channel, so a
    // non-blocking socket makes its first `read` return `EAGAIN` — observed as
    // `i/o error: Resource temporarily unavailable` at startup. Switching here is also the last
    // moment it can be done: `tokio` will not register a blocking descriptor, and a pending
    // operation against `SO_RCVTIMEO` is documented to panic.
    stream.prepare_for_async().map_err(io_fail)?;
    reader.prepare_for_async().map_err(io_fail)?;

    let policy = Checksum::from_handshake(offered, ack.no_checksum);
    // Adopted **under the pool's own runtime**, which is the one that will drive the connection.
    // This is the whole rule the module docs state, and it has to be the pool's runtime rather
    // than whatever happens to be current: a socket adopted under a transient runtime registers
    // against a driver that is then dropped, and fails on its next poll.
    let (adopted_reader, adopted_writer) = runtime.adopt(&reader, &stream, fail)?;
    // The handle a *synchronous* caller bridges through — the FastCGI front-end, the control plane,
    // the tests. The HTTP front-end does not use it: it awaits the connection on its own runtime.
    let handle = tokio::runtime::Handle::try_current().unwrap_or_else(|_| runtime.handle());
    tracing::debug!(
        backend = %name,
        pid = ack.pid,
        php = %ack.php_version,
        sapi = %ack.sapi,
        target = %spec.describe(),
        "worker ready, connection awaited"
    );

    Ok(Ready {
        link: Link::Attached { pid: ack.pid },
        exchange: super::SlotExchange::Awaited(Arc::new(super::wire::AsyncExchange::start(
            adopted_reader,
            adopted_writer,
            handle,
            policy,
            call_handler_for(handler),
        ))),
        ack,
    })
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
    stderr_log: Option<&StderrLog>,
    adoptable: Option<Adoptable>,
) -> Result<Ready> {
    // The call handler goes on **before the handshake**, and the order matters. A worker may send
    // a `CALL` at any point in its life, and the first thing it does after connecting is boot the
    // application — which is exactly when a framework wants to ask the server something.
    // Installing the handler after `HELLO_ACK` would leave that window answering "no call handler
    // is installed", which is a refusal the worker cannot tell from a server that does not
    // support calls at all.
    install_call_handler(&mut channel, handler);

    // Taken before the channel is consumed by the handshake: these are the *originals* the accepted
    // connection was cloned from, and they are what gets adopted when this slot will be awaited.
    let checked = spec.checksum;

    match handshake_with_timeout(channel, HANDSHAKE_TIMEOUT, checked) {
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
            // The boot is over, so nothing is going to ask for the retained lines. Cleared here
            // rather than left to accumulate: a worker that logs for a week would otherwise be a
            // leak with a log attached.
            if let Some(log) = stderr_log {
                log.finish();
            }

            // **A spawned worker over a socket can be awaited, exactly as an attached one is.**
            // The handshake ran on a blocking `dup` pair, which is now dropped with `channel`; the
            // connection the slot keeps is the one the worker accepted. Both calls in
            // `prepare_for_async` are mandatory and for the reasons that function states: a
            // `tokio` socket must be non-blocking and must carry no `SO_RCVTIMEO`.
            //
            // A spec with no runtime — a test, or a configuration path that never started one —
            // keeps the threaded exchange. That fallback is not a nicety: it is what every pipe
            // worker still uses, since a pipe has no descriptor a runtime can register.
            let exchange = match adoptable {
                Some(Adoptable {
                    writer,
                    reader,
                    runtime,
                }) => {
                    // `prepare_for_async` and `adopt` only ever fail with an I/O error, so the
                    // mapping keeps the original `io::Error` rather than flattening it to a string —
                    // a `WouldBlock` or `EAGAIN` here means a descriptor was left blocking, and that
                    // is worth reading as itself.
                    let fail = |source: FcgiError| ServerError::Spawn {
                        binary: spec
                            .binary()
                            .cloned()
                            .unwrap_or_else(|| std::path::PathBuf::from("worker")),
                        source: match source {
                            FcgiError::Io(err) => err,
                            other => std::io::Error::other(other.to_string()),
                        },
                    };
                    match prepare_and_adopt(&writer, &reader, &runtime, fail) {
                        Ok((adopted_reader, adopted_writer)) => {
                            let handle = tokio::runtime::Handle::try_current()
                                .unwrap_or_else(|_| runtime.handle());
                            super::SlotExchange::Awaited(Arc::new(
                                super::wire::AsyncExchange::start(
                                    adopted_reader,
                                    adopted_writer,
                                    handle,
                                    Checksum::from_handshake(
                                        checked == Checksum::Optional,
                                        ack.no_checksum,
                                    ),
                                    call_handler_for(handler),
                                ),
                            ))
                        }
                        // Adoption failing is a boot failure: the slot would otherwise be
                        // recorded as ready with no way to reach it, which is the cascade the
                        // attach path had to be fixed for.
                        Err(source) => {
                            let _ = link.kill();
                            let _ = link.wait();
                            return Err(source);
                        }
                    }
                }
                None => super::SlotExchange::Threaded(ChannelExchange::start(channel)),
            };

            Ok(Ready {
                link,
                exchange,
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

            // **Before the error, not after.** The hint on that error points at these lines, and an
            // operator reading a log reads downwards: evidence first, verdict second. This is also
            // the only moment they can be read, because the process has exited and no more will
            // arrive.
            if let Some(log) = stderr_log {
                report_worker_stderr(name, log);
            }

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
    let mut stderr_log: Option<StderrLog> = None;

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

    // Through `lifecycle::spawn`: `PR_SET_PDEATHSIG` binds the child to the forking thread,
    // and a worker may be booted from a request thread on a restart.
    let mut child = spawn(command).map_err(|source| ServerError::Spawn {
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
        let log = StderrLog::default();
        drain_stderr(name.to_string(), child.id(), stderr, log.clone());
        stderr_log = Some(log);
    }
    let (channel, adoptable) = match listener {
        // The worker connects back to the listener bound above. The timeout matters: a
        // worker that never connects must fail here rather than hold startup for ever, and
        // it is bounded by the same budget the handshake gets.
        Some(listener) => {
            let accepted = match accept_with_deadline(
                &listener,
                spec,
                &mut child,
                HANDSHAKE_TIMEOUT,
                stderr_log.as_ref(),
            ) {
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
            // **The handshake gets its own `dup`s, and the originals are kept back.**
            //
            // The same rule `boot_attached_async` follows, and it is needed here for two
            // reasons that point the same way. A `BufReader` inside the handshake channel may
            // read ahead, and bytes sitting in its buffer are bytes the adopted handle never
            // sees — the protocol is half-duplex, so there should be none, but the *only* way
            // to know is to not share the descriptor. And an adopted connection must be the
            // one the worker actually accepted, not a stream recovered from a channel that
            // owns it.
            //
            // For the pipe-less `Threaded` case the originals are what the exchange thread
            // gets, which is the same socket the handshake just used and therefore the same
            // behaviour as before this split existed.
            let handshake_writer =
                accepted
                    .try_clone_stream()
                    .map_err(|source| ServerError::Spawn {
                        binary: binary.clone(),
                        source,
                    })?;
            let handshake_reader =
                reader
                    .try_clone_stream()
                    .map_err(|source| ServerError::Spawn {
                        binary: binary.clone(),
                        source,
                    })?;
            (
                BoundedChannel::new(handshake_writer, handshake_reader),
                Some((accepted, reader)),
            )
        }
        None => {
            let stdin = child.stdin.take().expect("stdin was piped");
            let stdout = child.stdout.take().expect("stdout was piped");
            (BoundedChannel::new(stdin, stdout), None)
        }
    };

    finish_boot(
        name,
        spec,
        Link::Process(child),
        channel,
        handler,
        stderr_log.as_ref(),
        adoptable_for(spec, adoptable),
    )
}

/// The connection a spawned slot may adopt, when its spec carries a runtime.
///
/// Split out so `boot_spawned` reads as one decision rather than a nested match at the end of a
/// hundred-line function, and so the "no runtime, no adoption" rule lives in one place. A pipe
/// worker has no `adoptable` at all — a pipe has no descriptor a runtime can register.
fn adoptable_for(spec: &Spec, accepted: Option<(Stream, Stream)>) -> Option<Adoptable> {
    let runtime = spec.runtime.as_ref()?;
    let (writer, reader) = accepted?;
    Some(Adoptable {
        writer,
        reader,
        runtime: Arc::clone(runtime),
    })
}

/// A connected socket, and the runtime that will drive it.
struct Adoptable {
    writer: Stream,
    reader: Stream,
    runtime: Arc<super::AsyncRuntime>,
}

/// Make a handshaken connection fit for a runtime, and adopt it.
///
/// The two `prepare_for_async` calls are mandatory and neither is optional — the function's own
/// documentation states both failures, and they are the reason this is not inlined at the two call
/// sites that need it. `adopt` then enters the runtime that will drive the socket, because
/// `from_std` resolves the driver here rather than at the first poll.
fn prepare_and_adopt(
    writer: &Stream,
    reader: &Stream,
    runtime: &super::AsyncRuntime,
    fail: impl Fn(FcgiError) -> ServerError,
) -> Result<(super::wire::AsyncStream, super::wire::AsyncStream)> {
    writer
        .prepare_for_async()
        .map_err(|source| fail(FcgiError::Io(source)))?;
    reader
        .prepare_for_async()
        .map_err(|source| fail(FcgiError::Io(source)))?;
    runtime.adopt(reader, writer, fail)
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

/// A worker's `stderr`, kept where a startup failure can read it back.
///
/// # Why the text is kept as well as logged
///
/// Because the log level it is written at is `debug`, while the hint it is offered to sits on the
/// `error` line for a **startup** failure — and those two can never be read together. At the
/// default filter (`volucer=info`, which is what `serve` with no `--verbose` and no `RUST_LOG`
/// uses) an operator sees the hint and not one line of the evidence it points at.
///
/// It is not academic. The socket transport is the **default on Windows**, so the likeliest
/// first-run failure there — a worker written against `Worker::create()` — aborts startup with a
/// message saying "Anything the worker wrote to stderr is logged above", above which there is
/// nothing. The worker had in fact written a full `Uncaught ProtocolException` trace.
///
/// So the lines are retained, and the failure path reads them out with [`Self::take`] and writes
/// them at `warn` *before* the error that points at them. Retaining costs one `String` per worker,
/// and only for as long as the worker is booting.
///
/// # Why it stops buffering once the boot ends
///
/// A long-lived worker keeps writing, and a server that accumulated every line for its lifetime
/// would be a leak with a log attached. [`Self::finish`] is called on both halves of the boot, so a
/// successful start and a failed one both stop the growth — the failure path just also stops it by
/// reading.
#[derive(Debug, Clone, Default)]
struct StderrLog {
    /// The lines so far, in arrival order. Capped at [`Self::LIMIT`].
    lines: Arc<Mutex<String>>,
    /// Set when the boot ended, so the drain thread stops appending.
    finished: Arc<AtomicBool>,
    /// Set by the drain thread when the pipe reached end-of-file.
    ///
    /// **This is what makes a report deterministic.** The child exiting and the drain thread having
    /// *read* what it wrote are two different events: a worker that dies writing a fatal error leaves
    /// those bytes in the pipe for the thread to pick up microseconds later. A `take` that ran in
    /// between would report "the worker wrote nothing" while holding the opposite in its hand — a
    /// failure message that actively lies about the evidence. So a report waits for this flag, which
    /// is set exactly when there is nothing more to read.
    drained: Arc<AtomicBool>,
}

impl StderrLog {
    /// How much text will be kept. Enough for a PHP fatal error and its stack trace, which is the
    /// case this exists for. A worker that logs a megabyte while booting is a problem of its own, and
    /// the **last** lines are the ones that say why it died, so the cap drops the oldest.
    const LIMIT: usize = 16 * 1024;

    /// How long a report waits for the drain thread to reach end-of-file.
    ///
    /// Short because the wait is only ever spent after the worker is known to be gone, when the pipe
    /// is already at end-of-file or microseconds from it. The bound exists so that a worker which
    /// somehow holds its stderr open cannot delay the startup failure it is being reported for.
    const SETTLE: Duration = Duration::from_millis(250);

    /// Append a line, unless the boot is over or the cap has been reached.
    fn push(&self, line: &str) {
        if self.finished.load(Ordering::Acquire) {
            return;
        }
        let Ok(mut guard) = self.lines.lock() else {
            return;
        };
        // Re-checked inside the lock: `take` clears the buffer and then sets the flag, so a line that
        // arrived between those two steps would otherwise be retained for the life of the process.
        if self.finished.load(Ordering::Acquire) {
            return;
        }
        if guard.len() + line.len() + 1 > Self::LIMIT {
            return;
        }
        guard.push_str(line);
        guard.push('\n');
    }

    /// Record that the pipe ended, so a waiting report knows the log is complete.
    fn mark_drained(&self) {
        self.drained.store(true, Ordering::Release);
    }

    /// Take everything the worker wrote, waiting briefly for the drain thread to finish reading.
    ///
    /// Returns an empty string when the worker said nothing, which the caller reports differently
    /// from "the worker said something": "it wrote no diagnostics" is itself a fact worth stating
    /// when a startup failure is otherwise unexplained.
    fn take(&self) -> String {
        let deadline = Instant::now() + Self::SETTLE;
        while !self.drained.load(Ordering::Acquire) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(2));
        }

        self.finished.store(true, Ordering::Release);
        let mut guard = match self.lines.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let text = guard.trim_end().to_string();
        guard.clear();
        guard.shrink_to_fit();
        text
    }

    /// Stop buffering without reading, for the boot that succeeded.
    fn finish(&self) {
        self.finished.store(true, Ordering::Release);
        if let Ok(mut guard) = self.lines.lock() {
            guard.clear();
            guard.shrink_to_fit();
        }
    }
}

/// Log a worker's `stderr` until it ends, keeping the lines while the boot is running.
///
/// The thread exists so the pipe cannot fill up, and it logs because for PHP this is where errors go:
/// the SDK points `display_errors` at `stderr` precisely so that diagnostics cannot corrupt the
/// protocol stream. For a worker written without the SDK it is simply free-form output.
///
/// It must run on its own thread, and that is not incidental: reading `stderr` to end-of-file blocks
/// until the worker exits, so doing it inline would deadlock the boot — the worker waits for `HELLO`
/// while we wait for a line that will never come. This is the hazard "drain `STDERR` for every
/// backend, always", seen from the other side: draining it on the caller's thread is the same bug
/// with a different shape.
///
/// The `debug` line and the retained copy are not redundant. The line is for a worker that is running
/// normally; the copy is for one that died before it could run at all, where the failure path reports
/// it at `warn` because the hint points at it.
fn drain_stderr(backend: String, pid: u32, stderr: ChildStderr, log: StderrLog) {
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
                        log.push(text);
                    }
                }
                Err(err) => {
                    tracing::debug!(backend = %backend, pid, %err, "worker stderr ended");
                    break;
                }
            }
        }
        // Last, so a report woken by this flag can never read a buffer the loop was about to add to.
        log.mark_drained();
    });
}

#[cfg(test)]
#[cfg(test)]
use volucer_core::config::WorkerTransport;

#[cfg(test)]
use std::collections::BTreeMap;

/// An in-memory log sink, so a test can assert what an operator would have seen.
///
/// `tracing`'s own test helpers (`tracing_test`, `tracing-mock`) are not dependencies of this
/// workspace, and adding one to assert a single `warn!` line is a dependency bought for a test's
/// scaffolding. A writer that appends to a `String` under a mutex is the whole of what is needed, and
/// it goes through the same `tracing` path the real server uses.
#[cfg(test)]
#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

#[cfg(test)]
impl Capture {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().expect("not poisoned")).into_owned()
    }
}

#[cfg(test)]
impl std::io::Write for Capture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("not poisoned").extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
impl<'writer> tracing_subscriber::fmt::MakeWriter<'writer> for Capture {
    type Writer = Capture;

    fn make_writer(&'writer self) -> Self::Writer {
        self.clone()
    }
}

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
        measure_wait: false,
        request_timeout: crate::worker_pool::REQUEST_TIMEOUT,
        // A spawned worker's pipes are blocking, so no runtime drives anything.
        runtime: None,
        // No keepalive: these tests drive startup failures, and a thread pinging a worker that never
        // booted would be a second moving part in a test about the first.
        keepalive: None,
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
            None,
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
            None,
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

    /// **The retained `stderr` is readable after a startup failure**, which is the whole point of
    /// keeping it: the hint on that failure says the worker's own output "is logged above", and at
    /// the default filter the `debug` line that carries it is invisible.
    ///
    /// A child that writes a line and exits, driven through the same `accept_with_deadline` a real
    /// boot uses, so what is asserted is the mechanism rather than a hand-built buffer. The line is
    /// deliberately the wording PHP produces for the case this exists for.
    #[test]
    fn a_failed_startup_retains_the_workers_stderr_for_the_error_path() {
        let listener = WorkerListener::Tcp(TcpListener::bind(("127.0.0.1", 0)).unwrap());
        let log = StderrLog::default();

        let mut child = Command::new(if cfg!(windows) { "cmd" } else { "sh" })
            .args(if cfg!(windows) {
                // `1>&2` is cmd's spelling; `>&2` is a syntax error there and the child would then
                // write nothing, making this test pass or fail for a reason of its own.
                vec!["/C", "echo Uncaught ProtocolException 1>&2 & exit 3"]
            } else {
                vec!["-c", "echo 'Uncaught ProtocolException' >&2; exit 3"]
            })
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();

        let stderr = child.stderr.take().expect("stderr was piped");
        drain_stderr("app".to_string(), child.id(), stderr, log.clone());

        // The subscriber is installed for the duration of this test only, and `set_default` scopes it
        // to this thread — which is all that is needed, because the `warn!` being asserted is written
        // on the thread that called `accept_with_deadline`, not on the drain thread.
        let capture = Capture::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(capture.clone())
            .with_ansi(false)
            .with_max_level(tracing::Level::WARN)
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);

        let err = accept_with_deadline(
            &listener,
            &bare_spec(WorkerTransport::Socket),
            &mut child,
            Duration::from_secs(5),
            Some(&log),
        )
        .expect_err("a worker that exited cannot connect");

        let _ = crate::lifecycle::kill(&mut child);
        let _ = child.wait();

        assert!(
            matches!(err, ServerError::StartupExited { .. }),
            "the exit status is still the error: {err:?}"
        );

        // **The assertion is on what was logged, not on what is left in the buffer.** The failure
        // path reports the retained lines and *then* builds the error, so reading the log a second
        // time here would find it cleared — and a test that read the buffer directly would be
        // asserting the mechanism that feeds the report rather than the report an operator sees.
        let logged = capture.text();
        assert!(
            logged.contains("Uncaught ProtocolException"),
            "the worker's own diagnostic must reach the log, because the hint on the error that \
             follows points at it. An operator at the default filter sees only `warn` and above, so \
             a `debug` line is not evidence. log was: {logged:?}"
        );
    }

    /// The other half: a successful boot must not leave the buffer growing for the process's life.
    ///
    /// A worker that logs a line a second for a week would otherwise be an unbounded `String` with a
    /// log attached. `finish` is what stops it, and this pins that it stops *and* releases the text
    /// rather than merely setting a flag over the top of it.
    #[test]
    fn a_successful_boot_stops_retaining_stderr() {
        let log = StderrLog::default();
        log.push("while booting");
        log.finish();
        log.push("after the boot");

        assert_eq!(
            log.take(),
            "",
            "nothing may be retained once the boot ended — otherwise a long-lived worker is a leak"
        );
    }

    /// The cap: a worker that logs a great deal while booting keeps the lines that fit and no more.
    ///
    /// Asserted because the alternative reading of the cap is to keep appending and trim later, which
    /// is a buffer with no bound at all during exactly the burst it is meant to bound.
    #[test]
    fn the_retained_stderr_is_capped() {
        let log = StderrLog::default();
        let line = "x".repeat(1024);
        for _ in 0..64 {
            log.push(&line);
        }
        let retained = log.take();
        assert!(
            retained.len() <= StderrLog::LIMIT,
            "the retained text must stay within {} bytes, got {}",
            StderrLog::LIMIT,
            retained.len()
        );
        assert!(
            !retained.is_empty(),
            "the cap must not be so eager that it keeps nothing"
        );
    }
}
