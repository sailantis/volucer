# The Worker Protocol

This page summarizes version 1 of the wire protocol between Volucer and a worker.

The normative specification is
[`docs/worker-protocol.md`](https://github.com/sailantis/volucer/blob/main/docs/worker-protocol.md)
in the server repository. `crates/volucer-worker` is the reference implementation, and its
tests verify the encoded bytes.

The protocol is implemented by the Rust reference and the
[`sailantis/volucer-php-sdk`](https://github.com/sailantis/volucer-php-sdk),
[`sailantis/volucer-python-sdk`](https://github.com/sailantis/volucer-python-sdk) and
[`sailantis/volucer-ruby-sdk`](https://github.com/sailantis/volucer-ruby-sdk). Each SDK
repository links to the specification at a pinned commit.

If you are writing a PHP worker, use the PHP SDK. If you are implementing a worker in
another language, consult the normative specification; this page is an overview, not a
replacement for it.

## Why it exists

Worker mode keeps an application process alive to handle multiple requests. The worker also
needs a channel for sending messages back to Volucer. The two common alternatives do not
provide the required behavior:

- FastCGI is server-driven: the worker answers the request it receives, but cannot send
  unsolicited messages such as logs, metrics, or a request to recycle.
- Goridge provides framing, not the worker-service semantics Volucer needs. Using its
  framing would also tie Volucer to its wire-format versioning; adopting its semantics would
  require compatibility with RoadRunner's protocol.

Volucer therefore uses a similar frame header with its own versioned message types and
semantics.

## Transport

By default, the protocol uses the child process's stdin and stdout pipes. This works across
platforms and does not require a separate port for each worker. Stderr is outside the
protocol and remains available for diagnostics; the supervisor drains it to prevent the pipe
buffer from filling and blocking the worker.

```
  volucer ──HELLO/REQUEST/CONTROL/PING/CALL──►  php worker  (stdin)
            ◄─HELLO_ACK/RESPONSE/BODY/RETURN ──  … ──LOG/METRIC/EVENT─  (stdout)
```

With `transport = "socket"`, the same frames, handshake, and ordering are used over a
socket. Only the transport changes.

Volucer creates the socket and provides its address to the worker through
`VOLUCER_WORKER_SOCKET`:

| Value | Transport |
|---|---|
| `127.0.0.1:34567` | loopback TCP, an ephemeral port the server bound |
| `unix:/run/volucer/worker.0.sock` | a unix socket, from the backend's `socket` key |

Named pipes are not supported as a worker transport. The protocol requires a bidirectional
connection, while a Windows pipe's state is tied to each connection and cannot be split into
independent read and write halves.

The protocol is symmetric: both sides use the same frame format to read and write messages.

## Frame format

Each frame consists of a 16-byte header followed by exactly `payload_len` bytes.

```
offset  size  field
0       4     payload_len        payload bytes, excluding this header
4       1     protocol_version   must be 1
5       1     message_type       1..=18, see below
6       2     flags              see below
8       4     request_id         0 = unsolicited / control
12      4     crc32c(payload)    CRC-32C, see below
```

All multi-byte integers use little-endian byte order.

The reference frame, in bytes, for `REQUEST` with `request_id = 1`, `payload_len = 2`:

```
02 00 00 00   payload_len = 2
01            protocol_version = 1
03            message_type = REQUEST
00 00         flags = 0
01 00 00 00   request_id = 1
aa d0 7b 29   crc32c("{}") = 0x297BD0AA
7b 7d         payload = "{}"
```

### Payload limits

| Limit | Value | Why |
|---|---|---|
| `MAX_PAYLOAD_LEN` | 64 MiB | A corrupt `payload_len` must not make the reader allocate 4 GiB |
| `MAX_BODY_CHUNK` | 64 KiB | Bounds the memory a single frame can demand |

A frame that declares more than `MAX_PAYLOAD_LEN` is rejected before the receiver allocates
its payload buffer.

## Message types

Message type numbers are part of the protocol. Do not reorder or reuse them.

| # | Name | Direction | Correlated | Carries |
|---|---|---|---|---|
| 1 | `HELLO` | server → worker | no | Protocol version, for the handshake |
| 2 | `HELLO_ACK` | worker → server | no | `protocol_version`, `php_version`, `sapi`, `pid`, `runtime` |
| 3 | `REQUEST` | server → worker | yes | Request metadata (JSON). The body follows in `BODY` frames |
| 4 | `RESPONSE` | worker → server | yes | Response head: `status`, `headers`, `body_len` |
| 5 | `BODY` | either direction | yes | Raw bytes of the request or response body |
| 6 | `EVENT` | worker → server | no | Application-level telemetry |
| 7 | `LOG` | worker → server | no | One log record: `level`, `message`, `request_id` |
| 8 | `METRIC` | worker → server | no | One sample: `name`, `value`, `unit` |
| 9 | `CONTROL` | server → worker | no | `drain`, `shutdown`, `reload` or `report` |
| 10 | `CONTROL_ACK` | worker → server | no | Acknowledgement of a `CONTROL` |
| 11 | `END` | worker → server | yes | `app_status`, `peak_memory_bytes`, `duration_us`, `keep_alive` |
| 12 | `PING` | either direction | no | Keep-alive probe |
| 13 | `PONG` | either direction | no | Answer to `PING` |
| 14 | `ERROR` | either direction | no | `message`, `request_id`, `recoverable` |
| 15 | `CALL` | worker → server | yes | `method`, JSON params. A call to the *server*, asked while a request is in flight |
| 16 | `RETURN` | server → worker | yes | `ok`, and a JSON result or an error |
| 17 | `JOB` | server → worker | yes | `pipeline`, `id`, `task`, `headers`, `attempts`, `body_len` |
| 18 | `ACK` | worker → server | yes | `outcome` (`ack`, `requeue`, `reject`) and an optional `reason` |

Unknown type numbers are rejected rather than skipped, since skipping one could leave the
reader and sender out of sync.

### Which types each side may receive

A worker may receive `HELLO`, `REQUEST`, `JOB`, `PING`, `CONTROL`, and `RETURN`. It sends
`CALL` and `ACK`; it must not receive them. A worker must also reject server messages that
are worker-to-server only, including `RESPONSE`, `BODY`, `END`, `HELLO_ACK`, `CONTROL_ACK`,
`PONG`, and `ERROR`.

The server sends `REQUEST`, `JOB`, `HELLO`, `CONTROL`, and `PING`, and answers a worker's
`CALL` with `RETURN`. A `CALL` received by the server is a protocol error. In general, a
`CALL` is sent by the side requesting a service; in this protocol, that is the worker.

## Body framing

A body consists of zero or more `BODY` frames in the direction of travel. All frames use
the same request ID. Receivers must enforce both of these rules:

1. `body_len` specifies the exact number of bytes to receive, allowing the receiver to detect
   a short body.
2. The final `BODY` frame carries the `LAST` flag.

A zero-length body has no `BODY` frames.

Bodies are transmitted as raw bytes, not JSON or base64. This avoids base64 expansion and
unnecessary per-byte encoding work.

## Flags

| Bit | Name | Meaning |
|---|---|---|
| `0x0001` | `COMPRESSED` | Reserved. No codec is negotiated in v1, so a frame carrying this bit is **rejected** |
| `0x0002` | `CHUNKED` | The payload is one chunk of a larger logical message |
| `0x0004` | `LAST` | This is the last chunk of a logical message |
| `0x0008` | `ERROR` | The sender is reporting a failure |
| `0x0010` | `NO_CHECKSUM` | The `crc32c` field is **not** a checksum and must not be verified |
| `0x0020` | `STREAMED` | On `RESPONSE`: the body's length is not declared, and `LAST` is its only end signal |

`NO_CHECKSUM` is a flag rather than a zero checksum value. Zero is a valid checksum:
`crc32c("") == 0`, including for frames with an empty payload.

`STREAMED` is a flag rather than reading `body_len: 0` as "unknown", for the same kind of reason:
a zero length is already a real answer — an empty body, a `204`, a `HEAD` — so a reader that treated
every zero as "stream me bytes" would consume the next request's frames as this response's body. A
streamed response declares `body_len: 0` *and* this bit, ends at the `LAST`-flagged `BODY` frame,
and has no length to check the body against — which is why `END` without `LAST` is a truncation
rather than a short body. The full rule is in the
[protocol specification](https://github.com/sailantis/volucer/blob/main/docs/worker-protocol.md#a-response-whose-length-is-not-known).

## Checksum

The header carries CRC-32C (Castagnoli), using reflected polynomial `0x82F63B78` and
initial and final values of `!0`. This is the `crc32c` algorithm, not `crc32b` as used by
zlib and PHP's standard `crc32()` function.

```
crc32c("123456789") == 0xE3069283
crc32c("")          == 0x00000000
```

In PHP, `hash('crc32c', '123456789')` produces the same value, allowing the PHP SDK to use
the built-in hash implementation.

A checksum mismatch indicates that the stream may be out of sync. The connection cannot be
recovered, so the worker must be discarded.

### Turning it off

Checksum validation can be disabled through a handshake negotiation:

- The server may include `no_checksum: true` in `HELLO` to offer disabling checksums.
- The worker sets `no_checksum` in `HELLO_ACK` to `true` only if it supports and accepts the
  option.
- Checksums are disabled only when both sides agree.

The negotiation has these constraints:

1. The handshake is always checksummed because it carries the negotiation itself.
2. A sender cannot disable checksums unilaterally. A `NO_CHECKSUM` frame before agreement
   is a protocol violation.
3. Frames without the `NO_CHECKSUM` flag remain subject to checksum validation.

## Correlation

`request_id` correlates `REQUEST`, `RESPONSE`, `BODY`, `END`, `CALL`, `RETURN`, `JOB`, and
`ACK`. Other message types are unsolicited and must use `request_id = 0`.

The server allocates request IDs starting at 1 and increments the value for each request.
Zero is reserved. After `u32::MAX`, the counter wraps to 1.

Call IDs are allocated downward from `u32::MAX`, keeping them separate from the
server-allocated request ID range until the ranges meet.

## Exchange sequences

### Handshake, once at boot

```
server  ──HELLO (version)──────────────────►  worker
        ◄─HELLO_ACK (version, php, sapi, pid)─
```

Volucer validates the worker's `protocol_version` during the handshake at startup. A
version mismatch therefore fails before the worker receives requests.

The `runtime` field identifies the implementation and build, for example
`volucer-worker-php/7c6945f`. It is distinct from the protocol version: the protocol version
changes with the wire format, while the runtime value identifies a particular SDK build.

### Serving a request

```
server  ──REQUEST (id, metadata, body_len)─►  worker
        ──BODY (id, chunk)*────────────────►
        ◄─RESPONSE (id, status, headers, body_len)─
        ◄─BODY (id, chunk)*, last flagged LAST─────
        ◄─END (id, app_status, rss, duration)──────
```

`app_status` in `END` is the application's status, not the HTTP response status. For
example, a worker may report `app_status = 1` after an exception while the response head
contains HTTP `status = 500`.

`END` also carries `peak_memory_bytes` and `keep_alive`. The supervisor can use the memory
value to recycle workers above a configured threshold. A worker that encounters a fatal
error sets `keep_alive = false`, so it is replaced rather than reused.

### A call, mid-request

```
server  ──REQUEST (id, …)──────────────────►  worker
        ◄─CALL (call_id, method, params)────     ← the worker asks the server for something
        ──RETURN (call_id, ok, result)──►         ← the server answers
        ◄─RESPONSE (id, status, …)────────
```

Workers may send `CALL` only while handling a request. Between requests, the server's worker
reader is waiting for the next request and cannot process a call. Both sides enforce this
restriction.

The server must answer every call with `RETURN`, including calls that fail. Dropping a call
would leave the worker waiting for a response until the request deadline.

### A job

```
server  ──JOB (id, pipeline, task, headers, body_len)──►  worker
        ──BODY (id, chunk)*─────────────────────────────►
        ◄─ACK (id, outcome, reason?)────────────────────────
```

Jobs use a distinct `JOB` frame rather than being represented as a kind of `REQUEST`.
Request workers reset request-specific state between requests; consumers may retain state
between jobs. A separate frame type makes the kind of work explicit to each implementation.

`ACK` is the terminal frame for a job exchange. Its payload determines how the server
handles the job:

| `outcome` | The queue does | Why |
|---|---|---|
| `ack` | Settles the reservation | The job is done |
| `requeue` | Negatively acknowledges, so the job is handed out again | Not done, and it should be retried |
| `reject` | **Settles the reservation without recording a failure** | Not done, and it must not come back |

`reject` settles the reservation without retrying the job. The server does not impose a
retry limit, backoff, or dead-letter policy. The driver increments `attempts` on each
reservation, allowing the application to implement its own retry policy.

The reservation ID is not the job ID. The server holds the reservation and sends the job;
`ACK` is correlated using the frame's `request_id`.

### gRPC calls

A gRPC call is delivered in a `REQUEST` frame. Volucer does not parse gRPC messages; the
request body contains the length-delimited gRPC message bytes.

Both sides identify gRPC requests by their `content-type`. The worker uses the content type
from the request it receives, matching the server's classification.

The gRPC status and outcome are sent in the trailer section, and the HTTP status is `200`.
gRPC clients read the result from the trailers rather than from an HTTP error status.

### Shutdown

When Volucer stops a worker, it closes the child's stdin. The worker reads EOF and exits
cleanly; it does not need to handle a signal or poll for a shutdown flag.

## Implementing a worker

When implementing a worker, ensure that it:

1. Uses stdout only for protocol frames. Capture application output or direct it elsewhere;
   the PHP SDK uses an output buffer and `display_errors=stderr`.
2. Completes the handshake first, checking the protocol version and reporting its runtime.
3. Handles one request at a time; Volucer assigns a worker to one request at a time.
4. Reads bodies as raw bytes, using `body_len` and the `LAST` flag.

Read the [normative specification](https://github.com/sailantis/volucer/blob/main/docs/worker-protocol.md)
before implementing a worker. It defines the exact wire format; the existing SDKs provide
independent implementations for reference.
