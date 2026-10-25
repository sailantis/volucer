//! gRPC service grants, and the call namespaces a backend may be given.

use super::*;

/// A misspelled namespace in `services` is a startup failure, not a silent refusal.
///
/// The failure this exists to prevent is quiet in a specific way. `services = ["workes"]` grants a
/// worker no capability at all, and the deployment boots, serves every request, and only shows the
/// problem when the application asks the server to reset a worker and is told the method is not
/// registered — which reads as a server bug rather than as a typo in a configuration file. So the
/// name is checked against the allow-list where the file is read, which is the same place
/// `deny_unknown_fields` refuses a misspelled *key*.
#[test]
fn a_misspelled_namespace_in_services_is_refused_by_name() {
    let text = r#"
[server]
default_backend = "app"
http = "127.0.0.1:8080"
[backends.app]
kind = "worker"
command = "php worker.php"
services = ["workes"]
"#;
    let config = Config::from_toml(text).expect("the file parses; the value is what is wrong");
    let err = config.validate().expect_err("must not validate");
    let message = err.to_string();
    assert!(
        message.contains("\"workes\""),
        "the error must quote the value that is wrong: {message}"
    );
    // The message names the alternatives, because an operator reading it has to fix the file
    // from the message alone.
    assert!(
        message.contains("\"config\"") && message.contains("\"workers\""),
        "the error must name the valid namespaces: {message}"
    );
}

/// A namespace named twice is refused, because two lines would disagree and one is read.
///
/// Harmless at this instant — the registry is a map, so a duplicate is absorbed — and worth
/// refusing anyway: a duplicate is what a hand-merged configuration looks like right before the
/// two copies diverge, and the divergence would then be invisible because the first copy wins.
#[test]
fn a_namespace_listed_twice_in_services_is_refused() {
    let text = r#"
[server]
default_backend = "app"
http = "127.0.0.1:8080"
[backends.app]
kind = "worker"
command = "php worker.php"
services = ["config", "config"]
"#;
    let config = Config::from_toml(text).expect("the file parses");
    let err = config.validate().expect_err("must not validate");
    assert!(
        err.to_string().contains("twice"),
        "the error must say what is wrong: {err}"
    );
}

/// A gRPC path is split into a service and a method, and only a gRPC path is.
///
/// The refusals matter more than the acceptance, because the thing a lenient answer does is route
/// an arbitrary URL into a worker that believed it was reading a method name. Each case below is a
/// way that could happen.
#[test]
fn a_grpc_path_is_two_segments_and_both_have_to_be_names() {
    let split = |path: &str| {
        parse_grpc_path(path).map(|call| (call.service.to_string(), call.method.to_string()))
    };

    assert_eq!(
        split("/greeter.Greeter/SayHello"),
        Some(("greeter.Greeter".to_string(), "SayHello".to_string()))
    );
    // A package with several components is still one service name, and a method starting with an
    // underscore is legal protobuf.
    assert_eq!(
        split("/a.b.c.v1_2/Get_Thing"),
        Some(("a.b.c.v1_2".to_string(), "Get_Thing".to_string()))
    );

    // Not a gRPC path at all: no leading slash, no method, an empty method.
    assert_eq!(split("greeter.Greeter/SayHello"), None);
    assert_eq!(split("/greeter.Greeter"), None);
    assert_eq!(split("/greeter.Greeter/"), None);
    assert_eq!(split("/"), None);
    assert_eq!(split(""), None);
    // **A third segment is not a deeper method.** gRPC's grammar has no room for one, so treating
    // it as a method would hand an application the string `SayHello/extra` as a method name.
    assert_eq!(split("/a.B/M/extra"), None);
    // A query string is not stripped: gRPC addresses a method by `:path` alone, and the `?` that
    // would begin one is not a byte a protobuf identifier may contain, so it is refused by the
    // same rule that refuses a hyphen. The client's own implementation of this rule makes the same
    // choice, and the two must not differ.
    assert_eq!(split("/a.B/M?x=1"), None);
    // Neither part may be something other than a protobuf name.
    assert_eq!(split("/-/M"), None, "a service is not a punctuation mark");
    assert_eq!(
        split("/a.B/9M"),
        None,
        "a method does not start with a digit"
    );
    assert_eq!(
        split("/a..B/M"),
        None,
        "an empty package part is not a name"
    );
    assert_eq!(split("/.a.B/M"), None);
    assert_eq!(split("/a.B./M"), None);
    assert_eq!(split("/a-B/M"), None, "a hyphen is not an identifier byte");
    assert_eq!(split("/a.B/M-x"), None);
}

/// A service name is validated the same way as the service a path names, and both are
/// case-sensitive.
///
/// One function, deliberately: if the configuration's rule and the path's rule were two, a
/// deployment could name a service no request could ever address, or the two could disagree about
/// a name that is *nearly* legal — which is exactly the class of typo an allow-list catches.
#[test]
fn a_service_name_is_identifiers_joined_by_dots() {
    assert!(is_valid_grpc_service("Greeter"));
    assert!(is_valid_grpc_service("greeter.Greeter"));
    assert!(is_valid_grpc_service("a.b.c.v1"));
    assert!(is_valid_grpc_service("_hidden.v1"));

    assert!(!is_valid_grpc_service(""));
    assert!(!is_valid_grpc_service(".Greeter"));
    assert!(!is_valid_grpc_service("Greeter."));
    assert!(!is_valid_grpc_service("greeter..Greeter"));
    assert!(!is_valid_grpc_service("greeter/Greeter"));
    assert!(!is_valid_grpc_service("9greeter.Greeter"));
    assert!(!is_valid_grpc_service("greeter.Greeter "));

    // Case-sensitive, and this is the one that would otherwise let a typo be granted by accident:
    // protobuf identifiers are case-sensitive, so these are two different services.
    assert_eq!(
        parse_grpc_path("/greeter.greeter/M")
            .map(|call| call.service)
            .filter(|service| *service == "greeter.Greeter"),
        None,
        "a granted `greeter.Greeter` must not admit `greeter.greeter`"
    );
    assert!(is_valid_grpc_service("greeter.Greeter") && is_valid_grpc_service("greeter.greeter"));
}

/// A service outside a backend's grant never reaches it, which is the whole of the key.
///
/// Both halves asserted, because a grant that admits everything would pass a test that only checked
/// the granted case — and the failure mode of the lenient reading is silent: the call arrives, the
/// application has no handler for it, and the client gets an error from the application rather than
/// the refusal the server should have made.
#[test]
fn only_a_granted_service_reaches_a_worker_backend() {
    let text = r#"
[server]
default_backend = "app"
http = "127.0.0.1:8080"
[backends.app]
kind = "worker"
command = "php worker.php"
grpc_services = ["greeter.Greeter"]
"#;
    let config = Config::from_toml(text).expect("the file parses");
    config.validate().expect("a legal service name validates");

    assert!(config.serves_grpc("app", "/greeter.Greeter/SayHello"));
    // Any method on a granted service: a method the application does not implement is one it can
    // answer `UNIMPLEMENTED` to itself, so listing methods here would put the application's
    // interface in the server's file.
    assert!(config.serves_grpc("app", "/greeter.Greeter/Anything"));

    // A different service, a different case, and a path that is not a method path.
    assert!(!config.serves_grpc("app", "/greeter.greeter/SayHello"));
    assert!(!config.serves_grpc("app", "/other.Service/SayHello"));
    assert!(!config.serves_grpc("app", "/greeter.Greeter"));
    assert!(!config.serves_grpc("app", "/a.B/M/extra"));
    // An unknown backend is not a grant either.
    assert!(!config.serves_grpc("nope", "/greeter.Greeter/SayHello"));
}

/// `grpc_services` is absent by default, and the default is the safe one.
///
/// The same argument `services` makes, in the other direction. A worker that was never meant to
/// speak gRPC answers `UNIMPLEMENTED` to every gRPC call, which is also the right answer for a
/// plain PHP application a misconfigured client happens to send `content-type: application/grpc`
/// to — its routes are not method paths, and nothing about them should become deliverable because
/// a client lied about its protocol.
#[test]
fn grpc_services_is_absent_by_default_and_absent_grants_nothing() {
    let text = r#"
[server]
default_backend = "app"
http = "127.0.0.1:8080"
[backends.app]
kind = "worker"
command = "php worker.php"
"#;
    let config = Config::from_toml(text).expect("the file parses");
    config
        .validate()
        .expect("no grant is not a misconfiguration");
    assert!(config.backends["app"].kind() == "worker");
    assert!(
        !config.serves_grpc("app", "/greeter.Greeter/SayHello"),
        "absenting `grpc_services` must grant nothing, not everything"
    );
}

/// A name in `grpc_services` that is not a protobuf service name is refused where the file is read.
///
/// The failure mode it prevents is quiet. `grpc_services = ["Greeter"]` beside a client calling
/// `/greeter.Greeter/SayHello` — a name written without its package, which is what an operator
/// copying from a method name rather than from a `.proto` produces — is a worker granted nothing,
/// so every call is answered `UNIMPLEMENTED` and no log line says why.
#[test]
fn a_grpc_service_name_that_is_not_a_protobuf_name_is_refused() {
    let with = |service: &str| {
        let text = format!(
            r#"
[server]
default_backend = "app"
http = "127.0.0.1:8080"
[backends.app]
kind = "worker"
command = "php worker.php"
grpc_services = ["{service}"]
"#
        );
        Config::from_toml(&text)
            .expect("the file parses; the value is what is wrong")
            .validate()
    };

    with("greeter.Greeter").expect("a legal name validates");

    for bad in ["Greeter/", "greeter.Greeter/SayHello", "", "a b.C", "9a.B"] {
        let err = with(bad).expect_err("must not validate");
        let message = err.to_string();
        assert!(
            message.contains("grpc_services"),
            "the error must name the key: {message}"
        );
        assert!(
            message.contains("greeter.Greeter"),
            "and show what a service name looks like, because the operator has to fix the file \
             from the message alone: {message}"
        );
    }
}

/// `grpc_serve_any` admits every well-formed method path, and nothing else.
///
/// The opt-out from the allow-list, and the two halves of it are what make it safe rather than
/// merely permissive: the *shape* check still applies (a URL is not a method name), and the flag is
/// something an operator wrote down rather than the absence of a list — which is why deleting
/// `grpc_services` still grants nothing.
///
/// The `attach` half is asserted too, because the two tables carry the same key and a reader will
/// assume they agree; if they did not, one of them would be a silently ignored setting.
#[test]
fn grpc_serve_any_admits_every_service_but_no_other_path() {
    let text = r#"
[server]
default_backend = "app"
http = "127.0.0.1:8080"
[backends.app]
kind = "worker"
command = "php worker.php"
grpc_serve_any = true
"#;
    let config = Config::from_toml(text).expect("the file parses");
    config.validate().expect("the flag alone validates");

    assert!(config.serves_grpc("app", "/greeter.Greeter/SayHello"));
    // A service nobody listed, which is the whole point.
    assert!(config.serves_grpc("app", "/never.Listed/Anything"));
    // Case still matters: these are different services, and "any" is not "any spelling".
    assert!(config.serves_grpc("app", "/greeter.greeter/SayHello"));
    assert!(config.serves_grpc("app", "/a.B/M"));

    // The shape rule is *not* relaxed, and this is the half that stops an arbitrary URL becoming a
    // method name a worker reads.
    for refused in [
        "/greeter.Greeter",
        "/a.B/M/extra",
        "/a.B/M?x=1",
        "/",
        "//",
        "/a.B/",
        "/greeter.Greeter/Say-Hello",
    ] {
        assert!(
            !config.serves_grpc("app", refused),
            "{refused:?} is not a gRPC method path and must be refused even under grpc_serve_any"
        );
    }

    // The `attach` table answers the same way.
    let attach = r#"
[server]
default_backend = "app"
http = "127.0.0.1:8080"
[backends.app]
kind = "attach"
workers = 1
relay = "unix:/run/app/workers.sock"
grpc_serve_any = true
"#;
    let config = Config::from_toml(attach).expect("the file parses");
    config.validate().expect("the flag alone validates");
    assert!(config.serves_grpc("app", "/never.Listed/Anything"));
}

/// `grpc_serve_any` beside a non-empty `grpc_services` is refused, on both tables.
///
/// Two statements about one thing, and every resolution of them is wrong: honouring the list
/// silently ignores the opt-out, honouring the opt-out silently ignores a list an operator wrote for
/// a reason. The same shape as `header` beside a key mode that does not read one.
#[test]
fn grpc_serve_any_beside_a_list_is_refused() {
    let worker = r#"
[server]
default_backend = "app"
http = "127.0.0.1:8080"
[backends.app]
kind = "worker"
command = "php worker.php"
grpc_services = ["a.B"]
grpc_serve_any = true
"#;
    let err = Config::from_toml(worker)
        .expect("the file parses; the pair is what is wrong")
        .validate()
        .expect_err("must not validate");
    assert!(
        err.to_string().contains("grpc_serve_any"),
        "the error has to name the key to remove: {err}"
    );

    let attach = r#"
[server]
default_backend = "app"
http = "127.0.0.1:8080"
[backends.app]
kind = "attach"
workers = 1
relay = "unix:/run/app/workers.sock"
grpc_services = ["a.B"]
grpc_serve_any = true
"#;
    let err = Config::from_toml(attach)
        .expect("the file parses; the pair is what is wrong")
        .validate()
        .expect_err("must not validate");
    assert!(
        err.to_string().contains("grpc_serve_any"),
        "the error has to name the key to remove: {err}"
    );
}

/// An `attach` backend's service names are checked where the file is read, as a `worker`'s are.
///
/// The same silent failure: a `grpc_services` entry that is not a legal protobuf name admits nothing
/// on an attached pool, so every call to the service it was meant to grant is answered
/// `UNIMPLEMENTED` and no line says why. Worth its own test because the two tables validate through
/// different functions, and only one of them had this check until it was written.
///
/// The value is a *path*, not a package-less name: `"Greeter"` is a legal protobuf service name
/// (protobuf does not require a package), so it is accepted and grants only `/Greeter/Method`.
#[test]
fn an_attach_backends_grpc_service_name_is_checked() {
    let text = r#"
[server]
default_backend = "app"
http = "127.0.0.1:8080"
[backends.app]
kind = "attach"
workers = 1
relay = "unix:/run/app/workers.sock"
grpc_services = ["greeter.Greeter/SayHello"]
"#;
    let err = Config::from_toml(text)
        .expect("the file parses; the value is what is wrong")
        .validate()
        .expect_err("must not validate");
    assert!(
        err.to_string().contains("protobuf service name"),
        "the error has to say what a service name looks like: {err}"
    );
}

/// A service listed twice in `grpc_services` is refused, for `services`' own reason.
#[test]
fn a_grpc_service_listed_twice_is_refused() {
    let text = r#"
[server]
default_backend = "app"
http = "127.0.0.1:8080"
[backends.app]
kind = "worker"
command = "php worker.php"
grpc_services = ["a.B", "a.B"]
"#;
    let config = Config::from_toml(text).expect("the file parses");
    let err = config.validate().expect_err("must not validate");
    assert!(err.to_string().contains("twice"), "{err}");
}

/// `grpc_services` beside a backend with no channel to deliver a call through is refused.
///
/// `deny_unknown_fields` is what makes this an error rather than a silently ignored key — the same
/// argument the `services` key's own test makes, and the reason it is worth a second test is that
/// the two keys are on the same table and a reader will assume they behave alike. They do; what
/// this asserts is that the *table* they are on is the only one that has them.
#[test]
fn grpc_services_beside_a_kind_that_has_no_channel_is_refused() {
    let text = r#"
[server]
default_backend = "app"
http = "127.0.0.1:8080"
[backends.app]
kind = "external"
socket = "127.0.0.1:9000"
grpc_services = ["a.B"]
"#;
    let err = Config::from_toml(text).expect_err("must fail");
    assert!(
        err.to_string().contains("grpc_services"),
        "the error has to name the key that does not belong: {err}"
    );
}
