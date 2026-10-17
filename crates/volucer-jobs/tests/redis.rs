//! The Redis queue driver, against a **real** Redis.
//!
//! **The whole file is behind the `redis` feature**, so a build without it has no test to skip and no
//! `RedisDriver` to import. A file that compiled in both would need every test's body gated as well, which
//! is the same gate written many times and one of them eventually forgotten.
//!
//! # Why these are skipped rather than mocked
//!
//! Almost everything worth checking here is a property of the *server*: that a Lua script's pop and its
//! reservation are one operation, that `LMOVE`-style reclaiming really returns a job to the head of the
//! list, that `ZRANGEBYSCORE` finds only the expired reservations, that `HINCRBY` on a missing key creates
//! it at one, and that two drivers on two connections see each other's jobs. A mock would answer all of
//! those however its author imagined Redis behaves — and the properties that make a queue shared between
//! processes work at all are exactly the ones a mock is unable to disagree with.
//!
//! The properties a mock *could* test — the header encoding, the reservation's split, the key layout —
//! are pure functions with unit tests in the module, so nothing is lost by not mocking.
//!
//! # Why the whole file is written to be *destructive* to its own keys only
//!
//! These run against a server somebody may be using for something else. Every key this driver makes starts
//! with `volucer:jobs:<pipeline>:`, and each test uses a pipeline named after itself **and after this run**
//! — so a `DEL` pattern can only reach keys the test itself wrote. See [`pipeline`].
//!
//! # Serial, and why the pipeline name carries a timestamp
//!
//! Windows reuses process ids aggressively. A lease left behind by an earlier run is a reservation the
//! *next* run finds, and a job a test pushed and did not consume would be picked up as an extra ready job.
//! The first cross-process lock test in this tree failed exactly that way, on its second run, having passed
//! on its first. So a pipeline name here is `test-<pid>-<test>-<nanos>`.

#![cfg(feature = "redis")]

use std::time::Duration;

use volucer_jobs::{Driver, Error, MemoryDriver, Pushed, RedisDriver};

/// The backend to use, or `None` with a printed reason.
fn connect(test: &str) -> Option<(RedisDriver, String)> {
    let url = std::env::var("REDIS_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379".to_string());
    let pipeline = pipeline(test);
    match RedisDriver::connect(
        &url,
        2,
        Duration::from_secs(5),
        [pipeline.clone(), format!("{pipeline}-other")],
    ) {
        Ok(driver) => {
            wipe(&pipeline);
            Some((driver, pipeline))
        }
        Err(err) => {
            let reason = format!("no Redis at {url}: {err}");
            if skip_is_a_failure() {
                panic!("VOLUCER_E2E is set, so a skipped test is a failure: {reason}");
            }
            eprintln!("skipping: {reason}");
            None
        }
    }
}

fn skip_is_a_failure() -> bool {
    match std::env::var("VOLUCER_E2E") {
        Ok(value) => !value.is_empty() && value != "0",
        Err(_) => false,
    }
}

/// A pipeline name unique to one test **and to one run of it**.
fn pipeline(test: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    format!("test-{}-{test}-{nanos}", std::process::id())
}

/// Remove every key a test may have made, so a rerun starts from nothing.
///
/// The pattern is built from the pipeline name, which is already unique to this run — so this cannot reach
/// another test's keys or a real deployment's, which is the property that makes it safe to run against a
/// server somebody else is using.
fn wipe(pipeline: &str) {
    let pattern = format!("volucer:jobs:{pipeline}*");
    // A direct client rather than the driver's own API: the driver deliberately exposes no way to delete
    // a pipeline, and adding one so a test could call it would be an API for a test's convenience.
    let url = std::env::var("REDIS_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379".to_string());
    let Ok(client) = redis::Client::open(url) else {
        return;
    };
    let Ok(mut connection) = client.get_connection() else {
        return;
    };
    let keys: Vec<String> = redis::cmd("KEYS")
        .arg(&pattern)
        .query(&mut connection)
        .unwrap_or_default();
    if !keys.is_empty() {
        let _ = redis::cmd("DEL").arg(keys).query::<()>(&mut connection);
    }
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

    // **A second driver on its own connections**, which is what a second server is. A reserve through the
    // same object would not prove that the job is in Redis rather than in this process.
    let other = RedisDriver::connect(
        &std::env::var("REDIS_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379".to_string()),
        1,
        Duration::from_secs(5),
        [pipeline.clone()],
    )
    .expect("the second driver connects");

    let reserved = other.reserve(&pipeline, 1, long()).expect("reserved");
    assert_eq!(reserved.len(), 1, "the pushed job must be reservable");
    let job = &reserved[0].job;
    assert_eq!(
        job.id, id,
        "the id a push answers with is the id a reserve reports"
    );
    assert_eq!(job.task, "App\\Jobs\\SendEmail");
    assert_eq!(
        job.payload, b"{\"to\":\"a@b\"}",
        "the payload is the bytes that went in"
    );
    assert_eq!(job.header("traceparent"), Some("00-abc-def-01"));
    assert_eq!(
        job.header("retry-delay"),
        Some("30"),
        "and every header survives, not only the first"
    );
    assert_eq!(
        job.attempts, 1,
        "the first reservation is attempt one, and the *driver* decides that rather than the producer"
    );

    other
        .ack(&pipeline, &reserved[0].reservation)
        .expect("acked");
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
}

#[test]
fn a_reserved_job_is_not_handed_out_again_while_its_lease_holds() {
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
    assert_eq!(driver.size(&pipeline).expect("size"), 0);
    assert_eq!(
        driver.in_flight(&pipeline).expect("in flight"),
        1,
        "and the reservation is what says so"
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
}

/// The reclamation, which is the whole reason the deadline index exists.
///
/// The failure it guards against is the one a queue must not have: a consumer that died holding a job, and
/// a job that nobody will ever hand out again. It is also the test that would fail if the reclaim ran
/// *after* the pop rather than before — the job would come back on the next reserve instead of this one,
/// which is a consumer idling for a poll interval on a ready job.
#[test]
fn a_reservation_whose_lease_expires_comes_back_on_the_next_reserve() {
    let Some((driver, pipeline)) = connect("expiry") else {
        return;
    };
    driver
        .push(&pipeline, Pushed::new("slow-consumer"))
        .expect("pushed");

    let first = driver
        .reserve(&pipeline, 1, Duration::from_millis(1))
        .expect("reserved");
    assert_eq!(first.len(), 1);
    std::thread::sleep(Duration::from_millis(50));

    let second = driver.reserve(&pipeline, 1, long()).expect("reserved");
    assert_eq!(
        second.len(),
        1,
        "a job whose lease expired must be available again"
    );
    assert_eq!(
        second[0].job.id, first[0].job.id,
        "and it is the same job, not a second copy of it"
    );
    assert_eq!(
        second[0].job.attempts, 2,
        "the second reservation is attempt two, which only a driver-side counter can know"
    );

    // The first consumer's reservation is gone, so its late ack is refused rather than settling a job the
    // second consumer is now running — the check the reservation value exists for.
    assert!(
        matches!(
            driver.ack(&pipeline, &first[0].reservation),
            Err(Error::Lost { .. })
        ),
        "a consumer whose lease expired must not be able to acknowledge the new holder's job"
    );

    driver
        .ack(&pipeline, &second[0].reservation)
        .expect("acked");
}

#[test]
fn a_nack_returns_the_job_to_the_back_of_the_queue() {
    let Some((driver, pipeline)) = connect("nack") else {
        return;
    };
    driver
        .push(&pipeline, Pushed::new("first"))
        .expect("pushed");
    driver
        .push(&pipeline, Pushed::new("second"))
        .expect("pushed");

    let first = driver.reserve(&pipeline, 1, long()).expect("reserved");
    assert_eq!(first[0].job.task, "first");
    driver
        .nack(&pipeline, &first[0].reservation)
        .expect("nacked");

    let next = driver.reserve(&pipeline, 2, long()).expect("reserved");
    let tasks: Vec<&str> = next.iter().map(|held| held.job.task.as_str()).collect();
    assert_eq!(
        tasks,
        vec!["second", "first"],
        "a job that just failed must not jump ahead of work that has been waiting"
    );
    for held in &next {
        driver.ack(&pipeline, &held.reservation).expect("acked");
    }
}

#[test]
fn an_acknowledgement_for_a_reservation_nobody_holds_is_refused() {
    let Some((driver, pipeline)) = connect("unknown") else {
        return;
    };
    driver.push(&pipeline, Pushed::new("x")).expect("pushed");
    let reserved = driver.reserve(&pipeline, 1, long()).expect("reserved");
    driver
        .ack(&pipeline, &reserved[0].reservation)
        .expect("acked");

    assert!(
        matches!(
            driver.ack(&pipeline, &reserved[0].reservation),
            Err(Error::Lost { .. })
        ),
        "a second ack of the same reservation is a `Lost`, not a silent success"
    );
}

#[test]
fn two_pipelines_share_a_server_and_nothing_else() {
    let Some((driver, pipeline)) = connect("two-queues") else {
        return;
    };
    let other = format!("{pipeline}-other");

    driver.push(&pipeline, Pushed::new("mine")).expect("pushed");
    driver.push(&other, Pushed::new("theirs")).expect("pushed");

    assert_eq!(driver.size(&pipeline).expect("size"), 1);
    assert_eq!(driver.size(&other).expect("size"), 1);

    let mine = driver.reserve(&pipeline, 1, long()).expect("reserved");
    assert_eq!(
        mine[0].job.task, "mine",
        "a reservation from one pipeline must never reach another pipeline's job"
    );
    assert_eq!(
        driver.size(&other).expect("size"),
        1,
        "and taking from one pipeline leaves the other's queue alone"
    );
    driver.ack(&pipeline, &mine[0].reservation).expect("acked");
    wipe(&pipeline);
    wipe(&other);
}

#[test]
fn a_pipeline_this_driver_does_not_serve_is_refused_by_name() {
    let Some((driver, _)) = connect("unknown-pipeline") else {
        return;
    };
    let err = driver
        .push("definitely-not-configured", Pushed::new("x"))
        .expect_err("a pipeline the driver does not serve is refused");
    let message = err.to_string();
    assert!(message.contains("definitely-not-configured"), "{message}");
    assert!(
        message.contains("configured"),
        "and it names what there is, so the fix is a rename: {message}"
    );
}

/// A payload Redis cannot hold is refused at the *push*, not mangled at the reserve.
///
/// A Redis hash field is a string, so bytes that are not valid UTF-8 cannot make the round trip. The
/// alternative — accepting the push and corrupting the bytes — would surface as a deserialization error in
/// the application, days later and nowhere near the cause.
#[test]
fn a_payload_that_is_not_utf8_is_refused_by_the_redis_driver_and_taken_by_the_memory_one() {
    let Some((driver, pipeline)) = connect("utf8") else {
        return;
    };
    let bytes = vec![0x00, 0xff, 0xfe, 0x7b];

    let err = driver
        .push(&pipeline, Pushed::new("x").with_payload(bytes.clone()))
        .expect_err("a Redis hash field cannot hold arbitrary bytes");
    let message = err.to_string();
    assert!(message.contains("UTF-8"), "{message}");
    assert_eq!(
        driver.size(&pipeline).expect("size"),
        0,
        "and the refused push left nothing behind"
    );

    // The same push through the memory driver, which stores a `Vec<u8>` and has no such constraint — so
    // this is a *driver* limit and not a rule the trait imposes.
    let memory = MemoryDriver::single("emails");
    memory
        .push("emails", Pushed::new("x").with_payload(bytes.clone()))
        .expect("the memory driver takes any bytes");
    let reserved = memory.reserve("emails", 1, long()).expect("reserved");
    assert_eq!(reserved[0].job.payload, bytes);
}

/// The two drivers satisfy the same trait, so anything that holds one holds the other.
///
/// Not a formality: the server's `build_queues` chooses between them from configuration and names both
/// through `Arc<dyn Driver>`, so a driver that could not be used there would be a driver that only ever
/// worked in its own tests.
#[test]
fn the_redis_driver_is_usable_as_a_boxed_driver_like_the_memory_one() {
    let Some((driver, pipeline)) = connect("trait-object") else {
        return;
    };
    let drivers: Vec<Box<dyn Driver>> = vec![Box::new(driver), Box::new(MemoryDriver::single("m"))];
    assert_eq!(drivers[0].name(), "redis");
    assert_eq!(drivers[1].name(), "memory");
    assert_eq!(
        drivers[0].pipelines(),
        vec![pipeline.as_str(), format!("{pipeline}-other").as_str()],
        "the pipelines the driver was built for, in the order the configuration wrote them"
    );
}
