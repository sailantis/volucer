# Plugins and additional runtimes

**What RoadRunner has, and how each piece would land in volucer.**

This document exists because the question "can volucer also do gRPC / KV / locks / jobs /
metrics, and can it run Django and Rails?" deserves an answer that is a design rather than a
shrug. It is a **plan, not a description of the code**: nothing in it is built unless a section
says so. `docs/plan.md` remains the record of what exists, and its §10 is the list to read for the
current state.

Read §1 if you want the survey, §2–§4 if you want the decisions that everything else depends on,
and §8 if you only want the order. §9 is the part that will be quoted back at us, because it says
which features are deliberately **not** the same as RoadRunner's.

---

## 1. What RoadRunner actually has

Vendored checkout: `../roadrunner` (engine v3, plugin line v6 — `roadrunner/go.mod` pins
`kv/v6`, `lock/v6`, `jobs/v6 v6.0.1`, `metrics/v6`, `grpc/v6`, `rpc/v6`, `otel/v6`).
`roadrunner/container/plugins.go` is the authoritative bundle list; each plugin is a **separate Go
module**, not vendored, so the config schema is the best local source for the real key surface:
`roadrunner/schemas/config/3.0.schema.json`, plus the annotated sample `roadrunner/.rr.yaml`.

RoadRunner is a single Go binary that loads ~40 plugins through the `endure` DI container. The
architecture that matters for us is this: **all control-plane traffic between PHP and the server
goes over one socket** — `rpc.listen` (default `tcp://127.0.0.1:6001`) — carrying **Goridge**
framing over Go's standard `net/rpc` (`roadrunner/internal/rpc/client.go:70` builds
`rpc.NewClientWithCodec(goridgeRpc.NewClientCodec(conn))`). KV, locks, jobs' producer side, the
informer (`rr workers`), the resetter (`rr reset`) and custom metrics are *all* just method calls
on that one socket. The CLI is itself a Goridge RPC client, which is why `rr workers` needs no
privileged access.

Two things are emphatically **not** on that socket: the gRPC plugin, which runs a real
gRPC/HTTP-2 server of its own (`grpc.listen`), and the metrics plugin, which exposes a real
Prometheus HTTP endpoint. Those are their own listeners.

### 1.1 Survey

| Plugin | What it is | Its transport | External dependency | Reimplementation |
| --- | --- | --- | --- | --- |
| `rpc` | The control-plane listener itself. Holds no domain logic; other plugins register method sets on it | Goridge over TCP/Unix | `goridge/v4` | **Low** — the real work is the framing codec |
| `metrics` | Embedded Prometheus registry: `/metrics` for scrapers, plus a `collect:` table PHP can push into over RPC | HTTP `/metrics` + Goridge RPC | `prometheus/client_golang` | **Low** — thin wrapper over a registry |
| `kv` | PSR-16 cache, proxied so PHP never talks to Redis itself | Goridge RPC | redis / memcached / bbolt clients | **Low** — 5 non-test Go files; the difficulty is the driver |
| `lock` | Exclusive and read (shared) distributed mutexes | Goridge RPC | go-redis v9 (Lua + Pub/Sub) | **Medium** — the Redis lock is the hard part |
| `jobs` | Priority queue with 7 broker drivers, consumer pools, ack/nack/requeue | Goridge RPC (producer + control) and the worker protocol (payload) | amqp091-go, aws-sdk-go-v2, franz-go, nats.go, … | **High** — 14 non-test files *plus* 7 driver modules |
| `grpc` | A real gRPC server that loads `.proto` at runtime and dispatches each call to a PHP worker | gRPC/HTTP-2 + the worker protocol | `google.golang.org/grpc`, `jhump/protoreflect/v2` | **Medium-high** — dynamic proto dispatch is the cost |
| `otel` | A shared tracer provider the other plugins pull spans from | none of its own | the OTel Go SDK | **Low-medium** plugin, pervasive wiring |

Also in the bundle and worth naming because they are adjacent rather than requested: `temporal`
(the largest thing in the tree), `centrifuge` (WebSockets/broadcast), `fileserver` + `static`,
`gzip` + `zstd`, `headers`, `send` (X-Sendfile), `status` (health/ready), `rate-limiter`,
`proxy_ip_parser`, `protoreg`, `informer`, `resetter`, `service` (a mini systemd for arbitrary
child processes), `app-logger`. `resetter` and `status` are effectively zero-config.

### 1.2 The config surface, verbatim

Worth having in one place, because "can volucer read my `.rr.yaml`?" is the first question anyone
asks. It cannot, and §3 says why — but these are the shapes that were studied.

```yaml
rpc:
  listen: tcp://127.0.0.1:6001        # or unix:///run/rr/rpc.sock + unix_socket.mode

kv:
  local-memory:  { driver: memory,  config: { } }
  fast-kv-fr:    { driver: redis,   config: { addrs: ["localhost:6379"], db: 0, … } }
  us-central-kv: { driver: memcached, config: { addr: ["localhost:11211"] } }
  boltdb-south:  { driver: boltdb,  config: { file: "rr.db", permissions: 0755, interval: 40 } }
  # driver ∈ {boltdb, memcached, memory, redis}; `memory` requires an empty config object

lock:                                  # zero-config by default (in-memory); ABSENT from the JSON schema
  driver: redis                        # ∈ {memory, redis}; redis needs Redis ≥ 7
  config: { addrs: ["localhost:6379"], read_timeout: 5s, tls: { … } }

metrics:
  address: "127.0.0.1:2112"            # /metrics is appended automatically
  prefix: "myapp_"                     # applied to rr_* plugin metrics only
  labels: { service: "my-microservice" }
  collect:                             # app metrics, pushed over RPC
    app_metric_histogram: { type: histogram, help: "…", labels: ["type"], buckets: [0.1, 0.2, 1.0] }

status:
  address: 127.0.0.1:2114              # GET /health?plugin=http, GET /ready
  unavailable_status_code: 503
  check_timeout: 60

jobs:
  num_pollers: 32                      # default: workers + 1
  timeout: 60
  pipeline_size: 100000                # the ONE internal priority queue, in memory only
  pool:
    command: "php my-super-app.php"    # consumers are a worker pool like any other
    num_workers: 0
    max_jobs: 0
    max_queue_size: 0
    dynamic_allocator: { max_workers: 25, spawn_rate: 10, idle_timeout: 10s }
  pipelines:
    test-local:
      driver: memory                   # ∈ {memory, amqp, sqs, kafka, beanstalk, nats, nsq, google_pub_sub, boltdb}
      config: { priority: 10, prefetch: 100000, … }

grpc:
  listen: "tcp://127.0.0.1:9001"       # a real HTTP/2 server, not a proxy
  proto: ["*.proto"]                   # globs allowed; used for routing AND reflection
  tls: { key: …, cert: …, root_ca: …, client_auth_type: no_client_certs }
  max_send_msg_size: 50  # MB
  max_recv_msg_size: 50
  max_concurrent_streams: 10
  ping_time: 1s
  timeout: 20s
  pool: { num_workers: 4 }             # the workers still speak RR's framed protocol

otel:
  resource: { service_name: "rr_test", service_version: "1.0.0" }
  exporter: otlp                       # v6 rejects zipkin; jaeger is gone
  client: grpc                         # ∈ {grpc, http} → :4317 or :4318
  endpoint: 127.0.0.1:4317
  insecure: true
  headers: { api-key: "secret" }
```

### 1.3 Semantics worth copying, and one worth refusing

Collected so that the design sections can refer to them rather than restating them.

**KV.** Per-item TTL; `getTtl` unsupported on memcached (a protocol limitation, not a bug);
pluggable serializers (PHP `serialize`, igbinary, sodium for E2E encryption). There is no
cross-key transaction beyond whatever the driver gives you, and no claim of one.

**Locks.** TTL per lock, `ttl = 0` meaning "held until released"; `UpdateTTL` **replaces** the
remaining TTL rather than extending it; a second read lock with the same owner id is refused
(dedup by id); `wait = 0` means exactly one attempt, bounded to 1 ms on memory and to
`read_timeout` on Redis; the Redis key prefix is fixed (`rr:lock:<resource>`); lock waiting
multiplexes over one shared Pub/Sub connection. **The important one:** a failed or timed-out Redis
command during acquisition returns an error whose lock state is *unknown*, so the caller must poll
`Exists`/`Release` to resolve it. And there are **no fencing tokens** — see §9.2.

**Jobs.** One in-memory priority queue shared by all pipelines (`pipeline_size`), which is *not*
persisted, so a crash loses everything reserved but not yet delivered regardless of how durable
the driver is. `autoAck` acks the instant the job leaves the broker, before the application sees
it. Named pools are selected by a reserved `pool` header, and an unknown pool name
**negatively-acks** even in single-pool mode. Requeue is a job-level call
(`fail($e, requeue: true)`); retry policy, backoff and dead-lettering are **application
convention, not server features** — RR has no built-in schedule. Trace context rides along as
job headers.

**Metrics.** Names are `rr_<plugin>_<metric>`, e.g. `rr_http_request_total`,
`rr_http_request_duration_seconds{status}`, `rr_grpc_request_total{grpc_method,status_code}`,
`rr_jobs_jobs_ok` / `_err` / `_requeue`, `rr_jobs_push_latency_bucket`, and a common pool set
(`{{plugin}}_total_workers`, `_worker_memory_bytes{pid}`, `_worker_state{pid,state}`,
`_workers_ready`, `_workers_working`, `_workers_invalid`) emitted by every plugin that owns a
pool. `prefix` and `labels` apply to plugin metrics only, never to `go_*`/`process_*` or `collect`
metrics, and a label name colliding with a reserved one (`pid`, `state`, `status`) **fails the
plugin at startup**. In v6 the jobs counters became counters rather than gauges, with the note that
failed and requeued outcomes no longer also increment `_ok`.

**gRPC.** Connection knobs are Go's `grpc.KeepaliveParams` / server parameters almost 1:1
(`max_connection_idle`, `max_connection_age`, `max_connection_age_grace`, `ping_time`, `timeout`,
`max_concurrent_streams`), which map cleanly onto the tuning hyper's h2 layer already accepts. The
service definitions are loaded **at runtime from `.proto`** and used for both routing and server
reflection — which is the single heaviest piece of the plugin, and §6.6 is about not having to copy
it: there, routing is a path string and the descriptors belong to an optional reflection service.

**RPC.** `rpc.Version` and `rpc.Config` exist; `rpc.Config` returns the server's configuration to
anyone who can reach the socket. §4.5 treats that as a hazard rather than a feature.

### 1.4 The rest of the bundle, sorted by what it would cost here

The survey above covers the six plugins that were asked for. This is the remainder of
`roadrunner/container/plugins.go` — the picture in the README names most of them — sifted once so
that "is there anything else worth taking?" has a written answer instead of a re-derivation. The
sorting rule is the one §5 states: **what does it cost here, given what is already built?**

#### Worth building — a feature a deployment would notice missing

| What | RR's shape | What it costs here | Why it is worth it |
| --- | --- | --- | --- |
| **`volucer workers` / `volucer reset`** (§4.6, §4.7) | `informer` + `resetter`, both RPC clients of `rpc.listen` | **S–M**, and not the **S** this was first called. The methods exist and are tested; what is missing is a way *in* from a second process, which is a listener rather than a client. Route it on the `[status]` listener that already exists — see §4.7 | §4.6 promises it and the promise was false in a way worth keeping written down: the channel the registry travels on is held by the pool and the child, so it has no second-process half. Without it, `worker.reset` is reachable only from application code — the operator's own lever is missing. **It also makes two subcommands optional**, which is why it is the endpoints rather than the CLI: `curl -f -XPOST .../workers/3/reset` is the whole command |
| **`http.trusted_subnets` + forwarded headers** — **built, §6.8** | `proxy_ip_parser` | **S–M**, and it was S–M. A CIDR check against the peer, then the first parsable of an ordered header list, hand-rolled rather than a crate | **Not a convenience — a correctness bug, and the only defect in this table.** A client behind nginx arrives with the proxy's address, so a rate limit keyed on `ip` limits nginx, and the access log records a peer that is not the client. Both places already took an address, which is why the change is small |
| **`metrics.observe`** — **built, §6.1** | `metrics.collect` | **S**, and it was **not** S. The estimate said "one registry entry and one namespace arm"; the real work was a declared-kind dispatch (`Registry::record`), a validation that runs *before* a series exists, a name rule, and fixing the SDK's own examples, which used dots and could never have rendered | `emitMetric` rode the wire and landed in a `Vec` nobody read (`codec.rs:1136`), which §6.1 called the first thing to fix. **That was fixed and the feature still did not work**: a registry refuses an undeclared name, so every real sample was dropped while three tests passed by declaring their own metric. The table was the feature, not an upgrade to it |
| **Rate limiting** — **built, §6.10** | `rate-limiter`, `http.middleware` | **S**, and it depends on the row above — which is the half the estimate got right. The bucket *is* a page of arithmetic; what was not S was deciding what a refusal means on **two** front-ends and **two** protocols, and finding that RoadRunner answers a full table with a 503 | A reverse proxy in front of an unauthenticated endpoint has no other lever; the token bucket is a page of arithmetic. It is the one middleware RR ships that is not a substitute for something nginx does better |

#### Worth considering — real value, but it duplicates or complicates something that exists

| What | RR's shape | What it costs here | The argument against, and what would beat it |
| --- | --- | --- | --- |
| **`file` KV driver** | `boltdb` | **S–M**: an embedded store, or a directory of files | Already §6.2's next driver, so it is here only to be counted. Note what a filesystem buys that boltdb does not: a cache survives a restart **without a dependency**, which is the whole reason it beats `memcached` in this order |
| **`server.access_log` rotation** | `app-logger` is request logs *out of the application*; RR does not rotate either | **S** if it is "reopen on `SIGHUP`"; **M+** if it is rotation by age or size | A log an unprivileged server cannot reopen survives exactly one `logrotate` and then writes to a deleted inode for ever, silently. **The signal is already wired** — `SIGHUP` reloads, so "reopen the log first" is a few lines rather than a subsystem. Size-based rotation is where it becomes a subsystem, and `logrotate` already exists |
| **`max_request_size`** | `http.max_request_size` | **S, and the estimate was wrong again** — built as `server.max_body_bytes`; the FastCGI front-end had no bound at all, so the "one `HttpLimits` field" reading was half the work | The limit existed and only its configurability was missing — but only on one protocol. Behind nginx the only bound was `client_max_body_size` on somebody else's configuration, while `read_next` extended a `Vec` for as long as `STDIN` records arrived. See §6.9 |

#### Not worth building, and the reason is a property of something already here

- **`centrifuge` (WebSockets/broadcast)** — **L**, and it is a second product rather than a plugin:
  rooms, a hub, a presence protocol, and a client SDK. §7.3's rule about dead bridges applies to
  scope too. More decisively, WebSockets are already refused here for one specific reason — the
  hop-by-hop filter drops `Connection` and `Upgrade`, so a handshake does not survive (README,
  *Backends*) — which means this is *two* projects rather than one: the upgrade path, then the hub
  on top of it.
- **`tcp`** — gone from RR v3, and structurally impossible here for a reason RR states itself: PHP
  workers are stateless between requests, so a TCP session has no home. The same objection covers
  the SMTP-server example in the picture.
- **`memory`/`boltdb` jobs drivers, and the other five brokers** — the `Driver` trait makes each one
  a module, but §9.4's argument stands: seven brokers is not a feature. `redis` and `memory` cover
  the single-node and the shared case; anything else should be *your* broker's client on the HTTP
  side, or a driver when somebody needs it.
- **`gzip`/`zstd`/`headers`/`send`/`static`/`fileserver`** — every one is something nginx already
  does, and `precompressed` covers the case where compression has to be *free*. §9 is where that
  sentence belongs; the exception is `send` (X-Sendfile), which is genuinely useful only when the
  file is not reachable from the front-end and the proxy has a faster path to it — a condition that
  does not hold here.
- **`service` (a mini systemd) and `temporal`** — `service` supervises arbitrary children, which
  §7.1's `command` already does for an origin and php-fpm does for anything else. `temporal` is a
  workflow engine: a separate product with its own protocol, not a plugin. **`otel`** is done in its
  useful half (§6.7) without the SDK; **`informer`/`resetter`** are the first row of this table;
  **`status`**, **`rpc`**, **`metrics`**, **`kv`**, **`lock`**, **`jobs`**, **`grpc`** are §6; **`logger`**
  and **`app-logger`** are the access log plus the worker `LOG` frame.
- **`protoreg`** — see §10 item 2 and §6.6's RR comparison. It exists to feed reflection, which is
  declined, and it is the `protox` cost moved into a plugin. Having now read it, the cost is larger
  than that sentence suggested: it pulls `jhump/protoreflect` (both v1 and the v2 beta) and compiles
  every file *and its imports* recursively, which is a descriptor compiler and not a parser. §6.6
  records what it buys and what its absence costs.
- **`plugin-abis`** — a compile-time DI container in Go, so there is no ABI to copy; `plan.md` item 39
  already refuses it.

**One of the four rows in the first table was a defect rather than a missing feature, and it is
built — §6.8.** That is the useful conclusion of the sort: the parts of RoadRunner that volucer
has deliberately not copied are the parts that duplicate a reverse proxy or a framework, and the
parts that remain are small.

---

## 2. The question this document has to answer first

There are two separable requests in the original ask, and conflating them is how a plan becomes a
wish list:

1. **More RoadRunner-style services** — gRPC, KV, locks, jobs, metrics, a control plane.
2. **More runtimes** — Python/Django and Ruby/Rails in worker mode.

They share exactly one foundation, and it is not obvious which: **a way to call the server from a
worker**. KV, locks and the jobs producer are not really "plugins" in RR; they are *methods with a
different implementation behind each*. So volucer cannot have two of them without first having the
call path. Meanwhile the runtimes need no control plane at all — §7 shows the cheap path uses the
`upstream` backend that already exists.

That is the whole structure of the plan: **one new subsystem (§4), then services on top of it
(§6), and a separate, smaller track for runtimes (§7).** And §4 is smaller than it sounds, because
the channel is already open: what §4 designs is the *branch* on inbound frames, not a transport.

---

## 3. Compatibility stance: native, not drop-in

**Recommendation: volucer-native configuration and ergonomics; RoadRunner's *semantics* as the
spec, its *file format* not at all.** Application-facing APIs should be shape-compatible where a
PHP app would otherwise have to change code, which is a different thing from parsing `.rr.yaml`.

Reasons, none of which are about effort:

- **It is already the settled position.** `docs/plan.md` §3 and §4 chose an own protocol over
  goridge ("adopting its semantics means becoming a RoadRunner clone"), TOML over YAML, and a
  config model with `#[serde(deny_unknown_fields)]` on every table. Reading `.rr.yaml` would add a
  second configuration language, a YAML dependency, and a versioning commitment to somebody else's
  schema — and the schema in the checkout is already visibly out of step with the engine (it still
  enumerates `zipkin` and `jaeger` exporters that v6 rejects).
- **The formats disagree on purpose.** RR's plugin sections are a flat namespace with
  `driver` + `config` nesting and driver-specific keys interpreted by JSON-schema `if`/`then`
  chains. volucer's model is typed enums tagged by `kind` (`BackendConfig`,
  `crates/volucer-core/src/config.rs:1349`) with validation in Rust where the error message can be
  written for a human. Emulating the former in the latter loses the only advantage the latter has.
- **Where compatibility *does* pay is the PHP side**, and that is cheap to provide without
  touching the config: a KV client that implements `Psr\SimpleCache\CacheInterface`, and a jobs
  driver that implements the queue interface `sailantis/azera-queue` already defines
  (`azera-queue/src/Backend/Contract/ReservableQueueInterface.php`). An application swaps a class,
  not a YAML file.
- **The one thing to borrow literally is metric names.** `rr_http_request_total` and friends are
  what dashboards and alerts are already written against. Renaming them buys nothing and costs
  every operator a migration. See §6.1.

There is, however, one deliberate *structural* lesson to take from RR, in §5.

---

## 4. The control plane

### 4.1 What is missing today

volucer has no external control surface at all. `Command` in `crates/volucer/src/main.rs:34` is
`Serve`, `Check`, `Version` and nothing else; a running server exposes no way to ask it anything or
tell it anything. Signals are the entire interface (shutdown, and `SIGHUP` for reload — plan.md
hazard 60). Internally the pieces exist and are unused by anything outside the process: worker
`CONTROL` messages (`Drain`, `Shutdown`, `Reload`, `Report` —
`crates/volucer-worker/src/message.rs`), per-listener `Stats`
(`crates/volucer-server/src/listener.rs`), and the process-wide `volucer_watch::Registry` that
owns the one reload path.

**This section is still true, and that is the point.** §4.2–§4.6 were built, so an application inside a
worker can now call `workers.list`, `worker.reset`, `kv.get`, `lock.acquire` and the rest. What is
*not* built is the second process — which is what an operator has, and the reason the CLI is still
`serve`, `check`, `version`. §4.6 assumed those two were one feature. §4.7 is the accounting of why
they are not, and it is the only premise of this section that did not survive the code.

### 4.2 The design: one channel, one dispatcher

The premise is narrower than "build an RPC framework". Everything already arrives on one pipe; the
only open question is **what an inbound frame from the server means**, and the answer is a two-way
branch that already exists:

- a frame belonging to the **request in flight** → it is the response, and the loop proceeds exactly
  as today (`RESPONSE` → `BODY` → `END`, terminating at `END`);
- **anything else** → it is a call from the server, which gets an answer and changes nothing about
  the response exchange.

That second arm is what already happens. `CONTROL` gets an answer and the loop continues; `PING`
gets `PONG`; `LOG`, `METRIC` and `EVENT` are absorbed without derailing the exchange. A
`CALL`/`RETURN` pair is *another arm on that same match*, not a new transport, not a new port, and
not a new socket class. The direction is **worker → server** (§4.3): the application asks, the server
answers. The user-facing rule is one sentence: **while a request is in flight, a frame that is not
part of the response is a call.**

**Client side is small, and it can be non-reentrant.** `Worker::call()` writes the `CALL` and reads
frames until its own id comes back, handling a `PING`, a `CONTROL` or another `CALL` on the way
exactly as the main loop handles them — the same shape as the existing dispatch, with `RETURN` arms
added. Nothing needs to interrupt the application, because:

- worker-initiated calls only make sense from inside a request (that is when there is state to
  inspect, and when the server has a reader waiting);
- a `CALL` that arrives while the worker is idle is therefore impossible by construction, and
  `Worker::call()` raises rather than hanging if it is attempted — see §4.3.

So the SDK never needs a nested pump inside handler execution. It needs a dispatch loop that answers
the arms, which it already has, plus one method that adds an arm of its own.

**Server side is where the design work is, and all of it is one thread.** There is exactly one
reader per worker, on the lifetime exchange thread (`exchange.rs:76`), and it is blocked on
`recv()` **between** requests — nothing reads the pipe then, so telemetry and control frames are
only ever drained mid-exchange and only while a request is in flight. Four consequences:

1. **Handle calls in `read_response`, and answer them from the registry.** `codec.rs:735` currently
   ends in `other => Err(WorkerError::NotA { expected: "a response frame", … })`, which is *fatal* —
   an unknown type mid-response kills the worker. A `CALL` arm has to be handled before that, or it
   behaves as a protocol failure. The arm writes a `RETURN` rather than a `CALL`: the server answers
   here, and the only thing that originates a call is the worker (§4.3).
2. **The exchange thread does not need to know what a KV store is.** It hands the call to a handler
   and writes back whatever comes out, so the frame layer does not learn about services and the
   service layer does not learn about framing. §5's service bag is what the handler set is built
   from.
3. **The call inherits the request's deadline, and the deadline is a kill.** `request_staged`'s own
   documentation is the reason this is the one real cost: a read failure means "the application may
   be running it right now", and the pool's answer is to kill the worker to make the pending read
   return. A worker blocked on a slow `lock.acquire` is therefore a worker that can be killed
   *mid-call*, and the server must not conclude "not held" from that — see §6.3, where the same
   ambiguity is already an explicit error variant. The fix is a per-call deadline enforced *inside*
   the handler, so that "the backend was slow" is an answer rather than a kill.
4. **A server-side panic takes the worker with it.** The handler runs on the exchange thread, so an
   escaping panic is a poisoned or dead worker. Handlers must be `catch_unwind`-guarded, and that is
   a rule rather than a nicety because the failure mode is a request path that dies silently.

`CALL` is **worker → server** only. A server never originates one — it already has `REQUEST` for
that — so a `CALL` arriving *at* the server is a protocol error, and the server's arm for it writes a
`RETURN`. Anything a worker says on its own keeps the semantics it already has (`LOG`/`METRIC`/`EVENT`
are unsolicited and absorbed), and the response remains the worker's way to answer the work it was
given.

**Why not a separate socket.** It was the earlier recommendation in this document, and it is worse
for a reason that has nothing to do with effort: a second socket makes the worker's liveness
ambiguous. Close the data pipe and the worker's `read` returns EOF and it exits — that is the whole
shutdown mechanism, and `docs/worker-protocol.md` is explicit that it is "shutdown by
construction", with no signal to catch. Adding a channel the worker also blocks on destroys that
property. It also costs a second accept loop, a second handshake, a second protocol to version, and
its own address plumbing, to reach a process the server is *already* connected to over a channel
that was measured faster than the alternatives (hazard 46 — a Windows pipe read is ~8 ms for PHP
against ~0.09 ms on a socket; the socket transport already exists for that case and is selected
automatically).

**Hazard: a call is served while the caller's own slot lock is held, so a call handler may not take a slot
lock.** Found by the first end-to-end test of the call plane, which hung until the request deadline killed
the worker: `workers.list` walked every slot with `lock()`, and the call was being served on the exchange
thread of a worker whose request was holding slot 0 — so it waited for its own lock. Nothing in the design
above is wrong; what was missing is that **a call runs inside a request**, which the existing invariant
"the slot lock is never held across a request" had stopped being true of. The rule for handlers is
therefore: a slot lock may only be taken by something that is not serving a request, so a handler reads
pool state with `try_lock` and reports what it could not observe rather than waiting for it. That is not a
degradation — a slot that is locked is a slot serving a request, which is exactly `idle: false`, and it is
not restartable either (`WorkerPool::restart` refuses it with `AllBusy`).

**Hazard: an undrained channel stalls a call at the deadline.**
`ChannelExchange::request` posts an exchange and waits on `recv_timeout`, so a worker blocked
answering a server call cannot have the next *request* written to it — the send succeeds, the
answer never comes, and the request dies at its deadline with the worker then killed. The rule that
makes this sound is what §6.3 already learned from RoadRunner's own lock implementation: **a call
that ran out of time has unknown state.** The server may drop the operation and keep the worker
(reporting the failure), but it must **not** kill the worker as the timeout strategy while calls are
outstanding — killing closes stdin, the worker exits, and the response that was in progress is lost
too. This is the one policy that has to be written for this design and it is not obvious from
reading either side alone.

**Why "one request in flight" survives this.** The three places that state it
(`docs/worker-protocol.md`, *Correlation*; `crates/volucer-server/src/pool.rs:12`;
`crates/volucer-fcgi/src/client.rs:7`) are making two separate claims, and only the second is at
stake here. The first — no request *multiplexing*, so no demux table — is untouched: there is still
one request in flight. The second — that the worker "only ever handles `HELLO`, `REQUEST`, `PING`
and `CONTROL`" (`docs/worker-protocol.md`, *Which types a worker may receive*) — becomes false, and
the doc has to say so rather than quietly gaining an arm. Neither claim is about the *response*
being one exchange, which is what actually has to hold.

**An earlier draft of this section recommended a separate relay socket, and was wrong about it.**
The reasoning it gave — "a nested pump that re-enters the same read path" — overstated the client
and misplaced the server difficulty. The client needs a dispatch arm, not a reentrant pump, and the
server's cost is one `catch_unwind` and one deadline policy on a thread that already exists.
Preproduction and one channel in hand make this the only sensible choice, and the rest of §4 is
written for it.

### 4.3 The message set on the one channel

Two new types, and one optional field on an existing one. Nothing else changes.

| # | Name | Direction | Correlated | Carries |
| --- | --- | --- | --- | --- |
| 15 | `CALL` | worker → server | yes | `name` (`"<namespace>.<verb>"`), JSON params, `request_id` = the call's own id |
| 16 | `RETURN` | server → worker | yes | `ok`, JSON result or an error, and `unknown`, matching `request_id` |

**The direction is worker → server, and it is worth stating plainly because the opposite reading is
just as plausible on first contact.** A call is the *application* asking the server for something —
`kv.get`, `lock.acquire`, a metric — which is the only direction that has a reason to exist: the
server already has a complete way to ask a worker to do work, and it is `REQUEST`. So a call changes
nothing about the response exchange it arrives during, and the id it carries is allocated by the
**worker**, counting down from `u32::MAX`.

`CALL` ids are therefore allocated from a range disjoint from request ids — request ids are the
server's and start at 1 and increment (`FIRST_REQUEST_ID`) — which keeps "a stray correlation bug is
visible instead of silently matching" true (`crates/volucer-worker/src/lib.rs:53`), the property that
made 0 reserved. Two consequences follow, and both are load-bearing:

- **A server never originates a call**, so a `CALL` arriving *at* the server is a protocol error rather
  than something to serve — `docs/worker-protocol.md`'s "which types may be received" table now says so
  on both sides.
- **A worker only ever calls while a request is in flight.** The server's reader for a worker is parked
  on `recv()` between requests, so an answer to a call made outside a request has nobody to arrive at.
  `Worker::call()` refuses rather than hanging on that, because a hang there is a worker killed at the
  *request's* deadline with nothing in the log naming the call responsible.

`HELLO_ACK` gains one optional field, `runtime` (`"php"`/`"python"`/`"ruby"`), which §7.2 needs
anyway and which lets the server apply the right policy without guessing. Additive, but see §4.8 —
nothing here is versioned against a deployed peer.

**What the discarded relay design would have added, and does not.** No second socket, therefore no
cross-wiring hazard between two protocols sharing one header, no second handshake, no second version
to negotiate, and no crate extraction to keep a second protocol from depending on the data-plane
crate: the frame primitives stay in `volucer-worker` because only one protocol uses them. What
replaces the old §4.4 is the method registry below.

### 4.4 The method registry

Methods are `"<namespace>.<verb>"`: `kv.get`, `kv.set`, `lock.acquire`, `lock.release`,
`jobs.push`, `metrics.observe`, `workers.list`, `worker.reset`, `config.describe`, `server.version`.
Params and results are JSON (the codec stays behind its boundary; msgpack remains available for the
same reason it does in the worker protocol).

> **`metrics.observe` is *not* how an application's numbers arrive, and the name in this list is a
> leftover.** `emitMetric` is an unsolicited `METRIC` frame — there is no call, no return, and no
> `metrics` namespace in [`CALL_NAMESPACES`](crates/volucer-core/src/config.rs) — and the declaration
> that decides a sample's meaning is `[metrics.collect]` (§6.1). A `metrics.observe` method could
> still be worth having, for an application that wants to *declare* a histogram at runtime rather
> than a deployment declaring it in a file, but it is unbuilt and the frame path covers the common
> case. Recorded here because a list of method names is read as a list of what exists.

The registry maps a namespace to a handler, and the handler set is *the* thing a new feature
registers with — which is §5's shape. It lives in `volucer-server`, is built by `serve` from the
configured services, and the exchange thread holds an `Arc` to it. **A method that is not registered
is a `RETURN` error naming the method**, not a dropped frame and not a dead worker.

`workers.list`, `worker.reset` and `config.describe` need no new service at all — they read the pool
and the configuration that already exist — so a control plane is available from the first commit
here rather than after KV is written. `volucer workers` and `volucer reset` are meant to be clients of
it; **what that client has to connect *to* is §4.7, and it is not the channel this registry travels
on.** `config.describe` is **redacted** (§4.6), because the answer includes driver config and would
otherwise hand out credentials over a channel any worker can write to.

### 4.5 Security: the channel is now a remote-execution surface

Routing calls over the worker channel removes the socket and its permission bits, so the only thing
between an application and `worker.reset` is the pipe the server opened. That is not a hole — a
worker already runs application code as the same user — but three things follow, and the second is
the one that would be forgotten:

- **Capability is per-worker, not per-address.** There is no address to protect, so the registry
  must be selected per *backend*: a `worker` backend for the public HTTP listener gets read-only
  namespaces, and `worker.reset` / `config.describe` are available only where configured. A global
  method table would give every worker every capability.
- **`config.describe` is redacted** with a per-service allow-list, because its answer includes
  driver configuration and would otherwise hand out credentials over a channel any worker can write
  to. RR's `rpc.Config` does exactly this and it is a cautionary example, not a precedent.
- **The calls are refused when `[rpc]` is absent.** No namespace is registered, so nothing is
  reachable, and an application that tries gets a named error rather than silence. This replaces
  "the relay is off unless configured" with something stronger: there is no surface to be off.

### 4.6 What this unlocks immediately

- `volucer workers` and `volucer reset`, which would be clients of the method registry if the registry
  were reachable — see §4.7, which is the one thing in §4 that had to be corrected after the code was
  written. Neither needs a *service*: they read the pool
  (`restart(index)`, `crates/volucer-server/src/worker_pool/mod.rs:1161`)
  and the configuration that already exist. The sentence that matters is that no *service* has to exist
  for them, which turned out to be true: `workers.list` and `worker.reset` are answered by the server's
  own dispatcher, so nothing has to be written but the way in.
- The `status`/health endpoint (§6.1) can borrow the same `ready` notion.
- Every §6 service has somewhere to live from the day it is written.

### 4.7 The correction: there is no channel for a second process

**§1.4 and §4.6 both said the CLI "needs a channel and nothing else", and that is wrong** — wrong in
a way worth writing down rather than quietly editing, because the sentence is plausible on first
contact and would be re-derived by anyone reading §4.2 alone.

The channel that carries `CALL`/`RETURN` is the pipe volucer itself created when it spawned
`php worker.php` ([`boot`](crates/volucer-server/src/worker_pool/handshake.rs:167), which takes the
child's `stdin`/`stdout`) or the loopback socket it bound for a socket-transport worker
([`handshake.rs:162`](crates/volucer-server/src/worker_pool/handshake.rs:162)). Both ends of it are
already held: one by the pool's exchange thread, the other by the child. **No second process can open
it**, and on the pipe transport the kernel did not even give it a name. The method registry travels
on that channel, so the registry is reachable from *inside an application* and from nothing else.

**RoadRunner is not a counterexample, and the difference is the whole point.** `rr workers` is
`net.Dial` to `rpc.listen` (`roadrunner/internal/rpc/client.go:70`) — a listener the plugin created
*before* any worker existed, deliberately, because its CLI was always going to be a client of it.
volucer's §4.2 removed exactly that listener and replaced it with the per-worker channel, which is a
better design for the application-facing half and **has no second-process half at all**. The two
halves were never the same feature, and collapsing them into "one socket" is where this document went
wrong.

So the missing piece is one inbound transport for a caller that is not a worker, and its shape is
decided by one property: **it is the first thing in this process that a client the server did not
spawn can connect to and *mutate* through.**

#### The shape: an HTTP endpoint on the listener that already exists

Not a raw line protocol, and not a second listener. The reasons, in order:

- **A framing codec would have to be specified, and it already has been — as HTTP.** `workers.list`
  answers JSON ([calls.rs](crates/volucer-server/src/calls.rs:2532), `serde_json::to_value`), and the
  registry's whole point is that a method is a name, JSON params and a JSON result
  ([§4.4](#44-the-method-registry)). HTTP carries that with no new document, and the alternative —
  a length-prefixed frame the CLI must frame by hand — is a second wire format to version for no
  reader that needs one.
- **The listener already exists, already reaches the dispatcher, and is already off by default.**
  §6.1's `[status]` endpoint holds one `Arc<Dispatcher>` and serves `/health` and `/ready` from it.
  `Dispatcher::pools()` ([dispatch.rs:612](crates/volucer-server/src/dispatch.rs:612)) exists
  specifically so that "which pool is worker 3 in" has one implementation, and its own comment
  already names the CLI as the intended caller. `GET /workers` is that accessor rendered, and
  `POST /workers/{index}/reset` is `WorkersNamespace::serve` with the index taken from the path
  instead of from a JSON body.
- **A second listener would be a second address to bind, to print, to firewall and to stop** — and
  it would have to re-answer the question §6.1 already answered, about which of the process's
  addresses faces the public. There is no *capability* argument for keeping the mutation endpoint off
  the scrape port, because either address can be exposed by a one-line misconfiguration; what
  protects the endpoint is that it does not exist unless asked for, which is the same rule that
  makes `[metrics]` opt-in.
- **It removes the one objection "why not a separate socket" does not already settle.** §4.2 refuses
  a second socket *for the worker*, because a worker that blocks on two channels destroys
  shutdown-by-construction. A CLI-facing socket is server-only and touches none of that — which is
  why the shape here is a design choice rather than a consequence of §4.2. The choice is made on the
  cost of a listener, not on protocol purity.

#### What has to be written, and the two decisions that are not obvious

**`[status]` grows a mutation switch, and it is off until it is set.** A `GET` is safe to hand to
whoever can reach a port; a `POST` that recycles a worker is not. So the routes exist only when the
deployment says so, which keeps §6.1's guarantee — a deployment that configured `[status]` for an
orchestrator gains no mutation surface by upgrading:

```toml
[status]
address = "127.0.0.1:2114"
allow_worker_reset = true    # default false. Absent, POST is a 405 and GET /workers a 404
```

Two things follow from the routes being *inside* the status listener rather than beside it, and
neither is a detail:

1. **The reset must not run on the status listener's thread.** `RunningStatus` is a `tokio` runtime
   ([status.rs:167](crates/volucer-http/src/status.rs:167)), and `WorkerPool::restart` boots a
   process and completes a handshake under a 30-second deadline — blocking work with a blocking
   failure mode. It goes through `spawn_blocking`, the same boundary the dispatcher is already
   reached through on the request path.
2. **A refusal is a status code, not a 200 with a JSON error.** `WorkerPool::restart` already
   distinguishes the cases ([worker_pool/mod.rs:1161](crates/volucer-server/src/worker_pool/mod.rs:1161)):
   an index past the end is `404`, a slot that is *serving a request* is `409` — because
   `take_specific` returns `None` exactly when the worker is busy — and a respawn that failed is
   `500`. A caller scripting this wants `curl -f` to fail on the first two, and a JSON envelope that
   says the same thing would make it parse a document to find out.

**What is *not* proposed: `config.describe` over HTTP.** The namespace exists and is redacted, but
its whole threat model in §4.5 is a channel only an application can write to; a redacted document on
a port an orchestrator scrapes is a different exposure, and it should not ride in on this row. It
stays a worker-callable method.

#### Verified against the code, not assumed

- The registry is a `BTreeMap<String, Arc<dyn Namespace>>` and the same `WorkersNamespace` instance is
  what answers a worker's call, so the status listener can hold an `Arc` to *that* rather than to a
  second copy — one implementation of `workers.list`, two callers.
- `MethodRegistry::serve` is `&Call -> Return` and needs nothing from the frame layer, so nothing
  about `volucer-worker`'s message set changes: no new type number, no handshake field, no
  `docs/worker-protocol.md` edit. This is the §4.8 argument reused, and it is why the row is small.
- `RunningStatus` binds inside `start` and prints its `.local_addr()`, so a bound port is already a
  startup fact and no new failure mode is introduced.
- The status listener answers a 404 to every path it does not serve ([status.rs:28](crates/volucer-http/src/status.rs:28)),
  which is the behaviour `POST /workers/{i}/reset` has to keep for the paths this row does not add.

### 4.8 Versioning, now that nothing is deployed

`protocol_version` stays `1` and gains no "v2" — it is the *frame* version, and the frame did not
change. What changed is the message set, and the honest record of that is
`docs/worker-protocol.md` plus the SDK's version, not a protocol bump. This is only safe because
nothing is released: with deployed workers in the field, a new message type is **fatal** to an old
SDK (`docs/worker-protocol.md`'s error table makes an unexpected type unusable-stream, not merely a
warned-and-skipped frame), so this would have been the one change in this document that could not be
rolled out server-first. Writing it down now is what stops it being discovered later.

---

## 5. The extension point, which is the one structural lesson

RR's extensibility comes from `endure`, a runtime DI container, and a plugin model where any plugin
can request any other. That is the right idea for Go and the wrong one for Rust: we do not need a
container to discover types, and a dynamic plugin ABI would give up static dispatch, the MSRV gate,
`cargo package --workspace` and cross-compilation in one step.

The Rust-shaped equivalent is a **compile-time service registry with a lifecycle trait**, plus
cargo features to choose what is compiled:

```rust
// crates/volucer-server/src/service.rs
pub trait Service: Send + Sync {
    fn name(&self) -> &'static str;
    /// Report readiness to the status endpoint. Cheap, non-blocking.
    fn ready(&self) -> Ready;
    /// Stop, with the same deadline discipline the rest of the server uses.
    fn stop(&self);
}
```

`serve` builds a `Services` bag, starts whatever the configuration asks for, hands the bag to the
listeners that need it, and stops it in one place alongside the existing shutdown sequence
(`crates/volucer/src/main.rs:1048`). A listener that wants metrics reads them from the bag; a
call handler that wants a queue asks the bag. Three properties follow, and each is a bug the
alternative has:

- **No service can be started twice or leaked.** The bag is the only list, so "the server shut down
  and left a Kafka consumer running" is not expressible.
- **A call cannot reach a service that was not configured**, because the bag is what the method
  registry was built from.
- **A build without a driver has no code for it**: `cargo install volucer` must not link a Redis, a
  Memcached and an AMQP client for a deployment that uses the in-memory queue. Features, grouped
  per driver, with `memory` in the default set.

The **configuration shape** follows the feature boundary rather than nesting everything under
`[server]`: `[metrics]`, `[status]`, `[services]`, `[kv]`, `[lock]`, `[jobs]`, `[grpc]`, each
`deny_unknown_fields`, each optional. `[server]` stays about the request path. This mirrors RR's
top-level section names, which is a coincidence worth having: an operator reading both
documentations recognises the words.

**There is no `[rpc]` table**, because there is no listener to configure: the method registry
travels on the worker channel that already exists, and §4.4 is about which methods a *backend* may
call. That is a per-backend setting, so it belongs beside the backend:

```toml
[backends.app]
kind    = "worker"
command = "php vendor/sailantis/azera-volucer/azera-worker.php"
# Namespaces this backend's workers may call. Absent = none, which is the safe default.
services = ["kv", "lock", "jobs", "metrics"]

[backends.ops]
kind     = "worker"
command  = "php bin/ops-worker.php"
services = ["workers", "config"]
```

A **worker with no `services` key can call nothing**, which is the right default for a backend
facing the public and means "add KV" is a one-line diff rather than a new exposure.

---

## 6. The services

Each section answers the same four questions: what it is, what it attaches to, what it copies
faithfully, and what is genuinely hard.

### 6.1 Metrics and status — first, and not negotiable

**Why first.** It is the only feature whose absence makes every other feature harder to land: there
is currently no way to see whether a worker pool is healthy, a queue is draining, or a lock is
contended, and `volucer-observe` today is request identity, W3C trace context and an access log
(`crates/volucer-observe/src/lib.rs`) with no registry at all. It also needs no calls, no driver
and no new protocol, so it can land while the design of everything else is still being argued.

**What already points at it.** `absorb_log` / `absorb_metric` in
`crates/volucer-worker/src/codec.rs:720` already decode `LOG` and `METRIC` frames and then do
almost nothing with them — `METRIC` is attached to the response and dropped, `EVENT` is a
`tracing::trace!` (`codec.rs:721`). The PHP SDK already has `emitMetric`, `emitLog` and `recycle`. The PHP SDK already has `emitMetric`, `emitLog` and `recycle`. So the wire path
for "an application publishes a metric" exists and is tested; what is missing is a place for the
sample to go.

**Design.**

- New `volucer-metrics` crate: a registry of counters, gauges and histograms, keyed by name and
  label set, cheap enough to update from a request path (atomics and sharded maps; a histogram is a
  bucket array of atomics).
- A **hand-written** Prometheus text exposition encoder rather than the `prometheus` crate. The
  format is `# HELP` / `# TYPE` / `name{label="v"} value`, it is a page of specification, and the
  crate brings `protobuf` for OpenMetrics which we do not want in the graph — a decision consistent
  with the static-file layer and the access log, both hand-written for the same reason.
- `[metrics] address`, off by default; **its own listener**, never a route on the HTTP listener,
  because that listener may be TLS-only and faces the public.
- `[status] address` with `GET /health` and `GET /ready`, which reports per-service readiness from
  the bag. Cheap, and the thing a container orchestrator asks for.
- **Worker `METRIC` frames become samples.** `absorb_metric` publishes into the registry, so a
  PHP application's `emitMetric` reaches Prometheus without the application knowing the endpoint
  exists. `LOG` keeps its current behaviour (attached to the response, already forwarded to the
  front-end's error log).
- **Names.** Plugin metrics keep RoadRunner's names with the `rr_` prefix, because operators'
  dashboards are written against them (see §3). The pool set is the same shape RR emits for
  `http`, `grpc` and `jobs`:
  `rr_http_total_workers`, `rr_http_worker_state{pid,state}`,
  `rr_http_workers_ready`, `rr_http_workers_working`, `rr_http_worker_memory_bytes{pid}`,
  `rr_http_request_total{status}`, `rr_http_request_duration_seconds{status}`,
  `rr_http_requests_queue`, `rr_http_uptime_seconds`.
  Our own additions — a FastCGI front-end, the ACME path, reloads — get names in the same
  namespace: `rr_fastcgi_request_total`, `rr_acme_renewals_total`, `rr_reloads_total`.
  Note that RR requires the `http_metrics` middleware to be listed for the HTTP metrics; volucer
  should emit them whenever `[metrics]` is present, and be explicit about the divergence in §9.3.
- **`[metrics] collect`** for application-declared metrics, pushed with a `metrics.observe` call,
  so `emitMetric` can be upgraded from fire-and-forget to a
  declared histogram without a protocol change.

**Built, and the estimate above was wrong in the flattering direction.** The crate, the encoder,
both listeners and the `absorb_metric` wiring landed first; what did not land for a long time was
the `collect` table, and it turned out to be **not** an optional upgrade. A registry refuses a name
nobody described — deliberately, so an application cannot widen the published set at runtime — and
the only names the process described were `rr_http_*`. So **every `emitMetric` in every deployment
was decoded, counted as `undeclared`, and dropped**, and the three tests of the path all passed
because each of them declared its own metric by hand. The tests were the missing configuration.

Four things came out of building it, and each is a decision rather than a detail:

- **A sample's meaning comes from its declaration, not from the sender.** `emitMetric` carries a
  name and a value, so `Registry::record` reads the declared kind and applies it — a counter's
  sample is an increment, a gauge's is a reading, a histogram's is one observation. Without that,
  the only way to write a sample was `observe`, which refuses anything that is not a histogram as
  the wrong kind, so a declared counter would have been refused on every write.
- **A counter's increment must be a whole, non-negative number**, refused rather than rounded — and
  the check runs *before* the series is created. The first version checked it after, and a refused
  sample left a zero-valued counter behind: the "metric that exists and never moves" that refusing
  an undeclared name exists to prevent. A test that asserted the render was empty caught it.
- **Names beginning `rr_` are refused**, because that namespace is the server's. The collision is
  real: `rr_http_request_total` is a name every operator's dashboard already has, and an
  application declaring it would replace a server metric under the same label-free series.
- **Prometheus puts the unit in the name** (`_seconds`, `_bytes`), so `MetricRecord`'s `unit` field
  has nowhere to go on the exposition. It is kept on the wire and not rendered; the declaration's
  `help` and the name carry it.

**And it caught the SDK documenting names that could never work.** `emitMetric`'s own signature
allowed anything, and every example named a metric with dots — `request.count`, `example.request`,
`worker.peak_memory_bytes`. A dot is illegal in a Prometheus name (it is Prometheus's own
`recording.rule` vocabulary), so each of those was a sample that would arrive and silently do
nothing. The SDK now says the rule in `emitMetric`'s docblock and uses `example_request_total` and
`worker_peak_memory_bytes`.

**What is hard.** Nothing conceptually; the discipline is in the cardinality budget. A label
derived from a request path is how a metrics endpoint takes down a Prometheus server, so labels
come from a closed set (status class, method, site name) and the registry caps distinct series per
metric with a logged refusal beyond the cap.

**Sizing.** S–M. Registry + encoder + two listeners + wiring `absorb_metric`. **The `collect`
table alone was S, and understated for the reason above: the row in §1.4 read "one registry entry
and one namespace arm" and the real work was a declared-kind dispatch, a pre-creation validation
and a name rule the SDK was violating.**

### 6.2 KV

**Attaches to.** The call path (§4), plus a `Service`.

**Config.**

```toml
[kv.default]
driver = "memory"           # memory | redis | memcached | file
# [kv.default.redis]
# address = "redis://127.0.0.1:6379"
# db = 0
# pool_size = 16
# [kv.sessions]
# driver = "file"
# path = "/var/lib/volucer/kv/sessions.db"
```

Deliberately flatter than RR's `driver` + untyped `config` pair: a driver-tagged enum, which is the
same pattern `BackendConfig` uses and which makes an invalid key an error at parse time rather than
a silently-ignored one.

**Built, and three things about it were decided by the code rather than by this document.**

- **`memory` is done, end to end.** `volucer-kv` is a leaf crate — the trait and the driver — and a
  test drives a real PHP process through `Kv::set` and `Kv::get` against a real `MemoryStore`, with
  two workers so that "the next request finds what the last one left" is asserted rather than
  assumed. The bound is **bytes**, not entries (100k entries of 1 KiB is 100 MB and of 1 MiB is
  100 GB), expiry is decided on read and swept only when an insert is about to fail, and a store
  that is full **refuses** rather than evicting, because a refusal is visible in a log and an
  eviction is a cache miss that looks like the application's bug.
- **A storage is named, and a backend is granted it by name.** `services = ["kv.sessions"]`, checked
  against this file's own `[kv.*]` tables at startup. A bare `"kv"` is refused rather than resolved to
  the only storage there is: a deployment that adds a second later would otherwise find the first
  worker's calls quietly going somewhere else.
- **The value is base64 on the wire and the server has no serializer.** A JSON string cannot carry a
  byte above `0x7f`, and an application that compresses or encrypts what it caches has such bytes
  constantly. This is the part of the design that is load-bearing rather than stylistic: it is what
  makes end-to-end encryption possible at all, where RR's server-side serialization makes it
  impossible.

**Copied faithfully.** Per-item TTL; PSR-16 method set (`get`, `set`, `has`, `delete`, `clear`,
`getMultiple`, `setMultiple`, TTL accessors); a key namespace per storage so two applications sharing
one Redis do not collide; the serializer being pluggable on the *client* side rather than fixed on
the server, because that is where the E2E-encryption case lives. `deleteMultiple` is not implemented:
`getMultiple` and `setMultiple` exist because they are one call where the loop is many, and a delete
of n keys is n cheap calls whose batched form would buy a round trip and cost a wire format.

**Drivers, in order.** `memory` (**done**) → `redis` (**done**, behind the `redis` feature) → `file` (an
embedded store; see the hazard below) → `memcached` (a small text-protocol client; the weakest of the four
because RR's own memcached driver cannot report TTL, so it is last and optional).

**Hazard.** RR's boltdb driver has a `permissions: 0755` default on a *database file*, which is
world-readable where it should not be. If volucer ships a file-backed driver, the default is `0600`
and the check is on the file rather than on the directory — the same rule plan.md hazard 73 settled
for certificates.

**What the `redis` feature costs, measured rather than assumed.** Thirty-one crates, almost all of them
ICU through the `url` parser the client uses for its connection string — `icu_normalizer`, `zerotrie`,
`yoke`, and their derive crates. That is a real cost and it is why the feature exists rather than the
dependency being unconditional: `cargo tree -p volucer --no-default-features` contains no `redis` and no
ICU at all, and a deployment using the in-memory driver pays none of it. Worth writing down because the
alternative — "just depend on redis, it is one crate" — is wrong by a factor of thirty-one, and the
measurement is the only way anybody would know.

**Sizing.** S for `memory`, M for `redis` and the trait, S more per additional driver.

### 6.3 Locks

**Attaches to.** KV's connection handling (the Redis case shares a client), and the call path.

**Config.**

```toml
[lock]
driver = "memory"           # memory | redis
# [lock.redis]
# address = "redis://127.0.0.1:6379"
# read_timeout = "5s"
```

**Copied faithfully, including the parts that are easy to get wrong:** `ttl = 0` means held until
released; `update_ttl` replaces the remaining TTL rather than extending it; a second *read* lock
with the same owner id is refused; `wait = 0` is exactly one attempt bounded by `read_timeout`; the
key prefix is configurable but fixed per storage; and — the one that matters most — **a command
that failed or timed out during acquisition returns "lock state unknown", not "not held"**, because
treating an unknown as a failure is how two processes end up in the same critical section. That
error is a distinct variant, not a generic one.

**Hazard, and it must be in the README, not only here.** A TTL lock is not a correctness primitive
under a paused process: a worker that stalls past its TTL holds a lock the server believes has
expired, and no amount of care on the server side fixes it. RoadRunner's locks have **no fencing
tokens**, which means they cannot stop a late writer — the classic Kleppmann objection. So:

- The docs say what a lock is *for* (avoiding duplicated work, coalescing a cache miss) and what it
  is **not** for (protecting an invariant whose violation corrupts data). That is a stronger
  statement than RR makes and a more useful one.
- Where a real invariant is needed, the answer is the storage's own conditional write — Redis
  `SET NX`, a unique index, a compare-and-swap — and the KV API should expose a
  `set_if_absent` so that path exists.
- **Single-node locks work today and need no external service**, which is worth having for the
  common deployment before the Redis case is written.

**Sizing.** S for `memory`, M for the Redis script and its tests, S for the call methods.

**Built (`memory`), and it sharpened the argument rather than confirming it.** The sentence above —
"lock state unknown, not not-held" — is a type now, and one this document had already argued for twice
without either argument reaching code. Three things worth keeping:

- **`Return::unknown_state` had no producer until this step**, and the temptation when writing the
  first one was to use it too widely. Reporting a *refused resource name* as unknown — the caller's
  mistake, refused locally, nothing sent anywhere — is worse than reporting a real unknown as
  not-held, because a flag that is set on failures that are perfectly well known is a flag callers
  learn to ignore, and then the one real unknown is invisible among them. The code classifies three
  ways: `Unknown` is undetermined by construction; a rejected name is decided because nothing was
  touched; and a *backend* failure is undetermined during an acquisition only.
- **`ttl = 0` means held until released, and that is the opposite of KV's zero.** A lock that copied
  the KV accessor would be granted and released inside one call — which does not fail loudly, it never
  contends, so everything is allowed through and the exclusion is simply absent.
- **The wait slice is 5 ms, not RoadRunner's 1 ms.** A memory lease can expire because another process
  stopped existing, so nothing can be signalled and a wait is a poll; but a call is answered on the
  worker's own exchange thread, where a tight poll costs a core rather than a task.

What is still open, and stated as such: the `redis` driver, whose Lua script is the interesting part
and whose `SET NX`-with-owner-check is what makes release safe against a late writer.

**The `redis` driver is built, and that paragraph was half right.** `SET NX` is not what makes release
safe; the *owner check* is, and `SET NX` cannot express it because a set is one field and a resource
needs a set of readers. So a resource is a **hash** — one field per holder plus a `<id>:deadline`
sibling — and the scripts are what make the check-and-write one operation. Three of them, and each
exists for a race that a single-threaded test cannot see:

- `ACQUIRE` sweeps the resource's expired holders, refuses if the id already holds it, refuses on a
  conflict, and writes both fields — one operation, because two holders reaching a check-then-write in
  the same millisecond both conclude they won.
- `RELEASE` and `UPDATE` are scripts for a subtler reason than that one: a `HEXISTS` followed by an
  `HDEL` can have a *new* holder's write in between, and the `HDEL` would then remove the field a live
  holder had just created — a lock taken and silently thrown away.
- The expiry sweep rides on the way into `ACQUIRE` rather than on a timer, the same shape the memory
  driver uses and reached independently: a resource nobody writes to again keeps a few stale bytes,
  where a sweeper needs a connection, a timer and a lock to avoid racing the scripts that matter.

**And this is the first code that produces an unknown state**, which is what §6.3 was written for. The
classification is by error *kind*, and getting it wrong in either direction is expensive: a
`Client(_)` error is `redis-rs` refusing to send, so nothing is undetermined and the first version's
reporting it as unknown was over-reporting; a `Server(_)` error is Redis answering, however alarming,
and is a decision. Only `Io` — dropped or timed out — leaves an acquisition undetermined.

### 6.4 Jobs

The largest item, and the one to be most careful about promising. Split by *driver*, which is where
the cost actually lives:

| Driver | Client | Verdict |
| --- | --- | --- |
| `memory` | none | **ship** — the reference implementation, enough for a single node and for tests |
| `redis` | `redis-rs` (shared with §6.2/§6.3) | **ship** — the honest multi-node default, with a Lua script for reserve/ack atomicity |
| `file` | embedded store | probably, for durability without a broker |
| `amqp` | `lapin` | later |
| `sqs`, `kafka`, `nats`, `beanstalk`, `nsq`, `google_pub_sub` | one large client each | **not in this plan** — see §9.4 |**The engine** is driver-independent and is the part worth designing carefully. It copies RR's:

- **one internal priority queue** shared by all pipelines, of bounded `pipeline_size`, with
  `num_pollers` tasks pulling from it — and with the limitation **stated in the config
  documentation rather than discovered**: the queue is in memory, so a crash loses jobs that were
  reserved from the driver but not yet delivered, however durable the driver is;
- **reservation with a TTL**, ack/nack, requeue, and `auto_ack` as a per-job flag whose danger is
  documented where it is set;
- **named pools** selected by a reserved header, with an unknown pool name negatively-acking rather
  than silently routing;
- **retry, backoff and dead-lettering left to the application**, because RR leaves them there and
  inventing a server-side policy would make two applications' semantics fight;
- **trace context propagated as job headers**, once §6.7 exists;
- **`jobs.push` as a call** for the producer side, and the worker protocol for the consumer
  side (below).

**The consumer side reuses the worker pool, and this is the concrete integration.** A consumer is a
persistent process that booted once and is handed work repeatedly — which is exactly what
`WorkerPool` already does: boot, handshake, hand over one unit of work, read the answer, recycle on
`keep_alive = false` / `max_requests` / `peak_memory_bytes`
(`crates/volucer-server/src/worker_pool/mod.rs`, and `docs/worker-protocol.md`'s *server's side of
the contract*). So:

- A **`job` exchange** joins the existing `request` exchange in
  `crates/volucer-server/src/worker_pool/exchange.rs`. Same slot leasing, same timeouts and kill
  discipline, same recycling rules; only the payload differs.
- The protocol gains a small message set for it — the job payload in, and a terminal frame carrying
  `ack` / `nack` / `requeue` plus the failure reason and whether to requeue. It is deliberately
  *not* a new `REQUEST` with a magic URI: a worker must be able to tell a job from an HTTP request,
  because the per-request state reset differs.
- `[jobs.pool]` reuses `WorkerConfig` and therefore every knob it already has (workers,
  `max_requests`, `memory_limit_mb`, `checksum`, `transport`), plus RR's `max_jobs` alias is
  **not** copied — `max_requests` already means this and means it explicitly (see the field's own
  comment at `crates/volucer-core/src/config.rs:1795`).

**And it is done, as steps 5a–5c below.** What building it settled, in the order the code found it:

- **The two exchanges are two frame types, not one with a flag.** `JOB` (17) and `ACK` (18), because
  the thing a worker has to know *before* it touches any state is whether its own state is reset
  between units of work. A request worker resets; a consumer does not, and that is the entire point
  of the feature for an application. A magic URI on `REQUEST` would have made that question one each
  implementation answers for itself, and one of them would get it wrong. **A worker pointed at the
  wrong mode now refuses by name** rather than serving the frame as the other kind — in both
  directions, because a `REQUEST` on a consumer has nowhere to go either.
- **`ACK` had to carry the recycling signals, and the design did not predict it.** A job has no `END`
  frame, so `peak_memory_bytes`, `keep_alive` and `duration_us` have nowhere else to live — and
  without them `[jobs.pool] memory_limit_mb` and `max_jobs` would be configuration settings that
  parse, validate, and are read by nothing. That is worse than not offering them, because an operator
  has no way to find out. The frame is `Ack { outcome, peak_memory_bytes, keep_alive, duration_us }`
  with the outcome flattened in, so the JSON stays `{"outcome":"ack",…}` rather than growing an
  envelope that carries the type twice.
- **Three outcomes, not RoadRunner's five.** `NoError`, `Error`, `ACK`, `NACK` and `REQUEUE` are three
  decisions written two ways. `Ack` settles the reservation; `Requeue` negatively acknowledges it;
  `Reject` **acknowledges it without recording a failure**, because a job an application has decided
  against is not a job that is waiting, and nacking it would bring it back for ever.
- **The reservation is not the job id.** `ACK` correlates by the frame's `request_id`, which is the
  mechanism every other exchange here uses — so a worker whose lease expired cannot acknowledge the
  job that has since been handed to somebody else. The server checks a reservation the worker never
  saw.
- **One poller thread, and it reserves only once it has a slot.** One thread per pipeline would be
  several threads competing for the same slots, and the failure of that shape is not slowness: a
  thread that reserved and *then* waited for a slot holds a reservation while it waits, so the lease
  runs down on a job nobody is running. Reserving after the slot is taken is what makes the lease mean
  the time a consumer spends *working*.
- **`attempts` starts at 0 and is incremented only on the way out of a reserve**, never on a nack. A
  worker killed mid-job never runs its nack, so a count including failures would miss exactly the
  crashes worth counting.
- **No retry policy, no backoff, no dead-lettering** — the fifth thing this section said, and the one
  the code kept. The server's contribution is that `attempts` is *observable*; deciding what to do at
  attempt four stays the application's, and `example/consumer.php` shows the three lines it takes.
- **A throwing handler requeues.** A consumer that crashed must not lose the job, and an application
  that wants a crashing job *dropped* catches its own exception and returns a string — the visible,
  deliberate act. The loop that the opposite default guards against is bounded by the count above.
- **A consumer's `echo` is captured and forwarded as a `LOG`, not left alone.** Rule 1 of the protocol
  applies to a consumer exactly as it does to a request worker: an unbuffered write to stdout lands
  inside a frame. What differs is only where the bytes go — a job has no response body, so they go to
  the server's log rather than to a client.


**Both drivers are now built, and the Redis one is where three of this section's own predictions were
wrong** — which is worth recording, because two of them were the kind a test finds and one was the kind
only a *deployment* would have:

- **The first script was a stack, not a queue.** It pushed with `RPUSH` and popped with `RPOP`, which is
  LIFO: the newest job would have been handed out first. Nothing about that looks like a failure until an
  application depends on ordering, and the FIFO test failed on its first run. `RPUSH` in, `LPOP` out — and
  both reclaim and requeue use `LPUSH`, which is the *head*, so a job whose consumer died is the very next
  one handed out.
- **A reservation has to be two values, not one.** The map is keyed by the job id and holds
  `"<id>|<nonce>"`, where the nonce is fresh per reservation. The first version wrote the job id as *both*
  — which makes the settle's check "does the map hold what you say?" always true, and the check is the only
  thing that stops a worker whose lease expired from acknowledging the job a second consumer is running.
  §6.4 predicted the check; it did not predict that the check needs a value the caller *cannot* derive.
- **A payload that is not valid UTF-8 cannot make the round trip, and the driver says so at the push.** A
  Redis hash field is a string. Accepting the push and mangling the bytes would surface as a
  deserialization error in the application days later and nowhere near the cause, so the refusal is at the
  push, by name. The memory driver takes the same bytes happily — this is a *driver* limit, not a rule the
  trait imposes, which is why the test asserts both halves.
- **A Redis pipeline gets its own connection pool, unlike a KV storage.** Two storages on one Redis share a
  server and a namespace, so sharing connections is free. Two pipelines are two independent queues whose
  only relationship is the host, so pooling them would be head-of-line blocking across queues that have
  nothing to do with each other.
- **Reclamation rides on the reserve, and it is before the pop, not after.** A reclaim that ran afterwards
  would return the job to the ready list and leave it there until the *next* reserve — a consumer idling
  for a poll interval on a job that is ready right now. Same shape as the memory driver's correction, and
  the test fails either way, which is what a shared rule is for.
- **Redis Cluster is not supported and is not claimed to be.** The keys of one pipeline share no hash tag,
  so the reserve script spans keys a cluster would refuse to co-locate. `docs/plugins.md` §9.4 records that
  as a decision.

**PHP integration is the point of the feature for this organisation.** `sailantis/azera-queue`
already defines `Backend\Contract\ReservableQueueInterface`, a `Worker\Worker`, a
`Dispatch\Dispatcher` and a failed-job provider. A `VolucerQueue` backend implementing that
interface against the call path means an Azera application gets a queue by changing one binding. That
is worth more than seven broker drivers, and it is the reason to do this at all.

**Sizing.** L. Engine + memory driver + protocol messages + pool exchange: the bulk. Each further
driver: M to L, and each is really its own decision.

### 6.5 What jobs and the call path imply for `volucer-observe`

Not a feature, a consequence worth planning for: once there are spans from a queue push and a job
consume, the trace has to continue across the call boundary, and the current observability story
is request-scoped by construction (`volucer-observe`). The call metadata should carry `traceparent`
from the beginning — it costs a field now and is a protocol change later.

**Done, and the field was already there.** `Call::traceparent` and `JobHeader::traceparent` both
exist, and §6.7's exporter reads the same context they are filled from. What step 6 did have to add
was the *parent*: `TraceContext::continuing` keeps the caller's trace and mints our own span id, so
the caller's span id had nowhere to live and an exported span had no `parentSpanId` — a waterfall
that broke at exactly the hop it exists to show. See §6.7.

The remaining piece of this section is **not** built and is not part of step 6: a *job's* span is
exported on the job's `JobHeader::traceparent`, but nothing yet emits a `SpanKind::Producer` span at
the push or a `SpanKind::Consumer` span at the reserve. The types exist (`SpanKind::Producer` and
`SpanKind::Consumer` are in `volucer-observe::span`) so it is a call site rather than a design
question, and it belongs with whichever step next touches the jobs path.

### 6.6 gRPC — three tiers, and the schema is not the server's problem

`docs/plan.md` §6 (*"gRPC — a named non-goal"*) settled the shape and recorded a trigger. This
section answers the question it left open — *static or dynamic `.proto`?* — and the answer is that
**it is a false choice**. The build step belongs to the *application*, and the server can route,
frame and answer without parsing a single `.proto`. The only thing that genuinely needs descriptors
is **reflection**, which is optional and separable.

#### What was already verified in this tree

Five facts, all read from source rather than from documentation, and together they move this from
"XL, gated, probably a rewrite" to something with a small first step:

1. **`h2` 0.4.20 and `http-body` 1.1.0 are already in `Cargo.lock`**, as hyper's own dependencies.
   No protobuf crate is present at all.
2. **hyper 1.12 surfaces trailers on the receive side for both protocols.**
   `hyper-1.12.0/src/body/incoming.rs:176` (the h1 channel) and `:209` (`h2.poll_trailers`) both
   yield `Frame::trailers`, and `Frame::data`/`trailers` are distinct variants, so a
   frame-preserving wrapper cannot leak trailers into the data stream.
3. **A body that preserves frames already exists here.**
   `volucer_upstream::incoming_body` is `BodyExt::map_err(body, …).boxed_unsync()`
   (`crates/volucer-upstream/src/client.rs:248`) — deliberately *not* `collect`, and deliberately
   not `BodyExt::boxed`, because that would require `Sync` and an h2 body is not.
4. **Trailers already survive the proxy path, needing no code.** `translate_response` copies only
   `{status, headers, body, version}` (`crates/volucer-upstream/src/translate.rs:152`) — it never
   inspects a frame — and the front-end hands that same body straight out:
   `outgoing.body(answer.body)`, with the comment *"`UnsyncBoxBody` is the reason this needs no
   conversion: an origin's body is exactly what the response type is now"*
   (`crates/volucer-http/src/listener.rs:1323`). `UpstreamBody` and `ResponseBody` are the same
   type alias. **This one was a reading and is now a measurement** — see below.
5. **A gRPC endpoint is the same listener, not a new one.** `serve_connection` is generic over the
   stream and builds a `hyper_util` auto builder with ALPN-aware protocol detection
   (`crates/volucer-http/src/listener.rs:990`), so HTTP/2 with prior knowledge — which the project
   has already proven by hand — arrives at the *existing* `service_fn`. A gRPC route is
   `/pkg.Service/Method`, which `path_prefix` already expresses
   (`crates/volucer-core/src/config.rs:333`).

#### The trailers claim, measured

Finding 4 was the load-bearing one — `plan.md` §6 says gRPC is gated because trailers are absent,
and the whole "Tier A is small" argument is that they are *not*. So it was tested rather than
argued, in `crates/volucer-upstream/tests/trailers.rs`: a plaintext origin answers with a chunked
response whose trailer section carries `grpc-status: 0` and `grpc-message: OK`, and the test
asserts the fields reach the caller **with the body intact**.

It passes. Three things about the test are worth carrying into the design:

- **The `Trailer` declaration and the fields are asserted separately**, because "the declaration
  was stripped" and "the fields were lost with it" are the two readings that a single assertion
  cannot tell apart. `Trailer` is in `NEVER_FORWARDED`, so it is stripped on purpose, and the
  fields arrive anyway — which is correct, since they travel as frames and not as headers.
- **A control asserts that a response with no trailers reports none.** Without it, a `trailers()`
  that returned `Some` for any reason would pass the main test — hazard 67's shape.
- **The claim is falsified by handing back an empty body**, which fails the trailer assertion
  specifically. So the test can fail, and fails for the reason it exists.

**So the correction to `plan.md` §6 is now measured, not argued.** That section's finding is right
about the *code* — nothing here constructs a trailer, and the three `grep -i trailer` hits really
are a strip list — and wrong about the *effort*: carrying one already works, and what is missing is
the construction side alone.

**And the measurement found something the reading did not.** The connector speaks HTTP/2 **only
over TLS**: `negotiated_h2` is set from the ALPN result and nothing else ever sets it
(`crates/volucer-upstream/src/client/connect.rs:264`, and `TlsIo`'s `Connection` impl at `:338`),
so a plaintext or `unix:` origin is pinned to HTTP/1.1. gRPC's transport is HTTP/2, so tier A needs
either TLS with ALPN or an explicit h2c path. This is a decision for tier A rather than a defect in
the proxy — the h1 path carries the trailers correctly today — but it is invisible until something
needs h2c, which is exactly why it is written down here.

**Both halves are now built.** TLS with ALPN already worked; the h2c path is `protocol = "h2c"` on a
plaintext or `unix:` origin, and §10 item 3 has the decision record.

#### What the server actually has to do, and what needs a schema

The distinction that dissolves the question:

| Step | Needs the schema? |
| --- | --- |
| Read `:path` as `/pkg.Service/Method` | **no** — it is a string |
| Route it (`path_prefix`, or a service table) | **no** |
| Forward `grpc-timeout`, `grpc-encoding`, `grpc-accept-encoding`, `te: trailers` | **no** — headers |
| Pass request DATA frames through as opaque bytes | **no** |
| Pass reply DATA frames back | **no** |
| Write `grpc-status` and `grpc-message` as trailers | **no** — two header names |
| Map `RST_STREAM` and connection failures to a status | **no** |
| Reflection, JSON transcoding, validation, typed handlers | **yes** |

The reason rows four and five are schema-free is the part worth stating plainly: gRPC's inner
framing is **length-delimited, not schema-dependent** — one compression-flag byte, a big-endian
`u32` length, then that many bytes — so a server that never decodes a message still knows exactly
where each one ends. The schema is needed to *interpret* bytes, not to *carry* them.

#### The three tiers

**Tier A — a transparent gRPC pass-through (no protobuf crate at all).**
Route `content-type: application/grpc*` to a Unix-socket origin and relay the call. What it needs
is the **construction** side of trailers — a small `Body` that yields `Frame::trailers` for the two
status headers — which is a shape this codebase already hand-writes at
`crates/volucer-http/src/static_files.rs:806`. That is roughly a forty-line type plus tests.

The one thing tier A must settle first is the **transport**, and this is now measured rather than
open: `crates/volucer-upstream/tests/h2c.rs` shows that **`Connected::negotiated_h2()` is the whole
mechanism**. `client-legacy` reads that answer off the connector's response and dispatches to its
HTTP/2 client instead of its HTTP/1.1 one, and `ConnIo` already forwards `connected()` to whatever
`Io` it wraps — so an h2c path for a plaintext origin is a decision about *when* to answer
`negotiated_h2`, not new machinery. The probe asserts both halves, because only the negative case
makes the positive one mean anything: with the claim, hyper opens with the HTTP/2 connection preface;
without it, the same client sends `POST … HTTP/1.1`. Falsified by making the claim a no-op, which fails
the positive case and leaves the control passing.

What tier A needed, then, was the *policy* rather than a mechanism: a configuration key saying "this
origin speaks h2", and `connect.rs` answering `negotiated_h2` for the plaintext and unix origins so
marked. **Both are now built** — `protocol = "h2c"`, with the vocabulary decision recorded in §10
item 3 — and the *transport* half of tier A is therefore done. TLS with ALPN needed nothing.

This is what makes volucer a gRPC *server* for runtimes that already speak it — Node's
`@grpc/grpc-js`, Go, Python's `grpcio` — **using the same `upstream` backend §7.1 is adding anyway**.
It is the cheapest gRPC capability in this document and it should land first.

What tier A still needs beyond the transport, and what writing it found: **`TE: trailers` was in
`NEVER_FORWARDED`, and gRPC's protocol requires the server to receive it.** It is hop-by-hop by RFC
9110 §7.6.1, so the client's own value must not cross — but the intent must, and the proxy now writes
exactly one spelling of it rather than forwarding whatever arrived, because forwarding a client's
list would let it ask an origin for an encoding this server never negotiated. That is the first
header this proxy *constructs* rather than filters, and it is the reason a gRPC route needed more
than a config key.

`is_grpc` recognises a call by its media type and by nothing else: `application/grpc` with an optional
`+proto`/`+json`/`-web` suffix, followed by the end of the value, `;`, `+` or `-`. The first version was
a bare prefix test and accepted `application/grpcx`, which its own test caught — a media type is a
token rather than a string, and `application/grpcx` is a different type that begins with the same
letters.

**Tier B — the application as the gRPC handler (still no schema on the server).**
`CALL`/`RETURN` (§4.2) carries the method path and the raw message bytes; the application decodes
with whatever it already uses. Needs:

- Tier A's frame-preserving construction side, so the reply's trailers reach the client;
- a **service allow-list in configuration** — `services = ["greeter"]` beside the backend — so a
  route cannot claim an arbitrary path. This is *not* `.proto` loading; it is the same shape as the
  namespace allow-list §4.5 already introduces for KV and locks;
- an application-facing contract in the SDK, e.g.
  `GrpcServiceInterface { handle(string $method, string $bytes): string }`, with the status and
  message supplied separately so a failed call is expressible.

The server never parses a message. Note that RoadRunner needs `protoc-gen-php-grpc` only because
*its* worker wants typed DTOs; the transport does not require them, and neither will this.

**Both halves of tier B are now built**, and three of the design's own premises needed correcting on
contact — each one recorded here rather than quietly fixed:

1. **The allow-list key is not `services`, and it is not a path prefix.** `grpc_services` is a second
   key on the same backend because the two lists answer *opposite* questions: `services` decides what
   the application may **call**, `grpc_services` decides what may be **delivered** to it. One key would
   have meant a worker granted `kv.sessions` also being a gRPC endpoint for every service anyone
   invented. It holds **service names**, not paths and not methods — `"greeter.Greeter"` — because a
   method the application does not implement is one it can answer `UNIMPLEMENTED` to itself, while
   listing methods here would put the application's interface in the server's file and make every
   application deploy a server configuration change. That is §6.6's whole argument, and the key's shape
   is where it has to hold.
2. **The call is not always granted — the *service* is, and the refusal is `UNIMPLEMENTED`.** §10 item 1
   asked whether an unlisted method is `UNIMPLEMENTED` or `NOT_FOUND`. It is `UNIMPLEMENTED`, and not
   only because that is what a gRPC client expects from a non-reflective server: `NOT_FOUND` would
   leak whether a service *exists*, which is the same argument §4.5 makes about `config.describe`.
   What the code added to the question is that the granularity is the **service** and nothing finer —
   see item 3 below for why a path-level rule could not have worked.
3. **A gRPC path is two segments and both have to be protobuf names.** `/pkg.Service/Method`, exactly:
   a third segment is refused, and so is `Method?x=1` — not by a rule about query strings but because
   `?` is not a byte a protobuf identifier may contain. The first version of this had the server and
   the *SDK* disagreeing about that last case, because PHP's `parse_url` strips a query and Rust's
   parser does not; the two now state the same rule and each says so in its own source.
4. **Routing to a `worker` backend had to become protocol-aware, one level above the backend.** The
   listener decided *upstream versus CGI* before it had read the body, because an origin's body must
   not be touched. A gRPC call to a worker has to be refused **before the body is read** as well, since
   buffering a body for a call that will not be dispatched is work an attacker asked for — so the
   allow-list check sits in the same pre-body position as the upstream branch, and the CGI path's
   failures now carry which *kind* they were (a request that never became a CGI request, or a backend
   that was asked and failed), because only the second is a gRPC status.
5. **The application states its outcome in two ordinary response headers, and the server moves them.**
   This is the SDK contract, and it is smaller than the sketch above: not an interface with `handle()`,
   but a `GrpcResponse` value whose status and message become `grpc-status` and `grpc-message` on the
   `RESPONSE` head. The worker protocol's head has nowhere else to put them. What the *client* library
   gets is `Request::grpcMethod()` and `Request::isGrpc()` — reading the path and the media type, which
   is all either side needs.

**A worker that serves both protocols is the ordinary shape, and the SDK had to change for it.** One
pool is one backend, so a `worker` backend reached by an HTTP route *and* by `grpc_services` has one
process answering both. `Worker::serve` therefore identifies a call by `content-type` rather than by
how the process was started, and the SDK grew `onGrpc()` beside `run()` so that both halves can be
registered before either loop begins. A worker that never registered a gRPC handler answers
`UNIMPLEMENTED` rather than crashing — which is also the honest answer for a plain PHP application a
misconfigured client sends `content-type: application/grpc` to.

**What tier B does *not* do, and the one place it does not generalise.** The reply is buffered: the
worker protocol declares `body_len` in the `RESPONSE` head, so a server-streaming method has nowhere to
put its later messages. That is §10 item 4, it is unrelated to gRPC's own streaming (which the transport
would carry fine), and it is a property of the worker protocol rather than of this tier. The other
limit is one-request-in-flight: a gRPC call is *work* for a worker, so tier B reuses the request that is
already in flight and adds no second exchange — which is why it never touched §10 item 9.

**Tier C — reflection (the only descriptor consumer).**
`grpc.reflection.v1.ServerReflection/ServerReflectionInfo` answers with a serialized
`FileDescriptorSet` for the requested symbols, so `grpcurl` and `grpcui` can introspect without
being handed `.proto` files. This is where runtime parsing lives: `protox` compiles `.proto` text to
a `prost_types::FileDescriptorSet` **purely in Rust, with no `protoc` binary**, and the set is cached
in the service bag (§5). Reflection is a small *service*, not a request-path dependency, and it may
legitimately be off.

**It is off, and the reason is stronger than "optional" — see §10 item 2 for the decision and for
the transcoding case that would be the real reason to load descriptors.** The short form: after
tier B, there is nothing left in this server that a schema would let it do. Tier A and tier B carry
framed bytes and write two trailer fields, so the only consumer a descriptor would have is
reflection itself, which buys a `grpcurl` user a `.proto` file they can already bring. Not building
it keeps the graph free of `prost`, `prost-types` and `protox`, which `Cargo.lock` can confirm by
their absence rather than by intention.

#### How RoadRunner's plugin resolves the same three tiers, read from its source

This was checked against `roadrunner-server/grpc` v6.0.0 and `roadrunner-server/protoreg` v6.0.0
directly rather than from the plugin's documentation, because two things in this section are claims
about what a *server* has to do and RR is the only working implementation of the tier-B shape to
compare against. Four facts, each with its own consequence.

**1. RR parses `.proto` files with a syntax parser, and never compiles them.** `parser/parse.go`
walks the file with `emicklei/proto` and builds this struct, reproduced as the file has it:

```go
type Method struct {
	// Name is a method name.
	Name string

	// StreamsRequest defines if the method accepts stream input.
	StreamsRequest bool

	// RequestType defines the message name (from the same package) of method input.
	RequestType string

	// StreamsReturns defines if method streams result.
	StreamsReturns bool

	// ReturnsType defines the message name (from the same package) of the method return value.
	ReturnsType string
}
```

Of those five fields **the plugin reads one**. `RegisterMethod` is called with `m.Name` only
(`server.go:79`) and everything downstream switches on that string; `RequestType` and `ReturnsType`
are populated by the parser, documented in its own comments, and referenced nowhere else in the
repository. So the request type and the response type — the two things a descriptor exists to
describe — are parsed and dropped. That is the strongest available evidence for this section's
central claim: the method *name* is all a server needs, and the message types are dead weight even
when they are lying right there in a struct. RR is schema-aware and still schema-blind on the request
path.

Its *config* is where the rest of the schema surface shows up, and it is worth naming because it
looks like the descriptor feature and is not: `grpc.proto` takes a **list of globs**
(`doublestar.FilepathGlob`, `config.go:81`) and `parser.File` recurses into each file's `import`s
looking for a sibling at `importPath+"/"+i.Filename` (`parse.go:96`) — a naive join that only
resolves imports that sit next to the file that named them. So the parser reaches into imports to
find *more service definitions*, not to resolve message types. It is a service *discovery* mechanism,
and the glob is what makes a multi-file application work without a generated registry.

**2. There is no transcoding anywhere in RR's gRPC plugin.** `grep -ri` over the plugin for
`gateway`, `transcod`, `jsonpb` and `protojson` returns nothing — the same grep returns five hits
for `RegisterService`, so it is finding things; the only `grpc-gateway` in `roadrunner/go.mod` is an
`// indirect` entry the plugin never imports. So the answer to *"does RR transcode?"* is no — it is a
pure gRPC server for the tiers A and B shape, exactly as this section describes volucer's.
Transcoding in RR's ecosystem is a second process the operator adds, and §10 item 11 is the same
conclusion reached here. (It has no descriptor compiler *either*: compiling is `protoreg`'s job, and
the plugin's own `jhump/protoreflect` usage is a single import in `api/interfaces.go:8` — the
`*protoresolve.Registry` *interface type* that protoreg is injected through. Its `go.mod` names
`protoreflect/v2` for that type alone; there is no `protoparse` call anywhere in the plugin.)

**3. RR's server-side health check is a *service*, not a built-in, and that settles a question this
section left implicit.** `plugin.go:171` installs `grpc_health_v1.RegisterHealthServer` against
`health_server.go`, which is a real `Health/Watch/List` implementation that reports the server's own
up/down status and nothing about the application. The application's own health is a *separate*
service in the fixture: `tests/php_test_files/grpc/src/HealthService.php` implements
`HealthInterface` — a `.proto`-generated interface with `Check` and `Watch` — and is registered by
the test worker beside the echo service. **So RR answers the protocol-level health question itself,
because that answer is about the pool, and expects the application to answer the
application-level one through an ordinary service.** That distinction is the one volucer needs when
a health route is ever added: `grpc.health.v1.Health` belongs in `grpc_services` with everything else,
and its reply is a protobuf message the application encodes, which is the one thing this server
declines to do. There is no version of this where the server builds a `HealthCheckResponse`.

**4. RR bypasses protobuf encoding in the *server* with a codec, and the application decodes anyway.**
`codec/codec.go` registers itself under the name `"proto"` — displacing protobuf's own codec — and
its `Marshal`/`Unmarshal` short-circuit on a `RawMessage` type alias for `[]byte`. The `invoke` body
in `proxy/proxy.go` then decodes the request into `&codec.RawMessage{}`, hands those raw bytes to the
worker, and returns `codec.RawMessage(r.Body)` as the response. The server never builds or reads a
message. So far it is the same design as tier B.

**The half that differs is on the other side of the worker protocol, and it is the answer to the
original question.** `spiral/roadrunner-grpc`'s `Invoker` — the application-side SDK, which the
fixture requires as `"spiral/roadrunner-grpc": "^3.0"` — does
`$in->mergeFromString($body)` and `$message->serializeToString()`, against a class from
`$method->getInputType()`. That type comes from a **generated PHP service interface**, produced by
this repository's own `protoc_plugins/protoc-gen-php-grpc`. **So RR answered "static or dynamic
`.proto`" by taking both, in separate places: codegen in the application, and a runtime syntax
parser used only to build the routing table and the reflection registry.** The dynamic parse never
touches a message; the typed decode never touches the server. That is the split §6.6 argues for,
implemented, and it means the `RequestType`/`ReturnsType` fields above are dead *in the server*
precisely because the codegen already put the same information into the worker.

Two consequences worth carrying:

- **The build step is real and required.** An RR gRPC application cannot start without
  `protoc-gen-php-grpc` having generated its interfaces, which is why the plugin ships the generator.
  volucer's tier B needs nothing generated, because the SDK's contract is two response headers and
  the payload is opaque — a weaker coupling, bought by the application writing its own framing.
- **The framing differs, and this settles it as a deliberate choice rather than an oversight.** RR's
  worker receives the **message alone** — gRPC's length prefix is consumed by the framework, which
  re-frames on the way out — so `mergeFromString` can be called on the bytes directly. volucer hands
  the worker the prefix along with the message: the tier-B test posts `\x00\x00\x00\x00\x05hello` and
  asserts those bytes come back (`crates/volucer-http/tests/frontend.rs`), and
  `GrpcResponse::message()`'s "a compression flag, a big-endian length, then the message" is the SDK
  saying so. Both are schema-free; the trade is that volucer's SDK author writes five bytes they
  could otherwise ignore, in exchange for a server that has no framing opinion at all.

**The one thing RR has that this section deliberately gives up is reflection.** `protoreg` compiles
the protos properly (`jhump/protoreflect`'s `protoparse` → `protoresolve.Registry`), the gRPC plugin
consumes it through an `api.Registry` dependency (`plugin.go:307`), and `registerReflection` feeds it
to `reflection.ServerOptions.DescriptorResolver` so `grpcurl` gets real file descriptors. Its own
comment concedes the fallback: without `protoreg`, "the global protobuf registry … only exposes
service/method names for the proxies". That is the cost of §10 item 2's decision, stated by the
implementation that pays it, and it is the shape to copy if the decision is ever reversed.

#### The decision, restated as two independent ones

Replace *"static or dynamic `.proto`"* with:

1. **Message encoding: always server-opaque.** The server carries bytes and writes two header
   names. This is not a trade-off; there is nothing to weigh, and the `prost`/`tonic` codegen
   pipeline is never on the critical path.
2. **Service discovery: reflection, optional, runtime-parsed.** `protox` → `prost-types` →
   `ServerReflection`, for tooling only. Turning it off costs a caller the ability to introspect,
   which is exactly what happens against any non-reflective server today.

#### What remains genuinely hard

- **A gRPC call as *work* for a worker is the other direction.** `CALL`/`RETURN` is server→worker,
  for the application to reach the server, and preserves one request in flight. A gRPC call is a
  second unit of *work* delivered to a worker, which is either a second exchange or true
  multiplexing — the one thing here that does contradict the one-request-in-flight invariant.
  §10 keeps it open. **Tiers A and B do not need it**: A never involves a worker, and B reuses the
  in-flight request that is already there.
- **Framing code gets its own step with its own tests**, and nginx is the evidence — HTTP/2 to an
  origin arrived only in 1.29.4 and has since had two memory-safety CVEs in that path, one of them
  specifically about a crafted request proxied to an HTTP/2 **or gRPC** backend. The mitigating
  fact worth noting: this server does not *construct* frames, hyper does, so the exposure is the
  construction side of trailers and the status mapping rather than a hand-written h2 state machine.

**Sizing.** Tier A: **S–M plus one transport decision** — the trailer construction is small and
measured, but an h2c path for a plaintext gRPC origin does not exist and is the real cost (§10
item 3). It was that, and the estimate held. Tier B: **M**, and it was a little more than M: the
config key and the SDK contract were each small, but *routing to a worker became protocol-aware*, and
that meant a new error shape on the CGI path plus the SDK's two-protocol request path — the kind of
change that is only small once it is understood. Tier C: **M** (`protox` + a reflection service), and
**declined** — §10 item 2 records why, and the reason is stronger than "optional": nothing in this
server has a use for the schema other than the service that would exist to hand it out. The
worker-as-work question: **L** on its own, and no longer on the critical path for either A or B.

### 6.7 OTLP export — **done**, and two of this section's own premises were false

Not on the original list, and adjacent to `metrics` in a way that invites confusion, so it is named
so it can be excluded on purpose. `docs/plan.md` §6 decided the shape: an exporter on top of the
existing request ids and trace context, over **OTLP/HTTP on 4318**, which is what RoadRunner
defaults to. It lands **after** metrics, because the metric registry is what makes the service bag
and the listener pattern exist, and **before** gRPC, because gRPC spans are the reason anyone asks
for it.

#### What the plan said, and what measurement changed

Two claims in the paragraph above were checked before anything was built, and **both were wrong**.
Recording them because each one moved the design:

1. **"avoids `tonic` and `prost` entirely" — half true.** Measured from `opentelemetry-otlp`
   0.33.1's own manifest: `http-proto = ["prost", "opentelemetry-http",
   "opentelemetry-proto/gen-tonic-messages", "http", …]`. OTLP/HTTP avoids **`tonic`** — no gRPC, no
   HTTP/2 framing of messages — but its body is a protobuf message, so `prost` is required. No
   feature of that crate serializes traces without it; `http-json` names `prost` too, because it
   round-trips through the generated types.
2. **"an exporter *layer*" — the layer cannot carry the id.** `SpanBuilder` has no `with_span_id`
   and `SdkTracer::build_with_context` always calls `config.id_generator.new_span_id()`, so a span id
   can only be *inherited* from a parent context. But `volucer-observe` mints the span id it puts on
   the request a backend receives — PHP's child span names **that** id as its parent. A layer-built
   span would export a different one, and the trace would break at the very hop it exists to show.

   The fix that makes the ids agree is to drive the encoder directly from spans volucer already
   has, rather than to make the layer agree with them.

#### What was built

**`volucer-otlp`**, a new crate, and **`volucer-observe::span`**, and the hand-written encoder falls
out of correction 1 the same way the Prometheus encoder and the access log already did: the format
is a page of specification, and a hand-written encoder is the only way to guarantee it *cannot
fail*. Measured cost of the alternative, from a scratch project resolving the real crates:

| Transport | New packages for this workspace |
| --- | --- |
| `opentelemetry-otlp` with `reqwest-blocking-client` | **41** |
| `opentelemetry-otlp` with `hyper-client` | **22** |
| hand-written OTLP/JSON over the existing client | **0** |

Zero, not "few": `Cargo.lock` went from 253 packages to 254, and the one addition is `volucer-otlp`
itself. The transport is `volucer-upstream`'s client, so the exporter also inherits TLS against a
named CA or the public roots, pooling, and one connector for both.

#### Findings worth recording

- **`RequestContext` discarded the parent span id.** `TraceContext::continuing` deliberately replaces
  the span id with this hop's own — correct for the header it sends, but it meant the caller's span
  id had nowhere to live. OTLP needs it as `parentSpanId`, and *absent* rather than all-zero for a
  root span (the specification calls an all-zero one invalid). So a `parent_span_id` field was added,
  and an untraced request deliberately does **not** get a synthetic parent: a trace this server
  starts is not a continuation of anything.
- **`Observability::is_enabled` was the wrong guard, and it would have exported nothing.** It reports
  the *access log*. Tracing and access logging are separately configured, so a front-end that skipped
  its record on `is_enabled` would export no spans at all in a deployment with `[telemetry]` and no
  `[server.access_log]` — which is the ordinary way to try tracing first. `is_active` is the guard
  the front-ends now use; the access log keeps its own.
- **`Exporter::stop` takes `&self`, not `&mut self`, and that is forced.** The front-ends hold an
  `Arc<dyn SpanRecorder>` pointing at the exporter for the whole run, so it is never uniquely owned
  while it is running — a `&mut self` method could only be called by someone who had already given
  the recorder away, which is everybody. A `Mutex<Option<JoinHandle>>` is the same shape the
  listener's shutdown already uses. (The first version had `try_unwrap` here, which can never
  succeed for exactly this reason.)
- **The first encoder emitted invalid JSON — four resource attributes with no separators between
  them.** The unit that found it was the one that parses the *whole* document rather than inspecting
  a field, which is now the rule every test in that module follows. Verified independently by piping
  the encoder's own output through Python's `json` module.
- **OTLP/JSON's three traps**, each asserted rather than trusted: `bytes` fields are **lowercase hex,
  not base64**, for `traceId`/`spanId`; 64-bit integers are **quoted strings**, because JSON numbers
  are doubles and `1750000000000000000` has no nanosecond precision unquoted; and enums are integers
  with camelCase field names.
- **A `SystemTime` was needed beside the existing `Instant`.** A span's timestamp must be UNIX epoch
  nanoseconds, which an `Instant` has no defined relation to, while a *duration* must come from a
  monotonic clock because wall-clock time jumps when NTP corrects it. Both are taken in `begin`, in
  the one call that says "this request has begun", so they cannot disagree.
- **The FastCGI span's end time is derived from the elapsed duration**, not read again — which keeps
  the exported span's duration and the access log's `request_time` the *same number* rather than two
  readings of a clock that can step between them.
- **A bad endpoint is reported, not fatal.** A server that refused to start because its telemetry
  collector was misconfigured would invert the priority the access log already states: a deployment
  whose log directory is missing logs that fact and serves anyway.
- **A batch is dropped, never retried**, and the queue **drops rather than blocks** when full. The
  alternative for either is converting a telemetry outage into an application outage, which is the
  one outcome this project refuses everywhere else. What is not hidden: the first failure of an
  outage warns, the recovery logs, and the shutdown summary prints exported/failed/dropped.

**Falsified, and it was the important one.** Replacing `span_id: *context.trace.span_id()` with a
freshly minted id — exactly what a `tracing-opentelemetry` layer does — fails the end-to-end test
with `left: "0707070707070707", right: "92f318462a646683"` and the message naming why. The test does
not merely agree with the code; it detects the defect the section is about.

### 6.8 Whose address is the client's — **done**, and it was a defect rather than a feature

The one row of §1.4's first table that was **something wrong** rather than something missing. Behind
nginx every connection arrives from nginx, so two readers of the peer address were quietly wrong in a
way that looks like correct behaviour:

- the access log's first field recorded the **proxy**, so "who requested this" had no answer in the
  one place an operator looks for it;
- anything keyed on the address — a rate limiter, an IP allow-list, an abuse heuristic — keyed on the
  proxy, which means it limits *nginx* and cannot tell two clients apart at all.

Neither is visible from the client's side, and a single-machine test passes, so this is the kind of
defect that ships. It is also why it went first: the other three rows add a feature nobody is missing
yet, and this one was wrong in every deployment that already has a proxy.

#### The trust list is the feature, and the headers are the easy part

`X-Forwarded-For` is a **request header**, so any client can send one. A design that reads it is not
resolving an address, it is accepting a claim. So the rule is not "read the header" but "read the
header *only if the peer is a proxy this deployment named*":

1. The peer's address is inside a configured `trusted_subnets` entry → the first header in
   `trusted_headers` that yields a parsable address is the client.
2. Otherwise → **no header is read at all**, and the peer is the answer. An untrusted claim is not a
   weaker answer, it is no answer.

Which is why **the empty list is the default and means the feature is off**: an existing deployment
is not changed by upgrading, and turning it on is a statement that a specific proxy is in front. The
names are RoadRunner's (`http.trusted_subnets`, `http.trusted_headers`) because an operator copying
an nginx snippet already has that vocabulary.

Two decisions inside that are worth more than the rest:

- **A malformed range stops the server.** Every other list in this configuration drops what it cannot
  read. A trust list cannot: a range that fails to parse is a proxy whose headers are suddenly
  disbelieved, so a rate limit keyed on the address throttles every user at once and *nothing in the
  logs says why*. `10.0.0.0/8` with a `/99` is refused at startup, naming the entry.
- **Host bits past the prefix are masked rather than kept.** `10.1.2.3/8` means `10.0.0.0/8`, which is
  what the `/8` asked for. The other direction — trusting `10.1.2.3` while the file appears to say
  `/8` — is the failure that would actually be dangerous, so the test pins the direction.

#### What was deliberately *not* wired, and the check that decided it

The FastCGI front-end **does not consult this table**, and it had a comment saying so before the
feature existed ("`REMOTE_ADDR` here is nginx\`s address … writing the header nginx put in
`X-Forwarded-For` instead would make this log assert a peer it never saw"). That comment turned out to
describe the right *outcome* for a partly wrong *reason*: this listener logs the param, and the param
is not the socket peer.

Reading nginx's own `conf/fastcgi_params` settled it. nginx sets

    fastcgi_param  REMOTE_ADDR        $remote_addr;

— the address **nginx** saw the client at, already resolved by nginx's own `real_ip` handling — while
the `HTTP_*` params beside it are the client's claims relayed verbatim. So `REMOTE_ADDR` is already
the client's address, and applying the policy here would replace a value the *proxy* established with
one the *client* merely asserted. The two front-ends look symmetric and are not; the asymmetry is that
one line.

That is a better outcome than a reversal would have been, and it is worth naming because the
symmetric-looking change was the obvious one to make. The unaddressed case is now stated in the doc
comment instead: a listener behind something that forwards raw FastCGI without setting `REMOTE_ADDR`
would need the *socket* peer trusted, which is a different check.

#### Where it lives, and why it is restart-only

The policy is a field on `ReloadHandles` beside `observe` — the struct that already means "what a
front-end must be handed that a reload may also move" — so both front-ends read one instance and the
request path resolves an address **once**, in the same expression that feeds both `REMOTE_ADDR` and
the log record. Resolving them separately is exactly how the two come to disagree, which would be a
log that contradicts the application.

It is **restart-only**, and `restart_only()` names it on every reload. The alternative was to put it
on `Reloaded`, whose fields are per-site maps that `Reloaded::from_sites` derives and which
`volucer-watch` owns. A `vec![Cidr]` per request does not belong in a per-site map, and worse: a trust
list that silently did not change on `SIGHUP` while the operator believes it did is a security bug
with a slow fuse. A statement about the deployment's topology is the same kind of setting as `alpn`,
which is already restart-only for a comparable reason.

#### Verified

- **The trust check is falsifiable in both directions, and was falsified in both.** Making `resolve`
  return the peer unconditionally fails the *trusted* half; removing the `trusts` guard so every
  header is believed fails the *untrusted* half with
  `x-saw-remote: 203.0.113.9` where `127.0.0.1` belongs. A test that only checked the first half would
  pass against a server with a spoofing hole.
- **End to end through `serve`, against a real `php-cgi`**, because the assertion that matters is what
  `$_SERVER['REMOTE_ADDR']` holds rather than what the server believes it sent. The two runs differ in
  one line of configuration and use the **same socket peer**, so the decision is demonstrably the
  configuration's rather than the connection's.
- **The upstream half is asserted on the appended field, not on the client's own claim.** The request
  carries `X-Real-Ip` (which the policy is told to believe) *and* `X-Forwarded-For` (which the append
  rule relays verbatim); the assertion is that the resolved `198.51.100.9` appears in the outgoing
  chain. Checking only for `X-Forwarded-For: 203.0.113.11` would pass with the policy switched off,
  which is a test that confirms the append rule and calls it the feature.
- 13 unit tests over the parser and the policy, including the ranges that would panic a naive mask
  (`/0` and `/32` shift by a full width), the four spellings a forwarded address arrives in, and the
  RFC 7239 `for=` target — with `by=` as the control that it is not mistaken for it.

### 6.9 The request body limit: it was a defect, not a setting

**Built, and it is the second estimate in this document that was wrong in the flattering
direction.** §1.4's row said this was "one `HttpLimits` field read beside the CLI's
`--max-connections`", which is true of the HTTP front-end and **entirely false of the FastCGI
one**:

- `volucer-server`'s `read_next` did `body.extend_from_slice(&record.content)` for as long as
  `STDIN` records arrived. There was no bound at all. Behind nginx the only limit was nginx's
  `client_max_body_size`, which is somebody else's configuration file — and a deployment that
  forwarded to volucer from a load balancer, or from a proxy that did not set one, had nothing.
- So the slice was not "read a field"; it was "give the second front-end a bound it never had",
  and the config key is what made the two front-ends agree rather than the *point* of the slice.

Four decisions, recorded because each could reasonably have gone the other way:

1. **The unit is in the name** — `max_body_bytes`, not RoadRunner's `max_request_size` (megabytes).
   A key with the same name and a different unit is the one kind of compatibility that costs more
   than it buys, and `deny_unknown_fields` turns a deployment that copies RR's spelling into an
   error rather than a server that reads as configured and keeps the default. `memory_limit_mb` and
   `static_files.max_multirange_bytes` already establish both conventions.
2. **Zero is refused, not read as "unlimited".** RR's own documentation says "zero means no limit";
   reading a zero as unlimited is the one interpretation that turns a typo into an unbounded
   allocation. The message names the 32 MiB default, because the useful information is what you
   get by deleting the line.
3. **The check runs before the extend, not after.** Checking afterwards lets one record over the
   limit be buffered and then refused, so the memory a client can make the process hold is
   `max_body_bytes` **plus a record** rather than `max_body_bytes`. Same rule as the pre-creation
   counter check in `volucer-metrics`: a limit enforced after the work it exists to prevent is not
   a limit.
4. **`413`, and the status line had to move.** `write_error` hardcoded `Status: 502 Bad Gateway`
   for every `ServerErrorKind`, so an oversized body would have told the client the *gateway*
   failed. The kind now carries its own status line. The `END_REQUEST` protocol status stays
   `RequestComplete` deliberately: an oversized body is oversized at every worker, so `Overloaded`
   would invite nginx to retry elsewhere for ever.

**The upstream path is deliberately still unbounded**, and now says so beside a key that reads as
though it bounds everything. An `upstream` backend is handed the body unread, so applying a limit
there would be a reverse proxy imposing a cap its own client never set. The README states it in
both places.

**And the fixture was the bug, twice.** Both new refusal tests first asserted against a body that
ended at its first chunk: the `stdin` helper appends its own terminator, and `write_records` takes
a `terminate` flag that the end-to-end test passed as `true` per chunk. The limit then looked like
it worked because it was never reached. Two lessons, both already in this document's list: a test
fixture that builds the input for the assertion can hide the assertion, and the fix is to check
that the test *fails* when the code under test is broken — which it now does, verified by ignoring
the configured limit and watching the 413 become a 502.

### 6.10 Rate limiting: a refusal is a statement about the caller

**Built, and the first row of §1.4 whose estimate held.** "The token bucket is a page of arithmetic"
was true; what the estimate did not see is that the arithmetic is the *small* half. The work was
deciding what a refusal means when there are two front-ends, two protocols, and a table that has to
say what happens when it is full.

#### Six decisions, each of which could reasonably have gone the other way

1. **`429`, not `503`.** RoadRunner answers a full key table with `503 Service Unavailable`. That is
   the one status a load balancer *acts* on: it takes the node out of rotation. One client that
   varies a header would then get the deployment removed from its pool, which is an outage caused by
   the thing the limit exists to survive. `429 Too Many Requests` is a statement about the caller and
   the process stays in rotation. A new variant, `HttpError::RateLimited`, exists for exactly this and
   a test pins that it never shares a status with `Overloaded`.
2. **`Retry-After`, always, in whole seconds, floored at one.** Its absence is what makes a client
   retry immediately against a bucket that is still empty. The number comes from the limiter's own
   arithmetic so the promise and the bucket cannot disagree.
3. **The `burst` default is `rate`, not `1`.** RoadRunner defaults it to one, which turns
   `rate: 60, interval: 1m` into "one request, then one per second" — a limit nobody writing that
   line meant. Defaulting to `rate` is the ordinary reading of "60 a minute".
4. **Refilling is continuous, not windowed.** nginx's `limit_req` is a fixed-window counter, and a
   client can spend one window at `t = interval − 1ms` and the next at `t = interval + 1ms`, doubling
   the instantaneous rate. A bucket has no boundary to exploit: it cannot hold more than `burst`.
5. **A full table evicts only what costs nothing to lose.** A bucket that has refilled to `burst` is
   indistinguishable from an absent one, so reclaiming it is lossless. A *depleted* bucket is not:
   evicting it hands that caller a fresh allowance, which is exactly what the limit exists to refuse.
   So a full table of depleted buckets refuses the newcomer, and the sweep runs when the table is
   full rather than on RoadRunner's one-minute timer — the only moment it can change an answer.
6. **Header keys are hashed, not stored.** A header value is attacker-chosen and unbounded; storing it
   verbatim would let an unauthenticated client make the process hold an arbitrary amount per bucket.
   `std`'s `RandomState` is a seeded SipHash, so it is also not precomputable by a client trying to
   fill the table deliberately — and it adds no dependency, which SHA-256 would.

#### The two front-ends, and why the identity is not the same on both

One `Arc<RateLimitPolicy>` reaches both, exactly as one `Arc<ClientIpPolicy>` does, because two
policies would give a client twice the configured allowance depending on which protocol received its
request — invisible to any test that exercises one front-end. The **identity**, though, is resolved
differently on each, and §6.8 already established why:

- **HTTP** keys on the address `[server.proxy]` resolved.
- **FastCGI** keys on `REMOTE_ADDR`, which is the param nginx sets from `$remote_addr` — the client,
  already resolved by nginx's own `real_ip`. Applying the trust list here would replace a value the
  *proxy* established with one the client merely asserted, which is the same asymmetry §6.8 found.

A request with neither (no `REMOTE_ADDR` at all) gets **no bucket** rather than a shared one:
bucketing every such request together would make the limit a global one that reads as per-caller.

#### Where the check runs, and the test that proves it

In `prepare`, before the redirect, before the site table, before the static layer's `stat`, and
before the body is read. The purpose of the feature is to stop a caller spending the server's work,
so a check that ran after some of that work would limit only the remainder.

That is not assertable by the status alone, so the test changes the world: the burst is spent on a
real file, the file is **deleted**, and the next request must still be a `429`. A limiter behind the
static layer answers `502` there, because the path fell through to a deliberately unreachable front
controller. Verified by moving the check and watching it fail with `left: 502`.

#### gRPC

A limited gRPC call is `RESOURCE_EXHAUSTED` in the trailers, not a `429`. A gRPC client reads its
outcome from the trailer section and reports "unexpected HTTP status" for anything else, so the retry
the caller should perform would never happen. `GrpcStatus` gained a fourth variant for this, and the
branch is decided **in `prepare`** rather than in `error_response` because that is the last point at
which the request's headers still exist — `error_response` sees only the error.

#### What is deliberately not here

Route-level and per-site limits. A bucket's identity is (who, *which limit*), so two policies over one
client are two buckets and a route table would need the policy's name in the key. RoadRunner has the
same restriction. **No timer and no background thread** either: the sweep runs when the table is full.

`[server.rate_limit]` is **restart-only**, and `restart_only()` names it. The buckets are live state,
so replacing the policy on a `SIGHUP` would reset every caller's allowance — a limit escapable by
asking the operator to reload.

#### Verified

- 12 unit tests over the bucket: the burst boundary, a token returning exactly on time and not a
  millisecond early, refilling capped at `burst` rather than banked, two clients not sharing, a
  v4-mapped address sharing its v4 bucket, both halves of the sweep, a blank header value refused
  rather than bucketed, and a `Retry-After` that is never zero or fractional.
- **Both front-ends end to end.** HTTP: a `429` with `Retry-After: 3600` and `no-store` over a real
  socket. FastCGI: the same over a real FastCGI connection, on **one** connection with keep-alive, so
  "the refusal does not close it" is asserted rather than assumed — plus a second address on that same
  connection that is *not* throttled, which is what makes the key mode real.
- gRPC: a limited call carries `grpc-status: 8` and no `429`.
- Falsified twice: moving the check behind the static layer, and removing it from the FastCGI path.
  Both times the relevant test failed with the answer the request became instead.
---

## 7. Python/Django and Ruby/Rails

Two tiers. The first is almost free and gets the runtimes running this month; the second is the
native answer and needs protocol work. Recommending both is not a hedge — they answer different
questions, and Tier 1 is what lets Tier 2 be judged on evidence.

### 7.1 Tier 1 — supervise their own server, which already exists

**Done.** `UpstreamConfig::command` and `crates/volucer-server/src/upstream_pool.rs`; the
per-runtime guidance below is what the plan said, and §8 records what the code corrected.

Every Python and Ruby application server already implements "boot once, serve many" and already
speaks HTTP over a **Unix socket**:

- Python: gunicorn (`--preload`), uvicorn, hypercorn, granian — all support
  `--bind unix:/run/app.sock`.
- Ruby: Puma (`preload_app!`), Falcon (`preload.rb`), Pitchfork (reforking), Unicorn
  (`preload_app true`) — all bind a Unix socket, and all call `app.call(env)` per request.
- Django's boot-once handle is `get_wsgi_application()` / `get_asgi_application()`; Rails' is
  `Rails.application`, a Rack app whose `call(env)` returns `[status, headers, body]`.

volucer **already** reaches an HTTP origin, including over a Unix socket —
`kind = "upstream"`, `origin = "unix:/run/node.sock"`, documented in the README's *Backends*
section. So the runtime request is not "add a protocol"; it is **"supervise the origin"**:

```toml
[backends.django]
kind    = "upstream"
origin  = "unix:/run/app.sock"
command = 'gunicorn myapp.wsgi --bind unix:/run/app.sock --workers 4'
working_dir = '/srv/app'
# readiness probe: volucer waits for the socket to answer before serving
ready_timeout_ms = 30000
```

That is a new field on an existing backend rather than a new backend, and it is worth doing even
if Tier 2 lands, because it is also how a Node, Go or Rust origin gets supervised. What it needs:

- `command` + `env` + `working_dir` + restart policy on `UpstreamConfig`, and a readiness gate so
  the first request does not 502 while Django is still migrating;
- the same lifecycle guard the worker pool uses (job object on Windows, process group on Unix) so
  an orphaned gunicorn does not survive volucer;
- `RequestScope::OriginScoped` already describes this correctly and `check` already prints it
  (`crates/volucer-core/src/config.rs:1373`) — no change needed, which is a nice confirmation that
  the enum was right.

**Cost: S–M per runtime, and mostly in the supervisor, not the runtime.** And it is honest: HTTP
framing is a real cost, and it is the cost every Rails and Django deployment already pays.

**Built, and four of the plan's own details changed in the writing — see §8 for the full list.**
The two worth stating here: the key is `ready_timeout_ms` (milliseconds, matching every other
timeout in this configuration) rather than the `ready_timeout = "30s"` above, and the restart policy
defaults to `on_failure` rather than to "always", because a `gunicorn` that exited **0** was stopped
by somebody and starting it again would fight them.

### 7.2 Tier 2 — a native worker protocol for another language

This is the interesting one, and the good news is that **the protocol is already almost
language-neutral.** The frame, the CRC-32C, the message numbers, the body chunking and the
handshake have nothing PHP-specific in them; what is PHP-specific is *metadata and adapter work*:

| Where | PHP-specific today | What it should become |
| --- | --- | --- |
| `HELLO_ACK` | `php_version`, `sapi`, and `runtime` carrying PHP bootstrap text | `runtime` (`"php"`/`"python"`/`"ruby"`), `runtime_version`, and optionally `framework`; keep `php_version` deprecated for one version |
| `RequestHeader` | `script_filename` is a CGI notion meaning "the absolute script routing chose" | rename to `entrypoint`, documented as "routing metadata, not a file to execute" — Django and Rails must not treat it as a path |
| `RequestEnd` | `peak_memory_bytes` from `memory_get_peak_usage(true)` | optional; `0` means "this runtime does not report it", and the pool must not recycle on a value it did not get |
| `RESPONSE` | `body_len` declared before the body, so responses are **buffered** | acceptable for v1; a chunked/unknown-length response is a protocol change and is listed in §10 |
| per-request reset | the adapter clears `$_COOKIE`, sessions, request scope | runtime-specific by nature: Django needs `close_old_connections()` and transaction cleanup; Rails needs `Rails.application.executor.wrap` (which also returns ActiveRecord connections to the pool) |
| stdout | is the protocol channel, so the SDK captures `echo` | Python and Ruby must do the same: any print/puts to stdout corrupts framing. This is the single highest-risk adapter mistake and belongs in both SDKs' READMEs. |

**The two concrete protocol changes are additive and backward-compatible** — an optional JSON field
with a `serde` default, which a v1 SDK ignores. `docs/worker-protocol.md` should say so explicitly,
because "the field names are part of the protocol" is otherwise read as "no field may be added",
and the distinction between *adding an optional field* and *renaming one* is exactly what a
versioning note is for.

**What each SDK must implement**, mirroring `sailantis/volucer-php-sdk`'s split
(`Worker`/`Request`/`Response`/`Frame`/`Header`/`MessageType`): all **fourteen** message types
(`HELLO`, `HELLO_ACK`, `REQUEST`, `RESPONSE`, `BODY`, `EVENT`, `LOG`, `METRIC`, `CONTROL`,
`CONTROL_ACK`, `END`, `PING`, `PONG`, `ERROR`), CRC-32C and the `no_checksum` negotiation,
body-chunk length enforcement with `LAST`, the "absorb unsolicited frames without derailing the
exchange" rule, and `VOLUCER_WORKER_SOCKET` support so Windows gets the socket transport.

**Then the adapters, which are the real work:**

- **Python**: `to_wsgi_environ()` building `REQUEST_METHOD`, `SCRIPT_NAME`, `PATH_INFO`,
  `QUERY_STRING`, `CONTENT_TYPE`, `CONTENT_LENGTH`, `SERVER_NAME`, `SERVER_PORT`,
  `SERVER_PROTOCOL`, `wsgi.url_scheme` (from the `https` flag, since the child has no socket),
  `wsgi.input` as a real file-like object, and `wsgi.multithread`/`multiprocess`/`run_once`.
  Sync WSGI first — one thread, one request in flight, which matches the protocol exactly. **ASGI
  is not a small extension**: concurrency inside one worker needs multiplexed requests on one
  connection, which is a protocol change (§10), so do not promise it in the first release.
- **Ruby**: `to_rack_env()` must produce a `Rack::Lint`-compliant env, including `rack.input`
  responding to `gets`/`each`/`read`/`rewind` and a body responding to `each`; and every dispatch
  must be wrapped in `Rails.application.executor.wrap` (or the `Reloader` in development, so code
  reload keeps working) or the connection pool is exhausted.

**Precedent, honestly stated.** Nobody has done this for volucer's protocol, and the nearest thing
is negative evidence: `pygoridge` is a Python Goridge client that exists and is **archived**, and
there is **no Ruby Goridge client at all** after six years of RoadRunner. That is a warning about
demand, and it is why Tier 1 comes first: it makes the runtimes work without depending on anyone
adopting a bespoke SDK. The counter-argument for Tier 2 is real though — it removes HTTP framing
from the hot path, and it is what Granian proves is viable for Python (a Rust server that defines
its own interface rather than making Python parse HTTP), the difference being that Granian embeds
CPython via FFI while volucer's whole design is to spawn it.

**Sizing.** Protocol change: S. Each SDK: M. Each adapter: M, and the Django and Rails ones are
where the framework knowledge is.

### 7.3 What not to do

- **Not FastCGI for Python or Ruby.** FastCGI exists here and would need no new code, but the
  Python bridges (`flup`) are unmaintained and Ruby's are legacy. Recommending a dead bridge
  because it is free is how a plan becomes technical debt.
- **Not an embedded interpreter.** `docs/plan.md` §1 rejected embedding PHP for reasons that apply
  verbatim to CPython and MRI (no C-level SAPI, lost isolation, a rebuild per version), and hazard
  53 measured the spawn path against the embedded one. The answer for Python and Ruby is the same
  as it was for PHP.
- **Not a protocol per language.** One protocol, three SDKs, and the §5 feature flag deciding what
  is compiled. A `python` backend kind would be a mistake for exactly the reason
  `RequestScope::OriginScoped` gives: volucer does not reach into somebody else's runtime.

---

## 8. Order, and why

Each step is a prerequisite of the next thing that needs it, and nothing later is started to make
progress on something earlier look bigger.

| # | Step | Depends on | Size | Why here |
| --- | --- | --- | --- | --- |
| 1 | **Metrics + status** | — | S–M | **Done**, and the `[metrics.collect]` half landed last (see §6.1): without it every `emitMetric` was refused as undeclared. No calls, no driver, no protocol change; makes everything after it observable |
| 2 | **`CALL`/`RETURN` on the worker channel + `workers.list` / `worker.reset`** | 1 | M | **Done.** The branch on inbound frames (§4.2), the method registry, and the per-backend `services` allow-list. A control plane exists from the first commit that installs a handler |
| 3 | **KV: `memory`, then `redis`** | 2 | S, M | **Done.** Smallest service on the call path; proves the registry shape. `redis` is behind a feature and shares its client with step 4 |
| 4 | **Locks: `memory`, then `redis`** | 2, 3 (Redis client) | S, M | **Done.** Shares the Redis client; carries the fencing warning |
| 5 | **Jobs: engine + `memory` + `redis`** | 2 | L | **Done.** Biggest item; the consumer-side exchange is the interesting part |
| 6 | **OTLP export** | 1, 5 (spans across a push) | M | **Done.** Not the layer the plan named: measured, `http-proto` needs `prost`, and a layer cannot carry the span id PHP already has. Hand-written OTLP/JSON over the existing client — **0 new packages** |
| 7 | **gRPC Tier A** (trailers constructed, pass-through to a Unix-socket origin) | §7.1's `upstream` supervision | S–M | **Done.** `protocol = "h2c"` for plaintext and socket origins, `TE: trailers` constructed rather than forwarded, and a failure reported as a `200` with `grpc-status` in the trailers. No protobuf crate, no worker, no new listener |
| 8 | **gRPC Tier B** (application as the handler, via `CALL`/`RETURN`) | 2, 7 | M | **Done.** `grpc_services` on a backend, `UNIMPLEMENTED` for anything else, and the application's status moved into the trailers. The application states its outcome in two response headers; the server never parses a message. Five of this section's own premises were corrected on contact, and the largest is that routing to a *worker* became protocol-aware |
| 9 | **gRPC Tier C** (reflection) | 8, and §10 item 2 | M, optional | **Settled: not built**, and §10 item 2 is the decision. The only descriptor consumer turned out to be the service itself — a reply is already-framed bytes the server never decodes — so no other use could justify loading a schema either |
| — | **`http.trusted_subnets` + forwarded headers** — §6.8 | — | S–M | **Done.** Independent of 1–9, and the only item in §1.4 that was a *defect* rather than a missing feature: behind nginx the access log and any future rate limit both keyed on the proxy. Restart-only, and deliberately not applied on the FastCGI path |
| — | **Rate limiting** — §6.10 | 1, §6.8's resolved address | S | **Done.** The bucket is a page of arithmetic as §1.4 said; the work was the two front-ends and two protocols. 429 (not RoadRunner's 503, which would remove the node from a load balancer), Retry-After, and RESOURCE_EXHAUSTED for a gRPC call. Restart-only, because the buckets are live state |
| — | **`volucer workers` / `volucer reset`** — §4.7 | 1, 2, and §6.1's `[status]` listener | S–M | **Not built, and it is the operator's only lever on a running pool.** §4.6 called it a client; it is an endpoint, because the registry's channel has no second-process half. Routes on the listener `[status]` already brings up, behind an off-by-default key |
| — | **Runtimes, Tier 1 (`upstream` supervision)** | — | S–M | **Done.** Independent of 1–9. Highest value per unit of work in this document — and it turned out to be the prerequisite step 7 was waiting for |
| — | **Runtimes, Tier 2 (protocol + SDKs)** | 2 | M each | Only after Tier 1 shows a measured reason |

Steps 1–2 are the ones worth doing regardless of how much of the rest is ever built: a metrics
endpoint and a control plane are what make a server operable, and neither requires believing
anything in §6 or §7. Step 2 is also the whole of the "should it be a socket?" question, and its
answer is now *no* — which means every later step is a registry entry rather than a transport.

**1 and 2 are done**, and three things about them are worth more than the status line:

- **The seam that mattered was the last one.** The registry, the message types and the read-loop arm were
  each correct and independently tested, and a call still could not happen until the handler was installed
  on a live channel — because *that* is the code that decides which workers have a capability. A test per
  layer cannot see a missing `set_call_handler`; the end-to-end test did, in one run.
- **That test then found a deadlock in the call path**, not in any layer of it: a call is served from
  inside a request, so a handler that takes a slot lock waits for its own caller's. See the hazard in §4.2
  — it is the one design rule in this document that could not have been derived by reading either side
  alone.
- **A capability is now visible in the file that grants it.** `services = ["config"]` on a backend is the
  whole of what its workers may reach, checked against an allow-list at startup so a typo is a startup
  failure rather than an application that silently cannot call anything.

Step 3 (KV) is next: the first service to be a registry entry rather than machinery, and the one that
tests whether the "capability is a namespace" shape holds up for a service that is not about the server
itself.

**Step 3's `memory` driver is done, and it answered that question.** The shape holds: `kv` is one namespace
per backend holding only the storages that backend's `services` list names, so §4.5's rule is enforced
without any code in the storage layer knowing what a capability is. Two things the document did not
anticipate and the code settled:

- **The registry routes on the first dot**, so `kv.sessions.get` is the `kv` namespace with the verb
  `sessions.get`. One `MethodRegistry::with` call per granted storage would therefore have the second
  replace the first — one of two caches silently answering "no storage configured" — which is why the
  granted storages are collected and installed once.
- **The only thing connecting the configuration to the registry is `build_stores`.** Six layers were
  tested and none of them could see its absence; the end-to-end test found it in one run. That is the
  second time this document has been corrected by a seam rather than by a design argument, and it is
  worth noticing as a pattern: **the wiring between two tested things is where the untested bug lives.**
  Step 4 then produced a **third** instance of the same shape (`build_locks`, and a lock plane the
  configuration granted while nothing built it), which is enough for the pattern to be a rule: an
  end-to-end test that goes through the entry point a *deployment* uses is not optional for a service
  added to the registry.

`redis` remains, and it is wanted before locks because §6.3's implementation shares its client.

**Step 4 is done, `memory` and `redis`, and it is the step that made §6.3's argument into code.** The
sentence that was a warning in prose — *a lock whose state is unknown must not be reported as not-held* —
is a distinct error variant all the way from a Lua script to the PHP client, and the first code to
produce it. It also produced the third instance of the seam pattern (`build_locks`) and the first two
drivers to share a client, which is the shape §10 item 6 asks about: one `redis` crate, one feature, and
two — soon three — sets of semantics over it.

Three more things the code settled that the design did not:

- **An acquisition is not the only thing that can be undetermined, and it is not always.** The first
  implementation reported *any* failure during an acquisition as unknown — including a resource name
  the backend refused locally, which never reached a network. That is worse than the bug it guards
  against, because a flag set on failures that are perfectly well known is a flag callers learn to
  ignore. The rule the code now encodes: `Unknown` is undetermined by construction; a *rejected name*
  is decided because nothing was touched; and a backend failure is undetermined during an acquisition
  only.
- **`build_locks` was the seam, for the third time.** `set_call_handler`, then `build_stores`, now
  this: a function between two tested things, with six layers of tests that could not see its absence.
  It is recorded as a pattern rather than an anecdote because three instances is not a coincidence.
- **`ttl = 0` means "held until released" and it is the opposite of what a zero TTL means to KV.**
  Copying the KV TTL accessor would have produced a lock released the instant it was granted, which
  does not fail loudly — it never contends, so everything is allowed through.

Sizing, as revised by building it: S was right for the trait and the memory driver, and the call
methods cost more than S because of the third answer — every verb now has to decide which kind of
failure it is reporting, and `update_lease` returning `false` is a second "no" that means the opposite
of contention.

**Step 5 is done, both drivers, and it was three commits' worth of one design question plus a fourth for
the second driver.** `crates/volucer-jobs/` (the `Driver` trait and `MemoryDriver`), the `jobs` namespace
and `[jobs]`/`[jobs.pipelines]` (the producer half), then `JOB`/`ACK`, the `job` exchange, `[jobs.pool]`
and `crates/volucer-server/src/jobs.rs` (the consumer half, whose full list of consequences is in §6.4
above rather than repeated here), and finally `RedisDriver` behind the same feature the KV store and the
lock backend use.

The thing this step should be remembered for is not any of those items but the size of the gap the
*consumer* commit closed. Every layer was tested and correct: the driver's ack/nack against the driver, the
pool's `job` against a channel, the SDK's `consume` against a fixture. **What no test covered was whether
the three agreed** — and the three failure modes are all silent. A `Requeue` treated as an `Ack` loses work
an application asked to retry; an `Ack` treated as a `Requeue` runs every job twice; a `Reject` nacked
rather than acked brings a poisoned job back for ever. None of them is an error anywhere; they are a queue
that behaves oddly. So the end-to-end test for this step asserts on the *queue* — `size` and `in_flight`
after each outcome — rather than on the answer frame, because the reservation is what the policy is about.

That is the seam pattern's fifth instance, and this one generalises it: what sits between two tested
things is not always a `build_*` function. Here it was a **policy**, and the test that finds a policy bug is
one that runs the whole path a deployment runs and then asks the durable state what happened.

The fourth commit produced a second kind of evidence. `RedisDriver`'s tests found two bugs that reading
could not have — the reserve script was a stack rather than a queue, and the reservation had to be two
values rather than one for the settle's check to mean anything — and both are recorded in §6.4. **A driver
is where a design's assumptions about the storage engine get tested**, which is why §6.4's own table
predicted the Redis driver would be where the cost lives and was right.

**Three Redis drivers now share one client and one feature** — `volucer-kv`'s store, `volucer-lock`'s
locks, and now `volucer-jobs`' queue — which is the answer §10 item 6 was asking for: one `redis` crate, one
feature, three sets of semantics over it. What the fourth driver would cost is now known: a `PipelineConfig`
variant, a `build_queues` arm, and a test file. Two smaller things worth recording, both found this way
rather than by reasoning:

- **`max_jobs` and `max_requests` are one threshold under two names, and `WorkerPool::start_consumers`
  is where that is written down.** A consumer serves one job at a time, so "requests served" and "jobs
  handled" are the same counter counting the same events — which is why `[jobs.pool]` is a separate
  type but not a separate pool.
- **A consumer pool charges the same wait counters a request pool does.** The first version left
  `requests_queue`, the wait histogram and `jobs_served` at zero for a consumer pool, which reads as
  "no contention" on a pool with jobs queueing behind one another — a graph that says the opposite of
  the truth. The fix is that `WorkerPool::job` charges exactly what `attempt` charges.

Note the shape of the end of the list. gRPC is **no longer one XL item behind a gate**: it is three
items of S–M each, the first of which needs no schema, no worker and no new listener. The XL item
that remains — work delivered to a busy worker — is real and is §10 item 8, but nothing above it
depends on it.

**The last independent row landed before step 7 rather than beside it, and the dependency column is
what said so.** §7.1's supervision was listed as parallel work; step 7 lists it as a *dependency*,
and that is the correct reading — tier A routes `application/grpc` to a Unix-socket origin, which is
the same origin §7.1 teaches volucer to start and wait for. So it is done, and five things about it
are worth more than the status line:

- **The seam pattern's sixth instance, in its original form.** `UpstreamConfig::command` is read by
  `Backend::from_config`, and nothing in the config layer, the supervisor's own tests, or the
  front-end could see whether the dispatcher built a `Supervisor` at all. The end-to-end test went
  through `Config::from_toml` → `Dispatcher` → `Server` → an HTTP request, and it **was falsified
  properly**: making the construction a no-op fails it with `502 … the origin is unreachable`,
  which is what an unsupervised `command` looks like in production.
- **`restart` had to be an `Option`, and the first version was a serde default.** A default of
  `on_failure` made an *unsupervised* origin indistinguishable from one that had asked to be
  restarted, so the validator could not tell "this key does nothing" from "this key is set" — and it
  rejected every plain `kind = "upstream"` file in the tree. Two tests caught it. The rule that
  generalises: **a serde default and a validator that asks "was this key set" are not compatible**,
  and only a test that parses the ordinary configuration sees the disagreement.
- **Every path out of startup that fails has to kill the child itself.** On Unix `LifecycleGuard` is
  a deliberate no-op — the process group only helps when *we* are signalled — so the readiness
  timeout, where the process is running and `start` returns an error, would have left a `gunicorn`
  holding the socket until the machine rebooted. The test that found it asserts the *port is free*
  rather than counting processes, which would be platform-specific.
- **`restart_limit` had to acquire a meaning it did not have.** Written as a total budget it is a
  limit on a service's lifetime restarts: a crash once a week plus three consecutive failures would
  abandon an otherwise healthy origin. A run that outlived `STABLE_RUN` (a minute) now resets the
  count, which is what "consecutive" has to mean to be useful.
- **A zero `restart_backoff_ms` is refused, and the refusal is conditional.** With a limit above one
  it is a bounded-looking restart loop that runs back to back; with no restarts to space out it
  describes nothing and is allowed. A validator that refused it unconditionally would be refusing a
  harmless value, which is how a check earns being worked around.

The other smaller correction is worth recording because it cost a test run: **a Windows path in a
TOML basic string needs its backslashes doubled**, so `command = "…\Temp\x"` is a parse error at
`\T`. The example configuration and the integration test both write paths in single quotes, which
is the spelling that survives being copied to either platform.

**Step 8 is done, and it re-taught the step-3 rule in a place where it looked like it could not
apply: a service delivered to a worker is not a registry entry, and it needed a seam of its own.**

Every gRPC assertion in this step is one test —
`a_grpc_call_reaches_a_php_worker_and_its_status_comes_back_in_the_trailers` — and it is one test for
a reason: `granted_grpc_call` had to be reached from `Config::from_toml` → `Dispatcher` → `Server` → a
real PHP process, exactly as `build_stores` and `build_locks` had to be, and for the fourth time *no
test below the seam could see its absence*. It was falsified twice rather than once — making the
allow-list admit everything failed it at the config assertion, and disabling the front-end's gate
alone failed it at the assertion that the application was never asked — because the two halves fail in
opposite directions and a single falsification would have proved only one of them wired.

Four more things the code settled that the design did not:

- **Routing to a `worker` backend had to become protocol-aware, and that is the real cost of tier B.**
  The listener chose *upstream versus CGI* before reading the body, because an origin's body must not be
  touched; a gRPC call to a worker has to be refused in that same pre-body position, because buffering a
  body for a call that will not be dispatched is work an attacker asked for. What that forced is a
  second *kind* on the CGI path's failure — "the request never became a CGI request" (a 404, whatever the
  protocol) versus "a backend was asked and failed" (where the gRPC mapping belongs) — and collapsing
  them would mean answering a missing script with a `grpc-status`, claiming a service was consulted
  where none was.
- **The service is what is granted; the method is the application's.** Listing methods in
  `grpc_services` would put the application's interface in the server's file, so every application
  deploy would become a server configuration change — the coupling §6.6 exists to avoid. And the
  grant is a **literal service name** rather than a `path_prefix`, which is a decision the earlier
  draft had left open: a prefix can express `/pkg.Service/` but can also express `/pkg.Service/../other`
  unless a second rule refuses it, while two segments checked as protobuf names cannot name a service
  that is not one. See §10 item 1.
- **A worker serves both protocols, and that is the ordinary shape rather than a special case** — one
  pool is one backend, so a `worker` backend reached by an HTTP route *and* by `grpc_services` has one
  process answering both. The SDK's request path therefore identifies a call by `content-type` rather
  than by how the process was started, which cost it `onGrpc()` beside `run()`. The failure mode this
  avoids is quiet: a worker that answered gRPC framing to ordinary requests would pass any test that
  only made gRPC calls.
- **The server and the SDK disagreed about a query string, and a test found it rather than a reading.**
  PHP's `parse_url($uri, PHP_URL_PATH)` *strips* a query; the server's parser does not, because `?` is
  not a byte a protobuf identifier may contain. So `/a.B/M?x=1` was a refusal on one side and a
  method call on the other — invisible, because either side alone is self-consistent. The SDK now
  reads the path without `parse_url` and says why, and both sides' tests assert the same cases.

Sizing, as revised by building it: **M was right and the estimate was still optimistic.** The config
key is a dozen lines and the SDK contract is two methods; what was not M-sized was making the *request
path* protocol-aware, and that is a property of adding a second protocol to a router that had one.

---

## 9. Divergences, stated as divergences

RoadRunner is the reference, not the specification. Where copying it would copy a mistake, this
document says so, and the reason it can say so is that the mistakes are visible in its own source
and docs.

### 9.1 No runtime plugin container

RR's `endure` DI container and dynamic plugin registration are not reproducible in Rust without
giving up static dispatch, the MSRV job, `cargo package --workspace` and cross-compilation.
§5 is the substitute: a lifecycle trait, a service bag, and cargo features.

### 9.2 Locks are not a correctness primitive, and will not be advertised as one

RR's locks have TTLs and no fencing tokens. A late writer after a TTL expiry is not prevented, and
calling that a distributed lock without qualification is how an application acquires a data
corruption bug. volucer will ship locks, document exactly what they are for, expose
`set_if_absent` on KV as the primitive that *does* protect an invariant, and put the warning in the
README rather than only in the design document.

**The warning is now in four places, in this order of who reads it first:** `Lock.php`'s class
documentation, where an application author actually is; `volucer.example.toml`, where a deployment
copies from; the README; and here. That is not belt-and-braces — the argument for shipping locks at
all rests on the warning arriving *before* the call, and the plan is the place it arrives last.

`set_if_absent` on KV remains **not built**, and that is worth saying plainly rather than leaving the
sentence above to imply otherwise: the primitive that protects an invariant is the one an application
needs when it discovers a lock cannot, and it is still a sentence in a design document.

### 9.3 Metrics are on when configured, not when a middleware is listed

RR requires `http_metrics` in the middleware list before HTTP metrics exist, which means the
default observation of a fresh deployment is nothing. volucer emits them whenever `[metrics]` is
configured. The names stay RR-compatible; the activation rule does not.

### 9.4 Seven broker drivers is not a feature

RR ships AMQP, Kafka, SQS, Beanstalk, NATS, NSQ and Google Pub/Sub. Each is a large client
dependency, a set of offset and acknowledgement semantics to reproduce faithfully (Kafka's
consumer-group commits can skip unfinished records in a partition after a crash — from RR's own
docs), and a compatibility surface that would be the largest part of this project. volucer's
position: **one in-memory driver, one shared Redis driver, and a clean driver trait** so that a
broker nobody here runs is a contribution rather than a commitment. An application that needs
Kafka today needs `jobs.kafka`, and volucer should say that plainly instead of half-shipping it.

### 9.5 The call path is capability-scoped, and `config.describe` is redacted

RR binds `rpc.listen` to loopback with no authentication and exposes `rpc.Config`. §4.5 is
deliberately different: the registry is selected per backend, so a public HTTP worker does not get
`worker.reset`, and configuration is described with credentials elided by name. The difference is a
refusal rather than a warning.

### 9.6 gRPC carries bytes; it does not learn your schema

RoadRunner loads `.proto` at runtime and wires the descriptors into routing *and* reflection. That
makes the schema a server concern, and it is why its gRPC plugin is the heaviest in the tree. §6.6
takes the opposite position: **the server carries bytes, writes two header names, and never parses
a message.** Routing is a path string, the inner framing is length-delimited and therefore
schema-independent, and the descriptors are needed only by an optional reflection service. The
practical consequence is that a `.proto` change is an application deploy and never a server one,
and that no build step enters the server's dependency graph.

**Tier B kept that promise, and the way it did is worth stating because it is a stronger claim than
tier A's.** The application never sees a decoded message either: a gRPC handler is handed the framed
bytes, the method name as two strings, and nothing else. There is no generated DTO, no `protoc`, and no
descriptor anywhere on the path — the SDK's own reply helper does not encode a message, it carries one.
The one thing the SDK *does* know about gRPC is where the outcome goes (two trailer fields) and how a
media type is spelled, and both are protocol rather than schema.

The `upstream`-origin tier of that design is also the honest answer for gRPC today: a runtime that
already speaks gRPC is reached the same way a Django or Rails app is (§7.1), and volucer does not
need to be in the message path to be useful.

### 9.7 The gate is the construction side of trailers, not gRPC itself

`docs/plan.md` §6 recorded "every call would look failed" as the reason gRPC was gated. That was
correct about *the code* and wrong about *the effort*, and the distinction is worth keeping
because it is the kind of claim that gets quoted long after the code has moved. What the tree
lacks is any code that **constructs** a trailer; what it already has is hyper surfacing them on
ingress (h1 and h2), a frame-preserving body wrapper, and a response path that hands an origin's
body to the client without conversion. So the residual risk is one small `Body` implementation and
the status mapping — not a hand-written HTTP/2 state machine, which is where nginx's two CVEs
actually live. §6.6 carries the citations.

**Tier B confirmed it, and the confirmation has a shape worth naming: every bug on this path was in
the *construction* of a reply and none of them was in the transport.** `GrpcReplyBody` is the second
hand-written body in the tree and it needed the same two things its sibling does — an unknown size hint
while anything remains, and a `Trailer` declaration naming the fields — both of which are hyper's rules
rather than gRPC's, and both of which fail *silently*: a reply with either mistake is a valid `200` that
a gRPC client reads as a successful call with no message. nginx's CVEs are in the other half, the state
machine that parses what arrives. This server never parses a frame; hyper does.

---

## 10. Open decisions

1. **The service allow-list's granularity.** Tier B of §6.6 routes a gRPC call by its `:path`
   (`/pkg.Service/Method`). Is the allow-list a list of literal service names, a prefix table, or
   the same `path_prefix` routing used for HTTP — and does a call whose method is not in the list
   answer `UNIMPLEMENTED` or `NOT_FOUND`? The first is what a gRPC client expects from a server
   that does not implement a method; the second leaks whether the service exists.

   **Settled, and building tier B changed the answer twice.** The key is `grpc_services` — a *second*
   key beside `services`, not a reuse of it, because the two answer opposite questions (what a worker
   may call, and what may be delivered to it). It holds **literal service names**; the answer is
   `UNIMPLEMENTED` for the reason item 1 already gives, and the reason it must be literal names rather
   than `path_prefix` is not that a prefix could not express `/pkg.Service/` but that it *could also
   express anything else* — a prefix table admits a path like `/pkg.Service/../other` unless a second
   rule refuses it, whereas two segments with both halves checked as protobuf names cannot name a
   service that is not one. A route that names no method is refused for the same reason; see §6.6 for
   the full statement, including the case that first made the rule concrete (`?` in a method, where the
   server and the SDK initially disagreed).

2. **Reflection, and whether descriptors are ever loaded.** **Settled: nothing in this server parses
   a `.proto`, and no descriptor is loaded anywhere.** §6.6 made runtime parsing a property of one
   optional service (`protox` → `prost-types` → `ServerReflection`), and that service is not built: a
   `grpcurl` user supplies their own `.proto`, which is what they already do against every
   non-reflective server, and declining removes the only protobuf crate from the graph.

   **The question that looked like it might reopen this does not, and finding that out is the useful
   part.** A worker's reply is a complete, already-framed message — tier B's SDK says so in its own
   constructor — so *sending* one needs no descriptor, only a length read off the bytes. The one case
   that genuinely does need a message descriptor is **answering a client that cannot decode
   `application/grpc+proto` itself**: gRPC-JSON transcoding, where the reply is a `protojson` document
   and the encoding is defined over the schema rather than over the bytes. That needs the *response's
   own* type, which belongs to the application, so a reflection registry could not supply it either —
   it is a transcoding feature and belongs in item 11 below, not in a descriptor question. Everything
   else on the wire is length-delimited: one compression-flag byte, a big-endian `u32` length, then
   exactly that many bytes.

   **RoadRunner is a working implementation of this decision, and reading its source supports it
   rather than complicating it** — §6.6 has the citations. Its server parses `.proto` files with a
   syntax parser, uses the result only to build a routing table, and never reads the two fields that
   name the request and the response message; its reply path is a raw-bytes codec. Its *application*
   SDK is what decodes, against interfaces a `protoc` plugin generated. So the schema work happens,
   and it happens entirely on the application's side of the wire.
3. **How a plaintext gRPC origin says it speaks h2.** **Settled, and the answer was the explicit
   key**: `protocol = "h2c"` on an `http://` or `unix:` origin, refused beside `https://` where ALPN
   is the answer. `UpstreamProtocol` has one variant and no `Http1`, so HTTP/1.1 is the absence of the
   key rather than a value of it — the same rule `restart` follows, and for the same reason: a default
   would make "unset" and "set to the default" indistinguishable, and only one of those is a fact
   about the origin. The rejected alternative was inferring it from the route that selected the
   backend, which needs no key but makes an origin's protocol depend on where a request came from.

   The claim lives on `OriginSpec` — on *both* its variants, since a gRPC application on a unix socket
   is the shape this key exists for — which is what makes "`protocol` beside a handshake was refused"
   a property of the type rather than of `validate` having run. `Origin::speaks_h2()` reads it, and
   the connector answers `negotiated_h2()` for a plaintext TCP stream through a newtype or for a
   socket through a flag. Measured end to end in `crates/volucer-http/tests/frontend.rs`, with the
   negative case in the same test: a proxy that spoke h2 to every origin would pass a positive-only
   test and be broken for every HTTP/1.1 origin in the tree.
4. **Streaming responses in the worker protocol.** `RESPONSE` declares `body_len` up front, so an
   application must buffer. That is fine for HTML and wrong for a large download or a
   Server-Sent Events endpoint. Is a chunked response (unknown length, terminated by `LAST`) a v2
   feature, and does it land before or after a work-delivery exchange?
5. **ASGI/Django Channels.** Multiplexed requests per worker is a protocol change, not an SDK
   feature, and it is the same change a gRPC call as *work* needs. Do they share one design?
6. **Redis as one dependency for three features.** KV, locks and jobs all want a Redis client, and
   three drivers sharing one connection pool is better than three pools — but it also couples their
   feature flags. Is `redis` one feature that enables all three drivers, or three features over one
   crate?
7. **`jobs.pool.command` vs the server's command.** RR lets the jobs pool override the server
   command so consumers are a different script from request workers. volucer's `[server]` has no
   single command — backends carry their own — so the question is whether `[jobs.pool]` is a
   `WorkerConfig` (a fifth backend, effectively) or a distinct table.
8. **Which runtime gets Tier 2 first.** Django's adapter needs more explicit cleanup than Rails'
   (Rails' `executor.wrap` does the connection work for you), so if the answer is "the harder one
   proves the protocol", it is Django; if it is "the easier one ships", it is Rails.
9. **Second unit of work, or multiplexing?** A gRPC call as work (§6.6) and ASGI (item 5) both need
   a worker to be handed something while it is busy. The cheapest shape is a second *exchange* —
   the same `CALL`/`RETURN` discipline pointed the other way, work in and answer out — and the
   expensive one is genuine stream multiplexing with per-stream ids. Which, and does it land once
   for both? Note this is the *only* thing left that contradicts one request in flight, and it is
   off the critical path for every gRPC tier in §6.6.
10. **Whose deadline is a kill?** §4.2 item 3 and its hazard: a call blocking past the request's
    deadline currently means the worker is killed. The policy (report the call as *unknown state* and
    keep the worker, versus kill) is a decision, not a detail, and §6.3's error variant depends on it.
11. **gRPC-JSON transcoding, which is the one thing a descriptor would actually buy.** A browser
    cannot speak `application/grpc+proto` natively, and the usual answer — `grpc-gateway`, Envoy, or
    gRPC's own `grpc-web` proxy — is a second process with the same schema problem. The shape here
    would be a `content-type` that names JSON and a route that transcodes, and it is genuinely harder
    than any of tiers A–C for a reason worth stating: **transcoding needs the response's own message
    type**, which belongs to the application, so it is not something a reflection registry could
    supply even if tier C existed. It also collides with §6.6's central claim — that a message is
    bytes and the schema is the application's — because a transcoder has to decode. Recorded rather
    than recommended, and **gRPC-Web is the same question with a smaller answer**: it is a framing
    layer over the bytes already carried, needing no schema at all, and would be the version to build
    first if a browser ever has to call a service directly.
12. **Whether an untrusted peer's `X-Forwarded-For` should be *stripped* rather than ignored.** §6.8
    ignores it, which is the conservative answer and enough for the two readers that exist (the log
    and `REMOTE_ADDR`). A **rate limiter** changes the shape of the question, because it is the first
    reader whose correctness depends on the *chain*: the fully correct algorithm walks the list
    backwards from the peer and stops at the first untrusted hop, which is what tells two clients
    apart behind nested proxies. That needs every hop's address rather than the one §6.8 resolves, so
    it is a decision for the rate-limit step rather than a defect in this one. The other half of the
    same question: an origin downstream of volucer sees the client's claim relayed verbatim, which is
    `X-Forwarded-*`'s specified behaviour — but a deployment where the origin *trusts* that header and
    volucer is the last hop would want the claim dropped instead. Recorded, not changed: no origin in
    this tree reads it.

    **Settled for the rate-limit step, and the answer is that it is still not needed.** §6.10 keys
    the limiter on the address `[server.proxy]` already resolves — the same one `REMOTE_ADDR` and the
    access log use — because the chain walk only changes an answer when the chain mixes trusted and
    untrusted hops. In the shape this table is for (one proxy whose address the deployment names),
    the single resolved address *is* the rightmost untrusted one, so a backwards walk would return the
    same address with more parsing. A deployment nested two proxies deep is the case it would fix, and
    that deployment can name both ranges in `trusted_subnets` to get the same effect today.
    RoadRunner's `proxy_ip_parser` does not walk the chain either. So the decision is *deferred with
    its trigger stated*: build it when a deployment's configuration cannot express its topology,
    which is also when an operator would be able to describe the chain it wants.