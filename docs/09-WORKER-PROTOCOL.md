# The Worker Protocol

**The wire format between volucer and a PHP worker.**

**Version 1.** The normative specification is
[`docs/worker-protocol.md`](https://github.com/sailantis/volucer/blob/main/docs/worker-protocol.md)
in the server repository, and `crates/volucer-worker` is its reference implementation. The
tests in that crate pin the bytes exactly.

**Four implementations exist**, and each pins the same frozen bytes rather than its own: the
Rust reference, [`sailantis/volucer-php-sdk`](https://github.com/sailantis/volucer-php-sdk),
[`sailantis/volucer-python-sdk`](https://github.com/sailantis/volucer-python-sdk) and
[`sailantis/volucer-ruby-sdk`](https://github.com/sailantis/volucer-ruby-sdk). Each lives in
its own repository because its package manager needs a manifest at the root, and a release of
each links back to the specification at a pinned commit.

**You probably do not need this page.** If you are writing a worker in PHP, use the SDK. If
you are writing one in another language, read the specification — this page is the summary
that tells you what to look for.

## Why it exists

Worker mode boots a PHP application once and serves many requests from it. That needs a
channel the child can talk back on, and neither of the obvious candidates works:

- **FastCGI cannot carry it.** FastCGI is server-driven and half-duplex: the worker can only
  answer the request it was handed. It has no way to push a log line, a metric, or a
  "recycle me" notice — and worker mode is exactly the case where the worker needs to talk
  first.
- **goridge is a framing codec, not a protocol.** Adopting its 12-byte header saves the easy
  part while importing a versioning commitment to its wire format; adopting its semantics
  means becoming a RoadRunner clone.

So volucer models the same proven header shape and owns a message set.

## Transport

The channel is the child process's **stdin/stdout pipe** by default. That is portable, needs
no port per worker, and cannot exhaust sockets. `stderr` is *not* part of the protocol: it
is free for the worker to write to, and the supervisor drains it so the pipe buffer cannot
fill and block the child.

```
  volucer ──HELLO/REQUEST/CONTROL/PING/CALL──►  php worker  (stdin)
            ◄─HELLO_ACK/RESPONSE/BODY/RETURN ──  … ──LOG/METRIC/EVENT─  (stdout)
```

`transport = "socket"` moves the same frames onto a socket. **Nothing about the protocol
changes** — the frames, the handshake and the ordering are identical; only the file
descriptor they travel on differs.

The server picks the socket and tells the worker where it is, in `VOLUCER_WORKER_SOCKET`:

| Value | Transport |
|---|---|
| `127.0.0.1:34567` | loopback TCP, an ephemeral port the server bound |
| `unix:/run/volucer/worker.0.sock` | a unix socket, from the backend's `socket` key |

A **named pipe** is deliberately not offered for the worker transport: the protocol needs the
connection as a reader and a writer at once, and a Windows pipe's state is per connection, so
it cannot be duplicated into two halves.

The protocol is **symmetric**: the same codec reads what it wrote, so an SDK mirrors the
specification rather than inverting it.

## Frame format

Every frame is a 16-byte header followed by exactly `payload_len` payload bytes.

```
offset  size  field
0       4     payload_len        payload bytes, excluding this header
4       1     protocol_version   must be 1
5       1     message_type       1..=18, see below
6       2     flags              see below
8       4     request_id         0 = unsolicited / control
12      4     crc32c(payload)    CRC-32C, see below
```

All multi-byte integers are **little-endian**.

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

A frame declaring more than `MAX_PAYLOAD_LEN` is rejected **before** allocating.

## Message types

The numbering is part of the protocol. These values must never be reordered or reused.

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

An unknown type number is rejected rather than skipped: skipping it would silently misalign
the reader from the sender's expectations.

### Which types each side may receive

A worker handles `HELLO`, `REQUEST`, `JOB`, `PING`, `CONTROL` and `RETURN`. It **originates**
`CALL`, `ACK` and never receives one. It does not handle `RESPONSE`, `BODY`, `END`,
`HELLO_ACK`, `CONTROL_ACK`, `PONG` or `ERROR` — a server sending any of those is a protocol
error, and both the Rust codec and the PHP SDK refuse them rather than guessing.

The server is the mirror image: it **originates** `REQUEST`, `JOB`, `HELLO`, `CONTROL` and
`PING`, **answers** `CALL` with a `RETURN`, and treats a `CALL` arriving *at* it as a protocol
error.

**The one sentence to remember:** a call travels from the side that has a question to the
side that has the answer, and in this protocol the application is the side with the questions.

## Body framing

A body is zero or more `BODY` frames in the direction of travel, all sharing the request's
id. Two invariants apply, and a reader must enforce both:

1. **`body_len` is authoritative.** The receiver knows how much to expect before the first
   byte arrives, and a short body is *detected* rather than hanging.
2. **The last `BODY` frame carries `LAST`.** A cheap second check that catches a stream that
   has desynchronised in a way the length alone would not.

A body of zero length sends **no** `BODY` frames at all.

Bodies are never JSON. Base64 inside JSON inflates an upload by a third and makes the runtime
do work per byte on the hot path, so bytes travel as bytes.

## Flags

| Bit | Name | Meaning |
|---|---|---|
| `0x0001` | `COMPRESSED` | Reserved. No codec is negotiated in v1, so a frame carrying this bit is **rejected** |
| `0x0002` | `CHUNKED` | The payload is one chunk of a larger logical message |
| `0x0004` | `LAST` | This is the last chunk of a logical message |
| `0x0008` | `ERROR` | The sender is reporting a failure |
| `0x0010` | `NO_CHECKSUM` | The `crc32c` field is **not** a checksum and must not be verified |

`NO_CHECKSUM` is a **flag rather than a zero checksum**, because zero is already a real
checksum: `crc32c("") == 0`, which every empty-payload frame carries.

## Checksum

The header carries **CRC-32C (Castagnoli)**: polynomial `0x82F63B78`, reflected, initial and
final value `!0`. This is the "crc32c" algorithm, *not* the "crc32b" one that zlib and PHP's
plain `crc32()` use.

```
crc32c("123456789") == 0xE3069283
crc32c("")          == 0x00000000
```

The same value is what `hash('crc32c', '123456789')` produces in PHP, which is why the PHP SDK
can use the built-in hash rather than reimplementing the polynomial.

A mismatch means framing has already desynchronised, so the stream is unrecoverable and the
worker must be discarded.

### Turning it off

The checksum is optional and negotiated:

- The server may set `no_checksum: true` in its `HELLO`, which is an **offer**.
- The worker **answers** with `no_checksum` in its `HELLO_ACK`, `true` only if it understood
  the field and agrees.
- Only when both said `true` does either end stop hashing.

Three properties are deliberate:

1. **The handshake is always checksummed.** The agreement is carried *in* frames, so the
   frames that carry it must themselves be verified.
2. **A sender cannot turn the check off by itself.** A frame claiming `NO_CHECKSUM` on an end
   that never agreed is a protocol violation, and is refused as such.
3. **A frame that says it is checksummed still is.** The policy relaxes the check only for
   frames that opt out.

## Correlation

`request_id` is meaningful for `REQUEST`, `RESPONSE`, `BODY`, `END`, `CALL`, `RETURN`, `JOB`
and `ACK`. Every other type is unsolicited and **must** carry `request_id = 0`.

Request ids are allocated by the server, start at **1**, and increment per request. **`0` is
reserved** and never used as a request id, so a correlation bug is visible instead of silently
matching. The counter wraps at `u32::MAX` back to 1, skipping 0.

**Call ids are allocated downward from `u32::MAX`**, so the two ranges meet only after four
billion requests and a mix-up between a call and a request is a visible correlation failure
rather than a silent match.

## Exchange sequences

### Handshake, once at boot

```
server  ──HELLO (version)──────────────────►  worker
        ◄─HELLO_ACK (version, php, sapi, pid)─
```

The worker's `protocol_version` is checked **here**, at boot, not on the first request: a
mismatched SDK is a configuration problem and should look like one.

`runtime` identifies the implementation and the **build** it is running, as
`volucer-worker-php/7c6945f`. The build is not the protocol version: the protocol is `1` and
moves only when the wire format does, while the build names one particular checkout or
release of the SDK.

### Serving a request

```
server  ──REQUEST (id, metadata, body_len)─►  worker
        ──BODY (id, chunk)*────────────────►
        ◄─RESPONSE (id, status, headers, body_len)─
        ◄─BODY (id, chunk)*, last flagged LAST─────
        ◄─END (id, app_status, rss, duration)──────
```

`app_status` in `END` is the **application's** exit status and is **not** the HTTP status. A
worker that throws can report `app_status = 1` while the response head still carries
`status = 500`.

`END` also carries `peak_memory_bytes`, so the supervisor can recycle a worker on an RSS
threshold, and `keep_alive`. A worker that handled a fatal error reports `keep_alive = false`
and is replaced instead of reused.

### A call, mid-request

```
server  ──REQUEST (id, …)──────────────────►  worker
        ◄─CALL (call_id, method, params)────     ← the worker asks the server for something
        ──RETURN (call_id, ok, result)──►         ← the server answers
        ◄─RESPONSE (id, status, …)────────
```

**A worker may only call while a request is in flight.** The server has one reader per
worker, parked on `recv()` *between* requests — so a call made when the worker is idle has no
reader to answer it and would block until the request deadline. Both sides enforce this.

Calls must be answered even when they cannot be served — a `RETURN` carrying an error, never
a dropped frame — because dropping one leaves the worker blocked on a read that will not
return until the request's deadline kills it.

### A job

```
server  ──JOB (id, pipeline, task, headers, body_len)──►  worker
        ──BODY (id, chunk)*─────────────────────────────►
        ◄─ACK (id, outcome, reason?)────────────────────────
```

A job is a **separate exchange with its own frame type**, and the reason is the one thing a
consumer and a request handler must not share: **per-request state reset.** A request handler
resets what the last request left behind. A consumer does the *opposite*: boot once, keep the
connection pool, the container and the warm caches.

That difference cannot be expressed by a flag on `REQUEST` without making "is this a request
or a job?" a question each implementation answers for itself, which is how one of them gets
it wrong.

`ACK` is the terminal frame and the only place in this protocol where a payload **decides
something the server does** rather than something it reports:

| `outcome` | The queue does | Why |
|---|---|---|
| `ack` | Settles the reservation | The job is done |
| `requeue` | Negatively acknowledges, so the job is handed out again | Not done, and it should be retried |
| `reject` | **Settles the reservation without recording a failure** | Not done, and it must not come back |

`reject` is the arm worth reading twice. Nacking it would bring the job back for ever, and
the server imposes no retry count, no backoff and no dead-lettering. What makes the policy
writable is that the driver increments `attempts` on every reservation, so an application
that wants "give up after three" reads the number it is given.

The reservation is deliberately **not** the job id. The server holds the reservation and sends
only the job; `ACK` correlates by the frame's `request_id`.

### gRPC calls

A gRPC call is delivered as an ordinary `REQUEST`, and this is a decision rather than a
shortcut: **the server never parses a gRPC message**, so a call is a request whose body
happens to be length-delimited framed messages instead of a form.

What tells the two apart is `content-type`, on **both** sides. A worker identifies a call by
the media type of the request it just read, exactly as the server did before delivering it.

The outcome travels in the *trailer* section, and the status is `200` — because a gRPC client
reads the outcome from the trailer section, so an HTTP error status would be invisible to it.

### Shutdown

When the server wants the worker to stop it closes the child's stdin. The worker reads that
as a clean EOF and exits on its own. There is **no signal to catch** and no flag to poll.

## Implementing a worker

The four things to get right, in order:

1. **stdout is the protocol channel.** Capture your application's output rather than writing
   it there. The PHP SDK does this with an output buffer and `display_errors=stderr`; another
   language needs its own equivalent.
2. **The handshake comes first.** Check the protocol version, and answer with your runtime
   identification.
3. **One request at a time.** The server leases a worker to one request.
4. **A body is bytes, not JSON.** Read `body_len` bytes, honour the `LAST` flag.

Read the [normative specification](https://github.com/sailantis/volucer/blob/main/docs/worker-protocol.md)
before implementing. It pins the exact bytes, and the four existing SDKs are four
independent implementations of it.
