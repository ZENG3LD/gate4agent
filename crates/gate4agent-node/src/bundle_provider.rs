use crate::bundle_catalog::NodeBundle;
use crate::protocol::SessionMode;
use gate4agent_types::AgentId;
use std::ffi::OsString;
use std::path::Path;
use thiserror::Error;

const CLAUDE_PLUGIN_MANIFEST: &str = ".claude-plugin/plugin.json";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BundleProviderLayout {
    Claude,
    Kimi,
    Codex,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum BundleProviderError {
    #[error("the bundle is not supported by this provider and mode")]
    UnsupportedBinding,
    #[error("the Claude bundle manifest is missing")]
    MissingClaudeManifest,
    #[error("the materialized bundle root is not absolute")]
    BundleRootNotAbsolute,
    #[error("the materialized provider home is not absolute")]
    ProviderHomeNotAbsolute,
}

pub(crate) fn validate_bundle_binding(
    provider: &AgentId,
    mode: SessionMode,
    bundle: &NodeBundle,
) -> Result<BundleProviderLayout, BundleProviderError> {
    resolve_layout(
        provider,
        mode,
        bundle
            .files()
            .iter()
            .any(|file| file.path() == CLAUDE_PLUGIN_MANIFEST),
    )
}

pub(crate) fn bundle_launch_arguments(
    layout: BundleProviderLayout,
    bundle_root: &Path,
    provider_home: &Path,
) -> Result<Vec<OsString>, BundleProviderError> {
    launch_arguments(layout, bundle_root, provider_home)
}

fn resolve_layout(
    provider: &AgentId,
    mode: SessionMode,
    has_claude_manifest: bool,
) -> Result<BundleProviderLayout, BundleProviderError> {
    if mode != SessionMode::Pty {
        return Err(BundleProviderError::UnsupportedBinding);
    }
    match provider.as_str() {
        "claude" if has_claude_manifest => Ok(BundleProviderLayout::Claude),
        "claude" => Err(BundleProviderError::MissingClaudeManifest),
        "kimi" => Ok(BundleProviderLayout::Kimi),
        "codex" => Ok(BundleProviderLayout::Codex),
        _ => Err(BundleProviderError::UnsupportedBinding),
    }
}

fn launch_arguments(
    layout: BundleProviderLayout,
    bundle_root: &Path,
    provider_home: &Path,
) -> Result<Vec<OsString>, BundleProviderError> {
    if !bundle_root.is_absolute() {
        return Err(BundleProviderError::BundleRootNotAbsolute);
    }
    if !provider_home.is_absolute() {
        return Err(BundleProviderError::ProviderHomeNotAbsolute);
    }
    match layout {
        BundleProviderLayout::Claude => Ok(vec![
            OsString::from("--plugin-dir"),
            bundle_root.as_os_str().to_owned(),
        ]),
        BundleProviderLayout::Kimi => Ok(vec![
            OsString::from("--skills-dir"),
            bundle_root.join("skills").into_os_string(),
        ]),
        BundleProviderLayout::Codex => Ok(Vec::new()),
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

    fn absolute_provider_home() -> PathBuf {
        if cfg!(windows) {
            PathBuf::from(r"C:\gate4agent\materializations\home")
        } else {
            PathBuf::from("/gate4agent/materializations/home")
        }
    }

    #[test]
    fn claude_and_kimi_receive_exact_bundle_arguments() {
        let root = absolute_bundle_root();
        let home = absolute_provider_home();
        assert_eq!(
            launch_arguments(BundleProviderLayout::Claude, &root, &home).unwrap(),
            vec![OsString::from("--plugin-dir"), root.as_os_str().to_owned()],
        );
        assert_eq!(
            launch_arguments(BundleProviderLayout::Kimi, &root, &home).unwrap(),
            vec![
                OsString::from("--skills-dir"),
                root.join("skills").into_os_string(),
            ],
        );
        assert!(launch_arguments(BundleProviderLayout::Codex, &root, &home)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn claude_requires_the_exact_optional_manifest() {
        assert_eq!(
            resolve_layout(&agent("claude"), SessionMode::Pty, false),
            Err(BundleProviderError::MissingClaudeManifest),
        );
    }

    #[test]
    fn codex_pty_resolves_without_claude_manifest() {
        assert_eq!(
            resolve_layout(&agent("codex"), SessionMode::Pty, false),
            Ok(BundleProviderLayout::Codex),
        );
    }

    #[test]
    fn inline_and_other_pty_providers_are_rejected() {
        for provider in ["claude", "kimi", "codex", "qwen-code", "grok", "other"] {
            assert_eq!(
                resolve_layout(&agent(provider), SessionMode::Inline, true),
                Err(BundleProviderError::UnsupportedBinding),
            );
        }
        for provider in ["qwen-code", "grok", "other"] {
            assert_eq!(
                resolve_layout(&agent(provider), SessionMode::Pty, true),
                Err(BundleProviderError::UnsupportedBinding),
            );
        }
    }

    #[test]
    fn launch_arguments_require_an_absolute_materialized_root() {
        assert_eq!(
            launch_arguments(
                BundleProviderLayout::Kimi,
                Path::new("relative/bundle"),
                &absolute_provider_home(),
            ),
            Err(BundleProviderError::BundleRootNotAbsolute),
        );
    }
}
