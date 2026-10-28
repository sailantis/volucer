//! The site table: host patterns, duplicates, and per-site settings.

use super::rules::{
    validate_front_controller, validate_host_pattern, validate_route, validate_statics,
};
use super::*;

impl Config {
    /// The site table: host patterns, duplicates, and per-site settings.    ///
    /// Separate from `validate` only because it is long; the two are one rule set
    /// and are always called together.
    pub(super) fn validate_sites(&self) -> Result<()> {
        // Host patterns, and the duplicate check. A host claimed by two sites is
        // the one mistake a reader cannot see from either site alone: one of them
        // would silently never be reached, and which one depends on map order.
        let mut claimed: BTreeMap<String, &str> = BTreeMap::new();
        let mut address_defaults: BTreeMap<String, &str> = BTreeMap::new();
        for (name, site) in &self.sites {
            if site.hosts.is_empty() && site.addresses.is_empty() {
                return Err(Error::Config(format!(
                    "sites.{name} states neither `hosts` nor `addresses`, so this site would never \
                     be reached; list at least one host name, at least one address, or both"
                )));
            }
            if site.document_root.as_os_str().is_empty() {
                return Err(Error::Config(format!(
                    "sites.{name}.document_root is empty; set the directory holding this site"
                )));
            }

            for host in &site.hosts {
                validate_host_pattern(host, &format!("sites.{name}.hosts"))?;
                let key = host.trim().to_ascii_lowercase();
                if let Some(existing) = claimed.insert(key, name) {
                    return Err(Error::Config(format!(
                        "sites.{existing} and sites.{name} both claim the host {host:?}; the first \
                         one declared would be the only one ever reached"
                    )));
                }
            }

            // Parsed with the same parser `[server.proxy].trusted_subnets` uses, so a range means
            // one thing in this file. Refused rather than dropped: an address that does not parse
            // is an operator who believes a site is bound to something it is not, and the failure
            // in that direction is an internal site served to whatever reached the port.
            for spec in &site.addresses {
                crate::clientip::Cidr::parse(spec)
                    .map_err(|reason| Error::Config(format!("sites.{name}.addresses: {reason}")))?;
            }

            if let Some(protocols) = &site.protocols {
                if protocols.is_empty() {
                    return Err(Error::Config(format!(
                        "sites.{name}.protocols is empty, so this site would answer on no \
                         protocol at all; name at least one of \"http\", \"https\" or \"fastcgi\", \
                         or remove the key to accept every one"
                    )));
                }
                let mut seen = Vec::new();
                for protocol in protocols {
                    if seen.contains(protocol) {
                        return Err(Error::Config(format!(
                            "sites.{name}.protocols names \"{protocol}\" twice; each protocol is \
                             listed at most once"
                        )));
                    }
                    seen.push(*protocol);
                }
            }

            // A site with addresses and no hosts is a *default for those addresses*: it is chosen
            // by the address alone, for a client that sent no name or a name nobody claims. That is
            // the nginx `default_server` shape and it is worth having — an internal IP serving one
            // application should not have to invent a hostname — but two of them claiming the same
            // address would be ambiguous, so that is refused rather than resolved by declaration
            // order.
            if site.hosts.is_empty() {
                for spec in &site.addresses {
                    let key = spec.trim().to_ascii_lowercase();
                    if let Some(existing) = address_defaults.insert(key, name) {
                        return Err(Error::Config(format!(
                            "sites.{existing} and sites.{name} are both address-only sites claiming \
                             {spec:?}; two defaults for one address cannot be told apart, so one \
                             would silently never be reached — give one of them a `hosts` list"
                        )));
                    }
                }
            }

            if let Some(front) = &site.front_controller {
                validate_front_controller(front, &format!("sites.{name}.front_controller"))?;
            }

            // A redirect is only meaningful on a site that can *receive* cleartext, and only on one
            // that has somewhere to send it. Both halves are checked here rather than at the
            // request path, where the second one would show up as a client bounced to a scheme the
            // site never serves.
            if site.redirect_to_https == Some(true) {
                if let Some(protocols) = &site.protocols {
                    if !protocols.contains(&SiteProtocol::Http) {
                        return Err(Error::Config(format!(
                            "sites.{name}.redirect_to_https is true but sites.{name}.protocols \
                             does not include \"http\", so this site never receives a cleartext \
                             request to redirect; add \"http\" to protocols, or remove the key"
                        )));
                    }
                    if !protocols.contains(&SiteProtocol::Https) {
                        return Err(Error::Config(format!(
                            "sites.{name}.redirect_to_https is true but sites.{name}.protocols \
                             does not include \"https\", so the redirect would send clients to a \
                             scheme this site does not answer; add \"https\", or remove the key"
                        )));
                    }
                }
            }

            // `[server.hsts]` and the site tables, for the three rules that need the site's own
            // facts. `validate_tls` holds the ones about the deployment's listeners.
            validate_hsts(name, site, &self.server)?;

            if let Some(extensions) = &site.script_extensions {
                crate::script::ScriptExtensions::from_config(
                    extensions,
                    &format!("sites.{name}.script_extensions"),
                )?;
            }

            if let Some(routes) = &site.routes {
                for (index, route) in routes.iter().enumerate() {
                    validate_route(
                        route,
                        &format!("sites.{name}.routes"),
                        index,
                        &self.backends,
                    )?;
                }
            }

            if let Some(backend) = &site.default_backend {
                self.resolve_default_backend(backend, &format!("sites.{name}.default_backend"))?;
            }

            if let Some(statics) = &site.static_files {
                let resolved = statics.applied_to(&self.server.statics());
                validate_statics(&resolved, &format!("sites.{name}.static_files"))?;
            }
            // `static_files = false` and `serve_only = "static"` are both a way of
            // saying "files only", so writing them together is a contradiction and
            // writing `false` beside `"backend"` is a double negative. Rejecting is
            // what keeps the resolved answer a single statement.
            if let Some(StaticsSetting::Switch(enabled)) = &site.static_files {
                match (enabled, site.serve_only) {
                    (true, Some(ServeOnly::Backend)) => {
                        return Err(Error::Config(format!(
                            "sites.{name}: static_files = true cannot be combined with \
                             serve_only = \"backend\", which skips the static layer"
                        )))
                    }
                    (false, Some(ServeOnly::Static)) => {
                        return Err(Error::Config(format!(
                            "sites.{name}: static_files = false cannot be combined with \
                             serve_only = \"static\"; write one of them"
                        )))
                    }
                    _ => {}
                }
            }
        }

        Ok(())
    }
}

/// The `Strict-Transport-Security` rules that need a site's own facts.
///
/// The deployment-wide rules — a TLS endpoint existing at all — live in `validate_tls`, because they
/// are about the listeners; these are about one site, and every one of them is a way for the header
/// to be inert or the policy to be stronger than the deployment can honour.
fn validate_hsts(name: &str, site: &SiteConfig, server: &ServerConfig) -> Result<()> {
    let Some(hsts) = &site.hsts else {
        return Ok(());
    };
    if !hsts.is_enabled() {
        // A site turning the policy off is the whole reason `enabled` is a key, and none of the
        // rules below apply to a table that emits nothing.
        return Ok(());
    }

    let inherited = server
        .hsts
        .as_ref()
        .filter(|global| global.is_enabled())
        .and_then(|global| global.resolved(None));
    if hsts.max_age_s.is_none() && inherited.is_none() {
        return Err(Error::Config(format!(
            "sites.{name}.hsts states no max_age_s and [server.hsts] states none either, so there \
             is no policy to send; write max_age_s here, or add [server.hsts] with one"
        )));
    }
    let Some(policy) = hsts.resolved(inherited.as_ref()) else {
        return Ok(());
    };

    // RFC 6797 §8.3: a policy is stored against a *domain name*, and an IP literal never matches
    // one. An address-only site is served for addresses with no name claimed, so there is no name
    // for the browser to file the policy under.
    if site.hosts.is_empty() {
        return Err(Error::Config(format!(
            "sites.{name}.hsts is set but sites.{name} has no `hosts`, so it is served by address \
             alone — a browser stores a policy against a host name and never against an IP, so \
             this header would be ignored by every client that could receive it; give the site the \
             names it serves, or remove the table"
        )));
    }

    // Same argument as the redirect: a site that never answers TLS never sends the header, because
    // §7.2 forbids it over a non-secure transport and browsers ignore it there.
    if let Some(protocols) = &site.protocols {
        if !protocols.contains(&SiteProtocol::Https) {
            return Err(Error::Config(format!(
                "sites.{name}.hsts is set but sites.{name}.protocols does not include \"https\", \
                 so this site never answers over a secure transport — the header may only be sent \
                 there (RFC 6797 §7.2), so it would never be emitted; add \"https\" to protocols, \
                 or remove the table"
            )));
        }
    }

    if let Some(global) = &server.hsts {
        if !global.is_enabled() && hsts.max_age_s.is_none() {
            return Err(Error::Config(format!(
                "sites.{name}.hsts is enabled but [server.hsts] is not, and this site states no \
                 max_age_s; there is no inherited policy, so write max_age_s here"
            )));
        }
    }

    if policy.preload {
        if !policy.include_subdomains {
            return Err(Error::Config(format!(
                "sites.{name}.hsts.preload is true without include_subdomains; the preload list \
                 requires it, and a name added without it stops covering the subdomains it was \
                 submitted for"
            )));
        }
        if policy.max_age_s < HstsPolicy::PRELOAD_MIN_MAX_AGE_S {
            return Err(Error::Config(format!(
                "sites.{name}.hsts.preload is true with max_age_s = {}; the preload list requires \
                 at least {} (one year), and a shorter policy is rejected when the name is \
                 submitted",
                policy.max_age_s,
                HstsPolicy::PRELOAD_MIN_MAX_AGE_S
            )));
        }
    }

    if policy.max_age_s == 0 {
        return Err(Error::Config(format!(
            "sites.{name}.hsts.max_age_s is 0, which in the header means \"delete this policy\"; \
             turn HSTS off with `enabled = false` instead of writing a zero duration"
        )));
    }

    Ok(())
}
