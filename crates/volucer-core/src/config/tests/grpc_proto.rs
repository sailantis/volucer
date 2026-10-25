//! `grpc_proto`: reading a backend's grants out of `.proto` files.
//!
//! The scan itself is tested in `crate::proto`; what is asserted here is the part that turns a
//! configuration into a grant — which files an entry names, what a relative entry means, and the
//! refusals that stand between a mistyped directory and a worker granted nothing.

use super::*;

/// A directory that removes itself, named after the test so two tests cannot collide.
struct Fixture(PathBuf);

impl Fixture {
    fn new(name: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("volucer-grpc-proto-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create the fixture");
        Fixture(path)
    }

    fn write(&self, relative: &str, text: &str) {
        let path = self.0.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create the parent");
        }
        std::fs::write(&path, text).expect("write the fixture");
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn worker_config(proto_entries: &str) -> Config {
    let text = format!(
        r#"
[server]
default_backend = "app"
http = "127.0.0.1:8080"
[backends.app]
kind = "worker"
command = "php worker.php"
grpc_proto = [{proto_entries}]
"#
    );
    Config::from_toml(&text).expect("the file parses")
}

/// A file entry is read, and its services become the backend's grants.
///
/// Both halves asserted, because they are different claims: the list is what `check` prints, and the
/// grant is what the listener consults. A key that filled one and not the other would look right in
/// a report and answer `UNIMPLEMENTED` on the wire.
#[test]
fn a_proto_file_becomes_the_backends_grants() {
    let fixture = Fixture::new("file");
    fixture.write(
        "greeter.proto",
        r#"
syntax = "proto3";
package greeter;
service Greeter {
  rpc SayHello (HelloRequest) returns (HelloReply);
}
"#,
    );

    let mut config = worker_config(r#""greeter.proto""#);
    config.validate().expect("a path alone validates");
    config
        .expand_grpc_proto(&fixture.0)
        .expect("the file resolves a service");

    let grants = config.grpc_grants("app").expect("a worker has grants");
    assert_eq!(grants.names(), ["greeter.Greeter"]);
    assert!(config.serves_grpc("app", "/greeter.Greeter/SayHello"));
    // The shape rule is not the file's business: a method on the service is the application's.
    assert!(config.serves_grpc("app", "/greeter.Greeter/Anything"));
    assert!(!config.serves_grpc("app", "/greeter.Other/SayHello"));
}

/// A directory is searched recursively, and a file that is not `.proto` is not read.
#[test]
fn a_directory_is_searched_recursively_and_only_for_proto_files() {
    let fixture = Fixture::new("dir");
    fixture.write(
        "api/v1/greeter.proto",
        "package greeter.v1;\nservice Greeter {}",
    );
    fixture.write(
        "api/v1/health.proto",
        "package health.v1;\nservice Health {}",
    );
    // Named to catch a walk that reads *everything* and lets the parse fail quietly.
    fixture.write("api/v1/notes.txt", "package greeter.v1;\nservice Nope {}");
    fixture.write("api/v1/README.md", "service AlsoNope {}");

    let mut config = worker_config(r#""api""#);
    config.validate().expect("a path alone validates");
    config
        .expand_grpc_proto(&fixture.0)
        .expect("the directory resolves two services");

    let grants = config.grpc_grants("app").expect("a worker has grants");
    assert_eq!(grants.names(), ["greeter.v1.Greeter", "health.v1.Health"]);
    assert!(!config.serves_grpc("app", "/greeter.v1.Nope/N"));
    assert!(!config.serves_grpc("app", "/AlsoNope/N"));
}

/// Two files declaring the same service produce one grant.
///
/// Sorted and deduplicated rather than concatenated: `grpc_services` refuses a duplicate name, so an
/// expansion that kept both would produce a configuration its own validator would reject — and the
/// failure would appear on a reload rather than at startup.
#[test]
fn a_service_declared_in_two_files_is_one_grant() {
    let fixture = Fixture::new("dup");
    fixture.write("a.proto", "package shared;\nservice Thing {}");
    fixture.write("b.proto", "package shared;\nservice Thing {}");
    fixture.write("c.proto", "package shared;\nservice Second {}");

    let mut config = worker_config(r#""a.proto", "b.proto", "c.proto""#);
    config.validate().expect("the paths alone validate");
    config
        .expand_grpc_proto(&fixture.0)
        .expect("the files resolve");

    let grants = config.grpc_grants("app").expect("a worker has grants");
    assert_eq!(grants.names(), ["shared.Second", "shared.Thing"]);
    // The names land in their own field, not in `grpc_services` — those two keys are mutually
    // exclusive, so an expansion that wrote the list would produce a configuration its own
    // validator refuses, and the only symptom would be a `check` on a file already loaded.
    let BackendConfig::Worker(worker) = &config.backends["app"] else {
        panic!("the fixture is a worker backend");
    };
    assert!(worker.grpc_services.is_empty());
    config
        .validate()
        .expect("the expanded configuration must still validate");
}

/// A relative entry is resolved against the configuration's directory, not the working directory.
#[test]
fn a_relative_entry_is_resolved_against_the_configuration_directory() {
    let fixture = Fixture::new("relative");
    fixture.write(
        "proto/greeter.proto",
        "package greeter;\nservice Greeter {}",
    );

    let mut config = worker_config(r#""proto/greeter.proto""#);
    config.validate().expect("a path alone validates");
    // The working directory is deliberately *not* the fixture's: an implementation that used it
    // would fail here rather than in production, under a systemd unit whose working directory is
    // something else again.
    config
        .expand_grpc_proto(&fixture.0)
        .expect("the path resolves against the configuration's directory");
    assert!(config.serves_grpc("app", "/greeter.Greeter/SayHello"));
}

/// Re-expanding after the file changed replaces the grants, in both directions.
///
/// The reload's half of the feature, asserted without a process: a service added to the `.proto` is
/// granted on the next expansion, and — the direction an allow-list has to move — one removed from
/// it stops being granted. An expansion that appended would pass every test that only ever added.
#[test]
fn re_expanding_follows_the_file_in_both_directions() {
    let fixture = Fixture::new("reload");
    fixture.write("greeter.proto", "package greeter;\nservice Greeter {}");

    let mut config = worker_config(r#""greeter.proto""#);
    config.validate().expect("a path alone validates");
    config.expand_grpc_proto(&fixture.0).expect("expands");
    assert!(config.serves_grpc("app", "/greeter.Greeter/SayHello"));
    assert!(!config.serves_grpc("app", "/greeter.v2.Greeter/SayHello"));

    fixture.write(
        "greeter.proto",
        "package greeter.v2;\nservice Greeter {}\nservice Admin {}",
    );
    config.expand_grpc_proto(&fixture.0).expect("expands again");
    assert!(
        !config.serves_grpc("app", "/greeter.Greeter/SayHello"),
        "the old name is gone from the file, so it must be gone from the grant"
    );
    assert!(config.serves_grpc("app", "/greeter.v2.Greeter/SayHello"));
    assert!(config.serves_grpc("app", "/greeter.v2.Admin/Anything"));
}

/// A path that is not there refuses the load, naming the key and the path.
#[test]
fn an_entry_that_is_not_there_is_refused() {
    let fixture = Fixture::new("missing");
    let mut config = worker_config(r#""not-there.proto""#);
    config.validate().expect("a path alone validates");

    let err = config
        .expand_grpc_proto(&fixture.0)
        .expect_err("a missing file must not resolve to an empty grant");
    let message = err.to_string();
    assert!(
        message.contains("grpc_proto") && message.contains("not-there.proto"),
        "the error has to name the key and the path the operator wrote: {message}"
    );
}

/// A set that resolves to no service at all refuses the load, rather than granting nothing.
///
/// **This is the refusal the key exists for.** A backend granted nothing answers `UNIMPLEMENTED` to
/// every call with no line in the log saying why — the exact silent failure `grpc_services` was
/// introduced to remove — and a mistyped directory, or a `.proto` that declares only messages, would
/// reproduce it under a key whose whole purpose is to stop it happening by accident.
#[test]
fn a_set_that_resolves_no_service_is_refused() {
    let fixture = Fixture::new("empty");
    fixture.write("messages.proto", "package app;\nmessage OnlyAMessage {}");

    let mut config = worker_config(r#""messages.proto""#);
    config.validate().expect("a path alone validates");

    let err = config
        .expand_grpc_proto(&fixture.0)
        .expect_err("a file with no service must not be a silent empty grant");
    let message = err.to_string();
    assert!(
        message.contains("resolved no gRPC service"),
        "the error has to say what went wrong: {message}"
    );
    assert!(
        message.contains("messages.proto"),
        "and name the file it read, so the operator knows which one: {message}"
    );

    // The same for a directory that holds no `.proto` at all.
    let empty = Fixture::new("empty-dir");
    std::fs::create_dir_all(empty.0.join("nothing")).expect("create the fixture");
    let mut config = worker_config(r#""nothing""#);
    config.validate().expect("a path alone validates");
    assert!(config.expand_grpc_proto(&empty.0).is_err());
}

/// `grpc_proto` beside either of the other two statements is refused on both tables.
///
/// Three ways to say one thing, and every resolution silently ignores two of them. Refused where the
/// file is read rather than at expansion, so the answer does not depend on whether the `.proto` files
/// happened to resolve.
#[test]
fn grpc_proto_beside_the_other_two_keys_is_refused() {
    let with = |kind: &str, extra: &str| {
        let text = format!(
            r#"
[server]
default_backend = "app"
http = "127.0.0.1:8080"
[backends.app]
kind = "{kind}"
{extra}
grpc_proto = ["greeter.proto"]
"#
        );
        Config::from_toml(&text)
            .expect("the file parses; the combination is what is wrong")
            .validate()
    };

    for (kind, kind_specific) in [
        ("worker", r#"command = "php worker.php""#),
        (
            "attach",
            "workers = 1\nrelay = \"unix:/run/app/workers.sock\"",
        ),
    ] {
        with(kind, kind_specific).expect("the path alone validates");

        let err = with(kind, &format!("{kind_specific}\ngrpc_services = [\"a.B\"]"))
            .expect_err("a list beside grpc_proto must be refused");
        assert!(
            err.to_string().contains("grpc_proto") && err.to_string().contains("grpc_services"),
            "the error has to name both keys: {err}"
        );

        let err = with(kind, &format!("{kind_specific}\ngrpc_serve_any = true"))
            .expect_err("serve-any beside grpc_proto must be refused");
        assert!(
            err.to_string().contains("grpc_proto"),
            "the error has to name the key to remove: {err}"
        );
    }
}

/// An entry that names nothing is refused where the file is read, without touching a disk.
///
/// `""` joins to the configuration's directory and would otherwise resolve the whole tree, which is
/// not what anyone wrote and is exactly the over-grant this key makes possible.
#[test]
fn an_empty_entry_is_refused_without_reading_a_disk() {
    let config = worker_config(r#""""#);
    let err = config
        .validate()
        .expect_err("an empty path is not a proto source");
    assert!(
        err.to_string().contains("names no file"),
        "the error has to say what is wrong with the entry: {err}"
    );
}

/// A backend that names services has no `grpc_proto`, and a backend with no entry is untouched.
///
/// The accessors have to answer for every kind, including the three that cannot serve gRPC at all —
/// `check` asks each backend, and a version that panicked or guessed would make one of them wrong.
#[test]
fn the_accessors_answer_for_every_kind() {
    let text = r#"
[server]
default_backend = "app"
http = "127.0.0.1:8080"
[backends.app]
kind = "worker"
command = "php worker.php"
grpc_services = ["a.B"]
[backends.legacy]
kind = "external"
socket = "127.0.0.1:9000"
grpc_proto_ignored = 1
"#;
    // The unknown key on `external` is refused, which is what keeps a `grpc_proto` on a kind with no
    // channel from being a silently ignored setting rather than a mistake.
    assert!(Config::from_toml(text).is_err());

    let text = text.replace("grpc_proto_ignored = 1", "");
    let config = Config::from_toml(&text).expect("the file parses");
    config.validate().expect("the file validates");

    assert!(config.grpc_proto("app").is_empty());
    assert!(config.grpc_proto("legacy").is_empty());
    assert!(config.grpc_proto("nope").is_empty());
    assert!(
        config.grpc_proto_all().is_empty(),
        "a backend that names its services has no proto source to report"
    );
    // Expanding a configuration with no entries is a no-op, not an error, and leaves the list alone.
    let mut config = config;
    config
        .expand_grpc_proto(Path::new("."))
        .expect("nothing to expand is not a failure");
    assert_eq!(config.grpc_grants("app").expect("grants").names(), ["a.B"]);
}

/// A backend that names `grpc_proto` but has not been expanded yet grants nothing.
///
/// The safe direction, and the one a test or a hand-built `Config` sees: `Dispatch` and `check` read
/// `grpc_grants`, so an entry that was never read must not be interpreted as "any" — nor as the
/// list that is not there. It is the same answer an absent `grpc_services` gives.
#[test]
fn an_unexpanded_proto_entry_grants_nothing() {
    let config = worker_config(r#""greeter.proto""#);
    config.validate().expect("a path alone validates");

    assert_eq!(
        config.grpc_grants("app").expect("a worker has grants"),
        GrpcGrants::none(),
        "the file has not been read, so there is nothing to grant"
    );
    assert!(!config.serves_grpc("app", "/greeter.Greeter/SayHello"));
}

/// The entries are what `check` prints, and they survive a change to the list.
///
/// Kept separate from `grpc_services` rather than cleared after expansion: a directory that pulls in
/// a vendored SDK's `.proto` files is the over-grant this key makes possible, and an operator can
/// only see it if `check` can still name the source beside the resolved count.
#[test]
fn the_entries_are_reported_separately_from_the_names_they_produce() {
    let fixture = Fixture::new("report");
    fixture.write("api/greeter.proto", "package greeter;\nservice Greeter {}");

    let mut config = worker_config(r#""api", "api/greeter.proto""#);
    config.validate().expect("the paths alone validate");
    config.expand_grpc_proto(&fixture.0).expect("expands");

    let reported = config.grpc_proto_all();
    assert_eq!(
        reported.keys().collect::<Vec<_>>(),
        ["app"],
        "only the backend with a source is reported"
    );
    assert_eq!(
        reported["app"],
        [PathBuf::from("api"), PathBuf::from("api/greeter.proto")],
        "the entries are reported as written, so `check` reads like the file"
    );
    assert_eq!(
        config.grpc_grants("app").expect("grants").names(),
        ["greeter.Greeter"],
        "and the *same* file named twice is still one grant"
    );
}
