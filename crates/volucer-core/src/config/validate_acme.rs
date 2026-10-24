//! The ACME tables, and the orders they describe.

use super::tls::validate_acme_table;
use super::*;

impl Config {
    /// Validate `[server.acme]`, and the arrangement it has to agree with.
    ///
    /// Every rule here exists because the alternative is a deployment that reports nothing and then
    /// behaves in a way the file does not say. Two of them are about silence, which is the failure
    /// mode ACME has that nothing else in this server does: issuance that never succeeds is
    /// indistinguishable from issuance that is working, until a certificate expires.
    /// Validate the ACME tables — the deployment's and each site's — and the arrangement they have
    /// to agree with.
    ///
    /// Every rule here exists because the alternative is a deployment that reports nothing and then
    /// behaves in a way the file does not say. Two of them are about silence, which is the failure
    /// mode ACME has that nothing else in this server does: issuance that never succeeds is
    /// indistinguishable from issuance that is working, until a certificate expires.
    pub(super) fn validate_acme(&self) -> Result<()> {
        // "Will something be issued" rather than "is a table present": `enabled = false` keeps a
        // table for its settings, and every reader below has to agree that it issues nothing. The
        // difference is not cosmetic — a disabled table that still counted would demand a
        // `[server.tls]` for a deployment that never orders, and would suppress the error for an
        // absent pair that nothing will replace.
        let server_issues = self.server.acme.as_ref().is_some_and(|acme| acme.enabled);
        let site_issues: Vec<(&String, &SiteConfig, &SiteAcmeConfig)> = self
            .sites
            .iter()
            .filter_map(|(name, site)| {
                let acme = site.acme.as_ref()?;
                match acme.issues() {
                    true => Some((name, site, acme)),
                    false => None,
                }
            })
            .collect();
        let any_acme = server_issues || !site_issues.is_empty();

        // TLS has to be on at all, because TLS-ALPN-01 is validated on the TLS port: with no
        // `[server.tls]` there is no TLS listener for the CA to connect to. This holds for a
        // site-level table too, since a per-site certificate is still presented by the server's
        // listener.
        if any_acme && self.server.tls.is_none() {
            return Err(Error::Config(
                "an ACME table is set but [server.tls] is not; the TLS-ALPN-01 challenge is \
                 validated on the TLS port, so add [server.tls] (its certificate and key may be \
                 left out, and one will be generated)"
                    .to_string(),
            ));
        }

        // A **generated** placeholder is only honest when something will replace it. Without that,
        // a listener with nothing to serve fails every handshake that is not a challenge — hazard
        // 56's sibling — and the failure appears at connect time rather than at startup.
        if !any_acme {
            if let Some(tls) = &self.server.tls {
                if tls.certificate.is_none() && tls.key.is_none() {
                    return Err(Error::Config(
                        "server.tls.certificate and server.tls.key are both absent, and no ACME \
                         table will issue a certificate to replace a generated one; name a pair, \
                         or add [server.acme]"
                            .to_string(),
                    ));
                }
            }
        }

        if let Some(acme) = &self.server.acme {
            // Validated even when disabled, because the settings are still in force: a site that
            // inherits them inherits into a table that has to be well formed, and a bad `cache` or
            // `contact` would be reported at whatever point the site finally used it. The domains
            // rule is the exception — a disabled table issues for nothing, so an empty list is not
            // an error there, and refusing it would make "turn it off" impossible to write for a
            // deployment that never stated any.
            if acme.enabled && acme.domains.is_empty() {
                return Err(Error::Config(
                    "server.acme.domains is empty, so there is nothing to issue for; list the \
                     names the certificate must cover, or set server.acme.enabled = false"
                        .to_string(),
                ));
            }
            if acme.enabled {
                validate_acme_table("server.acme", &acme.domains, &acme.contact, &acme.cache)?;
            }
        }

        let mut claimed_names: BTreeMap<String, String> = BTreeMap::new();
        if let Some(acme) = &self.server.acme {
            if acme.enabled {
                for domain in &acme.domains {
                    claimed_names.insert(domain.clone(), "server.acme".to_string());
                }
            }
        }

        for (name, site) in &self.sites {
            let Some(acme) = &site.acme else { continue };
            let label = format!("sites.{name}.acme");

            // A site that names its own certificate has decided not to have one issued, so it
            // orders nothing and there is no second answer to refuse. Mechanically the two already
            // work together: `CertificateSet` gives such a site no generated placeholder, and an
            // issued certificate overwrites the named entry for the names it covers. Naming a pair
            // *and* asking for one is therefore how a deployment migrates from a certificate
            // another authority issued, serving it until ACME replaces it — which is exactly what
            // `[server.tls]` plus `[server.acme]` has always allowed. The two levels agree.
            //
            // An *absent* pair beside a site ACME table is the other half of the feature: the site
            // starts on a generated placeholder. Half a pair is still half a pair, and
            // `validate_tls` reports that.
            if !acme.issues() {
                continue;
            }

            let (names, dropped) = acme.resolve_domains(&site.hosts);
            if names.is_empty() {
                // The derivation found nothing it could put on a certificate. Reported with the
                // patterns that were left out, because "which names did I lose?" is the operator's
                // next question and the answer belongs in this message rather than in a log line.
                return Err(Error::Config(format!(
                    "{label} has no domains and none of sites.{name}.hosts can go on a certificate \
                     ({}); a wildcard cannot be a certificate name, so list the real names in \
                     {label}.domains",
                    dropped.join(", ")
                )));
            }
            for pattern in &dropped {
                // Logged rather than an error: the certificate simply does not cover that pattern,
                // which the operator may well have intended. It is worth saying out loud because
                // the symptom is a browser warning on a name this site answers to.
                tracing::warn!(
                    site = name,
                    pattern = pattern.as_str(),
                    "this site's ACME table derives its domains from hosts, and a wildcard cannot \
                     be a certificate name: the site answers to that pattern and the certificate \
                     will not cover it"
                );
            }
            let cache = acme.resolved_cache(&self.server.acme);
            validate_acme_table(
                &label,
                &names,
                acme.resolved_contact(&self.server.acme),
                &cache,
            )?;

            // A name claimed by two ACME tables cannot be issued twice, and the failure it leads to
            // is invisible. A validation handshake carries only the SNI name, so the deployment's
            // source answers first and returns the *deployment's* challenge certificate — whose
            // `acmeIdentifier` commits to the deployment's key authorization while the site's
            // server is being asked for the site's. The site's order then fails validation on every
            // attempt, and all it can do is retry.
            //
            // Refused rather than warned about because a warning here is indistinguishable from the
            // noise of a first deployment, and because the two ways out are both one edit: drop the
            // name from the deployment's list, or state the site's names so they no longer overlap.
            for domain in &names {
                if let Some(other) = claimed_names.insert(domain.clone(), label.clone()) {
                    return Err(Error::Config(format!(
                        "{domain:?} is issued for by both {other} and {label}; one name belongs to \
                         one order, so remove it from one of them (a site that states its own \
                         `domains` no longer overlaps the deployment's)"
                    )));
                }
            }
        }

        Ok(())
    }
}
