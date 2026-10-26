//! The exporter against a real HTTP server, over a real socket.
//!
//! # Why this test exists and what it is not
//!
//! The encoder's own tests prove the *bytes* are right. They cannot prove that anything ever puts
//! those bytes on a socket, or that the two ids agree with the ones the request carried — and that
//! second one is the entire reason this crate does not use a `tracing` layer. So this drives the
//! whole path: an `Observability` with a real recorder, a real request context, the batcher thread,
//! the `upstream` client, and a collector written here in twenty lines.
//!
//! **The collector is a raw TCP server, not a `hyper` one.** Deliberate: a fixture built from the
//! same library the client uses would agree with the client about a malformed request, which is the
//! one thing a test of a wire format must not do. This parses what it is sent — the request line,
//! the headers, the `Content-Length` and the body — and answers `200 {}`, and the assertions are on
//! the parsed body.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use volucer_core::config::TelemetryConfig;
use volucer_observe::Observability;
use volucer_otlp::Exporter;

/// One request a collector received.
struct Received {
    request_line: String,
    headers: String,
    body: String,
}

/// A collector that answers every request and reports what it was sent.
///
/// Bound before the exporter starts, so the port is real and the exporter dials it on the first
/// export rather than failing once and being reported.
fn collector() -> (String, mpsc::Receiver<Received>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("a free port");
    let address = listener.local_addr().expect("an address");
    let (sender, receiver) = mpsc::channel();

    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let Some(received) = read_request(&mut stream) else {
                continue;
            };
            let sent = sender.send(received);
            // `{}` with a length, because a conforming OTLP server answers an empty JSON object and
            // a client that waited for a body that never came would hang rather than proceed.
            let _ = stream.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{}",
            );
            let _ = stream.flush();
            if sent.is_err() {
                // The test has finished; nothing is listening any more.
                break;
            }
        }
    });

    (format!("http://{address}"), receiver)
}

/// Read one HTTP/1.1 request, headers and all.
fn read_request(stream: &mut TcpStream) -> Option<Received> {
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("a timeout is settable");

    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    // Read until the headers are complete, which is where `Content-Length` becomes readable.
    let header_end = loop {
        if let Some(index) = find_double_crlf(&buffer) {
            break index;
        }
        let read = stream.read(&mut chunk).ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
    };

    let head = String::from_utf8_lossy(&buffer[..header_end]).to_string();
    let content_length = head
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())?
        })
        .unwrap_or(0);

    let body_start = header_end + 4;
    while buffer.len() < body_start + content_length {
        let read = stream.read(&mut chunk).ok()?;
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
    }

    let mut lines = head.lines();
    let request_line = lines.next().unwrap_or_default().to_string();
    let headers = lines.collect::<Vec<_>>().join("\n");
    let body = String::from_utf8_lossy(&buffer[body_start..]).to_string();
    Some(Received {
        request_line,
        headers,
        body,
    })
}

fn find_double_crlf(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|window| window == b"\r\n\r\n")
}

fn config(endpoint: &str) -> TelemetryConfig {
    TelemetryConfig {
        endpoint: endpoint.to_string(),
        service_name: "checkout".to_string(),
        headers: std::collections::BTreeMap::new(),
        timeout_ms: 5_000,
        batch_size: 4,
        // Short, so the test does not depend on filling a batch — which is the property being
        // tested: a partly-filled batch must be sent on time.
        interval_ms: 100,
        // `always`, because these tests are about the wire rather than about cost, and the
        // default (`parent_based`) would drop a request that arrived without a sampled parent.
        sample: volucer_core::config::SampleMode::Always,
        sample_ratio: None,
    }
}

/// The whole path: a request context, the recorder, the batcher, the client, the collector.
#[test]
fn a_recorded_request_reaches_a_collector_with_the_ids_the_request_carried() {
    let (endpoint, received) = collector();
    let exporter = Arc::new(Exporter::start(&config(&endpoint)).expect("an exporter"));
    let observe = Observability::new(
        volucer_observe::access_log::AccessLog::disabled(),
        Some("X-Request-Id".to_string()),
    )
    .with_spans(exporter.recorder());

    // An inbound `traceparent`, exactly as the HTTP front-end reads it: the trace is the caller's,
    // the span is ours, and the caller's span id becomes our parent.
    let caller = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
    let context = observe.begin(Some("abc-123"), Some(caller));
    observe.record_server_span(
        &context,
        context.started_at,
        std::time::SystemTime::now(),
        volucer_observe::ServerSpanAttributes {
            method: "GET",
            path: "/checkout",
            host: Some("shop.example"),
            status: 200,
            name: "http.request",
            status_message: None,
            extra: &[],
        },
    );

    let request = received
        .recv_timeout(Duration::from_secs(20))
        .expect("the exporter must post a batch without the queue filling up");

    // The request line and the headers, which the encoder's own tests cannot check.
    assert_eq!(request.request_line, "POST /v1/traces HTTP/1.1");
    assert!(
        request
            .headers
            .to_ascii_lowercase()
            .contains("content-type: application/json"),
        "an OTLP/JSON body must be declared as JSON: {}",
        request.headers
    );

    let json: Value = serde_json::from_str(&request.body).expect("the body must be valid JSON");
    let span = &json["resourceSpans"][0]["scopeSpans"][0]["spans"][0];

    // **The assertions this whole design exists for.** The trace id is the caller's, and the span id
    // is the one this server already put on the request it passed to a backend — so a backend's
    // child span, which names that id as its parent, joins this trace.
    assert_eq!(span["traceId"], "4bf92f3577b34da6a3ce929d0e0e4736");
    assert_eq!(
        span["spanId"],
        context.trace.span_id_hex(),
        "the exported span id must be the one PHP received as its parent"
    );
    assert_ne!(
        span["spanId"], "00f067aa0ba902b7",
        "our span is not the caller's"
    );
    assert_eq!(span["parentSpanId"], "00f067aa0ba902b7");
    assert_eq!(span["kind"], 2, "a server span");
    assert_eq!(span["name"], "http.request");
    // The caller's `-01` is the flag exported, so the collector and the backend were told the same
    // thing. A literal `1` here would pass this assertion and be wrong for every `-00` trace.
    assert_eq!(
        span["flags"], 1,
        "the sampled flag must be the one the request carried"
    );

    // And the attributes carry what the request carried, with the status as a number so a backend
    // can range-query it.
    let attributes = span["attributes"].as_array().expect("an array").clone();
    let find = |key: &str| {
        attributes
            .iter()
            .find(|pair| pair["key"] == key)
            .unwrap_or_else(|| panic!("{key} must be present"))
            .clone()
    };
    assert_eq!(find("http.request.method")["value"]["stringValue"], "GET");
    assert_eq!(find("url.path")["value"]["stringValue"], "/checkout");
    assert_eq!(
        find("server.address")["value"]["stringValue"],
        "shop.example"
    );
    assert_eq!(
        find("http.response.status_code")["value"]["intValue"],
        "200"
    );

    // The resource names the deployment, not this crate.
    let resource = json["resourceSpans"][0]["resource"]["attributes"]
        .as_array()
        .expect("an array")
        .clone();
    assert!(resource
        .iter()
        .any(|pair| pair["key"] == "service.name" && pair["value"]["stringValue"] == "checkout"));

    exporter.stop();
}

/// A configured header reaches the collector, because that is how a hosted collector authenticates
/// and a key that was silently dropped would look exactly like a rejected request.
#[test]
fn a_configured_header_is_sent_with_the_export() {
    let (endpoint, received) = collector();
    let mut settings = config(&endpoint);
    settings
        .headers
        .insert("x-honeycomb-team".to_string(), "secret123".to_string());
    settings.interval_ms = 50;
    let exporter = Arc::new(Exporter::start(&settings).expect("an exporter"));
    let observe = Observability::disabled().with_spans(exporter.recorder());

    let context = observe.begin(None, None);
    observe.record_server_span(
        &context,
        context.started_at,
        std::time::SystemTime::now(),
        volucer_observe::ServerSpanAttributes {
            // A request with no inbound traceparent is a **root** span: no parent, and a trace id
            // this server minted.
            method: "POST",
            path: "/orders",
            host: None,
            status: 201,
            name: "http.request",
            status_message: None,
            extra: &[],
        },
    );

    let request = received
        .recv_timeout(Duration::from_secs(20))
        .expect("the batch must be sent");
    assert!(
        request
            .headers
            .to_ascii_lowercase()
            .contains("x-honeycomb-team: secret123"),
        "the configured header was not sent: {}",
        request.headers
    );

    let json: Value = serde_json::from_str(&request.body).expect("valid JSON");
    let span = &json["resourceSpans"][0]["scopeSpans"][0]["spans"][0];
    assert!(
        span.get("parentSpanId").is_none(),
        "a request with no inbound trace is a root span"
    );
    assert_eq!(span["traceId"], context.trace.trace_id_hex());

    exporter.stop();
}

/// A deployment with **no collector listening** must serve requests and lose only its spans — this
/// is the failure mode that decides whether tracing is safe to turn on, and it is asserted rather
/// than asserted-in-a-comment because the exporter's whole contract is "the request path never
/// waits on the network".
#[test]
fn an_unreachable_collector_loses_spans_and_never_fails_a_request() {
    // Bound and dropped, so the port is almost certainly free: connecting to it is a refusal.
    let listener = TcpListener::bind("127.0.0.1:0").expect("a free port");
    let address = listener.local_addr().expect("an address");
    drop(listener);

    let mut settings = config(&format!("http://{address}"));
    settings.interval_ms = 50;
    settings.timeout_ms = 500;
    let exporter = Arc::new(Exporter::start(&settings).expect("an exporter"));
    let observe = Observability::disabled().with_spans(exporter.recorder());

    // Every one of these must return promptly, with no panic and no block.
    let started = std::time::Instant::now();
    for _ in 0..200 {
        let context = observe.begin(None, None);
        observe.record_server_span(
            &context,
            context.started_at,
            std::time::SystemTime::now(),
            volucer_observe::ServerSpanAttributes {
                method: "GET",
                path: "/health",
                host: None,
                status: 200,
                name: "http.request",
                status_message: None,
                extra: &[],
            },
        );
    }
    let elapsed = started.elapsed();
    // The bound is generous — this is a smoke test that recording did not become a synchronous
    // 500 ms dial per span, which is what a naive implementation would do.
    assert!(
        elapsed < Duration::from_secs(5),
        "recording 200 spans took {elapsed:?}, which means the request path is waiting on the \
         network"
    );

    // The batch is attempted and fails rather than being silently discarded, and the stop still
    // returns — a shutdown that hung on an unreachable collector would be worse than the outage.
    let stopped = std::time::Instant::now();
    exporter.stop();
    assert!(stopped.elapsed() < Duration::from_secs(10));
    let counts = exporter.counts();
    assert_eq!(
        counts.exported, 0,
        "nothing can have been exported to a port nothing listens on"
    );
}

/// A trace the caller asked **not** to record is not exported — and the collector, being absent,
/// cannot be the reason. This is the end-to-end half of the flag fix: the encoder can carry a zero
/// flag, and `record_server_span` can decline, but only this shows that a real exporter posting to
/// a real collector sends nothing for a `-00` request.
#[test]
fn a_request_the_caller_marked_unsampled_produces_no_export_at_all() {
    let (endpoint, received) = collector();
    let mut settings = config(&endpoint);
    settings.interval_ms = 50;
    settings.sample = volucer_core::config::SampleMode::ParentBased;
    let exporter = Arc::new(Exporter::start(&settings).expect("an exporter"));
    let observe = Observability::disabled()
        .with_spans(exporter.recorder())
        .with_sampling(volucer_observe::Sampler::ParentBased);

    // The caller's flag is clear: "do not record this". nginx filters on `$otel_parent_sampled`
    // for exactly this case.
    let caller = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-00";
    let context = observe.begin(Some("abc-123"), Some(caller));
    assert!(
        !context.trace.sampled(),
        "the caller's flag must be honoured"
    );
    observe.record_server_span(
        &context,
        context.started_at,
        std::time::SystemTime::now(),
        volucer_observe::ServerSpanAttributes {
            method: "GET",
            path: "/checkout",
            host: Some("shop.example"),
            status: 200,
            name: "http.request",
            status_message: None,
            extra: &[],
        },
    );

    // And a root — no inbound `traceparent` — *is* exported, which is the other half of the
    // default: without it, a standalone deployment would export nothing while believing otherwise.
    let root = observe.begin(None, None);
    assert!(root.trace.sampled(), "a root must be recorded");
    observe.record_server_span(
        &root,
        root.started_at,
        std::time::SystemTime::now(),
        volucer_observe::ServerSpanAttributes {
            method: "GET",
            path: "/",
            host: None,
            status: 200,
            name: "http.request",
            status_message: None,
            extra: &[],
        },
    );

    let request = received
        .recv_timeout(Duration::from_secs(20))
        .expect("the root's span must be exported");
    let json: Value = serde_json::from_str(&request.body).expect("valid JSON");
    let spans = json["resourceSpans"][0]["scopeSpans"][0]["spans"]
        .as_array()
        .expect("an array");
    assert_eq!(
        spans.len(),
        1,
        "only the root was exported; the unsampled trace must have produced nothing"
    );
    // The one that arrived is the root's, not the dropped trace's — asserted by the trace id,
    // because a batch of one that happened to be the wrong one would satisfy the count.
    assert_eq!(spans[0]["traceId"], root.trace.trace_id_hex());
    assert_eq!(spans[0]["flags"], 1);

    exporter.stop();
}
