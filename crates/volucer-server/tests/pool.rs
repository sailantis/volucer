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
use common::{find_php_cgi, free_port_run, serial, write_script};

fn config_for(binary: &Path, workers: u32, base_port: u16) -> SpawnCgiConfig {
    SpawnCgiConfig {
        binary: Some(binary.to_path_buf()),
        command: None,
        capacity: 0,
        working_dir: None,
        workers,
        bind_host: "127.0.0.1".to_string(),
        base_port,
        socket: None,
        max_requests: None,
        children: 0,
        env: Default::default(),
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
        eprintln!("skipping: no php-cgi on PATH");
        return;
    };
    let Some(base_port) = free_port_run(1) else {
        eprintln!("skipping: no free port");
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
        eprintln!("skipping: no php-cgi on PATH");
        return;
    };

    let (root, script_name) = fixture("unix");
    let prefix = format!("/tmp/volucer-pool-unix-{}", std::process::id());
    let config = SpawnCgiConfig {
        binary: Some(binary.clone()),
        command: None,
        capacity: 0,
        working_dir: None,
        workers: 2,
        bind_host: "127.0.0.1".to_string(),
        base_port: 19_900,
        socket: Some(format!("unix:{prefix}")),
        max_requests: None,
        children: 0,
        env: Default::default(),
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
        eprintln!("skipping: no php-cgi on PATH");
        return;
    };
    let (root, script_name) = fixture("pipe");

    // A name unique to this process and test, so parallel binaries cannot collide.
    let prefix = format!(r"\\.\pipe\volucer-pool-{}", std::process::id());
    let config = SpawnCgiConfig {
        binary: Some(binary.clone()),
        command: None,
        capacity: 0,
        working_dir: None,
        workers: 2,
        bind_host: "127.0.0.1".to_string(),
        base_port: 19_920,
        socket: Some(prefix.clone()),
        max_requests: None,
        children: 0,
        env: Default::default(),
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
        eprintln!("skipping: no php-cgi on PATH");
        return;
    };

    let (root, script_name) = fixture("unix-restart");
    let prefix = format!("/tmp/volucer-pool-unix-restart-{}", std::process::id());
    let config = SpawnCgiConfig {
        binary: Some(binary.clone()),
        command: None,
        capacity: 0,
        working_dir: None,
        workers: 1,
        bind_host: "127.0.0.1".to_string(),
        base_port: 19_950,
        socket: Some(format!("unix:{prefix}")),
        max_requests: None,
        children: 0,
        env: Default::default(),
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
/// address, and the only way to observe that is for the script to sleep: a serialised slot
/// takes four sleeps, a concurrent one takes about one.
///
/// The negative control is the `children = 0` half: if the pool were secretly concurrent
/// already, that half would be fast and would fail — which is what makes the timings mean
/// something rather than both being "fast enough".
#[test]
fn children_lets_one_slot_serve_several_requests_at_once() {
    use std::time::Instant;

    let _serial = serial();
    let Some(binary) = find_php_cgi() else {
        eprintln!("skipping: no php-cgi on PATH");
        return;
    };
    let Some(base) = free_port_run(2) else {
        eprintln!("skipping: no run of free ports");
        return;
    };

    const SLEEP_MS: u64 = 400;
    const AT_ONCE: usize = 4;

    let root = std::env::temp_dir().join(format!("volucer-pool-children-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create the document root");
    // Long enough that serialisation is unmistakable, short enough to keep the test quick.
    //
    // No trailing `$` and no heredoc: the first version of this fixture ended with a stray `$`,
    // which made the script a parse error — php-cgi then answered in microseconds and the
    // *control* assertion below is the only thing that noticed. A test whose fixture does not
    // execute measures the interpreter's error path, which is exactly the shape this defect
    // takes.
    let script = write_script(
        &root,
        "index.php",
        &format!("<?php usleep({}); echo 'SLEPT';", SLEEP_MS * 1000),
    );
    assert!(script.is_file(), "the fixture script exists");

    let time = |children: u32, port: u16| -> (Duration, String) {
        let mut config = config_for(&binary, 1, port);
        config.children = children;
        let mut pool = CgiPool::start("children", &config).expect("pool starts");
        let request = request_for(&root, "index.php", "");

        let started = Instant::now();
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
        let elapsed = started.elapsed();
        // One more request to read the body, because a `scope` cannot return the values from
        // its threads without collecting them and this is simpler.
        let response = pool.request(&request).expect("a request was served");
        let body = String::from_utf8_lossy(&response.stdout).into_owned();
        pool.shutdown();
        (elapsed, body)
    };

    // The control first, on its own port so a lingering socket cannot confuse the second.
    let (serial, body) = time(0, base);
    // The fixture must actually run. A parse error answers instantly, which makes the control
    // assertion below fail *and* would make the preforked half look fast for the wrong reason.
    assert!(
        body.contains("SLEPT"),
        "the fixture did not execute — a parse error would make both halves meaningless: {body:?}"
    );
    assert!(
        serial >= Duration::from_millis(SLEEP_MS * AT_ONCE as u64 * 3 / 4),
        "children=0 served {AT_ONCE} requests in {serial:?}, which is faster than {AT_ONCE} \
         serialised {SLEEP_MS} ms requests — the control is not controlling anything"
    );

    let (preforked, _) = time(AT_ONCE as u32, base + 1);
    // Half the serial time is a generous bound: four requests against four children should be
    // about one sleep plus overhead, but CI machines are slow and the assertion should not
    // flake on a bound the property does not need.
    assert!(
        preforked * 2 < serial,
        "children={AT_ONCE} served {AT_ONCE} requests in {preforked:?} against {serial:?} \
         serialised — the slot is still one request at a time, so `children` is not taking effect"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn starts_a_pool_and_serves_requests_through_every_slot() {
    let _serial = serial();
    let Some(binary) = find_php_cgi() else {
        eprintln!("skipping: no php-cgi on PATH");
        return;
    };
    let Some(base) = free_port_run(3) else {
        eprintln!("skipping: no run of three free ports");
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
        eprintln!("skipping: no php-cgi on PATH");
        return;
    };
    let Some(base) = free_port_run(3) else {
        eprintln!("skipping: no run of three free ports");
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
        eprintln!("skipping: no php-cgi on PATH");
        return;
    };
    let Some(base) = free_port_run(1) else {
        eprintln!("skipping: no free port");
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
        eprintln!("skipping: no php-cgi on PATH");
        return;
    };
    let Some(base) = free_port_run(1) else {
        eprintln!("skipping: no free port");
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
    assert_ne!(
        pool.pids()[0],
        original,
        "the slot should have been recycled after max_requests"
    );
    assert_eq!(pool.ports(), vec![base], "the recycled slot kept the port");

    pool.shutdown();
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn dropping_the_pool_leaves_no_processes_behind() {
    let _serial = serial();
    let Some(binary) = find_php_cgi() else {
        eprintln!("skipping: no php-cgi on PATH");
        return;
    };
    let Some(base) = free_port_run(2) else {
        eprintln!("skipping: no run of two free ports");
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
        eprintln!("skipping: no php-cgi on PATH");
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
        eprintln!("skipping: no free port after {base}");
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
        eprintln!("skipping: no php-cgi on PATH");
        return;
    };
    let Some(base_port) = free_port_run(2) else {
        eprintln!("skipping: no free port");
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
        children: 0,
        // One request per slot: a bare `php-cgi -b` does not prefork, so this is the honest
        // number for what a `command` slot can answer at once.
        capacity: 1,
        working_dir: None,
        env: Default::default(),
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
        eprintln!("skipping: no free port");
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
        children: 0,
        capacity: 0,
        working_dir: None,
        env: Default::default(),
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
        eprintln!("skipping: no free port");
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
        children: 0,
        capacity: 0,
        working_dir: None,
        env: Default::default(),
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
