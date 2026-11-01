//! Live tests: the real `volucer serve` binary reaching **real websites** over the real internet.
//!
//! # Why this file exists when every other suite is hermetic
//!
//! Everything else in this repository talks to a fixture on `127.0.0.1`. That is the right default —
//! it is fast, it is deterministic, and a failure is always the code's — but four properties cannot
//! be reached that way:
//!
//! 1. **A real certificate chain the client was not told about.** `volucer-upstream` proves the
//!    *negative*: a self-signed origin is refused when no `ca` is named, so a client that installed no
//!    verifier fails that test. Nothing proves a genuine public chain is *accepted*. That test stays
//!    green if `webpki-roots` were empty, built from the wrong bundle, or bypassed — and in each of
//!    those states every `https://` origin in a deployment answers 502 with no test noticing.
//! 2. **ALPN against a server that prefers HTTP/2.** `connect.rs` states the hazard itself: the
//!    connector reads `alpn_protocol()` after the handshake and reports it through
//!    `Connected::negotiated_h2()` — *"without it, ALPN could negotiate `h2` on the wire and the client
//!    would still speak HTTP/1.1 over it — a hang, not an error."* The listener's half is pinned by
//!    `volucer-http`'s `alpn_h2_negotiates_a_real_http_2_connection`. The client's half is pinned
//!    nowhere, and cannot be: a loopback fixture has to be *told* to speak h2, so the test would be
//!    asserting its own fixture.
//! 3. **Real response shapes.** A CDN's compressed chunked body, an origin's own redirect, a status
//!    code no layer here generates. Each is a hand-written fixture today, and a fixture is a sample of
//!    what one person imagined.
//! 4. **A virtual host choosing a backend, end to end.** Site selection is proved against document
//!    roots built in-process, and that a bound site is refused by name is proved on loopback. Neither
//!    runs the shipped binary, so nothing showed that the site the `Host` selected is the one whose
//!    route table reaches a real origin — and writing that test is what turned up a divergence: over
//!    an HTTP/2 upstream hop the origin is given its own authority rather than the client's `Host`.
//!
//! So this file is narrow on purpose: it proves those four things and nothing a loopback test already
//! proves better.
//!
//! # Why it does not run by default
//!
//! Because a skip in this repository means a failure (`VOLUCER_E2E`), and a live test on every pull
//! request would make CI red for a reason that is not the diff: an expired fixture, a CDN having a bad
//! morning, a runner with no egress. `VOLUCER_LIVE=1` opts in and makes a skip fatal, and the `live`
//! job in `.github/workflows/live.yml` sets it on a schedule rather than on a push. That is the same
//! distinction `VOLUCER_E2E` draws one level down: a suite whose prerequisites are *installed* can run
//! everywhere, and a suite whose prerequisites are *external* needs an opt-in.
//!
//! # Why the origins are environment-overridable
//!
//! The dependency here is somebody else's server. Every origin is a default rather than a constant —
//! `VOLUCER_LIVE_<NAME>_ORIGIN` replaces it — because a third party can retire a debug endpoint, change
//! a response or block a runner, and because a release check wants to point at *your* deployment. See
//! [`origin`].

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Leave a test unrun, unless something says a skip is a failure.
///
/// The live counterpart of `serve.rs`'s macro of the same name, and separate from it rather than
/// shared: `tests/` has no `common` module in this crate, and the two gates answer different
/// questions. `VOLUCER_E2E` says "the interpreter and SDK this machine should have are here";
/// `VOLUCER_LIVE` says "this machine may reach the internet". A local run with neither still runs
/// every hermetic suite.
macro_rules! skip_or_fail {
    ($reason:expr) => {{
        let reason = $reason;
        if live_enabled() {
            panic!("VOLUCER_LIVE is set, so a skipped test is a failure: {reason}");
        }
        eprintln!("skipping: {reason}");
        return;
    }};
}

/// Whether the live tests were asked for.
fn live_enabled() -> bool {
    std::env::var("VOLUCER_LIVE")
        .map(|value| !value.is_empty() && value != "0")
        .unwrap_or(false)
}

/// Leave a test unrun unless the suite was opted into.
macro_rules! require_live {
    () => {
        if !live_enabled() {
            skip_or_fail!(
                "the live suite is opt-in: set VOLUCER_LIVE=1 to reach real websites — see this \
                 file's header for why it is not on by default"
            );
        }
    };
}

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

/// Bind the built binary, or leave the test unrun.
macro_rules! require_binary {
    ($name:ident) => {
        let Some($name) = volucer_binary() else {
            skip_or_fail!("the volucer binary has not been built");
        };
    };
}

/// One origin: a default a third party owns, and an override you own.
///
/// Read from `VOLUCER_LIVE_<NAME>_ORIGIN`, upper-cased. The override exists because this suite's
/// dependency is somebody else's server, and because a release check wants to reach an internal
/// deployment rather than a public fixture.
///
/// A present-but-empty override is a **panic**, not a fallback: a run that reports it tested your
/// origin while silently testing the public one is worse than a red one.
fn origin(name: &str, default: &str) -> String {
    let key = format!("VOLUCER_LIVE_{}_ORIGIN", name.to_ascii_uppercase());
    match std::env::var(&key) {
        Ok(value) if !value.trim().is_empty() => value.trim().to_string(),
        Ok(_) => {
            panic!("{key} is set but empty — unset it to use the default, or give it an origin")
        }
        Err(_) => default.to_string(),
    }
}

/// A `volucer serve` process that is killed when the test ends.
struct Server {
    child: Child,
    /// The HTTP listener's address, read from the banner.
    http_address: String,
    /// The fixture directory this server was started from, removed when it is dropped.
    ///
    /// Owned here rather than cleaned by a `remove_dir_all` at the end of each test, because a test
    /// that panics never reaches its last line — and a failing test is exactly when a stale directory
    /// is most confusing, since the next run has to be sure it is reading a fresh configuration.
    root: PathBuf,
    /// What the server wrote to stderr, accumulated as it arrives.
    ///
    /// **A failed live test needs a reason more than a hermetic one does**, because the cause may be
    /// the origin's rather than the code's: a 502 is the server saying "the backend failed", and
    /// *why* is a log line. Shared rather than joined on demand, because the reader thread only
    /// finishes when the pipe closes — and the moment a diagnostic is wanted is before that.
    stderr: Arc<Mutex<String>>,
    /// Everything on stdout after the banner, drained so the process is neither blocked by a full
    /// pipe nor killed by one. The server's shutdown summary uses `println!`, which panics on
    /// `Broken pipe`.
    stdout_tail: Option<std::thread::JoinHandle<()>>,
}

impl Server {
    /// Start the server from a configuration file, waiting for its banner.
    fn start(binary: &Path, config: &Path) -> Option<Self> {
        let mut child = Command::new(binary)
            .args([
                std::ffi::OsStr::new("serve"),
                std::ffi::OsStr::new("--config"),
                config.as_os_str(),
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .ok()?;

        // Started before the banner is read, for the reason `serve.rs` gives: a pipe nobody reads
        // fills and blocks the server on its next log line, which turns a diagnostic into a hang.
        let stderr = Arc::new(Mutex::new(String::new()));
        if let Some(pipe) = child.stderr.take() {
            let buffer = Arc::clone(&stderr);
            std::thread::spawn(move || {
                let mut reader = BufReader::new(pipe);
                let mut line = String::new();
                loop {
                    line.clear();
                    match reader.read_line(&mut line) {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {
                            if let Ok(mut guard) = buffer.lock() {
                                guard.push_str(&line);
                            }
                        }
                    }
                }
            });
        }

        let stdout = child.stdout.take().expect("piped stdout");
        let mut reader = BufReader::new(stdout);
        let mut http_address = String::new();
        let mut line = String::new();
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            line.clear();
            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                break;
            }
            let trimmed = line.trim();
            if let Some(rest) = trimmed.strip_prefix("http:") {
                let address = rest.split_whitespace().next().unwrap_or("");
                if !address.is_empty() && address != "(disabled)" {
                    http_address = address.to_string();
                }
            } else if trimmed.starts_with("press Ctrl-C") {
                // The banner ends here; the listeners are serving from this point.
                break;
            }
        }

        let stdout_tail = Some(std::thread::spawn(move || {
            let mut sink = String::new();
            let _ = reader.read_to_string(&mut sink);
        }));

        if http_address.is_empty() {
            // A server that never printed an address either failed to start or died; returning it
            // would turn a startup failure into a connect timeout.
            let _ = child.kill();
            let _ = child.wait();
            eprintln!("the server reported no HTTP listener:\n{}", lock(&stderr));
            return None;
        }

        Some(Server {
            child,
            http_address,
            root: config.parent().unwrap_or(config).to_path_buf(),
            stderr,
            stdout_tail,
        })
    }

    /// What the server has written to stderr so far.
    fn diagnostics(&self) -> String {
        lock(&self.stderr)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(handle) = self.stdout_tail.take() {
            let _ = handle.join();
        }
        // After the process is reaped: a configuration on Windows is held open for the life of the
        // child, so removing the directory first would leave it behind and the removal would fail
        // silently. Best effort — a test that is already panicking is not the place to report that a
        // temporary directory would not delete.
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Read a shared buffer, tolerating a poisoned lock.
///
/// A panic elsewhere must not turn "read the diagnostics" into a second panic that replaces the
/// first one's message — which is the only thing a failing test has left.
fn lock(buffer: &Arc<Mutex<String>>) -> String {
    match buffer.lock() {
        Ok(guard) => guard.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    }
}

/// Start a server from a configuration built for a fixture root, retrying a few times.
///
/// `http = "127.0.0.1:0"` means the kernel picks the port and the banner reports it, so there is no
/// reservation to race — which is why this retries only a handful of times and reports the server's
/// own stderr rather than a port.
fn start_server(binary: &Path, what: &str, config_for: &dyn Fn(&Path) -> String) -> Server {
    let mut last = String::new();
    for attempt in 0..3 {
        let root = std::env::temp_dir().join(format!(
            "volucer-live-{}-{what}-{attempt}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("create the fixture root");
        let config_path = root.join("volucer.toml");
        std::fs::write(&config_path, config_for(&root)).expect("write the config");
        if let Some(server) = Server::start(binary, &config_path) {
            return server;
        }
        last = root.display().to_string();
    }
    panic!("the server did not start for {what} (last attempt in {last})");
}

/// One HTTP response, parsed strictly enough to be evidence.
struct HttpResponse {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl HttpResponse {
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

/// A `Debug` that does not print a whole page.
///
/// `assert_eq!` formats both sides, and the part a failure needs is the headers. Printing a kilobyte
/// of HTML into it scrolls the useful part away.
struct ResponseDebug<'a>(&'a HttpResponse);

impl std::fmt::Debug for ResponseDebug<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HttpResponse")
            .field("status", &self.0.status)
            .field("headers", &self.0.headers)
            .field("body_len", &self.0.body.len())
            .finish()
    }
}

/// A request through the server, with a body when the method needs one.
///
/// Its own helper rather than `serve.rs`'s, because this file sends a *body* and that helper writes
/// `Content-Length: 0` for every non-GET.
fn http(
    address: &str,
    method: &str,
    path: &str,
    extra: &[(&str, &str)],
    body: &[u8],
) -> HttpResponse {
    let mut stream = TcpStream::connect(address).expect("connect to the HTTP listener");
    stream
        .set_read_timeout(Some(Duration::from_secs(60)))
        .expect("a read timeout");
    // **Deliberately not `Connection: close`.** A `Content-Length` frames this response and reading
    // stops when it has arrived; a chunked one is read until its terminator. Asking for a close would
    // make every answer look length-framed, and the assertion on `transfer-encoding: chunked` below
    // would then be asserting what the *request* asked for rather than what the origin sent.
    // A caller that supplied its own `Host` in `extra` keeps it: sending two would be a request no
    // server is obliged to interpret, and the tests that select a site *have* to name one.
    let mut request = format!("{method} {path} HTTP/1.1\r\n");
    if !extra
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case("host"))
    {
        request.push_str("Host: example.test\r\n");
    }
    for (name, value) in extra {
        request.push_str(name);
        request.push_str(": ");
        request.push_str(value);
        request.push_str("\r\n");
    }
    if !body.is_empty() || method != "GET" {
        request.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    request.push_str("\r\n");
    stream
        .write_all(request.as_bytes())
        .expect("write the request head");
    if !body.is_empty() {
        stream.write_all(body).expect("write the request body");
    }
    stream.flush().expect("flush");

    parse(&read_response(&mut stream))
}

/// Read one response, honouring both framings.
fn read_response(stream: &mut TcpStream) -> Vec<u8> {
    let mut raw = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        if let Some(position) = raw.windows(4).position(|window| window == b"\r\n\r\n") {
            break position + 4;
        }
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => panic!(
                "the connection closed before a response head arrived: {:?}",
                String::from_utf8_lossy(&raw)
            ),
            Ok(n) => raw.extend_from_slice(&chunk[..n]),
        }
    };
    let head = String::from_utf8_lossy(&raw[..head_end]).to_ascii_lowercase();

    let chunked = head
        .lines()
        .any(|line| line.trim() == "transfer-encoding: chunked");
    let content_length = head.lines().find_map(|line| {
        line.strip_prefix("content-length:")
            .and_then(|value| value.trim().parse::<usize>().ok())
    });

    if chunked {
        // Until the zero-length chunk and its terminating CRLF. The check is on the *wire* form,
        // because the decoder has to see the terminator it was written for.
        while !raw[head_end..]
            .windows(5)
            .any(|window| window == b"0\r\n\r\n")
        {
            match stream.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => raw.extend_from_slice(&chunk[..n]),
            }
        }
    } else if let Some(expected) = content_length {
        while raw.len() < head_end + expected {
            match stream.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => raw.extend_from_slice(&chunk[..n]),
            }
        }
    }

    raw
}

/// Split a raw response into status, headers and a **de-chunked** body.
fn parse(raw: &[u8]) -> HttpResponse {
    let split = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .unwrap_or_else(|| panic!("no header terminator in {:?}", String::from_utf8_lossy(raw)));
    let head = String::from_utf8_lossy(&raw[..split]).into_owned();
    let mut lines = head.lines();
    let status = lines
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .unwrap_or_else(|| panic!("no status in {head:?}"));
    let headers: Vec<(String, String)> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_string(), value.trim().to_string()))
        .collect();

    let chunked = headers.iter().any(|(name, value)| {
        name.eq_ignore_ascii_case("transfer-encoding") && value.eq_ignore_ascii_case("chunked")
    });
    let body = &raw[split + 4..];
    let body = if chunked {
        decode_chunked(body)
    } else {
        body.to_vec()
    };

    HttpResponse {
        status,
        headers,
        body,
    }
}

/// Decode a chunked body: `size\r\ndata\r\n` repeated, ended by a zero-size chunk.
///
/// Written out rather than delegated, because the assertion that matters is on the resulting *bytes*:
/// a decoder that tolerated a malformed stream would produce bytes that agree with a lenient
/// expectation. A size that is not hexadecimal panics, which is the honest outcome — the server
/// emitted framing no client could read.
fn decode_chunked(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut rest = raw;
    while let Some(line_end) = rest.windows(2).position(|window| window == b"\r\n") {
        // A chunk extension after `;` is legal and is not part of the size.
        let size_text = String::from_utf8_lossy(&rest[..line_end]);
        let size_text = size_text.split(';').next().unwrap_or("").trim();
        let Ok(size) = usize::from_str_radix(size_text, 16) else {
            panic!("a chunk size that is not hexadecimal: {size_text:?}");
        };
        rest = &rest[line_end + 2..];
        if size == 0 {
            break;
        }
        assert!(
            rest.len() >= size,
            "a chunk claims {size} bytes but only {} arrived",
            rest.len()
        );
        out.extend_from_slice(&rest[..size]);
        rest = &rest[size..];
        if rest.starts_with(b"\r\n") {
            rest = &rest[2..];
        }
    }
    out
}

/// A configuration for one site proxying to an `upstream`, which is all this file exercises.
fn proxy_config(root: &Path, upstream: &str) -> String {
    format!(
        r#"
[server]
http = "127.0.0.1:0"
document_root = {root}
default_backend = "site"
# Off explicitly. `static_files` defaults to *on*, and a static layer answering `/` from the empty
# document root would make a 200 mean two different things — the test would pass on a directory
# listing while the origin was never reached at all.
static_files = false

[backends.site]
kind = "upstream"
origin = {origin}
"#,
        root = toml_string(&root.display().to_string()),
        origin = toml_string(upstream),
    )
}

/// A local HTTP origin that answers every request with a fixed marker.
///
/// The second backend of the vhost test, and it is local **on purpose**. The alternative was a
/// second public echo service, and two were tried and rejected on measurement: `postman-echo.com`
/// answers `409 Conflict` to a foreign `Host` (so "the request arrived" and "the request was
/// refused" cannot be told apart), and `httpbingo.org` answers `402` under the load a test suite
/// generates. A discriminator that depends on a third party's rate limiter is a flaky test wearing a
/// live test's clothes.
///
/// Keeping this one local costs nothing the test was buying: the property under test is that the
/// site the `Host` selected is the site whose route table picks the backend, and one end of that —
/// the real origin — is what needs a real network. The other end only has to be *unmistakably not*
/// the real one.
struct LocalOrigin {
    address: String,
    /// Held so the listener lives as long as the test; dropping it closes the port.
    _listener: std::sync::Arc<std::net::TcpListener>,
}

impl LocalOrigin {
    fn start(marker: &'static str) -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind a local origin");
        let address = listener.local_addr().expect("an address").to_string();
        let listener = std::sync::Arc::new(listener);
        let accepted = std::sync::Arc::clone(&listener);
        std::thread::spawn(move || {
            for stream in accepted.incoming() {
                let Ok(mut stream) = stream else { continue };
                // Read the request head before answering: a server that replies before reading can
                // reset the connection under the client, which reads as a transport error.
                let mut reader = BufReader::new(stream.try_clone().expect("clone the stream"));
                let mut line = String::new();
                while reader.read_line(&mut line).unwrap_or(0) > 0 {
                    if line == "\r\n" {
                        break;
                    }
                    line.clear();
                }
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\
                     Connection: close\r\n\r\n{}",
                    marker.len(),
                    marker
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            }
        });
        LocalOrigin {
            address,
            _listener: listener,
        }
    }

    fn origin_url(&self) -> String {
        format!("http://{}", self.address)
    }
}

/// A configuration with two virtual hosts, each routing to its own origin.
///
/// Routes rather than per-site origins, because a site has no `origin` key: the HTTP path resolves
/// the site from the `Host` and then the *route table of that site* decides the backend. Two routes
/// naming different hosts and different backends is therefore the smallest configuration that makes
/// "which site answered" observable from outside.
fn vhost_config(root: &Path, alpha: &str, beta: &str) -> String {
    format!(
        r#"
[server]
http = "127.0.0.1:0"
document_root = {root}
default_backend = "alpha"
static_files = false

[backends.alpha]
kind = "upstream"
origin = {alpha}

[backends.beta]
kind = "upstream"
origin = {beta}

[sites.alpha]
hosts = ["alpha.live.test"]
document_root = {root}

[[sites.alpha.routes]]
host = "alpha.live.test"
path_prefix = "/"
backend = "alpha"

[sites.beta]
hosts = ["beta.live.test"]
document_root = {root}

[[sites.beta.routes]]
host = "beta.live.test"
path_prefix = "/"
backend = "beta"
"#,
        root = toml_string(&root.display().to_string()),
        alpha = toml_string(alpha),
        beta = toml_string(beta),
    )
}

/// Quote a string for a TOML basic string, escaping the two characters that matter.
///
/// An origin from the environment is a string this code did not write, and a Windows backslash in a
/// TOML basic string is a parse error rather than a path — the same papercut `serve.rs` documents.
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

// ---------------------------------------------------------------------------
// The tests.
// ---------------------------------------------------------------------------

/// **A real public certificate chain is accepted when no `ca` is named.**
///
/// The assertion `volucer-upstream` cannot make. Its own test proves the negative — a self-signed
/// origin is refused, so a client with no verifier fails it — and that test stays green if
/// `webpki-roots` were empty, built from the wrong bundle, or replaced by the platform's roots. All
/// three are states in which every real `https://` origin in a deployment answers 502 and nothing in
/// this repository notices.
///
/// `example.com` is the origin because it is the one host maintained for exactly this: one small
/// page, no interest in blocking a runner.
#[test]
fn a_real_public_certificate_chain_is_accepted() {
    require_live!();
    require_binary!(binary);

    let target = origin("example", "https://example.com");
    let server = start_server(&binary, "public-roots", &|root| proxy_config(root, &target));

    let answer = http(&server.http_address, "GET", "/", &[], b"");
    assert_eq!(
        answer.status,
        200,
        "an origin signed by a public root must be reached with no `ca` configured. A 502 here \
         means the default trust store is wrong, and every https:// origin in a deployment is too. \
         origin: {target}\n{}",
        server.diagnostics()
    );
    assert!(
        answer.text().to_ascii_lowercase().contains("<html"),
        "the origin's page must arrive whole: {:?}",
        ResponseDebug(&answer)
    );
}

/// **ALPN negotiates HTTP/2 to an origin that prefers it, and the client actually speaks it.**
///
/// `connect.rs` names the hazard in its own words: the connector reads `alpn_protocol()` after the
/// handshake and reports it through `Connected::negotiated_h2()`, *"without it, ALPN could negotiate
/// `h2` on the wire and the client would still speak HTTP/1.1 over it — a hang, not an error."* This
/// is the only end-to-end evidence that the value is read and acted on.
///
/// # Why the observer is the origin
///
/// The server publishes nothing about the upstream hop's version: the access log records the version
/// the **client** spoke, and `upstream_version` deliberately rewrites the outbound version to 1.1. So
/// the only honest witness is the origin, and Cloudflare publishes one — `/cdn-cgi/trace` reports both
/// the HTTP version and the TLS version the request arrived with.
///
/// # What makes it a measurement rather than a claim
///
/// Two fields, and the second stops the first from being about the wrong hop. `http=http/2` says the
/// request went over HTTP/2; `tls=TLSv1.x` says that hop was a real TLS session, so the version cannot
/// have come from a plaintext connection. The negative half — that a connector which does *not* claim
/// h2 speaks 1.1 — is pinned by `h2c.rs`'s `without_the_claim_hyper_speaks_http_1_1`. It cannot be
/// observed from outside, which is why the two halves live in different files.
#[test]
fn alpn_negotiates_http_2_to_an_origin_that_prefers_it() {
    require_live!();
    require_binary!(binary);

    let target = origin("h2", "https://www.cloudflare.com");
    let server = start_server(&binary, "alpn-h2", &|root| proxy_config(root, &target));

    let answer = http(&server.http_address, "GET", "/cdn-cgi/trace", &[], b"");
    assert_eq!(
        answer.status,
        200,
        "the trace endpoint must answer through the proxy. **A 502 here is the strongest possible \
         failure signal**: hyper spoke HTTP/1.1 over a connection where ALPN agreed `h2`, which is \
         exactly the `connect.rs` hazard and is fatal rather than cosmetic. origin: {target}\n{}",
        server.diagnostics()
    );

    let document = answer.text();
    let through = field(&document, "http").unwrap_or_else(|| {
        panic!(
            "no `http=` line in the trace answer — the endpoint's shape has changed, which is why \
             VOLUCER_LIVE_H2_ORIGIN exists: {document:?}"
        )
    });
    assert_eq!(
        through, "http/2",
        "the upstream hop must be HTTP/2 when the origin prefers it and the connector offers `h2` \
         first. `http/1.1` here is exactly the failure `connect.rs` documents: the handshake agreed \
         h2, the connector did not report it, and hyper spoke 1.1 over an h2 connection — which in \
         the presence of a real h2 server is a hang rather than an error. origin: {target}"
    );

    let tls = field(&document, "tls").unwrap_or_else(|| {
        panic!(
            "no `tls=` line in the trace answer, so the hop cannot be shown to be TLS: {document:?}"
        )
    });
    assert!(
        tls.starts_with("TLSv1."),
        "the hop must be a real TLS session, or `http/2` above is a statement about a connection \
         that was never encrypted: {tls:?}"
    );
}

/// **A real compressed, chunked body arrives with its framing intact.**
///
/// A fixture can produce gzip and a fixture can produce chunked, but the two are exercised separately
/// and a fixture's gzip is a few hundred bytes of a string somebody wrote. A CDN's answer is both at
/// once over tens of kilobytes, which is where an off-by-one in either decoder shows up and a small
/// fixture hides it.
///
/// The assertion is on `content-encoding: gzip` **and** on the decompressed bytes being markup. A
/// server that dropped the header while relaying compressed bytes fails the second; one that
/// decompressed while leaving the header fails the first. The pair is what makes this a statement
/// about the relay rather than about the origin.
#[test]
fn a_real_compressed_chunked_body_arrives_with_its_framing_intact() {
    require_live!();
    require_binary!(binary);

    let target = origin("example", "https://example.com");
    let server = start_server(&binary, "gzip", &|root| proxy_config(root, &target));

    let answer = http(
        &server.http_address,
        "GET",
        "/",
        &[("Accept-Encoding", "gzip")],
        b"",
    );
    assert_eq!(
        answer.status,
        200,
        "origin: {target}\n{}",
        server.diagnostics()
    );
    assert_eq!(
        answer.header("content-encoding"),
        Some("gzip"),
        "the origin compressed because the client asked, so the header must cross unchanged — an \
         origin that now declines to compress makes this test vacuous, which is what \
         VOLUCER_LIVE_EXAMPLE_ORIGIN is for: {:?}",
        ResponseDebug(&answer)
    );

    // `parse` already removed the chunk framing; the bytes are still gzip, because compression is the
    // origin's and volucer relays it. Decoding here proves the *chunk* framing survived: a boundary in
    // the wrong place corrupts the stream and `GzDecoder` errors rather than returning bytes.
    let mut decoded = Vec::new();
    flate2::read::GzDecoder::new(&answer.body[..])
        .read_to_end(&mut decoded)
        .expect("the gzip body decodes, which is what proves the chunk framing survived");
    assert!(
        decoded.len() > 100,
        "a {} byte page is too small to be the origin's: {:?}",
        decoded.len(),
        String::from_utf8_lossy(&decoded)
    );
    assert!(
        String::from_utf8_lossy(&decoded)
            .to_ascii_lowercase()
            .contains("<html"),
        "the decompressed body must be the page: {:?}",
        String::from_utf8_lossy(&decoded)
    );
}

/// **Two virtual hosts reaching two different real origins, chosen by `Host` alone.**
///
/// The composition no existing suite makes. `volucer-http`'s `two_sites_in_one_process_serve_different_roots`
/// proves site *selection* against static roots built in-process, and `serve.rs` proves that the
/// listener passes the address a connection arrived on rather than the configured one. Neither runs
/// the shipped binary against an origin, so nothing proves the two halves meet: that the site the
/// `Host` selected is the one whose **route table** picks the backend, all the way to a real server.
///
/// # What each end proves
///
/// `alpha` is a **real origin** (`httpbin.org` over plaintext), so the request genuinely crosses the
/// internet through the shipped binary. `beta` is a [local marker](LocalOrigin), because what the
/// test needs from the second backend is only that its answer is unmistakably not the first's — and
/// two public echo services were measured and rejected for the second role: `postman-echo.com`
/// answers `409` to a foreign `Host`, and `httpbingo.org` answers `402` under a test suite's load.
///
/// # Why `beta` is asserted before `alpha`
///
/// Deliberately, and in that order: `alpha` is also `default_backend`, so a run where site selection
/// did nothing at all would still answer `alpha` correctly. Asking `beta` first — a name that is
/// only reachable through `[sites.beta]` — makes a pass require the selection to have happened. The
/// falsification this was checked against (beta's route `host` changed to a name no request carries)
/// fails it.
///
/// # Why the real origin is `http://` by default
///
/// Measured, not assumed, and the measurement is worth recording because it is a real limitation:
///
/// | upstream spoken to | `Host` the origin receives |
/// |---|---|
/// | HTTP/1.1 | the client's own `Host`, verbatim |
/// | HTTP/2 | the origin's own authority, e.g. `httpbin.org:443` |
///
/// `prepare_request` forwards the `host` header faithfully in both cases (`the_clients_host_is_kept_verbatim`
/// pins that in `volucer-upstream`), but an HTTP/2 request carries its identity in the `:authority`
/// pseudo-header instead, and `client.rs`'s `destination` builds that from the origin. hyper's h2
/// client then sends `:authority`, and a server echoes *that* as `Host`. So an origin that selects a
/// virtual host by name is told the name this server dialled rather than the one the client asked
/// for — on the h2 hop only, and `https:` origins negotiate h2 by ALPN.
///
/// This test therefore asserts the property on an HTTP/1.1 hop, which is where it holds, rather than
/// asserting the h2 behaviour either way: encoding a known divergence into a test would freeze it.
/// The default is plaintext for exactly that reason; overriding it with an `https://` origin will
/// fail the `Host` assertion, and that failure is the limitation above rather than a bad fixture.
#[test]
fn two_virtual_hosts_reach_two_different_real_origins() {
    require_live!();
    require_binary!(binary);

    let alpha = origin("vhost_alpha", "http://httpbin.org");
    let marker = LocalOrigin::start("BETA-SITE-MARKER");
    let beta = origin("vhost_beta", &marker.origin_url());
    let server = start_server(&binary, "vhosts", &|root| vhost_config(root, &alpha, &beta));

    // The name only `[sites.beta]` claims. The marker is the assertion: the site was selected, its
    // route table named the `beta` backend, and that backend carried the request.
    let seen = http(
        &server.http_address,
        "GET",
        "/anything",
        &[("Host", "beta.live.test")],
        b"",
    );
    assert_eq!(
        seen.status,
        200,
        "beta.live.test must be answered by the backend its site's route table names. A 502 means \
         the route table of the selected site was not the one consulted. origins: alpha={alpha} \
         beta={beta}\n{}",
        server.diagnostics()
    );
    assert_eq!(
        seen.text(),
        "BETA-SITE-MARKER",
        "beta.live.test reached the wrong backend: `{:?}` is not the beta marker, so the request \
         went to the default site instead of the one its `Host` selected",
        seen.text()
    );

    // And the real origin, to show the two did not both land on one backend — and that the site
    // whose backend is a real server reached it, with the client's identity intact.
    let first = http(
        &server.http_address,
        "GET",
        "/headers",
        &[("Host", "alpha.live.test")],
        b"",
    );
    assert_eq!(
        first.status,
        200,
        "alpha.live.test must reach the real origin `{alpha}`: {}",
        server.diagnostics()
    );
    let document: serde_json::Value = serde_json::from_str(&first.text()).unwrap_or_else(|err| {
        panic!(
            "the origin must answer with JSON ({err}): {:?}",
            first.text()
        )
    });
    assert_eq!(
        document["headers"]["Host"].as_str(),
        Some("alpha.live.test"),
        "the origin must receive the Host the client sent, unchanged — a proxy that replaced it \
         with the upstream's own name would make every virtual host one site: {document:#}"
    );
}

/// **An origin's own status code and redirect reach the client unchanged.**
///
/// A proxy that turned a 418 into a 500, or followed a redirect instead of relaying it, passes every
/// existing suite: those tests *choose* the origin's answer and then assert it, so an origin that
/// always says 200 can never reveal a relay that rewrites statuses.
///
/// Both cases in one test because they are one rule seen from either side — the status line and the
/// `Location` are the origin's, not the proxy's — and a relay that mangled either would be the same
/// defect. `418` is chosen because no layer of this server ever generates it, so it can only have come
/// from the origin.
#[test]
fn a_real_origins_status_and_redirect_reach_the_client_unchanged() {
    require_live!();
    require_binary!(binary);

    let target = origin("status", "https://httpbin.org");
    let server = start_server(&binary, "status", &|root| proxy_config(root, &target));

    let teapot = http(&server.http_address, "GET", "/status/418", &[], b"");
    assert_eq!(
        teapot.status, 418,
        "an origin's status must cross unchanged; this server generates no 418, so any other value \
         means the status was rewritten. origin: {target}\n{}",
        server.diagnostics()
    );

    let redirect = http(&server.http_address, "GET", "/redirect/2", &[], b"");
    assert!(
        (300..400).contains(&redirect.status),
        "the origin's redirect must reach the client rather than being followed: {:?}",
        ResponseDebug(&redirect)
    );
    assert!(
        redirect.header("location").is_some(),
        "a redirect without its Location is a dead end: {:?}",
        ResponseDebug(&redirect)
    );
}

/// **A request body reaches a real origin and is parsed there.**
///
/// The existing end-to-end body test asserts the `Content-Length` an origin saw, which is the framing.
/// What it cannot show is that a real server *parsed* the body: a right length over wrong bytes passes
/// it, and so does a body delivered with the head after it.
///
/// The echo origin answers with what it decoded, so the assertion is on the origin's own parse. A
/// populated `form` means the framing, the encoding and the chunk handling all survived a round trip
/// through two independent implementations.
#[test]
fn a_real_origin_receives_the_request_body_intact() {
    require_live!();
    require_binary!(binary);

    let target = origin("echo", "https://httpbin.org");
    let server = start_server(&binary, "body", &|root| proxy_config(root, &target));

    let answer = http(
        &server.http_address,
        "POST",
        "/anything",
        &[("Content-Type", "application/x-www-form-urlencoded")],
        b"hello=world&n=42",
    );
    assert_eq!(
        answer.status,
        200,
        "origin: {target}\n{}",
        server.diagnostics()
    );

    let document: serde_json::Value = serde_json::from_str(&answer.text()).unwrap_or_else(|err| {
        panic!(
            "the echo origin answered with JSON ({err}): {:?}",
            answer.text()
        )
    });
    assert_eq!(
        document["form"]["hello"], "world",
        "the origin must have parsed the body it was sent: {document:#}"
    );
    assert_eq!(document["form"]["n"], "42", "{document:#}");
    assert_eq!(document["method"], "POST", "{document:#}");
}

/// **A name that does not resolve is a 502, promptly.**
///
/// The live counterpart of `an_unreachable_origin_is_a_502_and_a_silent_one_is_a_504`, worth having
/// separately because the *reason* differs: a loopback refusal is a closed port, this is DNS. A
/// resolver that hangs, or a failure path that retried, turns a misconfigured deployment into a
/// request that sits until the client gives up — which reads as "the server is down" rather than as
/// one wrong origin.
///
/// `.invalid` is reserved by RFC 2606 for exactly this, so it can never be registered and start
/// resolving. The bound is generous: what is being ruled out is a hang, not a slow resolver.
#[test]
fn an_origin_that_does_not_resolve_is_a_502_and_not_a_hang() {
    require_live!();
    require_binary!(binary);

    let target = origin("unresolvable", "https://no-such-origin.invalid");
    let server = start_server(&binary, "unresolvable", &|root| proxy_config(root, &target));

    let started = Instant::now();
    let answer = http(&server.http_address, "GET", "/", &[], b"");
    let elapsed = started.elapsed();

    assert_eq!(
        answer.status,
        502,
        "a name that does not resolve is a bad gateway — not a 500, and not a hang. \
         origin: {target}\n{}",
        server.diagnostics()
    );
    assert!(
        elapsed < Duration::from_secs(30),
        "the failure took {elapsed:?}, which is the shape of a hang rather than a refusal"
    );
}

/// The `key=value` line of an origin's trace document, if it has one.
fn field(document: &str, key: &str) -> Option<String> {
    document.lines().find_map(|line| {
        line.strip_prefix(key)
            .and_then(|rest| rest.strip_prefix('='))
            .map(|value| value.trim().to_string())
    })
}
