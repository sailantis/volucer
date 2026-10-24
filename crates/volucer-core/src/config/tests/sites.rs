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

    // No hosts.
    let text = format!("{base}\n[sites.a]\nhosts = []\ndocument_root = \"/srv/a\"\n");
    let config = Config::from_toml(&text).expect("parses");
    let err = config.validate().expect_err("must fail");
    assert!(err.to_string().contains("hosts is empty"), "{err}");

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

// ---- TLS -------------------------------------------------------------------------
