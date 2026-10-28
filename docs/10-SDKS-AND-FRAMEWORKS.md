# SDKs and Frameworks

Volucer provides worker SDKs for PHP, Python, and Ruby, along with integrations for
selected PHP frameworks.

## The SDKs

The SDKs implement the worker side of the [worker protocol](09-WORKER-PROTOCOL.md). Their
repositories include package-manager manifests and tests for the protocol's wire format.

The PHP and Python SDKs also carry a **client for the `[rpc]` call plane**, which a FastCGI or
`upstream` application uses to reach `kv`, `lock` and `jobs` without being a worker. The Ruby SDK
carries one too: `Volucer::Rpc`, which reads the call plane from a Rack environment.

| SDK    | Package                    | Install                                     |
| ------ | -------------------------- | ------------------------------------------- |
| PHP    | `sailantis/volucer-worker` | `composer require sailantis/volucer-worker` |
| Python | `volucer-worker`           | `pip install volucer-worker`                |
| Ruby   | `volucer-worker`           | `gem install volucer-worker`                |

The SDKs provide transport rather than a framework: they do not require a router, container,
or middleware pipeline. Their `Request` and `Response` objects are plain value objects.
Python WSGI and Ruby Rack applications can be connected using the supplied adapters.

Python and Ruby workers do not require PHP to be installed on the host. The PHP SDK requires
PHP 8.2 or later because it uses `memory_reset_peak_usage()` to report peak memory per
request. This is a PHP SDK requirement, not a server-side restriction; workers that do not
report a PHP version are supported.

### PHP

The PHP SDK is also used by the framework integrations described below.

The SDK uses PHP's standard `hash` and `json` extensions. It does not require FFI or a build
step.

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

`Worker.connect()` uses `VOLUCER_WORKER_SOCKET` when Volucer sets it and otherwise falls
back to stdio. Use it in a worker entry point. `Worker.create()` is for explicitly
providing the streams.

Use the supplied `wsgi` adapter to serve an existing WSGI application:

```python
from volucer_worker import Worker, wsgi
from myapp import application

Worker.connect().run(wsgi.call(application))
```

The SDK requires Python 3.10 or later and has no runtime dependencies beyond the standard
library. This keeps worker startup independent of optional SDK dependencies.

For Django and Flask applications, Volucer's `upstream` backend can supervise gunicorn over
HTTP without a worker SDK. The `spawn_cgi` backend can also run a FastCGI server, such as
gunicorn's `flup` worker or a `flup-fcgi` process. Use the worker SDK when you need
worker-channel features such as metrics, memory-based worker recycling, or direct request
handling without HTTP between the server and worker.

**Key-value storage and locks no longer require the worker channel.** `[rpc]` serves the same
namespaces on a socket, and `Rpc` in this SDK dials it, so a WSGI application under `spawn_cgi`
reaches `kv`, `lock` and `jobs` while remaining an ordinary WSGI application:

```python
from volucer_worker import Rpc

def application(environ, start_response):
    # `environ` is required: volucer puts the address in the CGI request, not in the process.
    rpc = Rpc.connect(environ=environ)
    rpc.kv("cache").set("greeting", "hello")
```

### Ruby

```ruby
require 'volucer'
require 'my_app'

Volucer::Worker.connect.run(&Volucer::Rack.handler(MyApp.new))
```

The Rack adapter supports Rack applications, including Rails. `Volucer::Worker.connect`
uses `VOLUCER_WORKER_SOCKET` when available and otherwise uses stdio. Use
`Volucer::Worker.create` when providing streams explicitly.

A Rack application that is **not** a worker — a FastCGI or `upstream` backend — reaches the
same services through `Volucer::Rpc`, passing its own environment:

```ruby
def call(env)
  rpc = Volucer::Rpc.connect(env: env)
  rpc.kv('cache').set('greeting', 'hello')
  [200, { 'content-type' => 'text/plain' }, ['ok']]
end
```

`env:` is not optional there, and the reason is the same one the Python snippet gives: volucer
puts the address in the _request_, so the process's `ENV` does not carry it.

The SDK requires Ruby 3.1 or later and has no runtime dependencies beyond the standard
library.

For Rails and Sinatra applications, the `upstream` backend can supervise Puma over HTTP
without the worker SDK. The `spawn_cgi` backend can run a FastCGI Rack server as well.

## The framework bridges

Four framework bridges are available. Each boots the framework and invokes its entry point
from a worker loop without requiring changes to the framework.

### Why a bridge rather than the `upstream` backend

The `upstream` backend can supervise an existing HTTP application, including Puma, gunicorn,
FrankenPHP, or `php artisan serve`, without a worker package.

Use a framework bridge when you need worker-channel features such as:

- the `kv`, `lock`, `jobs` and `metrics` services, reachable from inside a request
- recycling on RSS, with the framework's own container still warm
- no HTTP framing on the hot path

### Spiral

```sh
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

`ContainerInterface::runScope()` isolates each request. When the scope ends, objects created
within it—including the PSR-7 request, its bindings, and resolved instances—are discarded.
The outer container retains application-scoped objects. The bridge runs Spiral's existing
dispatcher inside this scope.

### Symfony

```sh
composer require sailantis/symfony-volucer
```

Register the bundle in `config/bundles.php`:

```php
return [
    // …
    Sailantis\SymfonyVolucer\SymfonyVolucerBundle::class => ['all' => true],
];
```

Create a worker entry point that boots the kernel and calls `handle()` for each request. The
kernel is rebooted per request; no changes to Symfony itself are required.

### Laravel

```sh
composer require sailantis/laravel-volucer
```

Register the provider in `bootstrap/providers.php`:

```php
return [
    // …
    Sailantis\LaravelVolucer\LaravelVolucerServiceProvider::class,
];
```

This package is a standalone bridge, not a Laravel Octane driver. Octane's supported engine
list is hardcoded in `StartCommand`, so adding Volucer to `octane:start --server=volucer`
would require an upstream change. The bridge instead boots Laravel and invokes it in a
worker loop.

### Azera

```sh
composer require sailantis/azera-volucer
```

Configure a `worker` backend to launch the entry point provided by the package:

```toml
[server]
http = "0.0.0.0:8001"
document_root = "/srv/app/public"
front_controller = "index.php"

[backends.app]
kind = "worker"
command = "php vendor/sailantis/azera-volucer/azera-worker.php"
workers = 4
```

The package reuses Azera's `AppContext` dependency-injection container, router, dispatcher,
and middleware pipeline across requests.

## `volucer-cli`

`volucer-cli` is a Composer package that downloads and installs the Volucer binary for your
platform after verifying its checksum.

```sh
composer require --dev sailantis/volucer-cli
vendor/bin/volucer-cli
```

Volucer is distributed as a single Rust binary. This Composer package installs that binary
from a release.

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

The installer downloads the archive to a temporary file and compares it with the release's
`SHA-256` checksum before extracting anything. It then installs the binary. If any step
fails, the existing binary is left unchanged.

The installation scripts in the server repository are the recommended installation
method. They use common command-line tools and avoid maintaining a separate implementation
of archive extraction and checksum verification. The Composer installer remains available
as an alternative.

## Which one should I use?

| If you want                                         | Use                                                                                                                    |
| --------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------- |
| The simplest path, and a running server now         | The [install scripts](00-GETTING-STARTED.md#install)                                                                   |
| PHP on the worker channel                           | `sailantis/volucer-worker`                                                                                             |
| Django, Flask or Rails on the worker channel        | The Python or Ruby SDK                                                                                                 |
| An existing PHP framework, on the worker channel    | The framework bridge                                                                                                   |
| An existing app, changed as little as possible      | `external` or `spawn_cgi` — see [Backends](02-BACKENDS.md)                                                             |
| Any FastCGI server, started and supervised          | `spawn_cgi` with a `command` — see [Backends](02-BACKENDS.md#any-fastcgi-server)                                       |
| `kv` or `lock` from a FastCGI app, without a worker | `[rpc]` plus `Rpc` in the PHP, Python or Ruby SDK — see [Services](08-SERVICES.md#an-application-that-is-not-a-worker) |
| Node, Go or any other HTTP origin                   | The `upstream` backend                                                                                                 |
