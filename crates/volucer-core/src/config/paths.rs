//! Paths written in the file, resolved against the directory that holds it.

use super::*;

/// The access log's path, resolved against the directory holding the config file.
///
/// The same rule `[server.tls]`'s `certificate` follows, and stated once so `serve` and `check`
/// cannot print different paths for one deployment — `check` reports what this returns.
#[must_use]
pub fn resolve_access_log_path(path: &Path, config_dir: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        config_dir.join(path)
    }
}

/// A `[kv.<name>]` file store's directory, resolved against the directory holding the config file.
///
/// The same rule [`resolve_access_log_path`] follows, and a separate function rather than a shared
/// one with a rename: the two settings are resolved at different times by different callers, and a
/// generic `resolve_path` would be a name that says nothing about which setting it is resolving.
#[must_use]
pub fn resolve_kv_path(path: &Path, config_dir: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        config_dir.join(path)
    }
}
