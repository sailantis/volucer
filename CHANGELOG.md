# Changelog

All notable changes to volucer are documented here.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **`[server.error_pages]` gives an error status a page of its own.** Every `502`, `504`, `429` and
  router-level `404` was a single line of plain text, which is what a client sees on the worst day
  the deployment has.

  ```toml
  [server.error_pages]
  "404" = "pages/404.html"
  "502" = "pages/502.html"
  ```

  **Only the body is replaced, never the status** — a page for `502` is sent with `502` and
  `Cache-Control: no-store`, so a client can still tell what happened and a cache will not keep it.
  A page that cannot be read falls back to the plain-text body and logs why, because the status is
  the answer and the body is decoration: a missing page must not turn a `502` into a `500`. Only
  `400`–`599` are accepted; a `2xx` page could never be reached.

  Two things are deliberately untouched. A **backend's own error response** is not rewritten — a
  `404` a PHP application produces is the application's page, and these pages cover the statuses this
  server generates. And **gRPC** reports its outcome in the trailer section, so there is no body to
  replace.

  The table lives in `Reloaded` with the site table and the gRPC grants, so a reload applies it in
  the same swap and a request cannot be answered under one generation's routes and another's pages.
  The file is read per error response rather than cached: an operator fixes a broken `502` page and
  the next request uses it, which is the ordering that matters when something is already wrong.

- **`request_timeout_ms` is a per-backend key, and `[server] max_connections` exists.** Both were
  limits that only code knew about.

  `request_timeout_ms` was hard-coded at thirty seconds for a `worker` and a `spawn_cgi` slot, so a
  request that legitimately takes sixty — a report export, a slow external call, a large upload being
  processed — could not be served at all: the pool killed the worker mid-request and the client saw a
  `502`. Nothing in the file said so and nothing in `check` reported it, which is exactly the class of
  defect `check` exists to prevent. `0` is refused rather than read as "no limit": a request with no
  deadline holds its slot for ever, and the pool cannot tell that from a slow one until it has run out
  of slots. `volucer check` prints the effective deadline for each backend.

  **Two defects surfaced while verifying it, and neither was findable by reading.** A deadline was
  classified as a transport error, so the one retry spent a *second* full budget on a worker that had
  already proved it was not answering — a three-second fixture with `request_timeout_ms = 1000`
  answered after 2.1 s. The two are separated by `is_deadline_expired`, which the pools ask before the
  retry predicate. And the error a client saw was the OS's socket timeout (`os error 10060`, "a
  connection attempt failed"), which sends an operator looking for a network problem that does not
  exist; it now names the millisecond budget the configuration set. Measured after the fix: 1.09 s,
  with the deadline in the message.

  `max_connections` was reachable only as `--max-connections`, the single limit of its kind with no
  key — so a deployment driven by a systemd unit and a file could not express it, and an operator
  reading `[server]` had no way to learn it existed. The flag now has no `default_value`, which is
  what makes the file reachable at all: a flag carrying a default is indistinguishable from one an
  operator typed, so the configuration could never win. `0` means no ceiling, and the flag still
  overrides the file.

- **`[server.access_log] path = "-"` writes the access log to the process's stdout.** This is what
  a container expects: the runtime's log driver captures the records, no volume is needed, and
  `docker logs` shows the access log beside the diagnostics, which go to stderr. nginx spells the
  same destination the same way.

  A stream is a distinct sink rather than a file, because the two differ in exactly the behaviours
  the access log is about. There is no path, so `SIGHUP` does not reopen it — a rotation cannot
  detach what has no name, and `volucer check` says so instead of printing the usual `logrotate`
  instruction. And it writes through the handle the process already has rather than opening
  `/dev/stdout`, which is not cosmetic: a second handle on the same file description keeps writing
  into the renamed inode after a rotation, which is the failure the reopen exists to prevent.

  `-` is also no longer resolved against the configuration directory. `config_dir.join("-")` is a
  perfectly writable path, so the previous resolution would have started cleanly, logged to a file
  nobody opens, and left a container's own log empty — indistinguishable from logging being off. An
  empty `path` is now refused by name and points at `-`.

- **A stop now finishes the requests that are already running, bounded by
  `server.shutdown_timeout_ms`.** The listener stops accepting immediately, asks every open
  connection to complete its current request through `hyper`'s `graceful_shutdown`, and waits up to
  the budget for them. Thirty seconds by default, matching `docker stop`'s and `systemd`'s own
  defaults so an operator who has changed neither gets a drain that completes. When the budget
  expires, the number of abandoned requests is logged at `warn` — a truncated response is now
  distinguishable from a fast one. `0` is accepted and means "do not wait", for a deployment that
  would rather recycle instantly.

  This replaces a behaviour that was wrong and documented as right. Leaving the accept loop used to
  *return*, which dropped the runtime that owned the connection tasks and cancelled every one of
  them: a stop in the middle of a response truncated it. Measured against the previous binary, a
  four-second response in flight produced `volucer exited 0 after 0.28s` and a client that received
  `RemoteDisconnected` with no body; the same request now completes and the process exits 3.03s
  later, after the bytes are written. The HTTP/3 listener drains by the same budget.

  **The budget must be shorter than `TimeoutStopSec` / `stop_grace_period`**, or `SIGKILL` lands
  mid-drain and truncates exactly what the setting protects; [Operations](docs/11-OPERATIONS.md#systemd)
  now says so where an operator will read it.

- **`server.http` and `server.fastcgi` accept a unix socket path.** `unix:/run/volucer/http.sock`
  is now a listener rather than a spelling that passed `volucer check` and then failed at bind with
  `invalid port value`, which is what it did before: `parse_listener` accepted the form, both bind
  paths called `TcpListener::bind`, and only the *outbound* FastCGI client actually supported a
  socket. This is the shape an nginx deployment wants — `proxy_pass
  http://unix:/run/volucer/http.sock:/;` and `fastcgi_pass unix:/run/volucer/fcgi.sock;` — and it
  was the last of the three ways nginx reaches an application server that volucer could not serve.

  The transport is `BoundListener`/`Accepted` on the HTTP side and `Bound`/`Connected` on the FastCGI
  side, mirroring `[rpc]`'s existing pair: one accept loop and one request path per front-end, so a
  fix cannot land on a TCP socket and miss a unix one.

  Three consequences are modelled rather than papered over, because each would otherwise be a fake
  number an application acts on. There is **no port**, so `SERVER_PORT` falls back to the request's
  `Host` or to `80` and `redirect_to_https` is refused by `check` rather than emitting a `Location`
  that names a port no client can reach. There is **no peer address**, so `REMOTE_ADDR` is omitted
  from the CGI environment instead of being set to `0.0.0.0` or a loopback address, and a rate limit
  keyed on `key = "ip"` applies no bucket rather than one shared by every caller. And a
  `sites.*.addresses` binding cannot match, because the connection did not arrive on an address.

  `https` and `[server.http3].listen` refuse `unix:` **by name** — a TLS listener exists partly to
  name the port a redirect points at, and QUIC is UDP with no socket path — so `volucer check`
  reports it instead of the process failing after the pools are already up. A stale socket path left
  by a crash is unlinked before binding and removed again once the listener has drained, so an
  unclean shutdown does not need manual cleanup and a clean one leaves nothing behind.

- **Operations documents how to add PHP to a container image.** The published images include no
  PHP runtime, so the guide shows how to combine the `-glibc` binary with
  `php:8.5-cli-bookworm` for `worker` and `spawn_cgi`. It covers required extension packages,
  built-in OPcache, the default root user, replacing the base entrypoint, and adding a
  `HEALTHCHECK`. See [Adding PHP to the image](docs/11-OPERATIONS.md#adding-php-to-the-image).

- **A configuration can be split across files with `include`**, the way MariaDB's `!include` and
  `!includedir` split `my.cnf` — in one list where MariaDB needs two directives:

  ```toml
  include = ["base.toml", "conf.d", "sites.d", "sites/example.toml"]
  ```

  **Each entry is a file or a directory**, decided by what the name resolves to, and the list is
  read **in the order written**: `base.toml`, then every `*.toml` of `conf.d` in the **byte order of
  their names** — so the numeric prefixes packaged configuration uses (`10-base.toml`,
  `50-server.toml`) order the merge — then `sites.d`, then the file. MariaDB keeps `!include` and
  `!includedir` apart because a line in an option file has nowhere to say "this one is a
  directory"; a list does, so one key says everything and there is no precedence rule between two
  kinds to learn. `include_dir` is refused by name with the list that replaces it.

  A chain may be ten files deep — MySQL's own limit — and a cycle is refused by the chain that
  closes it.

  **The directive may be written anywhere in the file** — top, after the last table, or inside one.
  The key is removed from the parsed table before it is merged, so position says nothing about
  meaning. That recovers the property MariaDB's format has for free (their `!include` is a line, and
  their documentation writes it inside `[mariadb]`) and that TOML otherwise cannot express, because
  a `[table]` header *is* a scope. It is safe because no table in the model has an `include` field.
  A bare directive used to be read as a key of whatever table preceded it and reach the reader as
  `unknown field 'include'`; it is now the directive it reads as.

  It may be given **once per file**: a second list is refused naming both places, because any merge
  order would be the parser's alphabetical one and no operator wrote it.

  **The naming file wins over what it includes**, and later entries win over earlier ones, which is
  what makes a drop-in a base to be amended rather than an override laid on top. `-o` still beats
  all of them: `include` → the file → overrides.

  Two rules differ from MariaDB's, because TOML asks a question its format never does:

  * **The merge is of parsed tables, not of lines.** A second `[server]` header in one TOML
    document is a parse error, so splicing text would refuse the ordinary layout — a base file
    with `[server]`, a drop-in adding `[server.tls]`. Tables are merged key by key instead, and a
    drop-in adds to `[server]` without restating it.
  * **An array is replaced, never concatenated.** Two files that both list `[[server.routes]]`
    have no key to match elements on, and concatenating would make the result depend on which
    file was read first.

  Each file is expanded (`${...}`) and parsed on its own, so an unset variable or a syntax error
  names the file it was written in rather than the one on the command line. `volucer check` prints
  every file it merged, in merge order — the fastest answer to "why is my edit being overridden".
  A reload rereads them all, so editing a drop-in takes effect on the next `SIGHUP`.

- **HTTP/3 over QUIC, behind the `http3` cargo feature and a `[server.http3]` table.** It is a
  second transport rather than a second server: site resolution, the rate limit, routing, static
  files, the dispatcher, the access log and the metrics are the same code that serves HTTP/1.1 and
  HTTP/2, and the only new module is the listener that turns a QUIC stream into a request. The
  certificate resolver is the TCP listener's own, so per-site certificates, ACME issuance and a
  certificate reload reach it unchanged.

  ```toml
  [server.http3]
  listen = "0.0.0.0:443"     # UDP; the same port number as `http` is fine
  ```

  `Alt-Svc` is sent automatically on every TLS answer when the table is present, naming the port
  that actually bound — without it no browser ever discovers the endpoint, so it is not an optional
  extra. It is never sent over cleartext, where no browser acts on it.

  Three things about the feature are worth stating rather than discovering:

  * **The request body is buffered**, up to `max_body_bytes`. A QUIC request stream is read by the
    application, so it cannot be handed across the blocking boundary the CGI path needs. Responses
    still stream, so SSE and large downloads are unaffected. This is the one behavioural difference
    from the TCP path and it is documented in `docs/05-TLS-AND-ACME.md`.
  * **The QUIC TLS configuration is built TLS-1.3-only, in its own function.** Reusing
    `tls::server_config` would be the obvious implementation and is a crash:
    `QuicServerConfig::try_from` accepts a config that permits TLS 1.2 (it only checks for an
    AES-128-GCM suite), and `quinn-proto` then `unwrap`s the version check inside `start_session`.
    Since `min_version` defaults to `"1.2"`, that mistake binds the socket and then panics a
    runtime worker on *every* handshake. `Http3Listener::bind` asserts the invariant eagerly, so a
    regression is a startup refusal rather than a per-connection panic.
  * **`max_field_section_size` defaults to 16 KiB, deliberately, and the ceiling above it is
    derived from a panic rather than chosen for roundness.** `h3` 0.0.8 defaults the setting to
    `VarInt::MAX`, which is hyperium/h3#379 — a remotely triggerable amplification, and the reason
    `deny.toml` and its job exist. The cap is a security setting rather than a tuning knob; see the
    security entry below.

  A build without the feature refuses `[server.http3]` **by name**, naming `--features http3`,
  rather than reporting an unknown field. Published releases and the Docker images are built with
  it.

- **gRPC works over HTTP/3, in both directions of the trailer section.** gRPC reports its outcome in
  the trailer section, and a transport that dropped trailers would turn every call into a transport
  error while every other assertion passed. The QUIC response writer forwards trailer frames with
  `send_trailers`, so a call's `grpc-status` reaches a QUIC client exactly as it reaches an HTTP/2
  one. Both ways a trailer section is produced are now asserted rather than assumed: constructed by
  Volucer from a worker's answer
  (`a_grpc_call_over_http_3_reports_its_status_in_the_trailers`, including the `UNIMPLEMENTED`
  refusal path that answers `200` with status `12`) and passed through from an h2c origin
  (`an_h2c_origins_trailers_reach_an_http_3_client`). Nothing in the gRPC implementation needed to
  change — it lives in the transport-agnostic request path — but the property was untested, so the
  `send_trailers` arm could have been removed with no test failing.

### Changed

- **`SIGHUP` no longer appears to deploy application code, because it never did — `SIGUSR2` does
  now.** The two were conflated by omission: `SIGHUP` re-reads the configuration and swaps the site,
  certificate, rate-limit and gRPC-grant tables, and leaves every PHP process exactly as it was. A
  persistent worker keeps the code it loaded when it booted, which is the point of a persistent
  worker, so deploying new PHP required a **full process restart** — the one path that dropped
  requests. There was no rolling-restart primitive anywhere in the tree.

  Three triggers now reach one implementation, so they cannot disagree about what an idle slot is:

  | Trigger | How |
  | --- | --- |
  | `SIGUSR2` | the default; `systemctl kill -s SIGUSR2 volucer` |
  | `volucer workers restart` | a client of the `[status]` route, so it works where signals do not (Windows, a container with no shell) |
  | `POST /workers/restart` | a new `workers.restart` grant, alongside `workers.list` and `workers.reset` |

  `workers.restart` is **not** an alias for `workers.reset`. A reset names one worker and either
  replaces it or is refused with `409`; a restart sweeps every slot and replaces the ones that are
  not serving a request. It answers `200` when the sweep finished and **`202` when it left a busy
  slot for the next pass** — a rolling restart is explicitly allowed to be incomplete, because
  replacing a slot mid-request truncates a live response. `volucer workers restart` exits non-zero
  on `202` for the same reason, so a pipeline loops instead of stopping one sweep short of deploying.

  Two backend kinds are reported as *unmanaged* rather than counted as success, with the reason
  naming who owns them: an `attach` pool's processes belong to somebody else's supervisor, and an
  `external` (`php-fpm`) pool's belong to php-fpm. An `upstream` origin is restarted if volucer
  supervises it, and that is the one arm that cannot avoid losing in-flight requests: an HTTP origin
  serves many at once and has no idle state.

  The idleness test is conservative by construction. A worker slot is *known* idle when it is on the
  pool's free list — the same condition the request path uses — and it is **reserved** from that list
  before being replaced, so a request cannot arrive between the check and the kill and be served by a
  worker that is being torn down. A `spawn_cgi` slot is idle when its permit count is zero and its
  lock is uncontended, because with `children > 0` one address serves several requests at once and
  the lock alone would not say so. Correctness over completeness: a slot that merely *might* be busy
  is skipped and reported as `skipped_busy`.

  **`SIGUSR1` is now registered in order to be ignored, and not registering it was a crash.** It
  means "reopen the log" in nginx, so a `logrotate` script written for nginx sends it — and an
  unregistered signal keeps its default disposition, which for `SIGUSR1` is to terminate the
  process. Measured against the build that only *logged* a warning arm for it: `kill -USR1` on a
  running server exited with code 10 and wrote nothing to the log. Registering the signal turns that
  into a warning that names the signal the operator probably meant.

  **A `spawn_cgi` pool and a supervised `upstream` origin are swept too, and the route exists for
  them.** The worker-control surface was gated on holding a `WorkerPool`, so `POST /workers/restart`
  answered `405` on exactly the deployments whose code it is able to roll — found by running it
  against an `upstream` deployment. A sweep now goes through the dispatcher, which reaches every
  backend kind; the list and the reset still need a pool, and are still `404` without one.

- **A single backend no longer needs `default_backend`.** With exactly one backend there is nothing
  to choose, so the key may be omitted and that backend receives every request that matches no
  route:

  ```toml
  [server]
  http = "0.0.0.0:8080"

  [backends.app]
  kind = "worker"
  command = "php worker.php"
  ```

  `volucer check` prints the resolved default and says it was inferred, so a file that relies on
  this is not quietly asserting something invisible. A stated `default_backend` is always used as
  written and is never overridden.

  The inference is deliberately `exactly one`, not `the first one`: `backends` is a `BTreeMap`, so
  file order does not survive parsing, and a first-one-wins rule would resolve alphabetically —
  handing unrouted requests to `assets` because it sorts before `app`. Two or more backends with no
  default is still a configuration error, and the message now names the candidates and shows the
  key to write.

- **`services` can name a method, not only a namespace.** A grant used to hand over a whole
  namespace: `"workers"` gave a worker both `workers.list` and `workers.reset`, and `"kv.cache"`
  gave it `get` along with `clear`. Both spellings still work, and a namespace name still grants
  everything in it, but a backend may now be granted one method:

  ```toml
  services = ["kv.cache.get", "kv.cache.has", "workers.list"]   # read a cache and see a pool
  ```

  `volucer check` prints what the list expands to, because a wide name is a decision about a set:
  `services: workers, kv.cache` beside `expands to: kv.cache.clear, …, workers.reset`. The
  vocabulary is the one `[status.workers] allow` already uses, so the HTTP routes and the call
  plane are named the same way. `volucer-core` keeps the catalogue of every method — name,
  namespace, verb, whether it mutates — and a test in `volucer-server` asserts it agrees with the
  namespaces' own `match` arms in both directions, so a method added to a namespace without being
  catalogued fails the build rather than shipping a method no grant can name.
- **`workers.reset` is refused on a backend that owns no pool.** A reset acts on a pool slot by
  index, and only a `worker` or an `attach` backend has a pool; an `external`, `spawn_cgi` or
  `upstream` application has none. The grant was accepted and documented as "an answer nobody
  wants" — it passed `check`, the application dialled the call plane, and the call could only
  ever fail. It is now a startup refusal naming the kinds that can take it. `workers.list` is
  still grantable there and still answers `[]`, because it is a fact about the process rather
  than a lever on it.
- **A verb no namespace serves is refused by name, and the message lists the real ones.** This
  is what caught `lock.steal` — a method the documentation offered and no namespace implemented.
  A misspelled verb and a version that lacks the feature read identically otherwise, so the
  refusal names every verb the namespace does have.
- **`attach` backends now validate their `services` list.** They have one and their registry is
  built from it, but nothing checked the names, so `services = ["workes"]` passed `check` and
  granted nothing at all — on the one kind whose workers do get a channel.
- **`volucer check` prints each backend's `services`.** The list was invisible for a `worker` or
  `attach` backend, so the question a narrow grant raises — did it reach the method I meant — could
  only be answered by reading the source.
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

### Security

- **Three vulnerabilities in the dependency graph, found by adding `cargo-deny` and fixed by
  upgrade rather than suppression.** `rustls-webpki` (RUSTSEC-2026-0382) could accept a certificate
  path containing distrusted emSign intermediates, `webpki-roots` (RUSTSEC-2026-0381) bundled one of
  those roots, and `tokio` (RUSTSEC-2026-0386) had an I/O-safety unsoundness in `AsyncFd::new`.
  All three are patch upgrades — 0.103.16, 1.0.11, 1.54.1 — with no API change. Nothing in this
  repository was reading the advisory database, so they had been in the tree unnoticed; the `deny`
  CI job is what found them and what keeps the next one from arriving quietly.
- **`h3` 0.0.8 carries an unpatched amplification (hyperium/h3#379), mitigated by configuration.**
  Its `max_field_section_size` defaults to `VarInt::MAX`, so a `HEADERS` frame of one-byte
  static-table references decodes into a `HeaderMap` with thousands of entries. `[server.http3]
  max_field_section_size` (16 KiB) is what bounds it: RFC 9114 §4.2.2 charges each decoded field at
  least 32 bytes, and `qpack::decode_stateless` rejects the section *before* the fields are
  materialised. **No release carries the upstream fix** — h3 0.0.8 is still the newest, and
  hyperium/h3#378 (which replaces the panicking pre-allocation) is open and unmerged — so the
  mitigation is configuration and is pinned by
  `an_oversized_header_section_is_answered_431_and_the_listener_survives` — which asserts the
  server answers 431 *and* serves the next request, and which fails if the cap is returned to the
  crate's default. `deny.toml` records the advisory rather than hiding it.

  The **ceiling** above the default is part of the mitigation rather than a guard rail on it: `h3`
  builds the request's header map with `HeaderMap::with_capacity(fields.len())`, which panics above
  24 576 fields (measured: 24 576 succeeds, 24 577 aborts with `size overflows MAX_SIZE`). Since
  the budget bounds the field count at 32 bytes each, a ceiling above 786 432 bytes would admit
  the very crash it existed to prevent — so it is 512 KiB, and
  `the_ceiling_cannot_admit_a_panicking_field_count` pins the arithmetic between two constants
  that live in different crates.
- **`rustls-pemfile` is unmaintained (RUSTSEC-2025-0134), recorded rather than migrated here.** It
  is a thin wrapper over `rustls-pki-types` and is not vulnerable; the migration is to use
  `rustls-pki-types`' own PEM parsing and drop the dependency. `deny.toml` ignores the advisory with
  that reason beside it, because it is not part of the HTTP/3 change.

### Fixed

- **A cache TTL below one second silently deleted the key instead of storing it.** The file KV
  driver's header holds a deadline as an integer, and it was written in **seconds** with
  `ttl.as_secs()`. Truncating 900 ms gives 0, so the deadline written was *now* — `set` returned
  `Ok`, and the very next `get` was a miss. An application asking for a short-lived cache entry got
  no entry at all, with nothing logged, and the window where it worked was a lucky sub-second
  boundary. The header now stores **milliseconds**, the same resolution the Redis driver uses
  (`PX`, not `EX`) and for the same reason, so the two stores agree about what a caller's
  `Duration` means. Deadlines round **up**, so a TTL is never shortened — 1.5 ms is stored as 2 ms —
  and a sub-millisecond TTL becomes the smallest deadline that is not already past rather than
  zero. Asserted by `a_ttl_below_one_second_is_not_an_immediate_expiry`, which fails against the
  old code, and by `a_deadline_is_never_sooner_than_the_ttl_asked_for`, which pins the rounding
  direction against the clock read before the call.

  Two tests also asserted things only a stopped clock could satisfy, which is why this first
  appeared as a flake rather than a failure: `a_ttl_is_absolute_…` demanded
  `Ttl::In(Duration::from_secs(600))` exactly, which a deadline read one millisecond later cannot
  produce, and `ttl_distinguishes_…` slept for an expiry it could write by hand. Both now assert
  the property — a range, or a deadline planted in the file — so a loaded machine is not a red
  build, and neither test needs a sleep.
- **Every HTTP/3 request to an `upstream` backend failed as an unreachable origin.** The forward
  path passed the request's inbound HTTP version to the origin client, so a request that arrived
  over QUIC carried `HTTP_3` into `hyper-util`'s pooled client — which accepts HTTP/1.0, 1.1 and 2
  only and refuses anything else with `UserUnsupportedVersion` **before dialling a connection**.
  The failure surfaced as `502 … the origin is unreachable`, naming an origin that had never been
  contacted, so the message pointed at the wrong component; the transport was the cause and the
  origin was fine. The version is now decided by one helper, `upstream_version`: HTTP/1.0 and
  HTTP/1.1 pass through, and HTTP/2 or HTTP/3 become HTTP/1.1. It is deliberately *not* a
  preference — how an origin is spoken to is `protocol = "h2c"` or the ALPN result, both applied
  by the connector, which sends an HTTP/2 request over an h2c connection whatever this value says.
  Asserted by `an_h2c_origins_trailers_reach_an_http_3_client`, which drives real QUIC at a real
  h2c origin and fails against the old code, plus a property unit test that no inbound version
  produces an outbound one the client refuses.
- **Every browser request over HTTP/2 was served by the default site.** `h2` converts the
  `:authority` pseudo-header into `Uri::authority` and does **not** synthesise a `Host` header, and
  the request path read the host from `Host` alone. So `resolve_request` saw `None`, fell through
  to the `[server]` site, and answered from the wrong document root — a multi-site deployment over
  TLS served site A's files for a request addressed to site B. The bug is as old as the `h2` path
  and was invisible because the only assertion on it
  (`alpn_h2_negotiates_a_real_http_2_connection`) checks that a SETTINGS frame arrives and never
  sends a request. The host is now `Host` **or** the URI's authority, in one helper
  (`request_host`) used by the site lookup, the HTTPS redirect and the CGI environment; `Host`
  still wins when both are present, because that is the name a proxy rewrote. Two consequences
  beyond the site lookup are fixed with it: `Alt-Svc`/redirect answers and access-log `host` now
  name the site for an `h2` request rather than logging nothing, and an application receives
  `HTTP_HOST` — PHP builds absolute URLs from it, and an `h2` request used to reach it with
  `SERVER_NAME` set and `HTTP_HOST` absent, a state no HTTP/1.1 request can produce. Asserted end
  to end by `an_http_2_request_with_only_an_authority_serves_the_named_site`, which drives a real
  `h2` connection with an `:authority` and no `Host` — the shape a browser sends — and which fails
  against the old code.
- **`kv.getMultiple` and `kv.setMultiple` disagreed with every single-key method about what a key
  is.** The batch methods read their keys with `string_list` and used each string's literal UTF-8
  bytes, while `setMultiple` decoded *both* halves of its object and every scalar method decoded its
  `key`. The two spellings could therefore never address the same entry: a batch read reported every
  key as missing, and a batch write stored under a name no `get` could produce. Worse than a failure,
  it was silent — a cache that never hits, and for `setMultiple` writes that exist only to be
  expired. The refusal `keys_of` emitted said "needs a \"keys\" list of base64-encoded keys", which
  is what the code did not implement, and nothing tested either batch method. Present since the `kv`
  namespace was written. The keys are now decoded, a badly encoded one is refused by **position**
  (`keys[0]`), an empty one is refused as a scalar empty key already was, and two tests check both
  directions against one store — what a batch wrote must be readable by `get`, and what `set` wrote
  must be readable by a batch.
- **A declared metric that had not been counted yet rendered nothing, so `/metrics` could answer
  `200` with an empty body.** `Registry::render` skipped every family with no series, and the comment
  defended it on the grounds that a described-but-absent metric would puzzle a reader — which is
  true of the server's own `rr_http_*` set, whose families are pool-scoped and genuinely do not
  apply to a process with no pool, and false of a name the *deployment* wrote in
  `[status.metrics.collect]`. A deployment that declared two metrics and started a server with no
  worker pool got a zero-byte page that could not be told apart from an endpoint that was not wired
  up. Names now carry their provenance: `Registry::declare` (used by `describe_collected` for
  `collect` entries) renders `# HELP` and `# TYPE` with no sample until the first `emitMetric`, and
  `Registry::describe` keeps the old omit-when-empty behaviour. `prefix` and histograms are covered.
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

- **The Python SDK now has an `[rpc]` client, so a FastCGI or WSGI application reaches `kv`,
  `lock` and `jobs` without being a worker.** `volucer_worker.Rpc` is the counterpart of the PHP
  `Volucer\Rpc` and speaks the same wire: identical frames, the token on every call, no handshake.
  `connect()` dials nothing until the first call, so a request that never touches a store pays for
  neither a socket nor a failed connect, and `last_call_was_unknown` separates a refusal from a
  call that may have been served. The one thing a WSGI application must do differently is pass its
  `environ`: volucer puts the address in the CGI *request*, and the process only receives
  `VOLUCER_FCGI_ADDRESS`, so a client reading `os.environ` works for a script started by hand and
  fails for every real request. Verified end to end against a real server through `spawn_cgi`.
- **The Ruby SDK has one too, so all three SDKs reach `[rpc]`.** `Volucer::Rpc` reads the call
  plane from a Rack environment and takes the same `env:` argument the Python client takes its
  `environ` for, and for the same reason. It is the third implementation of one wire rather than a
  translation of either neighbour, so the differences are Ruby's: a failure **raises**
  `ProtocolError` rather than returning `nil` with a recorded reason, and that error answers
  `unknown?` — the "the call was written and never answered, so retrying may duplicate a side
  effect" case. **One deadline bounds the whole call, the dial included**, where the PHP client's
  connect and read budgets are additive; the worst case is therefore the configured timeout rather
  than twice it. Verified against a real `spawn_cgi` deployment (both the CGI-parameter and the
  `upstream` header form) as well as a live `[rpc]` listener.
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
