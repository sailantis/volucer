# Backends

Backends connect Volucer to the application that handles a request. Choose the backend
that matches how your application runs: as a persistent Volucer worker, a FastCGI service,
or an HTTP server. PHP, Python, Ruby, and other runtimes can all be served through the
appropriate backend.

## The five kinds

Set `kind` in `[backends.<name>]` to choose one of five backend types:

| `kind` | What it is | State between requests | Platforms |
|---|---|---|---|
| `worker` | A persistent process volucer starts and supervises, speaking volucer's protocol | The application keeps it, and clears its own request scope | all |
| `attach` | The same, but the processes were started by somebody else | The same | Unix, or a named pipe on Windows |
| `external` | An already-running FastCGI server (`php-fpm`, or anything else) | Reset by the backend, per request | Linux, macOS |
| `spawn_cgi` | FastCGI processes volucer starts — `php-cgi`, or any command | Reset by the backend, per request | all — **required on Windows** |
| `upstream` | An HTTP origin: Node, Go, Python, Ruby | The origin's own business | all |

## Choosing one

Start by asking whether your application can reset its request state between requests.

If it can, use `worker`. The application starts once and remains resident, avoiding a
framework boot for every request.

If it cannot, use `external` on Linux or macOS, or `spawn_cgi` on any supported platform.
These backends preserve the per-request reset behavior provided by `php-fpm`; your
application does not need to change.

FastCGI is a protocol, not a PHP-specific feature. Both FastCGI backends can also serve
WSGI, Rack, Go, or other applications that speak FastCGI. Request-state reset is provided
by the application server: PHP's SAPI for `php-cgi` and `php-fpm`, or the relevant
framework for a `spawn_cgi` `command`.

If your application already serves HTTP, use `upstream` to relay requests to it.

Backends can be combined in one Volucer instance. Routes select the backend for each
request; for example, the default can use a persistent worker while `/legacy` uses
`php-fpm`:

```toml
[server]
default_backend = "app"

[backends.app]
kind = "worker"
command = "php worker.php"

[backends.fpm]
kind = "external"
socket = "unix:/run/php/php8.3-fpm.sock"

[[server.routes]]
path_prefix = "/legacy"
backend = "fpm"
```

## `worker`

A persistent process that starts your application once and serves multiple requests.

```toml
[backends.app]
kind = "worker"
command = "php worker.php"
workers = 8
```

`command` is split into arguments without invoking a shell. Double quotes group
arguments. A backslash is preserved unless it escapes a quote or another backslash, so
Windows paths do not need doubled backslashes.

Volucer starts each worker, completes the protocol handshake, and keeps it running.
Worker `stderr` is drained on a separate thread so verbose output cannot fill a pipe and
block the process.

Each worker handles one request at a time. The pool can therefore serve up to
`workers` requests concurrently; additional requests wait in the queue. `workers = 0`
starts one worker per logical CPU.

### Recycling

A worker is replaced when any of the following occurs:

- it reaches `max_requests`
- the peak RSS it reports exceeds `memory_limit_mb`
- it asks to be replaced, by answering with `keep_alive = false`
- it crashes, or a request times out

If `max_requests` is omitted, the worker is not recycled based on request count.
Replacement happens in place, so the pool keeps its configured size.

Each wait has a deadline; Volucer enforces it by terminating the worker. Failed worker
requests are not retried. A failure can occur after the application has already processed
the request, so retrying could run the same operation twice.

### Transports

```toml
[backends.app]
kind = "worker"
command = "php worker.php"
transport = "socket"              # the default on Windows
socket = "unix:/run/volucer/worker"   # a PREFIX: worker i listens on ...worker.<i>.sock
```

By default, the protocol uses the worker's `stdin` and `stdout`. This portable transport
needs no port for each worker and avoids exhausting sockets.

`transport = "socket"` sends the same protocol frames over a socket. This option addresses
a measured Windows performance difference: PHP takes about 8 ms to return from a
blocking pipe read, compared with about 0.09 ms for a socket read. Rust takes about
0.010 ms to read the same pipe, indicating that the difference is in PHP's Windows
stream layer. Because the protocol reads once per request, this can affect overall
performance; one route measured a 21× difference.

The default is a socket on Windows and a pipe on other platforms. Volucer selects the
transport that performed better in testing for each platform and logs the choice at
startup. Both transports use the same protocol, so a mismatched choice can still serve
requests, but more slowly.

`socket` is a prefix and must begin with `unix:`. Each worker uses
`<prefix>.<slot>.sock`, allowing Volucer to manage and address each slot independently.
A bare path is interpreted as relative; copying an `external` backend's socket value
without its `unix:` prefix would create a relative path instead of reporting an error.

Volucer rejects `socket` when `transport = "pipe"` and rejects `host:port` values for
this setting. These values would otherwise appear to be accepted without affecting the
worker transport.

### Checksums

`checksum = "optional"` allows the protocol to omit per-frame CRC-32C checks. The default
is `"required"`, preserving the behavior used by every release.

With `"optional"`, Volucer advertises the option in `HELLO` and disables frame checksums
only if the worker agrees in `HELLO_ACK`. Older SDKs omit this field, which is treated
as a refusal, so checksums remain enabled. The handshake is always checksummed in both
directions.

The potential saving is roughly the cost of one page-sized `memcpy` per frame and may be
negligible for requests dominated by application work. Treat this as an option to measure,
not a guaranteed optimization.

## `attach`

`attach` uses the same protocol as `worker`, but connects to processes started by another
supervisor.

```toml
[backends.app]
kind = "attach"
relay = "unix:/run/app/workers.sock"
workers = 16
```

A supervisor starts the application, forks its children, and listens on the relay.
Volucer connects to the relay; it does not start these processes.

### Why it exists

The `worker` backend starts each worker with `exec`. Each process therefore has its own
OPcache segment and performs its own framework boot. With `fork()`, children can share
both; for PHP, forking is the way to share an existing OPcache segment because an
`exec`ed process cannot attach to one.

The fork must happen in a PHP process. PHP cannot pass the listening file descriptor to
another process because `ext/sockets` does not support `SCM_RIGHTS`. As a result, the
process that forks the workers must also own the listening socket.

In this setup, a PHP-side supervisor forks the workers and listens on the relay; Volucer
dials that relay.

### The relay

`relay` is the address Volucer connects to. If omitted, Volucer reads
`VOLUCER_RELAY_SOCKET` from its environment. The supervisor and Volucer can therefore
share the same setting without duplicating it in command-line arguments.

`relay` is a single path, not a list of addresses. Volucer connects to it `workers` times
and uses each connection as a pool slot.

If the relay is unavailable, startup fails immediately instead of starting a backend
that cannot accept requests:

```
backend "app" could not connect to its relay "unix:/run/app/workers.sock": …
```

The handshake, per-slot exchange, and recycling behavior are otherwise the same as for
`worker`.

### A PHP that loads `grpc` must be started with `GRPC_ENABLE_FORK_SUPPORT=1`

This requirement applies to the supervisor process. If it is missed, the pool can stall
after workers are recycled.

When the `grpc` module initializes, it starts executor, resolver, and timer threads. If
the supervisor forks after initialization, the child inherits their locks and can
deadlock during module shutdown on `exit()`. The child does not exit, so `SIGCHLD` is not
delivered and the supervisor cannot reap it. A supervisor with a child limit may
eventually stop accepting workers. In Volucer, this can appear as a slot that stops
responding after several recycles.

In testing, CLI PHP with `grpc` loaded produced nine stuck children after eight recycles;
setting the environment variable prevented the issue. It must be set in the environment
before PHP starts. Using `-d grpc.enable_fork_support=1` or calling `putenv()` from PHP
is too late: the module starts its threads before the script runs.

```ini
# worker.service, or your container's environment
Environment=GRPC_ENABLE_FORK_SUPPORT=1
```

`Volucer\Supervisor::serve()` warns if it detects this configuration, but the warning
does not resolve the issue because the threads exist before PHP code runs.

## `external`

`external` connects to an existing FastCGI server.

```toml
[backends.fpm]
kind = "external"
socket = "unix:/run/php/php8.3-fpm.sock"
```

Use this backend to connect Volucer to an existing `php-fpm` service. PHP's SAPI
continues to reset request state for each request.

This backend is not PHP-specific. It connects over a socket using FastCGI, so it can also
connect to a Gunicorn server with a FastCGI adapter, a FastCGI-speaking `uwsgi`, or a Rack
server behind `flup`. The relevant settings are `socket` and `max_connections`.

Volucer does not manage this server's process pool, so there is no `workers` setting.
Configure pool size on the server side, for example with `pm.max_children` or
`PHP_FCGI_CHILDREN`. Each FastCGI connection handles one request, so concurrency is
limited by the remote pool.

`max_connections` limits how many connections Volucer keeps open to the pool. If omitted,
Volucer opens connections as needed.

`php-fpm` manages worker recycling itself. Its `pm.max_requests` setting serves the same
purpose as `max_requests` on other backends.

## `spawn_cgi`

`spawn_cgi` starts and supervises FastCGI processes.

Specify exactly one of `binary` or `command`. Use `binary` for `php-cgi`; use `command`
for another program that serves FastCGI.

```toml
[backends.cgi]
kind = "spawn_cgi"
binary = "C:/php-8.3.31/php-cgi.exe"
workers = 8
```

On Windows, `spawn_cgi` is the available backend for per-request reset because `php-fpm`
is not available there. `php-cgi -b` provides PHP SAPI request isolation, including
superglobals, POST bodies, multipart uploads, `$_FILES`, and `is_uploaded_file()`.
Volucer supplies the process management that `php-cgi -b` does not: starting, supervising,
health-checking, and recycling the processes.

The pool starts each process, assigns its address, checks its health, restarts it if it
exits, and recycles it according to `max_requests`.

On Unix, processes run in their own process group. On Windows, they are assigned to a job
object using `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`. This ensures the processes are
terminated if Volucer exits.

### Any FastCGI server

```toml
[backends.py]
kind = "spawn_cgi"
command = "gunicorn myapp.wsgi --workers {concurrency} --bind {address}"
workers = 4
concurrency = 4        # substituted above: one slot is one address, and it serves four
working_dir = "/srv/app"
env = { APP_ENV = "production" }
```

`{address}` identifies the address on which a slot must listen. Volucer assigns one
address per slot so it can manage each process independently. It substitutes the slot's
address (for example, `127.0.0.1:19001` or `unix:/run/app/py.0.sock`) into command
arguments and also exports it as `VOLUCER_FCGI_ADDRESS`. A command must use one of these
mechanisms; otherwise, it would listen on a different address and requests would fail
after the startup timeout.

For a `command` slot, Volucer provides the same lifecycle management as for `php-cgi`:
starting, readiness checks by connecting, restarting on exit, recycling according to
`max_requests`, and termination as a process group. Standard output and error are
captured in the log, where application diagnostics can help explain request failures.

Use `concurrency` with `command` and `children` with `binary`; set only one. `children`
controls the `PHP_FCGI_CHILDREN` prefork count for `php-cgi`. For another FastCGI server,
`concurrency` tells Volucer how many requests a single slot can handle at once. Set it
below the server's actual capacity and throughput is unnecessarily limited. This value
is per slot, so the backend's maximum concurrency is `workers × concurrency`.

Volucer runs the command once per slot, supplying a different `{address}` each time.
Processes started by that command serve that slot's address. For example,
`--workers 4` corresponds to `concurrency = 4`; with `workers = 4`, Volucer starts four
Gunicorn instances, for sixteen worker processes total. Threaded servers multiply
concurrency further. `volucer check` reports the process total on its `processes:` line.

Use `{concurrency}` in the command to keep its worker count aligned with the pool setting.
Without it, changing `concurrency` but not the command can leave Volucer holding more
connections than the server can handle; excess requests then wait in the socket backlog.
The placeholder is replaced with the resolved value. For example, `concurrency = 0`
defaults to one, so the command receives `1`, not `0`. `volucer check` displays the
resulting command line.

`{workers}` is not a supported placeholder and is rejected. Each slot is started
independently with its own address, so a slot has no sibling count to pass to the program.
Other brace expressions are left unchanged; for example, `--define={a:1}` is preserved.

You can also write the worker count directly, such as `--workers 4` with
`concurrency = 4`. This works the same way but requires keeping the two values in sync.

The distinction between `binary` and `command` affects three other settings. These
differences come from `php-cgi`, not from the pool:

| | `binary` | `command` |
| --- | --- | --- |
| how the address is given | `-b <address>` | `{address}` and `VOLUCER_FCGI_ADDRESS` |
| concurrency inside a slot | `children` → `PHP_FCGI_CHILDREN` | `concurrency` |
| a unix socket on Windows | a named pipe | TCP only |

A `command` process that exits is reported once and is not retried on another address.
The retry behavior for `php-cgi` exists because it can exit after losing a bind race; that
assumption does not apply to arbitrary commands.

**Two settings are PHP-shaped by default, and a non-PHP deployment sets them explicitly.**
`front_controller` defaults to `index.php` and `script_extensions` to `php`, `php3`,
`phtml` — both server-level, and both meaningful only as a starting point:

```toml
[server]
front_controller = "app.py"        # your entry point, or "" for only real files
script_extensions = ["py"]
```

For a non-PHP application, set these server-level options to match its entry point and
script extensions. Otherwise, `volucer check` may warn that `index.php` is missing, and a
request for `/app.py` may be treated as `PATH_INFO` for the front controller rather than
as a script.

### Prefork

```toml
[backends.cgi]
kind = "spawn_cgi"
binary = "/usr/bin/php-cgi"
workers = 1        # one listening socket
children = 16      # 16 children behind that master, sharing its OPcache
```

Set `children > 0` to enable prefork. This uses about 4.5× the memory for the same
concurrency. Since `php-cgi` handles one request per connection, concurrent requests
from a slot require multiple processes. Forked children inherit the listening socket and
OPcache segment, so compiled scripts can be shared.

`children` counts child processes in addition to the `php-cgi` master. Thus,
`children = 16` starts 17 processes per slot, and the total is
`workers × (children + 1)`. A value of `children = 1` would start two processes while
still serving only one request at a time, so Volucer maps it to `0`, the default.

Volucer manages the slot as a unit: `max_requests` applies to the slot, and shutdown
terminates the entire process group rather than only the master process.

### Sockets

By default, a `spawn_cgi` pool connects to its processes over loopback TCP. The `socket`
setting selects a local socket instead: a Unix socket path on Linux and macOS, or a named
pipe on Windows, which is the mechanism `php-cgi -b` supports there.

The value is a prefix; Volucer appends each slot's index. For example,
`\\.\pipe\volucer-cgi` becomes `\\.\pipe\volucer-cgi.0` for slot 0.

For a `command` slot, Volucer provides the address in `unix:` form (for example,
`unix:/run/app/py.0.sock`) as both the `{address}` substitution and
`VOLUCER_FCGI_ADDRESS`. Named pipes are not supported for `command`; they are specific to
`php-cgi -b` on Windows.

## `upstream`

`upstream` forwards requests to an HTTP server that manages its own request loop, such as
an application built with Node, Go, Python, or Ruby.

```toml
[backends.node]
kind = "upstream"
origin = "http://127.0.0.1:3000"
```

Unlike the other backend types, `upstream` uses HTTP directly. There is no CGI
environment, FastCGI record, or worker protocol. Request and response bodies are
streamed, and the request path is forwarded as part of the URL.

Volucer parses `origin` at startup, so invalid values are reported before serving
requests:

```text
http://127.0.0.1:3000       TCP, plain text
https://10.0.0.5:8443       TCP, TLS — always verified
unix:/run/node.sock         a unix socket, no network at all
```

Base paths and credentials are rejected because they are not applied. Configure path
prefixes in the route table instead.

### gRPC

gRPC uses HTTP/2 **on the hop to the origin**. For an `https://` origin, TLS negotiates HTTP/2
through ALPN, so no additional protocol setting is needed. A plaintext or `unix:` origin has no
TLS handshake; use h2c (HTTP/2 with prior knowledge) instead.

```toml
[backends.greeter]
kind = "upstream"
origin = "unix:/run/greeter.sock"
protocol = "h2c"
command = '/srv/greeter/greeter --listen /run/greeter.sock'
```

`protocol = "h2c"` is the only supported value; if omitted, Volucer uses HTTP/1.1, which
cannot carry a gRPC call. Volucer rejects this setting with an `https://` origin because TLS
negotiation determines the protocol there.

**That setting, not the client's own version, decides the origin hop.** A gRPC call that arrives
over HTTP/1.1, HTTP/2 or HTTP/3 reaches an h2c or ALPN-`h2` origin as HTTP/2 and an ordinary
origin as HTTP/1.1, so a call to an origin that speaks only HTTP/1.1 is a misconfiguration this
surfaces rather than silently works around.

The **client-facing** hop is independent of that setting and may be HTTP/1.1, HTTP/2 or
HTTP/3. A gRPC call's outcome travels in the response trailer section on every one of them,
so `grpc-status` reaches the client whichever transport it chose. `[server.http3]` is
therefore not a gRPC-specific option; it enables QUIC for all traffic.

Volucer does not parse gRPC messages. It routes by path, forwards the framed bytes, and
passes the application's `grpc-status` in the response trailers. `.proto` files are not
part of the server build, so schema changes require an application deployment, not a
Volucer rebuild.

### Supervising the origin

Volucer can also start and supervise the process that serves an upstream. This lets a
Python or Ruby application run behind Volucer without implementing another protocol:

```toml
[backends.django]
kind = "upstream"
origin = "unix:/run/app.sock"
command = 'gunicorn myapp.wsgi --bind unix:/run/app.sock --workers 4'
working_dir = '/srv/app'
ready_timeout_ms = 30000
```

| Key | Default | Purpose |
|---|---|---|
| `command` | absent; the origin is managed externally | Start the origin and let Volucer stop it |
| `working_dir` | Volucer's working directory | Resolve paths such as `manage.py` and `config.ru` |
| `env` | empty | Set application variables such as `DJANGO_SETTINGS_MODULE` or `RAILS_ENV` |
| `restart` | `"on_failure"` | Avoid restarting a process that exited successfully because it was intentionally stopped |
| `restart_limit` | `3` | Limit retries when a command repeatedly fails to start |
| `restart_backoff_ms` | `1000` | Set the delay between restart attempts |
| `ready_timeout_ms` | `30000` | Wait for the origin to accept a connection before Volucer starts serving |

Volucer waits for the origin to accept a connection before completing startup. This
prevents requests from being sent to an origin that is still initializing.

If a command cannot be started, Volucer reports the startup error directly rather than
waiting for the readiness timeout. For example, a missing executable is reported by name.

`restart_limit` counts consecutive failures rather than lifetime restarts. A successful
run resets the count.

### What is different about an upstream

- Request bodies stream without a Volucer size limit. `server.max_body_bytes` applies to
  the CGI path, where the request length must be known before `php-cgi` reads the body.
  Configure request limits in the upstream application.
- Response bodies also stream, supporting server-sent events (SSE) and large downloads.
- Upstreams require an HTTP listener. A configuration with an upstream but no
  `server.http` listener is invalid.
- Backends are restart-only during reload. See [Operations](11-OPERATIONS.md).
- WebSockets are not supported; SSE is supported.

## Large responses: what each backend does

A 1 GB download does not behave the same way on all five backends, and the difference is worth
knowing before choosing one for an application that serves files.

| Backend | Request body | Response body | A 1 GB download |
|---|---|---|---|
| `upstream` | streams | streams | ✅ forwarded as it arrives |
| `spawn_cgi` | buffered | relayed above `stream_threshold_bytes` | ✅ 64 KiB records, relayed |
| `external` (php-fpm) | buffered | relayed above `stream_threshold_bytes` | ✅ 64 KiB records, relayed |
| `worker` | buffered | buffered | ❌ the whole response in memory |
| static files | n/a | streams | ✅ 64 KiB reads, ranges supported |

The two FastCGI kinds **relay** a response larger than
[`server.stream_threshold_bytes`](01-CONFIGURATION.md#stream_threshold_bytes): the header block is
answered at once and each subsequent FastCGI record is forwarded as it arrives, so the peak memory
is one record rather than two copies of the response.

**`worker` mode still buffers**, because its protocol frames a response with a declared length before
the body. That is the next change, and until it lands an application in worker mode should either use
`X-Sendfile` for files it holds (see [Static Files](04-STATIC-FILES.md#x-sendfile)) or be served on a
FastCGI backend.

`X-Sendfile` is the best answer for a download that is a *file*, on any backend: the static layer
serves it with ranges and validators, and costs 64 KiB whatever the size. Relaying is for a body the
application generates — a CSV export, a log, a media file it streams itself.

## TLS to an upstream origin

HTTPS upstreams are always certificate-verified. There is no `verify = false` option.

```toml
[backends.node]
kind = "upstream"
origin = "https://127.0.0.1:3000"
tls = { ca = "/etc/ssl/internal-ca.pem" }   # absent = the public roots
```

`tls.ca` specifies a PEM bundle; if omitted, Volucer uses the public root certificates.
`tls.sni` overrides the server name sent in the TLS handshake and defaults to the
origin's host.

Disabling certificate verification would encrypt traffic without verifying the server's
identity. If an internal deployment intentionally does not use TLS verification, use an
`http://` origin over a trusted local connection rather than an apparently secure but
unverified TLS connection.

## The `kind` you cannot use

`kind = "spawn_fpm"` is rejected because this backend is not implemented. Volucer does
not manage `php-fpm` processes; `php-fpm` already provides pool management, recycling,
multiple pools, and graceful reloads. Use `external` to connect to an existing
`php-fpm` service.
