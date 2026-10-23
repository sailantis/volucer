# SDKs and Frameworks

**The PHP, Python and Ruby SDKs, and the framework integrations built on them.**

## The SDKs

Three SDKs implement the other end of the [worker protocol](09-WORKER-PROTOCOL.md), and each
pins the same frozen wire bytes as the Rust tests. They live in separate repositories
because each one's package manager needs a manifest at the root.

| SDK | Package | Install |
|---|---|---|
| PHP | `sailantis/volucer-worker` | `composer require sailantis/volucer-worker` |
| Python | `volucer-worker` | `pip install volucer-worker` |
| Ruby | `volucer-worker` | `gem install volucer-worker` |

### PHP

The most complete of the three, and the one the framework bridges are built on.

It is **stdlib-only**: no extensions, no FFI, no build step, and no Composer requirement just
to *run* a worker.

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

See [Worker Applications](07-WORKER-APPLICATIONS.md) for the full API.

### Python

```python
from volucer_worker import Worker, Response

def handle(request, worker):
    return Response.text(f"hello {request.uri}")

Worker.connect().run(handle)
```

`Worker.connect()` honours `VOLUCER_WORKER_SOCKET` and falls back to stdio. It is the call a
worker file should make; `Worker.create()` is the explicit-streams form, used when you are
binding streams yourself.

A plain WSGI application needs no adapter beyond what the package already has:

```python
from volucer_worker import Worker, wsgi
from myapp import application

Worker.connect().run(wsgi.call(application))
```

Python 3.10 or newer. Nothing is required at runtime beyond the standard library, and there
are no optional dependencies — a worker SDK loads before an application's own environment is
known to work, so a dependency here is a failure mode unrelated to the protocol.

**If you want the shortest path to "my Django or Flask app runs behind volucer", use the
`upstream` backend instead.** It supervises gunicorn over HTTP, needs no SDK, and is the
easier route. The SDK exists for the case where the worker channel itself is what you want:
the `kv`, `lock`, `jobs` and `metrics` services, recycling on RSS, and no HTTP framing on
the hot path.

### Ruby

```ruby
require 'volucer'
require 'my_app'

Volucer::Worker.connect.run(&Volucer::Rack.handler(MyApp.new))
```

Any Rack application works, including a Rails app. `Volucer::Worker.connect` honours
`VOLUCER_WORKER_SOCKET` and falls back to stdio; `Volucer::Worker.create` is the
explicit-streams form, used when you are binding streams yourself.

Ruby 3.1 or newer. No runtime dependencies beyond the standard library, deliberately: a
worker SDK is loaded before an application's own gems are known to work.

**If you want the shortest path to "my Rails or Sinatra app runs behind volucer", use the
`upstream` backend instead.** It supervises Puma over HTTP, needs no gem, and is the easier
route.

## The framework bridges

Four bridges exist, each putting a framework on the worker channel. They are **bridges, not
upstream changes** — the shape `baldinof/roadrunner-bundle` established for Symfony, and the
only one available: a framework's kernel has no "server" concept to extend, so a bridge is a
package that boots the framework and calls its entry point in a loop.

### Why a bridge rather than the `upstream` backend

The `upstream` backend supervises Puma, gunicorn, FrankenPHP or `php artisan serve` over
HTTP and needs no package at all. That is the easier route, and it is the right one for many
applications.

A bridge exists for the case where **the worker channel itself** is what you want:

- the `kv`, `lock`, `jobs` and `metrics` services, reachable from inside a request
- recycling on RSS, with the framework's own container still warm
- no HTTP framing on the hot path

### Spiral

```console
composer require sailantis/spiral-volucer
```

```php
<?php
// app.php — the worker entry point

use Sailantis\SpiralVolucer\WorkerRunner;
use Spiral\Framework\Kernel;
use Spiral\Http\Http;

require __DIR__ . '/vendor/autoload.php';

$app = Kernel::create(
    directories: ['root' => __DIR__],
    drivers: [],   // your bootloaders
)->build();

$container = $app->getContainer();

(new WorkerRunner($container->get(Http::class), $container))->run();
```

**`ContainerInterface::runScope()` is exactly the per-request isolation primitive the
protocol asks for.** Everything created inside a scope — the PSR-7 request, its bindings,
every resolved instance — is discarded when the scope ends, and the outer container keeps
only what is genuinely application-scoped.

That is not a coincidence: RoadRunner support is why the scope exists. So this bridge does
not invent a reset. It runs your existing dispatcher inside a scope and lets the framework do
the thing it was built to do.

### Symfony

```console
composer require sailantis/symfony-volucer
```

Register the bundle in `config/bundles.php`:

```php
return [
    // …
    Sailantis\SymfonyVolucer\SymfonyVolucerBundle::class => ['all' => true],
];
```

Then create a worker entry point that boots the kernel and calls `handle()` in a loop. The
kernel is rebooted per request, so no upstream change is needed.

### Laravel

```console
composer require sailantis/laravel-volucer
```

Register the provider in `bootstrap/providers.php`:

```php
return [
    // …
    Sailantis\LaravelVolucer\LaravelVolucerServiceProvider::class,
];
```

**This is a standalone bridge and not an Octane driver.** Octane's engine list is hardcoded
in `StartCommand`, so `octane:start --server=volucer` would be an upstream pull request
rather than something a package can add. What a package *can* do is boot the framework
itself and call into it in a loop.

### Azera

```console
composer require sailantis/azera-volucer
```

Point a `worker` backend at the entry point the package provides:

```toml
[server]
http = "0.0.0.0:8001"
document_root = "/srv/app/public"
front_controller = "index.php"
default_backend = "app"

[backends.app]
kind = "worker"
command = "php vendor/sailantis/azera-volucer/azera-worker.php"
workers = 4
```

Your existing routes, controllers, middleware and templates run unchanged. The package
reuses the Azera `AppContext` DI container, Router, Dispatcher and middleware pipeline across
requests, so the framework's own machinery keeps working the way it does under PHP-FPM.

## `volucer-cli`

A Composer package that downloads the volucer binary for your platform, verifies its
checksum, and installs it.

```console
composer require --dev sailantis/volucer-cli
vendor/bin/volucer-cli
```

**The server is a single Rust binary.** This package is the Composer-shaped way to get it, so
a PHP project can say `composer require` and have a working server.

```
volucer-cli [install] [options]

  --release <tag>     Release to install, e.g. 0.1.0 or v0.1.0.
  --location <path>   Where to put the binary. The current directory by default.
  --force             Replace an existing binary.
  --dry-run           Resolve the release and its checksum, download nothing,
                      and install nothing.
  --api <url>         GitHub API root.
  --version           Print this tool's version.
  --help              This text.
```

The order it does things in is the point: it fetches the archive to a **temporary file**,
fetches the release's own `SHA-256` and compares it **before anything is extracted**, and
only then moves the binary into place. A failure at any step leaves your existing binary
exactly as it was.

**This is not the supported way to install volucer.** The install scripts in the server
repository are, because they need nothing but `curl` or `wget` and `tar` — which every
platform volucer runs on has, and which PHP does not supply. A Composer installer would be a
*second* implementation of the same download, in the one language whose users are a third of
volucer's, needing its own tar reader, its own checksum logic and its own defences against a
hostile archive.

It is kept for anyone who wants to read the PHP, and it is a working downloader.

## Which one should I use?

| If you want | Use |
|---|---|
| The simplest path, and a running server now | The [install scripts](00-GETTING-STARTED.md#install) |
| PHP on the worker channel | `sailantis/volucer-worker` |
| Django, Flask or Rails on the worker channel | The Python or Ruby SDK |
| An existing PHP framework, on the worker channel | The framework bridge |
| An existing app, changed as little as possible | `external` or `spawn_cgi` — see [Backends](02-BACKENDS.md) |
| Node, Go or any other HTTP origin | The `upstream` backend |
