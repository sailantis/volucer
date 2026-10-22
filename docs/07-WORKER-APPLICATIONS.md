# Worker Applications

**Writing a PHP worker: an application that boots once and serves many requests.**

This is the `worker` backend from the application's side. The server side is in
[Backends](02-BACKENDS.md).

## Install the SDK

```console
composer require sailantis/volucer-php-sdk
```

It is **stdlib-only**: no extensions, no FFI, no build step.

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

Start it with `php worker.php`. It will sit waiting, because a worker speaks the protocol
over stdin/stdout rather than HTTP — point volucer at it:

```toml
[backends.app]
kind = "worker"
command = "php worker.php"
```

**Use `connect()`, not `create()`.** `Worker::connect()` is `Worker::create()` with one extra
step: when the server has set `VOLUCER_WORKER_SOCKET` — which it does on the `socket`
transport, **the default on Windows** — the protocol runs over that socket instead of stdio.

This is not cosmetic on Windows. A worker that calls `create()` cannot read a socket, so
under the default transport it reads `php://stdin` — which the server has set to
end-of-file — and exits during its handshake. The server reports that at startup, naming
`connect()` as the fix, so it fails fast and loudly rather than hanging. With no address in
the environment `connect()` is exactly `create()`, so it is backward compatible either way.

| `VOLUCER_WORKER_SOCKET` value | Transport |
|---|---|
| `127.0.0.1:34567` | loopback TCP — the ephemeral port the server bound |
| `unix:/run/volucer/worker.0.sock` | a unix socket, from `socket = "unix:/run/volucer/worker"` |

## The SDK is a transport, not a framework

There is no router, no container, no middleware pipeline, and no PSR-7 dependency — because
forcing one would mean this package could not be used from the many applications that use a
different one.

`Request` and `Response` are plain value objects; adapting them to whatever your application
already speaks is a few lines.

## Writing a handler

The handler receives the `Request` and returns one of:

| Return | Result |
|---|---|
| `Response` | sent exactly as given |
| `string` | sent as a `200` `text/html` body |
| `null` | the captured output becomes the body |

If the handler throws, the SDK logs it to stderr, answers `500`, and **keeps serving**. One
bad request does not take the worker down.

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

Both are enforced by the SDK, not merely documented. They are the two things that go wrong
in every persistent-PHP protocol.

### 1. stdout is the protocol channel

Every frame goes to `php://stdout`. Anything the *application* writes there would land
inside a frame and desynchronise the stream permanently.

So application output is not banned — it is **captured**:

- `ob_start()` runs around each request, so `echo` and template output are collected rather
  than written.
- `ini_set('display_errors', 'stderr')` sends warnings and notices to stderr, which is not
  part of the protocol.

The captured output becomes the response body when your handler returns `null`. That means **a
plain PHP script works as a worker without being changed** — if it echoes, that echo is the
response. If your handler returns a `Response`, the captured output is discarded.

To talk to the server *outside* a request, use `emitLog()` and `emitMetric()`. Those are the
reason this protocol exists instead of using FastCGI: a worker can speak first.

The residual risk is a write made directly to the `STDOUT` constant
(`fwrite(STDOUT, …)`, `file_put_contents('php://stdout', …)`), which no output buffer can
intercept. Nothing in PHP or any framework produces output that way, and if it happens the
consequence is a checksum mismatch the server reports clearly rather than silent corruption.

### 2. Shutdown by construction

When the server wants the worker to stop it closes the child's stdin. The loop reads that as
a clean EOF and returns from `run()`, so the worker exits on its own. There is **no signal to
catch** and no flag to poll — which is why the server never has to ask twice before killing.

## `Request`

| Member | Notes |
|---|---|
| `method`, `uri`, `protocol` | as sent |
| `headers` | `name => value`; `header($name)` looks up case-insensitively |
| `body` | the raw request body |
| `contentType()` | shorthand for the `Content-Type` header |
| `query()` | the query string parsed the way PHP populates `$_GET` |
| `cookies()` | the `Cookie` header parsed the way PHP populates `$_COOKIE` |
| `serverVars()` | CGI-style variables derived from the request (for legacy code) |
| `remoteAddr`, `scriptFilename`, `https` | as sent |

## `Response`

```php
Response::text('plain');                       // 200, text/plain
Response::html('<h1>hi</h1>');                 // 200, text/html
Response::json(['ok' => true]);                // 200, application/json
new Response(404, [['Content-Type', 'text/plain']], 'not found');

$response->withStatus(201)->withHeader('X-Trace', 'abc')->withBody('…');
```

`withHeader()` **appends** rather than replaces, so `Set-Cookie` can be sent more than once.

## `Worker`

| Method | Purpose |
|---|---|
| `Worker::create($in?, $out?)` | Bind explicit streams (default: stdio), force `display_errors=stderr` |
| `Worker::connect()` | `create()`, but honour `VOLUCER_WORKER_SOCKET` when the server sets it |
| `run($handler)` | Handshake, then serve until EOF |
| `emitLog($message, $level)` | One log record, unsolicited |
| `emitMetric($name, $value, $unit)` | One metric sample, unsolicited |
| `recycle()` | Ask the server to replace this worker after the request |
| `currentRequestId()` | The in-flight request id, for telemetry correlation |

`emitMetric()`'s `$name` must be a Prometheus name (`[a-zA-Z_][a-zA-Z0-9_]*`) and declared in
the server's `[metrics.collect]`.

`recycle()` is what an application calls when it has put itself into a state it cannot clean
up — a leaked global, a stuck connection, a fat static cache. The server sees `keep_alive =
false` and boots a replacement; the request that asked is still answered normally, so a
handler never has to choose between responding and being cleaned up.

## Sessions

A worker is not a SAPI, so the session half of what PHP normally does for an application is
missing: nothing reads the `Cookie` header, and `header()`/`setcookie()` cannot deliver the
`Set-Cookie` back. `Volucer\Session` does both, and every framework bridge should use it
rather than reimplement it.

| Method | Purpose |
|---|---|
| `begin($request)` | Adopt the session the request presented, and relax PHP's cookie handling |
| `beginWithCookies($cookies)` | The same, for a request that is not a `Volucer\Request` |
| `applyTo($response)` | **Add the `Set-Cookie` and end the request** — the normal call |
| `cookie()` | The `Set-Cookie` value alone, for a response object this SDK does not own |
| `end()` | End the session and forget the id, if you did not use `applyTo()` |

```php
$session = new Session();

$handler = static function (Request $request, Worker $worker) use ($session): Response {
    $session->begin($request);

    session_start();              // the application's own choice, as under FPM
    $_SESSION['seen'] = true;

    return $session->applyTo(Response::text('ok'));
};
```

**`applyTo()` ends the request as well, deliberately.** `session_id()` outlives the request,
so a client arriving with no cookie would otherwise inherit the previous client's session —
and reading the id *after* ending the session yields `''`, which means a caller that gets the
order wrong drops the cookie **silently**: the session is created on every request, and
nothing logs a word. Fusing the two removes the opportunity. Calling `end()` as well is
harmless.

A session is not started for you. A request that never calls `session_start()` — a 404, a
static file — creates no session and gets no cookie, exactly as under FPM.

## Clearing your own request scope

This is the part of worker mode that is yours to get right, and it is the whole reason to
choose worker mode over FastCGI mode: **the application keeps its state, so the application
clears it.**

What you must reset between requests depends on your framework. The general list:

- Anything you put in a global, a static property or a singleton.
- The database connection, if you opened one at boot. A connection that is inherited by the
  next request is a connection two requests share.
- Anything you cached that was true only for the request you served.

What you must **not** reset is the expensive part: the loaded classes, the compiled
templates, the warmed caches. That is the point.

## Memory

Each `END` frame reports `peak_memory_bytes` for **that request**, not for the process. The
worker reads `memory_get_peak_usage(true)` and then calls `memory_reset_peak_usage()`, so a
spike in one request does not make the server recycle the worker after every later one.

That reset exists on PHP 8.2+, which is the SDK's floor.

## Attaching instead of spawning

The `worker` backend starts your process. The `attach` backend does not: it *dials* a relay
that a supervisor in your own process listens on, and holds one connection per slot.

That is the shape that lets a supervisor fork its workers after booting the application
once, so every child inherits the interpreter, the loaded classes and the OPcache segment
instead of mapping them again.

**Volucer cannot own the fork.** It has to happen in the application's process, because a PHP
process cannot hand a file descriptor to another one — `ext/sockets` has no `SCM_RIGHTS`, so
the side that owns the listening socket has to be the side that owns the fork. The fork
therefore lives in `Volucer\Supervisor`, and you call it:

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

The handler is the same callable `Worker::run()` takes, so a worker file usually becomes a
supervisor by replacing its last line and nothing else.

### The relay address

With the first argument `null` the supervisor reads `VOLUCER_RELAY_SOCKET`, and so does the
server when `relay` is omitted:

```ini
# the unit file, or a container's environment
Environment=VOLUCER_RELAY_SOCKET=unix:/run/app/workers.sock
```

That is the counterpart of `VOLUCER_WORKER_SOCKET`. This one is set by the operator and both
sides read it. A value passed to `serve()`, or a `relay` key in the file, still wins — so a
stale variable in a shell cannot move a deployment.

### `children` needs no tuning

The third argument caps how many children may run at once, and the default (`0`) is *no cap*,
which is what an `attach` deployment wants: volucer dials `workers` times and holds one
connection per slot, so the live children are already bounded by that number.

### Boot before the fork, connect after it

A container, loaded classes and a warm template cache are inherited by every child, which is
the saving.

An *open database connection* must not be: every child would inherit the socket and they
would take turns reading from it. Open connections lazily, per child, on the first request —
Doctrine's `EntityManager` and PDO both do this already.

### Unix only

Windows has no `pcntl`, and `serve()` raises there rather than silently running a single
process. Use `kind = "worker"` or `kind = "upstream"` on Windows.

## Writing a supervisor yourself

`Supervisor::serve()` is a convenience, not the contract. What `attach` needs from any
supervisor is that it serves a connection **it** opened, which is `Worker::create()`:

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

Two details are load-bearing and easy to miss. `Worker::create()`, not `connect()` — the
connection already exists and is handed over. And the accept has to happen in the master,
before the fork: the kernel wakes one waiter per connection and does it unfairly, so N
children sharing one listener can leave one holding two connections and another none, which
for volucer is a slot whose connection nobody is reading.

## Consuming jobs

The same worker file can consume a queue instead of serving HTTP:

```php
Worker::connect()->consume(function (Volucer\Job $job, Worker $worker) {
    $worker->emitLog("processing {$job->id}");

    return true;          // ack
});
```

The handler's **return value** is the outcome:

| Return | Outcome |
|---|---|
| anything else, including `true` | `ack` — the job is done |
| `false` | `requeue` — run it again |
| a string | `reject` — the string is the reason |

If the handler throws, the job is **requeued, not acknowledged**: a consumer that crashed
must not lose the job. The job's lease then expires, so the loop is bounded rather than
tight.

See [Services](08-SERVICES.md) for the queue API.

## An example to copy

The SDK ships a runnable worker at `example/worker.php`, with `/`, `/health`, `/echo`, `/big`,
`/boom` and `/recycle` routes — including a route that throws, to show that a failing request
does not kill the process, and one that asks to be recycled.
