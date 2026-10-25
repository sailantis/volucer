# Changelog

All notable changes to volucer are documented here.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

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
- **`default_backend` and `routes` now belong in `[server]`.** Old top-level keys fail with a
  migration hint; the compatibility shim will be removed in the next release.
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

[Unreleased]: https://github.com/sailantis/volucer/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/sailantis/volucer/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/sailantis/volucer/releases/tag/v0.1.0
