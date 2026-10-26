//! `[server].trace_context`: the key that decides what a backend receives.

use super::*;

#[test]
fn the_default_is_extract_which_is_what_the_server_always_did() {
    // The upgrade promise: a deployment that says nothing keeps relaying the client's header
    // untouched. Asserted rather than assumed, because the whole point of the key is that one of
    // its four values *changes* what a backend sees.
    let text = "[server]\nhttp = \"127.0.0.1:8080\"\n";
    let config = Config::from_toml(text).expect("parses");
    assert_eq!(config.server.trace_context, PropagationMode::Extract);
    assert_eq!(PropagationMode::default().as_str(), "extract");
}

#[test]
fn all_four_values_parse_and_are_told_apart() {
    // The four are two independent decisions, so each must be reachable and each must report the
    // two answers that make it distinct. A table rather than four tests, because the point is the
    // *combination*: `propagate` and `extract` differ only in the second column, and an
    // implementation that wired one value to the wrong pair would otherwise pass.
    for (value, extracts, injects) in [
        ("ignore", false, false),
        ("extract", true, false),
        ("inject", false, true),
        ("propagate", true, true),
    ] {
        let text = format!("[server]\nhttp = \"127.0.0.1:8080\"\ntrace_context = \"{value}\"\n");
        let config = Config::from_toml(&text).unwrap_or_else(|err| panic!("{value:?}: {err}"));
        assert_eq!(config.server.trace_context.as_str(), value);
        assert_eq!(config.server.trace_context.extracts(), extracts, "{value}");
        assert_eq!(config.server.trace_context.injects(), injects, "{value}");
    }
}

#[test]
fn a_misspelled_value_is_refused_rather_than_defaulted() {
    // The rule `[server.access_log].format` already follows, and for the same reason: a deployment
    // that asked for `propogate` and silently got `extract` has a waterfall that is quietly wrong,
    // which is worse than a startup failure naming the key.
    for bad in ["Propagate", "propagate ", "on", "true", ""] {
        let text = format!("[server]\nhttp = \"127.0.0.1:8080\"\ntrace_context = \"{bad}\"\n");
        assert!(
            Config::from_toml(&text).is_err(),
            "{bad:?} was accepted as a trace-context mode"
        );
    }
}

#[test]
fn the_mode_does_not_need_a_telemetry_table() {
    // **The shape that made this a `[server]` key rather than a `[telemetry]` one.** nginx in
    // front, a collector behind the application, and nothing exported from here: `propagate` still
    // has to be writable, because it is about what the backend receives and not about where spans
    // go.
    let text = "[server]\nhttp = \"127.0.0.1:8080\"\ntrace_context = \"propagate\"\n";
    let config = Config::from_toml(text).expect("parses");
    config
        .validate()
        .expect("valid without any telemetry table");
    assert!(config.telemetry.is_none());
    assert_eq!(config.server.trace_context, PropagationMode::Propagate);
}

#[test]
fn the_behaviour_enum_is_reachable_under_the_same_names() {
    // The mirror in `volucer-observe` exists because `volucer-core` is a leaf and cannot depend on
    // it. This is the check that the two cannot drift: every name here is a name there, and the two
    // questions have the same answers. The conversion itself lives in `volucer`'s `main`, where the
    // process builds its `Observability`; this is the half a unit test can see.
    for mode in [
        PropagationMode::Ignore,
        PropagationMode::Extract,
        PropagationMode::Inject,
        PropagationMode::Propagate,
    ] {
        assert!(
            ["ignore", "extract", "inject", "propagate"].contains(&mode.as_str()),
            "{mode:?} names itself {:?}, which is not one of the documented values",
            mode.as_str()
        );
    }
    assert_eq!(PropagationMode::Ignore.as_str(), "ignore");
    assert_eq!(PropagationMode::Extract.as_str(), "extract");
    assert_eq!(PropagationMode::Inject.as_str(), "inject");
    assert_eq!(PropagationMode::Propagate.as_str(), "propagate");
}
