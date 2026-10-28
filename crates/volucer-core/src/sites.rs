//! Choosing a site from a `Host` header.
//!
//! This is the piece that makes vhosts work. Everything else about a request is
//! already decided per-process: `Routing` holds *one* document root, and the
//! router resolves `SCRIPT_FILENAME` from it before any backend is chosen. A
//! `host` matcher in the route table is therefore too late to change the root —
//! it can only pick a backend.
//!
//! So the host has to be resolved *before* the routing, and that is what this
//! module does. It returns configuration rather than a `Routing` on purpose:
//! `Routing` lives in `volucer-fcgi`, which depends on this crate, so naming it
//! here would be a cycle. The front-end turns the answer into a `Routing` once, at
//! startup, and holds one per site.
//!
//! Free of the filesystem almost everywhere on purpose: the decisions here are about
//! strings the configuration contains, which is what makes them testable without a
//! fixture on disk. The exception is one `read_dir` of a document root's top level, in
//! [`suppress_unused_static_layer`], because "is there anything here to serve" is not a
//! question the file can answer and a `stat` per request is too late to ask it.

use std::collections::BTreeMap;
use std::net::IpAddr;
use std::path::{Path, PathBuf};

use crate::clientip::Cidr;
use crate::config::{
    Config, RouteConfig, ServeOnly, SiteConfig, SiteProtocol, StaticConfig, StaticsSetting,
};
use crate::hostmap::HostMap;

/// Which addresses a site answers on, resolved once at startup.
///
/// `Any` rather than an empty list for the unbound case, because the request path must not have to
/// ask "is this empty, and what does empty mean": a deployment that named no address serves every
/// address, and that is a different statement from a site bound to nothing.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum SiteBinding {
    /// No `addresses` key: this site answers on whatever the listener reached.
    #[default]
    Any,
    /// One or more ranges, parsed at startup so the request path is a comparison and nothing else.
    Ranges(Vec<Cidr>),
}

impl SiteBinding {
    /// Whether a connection that arrived on `local` may select this site.
    ///
    /// The comparison is against the **local** address — what `getsockname` reports for the accepted
    /// socket — and not the peer. That is the whole point of the key: a client chooses the `Host` it
    /// sends, and a client chooses the address it connects to only insofar as DNS and routing let
    /// it, so the local address is the one thing about a request an attacker cannot rewrite.
    pub fn admits(&self, local: IpAddr) -> bool {
        match self {
            SiteBinding::Any => true,
            SiteBinding::Ranges(ranges) => ranges.iter().any(|range| range.contains(local)),
        }
    }

    /// Whether this site is bound to specific addresses at all.
    pub fn is_bound(&self) -> bool {
        matches!(self, SiteBinding::Ranges(_))
    }

    /// The narrowest range that admits `local`, for choosing between two bound sites.
    ///
    /// `None` when nothing admits the address, which is the caller's signal to fall back to the
    /// default site. The narrowest wins because a site bound to `192.168.9.0/24` and one bound to
    /// `192.168.9.123` both admit the same connection, and the more specific one is the one that
    /// says what the operator meant — the same "most specific wins" rule the host table follows
    /// for wildcards, so the two halves of site selection read the same way.
    pub fn specificity(&self, local: IpAddr) -> Option<u8> {
        match self {
            // Any is the least specific possible answer: it matches everything, so it must lose to
            // any explicit range. `0` rather than `None`, because it *does* admit the address.
            SiteBinding::Any => Some(0),
            SiteBinding::Ranges(ranges) => ranges
                .iter()
                .filter(|range| range.contains(local))
                .map(Cidr::bits)
                .max(),
        }
    }
}

/// The protocols a site answers on, resolved once at startup.
///
/// Bit flags rather than a `Vec`, because the request path asks one question per request — "is my
/// protocol in this set" — and a vector would be a linear scan for an answer that cannot change. The
/// unset case is every bit, so an unconfigured site keeps serving whatever reaches it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SiteProtocols(u8);

impl Default for SiteProtocols {
    /// Absent means every protocol, which is the behaviour that existed before the key.
    fn default() -> Self {
        SiteProtocols(
            SiteProtocol::Http as u8 | SiteProtocol::Https as u8 | SiteProtocol::Fastcgi as u8,
        )
    }
}

impl SiteProtocols {
    /// The set a configured list names, with duplicates and unknown entries already refused by
    /// validation.
    pub fn from_config(protocols: &[SiteProtocol]) -> Self {
        let mut bits = 0;
        for protocol in protocols {
            bits |= *protocol as u8;
        }
        SiteProtocols(bits)
    }

    /// Whether this site answers on `protocol`.
    pub fn admits(self, protocol: SiteProtocol) -> bool {
        self.0 & (protocol as u8) != 0
    }
}

/// The settings a request is served with, after a host has been resolved.
///
/// Borrowed from the configuration where possible, so building one per request
/// would be cheap — but the front-end builds them once at startup and keeps them,
/// because the `Routing` it derives from this must be prebuilt rather than
/// reconstructed on the hot path.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedSite {
    /// The name this site is written under, or `"<default>"` for `[server]`.
    pub name: String,
    /// The directory this site's scripts and files resolve against.
    pub document_root: PathBuf,
    /// The front controller, or `None` for "serve only real files".
    pub front_controller: Option<String>,
    /// The extensions this site treats as scripts.
    ///
    /// Carried on the site rather than read from the configuration at the call site, because the
    /// three decisions that depend on it — routing, the static layer's refusal, and `skip_stat` —
    /// all happen per request against *this* site. A list read from `[server]` at those points
    /// would be a second answer for a site that widened it.
    pub script_extensions: crate::script::ScriptExtensions,
    /// What this site serves.
    pub serve_only: ServeOnly,
    /// The static layer, with inheritance already applied.
    pub static_files: StaticConfig,
    /// Whether [`ResolvedSite::static_files`] was turned off because there is nothing to serve.
    ///
    /// The alternative — warning about it and leaving the layer on — was measured at 18–26% of
    /// throughput on `/` for a front-controller application, for a `stat` that can only ever
    /// answer "no file here". So the resolved table states the decision the listener should make,
    /// and this flag is what lets `check` and `serve` say *why* the layer they configured is not
    /// running. It is `false` when the layer is on, and also when it was off to begin with.
    pub static_files_suppressed: bool,
    /// Compression settings, with inheritance already applied.
    ///
    /// Resolved here for the same reason as the static layer: a site's override must be applied
    /// once, against the same snapshot the rest of the request reads.
    pub compression: crate::config::CompressionSetting,
    /// The route table in effect for this site.
    pub routes: Vec<RouteConfig>,
    /// The backend a request matching no route goes to.
    pub default_backend: Option<String>,
    /// The local addresses this site answers on; see [`SiteBinding`].
    ///
    /// Carried on the site rather than asked of the configuration per request for the reason every
    /// other resolved field is: the site table is built once, and the request path must not parse
    /// a range or reach back into a `SiteConfig` that a reload may have replaced.
    pub binding: SiteBinding,
    /// The protocols this site answers on; see [`SiteProtocols`].
    pub protocols: SiteProtocols,
    /// Whether this site answers its own cleartext requests with a `301` to `https://`.
    ///
    /// Resolved here rather than read from the configuration per request for the same reason
    /// everything else on this type is: the answer is a site's, and the request path must not
    /// reach back into a `SiteConfig` that a reload may have replaced. `[server]` is what a site
    /// inherits when it states nothing, so the default site carries `server.redirect_to_https`.
    pub redirect_to_https: bool,
    /// The `Strict-Transport-Security` policy for this site, with inheritance applied.
    ///
    /// `None` is "send no header" — either nobody asked for one, or this site turned off a policy
    /// it would otherwise inherit. Sent only over TLS; see [`HstsPolicy`].
    pub hsts: Option<crate::config::HstsPolicy>,
}

/// The site table, with the `[server]` values it falls back to.
///
/// Built once from a validated [`Config`]. When `config.sites` is empty this
/// still produces exactly one site — the default one — so callers have a single
/// shape to handle and the single-site deployment is not a special case in the
/// request path.
#[derive(Debug, Clone)]
pub struct Sites {
    /// The default site, from `[server]`.
    default: ResolvedSite,
    /// Every configured site by name, so `volucer check` can list them.
    named: BTreeMap<String, ResolvedSite>,
    /// Which site claims a host. The rule — exact, then longest wildcard — lives in
    /// [`HostMap`] so the certificate table and the route table share it rather than each
    /// growing a copy that could drift.
    hosts: HostMap<String>,
    /// Sites that name addresses and no hosts, narrowest range first.
    ///
    /// A site with no `hosts` is a *default for the addresses it names*: it is chosen when the
    /// connection arrived on one of them and no name claimed it. Kept as a list, pre-sorted, rather
    /// than folded into [`Sites::hosts`] — a host map is keyed by a name this site does not have,
    /// and inventing one (`*`) would claim every name rather than the addresses it was written for.
    ///
    /// Sorted by descending range width so the first match is the most specific, which is the same
    /// rule the host table applies to wildcards, so both halves of selection read the same way. The
    /// sort is done once here rather than per request, for the same reason the routings are
    /// prebuilt: it is a property of the configuration.
    address_only: Vec<(String, SiteBinding)>,
}

impl Sites {
    /// Resolve the site table, applying inheritance from `[server]`.
    ///
    /// `document_root` is the root the command line resolved, since the CLI
    /// absolutises it and a site must not be resolved against a relative path. A
    /// site always states its own root, so this is only the default site's.
    pub fn from_config(config: &Config, document_root: &Path) -> Self {
        Self::from_config_with(config, document_root, None)
    }

    /// [`Sites::from_config`], with the front controller the command line resolved.
    ///
    /// The flag describes the *default* site, so it is applied to that one and to sites
    /// that inherit: a site stating its own controller keeps it, which is what "a site
    /// states its own" means.
    pub fn from_config_with(
        config: &Config,
        document_root: &Path,
        front_controller_override: Option<&str>,
    ) -> Self {
        // Through `resolve_front_controller`, so the default (`index.php`) is applied
        // here rather than left to the CLI. Otherwise a site that omits the key would
        // get no controller while the single-site path got `index.php` — the same
        // deployment behaving two ways depending on which front-end read it.
        let default_front = config
            .server
            .resolve_front_controller(front_controller_override);
        let default_extensions = config.server.script_extensions();
        let default_statics = config.server.statics();
        let default_compression = config.server.compression();
        // Both halves of "this layer could be shadowing a backend": an HTTP listener the layer
        // belongs to, and a backend for the requests it declines. With either missing the layer
        // is either unreachable or the whole application, and taking it away would be wrong.
        let can_shadow_a_backend = config.server.http.is_some() && !config.backends.is_empty();

        let mut default = ResolvedSite {
            name: "<default>".to_string(),
            document_root: document_root.to_path_buf(),
            front_controller: default_front.clone(),
            script_extensions: default_extensions.clone(),
            serve_only: config.server.serve_only,
            static_files: default_statics.clone(),
            static_files_suppressed: false,
            compression: default_compression.clone(),
            routes: config.server.routes.clone(),
            default_backend: config
                .implied_backend(config.server.default_backend.as_deref())
                .map(str::to_string),
            // `[server]` is the fallback site and cannot be bound: it is what answers a connection
            // no site claims, so a binding here would be a way to answer nothing at all. A
            // deployment that wants the default site restricted writes a site for it and leaves
            // `[server]` as the refusal.
            binding: SiteBinding::Any,
            protocols: SiteProtocols::default(),
            // `[server]` is the fallback site, and it *is* the deployment's own settings, so these
            // two are the server's rather than inherited from anywhere.
            redirect_to_https: config.server.redirect_to_https,
            hsts: config
                .server
                .hsts
                .as_ref()
                .and_then(|hsts| hsts.resolved(None)),
        };
        suppress_unused_static_layer(
            &mut default,
            can_shadow_a_backend,
            config.server.static_files.asks_for_statics(),
        );

        let mut named = BTreeMap::new();
        let mut hosts: HostMap<String> = HostMap::new();
        let mut address_only: Vec<(String, SiteBinding)> = Vec::new();

        for (name, site) in &config.sites {
            let mut resolved = resolve_site(
                name,
                site,
                default_front.as_deref(),
                &default_extensions,
                config.server.serve_only,
                &default_statics,
                &default_compression,
                &config.server.routes,
                config.implied_backend(config.server.default_backend.as_deref()),
                config.server.redirect_to_https,
                default.hsts.as_ref(),
            );
            suppress_unused_static_layer(
                &mut resolved,
                can_shadow_a_backend,
                site_asks_for_statics(site, &config.server.static_files),
            );

            for host in &site.hosts {
                // `HostMap::insert` does the trimming, lowercasing and `*.` split; doing it
                // here as well would be a second normalisation that could drift from the one
                // the lookup applies to the header.
                hosts.insert(host, name.clone());
            }

            // An address-only site is not in the host map at all, so it is recorded separately.
            // Its binding is already resolved, so this is a move rather than a second parse.
            if site.hosts.is_empty() && resolved.binding.is_bound() {
                address_only.push((name.clone(), resolved.binding.clone()));
            }

            named.insert(name.clone(), resolved);
        }

        // Narrowest last, so a linear scan takes the most specific first. See the field's doc: the
        // order is the rule, and computing it here keeps it out of the request path.
        address_only.sort_by_key(|(_, binding)| {
            std::cmp::Reverse(match binding {
                SiteBinding::Ranges(ranges) => ranges.iter().map(Cidr::bits).max().unwrap_or(0),
                SiteBinding::Any => 0,
            })
        });

        Sites {
            default,
            named,
            hosts,
            address_only,
        }
    }

    /// The default site: what `[server]` describes.
    pub fn default_site(&self) -> &ResolvedSite {
        &self.default
    }

    /// Every configured site by name, excluding the default.
    pub fn named(&self) -> &BTreeMap<String, ResolvedSite> {
        &self.named
    }

    /// The site that answers a `Host` header.
    ///
    /// The rule is the whole rule, so a reader of the file can predict it: an
    /// exact name wins, then the longest matching `*.` wildcard, then the default
    /// site. A host with a port, and a host in any case, matches the same as one
    /// without — the `Host` header is a client's word and neither a port nor a case
    /// is part of the name a site claims.
    ///
    /// **This is the host-only answer.** It takes no address and no protocol, so a site bound to
    /// addresses is returned here without its binding being consulted — correct for a caller that
    /// has no connection to describe (a diagnostic that asks "which site claims this name"), and
    /// wrong for a request. The request path uses [`Sites::resolve_request`].
    pub fn resolve(&self, host: Option<&str>) -> &ResolvedSite {
        let Some(host) = host else {
            return &self.default;
        };
        self.resolve_str(host)
    }

    /// [`Sites::resolve`], taking the header directly.
    pub fn resolve_str(&self, host: &str) -> &ResolvedSite {
        // The matching rule is `HostMap`'s, so this is only the fallback decision: a host
        // nobody claims is the default site's. A name in the table is a name in `named`,
        // because both were filled from the same map in the same pass.
        self.hosts
            .get(host)
            .and_then(|name| self.named.get(name.as_str()))
            .unwrap_or(&self.default)
    }

    /// The site that serves **this connection**: a name, the address it arrived on, and the
    /// protocol it arrived over.
    ///
    /// This is the request-path answer, and it is the one that makes a bound site worth writing.
    /// The order is the security property — the first two rules are the ones that cannot be
    /// influenced by what a client sends:
    ///
    /// 1. **A site that names protocols must admit `protocol`.** The listener is a fact about the
    ///    connection: a site restricted to `https` does not answer the cleartext half of a
    ///    `plain = true` port, however the request is written.
    /// 2. **A site that names addresses must admit `local`.** The name is a client's choice and the
    ///    local address is the kernel's answer, so a site bound to an internal address cannot be
    ///    selected by a client that reached a public one.
    /// 3. Among the sites that survive, the host rule applies unchanged — exact, then longest
    ///    wildcard — to the ones that **do** survive. A name claimed only by an ineligible site
    ///    therefore falls through to a less specific pattern that can serve this connection, or to
    ///    an address-only site, or to the default — never to the ineligible site.
    /// 4. An **address-only** site (one with no `hosts`) is considered last, and the narrowest range
    ///    that admits `local` wins. This is the nginx `default_server` shape: one application on one
    ///    internal address, with no hostname to invent for it.
    ///
    /// A site that states neither binding behaves exactly as before — the filters admit everything
    /// — so a deployment that does not use these keys pays one comparison per candidate and nothing
    /// else.
    pub fn resolve_request(
        &self,
        host: Option<&str>,
        local: Option<IpAddr>,
        protocol: SiteProtocol,
    ) -> &ResolvedSite {
        let admits = |site: &ResolvedSite| admits_connection(site, local, protocol);

        // The most specific site whose name matches *and* which can serve this connection. The
        // `keep` predicate runs inside the walk, so a rejected exact name still falls through to
        // the wildcards rather than hiding them — see `HostMap::get_where`.
        let named = host
            .map(crate::hostmap::strip_port)
            .filter(|host| !host.is_empty())
            .and_then(|host| {
                self.hosts
                    .get_where(host, |name| {
                        self.named.get(name.as_str()).is_some_and(admits)
                    })
                    .and_then(|name| self.named.get(name.as_str()))
            });

        if let Some(site) = named {
            return site;
        }

        // Nothing claimed it by name. An address-only site is the answer when one admits this
        // connection, and an unknown local address (a unix socket) admits none of them.
        if let Some(local) = local {
            for (name, binding) in &self.address_only {
                if binding.admits(local) {
                    if let Some(site) = self.named.get(name.as_str()) {
                        if admits(site) {
                            return site;
                        }
                    }
                }
            }
        }

        &self.default
    }

    /// The host patterns this deployment claims, for diagnostics.
    ///
    /// The same table site selection uses, exposed so a caller can ask "is this name
    /// configured at all" without re-deriving the rule. The certificate resolver does not
    /// share this — it needs host→certificate, not host→site — but it shares `HostMap`.
    pub fn host_map(&self) -> &HostMap<String> {
        &self.hosts
    }

    /// The sites whose static layer was turned off because there was nothing to serve.
    ///
    /// The default site first, then the named ones in name order, which is the order
    /// `check` prints them in. Returned rather than logged here so this module stays a
    /// library: `serve` logs, `check` prints.
    pub fn suppressed_static_layer(&self) -> Vec<&ResolvedSite> {
        std::iter::once(&self.default)
            .chain(self.named.values())
            .filter(|site| site.static_files_suppressed)
            .collect()
    }
}

/// Whether a site may serve a connection described by `local` and `protocol`.
///
/// The one place the two filters are applied, so site selection and the diagnostics that explain it
/// cannot disagree about whether a site is eligible.
///
/// An **unknown** local address (`None`) fails closed for a bound site: admitting a site that names
/// addresses without being able to check them is the one direction that exposes an internal site
/// rather than hiding it. It is unreachable today — both front-ends bind a `TcpListener`, and
/// `server.http = "unix:…"` is refused at bind time with "invalid port value" — so this is a guard
/// that costs a comparison and cannot be triggered, rather than a path anything takes.
fn admits_connection(site: &ResolvedSite, local: Option<IpAddr>, protocol: SiteProtocol) -> bool {
    if !site.protocols.admits(protocol) {
        return false;
    }
    match local {
        Some(address) => site.binding.admits(address),
        None => !site.binding.is_bound(),
    }
}

/// Whether a site asked for the static layer, as opposed to inheriting it.
///
/// A site that says nothing inherits the server's answer — including the default, which is
/// "on". So the question a suppression has to answer is not "is it resolved on" (it usually
/// is) but "did someone write this down": a deployment that *typed* `enabled = true` is taken
/// at its word, and only a layer nobody asked for is switched off on its behalf.
fn site_asks_for_statics(site: &SiteConfig, server: &StaticsSetting) -> bool {
    match &site.static_files {
        Some(setting) => setting.asks_for_statics(),
        None => server.asks_for_statics(),
    }
}

/// Turn off a static layer that cannot answer anything, and say so.
///
/// This is the decision the warning used to merely describe. A front controller means every
/// request that is not a real file is the application's, so for a document root holding no
/// asset the layer answers `NotAFile` on every request — after a `stat` and a hop to the
/// blocking pool. Measured on the competition host, same binary, `GET /items` at c=32: 33,043
/// rps with the layer on against 42,142 with it off.
///
/// Three things keep this from being the silent behaviour change the warning was written to
/// avoid:
///
/// - **Only an unrequested layer is touched.** `explicit` says the deployment wrote the
///   setting down, and a stated preference is honoured even when this thinks it is a waste.
/// - **Only an app that cannot use it.** A root with any asset in it keeps the layer, so the
///   common `public/` deployment is never affected.
/// - **Always reported.** Every suppression is logged by `serve` and printed by `check`, so
///   the setting is never quietly different from the file.
fn suppress_unused_static_layer(site: &mut ResolvedSite, layer_could_run: bool, explicit: bool) {
    if explicit
        || !site.static_files.enabled
        || !layer_could_run
        || site.serve_only != ServeOnly::All
        // An empty controller is "serve only real files" — a deployment whose paths *are*
        // files, which is the one shape where this layer is the whole application.
        || site.front_controller.as_deref().is_none_or(str::is_empty)
        || !site.document_root.is_dir()
        || has_static_assets(&site.document_root)
    {
        return;
    }
    site.static_files.enabled = false;
    site.static_files_suppressed = true;
}

/// Whether anything under the document root could be served as a file.
///
/// Only the top level is read, and that is the deliberate part: the question is not
/// "does this application ship a file anywhere" — every application ships its own
/// `index.php`, which this layer refuses to serve — but "does it have an *assets*
/// story". An application that serves assets puts them in a directory beside the
/// front controller (`assets/`, `static/`, `css/`, `js/`, `images/`, `build/`,
/// `public/`); one that does not has a document root holding a controller and
/// nothing else. Reading one level deep is a `read_dir` at startup, not a walk.
///
/// It is deliberately generous. A directory is enough to keep the layer even if it is
/// empty, because a deployment may create it at build time; what this must never do is
/// tell an application with an asset directory that it has none — that would turn a
/// working `GET /logo.png` into the front controller's 404.
fn has_static_assets(document_root: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(document_root) else {
        // Unreadable is not "no assets": a root this cannot read is a root whose
        // contents are unknown, and guessing would take the layer away from a site
        // that may well be serving from it.
        return true;
    };
    entries.flatten().any(|entry| {
        entry.file_type().is_ok_and(|kind| kind.is_dir())
            || entry
                .path()
                .extension()
                .is_some_and(|ext| !is_script_extension(ext))
    })
}

/// Whether an extension names something a backend executes rather than a file.
///
/// The three PHP spellings are the ones this layer refuses to serve from disk, so a
/// document root holding only those is a document root with nothing to serve. The
/// list is the *default* script list rather than a site's configured one: being wrong
/// in the direction of "this is a script" only costs the layer, which is the safe
/// direction to be wrong in.
fn is_script_extension(extension: &std::ffi::OsStr) -> bool {
    matches!(
        extension.to_str().map(str::to_ascii_lowercase).as_deref(),
        Some("php" | "php3" | "phtml")
    )
}

/// Apply the fallbacks that turn a [`SiteConfig`] into a [`ResolvedSite`].
///
/// One place, so a site that omits a key and a site that repeats the global value
/// cannot end up with different settings.
#[allow(clippy::too_many_arguments)]
fn resolve_site(
    name: &str,
    site: &SiteConfig,
    default_front: Option<&str>,
    default_extensions: &crate::script::ScriptExtensions,
    default_serve_only: ServeOnly,
    default_statics: &StaticConfig,
    default_compression: &crate::config::CompressionSetting,
    default_routes: &[RouteConfig],
    default_backend: Option<&str>,
    default_redirect_to_https: bool,
    default_hsts: Option<&crate::config::HstsPolicy>,
) -> ResolvedSite {
    let front_controller = match &site.front_controller {
        Some(controller) => controller.as_path().map(str::to_string),
        None => default_front.map(str::to_string),
    };

    // Parsed against the site's own key rather than the server's, so a site that widens the list
    // says so in its own error message. Validation has already refused a bad list; the fallback
    // here is the inherited one rather than the default, which is what "absent inherits" means.
    let script_extensions = match &site.script_extensions {
        Some(extensions) => crate::script::ScriptExtensions::from_config(
            extensions,
            &format!("sites.{name}.script_extensions"),
        )
        .unwrap_or_else(|_| default_extensions.clone()),
        None => default_extensions.clone(),
    };

    let static_files = match &site.static_files {
        // Resolved against the server's layer, not against nothing: a site naming
        // one key must keep the root's other settings.
        Some(setting) => setting.applied_to(default_statics),
        None => default_statics.clone(),
    };

    let compression = match &site.compression {
        Some(overrides) => overrides.apply(default_compression),
        None => default_compression.clone(),
    };

    // Parsed here rather than on the request path, and *tolerantly*: validation has already refused
    // an entry that does not parse, so a malformed one reaching this point means the table was built
    // without `validate` — and dropping it is the direction that cannot expose a site. An entry that
    // silently vanished is reported by `check`, which prints the binding it actually holds.
    let binding = if site.addresses.is_empty() {
        SiteBinding::Any
    } else {
        SiteBinding::Ranges(
            site.addresses
                .iter()
                .filter_map(|spec| Cidr::parse(spec).ok())
                .collect(),
        )
    };

    ResolvedSite {
        name: name.to_string(),
        document_root: site.document_root.clone(),
        front_controller,
        script_extensions,
        serve_only: site.serve_only.unwrap_or(default_serve_only),
        static_files,
        // Decided by `suppress_unused_static_layer`, against this site's own root: whether a
        // site can use the layer is a property of its document root, not of the server's.
        static_files_suppressed: false,
        compression,
        routes: site
            .routes
            .clone()
            .unwrap_or_else(|| default_routes.to_vec()),
        default_backend: site
            .default_backend
            .clone()
            .or_else(|| default_backend.map(str::to_string)),
        binding,
        protocols: site
            .protocols
            .as_deref()
            .map(SiteProtocols::from_config)
            .unwrap_or_default(),
        // Both are the server's answer unless the site states one, which is the same direction
        // every other per-site key inherits in. `default_hsts` is passed already resolved, so a
        // site that states `max_age_s` alone keeps the server's other two directives rather than
        // resetting them.
        redirect_to_https: site.redirect_to_https.unwrap_or(default_redirect_to_https),
        hsts: match &site.hsts {
            Some(hsts) => hsts.resolved(default_hsts),
            None => default_hsts.cloned(),
        },
    }
}

/// A `Host` header without its `:port`, handling a bracketed IPv6 literal.
///
/// Moved to [`crate::hostmap`] so the site table, the certificate table and the route table
/// normalise a header identically; re-exported here because callers of this module had it.
#[allow(unused_imports)]
pub(crate) use crate::hostmap::strip_port;

#[cfg(test)]
mod tests {
    use super::*;

    fn config(text: &str) -> Config {
        let config = Config::from_toml(text).expect("config parses");
        config.validate().expect("config validates");
        config
    }

    fn ip(spec: &str) -> IpAddr {
        spec.parse().expect("the test names a valid address")
    }

    const TWO_SITES: &str = r#"
[server]
default_backend = "app"
http = "127.0.0.1:8080"
document_root = "/srv/default"
front_controller = "index.php"

[backends.app]
kind = "worker"
command = "php worker.php"

[backends.legacy]
kind = "spawn_cgi"
binary = 'C:\php\php-cgi.exe'

[sites.example_com]
hosts = ["example.com", "www.example.com", "*.shop.example.com"]
document_root = "/srv/example/public"
default_backend = "app"

[sites.corp]
hosts = ["corp.internal", "*.corp.internal", "*.deep.corp.internal"]
document_root = "/srv/corp"
front_controller = false

  routes = [
      { path_suffix = ".php", backend = "legacy" },
  ]
"#;

    #[test]
    fn an_empty_site_table_is_one_default_site() {
        // The single-site deployment must not be a special case in the request
        // path, so it is the same shape with one entry in it.
        let config = config("[server]\nhttp = \"127.0.0.1:8080\"\n");
        let sites = Sites::from_config(&config, Path::new("/srv/app"));

        assert!(sites.named().is_empty());
        let site = sites.resolve(Some("anything.test"));
        assert_eq!(site.name, "<default>");
        assert_eq!(site.document_root, PathBuf::from("/srv/app"));
        assert_eq!(site.front_controller.as_deref(), Some("index.php"));
        assert_eq!(site.serve_only, ServeOnly::All);
    }

    #[test]
    fn a_longer_wildcard_beats_a_shorter_one_for_the_same_host() {
        // The site names must differ, or the assertion cannot tell which pattern won.
        // The fixture above has both `*.corp.internal` and `*.deep.corp.internal` on
        // *one* site, so "the longest won" and "the shortest won" are the same answer
        // there — this test is the one that would catch a walk running shortest-first.
        let config = config(
            r#"
[server]
default_backend = "app"
http = "127.0.0.1:8080"
document_root = "/srv/default"

[backends.app]
kind = "worker"
command = "php worker.php"

[sites.wide]
hosts = ["*.corp.internal"]
document_root = "/srv/wide"

[sites.narrow]
hosts = ["*.deep.corp.internal"]
document_root = "/srv/narrow"
"#,
        );
        let sites = Sites::from_config(&config, Path::new("/srv/default"));

        // Three labels past the suffix: both patterns match, the longer one must win.
        assert_eq!(sites.resolve_str("x.deep.corp.internal").name, "narrow");
        // Two labels past `corp.internal`: only the wide rule matches.
        assert_eq!(sites.resolve_str("x.corp.internal").name, "wide");
        // The suffix itself, and its immediate parent, match neither wildcard.
        assert_eq!(sites.resolve_str("corp.internal").name, "<default>");
        assert_eq!(sites.resolve_str("deep.corp.internal").name, "wide");
    }

    #[test]
    fn an_exact_host_wins_then_the_longest_wildcard_then_the_default() {
        let config = config(TWO_SITES);
        let sites = Sites::from_config(&config, Path::new("/srv/default"));

        assert_eq!(sites.resolve_str("example.com").name, "example_com");
        assert_eq!(sites.resolve_str("www.example.com").name, "example_com");
        assert_eq!(sites.resolve_str("a.shop.example.com").name, "example_com");
        assert_eq!(sites.resolve_str("corp.internal").name, "corp");
        assert_eq!(sites.resolve_str("a.corp.internal").name, "corp");
        // Both of this site's wildcards match here and both name the same site, so
        // this only asserts the match; `a_longer_wildcard_beats_a_shorter_one_for_the_
        // same_host` is the test that distinguishes which one won.
        assert_eq!(sites.resolve_str("x.deep.corp.internal").name, "corp");

        // Unlisted, so the default site answers.
        assert_eq!(sites.resolve_str("other.test").name, "<default>");
        assert_eq!(sites.resolve(None).name, "<default>");
    }

    #[test]
    fn a_host_is_matched_without_its_port_and_without_regard_to_case() {
        let config = config(TWO_SITES);
        let sites = Sites::from_config(&config, Path::new("/srv/default"));

        assert_eq!(sites.resolve_str("EXAMPLE.com:8080").name, "example_com");
        assert_eq!(sites.resolve_str("corp.internal:443").name, "corp");
        assert_eq!(sites.resolve_str("[::1]:8080").name, "<default>");
        assert_eq!(sites.resolve_str("").name, "<default>");
        // `*.shop.example.com` is a subdomain rule, so the bare name it hangs off
        // is not a match — it is a different site's or the default's.
        assert_eq!(sites.resolve_str("shop.example.com").name, "<default>");
    }

    /// A site bound to addresses is unreachable by name from anywhere else.
    ///
    /// This is the whole of the key: `Host` is written by the client, so before this a site named
    /// `internal.corp` was selectable by anyone who could reach the port. The binding makes the
    /// kernel's answer part of the decision.
    #[test]
    fn a_bound_site_is_refused_by_name_on_another_address() {
        let text = r#"
[server]
http = "0.0.0.0:8080"
document_root = "/srv/public"

[sites.internal]
hosts = ["internal.corp"]
addresses = ["192.168.9.123"]
document_root = "/srv/internal"
"#;
        let config = config(text);
        let sites = Sites::from_config(&config, Path::new("/srv/public"));
        let internal = ip("192.168.9.123");
        let public = ip("135.181.172.123");

        // On the bound address the name selects the site.
        assert_eq!(
            sites
                .resolve_request(Some("internal.corp"), Some(internal), SiteProtocol::Http)
                .name,
            "internal"
        );

        // From the public address the *same name* is refused, and falls to the default site —
        // not to the internal one, which is the entire point.
        assert_eq!(
            sites
                .resolve_request(Some("internal.corp"), Some(public), SiteProtocol::Http)
                .name,
            "<default>"
        );

        // And a name nobody claims never reached it either.
        assert_eq!(
            sites
                .resolve_request(Some("other.test"), Some(internal), SiteProtocol::Http)
                .name,
            "<default>"
        );
    }

    /// A site that names addresses and no hosts is a default for those addresses.
    ///
    /// The nginx `default_server` shape: one application on one internal address, with no hostname
    /// to invent. It must answer an unclaimed name — and only there.
    #[test]
    fn an_address_only_site_answers_unclaimed_names_on_its_address() {
        let text = r#"
[server]
http = "0.0.0.0:8080"
document_root = "/srv/public"

[sites.internal]
addresses = ["192.168.9.0/24"]
document_root = "/srv/internal"
"#;
        let config = config(text);
        let sites = Sites::from_config(&config, Path::new("/srv/public"));

        assert_eq!(
            sites
                .resolve_request(
                    Some("anything.test"),
                    Some(ip("192.168.9.5")),
                    SiteProtocol::Http
                )
                .name,
            "internal"
        );
        assert_eq!(
            sites
                .resolve_request(None, Some(ip("192.168.9.5")), SiteProtocol::Http)
                .name,
            "internal"
        );
        assert_eq!(
            sites
                .resolve_request(
                    Some("anything.test"),
                    Some(ip("135.181.172.123")),
                    SiteProtocol::Http
                )
                .name,
            "<default>"
        );
    }

    /// Between two bound sites, the narrower range wins.
    ///
    /// The same specificity rule the host table applies to wildcards, so both halves of selection
    /// read the same way — and so a `/32` carved out of a `/24` is reachable.
    #[test]
    fn a_narrower_address_binding_beats_a_wider_one() {
        let text = r#"
[server]
http = "0.0.0.0:8080"
document_root = "/srv/public"

[sites.wide]
addresses = ["192.168.9.0/24"]
document_root = "/srv/wide"

[sites.narrow]
addresses = ["192.168.9.123"]
document_root = "/srv/narrow"
"#;
        let config = config(text);
        let sites = Sites::from_config(&config, Path::new("/srv/public"));

        assert_eq!(
            sites
                .resolve_request(None, Some(ip("192.168.9.123")), SiteProtocol::Http)
                .name,
            "narrow"
        );
        assert_eq!(
            sites
                .resolve_request(None, Some(ip("192.168.9.200")), SiteProtocol::Http)
                .name,
            "wide"
        );
    }

    /// A protocol restriction is decided by the connection, not by a header.
    #[test]
    fn a_site_restricted_to_https_does_not_answer_cleartext() {
        let text = r#"
[server]
http = "0.0.0.0:8080"
document_root = "/srv/public"

[sites.secure]
hosts = ["secure.test"]
protocols = ["https"]
document_root = "/srv/secure"
"#;
        let config = config(text);
        let sites = Sites::from_config(&config, Path::new("/srv/public"));
        let any = Some(ip("127.0.0.1"));

        assert_eq!(
            sites
                .resolve_request(Some("secure.test"), any, SiteProtocol::Https)
                .name,
            "secure"
        );
        assert_eq!(
            sites
                .resolve_request(Some("secure.test"), any, SiteProtocol::Http)
                .name,
            "<default>"
        );
        // And the FastCGI front-end is a third protocol, not a synonym for either.
        assert_eq!(
            sites
                .resolve_request(Some("secure.test"), any, SiteProtocol::Fastcgi)
                .name,
            "<default>"
        );
    }

    /// A name claimed by an ineligible site must not hide an eligible wildcard.
    ///
    /// The failure this prevents is subtle: an exact name bound to one address would otherwise
    /// *shadow* the wildcard that could serve the connection, and the request would fall all the
    /// way to the default rather than to the site that claims it by pattern.
    #[test]
    fn a_refused_exact_name_falls_through_to_a_wildcard_that_admits() {
        let text = r#"
[server]
http = "0.0.0.0:8080"
document_root = "/srv/public"

[sites.exact]
hosts = ["api.corp.test"]
addresses = ["192.168.9.123"]
document_root = "/srv/exact"

[sites.wild]
hosts = ["*.corp.test"]
document_root = "/srv/wild"
"#;
        let config = config(text);
        let sites = Sites::from_config(&config, Path::new("/srv/public"));

        // On the bound address the exact name wins, as it always does.
        assert_eq!(
            sites
                .resolve_request(
                    Some("api.corp.test"),
                    Some(ip("192.168.9.123")),
                    SiteProtocol::Http
                )
                .name,
            "exact"
        );
        // Elsewhere it is refused, and the wildcard serves it rather than the default.
        assert_eq!(
            sites
                .resolve_request(
                    Some("api.corp.test"),
                    Some(ip("135.181.172.123")),
                    SiteProtocol::Http
                )
                .name,
            "wild"
        );
    }

    /// A v4 binding does not admit a v4-mapped v6 peer address.
    ///
    /// Deliberate rather than an oversight: the refusal is visible, where silently comparing the
    /// masked halves would serve an internal site on a socket the operator did not think about.
    #[test]
    fn a_binding_compares_within_one_address_family() {
        let text = r#"
[server]
http = "[::]:8080"
document_root = "/srv/public"

[sites.internal]
hosts = ["internal.corp"]
addresses = ["192.168.9.123"]
document_root = "/srv/internal"
"#;
        let config = config(text);
        let sites = Sites::from_config(&config, Path::new("/srv/public"));

        assert_eq!(
            sites
                .resolve_request(
                    Some("internal.corp"),
                    Some(ip("::ffff:192.168.9.123")),
                    SiteProtocol::Http
                )
                .name,
            "<default>",
            "a mapped v6 address is a v6 address"
        );
        assert_eq!(
            sites
                .resolve_request(
                    Some("internal.corp"),
                    Some(ip("192.168.9.123")),
                    SiteProtocol::Http
                )
                .name,
            "internal"
        );
    }

    /// A site with neither a name nor an address is refused rather than silently unreachable.
    #[test]
    fn a_site_with_no_name_and_no_address_is_refused() {
        let text = r#"
[server]
http = "127.0.0.1:8080"
document_root = "/srv/default"

[sites.nothing]
document_root = "/srv/nothing"
"#;
        let config = Config::from_toml(text).expect("parses");
        let err = config
            .validate()
            .expect_err("a site nothing can reach is refused");
        let message = err.to_string();
        assert!(
            message.contains("hosts") && message.contains("addresses"),
            "the message must name both keys, got: {message}"
        );
    }

    /// A malformed address is refused, because the failure it hides is a site served too widely.
    #[test]
    fn a_site_with_a_malformed_address_is_refused() {
        let text = r#"
[server]
http = "127.0.0.1:8080"
document_root = "/srv/default"

[sites.typo]
hosts = ["typo.test"]
addresses = ["192.168.9.300"]
document_root = "/srv/typo"
"#;
        let config = Config::from_toml(text).expect("parses");
        let err = config.validate().expect_err("a bad address is refused");
        assert!(
            err.to_string().contains("sites.typo.addresses"),
            "the message must name the key, got: {err}"
        );
    }

    /// An empty protocol list would make a site reachable by nothing, so it is refused.
    #[test]
    fn a_site_with_an_empty_protocol_list_is_refused() {
        let text = r#"
[server]
http = "127.0.0.1:8080"
document_root = "/srv/default"

[sites.none]
hosts = ["none.test"]
protocols = []
document_root = "/srv/none"
"#;
        let config = Config::from_toml(text).expect("parses");
        let err = config
            .validate()
            .expect_err("an empty list is a site on no protocol");
        assert!(
            err.to_string().contains("protocols"),
            "the message must name the key, got: {err}"
        );
    }

    /// Two address-only sites claiming one address cannot be told apart.
    #[test]
    fn two_address_only_sites_claiming_one_address_are_refused() {
        let text = r#"
[server]
http = "127.0.0.1:8080"
document_root = "/srv/default"

[sites.a]
addresses = ["192.168.9.123"]
document_root = "/srv/a"

[sites.b]
addresses = ["192.168.9.123"]
document_root = "/srv/b"
"#;
        let config = Config::from_toml(text).expect("parses");
        let err = config
            .validate()
            .expect_err("two defaults for one address are ambiguous");
        assert!(
            err.to_string().contains("address-only"),
            "the message must explain the rule, got: {err}"
        );
    }

    #[test]
    fn a_site_inherits_every_key_it_omits() {
        let config = config(TWO_SITES);
        let sites = Sites::from_config(&config, Path::new("/srv/default"));

        // `example_com` states only hosts, root and default_backend, so the front
        // controller, serve_only, statics and routes come from `[server]`.
        let example = sites.resolve_str("example.com");
        assert_eq!(example.front_controller.as_deref(), Some("index.php"));
        assert_eq!(example.serve_only, ServeOnly::All);
        assert!(example.routes.is_empty());
        assert_eq!(
            example.static_files,
            config.server.statics(),
            "the inherited static layer must be the server's, not the defaults"
        );

        // `corp` states a boolean front controller and its own routes.
        let corp = sites.resolve_str("corp.internal");
        assert_eq!(corp.front_controller, None, "`false` means only real files");
        assert_eq!(corp.routes.len(), 1);
        assert_eq!(corp.routes[0].path_suffix.as_deref(), Some(".php"));
        assert_eq!(corp.default_backend.as_deref(), Some("app"));
    }

    #[test]
    fn a_site_static_setting_is_merged_key_by_key() {
        // A site naming one key keeps the root's other settings. All-or-nothing
        // inheritance would silently reset `cache_control` to the default here.
        let text = r#"
[server]
http = "127.0.0.1:8080"
document_root = "/srv/default"

[server.static_files]
enabled = true
cache_control = "public, max-age=60"

[sites.a]
hosts = ["a.test"]
document_root = "/srv/a"
static_files = { precompressed = true }
"#;
        let config = config(text);
        let sites = Sites::from_config(&config, Path::new("/srv/default"));
        let site = sites.resolve_str("a.test");

        assert!(site.static_files.precompressed, "the site's own key");
        assert!(
            site.static_files.enabled,
            "the root's enabled must survive the merge"
        );
        assert_eq!(
            site.static_files.cache_control, "public, max-age=60",
            "and so must the root's cache_control"
        );
    }

    #[test]
    fn a_site_can_turn_the_static_layer_off_while_the_root_has_it_on() {
        // The opt-out has to work in the direction that matters: a root serving
        // files, one application site that must not.
        let text = r#"
[server]
http = "127.0.0.1:8080"
document_root = "/srv/default"
static_files = { enabled = true }

[sites.api]
hosts = ["api.test"]
document_root = "/srv/api"
static_files = false
"#;
        let config = config(text);
        let sites = Sites::from_config(&config, Path::new("/srv/default"));

        assert!(sites.default_site().static_files.enabled);
        assert!(
            !sites.resolve_str("api.test").static_files.enabled,
            "a site must be able to opt out of a layer the root turned on"
        );
    }

    #[test]
    fn the_default_site_carries_the_servers_route_table() {
        // Routes on `[server]` apply to the default site, not to every site: a site
        // that omits `routes` inherits them, which is what makes one application on
        // several hosts one configuration.
        let text = r#"
[server]
default_backend = "app"
http = "127.0.0.1:8080"
document_root = "/srv/default"

[backends.app]
kind = "worker"
command = "php worker.php"

[backends.admin]
kind = "worker"
command = "php worker.php"

[[server.routes]]
path_prefix = "/admin"
backend = "admin"

[sites.a]
hosts = ["a.test"]
document_root = "/srv/a"
"#;
        let config = config(text);
        let sites = Sites::from_config(&config, Path::new("/srv/default"));

        assert_eq!(sites.default_site().routes.len(), 1);
        assert_eq!(
            sites.resolve_str("a.test").routes.len(),
            1,
            "a site that omits routes inherits the server's"
        );
    }

    // ---- the static layer nobody asked for -------------------------------------------

    /// A root that holds only what a front-controller application has in it.
    fn controller_only(name: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("volucer-sites-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create the fixture");
        std::fs::write(path.join("index.php"), "<?php").expect("write the controller");
        path
    }

    fn front_controller_config(extra: &str) -> Config {
        let text = format!(
            r#"
[server]
default_backend = "app"
http = "127.0.0.1:8080"
{extra}

[backends.app]
kind = "worker"
command = "php worker.php"
"#
        );
        config(&text)
    }

    /// The decision, and the two things that bound it.
    ///
    /// Asserted here rather than only through `inspect` because this is where the answer lives:
    /// the request path reads `ResolvedSite::static_files`, so a test that only asked `check`
    /// would be asserting on the report of a decision instead of the decision.
    #[test]
    fn a_layer_nobody_asked_for_is_turned_off_when_the_root_has_no_asset() {
        let root = controller_only("auto-off");
        let config = front_controller_config("");
        let sites = Sites::from_config(&config, &root);

        assert!(
            !sites.default_site().static_files.enabled,
            "a root holding only a controller cannot use the layer"
        );
        assert!(sites.default_site().static_files_suppressed);
        assert_eq!(sites.suppressed_static_layer().len(), 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Every one of these is a reason to keep a layer that looks useless.
    ///
    /// One test with five cases rather than five tests, because they are one rule with one
    /// direction: each says "this site is not the shape that pays for nothing", and the failure
    /// this guards against — taking the layer away from a site that serves files — is the same
    /// bug in all five.
    #[test]
    fn the_layer_is_kept_whenever_it_could_be_doing_work() {
        // A directory is enough, even empty: a build may fill it later.
        let with_directory = controller_only("has-dir");
        std::fs::create_dir_all(with_directory.join("assets")).expect("create the asset directory");
        let layer_by_default = front_controller_config("");
        assert!(
            Sites::from_config(&layer_by_default, &with_directory)
                .default_site()
                .static_files
                .enabled,
            "a directory is an assets story"
        );
        let _ = std::fs::remove_dir_all(&with_directory);

        // A non-script file is enough.
        let with_file = controller_only("has-file");
        std::fs::write(with_file.join("favicon.ico"), b"\x00").expect("write the asset");
        assert!(
            Sites::from_config(&layer_by_default, &with_file)
                .default_site()
                .static_files
                .enabled,
            "a file the layer would serve is an asset"
        );
        let _ = std::fs::remove_dir_all(&with_file);

        // A deployment that wrote `enabled = true` is taken at its word.
        let explicit = controller_only("explicit");
        let stated = front_controller_config("static_files = { enabled = true }");
        let sites = Sites::from_config(&stated, &explicit);
        assert!(sites.default_site().static_files.enabled);
        assert!(
            !sites.default_site().static_files_suppressed,
            "kept, and not reported as suppressed"
        );
        let _ = std::fs::remove_dir_all(&explicit);

        // A deployment with no backend is files-only, and the layer *is* the application.
        let files_only = controller_only("files-only");
        let backendless = config(
            r#"
[server]
http = "127.0.0.1:8080"
"#,
        );
        assert!(
            Sites::from_config(&backendless, &files_only)
                .default_site()
                .static_files
                .enabled,
            "with no backend, a layer that declines every path is the whole application"
        );
        let _ = std::fs::remove_dir_all(&files_only);

        // An empty front controller means the paths *are* files, so the layer is the point.
        let files_only = controller_only("no-controller");
        let mut no_controller = front_controller_config("");
        no_controller.server.front_controller = Some(String::new());
        assert!(
            Sites::from_config(&no_controller, &files_only)
                .default_site()
                .static_files
                .enabled,
            "`front_controller = false` is the files-only deployment"
        );
        let _ = std::fs::remove_dir_all(&files_only);
    }

    /// A site is judged against its own document root, not the default site's.
    ///
    /// The bug this rules out is a single answer applied to every site: a vhost deployment in
    /// which one application serves assets and another does not is the ordinary case, and a
    /// process-wide decision would take the layer from the one that needs it.
    #[test]
    fn each_site_is_judged_against_its_own_root() {
        let application = controller_only("vhost-app");
        let static_site = controller_only("vhost-static");
        std::fs::create_dir_all(static_site.join("assets")).expect("create the asset directory");

        let text = format!(
            r#"
[server]
default_backend = "app"
http = "127.0.0.1:8080"

[backends.app]
kind = "worker"
command = "php worker.php"

[sites.application]
hosts = ["app.test"]
document_root = {app:?}

[sites.static]
hosts = ["static.test"]
document_root = {static_site:?}
"#,
            app = application.display().to_string(),
            static_site = static_site.display().to_string(),
        );
        let config = config(&text);
        let sites = Sites::from_config(&config, Path::new("/srv/default"));

        assert!(
            !sites.resolve_str("app.test").static_files.enabled,
            "the application site holds only a controller"
        );
        assert!(
            sites.resolve_str("static.test").static_files.enabled,
            "the asset site must keep its layer"
        );
        // Named, and only the one: `check` reports these, so the list has to be the sites that
        // were actually changed.
        let names: Vec<&str> = sites
            .suppressed_static_layer()
            .iter()
            .map(|site| site.name.as_str())
            .collect();
        assert_eq!(names, vec!["application"], "{names:?}");

        let _ = std::fs::remove_dir_all(&application);
        let _ = std::fs::remove_dir_all(&static_site);
    }
}
