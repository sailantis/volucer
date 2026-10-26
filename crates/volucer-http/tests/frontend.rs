//! Integration tests for the HTTP front-end, against a real PHP worker.
//!
//! These are the tests that prove the front-end actually works end to end: a
//! real `volucer-http` listener, a real HTTP client over a real socket, a real
//! worker pool, and the shipped PHP SDK on the far end. The unit tests in
//! `listener.rs` cover the two conversions in isolation; what is under test
//! *here* is everything between a browser-shaped request and the bytes that come
//! back.
//!
//! Two things are asserted that a unit test cannot reach. First, that the
//! blocking backends are reached safely from the async front-end — the pool
//! serves through `spawn_blocking`, and a mistake there shows up as a stall
//! rather than a wrong value. Second, that concurrent requests are served in
//! parallel: the pool has more slots than one, and the front-end must not
//! serialise them behind a single blocking call.
//!
//! Skipped, with a printed reason, when no `php` binary is on `PATH`.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use volucer_core::config::{BackendConfig, Config, ServerConfig, WorkerConfig};
use volucer_core::sites::Sites;
use volucer_fcgi::Routing;
use volucer_http::{HttpLimits, HttpListener};
use volucer_server::Dispatcher;

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
/// `VOLUCER_SDK_DIR` overrides it; otherwise the sibling layout is assumed.
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
/// ordinary run. Set `VOLUCER_E2E=1` where a skip should be a failure.
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

/// A configuration pointing a worker backend at the example worker.
///
/// Callers check `skip_reason()` first, so reaching the assertion below means
/// something asked for this config without one — which is a test bug, not an
/// environment fact, and has to fail loudly rather than skip.
fn worker_config(workers: u32) -> Config {
    let script = example_worker();
    assert!(
        script.is_file(),
        "the SDK example worker is missing at {} — set VOLUCER_SDK_DIR to a \
         volucer-php-sdk checkout",
        script.display()
    );

    Config {
        server: ServerConfig {
            http: Some("127.0.0.1:0".to_string()),
            default_backend: Some("app".to_string()),
            routes: Vec::new(),
            ..Default::default()
        },
        backends: [(
            "app".to_string(),
            BackendConfig::Worker(WorkerConfig {
                command: format!("php \"{}\"", script.display()),
                workers,
                memory_limit_mb: None,
                max_requests: None,
                working_dir: None,
                env: Default::default(),
                checksum: Default::default(),
                services: Vec::new(),
                grpc_services: Vec::new(),
                grpc_serve_any: false,
                grpc_proto: Vec::new(),
                grpc_proto_services: Vec::new(),
                transport: Default::default(),
                socket: None,
            }),
        )]
        .into_iter()
        .collect(),
        sites: BTreeMap::new(),
        ..volucer_core::config::Config::default()
    }
}

/// The routing a front controller implies.
fn front_controller_routing() -> Routing {
    Routing {
        document_root: Path::new(".").to_path_buf(),
        front_controller: Some("index.php".to_string()),
        ..Routing::default()
    }
}

/// A running HTTP server on a loopback port, shut down when dropped.
struct Server {
    address: String,
    shutdown: volucer_http::HttpShutdown,
    /// Replaces the listener's reloadable state, for the reload tests.
    reload: volucer_http::ReloadHandle,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Server {
    /// Start a listener whose requests are served by a real worker pool.
    ///
    /// The runtime is built here and moved onto the thread that runs it, because
    /// the listener can only be created inside a runtime. `#[tokio::test]` is not
    /// used anywhere: it would need the `macros` feature, and a test that builds
    /// its own runtime keeps the async boundary visible in the test as well as in
    /// the production code.
    fn with_workers(workers: u32) -> Option<Server> {
        if let Some(reason) = skip_reason() {
            report_skip(reason);
            return None;
        }
        let config = worker_config(workers);
        let routing = front_controller_routing();
        let dispatcher = match Dispatcher::from_config(&config, routing.clone()) {
            Ok(dispatcher) => dispatcher,
            Err(err) => {
                eprintln!("skipping: the worker pool could not start: {err}");
                return None;
            }
        };

        Server::start(
            dispatcher,
            routing,
            HttpLimits::default(),
            Default::default(),
        )
    }

    /// Start a listener around an already-built dispatcher.
    fn start(
        dispatcher: Dispatcher,
        routing: Routing,
        limits: HttpLimits,
        statics: volucer_core::config::StaticConfig,
    ) -> Option<Server> {
        Server::start_sites(dispatcher, Arc::new(sites_for(&routing, statics)), limits)
    }

    /// Start a listener around an explicit site table, for tests that serve more than one.
    fn start_sites(
        dispatcher: Dispatcher,
        sites: Arc<Sites>,
        limits: HttpLimits,
    ) -> Option<Server> {
        let handles = volucer_http::ReloadHandles::new(Arc::new(volucer_http::Registry::new(
            volucer_http::SharedState::new((*sites).clone()),
        )));
        Server::start_with_handles(dispatcher, limits, handles)
    }

    /// Start a listener around handles a test has built, for the rate-limit tests.
    ///
    /// `bind_with_resolver` is the production path — `serve` builds its handles and passes them in
    /// — so a test that needs a limiter uses it rather than a side door that only tests can reach.
    fn start_with_handles(
        dispatcher: Dispatcher,
        limits: HttpLimits,
        handles: volucer_http::ReloadHandles,
    ) -> Option<Server> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("a tokio runtime");

        let listener = runtime.block_on(HttpListener::bind_with_resolver(
            "127.0.0.1:0",
            Arc::new(dispatcher),
            handles,
            limits,
            volucer_http::ListenerSettings::default(),
        ));
        let listener = match listener {
            Ok(listener) => listener,
            Err(err) => {
                eprintln!("skipping: the HTTP listener could not bind: {err}");
                return None;
            }
        };

        let address = listener.local_addr().expect("bound address").to_string();
        let shutdown = listener.shutdown_handle();
        // Taken before the listener is moved onto its thread, because `run` consumes it.
        let reload = listener.reload_handle();

        let handle = std::thread::spawn(move || {
            let _guard = runtime.enter();
            if let Err(err) = runtime.block_on(listener.run()) {
                eprintln!("http listener stopped: {err}");
            }
        });

        Some(Server {
            address,
            shutdown,
            reload,
            handle: Some(handle),
        })
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.shutdown.shutdown();
        // Joining waits for the listener to leave its accept loop, which is what
        // guarantees the pool — and therefore every worker process — is dropped
        // before the test ends. Without the join the next test can race the
        // reaping, which is a flake rather than a failure.
        let _ = self.handle.take().map(|handle| handle.join());
    }
}

/// One HTTP exchange, written and read over a raw socket.
#[derive(Debug)]
struct Exchange {
    status: u16,
    reason: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Exchange {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// Send a raw request and read the whole response.
///
/// Raw bytes rather than an HTTP client library, deliberately: the front-end's
/// job is to be correct about framing, and a client that hides framing would hide
/// the bug. `Connection: close` is sent so the response ends at EOF and no
/// chunked decoding is needed.
fn exchange(address: &str, request: &[u8]) -> Exchange {
    let mut stream = TcpStream::connect(address).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .expect("a read timeout");
    stream.write_all(request).expect("write the request");
    stream.flush().expect("flush");

    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).expect("read the response");

    parse(&raw)
}

/// A `GET` on a fresh connection.
fn get(address: &str, path: &str) -> Exchange {
    let request = format!("GET {path} HTTP/1.1\r\nHost: example.test\r\nConnection: close\r\n\r\n");
    exchange(address, request.as_bytes())
}

/// A `GET` naming the host, so a test can drive two sites through one listener.
fn get_as(address: &str, host: &str, path: &str) -> Exchange {
    let request = format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
    exchange(address, request.as_bytes())
}

fn parse(raw: &[u8]) -> Exchange {
    let split = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .unwrap_or_else(|| panic!("no header terminator in {:?}", String::from_utf8_lossy(raw)));
    let head = String::from_utf8_lossy(&raw[..split]).into_owned();
    let body = raw[split + 4..].to_vec();

    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap_or_default();
    let mut parts = status_line.splitn(3, ' ');
    let _version = parts.next();
    let status = parts
        .next()
        .and_then(|code| code.parse().ok())
        .unwrap_or_else(|| panic!("no status in {status_line:?}"));
    let reason = parts.next().unwrap_or_default().to_string();

    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_string(), value.trim().to_string()))
        .collect();

    Exchange {
        status,
        reason,
        headers,
        body,
    }
}

/// A `Server` for a test, or a printed reason why it cannot run.
macro_rules! server {
    ($workers:expr) => {
        match Server::with_workers($workers) {
            Some(server) => server,
            None => return,
        }
    };
}

#[test]
fn a_get_reaches_the_php_worker_and_comes_back() {
    let server = server!(2);

    let response = get(&server.address, "/health");

    assert_eq!(response.status, 200, "{response:?}");
    let body = response.text();
    assert!(
        body.contains("ok"),
        "the worker's body did not arrive: {body}"
    );
    // The body was produced by PHP, so Content-Length has to describe it.
    assert_eq!(
        response.header("content-length").expect("a content length"),
        response.body.len().to_string()
    );
}

#[test]
fn a_path_and_query_string_reach_php_intact() {
    let server = server!(2);

    let response = get(&server.address, "/echo?x=1&y=two%20words");

    assert_eq!(response.status, 200, "{response:?}");
    let body = response.text();
    // The example worker echoes the URI and the parsed query, so this asserts
    // that both REQUEST_URI and QUERY_STRING survived the translation.
    assert!(body.contains("/echo"), "{body}");
    assert!(body.contains("x=1"), "{body}");
}

#[test]
fn a_post_body_reaches_the_application_byte_for_byte() {
    let server = server!(2);

    let payload = "abcdefghij-a-marker-string";
    let request = format!(
        "POST /echo HTTP/1.1\r\nHost: example.test\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
        payload.len()
    );
    let response = exchange(&server.address, request.as_bytes());

    assert_eq!(response.status, 200, "{response:?}");
    let body = response.text();
    assert!(
        body.contains(&format!("payload: {payload}")),
        "the body did not arrive intact: {body}"
    );
    // The application's own count must agree with what PHP read.
    assert!(
        body.contains(&format!("body: {} bytes", payload.len())),
        "the declared length and the received body disagree: {body}"
    );
}

#[test]
fn request_headers_become_the_cgi_environment() {
    let server = server!(2);

    let request = "GET /headers HTTP/1.1\r\nHost: example.test\r\nX-Marker: a-value\r\nConnection: close\r\n\r\n";
    let response = exchange(&server.address, request.as_bytes());

    assert_eq!(response.status, 200, "{response:?}");
    let body = response.text();
    // `X-Marker` is only visible to PHP as `HTTP_X_MARKER`, so finding it proves
    // the header made the round trip through the CGI environment.
    assert!(body.contains("a-value"), "{body}");
}

#[test]
fn an_application_error_is_a_500_and_the_worker_survives() {
    let server = server!(2);

    // The example worker's `/boom` route throws, and the SDK turns an uncaught
    // throwable into a 500 while keeping the worker alive — which is the property
    // that gives worker mode its value.
    let response = get(&server.address, "/boom");

    assert_eq!(response.status, 500, "{response:?}");
    let body = response.text();
    assert!(body.contains("Internal Server Error"), "{body}");

    // And the same worker must still serve the next request: a failing request is
    // not a failing process.
    let after = get(&server.address, "/health");
    assert_eq!(after.status, 200, "{after:?}");
}

#[test]
fn a_missing_script_is_a_404_without_troubling_the_backend() {
    // No front controller, so only a real file can be served. Two things make this
    // a 404 rather than a gateway error: the routing layer resolves the path
    // before dispatch, and the backend here is deliberately unreachable, so a
    // backend failure could not be mistaken for it.
    let routing = Routing {
        document_root: Path::new("a-directory-that-does-not-exist").to_path_buf(),
        front_controller: None,
        ..Routing::default()
    };
    let dispatcher = Dispatcher::from_config(&unreachable_config(), routing.clone())
        .expect("a dispatcher that starts no process");

    let Some(server) = Server::start(
        dispatcher,
        routing,
        HttpLimits::default(),
        Default::default(),
    ) else {
        return;
    };
    let response = get(&server.address, "/nothing.php");

    assert_eq!(response.status, 404, "{response:?}");
    assert_eq!(response.reason, "Not Found");
    assert!(response.text().contains("not found"), "{}", response.text());
}

/// A configuration whose backend starts no process, for tests that must not need
/// `php`. An `external` backend pointing at a loopback port nothing listens on.
fn unreachable_config() -> Config {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a probe port");
    let socket = listener.local_addr().expect("address").to_string();
    drop(listener);

    Config {
        server: ServerConfig {
            http: Some("127.0.0.1:0".to_string()),
            default_backend: Some("fpm".to_string()),
            routes: Vec::new(),
            ..Default::default()
        },
        backends: [(
            "fpm".to_string(),
            BackendConfig::External(volucer_core::config::ExternalConfig {
                socket,
                max_connections: None,
            }),
        )]
        .into_iter()
        .collect(),
        sites: BTreeMap::new(),
        ..volucer_core::config::Config::default()
    }
}

#[test]
fn an_unreachable_backend_is_a_502() {
    let routing = front_controller_routing();
    let dispatcher = Dispatcher::from_config(&unreachable_config(), routing.clone())
        .expect("a dispatcher that starts no process");

    let Some(server) = Server::start(
        dispatcher,
        routing,
        HttpLimits::default(),
        Default::default(),
    ) else {
        return;
    };

    // The path routes to the front controller, so the failure is the backend being
    // unreachable rather than the script being missing.
    let response = get(&server.address, "/health");

    assert_eq!(response.status, 502, "{response:?}");
    assert_eq!(response.reason, "Bad Gateway");
}

#[test]
fn an_oversized_body_is_refused_with_a_413() {
    let config = unreachable_config();
    let routing = front_controller_routing();
    let dispatcher = Dispatcher::from_config(&config, routing.clone()).expect("a dispatcher");

    // A tiny limit, so the refusal is observable without sending megabytes.
    let limits = HttpLimits {
        max_body_bytes: 64,
        ..HttpLimits::default()
    };
    let Some(server) = Server::start(dispatcher, routing, limits, Default::default()) else {
        return;
    };

    let payload = "x".repeat(256);
    let request = format!(
        "POST /echo HTTP/1.1\r\nHost: example.test\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
        payload.len()
    );
    let response = exchange(&server.address, request.as_bytes());

    // The limit is enforced while the body is read, so this is refused before any
    // backend is reached — which is why an unreachable backend does not turn it
    // into a 502.
    assert_eq!(response.status, 413, "{response:?}");
}

#[test]
fn concurrent_requests_are_served_in_parallel() {
    let server = server!(4);

    // Four workers means four slots, so four simultaneous requests can all be in
    // flight at once. If the front-end serialised them behind a single blocking
    // call, the total time would be roughly the sum of the parts. The assertion is
    // deliberately loose — a smoke test for the boundary, not a benchmark.
    let started = std::time::Instant::now();
    let mut handles = Vec::new();
    for _ in 0..4 {
        let address = server.address.clone();
        handles.push(std::thread::spawn(move || get(&address, "/health")));
    }
    let responses: Vec<Exchange> = handles
        .into_iter()
        .map(|handle| handle.join().expect("a client thread"))
        .collect();

    for response in &responses {
        assert_eq!(response.status, 200, "a concurrent request failed");
    }
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "four parallel requests took {:?}",
        started.elapsed()
    );
}

#[test]
fn an_upgrade_h2c_offer_is_answered_in_http_1_1() {
    // Pinned as a *decision* rather than as a limitation. `hyper-util`'s `auto::Builder` only
    // honours an `Upgrade` through `serve_connection_with_upgrades`; `listener.rs` calls plain
    // `serve_connection`, and the choice is deliberate: prior knowledge is what a real reverse
    // proxy uses for h2c, and a browser-facing listener gets HTTP/2 over ALPN instead.
    //
    // The gap was pre-existing and unrecorded. A test is what makes it a decision, because a
    // `hyper` upgrade that started honouring the offer would otherwise change this silently.
    let server = server!(2);

    let request = concat!(
        "GET /health HTTP/1.1\r\n",
        "Host: example.test\r\n",
        "Connection: Upgrade, HTTP2-Settings\r\n",
        "Upgrade: h2c\r\n",
        "HTTP2-Settings: AAMAAABkAAQAAP__\r\n",
        "Connection: close\r\n",
        "\r\n",
    );
    let response = exchange(&server.address, request.as_bytes());

    assert_eq!(response.status, 200, "{response:?}");
    // The half that matters: no `101` and no upgrade accepted. A `101` would mean the offer was
    // taken, and the response would then be HTTP/2 rather than the HTTP/1.1 every other test in
    // this file assumes.
    assert_ne!(
        response.status, 101,
        "an Upgrade: h2c offer must not be accepted: {response:?}"
    );
    assert!(
        response.header("upgrade").is_none(),
        "no upgrade header may be returned: {response:?}"
    );
}
#[test]
fn http_1_1_keep_alive_serves_two_requests_on_one_connection() {
    let server = server!(2);

    let mut stream = TcpStream::connect(&server.address).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .expect("a read timeout");

    // The first request asks for keep-alive, so the server must answer it and then
    // stay open for the second.
    stream
        .write_all(b"GET /health HTTP/1.1\r\nHost: example.test\r\n\r\n")
        .expect("write the first request");
    stream.flush().expect("flush");

    let mut reader = BufReader::new(stream.try_clone().expect("clone"));
    assert_eq!(read_one_response(&mut reader), 200);

    stream
        .write_all(b"GET /health HTTP/1.1\r\nHost: example.test\r\nConnection: close\r\n\r\n")
        .expect("write the second request");
    stream.flush().expect("flush");

    assert_eq!(
        read_one_response(&mut reader),
        200,
        "the second request on a kept-alive connection"
    );
}

/// Read one HTTP response, leaving the stream positioned after it.
///
/// A `Content-Length` response is read exactly, so a second request can follow on
/// the same connection — which is the point of the test that uses this.
fn read_one_response(reader: &mut BufReader<TcpStream>) -> u16 {
    let mut line = String::new();
    reader.read_line(&mut line).expect("a status line");
    let status = line
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or_else(|| panic!("no status in {line:?}"));

    let mut length: Option<usize> = None;
    loop {
        let mut header = String::new();
        reader.read_line(&mut header).expect("a header line");
        let header = header.trim_end();
        if header.is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                length = value.trim().parse().ok();
            }
        }
    }

    let length = length.expect("a content-length on a keep-alive response");
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body).expect("the body");

    status
}

// --------------------------------------------------------- static content
//
// The static layer needs no PHP at all, which is most of the point: these tests
// start a listener over an *unreachable* backend, so a request answered correctly
// can only have been answered by the file server. A request for a path that names
// no file must still reach the backend, and that is what the 502s below prove.

/// A document root with the files a static test needs.
fn static_root(name: &str) -> PathBuf {
    let root =
        std::env::temp_dir().join(format!("volucer-http-static-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("assets")).expect("create the root");
    std::fs::write(root.join("index.html"), "<h1>home</h1>").expect("write index.html");
    std::fs::write(root.join("assets/app.css"), "body { margin: 0 }").expect("write app.css");
    // A directory needs an index for the trailing-slash redirect to mean
    // anything: without one there is nothing to serve at `/assets/`.
    std::fs::write(root.join("assets/index.html"), "<h1>assets</h1>").expect("write assets index");
    std::fs::write(root.join("assets/app.js"), "console.log(1)").expect("write app.js");
    std::fs::write(root.join("assets/app.js.br"), "brotli-bytes").expect("write app.js.br");
    std::fs::write(root.join("assets/logo.svg"), "<svg/>").expect("write logo.svg");
    std::fs::write(root.join(".env"), "SECRET=1").expect("write a dotfile");
    // A script, so a test can prove this layer refuses to hand one over. Its
    // contents are what a client would receive if that ever regressed.
    std::fs::write(root.join("index.php"), "<?php echo 'SECRET-SOURCE';")
        .expect("write a front controller");
    std::fs::write(root.join("big.bin"), vec![b'x'; 256 * 1024]).expect("write a binary file");
    root
}

/// A server that serves files from `root`, with a backend nothing can reach.
fn static_server(root: &Path, config: volucer_core::config::StaticConfig) -> Option<Server> {
    let routing = Routing {
        document_root: root.to_path_buf(),
        front_controller: Some("index.php".to_string()),
        ..Routing::default()
    };
    let dispatcher = Dispatcher::from_config(&unreachable_config(), routing.clone())
        .expect("a dispatcher that starts no process");

    Server::start(dispatcher, routing, HttpLimits::default(), config)
}

/// The single default site a test's routing and static settings describe.
///
/// The listener takes a [`Sites`] table because that is what `serve` builds, and a test
/// exercising the same path should build one the same way rather than through a side door.
fn sites_for(routing: &Routing, statics: volucer_core::config::StaticConfig) -> Sites {
    let config = volucer_core::config::Config::from_toml(&toml_for(routing, &statics))
        .expect("the test's own configuration parses");
    Sites::from_config(&config, &routing.document_root)
}

fn toml_for(routing: &Routing, statics: &volucer_core::config::StaticConfig) -> String {
    let root = routing.document_root.to_string_lossy();
    // Everything lives on `[server]` — the default site — except "files only", which the
    // global `front_controller` cannot spell: it reserves the empty value, because TOML
    // cannot tell `""` from an absent key. A site may say `false`, so the one case that
    // needs it is expressed as a site naming the host these tests send.
    let files_only = match routing.front_controller.as_deref() {
        Some(path) => format!("front_controller = {path:?}\n"),
        None => String::new(),
    };
    let site = match routing.front_controller {
        Some(_) => String::new(),
        None => format!(
            "\n[sites.test]\nhosts = [\"example.test\", \"localhost\"]\n\
             document_root = {root:?}\nfront_controller = false\n"
        ),
    };
    let text = format!(
        "[server]\nhttp = \"127.0.0.1:0\"\ndocument_root = {root:?}\n{files_only}\
         static_files = {{ enabled = {enabled}, precompressed = {precompressed}, \
         directory_index = {directory_index} }}\n{site}",
        enabled = statics.enabled,
        precompressed = statics.precompressed,
        directory_index = statics.directory_index,
    );
    text
}

/// The static configuration a test wants, with serving switched on.
fn static_config() -> volucer_core::config::StaticConfig {
    volucer_core::config::StaticConfig {
        enabled: true,
        ..Default::default()
    }
}

/// A request with arbitrary headers, ending at EOF.
fn request_with(address: &str, request: &str) -> Exchange {
    exchange(address, request.as_bytes())
}

#[test]
fn a_file_is_served_without_a_backend() {
    let root = static_root("serve");
    let Some(server) = static_server(&root, static_config()) else {
        return;
    };

    let response = get(&server.address, "/assets/app.css");

    assert_eq!(response.status, 200, "{response:?}");
    assert_eq!(response.text(), "body { margin: 0 }");
    assert_eq!(
        response.header("content-type"),
        Some("text/css; charset=utf-8")
    );
    assert_eq!(
        response.header("content-length"),
        Some(response.body.len().to_string().as_str())
    );
    assert_eq!(response.header("accept-ranges"), Some("bytes"));
    assert_eq!(
        response.header("cache-control"),
        Some("public, max-age=3600")
    );
    assert!(
        response.header("etag").is_some(),
        "a file needs a validator to be cacheable"
    );
    assert!(response.header("last-modified").is_some());

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_path_that_names_no_file_still_reaches_the_backend() {
    let root = static_root("fallthrough");
    let Some(server) = static_server(&root, static_config()) else {
        return;
    };

    // `/about` is not a file, so it goes to the front controller — and the
    // backend here is unreachable, which is how this test proves the request
    // really was dispatched rather than answered from the filesystem.
    let response = get(&server.address, "/about");
    assert_eq!(response.status, 502, "{response:?}");

    // A real PHP file is not the static layer's business either: it is a script,
    // and the backend decides what happens.
    let response = get(&server.address, "/index.php");
    assert_eq!(response.status, 502, "{response:?}");

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_head_request_describes_the_file_without_sending_it() {
    let root = static_root("head");
    let Some(server) = static_server(&root, static_config()) else {
        return;
    };

    let response = request_with(
        &server.address,
        "HEAD /assets/app.css HTTP/1.1\r\nHost: example.test\r\nConnection: close\r\n\r\n",
    );

    assert_eq!(response.status, 200, "{response:?}");
    assert!(response.body.is_empty(), "a HEAD has no body");
    assert_eq!(
        response.header("content-length"),
        Some("18"),
        "but it still declares the length it would have sent"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn an_unchanged_file_is_a_304_with_no_body() {
    let root = static_root("conditional");
    let Some(server) = static_server(&root, static_config()) else {
        return;
    };

    let first = get(&server.address, "/assets/app.css");
    let tag = first.header("etag").expect("an etag").to_string();

    let response = request_with(
        &server.address,
        &format!(
            "GET /assets/app.css HTTP/1.1\r\nHost: example.test\r\nIf-None-Match: {tag}\r\nConnection: close\r\n\r\n"
        ),
    );

    assert_eq!(response.status, 304, "{response:?}");
    assert!(response.body.is_empty(), "a 304 carries no body");

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_range_request_returns_just_those_bytes() {
    let root = static_root("range");
    let Some(server) = static_server(&root, static_config()) else {
        return;
    };

    let response = request_with(
        &server.address,
        "GET /big.bin HTTP/1.1\r\nHost: example.test\r\nRange: bytes=0-9\r\nConnection: close\r\n\r\n",
    );

    assert_eq!(response.status, 206, "{response:?}");
    assert_eq!(response.header("content-range"), Some("bytes 0-9/262144"));
    assert_eq!(response.header("content-length"), Some("10"));
    assert_eq!(response.body, vec![b'x'; 10]);

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_range_past_the_end_is_a_416() {
    let root = static_root("unsatisfiable");
    let Some(server) = static_server(&root, static_config()) else {
        return;
    };

    let response = request_with(
        &server.address,
        "GET /assets/app.css HTTP/1.1\r\nHost: example.test\r\nRange: bytes=9999-\r\nConnection: close\r\n\r\n",
    );

    assert_eq!(response.status, 416, "{response:?}");
    assert_eq!(
        response.header("content-range"),
        Some("bytes */18"),
        "the client is told what would have worked"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_directory_without_its_slash_redirects() {
    let root = static_root("redirect");
    let Some(server) = static_server(&root, static_config()) else {
        return;
    };

    let response = get(&server.address, "/assets");

    assert_eq!(response.status, 301, "{response:?}");
    assert_eq!(
        response.header("location"),
        Some("//example.test/assets/"),
        "the client's own Host is used, so the redirect stays on it"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_dotfile_is_not_served_and_is_not_reported_as_missing() {
    let root = static_root("dotfile");
    let Some(server) = static_server(&root, static_config()) else {
        return;
    };

    let response = get(&server.address, "/.env");

    // 404 rather than 403: whether a file exists is not a client's business, and
    // a 403 would confirm it is there.
    assert_eq!(response.status, 404, "{response:?}");

    let _ = std::fs::remove_dir_all(&root);
}

/// With static serving **off**, a non-script file must not reach PHP either.
///
/// The backend here is unreachable, so a 502 is the proof that routing declined
/// to use the file as a script. Before this was fixed, routing matched *any*
/// existing file and handed it to php-cgi, which does not check that a script is
/// PHP: given `/.env` or `/config.json` it emitted the contents verbatim with a
/// 200. The static layer's own refusals do not cover this, because the request
/// never reached it — `POST` never does, and nor does anything at all when the
/// layer is disabled.
#[test]
fn a_non_script_file_is_not_dispatched_to_php() {
    let root = static_root("nonscript");
    // The fixture has `.env`; add a plain non-PHP data file next to it.
    std::fs::write(root.join("config.json"), "{\"secret\":1}").expect("write config.json");

    let Some(server) = static_server(&root, Default::default()) else {
        return;
    };

    for path in ["/config.json", "/assets/app.css"] {
        let response = get(&server.address, path);
        assert_eq!(
            response.status, 502,
            "{path} must reach the backend as application input, not be read as a script: {response:?}"
        );
        assert!(
            !response.text().contains("secret") && !response.text().contains("SECRET"),
            "{path} leaked file contents: {response:?}"
        );
    }

    // A dotfile is hidden outright, not passed on as input.
    let response = get(&server.address, "/.env");
    assert_eq!(response.status, 404, "{response:?}");
    assert!(
        !response.text().contains("SECRET"),
        "the dotfile's contents leaked: {response:?}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_traversal_is_refused() {
    let root = static_root("traversal");
    let Some(server) = static_server(&root, static_config()) else {
        return;
    };

    let response = get(&server.address, "/../Cargo.toml");
    assert!(
        response.status == 403 || response.status == 404,
        "a traversal must not be served: {response:?}"
    );
    assert!(
        !response.text().contains("volucer"),
        "and must not leak a file above the root"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_precompressed_variant_is_served_when_it_is_accepted() {
    let root = static_root("precompressed");
    let config = volucer_core::config::StaticConfig {
        enabled: true,
        precompressed: true,
        ..Default::default()
    };
    let Some(server) = static_server(&root, config) else {
        return;
    };

    let response = request_with(
        &server.address,
        "GET /assets/app.js HTTP/1.1\r\nHost: example.test\r\nAccept-Encoding: gzip, br\r\nConnection: close\r\n\r\n",
    );

    assert_eq!(response.status, 200, "{response:?}");
    assert_eq!(response.text(), "brotli-bytes");
    assert_eq!(response.header("content-encoding"), Some("br"));
    assert_eq!(
        response.header("vary"),
        Some("accept-encoding"),
        "a cache has to key on the encoding"
    );

    // A client that does not accept it gets the original.
    let response = request_with(
        &server.address,
        "GET /assets/app.js HTTP/1.1\r\nHost: example.test\r\nConnection: close\r\n\r\n",
    );
    assert_eq!(response.text(), "console.log(1)");
    assert_eq!(response.header("content-encoding"), None);

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn static_serving_is_inert_until_it_is_enabled() {
    let root = static_root("disabled");
    // The same document root, but serving switched off: every request must reach
    // the backend, exactly as it did before this layer existed.
    let Some(server) = static_server(&root, Default::default()) else {
        return;
    };

    let response = get(&server.address, "/assets/app.css");
    assert_eq!(
        response.status, 502,
        "a disabled static layer must not answer at all: {response:?}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_large_file_arrives_whole() {
    let root = static_root("large");
    let Some(server) = static_server(&root, static_config()) else {
        return;
    };

    // 256 KiB is four times the streaming block size, so this crosses the
    // boundary where a hand-written body would drop or duplicate a block.
    let response = get(&server.address, "/big.bin");

    assert_eq!(response.status, 200, "{response:?}");
    assert_eq!(response.body.len(), 256 * 1024);
    assert!(response.body.iter().all(|byte| *byte == b'x'));

    let _ = std::fs::remove_dir_all(&root);
}

/// A script must never be handed over as a file.
///
/// This is the one failure in this layer that is a *disclosure* rather than a
/// wrong answer, and it is silent by nature: PHP source is a valid body, so the
/// only symptom is that the client received the program instead of its output.
/// The backend here is unreachable, so a 502 is the proof that the layer
/// declined and left the decision to the layer that executes scripts.
#[test]
fn a_php_script_is_never_served_as_a_file() {
    let root = static_root("phpguard");
    let Some(server) = static_server(&root, static_config()) else {
        return;
    };

    let response = get(&server.address, "/index.php");

    assert_eq!(
        response.status, 502,
        "a .php path must reach the backend, never the file server: {response:?}"
    );
    assert!(
        !response.text().contains("SECRET-SOURCE"),
        "the script's source was disclosed: {response:?}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// The extension rule is the routing layer's, not a second list.
///
/// A path that names a script *and* then more path is the `PATH_INFO` case, and
/// it is the one nginx users rely on every day; it has to reach PHP too.
#[test]
fn a_php_script_with_path_info_still_reaches_the_backend() {
    let root = static_root("phpinfo");
    let Some(server) = static_server(&root, static_config()) else {
        return;
    };

    let response = get(&server.address, "/index.php/deep/route");

    assert_eq!(
        response.status, 502,
        "PATH_INFO must reach the backend: {response:?}"
    );
    assert!(
        !response.text().contains("SECRET-SOURCE"),
        "the script's source was disclosed: {response:?}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// Two sites in one process: the same path answers differently per `Host`.
///
/// This is the whole feature, and it is asserted with static content because that is what
/// makes the difference visible without a backend: the document root is chosen by the
/// `Host` header before anything else is decided, so `/whoami.txt` cannot be the same file
/// for two hosts.
#[test]
fn two_sites_in_one_process_serve_different_roots() {
    let base = std::env::temp_dir().join(format!("volucer-sites-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    for (name, body) in [
        ("a", "site-a"),
        ("b", "site-b"),
        ("default", "site-default"),
    ] {
        std::fs::create_dir_all(base.join(name)).expect("create a site root");
        std::fs::write(base.join(name).join("whoami.txt"), body).expect("write the marker");
    }

    let text = format!(
        r#"
[server]
http = "127.0.0.1:0"
document_root = {default:?}
static_files = {{ enabled = true }}

[sites.a]
hosts = ["a.test", "*.a.test"]
document_root = {a:?}
static_files = {{ enabled = true }}

[sites.b]
hosts = ["b.test"]
document_root = {b:?}
static_files = {{ enabled = true }}
"#,
        default = base.join("default").to_string_lossy(),
        a = base.join("a").to_string_lossy(),
        b = base.join("b").to_string_lossy(),
    );
    let config = volucer_core::config::Config::from_toml(&text).expect("config parses");
    let sites = Arc::new(Sites::from_config(&config, &base.join("default")));
    let routing = Routing {
        document_root: base.join("default"),
        front_controller: Some("index.php".to_string()),
        ..Routing::default()
    };
    let dispatcher =
        Dispatcher::from_config(&unreachable_config(), routing).expect("no process is started");

    let Some(server) = Server::start_sites(dispatcher, sites, HttpLimits::default()) else {
        return;
    };
    let address = server.address.clone();

    let answer = |host: &str| {
        let response = get_as(&address, host, "/whoami.txt");
        assert_eq!(response.status, 200, "{host}: {response:?}");
        response.text()
    };

    assert_eq!(answer("a.test"), "site-a");
    assert_eq!(answer("b.test"), "site-b");
    // The wildcard belongs to `a`, and is matched without regard to case or port.
    assert_eq!(answer("x.a.test"), "site-a");
    assert_eq!(answer("A.TEST:8080"), "site-a");
    // An unlisted host is the default site's.
    assert_eq!(answer("elsewhere.test"), "site-default");

    // A path naming no file belongs to the matching site's front controller, which is
    // unreachable here — 502 rather than a 404 or another site's content.
    let missing = get_as(&address, "b.test", "/no-such-file");
    assert_eq!(missing.status, 502, "{missing:?}");

    drop(server);
    let _ = std::fs::remove_dir_all(&base);
}

/// A reload changes what the next request is served, with no restart and no dropped listener.
///
/// This is the end-to-end claim the whole reload mechanism exists to make, so it is asserted
/// the way a client would see it: the *same* address and the *same* listener, answering from a
/// different document root after a reload — not by inspecting the state that was installed.
#[test]
fn a_reload_changes_what_the_next_request_is_served() {
    let base = std::env::temp_dir().join(format!("volucer-reload-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let before = base.join("before");
    let after = base.join("after");
    for root in [&before, &after] {
        std::fs::create_dir_all(root).expect("create a site root");
    }
    std::fs::write(before.join("whoami.txt"), "before").expect("write the marker");
    std::fs::write(after.join("whoami.txt"), "after").expect("write the marker");

    // Two configs differing in exactly one setting, which is the control: the reload has to
    // be attributable to the document root rather than to everything about the config.
    let config_for = |root: &Path| {
        let text = format!(
            r#"
[server]
http = "127.0.0.1:0"
document_root = {root:?}
static_files = {{ enabled = true }}
"#,
            root = root.to_string_lossy(),
        );
        let config = volucer_core::config::Config::from_toml(&text).expect("config parses");
        config.validate().expect("config validates");
        config
    };

    let first = config_for(&before);
    let sites = Arc::new(Sites::from_config(&first, &before));
    let dispatcher = Dispatcher::from_config(&unreachable_config(), front_controller_routing())
        .expect("no process is started");

    let Some(server) = Server::start_sites(dispatcher, sites, HttpLimits::default()) else {
        return;
    };
    let address = server.address.clone();

    assert_eq!(
        get_as(&address, "any.test", "/whoami.txt").text(),
        "before",
        "the first request must be served from the starting root"
    );

    // Replace the site table. The listener keeps running: nothing is rebound, and the accept
    // loop is never told anything happened.
    let second = config_for(&after);
    let replaced = server.reload.apply(
        Sites::from_config(&second, &after),
        std::sync::Arc::new(volucer_http::GrpcGrantsMap::new()),
    );

    // The handle reports what it actually replaced, which is what lets a caller log a change
    // rather than only that a reload was attempted.
    assert_eq!(
        replaced.sites().default_site().document_root,
        before,
        "apply must report the state it replaced, not the one it installed"
    );

    assert_eq!(
        get_as(&address, "any.test", "/whoami.txt").text(),
        "after",
        "the next request must be served from the new root, on the same listener"
    );

    drop(server);
    let _ = std::fs::remove_dir_all(&base);
}

/// A request that is already in flight keeps the configuration it started under.
///
/// The counterpart to the test above, and the reason a snapshot is taken once per request:
/// without it a request could resolve its site against one table and route against another,
/// which would surface as a site serving the wrong document root — once, under load, and
/// blamed on the filesystem.
#[test]
fn a_snapshot_survives_a_reload_landing_mid_request() {
    // Driven through the state type directly rather than through a socket, because the
    // assertion is about *which value a holder sees*, and a race with a real reload cannot be
    // scheduled from a test. The socket-level behaviour is the test above.
    let base = std::env::temp_dir().join(format!("volucer-snapshot-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).expect("create a root");

    let config_for = |marker: &str| {
        std::fs::create_dir_all(base.join(marker)).expect("create a site root");
        let text = format!(
            r#"
[server]
http = "127.0.0.1:0"
document_root = {root:?}
static_files = {{ enabled = true }}
"#,
            root = base.join(marker).to_string_lossy(),
        );
        let config = volucer_core::config::Config::from_toml(&text).expect("config parses");
        Sites::from_config(&config, &base.join(marker))
    };

    let first = Arc::new(config_for("one"));
    let state = volucer_http::Swappable::new(volucer_http::Reloaded::from_sites(
        first,
        8080,
        std::sync::Arc::new(volucer_http::GrpcGrantsMap::new()),
    ));
    // What a request does: take one snapshot and hold it.
    let in_flight = state.load();
    let previous = state.replace(volucer_http::Reloaded::from_sites(
        Arc::new(config_for("two")),
        8080,
        std::sync::Arc::new(volucer_http::GrpcGrantsMap::new()),
    ));

    assert_eq!(
        in_flight.sites().default_site().document_root,
        base.join("one"),
        "the in-flight request must keep the root it started with"
    );
    assert_eq!(
        previous.sites().default_site().document_root,
        base.join("one"),
        "the replaced state is the one the request is still using"
    );
    assert_eq!(
        state.load().sites().default_site().document_root,
        base.join("two"),
        "the next request takes the new root"
    );

    let _ = std::fs::remove_dir_all(&base);
}

/// A reload landing *while requests are in flight* is still one configuration per request.
///
/// The sequential test above pins that a reload reaches the *next* request, and the held-snapshot
/// test under it pins that a request keeps the generation it started under. Neither covers what
/// this one does, and the gap is not academic: the state a front-end serves from is a site table,
/// a routing map and a static layer, and it is **one** `Swappable<Reloaded>` rather than three
/// `Swappable`s because three swaps are not one snapshot — a request could take the new routing
/// map and the old site table, resolve a site the map is not for, and be served from the wrong
/// document root. That failure would appear once, under load, and be blamed on the filesystem.
///
/// So the assertion is the invariant, sampled while a reload is being applied: the site a sample
/// resolves, its routing and its static layer must all name the same document root. The two
/// generations are built so that **each of the three is individually identifiable** — the
/// default site's root, the named site's root, and the static layer's root are three different
/// directories — because an invariant between three values that are always equal cannot fail.
///
/// The sampling technique is validated by `three_separate_swaps_be_observed_mid_generation` in
/// `volucer-watch`, which runs the same loop against the three parts *deliberately* separated and
/// observes the disagreement. Without that, a clean run here would mean nothing.
///
/// Two things run at once: requesters driving a real listener, and samplers reading the handle.
/// The reload happens once, between them.
#[test]
fn a_reload_under_concurrent_requests_is_one_configuration_per_request() {
    let base = std::env::temp_dir().join(format!("volucer-concurrent-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let first = base.join("first");
    let second = base.join("second");
    // The default site's root, the named site's root and the static layer's root are all distinct
    // — `first`, `first/app` and the file that is only in one of them — so a sample that mixed two
    // generations would have to disagree about something rather than about nothing.
    for root in [&first, &second] {
        std::fs::create_dir_all(root.join("app")).expect("create a site root");
        std::fs::write(
            root.join("app/whoami.txt"),
            root.file_name().unwrap().to_string_lossy().to_string(),
        )
        .expect("write the marker");
    }

    let config_for = |root: &Path| {
        let text = format!(
            r#"
[server]
http = "127.0.0.1:0"
document_root = {root:?}
static_files = {{ enabled = true }}

[sites.app]
hosts = ["app.test"]
document_root = {app:?}
"#,
            root = root.to_string_lossy(),
            app = root.join("app").to_string_lossy(),
        );
        let config = volucer_core::config::Config::from_toml(&text).expect("config parses");
        config.validate().expect("config validates");
        config
    };

    let one = config_for(&first);
    let sites = Arc::new(Sites::from_config(&one, &first));
    let dispatcher = Dispatcher::from_config(&unreachable_config(), front_controller_routing())
        .expect("no process is started");

    let Some(server) = Server::start_sites(dispatcher, sites, HttpLimits::default()) else {
        return;
    };
    let address = server.address.clone();
    let reload = server.reload.clone();

    let stop = std::sync::atomic::AtomicBool::new(false);
    let bodies = std::sync::Mutex::new(Vec::new());
    let samples = std::sync::atomic::AtomicUsize::new(0);

    std::thread::scope(|scope| {
        // The invariant sampler. Every trip reads a site, its routing and its static layer *from
        // one snapshot* and checks they describe the same place. A mixture is the only way those
        // three can disagree, and they can only disagree if the one pointer becomes three.
        for _ in 0..2 {
            let reload = reload.clone();
            let stop = &stop;
            let samples = &samples;
            scope.spawn(move || {
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    let snapshot = reload.current();
                    for host in [Some("app.test"), None] {
                        let resolved = snapshot.resolve(host);
                        let site = resolved.site.document_root.clone();
                        assert_eq!(
                            resolved.routing.document_root, site,
                            "the routing came from a different generation than the site it \
                             resolved: a request would stat one root and render another"
                        );
                        assert_eq!(
                            resolved.statics.document_root, site,
                            "the static layer came from a different generation than the site it \
                             resolved: a request would resolve one root and read another"
                        );
                    }
                    samples.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
            });
        }

        // Real requests on the same listener, throughout. What these add is that the reload does
        // not *break* a request: a rebuild that left a map without an entry for the site it
        // resolved would fail here rather than in a sampler.
        for _ in 0..4 {
            let address = address.clone();
            let stop = &stop;
            let bodies = &bodies;
            scope.spawn(move || {
                let mut local = Vec::new();
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    let answer = get_as(&address, "app.test", "/whoami.txt");
                    local.push((answer.status, answer.text()));
                }
                bodies.lock().expect("not poisoned").extend(local);
            });
        }

        // Let every reader get into its loop, then apply the reload between them.
        std::thread::sleep(Duration::from_millis(100));
        let _replaced = reload.apply(
            Sites::from_config(&config_for(&second), &second),
            std::sync::Arc::new(volucer_http::GrpcGrantsMap::new()),
        );
        std::thread::sleep(Duration::from_millis(100));
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
    });

    let samples = samples.load(std::sync::atomic::Ordering::Relaxed);
    assert!(
        samples > 100,
        "the sampler must actually have raced the reload, or this asserts nothing: {samples} \
         sample(s)"
    );

    let bodies = bodies.into_inner().expect("not poisoned");
    assert!(
        bodies.len() > 20,
        "the fixture must actually have run requests across the reload: {} answer(s)",
        bodies.len()
    );
    for (status, body) in &bodies {
        assert_eq!(
            *status, 200,
            "a reload must not break a request in flight: {body:?}"
        );
        assert!(
            body == "first" || body == "second",
            "a request was served body {body:?}, which neither generation's document root holds \
             — the site resolved against one generation and served from another"
        );
    }

    // And the reload took effect, which is what keeps the assertions above from passing against a
    // reload that applied nothing at all — a vacuous assertion.
    assert_eq!(
        get_as(&address, "app.test", "/whoami.txt").text(),
        "second",
        "the reload must be in effect by the end, or the invariants above prove nothing"
    );

    drop(server);
    let _ = std::fs::remove_dir_all(&base);
}

// ---------------------------------------------------------------------------
// The upstream request path: streaming, the body cap, and concurrency.
//
// These live here rather than in `volucer`'s CLI tests because they need an in-process listener
// with a lowered `HttpLimits` and an origin whose behaviour a test can change — and because an
// upstream needs no PHP, so unlike most of this file they always run rather than skipping.
// ---------------------------------------------------------------------------

use std::net::TcpListener as OriginListener;
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

/// How a test origin answers every request.
#[derive(Clone, Copy, PartialEq, Eq)]
enum OriginBehaviour {
    /// Sleep before answering, so concurrent requests can be told from ones served a thread at a
    /// time.
    SlowAnswer { pause_ms: u64 },
    /// Write one chunk, sleep, write another, then a terminator. Proves the proxy did not buffer:
    /// a buffered one cannot deliver the first chunk before the second exists.
    TwoChunks { pause_ms: u64 },
    /// Read the whole request body and answer with its length. Proves the body was not truncated.
    EchoLength,
    /// Answer with the `traceparent` the request carried, or `-` when it carried none. The one
    /// fixture that can tell "the proxy forwarded the client's header" from "the proxy wrote its
    /// own", which is the whole of `[server].trace_context`.
    EchoTraceparent,
}

/// A test origin: a stdlib listener that answers every request the same way.
struct Origin {
    address: String,
    seen: Arc<AtomicUsize>,
    stop: Arc<std::sync::atomic::AtomicBool>,
}

impl Origin {
    fn start(behaviour: OriginBehaviour) -> Origin {
        let listener = OriginListener::bind("127.0.0.1:0").expect("bind an origin");
        listener.set_nonblocking(true).expect("non-blocking accept");
        let address = listener.local_addr().expect("address").to_string();
        let seen = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));

        let counted = Arc::clone(&seen);
        let stopped = Arc::clone(&stop);
        std::thread::spawn(move || {
            // Polling rather than blocking, so the thread ends when the test does instead of
            // holding the process open. 5 ms is invisible to these assertions.
            while !stopped.load(AtomicOrdering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        // One thread per connection. **Not an optimisation** — an accept loop that
                        // served each connection inline would serialise the *origin*, and the test
                        // that measures whether volucer serialises would then be measuring this
                        // fixture instead. That is exactly what it did first: five 400 ms requests
                        // took 2000 ms, which looked like a stalled front-end and was a
                        // single-threaded origin.
                        let counted = Arc::clone(&counted);
                        std::thread::spawn(move || serve_origin(stream, behaviour, &counted));
                    }
                    Err(ref err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => return,
                }
            }
        });

        Origin {
            address,
            seen,
            stop,
        }
    }

    fn request_count(&self) -> usize {
        self.seen.load(AtomicOrdering::Relaxed)
    }
}

impl Drop for Origin {
    fn drop(&mut self) {
        self.stop.store(true, AtomicOrdering::Relaxed);
    }
}

/// Serve one request according to the behaviour, then close.
fn serve_origin(mut stream: TcpStream, behaviour: OriginBehaviour, seen: &Arc<AtomicUsize>) {
    // **Explicitly blocking, because the listener is not.** On Windows an accepted socket inherits
    // the listener's non-blocking mode, so every read and write here would return `WouldBlock` —
    // which surfaced as a partial response and, one layer up, as "client error (SendRequest)" from
    // the proxy. On Unix accepted sockets are already blocking, so this is a no-op there and the
    // reason it is stated rather than assumed.
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
    seen.fetch_add(1, AtomicOrdering::Relaxed);

    match behaviour {
        OriginBehaviour::SlowAnswer { pause_ms } => {
            let _ = read_request(&mut stream, false);
            std::thread::sleep(Duration::from_millis(pause_ms));
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nslow!");
            let _ = stream.flush();
        }
        OriginBehaviour::TwoChunks { pause_ms } => {
            let _ = read_request(&mut stream, false);
            let _ = stream.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: \
                  chunked\r\n\r\n",
            );
            let _ = stream.write_all(b"6\r\nfirst\n\r\n");
            let _ = stream.flush();
            std::thread::sleep(Duration::from_millis(pause_ms));
            let _ = stream.write_all(b"7\r\nsecond\n\r\n0\r\n\r\n");
            let _ = stream.flush();
        }
        OriginBehaviour::EchoLength => {
            let raw = read_request(&mut stream, true);
            // The *body*, not everything read off the socket: reporting the head's length too would
            // be a number the test cannot predict, which is also how a fixture bug hides a real
            // truncation.
            let body = request_body(&raw);
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
        OriginBehaviour::EchoTraceparent => {
            let raw = read_request(&mut stream, false);
            let head = String::from_utf8_lossy(&raw).to_ascii_lowercase();
            // The *last* `traceparent` line, not the first: a proxy that appended beside the
            // client's header would put its own after it, and reading the first would then report
            // the client's while the origin actually saw both. Asserting on the last is stricter
            // and is what the replacement rule promises.
            let traceparent = head
                .lines()
                .filter_map(|line| line.strip_prefix("traceparent:"))
                .map(|value| value.trim().to_string())
                .next_back()
                .unwrap_or_else(|| "-".to_string());
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{traceparent}",
                traceparent.len()
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    }
    let _ = stream.shutdown(std::net::Shutdown::Both);
}

/// Read a request, and optionally its body, without knowing the length in advance.
fn read_request(stream: &mut TcpStream, want_body: bool) -> Vec<u8> {
    let mut raw = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => raw.extend_from_slice(&chunk[..n]),
            Err(_) => break,
        }
        let Some(head_end) = raw.windows(4).position(|w| w == b"\r\n\r\n") else {
            continue;
        };
        if !want_body {
            break;
        }
        let head = String::from_utf8_lossy(&raw[..head_end]).to_ascii_lowercase();
        let expected = head
            .lines()
            .find_map(|line| line.strip_prefix("content-length:"))
            .and_then(|value| value.trim().parse::<usize>().ok())
            .unwrap_or(0);
        if raw.len() >= head_end + 4 + expected {
            return raw;
        }
    }
    raw
}

/// The body of a request read off the socket, headers excluded.
fn request_body(raw: &[u8]) -> &[u8] {
    match raw.windows(4).position(|w| w == b"\r\n\r\n") {
        Some(at) => &raw[at + 4..],
        None => &[],
    }
}

/// A configuration whose only backend is an upstream pointed at `origin`.
fn upstream_config(origin: &Origin) -> Config {
    Config {
        server: ServerConfig {
            http: Some("127.0.0.1:0".to_string()),
            default_backend: Some("node".to_string()),
            routes: Vec::new(),
            ..Default::default()
        },
        backends: [(
            "node".to_string(),
            BackendConfig::Upstream(volucer_core::config::UpstreamConfig::new(format!(
                "http://{}",
                origin.address
            ))),
        )]
        .into_iter()
        .collect(),
        sites: BTreeMap::new(),
        ..volucer_core::config::Config::default()
    }
}

/// A listener serving `origin`, with the given limits.
fn upstream_server(origin: &Origin, limits: HttpLimits) -> Option<Server> {
    let config = upstream_config(origin);
    let routing = front_controller_routing();
    let dispatcher = match Dispatcher::from_config(&config, routing.clone()) {
        Ok(dispatcher) => dispatcher,
        Err(err) => {
            eprintln!("skipping: the upstream backend could not be built: {err}");
            return None;
        }
    };
    Server::start(
        dispatcher,
        routing,
        limits,
        volucer_core::config::StaticConfig::default(),
    )
}

/// **The body cap does not apply to an upstream, and the same body to a CGI backend is still a
/// 413.**
///
/// Both halves are needed. A cap that was merely *raised* would pass a "does a big body work" test
/// for every body under the default, so the limit is lowered in this test's own `HttpLimits` — as
/// the existing CGI 413 test does — and the same body is sent down both paths.
///
/// The CGI half is the contrast that makes the bypass a decision rather than a regression: the
/// environment has to carry `CONTENT_LENGTH` before PHP will read a byte, so `read_body` cannot
/// stream there, and the limit is what keeps an unauthenticated client from allocating at will.
#[test]
fn a_body_past_the_cap_streams_to_an_origin_and_is_still_a_413_for_cgi() {
    let origin = Origin::start(OriginBehaviour::EchoLength);
    let tiny = HttpLimits {
        max_body_bytes: 64,
        ..HttpLimits::default()
    };
    let Some(server) = upstream_server(&origin, tiny) else {
        return;
    };

    // 256 bytes against a 64-byte cap. The origin's answer is the length it read, so a truncated or
    // refused body is a smaller number rather than a different status — a stronger assertion than
    // "not 413".
    let payload = "x".repeat(256);
    let request = format!(
        "POST /upload HTTP/1.1\r\nHost: example.test\r\nContent-Length: {}\r\nConnection: \
         close\r\n\r\n{payload}",
        payload.len()
    );
    let response = exchange(&server.address, request.as_bytes());
    assert_eq!(
        response.status, 200,
        "an upstream must not refuse a body for a cap its own client never set: {response:?}"
    );
    assert_eq!(
        response.text(),
        "256",
        "the origin must have received every byte: {response:?}"
    );

    drop(server);

    // The same body against the CGI path, through the same lowered limit: still refused, and
    // refused *while being read* rather than by a backend.
    let routing = front_controller_routing();
    let dispatcher = Dispatcher::from_config(&unreachable_config(), routing.clone())
        .expect("a dispatcher that starts no process");
    let Some(cgi_server) = Server::start(
        dispatcher,
        routing,
        tiny,
        volucer_core::config::StaticConfig::default(),
    ) else {
        return;
    };
    let refused = exchange(&cgi_server.address, request.as_bytes());
    assert_eq!(
        refused.status, 413,
        "the cap still guards the CGI path, which cannot stream: {refused:?}"
    );
}

/// **The response body arrives in pieces, not at the end.**
///
/// Asserted by *timing*, and nothing weaker would do: an origin that wrote two chunks and then
/// terminated would give a buffering proxy the same bytes and the same status. What a buffered
/// proxy cannot do is deliver the first chunk before the second exists — so this reads until the
/// first chunk arrives, checks the clock, and only then lets the origin finish.
///
/// The origin's pause is deliberately much larger than the assertion's margin: a flaky test here
/// would be read as "streaming broke", which is the one conclusion this must not produce by
/// accident.
#[test]
fn an_origin_that_answers_in_pieces_reaches_the_client_in_pieces() {
    let origin = Origin::start(OriginBehaviour::TwoChunks { pause_ms: 1500 });
    let Some(server) = upstream_server(&origin, HttpLimits::default()) else {
        return;
    };

    let mut stream = TcpStream::connect(&server.address).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .expect("a read timeout");
    stream
        .write_all(b"GET /events HTTP/1.1\r\nHost: example.test\r\nConnection: close\r\n\r\n")
        .expect("write");

    let started = std::time::Instant::now();
    let mut raw = Vec::new();
    let mut chunk = [0u8; 512];
    // Read until the first chunk has arrived, which is what "not buffered" means.
    while !String::from_utf8_lossy(&raw).contains("first") {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => raw.extend_from_slice(&chunk[..n]),
            Err(err) => panic!("reading the first chunk failed: {err}"),
        }
        assert!(
            started.elapsed() < Duration::from_millis(1200),
            "the origin holds its second chunk for 1500 ms, so the first arriving this late means \
             the proxy waited for the whole body: {:?}",
            String::from_utf8_lossy(&raw)
        );
    }

    let first_chunk_at = started.elapsed();
    let head = String::from_utf8_lossy(&raw).into_owned();
    assert!(
        head.starts_with("HTTP/1.1 200"),
        "the head must arrive before the body does: {head:?}"
    );
    assert!(
        !head.contains("second"),
        "the second chunk must not exist yet, or this test proves nothing: {head:?}"
    );

    // Then read the rest, which the origin writes after its pause.
    let mut rest = Vec::new();
    stream.read_to_end(&mut rest).expect("read the rest");
    let whole = format!("{head}{}", String::from_utf8_lossy(&rest));
    assert!(
        whole.contains("second"),
        "the rest must still arrive: {whole:?}"
    );
    // The timing assertion restated as the number it was: a buffered proxy would have delivered
    // both chunks together, at or after the pause.
    assert!(
        first_chunk_at < Duration::from_millis(1200),
        "the first chunk arrived at {first_chunk_at:?}, which is after the origin's pause"
    );
}

/// A slow origin is served concurrently, with more requests in flight than the runtime has threads.
///
/// The `spawn_blocking` boundary's failure mode is a **stall**, not a wrong answer — the module doc
/// says so — so a single-request test passes while the server is serialised behind one call.
///
/// **The thread count is the point, and a falsification is what found that out.** This test's first
/// version sent five requests against a two-thread runtime and *passed* when the upstream path was
/// wrapped in `spawn_blocking`: that pool is not a single slot, so the mistake is not "takes N times
/// as long" but "the runtime has no thread left to drive the other connections". `Server::start_sites`
/// builds a **two-thread** runtime, so ten requests is five times over-subscribed, and a path that
/// parks a runtime thread cannot satisfy them.
///
/// The origin sleeps 400 ms per request. Ten concurrently finish in about 400 ms; serviced a
/// runtime-thread at a time they take whole multiples of that.
#[test]
fn a_slow_origin_is_served_concurrently_rather_than_thread_at_a_time() {
    let origin = Origin::start(OriginBehaviour::SlowAnswer { pause_ms: 400 });
    let Some(server) = upstream_server(&origin, HttpLimits::default()) else {
        return;
    };
    let address = server.address.clone();

    // Deliberately far above the two-thread runtime `Server::start_sites` builds.
    const CONCURRENCY: usize = 10;

    let started = std::time::Instant::now();
    let workers: Vec<_> = (0..CONCURRENCY)
        .map(|_| {
            let address = address.clone();
            std::thread::spawn(move || get(&address, "/slow"))
        })
        .collect();
    let answers: Vec<_> = workers
        .into_iter()
        .map(|w| w.join().expect("join"))
        .collect();
    let elapsed = started.elapsed();

    for answer in &answers {
        assert_eq!(answer.status, 200, "{answer:?}");
        assert_eq!(answer.text(), "slow!");
    }
    assert_eq!(
        origin.request_count(),
        CONCURRENCY,
        "every request must have reached the origin"
    );

    // Ten 400 ms requests in parallel take about 400 ms; serviced a runtime-thread at a time they
    // take 2000 ms or more. The bound sits between the two with room on both sides, because this is
    // a smoke test for a stall rather than a benchmark.
    assert!(
        elapsed < Duration::from_millis(1600),
        "{CONCURRENCY} concurrent requests to an origin that sleeps 400 ms each took {elapsed:?}, \
         which is the shape of a front-end that parks a runtime thread per request (about 2000 ms) \
         rather than one that awaits (about 400 ms)"
    );
}

// ---------------------------------------------------------------------------
// A supervised origin: the origin's *process* is volucer's.
//
// The seam pattern applies here in its original form: every layer of
// this — the config, the supervisor, the readiness gate, the client — was tested on its own, and
// none of those tests could see that the dispatcher was building a `Supervisor` at all. So this one
// goes through the entry point a *deployment* uses: a `Config` built from TOML, a `Dispatcher`, a
// `Server`, and an HTTP request that reaches the process volucer started.
// ---------------------------------------------------------------------------

/// The port a config-only origin will use, chosen as free first.
fn free_origin_port() -> u16 {
    let listener = OriginListener::bind("127.0.0.1:0").expect("bind a probe port");
    let port = listener.local_addr().expect("address").port();
    drop(listener);
    port
}

/// A `python -m http.server` command that answers `GET /` with a directory listing.
///
/// Deliberately a *real* server rather than a fixture: the point of this test is that volucer starts
/// a command it was given and waits for it to answer, so the command has to behave like the thing a
/// deployment would name. `python -m http.server` is present on every machine this project builds
/// on, binds a port, and answers a request.
///
/// **The interpreter is a parameter rather than the word `python`**, and that is a bug this test had:
/// the guard resolved `python3` on Linux while the command said `python`, so the test ran and failed
/// with "the origin command python does not exist" — a fixture that decided whether to run using one
/// answer and then asked for a different one. The two must be the same value, or the guard is worse
/// than no guard.
///
/// The directory is quoted because `split_command` is deliberately not a shell: a space in the path
/// would otherwise split into two arguments. The config *literal* it is written into matters too,
/// and this test found that the hard way — a Windows path in a TOML **basic** string needs its
/// backslashes doubled, so `command = "…\Temp\x"` is a TOML parse error at `\T`. Single quotes make
/// it a literal string and leave the path alone, which is the spelling the example configuration
/// uses for every path for the same reason.
fn python_http_server(interpreter: &Path, port: u16, dir: &Path) -> String {
    format!(
        "{} -m http.server {port} --bind 127.0.0.1 --directory \"{}\"",
        interpreter.display(),
        dir.display()
    )
}

/// **A configured `command` is started, waited for, reached and stopped.**
///
/// Four assertions, and each is a different way for supervision to be silently absent:
///
/// 1. the process is running and reached the *point of answering* before `Server::start` returned —
///    which is the readiness gate, and without it the first request is a 502;
/// 2. a request actually reaches it, so the supervisor did not merely spawn something unrelated to
///    the backend's origin;
/// 3. dropping the server stops it — the orphan hazard, and the one that is invisible until a
///    deploy leaves a `gunicorn` holding a socket;
/// 4. the origin's *own* output is forwarded (asserted by the log rather than here, because it is a
///    `tracing` concern; what is asserted here is that its pipes did not fill up and block it).
#[test]
fn a_configured_origin_command_is_started_waited_for_reached_and_stopped() {
    let Some(interpreter) = find_python() else {
        eprintln!("skipping: no python binary on PATH");
        return;
    };

    let port = free_origin_port();
    let root =
        std::env::temp_dir().join(format!("volucer-supervised-origin-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create the document root");
    std::fs::write(root.join("served.txt"), "from the supervised origin").expect("write a file");

    let text = format!(
        r#"
[server]
default_backend = "app"
http = "127.0.0.1:0"
[backends.app]
kind = "upstream"
origin = "http://127.0.0.1:{port}"
command = '{}'
ready_timeout_ms = 20000
"#,
        python_http_server(&interpreter, port, &root)
    );
    let config = Config::from_toml(&text).expect("a supervised origin parses");
    let routing = front_controller_routing();
    // The readiness gate is inside `from_config`, so this returning at all is assertion (1): if the
    // supervisor did not wait, the first request below would race the interpreter's start.
    let dispatcher = Dispatcher::from_config(&config, routing.clone())
        .expect("the origin starts and answers before this returns");
    let Some(server) = Server::start(
        dispatcher,
        routing,
        HttpLimits::default(),
        volucer_core::config::StaticConfig::default(),
    ) else {
        panic!("the listener must start");
    };

    let address = server.address.clone();
    let answer = get(&address, "/served.txt");
    assert_eq!(answer.status, 200, "{answer:?}");
    assert!(
        answer.text().contains("from the supervised origin"),
        "the request must be served by the process volucer started: {:?}",
        answer.text()
    );

    // Assertion (3): the process is gone after shutdown, not merely unreachable. The port is the
    // observable — nothing is left holding it — which is what a leaked `gunicorn` would do.
    drop(server);
    let mut bound = false;
    for _ in 0..40 {
        match OriginListener::bind(("127.0.0.1", port)) {
            Ok(_) => {
                bound = true;
                break;
            }
            Err(_) => std::thread::sleep(Duration::from_millis(50)),
        }
    }
    assert!(
        bound,
        "the supervised origin was still holding port {port} after the server was dropped, so it \
         outlived the process that started it"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// **A configured `protocol = "h2c"` decides what the origin is actually spoken to in.**
///
/// The whole of the h2c mechanism is one line — a connector answering `Connected::negotiated_h2()` —
/// and `crates/volucer-upstream/tests/h2c.rs` already measured that hyper obeys it. What no unit test
/// in that crate could reach is whether the *configuration* ever gets there through a deployment's
/// entry point, which is the seam this suite exists for: `Config::from_toml` -> `Dispatcher` ->
/// `Server` -> a request.
///
/// Both halves are asserted against the same fixture and one config key, and the negative one is what
/// makes the positive one mean anything: a proxy that spoke h2 to every origin would pass a test that
/// only checked the h2c case, and would be broken for every ordinary HTTP origin in this tree.
///
/// The assertion is on the **first bytes on the wire**, not on a version field this server reports:
/// the question is what the origin was actually sent.
#[test]
fn a_configured_h2c_origin_is_spoken_to_in_http2_and_a_plain_one_is_not() {
    const PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";

    for (protocol, wants_h2) in [(Some("h2c"), true), (None, false)] {
        let origin = ProtocolOrigin::spawn();
        let protocol = protocol
            .map(|value| format!("protocol = {value:?}\n"))
            .unwrap_or_default();
        let text = format!(
            r#"
[server]
default_backend = "grpc"
http = "127.0.0.1:0"
[backends.grpc]
kind = "upstream"
origin = "http://{}"
{protocol}"#,
            origin.address
        );
        let config = Config::from_toml(&text).expect("a gRPC-shaped origin parses");
        let routing = front_controller_routing();
        let dispatcher =
            Dispatcher::from_config(&config, routing.clone()).expect("the client builds");
        let Some(server) = Server::start(
            dispatcher,
            routing,
            HttpLimits::default(),
            volucer_core::config::StaticConfig::default(),
        ) else {
            panic!("the listener must start");
        };

        // The call cannot complete — the fixture answers with a SETTINGS frame and nothing else —
        // because what is under test is what was *sent*. A gRPC-shaped POST, since that is the shape
        // the path exists for.
        let address = server.address.clone();
        let _ = post_grpc(&address, "/pkg.Service/Method");
        let sent = origin.first_bytes();

        if wants_h2 {
            assert!(
                sent.starts_with(PREFACE),
                "protocol = \"h2c\" must make the client send the HTTP/2 preface.\nexpected {:?}\n\
                 got      {:?}",
                String::from_utf8_lossy(PREFACE),
                String::from_utf8_lossy(&sent)
            );
        } else {
            assert!(
                sent.starts_with(b"POST /pkg.Service/Method HTTP/1.1"),
                "with no `protocol` key the origin must be spoken to in HTTP/1.1.\ngot {:?}",
                String::from_utf8_lossy(&sent)
            );
            assert!(!sent.starts_with(PREFACE), "and must not send the preface");
        }
    }
}

/// **A failed gRPC call is a `200` with the status in the trailers, not a `502`.**
///
/// The mapping that is the remaining half of a gRPC proxy, driven through the
/// deployment's entry point: a real listener, a real request, and a backend whose origin is
/// deliberately not there.
///
/// Both halves in one test, and the contrast is the point. A plain HTTP request to the same dead
/// origin *must* still be a `502` — that is the status every proxy answers and every client and
/// operator expects. A gRPC request to it must be a `200`, because a gRPC client reads the outcome
/// from the trailer section and an HTTP error status is invisible to it: it would report
/// "unexpected status 502" and the failure volucer is trying to report would be lost.
///
/// The chunked framing is asserted rather than assumed, because there is nowhere else for the
/// trailers to travel: a `200` with `content-length` and no `Transfer-Encoding` is a reply a gRPC
/// client cannot read the status out of, and it would look like a successful call with no message.
#[test]
fn a_failed_grpc_call_is_a_200_with_the_status_in_the_trailers() {
    let origin = Origin::start(OriginBehaviour::EchoLength);
    // The port is taken and then released, so nothing is listening — an origin that cannot be
    // reached, which is the failure being mapped.
    let dead = {
        let listener = OriginListener::bind("127.0.0.1:0").expect("bind a probe port");
        let port = listener.local_addr().expect("an address").port();
        drop(listener);
        port
    };

    let mut config = upstream_config(&origin);
    let Some(BackendConfig::Upstream(upstream)) = config.backends.get_mut("node") else {
        panic!("the backend is an upstream");
    };
    upstream.origin = format!("http://127.0.0.1:{dead}");

    let routing = front_controller_routing();
    let dispatcher = match Dispatcher::from_config(&config, routing.clone()) {
        Ok(dispatcher) => dispatcher,
        Err(err) => {
            eprintln!("skipping: the upstream backend could not be built: {err}");
            return;
        }
    };
    let Some(server) = Server::start(
        dispatcher,
        routing,
        HttpLimits::default(),
        volucer_core::config::StaticConfig::default(),
    ) else {
        panic!("the listener must start");
    };
    let address = server.address.clone();

    // A one-message gRPC body: compression flag 0, a big-endian length of 5, then five bytes. The
    // framing is what makes this a *gRPC* request rather than an HTTP one with an unusual content
    // type, and nothing here parses it — which is the whole of the claim.
    let call = "POST /pkg.Service/Method HTTP/1.1\r\nHost: example.test\r\n\
                Content-Type: application/grpc+proto\r\nTE: trailers\r\nContent-Length: 5\r\n\
                Connection: close\r\n\r\n\x00\x00\x00\x00\x05";
    let raw = exchange_raw(&address, call.as_bytes());
    let text = String::from_utf8_lossy(&raw);
    assert!(
        text.starts_with("HTTP/1.1 200 OK"),
        "a gRPC failure is HTTP 200, or the client cannot read the status.\nGot: {text}"
    );
    assert!(
        text.to_ascii_lowercase().contains("transfer-encoding: chunked"),
        "the trailers need chunked framing to travel in, or the status has nowhere to go.\nGot: {text}"
    );
    // The trailer fields, read out of the chunked trailer section rather than out of the head:
    // `grpc-status` in a *header* is the mistake that makes a streamed gRPC reply unanswerable.
    //
    // The head is checked **line by line** rather than by searching it for the name, because a
    // legitimate `Trailer: grpc-status, grpc-message` declaration contains the name too — and a
    // naive `contains` would either pass for the wrong reason or, as the first version of this
    // did, fail for one. What must not appear is a header *field* named `grpc-status`.
    let (head, tail) = text.split_once("\r\n0\r\n").unwrap_or((&text, ""));
    assert!(
        !head
            .lines()
            .any(|line| line.to_ascii_lowercase().starts_with("grpc-status:")),
        "the status must be a trailer, not a header — a header cannot be sent after a streamed \
         body, which is why gRPC puts it in the trailer section.\nGot: {text}"
    );
    assert!(
        tail.to_ascii_lowercase().contains("grpc-status: 14"),
        "UNAVAILABLE (14) is what an unreachable origin is, and it is the code the specification \
         lets a client retry.\nGot: {text}"
    );

    // The control: the same dead origin, the same server, a request that is *not* gRPC.
    let plain = get(&address, "/anything");
    assert_eq!(
        plain.status, 502,
        "an ordinary HTTP request to an unreachable origin is still a 502 — the gRPC mapping is a \
         different answer to a different protocol, not a change to this one: {plain:?}"
    );
    let _ = origin;
}

/// Send a raw request and return the bytes, without parsing them.
///
/// `exchange` parses into an `Exchange`, which cannot represent a trailer section — it stops at the
/// header terminator. The gRPC mapping is *about* the trailer section, so this test reads the raw
/// bytes instead, which is also how a gRPC client sees them.
fn exchange_raw(address: &str, request: &[u8]) -> Vec<u8> {
    let mut stream = TcpStream::connect(address).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .expect("a read timeout");
    stream.write_all(request).expect("write the request");
    stream.flush().expect("flush");
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).expect("read the response");
    raw
}

/// **A gRPC call reaches a PHP worker and its outcome comes back in the trailers.**
///
/// Tier B, driven through the deployment's own entry point — `Config::from_toml` -> `Dispatcher` ->
/// `Server` -> a real PHP worker process. Every part of this is a thing that can be absent while the
/// layers below it are correct, which is why it is one test rather than four:
///
/// 1. the `grpc_services` allow-list is read from the file and admits the call, so a granted service
///    reaches a worker at all;
/// 2. the body crosses the CGI environment, the worker protocol and back **unframed and unparsed** —
///    the server carries gRPC's bytes without decoding them, which is the whole claim;
/// 3. the application's `grpc-status` header becomes a **trailer**, not a header, and the reply is an
///    HTTP 200 — the construction half a gRPC proxy needs, now carrying a real application's answer;
/// 4. **an HTTP route on the same worker still works.** The two protocols share one pool, so a change
///    that made every request gRPC would pass 1–3 and break the ordinary case.
///
/// The negative case is in the same test for the reason the h2c test's is: a server that delivered
/// every call to every worker would pass the positive half and ignore the allow-list entirely.
#[test]
fn a_grpc_call_reaches_a_php_worker_and_its_status_comes_back_in_the_trailers() {
    if let Some(reason) = skip_reason() {
        report_skip(reason);
        return;
    }

    // One config, through `from_toml`, so the allow-list is read by the same code a deployment's file
    // is read by. The `example` worker serves both protocols; see its own comments.
    let script = example_worker();
    let text = format!(
        r#"
[server]
default_backend = "app"
http = "127.0.0.1:0"
[backends.app]
kind = "worker"
command = 'php "{}"'
workers = 2
grpc_services = ["greeter.Greeter"]
"#,
        script.display()
    );
    let config = Config::from_toml(&text).expect("a worker serving both protocols parses");
    config.validate().expect("a legal service name validates");
    let routing = front_controller_routing();

    // The allow-list, asserted on the configuration the test is about to serve. A grant that admitted
    // everything would pass every assertion below about the granted service.
    assert!(config.serves_grpc("app", "/greeter.Greeter/SayHello"));
    assert!(
        !config.serves_grpc("app", "/other.Service/SayHello"),
        "a service that is not listed must not be admitted"
    );

    let dispatcher = match Dispatcher::from_config(&config, routing.clone()) {
        Ok(dispatcher) => dispatcher,
        Err(err) => {
            eprintln!("skipping: the worker pool could not start: {err}");
            return;
        }
    };
    let Some(server) = Server::start(
        dispatcher,
        routing,
        HttpLimits::default(),
        volucer_core::config::StaticConfig::default(),
    ) else {
        panic!("the listener must start");
    };
    let address = server.address.clone();

    // A one-message gRPC body: compression flag 0, a big-endian length of 5, then five bytes. Nothing
    // anywhere in volucer parses this, and that is the point — the assertion below is that the bytes
    // come back, not that they mean anything.
    let body = b"\x00\x00\x00\x00\x05hello";
    let call = format!(
        "POST /greeter.Greeter/SayHello HTTP/1.1\r\nHost: example.test\r\n\
         Content-Type: application/grpc+proto\r\nTE: trailers\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n",
        body.len()
    );
    let mut raw = call.into_bytes();
    raw.extend_from_slice(body);

    let raw = exchange_raw(&address, &raw);
    let answer = String::from_utf8_lossy(&raw).into_owned();

    assert!(
        answer.starts_with("HTTP/1.1 200 OK"),
        "a gRPC reply is an HTTP 200, whatever the outcome.\nGot: {answer}"
    );
    assert!(
        answer
            .to_ascii_lowercase()
            .contains("transfer-encoding: chunked"),
        "the trailers need chunked framing to travel in.\nGot: {answer}"
    );
    // The head is checked **line by line**, because a legitimate `Trailer: grpc-status, grpc-message`
    // declaration contains the name too — a `contains` on the head would either pass for the wrong
    // reason or, as an earlier version of the tier A test did, fail for one.
    let (head, tail) = answer.split_once("\r\n0\r\n").unwrap_or((&answer, ""));
    assert!(
        !head
            .lines()
            .any(|line| line.to_ascii_lowercase().starts_with("grpc-status:")),
        "the status must be a trailer, not a header.\nGot: {answer}"
    );

    // The application answered `SayHello` with a framed message, and it arrived whole. The framing is
    // the *application's*; this asserts the bytes rather than a decoded message, because decoding one
    // would need the schema the server deliberately does not have.
    //
    // Searched for in the response's **raw bytes**, and that is not fastidiousness: the message framing
    // is `00 00 00 22`, which is not valid UTF-8, so a `String::from_utf8_lossy` rendering of it has
    // already replaced the very bytes this asserts on. (This test found that the hard way: the first
    // version hexed the lossy string and could never have passed.)
    let payload = b"hello from volucer's gRPC handler\n";
    // gRPC's inner framing, which is the whole of what makes this schema-independent: one compression
    // flag byte, then a big-endian `u32` length, then that many bytes.
    let mut framed = vec![0u8];
    framed.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    let hex = |bytes: &[u8]| -> String { bytes.iter().map(|byte| format!("{byte:02x}")).collect() };
    let prefix = hex(&framed);
    assert!(
        hex(&raw).contains(&prefix),
        "the application's framing must arrive byte for byte — one compression flag, a big-endian \
         length of {}.\nExpected the hex {prefix}\nGot: {answer}",
        payload.len()
    );
    assert!(
        answer.contains("hello from volucer's gRPC handler"),
        "and so must the message itself.\nGot: {answer}"
    );

    assert!(
        tail.to_ascii_lowercase().contains("grpc-status: 0"),
        "the application's own status has to be in the trailer section.\nGot: {answer}"
    );

    // The **method the application was asked for**, and the reason this is asserted from the other
    // direction: a worker that could not tell which method it was serving would answer `SayHello`'s
    // reply to every call, and the status would still be `0`.
    let unknown = format!(
        "POST /greeter.Greeter/Nope HTTP/1.1\r\nHost: example.test\r\n\
         Content-Type: application/grpc+proto\r\nTE: trailers\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n",
        body.len()
    );
    let mut raw = unknown.into_bytes();
    raw.extend_from_slice(body);
    let answer = String::from_utf8_lossy(&exchange_raw(&address, &raw)).into_owned();
    let (_, tail) = answer.split_once("\r\n0\r\n").unwrap_or((&answer, ""));
    assert!(
        tail.to_ascii_lowercase().contains("grpc-status: 12"),
        "a method the application does not implement is UNIMPLEMENTED (12), which a client treats as \
         final rather than retrying.\nGot: {answer}"
    );

    // A service **not** in `grpc_services` never reaches a worker: the server answers it itself. Both
    // halves of the difference in one test, because "the server refused" and "the application
    // refused" produce the same status and only the reason in the log tells them apart.
    let refused = format!(
        "POST /other.Service/SayHello HTTP/1.1\r\nHost: example.test\r\n\
         Content-Type: application/grpc+proto\r\nTE: trailers\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n",
        body.len()
    );
    let mut raw = refused.into_bytes();
    raw.extend_from_slice(body);
    let answer = String::from_utf8_lossy(&exchange_raw(&address, &raw)).into_owned();
    assert!(
        answer.starts_with("HTTP/1.1 200 OK"),
        "a refused call is still a 200.\nGot: {answer}"
    );
    let (_, tail) = answer.split_once("\r\n0\r\n").unwrap_or((&answer, ""));
    assert!(
        tail.to_ascii_lowercase().contains("grpc-status: 12"),
        "an ungranted service is UNIMPLEMENTED, and it is the *server* that says so.\nGot: {answer}"
    );
    assert!(
        !answer.contains("hello from volucer"),
        "and the application must not have been asked: the reply must carry no message.\nGot: {answer}"
    );

    // **The ordinary case, on the same worker.** A change that made every request gRPC — the plausible
    // bug, since the branch is one `is_grpc` call — would pass everything above and break this.
    let plain = get(&address, "/health");
    assert_eq!(
        plain.status, 200,
        "an HTTP route on the same worker pool still answers as HTTP: {plain:?}"
    );
    assert!(
        plain.text().contains("ok"),
        "and with the application's own body: {:?}",
        plain.text()
    );
}

/// **A reload widens a gRPC grant, with no restart.**
///
/// The property the reloadable-grants change exists for, and the reason it is a socket test rather
/// than a unit one: the grant was captured into a worker pool's spec at startup, so this is only
/// demonstrable by showing that a *running* listener, on the *same* connection, admits a service it
/// refused a moment earlier.
///
/// Driven through the real PHP fixture, which serves both protocols — so the second half of the
/// assertion is that the call now *reaches the application*, not merely that the status changed. A
/// grant that widened without the worker being asked would produce the same `UNIMPLEMENTED` from a
/// different place, and only the message in the body tells them apart.
#[test]
fn a_reload_widens_a_grpc_grant_on_a_running_listener() {
    if let Some(reason) = skip_reason() {
        report_skip(reason);
        return;
    }

    let script = example_worker();
    let config_for = |services: &str| {
        format!(
            r#"
[server]
default_backend = "app"
http = "127.0.0.1:0"
[backends.app]
kind = "worker"
command = 'php "{}"'
workers = 1
grpc_services = [{services}]
"#,
            script.display()
        )
    };

    let before = Config::from_toml(&config_for(r#""greeter.Greeter""#)).expect("the first file");
    before.validate().expect("a legal service name validates");
    let after = Config::from_toml(&config_for(r#""greeter.Greeter", "other.Service""#))
        .expect("the second file");
    after.validate().expect("a legal service name validates");

    let routing = front_controller_routing();
    let document_root = routing.document_root.clone();
    let sites = Arc::new(sites_for(
        &routing,
        volucer_core::config::StaticConfig::default(),
    ));
    let handles = volucer_http::ReloadHandles::new(Arc::new(volucer_http::Registry::new(
        volucer_http::SharedState::new((*sites).clone()),
    )))
    .with_grpc_grants(Arc::new(before.grpc_grants_all()));

    let dispatcher = match Dispatcher::from_config(&before, routing.clone()) {
        Ok(dispatcher) => dispatcher,
        Err(err) => {
            eprintln!("skipping: the worker pool could not start: {err}");
            return;
        }
    };
    let Some(server) = Server::start_with_handles(dispatcher, HttpLimits::default(), handles)
    else {
        panic!("the listener must start");
    };
    let address = server.address.clone();

    // One call helper: `true` when the reply carries a message, which means a worker was asked.
    let reached_a_worker = |path: &str| -> (String, bool) {
        let body = b"\x00\x00\x00\x00\x05hello";
        let head = format!(
            "POST {path} HTTP/1.1\r\nHost: example.test\r\n\
             Content-Type: application/grpc+proto\r\nTE: trailers\r\nContent-Length: {}\r\n\
             Connection: close\r\n\r\n",
            body.len()
        );
        let mut raw = head.into_bytes();
        raw.extend_from_slice(body);
        let answer = String::from_utf8_lossy(&exchange_raw(&address, &raw)).into_owned();
        let asked = answer.contains("hello from volucer's gRPC handler");
        (answer, asked)
    };

    let (answer, asked) = reached_a_worker("/other.Service/SayHello");
    assert!(
        !asked,
        "the service is not granted yet, so no worker may be asked.\nGot: {answer}"
    );
    let (_, tail) = answer.split_once("\r\n0\r\n").unwrap_or((&answer, ""));
    assert!(
        tail.to_ascii_lowercase().contains("grpc-status: 12"),
        "an ungranted service is UNIMPLEMENTED before the reload.\nGot: {answer}"
    );

    // The reload, through the same handle a `SIGHUP` reaches.
    let _ = server.reload.apply(
        Sites::from_config(&after, &document_root),
        Arc::new(after.grpc_grants_all()),
    );

    let (answer, asked) = reached_a_worker("/other.Service/SayHello");
    assert!(
        asked,
        "after the reload the call must reach the application, which answers it — this is the \
         difference between a widened grant and a widened *refusal*.\nGot: {answer}"
    );

    drop(server);
}

/// Send a gRPC-shaped POST and return whatever came back, or nothing on the fixture's timeout.
///
/// Deliberately not asserting on the answer: the fixture is not an HTTP/2 server, so a completed call
/// is not the point. A client that fails here is still a client that sent the preface.
fn post_grpc(address: &str, path: &str) -> String {
    let Ok(mut stream) = TcpStream::connect(address) else {
        return String::new();
    };
    let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
    // A one-message gRPC body: compression flag 0, length 5, then five bytes. The framing is what
    // makes this a *gRPC* request rather than an HTTP one with an unusual content type, and it is
    // deliberately never parsed by anything here.
    let body = [0u8, 0, 0, 0, 5, b'h', b'e', b'l', b'l', b'o'];
    let head = format!(
        "POST {path} HTTP/1.1\r\nHost: origin\r\nContent-Type: application/grpc+proto\r\n\
         TE: trailers\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(&body);
    let _ = stream.flush();

    let mut raw = Vec::new();
    let mut chunk = [0u8; 1024];
    while let Ok(read) = stream.read(&mut chunk) {
        if read == 0 {
            break;
        }
        raw.extend_from_slice(&chunk[..read]);
    }
    String::from_utf8_lossy(&raw).into_owned()
}

/// An origin that records the first bytes it is sent and answers with an empty SETTINGS frame.
///
/// `crates/volucer-upstream/tests/h2c.rs` has a fixture of the same shape for the client-level
/// question; this one is deliberately its own, because that file asserts on a hand-written connector
/// and this one asserts on the *configured* one. Sharing a fixture between them would let a change to
/// the connector make both pass for the same wrong reason.
struct ProtocolOrigin {
    address: String,
    first_bytes: std::sync::mpsc::Receiver<Vec<u8>>,
    stop: Arc<std::sync::atomic::AtomicBool>,
}

impl ProtocolOrigin {
    fn spawn() -> ProtocolOrigin {
        let listener = OriginListener::bind("127.0.0.1:0").expect("bind an origin");
        listener
            .set_nonblocking(true)
            .expect("a non-blocking accept so this thread ends with the test");
        let address = listener.local_addr().expect("an address").to_string();
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (sender, first_bytes) = std::sync::mpsc::channel();

        let watching = Arc::clone(&stop);
        std::thread::spawn(move || {
            loop {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let _ = stream.set_nonblocking(false);
                        let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
                        let mut buffer = [0u8; 64];
                        let read = stream.read(&mut buffer).unwrap_or(0);
                        // One connection is enough; a second would be the pool's, and the first is
                        // the one that carries the preface.
                        let _ = sender.send(buffer[..read].to_vec());
                        // An empty SETTINGS frame, so an h2 client sees a server rather than a reset.
                        let _ = stream.write_all(&[0, 0, 0, 4, 0, 0, 0, 0, 0]);
                        let _ = stream.flush();
                        break;
                    }
                    Err(ref err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        if watching.load(AtomicOrdering::Relaxed) {
                            return;
                        }
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => return,
                }
            }
        });

        ProtocolOrigin {
            address,
            first_bytes,
            stop,
        }
    }

    fn first_bytes(&self) -> Vec<u8> {
        self.first_bytes
            .recv_timeout(Duration::from_secs(5))
            .unwrap_or_default()
    }
}

impl Drop for ProtocolOrigin {
    fn drop(&mut self) {
        self.stop.store(true, AtomicOrdering::Relaxed);
    }
}

/// Find a `python` on `PATH` that actually runs, for the one test whose origin is not a fixture.
///
/// **`is_file()` is not enough, and the failure it lets through is specific to Windows:** there,
/// `python3.exe` is commonly a Microsoft Store App Execution Alias — a real file that exits `9009`
/// without running an interpreter. A guard that asks only whether the name exists passes, and the
/// test then fails one layer deeper with a `StartupExited` that reads like a volucer bug. So the
/// check is that it runs, which is the property the test needs and the only one a file's existence
/// cannot fake.
fn find_python() -> Option<PathBuf> {
    #[cfg(windows)]
    let names: &[&str] = &["python3.exe", "python.exe", "python3", "python"];
    #[cfg(not(windows))]
    let names: &[&str] = &["python3", "python"];

    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        for name in names {
            let candidate = dir.join(name);
            if !candidate.is_file() {
                continue;
            }
            let runs = std::process::Command::new(&candidate)
                .arg("--version")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok_and(|status| status.success());
            if runs {
                return Some(candidate);
            }
        }
    }
    None
}

// ------------------------------------------------------------ rate limiting
//
// The limiter is shared with the FastCGI front-end, so these tests are about the *HTTP* half: that
// a limited request is refused before any work is done, that the answer is a 429 with a
// `Retry-After` a client can act on, and that a gRPC call gets the trailer a gRPC client can read
// instead. A test that only asserted the 429 would pass against a limiter that ran after the static
// layer and buffered a body first, which is the mistake this placement exists to avoid.

/// A server whose requests are answered by the static layer, with a rate limit in front of it.
///
/// The static layer is deliberate: it answers without PHP, so the refusal cannot be confused with a
/// backend failure, and a request that reaches it is observable as a 200.
fn rate_limited_server(limit: volucer_core::RateLimitPolicy) -> Option<Server> {
    let root = static_root("rate-limit");
    let routing = Routing {
        document_root: root.to_path_buf(),
        front_controller: Some("index.php".to_string()),
        ..Routing::default()
    };
    let dispatcher = Dispatcher::from_config(&unreachable_config(), routing.clone())
        .expect("a dispatcher that starts no process");
    let sites = Arc::new(sites_for(&routing, static_config()));
    let handles = volucer_http::ReloadHandles::new(Arc::new(volucer_http::Registry::new(
        volucer_http::SharedState::new((*sites).clone()),
    )))
    .with_rate_limit(Arc::new(volucer_http::RateLimitCell::new(Some(limit))));

    Server::start_with_handles(dispatcher, HttpLimits::default(), handles)
}

/// A policy of `burst` requests, refilling slowly enough that a test cannot outlast it.
fn slow_limit(burst: u32) -> volucer_core::RateLimitPolicy {
    volucer_core::RateLimitPolicy::new(
        volucer_core::KeyMode::Ip,
        None,
        1,
        Duration::from_secs(3600),
        Some(burst),
        None,
    )
    .expect("the test's policy is valid")
}

#[test]
fn a_request_over_the_rate_limit_is_a_429_with_a_retry_after() {
    let Some(server) = rate_limited_server(slow_limit(2)) else {
        return;
    };

    // Two requests inside the burst, and both reach the static layer.
    for n in 0..2 {
        let response = get(&server.address, "/assets/app.css");
        assert_eq!(response.status, 200, "request {n}: {response:?}");
    }

    let refused = get(&server.address, "/assets/app.css");
    assert_eq!(refused.status, 429, "{refused:?}");
    assert_eq!(refused.reason, "Too Many Requests");
    // **`Retry-After` is the whole reason a client can act on a 429.** Its absence is what turns a
    // rate limit into a request that is retried immediately, for ever. One hour is the configured
    // interval, so the number is the limiter's own arithmetic rather than a constant.
    assert_eq!(refused.header("retry-after"), Some("3600"), "{refused:?}");
    assert_eq!(
        refused.header("cache-control"),
        Some("no-store"),
        "a cacheable 429 would be served to every other client: {refused:?}"
    );
    assert!(
        refused.text().contains("rate limit"),
        "the page has to say what happened: {}",
        refused.text()
    );
}

/// **A `grpc_proto` grant is widened by re-reading the files, exactly as a written list is.**
///
/// The end-to-end half of the feature: `expand_grpc_proto` on the configuration, then the same
/// `apply` a `SIGHUP` reaches. Asserted through the socket rather than on the `Config`, because the
/// claim is that a *running listener* consults the grants its snapshot carries — a test that only
/// checked the expanded list would pass on an implementation that read the files and installed
/// nothing.
///
/// The `.proto` file is rewritten between the two halves, which is the whole point of the key: the
/// set of services comes from the file, so a deploy that adds one is a reload rather than an edit to
/// `volucer.toml`.
#[test]
fn a_reload_follows_a_proto_file_on_a_running_listener() {
    if let Some(reason) = skip_reason() {
        report_skip(reason);
        return;
    }

    let fixture = proto_fixture("reload");
    // One file, one package: `admin.proto` is a file a later deploy adds, and its service is granted
    // only once the entry that reaches it exists.
    fixture.write("greeter.proto", "package greeter;\nservice Greeter {}");

    let script = example_worker();
    let text_for = |entries: &str| {
        format!(
            r#"
[server]
default_backend = "app"
http = "127.0.0.1:0"
[backends.app]
kind = "worker"
command = 'php "{}"'
workers = 1
grpc_proto = [{entries}]
"#,
            script.display()
        )
    };

    let mut before = Config::from_toml(&text_for(r#""greeter.proto""#)).expect("the first file");
    before.validate().expect("a proto path validates");
    before
        .expand_grpc_proto(&fixture.0)
        .expect("the first file resolves a service");

    // The second generation names a file that did not exist when the server started. That is the
    // shape this key is for: the grant comes out of the application's own `.proto` files rather
    // than out of a list somebody maintains by hand in `volucer.toml`.
    fixture.write("admin.proto", "package admin;\nservice Admin {}");
    let mut after =
        Config::from_toml(&text_for(r#""greeter.proto", "admin.proto""#)).expect("the second file");
    after.validate().expect("two proto paths validate");
    after
        .expand_grpc_proto(&fixture.0)
        .expect("both files resolve a service");

    let routing = front_controller_routing();
    let document_root = routing.document_root.clone();
    let sites = Arc::new(sites_for(
        &routing,
        volucer_core::config::StaticConfig::default(),
    ));
    let handles = volucer_http::ReloadHandles::new(Arc::new(volucer_http::Registry::new(
        volucer_http::SharedState::new((*sites).clone()),
    )))
    .with_grpc_grants(Arc::new(before.grpc_grants_all()));

    let dispatcher = match Dispatcher::from_config(&before, routing.clone()) {
        Ok(dispatcher) => dispatcher,
        Err(err) => {
            eprintln!("skipping: the worker pool could not start: {err}");
            return;
        }
    };
    let Some(server) = Server::start_with_handles(dispatcher, HttpLimits::default(), handles)
    else {
        panic!("the listener must start");
    };
    let address = server.address.clone();

    let reached_a_worker = |path: &str| -> (String, bool) {
        let body = b"\x00\x00\x00\x00\x05hello";
        let head = format!(
            "POST {path} HTTP/1.1\r\nHost: example.test\r\n\
             Content-Type: application/grpc+proto\r\nTE: trailers\r\nContent-Length: {}\r\n\
             Connection: close\r\n\r\n",
            body.len()
        );
        let mut raw = head.into_bytes();
        raw.extend_from_slice(body);
        let answer = String::from_utf8_lossy(&exchange_raw(&address, &raw)).into_owned();
        let asked = answer.contains("hello from volucer's gRPC handler");
        (answer, asked)
    };

    // The service the first `.proto` declared: granted, and the application implements `SayHello`,
    // so the reply carries its message. Asserted on the *method* the handler accepts, because that
    // is the signal "a worker was asked": an ungranted service never reaches one at all.
    let (answer, asked) = reached_a_worker("/greeter.Greeter/SayHello");
    assert!(
        asked,
        "the service came from the .proto file and must be granted.\nGot: {answer}"
    );

    // The service the second `.proto` declares, before the reload.
    let (answer, asked) = reached_a_worker("/admin.Admin/SayHello");
    assert!(
        !asked,
        "the service is not granted yet, so no worker may be asked.\nGot: {answer}"
    );
    let (_, tail) = answer.split_once("\r\n0\r\n").unwrap_or((&answer, ""));
    assert!(
        tail.to_ascii_lowercase().contains("grpc-status: 12"),
        "an ungranted service is UNIMPLEMENTED before the reload.\nGot: {answer}"
    );

    let _ = server.reload.apply(
        Sites::from_config(&after, &document_root),
        Arc::new(after.grpc_grants_all()),
    );

    let (answer, asked) = reached_a_worker("/admin.Admin/SayHello");
    assert!(
        asked,
        "re-reading the .proto must widen the grant on the running listener.\nGot: {answer}"
    );

    drop(server);
}

/// A `grpc_proto` entry that resolves nothing refuses the reload, leaving the grants it had.
///
/// The counterpart, and the direction that matters: a reload that resolved nothing must not install
/// an empty grant, because an empty grant is a worker that answers `UNIMPLEMENTED` to everything with
/// no line saying why. `Registry::apply` is never reached here — the refusal happens while the
/// configuration is loaded, which is what makes "the previous one is still serving" true.
#[test]
fn a_reload_that_resolves_no_service_keeps_the_grants_it_had() {
    let fixture = proto_fixture("refuse");
    fixture.write("greeter.proto", "package greeter;\nservice Greeter {}");

    let text = r#"
[server]
default_backend = "app"
http = "127.0.0.1:8080"
[backends.app]
kind = "worker"
command = "php worker.php"
grpc_proto = ["greeter.proto"]
"#;
    let mut config = Config::from_toml(text).expect("the file parses");
    config.validate().expect("a proto path validates");
    config.expand_grpc_proto(&fixture.0).expect("expands");
    let grants = Arc::new(config.grpc_grants_all());
    assert!(grants["app"].granted("/greeter.Greeter/SayHello").is_some());

    // The `.proto` now declares only a message, so there is nothing to grant.
    fixture.write("greeter.proto", "package greeter;\nmessage OnlyA { }");
    let mut next = Config::from_toml(text).expect("the file parses");
    next.validate().expect("a proto path validates");
    let err = next
        .expand_grpc_proto(&fixture.0)
        .expect_err("a set that resolves nothing must refuse");
    assert!(
        err.to_string().contains("resolved no gRPC service"),
        "{err}"
    );

    // And the grant the running generation holds is untouched, because nothing replaced it.
    assert!(
        grants["app"].granted("/greeter.Greeter/SayHello").is_some(),
        "a refused expansion must leave the installed grants alone"
    );
}

/// A `.proto` fixture that removes itself, named after the test so two tests cannot collide.
struct ProtoFixture(PathBuf);

impl ProtoFixture {
    fn write(&self, relative: &str, text: &str) {
        let path = self.0.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create the parent");
        }
        std::fs::write(&path, text).expect("write the fixture");
    }
}

impl Drop for ProtoFixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn proto_fixture(name: &str) -> ProtoFixture {
    let path =
        std::env::temp_dir().join(format!("volucer-http-proto-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).expect("create the fixture");
    ProtoFixture(path)
}

/// **A reload replaces the limit, and a depleted caller stays depleted.**
///
/// The property that makes a reloadable limit a limit: a naive swap would hand every caller a fresh
/// burst, which is a limit that can be escaped by asking the operator to edit the file and signal.
/// The change here is only the interval, so the identity is unchanged and every bucket must survive.
///
/// Asserted through the socket, because the cell's own unit test cannot show that a *running listener*
/// consults the replacement rather than a stale copy.
#[test]
fn a_reloaded_rate_limit_does_not_hand_out_a_fresh_allowance() {
    let root = static_root("rate-limit-reload");
    let routing = Routing {
        document_root: root.to_path_buf(),
        front_controller: Some("index.php".to_string()),
        ..Routing::default()
    };
    let dispatcher = Dispatcher::from_config(&unreachable_config(), routing.clone())
        .expect("a dispatcher that starts no process");
    let sites = Arc::new(sites_for(&routing, static_config()));
    let cell = Arc::new(volucer_http::RateLimitCell::new(Some(slow_limit(1))));
    let handles = volucer_http::ReloadHandles::new(Arc::new(volucer_http::Registry::new(
        volucer_http::SharedState::new((*sites).clone()),
    )))
    .with_rate_limit(Arc::clone(&cell));

    let Some(server) = Server::start_with_handles(dispatcher, HttpLimits::default(), handles)
    else {
        panic!("the listener must start");
    };

    assert_eq!(get(&server.address, "/assets/app.css").status, 200);
    assert_eq!(
        get(&server.address, "/assets/app.css").status,
        429,
        "the single token is spent"
    );

    // The reload: same key mode, same header, a longer interval. Everything about the identity is
    // unchanged, so the buckets must be carried.
    assert!(
        cell.replace(Some(
            volucer_core::RateLimitPolicy::new(
                volucer_core::KeyMode::Ip,
                None,
                1,
                Duration::from_secs(7200),
                Some(1),
                None,
            )
            .expect("the test's policy is valid"),
        )),
        "a change of interval alone must carry every bucket"
    );

    let after = get(&server.address, "/assets/app.css");
    assert_eq!(
        after.status, 429,
        "a reload must not hand a depleted caller a fresh allowance: {after:?}"
    );
    // And the replacement is the one being consulted, which the *number* proves: a stale policy
    // would still say 3600.
    assert_eq!(after.header("retry-after"), Some("7200"), "{after:?}");
}

#[test]
fn a_rate_limit_is_checked_before_the_static_layer_or_a_backend_does_any_work() {
    // The placement test, and the reason it is a separate assertion: a limiter that ran after
    // `static_files::respond` would still answer a 429 eventually — but only after every refused
    // request had already cost a filesystem `stat`, and after a path naming no file had fallen
    // through to a backend the test deliberately made unreachable. What distinguishes the two
    // placements is therefore the status a *missing* file earns: refused first is a 429, and refused
    // later is whatever the request became next — a 404 on a files-only site, or a 502 here.
    //
    // Verified by moving the check behind the static layer and watching this assertion fail with
    // `left: 502`, which is the answer a request that reached the front controller gets.
    let root = static_root("rate-limit-placement");
    let Some(server) = rate_limited_server_for_root(&root, slow_limit(1)) else {
        return;
    };

    // The file must exist for the first request to be served by the static layer, which is what
    // spends the burst. It is then deleted, so a check that runs *after* the layer sees a path
    // naming no file — and one that runs before it never looks.
    assert_eq!(get(&server.address, "/assets/app.css").status, 200);
    std::fs::remove_file(root.join("assets/app.css")).expect("remove the file");

    let refused = get(&server.address, "/assets/app.css");
    assert_eq!(
        refused.status, 429,
        "anything else here means the static layer ran before the limit: {refused:?}"
    );
}

/// [`rate_limited_server`] around a root the caller owns, so a test can change the files mid-run.
fn rate_limited_server_for_root(
    root: &Path,
    limit: volucer_core::RateLimitPolicy,
) -> Option<Server> {
    let routing = Routing {
        document_root: root.to_path_buf(),
        front_controller: Some("index.php".to_string()),
        ..Routing::default()
    };
    let dispatcher = Dispatcher::from_config(&unreachable_config(), routing.clone())
        .expect("a dispatcher that starts no process");
    let sites = Arc::new(sites_for(&routing, static_config()));
    let handles = volucer_http::ReloadHandles::new(Arc::new(volucer_http::Registry::new(
        volucer_http::SharedState::new((*sites).clone()),
    )))
    .with_rate_limit(Arc::new(volucer_http::RateLimitCell::new(Some(limit))));

    Server::start_with_handles(dispatcher, HttpLimits::default(), handles)
}

#[test]
fn a_rate_limited_grpc_call_is_resource_exhausted_rather_than_a_429() {
    // A gRPC client reads its outcome from the trailer section and has no status code to branch on,
    // so a 429 reaches it as "unexpected HTTP status" and the retry never happens. The answer has to
    // be the gRPC spelling of the same instruction — `RESOURCE_EXHAUSTED`, HTTP 200.
    let Some(server) = rate_limited_server(slow_limit(1)) else {
        return;
    };

    // The single allowed request: refused for not being a granted service, which is *after* the
    // limiter and therefore spends the token — that is what makes the second one limited.
    let spent = post_grpc(&server.address, "/pkg.Service/Method");
    assert!(
        spent.contains("grpc-status: 12"),
        "the first call is UNIMPLEMENTED, not limited: {spent}"
    );

    let limited = post_grpc(&server.address, "/pkg.Service/Method");
    assert!(
        limited.contains("grpc-status: 8"),
        "a limited gRPC call must be RESOURCE_EXHAUSTED: {limited}"
    );
    assert!(
        !limited.contains("429"),
        "a gRPC failure must not use an HTTP error status: {limited}"
    );
}

// ------------------------------------------------------------ trace propagation
//
// `[server].trace_context` decides one thing: **what the backend receives.** These tests are about
// that, through a real origin, because the header is written by the listener and a unit test of
// `Observability` cannot show that the value it renders is the value that goes out. The
// configuration file is parsed rather than the enum being called directly, so the key's spelling is
// exercised too.

/// A `traceparent` whose fields are recognisable: trace `aaaa…`, span `1111…`.
const CLIENT_TRACE: &str = "00-aaaaaaaabbbbbbbbccccccccdddddddd-1111111111111111-01";

/// A listener whose only backend is an origin that echoes the `traceparent` it received.
///
/// The `Origin` is returned beside the server because dropping it stops its thread, and a stopped
/// origin answers nothing — which would make every assertion below pass by comparing against a
/// connection error rather than against a header.
fn trace_server(trace_context: &str) -> Option<(Server, Origin)> {
    trace_server_with(trace_context, volucer_observe::Sampler::default())
}

/// The same listener, with an explicit sampling policy.
///
/// A second builder rather than a third argument on the first, because almost every test above
/// wants the default and a call site that passes it every time is noise that hides the two tests
/// which actually vary it.
fn trace_server_with(
    trace_context: &str,
    sampler: volucer_observe::Sampler,
) -> Option<(Server, Origin)> {
    let origin = Origin::start(OriginBehaviour::EchoTraceparent);
    let text = format!(
        r#"
[server]
default_backend = "origin"
http = "127.0.0.1:0"
trace_context = "{trace_context}"

[backends.origin]
kind = "upstream"
origin = "http://{}"
"#,
        origin.address
    );
    let config = Config::from_toml(&text).expect("the test's configuration parses");
    config
        .validate()
        .expect("a legal trace_context value validates");
    let routing = front_controller_routing();
    let dispatcher = Dispatcher::from_config(&config, routing.clone())
        .expect("an upstream backend starts no process");
    let sites = Arc::new(sites_for(&routing, static_config()));
    let handles = volucer_http::ReloadHandles::new(Arc::new(volucer_http::Registry::new(
        volucer_http::SharedState::new((*sites).clone()),
    )))
    .with_observability(std::sync::Arc::new(
        volucer_observe::Observability::disabled()
            .with_propagation(propagation_of(config.server.trace_context))
            .with_sampling(sampler),
    ));
    let server = Server::start_with_handles(dispatcher, HttpLimits::default(), handles)?;
    Some((server, origin))
}

/// The configuration value as the behaviour enum, mirroring `volucer`'s own conversion.
///
/// Duplicated here deliberately: the conversion in `main` is private to the binary, and a test that
/// reached into it would be testing the binary's plumbing rather than the front-end's behaviour.
/// The mirror is exhaustive, so a new variant fails to compile here rather than being silently
/// mapped to a default.
fn propagation_of(
    mode: volucer_core::config::PropagationMode,
) -> volucer_observe::TracePropagation {
    use volucer_core::config::PropagationMode as From;
    use volucer_observe::TracePropagation as To;
    match mode {
        From::Ignore => To::Ignore,
        From::Extract => To::Extract,
        From::Inject => To::Inject,
        From::Propagate => To::Propagate,
    }
}

/// One `GET` carrying a `traceparent`, answered by an origin that echoes what it received.
fn get_with_trace(address: &str, traceparent: &str) -> String {
    let request = format!(
        "GET / HTTP/1.1\r\nHost: example.test\r\ntraceparent: {traceparent}\r\nConnection: close\r\n\r\n"
    );
    let answer = exchange(address, request.as_bytes());
    assert_eq!(answer.status, 200, "{answer:?}");
    answer.text().trim().to_string()
}

#[test]
fn extract_relays_the_clients_header_untouched() {
    // The default, and the behaviour that predates the key: our client's header is what the origin
    // sees. Asserted as *equality with the client's value*, which is what makes a sibling span
    // rather than a child, and is why `propagate` has to be opt-in.
    let Some((server, _origin)) = trace_server("extract") else {
        return;
    };
    assert_eq!(get_with_trace(&server.address, CLIENT_TRACE), CLIENT_TRACE);
}

#[test]
fn propagate_replaces_the_header_with_this_servers_span() {
    // **The whole point of the key.** The trace id is the caller's, the span id is not — a
    // replacement that kept both would make two hops claim to be one span, and one that changed
    // both would break the trace across this hop.
    let Some((server, _origin)) = trace_server("propagate") else {
        return;
    };
    let seen = get_with_trace(&server.address, CLIENT_TRACE);
    assert_eq!(
        &seen[3..35],
        "aaaaaaaabbbbbbbbccccccccdddddddd",
        "the caller's trace must be continued: {seen}"
    );
    assert_ne!(
        &seen[36..52],
        "1111111111111111",
        "the origin was handed the caller's span id, so its spans would be siblings: {seen}"
    );
    assert_eq!(
        seen.len(),
        55,
        "the value must be a valid traceparent: {seen}"
    );
    assert!(
        seen.ends_with("-01"),
        "the caller's sampling decision must be respected: {seen}"
    );
}

#[test]
fn inject_writes_a_header_where_the_client_sent_none() {
    // The half that does not read. Without it, an origin reached by a request that carried no
    // `traceparent` would have no context at all, and `propagate` would be silently absent for
    // exactly the traffic this server itself originates.
    let Some((server, _origin)) = trace_server("inject") else {
        return;
    };
    let answer = exchange(
        &server.address,
        b"GET / HTTP/1.1\r\nHost: example.test\r\nConnection: close\r\n\r\n",
    );
    let seen = answer.text().trim().to_string();
    assert_eq!(seen.len(), 55, "a header must be written anyway: {seen:?}");
    assert!(
        seen.starts_with("00-"),
        "the value must be a valid traceparent: {seen}"
    );
    // **Sampled, because a request with no inbound context is a root.** The default sampler is
    // OTel's `ParentBased(root=AlwaysOn)`, so this is the flag a backend is told to act on and the
    // flag the exporter writes into the span it keeps. Reporting `-00` here was the bug that made a
    // standalone deployment export nothing while `[telemetry]` looked configured.
    assert!(
        seen.ends_with("-01"),
        "a root must be recorded, so the header must say so: {seen}"
    );
}

#[test]
fn ignore_does_not_participate_but_does_not_strip_the_header_either() {
    // **`ignore` is "do not participate", not "strip".** A `traceparent` is an ordinary request
    // header, and a proxy that deleted one under a tracing setting would be breaking a correlation
    // its client set up, for a reason of its own. So the origin still sees the client's value; what
    // `ignore` changes is that this server neither continues the trace nor writes a header of its
    // own — which is observable in the *exported span's trace id*, and is asserted in
    // `volucer-observe`'s unit tests rather than here, because there is nothing at the wire to see.
    //
    // Still worth pinning: it is the assertion that `ignore` did not quietly become a strip list,
    // which would be a behaviour change for every deployment behind a client that traces.
    let Some((server, _origin)) = trace_server("ignore") else {
        return;
    };
    assert_eq!(get_with_trace(&server.address, CLIENT_TRACE), CLIENT_TRACE);
}

/// The caller's `sampled` flag reaches the backend **on the wire**, which is where a backend reads
/// it from.
///
/// The distinction this pins is the one the whole sampler exists for: an explicit `-00` is a caller
/// asking not to be recorded, and it must survive the hop; an absent header is a *root*, which the
/// default records. An implementation that flattened both to one answer would pass one half of this
/// test whichever answer it picked.
#[test]
fn the_sampled_flag_is_relayed_and_a_root_is_recorded() {
    const UNSAMPLED: &str = "00-aaaaaaaabbbbbbbbccccccccdddddddd-1111111111111111-00";

    let Some((server, _origin)) = trace_server("propagate") else {
        return;
    };
    // An explicit `-00` is honoured: this is nginx's `otel_trace $otel_parent_sampled`, and it is
    // what stops a proxy spending a tracing budget somebody upstream already declined to spend.
    let dropped = get_with_trace(&server.address, UNSAMPLED);
    assert_eq!(&dropped[3..35], "aaaaaaaabbbbbbbbccccccccdddddddd");
    assert!(
        dropped.ends_with("-00"),
        "the caller declined to be recorded, so the backend must be told: {dropped}"
    );

    // And a request with no context at all is a root, which the default sampler records.
    let answer = exchange(
        &server.address,
        b"GET / HTTP/1.1\r\nHost: example.test\r\nConnection: close\r\n\r\n",
    );
    let root = answer.text().trim().to_string();
    assert!(
        root.ends_with("-01"),
        "a root must be recorded, or a standalone deployment exports nothing: {root}"
    );
}

/// `always` overrides a caller's `-00`, and says so on the header.
///
/// The point of asserting the header rather than the export: the backend reads the header, so a
/// server that exported the span and told the backend `-00` would produce exactly the orphaned
/// parent span the default exists to prevent.
#[test]
fn always_records_a_trace_the_caller_declined_and_says_so_on_the_wire() {
    const UNSAMPLED: &str = "00-aaaaaaaabbbbbbbbccccccccdddddddd-1111111111111111-00";

    let Some((server, _origin)) = trace_server_with("propagate", volucer_observe::Sampler::Always)
    else {
        return;
    };
    let seen = get_with_trace(&server.address, UNSAMPLED);
    assert_eq!(&seen[3..35], "aaaaaaaabbbbbbbbccccccccdddddddd");
    assert!(
        seen.ends_with("-01"),
        "always must flip the flag it exports, or the backend drops a trace we kept: {seen}"
    );
}

/// A ratio writes the flag its decision reached, for both outcomes.
///
/// Run over several requests with **different trace ids**, because the decision is a function of the
/// trace id alone: reusing one caller would produce one decision repeated, which says nothing about
/// the mechanism. Both outcomes are required to appear, which is the property that distinguishes a
/// ratio from `always` and from a sampler that drops everything.
#[test]
fn a_ratio_writes_the_flag_its_decision_reached() {
    let Some((server, _origin)) = trace_server_with(
        "propagate",
        volucer_observe::Sampler::ratio(0.5).expect("a ratio"),
    ) else {
        return;
    };
    let mut kept = 0;
    let mut dropped = 0;
    for step in 0u8..24 {
        // A distinct trace id per request, spread across the whole 56-bit sampling space: the
        // sampler reads the low seven bytes, so writing the step *into* those bytes puts each
        // request at a different point in the range. Ids clustered low would all land on the same
        // side of the threshold and the test would say nothing.
        let mut bytes = [0xeeu8; 16];
        bytes[0] = 0x11;
        // The low 56 bits, with the step in the top byte of that window and a low byte that varies
        // too, so no two requests share a decision by accident.
        bytes[9] = step.wrapping_mul(11);
        bytes[15] = step.wrapping_mul(7).wrapping_add(1);
        let caller = format!(
            "00-{}-2222222222222222-01",
            bytes
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        );
        let seen = get_with_trace(&server.address, &caller);
        assert_eq!(seen.len(), 55, "{seen}");
        assert_eq!(
            &seen[3..35],
            &caller[3..35],
            "the trace must be continued, or the ratio was not applied to this caller's id"
        );
        if seen.ends_with("-01") {
            kept += 1;
        } else {
            dropped += 1;
        }
    }
    // A generous band on purpose: this is a deterministic hash over 24 trace ids, so the split is a
    // property of those ids and not of the ratio. What must hold is that both answers occur — a
    // constant sampler cannot produce both.
    assert!(kept > 0, "0.5 dropped every trace");
    assert!(dropped > 0, "0.5 kept every trace");
}
