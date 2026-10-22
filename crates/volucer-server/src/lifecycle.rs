//! Platform process lifecycle handling.
//!
//! The hazard this exists for (`internal/plan.md` §5.3): if the master dies, its
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
//! **Both halves are implemented here.** An earlier version of this file described
//! them and implemented neither: `killpg` appeared nowhere, every kill was
//! `Child::kill` on a single pid, and the group existed only as a comment. The
//! measured consequence was a server `SIGKILL`ed while its children kept running,
//! reparented to init and still holding their ports — precisely the failure §5.3 is
//! about.

use std::process::Child;

/// A guard that must outlive the children it protects.
///
/// Dropping it kills the children it owns, so it has to be held for as long as
/// the server runs and dropped last.
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

#[cfg(test)]
mod tests {
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

        let mut child = command.spawn().expect("spawn a trivial process");
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

        let mut child = command.spawn().expect("spawn the shim");
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
        let mut child = command.spawn().expect("spawn");

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
    /// runner, so what the kernel did is asserted by `scripts/verify-fixes.sh` on the bench
    /// host instead — it `SIGKILL`s a real server and counts the children that survive.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_configured_command_still_spawns_and_reaps() {
        let mut command = trivial_sleep();
        configure_command(&mut command);
        let mut child = command.spawn().expect("a hooked spawn still works");
        let status = child.wait().expect("and reaps");
        assert!(status.success(), "and the child ran to completion");
    }
}
