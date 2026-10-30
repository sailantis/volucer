//! TLS settings, and the listener arrangements they have to agree with.

use super::*;

impl Config {
    /// Validate the TLS settings, and the listener arrangements they have to agree with.
    ///
    /// Every rule here exists because the alternative is a configuration that parses, reports
    /// nothing, and then does something the file does not say — a listener that accepts and
    /// fails every handshake, or a per-site key that silently has no effect.
    pub(super) fn validate_tls(&self) -> Result<()> {
        // The deployment's policy, before the listener rules: it is one statement about this
        // server's responses, and every rule about it is independent of which listener carries
        // TLS. A site's own table is checked in `validate_sites`, against these same answers.
        self.validate_server_hsts()?;

        let Some(tls) = &self.server.tls else {
            // No `[server.tls]`: TLS is not configured, so a listener that needs it is the
            // mistake to report. Saying so by name is the difference between "why does my
            // https listener fail every handshake" and a config error.
            if self.server.https.is_some() {
                return Err(Error::Config(
                    "server.https is set but [server.tls] is not; an HTTPS listener needs a \
                     certificate, so add [server.tls] with a certificate and key"
                        .to_string(),
                ));
            }
            if self.sites.values().any(|site| site.tls.is_some()) {
                let name = self
                    .sites
                    .iter()
                    .find(|(_, site)| site.tls.is_some())
                    .map(|(name, _)| name.clone())
                    .unwrap_or_default();
                return Err(Error::Config(format!(
                    "sites.{name}.tls is set but [server.tls] is not; a per-site certificate \
                     overrides the server's, so the server needs one first"
                )));
            }
            if self.server.redirect_to_https {
                return Err(Error::Config(
                    "server.redirect_to_https is set but [server.tls] is not, so there is no \
                     HTTPS endpoint to redirect to"
                        .to_string(),
                ));
            }
            return Ok(());
        };

        // Every TLS key is a *pair*, and a pair with one half is a mistake rather than a partial
        // configuration: the other half has no meaning and the failure would be a missing file
        // named by a key the operator thought they had filled in. Absent **both** is the ACME case
        // and is checked against `validate_acme` below, because whether "generate one" is allowed
        // depends on something this function cannot see on its own.
        match (&tls.certificate, &tls.key) {
            (Some(certificate), Some(key)) => {
                if certificate.as_os_str().is_empty() {
                    return Err(Error::Config(
                        "server.tls.certificate is empty; name the PEM file".to_string(),
                    ));
                }
                if key.as_os_str().is_empty() {
                    return Err(Error::Config(
                        "server.tls.key is empty; name the PEM file".to_string(),
                    ));
                }
            }
            (None, None) => {}
            (Some(_), None) => {
                return Err(Error::Config(
                    "server.tls.certificate is set but server.tls.key is not; name both, or \
                     leave both out to have a placeholder generated (which needs [server.acme] \
                     to replace it)"
                        .to_string(),
                ));
            }
            (None, Some(_)) => {
                return Err(Error::Config(
                    "server.tls.key is set but server.tls.certificate is not; name both, or \
                     leave both out to have a placeholder generated (which needs [server.acme] \
                     to replace it)"
                        .to_string(),
                ));
            }
        }

        for protocol in &tls.alpn {
            if protocol.is_empty() {
                return Err(Error::Config(
                    "server.tls.alpn contains an empty protocol; name one, or remove the key to \
                     get the default"
                        .to_string(),
                ));
            }
            if !KNOWN_ALPN.contains(&protocol.as_str()) {
                return Err(Error::Config(format!(
                    "server.tls.alpn {protocol:?} is not a known protocol; expected one of \
                     {KNOWN_ALPN:?}"
                )));
            }
        }
        if tls.alpn.iter().any(|p| p == "h2") && !tls.alpn.iter().any(|p| p == "http/1.1") {
            // Not fatal — a deployment may genuinely want h2 only — but it is nearly always a
            // mistake with an invisible cost: a client that cannot do h2 fails the handshake
            // rather than being served HTTP/1.1.
            tracing::warn!(
                "server.tls.alpn offers \"h2\" without \"http/1.1\"; a client that cannot do \
                 HTTP/2 will fail the handshake rather than being served HTTP/1.1"
            );
        }

        match (&self.server.http, &self.server.https) {
            (None, None) => {
                return Err(Error::Config(
                    "[server.tls] is set but no listener carries it: set server.http (TLS on \
                     that port) and/or server.https"
                        .to_string(),
                ));
            }
            (Some(http), Some(https)) if http == https => {
                return Err(Error::Config(format!(
                    "server.http and server.https are both {http:?}; they are two listeners, so \
                     they need two addresses"
                )));
            }
            (Some(_), Some(_)) if tls.plain => {
                return Err(Error::Config(
                    "[server.tls].plain is set together with server.https; the plaintext \
                     listener is already server.http, so the sniff has nothing to decide \
                     — remove one of them"
                        .to_string(),
                ));
            }
            _ => {}
        }
        // A site's own table may only change which certificate is presented, because the name
        // is claimed before any request exists. Anything else here would describe something the
        // socket does not do, so it is refused rather than ignored.
        for (name, site) in &self.sites {
            let Some(names) = &site.tls else { continue };
            match (&names.certificate, &names.key) {
                (Some(certificate), Some(key)) => {
                    if certificate.as_os_str().is_empty() {
                        return Err(Error::Config(format!(
                            "sites.{name}.tls.certificate is empty; name the PEM file, or remove \
                             the table to inherit [server.tls]"
                        )));
                    }
                    if key.as_os_str().is_empty() {
                        return Err(Error::Config(format!(
                            "sites.{name}.tls.key is empty; name the PEM file, or remove the \
                             table to inherit [server.tls]"
                        )));
                    }
                }
                (None, None) => {}
                (Some(_), None) => {
                    return Err(Error::Config(format!(
                        "sites.{name}.tls.certificate is set but sites.{name}.tls.key is not; \
                         name both, or leave both out to have a placeholder generated (which \
                         needs an ACME table for this site to replace it)"
                    )));
                }
                (None, Some(_)) => {
                    return Err(Error::Config(format!(
                        "sites.{name}.tls.key is set but sites.{name}.tls.certificate is not; \
                         name both, or leave both out to have a placeholder generated (which \
                         needs an ACME table for this site to replace it)"
                    )));
                }
            }
        }

        // A redirect needs a plaintext listener to redirect *from*. With TLS on `http` and
        // `plain` off, that socket never answers cleartext, so the key could never fire.
        if self.server.redirect_to_https && !self.server.has_plaintext_listener() {
            return Err(Error::Config(
                "server.redirect_to_https is set but nothing serves cleartext: with TLS on \
                 server.http, set [server.tls].plain = true, or add a server.https listener \
                 so server.http stays plaintext"
                    .to_string(),
            ));
        }

        // **A cleartext socket has nowhere to redirect to.** The redirect target is
        // `https://host:<port>`, and a listener with no port cannot name one — so the answer would
        // be a `Location` an operator reads as a bug rather than as a property of the transport.
        // Refused here rather than at bind, for the reason every other listener rule is: a settings
        // file that passes `check` and cannot serve is what `check` exists to prevent.
        //
        // Only the listener that would carry the redirect is examined. A deployment with an `https`
        // listener is redirecting *from* `http`, and a unix `http` cannot do it either — which is
        // this same check, since `http` is where the cleartext request arrives.
        if self.server.redirect_to_https {
            if let Some(http) = &self.server.http {
                let cleartext_here = matches!(
                    parse_listener(http),
                    Ok(Listener::Unix(_))
                ) && self
                    .server
                    .tls
                    .as_ref()
                    .is_some_and(|tls| self.server.https.is_none() || tls.plain);
                if cleartext_here {
                    return Err(Error::Config(format!(
                        "server.redirect_to_https is set but server.http is {http}, a unix socket, \
                         which has no port for an https redirect to name; remove \
                         redirect_to_https, or serve this site over a TCP listener"
                    )));
                }
            }
        }

        Ok(())
    }

    /// `[server.hsts]` on its own terms, and whether the deployment has anywhere to send it.
    ///
    /// Separate from [`Config::validate_tls`]'s listener rules because the failure it prevents is a
    /// different one: a policy the file states and no response ever carries. `max_age_s` is required
    /// here — there is nothing above this table to inherit from — and a policy with no TLS endpoint
    /// at all can never be emitted, since RFC 6797 §7.2 forbids the header over a non-secure
    /// transport.
    fn validate_server_hsts(&self) -> Result<()> {
        let Some(hsts) = &self.server.hsts else {
            return Ok(());
        };
        if !hsts.is_enabled() {
            // `enabled = false` makes this table defaults only, so its other keys still apply to
            // sites that inherit them — which is why the site rules read it as well.
            return Ok(());
        }
        let Some(max_age_s) = hsts.max_age_s else {
            return Err(Error::Config(
                "[server.hsts] states no max_age_s; a policy needs a duration, and there is \
                 nothing above this table to inherit one from"
                    .to_string(),
            ));
        };
        if max_age_s == 0 {
            return Err(Error::Config(
                "[server.hsts].max_age_s is 0, which in the header means \"delete this policy\"; \
                 remove the table instead of writing a zero duration"
                    .to_string(),
            ));
        }
        let include_subdomains = hsts.include_subdomains.unwrap_or(false);
        if hsts.preload.unwrap_or(false) {
            if !include_subdomains {
                return Err(Error::Config(
                    "[server.hsts].preload is true without include_subdomains; the preload list \
                     requires it, and a name added without it stops covering the subdomains it was \
                     submitted for"
                        .to_string(),
                ));
            }
            if max_age_s < HstsPolicy::PRELOAD_MIN_MAX_AGE_S {
                return Err(Error::Config(format!(
                    "[server.hsts].preload is true with max_age_s = {max_age_s}; the preload list \
                     requires at least {} (one year)",
                    HstsPolicy::PRELOAD_MIN_MAX_AGE_S
                )));
            }
        }
        // No TLS anywhere: the header is forbidden over a non-secure transport (§7.2), so a policy
        // here would be one this deployment could never emit. A site with its own `[sites.x.tls]`
        // still needs `[server.tls]`, so this one question covers every site.
        let serves_tls = self.server.tls.is_some();
        if !serves_tls {
            return Err(Error::Config(
                "[server.hsts] is set but [server.tls] is not, so no response is ever sent over a \
                 secure transport and the header would never be emitted (RFC 6797 §7.2 forbids it \
                 over cleartext)"
                    .to_string(),
            ));
        }
        Ok(())
    }
}
