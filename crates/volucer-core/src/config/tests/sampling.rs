//! `[telemetry].sample` and `[telemetry].sample_ratio`: which traces leave the process.
//!
//! Separate from `endpoints.rs`, which covers the table's *plumbing* — where to send, how much at a
//! time, what to call itself. These two keys are a policy about cost rather than a fact about the
//! connection, and the tests that matter are about the two being one statement: a mode that reads a
//! ratio, and a ratio that a mode reads.

use super::*;

/// The smallest table, so each test differs from the others only in what it is testing.
fn telemetry_with(extra: &str) -> String {
    format!(
        "[server]\nhttp = \"127.0.0.1:8080\"\n[telemetry]\nendpoint = \"http://127.0.0.1:4318\"\n\
         {extra}"
    )
}

#[test]
fn the_default_is_parent_based_with_no_ratio() {
    // The upgrade promise: a deployment that has a `[telemetry]` table and says nothing about
    // sampling keeps exporting. `parent_based` records a root, so the effect of the default on an
    // existing standalone deployment is *nothing* — which is what makes it safe to be the default.
    let config = Config::from_toml(&telemetry_with("")).expect("parses");
    config.validate().expect("valid");
    let telemetry = config.telemetry.expect("present");
    assert_eq!(telemetry.sample, SampleMode::ParentBased);
    assert_eq!(telemetry.sample, SampleMode::default());
    assert_eq!(telemetry.sample.as_str(), "parent_based");
    assert!(telemetry.sample_ratio.is_none());
}

#[test]
fn all_three_modes_parse_and_are_told_apart() {
    // A table rather than three tests, because the failure this catches is one mode being wired to
    // another's spelling — which a per-mode test would still pass if both were swapped together.
    for value in ["parent_based", "always"] {
        let config = Config::from_toml(&telemetry_with(&format!("sample = \"{value}\"\n")))
            .unwrap_or_else(|err| panic!("{value:?}: {err}"));
        config
            .validate()
            .unwrap_or_else(|err| panic!("{value:?}: {err}"));
        let telemetry = config.telemetry.expect("present");
        assert_eq!(telemetry.sample.as_str(), value);
        assert!(telemetry.sample_ratio.is_none(), "{value} took a ratio");
    }

    // `ratio` on its own is the mode-without-its-value case, refused below — so it is written with
    // the fraction here, which is the only legal spelling of it.
    let config = Config::from_toml(&telemetry_with("sample = \"ratio\"\nsample_ratio = 0.1\n"))
        .expect("parses");
    config.validate().expect("valid");
    let telemetry = config.telemetry.expect("present");
    assert_eq!(telemetry.sample, SampleMode::Ratio);
    assert_eq!(telemetry.sample.as_str(), "ratio");
    assert_eq!(telemetry.sample_ratio, Some(0.1));
}

#[test]
fn a_misspelled_mode_is_refused_rather_than_defaulted() {
    // The same rule `[server].trace_context` follows, and the reason is the same: a deployment that
    // wrote `parent-based` and silently got `parent_based` is harmless, but one that wrote
    // `always` and got `parent_based` exports a tenth of what it believes it exports, and nothing
    // tells it.
    for bad in [
        "ParentBased",
        "parent-based",
        "parentbased",
        "always ",
        "on",
        "true",
        "",
    ] {
        let text = telemetry_with(&format!("sample = \"{bad}\"\n"));
        assert!(
            Config::from_toml(&text).is_err(),
            "{bad:?} was accepted as a sampling mode"
        );
    }
}

#[test]
fn ratio_without_a_fraction_is_refused_and_says_what_to_add() {
    // The half of the pair that is easy to forget, and the failure is silent otherwise: `ratio`
    // with no value cannot mean anything, and defaulting it would be this server choosing a
    // sampling rate — which is the deployment's decision and nobody else's.
    let config = Config::from_toml(&telemetry_with("sample = \"ratio\"\n")).expect("parses");
    let err = config.validate().expect_err("a ratio needs a value");
    let message = err.to_string();
    assert!(message.contains("sample_ratio"), "{message}");
    assert!(
        message.contains("e.g. 0.1"),
        "the error must say what a legal value looks like: {message}"
    );
}

#[test]
fn a_fraction_on_a_mode_that_does_not_read_it_is_refused() {
    // **A setting read by nothing is worse than one that is absent**, because the operator who
    // wrote it believes it does something. This is the same rule `[metrics.collect].buckets`
    // follows for a counter, and the two halves of this test are the two halves of that belief:
    // beside `always` and beside the default.
    for mode in ["always", "parent_based"] {
        let text = telemetry_with(&format!("sample = \"{mode}\"\nsample_ratio = 0.5\n"));
        let config = Config::from_toml(&text).expect("parses");
        let err = config
            .validate()
            .expect_err(&format!("{mode} does not read a ratio"));
        let message = err.to_string();
        assert!(message.contains("sample_ratio"), "{message}");
        assert!(
            message.contains(mode),
            "the error must name the mode that ignores it: {message}"
        );
    }
}

#[test]
fn a_fraction_outside_zero_to_one_is_refused_rather_than_clamped() {
    // `1.5` is an arithmetic error. Reading it as "all traces" is how a deployment discovers its
    // tracing bill was decided by a typo, and reading a negative value as "none" hides the same
    // mistake in the other direction.
    for bad in ["1.5", "2", "-0.1", "-1", "nan", "inf", "-inf"] {
        let text = telemetry_with(&format!("sample = \"ratio\"\nsample_ratio = {bad}\n"));
        let config = Config::from_toml(&text).expect("parses");
        let err = config
            .validate()
            .expect_err(&format!("{bad} is not a proportion"));
        assert!(err.to_string().contains("sample_ratio"), "{bad:?}: {err}");
    }
}

#[test]
fn the_exact_ends_and_the_smallest_representable_fraction_are_accepted() {
    // `0.0` and `1.0` are what an operator writes to mean "none" and "all", so they must be legal
    // values rather than approximations. And `2^-56` is the floor because the sampling decision
    // reads 56 bits of the trace id: a smaller fraction is not a probability this can honour, and
    // accepting it would claim a precision that does not exist.
    for good in ["0", "0.0", "1", "1.0", "0.1", "0.000000000000000014"] {
        let text = telemetry_with(&format!("sample = \"ratio\"\nsample_ratio = {good}\n"));
        let config = Config::from_toml(&text).unwrap_or_else(|err| panic!("{good:?}: {err}"));
        config
            .validate()
            .unwrap_or_else(|err| panic!("{good:?} must be accepted: {err}"));
    }
}

#[test]
fn a_fraction_below_the_representable_floor_is_refused_by_name() {
    // Not a rounding preference: a trace id carries 56 bits of randomness, so one in 10^30 is a
    // request the scheme cannot express. `0.0` already says "none" exactly, and the error says so.
    let text = telemetry_with("sample = \"ratio\"\nsample_ratio = 0.00000000000000000001\n");
    let config = Config::from_toml(&text).expect("parses");
    let err = config.validate().expect_err("below the floor");
    let message = err.to_string();
    assert!(message.contains("2^-56"), "{message}");
    assert!(
        message.contains("0.0"),
        "the error must point at the value that does mean none: {message}"
    );
}

#[test]
fn the_configuration_floor_and_the_behaviour_floor_are_one_number() {
    // Two crates hold this constant — the configuration model validates against it, and the
    // sampler decides with it — and a drift between them would let a value pass validation and then
    // be silently refused by `Sampler::ratio`, which the conversion turns into `parent_based`. So
    // the assertion is on the arithmetic rather than on the spelling: `2^-56` is the reciprocal of
    // the number of values 56 bits can hold.
    let two_to_the_56: f64 = (1u64 << 56) as f64;
    assert_eq!(MIN_SAMPLE_RATIO * two_to_the_56, 1.0);

    // And it is a real boundary rather than a number nothing consults: at the floor validation
    // accepts, and just below it refuses. A constant that no code read would pass the line above.
    let at_floor = "sample = \"ratio\"\nsample_ratio = 0.0000000000000000139\n";
    Config::from_toml(&telemetry_with(at_floor))
        .expect("parses")
        .validate()
        .expect("the floor itself is representable");

    let below = "sample = \"ratio\"\nsample_ratio = 0.000000000000000001\n";
    assert!(
        Config::from_toml(&telemetry_with(below))
            .expect("parses")
            .validate()
            .is_err(),
        "a fraction below the floor must be refused, not rounded"
    );
}

#[test]
fn an_unknown_key_in_the_telemetry_table_is_still_refused() {
    // The sampling keys must not have opened the table: `deny_unknown_fields` is what makes a typo
    // a startup failure, and `sample_ratioo` beside a working `sample` is exactly the mistake it
    // exists to catch.
    let text = telemetry_with("sample_ratioo = 0.5\n");
    assert!(
        Config::from_toml(&text).is_err(),
        "an unknown key was accepted once the table grew"
    );
}
