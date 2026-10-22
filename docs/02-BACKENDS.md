# Backends

**How volucer runs your PHP, and how to choose.**

## The five kinds

A backend is one of five shapes. You pick one with `kind` in `[backends.<name>]`:

| `kind` | What it is | State between requests | Platforms |
|---|---|---|---|
| `worker` | A persistent PHP process volucer starts and supervises | The application keeps it, and clears its own request scope | all |
| `attach` | The same, but the processes were started by somebody else | The same | Unix, or a named pipe on Windows |
| `external` | An already-running `php-fpm` | Reset by PHP's own SAPI | Linux, macOS |
| `spawn_cgi` | `php-cgi` processes volucer starts | Reset by PHP's own SAPI | all — **required on Windows** |
| `upstream` | An HTTP origin: Node, Go, Python, Ruby | The origin's own business | all |

## Choosing one

Start with the question: **can your application reset its own request state?**

If yes, use `worker`. The application boots once and stays resident, which is where the
performance difference comes from — a framework boot per request is a large part of what
`php-fpm` costs.

If no, use `external` (Linux, macOS) or `spawn_cgi` (everywhere). The behaviour is exactly
what `php-fpm` gives you, so nothing about your application has to change.

If your application is not PHP at all, use `upstream`. It relays HTTP to a runtime that
already owns its own HTTP loop.

You can mix them. A route picks a backend, so one process can serve a `/api` route from a
resident worker and a `/admin` route from `php-fpm`:

```toml
default_backend = "app"

[backends.app]
kind = "worker"
command = "php worker.php"

[backends.fpm]
kind = "external"
socket = "unix:/run/php/php8.3-fpm.sock"

[[routes]]
path_prefix = "/legacy"
backend = "fpm"
```

## `worker`

A persistent PHP process that boots your application once and serves many requests.

```toml
[backends.app]
kind = "worker"
command = "php worker.php"
workers = 8
```

`command` is a command line, split without a shell. Double quotes group, and a backslash
is kept unless it escapes a quote or another backslash — so a Windows path needs no
doubling.

Each worker is started, handshaken and kept resident. `stderr` is drained on its own
thread, so a chatty application cannot fill a pipe and block.

**One worker serves one request at a time.** `workers` workers therefore give you
`workers` concurrent requests; more than that waits in the pool queue. `workers = 0`
means one worker per logical CPU.

### Recycling

A worker is replaced when any of these happen:

- it reaches `max_requests`
- the peak RSS it reports exceeds `memory_limit_mb`
- it asks to be replaced, by answering with `keep_alive = false`
- it crashes, or a request times out

An absent `max_requests` means never recycle on request count. Recycling is in place: the
pool size stays the same.

Every wait has a deadline, enforced by killing the worker. A failed request is
**not** retried, unlike the FastCGI backends — a worker failure can happen after your
application already ran, and running it twice is worse than failing.

### Transports

```toml
[backends.app]
kind = "worker"
command = "php worker.php"
transport = "socket"              # the default on Windows
socket = "unix:/run/volucer/worker"   # a PREFIX: worker i listens on ...worker.<i>.sock
```

The protocol runs over the worker's `stdin`/`stdout` by default. That is portable, needs
no port per worker, and cannot exhaust sockets.

`transport = "socket"` moves the same frames onto a socket. It exists for one measured
reason: **on Windows, PHP takes about 8 ms to return from a blocking read on a pipe,
where the same read on a socket returns in about 0.09 ms.** Rust reading the identical
pipe returns in 0.010 ms, so this is PHP's Windows stream layer rather than anything
volucer does. But it is per request rather than per connection, so it decides which
deployment model wins — on a whole route it was 21×.

**The default is therefore a socket on Windows and a pipe everywhere else.** A
configuration that never mentions the setting gets the one its platform is fast on. The
choice is logged at startup, because the transport is invisible on the wire — a pool on
the wrong one answers every request with a `200`, only slower.

`socket` is a **prefix**, and `unix:` is required. Each worker gets `<prefix>.<slot>.sock`,
which is what lets a worker be recycled, counted and reached on its own. A bare path is
*relative*, so a value copied from an `external` backend would bind a file named
`unix:/run/php/php8.3-fpm.sock` in the working directory instead of failing.

Two mis-spellings are refused at startup rather than ignored, because a key that is read
by nothing looks supported and is not: `socket` beside `transport = "pipe"`, and a
`host:port` address.

### Checksums

`checksum = "optional"` turns off the protocol's per-frame CRC-32C. The default is
`"required"`, which is what every release has done.

When it is `"optional"`, the server *offers* the option in its `HELLO` and only stops
verifying if the worker answers in its `HELLO_ACK` that it agrees. An SDK that predates
the field omits it, which decodes as "no", and the pair stays checked. The handshake
itself is always checksummed in both directions, because it is the exchange that carries
the agreement.

Setting it is worth about what one `memcpy` of the page costs per frame, which on a
request that spends its time inside your application is close to nothing. It is a knob to
measure with, not a promised speed-up.

## `attach`

The same protocol as `worker`, but the PHP processes were started by somebody else.

```toml
[backends.app]
kind = "attach"
relay = "unix:/run/app/workers.sock"
workers = 16
```

A supervisor boots the application once, forks its children, and listens on the relay.
Volucer **dials that relay** and does not start a process at all.

### Why it exists

`worker` starts every worker with `exec`, so every worker maps **its own** OPcache segment
and pays the framework boot itself. `fork()` shares both instead, and on PHP that is the
only way to share them: an `exec`ed process cannot attach to an existing OPcache segment.

The catch is that the fork has to happen in a PHP process, and a PHP process cannot hand a
file descriptor to another process — `ext/sockets` has no `SCM_RIGHTS`. So the side that
owns the socket has to be the side that owns the fork.

That is the whole shape: a PHP-side supervisor forks and listens, volucer dials.

### The relay

`relay` is the address volucer connects to. When you leave it out, volucer reads
`VOLUCER_RELAY_SOCKET` from its own environment — so the supervisor and the server can
read the same variable instead of it being passed as an argument and written twice.

`relay` is a path, not an address list: volucer dials it `workers` times and holds each
connection as a slot.

**If the relay is not there, startup fails immediately** rather than starting a backend
that cannot serve:

```
backend "app" could not connect to its relay "unix:/run/app/workers.sock": …
```

Everything else — the handshake, the per-slot exchange, the recycling rules — is the same
as `worker`.

## `external`

An already-running `php-fpm`.

```toml
[backends.fpm]
kind = "external"
socket = "unix:/run/php/php8.3-fpm.sock"
```

This is the migration path. Point volucer at the `php-fpm` you already run and the
behaviour is exactly what you have today: PHP's SAPI resets everything per request.

**Volucer does not manage the pool.** There is no `workers` key, and that is the point —
the pool is somebody else's. Its size lives in `pm.max_children` or
`PHP_FCGI_CHILDREN`, in configuration volucer has no access to. One FastCGI connection
carries one request, so the real concurrency limit is `php-fpm`'s.

`max_connections` bounds how many connections *volucer* keeps open to the pool. Leave it
out and volucer opens what it needs.

`php-fpm` recycles its own workers. `pm.max_requests` is the equivalent of the
`max_requests` key the other backends have, and it is the one that matters here.

## `spawn_cgi`

`php-cgi` processes that volucer starts and supervises.

```toml
[backends.cgi]
kind = "spawn_cgi"
binary = "C:/php-8.3.31/php-cgi.exe"
workers = 8
```

**This is the only per-request-reset option on Windows**, because Windows has no
`php-fpm` at all. `php-cgi -b` provides real per-request SAPI reset — superglobals, POST
bodies, multipart and `$_FILES`, `is_uploaded_file()` — but nothing spawns, supervises,
health-checks or recycles it. That is the gap this backend fills.

The pool starts the processes, assigns ports, health-checks each slot, restarts one that
died, and recycles on `max_requests`.

On Unix the processes are placed in their own process group; on Windows they are adopted
into a job object with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`, so nothing is left running
if volucer dies.

### Prefork

```toml
[backends.cgi]
kind = "spawn_cgi"
binary = "/usr/bin/php-cgi"
workers = 1        # one listening socket
children = 16      # 16 children behind that master, sharing its OPcache
```

`children > 0` turns on prefork, and it is worth about 4.5× the memory for the same
concurrency. `php-cgi` cannot serve two requests on one connection, so more than one
concurrent request out of a slot means more than one process. Forked children inherit
both the listening socket **and** the OPcache segment, so the compiled scripts exist
once.

**`children` counts children, not processes.** `php-cgi` spawns them in addition to
itself, so `children = 16` is 17 processes and `workers × (children + 1)` is the process
count on your host. `children = 1` would be two processes for a slot that can still only
serve one request at a time, so it is mapped to `0` and stays a truthful alias for the
default.

The pool still owns the slot as one unit: `max_requests` counts the slot, and teardown
kills the process **group** rather than the master's pid.

### Sockets

A `spawn_cgi` pool talks to its `php-cgi` processes over loopback TCP by default. `socket`
moves it off the loopback stack, and what the setting means depends on the platform: a
unix socket path on Linux and macOS, and a **named pipe** on Windows, because that is
what `php-cgi -b` binds there.

Both are a *prefix* — each slot appends its index, so `\\.\pipe\volucer-cgi` gives slot 0
the pipe `\\.\pipe\volucer-cgi.0`.

## `upstream`

An HTTP origin that owns its own HTTP loop: Node, Go, Python, Ruby.

```toml
[backends.node]
kind = "upstream"
origin = "http://127.0.0.1:3000"
```

This is deliberately not a variation on the four above: there is no CGI environment, no
FastCGI record and no worker frame, only an HTTP client. The request body streams, the
response streams, and the path is the URL.

`origin` is parsed at startup, so a typo fails there rather than becoming a 502 on the
first request:

```text
http://127.0.0.1:3000       TCP, plain text
https://10.0.0.5:8443       TCP, TLS — always verified
unix:/run/node.sock         a unix socket, no network at all
```

A base path and credentials are refused by name, because neither would be honoured: a
route prefix belongs in the route table, where it is.

### gRPC

gRPC's transport **is** HTTP/2, so a gRPC origin reached over `https://` needs nothing —
TLS negotiates the protocol through ALPN. A plaintext or `unix:` origin has no handshake
to negotiate in, and the answer is h2c: HTTP/2 with prior knowledge.

```toml
[backends.greeter]
kind = "upstream"
origin = "unix:/run/greeter.sock"
protocol = "h2c"
command = '/srv/greeter/greeter --listen /run/greeter.sock'
```

`protocol = "h2c"` is the only value, and absent means HTTP/1.1. Beside an `https://`
origin it is **refused**, because there the handshake answers the question.

Volucer never parses a gRPC message. It routes on the path, carries the framed bytes,
and puts the application's `grpc-status` into the trailer section. `.proto` files never
enter this server's build, so a schema change is an application deploy rather than a
server one.

### Supervising the origin

An `upstream` origin can be **volucer's process** rather than systemd's, which is what
makes a Python or Ruby application runnable behind volucer without a new protocol:

```toml
[backends.django]
kind = "upstream"
origin = "unix:/run/app.sock"
command = 'gunicorn myapp.wsgi --bind unix:/run/app.sock --workers 4'
working_dir = '/srv/app'
ready_timeout_ms = 30000
```

| Key | Default | Why it exists |
|---|---|---|
| `command` | absent — the origin is somebody else's process | Start it, and make it volucer's to stop |
| `working_dir` | the server's own | `manage.py` and `config.ru` are found relative to it |
| `env` | empty | `DJANGO_SETTINGS_MODULE`, `RAILS_ENV` |
| `restart` | `"on_failure"` | A `gunicorn` that exited `0` was stopped by somebody; restarting it fights them |
| `restart_limit` | `3` | A command that cannot start exits in milliseconds — a limit, not a loop |
| `restart_backoff_ms` | `1000` | A fixed delay is either a hammer or an outage |
| `ready_timeout_ms` | `30000` | The readiness gate; without it a deploy is a burst of 502s from an origin still booting |

**The readiness gate is inside startup.** Volucer does not begin serving until the origin
accepts a connection, so the first request after a deploy is not a 502.

**A command that cannot start is a startup failure, not a timeout.** A program that does
not exist is reported by name before anything is spawned.

**`restart_limit` counts *consecutive* failures**, not a lifetime budget. An origin that
stayed up a minute resets the count.

### What is different about an upstream

- **The request body streams and has no size cap.** `server.max_body_bytes` protects the
  CGI path, where the environment must carry `CONTENT_LENGTH` before `php-cgi` will read
  a byte. An upstream is handed the body unread, so **the limit to configure is the
  origin's.**
- **The response streams too**, which is what makes SSE and a large download work.
- **Reachable over HTTP only.** There is no CGI environment for one, so an upstream with
  no `server.http` listener is a configuration error.
- **Backends are restart-only** on reload. See [Operations](11-OPERATIONS.md).
- **WebSockets are not supported.** SSE works; a WebSocket does not.

## TLS to an upstream origin

An `https:` origin is always verified, and there is no `verify = false`.

```toml
[backends.node]
kind = "upstream"
origin = "https://127.0.0.1:3000"
tls = { ca = "/etc/ssl/internal-ca.pem" }   # absent = the public roots
```

`tls.ca` is a PEM bundle; `tls.sni` overrides the name presented in the handshake, which
defaults to the origin's host.

An unverified TLS hop is nginx's classic footgun, because the default there is off. It is
strictly worse than plain text: it *looks* encrypted and checks nothing. A deployment that
genuinely wants that shape points the origin at `http://` on a loopback port, which is
honest about what the hop is.

## The `kind` you cannot use

`kind = "spawn_fpm"` is **rejected by name**. It was never implemented, and it is gone
rather than built: `php-fpm` already does pool management, recycling, multi-pool tenancy
and graceful reload better than a supervisor volucer would write. Use `external`, which
attaches to the `php-fpm` that systemd already runs.
