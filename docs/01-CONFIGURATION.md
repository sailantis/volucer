# Configuration

**Every key volucer accepts, with its default.**

This is the reference. The other pages in this guide explain what the keys *do*; this one
tells you what they are called and what happens if you leave them out.

## The file

Volucer reads a TOML file, `volucer.toml` by default. Point it somewhere else with
`volucer serve --config <path>`.

**The file is optional.** When neither `--config` nor a `volucer.toml` beside the working
directory exists, the configuration comes from the command line alone — `-o key=value` (and
`--listen` / `--fastcgi`) — and the two sources are the same table by the time anything reads
them. See [Getting Started](00-GETTING-STARTED.md#without-a-file-at-all) for the flag rules and
the limits.

**Unknown keys are an error, not a warning.** The configuration is parsed strictly, so a
typo stops the server at startup rather than silently doing nothing. This is deliberate: a
key that parses and is ignored looks supported and cannot work.

The one exception is a `deny_unknown_fields`-free path in a few places noted below.

## The command line as a source

`-o section.key=value` lays one value onto the table the file produced (or onto an empty one),
**before** deserialization. Three consequences follow from that order, and they are the whole
design:

| Rule | Why |
|---|---|
| The value is read as a TOML literal. | `-o workers=8` is a number because TOML says so, not because a table of key types says so. `127.0.0.1:8080` is not valid TOML, so it stays the string it looks like. Quote it to force a string when a value would otherwise read as a number or a boolean. |
| An unknown key is refused by name. | The same `deny_unknown_fields` that guards the file guards a flag, so `-o server.htpp=…` is a startup error rather than a setting that parses, is ignored, and looks supported. |
| `${NAME}` is expanded first. | A pair goes through the same expansion as the file's text, so `-o 'server.http=${HOST:-127.0.0.1}:8080'` works. |

`--listen` and `--fastcgi` are sugar for `-o server.http=…` and `-o server.fastcgi=…`, applied
**before** every `-o` pair — so an explicit `-o server.http=…` wins over `--listen …`.

Arrays of tables (`[[server.routes]]`, `[sites.<name>]`) and `map` values have no dotted
spelling and stay file-only.

**One key cannot be spelled as an override**: `server.front_controller`. Its empty string is
reserved to mean "serve only real files", and TOML cannot tell an empty string from a missing
key there — which is why `validate` refuses it. Use the flag for that one,
`volucer serve --front-controller ''`, which is the documented spelling.

**Relative paths and the command line.** `server.tls.certificate`, `server.tls.key`,
`[server.acme].cache`, a file KV store's `path` and `[server.access_log].path` resolve against
the **config file's directory**. With no file, they resolve against the **working directory** —
the only directory left once there is no file to be beside. `volucer check` prints the resolved
value either way.

## Environment variables in the file

`${NAME}` and `${NAME:-default}` are expanded from the server's own environment **before the
file is parsed**, so they work in a string, a number, a port or a listener address:

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

**Only the braced form is expanded.** A bare `$NAME` is left alone, unlike RoadRunner: a
password, a DSN or an access-log value may legitimately contain `$`, and a rule that expanded
it would make the file mean something its author did not write.

Three consequences worth knowing before writing one:

- **A `${...}` in a comment is expanded too.** The substitution is textual and does not know a
  comment from a value. This is the price of one rule rather than two.
- **An unset variable without a default stops the server** rather than becoming an empty
  string. An empty listener address is a server that binds nothing for a reason nothing logs.
- **A value may not contain a line break or a NUL byte.** Because the substitution produces
  TOML text, a newline inside a value could introduce a table header the file does not contain.
  The expansion refuses it, naming the variable.

`volucer check` reads the file through the same path, so it reports the deployment `serve`
would actually start — including the expansion's own failures.

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

Several features are turned on by the *existence* of a table, not by a flag inside it.
There is no `enabled = true` to write for these, and a present-but-empty table is not the
same as an absent one:

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

TLS and ACME are the reason this rule exists. A flag and a certificate are two statements
about one thing, and two statements can disagree. Having the table means you have a
certificate, or you have ACME configured to get one.

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
| `redirect_to_https` | bool | `false` | Answer cleartext requests with a 301 to the `https://` equivalent. |
| `access_log` | table | absent | The access log. Presence enables it. |
| `proxy` | table | absent | Which peers may assert a client address. |
| `rate_limit` | table | absent | The rate limiter. Presence enables it. |

### `max_body_bytes`

**Bytes, not megabytes.** The unit is in the name on purpose — RoadRunner spells this
`http.max_request_size` and counts megabytes, so copying that spelling here is an error
rather than a silently different unit.

It bounds both listeners. The FastCGI front-end had no bound at all before this key
existed, so a request forwarded by a proxy with no `client_max_body_size` of its own could
make volucer buffer without limit.

It does **not** bound an `upstream` backend, whose body streams to the origin unread. The
limit to configure for one of those is the origin's.

Zero is refused rather than read as "unlimited" — a zero here refuses every request with a
body, which is never what was meant.

### `serve_only`

| Value | Behaviour |
|---|---|
| `all` | Static files first, then the backend. The default. |
| `backend` | The backend only. A path naming a file is dispatched like any other. |
| `static` | Files only. A path naming no file is a 404, and no backend is consulted. |

### `[server.env]`

Environment variables that **every process this deployment starts** inherits: a worker pool, a
job consumer, a `php-cgi` slot, and a supervised `upstream` origin.

```toml
[server.env]
APP_ENV = "production"
DATABASE_URL = "postgres://app@db/app"
```

The order is `inherited environment → [server.env] → the backend's own env`. A backend's `env`
wins, so the common case is written once and the exception — one consumer pointed at a different
database — is still expressible on that table.

`attach` does not receive it: volucer starts those processes not at all.

Names are passed **exactly as written**, with no case folding (RoadRunner uppercases `server.env`
keys; a server that silently renamed a variable would leave an application reading `app_env` from
a file that says `APP_ENV`). A name containing `=` or a NUL byte is refused at startup, because
`Command::env` would otherwise fail at spawn with an error naming no key.

**Every value here is a secret as far as `config.describe` is concerned**: the call reports the
variable names and elides the values, so `${DATABASE_PASSWORD}` supplied from the environment
cannot reach a worker.

### The server's own variables

Two variables are set **after** the configured `env`, so a table cannot shadow either:

| Variable | Value |
|---|---|
| `VOLUCER_VERSION` | The version of the volucer that started the process. |
| `VOLUCER_WORKER_SOCKET` | Where a `socket`-transport worker connects back — see [Worker Applications](07-WORKER-APPLICATIONS.md). |
| `VOLUCER_FCGI_ADDRESS` | The address a `spawn_cgi` slot must bind. Only read by a `command` deployment; `php-cgi` takes it as `-b`. |

`VOLUCER_VERSION` exists because a `php-cgi` per-request process or a supervised origin has no
channel to ask the server what it is. Nothing in the SDK reads it; a worker's handshake already
reports its own version.

`VOLUCER_FCGI_ADDRESS` and the `{address}` placeholder carry the same string, so a `spawn_cgi`
`command` may take its bind address either way — as an argument or from the environment. Both
are written *after* the backend's `env` table, so a configured variable cannot point the program
at an address the pool is not connecting to.

### `front_controller`

The script a path that names no file is handed to, with `PATH_INFO` set. This is what
keeps a framework working: `/users/42` reaches `index.php` rather than being a 404.

Set it to an empty string on the command line (`--front-controller ''`) to turn it off
and serve only real files. An empty value cannot be written in the file, because TOML
cannot tell an empty string from an absent key.

## `[server.static_files]`

Also settable as a plain `true` or `false`, which is shorthand for `enabled`.

| Key | Type | Default | What it does |
|---|---|---|---|
| `enabled` | bool | `true` | Serve files from the document root. |
| `precompressed` | bool | `false` | Serve `file.gz` or `file.br` when the client accepts that encoding. |
| `directory_index` | bool | `true` | Serve `index.html` for a directory. |
| `dotfiles` | string | `"deny"` | `"deny"` or `"allow"`. |
| `cache_control` | string | `"public, max-age=3600"` | The `Cache-Control` header on served files. |
| `max_multirange_bytes` | integer | `1048576` (1 MiB) | The largest total a multi-range request may ask for. |

**The static layer is on by default.** You do not need to write this section to serve
files.

**But writing the table is not the same as leaving it out.** The table states overrides,
and a table that does not say `enabled = true` does not turn the layer on — it leaves the
inherited setting alone. So this turns it *off*:

```toml
[server.static_files]
enabled = false
```

And this says nothing about whether it runs, while still changing the other keys:

```toml
[server.static_files]
cache_control = "public, max-age=60"
```

A bare `static_files = false` is the one-key opt-out. A bare `static_files = true` turns
it on and leaves everything else inherited.

`dotfiles = "deny"` answers **404**, not 403, matching how nginx treats a hidden file by
default. Which files exist is not something a client needs to learn.

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

A relative certificate or key path is resolved against the directory holding the
configuration file, not the process's working directory. `volucer check` prints the path
it resolved.

See [TLS and ACME](05-TLS-AND-ACME.md) for the three deployment shapes and what
`plain` does.

## `[server.acme]`

| Key | Type | Default | What it does |
|---|---|---|---|
| `enabled` | bool | `true` | Issue certificates. |
| `authority` | string | `"staging"` | `"staging"` or `"production"`. |
| `domains` | array of strings | derived from the site's hosts | The names to issue for. |
| `contact` | array of strings | absent | Addresses the CA warns about failing renewals. |
| `cache` | path | `"acme-cache"` | Where issued certificates are kept, relative to the configuration file. |

The staging authority is the default because a production authority has rate limits, and
a first run against it can exhaust them. Switch to `production` once the configuration is
known to be right.

`enabled = false` keeps the table and its settings and issues nothing, which is how you
stage a contact address or park an order without deleting anything.

## `[server.access_log]`

| Key | Type | Default | What it does |
|---|---|---|---|
| `path` | path | **required** | Where the log is written. |
| `format` | string | `"combined"` | `"combined"` or `"json"`. |
| `request_id_header` | string | `"X-Request-Id"` | The header to adopt a request id from. |

`combined` is nginx's shape with the request and origin timings appended. `json` is one
object per line.

Set `request_id_header` to an empty string to always generate an id and never adopt one.

## `[server.proxy]`

| Key | Type | Default | What it does |
|---|---|---|---|
| `trusted_subnets` | array of strings | empty | The peers whose forwarded headers are believed. |
| `trusted_headers` | array of strings | empty | The headers to read, in order. |

Without this, every request appears to come from your reverse proxy, and a per-client rate
limit throttles the proxy instead of the clients.

## `[server.rate_limit]`

| Key | Type | Default | What it does |
|---|---|---|---|
| `rate` | integer | **required** | Requests allowed per interval. |
| `interval_ms` | integer | `1000` | The window the rate applies to. |
| `key` | string | `"ip"` | `"ip"`, `"global"` or `"header"`. |
| `header` | string | absent | The header to key on, when `key = "header"`. |
| `burst` | integer | `rate` | How many may be spent at once. |
| `max_entries` | integer | `10000` | Identities tracked before new ones are refused. |

`key = "ip"` buckets by client address **as `[server.proxy]` resolves it**, so behind nginx
you still get per-client limits.

`key = "header"` is **not** an authenticated identity. A client chooses that value, so a
client that varies it gets a fresh bucket per request. Use a value the *proxy* sets — an
API key it vouched for, a tenant it routed.

## `[[server.routes]]`

| Key | Type | Default | What it does |
|---|---|---|---|
| `path_prefix` | string | absent | Match paths starting with this. |
| `path_suffix` | string | absent | Match paths ending with this. |
| `host` | string | absent | Match this `Host` header. |
| `policy` | string | `"all"` | `"all"`, `"backend"` or `"static"`. |
| `backend` | string | absent | The backend for matched requests. Required unless `policy = "static"`. |

Routes belong to the unnamed site, so they are written as `[[server.routes]]` next to
`[server]`'s other keys. A `[sites.<name>]` table states its own `routes`, which **replaces**
this list rather than adding to it.

**Before 0.3.0 these were `[[routes]]` and `default_backend` at the top of the file.** Both
moved into `[server]`; the old spelling is refused at startup with a message naming the new
one, so an upgrade that misses it stops with a sentence rather than an unknown-field error.

## `[sites.<name>]`

A site is one application, identified by the `Host` header. `hosts` and `document_root` are
required; everything else is inherited from `[server]` when you leave it out.

| Key | Type | Default | What it does |
|---|---|---|---|
| `hosts` | array of strings | **required** | The host names this site answers to. |
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

## `[backends.<name>]`

A backend is one of five shapes, chosen by `kind`. See
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
| `checksum` | string | `"required"` | `"required"` or `"optional"`. |
| `transport` | string | platform default | `"pipe"` or `"socket"`. |
| `socket` | string | absent | Where the socket transport listens. |

`workers = 0` means one worker per logical CPU.

`transport` defaults to `"socket"` on Windows and `"pipe"` everywhere else. On Windows,
PHP's stream layer takes about 8 ms to return from a blocking read on a pipe and about
0.09 ms on a socket, so the default is the one each platform is fast on. The choice is
logged at startup, because the transport is invisible on the wire — a pool on the wrong one
answers every request with a `200`, only slower.

`socket` is a **prefix**, and `unix:` is required: each worker gets
`<prefix>.<slot>.sock`. A bare path is relative, so a value copied from an `external`
backend would bind a file named `unix:/run/php/php8.3-fpm.sock` in the working directory
instead of failing. `socket` beside `transport = "pipe"` is refused rather than ignored,
because a key that is read by nothing looks supported and is not.

### `kind = "attach"`

The same protocol as `worker`, but volucer dials a relay somebody else listens on
instead of starting processes.

| Key | Type | Default | What it does |
|---|---|---|---|
| `workers` | integer | **required** | How many connections to hold open. |
| `relay` | string | absent | The relay address. Falls back to `VOLUCER_RELAY_SOCKET`. |
| `max_requests` | integer | absent | Recycle a slot after this many requests. |
| `checksum` | string | `"required"` | `"required"` or `"optional"`. |
| `services` | array of strings | empty | The services this backend's workers may call. |
| `grpc_services` | array of strings | empty | The gRPC services this backend may serve. |

### `kind = "external"`

An already-running FastCGI server — `php-fpm`, or any other.

| Key | Type | Default | What it does |
|---|---|---|---|
| `socket` | string | **required** | The FastCGI address, e.g. `unix:/run/php/php8.3-fpm.sock` or `127.0.0.1:9000`. |
| `max_connections` | integer | absent | How many connections to keep open to the pool. |

`external` has no `workers` key, and that is the point: the pool is somebody else's. Its
size lives in `pm.max_children` or `PHP_FCGI_CHILDREN`, in configuration volucer has no
access to.

Nothing here is PHP-specific — it is a socket and a protocol, so a FastCGI-speaking WSGI,
Rack or Go server is the same backend.

### `kind = "spawn_cgi"`

FastCGI processes started by volucer — `php-cgi`, or any program that serves FastCGI. The
only per-request-reset option on Windows.

Exactly one of `binary` and `command`:

| Key | Type | Default | What it does |
|---|---|---|---|
| `binary` | path | — | The `php-cgi` executable. The `PHP_FCGI_*` driver. |
| `command` | string | — | A command that starts **one slot**, with `{address}` where it must bind. |
| `workers` | integer | `0` | How many slots. `0` means one per logical CPU. |
| `children` | integer | `0` | `php-cgi` children behind each slot (`binary` only). `0` means no prefork. |
| `capacity` | integer | `0` | Requests one slot serves at once (`command` only). `0` means one. |
| `working_dir` | path | absent | Working directory for each started process. |
| `bind_host` | string | `"127.0.0.1"` | Where the slots listen. Loopback only. |
| `base_port` | integer | `19001` | The first port, when not using a socket. |
| `socket` | string | absent | A socket prefix instead of TCP ports. |
| `max_requests` | integer | absent | Recycle a slot after this many requests. |
| `env` | table | empty | Extra environment variables for every slot. |

`children > 0` turns on prefork, and it is worth about 4.5× the memory for the same
concurrency: forked children inherit both the listening socket and the OPcache segment, so
the compiled scripts exist once. `children` counts children, **not** processes —
`php-cgi` spawns them in addition to itself, so `workers × (children + 1)` is the process
count on the host. `children = 1` is therefore mapped to `0` and stays a truthful alias
for the default.

`command` is the non-PHP shape. `{address}` is replaced in every argument with the address
that slot must bind, and the same string is exported as `VOLUCER_FCGI_ADDRESS`. A command
using neither is refused at startup, because it would bind somewhere the pool cannot reach.
`capacity` — not `children` — says how many requests one slot answers at once, which is the
number your application server was told to fork; setting both keys is refused.

```toml
[backends.py]
kind = "spawn_cgi"
command = "gunicorn myapp.wsgi --bind {address}"
workers = 4
capacity = 4
working_dir = "/srv/app"
```

`socket` is a prefix too, and it means a unix socket on Linux and macOS and a **named
pipe** on Windows, because that is what `php-cgi -b` binds there. `\\.\pipe\volucer-cgi`
gives slot 0 the pipe `\\.\pipe\volucer-cgi.0`. A `command` slot gets a `unix:` address and
may not use a pipe.

### `kind = "upstream"`

An HTTP origin that owns its own HTTP loop — Node, ASGI, Go, Python, Ruby.

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

`restart = "on_failure"` is the default because a process that received `SIGTERM` and
exited `0` has been shut down by somebody, and starting it again would fight the
operator. A process that died on an uncaught exception exited non-zero, and that is what
this is for.

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

`buckets` on a counter or a gauge is refused. A setting that is read by nothing looks
supported and cannot work.

## `[status]`

| Key | Type | Default | What it does |
|---|---|---|---|
| `address` | string | **required** | Where the status endpoint listens. |
| `unavailable_status_code` | integer | `503` | The code returned when the server is not ready. |
| `allow_worker_reset` | bool | `false` | Expose the worker-control routes. |

**`allow_worker_reset` has no authentication.** What protects those routes is that they do
not exist until the flag is set — `404` on the path, not `403` — and that a wildcard
`address` with the flag on is refused at startup. An unauthenticated `POST` that recycles
workers, reachable from anywhere, is a remote kill switch. Put `[status]` on loopback or
a private interface and reach it through your orchestrator's network, or leave the flag
off.

`volucer health` reads `address` from this table, so a probe does not have to repeat the port
— including in a container, where a wildcard `address` is normal and is probed on loopback.
See [Operations](11-OPERATIONS.md#health-checks).

## `[telemetry]`

| Key | Type | Default | What it does |
|---|---|---|---|
| `endpoint` | string | **required** | The collector. `/v1/traces` is appended. |
| `service_name` | string | `"volucer"` | The `service.name` resource attribute. |
| `headers` | table | empty | Extra headers, e.g. for a hosted collector. |
| `timeout_ms` | integer | `10000` | Per export request. |
| `batch_size` | integer | `512` | Spans per batch. |
| `interval_ms` | integer | `5000` | How long a partly-filled batch waits. |

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

Redis is only available in a build compiled with the `redis` feature. A build without it
cannot instantiate a Redis storage, and says so rather than accepting the configuration
and failing later.

## `[lock]`

| Key | Type | Default | What it does |
|---|---|---|---|
| `driver` | string | **required** | `"memory"` or `"redis"`. |

`driver = "redis"` also takes `url` (required), `pool_size` (default `16`) and
`read_timeout_ms` (default `5000`).

## `[jobs]`

| Key | Type | Default | What it does |
|---|---|---|---|
| `consume` | array of strings | empty | The pipelines this server consumes from. |
| `pipelines` | table of tables | empty | The named pipelines. |
| `pool` | table | absent | The consumer pool. Required when `consume` is not empty. |

`[jobs.pipelines.<name>]` takes the same `driver` key as `[kv.<name>]`: `"memory"` or
`"redis"`. The Redis variant also takes `url`, `pool_size` and `read_timeout_ms`.

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
