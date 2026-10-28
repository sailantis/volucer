//! `[server.http3]`: the rules a QUIC listener has to satisfy before it is bound.
//!
//! Every rule here exists because the alternative is a configuration that parses, reports nothing,
//! and then does something the file does not say — the same test `validate_tls` applies. The one
//! invariant this module deliberately does **not** check is TLS 1.3 support: that needs `rustls`,
//! which `volucer-core` does not depend on, and a throwaway config built here would prove a fact
//! about the *crypto provider* rather than about the configuration this server will actually use.
//! It is asserted instead on the real QUIC configuration, before the socket is bound — see
//! `volucer_http::http3::Http3Listener::bind`.

use super::*;

impl Config {
    /// Validate `[server.http3]`, and the arrangements it has to agree with.
    pub(super) fn validate_http3(&self) -> Result<()> {
        let Some(http3) = &self.server.http3 else {
            return Ok(());
        };

        // Refused by *name* rather than by `toml`'s `unknown field`, which is why the field exists
        // in the model unconditionally: an operator who wrote a block this build cannot honour must
        // read that, not "unknown field `http3`" — the key did not disappear, the binary cannot
        // serve it. This is the same courtesy `legacy_default_backend` pays.
        #[cfg(not(feature = "http3"))]
        {
            let _ = http3;
            return Err(Error::Config(
                "server.http3 is set, but this build of volucer was compiled without the `http3` \
                 feature; rebuild with `--features http3` (a published release carries it) or \
                 remove the table"
                    .to_string(),
            ));
        }

        #[cfg(feature = "http3")]
        {
            self.validate_http3_table(http3)
        }
    }

    /// The rules themselves, compiled only where the feature they describe exists.
    ///
    /// Split so the `cfg` above reads as one decision. A build without the feature has nothing to
    /// check these against, and a rule that could not be honoured is exactly what the branch above
    /// already refused.
    #[cfg(feature = "http3")]
    fn validate_http3_table(&self, http3: &Http3Config) -> Result<()> {
        // **QUIC is TLS 1.3 and has no cleartext mode at all**, so a certificate is not optional
        // the way it is for an `[server.tls]` that leaves it to ACME. The resolver this listener
        // holds is the one `[server.tls]` builds, so without that table there is nothing to present
        // and every handshake would fail — a listener that binds and then refuses everyone.
        let Some(tls) = &self.server.tls else {
            return Err(Error::Config(
                "server.http3 is set but [server.tls] is not; QUIC is TLS 1.3 with no cleartext \
                 mode, so the listener has no certificate to present — add [server.tls] with a \
                 certificate and key, or [server.acme] to issue one"
                    .to_string(),
            ));
        };

        // `h3` is the ALPN identifier HTTP/3 requires, and it is **not** negotiable over TCP.
        // Adding it to `[server.tls].alpn` would make the TCP socket advertise a protocol it cannot
        // speak, which breaks the handshake rather than degrading it: the client picks `h3` and
        // then sends a QUIC Initial on a TCP connection. So it is refused here with the table that
        // owns it named, rather than ignored.
        if let Some(protocol) = tls
            .alpn
            .iter()
            .find(|protocol| protocol.eq_ignore_ascii_case("h3"))
        {
            return Err(Error::Config(format!(
                "server.tls.alpn contains {protocol:?}, which is HTTP/3's ALPN identifier and is \
                 negotiated by the QUIC listener, not by the TCP one: a TCP socket that advertises \
                 `h3` answers a client's QUIC attempt with nothing. Remove it from \
                 [server.tls].alpn — [server.http3] states it already"
            )));
        }

        // TCP only. `parse_listener` accepts `unix:` because the other listeners can use it, and a
        // settings file that passes `check` and cannot bind is the failure `volucer check` exists
        // to prevent.
        match parse_listener(&http3.listen)
            .map_err(|e| Error::Config(format!("server.http3.listen: {e}")))?
        {
            Listener::Tcp(_) => {}
            Listener::Unix(_) => {
                return Err(Error::Config(format!(
                    "server.http3.listen is {:?}, and QUIC is a UDP protocol — it has no unix \
                     socket form. Write a UDP host:port, e.g. `0.0.0.0:443`",
                    http3.listen
                )));
            }
        }

        // **No rule here about needing a TCP listener**, and that is deliberate rather than an
        // omission: `validate_tls` already refuses a `[server.tls]` that no listener carries, and
        // this table requires `[server.tls]` above — so "HTTP/3 with nothing to advertise it on" is
        // unreachable. A rule that could never fire reads as a guarantee and is not one; the test
        // `http3_with_no_tcp_listener_is_refused_by_the_tls_rule` pins the transitive answer so it
        // cannot quietly disappear.

        // A connection slot is a slot; zero would refuse every connection while looking configured.
        if http3.max_connections == 0 {
            return Err(Error::Config(
                "server.http3.max_connections is 0, which would refuse every QUIC connection; \
                 remove the key for the default or set a positive limit"
                    .to_string(),
            ));
        }

        // `quinn` reads a zero idle timeout as **no timeout**, which is a connection that holds a
        // slot for ever — the opposite of what the key looks like it says.
        if http3.idle_timeout_ms == 0 {
            return Err(Error::Config(
                "server.http3.idle_timeout_ms is 0, which `quinn` reads as no idle timeout at all, \
                 so a silent connection would hold its slot for ever; remove the key for the \
                 thirty-second default"
                    .to_string(),
            ));
        }

        // `ma=` is a delta-seconds value in the header, so it is bounded by what a browser will
        // parse rather than by anything QUIC cares about.
        if http3.alt_svc_ma_seconds == 0 {
            return Err(Error::Config(
                "server.http3.alt_svc_ma_seconds is 0, so every `Alt-Svc` would expire the moment \
                 it was sent and no browser would cache the endpoint; remove the key for the \
                 one-day default"
                    .to_string(),
            ));
        }

        // **The one security rule in this table.** See [`DEFAULT_HTTP3_MAX_FIELD_SECTION_SIZE`]:
        // `h3` defaults this to `VarInt::MAX`, which is hyperium/h3#379. Zero would refuse every
        // request with any header at all, and a value above the ceiling would re-create the
        // amplification the default exists to prevent — so both ends are refused, and the ceiling
        // names the advisory because that is the number a future reader would otherwise raise.
        if http3.max_field_section_size == 0 {
            return Err(Error::Config(
                "server.http3.max_field_section_size is 0, which would refuse every request that \
                 carries a header; remove the key for the 16 KiB default"
                    .to_string(),
            ));
        }
        if http3.max_field_section_size > MAX_HTTP3_FIELD_SECTION_SIZE {
            // The reason is a **panic**, not a slowdown, and the message says so with the
            // arithmetic: `h3` builds the request's header map with
            // `HeaderMap::with_capacity(fields.len())`, which aborts above 24 576 fields. The
            // budget is what bounds that count, at 32 bytes per field. A message that said
            // "amplification" would be describing this project's opinion; this one describes the
            // crash, which is the thing an operator raising the value would cause.
            let fields = u64::from(http3.max_field_section_size) / 32;
            return Err(Error::Config(format!(
                "server.http3.max_field_section_size is {} bytes, above the \
                 {MAX_HTTP3_FIELD_SECTION_SIZE}-byte ceiling: at the RFC 9114 §4.2.2 minimum of 32 \
                 bytes per field that admits {fields} fields, and `h3` 0.0.8 builds the header map \
                 with `HeaderMap::with_capacity`, which panics above \
                 {HEADER_MAP_MAX_FIELDS}. A value this large would re-create the crash the ceiling \
                 exists to prevent (see hyperium/h3#379) — leave it at the 16 KiB default unless \
                 you need genuinely enormous cookies, and not above \
                 {MAX_HTTP3_FIELD_SECTION_SIZE}",
                http3.max_field_section_size
            )));
        }

        Ok(())
    }
}
