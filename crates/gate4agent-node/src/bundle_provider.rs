use crate::bundle_catalog::NodeBundle;
use crate::protocol::{
    DeliveryComponentKindV2, DeliveryComponentV2, DeliveryScopeV2, SessionMode,
};
use gate4agent_types::AgentId;
use std::ffi::OsString;
use std::path::Path;
use thiserror::Error;

const CLAUDE_PLUGIN_MANIFEST: &str = ".claude-plugin/plugin.json";

/// Claude Code's own installed CLI documents this as the plugin-root MCP
/// declaration file ("MCP configs go in `.mcp.json` at plugin root"), loaded
/// automatically from whatever directory `--plugin-dir` points at — the same
/// directory Gate4Agent already materializes skills and the plugin manifest
/// into. No other provider wired today (Kimi's `--skills-dir`, Codex's
/// isolated `CODEX_HOME` profile) exposes an equivalent session-scoped root
/// for a bundle-shipped MCP declaration to land in.
const CLAUDE_MCP_DECLARATION_PATH: &str = ".mcp.json";

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
    #[error("MCP declarations are only supported at the Claude plugin root's .mcp.json")]
    McpDeclarationUnsupported,
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
    let layout = resolve_layout(
        provider,
        mode,
        bundle
            .files()
            .iter()
            .any(|file| file.path() == CLAUDE_PLUGIN_MANIFEST),
    )?;
    if let Some(manifest) = bundle.delivery_manifest() {
        for component in &manifest.components {
            validate_delivery_component(layout, component)?;
        }
        bundle
            .validate_skill_bundle_contract()
            .map_err(|_| BundleProviderError::UnsupportedBinding)?;
    }
    Ok(layout)
}

fn validate_delivery_component(
    layout: BundleProviderLayout,
    component: &DeliveryComponentV2,
) -> Result<(), BundleProviderError> {
    if component.scope != DeliveryScopeV2::Session {
        return Err(BundleProviderError::UnsupportedBinding);
    }
    match component.kind {
        DeliveryComponentKindV2::Skill => {
            if !component.relative_path.as_str().starts_with("skills/") {
                return Err(BundleProviderError::UnsupportedBinding);
            }
        }
        DeliveryComponentKindV2::PluginManifest => {
            if !matches!(
                component.relative_path.as_str(),
                "plugin.json" | CLAUDE_PLUGIN_MANIFEST
            ) {
                return Err(BundleProviderError::UnsupportedBinding);
            }
        }
        DeliveryComponentKindV2::McpDeclaration => {
            if layout != BundleProviderLayout::Claude
                || component.relative_path.as_str() != CLAUDE_MCP_DECLARATION_PATH
            {
                return Err(BundleProviderError::McpDeclarationUnsupported);
            }
        }
        DeliveryComponentKindV2::Prompt
        | DeliveryComponentKindV2::Instructions
        | DeliveryComponentKindV2::AgentDefinition
        | DeliveryComponentKindV2::Command
        | DeliveryComponentKindV2::File
        | DeliveryComponentKindV2::Template => {
            return Err(BundleProviderError::UnsupportedBinding);
        }
    }
    Ok(())
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
    use crate::protocol::{
        DeliveryBlobDigestV1, DeliveryBlobReceiptV1, DeliveryRelativePathV2,
    };
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

    fn mcp_component(scope: DeliveryScopeV2, path: &str) -> DeliveryComponentV2 {
        DeliveryComponentV2 {
            kind: DeliveryComponentKindV2::McpDeclaration,
            scope,
            relative_path: DeliveryRelativePathV2::new(path).unwrap(),
            blob: DeliveryBlobReceiptV1::new(
                DeliveryBlobDigestV1::new(format!("sha256:{}", "a".repeat(64))).unwrap(),
                0,
            )
            .unwrap(),
        }
    }

    #[test]
    fn mcp_declaration_lands_only_at_the_grounded_claude_plugin_root_path() {
        let mcp = mcp_component(DeliveryScopeV2::Session, ".mcp.json");
        assert!(validate_delivery_component(BundleProviderLayout::Claude, &mcp).is_ok());
    }

    #[test]
    fn mcp_declaration_fails_closed_for_ungrounded_kimi_and_codex_layouts() {
        let mcp = mcp_component(DeliveryScopeV2::Session, ".mcp.json");
        for layout in [BundleProviderLayout::Kimi, BundleProviderLayout::Codex] {
            assert_eq!(
                validate_delivery_component(layout, &mcp),
                Err(BundleProviderError::McpDeclarationUnsupported),
            );
        }
    }

    #[test]
    fn mcp_declaration_fails_closed_off_the_exact_claude_path() {
        let wrong_path = mcp_component(DeliveryScopeV2::Session, "mcp.json");
        assert_eq!(
            validate_delivery_component(BundleProviderLayout::Claude, &wrong_path),
            Err(BundleProviderError::McpDeclarationUnsupported),
        );
        let nested = mcp_component(DeliveryScopeV2::Session, "config/.mcp.json");
        assert_eq!(
            validate_delivery_component(BundleProviderLayout::Claude, &nested),
            Err(BundleProviderError::McpDeclarationUnsupported),
        );
    }

    #[test]
    fn mcp_declaration_still_honors_the_session_scope_floor() {
        let workspace_scoped = mcp_component(DeliveryScopeV2::Workspace, ".mcp.json");
        assert_eq!(
            validate_delivery_component(BundleProviderLayout::Claude, &workspace_scoped),
            Err(BundleProviderError::UnsupportedBinding),
        );
    }
}
