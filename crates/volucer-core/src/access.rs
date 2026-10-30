//! IP allow/deny: which addresses may reach this server at all.
//!
//! # Why this is not a route or a site rule
//!
//! This refuses a request before it costs anything: no site resolution, no static `stat`, no body
//! read, no backend. That is the whole point — a deployment that publishes one address and wants
//! `/admin` reachable only from its own network can already do it per site, but "block this whole
//! range from everything" has to happen above every other decision or it is a filter on the
//! remaining work rather than a boundary.
//!
//! # The order is the security property
//!
//! Rules are evaluated **in the order they are written, and the first match decides**. The
//! alternative — "most specific range wins" — is the rule the *host* table uses, and it is wrong
//! here: a deployment that writes `deny 10.0.0.0/8` followed by `allow 10.1.2.3` means the
//! exception, and most-specific-wins would give the same answer by accident while making it
//! impossible to write the opposite order. First-match is what nginx's `allow`/`deny` do, it is
//! what an operator migrating from it expects, and it lets an exception in either direction be
//! written.
//!
//! # Why the list is one ordered array rather than two keys
//!
//! `allow = [...]` and `deny = [...]` cannot express an order *between* the two lists, and an
//! ordered rule set is the point. TOML arrays preserve order within one array, so the configuration
//! is a single `rules` list with an `action` on each entry.
//!
//! # The default, and why it is `allow`
//!
//! A deployment that listed a few rules and forgot the rest would, under `deny`, stop serving
//! everybody — an outage caused by a security setting. nginx appends an implicit `allow all`
//! for the same reason, and this follows it. A deployment that wants a closed door says
//! `default = "deny"` and lists what is admitted, which is one word more and impossible to write
//! by accident.

use std::net::IpAddr;

use serde::{Deserialize, Serialize};

use crate::clientip::Cidr;

/// What a rule does with a matching address.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    /// The request is served; evaluation stops here.
    ///
    /// The default of the whole policy, so an unlisted address is served — see the module doc.
    #[default]
    Allow,
    /// The request is refused with a `403`, before anything else is consulted.
    Deny,
}

impl Action {
    /// The word this action is written as, so a log line and a report cannot spell it differently.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Action::Allow => "allow",
            Action::Deny => "deny",
        }
    }
}

impl std::fmt::Display for Action {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One rule: an address range, and what to do with a request from inside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccessRule {
    action: Action,
    range: Cidr,
    /// The range as it was written, for a log line that quotes the operator's own text.
    spec: String,
}

impl AccessRule {
    /// What this rule does with `address`.
    #[must_use]
    pub fn action(&self) -> Action {
        self.action
    }

    /// The range as the configuration wrote it.
    #[must_use]
    pub fn spec(&self) -> &str {
        &self.spec
    }

    /// Whether `address` is inside this rule's range.
    #[must_use]
    pub fn matches(&self, address: IpAddr) -> bool {
        self.range.contains(address)
    }
}

/// The process's access policy: an ordered rule list and what an unlisted address gets.
///
/// Built once at startup and once per reload, from a validated configuration, and then read on the
/// request path. The ranges are parsed here rather than per request for the same reason the site
/// table's are: parsing a range is not something a request should pay for, and a range that failed
/// to parse must stop the server rather than become a rule that silently matches nobody.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccessPolicy {
    rules: Vec<AccessRule>,
    default: Action,
}

impl AccessPolicy {
    /// Build a policy from the pairs the configuration holds, in the order it holds them.
    ///
    /// The `&str` is the range as written, and it is kept so a refusal can quote what the operator
    /// wrote rather than a re-rendering of it. `Err` carries the reason with the offending entry
    /// named, because `check` prints it and an operator has to find the line.
    pub fn new<S: AsRef<str>>(
        rules: impl IntoIterator<Item = (Action, S)>,
        default: Action,
    ) -> Result<Self, String> {
        let mut parsed = Vec::new();
        for (index, (action, spec)) in rules.into_iter().enumerate() {
            let spec = spec.as_ref().trim().to_string();
            let range = Cidr::parse(&spec)
                .map_err(|reason| format!("rule {} ({action} {spec:?}): {reason}", index + 1))?;
            parsed.push(AccessRule {
                action,
                range,
                spec,
            });
        }
        Ok(AccessPolicy {
            rules: parsed,
            default,
        })
    }

    /// What an address no rule matched gets.
    #[must_use]
    pub fn default_action(&self) -> Action {
        self.default
    }

    /// The rules, in the order they are evaluated.
    #[must_use]
    pub fn rules(&self) -> &[AccessRule] {
        &self.rules
    }

    /// Whether no rule was written, which is the ordinary case and the one the request path
    /// checks to skip the scan entirely.
    ///
    /// **A policy with rules but `default = "allow"` is not a no-op**: a rule that denies is a
    /// rule, even though an unmatched address is served.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// Why a request from this address is refused, or `None` when it is admitted.
    ///
    /// The reason names the rule, which is what an operator needs to answer "which line blocked
    /// this client" — a bare "forbidden" sends them to read the whole list.
    ///
    /// **`None` (an absent address) is decided by the default.** A unix socket has no peer, so no
    /// rule can match it: it is served under `default = "allow"` and refused under `deny`, which is
    /// the only truthful answer and the reason `validate` refuses a `deny` default beside a socket
    /// listener rather than letting a deployment believe it closed a door it never had.
    #[must_use]
    pub fn refusal(&self, address: Option<IpAddr>) -> Option<Refusal> {
        let Some(address) = address else {
            return match self.default {
                Action::Allow => None,
                Action::Deny => Some(Refusal {
                    rule: None,
                    address: None,
                }),
            };
        };
        for rule in &self.rules {
            if rule.matches(address) {
                return match rule.action {
                    Action::Allow => None,
                    Action::Deny => Some(Refusal {
                        rule: Some(rule.spec.clone()),
                        address: Some(address),
                    }),
                };
            }
        }
        match self.default {
            Action::Allow => None,
            Action::Deny => Some(Refusal {
                rule: None,
                address: Some(address),
            }),
        }
    }

    /// Whether a request from this address is admitted. See [`AccessPolicy::refusal`] for why.
    #[must_use]
    pub fn admits(&self, address: Option<IpAddr>) -> bool {
        self.refusal(address).is_none()
    }
}

/// Why an access policy refused a request.
///
/// Carries the rule when one matched, so the log line can name the line the operator wrote. A
/// refusal by *default* has no rule and says so instead of inventing one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    /// The range as written, when a rule decided.
    pub rule: Option<String>,
    /// The client address, when there was one.
    pub address: Option<IpAddr>,
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match (&self.rule, self.address) {
            (Some(rule), Some(address)) => {
                write!(f, "the client at {address} is in the denied range {rule:?}")
            }
            (Some(rule), None) => {
                write!(f, "the client is in the denied range {rule:?}")
            }
            (None, Some(address)) => write!(
                f,
                "the client at {address} matches no allowed range, and server.access.default is deny"
            ),
            (None, None) => write!(
                f,
                "this connection has no client address, and server.access.default is deny"
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(rules: &[(Action, &str)], default: Action) -> AccessPolicy {
        AccessPolicy::new(rules.iter().copied(), default).expect("the ranges parse")
    }

    fn ip(text: &str) -> Option<IpAddr> {
        Some(text.parse().expect("a test address"))
    }

    #[test]
    fn an_unlisted_address_gets_the_default() {
        let allow = policy(&[(Action::Deny, "10.0.0.0/8")], Action::Allow);
        assert!(allow.admits(ip("203.0.113.5")));
        assert!(!allow.admits(ip("10.1.2.3")));

        let deny = policy(&[(Action::Allow, "10.0.0.0/8")], Action::Deny);
        assert!(!deny.admits(ip("203.0.113.5")));
        assert!(deny.admits(ip("10.1.2.3")));
    }

    #[test]
    fn the_first_matching_rule_decides_and_order_is_the_whole_point() {
        // **The exception must be listed before the range it excepts from**, because the first match
        // decides and a broad rule listed first claims everything inside it. Getting this backwards
        // is the one mistake this ordering invites, so both directions are pinned here.
        let excepted = policy(
            &[(Action::Allow, "10.1.2.3"), (Action::Deny, "10.0.0.0/8")],
            Action::Allow,
        );
        assert!(excepted.admits(ip("10.1.2.3")));
        assert!(!excepted.admits(ip("10.9.9.9")));

        // The same two ranges the other way round: the /8 comes first, so it claims `10.1.2.3` as
        // well and the narrower rule is never reached. This is not a quirk to be worked around — it
        // is exactly what "first match decides" means, and a most-specific-wins rule could not
        // express the case above at all.
        let swallowed = policy(
            &[(Action::Deny, "10.0.0.0/8"), (Action::Allow, "10.1.2.3")],
            Action::Allow,
        );
        assert!(!swallowed.admits(ip("10.9.9.9")));
        assert!(
            !swallowed.admits(ip("10.1.2.3")),
            "a broad rule listed first claims the addresses a later narrow rule names"
        );
    }

    #[test]
    fn a_socket_with_no_address_is_decided_by_the_default() {
        // A unix socket has no peer, so no rule can match it. Under `allow` it is served; under
        // `deny` it is refused, and `validate` is what stops the second from being written beside a
        // socket listener at all.
        assert!(policy(&[(Action::Deny, "10.0.0.0/8")], Action::Allow).admits(None));
        let closed = policy(&[(Action::Allow, "10.0.0.0/8")], Action::Deny);
        assert!(!closed.admits(None));
        assert!(closed.refusal(None).expect("refused").to_string().contains(
            "no client address"
        ));
    }

    #[test]
    fn a_refusal_names_the_rule_that_decided() {
        let policy = policy(&[(Action::Deny, "192.0.2.0/24")], Action::Allow);
        let refusal = policy.refusal(ip("192.0.2.7")).expect("refused");
        assert_eq!(refusal.rule.as_deref(), Some("192.0.2.0/24"));
        let text = refusal.to_string();
        assert!(text.contains("192.0.2.0/24"), "{text}");
        assert!(text.contains("192.0.2.7"), "{text}");
    }

    #[test]
    fn a_bad_range_is_refused_with_its_position_and_text() {
        // The reason must find the line: an operator with forty rules cannot search for "invalid".
        let err = AccessPolicy::new(
            [(Action::Allow, "10.0.0.0/8"), (Action::Deny, "not-an-address")],
            Action::Allow,
        )
        .expect_err("the second range does not parse");
        assert!(err.contains("rule 2"), "{err}");
        assert!(err.contains("not-an-address"), "{err}");
    }

    #[test]
    fn a_policy_with_no_rules_is_empty_but_still_has_a_default() {
        let open = policy(&[], Action::Allow);
        assert!(open.is_empty());
        assert!(open.admits(ip("198.51.100.1")));

        let closed = policy(&[], Action::Deny);
        assert!(closed.is_empty());
        assert!(!closed.admits(ip("198.51.100.1")));
    }

    #[test]
    fn a_v4_range_does_not_match_a_v6_address() {
        // The same rule `Cidr` states, pinned here because it is the one a security setting is most
        // likely to be read as saying the opposite of.
        let policy = policy(&[(Action::Deny, "10.0.0.0/8")], Action::Allow);
        assert!(policy.admits(ip("::ffff:10.1.2.3")));
        assert!(policy.admits(ip("fd00::1")));
    }
}
