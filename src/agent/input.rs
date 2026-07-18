use super::AgentId;
use thiserror::Error;

pub const TERMINAL_INPUT_CHUNK_MAX_BYTES: usize = 16 * 1024;
pub const TERMINAL_INPUT_MAX_BYTES: usize = 16 * 1024 * 1024;
pub const TERMINAL_SUBMIT_DELAY_MS: u64 = 500;
pub const TERMINAL_WRITE_DELAY_MAX_MS: u64 = 5_000;
pub const BRACKETED_PASTE_START: &[u8] = b"\x1b[200~";
pub const BRACKETED_PASTE_END: &[u8] = b"\x1b[201~";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PromptFraming {
    BracketedPaste,
    Literal,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PromptPayload {
    pub text: String,
    pub framing: PromptFraming,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentCommand {
    pub agent_id: AgentId,
    pub name: String,
    pub arguments: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ShellCommand {
    pub text: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TerminalText {
    pub text: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TerminalControl {
    Interrupt,
    EndOfFile,
    Enter,
    LineFeed,
    Escape,
    Backspace,
    Tab,
}

impl TerminalControl {
    pub fn bytes(self) -> &'static [u8] {
        match self {
            Self::Interrupt => b"\x03",
            Self::EndOfFile => b"\x04",
            Self::Enter => b"\r",
            Self::LineFeed => b"\n",
            Self::Escape => b"\x1b",
            Self::Backspace => b"\x08",
            Self::Tab => b"\t",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InputAction {
    InsertDraft(PromptPayload),
    SubmitPrompt(PromptPayload),
    AgentCommand(AgentCommand),
    ShellCommand(ShellCommand),
    TerminalText(TerminalText),
    TerminalControl(TerminalControl),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PreparedWriteKind {
    Framing,
    Data,
    Submit,
    Control,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreparedWrite {
    pub kind: PreparedWriteKind,
    pub bytes: Vec<u8>,
    /// Delay after the preceding write and before this write.
    pub delay_before_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreparedInput {
    kind: PreparedInputKind,
    writes: Vec<PreparedWrite>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PreparedInputKind {
    InsertDraft,
    SubmitPrompt,
    AgentCommand,
    TerminalText,
    TerminalControl,
}

impl PreparedInput {
    /// The operation semantic used to validate its readiness permit.
    pub fn kind(&self) -> PreparedInputKind {
        self.kind
    }

    /// Inspect the bounded write sequence without making it mutable or forgeable.
    pub fn writes(&self) -> &[PreparedWrite] {
        &self.writes
    }

    pub(crate) fn into_writes(self) -> Vec<PreparedWrite> {
        self.writes
    }
}

/// Converts an input action to bounded writes without touching a PTY.
///
/// Agent and shell commands require a dispatcher that can prove the current
/// foreground target. Prompt paste and submit are emitted as separate writes.
pub fn prepare_input(action: InputAction) -> Result<PreparedInput, InputPrepareError> {
    prepare_input_with_limits(
        action,
        TERMINAL_INPUT_CHUNK_MAX_BYTES,
        TERMINAL_INPUT_MAX_BYTES,
    )
}

pub fn prepare_input_with_limits(
    action: InputAction,
    max_chunk_bytes: usize,
    max_total_bytes: usize,
) -> Result<PreparedInput, InputPrepareError> {
    if max_chunk_bytes == 0 {
        return Err(InputPrepareError::ZeroChunkLimit);
    }
    match action {
        InputAction::InsertDraft(payload) => {
            prepare_prompt(payload, false, max_chunk_bytes, max_total_bytes)
        }
        InputAction::SubmitPrompt(payload) => {
            prepare_prompt(payload, true, max_chunk_bytes, max_total_bytes)
        }
        InputAction::TerminalText(text) => {
            if let Some((index, character)) = text
                .text
                .char_indices()
                .find(|(_, character)| character.is_control())
            {
                return Err(InputPrepareError::ControlCharacterInTerminalText {
                    index,
                    codepoint: character as u32,
                });
            }
            bounded_chunks(&text.text, max_chunk_bytes, max_total_bytes).map(|chunks| {
                PreparedInput {
                    kind: PreparedInputKind::TerminalText,
                    writes: chunks
                        .into_iter()
                        .map(|bytes| PreparedWrite {
                            kind: PreparedWriteKind::Data,
                            bytes,
                            delay_before_ms: 0,
                        })
                        .collect(),
                }
            })
        }
        InputAction::TerminalControl(control) => Ok(PreparedInput {
            kind: PreparedInputKind::TerminalControl,
            writes: vec![PreparedWrite {
                kind: PreparedWriteKind::Control,
                bytes: control.bytes().to_vec(),
                delay_before_ms: 0,
            }],
        }),
        InputAction::AgentCommand(_) => Err(InputPrepareError::AgentDispatcherRequired),
        InputAction::ShellCommand(_) => Err(InputPrepareError::ShellDispatcherRequired),
    }
}

/// Prepare a provider-native slash command after a dispatcher has selected the
/// foreground agent. Arguments are TUI text segments, not shell arguments.
pub fn prepare_agent_command(
    command: AgentCommand,
    target_agent: &AgentId,
) -> Result<PreparedInput, InputPrepareError> {
    if &command.agent_id != target_agent {
        return Err(InputPrepareError::AgentTargetMismatch {
            command_agent: command.agent_id,
            target_agent: target_agent.clone(),
        });
    }
    if command.name.is_empty()
        || command.name.len() > 128
        || !command.name.chars().all(|character| {
            character.is_ascii_lowercase()
                || character.is_ascii_digit()
                || matches!(character, '-' | '_')
        })
    {
        return Err(InputPrepareError::InvalidAgentCommandName(command.name));
    }

    let mut text = format!("/{}", command.name);
    for (argument_index, argument) in command.arguments.iter().enumerate() {
        if argument.is_empty()
            || argument
                .chars()
                .any(|character| character.is_control() || character == '\u{1b}')
        {
            return Err(InputPrepareError::InvalidAgentCommandArgument { argument_index });
        }
        text.push(' ');
        text.push_str(argument);
    }

    let chunks = bounded_chunks(
        &text,
        TERMINAL_INPUT_CHUNK_MAX_BYTES,
        TERMINAL_INPUT_MAX_BYTES,
    )?;
    let mut writes: Vec<_> = chunks
        .into_iter()
        .map(|bytes| PreparedWrite {
            kind: PreparedWriteKind::Data,
            bytes,
            delay_before_ms: 0,
        })
        .collect();
    writes.push(PreparedWrite {
        kind: PreparedWriteKind::Submit,
        bytes: TerminalControl::Enter.bytes().to_vec(),
        delay_before_ms: TERMINAL_SUBMIT_DELAY_MS,
    });
    Ok(PreparedInput {
        kind: PreparedInputKind::AgentCommand,
        writes,
    })
}

fn prepare_prompt(
    payload: PromptPayload,
    submit: bool,
    max_chunk_bytes: usize,
    max_total_bytes: usize,
) -> Result<PreparedInput, InputPrepareError> {
    if payload.text.len() > max_total_bytes {
        return Err(InputPrepareError::InputTooLarge {
            bytes: payload.text.len(),
            max: max_total_bytes,
        });
    }
    let sanitized = sanitize_prompt_text(&payload.text);
    let chunks = bounded_chunks(&sanitized, max_chunk_bytes, max_total_bytes)?;
    let mut writes = Vec::with_capacity(chunks.len() + 3);

    if payload.framing == PromptFraming::BracketedPaste {
        writes.push(PreparedWrite {
            kind: PreparedWriteKind::Framing,
            bytes: BRACKETED_PASTE_START.to_vec(),
            delay_before_ms: 0,
        });
    }
    writes.extend(chunks.into_iter().map(|bytes| PreparedWrite {
        kind: PreparedWriteKind::Data,
        bytes,
        delay_before_ms: 0,
    }));
    if payload.framing == PromptFraming::BracketedPaste {
        writes.push(PreparedWrite {
            kind: PreparedWriteKind::Framing,
            bytes: BRACKETED_PASTE_END.to_vec(),
            delay_before_ms: 0,
        });
    }
    if submit {
        writes.push(PreparedWrite {
            kind: PreparedWriteKind::Submit,
            bytes: TerminalControl::Enter.bytes().to_vec(),
            delay_before_ms: TERMINAL_SUBMIT_DELAY_MS,
        });
    }

    Ok(PreparedInput {
        kind: if submit {
            PreparedInputKind::SubmitPrompt
        } else {
            PreparedInputKind::InsertDraft
        },
        writes,
    })
}

/// Neutralizes terminal control sequences while preserving prompt newlines and tabs.
pub fn sanitize_prompt_text(text: &str) -> String {
    let mut sanitized = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '\n' | '\r' | '\t' => sanitized.push(character),
            '\u{1b}' => sanitized.push_str("<ESC>"),
            '\u{009b}' => sanitized.push_str("<CSI>"),
            '\u{009d}' => sanitized.push_str("<OSC>"),
            value if value.is_control() => {
                use std::fmt::Write;
                let _ = write!(sanitized, "<U+{:04X}>", value as u32);
            }
            value => sanitized.push(value),
        }
    }
    sanitized
}

fn bounded_chunks(
    text: &str,
    max_chunk_bytes: usize,
    max_total_bytes: usize,
) -> Result<Vec<Vec<u8>>, InputPrepareError> {
    if text.len() > max_total_bytes {
        return Err(InputPrepareError::InputTooLarge {
            bytes: text.len(),
            max: max_total_bytes,
        });
    }
    if text.is_empty() {
        return Ok(Vec::new());
    }

    let mut chunks = Vec::new();
    let mut start = 0;
    let mut current_bytes = 0;
    for (index, character) in text.char_indices() {
        let character_bytes = character.len_utf8();
        if character_bytes > max_chunk_bytes {
            return Err(InputPrepareError::ChunkLimitTooSmall {
                bytes: character_bytes,
                max: max_chunk_bytes,
            });
        }
        if current_bytes + character_bytes > max_chunk_bytes {
            chunks.push(text.as_bytes()[start..index].to_vec());
            start = index;
            current_bytes = 0;
        }
        current_bytes += character_bytes;
    }
    chunks.push(text.as_bytes()[start..].to_vec());
    Ok(chunks)
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum InputPrepareError {
    #[error("terminal input chunk limit cannot be zero")]
    ZeroChunkLimit,
    #[error("input is {bytes} bytes; the configured limit is {max} bytes")]
    InputTooLarge { bytes: usize, max: usize },
    #[error("a {bytes}-byte UTF-8 code point does not fit the {max}-byte chunk limit")]
    ChunkLimitTooSmall { bytes: usize, max: usize },
    #[error(
        "terminal text contains control U+{codepoint:04X} at byte {index}; use TerminalControl"
    )]
    ControlCharacterInTerminalText { index: usize, codepoint: u32 },
    #[error("agent commands require a provider-specific capability dispatcher")]
    AgentDispatcherRequired,
    #[error("agent command targets '{command_agent}', but foreground dispatcher selected '{target_agent}'")]
    AgentTargetMismatch {
        command_agent: AgentId,
        target_agent: AgentId,
    },
    #[error("invalid agent command name '{0}'")]
    InvalidAgentCommandName(String),
    #[error("agent command argument {argument_index} is empty or contains terminal controls")]
    InvalidAgentCommandArgument { argument_index: usize },
    #[error("shell commands require a confirmed foreground shell dispatcher")]
    ShellDispatcherRequired,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload(text: &str) -> PromptPayload {
        PromptPayload {
            text: text.to_owned(),
            framing: PromptFraming::BracketedPaste,
        }
    }

    #[test]
    fn draft_and_submit_have_distinct_write_sequences() {
        let draft = prepare_input(InputAction::InsertDraft(payload("hello"))).unwrap();
        let submit = prepare_input(InputAction::SubmitPrompt(payload("hello"))).unwrap();
        assert_eq!(draft.writes.last().unwrap().bytes, BRACKETED_PASTE_END);
        assert_eq!(
            submit.writes.last().unwrap().kind,
            PreparedWriteKind::Submit
        );
        assert_eq!(submit.writes.last().unwrap().bytes, b"\r");
        assert_eq!(
            submit.writes.last().unwrap().delay_before_ms,
            TERMINAL_SUBMIT_DELAY_MS
        );
    }

    #[test]
    fn prompt_controls_are_inert_inside_bracketed_paste() {
        let prepared = prepare_input(InputAction::InsertDraft(payload("a\x1b[31m\0b"))).unwrap();
        let joined: Vec<u8> = prepared
            .writes
            .iter()
            .flat_map(|write| write.bytes.iter().copied())
            .collect();
        assert_eq!(
            String::from_utf8(joined).unwrap(),
            "\x1b[200~a<ESC>[31m<U+0000>b\x1b[201~"
        );
    }

    #[test]
    fn chunks_never_split_unicode() {
        let prepared = prepare_input_with_limits(
            InputAction::TerminalText(TerminalText {
                text: "aЖ🙂b".to_owned(),
            }),
            4,
            32,
        )
        .unwrap();
        let decoded: Vec<_> = prepared
            .writes
            .iter()
            .map(|write| std::str::from_utf8(&write.bytes).unwrap())
            .collect();
        assert_eq!(decoded, ["aЖ", "🙂", "b"]);
    }

    #[test]
    fn raw_controls_require_the_control_variant() {
        let error = prepare_input(InputAction::TerminalText(TerminalText {
            text: "hello\n".to_owned(),
        }))
        .unwrap_err();
        assert!(matches!(
            error,
            InputPrepareError::ControlCharacterInTerminalText { .. }
        ));
    }

    #[test]
    fn agent_command_is_target_bound_and_delays_submit() {
        let agent = AgentId::new("codex").unwrap();
        let prepared = prepare_agent_command(
            AgentCommand {
                agent_id: agent.clone(),
                name: "review".to_owned(),
                arguments: vec!["focus on PTY ordering".to_owned()],
            },
            &agent,
        )
        .unwrap();
        assert_eq!(prepared.writes[0].bytes, b"/review focus on PTY ordering");
        assert_eq!(
            prepared.writes.last().unwrap().delay_before_ms,
            TERMINAL_SUBMIT_DELAY_MS
        );
    }

    #[test]
    fn agent_command_rejects_controls_and_foreign_targets() {
        let codex = AgentId::new("codex").unwrap();
        let kimi = AgentId::new("kimi").unwrap();
        let foreign = prepare_agent_command(
            AgentCommand {
                agent_id: codex.clone(),
                name: "help".to_owned(),
                arguments: Vec::new(),
            },
            &kimi,
        )
        .unwrap_err();
        assert!(matches!(
            foreign,
            InputPrepareError::AgentTargetMismatch { .. }
        ));

        let control = prepare_agent_command(
            AgentCommand {
                agent_id: codex.clone(),
                name: "rename".to_owned(),
                arguments: vec!["unsafe\rsubmit".to_owned()],
            },
            &codex,
        )
        .unwrap_err();
        assert!(matches!(
            control,
            InputPrepareError::InvalidAgentCommandArgument { .. }
        ));
    }
}
