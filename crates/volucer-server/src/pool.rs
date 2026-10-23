//! A pool of `php-cgi` slots, each a listening socket with a process tree behind it.
//!
//! This is the `spawn_cgi` backend, and on Windows it is the only way to get
//! per-request SAPI reset at all. The shape is forced by the platform
//! (`internal/plan.md` §9):
//!
//! - **A slot is an address with a capacity**, not a process. `children = 0` — the
//!   default — is one process, which is what this pool was until hazard 86. `children = n`
//!   is a preforked master plus `n` children, all sharing the one listening socket and the
//!   one OPcache segment; each child accepts its own connections.
//! - **`PHP_FCGI_CHILDREN` works on both platforms**, by different mechanisms: `fork()` on
//!   Unix (`cgi_main.c:1995`) and `CreateProcess` on Windows (`cgi_main.c:2083`). An earlier
//!   version of this comment — and of `internal/plan.md` hazard 43 — said the Linux path ignored
//!   it, which was wrong; what is true is that a lone `php-cgi -b` with no children serves
//!   its connections **one at a time**.
//! - Several processes cannot share one port via `SO_REUSEADDR` on Windows, so each *slot*
//!   needs its own port. Within a slot, the children share the one bound socket because the
//!   master binds once and forks afterwards.
//! - FastCGI request ids would allow multiplexing, but `php-cgi` refuses it outright —
//!   `FCGI_MPXS_CONNS` is `0` (`main/fastcgi.c:490`) and the accept loop is one request at a
//!   time (`cgi_main.c:2276`). So more concurrency out of one slot means more processes,
//!   which is exactly what `children` is, and a demux table would buy nothing.
//!
//! The pool owns the processes, assigns the addresses, health-checks by connecting,
//! restarts a slot that died, and lends a connection per request. Concurrency within a slot
//! is admitted by [`SlotPermits`] rather than by the slot's lock, so a slot's lock is held
//! only for bookkeeping — which is what lets `children` requests be in flight against one
//! address at once.

use std::collections::BTreeMap;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

use volucer_core::config::SpawnCgiConfig;
use volucer_fcgi::{Client, Endpoint, Request, Response};

use crate::error::{Hint, Result, ServerError};
use crate::lifecycle::{configure_command, LifecycleGuard};

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
/// **A slot is an address with a capacity, not a process.** With `children = 0` the two are
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
    /// **Slot-wide, not per child.** `php-cgi` gives no way to see a child's count from
    /// outside, and the replacement unit is the group — which is the only thing `killpg` can
    /// express. So this counter measures what the pool can act on.
    served: u64,
    /// Live connections, up to the slot's capacity.
    ///
    /// A `Vec` rather than one `Option` because a preforked master can answer several
    /// requests at once, and one connection carries one request: `FCGI_MPXS_CONNS` is `0` and
    /// the accept loop is one request at a time. An idle connection here is one warm child.
    clients: Vec<Client>,
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

    /// Drop every connection that the backend refused to keep.
    ///
    /// Separate from [`exchange`] because it has to run before a connection is taken,
    /// and doing it in one pass is cheaper than testing each one twice.
    fn prune_dead_clients(&mut self) {
        self.clients.retain(|client| client.is_reusable());
    }
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
) -> Result<(Response, Client)> {
    // Reuse the connection only when the previous request said it may be
    // reused; `php-cgi` may close after every request, and assuming
    // otherwise is how a pool ends up writing into a dead socket.
    let mut client = match take {
        Some(client) if client.is_reusable() => client,
        _ => Client::connect(endpoint, Some(REQUEST_TIMEOUT)).map_err(|source| {
            ServerError::Worker {
                backend: "spawn_cgi".to_string(),
                source,
            }
        })?,
    };

    let mut attempt = 0;
    loop {
        match client.request(request) {
            Ok(response) => return Ok((response, client)),
            // One retry on a transport error: a slot may have recycled
            // between requests without the port changing, and php-cgi closes
            // the connection even when keep-alive was requested.
            Err(err) if attempt == 0 && retryable(&err) => {
                attempt += 1;
                client = Client::connect(endpoint, Some(REQUEST_TIMEOUT)).map_err(|source| {
                    ServerError::Worker {
                        backend: "spawn_cgi".to_string(),
                        source,
                    }
                })?;
            }
            Err(source) => {
                return Err(ServerError::Worker {
                    backend: "spawn_cgi".to_string(),
                    source,
                })
            }
        }
    }
}

fn is_connection_error(err: &volucer_fcgi::FcgiError) -> bool {
    crate::backend::is_transport_error(err)
}

/// Gives a slot's place back on drop, on every path out of a request.
///
/// A guard rather than a call before each `return`: `?` accounts for three of the exits, and
/// a missed release shrinks the slot permanently — a failure that only shows up as a pool
/// that gets slower and never recovers.
#[derive(Debug)]
struct PermitGuard<'a>(&'a SlotPermits);

impl Drop for PermitGuard<'_> {
    fn drop(&mut self) {
        self.0.leave();
    }
}

/// The retry predicate, kept named so the intent is visible at the call site.
fn retryable(err: &volucer_fcgi::FcgiError) -> bool {
    is_connection_error(err)
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
    /// The string `php-cgi -b` is given.
    fn bind_argument(&self, bind_host: &str) -> String {
        match self {
            SlotAddress::Port(port) => format!("{bind_host}:{port}"),
            SlotAddress::Socket(path) => path.display().to_string(),
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
    fn endpoint(&self, bind_host: &str) -> Result<Endpoint> {
        match self {
            SlotAddress::Port(port) => Ok(Endpoint::Tcp(format!("{bind_host}:{port}"))),
            SlotAddress::Socket(path) => Endpoint::parse(&format!("unix:{}", path.display()))
                .map_err(|source| ServerError::Misconfigured {
                    backend: "spawn_cgi".to_string(),
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

/// A pool of `php-cgi` processes.
#[derive(Debug)]
pub struct CgiPool {
    name: String,
    /// Each slot behind its own lock, so the pool serves as many requests at once
    /// as it has slots and their capacity allows.
    ///
    /// The lock is per *slot*, not per process: with `children > 0` one slot is several
    /// processes behind one address, and the slot's [`SlotPermits`] is what bounds concurrent
    /// requests to that address. The lock is only held while a connection is taken or
    /// returned, never across a request — which is the difference between "one request per
    /// slot" and "`children` requests per slot".
    slots: Vec<Mutex<Slot>>,
    /// Admission per slot, so a slot's capacity is enforced without holding its lock.
    ///
    /// Sized to the slot count and created from `config.children`, so a slot's permit count
    /// and the number of processes `php-cgi` was told to fork are the same number read from
    /// the same place ([`SpawnCgiConfig::slot_capacity`]).
    permits: Vec<SlotPermits>,
    /// Round-robin position, so work and therefore memory are spread evenly.
    cursor: AtomicUsize,
    binary: PathBuf,
    bind_host: String,
    /// Socket paths this pool bound and therefore must unlink on the way out.
    socket_paths: Vec<PathBuf>,
    /// `PHP_FCGI_CHILDREN` for every slot: `0` for one process, `n` for a preforked master.
    ///
    /// Stored because a *replacement* slot must fork what the slot it replaces forked, and
    /// read from the configuration at startup rather than per spawn so a respawn cannot
    /// disagree with the pool's own capacity. Already mapped through
    /// [`SpawnCgiConfig::fcgi_children`], so `1` never reaches php-cgi as "a master plus one
    /// child".
    children: u32,
    max_requests: Option<u64>,
    /// The environment every slot is started with, already merged over `[server.env]`.
    ///
    /// Captured at construction for the same reason `children` is: a replacement slot must be
    /// started on the terms the pool was configured with, not on whatever the file says now.
    env: BTreeMap<String, String>,
    /// Held for the life of the pool; dropping it reaps every slot.
    _guard: LifecycleGuard,
}

/// How many requests one slot may serve at once.
///
/// A counter and a condvar, mirroring `backend::Limit` for an `external` backend's
/// `max_connections`: the request path is synchronous, so a request over capacity has to
/// *block*, and blocking is the feature — the alternative is opening a connection no child
/// is there to accept, which queues in the listen backlog where nothing counts it.
///
/// The capacity is `children.max(1)`. With `children = 0` this is 1, so the observable
/// behaviour of an existing configuration is exactly what it was.
#[derive(Debug)]
struct SlotPermits {
    state: Mutex<usize>,
    released: Condvar,
    capacity: usize,
}

impl SlotPermits {
    fn new(capacity: usize) -> Self {
        SlotPermits {
            state: Mutex::new(0),
            released: Condvar::new(),
            capacity,
        }
    }

    /// Wait until this slot is under capacity, then count the request.
    ///
    /// A poisoned lock is recovered rather than propagated: a panic in one request must not
    /// wedge every later one behind a condvar nobody will signal again.
    fn enter(&self) {
        let mut in_flight = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        while *in_flight >= self.capacity {
            in_flight = self
                .released
                .wait(in_flight)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
        *in_flight += 1;
    }

    /// Give the place back, which is what wakes a waiting request.
    fn leave(&self) {
        let mut in_flight = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *in_flight = in_flight.saturating_sub(1);
        self.released.notify_one();
    }
}

impl CgiPool {
    /// Start a pool from configuration.
    pub fn start(name: &str, config: &SpawnCgiConfig) -> Result<Self> {
        if !config.binary.is_file() {
            return Err(ServerError::Misconfigured {
                backend: name.to_string(),
                reason: format!("{} does not exist", config.binary.display()),
            });
        }

        let guard = LifecycleGuard::new(&format!("volucer-{name}")).map_err(|source| {
            ServerError::Misconfigured {
                backend: name.to_string(),
                reason: format!("could not create the process lifecycle guard: {source}"),
            }
        })?;

        let count = worker_count(config.workers);
        let capacity = config.slot_capacity();
        let mut pool = CgiPool {
            name: name.to_string(),
            slots: Vec::with_capacity(count as usize),
            permits: (0..count).map(|_| SlotPermits::new(capacity)).collect(),
            cursor: AtomicUsize::new(0),
            binary: config.binary.clone(),
            bind_host: config.bind_host.clone(),
            socket_paths: Vec::new(),
            children: config.fcgi_children(),
            max_requests: config.max_requests,
            env: config.env.clone(),
            _guard: guard,
        };

        pool.spawn_every_slot(config)?;

        tracing::info!(
            backend = %pool.name,
            slots = pool.slots.len(),
            binary = %pool.binary.display(),
            "php-cgi pool started"
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
                        SlotAddress::Port(first) => pool.spawn_port_slot(*first),
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
                        "php-cgi exited while binding; trying the next port"
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
        let mut command = Command::new(&self.binary);
        command
            .arg("-b")
            .arg(address.bind_argument(&self.bind_host))
            // php-cgi's own preforking, which the pool now *declares* rather than suppresses.
            //
            // A `php-cgi` started with `-b` is not necessarily one process: it is a parent
            // that forks (or, on Windows, `CreateProcess`es) `PHP_FCGI_CHILDREN` children,
            // all sharing the one bound socket, and each child accepts its own connections.
            // The variable is read from php-cgi's *own* environment (`cgi_main.c:1964`), so
            // leaving it unset inherits whatever a deployment exported for php-fpm — measured
            // at 20 on a 12-core machine, which made `workers` a lie.
            //
            // So the value is always written, and it is always the pool's own decision:
            // `children`, or `0` to mean one process per slot. `0` rather than `1` because
            // php-cgi spawns CHILDREN *in addition to itself* — measured against php-cgi
            // 8.3.31, `0` gives exactly one process and `1` gives two.
            //
            // `fcgi_children()`, not `children`: php-cgi spawns CHILDREN *in addition to
            // itself*, so `children = 1` would be a master plus a child — two PHP boots for a
            // slot that can still serve one request, because one child accepts one connection.
            // Measured: `1` gave two processes and one pid answering. The accessor maps
            // `children <= 1` to `0`, so `children = 1` means what it looks like.
            .env("PHP_FCGI_CHILDREN", self.children.to_string())
            .env("PHP_FCGI_MAX_REQUESTS", "0")
            // The deployment's own variables last, so a table cannot move either number above —
            // the same ordering `VOLUCER_WORKER_SOCKET` uses for a worker's address. These are the
            // only way an application running under `php-cgi` is configured at all: the CGI
            // environment volucer builds per request carries request data and nothing else.
            .envs(&self.env)
            // And the version after them, so it cannot be shadowed.
            .env(volucer_core::config::VERSION_ENV, crate::build::VERSION);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        configure_command(&mut command);

        let child = command.spawn().map_err(|source| ServerError::Spawn {
            binary: self.binary.clone(),
            source,
        })?;

        // Adopt before waiting, so a failure here cannot leave an orphan.
        self._guard
            .adopt(&child)
            .map_err(|source| ServerError::Spawn {
                binary: self.binary.clone(),
                source,
            })?;

        let mut slot = Slot {
            child,
            endpoint: address.endpoint(&self.bind_host)?,
            served: 0,
            clients: Vec::new(),
        };
        wait_until_listening(&mut slot, &self.binary, STARTUP_TIMEOUT)?;
        Ok(slot)
    }

    /// Number of live slots.
    pub fn len(&self) -> usize {
        self.slots.len()
    }

    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
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
        let permits = &self.permits[index];

        // Blocking rather than refusing: the front-end's own connection limit is where a
        // request is refused, and turning a full slot into an error here would make a pool
        // that is merely busy into a source of 502s. With `children = 0` the capacity is one,
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
            slot.prune_dead_clients();
            (slot.endpoint.clone(), slot.clients.pop())
        };

        let (response, client) = exchange(&endpoint, request, taken)?;

        let served = {
            let mut slot = self.slots[index]
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            // The connection goes back so the next request to this slot is warm. Retention is
            // per slot and bounded by capacity, which *is* the number of processes behind the
            // address — so a warm connection is one warm child and cannot starve a sibling.
            slot.clients.push(client);
            slot.served += 1;
            slot.served
        };

        // Recycle on the request count, mirroring `pm.max_requests`: this is what
        // bounds the effect of any leak in the application under test. `0` means
        // never, as it does for php-fpm — see `WorkerConfig::max_requests`.
        //
        // Slot-wide, and deliberately: with children each counts its own requests and php-cgi
        // offers no way to read those from outside, so the only count that exists is the one
        // this slot has served. Replacing the slot replaces the group, which is the unit
        // `killpg` can express anyway.
        if let Some(max) = self.max_requests.filter(|max| *max > 0) {
            if served >= max {
                let mut slot = self.slots[index]
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                // Re-checked under the lock: two concurrent requests can both reach the
                // threshold, and the second must not replace a slot the first has already
                // replaced — that would kill a group that has served nothing.
                if slot.served >= max {
                    self.replace_locked(&mut slot)?;
                }
            }
        }
        Ok(response)
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
                "a php-cgi slot died; restarting"
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

    /// Replace the process in a slot, keeping its address.
    pub fn restart(&self, index: usize) -> Result<()> {
        let mut slot = self.slots[index]
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.replace_locked(&mut slot)
    }

    /// Replace a locked slot's process with a fresh one on the same address.
    ///
    /// The replacement is booted before the old process is killed, so a slot that
    /// cannot be restarted keeps working rather than being left dead.
    fn replace_locked(&self, slot: &mut Slot) -> Result<()> {
        let address = slot.address();
        let mut replacement = self.spawn_slot(&address)?;
        replacement.served = 0;
        let mut old = std::mem::replace(slot, replacement);
        let _ = crate::lifecycle::kill(&mut old.child);
        let _ = old.child.wait();
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
/// machine it runs on is not evidence" defect `internal/plan.md` keeps recording. Taking the
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

    #[test]
    fn a_slot_address_knows_how_to_bind_and_how_to_connect() {
        let port = SlotAddress::Port(19_001);
        assert_eq!(port.bind_argument("127.0.0.1"), "127.0.0.1:19001");
        assert_eq!(
            port.endpoint("127.0.0.1")
                .expect("a port is always an endpoint"),
            Endpoint::Tcp("127.0.0.1:19001".to_string())
        );

        let socket = SlotAddress::Socket(PathBuf::from("/run/volucer/cgi.0.sock"));
        assert_eq!(
            socket.bind_argument("127.0.0.1"),
            "/run/volucer/cgi.0.sock",
            "php-cgi takes a bare path for a unix socket, not the `unix:` prefix"
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
                .endpoint("127.0.0.1")
                .expect("unix sockets exist here"),
            Endpoint::Unix("/run/volucer/cgi.0.sock".to_string())
        );
    }

    #[cfg(not(unix))]
    #[test]
    fn a_unix_slot_address_is_refused_where_the_platform_has_none() {
        let socket = SlotAddress::Socket(PathBuf::from("/run/volucer/cgi.0.sock"));
        let err = socket
            .endpoint("127.0.0.1")
            .expect_err("a unix socket cannot be reached here");
        assert!(err.to_string().contains("cannot be a unix socket"), "{err}");
    }

    /// A Windows pipe slot hands `php-cgi` the bare pipe name and connects to it.
    #[cfg(windows)]
    #[test]
    fn a_pipe_slot_address_binds_and_connects_by_name() {
        let pipe = SlotAddress::Pipe(PathBuf::from(r"\\.\pipe\volucer-cgi.0"));
        assert_eq!(
            pipe.bind_argument("127.0.0.1"),
            r"\\.\pipe\volucer-cgi.0",
            "php-cgi takes the pipe name verbatim, not `host:port`"
        );
        assert_eq!(
            pipe.endpoint("127.0.0.1")
                .expect("a pipe is always an endpoint"),
            Endpoint::Pipe(r"\\.\pipe\volucer-cgi.0".to_string())
        );
        // And the round trip has to come back to the same place, or a recycled slot
        // would bind a differently named pipe.
        assert!(matches!(
            slot_address_of(&pipe.endpoint("127.0.0.1").unwrap()),
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
            .endpoint("127.0.0.1")
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
            .endpoint("127.0.0.1")
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
            served: 0,
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
            binary: PathBuf::from("definitely-not-a-php-cgi"),
            workers: 1,
            bind_host: "127.0.0.1".to_string(),
            base_port: 19_700,
            socket: None,
            children: 0,
            max_requests: None,
            env: Default::default(),
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
