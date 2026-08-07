use gate4agent_types::{ProviderSessionIdentity, ProviderSessionKey};

pub const KIMI_PTY_SESSION_ID_MAX_BYTES: usize = 256;
const KIMI_PTY_IDENTITY_LINE_MAX_BYTES: usize = 1_024;
const CODEX_PTY_SESSION_ID_BYTES: usize = 36;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum AnsiState {
    #[default]
    Ground,
    Escape,
    Csi,
    Osc,
    OscEscape,
    ControlString,
    ControlStringEscape,
}

/// Extracts the one explicit Kimi terminal identity label without treating
/// arbitrary output, prompt echo, or an internal PTY UUID as provider truth.
#[derive(Debug, Default)]
pub struct KimiPtySessionIdentityExtractor {
    ansi: AnsiState,
    line: String,
    overflowed: bool,
    observed: Option<String>,
}

impl KimiPtySessionIdentityExtractor {
    pub fn push(&mut self, chunk: &str) -> Option<ProviderSessionIdentity> {
        if self.observed.is_some() {
            return None;
        }
        for character in chunk.chars() {
            if let Some(id) = self.push_character(character) {
                self.observed = Some(id.clone());
                return Some(ProviderSessionIdentity {
                    key: ProviderSessionKey::SessionId,
                    id,
                    transcript_path: None,
                });
            }
        }
        None
    }

    /// A transport gap invalidates partial ANSI and line state, but an already
    /// observed identity remains authoritative for the session lifetime.
    pub fn reset_stream(&mut self) {
        self.ansi = AnsiState::Ground;
        self.line.clear();
        self.overflowed = false;
    }

    /// Reads Kimi's own rendered welcome or `/status` panel. The status panel
    /// intentionally uses `Session  session_*` without a colon and is accepted
    /// only from a terminal screen snapshot, never from arbitrary raw output.
    pub fn observe_screen(&mut self, screen: &str) -> Option<ProviderSessionIdentity> {
        if self.observed.is_some() {
            return None;
        }
        let id = screen
            .lines()
            .find_map(|line| explicit_kimi_session_id(line, true, true))?;
        self.observed = Some(id.clone());
        Some(ProviderSessionIdentity {
            key: ProviderSessionKey::SessionId,
            id,
            transcript_path: None,
        })
    }

    fn push_character(&mut self, character: char) -> Option<String> {
        match self.ansi {
            AnsiState::Ground => match character {
                '\u{1b}' => self.ansi = AnsiState::Escape,
                '\r' | '\n' => {
                    let identity = (!self.overflowed)
                        .then(|| explicit_kimi_session_id(&self.line, true, false))
                        .flatten();
                    self.line.clear();
                    self.overflowed = false;
                    return identity;
                }
                '\t' if !self.overflowed => {
                    self.line.push('\t');
                    return explicit_kimi_session_id(&self.line, false, false);
                }
                character if character.is_control() => {
                    self.line.clear();
                    self.overflowed = true;
                }
                character if !self.overflowed => {
                    if self.line.len().saturating_add(character.len_utf8())
                        > KIMI_PTY_IDENTITY_LINE_MAX_BYTES
                    {
                        self.line.clear();
                        self.overflowed = true;
                    } else {
                        self.line.push(character);
                        return explicit_kimi_session_id(&self.line, false, false);
                    }
                }
                _ => {}
            },
            AnsiState::Escape => {
                self.ansi = match character {
                    '[' => AnsiState::Csi,
                    ']' => AnsiState::Osc,
                    'P' | '_' | '^' | 'X' => AnsiState::ControlString,
                    _ => AnsiState::Ground,
                };
            }
            AnsiState::Csi => {
                if ('@'..='~').contains(&character) {
                    self.ansi = AnsiState::Ground;
                }
            }
            AnsiState::Osc => match character {
                '\u{7}' => self.ansi = AnsiState::Ground,
                '\u{1b}' => self.ansi = AnsiState::OscEscape,
                _ => {}
            },
            AnsiState::OscEscape => {
                self.ansi = if character == '\\' {
                    AnsiState::Ground
                } else {
                    AnsiState::Osc
                };
            }
            AnsiState::ControlString => {
                if character == '\u{1b}' {
                    self.ansi = AnsiState::ControlStringEscape;
                }
            }
            AnsiState::ControlStringEscape => {
                self.ansi = if character == '\\' {
                    AnsiState::Ground
                } else {
                    AnsiState::ControlString
                };
            }
        }
        None
    }
}

/// Extracts Codex's canonical thread UUID only from its rendered `/status`
/// card. Raw PTY output is deliberately not accepted because prompt/model
/// echoes can contain status-shaped text.
#[derive(Debug, Default)]
pub struct CodexPtySessionIdentityExtractor {
    observed: Option<String>,
}

impl CodexPtySessionIdentityExtractor {
    pub fn observe_screen(&mut self, screen: &str) -> Option<ProviderSessionIdentity> {
        if self.observed.is_some() {
            return None;
        }
        let id = screen.lines().find_map(explicit_codex_status_session_id)?;
        self.observed = Some(id.clone());
        Some(ProviderSessionIdentity {
            key: ProviderSessionKey::SessionId,
            id,
            transcript_path: None,
        })
    }
}

fn explicit_codex_status_session_id(line: &str) -> Option<String> {
    let line = line.trim_start_matches([' ', '\t']);
    let border = line.chars().next()?;
    if !matches!(border, '│' | '┃' | '║') {
        return None;
    }
    let line = line[border.len_utf8()..].trim_start_matches([' ', '\t']);
    let remainder = line.strip_prefix("Session:")?;
    let whitespace = remainder
        .chars()
        .take_while(|character| matches!(character, ' ' | '\t'))
        .count();
    if whitespace < 3 {
        return None;
    }
    let remainder = remainder.trim_start_matches([' ', '\t']);
    if remainder.len() < CODEX_PTY_SESSION_ID_BYTES {
        return None;
    }
    let (candidate, trailing) = remainder.split_at(CODEX_PTY_SESSION_ID_BYTES);
    if !is_canonical_codex_thread_id(candidate) {
        return None;
    }
    let trailing = trailing.trim_matches([' ', '\t']);
    (trailing.chars().count() == 1 && trailing.starts_with(border))
        .then(|| candidate.to_owned())
}

fn is_canonical_codex_thread_id(candidate: &str) -> bool {
    candidate.len() == CODEX_PTY_SESSION_ID_BYTES
        && candidate.bytes().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                byte == b'-'
            } else {
                byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)
            }
        })
}

fn explicit_kimi_session_id(
    line: &str,
    line_complete: bool,
    allow_status_panel: bool,
) -> Option<String> {
    let mut line = line.trim_start_matches([' ', '\t']);
    let framed = line.starts_with(['│', '┃', '║']);
    if framed {
        let border_bytes = line
            .chars()
            .next()
            .expect("framed line has a leading border")
            .len_utf8();
        line = line[border_bytes..].trim_start_matches([' ', '\t']);
    }
    let (remainder, minimum_space) = match line.strip_prefix("Session:") {
        Some(remainder) => (remainder, 1),
        None if allow_status_panel => (line.strip_prefix("Session")?, 2),
        None => return None,
    };
    let whitespace = remainder
        .chars()
        .take_while(|character| matches!(character, ' ' | '\t'))
        .count();
    if whitespace < minimum_space {
        return None;
    }
    let remainder = remainder.trim_start_matches([' ', '\t']);
    if !remainder.starts_with("session_") {
        return None;
    }
    let token_bytes = remainder
        .bytes()
        .take_while(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        .count();
    let token = &remainder[..token_bytes];
    if token.len() <= "session_".len() || token.len() > KIMI_PTY_SESSION_ID_MAX_BYTES {
        return None;
    }
    let trailing = &remainder[token_bytes..];
    if trailing.is_empty() {
        return line_complete.then(|| token.to_owned());
    }
    let trailing = trailing.trim_matches([' ', '\t']);
    (trailing.is_empty()
        || (framed && matches!(trailing, "│" | "┃" | "║")))
        .then(|| token.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kimi_identity_is_ansi_aware_and_chunk_safe() {
        let mut extractor = KimiPtySessionIdentityExtractor::default();
        assert_eq!(extractor.push("\u{1b}[2;"), None);
        assert_eq!(extractor.push("1mSess"), None);
        assert_eq!(extractor.push("ion:\u{1b}[0m sess"), None);
        assert_eq!(extractor.push("ion_ab-19\r").unwrap().id, "session_ab-19");
        assert_eq!(extractor.push("Session: session_other\n"), None);
    }

    #[test]
    fn kimi_identity_rejects_echoes_unsafe_tokens_and_ansi_payloads() {
        for output in [
            "user echoed Session: session_injected\n",
            "Session: session_bad/path\n",
            "Session: session_bad:tail\n",
            "Session: session_\n",
            "Session: 7d8a6f4e\n",
            "Session  session_status-only\n",
            "\u{1b}]0;Session: session_title\u{7}ready\n",
            "\u{1b}PSession: session_dcs\u{1b}\\ready\n",
            "Sess\u{8}ion: session_backspace\n",
        ] {
            let mut extractor = KimiPtySessionIdentityExtractor::default();
            assert_eq!(extractor.push(output), None, "{output:?}");
        }
    }

    #[test]
    fn kimi_status_screen_accepts_the_exact_colonless_framed_label() {
        let mut extractor = KimiPtySessionIdentityExtractor::default();
        let identity = extractor
            .observe_screen(
                "╭ Status ─────────────╮\n  │ Session       session_c63be266-18dc-45dc-8a5c-50831076c260 │\n  ╰──────────────────────╯",
            )
            .expect("Kimi status identity");
        assert_eq!(
            identity.id,
            "session_c63be266-18dc-45dc-8a5c-50831076c260"
        );
        assert_eq!(extractor.push("Session: session_other\n"), None);
    }

    #[test]
    fn stream_reset_never_joins_identity_across_a_transport_gap() {
        let mut extractor = KimiPtySessionIdentityExtractor::default();
        assert_eq!(extractor.push("Session: sess"), None);
        extractor.reset_stream();
        assert_eq!(extractor.push("ion_false\n"), None);
        assert_eq!(
            extractor.push("Session: session_true\n").unwrap().id,
            "session_true"
        );
    }

    #[test]
    fn codex_status_screen_accepts_only_the_exact_framed_session_uuid() {
        let mut extractor = CodexPtySessionIdentityExtractor::default();
        let identity = extractor
            .observe_screen(
                "╭────────────────────────────────────────────╮\n│  Model:            gpt-5.4                   │\n│  Session:          0f0f3c13-6cf9-4aa4-8b80-7d49c2f1be2e │\n╰────────────────────────────────────────────╯",
            )
            .expect("Codex status identity");
        assert_eq!(identity.key, ProviderSessionKey::SessionId);
        assert_eq!(identity.id, "0f0f3c13-6cf9-4aa4-8b80-7d49c2f1be2e");
        assert_eq!(identity.transcript_path, None);
        assert_eq!(
            extractor.observe_screen(
                "│  Session:          11111111-1111-4111-8111-111111111111 │"
            ),
            None
        );
    }

    #[test]
    fn codex_status_screen_rejects_unframed_echoes_and_noncanonical_ids() {
        for screen in [
            "Session:          0f0f3c13-6cf9-4aa4-8b80-7d49c2f1be2e",
            "│ user echoed Session:          0f0f3c13-6cf9-4aa4-8b80-7d49c2f1be2e │",
            "│  Session           0f0f3c13-6cf9-4aa4-8b80-7d49c2f1be2e │",
            "│  Session:  0f0f3c13-6cf9-4aa4-8b80-7d49c2f1be2e │",
            "│  Session:          0F0F3C13-6CF9-4AA4-8B80-7D49C2F1BE2E │",
            "│  Session:          0f0f3c13-6cf9-4aa4-8b80-7d49c2f1be2e trailing │",
            "│  Session:          0f0f3c13/6cf9-4aa4-8b80-7d49c2f1be2e │",
        ] {
            let mut extractor = CodexPtySessionIdentityExtractor::default();
            assert_eq!(extractor.observe_screen(screen), None, "{screen:?}");
        }
    }
}
