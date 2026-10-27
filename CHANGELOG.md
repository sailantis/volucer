# Changelog

All notable changes to volucer are documented here.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

- **`[metrics]` and `[status]` are now one table, `[status]`, on one listener.** The old shape had
  two top-level tables that each named an address for the same socket, which is two settings for one
  fact — and the server could only reconcile them by *refusing* a file where they disagreed. It did,
  and the documented example was such a file: `[metrics] address = "127.0.0.1:2112"` beside
  `[status] address = "127.0.0.1:2114"` failed to start with "they share one listener". The listener
  is now named once, and each endpoint exists because its sub-table does:

  ```toml
  [status]
  listen = "127.0.0.1:2114"

  [status.health]                      # was: [status] unavailable_status_code
  unavailable_status = 503

  [status.metrics]                     # was: [metrics]
  path = "/metrics"
  prefix = "myapp_"
  collect.app_orders_total = { type = "counter", help = "Orders the application counted" }

  [status.workers]                     # was: [status] allow_worker_reset = true
  allow = ["workers.list"]
  ```

  `[status]` alone is the ordinary deployment — `/health` and `/ready` for an orchestrator, because
  a `[status]` table that carried no probe would be an address answering nothing. A file written the
  old way is refused by name rather than by `toml`'s `unknown field 'metrics'`: the setting did not
  disappear, it moved one table down, and each refusal says which key replaced it.
- **The worker-control routes are granted per method, and the list can be granted alone.**
  `[status] allow_worker_reset = true` became `[status.workers] allow = ["workers.list"]`, and the
  two routes are no longer one switch. `workers.list` publishes `GET /workers` and answers 404 to a
  reset — a deployment that wants to *see* a pool no longer has to arm a `POST` to do it. Naming
  `workers.reset` adds the recycle. The names are the call plane's own, so one vocabulary describes
  both surfaces. The wildcard-address refusal now follows the *method* rather than the table: it
  fires for `workers.reset`, and a `listen = "0.0.0.0:2114"` that grants only the list is legal,
  because a list of workers is a fact about the process rather than a lever on it. An empty `allow`,
  and a name that is not one of the two routes, are both refused at startup.
- **A `None` worker gate now fails closed.** It documented "`None` means the gate is permanently
  shut" and then treated it as open, so a listener built without one served both routes. Since the
  grant is now per method, the gate carries a `WorkerGrant` and `None` grants neither — configuring a
  mutation route is a positive act, and forgetting to is not consent.
- **`[status].listen` refuses a `unix:` address.** `parse_listener` accepts that form for every other
  listener and `RunningStatus` binds a `TcpListener`, so `listen = "unix:/run/volucer/status.sock"`
  passed `volucer check` and then failed at bind with `invalid port value`. The rustdoc advertised
  the form. It is now refused when the file is read, with the reason: the listener serves an HTTP
  readiness probe, a scrape and the worker-control routes, and an orchestrator reaches it by address.
- **The metrics exposition's path may not shadow one of the listener's own routes.** `path =
  "/health"`, `"/ready"` or `"/workers"` is refused at startup rather than resolved by whichever arm
  of the router happens to run first.
- **The configuration parser is now TOML 1.1** (`toml = "1"`, from `0.8` / TOML 1.0). The feature
  this buys is the one that was most awkward to write by hand: an inline table may now span several
  lines and end with a trailing comma, so a deployment's own metrics read as one block —
  `collect = { app_orders_total = { type = "counter", help = "…" }, … }` — instead of one
  `[status.metrics.collect.<name>]` header per metric, four levels deep. Nothing in the *code*
  changed: `toml::Table`, `Value::Table(..).try_into()` and `de::Error` are the same API, and the
  whole workspace passed without an edit. Three tests pin the new syntax so a silent pin back to
  `0.8` cannot pass: one asserts the block parses *and* that its entries reach the registry, one
  asserts the price of the form — an inline table cannot be extended later **in the same text**, by a
  header or a dotted key — and one asserts the limit of that rule, that an `-o` pair still adds an
  entry because it is applied after parsing. `volucer.example.toml` and two documentation pages now
  show both spellings and say when to pick each.

### Fixed

- **`workers.reset` was unreachable from a call, and had been since it was written.** The `workers`
  namespace is registered under that name, and `MethodRegistry::serve` routes on the text before the
  *first* dot — so the method, spelled `worker.reset` (singular), resolved to a `worker` namespace no
  registry contains. The lookup refused it before the arm that serves it could run, on the worker
  channel and over `[rpc]` alike; `POST /workers/{index}/reset` worked only because it calls
  `WorkerControl::restart` directly and never passes through a registry. The method is now
  `workers.reset`, matching its namespace and the HTTP route. Three tests assert that a reset reaches
  the pool *through a registry* rather than on the namespace alone, which is the layer that would
  have caught this: a test calling `WorkersNamespace::serve("worker.reset")` passes against the broken
  registration too.
- **A route tie went to the later entry, not the earlier one.** `select_route` used
  `Iterator::max_by_key`, which returns the *last* maximum, while the table's own documentation —
  and `Config::route_backend`'s — says ties are settled by declaration order so the earlier entry
  wins. The two disagreed harmlessly for as long as a tie meant two routes that behaved identically
  for routing. They no longer do: `[rpc]` derives a backend's token from the name a route selects,
  so reversing the tie handed one backend's call plane to another. Two regression tests now pin both
  halves of the rule — that a tie goes to the earlier entry, and that a later route stating *more*
  constraints still wins.

### Added

- **`[rpc]` gives a FastCGI or HTTP application the services a worker reaches over its channel.**
  A worker asks for `kv`, `lock` and `jobs` with `CALL` frames on the stream that carries its
  requests; a FastCGI application has no such stream, because it exists for one request and can only
  answer what it was handed. The new listener is a socket the application dials instead: same frames,
  same codec, and the **same `services` list**, so a deployment cannot grant a cache to its workers
  and forget the same cache for the same application. Volucer injects `VOLUCER_RPC` and
  `VOLUCER_RPC_TOKEN` into every request a granted backend serves — or `X-Volucer-Rpc` and
  `X-Volucer-Rpc-Token` for an `upstream` origin — so the address is written once, in one file.
  A backend that grants nothing is handed neither, which is what stops an unprivileged application
  from attempting a call that would only be refused. There is deliberately no handshake: a CGI
  process would pay for one on every request, so the token travels on each call and a withdrawn grant
  takes effect on the next one. `services` is now accepted on `external`, `spawn_cgi` and `upstream`
  backends; a grant without `[rpc]`, and `[rpc]` with nothing granted, are both refused at startup.
  The PHP SDK's `Volucer\Rpc` is the other half: it dials that listener from `$_SERVER`, and because
  `Kv`, `Lock` and `Jobs` are written against a shared `CallPlane` seam, the same storage code runs
  over either transport.
- **`[telemetry].sample` controls which traces are exported.** The default, `parent_based`, follows
  the sampling decision in an incoming trace and records requests with no `traceparent` (root
  requests). `always` exports every trace. `ratio` exports the fraction configured by
  `[telemetry].sample_ratio`, selected deterministically from the trace ID using the W3C
  consistent-probability rule. This lets every hop using the same ratio agree on which traces to
  record; lowering the ratio only removes traces. Ratios `0` and `1` are exact, and the smallest
  non-zero ratio is `2^-56`, the resolution of the 56-bit decision. Invalid or unused ratio settings
  are rejected at startup rather than clamped or ignored. Sampling does not affect access logging or
  change the trace ID, so a trace omitted from export can still be found in the log by its ID.
- **`-o` can update entries in an array of tables.** For example,
  `-o server.routes[0].backend=app` updates a field on the first entry. Using the array's length as
  the index appends an entry; skipping an index or targeting a container of the wrong type is
  rejected rather than silently padded or replaced. The whole array can still be replaced with an
  inline TOML value, such as `-o 'server.routes=[…]'`. Quoted key segments
  (`-o 'server.env."APP.NAME"=x'`) allow map keys to contain dots or brackets. Values that look
  like TOML but are invalid — including PHP array syntax (`['k' => 'v']`) and JSON
  (`[{"k": "v"}]`) — now produce a syntax error instead of being treated as strings and failing
  later with a type error.
- **`[server] trace_context` decides what a backend is handed as its `traceparent`.** The four
  values are nginx's `otel_trace_context` — `ignore`, `extract`, `inject`, `propagate` — with the
  same meaning, and `extract` is the default because it is what the server has always done: the
  client's header reaches the backend unchanged, so a backend's spans are *siblings* of this
  server's span. Under `propagate` the backend is handed this server's own context instead, so its
  spans are *children* and a trace view shows one request rather than two branches hanging from it.
  The replacement applies to all three backend kinds — a worker's `HTTP_TRACEPARENT`, an
  `upstream` origin's forwarded header, and the FastCGI listener's `PARAMS` block, where the entry
  is replaced rather than appended to because two entries would leave the parent decided by
  whichever one an application reads first. The key needs no `[telemetry]` table: it is about what a
  backend is told, which is exactly what the deployment that exports nothing from here (nginx in
  front, a collector behind the application) still has to configure. Without it, a backend's spans
  and this server's were siblings, so a waterfall split at the hop this server had just added.
- **The access log carries the trace id**, after the two timings and before the request id, as the
  32 lowercase hex characters a collector shows — nginx spells the same value `$otel_trace_id`. A
  `-` in the text format and `null` in the JSON one when the request belongs to no trace, the same
  distinction `origin_time` makes. Without it, joining a log line to a trace was a manual copy of a
  timestamp into a query.

### Fixed

- **An unsampled trace was exported as sampled.** The OTLP encoder wrote `"flags":1` unconditionally
  and ignored the `sampled` bit, so a caller that sent `traceparent …-00` had its trace propagated
  onward as *unsampled* while this server exported its span as *sampled* — the backend and the
  collector were told opposite things about one trace, and nothing on the wire failed. The flag is
  now carried from the request's context into the span, so the bit on the outbound `traceparent` and
  the bit in the exported payload are one decision. This is what nginx's `otel_trace
  $otel_parent_sampled` covers, and it is why a trace the caller dropped now produces no export at
  all rather than an orphaned parent span.
- **A request with no `traceparent` was treated as unsampled.** It has no parent to defer to — it is
  a *root* — and reporting it `00` meant a standalone deployment, with no proxy in front, exported
  nothing at all while `[telemetry]` looked configured. Roots are now recorded, which is OTel's
  `ParentBased(root=AlwaysOn)`; an explicit `-00` from a caller is still obeyed.
- **The docs claimed arrays of tables and maps could only be set in a file.** Maps always took
  dotted keys, and an array of tables could already be replaced with an inline TOML value
  (`-o 'server.routes=[{ path_prefix = "/api" }]'`). Both pages now describe what the parser does.

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
  is gone: a listener now supplies only the port a redirect should name, and *whether* a request is
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
