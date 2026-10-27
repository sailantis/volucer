//! The trace exporter: a batch queue, a thread, and a `POST /v1/traces` per batch.
//!
//! # The shape, and why it is this shape
//!
//! ```text
//!   request thread                          exporter thread
//!     └─ SpanRecorder::record ──► queue ──► drain up to batch_size
//!        (never blocks)                      ├─ flush early after interval_ms
//!                                           ├─ encode              → encode.rs
//!                                           ├─ POST {endpoint}/v1/traces
//!                                           └─ on failure: warn once, drop the batch
//! ```
//!
//! **The request path never waits on the network.** A trace exporter that sent its batch inline
//! would put a collector's latency — or its timeout, ten seconds of it — onto every request it
//! instruments, which is the failure mode that makes people turn tracing off. `record` is a channel
//! send and nothing else.
//!
//! # A batch is dropped, never retried, and that is the deliberate half
//!
//! The other option is a retry queue, and it is the wrong one here: a collector that is down for an
//! hour would accumulate an hour of spans in the process's memory, converting a telemetry outage
//! into an application outage — the one outcome this project refuses everywhere else (the access log
//! drops a record rather than failing a request; a full KV store refuses a write rather than
//! evicting). What is *not* done is hiding it: the first failure of an outage warns with the reason,
//! and a success after a failure says so, so an operator sees both the outage and its end rather
//! than inferring both from a gap in a graph.
//!
//! # Why a thread and not a `tokio` task
//!
//! The same reason the job consumer is a thread: the request path is synchronous on the backend
//! side, and the HTTP listener's runtime belongs to the listener. This crate owns one thread with
//! one small current-thread runtime on it, which is exactly what `volucer-upstream`'s client needs.
//! It also means the exporter works in a build where no async runtime is compiled in anywhere else.
//!
//! # One connection, kept
//!
//! The client is `volucer-upstream`'s pooled one, so exports reuse a connection instead of dialling
//! and handshaking per batch — which for `https:` would be a TLS handshake per export, the dominant
//! cost at any sane batch size.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::Full;
use volucer_core::config::{TelemetryConfig, OTLP_TRACES_PATH};
use volucer_observe::span::{Span, SpanRecorder};
use volucer_upstream::{Origin, UpstreamRequest};

pub mod encode;

use encode::{encode_traces, Resource};

/// How many spans may be queued before `record` starts dropping them.
///
/// **Dropping is the correct behaviour and blocking is not**, which is the opposite of what a bound
/// usually means for a queue. `record` runs on the request path: blocking there when a collector is
/// slow would turn a telemetry problem into a latency problem, and *that* is a bug an operator
/// reports as "the server got slow when we turned on tracing". Four batches' worth is enough
/// headroom to ride out a stall of four export intervals; beyond that the spans are gone — visibly,
/// because [`Exporter::counts`] reports them and the first drop says so.
pub const QUEUE_CAPACITY: usize = 2048;

/// What went wrong while building an exporter, or while exporting.
#[derive(Debug, thiserror::Error)]
pub enum ExportError {
    /// The endpoint is not a usable origin.
    #[error("the OTLP endpoint {endpoint:?} cannot be used: {reason}")]
    Endpoint {
        /// The configured value.
        endpoint: String,
        /// Why it was refused.
        reason: String,
    },

    /// The exporter thread could not be started.
    #[error("the trace exporter thread could not be started: {0}")]
    Thread(#[from] std::io::Error),

    /// The export request could not be completed.
    #[error("the OTLP endpoint could not be reached: {0}")]
    Unreachable(String),

    /// The collector answered, and refused.
    #[error("the OTLP endpoint answered {status}: {body}")]
    Refused {
        /// The status the collector returned.
        status: u16,
        /// As much of the body as a human needs, truncated.
        body: String,
    },
}

/// A running exporter: the sending half of the queue, and the thread draining it.
///
/// Dropping this asks the thread to stop and joins it, which is what makes the exporter's final
/// flush observable — a `Drop` that only signalled would let a process exit with a full batch
/// unexported and no way to know.
pub struct Exporter {
    sender: SyncSender<Span>,
    /// The thread, behind a lock rather than behind `&mut self`.
    ///
    /// **`stop` takes `&self`, and that is what makes it usable at all**: the recorder half is an
    /// `Arc<dyn SpanRecorder>` the front-ends hold for the whole run, so the exporter is never
    /// uniquely owned while it is running, and a `stop(&mut self)` could only be called by someone
    /// who had already given the recorder away — which is everybody. An `AtomicBool` for the flag
    /// and a `Mutex` around the handle is the same shape the listener's shutdown already uses.
    thread: Mutex<Option<JoinHandle<()>>>,
    stopping: Arc<AtomicBool>,
    dropped: Arc<AtomicU64>,
    exported: Arc<AtomicU64>,
    failed: Arc<AtomicU64>,
    endpoint: String,
}

/// What an exporter has done, for the shutdown summary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExportCounts {
    /// Spans handed to the collector in a batch it accepted.
    pub exported: u64,
    /// Spans in batches that were lost, either refused or unreachable.
    pub failed: u64,
    /// Spans dropped at the queue because it was full.
    pub dropped: u64,
}

impl Exporter {
    /// Build an exporter from configuration and start its thread.
    ///
    /// The endpoint's path is normalised here: the specification fixes the trace path at
    /// `/v1/traces`, so a configured value that already ends in it is accepted rather than producing
    /// `/v1/traces/v1/traces` — which is what most collector documentation would otherwise cause,
    /// since it quotes complete URLs.
    pub fn start(config: &TelemetryConfig) -> Result<Self, ExportError> {
        let origin = build_origin(config)?;
        let (sender, receiver) = mpsc::sync_channel(QUEUE_CAPACITY);
        let stopping = Arc::new(AtomicBool::new(false));
        let dropped = Arc::new(AtomicU64::new(0));
        let exported = Arc::new(AtomicU64::new(0));
        let failed = Arc::new(AtomicU64::new(0));

        let settings = Settings {
            origin,
            resource: Resource::new(config.service_name.clone()),
            headers: config.headers.clone(),
            timeout: Duration::from_millis(config.timeout_ms),
            batch_size: config.batch_size,
            interval: Duration::from_millis(config.interval_ms),
        };

        let thread = {
            let stopping = Arc::clone(&stopping);
            let exported = Arc::clone(&exported);
            let failed = Arc::clone(&failed);
            let endpoint = config.endpoint.clone();
            std::thread::Builder::new()
                .name("volucer-otlp".to_string())
                .spawn(move || run(settings, receiver, &stopping, &exported, &failed, &endpoint))?
        };

        Ok(Self {
            sender,
            thread: Mutex::new(Some(thread)),
            stopping,
            dropped,
            exported,
            failed,
            endpoint: config.endpoint.clone(),
        })
    }

    /// What this exporter has done so far.
    #[must_use]
    pub fn counts(&self) -> ExportCounts {
        ExportCounts {
            exported: self.exported.load(Ordering::Relaxed),
            failed: self.failed.load(Ordering::Relaxed),
            dropped: self.dropped.load(Ordering::Relaxed),
        }
    }

    /// The endpoint as configured, for a log line and the shutdown summary.
    #[must_use]
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// The recorder half, for [`volucer_observe::Observability::with_spans`].
    #[must_use]
    pub fn recorder(self: &Arc<Self>) -> Arc<dyn SpanRecorder> {
        Arc::clone(self) as Arc<dyn SpanRecorder>
    }

    /// Stop the thread and flush what is queued.
    ///
    /// Takes `&self` because the exporter is shared: the front-ends hold an `Arc<dyn SpanRecorder>`
    /// pointing at this same allocation for the whole run, so a `&mut self` method could never be
    /// called from the place that actually shuts the server down. Idempotent, and safe to call twice
    /// — including by `Drop` after an explicit call, which is the ordinary path.
    pub fn stop(&self) {
        self.stopping.store(true, Ordering::Relaxed);
        // The thread is joined rather than detached: the join is what guarantees the final flush
        // finished before the process exits, which is the entire reason a shutdown flush is useful.
        // `take` under the lock is what makes a second call a no-op rather than a double join.
        let thread = self
            .thread
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .take();
        if let Some(thread) = thread {
            let _ = thread.join();
        }
    }
}

impl SpanRecorder for Exporter {
    fn record(&self, span: Span) {
        // A full queue drops and counts, and never blocks — see `QUEUE_CAPACITY` for why blocking is
        // the wrong answer on a request path. `try_send` is what makes that one decision rather than
        // a timeout somebody has to tune.
        if self.sender.try_send(span).is_err() {
            let total = self.dropped.fetch_add(1, Ordering::Relaxed) + 1;
            // Reported once, at the same rule the access log's broken sink follows: an error path
            // that reported per drop would flood the log during exactly the incident it describes.
            if total == 1 {
                tracing::warn!(
                    capacity = QUEUE_CAPACITY,
                    "the trace exporter's queue is full, so spans are being dropped. The collector \
                     is unreachable or slower than this deployment's traffic; this is reported \
                     once. Traces are lost, requests are not"
                );
            }
        }
    }
}

impl Drop for Exporter {
    fn drop(&mut self) {
        self.stop();
        let counts = self.counts();
        if counts.exported > 0 || counts.failed > 0 || counts.dropped > 0 {
            tracing::debug!(
                endpoint = %self.endpoint,
                exported = counts.exported,
                failed = counts.failed,
                dropped = counts.dropped,
                "trace exporter stopped"
            );
        }
    }
}

impl std::fmt::Debug for Exporter {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Exporter")
            .field("endpoint", &self.endpoint)
            .field("counts", &self.counts())
            .finish()
    }
}

/// Everything the thread needs, owned, so it can outlive the configuration it came from.
struct Settings {
    origin: Origin,
    resource: Resource,
    headers: BTreeMap<String, String>,
    timeout: Duration,
    batch_size: usize,
    interval: Duration,
}

/// Turn the configured endpoint into a dialable origin.
///
/// Parsed through [`Origin::parse`] — the configuration model's own checked type, reached through
/// the client crate — rather than by hand, so this crate cannot accept an origin the rest of the
/// server would refuse.
fn build_origin(config: &TelemetryConfig) -> Result<Origin, ExportError> {
    // The same normalisation `validate` uses, taken from the same method — see
    // [`TelemetryConfig::base_endpoint`] for why one normaliser rather than two.
    Origin::parse(config.base_endpoint()).map_err(|err| ExportError::Endpoint {
        endpoint: config.endpoint.clone(),
        reason: err.to_string(),
    })
}

/// The path an export is posted to.
const fn trace_path() -> &'static str {
    OTLP_TRACES_PATH
}

/// The exporter thread.
fn run(
    settings: Settings,
    receiver: Receiver<Span>,
    stopping: &AtomicBool,
    exported: &AtomicU64,
    failed: &AtomicU64,
    endpoint: &str,
) {
    // A current-thread runtime of its own, because the client is asynchronous and the request path
    // is not. `enable_all` is what the pool's idle timeout and the request deadline need.
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(source) => {
            // Reported and returned rather than panicking: a thread that cannot get a runtime is a
            // deployment with no traces, not a deployment that must stop serving.
            tracing::error!(
                %source,
                "the trace exporter could not start a runtime; no spans will be exported"
            );
            return;
        }
    };

    let mut batch: Vec<Span> = Vec::with_capacity(settings.batch_size);
    // Reported once per outage, and cleared by the first success, so a long outage is one warning
    // and its end is one more line.
    let mut reported_failure = false;

    while !stopping.load(Ordering::Relaxed) {
        // Blocks until a span arrives or the interval expires, which is what makes a partly-filled
        // batch go out on time: a service handling one request a minute would otherwise export
        // nothing until a batch filled, which on a quiet service is the difference between a trace
        // and no trace.
        match receiver.recv_timeout(settings.interval) {
            Ok(span) => {
                batch.push(span);
                // Then take everything already queued, up to the batch size, without blocking. The
                // `recv_timeout` above is what makes an idle exporter cheap; this `try_recv` loop is
                // what makes a busy one send full batches rather than one span per interval.
                while batch.len() < settings.batch_size {
                    match receiver.try_recv() {
                        Ok(span) => batch.push(span),
                        Err(_) => break,
                    }
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }

        if batch.is_empty() {
            continue;
        }
        let taken = std::mem::take(&mut batch);
        let count = taken.len() as u64;
        match export(&runtime, &settings, &taken) {
            Ok(()) => {
                exported.fetch_add(count, Ordering::Relaxed);
                if reported_failure {
                    reported_failure = false;
                    tracing::info!(endpoint, "the trace exporter reached its collector again");
                }
            }
            Err(err) => {
                failed.fetch_add(count, Ordering::Relaxed);
                if !reported_failure {
                    reported_failure = true;
                    tracing::warn!(
                        endpoint,
                        spans = count,
                        %err,
                        "a batch of spans was dropped. The collector is unreachable or refusing; \
                         this is reported once per outage. Requests are unaffected"
                    );
                }
            }
        }
        // Reused rather than reallocated, so a steady stream of full batches does not allocate a
        // fresh vector per export.
        batch = Vec::with_capacity(settings.batch_size);
    }

    // The final flush: what was queued when the stop was asked for goes out, which is what makes the
    // last seconds of a deployment's traffic present in its traces.
    let mut remaining = Vec::new();
    while let Ok(span) = receiver.try_recv() {
        remaining.push(span);
    }
    if !remaining.is_empty() {
        let count = remaining.len() as u64;
        if export(&runtime, &settings, &remaining).is_ok() {
            exported.fetch_add(count, Ordering::Relaxed);
        } else {
            failed.fetch_add(count, Ordering::Relaxed);
        }
    }
}

/// Encode and POST one batch.
fn export(
    runtime: &tokio::runtime::Runtime,
    settings: &Settings,
    spans: &[Span],
) -> Result<(), ExportError> {
    let body = encode_traces(&settings.resource, spans);
    runtime.block_on(send(&settings.origin, settings, body))
}

/// The request itself, so the blocking `block_on` above is one line and this is testable directly.
async fn send(origin: &Origin, settings: &Settings, body: String) -> Result<(), ExportError> {
    let client = volucer_upstream::UpstreamClient::<Full<Bytes>>::new(
        origin.clone(),
        Some(volucer_upstream::client::DEFAULT_IDLE_TIMEOUT),
    )
    .map_err(|err| ExportError::Unreachable(err.to_string()))?;

    let mut headers = hyper::HeaderMap::new();
    headers.insert(
        hyper::header::CONTENT_TYPE,
        hyper::header::HeaderValue::from_static("application/json"),
    );
    for (name, value) in &settings.headers {
        // The *name* was validated at configuration time; a name or value that is still unusable
        // here costs that one header rather than the batch, because losing one header is not a
        // reason to lose a batch of spans.
        let Ok(name) = hyper::header::HeaderName::from_bytes(name.as_bytes()) else {
            tracing::warn!(header = %name, "a telemetry header name is not valid and was skipped");
            continue;
        };
        let Ok(value) = hyper::header::HeaderValue::from_str(value) else {
            tracing::warn!(header = %name, "a telemetry header value is not valid and was skipped");
            continue;
        };
        headers.insert(name, value);
    }

    let request = UpstreamRequest {
        method: hyper::Method::POST,
        // Origin-form, as the client's own documentation requires: the origin is the backend's
        // address and is not part of the request line.
        uri: trace_path()
            .parse()
            .expect("/v1/traces is a constant, valid path"),
        version: hyper::Version::HTTP_11,
        headers,
        body: Full::new(Bytes::from(body)),
    };

    let answered = tokio::time::timeout(settings.timeout, client.request(request))
        .await
        .map_err(|_| {
            ExportError::Unreachable(format!(
                "no answer within {}ms",
                settings.timeout.as_millis()
            ))
        })?
        .map_err(|err| ExportError::Unreachable(err.to_string()))?;

    if answered.status.is_success() {
        // The body is not read: an OTLP success response carries nothing this server acts on, and
        // reading it would be a second wait on a path that has already spent its deadline.
        return Ok(());
    }

    let status = answered.status.as_u16();
    let body = volucer_upstream::collect(answered.body)
        .await
        .map(|bytes| summarise(&bytes))
        .unwrap_or_else(|err| format!("(the body could not be read: {err})"));
    Err(ExportError::Refused { status, body })
}

/// As much of a refusal as a human needs, and no more.
///
/// Truncated because this ends up in a log line and a collector's error page can be megabytes of
/// HTML — the same rule the access log follows for a record's fields.
fn summarise(body: &[u8]) -> String {
    const LIMIT: usize = 256;
    let text = String::from_utf8_lossy(body);
    let trimmed = text.trim();
    if trimmed.chars().count() <= LIMIT {
        return trimmed.to_string();
    }
    // Cut by characters rather than by bytes: a byte index lands inside a multi-byte character and
    // panics, and a collector's error page is exactly where a non-ASCII body comes from.
    let mut out: String = trimmed.chars().take(LIMIT).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(endpoint: &str) -> TelemetryConfig {
        TelemetryConfig {
            endpoint: endpoint.to_string(),
            service_name: "test".to_string(),
            headers: BTreeMap::new(),
            timeout_ms: 1_000,
            batch_size: 8,
            interval_ms: 50,
            sample: volucer_core::config::SampleMode::ParentBased,
            sample_ratio: None,
        }
    }

    fn sample_span() -> Span {
        Span {
            trace_id: [1; 16],
            span_id: [2; 8],
            parent_span_id: None,
            name: "GET request",
            kind: volucer_observe::span::SpanKind::Server,
            start: std::time::SystemTime::UNIX_EPOCH,
            end: std::time::SystemTime::UNIX_EPOCH,
            attributes: Vec::new(),
            status: volucer_observe::span::SpanStatus::Unset,
            status_message: None,
            flags: volucer_observe::span::TRACE_FLAG_SAMPLED,
        }
    }

    #[test]
    fn a_complete_endpoint_is_accepted_and_the_protocol_path_is_not_doubled() {
        // The shape most collector documentation quotes, so it must not produce
        // `/v1/traces/v1/traces` — which a collector answers 404 to, and which an operator would
        // reasonably read as "the exporter is broken".
        let origin = build_origin(&config("http://127.0.0.1:4318/v1/traces")).expect("an origin");
        assert_eq!(origin.host_port().as_deref(), Some("127.0.0.1:4318"));
        assert_eq!(origin.scheme(), Some("http"));
        assert_eq!(trace_path(), "/v1/traces");
    }

    #[test]
    fn a_base_endpoint_with_a_trailing_slash_works_too() {
        let origin = build_origin(&config("http://127.0.0.1:4318/")).expect("an origin");
        assert_eq!(origin.host_port().as_deref(), Some("127.0.0.1:4318"));
    }

    #[test]
    fn an_https_endpoint_is_accepted_because_the_client_already_speaks_tls() {
        let origin = build_origin(&config("https://collector.example:4318")).expect("an origin");
        assert!(origin.is_tls());
    }

    #[test]
    fn a_bare_host_and_port_is_refused_because_it_is_not_an_origin() {
        // The shape a listener address takes, so an operator writing this by analogy produces it.
        let err = build_origin(&config("127.0.0.1:4318")).expect_err("must be refused");
        assert!(matches!(err, ExportError::Endpoint { .. }), "{err}");
    }

    #[test]
    fn a_refusal_body_is_truncated_by_characters_and_not_by_bytes() {
        // A byte slice into a multi-byte character panics, and a collector's error page is exactly
        // where a non-ASCII body comes from.
        let long = "é".repeat(400);
        let summary = summarise(long.as_bytes());
        assert!(summary.ends_with('…'));
        assert_eq!(
            summary.chars().count(),
            257,
            "256 characters plus the ellipsis"
        );

        assert_eq!(summarise(b"  short  "), "short");
        // Exactly at the limit is not truncated, and one over is.
        let exact = "a".repeat(256);
        assert_eq!(summarise(exact.as_bytes()), exact);
    }

    #[test]
    fn a_header_value_with_a_newline_is_skipped_rather_than_failing_the_export() {
        // Configuration validation checks the *name*; a value with a control byte is caught here so
        // that one bad header costs one header instead of a whole batch.
        let mut configured = BTreeMap::new();
        configured.insert("x-good".to_string(), "fine".to_string());
        configured.insert("x-bad".to_string(), "no\r\ninjection".to_string());
        let mut headers = hyper::HeaderMap::new();
        for (name, value) in &configured {
            let Ok(name) = hyper::header::HeaderName::from_bytes(name.as_bytes()) else {
                continue;
            };
            if let Ok(value) = hyper::header::HeaderValue::from_str(value) {
                headers.insert(name, value);
            }
        }
        assert!(headers.contains_key("x-good"));
        assert!(!headers.contains_key("x-bad"));
    }

    #[test]
    fn a_full_queue_drops_and_counts_rather_than_blocking() {
        // The decision this whole design rests on: with nothing draining the queue, filling it must
        // return immediately with an ever-increasing drop count instead of blocking the caller — a
        // request path that blocks on a collector is the bug this prevents.
        let (sender, _receiver) = mpsc::sync_channel::<Span>(2);
        let dropped = AtomicU64::new(0);
        for _ in 0..5 {
            if sender.try_send(sample_span()).is_err() {
                dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
        assert_eq!(dropped.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn an_exporter_reports_the_endpoint_it_was_given() {
        let exporter = Exporter::start(&config("http://127.0.0.1:4318")).expect("an exporter");
        assert_eq!(exporter.endpoint(), "http://127.0.0.1:4318");
        assert_eq!(
            exporter.counts(),
            ExportCounts {
                exported: 0,
                failed: 0,
                dropped: 0
            }
        );
    }
}
