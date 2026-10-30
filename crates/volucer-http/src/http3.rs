//! HTTP/3 over QUIC: its own UDP listener, its own transport, the *same* request path.
//!
//! # Why this module exists at all
//!
//! `hyper` has no HTTP/3 and is not getting one — HTTP/3 is QUIC, and QUIC is a different
//! transport rather than a different framing layer. So this is the one place in volucer where a
//! second protocol implementation runs, and it is deliberately the *only* one: everything below the
//! listener — site resolution, the rate limit, the static layer, the CGI translation, the
//! dispatcher, the access log, the metrics — is [`crate::listener`]'s and is not duplicated here.
//! A request that arrives over QUIC becomes a [`Request<Payload>`] and reaches the same `prepare`,
//! the same `upstream`, the same `to_cgi`.
//!
//! # What differs, and what does not
//!
//! | | TCP (`h1`/`h2`) | QUIC (`h3`) |
//! |---|---|---|
//! | TLS | optional (`[server.tls]`) | mandatory, TLS 1.3 only |
//! | ALPN | `[server.tls].alpn` | `h3`, fixed |
//! | Request body | streamed; unbounded for an origin | buffered, capped at `max_body_bytes` |
//! | Response body | streamed | streamed |
//! | `Date` | added by `hyper` | added here |
//! | Reuse | keep-alive, sequential | multiplexed, concurrent streams |
//!
//! The buffered request body is the one difference an application can see, and it is documented
//! rather than left to be discovered: a QUIC request stream is read by the application, and holding
//! one across the `spawn_blocking` the CGI path needs is not possible. The send direction still
//! streams, so SSE and large downloads are unaffected.
//!
//! # The two ways this can be made to crash, and what stops each
//!
//! 1. **A TLS-1.2-capable QUIC config.** `quinn-proto` unwraps the version check in
//!    `rustls::quic::ServerConnection::new`, so such a config panics a runtime worker on the first
//!    handshake. [`server_config`] builds TLS 1.3 only, and [`Http3Listener::bind`] proves it by
//!    making that same call eagerly — a regression becomes a startup refusal with a sentence.
//! 2. **`h3`'s unbounded header section.** Upstream defaults `max_field_section_size` to
//!    `VarInt::MAX`, which is hyperium/h3#379. [`connection_builder`] sets it from configuration,
//!    with the advisory named at the line so a future reader cannot "simplify" it back.

#![cfg(feature = "http3")]

use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use bytes::{Buf, Bytes};
use http_body_util::BodyExt;
use hyper::Response;
use volucer_core::config::Http3Config;
use volucer_server::Dispatcher;

use crate::error::{HttpError, Result};
use crate::listener::{
    body_of, handle_http3, FrontEnd, HttpLimits, HttpShutdown, HttpStats, ListenerSettings,
    Payload, ReloadHandles, ResponseBody,
};

/// The ALPN identifier HTTP/3 is negotiated with.
///
/// A constant rather than configuration: RFC 9114 §3.1 defines it, and a deployment that could
/// rename it could not talk to any browser. `[server.tls].alpn` describes the TCP socket, and
/// `validate_http3` refuses `h3` there by name.
const ALPN_H3: &[u8] = b"h3";

/// How long the accept loop waits before the shutdown flag is checked again. The same 200 ms the
/// TCP listener uses, for the same reason: a signal stream would be the only reason to pull one in.
const ACCEPT_POLL: std::time::Duration = std::time::Duration::from_millis(200);

/// How long teardown waits for in-flight requests, so a browser mid-page-load is not reset.
const DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1);

/// The rustls server configuration for QUIC.
///
/// **This is the function the feature's safety rests on, and it deliberately does not call
/// `crate::tls::server_config`.** That function honours `[server.tls].min_version`, which defaults
/// to `"1.2"` — and a QUIC config that permits TLS 1.2 is not a downgrade, it is a panic:
/// `quinn-proto`'s `QuicServerConfig::start_session` calls
/// `rustls::quic::ServerConnection::new(..).unwrap()`, and the version check lives there.
/// `QuicServerConfig::try_from` does **not** check it — a TLS-1.2-only config converts
/// successfully — so the obvious implementation would bind the socket and then crash a worker per
/// handshake.
///
/// The versions are therefore stated as TLS 1.3 and nothing else. The resolver is the same
/// [`crate::tls::SniResolver`] the TCP listener holds, so per-site certificates, ACME issuance and
/// the reload path all reach this listener unchanged.
pub fn server_config(
    resolver: Arc<crate::tls::SniResolver>,
) -> Result<Arc<quinn::rustls::ServerConfig>> {
    // `builder_with_protocol_versions` panics without an installed provider. A missing one is a
    // programming error rather than a configuration one, and defaulting here would silently pick a
    // provider the rest of the process is not using — the two-provider bug `one_rustls.rs` exists
    // to refuse.
    if quinn::rustls::crypto::CryptoProvider::get_default().is_none() {
        return Err(HttpError::Tls {
            reason: "no rustls crypto provider is installed; HTTP/3 must use the same one the TCP \
                     listener uses, so call `tls::install_crypto_provider` at startup"
                .to_string(),
        });
    }

    let mut server = quinn::rustls::ServerConfig::builder_with_protocol_versions(&[
        &quinn::rustls::version::TLS13,
    ])
    .with_no_client_auth()
    .with_cert_resolver(resolver);

    server.alpn_protocols = vec![ALPN_H3.to_vec()];
    // 0-RTT is deliberately **off**. `quinn` supports it, but replay becomes a property of the
    // application, and a PHP or Rack application is not written for that. Left as a later decision
    // with its own plan rather than enabled by an upgrade.
    server.max_early_data_size = 0;

    Ok(Arc::new(server))
}

/// The `quinn` transport settings for one deployment.
///
/// `TransportConfig::default()` is QUIC's recommended set (RFC 9308), so only what a deployment can
/// state is replaced. `max_concurrent_bidi_streams` is deliberately left at `quinn`'s 100: it
/// bounds how many requests one browser may have in flight on one connection, and lowering it would
/// serialise a page's asset loads — the exact cost HTTP/3 exists to recover on a lossy link.
fn transport_config(http3: &Http3Config) -> Result<quinn::TransportConfig> {
    let mut transport = quinn::TransportConfig::default();
    let idle = quinn::IdleTimeout::try_from(http3.idle_timeout()).map_err(|_| HttpError::Tls {
        reason: format!(
            "server.http3.idle_timeout_ms is {}ms, which is beyond what a QUIC varint can express",
            http3.idle_timeout_ms
        ),
    })?;
    transport.max_idle_timeout(Some(idle));
    Ok(transport)
}

/// The `h3` settings for one deployment.
///
/// **`max_field_section_size` is set from configuration and must never be left at the crate's
/// default.** `h3` 0.0.8 defaults it to `VarInt::MAX`, which is hyperium/h3#379: a `HEADERS` frame
/// of one-byte static-table references decodes into a `HeaderMap` with thousands of entries, so a
/// small request allocates a large one. RFC 9114 §4.2.2 charges each decoded field at least 32
/// bytes against this budget, and `qpack::decode_stateless` rejects a section that exceeds it
/// *before* the fields are materialised — so this number is what makes the mitigation real rather
/// than advisory.
fn connection_builder(http3: &Http3Config) -> h3::server::Builder {
    let mut builder = h3::server::builder();
    builder.max_field_section_size(u64::from(http3.max_field_section_size));
    // Grease is left at the crate's default (`true`): it is the mechanism that keeps a future
    // extension from being assumed absent because nobody ever sent one, and turning it off is a
    // debugging aid rather than a deployment option.
    builder
}

/// Prove a QUIC TLS config is usable by making the call `quinn` would make later.
///
/// `rustls::ServerConfig` keeps its version list private, so it cannot be read back. What can be
/// done is exactly what the first handshake does — build a `rustls::quic::ServerConnection` — which
/// is where a TLS-1.2-only config fails. Doing it here turns a per-connection panic inside `quinn`
/// into one startup error naming the cause.
fn assert_tls13_usable(server: &Arc<quinn::rustls::ServerConfig>) -> Result<()> {
    // The parameter block is empty on purpose: this probe is only after the version check, and
    // `quinn` builds the real block per session.
    quinn::rustls::quic::ServerConnection::new(
        Arc::clone(server),
        quinn::rustls::quic::Version::V1,
        Vec::new(),
    )
    .map_err(|err| HttpError::Tls {
        reason: format!(
            "the HTTP/3 TLS configuration is unusable: {err}. QUIC requires TLS 1.3, and a config \
             built from `[server.tls].min_version = \"1.2\"` fails here — which is why this \
             listener builds its own config instead of reusing the TCP one"
        ),
    })?;
    Ok(())
}

/// A bound QUIC listener, ready to be run.
pub struct Http3Listener {
    endpoint: quinn::Endpoint,
    /// The shared request path. The same type the TCP listener uses, so nothing below this module
    /// knows which transport a request arrived on.
    front_end: Arc<FrontEnd>,
    h3: Arc<h3::server::Builder>,
    max_body_bytes: usize,
    max_connections: usize,
    shutdown: HttpShutdown,
}

impl Http3Listener {
    /// Bind the UDP socket and build everything a connection needs.
    ///
    /// Everything that can be refused is refused **before the socket is bound**: a missing TLS
    /// table, a config that would panic `quinn`, an idle timeout QUIC cannot express. A listener
    /// that binds first and fails afterwards reports a port as up that never answers — and a
    /// service manager restarting on failure then takes and releases the port in a loop.
    pub fn bind(
        address: &str,
        dispatcher: Arc<Dispatcher>,
        handles: ReloadHandles,
        limits: HttpLimits,
        settings: ListenerSettings<'_>,
    ) -> Result<Self> {
        let Some(http3) = settings.tls.and_then(|tls| tls.config.server.http3.clone()) else {
            return Err(HttpError::Tls {
                reason: "no [server.http3] table was supplied, so there is nothing to bind"
                    .to_string(),
            });
        };
        let Some(resolver) = handles.tls.clone() else {
            return Err(HttpError::Tls {
                reason: "[server.http3] needs [server.tls]: QUIC is TLS 1.3 with no cleartext \
                         mode, so the listener has no certificate to present"
                    .to_string(),
            });
        };

        // **The eager TLS 1.3 assertion.** `server_config` builds a 1.3-only config, so this cannot
        // fail for the reason the trap describes — which is exactly why it is asserted: it is what
        // turns "somebody edited the version list" into a startup refusal rather than a panic
        // inside `quinn` on the first handshake.
        let rustls_config = server_config(Arc::clone(&resolver))?;
        assert_tls13_usable(&rustls_config)?;

        let transport = Arc::new(transport_config(&http3)?);
        let crypto = quinn::crypto::rustls::QuicServerConfig::try_from(Arc::clone(&rustls_config))
            .map_err(|_| HttpError::Tls {
                reason: "this rustls provider has no AES-128-GCM suite, so it cannot encrypt a \
                         QUIC Initial packet — the suite QUIC requires by specification"
                    .to_string(),
            })?;
        let mut quic_config = quinn::ServerConfig::with_crypto(Arc::new(crypto));
        quic_config.transport_config(transport);

        let endpoint =
            quinn::Endpoint::server(quic_config, parse_udp(address)?).map_err(|source| {
                HttpError::Bind {
                    address: address.to_string(),
                    source,
                }
            })?;
        let bound = endpoint.local_addr().map_err(|source| HttpError::Bind {
            address: address.to_string(),
            source,
        })?;

        // The snapshot-holder, **registered** so a reload reaches this listener as well as every
        // other one. Without `register`, a `SIGHUP` would move the TCP front-ends and leave this
        // one serving the configuration it started with — the one-protocol-only reload bug the
        // shared registry exists to prevent.
        let grpc_grants = if handles.grpc_grants.is_empty() {
            dispatcher.grpc_grants()
        } else {
            Arc::clone(&handles.grpc_grants)
        };
        let state = Arc::new(volucer_watch::Swappable::new(
            volucer_watch::Reloaded::from_sites(
                handles.shared.sites(),
                Some(bound.port()),
                grpc_grants,
            ),
        ));
        handles
            .registry
            .register(volucer_watch::RegisteredFrontEnd::new(
                Arc::clone(&state),
                Some(bound.port()),
            ));

        let front_end = Arc::new(FrontEnd::new(
            dispatcher,
            Arc::clone(&handles.registry),
            state,
            Some(resolver),
            limits,
            // QUIC always has a port: it is a UDP socket, so `LocalAddress` is fully populated and
            // the two absences that a unix socket has cannot occur here.
            crate::listener::LocalAddress {
                port: Some(bound.port()),
                // `None`: a redirect is a cleartext answer and QUIC has no cleartext. The value is
                // never read on this path, and passing the TCP listener's would be a number with no
                // meaning here.
                https_port: None,
            },
            Arc::clone(&handles.observe),
            Arc::clone(&handles.client_ip),
            Arc::clone(&handles.rate_limit),
            // `None`: this listener's own responses must not advertise the endpoint they arrived
            // on. A client already speaking HTTP/3 has nothing to learn from `Alt-Svc`.
            None,
            // The same budget the TCP listener drains with: a stop is one signal and has one
            // deadline, whichever transport a given connection arrived on.
            settings.shutdown_timeout,
        ));

        let shutdown = front_end.shutdown_handle();
        Ok(Http3Listener {
            endpoint,
            front_end,
            h3: Arc::new(connection_builder(&http3)),
            max_body_bytes: limits.max_body_bytes,
            max_connections: http3.max_connections,
            shutdown,
        })
    }

    /// The address actually bound, which matters when port 0 was requested.
    pub fn local_addr(&self) -> Result<SocketAddr> {
        self.endpoint
            .local_addr()
            .map_err(|source| HttpError::Bind {
                address: "<http3>".to_string(),
                source,
            })
    }

    /// The `Alt-Svc` value this endpoint should be advertised as.
    ///
    /// Built from the **bound** port rather than the configured string, so a deployment that asked
    /// for port 0 advertises the endpoint that exists.
    pub fn alt_svc(&self) -> String {
        let port = self.endpoint.local_addr().map(|a| a.port()).unwrap_or(0);
        format!("h3=\":{port}\"; ma=86400")
    }

    /// Statistics, for logging.
    pub fn stats(&self) -> Arc<HttpStats> {
        self.front_end.stats()
    }

    /// A handle that stops the listener from another thread.
    pub fn shutdown_handle(&self) -> HttpShutdown {
        self.shutdown.clone()
    }

    /// Accept QUIC connections until the shutdown flag is set.
    pub async fn run(self) -> Result<()> {
        let Http3Listener {
            endpoint,
            front_end,
            h3,
            max_body_bytes,
            max_connections,
            shutdown,
        } = self;

        while !shutdown.is_shutting_down() {
            // The poll boundary is also where the shutdown flag is observed: `accept()` itself has
            // no way to be interrupted, and adding a signal stream just for that would be the only
            // reason to pull one in.
            let Some(incoming) = tokio::time::timeout(ACCEPT_POLL, endpoint.accept())
                .await
                .ok()
                .flatten()
            else {
                // No connection in this window, which is the normal idle state. The loop re-reads
                // the flag, which is the whole purpose of the poll.
                continue;
            };

            let remote = incoming.remote_address();
            let active = front_end.connections().fetch_add(1, Ordering::Relaxed) + 1;
            if active > max_connections {
                front_end.rejected().fetch_add(1, Ordering::Relaxed);
                front_end.connections().fetch_sub(1, Ordering::Relaxed);
                tracing::warn!(
                    %remote,
                    active,
                    max_connections,
                    "refusing an HTTP/3 connection: at the limit"
                );
                // `refuse` sends a CONNECTION_CLOSE, so the client learns to stop. Dropping an
                // `Incoming` silently leaves it retrying the handshake.
                incoming.refuse();
                continue;
            }

            let front_end = Arc::clone(&front_end);
            let h3 = Arc::clone(&h3);
            tokio::spawn(async move {
                serve_connection(incoming, h3, Arc::clone(&front_end), max_body_bytes).await;
                front_end.connections().fetch_sub(1, Ordering::Relaxed);
            });
        }

        // A polite close: `wait_idle` lets in-flight requests finish rather than resetting them.
        endpoint.close(quinn::VarInt::from_u32(0), b"");
        let _ = tokio::time::timeout(DRAIN_TIMEOUT, endpoint.wait_idle()).await;

        let (connections, requests, rejected, failed) = front_end.stats().snapshot();
        let h3_snapshot = front_end.stats().h3_snapshot();
        tracing::info!(
            connections,
            requests,
            rejected,
            failed,
            h3_connections = h3_snapshot.connections,
            h3_requests = h3_snapshot.requests,
            h3_handshake_failures = h3_snapshot.handshake_failures,
            h3_oversized_headers = h3_snapshot.oversized_headers,
            "http3 listener stopped"
        );
        Ok(())
    }
}

/// A running HTTP/3 listener on a dedicated runtime thread.
///
/// The counterpart of [`crate::listener::RunningHttp`], and it exists for the same reason: a
/// blocking caller — `volucer serve`, whose FastCGI listener owns the main thread — runs an async
/// front-end on a thread that exists only for it. The alternative would be an async `main`, which
/// drags the whole program into the runtime instead of leaving it at this boundary.
///
/// The runtime is deliberately *not* stored: it is moved onto the listener's thread and dropped
/// there, which is what `tokio` requires. What is kept is the shutdown flag, the thread handle and
/// the bound address.
pub struct RunningHttp3 {
    shutdown: HttpShutdown,
    thread: Option<std::thread::JoinHandle<()>>,
    address: SocketAddr,
    stats: Arc<HttpStats>,
    alt_svc: String,
}

impl RunningHttp3 {
    /// Bind and start on a dedicated runtime thread.
    ///
    /// Returns once the socket is bound and the thread is accepting, so an address that is already
    /// taken is reported to the caller rather than to a thread — the same rule [`crate::listener::
    /// RunningHttp::start_with_tls`] follows, and for the same reason: `serve` prints its listener
    /// block from these results, and a port that failed afterwards would be printed as up.
    pub fn start(
        address: &str,
        dispatcher: Arc<Dispatcher>,
        handles: ReloadHandles,
        limits: HttpLimits,
        settings: ListenerSettings<'_>,
    ) -> Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(HttpError::Runtime)?;

        let listener = Http3Listener::bind(address, dispatcher, handles, limits, settings)?;
        let bound = listener.local_addr()?;
        let shutdown = listener.shutdown_handle();
        let stats = listener.stats();
        let alt_svc = listener.alt_svc();

        let thread = std::thread::Builder::new()
            .name("http3".to_string())
            .spawn(move || {
                // The runtime is owned by this thread and dropped when the listener stops, which is
                // the order `tokio` requires.
                if let Err(err) = runtime.block_on(listener.run()) {
                    tracing::warn!(%err, "http3 listener stopped with an error");
                }
            })
            .map_err(HttpError::Runtime)?;

        Ok(RunningHttp3 {
            shutdown,
            thread: Some(thread),
            address: bound,
            stats,
            alt_svc,
        })
    }

    /// The address actually bound, which matters when port 0 was requested.
    pub fn local_addr(&self) -> SocketAddr {
        self.address
    }

    /// The `Alt-Svc` value that names this endpoint, for the TCP listeners to advertise.
    pub fn alt_svc(&self) -> &str {
        &self.alt_svc
    }

    /// The port this endpoint is bound on, for `[server.http3]`'s `Alt-Svc` construction.
    pub fn port(&self) -> u16 {
        self.address.port()
    }

    /// Statistics, for logging at shutdown.
    pub fn stats(&self) -> Arc<HttpStats> {
        Arc::clone(&self.stats)
    }

    /// Stop the listener and wait for its thread to finish.
    pub fn stop(self) {
        drop(self);
    }
}

impl Drop for RunningHttp3 {
    fn drop(&mut self) {
        self.shutdown.shutdown();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Serve every request on one QUIC connection until it closes.
async fn serve_connection(
    incoming: quinn::Incoming,
    h3: Arc<h3::server::Builder>,
    front_end: Arc<FrontEnd>,
    max_body_bytes: usize,
) {
    let remote = incoming.remote_address();
    let connection = match incoming.await {
        Ok(connection) => connection,
        Err(err) => {
            // A failed handshake is counted rather than propagated: it is a client that could not
            // negotiate, and a port nothing can connect to must be visible rather than silent.
            front_end
                .stats()
                .h3_handshake_failures
                .fetch_add(1, Ordering::Relaxed);
            tracing::debug!(%remote, %err, "http3 handshake failed");
            return;
        }
    };
    front_end
        .stats()
        .h3_connections
        .fetch_add(1, Ordering::Relaxed);

    let mut h3_connection = match h3
        .build::<_, Bytes>(h3_quinn::Connection::new(connection))
        .await
    {
        Ok(connection) => connection,
        Err(err) => {
            tracing::debug!(%remote, %err, "http3 connection setup failed");
            return;
        }
    };

    // One task per request stream. `quinn` already bounds concurrency by the negotiated stream
    // limit, so no semaphore is needed; the `JoinSet` is reaped so a long-lived connection does not
    // accumulate finished tasks.
    let mut requests = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            accepted = h3_connection.accept() => match accepted {
                Ok(Some(resolver)) => {
                    let front_end = Arc::clone(&front_end);
                    requests.spawn(async move {
                        serve_request(resolver, front_end, max_body_bytes, remote).await;
                    });
                }
                // No more streams will be opened: the connection is closing.
                Ok(None) => break,
                Err(err) => {
                    tracing::debug!(%remote, %err, "http3 accept failed");
                    break;
                }
            },
            // Reap finished requests, so the set stays the size of the in-flight work.
            Some(_finished) = requests.join_next(), if !requests.is_empty() => {}
        }
    }
    requests.shutdown().await;
}

/// Resolve, dispatch and answer one HTTP/3 request.
async fn serve_request(
    resolver: h3::server::RequestResolver<h3_quinn::Connection, Bytes>,
    front_end: Arc<FrontEnd>,
    max_body_bytes: usize,
    peer: SocketAddr,
) {
    let (request, mut stream) = match resolver.resolve_request().await {
        Ok(pair) => pair,
        Err(err) => {
            // `h3` has already answered a too-large header section with a 431 and reset the stream
            // — see `ResolvedRequest::resolve`. Counting it is what makes an attack visible, and
            // answering 431 rather than dropping is what the amplification test pins.
            if matches!(err, h3::error::StreamError::HeaderTooBig { .. }) {
                front_end
                    .stats()
                    .h3_oversized_headers
                    .fetch_add(1, Ordering::Relaxed);
                tracing::warn!(%err, "http3 request refused: header section too large");
            } else {
                tracing::debug!(%err, "http3 request failed to resolve");
            }
            return;
        }
    };

    front_end
        .stats()
        .h3_requests
        .fetch_add(1, Ordering::Relaxed);

    // **The body is buffered here, not streamed.** A QUIC request stream belongs to `h3`, and
    // holding one across the `spawn_blocking` the CGI path needs is not possible. The cap is
    // applied as the bytes arrive, so an oversized upload is refused without being buffered first.
    let body = match read_body(&mut stream, max_body_bytes).await {
        Ok(body) => Payload::Buffered(body),
        Err(HttpError::BodyTooLarge { limit }) => {
            respond(&mut stream, error_page(&HttpError::BodyTooLarge { limit })).await;
            return;
        }
        Err(err) => {
            tracing::debug!(%err, "http3 request body failed");
            stream.stop_stream(h3::error::Code::H3_REQUEST_CANCELLED);
            return;
        }
    };

    let response = handle_http3(request.map(|()| body), front_end, peer, None).await;
    respond(&mut stream, response).await;
}

/// Read a request body into memory, refusing anything over the limit.
async fn read_body(
    stream: &mut h3::server::RequestStream<h3_quinn::BidiStream<Bytes>, Bytes>,
    limit: usize,
) -> Result<Bytes> {
    let mut out: Vec<u8> = Vec::new();
    while let Some(mut chunk) = stream
        .recv_data()
        .await
        .map_err(|err| HttpError::BadRequest(format!("reading the request body: {err}")))?
    {
        if out.len().saturating_add(chunk.remaining()) > limit {
            return Err(HttpError::BodyTooLarge { limit });
        }
        while chunk.has_remaining() {
            let bytes = chunk.chunk().to_vec();
            out.extend_from_slice(&bytes);
            chunk.advance(bytes.len());
        }
    }
    Ok(Bytes::from(out))
}

/// Write one response to an HTTP/3 stream.
///
/// The head first (`send_response`), then the body as data frames, then `finish()`. Trailers go
/// through `send_trailers`, which is what makes a gRPC call work here — gRPC reports its outcome
/// there, so a proxy that dropped them would turn every call into a transport error.
///
/// **`Date` is added here, and only here.** `hyper` adds it for `h1`/`h2` and `h3` does not, so
/// without this the same response would differ by transport — a difference a cache can see.
async fn respond(
    stream: &mut h3::server::RequestStream<h3_quinn::BidiStream<Bytes>, Bytes>,
    response: Response<ResponseBody>,
) {
    let (mut parts, body) = response.into_parts();
    if !parts.headers.contains_key(hyper::header::DATE) {
        if let Ok(value) = hyper::header::HeaderValue::from_str(&httpdate::fmt_http_date(
            std::time::SystemTime::now(),
        )) {
            parts.headers.insert(hyper::header::DATE, value);
        }
    }

    // `h3` takes the head as `Response<()>`, so the body is separated here rather than by a second
    // builder: the head must be sent before the first data frame is produced, and `send_response`
    // is what does that.
    if let Err(err) = stream.send_response(Response::from_parts(parts, ())).await {
        tracing::debug!(%err, "http3 response head failed");
        stream.stop_stream(h3::error::Code::H3_INTERNAL_ERROR);
        return;
    }

    let mut body = body;
    while let Some(frame) = body.frame().await {
        let frame = match frame {
            Ok(frame) => frame,
            Err(err) => {
                tracing::debug!(%err, "http3 response body errored");
                stream.stop_stream(h3::error::Code::H3_INTERNAL_ERROR);
                return;
            }
        };
        // One `match` on the frame's own parts: `Frame`'s accessors consume it, so asking it twice
        // would move it twice.
        match frame.into_data().map_err(|frame| frame.into_trailers()) {
            Ok(data) => {
                if let Err(err) = stream.send_data(data).await {
                    tracing::debug!(%err, "http3 response body failed");
                    stream.stop_stream(h3::error::Code::H3_INTERNAL_ERROR);
                    return;
                }
            }
            Err(Ok(trailers)) => {
                if let Err(err) = stream.send_trailers(trailers).await {
                    tracing::debug!(%err, "http3 response trailers failed");
                    stream.stop_stream(h3::error::Code::H3_INTERNAL_ERROR);
                    return;
                }
            }
            // Neither data nor trailers, which this server never produces.
            Err(Err(_)) => {}
        }
    }

    if let Err(err) = stream.finish().await {
        tracing::debug!(%err, "http3 response finish failed");
    }
}

/// A plain error page, for the failures that happen before a request reaches `prepare`.
///
/// The body-too-large refusal is answered here because the request never became one `prepare` could
/// see — it is refused while being read.
fn error_page(err: &HttpError) -> Response<ResponseBody> {
    let body = format!("{err}\n");
    Response::builder()
        .status(err.status())
        .header(hyper::header::CONTENT_TYPE, "text/plain; charset=utf-8")
        // A refusal this server produced must not be cached as the resource.
        .header(hyper::header::CACHE_CONTROL, "no-store")
        .body(body_of(Bytes::from(body)))
        .expect("a plain error page is a valid response")
}

/// Parse a UDP `host:port`, which is what a QUIC listener binds.
///
/// Deliberately not `volucer_core::config::parse_listener`: that accepts `unix:` forms, and a QUIC
/// listener cannot use one. `validate_http3` refuses them in the file; this refuses them at a
/// library call.
fn parse_udp(address: &str) -> Result<SocketAddr> {
    address
        .parse::<SocketAddr>()
        .map_err(|err| HttpError::Bind {
            address: address.to_string(),
            source: std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("{address:?} is not a UDP host:port ({err})"),
            ),
        })
}
