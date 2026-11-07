//! The backend abstraction the dispatcher talks to.
//!
//! One enum covers every backend shape from the configuration, so the
//! dispatcher and both front-ends have exactly one thing to call.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use volucer_core::config::BackendConfig;
use volucer_fcgi::{Endpoint, Request, Response};

use crate::calls::MethodRegistry;
use crate::error::{Result, ServerError};
use crate::pool::CgiPool;
use crate::upstream_pool::Supervisor;
use crate::worker_pool::WorkerPool;

/// How many idle connections to an external backend to hold open, **in total across both paths**.
///
/// Exactly one, which is not an arbitrary small number. `php-fpm` accepts a
/// connection *into a child* and then waits for the request on it, and a child
/// blocked in that read counts as **active** in php-fpm's own scoreboard — so
/// every connection parked there permanently removes one child from the set the
/// master is willing to dispatch to. Retaining more than one therefore costs
/// capacity in proportion to the cap, which is real and measurable: against a
/// 12-child pool, retaining 4 cost 7% of throughput at concurrency 32 (34,250 vs
/// 36,906 rps) and retaining 12 cost 26% (27,407 vs 36,906).
///
/// One is worth that single child, because the reconnect it saves is on the path
/// of *every* request: keeping a warm connection raised concurrency-1 throughput
/// from 2,986 to 3,598 rps, and cost nothing at concurrency 32 (36,497 vs 36,906,
/// inside the noise). More than one buys nothing further — 2 measured 3,578 at
/// concurrency 1, the same within noise as 1.
///
/// `nginx` retains *zero* upstream connections and reconnects for every request,
/// which is the correct default when the front-end is nginx and the connection is
/// a unix socket. volucer's front-end is not nginx, and the reconnect is
/// measurable, so one is kept.
///
/// **One in total, not one per mode.** The pool keeps two lists, because a
/// connection's mode is a property of the file description rather than of the
/// handle and cannot be changed back. That split is about *which* list a
/// connection may live in, not about how many may be parked: the cost above is
/// measured per **child**, and a deployment serving both the blocking and the
/// awaited path from one backend would park two children if each list kept its
/// own. Nothing asserts the combined total if the check is per list, so
/// [`ClientPool::retained`] counts both and [`ClientPool::release`] asks it.
const MAX_IDLE_CONNECTIONS: usize = 1;

/// Connections to one external backend, checked out per request.
///
/// One FastCGI connection carries one request at a time — it is a conversation,
/// and the codec has no request-id demultiplexer, so multiplexing is not an
/// option. A single connection therefore caps the whole backend at one request
/// at a time no matter how large its `pm.max_children` is, which is what
/// this type is the fix for: as many connections as
/// there are requests in flight, each reused when it comes back.
///
/// Connections are not limited *by volucer* unless [`ExternalConfig::max_connections`]
/// says so, because the number in flight is already bounded by the front-end's
/// connection limit and refusing to open a connection turns a busy backend into a
/// 503. The optional cap is back-pressure for a front-end that would otherwise
/// pile up faster than the pool can drain, not a declaration of the pool's size —
/// see `ExternalConfig` for why volucer does not try to discover that.
///
/// Only *retention* is capped unconditionally, at [`MAX_IDLE_CONNECTIONS`], which
/// see for why the number is one.
#[derive(Debug)]
pub struct ClientPool {
    endpoint: Endpoint,
    /// How long one request may take before volucer abandons it.
    ///
    /// Resolved at construction from `[backends.<n>].request_timeout_ms` — see
    /// [`ExternalConfig::request_timeout`], which refuses a zero rather than reading it as "no
    /// limit". **This backend needs it more than the other two, not less**: a `worker` or
    /// `spawn_cgi` slot that stops answering is eventually killed by the pool that owns it, and an
    /// `external` child belongs to `php-fpm`, so without a deadline here a hung request holds a
    /// connection, a permit and (on the awaited path) a runtime task for the life of the process.
    request_timeout: Duration,
    /// Warm connections, checked out by `acquire` and returned by `release`.
    idle: Mutex<Vec<volucer_fcgi::Client>>,
    /// Warm connections driven by a runtime, checked out by `acquire_async`.
    ///
    /// Separate from `idle` because the two are in different modes at the *file description* level:
    /// the awaited path needs `O_NONBLOCK` and no `SO_RCVTIMEO`, and neither can be undone for a
    /// socket a blocking read might touch. See [`ClientPool::acquire_async`].
    async_idle: Mutex<Vec<volucer_fcgi::AsyncClient>>,
    /// Connections parked across **both** lists, so [`MAX_IDLE_CONNECTIONS`] is a total.
    ///
    /// An atomic rather than `idle.len() + async_idle.len()` under one lock, because the two lists
    /// have to stay separate and there is no single lock that could answer the question without
    /// deadlocking against a release that already holds the other. Updated under the list's own lock
    /// and read without one, which is enough: the value only decides whether one more connection may
    /// be kept, and being off by one against a concurrent release is the same answer either way.
    retained: AtomicUsize,
    /// Requests in flight, when the operator asked for a ceiling.
    ///
    /// One counter for both paths, with a condvar for the blocking one and a `Notify` for the awaited
    /// one — see [`Limit`] for why a second counter would be a second limit rather than a detail.
    limit: Option<Arc<Limit>>,
}

impl ClientPool {
    fn new(endpoint: Endpoint, max_connections: Option<usize>, request_timeout: Duration) -> Self {
        ClientPool {
            endpoint,
            request_timeout,
            idle: Mutex::new(Vec::new()),
            async_idle: Mutex::new(Vec::new()),
            retained: AtomicUsize::new(0),
            limit: max_connections.map(Limit::new),
        }
    }

    fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    /// How many connections are parked warm right now, across both lists.
    ///
    /// The total [`MAX_IDLE_CONNECTIONS`] bounds. Exposed for the same reason [`Self::in_flight`] is
    /// — a test has to observe the number the constant is about, and "one per mode" and "one in
    /// total" are indistinguishable from a request's point of view: both serve every request, and
    /// only the number of children the backend has parked says which one happened.
    pub fn retained(&self) -> usize {
        self.retained.load(Ordering::Relaxed)
    }

    /// Whether another connection may be parked, given the ones already are.
    ///
    /// Consulted **after** the list's lock is taken and before a connection is pushed, so two
    /// releases cannot both decide there is room. See [`MAX_IDLE_CONNECTIONS`] for why the answer is
    /// a total rather than a count per list.
    fn may_retain(&self) -> bool {
        self.retained.load(Ordering::Relaxed) < MAX_IDLE_CONNECTIONS
    }

    /// The deadline one request has, for a message that names the key rather than the socket.
    fn request_timeout(&self) -> Duration {
        self.request_timeout
    }

    /// How many requests are inside the `max_connections` cap right now, or `None` when the backend
    /// is uncapped or the count cannot be read without waiting.
    ///
    /// **The permit count, not the number of sockets.** A request waiting for admission is absent
    /// from it, which is exactly the distinction a "the two paths share one limit" test needs: two
    /// counters would each reach 1 and never 2.
    pub fn in_flight(&self) -> Option<usize> {
        self.limit.as_ref().and_then(|limit| limit.in_flight())
    }

    /// Take a connection, reusing a warm one when there is one.
    ///
    /// A connection taken from the pool is not proven alive: the backend may have
    /// closed it since it was returned, and there is no way to tell without
    /// sending something. `request_external` recovers from exactly that, so the
    /// pooled connection is handed out as-is rather than probed — a probe would
    /// cost the same round trip as the request it is trying to avoid.
    ///
    /// With a `max_connections` cap this waits for a permit first, and the returned
    /// [`BackendPermit`] gives it back. The permit is held for the whole request, including
    /// the reconnect a stale connection may need, because the cap is about how many requests
    /// the backend is asked to do at once — not about how many sockets happen to exist.
    fn acquire(&self, backend: &str) -> Result<(volucer_fcgi::Client, Option<BackendPermit>)> {
        let permit = match &self.limit {
            None => None,
            Some(limit) => Some(
                limit
                    .enter()
                    .map(|permit| BackendPermit { _permit: permit })?,
            ),
        };

        let pooled = {
            let mut idle = self
                .idle
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let client = idle.pop();
            if client.is_some() {
                self.retained.fetch_sub(1, Ordering::Relaxed);
            }
            client
        };
        if let Some(client) = pooled {
            return Ok((client, permit));
        }

        volucer_fcgi::Client::connect(&self.endpoint, Some(self.request_timeout))
            .map(|client| (client, permit))
            .map_err(|source| ServerError::Worker {
                backend: backend.to_string(),
                source,
            })
    }

    /// Give a connection back, or drop it when it cannot serve another request.
    fn release(&self, client: volucer_fcgi::Client) {
        if !client.is_reusable() {
            // Dropping closes the socket, which is the correct end for a
            // connection the backend refused to keep. Checked before taking the
            // lock so a busy pool is not locked for a connection it will not keep.
            return;
        }

        let mut idle = self
            .idle
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // Both the room check and the increment happen under this list's lock, so a blocking and an
        // awaited release cannot both conclude there is room for their own connection.
        if idle.is_empty() && self.may_retain() {
            idle.push(client);
            self.retained.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Take a connection for an awaited request.
    ///
    /// **A separate list from `idle`, and it has to be.** Driving a socket from a runtime requires
    /// `O_NONBLOCK` and no `SO_RCVTIMEO`, and both are properties of the *file description* rather
    /// than of the handle — so a blocking `Client` adopted by the runtime could never be handed back
    /// to the blocking path. Keeping two pools is what makes each connection's mode a property of
    /// the list it lives in, instead of something a caller has to remember.
    ///
    /// The retention cap is the same constant, for the same measured reason.
    ///
    /// The place in the cap is taken with [`Limit::enter_async`] rather than on the condvar the
    /// blocking path uses. **Calling `enter` here would park a runtime thread**, which is the exact
    /// defect the `spawn_cgi` slot permits were given a second wake-up to avoid — and it is worse
    /// here, because a queued `external` request has no pool of its own to notice and no supervisor
    /// to kill anything.
    async fn acquire_async(
        &self,
        backend: &str,
    ) -> Result<(volucer_fcgi::AsyncClient, Option<BackendPermit>)> {
        let permit = match &self.limit {
            None => None,
            Some(limit) => Some(
                limit
                    .enter_async()
                    .await
                    .map(|permit| BackendPermit { _permit: permit })?,
            ),
        };

        let pooled = {
            let mut idle = self
                .async_idle
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let client = idle.pop();
            if client.is_some() {
                self.retained.fetch_sub(1, Ordering::Relaxed);
            }
            client
        };
        if let Some(client) = pooled {
            return Ok((client, permit));
        }

        volucer_fcgi::AsyncClient::connect(&self.endpoint, Some(self.request_timeout))
            .await
            .map(|client| (client, permit))
            .map_err(|source| ServerError::Worker {
                backend: backend.to_string(),
                source,
            })
    }

    /// Give an awaited request's connection back, or drop it.
    fn release_async(&self, client: volucer_fcgi::AsyncClient) {
        if !client.is_reusable() {
            return;
        }
        let mut idle = self
            .async_idle
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // The same total as the blocking half — see [`MAX_IDLE_CONNECTIONS`]. Taking this list's lock
        // before asking is what keeps a release from each path from both finding room.
        if idle.is_empty() && self.may_retain() {
            idle.push(client);
            self.retained.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Close every idle connection.
    fn clear(&mut self) {
        let idle = self
            .idle
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        idle.clear();
        // The awaited pool too. A connection driven by a runtime holds the same backend's child, so
        // closing only the blocking half would leave a request's worth of children parked.
        let async_idle = self
            .async_idle
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        async_idle.clear();
        // Both emptied together, so the total is the truth rather than the sum of two histories.
        self.retained.store(0, Ordering::Relaxed);
    }
}

/// One request's claim on the backend's `max_connections` budget.
///
/// Held for the life of the request and dropped at the end of it, which is what returns the
/// permit — there is no `release` to forget to call.
#[derive(Debug)]
struct BackendPermit {
    _permit: LimitPermit,
}

/// A ceiling on how many requests reach one backend at once.
///
/// A counter with two wake-ups rather than an async semaphore, because **both paths share the one
/// count and they wait differently**. The blocking path has to *block* — that is its whole nature —
/// and the awaited path must not: parking a runtime thread is the defect this whole change exists to
/// remove, and the `spawn_cgi` pool's slot permits have the same two-way shape for the same reason.
/// One counter, two ways to wait on it, and which one a caller uses is a property of the path it
/// took — never of the limit.
///
/// **One counter rather than one per kind, and that is a correctness requirement.** `max_connections`
/// is a statement about how many requests the backend sees at once; two counters would make a cap of
/// 1 admit one request of each kind simultaneously, which is two. It is also what
/// [`Backend::in_flight`] reads, so a test can prove the two paths contend rather than assert it from
/// a duration.
#[derive(Debug)]
struct Limit {
    state: Mutex<LimitState>,
    /// Blocking waiters. Signalled on every release, whether or not one is waiting.
    changed: Condvar,
    /// Awaited waiters. Signalled on every release for the same reason, and a signal nobody is
    /// waiting for is a lock-free no-op.
    released_async: tokio::sync::Notify,
}

#[derive(Debug)]
struct LimitState {
    in_flight: usize,
    max: usize,
}

impl Limit {
    fn new(max: usize) -> Arc<Self> {
        Arc::new(Limit {
            state: Mutex::new(LimitState { in_flight: 0, max }),
            changed: Condvar::new(),
            released_async: tokio::sync::Notify::new(),
        })
    }

    /// Wait until a request is under the cap, then count it.
    ///
    /// A poisoned lock is recovered rather than propagated: a panic in one request must not
    /// wedge every later one behind a condvar nobody will signal again.
    fn enter(self: &Arc<Self>) -> std::result::Result<LimitPermit, volucer_fcgi::FcgiError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        while state.in_flight >= state.max {
            state = self
                .changed
                .wait(state)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
        state.in_flight += 1;
        Ok(LimitPermit {
            limit: Arc::clone(self),
        })
    }

    /// [`Self::enter`], without parking the calling thread.
    ///
    /// **The awaited path used to call `enter`, which is a `Condvar::wait` inside an `async fn`.** A
    /// runtime worker that blocks in a condvar is not yielding — the task it was running stays on
    /// that thread and the driver cannot make progress on it — so with the cap reached, every queued
    /// awaited request parked a runtime thread, on a runtime sized at a handful. That is the same
    /// defect the `spawn_cgi` pool had, and it is why this method exists at all.
    ///
    /// The notification is registered **before** the check, so a release that lands between the two
    /// cannot be lost: `enable()` arms it and the loop re-tests after every wake. That ordering is
    /// load-bearing rather than stylistic — a request that missed the wake would wait for a release
    /// that has already happened, for the life of the process.
    async fn enter_async(
        self: &Arc<Self>,
    ) -> std::result::Result<LimitPermit, volucer_fcgi::FcgiError> {
        loop {
            let released = self.released_async.notified();
            tokio::pin!(released);
            released.as_mut().enable();

            {
                let mut state = self
                    .state
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if state.in_flight < state.max {
                    state.in_flight += 1;
                    return Ok(LimitPermit {
                        limit: Arc::clone(self),
                    });
                }
            }

            released.await;
        }
    }

    /// How many requests are inside the cap right now.
    ///
    /// `try_lock`, because the only caller is a test asking whether a permit was taken, and a
    /// diagnostic that waited for the lock would be waiting for the request it is reporting on.
    /// `None` means "not knowable from here", which a caller treats as "not yet".
    fn in_flight(&self) -> Option<usize> {
        self.state.try_lock().ok().map(|state| state.in_flight)
    }
}

/// Releases its place on drop, which is the only way the budget comes back.
#[derive(Debug)]
struct LimitPermit {
    limit: Arc<Limit>,
}

impl Drop for LimitPermit {
    fn drop(&mut self) {
        let mut state = self
            .limit
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.in_flight -= 1;
        // Both signals for one release, so whichever kind of waiter is queued takes the place and
        // the other kind's signal finds nobody waiting. Outside the lock, so the woken request does
        // not immediately contend with the releaser for the value it was just told had changed.
        self.limit.changed.notify_one();
        self.limit.released_async.notify_one();
    }
}

/// Slot contention in a worker pool, gathered for the shutdown report.
///
/// Every worker pool accumulates what its slot mutexes cost, but a library has no
/// business deciding where that goes, so it is surfaced here and the *program*
/// prints it. A server that logs its own diagnostics is a server whose embedder
/// cannot; the CLI has one place to say it and this is the data for that place.
#[derive(Debug, Clone)]
pub struct Contention {
    pub backend: String,
    pub workers: usize,
    /// Requests this pool served. A request replayed on a second slot counts twice.
    pub served: u64,
    /// Mean time a request waited for a slot, in microseconds.
    ///
    /// **`None` when the pool is not measuring**, which is the default: the figures are
    /// read once, at shutdown, so a deployment that never looks must not pay two clock
    /// reads per request to produce them (see `WorkerPool::wait_us`). `None` and `Some(0)`
    /// are different answers — the first is "nothing was measured", the second is "the
    /// pool was not contended" — and reporting the first as the second would make a pool
    /// that was never instrumented look like a pool that was never busy.
    pub mean_us: Option<u64>,
    /// The wait only one request in a hundred exceeded, and the median, in microseconds.
    ///
    /// The pair is the point: a mean says how much waiting there was, and these say how it
    /// was spread. A pool whose p99 is far above its p50 is queueing unfairly - most
    /// requests are served at once and a few wait for whole requests - which a mean cannot
    /// show and which is exactly the shape of a latency tail.
    /// `None` for the same reason [`Self::mean_us`] is.
    pub p50_us: Option<u64>,
    pub p99_us: Option<u64>,
    /// The slot that took the most wait, and how much. `None` when not measuring.
    pub busiest: Option<usize>,
    pub busiest_us: Option<u64>,
    /// Worker-emitted samples this pool refused because its queue was full.
    ///
    /// **The only report of this that a deployment without `[status.metrics]` ever gets.** The counter in the
    /// registry is reachable only by a scrape, and a deployment that was never scraped is exactly the
    /// one whose queue could fill unnoticed — so the figure rides here, to be printed once at
    /// shutdown, rather than being left to a graph nobody was drawing.
    ///
    /// Zero on a healthy pool, which is why the line is only printed when something was lost.
    pub dropped_metrics: u64,
}

/// A backend, ready to serve requests.
#[derive(Debug)]
pub enum Backend {
    /// An already-running FastCGI server, reached over its socket.
    ///
    /// volucer does not manage this pool: systemd and the server itself do, including
    /// `pm.max_requests` and per-pool users. We only speak FastCGI to it. Nothing about the
    /// variant is PHP-specific — a WSGI or Rack server speaking FastCGI is the same shape, and
    /// only the two doc comments below name `php-fpm` because it is the common case.
    External {
        name: String,
        /// Shared behind its own lock: one backend serves both front-ends, so a
        /// `&mut` here would make the whole dispatcher exclusive again.
        ///
        /// An `Arc` because a relayed response outlives the request that opened it: the task
        /// pumping its body returns the connection to the pool after the last byte, so it needs a
        /// handle rather than a borrow.
        pool: Arc<ClientPool>,
    },
    /// A pool of FastCGI processes that volucer starts and supervises: `php-cgi`, or any
    /// command that serves FastCGI on the address it is given.
    ///
    /// The pool is boxed, and that is a measurement rather than a preference: inline it is the
    /// largest variant by a wide margin — `CgiPool` carries a `Vec<Mutex<Slot>>`, a
    /// `Vec<SlotPermits>`, a `BTreeMap` environment and a `LifecycleGuard`, which is a `HANDLE` on
    /// Windows and empty elsewhere. That last difference is why the box is not cosmetic: on Windows
    /// the variant is 304 bytes against `External`'s 96, and `clippy::large_enum_variant` fires
    /// there while a Linux CI run stays quiet. Boxing makes the size the same everywhere and drops
    /// the enum to `External`'s width. A `Backend` is reached through `&self` on the request path,
    /// so the box is one indirection on a path that then does a syscall.
    SpawnCgi { name: String, pool: Box<CgiPool> },
    /// A pool of persistent PHP workers speaking volucer's own protocol.
    ///
    /// The application boots once and stays resident, so PHP does *not* reset
    /// state between requests — the application clears its own request scope.
    ///
    /// Behind an `Arc` because the call plane needs to *find* this pool later: `workers.reset` is answered
    /// by the very pool whose workers are asking, and the dispatcher keeps a [`Weak`](std::sync::Weak)
    /// handle to it rather than a strong one. The `Arc` is not about sharing — nothing else holds it —
    /// and everything above this line would work with the pool inline.
    ///
    /// **`attach` uses this variant too**, and shares it deliberately: from a request's point of view
    /// the two are the same thing — a persistent application, reached over the worker protocol, with a
    /// per-slot exchange — and the only difference is that volucer dialled rather than forked. A
    /// second variant would make every `match` above this line ask the same question twice and get
    /// two answers that had to be kept in agreement.
    Worker { name: String, pool: Arc<WorkerPool> },
    /// An HTTP origin volucer proxies to.
    ///
    /// The client is built in [`Backend::from_config`], because a CA that cannot be read is a
    /// startup failure rather than a 502 on the first request. It is cheap to clone — the pool,
    /// the connector and the configuration are behind `Arc` inside `hyper-util`'s `Client` — and
    /// it is `Send + Sync` without its body being `Sync`, so it fits the `&self` contract the
    /// dispatcher needs.
    Upstream {
        name: String,
        /// The pooled client to the origin.
        ///
        /// Boxed, and that is a measurement rather than a preference: on Linux the unboxed client made
        /// this variant the largest in the enum and `clippy::large_enum_variant` fired — a warning that
        /// was **already** present before this variant grew a `supervisor` field, so the fix belongs
        /// here rather than in the new field. A boxed client is one indirection per request, and the
        /// client is a pool of connections where per-request cost is a syscall anyway.
        ///
        /// The alternative was boxing the `supervisor` and leaving this: that keeps the enum the size
        /// of a connection pool, which is the thing a `Backend` is used *per request* through. The
        /// client is reached on the request path; the supervisor is reached once, at shutdown.
        client: Box<volucer_upstream::UpstreamClient<UpstreamBody>>,
        /// The origin's process, when this backend was asked to start one.
        ///
        /// Behind a `Mutex`, because the process has to be killable from `shutdown` while a watcher
        /// thread is polling it. `None` is the original shape of an upstream: somebody else's process,
        /// which is nginx's or systemd's job, and still the common case.
        supervisor: Option<Mutex<Box<Supervisor>>>,
    },
}

/// The body an inbound HTTP request carries on its way to an origin.
///
/// `hyper::body::Incoming`, boxed by the listener. Boxed rather than generic because a `Backend`
/// is one type for every configured backend, and making the *dispatcher* generic over a body type
/// to accommodate this one arm would drag the type parameter through every CGI call site that has
/// nothing to do with it.
///
/// The error is `io::Error` because that is what the listener already produces for a body it
/// failed to read — see `volucer_http::UnsyncBoxBody`, which the front-end builds for exactly this
/// parameter.
pub type UpstreamBody = volucer_upstream::UpstreamBody;

impl Backend {
    /// Build a backend from its configuration.
    ///
    /// A worker backend's workers may call nothing: this is [`Backend::from_config_with`] and a
    /// `None` registry, which is what a backend whose configuration names no `services` gets.
    pub fn from_config(name: &str, config: &BackendConfig) -> Result<Self> {
        Self::from_config_with(name, config, None)
    }

    /// Build a backend from its configuration, with a method registry for its workers if it is a pool.
    ///
    /// `registry` is ignored for the three kinds that have no channel to install a handler on, and is
    /// passed to the pool for the one that does — the pool installs it on every worker's channel before
    /// that worker's handshake, and again on every replacement.
    /// Whether this backend's pool is keeping its idle workers alive.
    ///
    /// `false` for every backend that is not a worker pool, and for a pool with no `idle_ping_ms`,
    /// one whose workers did not advertise the capability, or one whose keepalive switched itself
    /// off after a failed probe. One answer for all of them, because the question an operator is
    /// asking is "is anything pinging my workers", and the startup and failure log lines say which
    /// case it is.
    pub fn keepalive_running(&self) -> bool {
        match self {
            Backend::Worker { pool, .. } => pool.keepalive_running(),
            _ => false,
        }
    }

    pub fn from_config_with(
        name: &str,
        config: &BackendConfig,
        registry: Option<Arc<MethodRegistry>>,
    ) -> Result<Self> {
        match config {
            BackendConfig::External(external) => {
                let endpoint = Endpoint::parse(&external.socket).map_err(|source| {
                    ServerError::Misconfigured {
                        backend: name.to_string(),
                        reason: format!("{} is not a usable socket: {source}", external.socket),
                    }
                })?;
                // Resolved here rather than defaulted at each connect, so a `request_timeout_ms = 0`
                // is a startup failure naming the key — the same rule `CgiPool::start` follows, and
                // the only place this backend can report it: it has no supervisor to discover it.
                let request_timeout = external.request_timeout(name).map_err(|reason| {
                    ServerError::Misconfigured {
                        backend: name.to_string(),
                        reason,
                    }
                })?;
                Ok(Backend::External {
                    name: name.to_string(),
                    pool: Arc::new(ClientPool::new(
                        endpoint,
                        external.max_connections,
                        request_timeout,
                    )),
                })
            }
            BackendConfig::SpawnCgi(cgi) => Ok(Backend::SpawnCgi {
                name: name.to_string(),
                pool: Box::new(CgiPool::start(name, cgi)?),
            }),
            BackendConfig::Worker(worker) => Ok(Backend::Worker {
                name: name.to_string(),
                pool: crate::worker_pool::shared(WorkerPool::start_with(name, worker, registry)?),
            }),
            BackendConfig::Attach(attach) => Ok(Backend::Worker {
                name: name.to_string(),
                pool: crate::worker_pool::shared(WorkerPool::start_attached(
                    name, attach, registry,
                )?),
            }),
            BackendConfig::Upstream(upstream) => {
                // Validated rather than accepted on faith: `Config::validate` refuses a bad origin,
                // but a `Backend` can be built from a `BackendConfig` that never went through it,
                // and an origin that cannot parse would otherwise become a 502 on every request for
                // a reason the log does not name.
                let spec = volucer_core::config::OriginSpec::parse_with(
                    &upstream.origin,
                    upstream.tls.clone(),
                    upstream.protocol,
                )
                .map_err(|reason| ServerError::Misconfigured {
                    backend: name.to_string(),
                    reason: format!("backends.{name}.origin: {reason}"),
                })?;
                let origin = volucer_upstream::Origin::from_spec(&spec).map_err(|source| {
                    ServerError::Misconfigured {
                        backend: name.to_string(),
                        reason: source.to_string(),
                    }
                })?;
                // The client is built here, so a CA that cannot be read is a startup failure rather
                // than a 502 on the first request — the same rule `ClientPool::new` follows for a
                // php-fpm socket and `CgiPool::start` for a binary.
                // Pooled, and that is the default because an HTTP origin expects `keep-alive`:
                // reconnecting per request throws away a handshake the origin has already paid for.
                // Measured on a hello-world page with 12 Fastify origins, 73,476 rps pooled against
                // 18,073 with pooling off. `idle_timeout_ms = 0` restores the dial-per-request
                // behaviour for an origin that closes connections itself.
                let idle_timeout = match upstream.idle_timeout_ms {
                    0 => None,
                    ms => Some(std::time::Duration::from_millis(ms)),
                };
                let client = volucer_upstream::UpstreamClient::new(origin, idle_timeout).map_err(
                    |source| ServerError::Misconfigured {
                        backend: name.to_string(),
                        reason: source.to_string(),
                    },
                )?;
                // Started *after* the client, so a `command` that cannot be resolved is reported
                // rather than leaving a running process behind a failure, and *before* the backend
                // is returned, so the readiness gate is inside startup — a deployment is not serving
                // until its origin answers.
                let supervisor = match &upstream.command {
                    Some(_) => Some(Mutex::new(Box::new(Supervisor::start(
                        name, upstream, &spec,
                    )?))),
                    None => None,
                };
                Ok(Backend::Upstream {
                    name: name.to_string(),
                    client: Box::new(client),
                    supervisor,
                })
            }
        }
    }

    pub fn name(&self) -> &str {
        match self {
            Backend::External { name, .. }
            | Backend::SpawnCgi { name, .. }
            | Backend::Worker { name, .. }
            | Backend::Upstream { name, .. } => name,
        }
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Backend::External { .. } => "external",
            Backend::SpawnCgi { .. } => "spawn_cgi",
            Backend::Worker { .. } => "worker",
            Backend::Upstream { .. } => "upstream",
        }
    }

    /// Whether this backend runs the application rather than interpreting a path.
    ///
    /// A worker is told the request's `script_filename` but does not switch file on it:
    /// it routes on `REQUEST_URI`, and the path is metadata. That is what makes a route
    /// to a worker able to skip resolving the path to a script at all.
    pub fn is_worker(&self) -> bool {
        matches!(self, Backend::Worker { .. })
    }

    /// Whether this backend reads the path as a file to execute.
    ///
    /// The question the route table's `stat` turns on, and the reason it is not `!is_worker()`: an
    /// upstream does not interpret the path either, for a different reason — it is an HTTP peer
    /// that routes on the URL itself, so volucer stat-ing the path first would check a file the
    /// origin never reads. See [`BackendConfig::interprets_path`], which is where the rule and its
    /// reasoning live; this is the same answer for the built backend.
    pub fn interprets_path(&self) -> bool {
        matches!(self, Backend::External { .. } | Backend::SpawnCgi { .. })
    }

    /// Whether the backend itself resets state between requests.
    ///
    /// **Only meaningful for the CGI-shaped kinds.** An upstream is somebody else's runtime
    /// and this cannot answer for it — which is why it is not the question the front-end asks.
    /// [`BackendConfig::request_scope`] owns the three-answer version and the text `check` prints;
    /// this is the two-answer version, kept for the callers that genuinely mean "is this a
    /// per-request process".
    ///
    /// Both FastCGI shapes answer `true`: `php-fpm` resets through PHP's SAPI and a `spawn_cgi`
    /// slot through whatever the configured command is. Which mechanism it is has no bearing on
    /// the answer.
    pub fn resets_per_request(&self) -> bool {
        matches!(self, Backend::External { .. } | Backend::SpawnCgi { .. })
    }

    /// Send a request to this backend.
    ///
    /// `&self`: a backend is shared between the two front-ends and across
    /// request threads, so it must not need exclusive access. Each pool does its
    /// own synchronisation, and only around the work that actually needs it.
    pub fn request(&self, request: &Request) -> Result<Response> {
        match self {
            Backend::External { .. } => self.request_external(request),
            Backend::SpawnCgi { pool, .. } => pool.request(request),
            Backend::Worker { pool, .. } => pool.request(request),
            Backend::Upstream { name, .. } => Err(ServerError::WrongProtocol {
                backend: name.clone(),
            }),
        }
    }

    /// [`Backend::request`], without parking the calling thread while the worker thinks.
    ///
    /// Only a worker pool can answer this way, and that is not an accident of implementation: the
    /// three CGI kinds reach a process through a blocking pipe read on a thread this crate owns, so
    /// there is nothing to await. A backend that cannot answer async is answered synchronously and
    /// the caller still gets a correct response — it simply keeps its thread while it waits, which is
    /// what it did before this method existed.
    pub async fn request_async(&self, request: &Request) -> Result<Response> {
        match self {
            Backend::Worker { pool, .. } => pool.request_async(request).await,
            // `external` is awaited too. It used to fall through to `request` on the grounds that
            // `volucer-fcgi` had no `async fn` — which was true, and cost a blocking-pool thread
            // per in-flight request: measured at 185 threads against nginx's 5 at concurrency 100,
            // and 78.9 us of front-end CPU per request against 46.3 for the awaited worker path.
            // `volucer_fcgi::AsyncClient` is that `async fn`.
            Backend::External { name, pool } => Self::external_async(name, pool, request).await,
            // **`spawn_cgi` is awaited too, and it took a change to the slot lifecycle rather than to
            // a socket.** Its permit used to be taken with a `Condvar`, which parks whichever thread
            // asked — a runtime thread, on this path, twelve of which is the whole runtime. That one
            // count now has a second way to wait on it (`tokio::sync::Notify`) used when the request
            // is awaited, and the pool drives the connections it already owned from a runtime of its
            // own, so the same request is answered without parking anything. See
            // `CgiPool::request_async` and `CgiPool::drives_connections`.
            //
            // The guard is not enough on its own: `request_async` is unix-only, because a Windows slot
            // address may be a named pipe, which no runtime can drive. On that platform the `other`
            // arm answers it and `drives_connections` is `false`, so both statements are true and the
            // blocking path is the one that runs.
            #[cfg(unix)]
            Backend::SpawnCgi { pool, .. } if pool.drives_connections() => {
                pool.request_async(request).await
            }
            other => other.request(request),
        }
    }

    /// Ask an `external` backend for a response, across an `await`.
    ///
    /// The same sequence as [`Backend::request_external`] — take a connection, send, retry once on a
    /// reused connection the backend closed, give it back — with the socket driven by the runtime
    /// instead of a parked thread. The retry rule is identical because it is a property of the
    /// backend (php-fpm closes idle connections), not of the transport.
    ///
    /// # The deadline is enforced here, by `tokio::time::timeout`
    ///
    /// **It has to be, because this path has no socket timeout at all.** The blocking client's
    /// deadline is its `SO_RCVTIMEO`, set by `Endpoint::connect`; `prepare_for_async` *clears* it,
    /// since a pending `epoll` operation against a receive timeout is documented to panic. So
    /// `request_timeout` reaches this function as a connect budget and a number to quote in the
    /// error, and without the wrapper a backend that accepts the request and never answers would
    /// hold this future — and the `max_connections` permit — for ever, shrinking the pool one
    /// request at a time until nothing is admitted. `volucer_fcgi::AsyncClient::connect`'s own doc
    /// says as much: *"reads are bounded by the caller's own deadline"*, and before this existed
    /// there was no caller deadline on this backend to bound them with.
    ///
    /// Nothing is retried after an expiry: a timed-out request has already been waited for once,
    /// and spending a second budget on a server that is plainly not answering is what
    /// [`is_deadline_expired`] exists to prevent.
    async fn external_async(name: &str, pool: &ClientPool, request: &Request) -> Result<Response> {
        let timeout = pool.request_timeout();
        let (mut client, _permit) = pool.acquire_async(name).await?;
        let reused = client.is_reusable();

        for attempt in 0..2 {
            match tokio::time::timeout(timeout, client.request(request)).await {
                Ok(Ok(response)) => {
                    pool.release_async(client);
                    return Ok(response);
                }
                // **The connection is not returned after an expiry.** It is mid-exchange with a
                // server that did not answer, so the pool must not find it in `async_idle`: it is
                // dropped with `client`, which closes the socket and hands the child back.
                Ok(Err(source))
                    if reused
                        && attempt == 0
                        && is_transport_error(&source)
                        && !is_deadline_expired(&source) =>
                {
                    tracing::debug!(
                        backend = %name,
                        "the backend closed a reused connection; reconnecting"
                    );
                    client = volucer_fcgi::AsyncClient::connect(pool.endpoint(), Some(timeout))
                        .await
                        .map_err(|source| ServerError::Worker {
                            backend: name.to_string(),
                            source,
                        })?;
                }
                Ok(Err(source)) => return Err(deadline_or_worker(name, timeout, source)),
                Err(_) => return Err(deadline_expired(name, timeout)),
            }
        }

        Err(ServerError::Worker {
            backend: name.to_string(),
            source: volucer_fcgi::FcgiError::IncompleteResponse {
                request_id: volucer_fcgi::REQUEST_ID,
                complete: false,
            },
        })
    }

    /// [`Backend::request_relayed`], without parking the calling thread while the worker thinks.
    ///
    /// A worker answers this way; everything else is answered by the synchronous relayed path, which
    /// is correct for a CGI process (its body is relayed by a thread this crate owns either way) and
    /// for an upstream (there is nothing to relay — it is already an HTTP response).
    pub async fn request_relayed_async(
        &self,
        request: &Request,
        buffer: usize,
        relay: volucer_fcgi::Relay,
    ) -> Result<crate::pool::Delivery> {
        match self {
            Backend::Worker { pool, .. } => pool.request_relayed_async(request, relay).await,
            Backend::External { name, pool } => {
                Self::external_relayed_async(name, pool, request, buffer, relay).await
            }
            // **A `spawn_cgi` slot is awaited too, when its own address is one a runtime can drive.**
            // It is a change to the slot lifecycle rather than to a socket — the permit used to be
            // taken with a `Condvar` that parks whichever thread asked — so one admission count now
            // has a `Notify` for awaited requests as well; see `CgiPool::request_relayed_async`.
            //
            // The `false` arm is a Windows named-pipe slot, which no runtime can register. It keeps
            // the blocking path, and that is a correctness fallback rather than a performance one.
            #[cfg(unix)]
            Backend::SpawnCgi { pool, .. } if pool.drives_connections() => {
                pool.request_relayed_async(request, buffer, relay).await
            }
            other => other.request_relayed(request, buffer, relay),
        }
    }

    /// Ask this backend for a response to a request whose body is still arriving.
    ///
    /// Only a worker pool can take one. A CGI process cannot: its environment declares the body's
    /// length in `CONTENT_LENGTH` before `STDIN` is written, and the process will not read a byte
    /// until it has it. An `upstream` never reaches here because the front-end streams to an origin
    /// on its own path, before any of this is asked.
    pub fn request_streaming(
        &self,
        request: &Request,
        chunks: Box<
            dyn FnMut() -> std::result::Result<Option<Vec<u8>>, volucer_worker::WorkerError> + Send,
        >,
    ) -> Result<Response> {
        match self {
            Backend::Worker { pool, .. } => pool.request_streaming(request, chunks),
            Backend::External { name, .. } | Backend::SpawnCgi { name, .. } => {
                Err(ServerError::WrongProtocol {
                    backend: name.clone(),
                })
            }
            Backend::Upstream { name, .. } => Err(ServerError::WrongProtocol {
                backend: name.clone(),
            }),
        }
    }

    /// Whether this backend accepts a request body whose length it is not told.
    pub fn streams_request_bodies(&self) -> bool {
        match self {
            Backend::Worker { pool, .. } => pool.streams_request_bodies(),
            // A CGI process needs `CONTENT_LENGTH` before it reads `STDIN`, so there is no way to
            // hand it a body of unknown length. `false` is not a limitation being reported, it is
            // the protocol.
            Backend::External { .. } | Backend::SpawnCgi { .. } => false,
            // An origin is streamed to on its own path, never through this question.
            Backend::Upstream { .. } => false,
        }
    }

    /// Ask this backend for a response, relaying the body when it does not fit in `buffer`.
    ///
    /// **All three CGI-shaped kinds relay; an upstream says so by returning a whole response.** A
    /// worker's answer is framed by volucer's own protocol, and since `flags::STREAMED` exists the
    /// worker itself decides whether to stream — so this asks for a stream and answers `Buffered`
    /// when the worker measured its response or the policy refuses to relay it. An upstream already
    /// streams and needs nothing here, so `Buffered` from it is the honest answer rather than a
    /// missing feature.
    pub fn request_relayed(
        &self,
        request: &Request,
        buffer: usize,
        relay: volucer_fcgi::Relay,
    ) -> Result<crate::pool::Delivery> {
        match self {
            Backend::SpawnCgi { pool, .. } => pool.request_relayed(request, buffer, relay),
            Backend::External { name, pool } => {
                Self::external_relayed(name, pool, request, buffer, relay)
            }
            Backend::Worker { pool, .. } => pool.request_relayed(request, relay),
            Backend::Upstream { .. } => self.request(request).map(crate::pool::Delivery::Buffered),
        }
    }

    /// Whether this backend is the kind a gRPC call can be delivered to.
    ///
    /// **Only a `worker` (or an `attach`, which is a worker reached differently) answers.** The
    /// other three kinds are not gRPC handlers for three different reasons rather than one: an
    /// `external` or `spawn_cgi` backend is a CGI process with no channel the server could hand
    /// method bytes to and a request scope that resets underneath it, and an `upstream` speaks gRPC
    /// itself — it is tier A, and the server is a proxy for it rather than its handler.
    ///
    /// This is only half the question. *Which* services are granted is a reloadable answer and lives
    /// in the configuration generation a request is served under, not in the backend — see
    /// `volucer_watch::Reloaded::grpc_grants`. Keeping the two apart is what makes a grant
    /// changeable without rebuilding a pool, because nothing here is a property of a running process.
    pub fn accepts_grpc(&self) -> bool {
        matches!(self, Backend::Worker { .. })
    }

    /// Whether this backend can serve a request right now.
    ///
    /// **False for a worker pool with no free slot, and true for everything else**, which is the
    /// asymmetry a readiness probe needs rather than a general statement about health. A pool whose
    /// workers are all busy is ready in the sense that matters — it will answer, possibly after a
    /// wait — while a pool whose workers are still handshaking will not answer at all. The
    /// distinction is drawn by the pool itself; see [`WorkerPool::is_ready`].
    ///
    /// The three CGI-shaped kinds that are not worker pools answer `true` unconditionally, because
    /// volucer does not own their boot: an `external` server is already running and supervised by
    /// systemd, and a `spawn_cgi` pool's processes are up before the pool is built — the
    /// constructor waits for the port. A server that made a readiness probe wait for a process it did
    /// not start would be reporting on somebody else's runtime.
    pub fn is_ready(&self) -> bool {
        match self {
            Backend::Worker { pool, .. } => pool.is_ready(),
            Backend::External { .. } | Backend::SpawnCgi { .. } | Backend::Upstream { .. } => true,
        }
    }

    /// Why this backend cannot serve yet, or `None` when it can.
    pub fn readiness_reason(&self) -> Option<String> {
        match self {
            Backend::Worker { name, pool } if !pool.is_ready() => Some(format!(
                "{name}: {} of {} workers ready",
                pool.free_slots(),
                pool.len()
            )),
            _ => None,
        }
    }

    /// Publish this backend's gauges, when it has any to publish.
    ///
    /// Only a worker pool has numbers worth graphing: an `external` pool is somebody else's to
    /// report on, and a `spawn_cgi` pool has no per-worker telemetry because a FastCGI process has
    /// no channel
    /// to send any. A backend that published zeroes for those would make `rr_http_total_workers`
    /// claim a number nobody counted.
    pub fn publish_metrics(&self, registry: &volucer_metrics::Registry) {
        match self {
            Backend::Worker { pool, .. } => pool.publish_metrics(registry),
            Backend::External { .. } | Backend::SpawnCgi { .. } | Backend::Upstream { .. } => {}
        }
    }

    /// Replace the processes that run this backend's application, where that is possible without
    /// dropping a request.
    ///
    /// The one implementation of a rolling restart's *per-backend* half, so that a `SIGUSR2`, a
    /// `workers.restart` call and the `volucer workers restart` command cannot disagree about what
    /// a sweep is. What differs between the four kinds is genuinely different, and is stated here
    /// rather than left for a caller to discover:
    ///
    /// | Kind | What a sweep does |
    /// |---|---|
    /// | `worker` / `attach` | each idle slot is respawned in place; `attach` is refused as unmanaged |
    /// | `spawn_cgi` | each idle slot's process is replaced on its own address |
    /// | `external` (`php-fpm`) | nothing — reported as unmanaged, with the reason |
    /// | `upstream` | the origin is restarted if volucer supervises it; in-flight requests are lost |
    ///
    /// The `upstream` arm is the one that is not a slot sweep and does not pretend to be: an HTTP
    /// origin has no slots, so there is no idle state to wait for, and the report says one process
    /// was replaced rather than inventing a slot index. An origin volucer does not supervise is
    /// unmanaged like `php-fpm` — the process is somebody else's, which is the default shape of an
    /// `upstream`.
    pub fn respawn_idle(&self) -> crate::respawn::RespawnReport {
        match self {
            Backend::Worker { pool, .. } => pool.respawn_idle(),
            Backend::SpawnCgi { pool, .. } => pool.respawn_idle(),
            Backend::External { name, pool } => crate::respawn::RespawnReport::unmanaged_by(
                name.clone(),
                0,
                format!(
                    "this backend points at an already-running FastCGI server ({}), which \
                     volucer does not start, so it has no process to replace; reload it with \
                     php-fpm's own `kill -USR2`, or restart its service",
                    pool.endpoint()
                ),
            ),
            Backend::Upstream {
                name,
                supervisor: Some(supervisor),
                ..
            } => {
                let mut report = crate::respawn::RespawnReport::new(name.clone());
                let supervisor = supervisor
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                match supervisor.restart() {
                    Ok(()) => report.replaced.push(0),
                    Err(err) => {
                        tracing::error!(backend = %name, %err, "could not restart the origin");
                        report.failed.push(0);
                    }
                }
                report
            }
            Backend::Upstream {
                name,
                supervisor: None,
                ..
            } => crate::respawn::RespawnReport::unmanaged_by(
                name.clone(),
                0,
                "this backend proxies to an origin volucer did not start (no `command` is \
                 configured), so restarting it is the origin's own deploy's job"
                    .to_string(),
            ),
        }
    }

    /// Serve a request from an externally managed FastCGI server.
    ///
    /// A connection is taken from the pool for the duration of one request, so
    /// the backend serves as many requests at once as it has workers rather than
    /// as many as it has *sockets*.
    ///
    /// The retry is not belt-and-braces: `FCGI_KEEP_CONN` is only a *request*,
    /// and `php-cgi` in particular closes the connection after each response
    /// even when keep-alive was asked for. Verified against php-cgi 8.3.31: the
    /// second request on a reused connection fails with a connection reset. So a
    /// reused connection that fails at the transport level is re-established
    /// once, which is indistinguishable from the client's point of view and
    /// immune to the backend's choice.
    fn request_external(&self, request: &Request) -> Result<Response> {
        let Backend::External { name, pool } = self else {
            unreachable!("only called for the external backend")
        };

        let (mut client, _permit) = pool.acquire(name)?;
        let reused = client.is_reusable();
        let timeout = pool.request_timeout();

        for attempt in 0..2 {
            match client.request(request) {
                Ok(response) => {
                    pool.release(client);
                    return Ok(response);
                }
                Err(source) if reused && attempt == 0 && is_transport_error(&source) => {
                    // The backend closed a connection it had agreed to keep; ask
                    // for a fresh one. The failed client is dropped here, so a
                    // connection that cannot be trusted never goes back in.
                    tracing::debug!(
                        backend = %name,
                        "the backend closed a reused connection; reconnecting"
                    );
                    // **The replacement carries the deadline too.** A reconnect with `None` would
                    // hand the retry — and every later read on that connection — no `SO_RCVTIMEO`,
                    // so one stale-socket recovery would quietly exempt the connection from
                    // `request_timeout_ms` for the rest of its life.
                    client = volucer_fcgi::Client::connect(pool.endpoint(), Some(timeout))
                        .map_err(|source| ServerError::Worker {
                            backend: name.clone(),
                            source,
                        })?;
                }
                Err(source) => return Err(deadline_or_worker(name, timeout, source)),
            }
        }

        Err(ServerError::Worker {
            backend: name.clone(),
            source: volucer_fcgi::FcgiError::IncompleteResponse {
                request_id: volucer_fcgi::REQUEST_ID,
                complete: false,
            },
        })
    }

    /// Ask an `external` backend for a response, relaying the body when it does not fit in `buffer`.
    ///
    /// # Why the connection is not returned to the pool when it relays
    ///
    /// The client is mid-response: the pool would hand it to the next request with the previous
    /// response's records still in front of it. So a relayed response *takes* the connection for its
    /// duration, and the retention cap means the pool simply opens another one afterwards — which is
    /// exactly what `MAX_IDLE_CONNECTIONS` and the `max_connections` permit already model, because the
    /// permit is held for the whole request rather than for the head.
    ///
    /// The permit therefore has to outlive this function, which is why it moves into the reader
    /// thread. Dropping it here would let the pool exceed `max_connections` for as long as a download
    /// takes.
    ///
    /// **`on_end` sees the client and deliberately drops it, while its awaited twin returns it, and
    /// the asymmetry is not an oversight — it is the transport.** `spawn_reader` drains the response
    /// on the reader thread, so by the time `on_end` runs the client *is* reusable and the awaited
    /// path hands it to `release_async`. The blocking path could do the same, and does not: the
    /// reader thread is a `std::thread`, and putting a `Mutex<Vec<Client>>` push on it means the
    /// blocking pool's lock is taken from a thread that is not a request thread — a lock held across
    /// the end of a download rather than the start of one. It costs one reconnect for a streamed
    /// response on this path only, which is the trade `MAX_IDLE_CONNECTIONS` already made for the
    /// buffered one.
    fn external_relayed(
        name: &str,
        pool: &ClientPool,
        request: &Request,
        buffer: usize,
        relay: volucer_fcgi::Relay,
    ) -> Result<crate::pool::Delivery> {
        let (mut client, permit) = pool.acquire(name)?;
        let reused = client.is_reusable();
        let timeout = pool.request_timeout();

        // One retry, on the same rule as `request_external`: a reused connection the backend has
        // closed is indistinguishable from a working one until it is written to, and re-establishing
        // it costs one connect instead of a 502.
        let mut attempt = 0;
        let delivery = loop {
            match client.request_streaming(request, buffer, relay) {
                Ok(delivery) => break delivery,
                Err(source) if reused && attempt == 0 && is_transport_error(&source) => {
                    attempt += 1;
                    tracing::debug!(
                        backend = %name,
                        "the backend closed a reused connection; reconnecting"
                    );
                    client = volucer_fcgi::Client::connect(pool.endpoint(), Some(timeout))
                        .map_err(|source| ServerError::Worker {
                            backend: name.to_string(),
                            source,
                        })?;
                }
                Err(source) => return Err(deadline_or_worker(name, timeout, source)),
            }
        };

        match delivery {
            volucer_fcgi::Delivery::Buffered(response) => {
                pool.release(client);
                drop(permit);
                Ok(crate::pool::Delivery::Buffered(response))
            }
            volucer_fcgi::Delivery::Streamed(stream) => {
                let report = crate::relay::spawn_reader(
                    name.to_string(),
                    client,
                    stream,
                    move |_client, _stream| {
                        // The client is dropped rather than returned — see this function's own note
                        // on why the blocking arm and its awaited twin differ. The permit, which is
                        // what `max_connections` actually bounds, is given back here and only here,
                        // because a relayed response holds its place for as long as the download
                        // takes.
                        drop(permit);
                    },
                )
                .map_err(|source| ServerError::Spawn {
                    binary: std::path::PathBuf::from("volucer-relay"),
                    source,
                })?;
                Ok(crate::pool::Delivery::Relayed(report))
            }
        }
    }

    /// Ask an `external` backend for a response, relaying the body when it does not fit in `buffer`.
    ///
    /// The awaited twin of [`Backend::external_relayed`]. The *rules* are identical — the connection
    /// is taken out of the pool, never returned mid-response, the permit is held until the body has
    /// been drained, and a reused connection the backend closed is retried once. What differs is what
    /// pumps the body: a `tokio` task that `await`s each chunk, instead of a thread that blocks on it.
    ///
    /// This is the case that made the async work necessary rather than optional. Before it existed,
    /// `request_relayed_async` fell through to the synchronous relayed path for `external`, so the
    /// blocking FastCGI read ran *on a runtime worker* — the exact stall the original comment on
    /// `awaits_its_backend` warned about. It showed up as throughput going **down** (44.4k to 39.0k)
    /// while front-end CPU went down too (78.9 to 54.4 us), which is what a serialised runtime looks
    /// like from the outside.
    ///
    /// # The deadline is per chunk, not per body
    ///
    /// A large response legitimately takes longer than `request_timeout_ms`, so what that number
    /// means here is "the backend stopped producing", which is exactly one read. Without it a body
    /// that stalls half way through parks this task — and the permit, and a connection from
    /// `php-fpm` — for the life of the process, and unlike a `spawn_cgi` slot there is no supervisor
    /// here to notice.
    async fn external_relayed_async(
        name: &str,
        pool: &Arc<ClientPool>,
        request: &Request,
        buffer: usize,
        relay: volucer_fcgi::Relay,
    ) -> Result<crate::pool::Delivery> {
        let timeout = pool.request_timeout();
        let (mut client, permit) = pool.acquire_async(name).await?;
        let reused = client.is_reusable();

        let mut attempt = 0;
        let delivery = loop {
            match tokio::time::timeout(timeout, client.request_streaming(request, buffer, relay))
                .await
            {
                Ok(Ok(delivery)) => break delivery,
                Ok(Err(source))
                    if reused
                        && attempt == 0
                        && is_transport_error(&source)
                        && !is_deadline_expired(&source) =>
                {
                    attempt += 1;
                    tracing::debug!(
                        backend = %name,
                        "the backend closed a reused connection; reconnecting"
                    );
                    client = volucer_fcgi::AsyncClient::connect(pool.endpoint(), Some(timeout))
                        .await
                        .map_err(|source| ServerError::Worker {
                            backend: name.to_string(),
                            source,
                        })?;
                }
                Ok(Err(source)) => return Err(deadline_or_worker(name, timeout, source)),
                Err(_) => return Err(deadline_expired(name, timeout)),
            }
        };

        match delivery {
            volucer_fcgi::Delivery::Buffered(response) => {
                pool.release_async(client);
                drop(permit);
                Ok(crate::pool::Delivery::Buffered(response))
            }
            volucer_fcgi::Delivery::Streamed(stream) => {
                let head = stream.head().to_vec();
                let head_duration = stream.head_duration();
                let verdict = crate::relay::Verdict::new();
                // The pump publishes through its own clone of the handles. `Verdict` clones the two
                // `Arc`s and never the answer, so there is one verdict either way.
                let published = verdict.clone();
                let (sender, receiver) = crate::relay::channel();
                let backend = name.to_string();
                let pool = Arc::clone(pool);

                tokio::spawn(async move {
                    let mut stream = stream;
                    let mut failed = false;
                    loop {
                        // Every chunk is bounded by the same deadline as the head — see this
                        // function's own note. A per-chunk budget rather than a whole-body one,
                        // because a large response legitimately takes longer than
                        // `request_timeout_ms`; what the number means here is "the backend stopped
                        // producing", which is exactly one read.
                        match tokio::time::timeout(timeout, client.next_chunk(&mut stream)).await {
                            Ok(Ok(Some(chunk))) => {
                                if sender.send(bytes::Bytes::from(chunk)).await.is_err() {
                                    // The client went away: drain rather than abandon, so the
                                    // connection is not left half-read and can go back to the
                                    // pool. Each read is bounded for the same reason as the loop
                                    // above — a backend that stalls mid-drain must not park this
                                    // task for ever either.
                                    while let Ok(Ok(Some(_))) = tokio::time::timeout(
                                        timeout,
                                        client.next_chunk(&mut stream),
                                    )
                                    .await
                                    {}
                                    break;
                                }
                            }
                            Ok(Ok(None)) => break,
                            Ok(Err(err)) => {
                                tracing::warn!(backend = %backend, %err, "a streamed response failed part way through");
                                failed = true;
                                break;
                            }
                            Err(_) => {
                                tracing::warn!(
                                    backend = %backend,
                                    timeout_ms = timeout.as_millis() as u64,
                                    "a streamed response stalled past `request_timeout_ms`; \
                                     abandoning the connection"
                                );
                                failed = true;
                                break;
                            }
                        }
                    }
                    let complete = stream.is_finished() && !failed;
                    published.publish(stream.stderr(), complete);
                    // The permit is what `max_connections` actually bounds, so it is given back
                    // exactly here — after the body, never before.
                    drop(permit);
                    pool.release_async(client);
                });

                Ok(crate::pool::Delivery::Relayed(crate::relay::Relayed::new(
                    head,
                    receiver,
                    head_duration,
                    verdict,
                )))
            }
        }
    }

    /// Whether this backend is a worker pool whose slot contention should be
    /// reported when the server stops. `None` for every other shape.
    pub fn worker_contention(&self) -> Option<Contention> {
        let Backend::Worker { name, pool } = self else {
            return None;
        };
        let busiest = pool.busiest_slot_wait_us();
        Some(Contention {
            backend: name.clone(),
            workers: pool.len(),
            served: pool.requests_served(),
            mean_us: pool.mean_slot_wait_us(),
            p50_us: pool.slot_wait_percentile_us(0.50),
            p99_us: pool.slot_wait_percentile_us(0.99),
            busiest: busiest.map(|(slot, _)| slot),
            busiest_us: busiest.map(|(_, us)| us),
            dropped_metrics: pool.dropped_metrics(),
        })
    }

    /// Requests this backend is admitting right now, or `None` when it has no counter to report.
    ///
    /// **Exists for one test and one property**: that a blocking and an awaited request to the same
    /// `external` backend contend for *one* `max_connections` budget rather than one each. That
    /// cannot be asserted from a duration — with two counters the second request simply opens a
    /// second connection, so the wall time is unchanged and the bug is invisible — and it cannot be
    /// asserted from the connection count either, because a connection is what the cap is *about*
    /// rather than what it *is*. The admission count is the thing itself, so it is the thing
    /// exposed. Reported as the count and not as a bool, so a test can also say the cap is being
    /// reached rather than merely shared.
    ///
    /// `None` for every other kind: a worker pool counts slot contention instead, and a CGI slot
    /// publishes [`CgiPool::in_flight`](crate::CgiPool::in_flight) per slot.
    pub fn in_flight(&self) -> Option<usize> {
        match self {
            Backend::External { pool, .. } => pool.in_flight(),
            _ => None,
        }
    }

    /// Warm connections this backend is holding open, or `None` when it holds none by design.
    ///
    /// See [`ClientPool::retained`] for why the number is exposed: [`MAX_IDLE_CONNECTIONS`] is a
    /// **total**, and a per-mode check would park one connection on each path — two children of
    /// somebody else's `php-fpm` for a deployment that serves both front-ends from one backend, which
    /// is twice the cost the constant was derived to avoid.
    pub fn retained(&self) -> Option<usize> {
        match self {
            Backend::External { pool, .. } => Some(pool.retained()),
            _ => None,
        }
    }

    /// Stop the backend and everything it started.
    ///
    /// `&mut self`, and only reachable by the owner: `Drop` is what actually
    /// reaps the processes, so nothing on the request path calls this.
    pub fn shutdown(&mut self) {
        match self {
            Backend::SpawnCgi { pool, .. } => pool.shutdown(),
            Backend::Worker { pool, .. } => {
                // `get_mut` because the pool is now behind an `Arc`: the dispatcher keeps a *weak*
                // handle to it so that `workers.reset` can find it, and `get_mut` fails only if
                // something holds a strong one — which nothing but this enum does. A failure here
                // would be a leaked graceful stop rather than a leaked process, since `Drop` still
                // terminates every worker; it is logged rather than ignored so that a future clone
                // somewhere else is visible instead of silent.
                match Arc::get_mut(pool) {
                    Some(pool) => pool.shutdown(),
                    None => tracing::warn!(
                        "a worker pool could not be stopped gracefully because it is still shared; \
                         its workers will be killed rather than drained"
                    ),
                }
            }
            Backend::Upstream { supervisor, .. } => {
                // Nothing was started unless a `command` asked for it, and then what was started is
                // ours to stop: the readiness gate has already been passed, so the process is
                // running and would otherwise hold the socket until the machine rebooted on a
                // platform where the lifecycle guard does nothing (which is every Unix).
                if let Some(supervisor) = supervisor {
                    supervisor
                        .get_mut()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .shutdown();
                }
            }
            Backend::External { pool, .. } => {
                // Nothing was started, so there is nothing to stop; closing the connections is the
                // whole cleanup. `get_mut` because the pool is behind an `Arc` now — same rule as
                // the worker pool above, and the same warning if something outlived the dispatcher.
                match Arc::get_mut(pool) {
                    Some(pool) => pool.clear(),
                    None => tracing::warn!(
                        "an external pool could not be drained because a connection is still \
                         shared; its sockets will be closed when the last request finishes"
                    ),
                }
            }
        }
    }
}

/// Whether a failure means the connection is gone rather than the request being
/// refused.
/// Whether a failure is the *deadline expiring* rather than a dropped connection.
///
/// **Separate from [`is_transport_error`], and the distinction has to exist.** A timed-out request
/// has already been waited for once; retrying it spends a second whole budget on a worker that is
/// plainly not answering, so `request_timeout_ms = 1000` would take two seconds to fail and
/// `30000` would take a minute. Measured against the built binary: a request to a fixture that
/// sleeps three seconds, with the deadline set to one, answered `504` after 2.1 s — the deadline
/// plus the retry — and the message named a socket timeout rather than the deadline the operator
/// configured.
///
/// A `Broken pipe` or a reset, by contrast, is what a reconnecting retry is *for*: the backend
/// closed a connection it had agreed to keep, and one more attempt against a fresh connection is
/// the ordinary fix.
pub fn is_deadline_expired(err: &volucer_fcgi::FcgiError) -> bool {
    match err {
        volucer_fcgi::FcgiError::Io(io) => matches!(
            io.kind(),
            std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
        ),
        _ => false,
    }
}

///
/// Shared so both backends treat a dropped connection the same way.
///
/// **A deadline is not a dropped connection**, and it used to be treated as one: `TimedOut` and
/// `WouldBlock` were in this list, so the one retry spent a *second* full budget on a worker that
/// had already proved it was not answering. [`is_deadline_expired`] is the predicate that answer
/// belongs to, and the two pools ask it first.
pub fn is_transport_error(err: &volucer_fcgi::FcgiError) -> bool {
    match err {
        volucer_fcgi::FcgiError::Io(io) => matches!(
            io.kind(),
            std::io::ErrorKind::BrokenPipe
                | std::io::ErrorKind::ConnectionReset
                | std::io::ErrorKind::ConnectionAborted
                | std::io::ErrorKind::UnexpectedEof
                | std::io::ErrorKind::NotConnected
                | std::io::ErrorKind::TimedOut
                | std::io::ErrorKind::WouldBlock
        ),
        volucer_fcgi::FcgiError::IncompleteResponse { .. } => true,
        _ => false,
    }
}

/// Report a failed exchange the way `request_timeout_ms` would describe it.
///
/// **A deadline is named as a deadline.** `os error 10060` and "a connection attempt failed" are what
/// the OS says when a read times out; the operator configured `request_timeout_ms`, and a message that
/// does not say so sends them looking for a network problem that does not exist. Shared by every
/// backend that has the key, because one sentence with three copies in it is three chances to drift.
pub(crate) fn deadline_or_worker(
    backend: &str,
    timeout: Duration,
    source: volucer_fcgi::FcgiError,
) -> ServerError {
    if is_deadline_expired(&source) {
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

/// The error for an awaited exchange that `tokio::time::timeout` cut off.
///
/// The same sentence [`deadline_or_worker`] produces for a socket-level timeout, because to an
/// operator the two are one fact: the backend did not answer inside `request_timeout_ms`. The source
/// is a `TimedOut` `FcgiError` rather than a `tokio` elapsed error so that every layer above —
/// [`is_deadline_expired`], the status mapping, the log line — treats it as the deadline it is.
pub(crate) fn deadline_expired(backend: &str, timeout: Duration) -> ServerError {
    deadline_or_worker(
        backend,
        timeout,
        volucer_fcgi::FcgiError::Io(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "the exchange did not complete",
        )),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use volucer_core::config::{ExternalConfig, WorkerConfig};

    #[test]
    fn an_external_backend_parses_its_socket() {
        let backend = Backend::from_config(
            "fpm",
            &BackendConfig::External(ExternalConfig {
                socket: "127.0.0.1:9000".to_string(),
                max_connections: None,
                request_timeout_ms: None,
                services: Vec::new(),
            }),
        )
        .unwrap();
        assert_eq!(backend.kind(), "external");
        assert!(backend.resets_per_request());
        assert_eq!(backend.name(), "fpm");
    }

    #[test]
    fn a_bad_socket_is_a_misconfiguration() {
        #[cfg(not(unix))]
        let socket = "unix:/run/php.sock";
        #[cfg(unix)]
        let socket = "unix:";

        let err = Backend::from_config(
            "fpm",
            &BackendConfig::External(ExternalConfig {
                socket: socket.to_string(),
                max_connections: None,
                request_timeout_ms: None,
                services: Vec::new(),
            }),
        )
        .unwrap_err();
        assert!(matches!(err, ServerError::Misconfigured { .. }), "{err}");
    }

    #[test]
    fn a_worker_backend_without_a_real_command_is_reported_at_startup() {
        // The pool boots its workers in `from_config`, so a command that cannot
        // start is a startup error rather than a surprise on the first request.
        let err = Backend::from_config(
            "app",
            &BackendConfig::Worker(WorkerConfig {
                command: "definitely-not-a-php-binary worker.php".to_string(),
                workers: 1,
                memory_limit_mb: None,
                max_requests: None,
                working_dir: None,
                request_timeout_ms: None,
                idle_ping_ms: None,
                env: Default::default(),
                checksum: Default::default(),
                services: Vec::new(),
                grpc_services: Vec::new(),
                grpc_serve_any: false,
                grpc_proto: Vec::new(),
                grpc_proto_services: Vec::new(),
                measure_wait: false,
                transport: Default::default(),
                socket: None,
            }),
        )
        .unwrap_err();
        assert!(matches!(err, ServerError::Misconfigured { .. }), "{err}");
        assert!(err.to_string().contains("does not exist"), "{err}");
    }

    #[test]
    fn spawn_fpm_is_no_longer_a_backend_shape() {
        // It was accepted by the parser and then refused by `Backend::from_config`, so the
        // failure arrived at startup rather than where the file was read. Now that the
        // variant is gone the file cannot be written at all, which is the point of a
        // removed key: it should be noticed by whoever wrote it.
        let text = r#"
[server]
default_backend = "own"
http = "127.0.0.1:8080"
[backends.own]
kind = "spawn_fpm"
binary = "/usr/sbin/php-fpm"
"#;
        assert!(
            volucer_core::config::Config::from_toml(text).is_err(),
            "spawn_fpm must not parse"
        );
    }
}
