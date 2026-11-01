# Changelog

All notable changes to volucer are documented here.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **Access controls and custom error pages.** Configure client-address allow/deny rules and status-specific pages for server-generated errors.
- **AMQP job queues.** An optional `amqp` feature adds RabbitMQ as a job driver and passes queue arguments through to the broker.
- **Backend response streaming and file offload.** Large responses can be relayed past `stream_threshold_bytes`; backends can also use `X-Sendfile` or `X-Accel-Redirect` to serve files.
- **Configuration includes and CLI overrides.** Split configuration across files with `include`, and update array-of-table entries with `-o`.
- **Deployment guidance.** Document container PHP setup and end-to-end rolling deployments.
- **HTTP/3 and gRPC over QUIC.** An optional `http3` feature adds an HTTP/3 listener and supports gRPC requests and trailers over HTTP/3.
- **RPC services for non-worker backends.** FastCGI and HTTP applications can use worker services; Python and Ruby SDKs now include RPC clients.
- **Route redirects and roots.** Routes can redirect using `{path}`, `{rest}` and `{query}`, or mount a directory with `root`.
- **Live tests against real websites.** An opt-in suite (`VOLUCER_LIVE=1`) runs the binary against real origins to prove what a loopback fixture cannot: a genuine public certificate chain accepted by the default trust store, ALPN negotiating HTTP/2 to the upstream, and real response shapes (compressed chunked bodies, origin statuses and redirects, request bodies parsed by the origin). It also covers a virtual host selecting the backend that reaches a real origin, and runs daily in a scheduled CI workflow, where a skipped test is a failure.
- **Server operations settings.** Configure backend `request_timeout_ms`, `[server] max_connections` and `shutdown_timeout_ms`; `access_log.path = "-"` writes logs to stdout.
- **Tracing controls.** Configure trace sampling and backend `traceparent` propagation; access-log entries include the trace ID.
- **Unix-socket listeners.** HTTP and FastCGI front ends can listen on Unix sockets.
- **Worker request streaming and multipart uploads.** Stream request bodies to capable workers and spool uploaded files to disk; PHP worker mode populates `$_POST` and `$_FILES` while preserving `$request->body`. SDKs and framework bridges support the streamed form.
- **Worker response streaming.** Workers can send unknown-length responses incrementally; PHP's `StreamedResponse` supports generated downloads and server-sent events.
- **Worker restart diagnostics.** `volucer workers restart` reports why a restart sweep is incomplete.
- **Worker protocol streaming support.** The protocol now represents unknown-length response bodies with an explicit `STREAMED` flag.
- **TLS documentation.** Clarify that client-certificate authentication is unsupported and document the available service-to-service authentication options.

### Changed

- **Backend selection.** A single configured backend no longer requires `default_backend`.
- **Configuration format.** Parse TOML 1.1.
- **Configuration reload semantics.** `SIGHUP` reloads configuration only; `SIGUSR2` restarts workers to load updated application code.
- **Metrics and status configuration.** `[metrics]` and `[status]` are consolidated under `[status]` on one listener.
- **Service grants.** Grants can target individual methods, including worker-control methods; `attach` backends validate their service lists and `volucer check` displays them.
- **Worker gate behavior.** A missing gate now fails closed.
- **Worker reset validation.** `workers.reset` is refused for backends without a worker pool.
- **Status and metrics validation.** `[status].listen` rejects Unix sockets, and the metrics path cannot shadow a built-in listener route.
- **RPC errors.** Refused gRPC calls use `PERMISSION_DENIED` rather than an HTTP `403`.

### Fixed

- **AMQP publishing.** Treat unroutable mandatory publishes as failures and correctly parse the default virtual host in AMQP URLs.
- **A stale SDK checkout reported itself as a server bug.** The worker suites drive the `volucer-php-sdk`'s `example/worker.php` over the wire and assert on the bytes, so a checkout that predates a route answered with the worker's default page — which reads exactly like a front-end that dropped a stream. `/stream`, `/stream-boom` and `/upload` arrived over three SDK commits, and because `actions/checkout` clones the SDK's default branch, an unpushed SDK change passed locally and failed in CI with a message pointing at `volucer-server`. Six tests failed that way against a checkout four commits behind. `volucer-testkit::require_example_routes` now checks the route the test is about to drive and panics naming the checkout, so the two repositories' disagreement is reported as that rather than as a bug in whichever one ran second.
- **Skipped tests that reported success.** `VOLUCER_E2E=1` now has one implementation, in the new `volucer-testkit` crate, so every suite that can skip honours it. Three did not: the `php-cgi` suite and the `serve.rs` and `frontend.rs` FastCGI front ends printed `skipping:` and never consulted the variable, so a CI run without PHP reported success for suites that had not executed. A skip for a *missing capability* is now a failure under `VOLUCER_E2E=1`; a skip for port contention is not, because that is a race rather than a missing tool.
- **The graceful-stop tests were Linux-only, and macOS failed because of it.** `Server::stop_gracefully` and the three tests that use it were gated on `target_os = "linux"`, but the signal handling they exercise is `cfg(unix)`: `serve` installs the same `signal_hook` handler on macOS. So on macOS the unix-socket test sent no `SIGTERM`, stopped the server with `Drop` — which is `SIGKILL` — and then asserted the socket path had been unlinked, which a killed process cannot do. A correct server reported a socket leak its own harness had created, and the gate that hid the cause was one line above the assertion. Widened to `unix`; Windows stays out, having no `SIGTERM` to send.
- **Worker startup diagnostics.** A worker that dies while the server is waiting for its socket connection now has its own `stderr` reported at `warn`, before the error that refers to it. The hint on that error said the worker's output "is logged above", but the drain thread wrote it at `debug` — invisible at the default log level, which is what `serve` uses. This was the likeliest first-run failure on Windows, where the socket transport is the default and a worker written against `Worker::create()` exits during its handshake.
- **Cache TTL precision.** File-backed cache entries now retain sub-second TTLs without expiring immediately.
- **Concurrency test reliability.** The prefork test measures request overlap instead of comparing machine-dependent timings.
- **HTTP/2 site selection.** Resolve the request host from either `Host` or the URI authority, including for redirects and CGI variables.
- **HTTP/3 upstream requests.** Map inbound HTTP/2 and HTTP/3 versions to a version supported by the upstream client.
- **KV batch keys.** `kv.getMultiple` and `kv.setMultiple` now decode keys consistently with single-key methods.
- **Metrics exposition.** Declared collected metrics render their HELP and TYPE metadata even before they have samples.
- **Route precedence.** Ties now select the earlier configured route.
- **Trace sampling.** Preserve unsampled flags when exporting traces and handle requests without a parent `traceparent` correctly.
- **Worker stream truncation.** A response that ends without `LAST` is reported as truncated, and its worker is not reused.
- **Worker reset routing.** Correct the method name so `workers.reset` reaches its registered service.
- **Documentation accuracy.** Correct the route-loop explanation and CLI override guidance for arrays of tables and maps.

### Security

- **Dependency advisories.** Add `cargo-deny` and address three vulnerabilities in the dependency graph.
- **HTTP/3 header limits.** Bound header-section size to mitigate the unpatched `h3` amplification issue (hyperium/h3#379).
- **Unmaintained PEM parser advisory.** Record RUSTSEC-2025-0134 in `deny.toml`; migration to `rustls-pki-types` remains separate.

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

[Unreleased]: https://github.com/sailantis/volucer/compare/v0.3.0...HEAD
[0.3.0]: https://github.com/sailantis/volucer/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/sailantis/volucer/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/sailantis/volucer/releases/tag/v0.1.0
