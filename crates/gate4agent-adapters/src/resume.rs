use gate4agent_types::{
    AdapterId, ProviderSessionIdentity, ProviderSessionKey, PROVIDER_SESSION_LOCATOR_MAX_BYTES,
};
use thiserror::Error;

pub const RESUME_SESSION_ID_MAX_BYTES: usize = 512;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResumePlan {
    pub program: String,
    pub args: Vec<String>,
}

/// Builds argv only for live control-plane resume contracts grounded in pinned
/// reference implementations or verified vendor CLI contracts.
///
/// `None` is a supported negative capability.
pub fn build_resume_plan(
    adapter_id: &AdapterId,
    session_id: &str,
) -> Result<Option<ResumePlan>, ResumeAdapterError> {
    let key = if adapter_id.as_str() == "antigravity" {
        ProviderSessionKey::ConversationId
    } else {
        ProviderSessionKey::SessionId
    };
    build_resume_plan_for_identity(
        adapter_id,
        &ProviderSessionIdentity {
            key,
            id: session_id.to_owned(),
            transcript_path: None,
        },
    )
}

pub fn build_resume_plan_for_identity(
    adapter_id: &AdapterId,
    identity: &ProviderSessionIdentity,
) -> Result<Option<ResumePlan>, ResumeAdapterError> {
    let Some((program, prefix, expected_key, use_transcript_path)) = (match adapter_id.as_str() {
        "claude-code" => Some((
            "claude",
            &["--resume"][..],
            ProviderSessionKey::SessionId,
            false,
        )),
        "codex" => Some((
            "codex",
            &["resume"][..],
            ProviderSessionKey::SessionId,
            false,
        )),
        "kimi" => Some((
            "kimi",
            &["-r"][..],
            ProviderSessionKey::SessionId,
            false,
        )),
        "gemini" => Some((
            "gemini",
            &["--resume"][..],
            ProviderSessionKey::SessionId,
            false,
        )),
        "antigravity" => Some((
            "agy",
            &["--conversation"][..],
            ProviderSessionKey::ConversationId,
            false,
        )),
        "opencode" => Some((
            "opencode",
            &["--session"][..],
            ProviderSessionKey::SessionId,
            false,
        )),
        "pi" => Some((
            "pi",
            &["--session"][..],
            ProviderSessionKey::SessionId,
            true,
        )),
        "mimo-code" => Some((
            "mimo",
            &["--session"][..],
            ProviderSessionKey::SessionId,
            false,
        )),
        "droid" => Some((
            "droid",
            &["--resume"][..],
            ProviderSessionKey::SessionId,
            false,
        )),
        "grok" => Some((
            "grok",
            &["--resume"][..],
            ProviderSessionKey::SessionId,
            false,
        )),
        "devin" => Some((
            "devin",
            &["--resume"][..],
            ProviderSessionKey::SessionId,
            false,
        )),
        "copilot" | "cursor" | "qwen-code" | "omp" | "amp" | "command-code" | "hermes" => {
            None
        }
        id => return Err(ResumeAdapterError::UnsupportedAdapter(id.to_owned())),
    }) else {
        return Ok(None);
    };

    if identity.key != expected_key {
        return Err(ResumeAdapterError::InvalidSessionKey);
    }

    let session_id = normalize_session_id(&identity.id)?;
    let target = if use_transcript_path {
        normalize_transcript_path(
            identity
                .transcript_path
                .as_deref()
                .ok_or(ResumeAdapterError::MissingTranscriptPath)?,
        )?
    } else {
        session_id
    };
    let mut args = prefix
        .iter()
        .map(|value| (*value).to_owned())
        .collect::<Vec<_>>();
    args.push(target);
    Ok(Some(ResumePlan {
        program: program.to_owned(),
        args,
    }))
}

fn normalize_transcript_path(value: &str) -> Result<String, ResumeAdapterError> {
    let value = value.trim();
    if value.is_empty()
        || value.len() > PROVIDER_SESSION_LOCATOR_MAX_BYTES
        || value.starts_with('-')
        || value
            .chars()
            .any(|character| character.is_control() || character == '\u{7f}')
    {
        return Err(ResumeAdapterError::InvalidTranscriptPath);
    }
    Ok(value.to_owned())
}

fn normalize_session_id(value: &str) -> Result<String, ResumeAdapterError> {
    let value = value.trim();
    if value.is_empty()
        || value.len() > RESUME_SESSION_ID_MAX_BYTES
        || value.starts_with('-')
        || value
            .chars()
            .any(|character| character.is_control() || character == '\u{7f}')
    {
        return Err(ResumeAdapterError::InvalidSessionId);
    }
    Ok(value.to_owned())
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum ResumeAdapterError {
    #[error("resume session ID is empty, unsafe, or too large")]
    InvalidSessionId,
    #[error("resume provider session key does not match the adapter contract")]
    InvalidSessionKey,
    #[error("resume adapter requires an authoritative transcript path")]
    MissingTranscriptPath,
    #[error("resume transcript path is empty, unsafe, or too large")]
    InvalidTranscriptPath,
    #[error("resume adapter is unavailable for {0}")]
    UnsupportedAdapter(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(value: &str) -> AdapterId {
        AdapterId::new(value).unwrap()
    }

    #[test]
    fn grounded_resume_argv_is_exact() {
        let cases = [
            ("claude-code", "claude", vec!["--resume", "s1"]),
            ("codex", "codex", vec!["resume", "s1"]),
            ("kimi", "kimi", vec!["-r", "s1"]),
            ("gemini", "gemini", vec!["--resume", "s1"]),
            ("antigravity", "agy", vec!["--conversation", "s1"]),
            ("opencode", "opencode", vec!["--session", "s1"]),
            ("mimo-code", "mimo", vec!["--session", "s1"]),
            ("droid", "droid", vec!["--resume", "s1"]),
            ("grok", "grok", vec!["--resume", "s1"]),
            ("devin", "devin", vec!["--resume", "s1"]),
        ];
        for (adapter, program, args) in cases {
            let plan = build_resume_plan(&id(adapter), "s1").unwrap().unwrap();
            assert_eq!(plan.program, program);
            assert_eq!(plan.args, args);
        }
    }

    #[test]
    fn pi_resume_requires_the_paired_authoritative_session_file() {
        let identity = ProviderSessionIdentity {
            key: ProviderSessionKey::SessionId,
            id: "pi-session-1".to_owned(),
            transcript_path: Some("C:/sessions/pi-session-1.jsonl".to_owned()),
        };
        let plan = build_resume_plan_for_identity(&id("pi"), &identity)
            .unwrap()
            .unwrap();
        assert_eq!(plan.program, "pi");
        assert_eq!(plan.args, ["--session", "C:/sessions/pi-session-1.jsonl"]);
        assert_eq!(
            build_resume_plan(&id("pi"), "pi-session-1"),
            Err(ResumeAdapterError::MissingTranscriptPath)
        );
    }

    #[test]
    fn provider_session_key_must_match_the_resume_contract() {
        let identity = ProviderSessionIdentity {
            key: ProviderSessionKey::SessionId,
            id: "conversation-1".to_owned(),
            transcript_path: None,
        };
        assert_eq!(
            build_resume_plan_for_identity(&id("antigravity"), &identity),
            Err(ResumeAdapterError::InvalidSessionKey)
        );
    }

    #[test]
    fn transcript_locator_is_used_only_by_pi_and_never_as_an_option() {
        let claude = ProviderSessionIdentity {
            key: ProviderSessionKey::SessionId,
            id: "claude-session-1".to_owned(),
            transcript_path: Some("C:/sessions/claude-rollout-1.jsonl".to_owned()),
        };
        let plan = build_resume_plan_for_identity(&id("claude-code"), &claude)
            .unwrap()
            .unwrap();
        assert_eq!(plan.args, ["--resume", "claude-session-1"]);

        for path in ["", " --help", "bad\npath"] {
            let pi = ProviderSessionIdentity {
                key: ProviderSessionKey::SessionId,
                id: "pi-session-1".to_owned(),
                transcript_path: Some(path.to_owned()),
            };
            assert_eq!(
                build_resume_plan_for_identity(&id("pi"), &pi),
                Err(ResumeAdapterError::InvalidTranscriptPath)
            );
        }
    }

    #[test]
    fn kimi_live_resume_uses_the_verified_short_flag() {
        let plan = build_resume_plan(&id("kimi"), "session_1")
            .unwrap()
            .unwrap();
        assert_eq!(plan.program, "kimi");
        assert_eq!(plan.args, ["-r", "session_1"]);
    }

    #[test]
    fn unsafe_session_ids_never_reach_argv() {
        for value in ["", " --help", "line\nbreak"] {
            assert_eq!(
                build_resume_plan(&id("grok"), value),
                Err(ResumeAdapterError::InvalidSessionId)
            );
        }
    }

    #[test]
    fn unsupported_and_explicit_negative_capabilities_are_distinct() {
        assert_eq!(build_resume_plan(&id("qwen-code"), "s1").unwrap(), None);
        assert!(matches!(
            build_resume_plan(&id("future-provider"), "s1"),
            Err(ResumeAdapterError::UnsupportedAdapter(_))
        ));
    }
}
