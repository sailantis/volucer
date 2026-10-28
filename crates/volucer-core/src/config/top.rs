//! The configuration as a whole: the `Config` type, and how a file becomes one.

use super::*;

/// Top level configuration, deserialized from `volucer.toml`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub server: ServerConfig,

    /// Named backends. A route (or the default) refers to one of these.
    #[serde(default)]
    pub backends: BTreeMap<String, BackendConfig>,

    /// The pre-0.3 spelling of [`ServerConfig::default_backend`].
    ///
    /// Accepted by the parser and refused by [`Config::validate`] with a sentence naming the new
    /// location. Without it an upgrading operator reads `unknown field 'default_backend'` from
    /// `toml`, which is true and explains nothing: the key did not disappear, it moved one table
    /// down. Removed in the release after this one.
    ///
    /// **Never serialized.** It exists to be read once and refused, and a shim that appeared in
    /// `config.describe` would tell a worker the deployment has a `default_backend` at the top
    /// level — a claim that is only true for a file the server will not start with.
    #[serde(default, rename = "default_backend", skip_serializing)]
    pub legacy_default_backend: Option<String>,

    /// The pre-0.3 spelling of [`ServerConfig::routes`]. See [`Config::legacy_default_backend`].
    #[serde(default, rename = "routes", skip_serializing)]
    pub legacy_routes: Option<Vec<RouteConfig>>,

    /// Sites, by name. Empty means one site: what `[server]` describes.
    ///
    /// A site exists because a `host` in the route table is matched *after*
    /// `SCRIPT_FILENAME` is resolved, so a route can choose a backend but never
    /// a document root. Hosting two applications in one process therefore needs
    /// the decision made earlier, and that is what this table is for.
    #[serde(default)]
    pub sites: BTreeMap<String, SiteConfig>,

    /// The status listener: readiness, the Prometheus exposition, and the worker-control routes.
    /// Absent means no listener and none of those endpoints.
    ///
    /// **One table, because it is one socket.** It replaced a `[metrics]` table and a `[status]`
    /// table that each named an address for the same listener, which is two settings for one fact —
    /// and the server could only reconcile them by refusing a file where they disagreed. Each
    /// endpoint now exists because its *sub-table* does, which is the same presence-as-enablement
    /// rule `[server.tls]` follows.
    #[serde(default)]
    pub status: Option<StatusConfig>,

    /// The pre-0.4 spelling of [`StatusConfig::metrics`]. See [`Config::legacy_default_backend`].
    ///
    /// Refused by name rather than by `toml`'s `unknown field 'metrics'`, because the setting did
    /// not disappear — it moved one table down and its `address` is now the listener's `listen`.
    #[serde(default, rename = "metrics", skip_serializing)]
    pub legacy_metrics: Option<toml::Value>,

    /// Key-value storages, by name. Absent means none, and a worker cannot then call `kv.*` at all.
    ///
    /// **Named, because one deployment wants several.** A session cache and a fragment cache have
    /// different lifetimes, different bounds and sometimes different backends, and a single global
    /// store would make `clear` — which is per storage, see [`volucer_kv`'s own documentation] —
    /// unable to express that. RoadRunner has the same shape, and this is one of the places
    /// matching it costs nothing.
    ///
    /// A table rather than a list, so that a backend's `services` list can be told *which* storage
    /// it may reach; see [`Config::validate`] for where that is checked.
    #[serde(default)]
    pub kv: BTreeMap<String, KvStorageConfig>,

    /// The resource locks, or absent for none.
    ///
    /// **One table where KV has many**, and that is the difference between the two features rather than
    /// an inconsistency. A KV storage is *named* because a deployment wants several with different
    /// lifetimes and backends, and a worker has to say which one it is calling. A lock backend is a
    /// single mechanism: a lock is named by its *resource*, so two lock tables would be two independent
    /// views of one namespace — two processes locked on `"pdf:create"` through two tables would not see
    /// each other, which is a bug that is invisible in configuration and appears in production.
    ///
    /// The consequence for the call plane is the same as KV's: a backend reaches it by naming the
    /// namespace in `services`, and there is exactly one name to write.
    #[serde(default)]
    pub lock: Option<LockConfig>,

    /// The queues: which pipelines exist, and which of them this server consumes.
    ///
    /// Absent means no queues at all, and then a worker calling `jobs.*` is refused — the same rule every
    /// other service follows.
    #[serde(default)]
    pub jobs: Option<JobsConfig>,

    /// Where spans are exported. Absent means none are, which is the default.
    ///
    /// The same convention as [`Config::metrics`]: the presence of the table is what turns it on. A
    /// `[metrics]` table and a `[telemetry]` table are independent, and a deployment may well have
    /// one without the other — a Prometheus scrape costs one HTTP request a minute, while a trace
    /// exporter sends a request per batch for as long as traffic lasts.
    ///
    /// **`endpoint` is required**, because there is no default collector address that is right for
    /// anyone: `localhost:4318` is a guess that silently exports nothing on a host where the
    /// collector runs elsewhere, and a table whose only key can be defaulted would be a table whose
    /// presence already says everything.
    #[serde(default)]
    pub telemetry: Option<TelemetryConfig>,

    /// The call plane for FastCGI applications. Absent means no listener, and then a backend's
    /// `services` list reaches its workers' channel only.
    ///
    /// **A third listener beside `[metrics]` and `[status]`, deliberately not a route on
    /// `[server]`.** What travels here mutates: `kv.set`, `lock.release`, `jobs.push`,
    /// `workers.reset`. The public listener may be TLS-only, may redirect to HTTPS, and faces the
    /// internet, so a mutation surface on it would be one misconfiguration away from being public —
    /// which is exactly the argument `[metrics]` already makes for its own address, with a stronger
    /// conclusion.
    ///
    /// Unlike the other two, this table alone is not enough: a backend also has to grant a
    /// namespace in its `services`, or the socket would refuse every call. [`Config::validate`]
    /// refuses that combination by name.
    #[serde(default)]
    pub rpc: Option<RpcConfig>,
}

impl Config {
    /// Parse configuration from TOML text, without expanding `${...}`.
    ///
    /// **Deliberately environment-free**, and that is what makes it usable from a test: the
    /// process environment is global, so a parser that read it would make every configuration
    /// fixture depend on the machine it runs on. [`Config::load`] is the entry point that expands
    /// first — one place, so `serve`, `check` and a `SIGHUP` reload cannot disagree about what the
    /// file says.
    pub fn from_toml(text: &str) -> Result<Self> {
        Ok(toml::from_str(text)?)
    }

    /// Parse a configuration from an already-built TOML table.
    ///
    /// **The entry point a merged configuration uses**, and what makes the command line a second
    /// source rather than a second parser: [`Self::from_toml`] is this function after a parse, so
    /// a file, an `-o` pair and a reload are checked, defaulted and refused by exactly one
    /// deserializer. `deny_unknown_fields` is what that buys — an `-o server.htpp=127.0.0.1:8080`
    /// is a startup error naming the key, exactly as the same typo in a file is, rather than a
    /// setting that parses, is ignored, and looks supported.
    ///
    /// A *type* error here carries no line number, and that is the honest answer rather than a
    /// missing one: a value written on a command line has no line to point at. [`Self::load`] keeps
    /// the file's own spanned message for the case where nothing was overridden.
    pub fn from_table(table: toml::Table) -> Result<Self> {
        Ok(toml::Value::Table(table).try_into()?)
    }

    /// Read and expand a configuration file into its raw table.
    ///
    /// The mergeable form of a file, for a caller that has overrides to lay on top of it. Kept
    /// beside [`Self::load`] so the read and the expansion happen in one place: two reads with two
    /// `${...}` rules is how `serve` and a reload would come to disagree about the same file.
    pub fn table_from_path(path: &Path) -> Result<toml::Table> {
        Ok(toml::from_str(&Self::expanded_text(path)?)?)
    }

    /// The file's text with `${...}` expanded, still as TOML.
    ///
    /// Expansion happens on the **text**, before parsing, so `${...}` works in a number, port or
    /// listener position as well as in a string — see [`crate::env`] for why that is the only form
    /// that can. A reference to an unset variable with no `${NAME:-default}` is an error rather
    /// than an empty string.
    fn expanded_text(path: &Path) -> Result<String> {
        let text = std::fs::read_to_string(path).map_err(|source| Error::Read {
            path: path.to_path_buf(),
            source,
        })?;
        crate::env::expand(&text).map_err(Error::Config)
    }

    /// Read, expand and parse a configuration file.
    pub fn load(path: &Path) -> Result<Self> {
        Self::from_toml(&Self::expanded_text(path)?)
    }

    /// The backend the unnamed site falls back to, given the configured default; else `None`.
    ///
    /// This is the **single** place the rule lives, written once so that `validate`, `check`,
    /// `Dispatcher` and site resolution cannot disagree about it. They each ask this question, and
    /// four copies of it would be four chances to answer differently — a default applied on one path
    /// and not another.
    ///
    /// Four answers, in this order:
    ///
    /// 1. **A backend name**, which always wins. If a deployment has a backend *named* `worker`, then
    ///    `default_backend = "worker"` means that backend — the kind fallback below must not be able
    ///    to shadow a name, or introducing it would have silently changed an existing configuration.
    /// 2. **A backend kind** — `default_backend = "worker"` when exactly one backend is of that kind.
    ///    This is the useful multi-backend spelling: a deployment with `app` (a worker) and `assets`
    ///    (a `spawn_cgi` pool) says which kind serves unrouted requests instead of which name, and it
    ///    keeps working after a pool is renamed.
    /// 3. **A lone backend**, whatever it is called: with one backend there is nothing to choose.
    /// 4. **`None`**, which `Config::validate` turns into a refusal naming the candidates.
    ///
    /// Neither 2 nor 3 is order-dependent. [`Config::backends`] is a `BTreeMap`, so file order does
    /// not survive parsing, and a first-one-wins rule would resolve alphabetically — handing
    /// unrouted requests to `assets` because it sorts before `app`. "Exactly one of them" has no
    /// order to get wrong, and an ambiguous kind selects nothing rather than picking.
    pub fn implied_backend<'a>(&'a self, stated: Option<&'a str>) -> Option<&'a str> {
        let Some(stated) = stated else {
            return match self.backends.len() {
                1 => self.backends.keys().next().map(String::as_str),
                _ => None,
            };
        };

        if self.backends.contains_key(stated) {
            return Some(stated);
        }
        // Not a name — so it may be a kind. Resolved only when exactly one backend is of it, which
        // is what keeps this a statement about the file rather than a guess: two workers and a
        // `default_backend = "worker"` is ambiguous, and validation says so.
        if BackendConfig::is_kind(stated) {
            return self.lone_backend_of_kind(stated);
        }
        // Neither. Returned as written so that validation can report the name the operator typed
        // rather than losing it, and report it as an unknown *name*, which is what they wrote.
        Some(stated)
    }

    /// The only backend of `kind`, accepting either `spawn_cgi` or `spawn-cgi`.
    fn lone_backend_of_kind(&self, kind: &str) -> Option<&str> {
        let wanted = kind.replace('-', "_").to_ascii_lowercase();
        let mut matching = self
            .backends
            .iter()
            .filter(|(_, backend)| backend.kind() == wanted);
        match (matching.next(), matching.next()) {
            (Some((name, _)), None) => Some(name.as_str()),
            _ => None,
        }
    }

    /// How many backends are of `kind`, for the messages that explain why a kind selected nothing.
    pub fn count_of_kind(&self, kind: &str) -> usize {
        let wanted = kind.replace('-', "_").to_ascii_lowercase();
        self.backends
            .values()
            .filter(|backend| backend.kind() == wanted)
            .count()
    }

    /// Whether the default in force was inferred rather than written, and what it is.
    ///
    /// Exists for `volucer check`, which prints a line describing where the default came from. An
    /// operator reading a file with no `default_backend` should be told the server chose one for
    /// them, rather than left to infer it from the absence of an error. A kind that resolved to a
    /// name is *not* reported here: the operator wrote the key, so there is nothing they have not
    /// said — `check` shows the resolved name on the backend's own line instead.
    pub fn inferred_default_backend(&self) -> Option<&str> {
        match self.server.default_backend {
            None => self.implied_backend(None),
            Some(_) => None,
        }
    }
}
