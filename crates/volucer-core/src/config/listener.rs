//! Listener endpoints, and the per-slot socket names a pool binds.
//!
//! Both are spelled `host:port` or `unix:/path`, and both are bounded by the same `sun_path` limit,
//! so the two live together.

use super::*;

/// A parsed listener endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Listener {
    /// `host:port`
    Tcp(SocketAddr),
    /// `unix:/path/to.sock`
    Unix(PathBuf),
}

/// The longest `sun_path` a unix socket may bind, in bytes.
///
/// `sockaddr_un` is 110 bytes on Linux with a 2-byte `sun_family`, so 108 usable
/// for the path including its NUL terminator; 107 is the portable maximum. The
/// kernel truncates anything longer rather than failing, which would bind a path
/// nobody asked for, so the check is explicit.
pub const UNIX_SOCKET_PATH_MAX: usize = 107;

/// Validate a `spawn_cgi` `socket` prefix.
///
/// The value is a prefix rather than a finished path because the pool appends
/// `.<slot>.sock` — see [`SpawnCgiConfig::socket`]. The per-slot suffix is
/// accounted for here so a prefix that only fits because of a low slot number is
/// rejected at startup rather than at the twelfth worker.
///
/// On Windows the same setting means a **named pipe** rather than a unix socket,
/// because that is what `php-cgi -b` binds there. A pipe name carries no path
/// length limit worth enforcing, so only the name itself is checked.
pub fn validate_cgi_socket(spec: &str) -> std::result::Result<(), String> {
    #[cfg(windows)]
    if let Some(name) = spec.strip_prefix(r"\\.\pipe\") {
        if name.is_empty() {
            return Err(format!("{spec:?} has an empty pipe name"));
        }
        if name.contains('\\') {
            return Err(format!(
                "pipe name {name:?} may not contain a backslash; a pipe's name is a single \
                 component, e.g. `\\\\\\\\.\\\\pipe\\\\volucer-cgi`"
            ));
        }
        if spec.contains('\0') {
            return Err("pipe name contains a NUL byte".to_string());
        }
        return Ok(());
    }

    let path = spec.strip_prefix("unix:").ok_or_else(|| {
        #[cfg(windows)]
        return format!(
            "{spec:?} must start with `unix:` or `\\\\.\\pipe\\`, e.g. \
             `unix:/run/volucer/cgi` or `\\\\.\\pipe\\volucer-cgi`"
        );
        #[cfg(not(windows))]
        return format!("{spec:?} must start with `unix:`, e.g. `unix:/run/volucer/cgi`");
    })?;
    if path.is_empty() {
        return Err("unix socket path is empty".to_string());
    }
    if !path.starts_with('/') {
        return Err(format!(
            "unix socket path {path:?} must be absolute; php-cgi's working directory is not ours"
        ));
    }
    if path.contains('\0') {
        return Err("unix socket path contains a NUL byte".to_string());
    }
    if cfg!(not(unix)) {
        return Err(format!(
            "unix sockets are not available on this platform, so {spec:?} cannot be used"
        ));
    }
    check_per_slot_length(path)
}

/// Validate a `worker` (or `[jobs.pool]`) `socket` prefix.
///
/// Shares [`check_per_slot_length`] with [`validate_cgi_socket`] rather than carrying
/// a second copy of the arithmetic: the two settings append the identical
/// `.<slot>.sock`, so two copies of the bound could disagree, and the one that is
/// wrong is the one that truncates a path in the kernel.
///
/// Three things are refused rather than warned about, and each is a configuration
/// that looks reasonable in a file:
///
/// - **A bare path.** A bare path is *relative*, so `socket = "unix:/run/x"` copied
///   from an `external` backend would bind a file named `unix:/run/x` in the working
///   directory. Requiring the prefix makes that a startup error.
/// - **A bare path beside `transport = "pipe"`.** The transport would win and the
///   key would be read by nothing — the `spawn_fpm` shape, which is why that backend
///   was removed rather than left parsing.
/// - **`host:port`.** The loopback transport picks its own ephemeral port, so an
///   explicit one is a request this setting cannot honour; naming the alternative is
///   more useful than binding something else.
///
/// Windows refuses `unix:` here, exactly as `spawn_cgi` does: there is no unix
/// socket to bind. A named pipe is not offered for the *worker* transport because the
/// protocol needs the connection in two halves and a pipe is not cloneable — see
/// `volucer_fcgi::transport::Stream::try_clone_stream`.
pub fn validate_worker_socket(
    spec: &str,
    transport: WorkerTransport,
) -> std::result::Result<(), String> {
    if transport.is_pipe() {
        return Err(format!(
            "{spec:?} is set beside `transport = \"pipe\"`, which reads stdio and never opens a \
             socket, so the address would be ignored; set `transport = \"socket\"` to use it, or \
             remove the key"
        ));
    }

    let path = spec.strip_prefix("unix:").ok_or_else(|| {
        if spec.contains(':') && !spec.starts_with('/') {
            return format!(
                "{spec:?} is an address, but this key takes a unix socket path prefix; the \
                 loopback transport picks its own ephemeral port, so remove the key to let it — \
                 or write `unix:/run/volucer/worker` to listen on a unix socket"
            );
        }
        format!(
            "{spec:?} must start with `unix:`, e.g. `unix:/run/volucer/worker`. A bare path is \
             relative, so it would bind a file in the working directory rather than the path it \
             looks like"
        )
    })?;

    if path.is_empty() {
        return Err("unix socket path is empty".to_string());
    }
    if !path.starts_with('/') {
        return Err(format!(
            "unix socket path {path:?} must be absolute; the worker's working directory is not ours"
        ));
    }
    if path.contains('\0') {
        return Err("unix socket path contains a NUL byte".to_string());
    }
    if cfg!(not(unix)) {
        return Err(format!(
            "unix sockets are not available on this platform, so {spec:?} cannot be used; use \
             `transport = \"socket\"` with no `socket` key for a loopback port"
        ));
    }
    check_per_slot_length(path)
}

/// The length bound shared by every per-slot socket prefix.
///
/// `.<slot>.sock`: a dot, up to 10 digits of `u32`, then `.sock`. Kept in one place
/// because both the CGI pool and the worker pool append it, and a second copy of this
/// arithmetic is a second chance to be wrong about the same kernel limit.
fn check_per_slot_length(path: &str) -> std::result::Result<(), String> {
    let worst_case = path.len() + 1 + 10 + 5;
    if worst_case > UNIX_SOCKET_PATH_MAX {
        return Err(format!(
            "unix socket path {path:?} is too long: with the per-slot suffix a worker would need \
             up to {worst_case} bytes and the platform allows {UNIX_SOCKET_PATH_MAX}"
        ));
    }
    Ok(())
}

/// The socket or pipe name slot `index` binds, given a prefix.
///
/// A unix socket gets `.<index>.sock` appended because a path names a file. A
/// Windows pipe name is a namespace rather than a path, and `php-cgi`'s own
/// instances are already unlimited (`PIPE_UNLIMITED_INSTANCES`), so the slot's
/// name is qualified with the index instead, keeping one pipe per slot.
pub fn cgi_socket_path(prefix: &str, index: u32) -> PathBuf {
    #[cfg(windows)]
    if let Some(name) = prefix.strip_prefix(r"\\.\pipe\") {
        return PathBuf::from(format!(r"\\.\pipe\{name}.{index}"));
    }
    let base = prefix.strip_prefix("unix:").unwrap_or(prefix);
    PathBuf::from(format!("{base}.{index}.sock"))
}

/// The unix socket slot `index` binds for the worker transport, given a prefix.
///
/// The same suffix rule as [`cgi_socket_path`] and deliberately not a second copy: a
/// worker slot and a `php-cgi` slot are the same problem — one address per process, so
/// that a process can be reached, recycled and reasoned about on its own.
///
/// No pipe arm. Unlike `php-cgi -b`, the worker transport cannot use a named pipe: the
/// protocol needs the connection as a reader and a writer at once, and
/// `Stream::try_clone_stream` cannot clone a pipe's `OVERLAPPED` state.
pub fn worker_socket_path(prefix: &str, index: u32) -> PathBuf {
    let base = prefix.strip_prefix("unix:").unwrap_or(prefix);
    PathBuf::from(format!("{base}.{index}.sock"))
}

/// Parse a listener endpoint from either `host:port` or `unix:/path`.
pub fn parse_listener(spec: &str) -> std::result::Result<Listener, String> {
    let spec = spec.trim();
    if let Some(path) = spec.strip_prefix("unix:") {
        if path.is_empty() {
            return Err("unix socket path is empty".to_string());
        }
        return Ok(Listener::Unix(PathBuf::from(path)));
    }
    if spec.is_empty() {
        return Err("listener is empty".to_string());
    }
    spec.parse::<SocketAddr>()
        .map(Listener::Tcp)
        .map_err(|e| format!("{spec:?} is neither host:port nor unix:/path ({e})"))
}
