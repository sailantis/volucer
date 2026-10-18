//! The file store: a directory of one file per key, so a cache survives a restart.
//!
//! # What a directory buys that Redis does not, and what it costs
//!
//! The argument for this driver is in `docs/plugins.md` §1.4 beside `boltdb`: a cache that survives a
//! restart **without a dependency**. A single node with a session store or a fragment cache does not
//! want a server, a port and a backup policy for it, and the memory driver loses everything on
//! deploy — which for a session store is every user logged out. So this is the driver for the
//! deployment that has a disk and no Redis.
//!
//! What it does not buy is durability in the sense a database means: there is no log, no fsync
//! policy, no crash consistency beyond "a file is either there or it is not". A power loss may lose
//! the most recent writes, and that is correct for a cache. The driver that needs more than this
//! needs a database, which is what the `redis` arm is for.
//!
//! # One file per key, named by the key's SHA-256
//!
//! Two shapes were possible: one file for everything, or one file per key. **One file per key**, and
//! the reason is that the alternative is a second database: a single file means a record format, a
//! free list, an on-disk index and a compaction pass, which is `boltdb` reimplemented badly. A
//! directory means the kernel's own directory index, and `get` is one `open`.
//!
//! The file name is the **SHA-256 of the key**, hex-encoded, because a key is arbitrary bytes
//! (`docs/plugins.md` §6.2: the wire carries opaque bytes) and a filesystem name is not. A key with a
//! `/` in it would otherwise be a path, and a key with a `..` would be a traversal — the one bug this
//! whole design exists to not have. Hashing also fixes the length problem: a maximum-length key is
//! 64 characters of file name whatever the key was.
//!
//! The collision argument is worth stating because it is the reason a hash is safe at all here: two
//! keys sharing a file is a *wrong answer*, not a leak, and 2^-128 is small enough that a cache may
//! ignore it — the same reasoning that lets a hash table use a hash. (The alternative, storing the
//! key in the file to detect a collision, would make `get` read the whole file to find out it is the
//! wrong one.)
//!
//! # The file's own format, and why it is text
//!
//! ```text
//! expiry: 1767225600
//!
//! <the value's bytes>
//! ```
//!
//! A five-word header line, then the raw value. `expiry:` is the absolute deadline as a Unix
//! timestamp in seconds, or `never`. It is **absolute and wall-clock**, not a remaining duration:
//! a duration read from disk after a restart has no way to know how long it has been lying there.
//!
//! Text rather than a binary header, because this file is something an operator will look at with
//! `cat` when a cache behaves oddly, and because a two-line format has no endianness. The one cost
//! is that a value beginning with the four bytes `expi` could be misread as a header — so the
//! *fixed* length of the header is not used, and instead:
//!
//! - the header is read as everything up to and including the **first** `\n\n`;
//! - a file with no `\n\n` is treated as corrupt and removed;
//! - a header line that is not exactly `expiry: <number>` or `expiry: never` is corrupt.
//!
//! A value that *contains* `\n\n` is fine: the split is at the first one, which is always in the
//! header because the header ends there.
//!
//! # Permissions are `0600`, and the check is on the file
//!
//! `docs/plugins.md` §6.2 records RoadRunner's `permissions: 0755` default on a boltdb file as a
//! hazard, and this is the driver that hazard is about. A cache holds session data and fragments of
//! pages behind a login, so every file this driver writes is readable only by its owner. The mode is
//! set **on each file**, not only on the directory, because a directory's mode does not follow a
//! file moved into it and `umask` decides what a `create` gets.
//!
//! # Bounded by bytes, and swept by an insert
//!
//! The same two mechanisms the memory driver uses, for the same reasons: a `max_bytes` accounting
//! and a sweep that runs **because an insert was about to fail**. The difference is that the
//! accounting starts from what is already on disk, so a restart begins at the size the last run left
//! — a driver that started at zero would let a restart double the disk it uses.
//!
//! The sweep here is `readdir` and `stat`, which is far more expensive than the memory driver's walk
//! over a map, so it is rate-limited by [`SWEEP_INTERVAL`] and only ever triggered by a refusal.
//!
//! # What it deliberately does not do
//!
//! - **No fsync.** A cache that fsyncs is a cache that is ten times slower for a guarantee nothing
//!   asked for. See the first section.
//! - **No compaction, no WAL, no index.** Those are a database.
//! - **No cross-process coordination.** Two volucer processes sharing one directory would both
//!   sweep and both count, and while nothing corrupts (each file is written whole, by rename), the
//!   bound would be advisory. A deployment that wants one cache from two processes wants Redis.
//! - **No batching.** `Store`'s default `get_many` is a loop, which is the honest implementation:
//!   there is no batch primitive in a directory, and pretending otherwise would be a loop with a
//!   more expensive name.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::{clamp_ttl, Error, Result, Store, Ttl};

/// The longest a sweep may be deferred between two refusals.
///
/// Ten seconds rather than the memory driver's one, because a sweep here is `readdir` plus a `stat`
/// per file: on a directory of a hundred thousand keys that is a tenth of a second of syscalls, and
/// doing it on every refused insert would turn a full cache into a busy disk.
const SWEEP_INTERVAL: Duration = Duration::from_secs(10);

/// The header line's prefix, and the value that means "no expiry".
const EXPIRY: &str = "expiry: ";
const NEVER: &str = "never";

/// The separator between the header and the value.
const SEPARATOR: &[u8] = b"\n\n";

/// How many bytes of a file are read to find its header.
///
/// `"expiry: "` plus a twenty-digit `u64` is 28 bytes, so 128 is four times what any header this
/// driver writes could need, and small enough that a sweep's cost per file is the syscall rather than
/// the buffer. A file whose separator is not within this many bytes is not one of ours.
const HEADER_PROBE: usize = 128;

/// The permission bits every file this driver writes gets.
///
/// Owner read and write, and nothing else. A cache holds session ids and fragments of authenticated
/// pages, so group and world read are not a default to be opted out of. See the module documentation
/// on why this is set per file rather than only on the directory.
#[cfg(unix)]
const FILE_MODE: u32 = 0o600;

/// A directory of files, one per key, bounded by bytes and with per-item expiry.
#[derive(Debug)]
pub struct FileStore {
    root: PathBuf,
    /// The most bytes of stored values this directory may hold.
    max_bytes: usize,
    /// What the directory holds now, as this process has accounted for it.
    ///
    /// Behind a mutex rather than an atomic because a sweep adjusts it by a *read* of the directory
    /// and an insert adjusts it by a *difference*: two adjustments that must not interleave, or a
    /// sweep that concluded "12 MiB is used" would overwrite an insert that added two between the
    /// read and the store. The lock is held for arithmetic, never across an I/O call — the same
    /// ordering the memory driver's bounds follow.
    used_bytes: Mutex<usize>,
    /// When the last sweep started, in milliseconds since the store was built.
    ///
    /// Monotonic rather than wall-clock: a sweep's *rate limit* has nothing to do with the time of
    /// day, and an `Instant` derived from `SystemTime` would jump when the clock is corrected.
    last_sweep_ms: AtomicU64,
    started: Instant,
}

impl FileStore {
    /// Open (or create) the directory, and account for what is already in it.
    ///
    /// **Fallible, and the callers expect it**: the directory is created with `0700` if it is not
    /// there, and a path that exists as a *file*, or that cannot be created, is a startup failure —
    /// the same rule the Redis driver follows for an unreachable server. A cache that silently does
    /// not work is worse than one that refuses to start, because the application sees every `get` as
    /// a miss and reports nothing.
    pub fn open(root: impl Into<PathBuf>, max_bytes: usize) -> Result<Self> {
        let root = root.into();
        if let Err(err) = fs::create_dir_all(&root) {
            return Err(Error::Store {
                driver: "file",
                reason: format!("could not create {}: {err}", root.display()),
            });
        }
        // The directory is `0700` as well, and it is not redundant with the files: a directory's mode
        // is what governs `readdir`, so a world-executable directory would let a stranger learn the
        // *names* of every cached key — the hashes, not the keys, but also how many and when.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Err(err) = fs::set_permissions(&root, fs::Permissions::from_mode(0o700)) {
                return Err(Error::Store {
                    driver: "file",
                    reason: format!("could not set the mode of {}: {err}", root.display()),
                });
            }
        }

        let mut store = FileStore {
            root,
            max_bytes,
            used_bytes: Mutex::new(0),
            last_sweep_ms: AtomicU64::new(0),
            started: Instant::now(),
        };
        // Accounted for at startup rather than lazily, so the first insert already knows whether it
        // fits. A lazy count would make the bound apply "after the first sweep", which on a
        // deployment that never fills up is never — and on one that does is a refusal a restart too
        // late. This also *removes* the expired files the last run left, which is the only time
        // anything here can afford a full pass.
        store.used_bytes = Mutex::new(store.measure(true)?);
        Ok(store)
    }

    /// The directory this store owns. For a diagnostic, and for `config.describe`.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The path a key is stored at.
    ///
    /// The whole traversal defence, in one function: the name is derived from a hash, so no byte of a
    /// key can reach the filesystem. Written as a method rather than inline at four call sites so
    /// there is exactly one place a future change could break it.
    fn path_for(&self, key: &[u8]) -> PathBuf {
        self.root.join(hex(digest(key)))
    }

    /// Walk the directory, summing what is stored and removing what has expired.
    ///
    /// `remove_expired` decides whether this is the startup pass (which removes, and is the only pass
    /// that may afford to) or a sweep before a refusal. Both read every file's header rather than its
    /// value — the header is the first line, and a `read` of the whole file to learn a timestamp
    /// would make a sweep proportional to the *bytes* stored rather than to the number of files.
    ///
    /// A file that cannot be read is **not** fatal: a store whose directory contains one unreadable
    /// file must not refuse to start, or a cache becomes a single point of failure for the process.
    /// It is skipped, which under-counts the bound by its size — the safe direction, since the
    /// alternative is refusing an insert because of a file nobody can read.
    fn measure(&self, remove_expired: bool) -> Result<usize> {
        let entries = match fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(err) => {
                return Err(Error::Store {
                    driver: "file",
                    reason: format!("could not read {}: {err}", self.root.display()),
                })
            }
        };
        let mut total = 0usize;
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            if !metadata.is_file() {
                continue;
            }
            let bytes = metadata.len() as usize;
            if !remove_expired {
                total = total.saturating_add(bytes);
                continue;
            }
            // The startup pass, which is also the pass that can afford to open every file. A file
            // that has expired is *removed*, so the disk a process left behind does not decide what
            // the next one may store.
            match read_header(&path) {
                Header::Expired => {
                    let _ = fs::remove_file(&path);
                }
                Header::Corrupt => {
                    // Removed rather than kept: it cannot be read as a value, and keeping it means
                    // counting bytes for nothing while every `get` on that key pays for a parse that
                    // fails. Nothing legitimate produces one, so it is debris.
                    let _ = fs::remove_file(&path);
                }
                Header::Live { .. } | Header::Unreadable => {
                    // `Unreadable` is counted, not removed: a permission problem is the operator's,
                    // and deleting a file this process cannot read is a bigger decision than
                    // accounting for it.
                    total = total.saturating_add(bytes);
                }
            }
        }
        Ok(total)
    }

    /// Take bytes into the accounting, refusing if they do not fit.
    ///
    /// The bound is checked **before** anything is written, so a store that is full leaves the
    /// directory as it was rather than writing a file and then deleting it. The sweep runs here —
    /// only here — for the reason the memory driver states: it is the moment the alternative is an
    /// error.
    fn reserve(&self, bytes: usize) -> Result<()> {
        let mut used = self
            .used_bytes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if used.saturating_add(bytes) <= self.max_bytes {
            *used = used.saturating_add(bytes);
            return Ok(());
        }
        // Room may have been freed by keys expiring since the last insert. The sweep is rate-limited
        // and *re-measures*, so a store that is still full is one whose files are all live and there
        // is nothing to do but refuse.
        drop(used);
        if self.sweep_is_due() {
            let measured = self.measure(true)?;
            let mut used = self
                .used_bytes
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            *used = measured;
            if used.saturating_add(bytes) <= self.max_bytes {
                *used = used.saturating_add(bytes);
                return Ok(());
            }
        }
        Err(Error::Full {
            max: self.max_bytes,
        })
    }

    /// Give bytes back, when a key is removed or replaced.
    fn release(&self, bytes: usize) {
        let mut used = self
            .used_bytes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *used = used.saturating_sub(bytes);
    }

    /// Whether a sweep is allowed now, which also claims the slot.
    ///
    /// Claimed rather than asked, so two concurrent refusals do not both sweep: the second sees a
    /// `last_sweep_ms` the first has just set. A `load` followed by a `store` in the caller would
    /// have the same shape with a wider window, and the window is exactly where a full cache under
    /// load would spend its time.
    fn sweep_is_due(&self) -> bool {
        let now_ms = self.started.elapsed().as_millis() as u64;
        let interval = SWEEP_INTERVAL.as_millis() as u64;
        let last = self.last_sweep_ms.load(Ordering::Relaxed);
        if now_ms.saturating_sub(last) < interval {
            return false;
        }
        let _ =
            self.last_sweep_ms
                .compare_exchange(last, now_ms, Ordering::Relaxed, Ordering::Relaxed);
        true
    }
}

impl Store for FileStore {
    fn name(&self) -> &'static str {
        "file"
    }

    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        read(&self.path_for(key))
    }

    fn set(&self, key: &[u8], value: Vec<u8>, ttl: Option<Duration>) -> Result<()> {
        if ttl.is_some_and(|ttl| ttl.is_zero()) {
            self.delete(key)?;
            return Ok(());
        }
        let ttl = clamp_ttl(ttl);
        let expires = ttl.map(|ttl| unix_now() + ttl.as_secs());

        let path = self.path_for(key);
        // The *old* file's size is what the accounting gives back, not the value the caller is
        // replacing it with: an overwrite of a shorter value with a longer one would otherwise
        // release the wrong number, and the bound would drift upward on every rewrite.
        let previous = fs::metadata(&path)
            .map(|meta| meta.len() as usize)
            .unwrap_or(0);

        let mut body = Vec::with_capacity(value.len() + 32);
        match expires {
            Some(deadline) => body.extend_from_slice(format!("{EXPIRY}{deadline}\n").as_bytes()),
            None => body.extend_from_slice(format!("{EXPIRY}{NEVER}\n").as_bytes()),
        }
        body.push(b'\n');
        body.extend_from_slice(&value);

        // Reserved as the *net* addition, so a rewrite of one file by another of the same size is
        // not refused for a bound it does not exceed.
        let needed = body.len().saturating_sub(previous);
        self.reserve(needed)?;

        match write_file(&path, &body) {
            Ok(()) => {
                if body.len() < previous {
                    self.release(previous - body.len());
                }
                Ok(())
            }
            Err(err) => {
                // The reservation is given back on failure, or a disk that filled up would leave the
                // bound permanently short by every failed write.
                self.release(needed);
                Err(err)
            }
        }
    }

    fn delete(&self, key: &[u8]) -> Result<bool> {
        let path = self.path_for(key);
        let previous = fs::metadata(&path)
            .map(|meta| meta.len() as usize)
            .unwrap_or(0);
        match fs::remove_file(&path) {
            Ok(()) => {
                self.release(previous);
                Ok(true)
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(err) => Err(Error::Store {
                driver: "file",
                reason: format!("could not remove {}: {err}", path.display()),
            }),
        }
    }

    fn ttl(&self, key: &[u8]) -> Result<Ttl> {
        let path = self.path_for(key);
        match read_header(&path) {
            Header::Live { expires_in } => Ok(match expires_in {
                None => Ttl::Forever,
                Some(remaining) => Ttl::In(remaining),
            }),
            Header::Expired | Header::Unreadable => Ok(Ttl::Missing),
            Header::Corrupt => {
                // Removed, because `ttl` is the one lookup whose answer is *about* the file and the
                // caller has no value to compare against — leaving it means every later lookup pays
                // for the same parse.
                let _ = fs::remove_file(&path);
                Ok(Ttl::Missing)
            }
        }
    }

    fn clear(&self) -> Result<()> {
        let entries = fs::read_dir(&self.root).map_err(|err| Error::Store {
            driver: "file",
            reason: format!("could not read {}: {err}", self.root.display()),
        })?;
        for entry in entries.flatten() {
            let path = entry.path();
            if entry.metadata().map(|meta| meta.is_file()).unwrap_or(false) {
                // Files only, and this store's files only: the directory is assumed to be its own,
                // which is what `open` creating it with `0700` means. A subdirectory is left alone
                // rather than recursively removed — deleting a tree because somebody put one here
                // would be the surprising direction.
                fs::remove_file(&path).map_err(|err| Error::Store {
                    driver: "file",
                    reason: format!("could not remove {}: {err}", path.display()),
                })?;
            }
        }
        let mut used = self
            .used_bytes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *used = 0;
        Ok(())
    }
}

/// How a stored file's header reads.
#[derive(Debug, PartialEq, Eq)]
enum Header {
    /// The key is there.
    Live {
        /// How long it has left, or `None` for no expiry.
        expires_in: Option<Duration>,
    },
    /// The deadline has passed.
    Expired,
    /// The header cannot be parsed.
    Corrupt,
    /// The file is not there.
    Unreadable,
}

/// Read one file, removing it if it has expired.
fn read(path: &Path) -> Result<Option<Vec<u8>>> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => {
            return Err(Error::Store {
                driver: "file",
                reason: format!("could not read {}: {err}", path.display()),
            })
        }
    };
    let Some((header, value)) = split(&bytes) else {
        // No separator: not a file this driver wrote. Removed, as `ttl` does, so the debris does not
        // survive every subsequent lookup.
        let _ = fs::remove_file(path);
        return Ok(None);
    };
    match parse_deadline(header) {
        Ok(None) => Ok(Some(value.to_vec())),
        Ok(Some(deadline)) => {
            if deadline <= unix_now() {
                let _ = fs::remove_file(path);
                return Ok(None);
            }
            Ok(Some(value.to_vec()))
        }
        Err(()) => {
            let _ = fs::remove_file(path);
            Ok(None)
        }
    }
}

/// The header line of a file, without reading its value.
///
/// Bounded by [`HEADER_PROBE`], so this is one `open` and one short `read` whatever the value's size.
fn read_header(path: &Path) -> Header {
    let bytes = match read_header_bytes(path) {
        Ok(bytes) => bytes,
        Err(_) => return Header::Unreadable,
    };
    let Some((header, _)) = split(&bytes) else {
        return Header::Corrupt;
    };
    match parse_deadline(header) {
        Ok(None) => Header::Live { expires_in: None },
        Ok(Some(deadline)) => {
            let now = unix_now();
            if deadline > now {
                Header::Live {
                    expires_in: Some(Duration::from_secs(deadline - now)),
                }
            } else {
                Header::Expired
            }
        }
        Err(()) => Header::Corrupt,
    }
}

/// Split a file into its header line and its value.
///
/// The split is at the **first** `\n\n`, which is always inside the header because the header ends
/// there. A value containing `\n\n` is unaffected, and a file without one — anything not written by
/// this driver — is `None`.
fn split(bytes: &[u8]) -> Option<(&[u8], &[u8])> {
    let position = bytes
        .windows(SEPARATOR.len())
        .position(|window| window == SEPARATOR)?;
    Some((&bytes[..position], &bytes[position + SEPARATOR.len()..]))
}

/// Read just the header of a file, so a sweep costs one `read` per *file* rather than per byte.
///
/// The header is at most `"expiry: "` + twenty digits, so [`HEADER_PROBE`] bytes is comfortably more
/// than one; a file whose separator is not in the first that many bytes cannot be one of ours.
///
/// The probe is a real optimisation rather than tidiness: a sweep before a refusal is what a full
/// cache does instead of storing, and reading every value to learn a timestamp would make it
/// proportional to the bytes stored — which is the size of the cache, on the path where the cache is
/// already under pressure.
fn read_header_bytes(path: &Path) -> std::result::Result<Vec<u8>, std::io::Error> {
    use std::io::Read;

    let mut file = fs::File::open(path)?;
    let mut probe = vec![0u8; HEADER_PROBE];
    let mut filled = 0usize;
    while filled < HEADER_PROBE {
        match file.read(&mut probe[filled..]) {
            Ok(0) => break,
            Ok(read) => filled += read,
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        }
    }
    probe.truncate(filled);
    Ok(probe)
}

/// Parse `expiry: <unix seconds>` or `expiry: never`.
///
/// Returns `Ok(None)` for no expiry, `Ok(Some(deadline))` for a deadline, and `Err(())` for anything
/// else — a `Result` rather than an `Option` so that "no expiry" and "unparseable" are different
/// answers, which is the distinction a `Corrupt` file depends on.
fn parse_deadline(header: &[u8]) -> std::result::Result<Option<u64>, ()> {
    let header = std::str::from_utf8(header).map_err(|_| ())?;
    let rest = header.strip_prefix(EXPIRY).ok_or(())?;
    if rest == NEVER {
        return Ok(None);
    }
    rest.parse::<u64>().map(Some).map_err(|_| ())
}

/// Write one file whole, `0600`, replacing nothing.
///
/// **Written to a temporary name and renamed**, rather than truncated in place. The rename is what
/// makes a concurrent `get` see either the old value or the new one: a `File::create` on the target
/// would let a reader observe a half-written value, which is a cache that returns truncated data
/// rather than a miss. The temporary name is unique per call so two writers of one key cannot
/// interleave into one file.
fn write_file(path: &Path, body: &[u8]) -> Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    // A unique-enough temporary: the process id, the thread's own address and the bytes' length.
    // A counter would be one more field; the address is what makes it unique per thread, and the
    // process id per process. Collisions here would only mean two writers to one key share a
    // temporary, which the rename then makes harmless anyway.
    let temporary = parent.join(format!(
        ".{}.{}.{}.tmp",
        path.file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default(),
        std::process::id(),
        &body as *const _ as usize,
    ));

    let mut options = fs::File::create(&temporary).map_err(|err| Error::Store {
        driver: "file",
        reason: format!("could not create {}: {err}", temporary.display()),
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        options
            .set_permissions(fs::Permissions::from_mode(FILE_MODE))
            .map_err(|err| Error::Store {
                driver: "file",
                reason: format!("could not set the mode of {}: {err}", temporary.display()),
            })?;
    }
    options.write_all(body).map_err(|err| Error::Store {
        driver: "file",
        reason: format!("could not write {}: {err}", temporary.display()),
    })?;
    drop(options);

    fs::rename(&temporary, path).map_err(|err| {
        let _ = fs::remove_file(&temporary);
        Error::Store {
            driver: "file",
            reason: format!("could not replace {}: {err}", path.display()),
        }
    })
}

/// The wall clock, in seconds since the Unix epoch.
///
/// Wall-clock rather than monotonic, and the choice is forced: a deadline that is *stored* has to
/// survive a restart, and an `Instant` has no meaning across one. The cost is that the file store's
/// expiry follows the system clock, so a clock jumping backwards extends a TTL — which is the same
/// behaviour every cache with an absolute deadline has, and stating it is the alternative to a
/// surprise.
///
/// A clock before 1970 answers zero rather than panicking, which makes every deadline in the future
/// and so expires nothing — the conservative direction for a cache whose clock is wrong.
fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or(0)
}

/// A key's digest, as the file's name.
///
/// **A hash written here rather than `DefaultHasher`, and the reason is an upgrade hazard.** This
/// names a file, so it has to be the same value in every process *and across a Rust release*.
/// `std::collections::hash_map::DefaultHasher` explicitly does not promise that: its algorithm may
/// change, and if it did, every cached file would become unreachable and the whole store would look
/// cold after a deploy — silently, and only for deployments that keep a cache across restarts, which
/// is exactly the ones this driver is for.
///
/// `SipHash-1-3`'s construction is overkill for a file name, so this is **FNV-1a** on 128 bits: two
/// 64-bit lanes with different offsets and primes. FNV is what it is — not cryptographic, and not
/// pretending to be — but the property that matters here is stability, and FNV's specification is a
/// published constant rather than an implementation detail. Collision resistance is 2^-64 at worst,
/// which the module documentation already argues a cache may ignore.
///
/// Deliberately not seeded per process, which is the opposite of the rate limiter's choice and for the
/// opposite reason: that one hashes a header so a client cannot fill the table, and this one has to be
/// reproducible or the cache is lost.
fn digest(key: &[u8]) -> u128 {
    const OFFSET_A: u64 = 0xcbf2_9ce4_8422_2325;
    const OFFSET_B: u64 = 0x9e37_79b9_7f4a_7c15;
    const PRIME: u64 = 0x0000_0100_0000_01b3;

    let mut a = OFFSET_A;
    let mut b = OFFSET_B ^ key.len() as u64;
    for byte in key {
        a ^= u64::from(*byte);
        a = a.wrapping_mul(PRIME);
        b ^= u64::from(*byte).wrapping_add(0x9e37);
        b = b.wrapping_mul(PRIME).rotate_left(7);
    }
    // A final avalanche on the second lane, so two keys whose difference is only in a trailing byte
    // do not land next to each other in a directory listing. One multiply and one shift.
    b ^= b >> 32;
    b = b.wrapping_mul(PRIME);
    b ^= b >> 29;

    (u128::from(a) << 64) | u128::from(b)
}

/// Lower-case hexadecimal, thirty-two characters for a `u128`.
fn hex(value: u128) -> String {
    let mut out = String::with_capacity(32);
    for shift in (0..32).rev() {
        let nibble = ((value >> (shift * 4)) & 0xf) as u8;
        out.push(char::from(if nibble < 10 {
            b'0' + nibble
        } else {
            b'a' + nibble - 10
        }));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A directory of its own per test, named for the test so two runs do not share one.
    struct Fixture(PathBuf);

    impl Fixture {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir()
                .join(format!("volucer-kv-file-{label}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&path);
            Fixture(path)
        }

        fn store(&self, max_bytes: usize) -> FileStore {
            FileStore::open(&self.0, max_bytes).expect("the store opens")
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_value_survives_a_reopen_and_a_key_that_was_never_set_is_a_miss() {
        let fixture = Fixture::new("roundtrip");
        {
            let store = fixture.store(1024 * 1024);
            store
                .set(b"key", b"value".to_vec(), None)
                .expect("set succeeds");
            assert_eq!(store.get(b"key").expect("get"), Some(b"value".to_vec()));
            assert_eq!(store.get(b"other").expect("get"), None);
        }
        // The whole point of the driver: a new process, a new store, the same bytes.
        let store = fixture.store(1024 * 1024);
        assert_eq!(
            store.get(b"key").expect("get after reopen"),
            Some(b"value".to_vec()),
            "a file store that did not survive a reopen is the memory driver with extra steps"
        );
    }

    /// A key is bytes and the file name is a hash, so no key can be a path.
    ///
    /// This is the traversal defence and it is the one test in this module that is about safety
    /// rather than about caching. Each of these keys, used as a file name, would either escape the
    /// directory or fail to be a name at all.
    #[test]
    fn no_key_can_name_a_path_outside_the_directory() {
        let fixture = Fixture::new("traversal");
        let store = fixture.store(1024 * 1024);
        for key in [
            &b"../../etc/passwd"[..],
            &b"/etc/passwd"[..],
            &b".."[..],
            &b"a/b/c"[..],
            &b"\0nul"[..],
            &[0xff, 0xfe, 0x00, 0x01][..],
        ] {
            store.set(key, b"v".to_vec(), None).expect("set succeeds");
            assert_eq!(store.get(key).expect("get"), Some(b"v".to_vec()));
            let path = store.path_for(key);
            assert_eq!(
                path.parent(),
                Some(fixture.0.as_path()),
                "{key:?} named {path:?}, which is not in the store's directory"
            );
        }
        // And the directory holds exactly those files, none of which has a component with a slash.
        let count = fs::read_dir(&fixture.0).expect("readdir").count();
        assert_eq!(count, 6, "every key is its own file, with nothing escaping");
    }

    #[test]
    fn a_zero_ttl_deletes_and_an_expired_value_is_a_miss() {
        let fixture = Fixture::new("ttl");
        let store = fixture.store(1024 * 1024);
        store
            .set(b"gone", b"v".to_vec(), Some(Duration::ZERO))
            .expect("set succeeds");
        assert_eq!(store.get(b"gone").expect("get"), None);

        store
            .set(b"brief", b"v".to_vec(), Some(Duration::from_secs(1)))
            .expect("set succeeds");
        assert!(store.ttl(b"brief").expect("ttl").is_present());
        std::thread::sleep(Duration::from_millis(1100));
        assert_eq!(
            store.get(b"brief").expect("get"),
            None,
            "a value whose deadline has passed must be a miss, not a hit"
        );
    }

    #[test]
    fn forever_is_not_the_same_answer_as_missing() {
        let fixture = Fixture::new("forever");
        let store = fixture.store(1024 * 1024);
        store.set(b"k", b"v".to_vec(), None).expect("set");
        assert_eq!(store.ttl(b"k").expect("ttl"), Ttl::Forever);
        assert_eq!(store.ttl(b"absent").expect("ttl"), Ttl::Missing);
    }

    /// The bound is on the bytes written, and it refuses rather than evicting.
    #[test]
    fn the_bound_refuses_rather_than_evicting() {
        let fixture = Fixture::new("bound");
        // Small enough that two values do not fit, large enough that one does.
        let store = fixture.store(200);
        store
            .set(b"a", vec![7u8; 100], None)
            .expect("the first fits");
        let err = store
            .set(b"b", vec![7u8; 100], None)
            .expect_err("the second does not");
        assert!(
            matches!(err, Error::Full { .. }),
            "a full store refuses and says so: {err}"
        );
        // And the value that was there is still there, which is the difference between refusing and
        // evicting: a partial write would have made the cache lose data on a full disk.
        assert_eq!(store.get(b"a").expect("get"), Some(vec![7u8; 100]));
    }

    /// A restart counts what is already on disk, so it cannot double the bytes it holds.
    #[test]
    fn a_reopen_starts_from_what_the_last_run_left() {
        let fixture = Fixture::new("accounting");
        {
            let store = fixture.store(200);
            store
                .set(b"a", vec![7u8; 100], None)
                .expect("the first fits");
        }
        // A store that started at zero would accept a second 100-byte value plus overhead here,
        // and the directory would exceed the bound by the first run's contents.
        let store = fixture.store(200);
        let err = store
            .set(b"b", vec![7u8; 100], None)
            .expect_err("the bound is the directory's, not this process's");
        assert!(matches!(err, Error::Full { .. }), "{err}");
    }

    /// A file this driver did not write is debris, and debris is removed rather than returned.
    #[test]
    fn a_file_with_no_header_is_removed_rather_than_returned_as_a_value() {
        let fixture = Fixture::new("corrupt");
        let store = fixture.store(1024 * 1024);
        let path = store.path_for(b"junk");
        fs::write(&path, b"no separator here").expect("write the debris");

        assert_eq!(store.get(b"junk").expect("get"), None);
        assert!(
            !path.exists(),
            "a file that cannot be parsed must be removed, or every lookup pays for it again"
        );
    }

    /// A value containing the separator is still one value, because the split is at the first one.
    #[test]
    fn a_value_that_contains_the_header_separator_survives() {
        let fixture = Fixture::new("separator");
        let store = fixture.store(1024 * 1024);
        let value = b"line one\n\nline two".to_vec();
        store.set(b"k", value.clone(), None).expect("set");
        assert_eq!(store.get(b"k").expect("get"), Some(value));
    }

    /// `clear` empties this store's directory and nothing else.
    #[test]
    fn clear_removes_every_key_in_this_store_and_releases_the_accounting() {
        let fixture = Fixture::new("clear");
        let store = fixture.store(1024 * 1024);
        store.set(b"a", vec![1u8; 50], None).expect("set");
        store.set(b"b", vec![2u8; 50], None).expect("set");
        store.clear().expect("clear");

        assert_eq!(store.get(b"a").expect("get"), None);
        assert_eq!(store.get(b"b").expect("get"), None);
        // The accounting is released, so the store can take its full bound again.
        store
            .set(b"c", vec![3u8; 900], None)
            .expect("a cleared store has its bound back");
    }

    /// Two stores on two directories do not share anything, which is what `[kv.<name>]` promises.
    #[test]
    fn two_stores_on_two_directories_do_not_share_their_contents() {
        let one = Fixture::new("two-a");
        let two = Fixture::new("two-b");
        let a = one.store(1024 * 1024);
        let b = two.store(1024 * 1024);
        a.set(b"k", b"in a".to_vec(), None).expect("set");
        assert_eq!(b.get(b"k").expect("get"), None);
        assert_eq!(a.get(b"k").expect("get"), Some(b"in a".to_vec()));
    }

    /// An absolute deadline is read from the file, so a restart does not extend a TTL.
    #[test]
    fn a_ttl_is_absolute_so_time_keeps_passing_while_the_process_is_not_running() {
        let fixture = Fixture::new("absolute");
        let store = fixture.store(1024 * 1024);
        store
            .set(b"k", b"v".to_vec(), Some(Duration::from_secs(600)))
            .expect("set");
        assert_eq!(
            store.ttl(b"k").expect("ttl"),
            Ttl::In(Duration::from_secs(600)),
            "a set value with a TTL reads back as its deadline"
        );

        // Rewritten by hand with a deadline in the past, which is what a long sleep would produce
        // without making the test wait. The store is **not** reopened: a reopen would run the
        // startup pass and remove the file, and what this test is about is a deadline being read
        // from disk rather than a duration remembered in memory.
        let path = fixture.0.join(hex(digest(b"k")));
        fs::write(&path, format!("{EXPIRY}{}\n\nv", unix_now() - 1))
            .expect("write an already-expired file");
        assert_eq!(
            store.get(b"k").expect("get"),
            None,
            "a duration read from disk after a restart has no way to know how long it waited, so \
             the deadline is stored absolutely rather than as a remaining duration"
        );
    }

    /// The startup pass removes what expired, so a dead process's disk is not held by dead keys.
    #[test]
    fn expired_files_are_removed_when_the_store_opens() {
        let fixture = Fixture::new("startup-sweep");
        fs::create_dir_all(&fixture.0).expect("create the directory");
        let path = fixture.0.join(hex(digest(b"k")));
        fs::write(&path, format!("{EXPIRY}{}\n\nv", unix_now() - 1)).expect("write");

        let store = fixture.store(1024 * 1024);
        assert!(
            !path.exists(),
            "the startup pass is the one time a full walk is affordable, and it is what frees the \
             disk a previous process left behind"
        );
        assert_eq!(store.get(b"k").expect("get"), None);
    }

    /// A file name is the digest, so the same key is the same file in every process and run.
    ///
    /// `DefaultHasher::new()` is deterministic, which is the property this depends on and the
    /// opposite of what a per-process seed would give. Asserted on a *known* value rather than on
    /// stability across two calls in one process, because it is the cross-process case that matters
    /// and the only way to pin it is to write the value down.
    #[test]
    fn a_key_names_the_same_file_in_every_process() {
        assert_eq!(hex(digest(b"session:1")), hex(digest(b"session:1")));
        // **The literal is the point**: this names a file, so the value has to be the same in every
        // process *and across a Rust release*. The comment on `digest` says why the standard
        // `DefaultHasher` was refused; an assertion on a written-down constant is what keeps that
        // refusal honest, because a change here would silently orphan every cached file.
        assert_eq!(
            hex(digest(b"session:1")),
            "165bc95dcf22e8b2c5f8bfc0b462b629"
        );
        assert_eq!(hex(digest(b"")).len(), 32, "thirty-two hex characters");
        assert_ne!(hex(digest(b"a")), hex(digest(b"b")));
        // A one-byte difference anywhere in a key changes the name, which is what makes a directory
        // listing usable when a cache is behaving oddly.
        for position in 0..8 {
            let mut one = b"abcdefgh".to_vec();
            let mut two = one.clone();
            one[position] = b'x';
            two[position] = b'y';
            assert_ne!(hex(digest(&one)), hex(digest(&two)), "byte {position}");
        }
    }

    /// On unix, every file this driver writes is `0600`.
    ///
    /// The hazard `docs/plugins.md` §6.2 records about RoadRunner's `0755` default, asserted where it
    /// is fixed rather than trusted to the code that sets it.
    #[cfg(unix)]
    #[test]
    fn every_file_it_writes_is_readable_only_by_its_owner() {
        use std::os::unix::fs::PermissionsExt;

        let fixture = Fixture::new("permissions");
        let store = fixture.store(1024 * 1024);
        store.set(b"k", b"v".to_vec(), None).expect("set");
        let path = store.path_for(b"k");
        let mode = fs::metadata(&path)
            .expect("the file exists")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            mode, 0o600,
            "a cached session readable by the group is the hazard this driver exists to not repeat"
        );
    }

    /// An overwrite is accounted by the difference, so rewriting one key does not exhaust the bound.
    #[test]
    fn rewriting_one_key_many_times_does_not_move_the_byte_total() {
        let fixture = Fixture::new("overwrite");
        // Room for exactly one value, and the loop would exhaust it if each write were additive.
        let store = fixture.store(200);
        for _ in 0..20 {
            store
                .set(b"k", vec![1u8; 100], None)
                .expect("an overwrite of the same size always fits");
        }
        assert_eq!(store.get(b"k").expect("get"), Some(vec![1u8; 100]));
    }
}
