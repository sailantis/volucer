//! ACME: absent by default, staging unless named, and every rule an order is held to.

use super::*;

/// A config with ACME, as a deployment would write it.
fn acme_config(acme_body: &str) -> Config {
    let text = format!(
        r#"
[server]
http = "0.0.0.0:443"
document_root = "/srv/app"

[server.tls]
certificate = "fallback.crt"
key = "fallback.key"

[server.acme]
{acme_body}
"#
    );
    Config::from_toml(&text).expect("the fixture parses")
}

/// ACME is off unless the table is there, which is what keeps it additive.
///
/// The claim the whole phase rests on: a deployment with no `[server.acme]` behaves exactly as
/// it did before the key existed. A default that turned it on — or a required key — would make
/// every existing configuration either change behaviour or fail to parse, and neither is
/// visible from the ACME tests themselves.
#[test]
fn acme_is_absent_until_the_table_is_written() {
    let text = r#"
[server]
http = "0.0.0.0:443"
document_root = "/srv/app"

[server.tls]
certificate = "fallback.crt"
key = "fallback.key"
"#;
    let config = Config::from_toml(text).expect("an existing config still parses");
    assert!(
        config.server.acme.is_none(),
        "ACME must be off when the table is absent, or it is not additive"
    );
    config.validate().expect("and it still validates");
}

/// Production must be named; everything else defaults to staging.
///
/// The reason this is a test rather than a comment: getting it wrong issues a real certificate
/// for a name somebody owns, against a rate limit that is per-week and per-account.
#[test]
fn acme_defaults_to_staging_and_production_is_named() {
    let default = acme_config(r#"domains = ["example.com"]"#);
    assert_eq!(
        default.server.acme.as_ref().expect("present").authority,
        Authority::Staging,
        "an authority that is not named must be staging"
    );

    let named = acme_config(
        r#"
authority = "production"
domains = ["example.com"]
"#,
    );
    assert_eq!(
        named.server.acme.as_ref().expect("present").authority,
        Authority::Production
    );
    assert_eq!(Authority::Production.as_str(), "production");
    assert_eq!(Authority::default().as_str(), "staging");
}

/// An ACME table with no `[server.tls]` is refused, and the error names the fix.
///
/// A certificate-less ACME deployment is **allowed**, and this is the reversal.
///
/// The earlier answer was "no": name a fallback pair. That answered the
/// literal question and required the operator to hand-make a certificate that is replaced
/// moments later. What is actually needed is that the listener has *something* to
/// present, and generating one satisfies that without asking for work that is thrown away.
///
/// Asserted here rather than left to `serve`, because the interesting part is not that a
/// placeholder is generated but that the configuration is *accepted*: the refusal this replaces
/// was the only thing standing between an operator and a one-line ACME config.
#[test]
fn acme_needs_no_named_certificate_because_one_is_generated() {
    let text = r#"
[server]
http = "0.0.0.0:443"
document_root = "/srv/app"

[server.tls]

[server.acme]
domains = ["example.com"]
"#;
    let config = Config::from_toml(text).expect("it parses");
    config
        .validate()
        .expect("a pair that is generated is all the listener needs");

    let tls = config.server.tls.as_ref().expect("[server.tls] is present");
    assert!(
        tls.certificate.is_none() && tls.key.is_none(),
        "the point of the case is that neither is named"
    );
}

/// A site's `domains` may be left out, and then it follows the site's `hosts` — without the
/// wildcard, which cannot go on a certificate.
///
/// The answer to "one is writing host and the other domain": they are not the same thing, so
/// rather than renaming either, the site's table *derives* from the one an operator has already
/// written. The wildcard is the whole reason this is a rule rather than a default — `*.x` is a
/// host pattern, and a CA has no `dns-01` here, so it must be dropped **and** reported.
#[test]
fn a_site_domain_list_is_derived_from_its_hosts_without_the_wildcard() {
    let text = r#"
[server]
http = "127.0.0.1:443"
document_root = "/srv/app"

[server.tls]

[sites.corp]
hosts = ["corp.internal", "*.corp.internal"]
document_root = "/srv/corp"

[sites.corp.acme]
"#;
    let config = Config::from_toml(text).expect("parses");
    config
        .validate()
        .expect("a derivation that leaves a real name is valid");
    let site = config.sites.get("corp").expect("site present");
    let acme = site.acme.as_ref().expect("the site has its own table");
    let (names, dropped) = acme.resolve_domains(&site.hosts);
    assert_eq!(
        names,
        vec!["corp.internal".to_string()],
        "the exact name is kept and the wildcard is not"
    );
    assert_eq!(
        dropped,
        vec!["*.corp.internal".to_string()],
        "what was left out is named, because the certificate will not cover it"
    );
}

/// A derivation that leaves nothing is an **error**, not an empty order.
///
/// The case that makes the rule worth having: a site whose hosts are all wildcards has no name
/// that can go on a certificate, and the alternative — silently issuing for an empty identifier
/// set — is an ACME request for nothing at all, reported by the CA in wording that names no site.
#[test]
fn a_site_whose_hosts_are_all_wildcards_must_state_its_domains() {
    let text = r#"
[server]
http = "127.0.0.1:443"
document_root = "/srv/app"

[server.tls]

[sites.corp]
hosts = ["*.corp.internal"]
document_root = "/srv/corp"

[sites.corp.acme]
"#;
    let config = Config::from_toml(text).expect("parses");
    let err = config.validate().expect_err("nothing can be issued for");
    let message = err.to_string();
    assert!(
        message.contains("*.corp.internal") && message.contains("sites.corp.acme.domains"),
        "the error must name the wildcard and where to put the real names: {message}"
    );
}

/// A site's table inherits the deployment's contact and cache, which is what shares the account.
///
/// `rustls-acme` names the account file after a hash of the contact list and the directory URL, so
/// inheriting is not tidiness: it is the difference between one account for the deployment and
/// one per site. The cache is where the certificate files live, and inheriting keeps them in one
/// directory — safe because a certificate's file name hashes its own domain set.
#[test]
fn a_site_inherits_the_contact_and_cache_so_the_account_is_shared() {
    let text = r#"
[server]
http = "127.0.0.1:443"
document_root = "/srv/app"

[server.tls]

[server.acme]
domains = ["example.com"]
contact = ["mailto:ops@example.com"]
cache = "shared-cache"

[sites.corp]
hosts = ["corp.internal"]
document_root = "/srv/corp"

[sites.corp.acme]
domains = ["corp.internal"]
"#;
    let config = Config::from_toml(text).expect("parses");
    config
        .validate()
        .expect("a site that inherits the contact is valid");

    let site = config.sites.get("corp").expect("site present");
    let acme = site.acme.as_ref().expect("site table");
    assert_eq!(
        acme.resolved_contact(&config.server.acme),
        ["mailto:ops@example.com"],
        "the contact is inherited, which is what makes the account shared"
    );
    assert_eq!(
        acme.resolved_cache(&config.server.acme),
        PathBuf::from("shared-cache"),
        "the cache is inherited, so several sites' certificates live in one directory"
    );
    assert_eq!(
        acme.resolved_authority(&config.server.acme),
        Authority::Staging,
        "and the authority, since a site has no reason to differ by default"
    );
}

/// A site may override what it must, and only that.
#[test]
fn a_site_may_state_its_own_authority_and_contact() {
    let text = r#"
[server]
http = "127.0.0.1:443"
document_root = "/srv/app"

[server.tls]

[server.acme]
domains = ["example.com"]
contact = ["mailto:ops@example.com"]

[sites.corp]
hosts = ["corp.internal"]
document_root = "/srv/corp"

[sites.corp.acme]
domains = ["corp.internal"]
contact = ["mailto:corp@example.com"]
authority = "production"
cache = "corp-cache"
"#;
    let config = Config::from_toml(text).expect("parses");
    config.validate().expect("a site may override all three");

    let site = config.sites.get("corp").expect("site present");
    let acme = site.acme.as_ref().expect("site table");
    assert_eq!(
        acme.resolved_contact(&config.server.acme),
        ["mailto:corp@example.com"]
    );
    assert_eq!(
        acme.resolved_cache(&config.server.acme),
        PathBuf::from("corp-cache")
    );
    assert_eq!(
        acme.resolved_authority(&config.server.acme),
        Authority::Production
    );
}

/// A site's own `domains` is used as written, and nothing is reported as dropped.
#[test]
fn a_site_that_states_its_domains_keeps_them_verbatim() {
    let text = r#"
[server]
http = "127.0.0.1:443"
document_root = "/srv/app"

[server.tls]

[sites.corp]
hosts = ["corp.internal", "www.corp.internal"]
document_root = "/srv/corp"

[sites.corp.acme]
domains = ["corp.internal"]
"#;
    let config = Config::from_toml(text).expect("parses");
    config.validate().expect("valid");

    let site = config.sites.get("corp").expect("site present");
    let acme = site.acme.as_ref().expect("site table");
    let (names, dropped) = acme.resolve_domains(&site.hosts);
    assert_eq!(names, vec!["corp.internal".to_string()]);
    assert!(
        dropped.is_empty(),
        "nothing was derived, so nothing was left out"
    );
}

/// A site's derived names are held to the same rules as a stated list.
///
/// The point of factoring the rules: a derivation that produced a name the CA would refuse must
/// fail here, naming the site's table, and not only in the deployment-wide case. A single-label
/// host is the example because it is a *legal* `hosts` entry and an illegal certificate name —
/// so this is exactly the gap between the two words the derivation has to bridge.
#[test]
fn a_derived_name_is_checked_like_a_stated_one() {
    let text = r#"
[server]
http = "127.0.0.1:443"
document_root = "/srv/app"

[server.tls]

[server.acme]
domains = ["example.com"]

[sites.corp]
hosts = ["corp"]
document_root = "/srv/corp"

[sites.corp.acme]
"#;
    let config = Config::from_toml(text).expect("parses");
    let err = config
        .validate()
        .expect_err("a single-label name cannot be issued for");
    let message = err.to_string();
    assert!(
        message.contains("sites.corp.acme"),
        "the error must name the site's table, not the deployment's: {message}"
    );
    assert!(
        message.contains("no dot"),
        "and it must be the certificate rule that fired, not the host rule: {message}"
    );
}

/// Each domain rule fails on the thing it names, and the message says why.
///
/// Table-driven because the rules are independent — one case per rule, each asserting the
/// *reason* rather than only that validation failed. A test that asserted "some error" for all
/// of them would pass against a single over-broad rule.
#[test]
fn an_acme_domain_that_cannot_be_validated_is_refused_with_its_reason() {
    let cases: &[(&str, &[&str])] = &[
        (
            r#"domains = ["*.example.com"]"#,
            &["wildcard", "TLS-ALPN-01"],
        ),
        (r#"domains = ["example.com:443"]"#, &["port"]),
        (r#"domains = ["a..b.com"]"#, &["empty label"]),
        (r#"domains = ["localhost"]"#, &["no dot"]),
        (r#"domains = [""]"#, &["empty"]),
    ];

    for (body, expected) in cases {
        let config = acme_config(body);
        let err = config
            .validate()
            .expect_err(&format!("{body} must be refused"));
        let message = err.to_string();
        for fragment in *expected {
            assert!(
                message.contains(fragment),
                "the error for {body} must mention {fragment:?}: {message}"
            );
        }
    }

    // And a valid list passes, so the rules above are not refusing everything.
    acme_config(r#"domains = ["example.com", "www.example.com"]"#)
        .validate()
        .expect("a plain list of names must validate");
}

/// An empty domain list is refused rather than becoming an order for nothing.
#[test]
fn acme_with_no_domains_is_refused() {
    let config = acme_config(r#"contact = ["mailto:ops@example.com"]"#);
    let err = config.validate().expect_err("nothing to issue for");
    assert!(
        err.to_string().contains("domains is empty"),
        "the error must name the key: {err}"
    );
}

/// A duplicate name is refused, and the message points at the likely cause.
///
/// The CA would ignore the repeat, so this is never *harmful* — which is exactly why it needs
/// catching: the mistake it usually represents is a name that was meant to be there instead.
#[test]
fn a_duplicate_acme_domain_is_refused_with_both_positions() {
    let config = acme_config(r#"domains = ["example.com", "www.example.com", "example.com"]"#);
    let err = config
        .validate()
        .expect_err("a duplicate name is a mistake");
    let message = err.to_string();
    assert!(
        message.contains("twice") && message.contains("positions 0 and 2"),
        "the error must say where the duplicate is, so the reader can find it: {message}"
    );
}

/// A contact without `mailto:` is refused, because the CA would reject it after a round trip.
///
/// The bare address is the mistake worth catching: every other tool takes `ops@example.com`, so
/// an operator will write that, and the CA would see an unknown URI scheme.
#[test]
fn an_acme_contact_must_be_a_mailto_uri() {
    let bare = acme_config(
        r#"
domains = ["example.com"]
contact = ["ops@example.com"]
"#,
    );
    let err = bare
        .validate()
        .expect_err("a bare address is not a URI the CA accepts");
    let message = err.to_string();
    assert!(
        message.contains("mailto:") && message.contains("ops@example.com"),
        "the error must show the fix: {message}"
    );

    let empty = acme_config(
        r#"
domains = ["example.com"]
contact = ["mailto:"]
"#,
    );
    assert!(
        empty.validate().is_err(),
        "an empty mailto: URI must be refused"
    );

    // And a proper one passes, including several.
    acme_config(
        r#"
domains = ["example.com"]
contact = ["mailto:ops@example.com", "mailto:oncall@example.com"]
"#,
    )
    .validate()
    .expect("well-formed contacts must validate");
}

/// The cache defaults beside the configuration file rather than to a relative path a reader
/// cannot predict.
#[test]
fn the_acme_cache_defaults_to_a_named_directory() {
    let config = acme_config(r#"domains = ["example.com"]"#);
    let acme = config.server.acme.as_ref().expect("present");
    assert_eq!(
        acme.cache,
        PathBuf::from("acme-cache"),
        "the default must be a name an operator can find, not the working directory"
    );
    assert!(
        !acme.cache.is_absolute(),
        "a relative default is what makes it resolve against the config file, which is the \
         rule `certificate` and `key` already follow"
    );
}

/// An unknown key under `[server.acme]` is refused rather than ignored.
///
/// `deny_unknown_fields` on the table, asserted here because the alternative is the failure
/// this project keeps refusing: a key that looks like it does something and does not. A typo
/// such as `domain = [...]` (singular) would otherwise leave `domains` empty — which is caught
/// too, but the error would name the wrong key.
#[test]
fn an_unknown_key_under_server_acme_is_refused() {
    let text = r#"
[server]
http = "0.0.0.0:443"
document_root = "/srv/app"

[server.tls]
certificate = "fallback.crt"
key = "fallback.key"

[server.acme]
domain = ["example.com"]
"#;
    let err = Config::from_toml(text).expect_err("a misspelt key is not a key");
    assert!(
        err.to_string().contains("domain"),
        "the error must name the key it did not recognise: {err}"
    );
}
