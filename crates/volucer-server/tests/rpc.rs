//! The `[rpc]` listener, end to end over a real socket.
//!
//! What these cover is everything a unit test of `RpcGrants` cannot: that the framing on the wire is
//! the worker protocol's, that a refusal reaches the caller as a `RETURN` rather than as a closed
//! connection, that the idle deadline closes a quiet connection rather than hanging, and that a
//! connection serves many calls — which is the whole reason a CGI application pays a `connect` once.
//!
//! **No `php-cgi` here.** A client written against `volucer_worker`'s own codec is the honest peer
//! for a test of the *server*: an SDK test would fail for reasons that are the SDK's, and a
//! test that needed a PHP binary would not run in CI's non-PHP job. `php-cgi` is exercised by the
//! `spawn_cgi` integration tests, which already have a fixture for it.

use std::io::Write;
use std::sync::Arc;
use std::time::Duration;

use volucer_server::calls::{MethodRegistry, Namespace};
use volucer_server::{RpcGrants, RpcListener, Token};
use volucer_worker::{Call, Codec, Header, Json, MessageType, Return};

/// A namespace that echoes what it was asked, so an assertion can tell a served call from a refusal.
struct Echo;

impl Namespace for Echo {
    fn name(&self) -> &'static str {
        "echo"
    }

    fn serve(&self, call: &Call) -> Return {
        Return::ok(serde_json::json!({ "name": call.name, "params": call.params }))
    }
}

const SECRET: &str = "a secret only this process knows";

fn grants(backends: &[(&str, &[&str])]) -> Arc<RpcGrants> {
    let mut registries = std::collections::BTreeMap::new();
    for (name, services) in backends {
        let mut registry = MethodRegistry::new();
        // The namespaces a test grants are the ones it names, so a registry that serves nothing is
        // distinguishable from one that serves the wrong thing.
        if services.contains(&"echo") {
            registry = registry.with(Echo);
        }
        registries.insert((*name).to_string(), Arc::new(registry));
    }
    Arc::new(RpcGrants::new(registries, SECRET))
}

/// A bound listener on an ephemeral port, with its serving thread already running.
struct Running {
    address: String,
    listener: Arc<RpcListener>,
    stop: volucer_server::RpcStopHandle,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Running {
    fn start(grants: Arc<RpcGrants>, config: &str) -> Self {
        let config = volucer_core::config::RpcConfig {
            listen: config.to_string(),
            max_connections: None,
            idle_timeout_seconds: 30,
            call_timeout_seconds: 5,
        };
        let listener = Arc::new(RpcListener::bind(&config, grants).expect("the listener binds"));
        let address = listener
            .bound_address()
            .expect("an ephemeral TCP port")
            .to_string();
        let stop = listener.stop_handle();
        let running = Arc::clone(&listener);
        let thread = std::thread::spawn(move || running.run().expect("the listener serves"));
        Running {
            address,
            listener,
            stop,
            thread: Some(thread),
        }
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        self.stop.stop();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// A client bound to the protocol, with the token it was handed.
struct Client {
    stream: std::net::TcpStream,
    reader: std::io::BufReader<std::net::TcpStream>,
    next_id: u32,
}

impl Client {
    fn connect(address: &str) -> Self {
        let stream = std::net::TcpStream::connect(address).expect("connects");
        stream
            .set_nodelay(true)
            .expect("nodelay is settable on a connected socket");
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .expect("a read deadline");
        let reader = std::io::BufReader::new(stream.try_clone().expect("a second handle"));
        Client {
            stream,
            reader,
            // The first call id the SDK hands out, counting down as the protocol says. A call whose
            // id looked like a request's would be refused by the framing layer, which is a different
            // test.
            next_id: u32::MAX,
        }
    }

    /// Send one call and read its answer.
    fn call(&mut self, call: &Call) -> Return {
        let id = self.next_id;
        self.next_id -= 1;
        let payload = Json::encode(call).expect("encodes");
        let header = Header::new(MessageType::Call, id, &payload).expect("a header");
        volucer_worker::write_frame(&mut self.stream, &header, &payload).expect("the call is sent");

        let (header, payload) = Header::read_frame(&mut self.reader)
            .expect("a frame arrives")
            .expect("the server did not close");
        assert_eq!(
            header.message_type,
            MessageType::Return,
            "a call is answered with a RETURN, not with a stream that closes"
        );
        assert_eq!(header.request_id, id, "the answer is correlated by id");
        Json::decode(&payload).expect("the answer decodes")
    }

    fn token(&self, token: &str) -> Call {
        Call {
            name: String::new(),
            params: serde_json::Value::Null,
            traceparent: None,
            token: Some(token.to_string()),
        }
    }
}

fn token_for(backend: &str) -> String {
    Token::derive(SECRET, backend)
}

#[test]
fn a_granted_call_is_served_and_a_second_call_on_the_same_connection_is_too() {
    // The whole point of the feature, and the reason a CGI application is willing to pay for it: one
    // connection, many calls. A listener that closed after one answer would work — FastCGI
    // applications would still call — and would make every request pay a `connect`, which is the
    // cost this shape exists to avoid.
    let running = Running::start(grants(&[("app", &["echo"])]), "127.0.0.1:0");
    let mut client = Client::connect(&running.address);

    let mut call = client.token(&token_for("app"));
    call.name = "echo.ping".to_string();
    call.params = serde_json::json!({ "first": 1 });
    let answer = client.call(&call);
    assert!(answer.ok, "a granted call is served: {answer:?}");
    assert_eq!(
        answer.result.expect("a result"),
        serde_json::json!({ "name": "echo.ping", "params": { "first": 1 } }),
        "the namespace saw the call it was sent"
    );

    call.params = serde_json::json!({ "second": 2 });
    let again = client.call(&call);
    assert!(again.ok, "a second call on the same connection is served");
    assert_eq!(
        again.result.expect("a result")["params"]["second"],
        2,
        "and it is the second call's params, so nothing from the first was reused"
    );

    assert_eq!(running.listener.stats().calls, 2, "both calls are counted");
}

#[test]
fn a_call_with_no_token_is_refused_by_name_and_the_connection_survives() {
    // The refusal is an ordinary `RETURN`, not an `ERROR` frame and not a close. That is what lets
    // the SDK surface it through `lastCallError()` — the same path every other refusal takes — and it
    // is what makes the *next* call on the connection attemptable rather than lost.
    let running = Running::start(grants(&[("app", &["echo"])]), "127.0.0.1:0");
    let mut client = Client::connect(&running.address);

    let call = Call {
        name: "echo.ping".to_string(),
        params: serde_json::Value::Null,
        traceparent: None,
        token: None,
    };
    let answer = client.call(&call);
    assert!(!answer.ok, "a call with no token is refused");
    let error = answer.error.expect("a reason");
    assert!(
        error.contains("no token") && error.contains("VOLUCER_RPC_TOKEN"),
        "the refusal says what is missing and where it comes from: {error}"
    );

    // And the connection is still usable, which is the half a close would have lost.
    let mut call = client.token(&token_for("app"));
    call.name = "echo.ping".to_string();
    let answer = client.call(&call);
    assert!(
        answer.ok,
        "the connection is still usable afterwards: {answer:?}"
    );
}

#[test]
fn a_token_no_backend_was_issued_is_refused_without_echoing_it() {
    // Two failures that are fixed in different files, and the difference is why `RpcGrants` keeps the
    // token map rather than just the registries: this one is "the deployment handed the wrong address
    // (or a stale token) to this application", while the next test is "the `services` list is missing
    // a line". A single "refused" would leave an operator choosing between them.
    let running = Running::start(grants(&[("app", &["echo"])]), "127.0.0.1:0");
    let mut client = Client::connect(&running.address);

    let mut call = client.token("deadbeefdeadbeefdeadbeefdeadbeef");
    call.name = "echo.ping".to_string();
    let answer = client.call(&call);
    assert!(!answer.ok);
    let error = answer.error.expect("a reason");
    assert!(
        error.contains("no backend of this deployment was issued"),
        "the refusal names the actual problem: {error}"
    );
    assert!(
        !error.contains("deadbeef"),
        "and it does not publish the token it was given, which a log would keep: {error}"
    );
}

#[test]
fn a_token_names_only_what_its_own_backend_grants() {
    // The capability boundary, and the one thing a shared socket makes easy to get wrong: one
    // listener serves every granted backend, so the *token* has to decide. Two backends, two
    // registries, and a backend that granted nothing reaches nothing even though the socket serves
    // something.
    let running = Running::start(
        grants(&[("app", &["echo"]), ("public", &[])]),
        "127.0.0.1:0",
    );
    let mut client = Client::connect(&running.address);

    let mut granted = client.token(&token_for("app"));
    granted.name = "echo.ping".to_string();
    assert!(
        client.call(&granted).ok,
        "the backend that granted the namespace reaches it"
    );

    let mut ungranted = client.token(&token_for("public"));
    ungranted.name = "echo.ping".to_string();
    let answer = client.call(&ungranted);
    assert!(
        !answer.ok,
        "a backend that granted nothing reaches nothing, over the same socket"
    );
    let error = answer.error.expect("a reason");
    assert!(
        error.contains("no echo namespace is configured"),
        "and the refusal is the registry's own, naming the namespace: {error}"
    );

    // The third shape: a backend that granted *something*, asked for something else.
    let mut wrong = client.token(&token_for("app"));
    wrong.name = "kv.get".to_string();
    let answer = client.call(&wrong);
    assert!(
        !answer.ok,
        "a granted backend is still limited to what it granted"
    );
    assert!(answer.error.expect("a reason").contains("kv"));
}

#[test]
fn a_frame_that_is_not_a_call_is_refused_by_name_rather_than_ignored() {
    // A worker channel carries a dozen frame types; this listener carries two. Ignoring the rest
    // would leave a caller waiting for an answer that is not coming — which on a CGI process is a
    // request hanging until PHP's own limit kills it, naming `max_execution_time` rather than the
    // mistake.
    let running = Running::start(grants(&[("app", &["echo"])]), "127.0.0.1:0");
    let mut client = Client::connect(&running.address);

    let payload = serde_json::json!({ "status": 200 });
    let payload = serde_json::to_vec(&payload).expect("encodes");
    let header = Header::new(MessageType::Response, u32::MAX, &payload).expect("a header");
    volucer_worker::write_frame(&mut client.stream, &header, &payload).expect("sent");

    let (header, payload) = Header::read_frame(&mut client.reader)
        .expect("a frame arrives")
        .expect("the server did not close");
    assert_eq!(header.message_type, MessageType::Return);
    let answer: Return = Json::decode(&payload).expect("decodes");
    assert!(!answer.ok);
    let error = answer.error.expect("a reason");
    assert!(
        error.contains("RESPONSE") && error.contains("CALL and PING"),
        "the refusal names the frame it got and the two it accepts: {error}"
    );
}

#[test]
fn a_ping_is_answered_without_counting_as_a_call() {
    // `PING` exists on this socket for the same reason it exists on a worker channel: it is the only
    // way a client can find out whether a connection it is holding is still good without making a
    // call that has side effects. Counting it as a call would make the summary's numbers a lie.
    let running = Running::start(grants(&[("app", &["echo"])]), "127.0.0.1:0");
    let mut client = Client::connect(&running.address);

    let header =
        Header::new(MessageType::Ping, volucer_worker::UNSOLICITED, &[]).expect("a header");
    volucer_worker::write_frame(&mut client.stream, &header, &[]).expect("sent");

    let (header, payload) = Header::read_frame(&mut client.reader)
        .expect("a frame arrives")
        .expect("the server did not close");
    assert_eq!(header.message_type, MessageType::Pong);
    assert!(payload.is_empty(), "a PONG carries nothing");
    assert_eq!(running.listener.stats().calls, 0, "a PING is not a call");
}

#[test]
fn an_idle_connection_is_closed_rather_than_held_for_ever() {
    // The default is five minutes, which is right for a deployment and too long for a test — so this
    // binds a listener with a one-second deadline and asserts the close. What it protects is a pool
    // of 32 `php-fpm` children holding 32 descriptors through a quiet night.
    let config = volucer_core::config::RpcConfig {
        listen: "127.0.0.1:0".to_string(),
        max_connections: None,
        idle_timeout_seconds: 1,
        call_timeout_seconds: 5,
    };
    let listener =
        Arc::new(RpcListener::bind(&config, grants(&[("app", &["echo"])])).expect("binds"));
    let address = listener.bound_address().expect("a port").to_string();
    let stop = listener.stop_handle();
    let running = Arc::clone(&listener);
    let thread = std::thread::spawn(move || running.run().expect("serves"));

    let mut client = Client::connect(&address);
    let mut call = client.token(&token_for("app"));
    call.name = "echo.ping".to_string();
    assert!(client.call(&call).ok, "the first call is served");

    // Well past the deadline, then a read that must fail: a closed socket reads zero bytes, which the
    // framing layer reports as `None` rather than as an error.
    std::thread::sleep(Duration::from_millis(1800));
    let frame = Header::read_frame(&mut client.reader).expect("a closed socket is not an error");
    assert!(
        frame.is_none(),
        "the server closed the idle connection rather than holding it"
    );

    stop.stop();
    let _ = thread.join();
}

#[test]
fn a_malformed_frame_ends_the_connection_without_taking_the_listener_with_it() {
    // One connection's garbage is one connection's problem. The failure this rules out is a panic in
    // a connection thread — which would poison nothing but would also serve nothing, and would take
    // the test process down with it.
    let running = Running::start(grants(&[("app", &["echo"])]), "127.0.0.1:0");

    let mut garbage = std::net::TcpStream::connect(&running.address).expect("connects");
    garbage
        .write_all(b"this is not a frame header, and it is long enough to be looked at")
        .expect("sent");
    drop(garbage);

    // The listener is still there.
    let mut client = Client::connect(&running.address);
    let mut call = client.token(&token_for("app"));
    call.name = "echo.ping".to_string();
    assert!(
        client.call(&call).ok,
        "a second connection is served after the first one sent nonsense"
    );
}

#[test]
fn the_token_is_a_function_of_the_backend_name_and_the_process_secret() {
    // The derivation is what makes `services` the one statement of a grant, so it has to be
    // *deterministic* — the server mints a token once and the same value has to come out of the same
    // inputs — and it has to distinguish names.
    let app = Token::derive(SECRET, "app");
    assert_eq!(
        app,
        Token::derive(SECRET, "app"),
        "the same inputs, the same token"
    );
    assert_ne!(
        app,
        Token::derive(SECRET, "api"),
        "different backends differ"
    );
    assert_ne!(
        app,
        Token::derive("another secret", "app"),
        "and a different process secret makes the previous deployment's tokens useless"
    );
    // 32 hex characters, so a token is a fixed-width field on the wire and in a log.
    assert_eq!(app.len(), 32);
    assert!(app.chars().all(|c| c.is_ascii_hexdigit()));
    // A name that differs in its last byte must not produce a token that differs in one nibble: that
    // is what the per-byte mix in the derivation is for, and it is worth pinning because a
    // hand-rolled hash is exactly where such a property silently stops holding.
    let mut differences = 0;
    let short = Token::derive(SECRET, "app");
    let long = Token::derive(SECRET, "apps");
    for (a, b) in short.chars().zip(long.chars()) {
        if a != b {
            differences += 1;
        }
    }
    assert!(
        differences >= 4,
        "a one-character change in the name changes the whole token: {differences} characters differ"
    );
}

#[test]
fn the_token_carries_no_constant_across_backends() {
    // **A regression test for a real defect.** The first version formatted one 64-bit word and
    // appended the *secret's length* as the second, so every backend in a process had a token ending
    // in the same sixteen hex characters — thirty-two characters of which half were a constant. It
    // passed every other assertion here, including the "one character changes the whole token" one,
    // because both of those only compare tokens that differ in their first half.
    let names = ["app", "api", "admin", "worker", "jobs"];
    let tokens = names
        .iter()
        .map(|name| Token::derive(SECRET, name))
        .collect::<Vec<_>>();

    // No two characters-position may be identical across all of them. A position that is the same
    // in every token is a position that carries nothing.
    let length = tokens[0].len();
    for position in 0..length {
        let first = tokens[0].chars().nth(position);
        let constant = tokens
            .iter()
            .all(|token| token.chars().nth(position) == first);
        assert!(
            !constant,
            "character {position} is {:?} in every token, so it carries no information: {tokens:?}",
            first
        );
    }

    // And the two halves are independent: the second half of one token must not be the second half of
    // another, which is the shape the defect took.
    let seconds = tokens
        .iter()
        .map(|token| token[16..].to_string())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        seconds.len(),
        names.len(),
        "every backend's second half is distinct: {tokens:?}"
    );
}
