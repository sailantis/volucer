//! The state a running volucer serves from, and replacing it without a restart.
//!
//! This crate exists to answer one question in one place: **what does a reload replace, and what
//! can never be replaced?** It is separate from the front-ends because volucer has two of them —
//! an HTTP listener and a FastCGI listener for a deployment behind nginx — and a reload that
//! reached only one of them would be worse than no reload at all. That is not hypothetical: the
//! site table used to be held by each listener separately, so a reloaded site worked over `:80`
//! and silently kept reaching the old backend behind nginx, which reads as *the other front-end
//! being right* rather than as a bug.
//!
//! # Why a swap and not a lock
//!
//! The read is the request path and the write happens once per reload, which is the shape a
//! read-write lock handles badly: `RwLock` still has every reader touch one atomic counter, and a
//! writer cannot make progress until they all leave. That is fine at startup and wasteful per
//! request — and this project's whole design note is that a request's cost is not allowed to
//! depend on a feature the deployment is not using.
//!
//! [`arc_swap::ArcSwap`] is the reverse: readers load a pointer that no writer ever blocks. The
//! cost is measured rather than asserted —
//! `volucer-competition/scripts/reload-cost-ab.js` is the harness, and the result is −0.09%, 95%
//! CI [−1.43%, +1.27%] at n = 20, against an A-vs-A control at +0.34%.
//!
//! # The one pointer
//!
//! One value, not one per field, and that is the correctness rather than the tidiness: three
//! separate swaps are not one snapshot, so a request could take the new site table and the old
//! routing map and resolve a site the map has no entry for. The failure would not look like a
//! race — it would look like a site serving the wrong document root, once, under load.
//!
//! # What is *not* reloadable, and why that is not a limitation to work around
//!
//! Some settings describe a socket that is already bound or a process that is already running:
//! listener addresses, backend commands and worker counts, and the TLS settings that live inside a
//! `rustls::ServerConfig` (`min_version`, `alpn`). Those cannot be replaced in place, and pretending
//! otherwise is how a reload comes to change half of what it was asked to change. The rule this
//! crate follows is that **everything behind a [`Swappable`] is replaceable *whole*, and everything
//! outside one is restart-only** — so the line is drawn by the type rather than by a comment a later
//! change can contradict. The restart-only list is *reported* by every reload and named in
//! `volucer serve`'s own output, rather than left to be discovered from behaviour.
//!
//! Note what is **not** in that list, because an earlier draft of it said otherwise: a site's route
//! table and its static layer are both read per request out of the swapped table
//! (`Reloaded::from_sites` builds them from the site), so both are reloadable. The published list is
//! drawn from the code.
//!
//! # One apply path per process
//!
//! [`Registry`] owns the shared table *and* every front-end's snapshot-holder, so a caller applies a
//! reload through one call and no front-end can be missed. That is what makes a *trigger* writable:
//! `volucer serve`'s `SIGHUP` handler, a test, and a listener's own [`crate::SharedState`] all reach
//! the same `apply`. Before this, only a listener could apply a reload — so the process's own signal
//! handler would have had to either duplicate the rebuild or reach the table through one listener.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use arc_swap::ArcSwap;
use volucer_core::config::{GrpcGrants, StaticConfig};
use volucer_core::sites::{ResolvedSite, Sites};
use volucer_fcgi::cgi::Routing;

/// Which backend may be asked to serve which gRPC service, for one configuration generation.
///
/// Keyed by backend name, and only backends that can serve gRPC appear at all — a `worker` or an
/// `attach`. An absent key is therefore "this backend cannot be a gRPC handler", and a present one
/// with no names is "it can, and was granted nothing"; the second is the default and the safe
/// answer, and the two are deliberately distinguishable because the listener's refusal names which
/// of them applied.
pub type GrpcGrantsMap = BTreeMap<String, GrpcGrants>;

/// A value that can be replaced while readers are using it.
///
/// Always used behind an `Arc`, because a snapshot has to be cloneable into the blocking task
/// that serves the request: [`Swappable::load`] returns an owned `Arc`, so a handler that takes
/// one snapshot keeps the value it started with even if a reload lands mid-request. That is
/// deliberate. A request that resolved its site against one table and then routed against another
/// would be two answers to one question, and the failure would look like a site serving the wrong
/// document root rather than like a race.
///
/// `T: Send + Sync` is what the `Arc` requires for the readers to be on other threads, and it is
/// the whole of the type's constraint: a value that can be shared concurrently can be swapped
/// concurrently.
#[derive(Debug)]
pub struct Swappable<T> {
    value: ArcSwap<T>,
}

impl<T> Swappable<T> {
    /// Wrap a value: the state a listener starts with.
    pub fn new(value: T) -> Self {
        Self {
            value: ArcSwap::from_pointee(value),
        }
    }

    /// A snapshot, for the length of one request or one connection.
    ///
    /// This is the request-path read, so it is deliberately the only way in: there is no `Deref`,
    /// because a `Deref` would let a caller hold a borrow across an `.await` and thereby hold a
    /// snapshot it never chose to hold. Taking an `Arc` is the explicit version of the same thing,
    /// and it cannot outlive the guard by accident.
    pub fn load(&self) -> Arc<T> {
        self.value.load_full()
    }

    /// Replace the value, returning the previous one.
    ///
    /// Returns the old value so a caller can say *what* changed rather than only that a reload
    /// happened — which is the difference between an operator reading "site `api` document_root
    /// changed" and reading "configuration reloaded" and having to go and diff the file themselves.
    ///
    /// A reader that already loaded is unaffected: it keeps the value it took, which is the
    /// property that makes this safe to call while requests are in flight.
    #[must_use]
    pub fn replace(&self, value: T) -> Arc<T> {
        self.value.swap(Arc::new(value))
    }
}

impl<T: Default> Default for Swappable<T> {
    fn default() -> Self {
        Self::new(T::default())
    }
}

/// The process's rate limit, in a form a reload can replace.
///
/// Lives here rather than beside either front-end because **both** consult it and a reload has to be
/// able to replace it: `volucer-http` owns the HTTP listener and `volucer-server` the FastCGI one,
/// and neither depends on the other. This crate is already the home of "what a reload replaces", so
/// the rule travels with the cell rather than being re-derived per front-end.
///
/// A newtype rather than a bare [`Swappable`] because the *replacement rule* is the interesting part
/// and it must not be written twice: a policy that changes only its rate keeps every caller's bucket
/// (see [`volucer_core::RateLimitPolicy::adopt_buckets`]), and a policy that changes its *identity*
/// cannot. Two reload paths that each wrote that match would be two chances to get it wrong, and the
/// failure is silent — an allowance that resets is invisible in a log.
#[derive(Debug)]
pub struct RateLimitCell {
    current: Swappable<Option<Arc<volucer_core::RateLimitPolicy>>>,
}

impl RateLimitCell {
    /// A cell holding a policy, or `None` for a deployment with no `[server.rate_limit]`.
    pub fn new(policy: Option<volucer_core::RateLimitPolicy>) -> Self {
        RateLimitCell {
            current: Swappable::new(policy.map(Arc::new)),
        }
    }

    /// The policy to consult, or `None` to consult nothing.
    ///
    /// One pointer read, which is the cost the plain `Option` field had: a deployment with no limit
    /// pays the same as before, and one with a limit pays an atomic load it already paid for the
    /// site table.
    pub fn load(&self) -> Arc<Option<Arc<volucer_core::RateLimitPolicy>>> {
        self.current.load()
    }

    /// Whether a limit is configured at all, for `serve`'s own report.
    pub fn is_some(&self) -> bool {
        self.current.load().is_some()
    }

    /// Replace the policy, carrying the buckets across when the identity is unchanged.
    ///
    /// Returns `true` when every tracked caller kept its tokens — which is the answer for a change of
    /// `rate`, `burst`, `interval_ms` or `max_entries`, and `false` for a change of `key` or `header`
    /// (there is no honest way to map one caller's bucket onto another's) and for turning the limit
    /// on or off.
    ///
    /// `false` is therefore an ordinary outcome and not a failure. The caller has already built and
    /// validated the policy; a policy that cannot be built never reaches here, which is what keeps
    /// the previous one in force.
    #[must_use]
    pub fn replace(&self, next: Option<volucer_core::RateLimitPolicy>) -> bool {
        let previous = self.current.load();
        match next {
            Some(mut next) => {
                let carried = match previous.as_ref() {
                    Some(previous) => next.adopt_buckets(previous),
                    None => false,
                };
                let _replaced = self.current.replace(Some(Arc::new(next)));
                carried
            }
            None => {
                let _replaced = self.current.replace(None);
                false
            }
        }
    }
}

/// The site table both front-ends serve from, and what a reload replaces.
///
/// Wraps [`Sites`] rather than being an alias for it so that the *shared* thing is named: both
/// listeners are handed a clone of this handle, so replacing the table moves both of them or
/// neither, and there is no way to write the code that moves one.
///
/// The per-front-end state that is *derived* from the table — the HTTP listener's prebuilt
/// routings and static layers — is not here, because it is not shared and rebuilding it for a
/// front-end that has no use for it would be work for nothing. It is rebuilt from the table this
/// handle returns, so it cannot outlive the table it was built from.
#[derive(Clone)]
pub struct SharedState {
    sites: Arc<Swappable<Sites>>,
}

impl SharedState {
    /// A shared site table.
    pub fn new(sites: Sites) -> Self {
        SharedState {
            sites: Arc::new(Swappable::new(sites)),
        }
    }

    /// A snapshot of the site table, taken once per request.
    ///
    /// The single load is the point: everything downstream — site resolution, the routing map,
    /// the static layer — reads from the value this returns, so a request is served under exactly
    /// one configuration even if a reload lands while it is in flight.
    pub fn sites(&self) -> Arc<Sites> {
        self.sites.load()
    }

    /// Replace the site table, returning the one that was serving.
    ///
    /// Every front-end holding a clone of this handle is affected at once, which is the property
    /// the two-listener case needs and the reason this is one handle rather than a table per
    /// listener.
    #[must_use]
    pub fn replace(&self, sites: Sites) -> Arc<Sites> {
        self.sites.replace(sites)
    }
}

impl std::fmt::Debug for SharedState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SharedState")
            .field("sites", &self.sites.load().named().len())
            .finish_non_exhaustive()
    }
}

/// The key the default site's routing is stored under.
///
/// A name that cannot collide with a site's own, because a site name comes from a TOML
/// table key and `<>` is not valid in a bare TOML key.
pub const DEFAULT_SITE_KEY: &str = "<default>";

/// The static layer's configuration and document root.
///
/// A plain data struct rather than the HTTP listener's own type, and that is deliberate: the
/// layer is configured *from* a site, so it has to be built by whatever builds the site table —
/// which must not be the HTTP front-end, because a reload applies while no HTTP listener may
/// exist at all. The HTTP listener adds the behaviour and the response types; this is only what
/// it decides from. See `volucer_http::static_files::StaticFiles`, which is a type alias for it.
#[derive(Debug, Clone)]
pub struct StaticFiles {
    /// The layer's settings, with the site's inheritance already applied.
    pub config: StaticConfig,
    /// The root a request path is resolved against.
    pub document_root: PathBuf,
    /// The listener's own port, for a redirect when no `Host` header arrived.
    ///
    /// Fixed for the life of a listener — the socket is already bound — so a reload cannot
    /// change it, and it is passed in rather than stored with the table.
    ///
    /// **`None` on a unix socket listener**, which has no port. A directory request that arrived
    /// with no `Host` header is then redirected to `//localhost/…` with no port at all, which is
    /// what a client of a socket would follow anyway — inventing `:0` would be a URL that cannot
    /// work and that an operator would read as a bug in the redirect rather than a property of
    /// the transport.
    pub server_port: Option<u16>,
    /// The extensions this site treats as scripts, so a file that is one is never served
    /// from disk.
    ///
    /// The same value the router uses. See `volucer_core::script` for why this is one list
    /// rather than two.
    pub script_extensions: volucer_core::ScriptExtensions,
}

/// Everything a front-end derives from the shared site table, as one replaceable value.
///
/// The three travel together because they are derived from one another: the routing and
/// static maps are built *from* the site table, so a request that took them from different
/// generations would resolve a site the map has no entry for. [`Reloaded::from_sites`] is
/// the only constructor for that reason — the maps cannot be set independently of the table
/// they describe.
///
/// The `Arc`s are for the request path: a handler takes one snapshot and moves it into the
/// blocking dispatch task, so the request keeps the configuration it began under.
///
/// It lives in this crate rather than in a front-end because **both** front-ends need it *and* a
/// reload has to build it. Building it inside the HTTP listener meant a reload could not be
/// applied by anything that did not have that listener in hand, which is what made a single
/// shared site table impossible to reach from the process's own reload path.
pub struct Reloaded {
    /// The site table this was built from, as a snapshot of [`SharedState`].
    ///
    /// Held rather than borrowed so a request is served under one generation even if a reload
    /// lands while it is in flight — the property the in-flight test pins.
    sites: Arc<Sites>,
    /// One prebuilt routing per site, by site name, plus the default under
    /// [`DEFAULT_SITE_KEY`].
    ///
    /// Prebuilt, not derived per request: a `Routing` is a document root and a front
    /// controller, and rebuilding one for every request would put a `PathBuf` clone on
    /// the hot path to answer a question the configuration already answered at startup.
    routings: BTreeMap<String, Routing>,
    /// The static layer per site, for the same reason.
    statics: BTreeMap<String, StaticFiles>,
    /// How responses are compressed per site.
    ///
    /// Carried here rather than read from the site at request time for the same reason as the
    /// other two: a request must be served under one generation, and a reload landing between
    /// "which site" and "how to compress" would apply half of a configuration.
    compression: BTreeMap<String, volucer_core::config::CompressionSetting>,
    /// Which gRPC services each backend may be asked to serve, for this generation.
    ///
    /// **Here rather than on the pool, which is what makes it reloadable.** The grant used to be
    /// captured into a worker pool's spec at startup, so widening it meant restarting the server —
    /// for a list that never reaches a worker and is only ever a membership test on the request
    /// path. Nothing about it is a property of a running process: the server refuses a call it was
    /// not granted, before the body is read and before a worker is reached, which is a decision the
    /// front-end makes alone.
    ///
    /// It travels in `Reloaded` for the reason everything else there does: a request must be
    /// answered by one generation. A reload that changed a route *and* a grant moves both at once,
    /// so a call cannot be routed by the new table and admitted by the old grant.
    grpc_grants: Arc<GrpcGrantsMap>,
}

impl Reloaded {
    /// Build a front-end's maps from a snapshot of the shared site table.
    ///
    /// Takes the `Arc` the shared handle handed out rather than a table, so the value a request
    /// reads and the per-site maps it routes against are guaranteed to be the *same* generation:
    /// there is no constructor that could pair them from two.
    ///
    /// `server_port` is the bound port, which the static layer stamps into generated
    /// redirects — a reload cannot change it, so it is passed in rather than stored. `None` for a
    /// listener with no port, which is what a unix socket is.
    ///
    /// The gRPC grants are a *separate* argument rather than derived from the site table, because
    /// they come from `[backends.*]` and a `Sites` is built from `[server]` and `[sites.*]`. The
    /// caller that builds both has the whole configuration in hand; this pairs them into one
    /// snapshot so a request cannot see one generation's grants and another's routes.
    pub fn from_sites(
        sites: Arc<Sites>,
        server_port: Option<u16>,
        grpc_grants: Arc<GrpcGrantsMap>,
    ) -> Self {
        let mut routings = BTreeMap::new();
        let mut statics = BTreeMap::new();
        let mut compression = BTreeMap::new();
        for site in sites
            .named()
            .values()
            .chain(std::iter::once(sites.default_site()))
        {
            routings.insert(
                site.name.clone(),
                Routing {
                    document_root: site.document_root.clone(),
                    front_controller: site.front_controller.clone(),
                    script_extensions: site.script_extensions.clone(),
                    ..Routing::default()
                },
            );
            statics.insert(
                site.name.clone(),
                StaticFiles {
                    config: site.static_files.clone(),
                    document_root: site.document_root.clone(),
                    server_port,
                    script_extensions: site.script_extensions.clone(),
                },
            );
            compression.insert(site.name.clone(), site.compression.clone());
        }
        Reloaded {
            sites,
            routings,
            statics,
            compression,
            grpc_grants,
        }
    }

    /// The site table this snapshot was built from.
    pub fn sites(&self) -> &Sites {
        &self.sites
    }

    /// Which gRPC services each backend may be asked to serve, for *this* generation.
    ///
    /// The request-path answer, and the only one: asking the dispatcher or the pool would answer
    /// with whatever the process started with, which is the restart-only behaviour this replaced.
    pub fn grpc_grants(&self) -> &GrpcGrantsMap {
        &self.grpc_grants
    }

    /// The gRPC call a `:path` addresses, when this generation grants the backend that service.
    ///
    /// `None` for three different refusals, which the caller reports apart rather than collapsing:
    /// the backend cannot serve gRPC at all (an absent key), the path is not `/pkg.Service/Method`,
    /// and the service is not in that backend's grant. All three become `UNIMPLEMENTED` — what gRPC
    /// means by "this server does not implement that method" — but only the last one is something an
    /// operator fixes by editing `grpc_services`.
    ///
    /// Returning the parsed call rather than just the verdict is deliberate: it is what a log line
    /// and a test need, and re-parsing the path at either of those places would be a second
    /// implementation of the rule.
    pub fn granted_grpc_call<'a>(
        &self,
        backend: &str,
        path: &'a str,
    ) -> Option<volucer_core::config::GrpcPath<'a>> {
        self.grpc_grants.get(backend)?.granted(path)
    }

    /// Resolve a host against *this* generation and return everything the request needs from it.
    ///
    /// One call rather than a resolve followed by a lookup, because the two must agree: see
    /// [`Resolved`].
    ///
    /// **The host-only form**, for a caller with no connection to describe. The request path uses
    /// [`Reloaded::resolve_request`], which also consults the site's address binding and protocol
    /// restriction.
    pub fn resolve(&self, host: Option<&str>) -> Resolved<'_> {
        let site = self.sites.resolve(host);
        let (routing, statics, compression) = self.for_site(site);
        Resolved {
            site,
            routing,
            statics,
            compression,
        }
    }

    /// [`Reloaded::resolve`] for a real connection: the name, the local address, the protocol.
    ///
    /// The request-path entry point. Everything it returns comes from this one snapshot, so a
    /// request is served under one generation even if a reload lands while it is in flight — the
    /// property the in-flight test pins.
    pub fn resolve_request(
        &self,
        host: Option<&str>,
        local: Option<std::net::IpAddr>,
        protocol: volucer_core::config::SiteProtocol,
    ) -> Resolved<'_> {
        let site = self.sites.resolve_request(host, local, protocol);
        let (routing, statics, compression) = self.for_site(site);
        Resolved {
            site,
            routing,
            statics,
            compression,
        }
    }

    /// The routing and static layer for one site.
    ///
    /// A site resolved by [`Sites`] is always in both maps — they are built from the
    /// same table in the same pass — so a missing entry would mean the two had
    /// diverged, and falling back to the default is the safe direction: it serves the
    /// deployment's own root rather than an empty path.
    fn for_site(
        &self,
        site: &ResolvedSite,
    ) -> (
        &Routing,
        &StaticFiles,
        &volucer_core::config::CompressionSetting,
    ) {
        let routing = self
            .routings
            .get(&site.name)
            .or_else(|| self.routings.get(DEFAULT_SITE_KEY))
            .expect("the default site is always built");
        let statics = self
            .statics
            .get(&site.name)
            .or_else(|| self.statics.get(DEFAULT_SITE_KEY))
            .expect("the default site is always built");
        let compression = self
            .compression
            .get(&site.name)
            .or_else(|| self.compression.get(DEFAULT_SITE_KEY))
            .expect("the default site is always built");
        (routing, statics, compression)
    }
}

impl std::fmt::Debug for Reloaded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Reloaded")
            .field("sites", &self.sites.named().len())
            .finish_non_exhaustive()
    }
}

/// One request's configuration: a resolved site and the maps that go with it, from one generation.
///
/// Returned as a single value rather than as a pair of calls because the pairing *is* the
/// correctness. The site has to come from the `Reloaded` a request took, not from the shared table
/// live at the moment of resolution: a reload landing between the two would otherwise resolve a
/// site against one generation and route it against another, which is the failure this whole
/// mechanism exists to prevent — and it would look like a site serving the wrong document root,
/// once, under load.
///
/// The two borrows are tied to the `Arc<Reloaded>` the caller holds, so a request cannot keep them
/// past its own snapshot.
pub struct Resolved<'a> {
    /// The site this request is served by.
    pub site: &'a ResolvedSite,
    /// Its routing.
    pub routing: &'a Routing,
    /// Its static layer.
    pub statics: &'a StaticFiles,
    /// How its responses are compressed.
    pub compression: &'a volucer_core::config::CompressionSetting,
}

/// The process's one reloadable state, and the handle a trigger applies a new configuration
/// through.
///
/// One handle rather than one per front-end, because the failure this prevents is a reload that
/// reaches *some* of a running server: an HTTP listener and a FastCGI listener can both be
/// serving, and a reload that moved one and not the other takes effect on one protocol and reads
/// as the other front-end being right. Holding the shared site table *and* each listener's
/// derived maps here means "apply this configuration" is one call with no way to half-do it.
///
/// The two parts are not the same kind of thing, and that is why both are here:
///
/// - [`SharedState`] is the **live** table every front-end writes through, so replacing it moves
///   all of them.
/// - Each `Swappable<Reloaded>` is one front-end's **snapshot-holder**, rebuilt from the table
///   that is now live, so a request in flight keeps the generation it started under.
///
/// Folding them together would mean a reload had to reach the table *through* a listener, which is
/// precisely how the FastCGI front-end came to keep serving the old one.
pub struct Registry {
    shared: SharedState,
    /// One entry per registered front-end.
    ///
    /// Behind a mutex rather than an `ArcSwap` because registration is not the request path: it
    /// runs once per listener at startup, so a serialised write costs nothing that matters.
    front_ends: std::sync::Mutex<Vec<RegisteredFrontEnd>>,
    /// How many reloads have been applied. Reported, and what makes a test able to wait for the
    /// effect of a signal rather than sleep and hope.
    generations: Arc<AtomicU64>,
}

/// One front-end, as the registry needs to reach it.
pub struct RegisteredFrontEnd {
    /// The front-end's own snapshot-holder.
    state: Arc<Swappable<Reloaded>>,
    /// The port that front-end is bound to.
    ///
    /// Per front-end rather than one per registry, because two listeners on two ports share one
    /// site table and a generated redirect has to name the port the client actually reached — so
    /// one number for the process would be wrong for whichever listener it was not.
    ///
    /// `None` for a listener with no port, which is a unix socket: a reload rebuilds this front-end's
    /// state from the same table, and the port it stamps into a generated redirect has to stay the
    /// answer the socket gives, not a number invented here.
    port: Option<u16>,
}

impl RegisteredFrontEnd {
    /// A front-end, for the registry to rebuild on a reload.
    pub fn new(state: Arc<Swappable<Reloaded>>, port: Option<u16>) -> Self {
        RegisteredFrontEnd { state, port }
    }
}

impl Registry {
    /// An empty registry: no front-end has registered yet.
    pub fn new(shared: SharedState) -> Self {
        Registry {
            shared,
            front_ends: std::sync::Mutex::new(Vec::new()),
            generations: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Register a front-end's snapshot-holder, so a reload reaches it.
    ///
    /// Returns the handle the front-end stores in its own request path. The caller builds it with
    /// [`Reloaded::from_sites`] for its bound port; registering it is what makes a later
    /// [`Registry::apply`] touch it.
    ///
    /// Takes `&self` and synchronises internally, because registration happens while the registry
    /// is already shared: `serve` hands the same registry to a listener that binds before the next
    /// one does. Behind a mutex rather than an `RwLock` because this is not the request path — it
    /// runs once per listener at startup — so a cheaper write path is worth more here than a
    /// cheaper read, which does not exist.
    pub fn register(&self, front_end: RegisteredFrontEnd) {
        self.front_ends
            .lock()
            .expect("the registry mutex is not poisoned by anything but a panic in register")
            .push(front_end);
    }

    /// The site table every front-end serves from.
    pub fn shared(&self) -> &SharedState {
        &self.shared
    }

    /// How many reloads have been applied, for diagnostics and for a test that must not sleep.
    pub fn generation(&self) -> u64 {
        self.generations.load(Ordering::Acquire)
    }

    /// Apply a new configuration's site table and gRPC grants, whole, to every registered front-end.
    ///
    /// Whole-or-nothing is the only safe direction and it is a property of this call rather than a
    /// convention: both values are built by the caller *before* this is reached, so a configuration
    /// that failed validation never gets here at all, and there is no partial path once it does.
    ///
    /// The two travel together on purpose. A grant is checked against the site's route selection —
    /// a route names a backend, and the grant says whether that backend may serve the service — so a
    /// reload that installed one without the other would admit a call through a route that no longer
    /// selects it, or refuse one the new table just pointed at.
    ///
    /// This is the *applying* half; the trigger is `volucer serve`'s `signals` module. One apply path
    /// for the process is the point — a test and the process's own `SIGHUP` handler come through the
    /// same call, so there is no way to reload one front-end and not another.
    ///
    /// Returns the state the **first** front-end replaced, or `None` when nothing is registered.
    /// Reported rather than returned as a difference, because "what changed" is a comparison
    /// between two configurations, which is a question for the caller that has both of them — this
    /// only guarantees both are installed.
    #[must_use]
    pub fn apply(&self, sites: Sites, grpc_grants: Arc<GrpcGrantsMap>) -> Option<Arc<Reloaded>> {
        let _replaced = self.shared.replace(sites);
        // Read back through the handle rather than from the argument: `SharedState::replace`
        // returns the table it *replaced*, so rebuilding from its return value would build the
        // next generation's maps out of the previous generation's table. That bug was written and
        // caught once already; reading back is what makes it unwritable.
        let live = self.shared.sites();

        let front_ends = self
            .front_ends
            .lock()
            .expect("the registry mutex is not poisoned by anything but a panic in register");

        let mut previous = None;
        for front_end in front_ends.iter() {
            // The *front-end's own* port, not the one in the shared table: a redirect has to name
            // the port the client actually reached, and two listeners on two ports share one table.
            let next =
                Reloaded::from_sites(Arc::clone(&live), front_end.port, Arc::clone(&grpc_grants));
            let replaced = front_end.state.replace(next);
            previous = previous.or(Some(replaced));
        }
        self.generations.fetch_add(1, Ordering::Release);
        previous
    }
}

impl std::fmt::Debug for Registry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let front_ends = self.front_ends.lock().map(|list| list.len()).unwrap_or(0);
        f.debug_struct("Registry")
            .field("front_ends", &front_ends)
            .field("generation", &self.generation())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::sync::atomic::{AtomicBool, Ordering};
    use volucer_core::config::Config;

    fn sites_for(root: &str) -> Sites {
        let text = format!(
            "[server]\nhttp = \"127.0.0.1:0\"\ndocument_root = {root:?}\n",
            root = root
        );
        let config = Config::from_toml(&text).expect("config parses");
        Sites::from_config(&config, Path::new(root))
    }

    #[test]
    fn a_snapshot_taken_before_a_replace_still_reads_the_old_value() {
        // The property the request path depends on: a request that has its site table keeps it,
        // even if a reload lands before the response is written. If this ever failed, a request
        // could change document root halfway through and the symptom would be a served file from
        // the wrong site.
        let state = Swappable::new(1u32);
        let held = state.load();
        let previous = state.replace(2);
        assert_eq!(*held, 1, "a held snapshot must not change under it");
        assert_eq!(*previous, 1, "replace reports what it replaced");
        assert_eq!(*state.load(), 2);
    }

    #[test]
    fn a_reader_that_loads_while_a_replace_is_racing_sees_one_value_or_the_other() {
        // Not a proof of atomicity — that is `ArcSwap`'s and its own tests' business. What this
        // checks is the property the *callers* rely on: no reader observes a value that was never
        // installed. A torn read would show up here as a value that is neither.
        let state = Swappable::new(0u32);
        let stop = AtomicBool::new(false);
        let seen: Vec<u32> = std::thread::scope(|scope| {
            let reader = scope.spawn(|| {
                let mut seen = Vec::new();
                while !stop.load(Ordering::Relaxed) {
                    seen.push(*state.load());
                }
                seen
            });
            for value in 1..=500u32 {
                let _ = state.replace(value);
            }
            stop.store(true, Ordering::Relaxed);
            reader.join().expect("the reader thread panicked")
        });
        assert!(
            seen.iter().all(|value| *value <= 500),
            "a reader saw a value that was never installed: {seen:?}"
        );
        assert_eq!(
            *state.load(),
            500,
            "the last write must be the value visible now"
        );
    }

    #[test]
    fn replacing_the_shared_table_moves_every_holder_at_once() {
        // The two-listener case, and the reason this type exists: a clone handed to the FastCGI
        // front-end sees the replacement the HTTP front-end made, so a reloaded site cannot take
        // effect on one protocol only.
        let http = SharedState::new(sites_for("/srv/first"));
        let fastcgi = http.clone();

        assert_eq!(
            fastcgi.sites().default_site().document_root,
            Path::new("/srv/first")
        );
        let replaced = http.replace(sites_for("/srv/second"));

        assert_eq!(
            replaced.default_site().document_root,
            Path::new("/srv/first"),
            "replace must report the table it replaced, not the one it installed"
        );
        assert_eq!(
            fastcgi.sites().default_site().document_root,
            Path::new("/srv/second"),
            "a clone taken before the replacement must see the new table"
        );
        assert_eq!(
            http.sites().default_site().document_root,
            Path::new("/srv/second")
        );
    }

    /// A site table whose site and root are both named, so a sample can tell them apart.
    fn named_sites(root: &str) -> Sites {
        let text = format!(
            "[server]\nhttp = \"127.0.0.1:0\"\ndocument_root = {root:?}\n\n\
             [sites.app]\nhosts = [\"app.test\"]\ndocument_root = {root:?}\n",
            root = root
        );
        let config = Config::from_toml(&text).expect("config parses");
        Sites::from_config(&config, Path::new(root))
    }

    /// The sampling technique used by the concurrency test **would** catch a split generation.
    ///
    /// This is the control for `a_reload_under_concurrent_requests_is_one_configuration_per_request`
    /// in `volucer-http`, and it exists because a test whose claim is "no sample ever disagreed"
    /// passes just as well against a sampler that cannot disagree. Hazard 57's shape exactly: the
    /// assertion has to be able to fail.
    ///
    /// So the three parts are split *deliberately* — three `Swappable`s where the real type has
    /// one — and the same loop is run. If a disagreement is not observed here, the real test is
    /// not observing anything either.
    ///
    /// `Reloaded` is the type that makes this unwritable for callers, so the split is modelled
    /// with what a caller *could* have written: separate values for the table, the routing and the
    /// static layer, moved one after another.
    #[test]
    fn three_separate_swaps_be_observed_mid_generation() {
        let table = Swappable::new(named_sites("/srv/first"));
        let routings = Swappable::new(Path::new("/srv/first").to_path_buf());
        let statics = Swappable::new(Path::new("/srv/first").to_path_buf());

        let observed_a_mixture = std::sync::atomic::AtomicBool::new(false);
        let stop = AtomicBool::new(false);
        std::thread::scope(|scope| {
            let reader = scope.spawn(|| {
                while !stop.load(Ordering::Relaxed) {
                    // A request's three reads, in the order it would make them. Between any two
                    // of these a writer can land, which is the whole defect.
                    let sites = table.load();
                    let root = sites.default_site().document_root.clone();
                    let routing = routings.load();
                    let statics = statics.load();
                    if root != *routing || root != *statics {
                        observed_a_mixture.store(true, Ordering::Relaxed);
                    }
                }
            });

            for round in 0..20_000 {
                // Alternating, so a reader that only ever samples one direction cannot look clean
                // by accident.
                let to = if round % 2 == 0 {
                    "/srv/second"
                } else {
                    "/srv/first"
                };
                // A yield between the three writes, which is what makes a mixture observable
                // rather than a matter of luck: the reader's loop is pure atomic loads, so it gets
                // in between them every time. Without the yields this test is a race that would
                // only *sometimes* see the split, and a control that sometimes cannot fail is not
                // a control.
                let _ = table.replace(named_sites(to));
                std::thread::yield_now();
                let _ = routings.replace(Path::new(to).to_path_buf());
                std::thread::yield_now();
                let _ = statics.replace(Path::new(to).to_path_buf());
            }
            stop.store(true, Ordering::Relaxed);
            reader.join().expect("the reader thread panicked");
        });

        assert!(
            observed_a_mixture.load(Ordering::Relaxed),
            "the sampler never saw a split generation even with the parts deliberately separated, \
             so the concurrency test's clean run proves nothing"
        );
    }

    /// Every part of one generation is installed by **one** swap, which is what makes the
    /// concurrency test's clean run meaningful rather than lucky.
    ///
    /// The counterpart to the test above: the same racing reader, against the real type. The
    /// reader can only ever see two complete generations, and the assertion is that the sampling
    /// loop *ran* — a sampler that never sampled would "observe no mixture" too.
    #[test]
    fn one_swap_never_yields_a_partial_generation() {
        let state = Swappable::new(Reloaded::from_sites(Arc::new(named_sites("/srv/first")), Some(8080),
            Arc::new(GrpcGrantsMap::new()),
        ));

        let stop = AtomicBool::new(false);
        let mut seen: Vec<PathBuf> = Vec::new();
        std::thread::scope(|scope| {
            let reader = scope.spawn(|| {
                let mut seen = Vec::new();
                while !stop.load(Ordering::Relaxed) {
                    let snapshot = state.load();
                    let resolved = snapshot.resolve(Some("app.test"));
                    assert_eq!(
                        resolved.routing.document_root, resolved.site.document_root,
                        "one swap produced a routing from another generation"
                    );
                    assert_eq!(
                        resolved.statics.document_root, resolved.site.document_root,
                        "one swap produced a static layer from another generation"
                    );
                    seen.push(resolved.site.document_root.clone());
                }
                seen
            });

            for round in 0..5_000 {
                let root = if round % 2 == 0 {
                    "/srv/first"
                } else {
                    "/srv/second"
                };
                let _ = state.replace(Reloaded::from_sites(
                    Arc::new(named_sites(root)),
                    Some(8080),
                    Arc::new(GrpcGrantsMap::new()),
                ));
            }
            stop.store(true, Ordering::Relaxed);
            seen = reader.join().expect("the reader thread panicked");
        });

        assert!(
            !seen.is_empty(),
            "the sampler never ran, so it observed nothing"
        );
        assert!(
            seen.iter()
                .all(|root| root == Path::new("/srv/first") || root == Path::new("/srv/second")),
            "a sample resolved to a root no generation named"
        );
    }

    /// A gRPC grant is per **generation**, so a reload can widen it without a restart.
    ///
    /// The property the whole change exists for. It is asserted on `Reloaded` rather than on a pool
    /// because that is where the answer now lives: a grant that had stayed on the pool would be
    /// pinned to the process, and this test would be unable to move it.
    #[test]
    fn a_generation_carries_its_own_grpc_grants() {
        let mut before = GrpcGrantsMap::new();
        before.insert(
            "app".to_string(),
            GrpcGrants::from_config(&["greeter.Greeter".to_string()], false),
        );
        let mut after = GrpcGrantsMap::new();
        after.insert(
            "app".to_string(),
            GrpcGrants::from_config(
                &["greeter.Greeter".to_string(), "other.Service".to_string()],
                false,
            ),
        );

        let first =
            Reloaded::from_sites(Arc::new(named_sites("/srv/first")), Some(8080), Arc::new(before));
        let second =
            Reloaded::from_sites(Arc::new(named_sites("/srv/first")), Some(8080), Arc::new(after));

        assert!(first
            .granted_grpc_call("app", "/greeter.Greeter/SayHello")
            .is_some());
        assert!(
            first
                .granted_grpc_call("app", "/other.Service/SayHello")
                .is_none(),
            "the first generation must not admit a service it never listed"
        );
        assert!(
            second
                .granted_grpc_call("app", "/other.Service/SayHello")
                .is_some(),
            "the second generation must admit what it added, with no restart"
        );

        // A backend with no entry at all is a kind that cannot serve gRPC, and is refused no matter
        // what the path names.
        assert!(second
            .granted_grpc_call("nope", "/greeter.Greeter/SayHello")
            .is_none());
        // The shape rule is unchanged: a URL is not a method name.
        assert!(second
            .granted_grpc_call("app", "/greeter.Greeter")
            .is_none());
    }

    /// `grpc_serve_any` grants every well-formed service, still refusing every other path shape.
    #[test]
    fn a_generation_may_grant_every_service_but_not_every_path() {
        let mut grants = GrpcGrantsMap::new();
        grants.insert("app".to_string(), GrpcGrants::any());
        let state =
            Reloaded::from_sites(Arc::new(named_sites("/srv/first")), Some(8080), Arc::new(grants));

        assert!(state
            .granted_grpc_call("app", "/never.Listed/Method")
            .is_some());
        for refused in ["/a.B/M/extra", "/a.B", "/", "/a.B/M?x=1"] {
            assert!(
                state.granted_grpc_call("app", refused).is_none(),
                "{refused:?} is not a method path and must be refused even under grpc_serve_any"
            );
        }
    }

    /// A rate-limit replacement keeps the buckets when the identity is unchanged.
    ///
    /// The property that makes a reloadable limit a limit rather than a reset button. Asserted from
    /// the caller's side: a depleted caller must still be depleted after the swap.
    #[test]
    fn replacing_a_rate_limit_keeps_buckets_when_only_the_rate_changed() {
        fn policy(interval_secs: u64) -> volucer_core::RateLimitPolicy {
            volucer_core::RateLimitPolicy::new(
                volucer_core::KeyMode::Ip,
                None,
                1,
                std::time::Duration::from_secs(interval_secs),
                Some(1),
                None,
            )
            .expect("valid")
        }

        let cell = RateLimitCell::new(Some(policy(3600)));
        let client = volucer_core::Identity::Address("203.0.113.7".parse().expect("an address"));
        let now = std::time::Instant::now();

        {
            let held = cell.load();
            let limit = held.as_ref().as_ref().expect("a policy is configured");
            assert_eq!(limit.check(client, now), volucer_core::Verdict::Allowed);
            assert!(matches!(
                limit.check(client, now),
                volucer_core::Verdict::Limited { .. }
            ));
        }

        assert!(
            cell.replace(Some(policy(7200))),
            "a change of interval alone must carry the buckets"
        );
        {
            let held = cell.load();
            let limit = held.as_ref().as_ref().expect("a policy is configured");
            assert!(
                matches!(
                    limit.check(client, now),
                    volucer_core::Verdict::Limited { .. }
                ),
                "a reload must not hand a depleted caller a fresh allowance"
            );
        }

        // Turning the limit off is not a carry: there is no bucket to keep.
        assert!(!cell.replace(None));
        assert!(cell.load().is_none());
    }
}
