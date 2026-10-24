//! The checks a name or a table is held to.
//!
//! Each is called from more than one table — a route from `[server.routes]` and from a site's own, a
//! metric name from the table and from the prefix it is prepended to — which is why they are free
//! functions here rather than a method on one owner.

use super::*;

/// Whether `name` is a header name `hyper` will accept.
///
/// An RFC 9110 `token`: no spaces, no colons, no control characters, no non-ASCII. Checked here
/// because `hyper` accepts a bad name at construction of a `HeaderName` only via `from_bytes`, which
/// returns an error — and a caller that ignored it would silently never read or write the header.
pub(super) fn is_header_name(name: &str) -> bool {
    !name.is_empty()
        && name.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'!' | b'#'
                        | b'$'
                        | b'%'
                        | b'&'
                        | b'\''
                        | b'*'
                        | b'+'
                        | b'-'
                        | b'.'
                        | b'^'
                        | b'_'
                        | b'`'
                        | b'|'
                        | b'~'
                )
        })
}

/// Whether `prefix` may be prepended to the metrics this server provides.
///
/// A Prometheus metric name is `[a-zA-Z_:][a-zA-Z0-9_:]*`, so a *prefix* must be a (possibly empty)
/// run of those characters that is also a valid start — which means it may not begin with a digit.
/// Colons are deliberately not offered: they are reserved for recording rules, and a deployment
/// that used one would find its metrics rewritten out from under it by whatever rule happened to
/// match. An empty prefix is legal and means "no prefix", which is what an absent key already means
/// — so the two spellings agree rather than one being an error.
pub(super) fn is_metric_name_prefix(prefix: &str) -> bool {
    if prefix.is_empty() {
        return true;
    }
    if prefix.starts_with(|c: char| c.is_ascii_digit()) {
        return false;
    }
    prefix
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Whether `name` may be a metric name.
///
/// Prometheus's rule is `[a-zA-Z_:][a-zA-Z0-9_:]*`, but **colons are refused here** even though the
/// prefix rule above already refuses them, and the reason is different enough to say: a colon is
/// reserved for recording rules, and a metric an application declares is the last thing that should
/// be rewritten out from under it by whatever rule happened to match. The server's own names do not
/// use one either, so nothing in this process needs the character.
///
/// Distinct from [`is_metric_name_prefix`] rather than a reuse: a prefix may be empty, and a name may
/// not. Sharing one predicate would either accept an empty name or reject an absent prefix.
pub(super) fn is_metric_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(first) if first.is_ascii_alphabetic() || first == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Whether a byte may appear in an HTTP header name, per RFC 9110's `token`.
///
/// A local predicate rather than a dependency: `telemetry.headers` is validated before any header
/// type exists, because `volucer-core` is the leaf crate and cannot name `hyper`'s `HeaderName`. The
/// check is the `token` production — the characters an unquoted field name is made of — which is
/// exactly what would reject a key an operator copy-pasted from a vendor's documentation with a
/// space or a colon in it.
pub(super) const fn is_header_name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'!' | b'#'
                | b'$'
                | b'%'
                | b'&'
                | b'\''
                | b'*'
                | b'+'
                | b'-'
                | b'.'
                | b'^'
                | b'_'
                | b'`'
                | b'|'
                | b'~'
        )
}

/// Whether `name` is a shape a certificate can be verified against.
///
/// Deliberately stricter than DNS allows: no leading or trailing dot, no empty label, and no
/// characters outside letters, digits, `-` and `_`. A name that is *more* permissive than this
/// would be accepted here and then fail rustls' own `ServerName` parse at startup, which reports
/// the failure from a place that cannot say which key it came from.
pub(super) fn is_dns_name(name: &str) -> bool {
    // A single trailing dot is a valid absolute name in DNS but rustls' `ServerName` refuses it,
    // and a certificate's SAN never carries it either, so refusing it here says so once.
    if name.is_empty() || name.len() > 253 || name.ends_with('.') || name.starts_with('.') {
        return false;
    }
    name.split('.').all(|label| {
        if label.is_empty() || label.len() > 63 {
            return false;
        }
        // A label may not begin or end with a hyphen — DNS's own rule, and the reason a bare `-`
        // is not a host name. Enforced rather than left to `ServerName`, so the error names the
        // key it came from.
        let mut bytes = label.bytes();
        let first = bytes.next().expect("a non-empty label has a first byte");
        let last = label.as_bytes()[label.len() - 1];
        let edge = |b: u8| b.is_ascii_alphanumeric();
        if !edge(first) || !edge(last) {
            return false;
        }
        bytes.all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    })
}

/// Validate one route, whichever table it came from.
///
/// `where_` is the table's name as a reader would write it (`routes` or
/// `sites.<name>.routes`), so a message names the entry that owns the mistake
/// rather than an index into something the reader cannot see.
pub(super) fn validate_route(
    route: &RouteConfig,
    where_: &str,
    index: usize,
    backends: &BTreeMap<String, BackendConfig>,
) -> Result<()> {
    // A route that can reach a backend without naming one would have to fall back to the
    // site's `default_backend`, which is a different table's setting — so it is refused at the
    // route that omitted it rather than resolved somewhere the operator did not look. Only
    // `static` may leave it out, because it never dispatches.
    let backend = match (route.policy, &route.backend) {
        (RoutePolicy::Static, None) => None,
        (_, Some(name)) => Some(name),
        (policy, None) => {
            return Err(Error::Config(format!(
                "{where_}[{index}].backend is required for policy {:?}: a route that can reach \
                 a backend must name the one it reaches; only policy \"static\" may omit it",
                policy.as_str()
            )))
        }
    };

    if let Some(name) = backend {
        if !backends.contains_key(name) {
            return Err(Error::Config(format!(
                "{where_}[{index}].backend {name:?} does not match any configured backend"
            )));
        }
    }
    if route.path_prefix.is_none() && route.path_suffix.is_none() && route.host.is_none() {
        return Err(Error::Config(format!(
            "{where_}[{index}] has no path_prefix, path_suffix or host, so it would match every \
             request; set default_backend instead"
        )));
    }
    if let Some(prefix) = &route.path_prefix {
        if !prefix.starts_with('/') {
            return Err(Error::Config(format!(
                "{where_}[{index}].path_prefix {prefix:?} must start with '/'"
            )));
        }
    }
    if let Some(suffix) = &route.path_suffix {
        // A suffix that does not start with `.` or `/` matches far more than it
        // looks like it does — `"php"` matches `/notphp` — so it is refused rather
        // than left to be discovered as a routing surprise.
        if !(suffix.starts_with('.') || suffix.starts_with('/')) {
            return Err(Error::Config(format!(
                "{where_}[{index}].path_suffix {suffix:?} must start with '.' or '/', so that it \
                 cannot match an unrelated path by accident"
            )));
        }
    }
    if let Some(host) = &route.host {
        validate_host_pattern(host, &format!("{where_}[{index}].host"))?;
    }
    Ok(())
}

/// Validate a host pattern, as `*.example.com` or a bare name.
pub(super) fn validate_host_pattern(pattern: &str, where_: &str) -> Result<()> {
    if pattern.is_empty() {
        return Err(Error::Config(format!(
            "{where_} is empty; name a host, or remove the route"
        )));
    }
    // The port is not part of the pattern: a `Host` header carries one, but the
    // name a site or route claims does not, and accepting one would make
    // `a.test:8080` and `a.test` two different sites that look the same.
    let name = pattern.strip_prefix("*.").unwrap_or(pattern);
    if name.is_empty() || name.contains(':') || name.contains('/') {
        return Err(Error::Config(format!(
            "{where_} {pattern:?} must be a bare host name without a port, optionally with a \
             leading \"*.\""
        )));
    }
    // `*.` must leave at least one label, or the wildcard claims the whole of a
    // top-level domain.
    if pattern.starts_with("*.") && !name.contains('.') {
        return Err(Error::Config(format!(
            "{where_} {pattern:?} would match every host under {name:?}; a wildcard needs at \
             least two labels after it, e.g. \"*.example.com\""
        )));
    }
    Ok(())
}

/// Validate a front controller, as a relative path below the document root.
pub(super) fn validate_front_controller(controller: &FrontController, where_: &str) -> Result<()> {
    let front = match controller {
        // `false` means "only real files", which is what the key being absent
        // already means in a site table — so it is redundant but not wrong, and
        // saying it explicitly is how a reader checks the deployment's intent.
        FrontController::FilesOnly(false) => return Ok(()),
        // `true` is not a path and has no sensible reading: it would have to mean
        // some particular script, and guessing which one is exactly the ambiguity
        // the empty-string rule exists to avoid.
        FrontController::FilesOnly(true) => {
            return Err(Error::Config(format!(
                "{where_} = true is not a path; name the script, e.g. \"index.php\", or write \
                 `false` to serve only real files"
            )))
        }
        FrontController::Path(path) => path.as_str(),
    };
    if front.trim().is_empty() {
        return Err(Error::Config(format!(
            "{where_} is empty; write `false` to serve only real files, or name a script"
        )));
    }
    let front = front.trim_start();
    if front.starts_with('/')
        || front.contains('\\')
        || Path::new(front)
            .components()
            .any(|part| matches!(part, std::path::Component::ParentDir))
    {
        return Err(Error::Config(format!(
            "{where_} must be a relative path below the document root, e.g. \"index.php\" or \
             \"public/index.php\" — an absolute path, a `..` component or a backslash would \
             escape it, and `false` means \"only real files\""
        )));
    }
    Ok(())
}

/// Validate the resolvable parts of a static-file setting.
pub(super) fn validate_statics(statics: &StaticConfig, where_: &str) -> Result<()> {
    if statics.cache_control.trim().is_empty() {
        return Err(Error::Config(format!(
            "{where_}.cache_control is empty; set a value or remove the key"
        )));
    }
    if statics.max_multirange_bytes == 0 {
        return Err(Error::Config(format!(
            "{where_}.max_multirange_bytes is zero, which refuses every multi-range request; \
             set a value or remove the key"
        )));
    }
    Ok(())
}
