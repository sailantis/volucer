//! Integration tests for the `external` backend, which attaches to a FastCGI
//! server it does not own.
//!
//! `php-fpm` does not exist on Windows, so these tests point the backend at a
//! `php-cgi -b` process started by the test itself. That is the same code path:
//! the backend only ever sees a socket it did not create.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};

use volucer_core::config::{BackendConfig, Config, ExternalConfig, ServerConfig};
use volucer_fcgi::{
    build_params, read_record, write_records, EndRequest, RecordType, Request, RequestInfo,
    Routing, Stream, REQUEST_ID,
};
use volucer_server::Dispatcher;

mod common;
use common::{find_php_cgi, serial, write_script};

/// A FastCGI server the test owns, standing in for an external `php-fpm`.
struct ExternalServer {
    child: Child,
    port: u16,
}

impl ExternalServer {
    /// Start a stand-in FastCGI server, or fail having tried more than one port.
    ///
    /// The port has to be released before `php-cgi -b` can take it, and releasing a
    /// port makes it available to everything on the machine — including the other
    /// tests in this binary, which run concurrently. That is not theoretical: this
    /// used to bind `:0`, read the port, drop the listener, and then probe the port
    /// with `TcpStream::connect`. The kernel is free to hand that port to a later
    /// `bind(":0")` — a listener another test was polling `accept()` on — and the
    /// probe then *connected to a stranger*, turning that test's "nobody connects"
    /// assertion into a connection. It surfaced as
    /// `a_socket_worker_that_never_connects_times_out_rather_than_hanging` failing in
    /// CI with a live `TcpStream`, several modules away from the cause.
    ///
    /// So the retry is on the whole bind-spawn-probe sequence rather than on the
    /// probe, and the probe is made meaningful: a stolen port kills our child,
    /// because `php-cgi` cannot bind a port something else holds. "Something answers"
    /// alone would have been exactly the check that passed for the wrong reason.
    fn start(binary: &Path) -> Self {
        const ATTEMPTS: usize = 10;
        let mut last: Option<String> = None;

        for _ in 0..ATTEMPTS {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind a port");
            let port = listener.local_addr().expect("address").port();
            drop(listener);

            let mut child = Command::new(binary)
                .arg("-b")
                .arg(format!("127.0.0.1:{port}"))
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("start the stand-in server");

            let deadline = Instant::now() + Duration::from_secs(10);
            while Instant::now() < deadline {
                // A child that is gone could not bind, which for a port we have just
                // released means somebody else took it. Another port is the fix.
                match child.try_wait() {
                    Ok(Some(status)) => {
                        last = Some(status.to_string());
                        break;
                    }
                    Ok(None) => {}
                    Err(err) => panic!("could not read the stand-in server's status: {err}"),
                }
                if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
                    return ExternalServer { child, port };
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            let _ = child.kill();
            let _ = child.wait();
        }

        // The child's own status rather than "no port could be taken": a genuinely
        // broken binary also exits, and relabelling that as a network problem would
        // send the operator to the wrong place. Same reasoning as `pool.rs`.
        panic!(
            "the stand-in server never started listening in {ATTEMPTS} attempts{}",
            last.map(|status| format!("; the last one {status}"))
                .unwrap_or_default()
        );
    }

    fn socket(&self) -> String {
        format!("127.0.0.1:{}", self.port)
    }
}

impl Drop for ExternalServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A FastCGI server that answers every connection at once, with nothing to
/// serialise on.
///
/// The `spawn_cgi` stand-in used by the other tests is a real `php-cgi -b`, and one
/// process serves one connection at a time however many are open: it takes the second
/// request on the connection it is already holding, because the client asked for
/// keep-alive. **That is about the process, not about `php-cgi` on Linux** — with
/// `PHP_FCGI_CHILDREN` set, `php-cgi` preforking is real: measured here, `n = 4`
/// produced four children serving twelve sequential requests round-robin, and this pool
/// forces `PHP_FCGI_CHILDREN = 0` so that a slot really is one process (see
/// `pool.rs`). A lone `php-cgi` started without that variable is therefore a test
/// fixture that cannot exhibit the property under test, whatever the platform.
///
/// A *real* `php-fpm` is a master over a child pool and answers as many at once as
/// `pm.max_children` allows, which is the thing `ClientPool` exists for.
///
/// This server can. It accepts, reads a request, sleeps, and replies, all in a
/// thread per connection, and it deliberately does **not** honour keep-alive: one
/// request per connection, then close. That is a stricter backend than `php-fpm`
/// (which is happy to be reused) and it keeps the assertion about the pool rather
/// than about the protocol.
struct ConcurrentServer {
    port: u16,
    /// Requests inside `serve_once` right now, and the highest that figure ever was.
    ///
    /// **The measurement the cap is actually about.** A test that asserted a *duration* to prove a
    /// cap was enforced would pass on a broken pool whenever the second request happened to be slow
    /// to arrive; counting what the backend itself saw cannot. Read with [`Self::peak`].
    live: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
}

impl ConcurrentServer {
    fn start(sleep: Duration) -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind a port");
        let port = listener.local_addr().expect("address").port();
        let live = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let observed = (Arc::clone(&live), Arc::clone(&peak));

        thread::spawn(move || {
            let mut workers = Vec::new();
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let (live, peak) = (Arc::clone(&observed.0), Arc::clone(&observed.1));
                workers.push(thread::spawn(move || {
                    serve_once(Stream::Tcp(stream), sleep, &live, &peak)
                }));
            }
        });

        ConcurrentServer { port, live, peak }
    }

    fn socket(&self) -> String {
        format!("127.0.0.1:{}", self.port)
    }

    /// The most requests this server was answering at the same instant.
    fn peak(&self) -> usize {
        self.peak.load(Ordering::SeqCst)
    }

    /// Requests being answered right now.
    fn live(&self) -> usize {
        self.live.load(Ordering::SeqCst)
    }
}

/// Read one FastCGI request and answer it, sleeping first to widen the window in
/// which the pool's concurrency is observable.
fn serve_once(stream: Stream, sleep: Duration, live: &AtomicUsize, peak: &AtomicUsize) {
    let mut stream = stream;

    // BEGIN_REQUEST, PARAMS, PARAMS-terminator, STDIN, STDIN-terminator.
    for _ in 0..5 {
        match read_record(&mut stream) {
            Ok(Some(_)) => {}
            _ => return,
        }
    }

    // Counted around the sleep rather than around the whole function, because the sleep is what
    // stands in for the application thinking — the part a connection cap is meant to serialise.
    let now = live.fetch_add(1, Ordering::SeqCst) + 1;
    peak.fetch_max(now, Ordering::SeqCst);
    thread::sleep(sleep);
    live.fetch_sub(1, Ordering::SeqCst);

    let body = b"Status: 200 OK\r\nContent-Type: text/plain\r\n\r\nCONCURRENT";
    if write_records(&mut stream, RecordType::Stdout, REQUEST_ID, body, true).is_err() {
        return;
    }
    let end = EndRequest::complete();
    let _ = write_records(
        &mut stream,
        RecordType::EndRequest,
        REQUEST_ID,
        &end.encode(),
        false,
    );
}

fn dispatcher_for(socket: &str, root: &Path) -> Dispatcher {
    dispatcher_with(socket, root, None)
}

fn dispatcher_with(socket: &str, root: &Path, max_connections: Option<usize>) -> Dispatcher {
    dispatcher_tuned(socket, root, max_connections, None)
}

fn dispatcher_tuned(
    socket: &str,
    root: &Path,
    max_connections: Option<usize>,
    request_timeout_ms: Option<u64>,
) -> Dispatcher {
    let config = Config {
        server: ServerConfig {
            http: Some("127.0.0.1:8080".to_string()),
            default_backend: Some("fpm".to_string()),
            routes: Vec::new(),
            ..Default::default()
        },
        backends: [(
            "fpm".to_string(),
            BackendConfig::External(ExternalConfig {
                socket: socket.to_string(),
                max_connections,
                request_timeout_ms,
                services: Vec::new(),
            }),
        )]
        .into_iter()
        .collect(),
        sites: BTreeMap::new(),
        ..volucer_core::config::Config::default()
    };
    config.validate().expect("the config is valid");

    let routing = Routing {
        document_root: root.to_path_buf(),
        front_controller: Some("index.php".to_string()),
        ..Routing::default()
    };
    Dispatcher::from_config(&config, routing).expect("the dispatcher starts")
}

/// A request to a backend that routes on nothing, for the fixtures that answer any request the same.
fn plain_request() -> Request {
    Request {
        params: build_params(
            &RequestInfo {
                method: "POST".to_string(),
                uri: "/".to_string(),
                ..RequestInfo::default()
            },
            &Routing::default(),
        )
        .expect("params build"),
        body: b"x".to_vec(),
        keep_conn: true,
    }
}

fn request_for(root: &Path, marker: &str) -> Request {
    let body = marker.as_bytes().to_vec();
    let info = RequestInfo {
        method: "POST".to_string(),
        uri: "/".to_string(),
        body_len: body.len(),
        ..RequestInfo::default()
    };
    let routing = Routing {
        document_root: root.to_path_buf(),
        front_controller: Some("index.php".to_string()),
        ..Routing::default()
    };
    Request {
        params: build_params(&info, &routing).expect("params build"),
        body,
        keep_conn: true,
    }
}

#[test]
fn an_external_backend_serves_requests_over_its_socket() {
    let _serial = serial();
    let Some(binary) = find_php_cgi() else {
        volucer_testkit::skip("no php-cgi on PATH");
        return;
    };

    let server = ExternalServer::start(&binary);
    let root = std::env::temp_dir().join(format!("volucer-external-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create the document root");
    write_script(
        &root,
        "index.php",
        r#"<?php echo "EXT="; echo file_get_contents("php://input");"#,
    );

    let dispatcher = dispatcher_for(&server.socket(), &root);
    assert_eq!(dispatcher.default_backend(), "fpm");
    assert_eq!(dispatcher.backend_names(), vec!["fpm"]);

    let response = dispatcher
        .dispatch(&request_for(&root, "first"))
        .expect("the external backend answered");
    response.check_protocol().expect("completed");
    let stdout = String::from_utf8_lossy(&response.stdout);
    assert!(stdout.contains("EXT=first"), "{stdout:?}");

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn an_external_backend_reuses_its_connection_across_requests() {
    let _serial = serial();
    let Some(binary) = find_php_cgi() else {
        volucer_testkit::skip("no php-cgi on PATH");
        return;
    };

    let server = ExternalServer::start(&binary);
    let root = std::env::temp_dir().join(format!("volucer-reuse-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create the document root");
    write_script(
        &root,
        "index.php",
        r#"<?php echo "N="; echo file_get_contents("php://input");"#,
    );

    let dispatcher = dispatcher_for(&server.socket(), &root);

    // Several requests in a row: the backend must not need a new connection each
    // time, and must not fail if the server does close between requests.
    for index in 0..5 {
        let response = dispatcher
            .dispatch(&request_for(&root, &format!("r{index}")))
            .unwrap_or_else(|err| panic!("request {index} failed: {err}"));
        response.check_protocol().expect("completed");
        let stdout = String::from_utf8_lossy(&response.stdout);
        assert!(stdout.contains(&format!("N=r{index}")), "{stdout:?}");
    }

    let _ = std::fs::remove_dir_all(&root);
}

/// A `max_connections` cap makes requests wait rather than open more connections.
///
/// **The assertion is about what the backend saw, not about how long it took.** The original version
/// of this test measured the wall time of four requests against a cap of 1 and against no cap, which
/// is the weakest possible evidence: a duration separates "overlapped" from "serialised" only if
/// nothing else does, and on a loaded runner a serialising pool can still finish fast. Counting the
/// requests the fixture was answering at the same instant is the property itself — a cap of 1 can
/// never show a peak above 1, and the uncapped control reaches 4.
///
/// It uses `ConcurrentServer` rather than a real `php-cgi` for the same reason
/// `concurrent_requests_reach_the_backend_at_the_same_time` does: a lone `php-cgi` serves a single
/// connection at a time, so it would serialise the control arm and prove nothing about the cap.
#[test]
fn a_connection_cap_serialises_requests_instead_of_refusing_them() {
    let _serial = serial();

    const SLEEP: Duration = Duration::from_millis(200);

    let server = ConcurrentServer::start(SLEEP);

    fn run(server: &ConcurrentServer, max_connections: Option<usize>) -> usize {
        const REQUESTS: usize = 4;
        let dispatcher = dispatcher_with(
            &server.socket(),
            std::env::temp_dir().as_path(),
            max_connections,
        );
        let before = server.peak();
        std::thread::scope(|scope| {
            for _ in 0..REQUESTS {
                let dispatcher = &dispatcher;
                scope.spawn(move || {
                    let request = Request {
                        params: build_params(
                            &RequestInfo {
                                method: "POST".to_string(),
                                uri: "/".to_string(),
                                ..RequestInfo::default()
                            },
                            &Routing::default(),
                        )
                        .expect("params build"),
                        body: b"x".to_vec(),
                        keep_conn: true,
                    };
                    let response = dispatcher.dispatch(&request).expect("served");
                    // Nothing was refused: a cap that answered 503 would be a different feature.
                    assert_eq!(200, response.parse_status_headers().0);
                });
            }
        });
        server.peak() - before
    }

    // The control first, because without it a pool that serialises *everything* — the original
    // one-connection-per-backend defect — would satisfy the capped arm.
    const REQUESTS: usize = 4;
    let uncapped = run(&server, None);
    assert_eq!(
        REQUESTS, uncapped,
        "without a cap all {REQUESTS} requests must reach the backend at once, but it never saw \
         more than {uncapped}"
    );

    // A fresh server, so the control's peak cannot stand in for this one's.
    let capped = ConcurrentServer::start(SLEEP);
    let peak = run(&capped, Some(1));
    assert_eq!(
        1, peak,
        "with a cap of 1 the backend must never see two requests at once, but it saw {peak}"
    );
}

/// The two paths share one admission count.
///
/// `max_connections` is a statement about how many requests the backend sees at once, so a blocking
/// and an awaited request must contend for the same budget. Two counters would admit one of each
/// with a cap of 1, which is two.
///
/// **The assertion is what the backend saw, not how long anything took.** With separate counters the
/// awaited request is admitted at once and opens a second connection, so the fixture — which answers
/// every connection on its own thread — is serving two at the same instant. That is the difference
/// between one budget and two, and it is a property rather than a duration. The `spawn_cgi` version
/// of this test originally measured elapsed time and passed against a deliberately broken pool,
/// because the second request simply waited for the backend either way.
#[test]
fn the_two_paths_share_one_admission_count() {
    let _serial = serial();

    const SLEEP: Duration = Duration::from_millis(400);
    let server = ConcurrentServer::start(SLEEP);
    let dispatcher = Arc::new(dispatcher_with(
        &server.socket(),
        std::env::temp_dir().as_path(),
        Some(1),
    ));

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("a runtime");

    let before = server.peak();
    std::thread::scope(|scope| {
        let blocking = Arc::clone(&dispatcher);
        scope.spawn(move || {
            blocking
                .dispatch(&plain_request())
                .expect("the blocking request is served")
        });

        // The blocking request must be *inside* the backend before the awaited one starts, so that
        // the awaited one is genuinely the waiter — otherwise either could take the permit first and
        // the test would prove nothing about the second admission.
        let deadline = Instant::now() + Duration::from_secs(5);
        while server.live() == 0 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(
            1,
            server.live(),
            "the blocking request should be holding the backend before the awaited one starts"
        );
        assert_eq!(
            Some(1),
            dispatcher.in_flight("fpm"),
            "the blocking request should be the one inside the cap"
        );

        let awaited = Arc::clone(&dispatcher);
        let served = scope.spawn(move || {
            runtime
                .block_on(async move { awaited.dispatch_to_async("fpm", &plain_request()).await })
        });

        // A deferred admission is the only way the shared count can be observed as *shared* rather
        // than as merely present: with two counters this would return at once and the backend would
        // see two requests at the same instant.
        served
            .join()
            .expect("the awaited request is served")
            .expect("the awaited request is answered");
    });

    assert_eq!(
        1,
        server.peak() - before,
        "a cap of 1 bounds both paths together, so the backend must never see two requests at \
         once; it saw {}",
        server.peak() - before
    );
}

/// An awaited request over the cap waits **without parking the runtime thread it runs on**.
///
/// **This is the defect the `spawn_cgi` change fixed and `external` did not.** `Limit::enter` is a
/// `Condvar::wait`, and calling it from an `async fn` parks whichever thread polls the future. That
/// thread is a runtime worker, so the task's own socket future can no longer be polled by it — and
/// if the permit that task is waiting for is held by another task *on the same runtime*, nothing can
/// ever make progress. It is a deadlock, not a slowdown, and on a small runtime a handful of queued
/// requests are enough to reach it.
///
/// So the shape of the test is the deadlock's: a **single-threaded** runtime, a cap of 1 and two
/// awaited requests. The first takes the permit and is inside the backend for `SLEEP`; the second
/// must wait for it. With `enter_async` the waiting task yields and the first one finishes; with
/// `enter` the one runtime thread blocks in the condvar and the test never returns.
///
/// Bounded rather than joined, because the failure being tested for *is* a hang: a regression has to
/// report in a few seconds, not park the suite for ever.
#[test]
fn an_awaited_request_waits_for_a_permit_without_parking_its_runtime() {
    let _serial = serial();

    const SLEEP: Duration = Duration::from_millis(300);
    let server = ConcurrentServer::start(SLEEP);
    let dispatcher = Arc::new(dispatcher_with(
        &server.socket(),
        std::env::temp_dir().as_path(),
        Some(1),
    ));

    let (tx, rx) = mpsc::channel();
    let runner = Arc::clone(&dispatcher);
    let worker = thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a runtime");
        let (one, two) = (plain_request(), plain_request());
        let served = runtime.block_on(async move {
            tokio::join!(
                runner.dispatch_to_async("fpm", &one),
                runner.dispatch_to_async("fpm", &two),
            )
        });
        let _ = tx.send(served);
    });

    let deadline = Duration::from_secs(10);
    let (first, second) = rx
        .recv_timeout(deadline)
        .unwrap_or_else(|_| panic!("the second awaited request never got its place within {deadline:?} — a waiting request must yield, not park the runtime"));
    worker.join().expect("the runtime thread finished");
    first.expect("the first request is served");
    second.expect("the second request is served");
}

/// Both paths together hold **one** warm connection, not one each.
///
/// `MAX_IDLE_CONNECTIONS` is one because every parked connection removes a child from `php-fpm`'s
/// dispatch set — measured, not assumed: against a 12-child pool, retaining 4 cost 7% of throughput
/// at concurrency 32 and retaining 12 cost 26%. That cost is per **child**, so it does not halve
/// when a backend serves two paths: a per-list check would keep one blocking connection and one
/// awaited one, which is two children gone for a deployment that uses both front-ends — exactly the
/// configuration volucer documents and the one this test sets up.
///
/// Nothing in a request's behaviour shows the difference. Both connections work, every request is
/// served, and the only visible symptom is a child quietly missing from the far side's pool — which
/// is why the count has to be asserted directly.
#[test]
fn both_paths_share_one_warm_connection() {
    let _serial = serial();

    const SLEEP: Duration = Duration::from_millis(50);
    let server = ConcurrentServer::start(SLEEP);
    let dispatcher = Arc::new(dispatcher_with(
        &server.socket(),
        std::env::temp_dir().as_path(),
        None,
    ));

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("a runtime");

    // Blocking first, so its connection is the one parked; then an awaited request, which would
    // park a second one under a per-mode cap.
    dispatcher
        .dispatch_to("fpm", &plain_request())
        .expect("the blocking request is served");
    runtime
        .block_on(dispatcher.dispatch_to_async("fpm", &plain_request()))
        .expect("the awaited request is served");

    assert_eq!(
        Some(1),
        dispatcher.retained("fpm"),
        "one backend may park one warm connection in total, whatever mixture of paths it serves"
    );

    // And each path still reuses what is there rather than opening a fresh socket per request, so
    // the single connection is genuinely shared rather than merely counted.
    dispatcher
        .dispatch_to("fpm", &plain_request())
        .expect("a second blocking request is served");
    runtime
        .block_on(dispatcher.dispatch_to_async("fpm", &plain_request()))
        .expect("a second awaited request is served");
    assert_eq!(
        Some(1),
        dispatcher.retained("fpm"),
        "reuse must not grow the parked set past the one connection it is allowed"
    );
}

#[test]
fn an_external_backend_reconnects_after_the_server_is_replaced() {
    let _serial = serial();
    let Some(binary) = find_php_cgi() else {
        volucer_testkit::skip("no php-cgi on PATH");
        return;
    };

    let mut server = ExternalServer::start(&binary);
    let port = server.port;
    let root = std::env::temp_dir().join(format!("volucer-reconnect-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create the document root");
    write_script(
        &root,
        "index.php",
        r#"<?php echo "RC="; echo file_get_contents("php://input");"#,
    );

    let dispatcher = dispatcher_for(&server.socket(), &root);
    dispatcher
        .dispatch(&request_for(&root, "before"))
        .expect("served before the restart");

    // Restart the server on the same port, the way systemd would reload php-fpm.
    // The dispatcher still holds a connection to the dead process.
    let _ = server.child.kill();
    let _ = server.child.wait();
    std::thread::sleep(Duration::from_millis(200));

    server.child = Command::new(&binary)
        .arg("-b")
        .arg(format!("127.0.0.1:{port}"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("restart the stand-in server");
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline && std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
        std::thread::sleep(Duration::from_millis(50));
    }

    // The first request after a restart may hit the stale connection; what
    // matters is that the backend recovers rather than staying broken.
    let mut last_error = None;
    let mut served = false;
    for attempt in 0..3 {
        match dispatcher.dispatch(&request_for(&root, &format!("after{attempt}"))) {
            Ok(response) => {
                let stdout = String::from_utf8_lossy(&response.stdout);
                if stdout.contains(&format!("RC=after{attempt}")) {
                    served = true;
                    break;
                }
                last_error = Some(format!("unexpected body: {stdout:?}"));
            }
            Err(err) => last_error = Some(err.to_string()),
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    assert!(served, "the backend never recovered: {last_error:?}");

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_socket_that_is_not_listening_fails_the_request_not_the_startup() {
    let _serial = serial();
    let root = std::env::temp_dir().join(format!("volucer-dead-{}", std::process::id()));
    std::fs::create_dir_all(&root).expect("create the document root");

    // Attaching to a socket is lazy: the dispatcher must start even when nothing
    // is listening, because php-fpm may be starting alongside us. The failure
    // then belongs to the request.
    let dispatcher = dispatcher_for("127.0.0.1:1", &root);
    let err = dispatcher
        .dispatch(&request_for(&root, "x"))
        .expect_err("nothing is listening on port 1");
    assert!(err.to_string().contains("fpm"), "{err}");

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn concurrent_requests_reach_the_backend_at_the_same_time() {
    let _serial = serial();

    // The regression test for the one-connection-per-backend defect. The backend used to hold
    // one `Mutex<Option<Client>>` for the whole request, so a pool of any size
    // served exactly one request at a time. A backend that can only be talked to
    // through one connection is invisible to a sequential test — every request
    // succeeds and every response is correct — so the assertion has to be about
    // *time*, not about correctness.
    //
    // The backend is `ConcurrentServer` rather than a real `php-cgi`, which is the
    // correction this test needed. A lone Linux `php-cgi` serves one connection at
    // a time, so it serialises four requests no matter how good the pool is, and it
    // blocks forever on the second request if the first connection is kept alive —
    // on this host the test did not fail, it *hung*. Measured on the VM: one
    // request passes in 0.3 s, two hang past 60 s, and the threads sit in `recv`
    // inside `Stream::read` with no timeout set.
    //
    // Four requests that each sleep 300 ms must finish in about one sleep, not
    // four. The bound is deliberately loose (1 s rather than 400 ms) because this
    // is not a benchmark: it only has to separate "at the same time" from "one
    // after another", and 1.2 s of serialisation is unambiguous.
    const REQUESTS: usize = 4;
    const SLEEP: Duration = Duration::from_millis(300);

    let server = ConcurrentServer::start(SLEEP);
    let dispatcher = dispatcher_for(&server.socket(), std::env::temp_dir().as_path());

    let started = Instant::now();
    let (tx, rx) = mpsc::channel();
    std::thread::scope(|scope| {
        for index in 0..REQUESTS {
            let tx = tx.clone();
            let dispatcher = &dispatcher;
            scope.spawn(move || {
                let mut info = RequestInfo {
                    method: "POST".to_string(),
                    uri: "/".to_string(),
                    ..RequestInfo::default()
                };
                let body = format!("c{index}").into_bytes();
                info.body_len = body.len();
                let request = Request {
                    params: build_params(&info, &Routing::default()).expect("params build"),
                    body,
                    keep_conn: true,
                };
                let outcome = dispatcher
                    .dispatch(&request)
                    .map(|response| String::from_utf8_lossy(&response.stdout).to_string())
                    .map_err(|err| err.to_string());
                let _ = tx.send(outcome);
            });
        }
        drop(tx);

        // Bounded, so a regression reports rather than waits. The original version
        // of this test hung for 17 minutes on the VM instead of failing, which is
        // how a real defect stayed invisible.
        for _ in 0..REQUESTS {
            let body = rx
                .recv_timeout(Duration::from_secs(5))
                .expect("every request was answered within 5s")
                .expect("every request was answered");
            assert!(body.contains("CONCURRENT"), "{body:?}");
        }
    });
    let done = started.elapsed();

    let serial = SLEEP * REQUESTS as u32;
    assert!(
        done < Duration::from_secs(1),
        "{REQUESTS} concurrent requests took {done:?}, which is close to the \
         {serial:?} they would take if they were serialised on one connection"
    );
}

/// A FastCGI server that reads the request and then never answers.
///
/// The shape `php-fpm` has after a child wedges: the connection is accepted, the request arrives in
/// full, and no record ever comes back. Nothing in FastCGI makes that detectable from this side, so
/// the only thing that can end the request is a deadline — which is the point of the test below.
struct SilentServer {
    port: u16,
    /// Held so the accepted sockets outlive the request: a closed socket would be an EOF, which is a
    /// transport error rather than the silence this fixture exists to produce.
    held: Arc<std::sync::Mutex<Vec<Stream>>>,
}

impl SilentServer {
    fn start() -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind a port");
        let port = listener.local_addr().expect("address").port();
        let held = Arc::new(std::sync::Mutex::new(Vec::new()));
        let accepted = Arc::clone(&held);

        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                // Read the whole request so the client's write completes — a backend that did not
                // read would give the client a broken pipe, which is a different failure.
                let mut stream = Stream::Tcp(stream);
                for _ in 0..5 {
                    match read_record(&mut stream) {
                        Ok(Some(_)) => {}
                        _ => break,
                    }
                }
                if let Ok(mut held) = accepted.lock() {
                    held.push(stream);
                }
            }
        });

        SilentServer { port, held }
    }

    fn socket(&self) -> String {
        format!("127.0.0.1:{}", self.port)
    }
}

/// An awaited request to a server that never answers fails on its deadline, rather than for ever.
///
/// **`external` had no deadline at all before this key existed**, on either path, and it is the
/// backend where that matters most: `worker` and `spawn_cgi` own their processes and can kill one
/// that stops answering, while an `external` child belongs to `php-fpm`. A request to it that is
/// accepted and never answered therefore held its connection and its `max_connections` permit for
/// the life of the process, with the count only ever going up.
///
/// The awaited path is the one that needs the *wrapper* rather than a socket option, because
/// `SO_RCVTIMEO` is cleared when a socket is adopted by a runtime. Both are tested here through the
/// same public entry point, so a fix that only reached one of them shows up.
#[test]
fn a_backend_that_never_answers_fails_on_the_request_deadline() {
    let _serial = serial();

    const TIMEOUT_MS: u64 = 400;
    let server = SilentServer::start();
    // A cap, so the permit count below is observable at all — `in_flight` is `None` for an uncapped
    // backend, because there is no count to report. It is large enough not to serialise anything.
    let dispatcher = dispatcher_tuned(
        &server.socket(),
        std::env::temp_dir().as_path(),
        Some(8),
        Some(TIMEOUT_MS),
    );

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("a runtime");

    // Awaited first, because that is the path whose deadline did not exist: the socket has no
    // receive timeout once a runtime owns it, so only `tokio::time::timeout` can end this.
    //
    // **Bounded from the outside too**, because the failure being tested for is a hang: without the
    // wrapper inside the backend the future never resolves, and a test that simply awaited it would
    // park the suite instead of reporting. Ten seconds is many times the deadline and still short
    // enough to be a failure rather than a timeout.
    let budget = Duration::from_secs(10);
    let started = Instant::now();
    let outcome = runtime.block_on(async {
        tokio::time::timeout(
            budget,
            dispatcher.dispatch_to_async("fpm", &plain_request()),
        )
        .await
    });
    let elapsed = started.elapsed();
    let err = outcome
        .unwrap_or_else(|_| {
            panic!(
                "the awaited request never returned, so it is bounded by nothing; a backend that \
                 accepts a request and never answers would hold its connection for ever"
            )
        })
        .expect_err("a server that never answers must not answer");

    assert!(
        elapsed < Duration::from_secs(5),
        "the awaited request should fail on its {TIMEOUT_MS} ms deadline, but took {elapsed:?}"
    );
    assert!(
        err.to_string().contains("request_timeout_ms"),
        "the error should name the key the operator configured: {err}"
    );

    // Then the blocking path, whose deadline is the socket's own. It must not be *slower* than the
    // awaited one, which is what a missing `SO_RCVTIMEO` on the reconnect would produce. Bounded on
    // its own thread for the same reason as above — a socket with no receive timeout blocks its
    // reader for ever, and that is a hang rather than a failure.
    let (tx, rx) = mpsc::channel();
    std::thread::scope(|scope| {
        let blocking = &dispatcher;
        let tx = tx.clone();
        scope.spawn(move || {
            let started = Instant::now();
            let outcome = blocking
                .dispatch_to("fpm", &plain_request())
                .map_err(|err| err.to_string());
            let _ = tx.send((outcome, started.elapsed()));
        });
        let (outcome, elapsed) = rx.recv_timeout(budget).unwrap_or_else(|_| {
            panic!("the blocking request never returned, so it is bounded by nothing")
        });
        let err = outcome.expect_err("a server that never answers must not answer");
        assert!(
            elapsed < Duration::from_secs(5),
            "the blocking request should fail on its {TIMEOUT_MS} ms deadline, but took {elapsed:?}"
        );
        assert!(
            err.contains("request_timeout_ms"),
            "the error should name the key the operator configured: {err}"
        );
    });
    drop(tx);

    // Nothing is left in flight behind either failure: a timed-out request that kept its permit
    // would leak the cap one request at a time, which is the failure mode the deadline exists to
    // stop.
    assert_eq!(
        Some(0),
        dispatcher.in_flight("fpm"),
        "a request that timed out must not still be holding a place"
    );

    drop(server.held.lock().map(|held| held.len()));
}
