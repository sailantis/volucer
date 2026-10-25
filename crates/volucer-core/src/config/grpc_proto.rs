//! Turning `grpc_proto` entries into the service names a backend is granted.
//!
//! The grant itself lives in [`GrpcGrants`](crate::config::GrpcGrants) and the reading of one file
//! in [`crate::proto`]. What is here is the part in between: which files a backend's entries name,
//! what to say when they name nothing service, and when the answer is installed.
//!
//! **This runs when the configuration is loaded, not when it is parsed.** [`Config::validate`] is a
//! pure function of the file — every one of its rules can be checked without touching a disk, which
//! is what keeps the model testable and what `check` relies on to report a file it could not start
//! from. Reading `.proto` files is I/O by definition, so it is a separate step with a directory
//! argument, and its result is written back into `grpc_services` where every other rule already
//! looks.
//!
//! That write-back is the whole design: **after expansion a backend has one statement of its grants
//! and no knowledge of where it came from**, so the request path, `volucer check` and a reload need
//! no branch each. The `grpc_proto` entries are deliberately left in place rather than cleared, so
//! `check` can name the files a grant came out of — a directory that pulls in a vendored SDK is the
//! over-grant this key makes possible, and it should be visible rather than inferred.

use super::*;
use crate::proto;

impl Config {
    /// The `.proto` files or directories a backend reads its services from, empty when it names them.
    pub fn grpc_proto(&self, backend: &str) -> &[PathBuf] {
        match self.backends.get(backend) {
            Some(BackendConfig::Worker(worker)) => &worker.grpc_proto,
            Some(BackendConfig::Attach(attach)) => &attach.grpc_proto,
            Some(BackendConfig::External(_))
            | Some(BackendConfig::SpawnCgi(_))
            | Some(BackendConfig::Upstream(_))
            | None => &[],
        }
    }

    /// Every backend's `.proto` entries, so the reporting sides do not repeat the match above.
    pub fn grpc_proto_all(&self) -> BTreeMap<String, Vec<PathBuf>> {
        self.backends
            .keys()
            .filter_map(|name| {
                let entries = self.grpc_proto(name);
                (!entries.is_empty()).then(|| (name.clone(), entries.to_vec()))
            })
            .collect()
    }

    /// Read every backend's `grpc_proto` and record the names it found as its grants.
    ///
    /// Idempotent in the sense that matters for a reload: it *replaces* the resolved list rather than
    /// adding to it, so a service removed from a `.proto` file and a `SIGHUP` later stops being
    /// granted — which is the direction an allow-list has to move.
    ///
    /// `config_dir` is the directory holding the configuration file, so a relative entry means the
    /// same thing here as `[server.tls] certificate` and `[kv.*] path` do. Called by `serve`, `check`
    /// and the reload through one place (`Source::load`), which is what keeps the three from reading
    /// the files at different moments.
    ///
    /// Three refusals, each naming the key: an entry that is not there, a file that cannot be read,
    /// and a set that resolves to **no** service. The last one is the important one. An empty grant
    /// answers `UNIMPLEMENTED` to every call with no line in the log saying why, and this key exists
    /// precisely to stop that happening by accident — so a mistyped directory is a startup error
    /// rather than a worker that silently serves nothing.
    pub fn expand_grpc_proto(&mut self, config_dir: &Path) -> Result<()> {
        for name in self.backends.keys().cloned().collect::<Vec<_>>() {
            let entries = self.grpc_proto(&name).to_vec();
            if entries.is_empty() {
                continue;
            }

            let mut files = Vec::new();
            for entry in &entries {
                collect_proto_files(&name, config_dir, entry, &mut files)?;
            }

            // A set rather than a vector: two files may legitimately declare the same package and
            // service name in a way `protoc` would refuse, and the grant is a membership test, so
            // the second occurrence would be a duplicate the router never notices. Sorted, because
            // `check` prints this list and a directory's `read_dir` order is not stable across
            // machines — an operator comparing two hosts' output should be comparing the services.
            let mut services = std::collections::BTreeSet::new();
            for file in &files {
                let text = std::fs::read_to_string(file).map_err(|source| Error::Read {
                    path: file.clone(),
                    source,
                })?;
                services.extend(proto::service_names(&text));
            }

            if services.is_empty() {
                return Err(Error::Config(format!(
                    "backends.{name}.grpc_proto resolved no gRPC service from {} file(s) under {}; \
                     a backend granted nothing would answer UNIMPLEMENTED to every call. Check that \
                     the entries name `.proto` files with a top-level `service` declaration, and \
                     that a directory search is reaching them",
                    files.len(),
                    entries
                        .iter()
                        .map(|entry| entry.display().to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                )));
            }

            if let Some(configured) = self.backends.get_mut(&name) {
                match configured {
                    BackendConfig::Worker(worker) => {
                        worker.grpc_proto_services = services.into_iter().collect()
                    }
                    BackendConfig::Attach(attach) => {
                        attach.grpc_proto_services = services.into_iter().collect()
                    }
                    BackendConfig::External(_)
                    | BackendConfig::SpawnCgi(_)
                    | BackendConfig::Upstream(_) => {}
                }
            }
        }
        Ok(())
    }
}

/// Append every `.proto` file `entry` names, resolving a relative entry against `config_dir`.
///
/// A directory is searched recursively, and **the visited set is what stops a symlink loop** from
/// hanging startup: `a/link -> a` is a legal directory layout and a depth limit would answer it
/// with "too deep" rather than with the files it does contain. Canonical paths are used for the set
/// only; the file paths that come out are the ones walked into, so a diagnostic shows what the
/// operator wrote.
fn collect_proto_files(
    backend: &str,
    config_dir: &Path,
    entry: &Path,
    out: &mut Vec<PathBuf>,
) -> Result<()> {
    let path = if entry.is_absolute() {
        entry.to_path_buf()
    } else {
        config_dir.join(entry)
    };

    let metadata = std::fs::metadata(&path).map_err(|source| {
        Error::Config(format!(
            "backends.{backend}.grpc_proto names {}, which cannot be read: {source}",
            path.display()
        ))
    })?;

    if metadata.is_file() {
        out.push(path);
        return Ok(());
    }
    if !metadata.is_dir() {
        return Err(Error::Config(format!(
            "backends.{backend}.grpc_proto names {}, which is neither a file nor a directory",
            path.display()
        )));
    }

    let mut visited = std::collections::BTreeSet::new();
    walk_proto_dir(backend, &path, &mut visited, out)
}

fn walk_proto_dir(
    backend: &str,
    dir: &Path,
    visited: &mut std::collections::BTreeSet<PathBuf>,
    out: &mut Vec<PathBuf>,
) -> Result<()> {
    let canonical = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    if !visited.insert(canonical) {
        return Ok(());
    }

    let mut children: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|source| {
            Error::Config(format!(
                "backends.{backend}.grpc_proto cannot search {}: {source}",
                dir.display()
            ))
        })?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .collect();
    // Sorted so the walk is the same on every filesystem: the services are sorted anyway, but the
    // *first* file to declare a name decides which duplicates are dropped, and a list that changed
    // with the directory order would be a reload nobody could explain.
    children.sort();

    for child in children {
        let Ok(metadata) = std::fs::metadata(&child) else {
            continue;
        };
        if metadata.is_dir() {
            walk_proto_dir(backend, &child, visited, out)?;
        } else if child.extension().is_some_and(|ext| ext == "proto") {
            out.push(child);
        }
    }
    Ok(())
}
