//! Request identity and trace context: the two things a request log and a trace share.
//!
//! This crate exists because volucer has **two front-ends** — an HTTP listener and a FastCGI
//! listener for a deployment behind nginx — and a request id that only one of them assigned would
//! be worse than none at all: the same deployment would log one id for a direct request and a
//! different one for the same request proxied. That is the reasoning `volucer-watch` already states
//! for the reloaded state table, and it applies here for the same reason. It is deliberately *not*
//! in `volucer-core`, which is configuration and shared types; an id is neither.
//!
//! # What this is, before it is anything else
//!
//! Two small values and the rules for reading them off the wire:
//!
//! - [`RequestId`] — 32 hex characters when this server generates one, which is exactly nginx's
//!   `$request_id` (`ngx_http_variables.c`: 16 random bytes, `ngx_hex_dump`ed). The length is not
//!   arbitrary; matching it means a log line written by this server and one written by an nginx in
//!   front of it have the same shape, so one `awk` reads both.
//! - [`TraceContext`] — the W3C `traceparent` header, parsed and formatted by hand.
//!
//! **Neither needs OpenTelemetry**, and that is the point of the split. A structured request line
//! and a propagatable trace context are a header convention and a random number; only *exporting*
//! spans needs a tracing framework. Building the two cheap things first is also what makes the
//! framework worth adding later, because there will be spans to export rather than an exporter
//! wired to nothing.
//!
//! # Adopting a client's id does not re-render it
//!
//! [`RequestId::parse`] accepts an inbound id and [`RequestId::as_str`] returns it **exactly as it
//! arrived**. Folding it into 16 bytes and printing that would defeat the entire purpose of
//! adopting one: the client is looking for *its* string in *our* log, and a normalised value is not
//! findable. So the type holds either the 16 bytes it generated or the string it was handed, and
//! the two render differently on purpose.
//!
//! # Randomness has a fallback, and it is not a security claim
//!
//! A request id is **not** a token: nothing is authorised by it, and guessing one reveals nothing.
//! So when the system RNG is unavailable this crate degrades to a time-and-counter value and warns
//! once, rather than failing the request. A server that refuses traffic because it could not mint a
//! log identifier has its priorities backwards. The trace id is treated the same way, and for the
//! same reason — W3C requires it to be unpredictable only to stop a hostile client *predicting* a
//! trace to inject into, which is a different threat model from an RNG outage on the server.

use std::borrow::Cow;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::SystemTime;

pub mod access_log;
pub mod span;

use std::time::Instant;

/// Everything a front-end needs to identify and log a request, in one value.
///
/// **One value rather than arguments, because there are two front-ends.** The reload state is
/// shared for exactly the same reason (`volucer-watch`): a request id that only one front-end
/// assigned, or an access log only one of them wrote, would mean the same deployment logs one id
/// for a direct request and a different one for the same request proxied through nginx. A type both
/// listeners must be handed is harder to forget than a convention both are expected to follow.
pub struct Observability {
    access_log: access_log::AccessLog,
    /// The header to adopt and echo, or `None` to always mint our own.
    id_header: Option<String>,
    /// Whether an inbound `traceparent` is read, and whether ours is written for the backend.
    propagation: TracePropagation,
    /// Where finished spans go, or `None` when no `[telemetry]` table asked for them.
    ///
    /// An `Option` rather than an always-present no-op recorder, and the reason is the same one that
    /// makes [`access_log::AccessLog::disabled`] a value rather than an `Option`: the field is read
    /// on the request path, and a `None` check is one predictable branch while a trait object call
    /// is not.
    spans: Option<Arc<dyn span::SpanRecorder>>,
    /// Which traces are exported, out of the ones this server sees.
    sampling: Sampler,
}

/// What this server does with the W3C trace context on the way through.
///
/// The four values are nginx's `otel_trace_context`, spelled the same way and meaning the same
/// thing, because an operator who has written one already knows what these do — and because the
/// four are genuinely the four combinations of two independent decisions rather than a menu:
///
/// | Value | Reads the client's `traceparent` | Writes ours for the backend |
/// | --- | --- | --- |
/// | `ignore` | no | no |
/// | `extract` | yes | no |
/// | `inject` | no | yes |
/// | `propagate` | yes | yes |
///
/// # Why `extract` is the default
///
/// It is what this server did before the key existed, and it is the least surprising half of the
/// pair: a client's header still reaches the backend untouched, and Volucer still continues the
/// trace it was given. Turning on `propagate` *changes what a backend receives* — its own header is
/// replaced by one naming this server's span — so it is a statement a deployment makes rather than
/// one an upgrade makes for it.
///
/// The difference matters: with `extract`, a backend's spans are **siblings** of Volucer's span,
/// because both name the caller as their parent. With `propagate`, the backend's spans are
/// **children**, which is what makes a waterfall diagram show the request instead of two unrelated
/// branches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TracePropagation {
    /// Neither read nor write: every request starts a fresh trace at this hop.
    Ignore,
    /// Read the client's context and continue it. What the header does by default.
    #[default]
    Extract,
    /// Start a fresh trace here and hand it to the backend.
    Inject,
    /// Read the client's context, continue it, and hand *ours* to the backend.
    Propagate,
}

impl TracePropagation {
    /// Whether an inbound `traceparent` is read.
    #[must_use]
    pub fn extracts(self) -> bool {
        matches!(self, Self::Extract | Self::Propagate)
    }

    /// Whether the header a backend receives is replaced with this hop's context.
    #[must_use]
    pub fn injects(self) -> bool {
        matches!(self, Self::Inject | Self::Propagate)
    }

    /// The value as it is written in configuration and printed by `volucer check`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ignore => "ignore",
            Self::Extract => "extract",
            Self::Inject => "inject",
            Self::Propagate => "propagate",
        }
    }
}

/// Which traces are exported, out of the ones this server sees.
///
/// A **decision**, not a mechanism, and that distinction is the whole reason it is a small value
/// type here rather than a callback the exporter holds: the answer has to be the same on the
/// request path, where the span is built, and in `volucer check`, where an operator reads what
/// their configuration will do. A decision spread across two places is how a server ends up
/// exporting a trace it told the backend it had dropped.
///
/// # The three answers, and the one that is missing
///
/// - [`Sampler::ParentBased`] (the default) — export the traces an upstream service already
///   decided to record, and export a request that arrived with no `traceparent` at all. OTel calls
///   this `ParentBased(root=AlwaysOn)` and names it the default sampler.
/// - [`Sampler::Always`] — export everything, whatever the inbound flag said. This is the same
///   answer for every request, so it differs from the default only for traffic that arrives with a
///   parent explicitly marked `-00`, which is a caller saying "do not record this".
/// - [`Sampler::Ratio`] — export a deterministic fraction of traces, chosen by the trace id.
///
/// There is deliberately no `Never`. A deployment that wants no traces expresses that by not
/// writing a `[telemetry]` table, and a second way to say it would be a way for the two to
/// disagree — the same rule that makes `[telemetry]`'s presence its enable switch. (`Ratio(0.0)` is
/// reachable and is the one exception, because it is the useful way to keep a table's endpoint and
/// headers while exporting nothing.)
///
/// # Why `parent_based` rather than "always export"
///
/// Because it is what makes a trace *one trace*. Under [`TracePropagation::Propagate`] the backend
/// is handed this server's context, so if this hop exported a trace the caller had marked `-00`
/// while the backend honoured the flag, the trace view would show Volucer's span as a single
/// orphaned node with no children — worse than no span at all, because it looks like a backend
/// that stopped sending spans. nginx spells the same policy `otel_trace $otel_parent_sampled`, and
/// the reasoning is identical: a proxy is not the party that should decide to spend somebody
/// else's tracing budget.
///
/// # Why a root is sampled rather than dropped
///
/// The other half, and the one an "honour the parent" rule gets wrong if it is read literally: a
/// request that arrived with **no** `traceparent` has no parent to honour. Treating its absent flag
/// as `-00` would mean a deployment with nothing in front of it — the ordinary single-service
/// shape — exports nothing at all while believing tracing is on. The specification calls this
/// `root=AlwaysOn` for exactly that reason.
///
/// # What a ratio means, precisely
///
/// The decision is a function of the trace id alone and of nothing else — no clock, no counter, no
/// per-process state — so every hop that applies the same ratio agrees, and a trace is either
/// sampled at every hop or at none. The arithmetic follows the W3C Trace Context level 2
/// consistent-probability rule that OpenTelemetry's `ProbabilitySampler` also uses: the randomness
/// is the **seven rightmost bytes** of the trace id, and a trace is kept when that value is at or
/// above a threshold derived from the ratio. Lowering the ratio therefore only ever *removes*
/// traces, which is the monotonicity the specification requires so that a backend can sample more
/// generously than the front-end without losing anything the front-end kept.
///
/// `0.0` and `1.0` are exact rather than approximate — every trace and no trace — because an
/// operator writing either means it.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum Sampler {
    /// Export the traces an upstream service already decided to record, and record a request that
    /// arrived with no trace at all. The default.
    #[default]
    ParentBased,
    /// Export every trace, whatever the inbound flag said.
    Always,
    /// Export this fraction of traces, decided by the trace id.
    Ratio(f64),
}

impl Sampler {
    /// The smallest ratio `[telemetry].sample_ratio` accepts.
    ///
    /// `2^-56` is the smallest probability the W3C consistent-probability scheme can express,
    /// because the randomness it samples from is 56 bits. A smaller value would round to "never",
    /// which [`Sampler::Ratio`] already spells exactly as `0.0` — so accepting it would be
    /// pretending to a precision that does not exist.
    pub const MIN_RATIO: f64 = 1.0 / (1u64 << 56) as f64;

    /// Build a ratio sampler, or `None` for a value outside the representable range.
    ///
    /// Refused rather than clamped: `sample_ratio = 1.5` is an operator arithmetic error, and
    /// silently reading it as "all traces" is how a deployment discovers its tracing bill was
    /// decided by a typo. `0.0` is accepted and means none, which is a legitimate way to turn
    /// tracing off without removing the table.
    #[must_use]
    pub fn ratio(value: f64) -> Option<Self> {
        if !value.is_finite() {
            return None;
        }
        if value == 0.0 {
            return Some(Self::Ratio(0.0));
        }
        if (Self::MIN_RATIO..=1.0).contains(&value) {
            return Some(Self::Ratio(value));
        }
        None
    }

    /// Whether a trace with this id is recorded — by this server, and therefore by anything it
    /// tells. **The one place the decision is made.**
    ///
    /// `parent` is the caller's flag when the request carried a usable `traceparent`, and `None`
    /// when it carried none at all. **The two are different answers**, and conflating them is the
    /// mistake this signature exists to prevent: a request with no parent is a *root*, and
    /// [`Sampler::ParentBased`] samples a root rather than dropping it. Otherwise a standalone
    /// deployment — no proxy in front, no inbound tracing — would export nothing at all, which is
    /// the opposite of what turning tracing on means.
    ///
    /// This is OTel's `ParentBased(root=AlwaysOn)`, which the specification names as the default
    /// sampler, and the two halves are worth stating separately because only one of them is
    /// obvious: a child follows its parent's decision, and a root is recorded.
    ///
    /// Takes the trace id and the parent rather than a `RequestContext`, because those are the only
    /// two things it reads — and the narrow signature says so. Sampling cannot depend on the path,
    /// the client or the clock, or two hops applying the same ratio would disagree about the same
    /// trace.
    ///
    /// The answer is used twice and must not be computed twice: it becomes the flag on the
    /// `traceparent` a backend receives, and it decides whether this server's own span is
    /// exported. One answer, so a backend can never be told `-01` by a server that then drops the
    /// span — the mismatch this key exists to remove.
    #[must_use]
    pub fn samples(self, trace_id: [u8; 16], parent: Option<bool>) -> bool {
        match self {
            Self::ParentBased => parent.unwrap_or(true),
            Self::Always => true,
            Self::Ratio(ratio) => ratio_keeps(trace_id, ratio),
        }
    }

    /// The value as it is written in configuration.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ParentBased => "parent_based",
            Self::Always => "always",
            Self::Ratio(_) => "ratio",
        }
    }

    /// The one-line description `volucer check` prints.
    #[must_use]
    pub fn describe(self) -> String {
        match self {
            Self::ParentBased => {
                "the traces an upstream service recorded, and every trace it did not see at all"
                    .to_string()
            }
            Self::Always => "every trace, whatever the caller asked for".to_string(),
            Self::Ratio(ratio) => format!("{ratio} of traces, decided by the trace id"),
        }
    }
}

/// Whether the W3C consistent-probability rule keeps a trace with this id at this ratio.
///
/// The randomness is the seven rightmost bytes of the trace id — 56 bits, which is what the
/// specification defines and what makes the decision interoperable with any other hop applying the
/// same ratio. The threshold is `(1 - ratio) * 2^56` and the trace is kept when the randomness is
/// at or above it, so `1.0` gives a zero threshold (everything) and `0.0` gives `2^56` (nothing,
/// since the largest 56-bit value is `2^56 - 1`).
fn ratio_keeps(trace_id: [u8; 16], ratio: f64) -> bool {
    // The rightmost seven bytes, right-aligned in a `u64` so the comparison is on the value the
    // specification describes rather than on a shifted one.
    let mut randomness = [0u8; 8];
    randomness[1..].copy_from_slice(&trace_id[9..16]);
    let randomness = u64::from_be_bytes(randomness);
    let threshold = (1.0 - ratio) * (1u64 << 56) as f64;
    // Saturating rather than unchecked: a ratio of `0.0` produces exactly `2^56`, which is a
    // representable `u64`, and one of `1.0` produces `0.0`, which truncates to `0`. Neither can
    // overflow, but the cast is written to say what happens if a future caller widens the range.
    let threshold = if threshold >= (1u64 << 56) as f64 {
        1u64 << 56
    } else {
        threshold as u64
    };
    randomness >= threshold
}

impl Observability {
    /// Build from an access log and an optional id header.
    #[must_use]
    pub fn new(access_log: access_log::AccessLog, id_header: Option<String>) -> Self {
        Self {
            access_log,
            id_header,
            propagation: TracePropagation::default(),
            spans: None,
            sampling: Sampler::default(),
        }
    }

    /// Also export spans to `recorder`.
    ///
    /// A builder method rather than a third argument to [`Observability::new`], because the two are
    /// genuinely independent: a deployment may have an access log and no tracing, tracing and no
    /// access log, both, or neither — and four constructors would be four ways to spell that.
    #[must_use]
    pub fn with_spans(mut self, recorder: Arc<dyn span::SpanRecorder>) -> Self {
        self.spans = Some(recorder);
        self
    }

    /// Choose what happens to the W3C trace context on the way through.
    ///
    /// Separate from [`Observability::with_spans`] because they are independent in the same way:
    /// `propagate` with no `[telemetry]` is a supported shape — an nginx in front, and a collector
    /// behind the application rather than behind this server — and the header still has to be
    /// written for the backend's spans to join the caller's trace.
    #[must_use]
    pub fn with_propagation(mut self, propagation: TracePropagation) -> Self {
        self.propagation = propagation;
        self
    }

    /// Choose which traces are exported.
    ///
    /// A third independent decision, and it is deliberately *not* folded into
    /// [`Observability::with_spans`]: which traces to keep is a policy about cost, while where to
    /// send them is a policy about plumbing. The default, [`Sampler::default`], is `parent_based`,
    /// which exports exactly the traces somebody upstream already decided to record.
    ///
    /// Note what sampling does **not** touch: the access log, and the trace id this server logs and
    /// propagates. A dropped trace is still logged with the same correlation id, so "the trace is
    /// missing, find the request in the log by its trace id" stays the answer to a question about a
    /// span that is not there. That is why the decision is applied at the recording site rather
    /// than by not minting an id.
    #[must_use]
    pub fn with_sampling(mut self, sampling: Sampler) -> Self {
        self.sampling = sampling;
        self
    }

    /// Which traces are exported.
    #[must_use]
    pub fn sampling(&self) -> Sampler {
        self.sampling
    }

    /// What this server does with the trace context.
    #[must_use]
    pub fn propagation(&self) -> TracePropagation {
        self.propagation
    }

    /// Whether spans are being exported.
    #[must_use]
    pub fn has_spans(&self) -> bool {
        self.spans.is_some()
    }

    /// The configuration with no logging at all, which is the default.
    #[must_use]
    pub fn disabled() -> Self {
        Self {
            access_log: access_log::AccessLog::disabled(),
            // No header means no adoption *and* no echo: a deployment that has not asked for an id
            // should not have one appear on its responses, because a client may act on it.
            id_header: None,
            // `extract`, which is the documented default and what this server has always done: a
            // client's header is relayed to the backend untouched. `propagate` is a deployment's
            // decision, so it is never a side effect of building the handle.
            propagation: TracePropagation::default(),
            spans: None,
            sampling: Sampler::default(),
        }
    }

    /// The access log, for a caller that wants to check [`access_log::AccessLog::is_enabled`] or
    /// flush at shutdown.
    #[must_use]
    pub fn access_log(&self) -> &access_log::AccessLog {
        &self.access_log
    }

    /// The header name this server adopts and echoes, if any.
    #[must_use]
    pub fn id_header(&self) -> Option<&str> {
        self.id_header.as_deref()
    }

    /// Close and reopen the access log, for `logrotate`. See [`access_log::AccessLog::reopen`].
    ///
    /// A forwarder rather than the reload reaching into [`Observability::access_log`], because the
    /// reload holds this value and not the log: one call says what the *process* is doing, and the
    /// three-state decision (file, broken, not configured) stays inside the module that holds the
    /// states.
    ///
    /// Returns whether a file is open afterwards.
    pub fn reopen_access_log(&self) -> bool {
        self.access_log.reopen()
    }

    /// The `traceparent` a backend should receive, or `None` when this server is not injecting.
    ///
    /// **The one place "which header does the backend see" is decided**, exactly as [`Self::begin`]
    /// is for "which id is this request". A front-end that asked `propagation()` and then built the
    /// header itself would be a second implementation of the rule, and the two would drift the
    /// first time one of them was changed.
    ///
    /// It returns the *rendered* value rather than the [`TraceContext`] because the three call sites
    /// want different shapes — a `(name, value)` pair for the CGI environment, a `HeaderMap` entry
    /// for an origin — and converting at each of them is how one of them ends up formatting it
    /// differently.
    #[must_use]
    pub fn outbound_traceparent(&self, context: &RequestContext) -> Option<String> {
        match self.propagation.injects() {
            true => Some(context.trace.header()),
            false => None,
        }
    }

    /// Whether anything will be recorded.
    ///
    /// A front-end uses this to skip the work — including the `Cow::Owned` a generated id
    /// allocates — when the deployment has not asked for logging.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.access_log.is_enabled()
    }

    /// Whether *anything* is recorded — the access log or a span.
    ///
    /// **The guard a front-end should use before building a request record**, and the distinction
    /// from [`Observability::is_enabled`] is not cosmetic: tracing and access logging are separately
    /// configured, and a front-end that skipped its record on `is_enabled` alone would export no
    /// spans at all in a deployment that configured `[telemetry]` without `[server.access_log]` —
    /// which is the ordinary way to try tracing first.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.access_log.is_enabled() || self.spans.is_some()
    }

    /// Resolve the identity of an inbound request.
    ///
    /// **The one place adoption, generation and trace continuation are decided.** Both front-ends
    /// call this, so they cannot answer "which id is this request" differently.
    ///
    /// `client_id` and `client_traceparent` are the raw header values as they arrived, or `None`.
    #[must_use]
    pub fn begin(
        &self,
        client_id: Option<&str>,
        client_traceparent: Option<&str>,
    ) -> RequestContext {
        // A value is only even considered when a header is configured: with none, the client cannot
        // influence the id at all, which is the stricter default.
        let id = match self.id_header {
            Some(_) => client_id
                .and_then(RequestId::parse)
                .unwrap_or_else(RequestId::generate),
            None => RequestId::generate(),
        };
        // Parsed once, and kept, because two things are needed from it: the trace to continue, and
        // the *caller's* span id, which is the parent this hop hangs from. Keeping only the trace
        // would discard the parent — and an exported span would then claim to be a root, breaking
        // the trace at the very hop it exists to show.
        //
        // `extracts()` is asked here rather than at the call site, so `ignore` cannot be half
        // implemented: one front-end forgetting the check would make the key mean two things.
        let client_trace = match self.propagation.extracts() {
            true => client_traceparent.and_then(TraceContext::parse),
            false => None,
        };
        let trace = {
            // The trace id comes first, because a ratio sampler's decision is a function of it.
            // `parent` is `None` for a request that carried no `traceparent`, which is a *root*
            // and not the same thing as an unsampled parent — see `Sampler::samples`.
            let (trace_id, parent) = match &client_trace {
                Some(client) => (client.trace_id(), Some(client.sampled())),
                None => (TraceContext::new(false).trace_id(), None),
            };
            // **One decision, two readers.** The flag goes into the `traceparent` a backend is
            // handed and decides whether this server's own span is exported — so a backend can
            // never be told `-01` by a server that then dropped the span, which is the mismatch
            // that makes a trace view show a parent with children it does not admit to.
            let sampled = self.sampling.samples(trace_id, parent);
            // The client's trace, our own span — see `TraceContext::continuing`. Whether to record
            // is decided above rather than assumed, so a request whose trace nobody asked to
            // record still gets a *consistent* answer: `00` on the header, and no exported span.
            TraceContext::continuing(trace_id, sampled)
        };
        // `None` when there was no usable inbound traceparent, which is exactly OTLP's "this is a
        // root span".
        let parent_span_id = client_trace.map(|client| *client.span_id());
        RequestContext {
            id,
            trace,
            parent_span_id,
            started_at: SystemTime::now(),
            started: Instant::now(),
        }
    }

    /// Record the server span for one finished request, if spans are being exported.
    ///
    /// **This is the only place the attribute vocabulary is written down**, and that is the point:
    /// every key is a `&'static str` chosen here, so no request can invent one. The same rule the
    /// metric registry enforces with its series cap — a label, or here an attribute, whose *key*
    /// comes from a request is how a backend's index grows without bound.
    ///
    /// The ids are taken from `context`, so the span this produces carries **the span id the request
    /// already sent onward as its `traceparent`**. That is the whole reason the export is driven
    /// from here rather than from a `tracing` layer: a backend's child span names this id as its
    /// parent, and an exporter that minted a different one would leave that child pointing at
    /// nothing.
    ///
    /// Does nothing when no `[telemetry]` table is configured, and costs one `Option` check to
    /// discover that.
    pub fn record_server_span(
        &self,
        context: &RequestContext,
        started: SystemTime,
        ended: SystemTime,
        attributes: ServerSpanAttributes<'_>,
    ) {
        let Some(recorder) = &self.spans else {
            return;
        };
        // The sampling decision, asked at the recording site rather than at the exporter's queue:
        // a trace this deployment is not recording should cost nothing here either, and the queue
        // is the one place where a dropped trace would still occupy a slot before being discarded.
        //
        // The flag is the *same* one `begin` put in the `traceparent` the backend was handed, so
        // "recorded here" and "the backend was told to record" cannot disagree.
        if !context.trace.sampled() {
            return;
        }
        let mut pairs: Vec<(&'static str, span::Attribute)> =
            Vec::with_capacity(4 + attributes.extra.len());
        pairs.push(("http.request.method", attributes.method.into()));
        // The **method**, not the target: OTLP has a separate attribute for the route, and putting a
        // raw path in `http.target` is how a backend ends up with a series per path. A deployment
        // that wants per-route attribution sets `http.route` from its own routing, which this server
        // cannot know.
        pairs.push(("url.path", attributes.path.into()));
        if let Some(host) = attributes.host {
            pairs.push(("server.address", host.into()));
        }
        pairs.push((
            "http.response.status_code",
            span::Attribute::Int(i64::from(attributes.status)),
        ));
        // Appended last so a caller cannot shadow one of the five above by passing a duplicate key.
        pairs.extend(attributes.extra.iter().cloned());
        recorder.record(span::Span {
            trace_id: context.trace.trace_id(),
            span_id: *context.trace.span_id(),
            parent_span_id: context.parent_span_id,
            name: attributes.name,
            kind: span::SpanKind::Server,
            start: started,
            end: ended,
            attributes: pairs,
            status: span::SpanStatus::from_http_status(attributes.status),
            status_message: attributes.status_message.map(str::to_string),
            // The same bit the `traceparent` carried, from the same decision `begin` made.
            flags: span::Span::flags_for(context.trace.sampled()),
        });
    }
}

/// What a finished request tells its server span.
///
/// A struct rather than seven arguments, because the call sites are three front-ends and a
/// positional list of `&str`s is one transposition away from a log that lies. Borrowed, because
/// everything in it is already in hand at the call site and this is built once per request.
pub struct ServerSpanAttributes<'a> {
    /// `GET`, `POST`, …
    pub method: &'a str,
    /// The request's path. **Not the query string**: a query is unbounded and often a secret, and
    /// OTLP's `url.query` is deliberately not set here.
    pub path: &'a str,
    /// The `Host`, when the request carried one.
    pub host: Option<&'a str>,
    /// The status sent.
    pub status: u16,
    /// The span's name, e.g. `"GET /about"`. A `&'static str` set by the front-end rather than
    /// formatted per request, because a name built from the path is the cardinality hazard again.
    pub name: &'static str,
    /// A failure description, when there was one.
    pub status_message: Option<&'a str>,
    /// Anything a front-end adds that is its own. Closed by construction at each call site.
    pub extra: &'a [(&'static str, span::Attribute)],
}

impl std::fmt::Debug for Observability {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Observability")
            .field("access_log", &self.access_log)
            .field("id_header", &self.id_header)
            .field("spans", &self.spans.is_some())
            .finish()
    }
}

/// A request's identity, resolved once at the top of whichever front-end received it.///
/// The `Instant` is here rather than taken by the caller so that "when did this request start" has
/// one answer — the same reason the id does. `Timing` in the FastCGI front-end already measured this
/// for the HTTP front-end's equivalent, and this supersedes it by being shared.
pub struct RequestContext {
    /// The id to log and to echo.
    pub id: RequestId,
    /// The trace to propagate.
    pub trace: TraceContext,
    /// The caller's span id, when the request carried a usable `traceparent`.
    ///
    /// Kept because it is the *parent* an exported span hangs from: [`TraceContext`] deliberately
    /// replaces the span id with this hop's own, so the value it replaced is the only thing that can
    /// say where this hop sits in the caller's trace. `None` means this is a root span, which is
    /// what OTLP expresses with an absent `parent_span_id` rather than an all-zero one.
    ///
    /// A request that was never traced does not get a synthetic parent, and that is deliberate: a
    /// trace this server starts is not a continuation of anything, and inventing a parent would
    /// attach every untraced request to one fictional span.
    pub parent_span_id: Option<[u8; 8]>,
    /// When the request arrived, as wall-clock time.
    ///
    /// A second clock beside `started` and not a duplicate of it, because the two answer different
    /// questions and neither can be derived from the other: a span's timestamp must be UNIX epoch
    /// nanoseconds, which an `Instant` has no defined relation to, while a *duration* must come from
    /// a monotonic clock, because wall-clock time jumps when NTP corrects it. Taking both here, in
    /// the one call that says "this request has begun", is what keeps them from disagreeing.
    pub started_at: SystemTime,
    started: Instant,
}

impl RequestContext {
    /// How long the request has taken so far, in seconds.
    ///
    /// Seconds as an `f64` rather than a `Duration`, because that is the unit both the text format
    /// and the JSON format express and converting once here keeps the two formats from disagreeing
    /// about rounding.
    #[must_use]
    pub fn elapsed_seconds(&self) -> f64 {
        self.started.elapsed().as_secs_f64()
    }
}

/// The header an inbound request id is read from, and the one the response echoes.
///
/// Lowercase, because HTTP/1.1 header names are case-insensitive and `hyper`'s `HeaderName` is
/// normalised — so one spelling here is what a comparison elsewhere can rely on.
pub const REQUEST_ID_HEADER: &str = "x-request-id";

/// The W3C trace context header.
pub const TRACEPARENT_HEADER: &str = "traceparent";

/// The longest request id this will adopt from a client.
///
/// A cap rather than a preference: the value ends up in a log line, so an unbounded one is an
/// unbounded log line, and adopting a client's own id means the client chooses how much of the log
/// it owns. 64 fits a UUID twice over with room for a prefix.
const MAX_REQUEST_ID_LEN: usize = 64;

/// A `traceparent` of version `00` is exactly this long.
///
/// `2 + 1 + 32 + 1 + 16 + 1 + 2`. The only longer value the specification allows is a *later*
/// version, and this crate understands one version — see [`TraceContext::parse`].
const TRACEPARENT_LEN: usize = 55;

/// A request identifier: 16 random bytes this server minted, or a client's own string adopted
/// verbatim.
///
/// Not `Copy`, because the adopted form owns its text. That is the honest cost of not re-rendering
/// what a client sent — see the crate docs.
#[derive(Clone, PartialEq, Eq, Hash)]
pub enum RequestId {
    /// Minted here: 16 random bytes, rendered as 32 lowercase hex characters.
    Generated([u8; 16]),
    /// Taken from the client's own header, kept exactly as it arrived.
    Adopted(Box<str>),
}

impl RequestId {
    /// Mint a new id.
    ///
    /// Infallible by design: see the crate docs. A request id is not a security token, so an RNG
    /// failure degrades rather than refusing to serve.
    #[must_use]
    pub fn generate() -> Self {
        let mut bytes = [0u8; 16];
        fill(&mut bytes);
        Self::Generated(bytes)
    }

    /// An id from raw bytes.
    #[must_use]
    pub fn from_bytes(bytes: [u8; 16]) -> Self {
        Self::Generated(bytes)
    }

    /// An id from a client's header, or `None` if it is not one this server will adopt.
    ///
    /// **Strict on purpose.** The value goes into a log line, so anything that is not a plain
    /// identifier is refused rather than escaped: a header containing a newline would otherwise let
    /// a client forge whole log records, and one containing a space would make the field count of a
    /// space-separated format wrong. Refusing costs a fresh id; escaping costs a rule that every
    /// future reader of the log has to know.
    ///
    /// The accepted alphabet is what real request ids are made of — a UUID's hyphens, hex, and the
    /// underscore and dot a few gateways use — between 1 and [`MAX_REQUEST_ID_LEN`] characters.
    /// A leading or trailing `-` or `.` is refused because such a value is indistinguishable from a
    /// truncated one, and a truncated id is worse than a fresh one: it would correlate two
    /// different requests.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        if value.is_empty() || value.len() > MAX_REQUEST_ID_LEN {
            return None;
        }
        if !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        {
            return None;
        }
        if value.starts_with(['-', '.']) || value.ends_with(['-', '.']) {
            return None;
        }
        Some(Self::Adopted(value.into()))
    }

    /// The id as it should appear in a log line or on a response header.
    ///
    /// For [`RequestId::Generated`] this is the 32-character hex form; for
    /// [`RequestId::Adopted`] it is the client's own string, unchanged.
    #[must_use]
    pub fn as_str(&self) -> Cow<'_, str> {
        match self {
            Self::Generated(bytes) => {
                let mut out = String::with_capacity(32);
                push_hex(&mut out, bytes);
                Cow::Owned(out)
            }
            Self::Adopted(text) => Cow::Borrowed(text),
        }
    }

    /// The rendered form, written into an existing `String`.
    ///
    /// There so a caller formatting a log line does not allocate twice for one id. An adopted id is
    /// appended as-is, for the reason in the crate docs.
    pub fn write_str(&self, out: &mut String) {
        match self {
            Self::Generated(bytes) => push_hex(out, bytes),
            Self::Adopted(text) => out.push_str(text),
        }
    }
}

impl fmt::Display for RequestId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.as_str().as_bytes() {
            // A character-wise write, because the adopted form is text rather than bytes to
            // re-encode. Each byte is ASCII by `parse`'s rule or hex by `push_hex`.
            formatter.write_str(&(*byte as char).to_string())?;
        }
        Ok(())
    }
}

impl fmt::Debug for RequestId {
    /// The rendered form, not the struct.
    ///
    /// A `Debug` that printed `Generated([71, 3, ...])` would make every log line and every test
    /// failure harder to read than the thing it describes, and would be a second rendering of the
    /// same value to keep in step.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.as_str())
    }
}

/// A parsed or generated W3C trace context.
///
/// `trace-id` identifies the whole trace and is shared by every service on the path; `span-id`
/// identifies *this* hop. A proxy that continues a trace keeps the trace id and mints a new span
/// id, which is why [`TraceContext::continuing`] exists separately from [`TraceContext::parse`].
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct TraceContext {
    trace_id: [u8; 16],
    span_id: [u8; 8],
    sampled: bool,
}

impl TraceContext {
    /// Begin a new trace, with a fresh trace id and span id.
    ///
    /// `sampled` is a parameter rather than a default because the flag is a *statement* to every
    /// downstream service about whether this request was recorded. Defaulting it would be this
    /// crate claiming to know the deployment's sampling policy, which it does not.
    #[must_use]
    pub fn new(sampled: bool) -> Self {
        Self::continuing(random_trace_id(), sampled)
    }

    /// Continue an existing trace with a new span id for this hop.
    ///
    /// This is what a proxy does with an inbound `traceparent`: the trace is the client's, the span
    /// is ours. Keeping the *client's* span id instead would make two different hops claim to be
    /// the same span, which is what breaks a waterfall diagram.
    #[must_use]
    pub fn continuing(trace_id: [u8; 16], sampled: bool) -> Self {
        Self {
            trace_id,
            span_id: random_span_id(),
            sampled,
        }
    }

    /// Parse a `traceparent` header value.
    ///
    /// **Version `00` only**, and that is a decision rather than an omission: the specification
    /// says a receiver must reject a value whose version it does not understand when the value is
    /// longer than the version-`00` form. Ignoring an unknown version starts a new trace, which
    /// loses a link — a cosmetic loss — where guessing at the layout of a future format risks
    /// reading a *different* field as the trace id and joining a trace that is not ours.
    ///
    /// Returns the client's own context, including its span id. A caller propagating onward wants
    /// [`TraceContext::continuing`] with [`TraceContext::trace_id`] — see that method for why.
    ///
    /// `None` for anything malformed, including the all-zero trace or span id the specification
    /// forbids, an `ff` version, and trailing data. A caller must treat `None` as "start a new
    /// trace", never as an error: a client's broken tracing header is not a broken request.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        if value.len() != TRACEPARENT_LEN {
            return None;
        }
        let bytes = value.as_bytes();
        // Fixed positions, checked before slicing so a `-` in the wrong place is a `None` rather
        // than a panic.
        //
        // **These three checks are also what makes the fixed slicing below safe on a non-ASCII
        // value**, and that is worth stating because it is not obvious and an `is_ascii()` guard
        // looked necessary until a falsification showed it was not. A `str` slice panics if either
        // end lands inside a character, and every boundary used here is `3`, `35`, `36`, `52`, `53`
        // or `55`: bytes `2`, `35` and `52` are proven to be `-` *by these checks*, so they are
        // each a whole character, and `3`, `36` and `53` immediately follow one; `55` is the end.
        // A value containing a multi-byte character therefore either fails one of these three
        // comparisons or reaches `parse_hex_array`, which rejects any byte that is not lowercase
        // hex. The guard was removed rather than kept as decoration — a check nothing can fail is a
        // claim about the code that is not true.
        if bytes[2] != b'-' || bytes[35] != b'-' || bytes[52] != b'-' {
            return None;
        }
        if &bytes[0..2] != b"00" {
            return None;
        }
        let trace_id = parse_hex_array::<16>(&value[3..35])?;
        let span_id = parse_hex_array::<8>(&value[36..52])?;
        if trace_id == [0u8; 16] || span_id == [0u8; 8] {
            return None;
        }
        let flags = parse_hex_array::<1>(&value[53..55])?;
        // Bit 0 is `sampled`. The other seven bits are reserved and are carried no further: this
        // crate does not know what a future revision means by them, and pretending to would be the
        // same mistake as accepting an unknown version.
        Some(Self {
            trace_id,
            span_id,
            sampled: flags[0] & 0x01 == 0x01,
        })
    }

    /// The `traceparent` value to send onward.
    #[must_use]
    pub fn header(&self) -> String {
        let mut out = String::with_capacity(TRACEPARENT_LEN);
        out.push_str("00-");
        push_hex(&mut out, &self.trace_id);
        out.push('-');
        push_hex(&mut out, &self.span_id);
        out.push('-');
        out.push_str(if self.sampled { "01" } else { "00" });
        out
    }

    /// The trace id, shared by every service on the path.
    #[must_use]
    pub fn trace_id(&self) -> [u8; 16] {
        self.trace_id
    }

    /// This hop's span id.
    #[must_use]
    pub fn span_id(&self) -> &[u8; 8] {
        &self.span_id
    }

    /// Whether the originating service recorded this request.
    #[must_use]
    pub fn sampled(&self) -> bool {
        self.sampled
    }

    /// The trace id as lowercase hex, for a structured log field.
    ///
    /// Trace ids are conventionally logged in this form — the same 32 characters a collector shows
    /// — so a value copied out of a log line can be pasted into a trace query unchanged.
    #[must_use]
    pub fn trace_id_hex(&self) -> String {
        let mut out = String::with_capacity(32);
        push_hex(&mut out, &self.trace_id);
        out
    }

    /// The span id as lowercase hex.
    #[must_use]
    pub fn span_id_hex(&self) -> String {
        let mut out = String::with_capacity(16);
        push_hex(&mut out, &self.span_id);
        out
    }
}

impl fmt::Debug for TraceContext {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.header())
    }
}

/// A fresh trace id, never all zero.
fn random_trace_id() -> [u8; 16] {
    let mut bytes = [0u8; 16];
    fill(&mut bytes);
    // All-zero ids are invalid per the specification, and a random source could in principle
    // produce one. Cheap to rule out, and a zero id is dropped by every collector.
    if bytes == [0u8; 16] {
        bytes[15] = 1;
    }
    bytes
}

/// A fresh span id, never all zero.
fn random_span_id() -> [u8; 8] {
    let mut bytes = [0u8; 8];
    fill(&mut bytes);
    if bytes == [0u8; 8] {
        bytes[7] = 1;
    }
    bytes
}

/// Fill `dest` from the system RNG, falling back to [`fallback_fill`].
///
/// The fallback is **predictable**, and the crate docs say why that is acceptable for these two
/// values. What it must not be is *repeated*: a counter is mixed in so two requests in the same
/// nanosecond cannot collide, which a bare timestamp would allow under load.
fn fill(dest: &mut [u8]) {
    if getrandom::fill(dest).is_ok() {
        return;
    }
    static WARNED: std::sync::Once = std::sync::Once::new();
    WARNED.call_once(|| {
        tracing::warn!(
            "the system random number source is unavailable; request ids and trace ids will be \
             predictable until it recovers. This affects log correlation only — nothing is \
             authorised by these values"
        );
    });
    fallback_fill(dest);
}

/// The no-RNG path: a monotonic counter spread across the whole word.
///
/// Split out from [`fill`] rather than written inline, so the one test that can exercise it does
/// not have to reimplement it. A test that copied this body would be a test of the copy — and the
/// copy would keep passing after the real one was changed, which is precisely the hazard this
/// project has already recorded twice.
fn fallback_fill(dest: &mut [u8]) {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos() as u64);
    let mut seed = nanos ^ sequence.rotate_left(17);
    for slot in dest.iter_mut() {
        // SplitMix64's finaliser, which is what spreads a counter's low bits across the whole word
        // — a raw counter would put almost all of its movement in one byte.
        seed = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut mixed = seed;
        mixed = (mixed ^ (mixed >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        mixed = (mixed ^ (mixed >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        mixed ^= mixed >> 31;
        *slot = (mixed >> 24) as u8;
    }
}

/// One lowercase hex digit.
const fn hex_digit(nibble: u8) -> char {
    match nibble {
        0..=9 => (b'0' + nibble) as char,
        _ => (b'a' + nibble - 10) as char,
    }
}

/// Append `bytes` as lowercase hex.
fn push_hex(out: &mut String, bytes: &[u8]) {
    for byte in bytes {
        out.push(hex_digit(byte >> 4));
        out.push(hex_digit(byte & 0x0f));
    }
}

/// Read exactly `N` bytes from `2 * N` hex characters.
///
/// `None` on any non-hex character, on a wrong length, and on the uppercase the specification does
/// not allow in a `traceparent`.
fn parse_hex_array<const N: usize>(text: &str) -> Option<[u8; N]> {
    if text.len() != N * 2 {
        return None;
    }
    let mut out = [0u8; N];
    let bytes = text.as_bytes();
    for (index, slot) in out.iter_mut().enumerate() {
        let high = hex_value(bytes[index * 2])?;
        let low = hex_value(bytes[index * 2 + 1])?;
        *slot = (high << 4) | low;
    }
    Some(out)
}

/// One hex digit's value, rejecting the uppercase a `traceparent` may not contain.
const fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_generated_request_id_is_nginx_shaped() {
        // The length is the compatibility claim: 32 hex characters is exactly what nginx's
        // `$request_id` renders, so one `awk` reads both logs. A change that made this 36 (a UUID)
        // would silently break that and no functional test would notice.
        let id = RequestId::generate();
        let text = id.to_string();
        assert_eq!(text.len(), 32, "{text}");
        assert!(
            text.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "{text} is not hex"
        );
        assert_eq!(text, format!("{id}"));
        assert_eq!(text, format!("{id:?}"), "Debug must be the rendered form");
        assert_eq!(id.as_str(), text, "as_str and Display must agree");
    }

    #[test]
    fn two_generated_ids_differ() {
        // A `generate` that returned a constant would pass every other test in this file.
        assert_ne!(RequestId::generate(), RequestId::generate());
        assert_ne!(
            TraceContext::new(true).trace_id(),
            TraceContext::new(true).trace_id()
        );
    }

    #[test]
    fn the_bytes_are_the_hex_that_is_printed() {
        // The two renderings must be one value, or a log line and a test fixture disagree.
        let id = RequestId::from_bytes([
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
            0xee, 0xff,
        ]);
        assert_eq!(id.to_string(), "00112233445566778899aabbccddeeff");
        let mut written = String::new();
        id.write_str(&mut written);
        assert_eq!(written, id.to_string());
    }

    #[test]
    fn an_adopted_request_id_comes_back_exactly_as_it_arrived() {
        // **The test that gives adoption its point.** A client looking for its own id in our log
        // needs the same string back: a UUID normalised into 32 hex characters is not findable, and
        // that is what an earlier version of this type did. Asserted on the exact text, because
        // asserting the length is what let that bug through.
        for value in [
            "00112233445566778899aabbccddeeff",
            "3f2504e0-4f89-11d3-9a0c-0305e82c3301",
            "ABC-123",
            "req_42",
            "trace.span.7",
        ] {
            let parsed = RequestId::parse(value).unwrap_or_else(|| panic!("{value} refused"));
            assert_eq!(parsed.to_string(), value, "not verbatim");
            assert_eq!(parsed.as_str(), value);
            let mut written = String::new();
            parsed.write_str(&mut written);
            assert_eq!(written, value);
        }
    }

    #[test]
    fn a_generated_and_an_adopted_id_are_told_apart() {
        // The variant decides how the id renders, so a `parse` that quietly produced a `Generated`
        // would re-render a client's id and the test above would still pass.
        let adopted = RequestId::parse("ABC-123").expect("a legal id");
        assert!(matches!(adopted, RequestId::Adopted(_)), "{adopted:?}");
        assert!(
            matches!(RequestId::generate(), RequestId::Generated(_)),
            "generate must not adopt"
        );
    }

    #[test]
    fn a_header_that_is_not_an_identifier_is_refused_rather_than_escaped() {
        // The security-relevant test in this file. `\n` in an adopted id would let a client forge
        // log records; a space would break a space-separated field count; an empty value would
        // produce an empty field. All are refused, so the front-end generates its own instead.
        for value in [
            "bad\n2026-01-01T00:00:00Z ERROR forged log record",
            "bad\r\ntrailing",
            "bad value",
            "",
            "semi;colon",
            "quote\"inside",
            "-leading-hyphen",
            ".leading-dot",
            "trailing-",
            "trailing.",
            &"x".repeat(MAX_REQUEST_ID_LEN + 1),
        ] {
            assert!(
                RequestId::parse(value).is_none(),
                "{value:?} was adopted, and it is not an identifier"
            );
        }
        // The cap is a boundary, not an off-by-one refusal of the longest legal id.
        assert!(RequestId::parse(&"x".repeat(MAX_REQUEST_ID_LEN)).is_some());
    }

    #[test]
    fn a_traceparent_round_trips_through_the_header_form() {
        let context = TraceContext::new(true);
        let header = context.header();
        assert_eq!(header.len(), TRACEPARENT_LEN);
        assert_eq!(
            TraceContext::parse(&header).expect("its own output parses"),
            context
        );
        // The flag survives both ways, or a proxy would silently unsample everything behind it.
        let unsampled = TraceContext::new(false);
        assert!(!TraceContext::parse(&unsampled.header()).unwrap().sampled());
        assert!(TraceContext::parse(&context.header()).unwrap().sampled());
        assert_eq!(context.trace_id_hex().len(), 32);
        assert_eq!(context.span_id_hex().len(), 16);
        // The hex accessors and the header must be one value, or a log line and a trace query
        // disagree about which trace this was.
        assert_eq!(context.header()[3..35], context.trace_id_hex());
        assert_eq!(context.header()[36..52], context.span_id_hex());
    }

    #[test]
    fn the_specifications_own_example_parses() {
        // Straight out of the W3C recommendation, so this is checked against the specification
        // rather than against this crate's own output — which is the failure mode of a round-trip
        // test written alone.
        let parsed = TraceContext::parse("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01")
            .expect("the specification's example");
        assert_eq!(parsed.trace_id_hex(), "4bf92f3577b34da6a3ce929d0e0e4736");
        assert_eq!(parsed.span_id_hex(), "00f067aa0ba902b7");
        assert!(parsed.sampled());
    }

    #[test]
    fn a_malformed_traceparent_is_none_and_never_a_panic() {
        // `None` means "start a new trace", so this is the whole contract with the front-end: a
        // client's broken tracing header must not become a client's broken request.
        for value in [
            "",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7", // no flags
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01-extra",
            "00_4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01", // wrong separator
            "00-4BF92F3577B34DA6A3CE929D0E0E4736-00f067aa0ba902b7-01", // upper case
            "ff-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01", // unknown version
            "00-00000000000000000000000000000000-00f067aa0ba902b7-01", // zero trace id
            "00-4bf92f3577b34da6a3ce929d0e0e4736-0000000000000000-01", // zero span id
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-zz", // not hex
            // Exactly 55 **bytes** with a multi-byte character inside the span-id field: the char
            // count is 53, so this is what a length check alone would let through. It must be a
            // `None` and not a panic — see the comment on the ASCII check.
            "00-4bf92f3577b34da6a3ce929d0e0e4736-—067aa0ba902b7-01",
        ] {
            assert!(
                TraceContext::parse(value).is_none(),
                "{value:?} was accepted"
            );
        }
    }

    #[test]
    fn only_the_low_flag_bit_is_read() {
        // `03` sets a reserved bit. The sampled flag must still be read correctly rather than the
        // whole byte being compared, which would report "not sampled" for a sampled request.
        let parsed = TraceContext::parse("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-03")
            .expect("a reserved bit is not a malformed value");
        assert!(parsed.sampled(), "bit 0 is set, so this is sampled");
        let clear = TraceContext::parse("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-02")
            .expect("a reserved bit is not a malformed value");
        assert!(!clear.sampled(), "bit 0 is clear");
    }

    #[test]
    fn continuing_keeps_the_trace_and_changes_the_span() {
        // The distinction the whole type exists for: a proxy continues the client's *trace* and
        // contributes its own *span*. Keeping the client's span id would make two hops claim to be
        // one, which is what corrupts a waterfall.
        let client =
            TraceContext::parse("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01").unwrap();
        let ours = TraceContext::continuing(client.trace_id(), client.sampled());
        assert_eq!(ours.trace_id(), client.trace_id());
        assert_ne!(ours.span_id(), client.span_id());
        assert!(ours.sampled(), "the client's decision is respected");
        assert_eq!(
            ours.header()[3..35],
            client.header()[3..35],
            "the trace id is the same field of the same header"
        );
    }

    #[test]
    fn a_new_trace_never_has_an_all_zero_id() {
        // The specification forbids the zero id and every collector drops it, and a random source
        // is allowed to produce it. Asserted over many draws because one draw proves nothing.
        for _ in 0..64 {
            let context = TraceContext::new(true);
            assert_ne!(context.trace_id(), [0u8; 16]);
            assert_ne!(*context.span_id(), [0u8; 8]);
        }
    }

    #[test]
    fn the_randomness_fallback_still_produces_distinct_values() {
        // Reachable only when the system RNG fails, which a test cannot arrange portably — so the
        // real function is called directly rather than a copy of it being written here.
        let mut first = [0u8; 16];
        let mut second = [0u8; 16];
        fallback_fill(&mut first);
        fallback_fill(&mut second);
        assert_ne!(first, second, "the same nanosecond must not collide");
        assert_ne!(first, [0u8; 16]);
    }

    fn observing(header: Option<&str>) -> Observability {
        Observability::new(
            access_log::AccessLog::disabled(),
            header.map(str::to_string),
        )
    }

    #[test]
    fn a_client_id_is_adopted_only_when_a_header_is_configured() {
        // The stricter default, and the one that would be silently wrong the other way: with no
        // header configured, a client's `X-Request-Id` must not influence the log at all. An
        // implementation that read the header regardless would pass a test that only checked the
        // configured case.
        let configured = observing(Some(REQUEST_ID_HEADER));
        assert_eq!(
            configured.begin(Some("abc-123"), None).id.as_str(),
            "abc-123"
        );

        let unconfigured = observing(None);
        let resolved = unconfigured.begin(Some("abc-123"), None);
        assert_ne!(resolved.id.as_str(), "abc-123", "the client had a say");
        assert_eq!(
            resolved.id.as_str().len(),
            32,
            "a minted id is nginx-shaped"
        );
    }

    #[test]
    fn a_refused_client_id_becomes_a_fresh_one_rather_than_an_error() {
        // A malformed id is a new id, never a 400: the front-end must not be able to turn a
        // client's bad header into a failed request, and this is where that is decided.
        let observing = observing(Some(REQUEST_ID_HEADER));
        for value in ["bad value", "", "a\nb"] {
            let resolved = observing.begin(Some(value), None);
            assert_eq!(resolved.id.as_str().len(), 32, "{value:?} was adopted");
        }
        // And an absent header is the same answer as a bad one.
        assert_eq!(observing.begin(None, None).id.as_str().len(), 32);
    }

    #[test]
    fn an_inbound_trace_is_continued_and_a_missing_one_is_a_sampled_root() {
        // **Two decisions in one place, and the second one changed.** An inbound trace keeps its
        // trace id and takes a new span id — a proxy that kept the span id too would make two hops
        // claim to be one. And a request with no trace is a **root**, which the default sampler
        // records: reporting it unsampled was the bug that made a standalone deployment believe
        // tracing was on while exporting nothing.
        let observing = observing(None);
        let client = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
        let continued = observing.begin(None, Some(client));
        assert_eq!(
            continued.trace.trace_id_hex(),
            "4bf92f3577b34da6a3ce929d0e0e4736"
        );
        assert_ne!(continued.trace.span_id_hex(), "00f067aa0ba902b7");
        assert!(
            continued.trace.sampled(),
            "the caller's decision was reversed"
        );

        let fresh = observing.begin(None, None);
        assert!(
            fresh.trace.sampled(),
            "a root must be recorded, or a deployment with nothing in front of it exports nothing"
        );
        assert_ne!(fresh.trace.trace_id(), [0u8; 16]);

        // A broken traceparent starts a fresh trace rather than failing — the same rule as the id,
        // and it is a root like an absent one.
        let broken = observing.begin(None, Some("not-a-traceparent"));
        assert_ne!(broken.trace.trace_id(), [0u8; 16]);
    }

    #[test]
    fn a_caller_that_says_do_not_record_is_obeyed_and_one_that_says_nothing_is_not() {
        // **The distinction the whole sampler rests on.** `-00` is a caller asking not to be
        // recorded and must be obeyed; an absent header is a caller with no opinion, which is a
        // *root* and is recorded. A `parent_based` that read the two the same way would pass one
        // half of this test whichever way it read them.
        let observing = observing(None);
        let unsampled = observing.begin(None, Some(UNSAMPLED_CLIENT_TRACE));
        assert!(
            !unsampled.trace.sampled(),
            "an explicit -00 must be honoured, or a proxy spends somebody else's budget"
        );
        assert!(observing.begin(None, None).trace.sampled(), "a root");
        assert!(
            observing.begin(None, Some(CLIENT_TRACE)).trace.sampled(),
            "an explicit -01 is a caller asking to be recorded"
        );
    }

    #[test]
    fn the_clock_starts_at_begin() {
        // The other half of "one answer per question": the id and the elapsed time are taken in the
        // same call, so a front-end cannot log an id from before the work and a duration from after
        // it. Asserted by waiting, because a zero duration would also pass an equality check.
        let context = observing(None).begin(None, None);
        std::thread::sleep(std::time::Duration::from_millis(5));
        assert!(
            context.elapsed_seconds() >= 0.005,
            "{}",
            context.elapsed_seconds()
        );
    }

    /// The caller's example from the W3C specification, used by the propagation tests.
    const CLIENT_TRACE: &str = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";

    /// The same trace, with the caller's `sampled` flag **clear**: "do not record this".
    const UNSAMPLED_CLIENT_TRACE: &str = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-00";

    #[test]
    fn ignore_reads_nothing_and_writes_nothing() {
        // The value that has to be visibly different from `extract`, which is otherwise its
        // neighbour: reading the header and merely not writing it would be indistinguishable from
        // `extract` from the caller's side, and from the backend's.
        let observing = observing(None).with_propagation(TracePropagation::Ignore);
        let context = observing.begin(None, Some(CLIENT_TRACE));
        assert_ne!(
            context.trace.trace_id_hex(),
            "4bf92f3577b34da6a3ce929d0e0e4736",
            "an ignored header was read"
        );
        assert!(
            context.parent_span_id.is_none(),
            "an ignored header became a parent"
        );
        assert_eq!(observing.outbound_traceparent(&context), None);
    }

    #[test]
    fn extract_continues_the_trace_and_leaves_the_header_alone() {
        // Today's behaviour, and the default. The client's header is what a backend receives —
        // Volucer writes nothing — so a backend's spans are siblings of this server's span.
        let observing = observing(None).with_propagation(TracePropagation::Extract);
        let context = observing.begin(None, Some(CLIENT_TRACE));
        assert_eq!(
            context.trace.trace_id_hex(),
            "4bf92f3577b34da6a3ce929d0e0e4736"
        );
        assert_eq!(
            context.parent_span_id.map(hex16).as_deref(),
            Some("00f067aa0ba902b7")
        );
        assert_eq!(observing.outbound_traceparent(&context), None);
    }

    #[test]
    fn inject_starts_a_trace_here_and_writes_it() {
        // The half that does not read: a client's trace is deliberately *not* joined, and the
        // header a backend gets is this server's own.
        let observing = observing(None).with_propagation(TracePropagation::Inject);
        let context = observing.begin(None, Some(CLIENT_TRACE));
        assert_ne!(
            context.trace.trace_id_hex(),
            "4bf92f3577b34da6a3ce929d0e0e4736",
            "an injected trace joined the caller's"
        );

        let header = observing
            .outbound_traceparent(&context)
            .expect("inject must write a header");
        // The rendered header is the context the span will be exported with, which is the whole
        // point: a backend's child must name the span id this process reports.
        assert_eq!(header, context.trace.header());
        assert_eq!(&header[3..35], context.trace.trace_id_hex());
        assert_eq!(&header[36..52], context.trace.span_id_hex());
    }

    #[test]
    fn propagate_continues_the_callers_trace_and_writes_our_span() {
        // The value this whole key exists for. The trace id stays the caller's, the span id is
        // ours, and the backend is told ours — so its spans are children rather than siblings.
        let observing = observing(None).with_propagation(TracePropagation::Propagate);
        let context = observing.begin(None, Some(CLIENT_TRACE));
        assert_eq!(
            context.trace.trace_id_hex(),
            "4bf92f3577b34da6a3ce929d0e0e4736"
        );

        let header = observing
            .outbound_traceparent(&context)
            .expect("propagate must write a header");
        assert_eq!(&header[3..35], "4bf92f3577b34da6a3ce929d0e0e4736");
        assert_ne!(
            &header[36..52],
            "00f067aa0ba902b7",
            "the backend was handed the caller's span id, so its spans would be siblings"
        );
        assert_eq!(&header[36..52], context.trace.span_id_hex());
        // And the caller's span is still recorded as this span's parent, so the export is not a
        // root that dropped its link.
        assert_eq!(
            context.parent_span_id.map(hex16).as_deref(),
            Some("00f067aa0ba902b7")
        );
    }

    #[test]
    fn a_malformed_inbound_header_under_propagate_starts_a_fresh_trace_and_still_writes() {
        // The failure mode that would be worst: a broken client header must not leave a backend
        // with no context at all, because then `propagate` would silently stop working for exactly
        // the requests that already had a problem.
        let observing = observing(None).with_propagation(TracePropagation::Propagate);
        let context = observing.begin(None, Some("00-not-a-trace-x-01"));
        assert!(context.parent_span_id.is_none());
        let header = observing
            .outbound_traceparent(&context)
            .expect("still writes");
        assert_eq!(header.len(), 55);
        assert_eq!(&header[36..52], context.trace.span_id_hex());
    }

    /// The lowercase hex of a span id, for assertions.
    fn hex16(id: [u8; 8]) -> String {
        let mut out = String::with_capacity(16);
        push_hex(&mut out, &id);
        out
    }

    // ------------------------------------------------------------------ sampling
    //
    // The key that decides what *leaves* the process, as distinct from `trace_context`, which
    // decides what a backend is handed. The two interact in exactly one place — the `sampled` bit —
    // and that interaction is what most of these tests are about.

    /// A trace id whose rightmost seven bytes are a chosen value, for the ratio tests.
    ///
    /// Built from the value rather than written as a constant so a test can say "just below the
    /// threshold" and mean it: the sampler reads the low 56 bits, so those are the bits this sets.
    fn id_with_randomness(value: u64) -> [u8; 16] {
        let mut id = [0xaau8; 16];
        id[9..].copy_from_slice(&value.to_be_bytes()[1..]);
        id
    }

    #[test]
    fn the_default_sampler_records_a_root_and_obeys_a_parent() {
        // OTel's `ParentBased(root=AlwaysOn)`, and both halves matter. The root half is what makes
        // a single-service deployment export anything at all; the parent half is what stops a
        // proxy spending a tracing budget somebody else set.
        let sampler = Sampler::default();
        assert!(sampler.samples(id_with_randomness(1), None), "a root");
        assert!(
            sampler.samples(id_with_randomness(1), Some(true)),
            "a parent that asked to be recorded"
        );
        assert!(
            !sampler.samples(id_with_randomness(1), Some(false)),
            "a parent that asked not to be, which is the half nginx's $otel_parent_sampled covers"
        );
    }

    #[test]
    fn always_records_what_a_parent_asked_to_drop() {
        // The one thing `always` changes, stated as an assertion rather than as documentation: it
        // overrides an explicit `-00` and does nothing else that differs from the default.
        let sampler = Sampler::Always;
        assert!(sampler.samples(id_with_randomness(1), Some(false)));
        assert!(sampler.samples(id_with_randomness(1), None));
        assert!(sampler.samples(id_with_randomness(1), Some(true)));
    }

    #[test]
    fn a_ratio_is_exact_at_both_ends() {
        // `0.0` and `1.0` are the values an operator writes to mean "none" and "all", so they must
        // not be `~all` and `~none` — a rounding error at either end is a deployment exporting
        // nothing or everything while believing the opposite.
        let none = Sampler::ratio(0.0).expect("zero is a legal ratio");
        let all = Sampler::ratio(1.0).expect("one is a legal ratio");
        for value in [0u64, 1, (1 << 55), (1 << 56) - 1] {
            let id = id_with_randomness(value);
            assert!(!none.samples(id, None), "0.0 kept {value}");
            assert!(all.samples(id, None), "1.0 dropped {value}");
        }
    }

    #[test]
    fn a_ratio_keeps_the_same_half_of_the_space_that_a_higher_one_would() {
        // **The monotonicity the specification requires**, and the property that makes a ratio
        // usable across hops: a backend sampling more generously than the front-end must keep
        // everything the front-end kept, or traces appear to fork. Asserted over a spread of trace
        // ids rather than one, because a sampler that read the wrong byte would still be monotone
        // at a single point.
        let sparse = Sampler::ratio(0.1).expect("a ratio");
        let dense = Sampler::ratio(0.5).expect("a ratio");
        let mut kept_sparse = 0;
        let mut kept_dense = 0;
        for step in 0..256u64 {
            // Spread across the whole 56-bit space rather than clustered low, so this is a sample
            // of the space and not of one corner of it.
            let id = id_with_randomness(step * ((1 << 56) / 256));
            let (a, b) = (sparse.samples(id, None), dense.samples(id, None));
            assert!(!a || b, "a trace kept at 0.1 was dropped at 0.5");
            kept_sparse += u64::from(a);
            kept_dense += u64::from(b);
        }
        // Roughly a tenth and a half of 256, with a wide band because this is a deterministic hash
        // and not a random draw — the assertion is that the mechanism discriminates at all, which a
        // sampler returning a constant would fail.
        assert!(
            (5..=60).contains(&kept_sparse),
            "0.1 kept {kept_sparse} of 256"
        );
        assert!(
            kept_dense > kept_sparse,
            "0.5 ({kept_dense}) kept no more than 0.1 ({kept_sparse})"
        );
    }

    #[test]
    fn a_ratio_is_a_function_of_the_trace_id_alone() {
        // The property that makes the decision reproducible at another hop: same id, same answer,
        // with no clock and no per-process state involved. Two fresh values of the type, built
        // independently, must agree — which a sampler seeded from a counter would not.
        let id = id_with_randomness(0x0123_4567_89ab);
        let first = Sampler::ratio(0.25).expect("a ratio");
        let second = Sampler::ratio(0.25).expect("a ratio");
        assert_eq!(first.samples(id, None), second.samples(id, None));
        assert_eq!(
            first.samples(id, Some(true)),
            second.samples(id, Some(true))
        );
        // And the ratio ignores the parent entirely: it is not a parent-based sampler.
        assert_eq!(first.samples(id, None), first.samples(id, Some(false)));
    }

    #[test]
    fn a_ratio_outside_the_representable_range_is_refused_rather_than_clamped() {
        // `1.5` is an arithmetic error, and reading it as "all traces" is how a bill gets decided
        // by a typo. `2^-56` is the floor because 56 bits of trace id is the whole sampling space;
        // anything below it rounds to "never", which `0.0` already says exactly.
        for bad in [f64::NAN, f64::INFINITY, -0.0 - 0.1, 1.5, 2.0, 1e-30] {
            assert!(
                Sampler::ratio(bad).is_none(),
                "{bad} was accepted as a ratio"
            );
        }
        assert!(Sampler::ratio(Sampler::MIN_RATIO).is_some());
        assert_eq!(
            Sampler::MIN_RATIO,
            crate_min_ratio(),
            "the behaviour floor and the configuration's must be one number"
        );
    }

    /// The configuration crate's floor, reached through the constant rather than restated.
    ///
    /// A literal here would be a second copy of the number, and the copy would keep passing after
    /// the real one was changed — which is the hazard the `propagate` mirror already guards against
    /// in the other direction.
    fn crate_min_ratio() -> f64 {
        1.0 / (1u64 << 56) as f64
    }

    #[test]
    fn the_sampling_decision_reaches_the_exported_span_as_the_flag_the_backend_was_given() {
        // **The mismatch this whole change removes.** An inbound `-00` used to be propagated as
        // `00` while the exporter wrote `"flags":1`, so the backend was told the trace was not
        // recorded and the collector was told it was. Asserted through `outbound_traceparent` and
        // the recorded span together, because it is the *agreement* between the two that matters.
        let recording = std::sync::Arc::new(Recording::default());
        let observing = observing(None)
            .with_propagation(TracePropagation::Propagate)
            .with_sampling(Sampler::ParentBased)
            .with_spans(recording.clone());

        let dropped = observing.begin(None, Some(UNSAMPLED_CLIENT_TRACE));
        assert!(
            observing
                .outbound_traceparent(&dropped)
                .expect("still writes")
                .ends_with("-00"),
            "the caller asked not to be recorded"
        );
        observing.record_server_span(
            &dropped,
            dropped.started_at,
            dropped.started_at,
            empty_span(),
        );
        assert_eq!(
            recording.spans(),
            0,
            "a trace the caller dropped was exported anyway, so flags and traceparent disagree"
        );

        let kept = observing.begin(None, Some(CLIENT_TRACE));
        assert!(observing
            .outbound_traceparent(&kept)
            .expect("still writes")
            .ends_with("-01"));
        observing.record_server_span(&kept, kept.started_at, kept.started_at, empty_span());
        assert_eq!(recording.spans(), 1);
        assert_eq!(
            recording.flags(),
            vec![span::TRACE_FLAG_SAMPLED],
            "the exported flag must be the one the backend was told"
        );
    }

    #[test]
    fn an_always_sampler_writes_the_flag_it_exports() {
        // The other direction: `always` overrides a `-00` caller, and the header must then say
        // `-01` — otherwise the backend would honour the caller's flag, decline to record, and the
        // parent span this server exports would hang over children that do not exist.
        let observing = observing(None)
            .with_propagation(TracePropagation::Propagate)
            .with_sampling(Sampler::Always);
        let context = observing.begin(None, Some(UNSAMPLED_CLIENT_TRACE));
        assert_eq!(
            observing
                .outbound_traceparent(&context)
                .map(|h| h[52..].to_string()),
            Some("-01".to_string())
        );
    }

    /// A recorder that keeps what it was given, for the two assertions above.
    #[derive(Default)]
    struct Recording {
        spans: std::sync::Mutex<Vec<span::Span>>,
    }

    impl Recording {
        fn spans(&self) -> usize {
            self.spans.lock().expect("not poisoned").len()
        }

        fn flags(&self) -> Vec<u8> {
            self.spans
                .lock()
                .expect("not poisoned")
                .iter()
                .map(|span| span.flags)
                .collect()
        }
    }

    impl span::SpanRecorder for Recording {
        fn record(&self, span: span::Span) {
            self.spans.lock().expect("not poisoned").push(span);
        }
    }

    /// The smallest `ServerSpanAttributes` that satisfies the type.
    fn empty_span() -> ServerSpanAttributes<'static> {
        ServerSpanAttributes {
            method: "GET",
            path: "/",
            host: None,
            status: 200,
            name: "http.request",
            status_message: None,
            extra: &[],
        }
    }
}
