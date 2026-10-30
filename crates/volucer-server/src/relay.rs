//! A backend response relayed instead of buffered.
//!
//! # The problem this exists for
//!
//! Every FastCGI path used to assemble a whole response before the front-end saw a byte of it:
//! `Client::receive` extended one `Vec<u8>` until `END_REQUEST`, and `from_cgi` copied it a second
//! time. So a 1 GB response — a generated CSV, an export, a media file an application streams through
//! `fpassthru` — peaked at roughly 2 GB of resident memory, and a client that gave up half way
//! through still paid for all of it on this side.
//!
//! # The shape
//!
//! [`Relayed`] is what a backend answered with when it did not fit in the buffer: the parsed header
//! block, plus a [`Body`] that produces the rest. The header block arrives **before** the body, which
//! is what lets the front-end choose a status, a content type and a framing on facts rather than on a
//! guess.
//!
//! # Why a thread rather than a channel with an async sink
//!
//! The reader is a blocking FastCGI client on a slot it has leased, and the writer is an async task
//! holding a connection to the client. Both ends have to make progress for either to finish, so
//! neither may wait on the other indefinitely:
//!
//! - The **bounded** channel gives back-pressure the only direction it is useful. A client that reads
//!   slowly leaves the channel full, the reader blocks in `blocking_send`, and the backend blocks on
//!   its own socket — which is exactly how back-pressure is supposed to reach an application.
//! - The reader runs on its own **thread**, not on a runtime worker: `blocking_send` from a worker
//!   would consume the thread the writer needs to drain the channel, which is a deadlock rather than
//!   a stall. One thread per streamed response is acceptable because only a large response starts
//!   one, and a large response is by definition not the high-frequency path.
//! - A writer that goes away (a client that hung up) **closes the receiver**, which makes
//!   `blocking_send` return `Err` at once — so the reader stops, drains what is left to `END_REQUEST`
//!   if it can, and returns the slot to the pool instead of leaving it wedged on a full pipe.
//!
//! # What is *not* here
//!
//! No compression and no `Content-Length`: both need the whole body, and the whole body is the thing
//! that does not exist yet. The front-end therefore answers a relayed response with chunked encoding
//! and `Content-Encoding` untouched from what the application sent. That is the same trade every
//! other server makes for a streamed response, and the reason a relayed response is only used when
//! the application declared no length of its own.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use tokio::sync::mpsc;

/// How many body chunks may be waiting for the client before the backend is made to wait.
///
/// Small on purpose. A chunk is a FastCGI record — at most 64 KiB — so this bounds the memory a
/// single streamed response can cost on this side at roughly 2 MiB, whatever the response's size.
/// The alternative to a small bound is a buffer that grows with the *client's* patience rather than
/// with the response, which is how a proxy runs out of memory during an incident.
pub const CHUNKS_IN_FLIGHT: usize = 32;

/// A streamed body, produced by a thread reading the backend.
///
/// Not a `Stream`: the front-end wraps it in `hyper`'s body type, and `mpsc::Receiver` has the
/// `poll_recv` a `Body` implementation needs without an extra dependency.
pub type Chunks = mpsc::Receiver<Bytes>;

/// What a backend answered with when its response did not fit in the buffer.
#[derive(Debug)]
pub struct Relayed {
    /// The `Status:` line and headers, `\r\n\r\n` included, exactly as the application wrote them.
    ///
    /// Kept raw rather than parsed here because the two front-ends parse it differently — the HTTP
    /// one has to translate it into `hyper`'s header map and the FastCGI one relays it verbatim.
    pub head: Vec<u8>,
    /// The body, chunk by chunk, ending when the sender is dropped.
    pub body: Chunks,
    /// How long the backend took to produce the header block. Time to first byte.
    pub head_duration: Duration,
    /// Set when the backend finished cleanly. Read after the body is drained.
    ///
    /// A `false` here means the response was truncated, and the front-end must not pretend
    /// otherwise: for HTTP/1.1 the only honest signal left is closing the connection without the
    /// final chunk, which is what `hyper` does when a body ends with an error.
    pub(crate) finished: Arc<AtomicBool>,
    /// `STDERR` collected while the response was being relayed.
    pub(crate) stderr: Arc<std::sync::Mutex<Vec<u8>>>,
}

impl Relayed {
    /// Whether the backend reached `END_REQUEST` after every byte had been relayed.
    pub fn finished(&self) -> bool {
        self.finished.load(Ordering::Acquire)
    }

    /// The same flag the reader thread will set, for a body that has to report truncation.
    ///
    /// Handed out rather than a `bool` because the answer is not known yet when the head is returned:
    /// the body polls it after the channel closes, which is the only point at which it can be.
    pub fn finished_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.finished)
    }

    /// The `STDERR` bytes seen so far.
    pub fn stderr(&self) -> Vec<u8> {
        self.stderr
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Split the head into its status, headers and (empty) body.
    pub fn parse(&self) -> (u16, volucer_fcgi::ParsedHeaders<'_>) {
        volucer_fcgi::parse_response_headers(&self.head)
    }
}

/// The handles a reader thread writes its verdict into.
///
/// A small struct rather than two arguments because they must be updated in the same order every
/// time — `finished` after `stderr`, so a reader that observes `finished == true` also observes the
/// last `STDERR` byte.
pub(crate) struct Verdict {
    pub(crate) finished: Arc<AtomicBool>,
    pub(crate) stderr: Arc<std::sync::Mutex<Vec<u8>>>,
}

impl Verdict {
    pub(crate) fn new() -> Self {
        Verdict {
            finished: Arc::new(AtomicBool::new(false)),
            stderr: Arc::new(std::sync::Mutex::new(Vec::new())),
        }
    }

    /// Publish the outcome, in the order a reader may rely on.
    pub(crate) fn publish(&self, stderr: &[u8], finished: bool) {
        {
            let mut slot = self
                .stderr
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            slot.clear();
            slot.extend_from_slice(stderr);
        }
        self.finished.store(finished, Ordering::Release);
    }
}

/// Build the channel pair a reader thread and the front-end share.
pub(crate) fn channel() -> (mpsc::Sender<Bytes>, Chunks) {
    mpsc::channel(CHUNKS_IN_FLIGHT)
}

/// Give a streamed response its own thread and hand back the head plus the channel.
///
/// # Why one helper rather than one per backend
///
/// Every relaying backend does the same four things: pump the body into a bounded channel, publish
/// the verdict, release whatever the backend lent for the request, and count the request. The *fifth*
/// step — what to do with the client when the response ends — differs, so it is a closure. Written per
/// backend, the loop would be three copies of the same reasoning, and the one that drifted would be
/// the one with the subtle bug: a permit released before the body was drained.
///
/// # Why a thread rather than a task
///
/// `blocking_send` on a runtime worker consumes the thread the *receiver* needs in order to drain the
/// channel, which is a deadlock rather than a stall. A separate thread is the only shape in which both
/// ends can block. It costs one thread per streamed response, which is acceptable because only a large
/// response starts one.
///
/// # What `on_end` may rely on
///
/// It runs on the reader thread after the last byte, with the verdict already published — so it may
/// hand the client back to a pool and must not assume anything after it returns.
pub(crate) fn spawn_reader<T, F>(
    backend: String,
    mut client: volucer_fcgi::Client<T>,
    mut stream: volucer_fcgi::ResponseStream,
    on_end: F,
) -> std::io::Result<Relayed>
where
    T: std::io::Read + std::io::Write + Send + 'static,
    F: FnOnce(volucer_fcgi::Client<T>, &volucer_fcgi::ResponseStream) + Send + 'static,
{
    let head = stream.head().to_vec();
    let head_duration = stream.head_duration();
    let verdict = Verdict::new();
    let (sender, receiver) = channel();
    let finished = Arc::clone(&verdict.finished);
    let stderr = Arc::clone(&verdict.stderr);

    std::thread::Builder::new()
        .name("volucer-relay".to_string())
        .spawn(move || {
            let mut forwarded = 0usize;
            loop {
                match client.next_chunk(&mut stream) {
                    Ok(Some(chunk)) => {
                        forwarded += chunk.len();
                        if sender.blocking_send(Bytes::from(chunk)).is_err() {
                            // **The client went away.** Drained rather than abandoned: the backend is
                            // mid-write, and a connection left half-read wedges the slot on a full
                            // pipe — which is not recoverable from anywhere else.
                            let _ = client.drain_stream(&mut stream);
                            break;
                        }
                    }
                    Ok(None) => break,
                    Err(err) => {
                        tracing::warn!(
                            backend = %backend,
                            %err,
                            "a streamed response failed part way through"
                        );
                        break;
                    }
                }
            }
            verdict.publish(stream.stderr(), stream.is_finished());
            let complete = stream.is_finished();
            on_end(client, &stream);
            tracing::debug!(
                backend = %backend,
                bytes = forwarded,
                finished = complete,
                "a streamed response ended"
            );
        })?;

    Ok(Relayed {
        head,
        body: receiver,
        head_duration,
        finished,
        stderr,
    })
}

/// The handles a reader thread and the body share, so the body can report truncation.
///
/// Two `Arc`s rather than one, and that is deliberate: `finished` is read by the body on every poll
/// (cheaply, through `Acquire`) while `stderr` is only touched once at the end. Fusing them into one
/// mutex would put a lock on the body's poll path for a value only the log reads.
pub fn shared() -> (Arc<AtomicBool>, Arc<std::sync::Mutex<Vec<u8>>>) {
    (
        Arc::new(AtomicBool::new(false)),
        Arc::new(std::sync::Mutex::new(Vec::new())),
    )
}
