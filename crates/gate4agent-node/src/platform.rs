use crate::protocol::{
    ArchitectureId, HostDescriptor, LocalTransportKind, OperatingSystemId, PathEncoding,
    PathSemantics, PathStyle,
};
use std::path::{Path, PathBuf};

#[cfg(windows)]
pub const DEFAULT_NODE_ENDPOINT: &str = r"\\.\pipe\gate4agent-node";

pub(crate) fn default_node_endpoint() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        return Some(PathBuf::from(DEFAULT_NODE_ENDPOINT));
    }
    #[cfg(unix)]
    {
        if let Some(root) = std::env::var_os("XDG_RUNTIME_DIR")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
        {
            return Some(root.join("gate4agent-node.sock"));
        }
        secure_fallback_runtime_dir().map(|root| root.join("n.sock"))
    }
}

pub(crate) fn default_state_path(node_id: &str) -> Option<PathBuf> {
    #[cfg(windows)]
    {
        return std::env::var_os("LOCALAPPDATA")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .map(|root| {
                root.join("Gate4Agent")
                    .join("nodes")
                    .join(node_id)
                    .join("state-v1.json")
            });
    }
    #[cfg(unix)]
    {
        #[cfg(target_os = "macos")]
        let root = std::env::var_os("HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .map(|home| home.join("Library").join("Application Support"))?;
        #[cfg(not(target_os = "macos"))]
        let root = std::env::var_os("XDG_STATE_HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .or_else(|| {
                std::env::var_os("HOME")
                    .filter(|value| !value.is_empty())
                    .map(PathBuf::from)
                    .filter(|path| path.is_absolute())
                    .map(|home| home.join(".local").join("state"))
            })?;
        Some(
            root.join("gate4agent")
                .join("nodes")
                .join(node_id)
                .join("state-v1.json"),
        )
    }
}

#[cfg(unix)]
fn secure_fallback_runtime_dir() -> Option<PathBuf> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let root = std::env::temp_dir().join(format!("g4a-{}", unsafe { libc::geteuid() }));
    if !root.is_absolute() || std::fs::create_dir_all(&root).is_err() {
        return None;
    }
    let metadata = std::fs::symlink_metadata(&root).ok()?;
    if !metadata.file_type().is_dir() || metadata.uid() != unsafe { libc::geteuid() } {
        return None;
    }
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).ok()?;
    let metadata = std::fs::symlink_metadata(&root).ok()?;
    if metadata.permissions().mode() & 0o7777 != 0o700 {
        return None;
    }
    Some(root)
}

pub(crate) fn validate_endpoint(endpoint: &str) -> bool {
    #[cfg(windows)]
    {
        return endpoint.starts_with(r"\\.\pipe\")
            && endpoint.len() > r"\\.\pipe\".len()
            && endpoint.len() <= 1_024;
    }
    #[cfg(unix)]
    {
        let path = Path::new(endpoint);
        return !endpoint.is_empty()
            && endpoint.len() <= 1_024
            && !endpoint.chars().any(char::is_control)
            && path.is_absolute()
            && path.file_name().is_some();
    }
}

pub(crate) fn root_identity(value: &str) -> String {
    #[cfg(windows)]
    {
        return value.to_lowercase();
    }
    #[cfg(unix)]
    {
        value.to_owned()
    }
}

pub(crate) fn roots_equal(left: &str, right: &str) -> bool {
    #[cfg(windows)]
    {
        return left.eq_ignore_ascii_case(right);
    }
    #[cfg(unix)]
    {
        left == right
    }
}

pub(crate) fn normalize_canonical_root(value: String) -> String {
    #[cfg(windows)]
    {
        if let Some(rest) = value.strip_prefix(r"\\?\UNC\") {
            return format!(r"\\{rest}");
        }
        if let Some(rest) = value.strip_prefix(r"\\?\") {
            return rest.to_owned();
        }
    }
    value
}

pub(crate) fn host_descriptor() -> Result<HostDescriptor, String> {
    Ok(HostDescriptor {
        operating_system: OperatingSystemId::new(std::env::consts::OS)
            .map_err(|error| error.to_string())?,
        architecture: ArchitectureId::new(std::env::consts::ARCH)
            .map_err(|error| error.to_string())?,
    })
}

pub(crate) fn path_semantics() -> PathSemantics {
    #[cfg(windows)]
    {
        return PathSemantics {
            style: PathStyle::Windows,
            encoding: PathEncoding::Utf8,
        };
    }
    #[cfg(unix)]
    {
        PathSemantics {
            style: PathStyle::Posix,
            encoding: PathEncoding::Utf8,
        }
    }
}

pub(crate) fn local_transport() -> LocalTransportKind {
    #[cfg(windows)]
    {
        return LocalTransportKind::WindowsNamedPipe;
    }
    #[cfg(unix)]
    {
        LocalTransportKind::UnixDomainSocket
    }
}

pub(crate) fn workspace_root_supported(value: &str) -> bool {
    #[cfg(windows)]
    if value.starts_with(r"\\") {
        return false;
    }
    !value.is_empty()
        && !value.chars().any(char::is_control)
        && Path::new(value).is_absolute()
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn unix_platform_contract_is_native_and_case_sensitive() {
        let endpoint = default_node_endpoint().expect("Unix runtime endpoint");
        assert!(endpoint.is_absolute());
        assert!(validate_endpoint(endpoint.to_str().expect("UTF-8 endpoint")));
        assert!(!roots_equal("/tmp/Repo", "/tmp/repo"));
        assert_eq!(path_semantics().style, PathStyle::Posix);
        assert_eq!(path_semantics().encoding, PathEncoding::Utf8);
        assert_eq!(local_transport(), LocalTransportKind::UnixDomainSocket);
        let host = host_descriptor().unwrap();
        assert_eq!(host.operating_system.as_str(), std::env::consts::OS);
        assert_eq!(host.architecture.as_str(), std::env::consts::ARCH);
    }
}
