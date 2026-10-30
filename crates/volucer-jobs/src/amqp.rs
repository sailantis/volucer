//! The AMQP driver: the same [`Driver`] over a broker another process owns.
//!
//! # What maps cleanly, and what does not
//!
//! AMQP 0-9-1 fits five of [`Driver`]'s eight methods well and two badly, and the honest thing is to say
//! which is which before the code, because the two bad ones are where a deployment's expectations will be
//! wrong. (`name` and `pipelines` are not in the table: both are a `BTreeMap` key listing.)
//!
//! | Method | Here |
//! |---|---|
//! | `push` | `basic_publish` under publisher confirms, so a push really is accepted or refused |
//! | `reserve` | one `basic_get` per job asked for |
//! | `ack` | `basic_ack` |
//! | `nack` | `basic_nack(requeue = true)` |
//! | `size` | a passive `queue_declare`'s ready count |
//! | `in_flight` | **this process's** unacked count, not the broker's |
//!
//! One field the trait promises is not a method at all, and the protocol cannot always supply it:
//! [`Job::attempts`] is exact on a quorum queue and coarse on a classic one — see its own section below.
//!
//! ## The lease is the connection's lifetime, not a deadline
//!
//! Every other driver here turns `reserve`'s `lease` into a deadline: a reservation whose time runs out
//! is available again, checked on the way into the next reserve. **AMQP has no such mechanism.** A
//! message handed out by `basic_get` is unacknowledged on a *channel*, and it comes back when that
//! channel or its connection closes — not when a timer expires.
//!
//! That is stronger in one way and weaker in another, and both matter:
//!
//! - **Stronger**: a consumer process that is killed has its TCP connection reset, and the broker
//!   requeues its messages immediately. There is no window in which a killed worker's job waits out a
//!   lease it will never honour.
//! - **Weaker**: a consumer that *hangs* — a blocked socket, a deadlock, a `while (true)` — stays
//!   connected, so its message stays unacked for as long as the process lives. `job_lease_seconds` does
//!   not bound that, and a deployment migrating from `driver = "redis"` must know it.
//!
//! So `lease` is accepted and used only as this process's own record rather than as a broker deadline.
//! Honouring it against the broker would need either a per-message TTL with a dead-letter topology the
//! application's queue would have to share, or a background timer calling `basic_recover` — and the
//! second would redeliver work that is still being done, which is precisely the double execution the
//! reservation model exists to prevent.
//!
//! ## `in_flight` is per process
//!
//! AMQP 0-9-1 has no method that reports a queue's unacknowledged count: `queue_declare` answers the
//! *ready* count. The management HTTP API has `messages_unacknowledged`, but depending on a second
//! protocol and a second port to answer one gauge is not a trade this driver makes.
//!
//! What it reports is the number of reservations **this process** is holding unacked. With one server
//! that is the true figure. With several sharing a queue it under-reports by however many the others
//! hold, and this paragraph is why the number is documented rather than trusted.
//!
//! ## `attempts` is exact only on a quorum queue
//!
//! The trait promises that `attempts` "is incremented by the driver on every reservation, not sent by
//! the producer". AMQP gives a consumer exactly one bit for this: `redelivered`. Measured against
//! RabbitMQ 4.4.0, a message nacked with requeue and taken three times reports `redelivered: true` on
//! the second and third deliveries and nothing else on a **classic** queue.
//!
//! A **quorum** queue is different: the broker injects `x-acquired-count` into the delivered headers,
//! counting *previous* acquisitions (measured: absent, then `1`, then `2`). Where that header is
//! present, this driver adds one to it — the number the trait means. Where it is absent the count is
//! `1` on the first delivery and `2` on every redelivery: true, but not a count.
//!
//! An application whose retry policy needs an exact count therefore needs a quorum queue, and that is a
//! queue argument (`x-queue-type = "quorum"`) declared through [`AmqpQueue::arguments`]. It is not
//! something this driver can impose, because a queue's type is fixed at declaration and belongs to
//! whoever declared it first.
//!
//! # Three measurements that shaped the code
//!
//! **A bogus delivery tag closes the channel.** `basic_ack` is a send, not a request — the client
//! cannot report the broker's complaint on the call that caused it. Measured: acking a tag that no
//! longer exists is *accepted* by the call, and every later operation on that channel then fails with
//! `channel closed`. So this driver never sends an ack for a reservation it did not issue: the
//! reservation string maps to a delivery tag in a map this process owns, and an unknown reservation is
//! [`Error::Lost`] without touching the socket. That is also the right answer for a reservation a
//! second server reclaimed — the case `Lost` exists for.
//!
//! **Confirms arrive cumulatively.** RabbitMQ answers three pipelined publishes with one
//! `ack tag=3 multiple=true`, not three acks. A driver waiting for "an ack whose tag is mine" would
//! hang on the second publish of a batch, so the confirmation is tracked as a high-water mark that
//! `multiple` advances.
//!
//! **An unroutable publish is returned *and* confirmed.** Measured: publishing `mandatory = true` to a
//! queue that does not exist produces a `basic.return` with `312 NO_ROUTE` and then a normal ack.
//! Reporting the ack alone would make a push to a deleted queue look successful, so a return inside the
//! wait window fails the push.
//!
//! # Why an owned runtime
//!
//! `amqprs` is async and this crate's [`Driver`] is synchronous — deliberately, because the consumer
//! poller is a plain thread and the `jobs` namespace runs on the worker exchange thread. So the driver
//! owns a `tokio` runtime and blocks on it.
//!
//! **That is safe only from a thread that is not itself driving async tasks.** Measured on this
//! machine: `Runtime::block_on` from a plain thread works, from a `spawn_blocking` thread works, and
//! from inside an async context **panics** with "Cannot start a runtime from within a runtime". The two
//! call sites in this tree are the poller thread and the exchange thread and neither is async — but a
//! future caller must not make one async, which is why this paragraph exists rather than a comment
//! saying "owned runtime".

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use amqp_serde::types::{FieldName, FieldTable, FieldValue};
use amqprs::{
    callbacks::{ChannelCallback, DefaultConnectionCallback},
    channel::{
        BasicAckArguments, BasicGetArguments, BasicNackArguments, BasicPublishArguments, Channel,
        ConfirmSelectArguments, QueueDeclareArguments,
    },
    connection::{Connection, OpenConnectionArguments},
    error::Error as AmqpError,
    Ack, BasicProperties, Cancel, CloseChannel, Nack, Return,
};
use async_trait::async_trait;

use crate::{Driver, Error, Job, Pushed, Reserved, Result};

/// Rewrite the two spellings of the default virtual host into the one `amqprs` understands.
///
/// `amqp://host/%2f` and `amqp://host/` both mean "the default virtual host, `/`". `amqprs` takes the
/// path after the slash verbatim, so it reads `%2f` as a literal name and the broker refuses it — and
/// reads an empty path as the empty string, which is also not the default. Measured against RabbitMQ
/// 4.4.0: `%2f` answers `530 NOT_ALLOWED - vhost %2f not found`, while a URL with no path at all
/// resolves to `/` and connects.
///
/// So both spellings are reduced to *no path*, which is the one form the parser handles correctly.
///
/// # What this deliberately does not do
///
/// It is not a percent-decoder. A virtual host whose real name contains `%2f` — which is possible — would
/// be rewritten by this, so such a deployment has to write its name some other way, and a general decode
/// would be a second URL parser whose bugs would be silent. Two spellings of one well-known value is a
/// substitution; anything more is a parser.
///
/// Public because it is a fact about the **client**, not about this driver: anything that opens a
/// connection from a configured URL — the integration tests included — needs the same rewrite, and a test
/// that re-implemented it would be testing its own copy of the bug.
pub fn normalise_default_vhost(url: &str) -> String {
    // The path starts at the first `/` after the scheme's `://` and its authority. Splitting on the
    // scheme is what keeps a `%2f` in the password (base64 is full of slashes and plus signs) from being
    // touched: only the path after the authority is rewritten.
    let Some(scheme_end) = url.find("://") else {
        return url.to_string();
    };
    let after_scheme = scheme_end + 3;
    let Some(path_start) = url[after_scheme..].find('/').map(|at| after_scheme + at) else {
        // No path at all, which is already the correct form.
        return url.to_string();
    };
    let path = &url[path_start..];
    if path == "/" || path.eq_ignore_ascii_case("/%2f") || path.eq_ignore_ascii_case("/%2F") {
        return url[..path_start].to_string();
    }
    // Any other path is the operator's own virtual host name, left exactly as written.
    url.to_string()
}

/// The header a job's task name travels in.
///
/// A header rather than the routing key, because the routing key is what the *exchange* routes on and
/// an application that binds a topic exchange must be free to use it. A header rather than a prefixed
/// payload, because the payload is the application's and must stay opaque.
pub const TASK_HEADER: &str = "volucer-task";

/// The header written on push recording that the producer claimed no attempts.
///
/// Written for a consumer that reads a message straight off the broker without this driver, so that
/// "how many times has this run" has an answer in the message itself rather than only in the code that
/// reserved it. Never trusted on the way back in — see [`ACQUIRED_COUNT_HEADER`].
pub const ATTEMPTS_HEADER: &str = "volucer-attempts";

/// The header RabbitMQ adds to every delivery from a **quorum** queue.
///
/// Not written by anything here — it appears on delivery, counting previous acquisitions. See the
/// module documentation for why `attempts` is exact only where this is present.
pub const ACQUIRED_COUNT_HEADER: &str = "x-acquired-count";

/// How long a broker round trip may take before it is treated as failed.
///
/// Five seconds, matching every other driver in this tree. A queue command is on a poll path, where
/// this bounds how long a stalled poller stalls rather than how long a request may take.
pub const DEFAULT_COMMAND_TIMEOUT: Duration = Duration::from_secs(5);

/// One pipeline, as this driver needs it.
#[derive(Debug, Clone)]
pub struct AmqpQueue {
    /// The queue name. Defaults to the pipeline's own name.
    pub queue: String,
    /// The exchange to publish to. `""` is the default exchange, which routes by queue name.
    pub exchange: String,
    /// The routing key. Defaults to the queue name, which is what the default exchange needs.
    pub routing_key: String,
    /// Whether the queue is declared durable. Ignored by a quorum queue, which is always durable.
    pub durable: bool,
    /// The queue arguments to declare with, passed through to the broker.
    ///
    /// This is where an application asks for `x-queue-type = "quorum"`, `x-message-ttl`,
    /// `x-dead-letter-exchange` or a priority bound. Volucer deliberately has no opinion about any of
    /// them: they are the application's semantics, and a server that imposed its own would be one whose
    /// queue behaves differently from the one the application declared.
    pub arguments: FieldTable,
}

impl AmqpQueue {
    /// A durable queue named after its pipeline, on the default exchange.
    pub fn new(pipeline: &str) -> Self {
        AmqpQueue {
            queue: pipeline.to_string(),
            exchange: String::new(),
            routing_key: pipeline.to_string(),
            durable: true,
            arguments: FieldTable::new(),
        }
    }
}

/// What the broker told us about a publish.
#[derive(Debug, Clone)]
enum BrokerEvent {
    /// A confirmation, or a range of them when `multiple` is set.
    Confirmed { tag: u64, multiple: bool },
    /// A refusal: the broker will not accept the message.
    Refused { tag: u64 },
    /// A message with no route. Only `mandatory` publishes are returned, so one here is a publish this
    /// driver made and must not report as accepted.
    Returned { code: u16, text: String },
}

/// The channel's callback, which is the only place a confirmation can arrive.
///
/// # Why it also records the broker's close reason
///
/// When the broker refuses a channel it sends a `close` with a reply code and text, and **that text is
/// the only useful explanation**. The client turns it into a generic `channel closed` on whichever call
/// happens to be next, so a driver that ignored the callback would report every failure as the same
/// unhelpful sentence — which is exactly what happened while this driver was being written: a
/// `queue_declare` that the broker refused for a specific reason came back as `channel closed`, with the
/// reason sitting in a callback nothing read.
///
/// So the reason is stored here and reported by whoever observes the dead channel.
struct Confirms {
    events: Mutex<tokio::sync::mpsc::UnboundedSender<BrokerEvent>>,
    /// Why the broker closed the channel, if it said — shared with the `Link` so a caller can read it.
    closed: Arc<Mutex<Option<String>>>,
}

#[async_trait]
impl ChannelCallback for Confirms {
    async fn close(
        &mut self,
        _channel: &Channel,
        close: CloseChannel,
    ) -> std::result::Result<(), AmqpError> {
        // The broker closed the channel. Recording the reason is what turns "channel closed" into
        // something an operator can act on; dropping the sender is what tells a waiting publish that no
        // confirmation can arrive, so a `recv` answers `None` rather than hanging.
        if let Ok(mut reason) = self.closed.lock() {
            *reason = Some(format!(
                "{} {}",
                close.reply_code(),
                close.reply_text()
            ));
        }
        Ok(())
    }

    async fn cancel(
        &mut self,
        _channel: &Channel,
        _cancel: Cancel,
    ) -> std::result::Result<(), AmqpError> {
        Ok(())
    }

    async fn flow(
        &mut self,
        _channel: &Channel,
        active: bool,
    ) -> std::result::Result<bool, AmqpError> {
        Ok(active)
    }

    async fn publish_ack(&mut self, _channel: &Channel, ack: Ack) {
        self.send(BrokerEvent::Confirmed {
            tag: ack.delivery_tag(),
            multiple: ack.mutiple(),
        });
    }

    async fn publish_nack(&mut self, _channel: &Channel, nack: Nack) {
        self.send(BrokerEvent::Refused {
            tag: nack.delivery_tag(),
        });
    }

    async fn publish_return(
        &mut self,
        _channel: &Channel,
        ret: Return,
        _properties: BasicProperties,
        _content: Vec<u8>,
    ) {
        self.send(BrokerEvent::Returned {
            code: ret.reply_code(),
            text: ret.reply_text().to_string(),
        });
    }
}

impl Confirms {
    fn send(&self, event: BrokerEvent) {
        // A closed receiver means the driver is gone, which is not worth logging: the connection is
        // being torn down and the answer is for a publish nobody is waiting on.
        if let Ok(sender) = self.events.lock() {
            let _ = sender.send(event);
        }
    }
}

/// The live connection, which is replaced whole when it dies.
///
/// # Why the `Drop` impl exists
///
/// `amqprs`' own `Drop` for a connection and for a channel calls `tokio::spawn` to send a close frame,
/// and **that panics when it runs outside a runtime**: measured, `thread 'main' panicked at
/// amqprs/src/api/channel/mod.rs: "there is no reactor running, must be called from the context of a
/// Tokio 1.x runtime"`. A `Channel` owns its `Connection`, so *any* drop of this struct from a plain
/// thread — a failed dial, a channel being replaced, the whole driver going out of scope at shutdown —
/// would take the process down with a panic in a destructor.
///
/// Entering the runtime around the drop is what fixes it, and it was verified rather than assumed:
/// dropping inside `runtime.enter()` succeeds where dropping outside it panics, and dropping the runtime
/// afterwards is safe.
struct Link {
    /// The connection the channel was opened on.
    ///
    /// Held rather than dropped for a reason that is not obvious and was measured: returning only the
    /// channel breaks it, because the next operation then fails with a bare `channel closed`. See the
    /// comment where it is constructed.
    ///
    /// An `Option` because the `Drop` impl has to take it, and that is not a tidiness choice — see there.
    connection: Option<Connection>,
    /// The channel, also taken by the `Drop` impl: it spawns a close on drop too.
    channel: Option<Channel>,
    events: tokio::sync::mpsc::UnboundedReceiver<BrokerEvent>,
    /// The highest publish tag the broker has confirmed, and the tag this driver last assigned.
    confirmed: u64,
    published: u64,
    /// The runtime, held so the `Drop` impl can enter it. An `Arc` clone rather than a reference because
    /// the drop has to be able to run at any point, including after the driver's own fields are gone.
    runtime: Arc<tokio::runtime::Runtime>,
    /// Why the broker closed this channel, recorded by the callback.
    ///
    /// Reported by whoever notices the channel is dead, because the client otherwise discards it and
    /// every failure reads as the same generic `channel closed`.
    closed: Arc<Mutex<Option<String>>>,
}

impl Drop for Link {
    fn drop(&mut self) {
        // A guard rather than `block_on`: the close frame is spawned, not awaited, so there is nothing to
        // block on — the runtime only has to be *entered* for `tokio::spawn` to have a context.
        let _guard = self.runtime.enter();
        // **Both are dropped here, inside the body, and that is the whole point.** A `Drop::drop` body
        // runs *before* the struct's fields are dropped, so a guard taken in the body would be gone by the
        // time a field's own `Drop` ran: measured, that is a panic at `amqprs`'s `tokio::spawn` with
        // "there is no reactor running", and in a destructor it is not a catchable error but a
        // `STATUS_STACK_BUFFER_OVERRUN` abort.
        //
        // **The channel first, then the connection**, because the channel's close is a frame sent over the
        // connection: dropping them the other way round would send that frame down a socket whose owner is
        // already gone.
        drop(self.channel.take());
        drop(self.connection.take());
    }
}

/// One job this process handed out and has not settled.
///
/// The delivery tag is what a settle actually needs, and the reservation string is what the trait hands
/// out. Keeping both is what makes a settle checkable **before** it reaches the socket, which the module
/// documentation explains is the difference between a refusal and a dead channel.
#[derive(Debug, Clone)]
struct Held {
    pipeline: String,
    tag: u64,
}

/// Queues in an AMQP broker, shared between processes.
pub struct AmqpDriver {
    /// The runtime the client's futures are driven on. See the module documentation for why an owned
    /// runtime is what makes an async client usable from this crate's synchronous trait.
    runtime: Arc<tokio::runtime::Runtime>,
    /// The URL, kept so a connection can be re-made when one fails. A URL rather than the parsed
    /// arguments, because `OpenConnectionArguments` is not `Clone` and re-making one from the URL is
    /// what guarantees a redial is the *same* connection rather than a reconstructed guess.
    url: String,
    /// The live connection, absent while it is being replaced.
    link: Mutex<Option<Link>>,
    /// Which pipelines exist, and how each is reached.
    pipelines: BTreeMap<String, AmqpQueue>,
    /// The reservations this process issued, so a settle is checked before it reaches the socket.
    held: Mutex<HashMap<String, Held>>,
    /// The command deadline.
    command_timeout: Duration,
    /// A counter, so a reservation is unique within this process without a round trip.
    sequence: AtomicU64,
}

impl std::fmt::Debug for AmqpDriver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Hand-written because `Channel` is not `Debug`. The URL is deliberately **not** printed: it may
        // carry a password, and a `Debug` that printed it would put a credential into whatever log line
        // or panic message happened to format this — the rule the Redis driver states.
        f.debug_struct("AmqpDriver")
            .field("pipelines", &self.pipelines.keys().collect::<Vec<_>>())
            .field("command_timeout", &self.command_timeout)
            .finish_non_exhaustive()
    }
}

impl AmqpDriver {
    /// Connect to a broker and declare every pipeline's queue.
    ///
    /// Connects and opens the channel eagerly, so an unreachable broker — or a URL that points at
    /// something which is not AMQP — is a startup failure rather than a request that finds out. Every
    /// driver in this tree follows that rule for the same reason: a misconfiguration belongs where the
    /// configuration is read.
    ///
    /// # Why the queues are declared here
    ///
    /// A publish to a queue that does not exist is *routed nowhere* and, under publisher confirms, still
    /// confirmed — measured: the broker answers `basic.return` with `312 NO_ROUTE` and then acks.
    /// Declaring at connect is what makes "the pipeline exists" true before the first push, and it is
    /// also what applies the application's queue arguments once rather than trusting whoever happened to
    /// declare first.
    pub fn connect(
        url: &str,
        command_timeout: Duration,
        pipelines: impl IntoIterator<Item = (String, AmqpQueue)>,
    ) -> Result<Self> {
        let command_timeout = if command_timeout.is_zero() {
            DEFAULT_COMMAND_TIMEOUT
        } else {
            command_timeout
        };

        let runtime = tokio::runtime::Builder::new_multi_thread()
            // One worker is enough: there is one connection and its channel serializes. The concurrency
            // here is the application's workers, not the client's threads.
            .worker_threads(1)
            .enable_all()
            .build()
            .map_err(|err| Error::Backend {
                driver: "amqp",
                reason: format!("could not start the client's runtime: {err}"),
            })?;

        let driver = AmqpDriver {
            runtime: Arc::new(runtime),
            url: url.to_string(),
            link: Mutex::new(None),
            pipelines: pipelines.into_iter().collect(),
            held: Mutex::new(HashMap::new()),
            command_timeout,
            sequence: AtomicU64::new(0),
        };

        // Dialling once here is what makes an unusable URL a startup failure rather than a request that
        // finds out — the rule every driver in this tree follows.
        driver.with_link("connect", |_channel| async move { Ok(()) })?;
        driver.declare_all()?;

        tracing::info!(
            pipelines = driver.pipelines.len(),
            command_timeout_ms = driver.command_timeout.as_millis() as u64,
            "amqp queue driver ready"
        );
        Ok(driver)
    }

    /// Declare every pipeline's queue, so a push has somewhere to go.
    fn declare_all(&self) -> Result<()> {
        let queues: Vec<(String, AmqpQueue)> = self
            .pipelines
            .iter()
            .map(|(name, queue)| (name.clone(), queue.clone()))
            .collect();
        for (name, queue) in queues {
            self.with_link("queue_declare", |channel| {
                let queue = queue.clone();
                async move {
                    let mut declare = QueueDeclareArguments::new(&queue.queue);
                    declare
                        .durable(queue.durable)
                        .arguments(queue.arguments.clone());
                    channel.queue_declare(declare.finish()).await.map(|_| ())
                }
            })?;
            tracing::debug!(pipeline = %name, queue = %queue.queue, "queue declared");
        }
        Ok(())
    }

    /// The parsed connection arguments, from the URL.
    ///
    /// The URL is never included in a message: it may carry a password, and an error string ends up in a
    /// log.
    ///
    /// # The percent-encoding `amqprs` does not do
    ///
    /// The AMQP URI specification writes the default virtual host as `/` in a URL, which means `%2f`
    /// — and `amqp://host/%2f` is what every other AMQP client's documentation shows, including
    /// RabbitMQ's own. `amqprs`'s parser takes the path **after** the leading slash verbatim, so it
    /// resolves that to a vhost named `%2f` and the broker refuses the connection with
    /// `530 NOT_ALLOWED - vhost %2f not found`.
    ///
    /// That was measured, and it is the difference between a documented configuration that works and one
    /// that fails at startup with a message about a virtual host the operator never wrote. The
    /// substitution is deliberately limited to `%2f`/`%2F` and the empty path: a general percent-decode
    /// would be a second URL parser, and a vhost that really is named with a percent sign in it is
    /// better served by writing that name literally than by guessing.
    fn arguments(&self) -> Result<OpenConnectionArguments> {
        let url = normalise_default_vhost(&self.url);
        OpenConnectionArguments::try_from(url.as_str()).map_err(|err| Error::Backend {
            driver: "amqp",
            reason: format!("the AMQP URL is not usable: {err}"),
        })
    }

    /// Open a fresh connection and channel, replacing whatever is there.
    ///
    /// # Why a dead channel is replaced whole rather than repaired
    ///
    /// A channel the broker closed is gone for good, and every reservation it held is back in the queue
    /// the moment the connection drops. Reusing the connection would leave this process holding
    /// reservation strings the broker no longer knows about, which is the *one* thing that must not
    /// happen: an ack for one of those closes the channel again. So the connection is dropped, every held
    /// reservation with it, and a new one is opened. The cost is that an in-flight settle fails once,
    /// which the trait already allows for as [`Error::Lost`].
    fn dial(&self) -> Result<Link> {
        let arguments = self.arguments()?;
        let deadline = self.command_timeout;
        let link = self.runtime.block_on(async {
            let connection = tokio::time::timeout(deadline, Connection::open(&arguments))
                .await
                .map_err(|_| Error::Backend {
                    driver: "amqp",
                    reason: format!("the broker did not answer within {} ms", deadline.as_millis()),
                })?
                .map_err(|err| Error::Backend {
                    driver: "amqp",
                    reason: format!("could not connect: {err}"),
                })?;

            connection
                .register_callback(DefaultConnectionCallback)
                .await
                .map_err(|err| Error::Backend {
                    driver: "amqp",
                    reason: format!("could not register the connection callback: {err}"),
                })?;

            let channel = connection
                .open_channel(None)
                .await
                .map_err(|err| Error::Backend {
                    driver: "amqp",
                    reason: format!("could not open a channel: {err}"),
                })?;

            // Confirms are enabled on every connection, not only where a caller asked: without them a
            // publish is a write to a socket and `push` would have to report "maybe", which the trait
            // refuses. The cost is one round trip per push and it is the price of `push` being either
            // accepted or an error.
            let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
            let closed = Arc::new(Mutex::new(None::<String>));
            channel
                .register_callback(Confirms {
                    events: Mutex::new(sender),
                    closed: Arc::clone(&closed),
                })
                .await
                .map_err(|err| Error::Backend {
                    driver: "amqp",
                    reason: format!("could not register the channel callback: {err}"),
                })?;
            channel
                .confirm_select(ConfirmSelectArguments::default())
                .await
                .map_err(|err| Error::Backend {
                    driver: "amqp",
                    reason: format!("could not enable publisher confirms: {err}"),
                })?;

            Ok(Link {
                // **The connection is kept, not dropped.** A `Channel` holds a `Connection` of its own,
                // so returning only the channel looks sufficient — and it is not: measured, dropping the
                // connection while keeping a channel that was opened on it closes the channel, and the
                // next `queue_declare` fails with a bare `channel closed`. The channel's own copy is
                // evidently not what keeps the connection's I/O alive; the `Connection` value is.
                connection: Some(connection),
                channel: Some(channel),
                events: receiver,
                confirmed: 0,
                published: 0,
                runtime: Arc::clone(&self.runtime),
                closed,
            })
        })?;

        // A fresh connection holds nothing, so reservations from the dead one are gone. Cleared here
        // rather than in the error path so the map and the socket cannot disagree about which
        // reservations exist.
        if let Ok(mut held) = self.held.lock() {
            held.clear();
        }
        Ok(link)
    }

    /// Run one operation against the live channel, replacing the connection if it fails.
    ///
    /// # Why one replacement and why it is not a replay
    ///
    /// A channel the broker closed fails the *next* operation, not the one that caused it — measured, and
    /// it is why this exists at all. Replacing the connection turns "the broker restarted" into one
    /// failed call rather than every call for the rest of the process's life. It is deliberately **one**
    /// attempt: a broker that is down is reported as down rather than hammered.
    ///
    /// The operation that failed is **not** re-run. That matters for `push`, whose replay could put two
    /// copies in the queue; the caller sees the error and its retry is the application's decision.
    fn with_link<T, F, Fut>(&self, operation: &'static str, run: F) -> Result<T>
    where
        F: FnOnce(Channel) -> Fut,
        Fut: std::future::Future<Output = std::result::Result<T, AmqpError>>,
    {
        // Held for the whole body so that **every** drop of an `amqprs` handle happens inside the
        // runtime. The channel is cloned for the future, and a clone dropped outside the runtime runs
        // `amqprs`'s `Drop`, which spawns and panics with "there is no reactor running" — measured. A
        // panic in a destructor is a `STATUS_STACK_BUFFER_OVERRUN` abort, not a catchable error.
        let _entered = self.runtime.enter();

        let mut guard = self
            .link
            .lock()
            // A poisoned lock is one a previous call panicked while holding. Recovering is right: the
            // guarded value is a channel, no panic can leave it in a state a redial cannot fix, and
            // refusing every later queue operation for the rest of the process's life is the worse
            // failure — the same argument the Redis driver makes.
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        if guard.is_none() {
            *guard = Some(self.dial()?);
        }
        let channel = guard
            .as_ref()
            .expect("a link was just dialled")
            .channel
            .as_ref()
            .expect("a live link always has its channel")
            .clone();

        let attempt =
            self.runtime
                .block_on(async { tokio::time::timeout(self.command_timeout, run(channel)).await });

        // Read **before** the channel is dropped, so a close the broker sent while this call was in
        // flight is included in the message rather than lost with the channel.
        let closed_reason = guard
            .as_ref()
            .and_then(|link| link.closed.lock().ok().and_then(|reason| reason.clone()));

        match attempt {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(err)) => {
                tracing::warn!(
                    operation,
                    %err,
                    reason = closed_reason.as_deref().unwrap_or("the broker gave no reason"),
                    "an amqp channel operation failed; the connection will be replaced"
                );
                *guard = None;
                Err(Error::Backend {
                    driver: "amqp",
                    reason: match closed_reason {
                        // The broker's own words, because "channel closed" tells an operator nothing and
                        // the reason it gave — a refused queue argument, a permission, a name clash — is
                        // the whole explanation.
                        Some(reason) => format!("{operation} failed: the broker closed the channel: {reason}"),
                        None => format!("{operation} failed: {err}"),
                    },
                })
            }
            Err(_) => {
                *guard = None;
                Err(Error::Backend {
                    driver: "amqp",
                    reason: format!(
                        "{operation} did not answer within {} ms, so the channel was abandoned",
                        self.command_timeout.as_millis()
                    ),
                })
            }
        }
    }

    /// The queue one pipeline is reached through, or a refusal naming the pipelines there are.
    fn queue(&self, pipeline: &str) -> Result<&AmqpQueue> {
        crate::pipeline(&self.pipelines, pipeline)
    }

    /// A reservation string unique within this process.
    ///
    /// Combined with the process id for the same reason the memory driver's ids are: a reservation is
    /// read in a log and typed into a diagnostic, and a bare counter would repeat in every server.
    fn next_reservation(&self) -> String {
        let sequence = self.sequence.fetch_add(1, Ordering::Relaxed);
        format!("{:x}-{:x}", std::process::id(), sequence)
    }

    /// One header as a string, if it is one.
    fn header<'a>(properties: &'a BasicProperties, name: &str) -> Option<&'a str> {
        let table = properties.headers()?;
        let value = table.get(&FieldName::try_from(name).ok()?)?;
        <&str>::try_from(value).ok()
    }

    /// How many times this delivery has been acquired, counting it.
    ///
    /// The exact number where the broker reports `x-acquired-count` (a quorum queue), and the coarse
    /// answer AMQP otherwise allows. See the module documentation: this is the one trait promise the
    /// protocol cannot always keep, and it is stated rather than approximated silently.
    ///
    /// **The header is an integer, not a string.** Measured against RabbitMQ 4.4.0, a quorum queue
    /// delivers `x-acquired-count` as a `LongLongInt` (`FieldValue::l`), so a reader that went through
    /// [`AmqpDriver::header`] would find nothing and quietly fall back to the approximation below — which
    /// is exactly the silent downgrade this driver is documented as *not* doing. A unit test pins both
    /// arms because of it.
    fn attempts(properties: &BasicProperties, redelivered: bool) -> u32 {
        if let Some(previous) = Self::count_header(properties, ACQUIRED_COUNT_HEADER) {
            return previous.saturating_add(1);
        }
        if redelivered {
            2
        } else {
            1
        }
    }

    /// One header read as an unsigned count, whatever integer width the broker chose.
    ///
    /// AMQP's field table has seven integer types and RabbitMQ picks one per field, so a reader that
    /// assumed a particular width would work until a broker version or a value's magnitude changed it.
    /// All of them that can hold a count are read here, and a field that is not a number is not a count
    /// rather than a zero.
    fn count_header(properties: &BasicProperties, name: &str) -> Option<u32> {
        let table = properties.headers()?;
        let value = table.get(&FieldName::try_from(name).ok()?)?;
        let number = match value {
            FieldValue::l(count) => *count,
            FieldValue::I(count) => i64::from(*count),
            FieldValue::i(count) => i64::from(*count),
            FieldValue::u(count) => i64::from(*count),
            FieldValue::s(count) => i64::from(*count),
            FieldValue::b(count) => i64::from(*count),
            FieldValue::B(count) => i64::from(*count),
            _ => return None,
        };
        u32::try_from(number).ok()
    }

    /// Publish one message and wait for the broker to confirm it.
    ///
    /// # Why the confirmation is a high-water mark
    ///
    /// Measured against RabbitMQ 4.4.0: three pipelined publishes answered with a single
    /// `ack tag=3 multiple=true`. `multiple` means "everything up to and including this tag", so the
    /// confirmation is a number that may jump, and a publish waits until that number reaches its own tag.
    /// Waiting for an exact tag would hang on every publish but the last of a batch.
    fn publish_and_confirm(
        &self,
        queue: &AmqpQueue,
        properties: BasicProperties,
        body: Vec<u8>,
    ) -> Result<u64> {
        // Held for the whole body, for the same reason `with_link` holds one: the channel is cloned for
        // the publish and the clone's `Drop` spawns a close frame, which panics outside a runtime.
        // Measured — the backtrace of this exact panic pointed at the drop of the clone below.
        let _entered = self.runtime.enter();

        let mut guard = self
            .link
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if guard.is_none() {
            *guard = Some(self.dial()?);
        }
        let link = guard.as_mut().expect("a link was just dialled");

        link.published += 1;
        let tag = link.published;
        let channel = link
            .channel
            .as_ref()
            .expect("a live link always has its channel")
            .clone();
        let sent = self.runtime.block_on(async {
            tokio::time::timeout(
                self.command_timeout,
                channel.basic_publish(
                    properties,
                    body,
                    BasicPublishArguments {
                        // Mandatory, so a message with no route comes *back* instead of vanishing. The
                        // cost is a `basic.return` per unroutable publish; the benefit is that such a
                        // publish is an error rather than a silent loss.
                        mandatory: true,
                        immediate: false,
                        exchange: queue.exchange.clone(),
                        routing_key: queue.routing_key.clone(),
                    },
                ),
            )
            .await
        });

        match sent {
            Ok(Ok(())) => {}
            Ok(Err(err)) => {
                *guard = None;
                return Err(Error::Backend {
                    driver: "amqp",
                    reason: format!("publish failed: {err}"),
                });
            }
            Err(_) => {
                *guard = None;
                return Err(Error::Backend {
                    driver: "amqp",
                    reason: format!(
                        "publishing did not complete within {} ms",
                        self.command_timeout.as_millis()
                    ),
                });
            }
        }

        let outcome = self.runtime.block_on(async {
            tokio::time::timeout(self.command_timeout, async {
                loop {
                    match link.events.recv().await {
                        Some(BrokerEvent::Confirmed {
                            tag: acked,
                            multiple,
                        }) => {
                            if multiple {
                                link.confirmed = link.confirmed.max(acked);
                            } else if acked > link.confirmed {
                                link.confirmed = acked;
                            }
                            if link.confirmed >= tag {
                                return Ok(());
                            }
                        }
                        Some(BrokerEvent::Refused { tag: refused }) => {
                            if refused >= tag {
                                return Err(format!(
                                    "the broker refused the message (nack at tag {refused})"
                                ));
                            }
                        }
                        Some(BrokerEvent::Returned { code, text }) => {
                            return Err(format!(
                                "the broker could not route the message ({code} {text}), so the queue \
                                 or exchange it was published to may not exist"
                            ));
                        }
                        // The channel is gone, so no confirmation can arrive.
                        None => {
                            return Err("the channel closed before the broker confirmed the message"
                                .to_string())
                        }
                    }
                }
            })
            .await
        });

        match outcome {
            Ok(Ok(())) => Ok(tag),
            Ok(Err(reason)) => {
                *guard = None;
                Err(Error::Backend {
                    driver: "amqp",
                    reason,
                })
            }
            Err(_) => {
                *guard = None;
                Err(Error::Backend {
                    driver: "amqp",
                    reason: format!(
                        "the broker did not confirm the message within {} ms",
                        self.command_timeout.as_millis()
                    ),
                })
            }
        }
    }

    /// Ack or nack one reservation, refusing one this process did not issue.
    ///
    /// # Why the refusal never reaches the socket
    ///
    /// Measured: `basic_ack` with a delivery tag that no longer exists is *accepted* by the call and the
    /// broker then closes the channel, so every later operation on it fails. Sending an ack for a
    /// reservation a second server reclaimed would therefore take the whole pipeline down for this
    /// process. Checking the map first turns that into [`Error::Lost`], which is the answer the trait
    /// already has for exactly this situation.
    fn settle(&self, pipeline: &str, reservation: &str, requeue: bool) -> Result<()> {
        let _ = self.queue(pipeline)?;

        let known = {
            let mut held = self
                .held
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            held.remove(reservation).ok_or_else(|| Error::Lost {
                job: reservation.to_string(),
                reservation: reservation.to_string(),
                reason: "this process is not holding it: it was already settled, the connection it was \
                         taken on has been replaced, or it was taken before a restart"
                    .to_string(),
            })?
        };

        let tag = known.tag;
        let operation = if requeue { "basic_nack" } else { "basic_ack" };
        self.with_link(operation, move |channel| async move {
            if requeue {
                channel
                    .basic_nack(BasicNackArguments::new(tag, false, true))
                    .await
            } else {
                channel.basic_ack(BasicAckArguments::new(tag, false)).await
            }
        })
    }
}

impl Driver for AmqpDriver {
    fn name(&self) -> &'static str {
        "amqp"
    }

    fn push(&self, pipeline: &str, job: Pushed) -> Result<String> {
        job.validate(pipeline)?;
        let queue = self.queue(pipeline)?.clone();

        // The payload is opaque bytes and AMQP carries them as bytes, so unlike the Redis driver there is
        // no UTF-8 constraint to state here: a broker has no opinion about a body's encoding.
        let mut headers = FieldTable::new();
        headers.insert(
            FieldName::try_from(TASK_HEADER).expect("the task header is a valid short string"),
            FieldValue::from(job.task.clone()),
        );
        headers.insert(
            FieldName::try_from(ATTEMPTS_HEADER).expect("the attempts header is a valid short string"),
            FieldValue::I(0),
        );
        for (name, value) in &job.headers {
            // A header name the broker's short-string limit rejects is refused rather than dropped:
            // silently losing an application's trace context would make a distributed trace stop at the
            // queue with nothing saying why.
            let Ok(name) = FieldName::try_from(name.as_str()) else {
                return Err(Error::BadJob {
                    reason: format!(
                        "has a header named {name:?}, which is longer than an AMQP field name allows"
                    ),
                });
            };
            headers.insert(name, FieldValue::from(value.clone()));
        }

        let properties = BasicProperties::default()
            .with_delivery_mode(2)
            .finish()
            .with_headers(headers)
            .finish();

        let id = self.next_reservation();
        let tag = self.publish_and_confirm(&queue, properties, job.payload)?;
        tracing::debug!(pipeline, id = %id, task = %job.task, tag, "job pushed");
        // The id is the driver's answer to "where did it go", and a broker assigns none. A process-unique
        // string rather than the broker's delivery tag, because the tag is a *channel* number that a
        // second consumer of the same queue would also see.
        Ok(id)
    }

    fn reserve(&self, pipeline: &str, max: usize, lease: Duration) -> Result<Vec<Reserved>> {
        let queue_name = self.queue(pipeline)?.queue.clone();
        if max == 0 {
            return Ok(Vec::new());
        }

        let mut out = Vec::new();
        for _ in 0..max {
            let name = queue_name.clone();
            // `basic_get` rather than a consumer: a consumer is a push model that would need a task per
            // pipeline and a buffer between it and the poller, and the poller already decides when it
            // wants work. One `get` per job asked for keeps the reservation synchronous, which is what
            // the trait is.
            let message = self.with_link("basic_get", move |channel| async move {
                channel.basic_get(BasicGetArguments::new(&name)).await
            })?;

            let Some((get_ok, properties, body)) = message else {
                // An empty queue: the common case, and not an error.
                break;
            };

            let mut headers = BTreeMap::new();
            if let Some(table) = properties.headers() {
                // The application's own headers are read back out, so a consumer sees what the producer
                // put in — which is what makes a trace context survive the queue.
                for (name, value) in table.as_ref() {
                    if let Ok(text) = <&str>::try_from(value) {
                        headers.insert(name.to_string(), text.to_string());
                    }
                }
            }
            // The task travels in a header and `Job::task` is what a consumer switches on, so it is
            // lifted out of the map rather than left duplicated there. `volucer-task` and
            // `volucer-attempts` are the server's own keys, and a job's map is the *application's*.
            headers.remove(TASK_HEADER);
            headers.remove(ATTEMPTS_HEADER);
            let task = Self::header(&properties, TASK_HEADER)
                .unwrap_or_default()
                .to_string();
            let attempts = Self::attempts(&properties, get_ok.redelivered());
            let pushed_at = properties
                .timestamp()
                .map(|seconds| UNIX_EPOCH + Duration::from_secs(seconds))
                .unwrap_or_else(SystemTime::now);

            let reservation = self.next_reservation();
            if let Ok(mut held) = self.held.lock() {
                held.insert(
                    reservation.clone(),
                    Held {
                        pipeline: pipeline.to_string(),
                        tag: get_ok.delivery_tag(),
                    },
                );
            }

            out.push(Reserved {
                job: Job {
                    // The reservation, because a broker assigns no id of its own and the pair has to be
                    // read the same way by every driver: an id a consumer logs is one this driver made.
                    id: reservation.clone(),
                    task,
                    payload: body,
                    headers,
                    attempts,
                    pushed_at,
                },
                reservation,
                // This process's record of the lease. The broker does not honour it — see the module
                // documentation — but the expiry is what `Reserved::remaining` reports, so reporting the
                // configured lease is more honest than reporting nothing.
                expires_at: SystemTime::now() + lease,
            });
        }
        Ok(out)
    }

    fn ack(&self, pipeline: &str, reservation: &str) -> Result<()> {
        self.settle(pipeline, reservation, false)
    }

    fn nack(&self, pipeline: &str, reservation: &str) -> Result<()> {
        self.settle(pipeline, reservation, true)
    }

    fn size(&self, pipeline: &str) -> Result<usize> {
        let name = self.queue(pipeline)?.queue.clone();
        // A passive declare is the only way AMQP 0-9-1 reports a queue's contents, and it reports the
        // **ready** count — measured: one message held unacked and one ready answers `message_count = 1`.
        // That is exactly what the trait means by "ignoring those held by a live lease".
        let counts = self.with_link("queue_declare", move |channel| async move {
            let mut declare = QueueDeclareArguments::new(&name);
            declare.passive(true);
            channel.queue_declare(declare.finish()).await
        })?;
        Ok(counts.map(|(_, ready, _)| ready as usize).unwrap_or(0))
    }

    fn in_flight(&self, pipeline: &str) -> Result<usize> {
        let _ = self.queue(pipeline)?;
        let held = self
            .held
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // **This process's** reservations, which is the one number AMQP can give without a second
        // protocol. See the module documentation for why the broker's own figure is unavailable and what
        // that means for a deployment with more than one server.
        Ok(held
            .values()
            .filter(|reservation| reservation.pipeline == pipeline)
            .count())
    }

    fn pipelines(&self) -> Vec<&str> {
        self.pipelines.keys().map(String::as_str).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The attempt count is the exact one where the broker offers it, and an honest approximation where
    /// it does not.
    ///
    /// This is the trait's one promise AMQP cannot always keep, and both halves are asserted here so that
    /// "attempts is the number of times this has run" is never claimed for a queue that cannot support it.
    ///
    /// **The integer width is part of the test, not an implementation detail.** RabbitMQ delivers
    /// `x-acquired-count` as a `LongLongInt`, and an earlier version of this function read every header
    /// as a string — so it found nothing, silently fell back to the `redelivered` guess, and reported
    /// `2` where the broker had said `2 previous acquisitions`. This assertion is what caught that.
    #[test]
    fn attempts_uses_the_brokers_own_count_when_it_is_there_and_says_what_it_can_otherwise() {
        let with_count = |value: FieldValue| {
            let mut headers = FieldTable::new();
            headers.insert(
                FieldName::try_from(ACQUIRED_COUNT_HEADER).expect("a valid field name"),
                value,
            );
            BasicProperties::default()
                .with_headers(headers)
                .finish()
        };

        // A quorum queue's header counts *previous* acquisitions, so the current one is that plus one.
        assert_eq!(AmqpDriver::attempts(&with_count(FieldValue::l(0)), false), 1);
        assert_eq!(
            AmqpDriver::attempts(&with_count(FieldValue::l(2)), true),
            3,
            "the broker's own count must win over the redelivered bit, which is why the width matters"
        );
        // Every integer width the field table can carry, because which one RabbitMQ picks for a given
        // magnitude is not something this driver decides.
        for width in [
            FieldValue::I(4),
            FieldValue::i(4),
            FieldValue::u(4),
            FieldValue::s(4),
            FieldValue::b(4),
            FieldValue::B(4),
        ] {
            assert_eq!(
                AmqpDriver::attempts(&with_count(width), false),
                5,
                "an integer count read as a string would silently become the approximation"
            );
        }
        // A header that is not a number is not a count: the approximation is better than a zero.
        assert_eq!(
            AmqpDriver::attempts(&with_count(FieldValue::from("many")), true),
            2
        );

        // No header: one bit of information, so the answer is true but not a count. Asserted rather than
        // left to a comment, because a reader who assumes otherwise would build a retry policy on it.
        let bare = BasicProperties::default().finish();
        assert_eq!(AmqpDriver::attempts(&bare, false), 1);
        assert_eq!(AmqpDriver::attempts(&bare, true), 2);
        assert_eq!(
            AmqpDriver::attempts(&bare, true),
            AmqpDriver::attempts(&bare, true),
            "two redeliveries are indistinguishable, which is the limitation and not an accident"
        );
    }

    /// A header that is not a string, or is missing, is not the task name.
    #[test]
    fn the_task_header_is_read_only_when_it_is_a_string() {
        let mut headers = FieldTable::new();
        headers.insert(
            FieldName::try_from(TASK_HEADER).expect("a valid field name"),
            FieldValue::I(7),
        );
        let numeric = BasicProperties::default()
            .with_headers(headers)
            .finish();
        assert_eq!(
            AmqpDriver::header(&numeric, TASK_HEADER),
            None,
            "a task name that is a number is not a task name, and guessing would let a consumer switch \
             on a string nothing wrote"
        );

        assert_eq!(
            AmqpDriver::header(&BasicProperties::default().finish(), TASK_HEADER),
            None,
            "and an absent header is absent rather than the empty string"
        );
    }

    /// A queue's defaults are the ones a deployment behind the default exchange needs.
    #[test]
    fn a_queue_defaults_match_its_pipeline_and_route_through_the_default_exchange() {
        let queue = AmqpQueue::new("emails");
        assert_eq!(queue.queue, "emails");
        assert_eq!(queue.routing_key, "emails", "the default exchange routes by queue name");
        assert!(queue.exchange.is_empty(), "the default exchange is the empty name");
        assert!(queue.durable, "a transient queue would lose jobs on a broker restart");
        assert!(
            queue.arguments.as_ref().is_empty(),
            "and no queue argument is imposed: the application's queue type and TTLs are its own"
        );
    }

    /// The two spellings of the default virtual host are reduced to the one `amqprs` handles.
    ///
    /// This is the defect a `VOLUCER_E2E` run caught: every AMQP test was *silently skipping* because the
    /// documented URL — `amqp://guest:guest@host/%2f`, which is what RabbitMQ's own documentation shows —
    /// was refused with `530 NOT_ALLOWED - vhost %2f not found`. The gate turned ten green skips into ten
    /// red failures, which is the only reason it was found.
    #[test]
    fn the_default_virtual_host_is_rewritten_to_the_form_the_client_parses() {
        // Both documented spellings become "no path", which is the form that resolves to `/`.
        assert_eq!(
            normalise_default_vhost("amqp://guest:guest@127.0.0.1:5672/%2f"),
            "amqp://guest:guest@127.0.0.1:5672"
        );
        assert_eq!(
            normalise_default_vhost("amqp://guest:guest@127.0.0.1:5672/%2F"),
            "amqp://guest:guest@127.0.0.1:5672"
        );
        assert_eq!(
            normalise_default_vhost("amqps://guest:guest@broker:5671/"),
            "amqps://guest:guest@broker:5671"
        );
        assert_eq!(
            normalise_default_vhost("amqp://guest:guest@127.0.0.1:5672"),
            "amqp://guest:guest@127.0.0.1:5672",
            "a URL with no path is already right, and must not be broken by a rewrite"
        );

        // A named virtual host is left exactly as the operator wrote it.
        assert_eq!(
            normalise_default_vhost("amqp://guest:guest@host:5672/production"),
            "amqp://guest:guest@host:5672/production"
        );

        // A `%2f` in the *password* must not be touched: base64 is full of slashes, and decoding one in
        // the authority would corrupt the credential rather than fix the path.
        assert_eq!(
            normalise_default_vhost("amqp://user:a%2fb@host:5672/%2f"),
            "amqp://user:a%2fb@host:5672",
            "only the path after the authority is rewritten"
        );

        // Something that is not a URL at all is passed through so the client's own parser reports it,
        // rather than this function inventing a message about a string it does not understand.
        assert_eq!(normalise_default_vhost("not a url"), "not a url");
        assert_eq!(normalise_default_vhost("127.0.0.1:5672"), "127.0.0.1:5672");
    }
}
