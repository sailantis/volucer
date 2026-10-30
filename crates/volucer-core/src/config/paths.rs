//! Paths written in the file, resolved against the directory that holds it.

use super::*;

/// The access log's path, resolved against the directory holding the config file.
///
/// The same rule `[server.tls]`'s `certificate` follows, and stated once so `serve` and `check`
/// cannot print different paths for one deployment — `check` reports what this returns.
///
/// **`-` is returned unchanged, and it means the process's own stdout.** That is nginx's spelling and
/// what a container expects, and treating it as a relative path would create a file named `-` beside
/// the configuration while the container's log stayed empty — a failure that looks like logging being
/// off. Handled here rather than at each caller so `check`'s report says `- (stdout)` rather than a
/// resolved path that would never exist.
#[must_use]
pub fn resolve_access_log_path(path: &Path, config_dir: &Path) -> PathBuf {
    // The dash is not a relative name, so it must not join the directory; every other case is the
    // ordinary absolute-or-relative rule, which `is_absolute` already spells.
    if is_stdout_log(path) || path.is_absolute() {
        path.to_path_buf()
    } else {
        config_dir.join(path)
    }
}

/// Whether an access-log path means "the process's stdout" rather than a file.
///
/// One predicate, because two callers have to agree: the resolver above must not turn `-` into a
/// path, and `build_observability_with_exporter` must choose the stream sink for it. A second
/// spelling of the check is how the two would come to disagree about what `-` means.
#[must_use]
pub fn is_stdout_log(path: &Path) -> bool {
    path.as_os_str() == "-"
}

/// A `[kv.<name>]` file store's directory, resolved against the directory holding the config file.
///
/// The same rule [`resolve_access_log_path`] follows, and a separate function rather than a shared
/// one with a rename: the two settings are resolved at different times by different callers, and a
/// generic `resolve_path` would be a name that says nothing about which setting it is resolving.
#[must_use]
pub fn resolve_kv_path(path: &Path, config_dir: &Path) -> PathBuf {    if path.is_absolute() {
        path.to_path_buf()
    } else {
        config_dir.join(path)
    }
}
