//! `Config::validate`: the rules that no single table can answer on its own.
//!
//! The per-family halves are the `validate_*` modules beside this one, so a rule and the keys it reads
//! are always in the same place. What is left here is the order they run in.

use super::rules::{
    is_header_name, is_header_name_byte, is_metric_name, is_metric_name_prefix, validate_route,
};
use super::services::default_kv_max_bytes;
#[cfg(feature = "redis")]
use super::services::validate_redis;
use super::*;

impl Config {
    /// Reject configurations that cannot work, with actionable messages.
    pub fn validate(&self) -> Result<()> {
        if self.server.http.is_none()
            && self.server.fastcgi.is_none()
            && self.server.https.is_none()
        {
            return Err(Error::Config(
                "no listener configured: set server.http, server.https and/or server.fastcgi"
                    .to_string(),
            ));
        }

        if let Some(http) = &self.server.http {
            parse_listener(http).map_err(|e| Error::Config(format!("server.http: {e}")))?;
        }
        if let Some(fastcgi) = &self.server.fastcgi {
            parse_listener(fastcgi).map_err(|e| Error::Config(format!("server.fastcgi: {e}")))?;
        }
        if let Some(https) = &self.server.https {
            parse_listener(https).map_err(|e| Error::Config(format!("server.https: {e}")))?;
        }

        // The proxy trust list is *validated*, not merely deserialized, and this is the reason it is
        // a method call: `trusted_subnets` is a security boundary where a typo has a direction —
        // a range that fails to parse must stop the server rather than quietly not be trusted or,
        // worse, be trusted in a wider form than written.
        self.server.client_ip().map_err(Error::Config)?;

        // The server's shared env baseline is checked here rather than when a process is spawned:
        // `Command::env` accepts anything and only fails at spawn, with an `InvalidInput` error that
        // names no key — a startup failure an operator cannot trace back to a line in their file.
        crate::env::check_env_table("server.env", &self.server.env).map_err(Error::Config)?;

        // The metrics and status listeners are validated here even though they are not `server.*`,
        // because a *typo* in an address is a configuration mistake and belongs in the same message
        // as every other one — `volucer check` reports it before anything tries to bind, rather than
        // the server failing to start with a bind error after the worker pools are already up.
        //
        // Note that neither counts towards the "no listener configured" refusal above: a deployment
        // with only `[metrics]` would be a process that serves nothing and reports on it.
        if let Some(metrics) = &self.metrics {
            parse_listener(&metrics.address)
                .map_err(|e| Error::Config(format!("metrics.address: {e}")))?;
            if !metrics.path.starts_with('/') {
                return Err(Error::Config(format!(
                    "metrics.path {:?} must start with '/', e.g. \"/metrics\"",
                    metrics.path
                )));
            }
            if let Some(prefix) = &metrics.prefix {
                if !is_metric_name_prefix(prefix) {
                    return Err(Error::Config(format!(
                        "metrics.prefix {prefix:?} is not a valid metric name prefix; letters, \
                         digits and underscores only, e.g. \"myapp_\""
                    )));
                }
            }
            // A collected name is validated as a *whole* metric name rather than as a prefix, and the
            // two rules are not the same: a prefix may be empty and may not begin with a digit, while
            // a name may do neither. A name that broke the rule would still be registered and still
            // render — as a line Prometheus cannot parse, which drops the whole family and every
            // family after it. That is silent, so it is refused here instead.
            for (name, metric) in &metrics.collect {
                if !is_metric_name(name) {
                    return Err(Error::Config(format!(
                        "metrics.collect.{name}: {name:?} is not a valid metric name; it must match \
                         [a-zA-Z_][a-zA-Z0-9_]*, e.g. \"app_orders_total\""
                    )));
                }
                // The `rr_` namespace is this server's own. An application metric landing there
                // would collide with a metric an operator's dashboard already has, or silently
                // replace it, and the failure would be a graph that means two different things over
                // time. Refused rather than prefixed, because a prefix would make the name in the
                // application's `emitMetric` and the name in the graph differ, which is a worse
                // surprise than an error at startup.
                if name.starts_with("rr_") {
                    return Err(Error::Config(format!(
                        "metrics.collect.{name}: names beginning with \"rr_\" are this server's own \
                         metrics, so an application metric may not claim one; pick another name"
                    )));
                }
                if metric.help.trim().is_empty() {
                    return Err(Error::Config(format!(
                        "metrics.collect.{name}: `help` must not be empty; it is the line a reader \
                         sees six months from now, and a metric without one is a bare name"
                    )));
                }
                // Buckets on a kind that has no buckets is a setting read by nothing, which is the
                // failure mode `spawn_fpm` was removed for: it looks supported and cannot work.
                if !metric.buckets.is_empty() && metric.kind != CollectedKind::Histogram {
                    return Err(Error::Config(format!(
                        "metrics.collect.{name}: `buckets` is only meaningful for a histogram, and \
                         this one is declared as a {:?}; remove the key or change the type",
                        metric.kind
                    )));
                }
                // A bucket that is not a number, or a negative one, is a histogram whose `le` label
                // would be unparseable. Non-finite is checked as well because TOML lets `inf` and
                // `nan` be written and neither is a bound.
                if let Some(bad) = metric.buckets.iter().find(|b| !b.is_finite() || **b < 0.0) {
                    return Err(Error::Config(format!(
                        "metrics.collect.{name}: the bucket bound {bad} is not a finite, \
                         non-negative number"
                    )));
                }
            }
        }
        if let Some(status) = &self.status {
            parse_listener(&status.address)
                .map_err(|e| Error::Config(format!("status.address: {e}")))?;
            if !(100..=599).contains(&status.unavailable_status_code) {
                return Err(Error::Config(format!(
                    "status.unavailable_status_code {} is not an HTTP status code",
                    status.unavailable_status_code
                )));
            }
            // The mutation routes recycle a worker, so they are worth a refusal rather than a
            // warning on the one address that makes them reachable from anywhere. A loopback
            // address is not proof of safety — a container's loopback is the container's — but a
            // wildcard bind is proof of the opposite, and an operator who wrote one while setting
            // this flag has almost certainly not meant to. The hint names the fix rather than
            // restating the rule.
            if status.allow_worker_reset {
                let listener = parse_listener(&status.address)
                    .map_err(|e| Error::Config(format!("status.address: {e}")))?;
                let broad = match &listener {
                    Listener::Tcp(address) => address.ip().is_unspecified(),
                    Listener::Unix(_) => false,
                };
                if broad {
                    return Err(Error::Config(format!(
                        "status.allow_worker_reset is set and status.address is {}, which binds \
                         every interface; POST /workers/{{index}}/reset recycles a worker and has \
                         no authentication, so give [status] a loopback or private address and \
                         reach it through the orchestrator's own network",
                        status.address
                    )));
                }
            }
        }

        // The trace exporter, checked here for the same reason the listeners above are: a bad
        // endpoint or a zero batch size is a configuration mistake, and `volucer check` should name
        // it rather than a background thread discovering it on the first export — where the failure
        // is a dropped batch rather than a message an operator ever reads.
        if let Some(telemetry) = &self.telemetry {
            // Parsed as an origin, not as a listener, because this is a *client* address: the
            // exporter dials it. `parse_listener` accepts a bare `host:port` and a `unix:` path,
            // both of which would be a configuration that parses and cannot be dialled as HTTP.
            //
            // Parsed through `base_endpoint`, which is the *same* normalisation the exporter uses —
            // so a value this accepts is exactly a value the exporter can dial. Checking the raw
            // string here while the exporter trimmed it is what made the first version reject a
            // complete `/v1/traces` URL that the documentation promised to accept.
            let base = telemetry.base_endpoint();
            let spec = OriginSpec::parse(base, None).map_err(|reason| {
                Error::Config(format!(
                    "telemetry.endpoint {:?} is not a usable OTLP/HTTP endpoint: {reason}",
                    telemetry.endpoint
                ))
            })?;
            // A unix socket *is* dialable by the client, but the exporter is not given a path
            // through this configuration — and accepting one that then dialled TCP would be the
            // silent substitution this project's `bind_host` documentation already refuses.
            if !matches!(spec, OriginSpec::Network { .. }) {
                return Err(Error::Config(format!(
                    "telemetry.endpoint {:?} is a unix socket; the trace exporter takes an \
                     http:// or https:// address, e.g. \"http://127.0.0.1:4318\"",
                    telemetry.endpoint
                )));
            }
            // Zero is refused rather than read as "no deadline". A request with no deadline against
            // a collector that accepts and never answers holds the exporter's thread for ever, and
            // an operator who wrote `0` meant "no limit" — which is the one thing this must not do.
            if telemetry.timeout_ms == 0 {
                return Err(Error::Config(
                    "telemetry.timeout_ms is 0; every export would be abandoned the moment it was \
                     sent. Give a deadline in milliseconds, e.g. 10000"
                        .to_string(),
                ));
            }
            if telemetry.batch_size == 0 {
                return Err(Error::Config(
                    "telemetry.batch_size is 0, so a batch could never be filled and no span would \
                     ever be sent. Use at least 1"
                        .to_string(),
                ));
            }
            if telemetry.service_name.is_empty() {
                return Err(Error::Config(
                    "telemetry.service_name is empty; it becomes the `service.name` resource \
                     attribute, which is what every backend groups spans by"
                        .to_string(),
                ));
            }
            for name in telemetry.headers.keys() {
                if name.is_empty() || name.bytes().any(|byte| !is_header_name_byte(byte)) {
                    return Err(Error::Config(format!(
                        "telemetry.headers has {name:?}, which is not a valid HTTP header name"
                    )));
                }
            }
        }

        // The KV storages, checked here for the same reason the listeners above are: an empty name or
        // a typo in one is a configuration mistake, and `volucer check` should say so rather than the
        // first `kv.get` from a worker.
        for (name, storage) in &self.kv {
            if name.is_empty() {
                return Err(Error::Config(
                    "a [kv.<name>] table has an empty name, so there is no way for a call to name it"
                        .to_string(),
                ));
            }
            match storage {
                KvStorageConfig::Memory(memory) => {
                    // Zero is refused rather than accepted as "unbounded". An unbounded cache is the
                    // thing the bound exists to prevent, and an operator who wrote `max_bytes = 0`
                    // meant something — most likely "no limit" — that this will not do. Saying so is
                    // the only way they find out before the OOM killer does.
                    if memory.max_bytes == 0 {
                        return Err(Error::Config(format!(
                            "kv.{name}.max_bytes is 0, which would refuse every value; set a limit \
                             or remove the key for the {default} byte default",
                            default = default_kv_max_bytes()
                        )));
                    }
                }
                #[cfg(feature = "redis")]
                KvStorageConfig::Redis(redis) => {
                    // The label is the same spelling the keys use, so an operator reading the refusal can
                    // find the line by grep.
                    validate_redis(&format!("kv.{name}"), &redis.url, redis.pool_size)?;
                }
                KvStorageConfig::File(file) => {
                    // An empty path is refused rather than resolved to "", which would make the store's
                    // directory the process's working directory — a cache that scatters hashed files
                    // beside the application's own files is the worst of both worlds, and it is what
                    // `Path::new("")` would build.
                    if file.path.as_os_str().is_empty() {
                        return Err(Error::Config(format!(
                            "kv.{name}.path is empty; give the store a directory of its own"
                        )));
                    }
                    // The same argument as `max_bytes` on the memory driver: zero is refused rather than
                    // read as "unbounded", because an unbounded cache is what the bound exists to
                    // prevent and an operator who wrote it meant something this will not do.
                    if file.max_bytes == 0 {
                        return Err(Error::Config(format!(
                            "kv.{name}.max_bytes is 0, which would refuse every value; set a limit \
                             or remove the key for the {default} byte default",
                            default = default_kv_max_bytes()
                        )));
                    }
                }
            }
        }

        #[cfg(feature = "redis")]
        if let Some(LockConfig::Redis(redis)) = &self.lock {
            validate_redis("lock", &redis.url, redis.pool_size)?;
            if redis.read_timeout_ms == 0 {
                // Zero is refused rather than read as "no deadline": a command with no deadline is a
                // worker that hangs on a Redis that went away, which is the failure this key exists to
                // bound. An operator who wrote it meant "wait for ever", and that is not on offer.
                return Err(Error::Config(
                    "lock.read_timeout_ms is 0, so a lock command would have no deadline and a worker \
                     blocked on it could not be told the outcome; remove the key for the five-second \
                     default"
                        .to_string(),
                ));
            }
        }

        self.validate_tls()?;

        // The queues, checked here for the reason the KV storages are: a typo in a pipeline name or a
        // `consume` entry with no pipeline behind it is a configuration mistake, and `volucer check`
        // should say so rather than a consumer that starts, finds nothing, and reports no error.
        if let Some(jobs) = &self.jobs {
            for (name, pipeline) in &jobs.pipelines {
                if name.is_empty() {
                    return Err(Error::Config(
                        "a [jobs.pipelines.<name>] table has an empty name, so no producer could name it"
                            .to_string(),
                    ));
                }
                // Checked for the same reason a KV storage's Redis URL is: `volucer check` must not
                // require the server to be up, so a URL that parses is all the configuration can vouch
                // for — but a URL that *does not* parse is a mistake that would otherwise surface as a
                // queue that could not be built at startup, with the message buried in a connect error.
                #[cfg(feature = "redis")]
                if let PipelineConfig::Redis(redis) = pipeline {
                    validate_redis(
                        &format!("jobs.pipelines.{name}"),
                        &redis.url,
                        redis.pool_size,
                    )?;
                    if redis.read_timeout_ms == 0 {
                        return Err(Error::Config(format!(
                            "[jobs.pipelines.{name}] sets read_timeout_ms = 0, which would make every \
                             command time out the instant it was sent; leave it out for the default"
                        )));
                    }
                }
                let _ = pipeline.driver();
            }

            let mut seen = std::collections::BTreeSet::new();
            for name in &jobs.consume {
                if !jobs.pipelines.contains_key(name) {
                    return Err(Error::Config(format!(
                        "jobs.consume names {name:?}, which is not a configured pipeline; {}",
                        if jobs.pipelines.is_empty() {
                            "this deployment configures no pipelines at all".to_string()
                        } else {
                            format!(
                                "this deployment has {}",
                                jobs.pipelines
                                    .keys()
                                    .map(|name| format!("{name:?}"))
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            )
                        }
                    )));
                }
                if !seen.insert(name) {
                    return Err(Error::Config(format!(
                        "jobs.consume names {name:?} twice, which means two lines disagree and only one \
                         of them is read"
                    )));
                }
            }

            // A consumer needs a command, and the check is here rather than at startup because there is
            // nothing to default to: a jobs pool without one is a server that consumes a queue with
            // nobody to hand jobs to, which reports as a pool of zero rather than as a missing key.
            match (&jobs.pool, jobs.consume.is_empty()) {
                (None, false) => {
                    return Err(Error::Config(
                        "jobs.consume names pipelines to consume but there is no [jobs.pool] table, so \
                         there is no command to start a consumer with"
                            .to_string(),
                    ))
                }
                (Some(pool), _) => {
                    if pool.command.trim().is_empty() {
                        return Err(Error::Config(
                            "jobs.pool.command must not be empty".to_string(),
                        ));
                    }
                    crate::env::check_env_table("jobs.pool.env", &pool.env)
                        .map_err(Error::Config)?;
                    if pool.job_lease_seconds == 0 {
                        // Zero would expire every reservation the instant it was handed out, which means
                        // every job is handed to every consumer at once. An operator who wrote it meant
                        // "no limit", and that is not a thing a lease can be.
                        return Err(Error::Config(
                            "jobs.pool.job_lease_seconds is 0, which would expire every reservation \
                             immediately and hand the same job to every consumer; set a time longer than \
                             the slowest job"
                                .to_string(),
                        ));
                    }
                    if pool.prefetch == 0 {
                        return Err(Error::Config(
                            "jobs.pool.prefetch is 0, so no consumer would ever reserve a job; remove the \
                             key for the one-at-a-time default"
                                .to_string(),
                        ));
                    }
                    // The same key and the same rules as a backend's, because a consumer's transport
                    // default is the backend's and the address has to be expressible wherever the
                    // transport can be. A jobs pool that could choose the socket transport but not
                    // say where to listen would be forced onto loopback TCP and a fresh ephemeral
                    // port per consumer.
                    if let Some(socket) = &pool.socket {
                        validate_worker_socket(socket, pool.transport)
                            .map_err(|err| Error::Config(format!("jobs.pool.socket: {err}")))?;
                    }
                }
                (None, true) => {}
            }
        }

        // The access log's header must be a usable header name, and the check is here rather than
        // in the front-end because a bad name is a configuration mistake: `hyper`'s `HeaderName`
        // rejects a space or a colon, and a header that cannot be read or written would make the
        // adoption silently never happen — which looks like "tracing does not work" rather than
        // like a typo.
        //
        // The *path* is deliberately not checked here. A missing directory is reported once at
        // startup by `AccessLog::open`, which is the only place that knows whether the file could
        // actually be created — `Path::is_dir` here would be a second, weaker answer to the same
        // question, and a warning that fires on a directory that exists but is not writable would
        // teach an operator to ignore it.
        if let Some(log) = &self.server.access_log {
            if !log.request_id_header.is_empty() && !is_header_name(&log.request_id_header) {
                return Err(Error::Config(format!(
                    "server.access_log.request_id_header {:?} is not a valid header name; leave it \
                     empty to always generate a request id instead of adopting one",
                    log.request_id_header
                )));
            }
        }

        for (name, backend) in &self.backends {
            self.validate_backend(name, backend)?;
        }

        // An upstream is only reachable over HTTP: there is no CGI environment to build for it and
        // the FastCGI front-end has no way to hand it a request. A deployment that configures one
        // and no HTTP listener has configured something that cannot receive a request at all, and
        // the failure is otherwise silent — the backend starts, the sockets bind, and nothing ever
        // reaches the origin. An error rather than a warning, unlike the static-layer check below,
        // because there is no *second* thing the upstream could be for.
        if self.server.http.is_none() {
            if let Some((name, _)) = self
                .backends
                .iter()
                .find(|(_, backend)| matches!(backend, BackendConfig::Upstream(_)))
            {
                return Err(Error::Config(format!(
                    "backends.{name} is an upstream, which is only reached over HTTP, but no \
                     server.http listener is configured; set server.http, or use a FastCGI \
                     backend for this application"
                )));
            }
        }

        // Static content is served over HTTP, and a FastCGI client does its own — so asking
        // for it with no HTTP listener is a mistake worth reporting. But *asking* is the
        // operative word: the layer is on unless a deployment says otherwise, and a
        // FastCGI-only config that never mentioned it is not asking for anything. Rejecting
        // that would make the default unshippable, so only an explicit request is refused —
        // which is why `StaticsSetting` has an `Unspecified` variant to tell them apart.
        let asked_for_statics = self.server.static_files.asks_for_statics()
            || self.sites.values().any(|site| {
                site.static_files
                    .as_ref()
                    .is_some_and(StaticsSetting::asks_for_statics)
            });
        if asked_for_statics && self.server.http.is_none() {
            return Err(Error::Config(
                "static_files is enabled but no server.http listener is configured; \
                 static content is served over HTTP, and a FastCGI client does its own"
                    .to_string(),
            ));
        }
        if self
            .server
            .document_root
            .as_ref()
            .is_some_and(|root| root.as_os_str().is_empty())
        {
            return Err(Error::Config(
                "server.document_root is empty; set a directory or remove the key".to_string(),
            ));
        }
        if self
            .server
            .front_controller
            .as_ref()
            .is_some_and(|front| front.trim().is_empty())
        {
            return Err(Error::Config(
                "server.front_controller is empty, which is not how \"serve only real files\" is \
                 written here — TOML has no way to tell an empty string from an absent key, so \
                 the meaning is reserved. Remove the key to get the default, or use \
                 `volucer serve --front-controller ''` to mean \"only real files\"."
                    .to_string(),
            ));
        }
        // Parsed rather than only checked, because the same list decides three things — what the
        // router hands a backend, what the static layer refuses to serve from disk, and whether a
        // route may skip the router's `stat`. A list that is refused here can therefore never be
        // half-applied: there is one parse and one value.
        if let Some(extensions) = &self.server.script_extensions {
            crate::script::ScriptExtensions::from_config(extensions, "server.script_extensions")?;
        }
        if self.server.front_controller.as_ref().is_some_and(|front| {
            let front = front.trim_start();
            front.starts_with('/')
                || front.contains('\\')
                || Path::new(front)
                    .components()
                    .any(|part| matches!(part, std::path::Component::ParentDir))
        }) {
            return Err(Error::Config(
                "server.front_controller must be a relative path below the document root, e.g. \
                 \"index.php\" or \"public/index.php\" — an absolute path, a `..` component or a \
                 backslash would escape it"
                    .to_string(),
            ));
        }
        if self.server.statics().cache_control.trim().is_empty() {
            return Err(Error::Config(
                "server.static_files.cache_control is empty; set a value or remove the key"
                    .to_string(),
            ));
        }
        if self.server.statics().max_multirange_bytes == 0 {
            return Err(Error::Config(
                "server.static_files.max_multirange_bytes is zero, which refuses every \
                 multi-range request; set a value or remove the key"
                    .to_string(),
            ));
        }
        if self.server.max_body_bytes == Some(0) {
            return Err(Error::Config(
                "server.max_body_bytes is zero, which refuses every request with a body; set a \
                 value or remove the key — an unset key means the 32 MiB default"
                    .to_string(),
            ));
        }
        // Checked through the same constructor the server uses, so a file the CLI accepts is a file
        // a listener can apply. The alternative is a validation that repeats the arithmetic and a
        // server that disagrees with it.
        if let Err(reason) = self.server.rate_limiter() {
            return Err(Error::Config(reason));
        }
        // `serve_only = "backend"` and an enabled static layer are contradictory:
        // one says the static layer never runs, the other says it must. Which wins
        // would otherwise depend on which check read the config, so it is refused.
        if self.server.serve_only == ServeOnly::Backend && self.server.statics().enabled {
            return Err(Error::Config(
                "server.serve_only is \"backend\", which skips the static layer, but \
                 server.static_files is enabled; write one of them"
                    .to_string(),
            ));
        }

        // The two keys that moved into `[server]` in 0.3.0. Refused by name and with the new
        // location, because `deny_unknown_fields` would otherwise report them as unknown — true,
        // and useless to an operator whose file says exactly what it used to.
        if self.legacy_default_backend.is_some() {
            return Err(Error::Config(
                "default_backend moved into the [server] table in 0.3.0; write it as \
                 `[server]` + `default_backend = \"...\"` (the top-level key is no longer read)"
                    .to_string(),
            ));
        }
        if self.legacy_routes.is_some() {
            return Err(Error::Config(
                "routes moved into the [server] table in 0.3.0; write `[[server.routes]]` instead \
                 of `[[routes]]` (the top-level key is no longer read)"
                    .to_string(),
            ));
        }

        match &self.server.default_backend {
            None => {
                // Backends without a `default_backend` is a typo: nothing could
                // select them. No backends at all is the static-only deployment,
                // which `Dispatcher` accepts and the CLI warns about.
                if !self.backends.is_empty() {
                    return Err(Error::Config(
                        "backends are defined but default_backend is not set".to_string(),
                    ));
                }
            }
            Some(name) if !self.backends.contains_key(name) => {
                return Err(Error::Config(format!(
                    "default_backend {name:?} does not match any configured backend"
                )));
            }
            Some(_) => {}
        }

        for (index, route) in self.server.routes.iter().enumerate() {
            validate_route(route, "server.routes", index, &self.backends)?;
        }

        self.validate_sites()?;
        self.validate_acme()?;

        Ok(())
    }
}
