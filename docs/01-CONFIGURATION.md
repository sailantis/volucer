# Configuration

**Configuration options, types, and defaults for Volucer.**

Use this page as a reference for configuration keys and their defaults. The other
documentation pages explain how the features work.

## The file

Volucer reads TOML configuration from `volucer.toml` by default. Use
`volucer serve --config <path>` to select another file.

**A configuration file is optional.** If you do not pass `--config` and there is no
`volucer.toml` in the working directory, configure the server with command-line overrides
(`-o key=value`, `--listen`, and `--fastcgi`). These options produce the same configuration
table as a file. See [Getting Started](00-GETTING-STARTED.md#run-without-a-configuration-file)
for syntax and limitations.

**Unknown keys are errors.** Volucer validates configuration strictly, so a typo stops
startup instead of being silently ignored.

Any exceptions to this rule are called out in the relevant sections below.

**The file is TOML 1.1.** The one 1.1 feature this matters for is that an inline table may
span several lines and end with a trailing comma, so a table of application metrics reads as
one block:

```toml
[metrics]
address = "127.0.0.1:2112"

collect = {
    app_orders_total = {
        type = "counter",
        help = "Orders the application counted",
    },
    app_queue_depth = { type = "gauge", help = "Jobs waiting" },
}
```

rather than one `[metrics.collect.<name>]` header per metric. Both spellings mean the same
thing, and are freely mixed across files.

**An inline table is closed in the text that writes it.** A `collect = { … }` cannot be
continued in the same file — neither by a later `[metrics.collect.x]` header nor by a dotted
key — and the parser says so: "cannot extend value of type inline table". An `-o` override is
*not* affected: it is laid onto the parsed table, where nothing is inline any more, so
`-o metrics.collect.x.type=counter` adds an entry either way. Use headers when the file
itself has to grow the table in more than one place.

**Editor tooling has to be told the version.** Taplo — which backs the *Even Better TOML*
extension, `taplo lint` and `taplo fmt` — implements TOML 1.0 and reports a multi-line inline
table as three errors (`newline is not allowed in an inline table`, twice, plus the trailing
comma). It has no plan to add 1.1: the request is [taplo#839](https://github.com/tamasfe/taplo/issues/839),
closed pointing at the maintainer's [handover issue](https://github.com/tamasfe/taplo/issues/715).
Tombi (`tombi-toml.github.io`, and its own VS Code extension) is maintained, tests against
`toml-test`, and understands 1.1 once its configuration file says so:

```toml
# tombi.toml at the repository root
toml-version = "v1.1.0"
```

That file is for the editor — Volucer does not read it. `volucer check` remains the authority
on whether a configuration is valid.

## The command line as a source

Each `-o section.key=value` override adds a value to the configuration table, whether it
came from a file or started empty. Overrides are applied **before** deserialization. This
determines how their values are interpreted:

| Rule | Why |
|---|---|
| Values use TOML literal syntax. | `-o workers=8` sets a number. `127.0.0.1:8080` is not a valid TOML literal, so it is read as a string. Quote a value when it might otherwise be read as a number or boolean. |
| Unknown keys are rejected. | Overrides use the same strict validation as files, so `-o server.htpp=…` fails at startup and identifies the invalid key. |
| Environment variables are expanded first. | Overrides use the same expansion as the file, so `-o 'server.http=${HOST:-127.0.0.1}:8080'` works. |

`--listen` and `--fastcgi` are shorthand for `-o server.http=…` and
`-o server.fastcgi=…`. Volucer applies them **before** all `-o` pairs, so an explicit
override such as `-o server.http=…` takes precedence over `--listen …`.

Use dotted keys to override values in maps, which are ordinary TOML tables. For example,
`-o backends.app.workers=8` sets a worker count. If a key itself contains a dot, quote it:
`-o 'server.env."APP.NAME"=x'`.

Address entries in an **array of tables** by their zero-based index:

| Form | Effect |
|---|---|
| `-o server.routes[0].backend=app` | Sets a field on the first element. |
| `-o server.routes[2].path_prefix=/x` | Appends an element if the array currently has two entries. Indices beyond the next available position are rejected, not padded. |
| `-o 'server.routes=[{ path_prefix = "/api" }]'` | Replaces the entire array with the supplied inline TOML value. |

Override values use TOML syntax. PHP array syntax (`['k' => 'v']`) and JSON are not accepted;
in an inline array of tables, separate each key from its value with `=`.

**`server.front_controller` cannot be set with `-o`.** Its empty string means “serve only
real files,” but TOML treats an empty string as a missing value for this key. Use
`volucer serve --front-controller ''` instead.

**Relative paths.** `server.tls.certificate`, `server.tls.key`, `[server.acme].cache`,
a file KV store's `path`, and `[server.access_log].path` resolve relative to the
configuration file's directory. Without a file, they resolve relative to the working
directory. `volucer check` prints each resolved path.

## Environment variables in the file

Volucer expands `${NAME}` and `${NAME:-default}` from its environment **before parsing the
file**. Expansion works in strings, numbers, ports, and listener addresses:

```toml
[server]
http = "127.0.0.1:${PORT:-8080}"

[backends.app]
kind = "worker"
command = "php worker.php"
workers = ${WORKERS:-0}
```

| Form | Meaning |
|---|---|
| `${NAME}` | The variable's value. **Unset is an error**, naming the variable and the line. |
| `${NAME:-default}` | The value, or `default` when the variable is unset **or empty** — bash's `:-`. |
| `$$` | One literal `$`, so `$${NAME}` is the text `${NAME}`. |

**Only braced variables are expanded.** A bare `$NAME` is left unchanged. This allows
values such as passwords, DSNs, and log settings to contain `$` without accidental
substitution.

Important behavior to keep in mind:

- **Variables in comments are expanded too.** Expansion is textual and does not distinguish
  comments from values.
- **An unset variable without a default is an error.** It does not become an empty string.
- **Values cannot contain a newline or NUL byte.** Expansion produces TOML text, so a
  newline could add configuration that is not present in the file. Volucer rejects these
  values and identifies the variable.

`volucer check` uses the same configuration-loading path as `serve`, including variable
expansion, so it reports the configuration Volucer would actually use.

## Top level

| Key | Type | Default | What it does |
|---|---|---|---|
| `backends` | table of tables | empty | The named backends. See [Backends](02-BACKENDS.md). |
| `sites` | table of tables | empty | Per-host applications. See [Routing and Sites](03-ROUTING-AND-SITES.md). |
| `server` | table | all defaults | Listeners, paths and HTTP behaviour. |
| `metrics` | table | absent | The Prometheus endpoint. Absent means no endpoint. |
| `status` | table | absent | The health and worker-control endpoint. Absent means no endpoint. |
| `telemetry` | table | absent | OTLP trace export. Absent means no export. |
| `kv` | table of tables | empty | Named key-value storages. |
| `lock` | table | absent | The lock backend. |
| `jobs` | table | absent | The queue configuration. |

## Sections enabled by their presence

Some features are enabled by the presence of their table. They do not need an
`enabled = true` setting, and an empty table still enables the feature:

| Section | What its presence turns on |
|---|---|
| `[server.tls]` | TLS |
| `[server.acme]` | Automatic certificates (`enabled` inside it defaults to `true`) |
| `[server.access_log]` | Access logging |
| `[server.rate_limit]` | Rate limiting |
| `[metrics]` | The Prometheus endpoint |
| `[status]` | The health endpoint |
| `[telemetry]` | OTLP trace export |
| `[lock]` | The lock backend |
| `[jobs]` | Queues |
| `[kv.<name>]` | That key-value storage |

This is especially important for TLS and ACME: the configuration table itself expresses
that the feature is intended to be active, avoiding a separate flag that could contradict
its certificate or ACME settings.

## `[server]`

| Key | Type | Default | What it does |
|---|---|---|---|
| `http` | string | absent | The cleartext listener address, e.g. `127.0.0.1:8080` or `0.0.0.0:80`. |
| `https` | string | absent | The TLS listener address. Separate from `http`, so you can serve both. |
| `fastcgi` | string | absent | The FastCGI listener, for nginx or another web server to forward to. |
| `document_root` | path | `.` | Where PHP scripts and static files are resolved from. |
| `front_controller` | string | `index.php` | The script a path that names no file is handed to. |
| `script_extensions` | array of strings | `["php", "php3", "phtml"]` | Which extensions are executed as PHP. |
| `max_body_bytes` | integer | `33554432` (32 MiB) | The largest request body either front-end accepts. |
| `static_files` | table or bool | enabled | The static file layer. See [Static Files](04-STATIC-FILES.md). |
| `compression` | table | absent | Response compression. Off unless the table is there. |
| `serve_only` | string | `"all"` | `"all"`, `"backend"` or `"static"`. |
| `default_backend` | string | — | The backend used when a request matches no route. |
| `routes` | array of tables | empty | The routing rules for the unnamed site. See [Routing and Sites](03-ROUTING-AND-SITES.md). |
| `env` | table | empty | Environment variables every started process inherits. |
| `tls` | table | absent | TLS configuration. Presence enables it. |
| `acme` | table | absent | Automatic certificates. Presence enables it. |
| `redirect_to_https` | bool | `false` | Answer cleartext requests with a 301 to the `https://` equivalent. Per site too. See [TLS and ACME](05-TLS-AND-ACME.md). |
| `hsts` | table | absent | `Strict-Transport-Security` for every site that does not state its own. See [TLS and ACME](05-TLS-AND-ACME.md). |
| `access_log` | table | absent | The access log. Presence enables it. |
| `proxy` | table | absent | Which peers may assert a client address. |
| `rate_limit` | table | absent | The rate limiter. Presence enables it. |
| `trace_context` | string | `"extract"` | How the client's trace context is handled when setting a backend's `traceparent`: `"ignore"`, `"extract"`, `"inject"` or `"propagate"`. See [Observability](06-OBSERVABILITY.md#trace-context). |

### `trace_context`

These values correspond to nginx's `otel_trace_context` setting:

| Value | Reads the client's `traceparent` | Sends Volucer's context to the backend |
|---|---|---|
| `"ignore"` | no | no |
| `"extract"` | yes | no — the default |
| `"inject"` | no | yes |
| `"propagate"` | yes | yes |

With `"extract"`, the client header reaches the backend unchanged; this is the default and
matches the server's previous behavior. `"inject"` sends Volucer's context without reading
the client's header. `"propagate"` reads the client context, then replaces the header sent
to the backend with Volucer's own context. The backend's spans are therefore children of
Volucer's span, rather than siblings. Because it changes the context the backend receives,
`"propagate"` is opt-in.

This key does not need a `[telemetry]` table. It is about what a *backend* is told, which
matters most in the deployment that exports nothing from here.

### `max_body_bytes`

**The value is in bytes.** This differs from RoadRunner's `http.max_request_size`, which
uses megabytes; do not copy that setting's unit.

The limit applies to both HTTP and FastCGI front-ends. It does not apply to an
`upstream` backend, where the request body streams to the origin. Set the body limit on
the origin instead.

Zero is rejected rather than treated as “unlimited”; it would reject every request with a
body.

### `serve_only`

| Value | Behaviour |
|---|---|
| `all` | Static files first, then the backend. The default. |
| `backend` | The backend only. A path naming a file is dispatched like any other. |
| `static` | Files only. A path naming no file is a 404, and no backend is consulted. |

### `[server.env]`

These variables are inherited by **every process started by this deployment**, including
workers, job consumers, `php-cgi` slots, and supervised `upstream` origins.

```toml
[server.env]
APP_ENV = "production"
DATABASE_URL = "postgres://app@db/app"
```

Environment values are applied in this order:
`inherited environment → [server.env] → backend-specific env`. A backend's `env` takes
precedence, so shared settings can live in one place while a particular backend can
override them.

`attach` workers do not receive these settings because Volucer does not start their
processes.

Variable names are passed **exactly as written**; Volucer does not change their case.
Names containing `=` or a NUL byte are rejected at startup because they cannot be passed
to the process environment.

**Values are treated as secrets by `config.describe`.** It reports variable names but
omits values, including values supplied through expansion such as `${DATABASE_PASSWORD}`.

### The server's own variables

Volucer sets these variables **after** applying configured environment values, so they
cannot be overridden:

| Variable | Value |
|---|---|
| `VOLUCER_VERSION` | The version of the volucer that started the process. |
| `VOLUCER_WORKER_SOCKET` | Where a `socket`-transport worker connects back — see [Worker Applications](07-WORKER-APPLICATIONS.md). |
| `VOLUCER_FCGI_ADDRESS` | The address a `spawn_cgi` slot must bind. Only read by a `command` deployment; `php-cgi` takes it as `-b`. |

`VOLUCER_VERSION` lets a `php-cgi` process or supervised origin identify the Volucer
version that started it. Worker SDKs do not need this variable because the worker
handshake reports the worker's version.

`VOLUCER_FCGI_ADDRESS` contains the same address as the `{address}` placeholder. A
`spawn_cgi` command can use either the command-line argument or the environment variable
to learn where to bind. Volucer sets this variable after backend-specific `env` values so
the process cannot be directed to an address the pool does not use.

### `front_controller`

Requests for paths that do not name a file are passed to this script with `PATH_INFO` set.
For example, `/users/42` can be handled by `index.php` instead of returning 404.

Set it to an empty string with `--front-controller ''` to disable the front controller
and serve only existing files. An empty value cannot be set in the TOML file because it
is treated as though the key were absent.

## `[server.static_files]`

This section can also be set to a plain `true` or `false`, as shorthand for `enabled`.

| Key | Type | Default | What it does |
|---|---|---|---|
| `enabled` | bool | `true` | Serve files from the document root. |
| `precompressed` | bool | `false` | Serve `file.gz` or `file.br` when the client accepts that encoding. |
| `directory_index` | bool | `true` | Serve `index.html` for a directory. |
| `dotfiles` | string | `"deny"` | `"deny"` or `"allow"`. |
| `cache_control` | string | `"public, max-age=3600"` | The `Cache-Control` header on served files. |
| `max_multirange_bytes` | integer | `1048576` (1 MiB) | The largest total a multi-range request may ask for. |

**Static-file serving is enabled by default.** You do not need to add this section to
serve files.

**A table only overrides the settings it contains.** If `enabled` is omitted, its
inherited value remains unchanged. For example, this explicitly disables static files:

```toml
[server.static_files]
enabled = false
```

This changes the cache header without changing whether static-file serving is enabled:

```toml
[server.static_files]
cache_control = "public, max-age=60"
```

Use `static_files = false` as a one-key opt-out, or `static_files = true` to enable
serving while retaining the other inherited settings.

With `dotfiles = "deny"`, requests for hidden files return **404**, not 403. This avoids
revealing whether a hidden file exists.

## `[server.compression]`

| Key | Type | Default | What it does |
|---|---|---|---|
| `enabled` | bool | `false` | Compress responses. |
| `encodings` | array of strings | `["gzip", "zstd"]` | The encodings offered, in preference order. |
| `min_length` | integer | `1024` | Responses shorter than this are not compressed. |
| `level` | integer | `6` | The compression level. |

## `[server.tls]`

| Key | Type | Default | What it does |
|---|---|---|---|
| `certificate` | path | absent | The PEM certificate chain, leaf first. |
| `key` | path | absent | The PEM private key. |
| `min_version` | string | `"1.2"` | `"1.2"` or `"1.3"`. A floor, not a cap. |
| `alpn` | array of strings | `["h2", "http/1.1"]` | The protocols offered. |
| `plain` | bool | `false` | Also accept cleartext on the TLS port. |

Relative certificate and key paths are resolved from the configuration file's directory,
not the process's working directory. `volucer check` prints the resolved paths.

See [TLS and ACME](05-TLS-AND-ACME.md) for deployment examples and details about `plain`.

## `[server.acme]`

| Key | Type | Default | What it does |
|---|---|---|---|
| `enabled` | bool | `true` | Issue certificates. |
| `authority` | string | `"staging"` | `"staging"` or `"production"`. |
| `domains` | array of strings | derived from the site's hosts | The names to issue for. |
| `contact` | array of strings | absent | Addresses the CA warns about failing renewals. |
| `cache` | path | `"acme-cache"` | Where issued certificates are kept, relative to the configuration file. |

The default authority is `staging`. Production certificate authorities enforce rate
limits, so validate the configuration with staging before switching to `production`.

Setting `enabled = false` retains the ACME settings but disables certificate issuance.
Use this to prepare contact details or pause an order without removing the configuration.

## `[server.access_log]`

| Key | Type | Default | What it does |
|---|---|---|---|
| `path` | path | **required** | Where the log is written. |
| `format` | string | `"combined"` | `"combined"` or `"json"`. |
| `request_id_header` | string | `"X-Request-Id"` | The header to adopt a request id from. |

The `combined` format follows nginx's log format and appends request and origin timings.
The `json` format writes one JSON object per line.

Set `request_id_header` to an empty string to always generate a new request ID instead of
adopting one from a request header.

## `[server.proxy]`

| Key | Type | Default | What it does |
|---|---|---|---|
| `trusted_subnets` | array of strings | empty | The peers whose forwarded headers are believed. |
| `trusted_headers` | array of strings | empty | The headers to read, in order. |

Configure trusted proxies when Volucer runs behind a reverse proxy. Otherwise, requests
appear to come from the proxy itself, so a per-client rate limit would apply to the proxy
rather than to individual clients.

## `[server.rate_limit]`

| Key | Type | Default | What it does |
|---|---|---|---|
| `rate` | integer | **required** | Requests allowed per interval. |
| `interval_ms` | integer | `1000` | The window the rate applies to. |
| `key` | string | `"ip"` | `"ip"`, `"global"` or `"header"`. |
| `header` | string | absent | The header to key on, when `key = "header"`. |
| `burst` | integer | `rate` | How many may be spent at once. |
| `max_entries` | integer | `10000` | Identities tracked before new ones are refused. |

With `key = "ip"`, rate-limit buckets use the client address resolved by
`[server.proxy]`. This preserves per-client limits behind nginx or another trusted proxy.

`key = "header"` does **not** authenticate a client. Clients can choose their own header
values and evade limits by changing them. Use a value set by a trusted proxy, such as a
verified API key or tenant identifier.

**Reloadable.** On `SIGHUP`, Volucer replaces the policy in place. It preserves each
caller's bucket when the identity definition is unchanged—that is, when `key` and
`header` stay the same. Tightening `rate` therefore applies to callers' existing usage
rather than granting each a fresh burst.

Changing `key` or `header` changes the identity definition, so Volucer starts new buckets
and reports this in the log. Adding or removing the entire table also starts with empty
buckets.

## `[[server.routes]]`

| Key | Type | Default | What it does |
|---|---|---|---|
| `path_prefix` | string | absent | Match paths starting with this. |
| `path_suffix` | string | absent | Match paths ending with this. |
| `host` | string | absent | Match this `Host` header. |
| `policy` | string | `"all"` | `"all"`, `"backend"` or `"static"`. |
| `backend` | string | absent | The backend for matched requests. Required unless `policy = "static"`. |

These routes belong to the unnamed site and are declared as `[[server.routes]]` alongside
the other `[server]` settings. A `[sites.<name>]` table can define its own `routes`; that
list **replaces** the global list rather than extending it.

**Configuration changed in 0.3.0:** `[[routes]]` and the top-level `default_backend`
moved under `[server]`. The old forms are rejected at startup, with an error that names
the replacement setting.

## `[sites.<name>]`

A site represents an application selected by the `Host` header. It normally requires
`hosts` and `document_root`. Other settings inherit from `[server]` unless overridden.
If you specify `addresses`, `hosts` becomes optional.

| Key | Type | Default | What it does |
|---|---|---|---|
| `hosts` | array of strings | **required** unless `addresses` is set | The host names this site answers to. |
| `addresses` | array of strings | any address | The **local** addresses this site answers on, e.g. `["192.168.9.123"]` or `["192.168.9.0/24"]`. |
| `protocols` | array of strings | all | `"http"`, `"https"` and/or `"fastcgi"` — the front-ends this site answers on. |
| `document_root` | path | **required** | Where this site's files live. |
| `front_controller` | string | inherited | This site's front controller. |
| `script_extensions` | array of strings | inherited | This site's executed extensions. |
| `default_backend` | string | inherited | This site's default backend. |
| `routes` | array of tables | inherited | This site's routes. **Replaces** the global list; it is not merged. |
| `static_files` | table | inherited | This site's static file settings. |
| `compression` | table | inherited | This site's compression settings. |
| `serve_only` | string | inherited | This site's serving policy. |
| `tls` | table | inherited | This site's certificate and key. |
| `acme` | table | inherited | This site's ACME settings. |

### `addresses` and `protocols`

Both settings restrict which connections a site can serve. Volucer checks them
**before** consulting the `Host` header. See
[Routing and Sites](03-ROUTING-AND-SITES.md#binding-a-site-to-addresses-and-protocols)
for the matching order and rationale.

`addresses` refers to the **local address** that accepted the connection, not the
client's address. Because a caller controls the `Host` header, it should not be the only
way to select an internal site. Values can be addresses or CIDR ranges, using the same
syntax as `[server.proxy].trusted_subnets`:

```toml
[sites.internal]
hosts     = ["internal.corp"]
addresses = ["192.168.9.123", "10.0.0.0/8"]
document_root = "/srv/internal"
```

A site with `addresses` but no `hosts` acts as the default site for those addresses,
similar to nginx's `default_server`. It handles all requests received on them, including
requests with an otherwise unclaimed host name. `[server]` is the fallback for
unmatched sites and cannot itself be bound to an address.

## `[backends.<name>]`

A backend is one of five types, selected with `kind`. See
[Backends](02-BACKENDS.md) for what each one does.

```toml
[backends.app]
kind = "worker"
command = "php worker.php"
```

### `kind = "worker"`

| Key | Type | Default | What it does |
|---|---|---|---|
| `command` | string | **required** | The command that starts a worker, e.g. `php worker.php`. |
| `workers` | integer | `0` | How many workers to run. `0` means one per logical CPU. |
| `memory_limit_mb` | integer | absent | Recycle a worker whose peak RSS exceeds this. |
| `max_requests` | integer | absent | Recycle a worker after this many requests. |
| `working_dir` | path | absent | The working directory for the worker. |
| `env` | table | empty | Extra environment variables for the worker, merged over `[server.env]`. |
| `services` | array of strings | empty | The services this backend's workers may call. |
| `grpc_services` | array of strings | empty | The gRPC services this backend may serve. |
| `grpc_serve_any` | boolean | `false` | Serve any gRPC service instead of naming them. Refused beside a non-empty `grpc_services`. |
| `grpc_proto` | array of paths | empty | Read the gRPC services from `.proto` files or directories. Refused beside both `grpc_services` and `grpc_serve_any`; re-read on reload. |
| `checksum` | string | `"required"` | `"required"` or `"optional"`. |
| `transport` | string | platform default | `"pipe"` or `"socket"`. |
| `socket` | string | absent | Where the socket transport listens. |

`workers = 0` starts one worker per logical CPU.

The default transport is `"socket"` on Windows and `"pipe"` on other platforms. This
accounts for platform differences in PHP's stream handling: on Windows, a blocking pipe
read can take about 8 ms to return, compared with about 0.09 ms for a socket. Volucer
logs the selected transport at startup. A mismatched transport can still return
successful responses, but more slowly.

`socket` is a **prefix** and must start with `unix:`. Each worker listens on
`<prefix>.<slot>.sock`. A bare path is relative and can produce an unexpected filename,
so do not copy a socket address from an `external` backend. Volucer rejects `socket` when
`transport = "pipe"` rather than silently ignoring the unused setting.

### `kind = "attach"`

This backend uses the same protocol as `worker`, but connects to an existing relay
instead of starting worker processes.

| Key | Type | Default | What it does |
|---|---|---|---|
| `workers` | integer | **required** | How many connections to hold open. |
| `relay` | string | absent | The relay address. Falls back to `VOLUCER_RELAY_SOCKET`. |
| `max_requests` | integer | absent | Recycle a slot after this many requests. |
| `checksum` | string | `"required"` | `"required"` or `"optional"`. |
| `services` | array of strings | empty | The services this backend's workers may call. |
| `grpc_services` | array of strings | empty | The gRPC services this backend may serve. |
| `grpc_serve_any` | boolean | `false` | Serve any gRPC service instead of naming them. Refused beside a non-empty `grpc_services`. |
| `grpc_proto` | array of paths | empty | Read the gRPC services from `.proto` files or directories. Refused beside both `grpc_services` and `grpc_serve_any`; re-read on reload. |

### `kind = "external"`

Connect to an already-running FastCGI server, such as `php-fpm`.

| Key | Type | Default | What it does |
|---|---|---|---|
| `socket` | string | **required** | The FastCGI address, e.g. `unix:/run/php/php8.3-fpm.sock` or `127.0.0.1:9000`. |
| `max_connections` | integer | absent | How many connections to keep open to the pool. |
| `services` | array of strings | empty | What this backend's application may reach over `[rpc]`. See [Services](08-SERVICES.md#an-application-that-is-not-a-worker). |

There is no `workers` setting for `external`: the pool is managed separately. Configure
its size where that server is configured, for example with `pm.max_children` or
`PHP_FCGI_CHILDREN`.

This backend is not PHP-specific. Any FastCGI-speaking WSGI, Rack, or Go server can use
it.

### `kind = "spawn_cgi"`

Use `spawn_cgi` to have Volucer start and supervise FastCGI processes. This includes
`php-cgi` and other FastCGI servers. On Windows, it is the FastCGI option for
per-request state reset.

Exactly one of `binary` and `command`:

| Key | Type | Default | What it does |
|---|---|---|---|
| `binary` | path | — | The `php-cgi` executable. The `PHP_FCGI_*` driver. A bare name is looked up in the PHP search directories (`PATH` first); a value containing a separator is used as a path. |
| `command` | string | — | A command that starts **one slot**, with `{address}` where it must bind and optionally `{concurrency}` where its worker count belongs. |
| `workers` | integer | `0` | How many slots. `0` means one per logical CPU. |
| `children` | integer | `0` | `php-cgi` children behind each slot (`binary` only). `0` means no prefork. |
| `concurrency` | integer | `0` | Requests one slot serves at once (`command` only). `0` means one. |
| `working_dir` | path | absent | Working directory for each started process. |
| `bind_host` | string | `"127.0.0.1"` | Where the slots listen. Loopback only. |
| `base_port` | integer | `19001` | The first port, when not using a socket. |
| `socket` | string | absent | A socket prefix instead of TCP ports. |
| `max_requests` | integer | absent | Recycle a slot after this many requests. |
| `env` | table | empty | Extra environment variables for every slot. |
| `services` | array of strings | empty | What this backend's application may reach over `[rpc]`. See [Services](08-SERVICES.md#an-application-that-is-not-a-worker). |

Setting `children > 0` enables prefork. For the same concurrency, prefork uses about
4.5× the memory: child processes inherit the listening socket and OPcache segment, so
compiled scripts are shared. `children` counts child processes, not total processes;
`php-cgi` also has a master process. The total is
`workers × (children + 1)`. A value of `children = 1` is normalized to `0`, preserving
the default no-prefork behavior.

Use `command` for a non-PHP server. Volucer replaces `{address}` in each argument with
the address the slot must bind and also sets `VOLUCER_FCGI_ADDRESS` to that value. Startup
fails if the command uses neither, because the pool would have no address to connect to.
`concurrency` sets how many requests **one slot** can handle. The total capacity is
`workers × concurrency`; setting the total as `concurrency` would incorrectly assign that
many connections to every address.

**Volucer runs the command once per slot**, providing a different `{address}` each time.
Any processes the command starts belong to that slot, so their request capacity is
`concurrency`. For example, a Gunicorn command with `--workers 4` needs
`concurrency = 4`. If the server also uses four threads per worker, set
`concurrency = 16`. With `workers = 4` and `concurrency = 4`, Volucer starts four
Gunicorn instances with four workers each: sixteen processes in total, four per address.
`volucer check` reports the total in its `processes:` line.

Do not set both `children` and `concurrency`; Volucer rejects that combination. The old
setting name `capacity` is also rejected, with an error naming its replacement.

**Use `{concurrency}` in the command to keep the worker count in one place.** Without the
placeholder, the command and the configuration repeat the same number, and can disagree:

```toml
[backends.py]
kind = "spawn_cgi"
command = "gunicorn myapp.wsgi --workers {concurrency} --bind {address}"
workers = 4       # four slots: the command is started four times, four addresses
concurrency = 4   # substituted above: each address serves four requests at once
working_dir = "/srv/app"
```

Volucer substitutes the **resolved** concurrency. The default `concurrency = 0` resolves
to `1`, so the program receives the actual per-slot limit rather than a zero that could
mean “start no workers.” `volucer check` prints both the resolved command and total
process count.

`{workers}` is **not** a supported placeholder and is rejected. The command runs once for
each slot, so the worker count controls how many copies Volucer starts; it is not a
setting for an individual process. Other brace expressions are left unchanged, so
arguments such as `--define={a:1}` still work.

You can still write the number directly in the command, for example
`command = "gunicorn myapp.wsgi --workers 4 --bind {address}"` with `concurrency = 4`.
That starts the same processes, but you must keep the two values in sync.

Here, `socket` is also a prefix. It creates Unix sockets on Linux and macOS and **named
pipes** on Windows, which is what `php-cgi -b` binds there. For example,
`\\.\pipe\volucer-cgi` gives slot 0 the pipe `\\.\pipe\volucer-cgi.0`. A `command` slot
receives a `unix:` address and cannot use a named pipe.

### `kind = "upstream"`

Route to an HTTP origin that runs its own HTTP server, such as Node, ASGI, Go, Python,
or Ruby.

| Key | Type | Default | What it does |
|---|---|---|---|
| `origin` | string | **required** | The origin's address. |
| `command` | string | absent | A command to start the origin with. |
| `working_dir` | path | absent | The origin's working directory. |
| `env` | table | empty | Extra environment variables for the origin, merged over `[server.env]`. |
| `protocol` | string | absent | `"h2c"` for HTTP/2 over cleartext, with prior knowledge. |
| `tls` | table | absent | How to reach an HTTPS origin. |
| `restart` | string | `"on_failure"` | `"never"`, `"on_failure"` or `"always"`. |
| `restart_limit` | integer | `3` | How many times to restart the origin. |
| `restart_backoff_ms` | integer | `1000` | The delay between restarts. |
| `ready_timeout_ms` | integer | `30000` | How long to wait for the origin to answer. |
| `services` | array of strings | empty | What this origin's application may reach over `[rpc]`, as headers. See [Services](08-SERVICES.md#an-application-that-is-not-a-worker). |

The default, `restart = "on_failure"`, restarts a process that exits with an error. A
process that receives `SIGTERM` and exits successfully is treated as an intentional
shutdown and is not restarted.

`[backends.app.upstream.tls]`:

| Key | Type | Default | What it does |
|---|---|---|---|
| `ca` | path | absent | A CA bundle. Public roots are used when absent. |
| `sni` | string | absent | The server name to send. |

## `[metrics]`

| Key | Type | Default | What it does |
|---|---|---|---|
| `address` | string | **required** | Where the Prometheus endpoint listens. |
| `path` | string | `"/metrics"` | The endpoint path. |
| `prefix` | string | absent | A prefix for the metric names. |
| `collect` | table of tables | empty | The metrics to collect. |

`[metrics.collect.<name>]`:

| Key | Type | Default | What it does |
|---|---|---|---|
| `type` | string | **required** | `"counter"`, `"gauge"` or `"histogram"`. |
| `help` | string | **required** | The description, as Prometheus requires. |
| `buckets` | array of numbers | empty | The bucket boundaries. Histograms only. |

`buckets` is valid only for histograms. Volucer rejects it on counters and gauges rather
than accepting an unused setting.

`collect` may be written as headers or as one inline table. **The inline form is closed in the
text that writes it**: a later `[metrics.collect.<name>]` header, or a dotted key inside the
same table, is a parse error ("cannot extend value of type inline table"). Overrides are not
affected — `-o metrics.collect.<name>.type=…` is applied after parsing, so it adds an entry to
either spelling.

## `[status]`

| Key | Type | Default | What it does |
|---|---|---|---|
| `address` | string | **required** | Where the status endpoint listens. |
| `unavailable_status_code` | integer | `503` | The code returned when the server is not ready. |
| `allow_worker_reset` | bool | `false` | Expose the worker-control routes. |

**`allow_worker_reset` does not enable authentication.** Worker-control routes do not
exist unless this option is enabled, and requests to them otherwise return 404. Volucer
also rejects this option when `address` is a wildcard. An unauthenticated request that
recycles workers could otherwise act as a remote kill switch. Bind `[status]` to
loopback or a private interface and expose it only to your orchestrator, or leave the
option disabled.

`allow_worker_reset` can be changed in either direction with `SIGHUP`. Disabling it
removes the routes on the next request, returning 404 without restarting the server.
Enabling it during a reload is subject to the same wildcard-address validation as at
startup. The `[status]` `address` cannot be reloaded because its socket is already bound.

`volucer health` reads the probe address from this table, so you do not need to specify
the port again. In a container, a wildcard listener is common; the health command probes
it through loopback. See [Health checks](11-OPERATIONS.md#health-checks).

## `[telemetry]`

| Key | Type | Default | What it does |
|---|---|---|---|
| `endpoint` | string | **required** | The collector. `/v1/traces` is appended. |
| `service_name` | string | `"volucer"` | The `service.name` resource attribute. |
| `headers` | table | empty | Extra headers, e.g. for a hosted collector. |
| `timeout_ms` | integer | `10000` | Per export request. |
| `batch_size` | integer | `512` | Spans per batch. |
| `interval_ms` | integer | `5000` | How long a partly-filled batch waits. |
| `sample` | string | `"parent_based"` | Which traces are exported: `"parent_based"`, `"always"` or `"ratio"`. See [Sampling](06-OBSERVABILITY.md#sampling). |
| `sample_ratio` | number | absent | The fraction for `sample = "ratio"`, from `2^-56` to `1`. Refused for the other two modes. |

What a backend receives is **not** configured here. `[server].trace_context` decides that,
because it is a fact about the request path rather than about where spans go — see
[Observability](06-OBSERVABILITY.md#trace-context).

`sample` decides what *leaves the process*, which is a different question: a deployment may
propagate a trace to its backend and still decline to export its own span. See
[Sampling](06-OBSERVABILITY.md#sampling).

## `[rpc]`

The call plane for applications that are not workers: a socket a FastCGI or HTTP backend's
application connects to, to reach the `services` its backend names. Absent means no
listener.

| Key | Type | Default | What it does |
|---|---|---|---|
| `listen` | string | **required** | `unix:/run/volucer/rpc.sock` or `127.0.0.1:6001`. A wildcard address is refused. |
| `max_connections` | integer | absent | Refuse a connection beyond this many. `0` is refused. |
| `idle_timeout_seconds` | integer | `300` | Close a connection this long after its last call. `0` is refused. |
| `call_timeout_seconds` | integer | `10` | A per-call deadline. `0` is refused. |

Unlike `[metrics]` and `[status]`, this table alone is not enough: at least one backend must
also grant something in its `services`, or the listener would refuse every call. Volucer
refuses either half alone, and names the key the file is missing.

```toml
[backends.app]
kind     = "external"
socket   = "unix:/run/php/php8.3-fpm.sock"
services = ["kv.cache"]

[kv.cache]
driver = "memory"

[rpc]
listen = "unix:/run/volucer/rpc.sock"
```

The address and a per-backend token reach the application as the CGI parameters
`VOLUCER_RPC` and `VOLUCER_RPC_TOKEN`, and an `upstream` origin receives the same two
values as the headers `X-Volucer-Rpc` and `X-Volucer-Rpc-Token`. A backend that grants
nothing is given neither. See
[Services](08-SERVICES.md#an-application-that-is-not-a-worker).

## `[kv.<name>]`

| Key | Type | Default | What it does |
|---|---|---|---|
| `driver` | string | **required** | `"memory"`, `"file"` or `"redis"`. |

`driver = "memory"`:

| Key | Type | Default |
|---|---|---|
| `max_bytes` | integer | `67108864` (64 MiB) |

`driver = "file"`:

| Key | Type | Default |
|---|---|---|
| `path` | path | **required** |
| `max_bytes` | integer | `67108864` (64 MiB) |

`driver = "redis"`:

| Key | Type | Default |
|---|---|---|
| `url` | string | **required** |
| `pool_size` | integer | `16` |

Redis support is available only when Volucer is built with the `redis` feature. Without
that feature, Volucer rejects Redis configuration at startup.

## `[lock]`

| Key | Type | Default | What it does |
|---|---|---|---|
| `driver` | string | **required** | `"memory"` or `"redis"`. |

With `driver = "redis"`, configure `url` (required), `pool_size` (default `16`), and
`read_timeout_ms` (default `5000`).

## `[jobs]`

| Key | Type | Default | What it does |
|---|---|---|---|
| `consume` | array of strings | empty | The pipelines this server consumes from. |
| `pipelines` | table of tables | empty | The named pipelines. |
| `pool` | table | absent | The consumer pool. Required when `consume` is not empty. |

Each `[jobs.pipelines.<name>]` uses the same `driver` options as `[kv.<name>]`:
`"memory"` or `"redis"`. Redis pipelines also accept `url`, `pool_size`, and
`read_timeout_ms`.

`[jobs.pool]`:

| Key | Type | Default | What it does |
|---|---|---|---|
| `command` | string | **required** | The command that starts a consumer. |
| `workers` | integer | `0` | How many consumers. `0` means one per logical CPU. |
| `max_jobs` | integer | absent | How many jobs one consumer takes before exiting. |
| `job_lease_seconds` | integer | `300` | How long a reserved job is held before it returns to the queue. |
| `prefetch` | integer | `1` | How many jobs a consumer reserves in advance. |
| `memory_limit_mb` | integer | absent | Recycle a consumer above this RSS. |
| `checksum` | string | `"required"` | `"required"` or `"optional"`. |
| `transport` | string | platform default | `"pipe"` or `"socket"`. |
| `socket` | string | absent | Where the socket transport listens. |
| `working_dir` | path | absent | The working directory. |
| `env` | table | empty | Extra environment variables, merged over `[server.env]`. |

## A complete example

```toml
[server]
http = "127.0.0.1:8080"
document_root = "./public"
front_controller = "index.php"
default_backend = "app"
max_body_bytes = 33554432
redirect_to_https = false

[server.static_files]
enabled = true
cache_control = "public, max-age=3600"

[server.compression]
enabled = true
encodings = ["gzip", "zstd"]

[server.tls]
certificate = "certs/site.crt"
key = "certs/site.key"

[server.access_log]
path = "/var/log/volucer/access.log"
format = "json"

[server.proxy]
trusted_subnets = ["127.0.0.1/32", "10.0.0.0/8"]
trusted_headers = ["x-forwarded-for"]

[server.rate_limit]
rate = 100
interval_ms = 1000
key = "ip"
burst = 20

[backends.app]
kind = "worker"
command = "php worker.php"
workers = 8
memory_limit_mb = 256
max_requests = 1000
services = ["kv", "lock", "jobs"]
grpc_services = ["greeter.Greeter"]

[backends.api]
kind = "worker"
command = "php api-worker.php"

[[server.routes]]
path_prefix = "/api"
backend = "api"

[sites.shop]
hosts = ["shop.example.com"]
document_root = "./shop/public"

[metrics]
address = "127.0.0.1:2112"

[status]
address = "127.0.0.1:2114"

[telemetry]
endpoint = "http://127.0.0.1:4318"
service_name = "shop"

[kv.cache]
driver = "memory"
max_bytes = 67108864

[lock]
driver = "memory"
```

A larger, annotated example covering every key lives in
[`volucer.example.toml`](https://github.com/sailantis/volucer/blob/main/volucer.example.toml).
