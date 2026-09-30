//! Opaque correlation ids and tool-class labels the node mints for a
//! session's provider events.
//!
//! The ids are part of the wire contract: `NodeRequest::ResolveInteraction`
//! answers an `AgentStreamChunkKindV1::InteractionPrompt::correlation_id`, and
//! a client that derives its own telemetry from `NodeEvent::Control` must mint
//! the very same id to line the two up. They live here, next to the request
//! that consumes them, so the node and every client compute them through one
//! function.
//!
//! Every id is a short prefix plus the first eight bytes of a SHA-256 over the
//! session identity and the provider's own id, so the provider's raw request or
//! tool id never reaches the wire.

use gate4agent_types::{AgentInstanceId, ControlEvent, ProviderSource, SessionGeneration};
use ring::digest::{digest, SHA256};
use std::fmt::Write as _;

fn hex_suffix_correlation(prefix: &str, material: &[u8]) -> String {
    let digest = digest(&SHA256, material);
    let mut correlation = String::with_capacity(prefix.len() + 16);
    correlation.push_str(prefix);
    for byte in &digest.as_ref()[..8] {
        // Writing to a `String` never fails; the `Result` only exists because
        // `fmt::Write` is shared with fallible sinks.
        let _ = write!(&mut correlation, "{byte:02x}");
    }
    correlation
}

fn provider_source_material(source: &ProviderSource) -> Vec<u8> {
    // `ProviderSource` is a plain serde tree of strings and enums, so its JSON
    // form cannot fail. An empty fallback keeps the id deterministic rather
    // than panicking in library code.
    serde_json::to_vec(source).unwrap_or_default()
}

/// Correlation id of one provider-side subagent.
pub fn subagent_correlation(
    instance_id: AgentInstanceId,
    generation: SessionGeneration,
    source: &ProviderSource,
    provider_agent_id: &str,
) -> String {
    let mut material = Vec::with_capacity(16 + provider_agent_id.len());
    material.extend_from_slice(&instance_id.0.to_le_bytes());
    material.extend_from_slice(&generation.0.to_le_bytes());
    material.extend_from_slice(&provider_source_material(source));
    material.extend_from_slice(provider_agent_id.as_bytes());
    hex_suffix_correlation("sub-", &material)
}

/// Correlation id of one provider tool call carried by `event`.
pub fn tool_correlation(
    event: &ControlEvent,
    source: &ProviderSource,
    provider_tool_id: &str,
) -> String {
    let mut material = Vec::new();
    material.extend_from_slice(&event.instance_id.0.to_le_bytes());
    material.extend_from_slice(&event.generation.0.to_le_bytes());
    material.extend_from_slice(&provider_source_material(source));
    material.extend_from_slice(provider_tool_id.as_bytes());
    hex_suffix_correlation("tool-", &material)
}

/// Correlation id of one interaction (approval or question) of a session.
///
/// Takes the `(instance_id, generation, interaction_id)` triple directly so the
/// reverse lookup in a request handler can replay the same digest for each
/// interaction a session's live snapshot still remembers, without a
/// `ControlEvent` to borrow the identity from.
pub fn interaction_correlation(
    instance_id: AgentInstanceId,
    generation: SessionGeneration,
    interaction_id: u64,
) -> String {
    let mut material = Vec::with_capacity(24);
    material.extend_from_slice(b"interaction");
    material.extend_from_slice(&instance_id.0.to_le_bytes());
    material.extend_from_slice(&generation.0.to_le_bytes());
    material.extend_from_slice(&interaction_id.to_le_bytes());
    hex_suffix_correlation("int-", &material)
}

/// Correlation id of the provider process a session owns.
pub fn process_correlation(instance_id: AgentInstanceId, generation: SessionGeneration) -> String {
    let mut material = Vec::with_capacity(24);
    material.extend_from_slice(b"provider-session");
    material.extend_from_slice(&instance_id.0.to_le_bytes());
    material.extend_from_slice(&generation.0.to_le_bytes());
    hex_suffix_correlation("proc-", &material)
}

/// Collapses a provider tool name into a small class label (`Read`, `Write`,
/// `Shell`, ...). Returns `None` for an empty name; sets `truncated` whenever
/// the label differs from the trimmed input, so a reader can tell a
/// generalised label from a verbatim one.
pub fn sanitize_progress_tool_label(value: &str, truncated: &mut bool) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        *truncated = true;
        return None;
    }
    let normalized = trimmed.to_ascii_lowercase();
    let class = if ["read", "view", "open", "get"].iter().any(|term| normalized.contains(term)) {
        "Read"
    } else if ["write", "create", "save"].iter().any(|term| normalized.contains(term)) {
        "Write"
    } else if ["edit", "patch", "replace"].iter().any(|term| normalized.contains(term)) {
        "Edit"
    } else if ["shell", "bash", "powershell", "terminal", "exec", "command"]
        .iter()
        .any(|term| normalized.contains(term))
    {
        "Shell"
    } else if ["search", "find", "grep", "query"].iter().any(|term| normalized.contains(term)) {
        "Search"
    } else if ["browser", "web", "http", "fetch"].iter().any(|term| normalized.contains(term)) {
        "Browse"
    } else if ["git", "commit", "diff"].iter().any(|term| normalized.contains(term)) {
        "Git"
    } else if ["ask", "question", "approval", "input"].iter().any(|term| normalized.contains(term)) {
        "Ask"
    } else if ["task", "agent", "spawn"].iter().any(|term| normalized.contains(term)) {
        "Task"
    } else {
        "Tool"
    };
    if trimmed != class {
        *truncated = true;
    }
    Some(class.to_owned())
}

/// The class label for a tool or request method, `"Tool"` when the name is empty.
pub fn tool_class_label(name: &str) -> String {
    let mut truncated = false;
    sanitize_progress_tool_label(name, &mut truncated).unwrap_or_else(|| "Tool".to_owned())
}

/// Cuts `value` to at most `max_bytes` on a character boundary. The returned
/// flag says whether a cut happened; a caller that cuts is responsible for
/// reporting it.
pub fn truncate_text(value: &str, max_bytes: usize) -> (String, bool) {
    if value.len() <= max_bytes {
        return (value.to_owned(), false);
    }
    let mut end = max_bytes;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    (value[..end].to_owned(), true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interaction_correlation_is_stable_and_identity_bound() {
        let a = interaction_correlation(AgentInstanceId(7), SessionGeneration(3), 9);
        assert_eq!(a, interaction_correlation(AgentInstanceId(7), SessionGeneration(3), 9));
        assert!(a.starts_with("int-"));
        assert_eq!(a.len(), 4 + 16);
        assert_ne!(a, interaction_correlation(AgentInstanceId(7), SessionGeneration(4), 9));
        assert_ne!(a, interaction_correlation(AgentInstanceId(8), SessionGeneration(3), 9));
        assert_ne!(a, interaction_correlation(AgentInstanceId(7), SessionGeneration(3), 10));
    }

    #[test]
    fn process_correlation_has_its_own_prefix() {
        let id = process_correlation(AgentInstanceId(1), SessionGeneration(1));
        assert!(id.starts_with("proc-"));
        assert_eq!(id.len(), 5 + 16);
    }

    #[test]
    fn tool_class_label_generalises_and_defaults() {
        assert_eq!(tool_class_label("Bash"), "Shell");
        assert_eq!(tool_class_label("  "), "Tool");
        assert_eq!(tool_class_label("zzz"), "Tool");
    }

    #[test]
    fn truncate_text_cuts_on_a_character_boundary() {
        assert_eq!(truncate_text("abc", 8), ("abc".to_owned(), false));
        assert_eq!(truncate_text("ééé", 3), ("é".to_owned(), true));
    }
}
