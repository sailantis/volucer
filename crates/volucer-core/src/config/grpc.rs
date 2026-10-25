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

/// The gRPC services one backend may be asked to serve.
///
/// **The routing rule and the grant it is checked against are one value**, because they have to move
/// together. This used to be a bare `Vec<String>` captured into a worker pool at startup, so a reload
/// could not change it and the pool had to be rebuilt to widen a grant — for a list that never
/// reaches a worker and is only ever a membership test on the request path.
///
/// Two shapes rather than one because the second is the one an operator has to *write down*:
///
/// - [`GrpcGrants::none`] answers `UNIMPLEMENTED` to every call. It is the default, and the reason
///   the list exists at all: a backend that names nothing is granted nothing, including a plain PHP
///   application a misconfigured client happens to send `content-type: application/grpc` to.
/// - [`GrpcGrants::any`] admits every well-formed `/pkg.Service/Method`. It is what
///   `grpc_serve_any = true` buys, and it is deliberately **not** spelled as a `"*"` entry in the
///   list: `*` is not a protobuf service name, so a sentinel there would have to be exempted from
///   [`is_valid_grpc_service`] and would then be a name that looks like a service and is not one.
///
/// The list form is the ordinary one, and it is a *security* boundary rather than a routing
/// convenience: see [`Config::serves_grpc`] and the listener's refusal.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GrpcGrants {
    /// The protobuf service names this backend was granted, in configuration order.
    services: Vec<String>,
    /// Whether every service is granted, which is `grpc_serve_any = true`.
    any: bool,
}

impl GrpcGrants {
    /// A backend that may be asked to serve nothing.
    pub fn none() -> Self {
        Self::default()
    }

    /// A backend that may be asked to serve any well-formed service name.
    pub fn any() -> Self {
        GrpcGrants {
            services: Vec::new(),
            any: true,
        }
    }

    /// The grants a backend's table asks for.
    ///
    /// `serve_any` beside a non-empty list is refused by validation rather than resolved here: two
    /// statements about one thing that can disagree is the shape this crate refuses everywhere else,
    /// and a silent precedence rule would make the file mean something it does not say.
    pub fn from_config(services: &[String], serve_any: bool) -> Self {
        if serve_any {
            return Self::any();
        }
        GrpcGrants {
            services: services.to_vec(),
            any: false,
        }
    }

    /// Whether every service is granted.
    pub fn is_any(&self) -> bool {
        self.any
    }

    /// The service names this backend was granted, empty when [`GrpcGrants::is_any`].
    pub fn names(&self) -> &[String] {
        &self.services
    }

    /// Whether this grants nothing, which is the default and the safe answer.
    pub fn is_empty(&self) -> bool {
        !self.any && self.services.is_empty()
    }

    /// The gRPC call a `:path` addresses, when these grants admit it; `None` otherwise.
    ///
    /// **The one implementation of the routing rule**, called from the listener per request and from
    /// `check` when it reads the file. Two copies of "parse the path, then look the service up" is
    /// exactly the drift the allow-list exists to prevent — one of them would eventually normalise a
    /// name the other did not, and a service would be granted in one place and refused in the other.
    ///
    /// A path that is not `/pkg.Service/Method` is `None` **even under [`GrpcGrants::any`]**, because
    /// the shape check is what stops an arbitrary URL becoming a method name a worker reads.
    pub fn granted<'a>(&self, path: &'a str) -> Option<GrpcPath<'a>> {
        let call = parse_grpc_path(path)?;
        if self.any || self.services.iter().any(|granted| granted == call.service) {
            return Some(call);
        }
        None
    }
}

impl Config {
    /// The gRPC grants a backend's table asks for, or `None` for a kind that cannot serve gRPC.
    ///
    /// Only `worker` and `attach` have the keys: an `external` or `spawn_cgi` backend is a CGI
    /// process with no channel the server could hand method bytes to, and an `upstream` speaks gRPC
    /// itself and needs no permission from here.
    ///
    /// **The three sources are resolved here and nowhere else**, which is what keeps them from
    /// drifting: `grpc_serve_any` is the opt-out, `grpc_services` is what the file says, and
    /// [`WorkerConfig::grpc_proto_services`](crate::config::WorkerConfig::grpc_proto_services) is
    /// what the `.proto` files said. Validation refuses any two of them together, so the match below
    /// has at most one non-empty branch to choose between — it is a read of three disjoint fields,
    /// not a precedence rule.
    pub fn grpc_grants(&self, backend: &str) -> Option<GrpcGrants> {
        let (services, serve_any, from_proto) = match self.backends.get(backend)? {
            BackendConfig::Worker(worker) => (
                worker.grpc_services.as_slice(),
                worker.grpc_serve_any,
                worker.grpc_proto_services.as_slice(),
            ),
            BackendConfig::Attach(attach) => (
                attach.grpc_services.as_slice(),
                attach.grpc_serve_any,
                attach.grpc_proto_services.as_slice(),
            ),
            BackendConfig::External(_)
            | BackendConfig::SpawnCgi(_)
            | BackendConfig::Upstream(_) => return None,
        };
        // The proto half first, because it is the one that is *not* in the file: an entry that has
        // not been expanded yet (a configuration built by hand, in a test) grants nothing, which is
        // the same answer an absent list gives and the safe one for a reload that never ran.
        if !from_proto.is_empty() {
            return Some(GrpcGrants::from_config(from_proto, false));
        }
        Some(GrpcGrants::from_config(services, serve_any))
    }

    /// Every backend's grants, which is what a front-end serves from.
    ///
    /// Built once per configuration and carried through the reload, so the answer a request gets is
    /// the one its own snapshot holds rather than one read out of a live configuration.
    pub fn grpc_grants_all(&self) -> BTreeMap<String, GrpcGrants> {
        self.backends
            .keys()
            .filter_map(|name| self.grpc_grants(name).map(|grants| (name.clone(), grants)))
            .collect()
    }

    /// Whether a gRPC call to `path` may reach this backend's workers.
    ///
    /// The membership test a *deployment* asks, for `check` and for tests, where reading the answer
    /// out of the file is the point. The listener asks [`GrpcGrants::granted`] of the grants its own
    /// snapshot carries, which is the same rule against a different generation.
    pub fn serves_grpc(&self, backend: &str, path: &str) -> bool {
        self.grpc_grants(backend)
            .is_some_and(|grants| grants.granted(path).is_some())
    }
}
