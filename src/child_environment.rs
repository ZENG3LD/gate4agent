//! Minimal inherited environment for native provider children.

use std::ffi::{OsStr, OsString};

#[cfg(windows)]
const PLATFORM_CHILD_ENV_ALLOWLIST: &[&str] = &[
    "SystemDrive",
    "SystemRoot",
    "WINDIR",
    "ComSpec",
    "PATHEXT",
    "PATH",
    "HOME",
    "USERPROFILE",
    "APPDATA",
    "LOCALAPPDATA",
    "TEMP",
    "TMP",
    "ProgramFiles",
    "ProgramFiles(x86)",
    "ProgramW6432",
    "CommonProgramFiles",
    "CommonProgramFiles(x86)",
    "CommonProgramW6432",
    "PSModulePath",
];

#[cfg(not(windows))]
const PLATFORM_CHILD_ENV_ALLOWLIST: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "TMPDIR",
    "LANG",
    "XDG_CONFIG_HOME",
    "XDG_CACHE_HOME",
    "XDG_DATA_HOME",
    "XDG_RUNTIME_DIR",
];

pub(crate) fn is_platform_child_environment_key(key: &OsStr) -> bool {
    let Some(key) = key.to_str() else {
        return false;
    };
    #[cfg(windows)]
    {
        PLATFORM_CHILD_ENV_ALLOWLIST
            .iter()
            .any(|allowed| key.eq_ignore_ascii_case(allowed))
    }
    #[cfg(not(windows))]
    {
        PLATFORM_CHILD_ENV_ALLOWLIST.contains(&key) || key.starts_with("LC_")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platform_allowlist_preserves_the_proven_pty_baseline() {
        #[cfg(windows)]
        assert_eq!(
            PLATFORM_CHILD_ENV_ALLOWLIST,
            &[
                "SystemDrive",
                "SystemRoot",
                "WINDIR",
                "ComSpec",
                "PATHEXT",
                "PATH",
                "HOME",
                "USERPROFILE",
                "APPDATA",
                "LOCALAPPDATA",
                "TEMP",
                "TMP",
                "ProgramFiles",
                "ProgramFiles(x86)",
                "ProgramW6432",
                "CommonProgramFiles",
                "CommonProgramFiles(x86)",
                "CommonProgramW6432",
                "PSModulePath",
            ]
        );
        #[cfg(not(windows))]
        {
            assert_eq!(
                PLATFORM_CHILD_ENV_ALLOWLIST,
                &[
                    "PATH",
                    "HOME",
                    "USER",
                    "LOGNAME",
                    "SHELL",
                    "TMPDIR",
                    "LANG",
                    "XDG_CONFIG_HOME",
                    "XDG_CACHE_HOME",
                    "XDG_DATA_HOME",
                    "XDG_RUNTIME_DIR",
                ]
            );
            assert!(is_platform_child_environment_key(OsStr::new("LC_MESSAGES")));
        }
    }
}

/// Returns the machine-local variables required to locate executables, user
/// configuration, and temporary storage. Provider credentials, proxy state,
/// SSH agents, and unrelated daemon variables are excluded.
pub fn platform_minimal_child_environment() -> Vec<(OsString, OsString)> {
    platform_minimal_child_environment_from(std::env::vars_os())
}

pub(crate) fn platform_minimal_child_environment_from<I, K, V>(
    inherited: I,
) -> Vec<(OsString, OsString)>
where
    I: IntoIterator<Item = (K, V)>,
    K: AsRef<OsStr>,
    V: AsRef<OsStr>,
{
    inherited
        .into_iter()
        .filter_map(|(key, value)| {
            is_platform_child_environment_key(key.as_ref()).then(|| {
                (
                    key.as_ref().to_os_string(),
                    value.as_ref().to_os_string(),
                )
            })
        })
        .collect()
}
