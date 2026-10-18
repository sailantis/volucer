//! Errors from the HTTP front-end.

/// A failure that happens before a response can be produced.
///
/// The variants exist so the listener can choose a status code without inspecting
/// strings. Everything that reaches the client as an error page is decided by
/// [`HttpError::status`].
#[derive(Debug, thiserror::Error)]
pub enum HttpError {
    /// The request could not be read or understood.
    #[error("bad request: {0}")]
    BadRequest(String),

    /// The request body was larger than the configured limit.
    ///
    /// Separate from [`HttpError::BadRequest`] because the status differs: this is
    /// a 413, not a 400, and a client that sent a large upload should be told
    /// exactly that.
    #[error("the request body exceeds the {limit} byte limit")]
    BodyTooLarge { limit: usize },

    /// No script matched the request, and no front controller is configured.
    ///
    /// The CGI layer produces this as a "bad request", but for HTTP a request for
    /// a path that names no script is a 404 — the client asked for something that
    /// does not exist, not for something malformed.
    #[error("not found: {0}")]
    NotFound(String),

    /// The path names something the configuration forbids.
    #[error("forbidden: {0}")]
    Forbidden(String),

    /// The backend could not be reached.
    #[error("bad gateway: {0}")]
    BadGateway(String),

    /// The backend was reached but did not answer in time.
    #[error("gateway timeout: {0}")]
    GatewayTimeout(String),

    /// The server is at its connection limit.
    #[error("too many connections")]
    Overloaded,

    /// Binding the listener failed.
    #[error("cannot bind {address}: {source}")]
    Bind {
        address: String,
        #[source]
        source: std::io::Error,
    },

    /// The `tokio` runtime could not be created.
    #[error("cannot start the async runtime: {0}")]
    Runtime(#[source] std::io::Error),

    /// An accepted listener could not be handed to the runtime.
    ///
    /// Distinct from [`HttpError::Bind`], which is about *taking* an address: this one happens after
    /// the port is held, when converting a bound `std::net::TcpListener` into its `tokio` equivalent
    /// fails. The two want different sentences in a log — one says "this address is not yours", the
    /// other says "this process is out of descriptors" — and merging them would make a resource
    /// exhaustion read as a configuration mistake.
    #[error("the listener could not be served: {0}")]
    Io(#[source] std::io::Error),

    /// A certificate or key could not be loaded, or they do not pair.
    ///
    /// Reported before the port is bound, because a listener that starts and then fails every
    /// handshake is worse than one that refuses to start: the failure belongs to the operator
    /// reading the log, not to every client that connects. Failing *before the bind* is the
    /// stronger half of that: a listener that binds and then discovers its certificate is bad
    /// has already taken the port, so a service manager restarting on failure takes and releases
    /// it in a loop.
    #[error("TLS is configured but unusable: {reason}")]
    Tls { reason: String },

    /// A gRPC call failed, and gRPC's answer is HTTP **200** with a status in the trailers.
    ///
    /// Its own variant rather than a status code on another one, because the reason this server
    /// cannot answer a gRPC failure the way it answers every other failure is a *protocol* fact and
    /// not a choice: an HTTP error status is unreadable to a gRPC client, which reads the outcome
    /// from the trailer section and has no status code to branch on otherwise. A proxy that answers
    /// `502` to a gRPC call has thrown away the failure it is trying to report.
    ///
    /// Carries the real reason as **text** rather than as another `HttpError`, and that is what makes
    /// it usable from both places a gRPC failure is born: a dispatch that failed (which has a
    /// `ServerError` and, through [`HttpError`], an HTTP status it would have used) and a call the
    /// server refused before dispatching (which has no `ServerError` at all, because nothing was
    /// asked of a backend). One of those two has only a sentence, so a sentence is what the variant
    /// holds.
    ///
    /// The sentence goes to the **log**, not to the wire: `grpc-message` is percent-encoded ASCII and
    /// this is arbitrary text. The wire carries a fixed message a client could act on; the operator
    /// reads the log, where the reason is written in full.
    #[error("the gRPC call failed: {reason}")]
    Grpc {
        /// The status the client is told, in the trailers.
        status: volucer_upstream::GrpcStatus,
        /// Why, in full, for the log.
        reason: String,
    },

    /// The caller has spent its allowance: a `429`, with a `Retry-After`.
    ///
    /// Its own variant rather than a [`HttpError::Overloaded`], and the distinction is the point:
    /// `Overloaded` is a 503, which a load balancer reads as "this node is unhealthy" and acts on
    /// by taking it out of rotation. A rate limit is a statement about *the caller*, not about the
    /// server — the process is healthy and will serve the next request, from somebody else,
    /// immediately. Answering 503 would turn one misbehaving client into an outage.
    ///
    /// `Retry-After` is carried on the variant rather than recomputed by the response builder,
    /// because it is the limiter's own arithmetic: the number the client is told has to be the
    /// number the bucket will actually honour.
    ///
    /// **A gRPC call is refused differently and by the same mechanism.** `error_response` turns
    /// this variant into `RESOURCE_EXHAUSTED` in the trailers for a gRPC request, because a gRPC
    /// client cannot read a 429. Both are "wait and try again", which is what the code means.
    #[error("too many requests: the {key} rate limit refused this request")]
    RateLimited {
        /// The key mode that refused, for the page and the log.
        key: volucer_core::KeyMode,
        /// When the caller may try again.
        retry_after: std::time::Duration,
    },
}

impl HttpError {
    /// The status code that should be sent for this failure.
    pub fn status(&self) -> u16 {
        match self {
            HttpError::BadRequest(_) => 400,
            HttpError::Forbidden(_) => 403,
            HttpError::NotFound(_) => 404,
            HttpError::BodyTooLarge { .. } => 413,
            HttpError::BadGateway(_) => 502,
            HttpError::Overloaded => 503,
            HttpError::GatewayTimeout(_) => 504,
            // **429, not 503**, and the distinction is the whole reason this variant exists: a
            // 503 takes the node out of a load balancer's rotation, which is an outage caused by
            // one client. "Too Many Requests" says exactly what happened and what to do.
            HttpError::RateLimited { .. } => 429,
            // None of these reaches a client: they stop the listener before it accepts anything, or
            // they are the listener failing to be served at all. Reported as a 500 rather than
            // panicking.
            HttpError::Bind { .. }
            | HttpError::Runtime(_)
            | HttpError::Io(_)
            | HttpError::Tls { .. } => 500,
            // **200**, and this is the one variant whose status is not an error status at all.
            // gRPC reports every outcome in the trailer section, so the failure travels as
            // `grpc-status` and the HTTP status is a plain success. See `grpc_failure`.
            HttpError::Grpc { .. } => 200,
        }
    }

    /// The reason phrase for this failure's status.
    pub fn reason(&self) -> &'static str {
        match self.status() {
            400 => "Bad Request",
            403 => "Forbidden",
            404 => "Not Found",
            413 => "Content Too Large",
            429 => "Too Many Requests",
            502 => "Bad Gateway",
            503 => "Service Unavailable",
            504 => "Gateway Timeout",
            // A gRPC failure is a 200, so it takes the success phrase rather than an error one.
            200 => "OK",
            _ => "Internal Server Error",
        }
    }
}

/// Convenience alias.
pub type Result<T> = std::result::Result<T, HttpError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_failure_has_the_status_a_client_needs() {
        assert_eq!(HttpError::BadRequest("x".into()).status(), 400);
        assert_eq!(HttpError::Forbidden("x".into()).status(), 403);
        assert_eq!(HttpError::NotFound("x".into()).status(), 404);
        assert_eq!(HttpError::BodyTooLarge { limit: 1 }.status(), 413);
        assert_eq!(HttpError::BadGateway("x".into()).status(), 502);
        assert_eq!(HttpError::Overloaded.status(), 503);
        assert_eq!(HttpError::GatewayTimeout("x".into()).status(), 504);
    }

    #[test]
    fn a_rate_limit_is_a_429_and_not_the_503_a_full_pool_gets() {
        // The distinction this variant exists for. `Overloaded` is a 503 because the *server* has
        // no capacity; a rate limit is a 429 because the *caller* has none. A load balancer acts on
        // the two differently — it takes a node out of rotation for the first — so collapsing them
        // turns one client's abuse into an outage.
        let limited = HttpError::RateLimited {
            key: volucer_core::KeyMode::Ip,
            retry_after: std::time::Duration::from_secs(7),
        };
        assert_eq!(limited.status(), 429);
        assert_eq!(limited.reason(), "Too Many Requests");
        assert!(limited.to_string().contains("ip"), "{limited}");
        assert_ne!(
            limited.status(),
            HttpError::Overloaded.status(),
            "these two must never share a status"
        );
    }

    #[test]
    fn a_missing_script_is_a_404_not_a_500() {
        // The distinction the front-end exists to make: the CGI layer calls this
        // a bad request, because there it is a protocol-level refusal. To an HTTP
        // client it is simply a path that does not exist.
        let missing = HttpError::NotFound("/nope".to_string());
        assert_eq!(missing.status(), 404);
        assert_eq!(missing.reason(), "Not Found");
        assert!(missing.to_string().contains("/nope"), "{missing}");
    }
}
