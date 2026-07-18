use crate::RuntimePlatform;

/// Normalize an executable name for portable provider matching.
///
/// This function performs no filesystem access. It only applies the platform
/// naming rules to a supplied path or command string.
pub fn normalize_executable_name(
    command_or_path: &str,
    platform: RuntimePlatform,
) -> String {
    let basename = command_or_path
        .trim()
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or_default();
    if platform == RuntimePlatform::Windows {
        let lowercase = basename.to_ascii_lowercase();
        for suffix in [".exe", ".cmd", ".bat", ".ps1"] {
            if let Some(stripped) = lowercase.strip_suffix(suffix) {
                return stripped.to_owned();
            }
        }
        lowercase
    } else {
        basename.to_owned()
    }
}
