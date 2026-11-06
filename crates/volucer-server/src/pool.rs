//! A pool of FastCGI slots, each an address with a process tree behind it.
//!
//! This is the `spawn_cgi` backend, and it is the only way to get a per-request reset on
//! Windows, where `php-fpm` does not exist. The shape is forced by the platform:
//!
//! - **A slot is an address with a concurrency**, not a process. `children = 0` (or
//!   `concurrency = 0`) is one process per slot. `children = n` is a preforked master plus `n`
//!   children, all sharing the one listening socket and the one OPcache segment; each child
//!   accepts its own connections. `concurrency = n` is the same number for a `command` slot,
//!   which does its own forking and reports its width instead of having it dictated.
//! - **`PHP_FCGI_CHILDREN` works on both platforms**, by different mechanisms: `fork()` on
//!   Unix (`cgi_main.c:1995`) and `CreateProcess` on Windows (`cgi_main.c:2083`). An earlier
//!   version of this comment said the Linux path ignored
//!   it, which was wrong; what is true is that a lone `php-cgi -b` with no children serves
//!   its connections **one at a time**.
//! - Several processes cannot share one port via `SO_REUSEADDR` on Windows, so each *slot*
//!   needs its own port. Within a slot, the children share the one bound socket because the
//!   master binds once and forks afterwards.
//! - FastCGI request ids would allow multiplexing, but neither driver does `FCGI_MPXS_CONNS` —
//!   `php-cgi` refuses it outright (`main/fastcgi.c:490`) and an application server behind
//!   `command` is an ordinary CGI framework, so one request per connection either way. More
//!   concurrency out of one slot means more processes, which is what `children` and
//!   `concurrency` are, and a demux table would buy nothing.
//!
//! **Two drivers, one pool.** Everything above the spawn is shared: the addresses, the
//! readiness probe, the restart on death, `max_requests`, and the per-slot admission. What
//! differs is the argv ([`SpawnDriver`]) and whether `PHP_FCGI_*` is exported — so the pool
//! holds a [`SlotLaunch`] and asks it to build a command, rather than branching at every site.
//!
//! The pool owns the processes, assigns the addresses, health-checks by connecting, restarts a
//! slot that died, and lends a connection per request. Concurrency within a slot is admitted by
//! [`SlotPermits`] rather than by the slot's lock, so a slot's lock is held only for
//! bookkeeping — which is what lets a slot's whole concurrency be in flight against one address at
//! once.

use std::collections::BTreeMap;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use volucer_core::config::{
    SpawnCgiConfig, SpawnDriver, FCGI_ADDRESS_ENV, FCGI_ADDRESS_PLACEHOLDER,
    FCGI_CONCURRENCY_PLACEHOLDER,
};
use volucer_core::program::{resolve_program, split_command};
#[cfg(unix)]
use volucer_fcgi::AsyncClient;
use volucer_fcgi::{Client, Endpoint, Request, Response};

use crate::error::{Hint, Result, ServerError};
use crate::lifecycle::{configure_command, spawn, LifecycleGuard};
use crate::relay::Relayed;
use crate::respawn::RespawnReport;

/// What a backend answered with.
///
/// The two shapes a request path has to distinguish, and the distinction is a *size* one rather than
/// a protocol one: a response that fits in the buffer is everything it ever was — compressible,
/// length-declaring, truncation-checked — and one that does not is relayed as it arrives.
#[derive(Debug)]
pub enum Delivery {
    /// A whole response, in memory.
    Buffered(Response),
    /// A response whose body is still being produced. The head is known; see [`Relayed`].
    Relayed(Relayed),
}

/// A streamed response that has been opened.
///
/// # Why this is a type at all
///
/// The FastCGI path hands back a [`Relayed`] and nothing else, because its reader thread has already
/// taken the slot's permit: it will be released when that thread finishes. A *worker* has one
/// exchange thread per worker for the worker's whole life, and the pool keeps the lease — so the
/// ending is a callback the pool must run, not a `Drop` on something the reader owns. This pairs the
/// two so they cannot be separated: a caller either holds both or neither.
pub(crate) struct RelayOpening {
    /// The head and the body still arriving.
    pub(crate) relayed: Relayed,
}

impl RelayOpening {
    pub(crate) fn new(relayed: Relayed) -> Self {
        RelayOpening { relayed }
    }
}

/// Render a worker response the way the synchronous streamed path does.
///
/// The relay policy can refuse to relay a response that arrived on the async path — an error page,
/// an `X-Sendfile` instruction, a body the worker measured — and when it does the caller must get
/// exactly the bytes it would have got from the ordinary path. One helper, so the two cannot drift.
pub(crate) fn rendered(response: volucer_worker::codec::WorkerResponse) -> Response {
    crate::worker_pool::render_worker(response, std::time::Duration::ZERO)
}

/// What an async streamed exchange opened: a body still arriving, or a complete response.
///
/// The distinction exists because `request_streamed_async` cannot know which it will get — the
/// relay policy is consulted *after* the head, by the worker's own thread — so the caller is told
/// which of the two meanings its answer has rather than having to ask again.
pub(crate) enum Opened {
    /// The relay policy refused to relay it: an error page, an `X-Sendfile` instruction, or a body
    /// the worker measured. Complete, and answered exactly as a buffered dispatch would be.
    Buffered(Response),
    /// The head is known and the body is arriving.
    Relayed(RelayOpening),
}

/// How long to wait for a slot to start accepting connections.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(10);

/// How many ports to try when a slot loses the bind race during startup.
///
/// Only reached when php-cgi **exits**, which means it could not bind — see
/// `CgiPool::spawn_port_slot`. Ten is enough to outlast a burst of probe-and-release
/// from concurrent test processes without turning a genuinely broken binary into a
/// slow failure: a real misconfiguration fails on the first attempt.
const PORT_ATTEMPTS: usize = 10;

/// Timeout for a single request to a slot.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// How long to wait after the startup probe connects before believing the address is ours.
///
/// **What this detects, and why it can be short.** The port a slot is about to use was freed by a
/// `port_is_free` probe and is bound by `php-cgi` a moment later, so between the two a sibling
/// process can take it. A connect then succeeds against *somebody else's* listener, the slot is
/// recorded as healthy, and the first real request comes back a 502 — which is exactly what
/// `Test (windows-latest)` reported: a green banner and a 502 on the first request.
///
/// What has to become visible here is one thing only: a `php-cgi` that **lost the bind exits**.
/// That is an `exit()` after a failed `bind()`, which is microseconds of work, not a PHP boot. So
/// the window only has to cover a process exit plus one trip through the scheduler.
///
/// It was briefly 50 ms, on the reasoning that CI is slow. That was a guess, and with
/// `workers = 32` it cost 1.6 s of start-up for a window nothing needed — see
/// [`CgiPool::start`], which now spawns slots concurrently so the per-slot cost is not multiplied.
///
/// **Why not a FastCGI handshake instead?** Tried, measured, rejected as the primary guard.
/// `php-cgi` binds its listening socket *before* its children are ready to accept, so a
/// `GET_VALUES` probe arriving in that window blocks until it times out and the settle runs
/// anyway: a 32-child start-up measured 372 ms against 91 ms for this. The handshake proves more,
/// but it costs a timeout on every slot in the ordinary case, which is the wrong trade for a
/// question the process's own exit status already answers.
const SETTLE_AFTER_CONNECT: Duration = Duration::from_millis(10);

/// One `php-cgi` listening socket, the process tree behind it, and its connections.
///
/// **A slot is an address with a concurrency, not a process.** With `children = 0` the two are
/// the same thing and this is what it always was — one process, one connection, the slot
/// mutex serialising requests. With `children = n` the master has forked `n` children that
/// share the listening socket, so it can answer `n` requests at once and needs `n`
/// connections to do it. Everything the pool decides — recycle, restart, kill, count — stays
/// per slot, which is what keeps `children` a number the pool declared and can therefore
/// enforce rather than a number hidden inside a process.
#[derive(Debug)]
struct Slot {
    /// The master process. With children it serves nothing itself: it forks, reaps and
    /// replaces, and the children accept.
    child: Child,
    /// Loopback TCP or a unix socket, depending on the configuration.
    endpoint: Endpoint,
    /// Requests this slot has served, for `max_requests` recycling.
    ///
    /// **An `AtomicU64` rather than a `u64`**, because a *relayed* response is still being served
    /// after it has been handed to the front-end: the reader thread counts it, and it can only do so
    /// through a shared handle. A plain counter would make `max_requests` a suggestion for a
    /// deployment that streams.
    ///
    /// **Slot-wide, not per child.** `php-cgi` gives no way to see a child's count from
    /// outside, and the replacement unit is the group — which is the only thing `killpg` can
    /// express. So this counter measures what the pool can act on.
    served: Arc<AtomicU64>,
    /// Live connections, up to the slot's concurrency.
    ///
    /// The slot's warm connections, in whichever mode the requests that left them put them.
    ///
    /// **One list, one connection per child, and a mode — not one list per mode.** `php-cgi` serves
    /// one *connection* at a time per child (`FCGI_MPXS_CONNS` is 0), so a slot that kept a warm
    /// connection in each mode would hold two sockets against one child and the second request would
    /// wait for the first child to finish — measured as an 8-second stall, with `connect` succeeding
    /// and the write never being read. A slot therefore owns at most `concurrency` connections, and
    /// each is tagged with the runtime mode it is currently in.
    clients: Vec<WarmClient>,
}

/// A connection a slot is holding warm, and how it is currently driven.
///
/// `O_NONBLOCK` and the cleared `SO_RCVTIMEO` are properties of the *file description*, not of the
/// handle, so a socket's mode is a property of the connection and cannot be inferred from anything
/// else. Tagging it is what lets a slot reuse one child's connection from either request path
/// without ever holding two, which is the difference between a pool that works on `php-cgi` and one
/// that stalls.
#[derive(Debug)]
enum WarmClient {
    Blocking(Client),
    #[cfg(unix)]
    Awaited(AsyncClient),
}

impl WarmClient {
    /// Whether the connection can serve another request, in whatever mode it is in.
    fn is_reusable(&self) -> bool {
        match self {
            WarmClient::Blocking(client) => client.is_reusable(),
            #[cfg(unix)]
            WarmClient::Awaited(client) => client.is_reusable(),
        }
    }

    /// Which client drives it. See [`Mode`].
    fn mode(&self) -> Mode {
        match self {
            WarmClient::Blocking(_) => Mode::Blocking,
            #[cfg(unix)]
            WarmClient::Awaited(_) => Mode::Awaited,
        }
    }
}

impl Slot {
    fn is_alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// The address this slot is bound to, reconstructed so a replacement can bind
    /// the same one.
    ///
    /// Derived from `endpoint` rather than stored separately, so a slot cannot
    /// hold two disagreeing notions of where it listens.
    fn address(&self) -> SlotAddress {
        slot_address_of(&self.endpoint)
    }

    /// Take this slot's warm connection, if it is one this mode can drive.
    ///
    /// **A connection in the other mode is closed, not converted**, and that is what keeps a slot to
    /// one connection at a time — which `php-cgi` requires. A single `php-cgi -b` serves one
    /// *connection* and loops on it: it never accepts a second until the first closes, so holding a
    /// warm blocking connection while the awaited path opens its own leaves the second request
    /// unserved until the first child is done with. Measured three ways: one slot with one child
    /// hangs; the same slot with `children = 4` passes; `strace` shows the child serving the first
    /// connection and never accepting the second.
    ///
    /// Closing costs a reconnect on the request that changes mode. That is the right trade — a
    /// deployment's traffic does not alternate paths per request (one front-end uses one), and a
    /// reconnect is a round trip where a second connection is a stall.
    fn take_warm(&mut self, mode: Mode) -> Option<WarmClient> {
        self.clients.retain(WarmClient::is_reusable);
        match self.clients.pop() {
            Some(warm) if warm.mode() == mode => Some(warm),
            // The other mode's connection, or one that is past its keep-alive. Dropping it closes the
            // socket, which is what hands the child back.
            _ => None,
        }
    }
}

/// Which client drives a connection, and therefore which of the two can use it.
///
/// A connection's mode is a property of its *file description* — `O_NONBLOCK` and the cleared
/// `SO_RCVTIMEO` — so it cannot be inferred from anything else and has to be recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Blocking,
    #[cfg(unix)]
    Awaited,
}

/// Send one request over a slot's address, returning the connection it used.
///
/// **A free function rather than a `Slot` method**, and that is what makes `children` work:
/// the slot's lock is not held here, so several requests can be in flight against one address
/// at once. Everything the request needs about the slot is its `endpoint`, which is copied
/// before this is called.
///
/// `take` is a connection this slot already owned, or `None` to open one. Returning the
/// connection rather than storing it keeps the slot free of state that would have to be
/// synchronised across a request.
fn exchange(
    endpoint: &Endpoint,
    request: &Request,
    take: Option<Client>,
    backend: &str,
    timeout: Duration,
) -> Result<(Response, Client)> {
    // Reuse the connection only when the previous request said it may be
    // reused; a CGI backend may close after every request, and assuming
    // otherwise is how a pool ends up writing into a dead socket.
    //
    // **Named**, not hardcoded: this used to report every failure as
    // `backend: "spawn_cgi"`, so a deployment with two FastCGI backends could not tell which
    // one refused the request from the error — and the log line named a configuration block
    // that may not even exist under that name.
    let mut client = match take {
        Some(client) if client.is_reusable() => client,
        _ => Client::connect(endpoint, Some(timeout)).map_err(|source| ServerError::Worker {
            backend: backend.to_string(),
            source,
        })?,
    };

    let mut attempt = 0;
    loop {
        match client.request(request) {
            Ok(response) => return Ok((response, client)),
            // One retry on a transport error: a slot may have recycled between requests
            // without the address changing, and `php-cgi` closes the connection even when
            // keep-alive was requested.
            //
            // **But not when the deadline expired**, and that condition is not a detail: a timed-out
            // request has already been waited for once, so retrying it spends a second whole budget
            // on a worker that is plainly not answering. Measured: a three-second fixture with
            // `request_timeout_ms = 1000` answered after 2.1 s rather than one second.
            Err(err)
                if attempt == 0
                    && retryable(&err)
                    && !crate::backend::is_deadline_expired(&err) =>
            {
                attempt += 1;
                client = Client::connect(endpoint, Some(timeout)).map_err(|source| {
                    ServerError::Worker {
                        backend: backend.to_string(),
                        source,
                    }
                })?;
            }
            Err(source) => {
                // **Named as the deadline it is**, rather than passed through as a socket timeout.
                // `os error 10060` and "a connection attempt failed" are what the OS says when a
                // read times out; the operator configured `request_timeout_ms`, and the message has
                // to say so or the fix is a search for a network problem that does not exist.
                if crate::backend::is_deadline_expired(&source) {
                    return Err(ServerError::Worker {
                        backend: backend.to_string(),
                        source: volucer_fcgi::FcgiError::Io(std::io::Error::new(
                            std::io::ErrorKind::TimedOut,
                            format!(
                                "the backend did not answer within {} ms — that is the deadline \
                                 `request_timeout_ms` sets for this backend, not a network timeout",
                                timeout.as_millis()
                            ),
                        )),
                    });
                }
                return Err(ServerError::Worker {
                    backend: backend.to_string(),
                    source,
                });
            }
        }
    }
}

fn is_connection_error(err: &volucer_fcgi::FcgiError) -> bool {
    crate::backend::is_transport_error(err)
}

/// Open a connection that a runtime — `runtime`, specifically — owns.
///
/// **Synchronous on purpose, and the two calls it makes are the reason.** A `tokio` connection
/// resolves its driver from the *current* runtime context at the moment it is constructed
/// (`from_std`), and that runtime must outlive the connection. `AsyncClient::connect` is a future,
/// so which runtime it registers under depends on who polls it — the *caller's* — and a slot's warm
/// connection is later polled by whoever asks next. Registering it on the caller's runtime and
/// storing it in the slot is therefore the same bug `AsyncRuntime` documents: a socket whose driver
/// is gone fails its next poll with *"a Tokio 1.x context was found, but it is being shutdown"*.
///
/// So the transport is opened with the blocking `Endpoint::connect` (a loopback connect or a unix
/// socket: no name server, no park worth avoiding) and handed to `adopt`, which runs
/// `prepare_for_async` and `from_std`. Both happen inside `enter`, and **neither is across an
/// `await`** — an `EnterGuard` is thread-local state, so holding one across a yield point would
/// leave the pool's runtime "entered" for whatever unrelated task the thread ran next.
///
/// A `?` here cannot leak the guard: it is a plain binding dropped at the end of the function.
#[cfg(unix)]
fn connect_awaited(
    runtime: &crate::worker_pool::AsyncRuntime,
    endpoint: &Endpoint,
    backend: &str,
    timeout: Duration,
) -> Result<AsyncClient> {
    let _entered = runtime.enter();
    let stream = endpoint
        .connect(Some(timeout))
        .map_err(|source| deadline_or_worker(backend, timeout, source))?;
    let adopted = volucer_fcgi::adopt(&stream).map_err(|source| ServerError::Worker {
        backend: backend.to_string(),
        source,
    })?;
    Ok(AsyncClient::from_stream(adopted))
}

/// [`exchange`], across an `await`.
///
/// The same sequence and the same two decisions: reuse the connection only when the previous request
/// said it may be reused, and retry once on a transport error — but not when the deadline expired,
/// because a timed-out request has already been waited for once.
///
/// Returns the connection so the caller can put it back, exactly as [`exchange`] does: a slot's warm
/// connection is what makes the next request to it cheap, and on this path it is also what keeps the
/// connection on the runtime that owns its waker.
#[cfg(unix)]
async fn exchange_async(
    runtime: &crate::worker_pool::AsyncRuntime,
    endpoint: &Endpoint,
    request: &Request,
    take: Option<AsyncClient>,
    backend: &str,
    timeout: Duration,
) -> Result<(Response, AsyncClient)> {
    let mut client = match take {
        Some(client) if client.is_reusable() => client,
        _ => connect_awaited(runtime, endpoint, backend, timeout)?,
    };

    let mut attempt = 0;
    loop {
        match client.request(request).await {
            Ok(response) => return Ok((response, client)),
            Err(err)
                if attempt == 0
                    && retryable(&err)
                    && !crate::backend::is_deadline_expired(&err) =>
            {
                attempt += 1;
                client = connect_awaited(runtime, endpoint, backend, timeout)?;
            }
            Err(source) => return Err(deadline_or_worker(backend, timeout, source)),
        }
    }
}

/// Report a failed awaited exchange the way the blocking one reports it.
///
/// **A deadline is named as a deadline.** `os error 10060` and "a connection attempt failed" are what
/// the OS says when a read times out; the operator configured `request_timeout_ms`, and a message
/// that does not say so sends them looking for a network problem that does not exist. The blocking
/// path builds this same sentence inline; it is a function here because the awaited path reaches it
/// from two places (the buffered exchange and the relayed one).
#[cfg(unix)]
fn deadline_or_worker(
    backend: &str,
    timeout: Duration,
    source: volucer_fcgi::FcgiError,
) -> ServerError {
    if crate::backend::is_deadline_expired(&source) {
        return ServerError::Worker {
            backend: backend.to_string(),
            source: volucer_fcgi::FcgiError::Io(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!(
                    "the backend did not answer within {} ms — that is the deadline \
                     `request_timeout_ms` sets for this backend, not a network timeout",
                    timeout.as_millis()
                ),
            )),
        };
    }
    ServerError::Worker {
        backend: backend.to_string(),
        source,
    }
}

/// Gives a slot's place back on drop, on every path out of a request.
///
/// A guard rather than a call before each `return`: `?` accounts for three of the exits, and
/// a missed release shrinks the slot permanently — a failure that only shows up as a pool
/// that gets slower and never recovers.
#[derive(Debug)]
struct PermitGuard(Arc<SlotPermits>);

impl Drop for PermitGuard {
    fn drop(&mut self) {
        self.0.leave();
    }
}

/// The retry predicate, kept named so the intent is visible at the call site.
fn retryable(err: &volucer_fcgi::FcgiError) -> bool {
    is_connection_error(err)
}

/// How one slot's process is started, resolved once at construction.
///
/// A value rather than a branch at every spawn site, and *resolved* rather than read from the
/// configuration again: a replacement slot must be started on the terms the pool was configured
/// with, not on whatever the file says now — the same rule `children` and `env` follow.
#[derive(Debug, Clone)]
struct SlotLaunch {
    kind: SlotKind,
    /// The environment every slot is started with, already merged over `[server.env]`.
    env: BTreeMap<String, String>,
    working_dir: Option<PathBuf>,
}

#[derive(Debug, Clone)]
enum SlotKind {
    /// `php-cgi -b <address>` plus the prefork variables.
    PhpCgi {
        binary: PathBuf,
        /// `PHP_FCGI_CHILDREN`: `0` for one process, `n` for a preforked master.
        children: u32,
    },
    /// A configured command, with `{address}` substituted per slot.
    Command {
        /// The program, resolved through `PATH` once so a mistyped name is a startup error
        /// rather than a pool of 502s, and so `check` and `serve` agree about which file runs.
        binary: PathBuf,
        /// The remaining tokens, still carrying `{address}` (and `{concurrency}`) where the
        /// operator put them.
        args: Vec<String>,
        /// What `{concurrency}` becomes, resolved once here rather than read from the
        /// configuration at each spawn.
        ///
        /// **Carried for the same reason `children` is.** A replacement slot has to be started
        /// on the terms the pool was configured with, and `{concurrency}` is now one of those
        /// terms — so a recycled slot must see the same number as the one it replaces, not
        /// whatever the file says if it were re-read. Resolved through
        /// [`SpawnCgiConfig::slot_concurrency`] so `concurrency = 0` becomes the `1` the pool
        /// actually admits, never a `0` the program would read as "start nothing".
        concurrency: usize,
    },
}

impl SlotLaunch {
    /// Resolve a slot's launch from the configuration, before anything is spawned.
    ///
    /// Every check that can be made without a process happens here, so a mistyped `binary` or
    /// `command` is a startup failure naming the key rather than a slot that spawns and exits.
    fn from_config(config: &SpawnCgiConfig, backend: &str) -> Result<Self> {
        let kind = match config.driver() {
            SpawnDriver::PhpCgi => {
                let configured =
                    config
                        .binary
                        .clone()
                        .ok_or_else(|| ServerError::Misconfigured {
                            backend: backend.to_string(),
                            reason: "`binary` is required for the php-cgi driver".to_string(),
                        })?;
                // Resolved exactly as `check` resolves it, so a bare `php-cgi` means the same
                // file to both commands. A value with a separator is returned unchanged.
                let binary = volucer_core::php::resolve_cgi_binary(&configured);
                if !binary.is_file() {
                    return Err(ServerError::Misconfigured {
                        backend: backend.to_string(),
                        reason: format!(
                            "{} does not exist{}",
                            binary.display(),
                            if binary == configured {
                                String::new()
                            } else {
                                format!(" (from binary \"{}\")", configured.display())
                            }
                        ),
                    });
                }
                SlotKind::PhpCgi {
                    binary,
                    children: config.fcgi_children(),
                }
            }
            SpawnDriver::Command => {
                let command =
                    config
                        .command
                        .as_deref()
                        .ok_or_else(|| ServerError::Misconfigured {
                            backend: backend.to_string(),
                            reason: "`command` is required when no `binary` is set".to_string(),
                        })?;
                let (program, args) =
                    split_command(command).map_err(|reason| ServerError::Misconfigured {
                        backend: backend.to_string(),
                        reason,
                    })?;
                let binary = resolve_program(&program);
                if !binary.is_file() {
                    return Err(ServerError::Misconfigured {
                        backend: backend.to_string(),
                        reason: format!(
                            "the slot program {} does not exist (from command {command:?})",
                            binary.display()
                        ),
                    });
                }
                // Checked here rather than at the spawn, because the failure it prevents is
                // worse than a wrong path: a command with no `{address}` and no reader of
                // `VOLUCER_FCGI_ADDRESS` ignores the address it was given and binds somewhere
                // of its own, so the pool connects to nothing and every request is a 502 after
                // the startup timeout. There is no way to tell the two apart from here, so the
                // placeholder requirement is stated rather than guessed: the variable is
                // documented as the alternative for a program that reads one, and a program
                // that reads *neither* cannot be started by a supervisor that assigns
                // addresses.
                if !args
                    .iter()
                    .any(|arg| arg.contains(FCGI_ADDRESS_PLACEHOLDER))
                {
                    // `{address}` may also be the program's own path, which is legal though
                    // unusual — `resolve_program` would have failed on it, so it cannot reach
                    // here; the check is on the args for that reason.
                    return Err(ServerError::Misconfigured {
                        backend: backend.to_string(),
                        reason: format!(
                            "the command {command:?} does not use {FCGI_ADDRESS_PLACEHOLDER}, so \
                             this slot would never be told the address to bind; put \
                             {FCGI_ADDRESS_PLACEHOLDER} where the program expects it (e.g. \
                             \"gunicorn app.wsgi --bind {FCGI_ADDRESS_PLACEHOLDER}\"), or, if it \
                             reads {FCGI_ADDRESS_ENV} from the environment, name it in the command \
                             so the placeholder is still present"
                        ),
                    });
                }
                SlotKind::Command {
                    binary,
                    args,
                    concurrency: config.slot_concurrency(),
                }
            }
        };

        if let Some(dir) = &config.working_dir {
            if !dir.is_dir() {
                return Err(ServerError::Misconfigured {
                    backend: backend.to_string(),
                    reason: format!("working_dir {} is not a directory", dir.display()),
                });
            }
        }

        Ok(SlotLaunch {
            kind,
            env: config.env.clone(),
            working_dir: config.working_dir.clone(),
        })
    }

    /// The command for one slot, with the address substituted.
    fn command(&self, address: &str) -> Command {
        match &self.kind {
            SlotKind::PhpCgi { binary, children } => {
                let mut command = Command::new(binary);
                command
                    .arg("-b")
                    .arg(address)
                    // php-cgi's own preforking, which the pool *declares* rather than
                    // suppresses.
                    //
                    // A `php-cgi` started with `-b` is not necessarily one process: it is a
                    // parent that forks (or, on Windows, `CreateProcess`es)
                    // `PHP_FCGI_CHILDREN` children, all sharing the one bound socket, and each
                    // child accepts its own connections. The variable is read from php-cgi's
                    // *own* environment (`cgi_main.c:1964`), so leaving it unset inherits
                    // whatever a deployment exported for php-fpm — measured at 20 on a 12-core
                    // machine, which made `workers` a lie.
                    //
                    // So the value is always written, and it is always the pool's own
                    // decision: `children`, or `0` to mean one process per slot.
                    .env("PHP_FCGI_CHILDREN", children.to_string())
                    .env("PHP_FCGI_MAX_REQUESTS", "0")
                    .envs(&self.env);
                command
            }
            SlotKind::Command {
                binary,
                args,
                concurrency,
            } => {
                let mut command = Command::new(binary);
                // `replace` rather than a shell: the tokenizer has already made each token one
                // argument, so a path with a space in it stays one argument, and an address can
                // never become shell syntax. A token may carry either placeholder more than once
                // and any number of tokens may — `--bind {address} --pidfile {address}.pid` is
                // an ordinary pairing.
                //
                // `{concurrency}` is replaced with the *resolved* number, so the value the
                // program is told to fork equals the number of permits this pool holds for the
                // same address. Writing the configured value instead would pass `0` for
                // `concurrency = 0`, which means one, and a program told `--workers 0` starts
                // nothing while the pool happily admits a request to it.
                let concurrency = concurrency.to_string();
                for arg in args {
                    let arg = arg
                        .replace(FCGI_ADDRESS_PLACEHOLDER, address)
                        .replace(FCGI_CONCURRENCY_PLACEHOLDER, &concurrency);
                    command.arg(arg);
                }
                command.envs(&self.env);
                command
            }
        }
    }

    /// The binary, for error messages.
    fn binary(&self) -> &Path {
        match &self.kind {
            SlotKind::PhpCgi { binary, .. } => binary,
            SlotKind::Command { binary, .. } => binary,
        }
    }

    /// Whether this driver's process is known to exit when it loses the bind race.
    ///
    /// `php-cgi` does — `exit()` after a failed `bind()`, which is the whole reason
    /// [`CgiPool::spawn_port_slot`] retries on the next port. A configured command's behaviour
    /// is not knowable from here, and retrying a program that died for the operator's own reason
    /// (a syntax error, a missing module) would turn one loud failure into ten slow ones. So the
    /// retry stays a php-cgi property, which is the one thing about the two drivers that differs
    /// in *policy* rather than in argv.
    fn retries_lost_bind(&self) -> bool {
        matches!(self.kind, SlotKind::PhpCgi { .. })
    }
}

/// Read a slot's output pipe into the log until it closes.
///
/// A thread per pipe rather than `Stdio::null()`, because the reason a non-PHP slot never started
/// listening is usually one line in one of these: a Python traceback, a Ruby `LoadError`, a
/// `php-cgi` warning about an extension. Nulling them turned every one of those into a bare
/// startup timeout.
///
/// `debug` rather than `info`, and that is the same level `upstream_pool` uses for an origin's
/// output: an application's access log belongs to the application, and a server that echoed every
/// request line at `info` would bury its own operational messages. A crash still surfaces —
/// through the slot's exit status in `StartupExited`, and through this line when the level is
/// raised.
///
/// The thread is detached and ends when the pipe does, which happens when the child exits or is
/// killed: nothing joins it, and nothing needs to, because it owns no state the pool reads.
fn drain<R>(backend: String, pid: u32, source: R, stream: &'static str)
where
    R: std::io::Read + Send + 'static,
{
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
                        tracing::debug!(backend = %backend, pid, stream, line = %text, "slot output");
                    }
                }
                Err(err) => {
                    tracing::debug!(backend = %backend, pid, stream, %err, "slot output ended");
                    break;
                }
            }
        }
    });
}

/// Where a slot binds: a loopback port, a unix socket path, or a Windows pipe.
#[derive(Debug, Clone)]
enum SlotAddress {
    Port(u16),
    Socket(PathBuf),
    #[cfg(windows)]
    Pipe(PathBuf),
}

impl SlotAddress {
    /// The address string this slot's program is given.
    ///
    /// **One string, both drivers.** `php-cgi -b` takes a bare path for a unix socket and
    /// `host:port` for TCP; a configured command is handed the `unix:` form, because that is
    /// what the rest of the world spells a socket and what the same value looks like in
    /// [`FCGI_ADDRESS_ENV`] — a program reading the variable and a program reading the argument
    /// must not see two spellings of one address.
    fn bind_argument(&self, bind_host: &str, driver: SpawnDriver) -> String {
        match self {
            SlotAddress::Port(port) => format!("{bind_host}:{port}"),
            SlotAddress::Socket(path) => match driver {
                // `php-cgi -b` parses its argument and only falls back to TCP when it contains
                // a `:` or is all digits, so the bare path is the form it understands. The
                // `unix:` prefix would be taken as a host name.
                SpawnDriver::PhpCgi => path.display().to_string(),
                SpawnDriver::Command => format!("unix:{}", path.display()),
            },
            #[cfg(windows)]
            SlotAddress::Pipe(name) => name.display().to_string(),
        }
    }

    /// The address the client connects to.
    ///
    /// Fallible rather than infallible because the `Socket` variant is
    /// unrepresentable as an [`Endpoint`] on Windows; config validation rejects
    /// such a configuration before a pool is built, so reaching the error here
    /// means the pool was constructed by hand.
    fn endpoint(&self, bind_host: &str, backend: &str) -> Result<Endpoint> {
        match self {
            SlotAddress::Port(port) => Ok(Endpoint::Tcp(format!("{bind_host}:{port}"))),
            SlotAddress::Socket(path) => Endpoint::parse(&format!("unix:{}", path.display()))
                .map_err(|source| ServerError::Misconfigured {
                    backend: backend.to_string(),
                    reason: format!("{} cannot be a unix socket here: {source}", path.display()),
                }),
            #[cfg(windows)]
            SlotAddress::Pipe(name) => Ok(Endpoint::Pipe(name.display().to_string())),
        }
    }
}

/// Recover the bind address from a live endpoint.
///
/// This is the inverse of [`SlotAddress::endpoint`], and it is what lets recycling
/// and restart bind the *same* place again. A slot stores only its endpoint, so
/// there is no second field that could drift out of agreement with it.
fn slot_address_of(endpoint: &Endpoint) -> SlotAddress {
    match endpoint {
        Endpoint::Tcp(spec) => SlotAddress::Port(
            spec.rsplit_once(':')
                .and_then(|(_, port)| port.parse().ok())
                .unwrap_or_else(|| panic!("a TCP endpoint always carries a port, got {spec:?}")),
        ),
        #[cfg(windows)]
        Endpoint::Pipe(name) => SlotAddress::Pipe(PathBuf::from(name)),
        #[cfg(unix)]
        Endpoint::Unix(path) => SlotAddress::Socket(PathBuf::from(path)),
    }
}

/// A configured `request_timeout_ms`, resolved or refused.
///
/// The same rule the worker pool's `Spec` applies, and stated separately because the two crates do
/// not share a module: `0` is refused rather than read as "no limit". A `php-cgi` with no deadline
/// holding its slot is indistinguishable from a slow request until the pool runs out of slots, and
/// by then the cause is gone.
fn request_timeout_of(configured: Option<u64>, backend: &str) -> Result<Duration> {
    match configured {
        None => Ok(REQUEST_TIMEOUT),
        Some(0) => Err(ServerError::Misconfigured {
            backend: backend.to_string(),
            reason: "request_timeout_ms = 0 would give a slot no deadline at all, so a hung \
                     request could hold it for ever; write the number of milliseconds you mean \
                     instead"
                .to_string(),
        }),
        Some(millis) => Ok(Duration::from_millis(millis)),
    }
}

/// A pool of `php-cgi` processes.
#[derive(Debug)]
pub struct CgiPool {
    name: String,
    /// Each slot behind its own lock, so the pool serves as many requests at once
    /// as it has slots and their concurrency allows.
    ///
    /// The lock is per *slot*, not per process: with `children > 0` one slot is several
    /// processes behind one address, and the slot's [`SlotPermits`] is what bounds concurrent
    /// requests to that address. The lock is only held while a connection is taken or
    /// returned, never across a request — which is the difference between "one request per
    /// slot" and "`children` or `concurrency` requests per slot".
    slots: Vec<Mutex<Slot>>,
    /// Admission per slot, so a slot's concurrency is enforced without holding its lock.
    ///
    /// Sized to the slot count and created from `config.slot_concurrency()`, so a slot's permit
    /// count and the number of processes the driver was told to start are the same number read
    /// from the same place.
    /// Behind an `Arc` because a relayed response releases its place from the reader thread, after
    /// the request that took it has already returned: `&self` cannot own that guard far enough.
    permits: Vec<Arc<SlotPermits>>,
    /// Round-robin position, so work and therefore memory are spread evenly.
    cursor: AtomicUsize,
    /// How one slot's process is started, resolved once at construction.
    ///
    /// Held as a value rather than re-derived from the configuration, because a replacement slot
    /// must be started on the terms the pool was configured with — the same rule `concurrency`
    /// and `env` follow.
    launch: SlotLaunch,
    /// Which driver built `launch`, for the three decisions that are not about argv: whether a
    /// Windows `socket` is a pipe, how a unix address is spelled, and whether a lost bind is
    /// retried.
    driver: SpawnDriver,
    bind_host: String,
    /// Socket paths this pool bound and therefore must unlink on the way out.
    socket_paths: Vec<PathBuf>,
    max_requests: Option<u64>,
    /// How long one request may take before the slot's process is treated as gone.
    ///
    /// Resolved once at construction from `[backends.<n>].request_timeout_ms`, for the same reason
    /// `max_requests` is: a deadline is a property of the pool this process built, so a reload does
    /// not change it — `restart_only` says so, and reading the configuration per request would let
    /// the two disagree.
    request_timeout: Duration,
    /// The runtime that drives a slot's connections, when this pool can use one at all.
    ///
    /// `None` on Windows (a slot's address may be a named pipe, which no runtime can drive) and
    /// `Some` everywhere else. Its presence is exactly what [`Self::drives_connections`] answers,
    /// which is the question the front-end's gate asks.
    ///
    /// **Owned by the pool rather than borrowed from the front-end**, and that is a rule rather than
    /// a preference: a connection's waker belongs to whichever runtime first polls it, and that
    /// runtime must outlive the connection. `CgiPool::start` runs long before any listener exists,
    /// so there is no front-end runtime to borrow. `worker_pool::AsyncRuntime` states the same rule
    /// from the other side, for the same measured reason — a socket whose runtime was shut down
    /// fails its next poll with *"a Tokio 1.x context was found, but it is being shutdown"*.
    #[cfg(unix)]
    runtime: Option<crate::worker_pool::AsyncRuntime>,
    /// Held for the life of the pool; dropping it reaps every slot.
    _guard: LifecycleGuard,
}

/// How many requests one slot may serve at once.
///
/// A counter with **two ways to wait on it**, because the same count has two kinds of caller.
/// A synchronous request (the FastCGI front-end, the control plane, a test) has to *block* —
/// blocking is the feature, since the alternative is opening a connection no child is there to
/// accept, which queues in the listen backlog where nothing counts it. An awaited request on the
/// HTTP front-end must **not** park its thread: at `c=100` with twelve slots, a hundred requests
/// park twelve runtime threads and nothing is left running to release one. So there is one number
/// and two wake-ups, and which one a request uses is a property of the path it took — never of the
/// limit, which would be a second limit.
///
/// **One counter rather than one per kind, and that is a correctness requirement.** With two, a slot
/// whose concurrency is 1 would admit a blocking request and an awaited one simultaneously — two
/// requests inside an address `php-cgi` serves strictly one at a time. Worse, [`CgiPool::respawn_idle`]
/// reads this count to decide whether a slot is serving, so a request counted in a list it did not
/// read would be replaced mid-response.
///
/// The concurrency is `children.max(1)` for `binary` and `concurrency.max(1)` for `command`.
/// With `children = 0` it is 1, so the observable behaviour of an existing configuration is
/// exactly what it was.
#[derive(Debug)]
struct SlotPermits {
    state: Mutex<usize>,
    /// Blocking waiters. Signalled on every release, whether or not one is waiting.
    released: Condvar,
    /// Awaited waiters. Signalled on every release for the same reason, and the cost of a signal
    /// nobody is waiting for is a lock-free no-op — the same trade
    /// [`WorkerPool::release`](crate::WorkerPool) makes for its two kinds.
    released_async: tokio::sync::Notify,
    concurrency: usize,
}

impl SlotPermits {
    fn new(concurrency: usize) -> Self {
        SlotPermits {
            state: Mutex::new(0),
            released: Condvar::new(),
            released_async: tokio::sync::Notify::new(),
            concurrency,
        }
    }

    /// Wait until the slot is below its concurrency, then count the request.
    ///
    /// A poisoned lock is recovered rather than propagated: a panic in one request must not
    /// wedge every later one behind a condvar nobody will signal again.
    fn enter(&self) {
        let mut in_flight = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        while *in_flight >= self.concurrency {
            in_flight = self
                .released
                .wait(in_flight)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
        *in_flight += 1;
    }

    /// [`Self::enter`], without parking the calling thread.
    ///
    /// The notification is registered **before** the check, so a `leave` that lands between the two
    /// cannot be lost: `enable()` arms it and the loop re-tests after every wake. That ordering is
    /// load-bearing rather than stylistic — a request that missed the wake would wait for a release
    /// that has already happened, for the rest of the slot's life.
    ///
    /// `#[cfg(unix)]` because its only callers are, and not as a convenience: a Windows slot address
    /// may be a named pipe, which no runtime can drive, so on that platform every request takes
    /// [`Self::enter`] and this would be dead code rather than a second path.
    #[cfg(unix)]
    async fn enter_async(&self) {
        loop {
            let released = self.released_async.notified();
            tokio::pin!(released);
            released.as_mut().enable();

            {
                let mut in_flight = self
                    .state
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if *in_flight < self.concurrency {
                    *in_flight += 1;
                    return;
                }
            }

            released.await;
        }
    }

    /// Give the place back, which is what wakes a waiting request — of either kind.
    fn leave(&self) {
        {
            let mut in_flight = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            *in_flight = in_flight.saturating_sub(1);
        }
        // Both signals for one release, and that is how a pool with two kinds of waiter works:
        // whichever kind is queued takes the place, and the other kind's signal finds nobody
        // waiting. Signalled outside the lock so the woken request does not immediately contend
        // with the releaser for the value it was just told had changed.
        self.released.notify_one();
        self.released_async.notify_one();
    }

    /// How many requests the slot is serving right now, or `None` if even reading that would
    /// block.
    ///
    /// `try_lock`, because the answer is only wanted by a rolling restart, and a sweep that
    /// waited here would be waiting for the requests it is supposed to leave alone. `None` means
    /// "not knowable from here", which the caller treats as busy — the conservative direction.
    ///
    /// Counts both kinds of request, because there is one count: see the type's own note on why a
    /// second counter would be a second limit.
    fn in_flight(&self) -> Option<usize> {
        self.state.try_lock().ok().map(|in_flight| *in_flight)
    }
}

impl CgiPool {
    /// Start a pool from configuration.
    pub fn start(name: &str, config: &SpawnCgiConfig) -> Result<Self> {
        // Resolved before anything is spawned: a mistyped `binary`, a `command` naming a program
        // that is not there, and a command with no `{address}` are all startup failures that name
        // the key rather than a pool of slots that spawn and exit.
        let launch = SlotLaunch::from_config(config, name)?;

        let guard = LifecycleGuard::new(&format!("volucer-{name}")).map_err(|source| {
            ServerError::Misconfigured {
                backend: name.to_string(),
                reason: format!("could not create the process lifecycle guard: {source}"),
            }
        })?;

        let count = worker_count(config.workers);
        let concurrency = config.slot_concurrency();
        let mut pool = CgiPool {
            name: name.to_string(),
            slots: Vec::with_capacity(count as usize),
            permits: (0..count)
                .map(|_| Arc::new(SlotPermits::new(concurrency)))
                .collect(),
            cursor: AtomicUsize::new(0),
            launch,
            driver: config.driver(),
            bind_host: config.bind_host.clone(),
            socket_paths: Vec::new(),
            max_requests: config.max_requests,
            request_timeout: request_timeout_of(config.request_timeout_ms, name)?,
            #[cfg(unix)]
            runtime: Some(
                crate::worker_pool::AsyncRuntime::start(name, count as usize).map_err(
                    |source| ServerError::Misconfigured {
                        backend: name.to_string(),
                        reason: format!(
                        "could not start the runtime that would drive this pool's connections: \
                         {source}"
                    ),
                    },
                )?,
            ),
            _guard: guard,
        };

        pool.spawn_every_slot(config)?;

        tracing::info!(
            backend = %pool.name,
            slots = pool.slots.len(),
            driver = pool.driver.as_str(),
            program = %pool.launch.binary().display(),
            concurrency = pool.permits.first().map_or(0, |permits| permits.concurrency),
            "FastCGI pool started"
        );
        Ok(pool)
    }

    /// Spawn every slot, **concurrently**.
    ///
    /// The slots are independent — each owns its own address and its own process tree, and nothing
    /// in this loop reads another's state — so starting them one at a time was a serialisation
    /// with no purpose. What it cost was every per-slot wait, multiplied: the settle after each
    /// connect (see [`SETTLE_AFTER_CONNECT`]) plus each child's own boot. At `workers = 32` that
    /// turned a fixed cost into a linear one, and it is start-up latency on a deployment that is
    /// not yet serving.
    ///
    /// **Ports are still allocated serially, and that has to be the case.** `port_is_free` is a
    /// probe-then-bind: two threads probing at once would both be told the same port is free and
    /// hand it to two children. So the *addresses* are chosen here, in order, and only the spawn
    /// and the wait happen in parallel. That keeps the allocation's single-threaded semantics
    /// exactly as they were.
    ///
    /// Results are collected **in index order**, so a failure reports the slot it belongs to and
    /// the pool is all-or-nothing: a partial pool would be a server that silently serves with
    /// fewer slots than its configuration asked for.
    fn spawn_every_slot(&mut self, config: &SpawnCgiConfig) -> Result<()> {
        let count = self.permits.len() as u32;

        // Addresses, decided up front so the port arithmetic stays single-threaded.
        let mut addresses = Vec::with_capacity(count as usize);
        let mut port = config.base_port;
        for index in 0..count {
            let address = match &config.socket {
                Some(prefix) => {
                    let path = volucer_core::config::cgi_socket_path(prefix, index);
                    #[cfg(windows)]
                    if path.to_str().is_some_and(|p| p.starts_with(r"\\.\pipe\")) {
                        SlotAddress::Pipe(path)
                    } else {
                        SlotAddress::Socket(path)
                    }
                    #[cfg(not(windows))]
                    SlotAddress::Socket(path)
                }
                None => {
                    // Skip ports that are already taken rather than failing the whole
                    // pool: another service on 19001 should not stop startup. The
                    // *probe* is only a hint — php-cgi binds the port itself a
                    // moment later, so it can still lose it — which is what
                    // `spawn_port_slot` handles.
                    while !port_is_free(&self.bind_host, port) {
                        port = port
                            .checked_add(1)
                            .ok_or_else(|| ServerError::Misconfigured {
                                backend: self.name.clone(),
                                reason: "ran out of ports while allocating slots".to_string(),
                            })?;
                    }
                    let chosen = port;
                    port = port
                        .checked_add(1)
                        .ok_or_else(|| ServerError::Misconfigured {
                            backend: self.name.clone(),
                            reason: "ran out of ports while allocating slots".to_string(),
                        })?;
                    SlotAddress::Port(chosen)
                }
            };
            addresses.push(address);
        }

        let mut spawned: Vec<Result<Slot>> = std::thread::scope(|scope| {
            let pool: &CgiPool = self;
            let handles: Vec<_> = addresses
                .iter()
                .map(|address| {
                    // `&self` is borrowed immutably and shared by every thread: spawning reads
                    // the binary, the bind host and the lifecycle guard, and mutates none of
                    // them. `CgiPool::start` has not published this pool to anyone yet.
                    scope.spawn(move || match address {
                        // Only a driver whose process exits on a lost bind gets a retry — see
                        // `SlotLaunch::retries_lost_bind`. A configured command that dies is
                        // reported as itself, because the next port would not be the fix.
                        SlotAddress::Port(first) if pool.launch.retries_lost_bind() => {
                            pool.spawn_port_slot(*first)
                        }
                        other => pool.spawn_slot(other),
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| {
                    handle.join().unwrap_or_else(|_| {
                        Err(ServerError::Misconfigured {
                            backend: "spawn".to_string(),
                            reason: "a slot-spawning thread panicked".to_string(),
                        })
                    })
                })
                .collect()
        });

        // Index order, so the error names the slot it came from rather than whichever thread
        // happened to finish first.
        for (index, result) in spawned.drain(..).enumerate() {
            let slot = result?;
            if let SlotAddress::Socket(path) = &addresses[index] {
                self.socket_paths.push(path.clone());
            }
            self.slots.push(Mutex::new(slot));
        }
        Ok(())
    }

    /// Recycle a slot after this many requests, the equivalent of php-fpm's
    /// `pm.max_requests`. **`0` means never recycle**, as it does there.
    pub fn with_max_requests(mut self, max_requests: Option<u64>) -> Self {
        self.max_requests = max_requests.filter(|max| *max > 0);
        self
    }

    /// Spawn a slot on a port, retrying only when the port was lost to a race.
    ///
    /// `CgiPool::start` probes a port with `port_is_free` and then hands it to php-cgi,
    /// which binds it itself a moment later. **Nothing can hold the port across that
    /// gap**: the probe binds and immediately drops, so between the two calls the port
    /// is free for anything else on the machine to take — including another test
    /// process doing exactly this. On macOS the kernel hands the most recently released
    /// port out first, which turns the gap into a repeatable collision rather than a
    /// rare one, and it showed up as an intermittent `StartupExited` from php-cgi
    /// (which exits non-zero when it cannot bind) and, from the outside, as a server
    /// that printed no banner.
    ///
    /// The retry is narrow on purpose. A child that **exited** during startup failed to
    /// bind, so a different port is the fix and the next one is already known. A child
    /// that is still running but never accepted a connection is a real misconfiguration
    /// — a wrong `-b`, a stuck interpreter — and is returned unchanged rather than
    /// retried, so this cannot turn a broken deployment into a slow one.
    ///
    /// When every attempt has been used, the **last child's own error is returned**, not
    /// a message about ports. A binary that is genuinely broken also exits, so it is
    /// retried too — and reporting "could not bind a port in 10 attempts" for a missing
    /// extension or a bad interpreter would replace the one diagnostic that names the
    /// real cause with one that sends the operator looking at the network. Each retry is
    /// a spawn that exits immediately, so the cost of being wrong here is small and the
    /// message stays true.
    fn spawn_port_slot(&self, first: u16) -> Result<Slot> {
        let mut port = first;
        let mut attempts = 0;
        loop {
            if !port_is_free(&self.bind_host, port) {
                port = port
                    .checked_add(1)
                    .ok_or_else(|| ServerError::Misconfigured {
                        backend: self.name.clone(),
                        reason: "ran out of ports while allocating slots".to_string(),
                    })?;
                continue;
            }
            match self.spawn_slot(&SlotAddress::Port(port)) {
                Ok(slot) => return Ok(slot),
                // The child died before accepting: it lost the bind race.
                Err(err @ ServerError::StartupExited { .. }) => {
                    attempts += 1;
                    if attempts >= PORT_ATTEMPTS {
                        return Err(err);
                    }
                    tracing::debug!(
                        backend = %self.name,
                        port,
                        attempt = attempts,
                        "the slot exited while binding; trying the next port"
                    );
                    port = port
                        .checked_add(1)
                        .ok_or_else(|| ServerError::Misconfigured {
                            backend: self.name.clone(),
                            reason: "ran out of ports while allocating slots".to_string(),
                        })?;
                }
                Err(other) => return Err(other),
            }
        }
    }

    fn spawn_slot(&self, address: &SlotAddress) -> Result<Slot> {
        let bind = address.bind_argument(&self.bind_host, self.driver);
        let mut command = self.launch.command(&bind);
        // Written after the configured `env`, so a table cannot point the program at an address
        // the pool is not connecting to — the same ordering `VOLUCER_WORKER_SOCKET` uses for a
        // worker's address. Exported for both drivers: `php-cgi` ignores it, and a command that
        // reads it instead of taking `{address}` is the case this exists for.
        command
            .env(FCGI_ADDRESS_ENV, &bind)
            // And the version last, so it cannot be shadowed.
            .env(volucer_core::config::VERSION_ENV, crate::build::VERSION);
        if let Some(dir) = &self.launch.working_dir {
            command.current_dir(dir);
        }
        // **Piped and drained, not nulled.** A `php-cgi` slot with a broken extension writes a
        // start-up warning here and then serves; a Python or Ruby application writes the
        // traceback that explains why it will not, and nulling it left an operator with a 502
        // and no reason. The same courtesy `upstream_pool` extends to a supervised origin, and
        // for the same reason: a dropped pipe is the half of the story that matters.
        //
        // stdin stays null: nothing volucer starts here reads it, and a program that blocks on
        // a terminal that never arrives would hang the slot rather than fail it.
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        configure_command(&mut command);

        // Through `lifecycle::spawn` rather than `command.spawn()`: this runs on whichever
        // thread asked for a slot — a `thread::scope` thread at start-up, a request thread on
        // a restart — and on Linux `PR_SET_PDEATHSIG` ties the child to *that* thread's
        // lifetime. See `lifecycle`'s module doc.
        let mut child = spawn(command).map_err(|source| ServerError::Spawn {
            binary: self.launch.binary().to_path_buf(),
            source,
        })?;

        // Drained before the readiness wait, because the reason a slot never listens is very
        // often in one of these two pipes — and a pipe nobody reads fills up and blocks the
        // child, which would turn "the application logged a lot while booting" into a startup
        // timeout with no output at all.
        if let Some(stdout) = child.stdout.take() {
            drain(self.name.clone(), child.id(), stdout, "slot stdout");
        }
        if let Some(stderr) = child.stderr.take() {
            drain(self.name.clone(), child.id(), stderr, "slot stderr");
        }

        // Adopt before waiting, so a failure here cannot leave an orphan.
        self._guard
            .adopt(&child)
            .map_err(|source| ServerError::Spawn {
                binary: self.launch.binary().to_path_buf(),
                source,
            })?;

        let mut slot = Slot {
            child,
            endpoint: address.endpoint(&self.bind_host, &self.name)?,
            served: Arc::new(AtomicU64::new(0)),
            clients: Vec::new(),
        };
        wait_until_listening(&mut slot, self.launch.binary(), STARTUP_TIMEOUT)?;
        Ok(slot)
    }

    /// Number of live slots.
    pub fn len(&self) -> usize {
        self.slots.len()
    }

    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// Whether this pool's connections are ones a runtime drives, rather than ones a thread holds.
    /// Asked by the front-end before it decides to `await` a request, and answered from the pool
    /// rather than from the configuration: on Unix every address a slot can have — a loopback port or
    /// a unix socket — is one [`volucer_fcgi::adopt`] can hand to a runtime, so the runtime's own
    /// presence is the whole answer. On Windows it is `false` whenever an address may be a named
    /// pipe, which no runtime can drive; see [`volucer_fcgi::AsyncClient::connect`], which refuses
    /// the same case from the other side.
    ///
    /// A property of the pool, so constant for its life, which is why it can be asked per request
    /// without touching a slot.
    pub fn drives_connections(&self) -> bool {
        #[cfg(unix)]
        {
            // A pool cannot outlive its runtime: the `Option` is emptied only by `Drop`, which runs
            // after every slot has been stopped.
            self.runtime.is_some()
        }
        #[cfg(not(unix))]
        {
            false
        }
    }

    /// Send a request through the pool.
    ///
    /// Takes `&self`. Admission is per slot and *outside* the slot's lock: a request first
    /// waits for a place in a slot (`children` places, or one when prefork is off), and only
    /// then takes the lock to borrow a connection. That is what lets `children = n` serve `n`
    /// requests at once from one address while still leaving the pool able to recycle, count
    /// and kill the slot as one unit.
    ///
    /// Replacing the child process is `restart`'s job.
    pub fn request(&self, request: &Request) -> Result<Response> {
        if self.slots.is_empty() {
            return Err(ServerError::NoWorker {
                backend: self.name.clone(),
            });
        }

        let index = self.next_slot_index();
        let permits = Arc::clone(&self.permits[index]);

        // Blocking rather than refusing: the front-end's own connection limit is where a
        // request is refused, and turning a full slot into an error here would make a pool
        // that is merely busy into a source of 502s. With `children = 0` the concurrency is one,
        // so this is a mutex by another name — exactly what it replaces.
        permits.enter();
        // Given back on every path out, including the `?` below. A `Drop` guard is the only
        // way to get that right without a `catch_unwind`, and a leaked permit would shrink
        // the slot for the life of the process.
        let _permit = PermitGuard(permits);

        // The lock covers *bookkeeping only* — take a warm connection, note the endpoint —
        // and is released before any I/O. Holding it across the request is what used to make
        // a slot strictly one request at a time; with `children > 0` that would throw away
        // the whole point, because the far side can answer several at once.
        let (endpoint, taken) = {
            let mut slot = self.slots[index]
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let taken = match slot.take_warm(Mode::Blocking) {
                Some(WarmClient::Blocking(client)) => Some(client),
                _ => None,
            };
            (slot.endpoint.clone(), taken)
        };

        let (response, client) =
            exchange(&endpoint, request, taken, &self.name, self.request_timeout)?;

        let served = {
            let mut slot = self.slots[index]
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            // The connection goes back so the next request to this slot is warm. Retention is
            // per slot and bounded by concurrency, which *is* the number of processes behind the
            // address — so a warm connection is one warm child and cannot starve a sibling.
            slot.clients.push(WarmClient::Blocking(client));
            slot.served.fetch_add(1, Ordering::Relaxed) + 1
        };
        self.recycle_if_needed(index, served)?;
        Ok(response)
    }

    /// [`Self::request`], without parking the calling thread while the slot's process thinks.
    ///
    /// The awaited twin, and it exists for the reason the front-end's gate exists: on a runtime
    /// thread, `permits.enter()` parks a thread the runtime needs to keep serving. Twelve parked
    /// threads out of twelve is the stall `WorkerPool::acquire_owned_async` documents — measured
    /// there as 35–50 rps with `too many connections`, which is what a self-inflicted deadlock
    /// looks like from outside.
    ///
    /// Every *rule* is the same as the blocking path's, deliberately, because a difference between
    /// them would be a correctness bug on one:
    ///
    /// - admission is per slot and outside the slot's lock, so `children = n` still serves `n` at
    ///   once from one address;
    /// - the lock covers bookkeeping only and is released before any I/O;
    /// - the connection goes back to the slot it came from, so the next request to that slot is warm;
    /// - the permit is given back on every path out, including the `?`s below.
    ///
    /// What differs is only *which* connection and *how the place is waited for*: both paths take from
    /// the same one list, and a connection is only reused by the mode that can drive it — a mode
    /// mismatch closes it rather than opening a second, because `php-cgi` serves one *connection* at a
    /// time per child and a second would wait for the first child to finish (see [`Slot::take_warm`]).
    /// The place is taken with [`SlotPermits::enter_async`] rather than on the `Condvar` the blocking
    /// path uses.
    ///
    /// Returns `WrongProtocol` rather than falling back when the address is one this path cannot
    /// drive, because a caller that asked for an awaited exchange has already told its front-end it
    /// may `await`; answering it synchronously anyway would be the silent stall
    /// `Backend::request_async` was written to remove.
    #[cfg(unix)]
    pub async fn request_async(&self, request: &Request) -> Result<Response> {
        if self.slots.is_empty() {
            return Err(ServerError::NoWorker {
                backend: self.name.clone(),
            });
        }
        let Some(runtime) = &self.runtime else {
            return Err(ServerError::WrongProtocol {
                backend: self.name.clone(),
            });
        };

        let index = self.next_slot_index();
        let permits = Arc::clone(&self.permits[index]);
        // Awaited rather than entered: the wait yields the runtime thread instead of consuming it,
        // on the *same* count the blocking path uses.
        permits.enter_async().await;
        let _permit = PermitGuard(permits);

        // Bookkeeping only, and released before any I/O — the same critical section the blocking
        // path takes.
        let (endpoint, taken) = {
            let mut slot = self.slots[index]
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let taken = match slot.take_warm(Mode::Awaited) {
                Some(WarmClient::Awaited(client)) => Some(client),
                _ => None,
            };
            (slot.endpoint.clone(), taken)
        };

        // **Every socket on this path is opened by `exchange_async`, inside the pool's runtime, and
        // never across an `await`.** See `connect_awaited`: a connection's driver is resolved when it
        // is constructed and must outlive it, so it cannot be registered on whichever runtime happens
        // to poll the future.
        let (response, client) = exchange_async(
            runtime,
            &endpoint,
            request,
            taken,
            &self.name,
            self.request_timeout,
        )
        .await?;

        let served = {
            let mut slot = self.slots[index]
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            slot.clients.push(WarmClient::Awaited(client));
            slot.served.fetch_add(1, Ordering::Relaxed) + 1
        };
        self.recycle_if_needed(index, served)?;
        Ok(response)
    }

    /// Replace a slot's process when it has reached `max_requests`.
    ///
    /// Recycle on the request count, mirroring `pm.max_requests`: this is what bounds the effect of
    /// any leak in the application. `0` means never, as it does for php-fpm — see
    /// `WorkerConfig::max_requests`.
    ///
    /// Slot-wide, and deliberately: with children each counts its own requests and php-cgi offers no
    /// way to read those from outside, so the only count that exists is the one this slot has
    /// served. Replacing the slot replaces the group, which is the unit `killpg` can express anyway.
    ///
    /// `served` is the value the caller observed *before* this check, so a concurrent request that
    /// also crossed the threshold does not replace a slot the first one has already replaced — that
    /// would kill a group that has served nothing.
    fn recycle_if_needed(&self, index: usize, served: u64) -> Result<()> {
        let Some(max) = self.max_requests.filter(|max| *max > 0) else {
            return Ok(());
        };
        if served < max {
            return Ok(());
        }
        let mut slot = self.slots[index]
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if slot.served.load(Ordering::Relaxed) >= max {
            self.replace_locked(&mut slot)?;
        }
        Ok(())
    }

    /// Pick a slot, preferring a live one.
    ///
    /// A slot that died is restarted in place so the pool keeps its size; the
    /// alternative is a pool that silently shrinks under repeated crashes.
    fn next_slot_index(&self) -> usize {
        let count = self.slots.len();
        let base = self.cursor.fetch_add(1, Ordering::Relaxed);
        for offset in 0..count {
            let index = (base + offset) % count;
            let mut slot = self.slots[index]
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if slot.is_alive() {
                return index;
            }
            // Give up on restart here rather than in a dispatch path: if this
            // fails the slot is replaced lazily on the next tick.
            tracing::warn!(
                backend = %self.name,
                address = %slot.endpoint,
                "a FastCGI slot died; restarting"
            );
            if let Err(err) = self.replace_locked(&mut slot) {
                tracing::warn!(backend = %self.name, %err, "could not restart a slot");
                continue;
            }
            return index;
        }
        // Every slot is gone; the first one will be replaced on the next request.
        0
    }

    /// Send a request through the pool, relaying the body instead of buffering it.
    ///
    /// The same admission, the same slot choice and the same permit accounting as [`Self::request`],
    /// with one difference at the end: when the response does not fit in `buffer` bytes the body is
    /// pumped by a thread of its own into a bounded channel, and the caller gets the header block
    /// immediately.
    ///
    /// # Why the permit is released by the reader thread
    ///
    /// The slot is *still serving this request* while its body is being relayed — a streamed response
    /// has not finished when its head arrives. So the permit has to live until `END_REQUEST`, which
    /// happens on the reader thread. Releasing it at return time would let the pool admit `children`
    /// more requests than there are processes and queue them invisibly inside the backend.
    ///
    /// # The client is not returned to the slot here
    ///
    /// It cannot be: it is owned by the reader thread until the response ends. A slot therefore has
    /// one fewer warm connection for the duration of a streamed response, which is correct — the
    /// connection genuinely is busy — and it costs one reconnect afterwards, on a path that only a
    /// large response takes.
    pub fn request_relayed(
        &self,
        request: &Request,
        buffer: usize,
        relay: volucer_fcgi::Relay,
    ) -> Result<Delivery> {
        if self.slots.is_empty() {
            return Err(ServerError::NoWorker {
                backend: self.name.clone(),
            });
        }

        let index = self.next_slot_index();
        let permits = Arc::clone(&self.permits[index]);
        permits.enter();
        // Moved into the reader thread when the response is streamed, and dropped here otherwise.
        // Either way it is released exactly once, which is why it is a guard rather than a call.
        let permit = PermitGuard(permits);

        let (endpoint, taken) = {
            let mut slot = self.slots[index]
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let taken = match slot.take_warm(Mode::Blocking) {
                Some(WarmClient::Blocking(client)) => Some(client),
                _ => None,
            };
            (slot.endpoint.clone(), taken)
        };

        let mut client = match taken {
            Some(client) if client.is_reusable() => client,
            _ => Client::connect(&endpoint, Some(self.request_timeout)).map_err(|source| {
                ServerError::Worker {
                    backend: self.name.clone(),
                    source,
                }
            })?,
        };

        let delivery = client
            .request_streaming(request, buffer, relay)
            .map_err(|source| {
                // The slot's connection is mid-response or dead; nothing goes back.
                ServerError::Worker {
                    backend: self.name.clone(),
                    source,
                }
            })?;

        match delivery {
            volucer_fcgi::Delivery::Buffered(response) => {
                let served = {
                    let mut slot = self.slots[index]
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    slot.clients.push(WarmClient::Blocking(client));
                    slot.served.fetch_add(1, Ordering::Relaxed) + 1
                };
                self.recycle_if_needed(index, served)?;
                Ok(Delivery::Buffered(response))
            }
            volucer_fcgi::Delivery::Streamed(stream) => {
                let served = Arc::clone(
                    &self.slots[index]
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .served,
                );
                // **Both the permit and the count are released on the reader thread.** The slot
                // serves this request until its last byte, so releasing either at return time would
                // let the pool admit more requests than there are processes — and would stop
                // `max_requests` from ever recycling a slot that only serves large responses.
                let report = crate::relay::spawn_reader(
                    self.name.clone(),
                    client,
                    stream,
                    move |_client, _stream| {
                        drop(permit);
                        served.fetch_add(1, Ordering::Relaxed);
                    },
                )
                .map_err(|source| ServerError::Spawn {
                    binary: std::path::PathBuf::from("volucer-relay"),
                    source,
                })?;
                Ok(Delivery::Relayed(report))
            }
        }
    }

    /// [`Self::request_relayed`], without parking the calling thread while the slot's process thinks.
    ///
    /// The awaited twin, and the rules are the blocking one's unchanged: the permit is held until
    /// `END_REQUEST` because the slot is still serving the request while its body is being relayed,
    /// the permit and the served count are released by whatever finishes the body, and the
    /// connection is not returned to the slot mid-response.
    ///
    /// What differs is what pumps the body and where the endpoint comes from — see
    /// [`Self::request_async`] for both, and [`crate::backend`]'s `external_relayed_async` for the
    /// same shape on the `external` backend.
    #[cfg(unix)]
    pub async fn request_relayed_async(
        &self,
        request: &Request,
        buffer: usize,
        relay: volucer_fcgi::Relay,
    ) -> Result<Delivery> {
        if self.slots.is_empty() {
            return Err(ServerError::NoWorker {
                backend: self.name.clone(),
            });
        }
        let Some(runtime) = &self.runtime else {
            return Err(ServerError::WrongProtocol {
                backend: self.name.clone(),
            });
        };

        let index = self.next_slot_index();
        let permits = Arc::clone(&self.permits[index]);
        permits.enter_async().await;
        // Moved into the pump task when the response is streamed, and dropped here otherwise.
        // Either way it is released exactly once, which is why it is a guard rather than a call.
        let permit = PermitGuard(permits);

        let (endpoint, taken) = {
            let mut slot = self.slots[index]
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let taken = match slot.take_warm(Mode::Awaited) {
                Some(WarmClient::Awaited(client)) => Some(client),
                _ => None,
            };
            (slot.endpoint.clone(), taken)
        };

        // One retry on a transport error, on the same terms as the blocking path: a slot may have
        // recycled between requests without the address changing, and `php-cgi` closes the
        // connection even when keep-alive was requested. Never on a deadline — see
        // `backend::is_deadline_expired`, which is the same predicate the blocking path consults.
        //
        // The connection is opened by `connect_awaited`, on the pool's runtime — see there for why it
        // cannot be an `await` on this path.
        let mut client = match taken {
            Some(client) if client.is_reusable() => client,
            _ => connect_awaited(runtime, &endpoint, &self.name, self.request_timeout)?,
        };
        let mut attempt = 0;
        let delivery = loop {
            match client.request_streaming(request, buffer, relay).await {
                Ok(delivery) => break delivery,
                Err(err)
                    if attempt == 0
                        && retryable(&err)
                        && !crate::backend::is_deadline_expired(&err) =>
                {
                    attempt += 1;
                    client = connect_awaited(runtime, &endpoint, &self.name, self.request_timeout)?;
                }
                Err(source) => {
                    // **Named as the deadline it is**, exactly as the blocking path does: the
                    // operator configured `request_timeout_ms`, and a message that says "socket
                    // timeout" sends them looking for a network problem that does not exist.
                    return Err(deadline_or_worker(&self.name, self.request_timeout, source));
                }
            }
        };

        match delivery {
            volucer_fcgi::Delivery::Buffered(response) => {
                let served = {
                    let mut slot = self.slots[index]
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    slot.clients.push(WarmClient::Awaited(client));
                    slot.served.fetch_add(1, Ordering::Relaxed) + 1
                };
                self.recycle_if_needed(index, served)?;
                Ok(Delivery::Buffered(response))
            }
            volucer_fcgi::Delivery::Streamed(stream) => {
                let served = Arc::clone(
                    &self.slots[index]
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .served,
                );
                let head = stream.head().to_vec();
                let head_duration = stream.head_duration();
                let verdict = crate::relay::Verdict::new();
                // The pump publishes through its own clone of the handles. `Verdict` clones the two
                // `Arc`s and never the answer, so there is one verdict either way.
                let published = verdict.clone();
                let (sender, receiver) = crate::relay::channel();
                let backend = self.name.clone();

                // **A task, not a thread** — the difference between this and
                // `crate::relay::spawn_reader`. The blocking client must be pumped by a thread
                // because `blocking_send` on a runtime worker would consume the thread the receiver
                // needs to drain the channel; an awaited client has no such problem, because
                // `send` is a future. That is the whole of what this path buys on a large response.
                tokio::spawn(async move {
                    let mut stream = stream;
                    let mut failed = false;
                    loop {
                        match client.next_chunk(&mut stream).await {
                            Ok(Some(chunk)) => {
                                if sender.send(bytes::Bytes::from(chunk)).await.is_err() {
                                    // The client went away: drain rather than abandon, so the
                                    // slot's connection is not left half-read.
                                    while let Ok(Some(_)) = client.next_chunk(&mut stream).await {}
                                    break;
                                }
                            }
                            Ok(None) => break,
                            Err(err) => {
                                tracing::warn!(
                                    backend = %backend,
                                    %err,
                                    "a streamed response failed part way through"
                                );
                                failed = true;
                                break;
                            }
                        }
                    }
                    let complete = stream.is_finished() && !failed;
                    published.publish(stream.stderr(), complete);
                    // The slot serves this request until its last byte, so both the permit and the
                    // count are released here and nowhere earlier.
                    drop(permit);
                    served.fetch_add(1, Ordering::Relaxed);
                });

                Ok(Delivery::Relayed(crate::relay::Relayed::new(
                    head,
                    receiver,
                    head_duration,
                    verdict,
                )))
            }
        }
    }

    /// Replace the process in a slot, keeping its address.
    pub fn restart(&self, index: usize) -> Result<()> {
        let mut slot = self.slots[index]
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.replace_locked(&mut slot)
    }

    /// Replace every slot's process that is not serving a request, and report what was left.
    ///
    /// The FastCGI half of a rolling restart of application code. It is the same shape as
    /// [`WorkerPool::respawn_idle`](crate::WorkerPool::respawn_idle) and deliberately the same
    /// conservative test: a slot is skipped unless it is *known* idle, because replacing one
    /// mid-request truncates the response and looks, from the client, like an application bug.
    ///
    /// # What "idle" means here, and why it is two conditions
    ///
    /// A slot's concurrency is `children` (or `concurrency`), so "the slot lock is free" is not
    /// enough: with a preforked master the lock is only held for bookkeeping and *several*
    /// requests may be in flight against one address. [`SlotPermits`] is where that is counted, so
    /// a slot is idle when its permit count is zero **and** its lock is uncontended. Both are read
    /// with `try_lock`, so a sweep never waits for a request to finish; either one failing is a
    /// skip.
    ///
    /// # The address is the reason this stops the old process first
    ///
    /// [`CgiPool::replace_locked`] already explains it: a slot keeps its address, and a port can
    /// only be bound once. Nothing about a sweep changes that, so a sweep is not "boot the new one
    /// beside the old one" — it is "stop, then boot", on the same address, one slot at a time.
    pub fn respawn_idle(&self) -> RespawnReport {
        let mut report = RespawnReport::new(self.name.clone());
        for index in 0..self.slots.len() {
            // Both probes first, and neither waits: a request that is starting right now has
            // already taken a permit, so the window between this check and the replacement below
            // is a window in which the slot has been counted as idle and nothing else can have
            // claimed it — `replace_locked` holds the slot lock across the whole stop-and-spawn,
            // and a request needs that lock to take a connection.
            if self.permits[index].in_flight().unwrap_or(1) != 0 {
                report.skipped_busy.push(index);
                continue;
            }
            let mut slot = match self.slots[index].try_lock() {
                Ok(slot) => slot,
                Err(_) => {
                    report.skipped_busy.push(index);
                    continue;
                }
            };
            match self.replace_locked(&mut slot) {
                Ok(()) => report.replaced.push(index),
                Err(err) => {
                    tracing::error!(
                        backend = %self.name,
                        slot = index,
                        %err,
                        "could not replace a FastCGI slot during a rolling restart; the next \
                         request through this slot will retry"
                    );
                    report.failed.push(index);
                }
            }
        }
        report
    }

    /// Replace a locked slot's process with a fresh one on the same address.
    ///
    /// **The old process is stopped before the replacement is spawned, and that order is not
    /// negotiable.** A slot keeps its address, and one port can only be bound once: `php-cgi`
    /// that loses the bind exits with status 255, which surfaces here as `StartupExited` and,
    /// on the request path, as a failed recycle of a slot that had just served successfully.
    ///
    /// This used to boot the replacement first, on the reasoning that a slot that cannot be
    /// restarted then keeps working rather than being left dead. That trade is not available:
    /// macOS refuses the second bind outright, and `Test (macos-latest)` failed on the second
    /// request of `recycling_replaces_the_process_after_max_requests` for exactly that reason.
    /// The platforms that *appeared* to allow it allow it by sharing the port — Windows
    /// `SO_REUSEADDR` lets a second process take a port that is already listening, measured
    /// with two `php-cgi -b 127.0.0.1:<port>` alive at once — so the replacement was silently
    /// hijacking the address rather than taking it over, and the old worker was still serving
    /// the connections it had already accepted.
    ///
    /// So the old process is stopped and reaped first, and a spawn that fails after that is
    /// reported: the slot is left without a process, which the next request repairs through
    /// this same function, and the failure is named rather than hidden behind a worker that
    /// only looked healthy because it was the previous one.
    fn replace_locked(&self, slot: &mut Slot) -> Result<()> {
        let address = slot.address();
        // Stopped and reaped before the address is offered again: `kill` is a signal, and the
        // listening socket is still open until the process has actually gone. `shutdown` does
        // the same two steps for the same reason.
        let _ = crate::lifecycle::kill(&mut slot.child);
        let _ = slot.child.wait();

        let mut replacement = self.spawn_slot(&address)?;
        replacement.served = Arc::new(AtomicU64::new(0));
        *slot = replacement;
        Ok(())
    }

    /// Stop every slot, waiting for it to exit.
    ///
    /// `&mut self` because stopping means emptying the pool, and only the owner
    /// may do that. Nothing on the request path needs it: the server drops the
    /// pool, and `Drop` calls this.
    pub fn shutdown(&mut self) {
        for slot in &mut self.slots {
            // `get_mut`: the pool is exclusively borrowed, so there is nothing to
            // synchronise against.
            let slot = slot
                .get_mut()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let _ = crate::lifecycle::kill(&mut slot.child);
            let _ = slot.child.wait();
        }
        self.slots.clear();
        // `php-cgi` unlinks a stale path before binding, so a leftover socket is
        // not fatal — but a stopped pool should not leave litter behind, and on
        // Linux an unlinked file is what releases the name for the next run.
        for path in self.socket_paths.drain(..) {
            if let Err(err) = std::fs::remove_file(&path) {
                if err.kind() != std::io::ErrorKind::NotFound {
                    tracing::warn!(
                        backend = %self.name,
                        path = %path.display(),
                        %err,
                        "could not remove a unix socket"
                    );
                }
            }
        }
    }

    /// The ports in use, for diagnostics. Unix-socket and pipe slots are not
    /// represented: they hold no port.
    pub fn ports(&self) -> Vec<u16> {
        self.locked_slots()
            .iter()
            .filter_map(|slot| match &slot.endpoint {
                Endpoint::Tcp(spec) => spec
                    .rsplit_once(':')
                    .and_then(|(_, port)| port.parse::<u16>().ok()),
                #[cfg(windows)]
                Endpoint::Pipe(_) => None,
                #[cfg(unix)]
                Endpoint::Unix(_) => None,
            })
            .collect()
    }

    /// Every slot's address, in slot order, for diagnostics and tests.
    pub fn endpoints(&self) -> Vec<String> {
        self.locked_slots()
            .iter()
            .map(|slot| slot.endpoint.to_string())
            .collect()
    }

    /// The unix socket paths this pool bound.
    pub fn socket_paths(&self) -> &[PathBuf] {
        &self.socket_paths
    }

    /// The process ids of the slots, for diagnostics and tests.
    pub fn pids(&self) -> Vec<u32> {
        self.locked_slots()
            .iter()
            .map(|slot| slot.child.id())
            .collect()
    }

    /// Every slot, locked in turn, for a read that does not span a request.
    fn locked_slots(&self) -> Vec<std::sync::MutexGuard<'_, Slot>> {
        self.slots
            .iter()
            .map(|slot| slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner()))
            .collect()
    }
}

impl Drop for CgiPool {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Turn a configured worker count into an actual count.
///
/// `0` is documented as "one per logical CPU", matching the example
/// configuration and php-fpm's own `pm.start_servers` convention.
///
/// Shared with the worker pool, so both pool kinds interpret `workers = 0` the
/// same way.
pub(crate) fn worker_count(configured: u32) -> u32 {
    if configured > 0 {
        return configured;
    }
    std::thread::available_parallelism()
        .map(|count| count.get() as u32)
        .unwrap_or(1)
}

/// Whether a port can be bound right now.
fn port_is_free(host: &str, port: u16) -> bool {
    TcpListener::bind((host, port)).is_ok()
}

/// Wait for a slot to accept connections.
///
/// Connects through the slot's own endpoint, so this works the same whether the
/// slot bound a port or a unix socket — and it is the only startup probe that
/// proves the *client* can reach the address, rather than that something is
/// listening somewhere.
///
/// **A connect that succeeds is not yet proof that *our* child is the one listening.** See
/// [`SETTLE_AFTER_CONNECT`] for the failure this guards: a foreign listener on the same address,
/// a healthy-looking slot, and a 502 on the first real request. A successful connect is therefore
/// followed by one settle, and then the child itself is asked whether it survived — which is the
/// only evidence that distinguishes "our backend is up" from "somebody else is on our port".
fn wait_until_listening(slot: &mut Slot, binary: &Path, timeout: Duration) -> Result<()> {
    wait_until_listening_with(slot, binary, timeout, &mut |_| {
        std::thread::sleep(SETTLE_AFTER_CONNECT)
    })
}

/// The same probe, with the settle injected.
///
/// Exists so the guard can be **falsified** rather than merely exercised. A test that raced a real
/// child against a real clock would pass whether or not the guard ran — the child would usually
/// still be alive 10 ms after a successful connect — which is the "a test that cannot fail on the
/// machine it runs on is not evidence" defect this file keeps recording. Taking the
/// settle as a closure lets a test make the child exit *inside* the window, deterministically.
fn wait_until_listening_with(
    slot: &mut Slot,
    binary: &Path,
    timeout: Duration,
    settle: &mut dyn FnMut(&mut Child),
) -> Result<()> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        match slot.child.try_wait() {
            Ok(Some(status)) => {
                return Err(ServerError::StartupExited {
                    binary: binary.to_path_buf(),
                    status: status.to_string(),
                    hint: Hint::default(),
                })
            }
            Ok(None) => {}
            Err(source) => return Err(ServerError::Io(source)),
        }
        if slot
            .endpoint
            .connect(Some(Duration::from_millis(250)))
            .is_ok()
        {
            // The settle: long enough for a child whose bind lost the race to exit, and no
            // longer, because that is the only thing it is waiting for.
            settle(&mut slot.child);
            match slot.child.try_wait() {
                // Still running, so the listener we just reached is ours.
                Ok(None) => return Ok(()),
                // It exited between the connect and now: it never held the port, so the
                // address belongs to somebody else. Reported as the bind failure it is, which
                // is the one error the caller retries on a fresh port.
                Ok(Some(status)) => {
                    tracing::debug!(
                        binary = %binary.display(),
                        address = %slot.endpoint,
                        %status,
                        "the slot exited just after its address answered; the listener was \
                         somebody else's"
                    );
                    return Err(ServerError::StartupExited {
                        binary: binary.to_path_buf(),
                        status: status.to_string(),
                        hint: Hint::default(),
                    });
                }
                Err(source) => return Err(ServerError::Io(source)),
            }
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    Err(ServerError::StartupTimeout {
        binary: binary.to_path_buf(),
        address: slot.endpoint.to_string(),
        timeout,
        hint: Hint::default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serialise the unit tests in this module that need a loopback port.
    ///
    /// The same reasoning as `tests/common::serial`, which the integration tests use, applied one
    /// layer down: `port_is_free_reports_the_truth` releases an ephemeral port and then asserts it
    /// can be bound again, and "can I bind this port" is a fact about the whole machine, not about
    /// this test. Fifteen other tests in this workspace bind ephemeral ports while it runs, so the
    /// assertion was racing them for a port number the OS had just handed back. It failed only on
    /// macos-latest, where the released port sits in the ephemeral range that the next `bind(0)`
    /// picks from first, which is why it looked platform-specific rather than like a race.
    fn port_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[test]
    fn zero_workers_means_the_available_parallelism() {
        assert!(worker_count(0) >= 1);
        assert_eq!(worker_count(4), 4);
        assert_eq!(worker_count(1), 1);
    }

    /// `{concurrency}` becomes the *resolved* number in the argv the pool is about to spawn.
    ///
    /// Asserted on the `Command` the pool builds rather than on the configuration string, because
    /// the substitution is the thing that can be wrong: a `replace` on the wrong token, or the
    /// configured `0` instead of the resolved `1`, both leave a file that looks correct and a
    /// program that starts no workers. `get_args` is the same argv `spawn_slot` hands the OS.
    #[test]
    fn a_command_substitutes_the_resolved_concurrency() {
        let program = if cfg!(windows) { "cmd" } else { "true" };
        let mut config = SpawnCgiConfig {
            binary: None,
            command: Some(format!(
                "{program} --workers {{concurrency}} --bind {{address}}"
            )),
            workers: 2,
            bind_host: "127.0.0.1".to_string(),
            base_port: 19_001,
            socket: None,
            max_requests: None,
            request_timeout_ms: None,
            children: 0,
            concurrency: 4,
            working_dir: None,
            env: Default::default(),
            legacy_capacity: None,
            services: Vec::new(),
        };

        let launch = SlotLaunch::from_config(&config, "test").expect("the slot resolves");
        let args: Vec<String> = launch
            .command("127.0.0.1:19001")
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            vec![
                "--workers".to_string(),
                "4".to_string(),
                "--bind".to_string(),
                "127.0.0.1:19001".to_string()
            ],
            "both placeholders are replaced, and neither survives into the argv"
        );

        // `concurrency = 0` means one. Passing the configured value through would hand the
        // program `--workers 0`, which starts nothing, while the pool holds one permit for it.
        config.concurrency = 0;
        let launch = SlotLaunch::from_config(&config, "test").expect("the slot resolves");
        let args: Vec<String> = launch
            .command("127.0.0.1:19001")
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args[1], "1",
            "zero means one, so that is what the program is told: {args:?}"
        );
    }

    /// A placeholder the pool does not substitute is left exactly as written.
    ///
    /// `-DKEY={value}` is a legal argument for a program that takes one, so the substitution must
    /// not be a general "replace every brace" pass. Only the named tokens move; anything else
    /// reaches the program intact, which is what makes adding a placeholder safe for the
    /// deployments that already work.
    #[test]
    fn an_unknown_placeholder_is_left_alone() {
        let program = if cfg!(windows) { "cmd" } else { "true" };
        let config = SpawnCgiConfig {
            binary: None,
            command: Some(format!("{program} --define={{a:1}} --bind {{address}}")),
            workers: 1,
            bind_host: "127.0.0.1".to_string(),
            base_port: 19_001,
            socket: None,
            max_requests: None,
            request_timeout_ms: None,
            children: 0,
            concurrency: 2,
            working_dir: None,
            env: Default::default(),
            legacy_capacity: None,
            services: Vec::new(),
        };

        let launch = SlotLaunch::from_config(&config, "test").expect("the slot resolves");
        let args: Vec<String> = launch
            .command("127.0.0.1:19001")
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args[0], "--define={a:1}",
            "an unrelated brace pair is untouched: {args:?}"
        );
        assert_eq!(
            args[2], "127.0.0.1:19001",
            "the address is still substituted: {args:?}"
        );
    }

    #[test]
    fn a_slot_address_knows_how_to_bind_and_how_to_connect() {
        let port = SlotAddress::Port(19_001);
        assert_eq!(
            port.bind_argument("127.0.0.1", SpawnDriver::PhpCgi),
            "127.0.0.1:19001"
        );
        assert_eq!(
            port.bind_argument("127.0.0.1", SpawnDriver::Command),
            "127.0.0.1:19001",
            "a TCP address is spelled the same way for both drivers"
        );
        assert_eq!(
            port.endpoint("127.0.0.1", "test")
                .expect("a port is always an endpoint"),
            Endpoint::Tcp("127.0.0.1:19001".to_string())
        );

        let socket = SlotAddress::Socket(PathBuf::from("/run/volucer/cgi.0.sock"));
        assert_eq!(
            socket.bind_argument("127.0.0.1", SpawnDriver::PhpCgi),
            "/run/volucer/cgi.0.sock",
            "php-cgi takes a bare path for a unix socket, not the `unix:` prefix"
        );
        assert_eq!(
            socket.bind_argument("127.0.0.1", SpawnDriver::Command),
            "unix:/run/volucer/cgi.0.sock",
            "a configured command is handed the `unix:` form, the same spelling \
             VOLUCER_FCGI_ADDRESS carries"
        );
    }

    /// Where unix sockets exist they resolve to a `Unix` endpoint; where they do
    /// not, the same input is an error rather than a silent fallback to TCP.
    #[cfg(unix)]
    #[test]
    fn a_unix_slot_address_resolves_to_a_unix_endpoint() {
        let socket = SlotAddress::Socket(PathBuf::from("/run/volucer/cgi.0.sock"));
        assert_eq!(
            socket
                .endpoint("127.0.0.1", "test")
                .expect("unix sockets exist here"),
            Endpoint::Unix("/run/volucer/cgi.0.sock".to_string())
        );
    }

    #[cfg(not(unix))]
    #[test]
    fn a_unix_slot_address_is_refused_where_the_platform_has_none() {
        let socket = SlotAddress::Socket(PathBuf::from("/run/volucer/cgi.0.sock"));
        let err = socket
            .endpoint("127.0.0.1", "test")
            .expect_err("a unix socket cannot be reached here");
        assert!(err.to_string().contains("cannot be a unix socket"), "{err}");
    }

    /// A Windows pipe slot hands `php-cgi` the bare pipe name and connects to it.
    #[cfg(windows)]
    #[test]
    fn a_pipe_slot_address_binds_and_connects_by_name() {
        let pipe = SlotAddress::Pipe(PathBuf::from(r"\\.\pipe\volucer-cgi.0"));
        assert_eq!(
            pipe.bind_argument("127.0.0.1", SpawnDriver::PhpCgi),
            r"\\.\pipe\volucer-cgi.0",
            "php-cgi takes the pipe name verbatim, not `host:port`"
        );
        assert_eq!(
            pipe.endpoint("127.0.0.1", "test")
                .expect("a pipe is always an endpoint"),
            Endpoint::Pipe(r"\\.\pipe\volucer-cgi.0".to_string())
        );
        // And the round trip has to come back to the same place, or a recycled slot
        // would bind a differently named pipe.
        assert!(matches!(
            slot_address_of(&pipe.endpoint("127.0.0.1", "test").unwrap()),
            SlotAddress::Pipe(_)
        ));
    }

    #[test]
    fn a_slot_address_round_trips_through_an_endpoint() {
        // `slot_address_of` recovers the bind address from a live endpoint, which
        // is how recycling and restart bind the same place again. If that
        // reconstruction drifts, a recycled slot silently moves.
        let port = SlotAddress::Port(19_444);
        let endpoint = port
            .endpoint("127.0.0.1", "test")
            .expect("a port is always reachable");
        match slot_address_of(&endpoint) {
            SlotAddress::Port(recovered) => assert_eq!(recovered, 19_444),
            SlotAddress::Socket(path) => panic!("expected a port, got {path:?}"),
            #[cfg(windows)]
            SlotAddress::Pipe(name) => panic!("expected a port, got pipe {name:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_unix_slot_address_round_trips_through_an_endpoint() {
        let socket = SlotAddress::Socket(PathBuf::from("/run/volucer/cgi.3.sock"));
        let endpoint = socket
            .endpoint("127.0.0.1", "test")
            .expect("unix sockets exist here");
        match slot_address_of(&endpoint) {
            SlotAddress::Socket(recovered) => {
                assert_eq!(recovered, PathBuf::from("/run/volucer/cgi.3.sock"));
            }
            SlotAddress::Port(port) => panic!("expected a socket, got port {port}"),
            #[cfg(windows)]
            SlotAddress::Pipe(name) => panic!("expected a socket, got pipe {name:?}"),
        }
    }

    #[test]
    fn port_is_free_reports_the_truth() {
        let _serial = port_lock();

        // The "held" half is asserted first and is the one that matters: it is what the startup
        // path uses to decide a port is taken. The "released" half proves the predicate is not
        // simply always false.
        //
        // **The released half has to retry, and the mutex cannot help it.** `port_lock` only
        // serialises this binary; the workspace's integration tests run as *separate processes* and
        // bind ephemeral ports concurrently, so any of them can take the port in the window
        // between `drop(listener)` and the check. On macos-latest that window is unusually wide
        // because the kernel hands the just-released port out first, which is why the failure
        // looked platform-specific. Retrying here is not hiding a product bug: the property under
        // test is "the predicate answers honestly about *some* free port", and a port stolen by
        // another process is the environment changing, not the predicate lying. The "held" half is
        // still asserted on every attempt, and exhausting the attempts is a real failure.
        for attempt in 1..=10 {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind an ephemeral port");
            let port = listener.local_addr().unwrap().port();
            assert!(!port_is_free("127.0.0.1", port), "the port is held");
            drop(listener);
            if port_is_free("127.0.0.1", port) {
                return;
            }
            assert!(
                attempt < 10,
                "port_is_free called {port} taken on all 10 attempts after releasing it"
            );
        }
    }

    /// The startup probe must not mistake *somebody else's* listener for its own child.
    ///
    /// **This is the falsification of the guard, not a test of the happy path.** The defect it
    /// pins: a slot's port is freed by a probe and bound by `php-cgi` a moment later, so another
    /// process can be on it. A connect then succeeds, the slot is recorded as healthy, and the
    /// first real request comes back a 502 — which is what `Test (windows-latest)` reported in
    /// run 39829499236.
    ///
    /// The settle is injected so the child can be made to exit *inside* the window,
    /// deterministically. With the guard removed this test fails, because the slot is reported
    /// healthy; with it, the exit is seen and the error is the one the caller retries on.
    #[test]
    fn a_listener_that_is_not_ours_is_not_mistaken_for_a_started_slot() {
        let _serial = port_lock();

        // A stand-in for the foreign process: something listening on the port that is not the
        // child, and that will never answer a FastCGI record.
        let squatter = TcpListener::bind("127.0.0.1:0").expect("bind the squatted port");
        let port = squatter.local_addr().expect("address").port();

        // A child that has already ended, which is exactly the state a `php-cgi` that lost the
        // bind is in.
        let child = if cfg!(windows) {
            Command::new("cmd")
                .args(["/C", "exit", "3"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
        } else {
            Command::new("true").spawn()
        }
        .expect("spawn a child that exits immediately");

        let mut slot = Slot {
            child,
            endpoint: Endpoint::Tcp(format!("127.0.0.1:{port}")),
            served: Arc::new(AtomicU64::new(0)),
            clients: Vec::new(),
        };

        // The settle waits for the child to be gone, so the test does not depend on how long
        // that takes on a loaded runner.
        let err = wait_until_listening_with(
            &mut slot,
            Path::new("php-cgi-that-lost-the-bind"),
            Duration::from_secs(5),
            &mut |child| {
                let _ = child.wait();
            },
        )
        .expect_err("a listener belonging to another process is not a started slot");

        assert!(
            matches!(err, ServerError::StartupExited { .. }),
            "a lost bind has to look like one, because that is the only error the caller retries \
             on a fresh port: {err:?}"
        );
        drop(squatter);
    }

    #[test]
    fn a_missing_binary_is_a_misconfiguration_not_a_panic() {
        let config = SpawnCgiConfig {
            binary: Some(PathBuf::from("definitely-not-a-php-cgi")),
            command: None,
            workers: 1,
            bind_host: "127.0.0.1".to_string(),
            base_port: 19_700,
            socket: None,
            children: 0,
            concurrency: 0,
            max_requests: None,
            request_timeout_ms: None,
            working_dir: None,
            env: Default::default(),
            legacy_capacity: None,
            services: Vec::new(),
        };
        let err = CgiPool::start("test", &config).unwrap_err();
        assert!(matches!(err, ServerError::Misconfigured { .. }), "{err}");
        assert!(err.to_string().contains("does not exist"), "{err}");
    }

    #[test]
    fn connection_errors_are_recognised_for_retry() {
        use volucer_fcgi::FcgiError;
        for kind in [
            std::io::ErrorKind::BrokenPipe,
            std::io::ErrorKind::ConnectionReset,
            std::io::ErrorKind::UnexpectedEof,
        ] {
            assert!(is_connection_error(&FcgiError::Io(std::io::Error::new(
                kind, "test"
            ))));
        }
        assert!(is_connection_error(&FcgiError::IncompleteResponse {
            request_id: 1,
            complete: true
        }));
        // A protocol-level refusal is not a connection problem.
        assert!(!is_connection_error(&FcgiError::BackendProtocolStatus {
            status: volucer_fcgi::ProtocolStatus::Overloaded
        }));
    }
}
