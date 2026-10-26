//! The OTLP/JSON encoder: `Span`s in, one `ExportTraceServiceRequest` out.
//!
//! # Why this is written by hand, and what the plan got wrong about it
//!
//! The claim that OTLP/HTTP on 4318 "avoids `tonic`/`prost` entirely" is half true and half not, and
//! the difference decides this file's existence. Measured from
//! `opentelemetry-otlp` 0.33.1's own manifest:
//!
//! ```text
//! http-proto = ["prost", "opentelemetry-http", "opentelemetry-proto/gen-tonic-messages", "http", …]
//! ```
//!
//! So the *transport* avoids `tonic` — no gRPC, no HTTP/2 framing of messages — but the **encoding
//! does not**: `http-proto` requires `prost`, because OTLP/HTTP's body is a protobuf message. There
//! is no feature of that crate that serializes traces as JSON without `prost` either; `http-json`
//! also names it, because it round-trips through the generated message types.
//!
//! # So why not take `prost` and be done with it
//!
//! Three reasons, and the third is the decisive one:
//!
//! 1. **Measured cost.** `opentelemetry-otlp` with `hyper-client` adds 22 packages to this
//!    workspace's lockfile; with `reqwest-blocking-client` it adds 41. `prost` and `prost-derive`
//!    are two of the 22. The rest is the SDK's own machinery — samplers, span limits, processors.
//! 2. **This project already made this call twice.** The Prometheus exposition encoder and the
//!    access log are both hand-written, for the reason `volucer-metrics` states: the format is a
//!    page of specification, and a hand-written encoder is the only way to guarantee the property
//!    that matters most — **it cannot fail**.
//! 3. **The SDK cannot carry the id we need it to carry.** `SdkTracer::build_with_context` always
//!    calls `config.id_generator.new_span_id()`, and `SpanBuilder` has no `with_span_id` — a span id
//!    can only be *inherited* from a parent context. But this server mints the span id it puts on
//!    the request a backend receives, so PHP's child span names *that* id as its parent. An exporter
//!    built on the SDK would export a different one, and the trace would break at the very hop it
//!    exists to show. Driving the encoder directly is not a preference; it is what makes the ids
//!    agree.
//!
//! # The format's three traps, all of which this module handles
//!
//! OTLP/JSON is protobuf JSON, not "some JSON", and three of its rules are unusual enough that each
//! has cost somebody an afternoon:
//!
//! - **`bytes` fields are lowercase hex, not base64.** `trace_id` is 32 hex characters. (Base64 is
//!   for `bytesValue` *inside* an `AnyValue`, which is a different thing — see [`any_value`].)
//! - **64-bit integers are strings.** `start_time_unix_nano` is `"1750000000000000000"`, quoted,
//!   because JSON numbers are IEEE doubles and cannot hold a nanosecond timestamp. `intValue` in an
//!   attribute is quoted for the same reason.
//! - **Enums are integers here, and the field names are camelCase.** `"kind":2`.
//!
//! # What this encoder deliberately omits
//!
//! No `traceState`, no `schemaUrl`, no `droppedAttributesCount`, no span events or links. Every one
//! of those is optional in the message and every one of them is absent rather than zero: a `"0"` for
//! a dropped count says "some attributes were dropped" to a reader that sees the field, which is a
//! claim this server cannot make. Omitting a field is the honest spelling of "not applicable", and
//! it also keeps the body smaller on a path that runs once per batch.

use std::fmt::Write as _;
use std::time::{SystemTime, UNIX_EPOCH};

use volucer_observe::span::{Attribute, Span};

/// The resource attributes every batch carries.
///
/// `service.name` is required by the specification and is the one attribute every backend groups by,
/// so it is not defaulted to nothing. The SDK pair is what every OTLP producer sends and what lets a
/// collector route by implementation — worth two lines, and named for this crate rather than for the
/// umbrella `opentelemetry` package, which is not what produced these spans.
#[derive(Debug, Clone)]
pub struct Resource {
    /// `service.name`.
    pub service_name: String,
    /// `service.version`, when the caller knows it.
    pub service_version: Option<String>,
}

impl Resource {
    /// The resource for this build.
    #[must_use]
    pub fn new(service_name: impl Into<String>) -> Self {
        Self {
            service_name: service_name.into(),
            service_version: Some(env!("CARGO_PKG_VERSION").to_string()),
        }
    }
}

/// The `scope.name` every batch carries.
///
/// One scope, because there is one producer. The field exists so that a collector can tell two
/// libraries' spans apart, and inventing a second name here would be a fiction.
pub const SCOPE_NAME: &str = "volucer-otlp";

/// Encode a batch as an OTLP/JSON `ExportTraceServiceRequest`.
///
/// Returns the body of a `POST /v1/traces`. **Cannot fail**: it writes into a `String`, and the only
/// operation that could panic — an unvalidated UTF-8 slice — is never performed, because every
/// value written is either hex, a decimal number, or an escaped JSON string.
///
/// The span order is the order given, which is the order they were finished.
#[must_use]
pub fn encode_traces(resource: &Resource, spans: &[Span]) -> String {
    // A rough size: the fixed skeleton is a few hundred bytes, and a span with four attributes is
    // roughly 400. Under-reserving costs a reallocation once per batch; over-reserving wastes a
    // batch's worth of memory at every export.
    let mut out = String::with_capacity(512 + spans.len() * 420);

    out.push_str("{\"resourceSpans\":[{\"resource\":{\"attributes\":[");
    push_list(
        &mut out,
        [
            (
                "service.name",
                Attribute::String(resource.service_name.clone()),
            ),
            (
                "telemetry.sdk.name",
                Attribute::String(SCOPE_NAME.to_string()),
            ),
            (
                "telemetry.sdk.version",
                Attribute::String(env!("CARGO_PKG_VERSION").to_string()),
            ),
        ]
        .into_iter()
        .chain(
            resource
                .service_version
                .clone()
                .map(|version| ("service.version", Attribute::String(version))),
        ),
    );
    out.push_str("]},\"scopeSpans\":[{\"scope\":{\"name\":\"");
    out.push_str(SCOPE_NAME);
    out.push_str("\"},\"spans\":[");

    for (index, span) in spans.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        push_span(&mut out, span);
    }

    out.push_str("]}]}]}");
    out
}

/// One `Span` message.
fn push_span(out: &mut String, span: &Span) {
    out.push_str("{\"traceId\":\"");
    push_hex(out, &span.trace_id);
    out.push_str("\",\"spanId\":\"");
    push_hex(out, &span.span_id);
    out.push('"');
    // Absent, not zero, for a root span — the specification says an empty `parentSpanId` is a root
    // and an all-zero 16-character value is invalid. Writing `"0000000000000000"` for a root would
    // be a span that no reader can classify.
    if let Some(parent) = span.parent_span_id {
        out.push_str(",\"parentSpanId\":\"");
        push_hex(out, &parent);
        out.push('"');
    }
    // The trace flags, which for this server's spans is the `sampled` bit and nothing else. Written
    // from the span rather than as a literal `1`: the flag says whether the *trace* is recorded, and
    // a span that reached this point was recorded — but the field must agree with the `traceparent`
    // the backend was handed, or a collector is told a trace is complete that a backend was told to
    // drop. OTLP's `flags` is a `uint32`, and the value is the low byte.
    let _ = write!(out, ",\"flags\":{},\"name\":", span.flags);
    push_json_string(out, span.name);
    let _ = write!(out, ",\"kind\":{}", span.kind.otlp());
    let _ = write!(
        out,
        ",\"startTimeUnixNano\":\"{}\",\"endTimeUnixNano\":\"{}\"",
        epoch_nanos(span.start),
        epoch_nanos(span.end)
    );

    if !span.attributes.is_empty() {
        out.push_str(",\"attributes\":[");
        push_attribute_list(out, &span.attributes);
        out.push(']');
    }

    let code = span.status.otlp();
    // `"status":{}` is the default and is omitted rather than written: a reader that sees `"code":0`
    // may treat the field as present-and-unset, and an empty object is what the specification tells
    // a producer to send for the default.
    if code != 0 || span.status_message.is_some() {
        out.push_str(",\"status\":{");
        let mut first = true;
        if let Some(message) = &span.status_message {
            out.push_str("\"message\":");
            push_json_string(out, message);
            first = false;
        }
        if code != 0 {
            if !first {
                out.push(',');
            }
            let _ = write!(out, "\"code\":{code}");
        }
        out.push('}');
    }

    out.push('}');
}

/// One `KeyValue`, whose `value` is an `AnyValue`.
fn push_key_value(out: &mut String, key: &str, value: &Attribute) {
    out.push_str("{\"key\":");
    push_json_string(out, key);
    out.push_str(",\"value\":");
    any_value(out, value);
    out.push('}');
}

/// A comma-separated array of `KeyValue`s, written directly.
///
/// **A helper rather than a loop at each call site, and the first version of this file is why**: four
/// resource attributes were emitted with no separators between them, producing a body no JSON parser
/// accepts. The test that would have caught it in one field is the one that parses the *whole*
/// document — which is what every test in this module now does, and a rule worth keeping.
fn push_attribute_list(out: &mut String, pairs: &[(&'static str, Attribute)]) {
    push_list(out, pairs.iter().map(|(key, value)| (*key, value.clone())));
}

/// A comma-separated `KeyValue` array from any iterator of pairs.
///
/// Generic over the iterator so the resource's chain of fixed attributes and an optional
/// `service.version` can be one call rather than a special case.
fn push_list<I>(out: &mut String, pairs: I)
where
    I: IntoIterator<Item = (&'static str, Attribute)>,
{
    let mut first = true;
    for (key, value) in pairs {
        if !first {
            out.push(',');
        }
        first = false;
        push_key_value(out, key, &value);
    }
}

/// An `AnyValue`.
///
/// Exactly one field is set, which is what the message's `oneof` requires. Note the shape
/// difference that trips people up: `stringValue` is a bare JSON string, while `intValue` is a
/// **quoted** one — 64-bit integers are strings in protobuf JSON for the reason the module
/// documentation gives.
fn any_value(out: &mut String, value: &Attribute) {
    match value {
        Attribute::String(text) => {
            out.push_str("{\"stringValue\":");
            push_json_string(out, text);
            out.push('}');
        }
        Attribute::Int(number) => {
            out.push_str("{\"intValue\":\"");
            let _ = write!(out, "{number}");
            out.push_str("\"}");
        }
        Attribute::Bool(flag) => {
            out.push_str("{\"boolValue\":");
            out.push_str(if *flag { "true" } else { "false" });
            out.push('}');
        }
    }
}

/// A UNIX timestamp in nanoseconds, as protobuf JSON wants it.
///
/// Saturating at both ends rather than panicking: a `SystemTime` before the epoch is possible from a
/// badly-set clock, and a span with a wrong-by-a-century timestamp is far better than a panicking
/// exporter on the request path.
fn epoch_nanos(time: SystemTime) -> u64 {
    match time.duration_since(UNIX_EPOCH) {
        Ok(since) => u64::try_from(since.as_nanos()).unwrap_or(u64::MAX),
        // Before 1970, which the specification has no representation for. Zero is the honest report
        // of "this timestamp is not usable", and a reader shows it as 1970 rather than as a span
        // that ended before it started.
        Err(_) => 0,
    }
}

/// Write bytes as lowercase hex.
///
/// The single most common way to get OTLP/JSON wrong is to base64 the ids, because the field is
/// declared `bytes` and most JSON encoders treat `bytes` as base64. The specification makes an
/// exception for these fields and this is it.
fn push_hex(out: &mut String, bytes: &[u8]) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
}

/// Escape a string for JSON.
///
/// The same routine the access log's JSON format needs, and for the same reason: a value that came
/// from a request must not be able to close the string it is written into. `\u00XX` for control
/// characters because JSON has no other escape for them, and a bare control byte makes the whole
/// document unparseable — which would lose a batch rather than one field.
fn push_json_string(out: &mut String, value: &str) {
    out.push('"');
    for character in value.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            other if (other as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", other as u32);
            }
            other => out.push(other),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use std::time::Duration;
    use volucer_observe::span::{SpanKind, SpanStatus};

    fn span() -> Span {
        Span {
            trace_id: [
                0x0a, 0xf7, 0x65, 0x19, 0x16, 0xcd, 0x43, 0xdd, 0x84, 0x48, 0xeb, 0x21, 0x1c, 0x80,
                0x31, 0x9c,
            ],
            span_id: [0xb7, 0xad, 0x6b, 0x71, 0x69, 0x20, 0x33, 0x31],
            parent_span_id: None,
            name: "GET request",
            kind: SpanKind::Server,
            start: UNIX_EPOCH + Duration::from_nanos(1_750_000_000_000_000_000),
            end: UNIX_EPOCH + Duration::from_nanos(1_750_000_000_012_500_000),
            attributes: vec![
                ("http.request.method", Attribute::from("GET")),
                ("url.path", Attribute::from("/about")),
                ("http.response.status_code", Attribute::Int(200)),
            ],
            status: SpanStatus::Unset,
            status_message: None,
            flags: volucer_observe::span::TRACE_FLAG_SAMPLED,
        }
    }

    fn encode_one(span: Span) -> Value {
        let body = encode_traces(&Resource::new("checkout"), &[span]);
        serde_json::from_str(&body).expect("the encoder must produce valid JSON")
    }

    /// The whole point of the module: ids are **hex**, and they are the ids that were handed in.
    #[test]
    fn ids_are_lowercase_hex_and_are_not_base64() {
        let json = encode_one(span());
        let exported = &json["resourceSpans"][0]["scopeSpans"][0]["spans"][0];
        assert_eq!(
            exported["traceId"], "0af7651916cd43dd8448eb211c80319c",
            "a base64 id here is the classic OTLP/JSON mistake"
        );
        assert_eq!(exported["spanId"], "b7ad6b7169203331");
        // 16 bytes hex is 32 characters, and base64 of the same bytes would be 24 — so the length
        // alone distinguishes them, which is why it is asserted rather than assumed.
        assert_eq!(exported["traceId"].as_str().unwrap().len(), 32);
        assert_eq!(exported["spanId"].as_str().unwrap().len(), 16);
    }

    /// 64-bit integers are **strings** in protobuf JSON: JSON numbers are doubles and cannot hold a
    /// nanosecond timestamp, so an unquoted value silently loses its last digits.
    #[test]
    fn nanosecond_timestamps_are_quoted_strings_that_survive_round_tripping() {
        let json = encode_one(span());
        let exported = &json["resourceSpans"][0]["scopeSpans"][0]["spans"][0];
        assert_eq!(exported["startTimeUnixNano"], "1750000000000000000");
        assert_eq!(exported["endTimeUnixNano"], "1750000000012500000");
        assert!(
            exported["startTimeUnixNano"].is_string(),
            "an unquoted nanosecond timestamp is a double, and 1.75e18 has no nanosecond precision"
        );
    }

    /// An attribute's integer is also a string, and a string attribute is not — the asymmetry is
    /// the specification's, and getting it backwards is the second classic mistake.
    #[test]
    fn attribute_values_are_typed_the_way_the_oneof_requires() {
        let json = encode_one(span());
        let attributes = json["resourceSpans"][0]["scopeSpans"][0]["spans"][0]["attributes"]
            .as_array()
            .expect("an array")
            .clone();
        let find = |key: &str| {
            attributes
                .iter()
                .find(|pair| pair["key"] == key)
                .unwrap_or_else(|| panic!("{key} must be present"))
                .clone()
        };
        assert_eq!(find("http.request.method")["value"]["stringValue"], "GET");
        assert_eq!(find("url.path")["value"]["stringValue"], "/about");
        assert_eq!(
            find("http.response.status_code")["value"]["intValue"],
            "200",
            "intValue is quoted; an unquoted one loses precision above 2^53"
        );
    }

    /// A root span has **no** `parentSpanId` key at all. Writing an all-zero one is what the
    /// specification calls invalid, and a reader cannot tell it from a span whose parent is the zero
    /// id.
    #[test]
    fn a_root_span_omits_the_parent_rather_than_writing_zeros() {
        let json = encode_one(span());
        let exported = &json["resourceSpans"][0]["scopeSpans"][0]["spans"][0];
        assert!(
            exported.get("parentSpanId").is_none(),
            "a root span must not carry a parent field"
        );

        let mut child = span();
        child.parent_span_id = Some([0x00, 0xf0, 0x67, 0xaa, 0x0b, 0xa9, 0x02, 0xb7]);
        let json = encode_one(child);
        let exported = &json["resourceSpans"][0]["scopeSpans"][0]["spans"][0];
        assert_eq!(exported["parentSpanId"], "00f067aa0ba902b7");
    }

    /// The default status is an omitted field and not `"code":0`, and a failure carries both a code
    /// and the message a human reads.
    #[test]
    fn the_default_status_is_omitted_and_an_error_carries_its_message() {
        let json = encode_one(span());
        let exported = &json["resourceSpans"][0]["scopeSpans"][0]["spans"][0];
        assert!(
            exported.get("status").is_none(),
            "an unset status is the default and must be absent, not zero"
        );

        let mut failed = span();
        failed.status = SpanStatus::Error;
        failed.status_message = Some("the backend refused".to_string());
        let json = encode_one(failed);
        let status = &json["resourceSpans"][0]["scopeSpans"][0]["spans"][0]["status"];
        assert_eq!(status["code"], 2);
        assert_eq!(status["message"], "the backend refused");
    }

    /// `kind` is an integer, and `Internal` is 1 — the protocol reserves 0 for "unspecified" and
    /// tells receivers to reinterpret it.
    #[test]
    fn the_span_kind_is_an_integer_and_internal_is_never_zero() {
        let json = encode_one(span());
        let kind = &json["resourceSpans"][0]["scopeSpans"][0]["spans"][0]["kind"];
        assert_eq!(*kind, 2, "a server span is 2");

        let mut internal = span();
        internal.kind = SpanKind::Internal;
        let json = encode_one(internal);
        let kind = &json["resourceSpans"][0]["scopeSpans"][0]["spans"][0]["kind"];
        assert_eq!(*kind, 1);
        assert_ne!(*kind, 0);
    }

    /// The `flags` field carries the sampled bit from the span rather than a constant.
    ///
    /// **The bug this test exists for.** The encoder wrote `"flags":1` unconditionally, so an
    /// inbound `-00` trace was propagated to the backend as unsampled and exported to the collector
    /// as sampled. Nothing on the wire failed; the two just disagreed, and the only way to see it
    /// was to compare a `traceparent` with a collector's payload.
    #[test]
    fn the_sampled_flag_is_carried_and_not_assumed() {
        let mut dropped = span();
        dropped.flags = 0;
        let json = encode_one(dropped);
        assert_eq!(
            json["resourceSpans"][0]["scopeSpans"][0]["spans"][0]["flags"], 0,
            "an unsampled trace must not be exported as sampled"
        );

        // And the flag is a number rather than a string or a boolean: OTLP's `flags` is a
        // `uint32` in protobuf JSON, where every 64-bit integer is a string but a 32-bit one is
        // not — the distinction that catches a producer treating all integers alike.
        let json = encode_one(span());
        let flags = &json["resourceSpans"][0]["scopeSpans"][0]["spans"][0]["flags"];
        assert_eq!(*flags, 1);
        assert!(flags.is_number(), "flags must be a number, not a string");
    }

    /// The resource has to be there, because it is what every backend groups by, and the scope name
    /// has to be a real producer rather than the umbrella package that is not linked in.
    #[test]
    fn the_resource_names_the_service_and_the_scope_names_this_crate() {
        let json = encode_one(span());
        let resource = &json["resourceSpans"][0]["resource"]["attributes"];
        let attributes = resource.as_array().expect("an array").clone();
        let find = |key: &str| {
            attributes
                .iter()
                .find(|pair| pair["key"] == key)
                .unwrap_or_else(|| panic!("{key} must be present"))
                .clone()
        };
        assert_eq!(find("service.name")["value"]["stringValue"], "checkout");
        assert!(find("service.version")["value"]["stringValue"].is_string());
        assert_eq!(
            json["resourceSpans"][0]["scopeSpans"][0]["scope"]["name"],
            "volucer-otlp"
        );
    }

    /// A path with a quote or a control byte must not be able to close the string it is written
    /// into — the same injection the access log guards, on a different wire.
    #[test]
    fn an_attribute_cannot_break_out_of_its_string() {
        let mut hostile = span();
        hostile.attributes = vec![("url.path", Attribute::from("/a\",\"kind\":9,\"x\":\""))];
        let json = encode_one(hostile);
        let exported = &json["resourceSpans"][0]["scopeSpans"][0]["spans"][0];
        // The value survived intact and became *one* attribute, which is the whole claim: an
        // unescaped quote would have produced a second object and moved `kind`.
        let attributes = exported["attributes"].as_array().expect("an array");
        assert_eq!(attributes.len(), 1);
        assert_eq!(
            attributes[0]["value"]["stringValue"],
            "/a\",\"kind\":9,\"x\":\""
        );
        assert_eq!(exported["kind"], 2, "the injection moved the kind");
    }

    /// A control byte is escaped as `\u00XX`; written raw it makes the whole document unparseable,
    /// which loses a batch rather than a field. `serde_json` refusing to parse it is the assertion.
    #[test]
    fn a_control_byte_is_escaped_rather_than_written_raw() {
        let mut hostile = span();
        hostile.attributes = vec![("url.path", Attribute::from("a\u{1}b\u{7f}c"))];
        let body = encode_traces(&Resource::new("svc"), &[hostile]);
        assert!(body.contains("\\u0001"), "{body}");
        let json: Value = serde_json::from_str(&body).expect("valid JSON");
        assert_eq!(
            json["resourceSpans"][0]["scopeSpans"][0]["spans"][0]["attributes"][0]["value"]
                ["stringValue"],
            "a\u{1}b\u{7f}c"
        );
    }

    /// An empty batch encodes as a valid, empty request rather than as `""` or a panic — a batcher
    /// that flushed while a shutdown raced a send must still produce something postable.
    #[test]
    fn an_empty_batch_is_a_valid_empty_request() {
        let body = encode_traces(&Resource::new("svc"), &[]);
        let json: Value = serde_json::from_str(&body).expect("valid JSON");
        assert_eq!(
            json["resourceSpans"][0]["scopeSpans"][0]["spans"]
                .as_array()
                .expect("an array")
                .len(),
            0
        );
    }

    /// Many spans are comma-separated exactly once — the simplest way to emit invalid JSON is a
    /// stray or missing separator, and one span per batch would never find it.
    #[test]
    fn several_spans_are_separated_correctly() {
        let mut second = span();
        second.span_id = [1; 8];
        let body = encode_traces(&Resource::new("svc"), &[span(), second]);
        let json: Value = serde_json::from_str(&body).expect("valid JSON");
        let spans = json["resourceSpans"][0]["scopeSpans"][0]["spans"]
            .as_array()
            .expect("an array");
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0]["spanId"], "b7ad6b7169203331");
        assert_eq!(spans[1]["spanId"], "0101010101010101");
    }

    /// A clock before the epoch is a badly-set clock, not a reason to panic on the request path.
    #[test]
    fn a_timestamp_before_the_epoch_saturates_instead_of_panicking() {
        let mut ancient = span();
        ancient.start = UNIX_EPOCH - Duration::from_secs(60);
        let json = encode_one(ancient);
        let exported = &json["resourceSpans"][0]["scopeSpans"][0]["spans"][0];
        assert_eq!(exported["startTimeUnixNano"], "0");
    }

    /// Every byte written is either hex, a decimal, an escaped string, or the fixed skeleton — so
    /// the encoder has no failure mode. Asserted by encoding a span whose every textual field is
    /// hostile, with a body that must still parse.
    #[test]
    fn a_hostile_span_still_produces_parseable_json() {
        let mut hostile = span();
        hostile.status = SpanStatus::Error;
        hostile.status_message = Some("\"}\",\"resourceSpans\":[]".to_string());
        hostile.attributes = vec![
            ("url.path", Attribute::from("\u{0}\u{1f}\\")),
            ("http.request.method", Attribute::from(true)),
        ];
        let body = encode_traces(&Resource::new("a\"b"), &[hostile]);
        let json: Value = serde_json::from_str(&body).expect("valid JSON");
        assert_eq!(json["resourceSpans"].as_array().expect("an array").len(), 1);
    }
}
