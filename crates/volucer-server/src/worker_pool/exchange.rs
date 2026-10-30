//! One worker's channel and the thread that owns it, for the worker's whole life:
//! a request is posted to the thread and the answer comes back on a channel, so a
//! read with no portable timeout still has one. This is the fix for the
//! per-request thread cost; see [`ChannelExchange`].

use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::time::Duration;
use volucer_worker::codec::RequestStage;
use volucer_worker::codec::WorkerResponse;
use volucer_worker::{Ack, BoundedChannel, JobHeader, MessageType, WorkerError};

/// Both directions of one worker's pipes, buffered.
///
/// Buffered because every frame is written as a header and then a payload and read the same
/// way: over an unbuffered `ChildStdin`/`ChildStdout` each of those calls is its own syscall,
/// and each read its own wakeup of the exchange thread. See [`BoundedChannel`].
pub(super) type Channel = BoundedChannel;

/// What the exchange thread is asked to do, and what the caller is told.
///
/// `WorkerChannel::request` and `send_frame` answer one exchange each, so the
/// `request_id` that matches a response lives inside the channel and the caller
/// only has to say which call it made. Both answer `None` for a response, because
/// the only exchange that produces one is [`Exchange::Request`] — the handshake is
/// already done by the time this thread starts.
pub(super) enum Exchange {
    /// Send one request and collect its whole response.
    ///
    /// The head arrives **already encoded**. The pool's translation borrows the CGI environment —
    /// that is what `borrowed_request` is for — and a borrow cannot travel to this thread, because
    /// on a timeout the thread outlives the call that made it. So the pool serializes the head and
    /// sends the bytes, and the worker cannot tell which side of the boundary encoded them.
    Request { head: Vec<u8>, body: Vec<u8> },
    /// Send one job and collect the consumer's terminal answer.
    ///
    /// The head arrives **already built** rather than already encoded, and the asymmetry with
    /// [`Exchange::Request`] is the reason the two are separate variants instead of one with a flag:
    /// a request head borrows a CGI environment and therefore *must* cross this boundary as bytes,
    /// while a job head is built from a driver's `Reserved` and owns everything in it. Encoding it
    /// here costs nothing extra and keeps `volucer-worker` the only place that knows the job frame's
    /// shape.
    Job { head: JobHeader, body: Vec<u8> },
    /// Send one frame from an already-encoded payload.
    Send {
        message_type: MessageType,
        request_id: u32,
        payload: Vec<u8>,
        extra_flags: u16,
    },
    /// Send one request and hand back its head, relaying the body as it arrives.
    ///
    /// The counterpart of [`Exchange::Request`] and the reason it is a separate variant rather than
    /// a flag: this one **does not finish**. `Request` returns when the response is complete, and
    /// this returns when the *head* is complete — the channel stays out of the free list, and this
    /// thread stays inside `run`, until the last body frame has been forwarded.
    ///
    /// Progress travels on `events` rather than on this exchange's answer slot, because the caller
    /// stops waiting long before the work is done: [`round_trip`](ChannelExchange::round_trip) is
    /// one answer to one exchange, and this exchange has two answers and a body between them. See
    /// [`StreamEvent`] for the three things the caller is told and in what order.
    ///
    /// `relay` is the deployment's policy, consulted **after** the head arrives — the worker, not the
    /// server, decides whether to stream, so a response the policy refuses (an error page, an
    /// `X-Sendfile` instruction, a measured body) has to be collected and returned whole instead.
    ///
    /// `on_end` runs on this thread after the last frame is read and before the caller is told the
    /// exchange is over, which is the only point at which both "the slot is free again" and "the
    /// body channel is closed" are true. It is a `Box<dyn FnOnce>` rather than a channel because the
    /// pool's half of it is not `Send`-agnostic data: it releases a lease and may respawn a worker,
    /// which is work, not a message.
    Streamed {
        head: Vec<u8>,
        body: Vec<u8>,
        relay: volucer_fcgi::Relay,
        events: std::sync::mpsc::Sender<StreamEvent>,
        on_end: Box<dyn FnOnce(StreamDone) + Send>,
    },
}

/// What a streamed exchange tells its caller, in order.
///
/// Exactly one message, and then the sender is dropped — which is itself the "nothing more is
/// coming" signal. There is deliberately nothing after it on this channel: the *end* of a streamed
/// response is reported through `on_end`, because by then the caller's slot has to be released and
/// releasing it is work rather than a message.
pub(super) enum StreamEvent {
    /// A streamed response whose body is arriving.
    Head(crate::relay::StreamHead),
    /// The whole response instead of a stream, because the relay policy refused to relay it.
    ///
    /// Not a failure: an error page, a `X-Sendfile` instruction and a body the worker measured are
    /// all responses this server must answer with as it always did, and re-framing any of them as
    /// chunked would break the feature that depends on it.
    Buffered(volucer_worker::codec::WorkerResponse),
    /// Nothing was opened, and the request may be retried under the ordinary rule.
    Failed(StreamFailure),
}

/// A failure before a response existed.
///
/// `stage` is carried so the pool can apply the same replay rule it applies to an ordinary request:
/// a failure in the send phase cannot have run anything, and one in the read phase may have.
pub(super) struct StreamFailure {
    pub(super) stage: RequestStage,
    pub(super) message: String,
}

impl StreamFailure {
    fn from_error(err: &WorkerError) -> Self {
        StreamFailure {
            // A `request_streamed` failure is always in the read phase unless the head frame could
            // not be written, and the codec reports that by failing before any read happens. The
            // distinction is made by the staging callback, which has already run by now — so this
            // only has to be wrong-never rather than wrong-sometimes: `Reading` is the conservative
            // of the two, because it is the one that limits a replay to idempotent verbs.
            stage: RequestStage::Reading,
            message: err.to_string(),
        }
    }
}

/// How a streamed exchange ended.
pub(super) struct StreamDone {
    /// The worker's terminal frame, when there was one.
    ///
    /// `Some` is the only evidence that the worker finished the exchange and may serve again.
    pub(super) end: Option<volucer_worker::RequestEnd>,
    /// Whether the response was delivered **whole**, whether as a stream that reached `END` or as a
    /// buffered answer.
    ///
    /// Two facts rather than one, and the difference is load-bearing: a buffered response has no
    /// streamed state at all, so `end` is `None` for it — and reading that as "truncated" would
    /// replace the worker after *every* ordinary request, which turns a one-worker pool into a
    /// stream of respawns. `None` with `complete == true` means "a body that was measured, so there
    /// is no terminal frame to find".
    pub(super) complete: bool,
    /// Why the body did not reach its end, when it did not.
    pub(super) failure: Option<String>,
}
impl Exchange {
    /// Run this exchange on a live channel.
    ///
    /// `on_stage` is called as the request moves from writing to reading, so a
    /// failure can be attributed to the half it happened in. Only
    /// [`Exchange::Request`] and [`Exchange::Job`] have two phases; every other exchange reports
    /// `Reading` once, because a control or a frame write has nothing to read and its result
    /// is not retried.
    fn run(
        self,
        channel: &mut Channel,
        on_stage: &mut dyn FnMut(RequestStage),
    ) -> std::result::Result<Option<Completed>, WorkerError> {
        match self {
            Exchange::Request { head, body } => {
                // The stage is reported by the channel as it moves from writing to reading, which
                // is the only place that can know where a failure happened.
                let result = channel.request_encoded(head, &body, &mut *on_stage);
                result.map(|response| Some(Completed::Response(response)))
            }
            Exchange::Job { head, body } => {
                let result = channel.job(head, &body, &mut *on_stage);
                // The telemetry the consumer emitted while handling the job travels back with the
                // answer, drained from the channel's lifetime buffer. A request finds its records on
                // the response object; a job has no response object, so without this they would sit
                // in the channel until the consumer was recycled — which is exactly when a scrape can
                // no longer see them.
                result.map(|ack| Some(Completed::Job(ack, channel.take_metrics())))
            }
            Exchange::Send {
                message_type,
                request_id,
                payload,
                extra_flags,
            } => {
                on_stage(RequestStage::Reading);
                channel.send_frame(message_type, request_id, &payload, extra_flags)?;
                Ok(None)
            }
            Exchange::Streamed {
                head,
                body,
                relay,
                events,
                on_end,
            } => {
                // **Always `Ok(None)`, whatever happened.** `run`'s return value is queued on the
                // answer channel that `round_trip` reads, and this caller stops reading after one
                // message — so an `Err` here would sit on that channel until the *next* request's
                // `round_trip` consumed it and failed for a reason that had nothing to do with it.
                // Every outcome therefore leaves through `events` (for the opening) and `on_end`
                // (for the ending), which are this exchange's own channels and nobody else's.
                pump_streamed(channel, head, &body, relay, &events, on_end, &mut *on_stage);
                Ok(None)
            }
        }
    }
}

/// Send one request, hand its opening back, and then relay the body into the caller's channel.
///
/// # Why the pump runs here and not in `relay.rs`
///
/// `relay::spawn_reader` owns its own thread because the FastCGI client is a separate object on a
/// separate slot. A worker's pipe is already owned by exactly one thread — this one, for the worker's
/// whole life — and two readers on one pipe would interleave frames. So the *loop* lives here and the
/// *back-pressure policy* lives in `relay.rs`, which is also where the FastCGI path gets it. That
/// split is why a slow client stalls an application the same way whichever backend it is on.
///
/// # The five outcomes, and why each is told apart
///
/// - **Nothing opened.** `events` says why, and nothing has been promised to a client. This is the
///   same class of failure as an ordinary request's, so the pool applies the same retry rule.
/// - **A response the relay policy refuses.** Collected whole and handed over as `Buffered`, which
///   is what makes an error page, an `X-Sendfile` instruction and a measured body behave exactly as
///   they did before this existed.
/// - **Head, then a protocol error mid-body.** The head is already on the wire, so the only honest
///   signal left is a closed channel with `finished == false`, which the front-end renders as a
///   truncated response rather than a complete one.
/// - **Head, then the client went away.** `blocking_send` fails; the pump stops forwarding but keeps
///   reading to `LAST`/`END` — a pipe abandoned mid-response would leave the worker blocked writing
///   into a full one, and the slot would never be usable again.
/// - **Head, then the end.** The ordinary case: `END` arrived, so `finished` is true.
///
/// # Why this returns nothing
///
/// The caller stops listening after one message, so there is no return value anybody would read.
/// `on_end` is that caller's only channel for the ending, and it runs on this thread after the last
/// frame — which is the only moment at which both "the body channel is closed" and "the slot may be
/// reused" are true at once.
fn pump_streamed(
    channel: &mut Channel,
    head: Vec<u8>,
    body: &[u8],
    relay: volucer_fcgi::Relay,
    events: &std::sync::mpsc::Sender<StreamEvent>,
    on_end: Box<dyn FnOnce(StreamDone) + Send>,
    on_stage: &mut dyn FnMut(RequestStage),
) {
    let started = std::time::Instant::now();
    let verdict = crate::relay::Verdict::new();
    let (sender, receiver) = crate::relay::channel();
    // Whether the worker actually streamed. A buffered answer takes the other arm and has no terminal
    // frame — `stream_end()` is `None` for it — so this is what lets the ending tell "measured" from
    // "truncated" without guessing.
    let mut saw_stream = false;

    let outcome = match channel.request_streamed(head, body, &mut *on_stage) {
        Ok(volucer_worker::Delivery::Streamed(streamed)) => {
            saw_stream = true;
            // **The policy is consulted here, after the head, and that is the only place it can
            // be.** Whether a response may be relayed is a question about its status, its
            // `Content-Length` and whether it names a file — none of which exist until the head
            // has arrived. Asking the worker "will you stream?" first and the policy second would
            // mean the worker had already started emitting a body this server must not relay.
            let head_bytes = super::response::render_head(&streamed.head);
            let (_, parsed) = volucer_fcgi::parse_response_headers(&head_bytes);
            if !relay.admits(streamed.head.status, &parsed) {
                // Refused, so it is collected whole and returned as an ordinary response. The rest
                // of the body is already on the pipe and `next_chunk` is the only thing that can
                // read it, so this is a drain that happens to build the body — not a second
                // implementation of `request`.
                let mut collected = Vec::new();
                let drained = loop {
                    match channel.next_chunk() {
                        Ok(Some(chunk)) => collected.extend_from_slice(&chunk),
                        Ok(None) => break Ok(()),
                        Err(err) => break Err(err),
                    }
                };
                drop(sender);
                match drained {
                    Ok(()) => {
                        let mut response = volucer_worker::codec::WorkerResponse {
                            head: streamed.head,
                            body: collected,
                            ..Default::default()
                        };
                        if let Some(end) = channel.stream_end() {
                            response.end = end.clone();
                        }
                        response.logs = channel.logs().to_vec();
                        response.metrics = channel.metrics().to_vec();
                        let _ = events.send(StreamEvent::Buffered(response));
                        Ok(())
                    }
                    Err(err) => Err(err),
                }
            } else {
                let answered = crate::relay::StreamHead {
                    head: head_bytes,
                    head_duration: started.elapsed(),
                    body: receiver,
                    verdict: verdict.clone(),
                };
                if events.send(StreamEvent::Head(answered)).is_err() {
                    // Nobody will read this body, so pumping it is pointless — but the pipe still
                    // has to reach `END`, or the worker blocks writing into a full one and the slot
                    // is lost.
                    drain_stream(channel)
                } else {
                    pump_body(channel, &sender, &verdict)
                }
            }
        }
        // The worker measured its response, so there is nothing to relay. It is a complete response
        // and the caller answers with it exactly as `request` would have.
        Ok(volucer_worker::Delivery::Buffered(response)) => {
            drop(sender);
            let _ = events.send(StreamEvent::Buffered(response));
            Ok(())
        }
        Err(err) => {
            drop(sender);
            // Nothing was opened, so the caller is still waiting: told on `events` rather than
            // through `on_end`, which is only installed once a response exists.
            let _ = events.send(StreamEvent::Failed(StreamFailure::from_error(&err)));
            Err(err)
        }
    };

    // **Always run, and run last.** The pool's slot is only handed back here, so an exchange that
    // ended without this would leave a slot reserved for ever — a pool that silently shrinks by one
    // per streamed response, which is the failure mode this whole design has to avoid.
    //
    // The end frame is read *before* the callback so the caller can decide whether the worker is
    // reusable from evidence rather than from an absence. `complete` distinguishes the two kinds of
    // `None`: a buffered answer has no terminal frame because its body was measured, and a stream
    // that stopped has none because it was truncated. Reading the first as the second would replace
    // the worker after every ordinary request.
    let end = match outcome {
        Ok(()) => channel.stream_end().cloned(),
        Err(_) => None,
    };
    let body_ended = matches!(outcome, Ok(()));
    on_end(StreamDone {
        end: end.clone(),
        complete: body_ended && (end.is_some() || !saw_stream),
        failure: outcome.as_ref().err().map(|err| err.to_string()),
    });
}
/// Forward every remaining body chunk into `sender`, and publish the verdict.
fn pump_body(
    channel: &mut Channel,
    sender: &crate::relay::Sender,
    verdict: &crate::relay::Verdict,
) -> std::result::Result<(), WorkerError> {
    loop {
        match channel.next_chunk() {
            Ok(Some(chunk)) => {
                // **A client that went away is not a reason to stop reading.** `blocking_send`
                // returns `Err` once the receiver is dropped, and walking off here would leave the
                // worker blocked writing into a full pipe — the slot would never be reusable.
                if sender.blocking_send(bytes::Bytes::from(chunk)).is_err() {
                    return drain_stream(channel);
                }
            }
            Ok(None) => {
                let stderr = super::response::render_logs(channel.logs());
                verdict.publish(&stderr, true);
                return Ok(());
            }
            Err(err) => {
                // Left unfinished, deliberately: that is what makes the front-end report a
                // truncation rather than present a short body as the whole response.
                verdict.publish(&[], false);
                tracing::warn!(%err, "a streamed worker response failed part way through");
                return Err(err);
            }
        }
    }
}

/// Read a streamed body to its end without forwarding it.
///
/// The one caller is "the client went away", and the reason it exists is the pipe rather than the
/// client: a worker mid-body keeps writing, and a reader that walks off leaves it blocked forever.
fn drain_stream(channel: &mut Channel) -> std::result::Result<(), WorkerError> {
    loop {
        match channel.next_chunk() {
            Ok(Some(_)) => continue,
            Ok(None) => return Ok(()),
            Err(err) => return Err(err),
        }
    }
}

/// What one exchange produced.
///
/// An enum rather than a generic because the exchange thread is one thread per worker for that
/// worker's whole life, and it is created by `start` before anybody knows which exchange it will be
/// asked to run first. So the *thread* is not generic and this is: the answer travels as a value, and
/// the two callers destructure the variant they asked for. A caller that finds the other one has a bug
/// in this file, not a protocol condition, which is why both arms report it as a closed worker.
enum Completed {
    Response(WorkerResponse),
    /// The consumer's answer, and the telemetry it emitted while producing it.
    ///
    /// The records travel *with* the answer rather than being fetched later, because the buffer lives
    /// inside the exchange thread and the pool has no other way to reach it. A request path gets the
    /// same records on `WorkerResponse`; a job has no such object, so the exchange brings them back
    /// itself.
    Job(Ack, Vec<volucer_worker::MetricRecord>),
}

/// A worker's pipes, and the thread that owns them.
///
/// **This is the fix for the per-request thread cost.** A pipe read has no
/// portable timeout on any platform volucer runs on, so the deadline has to be
/// enforced by something outside the read: the worker process is killed, which is
/// what makes the read return. A thread is what can then be given up on — but it
/// does *not* have to be a new thread per request. One thread per worker, alive
/// for the worker's whole life, does the same job and is created once: a request
/// is posted to it on a channel, and the deadline is a `recv_timeout` on that
/// channel. Measured on the trivial handler this was worth about half the arm's
/// throughput (worker 20,422 rps vs `spawn_cgi` 29,950 at concurrency 32).
///
/// Two consequences, both deliberate:
///
/// - **A timeout still leaks this thread and its pipes.** The thread is blocked
///   in `read` and will not return, so nothing may be sent to it again — the
///   caller drops the whole exchange and kills the worker, and the process exit
///   is what finally ends the read. Sending a second request to a worker whose
///   first one timed out is the one thing that must not happen, and dropping the
///   exchange is what makes it impossible.
/// - **A write error means the thread has ended.** The sender is kept for the
///   whole call rather than re-sent, so nothing is delivered to a stale request's
///   answer.
#[derive(Debug)]
pub(super) struct ChannelExchange {
    work: Option<Sender<Exchange>>,
    /// The thread's answers, exactly one per exchange.
    answers: mpsc::Receiver<Answer>,
    /// Set once the thread has ended, so a write to a dead thread is reported as
    /// "closed" rather than as an unexplained send failure.
    ended: bool,
}

/// The thread's answers, exactly one per exchange.
///
/// A failure carries the [`RequestStage`] it happened in, because that is what says
/// whether the request could have been delivered.
type Answer = std::result::Result<Option<Completed>, (RequestStage, WorkerError)>;

impl ChannelExchange {
    /// Start the exchange thread that owns a freshly handshaken worker's pipes.
    ///
    /// Takes ownership of the channel and never gives it back: the thread ends
    /// when the channel is dropped, which is what `close` arranges.
    pub fn start(mut channel: Channel) -> Self {
        let (work_tx, work_rx) = mpsc::channel::<Exchange>();
        let (answer_tx, answers) = mpsc::channel();

        std::thread::spawn(move || {
            while let Ok(exchange) = work_rx.recv() {
                // The stage is tracked here, in the only place that can know it: a
                // failure is stamped with the half of the request it happened in.
                // The default is the send phase, so a failure before the callback
                // fires is still attributed to the write.
                let mut stage = RequestStage::Sending;
                let result = exchange.run(&mut channel, &mut |reported| stage = reported);
                // The receiver is gone only if the caller dropped this exchange,
                // which happens on a timeout. Then the answer is dropped with the
                // thread — the caller has already decided not to wait.
                if answer_tx
                    .send(result.map_err(|source| (stage, source)))
                    .is_err()
                {
                    break;
                }
            }
        });

        ChannelExchange {
            work: Some(work_tx),
            answers,
            ended: false,
        }
    }

    /// Run one request, giving up after `timeout`.
    ///
    /// One copy of the request body per request, and it is worth being honest about why:
    /// the work item is moved to the exchange thread, so it must own its bytes, and a
    /// channel of borrowed data would need a lifetime the standard library's channels
    /// cannot express. Removing that copy means `Request::body` becoming something cheap to
    /// share (`Arc<[u8]>`), which is a change to the FastCGI layer every backend shares —
    /// not worth it while the cost is one `memcpy` the previous design also paid. What is
    /// gone is the thread, which was the measured cost.
    ///
    /// The *head* is not copied by the channel either: it arrives already encoded, and the channel
    /// writes it straight out. What it costs is one `Vec` per request, and that is the price of
    /// crossing this thread boundary at all — the body copy below pays the same price for the same
    /// reason. Building the JSON is what the pool used to do, but *borrowing* the header for the
    /// worker is what it does now, and nine `String`s became one buffer.
    pub fn request(
        &mut self,
        head: Vec<u8>,
        body: &[u8],
        timeout: Duration,
    ) -> std::result::Result<WorkerResponse, (RequestStage, WorkerError)> {
        let exchange = Exchange::Request {
            head,
            body: body.to_vec(),
        };

        match self.round_trip(exchange, timeout) {
            Ok(Some(Completed::Response(response))) => Ok(response),
            // `request` always answers with a response, so a `None` — or a job's answer arriving for
            // a request — would be the thread misbehaving rather than a protocol condition.
            Ok(_) => Err((
                RequestStage::Reading,
                WorkerError::Closed("the exchange thread answered without a response".to_string()),
            )),
            Err(failure) => Err(failure),
        }
    }

    /// Hand one job to a consumer and wait for its terminal answer.
    ///
    /// The same shape as `request` — post an exchange, wait on a channel, let the timeout be the
    /// caller's — because there is one exchange thread per worker and it runs whichever exchange it
    /// is told to. That is the point of the design: a job is not a second thread, a second pipe or a
    /// second protocol; it is one more thing the same thread can be asked to do.
    pub fn job(
        &mut self,
        head: JobHeader,
        body: &[u8],
        timeout: Duration,
    ) -> std::result::Result<(Ack, Vec<volucer_worker::MetricRecord>), (RequestStage, WorkerError)>
    {
        let exchange = Exchange::Job {
            head,
            body: body.to_vec(),
        };

        match self.round_trip(exchange, timeout) {
            Ok(Some(Completed::Job(ack, metrics))) => Ok((ack, metrics)),
            Ok(_) => Err((
                RequestStage::Reading,
                WorkerError::Closed(
                    "the exchange thread answered a job without an outcome".to_string(),
                ),
            )),
            Err(failure) => Err(failure),
        }
    }

    /// Hand one request to a worker and come back as soon as its **head** is known.
    ///
    /// # Why this does not use `round_trip`
    ///
    /// `round_trip` posts an exchange and waits for one answer on `answers`, and it also marks the
    /// channel `ended` on a timeout. Neither is right here: this exchange produces *two* messages
    /// and a body between them, and the caller is expected to stop waiting after the first. So the
    /// exchange carries its own `events` sender, and the timeout below covers only the wait for the
    /// opening message — the deadline that covers the body is the slot's own, enforced by the
    /// front-end dropping the receiver and this thread draining to `END`.
    ///
    /// `Ok(Err(..))` is a response the relay policy refused, which the caller answers with as it
    /// always did. `Err` is a failure with nothing sent, and is `RequestStage`-tagged so the pool can
    /// decide whether the request may be replayed — the same rule as an ordinary request, which is
    /// only true because the head is the point this function returns at.
    pub fn request_streamed(
        &mut self,
        head: Vec<u8>,
        body: &[u8],
        timeout: Duration,
        relay: volucer_fcgi::Relay,
        on_end: Box<dyn FnOnce(StreamDone) + Send>,
    ) -> std::result::Result<
        std::result::Result<volucer_worker::codec::WorkerResponse, crate::pool::RelayOpening>,
        (RequestStage, WorkerError),
    > {
        if self.ended {
            return Err((
                RequestStage::Reading,
                WorkerError::Closed("the worker's exchange thread has ended".to_string()),
            ));
        }
        let Some(work) = self.work.as_ref() else {
            return Err((
                RequestStage::Sending,
                WorkerError::Closed("the worker's exchange thread has already ended".to_string()),
            ));
        };

        let (events_tx, events) = mpsc::channel();
        work.send(Exchange::Streamed {
            head,
            body: body.to_vec(),
            relay,
            events: events_tx,
            on_end,
        })
        .map_err(|_| {
            self.ended = true;
            (
                RequestStage::Sending,
                WorkerError::Closed("the worker's exchange thread has ended".to_string()),
            )
        })?;

        // The opening message, or the reason there is none. A timeout here is the *send* deadline
        // rather than a body deadline: nothing has been handed to a client yet, so the worker may
        // be killed and the request replayed under the ordinary rule.
        let opening = match events.recv_timeout(timeout) {
            Ok(opening) => opening,
            Err(RecvTimeoutError::Timeout) => {
                return Err((RequestStage::Reading, WorkerError::Timeout(timeout)))
            }
            Err(RecvTimeoutError::Disconnected) => {
                self.ended = true;
                return Err((
                    RequestStage::Reading,
                    WorkerError::Closed("the exchange thread ended without answering".to_string()),
                ));
            }
        };

        match opening {
            // The whole response instead of a stream, and `on_end` has already run by then: it is
            // the last thing `pump_streamed` does on every path, before it returns.
            StreamEvent::Buffered(response) => Ok(Ok(response)),
            StreamEvent::Head(stream_head) => {
                let relayed = stream_head.into_relayed();
                Ok(Err(crate::pool::RelayOpening::new(relayed)))
            }
            StreamEvent::Failed(failure) => {
                self.ended = true;
                Err((failure.stage, WorkerError::Closed(failure.message)))
            }
        }
    }

    /// Write one frame, without waiting for anything to come back.
    ///
    /// The payload arrives already encoded — `CONTROL drain` is a fixed byte
    /// string — so this needs no serializer and does not force one on this crate.
    /// A control frame is unsolicited, so its id is not a request id and is not
    /// correlated: the answer is drained because the thread produces one per
    /// exchange, and leaving it in the channel would make the *next* request read
    /// it and fail for a reason that has nothing to do with the request.
    pub fn send(
        &mut self,
        message_type: MessageType,
        request_id: u32,
        payload: &[u8],
        extra_flags: u16,
    ) -> std::result::Result<(), WorkerError> {
        let exchange = Exchange::Send {
            message_type,
            request_id,
            payload: payload.to_vec(),
            extra_flags,
        };

        match self
            .round_trip(exchange, Duration::from_secs(1))
            .map_err(|(_, source)| source)?
        {
            // `Send` writes a frame and reads nothing, so its answer carries no
            // completion and never can.
            None => Ok(()),
            Some(_) => Err(WorkerError::Closed(
                "the exchange thread answered a frame write with a completion".to_string(),
            )),
        }
    }
    /// End the exchange thread by closing the worker's stdin.
    ///
    /// Dropping the sender ends the thread's `recv`, and dropping the channel then
    /// closes the pipes. This is the shutdown signal that cannot fail, and it is
    /// deliberately not a method on `WorkerChannel`: the worker sees end-of-file on
    /// its next read and exits under its own control.
    pub fn close(self) {
        let ChannelExchange { work, answers, .. } = self;
        drop(work);
        drop(answers);
    }

    /// Post one exchange and wait for its answer.
    ///
    /// Exactly one exchange is in flight per worker at a time, so the single
    /// answer on `answers` is necessarily this one's and no request id needs to
    /// travel with it to say so. That is a property of the slot's lock, and it is
    /// why this is not a general-purpose multiplexer.
    fn round_trip(
        &mut self,
        exchange: Exchange,
        timeout: Duration,
    ) -> std::result::Result<Option<Completed>, (RequestStage, WorkerError)> {
        // Fail fast once the thread is known to be gone. Without this the send below
        // would succeed — the sender is still alive, only the answer end is closed —
        // and the call would block until the timeout for a worker that cannot answer.
        if self.ended {
            return Err((
                RequestStage::Reading,
                WorkerError::Closed("the worker's exchange thread has ended".to_string()),
            ));
        }

        let Some(work) = self.work.as_ref() else {
            return Err((
                RequestStage::Sending,
                WorkerError::Closed("the worker's exchange thread has already ended".to_string()),
            ));
        };

        work.send(exchange).map_err(|_| {
            self.ended = true;
            (
                RequestStage::Sending,
                WorkerError::Closed("the worker's exchange thread has ended".to_string()),
            )
        })?;

        match self.answers.recv_timeout(timeout) {
            Ok(result) => result,
            // Timed out. The thread is blocked in a read on this worker, so the
            // caller has to kill the worker: that is what makes the read return so
            // the thread can exit and drop the pipes.
            Err(RecvTimeoutError::Timeout) => {
                Err((RequestStage::Reading, WorkerError::Timeout(timeout)))
            }
            Err(RecvTimeoutError::Disconnected) => {
                self.ended = true;
                Err((
                    RequestStage::Reading,
                    WorkerError::Closed("the exchange thread ended without answering".to_string()),
                ))
            }
        }
    }
}
