//! The site table, the pre-0.3 spellings, and how hosts and paths are matched.

use super::*;

/// A two-site configuration, which is the shape this whole key exists for.
fn two_sites() -> Config {
    let text = r#"
[server]
default_backend = "app"
http = "127.0.0.1:8080"
document_root = "/srv/default"
front_controller = "index.php"

[backends.app]
kind = "worker"
command = "php worker.php"

[backends.legacy]
kind = "spawn_cgi"
binary = 'C:\php\php-cgi.exe'

[sites.example_com]
hosts = ["example.com", "www.example.com"]
document_root = "/srv/example/public"
front_controller = "index.php"
default_backend = "app"

[sites.corp]
hosts = ["corp.internal", "*.corp.internal"]
document_root = "/srv/corp"
front_controller = false
default_backend = "legacy"
serve_only = "backend"
static_files = { precompressed = true }

  routes = [
  { path_suffix = ".php", backend = "legacy" },
  ]
"#;
    let config = Config::from_toml(text).expect("config parses");
    config.validate().expect("config validates");
    config
}

/// The two keys that moved in 0.3.0 are refused **by name**, not as unknown fields.
///
/// This is the difference between an upgrade and a puzzle. `deny_unknown_fields` reports
/// `unknown field 'default_backend'`, which is accurate and tells an operator nothing: their
/// key did not disappear, it moved one table down, and the file they pasted is one line from
/// correct. Both halves are asserted — that the message names `[server]`, and that the *new*
/// spelling is accepted — because a shim that rejected both would pass the first assertion.
#[test]
fn the_pre_0_3_spelling_is_refused_with_the_new_location() {
    let base = "http = \"127.0.0.1:8080\"\n\
                [backends.app]\nkind = \"worker\"\ncommand = \"php worker.php\"\n";

    let old_backend = format!("default_backend = \"app\"\n[server]\n{base}");
    let config = Config::from_toml(&old_backend).expect("the old key still parses");
    let err = config
        .validate()
        .expect_err("the old spelling must not be silently accepted");
    assert!(err.to_string().contains("[server]"), "{err}");
    assert!(err.to_string().contains("0.3.0"), "{err}");

    let old_routes = format!(
        "[server]\ndefault_backend = \"app\"\n{base}\n\
         [[routes]]\npath_prefix = \"/x\"\nbackend = \"app\"\n"
    );
    let config = Config::from_toml(&old_routes).expect("the old table still parses");
    let err = config
        .validate()
        .expect_err("the old table must not be silently accepted");
    assert!(err.to_string().contains("[[server.routes]]"), "{err}");

    // And the new spelling, which is the half that makes the message trustworthy.
    let new = format!(
        "[server]\ndefault_backend = \"app\"\n{base}\n\
         [[server.routes]]\npath_prefix = \"/x\"\nbackend = \"app\"\n"
    );
    let config = Config::from_toml(&new).expect("the new spelling parses");
    config.validate().expect("the new spelling validates");
    assert_eq!(config.server.default_backend.as_deref(), Some("app"));
    assert_eq!(config.server.routes.len(), 1);
}

#[test]
fn parses_a_site_table() {
    let config = two_sites();
    assert_eq!(config.sites.len(), 2);

    let example = &config.sites["example_com"];
    assert_eq!(example.hosts, ["example.com", "www.example.com"]);
    assert_eq!(example.document_root, PathBuf::from("/srv/example/public"));
    assert_eq!(example.default_backend.as_deref(), Some("app"));

    let corp = &config.sites["corp"];
    assert_eq!(corp.serve_only, Some(ServeOnly::Backend));
    // A boolean front controller is "only real files", and it round-trips as
    // `None` path rather than an empty string, which TOML cannot express.
    assert_eq!(
        corp.front_controller
            .as_ref()
            .and_then(FrontController::as_path),
        None
    );
    assert_eq!(corp.routes.as_ref().map(Vec::len), Some(1));
    assert_eq!(
        corp.static_files.as_ref().map(StaticsSetting::resolved),
        Some(StaticConfig {
            precompressed: true,
            ..StaticConfig::default()
        })
    );
}

#[test]
fn a_bare_boolean_static_setting_is_the_short_form_of_enabled() {
    // The global key takes the same bare boolean a site does, so the two
    // spellings cannot diverge — which is the point: `false` and
    // `{ enabled = false }` must not need different words.
    for (written, enabled) in [("false", false), ("true", true)] {
        let text = format!("[server]\nhttp = \"127.0.0.1:8080\"\nstatic_files = {written}\n");
        let config = Config::from_toml(&text).expect("config parses");
        assert_eq!(
            config.server.statics().enabled,
            enabled,
            "static_files = {written}"
        );
    }

    // A table is the same setting spelled at length, so the two must agree.
    let text = "[server]\nhttp = \"127.0.0.1:8080\"\n\
                static_files = { enabled = false }\n";
    let table = Config::from_toml(text).expect("config parses");
    let short = Config::from_toml("[server]\nhttp = \"127.0.0.1:8080\"\nstatic_files = false\n")
        .expect("config parses");
    assert_eq!(table.server.statics(), short.server.statics());

    // And the real thing: a site carrying the bare boolean.
    let text = r#"
[server]
http = "127.0.0.1:8080"

[sites.a]
hosts = ["a.test"]
document_root = "/srv/a"
static_files = false
"#;
    let config = Config::from_toml(text).expect("config parses");
    let site = &config.sites["a"];
    assert_eq!(
        site.static_files.as_ref().map(StaticsSetting::resolved),
        Some(StaticConfig {
            enabled: false,
            ..StaticConfig::default()
        })
    );
    config.validate().expect("files-free site still validates");
}

#[test]
fn a_wildcard_host_matches_subdomains_but_not_the_bare_domain() {
    // A wildcard names a subdomain. Matching the bare domain too would
    // silently widen every rule that used one, which is the kind of widening a
    // reader cannot see from the pattern alone.
    assert!(host_matches("*.example.com", Some("a.example.com")));
    assert!(host_matches("*.example.com", Some("a.b.example.com")));
    assert!(host_matches("*.example.com", Some("A.Example.COM:8080")));
    assert!(!host_matches("*.example.com", Some("example.com")));
    assert!(!host_matches("*.example.com", Some(".example.com")));
    assert!(!host_matches("*.example.com", Some("notexample.com")));

    // An exact name is unchanged, including the port and case handling the
    // route table already had.
    assert!(host_matches("example.com", Some("EXAMPLE.com:8080")));
    assert!(!host_matches("example.com", Some("www.example.com")));
    // An IPv6 literal has several colons and no port, so the last one must not
    // be mistaken for a port separator.
    assert!(host_matches("::1", Some("::1")));
    assert!(host_matches("::1", Some("[::1]:8080")));
}

#[test]
fn a_path_suffix_matches_case_insensitively_like_the_router_does() {
    // The router decides what a script is case-insensitively, so the route
    // table must agree or `/upload.PHP` would be a script to one layer and miss
    // its route in the other.
    let config = routed_suffix_config();
    for path in ["/upload.php", "/upload.PHP", "/a/b/x.PhP"] {
        assert_eq!(
            config.route_backend(None, path),
            Some("legacy"),
            "{path} must reach the .php route"
        );
    }
    assert_eq!(config.route_backend(None, "/upload.phtml"), None);
    // A suffix is a suffix: it does not require a segment boundary, which is
    // what a reader of `ends_with` expects.
    assert_eq!(config.route_backend(None, "/notphp"), None);
    assert_eq!(config.route_backend(None, "/xmy.php"), Some("legacy"));
}

fn routed_suffix_config() -> Config {
    let text = r#"
[server]
default_backend = "app"
http = "127.0.0.1:8080"

[backends.app]
kind = "worker"
command = "php worker.php"

[backends.legacy]
kind = "spawn_cgi"
binary = 'C:\php\php-cgi.exe'

[[server.routes]]
path_suffix = ".php"
backend = "legacy"
"#;
    let config = Config::from_toml(text).expect("config parses");
    config.validate().expect("config validates");
    config
}

#[test]
fn an_exact_host_beats_a_wildcard_at_the_same_specificity() {
    // Both routes state one constraint and both match `admin.example.com`; the
    // exact name is the more specific claim, so it wins regardless of order.
    let text = r#"
[server]
default_backend = "app"
http = "127.0.0.1:8080"

[backends.app]
kind = "worker"
command = "php worker.php"

[backends.wild]
kind = "worker"
command = "php worker.php"

[backends.exact]
kind = "worker"
command = "php worker.php"

# Declared first, so order alone would pick it — which is what the test is about.
[[server.routes]]
host = "*.example.com"
backend = "wild"

[[server.routes]]
host = "admin.example.com"
backend = "exact"
"#;
    let config = Config::from_toml(text).expect("config parses");
    config.validate().expect("config validates");
    assert_eq!(
        config.route_backend(Some("admin.example.com"), "/"),
        Some("exact")
    );
    assert_eq!(
        config.route_backend(Some("other.example.com"), "/"),
        Some("wild")
    );
}

#[test]
fn rejects_a_path_suffix_that_would_match_by_accident() {
    // `path_suffix = "php"` matches `/notphp`, which is not what it looks like
    // it does, so it is refused rather than left as a routing surprise.
    for suffix in ["php", "*php", "php/"] {
        let text = format!(
            r#"
[server]
default_backend = "app"
http = "127.0.0.1:8080"
[backends.app]
kind = "worker"
command = "php worker.php"

[[server.routes]]
path_suffix = {suffix:?}
backend = "app"
"#
        );
        let config = Config::from_toml(&text).expect("config parses");
        let err = config.validate().expect_err("must fail");
        assert!(err.to_string().contains("path_suffix"), "{suffix}: {err}");
    }
}

#[test]
fn rejects_a_site_that_would_never_be_reached() {
    let base = r#"
[server]
http = "127.0.0.1:8080"
"#;

    // Neither a host nor an address: nothing could ever select it.
    let text = format!("{base}\n[sites.a]\nhosts = []\ndocument_root = \"/srv/a\"\n");
    let config = Config::from_toml(&text).expect("parses");
    let err = config.validate().expect_err("must fail");
    assert!(
        err.to_string().contains("neither `hosts` nor `addresses`"),
        "{err}"
    );

    // Nothing on disk to serve.
    let text = format!("{base}\n[sites.a]\nhosts = [\"a.test\"]\ndocument_root = \"\"\n");
    let config = Config::from_toml(&text).expect("parses");
    let err = config.validate().expect_err("must fail");
    assert!(err.to_string().contains("document_root is empty"), "{err}");

    // Two sites claiming one host: which one wins is map order, so neither the
    // file nor a reader can predict it.
    let text = format!(
        "{base}\n[sites.a]\nhosts = [\"same.test\"]\ndocument_root = \"/srv/a\"\n\
         [sites.b]\nhosts = [\"SAME.test\"]\ndocument_root = \"/srv/b\"\n"
    );
    let config = Config::from_toml(&text).expect("parses");
    let err = config.validate().expect_err("must fail");
    assert!(err.to_string().contains("both claim"), "{err}");
}

#[test]
fn rejects_a_site_setting_that_contradicts_another() {
    // `static_files = true` and `serve_only = "backend"` cannot both be
    // honoured, and `false` beside `"static"` is a double negative. Refusing
    // keeps the resolved answer a single statement.
    let base = r#"
[server]
http = "127.0.0.1:8080"
"#;
    for (statics, serve) in [("true", "\"backend\""), ("false", "\"static\"")] {
        let text = format!(
            "{base}\n[sites.a]\nhosts = [\"a.test\"]\ndocument_root = \"/srv/a\"\n\
             static_files = {statics}\nserve_only = {serve}\n"
        );
        let config = Config::from_toml(&text).expect("parses");
        let err = config.validate().expect_err("must fail");
        assert!(
            err.to_string().contains("static_files"),
            "{statics}/{serve}: {err}"
        );
    }
}

#[test]
fn rejects_a_site_naming_an_unknown_backend_or_escaping_its_root() {
    let base = r#"
[server]
default_backend = "app"
http = "127.0.0.1:8080"
[backends.app]
kind = "worker"
command = "php worker.php"
"#;
    let text = format!(
        "{base}\n[sites.a]\nhosts = [\"a.test\"]\ndocument_root = \"/srv/a\"\n\
         default_backend = \"nope\"\n"
    );
    let config = Config::from_toml(&text).expect("parses");
    let err = config.validate().expect_err("must fail");
    assert!(err.to_string().contains("default_backend"), "{err}");

    // A site's front controller is validated by the same rule as the global
    // one, so it cannot escape the site's root either.
    for front in ["/etc/passwd", "../index.php"] {
        let text = format!(
            "{base}\n[sites.a]\nhosts = [\"a.test\"]\ndocument_root = \"/srv/a\"\n\
             front_controller = {front:?}\n"
        );
        let config = Config::from_toml(&text).expect("parses");
        let err = config.validate().expect_err("must fail");
        assert!(
            err.to_string().contains("below the document root"),
            "{front}: {err}"
        );
    }
}

#[test]
fn rejects_a_wildcard_that_would_claim_a_whole_top_level_domain() {
    let text = r#"
[server]
http = "127.0.0.1:8080"

[sites.a]
hosts = ["*.com"]
document_root = "/srv/a"
"#;
    let config = Config::from_toml(text).expect("parses");
    let err = config.validate().expect_err("must fail");
    assert!(err.to_string().contains("at least two labels"), "{err}");
}

#[test]
fn a_front_controller_of_false_means_only_real_files() {
    // The site spelling of `--front-controller ''`, which the global key can
    // only express on the command line.
    let text = r#"
[server]
http = "127.0.0.1:8080"

[sites.a]
hosts = ["a.test"]
document_root = "/srv/a"
front_controller = false
"#;
    let config = Config::from_toml(text).expect("parses");
    config.validate().expect("validates");
    let controller = config.sites["a"].front_controller.as_ref().unwrap();
    assert_eq!(controller.as_path(), None);
    assert_eq!(controller, &FrontController::FilesOnly(false));

    // `true` is accepted as a value but is not a path, so it is refused as
    // meaningless rather than silently meaning "yes, index.php".
    let text = r#"
[server]
http = "127.0.0.1:8080"

[sites.a]
hosts = ["a.test"]
document_root = "/srv/a"
front_controller = true
"#;
    let config = Config::from_toml(text).expect("parses");
    let err = config.validate().expect_err("must fail");
    assert!(err.to_string().contains("front_controller"), "{err}");
}

// ---- Per-site redirect and HSTS --------------------------------------------------

/// The split `:80`/`:443` shape, which is the one both keys below need: a plaintext listener
/// beside a TLS endpoint, so a redirect has somewhere to go and HSTS has somewhere to be sent.
fn split_https(extra_server: &str, site: &str) -> String {
    format!(
        r#"
[server]
http = "127.0.0.1:80"
https = "127.0.0.1:443"
{extra_server}

[server.tls]
certificate = "c.crt"
key = "c.key"

[sites.a]
hosts = ["a.test"]
document_root = "/srv/a"
{site}
"#
    )
}

#[test]
fn a_site_redirects_only_when_it_has_somewhere_to_send_a_client() {
    // The two halves of "a redirect would be inert": no cleartext to receive, and no TLS to send
    // to. Both are refused rather than accepted, because a redirect a site never performs is a
    // line in the file that does nothing.
    let text = split_https("", "protocols = [\"https\"]\nredirect_to_https = true");
    let err = Config::from_toml(&text)
        .expect("parses")
        .validate()
        .expect_err("must fail");
    assert!(
        err.to_string().contains("does not include \"http\""),
        "{err}"
    );

    let text = split_https("", "protocols = [\"http\"]\nredirect_to_https = true");
    let err = Config::from_toml(&text)
        .expect("parses")
        .validate()
        .expect_err("must fail");
    assert!(
        err.to_string().contains("does not include \"https\""),
        "{err}"
    );

    // Both halves present: accepted, and this is the pair the feature exists for — a site that
    // is reached over cleartext and answers by pointing at its own TLS endpoint.
    Config::from_toml(&split_https(
        "",
        "protocols = [\"http\", \"https\"]\nredirect_to_https = true",
    ))
    .expect("parses")
    .validate()
    .expect("a site with both protocols can redirect from one to the other");
}

#[test]
fn a_site_that_says_nothing_inherits_the_deployments_redirect() {
    // Inheritance in both directions, because the point of the key is that one site can differ
    // from a deployment-wide default rather than that every site must restate it.
    let on = Config::from_toml(&split_https(
        "redirect_to_https = true",
        "protocols = [\"http\", \"https\"]",
    ))
    .expect("parses");
    on.validate().expect("valid");
    let sites = crate::sites::Sites::from_config(&on, std::path::Path::new("/srv"));
    assert!(
        sites.named()["a"].redirect_to_https,
        "a site that states nothing inherits the server's answer"
    );
    assert!(sites.default_site().redirect_to_https);

    // And the override that turns it off for one site, which is the case that motivated the key:
    // an internal site that is deliberately cleartext beside a public one that redirects.
    let off = Config::from_toml(&split_https(
        "redirect_to_https = true",
        "protocols = [\"http\", \"https\"]\nredirect_to_https = false",
    ))
    .expect("parses");
    off.validate().expect("valid");
    let sites = crate::sites::Sites::from_config(&off, std::path::Path::new("/srv"));
    assert!(!sites.named()["a"].redirect_to_https);
    assert!(sites.default_site().redirect_to_https);
}

#[test]
fn hsts_needs_a_duration_it_can_inherit_or_state() {
    // Nothing above the site to inherit from, and the site states none: there is no policy to
    // send, so the table would be a key that emits nothing.
    let text = split_https("", "[sites.a.hsts]\nenabled = true");
    let err = Config::from_toml(&text)
        .expect("parses")
        .validate()
        .expect_err("must fail");
    assert!(err.to_string().contains("max_age_s"), "{err}");

    // Stated on the site: fine, and the header is what the policy says.
    let config = Config::from_toml(&split_https("", "[sites.a.hsts]\nmax_age_s = 300"));
    let config = config.expect("parses");
    config
        .validate()
        .expect("a site may state its own duration");
    let sites = crate::sites::Sites::from_config(&config, std::path::Path::new("/srv"));
    assert_eq!(
        sites.named()["a"].hsts.as_ref().map(|p| p.header_value()),
        Some("max-age=300".to_string())
    );
    assert!(
        sites.default_site().hsts.is_none(),
        "a site's table is not the deployment's"
    );
}

#[test]
fn a_site_inherits_the_servers_policy_key_by_key() {
    // The inheritance that matters for a policy: a site naming one directive must keep the
    // server's others, or "I only wanted a longer max-age" would silently drop includeSubDomains.
    let text = split_https(
        "[server.hsts]\nmax_age_s = 600\ninclude_subdomains = true",
        "[sites.a.hsts]\nmax_age_s = 31536000",
    );
    let config = Config::from_toml(&text).expect("parses");
    config.validate().expect("valid");
    let sites = crate::sites::Sites::from_config(&config, std::path::Path::new("/srv"));
    let policy = sites.named()["a"].hsts.as_ref().expect("a policy");
    assert_eq!(policy.max_age_s, 31_536_000);
    assert!(
        policy.include_subdomains,
        "the server's includeSubDomains must survive a site stating only max_age_s"
    );
    assert_eq!(
        sites.default_site().hsts.as_ref().map(|p| p.header_value()),
        Some("max-age=600; includeSubDomains".to_string())
    );
}

#[test]
fn a_site_can_turn_off_a_policy_it_would_inherit() {
    // `enabled` is a key rather than the table's presence for exactly this: the site has to be
    // able to say "not for me" without deleting a setting it also wants the rest of.
    let text = split_https(
        "[server.hsts]\nmax_age_s = 600",
        "[sites.a.hsts]\nenabled = false",
    );
    let config = Config::from_toml(&text).expect("parses");
    config.validate().expect("valid");
    let sites = crate::sites::Sites::from_config(&config, std::path::Path::new("/srv"));
    assert!(sites.named()["a"].hsts.is_none());
    assert!(
        sites.default_site().hsts.is_some(),
        "the deployment keeps its own policy"
    );
}

#[test]
fn hsts_is_refused_where_it_could_never_be_honoured() {
    // An address-only site: a browser files a policy against a *name*, so a policy on a site
    // selected by address alone is one every client that could reach it would ignore.
    let text = r#"
[server]
http = "127.0.0.1:80"
https = "127.0.0.1:443"

[server.tls]
certificate = "c.crt"
key = "c.key"

[sites.internal]
addresses = ["192.168.9.123"]
document_root = "/srv/internal"

[sites.internal.hsts]
max_age_s = 600
"#;
    let err = Config::from_toml(text)
        .expect("parses")
        .validate()
        .expect_err("must fail");
    assert!(err.to_string().contains("no `hosts`"), "{err}");

    // A site restricted to cleartext never answers over a secure transport, and RFC 6797 §7.2
    // forbids the header anywhere else.
    let text = split_https(
        "",
        "protocols = [\"http\"]\n[sites.a.hsts]\nmax_age_s = 600",
    );
    let err = Config::from_toml(&text)
        .expect("parses")
        .validate()
        .expect_err("must fail");
    assert!(
        err.to_string().contains("does not include \"https\""),
        "{err}"
    );

    // No TLS at all, so there is no secure transport to send it on.
    let text = r#"
[server]
http = "127.0.0.1:80"

[server.hsts]
max_age_s = 600
"#;
    let err = Config::from_toml(text)
        .expect("parses")
        .validate()
        .expect_err("must fail");
    assert!(err.to_string().contains("[server.tls]"), "{err}");
}

#[test]
fn preload_is_refused_below_the_lists_own_bar() {
    // Preload is the one setting here that ships inside browsers, so the two requirements the
    // list actually enforces are checked rather than accepted and rejected years later.
    let short = split_https(
        "",
        "[sites.a.hsts]\nmax_age_s = 600\ninclude_subdomains = true\npreload = true",
    );
    let err = Config::from_toml(&short)
        .expect("parses")
        .validate()
        .expect_err("must fail");
    assert!(err.to_string().contains("one year"), "{err}");

    let no_subdomains = split_https("", "[sites.a.hsts]\nmax_age_s = 31536000\npreload = true");
    let err = Config::from_toml(&no_subdomains)
        .expect("parses")
        .validate()
        .expect_err("must fail");
    assert!(err.to_string().contains("include_subdomains"), "{err}");

    Config::from_toml(&split_https(
        "",
        "[sites.a.hsts]\nmax_age_s = 31536000\ninclude_subdomains = true\npreload = true",
    ))
    .expect("parses")
    .validate()
    .expect("a year with subdomains is what the preload list asks for");
}

#[test]
fn a_zero_max_age_is_refused_because_the_header_means_something_else_by_it() {
    // `max-age=0` is not "off"; it is "delete the policy". Writing it in a table is a second
    // statement about the same thing, so the table is refused and `enabled = false` is the answer.
    let text = split_https("", "[sites.a.hsts]\nmax_age_s = 0");
    let err = Config::from_toml(&text)
        .expect("parses")
        .validate()
        .expect_err("must fail");
    assert!(err.to_string().contains("delete this policy"), "{err}");
}

#[test]
fn a_wildcard_site_still_carries_its_policy() {
    // A wildcard is a pattern rather than a name, so it is not checked against the preflight
    // subdomain rule — but it is a legitimate site, and its policy must resolve like any other.
    let text = r#"
[server]
http = "127.0.0.1:80"
https = "127.0.0.1:443"

[server.tls]
certificate = "c.crt"
key = "c.key"

[sites.wild]
hosts = ["*.example.com"]
document_root = "/srv/wild"

[sites.wild.hsts]
max_age_s = 600
"#;
    let config = Config::from_toml(text).expect("parses");
    config.validate().expect("valid");
    let sites = crate::sites::Sites::from_config(&config, std::path::Path::new("/srv"));
    assert_eq!(
        sites.named()["wild"].hsts.as_ref().map(|p| p.max_age_s),
        Some(600)
    );
}

#[test]
fn the_header_value_is_spelled_the_way_the_specification_writes_it() {
    use crate::config::HstsPolicy;

    // Directive order and spelling are what an operator reads in `check` and in their own
    // responses; `includeSubDomains` keeps its camel case because every published example does.
    let bare = HstsPolicy {
        max_age_s: 600,
        include_subdomains: false,
        preload: false,
    };
    assert_eq!(bare.header_value(), "max-age=600");

    let full = HstsPolicy {
        max_age_s: 31_536_000,
        include_subdomains: true,
        preload: true,
    };
    assert_eq!(
        full.header_value(),
        "max-age=31536000; includeSubDomains; preload"
    );
}

// ---- TLS -------------------------------------------------------------------------
