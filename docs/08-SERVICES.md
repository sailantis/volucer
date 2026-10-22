# Services

**Key-value storage, locks, queues and gRPC, from inside a running request.**

## Enabling services

A worker can reach a service only if its backend names it. The default deployment's
workers can reach nothing:

```toml
[backends.app]
kind = "worker"
command = "php worker.php"
services = ["kv.cache", "lock", "jobs"]     # the grant
```

A service the backend does not name is refused on purpose. The alternative — a worker
reaching anything it asks for — turns the worker channel into a remote-execution surface,
and the only thing protecting it is the fact that the list is explicit.

A name the server does not know is **refused by name**, the same way a misspelled key is.

### The names

| Name | What it grants |
|---|---|
| `kv.<storage>` | One named key-value storage, e.g. `kv.cache` |
| `lock` | The lock backend |
| `jobs` | Every queue pipeline |
| `config` | The deployment's own description, redacted |
| `workers` | The worker list and the reset call |

**KV is named per storage; the others are not.** A KV storage is *named* because one
deployment wants several with different lifetimes and backends — a session cache and a
fragment cache are different things — and a worker has to say which one it is calling.

The lock backend is a single mechanism: a lock is named by its *resource*, so two lock
tables would be two independent views of one namespace. Two processes locked on
`"pdf:create"` through two tables would not see each other, which is a bug that is
invisible in configuration and appears in production. So there is exactly one `lock` name
to write.

## The call plane

These services are reached over the worker channel — a `CALL`/`RETURN` pair, which is a
frame the worker writes *unprompted*, mid-request, and the server answers.

That is the difference from FastCGI, and it is why this protocol exists: in FastCGI the
worker can only answer the request it was handed. A worker that needs a lock, a cache or
a queue needs to ask the server for something, and it needs to do that while a request is
in flight.

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

### `setIfAbsent` is the one to reach for

**Where a real invariant is at stake, use `Kv::setIfAbsent`** — the storage's own
conditional write, atomic per driver (`SET NX` on Redis, `O_CREAT|O_EXCL` on `file`, a
shard lock in memory).

That is the difference between a lock and an invariant. A lock is a hint that something
else might be happening; a conditional write is the thing itself.

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

`acquire()` returns the **lock id** you pass to `release()`, or `null` when it could not be
taken. `ttl` is in seconds; `ttl = 0` means no expiry.

### Locks are not a correctness primitive

This is worth stating plainly, because the API is shaped so that an application can be
correct about it rather than only warned.

**A TTL lock has no fencing token.** A worker that stalls past its lease can be writing
while somebody else holds it. The lock prevents *casual* overlap; it does not prevent a
paused process from carrying on.

Use a lock to avoid duplicated work. Where a real invariant is at stake, use
`Kv::setIfAbsent` — the storage's own conditional write, atomic per driver.

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

`$task` names what to do; `$payload` is the bytes; `$headers` is a per-job header map.

### Consuming

A consumer is a worker that receives jobs rather than HTTP requests. The same worker file
can do either:

```php
Worker::connect()->consume(function (Volucer\Job $job, Worker $worker) {
    return true;          // ack
});
```

| Return | Outcome |
|---|---|
| anything else, including `true` | `ack` — the job is done |
| `false` | `requeue` — run it again |
| a string | `reject` — the string is the reason |

A consumer is told apart from a request worker by the frame the work arrives in, because
the difference an application has to know is whether its own state is reset between units
of work: **a request worker resets, a consumer does not.**

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

`pool` is required when `consume` is not empty. `job_lease_seconds` is deliberately not
the request timeout, because the work is deliberately different work: five minutes is long
enough for anything an HTTP request would not have been allowed to do, and short enough
that a consumer which died does not leave work stuck for an hour.

## gRPC

A worker backend can serve gRPC. The service must be named on the backend:

```toml
[backends.app]
kind = "worker"
command = "php worker.php"
grpc_services = ["greeter.Greeter"]
```

A call naming a service that is not listed is answered `UNIMPLEMENTED` without reaching a
worker at all.

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

A worker that serves **only** gRPC can use `grpc()` instead, which registers the handler
and serves it with no HTTP routes:

```php
Worker::connect()->grpc(function (Volucer\Request $request, Worker $worker): Volucer\GrpcResponse {
    return GrpcResponse::message($payload);
});
```

A worker that serves both calls the two halves separately.

| Method | Purpose |
|---|---|
| `GrpcResponse::message($payload, $contentType)` | A successful response |
| `GrpcResponse::ok($contentType)` | A successful response with no body |
| `GrpcResponse::fail($status, $reason)` | A failure, with a gRPC status code |
| `GrpcResponse::isOk()` | Whether it succeeded |
| `GrpcResponse::statusName()` | The status, by name |

`fail()` uses the gRPC status codes: `12` is `UNIMPLEMENTED`, `13` is `INTERNAL`, `14` is
`UNAVAILABLE`.

The handler may also return a plain `string` or `null`, in which case the captured output
is the message body and the status is success.

### The server never parses a message

Volucer routes on the `:path` and carries the framed bytes. **The application owns the
schema** — `.proto` files never enter the server's build or its dependency graph, so a
schema change is an application deploy rather than a server one.

The one thing a handler has to state is its outcome, as a `GrpcResponse`. The status and
the message become two response headers, and the server moves them into the trailer
section — the worker protocol's response head has nowhere else to put them.

`Content-Type: application/grpc` is set for you. A gRPC client refuses a reply that does
not carry it before it reads a byte of the body, so a handler that forgot would see its
message rejected for a reason it could not discover from the answer.

## Serving gRPC from an origin

A gRPC origin that already speaks the protocol is reached as an `upstream` backend. See
[Backends](02-BACKENDS.md#grpc).

```toml
[backends.greeter]
kind = "upstream"
origin = "unix:/run/greeter.sock"
protocol = "h2c"
```

That is Tier A: the proxy relays to an origin that already speaks gRPC. The `worker`
backend is Tier B: the application serves the service itself, with no origin at all.
