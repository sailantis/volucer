//! Facts about what this build is, for `volucer version` and for docs.
//!
//! Kept next to the code they describe rather than typed into the CLI, so a
//! build cannot claim a protocol version it does not speak or a PHP floor it
//! does not have. Both are printed by `volucer version`.

/// The worker protocol version this build speaks.
///
/// Re-exported from the codec, which is the definition: the number is on the
/// wire, and this name exists so the CLI does not have to depend on the worker
/// crate to report it.
pub use volucer_worker::PROTOCOL_VERSION as WORKER_PROTOCOL_VERSION;

/// The volucer version this build is, as the string every spawned process is told.
///
/// The same value `volucer version` prints, and it lives here for the same reason the two above do:
/// a build cannot report a version it does not have. It is handed to a process through
/// [`volucer_core::config::VERSION_ENV`], because a `php-cgi` per-request process and a supervised
/// origin have no channel to ask on.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The lowest PHP version the worker SDK supports.
///
/// 8.2, and the reason is a function rather than syntax: the SDK calls
/// `memory_reset_peak_usage()` so that each `END` frame reports *that request's*
/// peak RSS. Below 8.2 the function does not exist and the SDK falls back to a
/// monotonic high-water mark, which the pool would mistake for a worker that
/// grows without bound and recycle on every request.
///
/// It is stated as a string because that is how it is printed and how the SDK
/// declares it in `composer.json`; the alternative is a parser for a version
/// number nobody asked for.
pub const MINIMUM_PHP_VERSION: &str = "8.2";
