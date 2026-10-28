//! HTTP/3, driven by a real QUIC client against a real UDP listener.
//!
//! A transport cannot be asserted from the server side alone — the whole point of the layer is
//! what a peer receives — so these tests start a real [`RunningHttp3`] and speak real HTTP/3 to
//! it, the same rule `tls.rs` follows for the TLS handshake.
//!
//! **The amplification test is the reason this file exists.** `h3` 0.0.8 defaults
//! `max_field_section_size` to `VarInt::MAX` (hyperium/h3#379), which is a remotely triggerable
//! memory amplification. The mitigation is a configured cap, and a mitigation nobody exercises is
//! an assertion — so `an_oversized_header_section_is_answered_431_and_the_listener_survives`
//! sends a section past the cap and asserts the server refuses it *and* serves the next request.
//!
//! Certificates are generated at test time rather than committed, for the reason `tls.rs` gives: a
//! committed key ends up in someone's deployment.

#![cfg(feature = "http3")]

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use bytes::Buf;
use volucer_core::config::Config;
use volucer_core::sites::Sites;
use volucer_http::{
    Http3Listener, HttpLimits, ListenerSettings, ReloadHandles, RunningHttp3, TlsSettings,
};
use volucer_server::Dispatcher;

/// A certificate pair on disk, named after the host it is for. The fixture `tls.rs` uses.
struct Cert {
    dir: PathBuf,
    /// The DER of the leaf, so a client can be told to trust exactly this one.
    leaf: Vec<u8>,
}

impl Cert {
    fn new(label: &str, name: &str) -> Self {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("volucer-h3-{label}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("certs")).expect("create fixture dir");

        let generated = rcgen::generate_simple_self_signed(vec![name.to_string()])
            .expect("generate a self-signed certificate");
        let leaf = generated.cert.der().to_vec();
        std::fs::write(dir.join("certs/server.crt"), generated.cert.pem()).expect("write cert");
        std::fs::write(
            dir.join("certs/server.key"),
            generated.key_pair.serialize_pem(),
        )
        .expect("write key");

        Cert { dir, leaf }
    }

    fn dir(&self) -> &Path {
        &self.dir
    }

    fn leaf(&self) -> &[u8] {
        &self.leaf
    }
}

impl Drop for Cert {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// A config with an `[server.http3]` table, on loopback port 0 so tests can run in parallel.
///
/// Written to disk because certificate paths resolve against the config file's directory, which is
/// part of what the listener under test does.
///
/// `serve_only = "static"` because there is no backend in this fixture: with the default (`all`) a
/// path naming no file falls through to a dispatch that cannot happen, which is a 500 — correct
/// behaviour, but not what these tests are about.
fn write_config(cert: &Cert, http3_keys: &str) -> Config {
    let text = format!(
        r#"
[server]
http = "127.0.0.1:0"
document_root = '{}'
serve_only = "static"

[server.tls]
certificate = "certs/server.crt"
key = "certs/server.key"

[server.http3]
listen = "127.0.0.1:0"
{http3_keys}
"#,
        cert.dir().display()
    );
    std::fs::write(cert.dir().join("volucer.toml"), &text).expect("write config");
    let config = Config::from_toml(&text).expect("config parses");
    config.validate().expect("config validates");
    config
}

/// Start an HTTP/3 listener on its own runtime thread and return a handle to it.
///
/// `RunningHttp3` rather than a bare `Http3Listener` because that is the shape `serve` uses — own
/// runtime, own thread — so the test exercises the startup path an operator gets.
fn start(config: &Config, cert: &Cert) -> RunningHttp3 {
    let dir = cert.dir();
    let dispatcher = Arc::new(
        Dispatcher::from_config(config, volucer_fcgi::Routing::default()).expect("dispatcher"),
    );
    let registry = Arc::new(volucer_http::Registry::new(volucer_http::SharedState::new(
        Sites::from_config(config, dir),
    )));
    let resolver = Arc::new(volucer_http::SniResolver::from_config(config, dir).expect("resolver"));

    volucer_http::tls::install_crypto_provider();

    let address = config
        .server
        .http3
        .as_ref()
        .expect("the table is in the config")
        .listen
        .clone();

    RunningHttp3::start(
        &address,
        dispatcher,
        ReloadHandles::new(registry).with_tls(resolver),
        HttpLimits {
            max_connections: 8,
            max_body_bytes: 1024,
        },
        ListenerSettings {
            tls: Some(TlsSettings {
                config,
                config_dir: dir,
            }),
            https_port: None,
            alt_svc_port: None,
        },
    )
    .expect("bind the HTTP/3 listener")
}

/// The `quinn` client configuration a test uses: trust exactly the fixture's leaf, ALPN `h3`.
fn client_config(cert: &Cert) -> quinn::ClientConfig {
    let mut roots = quinn::rustls::RootCertStore::empty();
    roots
        .add(quinn::rustls::pki_types::CertificateDer::from(
            cert.leaf().to_vec(),
        ))
        .expect("trust the fixture certificate");

    let mut tls = quinn::rustls::ClientConfig::builder_with_protocol_versions(&[
        &quinn::rustls::version::TLS13,
    ])
    .with_root_certificates(roots)
    .with_no_client_auth();
    tls.alpn_protocols = vec![b"h3".to_vec()];

    let crypto = quinn::crypto::rustls::QuicClientConfig::try_from(tls)
        .expect("a TLS 1.3 client config converts");
    quinn::ClientConfig::new(Arc::new(crypto))
}

/// The h3 client connection and its request sender, named once so `Client` can hold them.
///
/// **One `h3::client::new` per QUIC connection, held for the connection's life.** The call opens
/// the HTTP/3 control streams, so making it per request is invalid — a second call on the same QUIC
/// connection is answered with `H3_STREAM_CREATION_ERROR`, which is exactly what this scaffolding
/// got wrong first.
type H3Connection = h3::client::Connection<h3_quinn::Connection, bytes::Bytes>;
type H3SendRequest = h3::client::SendRequest<h3_quinn::OpenStreams, bytes::Bytes>;

/// One HTTP/3 client: an endpoint, one connection, and the h3 layer over it.
struct Client {
    endpoint: quinn::Endpoint,
    connection: quinn::Connection,
    /// Held so the HTTP/3 control streams stay open for the test's duration: dropping this ends
    /// every request on the connection, so it must outlive them.
    _h3: H3Connection,
    send_request: H3SendRequest,
}

impl Client {
    async fn connect(cert: &Cert, server: SocketAddr) -> Self {
        let endpoint =
            quinn::Endpoint::client("127.0.0.1:0".parse().expect("a loopback client address"))
                .expect("client endpoint");

        let connection = endpoint
            .connect_with(client_config(cert), server, "a.test")
            .expect("connect")
            .await
            .expect("the QUIC handshake completes");

        let (h3, send_request) = h3::client::new(h3_quinn::Connection::new(connection.clone()))
            .await
            .expect("h3 client");

        Client {
            endpoint,
            connection,
            // `h3` is held so the connection stays alive for the test's duration — dropping it
            // closes the control streams, which ends every request on this connection.
            _h3: h3,
            send_request,
        }
    }

    /// Send one request and read the whole answer: status, headers, body.
    ///
    /// **Sending is allowed to fail; receiving is not.** A server that answers early — a 413 for a
    /// body over the limit, a 431 for a header section past the ceiling — then declines the rest of
    /// that body, and RFC 9114 lets it do so with `STOP_SENDING`. `h3` reports that as
    /// `RemoteTerminate` on the *send* half, so `send_data` and `finish` can both fail for a request
    /// the server handled correctly.
    ///
    /// This was a flake rather than a theory: the assertion is on the 413, but the helper panicked
    /// on `finish` whenever the server's answer arrived before the client finished writing. On Linux
    /// and macOS the client won that race; on Windows it did not, so one platform went red at
    /// random.
    async fn request(&mut self, method: &str, path: &str, body: Option<&[u8]>) -> Answer {
        let mut stream = self
            .send_request
            .send_request(
                hyper::Request::builder()
                    .method(method)
                    .uri(format!("https://a.test{path}"))
                    .body(())
                    .expect("a valid request"),
            )
            .await
            .expect("send the request head");

        // Kept rather than discarded: if no response head arrives either, this is the only thing
        // that explains why, and a bare "a response head" would send a reader looking in the wrong
        // place.
        let mut send_error: Option<String> = None;
        if let Some(body) = body {
            if let Err(err) = stream.send_data(bytes::Bytes::copy_from_slice(body)).await {
                send_error = Some(format!("send the body: {err}"));
            }
        }
        if send_error.is_none() {
            if let Err(err) = stream.finish().await {
                send_error = Some(format!("finish the request: {err}"));
            }
        }

        let response = stream.recv_response().await.unwrap_or_else(|err| {
            panic!("no response head ({err}); the send half had {send_error:?}")
        });

        let mut collected = Vec::new();
        while let Some(mut chunk) = stream.recv_data().await.expect("a data frame") {
            while chunk.has_remaining() {
                let bytes = chunk.chunk().to_vec();
                collected.extend_from_slice(&bytes);
                chunk.advance(bytes.len());
            }
        }

        Answer {
            status: response.status().as_u16(),
            headers: response.headers().clone(),
            body: collected,
        }
    }

    fn close(self) {
        self.connection.close(0u32.into(), b"");
        self.endpoint.close(0u32.into(), b"");
    }
}

/// What a client read from one response.
struct Answer {
    status: u16,
    headers: hyper::HeaderMap,
    body: Vec<u8>,
}

impl Answer {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// Write a static file into the fixture's document root.
fn write_file(cert: &Cert, name: &str, contents: &str) {
    std::fs::write(cert.dir().join(name), contents).expect("write a static file");
}

/// The listener answers a real HTTP/3 request, with a `Date`, over the same static layer as TCP.
#[test]
fn an_http_3_request_is_answered_with_a_date_and_serves_a_static_file() {
    with_runtime(async {
        let cert = Cert::new("basic", "a.test");
        write_file(&cert, "index.html", "hello over quic");
        let config = write_config(&cert, "");
        let running = start(&config, &cert);
        let server = running.local_addr();

        let mut client = Client::connect(&cert, server).await;
        let answer = client.request("GET", "/index.html", None).await;

        assert_eq!(answer.status, 200, "body was {:?}", answer.text());
        assert_eq!(answer.text(), "hello over quic");
        // **`Date` is added by hand on this transport.** `hyper` adds it for h1/h2 and `h3` does
        // not, so without the injection the same response would differ by transport — a difference
        // a shared cache can see.
        assert!(
            answer.headers.contains_key(hyper::header::DATE),
            "an HTTP/3 response must carry `Date` exactly as an HTTP/1.1 one does: {:?}",
            answer.headers
        );

        client.close();
        running.stop();
    });
}

/// A response over QUIC must not advertise the endpoint it arrived on.
#[test]
fn an_http_3_response_does_not_carry_alt_svc() {
    with_runtime(async {
        let cert = Cert::new("alt-svc", "a.test");
        write_file(&cert, "index.html", "x");
        let config = write_config(&cert, "");
        let running = start(&config, &cert);

        let mut client = Client::connect(&cert, running.local_addr()).await;
        let answer = client.request("GET", "/index.html", None).await;
        assert!(
            !answer.headers.contains_key("alt-svc"),
            "an HTTP/3 response must not advertise the connection it arrived on: {:?}",
            answer.headers
        );

        client.close();
        running.stop();
    });
}

/// **The amplification test: an oversized header section is refused and the listener survives.**
///
/// This is the point of the file. `h3` 0.0.8 has no finite default for `max_field_section_size`
/// (hyperium/h3#379), so the mitigation is a configured cap plus the crate's own 431. If either
/// half regresses — the cap back to `VarInt::MAX`, or the refusal into a dropped stream — a small
/// request becomes a large allocation and nothing else in this tree would notice.
#[test]
fn an_oversized_header_section_is_answered_431_and_the_listener_survives() {
    with_runtime(async {
        let cert = Cert::new("oversized", "a.test");
        write_file(&cert, "index.html", "still alive");
        // 1 KiB cap: far below the 16 KiB default, so the test does not have to send 16 KiB to
        // cross it — and small enough that the assertion is about the *rule* rather than the
        // arithmetic.
        let config = write_config(&cert, "max_field_section_size = 1024\n");
        let running = start(&config, &cert);

        let mut client = Client::connect(&cert, running.local_addr()).await;

        // Many ordinary headers: RFC 9114 §4.2.2 charges each field at least 32 bytes, so this
        // crosses a 1 KiB budget long before the fields are materialised. 600 distinct names is
        // ~8 KiB of field bytes against a 1 KiB cap.
        let mut request = hyper::Request::builder()
            .method("GET")
            .uri("https://a.test/index.html")
            .body(())
            .expect("a valid request");
        for index in 0..600 {
            request.headers_mut().append(
                hyper::header::HeaderName::from_bytes(format!("x-pad-{index}").as_bytes())
                    .expect("a valid header name"),
                hyper::header::HeaderValue::from_static("0123456789abcdef"),
            );
        }

        // **The same `h3` layer the client already holds**, not a second one: a second
        // `h3::client::new` on one QUIC connection is a stream-creation error, which is why the
        // reusable half has to be taken by reference here rather than re-created.
        let outcome = async {
            let mut stream = client.send_request.send_request(request).await?;
            stream.finish().await?;
            let response = stream.recv_response().await?;
            Ok::<_, h3::error::StreamError>(response.status().as_u16())
        }
        .await;

        // Either shape is a correct refusal: `h3` answers 431 when it can, or it resets the stream.
        // What must **not** happen is a panic, or the request being served.
        match outcome {
            Ok(status) => assert_eq!(
                status, 431,
                "an oversized header section must be answered 431, not served"
            ),
            Err(err) => {
                let text = err.to_string();
                assert!(
                    text.contains("too big")
                        || text.contains("HeaderTooBig")
                        || text.contains("431"),
                    "the refusal must be the header-section rule, not something else: {text}"
                );
            }
        }

        // **And the listener is still up.** A mitigation that OOMs the process or panics a runtime
        // worker would pass a 431 assertion and fail the deployment, so the next request is the
        // assertion that matters.
        let answer = client.request("GET", "/index.html", None).await;
        assert_eq!(
            answer.status,
            200,
            "the listener must serve normally after refusing an oversized section: {:?}",
            answer.text()
        );
        assert_eq!(answer.text(), "still alive");

        client.close();
        running.stop();
    });
}

/// A body over `max_body_bytes` is refused with 413 rather than buffered.
#[test]
fn a_body_over_the_limit_is_refused_with_413() {
    with_runtime(async {
        let cert = Cert::new("toolarge", "a.test");
        write_file(&cert, "index.html", "x");
        // `start` sets `max_body_bytes: 1024`.
        let config = write_config(&cert, "");
        let running = start(&config, &cert);

        let mut client = Client::connect(&cert, running.local_addr()).await;
        let oversized = vec![b'x'; 4096];
        let answer = client
            .request("POST", "/index.html", Some(&oversized))
            .await;

        assert_eq!(
            answer.status,
            413,
            "a body past max_body_bytes must be refused, not buffered: {:?}",
            answer.text()
        );

        client.close();
        running.stop();
    });
}

/// A path naming no file and no backend is the honest 404 — proof the request travelled the same
/// static layer the TCP path uses rather than being refused by this listener.
#[test]
fn a_missing_file_is_a_404_rather_than_a_dropped_stream() {
    with_runtime(async {
        let cert = Cert::new("miss", "a.test");
        let config = write_config(&cert, "");
        let running = start(&config, &cert);

        let mut client = Client::connect(&cert, running.local_addr()).await;
        let answer = client.request("GET", "/nope.txt", None).await;

        assert_eq!(
            answer.status,
            404,
            "a missing file must be a 404: {:?}",
            answer.text()
        );

        client.close();
        running.stop();
    });
}

/// The QUIC config really is TLS 1.3 only, asserted by making the call `quinn` makes later.
///
/// The trap this guards: `QuicServerConfig::try_from` accepts a TLS-1.2-only config and
/// `quinn-proto` then `unwrap`s the version check inside `start_session` — so the mistake is a
/// panic per handshake, not a build error.
#[test]
fn a_quic_config_is_built_tls_13_only() {
    volucer_http::tls::install_crypto_provider();
    let cert = Cert::new("tls13", "a.test");
    let config = write_config(&cert, "");
    let resolver =
        Arc::new(volucer_http::SniResolver::from_config(&config, cert.dir()).expect("resolver"));

    let built = volucer_http::http3::server_config(resolver).expect("a QUIC config");
    assert_eq!(
        built.alpn_protocols,
        vec![b"h3".to_vec()],
        "QUIC's ALPN is `h3` by definition and is not read from [server.tls].alpn"
    );

    quinn::rustls::quic::ServerConnection::new(built, quinn::rustls::quic::Version::V1, Vec::new())
        .expect("a QUIC config built here must be TLS 1.3 capable");
}

/// **`Alt-Svc` reaches the TCP listener, and only over TLS.**
///
/// This is what makes the UDP endpoint discoverable at all — without the header no browser ever
/// tries QUIC, and the listener is "supported" rather than "used". The cleartext half is asserted
/// because a browser ignores `Alt-Svc` from a non-secure response, so writing it there is bytes
/// that teach nobody.
///
/// The port is supplied rather than discovered, because that is exactly what `serve` does: only a
/// bound socket knows its port, so the TCP listener is told. The value names the *UDP* endpoint, so
/// 8443 is not the port the header arrives on — that asymmetry is the point.
#[test]
fn alt_svc_is_sent_over_tls_and_never_over_cleartext() {
    with_runtime(async {
        let cert = Cert::new("altSvc", "a.test");
        let text = format!(
            r#"
[server]
http = "127.0.0.1:0"
document_root = '{}'
serve_only = "static"

[server.tls]
certificate = "certs/server.crt"
key = "certs/server.key"
plain = true

[server.http3]
listen = "127.0.0.1:8443"
alt_svc_ma_seconds = 3600
"#,
            cert.dir().display()
        );
        std::fs::write(cert.dir().join("volucer.toml"), &text).expect("write config");
        let config = Config::from_toml(&text).expect("config parses");
        config.validate().expect("config validates");
        write_file(&cert, "index.html", "x");

        let addr = start_tcp_with_alt_svc(&config, &cert, Some(8443)).await;

        let over_tls = fetch_over_tcp(addr, "a.test", true).await;
        assert!(
            over_tls.contains("alt-svc: h3=\":8443\"; ma=3600"),
            "a TLS answer must advertise the QUIC endpoint or no browser finds it: {over_tls}"
        );

        let cleartext = fetch_over_tcp(addr, "a.test", false).await;
        assert!(
            !cleartext.contains("alt-svc"),
            "a cleartext answer must not carry `Alt-Svc`: no browser acts on it there: {cleartext}"
        );
    });
}

/// A deployment that serves no HTTP/3 must not tell clients to look for one.
///
/// A failed QUIC attempt is a visible delay for every visitor, so this is a correctness assertion
/// rather than tidiness — and it is the half a future refactor is most likely to break by making
/// the header unconditional.
#[test]
fn alt_svc_is_absent_without_an_http3_table() {
    with_runtime(async {
        let cert = Cert::new("altSvcNone", "a.test");
        write_file(&cert, "index.html", "x");
        // Written first, then the table removed: the fixture builder always emits one, and this
        // test is about a deployment that has none.
        write_config(&cert, "");
        let text = std::fs::read_to_string(cert.dir().join("volucer.toml")).expect("the fixture");
        let without = text.replace("[server.http3]\nlisten = \"127.0.0.1:0\"\n", "");
        let config = Config::from_toml(&without).expect("config parses");
        config
            .validate()
            .expect("a deployment without http3 validates");
        std::fs::write(cert.dir().join("volucer.toml"), &without).expect("write config");

        let addr = start_tcp_with_alt_svc(&config, &cert, None).await;
        let over_tls = fetch_over_tcp(addr, "a.test", true).await;
        assert!(
            !over_tls.contains("alt-svc"),
            "a deployment with no [server.http3] must not advertise one: {over_tls}"
        );
    });
}

/// Start a **TCP** listener advertising an `Alt-Svc` port, which is the half `Alt-Svc` is about.
async fn start_tcp_with_alt_svc(
    config: &Config,
    cert: &Cert,
    alt_svc_port: Option<u16>,
) -> SocketAddr {
    let dir = cert.dir();
    let dispatcher = Arc::new(
        Dispatcher::from_config(config, volucer_fcgi::Routing::default()).expect("dispatcher"),
    );
    let resolver = Arc::new(volucer_http::SniResolver::from_config(config, dir).expect("resolver"));
    let registry = Arc::new(volucer_http::Registry::new(volucer_http::SharedState::new(
        Sites::from_config(config, dir),
    )));

    let listener = volucer_http::HttpListener::bind_with_resolver(
        "127.0.0.1:0",
        dispatcher,
        ReloadHandles::new(registry).with_tls(resolver),
        HttpLimits::default(),
        ListenerSettings {
            tls: Some(TlsSettings {
                config,
                config_dir: dir,
            }),
            https_port: None,
            alt_svc_port,
        },
    )
    .await
    .expect("bind the TCP listener");

    let addr = listener.local_addr().expect("a bound address");
    tokio::spawn(listener.run());
    addr
}

/// One HTTP/1.1 request over TLS or cleartext, as lowercase text.
///
/// Hand-written rather than pulled from `tls.rs` because that file's helpers are private to its own
/// crate target — and the assertion here is about one header, so the smallest thing that reads a
/// response head is the right size.
///
/// **Reads until the end of the head, not to EOF.** A keep-alive connection stays open after the
/// response, so `read_to_end` would block for the full idle timeout — which is what made this test
/// take thirty seconds before it was fixed.
async fn fetch_over_tcp(addr: SocketAddr, name: &str, tls: bool) -> String {
    use tokio::io::AsyncWriteExt;

    let request = format!("GET /index.html HTTP/1.1\r\nHost: {name}\r\n\r\n");
    let mut buffer = Vec::new();

    if tls {
        let mut config = quinn::rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AcceptAny))
            .with_no_client_auth();
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
        let stream = tokio::net::TcpStream::connect(addr).await.expect("connect");
        let mut stream = connector
            .connect(
                quinn::rustls::pki_types::ServerName::try_from(name.to_string()).expect("a name"),
                stream,
            )
            .await
            .expect("the TLS handshake");
        stream
            .write_all(request.as_bytes())
            .await
            .expect("write the request");
        read_head(&mut stream, &mut buffer).await;
    } else {
        let mut stream = tokio::net::TcpStream::connect(addr).await.expect("connect");
        stream
            .write_all(request.as_bytes())
            .await
            .expect("write the request");
        read_head(&mut stream, &mut buffer).await;
    }

    String::from_utf8_lossy(&buffer).to_ascii_lowercase()
}

/// Read bytes until the blank line that ends a response head.
async fn read_head<S>(stream: &mut S, buffer: &mut Vec<u8>)
where
    S: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt;

    let mut chunk = [0u8; 1024];
    while !buffer.windows(4).any(|window| window == b"\r\n\r\n") {
        match stream.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => buffer.extend_from_slice(&chunk[..n]),
        }
    }
}

/// Accept any certificate, because these fixtures are self-signed and generated per test.
#[derive(Debug)]
struct AcceptAny;

impl quinn::rustls::client::danger::ServerCertVerifier for AcceptAny {
    fn verify_server_cert(
        &self,
        _end_entity: &quinn::rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[quinn::rustls::pki_types::CertificateDer<'_>],
        _server_name: &quinn::rustls::pki_types::ServerName<'_>,
        _ocsp: &[u8],
        _now: quinn::rustls::pki_types::UnixTime,
    ) -> std::result::Result<quinn::rustls::client::danger::ServerCertVerified, quinn::rustls::Error>
    {
        Ok(quinn::rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &quinn::rustls::pki_types::CertificateDer<'_>,
        _dss: &quinn::rustls::DigitallySignedStruct,
    ) -> std::result::Result<
        quinn::rustls::client::danger::HandshakeSignatureValid,
        quinn::rustls::Error,
    > {
        Ok(quinn::rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &quinn::rustls::pki_types::CertificateDer<'_>,
        _dss: &quinn::rustls::DigitallySignedStruct,
    ) -> std::result::Result<
        quinn::rustls::client::danger::HandshakeSignatureValid,
        quinn::rustls::Error,
    > {
        Ok(quinn::rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<quinn::rustls::SignatureScheme> {
        use quinn::rustls::crypto::CryptoProvider;
        CryptoProvider::get_default()
            .expect("a provider is installed by start()")
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// A `[server.http3]` table is only bindable when a certificate resolver was supplied.
#[test]
fn binding_without_a_certificate_resolver_is_refused() {
    let cert = Cert::new("notls", "a.test");
    let config = write_config(&cert, "");
    let dispatcher = Arc::new(
        Dispatcher::from_config(&config, volucer_fcgi::Routing::default()).expect("dispatcher"),
    );
    let registry = Arc::new(volucer_http::Registry::new(volucer_http::SharedState::new(
        Sites::from_config(&config, cert.dir()),
    )));

    // No `with_tls`, which is what a caller would have if `[server.tls]` were absent — refused by
    // `validate_http3` in a real deployment, and refused here for a library caller.
    let outcome = Http3Listener::bind(
        "127.0.0.1:0",
        dispatcher,
        ReloadHandles::new(registry),
        HttpLimits::default(),
        ListenerSettings {
            tls: Some(TlsSettings {
                config: &config,
                config_dir: cert.dir(),
            }),
            https_port: None,
            alt_svc_port: None,
        },
    );
    let err = match outcome {
        Ok(_) => panic!("a QUIC listener with no certificate cannot present anything"),
        Err(err) => err,
    };
    assert!(
        err.to_string().contains("[server.tls]"),
        "the refusal must name the missing table: {err}"
    );
}

/// Drive an async test body on a fresh multi-threaded runtime.
///
/// A helper rather than `#[tokio::test]` because the listener owns a runtime of its own on its own
/// thread, and a current-thread test runtime would deadlock the moment the client waited on work
/// only the listener could progress.
fn with_runtime<F>(body: F)
where
    F: std::future::Future<Output = ()>,
{
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    runtime.block_on(body);
}
