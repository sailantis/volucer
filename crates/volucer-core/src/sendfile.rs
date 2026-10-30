//! Serving a file a backend named, instead of a body it produced.
//!
//! # Why this exists
//!
//! A backend that generates or holds a large file — a report, a media store, a backup — has no good
//! option today. It can read the file into the response body, which puts the whole thing in PHP
//! memory *and* in this server's memory (`from_cgi` copies it again), so a 1 GB download costs about
//! 2 GB of resident memory and a `memory_limit` failure somewhere in the middle. Or it can hand out a
//! signed URL and hope the client can reach the storage directly, which is a second origin to run.
//!
//! The answer every other server has is to let the application name a *path* and let the file-serving
//! layer do what it already does well: 64 KiB reads, ranges, `ETag`, `If-Range`, `Last-Modified`,
//! `304` and `HEAD`. `static_files` already does all of that, so this is a translation rather than a
//! second implementation.
//!
//! The header names are the ones already in the ecosystem: `X-Sendfile` (Apache's, and what
//! RoadRunner's plugin and nginx's `fastcgi_params` deployments use) and `X-Accel-Redirect` (nginx's).
//!
//! # The security boundary, which is the whole of the design
//!
//! **A header that names a filesystem path is a request from the application to read any file this
//! process can read.** `/etc/passwd`, another site's `.env`, a TLS private key. A backend is trusted
//! code, so this is not the same threat as a client-supplied path — but a backend is *often* the
//! thing that was exploited, and "the application can name any file and the server will stream it to
//! an unauthenticated client" turns one bug into a disclosure primitive.
//!
//! So a file may only be served from a **configured allow-list of roots**, and that list is separate
//! from `document_root` on purpose:
//!
//! - `document_root` is where *request paths* resolve, and it is a directory the application's
//!   sources live in. Serving from it means `X-Sendfile: /srv/app/.env` works, which is exactly the
//!   file a leaky debug endpoint would name.
//! - A `[server.sendfile] roots` entry is a directory an operator has decided is *publishable* —
//!   `storage/exports`, a media mount. Naming one is a statement that anything inside it may be sent
//!   to whoever the application decides.
//!
//! **Absent is the default and means the feature is off.** A key that existed and defaulted to
//! `document_root` would be off-by-one bug away from disclosing sources, and the failure would look
//! like the application's fault.
//!
//! # What a root check does and does not do
//!
//! The path is resolved (symlinks and all) and then compared against the resolved roots, so
//! `root/link-to-etc` is refused: a symlink inside an allowed root is the cheapest way to escape one,
//! and comparing unresolved strings would miss it. A path is inside a root when it is the root itself
//! or begins with the root followed by a separator — never on a bare string prefix, because
//! `/srv/exported` begins with `/srv/export` and is a different directory.
//!
//! Every failure is a `403` with the same text and a `warn` that names the real reason. A client is
//! not told whether a path exists, is a directory, is a symlink to elsewhere or is simply not
//! allowed: those are all the same answer to whoever asked, and the difference is only useful to
//! somebody probing.

use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The headers an application uses to name a file.
///
/// Two spellings for one idea, because both are already in use: `X-Sendfile` is what Apache defined
/// and what RoadRunner and a `fastcgi_params` deployment write, and `X-Accel-Redirect` is nginx's.
/// Accepting both means an application does not have to know which server it is running behind.
///
/// **Both are stripped from a backend's response whether or not this feature is on.** A header that
/// reached a client would be a filesystem path in the browser's address bar, and an application that
/// believed its `X-Accel-Redirect` was honoured by nginx in front would otherwise publish the path
/// nginx was meant to keep internal.
pub const SENDFILE_HEADERS: [&str; 2] = ["x-sendfile", "x-accel-redirect"];

/// The configured allow-list: which directories a backend may name a file in.
///
/// See the module documentation for why this is not `document_root`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SendfileConfig {
    /// The directories a backend may serve a file from. Required, and must not be empty.
    ///
    /// Each entry is absolutised against the configuration file's directory at startup, so
    /// `roots = ["storage/exports"]` means "beside the file that said so" rather than "wherever the
    /// process was started". A relative root would otherwise resolve differently under systemd and
    /// under a shell, which is the class of bug a path in a configuration file must not have.
    ///
    /// **A root that does not exist is refused at startup**, not skipped. A typo would otherwise be
    /// a feature that silently never works, and the 403 an operator saw would blame the application.
    pub roots: Vec<PathBuf>,
}

/// The resolved allow-list a front end consults per request.
///
/// Built once at startup and once per reload from a validated configuration; the roots are resolved
/// then, so the per-request work is one `canonicalize` and a prefix comparison.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SendfilePolicy {
    /// The roots, resolved, in the order they were written.
    ///
    /// Order is not a precedence rule here — a path inside two roots is inside both — but it is kept
    /// so `check` can print the list the operator wrote.
    roots: Vec<PathBuf>,
}

/// Why a backend's `X-Sendfile` was refused.
///
/// Every variant is answered to the client with the same `403` and the same text; the variants exist
/// so the **log** can say which one it was. An operator debugging a broken upload needs the reason,
/// and a client probing for files must not get it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// The feature is off, so the header was ignored.
    Disabled,
    /// The path is not inside any configured root.
    OutsideRoots {
        /// The path the backend named, resolved as far as it could be.
        path: PathBuf,
    },
    /// The path does not exist.
    Missing {
        /// The path the backend named.
        path: PathBuf,
    },
    /// The path is a directory.
    NotAFile {
        /// The path the backend named.
        path: PathBuf,
    },
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refusal::Disabled => f.write_str(
                "a backend asked to send a file, but server.sendfile is not configured, so the \
                 header was ignored",
            ),
            Refusal::OutsideRoots { path } => write!(
                f,
                "a backend named {} for X-Sendfile, which is not inside any server.sendfile root",
                path.display()
            ),
            Refusal::Missing { path } => write!(
                f,
                "a backend named {} for X-Sendfile, which does not exist",
                path.display()
            ),
            Refusal::NotAFile { path } => write!(
                f,
                "a backend named {} for X-Sendfile, which is a directory rather than a file",
                path.display()
            ),
        }
    }
}

impl SendfilePolicy {
    /// Build the policy from the configuration's roots, resolving each against `config_dir`.
    ///
    /// `Ok(None)` when there is no `[server.sendfile]` table, which is the default and means the
    /// feature is off. An error means a root is empty, is not a directory, or does not resolve —
    /// all three refused by name rather than skipped, because a skipped root is a feature that
    /// silently does not work and the 403 would blame the application.
    pub fn new(
        config: Option<&SendfileConfig>,
        config_dir: &Path,
    ) -> Result<Option<Self>, String> {
        let Some(config) = config else {
            return Ok(None);
        };
        if config.roots.is_empty() {
            return Err(
                "server.sendfile.roots is empty: list the directories a backend may serve files \
                 from, or remove the table to turn the feature off"
                    .to_string(),
            );
        }

        let mut roots = Vec::with_capacity(config.roots.len());
        for root in &config.roots {
            // Absolute as written wins; a relative one is the file's own directory, which is the
            // only meaning that does not depend on where the process was started.
            let path = if root.is_absolute() {
                root.clone()
            } else {
                config_dir.join(root)
            };
            // Resolved rather than lexical, so a root reached through a symlink is the directory it
            // really names and the comparison below is between two real paths.
            let resolved = path.canonicalize().map_err(|err| {
                format!(
                    "server.sendfile.roots names {}, which cannot be resolved: {err}",
                    path.display()
                )
            })?;
            if !resolved.is_dir() {
                return Err(format!(
                    "server.sendfile.roots names {}, which is not a directory",
                    path.display()
                ));
            }
            roots.push(resolved);
        }

        Ok(Some(SendfilePolicy { roots }))
    }

    /// The resolved roots, for `check`'s report.
    #[must_use]
    pub fn roots(&self) -> &[PathBuf] {
        &self.roots
    }

    /// Resolve a path a backend named, or say why it cannot be served.
    ///
    /// `Ok` carries the resolved path: the caller serves *that*, not the string it was handed, so the
    /// file that is opened is the file that was checked. Re-opening by the original path would be a
    /// time-of-check/time-of-use gap — a symlink swapped in between the check and the open — and it
    /// is free to avoid here because the resolved path is what the file layer wants anyway.
    pub fn resolve(&self, named: &str) -> Result<PathBuf, Refusal> {
        // A NUL cannot appear in a path, and it would truncate the path in a syscall rather than
        // fail. Refused here rather than reaching the filesystem.
        if named.contains('\0') {
            return Err(Refusal::Missing {
                path: PathBuf::from(named),
            });
        }

        let named_path = Path::new(named);
        // **Only an absolute path.** A relative one would resolve against this process's working
        // directory, which is not a thing an application can know — and `/srv/app/storage` written as
        // `storage` would silently be a different file under systemd.
        if !named_path.is_absolute() {
            return Err(Refusal::Missing {
                path: named_path.to_path_buf(),
            });
        }
        // A `..` component is refused before any filesystem access: `canonicalize` would resolve it,
        // and the result would be checked against the roots, so allowing it is not *unsafe* — but it
        // is always a bug in the caller, and refusing it turns a confused application into a clear
        // log line rather than a file that happens not to be in a root.
        if named_path
            .components()
            .any(|component| matches!(component, Component::ParentDir))
        {
            return Err(Refusal::Missing {
                path: named_path.to_path_buf(),
            });
        }

        // **Resolved, so a symlink is checked as well as followed.** `root/link-to-etc/passwd` must
        // be refused, and a comparison of unresolved strings would admit it.
        let resolved = named_path.canonicalize().map_err(|_| Refusal::Missing {
            path: named_path.to_path_buf(),
        })?;

        if !self.contains(&resolved) {
            return Err(Refusal::OutsideRoots { path: resolved });
        }

        // After the root check, so a directory outside the roots answers "not allowed" rather than
        // "is a directory" — the two must not be distinguishable to a caller probing for shape.
        match std::fs::metadata(&resolved) {
            Ok(meta) if meta.is_file() => Ok(resolved),
            Ok(_) => Err(Refusal::NotAFile { path: resolved }),
            Err(_) => Err(Refusal::Missing { path: resolved }),
        }
    }

    /// Whether a **resolved** path is inside one of the roots.
    ///
    /// A component-wise comparison rather than `starts_with` on the string: `/srv/exported` begins
    /// with `/srv/export` and is a different directory, and a string prefix check would admit it.
    #[must_use]
    pub fn contains(&self, resolved: &Path) -> bool {
        self.roots.iter().any(|root| resolved.starts_with(root))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "volucer-sendfile-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create the root");
        dir
    }

    fn policy(roots: &[PathBuf]) -> SendfilePolicy {
        SendfilePolicy {
            roots: roots
                .iter()
                .map(|root| root.canonicalize().expect("a real root"))
                .collect(),
        }
    }

    #[test]
    fn an_absent_table_means_the_feature_is_off() {
        // The default has to be off, and it has to be *absent* rather than a policy with no roots:
        // a policy that admitted nothing would still be a policy, and the request path would pay for
        // one on every response.
        let policy = SendfilePolicy::new(None, Path::new("/nowhere")).expect("absent is not an error");
        assert!(policy.is_none());
    }

    #[test]
    fn an_empty_root_list_is_refused_by_name() {
        let config = SendfileConfig { roots: Vec::new() };
        let err = SendfilePolicy::new(Some(&config), Path::new("/nowhere"))
            .expect_err("an empty list is a feature that cannot work");
        assert!(err.contains("server.sendfile.roots"), "{err}");
        assert!(err.contains("empty"), "{err}");
    }

    #[test]
    fn a_root_that_does_not_exist_is_refused_rather_than_skipped() {
        // A skipped root is a feature that silently never works, and the 403 an operator sees would
        // blame the application for a typo in the configuration.
        let config = SendfileConfig {
            roots: vec![PathBuf::from("/definitely/not/a/directory")],
        };
        let err = SendfilePolicy::new(Some(&config), Path::new("/nowhere"))
            .expect_err("a missing root is a typo");
        assert!(err.contains("/definitely/not/a/directory"), "{err}");
    }

    #[test]
    fn a_file_inside_a_root_resolves_and_a_sibling_outside_it_does_not() {
        // The prefix-versus-directory distinction: `/srv/exported` begins with `/srv/export` as
        // *text* and is a different directory. A string-prefix check would serve it.
        let base = temp_root("contains");
        let allowed = base.join("export");
        let sibling = base.join("exported");
        std::fs::create_dir_all(&allowed).expect("allowed");
        std::fs::create_dir_all(&sibling).expect("sibling");
        std::fs::write(allowed.join("report.csv"), b"a,b\n").expect("write");
        std::fs::write(sibling.join("secret.txt"), b"secret\n").expect("write");

        let policy = policy(std::slice::from_ref(&allowed));

        let inside = policy
            .resolve(allowed.join("report.csv").to_str().expect("utf-8"))
            .expect("a file inside the root is servable");
        assert!(inside.ends_with("report.csv"));

        let outside = policy
            .resolve(sibling.join("secret.txt").to_str().expect("utf-8"))
            .expect_err("a sibling directory is not inside the root");
        assert!(
            matches!(outside, Refusal::OutsideRoots { .. }),
            "{outside:?}"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_dotdot_is_refused_before_the_filesystem_is_consulted() {
        // `canonicalize` would resolve it and the root check would then refuse it, so this is not
        // about safety: it turns a confused application into a clear log line instead of a file that
        // merely happens not to be in a root.
        let base = temp_root("dotdot");
        std::fs::write(base.join("in.txt"), b"x").expect("write");
        let policy = policy(std::slice::from_ref(&base));

        let named = format!("{}/../in.txt", base.display());
        let err = policy.resolve(&named).expect_err("`..` is refused");
        assert!(matches!(err, Refusal::Missing { .. }), "{err:?}");

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_relative_path_is_refused() {
        // A relative path resolves against this process's working directory, which an application
        // cannot know — and under systemd it is a different file than under a shell.
        let base = temp_root("relative");
        let policy = policy(std::slice::from_ref(&base));
        let err = policy
            .resolve("report.csv")
            .expect_err("a relative path names nothing an application can know");
        assert!(matches!(err, Refusal::Missing { .. }), "{err:?}");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_out_of_a_root_is_refused() {
        // The cheapest way to escape an allow-list, and the reason the path is resolved rather than
        // compared as written.
        let base = temp_root("symlink");
        let allowed = base.join("export");
        let secret = base.join("secret.txt");
        std::fs::create_dir_all(&allowed).expect("allowed");
        std::fs::write(&secret, b"secret\n").expect("write");
        std::os::unix::fs::symlink(&secret, allowed.join("link.txt")).expect("symlink");

        let policy = policy(std::slice::from_ref(&allowed));
        let err = policy
            .resolve(allowed.join("link.txt").to_str().expect("utf-8"))
            .expect_err("a symlink out of a root is outside the root");
        assert!(matches!(err, Refusal::OutsideRoots { .. }), "{err:?}");

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_directory_inside_a_root_is_not_a_file() {
        let base = temp_root("directory");
        std::fs::create_dir_all(base.join("nested")).expect("nested");
        let policy = policy(std::slice::from_ref(&base));
        let err = policy
            .resolve(base.join("nested").to_str().expect("utf-8"))
            .expect_err("a directory is not servable");
        assert!(matches!(err, Refusal::NotAFile { .. }), "{err:?}");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_nul_byte_is_refused_before_it_can_truncate_a_path() {
        let base = temp_root("nul");
        std::fs::write(base.join("in.txt"), b"x").expect("write");
        let policy = policy(std::slice::from_ref(&base));
        let named = format!("{}/in.txt\0.png", base.display());
        let err = policy.resolve(&named).expect_err("a NUL truncates in a syscall");
        assert!(matches!(err, Refusal::Missing { .. }), "{err:?}");
        let _ = std::fs::remove_dir_all(&base);
    }
}

