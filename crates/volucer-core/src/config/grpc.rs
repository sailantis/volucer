//! gRPC call routing: the shape of a `:path`, and the allow-list that admits one.

use super::*;

/// A gRPC call's address, split out of the `:path` it arrived on.
///
/// gRPC's own routing rule is a string and nothing more: `:path` is
/// `/pkg.Service/Method`, and reading it needs no descriptor and no schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GrpcPath<'a> {
    /// The fully-qualified service, e.g. `greeter.Greeter`.
    pub service: &'a str,
    /// The method on that service, e.g. `SayHello`.
    pub method: &'a str,
}

/// Split a `:path` into the service and method a gRPC call addresses.
///
/// `None` for anything that is not a gRPC path, and the shape is deliberately strict rather than
/// forgiving, because the one thing a wrong answer here does is route an arbitrary URL into a
/// worker that believed it was reading a method name:
///
/// - exactly one `/` after the leading one — gRPC's own grammar has no room for a third segment,
///   so `/a.B/M/extra` is not a method and must not be treated as one;
/// - neither part empty, so `//` and `/Method` are refused;
/// - the service a legal protobuf full name ([`is_valid_grpc_service`]), because that is what it is;
/// - the method a legal protobuf identifier, for the same reason. A method that is not one is not
///   a method this server should be handing an application as a name.
///
/// A **query string is not stripped, and there is no special case for one**: gRPC addresses a method
/// by `:path` alone, so `/pkg.Service/Method?x=1` is not a path any gRPC client sends, and the `?` is
/// refused because it is not a byte a protobuf identifier may contain. That is deliberate rather than
/// incidental: a test for a query would be a second rule to keep in step with this one, and the
/// identifier rule already covers it. What the rule *does* need to catch is a separator hidden after
/// one — `?` then `/` — and the third-segment check above is what does that.
pub fn parse_grpc_path(path: &str) -> Option<GrpcPath<'_>> {
    let rest = path.strip_prefix('/')?;
    // `split_once` rather than `split('/').collect()`: a second `/` must be a refusal, not a third
    // field nobody reads. Checking the remainder for a separator is what makes that true.
    let (service, method) = rest.split_once('/')?;
    if method.contains('/') || method.is_empty() {
        return None;
    }
    if !is_valid_grpc_service(service) || !is_valid_proto_identifier(method) {
        return None;
    }
    Some(GrpcPath { service, method })
}

/// Whether a name is a legal protobuf **full service name**: identifiers joined by dots.
///
/// Used for two things, which is why it is one function: validating a `grpc_services` entry at
/// startup, and validating the service an incoming path names. If they were two rules, a
/// configuration could name a service no request could ever address — or, worse, the two could
/// disagree about a name that is *nearly* legal, which is precisely the class of typo the
/// allow-list exists to catch.
pub fn is_valid_grpc_service(name: &str) -> bool {
    !name.is_empty() && name.split('.').all(is_valid_proto_identifier)
}

/// Whether a name matches protobuf's `ident`: a letter or `_`, then letters, digits or `_`.
///
/// Case-sensitive, deliberately. Protobuf identifiers are, so a case-insensitive match here would
/// let `greeter.greeter` be granted by a list naming `greeter.Greeter` — and the two are different
/// services that could be routed to different backends.
fn is_valid_proto_identifier(name: &str) -> bool {
    let mut bytes = name.bytes();
    match bytes.next() {
        Some(first) => {
            (first.is_ascii_alphabetic() || first == b'_')
                && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        }
        None => false,
    }
}

impl Config {
    /// Whether a gRPC call to `path` may reach this backend's workers.
    ///
    /// The membership test a *deployment* asks; the server asks the same question of the list it
    /// captured at startup, through [`granted_grpc_call`], which is where the rule lives. This
    /// exists for `check` and for tests, where reading it out of the file is the point.
    pub fn serves_grpc(&self, backend: &str, path: &str) -> bool {
        let Some(BackendConfig::Worker(worker)) = self.backends.get(backend) else {
            return false;
        };
        granted_grpc_call(&worker.grpc_services, path).is_some()
    }
}

/// The gRPC call a `:path` addresses, when `services` grants it; `None` otherwise.
///
/// **The one implementation of the routing rule**, called from two places that must not disagree:
/// the server, against the list it captured when it built the backend, and the configuration, when
/// `check` reads the file. Two copies of "parse the path, then look the service up" is exactly the
/// drift the allow-list exists to prevent — one of them would eventually normalise a name the other
/// did not, and a service would be granted in one place and refused in the other.
pub fn granted_grpc_call<'a>(services: &[String], path: &'a str) -> Option<GrpcPath<'a>> {
    let call = parse_grpc_path(path)?;
    services
        .iter()
        .any(|granted| granted == call.service)
        .then_some(call)
}
