use crate::bundle_catalog::NodeBundle;
use crate::protocol::SessionMode;
use gate4agent_types::AgentId;
use std::ffi::OsString;
use std::path::Path;
use thiserror::Error;

const CLAUDE_PLUGIN_MANIFEST: &str = ".claude-plugin/plugin.json";

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum BundleProviderError {
    #[error("the bundle is not supported by this provider and mode")]
    UnsupportedBinding,
    #[error("the Claude bundle manifest is missing")]
    MissingClaudeManifest,
    #[error("the materialized bundle root is not absolute")]
    BundleRootNotAbsolute,
}

pub(crate) fn validate_bundle_binding(
    provider: &AgentId,
    mode: SessionMode,
    bundle: &NodeBundle,
) -> Result<(), BundleProviderError> {
    validate_binding(
        provider,
        mode,
        bundle
            .files()
            .iter()
            .any(|file| file.path() == CLAUDE_PLUGIN_MANIFEST),
    )
}

pub(crate) fn bundle_launch_arguments(
    provider: &AgentId,
    mode: SessionMode,
    bundle: &NodeBundle,
    bundle_root: &Path,
) -> Result<Vec<OsString>, BundleProviderError> {
    launch_arguments(
        provider,
        mode,
        bundle
            .files()
            .iter()
            .any(|file| file.path() == CLAUDE_PLUGIN_MANIFEST),
        bundle_root,
    )
}

fn validate_binding(
    provider: &AgentId,
    mode: SessionMode,
    has_claude_manifest: bool,
) -> Result<(), BundleProviderError> {
    if mode != SessionMode::Pty {
        return Err(BundleProviderError::UnsupportedBinding);
    }
    match provider.as_str() {
        "claude" if has_claude_manifest => Ok(()),
        "claude" => Err(BundleProviderError::MissingClaudeManifest),
        "kimi" => Ok(()),
        _ => Err(BundleProviderError::UnsupportedBinding),
    }
}

fn launch_arguments(
    provider: &AgentId,
    mode: SessionMode,
    has_claude_manifest: bool,
    bundle_root: &Path,
) -> Result<Vec<OsString>, BundleProviderError> {
    if !bundle_root.is_absolute() {
        return Err(BundleProviderError::BundleRootNotAbsolute);
    }
    validate_binding(provider, mode, has_claude_manifest)?;
    match provider.as_str() {
        "claude" => Ok(vec![
            OsString::from("--plugin-dir"),
            bundle_root.as_os_str().to_owned(),
        ]),
        "kimi" => Ok(vec![
            OsString::from("--skills-dir"),
            bundle_root.join("skills").into_os_string(),
        ]),
        _ => Err(BundleProviderError::UnsupportedBinding),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn agent(value: &str) -> AgentId {
        AgentId::new(value).unwrap()
    }

    fn absolute_bundle_root() -> PathBuf {
        if cfg!(windows) {
            PathBuf::from(r"C:\gate4agent\materializations\bundle")
        } else {
            PathBuf::from("/gate4agent/materializations/bundle")
        }
    }

    #[test]
    fn claude_and_kimi_receive_exact_bundle_arguments() {
        let root = absolute_bundle_root();
        assert_eq!(
            launch_arguments(&agent("claude"), SessionMode::Pty, true, &root).unwrap(),
            vec![OsString::from("--plugin-dir"), root.as_os_str().to_owned()],
        );
        assert_eq!(
            launch_arguments(&agent("kimi"), SessionMode::Pty, false, &root).unwrap(),
            vec![
                OsString::from("--skills-dir"),
                root.join("skills").into_os_string(),
            ],
        );
    }

    #[test]
    fn claude_requires_the_exact_optional_manifest() {
        assert_eq!(
            validate_binding(&agent("claude"), SessionMode::Pty, false),
            Err(BundleProviderError::MissingClaudeManifest),
        );
    }

    #[test]
    fn inline_and_other_pty_providers_are_rejected() {
        for provider in ["claude", "kimi", "codex", "qwen-code", "grok", "other"] {
            assert_eq!(
                validate_binding(&agent(provider), SessionMode::Inline, true),
                Err(BundleProviderError::UnsupportedBinding),
            );
        }
        for provider in ["codex", "qwen-code", "grok", "other"] {
            assert_eq!(
                validate_binding(&agent(provider), SessionMode::Pty, true),
                Err(BundleProviderError::UnsupportedBinding),
            );
        }
    }

    #[test]
    fn launch_arguments_require_an_absolute_materialized_root() {
        assert_eq!(
            launch_arguments(
                &agent("kimi"),
                SessionMode::Pty,
                false,
                Path::new("relative/bundle"),
            ),
            Err(BundleProviderError::BundleRootNotAbsolute),
        );
    }
}
