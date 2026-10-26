# Observability

Volucer includes request logging and identifiers, rate limiting, worker-control routes,
Prometheus metrics, and distributed trace propagation and export. These features are
independent and opt-in where appropriate. Trace export uses OTLP/JSON without requiring an
OpenTelemetry SDK dependency.

## The access log

The access log is disabled by default. Add `[server.access_log]` to enable it:

```toml
[server.access_log]
path = "logs/access.log"          # relative resolves against the config file's directory
format = "combined"               # or "json"
request_id_header = "X-Request-Id"
```

| Key                 | Type   | Default          | What it does                           |
| ------------------- | ------ | ---------------- | -------------------------------------- |
| `path`              | path   | **required**     | Where the log is written.              |
| `format`            | string | `"combined"`     | `"combined"` or `"json"`.              |
| `request_id_header` | string | `"X-Request-Id"` | The header to adopt a request id from. |

Volucer does not create the parent directory. If it does not exist or cannot be opened,
startup reports the problem once and the server continues without writing the access log.
Requests continue to be served even if a later write fails. Create the directory and ensure
the service account can write to it before starting Volucer.

### The format

The default format follows nginx's `combined` layout, with three fields appended:

```
127.0.0.1 - - [16/Oct/2026:21:35:05 +0000] "GET /x HTTP/1.1" 200 21 "-" "curl/8" 0.012 0.004 3f2504e0-4f89-11d3-9a0c-0305e82c3301
```

These fields are total request time, time to the origin's first byte, and request ID.
Total request time includes the time spent receiving the client's request body. The
origin-time field helps distinguish time spent waiting for an origin from time spent
receiving or processing the request.

The origin-time field is `-` when the request had no origin, such as a static-file response.
An origin that responds immediately has a measured duration instead. With
`format = "json"`, each line is a JSON object and `origin_time` is `null` when there was no
origin.

### Rotation

On Unix, `SIGHUP` closes and reopens the access-log file. This supports rotation tools such
as `logrotate`, which rename the active file before signaling the server. Reopening ensures
subsequent writes go to the new file rather than the renamed one.

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

The signal reopens the log independently of configuration reloads. Volucer does not rotate
logs by size or age; use a system log-rotation tool to apply the deployment's retention
policy. `SIGHUP` does not change the log path, format, or `request_id_header`; these
settings are read at startup.

### What it is not

The access log is separate from diagnostic logging and is not controlled by the diagnostic
log level. Its behavior has a few important limitations:

- A failed write does not fail the request. For example, a full disk or changed permissions
  can prevent logging while the server continues to serve traffic. Volucer reports a
  persistent write failure once rather than once per request. `volucer check` also reports
  a path that cannot be opened at startup.
- `bytes_sent` is `0` for streamed bodies, including static files and origin responses.
  Buffered text responses from a backend report their actual size.
- In the combined format, spaces, quotes, backslashes, and newlines in values are escaped
  as `\x20`, `\x22`, `\x5C`, and `\n`. This prevents request-controlled values from adding
  fields or lines to the log.

### The client address behind a proxy

When Volucer is behind a proxy, the access log's client-address field reflects the proxy
unless trusted proxy headers are configured with `[server.proxy]`:

```toml
[server.proxy]
trusted_subnets = ["127.0.0.1", "10.0.0.0/8"]   # the networks whose word is believed
trusted_headers = ["X-Real-Ip", "X-Forwarded-For"]   # consulted in this order
```

Volucer trusts a forwarded address only when the TCP peer belongs to a configured
`trusted_subnets` range. Without a trust list, it ignores forwarded-address headers. Invalid
subnet values cause startup to fail rather than silently reducing the configured trust list.

Headers are checked in the order listed in `trusted_headers`; the first one containing a
parsable address is used. Because proxies append values to `X-Forwarded-For`, its
left-most address may be a client-supplied claim. Configure the header order to match the
proxy topology you control.

The FastCGI listener does not use this table. In a typical nginx setup, nginx applies its
own `real_ip` configuration and sets `REMOTE_ADDR` to the resolved client address before
forwarding the request.

## The request id

Volucer assigns every request an ID and returns it in the response, including error
responses. Generated IDs contain 32 hexadecimal characters, matching the shape of nginx's
`$request_id`.

By default, Volucer adopts an inbound `X-Request-Id` when it contains a valid identifier.
This lets clients and upstream services correlate logs across tiers. Adopted values are
preserved verbatim, including UUID hyphens and letter case. Set `request_id_header = ""` to
always generate a new ID instead.

Malformed values are replaced with a generated ID rather than causing the request to fail.
This prevents newlines or spaces in a client-supplied value from corrupting log records.

## Rate limiting

Rate limiting is disabled by default. When no policy is configured, requests do not incur
rate-limit bookkeeping.

```toml
[server.rate_limit]
key = "ip"            # "ip" (default), "header", or "global"
rate = 60             # requests allowed per interval_ms
interval_ms = 60000   # how long `rate` requests take to accrue
# burst = 60          # how many may be spent at once; defaults to `rate`
# max_entries = 10000 # identities tracked before new ones are refused
```

The limiter uses a continuously refilling token bucket. Its capacity is `burst`, and tokens
refill at a rate determined by `rate` and `interval_ms`. Unlike a fixed-window counter, a
token bucket does not allow a client to spend two full windows' allowances on either side
of a window boundary. `burst` defaults to `rate`, so the full configured allowance can be
used at once and then refills over the interval.

With `key = "header"`, configure the header name as well. Header values are not
authenticated identities: clients can choose them, so use a value set by a trusted proxy.
Values are hashed rather than stored, so long values do not increase the size of each entry.

Requests that exceed the limit receive `429` and `Retry-After`, with
`Cache-Control: no-store` to prevent shared caches from reusing the refusal. gRPC requests
instead receive `RESOURCE_EXHAUSTED` in the trailers.

With `key = "ip"`, Volucer uses the address resolved by `[server.proxy]`, not necessarily
the TCP peer. Configure trusted proxy handling when the limiter needs to distinguish
clients connecting through a proxy.

One policy applies process-wide to both front ends; it does not vary by route or site.

## The worker-control routes

Worker-control routes are disabled by default. To expose them, configure `[status]` and
explicitly enable resets:

```toml
[status]
address = "127.0.0.1:2114"
allow_worker_reset = true
```

```sh
curl -sf http://127.0.0.1:2114/workers | jq        # the list
curl -sf -XPOST http://127.0.0.1:2114/workers/3/reset
```

`GET /workers` returns JSON describing workers across all pools. The `index` is the
process-wide worker index used by the reset route; `slot` is the worker's index within its
pool and corresponds to the number used in that pool's logs.

Reset responses use HTTP status codes: `404` means the index does not exist, `409` means
the worker is currently serving a request, and `500` means the replacement worker failed
to start. A busy worker can be reset after it becomes idle. Worker replacement runs via
`spawn_blocking`, so a slow startup does not block the status listener.

These routes do not provide authentication. Keep the listener on loopback or a private
interface and restrict access through your network configuration. If resets are enabled,
Volucer refuses to start when the status listener is bound to a wildcard address.

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

| Key       | Type            | Default      | What it does                           |
| --------- | --------------- | ------------ | -------------------------------------- |
| `address` | string          | **required** | Where the Prometheus endpoint listens. |
| `path`    | string          | `"/metrics"` | The endpoint path.                     |
| `prefix`  | string          | absent       | A prefix for the metric names.         |
| `collect` | table of tables | empty        | The metrics to collect.                |

`[metrics]` and `[status]` share a listener. If both are configured, their `address` values
must match; different addresses cause startup to fail. Configure only the feature you need
if you do not need both.

`[metrics.collect.<name>]`:

| Key       | Type             | Default      | What it does                             |
| --------- | ---------------- | ------------ | ---------------------------------------- |
| `type`    | string           | **required** | `"counter"`, `"gauge"` or `"histogram"`. |
| `help`    | string           | **required** | The description, as Prometheus requires. |
| `buckets` | array of numbers | empty        | The bucket boundaries. Histograms only.  |

`buckets` is valid only for histograms. Volucer rejects it for counters and gauges.

## Trace context

Volucer parses and continues the W3C `traceparent` header. When a valid header is present,
the request keeps its trace ID and Volucer creates a span for its own work. If the header is
missing or malformed, Volucer starts a new, unsampled trace.

Volucer forwards trace context to backends without additional configuration. For PHP
workers, `traceparent` is exposed as `HTTP_TRACEPARENT`; the header is not removed by the
strip lists.

## Exporting traces

Trace export is disabled by default. Configure `[telemetry]` to send spans to an OTLP
collector:

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

Volucer sends OTLP/JSON requests to the collector's `/v1/traces` endpoint using the pooled
HTTP client shared with the `upstream` backend. This does not add an OpenTelemetry SDK
dependency, and HTTPS endpoints use the server's existing TLS configuration.

For each request, Volucer creates a server span and forwards its trace and span context to
the backend. This allows spans produced by a worker or origin to appear as children of the
Volucer span, without requiring application changes to establish the parent context.

The server span includes `http.request.method`, `url.path`, `server.address`, and
`http.response.status_code`. The query string is omitted from `url.path` to avoid exporting
unbounded or potentially sensitive values. A 5xx response sets the span status to error;
4xx responses do not.

Trace export runs asynchronously. Requests enqueue spans without waiting for the collector;
collector failures can result in lost spans but do not add request latency. Volucer logs the
first export failure and recovery, and reports exported, lost, and queue-dropped spans at
shutdown. Failed batches are dropped rather than retried, preventing an unbounded retry
queue from consuming process memory during a collector outage.
