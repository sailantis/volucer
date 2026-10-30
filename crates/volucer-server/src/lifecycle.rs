//! Platform process lifecycle handling.
//!
//! The hazard this exists for: if the master dies, its
//! `php-cgi` children survive as orphans holding their ports, which makes the
//! development loop miserable. Every platform has its own mechanism.
//!
//! - **Windows:** a job object with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`. When
//!   the last handle to the job closes — including because our process died —
//!   the OS terminates every process in it.
//! - **Unix:** two mechanisms, because neither alone covers the whole hazard. A
//!   process group is what a *deliberate* kill targets, so [`kill`] reaches a
//!   child **and its own children**; `PR_SET_PDEATHSIG` is what covers the parent
//!   dying *without getting to kill anything* — a `SIGKILL`, an abort, an OOM kill.
//!
//! **`PR_SET_PDEATHSIG` is bound to the thread that called `fork`, not to the process.**
//! The kernel records the parent as that thread, and delivers the signal when *that thread*
//! exits — which a worker thread does the moment its work is done, long before the process
//! does. So a child must always be forked from a thread that outlives it, and every spawn
//! here goes through [`spawn`], which forks on one thread owned by this module and living
//! for the life of the process. Calling `Command::spawn` directly is how this was broken
//! once already: slots were spawned on `std::thread::scope` threads, and every `php-cgi`
//! was `SIGKILL`ed microseconds after it started listening, which surfaced as a 502 on the
//! request after the one that had just succeeded.
//!
//! **Both halves are implemented here.** An earlier version of this file described
//! them and implemented neither: `killpg` appeared nowhere, every kill was
//! `Child::kill` on a single pid, and the group existed only as a comment. The
//! measured consequence was a server `SIGKILL`ed while its children kept running,
//! reparented to init and still holding their ports — precisely the failure this
//! module is about.

use std::process::Child;

/// A guard that must outlive the children it protects.
///
/// Dropping it kills the children it owns, so it has to be held for as long as
/// the server runs and dropped last.
///
/// **Not `Clone`, and the streamed-response ending is why that matters.** A callback that ran on an
/// exchange thread had to reach the guard to replace a worker, and a second handle to a `Lifecycle`
/// job object is a second handle that can be closed while the first is still adopting children. The
/// ending therefore takes the pool's own `&Arc<WorkerPool>` instead of a cloned guard, so there is
/// still exactly one.
#[derive(Debug)]
pub struct LifecycleGuard {
    #[cfg(windows)]
    job: windows_sys::Win32::Foundation::HANDLE,
}

// The job handle is owned exclusively by this guard; the type-level invariant
// is what makes using it from another thread sound.
unsafe impl Send for LifecycleGuard {}

// The guard is only ever *used* (adopting a child into the job), and adopting is
// safe from any thread: the job object itself is safe to share. Marking it
// `Sync` is what lets a pool that holds it be shared across the request threads,
// which is exactly what per-slot parallelism needs.
unsafe impl Sync for LifecycleGuard {}

impl LifecycleGuard {
    /// Create the guard for this process's children.
    pub fn new(name: &str) -> std::io::Result<Self> {
        #[cfg(windows)]
        {
            let _ = name;
            Ok(LifecycleGuard { job: create_job()? })
        }
        #[cfg(not(windows))]
        {
            let _ = name;
            Ok(LifecycleGuard {})
        }
    }

    /// Attach a freshly spawned child, so it dies with us.
    ///
    /// Failing to attach is worth surfacing: it is the difference between
    /// clean shutdown and a pile of orphans.
    ///
    /// **On Unix there is nothing to attach**, and that is not a gap: the protection is
    /// installed at spawn time instead, by [`configure_command`] — the process group and, on
    /// Linux, `PR_SET_PDEATHSIG`. It has to be installed there because `PR_SET_PDEATHSIG` is
    /// set by the *child* on itself (`prctl` takes no pid), and a process group is a spawn-time
    /// attribute. Everything a guard could do from here would be a second, racier path to the
    /// same state, so this only confirms the child exists.
    ///
    /// The Windows half remains here because a job object *can* adopt a running process, and
    /// `AssignProcessToJobObject` has to be called after the spawn.
    pub fn adopt(&self, child: &Child) -> std::io::Result<()> {
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            use windows_sys::Win32::System::JobObjects::AssignProcessToJobObject;

            let process = child.as_raw_handle() as windows_sys::Win32::Foundation::HANDLE;
            // SAFETY: both handles are valid for the life of the call.
            let ok = unsafe { AssignProcessToJobObject(self.job, process) };
            if ok == 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        }
        #[cfg(not(windows))]
        {
            // A child with no pid has already been reaped; there is nothing to protect.
            let _ = child.id();
            Ok(())
        }
    }
}

/// Kill a child, and on Unix everything in its process group.
///
/// **The group rather than the pid**, and that difference is the point of
/// `process_group(0)` in [`configure_command`]: a PHP worker that forks — a prefork
/// supervisor, a `pcntl_fork` pool, which is what a fork-based worker *is* — has children of
/// its own, and killing only the pid it started with leaves those children running with
/// nothing holding their configuration. The group is also what makes `spawn_cgi` clean,
/// because a `php-cgi` reached through a wrapper script is the start of a tree rather than a
/// single process.
///
/// `SIGKILL`, because every caller is already past the polite stage. The worker pool sends
/// a `CONTROL drain` frame and closes the connection first — that is the graceful half — and
/// [`Slot`](crate::worker_pool) waits out `EXIT_TIMEOUT` before reaching here. The CGI pool
/// has no protocol to drain: `php-cgi` is either between requests or not answering. A caller
/// that wants grace has already had it, and a second signal vocabulary would need a second
/// deadline to decide when to escalate.
///
/// The group is derived as `pid == pgid`, which `process_group(0)` makes true and nothing in
/// this crate changes. The check is made rather than assumed so that a child spawned without
/// [`configure_command`] — a test helper, or a future spawn site that forgot it — degrades to
/// killing one pid instead of signalling an unrelated group. Our own pid is refused for the
/// same reason: `killpg` on it would signal *our* group and take the server down.
pub fn kill(child: &mut Child) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        // SAFETY: `getpgid` on a pid we spawned is a documented, safe call.
        let pid = child.id();
        let pgid = unsafe { libc::getpgid(pid as libc::pid_t) };
        if pgid <= 0 || pgid as u32 != pid || pid == std::process::id() {
            return child.kill();
        }

        // SAFETY: the pid is a group leader we created, and SIGKILL cannot be caught.
        if unsafe { libc::killpg(pgid, libc::SIGKILL) } == 0 {
            return Ok(());
        }
        let err = std::io::Error::last_os_error();
        // The group emptied between the two calls. That is the ordinary recycle race, and it
        // is not a failure: `Child::wait` is the authority on whether the process exited.
        if err.raw_os_error() == Some(libc::ESRCH) {
            return Ok(());
        }
        Err(err)
    }
    #[cfg(not(unix))]
    {
        child.kill()
    }
}

#[cfg(windows)]
impl Drop for LifecycleGuard {
    fn drop(&mut self) {
        use windows_sys::Win32::Foundation::CloseHandle;
        // Closing the last handle to the job triggers KILL_ON_JOB_CLOSE.
        // SAFETY: the handle was created by `create_job` and is closed once.
        unsafe {
            CloseHandle(self.job);
        }
    }
}

#[cfg(windows)]
fn create_job() -> std::io::Result<windows_sys::Win32::Foundation::HANDLE> {
    use windows_sys::Win32::System::JobObjects::{
        CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    // SAFETY: a null name and a null security descriptor are both documented as
    // valid, and the returned handle is checked before use.
    let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if job.is_null() {
        return Err(std::io::Error::last_os_error());
    }

    let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;

    // SAFETY: `limits` is a correctly sized, initialised struct and the handle
    // is valid.
    let ok = unsafe {
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            std::ptr::addr_of_mut!(limits).cast(),
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    };
    if ok == 0 {
        let err = std::io::Error::last_os_error();
        // SAFETY: the handle was created above and has not been closed.
        unsafe {
            use windows_sys::Win32::Foundation::CloseHandle;
            CloseHandle(job);
        }
        return Err(err);
    }

    Ok(job)
}

/// Spawn-side configuration that makes children die with their parent.
///
/// Kept as a single place so every spawn site gets the same treatment, and so the Unix-only
/// calls are compiled only where they exist.
///
/// The **whole** protection is installed here on Unix, and that is deliberate rather than
/// tidy: `PR_SET_PDEATHSIG` is set by the child on itself, because `prctl` takes no pid, and a
/// process group is a spawn-time attribute. There is no later moment at which either can be
/// done, so a guard that appeared to do it would be lying.
///
/// **A command configured here must be started with [`spawn`], not `Command::spawn`.** The
/// hook below records the *calling thread* as the child's parent, so a child forked from a
/// thread that returns is killed by the signal this very hook asked for. [`spawn`] forks on a
/// thread that outlives every child; calling `Command::spawn` directly is the bug this module
/// documents at the top.
pub fn configure_command(command: &mut std::process::Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Put the child in its own process group, so `kill` can reach the child *and*
        // anything it spawns, and our own group is untouched.
        command.process_group(0);

        // And the half a group cannot do. A group is something *we* signal, so it only helps
        // while we are alive to signal it; a `SIGKILL`ed or aborted server gets no handler and
        // no turn. `PR_SET_PDEATHSIG` moves that into the kernel: the child is killed when its
        // parent goes, however the parent goes.
        //
        // It runs between `fork` and `exec`, where only async-signal-safe work is legal — no
        // allocation, no locking, no logging. `prctl` on an integer argument is a single
        // syscall, which is why it is one of the few things that honestly belongs in a
        // `pre_exec` hook.
        //
        // Linux only. macOS and the BSDs have no `prctl`; FreeBSD's `procctl(PROC_REAP)` and
        // the kqueue `EVFILT_PROC` approach both need a supervisor thread per child, which is a
        // different design. Those platforms keep the process group, which is what every server
        // does in practice, and the gap is named here rather than covered up.
        #[cfg(target_os = "linux")]
        unsafe {
            command.pre_exec(|| {
                // `PR_SET_PDEATHSIG` = 1. The argument is the signal to deliver.
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NEW_PROCESS_GROUP so a console ctrl event can be targeted at
        // the child rather than at us; job objects handle the reaping.
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        command.creation_flags(CREATE_NEW_PROCESS_GROUP);
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = command;
    }
}

/// One job handed to the spawn thread: it spawns and answers on its own channel.
type Job = Box<dyn FnOnce() + Send + 'static>;

/// The thread that owns every `fork`, created once and living for the life of the process.
///
/// **Why a thread at all.** `PR_SET_PDEATHSIG` — see [`configure_command`] — is tied to the
/// thread that called `fork`, so a child forked from a thread that then returns is killed as
/// soon as that thread does. That is not a long shot: the pool spawns its slots on
/// `std::thread::scope` threads so start-up is parallel, and those return as soon as a slot
/// is listening. Every child then died a few microseconds later, which looked like a slot
/// that served the first request and refused the second. Forking here, on a thread nothing
/// else owns, decouples the child's lifetime from whichever thread asked for it.
///
/// Cached as a `OnceLock` of a `Result` rather than retried: a thread that cannot be created
/// is a process out of resources, and a retry per spawn would only make the failure slower.
static SPAWNER: std::sync::OnceLock<std::result::Result<std::sync::mpsc::Sender<Job>, String>> =
    std::sync::OnceLock::new();

fn sender() -> std::io::Result<&'static std::sync::mpsc::Sender<Job>> {
    SPAWNER
        .get_or_init(|| {
            let (jobs, queue) = std::sync::mpsc::channel::<Job>();
            std::thread::Builder::new()
                .name("volucer-spawn".to_string())
                .spawn(move || {
                    for job in queue {
                        // A panicking job must not take the spawner with it: every later
                        // `spawn` in the process would fail for a fault that belonged to one
                        // child. The caller is told through its own disconnected channel.
                        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(job));
                    }
                })
                .map(|_| jobs)
                .map_err(|err| err.to_string())
        })
        .as_ref()
        .map_err(|reason| std::io::Error::other(reason.clone()))
}

/// Start a configured child on the process-wide spawn thread.
///
/// The replacement for `Command::spawn` at every site that follows [`configure_command`],
/// and the only correct one on Linux: see [`SPAWNER`] for what forking on a short-lived
/// thread costs. It is a drop-in — same signature, same error — so a caller reads it as
/// "spawn, but somewhere that lasts".
///
/// The spawn happens on another thread, which means this blocks until that thread gets to
/// the job. The work there is one `fork` and one `exec` — measured at about 2.5 ms per
/// `php-cgi` on the development machine — and callers that want several at once still get it
/// from the threads they call this from; the waits that dominate start-up (readiness probes,
/// the settle after a connect) stay where they were, in the caller.
pub fn spawn(command: std::process::Command) -> std::io::Result<Child> {
    let (result_tx, result_rx) = std::sync::mpsc::sync_channel(1);
    // `&mut` binding, because `Command::spawn` takes one and the closure owns the value.
    let mut command = command;
    sender()?
        .send(Box::new(move || {
            // Discarded deliberately: a send only fails when the caller gave up, and there is
            // nothing useful to do about a child whose requester is gone.
            let _ = result_tx.send(command.spawn());
        }))
        .map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "the spawn thread has stopped",
            )
        })?;
    result_rx.recv().unwrap_or_else(|_| {
        Err(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "the spawn thread panicked instead of starting the child",
        ))
    })
}

#[cfg(test)]
mod tests {
    // `Duration` is used only by the Linux-gated thread tests below, so it must be gated too
    // or the Windows build sees an unused import.
    #[cfg(target_os = "linux")]
    use std::time::Duration;

    use super::*;

    fn trivial_sleep() -> std::process::Command {
        let mut command = std::process::Command::new(if cfg!(windows) { "cmd" } else { "sleep" });
        #[cfg(windows)]
        command.args(["/C", "exit 0"]);
        #[cfg(not(windows))]
        command.arg("0");
        command
    }

    #[test]
    fn a_guard_can_be_created_and_adopt_a_process() {
        let guard = LifecycleGuard::new("volucer-test").expect("create the guard");
        let mut command = trivial_sleep();
        configure_command(&mut command);

        let mut child = spawn(command).expect("spawn a trivial process");
        guard.adopt(&child).expect("adopt the child");
        let _ = child.wait();
    }

    #[test]
    fn configuring_a_command_is_infallible_and_idempotent() {
        let mut command = trivial_sleep();
        configure_command(&mut command);
        configure_command(&mut command);
    }

    /// A child in its own group is killed **with** its own children.
    ///
    /// This is the regression the file exists for. An earlier version killed one pid, so a
    /// worker that forked — the whole point of the fork experiment — left every one of its
    /// children running after the server stopped it.
    #[cfg(unix)]
    #[test]
    fn killing_a_child_kills_its_children_too() {
        use std::io::Read;

        // A shell that starts a long sleep in the background, prints its pid, then waits for
        // ever. It is a forking worker in one line.
        let mut command = std::process::Command::new("sh");
        command
            .arg("-c")
            .arg("sleep 60 & echo $!; wait")
            .stdout(std::process::Stdio::piped());
        configure_command(&mut command);

        let mut child = spawn(command).expect("spawn the shim");
        let mut stdout = child.stdout.take().expect("stdout was piped");
        let mut buffer = [0u8; 32];
        let read = stdout.read(&mut buffer).expect("read the grandchild pid");
        let text = String::from_utf8_lossy(&buffer[..read]);
        let grandchild: i32 = text.trim().parse().expect("a pid on the first line");

        kill(&mut child).expect("kill the group");
        let _ = child.wait();

        // The grandchild is not our child, so `wait` cannot reap it and there is nothing to
        // synchronise on. A short poll is the only portable question, and `kill(pid, 0)` is
        // it: success means something still holds that pid.
        let mut gone = false;
        for _ in 0..100 {
            // SAFETY: signal 0 performs the existence check and delivers nothing.
            if unsafe { libc::kill(grandchild, 0) } == -1 {
                gone = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(
            gone,
            "the grandchild {grandchild} survived a kill of its group"
        );
    }

    /// `configure_command` really does make the child a group leader, which is what `kill`
    /// relies on to decide between `killpg` and `Child::kill`.
    #[cfg(unix)]
    #[test]
    fn a_configured_child_is_its_own_group_leader() {
        let mut command = std::process::Command::new("sleep");
        command.arg("2");
        configure_command(&mut command);
        let mut child = spawn(command).expect("spawn");

        // SAFETY: `getpgid` on a pid we just spawned is a documented, safe call.
        let pgid = unsafe { libc::getpgid(child.id() as libc::pid_t) };
        assert_eq!(
            child.id() as libc::pid_t,
            pgid,
            "the child leads its own group"
        );
        kill(&mut child).expect("kill it");
        let _ = child.wait();
    }

    /// A child spawned without `configure_command` is killed one pid at a time, never by
    /// signalling a group it does not lead.
    ///
    /// The guard that matters: `killpg` on a pid that is not a pgid signals whatever group
    /// that number names, and when the number happens to be ours the server kills itself.
    #[cfg(unix)]
    #[test]
    fn an_ungrouped_child_falls_back_to_killing_the_pid() {
        let mut child = std::process::Command::new("sleep")
            .arg("2")
            .spawn()
            .expect("spawn");

        // SAFETY: `getpgid` is safe for a live pid, and this child inherited our group.
        let pgid = unsafe { libc::getpgid(child.id() as libc::pid_t) };
        assert_ne!(
            child.id() as libc::pid_t,
            pgid,
            "an unconfigured child shares our group, which is the hazard"
        );

        kill(&mut child).expect("kill it");
        let _ = child.wait();
    }

    /// The `pre_exec` hook must not make `spawn` fail: an error in it surfaces as a failed
    /// spawn, which is the shape a bad syscall would take.
    ///
    /// `PR_SET_PDEATHSIG` itself cannot be observed from in-process without killing the test
    /// runner, so what the kernel did is asserted by the two tests below instead — they
    /// observe a real child, and one of them observes the *absence* of the protection so that
    /// the other cannot be vacuous.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_configured_command_still_spawns_and_reaps() {
        let mut command = trivial_sleep();
        configure_command(&mut command);
        let mut child = spawn(command).expect("a hooked spawn still works");
        let status = child.wait().expect("and reaps");
        assert!(status.success(), "and the child ran to completion");
    }

    /// The state letter from `/proc/<pid>/stat`, or `None` once the entry is gone.
    ///
    /// `kill(pid, 0)` is not the question: it succeeds for a zombie, and a zombie is exactly
    /// what a `SIGKILL`ed child is until somebody reaps it. The letter after the command name
    /// is the only honest answer, and the command name is parenthesised and may itself contain
    /// spaces and `)`, so the split is on the **last** one.
    #[cfg(target_os = "linux")]
    fn process_state(pid: u32) -> Option<char> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        stat.rsplit_once(')')?.1.trim_start().chars().next()
    }

    /// **A child forked from a thread that then exits must still be alive.**
    ///
    /// This is the regression that took `Test (ubuntu-latest)` down. `PR_SET_PDEATHSIG` is
    /// bound to the thread that called `fork`, not to the process, so a `php-cgi` forked from
    /// one of `CgiPool`'s scoped start-up threads was `SIGKILL`ed as soon as that thread
    /// returned — microseconds after it began listening. The symptom was a 502 on the request
    /// after the one that had just been served, which is why it first looked like a flake.
    ///
    /// The thread here is a `scope` thread that returns immediately, the same shape the pool
    /// used, and the assertion is on the child itself rather than on the route that spawned it.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_child_spawned_from_a_short_lived_thread_outlives_it() {
        let mut child = std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    let mut command = std::process::Command::new("sleep");
                    command.arg("30");
                    configure_command(&mut command);
                    spawn(command).expect("spawn on the spawn thread")
                })
                .join()
                .expect("the spawning thread did not panic")
        });

        // Long enough for the forking thread to be gone and for the kernel to have delivered
        // the signal if it was going to — the real failure showed up within microseconds.
        std::thread::sleep(Duration::from_millis(300));
        let state = process_state(child.id());
        assert!(
            matches!(state, Some('S' | 'R' | 'D')),
            "the child {} should still be running after its spawning thread exited, but its \
             state is {state:?} (Z means the kernel killed it when the thread went away)",
            child.id()
        );

        kill(&mut child).expect("kill it");
        let _ = child.wait();
    }

    /// The control for the test above: forking on a short-lived thread **directly** does kill
    /// the child.
    ///
    /// Without this, `a_child_spawned_from_a_short_lived_thread_outlives_it` would pass on a
    /// platform that ignored the hook entirely and prove nothing — the point is that the
    /// assertion above can fail, and this shows it failing for the reason claimed. It also
    /// pins the kernel behaviour the fix rests on, so a future change to the hook that quietly
    /// stopped installing `PR_SET_PDEATHSIG` would be caught here rather than as 502s.
    #[cfg(target_os = "linux")]
    #[test]
    fn forking_on_a_short_lived_thread_kills_the_child() {
        let mut child = std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    let mut command = std::process::Command::new("sleep");
                    command.arg("30");
                    configure_command(&mut command);
                    command.spawn().expect("spawn directly, the wrong way")
                })
                .join()
                .expect("the spawning thread did not panic")
        });

        std::thread::sleep(Duration::from_millis(300));
        let state = process_state(child.id());
        assert!(
            !matches!(state, Some('S' | 'R' | 'D')),
            "forking directly from a short-lived thread should kill the child, but {} is still \
             in state {state:?} — if this passes, `spawn` is not the protection that was \
             measured",
            child.id()
        );

        let _ = child.wait();
    }
}
