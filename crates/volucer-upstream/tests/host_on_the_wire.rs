//! What an origin is *told* the request is for, on each hop it can be spoken to in.
//!
//! A proxy forwards two different names to an origin and they are not interchangeable: the address
//! it dialled, and the name the client asked for. An origin that selects a virtual host — nginx's
//! `server_name`, Apache's `VirtualHost`, any multi-tenant application — routes on the second, so a
//! proxy that sends the first has told it the wrong thing.
//!
//! `prepare_request` forwards the client's `Host` header faithfully, and
//! `the_clients_host_is_kept_verbatim` in `translate.rs` pins that. What this file measures is the
//! *other* half, and it is the half a header-level unit test cannot see: **HTTP/2 carries the same
//! identity in a `:authority` pseudo-header, not in `Host`.** hyper's h2 client derives
//! `:authority` from the request URI's authority, and `client.rs`'s `destination` builds that from
//! the *origin* — so on an h2 hop the forwarded `Host` header is not what the origin routes on.
//!
//! Measured, so that neither behaviour is an inference:
//!
//! | hop | what the origin reads the vhost from | value |
//! |---|---|---|
//! | HTTP/1.1 | the `Host` header | the client's, verbatim |
//! | HTTP/2 | `:authority` | the origin's own authority |
//!
//! The fixture is a real `h2` server, which reports the `Request` it was handed. That is not a
//! stylistic choice: the first version of this file parsed the HEADERS frame itself and failed with
//! `StringDecodingError(NotEnoughOctets)`, because RFC 9113 removed the priority field RFC 7540 made
//! mandatory and the hard-coded offset was right for neither. A hand-written frame parser here would
//! be a second implementation of a specification, and it would be the thing under test rather than
//! volucer.
//!
//! Both hops are asserted in one file, and the HTTP/1.1 arm is the control. With only the h2 arm, a
//! client that never forwarded the client's `Host` at all would pass — which is the regression this
//! is most likely to catch, since a fix for the h2 arm lives in the same function that could break
//! the HTTP/1.1 one.

use std::io::Read;
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc::{channel, Receiver};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::Empty;
use volucer_upstream::{Origin, UpstreamClient, UpstreamRequest};

/// The name a client asked for, and the one it must not be replaced with.
const CLIENT_HOST: &str = "shop.example.com";

/// An HTTP/1.1 origin that records the head of the first request it is sent, then says nothing.
///
/// It deliberately does not answer: what is under test is what volucer *sent*, so a fixture that
/// replied would make the assertion depend on its own conformance rather than on volucer's.
struct RecordingOrigin {
    address: String,
    received: Receiver<Vec<u8>>,
    stop: Arc<std::sync::atomic::AtomicBool>,
}

impl RecordingOrigin {
    fn spawn() -> RecordingOrigin {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind an origin");
        listener
            .set_nonblocking(true)
            .expect("a non-blocking accept so this thread ends with the test");
        let address = listener.local_addr().expect("an address").to_string();
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (sender, received) = channel();

        let watching = Arc::clone(&stop);
        std::thread::spawn(move || loop {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let _ = stream.set_nonblocking(false);
                    let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
                    let _ = sender.send(read_http_1_1_head(&mut stream));
                }
                Err(ref err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    if watching.load(std::sync::atomic::Ordering::Relaxed) {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(_) => return,
            }
        });

        RecordingOrigin {
            address,
            received,
            stop,
        }
    }

    /// The first thing the origin was sent, waiting for it to arrive.
    fn first_request(&self, within: Duration) -> Vec<u8> {
        self.received
            .recv_timeout(within)
            .expect("the origin was sent something")
    }
}

impl Drop for RecordingOrigin {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

/// An **h2c** origin that reports the request it was handed and then says nothing.
///
/// A real `h2` server rather than a byte recorder, and that is a correction rather than a
/// preference. The first version of this file parsed the HEADERS frame itself and failed with
/// `StringDecodingError(NotEnoughOctets)`, because RFC 9113 removed the priority field that RFC 7540
/// made mandatory and a hand-written offset was wrong for one of them. `h2` is the implementation
/// hyper itself uses, so asking it for the decoded `Request` is the only version of this fixture
/// that can be wrong about volucer rather than about the specification.
struct H2Origin {
    address: String,
    received: Receiver<hyper::Request<()>>,
    stop: Arc<std::sync::atomic::AtomicBool>,
}

impl H2Origin {
    fn spawn() -> H2Origin {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind an h2 origin");
        listener
            .set_nonblocking(true)
            .expect("a non-blocking accept so this thread ends with the test");
        let address = listener.local_addr().expect("an address").to_string();
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (sender, received) = channel();

        let watching = Arc::clone(&stop);
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("a runtime for the origin");
            runtime.block_on(async move {
                let listener =
                    tokio::net::TcpListener::from_std(listener).expect("adopt the listener");
                loop {
                    if watching.load(std::sync::atomic::Ordering::Relaxed) {
                        return;
                    }
                    let accepted =
                        tokio::time::timeout(Duration::from_millis(50), listener.accept()).await;
                    let Ok(Ok((stream, _))) = accepted else {
                        continue;
                    };

                    let mut connection = match h2::server::handshake(stream).await {
                        Ok(connection) => connection,
                        // A client that reset before the handshake finished is the test's own
                        // timeout, not a failure of what is being measured.
                        Err(_) => continue,
                    };
                    let sender = sender.clone();
                    tokio::spawn(async move {
                        // One request is the whole capture. The response is never sent: the client
                        // is expected to give up, and answering would change nothing it asserts —
                        // an `if let` rather than a `while` because exactly one is wanted.
                        if let Some(Ok((request, _respond))) = connection.accept().await {
                            let mut head = hyper::Request::builder()
                                .method(request.method().clone())
                                .uri(request.uri().clone())
                                .version(request.version())
                                .body(())
                                .expect("a copy of the request's head");
                            // The header map, which is the half under test. Without this the
                            // fixture reports an empty list and every assertion about a forwarded
                            // header fails for a reason that has nothing to do with volucer.
                            *head.headers_mut() = request.headers().clone();
                            let _ = sender.send(head);
                        }
                    });
                }
            });
        });

        H2Origin {
            address,
            received,
            stop,
        }
    }

    /// The `:authority` the client stated, waiting for the request to arrive.
    ///
    /// `Request::uri().authority()` is the decoded `:authority` pseudo-header: `h2` parses the
    /// pseudo-headers into the URI and does not leave them in the header map, which is why the
    /// assertion reads the URI rather than a header.
    fn authority(&self, within: Duration) -> (String, Vec<(String, String)>) {
        let head = self
            .received
            .recv_timeout(within)
            .expect("the origin was sent a request");
        let authority = head
            .uri()
            .authority()
            .unwrap_or_else(|| panic!("an h2 request must carry `:authority`: {:?}", head.uri()))
            .to_string();
        let fields = head
            .headers()
            .iter()
            .map(|(name, value)| {
                (
                    name.as_str().to_string(),
                    String::from_utf8_lossy(value.as_bytes()).into_owned(),
                )
            })
            .collect();
        (authority, fields)
    }
}

impl Drop for H2Origin {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

/// The value a header field carried, if it was sent.
fn field<'a>(fields: &'a [(String, String)], name: &str) -> Option<&'a str> {
    fields
        .iter()
        .find(|(field, _)| field == name)
        .map(|(_, value)| value.as_str())
}

/// A request as a client's hop would have built it: origin-form, with the client's own `Host`.
fn client_request() -> UpstreamRequest<Empty<Bytes>> {
    let mut headers = hyper::HeaderMap::new();
    headers.insert(hyper::header::HOST, CLIENT_HOST.parse().unwrap());
    headers.insert(
        hyper::header::ACCEPT,
        hyper::header::HeaderValue::from_static("text/plain"),
    );
    UpstreamRequest {
        method: hyper::Method::GET,
        uri: "/anything".parse().unwrap(),
        version: hyper::Version::HTTP_11,
        headers,
        body: Empty::<Bytes>::new(),
    }
}

/// Read one HTTP/1.1 request's head from a blocking socket.
///
/// Stops at the blank line: enough of the request line and headers to read `Host`, and nothing
/// after it matters.
fn read_http_1_1_head(stream: &mut TcpStream) -> Vec<u8> {
    let mut collected = Vec::new();
    let mut buffer = [0u8; 4096];
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while std::time::Instant::now() < deadline {
        match stream.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => collected.extend_from_slice(&buffer[..read]),
            Err(_) => break,
        }
        if collected.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }
    collected
}

/// Send one client request through the real client to an origin at `address`.
fn speak(address: &str, h2c: bool, preserve_host: bool) {
    let spec = format!("http://{address}");
    let protocol = h2c.then_some(volucer_core::config::UpstreamProtocol::H2c);
    let parsed = volucer_core::config::OriginSpec::parse_with(&spec, None, protocol)
        .expect("the origin parses")
        .with_preserve_host(preserve_host);
    let origin = Origin::from_spec(&parsed).expect("a valid origin");
    let client: UpstreamClient<Empty<Bytes>> =
        UpstreamClient::new(origin, None).expect("the client builds");

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime");
    // The origin never answers, so this is expected to fail. What matters is what was sent, which
    // the fixture has already captured by the time the call gives up.
    let _ = runtime.block_on(async {
        tokio::time::timeout(Duration::from_secs(3), client.request(client_request())).await
    });
}

/// **The control.** On an HTTP/1.1 hop the origin reads the client's `Host`, verbatim.
///
/// This is the behaviour that already worked, asserted here so the h2 arms below are a comparison
/// rather than a claim on their own. Same fixture, same request, and `preserve_host` on as well as
/// off: on this hop the key is a no-op, and a regression that made it rewrite the header would show
/// up here.
#[test]
fn an_http_1_1_hop_forwards_the_clients_host_header() {
    for preserve_host in [false, true] {
        let origin = RecordingOrigin::spawn();
        let address = origin.address.clone();
        let caller = std::thread::spawn(move || speak(&address, false, preserve_host));

        let sent = origin.first_request(Duration::from_secs(10));
        caller.join().expect("the client thread");

        let text = String::from_utf8_lossy(&sent);
        let host = text
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("host")
                    .then(|| value.trim().to_string())
            })
            .unwrap_or_else(|| panic!("the request has no Host header: {text:?}"));

        assert_eq!(
            host, CLIENT_HOST,
            "an HTTP/1.1 origin routes on the Host header, so it must be the name the client asked \
             for and not the address this server dialled (preserve_host = {preserve_host}).\n\
             what was sent:\n{text}"
        );
    }
}

/// **The default, measured.** On an HTTP/2 hop the origin reads `:authority`, and without
/// `preserve_host` it is the origin's own — not the client's `Host`.
///
/// This is the divergence, recorded rather than fixed, because it is what every deployment had
/// before the key existed and changing it unasked would break the configuration that wants it.
#[test]
fn an_h2_hop_carries_the_origins_authority_by_default() {
    let origin = H2Origin::spawn();
    let address = origin.address.clone();
    let dialled = address.clone();
    let caller = std::thread::spawn(move || speak(&dialled, true, false));

    let (authority, fields) = origin.authority(Duration::from_secs(10));
    caller.join().expect("the client thread");

    assert_eq!(
        authority, address,
        "HTTP/2 carries the vhost identity in `:authority`, and by default `destination` builds it \
         from the dialled origin. Headers the origin decoded:\n{fields:#?}"
    );
    // **Both facts at once, and that is the whole difficulty.** The forwarded `Host` header *is* the
    // client's — `prepare_request` is correct — but on an h2 hop it sits beside a `:authority` that
    // says something else, and RFC 9113 §8.3.1 makes `:authority` the authoritative one. So the
    // header-level unit test in `translate.rs` cannot see this: it asserts a true statement about a
    // header an h2 origin is entitled to ignore.
    assert_eq!(
        field(&fields, "host"),
        Some(CLIENT_HOST),
        "the forwarded `Host` header must still be the client's — if this failed, the h2 divergence \
         would be a *forwarding* bug rather than a pseudo-header one, which is a different fix. \
         Headers the origin decoded:\n{fields:#?}"
    );
}

/// **`preserve_host = true` fixes the h2 hop, and leaves the header alone.**
///
/// The whole point of the key, asserted on the same fixture as the default above: the `:authority`
/// becomes the name the client asked for, which is what a vhost origin routes on.
///
/// That it is still the client's *own* name and not, say, the origin's is what makes this a
/// virtual-hosting fix rather than a rename.
#[test]
fn preserve_host_puts_the_clients_name_in_the_h2_authority() {
    let origin = H2Origin::spawn();
    let address = origin.address.clone();
    let dialled = address.clone();
    let caller = std::thread::spawn(move || speak(&dialled, true, true));

    let (authority, fields) = origin.authority(Duration::from_secs(10));
    caller.join().expect("the client thread");

    assert_eq!(
        authority, CLIENT_HOST,
        "with `preserve_host = true` the `:authority` must be the name the client asked for, \
         which is what an origin that selects a virtual host routes on. Headers the origin \
         decoded:\n{fields:#?}"
    );
    assert_eq!(
        field(&fields, "host"),
        Some(CLIENT_HOST),
        "and the forwarded `Host` header agrees with it, so an origin that reads either sees one \
         name. A disagreement here is the RFC 9113 §8.3.1 case an origin may reject outright."
    );
}
