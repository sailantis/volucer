//! Integration tests for the `spawn_cgi` pool against real `php-cgi` processes.
//!
//! These are the tests that prove the supervisor actually supervises: it starts
//! processes, serves requests through them, restarts a dead slot, recycles on
//! `max_requests`, and leaves nothing running when it is dropped.

use std::path::{Path, PathBuf};
use std::time::Duration;

use volucer_core::config::SpawnCgiConfig;
use volucer_fcgi::{build_params, Request, RequestInfo, Routing};
use volucer_server::CgiPool;

mod common;
use common::{find_php_cgi, free_port_run, peak_overlap, serial, write_script};

fn config_for(binary: &Path, workers: u32, base_port: u16) -> SpawnCgiConfig {
    SpawnCgiConfig {
        binary: Some(binary.to_path_buf()),
        command: None,
        concurrency: 0,
        working_dir: None,
        workers,
        bind_host: "127.0.0.1".to_string(),
        base_port,
        socket: None,
        max_requests: None,
        request_timeout_ms: None,
        children: 0,
        env: Default::default(),
        legacy_capacity: None,
        services: Vec::new(),
    }
}

/// A request for the script, built through the real CGI environment.
fn request_for(root: &Path, script_name: &str, marker: &str) -> Request {
    let routing = Routing {
        document_root: root.to_path_buf(),
        front_controller: Some(script_name.to_string()),
        ..Routing::default()
    };
    let body = marker.as_bytes().to_vec();
    let info = RequestInfo {
        method: "POST".to_string(),
        uri: "/".to_string(),
        body_len: body.len(),
        ..RequestInfo::default()
    };
    Request {
        params: build_params(&info, &routing).expect("params build"),
        body,
        keep_conn: true,
    }
}

/// A document root plus a script that echoes whatever arrived on php://input.
fn fixture(name: &str) -> (PathBuf, String) {
    let root = std::env::temp_dir().join(format!(
        "volucer-pool-{name}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create the document root");
    write_script(
        &root,
        "index.php",
        r#"<?php echo "MARK="; echo file_get_contents("php://input");"#,
    );
    (root, "index.php".to_string())
}

/// A `spawn_cgi` slot's `env` reaches the PHP process, and `VOLUCER_VERSION` is set even though the
/// configured table does not name it.
///
/// Both halves matter and neither is a detail. `env` on this kind is the only way an application under
/// `php-cgi` is configured at all — the CGI environment volucer builds per request carries request data
/// and nothing else — and `VOLUCER_VERSION` is the one fact a per-request process cannot ask the server
/// for, because it has no channel.
#[test]
fn a_slot_receives_the_configured_environment_and_the_server_version() {
    let _serial = serial();
    let Some(binary) = find_php_cgi() else {
        volucer_testkit::skip("no php-cgi on PATH");
        return;
    };
    let Some(base_port) = free_port_run(1) else {
        volucer_testkit::skip_transient("no free port");
        return;
    };

    let root = std::env::temp_dir().join(format!(
        "volucer-pool-env-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create the document root");
    write_script(
        &root,
        "index.php",
        r#"<?php
           header('Content-Type: text/plain');
           echo "APP=", getenv('VOLUCER_TEST_APP'), "\n";
           echo "OVERRIDDEN=", getenv('VOLUCER_TEST_OVERRIDDEN'), "\n";
           echo "VERSION=", getenv('VOLUCER_VERSION'), "\n";
        "#,
    );

    let mut config = config_for(&binary, 1, base_port);
    config.env = [
        ("VOLUCER_TEST_APP", "from-config"),
        // A value the server sets itself. The pool writes it *after* the configured table, so this
        // entry must not win — that ordering is what makes the version trustworthy.
        ("VOLUCER_VERSION", "spoofed"),
    ]
    .into_iter()
    .map(|(key, value)| (key.to_string(), value.to_string()))
    .collect();
    // And the child must not inherit the variable from *this* process either, which is the failure a
    // `Command::env` without an explicit overwrite would hide on a developer's machine only.
    config
        .env
        .insert("VOLUCER_TEST_OVERRIDDEN".to_string(), "own".to_string());

    let mut pool = CgiPool::start("env-test", &config).expect("a pool starts");
    let response = pool
        .request(&request_for(&root, "index.php", ""))
        .expect("the slot answered");
    response.check_protocol().expect("completed");
    let stdout = String::from_utf8_lossy(&response.stdout).to_string();

    assert!(stdout.contains("APP=from-config"), "{stdout}");
    assert!(stdout.contains("OVERRIDDEN=own"), "{stdout}");
    assert!(
        stdout.contains(&format!("VERSION={}", volucer_server::VERSION)),
        "the server's own version, not the configured value:\n{stdout}"
    );
    assert!(!stdout.contains("spoofed"), "{stdout}");

    pool.shutdown();
    let _ = std::fs::remove_dir_all(&root);
}

/// A pool bound to unix sockets must serve exactly what the TCP pool serves.
///
/// This is the test that makes `socket = "unix:..."` a supported configuration
/// rather than a documented possibility: the same request, the same script, and
/// the same bytes back — with the difference that the transport is a file on disk
/// and `php-cgi`'s own `-b` is given a bare path rather than `host:port`.
#[cfg(unix)]
#[test]
fn a_pool_bound_to_unix_sockets_serves_the_same_bytes() {
    let _serial = serial();
    let Some(binary) = find_php_cgi() else {
        volucer_testkit::skip("no php-cgi on PATH");
        return;
    };

    let (root, script_name) = fixture("unix");
    let prefix = format!("/tmp/volucer-pool-unix-{}", std::process::id());
    let config = SpawnCgiConfig {
        binary: Some(binary.clone()),
        command: None,
        concurrency: 0,
        working_dir: None,
        workers: 2,
        bind_host: "127.0.0.1".to_string(),
        base_port: 19_900,
        socket: Some(format!("unix:{prefix}")),
        max_requests: None,
        request_timeout_ms: None,
        children: 0,
        env: Default::default(),
        legacy_capacity: None,
        services: Vec::new(),
    };

    let mut pool = CgiPool::start("unix-test", &config).expect("a unix-socket pool starts");
    assert_eq!(pool.len(), 2);
    assert!(
        pool.ports().is_empty(),
        "a unix-socket pool binds no ports, got {:?}",
        pool.ports()
    );
    let expected_paths = vec![
        PathBuf::from(format!("{prefix}.0.sock")),
        PathBuf::from(format!("{prefix}.1.sock")),
    ];
    assert_eq!(pool.socket_paths(), expected_paths.as_slice());
    assert!(
        expected_paths.iter().all(|path| path.exists()),
        "php-cgi should have bound both paths"
    );

    for index in 0..4 {
        let response = pool
            .request(&request_for(&root, &script_name, &format!("unix{index}")))
            .expect("a unix-socket slot answered");
        response.check_protocol().expect("completed");
        let stdout = String::from_utf8_lossy(&response.stdout);
        assert!(
            stdout.contains(&format!("MARK=unix{index}")),
            "request {index} returned {stdout:?}"
        );
    }

    pool.shutdown();
    assert!(pool.is_empty());
    // A stopped pool must not leave its sockets behind: the next run would bind
    // them only because php-cgi unlinks first, and an operator would see litter
    // that looks like a live server.
    assert!(
        expected_paths.iter().all(|path| !path.exists()),
        "shutdown should have removed every socket"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// A pool bound to Windows named pipes must serve exactly what the TCP pool serves.
///
/// `php-cgi -b` takes a pipe name on Windows instead of `host:port`, so this is the
/// supported way to keep the CGI arm off loopback TCP there. The same request, the
/// same script and the same bytes back — and a 4 KiB exchange costs about 20% less
/// per request than over loopback TCP (measured: 168 µs against 211 µs, spreads not
/// overlapping), which is the reason to prefer it.
#[cfg(windows)]
#[test]
fn a_pool_bound_to_named_pipes_serves_the_same_bytes() {
    let _serial = serial();
    let Some(binary) = find_php_cgi() else {
        volucer_testkit::skip("no php-cgi on PATH");
        return;
    };
    let (root, script_name) = fixture("pipe");

    // A name unique to this process and test, so parallel binaries cannot collide.
    let prefix = format!(r"\\.\pipe\volucer-pool-{}", std::process::id());
    let config = SpawnCgiConfig {
        binary: Some(binary.clone()),
        command: None,
        concurrency: 0,
        working_dir: None,
        workers: 2,
        bind_host: "127.0.0.1".to_string(),
        base_port: 19_920,
        socket: Some(prefix.clone()),
        max_requests: None,
        request_timeout_ms: None,
        children: 0,
        env: Default::default(),
        legacy_capacity: None,
        services: Vec::new(),
    };

    let mut pool = CgiPool::start("pipe-test", &config).expect("a pipe pool starts");
    assert_eq!(pool.len(), 2);
    assert!(
        pool.ports().is_empty(),
        "a pipe pool binds no ports, got {:?}",
        pool.ports()
    );
    let expected = vec![format!("{prefix}.0"), format!("{prefix}.1")];
    let mut endpoints = pool.endpoints();
    endpoints.sort();
    assert_eq!(endpoints, expected, "each slot gets its own pipe");

    for index in 0..4 {
        let response = pool
            .request(&request_for(&root, &script_name, &format!("pipe{index}")))
            .expect("a pipe slot answered");
        response.check_protocol().expect("completed");
        let stdout = String::from_utf8_lossy(&response.stdout);
        assert!(
            stdout.contains(&format!("MARK=pipe{index}")),
            "request {index} returned {stdout:?}"
        );
    }

    pool.shutdown();
    assert!(pool.is_empty());
    let _ = std::fs::remove_dir_all(&root);
}

/// A dead unix-socket slot is replaced on the same path, not on a new one.
#[cfg(unix)]
#[test]
fn a_dead_unix_socket_slot_is_replaced_on_the_same_path() {
    let _serial = serial();
    let Some(binary) = find_php_cgi() else {
        volucer_testkit::skip("no php-cgi on PATH");
        return;
    };

    let (root, script_name) = fixture("unix-restart");
    let prefix = format!("/tmp/volucer-pool-unix-restart-{}", std::process::id());
    let config = SpawnCgiConfig {
        binary: Some(binary.clone()),
        command: None,
        concurrency: 0,
        working_dir: None,
        workers: 1,
        bind_host: "127.0.0.1".to_string(),
        base_port: 19_950,
        socket: Some(format!("unix:{prefix}")),
        max_requests: None,
        request_timeout_ms: None,
        children: 0,
        env: Default::default(),
        legacy_capacity: None,
        services: Vec::new(),
    };

    let mut pool = CgiPool::start("unix-restart", &config).expect("pool starts");
    let before = pool.pids();
    kill(before[0]);
    std::thread::sleep(Duration::from_millis(300));

    let response = pool
        .request(&request_for(&root, &script_name, "unix-recovered"))
        .expect("the pool recovered");
    let stdout = String::from_utf8_lossy(&response.stdout);
    assert!(stdout.contains("MARK=unix-recovered"), "{stdout:?}");
    assert_ne!(pool.pids()[0], before[0], "the dead slot was replaced");
    assert_eq!(
        pool.endpoints(),
        vec![format!("unix:{prefix}.0.sock")],
        "the replacement kept the path"
    );

    pool.shutdown();
    let _ = std::fs::remove_dir_all(&root);
}

/// `children > 0` must actually let one slot serve several requests at once.
///
/// **This is the claim Part B exists for, and it is the one a throughput number cannot
/// establish.** With `children = 0` the slot's permit count is 1 and a second request blocks
/// until the first finishes — which is correct and is what every earlier test already
/// covers. With `children = 4` four requests must be *in flight together* against one
/// address.
///
/// **Measured as overlap, not as a ratio.** The previous version compared the preforked
/// half's wall time against the control's, and both numbers move with whatever else the
/// machine is doing: it failed 18 times out of 18 when six copies of this suite were run at
/// once, on runs where the pool was behaving perfectly. Timing is evidence here only in
/// aggregate, and one round is not an aggregate.
///
/// So the fixture records **when each request was being executed inside PHP**, and the
/// assertion is the property itself: of `AT_ONCE` requests, the largest number being executed
/// at the same instant. A serialised slot can only ever reach 1, however slow the machine is;
/// a preforked slot reaches `AT_ONCE` once its children have booted, however slow the machine
/// is. Nothing in that sentence mentions a duration, so nothing in it can flake on one.
///
/// The `children = 0` half is kept as the negative control: it proves the measurement is not
/// simply reporting `AT_ONCE` for anything that runs, which is the way a test of this shape
/// usually fails — measuring something that is always true.
///
/// **The clock the spans are measured against is PHP's**, not this process's, so the two
/// halves' numbers are never compared with each other and machine speed cannot enter.
#[test]
fn children_lets_one_slot_serve_several_requests_at_once() {
    let _serial = serial();
    let Some(binary) = find_php_cgi() else {
        volucer_testkit::skip("no php-cgi on PATH");
        return;
    };
    let Some(base) = free_port_run(2) else {
        volucer_testkit::skip_transient("no run of free ports");
        return;
    };

    const SLEEP_MS: u64 = 400;
    const AT_ONCE: usize = 4;
    // The preforked half's allowance for children that have not booted yet. A fixed handful
    // rather than a loop until the test passes.
    const BOOT_ROUNDS: usize = 4;

    let root = std::env::temp_dir().join(format!("volucer-pool-children-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create the document root");
    // The fixture records when PHP *started* and *finished* each request, onto one shared file
    // under a lock, so the trace survives four children appending to it at once. `usleep` is what
    // makes two requests' spans comparable: without it every span would be a few hundred
    // microseconds and the overlap below would be measuring the interpreter's start-up.
    //
    // No trailing `$` and no heredoc: the first version of this fixture ended with a stray `$`,
    // which made the script a parse error — php-cgi then answered in microseconds and the
    // *control* assertion below is the only thing that noticed. A test whose fixture does not
    // execute measures the interpreter's error path, which is exactly the shape this defect
    // takes.
    //
    // Forward slashes, because PHP on Windows accepts them and a doubled backslash inside a PHP
    // string literal is one more escape to get wrong.
    let trace = root.join("overlap.txt");
    let trace_php = trace.display().to_string().replace('\\', "/");
    let script = write_script(
        &root,
        "index.php",
        &format!(
            r#"<?php
               $start = microtime(true);
               usleep({sleep});
               $end = microtime(true);
               $f = fopen("{trace}", "a");
               flock($f, LOCK_EX);
               fwrite($f, $start . " " . $end . " " . getmypid() . "\n");
               fflush($f);
               flock($f, LOCK_UN);
               fclose($f);
               echo 'SLEPT';
            "#,
            sleep = SLEEP_MS * 1000,
            trace = trace_php,
        ),
    );
    assert!(script.is_file(), "the fixture script exists");

    // Serve `rounds` rounds of `AT_ONCE` concurrent requests and report the best overlap seen.
    //
    // **Several rounds for the preforked half and one for the control**, which is the only
    // asymmetry here. A preforked slot is listening the instant `CgiPool::start` returns — the
    // master binds, *then* forks — so early rounds can be served by whichever children have
    // booted, and until two of them have, the overlap is 1 for the honest reason that only one
    // worker exists. The control has nothing to fork: its single process had bound and booted
    // before `start` returned, so every round would measure the same thing.
    //
    // The number of rounds is fixed rather than "until the assertion passes", so a pool that
    // truly cannot overlap fails the test rather than being retried into silence.
    let run = |children: u32, port: u16, rounds: usize| -> (usize, usize) {
        let mut config = config_for(&binary, 1, port);
        config.children = children;
        let mut pool = CgiPool::start("children", &config).expect("pool starts");
        let request = request_for(&root, "index.php", "");

        let mut best = 0;
        let mut most_pids = 0;
        for _ in 0..rounds {
            let _ = std::fs::remove_file(&trace);
            std::thread::scope(|scope| {
                for _ in 0..AT_ONCE {
                    let pool = &pool;
                    let request = &request;
                    scope.spawn(move || {
                        let response = pool.request(request).expect("a request was served");
                        response.check_protocol().expect("completed");
                        String::from_utf8_lossy(&response.stdout).into_owned()
                    });
                }
            });
            // The trace is written by PHP before it answers, so by the time every caller has
            // returned it is complete. A moment's grace costs nothing and removes any doubt.
            std::thread::sleep(Duration::from_millis(50));
            let (peak, pids) = peak_overlap(&trace);
            best = best.max(peak);
            most_pids = most_pids.max(pids);
        }
        pool.shutdown();
        (best, most_pids)
    };

    // The control first, on its own port so a lingering socket cannot confuse the second.
    let (serial_peak, serial_pids) = run(0, base, 1);
    // The fixture must actually run, and must have run `AT_ONCE` times. A parse error answers
    // instantly and writes no span, which would make the control assertion below pass for the
    // worst possible reason: an empty trace has a peak overlap of zero.
    assert_eq!(
        serial_pids, 1,
        "children=0 must serve every request from one process, and the trace named {serial_pids} — \
         the fixture did not run as written"
    );
    assert_eq!(
        serial_peak, 1,
        "children=0 served {AT_ONCE} requests with an overlap of {serial_peak}, but one process \
         cannot serve two requests at once — the fixture or the measurement is wrong, not the pool"
    );

    let (preforked_peak, preforked_pids) = run(AT_ONCE as u32, base + 1, BOOT_ROUNDS);
    // **Two, not `AT_ONCE`.** The property is "several at once", and it is worth asserting at the
    // weakest form that a serialised pool can never reach: a loaded machine can leave two children
    // descheduled mid-request, and a test demanding all four would then be measuring the machine
    // again. Anything above one is a fact a one-at-a-time slot cannot produce.
    assert!(
        preforked_peak >= 2,
        "children={AT_ONCE} served {AT_ONCE} requests across {preforked_pids} process(es) with a \
         peak overlap of {preforked_peak} — the slot is still one request at a time, so `children` \
         is not taking effect"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn starts_a_pool_and_serves_requests_through_every_slot() {
    let _serial = serial();
    let Some(binary) = find_php_cgi() else {
        volucer_testkit::skip("no php-cgi on PATH");
        return;
    };
    let Some(base) = free_port_run(3) else {
        volucer_testkit::skip_transient("no run of three free ports");
        return;
    };

    let (root, script_name) = fixture("serve");
    let mut pool = CgiPool::start("test", &config_for(&binary, 3, base)).expect("pool starts");

    assert_eq!(pool.len(), 3);
    // Consecutive ports from base_port, so every slot is reachable.
    assert_eq!(pool.ports(), vec![base, base + 1, base + 2]);
    assert!(pool.pids().iter().all(|pid| *pid != 0));

    for index in 0..3 {
        let response = pool
            .request(&request_for(&root, &script_name, &format!("slot{index}")))
            .expect("a slot answered");
        response.check_protocol().expect("completed");
        let stdout = String::from_utf8_lossy(&response.stdout);
        assert!(
            stdout.contains(&format!("MARK=slot{index}")),
            "request {index} returned {stdout:?}"
        );
    }

    pool.shutdown();
    assert!(pool.is_empty());
    let _ = std::fs::remove_dir_all(&root);
}

/// A slot spawned at start-up is still alive a second later.
///
/// **This is the regression that made the Linux runner red.** `PR_SET_PDEATHSIG` is bound to
/// the thread that called `fork`, and start-up spawns slots on `std::thread::scope` threads so
/// they boot in parallel. Those threads return as soon as their slot is listening, and the
/// kernel then `SIGKILL`ed every child — so a pool that had just answered one request refused
/// the next with `Connection refused`. The assertion is deliberately about *elapsed time plus
/// liveness* rather than about a second request: a pool whose children are being killed
/// immediately can still pass a single request if it is fast enough, which is why this was
/// mistaken for a flake.
#[cfg(target_os = "linux")]
#[test]
fn every_slot_started_at_startup_is_still_running_a_moment_later() {
    let _serial = serial();
    let Some(binary) = find_php_cgi() else {
        volucer_testkit::skip("no php-cgi on PATH");
        return;
    };
    let Some(base) = free_port_run(3) else {
        volucer_testkit::skip_transient("no run of three free ports");
        return;
    };

    let (root, _script_name) = fixture("longevity");
    let mut pool = CgiPool::start("test", &config_for(&binary, 3, base)).expect("pool starts");
    let pids = pool.pids();

    // The start-up threads have certainly exited by now; the children must not have.
    std::thread::sleep(Duration::from_millis(300));
    for pid in &pids {
        // `is_running` is not the question: `/proc/<pid>` also exists for a zombie, and a
        // `SIGKILL`ed child is exactly that until it is reaped. The state letter is.
        let state = std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .and_then(|stat| stat.rsplit_once(')').map(|(_, rest)| rest.to_string()))
            .and_then(|rest| rest.trim_start().chars().next());
        assert!(
            matches!(state, Some('S' | 'R' | 'D')),
            "slot {pid} was started but did not survive its spawning thread exiting (state \
             {state:?}; Z means the kernel killed it)"
        );
    }

    // And the pool still answers, which is what an operator sees.
    for index in 0..3 {
        let response = pool
            .request(&request_for(&root, "index.php", &format!("alive{index}")))
            .expect("a slot answered");
        let stdout = String::from_utf8_lossy(&response.stdout);
        assert!(stdout.contains(&format!("MARK=alive{index}")), "{stdout:?}");
    }

    pool.shutdown();
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_killed_slot_is_replaced_and_the_pool_keeps_serving() {
    let _serial = serial();
    let Some(binary) = find_php_cgi() else {
        volucer_testkit::skip("no php-cgi on PATH");
        return;
    };
    let Some(base) = free_port_run(1) else {
        volucer_testkit::skip_transient("no free port");
        return;
    };

    let (root, script_name) = fixture("restart");
    let mut pool = CgiPool::start("test", &config_for(&binary, 1, base)).expect("pool starts");
    let before = pool.pids();
    assert_eq!(before.len(), 1);

    // Kill the slot behind the pool's back, the way a crash or an OOM kill would.
    kill(before[0]);
    std::thread::sleep(Duration::from_millis(300));

    // The next request must still succeed: the pool restarts the slot in place.
    let response = pool
        .request(&request_for(&root, &script_name, "recovered"))
        .expect("the pool recovered");
    let stdout = String::from_utf8_lossy(&response.stdout);
    assert!(stdout.contains("MARK=recovered"), "{stdout:?}");

    let after = pool.pids();
    assert_eq!(after.len(), 1, "the pool kept its size");
    assert_ne!(after[0], before[0], "the dead slot was replaced");
    assert_eq!(pool.ports(), vec![base], "the replacement kept the port");

    pool.shutdown();
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn recycling_replaces_the_process_after_max_requests() {
    let _serial = serial();
    let Some(binary) = find_php_cgi() else {
        volucer_testkit::skip("no php-cgi on PATH");
        return;
    };
    let Some(base) = free_port_run(1) else {
        volucer_testkit::skip_transient("no free port");
        return;
    };

    let (root, script_name) = fixture("recycle");
    let mut config = config_for(&binary, 1, base);
    config.max_requests = Some(2);
    let mut pool = CgiPool::start("test", &config).expect("pool starts");
    let original = pool.pids()[0];

    for _ in 0..2 {
        let response = pool
            .request(&request_for(&root, &script_name, "x"))
            .expect("a slot answered");
        assert!(String::from_utf8_lossy(&response.stdout).contains("MARK=x"));
    }

    // Two requests is the limit, so the slot must have been replaced.
    let recycled = pool.pids()[0];
    assert_ne!(
        recycled, original,
        "the slot should have been recycled after max_requests"
    );
    assert_eq!(pool.ports(), vec![base], "the recycled slot kept the port");

    // The process that was replaced is *gone*, not merely no longer in the pool.
    //
    // This is the half that the old order broke and that a "the replacement answered" check
    // cannot see. `replace_locked` used to spawn the replacement before killing the original,
    // so on a platform that hands two processes one listening socket the old worker stayed
    // alive and kept serving connections it had already accepted — the pool reported a fresh
    // slot while the previous process was still running. On macOS the second bind is refused
    // instead, which surfaced as `StartupExited { status: 255 }` on the second request.
    //
    // Waiting briefly because a kill is a signal: the process is reaped by `child.wait()` in
    // `replace_locked`, so it has already exited by the time this returns, and the sleep only
    // gives the OS a moment to make `is_running` agree on every platform.
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        !is_running(original),
        "the replaced worker {original} is still running; the replacement did not take the \
         address over cleanly"
    );

    pool.shutdown();
    let _ = std::fs::remove_dir_all(&root);
}

/// **A streamed response counts towards `max_requests` and gives its slot back.**
///
/// Both are the buffered arm's steps, and both were missing from the streamed one: the reader
/// incremented the slot's count and nothing ever acted on it, so a slot that only served large
/// responses was never recycled; and it dropped the client instead of returning it, so every
/// streamed response forced a reconnect.
///
/// **Why the assertion is on the process id.** `max_requests` is observable only through the slot's
/// process being replaced — there is no counter on the public surface, and adding one would let the
/// test agree with itself rather than with the property. A slot configured with `max_requests = 2`
/// must therefore have a different pid once it has served two *streamed* responses, which is what an
/// operator relying on recycling for leak containment depends on.
///
/// **Every request here is streamed, and that is load-bearing.** A buffered request would call
/// `recycle_if_needed` itself and mask the defect, because it reads the same counter. With only
/// streamed requests the counter is the *only* thing that can trigger a replacement.
///
/// The connection going back is asserted as the slot's permit returning to zero after each body,
/// which is the same `Slot::returned` call that returns the client — and the warm path itself is
/// covered by the buffered tests, which share the mechanism.
#[cfg(unix)]
#[test]
fn a_streamed_response_is_counted_and_recycles_the_slot() {
    use volucer_server::Delivery;

    let _serial = serial();
    let Some(binary) = find_php_cgi() else {
        volucer_testkit::skip("no php-cgi on PATH");
        return;
    };
    let Some(base) = free_port_run(1) else {
        volucer_testkit::skip_transient("no free port");
        return;
    };

    let root = std::env::temp_dir().join(format!("volucer-pool-stream-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create the document root");
    // 128 KiB with no `Content-Length`, so `Relay::unknown_length` relays it rather than buffering.
    // Echoed in one `str_repeat` so the body arrives in several FastCGI records. **Double quotes**,
    // because PHP single-quoted strings do not interpret `\r\n` and would send the escape literally.
    let script = write_script(
        &root,
        "index.php",
        "<?php echo \"Content-Type: text/plain\\r\\n\\r\\n\"; echo str_repeat('s', 131072);",
    );
    let script = script
        .file_name()
        .expect("a file name")
        .to_string_lossy()
        .into_owned();

    let mut config = config_for(&binary, 1, base);
    config.max_requests = Some(2);
    let mut pool = CgiPool::start("stream", &config).expect("pool starts");
    let original = pool.pids()[0];
    let request = request_for(&root, &script, "");

    // **Every request is streamed**, and that is load-bearing: a buffered request would call
    // `recycle_if_needed` itself and mask the defect, because it reads the same counter. With only
    // streamed requests the counter is the *only* thing that can trigger a replacement.
    for round in 0..3 {
        let mut relayed = match pool
            .request_relayed(&request, 4096, volucer_fcgi::Relay::unknown_length())
            .expect("the slot answered")
        {
            Delivery::Relayed(relayed) => relayed,
            Delivery::Buffered(_) => {
                panic!("round {round}: a 128 KiB response with no length must be relayed")
            }
        };
        // The body includes the head block (nothing was split off, because the header block and the
        // first body bytes arrived in the same record), so the assertion is on the total rather
        // than on the body alone: every byte the backend wrote must reach the client.
        let mut received = 0usize;
        while let Some(chunk) = relayed.body.blocking_recv() {
            received += chunk.len();
        }
        assert_eq!(
            received,
            "Content-Type: text/plain\r\n\r\n".len() + 131_072,
            "round {round}: every byte arrived"
        );
        // The permit came back with the connection: `Slot::returned` is one call that does both, so
        // a slot left holding a permit after its body ended would be the same defect in the other
        // direction — the pool would shrink by one per streamed response.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while pool.in_flight()[0] != Some(0) && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(
            pool.in_flight(),
            vec![Some(0)],
            "round {round}: the permit was not released when the body ended"
        );
    }

    // Three streamed requests against a limit of two: the second records the count, the slot is
    // replaced before it is handed out again, and the third is served by the replacement. So the
    // pid is the observable, and the third response's bytes are the proof the replacement works.
    let recycled = pool.pids()[0];
    assert_ne!(
        recycled, original,
        "a slot that served `max_requests` streamed responses was not recycled; `served` is \
         incremented but nothing acts on it, so `max_requests` never bounds a slot that only \
         serves large responses"
    );

    pool.shutdown();
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn dropping_the_pool_leaves_no_processes_behind() {
    let _serial = serial();
    let Some(binary) = find_php_cgi() else {
        volucer_testkit::skip("no php-cgi on PATH");
        return;
    };
    let Some(base) = free_port_run(2) else {
        volucer_testkit::skip_transient("no run of two free ports");
        return;
    };

    let pids = {
        let pool = CgiPool::start("test", &config_for(&binary, 2, base)).expect("pool starts");
        let pids = pool.pids();
        assert_eq!(pids.len(), 2);
        pids
        // `pool` is dropped here.
    };

    std::thread::sleep(Duration::from_millis(300));

    for pid in pids {
        assert!(
            !is_running(pid),
            "pid {pid} survived the pool being dropped"
        );
    }

    // The ports being free again is the symptom an orphan would cause.
    for offset in 0..2u16 {
        let port = base + offset;
        assert!(
            std::net::TcpListener::bind(("127.0.0.1", port)).is_ok(),
            "port {port} is still held after the pool was dropped"
        );
    }
}

#[test]
fn a_pool_skips_ports_that_are_already_in_use() {
    let _serial = serial();
    let Some(binary) = find_php_cgi() else {
        volucer_testkit::skip("no php-cgi on PATH");
        return;
    };

    // Hold the base port itself, so the pool has to move past it.
    let blocker = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let base = blocker.local_addr().expect("address").port();
    // `base` comes from the ephemeral range, so `base + 100` can pass 65535 and
    // panic on overflow — which made this test fail or pass depending on which
    // port the OS handed out. Saturating arithmetic keeps the search in range.
    let next_free = (base.saturating_add(1)..base.saturating_add(100))
        .find(|port| std::net::TcpListener::bind(("127.0.0.1", *port)).is_ok());
    let Some(next_free) = next_free else {
        volucer_testkit::skip_transient("no free port after {base}");
        return;
    };

    let mut pool = CgiPool::start("test", &config_for(&binary, 1, base)).expect("pool starts");
    assert_eq!(pool.len(), 1);
    assert_eq!(
        pool.ports(),
        vec![next_free],
        "the occupied port should have been skipped"
    );

    pool.shutdown();
    drop(blocker);
}

/// A `command` slot really serves: the placeholder becomes an address, the process binds it, and
/// the same bytes come back as from a `binary` slot.
///
/// The command here is `php-cgi -b {address}` — deliberately, because it exercises every part of
/// the command path while leaving the *server* identical to the tested one. What it proves is the
/// machinery a non-PHP deployment depends on: `{address}` substitution, `VOLUCER_FCGI_ADDRESS`,
/// spawning through a tokenizer rather than `Command::new(binary)`, and the readiness probe. What
/// it does not prove is another language's dialect, which cannot be tested from here at all.
#[test]
fn a_command_driver_serves_through_the_address_it_was_given() {
    let _serial = serial();
    let Some(binary) = find_php_cgi() else {
        volucer_testkit::skip("no php-cgi on PATH");
        return;
    };
    let Some(base_port) = free_port_run(2) else {
        volucer_testkit::skip_transient("no free port");
        return;
    };

    let (root, script_name) = fixture("command");
    let config = SpawnCgiConfig {
        binary: None,
        command: Some(format!("{} -b {{address}}", binary.display())),
        workers: 2,
        bind_host: "127.0.0.1".to_string(),
        base_port,
        socket: None,
        max_requests: None,
        request_timeout_ms: None,
        children: 0,
        // One request per slot: a bare `php-cgi -b` does not prefork, so this is the honest
        // number for what a `command` slot can answer at once.
        concurrency: 1,
        working_dir: None,
        env: Default::default(),
        legacy_capacity: None,
        services: Vec::new(),
    };

    let mut pool = CgiPool::start("command-test", &config).expect("a command pool starts");
    assert_eq!(pool.len(), 2, "one process per configured slot");
    assert_eq!(pool.ports().len(), 2, "each slot bound its own port");

    let response = pool
        .request(&request_for(&root, &script_name, "from-a-command-slot"))
        .expect("the slot answered");
    response.check_protocol().expect("completed");
    let stdout = String::from_utf8_lossy(&response.stdout);
    assert!(
        stdout.contains("MARK=from-a-command-slot"),
        "the request body has to reach the application: {stdout}"
    );

    pool.shutdown();
    let _ = std::fs::remove_dir_all(&root);
}

/// A command that never mentions `{address}` is refused before anything is spawned.
///
/// The failure it prevents is the worst kind: the program ignores its address, binds something of
/// its own, and the pool connects to nothing — so every request is a 502 after a ten-second
/// startup timeout, with no message naming the key that was wrong.
#[test]
fn a_command_with_no_address_placeholder_is_a_misconfiguration() {
    let _serial = serial();
    let Some(base_port) = free_port_run(1) else {
        volucer_testkit::skip_transient("no free port");
        return;
    };

    let config = SpawnCgiConfig {
        binary: None,
        // A program that exists, so the failure is unambiguously the placeholder rather than a
        // missing file — which is what makes this a test of the rule and not of `PATH`.
        command: Some(if cfg!(windows) {
            "cmd /C exit".to_string()
        } else {
            "true".to_string()
        }),
        workers: 1,
        bind_host: "127.0.0.1".to_string(),
        base_port,
        socket: None,
        max_requests: None,
        request_timeout_ms: None,
        children: 0,
        concurrency: 0,
        working_dir: None,
        env: Default::default(),
        legacy_capacity: None,
        services: Vec::new(),
    };

    let err = CgiPool::start("no-placeholder", &config).expect_err("must be refused");
    let text = err.to_string();
    assert!(
        text.contains("{address}"),
        "the message must name the placeholder: {text}"
    );
}

/// A `command` that exits is reported once, not retried on ten ports.
///
/// `php-cgi` exits when it loses a bind race, which is why that driver retries on the next port.
/// Nothing known is true of a configured command, and retrying a program that died for the
/// operator's own reason — a syntax error, a missing module — turns one loud failure into ten
/// slow ones with the last one's message.
#[test]
fn a_command_that_exits_is_not_retried_across_ports() {
    let _serial = serial();
    let Some(base_port) = free_port_run(1) else {
        volucer_testkit::skip_transient("no free port");
        return;
    };

    let config = SpawnCgiConfig {
        binary: None,
        command: Some(if cfg!(windows) {
            // Exits immediately, and understands no `{address}` because it never looks at it.
            "cmd /C exit 3 -b {address}".to_string()
        } else {
            "false -b {address}".to_string()
        }),
        workers: 1,
        bind_host: "127.0.0.1".to_string(),
        base_port,
        socket: None,
        max_requests: None,
        request_timeout_ms: None,
        children: 0,
        concurrency: 0,
        working_dir: None,
        env: Default::default(),
        legacy_capacity: None,
        services: Vec::new(),
    };

    let started = std::time::Instant::now();
    let err = CgiPool::start("exits", &config).expect_err("the slot cannot start");
    assert!(
        matches!(err, volucer_server::ServerError::StartupExited { .. }),
        "a process that exited must be reported as such: {err:?}"
    );
    // The retry would take ten spawns plus the settle after each connect; this bound is what
    // distinguishes "tried once" from "tried ten times" without measuring the port loop.
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "a command that exited must not be retried across ports; took {:?}",
        started.elapsed()
    );
}

/// Kill a process by id, without pulling in a signal crate.
fn kill(pid: u32) {
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/F"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
    #[cfg(unix)]
    {
        let _ = std::process::Command::new("kill")
            .args(["-9", &pid.to_string()])
            .status();
    }
}

/// Whether a process is still running.
fn is_running(pid: u32) -> bool {
    #[cfg(windows)]
    {
        match std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH"])
            .output()
        {
            Ok(output) => String::from_utf8_lossy(&output.stdout).contains(&pid.to_string()),
            // If we cannot ask, assume it is running rather than assert wrongly.
            Err(_) => true,
        }
    }
    #[cfg(unix)]
    {
        PathBuf::from(format!("/proc/{pid}")).exists()
            || std::process::Command::new("kill")
                .args(["-0", &pid.to_string()])
                .status()
                .map(|status| status.success())
                .unwrap_or(true)
    }
}

/// **A rolling restart replaces every idle `spawn_cgi` slot on its own address.**
///
/// The FastCGI half of Phase 3a. Two properties are asserted together, because either alone passes
/// against a broken implementation: every slot is a new pid (so new code is in front of traffic) and
/// the pool still answers on the *same ports* afterwards (so a deploy did not move the addresses the
/// front-end's routes were built from).
#[test]
fn a_sweep_replaces_every_idle_cgi_slot_and_keeps_its_addresses() {
    let _serial = serial();
    let Some(binary) = find_php_cgi() else {
        volucer_testkit::skip("no php-cgi on PATH");
        return;
    };
    let Some(base) = free_port_run(3) else {
        volucer_testkit::skip_transient("no run of free ports");
        return;
    };
    let (root, script) = fixture("sweep");

    let pool = CgiPool::start("sweep", &config_for(&binary, 3, base)).expect("pool starts");
    let addresses = pool.endpoints();
    let before = pool.pids();
    assert_eq!(before.len(), 3);

    // A request first, so the slot holds a warm connection: a replacement that kept the old
    // connection would be dialling a process that has exited.
    let request = request_for(&root, &script, "warm");
    pool.request(&request).expect("warm-up");

    let report = pool.respawn_idle();
    assert_eq!(report.backend, "sweep");
    assert_eq!(report.replaced, vec![0, 1, 2], "{report:?}");
    assert!(report.skipped_busy.is_empty(), "{report:?}");
    assert!(report.failed.is_empty(), "{report:?}");
    assert!(report.is_complete(), "{report:?}");
    assert!(report.unmanaged.is_none());

    assert_eq!(
        pool.endpoints(),
        addresses,
        "a sweep keeps each slot's address; the front-end's routes were built from them"
    );
    let after = pool.pids();
    for (old, new) in before.iter().zip(after.iter()) {
        assert_ne!(old, new, "{before:?} -> {after:?}");
    }
    for old in &before {
        assert!(!is_running(*old), "the replaced process {old} is gone");
    }

    // And the pool serves, which is the part a pid comparison cannot prove.
    for _ in 0..3 {
        let response = pool.request(&request).expect("a request after the sweep");
        response.check_protocol().expect("completed");
        assert!(
            String::from_utf8_lossy(&response.stdout).contains("warm"),
            "the replacement served the request"
        );
    }

    let _ = std::fs::remove_dir_all(&root);
}

/// **A `spawn_cgi` slot that is serving is skipped, and its request is answered.**
///
/// The `children` path is where this pool's idleness test is not obvious: with `children > 0` a slot
/// is several processes behind one address, so "the slot lock is free" does not mean "idle" — the
/// permit count does. This holds the slot with a slow request and asserts the sweep left it alone.
#[test]
fn a_sweep_leaves_a_busy_cgi_slot_alone() {
    use std::time::Instant;

    let _serial = serial();
    let Some(binary) = find_php_cgi() else {
        volucer_testkit::skip("no php-cgi on PATH");
        return;
    };
    let Some(base) = free_port_run(2) else {
        volucer_testkit::skip_transient("no run of free ports");
        return;
    };

    let root = std::env::temp_dir().join(format!("volucer-pool-sweep-busy-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create the document root");
    let _script = write_script(&root, "index.php", "<?php usleep(1200000); echo 'SLOW-OK';");

    let pool = CgiPool::start("busy", &config_for(&binary, 1, base)).expect("pool starts");
    let before = pool.pids();
    let request = request_for(&root, "index.php", "");
    let started = Instant::now();

    std::thread::scope(|scope| {
        let holder = scope.spawn(|| {
            let response = pool.request(&request).expect("the slow request is served");
            response.check_protocol().expect("completed");
            String::from_utf8_lossy(&response.stdout).into_owned()
        });

        // Wait until the request is genuinely in flight before sweeping, so this cannot pass by
        // racing it. The fixture sleeps for 1.2 s, so 400 ms in is unambiguous.
        while started.elapsed() < Duration::from_millis(400) {
            std::thread::sleep(Duration::from_millis(20));
        }

        let report = pool.respawn_idle();
        assert!(
            report.replaced.is_empty(),
            "a slot with a request in flight must not be replaced: {report:?}"
        );
        assert_eq!(report.skipped_busy, vec![0], "{report:?}");
        assert!(!report.is_complete(), "{report:?}");
        assert_eq!(
            pool.pids(),
            before,
            "the process serving the request is untouched"
        );

        let body = holder.join().expect("no thread panics");
        assert!(
            body.contains("SLOW-OK"),
            "the in-flight response arrived complete: {body:?}"
        );
    });

    // The slot is idle again, so a second sweep does the work the first one deferred.
    let report = pool.respawn_idle();
    assert_eq!(report.replaced, vec![0], "{report:?}");
    assert!(report.is_complete(), "{report:?}");

    let _ = std::fs::remove_dir_all(&root);
}

/// A pool of one slot can be built from a `command` driver and swept as well.
///
/// The `command` driver reaches its address through `{address}`, so a sweep is the case where a
/// replacement must be handed *the same* address: a driver that re-derived one would either move the
/// slot or fail to bind. `php-cgi` covers the `binary` half above; this covers `command`.
#[test]
fn a_command_driven_slot_is_swept_on_the_address_it_was_given() {
    let _serial = serial();
    let Some(binary) = find_php_cgi() else {
        volucer_testkit::skip("no php-cgi on PATH");
        return;
    };
    let Some(base) = free_port_run(1) else {
        volucer_testkit::skip_transient("no free port");
        return;
    };
    let (root, script) = fixture("sweep-command");
    let port = base;

    let mut config = config_for(&binary, 1, base);
    config.binary = None;
    config.command = Some(format!(
        "{} -b {{address}}",
        binary.to_string_lossy().replace('\\', "/")
    ));

    let pool = CgiPool::start("sweep-command", &config).expect("pool starts");
    let addresses = pool.endpoints();
    assert!(
        addresses[0].contains(&port.to_string()),
        "the command was given this slot's address: {addresses:?}"
    );
    let before = pool.pids();

    let report = pool.respawn_idle();
    assert_eq!(report.replaced, vec![0], "{report:?}");
    assert_eq!(
        pool.endpoints(),
        addresses,
        "a replacement is handed the same address"
    );
    assert_ne!(pool.pids(), before);

    let response = pool
        .request(&request_for(&root, &script, "after"))
        .expect("a request after the sweep");
    response.check_protocol().expect("completed");
    assert!(String::from_utf8_lossy(&response.stdout).contains("after"));

    let _ = std::fs::remove_dir_all(&root);
}

/// **A deadline is not a dropped connection, and retrying one spends a second budget.**
///
/// The predicate pair, asserted together because the defect was that they were one predicate:
/// `TimedOut` and `WouldBlock` sat in `is_transport_error`, so the single retry the CGI pool makes
/// re-sent a request whose worker had already been waited for. Measured against the built binary: a
/// three-second fixture with `request_timeout_ms = 1000` answered `504` after 2.1 s, and the message
/// said "a connection attempt failed" rather than naming the deadline an operator had configured.
#[test]
fn a_deadline_is_distinguished_from_a_dropped_connection() {
    use volucer_fcgi::{FcgiError, REQUEST_ID};

    let io = |kind| FcgiError::Io(std::io::Error::from(kind));

    // The two that mean "the deadline expired", and therefore must not be retried.
    for expired in [
        io(std::io::ErrorKind::TimedOut),
        io(std::io::ErrorKind::WouldBlock),
    ] {
        assert!(
            volucer_server::backend::is_deadline_expired(&expired),
            "{expired:?} is a deadline"
        );
    }

    // The four that mean "the connection went away", which is what a retry is for.
    for dropped in [
        io(std::io::ErrorKind::BrokenPipe),
        io(std::io::ErrorKind::ConnectionReset),
        io(std::io::ErrorKind::UnexpectedEof),
        FcgiError::IncompleteResponse {
            request_id: REQUEST_ID,
            complete: false,
        },
    ] {
        assert!(
            !volucer_server::backend::is_deadline_expired(&dropped),
            "{dropped:?} is a dropped connection, not a deadline"
        );
        assert!(
            volucer_server::backend::is_transport_error(&dropped),
            "{dropped:?} must stay retryable, or a recycled slot becomes a 502"
        );
    }

    // And the property the fix states: a deadline is still classified as a transport error — the
    // classification was not wrong, the *retry* was — so a caller that only asks that question is
    // unchanged. The two pools ask the new one first.
    assert!(
        volucer_server::backend::is_transport_error(&io(std::io::ErrorKind::TimedOut)),
        "removing this from the transport set would change the error a caller sees"
    );
}

/// The waited-on pool serves the same request as the blocking one, byte for byte.
///
/// The first thing an awaited exchange has to be is *correct*: the same request must produce the
/// same response through `AsyncClient` as through `Client`. Everything below this test is about what
/// it costs and what it shares; this one is about whether it works at all, and it is deliberately
/// asserted on the body rather than on a status, because a 200 with an empty body would pass a
/// weaker test.
#[cfg(unix)]
#[test]
fn a_pool_answers_the_same_request_through_the_awaited_path() {
    use std::sync::Arc;

    let _serial = serial();
    let Some(binary) = find_php_cgi() else {
        volucer_testkit::skip("no php-cgi on PATH");
        return;
    };
    let Some(base) = free_port_run(1) else {
        volucer_testkit::skip_transient("no free port");
        return;
    };

    let (root, script) = fixture("awaited");
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_test_writer()
        .try_init();
    let pool =
        Arc::new(CgiPool::start("awaited", &config_for(&binary, 1, base)).expect("pool starts"));
    assert!(
        pool.drives_connections(),
        "a loopback port is an address a runtime can drive"
    );

    let request = request_for(&root, &script, "AWAITED");

    // The blocking path first, as the reference.
    let blocking = pool.request(&request).expect("the blocking path answers");
    blocking.check_protocol().expect("completed");
    let expected = String::from_utf8_lossy(&blocking.stdout).into_owned();
    assert!(expected.contains("MARK=AWAITED"), "{expected:?}");

    // And the awaited one, on a **multi-threaded** runtime, because that is the shape the HTTP
    // front-end uses and the shape this has to work in: the future is polled on the caller's runtime
    // while the socket's driver lives on the pool's. A current-thread runtime is not equivalent — it
    // only ever polls the tasks on its own thread, inside its own `block_on`, so a readiness event
    // registered on the pool's runtime would never be delivered while this thread waits. Verified
    // rather than assumed: the current-thread version of this test hangs, which is exactly the
    // distinction `AsyncRuntime`'s docs describe from the other side.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("a caller runtime");
    let awaited = runtime
        .block_on(pool.request_async(&request))
        .expect("the awaited path answers");
    awaited.check_protocol().expect("completed");

    assert_eq!(
        String::from_utf8_lossy(&awaited.stdout),
        expected,
        "the two clients must produce one answer for one request"
    );

    drop(pool);
    let _ = std::fs::remove_dir_all(&root);
}

/// **The awaited path and the blocking path share one admission count.**
///
/// This is the property that makes the change safe rather than merely faster, and it is the one a
/// second counter would have broken silently. `php-cgi` serves one request at a time per child, so a
/// slot whose concurrency is `n` must admit exactly `n` requests — whichever path they arrived on.
/// With two counters a blocking request and an awaited one would each be admitted against their own
/// limit, and the surplus would queue invisibly inside the backend.
///
/// **Measured on the counter itself, not on elapsed time.** An earlier version asserted that a
/// blocking request took at least as long as an awaited one it should have been queued behind, and
/// that assertion cannot fail: with separate counters the blocking request is admitted at once,
/// opens its own connection, and then waits for the single child to finish the awaited request
/// anyway — so the end-to-end duration is the same either way. Two things are needed to observe the
/// difference, and this test has both:
///
/// - **A slot that can really serve two at once** (`children = 2`), so a wrongly admitted request is
///   answered rather than queued behind the first child. That is the only backend shape in which
///   "admitted" and "served" are the same fact.
/// - **An observation of the count while both are running**: with one counter it reads 2, and with a
///   counter per path the shared one the pool reports reads 1. Nothing here depends on how long
///   either request takes, so a loaded runner cannot change the answer.
#[cfg(unix)]
#[test]
fn the_two_paths_share_one_admission_count() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    /// How long the fixture holds a request inside PHP. Wide enough that both requests are in
    /// flight together for far longer than the polling interval below.
    const SLEEP_US: u64 = 900_000;
    /// How long to wait for the count to reach the value under test before concluding it will not.
    const PATIENCE: Duration = Duration::from_secs(5);

    let _serial = serial();
    let Some(binary) = find_php_cgi() else {
        volucer_testkit::skip("no php-cgi on PATH");
        return;
    };
    let Some(base) = free_port_run(1) else {
        volucer_testkit::skip_transient("no free port");
        return;
    };

    let root = std::env::temp_dir().join(format!("volucer-pool-admission-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create the document root");
    let script = write_script(
        &root,
        "index.php",
        &format!("<?php usleep({SLEEP_US}); echo 'SLOW';"),
    );
    let script = script
        .file_name()
        .expect("a file name")
        .to_string_lossy()
        .into_owned();

    // Two children, so the slot's concurrency is 2 and `php-cgi` genuinely answers two connections
    // at the same time. With one child, a second request that *was* wrongly admitted would still
    // wait for the first — the measurement below would then be unable to see the mistake.
    let mut config = config_for(&binary, 1, base);
    config.children = 2;
    let pool = Arc::new(CgiPool::start("admission", &config).expect("pool starts"));
    assert_eq!(
        pool.in_flight(),
        vec![Some(0)],
        "a fresh slot is serving nothing"
    );
    let request = request_for(&root, &script, "");

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("a caller runtime");

    // Poll until the slot's count reaches `want`, or give up after `PATIENCE` or as soon as neither
    // request can still be running. `try_lock` inside `in_flight` yields `None` at the instant
    // another thread holds the bookkeeping lock, which is why this is a poll rather than a sample.
    let settled = |want: usize, running: &dyn Fn() -> bool| -> Option<usize> {
        let deadline = std::time::Instant::now() + PATIENCE;
        loop {
            if let Some(count) = pool.in_flight()[0] {
                if count == want {
                    return Some(count);
                }
            }
            if !running() || std::time::Instant::now() >= deadline {
                return pool.in_flight()[0];
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    };

    let awaited_done = Arc::new(AtomicBool::new(false));
    let blocking_done = Arc::new(AtomicBool::new(false));

    std::thread::scope(|scope| {
        let awaited_flag = Arc::clone(&awaited_done);
        let awaited_pool = Arc::clone(&pool);
        let awaited_request = &request;
        let awaited = scope.spawn(move || {
            let response = runtime
                .block_on(awaited_pool.request_async(awaited_request))
                .expect("the awaited request is served");
            awaited_flag.store(true, Ordering::Release);
            response
        });

        // One request is in flight, and it is one *place* whichever path took it.
        let first = settled(1, &|| !awaited_done.load(Ordering::Acquire));
        assert_eq!(
            first,
            Some(1),
            "the awaited request must hold exactly one place while it is running"
        );

        let blocking_flag = Arc::clone(&blocking_done);
        let blocking_pool = Arc::clone(&pool);
        let blocking_request = &request;
        let blocking = scope.spawn(move || {
            let response = blocking_pool
                .request(blocking_request)
                .expect("the blocking request is served");
            blocking_flag.store(true, Ordering::Release);
            response
        });

        // **The assertion.** Both requests are being served at once, so one admission count reads
        // 2. A counter per path would leave this one at 1 while the blocking request ran against
        // its own — which is exactly the defect, and it is visible here without any timing.
        let both = settled(2, &|| {
            !awaited_done.load(Ordering::Acquire) || !blocking_done.load(Ordering::Acquire)
        });
        assert_eq!(
            both,
            Some(2),
            "the awaited and blocking requests are counted by separate limits: the slot admits \
             `children` requests in total, and a second counter lets it admit that many again, \
             so a one-child slot would serve two at once"
        );

        blocking
            .join()
            .expect("no thread panics")
            .check_protocol()
            .expect("completed");
        awaited
            .join()
            .expect("no thread panics")
            .check_protocol()
            .expect("completed");
    });

    // And every place comes back, whatever happened above.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while pool.in_flight()[0] != Some(0) && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        pool.in_flight(),
        vec![Some(0)],
        "both places must be released once the requests are answered"
    );

    drop(pool);
    let _ = std::fs::remove_dir_all(&root);
}
