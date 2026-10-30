//! One backend, checked against the kind it declares.

use super::rules::is_dns_name;
use super::upstream::{
    default_upstream_ready_timeout_ms, default_upstream_restart_backoff_ms,
    default_upstream_restart_limit,
};
use super::*;

impl Config {
    pub(super) fn validate_backend(&self, name: &str, backend: &BackendConfig) -> Result<()> {
        let bad = |msg: String| Err(Error::Config(format!("backends.{name}: {msg}")));
        // **`request_timeout_ms = 0` is refused for every kind that has the key**, and checked
        // before the per-kind match so a new kind cannot inherit the key without the rule. The two
        // pools also refuse it at construction, which is the load-bearing check — `Spec` and
        // `CgiPool` are reachable from a `Config` that never went through `validate` — so this is
        // the one `volucer check` reports, by name, before anything is started.
        let timeout = match backend {
            BackendConfig::Worker(w) => w.request_timeout_ms,
            BackendConfig::SpawnCgi(c) => c.request_timeout_ms,
            BackendConfig::Attach(_) | BackendConfig::External(_) | BackendConfig::Upstream(_) => {
                None
            }
        };
        if timeout == Some(0) {
            return bad(
                "request_timeout_ms = 0 would give a request no deadline at all, so one hung \
                 request could hold a slot for ever; write the number of milliseconds you mean \
                 instead"
                    .to_string(),
            );
        }
        match backend {
            BackendConfig::Worker(w) => {
                if w.command.trim().is_empty() {
                    return bad("worker command must not be empty".to_string());
                }
                crate::env::check_env_table(&format!("backends.{name}.env"), &w.env)
                    .map_err(Error::Config)?;
                // Checked by name against the allow-list, so a typo is refused here rather than
                // becoming a worker that boots, serves traffic, and is silently unable to call
                // anything. A duplicate is refused for the same reason a duplicate key would be:
                // it means two lines disagree and only one of them is read.
                validate_services(self, name, "worker", &w.services)?;

                // The gRPC side of the same rule, in the other direction: a name here decides what
                // may be *delivered* to this backend's workers. Checked against protobuf's own
                // grammar rather than against a table, because there is no table to check against —
                // the services live in the application, not in this file, and that is the whole
                // point.
                //
                // A name that is not a legal service is refused rather than warned about, and the
                // failure mode it prevents is the quiet one: `grpc_services = ["Greeter"]` beside a
                // client calling `/greeter.Greeter/SayHello` is a worker granted nothing, so every
                // call is answered `UNIMPLEMENTED` and no log line says why. That is the same silent
                // shape the `services` check exists for.
                let mut seen_service = std::collections::BTreeSet::new();
                for service in &w.grpc_services {
                    if !is_valid_grpc_service(service) {
                        return bad(format!(
                            "grpc_services contains {service:?}, which is not a protobuf service \
                             name; a service is identifiers joined by dots, e.g. \
                             \"greeter.Greeter\" — check the name in the application's `.proto`, \
                             with its package"
                        ));
                    }
                    if !seen_service.insert(service) {
                        return bad(format!("grpc_services names {service:?} twice"));
                    }
                }

                // `grpc_serve_any` beside a list is two statements about one thing, and each of the
                // two readings is wrong: honouring the list silently ignores the opt-out, and
                // honouring the opt-out silently ignores the list an operator wrote for a reason.
                // The same shape as `header` beside a key mode that does not read one.
                if w.grpc_serve_any && !w.grpc_services.is_empty() {
                    return bad(
                        "grpc_serve_any is set beside a non-empty grpc_services; they are two \
                         statements about what this backend may be asked to serve. Remove the \
                         list, or remove grpc_serve_any to keep the allow-list"
                            .to_string(),
                    );
                }

                // `grpc_proto` is the third statement about the same set, so it is refused beside
                // each of the other two for their reason: whatever the resolution order, one of the
                // three would be ignored, and an operator who wrote two of them wrote one for
                // nothing. This is the *only* rule about `grpc_proto` that can be checked without a
                // disk; what the files contain is read by `expand_grpc_proto`, when the
                // configuration is loaded.
                if !w.grpc_proto.is_empty() {
                    if !w.grpc_services.is_empty() {
                        return bad(
                            "grpc_proto is set beside a non-empty grpc_services; both name the \
                             services this backend may be asked to serve, and only one of them can \
                             be the answer. Remove grpc_services to let the `.proto` files decide, \
                             or remove grpc_proto to keep the list"
                                .to_string(),
                        );
                    }
                    if w.grpc_serve_any {
                        return bad(
                            "grpc_proto is set beside grpc_serve_any; serving every service makes \
                             reading the `.proto` files pointless. Remove one of them"
                                .to_string(),
                        );
                    }
                    if let Some(empty) =
                        w.grpc_proto.iter().find(|path| path.as_os_str().is_empty())
                    {
                        return bad(format!(
                            "grpc_proto contains {empty:?}, which names no file; give a `.proto` \
                             path or a directory, relative to the configuration file"
                        ));
                    }
                }

                // Where the server listens, when it is not a pipe. Checked here rather than
                // discovered at bind time for the reason every other address is: a typo is a
                // startup message naming the key, not a bind error after the pool half exists.
                if let Some(socket) = &w.socket {
                    validate_worker_socket(socket, w.transport)
                        .map_err(|err| Error::Config(format!("backends.{name}.socket: {err}")))?;
                }
            }
            BackendConfig::External(e) => {
                parse_listener(&e.socket)
                    .map_err(|err| Error::Config(format!("backends.{name}.socket: {err}")))?;
                // The same allow-list a worker's channel is checked against, because `[rpc]` is the
                // same call plane reached a different way: an `external` backend's application
                // dials the listener and asks for a namespace by name, and a name this file cannot
                // honour is a call that would be refused at runtime with no line saying why.
                validate_services(self, name, "external", &e.services)?;
                // `0` is refused rather than read as "unlimited". `max_connections` is an
                // `Option`, so absent is how "no cap" is spelled; a zero would otherwise be a
                // backend that can never serve anything, and it would be discovered on the first
                // request instead of here.
                if e.max_connections == Some(0) {
                    return bad(format!(
                        "backends.{name}.max_connections is 0, so no request could ever be \
                         served; remove the key to leave the backend uncapped, which is the default"
                    ));
                }
            }
            BackendConfig::Attach(a) => {
                // Everything checkable is checkable here: a relay address is a path or an
                // address, and a connection count of zero is a pool with nothing in it. The
                // relay's *existence* is not checked, deliberately — it is created by a
                // supervisor volucer does not own, and refusing to start before it appears
                // would make the two processes' start order a configuration requirement.
                // `preflight` warns about a missing path instead.
                a.validate(name).map_err(Error::Config)?;
                // **This was missing.** An `attach` backend has a `services` list and its registry is
                // built from it, but nothing validated the names — so `services = ["workes"]` on the
                // one kind whose workers do get a channel passed `check` and granted nothing at all,
                // which is the quiet failure this allow-list exists to prevent. `attach` owns a pool,
                // like `worker`, so `workers.reset` is grantable here.
                validate_services(self, name, "attach", &a.services)?;
            }
            BackendConfig::SpawnCgi(c) => {
                // One call, because the rules are about this table alone: which driver is in
                // use, whether `children`/`concurrency` describe the same number twice, and
                // whether a `socket` is a shape the driver can bind. Kept on the type rather
                // than here so `spawn_cgi`'s rules are not split across two files.
                c.validate(name).map_err(Error::Config)?;
                validate_services(self, name, "spawn_cgi", &c.services)?;
            }
            BackendConfig::Upstream(u) => {
                crate::env::check_env_table(&format!("backends.{name}.env"), &u.env)
                    .map_err(Error::Config)?;
                validate_services(self, name, "upstream", &u.services)?;
                // Parsing is also the validation of the `[backends.<n>.tls]` table: the origin and
                // the table are only meaningful together, so `OriginSpec::parse` takes the table and
                // refuses one that has no handshake to describe.
                let origin = OriginSpec::parse_with(&u.origin, u.tls.clone(), u.protocol)
                    .map_err(|reason| Error::Config(format!("backends.{name}.origin: {reason}")))?;
                // The scheme decides whether a `tls` table belongs at all, and the parse above has
                // already refused the combination that does not. `origin` is kept in scope rather
                // than discarded so the rule is readable here: `sni` and `ca` are only reachable
                // for a `https:` origin.

                // `protocol` beside a handshake is refused, because there the answer is ALPN's and a
                // claim of prior knowledge would be a second, worse answer: it would be wrong
                // whenever the origin offers `http/1.1` only, and the client would send the h2
                // preface to an HTTP/1.1 peer.
                if u.protocol.is_some() && origin.is_tls() {
                    return bad(format!(
                        "backends.{name}.protocol is set beside an `https://` origin, where the \
                         protocol is negotiated by ALPN during the handshake; remove the key to let \
                         the handshake decide, which is also what makes an HTTP/1.1-only origin work"
                    ));
                }

                // The supervisor's three knobs, checked here because every one of them is a
                // setting whose wrong value is silent rather than loud at runtime.
                if let Some(command) = &u.command {
                    if command.trim().is_empty() {
                        return bad(format!(
                            "backends.{name}.command is empty; remove the key to let this origin be \
                             somebody else's process, or name the program that serves it"
                        ));
                    }
                    if u.restart == Some(UpstreamRestart::Never) && u.restart_limit > 0 {
                        // Not contradictory — `never` wins — but the pair says two different things,
                        // which is how an operator comes to believe a limit is in force when no
                        // restart will ever happen.
                        return bad(format!(
                            "backends.{name} sets restart = \"never\" and restart_limit = {}, which \
                             cannot both be meant; remove restart_limit or say restart = \"on_failure\"",
                            u.restart_limit
                        ));
                    }
                    if u.restart_limit > 0 && u.restart_backoff_ms == 0 {
                        // A zero backoff with a limit above 1 is a restart loop that looks bounded
                        // and is not: `restart_limit` counts *consecutive* failures, and a command
                        // that exits instantly with no delay between attempts is a spin for as long
                        // as the limit is high.
                        return bad(format!(
                            "backends.{name}.restart_backoff_ms is 0, so {} restarts of an origin \
                             that exits immediately would run back to back with nothing between them",
                            u.restart_limit
                        ));
                    }
                    if u.restart_backoff_ms > UPSTREAM_RESTART_BACKOFF_CAP_MS {
                        // Refused rather than silently capped, so a deployment that wrote an hour
                        // learns that it is an hour *because this limit exists* rather than
                        // discovering a minute at runtime.
                        return bad(format!(
                            "backends.{name}.restart_backoff_ms is {}, above the {} ms cap; the \
                             delay doubles per failure, and a first delay longer than the cap would \
                             be reduced by the doubling rather than grow from it",
                            u.restart_backoff_ms, UPSTREAM_RESTART_BACKOFF_CAP_MS
                        ));
                    }
                    if u.ready_timeout_ms == 0 {
                        // The readiness gate is the whole point of supervising an origin here —
                        // see `UpstreamConfig::ready_timeout_ms`. Zero would mean "do not wait",
                        // which is the 502-on-first-request this key exists to prevent.
                        return bad(format!(
                            "backends.{name}.ready_timeout_ms is 0, so startup would not wait for \
                             the origin to answer at all; that is the 502 the readiness gate exists \
                             to prevent"
                        ));
                    }
                } else {
                    // The three supervisor keys without a `command` describe a process nobody
                    // starts. Each would be silently ignored, and the operator would believe they
                    // had configured a supervisor.
                    let mut orphans: Vec<&str> = Vec::new();
                    if u.working_dir.is_some() {
                        orphans.push("working_dir");
                    }
                    if !u.env.is_empty() {
                        orphans.push("env");
                    }
                    if u.restart.is_some() {
                        orphans.push("restart");
                    }
                    if u.restart_limit != default_upstream_restart_limit() {
                        orphans.push("restart_limit");
                    }
                    if u.restart_backoff_ms != default_upstream_restart_backoff_ms() {
                        orphans.push("restart_backoff_ms");
                    }
                    if u.ready_timeout_ms != default_upstream_ready_timeout_ms() {
                        orphans.push("ready_timeout_ms");
                    }
                    if !orphans.is_empty() {
                        return bad(format!(
                            "backends.{name} sets {} but no `command`, so there is no process for \
                             them to describe; add the command that starts the origin, or remove \
                             them",
                            orphans.join(", ")
                        ));
                    }
                }

                if let Some(tls) = origin.tls() {
                    if let Some(ca) = &tls.ca {
                        if ca.as_os_str().is_empty() {
                            return bad(format!(
                                "backends.{name}.tls.ca is empty; name the PEM file, or remove \
                                 the key to use the public roots"
                            ));
                        }
                    }
                    if let Some(sni) = &tls.sni {
                        // The verification name is checked at startup, not at connect time: a typo
                        // in `sni` is a configuration mistake, and a configuration mistake belongs
                        // where the file is read rather than in a 502 on the first request.
                        let sni = sni.trim();
                        if sni.is_empty() {
                            return bad(format!(
                                "backends.{name}.tls.sni is empty; name the host the certificate \
                                 carries, or remove the key to verify against the origin's own host"
                            ));
                        }
                        // `host:port` and a trailing dot are both refused by this, which is right:
                        // a verification name is a DNS name, and a port is not part of one.
                        if sni.parse::<std::net::IpAddr>().is_err() && !is_dns_name(sni) {
                            return bad(format!(
                                "backends.{name}.tls.sni {sni:?} is not a host name or IP address"
                            ));
                        }
                    }
                }
            }
        }
        Ok(())
    }
}
