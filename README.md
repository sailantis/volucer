![Volucer](docs/images/volucer-logo-text.svg)

<h3 align="center">A cross-platform PHP application server.</h3>

<p align="center">
Serve PHP from one binary — no nginx, no php-fpm, no orchestration to hold it together.
</p>

---

**volucer** runs PHP applications without embedding PHP. `php`, `php-cgi` and `php-fpm` are
started as ordinary external processes, which keeps your PHP exactly as it is: same version, same
extensions, same `php.ini`. What volucer replaces is the layer around it — the listener, the
routing, static files, TLS, the pool that supervises PHP and the supervisor that keeps the pool
alive.

It works on Linux, macOS and Windows. On Windows that gap is total: there is no `php-fpm` at all,
and nothing supervises `php-cgi`.

## Two ways to run a request

| | Boots the application | State between requests | Good for |
|---|---|---|---|
| **Worker mode** | Once, at startup | Reset by the application | APIs, and anything where framework boot dominates |
| **FastCGI mode** | On every request | Reset by PHP itself | Existing applications, unchanged |

Both are served by one dispatcher, so a single process can answer one route from a resident worker
and the next from a freshly booted `php-cgi`. You pick per route, in one configuration file.

The choice is not cosmetic. A resident worker avoids the framework boot entirely, and a preforked
`php-cgi` pool shares its pages copy-on-write — measured **4.5x less** resident memory than one
process per slot for the same 16 PHP processes, and **9.5x less** for 64. Both are in the
[benchmarks](docs/12-BENCHMARKS.md).

## Install

```sh
cargo install --path crates/volucer
```

Or take a prebuilt archive from the [releases page](https://github.com/sailantis/volucer/releases).
Then write a `volucer.toml` and serve:

```toml
default_backend = "app"

[server]
http = "0.0.0.0:8080"
document_root = "public"
front_controller = "index.php"

[backends.app]
kind = "worker"
command = "php worker.php"
workers = 8
```

```sh
volucer check      # validate the configuration before you start
volucer serve
```

`volucer check` names every listener, every backend and every PHP binary it found, and why it
chose it. It is the first thing to run when something does not work.

[**Getting started →**](docs/00-GETTING-STARTED.md)

## Documentation

The full guide is in [`docs/`](docs/README.md). Start with
[Getting Started](docs/00-GETTING-STARTED.md), then:

- [Backends](docs/02-BACKENDS.md) — the five ways volucer can run PHP, and which to pick
- [Configuration](docs/01-CONFIGURATION.md) — every key, with its default
- [Routing and Sites](docs/03-ROUTING-AND-SITES.md) — many applications, one process
- [Static Files](docs/04-STATIC-FILES.md) — [TLS and ACME](docs/05-TLS-AND-ACME.md)
- [Observability](docs/06-OBSERVABILITY.md) — access logs, metrics, tracing, rate limiting
- [Worker Applications](docs/07-WORKER-APPLICATIONS.md) — write a resident PHP worker
- [Services](docs/08-SERVICES.md) — key-value, locks, queues and gRPC from inside a request
- [Worker Protocol](docs/09-WORKER-PROTOCOL.md) — the wire protocol, for other languages
- [SDKs and Frameworks](docs/10-SDKS-AND-FRAMEWORKS.md) — PHP, Python and Ruby
- [Operations](docs/11-OPERATIONS.md) — reloads, containers, systemd, release process
- [Benchmarks](docs/12-BENCHMARKS.md) — measured, with the method and the raw rows

## Write a worker in any language

The worker protocol is specified — not implied — and every implementation agrees on it byte for
byte. If your language is not listed, the
[specification](docs/worker-protocol.md) is complete enough to write one.

| Language | Package |
|---|---|
| PHP | [`sailantis/volucer-php-sdk`](https://github.com/sailantis/volucer-php-sdk) |
| Python | [`sailantis/volucer-python-sdk`](https://github.com/sailantis/volucer-python-sdk) |
| Ruby | [`sailantis/volucer-ruby-sdk`](https://github.com/sailantis/volucer-ruby-sdk) |

Framework bridges build on those — see [SDKs and Frameworks](docs/10-SDKS-AND-FRAMEWORKS.md).

## Contributing

The design journals in [`internal/`](internal/README.md) record **why** each decision was made,
including the ones that were reversed. They are working notes rather than documentation, kept
because the source comments cite them by section number.

```sh
cargo test            # unit and integration tests, including live php-cgi
cargo build --release
```

The integration tests spawn real PHP processes, so `php` and `php-cgi` need to be on `PATH`.

## License

MIT — see [LICENSE](LICENSE).
