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

    /// The Prometheus endpoint. Absent means no metrics endpoint at all.
    ///
    /// **The presence of this table is what turns it on**, matching `[server.tls]`: an `enabled`
    /// flag and an address are two statements about one thing, and two statements can disagree — a
    /// table with `enabled = true` and no address has to be reported as a mistake, which is a
    /// question nobody would have asked without the flag.
    #[serde(default)]
    pub metrics: Option<MetricsConfig>,

    /// The health and readiness endpoint. Absent means no status endpoint.
    ///
    /// The same convention as [`Config::metrics`], and separate from it on purpose: a deployment
    /// that wants a container orchestrator to poll readiness may well not want a second port
    /// exposing every counter in the process.
    #[serde(default)]
    pub status: Option<StatusConfig>,

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
}
