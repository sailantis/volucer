//! What a rolling restart of application code did, and why it left anything alone.
//!
//! A sweep replaces the *processes* that run an application, keeping every address, so a
//! deployment can put new code behind a socket that never closes. It deliberately does not
//! promise to finish in one pass: a slot serving a request is left where it is, because the
//! alternative — replacing it anyway — truncates that request, and a truncated request is
//! indistinguishable from a bug in the application.
//!
//! So a sweep reports three things rather than a boolean, and the operator can then tell a
//! complete roll from a partial one. That distinction is the whole reason this type exists: a
//! rolling restart that silently replaced two of eight workers and said nothing would be worse
//! than no rolling restart at all, because the deployment would look done.

/// The slots a pool cannot replace, and why volucer is not the tool for them.
///
/// A reason rather than a bare count, because "skipped" on its own reads like a failure. These
/// are not failures: an `attach` pool's processes belong to somebody else's supervisor, and an
/// `external` pool's belong to `php-fpm`. Naming the tool that *does* own them is what turns a
/// no-op into an instruction.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Unmanaged {
    /// How many slots this covers, so a log line can say eight and not "some".
    pub slots: usize,
    /// What owns them, and therefore what to do instead.
    pub reason: String,
}

/// One backend's share of a rolling restart.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct RespawnReport {
    /// The backend this is about, which is the name the configuration gave it.
    pub backend: String,
    /// The slots that now run a fresh worker, by index **within the pool**.
    ///
    /// The pool's own numbering rather than the global one `workers.list` prints, because a
    /// sweep is per pool and the index is then matched against the same pool's log lines.
    pub replaced: Vec<usize>,
    /// The slots this sweep left alone, because a request either held one or was waiting for
    /// one when the sweep reached it.
    ///
    /// These are the gap the next sweep closes. Worth reporting separately from [`Self::failed`]
    /// and from [`Self::unmanaged`] because it is the only one of the three that is neither a
    /// problem nor a permanent state: it is a roll that is merely not finished.
    pub skipped_busy: Vec<usize>,
    /// The slots whose replacement could not be booted.
    ///
    /// A slot that fails here is left with no worker in it, which the next request through that
    /// slot repairs on its own — so this is a failure to *report*, not one to retry inside the
    /// sweep. Retrying would mean a sweep that can take as long as a broken application takes to
    /// fail, per slot, in series.
    pub failed: Vec<usize>,
    /// Set when volucer cannot replace any of this backend's slots at all.
    ///
    /// Mutually exclusive with the three lists above: a pool either owns its processes or it
    /// does not, so a report that had both would be describing two backends at once.
    pub unmanaged: Option<Unmanaged>,
}

impl RespawnReport {
    /// An empty report for a named backend, ready to be filled in.
    #[must_use]
    pub fn new(backend: impl Into<String>) -> Self {
        RespawnReport {
            backend: backend.into(),
            ..RespawnReport::default()
        }
    }

    /// A report for a backend whose processes volucer does not run.
    #[must_use]
    pub fn unmanaged_by(
        backend: impl Into<String>,
        slots: usize,
        reason: impl Into<String>,
    ) -> Self {
        RespawnReport {
            backend: backend.into(),
            unmanaged: Some(Unmanaged {
                slots,
                reason: reason.into(),
            }),
            ..RespawnReport::default()
        }
    }

    /// Whether this sweep left nothing behind — every slot it owns now runs fresh code.
    ///
    /// `false` for an unmanaged pool on purpose. An operator asking "did the deploy land" must
    /// not be told yes by a sweep that replaced nothing because nothing was volucer's to replace.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.unmanaged.is_none() && self.skipped_busy.is_empty() && self.failed.is_empty()
    }

    /// Whether this sweep changed anything at all, which is the question "should this be logged".
    #[must_use]
    pub fn is_noop(&self) -> bool {
        self.replaced.is_empty()
    }

    /// One line describing the sweep, for the log and for a status answer's message.
    #[must_use]
    pub fn summary(&self) -> String {
        if let Some(unmanaged) = &self.unmanaged {
            return format!(
                "{}: no slot was replaced; {}",
                self.backend, unmanaged.reason
            );
        }
        let total = self.replaced.len() + self.skipped_busy.len() + self.failed.len();
        let mut line = format!("{}: {} of {total} replaced", self.backend, self.replaced.len());
        if !self.skipped_busy.is_empty() {
            line.push_str(&format!(", {} busy", self.skipped_busy.len()));
        }
        if !self.failed.is_empty() {
            line.push_str(&format!(", {} failed", self.failed.len()));
        }
        line
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_finished_sweep_is_complete_and_a_partial_one_is_not() {
        let mut report = RespawnReport::new("app");
        assert!(report.is_complete(), "a pool with no slots is trivially done");
        assert!(report.is_noop());

        report.replaced.push(0);
        report.replaced.push(1);
        assert!(report.is_complete());
        assert!(!report.is_noop());

        report.skipped_busy.push(2);
        assert!(
            !report.is_complete(),
            "a slot left for the next sweep is a roll that is not finished"
        );
        assert_eq!("app: 2 of 3 replaced, 1 busy", report.summary());
    }

    #[test]
    fn a_failure_makes_a_sweep_incomplete_and_is_named() {
        let report = RespawnReport {
            backend: "app".to_string(),
            replaced: vec![0],
            failed: vec![1],
            ..RespawnReport::default()
        };
        assert!(!report.is_complete());
        assert_eq!("app: 1 of 2 replaced, 1 failed", report.summary());
    }

    #[test]
    fn an_unmanaged_pool_is_never_complete_and_says_who_owns_it() {
        let report = RespawnReport::unmanaged_by("jobs", 4, "php-fpm owns this pool");
        assert!(
            !report.is_complete(),
            "a sweep that replaced nothing must not read as a finished deploy"
        );
        assert!(report.is_noop());
        assert_eq!(
            "jobs: no slot was replaced; php-fpm owns this pool",
            report.summary()
        );
    }
}
