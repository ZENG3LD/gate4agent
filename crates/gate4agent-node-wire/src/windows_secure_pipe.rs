use std::ffi::c_void;
use std::io;
use std::path::{Path, PathBuf};
use std::ptr;
use std::time::Duration;
use tokio::net::windows::named_pipe::{
    ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions,
};
use tokio::time::sleep;
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, GENERIC_ALL, HANDLE, ERROR_INSUFFICIENT_BUFFER,
    ERROR_PIPE_BUSY,
};
use windows_sys::Win32::Security::{
    AddAccessAllowedAceEx, CreateWellKnownSid, GetLengthSid, GetTokenInformation,
    InitializeAcl, InitializeSecurityDescriptor, IsValidSecurityDescriptor, IsValidSid,
    SetSecurityDescriptorControl, SetSecurityDescriptorDacl, SetSecurityDescriptorOwner,
    TokenUser, WinLocalSystemSid, ACCESS_ALLOWED_ACE, ACL, ACL_REVISION, NO_INHERITANCE,
    SECURITY_ATTRIBUTES, SECURITY_DESCRIPTOR, SECURITY_MAX_SID_SIZE, SE_DACL_PROTECTED,
    TOKEN_QUERY, TOKEN_USER,
};
use windows_sys::Win32::System::SystemServices::SECURITY_DESCRIPTOR_REVISION;
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

const PIPE_CONNECT_RETRIES: usize = 100;
const PIPE_CONNECT_RETRY_DELAY_MS: u64 = 20;

pub type LocalServerStream = NamedPipeServer;
pub type LocalClientStream = NamedPipeClient;

pub struct OwnerOnlyLocalListener {
    endpoint: PathBuf,
    pending: Option<LocalServerStream>,
    first_instance: bool,
}

impl OwnerOnlyLocalListener {
    pub async fn bind(endpoint: impl AsRef<Path>) -> io::Result<Self> {
        Ok(Self {
            endpoint: endpoint.as_ref().to_owned(),
            pending: None,
            first_instance: true,
        })
    }

    pub async fn accept(&mut self) -> io::Result<LocalServerStream> {
        if self.pending.is_none() {
            self.pending = Some(create_current_user_pipe_server(
                &self.endpoint,
                self.first_instance,
            )?);
        }
        let Some(pending) = self.pending.as_mut() else {
            return Err(io::Error::other("local pipe listener lost its pending instance"));
        };
        pending.connect().await?;
        self.first_instance = false;
        self.pending
            .take()
            .ok_or_else(|| io::Error::other("local pipe listener lost its connected instance"))
    }
}

struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

struct CurrentUserPipeSecurity {
    descriptor: Box<SECURITY_DESCRIPTOR>,
    _acl: Vec<usize>,
    _current_user_sid: Vec<usize>,
}

impl CurrentUserPipeSecurity {
    fn new() -> io::Result<Self> {
        let current_user_sid_bytes = current_primary_token_user_sid()?;
        let mut current_user_sid = aligned_security_buffer(current_user_sid_bytes.len());
        unsafe {
            ptr::copy_nonoverlapping(
                current_user_sid_bytes.as_ptr(),
                current_user_sid.as_mut_ptr().cast::<u8>(),
                current_user_sid_bytes.len(),
            );
        }
        let current_user_sid_ptr = current_user_sid.as_mut_ptr().cast::<c_void>();

        let mut local_system_sid = aligned_security_buffer(SECURITY_MAX_SID_SIZE as usize);
        let mut local_system_sid_length = SECURITY_MAX_SID_SIZE;
        win32_bool(
            unsafe {
                CreateWellKnownSid(
                    WinLocalSystemSid,
                    ptr::null_mut(),
                    local_system_sid.as_mut_ptr().cast::<c_void>(),
                    &mut local_system_sid_length,
                )
            },
            "create LocalSystem SID",
        )?;

        let current_user_ace_bytes = access_allowed_ace_size(current_user_sid_bytes.len())?;
        let local_system_ace_bytes = access_allowed_ace_size(local_system_sid_length as usize)?;
        let acl_bytes = std::mem::size_of::<ACL>()
            .checked_add(current_user_ace_bytes)
            .and_then(|bytes| bytes.checked_add(local_system_ace_bytes))
            .ok_or_else(|| pipe_security_error("size pipe DACL", "size overflow"))?;
        let acl_length = u32::try_from(acl_bytes)
            .map_err(|_| pipe_security_error("size pipe DACL", "size exceeds u32"))?;
        let mut acl = aligned_security_buffer(acl_bytes);
        let acl_ptr = acl.as_mut_ptr().cast::<ACL>();
        win32_bool(
            unsafe { InitializeAcl(acl_ptr, acl_length, ACL_REVISION) },
            "initialize pipe DACL",
        )?;
        for sid in [
            current_user_sid_ptr,
            local_system_sid.as_mut_ptr().cast::<c_void>(),
        ] {
            win32_bool(
                unsafe {
                    AddAccessAllowedAceEx(
                        acl_ptr,
                        ACL_REVISION,
                        NO_INHERITANCE,
                        GENERIC_ALL,
                        sid,
                    )
                },
                "add pipe DACL entry",
            )?;
        }

        let mut descriptor = Box::new(SECURITY_DESCRIPTOR::default());
        let descriptor_ptr = (&mut *descriptor as *mut SECURITY_DESCRIPTOR).cast::<c_void>();
        win32_bool(
            unsafe {
                InitializeSecurityDescriptor(descriptor_ptr, SECURITY_DESCRIPTOR_REVISION)
            },
            "initialize pipe security descriptor",
        )?;
        win32_bool(
            unsafe { SetSecurityDescriptorOwner(descriptor_ptr, current_user_sid_ptr, 0) },
            "set pipe security owner",
        )?;
        win32_bool(
            unsafe { SetSecurityDescriptorDacl(descriptor_ptr, 1, acl_ptr, 0) },
            "set pipe DACL",
        )?;
        win32_bool(
            unsafe {
                SetSecurityDescriptorControl(
                    descriptor_ptr,
                    SE_DACL_PROTECTED,
                    SE_DACL_PROTECTED,
                )
            },
            "protect pipe DACL",
        )?;
        if unsafe { IsValidSecurityDescriptor(descriptor_ptr) } == 0 {
            return Err(pipe_security_error(
                "validate pipe security descriptor",
                "Windows rejected the constructed descriptor",
            ));
        }

        Ok(Self {
            descriptor,
            _acl: acl,
            _current_user_sid: current_user_sid,
        })
    }

    fn attributes(&self) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: (&*self.descriptor as *const SECURITY_DESCRIPTOR)
                .cast_mut()
                .cast::<c_void>(),
            bInheritHandle: 0,
        }
    }
}

/// Creates a local named-pipe server restricted to the current primary-token user and LocalSystem.
fn create_current_user_pipe_server(
    endpoint: impl AsRef<Path>,
    first_instance: bool,
) -> io::Result<NamedPipeServer> {
    let endpoint = endpoint.as_ref();
    let security = CurrentUserPipeSecurity::new()?;
    let mut attributes = security.attributes();
    let mut options = ServerOptions::new();
    options
        .first_pipe_instance(first_instance)
        .reject_remote_clients(true);
    // CreateNamedPipeW consumes SECURITY_ATTRIBUTES synchronously. Keep the
    // descriptor, DACL, and owner SID alive until this call returns.
    unsafe {
        options.create_with_security_attributes_raw(
            endpoint,
            (&mut attributes as *mut SECURITY_ATTRIBUTES).cast::<c_void>(),
        )
    }
}

pub async fn connect_local_stream(
    endpoint: impl AsRef<Path>,
) -> io::Result<LocalClientStream> {
    let endpoint = endpoint.as_ref();
    let mut last_error = None;
    for _ in 0..PIPE_CONNECT_RETRIES {
        match ClientOptions::new().open(endpoint) {
            Ok(client) => return Ok(client),
            Err(error) => {
                let retryable = matches!(error.kind(), io::ErrorKind::NotFound)
                    || error.raw_os_error() == Some(ERROR_PIPE_BUSY as i32);
                if !retryable {
                    return Err(error);
                }
                last_error = Some(error);
                sleep(Duration::from_millis(PIPE_CONNECT_RETRY_DELAY_MS)).await;
            }
        }
    }
    Err(last_error.unwrap_or_else(|| {
        io::Error::new(io::ErrorKind::NotFound, "named pipe was not available")
    }))
}

fn current_primary_token_user_sid() -> io::Result<Vec<u8>> {
    let mut token = ptr::null_mut();
    win32_bool(
        unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) },
        "open current process primary token",
    )?;
    let token = OwnedHandle(token);

    let mut required = 0;
    let first_call = unsafe {
        GetTokenInformation(token.0, TokenUser, ptr::null_mut(), 0, &mut required)
    };
    if first_call != 0 || unsafe { GetLastError() } != ERROR_INSUFFICIENT_BUFFER || required == 0 {
        return Err(last_pipe_security_error(
            "size current primary token user SID",
        ));
    }
    let mut token_user = aligned_security_buffer(required as usize);
    win32_bool(
        unsafe {
            GetTokenInformation(
                token.0,
                TokenUser,
                token_user.as_mut_ptr().cast::<c_void>(),
                required,
                &mut required,
            )
        },
        "read current primary token user SID",
    )?;
    let sid = unsafe { (*token_user.as_ptr().cast::<TOKEN_USER>()).User.Sid };
    if sid.is_null() || unsafe { IsValidSid(sid) } == 0 {
        return Err(pipe_security_error(
            "validate current primary token user SID",
            "Windows returned an invalid SID",
        ));
    }
    let sid_length = unsafe { GetLengthSid(sid) } as usize;
    let sid_bytes = unsafe { std::slice::from_raw_parts(sid.cast::<u8>(), sid_length) };
    Ok(sid_bytes.to_vec())
}

fn aligned_security_buffer(byte_length: usize) -> Vec<usize> {
    let words = byte_length
        .saturating_add(std::mem::size_of::<usize>() - 1)
        / std::mem::size_of::<usize>();
    vec![0_usize; words]
}

fn access_allowed_ace_size(sid_length: usize) -> io::Result<usize> {
    std::mem::size_of::<ACCESS_ALLOWED_ACE>()
        .checked_sub(std::mem::size_of::<u32>())
        .and_then(|header| header.checked_add(sid_length))
        .ok_or_else(|| pipe_security_error("size pipe DACL entry", "size overflow"))
}

fn win32_bool(result: i32, operation: &'static str) -> io::Result<()> {
    if result == 0 {
        Err(last_pipe_security_error(operation))
    } else {
        Ok(())
    }
}

fn last_pipe_security_error(operation: &'static str) -> io::Error {
    pipe_security_error(operation, io::Error::last_os_error())
}

fn pipe_security_error(operation: &'static str, source: impl std::fmt::Display) -> io::Error {
    io::Error::other(format!("{operation}: {source}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use windows_sys::Win32::Foundation::ERROR_ACCESS_DENIED;
    use windows_sys::Win32::Security::{GetAce, IsValidAcl};
    use windows_sys::Win32::System::SystemServices::ACCESS_ALLOWED_ACE_TYPE;

    static NEXT_PIPE_ID: AtomicU64 = AtomicU64::new(1);

    fn test_endpoint(label: &str) -> String {
        format!(
            r"\\.\pipe\gate4agent-node-wire-{label}-{}-{}",
            std::process::id(),
            NEXT_PIPE_ID.fetch_add(1, Ordering::Relaxed),
        )
    }

    #[test]
    fn secure_pipe_security_descriptor_is_exact() {
        let security = CurrentUserPipeSecurity::new().expect("build secure pipe descriptor");
        let descriptor = &*security.descriptor;
        let expected_current_user =
            current_primary_token_user_sid().expect("read expected current user SID");
        assert_eq!(
            descriptor.Owner,
            security._current_user_sid.as_ptr().cast_mut().cast(),
        );
        assert_eq!(descriptor.Dacl, security._acl.as_ptr().cast_mut().cast());
        assert_ne!(descriptor.Control & SE_DACL_PROTECTED, 0);
        assert_eq!(security.attributes().bInheritHandle, 0);
        let owner_length = unsafe { GetLengthSid(descriptor.Owner) } as usize;
        let owner = unsafe {
            std::slice::from_raw_parts(descriptor.Owner.cast::<u8>(), owner_length)
        };
        assert_eq!(owner, expected_current_user);
        assert_ne!(unsafe { IsValidAcl(descriptor.Dacl) }, 0);
        let acl = unsafe { &*descriptor.Dacl };
        assert_eq!(acl.AceCount, 2);

        let mut expected_local_system =
            aligned_security_buffer(SECURITY_MAX_SID_SIZE as usize);
        let mut expected_local_system_length = SECURITY_MAX_SID_SIZE;
        assert_ne!(
            unsafe {
                CreateWellKnownSid(
                    WinLocalSystemSid,
                    ptr::null_mut(),
                    expected_local_system.as_mut_ptr().cast::<c_void>(),
                    &mut expected_local_system_length,
                )
            },
            0,
        );
        let expected_local_system = unsafe {
            std::slice::from_raw_parts(
                expected_local_system.as_ptr().cast::<u8>(),
                expected_local_system_length as usize,
            )
        };

        for (index, expected_sid) in [expected_current_user.as_slice(), expected_local_system]
            .into_iter()
            .enumerate()
        {
            let mut ace = ptr::null_mut();
            assert_ne!(unsafe { GetAce(descriptor.Dacl, index as u32, &mut ace) }, 0);
            let ace = unsafe { &*ace.cast::<ACCESS_ALLOWED_ACE>() };
            assert_eq!(ace.Header.AceType, ACCESS_ALLOWED_ACE_TYPE as u8);
            assert_eq!(ace.Header.AceFlags, NO_INHERITANCE as u8);
            assert_eq!(ace.Mask, GENERIC_ALL);
            let sid = (&ace.SidStart as *const u32).cast_mut().cast::<c_void>();
            let sid_length = unsafe { GetLengthSid(sid) } as usize;
            assert_eq!(sid_length, expected_sid.len());
            let sid = unsafe { std::slice::from_raw_parts(sid.cast::<u8>(), sid_length) };
            assert_eq!(sid, expected_sid);
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn secure_pipe_accepts_current_owner_client() {
        let endpoint = test_endpoint("owner-connect");
        let mut listener = OwnerOnlyLocalListener::bind(&endpoint)
            .await
            .expect("bind current-user pipe listener");
        let accept = tokio::spawn(async move {
            listener.accept().await.expect("accept current-user client")
        });
        let client = connect_local_stream(&endpoint)
            .await
            .expect("current user opens restricted pipe");
        let server = accept.await.expect("join pipe accept task");
        drop(client);
        drop(server);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn listener_creates_only_during_accept_and_survives_cancelled_accept() {
        let endpoint = test_endpoint("accept-cancel");
        let mut listener = OwnerOnlyLocalListener::bind(&endpoint)
            .await
            .expect("construct owner-only listener");
        let before_accept = ClientOptions::new()
            .open(&endpoint)
            .expect_err("bind must not create a pipe before caller admission");
        assert_eq!(before_accept.kind(), io::ErrorKind::NotFound);

        tokio::time::timeout(Duration::from_millis(1), listener.accept())
            .await
            .expect_err("first accept remains pending without a client");
        let client = connect_local_stream(&endpoint)
            .await
            .expect("owner connects to the retained pending instance");
        let server = listener
            .accept()
            .await
            .expect("cancelled accept retains the same pending instance");
        drop(client);
        drop(server);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn secure_pipe_preserves_first_instance_collision_policy() {
        let endpoint = test_endpoint("first-instance");
        let first = create_current_user_pipe_server(&endpoint, false)
            .expect("create initial pipe instance");
        let second = create_current_user_pipe_server(&endpoint, false)
            .expect("allow another non-first pipe instance");
        drop(second);

        let collision = create_current_user_pipe_server(&endpoint, true)
            .expect_err("reject first-instance claim after pipe already exists");
        assert_eq!(collision.raw_os_error(), Some(ERROR_ACCESS_DENIED as i32));
        drop(first);
    }
}
