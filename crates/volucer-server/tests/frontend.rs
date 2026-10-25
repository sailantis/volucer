//! End-to-end test of the full chain, with no nginx present:
//!
//! ```text
//! FastCGI client (standing in for nginx)
//!     â”€â”€FastCGIâ”€â”€â–º volucer front-end â”€â”€â–º dispatcher â”€â”€FastCGIâ”€â”€â–º php-cgi
//! ```
//!
//! This is the milestone the whole chain was built for. The client role is played
//! by the same codec volucer uses as a client, which is legitimate here: the
//! point is to prove the front-end parses a real FastCGI request, resolves the
//! script through the CGI environment, gets a response from a real PHP process,
//! and re-frames it back without corrupting it.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use volucer_core::config::{BackendConfig, Config, ServerConfig, SpawnCgiConfig};
use volucer_core::sites::Sites;
use volucer_fcgi::{
    message::BeginRequest, nvpair, protocol::RecordType, record, Endpoint, Request, Response,
};
use volucer_server::listener::{FastCgiListener, Limits};
use volucer_server::{backend, Dispatcher};
use volucer_watch::{Registry, SharedState};

mod common;
use common::{find_php_cgi, free_port_run, serial, write_script};

/// Run one request through the in-process front-end on a fresh connection.
///
/// The front-end work is done with the library functions the real listener will
/// call, so the protocol handling under test is the real one.
fn front_end_round_trip(
    dispatcher: &mut Dispatcher,
    request_id: u16,
    params: &[(Vec<u8>, Vec<u8>)],
    body: &[u8],
    keep_conn: bool,
) -> volucer_server::Result<Response> {
    // Serialise the request exactly as nginx would.
    let mut wire = Vec::new();
    let begin = BeginRequest::responder(keep_conn);
    let header = record::Header::new(RecordType::BeginRequest, request_id, 8);
    header
        .write(&mut wire)
        .map_err(volucer_server::ServerError::Fcgi)?;
    wire.write_all(&begin.encode())?;

    let payload = nvpair::encode_pairs(
        params
            .iter()
            .map(|(name, value)| (name.as_slice(), value.as_slice())),
    );
    record::write_records(&mut wire, RecordType::Params, request_id, &payload, true)
        .map_err(volucer_server::ServerError::Fcgi)?;
    record::write_records(&mut wire, RecordType::Stdin, request_id, body, true)
        .map_err(volucer_server::ServerError::Fcgi)?;

    // The front-end reads it.
    let mut cursor = wire.as_slice();
    let incoming = volucer_server::frontend::read_request(
        &mut cursor,
        volucer_core::config::DEFAULT_MAX_BODY_BYTES,
    )
    .expect("the front-end parses the request")
    .expect("a request was present");

    // Dispatch to the backend.
    let response = dispatcher
        .dispatch(&incoming.clone().into_backend_request())
        .expect("the backend served the request");

    // Re-frame it as the front-end would.
    let mut out = Vec::new();
    volucer_server::frontend::write_response(
        &mut out,
        request_id,
        &response.stdout,
        &response.stderr,
        incoming.keep_conn,
    )
    .expect("the front-end writes the response");

    // And read it back the way nginx would.
    let mut reader = out.as_slice();
    let mut decoded = Response::default();
    while let Some(record) =
        record::read_record(&mut reader).map_err(volucer_server::ServerError::Fcgi)?
    {
        match record.record_type() {
            RecordType::Stdout => decoded.stdout.extend_from_slice(&record.content),
            RecordType::Stderr => decoded.stderr.extend_from_slice(&record.content),
            RecordType::EndRequest => {
                decoded.end = Some(
                    volucer_fcgi::EndRequest::decode(&record.content)
                        .map_err(volucer_server::ServerError::Fcgi)?,
                );
                break;
            }
            _ => continue,
        }
    }
    Ok(decoded)
}

/// Start a `php-cgi` pool backend plus a dispatcher in front of it.
fn chain(root: &Path, binary: &Path, base_port: u16) -> Dispatcher {
    let config = Config {
        server: ServerConfig {
            http: Some("127.0.0.1:0".to_string()),
            fastcgi: Some("127.0.0.1:0".to_string()),
            default_backend: Some("cgi".to_string()),
            routes: Vec::new(),
            ..Default::default()
        },
        backends: [(
            "cgi".to_string(),
            BackendConfig::SpawnCgi(SpawnCgiConfig {
                binary: Some(binary.to_path_buf()),
                command: None,
                workers: 1,
                bind_host: "127.0.0.1".to_string(),
                base_port,
                socket: None,
                max_requests: None,
                children: 0,
                concurrency: 0,
                working_dir: None,
                env: Default::default(),
                legacy_capacity: None,
            }),
        )]
        .into_iter()
        .collect(),
        sites: BTreeMap::new(),
        ..volucer_core::config::Config::default()
    };
    config.validate().expect("the config is valid");

    let routing = volucer_fcgi::Routing {
        document_root: root.to_path_buf(),
        front_controller: Some("index.php".to_string()),
        ..volucer_fcgi::Routing::default()
    };
    Dispatcher::from_config(&config, routing).expect("the dispatcher starts")
}

/// Params for a POST, the way nginx would compute them for an app root.
///
/// The method and body length matter: PHP only reads the body when
/// `CONTENT_LENGTH` is present, so a body sent with GET params would be ignored
/// by PHP (correctly) and the test would be proving nothing.
fn params_for(root: &Path, uri: &str, body_len: usize) -> Vec<(Vec<u8>, Vec<u8>)> {
    let routing = volucer_fcgi::Routing {
        document_root: root.to_path_buf(),
        front_controller: Some("index.php".to_string()),
        ..volucer_fcgi::Routing::default()
    };
    let info = volucer_fcgi::RequestInfo {
        method: "POST".to_string(),
        uri: uri.to_string(),
        body_len,
        ..volucer_fcgi::RequestInfo::default()
    };
    volucer_fcgi::build_params(&info, &routing).expect("params build")
}

#[test]
fn a_fastcgi_request_travels_the_whole_chain_and_comes_back_intact() {
    let _serial = serial();
    let Some(binary) = find_php_cgi() else {
        eprintln!("skipping: no php-cgi on PATH");
        return;
    };
    let Some(base) = free_port_run(1) else {
        eprintln!("skipping: no free port");
        return;
    };

    let root = std::env::temp_dir().join(format!("volucer-chain-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create the document root");
    write_script(
        &root,
        "index.php",
        r#"<?php
header('X-Chain: yes');
echo "URI=", $_SERVER['REQUEST_URI'] ?? '-', "\n";
echo "PATH=", $_SERVER['PATH_INFO'] ?? '-', "\n";
echo "BODY=", file_get_contents('php://input');
"#,
    );

    let mut dispatcher = chain(&root, &binary, base);
    let body = b"payload";
    let params = params_for(&root, "/deep/route?x=1", body.len());

    let response = front_end_round_trip(&mut dispatcher, 1, &params, body, true)
        .expect("the round trip completed");

    let stdout = String::from_utf8_lossy(&response.stdout);
    assert!(stdout.contains("URI=/deep/route?x=1"), "{stdout}");
    assert!(stdout.contains("PATH=/deep/route"), "{stdout}");
    assert!(stdout.contains("BODY=payload"), "{stdout}");
    assert!(stdout.contains("X-Chain: yes"), "{stdout}");

    assert!(
        response
            .end
            .map(|end| end.protocol_status.is_complete())
            .unwrap_or(false),
        "the front-end must end the request with REQUEST_COMPLETE"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn the_front_end_serves_several_requests_in_a_row() {
    let _serial = serial();
    let Some(binary) = find_php_cgi() else {
        eprintln!("skipping: no php-cgi on PATH");
        return;
    };
    let Some(base) = free_port_run(1) else {
        eprintln!("skipping: no free port");
        return;
    };

    let root = std::env::temp_dir().join(format!("volucer-chain-many-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create the document root");
    write_script(
        &root,
        "index.php",
        r#"<?php echo "I="; echo file_get_contents('php://input');"#,
    );

    let mut dispatcher = chain(&root, &binary, base);

    for index in 0..4 {
        let body = format!("req{index}");
        let params = params_for(&root, "/", body.len());
        let response = front_end_round_trip(&mut dispatcher, 1, &params, body.as_bytes(), true)
            .unwrap_or_else(|err| panic!("request {index} failed: {err}"));
        let stdout = String::from_utf8_lossy(&response.stdout);
        assert!(stdout.contains(&format!("I=req{index}")), "{stdout}");
    }

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_route_selects_the_backend_for_a_fastcgi_request_too() {
    let _serial = serial();

    // Two backends, both unreachable, so nothing is started — the assertion is about *which*
    // one the front-end chooses, and the error message names it. That is the whole reason the
    // route table has to be consulted here: it decides which pool serves a request, and the two
    // front-ends must not decide differently.
    let socket_of = |port: u16| volucer_core::config::ExternalConfig {
        socket: format!("127.0.0.1:{port}"),
        max_connections: None,
    };
    let config = Config {
        server: ServerConfig {
            fastcgi: Some("127.0.0.1:0".to_string()),
            default_backend: Some("default_pool".to_string()),
            routes: vec![volucer_core::config::RouteConfig {
                path_prefix: Some("/api".to_string()),
                path_suffix: None,
                host: None,
                policy: volucer_core::config::RoutePolicy::All,
                backend: Some("api_pool".to_string()),
            }],
            ..Default::default()
        },
        backends: [
            (
                "default_pool".to_string(),
                BackendConfig::External(socket_of(1)),
            ),
            (
                "api_pool".to_string(),
                BackendConfig::External(socket_of(2)),
            ),
        ]
        .into_iter()
        .collect(),
        sites: BTreeMap::new(),
        ..volucer_core::config::Config::default()
    };
    config.validate().expect("valid config");
    let dispatcher =
        Dispatcher::from_config(&config, volucer_fcgi::Routing::default()).expect("dispatcher");

    // `/api` is routed to the pool the route names; anything else falls to the default. The
    // environment is built directly rather than through the wire codec, because this test is
    // about which backend is chosen, not about parsing.
    let request_with = |uri: &str| volucer_fcgi::Request {
        params: vec![
            (b"REQUEST_METHOD".to_vec(), b"GET".to_vec()),
            (b"REQUEST_URI".to_vec(), uri.as_bytes().to_vec()),
            (b"HTTP_HOST".to_vec(), b"example.test".to_vec()),
        ],
        body: Vec::new(),
        keep_conn: true,
    };

    let err = dispatcher
        .dispatch_with_routes(&config.server.routes, &request_with("/api/users"))
        .expect_err("nothing listens on either port");
    assert!(
        err.to_string().contains("api_pool"),
        "the route must have chosen the pool it names: {err}"
    );

    let err = dispatcher
        .dispatch_with_routes(&config.server.routes, &request_with("/other"))
        .expect_err("nothing listens on either port");
    assert!(
        err.to_string().contains("default_pool"),
        "an unrouted request must reach the default backend: {err}"
    );
}

/// Open one FastCGI connection, the way nginx's upstream keep-alive does: several requests
/// arrive on one connection, and that connection is accepted once.
fn fastcgi_connection(address: std::net::SocketAddr) -> std::io::BufReader<TcpStream> {
    let stream = TcpStream::connect(address).expect("connect to the front-end");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("read timeout");
    std::io::BufReader::new(stream)
}

/// Send one request on an existing connection, and read the answer.
///
/// `keep_conn` is `true` and the loop stops at `END_REQUEST`, so the connection stays open for the
/// next call — which is the point rather than a convenience. A test that opened a fresh connection
/// per request would pass against a listener that captured its site table when the connection was
/// *accepted*, and that listener is the bug: nginx keeps an upstream alive, so the request that
/// arrives after a reload very often arrives on a connection accepted before it.
fn request_on(
    reader: &mut std::io::BufReader<TcpStream>,
    request_id: u16,
    host: &str,
    uri: &str,
) -> String {
    let params = [
        (b"REQUEST_METHOD".as_slice(), b"GET".as_slice()),
        (b"REQUEST_URI".as_slice(), uri.as_bytes()),
        (b"SCRIPT_FILENAME".as_slice(), uri.as_bytes()),
        (b"SERVER_PROTOCOL".as_slice(), b"HTTP/1.1".as_slice()),
        (b"HTTP_HOST".as_slice(), host.as_bytes()),
    ];
    let mut wire = Vec::new();
    let begin = BeginRequest::responder(true);
    record::Header::new(RecordType::BeginRequest, request_id, 8)
        .write(&mut wire)
        .expect("write BEGIN_REQUEST");
    wire.write_all(&begin.encode()).expect("write begin body");
    let payload = nvpair::encode_pairs(params);
    record::write_records(&mut wire, RecordType::Params, request_id, &payload, true)
        .expect("write params");
    record::write_records(&mut wire, RecordType::Stdin, request_id, b"", true)
        .expect("write stdin");
    reader.get_mut().write_all(&wire).expect("send the request");
    reader.get_mut().flush().expect("flush");

    // Read until END_REQUEST, which is the front-end's own terminator. A timeout here would mean
    // the listener answered nothing, so it is a 5 s bound rather than an unbounded read.
    let mut out = Vec::new();
    loop {
        let record = match record::read_record(reader) {
            Ok(Some(record)) => record,
            Ok(None) => break,
            Err(err) => panic!("the front-end sent a malformed record: {err}"),
        };
        let end = record.record_type() == RecordType::EndRequest;
        out.extend_from_slice(&record.content);
        if end {
            break;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[test]
fn a_reload_moves_the_fastcgi_front_ends_site_table_too() {
    // The defect this pins, and it was real rather than hypothetical: `FastCgiListener` holds its
    // **own** `Arc<Sites>` and resolves the site per request, while a reload replaced only the HTTP
    // front-end's table. `serve` hands both front-ends the same `Arc`, so a site whose backend a
    // reload changed kept reaching the old one behind nginx while the same change took effect over
    // HTTP. That failure does not look like a race or like a bug — it looks like the *other* front
    // end being right, and it is invisible to a test that only exercises HTTP.
    //
    // Asserted through the real connection path, because the property is "a request arriving after
    // the reload reaches the new backend", and a table that moved while the accept loop kept a
    // stale clone would pass a table-level assertion.
    let roots = std::env::temp_dir().join(format!("volucer-fcgi-reload-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&roots);
    for name in ["first", "second"] {
        std::fs::create_dir_all(roots.join(name)).expect("create a root");
    }

    // Two pools with nothing behind them, and the *site's own route table* is what selects
    // between them. That is the mechanism this front-end uses: a request arriving from nginx has
    // its `SCRIPT_FILENAME` already resolved, so the only thing site selection can change is which
    // pool serves it — and a route naming a backend is the operator's way of saying which.
    //
    // The assertion is therefore which pool is *chosen*, and the dispatcher names it in its error.
    // That is the technique `a_route_selects_the_backend_for_a_fastcgi_request_too` already uses,
    // and it is why neither pool needs a PHP process behind it.
    let config_for = |pool: &str, socket: &str, root: &Path, path_suffix: &str| {
        let text = format!(
            r#"
[server]
default_backend = "unreachable"
http = "127.0.0.1:0"
fastcgi = "127.0.0.1:0"
document_root = {root:?}

[backends.{pool}]
kind = "external"
socket = "{socket}"

[backends.unreachable]
kind = "external"
socket = "127.0.0.1:9"

[[server.routes]]
path_suffix = "{path_suffix}"
backend = "{pool}"
"#,
            root = root.to_string_lossy(),
        );
        let config = Config::from_toml(&text).expect("config parses");
        config.validate().expect("config validates");
        config
    };

    // The two configs differ only in which pool `/probe.php` reaches, so the reload has to be
    // attributable to the site table rather than to everything about the config.
    let before = config_for("first_pool", "127.0.0.1:1", &roots.join("first"), ".php");
    let after = config_for("second_pool", "127.0.0.1:2", &roots.join("second"), ".php");

    let sites = SharedState::new(Sites::from_config(&before, &roots.join("first")));
    let dispatcher = Arc::new(
        Dispatcher::from_config(&before, volucer_fcgi::Routing::default()).expect("dispatcher"),
    );

    let registry = Arc::new(Registry::new(sites));
    let listener = FastCgiListener::bind_registered(
        "127.0.0.1:0",
        Arc::clone(&dispatcher),
        Arc::clone(&registry),
        Limits::default(),
    )
    .expect("bind the fastcgi listener");
    let address = listener.local_addr().expect("bound address");
    let reload = listener.reload_handle();
    let shutdown = listener.shutdown_handle();

    let handle = std::thread::spawn(move || listener.run());
    // Give the accept loop a moment to reach `accept`, so the first request is not racing startup.
    std::thread::sleep(Duration::from_millis(100));

    // **One connection for both requests**, which is the whole point: nginx keeps its upstream
    // alive, so the request that arrives after a reload usually arrives on a connection accepted
    // before it. A listener that captured the site table when the connection was accepted would
    // serve the old pool for ever on that connection, and a test that dialled a fresh connection
    // per request would pass against it.
    let mut connection = fastcgi_connection(address);
    let first = request_on(&mut connection, 1, "any.test", "/probe.php");
    assert!(
        first.contains("first_pool"),
        "the fixture must reach the starting pool before the reload: {first}"
    );

    // The reload, through the shared handle every front-end holds. The new table's site routes
    // `/probe.php` to the other pool, so a listener that kept a stale clone is visible in the
    // answer rather than only in the table.
    let _replaced = reload.apply(
        Sites::from_config(&after, &roots.join("second")),
        std::sync::Arc::new(volucer_watch::GrpcGrantsMap::new()),
    );

    let second = request_on(&mut connection, 2, "any.test", "/probe.php");
    assert!(
        second.contains("second_pool"),
        "a request after the reload must reach the new backend, on a connection accepted before \
         the reload: {second}"
    );
    assert!(
        !second.contains("first_pool"),
        "the old backend must not still be chosen: {second}"
    );

    shutdown.shutdown();
    let _ = handle.join();
    let _ = std::fs::remove_dir_all(&roots);
}

#[test]
fn a_backend_failure_becomes_a_502_rather_than_a_broken_connection() {
    let _serial = serial();

    // Point at a socket with nothing behind it, so the dispatch fails.
    let config = Config {
        server: ServerConfig {
            fastcgi: Some("127.0.0.1:0".to_string()),
            default_backend: Some("dead".to_string()),
            routes: Vec::new(),
            ..Default::default()
        },
        backends: [(
            "dead".to_string(),
            BackendConfig::External(volucer_core::config::ExternalConfig {
                socket: "127.0.0.1:1".to_string(),
                max_connections: None,
            }),
        )]
        .into_iter()
        .collect(),
        sites: BTreeMap::new(),
        ..volucer_core::config::Config::default()
    };
    config.validate().expect("valid config");
    let dispatcher =
        Dispatcher::from_config(&config, volucer_fcgi::Routing::default()).expect("dispatcher");

    let wire = {
        let mut wire = Vec::new();
        let begin = BeginRequest::responder(true);
        let header = record::Header::new(RecordType::BeginRequest, 5, 8);
        header.write(&mut wire).unwrap();
        wire.write_all(&begin.encode()).unwrap();
        record::write_records(&mut wire, RecordType::Params, 5, b"", true).unwrap();
        record::write_records(&mut wire, RecordType::Stdin, 5, b"", true).unwrap();
        wire
    };

    let mut cursor = wire.as_slice();
    let incoming = volucer_server::frontend::read_request(
        &mut cursor,
        volucer_core::config::DEFAULT_MAX_BODY_BYTES,
    )
    .unwrap()
    .unwrap();

    // The dispatch fails, so the front-end must answer with an error response.
    let err = dispatcher
        .dispatch(&incoming.into_backend_request())
        .expect_err("nothing is listening");

    let kind = match &err {
        volucer_server::ServerError::Worker { .. } => backend::is_transport_error(match &err {
            volucer_server::ServerError::Worker { source, .. } => source,
            _ => unreachable!(),
        }),
        _ => false,
    };
    let kind = if kind {
        volucer_server::frontend::ServerErrorKind::Backend
    } else {
        volucer_server::frontend::ServerErrorKind::Overloaded
    };

    let mut out = Vec::new();
    volucer_server::frontend::write_error(&mut out, 5, &err.to_string(), kind)
        .expect("the error response is written");

    // nginx sees a 502, and the connection stays framed.
    let mut reader = out.as_slice();
    let mut body = Vec::new();
    while let Some(record) = record::read_record(&mut reader).unwrap() {
        match record.record_type() {
            RecordType::Stdout => body.extend_from_slice(&record.content),
            RecordType::EndRequest => break,
            _ => continue,
        }
    }
    let body = String::from_utf8_lossy(&body);
    assert!(body.contains("502 Bad Gateway"), "{body}");
}

/// Proves the front-end and the real client agree on framing, over a real
/// socket rather than an in-memory buffer.
#[test]
fn the_frame_codec_works_over_a_real_socket() {
    let _serial = serial();
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind a listener");
    let address = listener.local_addr().expect("address");

    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let request = volucer_server::frontend::read_request(
            &mut stream,
            volucer_core::config::DEFAULT_MAX_BODY_BYTES,
        )
        .expect("read the request")
        .expect("a request arrived");
        let method = request.param_str("REQUEST_METHOD").unwrap_or_default();
        let body = String::from_utf8_lossy(&request.body).into_owned();
        let reply = format!("Status: 200 OK\r\n\r\necho={method}:{body}");
        volucer_server::frontend::write_response(&mut stream, 1, reply.as_bytes(), b"log", true)
            .expect("write the response");
        (method, body)
    });

    let mut client = volucer_fcgi::Client::connect(
        &Endpoint::Tcp(address.to_string()),
        Some(Duration::from_secs(5)),
    )
    .expect("connect");

    let response = client
        .request(&Request {
            params: vec![
                (b"REQUEST_METHOD".to_vec(), b"POST".to_vec()),
                (b"CONTENT_LENGTH".to_vec(), b"4".to_vec()),
            ],
            body: b"data".to_vec(),
            keep_conn: true,
        })
        .expect("the front-end answered");

    let (method, body) = server.join().expect("the server thread finished");
    assert_eq!(method, "POST");
    assert_eq!(body, "data");

    let stdout = String::from_utf8_lossy(&response.stdout);
    assert!(stdout.contains("echo=POST:data"), "{stdout}");
    assert_eq!(response.stderr, b"log");
}

/// A raw HTTP-ish request through a TcpStream, to show the listener shape.
#[test]
fn a_socket_front_end_can_be_built_from_the_parser() {
    let _serial = serial();
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = listener.local_addr().expect("address");

    let handle = std::thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept");
        serve_one(stream)
    });

    let mut stream = TcpStream::connect(address).expect("connect");
    let params = nvpair::encode_pairs([
        (&b"REQUEST_METHOD"[..], &b"GET"[..]),
        (&b"REQUEST_URI"[..], &b"/hello"[..]),
    ]);
    let mut wire = Vec::new();
    let header = record::Header::new(RecordType::BeginRequest, 1, 8);
    header.write(&mut wire).unwrap();
    wire.write_all(&BeginRequest::responder(false).encode())
        .unwrap();
    record::write_records(&mut wire, RecordType::Params, 1, &params, true).unwrap();
    record::write_records(&mut wire, RecordType::Stdin, 1, b"", true).unwrap();

    stream.write_all(&wire).expect("write the request");
    stream.flush().expect("flush");

    let mut reply = Vec::new();
    stream.read_to_end(&mut reply).expect("read the reply");

    let mut cursor = reply.as_slice();
    let mut body = Vec::new();
    while let Some(record) = record::read_record(&mut cursor).expect("parse the reply") {
        if record.record_type() == RecordType::Stdout {
            body.extend_from_slice(&record.content);
        }
    }

    assert_eq!(handle.join().expect("join"), "GET /hello");
    let body = String::from_utf8_lossy(&body);
    assert!(body.contains("GET /hello"), "{body}");
}

/// Handle exactly one request on a connection, then close.
fn serve_one(mut stream: TcpStream) -> String {
    let request = volucer_server::frontend::read_request(
        &mut stream,
        volucer_core::config::DEFAULT_MAX_BODY_BYTES,
    )
    .expect("read the request")
    .expect("a request arrived");
    let summary = format!(
        "{} {}",
        request.param_str("REQUEST_METHOD").unwrap_or_default(),
        request.param_str("REQUEST_URI").unwrap_or_default()
    );
    let reply = format!("Status: 200 OK\r\n\r\n{summary}");
    volucer_server::frontend::write_response(&mut stream, 1, reply.as_bytes(), b"", false)
        .expect("write the response");
    let _ = stream.shutdown(std::net::Shutdown::Write);
    summary
}

#[test]
fn a_body_over_server_max_body_bytes_is_a_413_over_a_real_connection() {
    let _serial = serial();

    // **The defect this closes is that the FastCGI front-end had no bound at all.**
    // `read_next` extended a `Vec` for as long as `STDIN` records arrived, so a request that
    // reached this listener could make it allocate without limit — the protection was nginx's
    // `client_max_body_size`, on somebody else's configuration. `server.max_body_bytes` now bounds
    // both front-ends, and this is the assertion that it reaches *this* one.
    //
    // The backend is an unreachable socket on purpose: a refusal that only became visible after a
    // dispatch would be a limit applied too late, and an unreachable backend turns any such late
    // refusal into a 502 that this test would catch.
    let config = Config {
        server: ServerConfig {
            fastcgi: Some("127.0.0.1:0".to_string()),
            max_body_bytes: Some(16),
            default_backend: Some("dead".to_string()),
            ..Default::default()
        },
        backends: [(
            "dead".to_string(),
            BackendConfig::External(volucer_core::config::ExternalConfig {
                socket: "127.0.0.1:1".to_string(),
                max_connections: None,
            }),
        )]
        .into_iter()
        .collect(),
        ..volucer_core::config::Config::default()
    };
    config.validate().expect("a stated limit is valid");
    assert_eq!(config.server.max_body_bytes(), 16);

    let sites = SharedState::new(Sites::from_config(&config, Path::new(".")));
    let dispatcher = Arc::new(
        Dispatcher::from_config(&config, volucer_fcgi::Routing::default()).expect("dispatcher"),
    );
    let listener = FastCgiListener::bind_registered(
        "127.0.0.1:0",
        dispatcher,
        Arc::new(Registry::new(sites)),
        Limits {
            max_connections: 8,
            max_body_bytes: config.server.max_body_bytes(),
        },
    )
    .expect("bind the fastcgi listener");
    let address = listener.local_addr().expect("bound address");
    let shutdown = listener.shutdown_handle();
    let handle = std::thread::spawn(move || listener.run());
    std::thread::sleep(Duration::from_millis(100));

    // Two STDIN records over a 16-byte limit: the refusal has to happen on the second one, while
    // the body is still arriving. One record would be refused by the same check, so splitting it
    // is what proves the limit is cumulative rather than per record.
    let mut wire = Vec::new();
    record::Header::new(RecordType::BeginRequest, 9, 8)
        .write(&mut wire)
        .expect("write BEGIN_REQUEST");
    wire.write_all(&BeginRequest::responder(true).encode())
        .expect("write begin body");
    record::write_records(&mut wire, RecordType::Params, 9, b"", true).expect("write params");
    // **`false`, not `true`.** `write_records` emits a zero-length terminator when asked to, so
    // two `true` calls send `chunk, END, chunk, END` and the body ends at the first chunk — the
    // fixture then never reaches the limit this test exists to exercise. That is how the first
    // version of this test reported a 502 from the backend the body was never sent to.
    record::write_records(&mut wire, RecordType::Stdin, 9, b"0123456789", false)
        .expect("write the first chunk");
    record::write_records(&mut wire, RecordType::Stdin, 9, b"0123456789", false)
        .expect("write the second chunk");
    record::write_records(&mut wire, RecordType::Stdin, 9, b"", true).expect("terminate stdin");

    let mut connection = fastcgi_connection(address);
    connection
        .get_mut()
        .write_all(&wire)
        .expect("send the request");
    connection.get_mut().flush().expect("flush");

    let mut answer = Vec::new();
    loop {
        match record::read_record(&mut connection) {
            Ok(Some(record)) => {
                let end = record.record_type() == RecordType::EndRequest;
                answer.extend_from_slice(&record.content);
                if end {
                    break;
                }
            }
            // **The listener closed the connection instead of answering**, which is the failure
            // mode this test exists to catch: a client would see an upstream failure rather than
            // a 413.
            Ok(None) => panic!("the listener closed without answering"),
            Err(err) => panic!("the listener sent a malformed record: {err}"),
        }
    }
    let text = String::from_utf8_lossy(&answer);
    assert!(text.contains("Status: 413 Content Too Large"), "{text}");
    assert!(
        text.contains("max_body_bytes"),
        "the message must name the key: {text}"
    );
    assert!(
        !text.contains("502"),
        "an oversized body is not a gateway failure: {text}"
    );

    shutdown.shutdown();
    let _ = handle.join();
}

/// One FastCGI request carrying a `REMOTE_ADDR`, sent on an existing connection.
///
/// The params are the minimum a rate limit needs: nginx sets `REMOTE_ADDR = $remote_addr`, so this
/// is what the limiter keys on when `key = "ip"`.
fn request_from(
    reader: &mut std::io::BufReader<TcpStream>,
    request_id: u16,
    remote_addr: &str,
    uri: &str,
) -> String {
    let params = [
        (b"REQUEST_METHOD".as_slice(), b"GET".as_slice()),
        (b"REQUEST_URI".as_slice(), uri.as_bytes()),
        (b"SCRIPT_FILENAME".as_slice(), uri.as_bytes()),
        (b"SERVER_PROTOCOL".as_slice(), b"HTTP/1.1".as_slice()),
        (b"REMOTE_ADDR".as_slice(), remote_addr.as_bytes()),
    ];
    let mut wire = Vec::new();
    let begin = BeginRequest::responder(true);
    record::Header::new(RecordType::BeginRequest, request_id, 8)
        .write(&mut wire)
        .expect("write BEGIN_REQUEST");
    wire.write_all(&begin.encode()).expect("write begin body");
    let payload = nvpair::encode_pairs(params);
    record::write_records(&mut wire, RecordType::Params, request_id, &payload, true)
        .expect("write params");
    record::write_records(&mut wire, RecordType::Stdin, request_id, b"", true)
        .expect("write stdin");
    reader.get_mut().write_all(&wire).expect("send the request");
    reader.get_mut().flush().expect("flush");

    let mut out = Vec::new();
    loop {
        let record = match record::read_record(reader) {
            Ok(Some(record)) => record,
            Ok(None) => break,
            Err(err) => panic!("the front-end sent a malformed record: {err}"),
        };
        let end = record.record_type() == RecordType::EndRequest;
        out.extend_from_slice(&record.content);
        if end {
            break;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[test]
fn a_rate_limited_fastcgi_request_is_a_429_with_a_retry_after_and_keeps_the_connection() {
    // The FastCGI half of the rate limit, and **the same policy instance** the HTTP front-end would
    // hold — this test builds one the way `serve` does and hands it to this listener, so the
    // assertion covers the sharing rather than a second limiter with the same settings.
    //
    // The backend is deliberately unreachable: a refusal cannot then be confused with a backend
    // failure, and a *permitted* request is observable as a 502 from the front controller rather
    // than as a 200 that some PHP script happened to produce.
    let root = std::env::temp_dir().join(format!("volucer-fcgi-rate-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create the root");

    let text = format!(
        r#"
[server]
default_backend = "unreachable"
fastcgi = "127.0.0.1:0"
document_root = {root:?}
"#,
        root = root.to_string_lossy(),
    );
    let config = Config::from_toml(&text).expect("config parses");
    // No backends declared: `Config::validate` refuses a `default_backend` naming none, which is a
    // fact about this fixture rather than about the limiter, so it is asserted rather than ignored —
    // a fixture that quietly stopped parsing would make the test below pass for the wrong reason.
    assert!(
        config.validate().is_err(),
        "this fixture deliberately names a backend it does not declare"
    );

    let dispatcher = Arc::new(
        Dispatcher::from_config(&config, volucer_fcgi::Routing::default()).expect("dispatcher"),
    );
    let sites = SharedState::new(Sites::from_config(&config, &root));
    let registry = Arc::new(Registry::new(sites));
    let limiter = Arc::new(volucer_watch::RateLimitCell::new(Some(
        volucer_core::RateLimitPolicy::new(
            volucer_core::KeyMode::Ip,
            None,
            1,
            Duration::from_secs(3600),
            Some(1),
            None,
        )
        .expect("the policy is valid"),
    )));

    let listener = FastCgiListener::bind_registered(
        "127.0.0.1:0",
        Arc::clone(&dispatcher),
        Arc::clone(&registry),
        Limits::default(),
    )
    .expect("bind the fastcgi listener")
    .with_rate_limit(Arc::clone(&limiter));
    let address = listener.local_addr().expect("bound address");
    let shutdown = listener.shutdown_handle();
    let handle = std::thread::spawn(move || listener.run());
    std::thread::sleep(Duration::from_millis(100));

    let mut connection = fastcgi_connection(address);

    // The one permitted request reaches the backend, which cannot be reached — a 502 rather than a
    // 429, which is what makes the next assertion attributable to the limit.
    let permitted = request_from(&mut connection, 1, "203.0.113.7", "/index.php");
    assert!(
        permitted.contains("Status: 502") || permitted.contains("Status: 200"),
        "the first request must be allowed through to the backend: {permitted}"
    );
    assert!(
        !permitted.contains("429"),
        "the first request is inside the burst: {permitted}"
    );

    // The same connection, and the second request is refused. **`continue` rather than `close`** is
    // the property this pins: an over-limit request is refused *and the connection survives*, so
    // nginx can send the next one — which is what it does for any other 4xx.
    let refused = request_from(&mut connection, 2, "203.0.113.7", "/index.php");
    assert!(
        refused.contains("Status: 429 Too Many Requests"),
        "{refused}"
    );
    assert_eq!(
        refused.matches("Retry-After: 3600").count(),
        1,
        "the header has to carry the limiter's own number, exactly once: {refused}"
    );
    assert!(
        refused.contains("no-store"),
        "a cacheable 429 would be served to every other client: {refused}"
    );

    // And a *different* address on the same connection is not throttled, which is the whole point
    // of `key = "ip"`: a limiter keyed on something every request shared would fail here.
    let other = request_from(&mut connection, 3, "203.0.113.8", "/index.php");
    assert!(
        !other.contains("429"),
        "another client has its own bucket: {other}"
    );

    shutdown.shutdown();
    let _ = handle.join();
    let _ = std::fs::remove_dir_all(&root);
}
