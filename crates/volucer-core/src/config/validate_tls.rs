//! TLS settings, and the listener arrangements they have to agree with.

use super::*;

impl Config {
    /// Validate the TLS settings, and the listener arrangements they have to agree with.
    ///
    /// Every rule here exists because the alternative is a configuration that parses, reports
    /// nothing, and then does something the file does not say — a listener that accepts and
    /// fails every handshake, or a per-site key that silently has no effect.
    pub(super) fn validate_tls(&self) -> Result<()> {
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

        Ok(())
    }
}
