//! `[server.http3]`, and the listener arrangements it has to agree with.

use super::*;

/// A TLS deployment, so an http3 test only has to add its own table.
fn tls_config(extra: &str) -> String {
    format!(
        r#"
[server]
http = "127.0.0.1:8443"

[server.tls]
certificate = "certs/default.crt"
key = "certs/default.key"
{extra}
"#
    )
}

/// `Result::expect_err` needs the `Ok` type to be printable; this gives the same convenience for a
/// `()` payload with a message written by the caller.
fn expect_refusal(result: std::result::Result<(), crate::error::Error>, why: &str) -> String {
    match result {
        Ok(()) => panic!("{why}"),
        Err(err) => err.to_string(),
    }
}

/// Parsed **without** the `http3` feature, this table is refused by name; with it, the table's own
/// rules apply and are covered by the `with_listener` module below.
#[test]
fn the_table_is_refused_by_name_when_the_build_has_no_listener() {
    let text = format!(
        "{}listen = \"0.0.0.0:443\"\n",
        tls_config("[server.http3]\n")
    );
    let config = Config::from_toml(&text).expect("the table is in the model either way");

    #[cfg(not(feature = "http3"))]
    {
        // **By name, not "unknown field".** An operator who wrote a block this binary cannot honour
        // must read that the binary cannot honour it — the key did not disappear.
        let message = expect_refusal(config.validate(), "a build without the listener refuses it");
        assert!(
            message.contains("--features http3") && message.contains("server.http3"),
            "the refusal must name the key and the way out: {message}"
        );
    }

    #[cfg(feature = "http3")]
    config
        .validate()
        .expect("with the listener, this table is valid on its own merits");
}

#[cfg(feature = "http3")]
mod with_listener {
    use super::*;

    /// An `[server.http3]` table on a TLS deployment, with one key varied.
    fn http3_config(keys: &str) -> Config {
        let text = format!(
            "{}[server.http3]\nlisten = \"0.0.0.0:443\"\n{keys}",
            tls_config("")
        );
        Config::from_toml(&text).expect("parses")
    }

    /// The same config with a different `server.http3.listen`, so a test can vary one key.
    fn with_listen(mut config: Config, listen: &str) -> Config {
        if let Some(http3) = config.server.http3.as_mut() {
            http3.listen = listen.to_string();
        }
        config
    }

    #[test]
    fn a_well_formed_table_is_accepted_and_defaults_are_the_documented_ones() {
        let config = http3_config("");
        config
            .validate()
            .expect("listen and [server.tls] are enough");

        let http3 = config.server.http3.as_ref().expect("the table is present");
        assert_eq!(
            http3.max_field_section_size,
            16 * 1024,
            "the header-section cap is a security setting, so its default is asserted"
        );
        assert_eq!(http3.idle_timeout_ms, 30_000, "RFC 9308's recommendation");
        assert_eq!(http3.max_connections, 512, "the TCP listener's own default");
        assert_eq!(http3.alt_svc_ma_seconds, 86_400, "one day");
    }

    #[test]
    fn the_table_needs_a_tls_table_because_quic_has_no_cleartext_mode() {
        let text = r#"
[server]
http = "127.0.0.1:8080"

[server.http3]
listen = "0.0.0.0:443"
"#;
        let config = Config::from_toml(text).expect("parses");
        let message = config
            .validate()
            .expect_err("QUIC is TLS 1.3 and nothing else")
            .to_string();
        assert!(
            message.contains("[server.tls]") && message.contains("TLS 1.3"),
            "the error must name the missing table and the reason: {message}"
        );
    }

    /// `h3` in the TCP socket's ALPN list would advertise a protocol that socket cannot speak.
    #[test]
    fn h3_in_the_tls_alpn_list_is_refused_and_named() {
        let text = format!(
            "{}[server.http3]\nlisten = \"0.0.0.0:443\"\n",
            tls_config("alpn = [\"h2\", \"http/1.1\", \"h3\"]\n")
        );
        let config = Config::from_toml(&text).expect("parses");
        let message = config
            .validate()
            .expect_err("h3 is not negotiable over TCP")
            .to_string();
        assert!(
            message.contains("server.tls.alpn") && message.contains("\"h3\""),
            "the error must name the key and the value: {message}"
        );
    }

    #[test]
    fn a_unix_listen_address_is_refused_because_quic_is_udp() {
        // `cfg!(unix)` decides whether `parse_listener` accepts the form at all, so this is only
        // reachable on a unix host. On Windows the address fails to parse, which is also a refusal
        // and also names the key.
        let config = with_listen(http3_config(""), "unix:/run/volucer/quic.sock");
        let message = expect_refusal(config.validate(), "QUIC has no unix socket");
        assert!(
            message.contains("server.http3.listen"),
            "the error must name the key: {message}"
        );
    }

    /// `Alt-Svc` is how a browser learns the UDP endpoint exists, so it has to be sent on a TCP
    /// listener — and that requirement is already enforced transitively: this table needs
    /// `[server.tls]`, and `validate_tls` refuses a TLS table no listener carries.
    ///
    /// Asserted here rather than left implicit, because the guarantee is what makes an explicit
    /// rule in `validate_http3` unnecessary, and a guarantee nothing checks is one a future edit can
    /// remove without noticing.
    #[test]
    fn http3_with_no_tcp_listener_is_refused_by_the_tls_rule() {
        let text = r#"
[server]
fastcgi = "127.0.0.1:9000"

[server.tls]
certificate = "certs/default.crt"
key = "certs/default.key"

[server.http3]
listen = "0.0.0.0:443"
"#;
        let config = Config::from_toml(text).expect("parses");
        let message = expect_refusal(config.validate(), "a TLS table nothing carries is refused");
        assert!(
            message.contains("server.http") || message.contains("server.https"),
            "the error must name the listeners that would carry it: {message}"
        );
    }

    #[test]
    fn zero_is_refused_for_every_key_it_would_make_meaningless() {
        // One assertion per key, because each zero means something different: a limit that refuses
        // everything, a timeout that never expires, an advertisement that expires instantly, and a
        // header section that admits no header.
        for key in [
            "max_connections",
            "idle_timeout_ms",
            "alt_svc_ma_seconds",
            "max_field_section_size",
        ] {
            let config = http3_config(&format!("{key} = 0\n"));
            let message = expect_refusal(config.validate(), &format!("{key} = 0 must be refused"));
            assert!(
                message.contains(&format!("server.http3.{key}")),
                "{key}: the error must name the key: {message}"
            );
        }
    }

    /// The ceiling exists so the setting keeps meaning what it says.
    #[test]
    fn a_header_section_ceiling_above_the_advisory_cap_is_refused() {
        let config = http3_config("max_field_section_size = 8388608\n");
        let message = config
            .validate()
            .expect_err("8 MiB restores the crate's unbounded default in effect")
            .to_string();
        assert!(
            message.contains("max_field_section_size") && message.contains("h3#379"),
            "the error must name the key and the advisory: {message}"
        );

        // A larger-than-default value under the ceiling is accepted: one MiB is a real deployment's
        // enormous cookies, and refusing it would make the key unable to express the case it exists
        // for.
        let config = http3_config("max_field_section_size = 1048576\n");
        config.validate().expect("one MiB is allowed");
    }

    #[test]
    fn the_alt_svc_value_names_the_bound_port_and_the_configured_lifetime() {
        let http3 = Http3Config {
            idle_timeout_ms: 1000,
            alt_svc_ma_seconds: 3600,
            ..http3_config("").server.http3.expect("present")
        };
        assert_eq!(http3.alt_svc_value(443), "h3=\":443\"; ma=3600");
        assert_eq!(
            http3.alt_svc_value(8443),
            "h3=\":8443\"; ma=3600",
            "the bound port, not the configured string, is what a browser can reach"
        );
    }
}
