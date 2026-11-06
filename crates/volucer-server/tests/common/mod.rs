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
///
/// A re-export rather than a copy: the probe is the same one every other suite uses, and the copy
/// that lived here is how `pool.rs` ended up skipping without honouring `VOLUCER_E2E`.
///
/// Re-exported for the suites that spell it bare; `pool.rs` and `frontend.rs` import it from here.
#[allow(unused_imports)]
pub use volucer_testkit::find_php_cgi;

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

/// The largest number of recorded `start end` spans that were running at the same instant, and how
/// many distinct processes recorded any.
///
/// **A property, not a duration.** Two requests that overlap *inside PHP* were served at the same
/// time; one that merely finished quickly may not have been. A serialised backend can only ever
/// reach an overlap of 1 however fast the machine is, and a genuinely concurrent one reaches 2
/// however slow it is, so nothing here can flake on how loaded the runner is.
///
/// Ends are ordered before starts at equal timestamps, so two spans that merely touch are not
/// counted as overlapping: a serialised slot ends one request before the next begins, and calling
/// that a two-way overlap would let a broken implementation satisfy a test that asserts overlap.
pub fn peak_overlap(trace: &Path) -> (usize, usize) {
    let text = std::fs::read_to_string(trace).unwrap_or_default();
    let spans: Vec<(f64, f64, String)> = text
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let start: f64 = parts.next()?.parse().ok()?;
            let end: f64 = parts.next()?.parse().ok()?;
            let pid = parts.next().unwrap_or("?").to_string();
            Some((start, end, pid))
        })
        .collect();
    let pids: std::collections::BTreeSet<&str> =
        spans.iter().map(|(_, _, pid)| pid.as_str()).collect();
    let mut events: Vec<(f64, i32)> = Vec::with_capacity(spans.len() * 2);
    for (start, end, _) in &spans {
        events.push((*start, 1));
        events.push((*end, -1));
    }
    // `-1` sorts before `+1` at the same instant, which is the ordering described above.
    events.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap().then(a.1.cmp(&b.1)));
    let mut live = 0i32;
    let mut peak = 0i32;
    for (_, delta) in events {
        live += delta;
        peak = peak.max(live);
    }
    (peak as usize, pids.len())
}
