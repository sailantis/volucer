//! The message payloads, and the JSON codec behind them.
//!
//! Payloads are JSON in version 1. That choice is behind [`Codec`] so the wire
//! framing never has to change if profiling ever justifies msgpack: the header,
//! the checksum and the framing are payload-agnostic by construction.
//!
//! Bodies are deliberately **not** JSON. A request's body travels in [`Body`]
//! frames as raw bytes, because base64 inside JSON inflates an upload by a third
//! and makes the runtime do work per byte on the hot path.
//!
//! The field names here are part of the protocol, because the PHP SDK decodes
//! the same JSON. Renaming a field is a protocol change.

use serde::{Deserialize, Serialize};

/// The server's opening frame: the protocol version, and the policies it wants used.
///
/// A dedicated struct rather than the `RequestHeader` this used to reuse. Two reasons: a
/// `RequestHeader` travels on *every* request, so a policy field there would add bytes to
/// the hot path, and a `HELLO` carrying request-shaped JSON read as though a request were
/// being described. The field names are part of the protocol, because the PHP SDK decodes
/// the same JSON.
///
/// `no_checksum` defaults to `false`, which is the fail-safe direction: a peer that does not
/// know the field keeps verifying checksums, and a server that says nothing means "required".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    /// The protocol version the server speaks.
    ///
    /// Previously absent, because the reused `RequestHeader` had no such field and the SDK
    /// defaulted it to its own. Stating it makes a mismatched pair visible at the first frame.
    pub protocol_version: u8,
    /// Whether the worker may omit the per-frame checksum.
    #[serde(default)]
    pub no_checksum: bool,
}

/// The PHP and volucer versions a worker reports at handshake.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelloAck {
    /// The worker's own protocol version, so a mismatched SDK is caught at
    /// handshake rather than on the first request.
    pub protocol_version: u8,
    /// `PHP_VERSION`.
    #[serde(default)]
    pub php_version: String,
    /// `PHP_SAPI`.
    #[serde(default)]
    pub sapi: String,
    /// The worker process id, for logging.
    #[serde(default)]
    pub pid: u32,
    /// What the worker's bootstrap reported, free-form and for logs only.
    #[serde(default)]
    pub runtime: String,
    /// Whether the worker agreed to omit the per-frame checksum.
    ///
    /// The worker's *answer*, not its preference: a server that requires checksums will not
    /// have offered, so an absent or `false` value means the worker is checksumming. Recorded
    /// so the pool can see what a worker actually agreed to rather than assuming it followed.
    #[serde(default)]
    pub no_checksum: bool,
}

/// What the server tells a worker when asking it to serve a request.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestHeader {
    pub method: String,
    /// The full request target, e.g. `/index.php/foo?x=1`.
    pub uri: String,
    #[serde(default)]
    pub protocol: String,
    /// Request headers, preserving order and case as received.
    #[serde(default)]
    pub headers: Vec<(String, String)>,
    /// Client address, for `REMOTE_ADDR`.
    #[serde(default)]
    pub remote_addr: String,
    /// How many body bytes follow in `Body` frames, so the worker can decide
    /// whether to buffer or stream before reading a single one.
    #[serde(default)]
    pub body_len: u64,
    /// The absolute script the server routed to.
    #[serde(default)]
    pub script_filename: String,
    /// Whether the request arrived over TLS.
    #[serde(default)]
    pub https: bool,
}

impl RequestHeader {
    /// Look up a header case-insensitively, the way HTTP requires.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    /// The content type, if the client sent one.
    pub fn content_type(&self) -> Option<&str> {
        self.header("content-type")
    }

    /// A borrowed view of this header, with `body_len` supplied by the caller.
    ///
    /// Every field is borrowed, so this allocates nothing. It exists so an owned
    /// `RequestHeader` can be framed without cloning it, and so the frame's shape has one
    /// definition: the `body_len` that matters is the length of the body about to be framed,
    /// which the caller holds and this type cannot see.
    pub fn draft(
        &self,
        body_len: u64,
    ) -> crate::codec::RequestHeaderJson<'_, crate::codec::HeaderPairs<'_>> {
        crate::codec::RequestHeaderJson {
            method: &self.method,
            uri: &self.uri,
            protocol: &self.protocol,
            headers: crate::codec::BorrowedHeaders::new(crate::codec::HeaderPairs(
                self.headers.iter(),
            )),
            remote_addr: &self.remote_addr,
            body_len,
            script_filename: &self.script_filename,
            https: self.https,
        }
    }
}

/// The response head a worker sends before its body.
///
/// `Default` is hand-written rather than derived so it agrees with the serde
/// defaults below: an omitted field and a constructed default must not mean
/// different things.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResponseHead {
    /// HTTP status code.
    #[serde(default = "default_status")]
    pub status: u16,
    /// Response headers, in the order the application produced them.
    #[serde(default)]
    pub headers: Vec<(String, String)>,
    /// How many body bytes follow in `Body` frames.
    #[serde(default)]
    pub body_len: u64,
}

impl Default for ResponseHead {
    fn default() -> Self {
        ResponseHead {
            status: default_status(),
            headers: Vec::new(),
            body_len: 0,
        }
    }
}

fn default_status() -> u16 {
    200
}

/// The terminal message for a request: how it went, and what it cost.
///
/// `app_status` is the *application's* exit status and is **not** the HTTP
/// status. A worker that throws can report `app_status = 1` while the response
/// head still carries `status = 500`; conflating the two is how a protocol ends
/// up unable to express a failed request that produced a valid error page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestEnd {
    /// The application's own status. Non-zero is normal and not a protocol
    /// failure.
    #[serde(default)]
    pub app_status: i32,
    /// The worker's peak resident set in bytes, so the pool can recycle on a
    /// threshold without having to ask the operating system.
    #[serde(default)]
    pub peak_memory_bytes: u64,
    /// How long the application spent handling the request, in microseconds.
    /// Measured by the worker, so it excludes volucer's own overhead and makes
    /// the two comparable when profiling.
    #[serde(default)]
    pub duration_us: u64,
    /// Whether this worker is willing to serve another request. A worker that
    /// handled a fatal error reports `false` and is replaced instead of reused.
    #[serde(default = "default_true")]
    pub keep_alive: bool,
}

/// Hand-written for the same reason as [`ResponseHead::default`]: a constructed
/// default must not contradict the serde default for an omitted field.
impl Default for RequestEnd {
    fn default() -> Self {
        RequestEnd {
            app_status: 0,
            peak_memory_bytes: 0,
            duration_us: 0,
            keep_alive: default_true(),
        }
    }
}

fn default_true() -> bool {
    true
}

/// A log record emitted by a worker, outside any request.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogRecord {
    /// RFC 5424-ish level name: `emergency`, `alert`, `critical`, `error`,
    /// `warning`, `notice`, `info`, `debug`.
    #[serde(default)]
    pub level: String,
    #[serde(default)]
    pub message: String,
    /// The request this log line belongs to, when it happened inside one.
    #[serde(default)]
    pub request_id: u32,
}

/// A metric sample.
///
/// `PartialEq` only — an `f64` cannot be `Eq` (`NaN != NaN`), and a metric is a
/// measurement rather than an identifier, so exact equality is not a
/// meaningful claim to make about it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MetricRecord {
    pub name: String,
    /// Kept as `f64` because a metric that could only be an integer would be
    /// useless for latency and memory ratios.
    pub value: f64,
    #[serde(default)]
    pub unit: String,
}

/// A control message from the server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Control {
    /// Stop accepting new work and exit once the current request finishes.
    Drain,
    /// Exit immediately; the current request is lost.
    Shutdown,
    /// Re-run the worker's bootstrap without exiting, for a boot cache.
    Reload,
    /// Ask the worker to report its memory usage.
    Report,
}

/// A call the server makes to a running worker.
///
/// **The request is the method and its parameters, and the server never interprets either.** A call
/// is `"<namespace>.<verb>"` — `kv.get`, `lock.acquire`, `metrics.observe` — and the params are JSON
/// the namespace's handler understands. Keeping the payload opaque here is what lets the frame layer
/// stay ignorant of what a KV store is: the exchange thread hands the call to a handler and writes
/// back whatever comes out.
///
/// `name` rather than `method` because `method` is already the HTTP verb on [`RequestHeader`], and two
/// fields called `method` in one protocol is how a reader comes to believe a call carries an HTTP
/// request.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Call {
    /// `"<namespace>.<verb>"`, e.g. `kv.get`.
    pub name: String,
    /// The parameters, as the namespace's handler expects them. JSON, and opaque here.
    #[serde(default)]
    pub params: serde_json::Value,
    /// The `traceparent` this call belongs to, so a span can continue across the call.
    ///
    /// Carried from the start rather than added later, because it costs a field now and is a protocol
    /// change once anything is deployed. `docs/plugins.md` §6.5 is where that argument lives.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub traceparent: Option<String>,
}

/// The worker's answer to a [`Call`].
///
/// **An error is a successful `Return` with `ok: false`**, not an `ERROR` frame. The distinction
/// matters: an `ERROR` frame describes a protocol failure and can be unrecoverable, while a call that
/// could not be served — an unknown method, a backend that timed out, a lock whose state is unknown —
/// is an ordinary answer to an ordinary question. Sending `ERROR` for those would make a missing KV
/// entry look like a broken worker.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Return {
    /// Whether the call was served.
    pub ok: bool,
    /// The result, when `ok`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    /// Why it was not, when not `ok`. Free-form, and read by a human.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Whether the failure left the call's effect in an unknown state.
    ///
    /// **A third answer, not a boolean pair.** `ok: false` with `unknown: false` means nothing
    /// happened — an unknown method, a value that is not there. `ok: false` with `unknown: true` means
    /// the call may or may not have taken effect: a lock acquired before the reply was lost, a write
    /// that was sent and not acknowledged. The caller must resolve that by asking, and an API that
    /// collapsed the two would make "the write failed" and "the write may have happened" the same
    /// answer — which is how a retry duplicates a side effect. This is the ambiguity `docs/plugins.md`
    /// §6.3 records RoadRunner's own lock implementation as having, modelled explicitly rather than
    /// left to a comment.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub unknown: bool,
}

impl Return {
    /// A served call.
    pub fn ok(result: serde_json::Value) -> Self {
        Return {
            ok: true,
            result: Some(result),
            error: None,
            unknown: false,
        }
    }

    /// A call that did not happen.
    pub fn refused(reason: impl Into<String>) -> Self {
        Return {
            ok: false,
            result: None,
            error: Some(reason.into()),
            unknown: false,
        }
    }

    /// A call whose effect cannot be determined.
    pub fn unknown_state(reason: impl Into<String>) -> Self {
        Return {
            ok: false,
            result: None,
            error: Some(reason.into()),
            unknown: true,
        }
    }
}

/// A job handed to a consumer, for the payload of a [`crate::MessageType::Job`].
///
/// # Why this is not a `RequestHeader`
///
/// The two look alike — an id, a name, a body — and sharing one struct would have saved this type. It
/// would also have made "is this an HTTP request or a job?" a question answered by which fields happen
/// to be non-empty, which is exactly the ambiguity [`crate::MessageType::Job`] exists to remove. The
/// three fields below are the ones RoadRunner's own consumer receives: a task name, a string header
/// bag, and an opaque payload.
///
/// **The payload travels in `BODY` frames, not here**, exactly as a request body does and for the same
/// reason: base64 inside JSON inflates an upload by a third and makes the runtime do work per byte.
/// `body_len` is the declaration that lets the consumer size its read before a byte of it arrives.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobHeader {
    /// The pipeline this job came from, so a consumer can tell two queues apart in a log.
    pub pipeline: String,
    /// The driver's id for the job, unique within its pipeline.
    ///
    /// Worth sending separately from the task name because it is what an application writes down when
    /// it wants to find this job again in a dashboard, and because the reservation that acknowledges it
    /// is *not* derivable from it — see [`crate::MessageType::Ack`].
    pub id: String,
    /// The application's task name, e.g. `App\Jobs\SendEmail`. Opaque to the server.
    pub task: String,
    /// Application metadata, in the order the driver stored it. **String values only.**
    #[serde(default)]
    pub headers: Vec<(String, String)>,
    /// How many times this job has been reserved, counting this one.
    ///
    /// The number an application's retry policy reads. The server neither sets a limit nor acts on it:
    /// see `docs/plugins.md` §6.4 for why retry, backoff and dead-lettering stay the application's.
    #[serde(default)]
    pub attempts: u32,
    /// How many payload bytes follow in `BODY` frames.
    #[serde(default)]
    pub body_len: u64,
    /// The `traceparent` this job belongs to, so a job's span continues the producer's trace.
    ///
    /// A field rather than a reserved header, because headers are the *application's* and a server that
    /// wrote a key into them would be one rename away from corrupting an application that used it. Same
    /// argument as [`Call::traceparent`], and the same field name so both directions read alike.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub traceparent: Option<String>,
}

impl JobHeader {
    /// Look up a header case-insensitively.
    ///
    /// Case-insensitive because these are PSR-7-shaped headers and the SDK exposes them that way, so
    /// a consumer that reads `X-Retry` and a producer that wrote `x-retry` must agree.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

/// What a consumer decided about a job: the payload of a [`crate::MessageType::Ack`].
///
/// # Why this is one frame and not three
///
/// RoadRunner's consumer protocol has five answer types — `NoError`, `Error`, `ACK`, `NACK`, `REQUEUE`
/// — and three of them are the same decision written differently. What the queue can actually do with
/// an answer is exactly three things, so this is a three-variant enum and the frame type says which:
///
/// - [`JobOutcome::Ack`] — done, forget it. The server acknowledges the reservation.
/// - [`JobOutcome::Requeue`] — not done, and it should be tried again. The server negatively
///   acknowledges, which returns the job to its pipeline.
/// - [`JobOutcome::Reject`] — not done, and it should **not** be tried again. The server acknowledges
///   the reservation without a failure, because the alternative is a job that comes back for ever.
///
/// # Why the outcome is not the `ok` of a `Return`
///
/// Because "the consumer failed" and "the job failed" are different facts and the queue only acts on
/// the second. A consumer whose handler threw has produced a failure *result*; whether the job should
/// run again is a decision only the application can make — the same reason the server imposes no retry
/// policy at all. The two are separate fields here so that a consumer can report a reason without the
/// reason implying a requeue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum JobOutcome {
    /// The job was handled. The reservation is acknowledged.
    Ack,
    /// The job was not handled and should be handed out again.
    ///
    /// **The server does not delay it.** A retry delay is the application's — RoadRunner's consumer can
    /// say `withDelay`, and the honest way to offer that here is for the application to push a new job
    /// rather than for the server to hold a reservation open, because a held reservation is
    /// indistinguishable from a consumer that crashed.
    Requeue {
        /// Why it was requeued, for the log. Free-form, and read by a human.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    /// The job failed and must not be retried. The reservation is acknowledged rather than nacked,
    /// because a job that has been decided against is not a job that is waiting.
    Reject {
        /// Why, for the log.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
}

impl JobOutcome {
    /// The name of this outcome, as the protocol document and the SDK spell it.
    ///
    /// An associated function rather than a `Display` impl, so a log line can name an outcome as a
    /// `&'static str` without allocating.
    pub fn name(&self) -> &'static str {
        match self {
            JobOutcome::Ack => "ack",
            JobOutcome::Requeue { .. } => "requeue",
            JobOutcome::Reject { .. } => "reject",
        }
    }

    /// The reason, if the consumer gave one.
    pub fn reason(&self) -> Option<&str> {
        match self {
            JobOutcome::Ack => None,
            JobOutcome::Requeue { reason } | JobOutcome::Reject { reason } => reason.as_deref(),
        }
    }
}

/// The terminal frame of a `JOB` exchange: what the consumer decided, and what it cost.
///
/// # Why this wraps [`JobOutcome`] rather than being it
///
/// Because a job has no `END` frame, and the three things `END` carries are the three things the
/// pool recycles on: `peak_memory_bytes` against `memory_limit_mb`, `keep_alive` for a consumer that
/// asks to be replaced, and `duration_us` for the profile. A job exchange that reported only the
/// outcome would make `[jobs.pool] memory_limit_mb` a setting the configuration accepts and nothing
/// reads — which is worse than not offering it, because an operator would have no way to tell.
///
/// Flattened rather than nested for a wire reason: the frame already says it is an `ACK`, so an
/// envelope inside it would be a second copy of the same fact, and flattening keeps the JSON
/// `{"outcome":"ack","peak_memory_bytes":…}` rather than adding a level that every reader has to
/// traverse. Adding these fields is additive in the protocol's sense — a consumer that does not know
/// them sends an object the server reads with defaults, and the pool then recycles on memory limits it
/// cannot see, which is exactly what a worker that does not report does today.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ack {
    /// Whether the job was handled, should be retried, or was given up on.
    ///
    /// The only field that decides something the *server* does; the rest are what it reports.
    #[serde(flatten)]
    pub outcome: JobOutcome,
    /// The consumer's peak resident set in bytes, for `memory_limit_mb`.
    ///
    /// Per job and not per process, for the reason `RequestEnd::peak_memory_bytes` documents: the SDK
    /// resets PHP's high-water mark after reading it, so this describes the job that just finished.
    #[serde(default)]
    pub peak_memory_bytes: u64,
    /// Whether this consumer is willing to handle another job.
    ///
    /// A consumer that reports `false` is replaced rather than reused — the same signal as
    /// `RequestEnd::keep_alive`, and the mechanism an application uses when it has put itself into a
    /// state it cannot clean up. **Defaulting to `true`**, so a consumer that says nothing is reused:
    /// the fail-safe direction is the one that does not silently recycle a worker on every job.
    #[serde(default = "default_true")]
    pub keep_alive: bool,
    /// How long the application spent handling the job, in microseconds.
    #[serde(default)]
    pub duration_us: u64,
}

impl Default for Ack {
    fn default() -> Self {
        Ack {
            outcome: JobOutcome::Ack,
            peak_memory_bytes: 0,
            keep_alive: default_true(),
            duration_us: 0,
        }
    }
}

/// A description of a failure, used in `Error` frames.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorReport {
    #[serde(default)]
    pub message: String,
    /// The request the failure belongs to, when it is not the frame's own id.
    #[serde(default)]
    pub request_id: u32,
    /// Whether the worker considers itself usable afterwards.
    #[serde(default)]
    pub recoverable: bool,
}

/// A payload encoder. JSON today; the indirection is the extension point.
pub trait Codec {
    fn encode<T: Serialize>(value: &T) -> crate::Result<Vec<u8>>;
    /// The same encoding, into a buffer the caller owns and will reuse.
    ///
    /// The request frame is encoded once per request, and the channel that sends it lives
    /// for many requests, so a `Vec` per request is pure churn: the bytes are handed to
    /// [`write_frame`] and dropped immediately. The buffer is cleared, not shrunk, so the
    /// capacity outlives the request that needed it.
    fn encode_into<T: Serialize + ?Sized>(out: &mut Vec<u8>, value: &T) -> crate::Result<()>;
    fn decode<T: for<'de> Deserialize<'de>>(bytes: &[u8]) -> crate::Result<T>;
}

/// The JSON codec, which is what version 1 negotiates.
#[derive(Debug, Clone, Copy, Default)]
pub struct Json;

impl Codec for Json {
    fn encode<T: Serialize>(value: &T) -> crate::Result<Vec<u8>> {
        Ok(serde_json::to_vec(value)?)
    }

    fn encode_into<T: Serialize + ?Sized>(out: &mut Vec<u8>, value: &T) -> crate::Result<()> {
        out.clear();
        serde_json::to_writer(std::io::Cursor::new(&mut *out), value)?;
        Ok(())
    }

    fn decode<T: for<'de> Deserialize<'de>>(bytes: &[u8]) -> crate::Result<T> {
        Ok(serde_json::from_slice(bytes)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_header_round_trips_and_finds_headers_case_insensitively() {
        let header = RequestHeader {
            method: "POST".to_string(),
            uri: "/index.php?x=1".to_string(),
            protocol: "HTTP/1.1".to_string(),
            headers: vec![("Content-Type".to_string(), "application/json".to_string())],
            remote_addr: "127.0.0.1".to_string(),
            body_len: 17,
            script_filename: "/srv/app/public/index.php".to_string(),
            https: true,
        };

        let encoded = Json::encode(&header).unwrap();
        let decoded: RequestHeader = Json::decode(&encoded).unwrap();
        assert_eq!(decoded, header);
        assert_eq!(decoded.header("content-type"), Some("application/json"));
        assert_eq!(decoded.content_type(), Some("application/json"));
        assert!(decoded.header("absent").is_none());
    }

    #[test]
    fn a_missing_content_type_is_none_not_an_empty_string() {
        let header = RequestHeader::default();
        assert_eq!(header.content_type(), None);
    }

    #[test]
    fn a_response_without_a_status_defaults_to_200() {
        let decoded: ResponseHead = Json::decode(b"{}").unwrap();
        assert_eq!(decoded.status, 200);
        assert!(decoded.headers.is_empty());
    }

    #[test]
    fn a_missing_keep_alive_means_keep_alive() {
        // The permissive default is deliberate: a worker that omits the field
        // is not asking to be replaced.
        let end: RequestEnd = Json::decode(b"{}").unwrap();
        assert!(end.keep_alive);
        assert_eq!(end.app_status, 0);
    }

    #[test]
    fn a_non_zero_app_status_survives_the_round_trip() {
        let end = RequestEnd {
            app_status: -1,
            peak_memory_bytes: 1024,
            duration_us: 42,
            keep_alive: false,
        };
        let decoded: RequestEnd = Json::decode(&Json::encode(&end).unwrap()).unwrap();
        assert_eq!(decoded, end);
    }

    #[test]
    fn control_messages_are_tagged_by_kind() {
        let encoded = Json::encode(&Control::Drain).unwrap();
        assert_eq!(String::from_utf8(encoded).unwrap(), r#"{"kind":"drain"}"#);
        let decoded: Control = Json::decode(br#"{"kind":"reload"}"#).unwrap();
        assert_eq!(decoded, Control::Reload);
    }

    #[test]
    fn a_job_header_round_trips_and_finds_headers_case_insensitively() {
        let head = JobHeader {
            pipeline: "emails".to_string(),
            id: "1a-2".to_string(),
            task: "App\\Jobs\\SendEmail".to_string(),
            headers: vec![("traceparent".to_string(), "00-abc-01".to_string())],
            attempts: 3,
            body_len: 12,
            traceparent: Some("00-abc-01".to_string()),
        };
        let decoded: JobHeader = Json::decode(&Json::encode(&head).unwrap()).unwrap();
        assert_eq!(decoded, head);
        assert_eq!(decoded.header("TraceParent"), Some("00-abc-01"));
        assert_eq!(decoded.header("nope"), None);
    }

    #[test]
    fn a_job_outcome_is_tagged_by_outcome_and_spells_its_three_names() {
        let encoded = Json::encode(&JobOutcome::Ack).unwrap();
        assert_eq!(String::from_utf8(encoded).unwrap(), r#"{"outcome":"ack"}"#);
        let decoded: JobOutcome =
            Json::decode(br#"{"outcome":"reject","reason":"bad payload"}"#).unwrap();
        assert_eq!(
            decoded,
            JobOutcome::Reject {
                reason: Some("bad payload".to_string())
            }
        );
        assert_eq!(decoded.name(), "reject");
        assert_eq!(decoded.reason(), Some("bad payload"));
        assert_eq!(JobOutcome::Ack.name(), "ack");
        assert_eq!(JobOutcome::Ack.reason(), None);
    }

    #[test]
    fn a_job_outcome_without_a_reason_omits_the_field() {
        assert_eq!(
            String::from_utf8(Json::encode(&JobOutcome::Requeue { reason: None }).unwrap())
                .unwrap(),
            r#"{"outcome":"requeue"}"#
        );
    }

    #[test]
    fn a_log_record_round_trips() {
        let record = LogRecord {
            level: "warning".to_string(),
            message: "deprecated call".to_string(),
            request_id: 9,
        };
        let decoded: LogRecord = Json::decode(&Json::encode(&record).unwrap()).unwrap();
        assert_eq!(decoded, record);
    }

    #[test]
    fn a_metric_keeps_fractional_values() {
        let metric = MetricRecord {
            name: "latency_ms".to_string(),
            value: 1.5,
            unit: "ms".to_string(),
        };
        let decoded: MetricRecord = Json::decode(&Json::encode(&metric).unwrap()).unwrap();
        assert_eq!(decoded.value, 1.5);
    }

    #[test]
    fn invalid_json_is_an_error_not_a_panic() {
        let err = Json::decode::<RequestHeader>(b"{not json").unwrap_err();
        assert!(matches!(err, crate::WorkerError::Json(_)), "{err}");
        assert!(err.is_fatal());
    }

    #[test]
    fn a_hello_ack_only_needs_the_version() {
        let ack: HelloAck = Json::decode(b"{\"protocol_version\":1}").unwrap();
        assert_eq!(ack.protocol_version, 1);
        assert!(ack.php_version.is_empty());
    }
}
