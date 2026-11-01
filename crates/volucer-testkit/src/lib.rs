//! The skip gate and the environment probes, shared by every integration suite.
//!
//! # Why this is a crate and not a copied module
//!
//! Because the gate has already drifted once, in the way that matters. `VOLUCER_E2E=1` is what makes
//! a skip a failure, and it is the only thing that distinguishes "48 passed" from "48 tests that all
//! decided not to run" — libtest prints the same green line for both. Before this crate existed:
//!
//! - `crates/volucer/tests/serve.rs` and `crates/volucer-server/tests/worker_pool.rs` honoured it;
//! - `crates/volucer-fcgi/tests/php_cgi.rs` printed `skipping:` and never looked at it, so nine tests
//!   that drive a real `php-cgi` were silent under `VOLUCER_E2E=1`;
//! - `crates/volucer-server/tests/{pool,external,frontend}.rs` each had their own `eprintln!` and
//!   none of them was gated at all.
//!
//! A CI job that sets `VOLUCER_E2E=1` and a checkout with no `php` therefore reported success for a
//! workspace whose PHP-backed suites had not executed. That is not a documentation problem, and no
//! amount of prose in the README fixes it: the gate has to be one function that every suite calls.
//!
//! # What a gate is and is not for
//!
//! [`skip`] is for a **missing capability**: no `php`, no `php-cgi`, no SDK checkout, no Redis, no
//! broker. Those are properties of the machine, and a machine that is supposed to have them should
//! fail rather than pretend.
//!
//! It is **not** for transient contention. "No free port" is a race the suite lost that day, not a
//! capability the machine lacks — making it fatal would turn an occasional rerun into a red build
//! whose message names the wrong cause. [`skip_transient`] exists for that, and it says so at the
//! call site rather than leaving the reader to infer it.

use std::path::{Path, PathBuf};

/// Whether this machine is supposed to have the external prerequisites.
///
/// `VOLUCER_E2E=1` opts in; `0` and the empty string mean no, because an unset variable and one a
/// shell exported as empty are the same intent.
pub fn e2e_expected() -> bool {
    match std::env::var("VOLUCER_E2E") {
        Ok(value) => !value.is_empty() && value != "0",
        Err(_) => false,
    }
}

/// Leave a test unrun, unless this machine is supposed to be able to run it.
///
/// The reason is printed for an ordinary run and **panics** under `VOLUCER_E2E=1`. Both halves
/// matter: the print is the only way a developer learns why a suite was quiet, and the panic is the
/// only way CI learns that it was.
pub fn skip(reason: &str) {
    if e2e_expected() {
        panic!("VOLUCER_E2E is set, so a skipped test is a failure: {reason}");
    }
    eprintln!("skipping: {reason}");
}

/// Leave a test unrun because this run lost a race, not because the machine lacks anything.
///
/// Printed at the same level as [`skip`] and deliberately **never** fatal: a port this suite could
/// not take is contention between its own threads, and turning that into a red `CI` run would report
/// a missing capability that is not missing.
pub fn skip_transient(reason: &str) {
    eprintln!("skipping (transient): {reason}");
}

/// Leave a test unrun unless this machine is supposed to be able to run it.
///
/// The macro form exists because a skip has to `return` from the test body, and a function cannot do
/// that for its caller. Reach for [`skip`] when the skip is decided inside a helper that already
/// returns an `Option`.
///
/// ```ignore
/// volucer_testkit::skip_or_return!("no php on PATH");
/// ```
#[macro_export]
macro_rules! skip_or_return {
    ($reason:expr) => {{
        let reason = $reason;
        $crate::skip(&reason);
        return;
    }};
}

/// Bind `php` from `PATH`, or leave the test unrun.
#[macro_export]
macro_rules! require_php {
    ($name:ident) => {
        let Some($name) = $crate::find_php() else {
            $crate::skip_or_return!("no php on PATH");
        };
    };
}

/// Bind `php-cgi` from `PATH`, or leave the test unrun.
#[macro_export]
macro_rules! require_php_cgi {
    ($name:ident) => {
        let Some($name) = $crate::find_php_cgi() else {
            $crate::skip_or_return!("no php-cgi on PATH");
        };
    };
}

/// Find `php` on `PATH`.
pub fn find_php() -> Option<PathBuf> {
    #[cfg(windows)]
    let names: &[&str] = &["php.exe", "php"];
    #[cfg(not(windows))]
    let names: &[&str] = &["php"];

    find_on_path(names)
}

/// Find `php-cgi` on `PATH`.
///
/// A separate function from [`find_php`] rather than a parameter, because they are genuinely
/// separate packages on most systems: `brew install php` installs no `php-cgi`, and Debian ships it
/// as `php-cgi` beside `php-cli`. A suite that reports "no php" when `php-cgi` is what it wanted
/// sends the operator to install something they already have.
pub fn find_php_cgi() -> Option<PathBuf> {
    #[cfg(windows)]
    let names: &[&str] = &["php-cgi.exe", "php-cgi"];
    #[cfg(not(windows))]
    let names: &[&str] = &["php-cgi"];

    find_on_path(names)
}

fn find_on_path(names: &[&str]) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        for name in names {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// The workspace root, from this crate's own manifest directory.
fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
}

/// The sibling checkout the SDK is assumed to sit in when nothing names one.
fn sibling_sdk_dir() -> PathBuf {
    workspace_root()
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("volucer-php-sdk")
}

/// The PHP worker SDK checkout, which is a repository of its own.
///
/// `VOLUCER_SDK_DIR` overrides it, which is how a CI checkout at any path is pointed at it.
/// Otherwise the layout this project is developed in is assumed: `sailantis/volucer` and
/// `sailantis/volucer-php-sdk` beside each other. That assumption is a convenience for a developer
/// who has both, and it is **the usual reason a fresh clone reports every worker suite as skipped** —
/// so the reason [`php_worker_skip_reason`] returns names the path it looked at.
pub fn sdk_dir() -> PathBuf {
    match std::env::var_os("VOLUCER_SDK_DIR") {
        Some(dir) => PathBuf::from(dir),
        None => sibling_sdk_dir(),
    }
}

/// The SDK's example worker, which is what the worker suites are pointed at.
pub fn example_worker() -> PathBuf {
    sdk_dir().join("example").join("worker.php")
}

/// Why a suite that needs a PHP worker cannot run here, if it cannot.
pub fn php_worker_skip_reason() -> Option<String> {
    if find_php().is_none() && std::env::var_os("VOLUCER_SDK_DIR").is_none() {
        // Reported before the SDK check so the message names the first thing to install. A machine
        // with neither has one obvious step, not two.
        return Some("no php on PATH".to_string());
    }

    let script = example_worker();
    if !script.is_file() {
        return Some(if std::env::var_os("VOLUCER_SDK_DIR").is_some() {
            format!(
                "VOLUCER_SDK_DIR is set, but the SDK's example worker is not at {} — \
                 the variable must name the checkout root, which contains `example/worker.php`",
                script.display()
            )
        } else {
            format!(
                "the PHP worker SDK is not at {} — set VOLUCER_SDK_DIR to a volucer-php-sdk \
                 checkout, which the sibling layout this repository is developed in would \
                 have put there (the `php-cgi` suites do not need it)",
                script.display()
            )
        });
    }

    if find_php().is_none() {
        return Some(format!(
            "no php on PATH, and the SDK at {} needs one to run its example worker",
            sdk_dir().display()
        ));
    }

    None
}
