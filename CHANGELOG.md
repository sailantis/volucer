# Changelog

All notable changes to volucer are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this
project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Entries name the failure a change removes, not only the feature it adds, because the failures
are what an operator has to recognise. Where a number decided a change, the number is here —
the reasoning behind it lives in the source comments and in [`internal/plan.md`](internal/plan.md).

## [Unreleased]

### Added

- **`CHANGELOG.md`.** What changed between two tags could only be read out of the commit log,
  and the release notes for 0.1.0 and 0.2.0 were written after the fact. This file is that
  description, kept as the releases happen — and it is why `docs/11-OPERATIONS.md` now sends an
  operator here before an upgrade instead of to "the release notes".

### Changed

- **`spawn_cgi` starts its slots concurrently.** Each slot owns its own address and its own
  process tree, and nothing in the start-up loop reads another slot's state, so starting them
  one at a time was a serialisation with no purpose — every per-slot wait, multiplied by the
  number of slots. At `workers = 32` the pool took 10–14 s to start and now takes about 2.1 s,
  measured on the server's own log timestamps with the resulting `php-cgi` process count as
  ground truth. Port allocation is still serial, and has to be: `port_is_free` is a
  probe-then-bind, so two threads probing at once would hand the same port to two children.
  Only the spawn and the wait run in parallel.
- **`docs/` is a guide a reader can follow**, page by page. The design journals that had been
  published as documentation moved to [`internal/`](internal/README.md), where the source
  comments that cite them by hazard id still find them. A stale note in a published page is a
  public claim, and there were a lot of them.

### Fixed

- **A `spawn_cgi` slot could adopt a stranger's listener and answer 502.** The port a slot was
  about to use was freed by a `port_is_free` probe and bound by `php-cgi` a moment later, so
  between the two another process could take it. The startup probe connected successfully
  against *somebody else's* listener, recorded the slot as healthy, and the first real request
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
