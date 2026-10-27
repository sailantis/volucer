# Services

This guide describes the key-value storage, lock, queue, and gRPC services available to
Volucer workers.

## Enabling services

A worker can reach a service only if its backend names it. The default deployment's
workers can reach nothing:

```toml
[backends.app]
kind = "worker"
command = "php worker.php"
services = ["kv.cache", "lock", "jobs"]     # the grant
```

Workers can call only the services granted to their backend. An unlisted service or an
unknown service name is rejected. This explicit allowlist limits what a worker can access
through the worker channel.

### The names

| Name | What it grants |
|---|---|
| `kv.<storage>` | One named key-value storage, e.g. `kv.cache` |
| `lock` | The lock backend |
| `jobs` | Every queue pipeline |
| `config` | The deployment's own description, redacted |
| `workers` | The worker list and the reset call |

Key-value access is granted per storage, allowing a deployment to use separate stores for
different purposes. The lock backend is a single service; individual locks are identified
by their resource name, so there is only one `lock` grant.

The same list applies to every backend kind. A `worker` and an `attach` reach these services
over their channel; an `external`, `spawn_cgi` or `upstream` backend reaches them through
`[rpc]`, if one is configured. A backend whose list is empty is granted nothing, and a
backend that grants nothing is handed no address to dial.

**`config` and `workers` name machinery, not storage, and a deployment without worker pools
answers them anyway.** `services = ["workers"]` on a CGI backend is accepted by `check`, and
`workers.list` then *succeeds*, returning `[]` — the call reads the process's pool directory,
which exists whether or not it holds a pool. Only `workers.reset` can never work there, because
no index exists to reset. The same is true of `config`, which describes the deployment whatever
its backends are. So the grant is not an error; it is a grant of an answer nobody wants, and
`[status.workers]` is the surface to reach for when the goal is to *see* a pool rather than to
give an application a call plane.

### Read, and read-write

A namespace is one capability, so `services` grants every method in it. `workers` is the
pair to know: `workers.list` is an answer, and `workers.reset` kills worker *N* and waits for
the replacement to finish its handshake. Both arrive together, which is why an application
that only wants to display a pool should not be granted `workers` at all — and why
`[status.workers] allow = ["workers.list"]` exists as the narrower way to ask.

The same split runs through the others. `kv` reads with `get`, `has`, `ttl` and `getMultiple`,
and writes with `set`, `setIfAbsent`, `delete`, `clear` and `setMultiple` — `clear` wiping the
storage. `lock` answers with `exists` and `holders`, and mutates with `acquire`, `release`,
`release_all` and `steal`. `jobs` reads with `size` and `pipelines`, and writes with `push` and
`push_batch`.

## The call plane

Workers call these services over the worker channel using `CALL` and `RETURN` frames. A
worker can make a call while handling a request, and the server sends the result over the
same channel. Unlike FastCGI, this protocol allows a worker to ask the server for services
such as storage, locks, and queues during request handling.

### An application that is not a worker

A FastCGI application has no such channel. It exists for the duration of one request, the
server drives it, and it can only answer the frame it was handed — so there is nowhere on
that protocol for it to ask a question. To give one access to the same services, add an
`[rpc]` listener. The application connects to this listener, and the listener serves the
same namespaces over the same stores:

```toml
[backends.app]
kind     = "external"
socket   = "unix:/run/php/php8.3-fpm.sock"
services = ["kv.cache", "lock"]        # the same grant a worker would name

[kv.cache]
driver = "memory"

[rpc]
listen = "unix:/run/volucer/rpc.sock"
```

Volucer writes two CGI parameters into every request that backend serves:

| Parameter | Value |
|---|---|
| `VOLUCER_RPC` | The address from `[rpc].listen` |
| `VOLUCER_RPC_TOKEN` | The token that identifies this backend |

A backend whose `services` is empty is given neither. It is then not told where the socket
is, so it cannot even make an attempt that would be refused. This is why `[rpc]` and a
grant have to be configured together: a listener with no grant would refuse every call, and
a grant with no listener would give the application nothing to connect to. Volucer rejects
either configuration at startup.

The wire format is the worker protocol: same frames, same message types, same checksum.
The difference is that there is no handshake. Instead, every `CALL` carries the token,
because a CGI process would pay the cost of a handshake on every request. This also means
a token sent over an existing connection is checked on the next call.

```php
// The whole client. It reads VOLUCER_RPC and VOLUCER_RPC_TOKEN from the request it was handed.
$rpc = Volucer\Rpc::connect();

$rpc->kv('cache')->set('greeting', 'hello');
$rpc->lock()->acquire('pdf:create');
```

The frames are the worker protocol's, so a client written against the SDK's `Frame`/`Header` works
too — but there is no reason to write one. `Rpc` connects lazily (a request that never touches a
store pays nothing), reconnects after a broken connection, and puts the token on every call. It
enforces the deadline itself, because the server's `call_timeout_seconds` governs only how long it
will spend *writing* an answer.

`[rpc].listen` may be a `unix:` path or a loopback `host:port`. A wildcard bind is rejected:
every call on this socket mutates something, and the token identifies a backend within one
host; it is not a defence against another host.

| Key | Default | Meaning |
|---|---|---|
| `listen` | required | `unix:/path` or `127.0.0.1:6001` |
| `max_connections` | uncapped | Refuse a connection beyond this many. `0` is rejected |
| `idle_timeout_seconds` | `300` | Close a connection this long after its last call |
| `call_timeout_seconds` | `10` | How long one call may take before the connection is closed |

An `upstream` backend that grants services is handled the same way, except that it receives
the same two values as the request headers `X-Volucer-Rpc` and `X-Volucer-Rpc-Token`.

**`Volucer\Rpc` is that client, in the PHP SDK.** `Kv`, `Lock` and `Jobs` are written against a
three-method `CallPlane` interface — `call()`, `lastCallError()`, `lastCallWasUnknown()` — which both
`Worker` and `Rpc` implement, so the storage code is the same over either transport and only the type
that built it differs:

```php
$kv = $worker->kv('cache');     // inside a worker
$kv = $rpc->kv('cache');        // inside a FastCGI request
```

Two things it does that a worker's channel does not need, and both are worth knowing before relying
on one. A call has a **deadline** (`Rpc::connect(timeout: …)`, default 10 s, which has to cover a
`lock()->acquire()` that waits), and `lastCallWasUnknown()` is reachable on a path a worker never has
— a connection that broke after the call was written may still have been served.

## Key-value storage

```php
$kv = $worker->kv();                 // the "default" storage
$kv = $worker->kv('cache');          // a named one

$kv->set('greeting', 'hello');
$kv->set('temp', 'value', 60);       // with a TTL, in seconds
$found = null;
$value = $kv->get('greeting', $found);
```

| Method | Purpose |
|---|---|
| `get($key, &$found)` | The value, or `null`. `$found` says whether it was there |
| `set($key, $value, $ttl = null)` | Write a value, optionally with a TTL |
| `setIfAbsent($key, $value, $ttl = null)` | Write **only if the key does not exist** |
| `has($key)` | Whether the key exists |
| `delete($key)` | Remove a key |
| `ttl($key)` | The remaining TTL |
| `clear()` | Empty the storage |
| `getMultiple($keys, &$missing)` | Several at once |
| `setMultiple($values, $ttl = null)` | Several at once |
| `getMany($keys)` | Several at once, missing keys omitted |

### ### Using `setIfAbsent` for atomic writes

Use `Kv::setIfAbsent()` when a value must be created only if the key does not already
exist. The operation is atomic for each driver: for example, Redis uses `SET NX`, the file
driver uses `O_CREAT|O_EXCL`, and the in-memory driver uses a shard lock. This is a
conditional write on the storage itself, unlike a separate lock around application logic.

### Serializers

Values are JSON by default. To store something else:

```php
$kv->serializer(
    fn (MyValue $v): string => $v->toJson(),
    fn (string $raw): MyValue => MyValue::fromJson($raw),
);
```

## Locks

```php
$lock = $worker->lock();

$id = $lock->acquire('invoice:42', ttl: 30, wait: 5);
if ($id === null) {
    return Response::json(['error' => 'busy'], 409);
}

try {
    // ... the work ...
} finally {
    $lock->release('invoice:42', $id);
}
```

| Method | Purpose |
|---|---|
| `acquire($resource, $ttl, $wait, $id)` | Take the lock, optionally waiting for it |
| `acquireShared($resource, $ttl, $wait, $id)` | The shared variant |
| `tryAcquire($resource, $ttl, $id)` | Take it or return `null` |
| `tryAcquireShared($resource, $ttl, $id)` | The shared variant |
| `release($resource, $id)` | Release it |
| `forceRelease($resource)` | Release it whoever holds it |
| `exists($resource)` | Whether it is held |
| `holders($resource)` | Who holds it |
| `updateTtl($resource, $id, $ttl)` | Extend the lease |

`acquire()` returns a lock ID to pass to `release()`, or `null` if the lock could not be
acquired. TTL values are in seconds; `ttl = 0` means the lock does not expire.

### Locks are not a correctness primitive

TTL-based locks do not provide fencing tokens. If a worker stalls until its lease expires,
another worker may acquire the lock while the first worker is still running. Use locks to
reduce duplicate work, but do not rely on them alone to protect critical invariants. For an
atomic conditional write, use `Kv::setIfAbsent()`.

## Queues

```php
$queue = $worker->queue('emails');

$id = $queue->push('send-welcome', json_encode(['user' => 42]));
$queue->pushBatch([
    ['task' => 'send-welcome', 'payload' => '...'],
    ['task' => 'send-receipt',  'payload' => '...'],
]);

$queue->size();       // the pipeline sizes
$queue->pipelines();  // every pipeline the server has
```

| Method | Purpose |
|---|---|
| `push($task, $payload, $headers)` | Enqueue a job. Returns its id |
| `pushBatch($jobs)` | Enqueue several |
| `size()` | The size of each pipeline |
| `pipelines()` | The names of every pipeline |

`$task` identifies the work to perform, `$payload` contains the job data, and `$headers`
provides per-job metadata.

### Consuming

A consumer is a worker that receives jobs instead of HTTP requests. The same worker
application can be configured for either type of work:

```php
Worker::connect()->consume(function (Volucer\Job $job, Worker $worker) {
    return true;          // ack
});
```

| Return value | Outcome |
|---|---|
| Anything except `false` or a string (including `true`) | `ack` — the job is complete |
| `false` | `requeue` — make the job available again |
| A string | `reject` — use the string as the reason |

The incoming frame identifies whether the worker is handling a request or a job. Request
workers reset request state between requests; consumers do not automatically reset
application state between jobs. Ensure a consumer cleans up any job-specific state before
processing the next job.

### Configuring the server side

```toml
[jobs]
consume = ["emails"]        # the pipelines this server consumes from

[jobs.pipelines.emails]
driver = "memory"           # or "redis"

[jobs.pool]
command = "php consumer.php"
workers = 4
max_jobs = 100              # exit after this many jobs
job_lease_seconds = 300     # how long a reserved job is held
prefetch = 1                # how many to reserve in advance
```

Configure `[jobs.pool]` when `consume` is non-empty. `job_lease_seconds` controls how long
a reserved job remains leased; it is separate from the HTTP request timeout. Choose a lease
long enough for the job to complete, while ensuring abandoned jobs become available again
in a reasonable time if a consumer exits unexpectedly.

## gRPC

A worker backend can serve gRPC. List the services it may serve in the backend configuration:

```toml
[backends.app]
kind = "worker"
command = "php worker.php"
grpc_services = ["greeter.Greeter"]
```

A call for a service not listed in `grpc_services` receives `UNIMPLEMENTED` before Volucer
reads the request body or assigns a worker.

After `SIGHUP`, changes to `grpc_services` apply to the next request, including requests on
existing connections. The service list and site configuration are loaded as one generation,
so their changes take effect together.

### Serving any service instead of naming them

To deliver every well-formed gRPC method path to the worker, leave `grpc_services` unset
and set `grpc_serve_any`:

```toml
[backends.app]
kind = "worker"
command = "php worker.php"
grpc_serve_any = true
```

With this option, Volucer sends each well-formed `/pkg.Service/Method` path to the
application. The application is responsible for returning `UNIMPLEMENTED` for methods it
does not implement. To derive an explicit service list from `.proto` files, configure
`grpc_proto` instead.

Configuration rules, checked when the file is read:

- `grpc_serve_any` cannot be combined with a non-empty `grpc_services`.
- The request path must still match `/pkg.Service/Method`.

Volucer logs a warning at startup for each backend using `grpc_serve_any`.

`grpc_services` is also available on an `attach` backend, with the same rules.

### Reading the services from `.proto` files

Use `grpc_proto` to derive the service list from `.proto` files:

```toml
[backends.app]
kind = "worker"
command = "php worker.php"
grpc_proto = ["proto/greeter.proto"]
```

Each entry can be a file or directory. Volucer searches directories recursively for
`*.proto` files:

```toml
grpc_proto = ["proto/"]     # everything under proto/, at any depth
```

Paths are resolved relative to the directory containing `volucer.toml`.

The parser reads file-scope `package` declarations and top-level `service` declarations. It
does not follow `import` statements, or treat text in comments, strings, or message bodies
as declarations. A service in a file without a package uses its bare name. A file with an
invalid `package` declaration contributes no services.

`grpc_proto`, `grpc_services`, and `grpc_serve_any = true` are mutually exclusive. Combining
them causes a configuration error.

Volucer rejects the configuration if:

- an entry cannot be read or is neither a file nor a directory;
- the entries resolve to no services.

Volucer reads these files when configuration is loaded by `serve`, `check`, or `SIGHUP`.
Saving a `.proto` file does not apply changes until configuration is reloaded. If a reload
resolves no services, Volucer rejects it and keeps the previous configuration active.

`volucer check` displays the entries and the services they resolve to:

```
    app: worker, php worker.php
      grpc:              greeter.Greeter, greeter.Admin
      grpc from:         2 service(s) from grpc_proto: proto/
```

The output reports the number of services resolved from each entry. Compare it with the
services implemented by the application to verify the configured list.

### Handling a call

```php
Worker::connect()->onGrpc(function (Volucer\Request $request, Worker $worker): Volucer\GrpcResponse {
    // The path is /pkg.Service/Method
    if ($request->uri !== '/greeter.Greeter/SayHello') {
        return GrpcResponse::fail(12, 'UNIMPLEMENTED');
    }

    return GrpcResponse::message($payload);
});
```

A worker dedicated to gRPC can use `grpc()` instead. It registers the handler and serves
without HTTP routes:

```php
Worker::connect()->grpc(function (Volucer\Request $request, Worker $worker): Volucer\GrpcResponse {
    return GrpcResponse::message($payload);
});
```

A worker that handles both HTTP and gRPC registers the two handlers separately.

| Method | Purpose |
|---|---|
| `GrpcResponse::message($payload, $contentType)` | A successful response |
| `GrpcResponse::ok($contentType)` | A successful response with no body |
| `GrpcResponse::fail($status, $reason)` | A failure, with a gRPC status code |
| `GrpcResponse::isOk()` | Whether it succeeded |
| `GrpcResponse::statusName()` | The status, by name |

`fail()` accepts gRPC status codes. For example, `12` is `UNIMPLEMENTED`, `13` is
`INTERNAL`, and `14` is `UNAVAILABLE`.

The handler may also return a string or `null`. In either case, captured output becomes the
message body and the response status indicates success.

### The server never parses a message

Volucer routes gRPC requests using `:path` and forwards the framed message bytes. The
application owns the schema; `.proto` files are not included in the server build or
dependency graph. Schema changes therefore require an application deployment, not a server
build.

Return a `GrpcResponse` to specify the gRPC status and message. The worker protocol carries
these values in response headers, which Volucer moves into the gRPC trailer section.

Volucer sets `Content-Type: application/grpc` on the response. gRPC clients require this
header to recognize the response.

## Serving gRPC from an origin

A gRPC origin that already implements the protocol can be configured as an `upstream`
backend. See
[Backends](02-BACKENDS.md#grpc).

```toml
[backends.greeter]
kind = "upstream"
origin = "unix:/run/greeter.sock"
protocol = "h2c"
```

In this arrangement, Volucer proxies requests to an existing gRPC origin. With the `worker`
backend, the application implements the gRPC service directly and no separate origin is
required.
