use super::{AgentId, AgentRegistry, AgentSpec, RuntimePlatform};

const INTERPRETERS: &[&str] = &[
    "node",
    "python",
    "python3",
    "bash",
    "zsh",
    "sh",
    "fish",
    "pwsh",
    "powershell",
];
const INTERPRETER_OPTIONS_WITH_VALUE: &[&str] = &[
    "-r",
    "--require",
    "--import",
    "--loader",
    "--experimental-loader",
];
const INTERPRETER_INLINE_SOURCE_OPTIONS: &[&str] = &["-e", "--eval", "-p", "--print", "--check"];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecognitionPath {
    DirectProcess,
    CommandEntrypoint,
    NodePackageEntrypoint,
    PythonModule,
    PythonScript,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecognizedAgentProcess {
    pub agent_id: AgentId,
    pub process_name: String,
    pub path: RecognitionPath,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProcessRecognitionOptions {
    pub include_headless_one_shot: bool,
}

impl Default for ProcessRecognitionOptions {
    fn default() -> Self {
        Self {
            include_headless_one_shot: false,
        }
    }
}

pub fn recognize_agent_process(
    registry: &AgentRegistry,
    process_or_path: &str,
    platform: RuntimePlatform,
) -> Option<RecognizedAgentProcess> {
    let normalized = normalize_process_name(process_or_path, platform, false);
    if normalized.is_empty() {
        return None;
    }

    if let Some(spec) = registry.find_by_command(&normalized, platform) {
        return Some(recognized(spec, normalized, RecognitionPath::DirectProcess));
    }
    registry
        .iter()
        .find(|spec| {
            spec.expected_processes
                .iter()
                .any(|matcher| matcher.matches(&normalized, platform))
        })
        .map(|spec| recognized(spec, normalized, RecognitionPath::DirectProcess))
}

pub fn recognize_agent_process_from_command_line(
    registry: &AgentRegistry,
    command_line: &str,
    platform: RuntimePlatform,
    options: ProcessRecognitionOptions,
) -> Option<RecognizedAgentProcess> {
    let tokens = tokenize_command_line(command_line);
    let first = tokens.first()?;
    let first_normalized = normalize_process_name(first, platform, false);

    if let Some(direct) = recognize_agent_process(registry, first, platform) {
        if options.include_headless_one_shot
            || !is_headless_one_shot(direct.agent_id.as_str(), &tokens)
        {
            return Some(direct);
        }
    }

    let entrypoint = find_interpreter_entrypoint(&tokens, &first_normalized)?;
    let mut via_entrypoint = if is_python_process(&first_normalized) {
        recognize_python_entrypoint(registry, &tokens, entrypoint, platform)
    } else {
        recognize_agent_process(registry, entrypoint, platform)
            .map(|mut value| {
                value.path = RecognitionPath::CommandEntrypoint;
                value
            })
            .or_else(|| recognize_node_package_entrypoint(registry, entrypoint, platform))
    }?;

    if !options.include_headless_one_shot
        && is_headless_one_shot(via_entrypoint.agent_id.as_str(), &tokens)
    {
        return None;
    }
    if via_entrypoint.path == RecognitionPath::DirectProcess {
        via_entrypoint.path = RecognitionPath::CommandEntrypoint;
    }
    Some(via_entrypoint)
}

pub fn is_expected_agent_process(
    spec: &AgentSpec,
    process_or_path: &str,
    platform: RuntimePlatform,
) -> bool {
    let normalized = normalize_process_name(process_or_path, platform, false);
    spec.expected_processes
        .iter()
        .any(|matcher| matcher.matches(&normalized, platform))
}

/// Match a full process command line against one specification, including
/// controlled Node/Python entrypoints and the headless one-shot exclusions.
pub fn is_expected_agent_command_line(
    spec: &AgentSpec,
    command_line: &str,
    platform: RuntimePlatform,
) -> bool {
    let Ok(registry) = AgentRegistry::new([spec.clone()]) else {
        return false;
    };
    recognize_agent_process_from_command_line(
        &registry,
        command_line,
        platform,
        ProcessRecognitionOptions::default(),
    )
    .is_some_and(|recognized| recognized.agent_id == spec.id)
}

pub fn is_agent_foreground_wrapper(process_or_path: &str, platform: RuntimePlatform) -> bool {
    let normalized = normalize_process_name(process_or_path, platform, false);
    matches!(normalized.as_str(), "node" | "python" | "python3") || is_python_process(&normalized)
}

pub fn tokenize_command_line(command_line: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut escaped = false;
    let characters: Vec<char> = command_line.chars().collect();

    for (index, character) in characters.iter().copied().enumerate() {
        if escaped {
            current.push(character);
            escaped = false;
            continue;
        }
        if character == '\\' && quote != Some('\'') {
            if characters
                .get(index + 1)
                .is_some_and(|next| next.is_whitespace() || matches!(*next, '"' | '\'' | '\\'))
            {
                escaped = true;
                continue;
            }
        }
        if matches!(character, '"' | '\'') && quote.is_none() {
            quote = Some(character);
            continue;
        }
        if quote == Some(character) {
            quote = None;
            continue;
        }
        if character.is_whitespace() && quote.is_none() {
            if !current.is_empty() {
                tokens.push(std::mem::take(&mut current));
            }
            continue;
        }
        current.push(character);
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

fn recognized(
    spec: &AgentSpec,
    process_name: String,
    path: RecognitionPath,
) -> RecognizedAgentProcess {
    RecognizedAgentProcess {
        agent_id: spec.id.clone(),
        process_name,
        path,
    }
}

fn find_interpreter_entrypoint<'a>(tokens: &'a [String], first: &str) -> Option<&'a str> {
    if !is_interpreter_process(first) {
        return None;
    }
    let mut index = 1;
    while index < tokens.len() {
        let token = &tokens[index];
        if token == "--" {
            index += 1;
            continue;
        }
        if is_python_process(first) && token == "-m" {
            return tokens.get(index + 1).map(String::as_str);
        }
        if token.starts_with('-') {
            let option = option_name(token);
            if INTERPRETER_INLINE_SOURCE_OPTIONS.contains(&option) {
                return None;
            }
            if INTERPRETER_OPTIONS_WITH_VALUE.contains(&option) && option == token {
                index += 2;
            } else {
                index += 1;
            }
            continue;
        }
        if token.contains('/')
            || token.contains('\\')
            || has_process_extension(token)
            || is_python_process(first)
        {
            return Some(token);
        }
        index += 1;
    }
    None
}

fn recognize_node_package_entrypoint(
    registry: &AgentRegistry,
    token: &str,
    platform: RuntimePlatform,
) -> Option<RecognizedAgentProcess> {
    let path = comparable_path(token);
    let (command, marker) = if path.contains("node_modules/@openai/codex/") {
        ("codex", "codex")
    } else if path.contains("node_modules/@google/gemini-cli/") {
        ("gemini", "gemini")
    } else {
        return None;
    };
    let basename = normalize_process_name(token, platform, true);
    if basename != marker {
        return None;
    }
    let spec = registry.find_by_command(command, platform)?;
    Some(recognized(
        spec,
        basename,
        RecognitionPath::NodePackageEntrypoint,
    ))
}

fn recognize_python_entrypoint(
    registry: &AgentRegistry,
    tokens: &[String],
    entrypoint: &str,
    platform: RuntimePlatform,
) -> Option<RecognizedAgentProcess> {
    if let Some(module_index) = tokens.iter().position(|token| token == "-m") {
        let module = tokens.get(module_index + 1)?;
        if module.starts_with('-') {
            return None;
        }
        let command = module.split('.').next()?.to_ascii_lowercase();
        let spec = registry.find_by_command(&command, platform)?;
        return Some(recognized(spec, command, RecognitionPath::PythonModule));
    }

    if let Some(mut direct) = recognize_agent_process(registry, entrypoint, platform) {
        direct.path = RecognitionPath::CommandEntrypoint;
        return Some(direct);
    }

    let path = comparable_path(entrypoint);
    if !(path.ends_with(".py") || path.ends_with(".pyw"))
        || !["/bin/", "/scripts/", "/site-packages/"]
            .iter()
            .any(|marker| path.contains(marker))
    {
        return None;
    }
    let command = normalize_process_name(entrypoint, platform, true);
    let spec = registry.find_by_command(&command, platform)?;
    Some(recognized(spec, command, RecognitionPath::PythonScript))
}

fn normalize_process_name(
    value: &str,
    platform: RuntimePlatform,
    strip_script_extension: bool,
) -> String {
    let unquoted = value.trim().trim_matches(['"', '\'']);
    let basename = unquoted.rsplit(['/', '\\']).next().unwrap_or_default();
    let mut normalized = if platform == RuntimePlatform::Windows {
        basename.to_ascii_lowercase()
    } else {
        basename.to_owned()
    };
    for extension in [".exe", ".cmd", ".bat", ".ps1"] {
        if normalized.to_ascii_lowercase().ends_with(extension) {
            normalized.truncate(normalized.len() - extension.len());
            break;
        }
    }
    if strip_script_extension {
        for extension in [".mjs", ".cjs", ".js", ".pyw", ".py"] {
            if normalized.to_ascii_lowercase().ends_with(extension) {
                normalized.truncate(normalized.len() - extension.len());
                break;
            }
        }
    }
    normalized
}

fn comparable_path(value: &str) -> String {
    value
        .trim()
        .trim_matches(['"', '\''])
        .replace('\\', "/")
        .to_ascii_lowercase()
}

fn has_process_extension(value: &str) -> bool {
    let lowercase = value.to_ascii_lowercase();
    [".exe", ".cmd", ".bat", ".ps1"]
        .iter()
        .any(|extension| lowercase.ends_with(extension))
}

fn is_interpreter_process(value: &str) -> bool {
    INTERPRETERS.contains(&value) || is_python_process(value)
}

fn is_python_process(value: &str) -> bool {
    let Some(rest) = value.strip_prefix("python") else {
        return false;
    };
    rest.is_empty()
        || rest == "3"
        || rest
            .split('.')
            .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
}

fn option_name(token: &str) -> &str {
    token.split_once('=').map_or(token, |(name, _)| name)
}

fn is_headless_one_shot(agent_id: &str, tokens: &[String]) -> bool {
    match agent_id {
        "claude" => tokens.iter().skip(1).enumerate().any(|(offset, token)| {
            let index = offset + 1;
            let name = option_name(token);
            if matches!(name, "--print" | "-p") {
                return true;
            }
            if name != "--output-format" {
                return false;
            }
            let value = token
                .split_once('=')
                .map(|(_, value)| value)
                .or_else(|| tokens.get(index + 1).map(String::as_str));
            value.is_some_and(|value| {
                matches!(value.to_ascii_lowercase().as_str(), "json" | "stream-json")
            })
        }),
        "ante" => tokens.iter().skip(1).any(|token| {
            let name = option_name(token);
            matches!(name, "--prompt" | "-p")
                || (name.starts_with("-p") && !name.starts_with("--") && name.len() > 2)
        }),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::builtin_registry;

    fn recognize(command: &str) -> Option<RecognizedAgentProcess> {
        recognize_agent_process_from_command_line(
            builtin_registry(),
            command,
            RuntimePlatform::Linux,
            ProcessRecognitionOptions::default(),
        )
    }

    #[test]
    fn recognizes_direct_and_versioned_processes() {
        let qwen = recognize_agent_process(
            builtin_registry(),
            r"C:\Users\dev\npm\qwen.cmd",
            RuntimePlatform::Windows,
        )
        .unwrap();
        assert_eq!(qwen.agent_id.as_str(), "qwen-code");

        let grok =
            recognize_agent_process(builtin_registry(), "grok-0.2.51", RuntimePlatform::Linux)
                .unwrap();
        assert_eq!(grok.agent_id.as_str(), "grok");
    }

    #[test]
    fn recognizes_interpreter_entrypoints_without_scanning_prompts() {
        assert_eq!(
            recognize("python -m aider").unwrap().agent_id.as_str(),
            "aider"
        );
        assert_eq!(
            recognize("python3 /opt/homebrew/bin/hermes --tui")
                .unwrap()
                .agent_id
                .as_str(),
            "hermes"
        );
        assert_eq!(
            recognize("node /home/dev/node_modules/@openai/codex/bin/codex.js")
                .unwrap()
                .agent_id
                .as_str(),
            "codex"
        );
        assert!(recognize("node /tmp/not-an-agent.js compare opencode and kimi").is_none());
    }

    #[test]
    fn filters_known_headless_one_shots() {
        assert!(recognize("claude --print summarize").is_none());
        assert!(recognize("claude --output-format=json summarize").is_none());
        assert!(recognize("ante -psummarize").is_none());
        assert_eq!(
            recognize("claude --resume abc").unwrap().agent_id.as_str(),
            "claude"
        );
    }

    #[test]
    fn tokenizer_preserves_quoted_prompt_as_one_token() {
        assert_eq!(
            tokenize_command_line(r#"node "C:\Program Files\agent.js" "hello world""#),
            ["node", r"C:\Program Files\agent.js", "hello world"]
        );
    }
}
