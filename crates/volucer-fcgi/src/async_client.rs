//! A FastCGI client that a `tokio` runtime can drive, so a request does not hold a thread.
//!
//! # Why this exists
//!
//! [`Client`](crate::client::Client) is a blocking socket with a read timeout. That is the right
//! shape for a worker on a pipe and it was the right shape here too, until it was measured: the
//! HTTP front-end could only answer a FastCGI backend by parking a blocking-pool thread for the
//! whole exchange, and at concurrency 100 that is one thread per in-flight request.
//!
//! Measured on the 12-core reference host, one php-fpm pool of 96 children, `wrk -c100`, one page
//! (`GET /` -> `{"hello":"world"}`):
//!
//! | backend | dispatch | front-end CPU/req | throughput |
//! |---|---|---|---|
//! | `attach` (worker) | awaited | 46.3 us | 95,734 rps |
//! | `external` (this crate) | parked thread | 78.9 us | 44,362 rps |
//! | nginx -> the same pool | — | 35.5 us | 64,663 rps |
//!
//! The same binary, host, page and pool; the only difference is which dispatch path the backend was
//! allowed to take. Thread count made it unambiguous — 16 threads idle, 29 at `c=8`, 78 at `c=32`,
//! **185 at `c=100`**, against nginx's 5 — and a microbenchmark put the hand-off itself at 8.7 us
//! per request once pipelined, the rest being the scheduler pressure those threads create.
//!
//! So this type exists to answer the same question the client above does, `Send` across an `await`
//! instead of parking a thread for it.
//!
//! # What it deliberately does not do
//!
//! The record codec, the param encoding and the response parsing are reused verbatim from
//! [`record`](crate::record) and [`client`](crate::client): an async client that framed records its
//! own way would be a second protocol implementation with its own bugs. What is new here is only the
//! byte plumbing — [`AsyncStream`] over `tokio`'s sockets, and the two `read`/`write` helpers that
//! the existing codec needs.
//!
//! There is no multiplexing. `REQUEST_ID` is a constant and one connection carries one request at a
//! time, exactly as on the blocking side: a FastCGI connection is a conversation and this server
//! does not implement the request-id demultiplexer `FCGI_MPXS_CONNS` would require.

use std::io;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
#[cfg(unix)]
use tokio::net::UnixStream;

use crate::client::{Request, Response, REQUEST_ID};
use crate::error::{FcgiError, Result};
use crate::message::EndRequest;
use crate::protocol::{RecordType, HEADER_LEN};
use crate::record::Header;
use crate::transport::{Endpoint, Stream};

/// A connected backend that a runtime drives.
#[derive(Debug)]
pub enum AsyncStream {
    Tcp(TcpStream),
    #[cfg(unix)]
    Unix(UnixStream),
}

impl AsyncStream {
    /// Connect to a backend, asynchronously and with an optional deadline.
    ///
    /// `timeout` covers the connect only. Reads are bounded by the caller's own deadline, which is
    /// the contract every other awaited backend in this server has — the pool kills the child, and a
    /// per-read socket timeout would be a second, disagreeing answer.
    pub async fn connect(endpoint: &Endpoint, timeout: Option<Duration>) -> Result<Self> {
        match endpoint {
            Endpoint::Tcp(spec) => {
                let stream = connect_tcp(spec, timeout).await?;
                // The same two settings the blocking transport applies, for the same reasons: Nagle
                // only adds delay to a small header/PARAMS/STDIN sequence, and a Linux `php-cgi`
                // child never sets `TCP_NODELAY` itself.
                let _ = stream.set_nodelay(true);
                Ok(AsyncStream::Tcp(stream))
            }
            #[cfg(unix)]
            Endpoint::Unix(path) => Ok(AsyncStream::Unix(connect_unix(path, timeout).await?)),
            // A named pipe needs overlapped I/O that `tokio` does not expose, and the blocking
            // transport's pipe exists only because `php-cgi -b` binds one on Windows. Refusing it
            // here leaves the blocking path in place for that platform rather than pretending.
            #[cfg(windows)]
            Endpoint::Pipe(_) => Err(FcgiError::Io(io::Error::new(
                io::ErrorKind::Unsupported,
                "a named pipe cannot be driven by a tokio runtime",
            ))),
        }
    }
}

/// Connect to a TCP endpoint, honouring an optional deadline.
///
/// `tokio::net::TcpStream::connect` takes a `ToSocketAddrs`, and the endpoint is a string that may
/// be a host name; resolving it is the part that can block on a name server, so it goes through
/// `lookup_host` rather than being handed to `connect` directly.
async fn connect_tcp(spec: &str, timeout: Option<Duration>) -> Result<TcpStream> {
    let resolve = tokio::net::lookup_host(spec);
    let mut addrs = match timeout {
        Some(limit) => tokio::time::timeout(limit, resolve)
            .await
            .map_err(|_| timed_out("resolving the backend address"))??,
        None => resolve.await?,
    };
    let addr = addrs.next().ok_or_else(|| {
        FcgiError::Io(io::Error::new(
            io::ErrorKind::AddrNotAvailable,
            format!("could not resolve {spec:?}"),
        ))
    })?;

    match timeout {
        Some(limit) => tokio::time::timeout(limit, TcpStream::connect(addr))
            .await
            .map_err(|_| timed_out("connecting to the backend"))?,
        None => TcpStream::connect(addr).await,
    }
    .map_err(FcgiError::Io)
}

#[cfg(unix)]
async fn connect_unix(path: &str, timeout: Option<Duration>) -> Result<UnixStream> {
    match timeout {
        Some(limit) => tokio::time::timeout(limit, UnixStream::connect(path))
            .await
            .map_err(|_| timed_out("connecting to the backend"))?,
        None => UnixStream::connect(path).await,
    }
    .map_err(FcgiError::Io)
}

fn timed_out(what: &str) -> FcgiError {
    FcgiError::Io(io::Error::new(
        io::ErrorKind::TimedOut,
        format!("timed out {what}"),
    ))
}

impl AsyncRead for AsyncStream {
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

impl AsyncWrite for AsyncStream {
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

/// Read one record, discarding its padding — the awaited twin of [`record::read_record`].
///
/// Returns `None` at a clean end of stream, which the blocking reader reports the same way.
async fn read_record<R>(reader: &mut R) -> Result<Option<(Header, Vec<u8>)>>
where
    R: AsyncRead + Unpin,
{
    let mut head = [0u8; HEADER_LEN];
    if !read_exact_or_eof(reader, &mut head).await? {
        return Ok(None);
    }
    let header = Header::decode(head)?;

    let mut content = vec![0u8; header.content_length as usize];
    reader.read_exact(&mut content).await?;

    if header.padding_length > 0 {
        let mut padding = [0u8; 255];
        reader
            .read_exact(&mut padding[..header.padding_length as usize])
            .await?;
    }

    Ok(Some((header, content)))
}

/// Read exactly `buf.len()` bytes, reporting a clean EOF only when nothing at all was read.
///
/// Mirrors the blocking helper, including its distinction: a stream that ends part-way through a
/// header is an error, not a short read, because the alternative is a request that silently gets a
/// truncated response.
async fn read_exact_or_eof<R>(reader: &mut R, buf: &mut [u8]) -> Result<bool>
where
    R: AsyncRead + Unpin,
{
    let mut filled = 0;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..]).await {
            Ok(0) if filled == 0 => return Ok(false),
            Ok(0) => {
                return Err(FcgiError::Io(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "stream ended in the middle of a record header",
                )))
            }
            Ok(n) => filled += n,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(FcgiError::Io(err)),
        }
    }
    Ok(true)
}

/// Append a payload as one or more records to a buffer.
///
/// `record::append_records` is the blocking twin and the bytes are identical; it is reused rather
/// than reimplemented so a record framing bug cannot exist on only one path.
fn append_records(out: &mut Vec<u8>, record_type: RecordType, content: &[u8], terminate: bool) {
    crate::record::append_records(out, record_type, REQUEST_ID, content, terminate);
}

/// An awaited FastCGI client: one connection, one request at a time.
///
/// The buffer layout and the reuse discipline are copied from [`Client`] deliberately — `out` and
/// `params` are fields rather than locals so the hot path allocates nothing per request, which is
/// half of why the blocking client is fast in the first place. What is *not* copied is the socket:
/// that is [`AsyncStream`], and the read/write helpers below are the only places that know it.
#[derive(Debug)]
pub struct AsyncClient {
    stream: AsyncStream,
    /// The assembled request, reused across requests. See [`Client`] for why it is a field.
    out: Vec<u8>,
    /// The encoded `PARAMS` payload, kept apart so it can be built without borrowing `out`.
    params: Vec<u8>,
    /// Whether the last request asked for keep-alive.
    keep_conn_requested: bool,
    /// Set once reuse is known to be unsafe.
    closed_by_backend: bool,
    /// Set while a [`ResponseStream`](crate::client::ResponseStream) taken from this client has not
    /// been drained — the same invariant as the blocking client's, and for the same reason: handing
    /// the socket to the next request with the previous response's records still in front of it is
    /// the worst class of bug a pool can have.
    stream_open: bool,
}

impl AsyncClient {
    /// Wrap an already-connected backend.
    pub fn from_stream(stream: AsyncStream) -> Self {
        AsyncClient {
            stream,
            out: Vec::with_capacity(1024),
            params: Vec::with_capacity(512),
            keep_conn_requested: false,
            closed_by_backend: false,
            stream_open: false,
        }
    }

    /// Connect to a backend.
    pub async fn connect(endpoint: &Endpoint, timeout: Option<Duration>) -> Result<Self> {
        Ok(AsyncClient::from_stream(
            AsyncStream::connect(endpoint, timeout).await?,
        ))
    }

    /// Whether the connection is still usable.
    ///
    /// The same three conditions as the blocking client: not closed by the backend, no stream still
    /// open, and keep-alive was requested.
    pub fn is_reusable(&self) -> bool {
        !self.closed_by_backend && !self.stream_open && self.keep_conn_requested
    }

    /// Send one request and read its response, across an `await`.
    ///
    /// The sequence is identical to [`Client::request`]: assemble, one write, read until
    /// `END_REQUEST`, and mark the connection unusable if the backend refused keep-alive. A
    /// difference between the two would be a correctness bug on one of them, so the shape is
    /// intentionally the same.
    pub async fn request(&mut self, request: &Request) -> Result<Response> {
        let started = std::time::Instant::now();
        self.keep_conn_requested = request.keep_conn;
        self.send(request).await?;
        let mut response = self.receive().await?;
        response.duration = started.elapsed();
        if !response.keep_conn {
            self.closed_by_backend = true;
        }
        Ok(response)
    }

    async fn send(&mut self, request: &Request) -> Result<()> {
        self.out.clear();

        self.out
            .extend_from_slice(&Header::new(RecordType::BeginRequest, REQUEST_ID, 8).encode());
        self.out.extend_from_slice(
            &crate::message::BeginRequest::responder(request.keep_conn).encode(),
        );

        crate::nvpair::encode_pairs_into(
            &mut self.params,
            request
                .params
                .iter()
                .map(|(name, value)| (name.as_slice(), value.as_slice())),
        );
        append_records(&mut self.out, RecordType::Params, &self.params, true);
        append_records(&mut self.out, RecordType::Stdin, &request.body, true);

        self.stream.write_all(&self.out).await?;
        self.stream.flush().await?;
        Ok(())
    }

    async fn receive(&mut self) -> Result<Response> {
        let mut response = Response::default();
        loop {
            let Some((header, content)) = read_record(&mut self.stream).await? else {
                return Err(FcgiError::IncompleteResponse {
                    request_id: REQUEST_ID,
                    complete: true,
                });
            };

            match header.record_type {
                RecordType::Stdout => {
                    if !content.is_empty() {
                        response.stdout.extend_from_slice(&content);
                    }
                }
                RecordType::Stderr => {
                    if !content.is_empty() {
                        response.stderr.extend_from_slice(&content);
                    }
                }
                RecordType::EndRequest => {
                    response.end = Some(EndRequest::decode(&content)?);
                    break;
                }
                // A management record or a type we did not ask for: ignored, exactly as on the
                // blocking path.
                _ => continue,
            }
        }

        response.keep_conn = self.keep_conn_requested && !self.closed_by_backend;
        Ok(response)
    }

    /// Send one request and decide, from its header block, whether to buffer or to relay.
    ///
    /// The awaited twin of [`Client::request_streaming`], and the decision points are identical:
    /// the head is returned as soon as the header block terminates, and everything after it is the
    /// body. What differs is only that the records are read across an `await`.
    ///
    /// **Returning `Streamed` leaves the connection mid-response**, exactly as on the blocking side:
    /// the caller must drain it with [`AsyncClient::next_chunk`] before the socket is reusable, which
    /// is why `is_reusable` stays false while a stream is open.
    pub async fn request_streaming(
        &mut self,
        request: &Request,
        buffer: usize,
        relay: crate::client::Relay,
    ) -> Result<crate::client::Delivery> {
        let started = std::time::Instant::now();
        self.keep_conn_requested = request.keep_conn;
        self.send(request).await?;

        let mut head_block: Vec<u8> = Vec::new();
        let mut stderr: Vec<u8> = Vec::new();
        let mut head_len = 0usize;

        let end = loop {
            let Some((header, content)) = read_record(&mut self.stream).await? else {
                return Err(FcgiError::IncompleteResponse {
                    request_id: REQUEST_ID,
                    complete: true,
                });
            };

            match header.record_type {
                RecordType::Stdout => {
                    if content.is_empty() {
                        continue;
                    }
                    head_block.extend_from_slice(&content);
                    if head_len == 0 {
                        if let Some((at, separator)) = crate::client::find_header_end(&head_block) {
                            head_len = at + separator;
                        }
                    }
                    if head_len == 0 {
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
                        let (status, headers, _) = crate::client::parse_response(&head_block);
                        if relay.admits(status, &headers) {
                            let head = head_block[..head_len].to_vec();
                            let rest = head_block.split_off(head_len);
                            self.stream_open = true;
                            return Ok(crate::client::Delivery::Streamed(
                                crate::client::ResponseStream {
                                    head,
                                    pending: (!rest.is_empty()).then_some(rest),
                                    finished: false,
                                    stderr,
                                    end: None,
                                    started,
                                    head_duration: started.elapsed(),
                                    keep_conn_requested: self.keep_conn_requested,
                                },
                            ));
                        }
                    }
                }
                RecordType::Stderr => {
                    if !content.is_empty() {
                        stderr.extend_from_slice(&content);
                    }
                }
                RecordType::EndRequest => {
                    break Some(EndRequest::decode(&content)?);
                }
                _ => continue,
            }
        };

        let keep_conn = self.keep_conn_requested && !self.closed_by_backend;
        if !keep_conn {
            self.closed_by_backend = true;
        }
        Ok(crate::client::Delivery::Buffered(Response {
            stdout: head_block,
            stderr,
            end,
            keep_conn,
            duration: started.elapsed(),
        }))
    }

    /// Read the next piece of a streamed response, ending it at `END_REQUEST`.
    ///
    /// The awaited twin of [`Client::next_chunk`]. `Ok(None)` once the response is over; every byte
    /// the backend sent is returned exactly once and in order, including whatever was read past the
    /// head.
    pub async fn next_chunk(
        &mut self,
        stream: &mut crate::client::ResponseStream,
    ) -> Result<Option<Vec<u8>>> {
        if let Some(pending) = stream.pending.take() {
            return Ok(Some(pending));
        }
        if stream.finished {
            return Ok(None);
        }

        loop {
            let Some((header, content)) = read_record(&mut self.stream).await? else {
                // A stream that ends without `END_REQUEST` is truncation, not completion.
                self.closed_by_backend = true;
                stream.finished = false;
                return Ok(None);
            };

            match header.record_type {
                RecordType::Stdout => {
                    if content.is_empty() {
                        continue;
                    }
                    return Ok(Some(content));
                }
                RecordType::Stderr => {
                    if !content.is_empty() {
                        stream.stderr.extend_from_slice(&content);
                    }
                }
                RecordType::EndRequest => {
                    stream.end = Some(EndRequest::decode(&content)?);
                    stream.finished = true;
                    self.stream_open = false;
                    if !(self.keep_conn_requested && !self.closed_by_backend) {
                        self.closed_by_backend = true;
                    }
                    return Ok(None);
                }
                _ => continue,
            }
        }
    }
}

/// A connected blocking `Stream`, adopted by a runtime.
///
/// `external` and `spawn_cgi` already own a [`Client`] per socket, and the pool hands one out per
/// request. Adopting an existing socket rather than reconnecting is what keeps this change from
/// also changing *which* connection a request uses — the thing the affinity measurements spent a
/// whole probe on.
///
/// **The clone is load-bearing.** `prepare_for_async` switches the descriptor to non-blocking and
/// clears `SO_RCVTIMEO`/`SO_SNDTIMEO`, and those are properties of the *file description*, not of
/// the handle — so they apply to the original socket too, which is the point (the blocking side
/// must not touch it while the runtime owns it). `try_clone` is then what gives the runtime its own
/// handle, and it duplicates the same description, so both handles drive one connection.
pub fn adopt(stream: &Stream) -> Result<AsyncStream> {
    stream.prepare_for_async()?;
    match stream {
        Stream::Tcp(blocking) => {
            let std_stream = blocking.try_clone()?;
            Ok(AsyncStream::Tcp(TcpStream::from_std(std_stream)?))
        }
        #[cfg(unix)]
        Stream::Unix(blocking) => {
            let std_stream = blocking.try_clone()?;
            Ok(AsyncStream::Unix(UnixStream::from_std(std_stream)?))
        }
        #[cfg(windows)]
        Stream::Pipe(_) => Err(FcgiError::Io(io::Error::new(
            io::ErrorKind::Unsupported,
            "a named pipe cannot be driven by a tokio runtime",
        ))),
    }
}
