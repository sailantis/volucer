# Changelog

All notable changes to volucer are documented here.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **`redirect_to_https` is a per-site key.** A site that states nothing inherits `[server]`'s
  answer, and a site can turn it off — which is the deployment a single boolean cannot express: an
  internal site bound to a private address with `protocols = ["http"]` is deliberately cleartext,
  and a deployment-wide redirect used to bounce it to a scheme it does not serve. The redirect is
  now resolved **from the site**, so it is decided after site selection, and setting it `true` on a
  site whose `protocols` excludes `http` or `https` is refused as inert.
- **`Strict-Transport-Security`**, as `[server.hsts]` and `[sites.<name>.hsts]`:
  `max_age_s`, `include_subdomains`, `preload` and `enabled`. A site inherits per key, so naming
  only `max_age_s` keeps the server's `includeSubDomains`. Sent on **every** answer over TLS —
  error pages included, like nginx's `add_header … always` — and never over cleartext (RFC 6797
  §7.2) or FastCGI. Refused where it could never be honoured: a site with no `hosts` (a browser
  files a policy against a name, never an IP), a site whose `protocols` excludes `https`, and a
  deployment with no `[server.tls]`. `preload = true` requires `include_subdomains` and at least a
  year, and `max_age_s = 0` is refused because in the header it means "delete this policy".
  `volucer check` prints the resolved policy per site and warns when `include_subdomains` on one
  site covers another site in the same file — a browser that has seen the parent then refuses
  cleartext to the child, including any internal address it resolves to.
- **Sites can be bound to addresses and protocols.** `[sites.<name>].addresses` restricts a site
  to the **local** addresses a connection arrived on (`192.168.9.123` or `192.168.9.0/24`), and
  `.protocols` restricts it to `http`, `https` or `fastcgi`. Both are decided before the `Host`
  header is consulted, so a site naming an internal address can no longer be selected by a client
  that reached a public one and typed the name — the reason the key exists. A site with
  `addresses` and no `hosts` is a default for those addresses, which is nginx's `default_server`
  shape. `volucer check` prints both. See
  [Routing and Sites](docs/03-ROUTING-AND-SITES.md#binding-a-site-to-addresses-and-protocols).
- **The configuration can be the command line alone.** `serve` and `check` now take `-o`
  (`--override`) `KEY=VALUE` pairs, repeatable, plus `--listen` and `--fastcgi` as shorthand for
  `server.http` and `server.fastcgi`. `-c`/`--config` is optional: with no flag and no
  `volucer.toml` beside the working directory, the server starts from an empty table and everything
  comes from the overrides. This removes the failure that a container with a `CMD` and nothing to
  mount could not start at all — `Config::load` read a file unconditionally, so the process died
  with `failed to read volucer.toml` before it had a chance to be told anything.
  - Overrides are laid onto the `toml::Table` the file produced and deserialized **once**, so the
    value is typed by TOML (`workers=8` is a number) and `deny_unknown_fields` still refuses a
    typo — `-o server.htpp=…` is a startup error naming the key, not a setting that parses and is
    ignored. `${NAME}` is expanded first, as in the file.
  - `--listen` and `--fastcgi` apply before every `-o` pair, so `-o server.http=…` wins over
    `--listen …` rather than depending on argument order.
  - An explicit `-c` naming a missing file is still an error: "I said nothing" and "I named a file
    that is not there" are different, and only the first may fall back.
  - With no file, relative paths (`server.tls.certificate`, `[server.acme].cache`,
    `[server.access_log].path`, a file KV store's `path`) resolve against the working directory,
    which `check` prints. A `SIGHUP` replays the overrides exactly as given.
  - Arrays of tables and `map` values have no dotted spelling and stay file-only.
- **`volucer health`** checks a configured status or metrics endpoint, with `--url`, `--ready`,
  `VOLUCER_HEALTH_URL` and `VOLUCER_HEALTH_CONFIG` support. Wildcard listeners use loopback;
  missing endpoints, HTTPS URLs and non-2xx responses fail with diagnostic output.
- **Docker health checks** run `volucer health` in both images. The release workflow tests healthy
  and failing probes.
- **`.dockerignore`** excludes `.git/` and `target/` from Docker build contexts.
- **Generic `spawn_cgi` commands** support an address placeholder or `VOLUCER_FCGI_ADDRESS`,
  `capacity`, and TCP. They are mutually exclusive with `binary`; invalid commands fail at
  startup. Process supervision and recycling work with either driver.
- **`spawn_cgi.working_dir`** sets the working directory for non-PHP applications.
- **Changelog** documents releases; the operations guide links to it.
- **Configuration environment expansion** supports `${NAME}`, `${NAME:-default}` and `$$`.
  Unset variables without defaults and values containing newlines or NUL bytes fail at load time.
- **`[server.env]`** provides environment variables to spawned processes; backend values override
  it. Names are preserved, `attach` processes are unaffected, and invalid names fail at startup.
- **`spawn_cgi.env`** configures process variables without overriding pool-managed FastCGI values.
- **`VOLUCER_VERSION`** is set for every process volucer starts and cannot be overridden.
- **Installer provenance verification** supports `auto`, `require` and `off` through
  `VOLUCER_VERIFY` (`install.ps1 -Verify`). Failed verification is fatal; checksum-only
  verification is reported when attestation cannot be checked.
- **`install.sh` adds its install directory to `PATH`** by default. The update is idempotent and
  symlink-safe; `VOLUCER_MODIFY_PATH=0` opts out.
- **`install.ps1 -ModifyPath 0`** (or `VOLUCER_MODIFY_PATH=0`) opts out of PATH changes. Its
  self-tests verify default behavior and the opt-out without leaving stale entries.

### Changed

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
