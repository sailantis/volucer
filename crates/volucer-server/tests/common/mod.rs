//! Shared helpers for the integration tests.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};

/// Serialise tests that need loopback ports.
///
/// The pool tests contend for a genuinely global resource: free ports are
/// released by the helper before the pool binds them, so two tests running
/// concurrently can be handed the same range. Serialising is the honest fix,
/// rather than hoping the race does not happen.
pub fn serial() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Find `php-cgi` on `PATH`.
pub fn find_php_cgi() -> Option<PathBuf> {
    #[cfg(windows)]
    let names: &[&str] = &["php-cgi.exe", "php-cgi"];
    #[cfg(not(windows))]
    let names: &[&str] = &["php-cgi"];

    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        for name in names {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Find a run of `count` consecutive unused ports, returning the first.
///
/// Consecutive matters: the pool starts at `base_port` and increments, so scattered free
/// ports would not all be used. Every port in the run is held while checking, so the run
/// cannot be picked twice.
///
/// **Scanned from a per-call offset rather than taken from `bind(0)`.** `bind(0)` asks the
/// kernel for "any port", and on `macos-latest` the kernel hands out the most recently
/// released port first — so the next call in the same binary was offered the port the
/// previous one had just given up. Every test in a suite is a thread of one process, so they
/// walked the same numbers in the same order and the loser bound an address another test had
/// already taken. That surfaced as `unsupported FastCGI version 72` — 72 being `H` — when a
/// slot found somebody else's HTTP listener, and as `StartupExited { status: 255 }` when
/// `php-cgi` lost the bind outright.
///
/// `crates/volucer/tests/serve.rs` was fixed this way on its own; this is the copy the
/// `volucer-server` suites use, and it still had the defect. The range is chosen to sit
/// inside every platform's usable window — macOS keeps ephemeral ports up to 65535, Windows
/// reserves much of 49xxx-65xxx for Hyper-V, Linux starts at 32768 — since only "consecutive
/// and currently free" is required.
pub fn free_port_run(count: usize) -> Option<u16> {
    const FIRST: u16 = 20_000;
    const LAST: u16 = 60_000;

    // A per-call offset, not just a per-process one: every test in this binary runs as a
    // thread of the same process, so a `pid`-derived start alone would have them all walking
    // the same ports in the same order.
    static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let call = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

    let span = u32::from(LAST - FIRST - count as u16);
    let start = (std::process::id().wrapping_mul(2654435761) ^ call.wrapping_mul(40503)) % span;
    for attempt in 0..64u32 {
        let base = FIRST + ((start + attempt * 97) % span) as u16;
        let mut held = Vec::with_capacity(count);
        for offset in 0..count {
            match std::net::TcpListener::bind(("127.0.0.1", base + offset as u16)) {
                Ok(listener) => held.push(listener),
                Err(_) => break,
            }
        }
        if held.len() == count {
            drop(held);
            return Some(base);
        }
    }
    None
}

/// Write a PHP script into `root`, creating the file.
pub fn write_script(root: &Path, name: &str, body: &str) -> PathBuf {
    let path = root.join(name);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("create the script directory");
    }
    std::fs::write(&path, body).expect("write the script");
    path
}
