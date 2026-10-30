//! Every method a call can name, and which of them a backend may be granted.
//!
//! # Why a catalogue rather than a list of namespaces
//!
//! A `services` entry used to grant a whole namespace: `"workers"` handed a worker both
//! `workers.list` and `workers.reset`. That is one capability where the deployment may want two, and
//! the case that forces the distinction is a CGI backend — an application behind `php-fpm` has **no
//! worker pool**, because only a `worker` or an `attach` backend owns one. So `workers.reset` on such
//! a backend is a grant nobody can honour, and it was documented as "a grant of an answer nobody
//! wants" rather than refused.
//!
//! With a catalogue it is refused, because the name is not grantable there at all. The same listing
//! then answers the other half of the question: what a worker may reach is now a set of methods
//! rather than a set of namespaces, so a backend granted `kv.cache.get` can read the cache and not
//! write it.
//!
//! # One listing, two surfaces
//!
//! `[status.workers] allow` names the same methods as a backend's `services`, so the vocabulary an
//! operator learns for the HTTP routes is the vocabulary for the call plane. That is why the names
//! here are spelled exactly as a call spells them.
//!
//! # The one thing this cannot be
//!
//! It cannot be derived from the namespaces' own `match` arms, because they live in
//! `volucer-server` and this validation lives in `volucer-core` — the leaf every crate depends on.
//! So the catalogue is a second copy of a fact the namespaces already encode, and the copy is kept
//! honest by a test in `volucer-server`: every verb a namespace's `serve` answers is listed here, and
//! every verb listed here reaches an arm that answers it. Adding a method without cataloguing it
//! fails the build rather than shipping a method nothing can grant.

/// How a grant's spelling relates to the call name it covers.
///
/// A KV call is `kv.<storage>.<verb>` — three parts, with the storage in the *middle* — while every
/// other family is `<namespace>.<verb>`. That difference is the whole reason a plain prefix match
/// cannot be used to decide what a grant covers, and the reason a bare `"kv"` cannot be resolved by
/// guessing: the name it would grant is a call shape, not a namespace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MethodShape {
    /// The call is `<namespace>.<verb>`.
    Flat,
    /// The call is `kv.<storage>.<verb>`, for every storage this backend was granted.
    PerStorage,
}

/// One method a call can name, or one grantable subset of a namespace.
///
/// `"kv.cache.get"` and `"kv.get"` are two entries for one method: the first names the storage, the
/// second does not. Both are in the catalogue because both are legal grants with different meanings —
/// the narrow one is the point of the exercise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Method {
    /// The grantable spelling: `"workers.list"`, `"kv.cache.get"`, `"kv.get"`.
    pub name: &'static str,
    /// The namespace the call routes to, which is what `MethodRegistry` keys on.
    pub namespace: &'static str,
    /// The verb `serve` matches, without the storage component.
    pub verb: &'static str,
    /// Whether the method changes something, so that `check` can say so and a policy can refuse it.
    pub mutating: bool,
    pub shape: MethodShape,
}

/// Declare one flat method, so the table below reads as a list rather than as struct literals.
const fn flat(
    name: &'static str,
    namespace: &'static str,
    verb: &'static str,
    mutating: bool,
) -> Method {
    Method {
        name,
        namespace,
        verb,
        mutating,
        shape: MethodShape::Flat,
    }
}

/// Declare one storage-shaped method — a `kv` verb, whose call carries the storage's name.
const fn storage(verb: &'static str, mutating: bool) -> Method {
    Method {
        name: verb,
        namespace: "kv",
        verb,
        mutating,
        shape: MethodShape::PerStorage,
    }
}

/// The verbs a `kv` storage serves, in the order the namespace's own refusal lists them.
///
/// A constant rather than a filter over [`METHODS`], because [`Grant::parse`] needs the set *before*
/// it knows which shape a name has — the table answers "what can be granted", and this answers "is
/// this word a verb or a storage's name", which is a different question asked earlier.
pub const KV_VERBS: &[&str] = &[
    "get",
    "set",
    "setIfAbsent",
    "has",
    "delete",
    "clear",
    "ttl",
    "getMultiple",
    "setMultiple",
];

/// Every method a call can name.
///
/// Ordered namespace by namespace, and each namespace's verbs in the order its own refusal message
/// lists them — so an operator who compares this table with an error message finds the same order.
///
/// **A grant that names no verb grants every verb in the namespace**, which is what keeps an
/// existing `services = ["kv.cache"]` working. It is also why this table's flat entries exist: a
/// namespace-wide grant is the `kv.<storage>` shape without a verb, and the fixed namespaces spell
/// that as `"workers"` — see [`Method::namespace_wide`].
pub const METHODS: &[Method] = &[
    // The deployment's own description, redacted. Read-only, and the only method it has.
    flat("config.describe", "config", "describe", false),
    // The pools. `list` is an answer about the process; `reset` kills a worker and waits for its
    // replacement, which is why the two are separable at all. `restart` is the rolling form: it
    // replaces every worker that is not serving a request, which is how new application code reaches
    // the same addresses without a second process — see `RespawnReport` for why it is allowed to be
    // incomplete.
    flat("workers.list", "workers", "list", false),
    flat("workers.reset", "workers", "reset", true),
    flat("workers.restart", "workers", "restart", true),
    // KV, per verb and per storage. Reads first, then writes, then `clear`, which wipes a storage.
    storage("get", false),
    storage("has", false),
    storage("ttl", false),
    storage("getMultiple", false),
    storage("set", true),
    storage("setIfAbsent", true),
    storage("delete", true),
    storage("clear", true),
    storage("setMultiple", true),
    // Locks. `exists` and `holders` answer; the rest change something. `acquire` and `try_acquire`
    // are one arm in the namespace and two entries here, because a deployment may want a blocking
    // acquire and not the wait-free one.
    flat("lock.exists", "lock", "exists", false),
    flat("lock.holders", "lock", "holders", false),
    flat("lock.acquire", "lock", "acquire", true),
    flat("lock.try_acquire", "lock", "try_acquire", true),
    flat("lock.release", "lock", "release", true),
    flat("lock.release_all", "lock", "release_all", true),
    flat("lock.update_ttl", "lock", "update_ttl", true),
    // Queues, producer side only: a consumer reserves on its own channel, not through a call.
    flat("jobs.push", "jobs", "push", true),
    flat("jobs.push_batch", "jobs", "push_batch", true),
    flat("jobs.size", "jobs", "size", false),
    flat("jobs.pipelines", "jobs", "pipelines", false),
];

impl Method {
    /// Every verb in one namespace, which is what a namespace-wide grant expands to.
    ///
    /// `"kv.cache"` has no entry in [`METHODS`] because its verbs are the shared `kv` set; this walks
    /// them by namespace instead, so the two grant shapes cannot drift.
    pub fn namespace_wide(namespace: &str) -> impl Iterator<Item = &'static Method> + '_ {
        METHODS
            .iter()
            .filter(move |method| method.namespace == namespace)
    }

    /// The verb set of one namespace, sorted and deduplicated, for a message that has to list them.
    pub fn verbs_of(namespace: &str) -> Vec<&'static str> {
        let mut verbs: Vec<&'static str> = Self::namespace_wide(namespace)
            .map(|method| method.verb)
            .collect();
        verbs.sort_unstable();
        verbs.dedup();
        verbs
    }
}

/// One entry of a backend's `services` list, parsed.
///
/// The three shapes are separated here rather than at the point of use, so that validation, the
/// registry and `check`'s display all agree about what a name meant. Parsing is pure and total: a
/// name that parses is still checked against the deployment, which is the other half.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Grant {
    /// `"workers"`, `"config"`, `"lock"`, `"jobs"` — every method in the namespace.
    Namespace { namespace: String },
    /// `"workers.list"`, `"lock.acquire"`, `"jobs.push"` — one method.
    Method { namespace: String, verb: String },
    /// `"kv.cache"` — every verb, on one named storage.
    Storage { storage: String },
    /// `"kv.cache.get"` — one verb on one named storage.
    StorageMethod { storage: String, verb: String },
}

impl Grant {
    /// Read a `services` entry.
    ///
    /// Returns `None` only for shapes no spelling could mean — an empty name, an empty component, or
    /// a `kv` form with too many parts. Everything else is parsed even when the deployment has no
    /// such namespace, because the error for "there is no such namespace" is more useful than the
    /// error for "that is not a name", and only the second is available while parsing.
    pub fn parse(service: &str) -> Option<Grant> {
        let parts: Vec<&str> = service.split('.').collect();
        if parts.iter().any(|part| part.is_empty()) {
            return None;
        }
        match parts.as_slice() {
            [namespace] => Some(Grant::Namespace {
                namespace: (*namespace).to_string(),
            }),
            [name, second] if *name == "kv" => {
                // Two parts after `kv` is ambiguous on its face — `kv.get` is a verb across storages,
                // and `kv.sessions` names a storage — and the resolution is the fixed verb set: a
                // second part that is one of them is a verb, and anything else is a storage's name.
                // Guessing the other way would make `kv.get` a storage called `get`, which is a
                // grant nobody can write on purpose and everybody would assume means the verb.
                if KV_VERBS.contains(second) {
                    Some(Grant::Method {
                        namespace: "kv".to_string(),
                        verb: (*second).to_string(),
                    })
                } else {
                    Some(Grant::Storage {
                        storage: (*second).to_string(),
                    })
                }
            }
            [name, verb] => Some(Grant::Method {
                namespace: (*name).to_string(),
                verb: (*verb).to_string(),
            }),
            [name, storage, verb] if *name == "kv" => Some(Grant::StorageMethod {
                storage: (*storage).to_string(),
                verb: (*verb).to_string(),
            }),
            _ => None,
        }
    }

    /// The namespace this grant reaches, which is what a registry is keyed on.
    pub fn namespace(&self) -> &str {
        match self {
            Grant::Namespace { namespace } | Grant::Method { namespace, .. } => namespace,
            Grant::Storage { .. } | Grant::StorageMethod { .. } => "kv",
        }
    }

    /// The storage this grant names, if it names one.
    pub fn storage(&self) -> Option<&str> {
        match self {
            Grant::Storage { storage } | Grant::StorageMethod { storage, .. } => Some(storage),
            _ => None,
        }
    }

    /// The verb this grant names, or `None` when it grants the whole namespace.
    pub fn verb(&self) -> Option<&str> {
        match self {
            Grant::Method { verb, .. } | Grant::StorageMethod { verb, .. } => Some(verb),
            Grant::Namespace { .. } | Grant::Storage { .. } => None,
        }
    }

    /// Whether `call` — a full call name — is one this grant permits.
    ///
    /// The one function every surface uses, so that a `services` list and its expansion into a
    /// registry cannot disagree about what was granted.
    pub fn covers(&self, call: &str) -> bool {
        match self {
            Grant::Namespace { namespace } => {
                if namespace == "kv" {
                    // A bare `"kv"` is refused by `validate_services` rather than resolved, because
                    // it does not say which storage — so there is nothing for it to cover here.
                    return false;
                }
                call.split_once('.')
                    .is_some_and(|(namespace_of_call, _)| namespace_of_call == namespace)
            }
            Grant::Method { namespace, verb } if namespace == "kv" => {
                // `kv.get`: the verb in the last segment, any storage — so the call is exactly
                // `kv.<storage>.<verb>` and nothing longer.
                match call.rsplit_once('.') {
                    Some((head, call_verb)) => {
                        call_verb == verb
                            && head.split_once('.').is_some_and(|(ns, rest)| {
                                ns == "kv" && !rest.is_empty() && !rest.contains('.')
                            })
                    }
                    None => false,
                }
            }
            Grant::Method { namespace, verb } => call
                .split_once('.')
                .is_some_and(|(ns, call_verb)| ns == namespace && call_verb == verb),
            Grant::Storage { storage } => {
                // A storage-wide grant is the three-part `kv.<storage>.<verb>` shape, so a call
                // reaches it only when the middle segment is this storage's name. The histogram's
                // `_bucket`/`_sum`/`_count` suffixes are exposition spellings rather than call
                // names, so they are not matched here.
                call.split_once('.').is_some_and(|(namespace, rest)| {
                    namespace == "kv" && rest.starts_with(&format!("{storage}."))
                })
            }
            Grant::StorageMethod { storage, verb } => match call.rsplit_once('.') {
                Some((head, call_verb)) => call_verb == verb && head == format!("kv.{storage}"),
                None => false,
            },
        }
    }
}

/// Whether a `services` name is spelled in a way [`Grant::parse`] understands.
pub fn is_a_grant_spelling(service: &str) -> bool {
    Grant::parse(service).is_some()
}
