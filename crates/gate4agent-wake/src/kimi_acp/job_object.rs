//! Windows Job Object that kills every child when the last handle closes.
//! On Unix this is a no-op: `kill_on_drop` covers the normal path; a hard kill
//! of the host still leaves orphans on Windows without a Job Object.

#[cfg(windows)]
mod win {
    use std::io;
    use tokio::process::Child;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, SetInformationJobObject,
        JobObjectExtendedLimitInformation, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_ALL_ACCESS};

    pub struct JobObject(HANDLE);

    impl JobObject {
        /// Creates a kill-on-close Job Object and assigns `child` to it.
        pub fn attach(child: &mut Child) -> Result<Option<JobObject>, String> {
            let pid = child
                .id()
                .ok_or_else(|| "child has no pid".to_string())?;
            unsafe {
                let handle = CreateJobObjectW(std::ptr::null(), std::ptr::null());
                if handle.is_null() || handle == INVALID_HANDLE_VALUE {
                    return Err(format!("CreateJobObjectW: {}", io::Error::last_os_error()));
                }
                let mut info = std::mem::zeroed::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>();
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                if SetInformationJobObject(
                    handle,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const _,
                    std::mem::size_of_val(&info) as u32,
                ) == 0
                {
                    let err = io::Error::last_os_error();
                    CloseHandle(handle);
                    return Err(format!("SetInformationJobObject: {err}"));
                }
                let proc = OpenProcess(PROCESS_ALL_ACCESS, 0, pid);
                if proc.is_null() || proc == INVALID_HANDLE_VALUE {
                    let err = io::Error::last_os_error();
                    CloseHandle(handle);
                    return Err(format!("OpenProcess: {err}"));
                }
                let assigned = AssignProcessToJobObject(handle, proc);
                CloseHandle(proc);
                if assigned == 0 {
                    let err = io::Error::last_os_error();
                    CloseHandle(handle);
                    return Err(format!("AssignProcessToJobObject: {err}"));
                }
                Ok(Some(JobObject(handle)))
            }
        }
    }

    impl Drop for JobObject {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }

    unsafe impl Send for JobObject {}
}

#[cfg(windows)]
pub use win::JobObject;

#[cfg(not(windows))]
pub struct JobObject;

#[cfg(not(windows))]
impl JobObject {
    pub fn attach(_child: &mut tokio::process::Child) -> Result<Option<JobObject>, String> {
        Ok(None)
    }
}
