![Volucer](docs/images/volucer-logo-text.svg)

<h3 align="center">A cross-platform application server built to speed up PHP, Python and Ruby</h3>

<p align="center">
Run PHP, Python and Ruby applications with one binary, using worker or FastCGI backends. Other languages can be served as well.
</p>

---

**Volucer** runs cross-platform applications. It starts the application, supervises it, and serves it over HTTP or FastCGI.
For PHP it starts `php`, `php-cgi`, or connects to an existing `php-fpm` process, so applications keep their existing PHP version, extensions, and `php.ini`.
For Python and Ruby SDKs exist that speak the worker protocol.
Volucer can also run a FastCGI server — `gunicorn`, `flup`, a Rack adapter, or any other FastCGI-compatible backend.
Volucer provides the listener, routing, static-file serving, TLS, and process supervision.

It runs on Linux, macOS, and Windows. Windows does not provide `php-fpm`; Volucer can supervise `php-cgi` there.

## Two ways to run a request

|                  | Boots the application | State between requests     | Good for                         |
| ---------------- | --------------------- | -------------------------- | -------------------------------- |
| **Worker mode**  | Once, at startup      | Managed by the application | PHP, Python or Ruby applications |
| **FastCGI mode** | On every request      | Reset by the backend       | Any FastCGI application          |

Both modes use the same dispatcher. Routes can use different backends, configured in `volucer.toml`.

**Worker mode is language-agnostic.** It starts a process, performs a protocol handshake, and keeps it resident — the command is yours to choose, so it can be `php worker.php`, `python worker.py` or `bundle exec worker.rb` or any other command that runs a long-lived process.

**FastCGI mode is per-request.** A FastCGI-speaking WSGI, Rack or Go server is served exactly as `php-fpm` is. The backend either attaches to a pool somebody else runs (`external`), or Volucer starts and supervises the processes itself (`spawn_cgi`) — with `php-cgi` as one driver and any other FastCGI server as another:

```toml
[backends.py]
kind = "spawn_cgi"
command = "gunicorn myapp.wsgi --bind {address}"
workers = 4
capacity = 4
```

For PHP, Python and Ruby, worker mode is usually the better of the two, because it boots the application once instead of per request.

Worker mode avoids repeated application boot. An attached worker and a preforked `php-cgi` pool can also share memory pages: benchmarks measured 4.5x lower PSS than one process per slot at 16 PHP processes and 9.5x lower at 64. See the [benchmarks](docs/12-BENCHMARKS.md) for the method and results.

## Install

### Linux and macOS

```sh
curl -fsSL https://github.com/sailantis/volucer/releases/latest/download/install.sh | sh
```

The binary lands in `~/.local/bin`, which the installer adds to your `PATH` if it is not
already there. It writes to the file your shell actually reads — `~/.bashrc` (or `~/.profile`
when you have none), `~/.zshrc`, or `~/.config/fish/config.fish` — and prints the file and the
exact line it added, so you can take it back out. Set `VOLUCER_MODIFY_PATH=0` to install
without touching a dotfile.

### Windows

```powershell
powershell -ExecutionPolicy Bypass -c "irm https://github.com/sailantis/volucer/releases/latest/download/install.ps1 | iex"
```

The binary lands in `%LOCALAPPDATA%\volucer\bin`, which the installer adds to your user `PATH`. Open a new shell afterwards for that to take effect. Pass `-ModifyPath 0` (or set `VOLUCER_MODIFY_PATH=0`) to leave the environment alone.

### In a container

`packaging/docker/Dockerfile` builds two images from one crate — a glibc image and a `scratch`-based musl image:

```console
docker build --target musl -t volucer .
```

The image declares a `HEALTHCHECK` that runs `volucer health`, which reads the probe address from `[status]` — the `scratch` target has no shell and no `curl`, so the check has to be the binary itself. See [Operations](docs/11-OPERATIONS.md#docker) for the `docker run` and Compose invocations, and for which of `/health` and `/ready` belongs in which place.

### Build from source

```sh
cargo install --path crates/volucer
```

Or take a prebuilt archive from the [releases page](https://github.com/sailantis/volucer/releases).
Then write a `volucer.toml` and serve:

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

`volucer check` reports configured listeners and backends, discovered PHP binaries, and
configuration warnings.

**A file is not required.** The same deployment can be stated on the command line, which is what
a container `CMD` or a systemd `ExecStart` wants — `-o` takes any key in `dot.notation`, and
`--listen` is shorthand for `server.http`:

```sh
volucer serve --listen 0.0.0.0:8080 \
    -o server.document_root=public \
    -o server.front_controller=index.php \
    -o backends.app.kind=worker \
    -o backends.app.command="php worker.php" \
    -o backends.app.workers=8
```

Unknown keys are refused and the value is read as a TOML literal, so the strictness and the
defaults are the file's own. See [Configuration](docs/01-CONFIGURATION.md#the-command-line-as-a-source)
for the two limits (arrays of tables and maps stay file-only) and for how relative paths resolve
when there is no file.

[**Getting started →**](docs/00-GETTING-STARTED.md)

## Documentation

The full guide is in [`docs/`](docs/README.md). Start with
[Getting Started](docs/00-GETTING-STARTED.md), then:

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

## Write a worker in any language

The worker protocol is specified, and the implementations use the same wire format. If your
language is not listed, use the [specification](docs/worker-protocol.md) to implement a worker.

| Language | Package                                                                           | Install                                     |
| -------- | --------------------------------------------------------------------------------- | ------------------------------------------- |
| PHP      | [`sailantis/volucer-php-sdk`](https://github.com/sailantis/volucer-php-sdk)       | `composer require sailantis/volucer-worker` |
| Python   | [`sailantis/volucer-python-sdk`](https://github.com/sailantis/volucer-python-sdk) | `pip install volucer-worker`                |
| Ruby     | [`sailantis/volucer-ruby-sdk`](https://github.com/sailantis/volucer-ruby-sdk)     | `gem install volucer-worker`                |

All three are transports, not frameworks: no router, no container, no middleware pipeline. The
SDK is a codec and a channel, and adapting it to whatever your application already speaks is a
few lines. A plain WSGI or Rack application needs no adapter at all.

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

Python 3.10 or newer, and nothing to install beyond the standard library. A worker SDK loads
before an application's own environment is known to work, so a dependency here is a failure mode
unrelated to the protocol.

### Ruby

```ruby
require 'volucer'
require 'my_app'

Volucer::Worker.connect.run(&Volucer::Rack.handler(MyApp.new))
```

Ruby 3.1 or newer, and no runtime dependencies beyond the standard library, for the same reason.

Each SDK calls `connect()`, which honours `VOLUCER_WORKER_SOCKET` and falls back to stdio. That
is the call a worker file should make; `create()` is the explicit-streams form, used when you
are binding streams yourself — including when the connection was opened by somebody else, which
is what the `attach` backend needs.

**For a Django, Flask or Rails application, the `upstream` backend is the shorter path.** It
supervises gunicorn or Puma over HTTP and needs no SDK at all. The `spawn_cgi` backend is the
other one: it runs any FastCGI server, so a `flup` or a Rack adapter works with no SDK either.
Use the SDK when the worker channel itself is what you want: the `kv`, `lock`, `jobs` and
`metrics` services, recycling on RSS, and no HTTP framing on the hot path.

Framework bridges build on the PHP SDK — see [SDKs and Frameworks](docs/10-SDKS-AND-FRAMEWORKS.md).

## Contributing

See the documentation under [`docs/`](docs/README.md) for design and implementation details.

```sh
cargo test            # unit and integration tests, including live php-cgi
cargo build --release
```

Integration tests that spawn PHP processes require `php` and `php-cgi` on `PATH`.

## License

MIT — see [LICENSE](LICENSE).
