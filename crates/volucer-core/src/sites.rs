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
use std::path::{Path, PathBuf};

use crate::config::{Config, RouteConfig, ServeOnly, SiteConfig, StaticConfig, StaticsSetting};
use crate::hostmap::HostMap;

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
            routes: config.routes.clone(),
            default_backend: config.default_backend.clone(),
        };
        suppress_unused_static_layer(
            &mut default,
            can_shadow_a_backend,
            config.server.static_files.asks_for_statics(),
        );

        let mut named = BTreeMap::new();
        let mut hosts: HostMap<String> = HostMap::new();

        for (name, site) in &config.sites {
            let mut resolved = resolve_site(
                name,
                site,
                default_front.as_deref(),
                &default_extensions,
                config.server.serve_only,
                &default_statics,
                &default_compression,
                &config.routes,
                config.default_backend.as_deref(),
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

            named.insert(name.clone(), resolved);
        }

        Sites {
            default,
            named,
            hosts,
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
/// rps with the layer on against 42,142 with it off. See hazard 85 in `internal/plan.md`.
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

    const TWO_SITES: &str = r#"
default_backend = "app"

[server]
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
default_backend = "app"

[server]
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
default_backend = "app"

[server]
http = "127.0.0.1:8080"
document_root = "/srv/default"

[backends.app]
kind = "worker"
command = "php worker.php"

[backends.admin]
kind = "worker"
command = "php worker.php"

[[routes]]
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
default_backend = "app"

[server]
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
default_backend = "app"

[server]
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
