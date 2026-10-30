//! The route table: specificity, ties, policies, and what a route may not say.

use super::*;

fn routed_config() -> Config {
    let text = r#"
[server]
default_backend = "app"
http = "127.0.0.1:8080"

[backends.app]
kind = "worker"
command = "php worker.php"

[backends.legacy]
kind = "worker"
command = "php legacy.php"

[[server.routes]]
path_prefix = "/api"
backend = "legacy"

[[server.routes]]
host = "admin.example"
path_prefix = "/"
backend = "legacy"
"#;
    let config = Config::from_toml(text).expect("config parses");
    config.validate().expect("config validates");
    config
}

#[test]
fn a_route_selects_a_backend_by_path_prefix() {
    let config = routed_config();
    assert_eq!(config.route_backend(None, "/api/users"), Some("legacy"));
    assert_eq!(config.route_backend(None, "/other"), None);
    // A prefix match is textual, matching how nginx `location` behaves.
    assert_eq!(config.route_backend(None, "/apix"), Some("legacy"));
}

#[test]
fn a_host_route_ignores_the_port_and_the_case() {
    let config = routed_config();
    assert_eq!(
        config.route_backend(Some("Admin.Example:8080"), "/"),
        Some("legacy")
    );
    assert_eq!(
        config.route_backend(Some("admin.example"), "/x"),
        Some("legacy")
    );
    assert_eq!(config.route_backend(Some("public.example"), "/x"), None);
}

#[test]
fn the_most_specific_matching_route_wins() {
    // Both routes match `admin.example/api`; the one that states two
    // constraints beats the one that states a path alone.
    let text = r#"
[server]
default_backend = "app"
http = "127.0.0.1:8080"

[backends.app]
kind = "worker"
command = "php worker.php"

[backends.path_only]
kind = "worker"
command = "php path.php"

[backends.both]
kind = "worker"
command = "php both.php"

[[server.routes]]
path_prefix = "/api"
backend = "path_only"

[[server.routes]]
host = "admin.example"
path_prefix = "/"
backend = "both"
"#;
    let config = Config::from_toml(text).expect("config parses");
    config.validate().expect("config validates");
    assert_eq!(
        config.route_backend(Some("admin.example"), "/api/x"),
        Some("both")
    );
}

#[test]
fn rejects_a_route_naming_an_unknown_backend() {
    let text = r#"
[server]
default_backend = "app"
http = "127.0.0.1:8080"
[backends.app]
kind = "worker"
command = "php worker.php"
[[server.routes]]
path_prefix = "/api"
backend = "missing"
"#;
    let config = Config::from_toml(text).expect("config parses");
    let err = config.validate().expect_err("must fail");
    assert!(err.to_string().contains("routes[0].backend"), "{err}");
}

#[test]
fn a_route_without_a_backend_is_refused_unless_it_is_files_only() {
    // The rule, and it is not decoration: a route that can reach a backend and does not name
    // one would silently inherit the *site's* `default_backend`, so the request would be
    // answered by a backend this route's own table never mentioned. `static` is the one
    // policy that never dispatches, so it is the one that may omit the name.
    let with_route = |route: &str| {
        format!(
            r#"
[server]
default_backend = "app"
http = "127.0.0.1:8080"
[backends.app]
kind = "worker"
command = "php worker.php"
[[server.routes]]
path_prefix = "/assets"
{route}
"#
        )
    };

    for policy in ["", "policy = \"all\"", "policy = \"backend\""] {
        let text = with_route(policy);
        let config = Config::from_toml(&text).expect("config parses");
        let err = config.validate().expect_err(&format!(
            "policy {policy:?} without a backend must be refused"
        ));
        assert!(
            err.to_string().contains("routes[0].backend is required"),
            "policy {policy:?}: {err}"
        );
    }

    let text = with_route("policy = \"static\"");
    let config = Config::from_toml(&text).expect("config parses");
    config
        .validate()
        .expect("a files-only route may omit the backend, because it never dispatches");
}

#[test]
fn a_route_policy_defaults_to_all_so_existing_configuration_is_unchanged() {
    // The compatibility claim, asserted rather than assumed: every route written before
    // `policy` existed must behave exactly as it did.
    let text = r#"
[server]
default_backend = "app"
http = "127.0.0.1:8080"
[backends.app]
kind = "worker"
command = "php worker.php"
[[server.routes]]
path_prefix = "/api"
backend = "app"
"#;
    let config = Config::from_toml(text).expect("config parses");
    config.validate().expect("valid");
    let route = &config.server.routes[0];
    assert_eq!(RoutePolicy::All, route.policy);
    assert_eq!(Some("app"), route.backend.as_deref());
    assert!(route.policy.serves_files(), "all serves files");
    assert!(route.policy.serves_backend(), "and reaches a backend");
}

#[test]
fn each_policy_answers_which_layers_run() {
    // The predicate the request path uses, in isolation from it, so a routing regression
    // and a policy regression cannot be mistaken for each other.
    assert!(RoutePolicy::All.serves_files() && RoutePolicy::All.serves_backend());
    assert!(!RoutePolicy::Backend.serves_files() && RoutePolicy::Backend.serves_backend());
    assert!(RoutePolicy::Static.serves_files() && !RoutePolicy::Static.serves_backend());
    assert_eq!("all", RoutePolicy::All.as_str());
    assert_eq!("backend", RoutePolicy::Backend.as_str());
    assert_eq!("static", RoutePolicy::Static.as_str());
}

#[test]
fn a_files_only_route_never_skips_the_stat() {
    // `skip_stat` takes "the backend does not interpret the path" as its reason to skip.
    // A route with no backend is vacuously "a backend that does not interpret", and reading
    // that as a skip would remove the check the files-only subtree depends on for its 404.
    let route = RouteConfig {
        path_prefix: Some("/assets".to_string()),
        path_suffix: None,
        host: None,
        policy: RoutePolicy::Static,
        backend: None,
        redirect: None,
    };
    let extensions = crate::script::ScriptExtensions::default();
    assert!(
        !skip_stat(&route, false, &extensions),
        "no backend means the stat is the only thing resolving the request"
    );
}

#[test]
fn rejects_a_route_that_matches_everything() {
    let text = r#"
[server]
default_backend = "app"
http = "127.0.0.1:8080"
[backends.app]
kind = "worker"
command = "php worker.php"
[[server.routes]]
backend = "app"
"#;
    let config = Config::from_toml(text).expect("config parses");
    let err = config.validate().expect_err("must fail");
    assert!(
        err.to_string().contains("path_prefix, path_suffix or host"),
        "{err}"
    );
}

#[test]
fn rejects_a_path_prefix_without_a_leading_slash() {
    let text = r#"
[server]
default_backend = "app"
http = "127.0.0.1:8080"
[backends.app]
kind = "worker"
command = "php worker.php"
[[server.routes]]
path_prefix = "api"
backend = "app"
"#;
    let config = Config::from_toml(text).expect("config parses");
    let err = config.validate().expect_err("must fail");
    assert!(err.to_string().contains("must start with"), "{err}");
}
#[test]
fn a_tie_goes_to_the_earlier_route() {
    // The documented rule, and the one this table's own docstring states: "ties are broken by
    // declaration order so the earlier entry wins". `Iterator::max_by_key` breaks the other way —
    // it returns the *last* maximum — so this assertion is what keeps the implementation and the
    // documentation the same claim.
    //
    // **It is not a cosmetic ordering.** `[rpc]` derives a backend's token from the name a route
    // selects, so two routes that tie and name different backends decide which call plane a request
    // is handed. Before `[rpc]` existed a tie meant two routes that behaved identically; the test
    // exists because that stopped being true.
    let text = r#"
[server]
default_backend = "first"
http = "127.0.0.1:8080"

[backends.first]
kind = "worker"
command = "php first.php"

[backends.second]
kind = "worker"
command = "php second.php"

[[server.routes]]
path_prefix = "/api"
backend = "first"

[[server.routes]]
path_prefix = "/api"
backend = "second"
"#;
    let config = Config::from_toml(text).expect("config parses");
    config.validate().expect("config validates");
    assert_eq!(
        config.route_backend(None, "/api/x"),
        Some("first"),
        "an equally specific later route must not displace an earlier one"
    );
}

#[test]
fn a_more_specific_later_route_still_beats_an_earlier_one() {
    // The other half of the same rule: declaration order settles only a *tie*. A later route that
    // states more constraints wins, which is what makes the previous test about ties rather than
    // about position in the file.
    let text = r#"
[server]
default_backend = "plain"
http = "127.0.0.1:8080"

[backends.plain]
kind = "worker"
command = "php plain.php"

[backends.specific]
kind = "worker"
command = "php specific.php"

[[server.routes]]
path_prefix = "/api"
backend = "plain"

[[server.routes]]
host = "api.example"
path_prefix = "/"
backend = "specific"
"#;
    let config = Config::from_toml(text).expect("config parses");
    config.validate().expect("config validates");
    assert_eq!(
        config.route_backend(Some("api.example"), "/api/x"),
        Some("specific"),
        "specificity is decided before declaration order"
    );
}

// ------------------------------------------------------------ redirects

/// A minimal configuration around a route table, without needing a backend.
fn redirect_config(routes: &str) -> Config {
    let text = format!(
        r#"
[server]
http = "127.0.0.1:8080"
document_root = "."
{routes}
"#
    );
    Config::from_toml(&text).expect("the fixture parses")
}

#[test]
fn a_redirect_route_needs_no_backend_and_accepts_the_five_redirection_statuses() {
    // The `backend` requirement exists because a route that *can* dispatch must say where. A
    // redirect dispatches nowhere, so naming none is correct — and naming one is refused, because a
    // line that does nothing is the shape this file refuses everywhere else.
    for status in [301u16, 302, 303, 307, 308] {
        let config = redirect_config(&format!(
            r#"
[[server.routes]]
path_prefix = "/old"
redirect = {{ status = {status}, to = "/new" }}
"#
        ));
        config
            .validate()
            .unwrap_or_else(|err| panic!("status {status} is a redirection: {err}"));
    }

    let both = redirect_config(
        r#"
[[server.routes]]
path_prefix = "/old"
backend = "app"
redirect = { status = 301, to = "/new" }
"#,
    );
    let err = both
        .validate()
        .expect_err("a redirect and a backend cannot both be stated");
    assert!(format!("{err}").contains("redirect"), "{err}");
}

#[test]
fn a_redirect_refuses_a_status_that_is_not_a_redirection() {
    // `300`, `304` and `305` are in the 3xx range and are not this, and a `200` in a `redirect` is a
    // route that answers successfully and never redirects — the "looks configured, does nothing"
    // shape.
    for status in [200u16, 300, 304, 305, 399, 404] {
        let config = redirect_config(&format!(
            r#"
[[server.routes]]
path_prefix = "/old"
redirect = {{ status = {status}, to = "/new" }}
"#
        ));
        let err = config
            .validate()
            .expect_err("only 301/302/303/307/308 are redirections");
        assert!(
            format!("{err}").contains(&status.to_string()),
            "the message must name the status: {err}"
        );
    }
}

#[test]
fn a_redirect_target_must_be_absolute_or_a_path() {
    // A `Location` of `example.com/x` is read as a *relative* path by every client, so it resolves
    // to `/example.com/x` on the deploying host — a redirect to nothing, found in production.
    let config = redirect_config(
        r#"
[[server.routes]]
path_prefix = "/old"
redirect = { status = 301, to = "example.com/new" }
"#,
    );
    let err = config
        .validate()
        .expect_err("a bare host is not a redirect target");
    assert!(format!("{err}").contains("example.com/new"), "{err}");
    assert!(format!("{err}").contains("relative path"), "{err}");

    // And the three shapes that are fine: root-relative, absolute, and a token-only target.
    for to in ["/new", "https://new.example/x", "{path}"] {
        let config = redirect_config(&format!(
            r#"
[[server.routes]]
path_prefix = "/old"
redirect = {{ status = 301, to = "{to}" }}
"#
        ));
        // `{path}` beside `path_prefix = "/old"` is not a loop: it strips nothing but also does not
        // re-add the prefix, so a client is sent to the path it asked for — which *is* a loop, so
        // only check the two that are unambiguously fine here.
        if to == "{path}" {
            continue;
        }
        config
            .validate()
            .unwrap_or_else(|err| panic!("{to:?} is a valid target: {err}"));
    }
}

#[test]
fn a_redirect_that_loops_back_to_its_own_prefix_is_refused() {
    // `path_prefix = "/old"` with `to = "/old{path}"` maps every matched request onto itself: a
    // browser hides the loop from the operator and a scraper follows it for ever.
    let config = redirect_config(
        r#"
[[server.routes]]
path_prefix = "/old"
redirect = { status = 301, to = "/old{path}" }
"#,
    );
    let err = config
        .validate()
        .expect_err("a self-referential redirect is a loop");
    assert!(format!("{err}").contains("/old{path}"), "{err}");
    assert!(format!("{err}").contains("for ever"), "{err}");

    // A constant target under the route's own prefix loops too, and for the same reason: the
    // redirect lands on this route again.
    let constant = redirect_config(
        r#"
[[server.routes]]
path_prefix = "/blog"
redirect = { status = 302, to = "/blog/archive" }
"#,
    );
    constant
        .validate()
        .expect_err("a constant target under the same prefix is a loop");

    // The negative control: the same shape one prefix over is the ordinary migration and must be
    // accepted, or the check above would refuse the feature it exists for.
    let fine = redirect_config(
        r#"
[[server.routes]]
path_prefix = "/blog"
redirect = { status = 301, to = "/news{path}" }
"#,
    );
    fine.validate().expect("a different prefix is the migration shape");
}

#[test]
fn a_redirect_cannot_share_a_route_with_a_policy() {
    // `policy` decides which layer answers, and a redirect answers before either is consulted — so
    // stating one beside `redirect` is a line that appears to matter and does not.
    let config = redirect_config(
        r#"
[[server.routes]]
path_prefix = "/old"
policy = "static"
redirect = { status = 301, to = "/new" }
"#,
    );
    let err = config
        .validate()
        .expect_err("a redirect makes the policy inert");
    assert!(format!("{err}").contains("policy"), "{err}");
}

#[test]
fn a_redirect_outranks_an_equally_specific_route_that_serves() {
    // The tie rule: a deployment migrating `/old` would otherwise lose to the route that was there
    // before it, because a tie goes to the earlier entry — and the earlier entry is the one being
    // retired.
    let config = Config::from_toml(
        r#"
[server]
http = "127.0.0.1:8080"
document_root = "."
default_backend = "app"

[backends.app]
kind = "worker"
command = "php worker.php"

[[server.routes]]
path_prefix = "/old"
backend = "app"

[[server.routes]]
path_prefix = "/old"
redirect = { status = 301, to = "/new" }
"#,
    )
    .expect("the fixture parses");
    config.validate().expect("the fixture validates");
    let selected = super::select_route(&config.server.routes, None, "/old/x")
        .expect("one of the two matches");
    assert!(
        selected.redirect.is_some(),
        "the redirect must win the tie, not the route it is retiring"
    );
}
