//! The worker protocol over `AsyncRead`/`AsyncWrite`, so a slot's pipe can be awaited
//! instead of parked on.
//!
//! # Why this exists beside [`volucer_worker::codec`]
//!
//! `volucer-worker`'s channel is written against `std::io::Read`/`Write`, and that is the
//! right shape for its callers: a spawned worker's pipes are blocking, the FastCGI front-end
//! is blocking, and a read with no portable timeout needs a thread that can be given up on.
//! What that costs on the *HTTP* front-end is measured rather than argued — every request to
//! an `attach` or `worker` backend crossed to the exchange thread and back, which cost the
//! process ~13 µs of CPU per request in thread bookkeeping alone (see `docs/` and the branch
//! that measured it). An `upstream` backend pays none of it, because it drives a `tokio`
//! socket with `.await` on the runtime thread that owns the connection.
//!
//! An `attach` backend's transport is a unix socket. A unix socket is every bit as
//! awaitable as a TCP one. So the cost was never the transport — it was that the client
//! was written against blocking I/O and therefore needed a thread.
//!
//! # What is shared and what is not
//!
//! The framing decisions are **not** re-implemented here. A frame's bytes are produced by
//! [`volucer_worker::Header`] and the protocol's limits, flags and payload cap all come from
//! that crate; this module owns only *reading and writing those bytes asynchronously*. The
//! two spellings of the protocol must never disagree, so anything that decides what a frame
//! looks like is the worker crate's, and the one thing it cannot know — which I/O type the
//! bytes travel through — is this module's.
//!
//! # Which runtime polls this, and the one rule that follows
//!
//! [`WireChannel`] is a plain future over the connection; it does not own a runtime. Two
//! measured facts decide how it may be used, and both were checked on this codebase's target
//! rather than assumed:
//!
//! - `tokio::net::UnixStream::from_std` requires a runtime *context*, and it resolves the socket's
//!   driver — the waker behind every later readiness event — from that context **at the moment it
//!   is called**. It does not defer registration to some later poll.
//! - That runtime must therefore **outlive the connection**. A socket adopted under a runtime
//!   that is then shut down fails on the next poll from anywhere else with *"a Tokio 1.x context
//!   was found, but it is being shutdown"*, even though the adoption and every poll before it
//!   succeeded.
//!
//! So a connection must be adopted **and** driven by the long-lived runtime of the front-end that
//! serves it — never by a transient runtime created for one call. `AsyncRuntime::adopt` therefore
//! enters the pool's own runtime rather than a throwaway, and the code that used a throwaway "to
//! satisfy the context check" was wrong: at 12 workers it cost one worker replacement per slot on
//! the first request (12 in the same millisecond) and at 96 it cost 105, because a replacement that
//! fails leaves its slot with no exchange. Measured, pre-existing, and invisible in every table
//! here because the failures land inside the first second of load.
//!
//! A synchronous caller (the FastCGI front-end, a test) bridges to that same runtime through the
//! [`Handle`] the exchange stores, which keeps the waker's runtime alive for the connection's
//! whole life. See `worker_pool::AsyncExchange`.
//!
//! # Buffering, and why it is not optional
//!
//! Reading is through [`tokio::io::BufReader`] and writing through [`tokio::io::BufWriter`] at
//! the same 8 KiB the blocking [`volucer_worker::BoundedChannel`] uses. An unbuffered async
//! reader pays an epoll round trip *per frame* — a response is a head, a body and a `END` —
//! where the blocking one pays a syscall. Measured, unbuffered made the async arm 25% slower
//! than the hand-off it was meant to replace, which is a wrong answer produced entirely by a
//! buffering detail. See `BUFFER_CAPACITY`.

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader, BufWriter};

use volucer_worker::codec::{Delivery, RequestStage, StreamedHead, WorkerResponse};
use volucer_worker::{
    flags, Checksum, Codec, Control, Header, Json, LogRecord, MessageType, MetricRecord,
    RequestEnd, ResponseHead, Result, WorkerError, HEADER_LEN, MAX_BODY_CHUNK, MAX_PAYLOAD_LEN,
};

/// The buffer capacity, matching `volucer_worker::BoundedChannel`'s.
///
/// **Matched rather than chosen.** The two paths must make the same number of syscalls per
/// request for a measurement between them to mean anything, and the blocking channel's 8 KiB
/// is sized so that a whole small request, a response head and an `END` fit behind one read.
/// A different number here would be a difference in the transport masquerading as a
/// difference in the design.
const BUFFER_CAPACITY: usize = 8 * 1024;

/// How many body bytes are read into memory at once while forwarding a request body.
///
/// The same value the blocking channel frames with, for the same reason: a streamed upload
/// costs one chunk's buffer rather than the whole body.
const BODY_CHUNK: usize = MAX_BODY_CHUNK;

/// Both directions of one worker's connection, buffered, awaited.
///
/// `Debug` is written out rather than derived: a `BufReader` over a socket is, but deriving it
/// would be a statement about state this type does not want printed.
pub(super) struct WireChannel<R, W> {
    reader: BufReader<R>,
    writer: BufWriter<W>,
    next_request_id: u32,
    /// The checksum policy in force, adopted at handshake.
    ///
    /// Starts [`Checksum::Required`] and only becomes [`Checksum::Optional`] when the worker
    /// agrees to it, which is the safe direction: this end never sends frames a worker did not
    /// say it would accept unchecked.
    checksum: Checksum,
    /// Every log record this connection has produced, for the lifetime view.
    logs: Vec<LogRecord>,
    /// Every metric sample this connection has produced, for the lifetime view.
    metrics: Vec<MetricRecord>,
    /// Where this connection is in draining a streamed body, or `None` if it is not.
    ///
    /// Kept here rather than in the returned [`StreamedHead`] because the body is read through
    /// *this* connection: the head is a value the caller decides framing from, and
    /// [`Self::next_chunk`] pulls the body through the same connection that will serve the next
    /// request afterwards. No borrow, no extra generics, and only one thing can read the
    /// connection either way.
    streaming: Option<Streaming>,
    /// What this worker may call, or `None` to grant nothing.
    ///
    /// Installed at boot, before the handshake, for the reason the blocking channel's is: a worker
    /// may send a `CALL` at any point in its life, and the first thing it does after connecting is
    /// boot the application — which is exactly when a framework asks the server something.
    ///
    /// A `Box` rather than an `Arc` because the two differ in a way that matters here: an
    /// `Arc<dyn Fn + Send>` is **not** `Send` (an `Arc` needs `Send + Sync` for its contents), so
    /// holding one across an `await` would make every future in this module `!Send` and neither
    /// the pump task nor the front-end's spawned request could compile. A `Box` of the handler is.
    call_handler: Option<Box<volucer_worker::codec::CallHandler>>,
}

impl<R, W> std::fmt::Debug for WireChannel<R, W> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WireChannel")
            .field("next_request_id", &self.next_request_id)
            .field("checksum", &self.checksum)
            .finish_non_exhaustive()
    }
}

impl<R, W> WireChannel<R, W>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    /// Wrap a connected worker's two halves with the policy its handshake adopted.
    ///
    /// The handshake for an `attach` backend happens on the blocking path — the same boot code a
    /// spawned worker uses, which is what keeps one set of handshake rules — so the connection
    /// arrives here with its checksum policy already decided. Starting from
    /// [`Checksum::Required`] in that case would refuse frames the worker was told it could send
    /// unchecked, which is a failure on the first response rather than at boot.
    pub(super) fn new_with_policy(reader: R, writer: W, checksum: Checksum) -> Self {
        WireChannel {
            reader: BufReader::with_capacity(BUFFER_CAPACITY, reader),
            writer: BufWriter::with_capacity(BUFFER_CAPACITY, writer),
            next_request_id: volucer_worker::FIRST_REQUEST_ID,
            checksum,
            logs: Vec::new(),
            metrics: Vec::new(),
            streaming: None,
            call_handler: None,
        }
    }

    /// Send one request and read its whole response.
    ///
    /// The async counterpart of `WorkerChannel::request_encoded`, and the shape is the same
    /// because the protocol is: the head goes out already encoded (it borrows a CGI environment
    /// that cannot cross a thread boundary), the body follows in frames, and the answer is read
    /// until `END`.
    ///
    /// `on_stage` reports which half of the exchange is running, which is what decides whether a
    /// failure may be replayed: bytes that never left cannot have been executed.
    pub(super) async fn request_encoded(
        &mut self,
        head: Vec<u8>,
        body: &[u8],
        on_stage: &mut (dyn FnMut(RequestStage) + Send),
    ) -> Result<volucer_worker::codec::WorkerResponse> {
        let request_id = self.take_request_id();

        on_stage(RequestStage::Sending);
        self.send_frame(MessageType::Request, request_id, &head, 0)
            .await?;
        self.send_body(request_id, body).await?;

        on_stage(RequestStage::Reading);
        self.collect_response(request_id).await
    }

    /// Send a request whose body arrives in pieces, ending at a `LAST` frame.
    ///
    /// The counterpart of `WorkerChannel::request_encoded_streaming`, and the same reasoning: the
    /// head goes out with `body_len` absent and `STREAMED` set, `chunks` is pulled until it reports
    /// end-of-input, and `LAST` becomes the only end signal — so a 1 GB upload costs one frame's
    /// buffer rather than 1 GB.
    ///
    /// The worker must have advertised [`volucer_worker::HelloAck::stream_request_body`] before this
    /// is called; the pool gates that, because the capability lives on its side.
    ///
    /// # The deadline covers the worker, not the client
    ///
    /// `timeout` is applied **only after the terminating `LAST` frame has gone out**, which is the
    /// split `ChannelExchange::round_trip_streaming` documents and for the same reason: while the
    /// body is still arriving the worker has not been asked anything, so a deadline there would fail
    /// an upload that is merely slow. A 256 MiB `POST` over a phone link takes longer than the thirty
    /// seconds this pool allows a worker to think, and the client's own connection is the only thing
    /// that can bound that phase.
    ///
    /// Without this the awaited arm had no deadline at all: a worker that accepted the head and then
    /// said nothing held its slot for ever, and the pool shrank by one per silent worker while the
    /// threaded arm was answering `504` for the same worker.
    pub(super) async fn request_encoded_streaming(
        &mut self,
        head: Vec<u8>,
        chunks: &mut dyn FnMut() -> Result<Option<Vec<u8>>>,
        on_stage: &mut (dyn FnMut(RequestStage) + Send),
        timeout: std::time::Duration,
    ) -> Result<WorkerResponse> {
        let request_id = self.take_request_id();

        on_stage(RequestStage::Sending);
        self.send_frame(MessageType::Request, request_id, &head, flags::STREAMED)
            .await?;

        loop {
            let chunk = chunks()?;
            match chunk {
                // An empty piece carries no information, and a stream of them would spin here.
                Some(bytes) if bytes.is_empty() => continue,
                Some(bytes) => {
                    // Every piece of a chunk goes out **without** `LAST`, even the final piece of a
                    // chunk: `LAST` is the only end-of-body signal for a streamed request, and
                    // `chunks` returning `Some` says only that more arrived, never that there is no
                    // more. The terminal `None` arm below writes the one frame that carries it —
                    // unlike `send_body`, where the body is whole and its last piece genuinely is the
                    // end.
                    for piece in bytes.chunks(BODY_CHUNK) {
                        let header = Header::with_checksum(
                            MessageType::Body,
                            request_id,
                            piece,
                            self.checksum,
                        )?;
                        self.writer.write_all(&header.encode()).await?;
                        self.writer.write_all(piece).await?;
                        self.writer.flush().await?;
                    }
                }
                None => {
                    // The terminal frame, empty when nothing was sent. `LAST` is the only end signal
                    // for a streamed body, so it is written even for an empty one.
                    self.send_frame(MessageType::Body, request_id, &[], flags::LAST)
                        .await?;
                    break;
                }
            }
        }

        // Phase one ends here, so the worker's own deadline starts here. The head and every body
        // frame are on the wire, the worker owes an answer, and from this point the clock is the
        // pool's `request_timeout` rather than the client's upload speed.
        on_stage(RequestStage::Reading);
        match tokio::time::timeout(timeout, self.collect_response(request_id)).await {
            Ok(response) => response,
            Err(_) => Err(WorkerError::Timeout(timeout)),
        }
    }

    /// Write one frame whose payload is already encoded.
    ///
    /// The flush is per frame rather than per request, and that is the protocol's requirement
    /// rather than a choice: the channel is half-duplex, so a frame left in the buffer would be
    /// a deadlock rather than a latency win.
    pub(super) async fn send_frame(
        &mut self,
        message_type: MessageType,
        request_id: u32,
        payload: &[u8],
        extra_flags: u16,
    ) -> Result<()> {
        let mut header = Header::with_checksum(message_type, request_id, payload, self.checksum)?;
        header.flags |= extra_flags;
        self.write_frame(&header.encode(), payload).await?;
        self.writer.flush().await?;
        Ok(())
    }

    /// Write a header and its payload as **one** vectored send.
    ///
    /// The obvious spelling is two `write_all` calls, and it was: header, then payload. Under a
    /// `BufWriter` whose buffer is larger than both that reaches the socket as two `sendto` calls,
    /// because `tokio`'s `BufWriter` forwards a write larger than the space left rather than
    /// splitting it to fill the buffer. Measured on the `attach` relay at 32 workers, the second
    /// call cost a second syscall *and* a second wakeup of the blocked PHP reader:
    ///
    /// ```text
    /// 9.06%  UnixStream::poll_write -> sendto -> unix_stream_sendmsg -> sock_def_readable
    /// ```
    ///
    /// A half-duplex relay has no reason to hand the peer a partial message, so the two slices
    /// are written as one `IoSlice` pair. The buffer is bypassed rather than filled first: filling
    /// it would still need the flush below to reach the socket, which is the syscall this removes.
    async fn write_frame(&mut self, header: &[u8], payload: &[u8]) -> Result<()> {
        if payload.is_empty() {
            return self.writer.write_all(header).await.map_err(Into::into);
        }

        let slices = [io::IoSlice::new(header), io::IoSlice::new(payload)];
        let total = header.len() + payload.len();
        let written = self.writer.write_vectored(&slices).await?;

        // A short write means the socket's send buffer filled. Finish the remainder from whichever
        // slice it landed in. Dropping it would desynchronise the frame stream rather than fail, so
        // this is not an optimisation but the correctness half of using `write_vectored`.
        if written < total {
            if written < header.len() {
                self.writer.write_all(&header[written..]).await?;
                self.writer.write_all(payload).await?;
            } else {
                self.writer
                    .write_all(&payload[written - header.len()..])
                    .await?;
            }
        }

        Ok(())
    }

    /// Flush anything buffered, and shut the write half.
    ///
    /// The write half is what a worker's read loop watches: closing it is the shutdown signal
    /// that cannot fail, exactly as closing `stdin` is for a spawned worker.
    pub(super) async fn close(&mut self) {
        let _ = self.writer.flush().await;
        let _ = self.writer.shutdown().await;
    }

    /// Allocate a request id, reserving 0 for unsolicited frames.
    fn take_request_id(&mut self) -> u32 {
        let request_id = self.next_request_id;
        self.next_request_id = self.next_request_id.wrapping_add(1);
        if self.next_request_id == volucer_worker::UNSOLICITED {
            self.next_request_id = volucer_worker::FIRST_REQUEST_ID;
        }
        request_id
    }

    /// Ask the worker to drain and then stop, over the connection it is reading.
    ///
    /// The frame is **unsolicited** — no request id — and nothing reads an answer, because the
    /// next thing that happens is [`Self::close`]: the worker's read loop sees end-of-file and
    /// exits on its own. That is the same shutdown contract a spawned worker has, with the
    /// socket close in place of closing `stdin`.
    pub(super) async fn send_drain(&mut self) -> Result<()> {
        let payload = Json::encode(&Control::Drain)?;
        self.send_frame(
            MessageType::Control,
            volucer_worker::UNSOLICITED,
            &payload,
            0,
        )
        .await
    }

    /// Send the body as `Body` frames, flagging the last one.
    async fn send_body(&mut self, request_id: u32, body: &[u8]) -> Result<()> {
        if body.is_empty() {
            return Ok(());
        }

        let checksum = self.checksum;
        let mut chunks = body.chunks(BODY_CHUNK).peekable();
        while let Some(chunk) = chunks.next() {
            let last = chunks.peek().is_none();
            let mut header = Header::with_checksum(MessageType::Body, request_id, chunk, checksum)?;
            if last {
                header.flags |= flags::LAST;
            }
            self.writer.write_all(&header.encode()).await?;
            self.writer.write_all(chunk).await?;
            self.writer.flush().await?;
        }
        Ok(())
    }

    /// Read exactly one frame, or `None` at a clean end of stream.
    ///
    /// The payload is bounded and checksum-verified here rather than by a helper in the worker
    /// crate, because doing so needs *no* state beyond the policy — and routing it through the
    /// worker crate would mean that crate owning an `AsyncRead` bound it has no other reason to
    /// have. The rules themselves are `Header`'s: the cap is `MAX_PAYLOAD_LEN`, the flag that
    /// turns verification off is `declares_no_checksum`, and the hash is `Header::decode`'s own
    /// checksum field.
    async fn read_frame(&mut self) -> Result<Option<(Header, Vec<u8>)>> {
        let mut bytes = [0u8; HEADER_LEN];
        // A clean end of stream *before the first byte* is the only `None`; a stream that stops
        // part-way through a header is a truncated frame, which must not read as a clean close.
        match read_exact_or_eof(&mut self.reader, &mut bytes).await? {
            0 => return Ok(None),
            HEADER_LEN => {}
            _ => return Err(WorkerError::TruncatedHeader),
        }
        let header = Header::decode(&bytes)?;

        let policy = self.checksum;
        if let Some(error) = header.refuse_unchecked(policy) {
            return Err(error);
        }

        let len = header.payload_len as usize;
        if len > MAX_PAYLOAD_LEN {
            return Err(WorkerError::PayloadTooLarge {
                size: len,
                max: MAX_PAYLOAD_LEN,
            });
        }

        let mut payload = vec![0u8; len];
        if let Err(source) = self.reader.read_exact(&mut payload).await {
            if source.kind() == io::ErrorKind::UnexpectedEof {
                return Err(WorkerError::TruncatedFrame {
                    offset: 0,
                    needed: len,
                });
            }
            return Err(WorkerError::Io(source));
        }

        if !header.declares_no_checksum() {
            let computed = volucer_worker::crc32c(&payload);
            if computed != header.checksum {
                return Err(WorkerError::ChecksumMismatch {
                    request_id: header.request_id,
                    expected: header.checksum,
                    computed,
                });
            }
        }

        Ok(Some((header, payload)))
    }

    /// Telemetry the worker has emitted while this connection has been open.
    ///
    /// **Kept rather than dropped**, which is the one place this module deliberately differs from
    /// the blocking channel's shape. There, `absorb_log` and `absorb_metric` append to buffers the
    /// channel owns for its lifetime, and a request's own records are sliced out of them at
    /// `END`. Here a request's records are attached to its `WorkerResponse` directly — that is
    /// what the pool reads — while records that arrive *outside* a request (a scrape's worth of
    /// metrics, a log line between requests) accumulate on the connection. Without this they
    /// would be silently discarded, and a deployment's Prometheus scrape would read zero from
    /// this transport while reading correctly from every other.
    pub(super) fn take_telemetry(&mut self) -> (Vec<LogRecord>, Vec<MetricRecord>) {
        (
            std::mem::take(&mut self.logs),
            std::mem::take(&mut self.metrics),
        )
    }

    /// The log lines this connection has produced, for `STDERR` in a streamed verdict.
    pub(super) fn telemetry_logs(&self) -> &[LogRecord] {
        &self.logs
    }

    /// Read a response to its `END`.
    ///
    /// A buffered response and a streamed one both arrive here, and a **streamed head is
    /// accepted** rather than refused. That is deliberate and mirrors the blocking loop: a
    /// worker does not know which kind of caller it is talking to, so it may legitimately answer
    /// a buffered request with `STREAMED` set, and refusing it would let one deployment's
    /// front-end decide whether an application's response is legal. What changes is the end
    /// signal only: `body_len` is not authoritative, so the length check is skipped.
    async fn collect_response(&mut self, request_id: u32) -> Result<WorkerResponse> {
        let mut response = WorkerResponse::default();
        let mut have_head = false;
        let mut streamed = false;
        let mut body = Vec::new();
        let mut expected_body = 0u64;
        let mut seen_last_body = false;
        // Where this request's telemetry starts, so "what this request produced" is separated
        // from "what this connection has ever produced" — the same split the blocking loop makes.
        let log_mark = self.logs.len();
        let metric_mark = self.metrics.len();

        loop {
            let Some((header, payload)) = self.read_frame().await? else {
                return Err(WorkerError::Closed(format!(
                    "the worker closed its output while answering request {request_id}"
                )));
            };

            match header.message_type {
                MessageType::Response => {
                    if header.request_id != request_id {
                        return Err(WorkerError::WrongRequestId {
                            expected: request_id,
                            actual: header.request_id,
                        });
                    }
                    if have_head {
                        return Err(WorkerError::Remote {
                            message: format!("request {request_id} was answered twice"),
                        });
                    }
                    let head: ResponseHead = Json::decode(&payload)?;
                    streamed = header.flags & flags::STREAMED != 0;
                    if !streamed {
                        let declared = usize::try_from(head.body_len).unwrap_or(usize::MAX);
                        // Capped, so a worker that declares a length it never sends cannot make
                        // this commit the memory for it. The cap is the worker crate's, not one
                        // this module chose.
                        if declared <= volucer_worker::codec::BODY_RESERVE_LIMIT {
                            body.reserve(declared);
                        }
                        expected_body = head.body_len;
                    }
                    response.head = head;
                    have_head = true;
                }
                MessageType::Body => {
                    if header.request_id != request_id {
                        return Err(WorkerError::WrongRequestId {
                            expected: request_id,
                            actual: header.request_id,
                        });
                    }
                    if !have_head {
                        return Err(WorkerError::Remote {
                            message: format!("request {request_id} sent a body before its head"),
                        });
                    }
                    body.extend_from_slice(&payload);
                    if header.flags & flags::LAST != 0 {
                        seen_last_body = true;
                    }
                }
                MessageType::End => {
                    if header.request_id != request_id {
                        return Err(WorkerError::WrongRequestId {
                            expected: request_id,
                            actual: header.request_id,
                        });
                    }
                    response.end = Json::decode(&payload)?;
                    break;
                }
                MessageType::Log => self.absorb_log(&payload),
                MessageType::Metric => self.absorb_metric(&payload),
                MessageType::Event => {
                    tracing::trace!(request_id = header.request_id, "worker sent an event");
                }
                MessageType::Call => self.answer_call(&header, &payload).await?,
                MessageType::Error => {
                    let report: volucer_worker::ErrorReport = Json::decode(&payload)?;
                    if report.recoverable {
                        tracing::warn!(%report.message, "worker reported a recoverable error");
                    } else {
                        return Err(WorkerError::Remote {
                            message: report.message,
                        });
                    }
                }
                other => {
                    return Err(WorkerError::NotA {
                        expected: "a response frame",
                        actual: other,
                    })
                }
            }
        }

        if !streamed {
            if body.len() as u64 != expected_body {
                return Err(WorkerError::Remote {
                    message: format!(
                        "request {request_id} declared {expected_body} body bytes but sent {}",
                        body.len()
                    ),
                });
            }
            if expected_body > 0 && !seen_last_body {
                return Err(WorkerError::Remote {
                    message: format!("request {request_id} did not flag its last body frame"),
                });
            }
        }

        response.body = body;
        // Cloned, not moved: the connection's buffers are the lifetime view, so a caller that
        // wants everything the worker ever said keeps getting it while a caller that wants this
        // request's records finds them here. The same rule the blocking loop follows.
        response.logs = self.logs[log_mark..].to_vec();
        response.metrics = self.metrics[metric_mark..].to_vec();
        Ok(response)
    }

    /// Read a response only as far as its head, leaving the body on the connection.
    ///
    /// The counterpart of `WorkerChannel::request_streamed`. Two answers are possible and both
    /// are the worker's to choose: a head with `STREAMED` set, in which case the body is still
    /// arriving and [`Self::next_chunk`] is how it is read; or a head with a declared length, in
    /// which case the body is measured and is read whole before this returns. A caller that
    /// wanted a stream and got a measurement gets exactly what the blocking path gives it.
    ///
    /// Why this is two phases rather than one: a streamed response has to let the front-end
    /// choose a status, a content type and a framing **before** the first body byte reaches the
    /// client, which is impossible if the head is only known once the body is complete.
    pub(super) async fn request_streamed(
        &mut self,
        head: Vec<u8>,
        body: &[u8],
        on_stage: &mut (dyn FnMut(RequestStage) + Send),
    ) -> Result<Delivery> {
        let request_id = self.take_request_id();

        on_stage(RequestStage::Sending);
        self.send_frame(MessageType::Request, request_id, &head, 0)
            .await?;
        self.send_body(request_id, body).await?;

        on_stage(RequestStage::Reading);

        // Read up to and including the head, then stop. Telemetry and calls before it are
        // absorbed in place, so a worker that logs on the way to its first byte is unaffected.
        loop {
            let Some((header, payload)) = self.read_frame().await? else {
                return Err(WorkerError::Closed(format!(
                    "the worker closed its output while answering request {request_id}"
                )));
            };

            match header.message_type {
                MessageType::Response => {
                    if header.request_id != request_id {
                        return Err(WorkerError::WrongRequestId {
                            expected: request_id,
                            actual: header.request_id,
                        });
                    }
                    let head: ResponseHead = Json::decode(&payload)?;
                    if header.flags & flags::STREAMED != 0 {
                        if head.body_len != 0 {
                            // Both spellings at once is a worker bug that must not be resolved by
                            // preferring one: the sender and the reader would disagree about the
                            // body's end signal, and guessing produces either a truncated body or
                            // a frame read as payload.
                            return Err(WorkerError::Remote {
                                message: format!(
                                    "request {request_id} set STREAMED and declared {} body \
                                     bytes; a streamed response declares no length",
                                    head.body_len
                                ),
                            });
                        }
                        self.streaming = Some(Streaming {
                            request_id,
                            seen_last: false,
                            ended: None,
                        });
                        return Ok(Delivery::Streamed(StreamedHead { head, request_id }));
                    }
                    let mut response = self.collect_body(request_id, head, false).await?;
                    response.logs = Vec::new();
                    return Ok(Delivery::Buffered(response));
                }
                MessageType::Body => return Err(body_before_head(request_id)),
                MessageType::End => {
                    let end: RequestEnd = Json::decode(&payload)?;
                    return Err(WorkerError::Remote {
                        message: format!(
                            "request {request_id} ended without a response head (app status {})",
                            end.app_status
                        ),
                    });
                }
                MessageType::Log => self.absorb_log(&payload),
                MessageType::Metric => self.absorb_metric(&payload),
                MessageType::Event => {
                    tracing::trace!(request_id = header.request_id, "worker sent an event");
                }
                MessageType::Call => self.answer_call(&header, &payload).await?,
                MessageType::Error => {
                    let report: volucer_worker::ErrorReport = Json::decode(&payload)?;
                    if report.recoverable {
                        tracing::warn!(%report.message, "worker reported a recoverable error");
                    } else {
                        return Err(WorkerError::Remote {
                            message: report.message,
                        });
                    }
                }
                other => {
                    return Err(WorkerError::NotA {
                        expected: "a response frame",
                        actual: other,
                    })
                }
            }
        }
    }

    /// The terminal frame of a drained stream, once it has been read.
    pub(super) fn stream_end(&self) -> Option<&RequestEnd> {
        self.streaming.as_ref()?.ended.as_ref()
    }

    /// Send one job to a consumer and read its terminal `ACK`.
    ///
    /// A separate read loop from a response's, for the reason the blocking codec gives: the two
    /// accept different frames, and refusing a frame is the point of having a loop at all. A
    /// `RESPONSE` arriving while a job is in flight means the worker confused two exchanges, which
    /// is a fatal protocol error rather than something to guess about.
    pub(super) async fn job(
        &mut self,
        mut head: volucer_worker::JobHeader,
        body: &[u8],
        on_stage: &mut (dyn FnMut(RequestStage) + Send),
    ) -> Result<volucer_worker::Ack> {
        let request_id = self.take_request_id();
        // Stamped from the bytes about to be framed, so the declaration and the frames cannot
        // disagree — the same rule the request path follows, and for the same reason: a caller that
        // could set this could set it wrong.
        head.body_len = body.len() as u64;
        let payload = Json::encode(&head)?;

        on_stage(RequestStage::Sending);
        self.send_frame(MessageType::Job, request_id, &payload, 0)
            .await?;
        self.send_body(request_id, body).await?;

        on_stage(RequestStage::Reading);

        loop {
            let Some((header, payload)) = self.read_frame().await? else {
                return Err(WorkerError::Closed(format!(
                    "the worker closed its output while answering job {request_id}"
                )));
            };

            match header.message_type {
                MessageType::Ack => {
                    if header.request_id != request_id {
                        return Err(WorkerError::WrongRequestId {
                            expected: request_id,
                            actual: header.request_id,
                        });
                    }
                    return Json::decode(&payload);
                }
                MessageType::Log => self.absorb_log(&payload),
                MessageType::Metric => self.absorb_metric(&payload),
                MessageType::Event => {
                    tracing::trace!(request_id = header.request_id, "worker sent an event");
                }
                MessageType::Call => self.answer_call(&header, &payload).await?,
                MessageType::Error => {
                    let report: volucer_worker::ErrorReport = Json::decode(&payload)?;
                    if report.recoverable {
                        tracing::warn!(%report.message, "worker reported a recoverable error");
                    } else {
                        return Err(WorkerError::Remote {
                            message: report.message,
                        });
                    }
                }
                other => {
                    return Err(WorkerError::NotA {
                        expected: "a job acknowledgement",
                        actual: other,
                    })
                }
            }
        }
    }

    /// Ask an idle worker for its health, and wait for its `PONG`.
    ///
    /// The async twin of [`volucer_worker::WireChannel::ping`], and the reason it is written
    /// separately rather than shared: the blocking one cannot describe this one's `await`, and the
    /// blocking one must not be used on a connection a runtime owns.
    ///
    /// # Why it reads frames instead of using `expect_any`
    ///
    /// `volucer_worker`'s `expect_any` absorbs `LOG` and `METRIC` and returns anything else, which
    /// is right for a `CONTROL` acknowledgement — a control frame is sent *about* the worker and can
    /// carry its own telemetry. A ping is sent into a quiet window on the pool's behalf, so a frame
    /// arriving here is much more likely to be something a request is waiting for than something
    /// this exchange should consume. `EVENT` is traced and dropped by every other read loop in this
    /// module, so it is dropped here too; **anything that is not a `PONG`, telemetry or a call is
    /// left as an error rather than swallowed**, because the alternative is a keepalive that quietly
    /// eats a response.
    ///
    /// # The frame is unsolicited
    ///
    /// `PING` carries no request id — it belongs to no exchange — so this can never be confused with
    /// a response, and the protocol's correlation rules are untouched.
    pub(super) async fn ping(&mut self) -> Result<()> {
        self.send_frame(MessageType::Ping, volucer_worker::UNSOLICITED, &[], 0)
            .await?;

        loop {
            let Some((header, payload)) = self.read_frame().await? else {
                return Err(WorkerError::Closed(
                    "the worker closed its output while answering a ping".to_string(),
                ));
            };

            match header.message_type {
                MessageType::Pong => return Ok(()),
                MessageType::Log => self.absorb_log(&payload),
                MessageType::Metric => self.absorb_metric(&payload),
                MessageType::Event => {
                    tracing::trace!("worker sent an event while answering a ping");
                }
                // **Not absorbed, and deliberately.** A call is only answerable while a request is in
                // flight — the protocol says so, and a worker is required to refuse one it makes at
                // any other time. This is the one place where answering it would be worse than
                // failing: the pool would then be running a call handler from a keepalive, for a
                // request that does not exist.
                MessageType::Call => {
                    return Err(WorkerError::NotA {
                        expected: MessageType::Pong.name(),
                        actual: MessageType::Call,
                    });
                }
                other => {
                    return Err(WorkerError::NotA {
                        expected: MessageType::Pong.name(),
                        actual: other,
                    })
                }
            }
        }
    }

    /// The next non-empty body chunk, or `None` when the streamed body is over.
    ///
    /// `None` means `LAST` was seen **and** the terminal `END` was read, so it is the only signal
    /// a caller needs. An empty chunk is never returned: a `BODY` frame with no payload is legal
    /// and carries only `LAST`, and forwarding a zero-length write per chunk is a framing error
    /// to some clients.
    pub(super) async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>> {
        let Some(request_id) = self.streaming.as_ref().map(|s| s.request_id) else {
            return Err(WorkerError::Remote {
                message: "next_chunk was called with no streamed response in progress".to_string(),
            });
        };

        loop {
            if self.streaming.as_ref().is_some_and(Streaming::is_finished) {
                return Ok(None);
            }

            let Some((header, payload)) = self.read_frame().await? else {
                return Err(WorkerError::Closed(format!(
                    "the worker closed its output while streaming request {request_id}"
                )));
            };

            match header.message_type {
                MessageType::Body => {
                    if header.request_id != request_id {
                        return Err(WorkerError::WrongRequestId {
                            expected: request_id,
                            actual: header.request_id,
                        });
                    }
                    let last = header.flags & flags::LAST != 0;
                    if let Some(state) = self.streaming.as_mut() {
                        state.seen_last |= last;
                    }
                    if !payload.is_empty() {
                        return Ok(Some(payload));
                    }
                }
                // A second head mid-body means the worker began answering a different request on
                // this stream, which is a protocol error rather than something to tolerate.
                MessageType::Response => {
                    return Err(WorkerError::Remote {
                        message: format!("request {request_id} was answered twice"),
                    })
                }
                MessageType::End => {
                    if header.request_id != request_id {
                        return Err(WorkerError::WrongRequestId {
                            expected: request_id,
                            actual: header.request_id,
                        });
                    }
                    let seen_last = self.streaming.as_ref().is_some_and(|s| s.seen_last);
                    if !seen_last {
                        // `END` without `LAST` is a worker that did not flag the end of a body it
                        // was streaming. Reported rather than accepted, because accepting it would
                        // make a truncated body look complete — the one thing streaming must never
                        // do.
                        return Err(WorkerError::Remote {
                            message: format!(
                                "request {request_id} ended a streamed body without a LAST frame"
                            ),
                        });
                    }
                    let end: RequestEnd = Json::decode(&payload)?;
                    if let Some(state) = self.streaming.as_mut() {
                        state.ended = Some(end);
                    }
                    return Ok(None);
                }
                MessageType::Log => self.absorb_log(&payload),
                MessageType::Metric => self.absorb_metric(&payload),
                MessageType::Event => {
                    tracing::trace!(request_id = header.request_id, "worker sent an event");
                }
                MessageType::Call => self.answer_call(&header, &payload).await?,
                MessageType::Error => {
                    let report: volucer_worker::ErrorReport = Json::decode(&payload)?;
                    if report.recoverable {
                        tracing::warn!(%report.message, "worker reported a recoverable error");
                    } else {
                        return Err(WorkerError::Remote {
                            message: report.message,
                        });
                    }
                }
                other => {
                    return Err(WorkerError::NotA {
                        expected: "a response body frame",
                        actual: other,
                    })
                }
            }
        }
    }

    /// Read a response body to its `END`, given a head already decoded.
    ///
    /// The shared buffered tail for the streamed entry point, including the two independent
    /// truncation checks that are the reason a short body is *detected* rather than presented as
    /// complete. `streamed` says the worker sent `STREAMED`, in which case there is no declared
    /// length to check against and the body's size is whatever arrived.
    async fn collect_body(
        &mut self,
        request_id: u32,
        head: ResponseHead,
        streamed: bool,
    ) -> Result<WorkerResponse> {
        let mut response = WorkerResponse {
            head,
            ..WorkerResponse::default()
        };
        let mut body = Vec::new();
        let mut seen_last_body = false;
        if !streamed {
            let declared = usize::try_from(response.head.body_len).unwrap_or(usize::MAX);
            if declared <= volucer_worker::codec::BODY_RESERVE_LIMIT {
                body.reserve(declared);
            }
        }
        let expected_body = if streamed { 0 } else { response.head.body_len };

        loop {
            let Some((header, payload)) = self.read_frame().await? else {
                return Err(WorkerError::Closed(format!(
                    "the worker closed its output while answering request {request_id}"
                )));
            };

            match header.message_type {
                MessageType::Body => {
                    if header.request_id != request_id {
                        return Err(WorkerError::WrongRequestId {
                            expected: request_id,
                            actual: header.request_id,
                        });
                    }
                    body.extend_from_slice(&payload);
                    if header.flags & flags::LAST != 0 {
                        seen_last_body = true;
                    }
                }
                MessageType::End => {
                    if header.request_id != request_id {
                        return Err(WorkerError::WrongRequestId {
                            expected: request_id,
                            actual: header.request_id,
                        });
                    }
                    response.end = Json::decode(&payload)?;
                    break;
                }
                MessageType::Response => {
                    return Err(WorkerError::Remote {
                        message: format!("request {request_id} was answered twice"),
                    })
                }
                MessageType::Log => self.absorb_log(&payload),
                MessageType::Metric => self.absorb_metric(&payload),
                MessageType::Event => {
                    tracing::trace!(request_id = header.request_id, "worker sent an event");
                }
                MessageType::Call => self.answer_call(&header, &payload).await?,
                MessageType::Error => {
                    let report: volucer_worker::ErrorReport = Json::decode(&payload)?;
                    if report.recoverable {
                        tracing::warn!(%report.message, "worker reported a recoverable error");
                    } else {
                        return Err(WorkerError::Remote {
                            message: report.message,
                        });
                    }
                }
                other => {
                    return Err(WorkerError::NotA {
                        expected: "a response body frame",
                        actual: other,
                    })
                }
            }
        }

        if body.len() as u64 != expected_body {
            return Err(WorkerError::Remote {
                message: format!(
                    "request {request_id} declared {expected_body} body bytes but sent {}",
                    body.len()
                ),
            });
        }
        if expected_body > 0 && !seen_last_body {
            return Err(WorkerError::Remote {
                message: format!("request {request_id} did not flag its last body frame"),
            });
        }

        response.body = body;
        Ok(response)
    }

    /// Serve a `CALL` that arrived while a request was in flight.
    ///
    /// The handler runs on the runtime thread that is driving this connection. That is the
    /// difference from the blocking path, where the exchange thread is the only one allowed to
    /// touch the pipe and the handler therefore ran there. Here the connection belongs to this
    /// task, and a handler is a synchronous function over a call — it does no I/O on the
    /// connection, so running it inline cannot interleave frames.
    ///
    /// `catch_unwind` for the reason the blocking path gives: an escaping panic would poison the
    /// connection and take the worker with it, and the failure would be a request path that dies
    /// with no log line saying why. A panic is a bug in a handler, and the right answer is a
    /// refused call.
    async fn answer_call(&mut self, header: &Header, payload: &[u8]) -> Result<()> {
        let call: volucer_worker::Call = Json::decode(payload)?;
        let answer = match &self.call_handler {
            Some(handler) if header.request_id >= volucer_worker::FIRST_CALL_ID_HALF => {
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handler(&call)))
                    .unwrap_or_else(|_| {
                        tracing::error!(
                            call = %call.name,
                            "a call handler panicked; the call is refused"
                        );
                        volucer_worker::Return::refused("the call handler panicked")
                    })
            }
            Some(_) => volucer_worker::Return::refused(format!(
                "call {} arrived with request id {}, which is a request's range",
                call.name, header.request_id
            )),
            None => volucer_worker::Return::refused(format!(
                "no call handler is installed, so {} cannot be served",
                call.name
            )),
        };
        let payload = Json::encode(&answer)?;
        self.send_frame(MessageType::Return, header.request_id, &payload, 0)
            .await
    }

    fn absorb_log(&mut self, payload: &[u8]) {
        match Json::decode::<LogRecord>(payload) {
            Ok(record) => {
                tracing::debug!(level = %record.level, "worker: {}", record.message);
                self.logs.push(record);
            }
            // A log line is not worth failing a request over.
            Err(err) => tracing::warn!(%err, "ignoring an unreadable worker log record"),
        }
    }

    fn absorb_metric(&mut self, payload: &[u8]) {
        match Json::decode::<MetricRecord>(payload) {
            Ok(record) => self.metrics.push(record),
            Err(err) => tracing::warn!(%err, "ignoring an unreadable worker metric"),
        }
    }
}

/// Read into `buffer`, distinguishing "nothing at all" from "truncated".
async fn read_exact_or_eof<R: AsyncRead + Unpin>(
    reader: &mut R,
    buffer: &mut [u8],
) -> Result<usize> {
    let mut filled = 0;
    while filled < buffer.len() {
        match reader.read(&mut buffer[filled..]).await {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(source) if source.kind() == io::ErrorKind::Interrupted => continue,
            Err(source) => return Err(WorkerError::Io(source)),
        }
    }
    Ok(filled)
}

/// A `BODY` frame arrived before the head that describes it.
fn body_before_head(request_id: u32) -> WorkerError {
    WorkerError::Remote {
        message: format!("request {request_id} sent a body before its head"),
    }
}

/// Where a connection is in draining a streamed body.
///
/// `None` on the channel means it is not mid-stream, which is the state every buffered request
/// leaves it in.
#[derive(Debug)]
struct Streaming {
    request_id: u32,
    /// Whether the `LAST`-flagged body frame has arrived.
    seen_last: bool,
    /// The terminal frame, once it has been read.
    ended: Option<RequestEnd>,
}

impl Streaming {
    fn is_finished(&self) -> bool {
        self.ended.is_some()
    }
}

/// A worker's connection, and the runtime that drives it.
///
/// **The counterpart of `ChannelExchange` for a transport that can be awaited.** The difference
/// is one sentence: `ChannelExchange` owns a *thread* that parks on a blocking read, and this owns
/// a *connection* the front-end's own runtime drives. Everything the pool asks of a slot — one
/// request at a time, a stage for every failure, a deadline enforced by killing the worker — is
/// the same, and deliberately so: a second way to reach a worker must not be a second set of rules
/// about when a request may be replayed.
///
/// # Why it holds a `Handle` rather than a runtime
///
/// Because the runtime belongs to the front-end and outlives every pool. A connection's waker
/// belongs to whichever runtime first polls it, and that runtime must stay alive for the
/// connection's whole life: a socket whose waker's runtime has been shut down fails on its next
/// poll with *"a Tokio 1.x context was found, but it is being shutdown"* (measured). Holding the
/// `Handle` is what lets the *synchronous* callers — the FastCGI front-end, the control plane, the
/// tests — reach the same connection without creating a runtime of their own; see
/// [`AsyncExchange::block_on`].
///
/// # The deadline
///
/// `tokio::time::timeout` around the whole exchange, rather than a read timeout, and the effect is
/// the same as the blocking path's: on expiry the caller kills the worker, which is what makes the
/// pending read return. A connection whose deadline fired must never be reused, so
/// `Slot::terminate` drops it exactly as it drops an exchange thread.
pub(super) struct AsyncExchange {
    /// The connection. A `tokio` mutex rather than `&mut self` because the pool posts under the
    /// slot lock and awaits outside it, and a `MutexGuard` held across an `await` would make the
    /// future `!Send`. This lock is also what guarantees the protocol's one-request-at-a-time
    /// rule on the connection itself.
    wire: Arc<tokio::sync::Mutex<Wire>>,
    handle: tokio::runtime::Handle,
    /// Set once the connection has been closed, so a later call is reported as "closed" rather
    /// than as an unexplained write failure.
    ended: AtomicBool,
}

impl std::fmt::Debug for AsyncExchange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AsyncExchange")
            .field("ended", &self.ended.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

/// The connection a slot holds, at the one capacity the pool needs.
///
/// Named rather than written inline at several sites. An `attach` relay is `unix:/path` **or**
/// `host:port` — the same `Endpoint` the FastCGI client parses — so this is an enum rather than a
/// concrete socket: `tokio::net::UnixStream` does not exist on Windows, and a relay over loopback
/// TCP is a supported configuration on every platform.
pub(super) type Wire = WireChannel<AsyncStream, AsyncStream>;

/// A connected worker, over whichever transport the relay named.
///
/// The async twin of [`volucer_fcgi::transport::Stream`], and written the same way for the same
/// reason: the two halves above it are identical, and a variant per platform would put `cfg` in
/// every signature that touches a slot. Delegating `AsyncRead`/`AsyncWrite` keeps the protocol
/// layer ignorant of which socket it is on.
#[derive(Debug)]
pub(super) enum AsyncStream {
    Tcp(tokio::net::TcpStream),
    #[cfg(unix)]
    Unix(tokio::net::UnixStream),
}

impl AsyncStream {
    /// Adopt a connected transport, which must already be non-blocking.
    ///
    /// Requires a runtime context — `tokio` will not register a descriptor without one — and that
    /// context's runtime becomes the socket's driver, because `from_std` resolves it here rather
    /// than at the first poll. It must therefore outlive the connection; see the module docs.
    pub(super) fn from_std(stream: &volucer_fcgi::Stream) -> std::io::Result<Self> {
        match stream {
            volucer_fcgi::Stream::Tcp(stream) => stream
                .try_clone()
                .and_then(tokio::net::TcpStream::from_std)
                .map(AsyncStream::Tcp),
            // A named pipe is not a `tokio` transport, and the worker protocol has none anyway: a
            // worker is stdio or a socket.
            #[cfg(windows)]
            volucer_fcgi::Stream::Pipe(_) => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "a named pipe cannot be adopted by a tokio runtime",
            )),
            #[cfg(unix)]
            volucer_fcgi::Stream::Unix(stream) => stream
                .try_clone()
                .and_then(tokio::net::UnixStream::from_std)
                .map(AsyncStream::Unix),
        }
    }
}

impl tokio::io::AsyncRead for AsyncStream {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        match self.get_mut() {
            AsyncStream::Tcp(stream) => std::pin::Pin::new(stream).poll_read(cx, buf),
            #[cfg(unix)]
            AsyncStream::Unix(stream) => std::pin::Pin::new(stream).poll_read(cx, buf),
        }
    }
}

impl tokio::io::AsyncWrite for AsyncStream {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<io::Result<usize>> {
        match self.get_mut() {
            AsyncStream::Tcp(stream) => std::pin::Pin::new(stream).poll_write(cx, buf),
            #[cfg(unix)]
            AsyncStream::Unix(stream) => std::pin::Pin::new(stream).poll_write(cx, buf),
        }
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        match self.get_mut() {
            AsyncStream::Tcp(stream) => std::pin::Pin::new(stream).poll_flush(cx),
            #[cfg(unix)]
            AsyncStream::Unix(stream) => std::pin::Pin::new(stream).poll_flush(cx),
        }
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        match self.get_mut() {
            AsyncStream::Tcp(stream) => std::pin::Pin::new(stream).poll_shutdown(cx),
            #[cfg(unix)]
            AsyncStream::Unix(stream) => std::pin::Pin::new(stream).poll_shutdown(cx),
        }
    }
}

impl AsyncExchange {
    /// Take ownership of a handshaken connection and the runtime that will drive it.
    ///
    /// Both halves are the `dup` of one socket, so a request and its answer stay ordered; they are
    /// separate objects only because reading and writing are driven separately.
    pub(super) fn start(
        reader: AsyncStream,
        writer: AsyncStream,
        handle: tokio::runtime::Handle,
        checksum: Checksum,
        call_handler: Option<Box<volucer_worker::codec::CallHandler>>,
    ) -> Self {
        let mut wire = WireChannel::new_with_policy(reader, writer, checksum);
        wire.call_handler = call_handler;
        AsyncExchange {
            wire: Arc::new(tokio::sync::Mutex::new(wire)),
            handle,
            ended: AtomicBool::new(false),
        }
    }

    /// Start an exchange over an already-connected pair, for tests.
    ///
    /// The production constructor is reached only through a handshake on a dialled relay. A test
    /// wants the same object without a supervisor, so this exists rather than duplicating the
    /// constructor's body — and it runs under the test's own runtime, which is the one that will
    /// poll the connection. `AsyncStream` is platform-neutral, so this is too: the portability gate
    /// belongs on the fixtures that need a unix socket, not on the type.
    #[cfg(test)]
    pub(super) fn start_for_test(reader: AsyncStream, writer: AsyncStream) -> Self {
        AsyncExchange {
            wire: Arc::new(tokio::sync::Mutex::new(WireChannel::new_with_policy(
                reader,
                writer,
                Checksum::Optional,
            ))),
            handle: tokio::runtime::Handle::current(),
            ended: AtomicBool::new(false),
        }
    }

    /// Whether this connection has been closed.
    pub(super) fn is_ended(&self) -> bool {
        self.ended.load(Ordering::Acquire)
    }

    /// Mark the connection unusable, so the next request through this slot replaces the worker.
    ///
    /// Used by [`InvalidateOnDrop`] when a future is cancelled mid-exchange: a dropped future leaves
    /// the wire somewhere between the request frame and the answer, and *nothing* knows how far. The
    /// slot's lease is released on drop, so the next request would otherwise be handed this
    /// connection and read the abandoned exchange's frames as its own — the frame-desynchronisation
    /// failure the whole protocol is arranged to avoid.
    pub(super) fn poison(&self) {
        self.ended.store(true, Ordering::Release);
    }

    /// Run one exchange and return its answer, from a **synchronous** caller.
    ///
    /// The bridge a blocking front-end needs. `block_in_place` when called from inside the
    /// runtime — which is what the FastCGI front-end's connection handling is — and a plain
    /// `block_on` otherwise. Both were measured to work; the distinction matters because
    /// `block_on` from inside a runtime panics, and `block_in_place` from outside one panics in the
    /// other direction.
    pub(super) fn block_on<F>(&self, work: F) -> F::Output
    where
        F: std::future::Future,
    {
        match tokio::runtime::Handle::try_current() {
            // Inside a runtime: yield this thread's slot to it while waiting, or its other tasks on
            // this thread would be starved for the whole exchange.
            Ok(_) => tokio::task::block_in_place(|| self.handle.block_on(work)),
            // Outside: nothing to yield, and nothing to starve.
            Err(_) => self.handle.block_on(work),
        }
    }

    /// Post one request and await its response.
    ///
    /// `&self` rather than `&mut self` for the reason above. The connection's own lock is what
    /// enforces one request at a time on the wire, and the pool's lease is what guarantees only one
    /// caller reaches this at all.
    pub(super) async fn request_encoded(
        &self,
        head: Vec<u8>,
        body: &[u8],
    ) -> std::result::Result<WorkerResponse, (RequestStage, WorkerError)> {
        if self.is_ended() {
            return Err((RequestStage::Reading, ended_error()));
        }
        // `Sending` as the starting stage, matching the blocking thread's tracking: the sending
        // callback runs before any read, so an early failure is attributed to the write.
        let mut stage = RequestStage::Sending;
        let mut track = |reported: RequestStage| stage = reported;
        let mut wire = self.wire.lock().await;
        match wire.request_encoded(head, body, &mut track).await {
            Ok(response) => Ok(response),
            Err(source) => {
                if source.is_fatal() {
                    self.ended.store(true, Ordering::Release);
                }
                Err((stage, source))
            }
        }
    }

    /// Post one request whose body is still arriving, and await its response.
    ///
    /// The counterpart of `ChannelExchange::request_streaming`. `chunks` blocks while the next piece
    /// is not ready, which **is** the backpressure: the connection is the only writer to the worker,
    /// so while it waits it is not sending, and the client's frames stop being polled. Blocking here
    /// blocks the runtime thread, which is why this path is used only for an upload — one thread per
    /// upload is the upload's own cost, not the request path's.
    ///
    /// `timeout` is the deadline on the **worker**, and it starts only once the body has been sent —
    /// see [`WireChannel::request_encoded_streaming`]. Until then this is waiting on the client, and
    /// how long a client takes to upload is not something the pool's `request_timeout` may decide.
    pub(super) async fn request_streaming(
        &self,
        head: Vec<u8>,
        mut chunks: Box<dyn FnMut() -> std::result::Result<Option<Vec<u8>>, WorkerError> + Send>,
        on_stage: &mut (dyn FnMut(RequestStage) + Send),
        timeout: std::time::Duration,
    ) -> std::result::Result<WorkerResponse, WorkerError> {
        if self.is_ended() {
            return Err(ended_error());
        }
        let mut wire = self.wire.lock().await;
        match wire
            .request_encoded_streaming(head, &mut chunks, on_stage, timeout)
            .await
        {
            Ok(response) => Ok(response),
            Err(source) => {
                // **The head is already on the wire with `STREAMED` set**, so this worker is
                // mid-request and cannot be reused: it is waiting for `LAST` and would read the next
                // request's frames as this body. Reported fatal, which is what makes the pool replace
                // the slot rather than hand it out again.
                self.ended.store(true, Ordering::Release);
                Err(source)
            }
        }
    }

    /// Post one request and hand back a future that yields its response.
    ///
    /// The async counterpart of [`Self::request_encoded`], and the same exchange: nothing is
    /// parked while the worker thinks, because the caller awaits the connection directly rather
    /// than blocking a thread on it. Returns an owned future rather than awaiting here so the pool
    /// can post under its slot lock and release it before waiting — a `MutexGuard` held across an
    /// `await` would make the future `!Send`, and the front-end spawns it.
    pub(super) async fn post_request_async(
        &self,
        head: Vec<u8>,
        body: &[u8],
    ) -> std::result::Result<WorkerResponse, (RequestStage, WorkerError)> {
        self.request_encoded(head, body).await
    }

    /// Post one job and await its acknowledgement.
    ///
    /// A consumer pool is a `[jobs.pool]` table, which is a spawned worker, so nothing in the
    /// server reaches this today. It exists because the alternative — an awaited connection with no
    /// way to answer a job — would be a slot that silently cannot serve its own backend's kind, and
    /// the first configuration to move a pipeline onto an `attach` relay would discover it as a
    /// protocol error rather than as a missing feature.
    pub(super) async fn request_job(
        &self,
        head: volucer_worker::JobHeader,
        body: &[u8],
    ) -> std::result::Result<
        (volucer_worker::Ack, Vec<volucer_worker::MetricRecord>),
        (RequestStage, WorkerError),
    > {
        if self.is_ended() {
            return Err((RequestStage::Reading, ended_error()));
        }
        let mut stage = RequestStage::Sending;
        let mut track = |reported: RequestStage| stage = reported;
        let mut wire = self.wire.lock().await;
        match wire.job(head, body, &mut track).await {
            Ok(ack) => {
                let (_, metrics) = wire.take_telemetry();
                Ok((ack, metrics))
            }
            Err(source) => {
                if source.is_fatal() {
                    self.ended.store(true, Ordering::Release);
                }
                Err((stage, source))
            }
        }
    }

    /// [`Self::request_encoded`], reporting the stage as it goes.
    ///
    /// The stage decides whether a failure may be replayed: bytes that never left cannot have been
    /// executed. Separate from `request_encoded` only so a caller that does not care can pass
    /// nothing.
    pub(super) async fn request_encoded_tracked(
        &self,
        head: Vec<u8>,
        body: &[u8],
        on_stage: &mut (dyn FnMut(RequestStage) + Send),
    ) -> std::result::Result<WorkerResponse, (RequestStage, WorkerError)> {
        if self.is_ended() {
            return Err((RequestStage::Reading, ended_error()));
        }
        let mut stage = RequestStage::Sending;
        let mut track = |reported: RequestStage| {
            stage = reported;
            on_stage(reported);
        };
        let mut wire = self.wire.lock().await;
        match wire.request_encoded(head, body, &mut track).await {
            Ok(response) => Ok(response),
            Err(source) => {
                if source.is_fatal() {
                    self.ended.store(true, Ordering::Release);
                }
                Err((stage, source))
            }
        }
    }

    /// Ask an idle worker for its health over the connection a runtime drives.
    ///
    /// The counterpart of `ChannelExchange::ping`, and it holds the connection's own lock for the
    /// same reason every other method on this type does: the lock **is** the protocol's
    /// one-request-at-a-time rule, so a ping taken here cannot interleave with a request that
    /// started on another task. That is also what makes it safe to call from a keepalive thread at
    /// all — the pool only pings a slot it can lock, and a slot that is serving a request is one it
    /// cannot.
    ///
    /// A fatal error marks the connection ended, exactly as a request's would: a `PONG` that never
    /// arrives because the socket is gone must not leave a slot looking healthy.
    pub(super) async fn ping(&self) -> std::result::Result<(), WorkerError> {
        if self.is_ended() {
            return Err(ended_error());
        }
        let mut wire = self.wire.lock().await;
        match wire.ping().await {
            Ok(()) => Ok(()),
            Err(source) => {
                if source.is_fatal() {
                    self.ended.store(true, Ordering::Release);
                }
                Err(source)
            }
        }
    }

    /// Send a control frame and close the connection, which is how an attached worker is stopped.
    pub(super) async fn drain_and_close(&self) {
        self.ended.store(true, Ordering::Release);
        let mut wire = self.wire.lock().await;
        let _ = wire.send_drain().await;
        wire.close().await;
    }

    /// Post one relayed request and return as soon as its **head** is known.
    ///
    /// The counterpart of `ChannelExchange::post_streamed_async`, and the one the HTTP front-end
    /// actually drives: a response whose body does not fit the buffer is relayed, so the head has
    /// to be answered before the body exists.
    ///
    /// # The pump runs as a task, and that is the whole point
    ///
    /// In the blocking design the body is pumped by the worker's exchange *thread*, which is why
    /// every relayed request costs a wakeup pair. Here the pump is a task holding the connection's
    /// guard, so it costs no thread at all — and because the guard is moved into the task, no other
    /// request can touch the connection while the body is arriving. That is the same guarantee the
    /// exchange thread's ownership gave, expressed by the mutex instead of by a thread.
    ///
    /// # `on_end` runs exactly once on every path
    ///
    /// It releases the lease, so a path that returns without it leaves the slot reserved for ever
    /// and the pool silently shrinks by one per request. Measured: an early return in the buffered
    /// branch cost 44 rps with 596 timeouts in 20 s, which is the shape of a pool that has run out
    /// of slots. So every branch below reaches it: the buffered answer, the refused relay and the
    /// relayed body.
    pub(super) async fn post_streamed_async(
        &self,
        head: Vec<u8>,
        body: &[u8],
        relay: volucer_fcgi::Relay,
        on_end: Box<dyn FnOnce(super::exchange::StreamDone) + Send>,
    ) -> std::result::Result<crate::pool::Opened, (RequestStage, WorkerError)> {
        if self.is_ended() {
            return Err((RequestStage::Reading, ended_error()));
        }

        let started = std::time::Instant::now();
        let mut stage = RequestStage::Sending;
        // An **owned** guard, taken from a clone of the `Arc` rather than borrowed from `self`: a
        // relayed body is pumped by a spawned task, and a task must be `'static`. The borrow would
        // force the task to outlive nothing, which is the wrong way round — the task owns the
        // connection for as long as the body does.
        let mut wire = Arc::clone(&self.wire).lock_owned().await;
        let mut track = |reported: RequestStage| stage = reported;

        let delivery = match wire.request_streamed(head, body, &mut track).await {
            Ok(delivery) => delivery,
            // **The ending still runs.** Nothing was opened, so the caller is told nothing — but the
            // lease must come back, or this slot is reserved for ever and the pool shrinks by one
            // per failed request. The blocking path reaches `on_end` on this branch too, through
            // `pump_streamed`'s single tail; returning here without it was a leak that read as
            // `too many connections` after about thirteen requests.
            Err(source) => {
                if source.is_fatal() {
                    self.ended.store(true, Ordering::Release);
                }
                drop(wire);
                on_end(super::exchange::StreamDone {
                    end: None,
                    complete: false,
                    failure: Some(source.to_string()),
                    metrics: Vec::new(),
                });
                return Err((stage, source));
            }
        };

        let streamed = match delivery {
            // The worker measured its response, so there is nothing to relay. It is a complete
            // response, and the caller answers with it exactly as `request` would have — but the
            // exchange is over, so the lease must be released here rather than left to a pump that
            // will never run.
            volucer_worker::codec::Delivery::Buffered(response) => {
                let end = response.end.clone();
                // The records travel with the ending, because `rendered` below drops them.
                let metrics = response.metrics.clone();
                drop(wire);
                on_end(super::exchange::StreamDone {
                    end: Some(end),
                    complete: true,
                    failure: None,
                    metrics,
                });
                return Ok(crate::pool::Opened::Buffered(crate::pool::rendered(
                    response,
                )));
            }
            volucer_worker::codec::Delivery::Streamed(streamed) => streamed,
        };

        // **The policy is consulted here, after the head, and that is the only place it can be.**
        // Whether a response may be relayed is a question about its status, its `Content-Length`
        // and whether it names a file — none of which exist until the head has arrived.
        let head_bytes = super::response::render_head(&streamed.head);
        let (_, parsed) = volucer_fcgi::parse_response_headers(&head_bytes);
        if !relay.admits(streamed.head.status, &parsed) {
            // Refused, so it is collected whole and returned as an ordinary response. Draining is
            // not optional: the rest of the body is on the connection and nothing else may read it.
            let drained = drain_async(&mut wire).await;
            let mut response = volucer_worker::codec::WorkerResponse {
                head: streamed.head,
                ..Default::default()
            };
            let outcome = match drained {
                Ok(collected) => {
                    response.body = collected;
                    if let Some(end) = wire.stream_end() {
                        response.end = end.clone();
                    }
                    let (logs, metrics) = wire.take_telemetry();
                    response.logs = logs;
                    response.metrics = metrics;
                    Ok(())
                }
                Err(source) => {
                    if source.is_fatal() {
                        self.ended.store(true, Ordering::Release);
                    }
                    Err(source)
                }
            };
            // The exchange is over whether or not the drain succeeded: a failed drain leaves the
            // connection unusable, which `complete: false` is how the pool is told.
            let end = wire.stream_end().cloned();
            // Refused by the relay policy, so this is an ordinary answer — and its records would be
            // dropped by `rendered` below. They are put in the response before it leaves.
            let metrics = std::mem::take(&mut response.metrics);
            drop(wire);
            on_end(super::exchange::StreamDone {
                complete: outcome.is_ok() && end.is_some(),
                end,
                failure: outcome.as_ref().err().map(|err| err.to_string()),
                metrics,
            });
            return match outcome {
                Ok(()) => Ok(crate::pool::Opened::Buffered(crate::pool::rendered(
                    response,
                ))),
                Err(source) => Err((RequestStage::Reading, source)),
            };
        }

        let (sender, receiver) = crate::relay::channel();
        let verdict = crate::relay::Verdict::new();
        let answered = crate::relay::StreamHead {
            head: head_bytes,
            head_duration: started.elapsed(),
            body: receiver,
            verdict: verdict.clone(),
        };

        // The guard moves into the task, so the connection stays owned by this exchange for exactly
        // as long as the body does. Only the guard, the channel and the callback move: `self` is
        // not captured, so the task outlives nothing but the connection it holds.
        tokio::spawn(async move {
            let mut wire = wire;
            let outcome = pump_body_async(&mut wire, &sender, &verdict).await;
            // Read the end frame *before* the callback, so the pool can decide whether the worker
            // is reusable from evidence rather than from an absence.
            let end = match &outcome {
                Ok(()) => wire.stream_end().cloned(),
                Err(_) => None,
            };
            let complete = outcome.is_ok() && end.is_some();
            // The guard is released before the callback, so `on_end` may take the slot lock
            // without deadlocking against this task.
            drop(wire);
            on_end(super::exchange::StreamDone {
                end,
                complete,
                failure: outcome.as_ref().err().map(|err| err.to_string()),
                // A relayed body's records were already taken by `take_telemetry` above, and
                // `pump_streamed`'s tail reports them the same way for the threaded path.
                metrics: Vec::new(),
            });
        });

        Ok(crate::pool::Opened::Relayed(
            crate::pool::RelayOpening::new(answered.into_relayed()),
        ))
    }
}

/// Forward every remaining body chunk into `sender`, and publish the verdict.
///
/// The async twin of `exchange::pump_body`, and the one place the two differ in consequence: there
/// a client that went away leaves a thread blocked in `blocking_send`, so the reader must keep
/// reading to unblock the worker. Here `send` is a future that returns `Err` once the receiver is
/// dropped, and walking off would leave the worker blocked writing into a full pipe — so the same
/// drain is still required, for the same reason.
async fn pump_body_async<W: AsyncRead + AsyncWrite + Unpin>(
    wire: &mut WireChannel<W, W>,
    sender: &crate::relay::Sender,
    verdict: &crate::relay::Verdict,
) -> Result<()> {
    loop {
        match wire.next_chunk().await {
            Ok(Some(chunk)) => {
                if sender.send(bytes::Bytes::from(chunk)).await.is_err() {
                    return drain_async(wire).await.map(|_| ());
                }
            }
            Ok(None) => {
                let stderr = super::response::render_logs(wire.telemetry_logs());
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

/// Read a streamed body to its end without forwarding it, collecting it whole.
///
/// The one caller is a body the relay policy refused, and collecting is what that caller needs: it
/// answers with the bytes exactly as a buffered request would have. Draining is mandatory either
/// way — a worker mid-body keeps writing, and a reader that walks off leaves it blocked.
async fn drain_async<W: AsyncRead + AsyncWrite + Unpin>(
    wire: &mut WireChannel<W, W>,
) -> Result<Vec<u8>> {
    let mut collected = Vec::new();
    loop {
        match wire.next_chunk().await {
            Ok(Some(chunk)) => collected.extend_from_slice(&chunk),
            Ok(None) => return Ok(collected),
            Err(err) => return Err(err),
        }
    }
}

/// The error a closed connection reports, in the one spelling the pool recognises.
fn ended_error() -> WorkerError {
    WorkerError::Closed("the worker's connection has ended".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    // Only the unix-gated timeout test needs this, so it is gated too — otherwise every other
    // platform reports it unused.
    #[cfg(unix)]
    use std::time::Duration;
    use volucer_worker::{RequestEnd, ResponseHead};

    /// The request head these tests send. Its content does not matter to the codec — it is an
    /// opaque encoded payload — only that it is valid JSON.
    const REQUEST_HEAD: &str = "{\"method\":\"GET\",\"uri\":\"/\"}";

    /// A fresh duplex, split into the worker's write half and the front-end's two halves.
    ///
    /// The worker side is one end and the front-end the other, so a test writes frames exactly
    /// where a worker would and reads them exactly where the pool would.
    fn halves() -> (
        tokio::io::DuplexStream,
        tokio::io::ReadHalf<tokio::io::DuplexStream>,
        tokio::io::WriteHalf<tokio::io::DuplexStream>,
    ) {
        let (worker_side, front_side) = tokio::io::duplex(64 * 1024);
        let (front_read, front_write) = tokio::io::split(front_side);
        (worker_side, front_read, front_write)
    }
    /// Write one frame, as a worker would.
    async fn emit<W: AsyncWrite + Unpin>(
        writer: &mut W,
        message_type: MessageType,
        request_id: u32,
        extra_flags: u16,
        payload: &[u8],
    ) {
        let mut header =
            Header::with_checksum(message_type, request_id, payload, Checksum::Optional).unwrap();
        header.flags |= extra_flags;
        writer.write_all(&header.encode()).await.unwrap();
        writer.write_all(payload).await.unwrap();
        writer.flush().await.unwrap();
    }

    /// A buffered response is read whole: status, body and the terminal frame.
    #[tokio::test]
    async fn a_buffered_response_is_read_whole() {
        let (mut worker_write, front_read, front_write) = halves();

        let answered = tokio::spawn(async move {
            let head = ResponseHead {
                status: 201,
                headers: vec![("x-test".to_string(), "yes".to_string())],
                body_len: 5,
            };
            emit(
                &mut worker_write,
                MessageType::Response,
                1,
                flags::LAST,
                &Json::encode(&head).unwrap(),
            )
            .await;
            emit(
                &mut worker_write,
                MessageType::Body,
                1,
                flags::LAST,
                b"hello",
            )
            .await;
            let end = RequestEnd {
                keep_alive: true,
                ..RequestEnd::default()
            };
            emit(
                &mut worker_write,
                MessageType::End,
                1,
                0,
                &Json::encode(&end).unwrap(),
            )
            .await;
        });

        let mut channel = WireChannel::new_with_policy(front_read, front_write, Checksum::Optional);
        let mut stage = RequestStage::Sending;
        let response = channel
            .request_encoded(REQUEST_HEAD.as_bytes().to_vec(), b"", &mut |reported| {
                stage = reported
            })
            .await
            .expect("a buffered response");

        assert_eq!(response.head.status, 201);
        assert_eq!(response.body, b"hello");
        assert!(response.end.keep_alive);
        assert_eq!(
            stage,
            RequestStage::Reading,
            "the stage covers the read phase"
        );
        answered.await.unwrap();
    }

    /// A declared length that does not match what arrives is an error, not a short body.
    #[tokio::test]
    async fn a_body_shorter_than_its_declaration_is_refused() {
        let (mut worker_write, front_read, front_write) = halves();

        let answered = tokio::spawn(async move {
            let head = ResponseHead {
                status: 200,
                body_len: 10,
                ..ResponseHead::default()
            };
            emit(
                &mut worker_write,
                MessageType::Response,
                1,
                0,
                &Json::encode(&head).unwrap(),
            )
            .await;
            emit(
                &mut worker_write,
                MessageType::Body,
                1,
                flags::LAST,
                b"hello",
            )
            .await;
            emit(
                &mut worker_write,
                MessageType::End,
                1,
                0,
                &Json::encode(&RequestEnd::default()).unwrap(),
            )
            .await;
        });

        let mut channel = WireChannel::new_with_policy(front_read, front_write, Checksum::Optional);
        let err = channel
            .request_encoded(REQUEST_HEAD.as_bytes().to_vec(), b"", &mut |_| {})
            .await
            .expect_err("a short body must not read as complete");
        assert!(
            err.to_string()
                .contains("declared 10 body bytes but sent 5"),
            "unexpected error: {err}"
        );
        answered.await.unwrap();
    }

    /// A streamed response opens on its head, and the body follows chunk by chunk.
    #[tokio::test]
    async fn a_streamed_response_opens_on_its_head_and_then_streams() {
        let (mut worker_write, front_read, front_write) = halves();

        let answered = tokio::spawn(async move {
            let head = ResponseHead {
                status: 200,
                body_len: 0,
                ..ResponseHead::default()
            };
            // `STREAMED` with no declared length is the only legal spelling.
            emit(
                &mut worker_write,
                MessageType::Response,
                1,
                flags::STREAMED,
                &Json::encode(&head).unwrap(),
            )
            .await;
            emit(&mut worker_write, MessageType::Body, 1, 0, b"one").await;
            emit(&mut worker_write, MessageType::Body, 1, flags::LAST, b"two").await;
            emit(
                &mut worker_write,
                MessageType::End,
                1,
                0,
                &Json::encode(&RequestEnd::default()).unwrap(),
            )
            .await;
        });

        let mut channel = WireChannel::new_with_policy(front_read, front_write, Checksum::Optional);
        match channel
            .request_streamed(REQUEST_HEAD.as_bytes().to_vec(), b"", &mut |_| {})
            .await
            .expect("an opening")
        {
            Delivery::Streamed(opened) => assert_eq!(opened.head.status, 200),
            Delivery::Buffered(_) => panic!("a STREAMED head must not be collected"),
        }

        let mut collected = Vec::new();
        while let Some(chunk) = channel.next_chunk().await.expect("a chunk") {
            collected.extend_from_slice(&chunk);
        }
        assert_eq!(collected, b"onetwo");
        assert!(
            channel.stream_end().is_some(),
            "the terminal frame must be recorded, or the pool cannot tell a finished stream from a \
             truncated one"
        );
        answered.await.unwrap();
    }

    /// `STREAMED` together with a declared length is a worker bug, not a preference to resolve.
    #[tokio::test]
    async fn streamed_with_a_declared_length_is_refused() {
        let (mut worker_write, front_read, front_write) = halves();

        let answered = tokio::spawn(async move {
            let head = ResponseHead {
                status: 200,
                body_len: 7,
                ..ResponseHead::default()
            };
            emit(
                &mut worker_write,
                MessageType::Response,
                1,
                flags::STREAMED,
                &Json::encode(&head).unwrap(),
            )
            .await;
        });

        let mut channel = WireChannel::new_with_policy(front_read, front_write, Checksum::Optional);
        let err = channel
            .request_streamed(REQUEST_HEAD.as_bytes().to_vec(), b"", &mut |_| {})
            .await
            .expect_err("both end signals at once must be refused");
        assert!(
            err.to_string().contains("declares no length"),
            "unexpected error: {err}"
        );
        answered.await.unwrap();
    }

    /// A frame that claims no checksum is refused when the policy requires one.
    ///
    /// The rule is `Header::refuse_unchecked`'s, shared with the blocking reader; this asserts the
    /// async path *applies* it rather than restating it.
    #[tokio::test]
    async fn an_unchecked_frame_is_refused_under_a_required_policy() {
        let (mut worker_write, front_read, front_write) = halves();

        let answered = tokio::spawn(async move {
            let payload = Json::encode(&ResponseHead::default()).unwrap();
            let mut header =
                Header::with_checksum(MessageType::Response, 1, &payload, Checksum::Optional)
                    .unwrap();
            header.flags |= flags::NO_CHECKSUM;
            worker_write.write_all(&header.encode()).await.unwrap();
            worker_write.write_all(&payload).await.unwrap();
            worker_write.flush().await.unwrap();
        });

        // What a worker that never acknowledged the offer gets.
        let mut channel = WireChannel::new_with_policy(front_read, front_write, Checksum::Required);
        let err = channel
            .request_encoded(REQUEST_HEAD.as_bytes().to_vec(), b"", &mut |_| {})
            .await
            .expect_err("an unchecked frame must be refused");
        assert!(
            err.to_string().contains("requires one"),
            "unexpected error: {err}"
        );
        answered.await.unwrap();
    }

    /// A worker that vanishes mid-response is `Closed`, not a truncated body read as complete.
    #[tokio::test]
    async fn a_worker_that_disappears_mid_response_is_reported_as_closed() {
        let (mut worker_write, front_read, front_write) = halves();

        let answered = tokio::spawn(async move {
            let head = ResponseHead {
                status: 200,
                body_len: 9,
                ..ResponseHead::default()
            };
            emit(
                &mut worker_write,
                MessageType::Response,
                1,
                0,
                &Json::encode(&head).unwrap(),
            )
            .await;
            // Then the worker vanishes — the case a body read must tell apart from a short one.
            drop(worker_write);
        });

        let mut channel = WireChannel::new_with_policy(front_read, front_write, Checksum::Optional);
        let err = channel
            .request_encoded(REQUEST_HEAD.as_bytes().to_vec(), b"", &mut |_| {})
            .await
            .expect_err("a vanished worker is a failure");
        assert!(
            err.to_string().contains("closed its output"),
            "unexpected error: {err}"
        );
        answered.await.unwrap();
    }

    /// `on_end` runs even when nothing opened, which is what releases the lease.
    ///
    /// **This is the leak that made the whole path unusable.** Every other branch reaches the
    /// callback; this one originally returned early, so a worker that failed to open a stream left
    /// its slot reserved for ever and the pool shrank by one per failure. Observed as 44 rps and
    /// `too many connections` after about thirteen requests on a twelve-worker pool.
    ///
    /// Driven over a real socket pair rather than a `duplex`: the exchange is fixed to
    /// `AsyncStream`, and a pair of unix sockets is what production actually uses.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_failed_opening_still_runs_the_ending() {
        use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
        use std::sync::Arc;

        // A `tokio` unix stream has no `try_clone`, so the pair is made by hand exactly as
        // production does it: clone at the `std` layer, then adopt both. That also exercises the
        // same adoption path the pool uses.
        let (ours, theirs) = std::os::unix::net::UnixStream::pair().unwrap();
        let ours_reader = ours.try_clone().unwrap();
        ours.set_nonblocking(true).unwrap();
        ours_reader.set_nonblocking(true).unwrap();
        let connection = Arc::new(AsyncExchange::start_for_test(
            AsyncStream::Unix(tokio::net::UnixStream::from_std(ours_reader).unwrap()),
            AsyncStream::Unix(tokio::net::UnixStream::from_std(ours).unwrap()),
        ));
        // The worker end says nothing and goes away, so no head ever arrives.
        drop(theirs);

        let called = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&called);
        let outcome = connection
            .post_streamed_async(
                REQUEST_HEAD.as_bytes().to_vec(),
                b"",
                volucer_fcgi::Relay::unknown_length(),
                Box::new(move |_done| flag.store(true, AtomicOrdering::Release)),
            )
            .await;

        assert!(outcome.is_err(), "a silent worker cannot open a response");
        assert!(
            called.load(AtomicOrdering::Acquire),
            "the ending must run on the failure path, or the slot is never released"
        );
    }

    /// A telemetry frame inside a response reaches the response *and* the lifetime buffers.
    ///
    /// Both matter, and for different readers: a response's records are what the request path
    /// reports, and the connection's are what a metrics scrape drains.
    #[tokio::test]
    async fn telemetry_inside_a_response_reaches_both_the_response_and_the_connection() {
        let (mut worker_write, front_read, front_write) = halves();

        let answered = tokio::spawn(async move {
            emit(
                &mut worker_write,
                MessageType::Log,
                volucer_worker::UNSOLICITED,
                0,
                &Json::encode(&LogRecord {
                    level: "info".to_string(),
                    message: "booted".to_string(),
                    request_id: 1,
                })
                .unwrap(),
            )
            .await;
            let head = ResponseHead {
                status: 200,
                body_len: 2,
                ..ResponseHead::default()
            };
            emit(
                &mut worker_write,
                MessageType::Response,
                1,
                0,
                &Json::encode(&head).unwrap(),
            )
            .await;
            emit(&mut worker_write, MessageType::Body, 1, flags::LAST, b"ok").await;
            emit(
                &mut worker_write,
                MessageType::End,
                1,
                0,
                &Json::encode(&RequestEnd::default()).unwrap(),
            )
            .await;
        });

        let mut channel = WireChannel::new_with_policy(front_read, front_write, Checksum::Optional);
        let response = channel
            .request_encoded(REQUEST_HEAD.as_bytes().to_vec(), b"", &mut |_| {})
            .await
            .expect("a response");
        assert_eq!(response.logs.len(), 1, "the record belongs to this request");
        assert_eq!(response.logs[0].message, "booted");

        let (logs, _metrics) = channel.take_telemetry();
        assert_eq!(
            logs.len(),
            1,
            "and it is still on the connection, which is what a scrape reads"
        );
        answered.await.unwrap();
    }

    /// A `PING` is answered by the worker's `PONG`, and that is the whole keepalive contract.
    #[tokio::test]
    async fn a_ping_is_answered_by_a_pong() {
        let (mut worker_write, front_read, front_write) = halves();
        let mut wire = WireChannel::new_with_policy(front_read, front_write, Checksum::Optional);

        let answered = tokio::spawn(async move {
            // The `PING` the server sent, read as the worker would read it: unsolicited, no payload.
            let ping = wire_frame(&mut worker_write).await;
            assert_eq!(ping.0.message_type, MessageType::Ping);
            assert_eq!(
                ping.0.request_id,
                volucer_worker::UNSOLICITED,
                "a ping belongs to no exchange, so it can never be mistaken for a response"
            );
            emit(
                &mut worker_write,
                MessageType::Pong,
                volucer_worker::UNSOLICITED,
                0,
                &[],
            )
            .await;
            worker_write
        });

        wire.ping().await.expect("the pong completes the ping");
        answered.await.unwrap();
    }

    /// A frame that is not a `PONG` is refused rather than swallowed.
    ///
    /// **This is the property the keepalive depends on.** A ping is sent into a quiet window on the
    /// pool's behalf, so a `RESPONSE` arriving here belongs to something else — and a keepalive that
    /// returned `Ok` for it would consume a frame the request path is waiting for. The failure is
    /// also what makes a worker that does not speak `PING` visible instead of silently wrong: the
    /// pool stops pinging on it (see `WorkerPool::keepalive_once`).
    #[tokio::test]
    async fn a_frame_that_is_not_a_pong_is_refused() {
        let (mut worker_write, front_read, front_write) = halves();
        let mut wire = WireChannel::new_with_policy(front_read, front_write, Checksum::Optional);

        let answered = tokio::spawn(async move {
            let _ping = wire_frame(&mut worker_write).await;
            emit(
                &mut worker_write,
                MessageType::Response,
                1,
                0,
                &Json::encode(&ResponseHead::default()).unwrap(),
            )
            .await;
            worker_write
        });

        let err = wire.ping().await.unwrap_err();
        assert!(
            matches!(err, WorkerError::NotA { .. }),
            "expected a NotA naming the frame, got {err}"
        );
        answered.await.unwrap();
    }

    /// Telemetry that happens to be in flight is absorbed rather than mistaken for the answer.
    ///
    /// A worker emits `LOG` and `METRIC` on its own schedule, so a ping landing between two of them
    /// is the ordinary case on a chatty application. `EVENT` is traced and dropped here exactly as it
    /// is by every other read loop in this module.
    #[tokio::test]
    async fn telemetry_before_the_pong_is_absorbed() {
        let (mut worker_write, front_read, front_write) = halves();
        let mut wire = WireChannel::new_with_policy(front_read, front_write, Checksum::Optional);

        let answered = tokio::spawn(async move {
            let _ping = wire_frame(&mut worker_write).await;
            emit(
                &mut worker_write,
                MessageType::Event,
                volucer_worker::UNSOLICITED,
                0,
                b"{}",
            )
            .await;
            emit(
                &mut worker_write,
                MessageType::Pong,
                volucer_worker::UNSOLICITED,
                0,
                &[],
            )
            .await;
            worker_write
        });

        wire.ping()
            .await
            .expect("telemetry does not stand in for the answer");
        answered.await.unwrap();
    }

    /// A silent peer makes the ping fail rather than hang.
    ///
    /// **The property the keepalive thread depends on.** That thread sweeps slots one at a time, so a
    /// `PING` that waited forever would stop it for every slot behind it — and the failure it defends
    /// against would then be caused by the defence. `SlotExchange::ping` bounds the await with
    /// `PING_TIMEOUT`; this checks that the bound is real rather than a comment.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_silent_peer_times_the_ping_out_rather_than_hanging() {
        let (ours, theirs) = std::os::unix::net::UnixStream::pair().unwrap();
        let ours_reader = ours.try_clone().unwrap();
        ours.set_nonblocking(true).unwrap();
        ours_reader.set_nonblocking(true).unwrap();
        let connection = AsyncExchange::start_for_test(
            AsyncStream::Unix(tokio::net::UnixStream::from_std(ours_reader).unwrap()),
            AsyncStream::Unix(tokio::net::UnixStream::from_std(ours).unwrap()),
        );

        // The worker end is alive but says nothing: a hung worker, which is the case that must not
        // stall the sweep.
        let silent = theirs;

        let outcome = tokio::time::timeout(
            Duration::from_millis(200),
            // A short deadline, so the test is fast; production uses `PING_TIMEOUT`.
            tokio::time::timeout(Duration::from_millis(50), connection.ping()),
        )
        .await;

        // The outer timeout is the assertion: if the inner one did not bound the wait, this would
        // never return and the test would hang instead of failing.
        let inner = outcome.expect("the ping must be bounded by its own deadline");
        assert!(
            inner.is_err() || inner.unwrap().is_err(),
            "a peer that never answers must not report success"
        );

        drop(silent);
    }

    /// Read one frame from the worker's side of the duplex.
    ///
    /// Written here rather than reused from the codec because it is the *other* end of the wire: the
    /// point of these tests is that the two halves agree, and calling the codec's reader would make
    /// that agreement an assumption rather than the thing under test.
    async fn wire_frame<R: AsyncRead + Unpin>(reader: &mut R) -> (Header, Vec<u8>) {
        let mut bytes = [0u8; HEADER_LEN];
        reader.read_exact(&mut bytes).await.unwrap();
        let header = Header::decode(&bytes).unwrap();
        let mut payload = vec![0u8; header.payload_len as usize];
        reader.read_exact(&mut payload).await.unwrap();
        (header, payload)
    }

    /// **A worker that accepts a streamed body and then says nothing is timed out.**
    ///
    /// The awaited arm had no deadline at all before this: `collect_response` is an unbounded read,
    /// and the threaded arm's `round_trip_streaming` is what supplied the bound. So a silent worker
    /// held its slot for ever and the pool shrank by one per silent worker — the same failure the
    /// threaded arm answers with a `504`, arrived at by a different door.
    ///
    /// The `timeout` here is deliberately short and the assertion is on the *outcome*, not on a wall
    /// clock: a test that slept for a deadline would be slow and would still pass if the deadline
    /// were applied to the wrong phase.
    #[tokio::test]
    async fn a_silent_worker_times_a_streamed_body_out() {
        let (mut worker_write, front_read, front_write) = halves();
        let mut wire = WireChannel::new_with_policy(front_read, front_write, Checksum::Optional);

        // The worker reads the head and the terminal `LAST`, then says nothing at all. Draining
        // rather than ignoring is what a real worker does, so the front-end's writes do not block
        // on a full duplex buffer and the timeout is the only thing that can end this.
        let answered = tokio::spawn(async move {
            loop {
                let (header, _) = wire_frame(&mut worker_write).await;
                if header.flags & flags::LAST != 0 {
                    break;
                }
            }
            // Then nothing. Holding the writer is what keeps the connection open.
            worker_write
        });

        let mut stage = RequestStage::Sending;
        let mut track = |reported: RequestStage| stage = reported;
        // One piece, then end of input — so `LAST` goes out and the worker's deadline starts. The
        // worker has been written to say nothing afterwards, which is the case under test.
        let mut sent = false;
        let mut chunks = || {
            if sent {
                return Ok(None);
            }
            sent = true;
            Ok(Some(b"hello".to_vec()))
        };

        let err = wire
            .request_encoded_streaming(
                REQUEST_HEAD.as_bytes().to_vec(),
                &mut chunks,
                &mut track,
                std::time::Duration::from_millis(50),
            )
            .await
            .expect_err("a worker that never answers must not be waited on for ever");

        assert!(
            matches!(err, WorkerError::Timeout(_)),
            "expected the deadline to report a timeout, got {err}"
        );
        // The body was sent and the answer was not, so the stage is what decides whether a replay is
        // sound — and it must say `Reading`, or a `POST` would be replayed against a worker that may
        // have run it.
        assert!(
            matches!(stage, RequestStage::Reading),
            "the deadline covers the answer, so the failure is attributed to reading; got {stage:?}"
        );

        // Aborted rather than awaited: this worker's job is to stay silent, so it would never
        // finish on its own.
        answered.abort();
        let _ = answered.await;
    }

    /// **The deadline does not cover the client's upload.**
    ///
    /// The other half of the same rule, and the reason the timeout is applied after `LAST` rather
    /// than around the whole exchange. A body still arriving is the client's business: a 256 MiB
    /// `POST` over a phone link takes longer than the thirty seconds this pool allows a *worker* to
    /// think, and failing it would refuse a slow upload for being slow.
    ///
    /// Written as "an upload slower than the timeout still succeeds", because that is the property.
    /// An implementation that wrapped the whole call would pass the previous test and fail this one.
    ///
    /// Multi-threaded, deliberately: `chunks` blocks the thread it is called on, which is exactly
    /// what it does in production (the front-end's pump blocks on the client's socket), so a
    /// single-threaded runtime would starve the worker task rather than exercise the phase boundary.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_slow_upload_outlives_the_worker_deadline() {
        let (mut worker_write, front_read, front_write) = halves();
        let mut wire = WireChannel::new_with_policy(front_read, front_write, Checksum::Optional);

        let answered = tokio::spawn(async move {
            // Read the whole exchange, then answer it properly.
            loop {
                let (header, _) = wire_frame(&mut worker_write).await;
                if header.flags & flags::LAST != 0 {
                    break;
                }
            }
            let head = ResponseHead {
                status: 200,
                headers: Vec::new(),
                body_len: 2,
            };
            emit(
                &mut worker_write,
                MessageType::Response,
                1,
                0,
                &Json::encode(&head).unwrap(),
            )
            .await;
            emit(&mut worker_write, MessageType::Body, 1, flags::LAST, b"ok").await;
            emit(
                &mut worker_write,
                MessageType::End,
                1,
                0,
                &Json::encode(&RequestEnd::default()).unwrap(),
            )
            .await;
            worker_write
        });

        // Three pieces, each delayed by more than the whole deadline: 300 ms of uploading against a
        // 200 ms worker deadline. If the deadline covered this phase the request would fail.
        //
        // The margin after `LAST` is deliberately generous — the worker's answer is microseconds of
        // work, so a 200 ms budget cannot be lost to scheduling. A tight deadline here would race the
        // scheduler instead of testing the phase boundary, which is what the first version of this
        // test did: it blocked a worker thread for the whole upload and then demanded the answer
        // inside the same 50 ms.
        let mut sent = 0;
        let mut chunks = || {
            if sent == 3 {
                return Ok(None);
            }
            sent += 1;
            std::thread::sleep(std::time::Duration::from_millis(100));
            Ok(Some(b"piece".to_vec()))
        };

        let mut stage = RequestStage::Sending;
        let mut track = |reported: RequestStage| stage = reported;
        let response = wire
            .request_encoded_streaming(
                REQUEST_HEAD.as_bytes().to_vec(),
                &mut chunks,
                &mut track,
                std::time::Duration::from_millis(200),
            )
            .await
            .expect("a slow upload is the client's business, not the worker's deadline");

        assert_eq!(response.head.status, 200);
        assert_eq!(response.body, b"ok");
        drop(answered.await);
    }
}
