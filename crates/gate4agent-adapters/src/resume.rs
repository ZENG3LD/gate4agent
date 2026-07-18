use gate4agent_types::AdapterId;
use thiserror::Error;

pub const RESUME_SESSION_ID_MAX_BYTES: usize = 512;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResumePlan {
    pub program: String,
    pub args: Vec<String>,
}

/// Builds argv only for live control-plane resume contracts grounded in Orca's
/// pinned `agent-session-resume.ts` implementation.
///
/// `None` is a supported negative capability. In particular, Kimi history is
/// parseable but its separate AI Vault `--session` flow is not promoted into
/// the live hook/control-plane contract by this function.
pub fn build_resume_plan(
    adapter_id: &AdapterId,
    session_id: &str,
) -> Result<Option<ResumePlan>, ResumeAdapterError> {
    let Some((program, prefix)) = (match adapter_id.as_str() {
        "claude-code" => Some(("claude", &["--resume"][..])),
        "codex" => Some(("codex", &["resume"][..])),
        "gemini" => Some(("gemini", &["--resume"][..])),
        "opencode" => Some(("opencode", &["--session"][..])),
        "droid" => Some(("droid", &["--resume"][..])),
        "grok" => Some(("grok", &["--resume"][..])),
        "kimi" | "copilot" | "cursor" | "qwen-code" => None,
        id => return Err(ResumeAdapterError::UnsupportedAdapter(id.to_owned())),
    }) else {
        return Ok(None);
    };

    let session_id = normalize_session_id(session_id)?;
    let mut args = prefix
        .iter()
        .map(|value| (*value).to_owned())
        .collect::<Vec<_>>();
    args.push(session_id);
    Ok(Some(ResumePlan {
        program: program.to_owned(),
        args,
    }))
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
            ("gemini", "gemini", vec!["--resume", "s1"]),
            ("opencode", "opencode", vec!["--session", "s1"]),
            ("droid", "droid", vec!["--resume", "s1"]),
            ("grok", "grok", vec!["--resume", "s1"]),
        ];
        for (adapter, program, args) in cases {
            let plan = build_resume_plan(&id(adapter), "s1").unwrap().unwrap();
            assert_eq!(plan.program, program);
            assert_eq!(plan.args, args);
        }
    }

    #[test]
    fn kimi_live_resume_remains_a_negative_capability() {
        assert_eq!(build_resume_plan(&id("kimi"), "session_1").unwrap(), None);
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
}
