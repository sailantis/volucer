//! `[server]`: the listeners, and the request-facing tables beneath them.

use super::*;

/// Server-wide settings: the listeners, plus how a request path becomes a script.
///
/// Both listeners are optional, but at least one must be configured.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    /// HTTP listener, e.g. `127.0.0.1:8080` or `unix:/run/volucer/http.sock`.
    ///
    /// With no `[server.tls]` this listener is plaintext. With `[server.tls]` present and
    /// no `https`, TLS wraps *this* listener, and `[server.tls].plain` decides whether it
    /// also answers cleartext on the same port.
    ///
    /// **A unix socket is a real listener, not a spelling this rejects.** It is the shape a
    /// deployment behind `nginx` uses (`proxy_pass http://unix:/run/volucer/http.sock:/;`), and it
    /// has three consequences worth knowing: there is no port, so `SERVER_PORT` falls back to the
    /// request's `Host` or to `80`; there is no peer address, so `REMOTE_ADDR` is omitted and an
    /// IP-keyed rate limit does not apply; and `redirect_to_https` has nowhere to send a client, so
    /// `validate` refuses that combination rather than emitting a `Location` that cannot work.
    #[serde(default)]
    pub http: Option<String>,

    /// HTTPS listener, e.g. `0.0.0.0:443`.
    ///
    /// A second, independently bound listener that speaks TLS only. Setting it frees
    /// `http` to stay plaintext, which is what an `:80` and `:443` deployment needs — and
    /// what ACME's HTTP-01 challenge and an `:80` → `:443` redirect need, because those
    /// are the only parts of TLS that genuinely require a particular port.
    ///
    /// Requires `[server.tls]`: a TLS listener with no certificate is an error, not a
    /// listener that fails every handshake. Setting it together with `[server.tls].plain`
    /// is also an error — the port is already dedicated, so the same-port sniff would have
    /// nothing to decide.
    ///
    /// **TCP only.** `unix:` is refused here by name, because this listener's job includes naming
    /// the port an `http` redirect points at, and a socket has no port to name.
    #[serde(default)]
    pub https: Option<String>,

    /// FastCGI listener, e.g. `127.0.0.1:9000` or `unix:/run/volucer.sock`.
    ///
    /// **The unix spelling is the one an `nginx` deployment wants**, since its `fastcgi_pass` takes
    /// either form and a socket avoids a loopback round trip. A socket has no peer address, so
    /// `REMOTE_ADDR` is not reported for the connection — but on this front-end the CGI parameter of
    /// that name is nginx's to set and is relayed verbatim, so the request's identity is unaffected.
    #[serde(default)]
    pub fastcgi: Option<String>,

    /// How responses this server produces are compressed.
    ///
    /// Off by default, which is a deliberate inversion of nginx's: compressing a backend's output
    /// costs CPU on the request path of a server designed around one request at a time per worker
    /// slot, so a deployment opts in knowing that rather than discovering it. Precompressed
    /// static variants are a different mechanism and unaffected by this key.
    #[serde(default)]
    pub compression: Option<CompressionSetting>,

    /// The largest request body either front-end will accept, in bytes.
    ///
    /// Defaults to 32 MiB, which is what was hard-coded here before this key existed, so a
    /// deployment that says nothing keeps its behaviour. The **unit is in the name**, following
    /// [`max_multirange_bytes`](StaticConfig::max_multirange_bytes) and `memory_limit_mb`: RoadRunner
    /// spells this `http.max_request_size` and counts **megabytes**, and a key with the same name and
    /// a different unit is the one kind of compatibility that costs more than it buys.
    ///
    /// It bounds the **body**, not the headers: a header block is already bounded by the protocol
    /// layer, and the body is the part a client chooses freely. Both front-ends apply it, because
    /// both buffer the body before a backend sees it — the CGI environment has to carry
    /// `CONTENT_LENGTH` before `php-cgi` reads a byte, so there is no stream to pass through. That is
    /// also why it is a *refusal* rather than a back-pressure valve.
    ///
    /// **It deliberately does not bound an `upstream` backend**, whose request body streams to the
    /// origin unread. That is a documented decision rather than an omission — a reverse proxy
    /// silently imposing a limit its own client never set is a bug report waiting — and the limit to
    /// configure for one is the origin's. See the README, *Backends*.
    ///
    /// Zero is refused rather than read as "unlimited": a zero here refuses every request with a
    /// body, which is never what was meant, and the same rule already governs
    /// `static_files.max_multirange_bytes`.
    #[serde(default)]
    pub max_body_bytes: Option<usize>,

    /// How long a stop signal may spend finishing requests that are already in flight.
    ///
    /// A stop is not an abort: the listener stops accepting immediately, then lets each open
    /// connection complete the request it is serving before closing. This is the budget for that
    /// grace period. When it expires, whatever is still in flight is abandoned and the count is
    /// logged at `warn`, so an operator can tell "everything finished" from "three requests were
    /// dropped" — the distinction the previous behaviour could not make at all, because it dropped
    /// every one of them without saying so.
    ///
    /// **This must be shorter than whatever the process manager waits before killing.** A
    /// `systemd` `TimeoutStopSec` or a container's `stop_grace_period` that expires first means
    /// `SIGKILL` lands mid-drain, which produces exactly the truncated responses this key exists to
    /// prevent. The default is thirty seconds, which is the value `docker stop` and
    /// `systemd`'s `DefaultTimeoutStopSec` both use — so an operator who has not changed either
    /// gets a drain that completes.
    ///
    /// `0` is accepted and means "do not wait at all": a deployment that would rather recycle
    /// instantly than hold a supervisor for thirty seconds. It is a deliberate setting rather than
    /// a mistake, so it is not refused the way `max_body_bytes = 0` is.
    #[serde(default)]
    pub shutdown_timeout_ms: Option<u64>,

    /// Maximum concurrent client connections the HTTP front-end accepts.
    ///
    /// **A key as well as a flag**, so every other limit in this file has one too. It used to be
    /// reachable only as `--max-connections`, which is the single inconsistency of its kind: a
    /// deployment driven by a systemd unit and a configuration file cannot express it, and an
    /// operator reading `[server]` has no way to learn it exists.
    ///
    /// `0` means "no ceiling", which is the honest spelling of unlimited rather than a number large
    /// enough to look like one. The flag still wins, so a unit that overrides it keeps working.
    #[serde(default)]
    pub max_connections: Option<usize>,

    /// Responses this server generates for an error status, by status code.
    ///
    /// **A table per code rather than one template**, because the pages an operator writes are not
    /// interchangeable: a `404` wants the site's navigation and a `503` wants "we are restarting,
    /// try in a moment". One template for every error is a page that reads wrong for most of them.
    ///
    /// Keyed by the code, so an unknown key is a typo rather than a page nothing serves. A code the
    /// server never generates is accepted — an operator may write a `418` page for an application
    /// that returns one through an `upstream` — and the table is consulted for *every* error status
    /// this server answers, which is the only way a page for a status the front-end did not itself
    /// produce can ever be reached.
    ///
    /// **Only the bodies of `text/html` responses are replaced.** A `403` on a gRPC call, a `429` on
    /// a JSON API and a `404` on a `HEAD` are all cases where the body is either not HTML or not
    /// sent at all, and swapping a page in would break the client that asked. See
    /// [`ErrorPages::body_for`].
    #[serde(default)]
    pub error_pages: ErrorPages,

    /// Directory that PHP scripts and static files are resolved against.
    ///
    /// A relative path is relative to the process's working directory, not to
    /// this file — the same rule [`WorkerConfig::working_dir`] follows. Prefer
    /// an absolute path: a deployment that sets its working directory elsewhere
    /// would otherwise resolve somewhere else silently. `volucer check` prints
    /// the path it actually resolved.
    ///
    /// Unset means the `serve` command line decides, and failing that `.`.
    #[serde(default)]
    pub document_root: Option<PathBuf>,

    /// Front controller, relative to the document root, e.g. `index.php`.
    ///
    /// This is what makes a front controller work: a path naming no file under
    /// the document root is given to this script with `PATH_INFO` set, instead
    /// of answering 404.
    ///
    /// Set it to the empty string to serve only real `.php` files, matching
    /// nginx's `location ~ \.php$`. Unset means the `serve` command line
    /// decides, and failing that `index.php`.
    #[serde(default)]
    pub front_controller: Option<String>,

    /// Which extensions name a script a backend is asked to execute.
    ///
    /// Defaults to `["php", "php3", "phtml"]`, which is what the server has
    /// always accepted. Widening it is what makes a non-PHP worker reachable at
    /// all: `path_suffix` can *route* `/app.py`, but this list is what decides
    /// that `/app.py` resolves as a script rather than arriving at the front
    /// controller as `PATH_INFO`.
    ///
    /// **It is deliberately one list rather than two.** The same answer is used
    /// by the router (what to hand a backend), by the static layer (what must
    /// never be served from disk, because that discloses source) and by
    /// `skip_stat` (whether a route has already proved its path names a script).
    /// Widening the router alone would turn every newly-routable extension into
    /// a source disclosure, so the three move together by construction.
    ///
    /// A leading dot is accepted (`".php"` and `"php"` are the same entry) and
    /// the comparison is case-insensitive, matching PHP's own behaviour. An
    /// empty list is refused: it would silently send every script path to the
    /// front controller as `PATH_INFO`. See [`crate::script`].
    #[serde(default)]
    pub script_extensions: Option<Vec<String>>,

    /// Serving files from the document root, without a backend.
    ///
    /// A table tunes the layer; `false` turns it off and `true` turns it on with the
    /// defaults. Off by default, so an existing deployment is not changed by
    /// upgrading.
    #[serde(default)]
    pub static_files: StaticsSetting,

    /// What this deployment serves; see [`ServeOnly`]. Defaults to everything.
    #[serde(default)]
    pub serve_only: ServeOnly,

    /// The backend a request that matches no route is answered by.
    ///
    /// It lives here rather than at the top level because `[server]` **is** the unnamed site — see
    /// [`Config::sites`] — and this is a site setting, exactly like `document_root` and
    /// `front_controller`. `[sites.<name>]` overrides it for one host, and the inheritance runs in
    /// the same direction as every other per-site key: the more specific table wins.
    ///
    /// Required once `[backends]` is non-empty, and refused when it names no configured backend:
    /// backends nothing can select are a typo, and a name that matches nothing is a configuration
    /// that parses and then fails every request.
    #[serde(default)]
    pub default_backend: Option<String>,

    /// The route table, in the order written. See [`RouteConfig`] for the precedence rule.
    ///
    /// Here for the reason [`ServerConfig::default_backend`] is: routing is what the unnamed site
    /// does, not what the process does. A site's own `routes` **replaces** this list rather than
    /// merging with it, so a host that states one route does not inherit the other twenty.
    #[serde(default)]
    pub routes: Vec<RouteConfig>,

    /// Environment variables every process this deployment starts inherits.
    ///
    /// **A baseline, not a replacement.** A backend's own `env` is merged *over* this one, so the
    /// common case — one `APP_ENV` and one `DATABASE_URL` shared by a worker pool, a job consumer
    /// and a supervised origin — is written once instead of three times, and the exception is still
    /// expressible per backend. The order is `inherited → this table → the backend's own env`, which
    /// is the resolution every other volucer key already follows: the more specific statement wins.
    ///
    /// **Names are passed exactly as written**, unlike RoadRunner's `server.env`, which uppercases
    /// them. A server that silently changed a variable's name would leave an application reading
    /// `app_env` from a file that says `APP_ENV`, and the per-backend `env` here has never done it.
    ///
    /// **It does not reach `attach`**: volucer starts those processes not at all, and a key that
    /// described a process nobody starts is the shape [`UpstreamConfig::command`] already refuses.
    #[serde(default)]
    pub env: BTreeMap<String, String>,

    /// TLS settings. **Presence of this table is what turns TLS on.**
    ///
    /// No separate `enabled` boolean, deliberately: a flag and a certificate are two
    /// statements about one thing, and they can disagree. A table without both a
    /// certificate and a key is rejected instead.
    #[serde(default)]
    pub tls: Option<TlsConfig>,

    /// Certificate issuance. **Presence of this table is what turns ACME on.**
    ///
    /// Opt-in *in addition* to `[server.tls]`, which stays required: a deployment with no ACME
    /// behaves exactly as it did before this key existed. See [`AcmeConfig`] for why a fallback
    /// certificate is not optional.
    #[serde(default)]
    pub acme: Option<AcmeConfig>,

    /// HTTP/3 over QUIC, on its own UDP listener. **Presence of this table is what turns it on.**
    ///
    /// Absent by default, and absent is the same behaviour as every release before this key
    /// existed: `hyper` has no HTTP/3, so a deployment that says nothing serves HTTP/1.1 and
    /// HTTP/2 exactly as it did.
    ///
    /// **It needs a build with the `http3` feature.** The key parses either way, and a build
    /// without the feature refuses it *by name* rather than reporting an unknown field — see
    /// [`Http3Config`] and `validate_http3` for why that distinction is worth a field.
    #[serde(default)]
    pub http3: Option<Http3Config>,

    /// Answer cleartext requests on `http` with a `301` to the `https://` equivalent.
    ///
    /// Off by default: it changes what an existing cleartext deployment does, and a
    /// redirect is a deployment's decision rather than a server's. Only meaningful when
    /// there is a plaintext listener to redirect *from* — with TLS on `http` and
    /// `plain = false`, that listener only ever answers TLS, so setting this is an error
    /// rather than a silently inert key.
    #[serde(default)]
    pub redirect_to_https: bool,

    /// `Strict-Transport-Security` for every site that does not state its own.
    ///
    /// Absent is the default, and it means **no header**: adding one changes what an existing
    /// deployment's browsers do, and a policy is a deployment's decision rather than a server's —
    /// the same rule `redirect_to_https` follows.
    ///
    /// `max_age_s` is required here because there is nothing to inherit from; a site's table may
    /// leave it out and inherit this one. See [`HstsConfig`] for what the policy cannot reach and
    /// where the header is sent.
    #[serde(default)]
    pub hsts: Option<HstsConfig>,

    /// The access log. **Absent is what turns it off**, which is the default.
    ///
    /// Off by default on the same rule `static_files` follows: an existing deployment must not
    /// be changed by upgrading. A table rather than a boolean because a path is required for it
    /// to mean anything, and a bare `true` with no destination is the kind of two-statements-one-
    /// thing the `[server.tls]` doc already refuses.
    ///
    /// This is deliberately **not** the diagnostic log. See `volucer-observe`'s `access_log`
    /// module: an access log that shared a verbosity filter with diagnostics would be one
    /// setting away from silence, and both directions are wrong — debugging a worker loud
    /// floods it, and quieting the diagnostics erases the record of who requested what.
    #[serde(default)]
    pub access_log: Option<AccessLogConfig>,

    /// Where the client's address comes from when a reverse proxy is in front.
    ///
    /// **Absent is the default and means the peer address**, which is what every existing
    /// deployment already gets. The table exists because turning it on is a *trust* statement —
    /// `X-Forwarded-For` is a request header, so believing it from an arbitrary client is worse
    /// than ignoring it — and a trust statement should have to be written down and named.
    #[serde(default)]
    pub proxy: Option<ProxyConfig>,

    /// Refuse callers over a configured rate. **Absent is what turns it off**, and that is the
    /// default.
    ///
    /// The same convention as [`Config::metrics`] and [`ServerConfig::tls`]: the presence of the
    /// table is the enable switch, because an `enabled` flag beside a `rate` is two statements
    /// about one thing and they can disagree.
    ///
    /// It is keyed on the identity `[server.proxy]` resolves, which is the whole reason that table
    /// was built first: behind nginx a limit keyed on the peer throttles the proxy and cannot tell
    /// two clients apart at all. See [`Self::rate_limiter`].
    #[serde(default)]
    pub rate_limit: Option<RateLimitConfig>,

    /// Which client addresses may reach this server at all. **Absent is what turns it off.**
    ///
    /// The same presence-is-the-switch convention as [`Self::rate_limit`] and [`Self::proxy`]. It is
    /// consulted **first**, above the rate limit, the redirect and the site table, because its job is
    /// to stop a request costing anything — see [`AccessConfig`] for the ordering rule and for why
    /// the default admits rather than refuses.
    #[serde(default)]
    pub access: Option<AccessConfig>,

    /// Let a backend serve a file by naming it, instead of by producing its bytes.
    ///
    /// **Absent is what turns it off**, and absent is the default for a reason worth stating: this
    /// header tells the file layer to read a path the application chose, so a table that existed and
    /// defaulted to something would be one bug away from disclosing the application's own sources.
    /// Naming roots is a statement that the files under them are publishable.
    ///
    /// See [`SendfileConfig`] and `volucer_core::sendfile` for what the allow-list does and why it is
    /// not `document_root`.
    #[serde(default)]
    pub sendfile: Option<crate::sendfile::SendfileConfig>,

    /// What happens to the W3C `traceparent` on the way to a backend. Defaults to `extract`.
    ///
    /// # Why this is here rather than in `[telemetry]`
    ///
    /// It decides **what a backend receives**, which is a fact about the request path; `[telemetry]`
    /// is where finished spans *go*, and it requires an `endpoint` because an exporter with nowhere
    /// to send is not a feature. The two are therefore independent, and the shape that needs this
    /// key most is the one that configures no `[telemetry]` at all: nginx in front doing the
    /// exporting, and the application's own collector behind it. A key that existed only inside
    /// `[telemetry]` could not be written in that deployment.
    ///
    /// # The four values, and why `extract` is the default
    ///
    /// `ignore`, `extract`, `inject` and `propagate` are nginx's `otel_trace_context`, spelled the
    /// same and meaning the same — see [`PropagationMode`] for the table. `extract` is the default
    /// because it is what this server has always done: a client's `traceparent` reaches the backend
    /// untouched, and Volucer still continues the trace it was given. `propagate` **changes what a
    /// backend receives** — the client's header is replaced with one naming this server's span —
    /// so it is a deployment's statement rather than an upgrade's, which is the same rule
    /// `redirect_to_https` and `hsts` follow.
    #[serde(default)]
    pub trace_context: PropagationMode,
}

/// One rate-limit policy, applied by every front-end.
///
/// The names are RoadRunner's (`http.rate_limiter`) with the same three key modes, because an
/// operator who has written an `rr.yaml` block already knows what they mean. Two things depart from
/// it deliberately, and both are in [`ServerConfig::rate_limiter`]'s documentation: `burst` defaults
/// to `rate` rather than to `1`, and a full table answers a 429 rather than a 503.
///
/// # One policy per process, which is a limitation rather than a design
///
/// Route-level and per-site limits are the obvious next question, and the answer is no for a
/// reason worth recording: a bucket's identity is (who, *which limit*), so two policies over one
/// client are two buckets, and a route table would need the policy's name in the key. RoadRunner
/// has the same restriction and states it. A deployment that needs several limits puts a second
/// proxy in front, or waits for this to grow a route table.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RateLimitConfig {
    /// Who a bucket belongs to: `"ip"` (the default), `"header"`, or `"global"`.
    #[serde(default)]
    pub key: KeyMode,

    /// The header whose value names the caller, when `key = "header"`.
    ///
    /// Required in that mode and refused in the other two: a header named beside a key that does
    /// not read one is a line that believes it limits that header, and RoadRunner's own schema
    /// makes the same call.
    ///
    /// **Not an authenticated identity.** A client chooses the value, so a client that varies it
    /// gets one bucket per request until the table fills. Its use is a value the *proxy* sets — a
    /// tenant it routed, a key it vouched for.
    #[serde(default)]
    pub header: Option<String>,

    /// Requests allowed per `interval`. Required, because a rate limit with no rate is a table
    /// that appears to protect something and does not.
    pub rate: u32,

    /// How long `rate` requests take to accrue, in milliseconds.
    ///
    /// Named `interval_ms` rather than RoadRunner's `interval` (a Go duration string), following
    /// `[telemetry].interval_ms` and `[backends.pool].ready_timeout_ms`: a number with its unit in
    /// the key cannot be misread, and a duration syntax is a parser this file does not have.
    #[serde(default = "default_rate_interval_ms")]
    pub interval_ms: u64,

    /// How many requests may be spent at once, and how many a bucket holds.
    ///
    /// **Defaults to `rate`, not to 1.** RoadRunner defaults it to one, which turns
    /// `rate = 60, interval_ms = 60000` into "one request, then one per second" — a limit nobody
    /// writing that line meant. Defaulting to `rate` is the ordinary reading of "60 requests a
    /// minute": the allowance may be spent at once and it returns over the minute.
    #[serde(default)]
    pub burst: Option<u32>,

    /// How many identities one process tracks before it stops admitting new ones.
    ///
    /// Defaults to [`DEFAULT_MAX_ENTRIES`]. A full table that is only full of *depleted* buckets
    /// refuses the newcomer with a 429 rather than evicting somebody's allowance; see
    /// [`volucer_core::ratelimit`](crate::ratelimit) for why that is the lossless direction.
    #[serde(default)]
    pub max_entries: Option<usize>,
}

fn default_rate_interval_ms() -> u64 {
    1000
}

/// Which addresses may reach this server, as an ordered rule list.
///
/// ```toml
/// [server.access]
/// default = "allow"          # what an address no rule matched gets; the default
/// rules = [
///   { action = "allow", range = "192.0.2.7" },      # except one host...
///   { action = "deny",  range = "192.0.2.0/24" },   # ...from this scraped range
/// ]
/// ```
///
/// # Why one ordered list and not `allow` / `deny`
///
/// Two lists cannot express an order *between* them, and **the order is the security property**:
/// the first matching rule decides. See [`volucer_core::access`](crate::access) for why
/// most-specific-wins — the rule the host table uses — is the wrong answer here, and for why an
/// address that matches nothing is admitted by default.
///
/// **An exception goes above the rule it excepts from.** A broad range listed first claims every
/// address inside it, so a narrower rule written after it is never reached — `check` prints the list
/// in the order it will be evaluated so this can be read rather than discovered.
///
/// # Why this is keyed on the resolved client, not the socket peer
///
/// The address tested is the one `[server.proxy]` resolves, which is the same identity the rate
/// limiter and the access log use. Behind nginx the socket peer is nginx, so a list applied to it
/// would either block the proxy or block nobody — the list has to see what the proxy saw.
///
/// **That makes a trusted `X-Forwarded-For` an admission decision**, which is why it is
/// `trusted_subnets` that has to name the proxy and not this table: a client that can set the header
/// itself can claim any address, including an allowed one. The two tables are a pair and neither is
/// safe alone; the documentation says so rather than leaving it to be worked out.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccessConfig {
    /// What an address that matches no rule gets: `"allow"` (the default) or `"deny"`.
    ///
    /// Defaults to `"allow"` so a half-written list cannot take a deployment offline — the same
    /// rule nginx applies with its implicit `allow all`. A closed door is written
    /// `default = "deny"` plus the ranges to admit, and it is then impossible to arrive at by
    /// accident.
    ///
    /// **`"deny"` beside a listener with no peer address is refused by `validate`.** A unix socket
    /// has no address, so no rule can ever match it and every request would be refused — the
    /// deployment would have closed a door that has no lock on it, and the failure would look like
    /// a misconfigured rule rather than a listener that cannot be filtered at all.
    #[serde(default)]
    pub default: crate::access::Action,

    /// The rules, evaluated top to bottom; the first match decides.
    ///
    /// Each entry is `{ action = "allow" | "deny", range = "10.0.0.0/8" }`. The range forms follow
    /// `[server.proxy].trusted_subnets` exactly — one parser, one set of rules, so `10.0.0.0/8`
    /// means the same thing in both places. A bare address is a range of one.
    ///
    /// An empty list is allowed and means "no rule can match, so `default` decides everything";
    /// `check` reports it as such rather than as an active policy.
    #[serde(default)]
    pub rules: Vec<AccessRuleConfig>,
}

/// One line of [`AccessConfig::rules`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccessRuleConfig {
    /// What this rule does with a matching request.
    pub action: crate::access::Action,

    /// The address or CIDR range this rule matches, e.g. `"10.0.0.0/8"` or `"127.0.0.1"`.
    pub range: String,
}

/// Which peers may assert a client address, and through which headers.
///
/// The defect this answers is invisible: behind nginx every request arrives from nginx, so the
/// access log records the proxy and anything keyed on the address — a rate limiter, an IP
/// allow-list — keys on the proxy rather than the client. Both look like correct behaviour until
/// two clients have to be told apart.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProxyConfig {
    /// Networks whose forwarded headers are believed, e.g. `["127.0.0.1", "10.0.0.0/8"]`.
    ///
    /// **An empty list means the feature is off, and an absent `[server.proxy]` is the same thing.**
    /// A bare address is a network of one, which is how a single local proxy is named. Every entry
    /// is parsed at load time: a range that does not parse is a startup failure rather than a
    /// silently dropped entry, because a trust list is one of the few settings where a typo is a
    /// security decision and a shortened list is not visible afterwards.
    ///
    /// The names are RoadRunner's, and deliberately so: `http.trusted_subnets` is what an operator
    /// migrating from RoadRunner — or an nginx config they are copying from — already has in mind.
    #[serde(default)]
    pub trusted_subnets: Vec<String>,

    /// The headers to consult, in order, when the peer is trusted.
    ///
    /// **The order is the security-relevant part**, not a preference: the first header that yields
    /// a parsable address wins. Defaults to `Forwarded`, then the `X-Forwarded-For` family, which is
    /// RoadRunner's order. `X-Forwarded-For` is appended to by every hop, so behind a CDN its first
    /// element is the *client's own claim* rather than the client's address — a deployment that
    /// knows its topology should name this list rather than rely on the default.
    ///
    /// A name that is blank or repeated is dropped rather than refused, and any name other than the
    /// two structured headers is read as a single address, which is what makes a CDN's own header
    /// usable without a code change.
    #[serde(default)]
    pub trusted_headers: Vec<String>,
}

/// The access log's destination and shape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccessLogConfig {
    /// Where records are appended. Created if it does not exist.
    ///
    /// A relative path resolves against **the directory holding the config file**, the same rule
    /// `[server.tls]`'s `certificate` follows and for the same reason: a log beside `volucer.toml`
    /// is the case an operator means, and resolving it against wherever the process happened to
    /// start is a footgun. `volucer check` prints the path it resolved.
    pub path: PathBuf,

    /// `"combined"` (nginx's shape plus two timings, the default) or `"json"`.
    ///
    /// Parsed strictly rather than defaulted on a typo: quietly choosing a format the operator did
    /// not ask for produces a log their queries do not match, which is worse than refusing to
    /// start.
    #[serde(default)]
    pub format: AccessLogFormat,

    /// The header an inbound request id is adopted from, and echoed back on.
    ///
    /// Defaults to `X-Request-Id`, and **adopting is the default** because "one id across the
    /// tiers" is the feature: a request id that changes at every hop cannot correlate a client's
    /// log with ours. Set it to the empty string to always mint a fresh one, which is what nginx
    /// does — `$request_id` is always its own. The value is validated as an identifier, and a
    /// client's malformed one is replaced rather than refused; see `volucer-observe`.
    #[serde(default = "default_request_id_header")]
    pub request_id_header: String,
}

/// The header a request id is adopted from, unless a deployment says otherwise.
fn default_request_id_header() -> String {
    "X-Request-Id".to_string()
}

/// How an access-log record is rendered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum AccessLogFormat {
    /// nginx's `combined` shape, with the request and origin timings appended.
    #[default]
    #[serde(rename = "combined")]
    Combined,
    /// One JSON object per line.
    #[serde(rename = "json")]
    Json,
}

/// The front controller used when neither the config nor the command line says.
pub const DEFAULT_FRONT_CONTROLLER: &str = "index.php";

/// The largest request body a backend will be handed, when configuration says nothing.
///
/// The bodies this server sends for an error status, by code.
///
/// A newtype over a `BTreeMap<u16, PathBuf>` rather than a bare map, so the two rules that make a
/// page safe — the code has to be a *client* or server error, and the path is resolved against the
/// configuration's directory — live with the type rather than at each lookup.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, transparent)]
pub struct ErrorPages {
    /// The page per status code, e.g. `502 = "pages/502.html"`.
    ///
    /// A TOML key must be a string, so the codes are written as strings: `"502" = "…"`. Parsed
    /// strictly, because `[server.error_pages.502]` is not a table and a file that wrote a table
    /// would otherwise be read as no pages at all.
    pages: std::collections::BTreeMap<String, PathBuf>,
}

impl ErrorPages {
    /// The resolved page for a status, if the deployment wrote one.
    ///
    /// `resolve` is applied here rather than at load time because the path is relative to the
    /// configuration's directory, which only the caller knows — the same rule every other path in
    /// the file follows.
    #[must_use]
    pub fn page(&self, status: u16) -> Option<&Path> {
        self.pages.get(&status.to_string()).map(PathBuf::as_path)
    }

    /// Whether the table is empty, which is the case an operator did not configure it at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.pages.is_empty()
    }

    /// The codes that have a page, sorted, for `check` to report.
    #[must_use]
    pub fn codes(&self) -> Vec<u16> {
        self.pages
            .keys()
            .filter_map(|code| code.parse().ok())
            .collect()
    }

    /// Refuse a key that is not a status code, and a code that cannot be an error status.
    pub fn validate(&self) -> std::result::Result<(), String> {
        for code in self.pages.keys() {
            let Ok(status) = code.parse::<u16>() else {
                return Err(format!(
                    "server.error_pages has the key {code:?}, which is not a status code; write                      it quoted, e.g. \"502\" = \"pages/502.html\""
                ));
            };
            // A 2xx or 3xx page would be a page nothing could ever reach, and accepting one is how
            // an operator concludes the feature does not work rather than that the code is wrong.
            if !(400..=599).contains(&status) {
                return Err(format!(
                    "server.error_pages names {status}, which is not an error status; only 400 to                      599 are ever generated as an error"
                ));
            }
        }
        Ok(())
    }
}

/// The value `HttpLimits::default()` used before [`ServerConfig::max_body_bytes`] existed, kept
/// deliberately: an unconfigured deployment must not change behaviour because the key was added.
pub const DEFAULT_MAX_BODY_BYTES: usize = 32 * 1024 * 1024;

/// How long a stop may spend finishing in-flight requests, when configuration says nothing.
///
/// Thirty seconds, which is the value `docker stop` sends and `systemd`'s
/// `DefaultTimeoutStopSec` allows. That coincidence is the point: an operator who has changed
/// neither gets a drain that completes before the supervisor escalates to `SIGKILL`, so the default
/// cannot cause the truncation the setting exists to prevent.
pub const DEFAULT_SHUTDOWN_TIMEOUT_MS: u64 = 30_000;

/// How many concurrent client connections the HTTP front-end accepts, when configuration says
/// nothing.
///
/// The value the `--max-connections` flag has always defaulted to, so promoting the limit into
/// `[server]` changes nothing for a deployment that does not write the key.
pub const DEFAULT_MAX_CONNECTIONS: usize = 512;

/// ALPN protocol names this server understands.
///
/// Checked by name rather than passed through, because an unknown identifier is accepted by
/// rustls and then never selected by anyone: the listener would look configured and answer
/// only whatever a client happened to offer. `h2` and `http/1.1` are the two it can actually
/// serve.
pub const KNOWN_ALPN: [&str; 2] = ["h2", "http/1.1"];

impl ServerConfig {
    /// The environment one started process receives: this server's baseline, overlaid by the
    /// process's own table.
    ///
    /// One function rather than a merge at each spawn site, because there are four of them —
    /// a worker pool, a consumer pool, `php-cgi` and a supervised origin — and a merge written
    /// four times is a merge that will be right in three of them. The precedence is the one
    /// [`ServerConfig::env`] documents: the process's own value wins.
    #[must_use]
    pub fn env_for(&self, own: &BTreeMap<String, String>) -> BTreeMap<String, String> {
        let mut merged = self.env.clone();
        merged.extend(own.clone());
        merged
    }

    /// The effective front controller, or `None` for "serve only real files".
    ///
    /// The command line wins over this file, because a flag is the more specific
    /// statement, and an empty value on either side means "no front controller".
    pub fn resolve_front_controller(&self, from_command_line: Option<&str>) -> Option<String> {
        let value = from_command_line
            .or(self.front_controller.as_deref())
            .unwrap_or(DEFAULT_FRONT_CONTROLLER);
        let value = value.trim();
        if value.is_empty() {
            None
        } else {
            Some(value.to_string())
        }
    }

    /// The static layer's settings, with any bare boolean resolved.
    ///
    /// The layer is **on** unless a deployment says otherwise, so an absent key resolves to
    /// `enabled = true`. Only a path that names a real, non-script file changes behaviour:
    /// a front-controller application answers every unmatched path exactly as it did when the
    /// default was off.
    pub fn statics(&self) -> StaticConfig {
        self.static_files.applied_to(&StaticConfig {
            enabled: true,
            ..StaticConfig::default()
        })
    }

    /// The script extensions in effect, with the default resolved.
    ///
    /// Unset means [`crate::script::DEFAULT_SCRIPT_EXTENSIONS`], which is what every release
    /// before this key accepted. [`Config::validate`] refuses a list this cannot parse — an empty
    /// one, or an entry naming a path — so falling back to the default here is only the shape a
    /// caller sees if it skipped validation, never a configuration being silently reinterpreted.
    /// The server's compression settings, defaulted when absent.
    ///
    /// A key that is unset means "off", not "on with defaults": the difference is the whole
    /// decision, so it is made here once rather than at each reader.
    pub fn compression(&self) -> CompressionSetting {
        self.compression.clone().unwrap_or_default()
    }

    pub fn script_extensions(&self) -> crate::script::ScriptExtensions {
        self.script_extensions
            .as_deref()
            .and_then(|list| {
                crate::script::ScriptExtensions::from_config(list, "server.script_extensions").ok()
            })
            .unwrap_or_default()
    }

    /// The largest body either front-end will accept, with the default resolved.
    ///
    /// Reads through to the one constant rather than repeating it, so the HTTP and FastCGI
    /// front-ends cannot disagree about the answer — the defect this key was added to fix was
    /// that only one of them had an answer at all.
    pub fn max_body_bytes(&self) -> usize {
        self.max_body_bytes.unwrap_or(DEFAULT_MAX_BODY_BYTES)
    }

    /// How long a stop may spend draining in-flight requests.
    ///
    /// Reads through to [`DEFAULT_SHUTDOWN_TIMEOUT_MS`], so the HTTP and FastCGI front-ends cannot
    /// disagree — the same rule [`Self::max_body_bytes`] follows, and for the same reason.
    pub fn shutdown_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_millis(
            self.shutdown_timeout_ms
                .unwrap_or(DEFAULT_SHUTDOWN_TIMEOUT_MS),
        )
    }

    /// How many concurrent client connections to accept, with the default resolved.
    ///
    /// `0` reads through as `0`, which the front-end treats as no ceiling. Resolving the default
    /// here rather than at the flag is what lets the file be the only source: a flag carrying a
    /// `default_value` is indistinguishable from one an operator typed.
    pub fn max_connections(&self) -> usize {
        self.max_connections.unwrap_or(DEFAULT_MAX_CONNECTIONS)
    }

    /// Whether a plaintext listener exists to redirect *from*.
    ///
    /// Two shapes have one: `http` + `https`, and TLS on `http` with `plain = true`. With
    /// TLS on `http` alone, that socket never answers cleartext, so `redirect_to_https` has
    /// nothing to act on.
    pub fn has_plaintext_listener(&self) -> bool {
        let Some(tls) = &self.tls else {
            return false;
        };
        if self.https.is_some() {
            return self.http.is_some();
        }
        tls.plain
    }

    /// The client-address policy this deployment asks for, or the reason it is not usable.
    ///
    /// Returns a policy that is **off** when there is no `[server.proxy]` table or no
    /// `trusted_subnets`, which is the same answer either way: no header is consulted and every
    /// request resolves to its own peer.
    ///
    /// An error means a range did not parse, and it names the offending entry. That is the one
    /// setting here where the failure has a direction: a dropped range is a proxy whose headers are
    /// suddenly disbelieved, so a rate limit keyed on the address throttles every user at once and
    /// the reason is not in the logs.
    pub fn client_ip(&self) -> std::result::Result<crate::clientip::ClientIpPolicy, String> {
        let Some(proxy) = &self.proxy else {
            return Ok(crate::clientip::ClientIpPolicy::off());
        };
        crate::clientip::ClientIpPolicy::new(&proxy.trusted_subnets, &proxy.trusted_headers)
            .map_err(|reason| format!("server.proxy.trusted_subnets: {reason}"))
    }

    /// The rate-limit policy this deployment asks for, or the reason it is not usable.
    ///
    /// `Ok(None)` when there is no `[server.rate_limit]` table, which is the default and means no
    /// limiter is consulted at all — not a limiter configured to allow everything.
    ///
    /// **Built once and shared by both front-ends.** Two policies would give a client twice the
    /// configured allowance depending on which protocol received the request, which is the same
    /// one-front-end-only failure the shared site table exists to prevent.
    pub fn rate_limiter(
        &self,
    ) -> std::result::Result<Option<crate::ratelimit::RateLimitPolicy>, String> {
        let Some(limit) = &self.rate_limit else {
            return Ok(None);
        };
        crate::ratelimit::RateLimitPolicy::new(
            limit.key,
            limit.header.as_deref(),
            limit.rate,
            std::time::Duration::from_millis(limit.interval_ms),
            limit.burst,
            limit.max_entries,
        )
        .map(Some)
        .map_err(|reason| format!("server.rate_limit: {reason}"))
    }

    /// The access policy this deployment asks for, or the reason it is not usable.
    ///
    /// `Ok(None)` when there is no `[server.access]` table, which is the default and means no list
    /// is consulted at all — not a list configured to allow everything. The caller skips the scan
    /// entirely in that case, which is what keeps an unconfigured deployment's request path free.
    ///
    /// The ranges are parsed here, once. A range that does not parse is an error rather than a rule
    /// that matches nobody: this is the one setting whose typo has a direction, and a dropped
    /// `deny` line is a range that is served.
    pub fn access_policy(&self) -> std::result::Result<Option<crate::access::AccessPolicy>, String> {
        let Some(access) = &self.access else {
            return Ok(None);
        };
        crate::access::AccessPolicy::new(
            access.rules.iter().map(|rule| (rule.action, &rule.range)),
            access.default,
        )
        .map(Some)
        .map_err(|reason| format!("server.access: {reason}"))
    }
}
