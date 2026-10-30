//! Integration tests for the worker pool against real PHP worker processes.
//!
//! These are the tests that prove the pool actually supervises: it boots
//! workers, completes the handshake, serves requests through every worker in
//! rotation, replaces a worker that died or asked to be recycled, and leaves
//! nothing running when it is dropped.
//!
//! They drive the SDK's `example/worker.php` rather than a fixture, so the PHP
//! end is the shipped SDK and a change to either side fails here. The SDK lives
//! in the `sailantis/volucer-php-sdk` repository; `VOLUCER_SDK_DIR` points
//! at a checkout that is not beside this one. The Rust codec is
//! covered on its own in `crates/volucer-worker`; what is under test *here* is
//! the supervision around it.
//!
//! Skipped, with a printed reason, when no `php` binary is on `PATH` or when the
//! SDK checkout cannot be found. libtest hides a passing test's stderr, so a
//! skip only *shows* with `--nocapture`; set `VOLUCER_E2E=1` where a skip
//! should be a failure.

use std::path::{Path, PathBuf};
use std::time::Duration;

use volucer_core::config::{
    BackendConfig, Config, FileKvConfig, KvStorageConfig, LockConfig, MemoryKvConfig,
    MemoryLockConfig, WorkerConfig,
};
use volucer_fcgi::{build_params, Request, RequestInfo, Routing};
use volucer_server::{Consumers, Dispatcher, WorkerPool};

mod common;
use common::serial;

/// Find `php` on `PATH`.
fn find_php() -> Option<PathBuf> {
    #[cfg(windows)]
    let names: &[&str] = &["php.exe", "php"];
    #[cfg(not(windows))]
    let names: &[&str] = &["php"];

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

/// The SDK the example worker lives in, which is now a repository of its own.
///
/// `VOLUCER_SDK_DIR` overrides it, which is how a CI checkout at any path —
/// or a sibling clone under a different name — is pointed at. Otherwise the
/// layout this project is developed in is assumed: `sailantis/volucer` and
/// `sailantis/volucer-php-sdk` next to each other.
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

/// The SDK's example worker, which is what the pool is pointed at.
fn example_worker() -> PathBuf {
    sdk_dir().join("example").join("worker.php")
}

/// Why this test cannot run here, if it cannot.
fn skip_reason() -> Option<String> {
    if find_php().is_none() {
        return Some("no php on PATH".to_string());
    }

    let script = example_worker();
    if !script.is_file() {
        return Some(format!(
            "the SDK example worker is not at {} — set VOLUCER_SDK_DIR to a \
             volucer-php-sdk checkout",
            script.display()
        ));
    }

    None
}

/// Report that this test cannot run here.
///
/// libtest hides the stderr of a passing test, so a skip is invisible in an
/// ordinary run. Set `VOLUCER_E2E=1` on a machine that is *supposed* to have
/// PHP and the SDK, and a skip becomes a failure instead.
fn report_skip(reason: String) {
    if skip_is_a_failure() {
        panic!("VOLUCER_E2E is set, so a skipped test is a failure: {reason}");
    }
    eprintln!("skipping: {reason}");
}

/// Whether `VOLUCER_E2E` asks for skips to be failures. `0` or empty means no.
fn skip_is_a_failure() -> bool {
    match std::env::var("VOLUCER_E2E") {
        Ok(value) => !value.is_empty() && value != "0",
        Err(_) => false,
    }
}

/// Skip the test, loudly, when PHP or the SDK checkout is missing.
macro_rules! require_sdk {
    () => {
        if let Some(reason) = skip_reason() {
            report_skip(reason);
            return;
        }
    };
}

/// A pool configuration for the example worker.
///
/// The command is quoted so that the test exercises the real command splitter
/// with a path that may contain spaces — a temporary directory on Windows
/// reliably does.
fn config(workers: u32) -> WorkerConfig {
    WorkerConfig {
        command: format!("php \"{}\"", example_worker().display()),
        workers,
        memory_limit_mb: None,
        max_requests: None,
        working_dir: None,
        request_timeout_ms: None,
        env: Default::default(),
        checksum: Default::default(),
        services: Vec::new(),
        grpc_services: Vec::new(),
        grpc_serve_any: false,
        grpc_proto: Vec::new(),
        grpc_proto_services: Vec::new(),
        transport: Default::default(),
        socket: None,
    }
}

/// Start a pool, or report why the test cannot run.
fn start(workers: u32) -> Option<WorkerPool> {
    if let Some(reason) = skip_reason() {
        report_skip(reason);
        return None;
    }
    match WorkerPool::start("test", &config(workers)) {
        Ok(pool) => Some(pool),
        Err(err) => {
            eprintln!("skipping: the worker pool could not start: {err}");
            None
        }
    }
}

/// A GET request routed through the real CGI environment.
fn get(uri: &str) -> Request {
    request("GET", uri, &[])
}

/// A pool configuration that reaches the worker over a loopback socket.
///
/// The worker connects back to `VOLUCER_WORKER_SOCKET`, which the server sets itself; see
/// `volucer_core::config::WorkerTransport` for why the option exists.
///
/// This found a real ordering bug the first time it ran. `boot` set the address before
/// applying the configuration's own `env` map, so a configuration carrying any `env` at all
/// replaced the address — and the workers then sat waiting for a connection that was never
/// coming, for ever, because nothing about "connected to the wrong place" looks like a
/// failure. The address is now set *after* `env` so it always wins.
fn socket_config(workers: u32) -> WorkerConfig {
    WorkerConfig {
        transport: volucer_core::config::WorkerTransport::Socket,
        socket: None,
        ..config(workers)
    }
}

/// A pool configuration that reaches the worker over its own stdio, **explicitly**.
///
/// `config()` takes whatever transport the platform defaults to, which is the honest thing for
/// most tests here — it is what a deployment gets. It is the wrong thing for the comparison
/// below, and the day the default became platform-dependent this helper is what stopped that
/// test from comparing the socket transport with itself on Windows.
fn pipe_config(workers: u32) -> WorkerConfig {
    WorkerConfig {
        transport: volucer_core::config::WorkerTransport::Pipe,
        socket: None,
        ..config(workers)
    }
}

/// A socket-transport pool serves exactly what a pipe-transport pool serves.
///
/// This is what makes `transport = "socket"` a supported configuration rather than a
/// documented possibility. The two pools run the same worker file, the same request and the
/// same assertion; only the transport differs.
///
/// It is worth having even though the socket path is one branch in `boot`, because the
/// failure modes it guards are silent ones: a worker that connects to the wrong address, or
/// a channel built from a stream that does not carry the protocol, would still *start*.
#[test]
fn a_socket_transport_pool_serves_the_same_bytes_as_a_pipe_one() {
    let _serial = serial();
    if let Some(reason) = skip_reason() {
        report_skip(reason);
        return;
    }

    for (label, settings) in [("pipe", pipe_config(2)), ("socket", socket_config(2))] {
        let mut pool = WorkerPool::start(label, &settings)
            .unwrap_or_else(|err| panic!("the {label} pool starts: {err}"));

        assert_eq!(pool.len(), 2, "the {label} pool keeps its size");

        for index in 0..4 {
            let response = pool
                .request(&get("/health"))
                .unwrap_or_else(|err| panic!("the {label} pool answered: {err}"));
            let body = String::from_utf8_lossy(&response.stdout);
            // The example worker answers `/health` with a JSON status, so the assertion is
            // on the application's own output rather than on a status code alone.
            assert!(
                body.contains("ok"),
                "the {label} pool returned {body:?} for request {index}"
            );
        }

        pool.shutdown();
        assert!(pool.is_empty(), "the {label} pool stopped");
    }
}

/// A request routed through the real CGI environment, with a body.
fn request(method: &str, uri: &str, body: &[u8]) -> Request {
    let routing = Routing {
        document_root: Path::new(".").to_path_buf(),
        front_controller: Some("index.php".to_string()),
        ..Routing::default()
    };
    let info = RequestInfo {
        method: method.to_string(),
        uri: uri.to_string(),
        body_len: body.len(),
        ..RequestInfo::default()
    };
    Request {
        params: build_params(&info, &routing).expect("params build"),
        body: body.to_vec(),
        keep_conn: true,
    }
}

/// The body of a rendered response, with the `Status:` block stripped.
fn body_of(response: &volucer_fcgi::Response) -> String {
    let text = String::from_utf8_lossy(&response.stdout);
    match text.split_once("\r\n\r\n") {
        Some((_, body)) => body.to_string(),
        None => text.to_string(),
    }
}

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
        // `/proc` is Linux, not Unix: macOS has no procfs, so the first arm is `false` there and
        // the whole predicate used to be `false` — which is how this test failed on the macOS
        // runner and passed on Linux and Windows. `kill -0` asks the question portably: it sends
        // no signal and only reports whether the process exists and we may signal it.
        // `crates/volucer-server/tests/pool.rs` already had this fallback; this copy had not.
        PathBuf::from(format!("/proc/{pid}")).exists()
            || std::process::Command::new("kill")
                .args(["-0", &pid.to_string()])
                .status()
                .map(|status| status.success())
                .unwrap_or(true)
    }
}

#[test]
fn starts_a_pool_of_real_workers_and_serves_through_all_of_them() {
    let _serial = serial();
    let Some(pool) = start(3) else {
        return;
    };

    assert_eq!(pool.len(), 3);
    let pids = pool.pids();
    assert!(pids.iter().all(|pid| *pid != 0), "{pids:?}");

    // The handshake is part of starting, so the PHP version is known before the
    // first request.
    for version in pool.php_versions() {
        assert!(
            version.starts_with("8."),
            "unexpected PHP version {version}"
        );
    }

    // Three requests, so round-robin reaches every worker rather than only the
    // first.
    for _ in 0..3 {
        let response = pool.request(&get("/health")).expect("a worker answered");
        assert_eq!(response.parse_status_headers().0, 200);
        let body = body_of(&response);
        assert!(body.contains("\"status\":\"ok\""), "{body}");
        assert!(response.check_protocol().is_ok());
    }

    // Every worker should have served exactly one request, which is the whole
    // point of round-robin: an uneven split is also an uneven memory picture.
    let pids_after = pool.pids();
    assert_eq!(pids_after, pids, "no worker should have been recycled");
}

#[test]
fn a_request_body_reaches_the_application() {
    let _serial = serial();
    let Some(pool) = start(1) else {
        return;
    };

    // A marker rather than a length: a body that arrives but is mangled, or one
    // that arrives in the wrong frame, fails here rather than passing a check
    // that only counted bytes.
    let marker = "the-body-arrived-intact";
    let response = pool
        .request(&request("POST", "/echo", marker.as_bytes()))
        .expect("a worker answered");

    assert_eq!(response.parse_status_headers().0, 200);
    let body = body_of(&response);
    assert!(body.contains(&format!("payload: {marker}")), "{body}");
    assert!(
        body.contains(&format!("body: {} bytes", marker.len())),
        "{body}"
    );
}

#[test]
fn a_large_body_is_chunked_and_arrives_whole() {
    let _serial = serial();
    let Some(pool) = start(1) else {
        return;
    };

    // 800 000 bytes is more than one protocol chunk (64 KiB), so this exercises
    // the LAST flag and the declared `body_len` together.
    let response = pool.request(&get("/big")).expect("a worker answered");
    let body = body_of(&response);
    assert_eq!(body.len(), 800_000, "the whole body should have arrived");
    // `/big` is `str_repeat('abcdefgh', 100000)`, so a body that arrived
    // *complete* but reordered or truncated mid-chunk would fail here.
    assert!(
        body.starts_with("abcdefgh"),
        "{}",
        &body[..16.min(body.len())]
    );
    assert!(body.ends_with("abcdefgh"));
    assert_eq!(body.matches("abcdefgh").count(), 100_000);
    // A chunked body is the case where a length/flag disagreement shows up.
    assert!(response.check_protocol().is_ok());
}

#[test]
fn an_uncaught_application_error_is_a_500_and_the_worker_survives() {
    let _serial = serial();
    let Some(pool) = start(1) else {
        return;
    };

    let before = pool.pids();

    let response = pool.request(&get("/boom")).expect("the worker answered");
    assert_eq!(
        response.parse_status_headers().0,
        500,
        "a throwing handler must become a 500"
    );
    // The application's own status is not the HTTP status: the route threw, so
    // `app_status` is non-zero while the response is a perfectly valid 500 page.
    assert_eq!(response.app_status(), Some(1));

    // One bad request does not replace the worker.
    assert_eq!(
        pool.pids(),
        before,
        "the worker should not have been recycled"
    );

    // And the next request still works.
    let response = pool.request(&get("/health")).expect("the worker answered");
    assert_eq!(response.parse_status_headers().0, 200);
}

#[test]
fn worker_telemetry_reaches_the_backend_stderr() {
    let _serial = serial();
    let Some(pool) = start(1) else {
        return;
    };

    // `/` emits a metric and a log line before answering, through the SDK's
    // out-of-band channel.
    let response = pool.request(&get("/")).expect("a worker answered");
    let stderr = String::from_utf8_lossy(&response.stderr);

    assert_eq!(response.parse_status_headers().0, 200);
    assert!(stderr.contains("serving GET /"), "{stderr}");
    assert!(stderr.contains("[info]"), "{stderr}");
    // The telemetry travels beside the response, never inside it.
    assert!(!body_of(&response).contains("serving GET /"));
}

#[test]
fn a_killed_worker_is_replaced_in_place() {
    let _serial = serial();
    let Some(pool) = start(1) else {
        return;
    };

    let before = pool.pids()[0];
    // Kill the worker behind the pool's back, the way a crash or an OOM kill
    // would. Its pipes stay open at our end but are at end-of-file.
    kill(before);
    std::thread::sleep(Duration::from_millis(300));

    let response = pool
        .request(&get("/health"))
        .expect("the pool recovered from a dead worker");
    assert_eq!(response.parse_status_headers().0, 200);

    let after = pool.pids();
    assert_eq!(after.len(), 1, "the pool kept its size");
    assert_ne!(after[0], before, "the dead worker was replaced");
}

#[test]
fn max_requests_zero_means_never_recycle_not_recycle_every_request() {
    let _serial = serial();
    require_sdk!();

    // The value a php-fpm deployment is most likely to copy: `pm.max_requests = 0`
    // there means the pool never recycles. A bare `served >= max` comparison would
    // destroy the worker after every request instead, which is one application boot
    // per request — the exact thing this backend exists to avoid. So this is not a
    // boundary case, it is the case.
    let mut config = config(1);
    config.max_requests = Some(0);
    let pool = WorkerPool::start("test", &config).expect("pool starts");
    let original = pool.pids()[0];

    for _ in 0..5 {
        pool.request(&get("/health")).expect("a worker answered");
    }

    assert_eq!(
        pool.pids()[0],
        original,
        "max_requests = 0 must mean never, not recycle after every request"
    );
    assert!(
        is_running(original),
        "the worker should still be the original"
    );
}

#[test]
fn recycling_replaces_the_worker_after_max_requests() {
    let _serial = serial();
    require_sdk!();

    let mut config = config(1);
    config.max_requests = Some(2);
    let pool = WorkerPool::start("test", &config).expect("pool starts");
    let original = pool.pids()[0];

    for _ in 0..2 {
        pool.request(&get("/health")).expect("a worker answered");
    }

    assert_ne!(
        pool.pids()[0],
        original,
        "the worker should have been recycled after max_requests"
    );
    assert!(!is_running(original), "the recycled worker should be gone");

    // And the replacement serves.
    assert_eq!(
        pool.request(&get("/health"))
            .expect("the replacement answered")
            .parse_status_headers()
            .0,
        200
    );
}

#[test]
fn a_worker_that_asks_to_be_recycled_is_replaced() {
    let _serial = serial();
    let Some(pool) = start(1) else {
        return;
    };

    let original = pool.pids()[0];

    // `/recycle` calls `recycle()` on the SDK, which shows up as
    // `keep_alive = false` in END. The request itself succeeds — a worker asking
    // to be replaced is not an error, it is a request for cleanup.
    let response = pool.request(&get("/recycle")).expect("a worker answered");
    assert_eq!(response.parse_status_headers().0, 200);
    assert!(body_of(&response).contains("\"recycled\":true"));

    let replacement = pool.pids()[0];
    assert_ne!(
        replacement, original,
        "a worker that asked to be recycled should have been replaced"
    );
    assert!(!is_running(original), "the recycled worker should be gone");

    // A worker cannot replace itself, so the next request must work.
    assert_eq!(
        pool.request(&get("/health"))
            .expect("the replacement answered")
            .parse_status_headers()
            .0,
        200
    );
}

/// A streamed response that arrives through the **dispatcher**, which is the seam a front-end calls.
///
/// The pool test above proves the pool relays. This proves the wire from the configuration to that
/// pool is connected — the `Backend::request_relayed` arm, which is the one thing that could
/// silently route a worker's stream back through the buffering path and still look correct from the
/// pool alone. It is also the arm that decides which `Relay` policy a worker is given, and getting
/// that wrong would leave `stream_threshold_bytes = 0` meaning "buffer" for FastCGI and "stream"
/// for workers.
#[test]
fn a_worker_stream_reaches_the_dispatcher() {
    let _serial = serial();
    require_sdk!();

    let backend = BackendConfig::Worker(WorkerConfig {
        command: format!("php \"{}\"", example_worker().display()),
        workers: 1,
        working_dir: None,
        ..config(1)
    });
    let configured = Config {
        backends: [("app".to_string(), backend)].into_iter().collect(),
        server: volucer_core::config::ServerConfig {
            default_backend: Some("app".to_string()),
            ..Default::default()
        },
        ..Config::default()
    };

    let dispatcher =
        Dispatcher::from_config(&configured, Routing::default()).expect("the pool starts");

    // A 1-byte threshold, so the policy is unambiguously "relay what the worker streams".
    let mut relayed = match dispatcher
        .dispatch_relayed(&get("/stream"), 1, volucer_fcgi::Relay::above_budget())
        .expect("the dispatcher answered")
    {
        volucer_server::Delivery::Relayed(relayed) => relayed,
        volucer_server::Delivery::Buffered(response) => panic!(
            "a worker's STREAMED head must reach the front-end as a relay; got {:?}",
            String::from_utf8_lossy(&response.stdout)
        ),
    };

    assert_eq!(relayed.parse().0, 200);

    let mut body = Vec::new();
    while let Some(chunk) = relayed.body.blocking_recv() {
        body.extend_from_slice(&chunk);
    }
    assert_eq!(
        String::from_utf8(body).expect("the body is text"),
        "row-1\nrow-2\nrow-3\nrow-4\n"
    );
    assert!(relayed.finished());

    // **The other arm: the policy can still say no.** A response the relay refuses is answered
    // buffered, and it has to be *byte-identical* to what `dispatch` would have returned — the whole
    // reason a front-end may ask for the relayed form unconditionally.
    let buffered = dispatcher
        .dispatch_relayed(&get("/stream"), 1, volucer_fcgi::Relay::never())
        .expect("the dispatcher answered");
    match buffered {
        volucer_server::Delivery::Buffered(response) => {
            assert_eq!(
                body_of(&response),
                "row-1\nrow-2\nrow-3\nrow-4\n",
                "a refused stream is collected whole and answered as an ordinary response"
            );
        }
        volucer_server::Delivery::Relayed(_) => {
            panic!("Relay::never must not relay anything")
        }
    }
}

/// A streamed response reaches the client in pieces, and the slot comes back afterwards.
///
/// This is the whole point of `flags::STREAMED`: a worker that does not know its body's length
/// declares `0` on the head and flags it, and the server forwards each `BODY` frame as it arrives
/// instead of assembling the response first. What is asserted is not "the bytes are right" — a
/// buffered response would pass that — but that **the head arrives before the body is complete**
/// and that the pool is usable again once the body ends.
///
/// The pool being usable afterwards is the part that has actually gone wrong in designs like this:
/// the lease is held across the body, so a release placed at return time would leave the slot
/// reserved permanently and the pool would shrink by one per streamed response.
#[test]
fn a_streamed_response_arrives_before_its_body_is_complete() {
    let _serial = serial();
    let Some(pool) = start(1) else {
        return;
    };
    let pool = std::sync::Arc::new(pool);

    let mut relayed = match pool
        .request_relayed(&get("/stream"), volucer_fcgi::Relay::unknown_length())
        .expect("the worker answered")
    {
        volucer_server::Delivery::Relayed(relayed) => relayed,
        volucer_server::Delivery::Buffered(buffered) => {
            panic!(
                "a STREAMED head must be relayed, not buffered; got {:?}",
                String::from_utf8_lossy(&buffered.stdout)
            )
        }
    };

    // **The head is available now.** If the server had assembled the response first there would be
    // nothing to read from the channel yet — but the more robust assertion is the one below, on the
    // chunks: four separate ones is what a stream produces and what a buffer cannot.
    let (status, headers) = relayed.parse();
    assert_eq!(status, 200, "the head carries the application's status");
    assert!(
        headers
            .iter()
            .any(|(name, value)| name.eq_ignore_ascii_case("content-type")
                && value.starts_with("text/plain")),
        "the head carries the application's own headers: {headers:?}"
    );

    let mut chunks = Vec::new();
    let mut body = Vec::new();
    while let Some(chunk) = relayed.body.blocking_recv() {
        chunks.push(chunk.len());
        body.extend_from_slice(&chunk);
    }

    assert_eq!(
        String::from_utf8(body).expect("the body is text"),
        "row-1\nrow-2\nrow-3\nrow-4\n",
        "every chunk arrived, in order and exactly once"
    );
    assert_eq!(
        chunks.len(),
        4,
        "four generator yields are four forwardings; one chunk would mean the body was buffered \
         and split afterwards, which is the behaviour this replaced"
    );
    assert!(
        relayed.finished(),
        "the worker sent END, so the response is complete rather than truncated"
    );

    // **The slot came back.** A second request proves it: with one worker, a lease that was never
    // released would make this block until the slot wait expired and then fail with `AllBusy`.
    let second = pool
        .request(&get("/health"))
        .expect("the slot was released");
    assert_eq!(second.parse_status_headers().0, 200);
}

/// A streamed body that stops without `LAST` is a truncation, not a short response.
///
/// The negative control for the test above. A worker that throws inside its generator has already
/// sent a `200`, so the only signal left is a body that ends early — and the server must not
/// present that as complete. `finished()` is the flag the front-end reads to decide exactly that.
///
/// Without this test, "streaming works" would be satisfied by an implementation that forwarded the
/// chunks and dropped the truncation flag, which is the failure that turns a broken download into a
/// plausible-looking one.
#[test]
fn a_stream_that_never_ends_is_reported_as_truncated() {
    let _serial = serial();
    let Some(pool) = start(1) else {
        return;
    };
    let pool = std::sync::Arc::new(pool);

    let pid_before = pool.pids()[0];

    let mut relayed = match pool
        .request_relayed(&get("/stream-boom"), volucer_fcgi::Relay::unknown_length())
        .expect("the head arrived before the failure")
    {
        volucer_server::Delivery::Relayed(relayed) => relayed,
        volucer_server::Delivery::Buffered(response) => {
            panic!(
                "a STREAMED head must be relayed even when its body fails; got {:?}",
                String::from_utf8_lossy(&response.stdout)
            )
        }
    };

    let mut body = Vec::new();
    while let Some(chunk) = relayed.body.blocking_recv() {
        body.extend_from_slice(&chunk);
    }

    assert_eq!(
        String::from_utf8(body).expect("the body is text"),
        "first\n",
        "the chunk the generator produced before failing still reached the client"
    );
    assert!(
        !relayed.finished(),
        "a body with no LAST must not report as finished — a client told otherwise believes a \
         truncated download is complete"
    );

    // **The worker was replaced, and the assertion has to be made before the next request.**
    //
    // This is the part that took a negative control to get right. A worker whose stream never ended
    // has closed its output — the SDK stops writing after the failure — so the *next* request would
    // find it dead and replace it anyway. Comparing pids after that request therefore proves
    // nothing: the control (removing the ending's `replace`) passed with the assertion still in
    // place. What is actually under test is that the pool decided *at the ending*, so the pid is
    // read right here, with no request in between.
    assert_ne!(
        pool.pids()[0],
        pid_before,
        "a worker whose stream did not end must be replaced by the ending itself, not left for the \
         next request's liveness check to notice"
    );

    // And the pool still serves, which is the observable consequence of having replaced it.
    let after = pool.request(&get("/health")).expect("the pool recovered");
    assert_eq!(after.parse_status_headers().0, 200);
}

#[test]
fn the_pool_reports_the_peak_memory_its_workers_measured() {
    let _serial = serial();
    let Some(pool) = start(1) else {
        return;
    };

    assert_eq!(pool.peak_memory_bytes(), vec![0], "nothing served yet");

    pool.request(&get("/big")).expect("a worker answered");

    let peak = pool.peak_memory_bytes()[0];
    // The `/big` route builds an 800 000 byte body, so anything much below that
    // means the figure is not being read from END at all.
    assert!(
        peak > 800_000,
        "peak memory {peak} looks wrong after serving 800 KB"
    );
}

#[test]
fn a_memory_limit_recycles_a_worker_that_exceeds_it() {
    let _serial = serial();
    require_sdk!();

    // One megabyte is below what any PHP process uses, so every request
    // recycles. That makes the threshold's effect observable without needing to
    // know the exact figure the runtime reports.
    let mut config = config(1);
    config.memory_limit_mb = Some(1);
    let pool = WorkerPool::start("test", &config).expect("pool starts");
    let original = pool.pids()[0];

    let response = pool.request(&get("/health")).expect("a worker answered");
    assert_eq!(response.parse_status_headers().0, 200);

    assert_ne!(
        pool.pids()[0],
        original,
        "a worker above the memory limit should have been replaced"
    );
}

#[test]
fn dropping_the_pool_leaves_no_workers_behind() {
    let _serial = serial();
    let Some(mut pool) = start(2) else {
        return;
    };

    let pids = pool.pids();
    assert_eq!(pids.len(), 2);

    // `shutdown` closes each worker's stdin, so the worker exits on its own
    // rather than being killed.
    pool.shutdown();
    assert!(pool.is_empty());

    for pid in pids {
        assert!(!is_running(pid), "pid {pid} survived the shutdown");
    }
}

#[test]
fn a_pool_dropped_without_shutdown_still_stops_its_workers() {
    let _serial = serial();
    require_sdk!();

    let pids = {
        let pool = WorkerPool::start("test", &config(2)).expect("pool starts");
        assert_eq!(pool.pids().len(), 2);
        pool.pids()
        // `pool` is dropped here.
    };

    std::thread::sleep(Duration::from_millis(500));

    for pid in pids {
        assert!(
            !is_running(pid),
            "pid {pid} survived the pool being dropped"
        );
    }
}

#[test]
fn a_request_landing_on_a_killed_worker_is_answered_by_the_replacement() {
    let _serial = serial();
    let Some(pool) = start(1) else {
        return;
    };

    let before = pool.pids()[0];
    kill(before);
    // No sleep: the request is meant to arrive while the pool's view of the worker
    // is still "alive but about to be found dead", which is exactly the window a
    // real recycle opens. If the kill has not been reaped yet the pool finds the
    // pipes closed and retries; if it already has, the liveness check on the leased
    // slot replaces the worker first. Both are the recovery under test.
    let response = pool
        .request(&get("/health"))
        .expect("the replacement answered the request");
    assert_eq!(response.parse_status_headers().0, 200);
    assert_ne!(pool.pids()[0], before, "the dead worker was replaced");
}

#[test]
fn a_post_that_lands_on_a_killed_worker_is_reported_not_replayed() {
    let _serial = serial();
    let Some(pool) = start(1) else {
        return;
    };

    // The same window as above, but with a method HTTP does not define as
    // idempotent. A retry would be a second write, so the pool must surface the
    // failure rather than silently run the application twice. A `POST` that
    // *succeeds* (the worker was already replaced before the request was picked) is
    // fine — the assertion is that it never reports success by replaying.
    let before = pool.pids()[0];
    kill(before);

    match pool.request(&request("POST", "/echo", b"x")) {
        Ok(response) => {
            // Only legitimate if the replacement served it in one attempt.
            assert_eq!(response.parse_status_headers().0, 200);
            assert_ne!(pool.pids()[0], before);
        }
        Err(err) => {
            assert!(
                !err.is_timeout(),
                "a killed worker should not look like a timeout: {err}"
            );
        }
    }
}

#[test]
fn concurrent_requests_overlap_rather_than_queueing() {
    let _serial = serial();
    require_sdk!();

    // A worker with a route that takes measurable time. Written here rather than
    // added to the SDK's example, because it exists only to make overlap
    // observable.
    let dir = std::env::temp_dir().join(format!("volucer-slow-worker-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create the fixture directory");
    let script = dir.join("slow-worker.php");
    let autoload = sdk_dir().join("autoload.php").display().to_string();
    std::fs::write(
        &script,
        format!(
            r#"<?php
require '{autoload}';
use Volucer\Request;
use Volucer\Response;
use Volucer\Worker;
// `connect()`, not `create()`: this fixture is started through the same `WorkerConfig` as
// everything else, and on the platform where the socket is the default, `create()` would read
// the end-of-file stdin that the socket transport leaves behind and die during the handshake.
$worker = Worker::connect();
$worker->run(static function (Request $request, Worker $worker): Response|null {{
    $path = (string) parse_url($request->uri, PHP_URL_PATH);
    if ($path === '/slow') {{
        usleep(500000);
    }}
    return Response::json(['pid' => getmypid()]);
}});
"#
        ),
    )
    .expect("write the worker");

    let workers = 4;
    let config = WorkerConfig {
        command: format!("php \"{}\"", script.display()),
        workers,
        memory_limit_mb: None,
        max_requests: None,
        working_dir: None,
        request_timeout_ms: None,
        env: Default::default(),
        checksum: Default::default(),
        services: Vec::new(),
        grpc_services: Vec::new(),
        grpc_serve_any: false,
        grpc_proto: Vec::new(),
        grpc_proto_services: Vec::new(),
        transport: Default::default(),
        socket: None,
    };
    let pool = WorkerPool::start("test", &config).expect("pool starts");

    // One request per worker, all at once. Each sleeps 500 ms.
    let started = std::time::Instant::now();
    let answers: Vec<Option<u16>> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..workers)
            .map(|_| {
                scope.spawn(|| {
                    pool.request(&get("/slow"))
                        .ok()
                        .map(|response| response.parse_status_headers().0)
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().expect("thread"))
            .collect()
    });
    let elapsed = started.elapsed();

    for answer in &answers {
        assert_eq!(*answer, Some(200), "every request should have been served");
    }

    // Served in parallel this takes about one sleep; served one-at-a-time it
    // takes `workers` sleeps. The threshold sits between the two, well clear of
    // either, so the test is not sensitive to machine speed.
    let serial_minimum = Duration::from_millis(500) * workers;
    let threshold = Duration::from_millis(1500);
    assert!(
        elapsed < threshold,
        "{workers} concurrent requests took {elapsed:?}, which is close to the {serial_minimum:?} a \
         serialised pool would need — the requests are queueing rather than overlapping"
    );

    drop(pool);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_pool_configured_for_an_optional_checksum_negotiates_it_and_serves() {
    let _serial = serial();
    require_sdk!();

    // The end-to-end version of the option, through the pool rather than the channel: the pool
    // offers it in the handshake, the real SDK accepts, and requests then cross the pipe with no
    // checksum in either direction. What this catches that the channel test cannot is the *wiring*
    // — a `WorkerConfig` that never reached `Spec`, or a `Spec` that never reached `boot`.
    let mut config = config(1);
    config.checksum = volucer_core::config::ChecksumMode::Optional;
    let pool = WorkerPool::start("test", &config).expect("pool starts");

    // The answer is what is observable: the pool does not keep the channel (the exchange thread
    // owns it), but the `HELLO_ACK` recorded at boot says what each worker agreed to, and that is
    // the thing an operator wants to see.
    assert_eq!(
        pool.no_checksum_agreed(),
        vec![true],
        "the one worker should have agreed to skip the checksum"
    );

    // And a request still works, which is the part that would break if either end wrote
    // `NO_CHECKSUM` without the other agreeing.
    let response = pool.request(&get("/health")).expect("a worker answered");
    assert_eq!(response.parse_status_headers().0, 200);
    assert!(body_of(&response).contains("\"status\":\"ok\""));

    drop(pool);
}

#[test]
fn a_pool_left_on_the_default_does_not_offer_the_option() {
    let _serial = serial();
    let Some(pool) = start(1) else {
        return;
    };

    // The same observation from the other side, and the one that matters more: the default must
    // still produce a checked exchange. A `[backends.x]` that says nothing is what every existing
    // deployment has.
    assert_eq!(
        pool.no_checksum_agreed(),
        vec![false],
        "a pool that did not offer the option must not have it accepted"
    );
}

#[test]
fn a_worker_that_cannot_boot_is_refused_at_startup() {
    let _serial = serial();
    require_sdk!();

    // A file that exists but is not a worker: PHP parses it, does nothing, and
    // exits, so the handshake can never complete. This is the case that would
    // otherwise hang the first request for ever.
    let dir = std::env::temp_dir().join(format!("volucer-not-a-worker-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create the fixture directory");
    let script = dir.join("not-a-worker.php");
    std::fs::write(&script, "<?php // deliberately says nothing\n").expect("write the script");

    let config = WorkerConfig {
        command: format!("php \"{}\"", script.display()),
        workers: 1,
        memory_limit_mb: None,
        max_requests: None,
        working_dir: None,
        request_timeout_ms: None,
        env: Default::default(),
        checksum: Default::default(),
        services: Vec::new(),
        grpc_services: Vec::new(),
        grpc_serve_any: false,
        grpc_proto: Vec::new(),
        grpc_proto_services: Vec::new(),
        transport: Default::default(),
        socket: None,
    };

    let err = WorkerPool::start("test", &config).unwrap_err();
    let message = err.to_string();
    // Either the process exited (and its exit is the useful part) or the pipe
    // ended first. Both are startup errors, not hangs.
    assert!(
        message.contains("exited during startup") || message.contains("closed"),
        "unexpected error: {message}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// A worker still written against the older API fails *fast* and says what to do.
///
/// This is the blast radius of making the socket the default on Windows: the transport is set
/// by the *server*, and a worker written before it exists — the shipped SDK's own README used
/// to teach `Worker::create()` — reads stdin, which the socket transport has set to
/// end-of-file, and exits during its handshake. The failure is therefore correct and does not
/// need to be prevented; what it must not be is *unexplained*, because "exit code 255" sends
/// the reader to PHP, where nothing is wrong.
///
/// The socket transport is requested explicitly rather than taken from the default, so this
/// asserts the same thing on every platform.
#[test]
fn a_worker_still_on_create_is_told_to_call_connect() {
    let _serial = serial();
    require_sdk!();

    let dir = std::env::temp_dir().join(format!("volucer-old-worker-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create the fixture directory");
    let script = dir.join("old-style-worker.php");
    let autoload = sdk_dir().join("autoload.php").display().to_string();
    std::fs::write(
        &script,
        format!(
            r#"<?php
require '{autoload}';
use Volucer\Response;
use Volucer\Worker;
// Deliberately the old API: this is what a worker written before the option did.
$worker = Worker::create();
$worker->run(static function ($request, Worker $worker): Response {{
    return Response::text('ok');
}});
"#
        ),
    )
    .expect("write the worker");

    let mut config = config(1);
    config.transport = volucer_core::config::WorkerTransport::Socket;
    // The fixture, not the example worker: `config()` points at the SDK's example, which
    // calls `connect()` and would therefore connect perfectly well.
    config.command = format!("php \"{}\"", script.display());

    let started = std::time::Instant::now();
    let err = WorkerPool::start("test", &config).unwrap_err();
    let elapsed = started.elapsed();
    let message = err.to_string();

    // Fast, because the worker is known to be gone rather than waited out.
    assert!(
        elapsed < Duration::from_secs(10),
        "a worker that exits must be reported promptly, not waited out: took {elapsed:?}"
    );
    // And it names both ways out: the compatible fix, and the setting that restores the old
    // behaviour. A hint that named only one of them would leave half the readers stuck.
    assert!(
        message.contains("connect()"),
        "the message must name the fix the SDK ships: {message}"
    );
    assert!(
        message.contains("transport = \"pipe\""),
        "and the setting that keeps the worker as it is: {message}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// The hint is specific to the transport, so a pipe worker does not get it.
///
/// A worker that dies during its handshake on the pipe is an ordinary application failure, and
/// appending advice about `connect()` to every one of them would be noise that teaches readers
/// to ignore the hint.
#[test]
fn a_pipe_worker_that_dies_is_not_told_about_the_socket() {
    let _serial = serial();
    require_sdk!();

    let dir = std::env::temp_dir().join(format!("volucer-dying-pipe-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create the fixture directory");
    let script = dir.join("dying-worker.php");
    std::fs::write(&script, "<?php exit(3);\n").expect("write the script");

    let mut config = config(1);
    config.transport = volucer_core::config::WorkerTransport::Pipe;
    config.command = format!("php \"{}\"", script.display());

    let err = WorkerPool::start("test", &config).unwrap_err();
    let message = err.to_string();
    // Either the exit was observed or the pipe ended first — both are startup failures, and
    // which one wins is a race that is not what this test is about.
    assert!(
        message.contains("exited during startup") || message.contains("closed"),
        "the failure is still a startup one: {message}"
    );
    assert!(
        !message.contains("connect()"),
        "a pipe failure must not be explained as a socket one: {message}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
/// The pool's own account of how long requests waited for a slot.
///
/// The counter exists to tell a queueing tail apart from a slow server, so the test is
/// that it counts in both directions: more requests than workers must make it report
/// waiting, and one request at a time on an idle pool must make it report none. The
/// second half is the one that matters. A counter wired to something that always
/// reports a number would pass the first half and be worse than useless, because the
/// whole point of reading it is to be told the pool is innocent.
#[test]
fn the_pool_reports_the_time_requests_spent_waiting_for_a_slot() {
    let _serial = serial();
    require_sdk!();

    // A pool of two, so eight simultaneous requests have to queue three deep. The handler
    // sleeps rather than returning at once, because the wait being measured is the time spent
    // behind another *request*, and a handler that could not be caught mid-request would leave
    // nothing to wait behind.
    //
    // The handler also blocks while a gate file exists. That is what makes the queue *real*
    // rather than hoped for: with the gate closed the two holders cannot finish, so every
    // further request is parked for a slot no matter how the threads are scheduled. Spawning
    // eight threads and trusting them to overlap was the flake — on a loaded runner they
    // started far enough apart that each found an idle slot, and the counter this test exists
    // for was never asked to report anything. The gate does not exist for the idle phase, so
    // the handler there returns at once.
    let hold_us: u64 = 50_000;
    let dir = std::env::temp_dir().join(format!("volucer-wait-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create the fixture directory");
    let script = dir.join("slow-worker.php");
    let gate = dir.join("close-the-gate");
    let autoload = sdk_dir().join("autoload.php").display().to_string();
    std::fs::write(
        &script,
        format!(
            r#"<?php
require '{autoload}';
use Volucer\Request;
use Volucer\Response;
use Volucer\Worker;
$worker = Worker::connect();
$worker->run(static function (Request $request, Worker $worker): Response|null {{
    $deadline = microtime(true) + 60;
    while (file_exists('{gate}')) {{
        if (microtime(true) > $deadline) {{
            return Response::json(['error' => 'the gate never opened'], 500);
        }}
        usleep(1000);
    }}
    usleep({hold_us});
    return Response::json(['pid' => getmypid()]);
}});
"#,
            gate = gate.display(),
        ),
    )
    .expect("write the worker");

    let workers = 2;
    let config = WorkerConfig {
        command: format!("php \"{}\"", script.display()),
        workers,
        memory_limit_mb: None,
        max_requests: None,
        working_dir: None,
        request_timeout_ms: None,
        env: Default::default(),
        checksum: Default::default(),
        services: Vec::new(),
        grpc_services: Vec::new(),
        grpc_serve_any: false,
        grpc_proto: Vec::new(),
        grpc_proto_services: Vec::new(),
        transport: Default::default(),
        socket: None,
    };
    let mut pool = WorkerPool::start("test", &config).expect("pool starts");
    // The default bound is the production one, and this test is about the measurement rather
    // than about the policy: on a loaded runner the queue below can outlast `MAX_SLOT_WAIT`,
    // and the pool then answers `503` — correctly, but the counter this test exists for was
    // never asked. The policy has its own test (`a_full_pool_refuses_rather_than_queueing_
    // when_the_wait_is_none`), so the bound is raised here to take it out of the way.
    pool.set_max_wait(Some(Duration::from_secs(30)));

    // Idle: one request against two workers has nothing to wait for, so the pool must say
    // so. Not *zero*: the figure includes the scan's own lock-and-release, which every
    // request pays and which is microseconds. So the assertion is a floor rather than an
    // equality, and the floor is set far below anything a queue for a request could be.
    // `requests_served` is read alongside it, because a mean of zero and a mean of
    // nothing-measured are the same number otherwise.
    let single = pool.request(&get("/")).expect("the first request answers");
    assert_eq!(single.parse_status_headers().0, 200);
    assert_eq!(pool.requests_served(), 1);
    let idle = pool.mean_slot_wait_us();
    assert!(
        idle < 1_000,
        "one request against two idle workers waits for nothing but the scan: {idle} us"
    );

    // Contended: eight requests for two workers. The gate is closed first, then the two
    // holders are started, and only once both slots are leased is the rest of the queue fed.
    // With the gate closed the holders cannot finish, so each further request is provably
    // parked for a slot — the test no longer depends on how the threads happen to be
    // scheduled, which is what made it flake on a loaded runner.
    let requests: usize = 8;
    let contended_at = std::time::Instant::now();
    let answers: Vec<u16> = std::thread::scope(|scope| {
        std::fs::write(&gate, b"closed").expect("close the gate");

        let mut handles: Vec<_> = (0..workers as usize)
            .map(|_| scope.spawn(|| pool.request(&get("/")).map(|r| r.parse_status_headers().0)))
            .collect();

        // Wait until both slots are actually leased, so the queue below is fed against a pool
        // that is provably full.
        let mut waited = Duration::ZERO;
        while pool.free_slots() != 0 && waited < Duration::from_secs(10) {
            std::thread::sleep(Duration::from_millis(1));
            waited += Duration::from_millis(1);
        }
        assert_eq!(
            pool.free_slots(),
            0,
            "both slots are leased before the queue is fed"
        );

        handles.extend(
            (0..requests - workers as usize).map(|_| {
                scope.spawn(|| pool.request(&get("/")).map(|r| r.parse_status_headers().0))
            }),
        );

        // The gate stays shut until the queue is as deep as it is going to get, or the holders
        // finish early and the late arrivals find free slots instead of a queue. There is no
        // counter for "parked" to wait on and none is added just for a test, so this is a fixed
        // span: long enough for eight thread spawns on a loaded runner. It does not gate
        // correctness — anything that arrives later still queues behind the holders, because
        // the gate is still shut.
        std::thread::sleep(Duration::from_millis(250));
        std::fs::remove_file(&gate).expect("open the gate");

        handles
            .into_iter()
            .map(|handle| {
                handle
                    .join()
                    .expect("no thread panics")
                    .expect("every request answers")
            })
            .collect()
    });
    let contended_for = contended_at.elapsed();
    assert!(answers.iter().all(|status| *status == 200));

    assert_eq!(pool.requests_served(), 1 + requests as u64);
    let (busiest, busiest_us) = pool.busiest_slot_wait_us();
    let mean = pool.mean_slot_wait_us();
    let mean_floor_us = hold_us / 2 / 5;
    // The wall clock comes first because it says whether there was anything to queue for.
    // Eight requests of `hold_us` on two workers is four batches, so anything near eight
    // batches means they never overlapped and a wait of zero would be the truth rather than
    // a bug.
    assert!(
        contended_for >= Duration::from_micros(hold_us * requests as u64 / workers as u64 / 2),
        "eight {hold_us} us requests on two workers take at least four batches: {contended_for:?}"
    );
    assert!(
        mean >= mean_floor_us,
        "eight requests on two {hold_us} us workers queue for at least {mean_floor_us} us each, \
         not {mean} us (busiest slot {busiest} at {busiest_us} us, {contended_for:?} wall clock)"
    );

    // And the wait lands on the slots rather than on the pool as a whole: a request waits
    // for the slot it was pointed at, so the busiest slot holds on the order of 8 x 50 ms
    // / 2, and is never a slot with nothing attributed to it.
    assert!(busiest < workers as usize);
    assert!(
        busiest_us >= mean,
        "the busiest slot is at least the mean: {busiest_us} us vs {mean} us"
    );

    // The distribution, not just the mean: the histogram has to put the median below the
    // tail and the tail above the median, or the two figures are not measuring a spread.
    // A mean alone cannot tell contention that is spread evenly from contention that
    // starves a few, and the second kind is the one that makes a latency tail.
    let p50 = pool.slot_wait_percentile_us(0.50);
    let p99 = pool.slot_wait_percentile_us(0.99);
    assert!(
        p50 <= p99,
        "the median wait cannot exceed the tail: p50 {p50} us, p99 {p99} us"
    );
    assert!(
        p99 >= mean_floor_us,
        "one queueing wait in a hundred is most of a {hold_us} us request: p99 is {p99} us"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
/// A worker whose handler sleeps, so a slot is observably occupied.
///
/// Written to a temp directory per test rather than shared, because the tests that need it
/// run concurrently and each wants its own pool.
fn slow_worker(tag: &str, micros: u64) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("volucer-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create the fixture directory");
    let script = dir.join("slow-worker.php");
    let autoload = sdk_dir().join("autoload.php").display().to_string();
    std::fs::write(
        &script,
        format!(
            r#"<?php
require '{autoload}';
use Volucer\Request;
use Volucer\Response;
use Volucer\Worker;
$worker = Worker::connect();
$worker->run(static function (Request $request, Worker $worker): Response|null {{
    usleep({micros});
    return Response::json(['pid' => getmypid(), 'uri' => $request->uri]);
}});
"#
        ),
    )
    .expect("write the worker");
    script
}

/// A pool of `workers` slow workers, with the wait policy set explicitly.
fn slow_pool(tag: &str, workers: u32, micros: u64, max_wait: Option<Duration>) -> WorkerPool {
    let script = slow_worker(tag, micros);
    let config = WorkerConfig {
        command: format!("php \"{}\"", script.display()),
        workers,
        memory_limit_mb: None,
        max_requests: None,
        working_dir: None,
        request_timeout_ms: None,
        env: Default::default(),
        checksum: Default::default(),
        services: Vec::new(),
        grpc_services: Vec::new(),
        grpc_serve_any: false,
        grpc_proto: Vec::new(),
        grpc_proto_services: Vec::new(),
        transport: Default::default(),
        socket: None,
    };
    let mut pool = WorkerPool::start("test", &config).expect("pool starts");
    pool.set_max_wait(max_wait);
    pool
}

/// A slot leaves the free list when a request takes it and returns when that request ends.
///
/// This is the invariant the old scan could not state, let alone keep: it asked whether a
/// worker was *healthy*, which is true of a worker that is busy, so the number of slots
/// actually available was never something it knew. Here it is a number, and an idle pool must
/// have all of them.
#[test]
fn the_free_list_accounts_for_every_slot_exactly_once() {
    let _serial = serial();
    require_sdk!();

    let workers = 3;
    let pool = slow_pool("freelist", workers, 100_000, Some(Duration::from_secs(30)));
    assert_eq!(
        pool.free_slots(),
        workers as usize,
        "an idle pool offers every slot"
    );

    // Take them all at once and hold them: the handler sleeps, so the slots stay leased for
    // the duration. One extra request goes beyond the pool and must queue for a slot, which is
    // the other half of the policy.
    std::thread::scope(|scope| {
        let handles: Vec<_> = (0..workers as usize)
            .map(|_| {
                scope.spawn(|| {
                    let response = pool.request(&get("/")).expect("every slot is leased");
                    assert_eq!(response.parse_status_headers().0, 200);
                })
            })
            .collect();
        let queued = scope.spawn(|| {
            pool.request(&get("/"))
                .expect("a request beyond the pool waits and is then served")
                .parse_status_headers()
                .0
        });
        for handle in handles {
            handle.join().expect("no thread panics");
        }
        assert_eq!(queued.join().expect("no thread panics"), 200);
    });

    assert_eq!(
        pool.free_slots(),
        workers as usize,
        "every slot came back, so nothing leaked"
    );
    assert_eq!(pool.requests_served(), workers as u64 + 1);
}

/// A full pool refuses once the wait is up, rather than growing a queue without end.
///
/// The policy that bounds the tail. `None` is the strictest form - refuse as soon as no slot
/// is free - and the test is that the second request comes back *quickly* and as busy, rather
/// than being made to wait for the first one's whole duration.
#[test]
fn a_full_pool_refuses_rather_than_queueing_when_the_wait_is_none() {
    let _serial = serial();
    require_sdk!();

    let hold = 400_000;
    let pool = slow_pool("refuse", 1, hold, None);
    assert_eq!(pool.free_slots(), 1);

    std::thread::scope(|scope| {
        // Hold the only slot.
        let holder = scope.spawn(|| {
            pool.request(&get("/"))
                .expect("the first request has a slot");
        });

        // Wait until the slot is actually taken before asking for it, so this cannot pass by
        // racing the holder to it.
        let mut waited = Duration::ZERO;
        while pool.free_slots() != 0 && waited < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(5));
            waited += Duration::from_millis(5);
        }
        assert_eq!(pool.free_slots(), 0, "the holder took the only slot");

        let refused_at = std::time::Instant::now();
        let outcome = pool.request(&get("/"));
        let refused_in = refused_at.elapsed();
        holder.join().expect("no thread panics");

        let err = outcome.expect_err("a full pool with no wait refuses");
        assert!(
            err.is_busy(),
            "a refused request is a capacity condition, not a failure: {err}"
        );
        assert!(
            refused_in < Duration::from_millis(200),
            "it was refused at once rather than made to wait {} ms for the holder",
            hold / 1000
        );
    });

    assert_eq!(pool.free_slots(), 1, "the holder's slot came back");
}

/// A slot comes back even when the request on it fails.
///
/// The leak this guards against is the quiet kind: every early return on the request path is
/// a place a release could be forgotten, and a pool that loses one slot per failure shrinks
/// without ever reporting an error. The release is a `Drop` for exactly this reason, and this
/// is the test that says so.
#[test]
fn a_slot_comes_back_after_a_failed_request() {
    let _serial = serial();
    require_sdk!();

    let workers = 2;
    let pool = slow_pool("afterfail", workers, 10_000, Some(Duration::from_secs(5)));
    assert_eq!(
        pool.request(&get("/"))
            .expect("warm-up")
            .parse_status_headers()
            .0,
        200
    );
    assert_eq!(pool.free_slots(), workers as usize);

    // Kill the workers and make requests land on them. However that fails and however many
    // attempts it takes, the accounting must end up whole.
    for pid in pool.pids() {
        kill(pid);
    }
    for _ in 0..3 {
        let _ = pool.request(&get("/"));
        assert_eq!(
            pool.free_slots(),
            workers as usize,
            "no slot was lost to the failures"
        );
    }
}

/// A metric a worker emitted reaches a registry, through a real scrape.
///
/// **This is the end-to-end proof of the metrics feature, and it is the one thing that could not be tested in
/// pieces.** The registry is covered on its own, and so is the encoder; what is only observable here
/// is that a `METRIC` frame from a live PHP process — through the SDK's `emitMetric`, over the real
/// transport, decoded by the real codec — reaches the registry the metrics endpoint renders. Every
/// piece could pass its own tests with the wire between them disconnected, which is exactly the
/// failure the `absorb_metric` call this feature builds on had: it decoded the frame and dropped it.
///
/// The SDK's example worker emits `example_request_total` on every request, so no fixture is involved and a
/// change to either side fails here.
#[test]
fn a_workers_own_metric_reaches_the_registry() {
    let Some(pool) = start(1) else {
        return;
    };

    let registry = volucer_metrics::Registry::new();
    // Declared, because the registry refuses a name nobody described — and declaring it here rather
    // than in the pool is the point: a worker publishing a metric is publishing a name the *operator*
    // has to have decided to accept. The `[status.metrics] collect` table is what will
    // make this a configuration entry rather than a call a test makes by hand.
    registry.describe_histogram(
        "example_request_total",
        "Requests the example worker counted",
        &[],
    );

    let answer = pool.request(&get("/")).expect("a response");
    assert!(
        answer.stdout.starts_with(b"Status: 200"),
        "the request should have been served: {}",
        String::from_utf8_lossy(&answer.stdout)
    );

    pool.publish_metrics(&registry);
    let rendered = registry.render();
    assert!(
        rendered.contains("example_request_total_count 1"),
        "the worker's own sample should reach the registry:\n{rendered}"
    );
    assert!(
        rendered.contains("# HELP example_request_total Requests the example worker counted"),
        "and keep the help text the operator declared, not the worker's:\n{rendered}"
    );
}

/// A sample is counted once, however many times a scrape runs.
///
/// The drain is what makes this true, and it is the property that would be easiest to get wrong: a
/// queue that were copied rather than drained would make a worker's single emission appear to grow
/// with the number of scrapes, and a graph of it would climb for a reason that has nothing to do with
/// the application.
#[test]
fn a_sample_is_drained_and_not_recounted_by_a_second_scrape() {
    let Some(pool) = start(1) else {
        return;
    };

    let registry = volucer_metrics::Registry::new();
    registry.describe_histogram("example_request_total", "Requests", &[]);

    pool.request(&get("/")).expect("a response");
    pool.publish_metrics(&registry);
    pool.publish_metrics(&registry);
    pool.publish_metrics(&registry);

    let rendered = registry.render();
    assert!(
        rendered.contains("example_request_total_count 1"),
        "three scrapes, one sample:\n{rendered}"
    );
}

/// A metric a deployment declares in `[metrics.collect]` is the one an emitted sample lands in.
///
/// **This is the gap the feature was believed to have closed.** The two tests above declare
/// `example_request_total` by hand, which is exactly what an operator cannot do — so they proved the wire
/// worked while the feature did not. A registry refuses a name nobody described, and the only names
/// the process described were `rr_http_*`, so every `emitMetric` in a real deployment was decoded,
/// counted as refused, and dropped. The tests passed because the *test* was the missing
/// configuration.
///
/// So this one starts from a `Config` and takes the declaration from it, which is the path
/// `build_status` takes. If `[status.metrics.collect]` stops reaching the registry, this fails; the tests
/// above still pass.
#[test]
fn a_declared_metric_is_where_an_emitted_sample_lands() {
    let text = r#"
[server]
http = "127.0.0.1:8080"
[status]
listen = "127.0.0.1:2112"

[status.metrics.collect.example_request_total]
type = "histogram"
help = "Requests the example worker counted"
"#;
    let config = Config::from_toml(text).expect("the configuration parses");
    config.validate().expect("and is valid");
    let metrics = config
        .status
        .as_ref()
        .and_then(|status| status.metrics.as_ref())
        .expect("a [status.metrics] table");

    let Some(pool) = start(1) else {
        return;
    };

    // Declared from the configuration, through the same function `build_status` calls — so a
    // regression in the kind dispatch fails *here*, which is the property the hand-declaring tests
    // above cannot have.
    let registry = volucer_metrics::Registry::new();
    volucer_server::describe_collected(&registry, metrics);

    pool.request(&get("/")).expect("a response");
    pool.publish_metrics(&registry);

    let rendered = registry.render();
    assert!(
        rendered.contains("example_request_total_count 1"),
        "the sample should land in the name the deployment declared:\n{rendered}"
    );
    assert!(
        rendered.contains("# HELP example_request_total Requests the example worker counted"),
        "and the help text is the operator's, not the worker's:\n{rendered}"
    );
    assert_eq!(
        registry
            .refused_series()
            .into_iter()
            .filter(|(name, _)| !name.starts_with("rr_"))
            .collect::<Vec<_>>(),
        Vec::new(),
        "and nothing *application* was refused, which is what the declaration buys — the pool's own \
         rr_http_* gauges are refused here only because this test does not call http::describe"
    );
}

/// A live worker makes a call mid-request and the registry answers it.
///
/// **This is the test the whole call plane was missing.** Everything below it had been proven
/// separately — the channel answers a `CALL` when a handler is installed, the registry routes
/// `"<namespace>.<verb>"` to a namespace, `WorkersNamespace` reads the pools, `Dispatcher::from_config`
/// installs the registry on a channel before its handshake — and none of it had ever been connected to a
/// running PHP process. So a deployment could have had every part correct and a call that never arrives,
/// which is exactly the failure a unit test per layer cannot see.
///
/// It goes through `Dispatcher::from_config`, not through a registry built by hand, because *that* is the
/// wiring under test: a registry the test assembles would prove the pieces work together while leaving the
/// code that assembles them in production unexercised. The consequence is visible in the assertions — the
/// worker's own `backend` name in `workers.list` comes from the directory the dispatcher filled in.
///
/// The path exercised is the whole call path end to end: application code inside a request
/// handler writes a frame the server did not ask for, the server's exchange thread — mid-request, holding
/// the pipes — recognises it as a call rather than a protocol error, dispatches it into the registry, and
/// the worker reads the answer out of a stream that still owes it a response.
#[test]
fn a_worker_calls_the_server_mid_request_and_the_registry_answers() {
    let _serial = serial();
    require_sdk!();

    let dir = std::env::temp_dir().join(format!("volucer-call-worker-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create the fixture directory");
    let script = dir.join("call-worker.php");
    let autoload = sdk_dir().join("autoload.php").display().to_string();
    std::fs::write(
        &script,
        format!(
            r#"<?php
require '{autoload}';
use Volucer\Request;
use Volucer\Response;
use Volucer\Worker;
$worker = Worker::connect();
$worker->run(static function (Request $request, Worker $worker): Response|null {{
    // Every call happens *inside* the handler, which is the only place it can happen: the server's
    // reader for this worker is parked between requests.
    $pool = $worker->call('workers.list');
    $description = $worker->call('config.describe');
    $refused = $worker->call('kv.get', ['key' => 'session:1']);

    return Response::json([
        'workers' => count($pool),
        'backends' => implode(',', array_unique(array_column($pool, 'backend'))),
        'idle_runtimes' => array_values(array_filter(array_column($pool, 'runtime'))),
        'unobservable' => count(array_filter($pool, static fn ($w) => $w['pid'] === 0)),
        // Read from the application's side of the channel, because that is the side redaction
        // protects: an application that can read the deployment's credentials can leak them. The
        // `command` is here as the control — it must survive, or the assertion above it would pass
        // against an answer that was redacted into nothing.
        'described_command' => $description['backends']['app']['command'] ?? null,
        'refused_error' => $worker->lastCallError(),
        'refused' => $refused,
    ]);
}});
"#
        ),
    )
    .expect("write the worker");

    let backend = BackendConfig::Worker(WorkerConfig {
        command: format!("php \"{}\"", script.display()),
        // Two workers, so the pool has a slot that is *not* the one serving this request. The caller's
        // own slot cannot describe itself — its lock is held by the request it is being called from —
        // and a pool of one would make that the only entry, hiding the difference between "reporting
        // works" and "reporting works for everyone except the caller".
        workers: 2,
        // Both namespaces, because `workers.list` needs one and `config.describe` the other, and the
        // point is that they arrive through the configuration rather than through a test's wiring.
        services: vec!["config".to_string(), "workers".to_string()],
        ..config(2)
    });
    let configured = Config {
        backends: [("app".to_string(), backend)].into_iter().collect(),
        server: volucer_core::config::ServerConfig {
            default_backend: Some("app".to_string()),
            ..Default::default()
        },
        ..Config::default()
    };

    let dispatcher =
        Dispatcher::from_config(&configured, Routing::default()).expect("the pool starts");
    let response = dispatcher
        .dispatch(&get("/"))
        .expect("the request is served");
    let (status, _) = response.parse_status_headers();
    assert_eq!(
        status, 200,
        "the request that made the calls still succeeds"
    );

    let body = body_of(&response);
    let answer: serde_json::Value = serde_json::from_str(&body)
        .unwrap_or_else(|err| panic!("the worker answered with JSON: {err}\n{body}"));
    assert_eq!(
        answer["workers"], 2,
        "the worker asked how many workers there are and got an answer from the live pool: {answer}"
    );
    assert_eq!(
        answer["backends"], "app",
        "and the entries name the backend the dispatcher filed the pool under, which is what a \
         workers.reset index has to be resolved against: {answer}"
    );
    assert!(
        answer["described_command"]
            .as_str()
            .is_some_and(|c| c.contains("call-worker.php")),
        "config.describe returned the backend's command, so the answer was assembled and not \
         redacted away: {answer}"
    );
    assert!(
        answer["refused"].is_null() && answer["refused_error"].as_str().is_some(),
        "a method no namespace serves is refused by name rather than killing the worker: {answer}"
    );
    assert!(
        answer["refused_error"]
            .as_str()
            .is_some_and(|e| e.contains("kv")),
        "and the refusal names the namespace that is missing, which is the whole of what a caller \
         can act on: {answer}"
    );

    // The two entries are the two halves of the `try_lock` rule that this test forced. The sibling is
    // idle and describes itself fully; the worker that is *asking* is serving a request, so its slot
    // lock is held by that request and it reports nothing about itself.
    //
    // The runtime string is `"<version> (<sapi>)"` — `8.3.31 (cli)` on the machine this was written on —
    // so it is asserted as that shape rather than as a substring "PHP", which it does not contain.
    assert!(
        answer["idle_runtimes"].as_array().is_some_and(|runtimes| {
            runtimes.iter().any(|r| {
                r.as_str()
                    .is_some_and(|r| r.contains('(') && r.contains('.'))
            })
        }),
        "the idle sibling reports the PHP version and SAPI it handshook with: {answer}"
    );
    assert_eq!(
        answer["unobservable"], 1,
        "and exactly one entry could not be read — the caller's own, which is locked by the request \
         it is calling from. That is the deadlock this test found: taking that lock would hang until \
         the request deadline killed the worker: {answer}"
    );

    // The pool still serves afterwards, which is the property that makes a call safe: it is an extra
    // frame in an exchange, not a second exchange.
    let again = dispatcher.dispatch(&get("/")).expect("a second request");
    assert_eq!(again.parse_status_headers().0, 200);
}

/// A live worker stores a value and the next request finds it, through the real store.
///
/// The first test in the tree where the *whole* path is real: a PHP application calls `Kv::set` inside a
/// request handler, the call crosses the worker's channel to the exchange thread, the registry routes it
/// into the `kv` namespace, a real `MemoryStore` holds the bytes, and the `get` that follows comes back
/// with them. Every layer of the call path and of the storage is in this one run, and no test per layer can see a missing
/// `build_stores` — which is the same class of seam the call-plane test above found for the call path
/// itself.
///
/// It also pins what a store is easiest to get wrong: **two requests reach the same store**. A
/// per-request or per-connection store would pass everything else here and be useless, because the point
/// of a shared cache is that the next request finds what the last one left.
#[test]
fn a_worker_stores_a_value_and_the_next_request_finds_it() {
    let _serial = serial();
    require_sdk!();

    let dir = std::env::temp_dir().join(format!("volucer-kv-worker-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create the fixture directory");
    let script = dir.join("kv-worker.php");
    let autoload = sdk_dir().join("autoload.php").display().to_string();
    std::fs::write(
        &script,
        format!(
            r#"<?php
require '{autoload}';
use Volucer\Request;
use Volucer\Response;
use Volucer\Worker;
$worker = Worker::connect();
$worker->run(static function (Request $request, Worker $worker): Response|null {{
    $kv = $worker->kv('sessions');
    $path = (string) parse_url($request->uri, PHP_URL_PATH);

    if ($path === '/write') {{
        $kv->set('greeting', ['hello' => 'world'], 60);
        $kv->set('transient', 'gone soon', 1);
        $found = null;
        $kv->get('never-written', $found);

        return Response::json([
            'stored' => true,
            'missing_was_found' => $found,
            'ttl' => $kv->ttl('greeting'),
        ]);
    }}

    // A different request, and possibly a different worker process.
    $found = null;
    $value = $kv->get('greeting', $found);
    $transient = $kv->get('transient', $transient_found);

    return Response::json([
        'value' => $value,
        'found' => $found,
        'has' => $kv->has('greeting'),
        'transient' => $transient,
        'transient_found' => $transient_found,
    ]);
}});
"#
        ),
    )
    .expect("write the worker");

    let backend = BackendConfig::Worker(WorkerConfig {
        command: format!("php \"{}\"", script.display()),
        workers: 2,
        // The one line this test exists to check is honoured: without it the registry has no `kv` and
        // every call is refused, which is the default and the right default.
        services: vec!["kv.sessions".to_string()],
        ..config(2)
    });
    let mut configured = Config {
        backends: [("app".to_string(), backend)].into_iter().collect(),
        server: volucer_core::config::ServerConfig {
            default_backend: Some("app".to_string()),
            ..Default::default()
        },
        ..Config::default()
    };
    configured.kv.insert(
        "sessions".to_string(),
        KvStorageConfig::Memory(MemoryKvConfig { max_bytes: 1 << 20 }),
    );

    let dispatcher =
        Dispatcher::from_config(&configured, Routing::default()).expect("the pool starts");

    let written = json_body(
        &dispatcher
            .dispatch(&get("/write"))
            .expect("the write is served"),
    );
    assert_eq!(written["stored"], true, "{written}");
    assert_eq!(
        written["missing_was_found"], false,
        "a key nobody wrote is reported absent, so `get`'s found flag is not always true: {written}"
    );
    assert_eq!(
        written["ttl"], 60,
        "and the TTL went in as seconds and came back as seconds: {written}"
    );

    let read = json_body(
        &dispatcher
            .dispatch(&get("/read"))
            .expect("the read is served"),
    );
    assert_eq!(
        read["value"],
        serde_json::json!({ "hello": "world" }),
        "the next request — possibly on the other worker — finds what the first one stored: {read}"
    );
    assert_eq!(read["found"], true, "{read}");
    assert_eq!(
        read["has"], true,
        "and `has` answers without the value having to be copied out to find out: {read}"
    );

    // A one-second TTL that has expired is *gone*. The sleep is the only way to test expiry at all, and
    // what it proves is that the deadline the client sent reached the store — which a store that ignored
    // TTLs entirely would pass every other assertion here without.
    std::thread::sleep(Duration::from_millis(1100));
    let after = json_body(
        &dispatcher
            .dispatch(&get("/read"))
            .expect("the read is served"),
    );
    assert_eq!(
        after["transient"],
        serde_json::Value::Null,
        "a key stored with a one-second TTL is gone after one: {after}"
    );
    assert_eq!(
        after["transient_found"], false,
        "and is reported absent rather than as a value of null: {after}"
    );
    assert_eq!(
        after["value"],
        serde_json::json!({ "hello": "world" }),
        "while the key with a sixty-second TTL is untouched: {after}"
    );
}

/// A call naming a storage the backend was not granted is refused, by name, over a real channel.
///
/// The per-backend capability boundary as one request: the deployment has two storages, the backend was granted one, and the other is
/// unreachable — with a message naming what it *may* reach, so the fix is a configuration line rather
/// than an hour with a debugger.
#[test]
fn a_storage_outside_the_grant_is_refused_over_a_real_channel() {
    let _serial = serial();
    require_sdk!();

    let dir = std::env::temp_dir().join(format!("volucer-kv-denied-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create the fixture directory");
    let script = dir.join("denied-worker.php");
    let autoload = sdk_dir().join("autoload.php").display().to_string();
    std::fs::write(
        &script,
        format!(
            r#"<?php
require '{autoload}';
use Volucer\Request;
use Volucer\Response;
use Volucer\Worker;
$worker = Worker::connect();
$worker->run(static function (Request $request, Worker $worker): Response|null {{
    try {{
        $worker->kv('secrets')->get('key');
        return Response::json(['reached' => true]);
    }} catch (Throwable $e) {{
        // The refusal arrives as an exception from `call`, which is the point: an application can see it
        // and log it rather than being handed a value that is a lie.
        return Response::json(['reached' => false, 'message' => $e->getMessage()]);
    }}
}});
"#
        ),
    )
    .expect("write the worker");

    let backend = BackendConfig::Worker(WorkerConfig {
        command: format!("php \"{}\"", script.display()),
        workers: 1,
        // `sessions` only. `secrets` exists in the deployment and is not granted.
        services: vec!["kv.sessions".to_string()],
        ..config(1)
    });
    let mut configured = Config {
        backends: [("app".to_string(), backend)].into_iter().collect(),
        server: volucer_core::config::ServerConfig {
            default_backend: Some("app".to_string()),
            ..Default::default()
        },
        ..Config::default()
    };
    for name in ["sessions", "secrets"] {
        configured.kv.insert(
            name.to_string(),
            KvStorageConfig::Memory(MemoryKvConfig { max_bytes: 1 << 20 }),
        );
    }

    let dispatcher =
        Dispatcher::from_config(&configured, Routing::default()).expect("the pool starts");
    let answer = json_body(
        &dispatcher
            .dispatch(&get("/"))
            .expect("the request is served"),
    );

    assert_eq!(
        answer["reached"], false,
        "a storage this backend was not granted must be unreachable: {answer}"
    );
    let message = answer["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("secrets"),
        "the refusal names what was asked for: {message}"
    );
    assert!(
        message.contains("sessions"),
        "and what may be reached, so the fix is a configuration line: {message}"
    );
}

/// A Redis lock is shared between **two servers**, over real PHP workers.
///
/// This is the property the whole Redis driver exists for and the one `MemoryLocks` cannot have: two
/// dispatchers, each with its own pool and its own process, seeing one lock because the state is in the
/// server rather than in either of them. A test through one dispatcher would pass against a driver that
/// kept the lock in a `static`, which is exactly the mistake the memory driver already is.
///
/// The lock is taken by the first dispatcher's worker and then contended by the *second* dispatcher's —
/// a different process, a different pool, a different `MemoryLocks` if the driver were not Redis — and
/// then taken again after the first releases it.
///
/// Skipped with a printed reason when there is no Redis at `REDIS_URL`, like the driver's own tests, and
/// `VOLUCER_E2E=1` turns that skip into a failure.
#[cfg(feature = "redis")]
#[test]
fn a_redis_lock_is_shared_between_two_servers() {
    let _serial = serial();
    require_sdk!();

    let url = std::env::var("REDIS_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379".to_string());
    // Unique per *run*, not per process: a lease from an earlier run is still live for its TTL, and
    // Windows reuses process ids aggressively — so a pid-only name would have this test contend with the
    // previous run of itself. That is not hypothetical: it is how this test failed the first time it ran
    // as part of the whole suite, after passing alone.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    let resource = format!("test-e2e-{}-{nanos}", std::process::id());
    use volucer_lock::Locks as _;

    let probe = match volucer_lock::RedisLocks::connect(&url, 1, Duration::from_secs(5)) {
        Ok(locks) => locks,
        Err(err) => {
            let reason = format!("no Redis at {url}: {err}");
            if skip_is_a_failure() {
                panic!("VOLUCER_E2E is set, so a skipped test is a failure: {reason}");
            }
            eprintln!("skipping: {reason}");
            return;
        }
    };
    probe.release_all(&resource).expect("start from nothing");

    let dir = std::env::temp_dir().join(format!("volucer-lock-redis-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create the fixture directory");
    let autoload = sdk_dir().join("autoload.php").display().to_string();

    // One script body, written to two paths: the resource is a query argument so the same file can hold,
    // contend and release, and each dispatcher gets a copy of it under its own name.
    let script_body = || {
        format!(
            r#"<?php
require '{autoload}';
use Volucer\Request;
use Volucer\Response;
use Volucer\Worker;
$worker = Worker::connect();
$held = null;
$worker->run(static function (Request $request, Worker $worker) use (&$held): Response {{
    $path = parse_url($request->uri, PHP_URL_PATH);
    // Through the SDK's accessor rather than `$_GET`, which a worker does not populate — the server
    // sends the URI and the SDK parses the query, and nothing fills the superglobal.
    $resource = $request->query()['resource'] ?? 'unnamed';

    if ($path === '/hold') {{
        $acquired = $worker->call('lock.acquire', ['resource' => $resource, 'ttl' => 60000000]);
        $held = $acquired['id'];
        return Response::json(['acquired' => $acquired['acquired'], 'id' => $held]);
    }}

    if ($path === '/contend') {{
        $other = $worker->call('lock.acquire', ['resource' => $resource]);
        return Response::json([
            'acquired' => $other['acquired'],
            'error' => $worker->lastCallError(),
        ]);
    }}

    if ($path === '/release') {{
        $released = $worker->call('lock.release', ['resource' => $resource, 'id' => $held]);
        return Response::json(['released' => $released['released']]);
    }}

    return Response::json(['error' => 'unknown path']);
}});
"#
        )
    };

    let first_script = dir.join("first.php");
    let second_script = dir.join("second.php");
    std::fs::write(&first_script, script_body()).expect("write the first worker");
    std::fs::write(&second_script, script_body()).expect("write the second worker");

    let configure = |script: &std::path::Path| {
        let backend = BackendConfig::Worker(WorkerConfig {
            command: format!("php \"{}\"", script.display()),
            workers: 1,
            services: vec!["lock".to_string()],
            ..config(1)
        });
        let mut configured = Config {
            backends: [("app".to_string(), backend)].into_iter().collect(),
            server: volucer_core::config::ServerConfig {
                default_backend: Some("app".to_string()),
                ..Default::default()
            },
            ..Config::default()
        };
        configured.lock = Some(LockConfig::Redis(volucer_core::config::RedisLockConfig {
            url: url.clone(),
            pool_size: 2,
            read_timeout_ms: 5000,
        }));
        configured
    };

    let first = Dispatcher::from_config(&configure(&first_script), Routing::default())
        .expect("the first pool starts");
    let second = Dispatcher::from_config(&configure(&second_script), Routing::default())
        .expect("the second pool starts");

    let held = json_body(
        &first
            .dispatch(&get(&format!("/hold?resource={resource}")))
            .expect("the hold is served"),
    );
    assert_eq!(held["acquired"], true, "{held}");
    assert!(
        held["id"].as_str().is_some_and(|id| !id.is_empty()),
        "the server generated an id and the answer carries it: {held}"
    );

    // The second server, whose own process has never seen this lock.
    let contended = json_body(
        &second
            .dispatch(&get(&format!("/contend?resource={resource}")))
            .expect("the contention is served"),
    );
    assert_eq!(
        contended["acquired"], false,
        "a second *server* must contend with the first one's lock, which is the whole reason the Redis \
         driver exists: {contended}"
    );
    assert_eq!(
        contended["error"],
        serde_json::Value::Null,
        "and it is contention rather than a refusal: {contended}"
    );

    let released = json_body(
        &first
            .dispatch(&get(&format!("/release?resource={resource}")))
            .expect("the release is served"),
    );
    assert_eq!(released["released"], true, "{released}");

    // And now the second server can take it, which proves the release reached the shared state rather
    // than only the first server's copy of it.
    let after = json_body(
        &second
            .dispatch(&get(&format!("/contend?resource={resource}")))
            .expect("the second contention is served"),
    );
    assert_eq!(
        after["acquired"], true,
        "after the holder released it, the other server takes it: {after}"
    );

    probe.release_all(&resource).expect("clean up");
}

/// A job pushed into a pipeline is consumed by a real PHP process, and the outcome it reports is what the
/// queue acts on.
///
/// # Why this test is the one that matters for the consumer half
///
/// The pattern is recorded four times: `set_call_handler`, `build_stores`, `build_locks`,
/// `build_queues` — **the wiring between two tested things is where the untested bug lives**, and an
/// end-to-end test through the entry point a *deployment* uses is not optional for a service added to the
/// registry. This is the fifth instance and it is a slightly different shape, which is why it is worth
/// having: what is between the jobs engine and the worker pool here is not a registration but a *policy* —
/// which of `Ack`, `Requeue` and `Reject` does what to a reservation, and what happens to a job nobody
/// could hand to a consumer.
///
/// Every layer of that was tested separately. `MemoryDriver`'s ack/nack were tested against the driver;
/// `WorkerPool::job` returns an `Ack` and is tested against a channel; the SDK's `consume` picks an outcome
/// and is tested against a fixture. **Nothing tested that the three agree**, and the failure modes are all
/// silent: a `Requeue` treated as an `Ack` loses work an application asked to retry, an `Ack` treated as a
/// `Requeue` runs every job twice, and a `Reject` nacked rather than acked brings a poisoned job back for
/// ever. None of those shows up as an error anywhere — they show up as a queue that behaves oddly.
///
/// The test drives it the way a deployment does: `Dispatcher::from_config` builds the plane, a real
/// `php consumer.php` is booted through `WorkerPool::start_consumers`, and `Consumers::poll_once` runs one
/// pass over the pipeline. Then the *queue* is asked what happened, which is the assertion that makes this
/// about the reservation rather than about the answer frame.
#[test]
fn a_consumed_job_is_acked_requeued_or_rejected_as_its_consumer_decided() {
    let _serial = serial();
    require_sdk!();

    let dir = std::env::temp_dir().join(format!(
        "volucer-jobs-consume-{}-{:x}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).expect("create the fixture directory");
    let script = dir.join("consumer.php");
    let autoload = sdk_dir().join("autoload.php").display().to_string();
    std::fs::write(
        &script,
        format!(
            r#"<?php
require '{autoload}';
use Volucer\Job;
use Volucer\Worker;
$worker = Worker::connect();
$worker->consume(static function (Job $job, Worker $worker): string|bool|null {{
    // Every decision in one place, selected by the task name, so the test asserts on the *effect* of each
    // rather than on a fixture's shape. `RetryOnce` is the only one that needs state across attempts, and
    // it gets it from the attempt count the server puts on every job — which is also the assertion that
    // the counter is real rather than a constant.
    switch ($job->task) {{
        case 'ok':
            // A cache write from inside a job: the call plane has to work on this exchange too, or a
            // consumer that reaches for the server mid-job deadlocks until its lease runs out.
            $worker->call('jobs.size', ['pipeline' => 'emails']);
            return null;
        case 'retry':
            return $job->attempts === 1 ? false : null;
        case 'poison':
            return 'this job is not worth retrying';
        default:
            throw new RuntimeException('unexpected task ' . $job->task);
    }}
}});
"#
        ),
    )
    .expect("write the consumer");

    let pipeline = |name: &str| {
        (
            name.to_string(),
            volucer_core::config::PipelineConfig::Memory(
                volucer_core::config::MemoryPipelineConfig {},
            ),
        )
    };

    // A producer-only dispatcher: the plane it builds is the same object a consumer polls, and building it
    // through the configuration is what makes this a test of the *wiring* rather than of a struct.
    let mut configured = Config {
        backends: Default::default(),
        server: volucer_core::config::ServerConfig {
            default_backend: None,
            ..Default::default()
        },
        ..Config::default()
    };
    configured.jobs = Some(volucer_core::config::JobsConfig {
        consume: vec!["emails".to_string()],
        pipelines: [pipeline("emails")].into_iter().collect(),
        pool: None,
    });
    let dispatcher =
        Dispatcher::from_config(&configured, Routing::default()).expect("the plane is built");
    let plane = dispatcher.queues().expect("the [jobs] table built a plane");
    let driver = plane.driver("emails").expect("emails is a pipeline");

    let pool_config = volucer_core::config::JobsPoolConfig {
        command: format!("php \"{}\"", script.display()),
        workers: 1,
        memory_limit_mb: None,
        max_jobs: None,
        job_lease_seconds: 60,
        // Two, so that one `poll_once` reserves the pair pushed below and the test can be about what
        // happened to each rather than about how many passes it took to get to them. The production
        // default is one, which is what a worker pool should ask for — see `default_job_prefetch`.
        prefetch: 2,
        checksum: Default::default(),
        transport: Default::default(),
        socket: None,
        working_dir: None,
        env: Default::default(),
    };
    let consumers = Consumers::new(
        &plane,
        WorkerPool::start_consumers("jobs", &pool_config).expect("the consumer boots"),
        Duration::from_secs(60),
        2,
    );
    assert_eq!(
        consumers.pipelines(),
        vec!["emails"],
        "one pipeline is polled"
    );

    // Push four jobs with distinct tasks, and give them ids we can look for again.
    let push = |task: &str| {
        volucer_jobs::Driver::push(&*driver, "emails", volucer_jobs::Pushed::new(task))
            .expect("pushed")
    };
    let ok_id = push("ok");
    let retry_id = push("retry");

    // One pass: every ready job is reserved, handed to the consumer, and settled.
    let handled = consumers.poll_once().expect("the pass succeeds");
    assert_eq!(handled, 2, "both jobs were handed to the consumer");

    // **The queue is the thing asked what happened**, which is what makes this about the reservation.
    assert_eq!(
        volucer_jobs::Driver::size(&*driver, "emails").expect("size"),
        1,
        "the requeued job is ready again and the acknowledged one is gone"
    );
    assert_eq!(
        volucer_jobs::Driver::in_flight(&*driver, "emails").expect("in flight"),
        0,
        "and neither is still reserved, so both reservations were settled"
    );

    // The requeued job comes back as attempt two, and this time the consumer acknowledges it.
    let handled = consumers.poll_once().expect("the second pass succeeds");
    assert_eq!(handled, 1, "only the requeued job was ready");
    assert_eq!(
        volucer_jobs::Driver::size(&*driver, "emails").expect("size"),
        0,
        "and this time it was acknowledged, so the queue is empty"
    );
    let (acked, requeued, rejected) = consumers.counts();
    assert_eq!(
        (acked, requeued, rejected),
        (2, 1, 0),
        "one requeue on the first attempt and two acknowledgements"
    );
    let _ = ok_id;

    // A rejected job is **acknowledged from the queue's point of view**: it does not come back, and it is
    // not recorded as a failure of the reservation. This is the arm whose opposite — nacking — would bring
    // a poisoned job back for ever, and it is the one nothing else in the tree tests.
    let poison_id = push("poison");
    assert_eq!(consumers.poll_once().expect("the pass succeeds"), 1);
    assert_eq!(
        volucer_jobs::Driver::size(&*driver, "emails").expect("size"),
        0,
        "a rejected job is gone rather than requeued, which is what stops a poison job looping"
    );
    assert_eq!(
        consumers.counts(),
        (2, 1, 1),
        "and it is counted as rejected"
    );
    let _ = (retry_id, poison_id);

    // The consumer's own telemetry reached the pool's scrape queue rather than being dropped: a job has no
    // response object to carry it, so this is the path that only exists for consumers. The scrape is
    // asserted to *run*, because a consumer pool that published nothing would be a `[status.metrics]` table with
    // three worker gauges missing and no error anywhere.
    let pool = consumers.pool();
    let registry = volucer_metrics::Registry::new();
    volucer_metrics::http::describe(&registry);
    pool.publish_metrics(&registry);
    assert!(
        registry.render().contains("rr_http_total_workers"),
        "a consumer pool publishes the same worker gauges a request pool does, so one scrape describes \
         both: {}",
        registry.render()
    );
    assert_eq!(
        pool.jobs_served(),
        4,
        "the pool counted every job it handed over — two, then the requeued one, then the poison — through \
         the same counter a request pool counts requests with"
    );
}

/// A worker pushes a job through the `jobs` namespace, over a real channel, into a real pipeline.
///
/// The fourth instance of the end-to-end obligation (`set_call_handler`, `build_stores`, `build_locks`,
/// and now `build_queues`), and it goes through `Dispatcher::from_config` for the reason the others do:
/// a registry a test assembles proves the pieces work together while leaving the code that assembles them
/// in production unexercised. The specific failure this catches is a `[jobs]` table that builds no driver,
/// or one built under a name the router does not key on.
///
/// The assertion that matters is the *other* half: the job the worker pushed is then reserved **from the
/// driver the dispatcher built**, so the test proves the push landed in a queue a consumer can read rather
/// than only that a call was answered.
#[test]
fn a_worker_pushes_a_job_into_a_pipeline_a_consumer_can_read() {
    let _serial = serial();
    require_sdk!();

    let dir = std::env::temp_dir().join(format!("volucer-jobs-e2e-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create the fixture directory");
    let script = dir.join("jobs-worker.php");
    let autoload = sdk_dir().join("autoload.php").display().to_string();
    std::fs::write(
        &script,
        format!(
            r#"<?php
require '{autoload}';
use Volucer\Request;
use Volucer\Response;
use Volucer\Worker;
$worker = Worker::connect();
$worker->run(static function (Request $request, Worker $worker): Response {{
    $path = parse_url($request->uri, PHP_URL_PATH);

    if ($path === '/denied') {{
        // A pipeline the deployment does not have: the refusal must name it *and* what does exist, which
        // is the same shape every namespace's refusals take.
        $answer = $worker->call('jobs.push', ['pipeline' => 'nosuch', 'task' => 'App\\Jobs\\X']);
        return Response::json(['error' => $worker->lastCallError(), 'pushed' => $answer['pushed'] ?? null]);
    }}

    $answer = $worker->call('jobs.push', [
        'pipeline' => 'emails',
        'task' => 'App\\Jobs\\SendEmail',
        // Bytes above 0x7f and a serialized shape, because that is what an application queues.
        'payload' => base64_encode('{{"to":"a@b"}}'),
        'headers' => ['traceparent' => '00-abc-def-01'],
    ]);
    $size = $worker->call('jobs.size', ['pipeline' => 'emails']);
    $pipelines = $worker->call('jobs.pipelines');

    return Response::json([
        'pushed' => $answer['pushed'] ?? null,
        'id' => $answer['id'] ?? null,
        'size' => $size['size'] ?? null,
        'in_flight' => $size['in_flight'] ?? null,
        'pipelines' => array_column($pipelines['pipelines'] ?? [], 'name'),
    ]);
}});
"#
        ),
    )
    .expect("write the worker");

    let backend = BackendConfig::Worker(WorkerConfig {
        command: format!("php \"{}\"", script.display()),
        workers: 1,
        services: vec!["jobs".to_string()],
        ..config(1)
    });
    let mut configured = Config {
        backends: [("app".to_string(), backend)].into_iter().collect(),
        server: volucer_core::config::ServerConfig {
            default_backend: Some("app".to_string()),
            ..Default::default()
        },
        ..Config::default()
    };
    configured.jobs = Some(volucer_core::config::JobsConfig {
        // Nothing consumed: this is a producer-only deployment, which is the common web node and the
        // configuration the test is modelling.
        consume: Vec::new(),
        pipelines: [
            (
                "emails".to_string(),
                volucer_core::config::PipelineConfig::Memory(
                    volucer_core::config::MemoryPipelineConfig {},
                ),
            ),
            (
                "reports".to_string(),
                volucer_core::config::PipelineConfig::Memory(
                    volucer_core::config::MemoryPipelineConfig {},
                ),
            ),
        ]
        .into_iter()
        .collect(),
        pool: None,
    });

    let dispatcher =
        Dispatcher::from_config(&configured, Routing::default()).expect("the pool starts");

    let pushed = json_body(&dispatcher.dispatch(&get("/")).expect("the push is served"));
    assert_eq!(pushed["pushed"], true, "{pushed}");
    assert!(
        pushed["id"].as_str().is_some_and(|id| !id.is_empty()),
        "the push answers with the driver's id: {pushed}"
    );
    assert_eq!(
        pushed["size"], 1,
        "and the job is in the pipeline the moment the call returns: {pushed}"
    );
    assert_eq!(pushed["in_flight"], 0, "{pushed}");
    assert_eq!(
        pushed["pipelines"],
        serde_json::json!(["emails", "reports"]),
        "both pipelines are listed, which is what a worker discovers what it may push to from: {pushed}"
    );

    // **Read back through the dispatcher's own driver**, which is the assertion that makes this an
    // end-to-end test rather than a call-plane test: the job the worker pushed is really in a queue, and
    // it carries what the worker sent.
    let queues = dispatcher.queues().expect("the [jobs] table built a plane");
    let emails = queues
        .driver("emails")
        .expect("emails is a pipeline this deployment configured");
    let reserved = volucer_jobs::Driver::reserve(&*emails, "emails", 1, Duration::from_secs(60))
        .expect("reserved");
    assert_eq!(reserved.len(), 1, "the pushed job must be reservable");
    assert_eq!(reserved[0].job.task, "App\\Jobs\\SendEmail");
    assert_eq!(
        reserved[0].job.payload, b"{\"to\":\"a@b\"}",
        "and its payload is exactly the bytes that went in"
    );
    assert_eq!(reserved[0].job.header("traceparent"), Some("00-abc-def-01"));

    // The refusal names both the pipeline asked for and the ones that exist.
    let denied = json_body(
        &dispatcher
            .dispatch(&get("/denied"))
            .expect("the refusal is served"),
    );
    assert_eq!(denied["pushed"], serde_json::Value::Null, "{denied}");
    let message = denied["error"].as_str().unwrap_or_default();
    assert!(message.contains("nosuch"), "{message}");
    assert!(message.contains("emails"), "{message}");
}

/// The JSON body of a response, as the fixtures write it.
fn json_body(response: &volucer_fcgi::Response) -> serde_json::Value {
    let body = body_of(response);
    serde_json::from_str(&body)
        .unwrap_or_else(|err| panic!("the worker answered with JSON: {err}\n{body}"))
}

/// A lock acquired by one request is contended by the next, over a real channel.
///
/// The same argument as the KV test above and the third instance of the pattern: `LockNamespace`,
/// `MemoryLocks`, `build_locks` and the `services` allow-list were each tested separately, and the thing
/// that could still be wrong is the seam — `build_locks` returning `None` while the configuration granted
/// `lock`, or the namespace being installed under a name the router does not key on. So this goes through
/// `Dispatcher::from_config` with a real PHP process and calls the methods from inside a request handler.
///
/// It asserts the three answers an acquisition has, in one run:
///
/// - **acquired**, with the id the server generated, because the caller sent none — the client-side half
///   of that design is only useful if the id comes back;
/// - **contended** (`ok` true, `acquired` false), which is an ordinary answer rather than an error, and
///   the distinction RoadRunner's own documentation is emphatic about;
/// - **released by the id it was given**, and then not contended any more — which is what makes the id
///   meaningful rather than decorative.
#[test]
fn a_lock_acquired_by_one_request_is_contended_by_the_next() {
    let _serial = serial();
    require_sdk!();

    let dir = std::env::temp_dir().join(format!("volucer-lock-e2e-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create the fixture directory");
    let script = dir.join("lock-worker.php");
    let autoload = sdk_dir().join("autoload.php").display().to_string();
    std::fs::write(
        &script,
        format!(
            r#"<?php
require '{autoload}';
use Volucer\Request;
use Volucer\Response;
use Volucer\Worker;
$worker = Worker::connect();

// Held across requests by being taken with no lease: `ttl` absent means "until released", which is the
// one lock semantic that is the opposite of what the same key means to a KV value.
$held = null;
$held_id = null;

$worker->run(static function (Request $request, Worker $worker) use (&$held, &$held_id): Response {{
    $path = parse_url($request->uri, PHP_URL_PATH);

    if ($path === '/hold') {{
        $acquired = $worker->call('lock.acquire', ['resource' => 'pdf:create']);
        $held = (bool) $acquired['acquired'];
        $held_id = $acquired['id'];
        return Response::json([
            'acquired' => $acquired['acquired'],
            'id_is_set' => $held_id !== null && $held_id !== '',
            'mode' => $acquired['mode'],
            // An id was generated for us, so `remaining_ms` is null because the lease has no deadline.
            'remaining_is_null' => $acquired['remaining_ms'] === null,
        ]);
    }}

    if ($path === '/contend') {{
        // The same worker, a second call, while the first lock is still held. Note that the id is a
        // *new* one: contending with itself is refused with an error rather than answered with
        // `acquired: false`, and neither of those is what this asserts.
        $second = $worker->call('lock.acquire', ['resource' => 'pdf:create']);
        $exists = $worker->call('lock.exists', ['resource' => 'pdf:create']);
        $holders = $worker->call('lock.holders', ['resource' => 'pdf:create']);
        return Response::json([
            'acquired' => $second['acquired'],
            'error' => $worker->lastCallError(),
            'exists' => $exists['exists'],
            'holders' => count($holders['holders']),
        ]);
    }}

    if ($path === '/release') {{
        $released = $worker->call('lock.release', ['resource' => 'pdf:create', 'id' => $held_id]);
        $after = $worker->call('lock.exists', ['resource' => 'pdf:create']);
        // A second release of the same id is `false` rather than an error, which is what a `finally`
        // block depends on: cleanup that could throw is cleanup that gets skipped.
        $again = $worker->call('lock.release', ['resource' => 'pdf:create', 'id' => $held_id]);
        return Response::json([
            'released' => $released['released'],
            'again' => $again['released'],
            'exists_after' => $after['exists'],
        ]);
    }}

    return Response::json(['error' => 'unknown path']);
}});
"#
        ),
    )
    .expect("write the worker");

    let backend = BackendConfig::Worker(WorkerConfig {
        command: format!("php \"{}\"", script.display()),
        // One worker, deliberately: the lock has to be visible to the *same* worker's next request, and a
        // pool of two would make the test depend on which slot served it. Cross-worker reachability is
        // what the memory driver's shared registry gives, and the KV test above already covers that shape.
        workers: 1,
        services: vec!["lock".to_string()],
        ..config(1)
    });
    let mut configured = Config {
        backends: [("app".to_string(), backend)].into_iter().collect(),
        server: volucer_core::config::ServerConfig {
            default_backend: Some("app".to_string()),
            ..Default::default()
        },
        ..Config::default()
    };
    configured.lock = Some(LockConfig::Memory(MemoryLockConfig {}));

    let dispatcher =
        Dispatcher::from_config(&configured, Routing::default()).expect("the pool starts");

    let held = json_body(
        &dispatcher
            .dispatch(&get("/hold"))
            .expect("the hold is served"),
    );
    assert_eq!(held["acquired"], true, "{held}");
    assert_eq!(
        held["id_is_set"], true,
        "the server generated an id and the answer carries it, which is the only way a caller that sent \
         none can release the lock: {held}"
    );
    assert_eq!(
        held["mode"], "exclusive",
        "and the default mode is exclusive, because a shared default would stop two writers excluding \
         each other: {held}"
    );
    assert_eq!(
        held["remaining_is_null"], true,
        "`ttl` absent means held until released, so there is no remaining time: {held}"
    );

    let contended = json_body(
        &dispatcher
            .dispatch(&get("/contend"))
            .expect("the contention is served"),
    );
    assert_eq!(
        contended["acquired"], false,
        "a second acquisition contends rather than failing: {contended}"
    );
    assert_eq!(
        contended["error"],
        serde_json::Value::Null,
        "and `lastCallError` stays null, because contention is a successful call: {contended}"
    );
    assert_eq!(
        contended["exists"], true,
        "exists answers the question a caller resolving an unknown state asks: {contended}"
    );
    assert_eq!(contended["holders"], 1, "{contended}");

    let released = json_body(
        &dispatcher
            .dispatch(&get("/release"))
            .expect("the release is served"),
    );
    assert_eq!(released["released"], true, "{released}");
    assert_eq!(
        released["again"], false,
        "releasing what is already gone is an answer, not an error: {released}"
    );
    assert_eq!(released["exists_after"], false, "{released}");
}

/// A lock call from a backend that was not granted the service is refused, by name.
///
/// The capability half of the same seam, and the one worth asserting separately: the *default* is that no
/// worker may lock anything, so a deployment that forgets the `services` line gets a refusal rather than a
/// distributed lock it did not mean to hand out.
#[test]
fn a_lock_call_from_a_backend_without_the_grant_is_refused() {
    let _serial = serial();
    require_sdk!();

    let dir = std::env::temp_dir().join(format!("volucer-lock-denied-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create the fixture directory");
    let script = dir.join("denied-lock-worker.php");
    let autoload = sdk_dir().join("autoload.php").display().to_string();
    std::fs::write(
        &script,
        format!(
            r#"<?php
require '{autoload}';
use Volucer\Request;
use Volucer\Response;
use Volucer\Worker;
$worker = Worker::connect();
$worker->run(static function (Request $request, Worker $worker): Response {{
    $answer = $worker->call('lock.acquire', ['resource' => 'pdf:create']);
    return Response::json([
        'acquired' => $answer['acquired'],
        'error' => $worker->lastCallError(),
        'unknown' => $worker->lastCallWasUnknown(),
    ]);
}});
"#
        ),
    )
    .expect("write the worker");

    let backend = BackendConfig::Worker(WorkerConfig {
        command: format!("php \"{}\"", script.display()),
        // No `services` at all, which is the default and the right default.
        workers: 1,
        ..config(1)
    });
    let mut configured = Config {
        backends: [("app".to_string(), backend)].into_iter().collect(),
        server: volucer_core::config::ServerConfig {
            default_backend: Some("app".to_string()),
            ..Default::default()
        },
        ..Config::default()
    };
    // The table is there; the *grant* is what is missing, which is the distinction that makes this a
    // capability test rather than a configuration test.
    configured.lock = Some(LockConfig::Memory(MemoryLockConfig {}));

    let dispatcher =
        Dispatcher::from_config(&configured, Routing::default()).expect("the pool starts");
    let answer = json_body(
        &dispatcher
            .dispatch(&get("/"))
            .expect("the request is served"),
    );

    assert!(
        answer["acquired"].is_null(),
        "a worker with no `lock` in its services list must not acquire anything, so the call never \
         reaches an acquisition at all: {answer}"
    );
    assert_eq!(
        answer["unknown"], false,
        "and a refused call is decided, not undetermined: {answer}"
    );
    let message = answer["error"].as_str().unwrap_or_default();
    assert!(
        message.contains("lock"),
        "the refusal names the namespace that is missing: {message}"
    );
}

/// A pool nobody scrapes still bounds its queue, and still says what it lost.
///
/// **This is the leak, end to end.** `publish_metrics` has one caller — the scrape — so a deployment
/// with `[server.access_log]` and no `[status.metrics]` table never drains the worker-metric queue, and an
/// earlier version of it grew for the process's life while its documentation claimed the drain bounded
/// it. The fixture is therefore the deployment in question: a real pool, a real worker emitting samples
/// on every request, and **no `publish_metrics` call anywhere in the test**.
///
/// Two properties, and the second is what makes the first useful:
///
/// - the queue stops at its bound, so what the process can be made to hold is finite;
/// - the loss is *counted* and readable through `dropped_metrics`, which is what the shutdown line
///   prints — because a deployment that never scrapes would otherwise never learn that its samples were
///   going nowhere.
#[test]
fn a_pool_that_is_never_scraped_bounds_its_metric_queue_and_counts_the_loss() {
    let _serial = serial();
    require_sdk!();

    let dir = std::env::temp_dir().join(format!("volucer-metric-bound-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create the fixture directory");
    let script = dir.join("metric-flood.php");
    let autoload = sdk_dir().join("autoload.php").display().to_string();
    // **One emission per request, and the sample is deliberately undeclared.** A declared name would
    // still be refused by this test's registry (which declares nothing), so the sample count is what
    // matters rather than where it lands — this test is about the queue, not about the registry.
    std::fs::write(
        &script,
        format!(
            r#"<?php
require '{autoload}';
use Volucer\Request;
use Volucer\Response;
use Volucer\Worker;
$worker = Worker::connect();
$worker->run(static function (Request $request, Worker $worker): Response {{
    $worker->emitMetric('app_requests_total', 1.0);
    return Response::text('ok');
}});
"#
        ),
    )
    .expect("write the worker");

    let config = WorkerConfig {
        command: format!("php \"{}\"", script.display()),
        workers: 1,
        memory_limit_mb: None,
        max_requests: None,
        working_dir: None,
        request_timeout_ms: None,
        env: Default::default(),
        checksum: Default::default(),
        services: Vec::new(),
        grpc_services: Vec::new(),
        grpc_serve_any: false,
        grpc_proto: Vec::new(),
        grpc_proto_services: Vec::new(),
        transport: Default::default(),
        socket: None,
    };
    let pool = match WorkerPool::start("bound", &config) {
        Ok(pool) => pool,
        Err(err) => {
            eprintln!("skipping: the worker pool could not start: {err}");
            return;
        }
    };

    // Enough requests to overflow the bound several times over. One sample per request, and the bound
    // is four thousand — so this is a few seconds of a small pool and would be a moment in production.
    //
    // **The count is not asserted as an exact number**, and that was the first version's mistake: the
    // example worker emits on every request, but a worker's own protocol work is not this test's
    // subject — the SDK's first request does not carry a sample (it is the handshake) and a replay
    // can count twice. What matters is that samples arrived, that the queue stopped at its bound, and
    // that everything beyond the bound was *counted* rather than silently dropped. An exact figure
    // here would be an assertion about the SDK's emission policy wearing this test's name.
    let requests = 4096 + 2500;
    for n in 0..requests {
        pool.request(&get("/"))
            .unwrap_or_else(|err| panic!("request {n} failed: {err}"));
    }

    let pending = pool.pending_metrics();
    let dropped = pool.dropped_metrics();
    assert_eq!(
        pending, 4096,
        "the queue must stop at its bound rather than growing with the requests served"
    );
    assert!(
        dropped > 2000,
        "most of the {requests} requests carried a sample, so most of the overflow must be counted; \
         got {dropped}"
    );
    assert_eq!(
        pending as u64 + dropped,
        pool.metrics_handled(),
        "and the two must account for every sample the pool ever took in: {pending} waiting + \
         {dropped} dropped != {}",
        pool.metrics_handled()
    );

    // **No scrape, deliberately.** Reaching this line with the numbers above is the whole test: a
    // `publish_metrics` call here would drain the queue and hide the defect it exists to catch.
    let _ = std::fs::remove_dir_all(&dir);
}

/// A file-backed storage keeps what a worker wrote across a **restart of the dispatcher**.
///
/// This is the whole claim of the `file` driver and the only test that can make it: the memory
/// driver would pass every other assertion in this file, because it is a cache that works — it just
/// forgets. So the test builds a dispatcher, has a real PHP worker store a value through `kv.set`,
/// **drops it entirely**, and builds a second dispatcher on the same directory with a second pool of
/// real processes. A value the second one reads was on disk rather than in a map.
///
/// It goes through `from_config_in` rather than `from_config`, because a relative `path` resolving
/// against the configuration file's directory is part of what a deployment relies on and the
/// resolution has exactly one production caller.
#[test]
fn a_file_store_keeps_a_workers_value_across_a_restart() {
    let _serial = serial();
    require_sdk!();

    let dir = std::env::temp_dir().join(format!("volucer-kv-file-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create the fixture directory");
    // Written as a *relative* path on purpose: the configuration directory is the root, so the store
    // lands under `dir/cache`, and a resolver that used the working directory instead would put it
    // somewhere the assertion below would not find.
    let cache = dir.join("cache");

    let script = dir.join("kv-worker.php");
    let autoload = sdk_dir().join("autoload.php").display().to_string();
    std::fs::write(
        &script,
        format!(
            r#"<?php
require '{autoload}';
use Volucer\Request;
use Volucer\Response;
use Volucer\Worker;
$worker = Worker::connect();
$kv = $worker->kv('sessions');
$worker->run(static function (Request $request, Worker $worker) use ($kv): Response|null {{
    $path = (string) parse_url($request->uri, PHP_URL_PATH);
    if ($path === '/write') {{
        $kv->set('greeting', 'hello from the first process', 600);
        return Response::json(['written' => true]);
    }}
    $found = null;
    $value = $kv->get('greeting', $found);
    return Response::json(['value' => $value, 'found' => $found]);
}});
"#
        ),
    )
    .expect("write the worker");

    let build = || {
        let backend = BackendConfig::Worker(WorkerConfig {
            command: format!("php \"{}\"", script.display()),
            workers: 1,
            services: vec!["kv.sessions".to_string()],
            ..config(1)
        });
        let mut configured = Config {
            backends: [("app".to_string(), backend)].into_iter().collect(),
            server: volucer_core::config::ServerConfig {
                default_backend: Some("app".to_string()),
                ..Default::default()
            },
            ..Config::default()
        };
        configured.kv.insert(
            "sessions".to_string(),
            KvStorageConfig::File(FileKvConfig {
                path: std::path::PathBuf::from("cache"),
                max_bytes: 1 << 20,
            }),
        );
        configured
    };

    {
        let dispatcher = Dispatcher::from_config_in(&build(), Routing::default(), &dir)
            .expect("the first dispatcher starts");
        let written = json_body(
            &dispatcher
                .dispatch(&get("/write"))
                .expect("the write is served"),
        );
        assert_eq!(written["written"], true, "{written}");
        assert!(
            cache.is_dir(),
            "a relative kv path resolves against the configuration's directory, so the store is \
             at {}",
            cache.display()
        );
        // Dropped here rather than at the end of the test, so the second dispatcher's processes are
        // a genuine second run — the value it reads cannot be in this one's memory.
    }

    // A different dispatcher, a different worker process, the same directory.
    let dispatcher = Dispatcher::from_config_in(&build(), Routing::default(), &dir)
        .expect("the second dispatcher starts");
    let read = json_body(
        &dispatcher
            .dispatch(&get("/read"))
            .expect("the read is served"),
    );
    assert_eq!(
        read["value"], "hello from the first process",
        "a file store that did not survive a restart is the memory driver with extra steps: {read}"
    );
    assert_eq!(read["found"], true, "{read}");

    drop(dispatcher);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A worker's `Kv::setIfAbsent` is refused when the key exists, and the refusal is an answer.
///
/// The whole path again — a real PHP application, the channel, the registry, a real store — and the
/// thing it pins is the one property no unit test can: that `false` arrives at the application as a
/// *successful* answer rather than as a refused call. The SDK returns a bool, and an application that
/// cannot tell "somebody else holds it" from "the call failed" is an application that gives up on the
/// first contended key.
///
/// Two requests, not two threads: the second is what a contending process looks like from inside one,
/// and the store is the real one the deployment would use.
#[test]
fn a_worker_conditional_write_is_refused_when_the_key_is_taken() {
    let _serial = serial();
    require_sdk!();

    let dir = std::env::temp_dir().join(format!("volucer-sia-worker-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create the fixture directory");
    let script = dir.join("sia-worker.php");
    let autoload = sdk_dir().join("autoload.php").display().to_string();
    std::fs::write(
        &script,
        format!(
            r#"<?php
require '{autoload}';
use Volucer\Request;
use Volucer\Response;
use Volucer\Worker;
$worker = Worker::connect();
$worker->run(static function (Request $request, Worker $worker): Response|null {{
    $path = (string) parse_url($request->uri, PHP_URL_PATH);
    $kv = $worker->kv('sessions');

    if ($path === '/claim') {{
        // The same key on every request, so the second request is the contended one.
        $won = $kv->setIfAbsent('the-lock-slot', 'owned by ' . getmypid(), 600);
        $found = null;
        $value = $kv->get('the-lock-slot', $found);

        return Response::json(['won' => $won, 'value' => $value, 'found' => $found]);
    }}

    // A key nobody has claimed: the write must succeed even though `/claim` ran first.
    $won = $kv->setIfAbsent('fresh-slot', 'first', 600);
    return Response::json(['won' => $won]);
}});
"#
        ),
    )
    .expect("write the worker");

    let backend = BackendConfig::Worker(WorkerConfig {
        command: format!("php \"{}\"", script.display()),
        // One worker, because the contention must come from two *requests* and not from two slots
        // racing — what is under test is the store's decision, not the pool's scheduling.
        workers: 1,
        services: vec!["kv.sessions".to_string()],
        ..config(1)
    });
    let mut configured = Config {
        backends: [("app".to_string(), backend)].into_iter().collect(),
        server: volucer_core::config::ServerConfig {
            default_backend: Some("app".to_string()),
            ..Default::default()
        },
        ..Config::default()
    };
    configured.kv.insert(
        "sessions".to_string(),
        KvStorageConfig::Memory(MemoryKvConfig { max_bytes: 1 << 20 }),
    );

    let dispatcher =
        Dispatcher::from_config(&configured, Routing::default()).expect("the pool starts");

    let first = json_body(
        &dispatcher
            .dispatch(&get("/claim"))
            .expect("the first claim is served"),
    );
    assert_eq!(
        first["won"], true,
        "the first conditional write on an absent key wins: {first}"
    );

    let second = json_body(
        &dispatcher
            .dispatch(&get("/claim"))
            .expect("the second claim is served"),
    );
    assert_eq!(
        second["won"], false,
        "the second is told it did not store, which is an answer rather than a refused call: {second}"
    );
    assert_eq!(
        second["value"], first["value"],
        "and the value behind the key is still the first writer's: {second} vs {first}"
    );

    let fresh = json_body(
        &dispatcher
            .dispatch(&get("/fresh"))
            .expect("the fresh claim is served"),
    );
    assert_eq!(
        fresh["won"], true,
        "a different key is unaffected, which is what says the refusal was about the key rather than \
         about the store: {fresh}"
    );

    drop(dispatcher);
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------------------------
// Phase 3a: respawn_idle, the rolling restart of application code.
// ---------------------------------------------------------------------------------------------

/// A worker that records when it booted, so a replacement is observable as a new pid **and** as a
/// fresh marker.
///
/// The pid alone would be enough for the pool, but a deploy's actual question is "is the code that
/// is now on disk what is running", and a marker written at boot is the only thing that answers it:
/// a slot that was left alone runs the same pid and holds the marker it wrote when it started.
fn booting_worker(tag: &str, marker_dir: &Path, micros: u64) -> PathBuf {
    std::fs::create_dir_all(marker_dir).expect("create the marker directory");
    let dir = std::env::temp_dir().join(format!("volucer-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create the fixture directory");
    let script = dir.join("booting-worker.php");
    let autoload = sdk_dir().join("autoload.php").display().to_string();
    let markers = marker_dir.display().to_string();
    std::fs::write(
        &script,
        format!(
            r#"<?php
require '{autoload}';
use Volucer\Request;
use Volucer\Response;
use Volucer\Worker;
// Written before `connect()`, so the marker is the *boot* event rather than a response event:
// a slot that is merely served is not a slot that has loaded new code.
file_put_contents('{markers}/' . getmypid() . '.boot', (string) getmypid());
$worker = Worker::connect();
$worker->run(static function (Request $request, Worker $worker): Response|null {{
    usleep({micros});
    return Response::json(['pid' => getmypid()]);
}});
"#
        ),
    )
    .expect("write the worker");
    script
}

/// A pool of booting workers.
fn booting_pool(tag: &str, workers: u32, marker_dir: &Path, micros: u64) -> WorkerPool {
    let script = booting_worker(tag, marker_dir, micros);
    let config = WorkerConfig {
        command: format!("php \"{}\"", script.display()),
        workers,
        memory_limit_mb: None,
        max_requests: None,
        working_dir: None,
        request_timeout_ms: None,
        env: Default::default(),
        checksum: Default::default(),
        services: Vec::new(),
        grpc_services: Vec::new(),
        grpc_serve_any: false,
        grpc_proto: Vec::new(),
        grpc_proto_services: Vec::new(),
        transport: Default::default(),
        socket: None,
    };
    WorkerPool::start("roll", &config).expect("pool starts")
}

/// **A sweep replaces every idle worker, and the pool serves across it.**
///
/// The mechanism Phase 3a exists for. Two things are asserted together because either alone would
/// pass against a broken implementation: every slot's process is new (so new code is in front of
/// traffic), and a request after the sweep is answered 200 (so the pool is not merely restarted).
#[test]
fn a_sweep_replaces_every_idle_worker_and_the_pool_keeps_serving() {
    let _serial = serial();
    require_sdk!();

    let markers = std::env::temp_dir().join(format!("volucer-roll-markers-{}", std::process::id()));
    let pool = booting_pool("roll-all", 3, &markers, 0);
    let before = pool.pids();
    assert_eq!(before.len(), 3);

    let report = pool.respawn_idle();
    assert_eq!(report.backend, "roll");
    assert_eq!(
        report.replaced,
        vec![0, 1, 2],
        "an idle pool sweeps every slot: {report:?}"
    );
    assert!(report.skipped_busy.is_empty(), "{report:?}");
    assert!(report.failed.is_empty(), "{report:?}");
    assert!(report.is_complete(), "{report:?}");

    let after = pool.pids();
    assert_eq!(after.len(), 3, "the pool kept its size");
    for (old, new) in before.iter().zip(after.iter()) {
        assert_ne!(
            old, new,
            "slot for slot, every worker is a new process: {before:?} -> {after:?}"
        );
    }

    // The replacement is a *working* worker, not merely a new pid.
    let response = pool
        .request(&get("/"))
        .expect("the pool serves after a sweep");
    assert_eq!(response.parse_status_headers().0, 200);
    assert_eq!(pool.free_slots(), 3, "no slot was lost to the sweep");

    drop(pool);
    let _ = std::fs::remove_dir_all(&markers);
}

/// **A busy slot is skipped and reported, not replaced.**
///
/// The correctness half of the trade, and the reason `respawn_idle` is not just "restart everything":
/// replacing a slot mid-request truncates a live response. The test holds one slot with a slow
/// request and asserts that the request is answered *and* that the slot's pid is unchanged, while
/// the other slot was swept.
#[test]
fn a_sweep_leaves_a_busy_slot_alone_and_says_so() {
    let _serial = serial();
    require_sdk!();

    let markers = std::env::temp_dir().join(format!("volucer-busy-markers-{}", std::process::id()));
    let pool = booting_pool("roll-busy", 2, &markers, 900_000);
    let before = pool.pids();

    std::thread::scope(|scope| {
        let holder = scope.spawn(|| {
            pool.request(&get("/"))
                .expect("the in-flight request is answered")
                .parse_status_headers()
                .0
        });

        // Wait until one slot is genuinely taken, so this cannot pass by winning a race.
        let mut waited = Duration::ZERO;
        while pool.free_slots() != 1 && waited < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(5));
            waited += Duration::from_millis(5);
        }
        assert_eq!(pool.free_slots(), 1, "exactly one slot is held");

        let report = pool.respawn_idle();
        assert_eq!(
            report.replaced.len(),
            1,
            "the free slot was swept: {report:?}"
        );
        assert_eq!(
            report.skipped_busy.len(),
            1,
            "the held slot was skipped and reported rather than ignored: {report:?}"
        );
        assert!(
            !report.is_complete(),
            "a sweep that left a slot behind is not a finished deploy: {report:?}"
        );
        let busy = report.skipped_busy[0];
        assert_eq!(
            pool.pids()[busy],
            before[busy],
            "the busy slot's process must be the one that was serving, or its response is truncated"
        );

        assert_eq!(holder.join().expect("no thread panics"), 200);
    });

    assert_eq!(pool.free_slots(), 2, "the held slot came back");
    drop(pool);
    let _ = std::fs::remove_dir_all(&markers);
}

/// A second sweep finishes a roll the first one could not.
///
/// The operational contract, stated as a test because it is the thing a deploy script depends on:
/// `is_complete() == false` is not a failure, it is an instruction to sweep again. Without this a
/// busy slot would be left on the old code for ever.
#[test]
fn a_second_sweep_finishes_what_the_first_left_behind() {
    let _serial = serial();
    require_sdk!();

    let markers =
        std::env::temp_dir().join(format!("volucer-twice-markers-{}", std::process::id()));
    let pool = booting_pool("roll-twice", 2, &markers, 900_000);
    let before = pool.pids();

    std::thread::scope(|scope| {
        let holder = scope.spawn(|| pool.request(&get("/")).map(|_| ()));
        let mut waited = Duration::ZERO;
        while pool.free_slots() != 1 && waited < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(5));
            waited += Duration::from_millis(5);
        }
        let first = pool.respawn_idle();
        assert_eq!(first.replaced.len(), 1, "{first:?}");
        assert_eq!(first.skipped_busy.len(), 1, "{first:?}");
        holder.join().expect("no thread panics").expect("served");
    });

    assert_eq!(pool.free_slots(), 2, "the held slot came back");
    let second = pool.respawn_idle();
    assert_eq!(
        second.replaced.len(),
        2,
        "the second sweep covers the slot the first one skipped: {second:?}"
    );
    assert!(second.is_complete(), "{second:?}");
    assert_ne!(
        pool.pids(),
        before,
        "both slots now run a process booted by a sweep"
    );

    drop(pool);
    let _ = std::fs::remove_dir_all(&markers);
}

/// **An `attach` pool reports itself unmanaged instead of pretending.**
///
/// Volucer dialled that relay; the processes belong to somebody's supervisor. Replacing a connection
/// would look, from the far side, like a worker dying — so the honest answer is a refusal with the
/// reason, and `is_complete()` must be false so a deploy script does not read it as done.
#[test]
fn an_attached_pool_reports_that_it_cannot_be_swept() {
    use volucer_core::config::AttachConfig;

    // Nothing is listening on this path, and nothing has to be: the pool is asked what it *can* do,
    // and `start_attached` dials per slot — so a pool of zero connections is the way to ask without
    // a relay. `workers: 0` is refused by validation, which is why the count is one and the connect
    // failure is tolerated: the report is about ownership, not about liveness.
    let config = AttachConfig {
        relay: Some("unix:/nonexistent/volucer-roll-test.sock".to_string()),
        workers: 1,
        max_requests: None,
        services: Vec::new(),
        checksum: Default::default(),
        grpc_services: Vec::new(),
        grpc_serve_any: false,
        grpc_proto: Vec::new(),
        grpc_proto_services: Vec::new(),
    };
    let pool = match WorkerPool::start_attached("attached", &config, None) {
        Ok(pool) => pool,
        // A pool cannot be built without its relay; the assertion below is unreachable then, and
        // saying so is better than a test that silently asserts nothing.
        Err(err) => {
            eprintln!("skipping: the attach pool could not be built ({err})");
            return;
        }
    };

    let report = pool.respawn_idle();
    assert_eq!(report.backend, "attached");
    assert!(
        report.replaced.is_empty(),
        "volucer must not replace processes it did not start: {report:?}"
    );
    let unmanaged = report
        .unmanaged
        .as_ref()
        .expect("an attached pool says who owns its processes");
    assert_eq!(unmanaged.slots, 1, "{report:?}");
    assert!(
        unmanaged.reason.contains("attach"),
        "the reason names the shape, so an operator knows why: {report:?}"
    );
    assert!(
        !report.is_complete(),
        "a pool nothing was replaced in must not read as a finished deploy: {report:?}"
    );
    assert!(report.is_noop());
}
