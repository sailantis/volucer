//! `[server.http3]`: the QUIC listener.

use super::*;

/// How many `Alt-Svc` seconds a deployment inherits when it says nothing.
///
/// One day. The header tells a browser "this name is also reachable over HTTP/3 for this long";
/// too short and a returning visitor re-learns the endpoint on every visit, too long and a
/// deployment that turns HTTP/3 off leaves browsers knocking on a closed UDP port — the failure
/// mode is a connection attempt that silently falls back, so it costs latency rather than
/// correctness. A day is the value nginx's own documentation uses in its example.
pub const DEFAULT_ALT_SVC_MA_SECONDS: u32 = 86_400;

/// The largest request header section accepted over HTTP/3, when configuration says nothing.
///
/// **This is a security setting, not a tuning knob**, and it has no counterpart in the other
/// listeners because `hyper` bounds its own header block. `h3` 0.0.8 defaults
/// `max_field_section_size` to `VarInt::MAX` — that is hyperium/h3#379, a remotely triggerable
/// amplification: a small `HEADERS` frame of one-byte static-table references decodes into a
/// `HeaderMap` with thousands of entries.
///
/// 16 KiB is a deliberate choice rather than parity with anything in this tree — nginx's
/// `large_client_header_buffers` defaults to 8 KiB, Envoy to 60 KiB. RFC 9114 §4.2.2 charges each
/// decoded field at least 32 bytes against this budget, so 16 KiB caps a section at roughly 512
/// fields, where `HeaderMap` has 32768 slots. The decoded section is rejected *before* the fields
/// are materialised, so the cap is what stops the allocation rather than what notices it.
///
/// **Do not "simplify" this back to the crate's default.** See `http3::connection_config`.
pub const DEFAULT_HTTP3_MAX_FIELD_SECTION_SIZE: u32 = 16 * 1024;

/// The largest `max_field_section_size` this server accepts, in bytes.
///
/// Not a protocol limit — a QUIC varint reaches far higher — but the point past which the setting
/// stops bounding the `HeaderMap` allocation it exists to bound. 1 MiB admits a deployment with
/// genuinely enormous cookies (roughly 32 000 fields, against `HeaderMap`'s 32768 slots) while
/// refusing a value that would restore the crate's unbounded default.
pub const MAX_HTTP3_FIELD_SECTION_SIZE: u32 = 1024 * 1024;

/// HTTP/3 over QUIC: the listener's own address, and the three limits QUIC needs stated.
///
/// **Presence of this table is what turns HTTP/3 on**, following `[server.tls]` and
/// `[server.acme]`: a `enabled` flag beside an address is two statements about one thing, and
/// two statements can disagree.
///
/// The listener is **UDP and separate** from `[server] http`/`https`, because QUIC is not a
/// framing layer over TCP. A deployment may put `listen` on the same port number as its TCP
/// `https` — the two do not collide — which is the shape `Alt-Svc: h3=":443"` expects.
///
/// Requires `[server.tls]`: QUIC is TLS 1.3 with no cleartext mode anywhere in the protocol, so
/// there is no equivalent of `[server.tls].plain` and none is offered. The certificate comes from
/// the same `SniResolver`, so per-site certificates, ACME issuance and the reload path reach this
/// listener unchanged — the QUIC configuration holds the resolver, not a copy of the certificates.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Http3Config {
    /// UDP address to bind, e.g. `0.0.0.0:443`.
    ///
    /// Required: a table with nothing to bind describes nothing. TCP-only forms are refused by
    /// name rather than left to fail at bind — see `validate_http3`.
    pub listen: String,

    /// Maximum concurrent QUIC connections.
    ///
    /// The same meaning as `HttpLimits::max_connections` on the TCP listener: one accepted
    /// connection holds a slot for as long as it is open. The default matches that listener's,
    /// because an operator moving a deployment between transports should not discover a second,
    /// different limit.
    #[serde(default = "default_http3_max_connections")]
    pub max_connections: usize,

    /// How long a connection with no traffic is kept before it is closed, in milliseconds.
    ///
    /// Defaults to QUIC's own recommendation (RFC 9308 §3.2, 30 seconds), which is also
    /// `quinn`'s default. A QUIC connection is *not* one request: the browser multiplexes over it,
    /// so this bounds idleness rather than a request, and the 15-second value the TCP listener
    /// uses for keep-alive would close connections a browser is still using.
    ///
    /// Zero is refused: `quinn` reads it as no timeout at all, which is a connection that holds a
    /// slot for ever, not a fast one.
    #[serde(default = "default_http3_idle_timeout_ms")]
    pub idle_timeout_ms: u64,

    /// `ma=` in the `Alt-Svc` header this server sends over TCP, in seconds.
    ///
    /// Sent only when this table is present, and only over TLS — see [`Alt-Svc`](#alt-svc) in the
    /// TLS documentation. Defaults to one day.
    #[serde(default = "default_alt_svc_ma_seconds")]
    pub alt_svc_ma_seconds: u32,

    /// The largest request header section accepted, in bytes.
    ///
    /// Defaults to 16 KiB and **must not be raised to the crate's default**, which is unbounded:
    /// see [`DEFAULT_HTTP3_MAX_FIELD_SECTION_SIZE`] for the advisory and the arithmetic.
    #[serde(default = "default_http3_max_field_section_size")]
    pub max_field_section_size: u32,
}

fn default_http3_max_connections() -> usize {
    Http3Config::DEFAULT_MAX_CONNECTIONS
}

fn default_http3_idle_timeout_ms() -> u64 {
    Http3Config::DEFAULT_IDLE_TIMEOUT_MS
}

fn default_alt_svc_ma_seconds() -> u32 {
    DEFAULT_ALT_SVC_MA_SECONDS
}

fn default_http3_max_field_section_size() -> u32 {
    DEFAULT_HTTP3_MAX_FIELD_SECTION_SIZE
}

impl Http3Config {
    /// The connection limit an unconfigured table gets. Written as an associated constant so
    /// `Default` and the serde default below cannot drift.
    pub const DEFAULT_MAX_CONNECTIONS: usize = 512;

    /// The idle timeout an unconfigured table gets: RFC 9308's 30 seconds.
    pub const DEFAULT_IDLE_TIMEOUT_MS: u64 = 30_000;

    /// The idle timeout, as the [`Duration`] `quinn` wants.
    pub fn idle_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_millis(self.idle_timeout_ms)
    }

    /// The `Alt-Svc` header value this table asks for, given the port the listener is bound on.
    ///
    /// Built from the **bound** port rather than from `listen`, so a deployment that writes
    /// `0.0.0.0:443` and one that writes `:8443` each advertise the endpoint that exists. The
    /// `h3` string is the ALPN identifier and is fixed by the protocol; it is not configurable and
    /// is not part of `[server.tls].alpn`, which describes the TCP socket.
    pub fn alt_svc_value(&self, port: u16) -> String {
        format!("h3=\":{port}\"; ma={}", self.alt_svc_ma_seconds)
    }
}

impl Default for Http3Config {
    fn default() -> Self {
        Http3Config {
            // An empty address is what validation refuses; a default here would be a table that
            // names a listener while saying nothing about where it is.
            listen: String::new(),
            max_connections: Http3Config::DEFAULT_MAX_CONNECTIONS,
            idle_timeout_ms: Http3Config::DEFAULT_IDLE_TIMEOUT_MS,
            alt_svc_ma_seconds: DEFAULT_ALT_SVC_MA_SECONDS,
            max_field_section_size: DEFAULT_HTTP3_MAX_FIELD_SECTION_SIZE,
        }
    }
}
