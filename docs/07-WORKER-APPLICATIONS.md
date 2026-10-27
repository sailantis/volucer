# Worker Applications

This guide explains how to write PHP applications for Volucer's persistent `worker`
backend. For the server-side configuration, see [Backends](02-BACKENDS.md).

## Install the SDK

```sh
composer require sailantis/volucer-php-sdk
```

The SDK uses only the PHP standard library. It requires no extensions, FFI, or build step.

## The shortest worker that works

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

Start the worker with `php worker.php`. It waits for requests using Volucer's worker
protocol over stdin/stdout rather than HTTP. Configure Volucer to launch it:

```toml
[backends.app]
kind = "worker"
command = "php worker.php"
```

Use `Worker::connect()` for workers launched by Volucer. When Volucer sets
`VOLUCER_WORKER_SOCKET`—as it does for the `socket` transport, the default on Windows—the
SDK uses that socket instead of stdio. `Worker::create()` does not read this environment
variable and would attempt to use stdin/stdout, which are not connected to the worker
protocol in this setup. Without the variable, `connect()` behaves like `create()`.

| `VOLUCER_WORKER_SOCKET` value     | Transport                                                 |
| --------------------------------- | --------------------------------------------------------- |
| `127.0.0.1:34567`                 | loopback TCP — the ephemeral port the server bound        |
| `unix:/run/volucer/worker.0.sock` | a unix socket, from `socket = "unix:/run/volucer/worker"` |

## The SDK is a transport, not a framework

The SDK provides transport, not an application framework. It does not include a router,
container, middleware pipeline, or PSR-7 dependency, so it can be used alongside different
frameworks. `Request` and `Response` are plain value objects that can be adapted to your
application's existing interfaces.

## Writing a handler

The handler receives the `Request` and returns one of:

| Return     | Result                               |
| ---------- | ------------------------------------ |
| `Response` | sent exactly as given                |
| `string`   | sent as a `200` `text/html` body     |
| `null`     | the captured output becomes the body |

If the handler throws, the SDK logs the exception to stderr, responds with `500`, and keeps
the worker running so it can handle later requests.

```php
Worker::connect()->run(function (Request $request, Worker $worker): ?Response {
    // Telemetry never touches the response.
    $worker->emitMetric('request_count', 1.0, 'count');
    $worker->emitLog("serving {$request->uri}");

    if ($request->uri === '/health') {
        return Response::json(['status' => 'ok']);
    }

    // Echo case: the captured output is the body, so a plain script works.
    echo 'you asked for ', htmlspecialchars($request->uri, ENT_QUOTES);

    return null;
});
```

## The two rules

The SDK enforces these two rules to keep the persistent worker protocol reliable.

### 1. stdout is the protocol channel

Protocol frames are written to `php://stdout`. Application output written directly to that
stream would corrupt the protocol. The SDK captures ordinary application output instead:

- `ob_start()` runs around each request, so `echo` and template output are collected rather
  than written.
- `ini_set('display_errors', 'stderr')` sends warnings and notices to stderr, which is not
  part of the protocol.

If the handler returns `null`, captured output becomes the response body. This allows a
script that uses `echo` to run as a worker without changing its output code. If the handler
returns a `Response`, captured output is discarded.

Use `emitLog()` and `emitMetric()` to send information to Volucer outside a request. Unlike
FastCGI, the worker protocol also allows the worker to send these messages independently.

Output written directly through `STDOUT` or `php://stdout` cannot be captured by an output
buffer. Such a write can corrupt the protocol; Volucer reports a checksum mismatch.

### 2. Shutdown by construction

When Volucer stops a worker, it closes the child's stdin. The worker loop reads EOF and
returns from `run()`, allowing the process to exit cleanly without signal handling or
polling.

## `Request`

| Member                                  | Notes                                                          |
| --------------------------------------- | -------------------------------------------------------------- |
| `method`, `uri`, `protocol`             | as sent                                                        |
| `headers`                               | `name => value`; `header($name)` looks up case-insensitively   |
| `body`                                  | the raw request body                                           |
| `contentType()`                         | shorthand for the `Content-Type` header                        |
| `query()`                               | the query string parsed the way PHP populates `$_GET`          |
| `cookies()`                             | the `Cookie` header parsed the way PHP populates `$_COOKIE`    |
| `serverVars()`                          | CGI-style variables derived from the request (for legacy code) |
| `remoteAddr`, `scriptFilename`, `https` | as sent                                                        |

## `Response`

```php
Response::text('plain');                       // 200, text/plain
Response::html('<h1>hi</h1>');                 // 200, text/html
Response::json(['ok' => true]);                // 200, application/json
new Response(404, [['Content-Type', 'text/plain']], 'not found');

$response->withStatus(201)->withHeader('X-Trace', 'abc')->withBody('…');
```

`withHeader()` appends a header instead of replacing an existing one. This allows responses
to include multiple headers with the same name, such as multiple `Set-Cookie` headers.

## `Worker`

| Method                             | Purpose                                                                |
| ---------------------------------- | ---------------------------------------------------------------------- |
| `Worker::create($in?, $out?)`      | Bind explicit streams (default: stdio), force `display_errors=stderr`  |
| `Worker::connect()`                | `create()`, but honour `VOLUCER_WORKER_SOCKET` when the server sets it |
| `run($handler)`                    | Handshake, then serve until EOF                                        |
| `emitLog($message, $level)`        | One log record, unsolicited                                            |
| `emitMetric($name, $value, $unit)` | One metric sample, unsolicited                                         |
| `recycle()`                        | Ask the server to replace this worker after the request                |
| `currentRequestId()`               | The in-flight request id, for telemetry correlation                    |

The `$name` passed to `emitMetric()` must match the Prometheus name pattern
`[a-zA-Z_][a-zA-Z0-9_]*` and be declared under the server's `[status.metrics.collect]` table.

Call `recycle()` when the worker should be replaced after its current request, for example
if it has accumulated state that cannot be safely cleared. Volucer starts a replacement;
the current request still receives its normal response.

## Sessions

Workers do not run under a PHP SAPI, so PHP does not automatically manage request cookies
or send response headers through `header()` and `setcookie()`. `Volucer\Session` provides
the session-cookie integration for worker requests.

| Method                       | Purpose                                                                   |
| ---------------------------- | ------------------------------------------------------------------------- |
| `begin($request)`            | Adopt the session the request presented, and relax PHP's cookie handling  |
| `beginWithCookies($cookies)` | The same, for a request that is not a `Volucer\Request`                   |
| `applyTo($response)`         | **Add the `Set-Cookie` and end the request** — the normal call            |
| `cookie()`                   | The `Set-Cookie` value alone, for a response object this SDK does not own |
| `end()`                      | End the session and forget the id, if you did not use `applyTo()`         |

```php
$session = new Session();

$handler = static function (Request $request, Worker $worker) use ($session): Response {
    $session->begin($request);

    session_start();              // the application's own choice, as under FPM
    $_SESSION['seen'] = true;

    return $session->applyTo(Response::text('ok'));
};
```

`applyTo()` adds the session cookie to the response and ends the session. Ending the session
also clears its ID, so call `applyTo()` before any separate call to `end()` that would clear
the ID needed for the cookie. Calling `end()` after `applyTo()` is harmless.

The SDK does not start sessions automatically. If a request does not call `session_start()`,
it does not create a session or return a session cookie.

## Clearing your own request scope

Worker processes handle multiple requests, so application state can persist between them.
Reset request-specific state after each request. The exact cleanup depends on your
framework, but commonly includes:

- Anything you put in a global, a static property or a singleton.
- Database connections opened at worker startup, if they should not be shared across
  requests.
- Anything you cached that was true only for the request you served.

Avoid clearing reusable state such as loaded classes, compiled templates, and warmed
application caches unless your framework requires it.

## Memory

Each `END` frame reports `peak_memory_bytes` for the current request. The SDK reads
`memory_get_peak_usage(true)` and then calls `memory_reset_peak_usage()`, so a temporary
memory spike in one request does not affect the reported peak for subsequent requests.
`memory_reset_peak_usage()` is available in PHP 8.2 and later, which is also the SDK's
minimum supported version.

## Attaching instead of spawning

The `worker` backend launches worker processes. The `attach` backend instead connects to a
relay served by your application, using one connection per worker slot.

This model lets the application boot once and then fork workers, so each child inherits the
already-loaded interpreter, classes, and OPcache segment.

The application must own the fork: it also owns the listening socket, and the PHP SDK does
not pass that socket to a separate Volucer process. `Volucer\Supervisor` provides the
application-side supervisor:

```php
<?php
// supervisor.php — boot once, then fork
require __DIR__ . '/vendor/autoload.php';

$app = new App();
$app->bootstrap();                  // boot before the fork, or every child boots its own

Volucer\Supervisor::serve(
    null,                           // or 'unix:/run/app/workers.sock'
    static fn (Volucer\Request $request): Volucer\Response => new Volucer\Response(
        200,
        [['Content-Type', 'text/html; charset=utf-8']],
        $app->handle($request),
    ),
);
```

```toml
[backends.app]
kind    = "attach"
workers = 16
```

The handler has the same signature as the callable passed to `Worker::run()`. In many
applications, adapting a worker script to run under the supervisor only requires changing
the final call.

### The relay address

If the first argument is `null`, the supervisor reads `VOLUCER_RELAY_SOCKET`. The server
uses the same variable when `relay` is omitted:

```ini
# the unit file, or a container's environment
Environment=VOLUCER_RELAY_SOCKET=unix:/run/app/workers.sock
```

Unlike `VOLUCER_WORKER_SOCKET`, which Volucer sets for a spawned worker, the operator sets
`VOLUCER_RELAY_SOCKET` for both sides. An explicit address passed to `serve()` or configured
as `relay` takes precedence over the environment variable.

### `children` needs no tuning

The third argument limits the number of child processes. Its default, `0`, means no
additional limit. In an `attach` deployment, Volucer opens one connection per configured
worker slot, which already bounds the number of active children.

### Boot before the fork, connect after it

Forking after application boot lets each child inherit the initialized runtime and reusable
in-memory state.

Do not open database connections before forking if the children must use independent
connections. Each child would inherit the same socket, and concurrent use could corrupt
communication. Open connections lazily in each child, for example when handling its first
request.

### Unix only

Windows does not provide `pcntl`, so `Supervisor::serve()` raises an error there rather
than silently running a single process. On Windows, use `kind = "worker"` or
`kind = "upstream"`.

## Writing a supervisor yourself

`Supervisor::serve()` is optional. A custom supervisor can support the `attach` backend as
long as it serves a connection that it opened, using `Worker::create()`:

```php
$listener = stream_socket_server('unix:///run/app/workers.sock', $errno, $errstr);

while (true) {
    $connection = stream_socket_accept($listener, -1);
    $pid = pcntl_fork();

    if ($pid !== 0) {
        fclose($connection);        // the master must drop its copy
        continue;
    }

    fclose($listener);
    Worker::create($connection, $connection)->run($handler);
    exit(0);                        // not a return: a child must not run the master's shutdown
}
```

Use `Worker::create()`, not `connect()`, because the accepted connection is passed directly
to the worker. Accept each connection in the master process before forking. If all children
share the listener and accept independently, connection distribution is not guaranteed;
one child could receive multiple connections while another receives none.

## Consuming jobs

The same worker file can consume a queue instead of serving HTTP:

```php
Worker::connect()->consume(function (Volucer\Job $job, Worker $worker) {
    $worker->emitLog("processing {$job->id}");

    return true;          // ack
});
```

The handler's return value determines the job outcome:

| Return                          | Outcome                             |
| ------------------------------- | ----------------------------------- |
| anything else, including `true` | `ack` — the job is done             |
| `false`                         | `requeue` — run it again            |
| a string                        | `reject` — the string is the reason |

If the handler throws, the SDK requeues the job rather than acknowledging it. The job lease
then expires before it is retried, preventing an immediate retry loop.

See [Services](08-SERVICES.md) for the queue API.

## An example to copy

The SDK includes a runnable worker at `example/worker.php` with `/`, `/health`, `/echo`,
`/big`, `/boom`, and `/recycle` routes. The example demonstrates request handling, exception
recovery, and worker recycling.
