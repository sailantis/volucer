//! A minimal FastCGI *server*, for talking to nginx.
//!
//! The codec in `volucer-fcgi` is symmetric, so this reuses the same records
//! and helpers the client uses. What is new here is the request lifecycle from
//! the server's side: reading `PARAMS` and `STDIN` until their terminators,
//! ignoring `ABORT_REQUEST`, and writing `STDOUT`, `STDERR` and `END_REQUEST`.
//!
//! Two properties are deliberate:
//!
//! - **Nothing is parsed that can be relayed.** A request becomes a
//!   `volucer_fcgi::Request` (already-decoded name-value pairs plus a body)
//!   and a response goes back as `STDOUT` bytes, so the front controller's
//!   headers are never re-serialised.
//! - **`STDERR` is forwarded rather than logged away.** nginx shows it in the
//!   error log, which is where a developer looks first when PHP complains.

use std::io::{Read, Write};
use std::time::{Duration, Instant};

use volucer_fcgi::{
    message::{BeginRequest, EndRequest},
    nvpair,
    protocol::{ProtocolStatus, RecordType},
    record, Request, Response,
};

use crate::error::{Result, ServerError};

/// What the front-end learned about a request.
#[derive(Debug, Clone)]
pub struct IncomingRequest {
    /// The CGI environment, verbatim from `PARAMS`.
    pub params: Vec<(Vec<u8>, Vec<u8>)>,
    /// The request body, verbatim from `STDIN`.
    pub body: Vec<u8>,
    /// Whether the client asked for keep-alive.
    pub keep_conn: bool,
    /// Whether the client aborted the request in flight.
    pub aborted: bool,
}

impl IncomingRequest {
    /// Look up a `PARAMS` value by name.
    ///
    /// Names are compared exactly, because that is how they arrive on the wire.
    pub fn param(&self, name: &str) -> Option<&[u8]> {
        self.params
            .iter()
            .find(|(key, _)| key.as_slice() == name.as_bytes())
            .map(|(_, value)| value.as_slice())
    }

    /// A `PARAMS` value as a lossy UTF-8 string.
    pub fn param_str(&self, name: &str) -> Option<String> {
        self.param(name)
            .map(|value| String::from_utf8_lossy(value).into_owned())
    }

    /// Convert into what a backend client expects.
    pub fn into_backend_request(self) -> Request {
        Request {
            params: self.params,
            body: self.body,
            keep_conn: self.keep_conn,
        }
    }
}

/// How a connection ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionOutcome {
    /// The peer closed cleanly between requests.
    Closed,
    /// The peer asked to keep the connection open and may send another request.
    KeepAlive,
    /// The peer aborted the request in flight.
    Aborted,
}

/// What arrived on a connection.
#[derive(Debug, Clone)]
pub enum NextRequest {
    /// A real request to serve.
    Request(IncomingRequest),
    /// A `GET_VALUES` probe. nginx uses it to decide whether this server is
    /// usable, so answering it keeps volucer in rotation instead of marked
    /// down.
    GetValues(Vec<(Vec<u8>, Vec<u8>)>),
}

/// Read one request, or a management probe, from a connection.
///
/// Returns `Ok(None)` for a clean end of stream, which is how nginx closes an
/// idle keep-alive connection.
///
/// **`max_body_bytes` is a parameter rather than a constant because it is configuration**, and
/// because there is no second place to apply it: the body arrives as a sequence of `STDIN`
/// records that are only complete once their terminator is, so anything buffering the request has
/// to bound it as the records arrive. That is the same rule the HTTP front-end follows in
/// `read_body`, and the two must agree — a limit that only one protocol enforced would be a key
/// that reads as a setting and is not one.
///
/// An oversized body is [`ServerError::BodyTooLarge`], not a protocol error: the request was
/// well-formed and refused, and the caller has to tell that apart to answer `413` rather than
/// `502`.
pub fn read_next<R: Read>(reader: &mut R, max_body_bytes: usize) -> Result<Option<NextRequest>> {
    let begin = loop {
        let Some(record) = record::read_record(reader)? else {
            return Ok(None);
        };
        match record.record_type() {
            RecordType::BeginRequest => break record,
            RecordType::GetValues => {
                let names = nvpair::decode_pairs(&record.content).map_err(|source| {
                    ServerError::BadProtocol {
                        reason: format!("malformed GET_VALUES: {source}"),
                    }
                })?;
                return Ok(Some(NextRequest::GetValues(names)));
            }
            // Anything else before a BEGIN_REQUEST is unexpected, but skipping is
            // kinder than tearing the connection down.
            other => {
                tracing::debug!(record = ?other, "ignoring a record before BEGIN_REQUEST");
                continue;
            }
        }
    };

    let begin_request =
        BeginRequest::decode(&begin.content).map_err(|source| ServerError::BadProtocol {
            reason: format!("malformed BEGIN_REQUEST: {source}"),
        })?;

    let mut params = Vec::new();
    let mut body = Vec::new();

    loop {
        let Some(record) = record::read_record(reader)? else {
            return Err(ServerError::BadProtocol {
                reason: "the connection closed mid-request".to_string(),
            });
        };

        match record.record_type() {
            RecordType::Params => {
                if record.content.is_empty() {
                    continue;
                }
                params.extend(nvpair::decode_pairs(&record.content).map_err(|source| {
                    ServerError::BadProtocol {
                        reason: format!("malformed PARAMS: {source}"),
                    }
                })?);
            }
            RecordType::Stdin => {
                if record.content.is_empty() {
                    // The zero-length record terminates the body, which with the
                    // params terminator already seen ends the request.
                    break;
                }
                // **Checked before the extend, not after.** Checking afterwards would let one
                // record over the limit be buffered and then refused, so the memory a client can
                // make this process hold is `max_body_bytes` plus one record (64 KiB) rather than
                // `max_body_bytes`. The same ordering rule applies to the counter-refusal in
                // `volucer-metrics`: a limit enforced after the work it exists to prevent is not a
                // limit.
                let total = body.len().saturating_add(record.content.len());
                if total > max_body_bytes {
                    return Err(ServerError::BodyTooLarge {
                        limit: max_body_bytes,
                    });
                }
                body.extend_from_slice(&record.content);
            }
            RecordType::AbortRequest => {
                // There is no partial response to abandon, so an abort is simply
                // a request that will not be served.
                return Ok(Some(NextRequest::Request(IncomingRequest {
                    params,
                    body,
                    keep_conn: false,
                    aborted: true,
                })));
            }
            other => {
                tracing::debug!(record = ?other, "ignoring an unexpected record");
                continue;
            }
        }
    }

    let _ = begin_request.role;
    Ok(Some(NextRequest::Request(IncomingRequest {
        params,
        body,
        keep_conn: begin_request.keep_conn,
        aborted: false,
    })))
}

/// Read one request, treating a management probe as an error.
///
/// Kept for callers that only serve requests; the listener uses [`read_next`] so
/// it can answer probes.
///
/// Carries the limit through for the same reason [`read_next`] takes it: a caller reading a
/// request has to have an answer for how large one may be, and defaulting here would put a
/// second, invisible limit in the codebase.
pub fn read_request<R: Read>(
    reader: &mut R,
    max_body_bytes: usize,
) -> Result<Option<IncomingRequest>> {
    match read_next(reader, max_body_bytes)? {
        None => Ok(None),
        Some(NextRequest::Request(request)) => Ok(Some(request)),
        Some(NextRequest::GetValues(_)) => Err(ServerError::UnsupportedManagementRecord),
    }
}

/// Write a backend response back to the FastCGI client.
///
/// `stdout` is relayed verbatim: it is already `Status:`-header + headers +
/// body, which is exactly what the client expects.
pub fn write_response<W: Write>(
    writer: &mut W,
    request_id: u16,
    stdout: &[u8],
    stderr: &[u8],
    keep_conn: bool,
) -> Result<()> {
    // Built into this thread's scratch buffer and written once, rather than record field by
    // record field. The listener's socket is deliberately unbuffered for writes (`get_mut()`
    // past the read buffer), so without this a response with no `STDERR` costs five `write(2)`s
    // — STDOUT's header, its payload, END_REQUEST's header, its payload, and the flush.
    //
    // The bytes are identical either way: `append_records` is the tested twin of
    // `write_records` (`append_and_write_produce_the_same_bytes`), and the END_REQUEST pair
    // below is the same header-then-payload with the same 8-byte content and so the same zero
    // padding.
    let end = EndRequest::complete();

    write_assembled(writer, |out| {
        record::append_records(out, RecordType::Stdout, request_id, stdout, true);
        if !stderr.is_empty() {
            record::append_records(out, RecordType::Stderr, request_id, stderr, true);
        }
        // The application status is 0 because the backend already reported any
        // application-level outcome inside its response; the client sees that as the
        // `Status:` line.
        out.extend_from_slice(&record::Header::new(RecordType::EndRequest, request_id, 8).encode());
        out.extend_from_slice(&end.encode());
    })?;

    let _ = keep_conn;
    Ok(())
}

/// Build a whole response in a reused per-thread buffer and write it once.
///
/// Every response writer goes through this, because the listener's socket is deliberately
/// unbuffered for writes: it is a `BufReader` whose writes go through `get_mut()`, so each
/// individual `write_all` on it is its own `write(2)`, and on a socket with `TCP_NODELAY`
/// set each is its own segment too. Writing a response record field by record field cost
/// five syscalls; assembling it costs one.
///
/// **The buffer is per thread, not shared.** It never outlives the closure — nothing is
/// returned from it — so no concurrent writer can observe another's bytes, and there is no
/// lock anywhere on the response path. A `BufWriter` around the socket would have been the
/// other option and is the one `volucer-fcgi`'s client takes; here the response has a known
/// end, so assembling it whole is simpler than reasoning about where a flush must happen.
///
/// It is reused rather than allocated per response because a response is body-sized: a 12 KB
/// page would otherwise be a 12 KB allocation on every request, which is the cost this is
/// meant to avoid.
fn write_assembled<W, F>(writer: &mut W, build: F) -> Result<()>
where
    W: Write,
    F: FnOnce(&mut Vec<u8>),
{
    thread_local! {
        static BUFFER: std::cell::RefCell<Vec<u8>> = const {
            std::cell::RefCell::new(Vec::new())
        };
    }

    BUFFER.with(|buffer| {
        let mut out = buffer.borrow_mut();
        out.clear();
        build(&mut out);
        writer.write_all(&out)?;
        // One flush: a `BufWriter` puts its buffer on the wire, an unwrapped socket has
        // nothing to flush. `write_frame`/`write_records` flush for the same reason.
        writer.flush()?;
        Ok::<(), ServerError>(())
    })?;
    Ok(())
}

/// Write an error that happened before the backend produced a response.
///
/// nginx surfaces this as a 502, which is the correct outcome: the gateway is
/// what failed. **Except for a body that was too large**, which is a 413 — the reason decides the
/// status line, because the `Status:` line is how this front-end says anything to the client and
/// blaming the gateway for a limit the client exceeded would send an operator to look at PHP.
///
/// A **rate-limited** refusal also carries a `Retry-After`, which no other kind can: only this one
/// has a number to give the client. It goes through [`write_rate_limited`] so that the header and
/// the status line cannot be written by two callers that disagree.
pub fn write_error<W: Write>(
    writer: &mut W,
    request_id: u16,
    message: &str,
    reason: ServerErrorKind,
) -> Result<()> {
    write_error_with(writer, request_id, message, reason, None)
}

/// A refusal that tells the client when to come back.
///
/// Separate from [`write_error`] rather than an argument on it, because only one kind of refusal has
/// an answer to "when": passing `None` from the five callers that do not would be a parameter every
/// future caller has to think about for a case only this one reaches.
pub fn write_rate_limited<W: Write>(
    writer: &mut W,
    request_id: u16,
    message: &str,
    retry_after_secs: u64,
) -> Result<()> {
    write_error_with(
        writer,
        request_id,
        message,
        ServerErrorKind::RateLimited,
        Some(retry_after_secs.max(1)),
    )
}

/// A refusal by `[server.access]`.
///
/// Its own function rather than a `write_error(…, ServerErrorKind::AddressDenied)` call, so the
/// reason cannot be passed with the wrong kind: the status line and the `no-store` both follow from
/// this one argument, and a caller that picked `Backend` by mistake would answer a 502 for a
/// deliberate refusal.
pub fn write_forbidden<W: Write>(writer: &mut W, request_id: u16, message: &str) -> Result<()> {
    write_error_with(
        writer,
        request_id,
        message,
        ServerErrorKind::AddressDenied,
        None,
    )
}

fn write_error_with<W: Write>(
    writer: &mut W,
    request_id: u16,
    message: &str,
    reason: ServerErrorKind,
    retry_after_secs: Option<u64>,
) -> Result<()> {
    // `no-store` on both of the refuse-with-a-status kinds, and it is not decoration: a shared
    // cache that stored a 429 or a 413 would serve it to every other client of the same URL, which
    // turns one caller's problem into everybody's. RoadRunner sets it for the same reason.
    let extra = match (&reason, retry_after_secs) {
        (ServerErrorKind::RateLimited, Some(seconds)) => {
            format!("Cache-Control: no-store\r\nRetry-After: {seconds}\r\n")
        }
        (ServerErrorKind::BodyTooLarge | ServerErrorKind::RateLimited | ServerErrorKind::AddressDenied, _) => {
            "Cache-Control: no-store\r\n".to_string()
        }
        _ => String::new(),
    };
    let body = format!(
        "Status: {}\r\nContent-Type: text/plain; charset=utf-8\r\n{extra}\r\nvolucer: {message}\r\n",
        reason.status_line()
    );

    // A failed request also gets one assembled write, and its END_REQUEST carries the
    // *reason's* protocol status rather than `RequestComplete` — that is what tells nginx
    // this was a gateway failure. The application status stays 0 for the same reason as
    // above: the failure is expressed in the `Status:` line of the body.
    let end = EndRequest {
        app_status: 0,
        protocol_status: reason.protocol_status(),
    };
    write_assembled(writer, |out| {
        record::append_records(out, RecordType::Stdout, request_id, body.as_bytes(), true);
        record::append_records(
            out,
            RecordType::Stderr,
            request_id,
            message.as_bytes(),
            true,
        );
        out.extend_from_slice(&record::Header::new(RecordType::EndRequest, request_id, 8).encode());
        out.extend_from_slice(&end.encode());
    })?;
    Ok(())
}

/// Why a request could not be served, for the `END_REQUEST` status.
#[derive(Debug, Clone, Copy)]
pub enum ServerErrorKind {
    /// The backend failed; a 502 is the right answer.
    Backend,
    /// Too many concurrent requests for this server.
    Overloaded,
    /// Multiplexed connections are not supported.
    NoMultiplexing,
    /// The body was over `server.max_body_bytes`.
    BodyTooLarge,
    /// The caller has spent its rate-limit allowance.
    ///
    /// Its own kind rather than a reuse of [`ServerErrorKind::Overloaded`], because the two must not
    /// share a status line: `Overloaded` is a 503 — "this server has no capacity" — while a rate
    /// limit is a 429, which is a statement about the caller. nginx relays whichever `Status:` line
    /// arrives, so the difference reaches the client.
    ///
    /// The protocol status stays `RequestComplete` for the same reason [`ServerErrorKind::BodyTooLarge`]'s
    /// does: the request was read in full and refused by policy, so nothing about the *connection*
    /// went wrong. `Overloaded` would tell nginx to retry elsewhere, and a rate-limited caller is
    /// rate-limited at every worker.
    RateLimited,
    /// The caller's address is refused by `[server.access]`.
    ///
    /// A 403 and its own kind for the same reason [`ServerErrorKind::RateLimited`] is its own: nginx
    /// relays the `Status:` line, so a refusal by policy has to reach the client as the status the
    /// policy means. Reporting it as the backend failure beside it would send an operator to look at
    /// PHP for a decision this server made.
    AddressDenied,
}

impl ServerErrorKind {
    fn protocol_status(self) -> ProtocolStatus {
        match self {
            ServerErrorKind::Backend => ProtocolStatus::RequestComplete,
            ServerErrorKind::Overloaded => ProtocolStatus::Overloaded,
            ServerErrorKind::NoMultiplexing => ProtocolStatus::CantMpxConn,
            // **`RequestComplete`, deliberately.** The request was read to its terminator and
            // refused by policy, so nothing about the *connection* went wrong — the same reason a
            // 404 is `RequestComplete`. `Overloaded` would tell nginx to try elsewhere, and an
            // oversized body is oversized at every worker.
            ServerErrorKind::BodyTooLarge => ProtocolStatus::RequestComplete,
            ServerErrorKind::RateLimited => ProtocolStatus::RequestComplete,
            ServerErrorKind::AddressDenied => ProtocolStatus::RequestComplete,
        }
    }

    /// The `Status:` line this failure answers with.
    ///
    /// A `&'static str` rather than the code, because the whole string is written verbatim into
    /// the CGI response and a caller assembling it from parts would be a second place a status
    /// line could be wrong.
    fn status_line(self) -> &'static str {
        match self {
            // **413, not 502.** The gateway did not fail; the client sent a body over the limit,
            // and `413 Content Too Large` is the answer nginx itself gives for the same reason.
            ServerErrorKind::BodyTooLarge => "413 Content Too Large",
            ServerErrorKind::RateLimited => "429 Too Many Requests",
            ServerErrorKind::AddressDenied => "403 Forbidden",
            _ => "502 Bad Gateway",
        }
    }
}

/// Answer a `GET_VALUES` probe.
///
/// nginx uses this to decide whether a server is usable, so answering it keeps
/// volucer in rotation instead of being marked down.
pub fn write_get_values<W: Write>(writer: &mut W, values: &[(&str, &str)]) -> Result<()> {
    let payload = nvpair::encode_pairs(
        values
            .iter()
            .map(|(name, value)| (name.as_bytes(), value.as_bytes())),
    );
    record::write_records(
        writer,
        RecordType::GetValuesResult,
        volucer_fcgi::MANAGEMENT_REQUEST_ID,
        &payload,
        false,
    )?;
    writer.flush()?;
    Ok(())
}

/// The names a `GET_VALUES` probe asked about.
pub type ValueNames = Vec<(Vec<u8>, Vec<u8>)>;

/// Read a management record if one is pending, without consuming a request.
///
/// Kept separate from [`read_request`] because a probe is not a request and must
/// not be answered with an `END_REQUEST`.
pub fn read_management_request<R: Read>(reader: &mut R) -> Result<Option<ValueNames>> {
    let Some(record) = record::read_record(reader)? else {
        return Ok(None);
    };
    if record.record_type() != RecordType::GetValues {
        return Err(ServerError::BadProtocol {
            reason: format!(
                "expected a GET_VALUES record, received {:?}",
                record.record_type()
            ),
        });
    }
    Ok(Some(nvpair::decode_pairs(&record.content).map_err(
        |source| ServerError::BadProtocol {
            reason: format!("malformed GET_VALUES: {source}"),
        },
    )?))
}

/// How long a request took, for logging.
#[derive(Debug)]
pub struct Timing {
    pub started: Instant,
}

impl Timing {
    pub fn start() -> Self {
        Timing {
            started: Instant::now(),
        }
    }

    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }
}

/// Turn a backend response into the parts the front-end needs.
pub fn split_response(response: &Response) -> (&[u8], &[u8], u16) {
    (&response.stdout, &response.stderr, volucer_fcgi::REQUEST_ID)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The limit the request-reading tests pass.
    ///
    /// A named constant rather than a literal at each call site so the tests that assert a
    /// *successful* read are visibly separate from the ones that assert a refusal — a test that
    /// passes `LIMIT` and expects data is testing the default, and a test that expects an error
    /// has to say which limit it exceeded.
    const LIMIT: usize = volucer_core::config::DEFAULT_MAX_BODY_BYTES;

    fn framed(record_type: RecordType, request_id: u16, content: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let header = record::Header::new(record_type, request_id, content.len() as u16);
        header.write(&mut out).unwrap();
        out.extend_from_slice(content);
        out.extend(std::iter::repeat_n(0u8, header.padding_length as usize));
        out
    }

    fn begin(keep_conn: bool) -> Vec<u8> {
        let request = BeginRequest::responder(keep_conn);
        let mut out = Vec::new();
        let header = record::Header::new(RecordType::BeginRequest, 1, 8);
        header.write(&mut out).unwrap();
        out.extend_from_slice(&request.encode());
        out
    }

    fn params(pairs: &[(&str, &str)]) -> Vec<u8> {
        let payload = nvpair::encode_pairs(
            pairs
                .iter()
                .map(|(name, value)| (name.as_bytes(), value.as_bytes())),
        );
        let mut out = framed(RecordType::Params, 1, &payload);
        // The zero-length PARAMS record terminates the stream.
        out.extend(framed(RecordType::Params, 1, b""));
        out
    }

    fn stdin(body: &[u8]) -> Vec<u8> {
        let mut out = framed(RecordType::Stdin, 1, body);
        out.extend(framed(RecordType::Stdin, 1, b""));
        out
    }

    /// Read the next record, skipping the zero-length records that terminate a
    /// stream type. Real servers emit them, so the tests must tolerate them
    /// exactly as the client does.
    fn next_non_empty<R: Read>(reader: &mut R) -> Option<record::Record> {
        loop {
            let record = record::read_record(reader).ok()??;
            if record.content.is_empty() {
                continue;
            }
            return Some(record);
        }
    }

    /// Read the next record of any kind, terminators included.
    fn next_record<R: Read>(reader: &mut R) -> Option<record::Record> {
        record::read_record(reader).ok()?
    }

    #[test]
    fn reads_a_complete_request() {
        let mut wire = begin(true);
        wire.extend(params(&[
            ("REQUEST_METHOD", "POST"),
            ("SCRIPT_FILENAME", "/srv/index.php"),
        ]));
        wire.extend(stdin(b"a=1"));

        let request = read_request(&mut wire.as_slice(), LIMIT).unwrap().unwrap();
        assert_eq!(request.param_str("REQUEST_METHOD").as_deref(), Some("POST"));
        assert_eq!(
            request.param_str("SCRIPT_FILENAME").as_deref(),
            Some("/srv/index.php")
        );
        assert_eq!(request.body, b"a=1");
        assert!(request.keep_conn);
    }

    #[test]
    fn reads_a_request_split_across_several_params_records() {
        let mut wire = begin(false);
        // Two PARAMS records carrying one pair each, then the terminator.
        //
        // nginx does *not* do this: `ngx_http_fastcgi_create_request` sums every pair and emits
        // the whole environment as one record (verified in nginx 1.30.5). This shape is what the
        // front-end tolerates, not what it receives, and the split is at a *pair* boundary
        // because a pair straddling two records is not reassembled.
        let first = nvpair::encode_pairs([(&b"REQUEST_METHOD"[..], &b"GET"[..])]);
        let second = nvpair::encode_pairs([(&b"QUERY_STRING"[..], &b"x=1"[..])]);
        wire.extend(framed(RecordType::Params, 1, &first));
        wire.extend(framed(RecordType::Params, 1, &second));
        wire.extend(framed(RecordType::Params, 1, b""));
        wire.extend(stdin(b""));

        let request = read_request(&mut wire.as_slice(), LIMIT).unwrap().unwrap();
        assert_eq!(request.params.len(), 2);
        assert_eq!(request.param_str("QUERY_STRING").as_deref(), Some("x=1"));
        assert!(!request.keep_conn);
    }

    #[test]
    fn reads_a_body_split_across_several_stdin_records() {
        let mut wire = begin(true);
        wire.extend(params(&[("REQUEST_METHOD", "POST")]));
        wire.extend(framed(RecordType::Stdin, 1, b"part-one"));
        wire.extend(framed(RecordType::Stdin, 1, b"part-two"));
        wire.extend(framed(RecordType::Stdin, 1, b""));

        let request = read_request(&mut wire.as_slice(), LIMIT).unwrap().unwrap();
        assert_eq!(request.body, b"part-onepart-two");
    }

    #[test]
    fn a_clean_end_of_stream_is_not_an_error() {
        assert!(read_request(&mut [].as_slice(), LIMIT).unwrap().is_none());
    }

    #[test]
    fn a_connection_closing_mid_request_is_an_error() {
        let mut wire = begin(true);
        wire.extend(params(&[("REQUEST_METHOD", "GET")]));
        // No STDIN terminator: the connection died.
        assert!(read_request(&mut wire.as_slice(), LIMIT).is_err());
    }

    #[test]
    fn a_malformed_begin_request_is_reported() {
        let mut wire = Vec::new();
        let header = record::Header::new(RecordType::BeginRequest, 1, 2);
        header.write(&mut wire).unwrap();
        wire.extend_from_slice(&[0, 9]);
        wire.extend(std::iter::repeat_n(0u8, header.padding_length as usize));
        assert!(read_request(&mut wire.as_slice(), LIMIT).is_err());
    }

    #[test]
    fn writes_a_stdout_stderr_and_end_sequence() {
        let mut wire = Vec::new();
        write_response(
            &mut wire,
            7,
            b"Status: 200 OK\r\n\r\nhi",
            b"a warning",
            true,
        )
        .unwrap();

        let mut cursor = wire.as_slice();
        let stdout = next_non_empty(&mut cursor).unwrap();
        assert_eq!(stdout.record_type(), RecordType::Stdout);
        assert_eq!(stdout.request_id(), 7);
        assert_eq!(stdout.content, b"Status: 200 OK\r\n\r\nhi");

        let stderr = next_non_empty(&mut cursor).unwrap();
        assert_eq!(stderr.record_type(), RecordType::Stderr);
        assert_eq!(stderr.content, b"a warning");

        let end = next_non_empty(&mut cursor).unwrap();
        assert_eq!(end.record_type(), RecordType::EndRequest);
        assert!(EndRequest::decode(&end.content)
            .unwrap()
            .protocol_status
            .is_complete());
    }

    #[test]
    fn omits_the_stderr_record_when_there_is_nothing_to_report() {
        let mut wire = Vec::new();
        write_response(&mut wire, 1, b"body", b"", true).unwrap();

        let mut cursor = wire.as_slice();
        assert_eq!(
            next_non_empty(&mut cursor).unwrap().record_type(),
            RecordType::Stdout
        );
        // Straight to END_REQUEST: no STDERR record is written at all, and the
        // END_REQUEST itself is a single non-stream record.
        assert_eq!(
            next_non_empty(&mut cursor).unwrap().record_type(),
            RecordType::EndRequest
        );
        assert!(next_record(&mut cursor).is_none(), "nothing follows");
    }

    #[test]
    fn an_error_response_is_a_502_with_a_reason() {
        let mut wire = Vec::new();
        write_error(
            &mut wire,
            3,
            "the backend is down",
            ServerErrorKind::Backend,
        )
        .unwrap();

        let mut cursor = wire.as_slice();
        let stdout = next_non_empty(&mut cursor).unwrap();
        let text = String::from_utf8_lossy(&stdout.content);
        assert!(text.contains("502 Bad Gateway"), "{text}");
        assert!(text.contains("the backend is down"), "{text}");

        let stderr = next_non_empty(&mut cursor).unwrap();
        assert_eq!(stderr.content, b"the backend is down");
        assert_eq!(stderr.request_id(), 3);

        let end = next_non_empty(&mut cursor).unwrap();
        let end = EndRequest::decode(&end.content).unwrap();
        // The backend failed, but the request itself completed.
        assert!(end.protocol_status.is_complete());
    }

    #[test]
    fn an_overloaded_server_says_so_in_the_protocol_status() {
        let mut wire = Vec::new();
        write_error(&mut wire, 1, "busy", ServerErrorKind::Overloaded).unwrap();
        let mut cursor = wire.as_slice();
        let _ = next_non_empty(&mut cursor).unwrap();
        let _ = next_non_empty(&mut cursor).unwrap();
        let end = next_non_empty(&mut cursor).unwrap();
        assert_eq!(
            EndRequest::decode(&end.content).unwrap().protocol_status,
            ProtocolStatus::Overloaded
        );
    }

    #[test]
    fn a_multiplexed_connection_is_refused_rather_than_hung() {
        let mut wire = Vec::new();
        write_error(
            &mut wire,
            1,
            "no multiplexing",
            ServerErrorKind::NoMultiplexing,
        )
        .unwrap();
        let mut cursor = wire.as_slice();
        let _ = next_non_empty(&mut cursor).unwrap();
        let _ = next_non_empty(&mut cursor).unwrap();
        let end = next_non_empty(&mut cursor).unwrap();
        assert_eq!(
            EndRequest::decode(&end.content).unwrap().protocol_status,
            ProtocolStatus::CantMpxConn
        );
    }

    #[test]
    fn an_oversized_body_is_a_413_and_not_a_502() {
        // The `Status:` line was hardcoded to `502 Bad Gateway` for every kind, which would blame
        // the backend for a limit the *client* exceeded. Pinned as its own test because the string
        // is assembled in one place and nothing else reads it.
        let mut wire = Vec::new();
        write_error(
            &mut wire,
            1,
            "the request body exceeds the 15 byte limit",
            ServerErrorKind::BodyTooLarge,
        )
        .unwrap();

        let mut cursor = wire.as_slice();
        let stdout = next_non_empty(&mut cursor).unwrap();
        let text = String::from_utf8_lossy(&stdout.content);
        assert!(text.starts_with("Status: 413 Content Too Large"), "{text}");
        assert!(!text.contains("502"), "{text}");

        let _ = next_non_empty(&mut cursor).unwrap();
        let end = next_non_empty(&mut cursor).unwrap();
        // The *connection* was fine, so nginx must not retry elsewhere: an oversized body is
        // oversized at every worker.
        assert!(EndRequest::decode(&end.content)
            .unwrap()
            .protocol_status
            .is_complete());
    }

    #[test]
    fn a_rate_limited_request_is_a_429_with_a_retry_after_and_a_completed_request() {
        // Three things have to agree, and each was a separate decision:
        //
        // 1. The `Status:` line is 429, not the 502 every other kind defaults to — the gateway did
        //    not fail, the caller has no allowance.
        // 2. `Retry-After` is present and exact, because its absence is what turns a limit into a
        //    request that is retried immediately, for ever.
        // 3. The protocol status is `RequestComplete`, so nginx does **not** try another worker: a
        //    rate-limited caller is rate-limited at every one of them, and `Overloaded` would put
        //    nginx into a retry loop the limit exists to prevent.
        let mut wire = Vec::new();
        write_rate_limited(&mut wire, 1, "the ip rate limit refused this request", 42).unwrap();

        let mut cursor = wire.as_slice();
        let stdout = next_non_empty(&mut cursor).unwrap();
        let text = String::from_utf8_lossy(&stdout.content);
        assert!(text.starts_with("Status: 429 Too Many Requests"), "{text}");
        assert!(text.contains("Retry-After: 42"), "{text}");
        assert!(text.contains("Cache-Control: no-store"), "{text}");
        assert!(
            !text.contains("502"),
            "a rate limit is not a gateway failure: {text}"
        );
        assert!(
            !text.contains("503"),
            "a 503 would tell a load balancer this node is unhealthy: {text}"
        );

        let _ = next_non_empty(&mut cursor).unwrap();
        let end = next_non_empty(&mut cursor).unwrap();
        assert!(
            EndRequest::decode(&end.content)
                .unwrap()
                .protocol_status
                .is_complete(),
            "the connection was fine, so nginx must not retry elsewhere"
        );
    }

    #[test]
    fn a_zero_second_retry_after_is_floored_at_one() {
        // `Retry-After: 0` invites an immediate retry against a bucket that is still empty, so the
        // floor lives in the writer rather than in each caller. The limiter already refuses to
        // report zero; this is the second half of the same promise.
        let mut wire = Vec::new();
        write_rate_limited(&mut wire, 1, "limited", 0).unwrap();

        let mut cursor = wire.as_slice();
        let stdout = next_non_empty(&mut cursor).unwrap();
        let text = String::from_utf8_lossy(&stdout.content);
        assert!(text.contains("Retry-After: 1"), "{text}");
    }

    #[test]
    fn an_oversized_body_still_says_no_store_and_still_has_no_retry_after() {
        // The shared writer is what the 413 now goes through, so the header that was added for the
        // 429 must not have leaked onto it: there is no meaningful "try again" for a body that is
        // too large, and a `Retry-After` there would be an invented number.
        let mut wire = Vec::new();
        write_error(&mut wire, 1, "too large", ServerErrorKind::BodyTooLarge).unwrap();

        let mut cursor = wire.as_slice();
        let stdout = next_non_empty(&mut cursor).unwrap();
        let text = String::from_utf8_lossy(&stdout.content);
        assert!(text.contains("Cache-Control: no-store"), "{text}");
        assert!(!text.contains("Retry-After"), "{text}");
    }

    #[test]
    fn answers_a_get_values_probe() {
        let mut wire = Vec::new();
        write_get_values(&mut wire, &[("max_connections", "10")]).unwrap();

        let mut cursor = wire.as_slice();
        let result = next_non_empty(&mut cursor).unwrap();
        assert_eq!(result.record_type(), RecordType::GetValuesResult);
        assert_eq!(result.request_id(), volucer_fcgi::MANAGEMENT_REQUEST_ID);
        let pairs = nvpair::decode_pairs(&result.content).unwrap();
        assert_eq!(pairs[0].0, b"max_connections");
        assert_eq!(pairs[0].1, b"10");
    }

    #[test]
    fn reads_a_management_request() {
        let payload = nvpair::encode_pairs([(&b"max_connections"[..], &b""[..])]);
        let wire = framed(RecordType::GetValues, 0, &payload);
        let names = read_management_request(&mut wire.as_slice())
            .unwrap()
            .unwrap();
        assert_eq!(names[0].0, b"max_connections");

        // A non-management record is a protocol error in this position.
        let wire = framed(RecordType::Stdin, 1, b"x");
        assert!(read_management_request(&mut wire.as_slice()).is_err());
    }

    #[test]
    fn converts_into_a_backend_request_without_losing_anything() {
        let mut wire = begin(true);
        wire.extend(params(&[("REQUEST_METHOD", "GET")]));
        wire.extend(stdin(b""));
        let incoming = read_request(&mut wire.as_slice(), LIMIT).unwrap().unwrap();

        let backend = incoming.into_backend_request();
        assert_eq!(backend.body, b"");
        assert!(backend.keep_conn);
        assert_eq!(backend.params[0].0, b"REQUEST_METHOD");
    }

    #[test]
    fn a_request_before_begin_is_skipped_rather_than_fatal() {
        // Some clients emit a management record first; skipping keeps the
        // connection usable.
        let mut wire = framed(RecordType::Data, 0, b"ignored");
        wire.extend(begin(false));
        wire.extend(params(&[("REQUEST_METHOD", "GET")]));
        wire.extend(stdin(b""));

        let request = read_request(&mut wire.as_slice(), LIMIT).unwrap().unwrap();
        assert_eq!(request.param_str("REQUEST_METHOD").as_deref(), Some("GET"));
    }

    #[test]
    fn a_body_over_the_limit_is_refused_while_it_is_still_arriving() {
        // The defect this closes: `read_next` extended a `Vec` for as long as `STDIN` records
        // arrived, so a request that reached this front-end had **no** bound and the only limit
        // was nginx's `client_max_body_size` on somebody else's configuration.
        //
        // `framed` rather than the `stdin` helper, deliberately: `stdin` appends its own
        // terminator, so two calls to it send `chunk, END, chunk, END` and the body ends at the
        // first chunk. The limit then looks like it works when it is never reached — which is how
        // this test first passed its own setup and failed the assertion it was written for.
        let mut wire = begin(false);
        wire.extend(params(&[("REQUEST_METHOD", "POST")]));
        wire.extend(framed(RecordType::Stdin, 1, b"0123456789"));
        wire.extend(framed(RecordType::Stdin, 1, b"0123456789"));
        wire.extend(framed(RecordType::Stdin, 1, b""));

        let err = read_request(&mut wire.as_slice(), 15).expect_err("must be refused");
        assert!(
            matches!(err, ServerError::BodyTooLarge { limit: 15 }),
            "{err:?}"
        );
    }

    #[test]
    fn a_body_exactly_at_the_limit_is_accepted() {
        // The boundary, because "at the limit" and "over the limit" differ by one byte and an
        // off-by-one here refuses a legitimate upload of exactly the configured size.
        let mut wire = begin(false);
        wire.extend(params(&[("REQUEST_METHOD", "POST")]));
        wire.extend(framed(RecordType::Stdin, 1, b"0123456789"));
        wire.extend(framed(RecordType::Stdin, 1, b"01234"));
        wire.extend(framed(RecordType::Stdin, 1, b""));

        let request = read_request(&mut wire.as_slice(), 15)
            .expect("accepted")
            .expect("a request");
        assert_eq!(request.body.len(), 15);
    }

    #[test]
    fn a_limit_of_zero_refuses_a_body_but_not_a_request_without_one() {
        // Zero cannot be reached through `Config::validate`, which refuses it, but the read path
        // still has to have an answer — and the answer that matters is that a bodyless `GET` is
        // unaffected, so `max_body_bytes = 0` could never be mistaken for "refuse everything".
        let mut wire = begin(false);
        wire.extend(params(&[("REQUEST_METHOD", "GET")]));
        wire.extend(framed(RecordType::Stdin, 1, b""));
        let request = read_request(&mut wire.as_slice(), 0)
            .expect("a bodyless request is under any limit")
            .expect("a request");
        assert!(request.body.is_empty());

        let mut wire = begin(false);
        wire.extend(params(&[("REQUEST_METHOD", "POST")]));
        wire.extend(framed(RecordType::Stdin, 1, b"x"));
        wire.extend(framed(RecordType::Stdin, 1, b""));
        assert!(read_request(&mut wire.as_slice(), 0).is_err());
    }

    #[test]
    fn split_response_reports_the_constant_request_id() {
        let response = Response {
            stdout: b"body".to_vec(),
            stderr: b"log".to_vec(),
            ..Response::default()
        };
        let (stdout, stderr, request_id) = split_response(&response);
        assert_eq!(stdout, b"body");
        assert_eq!(stderr, b"log");
        assert_eq!(request_id, volucer_fcgi::REQUEST_ID);
    }

    #[test]
    fn timing_measures_something_positive() {
        let timing = Timing::start();
        std::thread::sleep(Duration::from_millis(1));
        assert!(timing.elapsed() >= Duration::from_millis(1));
    }
}
