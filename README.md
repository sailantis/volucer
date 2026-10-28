![Volucer](docs/images/volucer-logo-text.svg)

<h3 align="center">A cross-platform application server for PHP, Python, Ruby and more</h3>

<p align="center">
Run persistent worker applications with Volucer's PHP SDK, serve FastCGI applications, or route HTTP traffic to an existing server.
</p>

---

**Volucer** is a single binary that runs and serves applications on Linux, macOS, Windows and Docker. It provides the HTTP listener, host- and path-based routing, static-file serving, TLS with automatic ACME certificates, process supervision, and — built with the `http3` feature, which every published release is — HTTP/3 over QUIC alongside HTTP/1.1 and HTTP/2.

Use a worker SDK to keep a PHP application running between requests. For applications that use FastCGI, Volucer can connect to an existing server such as `php-fpm` or start and supervise a compatible backend such as `php-cgi`, `gunicorn`, or a Rack server. It can also route requests to an HTTP server written in another language, such as Go or Node.js.

## How requests are handled

Volucer supports two application backends. Both use the same listener and dispatcher, and
you can select a different backend for each route in `volucer.toml`.

| Mode        | Application lifetime           | Request state                            | Use it for                                                     |
| ----------- | ------------------------------ | ---------------------------------------- | -------------------------------------------------------------- |
| **Worker**  | Starts once and stays resident | The application resets its request state | Persistent PHP, Python, or Ruby apps, or any custom worker     |
| **FastCGI** | Managed by the FastCGI backend | Reset by the backend                     | Existing FastCGI apps and apps that need per-request isolation |

**Worker mode is language-agnostic.** Volucer starts a command, completes a protocol
handshake, and keeps the process available for requests. The command can be
`php worker.php`, `python worker.py`, `bundle exec worker.rb`, or any other long-running
program that implements the worker protocol.

**FastCGI works with more than PHP.** Volucer can connect to an existing FastCGI pool with
the `external` backend, or start and supervise a FastCGI server with `spawn_cgi`. This
works with `php-fpm`, `php-cgi`, and other FastCGI-compatible servers, including WSGI,
Rack, and Go applications:

```toml
[backends.py]
kind = "spawn_cgi"
command = "gunicorn myapp.wsgi --threads {concurrency} --bind {address}"
workers = 4        # four slots: the command is started four times, four addresses
concurrency = 4    # requests one slot answers at once, i.e. Gunicorn's --threads above
```

The `command` runs once per slot with a different `{address}`, so a concurrency flag inside it
describes **one** address. `workers × concurrency` is what the host runs. `{concurrency}` is
substituted with that number, so the flag and the key cannot drift.

An attached worker and a preforked `php-cgi` pool can share memory pages. In
benchmarks, this reduced proportional set size (PSS) by 4.5x at 16 PHP processes and
9.5x at 64, compared with one process per slot. See the [benchmark results](docs/12-BENCHMARKS.md)
for the method and measurements.

## Install

### Linux and macOS

```sh
curl -fsSL https://github.com/sailantis/volucer/releases/latest/download/install.sh | sh
```

The installer places the binary in `~/.local/bin` and adds that directory to your `PATH`
if needed. It updates the startup file for your shell (`~/.bashrc`, `~/.profile`,
`~/.zshrc`, or `~/.config/fish/config.fish`) and prints the exact change it made. Set
`VOLUCER_MODIFY_PATH=0` to install without editing a shell startup file.

### Windows

```powershell
powershell -ExecutionPolicy Bypass -c "irm https://github.com/sailantis/volucer/releases/latest/download/install.ps1 | iex"
```

The installer places the binary in `%LOCALAPPDATA%\volucer\bin` and adds it to your user
`PATH`. Open a new shell for the change to take effect. Pass `-ModifyPath 0` (or set
`VOLUCER_MODIFY_PATH=0`) to leave `PATH` unchanged.

### In a container

Each release publishes `linux/amd64` and `linux/arm64` images to GitHub Container
Registry:

```sh
docker pull ghcr.io/sailantis/volucer:latest          # musl, scratch-based
docker pull ghcr.io/sailantis/volucer:latest-glibc    # Debian-based, glibc
```

The default image uses musl and is based on `scratch`; the Debian-based glibc image uses the `-glibc` suffix. For reproducible deployments, pin a version tag such as `0.3.0` instead of using `latest`.

To build either image from a checkout, use `packaging/docker/Dockerfile`:

```sh
docker build -f packaging/docker/Dockerfile --target musl -t volucer .
```

The Docker `HEALTHCHECK` runs `volucer health` and reads its probe address from
`[status]`. This works in the `scratch` image, which does not include a shell or `curl`.
See [Docker operations](docs/11-OPERATIONS.md#docker) for `docker run`, Compose, and guidance on choosing `/health` or `/ready`.

### Build from source

```sh
cargo install --path crates/volucer
```

You can also download a prebuilt archive from the
[releases page](https://github.com/sailantis/volucer/releases).

Create a `volucer.toml` to configure the listener, document root, and worker:

```toml
[server]
http = "0.0.0.0:8080"
document_root = "public"
front_controller = "index.php"
default_backend = "app"

[backends.app]
kind = "worker"
command = "php worker.php"
workers = 8
```

```sh
volucer check      # validate the configuration before you start
volucer serve
```

Before serving requests, run `volucer check` to validate the configuration. It reports
the configured listeners and backends, discovered PHP binaries, and any configuration
warnings.

You can provide the same configuration on the command line instead of using a file. This
is useful for a container `CMD` or systemd `ExecStart`. Use `-o` with any
dot-notated configuration key; `--listen` is shorthand for `server.http`:

```sh
volucer serve --listen 0.0.0.0:8080 \
    -o server.document_root=public \
    -o server.front_controller=index.php \
    -o backends.app.kind=worker \
    -o backends.app.command="php worker.php" \
    -o backends.app.workers=8
```

Unknown keys are rejected, and values use TOML literal syntax. Command-line configuration
uses the same validation and defaults as a configuration file, with two exceptions: arrays
of tables and maps can only be set in a file. The
[configuration guide](docs/01-CONFIGURATION.md#the-command-line-as-a-source) also explains
how relative paths are resolved when no file is present.

[**Getting started →**](docs/00-GETTING-STARTED.md)

## Documentation

The [documentation guide](docs/README.md) links to the full reference. Start with
[Getting Started](docs/00-GETTING-STARTED.md), or browse the topics below:

- [Backends](docs/02-BACKENDS.md) — the five backend types and when to use them
- [Configuration](docs/01-CONFIGURATION.md) — every key, with its default
- [Routing and Sites](docs/03-ROUTING-AND-SITES.md) — many applications, one process
- [Static Files](docs/04-STATIC-FILES.md) — serve files from the document root
- [TLS and ACME](docs/05-TLS-AND-ACME.md) — configure TLS and automatic certificates
- [Observability](docs/06-OBSERVABILITY.md) — access logs, metrics, tracing, rate limiting
- [Worker Applications](docs/07-WORKER-APPLICATIONS.md) — write a resident worker
- [Services](docs/08-SERVICES.md) — key-value, locks, queues and gRPC from inside a request
- [Worker Protocol](docs/09-WORKER-PROTOCOL.md) — the wire protocol, for other languages
- [SDKs and Frameworks](docs/10-SDKS-AND-FRAMEWORKS.md) — the PHP, Python and Ruby SDKs
- [Operations](docs/11-OPERATIONS.md) — reloads, containers, systemd, release process
- [Benchmarks](docs/12-BENCHMARKS.md) — measured, with the method and the raw rows

## Worker SDKs

The worker protocol is language-independent. Use one of the SDKs below, or implement the
[protocol](docs/worker-protocol.md) for another language.

| Language | Package                                                                           | Install                                     |
| -------- | --------------------------------------------------------------------------------- | ------------------------------------------- |
| PHP      | [`sailantis/volucer-php-sdk`](https://github.com/sailantis/volucer-php-sdk)       | `composer require sailantis/volucer-worker` |
| Python   | [`sailantis/volucer-python-sdk`](https://github.com/sailantis/volucer-python-sdk) | `pip install volucer-worker`                |
| Ruby     | [`sailantis/volucer-ruby-sdk`](https://github.com/sailantis/volucer-ruby-sdk)     | `gem install volucer-worker`                |

These SDKs provide the worker transport, not an application framework: they do not impose
a router, dependency-injection container, or middleware pipeline. Connect your application
to the worker channel with a small adapter; WSGI and Rack applications can use the
provided handlers directly.

### PHP

```php
<?php
require __DIR__ . '/vendor/autoload.php';

use Volucer\Request;
use Volucer\Response;
use Volucer\Worker;

Worker::connect()->run(function (Request $request): Response {
    return Response::text('hello ' . $request->uri);
});
```

### Python

```python
from volucer_worker import Worker, wsgi
from myapp import application

Worker.connect().run(wsgi.call(application))
```

Requires Python 3.10 or newer. The SDK uses only the standard library, so the worker
protocol does not add runtime dependencies to your application.

### Ruby

```ruby
require 'volucer'
require 'my_app'

Volucer::Worker.connect.run(&Volucer::Rack.handler(MyApp.new))
```

Requires Ruby 3.1 or newer and has no runtime dependencies beyond the standard library.

Each SDK's `connect()` method uses `VOLUCER_WORKER_SOCKET` when set and otherwise connects
over standard input and output. Use it in a worker entry point. The `create()` method
accepts explicit streams for cases where your application manages the connection itself,
including the `attach` backend.

For Django, Flask, or Rails, the `upstream` backend is often simpler: it can supervise
Gunicorn or Puma over HTTP without a Volucer SDK. The `spawn_cgi` backend can likewise run
a FastCGI server such as `flup` or a Rack adapter. Choose a worker SDK when you want the
worker channel and its built-in `kv`, `lock`, `jobs`, and `metrics` services, worker
recycling based on memory use, or to avoid HTTP framing between Volucer and the application.

Framework bridges build on the PHP SDK — see [SDKs and Frameworks](docs/10-SDKS-AND-FRAMEWORKS.md).

## Contributing

See the [documentation](docs/README.md) for design and implementation details.

```sh
cargo test            # unit and integration tests, including live php-cgi
cargo build --release
```

Integration tests that spawn PHP processes require `php` and `php-cgi` on `PATH`.

## License

MIT — see [LICENSE](LICENSE).
