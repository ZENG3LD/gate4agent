#[cfg(windows)]
mod windows {
    use std::ffi::c_void;
    use std::io;
    use std::mem::{size_of, zeroed};
    use std::os::windows::io::AsRawHandle;
    use std::os::windows::process::CommandExt;
    use std::process::{Child, Command, ExitStatus};
    use std::ptr::{null, null_mut};
    use std::time::{Duration, Instant};

    type Bool = i32;
    type Dword = u32;
    type Handle = *mut c_void;

    const CREATE_SUSPENDED: Dword = 0x0000_0004;
    const CREATE_NEW_PROCESS_GROUP: Dword = 0x0000_0200;
    const CREATE_NO_WINDOW: Dword = 0x0800_0000;
    const JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE: Dword = 0x0000_2000;
    const JOB_OBJECT_BASIC_ACCOUNTING_INFORMATION: Dword = 1;
    const JOB_OBJECT_EXTENDED_LIMIT_INFORMATION: Dword = 9;
    const SEM_FAILCRITICALERRORS: Dword = 0x0001;
    const SEM_NOGPFAULTERRORBOX: Dword = 0x0002;
    const SEM_NOOPENFILEERRORBOX: Dword = 0x8000;
    const TH32CS_SNAPTHREAD: Dword = 0x0000_0004;
    const THREAD_SUSPEND_RESUME: Dword = 0x0002;
    const WER_FAULT_REPORTING_NO_UI: Dword = 0x0020;
    const INVALID_HANDLE_VALUE: Handle = -1isize as Handle;
    const MAX_TIMEOUT_MS: u64 = 10 * 60 * 1000;
    const CLEANUP_TIMEOUT: Duration = Duration::from_secs(5);
    const THREAD_DISCOVERY_TIMEOUT: Duration = Duration::from_secs(2);
    const POLL_INTERVAL: Duration = Duration::from_millis(25);

    #[repr(C)]
    struct JobObjectBasicLimitInformation {
        per_process_user_time_limit: i64,
        per_job_user_time_limit: i64,
        limit_flags: Dword,
        minimum_working_set_size: usize,
        maximum_working_set_size: usize,
        active_process_limit: Dword,
        affinity: usize,
        priority_class: Dword,
        scheduling_class: Dword,
    }

    #[repr(C)]
    struct IoCounters {
        read_operation_count: u64,
        write_operation_count: u64,
        other_operation_count: u64,
        read_transfer_count: u64,
        write_transfer_count: u64,
        other_transfer_count: u64,
    }

    #[repr(C)]
    struct JobObjectExtendedLimitInformation {
        basic_limit_information: JobObjectBasicLimitInformation,
        io_info: IoCounters,
        process_memory_limit: usize,
        job_memory_limit: usize,
        peak_process_memory_used: usize,
        peak_job_memory_used: usize,
    }

    #[repr(C)]
    struct JobObjectBasicAccountingInformation {
        total_user_time: i64,
        total_kernel_time: i64,
        this_period_total_user_time: i64,
        this_period_total_kernel_time: i64,
        total_page_fault_count: Dword,
        total_processes: Dword,
        active_processes: Dword,
        total_terminated_processes: Dword,
    }

    #[repr(C)]
    struct ThreadEntry32 {
        size: Dword,
        usage_count: Dword,
        thread_id: Dword,
        owner_process_id: Dword,
        base_priority: i32,
        delta_priority: i32,
        flags: Dword,
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn AssignProcessToJobObject(job: Handle, process: Handle) -> Bool;
        fn CloseHandle(handle: Handle) -> Bool;
        fn CreateJobObjectW(attributes: *const c_void, name: *const u16) -> Handle;
        fn CreateToolhelp32Snapshot(flags: Dword, process_id: Dword) -> Handle;
        fn OpenThread(desired_access: Dword, inherit_handle: Bool, thread_id: Dword) -> Handle;
        fn QueryInformationJobObject(
            job: Handle,
            information_class: Dword,
            information: *mut c_void,
            information_length: Dword,
            return_length: *mut Dword,
        ) -> Bool;
        fn ResumeThread(thread: Handle) -> Dword;
        fn SetErrorMode(mode: Dword) -> Dword;
        fn SetInformationJobObject(
            job: Handle,
            information_class: Dword,
            information: *const c_void,
            information_length: Dword,
        ) -> Bool;
        fn TerminateJobObject(job: Handle, exit_code: Dword) -> Bool;
        fn Thread32First(snapshot: Handle, entry: *mut ThreadEntry32) -> Bool;
        fn Thread32Next(snapshot: Handle, entry: *mut ThreadEntry32) -> Bool;
        fn WerSetFlags(flags: Dword) -> i32;
    }

    struct OwnedHandle(Handle);

    impl OwnedHandle {
        fn new(handle: Handle) -> io::Result<Self> {
            if handle.is_null() || handle == INVALID_HANDLE_VALUE {
                Err(io::Error::last_os_error())
            } else {
                Ok(Self(handle))
            }
        }

        fn raw(&self) -> Handle {
            self.0
        }
    }

    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }

    struct Job {
        handle: OwnedHandle,
    }

    impl Job {
        fn new() -> io::Result<Self> {
            let handle = OwnedHandle::new(unsafe { CreateJobObjectW(null(), null()) })?;
            let mut limits: JobObjectExtendedLimitInformation = unsafe { zeroed() };
            limits.basic_limit_information.limit_flags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let configured = unsafe {
                SetInformationJobObject(
                    handle.raw(),
                    JOB_OBJECT_EXTENDED_LIMIT_INFORMATION,
                    &limits as *const _ as *const c_void,
                    size_of::<JobObjectExtendedLimitInformation>() as Dword,
                )
            };
            if configured == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(Self { handle })
        }

        fn assign(&self, child: &Child) -> io::Result<()> {
            let assigned = unsafe {
                AssignProcessToJobObject(self.handle.raw(), child.as_raw_handle() as Handle)
            };
            if assigned == 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        }

        fn active_processes(&self) -> io::Result<Dword> {
            let mut accounting: JobObjectBasicAccountingInformation = unsafe { zeroed() };
            let queried = unsafe {
                QueryInformationJobObject(
                    self.handle.raw(),
                    JOB_OBJECT_BASIC_ACCOUNTING_INFORMATION,
                    &mut accounting as *mut _ as *mut c_void,
                    size_of::<JobObjectBasicAccountingInformation>() as Dword,
                    null_mut(),
                )
            };
            if queried == 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(accounting.active_processes)
            }
        }

        fn terminate(&self, exit_code: Dword) -> io::Result<()> {
            if unsafe { TerminateJobObject(self.handle.raw(), exit_code) } == 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        }

        fn terminate_and_verify_empty(&self, exit_code: Dword) -> io::Result<()> {
            if self.active_processes()? != 0 {
                self.terminate(exit_code)?;
            }
            let deadline = Instant::now() + CLEANUP_TIMEOUT;
            loop {
                if self.active_processes()? == 0 {
                    return Ok(());
                }
                if Instant::now() >= deadline {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "job still contains active processes after termination",
                    ));
                }
                std::thread::sleep(POLL_INTERVAL);
            }
        }
    }

    fn suppress_fault_dialogs() {
        unsafe {
            SetErrorMode(
                SEM_FAILCRITICALERRORS | SEM_NOGPFAULTERRORBOX | SEM_NOOPENFILEERRORBOX,
            );
            let _ = WerSetFlags(WER_FAULT_REPORTING_NO_UI);
        }
    }

    fn open_primary_thread(process_id: Dword) -> io::Result<OwnedHandle> {
        let deadline = Instant::now() + THREAD_DISCOVERY_TIMEOUT;
        loop {
            let snapshot = OwnedHandle::new(unsafe {
                CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0)
            })?;
            let mut entry: ThreadEntry32 = unsafe { zeroed() };
            entry.size = size_of::<ThreadEntry32>() as Dword;
            let mut has_entry = unsafe { Thread32First(snapshot.raw(), &mut entry) } != 0;
            while has_entry {
                if entry.owner_process_id == process_id {
                    return OwnedHandle::new(unsafe {
                        OpenThread(THREAD_SUSPEND_RESUME, 0, entry.thread_id)
                    });
                }
                has_entry = unsafe { Thread32Next(snapshot.raw(), &mut entry) } != 0;
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "suspended process primary thread was not found",
                ));
            }
            std::thread::sleep(POLL_INTERVAL);
        }
    }

    fn resume_primary_thread(child: &Child) -> io::Result<()> {
        let thread = open_primary_thread(child.id())?;
        let previous_suspend_count = unsafe { ResumeThread(thread.raw()) };
        if previous_suspend_count == Dword::MAX {
            Err(io::Error::last_os_error())
        } else if previous_suspend_count != 1 {
            Err(io::Error::new(
                io::ErrorKind::Other,
                format!(
                    "unexpected primary thread suspend count: {previous_suspend_count}"
                ),
            ))
        } else {
            Ok(())
        }
    }

    fn cleanup(job: &Job, child: &mut Child, exit_code: Dword) -> io::Result<()> {
        job.terminate_and_verify_empty(exit_code)?;
        wait_for_child_exit(child, CLEANUP_TIMEOUT)
    }

    fn wait_for_child_exit(child: &mut Child, timeout: Duration) -> io::Result<()> {
        let deadline = Instant::now() + timeout;
        loop {
            match child.try_wait()? {
                Some(_) => return Ok(()),
                None if Instant::now() < deadline => std::thread::sleep(POLL_INTERVAL),
                None => {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "supervised root process did not exit after termination",
                    ));
                }
            }
        }
    }

    fn terminate_unassigned(child: &mut Child) -> io::Result<()> {
        if child.try_wait()?.is_some() {
            return Ok(());
        }
        child.kill()?;
        wait_for_child_exit(child, CLEANUP_TIMEOUT)
    }

    fn fail_before_assignment(child: &mut Child, message: &str, error: io::Error) -> i32 {
        let cleanup_error = terminate_unassigned(child).err();
        eprintln!("{message}: {error}");
        if let Some(cleanup_error) = cleanup_error {
            eprintln!("failed to terminate unassigned suspended process: {cleanup_error}");
        }
        2
    }

    fn parse_arguments() -> Result<(Duration, std::path::PathBuf, Vec<std::ffi::OsString>), i32> {
        let mut arguments = std::env::args_os().skip(1);
        let timeout_ms = arguments
            .next()
            .and_then(|value| value.to_str().and_then(|value| value.parse::<u64>().ok()))
            .filter(|value| (1..=MAX_TIMEOUT_MS).contains(value))
            .ok_or_else(|| {
                eprintln!(
                    "usage: windows-headless-supervisor <timeout-ms> <absolute-executable> [args...]"
                );
                2
            })?;
        let executable = arguments.next().map(std::path::PathBuf::from).ok_or_else(|| {
            eprintln!(
                "usage: windows-headless-supervisor <timeout-ms> <absolute-executable> [args...]"
            );
            2
        })?;
        if !executable.is_absolute()
            || !executable.is_file()
            || !executable
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("exe"))
        {
            eprintln!("supervised executable must be an existing absolute .exe file");
            return Err(2);
        }
        Ok((
            Duration::from_millis(timeout_ms),
            executable,
            arguments.collect(),
        ))
    }

    fn status_code(status: ExitStatus) -> i32 {
        status.code().unwrap_or(1)
    }

    pub fn run() -> i32 {
        suppress_fault_dialogs();

        let (timeout, executable, arguments) = match parse_arguments() {
            Ok(arguments) => arguments,
            Err(code) => return code,
        };
        let job = match Job::new() {
            Ok(job) => job,
            Err(error) => {
                eprintln!("failed to create containment job: {error}");
                return 2;
            }
        };
        let mut child = match Command::new(&executable)
            .args(arguments)
            .env("GATE4AGENT_HEADLESS_SUPERVISOR", "1")
            .creation_flags(CREATE_SUSPENDED | CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP)
            .spawn()
        {
            Ok(child) => child,
            Err(error) => {
                eprintln!("failed to start supervised executable: {error}");
                return 2;
            }
        };
        if let Err(error) = job.assign(&child) {
            return fail_before_assignment(
                &mut child,
                "failed to assign supervised executable to containment job",
                error,
            );
        }
        if let Err(error) = resume_primary_thread(&child) {
            let cleanup_error = cleanup(&job, &mut child, 2).err();
            eprintln!("failed to resume supervised executable: {error}");
            if let Some(cleanup_error) = cleanup_error {
                eprintln!("failed to verify containment cleanup: {cleanup_error}");
            }
            return 2;
        }

        let deadline = Instant::now() + timeout;
        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    if let Err(error) = job.terminate_and_verify_empty(2) {
                        eprintln!("failed to verify containment cleanup: {error}");
                        return 2;
                    }
                    return status_code(status);
                }
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(POLL_INTERVAL);
                }
                Ok(None) => {
                    let cleanup_result = cleanup(&job, &mut child, 124);
                    if let Err(error) = cleanup_result {
                        eprintln!("failed to verify containment cleanup: {error}");
                        return 2;
                    }
                    eprintln!("supervised executable timed out; contained process tree terminated");
                    return 124;
                }
                Err(error) => {
                    let cleanup_error = cleanup(&job, &mut child, 2).err();
                    eprintln!("failed to observe supervised executable: {error}");
                    if let Some(cleanup_error) = cleanup_error {
                        eprintln!("failed to verify containment cleanup: {cleanup_error}");
                    }
                    return 2;
                }
            }
        }
    }
}

#[cfg(windows)]
fn main() {
    std::process::exit(windows::run());
}

#[cfg(not(windows))]
fn main() {
    eprintln!("windows-headless-supervisor is only available on Windows");
    std::process::exit(2);
}
