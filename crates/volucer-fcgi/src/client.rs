//! The FastCGI client.
//!
//! Deliberate design points, all consequences of the hazards FastCGI is famous for:
//!
//! - **One request in flight per connection.** `php-cgi` processes requests
//!   sequentially, so multiplexing would add a demux table and buy nothing. A
//!   client owns one connection and sends one request at a time.
//! - **`STDOUT` is returned raw.** The FastCGI front-end relays these bytes
//!   verbatim, so the client never parses headers or bodies on the hot path.
//! - **`STDERR` is never dropped.** It is accumulated and handed back with the
//!   response. A caller that discards it will eventually block a backend on a
//!   full pipe — the classic "all workers hung" bug.
//! - **Keep-alive is reported, not assumed.** `FCGI_KEEP_CONN` is a request; the
//!   backend may still close. The caller decides whether to reuse.

use std::io::{BufReader, Read, Write};
use std::time::{Duration, Instant};

use crate::error::{FcgiError, Result};
use crate::message::{BeginRequest, EndRequest};
use crate::nvpair::{self, NameValuePairs};
use crate::protocol::{ProtocolStatus, RecordType};
use crate::record::{self, Header};
use crate::transport::{Endpoint, Stream};

/// The request id used for every real request.
///
/// With one request in flight and no multiplexing a constant id is correct, and
/// it keeps captured traffic readable.
pub const REQUEST_ID: u16 = 1;

/// `GET_VALUES` is not tied to a request, so it uses request id 0.
pub const MANAGEMENT_REQUEST_ID: u16 = 0;

/// The capacity of the buffer the transport is read through.
///
/// A FastCGI record's content length is a `u16` and a 12 KB page arrives as a handful of
/// records, so this covers a whole response in the common case and several records in any
/// case. It is kept below the 8 KiB a socket returns in a single read: asking for more than
/// the kernel will hand over means the buffer is never full, so every `read` still enters the
/// kernel once — just far fewer times than one per record field.
const BUF_LEN: usize = 8 * 1024;

/// What to send to a backend for one request.
#[derive(Debug, Clone, Default)]
pub struct Request {
    /// The CGI environment, sent as `PARAMS`.
    pub params: NameValuePairs,
    /// The raw request body, sent as `STDIN`.
    ///
    /// volucer never interprets this: multipart and `$_FILES` handling belongs
    /// to the PHP SAPI, so the bytes are passed through untouched.
    pub body: Vec<u8>,
    /// Ask the backend to keep the connection open after responding.
    pub keep_conn: bool,
}

/// What a backend sent back.
#[derive(Debug, Clone, Default)]
pub struct Response {
    /// Raw `STDOUT` bytes: a `Status:`-prefixed header block, a blank line, then
    /// the body. Left unparsed so it can be relayed verbatim.
    pub stdout: Vec<u8>,
    /// `STDERR` bytes. Always collected; see the module docs.
    pub stderr: Vec<u8>,
    /// The `END_REQUEST` body.
    pub end: Option<EndRequest>,
    /// Whether the connection may be reused for another request.
    pub keep_conn: bool,
    /// How long the backend took to answer.
    pub duration: Duration,
}

/// A parsed header block, borrowed from the payload it was found in.
///
/// `&str` pairs rather than `(String, String)`: locating a header is a byte scan,
/// so copying each name and each value into its own `String` was two allocations
/// and two `memcpy`s per header — on the order of twenty allocations for a page
/// that sends ten headers — to hand back bytes that are still alive at every call
/// site ([`Response::parse`] already returns a body borrowing the same payload, so
/// no caller was gaining anything from the ownership). A caller that needs an owned
/// `String` — the worker's JSON protocol does — converts at that boundary, where
/// the copy is unavoidable.
///
/// Text rather than bytes because that is what a header is: the parse already
/// rejects non-ASCII, and ASCII is a subset of UTF-8, so the conversion is free.
pub type ParsedHeaders<'a> = Vec<(&'a str, &'a str)>;

impl Response {
    /// The application status reported by `END_REQUEST`, if any.
    pub fn app_status(&self) -> Option<u32> {
        self.end.map(|end| end.app_status)
    }

    /// The protocol status reported by `END_REQUEST`, if any.
    pub fn protocol_status(&self) -> Option<ProtocolStatus> {
        self.end.map(|end| end.protocol_status)
    }

    /// Fail if the backend did not complete the request.
    ///
    /// A non-zero *application* status is not an error here: PHP applications
    /// legitimately exit non-zero, and the HTTP status lives in the `Status:`
    /// header instead. Only the protocol status indicates a transport-level
    /// refusal such as `FCGI_OVERLOADED`.
    pub fn check_protocol(&self) -> Result<()> {
        match self.end {
            None => Err(FcgiError::IncompleteResponse {
                request_id: REQUEST_ID,
                complete: false,
            }),
            Some(end) if !end.protocol_status.is_complete() => {
                Err(FcgiError::BackendProtocolStatus {
                    status: end.protocol_status,
                })
            }
            Some(_) => Ok(()),
        }
    }

    /// Interpret `STDOUT` as `Status:` header + headers + body.
    ///
    /// Only needed when a caller wants the HTTP status rather than passing the
    /// bytes through, so the hot path never calls this.
    pub fn parse_status_headers(&self) -> (u16, ParsedHeaders<'_>) {
        let (status, headers, _) = parse_response(&self.stdout);
        (status, headers)
    }

    /// Status, headers and body located in a single pass over `STDOUT`.
    ///
    /// What a front-end that speaks a different protocol actually needs: it has
    /// to write its own header block and forward the body, so it wants all three.
    /// Asking [`parse_response_headers`] and [`split_response_body`] separately
    /// makes each one hunt for the same blank line, which is a full scan of the
    /// payload apiece — `perf` attributed 13.6% of the server process's CPU to the
    /// pair before they were combined into this.
    pub fn parse(&self) -> (u16, ParsedHeaders<'_>, &[u8]) {
        parse_response(&self.stdout)
    }
}

/// Split a FastCGI `STDOUT` payload into its status and headers.
///
/// A response that states no `Status:` is `200` per the specification — **unless it carries a
/// `Location`**, which makes it a `302`: RFC 3875 §6.2.3 leaves the status to the server when
/// the application states none, and a bare `Location` is the spec's way of asking for a
/// redirect. nginx, Caddy and Apache all resolve it the same way. See [`parse_response`].
///
/// The whole-payload `String::from_utf8_lossy` this used to start with was the
/// most expensive thing on the HTTP hot path after the checksum: it validates
/// UTF-8 across the *entire* `STDOUT` payload — body included — and copies it
/// when it is not already valid, to locate a `\r\n\r\n` that can be found on the
/// bytes for free. `perf` put it at 5.2% of the server process's CPU on a 12 KB
/// page (scripts/diag-profile.sh).
///
/// Only the header block needs to be text, and it is ASCII in practice. The body
/// is never touched here beyond being skipped, which also means a body that is
/// not valid UTF-8 can no longer influence the headers — a real bug the lossy
/// conversion hid, because a replacement character inside the body could move
/// the located header terminator.
pub fn parse_response_headers(stdout: &[u8]) -> (u16, ParsedHeaders<'_>) {
    let (status, headers, _) = parse_response(stdout);
    (status, headers)
}

/// Status, headers and body, with the payload scanned once.
///
/// The one entry point a front-end should use: [`parse_response_headers`] and
/// [`split_response_body`] both delegate here, so a caller that needs both pays
/// for one search rather than two.
///
/// The headers come back as slices of `stdout` rather than as owned `String`s,
/// which removes the two allocations per header that used to go with locating
/// one — see [`ParsedHeaders`]. The payload must outlive the result, which every
/// call site already arranges because the body borrows it too.
pub fn parse_response<'a>(stdout: &'a [u8]) -> (u16, ParsedHeaders<'a>, &'a [u8]) {
    let (head, body) = match find_header_end(stdout) {
        Some((ends_at, separator)) => {
            // `find_header_end` only ever returns an index whose separator fits
            // inside the payload, so this cannot go out of range; `get` says so in
            // the type rather than leaving a panic path behind an invariant held in
            // another function.
            (
                &stdout[..ends_at],
                stdout.get(ends_at + separator..).unwrap_or(&[]),
            )
        }
        // No blank line at all. The whole payload is read as the header block, which
        // is also why the body comes back empty: a real FastCGI response always
        // carries a header and a terminator, so this is a malformed response and the
        // choice is only about which half to call it. This case costs nothing in
        // practice — it cannot happen for a well-formed backend — so the reading that
        // recovers the most information wins over the cheapest one.
        None => (stdout, &[][..]),
    };

    let mut status = 200u16;
    // Whether the header block stated a status at all. It decides the `Location` rule below,
    // and it cannot be inferred from `status` afterwards because a backend may legitimately
    // send `Status: 200` explicitly.
    let mut stated = false;
    // Sixteen bytes per header is a typical line. Reserving saves the first few
    // doublings on a response carrying the usual dozen headers.
    let mut headers: ParsedHeaders<'a> = Vec::with_capacity(head.len() / 16);

    let mut lines = head.split(|&byte| byte == b'\n');
    // The status line is the first line with anything on it. Consuming it here rather
    // than comparing an index inside the loop is what makes the rest of the loop a
    // plain `for` — and it fixes the index: the old `enumerate()` counted lines that
    // the empty-line `continue` had skipped, so a block that opened with a line break
    // made the real first line `index == 1` and its `Status:` was silently ignored.
    for line in lines.by_ref() {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() {
            continue;
        }
        if let Some((name, value)) = header_parts(line) {
            if name.eq_ignore_ascii_case(b"status") {
                status = parse_status_code(value).unwrap_or(200);
                stated = true;
            } else {
                push_ascii(&mut headers, name, value);
            }
        }
        break;
    }
    for line in lines {
        push_header(&mut headers, line.strip_suffix(b"\r").unwrap_or(line));
    }

    // **A `Location` with no `Status:` is a `302`, not a `200`.** CGI/1.1 and nginx agree on
    // this, and it is the one part of the spec this parser used to get wrong: RFC 3875 §6.2.3
    // makes the *server* choose the status from the response fields, and §6.2.4 gives
    // client-redirect-with-document its own status precisely because a bare `Location` means
    // "send the client somewhere else". PHP frameworks always emit `Status`, so a PHP-only
    // deployment never saw the difference — a Rack or WSGI application that sets `Location` and
    // lets the server pick (Rack's `[302, {"Location" => ...}, []]` is usually explicit, but
    // Sinatra and several WSGI middlewares are not) would have been answered `200` with a
    // `Location` header, which no client follows.
    //
    // Only when the backend stated nothing: an explicit `Status: 200` beside a `Location` is
    // the application saying it meant a body with a location in it, and overriding that would
    // be this parser inventing a decision the application made.
    if !stated
        && headers
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("location"))
    {
        status = 302;
    }

    (status, headers, body)
}

/// Keep one header line, if it is a usable header.
fn push_header<'a>(headers: &mut ParsedHeaders<'a>, line: &'a [u8]) {
    if let Some((name, value)) = header_parts(line) {
        push_ascii(headers, name, value);
    }
}

/// Keep one already-split header, if it is ASCII.
///
/// A name or value that is not ASCII cannot be sent as an HTTP header anyway; the
/// lossy conversion used to mangle it and then try. Skipping is the honest outcome,
/// and it keeps this free of `String::from_utf8_lossy`. The two checks are separate
/// `let`s on purpose: a tuple pattern would scan the value even when the name had
/// already failed.
fn push_ascii<'a>(headers: &mut ParsedHeaders<'a>, name: &'a [u8], value: &'a [u8]) {
    let Some(name) = ascii_of(name) else {
        return;
    };
    let Some(value) = ascii_of(value) else {
        return;
    };
    headers.push((name, value));
}

/// Split one header line into its trimmed name and trimmed value.
///
/// `None` for an empty line or a line with no colon, which are the two cases the
/// caller skips. The `Status:` line goes through this as well, so a space before
/// the colon (`Status : 200`, which HTTP forbids but nothing else checks) is
/// treated the same for the status as for every ordinary header rather than
/// landing in the header list while the status stays `200`.
fn header_parts(line: &[u8]) -> Option<(&[u8], &[u8])> {
    let colon = line.iter().position(|&byte| byte == b':')?;
    let (name, value) = (trim_ascii(&line[..colon]), trim_ascii(&line[colon + 1..]));
    if name.is_empty() {
        return None;
    }
    Some((name, value))
}

/// Where the header block ends and how long its separator is.
///
/// One forward pass, checking both `\r\n\r\n` and `\n\n` at every byte that
/// could start either. That is equivalent to the minimum of two independent
/// searches — the earliest match wins, and the two patterns cannot begin at the
/// same offset because one starts with `\r` and the other with `\n`.
///
/// The scan is a hand-written loop rather than `memchr::memmem::find`, because
/// `memchr` is not a dependency here and the scan stops at the end of the *header*
/// block, which is tens to a few hundred bytes in practice — a vectorised skip loop
/// would have nothing to skip. What the loop does avoid is comparing four bytes at
/// every offset: the `bytes[index]` match is what gates the slice comparison, so an
/// ordinary header byte costs one comparison rather than two slice compares.
///
/// The slice comparisons rather than four indexed loads are deliberate: the bound
/// guard becomes `starts_with`'s own, and comparing the same way everywhere is what
/// lets the comparison go through the slice `PartialEq` (a `memcmp` for a fixed
/// length) instead of three separate byte checks.
fn find_header_end(bytes: &[u8]) -> Option<(usize, usize)> {
    let len = bytes.len();
    let mut index = 0;
    // Both terminators are at least two bytes long, so the last offset one can start
    // at is `len - 2`. This bound is too strict for a future one-byte terminator —
    // worth knowing before adding one.
    while index + 1 < len {
        match bytes[index] {
            b'\r' if bytes[index..].starts_with(b"\r\n\r\n") => return Some((index, 4)),
            b'\n' if bytes[index..].starts_with(b"\n\n") => return Some((index, 2)),
            _ => {}
        }
        index += 1;
    }
    None
}

/// The leading and trailing whitespace-free slice, as `str::trim` (which trims
/// ASCII whitespace and Unicode spaces) did for the ASCII that headers contain.
///
/// This delegates to `[u8]::trim_ascii` in std, which arrived in Rust 1.80. The
/// workspace's MSRV is 1.85, so it is available — and the MSRV is 1.85 because the
/// dependency tree needs it, not because this function does: `clap`, `hyper-util`,
/// `indexmap` and `hashbrown` all declare 1.85. The hand-rolled `position`/`rposition`
/// pair this replaced was correct, but it was two passes where one call will do.
fn trim_ascii(bytes: &[u8]) -> &[u8] {
    bytes.trim_ascii()
}

/// Interpret a header name or value as ASCII, or nothing.
///
/// Borrowed, not owned: the previous version built a `String` here, which was two
/// allocations and two `memcpy`s per header on every response. The ASCII check *is*
/// the UTF-8 validation (ASCII is a subset), so the returned `&str` costs nothing
/// beyond the scan that was already being made.
///
/// A `&[u8]` that is already ASCII would be accepted by `from_utf8` as well; the
/// difference is only the extra validity scan, which is why the check comes first.
fn ascii_of(bytes: &[u8]) -> Option<&str> {
    if bytes.is_ascii() {
        // SAFETY: `is_ascii` guarantees valid UTF-8, which is exactly the
        // precondition of `from_utf8_unchecked`.
        Some(unsafe { std::str::from_utf8_unchecked(bytes) })
    } else {
        // `std::str::from_utf8` would re-scan to say the same thing more slowly.
        None
    }
}

/// The numeric status from a `Status:` value, ignoring its reason phrase.
fn parse_status_code(value: &[u8]) -> Option<u16> {
    let end = value
        .iter()
        .position(|byte| byte.is_ascii_whitespace())
        .unwrap_or(value.len());
    ascii_of(&value[..end])?.parse().ok()
}

/// Split a FastCGI `STDOUT` payload into its header block and its body.
///
/// A payload with no blank line at all is treated as having no body. That is the
/// existing behaviour and it is worth naming rather than leaving to be
/// rediscovered: a real FastCGI response always carries at least one header and a
/// terminator, so "no separator" means a malformed response, and the choice is
/// between calling all of it headers or all of it body. `parse_response_headers`
/// reads it as headers, and this agrees.
///
/// The body is located on the bytes rather than through a lossy `String`, because
/// the body is arbitrary application bytes.
///
/// This finds the terminator itself rather than delegating to [`parse_response`],
/// even though that would agree — the parse builds the header list, and a caller that
/// wants only the body would pay for a `Vec` it never reads. There is an allocation
/// test for exactly this (`tests/allocations.rs`), which is how it was noticed.
pub fn split_response_body(stdout: &[u8]) -> &[u8] {
    match find_header_end(stdout) {
        // The offset cannot exceed the length: `find_header_end` only returns a
        // separator that fits.
        Some((head, separator)) => stdout.get(head + separator..).unwrap_or(&[]),
        None => &[],
    }
}

/// Which responses a caller is willing to receive in pieces.
///
/// A type rather than a `bool`, because the two rules it carries are the interesting part and a
/// `bool` at a call site cannot carry either:
///
/// - **A response that already states its own length is left alone by default.** `Content-Length`
///   is the backend telling the client how much to expect, and re-framing it as a chunked stream
///   throws that away for no gain. A `.php` that reads a 2 MB file and `echo`s it has already paid
///   for the buffering; a `.php` that generates a CSV does not know its length, which is exactly the
///   case that has to stream.
/// - **An error is never relayed.** A `502`/`500` page has to be answered as one, with its body
///   available to the front-end's error-page machinery, and a status the server may want to rewrite
///   must not already be on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Relay {
    enabled: bool,
    only_without_length: bool,
}

impl Relay {
    /// Relay nothing: every response is buffered, whatever its size.
    pub fn never() -> Self {
        Relay {
            enabled: false,
            only_without_length: false,
        }
    }

    /// Relay any response larger than the caller's buffer, whatever its declared length.
    pub fn above_budget() -> Self {
        Relay {
            enabled: true,
            only_without_length: false,
        }
    }

    /// Relay a response larger than the buffer **only when it declares no length**.
    ///
    /// The default, and the one that matches what every other server does: a response the backend
    /// measured is sent as it was measured, and only a response of unknown length is re-framed.
    pub fn unknown_length() -> Self {
        Relay {
            enabled: true,
            only_without_length: true,
        }
    }

    /// The relay a deployment's `stream_threshold_bytes` asks for.
    ///
    /// **The threshold and "may I relay" are one decision, not two**, and this is where that is made.
    /// A budget of zero means "buffer everything", which is the escape hatch an operator uses to keep
    /// compression and a declared length at the cost of unbounded downloads — and it would be a lie if
    /// the budget were zero *and* relaying were still enabled, because every response is larger than
    /// zero bytes.
    pub fn for_threshold(budget: usize) -> Self {
        if budget == 0 {
            Relay::never()
        } else {
            Relay::unknown_length()
        }
    }

    /// Whether this status and header block may be relayed at all.
    ///
    /// Public because a backend that frames its own responses — the worker pool, whose `STREAMED`
    /// flag is decided by the application — has to ask the same question about a head it parsed
    /// itself. Answering it twice would let the two paths disagree about which responses may be
    /// relayed, and the disagreement would show up as an error page arriving chunked.
    ///
    /// The decision reads the *parsed* head rather than the raw bytes, because the question is about
    /// what the response says — a `Status:` line, a `Content-Length`, a file name — not about its
    /// spelling.
    ///
    /// **Three refusals, each for its own reason:**
    ///
    /// - **A non-2xx status**, because an error page has to reach the front-end whole: it is the
    ///   answer `error_pages` and the gRPC translation both read, and a status already on the wire
    ///   cannot be rewritten.
    /// - **A declared `Content-Length`**, when the caller asked for `unknown_length` only. The
    ///   backend measured its response; re-framing it as a chunked stream throws that measurement
    ///   away for no gain.
    /// - **A `X-Sendfile`/`X-Accel-Redirect` name.** Such a response is an *instruction*, and the
    ///   front-end answers it from the static layer — which needs the head and nothing else. Relaying
    ///   it would put the reader thread and the file in a race over one slot, so it is answered
    ///   buffered as it always was. The body of such a response is a placeholder of a few bytes, so
    ///   buffering it costs nothing.
    pub fn admits(&self, status: u16, headers: &ParsedHeaders<'_>) -> bool {
        if !self.enabled {
            return false;
        }
        if !(200..300).contains(&status) {
            return false;
        }
        if headers.iter().any(|(name, _)| {
            volucer_core::SENDFILE_HEADERS
                .iter()
                .any(|candidate| name.eq_ignore_ascii_case(candidate))
        }) {
            return false;
        }
        if !self.only_without_length {
            return true;
        }
        !headers
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("content-length"))
    }
}

/// What became of a response the caller asked to receive incrementally.
///
/// Two shapes rather than a flag on [`Response`], because the caller's obligations differ: a
/// buffered answer has no lifecycle left to respect, while a streamed one means bytes have already
/// been handed over and the connection is only reusable if the backend finished.
///
/// **A streamed response is not complete when this value is returned, and that is the contract.**
/// The head is known and the body is still on the wire, so the caller receives the head as part of
/// [`StreamHead`] and drains the rest with [`ResponseStream`] — which is what lets it decide a `Content-Type`
/// and a framing before the first body byte, exactly as an origin's response allows.
#[derive(Debug)]
pub enum Delivery {
    /// The whole response stayed within the caller's buffer, so this is the ordinary [`Response`] —
    /// still compressible, still carrying its own `Content-Length`, still checked for truncation.
    Buffered(Response),
    /// The response outgrew the buffer, or the caller asked for every answer to be relayed.
    ///
    /// The header block has been read and split off; the body has not been touched beyond the
    /// records already sitting in the client's own buffer.
    Streamed(ResponseStream),
}

impl Delivery {
    /// Whether the backend delivered a whole response within the caller's budget.
    pub fn is_buffered(&self) -> bool {
        matches!(self, Delivery::Buffered(_))
    }
}

/// A response being relayed: the head is known, the body is not.
///
/// Holds nothing but the client it came from and the position it has reached, so what the caller
/// owns is "a connection mid-response" — which is why it must be drained or dropped and never
/// abandoned: dropping it closes the connection, which is the correct end for one that still has
/// records in the pipe.
#[derive(Debug)]
pub struct ResponseStream {
    /// The header block, `\r\n\r\n` included, so [`parse_response`] works on it unchanged.
    head: Vec<u8>,
    /// One already-read record, when the switch happened part-way through the buffer.
    pending: Option<Vec<u8>>,
    /// Whether every byte has been relayed and `END_REQUEST` was seen.
    finished: bool,
    /// `STDERR` seen so far. Grows as the body is drained, because a backend writes both.
    stderr: Vec<u8>,
    /// The `END_REQUEST` body, once it arrives.
    end: Option<EndRequest>,
    /// How long the backend had taken when the head came back.
    ///
    /// Recorded there rather than at the end because it is the figure that matters for a stream:
    /// time-to-first-byte. A total is not knowable until the client stops reading.
    started: Instant,
    head_duration: Duration,
    /// Whether the backend agreed to keep the connection.
    keep_conn_requested: bool,
}

impl ResponseStream {
    /// The header block, `\r\n\r\n` included.
    pub fn head(&self) -> &[u8] {
        &self.head
    }

    /// `STDERR` seen so far.
    pub fn stderr(&self) -> &[u8] {
        &self.stderr
    }

    /// Time to first byte.
    pub fn head_duration(&self) -> Duration {
        self.head_duration
    }

    /// The total time so far, which is only the total if the stream has finished.
    pub fn duration(&self) -> Duration {
        self.started.elapsed()
    }

    /// Whether every byte has arrived.
    pub fn is_finished(&self) -> bool {
        self.finished
    }

    /// Whether the connection can serve another request.
    ///
    /// False until the stream has been drained to `END_REQUEST`, and false for a backend that
    /// declared `FCGI_KEEP_CONN` but refused to honour it.
    pub fn is_reusable(&self) -> bool {
        self.finished && self.keep_conn_requested
    }

    /// The application status, once `END_REQUEST` has arrived.
    pub fn app_status(&self) -> Option<u32> {
        self.end.map(|end| end.app_status)
    }

    /// The protocol status, once `END_REQUEST` has arrived.
    pub fn protocol_status(&self) -> Option<ProtocolStatus> {
        self.end.map(|end| end.protocol_status)
    }
}

/// A client bound to one backend connection.
///
/// Generic over the transport so the protocol can be tested without a socket.
///
/// The transport is a [`BufReader`] around `T` rather than `T` itself, and reads go through
/// `get_mut` so that a write bypasses the read buffer. The reason is on the read side: a
/// FastCGI response arrives as a sequence of records, each framed as a header followed by a
/// content payload, and `record::read_record` asks for the two separately. Unbuffered that is
/// two entries into the kernel per record, so a two-record reply costs four reads — and over a
/// socket each one is a `read(2)` that could have been answered from what the previous one
/// already brought in. Measured on loopback: four reads for an answer small enough to arrive in
/// one segment. Buffered, it is one.
///
/// Writes still go out as one call per request, exactly as before, because `get_mut` hands the
/// underlying transport back without touching the buffer.
#[derive(Debug)]
pub struct Client<T = Stream> {
    stream: BufReader<T>,
    /// The request being built, reused across requests.
    ///
    /// A request is assembled here rather than written straight to the stream
    /// because of how it is framed: a FastCGI request is a `BEGIN_REQUEST` header,
    /// eight bytes of body, a `PARAMS` record, its terminator, a `STDIN` record and
    /// its terminator. Written field by field that is eight small writes, and on a
    /// socket with `TCP_NODELAY` set — which this client sets, and which it should —
    /// each one becomes its own TCP segment (measured: 8 segments per request with
    /// `tcpdump`, against 1–2 for nginx).
    ///
    /// Holding the buffer lets the whole request go out in a single write. It is a
    /// field rather than a local so the allocation survives across requests and the
    /// hot path never allocates; this is `&mut self` in `send`, so a single `self`
    /// needs no borrow juggling to write its own buffer.
    out: Vec<u8>,
    /// The encoded `PARAMS` payload, kept separately from `out` so it can be built
    /// without borrowing the buffer it is then appended to.
    ///
    /// Same reasoning as `out`: the environment is a few hundred bytes and is rebuilt
    /// identically for every request, so holding it across requests turns a per-request
    /// allocation into a one-off. It is also what lets `send` stay `&mut self` — the
    /// encoding writes here, then `append_records` copies it into `out`.
    params: Vec<u8>,
    /// Whether the last request asked for keep-alive.
    keep_conn_requested: bool,
    /// Set once reuse is known to be unsafe.
    closed_by_backend: bool,
    /// Set while a [`ResponseStream`] taken from this client has not been drained.
    ///
    /// A returned stream owns a response the connection is still in the middle of, so `is_reusable`
    /// has to answer `false` until it has been read to `END_REQUEST` — otherwise a pool would hand
    /// the socket to the next request with the previous response's records still in front of it.
    stream_open: bool,
}

impl Client<Stream> {
    /// Connect to a backend.
    pub fn connect(endpoint: &Endpoint, timeout: Option<Duration>) -> Result<Self> {
        Ok(Client::from_stream(endpoint.connect(timeout)?))
    }
}

impl<T> Client<T> {
    /// Whether the connection is still usable.
    ///
    /// **`false` while a streamed response is still being relayed.** The stream owns records this
    /// connection has not read yet, so handing the socket to the next request would prepend the
    /// previous response to it — the worst class of bug a pool can have, because it is
    /// intermittent and looks like the backend's fault.
    pub fn is_reusable(&self) -> bool {
        !self.closed_by_backend && !self.stream_open && self.keep_conn_requested
    }
}

impl<T: Read + Write> Client<T> {
    /// Wrap an already-connected transport, or a test double.
    pub fn from_stream(stream: T) -> Self {
        Client {
            // Sized for the largest FastCGI record a backend can send: a record's content
            // length is a `u16`, so nothing is ever larger than that, and most responses fit
            // in one fill. `BUF_LEN` is kept below the 8 KiB a socket hands over in one go so
            // the buffer covers a fill rather than asking for a second read to top it up.
            stream: BufReader::with_capacity(BUF_LEN, stream),
            // Sized for a typical request so the common case never grows the
            // buffer; a larger one still works, it just reallocates once.
            out: Vec::with_capacity(1024),
            params: Vec::with_capacity(512),
            keep_conn_requested: false,
            closed_by_backend: false,
            stream_open: false,
        }
    }

    /// Send one request and read its response.
    pub fn request(&mut self, request: &Request) -> Result<Response> {
        let started = Instant::now();
        self.keep_conn_requested = request.keep_conn;
        self.send(request)?;
        let mut response = self.receive()?;
        response.duration = started.elapsed();
        if !response.keep_conn {
            self.closed_by_backend = true;
        }
        Ok(response)
    }

    fn send(&mut self, request: &Request) -> Result<()> {
        // Assemble the whole request in `self.out` and write it once. `append_*`
        // mirrors the `write_*` free functions but onto a `Vec`, so the bytes are
        // identical; only the number of `write(2)` calls changes, from eight to one.
        self.out.clear();

        let begin = BeginRequest::responder(request.keep_conn);
        self.out
            .extend_from_slice(&Header::new(RecordType::BeginRequest, REQUEST_ID, 8).encode());
        self.out.extend_from_slice(&begin.encode());

        // Encoded into its own reused buffer, then framed into `out`. Doing it straight
        // into `out` would need `out` borrowed mutably by two calls at once.
        nvpair::encode_pairs_into(
            &mut self.params,
            request
                .params
                .iter()
                .map(|(name, value)| (name.as_slice(), value.as_slice())),
        );
        record::append_records(
            &mut self.out,
            RecordType::Params,
            REQUEST_ID,
            &self.params,
            true,
        );
        record::append_records(
            &mut self.out,
            RecordType::Stdin,
            REQUEST_ID,
            &request.body,
            true,
        );

        // One write, one flush. A failure here is the write phase, which is what
        // `may_retry` needs to know — see `request_staged`. `get_mut` writes around the
        // read buffer rather than through it, so this is still a single call.
        self.stream.get_mut().write_all(&self.out)?;
        self.stream.get_mut().flush()?;
        Ok(())
    }

    fn receive(&mut self) -> Result<Response> {
        let mut response = Response::default();
        loop {
            let Some(record) = record::read_record(&mut self.stream)? else {
                return Err(FcgiError::IncompleteResponse {
                    request_id: REQUEST_ID,
                    complete: true,
                });
            };

            match record.header.record_type {
                RecordType::Stdout => {
                    if !record.content.is_empty() {
                        response.stdout.extend_from_slice(&record.content);
                    }
                }
                RecordType::Stderr => {
                    if !record.content.is_empty() {
                        response.stderr.extend_from_slice(&record.content);
                    }
                }
                RecordType::EndRequest => {
                    response.end = Some(EndRequest::decode(&record.content)?);
                    break;
                }
                // A backend may send a management record or a type we did not
                // ask for; ignore it rather than failing the request.
                _ => continue,
            }
        }

        response.keep_conn = self.keep_conn_requested && !self.closed_by_backend;
        Ok(response)
    }

    /// Send one request and decide, from its header block, whether to buffer or to relay.
    ///
    /// `buffer` is how many bytes of `STDOUT` the caller is willing to hold before the response
    /// stops being buffered. Below it the answer is an ordinary [`Delivery::Buffered`] and nothing
    /// about the request path changes — which matters, because compression, `Content-Length` and the
    /// truncation check all want a whole body. Above it the call returns as soon as the header block
    /// is complete, with the rest of the response still in the pipe.
    ///
    /// # Why the head is a return value and the body is not
    ///
    /// A caller that relays has to decide `Content-Type`, framing and the compression question
    /// *before* the first body byte, and it cannot be handed the whole body to make that decision.
    /// So the head is read eagerly — it is a few hundred bytes by definition — and the body is left
    /// where it is. The alternative, waiting for a threshold to be crossed, means the caller cannot
    /// answer anything until an arbitrary amount of the response has been produced.
    ///
    /// For the same reason a response [`Relay`] does not admit is never relayed even above the
    /// budget: an error page has to be answered as one, and a `500` whose head arrived before its
    /// body is a decision the front-end would have to make blind. The filter is applied here rather
    /// than at the call site so it cannot be forgotten.
    ///
    /// # Draining
    ///
    /// A returned [`ResponseStream`] **must** be drained or dropped, and it is drained by
    /// [`Client::next_chunk`] or [`Client::drain_stream`]. Dropping one closes the connection, which
    /// is the correct end for a client that still has records in the pipe.
    pub fn request_streaming(
        &mut self,
        request: &Request,
        buffer: usize,
        relay: Relay,
    ) -> Result<Delivery> {
        let started = Instant::now();
        self.keep_conn_requested = request.keep_conn;
        self.send(request)?;

        let mut head_block: Vec<u8> = Vec::new();
        let mut stderr: Vec<u8> = Vec::new();
        let mut head_len = 0usize;

        let end = loop {
            let Some(record) = record::read_record(&mut self.stream)? else {
                return Err(FcgiError::IncompleteResponse {
                    request_id: REQUEST_ID,
                    complete: true,
                });
            };

            match record.header.record_type {
                RecordType::Stdout => {
                    if record.content.is_empty() {
                        continue;
                    }
                    head_block.extend_from_slice(&record.content);
                    if head_len == 0 {
                        if let Some((at, separator)) = find_header_end(&head_block) {
                            head_len = at + separator;
                        }
                    }
                    if head_len == 0 {
                        // Still assembling the header block. A block larger than the caller's whole
                        // budget is refused rather than held without limit — the budget exists to
                        // bound exactly this.
                        if head_block.len() > buffer {
                            self.closed_by_backend = true;
                            return Err(FcgiError::BadRequest(format!(
                                "the response header block exceeded the {buffer} byte streaming \
                                 budget before it terminated, so it cannot be parsed or relayed"
                            )));
                        }
                        continue;
                    }

                    let body_so_far = head_block.len() - head_len;
                    if body_so_far > buffer {
                        let (status, headers, _) = parse_response(&head_block);
                        if relay.admits(status, &headers) {
                            let head = head_block[..head_len].to_vec();
                            let rest = head_block.split_off(head_len);
                            self.stream_open = true;
                            return Ok(Delivery::Streamed(ResponseStream {
                                head,
                                pending: (!rest.is_empty()).then_some(rest),
                                finished: false,
                                stderr,
                                end: None,
                                started,
                                head_duration: started.elapsed(),
                                keep_conn_requested: self.keep_conn_requested,
                            }));
                        }
                    }
                }
                RecordType::Stderr => {
                    if !record.content.is_empty() {
                        stderr.extend_from_slice(&record.content);
                    }
                }
                RecordType::EndRequest => {
                    break Some(EndRequest::decode(&record.content)?);
                }
                _ => continue,
            }
        };

        let keep_conn = self.keep_conn_requested && !self.closed_by_backend;
        if !keep_conn {
            self.closed_by_backend = true;
        }
        Ok(Delivery::Buffered(Response {
            stdout: head_block,
            stderr,
            end,
            keep_conn,
            duration: started.elapsed(),
        }))
    }

    /// Read the next piece of a streamed response, ending it at `END_REQUEST`.
    ///
    /// Returns `Ok(None)` once the response is over. Every byte the backend sent is returned exactly
    /// once and in order, including whatever had already been read past the head.
    ///
    /// # This blocks
    ///
    /// For one record — as long as the backend takes to produce the next body chunk. It must be
    /// called from the thread that owns the connection, never from a runtime worker waiting on a
    /// channel that only this call can make progress: that is a deadlock rather than a stall.
    pub fn next_chunk(&mut self, stream: &mut ResponseStream) -> Result<Option<Vec<u8>>> {
        if stream.finished {
            return Ok(None);
        }
        if let Some(pending) = stream.pending.take() {
            if !pending.is_empty() {
                return Ok(Some(pending));
            }
        }

        loop {
            let Some(record) = record::read_record(&mut self.stream)? else {
                // A stream that stops mid-response is never reported as complete: the caller would
                // hand a truncated body to the client as if it were the whole thing.
                stream.finished = false;
                self.closed_by_backend = true;
                return Err(FcgiError::IncompleteResponse {
                    request_id: REQUEST_ID,
                    complete: true,
                });
            };

            match record.header.record_type {
                RecordType::Stdout => {
                    if record.content.is_empty() {
                        continue;
                    }
                    return Ok(Some(record.content));
                }
                RecordType::Stderr => {
                    if !record.content.is_empty() {
                        stream.stderr.extend_from_slice(&record.content);
                    }
                }
                RecordType::EndRequest => {
                    let end = EndRequest::decode(&record.content)?;
                    stream.finished = end.protocol_status.is_complete();
                    stream.end = Some(end);
                    self.stream_open = false;
                    if !stream.finished {
                        self.closed_by_backend = true;
                    }
                    return Ok(None);
                }
                _ => continue,
            }
        }
    }

    /// Read a streamed response to its end, discarding the body.
    ///
    /// What a caller does when the client went away, or when it changed its mind about relaying.
    /// Only after this is the connection reusable — and skipping it *silently* is how a pool fills
    /// with slots nobody can use, which is why `CgiPool` calls this rather than dropping a client it
    /// has already asked for a stream.
    pub fn drain_stream(&mut self, stream: &mut ResponseStream) -> Result<()> {
        while self.next_chunk(stream)?.is_some() {}
        Ok(())
    }

    /// Read backend variables with `GET_VALUES`.
    ///
    /// This is the mechanism `php-fpm` exposes for health and status polling, so
    /// it doubles as the liveness probe for a slot.
    pub fn get_values<'a, I>(&mut self, names: I) -> Result<NameValuePairs>
    where
        I: IntoIterator<Item = &'a [u8]>,
    {
        let payload = nvpair::encode_pairs(names.into_iter().map(|name| (name, &b""[..])));
        record::write_records(
            self.stream.get_mut(),
            RecordType::GetValues,
            MANAGEMENT_REQUEST_ID,
            &payload,
            false,
        )?;

        loop {
            let Some(record) = record::read_record(&mut self.stream)? else {
                return Err(FcgiError::IncompleteResponse {
                    request_id: MANAGEMENT_REQUEST_ID,
                    complete: false,
                });
            };
            match record.header.record_type {
                RecordType::GetValuesResult => return nvpair::decode_pairs(&record.content),
                // An unsupported name is answered with FCGI_UNKNOWN_TYPE, which
                // tells us this backend cannot be probed.
                RecordType::UnknownType => {
                    return Err(FcgiError::UnexpectedRecordType {
                        expected: RecordType::GetValuesResult,
                        actual: RecordType::UnknownType,
                    })
                }
                _ => continue,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// A test double with independent input and output streams.
    ///
    /// A bare `Cursor` cannot be used for both directions: writing would advance
    /// the same position the reads depend on.
    #[derive(Debug, Default)]
    struct Loopback {
        out: Vec<u8>,
        input: Cursor<Vec<u8>>,
    }

    impl Loopback {
        fn replying(reply: Vec<u8>) -> Self {
            Loopback {
                out: Vec::new(),
                input: Cursor::new(reply),
            }
        }

        fn written(&self) -> &[u8] {
            &self.out
        }
    }

    impl Read for Loopback {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.input.read(buf)
        }
    }

    impl Write for Loopback {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            std::io::Write::write(&mut self.out, buf)
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn framed(record_type: RecordType, content: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let header = Header::new(record_type, REQUEST_ID, content.len() as u16);
        header.write(&mut out).unwrap();
        out.extend_from_slice(content);
        out.extend(std::iter::repeat_n(0u8, header.padding_length as usize));
        out
    }

    fn end_record() -> Vec<u8> {
        framed(RecordType::EndRequest, &EndRequest::complete().encode())
    }

    fn client_for(reply: Vec<u8>) -> Client<Loopback> {
        Client::from_stream(Loopback::replying(reply))
    }

    /// A request that only has to be sendable: the reply is what the test is about.
    fn a_request() -> Request {
        Request {
            params: vec![(b"REQUEST_METHOD".to_vec(), b"GET".to_vec())],
            body: Vec::new(),
            keep_conn: true,
        }
    }

    /// Split a payload into `size`-byte `STDOUT` records, the way a backend flushing in chunks does.
    fn stdout_in_chunks(payload: &[u8], size: usize, request_id: u16) -> Vec<u8> {
        let mut out = Vec::new();
        for chunk in payload.chunks(size) {
            let header = Header::new(RecordType::Stdout, request_id, chunk.len() as u16);
            header.write(&mut out).unwrap();
            out.extend_from_slice(chunk);
            out.extend(std::iter::repeat_n(0u8, header.padding_length as usize));
        }
        out
    }

    #[test]
    fn a_response_within_budget_is_delivered_whole() {
        let mut reply = framed(RecordType::Stdout, b"Status: 200 OK\r\n\r\nsmall");
        reply.extend(end_record());

        let mut client = client_for(reply);
        let delivery = client
            .request_streaming(&a_request(), 1024, Relay::above_budget())
            .unwrap();

        let Delivery::Buffered(response) = delivery else {
            panic!("a small response must stay buffered");
        };
        assert_eq!(response.stdout, b"Status: 200 OK\r\n\r\nsmall");
        response.check_protocol().unwrap();
        // Complete, so the connection is reusable exactly as an ordinary request leaves it.
        assert!(client.is_reusable());
    }

    #[test]
    fn a_response_over_budget_returns_its_head_and_relays_the_body_in_order() {
        let head = b"Status: 200 OK\r\nContent-Type: text/csv\r\n\r\n";
        let body: Vec<u8> = (0..900u32)
            .flat_map(|n| format!("{n:04},").into_bytes())
            .collect();
        let mut payload = head.to_vec();
        payload.extend_from_slice(&body);

        let mut reply = stdout_in_chunks(&payload, 300, REQUEST_ID);
        reply.extend(end_record());

        let mut client = client_for(reply);
        // Smaller than the payload, larger than the head: the switch is possible.
        let delivery = client
            .request_streaming(&a_request(), 512, Relay::above_budget())
            .unwrap();

        let Delivery::Streamed(mut stream) = delivery else {
            panic!("expected a streamed delivery");
        };
        assert_eq!(
            stream.head(),
            head,
            "the head is the header block and its own separator, and nothing else"
        );
        let (status, headers, _) = parse_response(stream.head());
        assert_eq!(status, 200);
        assert!(
            headers
                .iter()
                .any(|(name, value)| *name == "Content-Type" && *value == "text/csv"),
            "the head is parseable as a header block: {headers:?}"
        );

        let mut relayed = Vec::new();
        while let Some(chunk) = client.next_chunk(&mut stream).unwrap() {
            relayed.extend_from_slice(&chunk);
        }
        assert_eq!(
            relayed, body,
            "every body byte, in order, and none of the head twice"
        );
        assert!(stream.is_finished(), "the backend reached END_REQUEST");
        assert!(
            client.is_reusable(),
            "a drained stream leaves the connection usable"
        );
    }

    #[test]
    fn an_undrained_stream_leaves_the_connection_unusable() {
        let mut payload = b"Status: 200 OK\r\n\r\n".to_vec();
        payload.extend(std::iter::repeat_n(b'x', 4096));
        let mut reply = stdout_in_chunks(&payload, 512, REQUEST_ID);
        reply.extend(end_record());

        let mut client = client_for(reply);
        let Delivery::Streamed(stream) = client
            .request_streaming(&a_request(), 1024, Relay::above_budget())
            .unwrap()
        else {
            panic!("expected a streamed delivery");
        };

        assert!(!stream.is_finished());
        // Abandoning the body leaves records in the pipe, so the connection must not be reused.
        assert!(!client.is_reusable());
    }

    #[test]
    fn draining_a_stream_discards_the_body_and_frees_the_connection() {
        let mut payload = b"Status: 200 OK\r\n\r\n".to_vec();
        payload.extend(std::iter::repeat_n(b'x', 4096));
        let mut reply = stdout_in_chunks(&payload, 512, REQUEST_ID);
        reply.extend(end_record());

        let mut client = client_for(reply);
        let Delivery::Streamed(mut stream) = client
            .request_streaming(&a_request(), 1024, Relay::above_budget())
            .unwrap()
        else {
            panic!("expected a streamed delivery");
        };
        client.drain_stream(&mut stream).unwrap();
        assert!(stream.is_finished());
        assert!(
            client.is_reusable(),
            "draining to END_REQUEST is what makes the slot usable again"
        );
    }

    #[test]
    fn a_body_arriving_with_the_head_is_relayed_in_full() {
        // One record holding the header block *and* a body larger than the whole budget. php-cgi
        // flushes in 8 KiB pages, so this is the ordinary shape of a large response, not a corner.
        let head = b"Status: 200 OK\r\n\r\n";
        let body: Vec<u8> = std::iter::repeat_n(b'y', 2048).collect();
        let mut payload = head.to_vec();
        payload.extend_from_slice(&body);

        let mut reply = framed(RecordType::Stdout, &payload);
        reply.extend(end_record());

        let mut client = client_for(reply);
        let Delivery::Streamed(mut stream) = client
            .request_streaming(&a_request(), 256, Relay::above_budget())
            .unwrap()
        else {
            panic!("expected a streamed delivery");
        };
        assert_eq!(
            stream.head(),
            head,
            "the body that shared the record is not in the head"
        );

        let mut relayed = Vec::new();
        while let Some(chunk) = client.next_chunk(&mut stream).unwrap() {
            relayed.extend_from_slice(&chunk);
        }
        assert_eq!(relayed, body);
        assert!(stream.is_finished());
    }

    #[test]
    fn a_declared_length_is_left_alone_unless_the_caller_asked_otherwise() {
        // A backend that measured its response is not re-framed: `Content-Length` is information the
        // client can use, and turning it into a chunked stream throws it away.
        let head = b"Status: 200 OK\r\nContent-Length: 9000\r\n\r\n";
        let mut payload = head.to_vec();
        payload.extend(std::iter::repeat_n(b'c', 9000));
        let mut reply = framed(RecordType::Stdout, &payload);
        reply.extend(end_record());

        let mut client = client_for(reply.clone());
        let delivery = client
            .request_streaming(&a_request(), 512, Relay::unknown_length())
            .unwrap();
        assert!(delivery.is_buffered(), "a measured response stays buffered");
        let Delivery::Buffered(response) = delivery else {
            unreachable!()
        };
        assert_eq!(response.stdout.len(), head.len() + 9000);

        let mut client = client_for(reply);
        let delivery = client
            .request_streaming(&a_request(), 512, Relay::above_budget())
            .unwrap();
        assert!(
            !delivery.is_buffered(),
            "the same response is relayed when the caller says size is the only question"
        );
    }

    #[test]
    fn an_error_is_never_relayed_even_over_budget() {
        // A 500 has to reach the front-end's error-page machinery whole. Relaying it would put a
        // status on the wire that the server may still want to rewrite.
        let head = b"Status: 500 Internal Server Error\r\n\r\n";
        let mut payload = head.to_vec();
        payload.extend(std::iter::repeat_n(b'e', 8192));
        let mut reply = framed(RecordType::Stdout, &payload);
        reply.extend(end_record());

        let mut client = client_for(reply);
        let delivery = client
            .request_streaming(&a_request(), 256, Relay::above_budget())
            .unwrap();
        assert!(
            delivery.is_buffered(),
            "a non-2xx response is answered as one"
        );
        let Delivery::Buffered(response) = delivery else {
            unreachable!()
        };
        assert_eq!(response.parse().0, 500);
    }

    #[test]
    fn relay_never_relays_anything() {
        let mut payload = b"Status: 200 OK\r\n\r\n".to_vec();
        payload.extend(std::iter::repeat_n(b'n', 8192));
        let mut reply = framed(RecordType::Stdout, &payload);
        reply.extend(end_record());

        let mut client = client_for(reply);
        let delivery = client
            .request_streaming(&a_request(), 16, Relay::never())
            .unwrap();
        assert!(delivery.is_buffered());
    }

    #[test]
    fn a_streamed_response_keeps_its_stderr() {
        let mut payload = b"Status: 200 OK\r\n\r\n".to_vec();
        payload.extend(std::iter::repeat_n(b'z', 4096));
        let mut reply = framed(RecordType::Stdout, &payload);
        reply.extend(framed(RecordType::Stderr, b"a warning"));
        reply.extend(end_record());

        let mut client = client_for(reply);
        let Delivery::Streamed(mut stream) = client
            .request_streaming(&a_request(), 1024, Relay::above_budget())
            .unwrap()
        else {
            panic!("expected a streamed delivery");
        };
        client.drain_stream(&mut stream).unwrap();
        // STDOUT is relayed, STDERR is not: it is this server's to log, and a caller that dropped it
        // would eventually block the backend on a full pipe.
        assert_eq!(stream.stderr(), b"a warning");
    }

    #[test]
    fn a_header_block_larger_than_the_budget_is_refused_rather_than_buffered_without_limit() {
        // No terminator anywhere, so there is no head to hand over and no body to relay. Buffering
        // it anyway would make the budget a suggestion.
        let payload: Vec<u8> = std::iter::repeat_n(b'h', 4096).collect();
        let mut reply = framed(RecordType::Stdout, &payload);
        reply.extend(end_record());

        let mut client = client_for(reply);
        let err = client
            .request_streaming(&a_request(), 128, Relay::above_budget())
            .unwrap_err();
        assert!(
            err.to_string().contains("header block exceeded"),
            "the refusal has to say what happened: {err}"
        );
    }

    #[test]
    fn an_incomplete_response_is_an_error_not_a_short_body() {
        // STDOUT only, no END_REQUEST: the backend died mid-response. Reporting that as a whole
        // body is how a truncated download reaches a client with a 200.
        let reply = framed(RecordType::Stdout, b"Status: 200 OK\r\n\r\npartial");

        let mut client = client_for(reply);
        let err = client
            .request_streaming(&a_request(), 4096, Relay::above_budget())
            .unwrap_err();
        assert!(matches!(err, FcgiError::IncompleteResponse { .. }), "{err}");
    }

    #[test]
    fn an_incomplete_stream_is_reported_when_the_body_ends_early() {
        let mut payload = b"Status: 200 OK\r\n\r\n".to_vec();
        payload.extend(std::iter::repeat_n(b'x', 4096));
        // No END_REQUEST.
        let reply = framed(RecordType::Stdout, &payload);

        let mut client = client_for(reply);
        let Delivery::Streamed(mut stream) = client
            .request_streaming(&a_request(), 256, Relay::above_budget())
            .unwrap()
        else {
            panic!("expected a streamed delivery");
        };
        let err = client.drain_stream(&mut stream).unwrap_err();
        assert!(matches!(err, FcgiError::IncompleteResponse { .. }), "{err}");
        assert!(!stream.is_finished());
        assert!(!client.is_reusable());
    }

    #[test]
    fn the_streaming_and_buffered_paths_agree_below_the_budget() {
        // The property that makes this safe to adopt: same request, same reply, same bytes.
        let payload = b"Status: 200 OK\r\nX-Test: 1\r\n\r\nhello there";
        let mut reply = framed(RecordType::Stdout, payload);
        reply.extend(end_record());

        let buffered = client_for(reply.clone()).receive().unwrap();
        let mut client = client_for(reply);
        let delivery = client
            .request_streaming(&a_request(), 4096, Relay::above_budget())
            .unwrap();
        let Delivery::Buffered(streamed) = delivery else {
            panic!("expected a buffered delivery");
        };
        assert_eq!(streamed.stdout, buffered.stdout);
        assert_eq!(streamed.stderr, buffered.stderr);
        assert_eq!(streamed.end, buffered.end);
    }

    #[test]
    fn receives_stdout_stderr_and_end() {
        let mut reply = framed(RecordType::Stdout, b"Status: 200 OK\r\n\r\nhello");
        reply.extend(framed(RecordType::Stderr, b"a warning"));
        reply.extend(end_record());

        let mut client = client_for(reply);
        let response = client.receive().unwrap();
        assert_eq!(response.stdout, b"Status: 200 OK\r\n\r\nhello");
        assert_eq!(response.stderr, b"a warning");
        assert_eq!(
            response.protocol_status(),
            Some(ProtocolStatus::RequestComplete)
        );
        response.check_protocol().unwrap();
    }

    #[test]
    fn empty_stream_records_are_ignored_but_end_is_required() {
        let mut reply = framed(RecordType::Stdout, b"body");
        reply.extend(framed(RecordType::Stdout, b""));
        assert!(client_for(reply).receive().is_err(), "missing END_REQUEST");

        let mut reply = framed(RecordType::Stdout, b"");
        reply.extend(end_record());
        let response = client_for(reply).receive().unwrap();
        assert!(response.stdout.is_empty());
        response.check_protocol().unwrap();
    }

    #[test]
    fn a_truncated_response_reports_incomplete_rather_than_hanging() {
        let reply = framed(RecordType::Stdout, b"partial");
        let err = client_for(reply).receive().unwrap_err();
        assert!(
            matches!(err, FcgiError::IncompleteResponse { complete: true, .. }),
            "{err}"
        );
    }

    #[test]
    fn an_overloaded_backend_surfaces_as_a_protocol_error() {
        let mut reply = framed(RecordType::Stdout, b"");
        reply.extend(framed(
            RecordType::EndRequest,
            &EndRequest {
                app_status: 0,
                protocol_status: ProtocolStatus::Overloaded,
            }
            .encode(),
        ));

        let response = client_for(reply).receive().unwrap();
        let err = response.check_protocol().unwrap_err();
        assert!(err.to_string().contains("Overloaded"), "{err}");
    }

    #[test]
    fn a_non_zero_application_status_is_not_a_protocol_failure() {
        let mut reply = framed(RecordType::Stdout, b"Status: 500\r\n\r\nboom");
        reply.extend(framed(
            RecordType::EndRequest,
            &EndRequest {
                app_status: 255,
                protocol_status: ProtocolStatus::RequestComplete,
            }
            .encode(),
        ));

        let response = client_for(reply).receive().unwrap();
        response.check_protocol().unwrap();
        assert_eq!(response.app_status(), Some(255));
        assert_eq!(response.parse_status_headers().0, 500);
    }

    #[test]
    fn sends_begin_params_and_terminated_stdin_records() {
        let mut reply = framed(RecordType::Stdout, b"ok");
        reply.extend(end_record());

        let mut client = client_for(reply);
        let request = Request {
            params: vec![(b"REQUEST_METHOD".to_vec(), b"GET".to_vec())],
            body: b"payload".to_vec(),
            keep_conn: true,
        };
        let response = client.request(&request).unwrap();
        assert_eq!(response.stdout, b"ok");
        assert!(response.keep_conn, "keep-alive requested and not refused");
        assert!(client.is_reusable());

        // Decode exactly what the client wrote.
        let written = client.stream.get_ref().written().to_vec();
        let mut cursor = written.as_slice();
        let begin = record::read_record(&mut cursor).unwrap().unwrap();
        assert_eq!(begin.record_type(), RecordType::BeginRequest);
        assert_eq!(begin.request_id(), REQUEST_ID);
        assert!(BeginRequest::decode(&begin.content).unwrap().keep_conn);

        let params = record::read_record(&mut cursor).unwrap().unwrap();
        assert_eq!(params.record_type(), RecordType::Params);
        let pairs = nvpair::decode_pairs(&params.content).unwrap();
        assert_eq!(pairs, vec![(b"REQUEST_METHOD".to_vec(), b"GET".to_vec())]);

        // The PARAMS stream terminator, then the body and its terminator.
        assert!(record::read_record(&mut cursor)
            .unwrap()
            .unwrap()
            .content
            .is_empty());
        let stdin = record::read_record(&mut cursor).unwrap().unwrap();
        assert_eq!(stdin.record_type(), RecordType::Stdin);
        assert_eq!(stdin.content, b"payload");
        assert!(record::read_record(&mut cursor)
            .unwrap()
            .unwrap()
            .content
            .is_empty());
    }

    #[test]
    fn a_request_without_keep_conn_marks_the_connection_unusable() {
        let mut reply = framed(RecordType::Stdout, b"ok");
        reply.extend(end_record());

        let mut client = client_for(reply);
        let request = Request {
            keep_conn: false,
            ..Request::default()
        };
        let response = client.request(&request).unwrap();
        assert!(!response.keep_conn);
        assert!(!client.is_reusable(), "must not be reused after a close");
    }

    #[test]
    fn parses_status_and_headers() {
        let stdout = b"Status: 404 Not Found\r\nContent-Type: text/html\r\nX-Test: 1\r\n\r\nbody";
        let (status, headers) = parse_response_headers(stdout);
        assert_eq!(status, 404);
        assert_eq!(headers.len(), 2);
        assert_eq!(headers[0], ("Content-Type", "text/html"));
        assert_eq!(headers[1], ("X-Test", "1"));
    }

    #[test]
    fn a_response_without_a_status_header_is_200() {
        let (status, headers) = parse_response_headers(b"Content-Type: text/plain\r\n\r\nhi");
        assert_eq!(status, 200);
        assert_eq!(headers.len(), 1);
    }

    /// A bare `Location` is a redirect, which is RFC 3875 §6.2.3's rule and nginx's.
    ///
    /// The case a PHP-only deployment never produced: frameworks send `Status`, so `200` was
    /// the right answer for every backend volucer had. A WSGI or Rack application that sets
    /// `Location` and lets the server choose would have been answered `200` here, with a
    /// header no client follows.
    #[test]
    fn a_location_without_a_status_is_a_found() {
        let (status, headers) = parse_response_headers(b"Location: /login\r\n\r\n");
        assert_eq!(status, 302);
        assert_eq!(headers, vec![("Location", "/login")]);
    }

    /// And an explicit status wins, because the application has then answered the question.
    #[test]
    fn an_explicit_status_beside_a_location_is_kept() {
        let (status, _) =
            parse_response_headers(b"Status: 201 Created\r\nLocation: /items/7\r\n\r\n");
        assert_eq!(status, 201);
    }

    /// A `Location` that is not in the header block at all leaves the default alone.
    ///
    /// Pins the *scope* of the rule: it is the header block the parser found, not a substring
    /// search of the payload, so an application whose body happens to contain the word
    /// `Location:` is not turned into a redirect.
    #[test]
    fn a_location_in_the_body_is_not_a_header() {
        let (status, headers) =
            parse_response_headers(b"Content-Type: text/plain\r\n\r\nLocation: /x");
        assert_eq!(status, 200);
        assert!(!headers.iter().any(|(name, _)| *name == "Location"));
    }

    #[test]
    fn parses_headers_terminated_by_a_bare_line_feed() {
        let (status, headers) = parse_response_headers(b"Status: 201 Created\n\nbody");
        assert_eq!(status, 201);
        assert!(headers.is_empty());
    }

    #[test]
    fn headers_only_apply_when_status_is_first() {
        // A `Status:` header that is not the first line is an ordinary header.
        let (status, headers) =
            parse_response_headers(b"Content-Type: text/plain\r\nStatus: 404\r\n\r\n");
        assert_eq!(status, 200);
        assert_eq!(headers.len(), 2);
    }

    #[test]
    fn a_malformed_status_line_falls_back_to_200() {
        let (status, _) = parse_response_headers(b"Status: nonsense\r\n\r\n");
        assert_eq!(status, 200);
    }

    #[test]
    fn a_response_without_a_blank_line_is_still_parsed() {
        let (status, headers) = parse_response_headers(b"Status: 204 No Content\r\nX-A: b");
        assert_eq!(status, 204);
        assert_eq!(headers.len(), 1);
    }

    #[test]
    fn the_body_is_split_off_with_either_separator() {
        assert_eq!(
            split_response_body(b"Status: 200\r\nX-A: b\r\n\r\nbody"),
            b"body"
        );
        assert_eq!(split_response_body(b"Status: 200\nX-A: b\n\nbody"), b"body");
        // Headers with an empty body, which is not the same case.
        assert_eq!(split_response_body(b"Status: 204\r\n\r\n"), b"");
        // Neither separator: the payload is all header block, so no body.
        assert_eq!(split_response_body(b"Status: 200\r\nX-A: b"), b"");
        assert_eq!(split_response_body(b""), b"");
    }

    #[test]
    fn the_single_pass_parser_agrees_with_the_two_entry_points() {
        // `Response::parse` scans once and returns all three, and the two older
        // entry points delegate to it. This pins them to each other so a future
        // change to one cannot silently diverge from the others - which is the
        // risk the refactor introduced when three functions became one.
        for stdout in [
            &b"Status: 200 OK\r\nContent-Type: text/html\r\n\r\nbody"[..],
            b"Status: 404\r\n\r\n",
            b"Content-Type: text/plain\r\n\r\nhi",
            b"Status: 204\nX-A: b\n\nbody",
            b"",
        ] {
            let response = Response {
                stdout: stdout.to_vec(),
                ..Response::default()
            };
            let (status, headers) = response.parse_status_headers();
            let (parse_status, parse_headers, body) = response.parse();

            assert_eq!(parse_status, status, "status disagreed for {stdout:?}");
            assert_eq!(parse_headers, headers, "headers disagreed for {stdout:?}");
            assert_eq!(
                body,
                split_response_body(stdout),
                "body disagreed for {stdout:?}"
            );
        }
    }

    #[test]
    fn a_body_that_is_not_utf8_does_not_disturb_the_headers() {
        // The header block is ASCII and the body is arbitrary bytes. The previous
        // implementation ran the whole payload through `from_utf8_lossy`, which
        // replaced these bytes with U+FFFD inside a `String` whose *length* then
        // differed from the slice's - and the body offset was computed from that
        // String. Both halves have to survive a body like this.
        let mut stdout =
            b"Status: 201 Created\r\nContent-Type: application/octet-stream\r\n\r\n".to_vec();
        let body: Vec<u8> = vec![0xFF, 0xFE, 0x00, 0x80, 0xC3, 0x28, 0x0D, 0x0A, 0x0D, 0x0A];
        stdout.extend_from_slice(&body);

        let (status, headers) = parse_response_headers(&stdout);
        assert_eq!(status, 201);
        assert_eq!(headers.len(), 1);
        assert_eq!(headers[0].0, "Content-Type");
        // Byte-for-byte, including a `\r\n\r\n` *inside* the body, which must not
        // be mistaken for a header terminator.
        assert_eq!(split_response_body(&stdout), &body[..]);
    }

    #[test]
    fn a_header_that_is_not_ascii_is_skipped_rather_than_mangled() {
        // A non-ASCII header name cannot be sent as an HTTP header, and the lossy
        // path used to hand the mangled version on. Skipping is the honest
        // outcome and keeps the parser free of the conversion.
        let (status, headers) =
            parse_response_headers(b"Status: 200\r\nX-\xFF: v\r\nX-A: b\r\n\r\n");
        assert_eq!(status, 200);
        assert_eq!(headers, vec![("X-A", "b")]);
    }

    #[test]
    fn the_status_code_is_read_from_the_value_however_it_is_spaced() {
        // The reason phrase is discarded, and both a tab and a run of spaces
        // separate it from the code the way `split_whitespace` did.
        assert_eq!(parse_response_headers(b"Status: 404\r\n\r\n").0, 404);
        assert_eq!(
            parse_response_headers(b"Status: 404 Not Found\r\n\r\n").0,
            404
        );
        assert_eq!(parse_response_headers(b"Status:\t500\tboom\r\n\r\n").0, 500);
        assert_eq!(parse_response_headers(b"Status:   201   \r\n\r\n").0, 201);
        assert_eq!(parse_response_headers(b"Status:404\r\n\r\n").0, 404);
    }

    #[test]
    fn a_blank_line_before_the_status_line_does_not_hide_it() {
        // A header block that opens with a line break is legal, and the status
        // line is then the *first non-empty* line. Counting loop iterations
        // instead made this `index == 1` and the `404` was silently dropped.
        let stdout = b"\r\nStatus: 404 Not Found\r\nX-A: b\r\n\r\nbody";
        let (status, headers) = parse_response_headers(stdout);
        assert_eq!(status, 404);
        assert_eq!(headers, vec![("X-A", "b")]);
        // And the same with a bare `\n`, which `find_header_end` also accepts.
        let stdout = b"\nStatus: 500\r\n\r\n";
        assert_eq!(parse_response_headers(stdout).0, 500);
        // Empty lines anywhere else are skipped just as before.
        let (_, headers) = parse_response_headers(b"X-A: b\r\n\r\nX-B: c\r\n\r\n");
        assert_eq!(headers, vec![("X-A", "b")]);
    }

    #[test]
    fn a_status_line_with_a_space_before_the_colon_is_read_like_any_other_header() {
        // HTTP forbids the space, and nothing else in this parser checks, so the rule
        // is simply that a header name is trimmed: `Status : 404` therefore names the
        // status header, and every ordinary header is trimmed the same way. What this
        // pins is the *consistency*: the name used to be compared for the status
        // untrimmed but pushed into the header list trimmed, so one line was read as
        // both `200` and a `Status` header at once.
        let (status, headers) = parse_response_headers(b"Status : 404\r\n\r\n");
        assert_eq!(status, 404);
        assert!(headers.is_empty(), "{headers:?}");

        let stdout = b"X-A : b\r\n\r\n";
        assert_eq!(parse_response_headers(stdout).1, vec![("X-A", "b")]);
    }

    #[test]
    fn an_owned_copy_of_the_headers_can_still_be_made() {
        // The worker protocol carries headers as JSON, so it needs owned `String`s
        // and converts at that boundary. This is that conversion, and it is the only
        // place an allocation is still unavoidable.
        let (_, headers) = parse_response_headers(b"Content-Type: text/html\r\nX-A: b\r\n\r\n");
        let owned: Vec<(String, String)> = headers
            .into_iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect();
        assert_eq!(
            owned,
            vec![
                ("Content-Type".to_string(), "text/html".to_string()),
                ("X-A".to_string(), "b".to_string())
            ]
        );
    }

    #[test]
    fn get_values_decodes_a_result_record() {
        let payload = nvpair::encode_pairs([
            (&b"max_connections"[..], &b"10"[..]),
            (&b"total_processes"[..], &b"3"[..]),
        ]);
        let mut reply = Vec::new();
        let header = Header::new(
            RecordType::GetValuesResult,
            MANAGEMENT_REQUEST_ID,
            payload.len() as u16,
        );
        header.write(&mut reply).unwrap();
        reply.extend_from_slice(&payload);

        let mut client = client_for(reply);
        let values = client.get_values([&b"max_connections"[..]]).unwrap();
        assert_eq!(values[0].0, b"max_connections");
        assert_eq!(values[0].1, b"10");

        let written = client.stream.get_ref().written().to_vec();
        let record = record::read_record(&mut written.as_slice())
            .unwrap()
            .unwrap();
        assert_eq!(record.record_type(), RecordType::GetValues);
        assert_eq!(record.request_id(), MANAGEMENT_REQUEST_ID);
    }

    #[test]
    fn get_values_reports_an_unsupported_backend() {
        let mut reply = Vec::new();
        let header = Header::new(RecordType::UnknownType, MANAGEMENT_REQUEST_ID, 1);
        header.write(&mut reply).unwrap();
        reply.push(RecordType::GetValues.as_u8());

        let mut client = client_for(reply);
        assert!(client.get_values([&b"anything"[..]]).is_err());
    }

    #[test]
    fn request_ids_are_constant_with_no_multiplexing() {
        assert_eq!(REQUEST_ID, 1);
        assert_eq!(MANAGEMENT_REQUEST_ID, 0);
    }
}
