//! Reading one configuration out of several files: `include`.
//!
//! # What this borrows from MariaDB, and where it deliberately differs
//!
//! MariaDB's option files have `!include` and `!includedir`, a directory's files are read in the
//! byte order of their names, and the last occurrence of an option wins. The first two are worth
//! copying exactly, because they are what makes `conf.d/50-server.cnf` mean the same thing in
//! every deployment that has ever read one.
//!
//! What is **not** copied is the split between the two directives. MariaDB separates them because
//! a line in an option file has nowhere to say "this one is a directory"; a list does, by what the
//! name resolves to, so here both are entries of one `include` list read in the order written.
//! `include_dir` is refused by name rather than ignored, so an operator arriving from `my.cnf` is
//! told where the key went.
//!
//! The third cannot be copied as written. A MariaDB directive splices *lines* into one stream,
//! where naming `[mysqld]` twice is legal; TOML forbids a second `[server]` header in one
//! document, so a text-level splice would turn the ordinary layout — a base file with `[server]`,
//! a drop-in adding `[server.tls]` — into a parse error as soon as two files name the same table.
//! The splice here is therefore of *parsed tables*: each file is read, expanded and parsed on its
//! own, and the tables are merged. `50-server.toml` still wins over `10-base.toml` for the reason
//! it does in MariaDB — it is read later.
//!
//! Two rules are ours, because TOML asks questions MariaDB's format never does:
//!
//! * **The including file wins over what it includes.** An include is a base to be amended, not
//!   an override laid on top: the file the operator opens is the file whose numbers they expect
//!   to be in force.
//! * **An array is replaced, never concatenated.** Two files that both list `[[server.routes]]`
//!   cannot be merged element-wise — there is no key to match them on — and concatenating would
//!   make the result depend on which file happened to be read first.
//!
//! # Paths
//!
//! An include is resolved against the directory of the file that names it, not the process's
//! working directory: the rule every other relative path in a configuration already follows.
//! Runtime paths *inside* an included file are not re-based — the merge produces one
//! configuration, and one configuration has one directory.

use std::path::{Path, PathBuf};

use super::*;
use crate::error::Error;

/// How long a chain may be, the same limit MySQL's option files enforce.
///
/// A limit rather than only cycle detection, because a chain that is not circular can still be
/// longer than anyone can review: ten files of inherited settings is already a configuration
/// whose effective values appear in no single file.
const MAX_DEPTH: usize = 10;

/// One configuration as read from disk.
#[derive(Debug)]
pub struct Loaded {
    /// The merged table: every included file, in read order, with the naming file's own keys on
    /// top. The directive is gone from it.
    pub table: toml::Table,

    /// Every *included* file, in the order it was read.
    ///
    /// The naming file is not in this list, because the caller already knows it. A file included
    /// more than once appears once: it is read once per inclusion — that is how precedence works
    /// — but a report that named it twice would suggest it was read twice for two reasons.
    pub included: Vec<PathBuf>,
}

/// Read `path` and everything it includes, merged into one table.
pub(super) fn load(path: &Path) -> Result<Loaded> {
    let mut chain = Vec::new();
    let mut included = Vec::new();
    let table = load_one(path, &mut chain, &mut included)?;

    let mut seen = std::collections::BTreeSet::new();
    included.retain(|file| seen.insert(file.clone()));

    Ok(Loaded { table, included })
}

/// [`load`] for one file of a chain: cycle detection, the depth limit, and the merge.
///
/// `chain` carries the files currently being read, outermost first, so that both refusals can name
/// the whole path that led to the problem rather than the one file that happens to be last.
fn load_one(
    path: &Path,
    chain: &mut Vec<(PathBuf, PathBuf)>,
    included: &mut Vec<PathBuf>,
) -> Result<toml::Table> {
    let identity = identity(path);
    if let Some(position) = chain.iter().position(|(seen, _)| *seen == identity) {
        let mut names: Vec<String> = chain[position..]
            .iter()
            .map(|(_, display)| display.display().to_string())
            .collect();
        names.push(path.display().to_string());
        return Err(Error::Config(format!(
            "{}: this include returns to a file the chain already read ({}); a file that includes \
             itself, directly or through another, has no first value to apply",
            path.display(),
            names.join(" -> ")
        )));
    }
    if chain.len() >= MAX_DEPTH {
        let names: Vec<String> = chain
            .iter()
            .map(|(_, display)| display.display().to_string())
            .collect();
        return Err(Error::Config(format!(
            "{}: this include is {MAX_DEPTH} files deep ({}); the limit matches MySQL's option \
             files, because a configuration inherited through that many files has no value a \
             reader can find in one place",
            path.display(),
            names.join(" -> ")
        )));
    }

    chain.push((identity, path.to_path_buf()));
    let result = read_and_merge(path, chain, included);
    chain.pop();
    result
}

/// One file's own table, with everything it includes merged underneath it.
fn read_and_merge(
    path: &Path,
    chain: &mut Vec<(PathBuf, PathBuf)>,
    included: &mut Vec<PathBuf>,
) -> Result<toml::Table> {
    let includer = path.to_path_buf();
    let mut table = read(path)?;
    let entries = take_includes(&mut table, path)?;

    // The merge target: the includes in read order, then this file on top of them.
    let mut merged = toml::Table::new();
    let dependencies = expand_entries(&entries, path)?;

    for dependency in dependencies {
        included.push(dependency.clone());
        let nested = load_one(&dependency, chain, included).map_err(|error| match error {
            // A file that is simply not there is the one failure whose own message says nothing
            // about *why* it was expected: "failed to read /etc/volucer/conf.d/99.toml" leaves an
            // operator hunting through every file that might have named it. Every other refusal
            // already carries a path, so only this one is re-worded.
            Error::Read { path, source } => Error::Config(format!(
                "{} names {}, which cannot be read: {source}; a missing file is an error \
                 rather than a skip, so a typo cannot silently drop a whole drop-in",
                includer.display(),
                path.display(),
                source = source
            )),
            other => other,
        })?;
        merge(&mut merged, nested);
    }

    merge(&mut merged, table);
    Ok(merged)
}

/// A file's text, expanded and parsed.
///
/// The expansion is per file rather than on the merged result, so that a `${...}` reference that
/// is unset names the file and the line it was written on — and so that an include path itself
/// may come from the environment, which it could not if expansion happened last.
fn read(path: &Path) -> Result<toml::Table> {
    let text = std::fs::read_to_string(path).map_err(|source| Error::Read {
        path: path.to_path_buf(),
        source,
    })?;
    let text = crate::env::expand(&text)
        .map_err(|reason| Error::Config(format!("{}: {reason}", path.display())))?;
    text.parse::<toml::Table>().map_err(|source| Error::Parse {
        path: path.to_path_buf(),
        source: Box::new(source),
    })
}

/// The directive found in one file, wherever it was written.
///
/// **Position-independent by construction.** The key is removed from *every* table at *any* depth
/// before the file is merged, so `include` under `[server]`, after `[backends.app]`, or inside
/// `[server.tls]` all mean the same thing. That is not a relaxation of strictness — no table in the
/// model has an `include` field, so the name cannot collide with a real setting, and a key that was
/// previously an `unknown field` error is now the directive it plainly reads as.
///
/// It is also the one way to get MariaDB's behaviour in a scoped format. MariaDB's `!include` is a
/// *line*, spliced where it sits: their own documentation writes it inside `[mariadb]`, and a group
/// header on either side of it changes nothing. A `[table]` header in TOML *is* a scope, so the same
/// shape would silently become `server.include`. Extracting the key before the merge restores the
/// property the format otherwise cannot express: the directive says what to read, not where it is.
#[derive(Default)]
struct Directives {
    /// The paths as written, in order. Each is a file or a directory; which one is decided by
    /// looking at it, not by which key it was written under.
    entries: Vec<PathBuf>,
    from: Option<String>,
}

impl Directives {
    /// The name a position is reported by: the top level, or the table's dotted path.
    fn place(prefix: &str) -> String {
        if prefix.is_empty() {
            "the top level".to_string()
        } else {
            format!("[{prefix}]")
        }
    }

    fn claim(&mut self, entries: Vec<PathBuf>, prefix: &str, path: &Path) -> Result<()> {
        if let Some(first) = &self.from {
            return Err(two_places(first, &Self::place(prefix), path));
        }
        self.from = Some(Self::place(prefix));
        self.entries = entries;
        Ok(())
    }
}

/// One setting written twice, in two places that a merge would have to order.
///
/// Any order would be invisible in the file — "the deeper table wins" and "the earlier one wins"
/// are both defensible, which is the sign that neither should be invented.
fn two_places(first: &str, second: &str, path: &Path) -> Error {
    Error::Config(format!(
        "{}: `include` is written in two places ({first} and {second}). It is one setting, so keep \
         one: the includes of a file are read before the file itself, and there is no order within \
         one file for a second list to follow",
        path.display()
    ))
}
/// Remove the directive from a file's table, so nothing downstream can see it.
///
/// Removing rather than only reading is what keeps the model honest: `Config` has no field that a
/// leftover key could land in, and an include list that survived into a `Config` would be a list
/// that described files nothing read.
///
/// **One key, one list, one order.** An entry is a file or a directory, and the type is decided by
/// what is on disk rather than by which key it was written under: `include = ["base.toml", "conf.d"]`
/// reads `base.toml` then every `*.toml` of `conf.d`, in exactly the order shown. MariaDB needs two
/// directives because an option file is a stream of lines with no place to say "this one is a
/// directory"; a list has no such limit, and keeping two keys would invent a precedence rule
/// between them that no operator wrote — the same objection that makes two `include` lists a
/// refusal below.
fn take_includes(table: &mut toml::Table, path: &Path) -> Result<Vec<PathBuf>> {
    let mut found = Directives::default();
    collect(table, "", path, &mut found)?;
    Ok(found.entries)
}

/// Walk every table, at any depth, removing what it holds.
///
/// Depth-first over a `BTreeMap`, so the traversal order is alphabetical — which is exactly why two
/// directives in two places are refused instead of merged: the order a concatenation would follow is
/// the map's, and no operator wrote it.
fn collect(
    table: &mut toml::Table,
    prefix: &str,
    path: &Path,
    found: &mut Directives,
) -> Result<()> {
    if let Some(value) = table.remove("include") {
        match value {
            toml::Value::Array(items) => {
                found.claim(read_entry_list(items, path)?, prefix, path)?
            }
            toml::Value::String(_) => {
                return Err(Error::Config(format!(
                    "{}: `include` is a list, because it names every file and directory the \
                     configuration is assembled from; write include = [\"base.toml\"], or \
                     include = [\"conf.d\"] for a directory of drop-ins",
                    path.display()
                )))
            }
            // The table spelling existed only to reach the end of the file, and a bare key does
            // that now that the directive is read out of whatever table it lands in. Named anyway,
            // because it was documented and someone may have copied it.
            toml::Value::Table(_) => {
                return Err(Error::Config(format!(
                    "{}: `[include]` is a table header, and the directive is a list; write \
                     include = [\"base.toml\", \"conf.d\"] — files and directories are told apart \
                     by what is on disk, so there is one list rather than one per kind",
                    path.display()
                )))
            }
            other => {
                return Err(Error::Config(format!(
                    "{}: `include` is a {}; write a list, such as \
                     include = [\"base.toml\", \"conf.d\"]",
                    path.display(),
                    other.type_str()
                )))
            }
        }
    }

    // MariaDB's second directive, refused by name rather than left to `deny_unknown_fields`. An
    // operator arriving from `my.cnf` writes the key they know; "unknown field `include_dir`"
    // would be true and useless, since the whole point of the key is that it is spelled
    // differently here.
    if table.remove("include_dir").is_some() {
        return Err(Error::Config(format!(
            "{}: `include_dir` is one entry of `include`; write include = [\"conf.d\"], or name \
             several, include = [\"conf.d\", \"sites.d\"] — an entry is a file or a directory, and \
             the list is read in the order written",
            path.display()
        )));
    }

    // Then the subtables, collected first because a `BTreeMap` cannot be mutated while iterating.
    let names: Vec<String> = table
        .iter()
        .filter(|(_, value)| value.is_table())
        .map(|(name, _)| name.clone())
        .collect();
    for name in names {
        let Some(toml::Value::Table(nested)) = table.get_mut(&name) else {
            continue;
        };
        let dotted = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}.{name}")
        };
        collect(nested, &dotted, path, found)?;
    }
    Ok(())
}

/// A list of include entries, with each one checked as it is read.
fn read_entry_list(items: Vec<toml::Value>, path: &Path) -> Result<Vec<PathBuf>> {
    items
        .into_iter()
        .map(|item| match item {
            toml::Value::String(text) if !text.trim().is_empty() => Ok(PathBuf::from(text)),
            toml::Value::String(_) => Err(Error::Config(format!(
                "{}: `include` lists an empty path; remove it, because an entry naming nothing is \
                 a line that reads as an include and does nothing",
                path.display()
            ))),
            other => Err(Error::Config(format!(
                "{}: `include` holds a {}; every entry is a file or a directory, so write \
                 include = [\"base.toml\", \"conf.d\"]",
                path.display(),
                other.type_str()
            ))),
        })
        .collect()
}

/// Every entry, expanded in the order written.
///
/// An entry naming a directory becomes that directory's files, in place: `["a.toml", "conf.d",
/// "z.toml"]` reads `a.toml`, then `conf.d/*` in byte order, then `z.toml`. Expanding in place
/// rather than grouping files before directories is the whole point of one list — the author's
/// order **is** the order, with no second rule to learn or to get wrong.
///
/// An entry that is neither a file nor a directory is refused rather than skipped, for the reason a
/// missing file is: a typo and a drop-in that was never installed must not mean the same thing.
fn expand_entries(entries: &[PathBuf], from: &Path) -> Result<Vec<PathBuf>> {
    let mut expanded = Vec::new();
    for entry in entries {
        let resolved = resolve(from, entry);
        // `is_file` follows a symlink and is false for a directory, so the two tests are exclusive.
        if resolved.is_file() {
            expanded.push(resolved);
        } else if resolved.is_dir() {
            expanded.extend(directory_entries(&resolved, from)?);
        } else {
            return Err(Error::Config(format!(
                "{}: include names {}, which is neither a file nor a directory; a missing include \
                 is an error rather than a skip, so a typo cannot silently drop a whole drop-in",
                from.display(),
                resolved.display()
            )));
        }
    }
    Ok(expanded)
}

/// The `.toml` files of a directory, in the byte order of their names.
///
/// Byte order, not dictionary order, is MariaDB's rule and the reason packaged files carry numeric
/// prefixes: `10-base.toml` sorts before `50-server.toml`, and `Z.toml` before `a.toml`. Anything
/// that is not a `.toml` file is skipped, including subdirectories — a directory include is one
/// level deep in MariaDB too, and a recursive one would make the order depend on a tree.
fn directory_entries(directory: &Path, from: &Path) -> Result<Vec<PathBuf>> {
    let entries = std::fs::read_dir(directory).map_err(|source| {
        Error::Config(format!(
            "{}: include names the directory {}, which cannot be read ({source}); a missing \
             directory is refused rather than read as empty, because a typo and an empty directory \
             would otherwise mean the same configuration",
            from.display(),
            directory.display()
        ))
    })?;

    let mut files = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| {
            Error::Config(format!(
                "{}: include names the directory {}, which could not be listed ({source})",
                from.display(),
                directory.display()
            ))
        })?;
        let path = entry.path();
        // `is_file` follows a symlink, which is what a drop-in that links to a shared file needs.
        if is_toml(&path) && path.is_file() {
            files.push(path);
        }
    }

    files.sort_by(|left, right| {
        name(left)
            .as_encoded_bytes()
            .cmp(name(right).as_encoded_bytes())
    });
    Ok(files)
}

/// Merge `overlay` into `base`, with `overlay` winning.
///
/// Tables are merged key by key and everything else — a scalar, an array, a table over a scalar —
/// replaces what was there. Recursing into tables rather than replacing them is the whole point:
/// it is what lets `50-tls.toml` add `[server.tls]` without restating `[server] http`.
fn merge(base: &mut toml::Table, overlay: toml::Table) {
    for (key, value) in overlay {
        match (base.get_mut(&key), value) {
            (Some(toml::Value::Table(existing)), toml::Value::Table(incoming)) => {
                merge(existing, incoming);
            }
            (_, incoming) => {
                base.insert(key, incoming);
            }
        }
    }
}

/// A directive's path, resolved against the directory of the file that wrote it.
fn resolve(from: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        from.parent().unwrap_or_else(|| Path::new("")).join(path)
    }
}

/// What identifies a file for cycle detection.
///
/// Canonical where the filesystem allows it, so that `conf.d/../base.toml` and `base.toml` are one
/// file rather than a chain that never closes. A path that cannot be canonicalised is used as
/// written; it will then fail to read, with a message about that instead.
fn identity(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn is_toml(path: &Path) -> bool {
    path.extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("toml"))
}

fn name(path: &Path) -> &std::ffi::OsStr {
    path.file_name().unwrap_or(path.as_os_str())
}
