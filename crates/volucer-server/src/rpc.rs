//! The `[rpc]` listener: the call plane for applications that have no worker channel.
//!
//! # Why a listener at all
//!
//! A worker reaches `kv`, `lock` and `jobs` over the channel that carries its requests — `CALL` and
//! `RETURN` are two more frame types on a stream the server already owns. A FastCGI application has
//! no such stream: it exists for the duration of one request, is driven by the server, and can only
//! answer what it was handed. So the call plane gets a socket the application *dials*, which is what
//! RoadRunner's `rpc.listen` is and for the same reason.
//!
//! The interesting part is not the socket, which is ordinary. It is the two questions the socket
//! raises that a worker channel never has to answer:
//!
//! - **Who is calling?** There is no connection-to-backend mapping here: one listener serves every
//!   granted backend, so a call has to say who made it. [`Token`] is that answer, and it is derived
//!   from the backend's name so that `[backends.*].services` stays the one statement of what a
//!   backend may reach.
//! - **What is already gone?** A CGI process dies with its request, so a connection may be closed by
//!   either end at any point between calls. Nothing here holds state that a reconnect would lose.
//!
//! # What is reused, and what is deliberately not
//!
//! The framing is the worker protocol's, unchanged: the same [`Header`], the same
//! [`MessageType::Call`]/[`MessageType::Return`] pair, the same CRC. That is what lets every SDK that
//! already speaks the call plane over a channel speak it over a socket with no second codec — and it
//! is why this module is a listener rather than a protocol.
//!
//! What is *not* reused is the handshake. A worker's `HELLO`/`HELLO_ACK` exchange establishes a
//! session — a checksum policy, a version, a process identity — and a CGI process would pay for all
//! of it on every request. `Call::token` costs sixty-four bytes on a frame that is about to do I/O
//! anyway, and it has a property a session does not: a reloaded configuration that withdraws a grant
//! takes effect on the next call rather than at the next reconnect.

use std::collections::BTreeMap;
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use volucer_core::config::{Listener as ListenerAddress, RpcConfig};
use volucer_worker::codec::CallHandler;
use volucer_worker::{Call, Codec, Header, Json, MessageType, Return};

use crate::calls::MethodRegistry;
use crate::error::{Result, ServerError};

/// The grants a call may be judged against, by token.
///
/// One value rather than a `BTreeMap<String, Arc<MethodRegistry>>` keyed by token, because the
/// registry is *not* the only thing a refusal needs to say: a call whose token is unknown and a call
/// whose token is known but whose method is not granted are different failures, and an operator
/// reading a log needs to be able to tell them apart. See [`RpcGrants::serve`].
pub struct RpcGrants {
    /// The registries, by backend name, exactly as the worker pools hold them.
    pub registries: BTreeMap<String, Arc<MethodRegistry>>,
    /// Which backend each token names.
    pub tokens: BTreeMap<String, String>,
}

impl std::fmt::Debug for RpcGrants {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Hand-written, and the reason is the point: the tokens are *values* an application is given
        // and a debug print is the wrong place to publish them. What is useful in a log is which
        // backends have a grant and what each may reach; `MethodRegistry`'s own `Debug` already
        // answers the second.
        f.debug_struct("RpcGrants")
            .field(
                "tokens",
                &self
                    .tokens
                    .values()
                    .map(|backend| format!("{backend}=<redacted>"))
                    .collect::<Vec<_>>(),
            )
            .field("registries", &self.registries)
            .finish()
    }
}

impl RpcGrants {
    /// Build the map from a deployment's grants.
    /// `registries` is the *same* map the worker pools were built from, so a backend granted
    /// `kv.cache` reaches one `KvNamespace` whether the call arrives over its channel or over this
    /// listener. Building a second registry here would mean two `KvNamespace` values over one store:
    /// correct, and a second thing to keep in step with the first.
    pub fn new(registries: BTreeMap<String, Arc<MethodRegistry>>, secret: &str) -> Self {
        let mut tokens = BTreeMap::new();
        for name in registries.keys() {
            tokens.insert(Token::derive(secret, name), name.clone());
        }
        RpcGrants { registries, tokens }
    }

    /// The token a backend's application is handed, or `None` when it grants nothing.
    pub fn token_for(&self, backend: &str) -> Option<&str> {
        self.tokens
            .iter()
            .find(|(_, name)| name.as_str() == backend)
            .map(|(token, _)| token.as_str())
    }

    /// The backend a token names, if it names one.
    pub fn backend_of(&self, token: &str) -> Option<&str> {
        self.tokens.get(token).map(String::as_str)
    }

    /// Whether any backend granted anything — the same question as `[rpc]` being useful at all.
    pub fn is_empty(&self) -> bool {
        self.registries.is_empty()
    }

    /// Answer one call, attributing it by token.
    ///
    /// The whole of the authorisation rule, in one place: a call with no token, or a token no backend
    /// was issued, or a token whose backend does not grant the namespace the method names, is a
    /// [`Return::refused`] that says which of the three it was. That distinction is not politeness —
    /// "no such token" is a deployment that handed the wrong address to the wrong application, while
    /// "not granted" is a `services` list missing a line, and the two are fixed in different files.
    pub fn serve(&self, call: &Call) -> Return {
        let Some(token) = call.token.as_deref() else {
            return Return::refused(format!(
                "call {} carried no token; an [rpc] call has to say which backend made it, because \
                 one listener serves every backend that grants services. Read {} from the request's \
                 CGI environment and send it with every call",
                call.name,
                volucer_core::config::RPC_TOKEN_PARAM
            ));
        };
        let Some(backend) = self.backend_of(token) else {
            // Deliberately does not echo the token. It is not a credential against a network peer,
            // but it is enough to make every call from one application, and a log line is the wrong
            // place to publish one.
            return Return::refused(format!(
                "call {} carried a token no backend of this deployment was issued, so there is \
                 nothing to attribute it to; check that the application is reading {} from its CGI \
                 environment rather than from a file written by another deployment",
                call.name,
                volucer_core::config::RPC_TOKEN_PARAM
            ));
        };
        let Some(registry) = self.registries.get(backend) else {
            // Unreachable through `new`, which builds both maps from one, and refused rather than
            // unwrapped because a panic here would take down a serving process for a condition that
            // has an ordinary answer.
            return Return::refused(format!(
                "call {} is attributed to backend {backend:?}, which has no method registry; that is \
                 an internal inconsistency, and the call has not been served",
                call.name
            ));
        };
        registry.serve(call)
    }
}

/// How a backend's identity is derived.
///
/// # Why derived rather than configured
///
/// The alternative is a `[rpc.clients.<name>]` table with its own `token` and `services` list. It is
/// more configurable and strictly worse here, for one reason: it makes what an application may reach
/// a *second* declaration of something `[backends.*].services` already declares, and two declarations
/// of one fact can disagree. The disagreement that matters is invisible in a file — one backend's
/// cache working while its lock calls are refused, because two lists were written a week apart.
///
/// Deriving it from the backend's name means the grant and the identity are the same list, read
/// once. What derivation costs is rotation: the token cannot be changed without restarting, because
/// it is `HMAC(process secret, name)` and the process secret is minted at startup. That is acceptable
/// precisely because the token is not a credential against a network attacker — the listener binds a
/// unix socket or loopback, and the token's job is to stop the wrong *local* application from
/// inheriting a grant.
pub struct Token;

impl Token {
    /// The token a backend is issued for one process's lifetime.
    ///
    /// A keyed hash of the backend's name rather than a stored random value, so that the map from
    /// token to backend is a one-line function of the name and there is no second table to keep.
    /// `secret` is minted once per process and never leaves it.
    ///
    /// **Not a cryptographic credential, and this says so rather than implying otherwise.** It has to
    /// be unguessable to a *sibling* process that can read another CGI child's environment, and a
    /// keyed hash of the name gives that. It cannot defend against a host that can read this
    /// process's memory, and nothing at this layer could.
    ///
    /// **Two independent 64-bit words, both keyed, and both formatted.** The first version formatted
    /// one word and appended the *secret's length* as the second — which is a constant for the whole
    /// process, so every backend's token ended in the same sixteen hex characters. Thirty-two
    /// characters that look like a token and carry thirty-two bits less entropy than they appear to
    /// is the kind of thing only an assertion catches; `the_token_carries_no_constant_across_backends`
    /// is that assertion.
    ///
    /// `std`'s `DefaultHasher` is deliberately not used: it is documented as not stable across
    /// releases, so a token derived from it would change when the toolchain does. This construction
    /// depends on nothing but the two inputs.
    pub fn derive(secret: &str, backend: &str) -> String {
        // Two chains over the same input with different starting constants and different
        // multipliers, so a collision in one word says nothing about the other. Neither is a
        // cryptographic hash; both are avalanche-mixed at the end, which is what makes two similar
        // names produce unrelated tokens rather than tokens sharing a prefix.
        let word = |seed: u64, prime: u64, finaliser: u64| -> u64 {
            let mut state = seed;
            for byte in secret
                .as_bytes()
                .iter()
                .chain(b"\x1f")
                .chain(backend.as_bytes())
            {
                state ^= u64::from(*byte);
                state = state.wrapping_mul(prime);
                // A second mix per byte, so a change late in the name is not only carried in the
                // high bits by the time the next byte multiplies.
                state ^= state >> 29;
            }
            state ^= state >> 33;
            state = state.wrapping_mul(0xFF51_AFD7_ED55_8CCD);
            state ^= state >> 33;
            state = state.wrapping_mul(finaliser);
            state ^= state >> 33;
            state
        };
        format!(
            "{:016x}{:016x}",
            word(
                0xCBF2_9CE4_8422_2325,
                0x0000_0100_0000_01B3,
                0xC4CE_B9FE_1A85_EC53
            ),
            word(
                0x9E37_79B9_7F4A_7C15,
                0x0000_0100_0000_01B3,
                0x94D0_49BB_1331_11EB
            ),
        )
    }
}

/// The `[rpc]` listener, bound and ready to serve.
pub struct RpcListener {
    listener: Bound,
    grants: Arc<RpcGrants>,
    config: RpcConfig,
    shutdown: Arc<AtomicBool>,
    connections: Arc<AtomicU64>,
    calls: Arc<AtomicU64>,
    refused: Arc<AtomicU64>,
}

/// Counters, for the shutdown summary and the tests.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RpcStats {
    pub connections: u64,
    pub calls: u64,
    pub refused: u64,
}

impl Drop for RpcListener {
    fn drop(&mut self) {
        self.listener.unlink();
    }
}

impl RpcListener {
    /// Bind the listener a deployment's `[rpc]` table names.
    pub fn bind(config: &RpcConfig, grants: Arc<RpcGrants>) -> Result<Self> {
        let listener = bind(&config.listen).map_err(|source| ServerError::Misconfigured {
            backend: "rpc".to_string(),
            reason: format!("{} could not be bound: {source}", config.listen),
        })?;
        Ok(RpcListener {
            listener,
            grants,
            config: config.clone(),
            shutdown: Arc::new(AtomicBool::new(false)),
            connections: Arc::new(AtomicU64::new(0)),
            calls: Arc::new(AtomicU64::new(0)),
            refused: Arc::new(AtomicU64::new(0)),
        })
    }

    /// The bound TCP address, or `None` for a unix socket.
    ///
    /// Only a test needs this, and only to discover the port `127.0.0.1:0` was given; what an
    /// application is told is [`Self::configured_address`], which is the string the deployment wrote.
    pub fn bound_address(&self) -> Option<std::net::SocketAddr> {
        self.listener.local_addr()
    }

    /// The address as the deployment wrote it — what an application is handed.
    pub fn configured_address(&self) -> &str {
        &self.config.listen
    }

    /// A handle that stops the listener from another thread.
    pub fn stop_handle(&self) -> RpcStopHandle {
        RpcStopHandle {
            shutdown: Arc::clone(&self.shutdown),
        }
    }

    /// The counters so far.
    pub fn stats(&self) -> RpcStats {
        RpcStats {
            connections: self.connections.load(Ordering::Relaxed),
            calls: self.calls.load(Ordering::Relaxed),
            refused: self.refused.load(Ordering::Relaxed),
        }
    }

    /// Accept and serve until stopped.
    ///
    /// One thread per connection, as the FastCGI front-end does, because a call is a blocking
    /// round trip and a single loop would make two applications queue behind one another for no
    /// reason. The count is bounded by nothing here: `max_connections` is checked on the accept path
    /// and a connection beyond it is closed, which is the same shape `[server]`'s listener uses.
    ///
    /// `&self` rather than `self`, unlike the FastCGI listener: the counters outlive the loop, and a
    /// shutdown summary that could not read them would report a call plane as never used.
    pub fn run(&self) -> Result<()> {
        self.listener
            .set_nonblocking(true)
            .map_err(ServerError::Io)?;

        let mut handlers: Vec<std::thread::JoinHandle<()>> = Vec::new();
        while !self.shutdown.load(Ordering::Relaxed) {
            match self.listener.accept() {
                Ok((stream, peer)) => {
                    let active = self.connections.fetch_add(1, Ordering::Relaxed) + 1;
                    if let Some(limit) = self.config.max_connections {
                        if active as usize > limit {
                            tracing::warn!(
                                %peer,
                                active,
                                limit,
                                "refusing an [rpc] connection: at max_connections"
                            );
                            self.connections.fetch_sub(1, Ordering::Relaxed);
                            drop(stream);
                            continue;
                        }
                    }
                    let grants = Arc::clone(&self.grants);
                    let shutdown = Arc::clone(&self.shutdown);
                    let calls = Arc::clone(&self.calls);
                    let refused = Arc::clone(&self.refused);
                    let connections = Arc::clone(&self.connections);
                    let idle = self.config.idle_timeout();
                    let call_timeout = self.config.call_timeout();

                    handlers.retain(|handle| !handle.is_finished());
                    handlers.push(std::thread::spawn(move || {
                        // **An accepted socket inherits the listener's non-blocking mode**, on Windows
                        // and on Linux alike — `accept` does not reset it. The listener is
                        // non-blocking so that the accept loop can poll its shutdown flag, and
                        // without this every connection would be served in non-blocking mode: the
                        // first `read` would return `WouldBlock` and the connection would be dropped
                        // before a single call was read. Found by a test rather than by reasoning,
                        // which is the only way this one is ever found.
                        if let Err(err) = stream.set_blocking(true) {
                            tracing::warn!(
                                %peer,
                                %err,
                                "an [rpc] connection could not be made blocking, so it is dropped \
                                 rather than served in a mode the frame loop would misread"
                            );
                            connections.fetch_sub(1, Ordering::Relaxed);
                            return;
                        }
                        let outcome = serve_connection(
                            stream,
                            &grants,
                            &shutdown,
                            idle,
                            call_timeout,
                            &calls,
                            &refused,
                        );
                        if let Err(err) = outcome {
                            tracing::debug!(%peer, %err, "an [rpc] connection ended");
                        }
                        connections.fetch_sub(1, Ordering::Relaxed);
                    }));
                }
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(source) => {
                    tracing::warn!(%source, "an [rpc] accept failed");
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        }

        for handle in handlers {
            let _ = handle.join();
        }
        Ok(())
    }
}

/// A socket the listener bound, over either transport.
///
/// Two shapes and one frame loop: [`serve_connection`] takes this rather than a `TcpStream`, because
/// the alternative was two copies of the loop — one per transport — and that is how a fix lands on a
/// unix socket and not on a TCP one. It is deliberately local to this module: it exists to keep
/// *this* loop's signature honest, not to become a general transport abstraction.
enum Bound {
    Tcp(std::net::TcpListener),
    #[cfg(unix)]
    Unix(std::os::unix::net::UnixListener, std::path::PathBuf),
}

/// A connection, split so the reader and writer can be held at once.
enum Connected {
    Tcp(std::net::TcpStream),
    #[cfg(unix)]
    Unix(std::os::unix::net::UnixStream),
}

impl Connected {
    fn try_clone(&self) -> std::io::Result<Self> {
        match self {
            Connected::Tcp(stream) => stream.try_clone().map(Connected::Tcp),
            #[cfg(unix)]
            Connected::Unix(stream) => stream.try_clone().map(Connected::Unix),
        }
    }

    fn set_read_timeout(&self, timeout: Option<Duration>) -> std::io::Result<()> {
        match self {
            Connected::Tcp(stream) => stream.set_read_timeout(timeout),
            #[cfg(unix)]
            Connected::Unix(stream) => stream.set_read_timeout(timeout),
        }
    }

    fn set_write_timeout(&self, timeout: Option<Duration>) -> std::io::Result<()> {
        match self {
            Connected::Tcp(stream) => stream.set_write_timeout(timeout),
            #[cfg(unix)]
            Connected::Unix(stream) => stream.set_write_timeout(timeout),
        }
    }

    /// `TCP_NODELAY` where there is a TCP stack to set it on.
    ///
    /// A unix socket has none, and the option is `ENOPROTOOPT` there — the SDK's own `connect` says
    /// the same thing about the same option, for the same reason.
    fn set_nodelay(&self) -> std::io::Result<()> {
        match self {
            Connected::Tcp(stream) => stream.set_nodelay(true),
            #[cfg(unix)]
            Connected::Unix(_) => Ok(()),
        }
    }

    /// Undo the non-blocking mode an accepted socket inherits from its listener.
    ///
    /// On Windows `accept` returns a socket that is non-blocking when the *listener* is, and this is
    /// what turns that off. Without it the first `read` returns `WouldBlock`, which the frame loop
    /// reads as an idle deadline and closes the connection — so every caller would be disconnected
    /// before its first call was looked at. On a unix socket the flag is per-descriptor too and the
    /// same rule applies.
    fn set_blocking(&self, blocking: bool) -> std::io::Result<()> {
        match self {
            Connected::Tcp(stream) => stream.set_nonblocking(!blocking),
            #[cfg(unix)]
            Connected::Unix(stream) => stream.set_nonblocking(!blocking),
        }
    }
}

impl std::io::Read for Connected {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Connected::Tcp(stream) => stream.read(buf),
            #[cfg(unix)]
            Connected::Unix(stream) => stream.read(buf),
        }
    }
}

impl std::io::Write for Connected {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Connected::Tcp(stream) => stream.write(buf),
            #[cfg(unix)]
            Connected::Unix(stream) => stream.write(buf),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Connected::Tcp(stream) => stream.flush(),
            #[cfg(unix)]
            Connected::Unix(stream) => stream.flush(),
        }
    }
}

impl Bound {
    fn set_nonblocking(&self, nonblocking: bool) -> std::io::Result<()> {
        match self {
            Bound::Tcp(listener) => listener.set_nonblocking(nonblocking),
            #[cfg(unix)]
            Bound::Unix(listener, _) => listener.set_nonblocking(nonblocking),
        }
    }

    fn accept(&self) -> std::io::Result<(Connected, std::net::SocketAddr)> {
        match self {
            Bound::Tcp(listener) => listener
                .accept()
                .map(|(stream, peer)| (Connected::Tcp(stream), peer)),
            #[cfg(unix)]
            Bound::Unix(listener, _) => listener.accept().map(|(stream, _)| {
                // A unix peer has no address. `SocketAddr::from(([0,0,0,0], 0))` would render as
                // `0.0.0.0:0`, which reads like a real peer in a log; the unspecified address is
                // the honest stand-in and every message that uses it says "peer" rather than
                // promising a route.
                (
                    Connected::Unix(stream),
                    std::net::SocketAddr::from(([0, 0, 0, 0], 0)),
                )
            }),
        }
    }

    /// The bound address, for a test that asked for port 0. `None` for a unix socket, which has no
    /// `SocketAddr` — the caller's `configured_address` is the answer there.
    fn local_addr(&self) -> Option<std::net::SocketAddr> {
        match self {
            Bound::Tcp(listener) => listener.local_addr().ok(),
            #[cfg(unix)]
            Bound::Unix(..) => None,
        }
    }

    /// Remove the socket file when the listener drops, so a restart does not meet a stale path.
    fn unlink(&self) {
        #[cfg(unix)]
        if let Bound::Unix(_, path) = self {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Stops an [`RpcListener`] from another thread.
#[derive(Clone)]
pub struct RpcStopHandle {
    shutdown: Arc<AtomicBool>,
}

impl RpcStopHandle {
    pub fn stop(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
    }
}

/// Serve one connection until the peer closes it, it goes idle, or the server stops.
///
/// The read carries the idle deadline, and the deadline is re-armed before every frame rather than
/// set once: an application that opens a connection during one request and calls again an hour later
/// is the ordinary case, and a deadline fixed at accept time would close the second call rather than
/// the idle gap.
fn serve_connection(
    stream: Connected,
    grants: &RpcGrants,
    shutdown: &AtomicBool,
    idle: Duration,
    call_timeout: Duration,
    calls: &AtomicU64,
    refused: &AtomicU64,
) -> Result<()> {
    stream.set_nodelay().map_err(ServerError::Io)?;
    // The *read* deadline is the idle one; the write deadline is the call one, because a peer that
    // has stopped reading is a call that has already exceeded the time it was given.
    stream
        .set_write_timeout(Some(call_timeout))
        .map_err(ServerError::Io)?;

    let reader = stream.try_clone().map_err(ServerError::Io)?;
    let mut reader = std::io::BufReader::new(reader);
    let mut writer = std::io::BufWriter::new(stream);

    while !shutdown.load(Ordering::Relaxed) {
        // Re-armed per frame, and on the *reader's* handle rather than the writer's: they are two
        // handles to one socket, and a deadline set on one governs only reads issued through it.
        reader
            .get_ref()
            .set_read_timeout(Some(idle))
            .map_err(ServerError::Io)?;
        let frame = match Header::read_frame(&mut reader) {
            Ok(Some(frame)) => frame,
            // A clean close between calls, which is what a CGI process does when its request ends.
            Ok(None) => return Ok(()),
            Err(volucer_worker::WorkerError::Io(io))
                if matches!(
                    io.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                tracing::debug!("closing an idle [rpc] connection");
                return Ok(());
            }
            Err(err) => {
                return Err(ServerError::WorkerProtocol {
                    backend: "rpc".into(),
                    source: err,
                })
            }
        };
        let (header, payload) = frame;

        match header.message_type {
            MessageType::Call => {
                let call: Call =
                    Json::decode(&payload).map_err(|source| ServerError::WorkerProtocol {
                        backend: "rpc".into(),
                        source,
                    })?;
                calls.fetch_add(1, Ordering::Relaxed);
                let answer = grants.serve(&call);
                if !answer.ok {
                    refused.fetch_add(1, Ordering::Relaxed);
                }
                write_return(&mut writer, header.request_id, &answer)?;
            }
            MessageType::Ping => {
                write_frame(
                    &mut writer,
                    MessageType::Pong,
                    volucer_worker::UNSOLICITED,
                    &[],
                )?;
            }
            // Anything else is a frame this listener has no use for. Refused by *name* rather than
            // dropped, for the reason `MethodRegistry` refuses an unknown method rather than
            // returning nothing: a caller waiting for an answer that never comes is a PHP request
            // hanging until `max_execution_time` kills it.
            other => {
                write_return(
                    &mut writer,
                    header.request_id,
                    &Return::refused(format!(
                        "an [rpc] connection carries only CALL and PING; {} was sent, and that is a \
                         worker channel's frame",
                        other.name()
                    )),
                )?;
            }
        }
    }
    Ok(())
}

fn write_return<W: Write>(writer: &mut W, request_id: u32, answer: &Return) -> Result<()> {
    let payload = Json::encode(answer).map_err(|source| ServerError::WorkerProtocol {
        backend: "rpc".into(),
        source,
    })?;
    write_frame(writer, MessageType::Return, request_id, &payload)
}

fn write_frame<W: Write>(
    writer: &mut W,
    kind: MessageType,
    request_id: u32,
    payload: &[u8],
) -> Result<()> {
    let header =
        Header::new(kind, request_id, payload).map_err(|source| ServerError::WorkerProtocol {
            backend: "rpc".into(),
            source,
        })?;
    volucer_worker::write_frame(writer, &header, payload).map_err(|source| {
        ServerError::WorkerProtocol {
            backend: "rpc".into(),
            source,
        }
    })
}

/// Bind a `unix:` path or a `host:port`.
fn bind(addr: &str) -> std::io::Result<Bound> {
    let listener = volucer_core::config::parse_listener(addr)
        .map_err(|reason| std::io::Error::new(std::io::ErrorKind::InvalidInput, reason))?;
    match listener {
        ListenerAddress::Tcp(address) => std::net::TcpListener::bind(address).map(Bound::Tcp),
        ListenerAddress::Unix(path) => {
            // A stale path is unlinked first, for the reason the worker pool's socket bind
            // documents: a socket path is a *file*, and binding one that survives a crash fails with
            // `EADDRINUSE`, so a restarted server could never come up. Safe because this process is
            // meant to be the only listener on that path — a live server would hold the socket and
            // the bind would fail rather than two processes sharing an address.
            #[cfg(unix)]
            {
                match std::fs::remove_file(&path) {
                    Ok(()) => {}
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                    Err(err) => return Err(err),
                }
                std::os::unix::net::UnixListener::bind(&path)
                    .map(|listener| Bound::Unix(listener, path))
            }
            #[cfg(not(unix))]
            {
                Err(std::io::Error::new(
                    std::io::ErrorKind::Unsupported,
                    format!(
                        "{} is a unix socket path, which this platform does not have; use a loopback \
                         address such as 127.0.0.1:6001",
                        path.display()
                    ),
                ))
            }
        }
    }
}

/// A [`CallHandler`] that attributes each call to the backend its token names.
///
/// Not installed anywhere — the worker channel installs a per-backend [`MethodRegistry`] instead,
/// because a worker's connection already answers "which backend". It exists so the two ways of
/// answering that question are side by side, and so a test can prove the listener's rule and the
/// channel's agree about what `services` grants.
pub struct TokenHandler {
    grants: Arc<RpcGrants>,
}

impl TokenHandler {
    pub fn new(grants: Arc<RpcGrants>) -> Self {
        TokenHandler { grants }
    }

    /// Box it as the channel's handler type.
    ///
    /// `call` by value: `CallHandler` takes a `Call` and the closure's parameter is already one, so a
    /// reference here would be created and immediately dereferenced.
    pub fn into_handler(self) -> Box<CallHandler> {
        Box::new(move |call| self.grants.serve(call))
    }
}

/// The token a process mints for itself, once.
///
/// `getrandom` rather than a hash of the process id: a pid is visible to every process on the host,
/// and a token derived from one would be derivable by the sibling process the whole scheme exists to
/// exclude.
///
/// **`None` rather than a degraded value**, unlike a request id: a request id that collides is a log
/// search that finds two lines, while a token with no entropy is a grant any local process can
/// claim. The caller turns `None` into a startup failure that names this.
pub fn new_secret() -> Option<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).ok()?;
    Some(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}
