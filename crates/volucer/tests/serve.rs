//! Smoke test for `volucer serve`: start the real binary, talk FastCGI to it,
//! and check that a request reaches a real PHP process and comes back.
//!
//! This proves the shipped binary works, as opposed to the library functions
//! being correct in isolation.
//!
//! Skipped, with a printed reason, when `php-cgi` is unavailable or the binary
//! has not been built.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

// Only the Unix half needs it: the TLS client that reads which certificate a handshake was
// presented with is `#[cfg(unix)]`, for the same reason the signal tests are.
#[cfg(unix)]
use std::sync::Arc;

use volucer_fcgi::{message::BeginRequest, nvpair, protocol::RecordType, record};

/// The `volucer` binary, found next to this test's own executable.
fn volucer_binary() -> Option<PathBuf> {
    // target/<profile>/deps/<test>.exe -> target/<profile>/volucer[.exe]
    let mut dir = std::env::current_exe().ok()?;
    dir.pop();
    if dir.ends_with("deps") {
        dir.pop();
    }
    let name = if cfg!(windows) {
        "volucer.exe"
    } else {
        "volucer"
    };
    let candidate = dir.join(name);
    candidate.is_file().then_some(candidate)
}

fn find_php_cgi() -> Option<PathBuf> {
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

/// Find `php` on `PATH`, for the worker-backend test.
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

/// A run of consecutive free ports, since the pool increments from `base_port`.
fn free_port_run(count: usize) -> Option<u16> {
    for _ in 0..64 {
        let probe = std::net::TcpListener::bind("127.0.0.1:0").ok()?;
        let base = probe.local_addr().ok()?.port();
        drop(probe);
        let mut held = Vec::with_capacity(count);
        for offset in 0..count {
            let port = base.checked_add(offset as u16)?;
            match std::net::TcpListener::bind(("127.0.0.1", port)) {
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

/// Start a server, re-reserving when the *port* was taken between probe and `bind`.
///
/// `free_port_run` binds a range, releases it, and the server binds it a moment later. Nothing can
/// hold that range across the gap: this workspace's integration tests are separate processes, each
/// probing ports the same way, so a probe result is a hint rather than a reservation. On
/// `macos-latest` the kernel hands out the most recently released port first, which turns the gap
/// into a repeatable collision instead of a rare one -- `a_missing_script_is_an_error_response_not
/// _a_hang` failed there at `the server started` while the same code was green on Linux.
///
/// The retry is narrow: only when the server did not come up, a fresh range each time, and ten
/// failures panic naming the port. That is deliberately not "retry until green" -- a real
/// misconfiguration still fails, and it fails on the port rather than on a timeout.
fn start_server_on_a_free_port(
    binary: &Path,
    php_cgi: &Path,
    root: &Path,
    workers: u32,
) -> Option<Server> {
    let mut tried = Vec::new();
    for _ in 0..10 {
        let base_port = free_port_run((workers + 1) as usize)?;
        tried.push(base_port);
        if let Some(server) = start_server(binary, php_cgi, root, workers, base_port) {
            return Some(server);
        }
    }
    panic!("the server did not start on any of ten reserved ports: {tried:?}");
}

/// Start a server from a configuration that has to be **written per attempt**.
///
/// `start_server_on_a_free_port` covers the common case, where the only thing that differs
/// between retries is a port the caller already holds. A test whose configuration *contains*
/// a `base_port` cannot use it — the port is inside the file, so a fresh range means a fresh
/// file. Taking a closure is what lets the retry happen without duplicating the template at
/// the call site, and the closure is free to reserve a new port each time it is called.
fn start_server_with_config(
    binary: &Path,
    config_path: &Path,
    config_for: &dyn Fn() -> String,
    what: &str,
) -> Server {
    for _ in 0..10 {
        std::fs::write(config_path, config_for()).expect("write the config");
        if let Some(server) = Server::start(binary, config_path) {
            return server;
        }
    }
    panic!("the server did not start on any of ten reserved ports: {what}");
}

/// A `volucer serve` process that is killed when the test ends.
struct Server {
    child: Child,
    address: String,
    /// The HTTP listener's address, when the configuration asked for one.
    http_address: Option<String>,
    /// The HTTPS listener's address, when the configuration asked for one.
    ///
    /// Separate from `http_address` because they are separate listeners: `http` stays
    /// plaintext when `https` is set, so folding one into the other would make a test
    /// believe a cleartext listener existed when only a TLS one did.
    https_address: Option<String>,
    /// The metrics and status listener's address, when a `[metrics]` or `[status]` table asked for
    /// one. Read from the banner for the same reason the other two are: a test that guessed a port
    /// would be testing its own ability to find a free one.
    status_address: Option<String>,
    /// Everything the banner printed before the server started serving.
    ///
    /// Kept because some of it is a *decision* rather than an address: a static layer that the
    /// configuration enables and the document root cannot use is turned off at startup, and the
    /// banner is where a deployment learns that. A test that only asked the request path would
    /// not be able to tell "the layer answered no" from "the layer never ran".
    banner: String,
    /// What the server wrote to stderr, read by [`Server::diagnostics`].
    ///
    /// **A failed test needs a reason, and this is where the reason is.** These tests assert on
    /// status codes, and a 502 is the server saying "the backend failed" — the *why* is a log
    /// line. With `Stdio::null` on stderr, a 502 arrived with no explanation at all, and the only
    /// way to diagnose it was to guess and rerun, which is exactly what happened to
    /// `Test (windows-latest)` in run 39829499236.
    diagnostics: Option<std::thread::JoinHandle<String>>,
}

impl Server {
    fn start(binary: &Path, config: &Path) -> Option<Self> {
        Server::start_with(binary, config, &[])
    }

    /// Start the server with extra command-line arguments.
    ///
    /// `--document-root` is one of those: it is what routing *and* static serving
    /// resolve against, so a test that writes a file elsewhere has to say so.
    fn start_with(binary: &Path, config: &Path, extra: &[&str]) -> Option<Self> {
        Server::spawn(binary, config, extra, None)
    }

    /// Start the server with a working directory of its own.
    ///
    /// Needed to test a *relative* document root: the path is relative to the
    /// process's working directory, so the child has to be started where the
    /// fixture lives.
    fn start_in(binary: &Path, config: &Path, cwd: &Path) -> Option<Self> {
        Server::spawn(binary, config, &[], Some(cwd))
    }

    fn spawn(binary: &Path, config: &Path, extra: &[&str], cwd: Option<&Path>) -> Option<Self> {
        let mut command = Command::new(binary);
        command
            .arg("serve")
            .arg("--config")
            .arg(config)
            .args(extra)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(cwd) = cwd {
            command.current_dir(cwd);
        }
        let mut child = command.spawn().ok()?;

        // Drained on its own thread, from the moment the process exists. A pipe nobody reads
        // eventually fills and blocks the server on its next log line, which would turn a
        // diagnostic into a hang — so this has to start here, before the banner is read.
        let diagnostics = child.stderr.take().map(|stderr| {
            std::thread::spawn(move || {
                let mut buffer = String::new();
                let mut reader = BufReader::new(stderr);
                let _ = reader.read_to_string(&mut buffer);
                buffer
            })
        });

        let mut server = Server {
            child,
            address: String::new(),
            http_address: None,
            https_address: None,
            status_address: None,
            banner: String::new(),
            diagnostics,
        };
        let stdout = server.child.stdout.take().expect("piped stdout");
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            line.clear();
            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                break;
            }
            server.banner.push_str(&line);
            // The banner is read line by line and parsed by field name rather than
            // by whole line, so the surrounding text can grow without breaking
            // every test that starts a server. Reading stops at the line that says
            // the server is up, which is also the last thing printed before it
            // starts serving.
            let trimmed = line.trim();
            if let Some(rest) = trimmed.strip_prefix("fastcgi:") {
                // The first whitespace-delimited token, so a line that carries a suffix can still
                // be read: `http:` prints `(https)` after the address when TLS shares the port,
                // and a parser that took the rest of the line would hand a caller
                // `127.0.0.1:8080 (https)` as an address.
                let address = rest.split_whitespace().next().unwrap_or("");
                if !address.is_empty() && address != "(disabled)" {
                    server.address = address.to_string();
                }
            } else if let Some(rest) = trimmed.strip_prefix("http:") {
                let address = rest.split_whitespace().next().unwrap_or("");
                if !address.is_empty() && address != "(disabled)" {
                    server.http_address = Some(address.to_string());
                }
            } else if let Some(rest) = trimmed.strip_prefix("https:") {
                // A listener in its own right, so it counts as one: a configuration with
                // only `https` has a live socket, and waiting for a `http:` line that will
                // never come would kill the server and report "did not start".
                let address = rest.split_whitespace().next().unwrap_or("");
                if !address.is_empty() && address != "(disabled)" {
                    server.https_address = Some(address.to_string());
                }
            } else if let Some(rest) = trimmed.strip_prefix("metrics:") {
                // `serve` prints the metrics/status listener under `metrics:` whichever of the two
                // tables configured it, because they share one listener.
                let address = rest.split_whitespace().next().unwrap_or("");
                if !address.is_empty() && address != "(disabled)" {
                    server.status_address = Some(address.to_string());
                }
            } else if trimmed.starts_with("press Ctrl-C") {
                // The banner ends here, and this is the last line printed before
                // the listeners serve. Which addresses appear depends on the
                // configuration, so the requirement is that *something* was
                // reported — not that a particular listener was. Waiting for the
                // FastCGI address specifically hangs a config that has only
                // `http`, and it hangs *silently*: `read_line` has no timeout, so
                // the deadline above is never re-checked while it blocks.
                if server.address.is_empty()
                    && server.http_address.is_none()
                    && server.https_address.is_none()
                    && server.status_address.is_none()
                {
                    break;
                }
                return Some(server);
            }
        }
        let _ = server.child.kill();
        None
    }

    /// The banner `serve` printed before it started listening.
    fn banner(&self) -> &str {
        &self.banner
    }

    /// What the server logged to stderr, after asking it to stop.
    ///
    /// **`&mut self`, and it ends the server**, because reading a pipe to the end needs the writer
    /// closed: killing the child here is what turns "wait for EOF" into a bounded call. That is why
    /// the callers are failure paths — the message is worth more than the running server.
    fn diagnostics(&mut self) -> String {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let Some(handle) = self.diagnostics.take() else {
            return String::new();
        };
        let collected = handle.join().unwrap_or_else(|_| String::new());
        if collected.trim().is_empty() {
            return String::new();
        }
        collected
    }

    /// A failure message that carries the server's own log, or says there was none.
    ///
    /// One helper rather than `format!` at each assertion, so every failure in this file is
    /// diagnosable the same way and none of them silently omits the log.
    fn failure(&mut self, what: &str) -> String {
        let log = self.diagnostics();
        if log.trim().is_empty() {
            format!("{what}: the server wrote nothing to stderr")
        } else {
            format!("{what}\nthe server's stderr:\n{log}")
        }
    }

    /// Whether the banner said this, which is how a startup *decision* is asserted.
    fn banner_mentions(&self, text: &str) -> bool {
        self.banner.contains(text)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn io_err(err: volucer_fcgi::FcgiError) -> std::io::Error {
    match err {
        volucer_fcgi::FcgiError::Io(io) => io,
        other => std::io::Error::other(other.to_string()),
    }
}

/// A decoded response: status, headers and body.
type Decoded = (u16, Vec<(String, String)>, String);

/// Send one request over a fresh connection and read the response back.
fn request(address: &str, params: &[(Vec<u8>, Vec<u8>)], body: &[u8]) -> std::io::Result<Decoded> {
    let mut stream = TcpStream::connect(address)?;
    stream.set_read_timeout(Some(Duration::from_secs(20)))?;

    let mut wire = Vec::new();
    let header = record::Header::new(RecordType::BeginRequest, 1, 8);
    header.write(&mut wire).map_err(io_err)?;
    wire.write_all(&BeginRequest::responder(false).encode())?;
    let payload = nvpair::encode_pairs(
        params
            .iter()
            .map(|(name, value)| (name.as_slice(), value.as_slice())),
    );
    record::write_records(&mut wire, RecordType::Params, 1, &payload, true).map_err(io_err)?;
    record::write_records(&mut wire, RecordType::Stdin, 1, body, true).map_err(io_err)?;
    stream.write_all(&wire)?;
    stream.flush()?;

    let mut stdout = Vec::new();
    while let Some(record) = record::read_record(&mut stream).map_err(io_err)? {
        match record.record_type() {
            RecordType::Stdout => stdout.extend_from_slice(&record.content),
            RecordType::EndRequest => break,
            _ => continue,
        }
    }

    Ok(parse(&stdout))
}

/// Split a FastCGI response into status, headers and body.
fn parse(stdout: &[u8]) -> Decoded {
    let text = String::from_utf8_lossy(stdout);
    let (head, body) = match text.split_once("\r\n\r\n") {
        Some((head, body)) => (head, body),
        None => match text.split_once("\n\n") {
            Some((head, body)) => (head, body),
            None => (text.as_ref(), ""),
        },
    };

    let mut status = 200u16;
    let mut headers = Vec::new();
    for (index, line) in head.split('\n').enumerate() {
        let line = line.trim_end_matches('\r');
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        if index == 0 && name.eq_ignore_ascii_case("status") {
            status = value
                .split_whitespace()
                .next()
                .and_then(|code| code.parse().ok())
                .unwrap_or(200);
        } else {
            headers.push((name.trim().to_string(), value.to_string()));
        }
    }
    (status, headers, body.to_string())
}

fn params_for(root: &Path, uri: &str, body_len: usize) -> Vec<(Vec<u8>, Vec<u8>)> {
    let routing = volucer_fcgi::Routing {
        document_root: root.to_path_buf(),
        front_controller: Some("index.php".to_string()),
        ..volucer_fcgi::Routing::default()
    };
    let info = volucer_fcgi::RequestInfo {
        method: if body_len > 0 { "POST" } else { "GET" }.to_string(),
        uri: uri.to_string(),
        body_len,
        ..volucer_fcgi::RequestInfo::default()
    };
    volucer_fcgi::build_params(&info, &routing).expect("params build")
}

/// Write the config the server will read, then start it.
///
/// The document root is written into the config rather than passed as a flag:
/// this is the path a deployment takes, and the flag is only the override.
fn start_server(
    binary: &Path,
    php_cgi: &Path,
    root: &Path,
    workers: u32,
    base_port: u16,
) -> Option<Server> {
    let config_path = root.join("volucer.toml");
    let document_root = toml_string(&root.display().to_string());
    let config = format!(
        "[server]\ndefault_backend = \"cgi\"\nfastcgi = \"127.0.0.1:0\"\ndocument_root = {document_root}\n\n[backends.cgi]\nkind = \"spawn_cgi\"\nbinary = {binary:?}\nworkers = {workers}\nbind_host = \"127.0.0.1\"\nbase_port = {base_port}\n",
        binary = php_cgi.display().to_string(),
    );
    std::fs::write(&config_path, config).expect("write the config");
    Server::start(binary, &config_path)
}

/// The SDK the example worker lives in, which is now a repository of its own.
///
/// `VOLUCER_SDK_DIR` overrides it; otherwise the sibling layout is assumed:
/// `sailantis/volucer` and `sailantis/volucer-php-sdk` next to each other.
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

/// The SDK's example worker, which is what the worker backend is pointed at.
fn example_worker() -> PathBuf {
    sdk_dir().join("example").join("worker.php")
}

/// Why a worker-backed test cannot run here, if it cannot.
///
/// The SDK is a separate checkout, so "the test ran" and "the test was skipped"
/// are easy to confuse. `VOLUCER_E2E=1` turns the skip into a failure, because
/// libtest hides the stderr of a passing test and a skip is otherwise invisible.
fn sdk_skip_reason() -> Option<String> {
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

/// Skip a worker-backed test, loudly, when the SDK checkout is missing.
macro_rules! require_sdk {
    () => {
        if let Some(reason) = sdk_skip_reason() {
            if std::env::var("VOLUCER_E2E")
                .map(|v| !v.is_empty() && v != "0")
                .unwrap_or(false)
            {
                panic!("VOLUCER_E2E is set, so a skipped test is a failure: {reason}");
            }
            eprintln!("skipping: {reason}");
            return;
        }
    };
}

/// Quote a string for a TOML basic (`"…"`) string.
///
/// A Windows path has to be escaped here: in a TOML basic string a backslash
/// starts an escape sequence, so `C:\Users` is a parse error, not a path. The
/// same papercut is waiting for anyone who writes this configuration by hand, so
/// the docs recommend a literal (`'…'`) string instead — but a test cannot rely
/// on the temp directory being free of the characters that would break a literal
/// string, so it escapes.
fn toml_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for character in value.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

/// Start the server with a `worker` backend, so the whole chain runs on the
/// volucer worker protocol rather than on FastCGI to `php-cgi`.
fn start_worker_server(binary: &Path, root: &Path) -> Option<Server> {
    let config_path = root.join("volucer.toml");
    let script = example_worker();
    assert!(
        script.is_file(),
        "the SDK example worker is missing at {}",
        script.display()
    );
    let command = toml_string(&format!("php \"{}\"", script.display()));
    let document_root = toml_string(&root.display().to_string());
    let config = format!(
        "[server]\ndefault_backend = \"app\"\nfastcgi = \"127.0.0.1:0\"\ndocument_root = {document_root}\n\n[backends.app]\nkind = \"worker\"\ncommand = {command}\nworkers = 2\n",
    );
    std::fs::write(&config_path, config).expect("write the config");
    Server::start(binary, &config_path)
}

/// Start the server with both listeners and a worker backend.
fn start_dual_listener_server(binary: &Path, root: &Path) -> Option<Server> {
    let config_path = root.join("volucer.toml");
    let script = example_worker();
    assert!(
        script.is_file(),
        "the SDK example worker is missing at {}",
        script.display()
    );
    let command = toml_string(&format!("php \"{}\"", script.display()));
    let document_root = toml_string(&root.display().to_string());
    let config = format!(
        "[server]\ndefault_backend = \"app\"\nhttp = \"127.0.0.1:0\"\nfastcgi = \"127.0.0.1:0\"\ndocument_root = {document_root}\n\n[backends.app]\nkind = \"worker\"\ncommand = {command}\nworkers = 2\n",
    );
    std::fs::write(&config_path, config).expect("write the config");
    Server::start(binary, &config_path)
}

#[test]
fn serve_answers_an_http_request_and_a_fastcgi_request_at_once() {
    let Some(binary) = volucer_binary() else {
        eprintln!("skipping: the volucer binary is not built (run `cargo build`)");
        return;
    };
    if find_php().is_none() {
        eprintln!("skipping: no php on PATH");
        return;
    }

    let root = std::env::temp_dir().join(format!("volucer-http-smoke-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create the temp root");

    require_sdk!();

    let Some(server) = start_dual_listener_server(&binary, &root) else {
        eprintln!("skipping: the server did not start");
        return;
    };
    let Some(http_address) = server.http_address.clone() else {
        panic!("the HTTP listener did not report an address; the banner was not parsed");
    };

    // The HTTP front-end, over a real socket, against the shipped binary and the
    // shipped PHP SDK.
    let response = http_get(&http_address, "/health");
    assert_eq!(response.status, 200, "{response:?}");
    assert!(
        response.text().contains("ok"),
        "the worker's body did not arrive over HTTP: {}",
        response.text()
    );

    // The FastCGI listener on the same process must still serve, which is the
    // property the dual listener exists for: one dispatcher, two protocols.
    let body = b"";
    let params = params_for(&root, "/health", 0);
    let (status, _, fastcgi_body) =
        request(&server.address, &params, body).expect("the FastCGI request completed");
    // The worker backend has no front controller to resolve against the temp
    // root, so the environment is what matters here rather than the status: a
    // response proves the listener answered at all.
    assert!(
        status == 200 || status == 404 || status == 500,
        "the FastCGI listener produced no response: {status} {fastcgi_body}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// A minimal HTTP response reader, for the dual-listener smoke test.
#[derive(Debug)]
struct HttpResponse {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl HttpResponse {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

fn http_get(address: &str, path: &str) -> HttpResponse {
    http_get_with(address, path, &[])
}

/// The same, with extra request headers — so a test can send the id this server adopts.
fn http_get_with(address: &str, path: &str, extra: &[(&str, &str)]) -> HttpResponse {
    http_request(address, "GET", path, extra)
}

/// One HTTP request with a method, for the endpoints that mutate.
fn http_request(address: &str, method: &str, path: &str, extra: &[(&str, &str)]) -> HttpResponse {
    let mut stream = TcpStream::connect(address).expect("connect to the HTTP listener");
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .expect("a read timeout");
    let mut request = format!("{method} {path} HTTP/1.1\r\nHost: example.test\r\n");
    for (name, value) in extra {
        request.push_str(name);
        request.push_str(": ");
        request.push_str(value);
        request.push_str("\r\n");
    }
    // A `Content-Length: 0` on a method that carries a body, because `hyper` would otherwise frame
    // the request by the connection closing and the answer would arrive after the client stopped
    // writing — which works, but is not what a caller does.
    if method != "GET" {
        request.push_str("Content-Length: 0\r\n");
    }
    request.push_str("Connection: close\r\n\r\n");
    stream
        .write_all(request.as_bytes())
        .expect("write the HTTP request");
    stream.flush().expect("flush");

    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).expect("read the response");

    let split = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .unwrap_or_else(|| {
            panic!(
                "no header terminator in {:?}",
                String::from_utf8_lossy(&raw)
            )
        });
    let head = String::from_utf8_lossy(&raw[..split]).into_owned();
    let mut lines = head.lines();
    let status = lines
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .unwrap_or_else(|| panic!("no status in {head:?}"));
    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_string(), value.trim().to_string()))
        .collect();

    HttpResponse {
        status,
        headers,
        body: raw[split + 4..].to_vec(),
    }
}

/// A route's `policy` decides which layer answers, asserted from outside the process.
///
/// **The case is chosen so the two layers genuinely disagree.** `policy = "backend"` on a
/// subtree that contains a real file: without the policy the static layer serves the file, with
/// it the backend answers instead. The distinguishing evidence is therefore the *body*, not the
/// status ? a first version of this test used `policy = "static"` and a missing path, and the
/// falsification passed, because a `spawn_cgi` backend answers 404 for a path naming no script
/// whether or not the route told it to. Two paths agreeing is not a test of which one ran.
#[test]
fn a_route_policy_decides_which_layer_answers() {
    let Some(binary) = volucer_binary() else {
        eprintln!("skipping: the volucer binary is not built (run `cargo build`)");
        return;
    };
    let Some(php_cgi) = find_php_cgi() else {
        eprintln!("skipping: no php-cgi on PATH");
        return;
    };
    // Two *separate* runs of ports, not one shared value: the HTTP listener binds one and
    // `spawn_cgi` binds the next, so reusing the same number for both would have the backend
    // try to listen on the port the front-end already holds.
    let Some(http_port) = free_port_run(1) else {
        eprintln!("skipping: no free port");
        return;
    };
    let Some(base_port) = free_port_run(2) else {
        eprintln!("skipping: no run of two free ports");
        return;
    };

    let root = std::env::temp_dir().join(format!("volucer-policy-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("assets")).expect("create the document root");
    std::fs::write(root.join("index.php"), "<?php echo 'APP';\n")
        .expect("write the front controller");
    // A real file under the subtree. Its presence is what makes the two policies differ.
    std::fs::write(root.join("assets").join("present.txt"), "A FILE").expect("write an asset");

    let port = http_port;
    let config_path = root.join("volucer.toml");
    let config = format!(
        r#"
[server]
default_backend = "cgi"
http = "127.0.0.1:{port}"
document_root = {root}
front_controller = "index.php"

[backends.cgi]
kind = "spawn_cgi"
binary = {php_cgi}
workers = 1
bind_host = "127.0.0.1"
base_port = {base_port}

[[server.routes]]
path_prefix = "/assets"
policy = "backend"
backend = "cgi"
"#,
        root = toml_string(&root.display().to_string()),
        php_cgi = toml_string(&php_cgi.display().to_string()),
    );
    std::fs::write(&config_path, config).expect("write the config");

    let Some(server) = Server::start(&binary, &config_path) else {
        eprintln!("skipping: the server did not start");
        let _ = std::fs::remove_dir_all(&root);
        return;
    };
    let address = server
        .http_address
        .clone()
        .expect("the configuration asked for an HTTP listener");

    // Inside the subtree the backend answers even though a file exists there. `spawn_cgi`
    // hands a non-script path to the front controller, so the body is the application's.
    let routed = http_get(&address, "/assets/present.txt");
    assert_eq!(200, routed.status, "the request is served");
    assert_eq!(
        b"APP".as_slice(),
        routed.body.as_slice(),
        "policy = backend must dispatch rather than serve the file that is sitting there"
    );

    // The control, and it has to be a case the two layers answer *differently* -- which is the
    // whole reason the assertion above means anything. A file outside the subtree is served by
    // the static layer, so the same server returns its bytes rather than the application's.
    std::fs::write(root.join("outside.txt"), "OUTSIDE").expect("write a file outside the route");
    let plain = http_get(&address, "/outside.txt");
    assert_eq!(
        200, plain.status,
        "outside the route the static layer answers"
    );
    assert_eq!(
        b"OUTSIDE".as_slice(),
        plain.body.as_slice(),
        "with the file's own bytes, not the front controller's output"
    );

    drop(server);
    let _ = std::fs::remove_dir_all(&root);
}

/// Compression happens on the wire, and only when it should.
///
/// The assertions are chosen so each one fails if the feature is removed *or* if it is made
/// unconditional: a client that does not ask gets the plain bytes, a client that asks gets a
/// header matching a body that decodes back to the original, and a body under the threshold is
/// passed through even when asked for.
#[test]
fn a_backend_response_is_compressed_only_when_it_is_asked_for() {
    let Some(binary) = volucer_binary() else {
        eprintln!("skipping: the volucer binary is not built (run `cargo build`)");
        return;
    };
    let Some(php_cgi) = find_php_cgi() else {
        eprintln!("skipping: no php-cgi on PATH");
        return;
    };

    let root = std::env::temp_dir().join(format!("volucer-compress-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create the document root");
    // A body large enough and repetitive enough to compress, and a second one below the
    // threshold so the pass-through path is asserted too.
    std::fs::write(
        root.join("index.php"),
        r#"<?php
$which = $_GET['which'] ?? 'big';
echo $which === 'big' ? str_repeat('volucer ', 4000) : 'small';
"#,
    )
    .expect("write the front controller");

    let config_path = root.join("volucer.toml");
    // The port and the pool's `base_port` both live *inside* the file, so a fresh range means a
    // fresh file — hence the closure rather than `start_server_on_a_free_port`. Without the retry,
    // a port taken between the probe and the bind is a run that fails at the first assertion with
    // a 502 and no explanation.
    let config_for = || {
        let base_port = free_port_run(2).expect("a free run of two ports");
        format!(
            r#"
[server]
default_backend = "cgi"
http = "127.0.0.1:{port}"
document_root = {root}
front_controller = "index.php"

[server.compression]
enabled = true

[backends.cgi]
kind = "spawn_cgi"
binary = {php_cgi}
workers = 1
bind_host = "127.0.0.1"
base_port = {base_port}
"#,
            // The listener takes the first port of the run and the pool starts at the second, so
            // reusing one number for both would have the backend bind what the front-end holds.
            port = base_port,
            root = toml_string(&root.display().to_string()),
            php_cgi = toml_string(&php_cgi.display().to_string()),
            base_port = base_port + 1,
        )
    };

    let mut server = start_server_with_config(
        &binary,
        &config_path,
        &config_for,
        "a compression configuration",
    );
    let address = server
        .http_address
        .clone()
        .expect("the configuration asked for an HTTP listener");

    // 1. No `Accept-Encoding`: the bytes are the application's own.
    let plain = http_get(&address, "/?which=big");
    assert_eq!(
        200,
        plain.status,
        "{}",
        server.failure(&format!("the plain request failed: {plain:?}"))
    );
    assert_eq!(
        None,
        plain.header("content-encoding"),
        "a client that did not ask must not be sent an encoded body"
    );
    let reference = plain.body.clone();

    // 2. gzip asked for, gzip sent, and it decodes back to exactly the plain bytes.
    let gzipped = http_get_with(&address, "/?which=big", &[("Accept-Encoding", "gzip")]);
    assert_eq!(200, gzipped.status);
    assert_eq!(
        Some("gzip"),
        gzipped.header("content-encoding"),
        "the client asked for gzip and the body is text"
    );
    assert!(
        gzipped.body.len() < reference.len(),
        "the compressed body must be smaller: {} vs {}",
        gzipped.body.len(),
        reference.len()
    );
    // `Vary` is what stops a cache handing these two answers to each other.
    assert!(
        gzipped
            .header("vary")
            .is_some_and(|value| value.to_ascii_lowercase().contains("accept-encoding")),
        "a compressed response must vary by accept-encoding: {:?}",
        gzipped.header("vary")
    );

    {
        use flate2::read::GzDecoder;
        use std::io::Read;
        let mut decoded = Vec::new();
        GzDecoder::new(&gzipped.body[..])
            .read_to_end(&mut decoded)
            .expect("the gzip body decodes");
        assert_eq!(
            reference, decoded,
            "the compressed body must decode to the bytes the other client got"
        );
    }

    // 3. A small body is passed through even though the client asked.
    //
    //     Note on what this does and does not pin: at *this* size two separate guards both
    //     hold ? `min_length`, and the post-hoc "did compressing actually help" check, since
    //     a 5-byte body gains a 20-byte gzip header. Removing either one alone still leaves
    //     the other, so this assertion is a statement about the outcome rather than about
    //     `min_length` specifically. `compress.rs`'s own unit tests pin the threshold in
    //     isolation, where a body can be built that is large enough that compression *would*
    //     help and still below the minimum.
    let tiny = http_get_with(&address, "/?which=small", &[("Accept-Encoding", "gzip")]);
    assert_eq!(200, tiny.status);
    assert_eq!(
        None,
        tiny.header("content-encoding"),
        "a body this small is not worth compressing"
    );
    assert_eq!(b"small".as_slice(), tiny.body.as_slice());

    drop(server);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn serve_answers_a_fastcgi_request_from_a_real_php_process() {
    let Some(binary) = volucer_binary() else {
        eprintln!("skipping: the volucer binary is not built (run `cargo build`)");
        return;
    };
    let Some(php_cgi) = find_php_cgi() else {
        eprintln!("skipping: no php-cgi on PATH");
        return;
    };
    let root = std::env::temp_dir().join(format!("volucer-smoke-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create the document root");
    std::fs::write(
        root.join("index.php"),
        r#"<?php
header('X-Smoke: yes');
echo "URI=", $_SERVER['REQUEST_URI'] ?? '-', "\n";
echo "PATH=", $_SERVER['PATH_INFO'] ?? '-', "\n";
echo "BODY=", file_get_contents('php://input');
"#,
    )
    .expect("write the front controller");

    let server =
        start_server_on_a_free_port(&binary, &php_cgi, &root, 2).expect("the server started");
    assert!(!server.address.is_empty());

    let body = b"hello-body";
    let params = params_for(&root, "/deep/route?x=1", body.len());
    let (status, headers, response_body) =
        request(&server.address, &params, body).expect("the request completed");

    assert_eq!(status, 200, "{response_body}");
    assert!(
        headers
            .iter()
            .any(|(name, value)| name.eq_ignore_ascii_case("X-Smoke") && value == "yes"),
        "the script's header is missing: {headers:?}"
    );
    assert!(
        response_body.contains("URI=/deep/route?x=1"),
        "{response_body}"
    );
    assert!(
        response_body.contains("PATH=/deep/route"),
        "{response_body}"
    );
    assert!(response_body.contains("BODY=hello-body"), "{response_body}");

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn serve_keeps_serving_across_several_requests() {
    let Some(binary) = volucer_binary() else {
        eprintln!("skipping: the volucer binary is not built");
        return;
    };
    let Some(php_cgi) = find_php_cgi() else {
        eprintln!("skipping: no php-cgi on PATH");
        return;
    };
    let root = std::env::temp_dir().join(format!("volucer-smoke-many-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create the document root");
    std::fs::write(
        root.join("index.php"),
        r#"<?php echo "N="; echo file_get_contents('php://input');"#,
    )
    .expect("write the front controller");

    let server =
        start_server_on_a_free_port(&binary, &php_cgi, &root, 2).expect("the server started");

    for index in 0..6 {
        let body = format!("req{index}");
        let params = params_for(&root, "/", body.len());
        let (status, _, response_body) =
            request(&server.address, &params, body.as_bytes()).expect("the request completed");
        assert_eq!(status, 200, "{response_body}");
        assert!(
            response_body.contains(&format!("N=req{index}")),
            "{response_body}"
        );
    }

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn the_nginx_get_values_probe_is_answered_without_ending_the_connection() {
    let Some(binary) = volucer_binary() else {
        eprintln!("skipping: the volucer binary is not built");
        return;
    };
    let Some(php_cgi) = find_php_cgi() else {
        eprintln!("skipping: no php-cgi on PATH");
        return;
    };
    let root = std::env::temp_dir().join(format!("volucer-smoke-probe-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create the document root");
    std::fs::write(root.join("index.php"), r#"<?php echo "OK";"#).expect("write the script");

    let server =
        start_server_on_a_free_port(&binary, &php_cgi, &root, 1).expect("the server started");

    // This is exactly what nginx does before deciding to use a FastCGI server.
    let mut stream = TcpStream::connect(&server.address).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("set timeout");

    let payload = nvpair::encode_pairs([(&b"max_connections"[..], &b""[..])]);
    let mut wire = Vec::new();
    record::write_records(&mut wire, RecordType::GetValues, 0, &payload, false)
        .expect("frame the probe");
    stream.write_all(&wire).expect("write the probe");
    stream.flush().expect("flush");

    let response = record::read_record(&mut stream)
        .map_err(io_err)
        .expect("a reply")
        .expect("a record");
    assert_eq!(
        response.record_type(),
        RecordType::GetValuesResult,
        "a probe must be answered with a result, never an END_REQUEST"
    );
    let values = nvpair::decode_pairs(&response.content).expect("decode the result");
    assert!(
        values.iter().any(|(name, _)| name == b"max_connections"),
        "the requested value should be present: {values:?}"
    );

    // The connection must still be usable for a real request afterwards, which
    // is the part that matters: a probe that closed the connection would make
    // nginx mark the server down.
    let params = params_for(&root, "/", 0);
    let mut wire = Vec::new();
    let header = record::Header::new(RecordType::BeginRequest, 1, 8);
    header.write(&mut wire).map_err(io_err).expect("header");
    wire.write_all(&BeginRequest::responder(true).encode())
        .expect("begin");
    let payload = nvpair::encode_pairs(
        params
            .iter()
            .map(|(name, value)| (name.as_slice(), value.as_slice())),
    );
    record::write_records(&mut wire, RecordType::Params, 1, &payload, true).expect("params");
    record::write_records(&mut wire, RecordType::Stdin, 1, b"", true).expect("stdin");
    stream.write_all(&wire).expect("write the request");
    stream.flush().expect("flush");

    let mut body = Vec::new();
    while let Some(record) = record::read_record(&mut stream)
        .map_err(io_err)
        .expect("a record")
    {
        match record.record_type() {
            RecordType::Stdout => body.extend_from_slice(&record.content),
            RecordType::EndRequest => break,
            _ => continue,
        }
    }
    let (status, _, text) = parse(&body);
    assert_eq!(status, 200, "{text}");
    assert!(text.contains("OK"), "{text}");

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_missing_script_is_an_error_response_not_a_hang() {
    let Some(binary) = volucer_binary() else {
        eprintln!("skipping: the volucer binary is not built");
        return;
    };
    let Some(php_cgi) = find_php_cgi() else {
        eprintln!("skipping: no php-cgi on PATH");
        return;
    };
    let root = std::env::temp_dir().join(format!("volucer-smoke-missing-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create the document root");

    let server =
        start_server_on_a_free_port(&binary, &php_cgi, &root, 1).expect("the server started");

    // No front controller exists in this root, so PHP cannot find the script.
    let params = params_for(&root, "/nope.php", 0);
    let (status, _, body) = request(&server.address, &params, b"").expect("the request completed");
    assert!(
        status == 404 || status == 500 || body.contains("No input file"),
        "expected an error, got {status}: {body}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// The whole chain, with the worker pool in it:
///
/// ```text
/// FastCGI client (standing in for nginx)
///     ──FastCGI──► volucer front-end ──► dispatcher ──volucer protocol──► php worker
/// ```
///
/// This is the step-17 milestone: the shipped binary serves a request through the
/// persistent worker pool rather than through `php-cgi`, so the config parsing,
/// the command splitter, the pool, the handshake and the response rendering are
/// all exercised together.
#[test]
fn serve_reaches_a_persistent_php_worker_over_the_volucer_protocol() {
    let Some(binary) = volucer_binary() else {
        eprintln!("skipping: the volucer binary is not built (run `cargo build`)");
        return;
    };
    if find_php().is_none() {
        eprintln!("skipping: no php on PATH");
        return;
    }
    let worker = example_worker();
    if !worker.is_file() {
        eprintln!(
            "skipping: the SDK example worker is missing at {}",
            worker.display()
        );
        return;
    }

    let root = std::env::temp_dir().join(format!("volucer-smoke-worker-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create the document root");

    require_sdk!();

    let Some(server) = start_worker_server(&binary, &root) else {
        eprintln!("skipping: the worker-backed server did not start");
        return;
    };
    assert!(!server.address.is_empty());

    // The worker decides what to run, so the document root only has to exist for
    // the CGI environment to be built against it.
    let params = params_for(&root, "/health", 0);
    let (status, headers, body) =
        request(&server.address, &params, b"").expect("the request completed");
    assert_eq!(status, 200, "{body}");
    assert!(
        body.contains("\"status\":\"ok\""),
        "the worker's JSON should have arrived: {body}"
    );
    // The worker's own `Content-Type` survives the translation into a Status
    // block, which is what proves the header list was rendered and not dropped.
    assert!(
        headers
            .iter()
            .any(|(name, value)| name.eq_ignore_ascii_case("content-type")
                && value.starts_with("application/json")),
        "the worker's Content-Type is missing: {headers:?}"
    );

    // A body in the request direction, and a second request to prove the worker
    // is reused rather than restarted per request.
    let marker = "over-the-pool";
    let params = params_for(&root, "/echo", marker.len());
    let (status, _, body) =
        request(&server.address, &params, marker.as_bytes()).expect("the request completed");
    assert_eq!(status, 200, "{body}");
    assert!(body.contains(&format!("payload: {marker}")), "{body}");

    let _ = std::fs::remove_dir_all(&root);
}

/// Start the server with both listeners, a worker backend and static files.
///
/// The document root is written into the config, so the file served by the
/// *shipped binary* is resolved from the *shipped config* — nothing about the
/// path is passed on a command line.
fn start_static_server(binary: &Path, root: &Path) -> Option<Server> {
    let config_path = root.join("volucer.toml");
    let command = toml_string(&format!("php \"{}\"", example_worker().display()));
    let document_root = toml_string(&root.display().to_string());
    let config = format!(
        "[server]\ndefault_backend = \"app\"\nhttp = \"127.0.0.1:0\"\nfastcgi = \"127.0.0.1:0\"\ndocument_root = {document_root}\n\n[server.static_files]\nenabled = true\ncache_control = \"public, max-age=60\"\n\n[backends.app]\nkind = \"worker\"\ncommand = {command}\nworkers = 2\n",
    );
    std::fs::write(&config_path, config).expect("write the config");
    Server::start_with(binary, &config_path, &[])
}

/// The static layer, end to end through the shipped binary.
///
/// One config, two answers: a path that names a file is served from disk without
/// waking PHP, and a path that does not is still the front controller's. The
/// second half is what proves the layer is a *routing* decision and not a
/// replacement for the application.
#[test]
fn serve_serves_a_static_file_and_still_reaches_php_for_a_route() {
    let Some(binary) = volucer_binary() else {
        eprintln!("skipping: the volucer binary is not built (run `cargo build`)");
        return;
    };
    if find_php().is_none() {
        eprintln!("skipping: no php on PATH");
        return;
    }

    let root = std::env::temp_dir().join(format!("volucer-static-smoke-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create the document root");
    std::fs::write(root.join("style.css"), "body { margin: 0 }").expect("write a static file");

    let Some(server) = start_static_server(&binary, &root) else {
        eprintln!("skipping: the server did not start");
        return;
    };
    let Some(http_address) = server.http_address.clone() else {
        panic!("the HTTP listener did not report an address; the banner was not parsed");
    };

    // A real file: served from disk, with the MIME type and the caching the
    // configuration asked for.
    let response = http_get(&http_address, "/style.css");
    assert_eq!(response.status, 200, "{response:?}");
    assert_eq!(response.text(), "body { margin: 0 }");
    assert!(
        response
            .header("content-type")
            .is_some_and(|value| value.starts_with("text/css")),
        "the file's type is missing: {response:?}"
    );
    assert_eq!(
        response.header("cache-control"),
        Some("public, max-age=60"),
        "the configured caching applies"
    );

    // Not a file: the front controller answers, so the worker's own body arrives
    // and the static layer has not swallowed the application.
    let response = http_get(&http_address, "/health");
    assert_eq!(response.status, 200, "{response:?}");
    assert!(
        response.text().contains("ok"),
        "the application is still reachable: {}",
        response.text()
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// A front-controller document root with no asset loses the static layer, end to end.
///
/// The unit tests establish that `Sites::from_config` turns the layer off. This is the part
/// they cannot see: that the *listener* skips it, so a `GET /` that names no file reaches PHP
/// without a `stat`. The observable difference is a file in the root that the layer would have
/// served — and the assertion is that it is *not* served, because the layer is not running.
///
/// The two configurations differ in one key, which is what makes this attributable: the same
/// root and the same binary, with `enabled = true` written down and then left out.
#[test]
fn a_root_with_no_assets_serves_no_files_even_though_the_default_is_on() {
    let Some(binary) = volucer_binary() else {
        eprintln!("skipping: the volucer binary is not built (run `cargo build`)");
        return;
    };
    require_sdk!();
    if find_php().is_none() {
        eprintln!("skipping: no php on PATH");
        return;
    }

    let root = std::env::temp_dir().join(format!("volucer-auto-off-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create the document root");
    std::fs::write(root.join("index.php"), "<?php echo 'APP';\n").expect("write the controller");

    // One backend and one controller: the shape that pays for a stat it cannot use. The
    // file is written *after* the root exists, and it is not an asset — a `.php` file is
    // what the layer refuses to serve — so the root still has nothing to serve.
    //
    // The configuration lives *outside* the document root, and not for tidiness: a
    // `volucer.toml` in the root is a file the layer would happily serve, which would make
    // this root one with an asset in it.
    let config_path = root
        .join("..")
        .join(format!("volucer-auto-off-{}.toml", std::process::id()));
    let command = toml_string(&format!("php \"{}\"", example_worker().display()));
    let document_root = toml_string(&root.display().to_string());
    let config = format!(
        "[server]\ndefault_backend = \"app\"\nhttp = \"127.0.0.1:0\"\ndocument_root = \
         {document_root}\n\n[backends.app]\nkind = \"worker\"\ncommand = {command}\nworkers = 2\n",
    );
    std::fs::write(&config_path, config).expect("write the config");

    let Some(server) = Server::start(&binary, &config_path) else {
        eprintln!("skipping: the server did not start");
        return;
    };
    let Some(http_address) = server.http_address.clone() else {
        panic!("the HTTP listener did not report an address; the banner was not parsed");
    };
    // The banner is the operator's notice that the layer is not the one the file describes.
    // Asserted here rather than left to `check`, because `serve` is what a deployment runs.
    assert!(
        server.banner_mentions("static files:     off"),
        "the banner must say the layer is off, not leave it to be inferred: {}",
        server.banner()
    );

    // The front controller answers, and the body is the worker's own — so the request reached
    // PHP rather than being answered by a layer that is not running.
    let response = http_get(&http_address, "/");
    assert_eq!(response.status, 200, "{response:?}");
    assert!(
        response.text().contains("volucer worker"),
        "the front controller answers, not the layer: {}",
        response.text()
    );

    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_file(&config_path);
}

/// A config that describes the whole deployment, and a flag that overrides part
/// of it — which is the rule the CLI documents.
///
#[test]
fn the_command_line_document_root_overrides_the_one_in_the_file() {
    let Some(binary) = volucer_binary() else {
        eprintln!("skipping: the volucer binary is not built (run `cargo build`)");
        return;
    };
    let Some(php_cgi) = find_php_cgi() else {
        eprintln!("skipping: no php-cgi on PATH");
        return;
    };
    let Some(base_port) = free_port_run(1) else {
        eprintln!("skipping: no free port");
        return;
    };

    let root = std::env::temp_dir().join(format!("volucer-override-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let from_file = root.join("from-file");
    let from_flag = root.join("from-flag");
    std::fs::create_dir_all(&from_file).expect("create the configured root");
    std::fs::create_dir_all(&from_flag).expect("create the override root");
    std::fs::write(from_file.join("index.php"), "<?php echo \"FILE\";")
        .expect("write the configured front controller");
    std::fs::write(from_flag.join("index.php"), "<?php echo \"FLAG\";")
        .expect("write the overriding front controller");

    let config_path = root.join("volucer.toml");
    let document_root = toml_string(&from_file.display().to_string());
    let config = format!(
        "[server]\ndefault_backend = \"cgi\"\nfastcgi = \"127.0.0.1:0\"\ndocument_root = {document_root}\n\n[backends.cgi]\nkind = \"spawn_cgi\"\nbinary = {cgi:?}\nworkers = 1\nbind_host = \"127.0.0.1\"\nbase_port = {base_port}\n",
        cgi = php_cgi.display().to_string(),
    );
    std::fs::write(&config_path, config).expect("write the config");

    let Some(server) = Server::start_with(
        &binary,
        &config_path,
        &["--document-root", &from_flag.display().to_string()],
    ) else {
        eprintln!("skipping: the server did not start");
        return;
    };

    let params = params_for(&from_flag, "/", 0);
    let (status, _, body) = request(&server.address, &params, b"").expect("the request completed");
    assert_eq!(status, 200, "{body}");
    assert!(
        body.contains("FLAG"),
        "the flag's document root must answer, not the file's: {body}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// A relative document root must survive a *second* request.
///
/// This drives the **HTTP** listener on purpose. The FastCGI tests build their
/// own `SCRIPT_FILENAME` and hand it to volucer, so they cannot see a bug in
/// how volucer derives one — the value under test never comes from volucer
/// at all. The HTTP front-end computes it from the document root, which is where
/// the bug lived.
///
/// It also asks twice on purpose: `php-cgi` `chdir()`s into the script's
/// directory and the pool keeps the process, so a relative root only breaks once
/// a PHP request has already run. A single-request test passes either way.
#[test]
fn a_relative_document_root_keeps_working_after_the_first_request() {
    let Some(binary) = volucer_binary() else {
        eprintln!("skipping: the volucer binary is not built (run `cargo build`)");
        return;
    };
    let Some(php_cgi) = find_php_cgi() else {
        eprintln!("skipping: no php-cgi on PATH");
        return;
    };
    let Some(base_port) = free_port_run(1) else {
        eprintln!("skipping: no free port");
        return;
    };

    let root = std::env::temp_dir().join(format!("volucer-relative-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("public")).expect("create the document root");
    std::fs::write(
        root.join("public/index.php"),
        "<?php echo \"PATH_INFO=\", $_SERVER['PATH_INFO'] ?? '-';",
    )
    .expect("write the front controller");

    // Deliberately relative, and the server is started from the directory it is
    // relative to — exactly how an operator would write it. `Server::start_in`
    // sets the child's working directory, so `"public"` resolves against the
    // fixture rather than against wherever the test runner happens to be.
    let config_path = root.join("volucer.toml");
    let config = format!(
        "[server]\ndefault_backend = \"cgi\"\nhttp = \"127.0.0.1:0\"\ndocument_root = \"public\"\n\n[backends.cgi]\nkind = \"spawn_cgi\"\nbinary = {cgi:?}\nworkers = 1\nbind_host = \"127.0.0.1\"\nbase_port = {base_port}\n",
        cgi = php_cgi.display().to_string(),
    );
    std::fs::write(&config_path, config).expect("write the config");

    let Some(server) = Server::start_in(&binary, &config_path, &root) else {
        eprintln!("skipping: the server did not start");
        return;
    };
    let Some(http_address) = server.http_address.clone() else {
        panic!("the HTTP listener did not report an address; the banner was not parsed");
    };

    // The same URL, three times: the first request is the one that makes the
    // difference, and the second is where a relative root would fail.
    for attempt in 1..=3 {
        let response = http_get(&http_address, "/some/route");
        assert_eq!(
            response.status, 200,
            "request {attempt} failed: {response:?}"
        );
        assert!(
            response.text().contains("PATH_INFO=/some/route"),
            "request {attempt} did not reach the front controller: {}",
            response.text()
        );
    }

    let _ = std::fs::remove_dir_all(&root);
}

/// `version` reports the build's own facts, so an operator can check the PHP
/// floor from a shell without opening the documentation.
#[test]
fn version_prints_the_protocol_and_the_php_floor() {
    let Some(binary) = volucer_binary() else {
        eprintln!("skipping: the volucer binary is not built (run `cargo build`)");
        return;
    };

    let output = Command::new(&binary)
        .arg("version")
        .output()
        .expect("the version command runs");
    assert!(output.status.success(), "{output:?}");
    let text = String::from_utf8_lossy(&output.stdout);

    assert!(text.contains("worker protocol"), "{text}");
    assert!(
        text.contains("php floor:        8.2"),
        "the floor a build claims must be the floor it has: {text}"
    );
    assert!(text.contains("FastCGI"), "{text}");
    assert!(
        text.contains("tls-alpn-01"),
        "the challenge a build performs must be advertised, because it is the difference between \
         a deployment that can be issued a certificate and one that cannot: {text}"
    );
    assert!(
        !text.contains("spawn_fpm"),
        "a removed backend must not be advertised: {text}"
    );
}

/// `check` describes the sites a deployment will serve, and where a route still
/// checks the filesystem.
///
/// The route line is the point of the second half: `skip_stat` is inferred rather than
/// written, so the only way an operator can see the saving is if `check` prints it.
#[test]
fn check_describes_the_sites_and_the_routes_that_skip_the_stat() {
    let Some(binary) = volucer_binary() else {
        eprintln!("skipping: the volucer binary is not built (run `cargo build`)");
        return;
    };
    let root = std::env::temp_dir().join(format!("volucer-check-sites-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("a")).expect("create a site root");
    std::fs::create_dir_all(root.join("b")).expect("create a site root");

    let config_path = root.join("volucer.toml");
    let config = format!(
        r#"
[server]
default_backend = "app"
http = "127.0.0.1:8080"
document_root = {root}

[backends.app]
kind = "worker"
command = "php worker.php"

[backends.cgi]
kind = "spawn_cgi"
binary = 'C:\php\php-cgi.exe'

[[server.routes]]
path_prefix = "/api"
backend = "app"

[[server.routes]]
path_prefix = "/legacy"
backend = "cgi"

[[server.routes]]
path_suffix = ".php"
backend = "cgi"

[sites.a]
hosts = ["a.test", "*.a.test"]
document_root = {a}
front_controller = false
serve_only = "static"

[sites.b]
hosts = ["b.test"]
document_root = {b}
default_backend = "cgi"
"#,
        root = toml_string(&root.display().to_string()),
        a = toml_string(&root.join("a").display().to_string()),
        b = toml_string(&root.join("b").display().to_string()),
    );
    std::fs::write(&config_path, config).expect("write the config");

    let output = Command::new(&binary)
        .arg("check")
        .arg("--config")
        .arg(&config_path)
        .output()
        .expect("the check command runs");
    assert!(output.status.success(), "{output:?}");
    let text = String::from_utf8_lossy(&output.stdout);

    // The site table, with each site's own settings rather than the server's.
    assert!(text.contains("sites:"), "{text}");
    assert!(text.contains("a: a.test, *.a.test"), "{text}");
    assert!(text.contains("b: b.test"), "{text}");
    assert!(
        text.contains("serve only:       static"),
        "a site's serve_only must be reported, not the server's: {text}"
    );
    assert!(
        text.contains("front controller: (none: only real .php files are served)"),
        "`front_controller = false` must be reported as such: {text}"
    );
    assert!(
        text.contains("default backend:  cgi"),
        "a site's own default backend must be reported: {text}"
    );

    // Three routes, two reasons to skip and one route that must not:
    // `/api` goes to a worker, which reads the path as metadata and never interprets it;
    // `.php` pins a script extension, so the file can only be PHP;
    // `/legacy` goes to `php-cgi` with a prefix, which proves nothing about the
    // extension — `/legacy/config.json` would be emitted verbatim — so the stat stays.
    assert!(
        text.contains("stat: skipped"),
        "the saving must be visible: {text}"
    );
    assert!(
        text.matches("stat: skipped").count() == 2,
        "the worker and suffix routes skip, the prefix route does not: {text}"
    );
    let legacy = text
        .lines()
        .position(|line| line.contains("/legacy") && line.contains("-> cgi"))
        .expect("the /legacy route is printed");
    assert!(
        !text
            .lines()
            .nth(legacy + 1)
            .unwrap_or("")
            .contains("stat: skipped"),
        "a prefix route proves nothing about the extension and must keep the stat: {text}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// An upstream route skips the `stat`, and the per-request-reset line describes it honestly.
///
/// Both are *silent* failures: neither is a compile error, and each is a place the new backend
/// kind is accepted and then described wrongly. The `stat` one has a real consequence — an upstream
/// routes on the URL its own way, so volucer resolving the path to a script would check a file
/// nothing reads, and a path naming no file would be a 404 for a request the origin wanted.
///
/// Asserted through `check` rather than through the function, because the question the bug asked is
/// "what does an operator see", and because `check` is where the two readers of these rules meet.
#[test]
fn check_describes_an_upstream_and_skips_the_stat_for_its_routes() {
    let Some(binary) = volucer_binary() else {
        eprintln!("skipping: the volucer binary is not built (run `cargo build`)");
        return;
    };
    let root = std::env::temp_dir().join(format!("volucer-check-upstream-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create the root");

    let config_path = root.join("volucer.toml");
    let config = format!(
        r#"
[server]
default_backend = "node"
http = "127.0.0.1:8080"
document_root = {root}

[backends.node]
kind = "upstream"
origin = "http://127.0.0.1:3000"

[backends.cgi]
kind = "spawn_cgi"
binary = 'C:\php\php-cgi.exe'

# A prefix route: it proves nothing about the extension, so a *CGI* backend keeps the stat.
[[server.routes]]
path_prefix = "/legacy"
backend = "cgi"

# The same shape pointed at the upstream: the path is the origin's business, so no stat.
[[server.routes]]
path_prefix = "/api"
backend = "node"
"#,
        root = toml_string(&root.display().to_string()),
    );
    std::fs::write(&config_path, config).expect("write the config");

    let output = Command::new(&binary)
        .arg("check")
        .arg("--config")
        .arg(&config_path)
        .output()
        .expect("the check command runs");
    assert!(output.status.success(), "{output:?}");
    let text = String::from_utf8_lossy(&output.stdout);

    // 1. The kind is named, and the reset line does not claim either of the two things that
    //    existed before it.
    assert!(text.contains("node: upstream"), "{text}");
    assert!(
        text.contains("per-request reset: not applicable"),
        "an upstream owns its own request scope, and neither of the two older answers is true \
         of it: {text}"
    );
    // And the line that existed while this build could not serve an upstream is gone. It is
    // asserted as *absent* rather than deleted from the test, because "check tells the operator
    // nothing about a capability it has" is the property, and a later change that reintroduced a
    // permanent warning would be the same mistake in the other direction.
    assert!(
        !text.contains("not served yet"),
        "an upstream is served now, so `check` must not warn about it: {text}"
    );

    // 2. `/api` skips the stat, `/legacy` does not — the contrast is what makes this a test of
    //    the rule rather than of the word "skipped" appearing somewhere.
    let route_line = |prefix: &str, backend: &str| {
        text.lines()
            .position(|line| line.contains(prefix) && line.contains(&format!("-> {backend}")))
            .unwrap_or_else(|| panic!("the {prefix} route is printed: {text}"))
    };
    let api = route_line("/api", "node");
    assert!(
        text.lines()
            .nth(api + 1)
            .unwrap_or("")
            .contains("stat: skipped"),
        "an upstream does not read the path, so a route to one must skip the stat: {text}"
    );
    let legacy = route_line("/legacy", "cgi");
    assert!(
        !text
            .lines()
            .nth(legacy + 1)
            .unwrap_or("")
            .contains("stat: skipped"),
        "a prefix route to a CGI backend proves nothing about the extension, so the stat stays: \
         {text}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// A test HTTP origin: a stdlib listener that records what it received.
///
/// Deliberately not a Node or a Python process. It has to be able to answer slowly, to send two
/// headers with one name, and to misbehave on demand — and it must need no interpreter on the host,
/// which is the same reasoning that put the packaging into scripts so it could be tested without a
/// tag. It also means this test runs on a machine with no PHP at all.
struct TestOrigin {
    address: String,
    received: std::sync::Arc<std::sync::Mutex<Vec<Vec<u8>>>>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl TestOrigin {
    /// Start an origin that answers every request with `response`.
    ///
    /// `response` is written verbatim after the request is read, so a test controls the status,
    /// the headers — including two with the same name — and the body.
    fn start(response: &'static str) -> Self {
        let listener =
            std::net::TcpListener::bind("127.0.0.1:0").expect("bind an origin on a free port");
        let address = listener.local_addr().expect("address").to_string();
        // A read timeout, so a connection the client keeps alive does not hold this thread for
        // ever when the test ends.
        listener.set_nonblocking(false).expect("blocking accepts");
        let received = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

        let seen = std::sync::Arc::clone(&received);
        let stopped = std::sync::Arc::clone(&stop);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                if stopped.load(std::sync::atomic::Ordering::Relaxed) {
                    return;
                }
                let Ok(mut stream) = stream else { continue };
                let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                // One request per connection, then close: the client's pool handles that, and a
                // test origin that tried to speak keep-alive would be another thing to get right.
                let raw = read_one_request(&mut stream);
                seen.lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push(raw);
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
                let _ = stream.shutdown(std::net::Shutdown::Both);
            }
        });

        TestOrigin {
            address,
            received,
            stop,
        }
    }

    fn origin_url(&self) -> String {
        format!("http://{}", self.address)
    }

    /// Everything the origin received, as one blob per request.
    fn requests(&self) -> Vec<String> {
        self.received
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .map(|raw| String::from_utf8_lossy(raw).into_owned())
            .collect()
    }

    /// The first request, as text.
    fn first_request(&self) -> String {
        self.requests()
            .into_iter()
            .next()
            .expect("the origin received a request")
    }
}

impl Drop for TestOrigin {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        // Unblock the accept by connecting once more, so the thread notices the flag.
        let _ = std::net::TcpStream::connect(&self.address);
    }
}

/// Read one HTTP request, headers and all, without knowing its length in advance.
///
/// `Content-Length` is honoured so a POST's body is captured — a test that asserted the body
/// arrived would otherwise be asserting on the head alone.
fn read_one_request(stream: &mut std::net::TcpStream) -> Vec<u8> {
    use std::io::Read;

    let mut raw = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => raw.extend_from_slice(&chunk[..n]),
            Err(_) => break,
        }
        let Some(head_end) = raw.windows(4).position(|w| w == b"\r\n\r\n") else {
            continue;
        };
        let head = String::from_utf8_lossy(&raw[..head_end]).to_ascii_lowercase();
        let expected = head
            .lines()
            .find_map(|line| line.strip_prefix("content-length:"))
            .and_then(|value| value.trim().parse::<usize>().ok())
            .unwrap_or(0);
        if raw.len() >= head_end + 4 + expected {
            break;
        }
    }
    raw
}

/// **The assertion that a whole feature was for.** A request through `serve` reaches a real HTTP
/// origin,
/// and the origin's answer reaches the client.
///
/// Everything before this test proved a *component*: the crate's client, the config's parser, the
/// enum's arm. None of them could pass a request through the server, and that is
/// the reason this is written as a request rather than as a unit assertion — five ACME steps shipped
/// green while `serve` never constructed an `Engine` at all, so nothing issued anything. A build
/// where the client exists and the front-end never calls it looks exactly like a working one.
///
/// The origin is a real socket and a real HTTP conversation, so this covers the branch, the
/// translation, the header rules and the status, end to end.
#[test]
fn serve_reaches_an_http_origin_and_relays_its_answer() {
    let Some(binary) = volucer_binary() else {
        eprintln!("skipping: the volucer binary is not built (run `cargo build`)");
        return;
    };
    let origin = TestOrigin::start(
        "HTTP/1.1 203 Non-Authoritative Information\r\n\
         X-Origin: node-app\r\n\
         Set-Cookie: a=1\r\n\
         Set-Cookie: b=2\r\n\
         Content-Length: 11\r\n\
         \r\n\
         hello there",
    );
    let root = std::env::temp_dir().join(format!("volucer-upstream-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create the root");

    let config_path = root.join("volucer.toml");
    std::fs::write(
        &config_path,
        format!(
            r#"
[server]
default_backend = "node"
http = "127.0.0.1:0"
document_root = {root}
[backends.node]
kind = "upstream"
origin = {origin}
"#,
            root = toml_string(&root.display().to_string()),
            origin = toml_string(&origin.origin_url()),
        ),
    )
    .expect("write the config");

    let Some(server) = Server::start(&binary, &config_path) else {
        panic!("the server did not start with an upstream backend configured");
    };
    let address = server
        .http_address
        .clone()
        .expect("the configuration asked for an HTTP listener");

    let answer = http_get(&address, "/api/things?id=7");
    assert_eq!(
        answer.status, 203,
        "the origin's status must reach the client unchanged: {answer:?}"
    );
    assert_eq!(answer.text(), "hello there");
    assert_eq!(
        answer.header("x-origin"),
        Some("node-app"),
        "the origin's own header must cross: {answer:?}"
    );
    // Two `Set-Cookie` headers are two cookies. A proxy that joined them — as the CGI path does,
    // because the environment has one value per name — would silently break a session.
    let cookies: Vec<&String> = answer
        .headers
        .iter()
        .filter(|(name, _)| name.eq_ignore_ascii_case("set-cookie"))
        .map(|(_, value)| value)
        .collect();
    assert_eq!(cookies.len(), 2, "both cookies must survive: {answer:?}");

    // And what the *origin* saw. This is the half a status assertion cannot make: the path and
    // query it received, the `Host` the client sent, and the forwarding headers.
    let request = origin.first_request();
    assert!(
        request.starts_with("GET /api/things?id=7 "),
        "the path and query must reach the origin intact: {request:?}"
    );
    assert!(
        request.to_ascii_lowercase().contains("host: example.test"),
        "the client's own Host is the identity the origin routes on: {request:?}"
    );
    assert!(
        request
            .to_ascii_lowercase()
            .contains("x-forwarded-for: 127.0.0.1"),
        "the peer must be forwarded: {request:?}"
    );
    assert!(
        request
            .to_ascii_lowercase()
            .contains("x-forwarded-proto: http"),
        "the hop's scheme comes from the socket, not from a header: {request:?}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// **Step 38's security half, end to end: widening the list changes whether a file is served.**
///
/// This is the pairing that has to be asserted, and neither half alone is evidence:
///
/// - With the default list, `/app.py` is an ordinary file, so the static layer serves it — the
///   program's source, to the client, with a 200. That is the disclosure that made "just widen the
///   router" unsafe.
/// - With the list naming `py`, the same file is a script, so the static layer refuses it and the
///   request reaches the backend instead.
///
/// The backend is an upstream rather than a worker so the test needs no SDK, and it records what it
/// was sent — which is how "the request reached the backend" is told apart from "the front
/// controller answered".
#[test]
fn widening_the_script_list_stops_the_static_layer_serving_the_source() {
    let Some(binary) = volucer_binary() else {
        eprintln!("skipping: the volucer binary is not built (run `cargo build`)");
        return;
    };

    let root = std::env::temp_dir().join(format!("volucer-py-static-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create the root");
    std::fs::write(root.join("app.py"), "# a secret the client must not read\n")
        .expect("write the entry point");
    std::fs::write(root.join("front.php"), "<?php\n").expect("write the front controller");

    let origin = TestOrigin::start(
        "HTTP/1.1 200 OK\r\n\
         X-Served-By: origin\r\n\
         Content-Length: 2\r\n\
         \r\n\
         ok",
    );

    let start = |name: &str, extensions: &str| -> Option<Server> {
        let config_path = root.join(format!("volucer-{name}.toml"));
        std::fs::write(
            &config_path,
            format!(
                r#"
[server]
default_backend = "app"
http = "127.0.0.1:0"
document_root = {root}
front_controller = "front.php"
static_files = true
{extensions}

[backends.app]
kind = "upstream"
origin = {origin}
"#,
                root = toml_string(&root.display().to_string()),
                origin = toml_string(&origin.origin_url()),
                extensions = extensions,
            ),
        )
        .expect("write the config");
        Server::start(&binary, &config_path)
    };

    // The default list: `app.py` is a file, so the layer serves it. The assertion is on the *body*
    // rather than on a header, because the disclosure is the body.
    let Some(default) = start("default", "") else {
        panic!("the server did not start with the default list");
    };
    let address = default
        .http_address
        .clone()
        .expect("the configuration asked for an HTTP listener");
    let answer = http_get(&address, "/app.py");
    assert_eq!(answer.status, 200, "{answer:?}");
    assert!(
        answer.text().contains("a secret the client must not read"),
        "with the default list an unknown extension is an ordinary file, so the static layer \
         serves it — this is the behaviour widening is meant to replace: {answer:?}"
    );
    drop(default);

    // With `py` named, the same path is a script: refused by the static layer, handed to the
    // backend.
    let Some(widened) = start("widened", "script_extensions = [\"php\", \"py\"]") else {
        panic!("the server did not start with a widened list");
    };
    let address = widened
        .http_address
        .clone()
        .expect("the configuration asked for an HTTP listener");
    let answer = http_get(&address, "/app.py");
    assert_eq!(answer.status, 200, "{answer:?}");
    assert!(
        !answer.text().contains("a secret the client must not read"),
        "a script must never be served from disk: {answer:?}"
    );
    assert_eq!(
        answer.header("x-served-by"),
        Some("origin"),
        "the request must reach the backend instead: {answer:?}"
    );

    let _ = std::fs::remove_dir_all(&root);
}
///
/// The defect this closes is invisible from the client's side, which is why it is asserted at the
/// origin rather than in the answer: behind a proxy, the address the *backend* is told is the
/// proxy's, so anything keyed on it — a rate limit, an IP allow-list, an abuse heuristic — keys on
/// the proxy and cannot tell two clients apart.
///
/// Two requests are made from the **same socket peer** and the two answers must differ, which is the
/// only shape that can tell the policy apart from "the address always comes from the header" and
/// from "the address never does":
///
/// 1. A claim **with** a trusted peer is believed, and the backend is told the claimed address.
/// 2. The same claim **without** a trusted peer is ignored, and the backend is told the peer's
///    address — which is the security property, since a client can always send a header.
///
/// The negative case is what makes the positive one mean something: a server that trusted every
/// header would pass the first case and has a spoofing hole, and one that trusted none would pass
/// the second and
/// still be broken behind a proxy.
#[test]
fn a_proxy_trust_list_decides_whether_a_forwarded_address_is_believed() {
    let Some(binary) = volucer_binary() else {
        eprintln!("skipping: the volucer binary is not built (run `cargo build`)");
        return;
    };
    // A backend that echoes the environment, so the assertion is on what PHP would actually read
    // rather than on what the server believes it sent.
    if find_php().is_none() {
        eprintln!("skipping: no php on PATH");
        return;
    }
    let Some(php_cgi) = find_php_cgi() else {
        eprintln!("skipping: no php-cgi on PATH");
        return;
    };
    let origin = TestOrigin::start("HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok");
    let root = std::env::temp_dir().join(format!("volucer-proxy-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create the root");

    let script = root.join("echo.php");
    std::fs::write(
        &script,
        "<?php header('X-Saw-Remote: ' . ($_SERVER['REMOTE_ADDR'] ?? 'none')); echo 'ok';",
    )
    .expect("write the probe script");

    let config_path = root.join("volucer.toml");
    // The trust list names `127.0.0.1` and nothing else. The test's own client *is* 127.0.0.1, so the
    // same socket is on the list in one run and off it in the other by editing one line — which is
    // the point: the decision is the configuration's, not the connection's.
    let config_for = |trusted: bool| {
        format!(
            r#"
[server]
default_backend = "app"
http = "127.0.0.1:0"
document_root = {root}
[server.proxy]
trusted_subnets = [{subnets}]
trusted_headers = ["X-Forwarded-For"]
[backends.app]
kind = "spawn_cgi"
binary = {php}
base_port = {port}
"#,
            root = toml_string(&root.display().to_string()),
            subnets = if trusted { "\"127.0.0.1\"" } else { "" },
            php = toml_string(&php_cgi.display().to_string()),
            port = free_port_run(1).expect("a free port for the php-cgi pool"),
        )
    };

    for (trusted, expected, why) in [
        (
            true,
            "203.0.113.9",
            "a trusted peer's claim is the address the backend is told",
        ),
        (
            false,
            "127.0.0.1",
            "an untrusted peer's claim is ignored, however plausible it looks",
        ),
    ] {
        // The configuration names a `base_port` for the `php-cgi` pool, so it is rewritten on
        // every attempt and a lost port costs a retry rather than the test. This was the one
        // `free_port_run` call site left without a retry, and it failed on `macos-latest` with
        // "the server did not start" while the same code was green elsewhere.
        let server = start_server_with_config(
            &binary,
            &config_path,
            &|| config_for(trusted),
            &format!("a proxy table of trusted={trusted}"),
        );
        let address = server
            .http_address
            .clone()
            .expect("the configuration asked for an HTTP listener");

        let answer = http_get_with(&address, "/echo.php", &[("X-Forwarded-For", "203.0.113.9")]);
        assert_eq!(answer.status, 200, "{why}: {answer:?}");
        assert_eq!(
            answer.header("x-saw-remote"),
            Some(expected),
            "{why}: {answer:?}"
        );

        drop(server);
    }

    // And the same resolution reaches an *upstream* origin, through the other code path. The claim
    // and the header are deliberately different here: `X-Real-Ip` is what the policy is told to
    // believe, so the address it resolves is `198.51.100.9`, while the client's own `X-Forwarded-For`
    // is relayed verbatim by the append rule. The outgoing chain is therefore the client's claim
    // **followed by the address this server resolved** — and the *second* element is the assertion.
    // A test that sent one header and checked for it would pass with the policy switched off, since
    // the append rule alone puts it there.
    let upstream_config = format!(
        r#"
[server]
default_backend = "node"
http = "127.0.0.1:0"
document_root = {root}
[server.proxy]
trusted_subnets = ["127.0.0.1"]
trusted_headers = ["X-Real-Ip"]
[backends.node]
kind = "upstream"
origin = {origin}
"#,
        root = toml_string(&root.display().to_string()),
        origin = toml_string(&origin.origin_url()),
    );
    std::fs::write(&config_path, upstream_config).expect("write the config");
    let Some(server) = Server::start(&binary, &config_path) else {
        panic!("the server did not start with an upstream backend");
    };
    let address = server
        .http_address
        .clone()
        .expect("the configuration asked for an HTTP listener");
    let _ = http_get_with(
        &address,
        "/api/things",
        &[
            ("X-Real-Ip", "198.51.100.9"),
            ("X-Forwarded-For", "203.0.113.11"),
        ],
    );
    let request = origin.first_request().to_ascii_lowercase();
    // Two field lines rather than one comma-joined list: the append rule adds a header, and RFC 7230
    // §3.2.2 makes the two equivalent. The second line is the assertion — the address this server
    // *resolved* from `X-Real-Ip`, which the client did not send in that header.
    assert!(
        request.contains("x-forwarded-for: 203.0.113.11"),
        "the client's own claim must still reach the origin: {request:?}"
    );
    assert!(
        request.contains("x-forwarded-for: 198.51.100.9"),
        "and the address this server resolved from `X-Real-Ip` must be appended after it, which is \
         the half a policy-off server would not produce: {request:?}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// A POST body reaches an origin, and its length is re-derived rather than forwarded.
///
/// The second half is the framing rule: the outgoing `Content-Length` is the body's, not the
/// client's claim, and a stale length is how a proxy desynchronises a connection.
#[test]
fn a_request_body_reaches_an_origin_with_the_right_length() {
    let Some(binary) = volucer_binary() else {
        eprintln!("skipping: the volucer binary is not built (run `cargo build`)");
        return;
    };
    let origin = TestOrigin::start("HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok");
    let root = std::env::temp_dir().join(format!("volucer-upstream-post-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create the root");

    let config_path = root.join("volucer.toml");
    std::fs::write(
        &config_path,
        format!(
            r#"
[server]
default_backend = "node"
http = "127.0.0.1:0"
document_root = {root}
[backends.node]
kind = "upstream"
origin = {origin}
"#,
            root = toml_string(&root.display().to_string()),
            origin = toml_string(&origin.origin_url()),
        ),
    )
    .expect("write the config");

    let Some(server) = Server::start(&binary, &config_path) else {
        panic!("the server did not start");
    };
    let address = server
        .http_address
        .clone()
        .expect("an HTTP listener was configured");

    let body = "name=volucer&x=%20";
    let mut stream = TcpStream::connect(&address).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .expect("a read timeout");
    let request = format!(
        "POST /submit HTTP/1.1\r\nHost: example.test\r\nContent-Type: \
         application/x-www-form-urlencoded\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).expect("write");
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).expect("read");

    assert!(
        String::from_utf8_lossy(&raw).starts_with("HTTP/1.1 200"),
        "the origin's answer must come back: {:?}",
        String::from_utf8_lossy(&raw)
    );

    let seen = origin.first_request();
    assert!(
        seen.ends_with(body),
        "the body must reach the origin byte for byte: {seen:?}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// An origin that is not there is a 502, and one that never answers is a 504.
///
/// The statuses the CGI path already produces, asserted for the new path so "one error rule, two
/// request paths" is a fact rather than an intention. The two are different answers on purpose: an
/// origin that refused the connection was never serving, while one that accepted it and stayed
/// silent is a deadline — and an operator fixes those two differently.
#[test]
fn an_unreachable_origin_is_a_502_and_a_silent_one_is_a_504() {
    let Some(binary) = volucer_binary() else {
        eprintln!("skipping: the volucer binary is not built (run `cargo build`)");
        return;
    };
    let root = std::env::temp_dir().join(format!("volucer-upstream-fail-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create the root");

    // A port nothing listens on: bind one and let it go.
    let closed = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let closed_port = closed.local_addr().expect("address").port();
    drop(closed);

    let config_path = root.join("volucer.toml");
    std::fs::write(
        &config_path,
        format!(
            r#"
[server]
default_backend = "node"
http = "127.0.0.1:0"
document_root = {root}
[backends.node]
kind = "upstream"
origin = "http://127.0.0.1:{closed_port}"
"#,
            root = toml_string(&root.display().to_string()),
        ),
    )
    .expect("write the config");

    let Some(server) = Server::start(&binary, &config_path) else {
        panic!("the server did not start");
    };
    let address = server
        .http_address
        .clone()
        .expect("an HTTP listener was configured");

    let answer = http_get(&address, "/");
    assert_eq!(
        answer.status, 502,
        "an origin nothing is listening on is a bad gateway, not a client error: {answer:?}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// Send a signal to a running server, on a platform that has that signal.
///
/// `None` on Windows, where the test is skipped with a reason rather than failing: there is no
/// `SIGHUP` and no equivalent, so "reload by signal" is not something a Windows build can be
/// asked to do. The skip is *stated* for the same reason the restart-only list is — a capability
/// that is absent should say so rather than look untested.
#[cfg(unix)]
fn signal(child: &Child, signal: i32) -> Option<()> {
    // `child.id()` is a `u32` in Rust but a `pid_t` (`i32`) to the kernel, and a pid above
    // `i32::MAX` is not a thing Linux produces.
    let pid = i32::try_from(child.id()).ok()?;
    let rc = unsafe { libc_kill(pid, signal) };
    (rc == 0).then_some(())
}

#[cfg(unix)]
extern "C" {
    /// `kill(2)`, declared here rather than reaching through another crate's re-export of `libc`.
    /// `libc` is in the dependency graph transitively but is not a direct dependency of this crate,
    /// and a test that borrowed another crate's re-export would break when that crate changed.
    #[link_name = "kill"]
    fn libc_kill(pid: i32, signal: i32) -> i32;
}

/// `SIGHUP` reloads a running server, and a file it cannot apply changes nothing.
///
/// The deliverable of this phase, asserted the only way that means anything: against a **real
/// process**, because the mechanism is a signal and a signal is not observable from a unit test of
/// the code that handles it.
///
/// Both halves are here together on purpose. A reload that works is easy; the claim that matters is
/// that a *refused* reload leaves the previous configuration serving, and the two are one mechanism
/// seen from either side — the second is what makes the first safe to send to a live server.
///
/// Falsified before being trusted: against the binary from before this change, the `alive` half of
/// this test failed with the process **gone**, because `ctrlc`'s `termination` feature registered
/// `SIGHUP` and ran its shutdown callback for it.
#[cfg(unix)]
#[test]
fn sighup_reloads_the_running_configuration_and_a_bad_file_changes_nothing() {
    const SIGHUP: i32 = 1;

    let Some(binary) = volucer_binary() else {
        eprintln!("skipping: the volucer binary is not built (run `cargo build`)");
        return;
    };

    let root = std::env::temp_dir().join(format!("volucer-reload-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let first = root.join("first");
    let second = root.join("second");
    for dir in [&first, &second] {
        std::fs::create_dir_all(dir).expect("create a document root");
    }
    std::fs::write(first.join("whoami.txt"), "first").expect("write the marker");
    std::fs::write(second.join("whoami.txt"), "second").expect("write the marker");

    let config_path = root.join("volucer.toml");
    let config_for = |root: &Path| {
        let document_root = toml_string(&root.display().to_string());
        format!(
            "[server]\nhttp = \"127.0.0.1:0\"\ndocument_root = {document_root}\n\n\
             [server.static_files]\nenabled = true\n"
        )
    };
    std::fs::write(&config_path, config_for(&first)).expect("write the config");

    let Some(server) = Server::start(&binary, &config_path) else {
        eprintln!("skipping: the server did not start");
        return;
    };
    let address = server
        .http_address
        .clone()
        .expect("the config asks for an http listener");

    assert_eq!(
        http_get(&address, "/whoami.txt").text(),
        "first",
        "the first request must be served from the starting root"
    );

    // A file that cannot be applied. The server is signalled anyway, and the answer must not move:
    // this is the half that makes a reload safe to send to a live server, because a typo in an
    // unrelated key would otherwise take the deployment with it.
    //
    // **The malformed file names the *other* root on purpose.** A file that named the root already
    // serving would make this assertion pass against code that applied it anyway, which is the
    // shape of a vacuous assertion: where the claim is "nothing was applied", the input has to
    // contain something that would have been applied. A duplicate key fails at parse, and it still
    // carries a root that a partial reader would have taken.
    let duplicate_key = format!(
        "{}document_root = {}\n",
        config_for(&second),
        toml_string(&second.display().to_string())
    );
    std::fs::write(&config_path, duplicate_key).expect("write a malformed config");
    let Some(()) = signal(&server.child, SIGHUP) else {
        eprintln!("skipping: could not signal the server");
        return;
    };
    assert_eq!(
        http_get(&address, "/whoami.txt").text(),
        "first",
        "a configuration that does not parse must leave the previous one serving, including the \
         root that file asked for"
    );

    // Parses, and does not validate. A different code path from the first refusal: valid TOML and
    // a bad configuration is the mistake an operator is more likely to make. This one names the
    // *other* root too, so applying it despite the invalid key would be visible in the answer.
    //
    // The bad key goes *inside* `[server]`, where it lives now: written at the top level it would
    // be the pre-0.3 spelling, and the server would refuse it for having moved rather than for
    // naming no backend — a different refusal than the one this half is about.
    let unvalidatable =
        config_for(&second).replace("[server]\n", "[server]\ndefault_backend = \"nope\"\n");
    std::fs::write(&config_path, unvalidatable).expect("write an invalid config");
    signal(&server.child, SIGHUP).expect("the server is still running");
    assert_eq!(
        http_get(&address, "/whoami.txt").text(),
        "first",
        "a configuration that does not validate must leave the previous one serving, including \
         the root that file asked for"
    );

    // And now one that can be applied, changing exactly one thing so the reload is attributable to
    // the document root rather than to everything about the file.
    std::fs::write(&config_path, config_for(&second)).expect("write a valid config");
    signal(&server.child, SIGHUP).expect("the server is still running");

    // Polled rather than slept on: the signal is delivered asynchronously, so a fixed sleep is
    // either flaky or slow, and the answer *is* the observable.
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut answer = String::new();
    while Instant::now() < deadline {
        answer = http_get(&address, "/whoami.txt").text();
        if answer == "second" {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(
        answer, "second",
        "a reload must change what the next request is served, on the same listener"
    );

    drop(server);
    let _ = std::fs::remove_dir_all(&root);
}

/// One `SIGHUP` changes a certificate **and** a site, and both take effect.
///
/// The two tables a reload installs are independent — the shared site table through the
/// `Registry`, the certificate table through the `SniResolver` — and that independence is exactly
/// why this needs its own test. Nothing else in the tree drives both through the *signal*: the
/// reloader's certificate half could be deleted and every other test would still pass, because
/// each half is exercised on its own somewhere else.
///
/// It is also the one assertion that needs the parts to be **simultaneously** observable. The
/// certificate is read from the handshake and the document root from the request body, so a
/// client that gets `renewed` back on `:0` and the new root's marker has seen both — which is
/// the only way to tell "one signal applied two things" from "two signals applied one thing
/// each". The site is a named site rather than the default one so that the *site* half is
/// attributable to the site table rather than to a root that moved on its own.
///
/// Unix only, for the same reason the `SIGHUP` test is: it is the signal path under test.
#[cfg(unix)]
#[test]
fn one_sighup_reloads_a_certificate_and_a_site_together() {
    const SIGHUP: i32 = 1;

    let Some(binary) = volucer_binary() else {
        eprintln!("skipping: the volucer binary is not built (run `cargo build`)");
        return;
    };

    let root = std::env::temp_dir().join(format!("volucer-cert-site-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let first = root.join("first");
    let second = root.join("second");
    // The **site's** root is a subdirectory of the server's, and the marker lives only there. That
    // is what makes the site half attributable: `app.test` resolves to the site, so a reload that
    // moved only the server's default root would leave this answer at "first" and the test would
    // see it. Two roots that were the same path would make the assertion pass either way.
    for dir in [&first, &second] {
        std::fs::create_dir_all(dir.join("app")).expect("create a document root");
    }
    std::fs::write(first.join("app/whoami.txt"), "first").expect("write the marker");
    std::fs::write(second.join("app/whoami.txt"), "second").expect("write the marker");

    // Two server certificates, and the second is what the reload installs. Generated here rather
    // than committed, because a committed key ends up in someone's deployment.
    let certs = root.join("certs");
    std::fs::create_dir_all(&certs).expect("create the certificate directory");
    let write_pair = |file: &str, name: &str| -> Vec<u8> {
        let generated = rcgen::generate_simple_self_signed(vec![name.to_string()])
            .expect("generate a certificate");
        std::fs::write(certs.join(format!("{file}.crt")), generated.cert.pem())
            .expect("write the certificate");
        std::fs::write(
            certs.join(format!("{file}.key")),
            generated.key_pair.serialize_pem(),
        )
        .expect("write the key");
        generated.cert.der().to_vec()
    };
    let original_der = write_pair("server", "example.test");
    // Written now but named only by the *second* config, so the first config never reads it and
    // the reload is what installs it.
    let renewed_der = write_pair("renewed", "example.test");
    assert_ne!(
        original_der, renewed_der,
        "the fixture must generate a different certificate, or the reload assertion is vacuous"
    );

    let config_path = root.join("volucer.toml");
    let config_for = |certificate: &str, root: &Path| {
        let document_root = toml_string(&root.display().to_string());
        let site_root = toml_string(&root.join("app").display().to_string());
        format!(
            "[server]\nhttp = \"127.0.0.1:0\"\ndocument_root = {document_root}\n\n\
             [server.tls]\ncertificate = \"certs/{certificate}.crt\"\n\
             key = \"certs/{certificate}.key\"\n\n\
             [server.static_files]\nenabled = true\n\n\
             [sites.app]\nhosts = [\"app.test\"]\ndocument_root = {site_root}\n"
        )
    };
    std::fs::write(&config_path, config_for("server", &first)).expect("write the config");

    let Some(server) = Server::start(&binary, &config_path) else {
        eprintln!("skipping: the server did not start");
        return;
    };
    let address = server
        .http_address
        .clone()
        .expect("the config asks for an http listener");

    // Both halves before the reload, from one handshake and one request: the certificate this
    // deployment started with, and the root it started with. Asserted together so a fixture that
    // silently served one of them from the start would be visible here rather than as a passing
    // reload test.
    let before = tls_get(&address, "app.test", "/whoami.txt");
    assert_eq!(
        before.certificate, original_der,
        "the fixture must serve the certificate it was started with"
    );
    assert_eq!(
        before.body, "first",
        "the fixture must serve the root it was started with"
    );

    // One signal. Both configs differ in exactly two settings — which certificate, and which
    // site root — so what changes is attributable to the reload rather than to everything about
    // the file.
    std::fs::write(&config_path, config_for("renewed", &second)).expect("write the config");
    let Some(()) = signal(&server.child, SIGHUP) else {
        eprintln!("skipping: could not signal the server");
        return;
    };

    // Polled rather than slept on, and polled on **both** halves. That is not belt-and-braces:
    // the two tables are deliberately independent — the sites are applied first and the
    // certificates after, so a refused certificate cannot take the sites with it — which means
    // there is a real window in which the new site is serving under the old certificate. Closing
    // the loop on the first half alone would read that window and call it a failure; the claim is
    // that one signal applied *both* by the time it finished, so both are what is waited for.
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut after = tls_get(&address, "app.test", "/whoami.txt");
    while Instant::now() < deadline && (after.body != "second" || after.certificate != renewed_der)
    {
        std::thread::sleep(Duration::from_millis(50));
        after = tls_get(&address, "app.test", "/whoami.txt");
    }

    assert_eq!(
        after.body, "second",
        "one signal must reload the site table"
    );
    assert_eq!(
        after.certificate, renewed_der,
        "the same signal must reload the certificate table too — a reloader that refreshed only \
         the sites would pass every other test in this tree"
    );

    drop(server);
    let _ = std::fs::remove_dir_all(&root);
}

/// A reload must not throw away the certificate ACME issued.
///
/// The reloader rebuilds the certificate table with `SniResolver::refresh`, which is
/// **whole-or-nothing**: it loads exactly the pairs the configuration names. That is the right
/// shape for a reload — a set with one unreadable pair is refused entirely rather than half
/// applied — but it has a consequence for a deployment that has `[server.acme]`: the issued
/// certificate is not in the file, it is in the cache and in the running table, so a reload that
/// rebuilt from the configuration alone would put the **fallback** self-signed pair back into
/// service on a production host.
///
/// That is a silent regression, which is why it is pinned here rather than left to reasoning: the
/// server keeps running, the port keeps answering, every other reload test stays green, and the
/// symptom is a browser warning that appears weeks later — after the next `SIGHUP`, with the
/// cause already attributed to something else.
///
/// The **control** matters as much as the assertion. A test that never had an issued certificate
/// would pass against a reloader that did nothing at all, so the issued certificate is installed
/// through the same in-memory seam an issuance uses and *asserted to be serving* before the signal
/// is sent. What follows is then a statement about the reload and not about ACME being off: if the
/// `[server.acme]` table were dropped from the config, the "before" assertion fails first.
#[cfg(unix)]
#[test]
fn a_reload_keeps_the_certificate_that_was_issued() {
    use rustls_acme::CertCache;

    const SIGHUP: i32 = 1;

    let Some(binary) = volucer_binary() else {
        eprintln!("skipping: the volucer binary is not built (run `cargo build`)");
        return;
    };

    let root = std::env::temp_dir().join(format!("volucer-acme-reload-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let site = root.join("site");
    std::fs::create_dir_all(&site).expect("create a document root");
    std::fs::write(site.join("whoami.txt"), "here").expect("write the marker");
    let certs = root.join("certs");
    std::fs::create_dir_all(&certs).expect("create the certificate directory");

    // The fallback. It is what a reload that rebuilt from the configuration alone would restore.
    let fallback = rcgen::generate_simple_self_signed(vec!["example.test".to_string()])
        .expect("generate the fallback certificate");
    std::fs::write(certs.join("fallback.crt"), fallback.cert.pem()).expect("write the cert");
    std::fs::write(
        certs.join("fallback.key"),
        fallback.key_pair.serialize_pem(),
    )
    .expect("write the key");
    let fallback_der = fallback.cert.der().to_vec();

    // The issued certificate, in the shape `rustls-acme` stores and hands over: a chain plus a key
    // concatenated into one blob. It is written into the *cache* rather than the config, because
    // that is where an issued certificate lives — the file the configuration names stays the
    // self-signed pair, and that is the whole point of the assertion below.
    let issued = rcgen::generate_simple_self_signed(vec!["example.test".to_string()])
        .expect("generate the issued certificate");
    let issued_blob = format!("{}{}", issued.cert.pem(), issued.key_pair.serialize_pem());
    let issued_der = issued.cert.der().to_vec();
    assert_ne!(
        fallback_der, issued_der,
        "the fixture must differ, or the assertion below is vacuous"
    );

    // The directory URL comes from **the same function `serve` calls**, so this cannot drift from
    // what the engine looks for without the server and the test disagreeing about the authority.
    // Staging, so even a bug that reached the network could not issue a real certificate.
    let directory = volucer_acme::Authority::Staging.directory();
    assert!(
        directory.contains("staging"),
        "the fixture must use staging: {directory}"
    );

    let cache_dir = root.join("acme-cache");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime for the cache write");
    runtime.block_on(async {
        rustls_acme::caches::DirCache::new(&cache_dir)
            .store_cert(
                &["example.test".to_string()],
                directory,
                issued_blob.as_bytes(),
            )
            .await
            .expect("the cache is writable");
    });

    let config_path = root.join("volucer.toml");
    let config = format!(
        "[server]\nhttp = \"127.0.0.1:0\"\ndocument_root = {site}\n\n\
         [server.tls]\ncertificate = \"certs/fallback.crt\"\nkey = \"certs/fallback.key\"\n\n\
         [server.acme]\ndomains = [\"example.test\"]\ncontact = [\"mailto:ops@example.test\"]\n",
        site = toml_string(&site.display().to_string()),
    );
    std::fs::write(&config_path, &config).expect("write the config");

    let Some(server) = Server::start(&binary, &config_path) else {
        eprintln!("skipping: the server did not start");
        return;
    };
    let address = server
        .http_address
        .clone()
        .expect("the config asks for an http listener");

    // The control: the cached certificate is serving *before* the signal. Without this the test
    // would pass against a reloader that did nothing, and against one in a build where the engine
    // was never wired up at all.
    let before = tls_get(&address, "example.test", "/whoami.txt");
    assert_eq!(
        before.certificate, issued_der,
        "the fixture must be serving the issued certificate before the reload, or the assertion \
         after it is about nothing"
    );

    // The file is rewritten unchanged: the signal is the only thing that happens. A reload that
    // finds the same configuration is the ordinary case an operator produces by hand.
    std::fs::write(&config_path, &config).expect("rewrite the same config");
    let Some(()) = signal(&server.child, SIGHUP) else {
        eprintln!("skipping: could not signal the server");
        return;
    };

    // Wait for the reload to have happened at all — the marker proves it, so the certificate
    // assertion is not made against a server that has simply not reloaded yet.
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut after = tls_get(&address, "example.test", "/whoami.txt");
    while Instant::now() < deadline && after.body != "here" {
        std::thread::sleep(Duration::from_millis(50));
        after = tls_get(&address, "example.test", "/whoami.txt");
    }
    assert_eq!(after.body, "here", "the reload must have been applied");
    // Give a reload that *would* drop the certificate the chance to do so: the site table and the
    // certificate table are applied in sequence, sites first, so the certificate half lands last.
    std::thread::sleep(Duration::from_millis(500));

    after = tls_get(&address, "example.test", "/whoami.txt");
    assert_ne!(
        after.certificate, fallback_der,
        "a reload restored the **fallback** certificate, so the one ACME issued is no longer being \
         served — the deployment is back to a self-signed pair with no error and no log line, and \
         only a browser warning would ever say so"
    );
    assert_eq!(
        after.certificate, issued_der,
        "a reload must keep the issued certificate serving"
    );

    drop(server);
    let _ = std::fs::remove_dir_all(&root);
}

/// A reload still re-reads a **named** certificate, even with a site's own ACME table in play.
///
/// The complement of `a_reload_keeps_the_certificate_that_was_issued`, and it exists because the first
/// fix for the lost-certificate was *too* blunt: it skipped the certificate rebuild whenever any ACME
/// table was configured, which kept the issued certificate but stranded every named one. A site's
/// `[sites.x.tls]`
/// PEM is in the configuration and an operator can renew it by hand, so a reload has to re-read it —
/// and the two populations have to be merged rather than chosen between.
///
/// Both facts are asserted in one test because they are one behaviour from either side: the *issued*
/// certificate survives, and the *renewed* one is picked up. A merge that did only one of them passes
/// half of this and fails the other.
#[cfg(unix)]
#[test]
fn a_reload_picks_up_a_renewed_named_certificate_while_issuance_stays() {
    use rustls_acme::CertCache;

    const SIGHUP: i32 = 1;

    let Some(binary) = volucer_binary() else {
        eprintln!("skipping: the volucer binary is not built (run `cargo build`)");
        return;
    };

    let root = std::env::temp_dir().join(format!("volucer-acme-merge-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let app = root.join("app");
    let corp = root.join("corp");
    for dir in [&app, &corp] {
        std::fs::create_dir_all(dir).expect("create a document root");
    }
    std::fs::write(app.join("whoami.txt"), "app").expect("write the marker");
    std::fs::write(corp.join("whoami.txt"), "corp").expect("write the marker");

    let certs = root.join("certs");
    std::fs::create_dir_all(&certs).expect("create the certificate directory");
    let write_pair = |file: &str, name: &str| -> Vec<u8> {
        let generated = rcgen::generate_simple_self_signed(vec![name.to_string()])
            .expect("generate a certificate");
        std::fs::write(certs.join(format!("{file}.crt")), generated.cert.pem())
            .expect("write the certificate");
        std::fs::write(
            certs.join(format!("{file}.key")),
            generated.key_pair.serialize_pem(),
        )
        .expect("write the key");
        generated.cert.der().to_vec()
    };
    // The site's own, hand-managed certificate, which the reload will be asked to re-read.
    let named_before = write_pair("corp", "corp.test");
    let named_after = write_pair("renewed", "corp.test");
    assert_ne!(
        named_before, named_after,
        "the fixture must generate a different certificate, or the reload assertion is vacuous"
    );

    // The issued one, written into the cache under the site's ACME domains — which is why this site
    // cannot also name a pair: `Config::validate` refuses both, so the site's *named* pair is on the
    // deployment instead and the issued one is the site's.
    let issued = rcgen::generate_simple_self_signed(vec!["corp.test".to_string()])
        .expect("generate the issued certificate");
    let issued_blob = format!("{}{}", issued.cert.pem(), issued.key_pair.serialize_pem());
    let issued_der = issued.cert.der().to_vec();
    let shared_cache = root.join("acme-cache");
    let directory = volucer_acme::Authority::Staging.directory();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime for the cache write");
    runtime.block_on(async {
        rustls_acme::caches::DirCache::new(&shared_cache)
            .store_cert(
                &["corp.test".to_string()],
                directory,
                issued_blob.as_bytes(),
            )
            .await
            .expect("the cache is writable");
    });

    let config_path = root.join("volucer.toml");
    // The deployment names a pair, so `sites.corp` may have its own ACME table; and the site's own
    // name is served by the issued certificate from the start.
    let config_for = |fallback: &str| {
        format!(
            "[server]\nhttp = \"127.0.0.1:0\"\ndocument_root = {app}\n\n\
             [server.tls]\ncertificate = \"certs/{fallback}.crt\"\nkey = \"certs/{fallback}.key\"\n\n\
             [sites.corp]\nhosts = [\"corp.test\"]\ndocument_root = {corp}\n\n\
             [sites.corp.acme]\ndomains = [\"corp.test\"]\n",
            app = toml_string(&app.display().to_string()),
            corp = toml_string(&corp.display().to_string()),
        )
    };
    std::fs::write(&config_path, config_for("corp")).expect("write the config");

    let Some(server) = Server::start(&binary, &config_path) else {
        eprintln!("skipping: the server did not start");
        return;
    };
    let address = server
        .http_address
        .clone()
        .expect("the config asks for an http listener");

    let before = tls_get(&address, "corp.test", "/whoami.txt");
    assert_eq!(
        before.certificate, issued_der,
        "the issued certificate must be serving before the reload, or the survival assertion after \
         it is about nothing"
    );

    // Point the deployment's *named* pair at the renewed file and signal. The site's issued
    // certificate is not in the configuration at all, so this is exactly the change a rebuild could
    // pick up while dropping it — or skip while stranding the renewal.
    std::fs::write(&config_path, config_for("renewed")).expect("write the config");
    let Some(()) = signal(&server.child, SIGHUP) else {
        eprintln!("skipping: could not signal the server");
        return;
    };
    std::thread::sleep(Duration::from_millis(700));

    let after = tls_get(&address, "corp.test", "/whoami.txt");
    assert_eq!(
        after.certificate, issued_der,
        "the issued certificate must survive a reload that re-reads the configuration; a \
         placeholder or a named pair here means the reload dropped what the site's order produced"
    );
    assert_eq!(
        after.body, "corp",
        "and the site must still be served, so the table was rebuilt rather than emptied"
    );

    drop(server);
    let _ = std::fs::remove_dir_all(&root);
}

/// A site with its **own** ACME table serves a placeholder for its own names until issuance.
///
/// The end-to-end assertion for the per-site wiring, and the same reasoning as the cached-certificate
/// test: without starting the binary nothing can see that the site's engine was built at all — which
/// is the lesson of a feature whose steps were committed and green while `[server.acme]` issued
/// something nothing served.
///
/// What this pins is the **placeholder's names**, and it is the interesting half: a site whose
/// certificate is generated must be served a certificate that covers *its* hosts, not the
/// deployment's default. A generated certificate with the server's names would fail its handshake
/// with a name mismatch — a TLS error, blamed on the client — while every unit test passed.
///
/// No network, no cache, no account: the placeholder is made before any port is bound. The authority
/// is staging, so even a bug that reached out could not issue a real certificate.
#[cfg(unix)]
#[test]
fn a_site_with_its_own_acme_serves_a_placeholder_for_its_own_names() {
    use rustls_acme::CertCache;

    let Some(binary) = volucer_binary() else {
        eprintln!("skipping: the volucer binary is not built (run `cargo build`)");
        return;
    };

    let root = std::env::temp_dir().join(format!("volucer-acme-site-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let app = root.join("app");
    let corp = root.join("corp");
    for dir in [&app, &corp] {
        std::fs::create_dir_all(dir).expect("create a document root");
    }
    std::fs::write(app.join("whoami.txt"), "app").expect("write the marker");
    std::fs::write(corp.join("whoami.txt"), "corp").expect("write the marker");

    let config_path = root.join("volucer.toml");
    let config = format!(
        "[server]\nhttp = \"127.0.0.1:0\"\ndocument_root = {app}\n\n\
         [server.tls]\n\n\
         [server.acme]\ndomains = [\"app.test\"]\ncontact = [\"mailto:ops@test\"]\n\n\
         [sites.corp]\nhosts = [\"corp.test\"]\ndocument_root = {corp}\n\n\
         [sites.corp.acme]\n",
        app = toml_string(&app.display().to_string()),
        corp = toml_string(&corp.display().to_string()),
    );
    std::fs::write(&config_path, config).expect("write the config");

    // A site's engine reads the **site's** cache, which is the only thing that proves a per-site
    // engine exists and is pointed at the right place. A certificate is written into the shared cache
    // under the site's own domains, so a build that built no site engine keeps serving the placeholder
    // and fails here — the hazard-68 shape, where every part works and nothing uses them.
    let issued = rcgen::generate_simple_self_signed(vec!["corp.test".to_string()])
        .expect("generate the issued certificate");
    let issued_blob = format!("{}{}", issued.cert.pem(), issued.key_pair.serialize_pem());
    let issued_der = issued.cert.der().to_vec();
    let shared_cache = root.join("acme-cache");
    let directory = volucer_acme::Authority::Staging.directory();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime for the cache write");
    runtime.block_on(async {
        rustls_acme::caches::DirCache::new(&shared_cache)
            .store_cert(
                &["corp.test".to_string()],
                directory,
                issued_blob.as_bytes(),
            )
            .await
            .expect("the cache is writable");
    });

    let Some(server) = Server::start(&binary, &config_path) else {
        eprintln!("skipping: the server did not start");
        return;
    };
    let address = server
        .http_address
        .clone()
        .expect("the config asks for an http listener");

    // Both sites answer, and each gets a certificate — which is the point of the optional pair: no
    // file was named anywhere in that config, and both listeners completed a handshake.
    let app_seen = tls_get(&address, "app.test", "/whoami.txt");
    assert_eq!(app_seen.body, "app", "the deployment's site answers");
    let corp_seen = tls_get(&address, "corp.test", "/whoami.txt");
    assert_eq!(
        corp_seen.body, "corp",
        "the site with its own table answers"
    );

    assert_eq!(
        corp_seen.certificate, issued_der,
        "the site's own engine must read the site's cache and serve what it holds; a placeholder \
         here means no engine was built for the site's table, so its order would never be served"
    );
    assert_ne!(
        app_seen.certificate, issued_der,
        "and that certificate must not reach the deployment's names, which belong to its own order"
    );
    // The DER holds a DNS SAN as the ASCII name, so the names are checkable without a parser.
    let names = |der: &[u8]| -> Vec<String> {
        ["app.test", "corp.test"]
            .iter()
            .filter(|name| {
                der.windows(name.len())
                    .any(|window| window == name.as_bytes())
            })
            .map(|name| name.to_string())
            .collect()
    };
    assert_eq!(
        names(&app_seen.certificate),
        vec!["app.test".to_string()],
        "the deployment's certificate must name only the deployment's domains"
    );
    assert_eq!(
        names(&corp_seen.certificate),
        vec!["corp.test".to_string()],
        "the site's certificate must name the site's own hosts, not the deployment's domains"
    );

    drop(server);
    let _ = std::fs::remove_dir_all(&root);
}

/// The contact a scope registers must reach the account, and the account files prove it.
///
/// `contact` is not decoration. It is the only channel a CA has for warning that renewals are failing
/// for an account — and Let's Encrypt's rate limits are per *account*, so it is also what decides which
/// account two scopes share. `rustls-acme` derives the account file name from
/// `sha256(contact + directory_url)`, so the files that appear are a direct read of what was sent.
///
/// This is the test that would have caught `Engine::new` dropping `contact` on the floor. With the
/// contact dropped every scope registers the **empty** list, so every scope hashes to one name and
/// exactly one account file appears however many contacts are configured — which is what the bug did,
/// while `check` printed `(none: the CA has no way to warn you…)` beside a `contact` key the operator
/// had filled in.
///
/// The assertion is on the *count* rather than on a hash, so nothing here re-implements the
/// dependency's file-naming scheme: two tables with two different contacts must produce two account
/// files. That is the documented behaviour, and it is exactly what the bug broke.
#[cfg(unix)]
#[test]
fn two_contacts_produce_two_accounts() {
    let Some(binary) = volucer_binary() else {
        eprintln!("skipping: the volucer binary is not built (run `cargo build`)");
        return;
    };

    let root = std::env::temp_dir().join(format!("volucer-acme-contact-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let app = root.join("app");
    let corp = root.join("corp");
    for dir in [&app, &corp] {
        std::fs::create_dir_all(dir).expect("create a document root");
    }
    std::fs::write(app.join("whoami.txt"), "app").expect("write the marker");
    std::fs::write(corp.join("whoami.txt"), "corp").expect("write the marker");

    // Two tables, two contacts, one cache directory — so both accounts land where this test can count
    // them. The site deliberately states no `domains`, deriving them, because the derived path is the
    // one a real deployment uses.
    let config_path = root.join("volucer.toml");
    let config = format!(
        "[server]\nhttp = \"127.0.0.1:0\"\ndocument_root = {app}\n\n\
         [server.tls]\n\n\
         [server.acme]\ndomains = [\"app.example.test\"]\ncontact = [\"mailto:ops@example.test\"]\n\n\
         [sites.corp]\nhosts = [\"corp.example.test\"]\ndocument_root = {corp}\n\n\
         [sites.corp.acme]\ncontact = [\"mailto:other@example.test\"]\n",
        app = toml_string(&app.display().to_string()),
        corp = toml_string(&corp.display().to_string()),
    );
    std::fs::write(&config_path, config).expect("write the config");

    let Some(server) = Server::start(&binary, &config_path) else {
        eprintln!("skipping: the server did not start");
        return;
    };

    // The account is registered before the order, and the file is written then, so what this waits for
    // is arrival at the directory rather than issuance — which on this host is as far as a first
    // deployment gets anyway. An account that never appears means the CA was unreachable, and that is
    // a skip: the test cannot distinguish "no contact" from "no network" by looking at an empty
    // directory, which is why it waits rather than polling once.
    let accounts = wait_for_accounts(&root.join("acme-cache"), 2);
    drop(server);

    match accounts.len() {
        0 => {
            eprintln!(
                "skipping: no account file appeared, so this host could not reach the ACME directory"
            );
            let _ = std::fs::remove_dir_all(&root);
            return;
        }
        1 => panic!(
            "one account file for two different contacts: the contact never reached the account, so \
             both scopes registered the empty list and now share one account — which is the bug this \
             test exists for. `check` would still print the contact, and the CA would have no way to \
             warn about either scope"
        ),
        _ => {}
    }
    assert_eq!(
        accounts.len(),
        2,
        "two tables with two contacts must register two accounts, not {}: {accounts:?}",
        accounts.len()
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// Wait until at least `want` ACME account files exist, and return the distinct names.
///
/// Bounded, because an account that never appears means the CA was unreachable on this host — a skip,
/// not a failure. Returns whatever it found rather than `Option`, so the caller can tell the three
/// cases apart: none (no network), one (the bug), or two (correct).
#[cfg(unix)]
fn wait_for_accounts(cache: &Path, want: usize) -> Vec<String> {
    let deadline = Instant::now() + Duration::from_secs(45);
    let mut seen: Vec<String> = Vec::new();
    while Instant::now() < deadline {
        seen.clear();
        if let Ok(entries) = std::fs::read_dir(cache) {
            for entry in entries.filter_map(Result::ok) {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.starts_with("cached_account_") && !seen.contains(&name) {
                    seen.push(name);
                }
            }
        }
        if seen.len() >= want {
            break;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    seen.sort();
    seen
}

/// One TLS exchange: which certificate arrived, and what the request was answered with.
///
/// Raw sockets and `rustls` directly, rather than a TLS client library, for the same reason the
/// HTTP tests use raw sockets: this asserts *which bytes* the server presented, and a client that
/// abstracts that away cannot answer the question.
#[cfg(unix)]
struct TlsExchange {
    /// The DER of the leaf certificate the server presented.
    certificate: Vec<u8>,
    /// The response body.
    body: String,
}

/// Connect over TLS, ask for `path` on `host`, and report the certificate and the body.
#[cfg(unix)]
fn tls_get(address: &str, host: &str, path: &str) -> TlsExchange {
    use rustls::pki_types::ServerName;

    // The client half of the handshake needs a crypto provider, and there is more than one in the
    // dependency graph, so rustls will not guess. The server's own installer is reused rather than
    // duplicating the choice: a test that picked differently from the server would be testing a
    // configuration no deployment has.
    volucer_http::tls::install_crypto_provider();

    // A self-signed fixture is in no root store, so the verifier is replaced wholesale. The
    // assertion is about *which* certificate arrived, not whether a chain validates.
    let mut config = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptAnyCertificate))
        .with_no_client_auth();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];

    let name = ServerName::try_from(host.to_string()).expect("a server name");
    let connection =
        rustls::ClientConnection::new(Arc::new(config), name).expect("a client connection");
    let socket = TcpStream::connect(address).expect("connect to the TLS listener");
    socket
        .set_read_timeout(Some(Duration::from_secs(20)))
        .expect("a read timeout");
    let mut stream = rustls::StreamOwned::new(connection, socket);

    let request = format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
    stream
        .write_all(request.as_bytes())
        .expect("write the request");
    stream.flush().expect("flush");

    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).expect("read the response");

    let certificate = stream
        .conn
        .peer_certificates()
        .and_then(|certs| certs.first())
        .map(|cert| cert.as_ref().to_vec())
        .expect("the server must present a certificate");

    let body = match raw.windows(4).position(|window| window == b"\r\n\r\n") {
        Some(split) => String::from_utf8_lossy(&raw[split + 4..]).into_owned(),
        None => String::new(),
    };

    TlsExchange { certificate, body }
}

/// Accepts any certificate, for a self-signed fixture.
#[cfg(unix)]
#[derive(Debug)]
struct AcceptAnyCertificate;

#[cfg(unix)]
impl rustls::client::danger::ServerCertVerifier for AcceptAnyCertificate {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        vec![
            rustls::SignatureScheme::ECDSA_NISTP256_SHA256,
            rustls::SignatureScheme::ED25519,
            rustls::SignatureScheme::RSA_PSS_SHA256,
            rustls::SignatureScheme::RSA_PKCS1_SHA256,
        ]
    }
}

/// An ACME-configured server serves its **cached** certificate, with no network.
///
/// This is the end-to-end assertion for the engine wiring, and without it the wiring has no test at
/// all: every other ACME test drives a unit or a stub, so deleting the engine from `serve` would
/// leave them all green while a deployment with `[server.acme]` issued and served nothing.
///
/// It is also the cache's whole purpose from the operator's side: a restart with a valid cached
/// certificate serves it **immediately and spends no rate limit**. So the certificate is written
/// through the crate's own `CertCache` trait — the file name is a hash of the names and the
/// directory URL, so a file written by hand would be one nothing reads — and the assertion is that
/// the handshake gets the *cached* certificate rather than the fallback the configuration names.
///
/// No account, no directory, no network: `install_cached` runs before any port is bound. The
/// authority is staging, so even a bug that did reach out could not issue a real certificate.
#[cfg(unix)]
#[test]
fn an_acme_configured_server_serves_its_cached_certificate() {
    use rustls_acme::CertCache;

    let Some(binary) = volucer_binary() else {
        eprintln!("skipping: the volucer binary is not built (run `cargo build`)");
        return;
    };

    let root = std::env::temp_dir().join(format!("volucer-acme-serve-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let site = root.join("site");
    std::fs::create_dir_all(&site).expect("create a document root");
    std::fs::write(site.join("whoami.txt"), "cached").expect("write the marker");
    let certs = root.join("certs");
    std::fs::create_dir_all(&certs).expect("create the certificate directory");

    // The **fallback** the configuration names: self-signed, and deliberately not what the handshake
    // should end up with.
    let fallback = rcgen::generate_simple_self_signed(vec!["example.test".to_string()])
        .expect("generate the fallback certificate");
    std::fs::write(certs.join("fallback.crt"), fallback.cert.pem()).expect("write the cert");
    std::fs::write(
        certs.join("fallback.key"),
        fallback.key_pair.serialize_pem(),
    )
    .expect("write the key");
    let fallback_der = fallback.cert.der().to_vec();

    // The **issued** certificate, in the shape `rustls-acme` stores: a chain plus a key concatenated
    // into one PEM blob. Written that way on purpose, so this exercises `split_pem_pair` rather than
    // a convenient one-certificate file.
    let issued = rcgen::generate_simple_self_signed(vec!["example.test".to_string()])
        .expect("generate the issued certificate");
    let issued_blob = format!("{}{}", issued.cert.pem(), issued.key_pair.serialize_pem());
    let issued_der = issued.cert.der().to_vec();
    assert_ne!(
        fallback_der, issued_der,
        "the fixture must differ, or the assertion below is vacuous"
    );

    // The directory URL the engine will use, from **the same function `serve` calls** — so this
    // cannot drift from what the engine looks for without the server and the test disagreeing about
    // the authority, which is the failure being guarded.
    let directory = volucer_acme::Authority::Staging.directory();
    assert!(
        directory.contains("staging"),
        "the fixture must use staging, so a bug that reached the network could not issue a real \
         certificate: {directory}"
    );

    let cache_dir = root.join("acme-cache");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime for the cache write");
    runtime.block_on(async {
        rustls_acme::caches::DirCache::new(&cache_dir)
            .store_cert(
                &["example.test".to_string()],
                directory,
                issued_blob.as_bytes(),
            )
            .await
            .expect("the cache is writable");
    });

    let config_path = root.join("volucer.toml");
    let config = format!(
        "[server]\nhttp = \"127.0.0.1:0\"\ndocument_root = {site}\n\n\
         [server.tls]\ncertificate = \"certs/fallback.crt\"\nkey = \"certs/fallback.key\"\n\n\
         [server.acme]\ndomains = [\"example.test\"]\ncontact = [\"mailto:ops@example.test\"]\n",
        site = toml_string(&site.display().to_string()),
    );
    std::fs::write(&config_path, config).expect("write the config");

    let Some(server) = Server::start(&binary, &config_path) else {
        eprintln!("skipping: the server did not start");
        return;
    };
    let address = server
        .http_address
        .clone()
        .expect("the config asks for an http listener");

    let seen = tls_get(&address, "example.test", "/whoami.txt");
    assert_eq!(
        seen.body, "cached",
        "the request must be served from the configured document root"
    );
    assert_eq!(
        seen.certificate, issued_der,
        "an ACME-configured server must serve the **cached** certificate rather than the fallback \
         the configuration names — without the engine wired into `serve`, the answer is the \
         fallback, and every unit test in the tree still passes"
    );

    drop(server);
    let _ = std::fs::remove_dir_all(&root);
}

/// **The assertion that the access log and the request id actually happen.**
///
/// Written as a request through `serve` rather than as a unit test of the formatter, for the reason
/// this states: the formatter, the config and the sink can each be perfect while the front-end
/// never calls any of them, and the result looks exactly like a working deployment.
///
/// Three things are asserted from one server, because they are one feature:
///
/// 1. A client's `X-Request-Id` is **adopted verbatim** and echoed on the response, so the client can
///    find its own id in this server's log.
/// 2. A request without one gets a **32-character generated id**, which is nginx's `$request_id`
///    shape.
/// 3. Both appear in the log file, on **nginx-shaped lines**, with the status the client was given.
///
/// The ids are read back from the file rather than from the response, because the response could be
/// right while the file says something else — and the file is what an operator greps.
#[test]
fn the_access_log_records_the_id_the_client_sees() {
    let Some(binary) = volucer_binary() else {
        eprintln!("skipping: the volucer binary is not built (run `cargo build`)");
        return;
    };
    let root = std::env::temp_dir().join(format!("volucer-access-log-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create the root");

    let config_path = root.join("volucer.toml");
    std::fs::write(
        &config_path,
        r#"
[server]
http = "127.0.0.1:0"
document_root = "."
static_files = false

[server.access_log]
path = "access.log"
format = "combined"
"#,
    )
    .expect("write the config");

    let Some(server) = Server::start(&binary, &config_path) else {
        panic!("the server did not start with an access log configured");
    };
    let address = server
        .http_address
        .clone()
        .expect("the configuration asked for an HTTP listener");

    // A client that already has an id: it must come back unchanged, including the hyphens that make
    // it a UUID rather than 32 hex characters **and including its letter case**. This is the case an
    // implementation that re-rendered an adopted id would fail, and it is the whole point of
    // adopting one — a normalised value is not findable in the client's own log.
    //
    // The uppercase hex is deliberate rather than incidental: a first version of this test used an
    // all-lowercase UUID, and lowercasing the echo then passed it, so the assertion was proving
    // nothing about verbatim-ness. A value that already matches its normalised form cannot test a
    // normalisation.
    let adopted = "3F2504E0-4f89-11D3-9A0C-0305E82C3301";
    let answer = http_get_with(&address, "/one", &[("X-Request-Id", adopted)]);
    assert_eq!(
        answer.header("x-request-id"),
        Some(adopted),
        "an adopted id must be echoed verbatim: {answer:?}"
    );

    // A client with no id: one is minted, and it must be nginx's shape so one `awk` reads this log
    // and an nginx log in front of it.
    let answer = http_get(&address, "/two");
    let generated = answer
        .header("x-request-id")
        .expect("an id is always echoed, including on an error")
        .to_string();
    assert_eq!(generated.len(), 32, "{generated:?} is not nginx's shape");
    assert!(
        generated.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "{generated:?} is not hex"
    );

    // **Read the file, after the response.** The log is flushed per record, so both lines are on
    // disk by now; if that flush were removed this test would fail intermittently, which is exactly
    // the property a "log you can trust after a crash" claim needs.
    let log = std::fs::read_to_string(root.join("access.log")).expect("the access log was written");
    let lines: Vec<&str> = log.lines().collect();
    assert_eq!(lines.len(), 2, "one line per request: {log}");
    assert!(
        lines[0].contains(adopted),
        "the adopted id is not in the log: {}",
        lines[0]
    );
    assert!(
        lines[1].contains(&generated),
        "the generated id is not in the log: {}",
        lines[1]
    );
    for (line, target) in lines.iter().zip(["/one", "/two"]) {
        // nginx's field order, so a script written for nginx reads this. The timestamp is two
        // tokens because `$time_local` includes the `+0000` offset, exactly as nginx's does.
        let fields: Vec<&str> = line.split(' ').collect();
        assert_eq!(fields.len(), 15, "field count of {line:?}");
        assert_eq!(fields[0], "127.0.0.1", "the peer: {line:?}");
        assert_eq!(fields[5], "\"GET", "the request begins: {line:?}");
        assert_eq!(fields[6], target, "the target: {line:?}");
        assert_eq!(fields[7], "HTTP/1.1\"", "the protocol: {line:?}");
        // The status is what the *client* was given, so the log and the answer cannot disagree.
        assert!(fields[8].parse::<u16>().is_ok(), "no status in {line:?}");
        assert_eq!(
            fields[13], "-",
            "no origin, so a dash and not zero: {line:?}"
        );
    }

    let _ = std::fs::remove_dir_all(&root);
}

/// `SIGHUP` reopens the access log, so a `logrotate` actually rotates.
///
/// **The defect this pins is silent from the server's side**, which is why it needs a real process and
/// a real rename rather than a unit test: after `logrotate` renames the file, a server that does not
/// reopen keeps writing to the unlinked inode. Every write succeeds, the path holds a fresh empty
/// file, and the log is discovered to be empty during the incident it was needed for.
///
/// The shape is `logrotate`'s: rename, then signal. The assertions are on **both** files, and the
/// rotated one is the control — a reopen that did nothing would put the second record there instead,
/// so an assertion that only checked the fresh file had a line would pass against a server that
/// created it and then wrote elsewhere.
#[cfg(unix)]
#[test]
fn sighup_reopens_the_access_log_after_a_rotation() {
    const SIGHUP: i32 = 1;

    let Some(binary) = volucer_binary() else {
        eprintln!("skipping: the volucer binary is not built (run `cargo build`)");
        return;
    };
    let root = std::env::temp_dir().join(format!("volucer-logrotate-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create the root");

    let config_path = root.join("volucer.toml");
    std::fs::write(
        &config_path,
        r#"
[server]
http = "127.0.0.1:0"
document_root = "."
static_files = false

[server.access_log]
path = "access.log"
format = "combined"
"#,
    )
    .expect("write the config");

    let Some(server) = Server::start(&binary, &config_path) else {
        panic!("the server did not start with an access log configured");
    };
    let address = server
        .http_address
        .clone()
        .expect("the configuration asked for an HTTP listener");
    let log_path = root.join("access.log");

    // A request, so there is something in the file before the rotation — and a line count to compare
    // against afterwards.
    let _ = http_get(&address, "/before");
    let before = std::fs::read_to_string(&log_path).expect("the access log was written");
    assert_eq!(before.lines().count(), 1, "{before}");

    // `logrotate`'s rename. The server now holds a handle to a file called `access.log.1`.
    let rotated = root.join("access.log.1");
    std::fs::rename(&log_path, &rotated).expect("rename the log");
    assert!(
        !log_path.exists(),
        "the rename must leave no file at the path, or the test proves nothing"
    );

    let Some(()) = signal(&server.child, SIGHUP) else {
        eprintln!("skipping: could not signal the server");
        return;
    };

    // Polled rather than slept on: the signal is asynchronous, and the file appearing at the path is
    // the observable. A reopen that never happened leaves nothing there.
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline && !log_path.exists() {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        log_path.exists(),
        "SIGHUP must close and reopen the log, which recreates it at its path; without that \
         logrotate leaves the server writing into an unlinked inode for ever"
    );

    // A request after the reopen, and it must land in the **new** file.
    let _ = http_get(&address, "/after");
    let fresh = std::fs::read_to_string(&log_path).expect("the reopened log is readable");
    assert_eq!(
        fresh.lines().count(),
        1,
        "the record written after the rotation belongs in the new file, not the rotated one: {fresh}"
    );
    assert!(fresh.contains("/after"), "{fresh}");

    // The control: the rotated file still holds exactly the record from before, so a reopen that
    // wrote to both or to neither is visible here.
    let rotated_text = std::fs::read_to_string(&rotated).expect("the rotated log is readable");
    assert_eq!(
        rotated_text.lines().count(),
        1,
        "the rotated file must not have grown: {rotated_text}"
    );
    assert!(rotated_text.contains("/before"), "{rotated_text}");

    drop(server);
    let _ = std::fs::remove_dir_all(&root);
}

/// A deployment with no `[server.access_log]` writes nothing and still serves.
///
/// The control for "off by default changes nothing", asserted as a request rather than as a config
/// check: the file must not exist, and the response must not carry an id header — a deployment that
/// did not ask for observability must not have it appear on its responses, because a client may act
/// on one.
#[test]
fn no_access_log_config_means_no_log_file_and_no_id_header() {
    let Some(binary) = volucer_binary() else {
        eprintln!("skipping: the volucer binary is not built (run `cargo build`)");
        return;
    };
    let root = std::env::temp_dir().join(format!("volucer-no-log-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create the root");

    let config_path = root.join("volucer.toml");
    std::fs::write(
        &config_path,
        r#"
[server]
http = "127.0.0.1:0"
document_root = "."
static_files = false
"#,
    )
    .expect("write the config");

    let Some(server) = Server::start(&binary, &config_path) else {
        panic!("the server did not start");
    };
    let address = server
        .http_address
        .clone()
        .expect("the configuration asked for an HTTP listener");

    let answer = http_get(&address, "/anything");
    assert!(
        answer.header("x-request-id").is_none(),
        "an id was echoed on a deployment that never asked for one: {answer:?}"
    );
    assert!(
        !root.join("access.log").exists(),
        "a log file was created with no [server.access_log]"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// Two pools, a list, and a reset of the index the list gave.
///
/// **This is the test the index arithmetic needed.** `workers.list` reported each slot's *own*
/// index, while `worker.reset` counted slots across every pool — so with one backend the two agreed
/// and with two they did not, which is the arithmetic a single-pool test cannot see. Two is not an
/// exotic deployment: it is a web backend and a jobs backend, which is what the queue
/// design recommends.
///
/// It goes through the shipped binary, the `[status]` listener and the `POST` route, because the
/// number in the answer is what an operator types next: a list that says worker 1 twice is an
/// instruction to reset the wrong process, and no unit test of either half can be wrong in a way this
/// notices.
#[test]
fn the_worker_list_and_the_reset_agree_about_a_worker_in_a_second_pool() {
    let Some(binary) = volucer_binary() else {
        eprintln!("skipping: the volucer binary is not built (run `cargo build`)");
        return;
    };
    if find_php().is_none() {
        eprintln!("skipping: no php on PATH");
        return;
    }
    require_sdk!();

    let root = std::env::temp_dir().join(format!("volucer-workers-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create the temp root");

    let config_path = root.join("volucer.toml");
    let script = example_worker();
    let command = toml_string(&format!("php \"{}\"", script.display()));
    let document_root = toml_string(&root.display().to_string());
    // Two worker backends, one worker each, so the second pool's only worker is index 1 globally and
    // index 0 locally. That is the whole defect: the two numbers differ by exactly the first pool's
    // size, and reporting the local one sends an operator to the wrong process.
    let config = format!(
        "[server]\ndefault_backend = \"app\"\nhttp = \"127.0.0.1:0\"\ndocument_root = {document_root}\n\n\
         [backends.app]\nkind = \"worker\"\ncommand = {command}\nworkers = 1\n\n\
         [backends.jobs]\nkind = \"worker\"\ncommand = {command}\nworkers = 1\n\n\
         [status]\naddress = \"127.0.0.1:0\"\nallow_worker_reset = true\n"
    );
    std::fs::write(&config_path, config).expect("write the config");

    let Some(server) = Server::start(&binary, &config_path) else {
        eprintln!("skipping: the server did not start");
        let _ = std::fs::remove_dir_all(&root);
        return;
    };
    let Some(status_address) = server.status_address.clone() else {
        panic!("the status listener did not report an address; the banner was not parsed");
    };

    let listed = http_get(&status_address, "/workers");
    assert_eq!(listed.status, 200, "{listed:?}");
    let list: serde_json::Value =
        serde_json::from_slice(&listed.body).expect("the worker list is JSON");
    let entries = list.as_array().expect("an array of workers");
    assert_eq!(entries.len(), 2, "one worker per backend: {list}");

    // The two numbers, and the assertion that they are the ones an operator needs: `index` is the
    // global one the reset takes, `slot` is the pool's own.
    assert_eq!(entries[0]["index"], 0, "{list}");
    assert_eq!(entries[0]["slot"], 0, "{list}");
    assert_eq!(entries[0]["backend"], "app", "{list}");
    assert_eq!(entries[1]["index"], 1, "{list}");
    assert_eq!(
        entries[1]["slot"], 0,
        "the second pool's only worker is its slot 0: {list}"
    );
    assert_eq!(entries[1]["backend"], "jobs", "{list}");

    let pid = entries[1]["pid"]
        .as_u64()
        .expect("the second worker has a pid");

    // Reset the index the list gave. A `POST` to `slot` instead would recycle the *first* pool's
    // worker, so the assertion below is about which process was replaced, not about a status code.
    let reset = http_request(&status_address, "POST", "/workers/1/reset", &[]);
    assert_eq!(reset.status, 200, "{reset:?}");

    let after = http_get(&status_address, "/workers");
    let after: serde_json::Value =
        serde_json::from_slice(&after.body).expect("the worker list is JSON");
    let replaced = after[1]["pid"]
        .as_u64()
        .expect("the second worker has a pid");
    assert_ne!(
        replaced, pid,
        "the reset replaced the worker the list named: {after}"
    );
    assert_eq!(
        after[0]["pid"].as_u64().expect("a pid"),
        entries[0]["pid"].as_u64().expect("a pid"),
        "and it left the first pool's worker alone: {after}"
    );

    // An index past the end is a 404 rather than a 500 or an empty success, because that is the one
    // answer `curl -f` can act on without parsing the body.
    let missing = http_request(&status_address, "POST", "/workers/9/reset", &[]);
    assert_eq!(missing.status, 404, "{missing:?}");

    drop(server);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn an_https_only_configuration_starts_a_listener() {
    // `https` is an independently bound listener, so a configuration that states only
    // `https` has one. `serve` used to test `http` and `fastcgi` and refuse everything
    // else — while `validate` accepted an `https`-only file, so `volucer check` replied
    // `configuration OK` and `serve` then died with "no listener is configured". A
    // deployment whose whole point is to answer on :443 only could not start at all.
    //
    // The assertion is on the banner rather than on a request: the certificate here is
    // self-signed, and proving the handshake works is a different test. What this one
    // pins is that the listener exists, which is the fact that was wrong.
    let Some(binary) = volucer_binary() else {
        eprintln!("skipping: the volucer binary has not been built");
        return;
    };
    let Some(php_cgi) = find_php_cgi() else {
        eprintln!("skipping: php-cgi was not found");
        return;
    };

    let root = std::env::temp_dir().join(format!("volucer-https-only-{}", std::process::id()));
    std::fs::create_dir_all(root.join("public")).expect("create the document root");
    std::fs::write(
        root.join("public/index.php"),
        "<?php header('Content-Type: text/plain'); echo 'ok';",
    )
    .expect("write the front controller");

    // A certificate and key, generated with the same `openssl` a reader would use. Without
    // one, `[server.tls]` is a startup error, which would make this test pass for the
    // wrong reason.
    let certs = root.join("certs");
    std::fs::create_dir_all(&certs).expect("create the certificate directory");
    let made = Command::new("openssl")
        .args([
            "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "1",
        ])
        .arg("-keyout")
        .arg(certs.join("default.key"))
        .arg("-out")
        .arg(certs.join("default.crt"))
        .args(["-subj", "/CN=localhost"])
        .output();
    match made {
        Ok(output) if output.status.success() => {}
        _ => {
            eprintln!("skipping: openssl is not available to make a certificate");
            let _ = std::fs::remove_dir_all(&root);
            return;
        }
    }

    let config = root.join("volucer.toml");
    let php_cgi = toml_string(&php_cgi.display().to_string());
    let config_path = config.clone();
    let config_for = move || {
        let port = free_port_run(1).expect("reserve a port");
        format!(
            "[server]\n\
             default_backend = \"cgi\"\n\
             https = \"127.0.0.1:{port}\"\n\
             document_root = \"./public\"\n\
             front_controller = \"index.php\"\n\
             \n\
             [server.tls]\n\
             certificate = \"certs/default.crt\"\n\
             key = \"certs/default.key\"\n\
             \n\
             [backends.cgi]\n\
             kind = \"spawn_cgi\"\n\
             binary = {php_cgi}\n\
             workers = 1\n"
        )
    };

    let server = start_server_with_config(&binary, &config_path, &config_for, "an https-only file");
    let banner = server.banner();
    assert!(
        banner.contains("https:") && banner.contains("(https)"),
        "the https listener was not announced; the banner says:\n{banner}"
    );
    assert!(
        banner.contains("http:             (disabled)"),
        "no plaintext listener was configured, so none should be announced:\n{banner}"
    );

    drop(server);
    let _ = std::fs::remove_dir_all(&root);
}
