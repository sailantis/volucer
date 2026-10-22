# Observability

**An access log, a request id that spans your tiers, a rate limiter, Prometheus metrics and
distributed tracing — without OpenTelemetry.**

The order is deliberate: a framework exports spans, this server had no spans and no request
identity, so adopting one first would have meant wiring an exporter to nothing.

## The access log

**Off by default**, so upgrading changes nothing. `[server.access_log]` turns it on:

```toml
[server.access_log]
path = "logs/access.log"          # relative resolves against the config file's directory
format = "combined"               # or "json"
request_id_header = "X-Request-Id"
```

| Key | Type | Default | What it does |
|---|---|---|---|
| `path` | path | **required** | Where the log is written. |
| `format` | string | `"combined"` | `"combined"` or `"json"`. |
| `request_id_header` | string | `"X-Request-Id"` | The header to adopt a request id from. |

**The directory is not created for you.** If `logs/` does not exist, the server starts,
serves normally and writes nothing: the failure is reported once at startup rather than per
request, and requests are never failed because a log could not be written. Create the
directory before the first start.

### The format

The default line is nginx's `combined` format with three fields appended, so a script
written for nginx reads this log too:

```
127.0.0.1 - - [16/Oct/2026:21:35:05 +0000] "GET /x HTTP/1.1" 200 21 "-" "curl/8" 0.012 0.004 3f2504e0-4f89-11d3-9a0c-0305e82c3301
```

The three additions are the whole request's time, the time to the origin's first byte, and
the request id.

**The first includes the client's own upload**, which is what nginx's `$request_time` does
and what surprises people about it. The second is the field that separates this server's
work from the client's slowness.

`-` means there was no origin: a static file never had one, where an origin that answered
instantly did. Rendering both as `0.000` would make a graph of a mixed deployment wrong in
the direction that flatters it. `format = "json"` emits one object per line instead, with
`origin_time: null` for the same distinction.

### Rotation

**`SIGHUP` closes and reopens the file, so `logrotate` works.**

That is the whole reason the reopen exists: `logrotate` renames the log and signals the
process, and a server that does not reopen keeps writing to the **unlinked inode**. Every
write succeeds, the path holds a fresh empty file, and the log is found to be empty during
the incident it was needed for.

```text
/var/log/volucer/access.log {
    daily
    rotate 14
    compress
    postrotate
        kill -HUP "$(pidof volucer)"    # or: systemctl reload volucer
    endscript
}
```

The signal reopens unconditionally, before the configuration is even read — a rotation is a
change to the filesystem and a config that fails to parse is not a reason to keep writing
into a renamed file.

**Rotation by size or age is not built**, deliberately: `logrotate` knows the deployment's
retention policy, and a second rotator is a second schedule to get wrong. `SIGHUP` also does
not change the log's path, format or `request_id_header` — those are chosen at startup.

### What it is not

**It is not the diagnostic log, and that separation is the point.** An access log that
shared a verbosity filter with diagnostics would be one setting away from silence, and both
directions are wrong: raising verbosity to debug a worker would flood it, and quieting the
diagnostics would erase the record of who requested what.

Three things it deliberately does not do:

- **A failed write never fails a request.** A full disk or a revoked permission means the
  log is lost, not that the server stops. The failure is reported **once**, not per request.
  A path that cannot be opened at startup is the same: `volucer check` says so, and `serve`
  serves without it.
- **`bytes_sent` is `0` for a streamed body.** A backend's answer is in memory and knows
  its length; a static file or an origin's answer does not until it has been written.
  Counting it for real would put a wrapper task on the hot path for one log field. Text
  responses from a backend report the real figure.
- **Values containing a space, quote, backslash or newline are escaped** as `\x20`, `\x22`,
  `\x5C` and `\n`. This is a security rule, not tidiness: the line is space-separated, so an
  unescaped space in a `User-Agent` lets a client write fields of *this* log line.

### The client address behind a proxy

**The first field is the client's address, which behind a proxy has to be told to the
server.** That is `[server.proxy]`, and without it the field is the proxy:

```toml
[server.proxy]
trusted_subnets = ["127.0.0.1", "10.0.0.0/8"]   # the networks whose word is believed
trusted_headers = ["X-Real-Ip", "X-Forwarded-For"]   # consulted in this order
```

**The trust list is the feature, not the headers.** `X-Forwarded-For` is a request header,
so any client can send one; an address from a header is believed only when the TCP peer is
inside a configured network. With no list — the default — no header is read at all, which is
also why a direct deployment needs no configuration.

A range that does not parse stops the server rather than being dropped: a trust list is
somewhere a typo is a security decision, and a shortened list is not visible afterwards.

The **order** of `trusted_headers` is the security-relevant part: the first header that
yields a parsable address wins. It is worth knowing what the default does *not* fix:
`X-Forwarded-For` is appended to by every hop, so behind a CDN its left-most element is the
client's own *claim* rather than its address. A deployment that knows its topology should
name the list.

On the **FastCGI** listener this table is deliberately not consulted. nginx sets
`REMOTE_ADDR` to the address it saw the client at, already resolved by nginx's own `real_ip`
handling, so the param is the client's address even though the socket belongs to nginx.

## The request id

Every request gets one, and it is **echoed on the response** — including on error pages,
because a client that cannot quote its id has no way to report the error.

Generated ids are 32 hex characters, which is exactly nginx's `$request_id` shape, so one
`awk` reads both logs.

**An inbound `X-Request-Id` is adopted by default**, and that is the opposite of nginx's
behaviour on purpose: one id across the tiers is the feature, and an id that changes at
every hop cannot correlate a client's log with this server's.

The value is returned **verbatim** — a UUID keeps its hyphens and its letter case, because
a normalised value is not findable in the client's own log. Set `request_id_header = ""` for
nginx's always-generate semantics.

A value that is not a plain identifier is **replaced, never refused**: a newline in an
adopted id would let a client forge whole log records, and a space would break the field
count. A client's malformed header must not become a client's failed request.

## Rate limiting

**Off by default**, and off is *no limiter* rather than a limiter configured to allow
everything: an unconfigured request path does one check and touches no map, no clock and no
lock.

```toml
[server.rate_limit]
key = "ip"            # "ip" (default), "header", or "global"
rate = 60             # requests allowed per interval_ms
interval_ms = 60000   # how long `rate` requests take to accrue
# burst = 60          # how many may be spent at once; defaults to `rate`
# max_entries = 10000 # identities tracked before new ones are refused
```

A **token bucket that refills continuously**, not a windowed counter: a `limit_req`-style
counter lets a client spend one window's allowance just before a boundary and the next just
after, doubling the instantaneous rate. A bucket cannot hold more than `burst` and refills at
a fixed rate.

`burst` defaults to `rate` rather than to RoadRunner's `1`, because `rate = 60,
interval_ms = 60000` reads as "60 a minute" — the allowance is spendable at once and returns
over the minute.

`key = "header"` needs a `header` name, and **that value is not an authenticated identity**:
a client chooses it, so a client that varies it gets one bucket per request until the table
fills. Its use is a header the *proxy* sets. Values are hashed rather than stored, so a long
value costs one entry.

**A refusal is `429` with `Retry-After`**, and `Cache-Control: no-store` so a shared cache
cannot serve one client's refusal to another. A **gRPC** call is refused as
`RESOURCE_EXHAUSTED` in the trailers instead, because a gRPC client cannot read a `429` at
all.

The `ip` key is the address `[server.proxy]` resolves, **not** the socket peer — behind
nginx without that table every request would be charged to the proxy and no two clients
could be told apart.

One policy per process, shared by both front-ends, and it does not distinguish routes or
sites: a bucket's identity is (who, *which limit*), so two policies over one client are two
buckets.

## The worker-control routes

**Off by default**, and the default is the safety: a deployment that configured `[status]`
for an orchestrator gains no mutation surface by upgrading.

```toml
[status]
address = "127.0.0.1:2114"
allow_worker_reset = true
```

```console
curl -sf http://127.0.0.1:2114/workers | jq        # the list
curl -sf -XPOST http://127.0.0.1:2114/workers/3/reset
```

`GET /workers` answers JSON with one entry per worker across every pool. `index` is the
**global** one a reset takes; `slot` is the pool's own, which is the number that pool's log
uses. Both are reported because with two backends they are different numbers.

**A refusal is a status code, so `curl -f` is the whole client**: `404` for an index that
does not exist, `409` for a worker that is serving a request (retry when it is idle), `500`
for a replacement that did not boot. That is why the reset boots through `spawn_blocking` —
a 30-second PHP start on the listener's own thread would stall the readiness probe this
port exists to answer.

**There is no authentication.** What protects these routes is that they do not exist until
the flag is set — `404` on the path, not `403` — and that a wildcard `address` with the
flag on is **refused at startup**. An unauthenticated `POST` that recycles workers,
reachable from anywhere, is a remote kill switch. Put `[status]` on loopback or a private
interface and reach it through your orchestrator's network, or don't turn it on.

## Metrics

```toml
[metrics]
address = "127.0.0.1:2112"
path = "/metrics"          # the default
prefix = "volucer_"       # optional

[metrics.collect.app_requests]
type = "counter"          # "counter", "gauge" or "histogram"
help = "Application requests handled"
```

| Key | Type | Default | What it does |
|---|---|---|---|
| `address` | string | **required** | Where the Prometheus endpoint listens. |
| `path` | string | `"/metrics"` | The endpoint path. |
| `prefix` | string | absent | A prefix for the metric names. |
| `collect` | table of tables | empty | The metrics to collect. |

**`[metrics]` and `[status]` share one listener.** If you configure both, they must carry the
same `address`; two different addresses are refused at startup rather than silently binding
one and dropping the other. Configure only one if you only need one.

`[metrics.collect.<name>]`:

| Key | Type | Default | What it does |
|---|---|---|---|
| `type` | string | **required** | `"counter"`, `"gauge"` or `"histogram"`. |
| `help` | string | **required** | The description, as Prometheus requires. |
| `buckets` | array of numbers | empty | The bucket boundaries. Histograms only. |

`buckets` on a counter or a gauge is refused. A setting that is read by nothing looks
supported and cannot work.

## Trace context

The W3C `traceparent` header is parsed and continued: the trace id is the client's, and
this server contributes its own span, because two hops claiming one span is what corrupts a
waterfall.

A request with no `traceparent` starts a **new trace, unsampled** — accepting a header is
not the same as volunteering to record a trace, and the party that knows what that costs is
the one collecting spans.

It is **forwarded to backends with no configuration at all**: `traceparent` becomes
`HTTP_TRACEPARENT` for PHP and is in neither strip list, so a correlation id already reaches
your application today. A malformed `traceparent` starts a fresh trace rather than returning
an error.

## Exporting traces

**Off by default**, like the access log, and for the same reason. `[telemetry]` turns it on:

```toml
[telemetry]
endpoint = "http://127.0.0.1:4318"   # the collector; /v1/traces is appended
service_name = "myapp"               # the `service.name` resource attribute
# timeout_ms = 10000                 # per export request
# batch_size = 512
# interval_ms = 5000                 # send a partly-filled batch after this long

[telemetry.headers]                  # for a hosted collector; redacted by `volucer check`
"x-honeycomb-team" = "..."
```

`POST /v1/traces` with a hand-written OTLP/JSON body, over the same pooled HTTP client the
`upstream` backend uses — so **it adds no dependency to the build**, and `https:` works with
the TLS configuration the rest of the server already has.

**The exported span carries the ids the request already has.** This server mints a trace id
and a span id per request and puts *that* `traceparent` on the request it hands a backend,
so a PHP worker's own span — or an origin's — names this server's span as its parent, and
the trace is one tree rather than two. Nothing in an application has to change to be part of
a trace.

An exported server span contains: `http.request.method`, `url.path` (**not** the query
string, which is unbounded and often a secret), `server.address`, and
`http.response.status_code`. A 5xx sets the span's status to error; a 4xx does not, because
marking a client's mistake as a server fault is how an error-rate alert becomes noise.

Two rules it follows that are worth knowing before turning it on:

- **A request never waits on the collector.** Recording a span is a queue send; sending
  happens on the exporter's own thread. A collector that is slow, refusing, or absent costs
  spans and never latency — and it is said out loud: the first failure of an outage is
  logged, the recovery is logged, and the shutdown summary prints how many spans were
  exported, lost, and dropped at the queue.
- **A batch is dropped, never retried.** A retry queue is what turns a collector outage into
  an application outage, by growing without bound in the process's memory.
