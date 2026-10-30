//! The AMQP driver, against a **real** RabbitMQ.
//!
//! **The whole file is behind the `amqp` feature**, so a build without it has no test to skip and no
//! `AmqpDriver` to import. A file that compiled in both would need every test's body gated as well, which
//! is the same gate written many times and one of them eventually forgotten — the rule `tests/redis.rs`
//! states.
//!
//! # Why these are skipped rather than mocked
//!
//! Almost everything worth checking here is a property of the *broker*, and three of them were measured
//! rather than assumed while this driver was written:
//!
//! - a **bogus delivery tag closes the channel**, so the driver must refuse an unknown reservation before
//!   it reaches the socket;
//! - a **requeue puts the message back at the head**, unlike the memory and Redis drivers, which push it
//!   to the tail;
//! - a **quorum queue** injects `x-acquired-count` and a classic one does not, which is the difference
//!   between a real attempt count and a guess.
//!
//! A mock would answer all three however its author imagined RabbitMQ behaves, and the properties that
//! make a shared queue work are exactly the ones a mock cannot disagree with.
//!
//! # Why the queue names carry a timestamp
//!
//! A queue outlives the process that declared it, so a leftover reservation or a message a test pushed and
//! did not consume would be found by the *next* run. Windows reuses process ids aggressively, so a name
//! built from the pid alone repeats. Every name here is `volucer-test-<pid>-<test>-<nanos>` and every test
//! deletes its own queue — which is also what makes it safe to run against a broker somebody else is
//! using.

#![cfg(feature = "amqp")]

use std::time::Duration;

use amqp_serde::types::{FieldName, FieldTable, FieldValue};
use volucer_jobs::{AmqpDriver, AmqpQueue, Driver, Error, Pushed};

/// How long to give a broker round trip in these tests.
///
/// Longer than the driver's own default because a CI broker may be cold, and short enough that a hung
/// test fails rather than hangs.
fn patience() -> Duration {
    Duration::from_secs(10)
}

/// The broker URL, or the default a local RabbitMQ listens on.
fn url() -> String {
    std::env::var("AMQP_URL").unwrap_or_else(|_| "amqp://guest:guest@127.0.0.1:5672/%2f".to_string())
}

fn skip_is_a_failure() -> bool {
    match std::env::var("VOLUCER_E2E") {
        Ok(value) => !value.is_empty() && value != "0",
        Err(_) => false,
    }
}

/// A queue name unique to one test **and to one run of it**.
fn queue_name(test: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    format!("volucer-test-{}-{test}-{nanos}", std::process::id())
}

/// Connect a driver over the named queue, or answer `None` with a printed reason.
fn connect(test: &str) -> Option<(AmqpDriver, String)> {
    connect_with(test, |_| {})
}

/// Connect a driver, letting the caller adjust the queue before it is declared.
///
/// The adjustment is applied *before* `connect` because that is when the queues are declared, and a queue
/// argument cannot be changed afterwards: RabbitMQ refuses a redeclare with different arguments, which is
/// the property that makes a queue's type belong to whoever declared it first.
fn connect_with(test: &str, adjust: impl FnOnce(&mut AmqpQueue)) -> Option<(AmqpDriver, String)> {
    let pipeline = queue_name(test);
    let mut queue = AmqpQueue::new(&pipeline);
    adjust(&mut queue);
    match AmqpDriver::connect(&url(), patience(), [(pipeline.clone(), queue)]) {
        Ok(driver) => Some((driver, pipeline)),
        Err(err) => {
            let reason = format!("no AMQP broker at {}: {err}", url());
            if skip_is_a_failure() {
                panic!("VOLUCER_E2E is set, so a skipped test is a failure: {reason}");
            }
            eprintln!("skipping: {reason}");
            None
        }
    }
}

/// Delete a test's queue, so a rerun starts from nothing.
///
/// A direct client rather than the driver's API: the driver deliberately exposes no way to delete a
/// pipeline, and adding one so a test could call it would be an API for a test's convenience — the rule
/// `tests/redis.rs` follows for the same reason.
fn wipe(pipeline: &str) {
    let normalised = volucer_jobs::amqp::normalise_default_vhost(&url());
    let Ok(arguments) = amqprs::connection::OpenConnectionArguments::try_from(normalised.as_str())
    else {
        return;
    };
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(_) => return,
    };
    // **The channel and connection are dropped inside `runtime.enter()`.** `amqprs` spawns a close frame
    // from both `Drop` impls, which panics outside a runtime — measured, and the same hazard
    // `AmqpDriver`'s own `Link::drop` exists for. A test doing this the obvious way aborts the process.
    let deleted = runtime.block_on(async {
        let connection = amqprs::connection::Connection::open(&arguments).await?;
        let channel = connection.open_channel(None).await?;
        let answer = channel
            .queue_delete(amqprs::channel::QueueDeleteArguments::new(pipeline))
            .await;
        // Dropped here, on the runtime's own thread, rather than at the end of the block outside it.
        drop(channel);
        drop(connection);
        answer
    });
    // Reported rather than ignored: a wipe that silently fails is how the "push into a deleted queue"
    // assertion came to pass against a queue that was still there, which is a test proving nothing.
    if deleted.is_err() {
        eprintln!(
            "warning: could not delete queue {pipeline:?}, so this run may not have been isolated"
        );
    }
    drop(runtime);
}

/// A lease long enough that a slow machine cannot expire it mid-test.
fn long() -> Duration {
    Duration::from_secs(60)
}

#[test]
fn a_pushed_job_comes_back_whole_from_a_different_connection() {
    let Some((driver, pipeline)) = connect("round-trip") else {
        return;
    };

    let pushed = Pushed::new("App\\Jobs\\SendEmail")
        .with_payload(b"{\"to\":\"a@b\"}".to_vec())
        .with_header("traceparent", "00-abc-def-01")
        .with_header("retry-delay", "30");
    let id = driver.push(&pipeline, pushed).expect("pushed");
    assert!(!id.is_empty());

    // **A second driver on its own connection**, which is what a second server is. A reserve through the
    // same object would not prove that the job is in the broker rather than in this process.
    let other = AmqpDriver::connect(
        &url(),
        patience(),
        [(pipeline.clone(), AmqpQueue::new(&pipeline))],
    )
    .expect("the second driver connects");

    let reserved = other.reserve(&pipeline, 1, long()).expect("reserved");
    assert_eq!(reserved.len(), 1, "the pushed job must be reservable");
    let job = &reserved[0].job;
    assert_eq!(
        job.task, "App\\Jobs\\SendEmail",
        "the task name travels in a header and is lifted back out of it"
    );
    assert_eq!(
        job.payload, b"{\"to\":\"a@b\"}",
        "the payload is the bytes that went in, unmodified"
    );
    assert_eq!(
        job.header("traceparent"),
        Some("00-abc-def-01"),
        "an application's header survives the broker"
    );
    assert_eq!(
        job.header("retry-delay"),
        Some("30"),
        "and every header survives, not only the first"
    );
    assert_eq!(
        job.attempts, 1,
        "the first reservation is attempt one, and the *driver* decides that rather than the producer"
    );
    assert_eq!(
        job.header("volucer-task"),
        None,
        "the server's own headers are not handed to the application as if it had written them"
    );

    other
        .ack(&pipeline, &reserved[0].reservation)
        .expect("acked");
    wipe(&pipeline);
}

#[test]
fn jobs_are_handed_out_in_the_order_they_were_pushed() {
    let Some((driver, pipeline)) = connect("fifo") else {
        return;
    };
    for index in 0..5 {
        driver
            .push(&pipeline, Pushed::new(format!("job-{index}")))
            .expect("pushed");
    }

    let reserved = driver.reserve(&pipeline, 5, long()).expect("reserved");
    let tasks: Vec<&str> = reserved.iter().map(|held| held.job.task.as_str()).collect();
    assert_eq!(
        tasks,
        vec!["job-0", "job-1", "job-2", "job-3", "job-4"],
        "FIFO within a pipeline is the promise an application relies on"
    );
    for held in &reserved {
        driver.ack(&pipeline, &held.reservation).expect("acked");
    }
    wipe(&pipeline);
}

/// A reserved job is held by its channel, not by a timer — which is what makes the lease a different thing
/// here than in the other two drivers.
#[test]
fn a_reserved_job_is_not_handed_out_again_while_its_channel_holds_it() {
    let Some((driver, pipeline)) = connect("exclusive") else {
        return;
    };
    driver.push(&pipeline, Pushed::new("only")).expect("pushed");

    let first = driver.reserve(&pipeline, 1, long()).expect("reserved");
    assert_eq!(first.len(), 1);

    assert!(
        driver
            .reserve(&pipeline, 1, long())
            .expect("reserved")
            .is_empty(),
        "a job in flight must not be handed to a second consumer"
    );
    assert_eq!(
        driver.size(&pipeline).expect("size"),
        0,
        "the broker's ready count excludes an unacknowledged message — measured, not assumed"
    );
    assert_eq!(
        driver.in_flight(&pipeline).expect("in flight"),
        1,
        "and this process's own record is what says so, because AMQP has no broker-side equivalent"
    );

    driver.ack(&pipeline, &first[0].reservation).expect("acked");
    assert_eq!(
        driver.in_flight(&pipeline).expect("in flight"),
        0,
        "an ack removes the reservation"
    );
    assert!(
        driver
            .reserve(&pipeline, 1, long())
            .expect("reserved")
            .is_empty(),
        "and after an ack the job is gone rather than available again"
    );
    wipe(&pipeline);
}

/// A nack with requeue brings the job back, and **to the head** — which is the opposite of the memory and
/// Redis drivers and is a property of the broker rather than a choice made here.
///
/// This test exists in this exact shape because copying the other drivers' assertion (`second`, then
/// `first`) would have encoded a promise AMQP does not keep. Measured against RabbitMQ 4.4.0: a message
/// taken first and requeued is the *next* one handed out.
#[test]
fn a_nack_returns_the_job_to_the_queue_and_the_broker_hands_it_out_first() {
    let Some((driver, pipeline)) = connect("nack") else {
        return;
    };
    driver
        .push(&pipeline, Pushed::new("first"))
        .expect("pushed");
    driver
        .push(&pipeline, Pushed::new("second"))
        .expect("pushed");

    let taken = driver.reserve(&pipeline, 1, long()).expect("reserved");
    assert_eq!(taken[0].job.task, "first");
    driver
        .nack(&pipeline, &taken[0].reservation)
        .expect("nacked");

    let next = driver.reserve(&pipeline, 2, long()).expect("reserved");
    let tasks: Vec<&str> = next.iter().map(|held| held.job.task.as_str()).collect();
    assert_eq!(
        tasks,
        vec!["first", "second"],
        "RabbitMQ requeues to the head, so a requeued job is the next one handed out — which is the \
         opposite of the memory and Redis drivers, and is documented rather than hidden"
    );
    assert_eq!(
        next[0].job.attempts, 2,
        "a requeued job is on its second attempt, and the broker's redelivered bit is what says so on a \
         classic queue"
    );
    for held in &next {
        driver.ack(&pipeline, &held.reservation).expect("acked");
    }
    wipe(&pipeline);
}

/// A quorum queue reports the exact attempt count, and the driver reads it.
///
/// This is the difference the module documentation spends a section on: on a classic queue the count is a
/// `redelivered` guess, and on a quorum queue RabbitMQ injects `x-acquired-count`. Asserted against the
/// real broker because the header's *presence* is a broker behaviour, and because an earlier version of
/// the driver read it as a string and found nothing.
#[test]
fn a_quorum_queue_reports_the_exact_attempt_count() {
    let Some((driver, pipeline)) = connect_with("quorum-attempts", |queue| {
        let mut arguments = FieldTable::new();
        arguments.insert(
            FieldName::try_from("x-queue-type").expect("a valid field name"),
            FieldValue::from("quorum"),
        );
        queue.arguments = arguments;
    }) else {
        return;
    };

    driver
        .push(&pipeline, Pushed::new("retried"))
        .expect("pushed");

    // Take it and put it back three times, and read the count on each. The first acquisition has no
    // header, so it is the coarse `1`; the second and third carry the broker's own count.
    let mut counts = Vec::new();
    for _ in 0..3 {
        let held = driver.reserve(&pipeline, 1, long()).expect("reserved");
        assert_eq!(held.len(), 1, "the requeued job must still be there");
        counts.push(held[0].job.attempts);
        driver
            .nack(&pipeline, &held[0].reservation)
            .expect("nacked");
    }
    assert_eq!(
        counts,
        vec![1, 2, 3],
        "a quorum queue's x-acquired-count is what makes the attempt count exact rather than a guess"
    );

    let last = driver.reserve(&pipeline, 1, long()).expect("reserved");
    driver
        .ack(&pipeline, &last[0].reservation)
        .expect("acked");
    wipe(&pipeline);
}

/// The reservation check that keeps a dead channel from closing the pipeline.
///
/// Measured: `basic_ack` with a delivery tag the channel no longer knows is *accepted* by the call, and
/// the broker then closes the channel — so every later operation fails. A driver that sent it would turn
/// one late ack into a broken pipeline. The refusal has to happen before the socket, which is what this
/// asserts.
#[test]
fn a_settle_for_an_unknown_reservation_is_refused_and_leaves_the_channel_usable() {
    let Some((driver, pipeline)) = connect("unknown-reservation") else {
        return;
    };
    driver
        .push(&pipeline, Pushed::new("taken"))
        .expect("pushed");

    let held = driver.reserve(&pipeline, 1, long()).expect("reserved");
    assert_eq!(held.len(), 1);

    // A reservation this process never issued. Sending it as an ack would close the channel.
    assert!(
        matches!(
            driver.ack(&pipeline, "0-deadbeef"),
            Err(Error::Lost { .. })
        ),
        "an unknown reservation is Lost, which is what the trait reserves that variant for"
    );

    // And the channel must still work: settling the real one is the proof.
    driver
        .ack(&pipeline, &held[0].reservation)
        .expect("the channel survived an unknown reservation");
    driver
        .push(&pipeline, Pushed::new("after"))
        .expect("a push after an unknown reservation still works");
    let after = driver.reserve(&pipeline, 1, long()).expect("reserved");
    assert_eq!(after[0].job.task, "after");
    driver
        .ack(&pipeline, &after[0].reservation)
        .expect("acked");
    wipe(&pipeline);
}

/// Settling the same reservation twice is `Lost` the second time, as it is in every other driver.
#[test]
fn acking_twice_is_a_lost_reservation_rather_than_a_dead_channel() {
    let Some((driver, pipeline)) = connect("double-ack") else {
        return;
    };
    driver.push(&pipeline, Pushed::new("once")).expect("pushed");
    let held = driver.reserve(&pipeline, 1, long()).expect("reserved");
    driver
        .ack(&pipeline, &held[0].reservation)
        .expect("the first ack settles it");

    assert!(
        matches!(
            driver.ack(&pipeline, &held[0].reservation),
            Err(Error::Lost { .. })
        ),
        "the second ack is refused by this process rather than sent to a broker that would close the \
         channel for it"
    );
    // The channel is still alive, which is the point.
    assert_eq!(driver.size(&pipeline).expect("size"), 0);
    wipe(&pipeline);
}

/// An unknown pipeline is refused by name, and the refusal names the ones there are — the same rule every
/// other driver follows.
#[test]
fn an_unknown_pipeline_is_refused_by_name_before_anything_reaches_the_broker() {
    let Some((driver, pipeline)) = connect("unknown-pipeline") else {
        return;
    };

    let err = driver
        .push("nnot-the-pipeline", Pushed::new("task"))
        .expect_err("a typo is not a pipeline");
    let message = err.to_string();
    assert!(message.contains("nnot-the-pipeline"), "{message}");
    assert!(
        message.contains(&format!("{pipeline:?}")),
        "and the ones that exist, so the fix is a rename rather than a search: {message}"
    );

    assert_eq!(driver.pipelines(), vec![pipeline.as_str()]);
    wipe(&pipeline);
}

/// A push to a queue that has been deleted is an error rather than a silent success.
///
/// The failure this guards against is subtle and was measured: RabbitMQ `basic.return`s an unroutable
/// `mandatory` publish with `312 NO_ROUTE` and then **confirms** it. A driver that watched only for the
/// confirmation would report a push into a deleted queue as accepted.
#[test]
fn a_push_to_a_queue_that_disappeared_is_an_error_rather_than_a_confirmed_no_op() {
    let Some((driver, pipeline)) = connect("vanished-queue") else {
        return;
    };
    driver
        .push(&pipeline, Pushed::new("before"))
        .expect("the queue exists, so this works");

    wipe(&pipeline);

    match driver.push(&pipeline, Pushed::new("after")) {
        Err(err) => {
            let message = err.to_string();
            assert!(
                message.contains("could not route") || message.contains("312"),
                "the refusal must say the message was not routed, not merely that something failed: \
                 {message}"
            );
        }
        Ok(_) => panic!(
            "a push into a deleted queue reported success, which would be a job silently lost — the \
             broker returns NO_ROUTE and then confirms, so the confirmation alone is not enough"
        ),
    }
}

/// An empty queue answers zero rather than an error, and `reserve` answers an empty list.
#[test]
fn an_empty_queue_is_not_an_error() {
    let Some((driver, pipeline)) = connect("empty") else {
        return;
    };
    assert_eq!(driver.size(&pipeline).expect("size"), 0);
    assert_eq!(driver.in_flight(&pipeline).expect("in flight"), 0);
    assert!(
        driver.reserve(&pipeline, 5, long()).expect("reserved").is_empty(),
        "fewer than asked for is normal and is not an error, the same rule every driver follows"
    );
    wipe(&pipeline);
}
