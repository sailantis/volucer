//! The consumer side of the queues: reserving a job, handing it to a worker, and acting on what comes
//! back.
//!
//! # What this is, in one paragraph
//!
//! The consumer side of the queues, in one paragraph. A consumer is a persistent process that booted once and
//! is handed work repeatedly, which is exactly what [`WorkerPool`] already does — so there is no second
//! pool, no second supervision and no second recycling rule. This module owns the one thing the pool does
//! not have an opinion about: **which job, from which pipeline, and what the answer means for the queue.**
//!
//! ```text
//!   poller thread (one)
//!     ├─ reserve from each consumed pipeline         → Reserved
//!     ├─ pool.job(pipeline, job)                     → Ack        (the JOB/ACK exchange)
//!     └─ act on Ack.outcome:
//!          ack      → driver.ack(pipeline, reservation)
//!          requeue  → driver.nack(pipeline, reservation)
//!          reject   → driver.ack(pipeline, reservation)
//! ```
//!
//! # Why one poller thread and not one per pipeline
//!
//! Because the pool is the concurrency, and a poller's whole job is to keep it fed. One thread per pipeline
//! would be several threads competing for the same slots, and the interesting failure of that shape is not
//! slowness — it is that a thread which reserved a job and then waited for a slot holds a *reservation*
//! while it waits, so the lease runs down on a job nobody is running. One poller reserves only after it has
//! a slot to put the job on, which is what makes the lease mean what it says: the time a consumer spends
//! working, not the time it spends queueing.
//!
//! # Why the poll loop is a thread rather than a task
//!
//! The pool is synchronous by construction — a slot is a mutex and a pipe read is blocking — and the HTTP
//! listener already runs on its own `tokio` runtime in a thread of its own. A consumer thread beside the
//! listeners is the same shape, and it is what lets the whole consumer side work in a build with no async
//! runtime at all.
//!
//! # Fairness across pipelines
//!
//! The poller asks each consumed pipeline for `prefetch` jobs in rotation, and moves on **whether or not
//! it got any**. So a pipeline that is empty cannot stall a pipeline that is busy, and a pipeline that is
//! always busy cannot starve a quiet one — the round-robin is over *the ask*, not over the work. What it
//! does not do is interleave: a pipeline with a thousand ready jobs and one worker gets them one at a
//! time, in order, because that is what a consumer of a queue expects and what FIFO within a pipeline
//! means.
//!
//! # What is deliberately absent
//!
//! No retry count, no backoff, no dead-letter queue and no delay between attempts. The reason is
//! deliberate: RoadRunner leaves those to the application, and a server-side policy would make two
//! applications' semantics fight. What the server does provide is the *number* — `attempts` is on every
//! job, incremented by the driver on every reservation — so an application can implement whatever policy it
//! wants and be sure of the count.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use volucer_jobs::{Driver, Reserved};
use volucer_worker::JobOutcome;

use crate::dispatch::JobsPlane;
use crate::error::{Result, ServerError};
use crate::worker_pool::WorkerPool;

/// How long a poller sleeps when every consumed pipeline was empty.
///
/// A poller with nothing to do must not spin, and the two ways to avoid that are to block on the driver
/// (which `volucer_jobs::Driver` deliberately does not offer — a blocking pop is a broker-specific notion
/// and the memory driver has no lock to block on) or to back off. This is the back-off.
///
/// 10 ms is chosen against what it costs and what it costs to be wrong. Waking ten times a second per
/// server is free; a job that waits 10 ms longer than it had to is invisible next to the work it then does,
/// which for anything worth putting in a queue is measured in milliseconds at the very least. A shorter
/// interval would be a busy loop dressed up as responsiveness, and a longer one would make a deployment
/// with a quiet queue look slow for no reason.
const IDLE_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// How many jobs to reserve per pipeline at once, per poll.
///
/// The configuration's `prefetch` decides this in practice; this is the floor for a value the config
/// validator already refuses to let be zero. Named rather than inlined so the arithmetic reads as "at
/// least one" and the test above can state the rule without a live pool.
const MIN_PREFETCH: usize = 1;

/// Raise a configured prefetch to the floor.
///
/// A free function rather than a `max` at the call site so that the rule has one name and one test. `0` is
/// refused by `Config::validate` with a message naming the key, so this is unreachable from a file on
/// disk — it exists because a poller that reserved zero jobs for ever would be a consumer that reports
/// itself healthy and consumes nothing, and a bug of that shape is worth one line to make impossible.
fn prefetch_or_one(prefetch: usize) -> usize {
    prefetch.max(MIN_PREFETCH)
}

/// The consumer side, as one object.
///
/// Holds everything a poll needs and nothing else: the drivers to reserve from, the pool to hand jobs to,
/// and the two settings that decide how it behaves. Built once at startup and moved onto the poller thread.
pub struct Consumers {
    /// The consumers' worker pool. One pool for every consumed pipeline, which is what `[jobs.pool]`
    /// describes — one pool rather than one per pipeline, so that a pipeline without a consumer does
    /// not each want a pool of their own.
    pool: Arc<WorkerPool>,
    /// The pipelines to poll, each with its driver, in the configuration's order.
    pipelines: Vec<(String, Arc<dyn Driver>)>,
    /// How long a reservation lasts. The one setting that must exceed the time a job takes.
    lease: Duration,
    /// How many jobs to ask a pipeline for at once.
    prefetch: usize,
    /// Set when the server is shutting down, so the poller stops asking for work.
    ///
    /// Checked *between* jobs rather than during one: a poller that abandoned a job it had already reserved
    /// would leave it in flight until its lease ran out, which is the exact cost the lease exists to bound.
    /// So shutdown stops the *reserving*, and a job already handed to a consumer is waited for.
    stopping: Arc<AtomicBool>,
    /// Jobs handled, by outcome, for the shutdown report and for the metrics scrape.
    ///
    /// Three counters rather than one plus a label, because they are written from the poller thread on
    /// every job and read once at shutdown: an atomic increment is the whole cost of the write, where a
    /// registry lookup with a label would take a lock on the hot path of the consumer side.
    acked: AtomicU64,
    requeued: AtomicU64,
    rejected: AtomicU64,
}

impl std::fmt::Debug for Consumers {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Hand-written because `dyn Driver` is not `Debug`, and the names are the useful part.
        f.debug_struct("Consumers")
            .field(
                "pipelines",
                &self.pipelines.iter().map(|(n, _)| n).collect::<Vec<_>>(),
            )
            .field("lease", &self.lease)
            .field("prefetch", &self.prefetch)
            .field("stopping", &self.stopping.load(Ordering::Relaxed))
            .finish()
    }
}

impl Consumers {
    /// Build the consumer side from the queues a deployment built.
    ///
    /// Takes the pool by value, because **this is its only owner**: a consumer pool exists for the queue
    /// and for nothing else, so there is no second holder to keep it alive and no question about who stops
    /// it. Dropping the `Consumers` drops the pool, and `Drop for WorkerPool` is what reaps the processes.
    pub fn new(plane: &JobsPlane, pool: WorkerPool, lease: Duration, prefetch: usize) -> Self {
        Consumers {
            pool: Arc::new(pool),
            pipelines: plane
                .consumed()
                .into_iter()
                .map(|(name, driver)| (name.to_string(), driver))
                .collect(),
            lease,
            prefetch: prefetch_or_one(prefetch),
            stopping: Arc::new(AtomicBool::new(false)),
            acked: AtomicU64::new(0),
            requeued: AtomicU64::new(0),
            rejected: AtomicU64::new(0),
        }
    }

    /// A handle that stops the poller.
    ///
    /// A flag rather than a channel, and read between jobs rather than during one, for the reason on the
    /// field: a stop that interrupted a job in flight would make the graceful shutdown *cause* the lease
    /// expiry it exists to avoid.
    pub fn stopper(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.stopping)
    }

    /// Whether there is anything to poll.
    ///
    /// False is a deployment with no `consume` list: the queue exists to push to and this server is a
    /// producer. Reported so `serve` can say so once at startup rather than leaving an operator to wonder
    /// why nothing is being consumed.
    pub fn is_empty(&self) -> bool {
        self.pipelines.is_empty()
    }

    /// How many consumers are kept.
    pub fn len(&self) -> usize {
        self.pool.len()
    }

    /// The pipelines being polled, by name.
    pub fn pipelines(&self) -> Vec<&str> {
        self.pipelines
            .iter()
            .map(|(name, _)| name.as_str())
            .collect()
    }

    /// The pool the consumers live in, for a diagnostic and for a scrape.
    ///
    /// Reachable for the same reason a backend's pool is: the metrics a consumer pool publishes are the
    /// ones `[status.metrics]` renders, so the scrape path has to be able to reach the pool it is describing.
    /// A borrow rather than a clone, because holding an `Arc` would keep the consumer processes alive
    /// past the `Consumers` that owns them.
    pub fn pool(&self) -> &Arc<WorkerPool> {
        &self.pool
    }

    /// How many jobs have been acknowledged, requeued and rejected.
    pub fn counts(&self) -> (u64, u64, u64) {
        (
            self.acked.load(Ordering::Relaxed),
            self.requeued.load(Ordering::Relaxed),
            self.rejected.load(Ordering::Relaxed),
        )
    }

    /// Run one pass over the consumed pipelines.
    ///
    /// Returns how many jobs were handed to a consumer, which is what the poll loop uses to decide whether
    /// to sleep. Public rather than folded into `run`, because a test can then drive exactly one pass
    /// against a real pool and a real driver without a thread, a shutdown flag or a timing assumption —
    /// and the pass is where every decision in this module actually lives.
    pub fn poll_once(&self) -> Result<usize> {
        let mut handled = 0;
        for (name, driver) in &self.pipelines {
            if self.stopping.load(Ordering::Relaxed) {
                return Ok(handled);
            }
            let reserved = driver
                .reserve(name, self.prefetch, self.lease)
                .map_err(|source| ServerError::JobHandoff {
                    reason: format!("could not reserve from pipeline {name:?}: {source}"),
                })?;
            for job in reserved {
                self.deliver(name, driver.as_ref(), &job)?;
                handled += 1;
            }
        }
        Ok(handled)
    }

    /// Hand one reserved job to a consumer and act on the answer.
    ///
    /// **The whole ack/nack policy is in the match at the end**, and it is deliberately the only place it
    /// exists — see the module documentation for why the server imposes no retry policy, and
    /// [`JobOutcome`] for why `Reject` acknowledges rather than nacks.
    ///
    /// A failure to hand the job over at all is *not* an error the caller should propagate as fatal: it is
    /// reported, and the job goes back to its pipeline through the same `nack` a `Requeue` uses, because a
    /// reserved job that nobody runs and nobody returns is a job stuck until its lease expires. The
    /// distinction between the two failures is worth keeping: the pool being unable to serve is a
    /// capacity condition that the next poll may not have, while a handover failure means a consumer is
    /// gone.
    fn deliver(&self, pipeline: &str, driver: &dyn Driver, job: &Reserved) -> Result<()> {
        match self.pool.job(pipeline, &job.job) {
            Ok(ack) => {
                let action = match &ack.outcome {
                    // Done: the reservation is removed.
                    JobOutcome::Ack => driver.ack(pipeline, &job.reservation),
                    // Not done, and it should be tried again: negatively acknowledging returns the job to
                    // the pipeline, where its `attempts` count — already incremented by the reservation —
                    // is what an application's retry policy reads.
                    JobOutcome::Requeue { .. } => driver.nack(pipeline, &job.reservation),
                    // Not done, and it must not come back. **Acknowledged, not nacked**, because a job an
                    // application has decided against is not a job that is waiting: nacking it would bring
                    // it back for ever.
                    JobOutcome::Reject { .. } => driver.ack(pipeline, &job.reservation),
                };
                if let Err(source) = action {
                    // The work is done and the bookkeeping failed, which is the one failure in this
                    // module an operator has to hear about: the job will be handed out again when its
                    // lease expires, so it is about to run *twice*. Reported as a warning with the
                    // consequence spelled out rather than as an error the poller dies on.
                    tracing::warn!(
                        pipeline = %pipeline,
                        job = %job.job.id,
                        outcome = %ack.outcome.name(),
                        error = %source,
                        "a job was handled but its reservation could not be settled; the lease is the \
                         only thing that will release it, and the job may run again"
                    );
                    return Ok(());
                }
                match ack.outcome {
                    JobOutcome::Ack => &self.acked,
                    JobOutcome::Requeue { .. } => &self.requeued,
                    JobOutcome::Reject { .. } => &self.rejected,
                }
                .fetch_add(1, Ordering::Relaxed);

                if let Some(reason) = ack.outcome.reason() {
                    tracing::debug!(
                        pipeline = %pipeline,
                        job = %job.job.id,
                        task = %job.job.task,
                        outcome = %ack.outcome.name(),
                        reason,
                        "job settled"
                    );
                }
                Ok(())
            }
            Err(source) => {
                // Nothing ran, so nothing has to be undone — but the *reservation* does, or the job waits
                // for its lease. Nacked rather than acked: the job did not happen, so it should be handed
                // out again rather than considered done.
                tracing::warn!(
                    pipeline = %pipeline,
                    job = %job.job.id,
                    error = %source,
                    "a job could not be handed to a consumer; returning it to its pipeline"
                );
                driver
                    .nack(pipeline, &job.reservation)
                    .map_err(|source| ServerError::JobHandoff {
                        reason: format!(
                            "a job could not be handed to a consumer and could not be returned to \
                             pipeline {pipeline:?}: {source}"
                        ),
                    })
            }
        }
    }

    /// Poll until stopped.
    ///
    /// The loop the poller thread runs. One pass, then a sleep if the pass found nothing — so a busy queue
    /// is consumed as fast as the pool can take it, and an empty one costs ten wake-ups a second.
    pub fn run(&self) {
        tracing::info!(
            pipelines = ?self.pipelines(),
            lease_seconds = self.lease.as_secs(),
            prefetch = self.prefetch,
            "consuming jobs"
        );
        while !self.stopping.load(Ordering::Relaxed) {
            match self.poll_once() {
                Ok(0) => std::thread::sleep(IDLE_POLL_INTERVAL),
                Ok(_) => {}
                Err(err) => {
                    // A driver that cannot be reached is not a reason to stop consuming: the broker may be
                    // restarting, and the pipelines that are reachable should keep being served. The poll
                    // is retried after the idle interval, which also bounds how loud this is.
                    tracing::error!(%err, "a poll over the consumed pipelines failed");
                    std::thread::sleep(IDLE_POLL_INTERVAL);
                }
            }
        }
        tracing::info!("the job poller stopped");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The poll loop's back-off is a timing choice, not a correctness one, and this is what says so: the
    /// two values it has to sit between are "long enough not to spin" and "short enough that a job is not
    /// visibly delayed".
    #[test]
    fn the_idle_poll_interval_is_a_backoff_not_a_delay() {
        assert!(
            IDLE_POLL_INTERVAL >= Duration::from_millis(1),
            "a spin, not a sleep"
        );
        assert!(
            IDLE_POLL_INTERVAL <= Duration::from_millis(100),
            "a delay, not a backoff"
        );
    }

    #[test]
    fn a_prefetch_of_zero_is_raised_to_one_rather_than_reserving_nothing() {
        // `Config::validate` refuses `prefetch = 0` by name, so this is only reachable from a hand-built
        // value — and a poller that reserved zero jobs for ever is a consumer that looks healthy and
        // consumes nothing, which is worth making impossible rather than merely refused.
        assert_eq!(prefetch_or_one(0), 1);
        assert_eq!(prefetch_or_one(1), 1);
        assert_eq!(prefetch_or_one(50), 50);
    }
}
