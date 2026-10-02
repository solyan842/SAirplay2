#[cfg(windows)]
mod implementation {
    use std::io;
    use std::os::windows::io::AsRawHandle;
    use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, ExitStatus};
    use std::sync::Arc;
    use std::thread;
    use std::time::{Duration, Instant};
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, SetInformationJobObject, TerminateJobObject,
        JobObjectExtendedLimitInformation, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    struct JobHandle {
        raw: usize,
    }

    impl JobHandle {
        fn attach(child: &Child) -> io::Result<Self> {
            let job = unsafe { CreateJobObjectW(None, PCWSTR::null()) }
                .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("CreateJobObjectW: {e}")))?;

            let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            if let Err(e) = unsafe {
                SetInformationJobObject(
                    job,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const core::ffi::c_void,
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                )
            } {
                let _ = unsafe { CloseHandle(job) };
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    format!("SetInformationJobObject(KILL_ON_JOB_CLOSE): {e}"),
                ));
            }

            let process = HANDLE(child.as_raw_handle());
            if let Err(e) = unsafe { AssignProcessToJobObject(job, process) } {
                let _ = unsafe { CloseHandle(job) };
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    format!("AssignProcessToJobObject: {e}"),
                ));
            }
            Ok(Self { raw: job.0 as usize })
        }

        fn terminate(&self) -> io::Result<()> {
            let handle = HANDLE(self.raw as *mut core::ffi::c_void);
            unsafe { TerminateJobObject(handle, 1) }
                .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("TerminateJobObject: {e}")))
        }
    }

    impl Drop for JobHandle {
        fn drop(&mut self) {
            let handle = HANDLE(self.raw as *mut core::ffi::c_void);
            let _ = unsafe { CloseHandle(handle) };
        }
    }

    #[derive(Clone)]
    pub struct ManagedChildKiller {
        job: Arc<JobHandle>,
    }

    impl ManagedChildKiller {
        pub fn terminate(&self) -> io::Result<()> {
            self.job.terminate()
        }
    }

    /// Name-independent lifecycle guard for every external SAirplay2 runtime helper.
    ///
    /// The child is attached to a Windows Job Object with KILL_ON_JOB_CLOSE.
    /// Error returns and Drop terminate+reap a live child, while callers can
    /// still allow a bounded graceful shutdown before the same forced cleanup.
    pub struct ManagedChild {
        child: Child,
        job: Arc<JobHandle>,
    }

    impl ManagedChild {
        pub fn spawn(command: &mut Command) -> io::Result<Self> {
            let mut child = command.spawn()?;
            let job = match JobHandle::attach(&child) {
                Ok(job) => Arc::new(job),
                Err(error) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(error);
                }
            };
            Ok(Self { child, job })
        }

        pub fn id(&self) -> u32 { self.child.id() }

        pub fn killer(&self) -> ManagedChildKiller {
            ManagedChildKiller { job: Arc::clone(&self.job) }
        }

        pub fn take_stdin(&mut self) -> Option<ChildStdin> { self.child.stdin.take() }
        pub fn take_stdout(&mut self) -> Option<ChildStdout> { self.child.stdout.take() }
        pub fn take_stderr(&mut self) -> Option<ChildStderr> { self.child.stderr.take() }
        pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> { self.child.try_wait() }
        pub fn wait(&mut self) -> io::Result<ExitStatus> { self.child.wait() }
        pub fn terminate_tree(&self) -> io::Result<()> { self.job.terminate() }

        pub fn terminate_and_wait(&mut self) -> io::Result<ExitStatus> {
            let _ = self.job.terminate();
            self.child.wait()
        }

        pub fn wait_or_terminate(&mut self, grace: Duration) -> io::Result<ExitStatus> {
            let deadline = Instant::now() + grace;
            loop {
                if let Some(status) = self.child.try_wait()? {
                    return Ok(status);
                }
                if Instant::now() >= deadline {
                    let _ = self.job.terminate();
                    return self.child.wait();
                }
                thread::sleep(Duration::from_millis(10));
            }
        }
    }

    impl Drop for ManagedChild {
        fn drop(&mut self) {
            match self.child.try_wait() {
                Ok(Some(_)) => {}
                _ => {
                    let _ = self.job.terminate();
                    let _ = self.child.wait();
                }
            }
        }
    }
}

#[cfg(windows)]
pub use implementation::{ManagedChild, ManagedChildKiller};
