//! The Redis driver: the same [`Driver`] over a server another process owns.
//!
//! # The data model, and why it is three keys rather than one
//!
//! A pipeline is a *set* of ready jobs and a *hash* of reservations. They cannot be one key, and the
//! reason is the operation that has to be atomic:
//!
//! ```text
//!   volucer:jobs:<pipeline>:ready          LIST  — job ids, oldest first
//!   volucer:jobs:<pipeline>:job:<id>       HASH  — task, payload, headers, attempts, pushed_at
//!   volucer:jobs:<pipeline>:reserved       HASH  — reservation → id + deadline
//!   volucer:jobs:<pipeline>:deadline:<id>  ZSET  — id → absolute ms, so an expired reservation can be
//!                                                  found without scanning the hash
//! ```
//!
//! **A claim is a `LMOVE`-like script, not a `LPOP` and a `HSET`.** The two-step version has a window in
//! which a job is in neither the ready list nor the reserved map — so a consumer crash inside it loses
//! the job with nothing recording that it ever existed. One script makes "take the id off the ready list
//! and write the reservation" a single operation, which is the same argument the lock driver's Lua makes
//! and the reason this crate has a `script` feature at all.
//!
//! **The deadline is a sorted set rather than a field read back and compared.** A reservation has to be
//! reclaimable when its lease expires, and finding those by scanning the reserved hash is O(reservations)
//! *per reserve* — on the path where a consumer with an empty queue is asking most often. A `ZSET` makes
//! it `ZRANGEBYSCORE -inf now`, which is the query the index exists for.
//!
//! **Expiry is arithmetic on the way into a reserve**, exactly as the memory driver's is, and that is a
//! correction this crate already made once: a rate-limited sweep makes a job's *availability* depend on
//! when the last sweep ran. Here the arithmetic is a `ZRANGEBYSCORE` at the top of the reserve, so a
//! reclaimable job is available on the very call that notices it.
//!
//! # Clocks
//!
//! Deadlines are absolute milliseconds from **Redis's own `TIME`**, never from this process. Two servers
//! whose clocks disagree would otherwise disagree about whether a lease has expired, and the disagreement
//! is silent: the job is simply handed to two consumers, or to none.
//!
//! # What is *not* done here
//!
//! No `WATCH`/`MULTI` transaction, no `XADD` stream, no keyspace notification. Every operation that has
//! to be atomic is one script and every operation that does not is one command — which is what keeps the
//! cost of a push at one round trip and a reserve at two (`TIME` for the deadline, then the script).
//! Redis Cluster is not supported and is not claimed to be: the keys of one pipeline share no hash tag,
//! so a cluster would refuse the script that spans them. That is a decision rather than an oversight.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::{Driver, Error, Job, Pushed, Reserved, Result};

/// How many connections a pool holds unless configured otherwise.
///
/// The same value as the KV store's and the lock backend's, and for the same reason: one per worker is
/// the natural size, since a reserve blocks the poller that issued it.
pub const DEFAULT_POOL_SIZE: usize = 16;

/// How long one command may take before it is treated as failed.
///
/// Five seconds, matching the other two Redis drivers. A driver command is on a consumer's poll path,
/// where there is no client waiting on the other end of it, so this bounds how long a stalled poller
/// stalls rather than how long a request may take.
pub const DEFAULT_READ_TIMEOUT: Duration = Duration::from_secs(5);

/// The prefix every key this driver makes carries.
///
/// Fixed rather than configurable, matching the lock driver's `volucer:lock:`. A pipeline name is the
/// application's; a second prefix would be a second thing to get right before two deployments sharing
/// one Redis could be told apart — and here a shared Redis is the normal case rather than the exception.
pub const PREFIX: &str = "volucer:jobs:";

/// The fields a stored job's hash carries.
///
/// Named constants rather than literals because they are written by `push` and read by `reserve`, in two
/// different functions and two different scripts. A renamed field would be a job whose payload silently
/// came back empty.
mod field {
    pub const TASK: &str = "task";
    pub const PAYLOAD: &str = "payload";
    pub const HEADERS: &str = "headers";
    pub const ATTEMPTS: &str = "attempts";
    pub const PUSHED_AT: &str = "pushed_at";
}

/// Take one job off the ready list and reserve it, in one operation.
///
/// # Why the whole claim is here and not in Rust
///
/// `LPOP` then `HSET` has a window where the job exists in neither structure. A consumer killed inside
/// that window loses the job with nothing anywhere recording that it was ever taken — not the ready list,
/// not the reserved map — which is the one failure a queue must not have.
///
/// # What it does, in order
///
/// 1. **Reclaims expired reservations.** `ZRANGEBYSCORE` finds the ids whose deadline has passed, and each
///    one is removed from the reserved map, removed from the index, and pushed back **to the front** of
///    the ready list — so a job whose consumer died long ago is handed out before one that died just now,
///    and both before work that arrived in the meantime. This is the memory driver's rule, kept.
/// 2. **Pops one id**, or returns nil when the list is empty.
/// 3. **Writes the reservation** — the id as the field, `"<id>|<nonce>"` as the value, the deadline into
///    the index — and increments the job's attempt count.
/// 4. **Answers the job**, id first, so a reserve is one round trip rather than a pop followed by five
///    reads.
///
/// The field of the reservation map is the **job id** and the value is the reservation the caller is
/// given, which is what makes a settle checkable: a worker whose lease expired asks to settle a
/// reservation the map no longer holds, and the script answers "not yours" rather than settling a job a
/// second consumer is running.
///
/// # The list's two ends, and which is which
///
/// **Push at the tail (`RPUSH`), take from the head (`LPOP`)** — that is FIFO, and it is the one property an
/// application relies on when it says "in the order they were pushed". The first version of this script
/// popped with `RPOP`, which made the queue a stack: a deployment would have handed out its newest job
/// first, and nothing about that looks like a failure until an application depends on ordering.
///
/// Reclamation and requeueing both use **`LPUSH`**, which puts a job back at the *head* — the end the pop
/// takes from — so a reclaimed job is the very next one handed out. That is deliberate for both: a job
/// whose consumer died is the most overdue work in the queue, and it is what makes the reclamation visible
/// on the same reserve that performs it.
///
/// The job's own key is built from `ARGV[4]`, because Redis's scripting is only allowed to construct keys
/// from arguments outside cluster mode — and this driver does not support cluster mode, which
/// is a decision rather than an oversight. Declaring the key as `KEYS[4]` would have been wrong:
/// the id is only known *after* the pop, so the key cannot exist before the script runs.
const RESERVE_SCRIPT: &str = r#"
local ready = KEYS[1]
local reserved = KEYS[2]
local deadlines = KEYS[3]
local now = tonumber(ARGV[1])
local nonce = ARGV[2]
local deadline_ms = tonumber(ARGV[3])
local job_prefix = ARGV[4]

local expired = redis.call('ZRANGEBYSCORE', deadlines, '-inf', now)
for _, expired_id in ipairs(expired) do
    redis.call('ZREM', deadlines, expired_id)
    redis.call('HDEL', reserved, expired_id)
    redis.call('LPUSH', ready, expired_id)
end

local id = redis.call('LPOP', ready)
if not id then
    return nil
end

local reservation = id .. '|' .. nonce
redis.call('HSET', reserved, id, reservation)
redis.call('ZADD', deadlines, deadline_ms, id)

local key = job_prefix .. id
local attempts = redis.call('HINCRBY', key, 'attempts', 1)
local task = redis.call('HGET', key, 'task')
local payload = redis.call('HGET', key, 'payload')
local headers = redis.call('HGET', key, 'headers')
local pushed_at = redis.call('HGET', key, 'pushed_at')
return { id, task, payload, headers, tostring(attempts), pushed_at, reservation }
"#;

/// Settle a reservation, in one operation.
///
/// `mode` is `ack` or `nack`, and the *reservation is checked* — both settle the job only if the caller
/// holds the reservation the map records. That check is what makes a worker whose lease expired unable to
/// acknowledge the job that has since been handed to somebody else.
///
/// A `nack` pushes the id back **to the end** of the ready list rather than the front: a job that just
/// failed should not jump ahead of work that has been waiting, and a job that fails deterministically
/// would otherwise starve everything behind it — a hot loop rather than a retry.
///
/// `ARGV[4]` is a boolean-ish flag for whether to delete the job's hash, passed rather than derived from
/// `mode` so that a settle does not have to know which mode means which — and so the job key, which is
/// built from `ARGV[5]`, is only touched on the one path that needs it.
const SETTLE_SCRIPT: &str = r#"
local ready = KEYS[1]
local reserved = KEYS[2]
local deadlines = KEYS[3]
local id = ARGV[1]
local reservation = ARGV[2]
local requeue = ARGV[3] == 'requeue'
local job_prefix = ARGV[4]

local held = redis.call('HGET', reserved, id)
if not held or held ~= reservation then
    return 0
end

redis.call('HDEL', reserved, id)
redis.call('ZREM', deadlines, id)

if requeue then
    redis.call('RPUSH', ready, id)
    return 1
end

redis.call('DEL', job_prefix .. id)
return 1
"#;

/// Locks and queues in Redis, shared between processes.
pub struct RedisDriver {
    /// The URL, kept so a connection can be re-made when one fails.
    url: String,
    /// The pipelines this driver serves, by name. What a pipeline *is* to this driver is a key prefix —
    /// the name is in every key, so two pipelines are two independent queues sharing one server.
    pipelines: BTreeMap<String, ()>,
    /// The pool, taken round-robin. The `Mutex` is held for one command, and no command is ever replayed
    /// on it.
    pool: Vec<std::sync::Mutex<redis::Connection>>,
    next: AtomicUsize,
    read_timeout: Duration,
    /// A counter, so an id is unique within a pipeline without a read.
    ///
    /// Combined with the process id for the same reason the memory driver's is: an id is read in a log
    /// and typed into a diagnostic, and a bare counter would repeat in every server. Not a UUID, because
    /// thirty-two characters of hex is worse at being read than a short number that happens to be unique.
    sequence: std::sync::atomic::AtomicU64,
}

impl std::fmt::Debug for RedisDriver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Hand-written because `redis::Connection` is not `Debug`. The URL is deliberately **not**
        // printed: it may carry a password, and a `Debug` that printed it would put a credential into
        // whatever log line or panic message happened to format this.
        f.debug_struct("RedisDriver")
            .field("pipelines", &self.pipelines.keys().collect::<Vec<_>>())
            .field("connections", &self.pool.len())
            .field("read_timeout", &self.read_timeout)
            .finish()
    }
}

impl RedisDriver {
    /// Connect a pool to Redis, and check the pipelines exist as queues only in the sense that they have
    /// a name — nothing is created on the server until the first push.
    ///
    /// Connects eagerly and `PING`s once, so an unreachable server — or a URL that happens to point at
    /// something which is not Redis — is a startup failure rather than a request that finds out. The
    /// worker pool, the KV store and the lock backend all follow the same rule for the same reason: a
    /// misconfiguration belongs where the configuration is read.
    pub fn connect(
        url: &str,
        pool_size: usize,
        read_timeout: Duration,
        pipelines: impl IntoIterator<Item = String>,
    ) -> Result<Self> {
        let pool_size = pool_size.max(1);
        let read_timeout = if read_timeout.is_zero() {
            DEFAULT_READ_TIMEOUT
        } else {
            read_timeout
        };

        let mut pool = Vec::with_capacity(pool_size);
        for _ in 0..pool_size {
            pool.push(std::sync::Mutex::new(Self::dial(url, read_timeout)?));
        }

        let driver = RedisDriver {
            url: url.to_string(),
            pipelines: pipelines
                .into_iter()
                .map(|name| (name, ()))
                .collect::<BTreeMap<_, _>>(),
            pool,
            next: AtomicUsize::new(0),
            read_timeout,
            sequence: std::sync::atomic::AtomicU64::new(0),
        };
        driver.ping()?;
        tracing::info!(
            pipelines = driver.pipelines.len(),
            connections = pool_size,
            read_timeout_ms = read_timeout.as_millis() as u64,
            "redis queue driver ready"
        );
        Ok(driver)
    }

    /// One connection to the server, with the command deadline already applied.
    ///
    /// The URL is never included in a message: it may carry a password, and an error string ends up in a
    /// log.
    fn dial(url: &str, read_timeout: Duration) -> Result<redis::Connection> {
        let client = redis::Client::open(url).map_err(|err| Error::Backend {
            driver: "redis",
            reason: format!("the Redis URL is not usable: {err}"),
        })?;
        let connection = client.get_connection().map_err(|err| Error::Backend {
            driver: "redis",
            reason: format!("could not connect: {err}"),
        })?;
        // Set on the connection rather than passed per command: the synchronous client has no
        // per-command deadline, and this is the setting that makes a hung server produce an error the
        // caller can act on instead of a blocked poller.
        connection
            .set_read_timeout(Some(read_timeout))
            .map_err(|err| Error::Backend {
                driver: "redis",
                reason: format!("could not set the command deadline: {err}"),
            })?;
        Ok(connection)
    }

    /// Run one command on a pooled connection.
    ///
    /// A dropped connection is repaired *in place* after a failure, so the next caller finds a working
    /// connection rather than meeting the same one. Whether the **failing command is replayed** is the
    /// caller's decision and not this helper's, which is the one place this deliberately differs from
    /// `volucer_kv`'s helper: a `GET` is idempotent, a `push` is not, and a retry of a reserve could hand
    /// the same job to two consumers. Nothing here replays anything.
    fn with_connection<T>(
        &self,
        operation: &'static str,
        run: impl Fn(&mut redis::Connection) -> redis::RedisResult<T>,
    ) -> Result<T> {
        let index = self.next.fetch_add(1, Ordering::Relaxed) % self.pool.len();
        let mut connection = self.pool[index]
            .lock()
            // A poisoned lock is one a previous call panicked while holding. Recovering is right: the
            // guarded value is a socket, no panic can leave it in a state a redial cannot fix, and
            // refusing every later queue operation for the rest of the process's life is the worse
            // failure.
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        match run(&mut connection) {
            Ok(value) => Ok(value),
            Err(err) => {
                if err.is_connection_dropped() || err.is_io_error() {
                    tracing::warn!(
                        operation,
                        %err,
                        "a redis queue connection failed; repairing the slot without replaying the command"
                    );
                    *connection = Self::dial(&self.url, self.read_timeout)?;
                }
                Err(Error::Backend {
                    driver: "redis",
                    reason: format!("{operation} failed: {err}"),
                })
            }
        }
    }

    /// Ping the server once, so an unusable URL is a startup error.
    fn ping(&self) -> Result<()> {
        self.with_connection("PING", |connection| {
            redis::cmd("PING").query::<String>(connection)
        })
        .map(|_| ())
    }

    /// The ready list's key.
    pub fn ready_key(pipeline: &str) -> String {
        format!("{PREFIX}{pipeline}:ready")
    }

    /// The reservations map's key.
    pub fn reserved_key(pipeline: &str) -> String {
        format!("{PREFIX}{pipeline}:reserved")
    }

    /// The deadline index's key.
    pub fn deadlines_key(pipeline: &str) -> String {
        format!("{PREFIX}{pipeline}:deadlines")
    }

    /// One job's hash key.
    pub fn job_key(pipeline: &str, id: &str) -> String {
        format!("{}{}", Self::job_prefix(pipeline), id)
    }

    /// The prefix a job's own key is built from.
    ///
    /// Separate from [`RedisDriver::job_key`] because the reserve script builds the key *inside* Redis,
    /// where the id is only known after the pop — so the prefix travels as an argument and the id is
    /// appended there. One definition of the prefix, used from both sides.
    pub fn job_prefix(pipeline: &str) -> String {
        format!("{PREFIX}{pipeline}:job:")
    }

    /// Redis's own `TIME`, in milliseconds since the epoch.
    ///
    /// Read from the server rather than computed locally because the deadline is the server's to
    /// enforce — see the module documentation. `SystemTime` is used only when a *producer* records
    /// `pushed_at`, which is a diagnostic rather than a decision.
    fn now_millis(connection: &mut redis::Connection) -> redis::RedisResult<i64> {
        let (seconds, micros): (i64, i64) = redis::cmd("TIME").query(connection)?;
        Ok(seconds * 1000 + micros / 1000)
    }

    /// The next job id.
    ///
    /// The process id and a counter, matching the memory driver's shape so that an operator moving from
    /// one driver to the other reads the same kind of id. The reservation id is a *different* value and
    /// is deliberately not derived from this one: acking by job id would let a worker whose lease expired
    /// settle the job a second worker is running.
    fn next_id(&self) -> String {
        let sequence = self
            .sequence
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        format!("{:x}-{:x}", std::process::id(), sequence)
    }

    /// Every header as `name\nvalue\nname\nvalue`, because a Redis hash field is one string.
    ///
    /// A separator that cannot appear in a header name or value is what makes this reversible without
    /// escaping, and `\n` is the one such byte: the protocol's `Pushed::validate` refuses a control
    /// character in a *task*, and this refuses one in a header name or value for the same reason —
    /// an application that sent one would be unable to read its own header back.
    fn encode_headers(headers: &BTreeMap<String, String>) -> String {
        let mut encoded = String::new();
        for (name, value) in headers {
            encoded.push_str(name);
            encoded.push('\n');
            encoded.push_str(value);
            encoded.push('\n');
        }
        encoded
    }

    /// The inverse of [`RedisDriver::encode_headers`].
    ///
    /// A malformed pair is **dropped rather than failing the reserve**, which is the same rule the SDK's
    /// consumer applies: a header an application cannot read is a bug in the producer, and failing every
    /// job in the pipeline over one malformed value would be a worse one. The job itself is still
    /// delivered, so the work happens.
    fn decode_headers(encoded: &str) -> BTreeMap<String, String> {
        let mut headers = BTreeMap::new();
        let mut lines = encoded.split('\n');
        while let (Some(name), Some(value)) = (lines.next(), lines.next()) {
            if name.is_empty() {
                // The trailing separator produces one empty name, and a `name\n\n` pair is not a header.
                continue;
            }
            headers.insert(name.to_string(), value.to_string());
        }
        headers
    }
}

impl Driver for RedisDriver {
    fn name(&self) -> &'static str {
        "redis"
    }

    fn push(&self, pipeline: &str, job: Pushed) -> Result<String> {
        job.validate(pipeline)?;
        crate::pipeline(&self.pipelines, pipeline)?;
        // The one constraint Redis adds that the memory driver does not impose, and it is stated rather
        // than discovered: a Redis hash field is a string, so a payload that is not valid UTF-8 cannot
        // make the round trip. Refused at the *push* rather than mangled at the reserve, because a queue
        // that silently corrupted an application's bytes would be a queue that hands a consumer work it
        // cannot possibly interpret — and the failure would surface as a deserialization error in the
        // application, days later and nowhere near the cause.
        if std::str::from_utf8(&job.payload).is_err() {
            return Err(Error::BadJob {
                reason:
                    "has a payload that is not valid UTF-8, which a Redis hash field cannot hold"
                        .to_string(),
            });
        }

        let id = self.next_id();
        let pushed_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|since| since.as_millis() as i64)
            .unwrap_or(0);

        self.with_connection("push", |connection| {
            // One `MULTI` rather than a script, and this is the one place a transaction is the right
            // tool: nothing here *reads* anything, so there is no check-and-write to make atomic. What
            // has to be atomic is that the job exists before its id is on the ready list — a consumer
            // that popped an id whose hash was not written yet would take a job with no task name.
            let mut pipe = redis::pipe();
            pipe.atomic()
                .cmd("HSET")
                .arg(Self::job_key(pipeline, &id))
                .arg(field::TASK)
                .arg(&job.task)
                .arg(field::PAYLOAD)
                .arg(&job.payload)
                .arg(field::HEADERS)
                .arg(Self::encode_headers(&job.headers))
                .arg(field::ATTEMPTS)
                .arg(0)
                .arg(field::PUSHED_AT)
                .arg(pushed_at)
                .ignore()
                .cmd("RPUSH")
                .arg(Self::ready_key(pipeline))
                .arg(&id)
                .query::<()>(connection)
        })?;

        tracing::debug!(pipeline, id = %id, task = %job.task, "job pushed");
        Ok(id)
    }

    fn reserve(&self, pipeline: &str, max: usize, lease: Duration) -> Result<Vec<Reserved>> {
        crate::pipeline(&self.pipelines, pipeline)?;
        if max == 0 {
            return Ok(Vec::new());
        }

        let ready = Self::ready_key(pipeline);
        let reserved_key = Self::reserved_key(pipeline);
        let deadlines = Self::deadlines_key(pipeline);
        // The prefix a job's own key is built from inside the script. One string, because Redis's
        // scripting may only construct a key from an argument — see `RESERVE_SCRIPT`.
        let job_prefix = Self::job_prefix(pipeline);

        let mut out = Vec::new();
        for _ in 0..max {
            // A fresh nonce per reservation, not the id: the *field* is the job id and the value is this,
            // so that a settle can check "is this still the reservation the map holds?" without the
            // driver having to reconstruct anything. Re-using the job id as the value would make the
            // check always true, which is the check the whole two-value shape exists for.
            let nonce = self.next_id();
            let answer = self.with_connection("reserve", |connection| {
                let now = Self::now_millis(connection)?;
                let deadline_ms = now + lease.as_millis().max(1) as i64;
                redis::Script::new(RESERVE_SCRIPT)
                    .key(&ready)
                    .key(&reserved_key)
                    .key(&deadlines)
                    .arg(now)
                    .arg(&nonce)
                    .arg(deadline_ms)
                    .arg(&job_prefix)
                    .invoke::<Option<Vec<Option<String>>>>(connection)
            })?;

            let Some(fields) = answer else {
                // An empty ready list: the common case, and not an error.
                break;
            };
            if fields.len() < 7 {
                // The script answers seven fields or nil, so this would be a script and this code
                // disagreeing. Reported rather than silently producing a job with no task name.
                return Err(Error::Backend {
                    driver: "redis",
                    reason: format!(
                        "a reservation for pipeline {pipeline:?} answered {} fields instead of 7",
                        fields.len()
                    ),
                });
            }

            let id = fields[0].clone().unwrap_or_default();
            let task = fields[1].clone().unwrap_or_default();
            // The payload is stored as a Redis hash field, which is a string — so the bytes make the
            // round trip through UTF-8. A payload that is *not* valid UTF-8 would be mangled, which is
            // why `Pushed::validate` refuses one: a queue that silently corrupted an application's bytes
            // would be worse than one that refused them at the push.
            let payload = fields[2].clone().unwrap_or_default().into_bytes();
            let headers = Self::decode_headers(fields[3].as_deref().unwrap_or_default());
            let attempts = fields[4]
                .as_deref()
                .and_then(|value| value.parse::<u32>().ok())
                .unwrap_or(0);
            let pushed_at = fields[5]
                .as_deref()
                .and_then(|value| value.parse::<u64>().ok())
                .map(|millis| UNIX_EPOCH + Duration::from_millis(millis))
                .unwrap_or_else(SystemTime::now);
            // The reservation comes back from the script rather than being reconstructed here, so the
            // encoding has exactly one definition and a settle compares two values the same code made.
            let reservation = fields[6].clone().unwrap_or_default();

            out.push(Reserved {
                job: Job {
                    id,
                    task,
                    payload,
                    headers,
                    attempts,
                    pushed_at,
                },
                reservation,
                expires_at: SystemTime::now() + lease,
            });
        }
        Ok(out)
    }

    fn ack(&self, pipeline: &str, reservation: &str) -> Result<()> {
        self.settle(pipeline, reservation, "ack")
    }

    fn nack(&self, pipeline: &str, reservation: &str) -> Result<()> {
        self.settle(pipeline, reservation, "requeue")
    }

    fn size(&self, pipeline: &str) -> Result<usize> {
        crate::pipeline(&self.pipelines, pipeline)?;
        self.with_connection("size", |connection| {
            redis::cmd("LLEN")
                .arg(Self::ready_key(pipeline))
                .query::<usize>(connection)
        })
    }

    fn in_flight(&self, pipeline: &str) -> Result<usize> {
        crate::pipeline(&self.pipelines, pipeline)?;
        self.with_connection("in_flight", |connection| {
            let now = Self::now_millis(connection)?;
            // Expired reservations are not in flight — they are ready again, whatever the map still
            // says, and reporting them would make an operator scale up a queue that has nothing to run.
            redis::cmd("ZCOUNT")
                .arg(Self::deadlines_key(pipeline))
                .arg(format!("({now}"))
                .arg("+inf")
                .query::<usize>(connection)
        })
    }

    fn pipelines(&self) -> Vec<&str> {
        self.pipelines.keys().map(String::as_str).collect()
    }
}

impl RedisDriver {
    /// Settle one reservation, for `ack` and `nack` alike.
    ///
    /// Both are the same script with one argument different, because both have to make the same check —
    /// that the caller holds the reservation the map records — and differ only in whether the job's hash
    /// is deleted or the id pushed back.
    ///
    /// The reservation the worker was given is `"<id>|<nonce>"` and the map is keyed by the id, so a settle
    /// splits the two. That split is why the `Driver` boundary can keep `reservation: &str` opaque: a
    /// driver that changed its own encoding would not change the trait.
    fn settle(&self, pipeline: &str, reservation: &str, mode: &str) -> Result<()> {
        crate::pipeline(&self.pipelines, pipeline)?;
        let id = reservation_id(reservation);

        let settled = self.with_connection("settle", |connection| {
            redis::Script::new(SETTLE_SCRIPT)
                .key(Self::ready_key(pipeline))
                .key(Self::reserved_key(pipeline))
                .key(Self::deadlines_key(pipeline))
                .arg(&id)
                .arg(reservation)
                .arg(mode)
                .arg(Self::job_prefix(pipeline))
                .invoke::<i64>(connection)
        })?;

        if settled == 0 {
            // The check the script makes is the whole point of this arm: the caller does not hold the
            // reservation, so its lease expired and the job has been — or is about to be — handed to
            // somebody else. Reported as `Lost` rather than as a backend failure, because the outcome is
            // *known* and it is not "the job completed".
            return Err(Error::Lost {
                job: id,
                reservation: reservation.to_string(),
                reason: "its lease expired, so the job has been or will be handed to somebody else"
                    .to_string(),
            });
        }
        Ok(())
    }
}

/// The id a reservation names.
///
/// The reservation value the driver hands out is `"<id>|<deadline_ms>"`, and the *map* is keyed by the id
/// alone. Two values rather than one because they answer different questions: the id is what a settle
/// matches and the deadline is what an operator reading `HGETALL` sees without a second lookup. Splitting
/// here rather than storing two hashes keeps the reservation a single opaque string at the [`Driver`]
/// boundary, which is what lets a driver change its own encoding without changing the trait.
fn reservation_id(reservation: &str) -> String {
    reservation
        .split_once('|')
        .map(|(id, _)| id.to_string())
        .unwrap_or_else(|| reservation.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headers_round_trip_through_the_one_field_they_share() {
        let headers = [
            ("traceparent".to_string(), "00-abc-01".to_string()),
            ("retry-delay".to_string(), "30".to_string()),
        ]
        .into_iter()
        .collect::<BTreeMap<_, _>>();

        let encoded = RedisDriver::encode_headers(&headers);
        assert_eq!(
            encoded, "retry-delay\n30\ntraceparent\n00-abc-01\n",
            "a `BTreeMap` orders by name, so the encoding is stable and testable"
        );
        assert_eq!(
            RedisDriver::decode_headers(&encoded),
            headers,
            "and it is reversible, which is the whole point of the separator"
        );
        assert_eq!(
            RedisDriver::decode_headers(""),
            BTreeMap::new(),
            "no headers is an empty map, not something a reader has to special-case"
        );
        // A value containing the *other* candidate separators, because these are application strings and
        // `=` or `:` is exactly what a base64 or a URL-shaped header would contain.
        let awkward = [("x".to_string(), "a=b:c,d".to_string())]
            .into_iter()
            .collect::<BTreeMap<_, _>>();
        assert_eq!(
            RedisDriver::decode_headers(&RedisDriver::encode_headers(&awkward)),
            awkward,
            "the separator is a byte no header value may contain, so nothing needs escaping"
        );
    }

    #[test]
    fn a_reservation_splits_into_the_id_the_map_is_keyed_by() {
        assert_eq!(reservation_id("1a-3|1729000000000"), "1a-3");
        // A value with no separator is its own id rather than an empty one, which is the fail-safe
        // direction: a settle that matched an empty id would settle a *different* job.
        assert_eq!(reservation_id("1a-3"), "1a-3");
    }

    #[test]
    fn every_key_of_one_pipeline_is_derived_from_its_name() {
        assert_eq!(
            RedisDriver::ready_key("emails"),
            "volucer:jobs:emails:ready"
        );
        assert_eq!(
            RedisDriver::reserved_key("emails"),
            "volucer:jobs:emails:reserved"
        );
        assert_eq!(
            RedisDriver::job_key("emails", "1a-0"),
            "volucer:jobs:emails:job:1a-0"
        );
        assert_eq!(
            RedisDriver::job_prefix("emails"),
            "volucer:jobs:emails:job:",
            "the prefix the reserve script builds its key from is the same string `job_key` builds with"
        );
        // Two pipelines share no key, which is what makes them two queues on one server rather than one
        // queue with two names.
        assert_ne!(
            RedisDriver::ready_key("emails"),
            RedisDriver::ready_key("reports")
        );
    }
}
