//! Checks that need the filesystem, not just the configuration file.
//!
//! Everything here is a *warning*, never an error. `volucer check` is a
//! diagnostic: a document root that does not exist yet is a normal thing to
//! check before a deploy creates it, and a front controller below the root is a
//! normal thing to serve from a directory whose contents are built later. What
//! these checks buy is that a mistake is reported once, here, instead of as a
//! puzzling 404 at the first request.

use std::path::{Path, PathBuf};

use crate::config::{BackendConfig, Config, OriginSpec, ServeOnly};
use crate::program::{resolve_program, split_command};

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

    /// A warning with nothing to act on â€” stated so a deployment can see what volucer is doing.
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
/// command line has had its say â€” so `check` can report the same decision `serve`
/// would make rather than the one the file happens to contain.
pub fn inspect(config: &Config, document_root: &Path) -> Vec<Warning> {
    inspect_with(config, document_root, None)
}

/// Whether anything under the document root could be served as a file.
///
/// Only the top level is read, and that is the deliberate part: the question is not
/// "does this application ship a file anywhere" â€” every application ships its own
/// `index.php`, which this layer refuses to serve â€” but "does it have an *assets*
/// story". An application that serves assets puts them in a directory beside the
/// front controller (`assets/`, `static/`, `css/`, `js/`, `images/`, `build/`,
/// `public/`); one that does not has a document root holding a controller and
/// nothing else. Reading one level deep is a `read_dir` at startup, not a walk.
///
/// It is deliberately generous. A directory is enough to suppress the warning even
/// if it is empty, because a deployment may create it at build time; what this
/// must never do is tell an application with an asset directory that it has none.
fn has_static_assets(document_root: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(document_root) else {
        // Unreadable is not "no assets". The caller already warns about a root it
        // cannot use, and guessing here would add a second, wronger complaint.
        return true;
    };
    entries.flatten().any(|entry| {
        entry.file_type().is_ok_and(|kind| kind.is_dir())
            || entry
                .path()
                .extension()
                .is_some_and(|ext| !is_script_extension(ext))
    })
}

/// Whether an extension names something a backend executes rather than a file.
///
/// The three PHP spellings are the ones this layer refuses to serve from disk, so a
/// document root holding only those is a document root with nothing to serve. The
/// list is the *default* script list rather than a site's configured one: this runs
/// before sites are resolved, and being wrong in the direction of "this is an asset"
/// only suppresses a warning.
fn is_script_extension(extension: &std::ffi::OsStr) -> bool {
    matches!(
        extension.to_str().map(str::to_ascii_lowercase).as_deref(),
        Some("php" | "php3" | "phtml")
    )
}

/// [`inspect`], resolving relative TLS paths against the config file's directory.
///
/// `config_path` is `None` when the caller has no file (a config built in a test), in which
/// case a relative TLS path is reported as written â€” the same answer `Path::parent` gives for
/// a bare filename, and better than inventing a directory.
pub fn inspect_with(
    config: &Config,
    document_root: &Path,
    config_path: Option<&Path>,
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
        if let Some(front) = config.server.resolve_front_controller(None) {
            let script = document_root.join(&front);
            if !script.is_file() {
                warnings.push(Warning::with_hint(
                    format!(
                        "front controller {front} was not found under {}",
                        document_root.display()
                    ),
                    // `front_controller = \"\"` cannot be written in the file â€”
                    // TOML cannot tell an empty string from an absent key, so the
                    // meaning is reserved for the flag. The hint has to say that,
                    // or it recommends something the validator rejects.
                    "check the path, or serve with `--front-controller ''` to mean \"only real \
                     .php files\" â€” every unmatched request will 404 meanwhile"
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
    // names no file â€” that is what a front controller *is* â€” so the layer answers
    // `NotAFile` on every one of them, after a `stat` and a hop to the blocking
    // pool.
    //
    // Measured on the competition host, same binary, `GET /items` at c=32: 2.05
    // `statx` per request with the layer on against 0.04 with it off, 33,043 rps
    // against 42,142. See hazard 85.
    //
    // A warning rather than an inferred default: the layer is legitimately useful
    // for an application that serves its own assets, and turning it off for one
    // that does would be a silent behaviour change on upgrade. What is not
    // defensible is leaving the operator to find out from a benchmark.
    if config.server.statics().enabled
        && config.server.serve_only == ServeOnly::All
        && !config.backends.is_empty()
        && config
            .server
            .resolve_front_controller(None)
            .is_some_and(|front| !front.is_empty())
        && document_root.is_dir()
        && !has_static_assets(document_root)
    {
        warnings.push(Warning::with_hint(
            "server.static_files is on and a front controller is configured, but the document \
             root holds no asset a file could be served from â€” every request stats for a file \
             that is never there",
            "write `serve_only = \"backend\"` to skip the static layer, which answers exactly \
             the same requests without the stat",
        ));
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
                "create it, or point this site's document_root somewhere else â€” every request \
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

    // A listener with no backends is a static-only deployment, which works â€” but
    // only for paths that name a file. A path that names nothing has nowhere to go
    // and answers 500, so it is worth saying once here rather than leaving it to be
    // discovered as an "internal error" at the first request.
    if config.backends.is_empty() {
        warnings.push(Warning::with_hint(
            "no backends are configured: only paths naming a real file can be answered",
            "with a front controller set, every other path is a 500 â€” either add a backend, or \
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
            "either add a `kind = \"worker\"` backend, or remove allow_worker_reset â€” a mutation \
             route with nothing behind it is an address an attacker can probe for free",
        ));
    }

    inspect_programs(config, &mut warnings);
    inspect_tls_with(config, config_path, &mut warnings);

    warnings
}

/// Report TLS settings whose files are missing or unusable.
///
/// The paths here are as written in the configuration, so this runs against whatever the
/// caller resolved â€” `check` passes the config-file-relative paths it printed, which is the
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
/// belongs to `serve`, which must do it anyway before it binds â€” and duplicating a real parser
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
                        "check the path, or the `command` of this backend â€” the worker is started \
                         with `command`, not with `binary`, so the two are independent settings",
                    ));
                }
            }
            BackendConfig::SpawnCgi(cgi) => {
                if !cgi.binary.is_file() {
                    warnings.push(Warning::with_hint(
                        format!("backends.{name}: {} does not exist", cgi.binary.display()),
                        "point `binary` at the php-cgi executable for this host â€” \
                         `volucer check` lists every PHP binary it found above",
                    ));
                }
            }
            BackendConfig::External(external) => {
                // Nothing to look for on disk: the socket belongs to another
                // process, and whether it is there yet is not a configuration
                // mistake. A `unix:` path can be checked cheaply, though, and a
                // missing one is the common cause of a first-request 502.
                if let Some(path) = external.socket.strip_prefix("unix:") {
                    if !Path::new(path).exists() {
                        warnings.push(Warning::with_hint(
                            format!(
                                "backends.{name}: nothing is listening on {} yet",
                                external.socket
                            ),
                            "start php-fpm before serving, or ignore this if it starts first",
                        ));
                    }
                }
            }
            BackendConfig::Upstream(upstream) => {
                // A unix socket is checked the same way the `external` backend's is, and for the
                // same reason: a missing one is the common cause of a first-request 502, and it is
                // cheap to notice. A malformed origin is *not* reported here â€” `Config::validate`
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
default_backend = "app"
[server]
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
    /// can never use, and a warning is the only honest way to say so.
    ///
    /// Both directions matter. A site whose document root holds only a front
    /// controller is warned about, because every request to it is a path that names
    /// no file; a site that ships an asset directory is not, because the layer is
    /// exactly what it wants. And a site that already said `serve_only = "backend"`
    /// is not warned about either â€” it has already made the decision the hint
    /// recommends.
    #[test]
    fn a_front_controller_with_no_assets_is_warned_about_the_static_layer() {
        let fixture = Fixture::bare("no-assets");
        fixture.write("index.php");

        let warnings = inspect(&config(), fixture.path());
        let found = warnings.iter().any(|warning| {
            warning
                .message
                .contains("no asset a file could be served from")
                && warning
                    .hint
                    .as_deref()
                    .is_some_and(|hint| hint.contains("serve_only"))
        });
        assert!(
            found,
            "expected the static-layer warning, got {warnings:#?}"
        );
        drop(fixture);
    }

    #[test]
    fn a_document_root_with_an_asset_directory_is_not_warned_about() {
        let fixture = Fixture::bare("with-assets");
        fixture.write("index.php");
        fixture.write("assets/app.css");

        let warnings = inspect(&config(), fixture.path());
        assert!(
            !warnings.iter().any(|warning| warning
                .message
                .contains("no asset a file could be served from")),
            "an application with assets wants the layer: {warnings:#?}"
        );
        drop(fixture);
    }

    #[test]
    fn opting_out_of_the_static_layer_is_not_warned_about() {
        let fixture = Fixture::bare("opted-out");
        fixture.write("index.php");

        let text = format!(
            r#"
default_backend = "app"
[server]
http = "127.0.0.1:8080"
serve_only = "backend"
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
            "the operator already asked for this: {warnings:#?}"
        );
        drop(fixture);
    }

    /// A configuration with one backend whose program cannot be started.
    fn config_with_worker_command(command: &str) -> Config {
        let text = format!(
            r#"
default_backend = "app"
[server]
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
    /// with a default â€” and it is the one `Config::validate` cannot see.
    #[test]
    fn a_php_cgi_binary_that_does_not_exist_is_reported() {
        let root = Fixture::new("bad-binary");
        root.write("index.php");
        let text = r#"
default_backend = "cgi"
[server]
http = "127.0.0.1:8080"
[backends.cgi]
kind = "spawn_cgi"
binary = "C:/definitely/not/php-cgi.exe"
"#;
        let config = Config::from_toml(text).expect("config parses");
        // Validation is about the shape of the file, so the missing file is not
        // its business â€” which is exactly why `inspect` has to catch it.
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
    /// would not be started â€” a key that looked supported and could not work. `deny_unknown_fields`
    /// on the backend table is what turns that into an error at the point the file is
    /// read, which is where a removed setting should be noticed.
    #[test]
    fn the_removed_spawn_fpm_backend_is_rejected_by_name() {
        let text = r#"
default_backend = "fpm"
[server]
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
        // a path naming no file would answer 500 â€” which is what the warning
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
        // Two: the root is gone, and the static layer â€” on by default â€” has nothing to
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

    /// A valid-looking PEM block. The check is deliberately shallow â€” it asks whether the
    /// file starts with a PEM header, not whether the certificate parses, because parsing
    /// belongs to `serve` and a second parser here would be a second answer.
    const PEM: &str = "-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----\n";

    /// Only the warnings this module's TLS checks produced.
    ///
    /// The fixture deliberately has no front controller and no backends, so `inspect_with`
    /// also reports those â€” they are real findings about the fixture, not about TLS, and
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

        // The server's pair is present, the site's is not â€” so exactly the site's two are
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
default_backend = "node"
[server]
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
default_backend = "node"
[server]
http = "127.0.0.1:8080"
[backends.node]
kind = "upstream"
origin = "http://127.0.0.1:3000"
"#;
        // `10.0.0.5` with TLS: across a network, but encrypted, so there is nothing to say.
        let tls = r#"
default_backend = "node"
[server]
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
default_backend = "node"
[server]
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
}
