# Changelog

All notable changes to volucer are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this
project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Entries name the failure a change removes, not only the feature it adds, because the failures
are what an operator has to recognise. Where a number decided a change, the number is here —
the reasoning behind it lives in the source comments and in the documentation under [`docs/`](docs/README.md).

## [Unreleased]

### Added

- **`spawn_cgi` runs any FastCGI server, not only `php-cgi`.** A new `command` key takes a
  program that starts one slot — `gunicorn myapp.wsgi --bind {address}`, a `flup` server, a
  Rack adapter — and the placeholder is replaced per slot with the address that slot must
  bind. The same string is exported as `VOLUCER_FCGI_ADDRESS` for a program that reads it from
  the environment, so a server needing no placeholder needs no argument either. `binary` and
  `command` are mutually exclusive and each keeps its own semantics: a `binary` deployment is
  still `php-cgi`, with `children` as its prefork count and a named pipe on Windows; a
  `command` deployment uses `capacity` for the same number and TCP. A command that names
  neither the placeholder nor a reader of the variable is refused at startup, because it would
  bind somewhere the pool cannot reach and every request would be a 502 after a ten-second
  timeout. What the pool guarantees is unchanged and now driver-independent: started, waited
  for, health-checked, restarted on death, recycled on `max_requests`, killed as a process
  group. A command that exits is reported once rather than retried across ten ports — the
  retry exists for `php-cgi` losing a bind race, and nothing known is true of another program
  that died.
- **`spawn_cgi` takes `working_dir`.** The key `worker`, a job consumer and `upstream` already
  had, and its absence was the one thing that made a non-PHP command unusable: a Python
  application finds its modules relative to the working directory and a Ruby one finds
  `config.ru` there, so a supervisor that guessed would start the process somewhere it cannot
  boot.
- **`CHANGELOG.md`.** What changed between two tags could only be read out of the commit log,
  and the release notes for 0.1.0 and 0.2.0 were written after the fact. This file is that
  description, kept as the releases happen — and it is why `docs/11-OPERATIONS.md` now sends an
  operator here before an upgrade instead of to "the release notes".
- **`${NAME}` and `${NAME:-default}` in the configuration file.** Expanded from the server's
  environment before the file is parsed, so a key that is not a string works too:
  `workers = ${WORKERS:-0}`, `http = "127.0.0.1:${PORT:-8080}"`. Only the braced form is
  expanded — a bare `$NAME` is left as written, unlike RoadRunner — because a password or a
  DSN may contain `$` and a rule that expanded it would make the file mean something its
  author did not write. `$$` is one literal `$`. **An unset variable with no default stops
  the server**, naming the variable and the line: an empty listener address is a server that
  binds nothing, for a reason nothing logs. A value containing a line break or a NUL byte is
  refused, because the substitution produces TOML text and a newline inside a value could
  introduce a table header the file does not contain. Expansion lives in `Config::load`, not
  in `from_toml`, so the ~230 test fixtures that parse text do not depend on the machine.
- **`[server.env]`: an environment baseline every started process inherits.** A worker pool,
  a job consumer, a `php-cgi` slot and a supervised origin; a backend's own `env` is merged
  over it, so the shared value is written once and the exception stays on the table that
  needs it. Names are passed **exactly as written** — RoadRunner uppercases `server.env`
  keys, and a server that silently renamed one would leave an application reading `app_env`
  from a file that says `APP_ENV`. `attach` does not receive it, because volucer starts those
  processes not at all. An unusable name (`=`, NUL) is refused at startup rather than failing
  at spawn with an `InvalidInput` that names no key.
- **`spawn_cgi` takes an `env` table.** It was the only kind that starts a process and had no
  way to configure it: the CGI environment volucer builds per request carries request data and
  nothing else, so a deployment whose PHP configuration was entirely `spawn_cgi` could not set
  `APP_ENV` at all. Written after `PHP_FCGI_CHILDREN`/`PHP_FCGI_MAX_REQUESTS`, so the table
  cannot move either number the pool enforces.
- **`VOLUCER_VERSION` is set for every process volucer starts**, after the configured `env`, so
  it cannot be shadowed. A `php-cgi` per-request process and a supervised origin have no channel
  to ask the server what it is, which is why the variable exists; nothing in the SDK reads it.
- **The installers check the build provenance attestation.** `VOLUCER_VERIFY` (`install.ps1`:
  `-Verify`) takes `auto` (default), `require` or `off`. `auto` verifies the attestation the
  release workflow already writes with `actions/attest-build-provenance` — signed by GitHub's
  OIDC identity and recorded in Sigstore's transparency log, so a hostile download origin cannot
  forge it — when the GitHub CLI is installed, authenticated, and the archive came from the GitHub
  release origin, and otherwise **prints plainly that the archive was checked only against the
  checksum beside it**. `require` refuses to install when the attestation cannot be checked, for a
  deployment that must not take the weaker guarantee; `off` does not mention it. A _failed_
  verification is fatal in every mode, and `off` is the deliberate way past it. The mode is
  validated before the download, because an installer that fetches five megabytes and then
  reports that its own argument was misspelled has spent the operator's time to learn something
  it could have said immediately.
- **`install.sh` puts the binary's directory on `PATH`.** Installing to `~/.local/bin` and then
  printing "add it, e.g. `export PATH=…`" left the operator's *next* command — `volucer` — failing
  on a fresh account, which is the state most installs happen in. The directory is now added to
  the file the operator's shell actually reads: `~/.bashrc` for bash (falling back to
  `~/.profile` when there is none, which is what a login bash reads), `~/.zshrc` for zsh,
  `~/.config/fish/config.fish` for fish, and `~/.profile` for anything else. `install.ps1` already
  did the equivalent with one registry value; this is the Unix half of the same promise.
  Four things make the edit safe rather than hidden. It is **idempotent** — the marker comment
  and the destination path are both checked, so a second release does not append a second line,
  and a stock Debian or Ubuntu `~/.profile` that already spells it `$HOME/.local/bin` is
  recognised rather than duplicated (a *commented-out* mention does not count, because it does
  not put anything on `PATH`). It **writes through a symlink** to its target, so a dotfiles
  repository keeps owning the file instead of being replaced by a regular one. It **names the
  file and the exact line** it added, plus how to undo it. And **`VOLUCER_MODIFY_PATH=0`** turns
  it off entirely, for CI and managed hosts, with the by-hand instruction printed instead. The
  mode is validated before the download, like `VOLUCER_VERIFY`, because a value read in the last
  block of the script would otherwise be reported after the binary was already replaced — and not
  at all on a host where the directory is already on `PATH`.
- **`install.ps1` takes `-ModifyPath 0`** (`VOLUCER_MODIFY_PATH`), the same opt-out as the shell
  installer, and `install.ps1`'s own `install-selftest.sh` now uses it everywhere. It had been
  appending one entry to the machine's **user `PATH`** per test case into a temporary directory
  the test then deleted: 43 dead entries on the machine this was found on, each one a directory
  that no longer exists, in a value every new process reads. The suite now also asserts the
  default still adds the directory — and removes it again afterwards — so the two behaviours are
  both pinned and the machine is left as it was found. `install.ps1` now prints how to undo the
  edit, too, which it did not before.

### Changed

- **`default_backend` and `routes` moved into `[server]`.** They were the only two bare keys at
  the top of the file, and that is where the confusion came from: every other key is a table, so
  a reader met `default_backend = "app"` alone above `[server]` and had to be told _why_ it
  could not go below it (TOML reads everything after a `[table]` header as belonging to that
  table). Both are site settings, and `[server]` **is** the unnamed site — the docs have said so
  since `sites` existed — so they belong beside `document_root` and `front_controller`, which is
  also the table `[sites.<name>]` inherits from. The precedence is unchanged in either spelling;
  only the table changed. **A configuration using the old spelling is refused at startup with a
  message naming the new one**, rather than with toml's `unknown field 'default_backend'`: the
  key did not disappear, it moved one table down, and an operator who pastes their existing file
  into a new binary deserves that sentence. The shim is removed in the release after this one.
- **`config.describe` now redacts two things it previously let through.** The rule was a
  sensitive _key name_ only, so a credential that arrived inside a **value** passed straight to
  a worker. Two structural rules are added: the values of any `env` or `headers` map
  are elided **with their keys kept** (`[telemetry.headers]`'s own documentation claimed this
  already worked, and it did not — the header name `x-honeycomb-team` contains no word the name
  rule matches), and a URL's userinfo is replaced with `******` so
  `redis://user:pw@host/db` is described as `redis://******@host/db` (`RedisKvConfig::url`'s
  documentation made the same claim, and the fixture that was supposed to pin it asserted the
  sensitive-named keys only). Both are what make a `${DATABASE_PASSWORD}` or a
  `${TELEMETRY_KEY}` safe to put in the file, which is the pairing this release intends.
- **`spawn_cgi` starts its slots concurrently.** Each slot owns its own address and its own
  process tree, and nothing in the start-up loop reads another slot's state, so starting them
  one at a time was a serialisation with no purpose — every per-slot wait, multiplied by the
  number of slots. At `workers = 32` the pool took 10–14 s to start and now takes about 2.1 s,
  measured on the server's own log timestamps with the resulting `php-cgi` process count as
  ground truth. Port allocation is still serial, and has to be: `port_is_free` is a
  probe-then-bind, so two threads probing at once would hand the same port to two children.
  Only the spawn and the wait run in parallel.
- **`docs/` is a guide a reader can follow**, page by page.

### Fixed

- **`cargo clippy --workspace --all-targets` did not compile, so `Clippy` and `MSRV` were red
  from the commit that added `command` onward.** `spawn_cgi` gained `command`, `capacity` and
  `working_dir`, and the commit updated `tests/pool.rs` but not `tests/frontend.rs`, which still
  built a `SpawnCgiConfig` without them and gave `binary` a `PathBuf` for a field that had become
  `Option<PathBuf>` (E0063 and E0308). The Windows-only unit test in `src/pool.rs` also still
  called `bind_argument` with one argument after it grew a `SpawnDriver` (E0061) — invisible on
  the `ubuntu-latest` `Clippy` job, which is the only place `-D warnings` runs, and never reached
  by the `Test` jobs because they `needs: [fmt, clippy]` and were skipped. A test that does not
  compile is a test that does not run, and `tests/frontend.rs` had not executed since.
- **The CGI environment was wrong for every non-PHP application, in three ways that PHP could
  not reveal.** All three are the same class of bug: the variables were built to PHP's habits
  because PHP was the only backend, and each one is silently wrong for a WSGI or Rack
  application. (1) **The site root carried no `PATH_INFO`.** It was omitted for `/` and for an
  empty value alike, which is fine for PHP — it routes on `REQUEST_URI` — and wrong for a
  runtime that dispatches on `PATH_INFO`, where an absent variable is not the same thing as
  `"/"`: a request for `/` arrived with no path at all. It is now sent whenever it is non-empty,
  including `"/"`. (2) **A bare `Location` was answered `200` instead of `302`.** RFC 3875
  §6.2.3 leaves the status to the server when the application states none, and a `Location`
  with no `Status:` is the spec's way of asking for a redirect; nginx, Caddy and Apache all
  resolve it that way. PHP frameworks always send `Status`, so this could only ever misfire for
  another runtime — and a `302` turned into a `200` is a header no client follows. An explicit
  `Status` still wins. (3) **A request header named `Proxy` became `HTTP_PROXY`** — httpoxy
  (CVE-2016-5385 and its siblings). Python, Ruby and Go HTTP clients read `HTTP_PROXY` from the
  environment to decide where to send their *outbound* requests, so a client sending
  `Proxy: http://attacker/` chose the proxy for the application's own upstream calls. The header
  is now dropped rather than renamed: it has no CGI meaning, and no request may name a proxy.
- **A `spawn_cgi` slot's stdout and stderr were discarded, and pool errors named the wrong
  backend.** The pipes go to `Stdio::null()`, which for `php-cgi` cost a lost extension warning
  and for any other driver cost the traceback that explains a 502 — the same reason
  `upstream_pool` has always drained both. Separately, every transport failure was reported as
  `backend: "spawn_cgi"` regardless of the configured name, so a deployment with two FastCGI
  backends could not tell which one refused a request, and the message named a configuration
  block that may not exist.
- **A path containing `..` could reach a script outside the document root, and php-cgi ran it.**
  `route()` refused any percent-decoded path that climbed out of the root, but `route_or_dispatch()`
  — the function the server actually calls for a route that has already claimed a backend — treated
  that `NotFound` as "names no script" and built a fallback `SCRIPT_FILENAME` from the path, with
  no second check. So `/../secret.php` composed with the root and escaped it, and on Windows
  `/C:/Windows/Temp/evil.php` was worse: `Path::join` discards the root for a drive-prefixed
  component, so the script could be anywhere on the volume. A deployment that lets a request reach
  a path-interpreting backend through a route with a script-extension `path_suffix` could be made
  to execute a `.php` file the site never pointed at — an uploaded file, or one belonging to
  another site on the same host — as the service account. The same refusal `route()` makes is now
  made again inside the fallback, with the reason in the comment: every path arriving there has
  already been answered `NotFound`, so the check has to be repeated rather than assumed.
- **The ACME cache published the account key and the certificate's private key.** The cache holds
  both, and `rustls-acme`'s `DirCache` writes them with the process umask — `0644` in a `0755`
  directory on a stock system — so any local user on the host could read the key to the site's
  certificate, and with the account key could order certificates as that deployment. The cache
  directory is now `0700` and the files in it `0600`, applied on **every** start rather than only
  when the directory is first created, because "already exists" is the case that is already wrong:
  an upgrading deployment finds a `0755` directory and `0644` keys, and a renewal is up to sixty
  days away. A `cache` path that is a file rather than a directory is refused at startup by name,
  instead of surfacing later as a network error from a CA.
- **The installers trusted a checksum fetched from the same server as the archive.** That detects
  corruption and nothing else: whoever can replace the archive can replace the `.sha256` beside
  it, and the default path is `releases/latest/`, which moves. The provenance attestation above
  closes that gap. The `wget` path also gained `--https-only` where the flag exists, which is the
  GNU-wget-only equivalent of the `--proto '=https'` the `curl` path already had, and it is
  guarded so a BusyBox wget without the flag is not broken by it.
- **A `spawn_cgi` slot could adopt a stranger's listener and answer 502.** The port a slot was
  about to use was freed by a `port_is_free` probe and bound by `php-cgi` a moment later, so
  between the two another process could take it. The startup probe connected successfully
  against _somebody else's_ listener, recorded the slot as healthy, and the first real request
  came back a 502 behind an otherwise green startup banner. A connect is now followed by a
  bounded settle and then a check that **our own child is still alive** — which is the only
  evidence that separates "our backend is up" from "someone is on our port". A child that lost
  the bind exits, that exit is now seen, and it becomes the `StartupExited` error the pool
  already retries on a fresh port.
- **`serve` refused an https-only configuration that `check` accepted.** `https` is a listener
  in its own right rather than a mode of `http`, so a deployment whose whole point was to answer
  on `:443` could not start at all while `volucer check` reported the same file as fine.
- **A failed end-to-end test now says why.** The tests spawned the server with stderr discarded,
  so a 502 arrived with no explanation and the only way to diagnose it was to guess and rerun.
  stderr is captured from spawn time and attached to the assertion that failed.

## [0.2.0] - 2026-10-22

### Added

- **`attach` backend** — a worker pool volucer dials instead of spawning. A supervisor boots
  the application once, forks, and listens on a unix relay; volucer connects and starts no
  process at all. Each connection is a slot with the same handshake, exchange thread, recycling
  and metrics as a spawned worker.
- **`spawn_cgi` `children`** — several `php-cgi` children behind one slot, sharing one listening
  socket and one OPcache segment. Measured at 4.5x less resident memory than one process per
  slot for 16 PHP processes, and 9.5x less for 64.
- **`socket` on the worker transport** — where a `transport = "socket"` worker listens, in the
  same dialect `external`, `attach` and `spawn_cgi` already take.
- **`max_connections` for external backends** — opt-in back-pressure. Neither `php-cgi` nor
  `php-fpm` enforces what its `FCGI_MAX_CONNS` advertises, so the value is never inferred from
  the far side.

### Fixed

- **A rotating test failure that was one production port race.** `CgiPool::start` probed a port
  and then handed it to `php-cgi`, which binds it later; one lost bind failed the pool and the
  server printed no banner. `spawn_port_slot` now retries only on `StartupExited`, because that
  error means the bind failed and another port is the fix — a child still running but never
  accepting is a real misconfiguration and is returned unchanged.
- **The prefork supervisor leaked a zombie per recycled connection** — one for each child that
  exited, measured at exactly 10 for 10 connections. `SIGCHLD` with a `WNOHANG` loop is the fix.
- **A killed server left its PHP processes running on Unix.** The process group and
  `PR_SET_PDEATHSIG` were documented in `lifecycle.rs` and implemented neither, so a `SIGKILL`ed
  server leaked `php-cgi` and workers with their ports still bound. Both halves are real now, and
  every kill site goes through them.

### Changed

- **The static layer turns itself off where it can never help.** For a front-controller
  application every request names a path that is not a file, so the layer answered `NotAFile` on
  all of them after a `stat` and a hop to the blocking pool. Measured at 18–26% of throughput on
  `/` and 7–14% on `/items`. An explicit setting still wins.

## [0.1.0] - 2026-10-18

The first release: one binary that serves PHP without embedding it.

### Added

- **Two ways to run a request, behind one dispatcher** — a resident worker that boots the
  application once, and `php-cgi` that boots it per request. One process can answer one route
  from a worker and the next from `php-cgi`, chosen per route in one configuration file.
- **Backends** — `worker` (spawned), `external` (an existing `php-fpm`), `spawn_cgi` (a
  supervised `php-cgi` pool) and an HTTP origin, with supervision, recycling and connection
  recovery; FastCGI over TCP, unix sockets and Windows named pipes.
- **Front-ends** — HTTP on hyper and FastCGI, from the same process, with routing, multiple
  sites, static files, and on-the-fly compression of backend responses.
- **TLS and ACME** — SNI, ALPN, plain and TLS on one port, a certificate per site, automatic
  issuance and renewal, and a redirect from cleartext.
- **Reload** — `SIGHUP` re-reads the configuration, whole-or-nothing, and never stops the
  server; restart-only settings are reported rather than silently ignored.
- **Observability** — access logs, request ids and distributed tracing, rate limiting, metrics
  and a status endpoint, and the client IP behind a proxy.
- **Worker services** — key-value storage, locks, jobs and gRPC from inside a running request,
  over a specified worker protocol with PHP, Python and Ruby SDKs.
- **`volucer check`** — names every listener, every backend and every PHP binary it found, and
  why it chose it.
- **Packaging** — tagged archives for six targets, an installer script for each family, and a
  systemd unit.

[Unreleased]: https://github.com/sailantis/volucer/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/sailantis/volucer/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/sailantis/volucer/releases/tag/v0.1.0
