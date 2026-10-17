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

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::{BodyExt, Empty};
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
