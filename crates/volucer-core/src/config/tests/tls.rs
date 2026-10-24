//! TLS, per-site certificates, ACME orders, and the listener matrix.

use super::*;

/// A config with TLS on `http`, so a test can vary one key at a time.
fn tls_config(extra: &str) -> String {
    format!(
        r#"
[server]
http = "127.0.0.1:443"

[server.tls]
certificate = "certs/default.crt"
key = "certs/default.key"
{extra}
"#
    )
}

#[test]
fn a_tls_table_with_both_paths_is_accepted() {
    let config = Config::from_toml(&tls_config("")).expect("parses");
    config.validate().expect("a certificate and key are enough");

    let tls = config.server.tls.as_ref().expect("tls present");
    assert_eq!(
        tls.min_version,
        MinTlsVersion::V1_2,
        "1.2 is the floor by default"
    );
    assert_eq!(tls.alpn, ["h2", "http/1.1"], "http/1.1 must not be dropped");
    assert!(!tls.plain, "accepting cleartext on a TLS port is opt-in");
    assert!(
        !config.server.redirect_to_https,
        "a redirect is a deployment's decision"
    );
}

#[test]
fn a_half_a_certificate_pair_is_refused_rather_than_defaulted() {
    // Both keys are optional now, so the *absence* of `key` no longer fails to parse — it has to
    // be caught by validation instead. That is the right layer for the half-pair case either
    // way: "TLS on, a certificate, no key" is a table an operator very nearly finished, and the
    // error has to say which half is missing rather than that some field is required.
    let text = r#"
[server]
http = "127.0.0.1:443"

[server.tls]
certificate = "certs/default.crt"
"#;
    let config = Config::from_toml(text).expect("both keys are optional, so this parses");
    let err = config.validate().expect_err("half a pair is refused");
    let message = err.to_string();
    assert!(
        message.contains("server.tls.key") && message.contains("name both"),
        "the error must name the missing half and the two ways out: {message}"
    );
}

/// A generated placeholder is only allowed when something will replace it.
///
/// The rule that makes the optional pair safe rather than convenient. Without it, a
/// `[server.tls]` with no certificate and no ACME table would start, generate one, and serve a
/// self-signed certificate for ever — a listener that looks healthy and is trusted by nobody,
/// which is the certificate-checked-too-late failure with the error moved from startup to the
/// client's browser.
#[test]
fn a_pair_left_out_needs_an_acme_table_to_replace_it() {
    let text = r#"
[server]
http = "127.0.0.1:443"
document_root = "/srv/app"

[server.tls]
"#;
    let config = Config::from_toml(text).expect("parses");
    let err = config
        .validate()
        .expect_err("nothing would replace a generated placeholder");
    let message = err.to_string();
    assert!(
        message.contains("server.tls.certificate") && message.contains("[server.acme]"),
        "the error must name what is absent and what would make it legal: {message}"
    );
}

/// A site's own certificate beside an ACME table is **allowed**, matching the server level.
///
/// This reverses the rule this test used to pin. The two levels disagreed: `[server.tls]` with a
/// pair plus `[server.acme]` was accepted, while the same shape at site level was refused as "two
/// answers to one question". But it is not two answers — it is one answer now and another when the
/// order completes, which is what the machinery already does: `CertificateSet` gives such a site
/// no generated placeholder, and an issued certificate overwrites the named entry for the names it
/// covers. It is also how a deployment migrates from a certificate issued elsewhere, which the
/// server level has always allowed. The rule that should hold at both levels is the absence of an
/// ACME table, not the absence of a certificate.
#[test]
fn a_site_may_name_a_certificate_and_ask_for_one_at_the_same_time() {
    let text = r#"
[server]
http = "127.0.0.1:443"
document_root = "/srv/app"

[server.tls]
certificate = "certs/default.crt"
key = "certs/default.key"

[sites.corp]
hosts = ["corp.internal"]
document_root = "/srv/corp"

[sites.corp.tls]
certificate = "certs/corp.crt"
key = "certs/corp.key"

[sites.corp.acme]
domains = ["corp.internal"]
"#;
    let config = Config::from_toml(text).expect("parses");
    config
        .validate()
        .expect("the named pair serves until the order replaces it, as it does at server level");
}

/// A site that names a certificate with no ACME table beside it is not an order, and must be real.
///
/// The half of the old rule that survives: the refusal was never about the pair, it was about the
/// pair being mistaken for an order. With a table present the pair is a placeholder-to-be-replaced;
/// without one it is the certificate, and has to exist.
#[test]
fn a_site_that_names_a_certificate_without_acme_is_not_an_order() {
    let text = r#"
[server]
http = "127.0.0.1:443"
document_root = "/srv/app"

[server.tls]
certificate = "certs/default.crt"
key = "certs/default.key"

[sites.corp]
hosts = ["corp.internal"]
document_root = "/srv/corp"

[sites.corp.tls]
certificate = "certs/corp.crt"
key = "certs/corp.key"
"#;
    let config = Config::from_toml(text).expect("parses");
    config
        .validate()
        .expect("a site with its own pair and no ACME table is an ordinary deployment");
    assert!(
        config.sites["corp"].acme.is_none(),
        "which is to say: no order is built for it"
    );
}

/// A **disabled** site table does not count as something that will replace a generated
/// placeholder.
///
/// The invariant that a generated placeholder needs something to replace it has to reach
/// `enabled`, or it is a hole: a deployment would be
/// allowed to leave the pair out on the strength of a table that orders nothing, and then serve a
/// self-signed certificate for ever — the port up, every handshake completing, and no client
/// trusting any of them.
#[test]
fn a_disabled_site_table_cannot_replace_a_generated_placeholder() {
    let text = r#"
[server]
http = "127.0.0.1:443"
document_root = "/srv/app"

[server.tls]

[sites.corp]
hosts = ["corp.example.com"]
document_root = "/srv/corp"

[sites.corp.acme]
enabled = false
"#;
    let config = Config::from_toml(text).expect("parses");
    let err = config
        .validate()
        .expect_err("a table that issues nothing replaces nothing");
    let message = err.to_string();
    assert!(
        message.contains("server.tls.certificate"),
        "the error must name what is absent: {message}"
    );
}

/// A name in two ACME tables is refused, naming both.
///
/// The failure this prevents cannot be seen in the configuration and cannot be recovered from at
/// runtime. A validation handshake carries only the SNI name, so the deployment's challenge source
/// answers first — with a challenge certificate whose `acmeIdentifier` commits to the
/// *deployment's* key authorization, while the CA is asking this server for the *site's*. The
/// site's order fails validation on every attempt and can only retry.
#[test]
fn a_name_issued_for_by_two_tables_is_refused_naming_both() {
    let text = r#"
[server]
http = "127.0.0.1:443"
document_root = "/srv/app"

[server.tls]

[server.acme]
domains = ["corp.internal", "example.com"]
contact = ["mailto:ops@example.com"]

[sites.corp]
hosts = ["corp.internal"]
document_root = "/srv/corp"

[sites.corp.acme]
"#;
    let config = Config::from_toml(text).expect("parses");
    let err = config
        .validate()
        .expect_err("one name belongs to one order");
    let message = err.to_string();
    assert!(
        message.contains("server.acme") && message.contains("sites.corp.acme"),
        "the error must name both tables, so the reader knows which to edit: {message}"
    );
    assert!(
        message.contains("corp.internal"),
        "and the name they both claim: {message}"
    );
}

/// A site that states its own `domains` is the way out of that refusal, and it works.
///
/// Nothing changed in the derivation to achieve this — stating the names was always a complete
/// replacement rather than a merge. The point of the test is that the *refusal* is escapable by
/// the edit the error message suggests, which is what makes it a good error rather than a wall.
#[test]
fn a_site_that_states_its_own_domains_no_longer_overlaps_the_deployment() {
    let text = r#"
[server]
http = "127.0.0.1:443"
document_root = "/srv/app"

[server.tls]

[server.acme]
domains = ["corp.internal", "example.com"]
contact = ["mailto:ops@example.com"]

[sites.corp]
hosts = ["corp.internal"]
document_root = "/srv/corp"

[sites.corp.acme]
domains = ["corp.example.com"]
"#;
    let config = Config::from_toml(text).expect("parses");
    config
        .validate()
        .expect("the site asks for a name the deployment does not");
}

/// A deployment-wide table that only stages settings issues nothing, so an empty `domains` is
/// legal on it — and a site's table still issues.
#[test]
fn a_disabled_server_table_is_defaults_only() {
    let text = r#"
[server]
http = "127.0.0.1:443"
document_root = "/srv/app"

[server.tls]

[server.acme]
enabled = false
contact = ["mailto:ops@example.com"]
authority = "production"

[sites.corp]
hosts = ["corp.example.com"]
document_root = "/srv/corp"

[sites.corp.acme]
"#;
    let config = Config::from_toml(text).expect("parses");
    config
        .validate()
        .expect("a disabled table stages contact and authority without ordering");

    let server = config.server.acme.as_ref().expect("the table is kept");
    assert!(
        server.domains.is_empty(),
        "and it names no domains, since it issues for none"
    );
    assert!(
        !server.enabled,
        "the switch is what makes the empty list legal rather than an error"
    );

    // The site still issues — the flag is not inherited — and it inherits the *settings*.
    let site = config.sites["corp"].acme.as_ref().expect("site table");
    assert!(
        site.issues(),
        "a disabled deployment does not silence a site"
    );
    assert_eq!(
        site.resolved_authority(&config.server.acme),
        Authority::Production,
        "but the deployment's authority is still what the site talks to"
    );
    assert_eq!(
        site.resolved_contact(&config.server.acme),
        ["mailto:ops@example.com"],
        "and its contact, which is what puts several sites on one account"
    );
}

/// A disabled site table keeps its settings and orders nothing.
#[test]
fn a_disabled_site_table_stops_the_order_and_keeps_the_settings() {
    let text = r#"
[server]
http = "127.0.0.1:443"
document_root = "/srv/app"

[server.tls]
certificate = "certs/default.crt"
key = "certs/default.key"

[sites.corp]
hosts = ["corp.example.com"]
document_root = "/srv/corp"

[sites.corp.acme]
enabled = false
domains = ["corp.example.com"]
"#;
    let config = Config::from_toml(text).expect("parses");
    config
        .validate()
        .expect("a disabled site table is not an order, so nothing has to replace anything");

    let site = config.sites["corp"]
        .acme
        .as_ref()
        .expect("the table is kept");
    assert!(!site.issues(), "nothing is ordered");
    let (names, _) = site.resolve_domains(&config.sites["corp"].hosts);
    assert_eq!(
        names,
        ["corp.example.com"],
        "and the settings are still there, which is the point of a switch over a deletion"
    );
}

/// `enabled` is not inherited, and that is the rule rather than an oversight.
///
/// A site that inherited `false` would go quiet when the *deployment's* orders were turned off,
/// and the deployments with several unrelated hostnames — the ones with several orders — are
/// exactly the ones that cannot afford it. So the flag is per table, and silence has to be said
/// where it is wanted.
#[test]
fn a_disabled_server_table_does_not_silence_a_site() {
    let text = r#"
[server]
http = "127.0.0.1:443"
document_root = "/srv/app"

[server.tls]

[server.acme]
enabled = false

[sites.corp]
hosts = ["corp.example.com"]
document_root = "/srv/corp"

[sites.corp.acme]
"#;
    let config = Config::from_toml(text).expect("parses");
    config.validate().expect("the site is still an order");

    assert!(
        !config.server.acme.as_ref().expect("table").enabled,
        "the deployment is off"
    );
    assert!(
        config.sites["corp"].acme.as_ref().expect("table").issues(),
        "and the site is not, because the flag belongs to the table that states it"
    );
}

/// A disabled table with a *bad* setting is still refused, because a site may inherit it.
///
/// The settings do not stop being in force when ordering does: a site that omits `contact` gets
/// the deployment's, and an account registered with a malformed one fails at the CA after a round
/// trip. So the format checks run either way, and only the `domains` rule is skipped.
#[test]
fn a_disabled_table_still_has_its_settings_checked() {
    let text = r#"
[server]
http = "127.0.0.1:443"
document_root = "/srv/app"

[server.tls]

[server.acme]
enabled = false
contact = ["ops@example.com"]

[sites.corp]
hosts = ["corp.example.com"]
document_root = "/srv/corp"

[sites.corp.acme]
"#;
    let config = Config::from_toml(text).expect("parses");
    let err = config
        .validate()
        .expect_err("a bare address would be sent as an unknown URI scheme");
    assert!(
        err.to_string().contains("mailto:"),
        "the error must name the fix: {err}"
    );
}

#[test]
fn https_without_a_tls_table_is_refused_by_name() {
    let text = r#"
[server]
http = "127.0.0.1:80"
https = "127.0.0.1:443"
"#;
    let config = Config::from_toml(text).expect("parses");
    let err = config.validate().expect_err("must fail");
    assert!(err.to_string().contains("server.https"), "{err}");
    assert!(err.to_string().contains("[server.tls]"), "{err}");
}

#[test]
fn a_tls_table_with_no_listener_carrying_it_is_refused() {
    let text = r#"
[server]
fastcgi = "127.0.0.1:9000"

[server.tls]
certificate = "c.crt"
key = "c.key"
"#;
    let config = Config::from_toml(text).expect("parses");
    let err = config.validate().expect_err("must fail");
    assert!(err.to_string().contains("no listener carries it"), "{err}");
}

#[test]
fn plain_and_https_together_are_refused() {
    // With a dedicated HTTPS port, cleartext already has a home: the `http` listener.
    // The sniff would have nothing to decide, so this is a contradiction rather than a
    // harmless redundancy.
    let text = r#"
[server]
http = "127.0.0.1:80"
https = "127.0.0.1:443"

[server.tls]
certificate = "c.crt"
key = "c.key"
plain = true
"#;
    let config = Config::from_toml(text).expect("parses");
    let err = config.validate().expect_err("must fail");
    assert!(err.to_string().contains("plain"), "{err}");
    assert!(err.to_string().contains("server.https"), "{err}");
}

#[test]
fn http_and_https_on_the_same_address_are_refused() {
    let text = r#"
[server]
http = "127.0.0.1:443"
https = "127.0.0.1:443"

[server.tls]
certificate = "c.crt"
key = "c.key"
"#;
    let config = Config::from_toml(text).expect("parses");
    let err = config.validate().expect_err("must fail");
    assert!(err.to_string().contains("two listeners"), "{err}");
}

#[test]
fn an_unknown_alpn_protocol_is_refused_by_name() {
    // Accepted by rustls and then never selected by anyone, which would leave a
    // listener that looks configured and answers only what a client offered.
    let config = Config::from_toml(&tls_config(r#"alpn = ["h2", "spdy/3"]"#)).expect("parses");
    let err = config.validate().expect_err("must fail");
    assert!(err.to_string().contains("spdy/3"), "{err}");
}

#[test]
fn h2_without_http11_is_allowed_but_warned_about() {
    // Not fatal: a deployment may deliberately serve only HTTP/2. It is nearly always
    // a mistake though, because a client that cannot do h2 fails the handshake rather
    // than being served HTTP/1.1 — so it validates and the warning is the signal.
    let config = Config::from_toml(&tls_config(r#"alpn = ["h2"]"#)).expect("parses");
    config
        .validate()
        .expect("h2 alone is a deliberate choice, not an error");
}

#[test]
fn a_per_site_tls_table_needs_the_server_one_first() {
    let text = r#"
[server]
http = "127.0.0.1:443"

[sites.shop]
hosts = ["shop.test"]
document_root = "/srv/shop"

[sites.shop.tls]
certificate = "shop.crt"
key = "shop.key"
"#;
    let config = Config::from_toml(text).expect("parses");
    let err = config.validate().expect_err("must fail");
    assert!(err.to_string().contains("shop.tls"), "{err}");
    assert!(err.to_string().contains("server.tls"), "{err}");
}

#[test]
fn a_per_site_certificate_overrides_the_servers() {
    let text = r#"
[server]
http = "127.0.0.1:443"

[server.tls]
certificate = "default.crt"
key = "default.key"

[sites.shop]
hosts = ["shop.test"]
document_root = "/srv/shop"

[sites.shop.tls]
certificate = "shop.crt"
key = "shop.key"
"#;
    let config = Config::from_toml(text).expect("parses");
    config
        .validate()
        .expect("a per-site certificate is the SNI case");

    let site = config.sites.get("shop").expect("site present");
    assert_eq!(
        site.tls.as_ref().and_then(|t| t.certificate.as_deref()),
        Some(Path::new("shop.crt")),
        "the site states its own certificate"
    );
}

#[test]
fn a_per_site_handshake_key_is_refused_rather_than_ignored() {
    // `min_version` describes the socket, and SNI cannot change what the socket did
    // before the request existed. Accepting it would let a site claim something the
    // listener does not do.
    let text = r#"
[server]
http = "127.0.0.1:443"

[server.tls]
certificate = "default.crt"
key = "default.key"

[sites.shop]
hosts = ["shop.test"]
document_root = "/srv/shop"

[sites.shop.tls]
certificate = "shop.crt"
key = "shop.key"
min_version = "1.3"
"#;
    let err = Config::from_toml(text).expect_err("unknown key in the site tls table");
    assert!(err.to_string().contains("min_version"), "{err}");
}

#[test]
fn a_redirect_needs_something_serving_cleartext() {
    // With TLS on `http` and `plain` off, that socket never answers cleartext, so the
    // key could never fire — refused rather than silently inert.
    let text = r#"
[server]
http = "127.0.0.1:443"
redirect_to_https = true

[server.tls]
certificate = "c.crt"
key = "c.key"
"#;
    let config = Config::from_toml(text).expect("parses");
    let err = config.validate().expect_err("must fail");
    assert!(err.to_string().contains("redirect_to_https"), "{err}");
    assert!(err.to_string().contains("cleartext"), "{err}");
}

#[test]
fn a_redirect_is_accepted_when_a_plaintext_listener_exists() {
    // Two shapes qualify: a dedicated `https` listener beside a plain `http` one, or
    // TLS on `http` with `plain = true`.
    let with_https = r#"
[server]
http = "127.0.0.1:80"
https = "127.0.0.1:443"
redirect_to_https = true

[server.tls]
certificate = "c.crt"
key = "c.key"
"#;
    Config::from_toml(with_https)
        .expect("parses")
        .validate()
        .expect("a dedicated https listener leaves http plaintext");

    let with_plain = r#"
[server]
http = "127.0.0.1:443"
redirect_to_https = true

[server.tls]
certificate = "c.crt"
key = "c.key"
plain = true
"#;
    Config::from_toml(with_plain)
        .expect("parses")
        .validate()
        .expect("plain = true gives the redirect something to fire on");
}

#[test]
fn redirect_without_tls_at_all_is_refused() {
    let text = r#"
[server]
http = "127.0.0.1:8080"
redirect_to_https = true
"#;
    let config = Config::from_toml(text).expect("parses");
    let err = config.validate().expect_err("must fail");
    assert!(err.to_string().contains("redirect_to_https"), "{err}");
}

#[test]
fn a_plaintext_deployment_is_unchanged_by_any_of_this() {
    // The control: no `[server.tls]`, no `https`, no redirect — exactly the config
    // shape that existed before TLS, and it must still validate and carry no TLS.
    let config = Config::from_toml("[server]\nhttp = \"127.0.0.1:8080\"\n").expect("parses");
    config.validate().expect("valid");
    assert!(config.server.tls.is_none());
    assert!(config.server.https.is_none());
    assert!(!config.server.redirect_to_https);
    // And the same control for the access log: **absent means off**, so an existing deployment
    // is not changed by upgrading — the rule `static_files` states and this must not break.
    assert!(config.server.access_log.is_none());
}

#[test]
fn has_plaintext_listener_matches_the_documented_matrix() {
    let bare = Config::from_toml("[server]\nhttp = \"127.0.0.1:8080\"\n").expect("parses");
    assert!(!bare.server.has_plaintext_listener());

    let tls_only = Config::from_toml(&tls_config("")).expect("parses");
    assert!(
        !tls_only.server.has_plaintext_listener(),
        "h2-only TLS on http"
    );

    let tls_plain = Config::from_toml(&tls_config("plain = true")).expect("parses");
    assert!(tls_plain.server.has_plaintext_listener());

    let both = Config::from_toml(
        r#"
[server]
http = "127.0.0.1:80"
https = "127.0.0.1:443"

[server.tls]
certificate = "c.crt"
key = "c.key"
"#,
    )
    .expect("parses");
    assert!(both.server.has_plaintext_listener());

    let https_only = Config::from_toml(
        r#"
[server]
https = "127.0.0.1:443"

[server.tls]
certificate = "c.crt"
key = "c.key"
"#,
    )
    .expect("parses");
    assert!(!https_only.server.has_plaintext_listener(), "TLS only");
    https_only
        .validate()
        .expect("a TLS-only deployment is valid");
}

// -----------------------------------------------------------------------
// The upstream backend's configuration surface.
// -----------------------------------------------------------------------
