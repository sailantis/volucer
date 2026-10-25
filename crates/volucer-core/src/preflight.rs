//! Checks that need the filesystem, not just the configuration file.
//!
//! Everything here is a *warning*, never an error. `volucer check` is a
//! diagnostic: a document root that does not exist yet is a normal thing to
//! check before a deploy creates it, and a front controller below the root is a
//! normal thing to serve from a directory whose contents are built later. What
//! these checks buy is that a mistake is reported once, here, instead of as a
//! puzzling 404 at the first request.

use std::path::{Path, PathBuf};

use crate::config::{BackendConfig, Config, OriginSpec};
use crate::program::{resolve_program, split_command};
use crate::sites::Sites;

/// One thing that is probably not what the operator meant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Warning {
    /// What is wrong, in a sentence an operator can act on.
    pub message: String,

    /// How to fix it, when there is a specific answer.
    pub hint: Option<String>,
}

impl Warning {
    fn new(message: impl Into<String>) -> Self {
        Warning {
            message: message.into(),
            hint: None,
        }
    }

    /// A warning with nothing to act on — stated so a deployment can see what volucer is doing.
    ///
    /// The same constructor as [`Warning::new`], named for a reader rather than a compiler: the
    /// other warnings in this file all carry a hint because each one is something to *change*, and
    /// this one is something to *know*. Keeping the distinction in the call sites is what stops the
    /// next reader from adding a hint that does not exist.
    fn plain(message: impl Into<String>) -> Self {
        Warning::new(message)
    }

    fn with_hint(message: impl Into<String>, hint: impl Into<String>) -> Self {
        Warning {
            message: message.into(),
            hint: Some(hint.into()),
        }
    }
}

/// Inspect the host and report anything that will not work as written.
///
/// `document_root` is the document root that will actually be used, after the
/// command line has had its say — so `check` can report the same decision `serve`
/// would make rather than the one the file happens to contain.
pub fn inspect(config: &Config, document_root: &Path) -> Vec<Warning> {
    inspect_with(config, document_root, None)
}

/// [`inspect`], resolving relative TLS paths against the config file's directory.
///
/// `config_path` is `None` when the caller has no file (a config built in a test), in which
/// case a relative TLS path is reported as written — the same answer `Path::parent` gives for
/// a bare filename, and better than inventing a directory.
pub fn inspect_with(
    config: &Config,
    document_root: &Path,
    config_path: Option<&Path>,
) -> Vec<Warning> {
    inspect_with_front_controller(config, document_root, config_path, None)
}

/// [`inspect_with`], with the front controller the command line resolved.
///
/// **The flag has to be passed in rather than read from the config**, because `serve` and `check`
/// resolve `--front-controller` (and the "only real files" value `--fcgi` implies) on top of the
/// table — so a preflight that read `config.server` alone would warn about an `index.php` the
/// deployment is not using, which is exactly the shape of the contradiction this file exists to
/// avoid.
pub fn inspect_with_front_controller(
    config: &Config,
    document_root: &Path,
    config_path: Option<&Path>,
    front_controller_flag: Option<&str>,
) -> Vec<Warning> {
    let mut warnings = Vec::new();

    if !document_root.exists() {
        warnings.push(Warning::with_hint(
            format!("document root {} does not exist", document_root.display()),
            "create it, or point server.document_root somewhere else",
        ));
    } else if !document_root.is_dir() {
        warnings.push(Warning::with_hint(
            format!(
                "document root {} is not a directory",
                document_root.display()
            ),
            "point server.document_root at the directory that holds the application",
        ));
    }

    // A front controller that is not there is the classic silent failure: every
    // unmatched path resolves to a script PHP will refuse to execute, and the
    // client sees a 404 that says nothing about the cause.
    if document_root.is_dir() {
        if let Some(front) = config
            .server
            .resolve_front_controller(front_controller_flag)
        {
            let script = document_root.join(&front);
            if !script.is_file() {
                warnings.push(Warning::with_hint(
                    format!(
                        "front controller {front} was not found under {}",
                        document_root.display()
                    ),
                    // `front_controller = \"\"` cannot be written in the file —
                    // TOML cannot tell an empty string from an absent key, so the
                    // meaning is reserved for the flag. The hint has to say that,
                    // or it recommends something the validator rejects.
                    "check the path, or serve with `--front-controller ''` to mean \"only real \
                     .php files\" — every unmatched request will 404 meanwhile"
                        .to_string(),
                ));
            }
        }
    }

    if config.server.statics().enabled && !document_root.is_dir() {
        warnings.push(Warning::new(
            "server.static_files is enabled, but nothing can be served while the document root \
             is unusable",
        ));
    }

    // A front-controller application that also runs the static layer pays for a
    // decision it cannot use. Every request to such an application is a path that
    // names no file — that is what a front controller *is* — so the layer answers
    // `NotAFile` on every one of them, after a `stat` and a hop to the blocking
    // pool.
    //
    // Measured on the competition host, same binary, `GET /items` at c=32: 2.05
    // `statx` per request with the layer on against 0.04 with it off, 33,043 rps
    // against 42,142.
    //
    // The layer is **turned off** for those sites rather than merely reported, so this
    // warning states a decision instead of proposing one. `Sites::from_config_with` makes it,
    // because that is the table `serve` actually serves from — a warning that disagreed with
    // the request path would send the operator to check a setting that is not in force. What
    // it must never be is silent: a layer the file configured and the server is not running
    // has to be said out loud.
    let sites = Sites::from_config_with(config, document_root, front_controller_flag);
    let suppressed = sites.suppressed_static_layer();
    if !suppressed.is_empty() {
        let names: Vec<&str> = suppressed.iter().map(|site| site.name.as_str()).collect();
        let subject = if names == ["<default>"] {
            "the document root holds".to_string()
        } else {
            format!("the document roots of {} hold", names.join(", "))
        };
        // The **resolved** front controller, not the configured one: with
        // `--front-controller ''` (which `--fcgi` implies) there is none, and the reason the
        // layer is off is then simply that the root has no asset — the clause about a front
        // controller would name something the deployment does not have.
        let cause = if config
            .server
            .resolve_front_controller(front_controller_flag)
            .is_some()
        {
            "server.static_files is on and a front controller is configured"
        } else {
            "server.static_files is on"
        };
        warnings.push(Warning::plain(format!(
            "{cause}, but {subject} no asset a file could be served from, so the static layer is \
             off for that site — it would answer the same requests, without a stat"
        )));
    }

    // Each site's own root and front controller, because those are what a request to that
    // site actually uses. A site whose root is missing is not visible in the default site's
    // checks at all, and it answers 404 or 500 for every request it receives.
    for (name, site) in &config.sites {
        let root = &site.document_root;
        if !root.is_dir() {
            warnings.push(Warning::with_hint(
                format!(
                    "sites.{name}: document root {} does not exist",
                    root.display()
                ),
                "create it, or point this site's document_root somewhere else — every request \
                 to this site's hosts will fail meanwhile",
            ));
            continue;
        }
        // The site's own controller, or the one it inherits. A site that says
        // `front_controller = false` has none on purpose and is not warned about.
        if let Some(controller) = &site.front_controller {
            if let Some(front) = controller.as_path() {
                if !root.join(front).is_file() {
                    warnings.push(Warning::with_hint(
                        format!(
                            "sites.{name}: front controller {front} was not found under {}",
                            root.display()
                        ),
                        "check the path, or write `front_controller = false` to serve only real \
                         files",
                    ));
                }
            }
        }
    }

    // A listener with no backends is a static-only deployment, which works — but
    // only for paths that name a file. A path that names nothing has nowhere to go
    // and answers 500, so it is worth saying once here rather than leaving it to be
    // discovered as an "internal error" at the first request.
    if config.backends.is_empty() {
        warnings.push(Warning::with_hint(
            "no backends are configured: only paths naming a real file can be answered",
            "with a front controller set, every other path is a 500 — either add a backend, or \
             serve with `--front-controller ''` if this really is files only",
        ));
    }

    // A mutation endpoint with nothing to mutate is a setting that does nothing, and the honest
    // report is a warning rather than a refusal: the deployment may be upstream-only today and gain
    // a worker backend tomorrow, and `serve` already answers `404` for every worker index in that
    // state. What it must not do is look configured.
    if config
        .status
        .as_ref()
        .is_some_and(|status| status.allow_worker_reset)
        && !config
            .backends
            .values()
            .any(|backend| matches!(backend, BackendConfig::Worker(_)))
    {
        warnings.push(Warning::with_hint(
            "status.allow_worker_reset is set, but no backend is a worker pool: the routes exist \
             and every index is a 404",
            "either add a `kind = \"worker\"` backend, or remove allow_worker_reset — a mutation \
             route with nothing behind it is an address an attacker can probe for free",
        ));
    }

    inspect_programs(config, &mut warnings);
    inspect_tls_with(config, config_path, &mut warnings);
    inspect_hsts(config, &mut warnings);

    warnings
}

/// Report `Strict-Transport-Security` policies that reach further than the site they are written on.
///
/// The one HSTS mistake this file can see and the browser cannot: `includeSubDomains` binds names
/// the configuration never mentions, and if one of them is another site in this same deployment then
/// a browser that has visited the parent will refuse cleartext to it — including when that site was
/// deliberately cleartext, or reachable only on an internal address.
///
/// A warning rather than an error, because the policy may be exactly what the operator wants: the
/// subdomain may be TLS-only and public. What must not happen is that they did not notice.
fn inspect_hsts(config: &Config, warnings: &mut Vec<Warning>) {
    // Every site's resolved policy, since inheritance is what makes the parent's policy reach a
    // subdomain it does not own.
    let sites = Sites::from_config_with(config, Path::new("."), None);

    for (parent_name, parent) in &config.sites {
        // A site with no names has nothing for a browser to file a policy against, and validation
        // has already refused one that states its own.
        if parent.hosts.is_empty() {
            continue;
        }
        let Some(policy) = sites
            .named()
            .get(parent_name)
            .and_then(|site| site.hsts.as_ref())
        else {
            continue;
        };
        if !policy.include_subdomains {
            continue;
        }
        for host in &parent.hosts {
            // A wildcard names a pattern rather than a domain to be a superdomain *of*; skipping it
            // keeps this a report about concrete names, which is what a browser matches.
            if host.contains('*') {
                continue;
            }
            let suffix = format!(".{}", host.trim_end_matches('.').to_ascii_lowercase());
            // **Every** site, not only those with a policy of their own: the site that needs the
            // warning most is the one nobody wrote an HSTS table for, whose cleartext or internal
            // addresses the parent's policy now silently forecloses.
            for (child_name, child) in &config.sites {
                if child_name == parent_name {
                    continue;
                }
                for name in &child.hosts {
                    if name.contains('*') || !name.to_ascii_lowercase().ends_with(&suffix) {
                        continue;
                    }
                    warnings.push(Warning::with_hint(
                        format!(
                            "sites.{parent_name} sets includeSubDomains for {host}, which covers \
                             sites.{child_name} ({name}) too — a browser that has seen the \
                             policy will refuse cleartext to that name, including any internal \
                             address it resolves to"
                        ),
                        format!(
                            "make sure {name} serves TLS on every address it answers, or drop \
                             includeSubDomains from sites.{parent_name}"
                        ),
                    ));
                }
            }
        }
    }
}

/// Report TLS settings whose files are missing or unusable.
///
/// The paths here are as written in the configuration, so this runs against whatever the
/// caller resolved — `check` passes the config-file-relative paths it printed, which is the
/// point: the warning names the same file the operator sees.
///
/// Everything is a warning, like the rest of this module. A certificate that is not there yet
/// is a normal thing to check before a deploy copies it in, and the failure it would otherwise
/// cause is the worst kind: a listener that binds, reports nothing, and fails every handshake.
pub fn inspect_tls(config: &Config, warnings: &mut Vec<Warning>) {
    inspect_tls_with(config, None, warnings)
}

/// [`inspect_tls`], resolving relative paths against the config file's directory.
pub fn inspect_tls_with(config: &Config, config_path: Option<&Path>, warnings: &mut Vec<Warning>) {
    let Some(tls) = &config.server.tls else {
        return;
    };

    let base = config_path.and_then(Path::parent);
    let resolve = |path: &Path| -> PathBuf {
        match base {
            Some(dir) if !dir.as_os_str().is_empty() && path.is_relative() => dir.join(path),
            _ => path.to_path_buf(),
        }
    };

    // A pair that is left out is *generated*, so there is no file to check and reporting a missing
    // path would name one the operator never wrote. Only a stated path is inspected.
    if let (Some(certificate), Some(key)) = (&tls.certificate, &tls.key) {
        let certificate = resolve(certificate);
        let key = resolve(key);
        check_pem(
            &format!("server.tls.certificate {}", certificate.display()),
            &certificate,
            "certificate",
            warnings,
        );
        check_pem(
            &format!("server.tls.key {}", key.display()),
            &key,
            "private key",
            warnings,
        );
    }

    for (name, site) in &config.sites {
        let Some(site_tls) = &site.tls else { continue };
        let (Some(certificate), Some(key)) = (&site_tls.certificate, &site_tls.key) else {
            continue;
        };
        let certificate = resolve(certificate);
        let key = resolve(key);
        check_pem(
            &format!("sites.{name}.tls.certificate {}", certificate.display()),
            &certificate,
            "certificate",
            warnings,
        );
        check_pem(
            &format!("sites.{name}.tls.key {}", key.display()),
            &key,
            "private key",
            warnings,
        );
    }
}

/// Report one PEM file that is missing, unreadable, or not PEM.
///
/// "Starts with a PEM header" is deliberately all this checks. Parsing the certificate properly
/// belongs to `serve`, which must do it anyway before it binds — and duplicating a real parser
/// here would be a second answer to the same question, which is the drift this codebase avoids.
/// A file that begins with `-----BEGIN` and is still corrupt is caught at startup.
fn check_pem(label: &str, path: &Path, what: &str, warnings: &mut Vec<Warning>) {
    if path.as_os_str().is_empty() {
        // Empty is already an error in `validate`; reaching here means the config was not
        // validated first, so saying nothing is better than repeating a weaker message.
        return;
    }
    let Ok(contents) = std::fs::read(path) else {
        warnings.push(Warning::with_hint(
            format!("{label} cannot be read"),
            format!("the {what} is missing or unreadable; TLS will fail to start"),
        ));
        return;
    };
    if !contents.windows(11).any(|w| w == b"-----BEGIN ") {
        warnings.push(Warning::with_hint(
            format!("{label} does not look like PEM"),
            format!("a {what} must be a PEM file beginning with -----BEGIN"),
        ));
    }
}

/// Warn when a per-slot socket prefix names a directory that is not there.
///
/// The directory, not the finished path: the pool appends `.<slot>.sock`, so a missing
/// parent is what a bind fails on, and the ENOENT the operator would otherwise see names a
/// path they never typed. A prefix whose directory exists is not checked further — whether
/// this process may write there is the bind's question, and answering it here would mean
/// creating and removing a probe file on every `check`.
///
/// Shared by `spawn_cgi` and the worker transport because the suffix rule is shared, so the
/// two warnings cannot disagree about which directory matters.
fn warn_about_socket_prefix(warnings: &mut Vec<Warning>, where_: &str, prefix: &str) {
    #[cfg(windows)]
    if prefix.starts_with(r"\\.\pipe\") {
        // A pipe is a namespace, not a path, so there is no directory to be missing.
        return;
    }
    let Some(path) = prefix.strip_prefix("unix:") else {
        return;
    };
    let Some(dir) = Path::new(path).parent() else {
        return;
    };
    if dir.as_os_str().is_empty() || dir.is_dir() {
        return;
    }
    warnings.push(Warning::with_hint(
        format!(
            "{where_}: the directory {} for socket {prefix} does not exist",
            dir.display()
        ),
        "create it before serving, or point `socket` at a directory that exists — the pool \
         appends `.<slot>.sock` to the prefix, so the directory has to be writable by the \
         user running volucer",
    ));
}

/// Report every configured program that cannot be started.
///
/// This is the check `check` was missing: a mistyped `binary` is the mistake a
/// configuration file is *most* likely to make, and it is the one thing in the
/// file that is a filesystem path rather than a value with a default. Without
/// this, `volucer check` answered "configuration OK" for a configuration that
/// could not start a single backend, because `Config::validate` is about the
/// shape of the file and the pool's own existence check only ran inside `serve`.
///
/// A warning rather than an error, for the same reason the document-root checks
/// are: an image that installs PHP in a later layer is a normal thing to check,
/// and refusing to report anything else because one binary is not there yet
/// would be the wrong trade. The value is that the mistake is reported once,
/// here, instead of as a startup failure the operator has to go looking for.
fn inspect_programs(config: &Config, warnings: &mut Vec<Warning>) {
    for (name, backend) in &config.backends {
        match backend {
            BackendConfig::Worker(worker) => {
                let (program, _) = match split_command(&worker.command) {
                    Ok(parts) => parts,
                    // An empty or unbalanced command is a hard error in
                    // `Config::validate`, so this is unreachable in practice;
                    // skipping keeps the two checks from disagreeing about
                    // who reports what.
                    Err(_) => continue,
                };
                let binary = resolve_program(&program);
                if !binary.is_file() {
                    warnings.push(Warning::with_hint(
                        format!(
                            "backends.{name}: the worker program {} does not exist (from command \
                             {:?})",
                            binary.display(),
                            worker.command
                        ),
                        "check the path, or the `command` of this backend — the worker is started \
                         with `command`, not with `binary`, so the two are independent settings",
                    ));
                }
                // Where the pool will listen, when it is not a pipe. A directory that does
                // not exist is the one mistake that is both cheap to check and confusing to
                // diagnose later: the bind fails after the pool has already resolved its
                // program, and the message that reaches the operator is a bare ENOENT from a
                // path they did not type — the pool appends `.<slot>.sock`.
                if let (Some(socket), false) = (&worker.socket, worker.transport.is_pipe()) {
                    warn_about_socket_prefix(warnings, &format!("backends.{name}"), socket);
                }
            }
            BackendConfig::SpawnCgi(cgi) => {
                // One program per driver, checked by the same rule: a mistyped path is the
                // mistake a configuration file is most likely to make, and it is a warning here
                // rather than an error for the reason the document root is — an image that
                // installs the runtime in a later layer is a normal thing to check.
                match (&cgi.binary, &cgi.command) {
                    (Some(configured), _) => {
                        // Resolved the way the pool resolves it: a bare `php-cgi` is a lookup
                        // in the PHP search directories, not a path relative to the working
                        // directory. Checking the configured string instead was how `check`
                        // reported a binary it had just found and then warned it was missing.
                        let binary = crate::php::resolve_cgi_binary(configured);
                        if !binary.is_file() {
                            // The "from binary …" clause only when the name was resolved and
                            // failed: with an explicit path there is nothing to add, and quoting
                            // a `PathBuf` with `{:?}` doubled every backslash of a Windows path.
                            let detail = if binary == *configured {
                                String::new()
                            } else {
                                format!(" (from binary \"{}\")", configured.display())
                            };
                            let hint = if detail.is_empty() {
                                "point `binary` at the php-cgi executable for this host — \
                                 `volucer check` lists every PHP binary it found above"
                            } else {
                                "check the path, or put the directory holding php-cgi first on \
                                 `PATH` — `check` resolved the name and found nothing there"
                            };
                            warnings.push(Warning::with_hint(
                                format!(
                                    "backends.{name}: {} does not exist{detail}",
                                    binary.display()
                                ),
                                hint,
                            ));
                        }
                    }
                    (None, Some(command)) => {
                        // Resolved the way the pool resolves it, so what is checked is the file
                        // that will actually run: a bare `gunicorn` is a `PATH` lookup.
                        let program = crate::program::split_command(command)
                            .map(|(program, _)| program)
                            .unwrap_or_else(|_| command.clone());
                        let binary = crate::program::resolve_program(&program);
                        if !binary.is_file() {
                            warnings.push(Warning::with_hint(
                                format!(
                                    "backends.{name}: the slot program {} does not exist (from \
                                     command {command:?})",
                                    binary.display()
                                ),
                                "check the path or the `command` of this backend — each slot is \
                                 started with `command`, and `{address}` is replaced with the \
                                 address that slot must bind",
                            ));
                        }
                        if let Some(dir) = &cgi.working_dir {
                            if !dir.is_dir() {
                                warnings.push(Warning::with_hint(
                                    format!(
                                        "backends.{name}: working_dir {} is not a directory",
                                        dir.display()
                                    ),
                                    "a Python or Ruby application is started here, so a missing \
                                     directory is a slot that cannot import its own code",
                                ));
                            }
                        }
                        if !command.contains(crate::config::FCGI_ADDRESS_PLACEHOLDER) {
                            warnings.push(Warning::with_hint(
                                format!(
                                    "backends.{name}: the command {command:?} does not use \
                                     {{address}}, so this slot is never told the address it must \
                                     bind"
                                ),
                                "put {address} where the program expects its bind address, e.g. \
                                 \"gunicorn app.wsgi --bind {address}\"",
                            ));
                        }
                    }
                    _ => {}
                }
                if let Some(socket) = &cgi.socket {
                    warn_about_socket_prefix(warnings, &format!("backends.{name}"), socket);
                }
            }
            BackendConfig::External(external) => {
                // Nothing to look for on disk: the socket belongs to another
                // process, and whether it is there yet is not a configuration
                // mistake. A `unix:` path can be checked cheaply, though, and a
                // missing one is the common cause of a first-request 502.
                //
                // Not PHP-specific: `external` is a socket and a protocol, and the pool behind
                // it may be `php-fpm` or any other FastCGI server. The message says "FastCGI
                // server" for that reason — an operator reading it has to be able to match it
                // to what they are running.
                if let Some(path) = external.socket.strip_prefix("unix:") {
                    if !Path::new(path).exists() {
                        warnings.push(Warning::with_hint(
                            format!(
                                "backends.{name}: nothing is listening on {} yet",
                                external.socket
                            ),
                            "start the FastCGI server before serving, or ignore this if it starts first",
                        ));
                    }
                }
            }
            BackendConfig::Attach(attach) => {
                // The same cheap check as `external`'s socket, and it means more here: an
                // attached supervisor is a process volucer has never met, so a path that does
                // not exist is the most likely reason a deployment starts and then 502s. The
                // warning is not an error because the supervisor may legitimately be slower to
                // start, and because a relay in another container is reached over TCP where
                // nothing is checkable here.
                //
                // A relay that comes from `RELAY_SOCKET_ENV` is checked identically: which of the
                // two places the address was written in makes no difference to whether something
                // is listening on it.
                let relay = attach.relay().unwrap_or_default();
                if let Some(path) = relay.trim().strip_prefix("unix:") {
                    if !Path::new(path).exists() {
                        warnings.push(Warning::with_hint(
                            format!(
                                "backends.{name}: nothing is listening on {} yet, so every \
                                 connection to this pool will fail at startup",
                                relay.trim()
                            ),
                            "start the supervisor that forks this backend's workers — volucer \
                             does not start it, which is the whole point of `attach` — or ignore \
                             this if it starts first",
                        ));
                    }
                }
            }
            BackendConfig::Upstream(upstream) => {
                // A unix socket is checked the same way the `external` backend's is, and for the
                // same reason: a missing one is the common cause of a first-request 502, and it is
                // cheap to notice. A malformed origin is *not* reported here — `Config::validate`
                // refuses it outright, so repeating it as a warning would be a weaker second
                // answer to a question already answered.
                let Ok(origin) = OriginSpec::parse_with(
                    &upstream.origin,
                    upstream.tls.clone(),
                    upstream.protocol,
                ) else {
                    continue;
                };
                if let Some(path) = origin.socket_path() {
                    if !path.exists() {
                        warnings.push(Warning::with_hint(
                            format!(
                                "backends.{name}: nothing is listening on {} yet",
                                upstream.origin
                            ),
                            "start the application before serving, or ignore this if it starts \
                             first",
                        ));
                    }
                }
                // The one thing an origin cannot state about itself, and the reason this is a
                // warning rather than silence: an operator who wrote `http://` for a hop that
                // crosses a network has made a security decision, possibly without noticing it.
                // Loopback and a socket are excluded, because neither leaves the host.
                if !origin.is_tls() && !origin.is_local() {
                    warnings.push(Warning::with_hint(
                        format!(
                            "backends.{name}: {:?} sends the request in clear text to another host",
                            upstream.origin
                        ),
                        "use `https://` if this hop crosses a network you do not control; \
                         `http://` is fine for a loopback address or a unix socket",
                    ));
                }
                // A supervised origin, reported because it is the one backend whose process volucer
                // owns and the one thing this file can say that the configuration cannot: the
                // command was resolved and started, and what `restart` will do about it.
                if let Some(command) = &upstream.command {
                    warnings.push(Warning::plain(format!(
                        "backends.{name}: volucer starts {:?}, waits up to {} ms for it to answer, \
                         and restarts it {}",
                        command,
                        upstream.ready_timeout_ms,
                        upstream.restart_policy()
                    )));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        // The worker command names a program that certainly exists, so these
        // tests stay about the document root. `inspect` now also reports a
        // configured program that cannot be started, so a fixture naming `php`
        // would make every test in this module depend on whether PHP happens to
        // be installed on the host.
        let exe = std::env::current_exe().expect("the test binary path");
        let text = format!(
            r#"
[server]
default_backend = "app"
http = "127.0.0.1:8080"
[backends.app]
kind = "worker"
command = "{}"
"#,
            // Backslashes are escaped because this is a TOML basic string, which
            // is the same papercut a hand-written Windows path hits.
            exe.display().to_string().replace('\\', "\\\\")
        );
        Config::from_toml(&text).expect("config parses")
    }

    /// The static layer's default costs a front-controller application a `stat` it
    /// can never use, so the layer is turned off for it — and that is said out loud.
    ///
    /// Both directions matter. A site whose document root holds only a front controller
    /// loses the layer, because every request to it is a path that names no file; a site
    /// that ships an asset directory keeps it, because the layer is exactly what it wants.
    /// And a site that stated the setting itself keeps whatever it stated — the decision
    /// this makes is only for a layer nobody asked for.
    #[test]
    fn a_front_controller_with_no_assets_loses_the_static_layer() {
        let fixture = Fixture::bare("no-assets");
        fixture.write("index.php");
        let config = config();

        let warnings = inspect(&config, fixture.path());
        // No hint: there is nothing left to change. The message says what happened.
        let found = warnings.iter().any(|warning| {
            warning
                .message
                .contains("no asset a file could be served from")
        });
        assert!(
            found,
            "expected the static-layer warning, got {warnings:#?}"
        );

        let sites = Sites::from_config(&config, fixture.path());
        assert!(
            !sites.default_site().static_files.enabled,
            "the layer must actually be off, not merely reported"
        );
        assert!(sites.default_site().static_files_suppressed);
        drop(fixture);
    }

    #[test]
    fn a_document_root_with_an_asset_directory_keeps_the_static_layer() {
        let fixture = Fixture::bare("with-assets");
        fixture.write("index.php");
        fixture.write("assets/app.css");
        let config = config();

        let warnings = inspect(&config, fixture.path());
        assert!(
            !warnings.iter().any(|warning| warning
                .message
                .contains("no asset a file could be served from")),
            "an application with assets wants the layer: {warnings:#?}"
        );
        assert!(
            Sites::from_config(&config, fixture.path())
                .default_site()
                .static_files
                .enabled,
            "a site with assets must keep the layer"
        );
        drop(fixture);
    }

    #[test]
    fn a_layer_the_deployment_asked_for_is_never_turned_off() {
        // The decision is only ever made on a layer nobody asked for. `enabled = true` is a
        // stated preference, and honouring it is what keeps this from being a silent
        // behaviour change on upgrade.
        let fixture = Fixture::bare("asked-for");
        fixture.write("index.php");

        let text = format!(
            r#"
[server]
default_backend = "app"
http = "127.0.0.1:8080"
static_files = {{ enabled = true }}

[backends.app]
kind = "worker"
command = "{}"
"#,
            std::env::current_exe()
                .expect("the test binary path")
                .display()
                .to_string()
                .replace('\\', "\\\\")
        );
        let config = Config::from_toml(&text).expect("config parses");
        let warnings = inspect(&config, fixture.path());
        assert!(
            !warnings.iter().any(|warning| warning
                .message
                .contains("no asset a file could be served from")),
            "a layer the deployment asked for is kept, and not warned about: {warnings:#?}"
        );
        assert!(
            Sites::from_config(&config, fixture.path())
                .default_site()
                .static_files
                .enabled,
            "a stated `enabled = true` must survive"
        );
        drop(fixture);
    }

    /// A configuration with one backend whose program cannot be started.
    fn config_with_worker_command(command: &str) -> Config {
        let text = format!(
            r#"
[server]
default_backend = "app"
http = "127.0.0.1:8080"
[backends.app]
kind = "worker"
command = "{command}"
"#
        );
        Config::from_toml(&text).expect("config parses")
    }

    /// A directory that is removed when the test ends.
    struct Fixture(std::path::PathBuf);

    impl Fixture {
        /// An empty root, for the tests that are *about* the static layer warning.
        fn bare(name: &str) -> Self {
            let path = std::env::temp_dir()
                .join(format!("volucer-preflight-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("create the fixture");
            Fixture(path)
        }

        fn new(name: &str) -> Self {
            let path = std::env::temp_dir()
                .join(format!("volucer-preflight-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("create the fixture");
            // A directory, so these tests stay about the document root rather than
            // about the static layer. A root holding only a front controller is now
            // warned about (it pays a stat per request for nothing), and a fixture
            // that triggered that would make every test here assert two subjects at
            // once. The warning has its own tests, which build their own root.
            std::fs::create_dir_all(path.join("assets")).expect("create the asset directory");
            Fixture(path)
        }

        fn write(&self, relative: &str) {
            let path = self.0.join(relative);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).expect("create the parent");
            }
            std::fs::write(path, "<?php\n").expect("write the fixture file");
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_directory_with_its_front_controller_is_clean() {
        let root = Fixture::new("clean");
        root.write("index.php");
        assert_eq!(inspect(&config(), root.path()), Vec::new());
    }

    /// The check `check` was missing: a `binary` that is not there.
    ///
    /// This is the mistake a configuration file is most likely to make, because
    /// `binary` is the one setting that is a filesystem path rather than a value
    /// with a default — and it is the one `Config::validate` cannot see.
    #[test]
    fn a_php_cgi_binary_that_does_not_exist_is_reported() {
        let root = Fixture::new("bad-binary");
        root.write("index.php");
        let text = r#"
[server]
default_backend = "cgi"
http = "127.0.0.1:8080"
[backends.cgi]
kind = "spawn_cgi"
binary = "C:/definitely/not/php-cgi.exe"
"#;
        let config = Config::from_toml(text).expect("config parses");
        // Validation is about the shape of the file, so the missing file is not
        // its business — which is exactly why `inspect` has to catch it.
        config.validate().expect("the shape of the file is valid");

        let warnings = inspect(&config, root.path());
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(
            warnings[0].message.contains("does not exist"),
            "{warnings:?}"
        );
    }

    /// A worker's program comes from `command`, not from a `binary` key, so the
    /// check has to read the command rather than look for a field that is not
    /// there.
    #[test]
    fn a_worker_command_naming_a_missing_program_is_reported() {
        let root = Fixture::new("bad-command");
        root.write("index.php");
        let config = config_with_worker_command("definitely-not-php worker.php");
        let warnings = inspect(&config, root.path());
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(
            warnings[0].message.contains("does not exist"),
            "{warnings:?}"
        );
        assert!(
            warnings[0].message.contains("command"),
            "the message must name `command`, since that is the setting at fault: {warnings:?}"
        );
    }

    /// The `spawn_fpm` key is gone, and a file that still carries it must be rejected
    /// by name rather than parsed into something else.
    ///
    /// It used to be accepted and then refused at startup, with `check` warning that it
    /// would not be started — a key that looked supported and could not work. `deny_unknown_fields`
    /// on the backend table is what turns that into an error at the point the file is
    /// read, which is where a removed setting should be noticed.
    #[test]
    fn the_removed_spawn_fpm_backend_is_rejected_by_name() {
        let text = r#"
[server]
default_backend = "fpm"
http = "127.0.0.1:8080"
[backends.fpm]
kind = "spawn_fpm"
binary = "/usr/sbin/php-fpm"
"#;
        let err = Config::from_toml(text).expect_err("the key must be rejected");
        assert!(
            err.to_string().contains("spawn_fpm"),
            "the message must name the key that was removed: {err}"
        );
    }

    /// No backends is a static-only deployment: valid, warned about, and the
    /// warning has to say what breaks (a path that names no file).
    #[test]
    fn a_static_only_configuration_is_valid_and_warned_about() {
        let root = Fixture::new("static-only");
        // A front controller that *is* present, so the warning under test is the
        // only one: this fixture deliberately has nothing to dispatch to while
        // still naming a script, which is the shape that answers 500.
        root.write("index.php");
        // No front controller key, so the default (`index.php`) is in effect and
        // a path naming no file would answer 500 — which is what the warning
        // exists to say before the first request does.
        let config = Config::from_toml(
            r#"
[server]
http = "127.0.0.1:8080"
"#,
        )
        .expect("config parses");
        config.validate().expect("no backends is valid");

        let warnings = inspect(&config, root.path());
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].message.contains("no backends"), "{warnings:?}");
        assert!(
            warnings[0]
                .hint
                .as_deref()
                .unwrap_or_default()
                .contains("front-controller"),
            "the hint must offer the files-only escape: {warnings:?}"
        );
    }

    #[test]
    fn a_missing_document_root_is_reported() {
        let root = Fixture::new("missing");
        let gone = root.path().join("not-created-yet");
        let warnings = inspect(&config(), &gone);
        // Two: the root is gone, and the static layer — on by default — has nothing to
        // serve from. Both are true and both name the same cause, which is the point.
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(
            warnings[0].message.contains("does not exist"),
            "{warnings:?}"
        );
        assert!(warnings[0].hint.is_some());
        assert!(
            warnings[1].message.contains("static_files is enabled"),
            "the static layer's own consequence must be stated: {warnings:?}"
        );
    }

    #[test]
    fn a_missing_front_controller_is_reported_with_the_escape_hatch() {
        let root = Fixture::new("no-front");
        let warnings = inspect(&config(), root.path());
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(
            warnings[0].message.contains("front controller"),
            "{warnings:?}"
        );
        let hint = warnings[0].hint.as_deref().unwrap_or_default();
        assert!(hint.contains("--front-controller"), "{hint}");
    }

    /// A hint that recommends something the validator rejects is worse than no
    /// hint, so the two are checked against each other rather than by eye. This
    /// is not hypothetical: the first version of this hint said to write
    /// `server.front_controller = ""`, which `Config::validate` refuses.
    #[test]
    fn no_hint_tells_the_operator_to_write_something_validation_refuses() {
        let root = Fixture::new("hints");
        for path in [root.path().to_path_buf(), root.path().join("gone")] {
            for warning in inspect(&config(), &path) {
                let Some(hint) = &warning.hint else { continue };
                assert!(
                    !hint.contains("front_controller = \"\""),
                    "the hint recommends a value the validator rejects: {hint}"
                );
            }
        }
    }

    #[test]
    fn an_empty_front_controller_is_not_checked_for() {
        // "Only real .php files" has nothing to look for, so a directory holding
        // no index.php is perfectly correct.
        let root = Fixture::new("files-only");
        let mut config = config();
        config.server.front_controller = Some(String::new());
        assert_eq!(inspect(&config, root.path()), Vec::new());
    }

    #[test]
    fn a_document_root_that_is_a_file_is_reported() {
        let root = Fixture::new("is-a-file");
        root.write("index.php");
        let warnings = inspect(&config(), &root.path().join("index.php"));
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(
            warnings[0].message.contains("not a directory"),
            "{warnings:?}"
        );
        assert!(
            warnings[1].message.contains("static_files is enabled"),
            "{warnings:?}"
        );
    }

    // ---- TLS -------------------------------------------------------------------------

    /// A config with `[server.tls]`, so the PEM checks have something to look at.
    fn tls_config(certificate: &str, key: &str) -> Config {
        let text = format!(
            r#"
[server]
http = "127.0.0.1:443"

[server.tls]
certificate = "{certificate}"
key = "{key}"
"#
        );
        Config::from_toml(&text).expect("config parses")
    }

    /// A valid-looking PEM block. The check is deliberately shallow — it asks whether the
    /// file starts with a PEM header, not whether the certificate parses, because parsing
    /// belongs to `serve` and a second parser here would be a second answer.
    const PEM: &str = "-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----\n";

    /// Only the warnings this module's TLS checks produced.
    ///
    /// The fixture deliberately has no front controller and no backends, so `inspect_with`
    /// also reports those — they are real findings about the fixture, not about TLS, and
    /// counting them here would make these tests break whenever an unrelated check grows.
    fn tls_warnings(warnings: &[Warning]) -> Vec<&Warning> {
        warnings
            .iter()
            .filter(|w| w.message.contains("tls."))
            .collect()
    }

    #[test]
    fn a_readable_pem_pair_is_clean() {
        let root = Fixture::new("tls-clean");
        std::fs::write(root.path().join("c.crt"), PEM).expect("write cert");
        std::fs::write(root.path().join("c.key"), PEM).expect("write key");

        let config = tls_config("c.crt", "c.key");
        let warnings = inspect_with(
            &config,
            root.path(),
            Some(&root.path().join("volucer.toml")),
        );
        assert_eq!(tls_warnings(&warnings).len(), 0, "{warnings:?}");
    }

    #[test]
    fn a_missing_certificate_is_reported_by_the_resolved_path() {
        // The path in the message must be the resolved one, not the path as written: the
        // reader is looking at a file, and `certs/a.crt` names two different files depending
        // on the directory it is resolved against.
        let root = Fixture::new("tls-missing");
        let config = tls_config("certs/default.crt", "certs/default.key");
        let config_path = root.path().join("volucer.toml");

        let warnings = inspect_with(&config, root.path(), Some(&config_path));
        let tls = tls_warnings(&warnings);
        assert_eq!(tls.len(), 2, "{warnings:?}");
        assert!(tls[0].message.contains("cannot be read"), "{warnings:?}");
        assert!(
            tls[0].message.contains("server.tls.certificate"),
            "{warnings:?}"
        );
        // Resolved against the config file's directory, which is the whole point of the
        // separate rule.
        assert!(
            tls[0].message.contains("certs") && tls[0].message.contains("default.crt"),
            "{warnings:?}"
        );
        assert!(tls[1].message.contains("server.tls.key"), "{warnings:?}");
    }

    #[test]
    fn a_file_that_is_not_pem_is_reported() {
        let root = Fixture::new("tls-notpem");
        std::fs::write(root.path().join("c.crt"), "not a certificate\n").expect("write");
        std::fs::write(root.path().join("c.key"), PEM).expect("write key");

        let config = tls_config("c.crt", "c.key");
        let warnings = inspect_with(
            &config,
            root.path(),
            Some(&root.path().join("volucer.toml")),
        );
        let tls = tls_warnings(&warnings);
        assert_eq!(tls.len(), 1, "{warnings:?}");
        assert!(
            tls[0].message.contains("does not look like PEM"),
            "{warnings:?}"
        );
        assert!(
            tls[0]
                .hint
                .as_deref()
                .is_some_and(|h| h.contains("-----BEGIN")),
            "the hint should name what PEM means: {warnings:?}"
        );
    }

    #[test]
    fn a_site_certificate_is_checked_too() {
        let root = Fixture::new("tls-site");
        std::fs::write(root.path().join("default.crt"), PEM).expect("write");
        std::fs::write(root.path().join("default.key"), PEM).expect("write");

        let text = r#"
[server]
http = "127.0.0.1:443"

[server.tls]
certificate = "default.crt"
key = "default.key"

[sites.shop]
hosts = ["shop.test"]
document_root = "/srv/shop"

[sites.shop.tls]
certificate = "shop.crt"
key = "shop.key"
"#;
        let config = Config::from_toml(text).expect("parses");
        let warnings = inspect_with(
            &config,
            root.path(),
            Some(&root.path().join("volucer.toml")),
        );

        // The server's pair is present, the site's is not — so exactly the site's two are
        // reported, and the server's absence of a warning is what proves the check is
        // looking at the right pair rather than reporting everything.
        let tls = tls_warnings(&warnings);
        assert_eq!(tls.len(), 2, "{warnings:?}");
        assert!(
            tls[0].message.contains("sites.shop.tls.certificate"),
            "{warnings:?}"
        );
        assert!(
            tls[1].message.contains("sites.shop.tls.key"),
            "{warnings:?}"
        );
    }

    #[test]
    fn a_plaintext_config_reports_nothing_about_tls() {
        // The control: no `[server.tls]` means the TLS checks must be entirely absent, not
        // merely quiet.
        let root = Fixture::new("tls-none");
        root.write("index.php");
        let warnings = inspect(&config(), root.path());
        assert!(
            tls_warnings(&warnings).is_empty(),
            "a deployment with no TLS must not be warned about TLS: {warnings:?}"
        );
    }

    // -----------------------------------------------------------------------
    // The upstream backend's two filesystem-adjacent checks.
    // -----------------------------------------------------------------------

    /// A plaintext origin on another host is the one thing an operator may not have noticed.
    ///
    /// The counterpart to a missing socket: it is not a mistake `Config::validate` can see,
    /// because `http://10.0.0.5:3000` is a perfectly well-formed origin. What makes it worth
    /// saying is that a hop crossing a network in clear text is a *security decision*, and the
    /// configuration does not otherwise say so anywhere.
    #[test]
    fn a_plaintext_origin_on_another_host_is_reported() {
        let root = Fixture::new("upstream-plaintext");
        root.write("index.php");
        let text = r#"
[server]
default_backend = "node"
http = "127.0.0.1:8080"
[backends.node]
kind = "upstream"
origin = "http://10.0.0.5:3000"
"#;
        let config = Config::from_toml(text).expect("parses");
        let warnings = inspect(&config, root.path());
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].message.contains("clear text"), "{warnings:?}");
        assert!(
            warnings[0].message.contains("10.0.0.5"),
            "the message must name the origin at fault: {warnings:?}"
        );
        assert!(
            warnings[0]
                .hint
                .as_deref()
                .is_some_and(|hint| hint.contains("https://")),
            "the hint must name the fix: {warnings:?}"
        );
    }

    /// A loopback origin, a socket and a TLS origin are all silent.
    ///
    /// The three ways a hop can be fine, asserted together because the rule is one rule: it is
    /// *crossing a network* that is worth a word, and none of these does. A check that warned on
    /// all of them would be noise an operator learns to skip.
    #[test]
    fn a_local_or_tls_origin_is_not_warned_about() {
        let root = Fixture::new("upstream-local");
        root.write("index.php");

        let local = r#"
[server]
default_backend = "node"
http = "127.0.0.1:8080"
[backends.node]
kind = "upstream"
origin = "http://127.0.0.1:3000"
"#;
        // `10.0.0.5` with TLS: across a network, but encrypted, so there is nothing to say.
        let tls = r#"
[server]
default_backend = "node"
http = "127.0.0.1:8080"
[backends.node]
kind = "upstream"
origin = "https://10.0.0.5:8443"
"#;
        for (label, text) in [("loopback", local), ("tls", tls)] {
            let config = Config::from_toml(text).expect("parses");
            let warnings = inspect(&config, root.path());
            assert!(
                !warnings.iter().any(|w| w.message.contains("clear text")),
                "{label} must not be warned about: {warnings:?}"
            );
        }
    }

    /// A unix-socket origin whose socket is not there is reported, exactly as the
    /// `external` backend's is.
    ///
    /// The same courtesy for the same reason: a missing local socket is the common cause of a
    /// first-request 502, and it is cheap to notice before the listener binds.
    #[cfg(unix)]
    #[test]
    fn a_missing_upstream_socket_is_reported() {
        let root = Fixture::new("upstream-socket");
        root.write("index.php");
        let socket = root.path().join("nothing-here.sock");
        let text = format!(
            r#"
[server]
default_backend = "node"
http = "127.0.0.1:8080"
[backends.node]
kind = "upstream"
origin = "unix:{}"
"#,
            socket.display()
        );
        let config = Config::from_toml(&text).expect("parses");
        let warnings = inspect(&config, root.path());
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(
            warnings[0].message.contains("nothing is listening"),
            "{warnings:?}"
        );
    }

    /// A policy that reaches a sibling site is reported, because the file is the only place the
    /// overlap is visible: the browser applies `includeSubDomains` to names this deployment never
    /// wrote in one table.
    #[test]
    fn a_parent_policy_covering_a_sibling_site_is_reported() {
        let root = Fixture::new("hsts-subdomains");
        root.write("index.php");
        let text = r#"
[server]
http = "127.0.0.1:8080"
https = "127.0.0.1:8443"

[server.tls]
certificate = "c.crt"
key = "c.key"

[sites.shop]
hosts = ["example.com"]
document_root = "."

[sites.shop.hsts]
max_age_s = 600
include_subdomains = true

[sites.admin]
hosts = ["admin.example.com"]
document_root = "."
"#;
        let config = Config::from_toml(text).expect("parses");
        config.validate().expect("valid");
        let warnings = inspect(&config, root.path());
        let message = warnings
            .iter()
            .find(|w| w.message.contains("includeSubDomains"))
            .unwrap_or_else(|| panic!("the overlap must be reported: {warnings:?}"));
        assert!(message.message.contains("admin.example.com"), "{message:?}");
        assert!(
            message.hint.as_deref().is_some_and(|h| h.contains("TLS")),
            "the hint must name the fix: {message:?}"
        );

        // Without `includeSubDomains` there is no overlap to report — the policy stops at the
        // name it was written for.
        let text = text.replace("include_subdomains = true", "");
        let config = Config::from_toml(&text).expect("parses");
        config.validate().expect("valid");
        let warnings = inspect(&config, root.path());
        assert!(
            !warnings
                .iter()
                .any(|w| w.message.contains("includeSubDomains")),
            "{warnings:?}"
        );
    }

    /// A subdomain that only *inherits* from the parent is not a second site, so it is not an
    /// overlap — the warning would otherwise fire on every wildcard deployment.
    #[test]
    fn a_site_is_not_reported_for_covering_itself() {
        let root = Fixture::new("hsts-self");
        root.write("index.php");
        let text = r#"
[server]
http = "127.0.0.1:8080"
https = "127.0.0.1:8443"

[server.tls]
certificate = "c.crt"
key = "c.key"

[server.hsts]
max_age_s = 600
include_subdomains = true
"#;
        let config = Config::from_toml(text).expect("parses");
        config.validate().expect("valid");
        let warnings = inspect(&config, root.path());
        assert!(
            !warnings
                .iter()
                .any(|w| w.message.contains("includeSubDomains")),
            "{warnings:?}"
        );
    }
}
