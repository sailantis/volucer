//! Integration tests for the `attach` backend: a worker pool volucer connects to rather than
//! starts.
//!
//! These prove the whole path end to end, with a real `fork()` on the other side: a PHP
//! supervisor boots once, listens on a unix socket, accepts a connection, and forks a child to
//! serve it. volucer dials that socket `workers` times and treats each connection as a slot.
//!
//! **Everything here is Unix-only**, and not as a limitation of the test: `AF_UNIX` sockets and
//! `pcntl_fork` are the two mechanisms the feature is made of.
//!
//! Skipped, with a printed reason, when no `php` binary is on `PATH` or the SDK checkout cannot be
//! found. Set `VOLUCER_E2E=1` where a skip should be a failure.

#![cfg(unix)]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use volucer_core::config::{AttachConfig, BackendConfig, Config};
use volucer_fcgi::{build_params, Request, RequestInfo, Routing};
use volucer_server::{Dispatcher, WorkerPool};

mod common;
use common::serial;

/// Find a PHP binary on `PATH`.
fn find_php() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        for name in ["php", "php8.5", "php8.4", "php8.3"] {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// The SDK the fixture's `require` line points at.
fn sdk_dir() -> PathBuf {
    match std::env::var_os("VOLUCER_SDK_DIR") {
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("..")
            .join("volucer-php-sdk"),
    }
}

/// Why this test cannot run here, if it cannot.
fn skip_reason() -> Option<String> {
    let php = find_php()?;

    // Not a missing file: a PHP build without `pcntl` cannot fork, and the fixture has nothing
    // else it could do. Asking the binary directly is the only honest check, because the
    // extension can be compiled in or loaded from a scan directory.
    let output = Command::new(&php)
        .args([
            "-r",
            "echo function_exists('pcntl_fork') ? 'fork' : 'nofork';",
        ])
        .output()
        .ok()?;
    let answer = String::from_utf8_lossy(&output.stdout);
    if !answer.contains("fork") || answer.contains("nofork") {
        return Some(format!("{} has no pcntl_fork", php.display()));
    }

    let autoload = sdk_dir().join("autoload.php");
    if !autoload.is_file() {
        return Some(format!(
            "the SDK is not at {} - set VOLUCER_SDK_DIR to a volucer-php-sdk checkout",
            autoload.display()
        ));
    }

    None
}

/// Report that this test cannot run here.
fn report_skip(reason: String) {
    match std::env::var("VOLUCER_E2E") {
        Ok(value) if !value.is_empty() && value != "0" => {
            panic!("VOLUCER_E2E is set, so a skipped test is a failure: {reason}")
        }
        _ => eprintln!("skipping: {reason}"),
    }
}

macro_rules! require_php {
    () => {
        if let Some(reason) = skip_reason() {
            report_skip(reason);
            return;
        }
    };
}

/// A temporary directory, removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        // A unix socket path is at most `SUN_LEN` bytes (104 on macOS, 108 on
        // Linux), and the whole path counts - not just the file name. macOS
        // puts `TMPDIR` under `/var/folders/<two>/<long>/T/`, which is
        // already 54 bytes, so a directory name built from the test's name
        // and a nanosecond stamp does not fit: the socket cannot be created
        // and the test fails with "path must be shorter than SUN_LEN".
        //
        // The name is therefore short, and the uniqueness comes from a
        // counter instead of from a timestamp: a nanosecond stamp is 19
        // bytes that the path cannot spare. The tag is kept because it is
        // what makes a leftover directory identifiable, and it is short
        // enough to keep - it is the timestamp, not the tag, that did not
        // fit. A counter is enough for uniqueness: the directory is
        // created, used and removed inside one test, and the process id
        // keeps two processes apart.
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let base = std::env::temp_dir().join(format!("va-{tag}-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&base).expect("create the temp dir");
        TempDir(base)
    }

    fn join(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The `fork()`ing supervisor, written out as a fixture so this test owns its shape.
///
/// Deliberately *not* the SDK's example worker: that file is an application, and a test that
/// edited it would be changing what the other integration tests drive. This is the smallest
/// thing that behaves like a prefork supervisor — boot, listen, accept, fork, serve — so that a
/// failure here is about the backend rather than about a fixture that drifted.
///
/// **The SDK is loaded before the first fork**, and that is load-bearing rather than tidy. The
/// autoloader is lazy, so a class the master has not touched is compiled by the first child that
/// needs it — and a `require` that fatals in a child after the master has already answered shows
/// up as a connection reset on the pool's first request, which is a long way from the cause. It
/// is also half the memory argument: a class table the master has loaded is inherited.
const SUPERVISOR: &str = r#"<?php

declare(strict_types=1);

require $argv[1];

use Volucer\Request;
use Volucer\Response;
use Volucer\Worker;

$relay = $argv[2];

// Force the SDK in now, so every child inherits it.
class_exists(Worker::class);
class_exists(Request::class);
class_exists(Response::class);

// The listener exists *before* the fork, which is what lets it be inherited: an fd is exactly
// what `fork()` shares, so a child holds the same listening socket with no descriptor passing.
// The supervisor is the side that listens because a PHP process cannot hand a socket to another
// process - `ext/sockets` has no `SCM_RIGHTS` - so the side that owns the socket has to be the
// side that owns the fork.
$path = str_starts_with($relay, 'unix:') ? substr($relay, 5) : $relay;
@unlink($path);
$listener = stream_socket_server('unix://' . $path, $errno, $errstr);
if ($listener === false) {
    fwrite(STDERR, "cannot listen on {$path}: {$errstr}\n");
    exit(1);
}

fwrite(STDOUT, "listening\n");
fflush(STDOUT);

// **Accept, then fork.** The accept has to happen in the master, because it holds the listener;
// the fork has to happen *after* the accept, because the connection is what the child serves.
// That ordering is what makes each slot exact: N children sharing one listener accept unfairly,
// so one can take two connections and another none - which for volucer means a slot whose
// connection nobody is reading.
//
// **The `SIGCHLD` handler is as load-bearing as the fork.** A child that exits stays a zombie
// until its parent waits for it, so a supervisor that only accepts and forks grows one per
// connection - measured at exactly 10 zombies for 10 connections. `WNOHANG` in a loop is what
// keeps the handler from blocking in the middle of the accept loop.
pcntl_async_signals(true);
pcntl_signal(SIGCHLD, static function (): void {
    while (pcntl_waitpid(-1, $status, WNOHANG) > 0) {
    }
});

while (true) {
    $connection = @stream_socket_accept($listener, -1);
    if ($connection === false) {
        // A signal interrupted the accept, which happens once per exiting child.
        continue;
    }

    $pid = pcntl_fork();
    if ($pid === -1) {
        fclose($connection);

        continue;
    }
    if ($pid !== 0) {
        // The master must drop its copy, or the socket never reaches end-of-file when the child
        // exits and one fd leaks per connection.
        fclose($connection);

        continue;
    }

    fclose($listener);

    // `create()`, not `connect()`: the connection is already open and is handed over.
    Worker::create($connection, $connection)->run(
        static function (Request $request): Response {
            $path = (string) parse_url($request->uri, PHP_URL_PATH);

            return match ($path) {
                '/health' => Response::json(['status' => 'ok', 'pid' => getmypid()]),
                default => Response::text('attached ' . $request->uri),
            };
        },
    );

    exit(0);
}
"#;

/// A running supervisor, killed on drop.
struct Supervisor {
    child: Child,
}

impl Supervisor {
    /// Start one, and wait until it says it is listening.
    fn start(php: &Path, script: &Path, relay: &str) -> Self {
        let mut child = Command::new(php)
            .arg(script)
            // The SDK's autoloader, which the fixture `require`s as `$argv[1]`. Passed rather
            // than hard-coded so `VOLUCER_SDK_DIR` can point anywhere.
            .arg(sdk_dir().join("autoload.php"))
            .arg(relay)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start the supervisor");

        // A line on stdout, so readiness is a fact rather than a timeout.
        use std::io::{BufRead, BufReader, Read};
        let stdout = child.stdout.take().expect("stdout was piped");
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        let _ = reader.read_line(&mut line);
        if !line.contains("listening") {
            let mut err = String::new();
            if let Some(mut stderr) = child.stderr.take() {
                let _ = stderr.read_to_string(&mut err);
            }
            let _ = child.kill();
            panic!("the supervisor did not come up: {line:?}\nstderr: {err}");
        }

        // Keep draining stdout so a chatty supervisor cannot fill the pipe and block.
        std::thread::spawn(move || {
            let mut sink = String::new();
            while matches!(reader.read_line(&mut sink), Ok(n) if n > 0) {
                sink.clear();
            }
        });

        Supervisor { child }
    }
}

impl Drop for Supervisor {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A GET, with the CGI environment an nginx front-end would have computed.
fn get(uri: &str) -> Request {
    let routing = Routing {
        document_root: Path::new(".").to_path_buf(),
        front_controller: Some("index.php".to_string()),
        ..Routing::default()
    };
    let info = RequestInfo {
        method: "GET".to_string(),
        uri: uri.to_string(),
        ..RequestInfo::default()
    };
    Request {
        params: build_params(&info, &routing).expect("params build"),
        body: Vec::new(),
        keep_conn: true,
    }
}

/// The body of a rendered FastCGI response, with the header block stripped.
fn body_of(response: &volucer_fcgi::Response) -> String {
    let text = String::from_utf8_lossy(&response.stdout);
    match text.split_once("\r\n\r\n") {
        Some((_, body)) => body.to_string(),
        None => text.to_string(),
    }
}

/// The pid a `/health` response reports.
fn pid_of(response: &volucer_fcgi::Response) -> u32 {
    let body = body_of(response);
    body.rsplit(':')
        .next()
        .and_then(|tail| tail.trim_end_matches('}').trim().parse().ok())
        .unwrap_or_else(|| panic!("a pid in {body:?}"))
}

fn attach_config(relay: String, workers: u32, max_requests: Option<u64>) -> AttachConfig {
    AttachConfig {
        relay: Some(relay),
        workers,
        max_requests,
        checksum: Default::default(),
        services: Vec::new(),
        grpc_services: Vec::new(),
    }
}

/// Write the fixture and start a supervisor for it, returning both.
fn supervisor(dir: &TempDir) -> (Supervisor, String) {
    let php = find_php().expect("php was found by skip_reason");
    let script = dir.join("supervisor.php");
    let mut file = std::fs::File::create(&script).expect("write the supervisor");
    file.write_all(SUPERVISOR.as_bytes())
        .expect("write the supervisor");

    let relay = format!("unix:{}", dir.join("workers.sock").display());
    let supervisor = Supervisor::start(&php, &script, &relay);
    (supervisor, relay)
}

/// The backend serves requests over a socket somebody else opened.
#[test]
fn an_attached_pool_serves_requests_over_a_relay_it_did_not_open() {
    let _serial = serial();
    require_php!();

    let dir = TempDir::new("serves");
    let (_supervisor, relay) = supervisor(&dir);
    let pool = WorkerPool::start_attached("app", &attach_config(relay, 2, None), None)
        .expect("attach to the supervisor's relay");

    assert_eq!(2, pool.len(), "one slot per connection");

    for _ in 0..4 {
        let response = pool.request(&get("/hello")).expect("the worker answered");
        assert_eq!(200, response.parse_status_headers().0);
        assert!(
            body_of(&response).starts_with("attached /hello"),
            "the body came from the worker: {:?}",
            body_of(&response)
        );
    }
}

/// Every slot is a different forked child, and both of them serve.
///
/// Two separate facts, and the test needs both. The handshake pid proves the two connections
/// were accepted and forked separately; the concurrent requests prove both slots actually answer.
/// A sequential loop would prove neither — an idle pool hands out one slot repeatedly.
#[test]
fn each_connection_reaches_a_different_forked_child() {
    let _serial = serial();
    require_php!();

    let dir = TempDir::new("pids");
    let (_supervisor, relay) = supervisor(&dir);
    let pool = std::sync::Arc::new(
        WorkerPool::start_attached("app", &attach_config(relay, 2, None), None)
            .expect("attach to the supervisor's relay"),
    );

    let pids: std::collections::BTreeSet<u32> = pool.pids().into_iter().collect();
    assert_eq!(
        2,
        pids.len(),
        "two connections reached two children, not one: {pids:?}"
    );
    assert!(
        pids.iter().all(|pid| *pid != 0),
        "every attached worker reports its own pid: {pids:?}"
    );

    // Both slots really serve. Two requests have to be in flight at once to show that.
    let (a, b) = std::thread::scope(|scope| {
        let first = scope.spawn({
            let pool = std::sync::Arc::clone(&pool);
            move || {
                pool.request(&get("/health"))
                    .expect("the first worker answered")
            }
        });
        let second = scope.spawn({
            let pool = std::sync::Arc::clone(&pool);
            move || {
                pool.request(&get("/health"))
                    .expect("the second worker answered")
            }
        });
        (first.join().unwrap(), second.join().unwrap())
    });

    let first = pid_of(&a);
    let second = pid_of(&b);
    assert_ne!(
        first, second,
        "two concurrent requests must reach two children, not one"
    );
}

/// A supervisor that forks per connection must reap its children, or a
/// `max_requests`-recycling pool fills the process table with one zombie per connection.
///
/// This is not a style check. The measured failure was **exactly 10 zombies for 10
/// connections**: a supervisor that only accepts and forks is correct as long as nobody
/// looks, and a deployment that recycles on `max_requests` looks after a few minutes.
/// `/proc/<pid>/task/<pid>/children` lists the supervisor's own children, so the assertion
/// is about the process table rather than about what the fixture claims to do.
#[test]
fn a_prefork_supervisor_reaps_its_children() {
    let _serial = serial();
    require_php!();

    let dir = TempDir::new("zombies");
    let (supervisor, relay) = supervisor(&dir);
    let pool = WorkerPool::start_attached("app", &attach_config(relay, 1, Some(1)), None)
        .expect("attach to the supervisor's relay");

    // Each request recycles the slot, which closes a connection and dials a new one: one
    // child per request. Without the `SIGCHLD` handler every one of them stays unreaped.
    for _ in 0..8 {
        pool.request(&get("/health")).expect("the worker answered");
    }

    // The pool's own connections are closed by the recycle, so the children are already
    // exiting when the last request ends. A moment for the kernel to deliver `SIGCHLD`.
    std::thread::sleep(std::time::Duration::from_millis(500));

    let children = format!(
        "/proc/{}/task/{}/children",
        supervisor.child.id(),
        supervisor.child.id()
    );
    let Ok(list) = std::fs::read_to_string(&children) else {
        // `/proc` without `CONFIG_PROC_CHILDREN` is a kernel build this test cannot read.
        eprintln!("skipping: {children} is unreadable");
        return;
    };

    // Pids still listed are running *or* zombie: an exited child stays in this file until
    // it is reaped, which is exactly the condition under test.
    let live_or_zombie: Vec<&str> = list.split_whitespace().collect();

    // A supervisor with a working reaper has at most the child currently serving, which is
    // already closed here, so the honest ceiling is "not one per connection".
    assert!(
        live_or_zombie.len() < 4,
        "the supervisor has {} unreaped children after 8 recycles: {list:?}",
        live_or_zombie.len()
    );
}

/// `max_requests` recycles a slot by reconnecting, which is the only recycling an attached pool
/// can do: the process is not ours to kill.
#[test]
fn max_requests_reconnects_rather_than_killing() {
    let _serial = serial();
    require_php!();

    let dir = TempDir::new("recycle");
    let (_supervisor, relay) = supervisor(&dir);
    let pool = WorkerPool::start_attached("app", &attach_config(relay, 1, Some(1)), None)
        .expect("attach to the supervisor's relay");

    let first = pid_of(&pool.request(&get("/health")).expect("the worker answered"));
    // The second request must be answered by a *different* child: the first connection was
    // closed after its one request, and the supervisor forked a replacement for the next accept.
    let second = pid_of(&pool.request(&get("/health")).expect("the worker answered"));

    assert_ne!(
        first, second,
        "recycling must reach a different child, not the same one twice"
    );
}

/// A `unix:` relay that nothing listens on is a startup failure, not a lazy 502.
#[test]
fn a_relay_nobody_listens_on_fails_at_startup() {
    let _serial = serial();
    require_php!();

    let dir = TempDir::new("absent");
    let relay = format!("unix:{}", dir.join("nobody.sock").display());
    let err = WorkerPool::start_attached("app", &attach_config(relay, 1, None), None)
        .expect_err("connecting to a socket nobody listens on must fail");

    let text = err.to_string();
    assert!(text.contains("relay"), "{text}");
    assert!(
        text.contains("nobody.sock"),
        "the message names the address: {text}"
    );
    assert!(
        text.contains("\"app\""),
        "and the backend, so a multi-backend deployment knows which one: {text}"
    );
}

/// The dispatcher builds an `attach` backend and routes a request through it.
///
/// The level above the pool: configuration, `Backend::from_config`, the route table, and the
/// request path — the parts a pool unit test cannot reach.
#[test]
fn the_dispatcher_serves_a_route_from_an_attached_backend() {
    let _serial = serial();
    require_php!();

    let dir = TempDir::new("dispatch");
    let (_supervisor, relay) = supervisor(&dir);

    let mut config = Config {
        server: volucer_core::config::ServerConfig {
            http: Some("127.0.0.1:0".to_string()),
            ..Default::default()
        },
        ..Default::default()
    };
    config.backends.insert(
        "app".to_string(),
        BackendConfig::Attach(attach_config(relay, 1, None)),
    );
    config.server.default_backend = Some("app".to_string());

    let dispatcher = Dispatcher::from_config(&config, Routing::default())
        .expect("an attach backend builds without a program to resolve");

    let response = dispatcher
        .dispatch(&get("/dispatch"))
        .expect("the attached worker answered through the dispatcher");
    assert_eq!(200, response.parse_status_headers().0);
    assert!(
        body_of(&response).contains("/dispatch"),
        "{:?}",
        body_of(&response)
    );
}
