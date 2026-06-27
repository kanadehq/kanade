//! Windows Job Object wrapper — assign a spawned process (and every
//! descendant it later creates) to a kernel Job so a single
//! `TerminateJobObject` tears down the WHOLE process tree.
//!
//! Why this exists: `tokio::process::Child::kill` (normal `system`
//! path) and `TerminateProcess` (the `run_as: user / system_gui`
//! path) only kill the IMMEDIATE child we spawned — the `powershell`
//! / `cmd` host. A script that launches a longer-lived grandchild
//! (e.g. a job that runs `claude`, which itself forks helpers) left
//! those grandchildren orphaned after a "強制終了" / timeout.
//!
//! Worse than the orphan: those grandchildren inherit the
//! stdout/stderr pipe *write* handles, so the agent's `read_to_end`
//! never sees EOF. `run_command_with_kill` then blocked forever on
//! the pipe drain, no `ExecResult` was ever enqueued, and the
//! `execution_results` row stayed `finished_at IS NULL` — i.e. the
//! Activity page was stuck on "実行中" even though the kill signal
//! had been delivered and the host process was dead.
//!
//! Assigning the host to a Job at spawn time and calling
//! `TerminateJobObject` on kill/timeout kills the whole tree at once,
//! which closes every inherited pipe handle and unblocks the drain.
//!
//! Deliberately NOT using `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`: we
//! only ever terminate the Job on the kill/timeout paths. On a clean
//! script exit we just close the Job handle, which leaves any
//! intentionally-detached background process the script may have
//! launched alive — preserving fire-and-forget semantics that
//! kill-on-close would silently break.

#[cfg(target_os = "windows")]
mod imp {
    use anyhow::{Result, anyhow};
    use tracing::warn;
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject, TerminateJobObject,
    };
    use windows::core::PCWSTR;

    /// Owns an unnamed Job Object handle. `terminate` kills every
    /// process currently assigned to the Job (the spawned host plus
    /// every descendant). Drop merely closes the handle — it does
    /// NOT kill the tree (see module docs).
    pub struct JobObject {
        handle: HANDLE,
    }

    // SAFETY: a HANDLE is an opaque kernel-table index. `JobObject`
    // owns it exclusively (move-only, single CloseHandle on Drop), so
    // sharing the wrapper across threads can't double-close or race
    // the underlying object.
    unsafe impl Send for JobObject {}
    unsafe impl Sync for JobObject {}

    impl JobObject {
        /// Create an unnamed Job and assign `process` (a raw process
        /// HANDLE — e.g. `tokio::process::Child::raw_handle`, or a
        /// `PROCESS_INFORMATION::hProcess`) to it.
        ///
        /// Caller note on races: assignment must happen before the
        /// process spawns descendants, or those early descendants
        /// escape the Job. The `run_as: user` path spawns
        /// `CREATE_SUSPENDED` and assigns before `ResumeThread`, so it
        /// is fully race-free. The `system` (tokio) path can't suspend
        /// the child, so it assigns immediately after `spawn()`; the
        /// window before the host (`powershell` / `cmd`) has even
        /// finished initializing — let alone launched a child — is
        /// microseconds, so in practice every descendant lands inside
        /// the Job.
        pub fn assign_handle(process: HANDLE) -> Result<Self> {
            unsafe {
                let handle = CreateJobObjectW(None, PCWSTR::null())
                    .map_err(|e| anyhow!("CreateJobObjectW: {e:?}"))?;
                // Wrap immediately so an early return below still
                // closes the handle via Drop.
                let job = JobObject { handle };
                AssignProcessToJobObject(handle, process)
                    .map_err(|e| anyhow!("AssignProcessToJobObject: {e:?}"))?;
                Ok(job)
            }
        }

        /// Like [`assign_handle`] but the Job is `KILL_ON_JOB_CLOSE`: when this
        /// `JobObject` handle is dropped — including the agent process exiting
        /// or crashing, which closes all its handles — the OS kills the whole
        /// assigned tree. Used for the supervised session-agent child (#855) so
        /// it can never outlive its parent agent (no orphan across self-update /
        /// shutdown / crash). Unlike the plain `assign_handle`, here KILL is the
        /// point, so the limit flag is deliberate.
        pub fn assign_handle_kill_on_close(process: HANDLE) -> Result<Self> {
            unsafe {
                let handle = CreateJobObjectW(None, PCWSTR::null())
                    .map_err(|e| anyhow!("CreateJobObjectW: {e:?}"))?;
                let job = JobObject { handle };
                let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                SetInformationJobObject(
                    handle,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const core::ffi::c_void,
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                )
                .map_err(|e| anyhow!("SetInformationJobObject(KILL_ON_JOB_CLOSE): {e:?}"))?;
                AssignProcessToJobObject(handle, process)
                    .map_err(|e| anyhow!("AssignProcessToJobObject: {e:?}"))?;
                Ok(job)
            }
        }

        /// Terminate every process in the Job (whole tree) with exit
        /// code 1. Best-effort: logs and continues on failure so the
        /// caller's outcome bookkeeping isn't derailed.
        pub fn terminate(&self) {
            unsafe {
                if let Err(e) = TerminateJobObject(self.handle, 1) {
                    warn!(
                        target: "kanade_agent::job_object",
                        "TerminateJobObject failed: {e:?}",
                    );
                }
            }
        }
    }

    impl Drop for JobObject {
        fn drop(&mut self) {
            unsafe {
                let _ = CloseHandle(self.handle);
            }
        }
    }
}

#[cfg(target_os = "windows")]
pub use imp::JobObject;

// Non-Windows stub so `Option<JobObject>` typechecks in the shared
// spawn path (`process.rs`). The agent's production target is
// Windows; on other platforms `job` is always `None` and the code
// falls back to the single-process kill. The stub carries no handle
// and `terminate` is a no-op — it never gets constructed off-Windows
// because `assign_handle` (the only constructor) is Windows-only.
#[cfg(not(target_os = "windows"))]
pub struct JobObject;

#[cfg(not(target_os = "windows"))]
impl JobObject {
    pub fn terminate(&self) {}
}
