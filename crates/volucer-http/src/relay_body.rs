//! A response body relayed from a FastCGI backend while it is still being produced.
//!
//! # Why this is here rather than in `volucer-server`
//!
//! `volucer-server` owns the reader — a blocking FastCGI client on a leased slot — and hands over a
//! channel. This side owns what `hyper` needs, which is a `Body` implementation: `poll_frame`,
//! `size_hint` and a way to say "this ended early". Those are `hyper`'s types, and `volucer-server`
//! has no business depending on the server half of `hyper` to provide them.
//!
//! # The three things this has to get right
//!
//! 1. **No declared length.** A relayed response cannot know its `Content-Length`, which is the whole
//!    reason it is relayed: the size_hint is `None` and `hyper` frames it as chunked. That is also
//!    why a relayed response is never compressed — see `relay::Relayed`.
//! 2. **A truncation is an error, not an end.** If the backend stopped before `END_REQUEST`, the body
//!    must end with an error so the connection is torn down without a terminating chunk. Ending
//!    cleanly would tell the client it had received the whole thing, which is the one outcome a
//!    relayed response must never produce.
//! 3. **A client that goes away frees the backend.** Dropping this body drops the receiver, which
//!    makes the reader's next `blocking_send` fail — so the reader stops, drains to `END_REQUEST` if
//!    it can, and the slot is usable again.
//!
//! (2) and (3) are why the body holds `finished` rather than inferring completion from the channel:
//! a channel that closes looks identical whether the backend finished or the reader thread panicked.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};

use bytes::Bytes;
use hyper::body::{Body, Frame, SizeHint};

use crate::listener::ResponseBody;

/// A response body arriving over a bounded channel from a thread reading the backend.
pub struct RelayBody {
    chunks: volucer_server::relay::Chunks,
    /// Set by the reader thread once `END_REQUEST` has been seen.
    finished: Arc<AtomicBool>,
    /// Set once the body has reported an error, so it is reported exactly once.
    failed: bool,
}

impl RelayBody {
    /// Wrap the channel a backend's reader thread feeds.
    pub fn new(chunks: volucer_server::relay::Chunks, finished: Arc<AtomicBool>) -> Self {
        RelayBody {
            chunks,
            finished,
            failed: false,
        }
    }

    /// Box it as the response body type.
    pub fn boxed(self) -> ResponseBody {
        http_body_util::BodyExt::boxed_unsync(self)
    }
}

/// The error a truncated relayed response ends with.
///
/// A sentence rather than a code, because the only thing a reader of it can learn is which of the two
/// truncations happened — and both are the same answer to a client, which is "the connection is
/// closing because what you have is not the whole response".
fn truncated() -> std::io::Error {
    std::io::Error::other("the backend stopped before completing the streamed response")
}

impl Body for RelayBody {
    type Data = Bytes;
    type Error = std::io::Error;

    fn poll_frame(mut self: std::pin::Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Result<Frame<Bytes>, std::io::Error>>> {
        if self.failed {
            return Poll::Ready(None);
        }
        match self.chunks.poll_recv(cx) {
            Poll::Ready(Some(bytes)) => Poll::Ready(Some(Ok(Frame::data(bytes)))),
            Poll::Ready(None) => {
                // The sender is gone: either the reader finished the response or it gave up on it.
                // Only the flag distinguishes the two, and reporting a giveaway as a clean end is
                // how a truncated download reaches a client with a 200.
                if self.finished.load(Ordering::Acquire) {
                    Poll::Ready(None)
                } else {
                    self.failed = true;
                    Poll::Ready(Some(Err(truncated())))
                }
            }
            Poll::Pending => Poll::Pending,
        }
    }

    fn is_end_stream(&self) -> bool {
        self.failed
    }

    fn size_hint(&self) -> SizeHint {
        // **Deliberately unknown.** A relayed response has no length to declare — that is the
        // property that put it on this path — so `hyper` frames it as chunked. Reporting an exact
        // length here would be a length the response has already contradicted.
        SizeHint::default()
    }
}
