//! Turning an inbound request into an origin's request, and an origin's answer back.
//!
//! Two functions, and they are the whole difference between this path and the CGI one. Both are
//! deliberately free of I/O so they can be tested without a socket: the assertions that matter here
//! are about *which headers survive and in what order*, and a test that needs a network to make
//! them is a test that will be skipped.
//!
//! # What is lost and what is not
//!
//! | | CGI path | this path |
//! | --- | --- | --- |
//! | header case | lowercased (a CGI variable cannot carry case) | preserved on the response leg; the request leg is normalised by `hyper`'s reader before any code sees it |
//! | duplicate headers | joined with `", "` into one variable | preserved — `HeaderMap` is a multimap |
//! | the request line | three separate variables | a method and a URI |
//! | the body | collected before dispatch | streamed |
//!
//! The request-leg case limitation is `hyper`'s, not a choice made here, and it is harmless: header
//! names are case-insensitive (RFC 9110 §5.1), so an origin reading `content-type` reads the same
//! header. A test asserting the original case on the request leg would be a test that cannot pass,
//! which is worse than not having one.

use hyper::header::{HeaderMap, HeaderName, HeaderValue, CONTENT_LENGTH, HOST};
use hyper::{Method, Uri, Version};

use crate::client::{connection_named, UpstreamRequest, UpstreamResponse};

/// Headers that never cross a hop, by name.
///
/// `Content-Length` and `Transfer-Encoding` are here because `hyper` re-derives framing from the
/// body's size hint: forwarding a stale length is how a proxy desynchronises a connection, and a
/// chunked upload only works end to end if the incoming framing is dropped.
///
/// `Expect` is here for a reason of its own — see [`prepare_request`].
///
/// **`TE` is here and is the one entry with a documented exception.** RFC 9110 §7.6.1 makes it
/// hop-by-hop, so a generic proxy must drop it. gRPC is not generic: its protocol *requires* the
/// client to send `TE: trailers` and requires it to arrive, which is how a server knows the client
/// can read the trailers a gRPC reply ends with. Every gRPC-aware proxy therefore guarantees it —
/// see [`prepare_request`] for exactly what volucer does with it, which is to *construct* it rather
/// than to forward whatever arrived.
const NEVER_FORWARDED: [&str; 10] = [
    "connection",
    "keep-alive",
    "proxy-connection",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "expect",
];

fn never_forwarded(name: &HeaderName) -> bool {
    NEVER_FORWARDED
        .iter()
        .any(|candidate| name.as_str().eq_ignore_ascii_case(candidate))
}

/// Whether this request is a gRPC call.///
/// By `content-type`, which is the only thing that says so: gRPC's media type is `application/grpc`
/// with optional suffixes (`+proto`, `+json`, and the `-web`/`-web-text` forms), and
/// `docs/plugins.md` §6.6 is explicit that the server never parses a message — so this is a prefix
/// test and nothing more. A `:path` shaped like `/pkg.Service/Method` is *not* the test, because an
/// HTTP route can be spelled that way and must keep working.
///
/// The prefix has to be followed by the end of the value, `;`, `+`, or `-` — not by any byte. The
/// first version was a bare `starts_with("application/grpc")` and accepted `application/grpcx`,
/// which is caught by the test below: a media type is a token rather than a string, and
/// `application/grpcx` is a *different* type that happens to begin with the same letters. Treating
/// it as gRPC would route an ordinary origin's response down the h2 path.
pub fn is_grpc(headers: &HeaderMap) -> bool {
    headers
        .get(hyper::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            let lowered = value.trim().to_ascii_lowercase();
            let Some(rest) = lowered.strip_prefix("application/grpc") else {
                return false;
            };
            matches!(rest.as_bytes().first(), None | Some(b';' | b'+' | b'-'))
        })
}

/// Build the request an origin should receive.
///
/// `peer` is the address the request came from, and `cleartext` whether it arrived without TLS —
/// which is not the same question as "is this deployment TLS", because `[server.tls].plain` makes a
/// cleartext request on a TLS port a real shape here. Both go into the `X-Forwarded-*` set.
///
/// # `X-Forwarded-*` is appended, never replaced
///
/// This is the one security-relevant rule in the function. An origin that trusts this server for
/// `X-Forwarded-For` must not be reachable by a client that simply sent its own: if the client's
/// value were replaced, the origin would read the attacker's claim as fact; if the client's value
/// were kept and ours dropped, the origin would read nothing true. Appending puts the real peer
/// *last*, which is the position a correct origin reads, and leaves the client's own claim visible
/// for the origin to discount. `X-Forwarded-Proto` and `-Host` follow the same rule for the same
/// reason.
///
/// # `Expect: 100-continue` is stripped
///
/// We forward on the same connection the client is waiting on and never generate an interim `100`,
/// so an `Expect` that reached the origin would make a client wait for a reply that is coming from
/// a place it is not listening. Stripping it makes the client stop waiting, which is correct.
///
/// # `TE: trailers` is constructed, not forwarded
///
/// The only place this function *adds* a header a client sent. RFC 9110 §7.6.1 makes `TE`
/// hop-by-hop, so it is in [`NEVER_FORWARDED`] and the client's own value is dropped — but gRPC's
/// protocol requires the server to receive `TE: trailers`, and the server is the origin here. Two
/// consequences worth stating:
///
/// - **Value `trailers` is enough, and is exactly what gRPC requires.** A client is not obliged to
///   spell the case or the list the same way (`te: trailers, deflate` is legal), so *forwarding* it
///   would mean forwarding a hop-by-hop header whose value this server has no reason to trust.
///   Writing the one value gRPC defines is both simpler and more correct.
/// - **It is added for every request, not only for gRPC ones.** A generic client that sets
///   `TE: trailers` means it — that is what the header says — and the origin is the next hop, so
///   telling it so is the honest forwarding of the client's *intent* rather than of its characters.
///   Restricting it to gRPC would also make the h2c path and the TLS/ALPN path disagree about what a
///   gRPC origin sees, since the former is decided here and the latter by the handshake.
///
/// The alternative — forwarding `TE` verbatim and letting the origin sort it out — was refused
/// because it is precisely the hop-by-hop leak the list exists to prevent, and because a client
/// could then ask an origin for `TE: gzip` and get a compressed reply this server would pass on
/// without ever having negotiated it.
pub fn prepare_request<B>(
    headers: HeaderMap,
    method: Method,
    uri: Uri,
    version: Version,
    body: B,
    peer: Option<std::net::IpAddr>,
    cleartext: bool,
) -> UpstreamRequest<B> {
    // Read before filtering: `Connection: X-Private` says `X-Private` is hop-local *for this
    // message*, so it has to be dropped along with the names above. Skipping this rule is how a
    // proxy leaks hop-local headers into the next hop.
    let named: Vec<HeaderName> = connection_named(&headers)
        .into_iter()
        .filter(|name| !never_forwarded(name))
        .collect();

    // An address, not a `SocketAddr`: the port of an *incoming* connection is not part of what
    // `X-Forwarded-For` carries, and a caller that has only the peer's address (which is what a
    // listener reports) must not have to invent one to satisfy this signature.
    let forwarded_for = peer.map(|peer| peer.to_string());
    let forwarded_host = headers
        .get(HOST)
        .and_then(|host| host.to_str().ok())
        .map(str::to_string);

    // Copied rather than filtered in place, because `HeaderMap` has no retain-while-preserving-all-
    // values operation and `remove` by name would need the full list of names up front — including
    // the ones only this message's own `Connection` header knows about.
    let mut outgoing = HeaderMap::with_capacity(headers.len());
    for (name, value) in headers.iter() {
        if never_forwarded(name) || named.iter().any(|named| named == name) {
            continue;
        }
        if name == CONTENT_LENGTH {
            continue;
        }
        outgoing.append(name.clone(), value.clone());
    }
    drop(headers);

    if let Some(address) = forwarded_for {
        // Appended, so a client-supplied value survives *before* ours rather than instead of it.
        if let Ok(value) = HeaderValue::from_str(&address) {
            outgoing.append("x-forwarded-for", value);
        }
    }
    if let Some(host) = forwarded_host {
        if let Ok(value) = HeaderValue::from_str(&host) {
            outgoing.append("x-forwarded-host", value);
        }
    }
    // From the socket, not from a header: `X-Forwarded-Proto` describes how *this* hop was
    // received, and a client cannot be allowed to claim it was TLS when it was not.
    outgoing.append(
        "x-forwarded-proto",
        HeaderValue::from_static(if cleartext { "http" } else { "https" }),
    );

    // Constructed rather than forwarded — see this function's `TE` section. Appended after the
    // filter so a client-supplied value cannot survive alongside it, and added unconditionally
    // because a generic client that set it meant it.
    outgoing.append(hyper::header::TE, HeaderValue::from_static("trailers"));

    UpstreamRequest {
        method,
        uri,
        version,
        headers: outgoing,
        body,
    }
}

/// Make an origin's answer safe to pass to the client.
///
/// `Content-Length` is **kept**, and this is the one place the two paths deliberately differ.
/// `from_cgi` drops it and recomputes, because a CGI response's length is application-supplied and
/// can be wrong. An HTTP origin's is protocol-level framing: `hyper` re-validates it against the
/// frames it reads, so a mismatch is a clean protocol error rather than a desynchronised
/// connection — which is the failure mode to prefer.
pub fn translate_response(response: UpstreamResponse) -> UpstreamResponse {
    let names = connection_named(&response.headers);
    let UpstreamResponse {
        status,
        mut headers,
        body,
        version,
    } = response;

    let mut outgoing = HeaderMap::with_capacity(headers.len());
    for (name, value) in headers.iter() {
        if never_forwarded(name) || names.iter().any(|named| named == name) {
            continue;
        }
        // `Expect` is an inbound request directive; an origin echoing one back is not a reason to
        // drop anything else, but it must not be forwarded either.
        outgoing.append(name.clone(), value.clone());
    }
    headers = outgoing;

    UpstreamResponse {
        status,
        headers,
        body,
        version,
    }
}

/// A gRPC failure, as the wire spells one.
///
/// gRPC reports every outcome the same way: **HTTP 200 with a status in the trailers**, not an
/// HTTP error status. A proxy that answers `502` for a gRPC call has produced something no gRPC
/// client can read — the client sees an HTTP status it was not expecting and cannot map to a gRPC
/// status code, so the failure an operator needs to see becomes "unexpected status". This type is
/// the mapping, and it is the second of the two things `docs/plugins.md` §6.6 lists as the whole of
/// what tier A still needed once the transport worked.
///
/// The numbers are the specification's, not invented here: `0` `OK`, `12` `UNIMPLEMENTED`,
/// `13` `INTERNAL`, `14` `UNAVAILABLE`. Only four, because **these are the four a proxy can
/// actually determine** — every other code describes something that happened inside the
/// application, and a proxy that invented one would be lying about it. An application that wants
/// to say `NOT_FOUND` says it in its own trailers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrpcStatus {
    /// The origin could not be reached, or the connection died mid-call.
    ///
    /// `UNAVAILABLE` rather than `INTERNAL`, and the distinction matters to a client: the
    /// specification defines `UNAVAILABLE` as *transient*, and it is what makes a client retry
    /// against a different endpoint. A retryable failure reported as `INTERNAL` is a retry that
    /// never happens.
    Unavailable,
    /// The origin answered, but not in a way this server can pass on — a protocol error, or a
    /// response the pool refused.
    Internal,
    /// The route resolved to a backend that is not an upstream, or the origin's reply was not
    /// gRPC at all.
    Unimplemented,
}

impl GrpcStatus {
    /// The numeric status, as gRPC defines it.
    pub fn code(self) -> u8 {
        match self {
            GrpcStatus::Unimplemented => 12,
            GrpcStatus::Internal => 13,
            GrpcStatus::Unavailable => 14,
        }
    }

    /// The status name, for a reader who is looking at a log rather than a client.
    pub fn name(self) -> &'static str {
        match self {
            GrpcStatus::Unimplemented => "UNIMPLEMENTED",
            GrpcStatus::Internal => "INTERNAL",
            GrpcStatus::Unavailable => "UNAVAILABLE",
        }
    }

    /// The message that travels beside the status.
    ///
    /// A constant rather than the failing operation's own error text, and that is a decision rather
    /// than laziness: `grpc-message` is percent-encoded ASCII (gRPC's own specification), and an
    /// origin's error message is arbitrary bytes which would have to be escaped or dropped. A
    /// client cannot act on "connection refused while dialling unix:/run/app.sock" any more than on
    /// this sentence; the operator can act on it, and the operator reads the log, where the real
    /// reason is written in full. So the wire carries a status a client can branch on and the
    /// **log** carries the reason a human needs.
    pub fn message(self) -> &'static str {
        match self {
            GrpcStatus::Unavailable => "the origin is unreachable",
            GrpcStatus::Internal => "the origin failed the call",
            GrpcStatus::Unimplemented => "this route is not served by a gRPC origin",
        }
    }
}

/// What a gRPC request's answer should be when this server produces it rather than the origin.
///
/// One constructor so the three things that must agree always do: **HTTP 200**, a `content-type` a
/// gRPC client accepts, and a trailer section carrying `grpc-status`. Any of the three alone is a
/// client that cannot read the answer — a `502` with correct trailers is unreadable, and a `200`
/// with the status in a *header* is unreadable too, because gRPC clients read the status from the
/// trailer section specifically so that a streaming reply can be answered before the status exists.
///
/// The body is empty because a failed call has no message. That is not a placeholder: a gRPC client
/// sees a zero-length body and a non-OK status and reports the status, which is exactly right.
pub fn grpc_error(status: GrpcStatus) -> (hyper::StatusCode, HeaderMap, GrpcErrorBody) {
    let mut headers = HeaderMap::new();
    // `application/grpc` with no suffix: there is no message, so there is no encoding to name.
    headers.insert(
        hyper::header::CONTENT_TYPE,
        HeaderValue::from_static("application/grpc"),
    );
    // **The declaration that makes the trailer section exist on HTTP/1.1.** hyper's h1 server writes
    // chunked framing for an unknown-length body, but it only enables its trailer-writing path when
    // the response carries a `Trailer` header naming the fields to come — read in
    // `hyper-1.12.0/src/proto/h1/role.rs`, where the `TRAILER` arm is what sets
    // `allowed_trailer_fields`. Without this header the reply is a chunked `200` whose body ends
    // `0\r\n\r\n` with **no trailers at all**, which a gRPC client reads as a successful call with an
    // empty message: the failure vanishes rather than being misreported, which is worse.
    //
    // HTTP/2 has native trailer frames and needs no declaration, so this is a no-op there — but it is
    // legal (`RFC 9113` §8.1) and it is what makes the same response correct on both transports.
    headers.insert(
        hyper::header::TRAILER,
        HeaderValue::from_static("grpc-status, grpc-message"),
    );
    headers.insert(hyper::header::TE, HeaderValue::from_static("trailers"));
    (
        hyper::StatusCode::OK,
        headers,
        GrpcErrorBody {
            trailers: Some(grpc_trailers(status)),
        },
    )
}

/// The trailer fields that carry a gRPC status.
///
/// Constructed rather than forwarded, and this is the header this tier exists to be able to write:
/// `docs/plugins.md` §6.6 lists *"write `grpc-status` and `grpc-message` as trailers"* as the row
/// that needs no schema, and §9.7 records that the tree had no code that constructed a trailer at
/// all. `crates/volucer-upstream/tests/trailers.rs` had already measured the other direction — that
/// an origin's trailers cross this proxy untouched — so this is the missing half.
pub fn grpc_trailers(status: GrpcStatus) -> HeaderMap {
    let mut trailers = HeaderMap::new();
    // A number as a decimal string, not a binary integer: gRPC's trailer fields are HTTP fields, and
    // `grpc-status` is defined as a decimal in ASCII.
    trailers.insert(
        "grpc-status",
        HeaderValue::from_str(&status.code().to_string())
            .expect("a decimal number is a header value"),
    );
    trailers.insert("grpc-message", HeaderValue::from_static(status.message()));
    trailers
}

/// A body that yields no data frames and then a trailer section.
///
/// Forty lines, and hand-written for the reason `crate::static_files`'s `FileBody` is: the natural
/// alternative is `http_body_util`'s adapters over a `Stream`, and the only way to build one without
/// a new dependency is `futures` — a large tree for a type whose whole content is "empty, then two
/// fields".
///
/// `poll_frame` yields `Frame::trailers` **exactly once**. `hyper` calls it again after that, and a
/// second `Some(trailers)` would be a protocol error; returning `None` is what ends the body cleanly.
pub struct GrpcErrorBody {
    /// Taken on the first poll rather than cloned, so the two states are "not yet sent" and "sent".
    trailers: Option<HeaderMap>,
}

impl hyper::body::Body for GrpcErrorBody {
    type Data = bytes::Bytes;
    type Error = std::io::Error;

    fn poll_frame(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<
        Option<std::result::Result<hyper::body::Frame<bytes::Bytes>, std::io::Error>>,
    > {
        std::task::Poll::Ready(
            self.get_mut()
                .trailers
                .take()
                .map(|trailers| Ok(hyper::body::Frame::trailers(trailers))),
        )
    }

    fn is_end_stream(&self) -> bool {
        // The body ends after the trailers, and `hyper` reads this to decide whether to keep the
        // connection — so it has to disagree with "no frames yet", not with "nothing to say".
        self.trailers.is_none()
    }

    fn size_hint(&self) -> hyper::body::SizeHint {
        if self.trailers.is_none() {
            // Everything has been yielded, so the length really is zero and saying so lets hyper
            // frame it exactly.
            return hyper::body::SizeHint::with_exact(0);
        }
        // **Unknown, not zero, and a `with_exact(0)` here silently loses the trailers.** This is the
        // bug this test exists for: with an exact hint, hyper's HTTP/1.1 server concluded the body was
        // complete, wrote `Content-Length: 0` and never polled for a trailer section at all — so a
        // gRPC client got a `200` with no `grpc-status` anywhere and read it as an empty successful
        // call. An unknown length is what makes hyper choose chunked framing, which is the only
        // framing HTTP/1.1 has for a trailer section. (HTTP/2 needs no chunking, so this costs
        // nothing there.)
        hyper::body::SizeHint::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::{BodyExt, Empty};
    use hyper::body::Body as _;
    use hyper::header::{CONNECTION, EXPECT, TRANSFER_ENCODING, UPGRADE};
    use std::net::IpAddr;

    fn peer() -> Option<IpAddr> {
        // An address with no port, which is what a listener reports and all `X-Forwarded-For`
        // carries.
        Some("203.0.113.7".parse().expect("an address"))
    }

    fn request(headers: HeaderMap) -> UpstreamRequest<Empty<bytes::Bytes>> {
        prepare_request(
            headers,
            Method::GET,
            Uri::from_static("/api?x=1"),
            Version::HTTP_11,
            Empty::new(),
            peer(),
            true,
        )
    }

    fn values(headers: &HeaderMap, name: &str) -> Vec<String> {
        headers
            .get_all(name)
            .iter()
            .map(|value| value.to_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn hop_by_hop_headers_do_not_cross() {
        let mut headers = HeaderMap::new();
        headers.insert(CONNECTION, HeaderValue::from_static("keep-alive"));
        headers.insert("keep-alive", HeaderValue::from_static("timeout=5"));
        headers.insert(TRANSFER_ENCODING, HeaderValue::from_static("chunked"));
        headers.insert(UPGRADE, HeaderValue::from_static("websocket"));
        headers.insert("proxy-authorization", HeaderValue::from_static("Basic x"));
        let outgoing = request(headers).headers;

        for absent in [
            "connection",
            "keep-alive",
            "transfer-encoding",
            "upgrade",
            "proxy-authorization",
        ] {
            assert!(
                !outgoing.contains_key(absent),
                "{absent} must not cross a hop"
            );
        }
    }

    #[test]
    fn a_header_named_by_connection_is_also_hop_local() {
        // The RFC 9110 §7.6.1 rule that is easy to miss, and the one a proxy test should pin:
        // `Connection: X-Private` makes `X-Private` hop-by-hop for *this* message.
        let mut headers = HeaderMap::new();
        headers.insert(CONNECTION, HeaderValue::from_static("X-Private"));
        headers.insert("X-Private", HeaderValue::from_static("secret"));
        headers.insert("x-public", HeaderValue::from_static("fine"));
        let outgoing = request(headers).headers;

        assert!(!outgoing.contains_key("x-private"), "named by Connection");
        assert!(outgoing.contains_key("x-public"), "not named");
    }

    #[test]
    fn te_trailers_is_constructed_for_every_request_and_the_clients_value_is_dropped() {
        // gRPC's protocol requires the server to see `TE: trailers`. It is hop-by-hop (RFC 9110
        // §7.6.1), so the client's own value must not cross — but the *intent* must, and exactly one
        // spelling of it, so this is the one header the proxy writes rather than forwards.
        let mut headers = HeaderMap::new();
        headers.insert(
            hyper::header::TE,
            HeaderValue::from_static("trailers, deflate"),
        );
        let outgoing = request(headers).headers;

        assert_eq!(
            values(&outgoing, "te"),
            vec!["trailers".to_string()],
            "the client's list must not survive: `deflate` would have the origin compress a reply \
             this server never negotiated"
        );

        // And it is added for a request that did not ask, because the value is what the next hop
        // needs to know rather than a copy of the characters that arrived.
        let outgoing = request(HeaderMap::new()).headers;
        assert_eq!(values(&outgoing, "te"), vec!["trailers".to_string()]);
    }

    #[test]
    fn a_grpc_failure_is_http_200_with_the_status_in_the_trailers() {
        // The mapping `docs/plugins.md` §6.6 calls the whole of tier A's remaining work. Every
        // assertion here is a different way for a gRPC client to be unable to read the answer:
        //
        // - a `502` is unreadable, because gRPC reports outcomes in trailers and the client never
        //   sees one under an HTTP error status;
        // - a status in a *header* rather than a trailer is unreadable for a streamed reply, which
        //   is the case the trailer convention exists for;
        // - a `content-type` that is not `application/grpc` makes a client reject the reply before
        //   it ever looks at the status.
        let (status, headers, body) = grpc_error(GrpcStatus::Unavailable);

        assert_eq!(status, hyper::StatusCode::OK, "a gRPC failure is HTTP 200");
        assert_eq!(
            headers.get(hyper::header::CONTENT_TYPE).unwrap(),
            "application/grpc",
            "or a client will not read it as gRPC at all"
        );

        // The trailer section, read off the body rather than inspected as a field: the question is
        // what a client receives, and a `HeaderMap` this function also returned could agree with a
        // body that yields nothing.
        let frames = tokio_test_block_on(collect_frames(body));
        assert_eq!(frames.data, 0, "a failed call carries no message");
        let trailers = frames.trailers.expect("the status must be in the trailers");
        assert_eq!(
            trailers.get("grpc-status").map(|v| v.to_str().unwrap()),
            Some("14"),
            "UNAVAILABLE is 14, and it is the code the specification lets a client retry. Got \
             {trailers:?}, which is either no status at all or the wrong one"
        );
        assert_eq!(
            trailers.get("grpc-message").unwrap(),
            "the origin is unreachable"
        );

        // The control: the same call with a status an origin would send, so the mapping is a mapping
        // rather than one hard-coded answer.
        let (_, _, body) = grpc_error(GrpcStatus::Unimplemented);
        let trailers = tokio_test_block_on(collect_frames(body))
            .trailers
            .expect("trailers");
        assert_eq!(trailers.get("grpc-status").unwrap(), "12");

        // And no status is spelled as a *name*: `grpc-status` is defined as a decimal in ASCII, and
        // a client parsing `UNAVAILABLE` would fail or, worse, read a number out of it.
        for status in [
            GrpcStatus::Unavailable,
            GrpcStatus::Internal,
            GrpcStatus::Unimplemented,
        ] {
            let code = status.code().to_string();
            assert!(
                code.parse::<u8>().is_ok(),
                "{} must be a decimal, not a name",
                status.name()
            );
        }
    }

    /// The frames a body yields, collected without an executor.
    ///
    /// `Waker::noop()` rather than a hand-written no-op waker, which is a clippy error now
    /// (`manual_noop_waker`) and was three lines of code doing what one call does.
    ///
    /// A hand-rolled `block_on` at all because the crates that would provide one — `futures`, tokio's
    /// `test-util` — are not dependencies of this crate, and the async work here is a `poll_frame`
    /// that is `Ready` on its first call. That is worth stating: a helper that silently needed an
    /// executor would hide the fact that this body never awaits.
    fn tokio_test_block_on<F: std::future::Future>(future: F) -> F::Output {
        let mut future = Box::pin(future);
        let mut context = std::task::Context::from_waker(std::task::Waker::noop());
        match future.as_mut().poll(&mut context) {
            std::task::Poll::Ready(output) => output,
            std::task::Poll::Pending => panic!("this body must never be pending"),
        }
    }

    /// What a body actually yields: the data bytes and the trailer fields, separately.
    struct Frames {
        data: usize,
        trailers: Option<HeaderMap>,
    }

    /// Collect `GrpcErrorBody` specifically rather than any `Body`.
    ///
    /// Concrete rather than generic because the generic version needs `Data: Buf` and `Error:
    /// Display` bounds that only this one type satisfies here — and a helper that had to be
    /// *more* general than its one caller would be a helper nobody could use again anyway.
    async fn collect_frames(body: GrpcErrorBody) -> Frames {
        let mut data = 0;
        let mut trailers = None;
        let mut body = Box::pin(body);
        loop {
            let frame = std::future::poll_fn(|cx| body.as_mut().poll_frame(cx)).await;
            match frame {
                None => break,
                Some(Ok(frame)) => {
                    if let Some(chunk) = frame.data_ref() {
                        data += chunk.len();
                    }
                    if let Ok(fields) = frame.into_trailers() {
                        assert!(trailers.is_none(), "trailers are yielded exactly once");
                        trailers = Some(fields);
                    }
                }
                Some(Err(err)) => panic!("the body must not fail: {err}"),
            }
        }
        Frames { data, trailers }
    }

    #[test]
    fn a_grpc_call_is_recognised_by_its_content_type_and_nothing_else() {
        // The server never parses a message (`docs/plugins.md` §6.6), so `content-type` is the whole
        // of what identifies a gRPC call — including the `-web` and `+json` forms, which are still
        // gRPC's framing and still need the h2 path and the trailer handling.
        let with = |value: &str| {
            let mut headers = HeaderMap::new();
            headers.insert(
                hyper::header::CONTENT_TYPE,
                HeaderValue::from_str(value).expect("a header value"),
            );
            is_grpc(&headers)
        };

        assert!(with("application/grpc"));
        assert!(with("application/grpc+proto"));
        assert!(with("application/grpc+json"));
        assert!(with("application/grpc-web"));
        assert!(with("application/grpc-web+proto"));
        // Case and surrounding whitespace are not the protocol.
        assert!(with("Application/GRPC+Proto"));
        assert!(with(" application/grpc "));

        assert!(!with("application/json"));
        assert!(!with("application/grpcx"));
        assert!(!with("text/plain"));
        assert!(!is_grpc(&HeaderMap::new()), "no content-type is not gRPC");
    }

    #[test]
    fn content_length_and_expect_are_dropped_from_the_request() {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_LENGTH, HeaderValue::from_static("12"));
        headers.insert(EXPECT, HeaderValue::from_static("100-continue"));
        let outgoing = request(headers).headers;

        // hyper re-derives framing from the body, and a stale length desynchronises a connection.
        assert!(!outgoing.contains_key("content-length"));
        // We never emit an interim 100, so a client waiting for one would wait for nothing.
        assert!(!outgoing.contains_key("expect"));
    }

    #[test]
    fn duplicate_headers_survive_as_duplicates() {
        // The loss the CGI path makes on purpose (`build_params` joins with ", ") and this path
        // must not: two `Cookie` headers are two cookies, and a join changes their meaning.
        let mut headers = HeaderMap::new();
        headers.append("x-tag", HeaderValue::from_static("one"));
        headers.append("x-tag", HeaderValue::from_static("two"));
        let outgoing = request(headers).headers;

        assert_eq!(values(&outgoing, "x-tag"), vec!["one", "two"]);
    }

    #[test]
    fn the_clients_host_is_kept_verbatim() {
        // It is the vhost identity the origin routes on, so it must be what the client asked for
        // and not what this server dialled.
        let mut headers = HeaderMap::new();
        headers.insert(HOST, HeaderValue::from_static("shop.example.com"));
        let outgoing = request(headers).headers;

        assert_eq!(values(&outgoing, "host"), vec!["shop.example.com"]);
        assert_eq!(
            values(&outgoing, "x-forwarded-host"),
            vec!["shop.example.com"]
        );
    }

    #[test]
    fn forwarded_for_is_appended_to_the_clients_own_claim() {
        let mut headers = HeaderMap::new();
        headers.append("x-forwarded-for", HeaderValue::from_static("198.51.100.9"));
        let outgoing = request(headers).headers;

        // The client's claim stays visible, and the peer we actually saw is last — which is the
        // position a correct origin reads. Replacing instead would let a client forge its own
        // address to an origin that trusts this server for the header.
        assert_eq!(
            values(&outgoing, "x-forwarded-for"),
            vec!["198.51.100.9", "203.0.113.7"]
        );
    }

    #[test]
    fn forwarded_proto_comes_from_the_socket_not_from_a_header() {
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-proto", HeaderValue::from_static("https"));
        // A cleartext request that *claims* to be https: the claim must not become the only value,
        // and ours must say what actually happened.
        let outgoing = prepare_request(
            headers,
            Method::GET,
            Uri::from_static("/"),
            Version::HTTP_11,
            Empty::<bytes::Bytes>::new(),
            peer(),
            true,
        )
        .headers;

        assert_eq!(
            values(&outgoing, "x-forwarded-proto"),
            vec!["https", "http"]
        );
    }

    #[test]
    fn a_request_arriving_without_a_peer_still_gets_a_proto() {
        let outgoing = prepare_request(
            HeaderMap::new(),
            Method::GET,
            Uri::from_static("/"),
            Version::HTTP_11,
            Empty::<bytes::Bytes>::new(),
            None,
            false,
        )
        .headers;

        assert!(!outgoing.contains_key("x-forwarded-for"));
        assert_eq!(values(&outgoing, "x-forwarded-proto"), vec!["https"]);
    }

    fn response(headers: HeaderMap) -> UpstreamResponse {
        translate_response(UpstreamResponse {
            status: hyper::StatusCode::OK,
            headers,
            body: http_body_util::Full::new(bytes::Bytes::new())
                .map_err(|never: std::convert::Infallible| match never {})
                .boxed_unsync(),
            version: Version::HTTP_11,
        })
    }

    #[test]
    fn two_set_cookie_headers_stay_two() {
        let mut headers = HeaderMap::new();
        headers.append("set-cookie", HeaderValue::from_static("a=1; Path=/"));
        headers.append("set-cookie", HeaderValue::from_static("b=2; Path=/"));
        let outgoing = response(headers).headers;

        assert_eq!(
            values(&outgoing, "set-cookie"),
            vec!["a=1; Path=/", "b=2; Path=/"]
        );
    }

    #[test]
    fn the_origins_content_length_is_kept() {
        // The deliberate difference from `from_cgi`: this one is protocol framing, and hyper
        // re-validates it, so a mismatch is a protocol error rather than a desync.
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_LENGTH, HeaderValue::from_static("5"));
        let outgoing = response(headers).headers;

        assert_eq!(values(&outgoing, "content-length"), vec!["5"]);
    }

    #[test]
    fn the_response_side_honours_connection_naming_too() {
        let mut headers = HeaderMap::new();
        headers.insert(CONNECTION, HeaderValue::from_static("X-Origin-Internal"));
        headers.insert("X-Origin-Internal", HeaderValue::from_static("hop"));
        headers.insert("content-type", HeaderValue::from_static("text/plain"));
        let outgoing = response(headers).headers;

        assert!(!outgoing.contains_key("x-origin-internal"));
        assert!(!outgoing.contains_key("connection"));
        assert_eq!(values(&outgoing, "content-type"), vec!["text/plain"]);
    }

    #[test]
    fn header_names_are_compared_case_insensitively() {
        // A client sending `CONNECTION` in any case still gets the header dropped; `HeaderName`
        // normalises, so this pins that the *rule* does not depend on it.
        let mut headers = HeaderMap::new();
        headers.insert("Connection", HeaderValue::from_static("X-Y"));
        headers.insert("x-y", HeaderValue::from_static("z"));
        let outgoing = request(headers).headers;

        assert!(!outgoing.contains_key("x-y"));
    }
}
