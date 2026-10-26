//! What a span is, before anything decides how to spell it.
//!
//! # Why the span is defined here and not in the exporter
//!
//! Because the ids are already here, and they are the part that cannot be got wrong later.
//! [`crate::TraceContext`] mints the trace id and the span id for a request, and that same header is
//! put on the request a backend receives — so the id in this server's span is the id PHP's (or an
//! origin's) child span will name as its parent. An exporter that minted its own span id would
//! export a parent that no child points at, and the trace would break at exactly the hop it exists
//! to show.
//!
//! So the split is: **this module owns identity and shape**, `volucer-otlp` owns the encoding and
//! the transport. That also keeps this crate free of any telemetry dependency, which is what lets a
//! build with no `[telemetry]` table link nothing extra at all.
//!
//! # Attributes are a closed vocabulary, and that is a decision
//!
//! Every attribute key here is a `&'static str` and the set is written down in
//! [`Observability::record_server_span`](crate::Observability::record_server_span). This is the
//! same rule the metric registry enforces with its series cap and for the same reason: an attribute
//! whose *key* comes from the request is how a tracing backend's index grows without bound. Values
//! are bounded by what a request carries — a path, a method, a status — and that is the point of an
//! attribute.

use std::time::SystemTime;

/// W3C's `sampled` trace flag: bit 0 of the flags byte in a `traceparent` and in OTLP's `flags`.
///
/// A named constant rather than a bare `1` at each site, because it is bit 0 of a byte whose other
/// seven bits are reserved — a `|= 1` somewhere and a `0x01` somewhere else is how a shift ends up
/// on the wrong bit.
pub const TRACE_FLAG_SAMPLED: u8 = 0x01;

/// How a span relates to the work around it.
///
/// The numbers are OTLP's own `SpanKind`, written down here rather than left to a mapping in the
/// encoder, because a value that must survive a copy into a wire format is a good thing to have
/// exactly one definition of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpanKind {
    /// Work inside this process, with no boundary crossed.
    Internal,
    /// This server received a request.
    Server,
    /// This server made a request to something else.
    Client,
    /// This server pushed a job into a queue.
    Producer,
    /// This server took a job out of a queue.
    Consumer,
}

impl SpanKind {
    /// The OTLP `SpanKind` value.
    ///
    /// Note that `Internal` is **1**, not 0: the protocol reserves 0 for "unspecified" and says
    /// explicitly not to use it, so a mapping that let `Internal` fall to 0 would emit a span every
    /// backend is permitted to reinterpret.
    #[must_use]
    pub const fn otlp(self) -> u8 {
        match self {
            SpanKind::Internal => 1,
            SpanKind::Server => 2,
            SpanKind::Client => 3,
            SpanKind::Producer => 4,
            SpanKind::Consumer => 5,
        }
    }
}

/// How the work ended.
///
/// Two states rather than three, and the missing one is deliberate. OTLP has `Unset`, `Ok` and
/// `Error`, but the HTTP semantic conventions say a server span should be left `Unset` unless it
/// failed — `Ok` is for a span whose success a human explicitly marked, and setting it everywhere
/// makes it carry no information. So the choice here is between "failed" and "did not fail", and
/// the encoder maps the latter to `Unset`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpanStatus {
    /// The work completed without a failure this server observed.
    Unset,
    /// The work failed. The message is what a human reads in the trace view.
    Error,
}

impl SpanStatus {
    /// The OTLP `StatusCode` value.
    #[must_use]
    pub const fn otlp(self) -> u8 {
        match self {
            SpanStatus::Unset => 0,
            SpanStatus::Error => 2,
        }
    }

    /// Whether a response status is a failure, by the HTTP semantic convention's rule.
    ///
    /// **5xx only.** A 4xx is the client's mistake and is recorded as an attribute rather than as a
    /// span error, because marking it as one makes every 404 in a deployment look like a server
    /// fault and an error-rate alert becomes noise. This is the convention's own rule, not a
    /// preference.
    #[must_use]
    pub const fn from_http_status(status: u16) -> Self {
        if status >= 500 {
            SpanStatus::Error
        } else {
            SpanStatus::Unset
        }
    }
}

/// One recorded span, in this server's own terms.
///
/// Owned rather than borrowed, unlike [`access_log::Record`](crate::access_log::Record), and the
/// difference is not an oversight: an access-log record is written synchronously under a lock before
/// the request's frame goes away, while a span is *queued* for a batcher, so it has to outlive the
/// request that produced it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    /// The trace this span belongs to. Shared with every service on the path.
    pub trace_id: [u8; 16],
    /// This span's own id — the one this server already sent onward as the `traceparent` span id.
    pub span_id: [u8; 8],
    /// The caller's span id, or `None` for a span this server started.
    pub parent_span_id: Option<[u8; 8]>,
    /// What the operation is called. **A closed set by construction**: see the module note.
    pub name: &'static str,
    /// How this span relates to the work around it.
    pub kind: SpanKind,
    /// When the work started, in wall-clock terms.
    ///
    /// A `SystemTime` rather than an `Instant` because OTLP timestamps are UNIX epoch nanoseconds,
    /// and an `Instant` has no defined relation to the epoch. The monotonic clock is still what the
    /// access log's durations come from — this is only ever a timestamp.
    pub start: SystemTime,
    /// When the work finished.
    pub end: SystemTime,
    /// Key–value pairs describing this span.
    pub attributes: Vec<(&'static str, Attribute)>,
    /// How the work ended.
    pub status: SpanStatus,
    /// The human-readable reason, for a failure.
    pub status_message: Option<String>,
    /// W3C trace flags, as they will be written in the OTLP `flags` field.
    ///
    /// Bit 0 is `sampled`, and it is **carried rather than assumed**. A span that reaches this
    /// crate was sampled by definition — but the field also describes the *trace*, and a collector
    /// uses it to decide whether the trace is complete. Writing `1` unconditionally, which an
    /// earlier version did, would claim a trace was recorded when the flag a backend was handed
    /// said otherwise.
    pub flags: u8,
}

impl Span {
    /// The W3C trace flags for a request context.
    ///
    /// The one conversion from "this server's sampling decision" to "the bit the protocol carries",
    /// so the value written into a span and the value written into a `traceparent` cannot be
    /// spelled differently. Reserved bits are not set: this crate does not know what a future
    /// revision means by them, which is the same rule [`crate::TraceContext::parse`] follows when
    /// it reads one.
    #[must_use]
    pub const fn flags_for(sampled: bool) -> u8 {
        if sampled {
            TRACE_FLAG_SAMPLED
        } else {
            0
        }
    }
}

impl Span {
    /// How long the span took, in nanoseconds.
    ///
    /// Saturating rather than returning a `Result`: a clock that went backwards between the two
    /// readings is not a reason to lose the span, and a zero duration is the honest report of "this
    /// measurement did not work".
    #[must_use]
    pub fn duration_nanos(&self) -> u64 {
        self.end.duration_since(self.start).map_or(0, |duration| {
            u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
        })
    }
}

/// A span attribute's value.
///
/// Three arms, not one, because OTLP distinguishes them on the wire and a number sent as a string is
/// a number a backend cannot graph or range-query. `Int` is an `i64` because that is OTLP's
/// `intValue`; a `u64` that does not fit is encoded as a string rather than silently wrapping, which
/// keeps a byte count above 2^63 wrong-but-visible instead of wrong-and-negative.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Attribute {
    /// A string.
    String(String),
    /// A signed integer.
    Int(i64),
    /// A boolean.
    Bool(bool),
}

impl Attribute {
    /// The value as a string, for a caller that has to render it.
    #[must_use]
    pub fn as_string(&self) -> String {
        match self {
            Attribute::String(text) => text.clone(),
            Attribute::Int(number) => number.to_string(),
            Attribute::Bool(flag) => flag.to_string(),
        }
    }
}

impl From<&str> for Attribute {
    fn from(value: &str) -> Self {
        Attribute::String(value.to_string())
    }
}

impl From<String> for Attribute {
    fn from(value: String) -> Self {
        Attribute::String(value)
    }
}

impl From<i64> for Attribute {
    fn from(value: i64) -> Self {
        Attribute::Int(value)
    }
}

impl From<u64> for Attribute {
    fn from(value: u64) -> Self {
        match i64::try_from(value) {
            Ok(number) => Attribute::Int(number),
            // Wider than OTLP's `intValue` can hold. A string is lossless and visible; a truncation
            // is neither. Reachable only above 9.2 exabytes, so it is a correctness guard rather
            // than a case anyone meets.
            Err(_) => Attribute::String(value.to_string()),
        }
    }
}

impl From<bool> for Attribute {
    fn from(value: bool) -> Self {
        Attribute::Bool(value)
    }
}

/// Something that accepts finished spans.
///
/// The seam that lets this crate own identity without owning a transport. Implemented by
/// `volucer-otlp`'s exporter; a test implements it to capture spans with no network at all.
///
/// **A `record` that blocks is a bug in the implementation, not a licence for the caller.** This is
/// called on the request path, so an implementation must hand the span to a queue and return — the
/// same contract the access log's `record` has for its file write, for the same reason.
pub trait SpanRecorder: Send + Sync {
    /// Take a finished span. Must not block and must not fail a request.
    fn record(&self, span: Span);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn internal_is_one_and_not_the_unspecified_zero() {
        // The protocol reserves 0 and tells receivers to reinterpret it, so a mapping that let
        // `Internal` fall through to 0 would emit a span a backend may treat as anything.
        assert_eq!(SpanKind::Internal.otlp(), 1);
        assert_eq!(SpanKind::Server.otlp(), 2);
        assert_eq!(SpanKind::Client.otlp(), 3);
        assert_eq!(SpanKind::Producer.otlp(), 4);
        assert_eq!(SpanKind::Consumer.otlp(), 5);
        assert_ne!(SpanKind::Internal.otlp(), 0);
    }

    #[test]
    fn a_client_error_is_not_a_span_error_and_a_server_error_is() {
        // The convention's rule, and the one that decides whether a 404 storm pages somebody.
        assert_eq!(SpanStatus::from_http_status(404), SpanStatus::Unset);
        assert_eq!(SpanStatus::from_http_status(200), SpanStatus::Unset);
        assert_eq!(SpanStatus::from_http_status(499), SpanStatus::Unset);
        assert_eq!(SpanStatus::from_http_status(500), SpanStatus::Error);
        assert_eq!(SpanStatus::from_http_status(503), SpanStatus::Error);
    }

    #[test]
    fn a_backwards_clock_loses_the_duration_and_not_the_span() {
        // Reachable across a clock step, which NTP does routinely. The alternative — refusing the
        // span, or subtracting and wrapping — turns a clock adjustment into either a gap in the
        // trace or a duration of four billion seconds.
        let later = SystemTime::UNIX_EPOCH + Duration::from_secs(100);
        let earlier = SystemTime::UNIX_EPOCH + Duration::from_secs(50);
        let span = Span {
            trace_id: [1; 16],
            span_id: [2; 8],
            parent_span_id: None,
            name: "GET request",
            kind: SpanKind::Server,
            start: later,
            end: earlier,
            attributes: Vec::new(),
            status: SpanStatus::Unset,
            status_message: None,
            flags: TRACE_FLAG_SAMPLED,
        };
        assert_eq!(span.duration_nanos(), 0);
    }

    #[test]
    fn a_duration_is_nanoseconds_and_not_truncated() {
        let span = Span {
            trace_id: [1; 16],
            span_id: [2; 8],
            parent_span_id: None,
            name: "GET request",
            kind: SpanKind::Server,
            start: SystemTime::UNIX_EPOCH,
            end: SystemTime::UNIX_EPOCH + Duration::from_micros(1500),
            attributes: Vec::new(),
            status: SpanStatus::Unset,
            status_message: None,
            flags: TRACE_FLAG_SAMPLED,
        };
        assert_eq!(span.duration_nanos(), 1_500_000);
    }

    #[test]
    fn a_u64_too_wide_for_the_wire_becomes_a_string_rather_than_wrapping() {
        assert_eq!(Attribute::from(7u64), Attribute::Int(7));
        let too_wide = Attribute::from(u64::MAX);
        assert_eq!(too_wide, Attribute::String(u64::MAX.to_string()));
        // The point of the test: it must not be a negative number.
        assert!(!matches!(too_wide, Attribute::Int(_)));
    }

    #[test]
    fn every_attribute_renders_as_text() {
        assert_eq!(Attribute::from("abc").as_string(), "abc");
        assert_eq!(Attribute::from(-3i64).as_string(), "-3");
        assert_eq!(Attribute::from(true).as_string(), "true");
    }
}
