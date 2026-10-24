//! Sites and routes: which document root a host reaches, and which backend a path reaches.
//!
//! A site is decided before `SCRIPT_FILENAME` is resolved and a route after, which is why both tables
//! exist. The static-file settings they share live here too, since a route and a site each carry
//! overrides for them.

use super::*;

/// One site: a document root, and optionally its own routing and static layer.
///
/// Every key except `hosts` and `document_root` falls back to the `[server]`
/// value, so a site that only sets those two inherits the rest — the usual
/// shape when several sites share one application.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct SiteConfig {
    /// The `Host` names this site answers to.
    ///
    /// An exact name, case-insensitively, with any `:port` ignored — or a
    /// leading `*.` wildcard, which matches one or more labels (`*.example.com`
    /// matches `a.example.com` and `a.b.example.com`).
    pub hosts: Vec<String>,

    /// Directory that this site's scripts and files resolve against.
    ///
    /// Absolutised at startup for the same reason `server.document_root` is:
    /// `php-cgi` runs in a persistent pool and `chdir()`s into the directory of
    /// the script it executed, so a relative root resolves against the previous
    /// request's directory from the second request onwards.
    pub document_root: PathBuf,

    /// This site's front controller; see [`ServerConfig::front_controller`].
    #[serde(default)]
    pub front_controller: Option<FrontController>,

    /// This site's script extensions; see [`ServerConfig::script_extensions`].
    ///
    /// Absent inherits the `[server]` list. A site serving a Django app beside a
    /// PHP one is the shape this exists for: the site with the Python worker
    /// names `["py"]`, and the site with the PHP front controller keeps the
    /// default. Listing the extension an *entry point* has rather than every
    /// file the application imports is the rule — the same rule as
    /// `path_suffix`, since a path the router does not recognise reaches the
    /// front controller as application input either way.
    #[serde(default)]
    pub script_extensions: Option<Vec<String>>,

    /// Backend used when a request to this site matches no route.
    #[serde(default)]
    pub default_backend: Option<String>,

    /// This site's route table, replacing the global one when present.
    #[serde(default)]
    pub routes: Option<Vec<RouteConfig>>,

    /// This site's static layer. Absent inherits the `[server]` one.
    #[serde(default)]
    pub static_files: Option<StaticsSetting>,

    /// This site's compression overrides. Absent inherits `[server.compression]`.
    #[serde(default)]
    pub compression: Option<CompressionOverrides>,

    /// What this site answers with; see [`ServeOnly`].
    ///
    /// Absent inherits the `[server]` value, which itself defaults to
    /// `ServeOnly::All`.
    #[serde(default)]
    pub serve_only: Option<ServeOnly>,

    /// This site's own certificate, presented by SNI for the site's hosts.
    ///
    /// Absent inherits `[server.tls]`. This is the only per-site TLS setting, because it is
    /// the only one that is a statement about the site rather than about the socket; see
    /// [`SiteTlsConfig`].
    #[serde(default)]
    pub tls: Option<SiteTlsConfig>,

    /// This site's certificates, obtained automatically.
    ///
    /// Absent inherits `[server.acme]`, which means the deployment's certificate covers this site's
    /// hosts too. Naming a table here gives the site its **own** certificate, for its own names,
    /// which is what a deployment with several unrelated hostnames wants — one certificate per
    /// hostname rather than one covering all of them.
    ///
    /// A site that states its own [`tls`](SiteConfig::tls) is left alone: naming a certificate is a
    /// decision not to have one issued, and honouring both would mean the site had two answers.
    #[serde(default)]
    pub acme: Option<SiteAcmeConfig>,
}

/// What a deployment serves.
///
/// The name is the rule: *serve only* this. Absent means "serve everything",
/// which is static files first and then a backend — the behaviour that existed
/// before this key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServeOnly {
    /// Static files first, then the backend. The default.
    #[default]
    All,
    /// The backend only: a path naming a file is dispatched like any other.
    Backend,
    /// Files only: a path naming no file is a 404, and no backend is consulted.
    Static,
}

impl ServeOnly {
    /// Whether the static layer runs for this site.
    pub fn serves_files(self) -> bool {
        !matches!(self, ServeOnly::Backend)
    }

    /// Whether a path that names no file may reach a backend.
    pub fn serves_backend(self) -> bool {
        !matches!(self, ServeOnly::Static)
    }

    /// The name as it is written in configuration, for logs and diagnostics.
    pub fn as_str(self) -> &'static str {
        match self {
            ServeOnly::All => "all",
            ServeOnly::Backend => "backend",
            ServeOnly::Static => "static",
        }
    }
}

impl std::fmt::Display for ServeOnly {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A front controller as configuration spells it.
///
/// A string is the script's path below the document root. `false` means "serve
/// only real `.php` files", which the global key can only express as an empty
/// string on the command line — TOML cannot tell `front_controller = ""` from an
/// absent key, so the file reserves the meaning and a *flag* supplies it. A site
/// has no flag of its own, so it needs a spelling, and a boolean is one that
/// cannot be confused with a path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum FrontController {
    /// A relative path below the document root, e.g. `index.php`.
    Path(String),
    /// `false`: no front controller, so only real scripts are served.
    FilesOnly(bool),
}

impl FrontController {
    /// The controller as a path, or `None` for "serve only real files".
    pub fn as_path(&self) -> Option<&str> {
        match self {
            FrontController::Path(path) => Some(path.as_str()),
            FrontController::FilesOnly(_) => None,
        }
    }
}

/// A static-file setting as configuration spells it.
///
/// A table tunes the layer; `false` turns it off and `true` turns it on with the
/// defaults. The bare boolean exists because "off" is otherwise
/// `static_files = { enabled = false }`, and the short form is what a reader
/// reaches for.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum StaticsSetting {
    /// The key was not written. On, by default — see `Default` — but *not* an explicit
    /// request, which is what lets a FastCGI-only deployment leave it out and still validate.
    Unspecified,
    /// A tuning table, naming only the keys it changes.
    Table(StaticOverrides),
    /// `false` disables the layer; `true` enables it with the defaults.
    Switch(bool),
}

impl Default for StaticsSetting {
    /// Absent means the layer is **on**, with the defaults.
    ///
    /// It used to be off, so that upgrading did not change an existing deployment: a path
    /// that had reached the front controller still did. There are no deployments yet, and
    /// the cost of the old default was that every new site had to be told to serve its own
    /// assets. Turning it on only changes the outcome for a path that names a real,
    /// non-script file — `/about` still reaches the front controller — so a front-controller
    /// application is unaffected either way.
    ///
    /// `Unspecified` rather than `Switch(true)`, because the two have to be told apart:
    /// static content is served over HTTP only, so *asking* for it with no HTTP listener is a
    /// mistake worth reporting, while never mentioning it is not asking for anything. A
    /// FastCGI-only deployment that leaves the key out must still validate.
    fn default() -> Self {
        StaticsSetting::Unspecified
    }
}

impl StaticsSetting {
    /// Whether a deployment actually asked for the static layer, as opposed to getting it by
    /// default. Only an explicit request can be contradicted by the rest of the file.
    pub fn is_explicit(&self) -> bool {
        !matches!(self, StaticsSetting::Unspecified)
    }

    /// Whether the key is present and says "on".
    pub fn asks_for_statics(&self) -> bool {
        match self {
            StaticsSetting::Unspecified => false,
            StaticsSetting::Switch(enabled) => *enabled,
            StaticsSetting::Table(overrides) => overrides.enabled == Some(true),
        }
    }

    /// The setting applied over `base`.
    ///
    /// A site resolves its static layer against the server's rather than against
    /// nothing, so a site naming one key keeps the root's other settings — an
    /// application that overrides `precompressed` must not silently reset
    /// `cache_control` to the default for its whole document root.
    pub fn applied_to(&self, base: &StaticConfig) -> StaticConfig {
        match self {
            // The key was not written, so the layer is the one already in effect. That the
            // *default* is on is expressed by `ServerConfig::statics` seeding the base with
            // `enabled: true`, not by this arm — which is what keeps "absent" and "written
            // true" distinguishable for validation.
            StaticsSetting::Unspecified => base.clone(),
            StaticsSetting::Table(overrides) => overrides.apply(base),
            // A bare boolean says one thing about `enabled` and nothing about the
            // rest, so the rest is inherited — which is what makes
            // `static_files = false` a usable opt-out for a site whose root serves
            // files with settings worth keeping.
            StaticsSetting::Switch(enabled) => StaticConfig {
                enabled: *enabled,
                ..base.clone()
            },
        }
    }

    /// The setting applied over the defaults, which is what a deployment with no
    /// inherited value gets.
    pub fn resolved(&self) -> StaticConfig {
        self.applied_to(&StaticConfig::default())
    }
}

/// The keys a `static_files` table states, each optional.
///
/// `Option` per key rather than a [`StaticConfig`] with defaults, because "did not
/// mention it" and "mentioned it and chose the default" are different answers when
/// the table is an override. Without that distinction a site setting
/// `precompressed = true` would also assert `enabled = false` and
/// `cache_control = "…"` — the defaults — over whatever the server had said.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StaticOverrides {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub precompressed: Option<bool>,
    #[serde(default)]
    pub directory_index: Option<bool>,
    #[serde(default)]
    pub dotfiles: Option<Dotfiles>,
    #[serde(default)]
    pub cache_control: Option<String>,
    #[serde(default)]
    pub max_multirange_bytes: Option<u64>,
}

impl StaticOverrides {
    /// The base settings with these overrides applied.
    ///
    /// `enabled` is the field whose default differs from [`StaticOverrides::default`]: a
    /// site that tunes only `precompressed` gets the layer it was already getting, while
    /// `static_files = false` is one key that says only "off". Both come out of this one
    /// function so the two spellings cannot disagree.
    pub fn apply(&self, base: &StaticConfig) -> StaticConfig {
        StaticConfig {
            enabled: self.enabled.unwrap_or(base.enabled),
            precompressed: self.precompressed.unwrap_or(base.precompressed),
            directory_index: self.directory_index.unwrap_or(base.directory_index),
            dotfiles: self.dotfiles.unwrap_or(base.dotfiles),
            cache_control: self
                .cache_control
                .clone()
                .unwrap_or_else(|| base.cache_control.clone()),
            max_multirange_bytes: self
                .max_multirange_bytes
                .unwrap_or(base.max_multirange_bytes),
        }
    }
}

/// What a matching route asks to be done with the request.
///
/// The distinction that matters is *which layer answers*, not which backend runs: a site's
/// `serve_only` is one setting for the whole site, and a route is the only place a
/// deployment can say "this subtree is files, that one is the application". Without it,
/// `/assets` costs a `stat` on every request and a miss falls through to the front
/// controller, which is what an application host pays for having a document root at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum RoutePolicy {
    /// Static files first, then the route's backend. What a site does by default.
    #[default]
    All,
    /// The route's backend only: a path naming a file is dispatched like any other.
    Backend,
    /// Files only: a path naming no file is a 404, and no backend is consulted.
    ///
    /// Refused when the route names no backend *and* the site has no
    /// `default_backend`, because there would be nothing for `/assets/missing.png` to be
    /// a 404 *instead of* — see `Config::validate`.
    Static,
}

impl RoutePolicy {
    /// Whether the static layer runs for a request matched by this route.
    pub fn serves_files(self) -> bool {
        !matches!(self, RoutePolicy::Backend)
    }

    /// Whether a path that names no file may reach the route's backend.
    pub fn serves_backend(self) -> bool {
        !matches!(self, RoutePolicy::Static)
    }

    /// The name as it is written in configuration, for logs and diagnostics.
    pub fn as_str(self) -> &'static str {
        match self {
            RoutePolicy::All => "all",
            RoutePolicy::Backend => "backend",
            RoutePolicy::Static => "static",
        }
    }
}

/// On-the-fly compression of responses a backend produced.
///
/// Absent (or `enabled = false`) means nothing is compressed, which is the default and the
/// cheap choice. The static layer's precompressed `.br`/`.gz` variants are a different
/// mechanism and are unaffected by this: those files cost nothing per request, because the
/// work was done at build time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompressionSetting {
    /// Whether responses are compressed at all.
    #[serde(default)]
    pub enabled: bool,

    /// Encodings to offer, in preference order.
    ///
    /// Order decides ties: a client accepting both `gzip` and `zstd` at the same quality gets
    /// whichever comes first here, so the answer is stable rather than dependent on how the
    /// request happened to be parsed. An explicit `q` value in the request still wins, because
    /// a client knows its own end better than the deployment does.
    #[serde(default = "default_compression_encodings")]
    pub encodings: Vec<String>,

    /// Bodies smaller than this are passed through unchanged.
    ///
    /// A deflate stream's framing is a few tens of bytes and the CPU is a few microseconds, so
    /// below some size the request pays for nothing a client would notice. `volucer-http`'s own
    /// default is 1024 when this is unset.
    #[serde(default)]
    pub min_length: Option<usize>,

    /// The level handed to the encoder.
    ///
    /// Clamped by encoding rather than refused: gzip accepts 1?9 and zstd 1?19, so a single key
    /// cannot mean the same number to both. Out-of-range values are clamped at the encoder.
    #[serde(default = "default_compression_level")]
    pub level: u32,
}

fn default_compression_encodings() -> Vec<String> {
    // gzip first: every client accepts it, so a client offering both gets the answer with no
    // compatibility surface. zstd second, for the ones that can use it.
    vec!["gzip".to_string(), "zstd".to_string()]
}

/// Level 6 is gzip's own default and is near zstd's sweet spot at these sizes.
fn default_compression_level() -> u32 {
    6
}

impl Default for CompressionSetting {
    fn default() -> Self {
        Self {
            enabled: false,
            encodings: default_compression_encodings(),
            min_length: None,
            level: default_compression_level(),
        }
    }
}

/// A site's overrides for compression.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompressionOverrides {
    /// Whether this site's responses are compressed.
    #[serde(default)]
    pub enabled: Option<bool>,

    /// This site's encodings, in preference order.
    ///
    /// Replaces the server's list rather than adding to it, the same rule `script_extensions`
    /// and `path_suffix` follow: a site saying "gzip only" must stop returning zstd.
    #[serde(default)]
    pub encodings: Option<Vec<String>>,

    /// This site's minimum length.
    #[serde(default)]
    pub min_length: Option<usize>,

    /// This site's encoder level.
    #[serde(default)]
    pub level: Option<u32>,
}

impl CompressionOverrides {
    /// The base settings with these overrides applied.
    pub fn apply(&self, base: &CompressionSetting) -> CompressionSetting {
        CompressionSetting {
            enabled: self.enabled.unwrap_or(base.enabled),
            encodings: self
                .encodings
                .clone()
                .unwrap_or_else(|| base.encodings.clone()),
            min_length: self.min_length.or(base.min_length),
            level: self.level.unwrap_or(base.level),
        }
    }
}

/// A route entry, selecting a backend for matching requests.
///
/// Both matchers are optional, but at least one must be present — a route with
/// neither is just `default_backend` written the long way, and it would make the
/// precedence rule meaningless. Constraints within one route are **all** of
/// ("and"), not any of.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct RouteConfig {
    /// Match a request whose path starts with this string, e.g. `/api`.
    #[serde(default)]
    pub path_prefix: Option<String>,

    /// Match a request whose path ends with this string, e.g. `.php`.
    ///
    /// Compared case-insensitively, deliberately: the router decides what a
    /// script is case-insensitively, so a case-sensitive suffix here would let
    /// `/upload.PHP` be recognised as a script by one layer and miss its route in
    /// the other. Two answers to "is this PHP" is the drift the static layer
    /// already guards against by sharing `has_php_extension`.
    #[serde(default)]
    pub path_suffix: Option<String>,

    /// Match a request whose `Host` header is exactly this, ignoring the
    /// `:port` suffix. Case-insensitive.
    #[serde(default)]
    pub host: Option<String>,

    /// Which layer answers. Defaults to `all`, which is what the site already does.
    #[serde(default)]
    pub policy: RoutePolicy,

    /// The backend to dispatch to.
    ///
    /// Optional only for `policy = "static"`, which never dispatches: a route that can
    /// reach a backend without naming one would have to guess, and the guess would be
    /// `default_backend` — which is the site's setting, not this route's. Refused
    /// otherwise, at the route that omitted it.
    #[serde(default)]
    pub backend: Option<String>,
}

impl RouteConfig {
    /// How specific this route is: the number of constraints it states.
    ///
    /// Used to pick a winner when several routes match, so a route naming a host
    /// and a path beats one naming only the path.
    pub fn specificity(&self) -> usize {
        self.host.is_some() as usize
            + self.path_prefix.is_some() as usize
            + self.path_suffix.is_some() as usize
    }

    /// Whether this route's host constraint is an exact name rather than a
    /// wildcard.
    ///
    /// An exact host is the more specific claim about the same request, so it wins
    /// a tie against a wildcard that also matches — see [`Config::route_backend`].
    pub fn has_exact_host(&self) -> bool {
        self.host
            .as_deref()
            .is_some_and(|host| !host.starts_with("*."))
    }

    /// Whether this route's constraints all match the given host and path.
    pub fn matches(&self, host: Option<&str>, path: &str) -> bool {
        if let Some(wanted) = &self.path_prefix {
            if !path.starts_with(wanted.as_str()) {
                return false;
            }
        }
        if let Some(wanted) = &self.path_suffix {
            // Byte-wise and case-insensitive, like `has_php_extension`, rather than
            // `path.to_ascii_lowercase().ends_with(...)`, which would allocate a copy
            // of the whole path per request to answer a question about its last few
            // bytes. The comparison is on bytes because a byte offset taken from the
            // end of a `&str` can land inside a multi-byte character and slicing
            // there panics, so a path with non-ASCII in it must still be answerable.
            let wanted = wanted.as_bytes();
            let bytes = path.as_bytes();
            if bytes.len() < wanted.len()
                || !bytes[bytes.len() - wanted.len()..].eq_ignore_ascii_case(wanted)
            {
                return false;
            }
        }
        if let Some(wanted) = &self.host {
            if !host_matches(wanted, host) {
                return false;
            }
        }
        true
    }
}

/// Whether a `Host` header matches a configured host pattern.
///
/// One function for the route table, and [`crate::hostmap::HostMap`] for the site and
/// certificate tables. The two are separate implementations of one rule — a map probe versus a
/// walk over the host's own labels — so they are pinned against each other by a test
/// (`this_table_and_host_matches_agree_on_every_shape` in the hostmap module) rather than
/// trusted to stay in step. That test found the divergence it was written for: this function
/// used to accept `a..example.com` for `*.example.com`, because an empty label is still a label
/// by this arithmetic.
///
/// The pattern is an exact name, compared case-insensitively with any `:port` stripped, or a
/// leading `*.` wildcard. `*.example.com` matches `a.example.com` and `a.b.example.com` but
/// *not* `example.com` itself: a wildcard names a subdomain, and treating it as also matching
/// the bare domain would silently widen every rule that used one.
pub fn host_matches(pattern: &str, host: Option<&str>) -> bool {
    let host = host.unwrap_or_default();
    let host = strip_port(host);
    if host.is_empty() {
        return false;
    }

    match pattern.strip_prefix("*.") {
        None => host.eq_ignore_ascii_case(pattern),
        Some(suffix) => {
            // The host must be longer than the suffix *plus* the separating dot, and that dot
            // must be there. Both halves are needed: without the length check, `.example.com`
            // would pass — it is exactly one byte longer than the suffix and that byte is a
            // dot — and the wildcard would be claimed by an empty label.
            let suffix_len = suffix.len();
            if host.len() <= suffix_len + 1 {
                return false;
            }
            let dot = host.len() - suffix_len - 1;
            if host.as_bytes()[dot] != b'.' {
                return false;
            }
            // And the label *before* that dot must be non-empty, which one length check cannot
            // express: `a..example.com` is longer than the suffix, and the byte before the
            // suffix really is a dot — but the label it separates is empty, so this is not a
            // subdomain of `example.com`. A name with a zero-length label is not a host name.
            if dot == 0 || host.as_bytes()[dot - 1] == b'.' {
                return false;
            }
            host[host.len() - suffix_len..].eq_ignore_ascii_case(suffix)
        }
    }
}

/// Pick a backend from a route table, or `None` when nothing matches.
///
/// Free rather than a method on [`Config`] because a site carries its own table, and
/// the two must be selected by the same rule — a site whose routes were matched even
/// slightly differently from the server's would be a bug nobody could reproduce from
/// reading either table alone.
pub fn select_backend<'a>(
    routes: &'a [RouteConfig],
    host: Option<&str>,
    path: &str,
) -> Option<&'a str> {
    select_route(routes, host, path).and_then(|route| route.backend.as_deref())
}

/// [`select_backend`], keeping the route so its other properties are available.
pub fn select_route<'a>(
    routes: &'a [RouteConfig],
    host: Option<&str>,
    path: &str,
) -> Option<&'a RouteConfig> {
    routes
        .iter()
        .filter(|route| route.matches(host, path))
        .max_by_key(|route| (route.specificity(), route.has_exact_host()))
}

/// Whether a route proves its path names a script, so the caller need not `stat` it.
///
/// `php-cgi` checks that a script exists and answers `No input file specified` when it
/// does not, so *existence* is genuinely its business. What it does **not** do is check
/// that the file is PHP: handed `config.json` it emits it verbatim with a 200. So the `stat` in
/// the router is doing two jobs, and only one of them is
/// removable.
///
/// The removable one is removable exactly when the pattern already pins a script
/// extension: then the file can only be a script, and its absence is PHP's own 404. That is a
/// static property of the configuration, so it is decided once at startup rather than
/// per request — and never written by hand, because the decision belongs with the code
/// that knows why it is safe.
///
/// `extensions` is the site's list rather than a constant. An earlier version of this function compared
/// against `["php", "php3", "phtml"]` directly, so a site that widened the router's list would
/// have had its `.py` routes quietly keep the `stat` — safe, but a second spelling of one rule
/// that could drift in the other direction too.
pub fn skip_stat(
    route: &RouteConfig,
    backend_interprets_path: bool,
    extensions: &crate::script::ScriptExtensions,
) -> bool {
    // A route may name no backend, which means `policy = "static"`: the static layer answers
    // every request under it and nothing dispatches. `backend_interprets_path` is then
    // vacuously false, and treating that as "skip the stat" would remove the one check a
    // files-only subtree depends on -- so the caller is the wrong place to ask. Nothing to skip
    // for: with no backend the stat is the only thing that resolves the request.
    if route.backend.is_none() {
        return false;
    }
    // A backend that does not interpret the path never reads the file it names, so there is
    // nothing for the stat to protect. That covers two kinds for two different reasons: a worker
    // reads `script_filename` as metadata and routes on `REQUEST_URI`, and an upstream is handed
    // the path as a URL and does its own routing with it. See `BackendConfig::interprets_path`.
    if !backend_interprets_path {
        return true;
    }
    // A suffix that pins a script extension, e.g. `.php`. Compared with the same
    // case-insensitive rule the router uses, so the two cannot disagree about `/upload.PHP`.
    route
        .path_suffix
        .as_deref()
        .is_some_and(|suffix| is_script_extension(suffix, extensions))
}

/// Whether a `path_suffix` names a script extension, and nothing but one.
///
/// `".php"` and `"/entry.php"` do; `".json"` and `".css"` do not. A prefix says nothing
/// about the extension at all, which is why only a suffix can make this claim.
fn is_script_extension(suffix: &str, extensions: &crate::script::ScriptExtensions) -> bool {
    // The last `.`-separated part of the suffix is the extension it pins, so a suffix
    // carrying a path (`/api/entry.php`) is judged by its end rather than rejected.
    match suffix.rsplit_once('.') {
        Some((_, extension)) => extensions
            .as_slice()
            .iter()
            .any(|known| extension.eq_ignore_ascii_case(known)),
        None => false,
    }
}

/// A `Host` header without its `:port`, handling a bracketed IPv6 literal.
///
/// One implementation, in [`crate::hostmap`], because `host_matches` below and that module's
/// lookup have to normalise a header the same way — a port stripped in one and not the other
/// would make a route and a site disagree about the same request.
use crate::hostmap::strip_port;

impl Config {
    /// The backend that should serve this request, if a route matches it.
    ///
    /// The most specific matching route wins, and ties are broken by declaration
    /// order so the earlier entry wins. "Most specific" is stated rather than
    /// weighted, because a table whose winner could not be predicted by reading
    /// the file would be worse than one that is merely simple:
    ///
    /// 1. A route stating more constraints beats one stating fewer, so
    ///    `host + path_suffix` beats a bare `path_suffix`.
    /// 2. Among routes stating the same number, an exact `host` beats a `*.`
    ///    wildcard, since an exact name is the more specific claim about the same
    ///    request.
    /// 3. Anything still tied is settled by declaration order.
    pub fn route_backend(&self, host: Option<&str>, path: &str) -> Option<&str> {
        select_backend(&self.server.routes, host, path)
    }
}
