# Changelog

All notable changes to volucer are documented here.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **`examples/docker/`: two complete, CI-verified image recipes.** `Dockerfile.ubuntu` (294 MB)
  and `Dockerfile.alpine` (98 MB) build a `worker`-ready image with PHP 8.5, Composer and the
  published binary. `verify.sh` starts the image, waits for its health check, and serves a real
  request, so a package renamed upstream or a `COPY` whose source moved fails a pull request
  rather than a user's first `docker run`. A new `Examples` job in `ci.yml` runs it against both.

### Changed

- **The documented PHP image recipe is Ubuntu, not the official `php` image.** The official
  `php:8.5-cli-bookworm` keeps a 330 MB compiler toolchain so that `docker-php-ext-install`
  works, and leaves a PHP compiled with its headers in place; a `worker` image built on it is
  819 MB. The same image on `ubuntu:26.04` + `php8.5-cli` is 229 MB, and the full recipe with
  `mbstring`, `curl`, `xml`, `sqlite3`, Composer and the application is 294 MB — about a third,
  and 71 MB instead of 189 MB to pull. Ubuntu 26.04 carries PHP 8.5 in its own repositories, so
  the change needs no third-party repository. See
  [Adding PHP to the image](docs/11-OPERATIONS.md#adding-php-to-the-image).
- **`pcntl` needs no install on Ubuntu, and the docs now say so.** Ubuntu compiles `pcntl`,
  `posix`, `sockets`, `shmop` and the SysV IPC extensions into `php8.5-cli`, so the `attach`
  backend works out of the box; the official image needs `docker-php-ext-install pcntl`. The
  reverse is also recorded: the official image bakes in `mbstring`, `curl`, `dom`/`xml*`,
  `sqlite3` and `mysqlnd`, which are separate `php8.5-*` packages on Ubuntu, so neither
  extension list is a superset of the other.

### Fixed

- **The old recipe could not run `composer install`.** Composer unpacks `dist` archives with
  `ext-zip` or an `unzip` binary, and the documented Dockerfile installed neither, so it failed
  with "The zip extension and unzip/7z commands are both missing" on any real application. Both
  example recipes now install `unzip`, and Alpine also needs `php85-phar` — Composer is a
  `.phar` and refuses to start without it, which Ubuntu's `php8.5-cli` provides but Alpine's
  `php85` does not.

## [0.6.0] - 2026-11-08

This is the release the published benchmark pins: the numbers in
`volucer-competition` are produced by this tag, so the table describes the artifact
`cargo install` gives you rather than a working tree near it.

Read *Changed* before upgrading. Two defaults flipped and one public field is gone.

### Added

- **`measure_wait`, opt-in slot-contention reporting.** A `worker` or `attach` backend now collects how long each request waited for a slot only when asked to, and the shutdown summary says `wait not measured` when it did not. See *Changed* below for why the default flipped.

### Performance

- **A worker frame never reaches the peer half-written.** `send_frame` wrote the header and the payload with two `write_all` calls. Under a `BufWriter` the second does not append to the first — `tokio` hands a write larger than the space left straight to the socket while still holding the first in its buffer — so the peer could see the payload before the header. The two slices now go out as one `IoSlice` pair, which fixes the ordering. The short-write tail is handled explicitly: a payload larger than the socket send buffer returns a partial count, and ignoring it would desynchronise the frame stream instead of failing.
- **A response body is read into the body, not into a temporary.** `read_frame` allocated a `vec![0u8; len]` per frame and every response loop then copied the bytes a second time with `extend_from_slice`. The read is now split into `read_header` (which validates everything refusable — checksum policy, length — before the payload is touched) and `read_payload`, so a body frame's bytes are read straight into the `Vec` the response keeps. One allocation and one copy of the whole body removed per request, with the destination truncated back on any error so a rejected frame leaves no trace. Measured on `/items` at 32 workers: **+1.7%**; on `/echo` (a 5-byte body, nothing to copy) it is neutral, which is what the mechanism predicts.
- **`spawn_cgi` awaits its exchange instead of parking a runtime thread.** It was the last backend on the blocking path because its exchange is owned by a supervised slot admitted through a `Condvar` — on the HTTP front-end that thread is a runtime thread, so twelve concurrent requests parked all twelve and nothing was left running to release one. The slot now keeps its admission count and gains a `Notify` beside the `Condvar`, so an awaited request waits on the count rather than on a thread.
- **Every awaited read is bounded.** `prepare_for_async` clears `SO_RCVTIMEO` (a pending `epoll` operation against a receive timeout is documented to panic), so on the awaited path the deadline has to be a `tokio::time::timeout`. Without one, a backend that accepts a request and never answers held the future, the permit and the connection for ever.
- **Static files no longer take two blocking-pool hops per request.** `decide()` sent one `statx` to `spawn_blocking` because `plan()` is synchronous; the hop cost far more than the work (a dispatch, two thread wakeups and a `Mutex` handoff). Sampling `GET /<file>` at 1,512 B and c=96 put 31% of that layer's own CPU in `_raw_spin_unlock_irqrestore`, reached through `futex_wake` — the hop, not the syscall. A slice of 256 KiB or less is now read with a single `pread` on the calling thread and handed to `hyper` as one in-memory frame, which is also the shape a small response wants. `STREAM_CHUNK` rose from 64 KiB to 512 KiB, because a large file paid 16 reads per MiB.
- **No request identity is minted when nothing consumes it.** A deployment with no `[server.access_log]` and no `[telemetry]` — the default — paid for a full request identity per request and discarded all of it: 16 random bytes for the id, 16 for a trace id, 8 for a span id through one `getrandom` per 64 ids, plus six `String` copies of `method`, `target`, `protocol`, `host`, `user_agent` and `referer`.
- **The compression header view is not built when nothing compresses.** `from_cgi` assembled a temporary `HeaderMap` — a new map plus a clone of every response header — purely to decide whether to compress, then built a second one for the response. `compress` returns `Unchanged` immediately when the feature is off or the body is under `min_length`, so with `[server.compression] enabled = false` that map was allocated and dropped on every request.

### Changed

- **Slot waiting is no longer measured unless configured to be.** The pool timed every request twice — on acquisition and on completion — to fill counters whose only reader is the exit summary, so a deployment that never looked paid two clock reads per request for nothing. On a host whose clocksource is not TSC (`xen` on a virtual machine, which is the ordinary case) `clock_gettime` is a **syscall at ~245 ns** rather than a vDSO call, which put a real cost on the request path. Set `measure_wait = true` on a `worker` or `attach` backend to get the mean and p50/p99 back; the report distinguishes "not measured" from "measured, and it was zero".
- **`Response::duration` is gone.** Both the worker pool and the FastCGI clients timed every backend exchange and stored the result in a public field that nothing in this workspace read. The timing went with the field. `ResponseStream::head_duration()` remains.

## [0.5.0] - 2026-11-04

### Added

- **Idle worker keepalives.** Set `idle_ping_ms` to keep idle worker connections alive. It is off by default and only applies to workers that advertise `answers_ping`; the PHP SDK supports it.
- **Upstream connection pooling.** Connections to `upstream` origins are now reused. Set `idle_timeout_ms` to control how long idle connections remain open (90 seconds by default); use `0` to disable pooling.

### Changed

- **FastCGI backend exchanges are awaited asynchronously.** `external` backends no longer hold a blocking-pool thread while waiting for a response, including when streaming. `spawn_cgi` continues to use the blocking path.
- **Connection limit.** `[server] max_connections` defaults to `512`; excess connections are closed immediately without an HTTP response. Set it to `0` for no limit, and ensure the process file-descriptor limit is high enough.
- **More efficient request handling.** Attached and socket-transport workers are awaited asynchronously; script checks avoid blocking when unnecessary. Unix builds use jemalloc, and request/trace IDs use a per-thread random stream. The pipe transport is unchanged.

### Fixed

- **Connection-limit handling.** Refused connections no longer leak capacity; `max_connections = 0` correctly disables the limit. Refusal warnings are rate-limited during bursts while exact rejection counts remain available in stats, metrics and shutdown logs.
- **Worker calls on awaited connections.** Calls to server services such as `kv`, `lock` and `jobs` now work for awaited workers.
- **Request streaming on awaited connections.** Fix a runtime panic that caused requests to streamed-body workers, including `attach` workers, to fail with `502`.
- **Worker-pool throughput.** Cache streamed-request capability per pool and refresh it when workers are replaced, avoiding repeated slot locks on each request.

### Documentation

- **Runtime sizing and single-process scaling.** The [operations guide](docs/11-OPERATIONS.md#sizing-the-runtime) covers `TOKIO_WORKER_THREADS`; its [scaling section](docs/11-OPERATIONS.md#scaling-a-single-process) documents core scaling and the `max_connections` and child-count limits.

### Upgrade notes

- **Upstream connections are pooled by default.** Set `idle_timeout_ms = 0` to disable reuse and restore one connection per request.
- **No configuration migration is required.** No keys were removed or renamed. `idle_ping_ms` defaults to `0` and only affects workers that advertise `answers_ping`; `idle_timeout_ms` applies only to `upstream` backends.
- **Review custom `max_connections` values.** The default remains `512`; lower limits close excess connections without an HTTP response, while limits raised to compensate for the fixed slot leak can be reduced. See [Scaling a single process](docs/11-OPERATIONS.md#scaling-a-single-process).

## [0.4.0] - 2026-11-02

### Added

- **Access controls and custom error pages.** Client-address allow/deny rules and status-specific pages for server-generated errors.
- **AMQP job queues.** The optional `amqp` feature adds RabbitMQ as a job driver and passes queue arguments to the broker.
- **Backend response streaming and file offload.** Responses larger than `stream_threshold_bytes` are relayed as a stream. Backends can serve files through `X-Sendfile` or `X-Accel-Redirect`.
- **Configuration file names.** `.volucer.toml` is accepted and answered with `404` by the static layer, so it can sit inside the document root. `VOLUCER_CONFIG` selects a file without `--config`. `volucer.toml` takes precedence when both names exist. Resolution order: `-c`, the subcommand variable (`VOLUCER_HEALTH_CONFIG`, `VOLUCER_STATUS_CONFIG`), `VOLUCER_CONFIG`, then the default names.
- **Configuration includes and CLI overrides.** `include` splits configuration across files. `-o` updates array-of-table entries.
- **Deployment guidance.** Container PHP setup and end-to-end rolling deployments are documented.
- **HTTP/3 and gRPC over QUIC.** The optional `http3` feature adds an HTTP/3 listener with gRPC requests and trailers.
- **Live tests against real origins.** The opt-in suite (`VOLUCER_LIVE=1`) covers public certificate chains, ALPN-negotiated HTTP/2, and real response shapes. It runs daily in CI, where a skipped test counts as a failure.
- **Rolling worker restarts.** `volucer workers restart` replaces idle workers one at a time, so a deploy does not drop a request. It reports why a sweep is incomplete.
- **Graceful shutdown.** A stop drains in-flight requests for `shutdown_timeout_ms` before exiting, instead of dropping them.
- **Route redirects and roots.** Redirects can use `{path}`, `{rest}` and `{query}`. Routes can mount a directory with `root`.
- **RPC clients for non-worker backends.** FastCGI and HTTP applications can use worker services. The Python and Ruby SDKs include RPC clients.
- **Server operations settings.** Backend `request_timeout_ms`, `[server] max_connections` and `shutdown_timeout_ms`. `access_log.path = "-"` writes logs to stdout.
- **TLS documentation.** States that client-certificate authentication is unsupported and lists the available service-to-service authentication options.
- **Tracing controls.** Trace sampling and backend `traceparent` propagation. Access-log entries include the trace ID.
- **Unix-socket listeners.** HTTP and FastCGI front ends can listen on Unix sockets.
- **Virtual hosting to HTTP/2 origins.** `preserve_host = true` on a backend sends the client's name in `:authority`. See [02-BACKENDS.md](docs/02-BACKENDS.md).
- **Worker request and response streaming.** Request bodies stream to capable workers and uploads spool to disk. PHP worker mode populates `$_POST` and `$_FILES` and keeps `$request->body`. Workers can send unknown-length responses incrementally using the `STREAMED` flag. PHP's `StreamedResponse` supports downloads and server-sent events.
- **Worker restart diagnostics.** `volucer workers restart` reports why a restart sweep is incomplete.

### Changed

- **Backend selection.** A single configured backend no longer requires `default_backend`.
- **Configuration format.** Configuration is parsed as TOML 1.1, so an inline table may span lines and end with a trailing comma.
- **Configuration reload.** `SIGHUP` reloads configuration only. `SIGUSR2` restarts workers to load updated application code.
- **One status listener.** `[metrics]` and `[status]` are replaced by `[status]` with a single `listen`, and the exposition lives at `[status.metrics]`. The Prometheus endpoint used to name an `address` of its own for the same socket; two settings for one fact could only be reconciled by refusing a file where they disagreed.
- **Worker control is granted per method.** `allow_worker_reset` becomes `[status.workers] allow = ["workers.list", "workers.restart"]`, so a deployment can expose the list without the lever.
- **Service grants can target individual methods**, including worker-control methods. `attach` backends validate their service lists, and `volucer check` displays them.
- **Worker gate.** A missing gate now fails closed.
- **Worker reset.** `workers.reset` is refused for backends without a worker pool.
- **Validation.** `[status].listen` rejects Unix sockets. The metrics path cannot shadow a built-in listener route.
- **RPC errors.** Refused gRPC calls return `PERMISSION_DENIED` instead of HTTP `403`.

### Upgrade notes

- **`[metrics]` and `[status] address` move under one listener.** A file carrying either the old
  `[metrics]` table or `[status] address` is refused at startup with a message naming the
  replacement, rather than `unknown field 'metrics'`. Write `[status] listen` and, if you exposed
  Prometheus, `[status.metrics]`.
- **`allow_worker_reset` becomes a method list.** `[status] allow_worker_reset = true` is refused;
  write `[status.workers] allow = ["workers.list", "workers.restart"]`. A method left out of the
  list has no route at all, so an upgrade does not gain a mutation surface it was not given.

### Fixed

- **HTTP/2 virtual hosts.** Origins reached over HTTP/2 received the dialed address instead of the client's name, because the name travels in `:authority`. HTTP/1.1 was not affected. Use `preserve_host = true` on the backend to send the client's name. It is opt-in because origins that validate their own name answer foreign names with `421 Misdirected Request`.
- **HTTP/2 site selection.** The request host is resolved from either `Host` or the URI authority, including for redirects and CGI variables.
- **HTTP/3 upstream requests.** Inbound HTTP/2 and HTTP/3 versions are mapped to a version the upstream client supports.
- **AMQP publishing.** Unroutable mandatory publishes are treated as failures. The default virtual host in AMQP URLs is parsed correctly.
- **Worker startup diagnostics.** A worker that exits during startup has its `stderr` logged at `warn` instead of `debug`, so the error is visible at the default log level.
- **Worker stream truncation.** A response that ends without `LAST` is reported as truncated, and its worker is not reused.
- **Worker reset routing.** `workers.reset` reaches its registered service.
- **Skipped tests.** `VOLUCER_E2E=1` is honored by every suite that can skip, via the shared `volucer-testkit` crate. A skip for a missing capability fails under this setting. A skip for port contention does not.
- **Graceful-stop tests on macOS.** The tests now run on all Unix platforms, not only Linux.
- **Stale SDK checkouts.** `volucer-testkit::require_example_routes` checks the routes a test drives and names the checkout when one is missing.
- **Cache TTL.** File-backed cache entries keep sub-second TTLs.
- **Concurrency tests.** The prefork test measures request overlap instead of machine-dependent timings.
- **KV batch keys.** `kv.getMultiple` and `kv.setMultiple` decode keys the same way as single-key methods.
- **Metrics exposition.** Declared metrics render their HELP and TYPE metadata before they have samples.
- **Route precedence.** Ties select the earlier configured route.
- **Trace sampling.** Unsampled flags are preserved on export. Requests without a parent `traceparent` are handled correctly.
- **Documentation.** The route-loop explanation and CLI override guidance for arrays of tables and maps are corrected.

### Security

- **Dependency advisories.** Adds `cargo-deny` and resolves three vulnerabilities in the dependency graph.
- **HTTP/3 header limits.** Header-section size is bounded to mitigate the unpatched `h3` amplification issue (hyperium/h3#379).
- **PEM parser advisory.** RUSTSEC-2025-0134 (unmaintained PEM parser) is recorded in `deny.toml`. Migration to `rustls-pki-types` is tracked separately.

## [0.3.0] - 2026-10-26

### Added

- **Container images are published to `ghcr.io/<repository>`.** Every `v*` tag pushes a
  multi-architecture manifest list (`linux/amd64` and `linux/arm64`) of both the `scratch`-based
  musl image and the Debian-based glibc image, so `docker pull ghcr.io/sailantis/volucer:latest`
  works without a checkout or a Rust toolchain. `musl` is the recommended image and takes the
  unsuffixed tags (`0.3.0`, `0.3`, `latest`); `glibc` always carries a suffix (`0.3.0-glibc`,
  `latest-glibc`), so a bare pull cannot resolve to the base image by accident. `latest`
  deliberately does not move for a pre-release tag. The images are pushed by digest during the
  build and tagged only in a final job that depends on the release being published, so a
  half-built multi-architecture tag is impossible. The package is created private and has to be
  made public once after the first release. See
  [Docker operations](docs/11-OPERATIONS.md#pulling-a-published-image).
- **The `musl` Docker stage builds for `arm64` as well as `amd64`.** The Rust target is derived
  from `TARGETARCH`, so `docker build --platform linux/arm64 --target musl` produces an
  `aarch64` binary; an unsupported architecture is a build failure rather than a mislabelled
  image. `docker build --target musl` is unchanged on both architectures. Two corrections to
  what the docs used to say came out of building both: the Dockerfile has to be named with
  `-f packaging/docker/Dockerfile` (it is not at the context root), and a `scratch` image is
  23 MB on amd64 and 21 MB on arm64.
- **`packaging/docker/smoke-test.sh`** runs an image and checks both halves of what it claims:
  that the binary runs, and that the `HEALTHCHECK` answers `healthy` for a serving container and
  fails against a closed port. It takes a tag or a digest, so the release runs it against exactly
  what it published. A new `docker` job in `ci.yml` runs it on both images for every push, so a
  broken `HEALTHCHECK` no longer waits for release day to be discovered. It also works from Git
  Bash on Windows, where `mktemp -d` and `docker exec /path` need translating - the script
  converts the bind-mount path with `cygpath`, and refuses to let a path-mangled `exec` count as
  the negative case passing.
- **`--root`, `--worker`, `--fcgi` and `--concurrency` in the same quick-start shape.** `--root`
  aliases `--document-root`, and `--php-cgi`, `--worker` and `--fcgi` each declare a default
  backend in one flag: `--php-cgi [PATH]` a `spawn_cgi` pool driven by `php-cgi`, `--worker
COMMAND` a resident worker, and `--fcgi COMMAND` any FastCGI server with `{address}`. `--workers
<N>` sizes whichever was chosen and requires one of them; `--concurrency <N>` is how many
  requests one `--fcgi` slot serves at once, and is refused for the other two. Two backend flags
  at once is a usage error. `--fcgi` also implies "only real files", because a WSGI or Rack
  program is not `index.php`. `volucer check` accepts the same flags, and its preflight now takes
  the resolved front controller rather than reading the table alone, so neither command warns
  about an `index.php` the deployment is not using. Nothing is detected without a flag.
- **A bare `spawn_cgi.binary` is resolved like a `command` program.** A value containing no path
  separator is looked up in the PHP search directories — `PATH` first, then the conventional
  install locations, with `PATHEXT` on Windows — so `binary = "php-cgi"` and
  `command = "php-cgi -b {address}"` find the same file, and `check` no longer lists a binary it
  found directly above a warning that it does not exist. A value with a separator is still used
  as a path, so every existing configuration is unchanged. `check` and the pool share one
  resolver, and `check` prints the path that will actually run.

- **`redirect_to_https` is configurable per site** and inherits `[server]` by default. Internal
  HTTP-only sites can disable redirects; enabling the setting when the site's protocols exclude
  `http` or `https` is rejected.
- **HTTP Strict Transport Security (HSTS)** is configurable in `[server.hsts]` and
  `[sites.<name>.hsts]` with `max_age_s`, `include_subdomains`, `preload` and `enabled`; sites
  inherit each unset key. The header is sent on every TLS response, including errors, never on
  cleartext or FastCGI. Invalid or unusable policies are rejected (including missing TLS/host,
  unsupported HTTPS, zero max-age, or preload without subdomains and a one-year max-age).
  `volucer check` shows resolved policies and warns when one site's subdomain policy covers another.
- **Sites can be restricted by local address and protocol.** `[sites.<name>].addresses` accepts
  addresses or CIDRs, and `.protocols` accepts `http`, `https` or `fastcgi`; both are matched
  before `Host`. Sites without `hosts` can serve as defaults for their addresses.
  `volucer check` reports these settings. See
  [Routing and Sites](docs/03-ROUTING-AND-SITES.md#binding-a-site-to-addresses-and-protocols).
- **`serve` and `check` can configure the server from CLI overrides.** Repeat `-o`/`--override
KEY=VALUE`, or use `--listen` and `--fastcgi`; `-c`/`--config` is optional when the default file
  is absent. Without a file, settings come from the overrides. They are applied to the TOML table
  and parsed once, preserving TOML types, environment expansion and unknown-key validation.
  Shorthand options apply before overrides, so overrides take precedence. An explicitly named
  missing config remains an error. Without a config, relative paths resolve from the working
  directory (shown by `check`), and `SIGHUP` reapplies the overrides. Arrays of tables and maps
  remain file-only.
- **`volucer health`** checks a configured status or metrics endpoint, with `--url`, `--ready`,
  `VOLUCER_HEALTH_URL` and `VOLUCER_HEALTH_CONFIG` support. Wildcard listeners use loopback;
  missing endpoints, HTTPS URLs and non-2xx responses fail with diagnostic output.
- **Docker health checks** run `volucer health` in both images. The release workflow tests healthy
  and failing probes.
- **`.dockerignore`** excludes `.git/` and `target/` from Docker build contexts.
- **Generic `spawn_cgi` commands** support an address placeholder or `VOLUCER_FCGI_ADDRESS`,
  `concurrency`, and TCP. They are mutually exclusive with `binary`; invalid commands fail at
  startup. Process supervision and recycling work with either driver.
- **`spawn_cgi.working_dir`** sets the working directory for non-PHP applications.
- **Changelog** documents releases; the operations guide links to it.
- **Configuration environment expansion** supports `${NAME}`, `${NAME:-default}` and `$$`.
  Unset variables without defaults and values containing newlines or NUL bytes fail at load time.
- **`[server.env]`** provides environment variables to spawned processes; backend values override
  it. Names are preserved, `attach` processes are unaffected, and invalid names fail at startup.
- **Worker gRPC services can be discovered from `.proto` files or directories** with
  `grpc_proto`; the resolved services are shown by `volucer check` and refreshed on `SIGHUP`.
- **`spawn_cgi.env`** configures process variables without overriding pool-managed FastCGI values.
- **`VOLUCER_VERSION`** is set for every process volucer starts and cannot be overridden.
- **Installer provenance verification** supports `auto`, `require` and `off` through
  `VOLUCER_VERIFY` (`install.ps1 -Verify`). Failed verification is fatal; checksum-only
  verification is reported when attestation cannot be checked.
- **`install.sh` adds its install directory to `PATH`** by default. The update is idempotent and
  symlink-safe; `VOLUCER_MODIFY_PATH=0` opts out.
- **`install.ps1 -ModifyPath 0`** (or `VOLUCER_MODIFY_PATH=0`) opts out of PATH changes. Its
  self-tests verify default behavior and the opt-out without leaving stale entries.
- **`spawn_cgi` commands can use `{concurrency}`.** A `command` is started once per slot with its
  own `{address}`, so the worker count its program is told to fork belongs to one address — which
  is exactly what `concurrency` means. Substituting it removes the second copy of that number:
  `command = "gunicorn app.wsgi --workers {concurrency} --bind {address}"` beside
  `concurrency = 4` cannot drift, where `--workers 4` beside `concurrency = 4` is two statements
  with nothing checking that they agree — the case where raising `concurrency` leaves the surplus
  waiting in the socket backlog, uncounted. The substitution is the **resolved** value, so
  `concurrency = 0` gives the program `1` rather than a `0` it would read as "start no workers";
  `volucer check` prints the resulting command line as `command resolves:`. `{workers}` is
  deliberately not a placeholder and is refused by name, because a slot is started once per slot
  with its own address and so has no sibling count to be told. Any other brace is left as written,
  so `--define={a:1}` still works.

### Changed

- **`volucer check` prints a `spawn_cgi` backend's process total.** `processes: 16 (workers 4 ×
concurrency 4)` for a `command` driver, `processes: 20 (workers 4 × (children 4+1))` for
  `php-cgi`, with `workers = 0` resolved to the core count first. The multiplication used to live
  only in prose, which made a `concurrency` misread as a backend-wide total invisible until the
  host ran out of memory. The line replaces the `php-cgi`-only `prefork:` line, which stated the
  same arithmetic for one driver.
- **`spawn_cgi`'s `capacity` key is now `concurrency`.** The old name read like a total for the
  backend, while the number is per slot: a `command` slot's width. The backend's total is
  `workers × concurrency`. The CLI flag follows (`--concurrency`), with `--capacity` kept as an
  alias. An existing file that still sets `capacity` fails at startup naming the key rather than
  being silently ignored.
- **The redirect is decided by the site, not by the listener.** `ListenerSettings::redirect_to_https`
  is gone: a listener now supplies only the port a redirect should name, and _whether_ a request is
  redirected comes from the resolved site. A cleartext listener that no site redirects on no longer
  logs that it will.
- **`default_backend` and `routes` now belong in `[server]`.** A file that still has the top-level
  key fails at startup with a message naming the new location, instead of the `unknown field` the
  parser would otherwise produce. The key is not read at all, so an old file cannot half-work: it
  is rejected until the table is moved.
- **`config.describe` redacts `env` and `headers` values and URL userinfo.**
- **`spawn_cgi` starts slots concurrently.** Port allocation remains serial; at 32 workers,
  startup fell from 10–14 seconds to about 2.1 seconds.
- **`docs/` is organized as a step-by-step guide.**

### Fixed

- **FastCGI children no longer die after startup on Linux.** Forking uses a long-lived thread so
  `PR_SET_PDEATHSIG` does not kill children when temporary startup threads exit.
- **Clippy and MSRV builds compile again.** Stale frontend and Windows-only test calls were fixed.
- **CGI variables now work for non-PHP applications:** root requests receive `PATH_INFO`, bare
  `Location` responses default to 302, and the unsafe `Proxy` request header is dropped.
- **`spawn_cgi` logs child output and reports the configured backend name** on transport errors.
- **Fallback script paths are checked against traversal**, including Windows drive-prefixed paths.
- **ACME cache keys are private:** directories are set to `0700` and files to `0600` on every
  start; a cache path that is not a directory fails at startup.
- **Installer downloads use HTTPS where supported**, and provenance verification closes the
  gap in relying only on a checksum served alongside the archive.
- **FastCGI startup detects port conflicts** by confirming the child remains alive after a
  successful connection.
- **`serve` accepts HTTPS-only configurations**, matching `check`.
- **End-to-end test failures include server stderr** for easier diagnosis.
- **Recycled `spawn_cgi` slots stop and reap the previous process before restarting.** This
  prevents the replacement from binding an address still held by its predecessor. On platforms
  that allow both processes to bind, the old process could otherwise continue serving connections
  while the pool treated the replacement as the active worker.
- **`volucer-server` tests avoid reusing recently released ports.** The shared test helper now
  scans a bounded port range, avoiding collisions caused by the operating system returning a
  just-released port to another concurrently running test.
- **The worker-pool slot-wait test establishes a real queue before measuring wait time.** A gate
  keeps the initial requests in flight while queued requests are submitted, and the test no longer
  depends on the production request timeout or on threads happening to overlap under load.

## [0.2.0] - 2026-10-22

### Added

- **`attach` backend** connects to externally supervised worker pools without spawning processes.
- **`spawn_cgi.children`** runs multiple PHP children per slot, reducing measured memory use by
  4.5x at 16 processes and 9.5x at 64.
- **Worker transport sockets** support the same configuration as other socket backends.
- **`max_connections`** adds opt-in back-pressure for external backends.

### Fixed

- **FastCGI port races** retry only when a child exits during startup.
- **Prefork supervisors** reap exited children and avoid zombies.
- **Unix shutdown** terminates PHP and worker processes through process groups and
  `PR_SET_PDEATHSIG`.

### Changed

- **Static serving disables itself for front-controller apps** unless explicitly enabled,
  avoiding unnecessary filesystem checks (measured throughput gains: 18–26% on `/` and 7–14%
  on `/items`).

## [0.1.0] - 2026-10-18

The first release: one binary that serves PHP without embedding it.

### Added

- **Worker and `php-cgi` request handling** with per-route backend selection.
- **Backends:** spawned workers, external `php-fpm`, supervised `php-cgi` pools and HTTP origins;
  FastCGI supports TCP, Unix sockets and Windows named pipes.
- **HTTP and FastCGI front ends** with routing, multiple sites, static files and compression.
- **TLS and ACME** with SNI, ALPN, per-site certificates, automatic renewal and HTTP redirects.
- **Whole-process configuration reloads** on `SIGHUP`; restart-only changes are reported.
- **Observability** with access logs, request IDs, tracing, rate limits, metrics, status and
  proxy-aware client IPs.
- **Worker services** for key-value storage, locks, jobs and gRPC, with PHP, Python and Ruby SDKs.
- **`volucer check`** reports listeners, backends, PHP binaries and selection reasons.
- **Release archives for six targets**, platform installers and a systemd unit.

[Unreleased]: https://github.com/sailantis/volucer/compare/v0.6.0...HEAD
[0.6.0]: https://github.com/sailantis/volucer/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/sailantis/volucer/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/sailantis/volucer/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/sailantis/volucer/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/sailantis/volucer/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/sailantis/volucer/releases/tag/v0.1.0
