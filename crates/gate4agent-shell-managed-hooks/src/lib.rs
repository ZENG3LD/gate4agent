//! Explicit native authority for reversible provider Hook configuration.
//!
//! No constructor or status call mutates disk. Mutation requires a caller to
//! create a bounded plan and then apply that exact plan. Apply rejects drift
//! between planning and writing, so a provider or user edit wins over a stale
//! Gate4Agent plan.

use gate4agent_adapters::{
    managed_hook_spec, ManagedHookAdapterError, ManagedHookAdapterSpec, ManagedHookConfigKind,
    ManagedHookConfigLocation, ManagedHookEventShape, ManagedHookEventSpec,
};
use gate4agent_shell_hooks::{HookIngressEndpoint, HOOK_INGRESS_PROTOCOL_VERSION};
use gate4agent_types::{AdapterBinding, RuntimePlatform};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use thiserror::Error;

pub const MANAGED_HOOK_PLAN_MAX_ACTIONS: usize = 8;
pub const MANAGED_HOOK_FILE_MAX_BYTES: usize = 4 * 1024 * 1024;
const MANAGED_MARKER: &str = "Managed by Gate4Agent. Do not edit; changes may be overwritten.";
const KIMI_BLOCK_START: &str =
    "# >>> gate4agent-managed-kimi-hooks (managed by Gate4Agent; do not edit) >>>";
const KIMI_BLOCK_END: &str = "# <<< gate4agent-managed-kimi-hooks <<<";
static NEXT_TEMP_FILE: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManagedHookRoots {
    pub home: PathBuf,
    pub runtime_data: PathBuf,
    pub app_data: Option<PathBuf>,
    pub platform: RuntimePlatform,
    pub system_root: Option<PathBuf>,
    pub environment_homes: BTreeMap<String, PathBuf>,
}

impl ManagedHookRoots {
    pub fn validate(&self) -> Result<(), ManagedHookError> {
        for path in [
            Some(&self.home),
            Some(&self.runtime_data),
            self.app_data.as_ref(),
        ]
        .into_iter()
        .flatten()
        {
            if !path.is_absolute() {
                return Err(ManagedHookError::RootMustBeAbsolute(path.clone()));
            }
        }
        if self.platform == RuntimePlatform::Windows
            && self
                .system_root
                .as_ref()
                .is_none_or(|path| !path.is_absolute())
        {
            return Err(ManagedHookError::MissingWindowsSystemRoot);
        }
        if self.platform == RuntimePlatform::Windows
            && self.system_root.as_ref().is_some_and(|path| {
                path.to_string_lossy().chars().any(|character| {
                    character.is_whitespace()
                        || character.is_control()
                        || matches!(character, '"' | '\'')
                })
            })
        {
            return Err(ManagedHookError::UnsafeWindowsSystemRoot);
        }
        for path in self.environment_homes.values() {
            if !path.is_absolute() {
                return Err(ManagedHookError::RootMustBeAbsolute(path.clone()));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ManagedHookOperation {
    Install,
    Remove,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ManagedHookState {
    Installed,
    ApprovalRequired,
    NotInstalled,
    Partial,
    Conflict,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManagedHookStatus {
    pub target: String,
    pub state: ManagedHookState,
    pub config_path: PathBuf,
    pub managed_hooks_present: bool,
    pub detail: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManagedHookActionSummary {
    pub path: PathBuf,
    pub kind: ManagedHookActionKind,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PublishedHookEndpoint {
    pub posix_path: PathBuf,
    pub windows_path: PathBuf,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ManagedHookActionKind {
    Create,
    Replace,
    Remove,
}

#[derive(Clone, Debug)]
pub struct ManagedHookPlan {
    target: String,
    operation: ManagedHookOperation,
    before: ManagedHookStatus,
    actions: Vec<PlannedFileMutation>,
}

impl ManagedHookPlan {
    pub fn target(&self) -> &str {
        &self.target
    }

    pub fn operation(&self) -> ManagedHookOperation {
        self.operation
    }

    pub fn before(&self) -> &ManagedHookStatus {
        &self.before
    }

    pub fn actions(&self) -> Vec<ManagedHookActionSummary> {
        self.actions
            .iter()
            .map(|action| ManagedHookActionSummary {
                path: action.path.clone(),
                kind: match (&action.expected, &action.replacement) {
                    (FileExpectation::Absent, Some(_)) => ManagedHookActionKind::Create,
                    (_, Some(_)) => ManagedHookActionKind::Replace,
                    (_, None) => ManagedHookActionKind::Remove,
                },
            })
            .collect()
    }

    pub fn is_noop(&self) -> bool {
        self.actions.is_empty()
    }
}

#[derive(Clone, Debug)]
struct PlannedFileMutation {
    path: PathBuf,
    expected: FileExpectation,
    replacement: Option<Vec<u8>>,
    executable: bool,
}

#[derive(Clone, Debug)]
enum FileExpectation {
    Absent,
    Bytes(Vec<u8>),
}

pub struct ManagedHookManager {
    roots: ManagedHookRoots,
}

impl ManagedHookManager {
    pub fn new(roots: ManagedHookRoots) -> Result<Self, ManagedHookError> {
        roots.validate()?;
        Ok(Self { roots })
    }

    pub fn status(&self, binding: &AdapterBinding) -> Result<ManagedHookStatus, ManagedHookError> {
        let spec = managed_hook_spec(binding)?;
        self.status_spec(spec)
    }

    pub fn plan(
        &self,
        binding: &AdapterBinding,
        operation: ManagedHookOperation,
    ) -> Result<ManagedHookPlan, ManagedHookError> {
        let spec = managed_hook_spec(binding)?;
        let before = self.status_spec(spec)?;
        let mut actions = match spec.config_kind {
            ManagedHookConfigKind::JsonHooks { .. } => self.plan_json(spec, operation)?,
            ManagedHookConfigKind::AmpPlugin => self.plan_amp(spec, operation)?,
            ManagedHookConfigKind::KimiToml => self.plan_kimi(spec, operation)?,
        };
        actions.retain(|action| !mutation_is_noop(action));
        if actions.len() > MANAGED_HOOK_PLAN_MAX_ACTIONS {
            return Err(ManagedHookError::PlanTooLarge(actions.len()));
        }
        Ok(ManagedHookPlan {
            target: spec.target.to_owned(),
            operation,
            before,
            actions,
        })
    }

    pub fn apply(&self, plan: ManagedHookPlan) -> Result<ManagedHookStatus, ManagedHookError> {
        for action in &plan.actions {
            verify_expectation(action)?;
        }
        let mut applied = Vec::new();
        for action in &plan.actions {
            if let Err(apply_error) =
                verify_expectation(action).and_then(|()| apply_mutation(action))
            {
                let mut rollback_error = None;
                for applied_action in applied.into_iter().rev() {
                    if let Err(error) = rollback_mutation(applied_action) {
                        rollback_error = Some(error);
                        break;
                    }
                }
                return match rollback_error {
                    Some(rollback_error) => Err(ManagedHookError::ApplyRollbackFailed {
                        apply: apply_error.to_string(),
                        rollback: rollback_error.to_string(),
                    }),
                    None => Err(apply_error),
                };
            }
            applied.push(action);
        }
        let binding = gate4agent_adapters::builtin_adapter_registry()
            .binding(gate4agent_types::AdapterFamily::ManagedHook, &plan.target)
            .ok_or_else(|| ManagedHookError::UnknownTarget(plan.target.clone()))?;
        self.status(binding)
    }

    /// Publishes refreshable listener coordinates for providers such as
    /// Command Code that sanitize TOKEN-like variables before running hooks.
    /// The files contain only Gate4Agent loopback ingress authority; provider
    /// credentials are never read or copied.
    pub fn publish_ingress_endpoint(
        &self,
        endpoint: &HookIngressEndpoint,
    ) -> Result<PublishedHookEndpoint, ManagedHookError> {
        let published = self.endpoint_paths()?;
        let posix_original = read_optional_bounded(&published.posix_path)?;
        let windows_original = read_optional_bounded(&published.windows_path)?;
        ensure_generated_file_or_absent(&published.posix_path, posix_original.as_deref())?;
        ensure_generated_file_or_absent(&published.windows_path, windows_original.as_deref())?;
        let fields = [
            ("GATE4AGENT_HOOK_PORT", endpoint.port().to_string()),
            (
                "GATE4AGENT_HOOK_TOKEN",
                endpoint.authorization_token().to_owned(),
            ),
            (
                "GATE4AGENT_HOOK_VERSION",
                HOOK_INGRESS_PROTOCOL_VERSION.to_owned(),
            ),
        ];
        let posix = format!(
            "# {MANAGED_MARKER}\n{}\n",
            fields
                .iter()
                .map(|(key, value)| format!("{key}={value}"))
                .collect::<Vec<_>>()
                .join("\n")
        );
        let windows = format!(
            "rem {MANAGED_MARKER}\r\n{}\r\n",
            fields
                .iter()
                .map(|(key, value)| format!("{key}={value}"))
                .collect::<Vec<_>>()
                .join("\r\n")
        );
        atomic_write_ephemeral(&published.posix_path, posix.as_bytes())?;
        if let Err(error) = atomic_write_ephemeral(&published.windows_path, windows.as_bytes()) {
            let _ = match posix_original {
                Some(bytes) => atomic_write_ephemeral(&published.posix_path, &bytes),
                None => fs::remove_file(&published.posix_path).map_err(ManagedHookError::from),
            };
            return Err(error);
        }
        Ok(published)
    }

    pub fn remove_published_ingress_endpoint(&self) -> Result<(), ManagedHookError> {
        let paths = self.endpoint_paths()?;
        let files = [paths.posix_path, paths.windows_path]
            .into_iter()
            .map(|path| read_optional_bounded(&path).map(|bytes| (path, bytes)))
            .collect::<Result<Vec<_>, _>>()?;
        for (path, bytes) in &files {
            ensure_generated_file_or_absent(path, bytes.as_deref())?;
        }
        for (path, bytes) in files {
            if bytes.is_some() {
                fs::remove_file(path)?;
            }
        }
        Ok(())
    }

    fn config_path(&self, spec: &ManagedHookAdapterSpec) -> Result<PathBuf, ManagedHookError> {
        let path = match spec.config_location {
            ManagedHookConfigLocation::HomeRelative(relative) => {
                checked_join(&self.roots.home, relative)?
            }
            ManagedHookConfigLocation::RuntimeDataRelative(relative) => {
                checked_join(&self.roots.runtime_data, relative)?
            }
            ManagedHookConfigLocation::EnvironmentHome {
                variable,
                fallback,
                suffix,
            } => {
                let base = self
                    .roots
                    .environment_homes
                    .get(variable)
                    .cloned()
                    .unwrap_or(checked_join(&self.roots.home, fallback)?);
                checked_join(&base, suffix)?
            }
            ManagedHookConfigLocation::AppDataOrHome {
                app_data_suffix,
                home_fallback,
            } => {
                if self.roots.platform == RuntimePlatform::Windows {
                    checked_join(
                        self.roots
                            .app_data
                            .as_ref()
                            .ok_or(ManagedHookError::MissingAppData)?,
                        app_data_suffix,
                    )?
                } else {
                    checked_join(&self.roots.home, home_fallback)?
                }
            }
        };
        Ok(path)
    }

    fn script_path(&self, spec: &ManagedHookAdapterSpec) -> Result<PathBuf, ManagedHookError> {
        let extension = if spec.target == "kimi" {
            "sh"
        } else if self.roots.platform == RuntimePlatform::Windows {
            "cmd"
        } else {
            "sh"
        };
        checked_join(
            &self.roots.home,
            &format!(".gate4agent/agent-hooks/{}.{}", spec.script_stem, extension),
        )
    }

    fn endpoint_paths(&self) -> Result<PublishedHookEndpoint, ManagedHookError> {
        Ok(PublishedHookEndpoint {
            posix_path: checked_join(&self.roots.home, ".gate4agent/agent-hooks/endpoint.env")?,
            windows_path: checked_join(&self.roots.home, ".gate4agent/agent-hooks/endpoint.cmd")?,
        })
    }

    fn managed_script(&self, spec: &ManagedHookAdapterSpec) -> Result<String, ManagedHookError> {
        Ok(managed_script(self.roots.platform, spec.target))
    }

    fn managed_command(
        &self,
        spec: &ManagedHookAdapterSpec,
        event: &ManagedHookEventSpec,
    ) -> Result<String, ManagedHookError> {
        let script_path = self.script_path(spec)?;
        if self.roots.platform == RuntimePlatform::Windows && spec.target != "kimi" {
            let system_root = self
                .roots
                .system_root
                .as_ref()
                .ok_or(ManagedHookError::MissingWindowsSystemRoot)?;
            let powershell = system_root
                .join("System32/WindowsPowerShell/v1.0/powershell.exe")
                .to_string_lossy()
                .replace('\\', "/");
            let quoted = powershell_quote(&script_path.to_string_lossy());
            let event_assignment = event.passes_event_name.then(|| {
                format!(
                    "$env:GATE4AGENT_HOOK_EVENT = {}; ",
                    powershell_quote(event.name)
                )
            });
            let command = format!(
                "{}if (Test-Path -LiteralPath {} -PathType Leaf) {{ & {}; exit $LASTEXITCODE }}; [Console]::In.ReadToEnd() | Out-Null; exit 0",
                event_assignment.unwrap_or_default(), quoted, quoted
            );
            return Ok(format!(
                "{} -NoProfile -NonInteractive -WindowStyle Hidden -ExecutionPolicy Bypass -EncodedCommand {}",
                powershell,
                base64_utf16le(&command)
            ));
        }
        let normalized = script_path.to_string_lossy().replace('\\', "/");
        let quoted = posix_quote(&normalized);
        let prefix = if event.passes_event_name {
            format!("GATE4AGENT_HOOK_EVENT={} ", posix_quote(event.name))
        } else {
            String::new()
        };
        Ok(format!(
            "if [ -f {quoted} ] && [ -r {quoted} ]; then {prefix}/bin/sh {quoted}; else cat >/dev/null; fi"
        ))
    }

    fn status_spec(
        &self,
        spec: &ManagedHookAdapterSpec,
    ) -> Result<ManagedHookStatus, ManagedHookError> {
        match spec.config_kind {
            ManagedHookConfigKind::JsonHooks { .. } => self.status_json(spec),
            ManagedHookConfigKind::AmpPlugin => self.status_amp(spec),
            ManagedHookConfigKind::KimiToml => self.status_kimi(spec),
        }
    }

    fn status_json(
        &self,
        spec: &ManagedHookAdapterSpec,
    ) -> Result<ManagedHookStatus, ManagedHookError> {
        let config_path = self.config_path(spec)?;
        let Some(bytes) = read_optional_bounded(&config_path)? else {
            return Ok(not_installed(spec, config_path));
        };
        let config = parse_json_config(spec, &bytes)?;
        let ManagedHookConfigKind::JsonHooks { container, .. } = spec.config_kind else {
            unreachable!()
        };
        let hook_map = config.get(container).and_then(Value::as_object);
        let mut present = 0;
        let mut any_managed = false;
        for event in spec.events {
            let command = self.managed_command(spec, event)?;
            let definitions = hook_map
                .and_then(|map| map.get(event.name))
                .and_then(Value::as_array);
            if definitions.is_some_and(|definitions| {
                definitions
                    .iter()
                    .any(|definition| definition_has_exact_command(definition, &command))
            }) {
                present += 1;
            }
        }
        if let Some(hook_map) = hook_map {
            any_managed = hook_map.values().any(|definitions| {
                definitions.as_array().is_some_and(|definitions| {
                    definitions.iter().any(|definition| {
                        definition_commands(definition)
                            .iter()
                            .any(|command| is_managed_command(spec, command))
                    })
                })
            });
        }
        let definitions_complete = present == spec.events.len();
        let approval_required = definitions_complete
            && spec.target == "codex"
            && !codex_hooks_are_trusted(self, spec, &config_path, &config)?;
        let state = if approval_required {
            ManagedHookState::ApprovalRequired
        } else if definitions_complete {
            ManagedHookState::Installed
        } else if present == 0 && !any_managed {
            ManagedHookState::NotInstalled
        } else {
            ManagedHookState::Partial
        };
        let detail = match state {
            ManagedHookState::ApprovalRequired => Some(
                "managed definitions are installed; approve them through Codex /hooks".to_owned(),
            ),
            ManagedHookState::Partial => Some(format!(
                "managed hooks present for {present}/{} events",
                spec.events.len()
            )),
            _ => None,
        };
        Ok(ManagedHookStatus {
            target: spec.target.to_owned(),
            state,
            config_path,
            managed_hooks_present: any_managed || present > 0,
            detail,
        })
    }

    fn plan_json(
        &self,
        spec: &ManagedHookAdapterSpec,
        operation: ManagedHookOperation,
    ) -> Result<Vec<PlannedFileMutation>, ManagedHookError> {
        let config_path = self.config_path(spec)?;
        let original = read_optional_bounded(&config_path)?;
        let mut config = match original.as_ref() {
            Some(bytes) => parse_json_config(spec, bytes)?,
            None => Value::Object(Map::new()),
        };
        let script_path = self.script_path(spec)?;
        let script_original = read_optional_bounded(&script_path)?;
        ensure_generated_file_or_absent(&script_path, script_original.as_deref())?;
        let codex_trust_action =
            if spec.target == "codex" && operation == ManagedHookOperation::Remove {
                plan_codex_trust_cleanup(self, spec, &config_path, &config)?
            } else {
                None
            };
        match operation {
            ManagedHookOperation::Install => {
                apply_json_install(self, spec, &mut config)?;
                let serialized =
                    format!("{}\n", serde_json::to_string_pretty(&config)?).into_bytes();
                Ok(vec![
                    mutation(
                        script_path,
                        script_original,
                        Some(self.managed_script(spec)?.into_bytes()),
                        self.roots.platform != RuntimePlatform::Windows,
                    ),
                    mutation(config_path, original, Some(serialized), false),
                ])
            }
            ManagedHookOperation::Remove => {
                apply_json_remove(spec, &mut config)?;
                let serialized =
                    format!("{}\n", serde_json::to_string_pretty(&config)?).into_bytes();
                let mut actions = vec![mutation(config_path, original, Some(serialized), false)];
                actions.extend(codex_trust_action);
                actions.push(mutation(script_path, script_original, None, false));
                Ok(actions)
            }
        }
    }

    fn status_amp(
        &self,
        spec: &ManagedHookAdapterSpec,
    ) -> Result<ManagedHookStatus, ManagedHookError> {
        let path = self.config_path(spec)?;
        let Some(bytes) = read_optional_bounded(&path)? else {
            return Ok(not_installed(spec, path));
        };
        let text = String::from_utf8_lossy(&bytes);
        if !text.contains(MANAGED_MARKER) {
            return Ok(conflict(
                spec,
                path,
                "Amp plugin path is occupied by an unmanaged file",
            ));
        }
        let complete = spec
            .events
            .iter()
            .all(|event| text.contains(&format!("amp.on('{}'", event.name)))
            && text.contains("GATE4AGENT_HOOK_ROUTE");
        Ok(ManagedHookStatus {
            target: spec.target.to_owned(),
            state: if complete {
                ManagedHookState::Installed
            } else {
                ManagedHookState::Partial
            },
            config_path: path,
            managed_hooks_present: true,
            detail: (!complete).then(|| "managed Amp plugin is incomplete or stale".to_owned()),
        })
    }

    fn plan_amp(
        &self,
        spec: &ManagedHookAdapterSpec,
        operation: ManagedHookOperation,
    ) -> Result<Vec<PlannedFileMutation>, ManagedHookError> {
        let path = self.config_path(spec)?;
        let original = read_optional_bounded(&path)?;
        if original
            .as_ref()
            .is_some_and(|bytes| !String::from_utf8_lossy(bytes).contains(MANAGED_MARKER))
        {
            return Err(ManagedHookError::UnmanagedConflict(path));
        }
        Ok(vec![mutation(
            path,
            original,
            (operation == ManagedHookOperation::Install).then(|| amp_plugin_source().into_bytes()),
            false,
        )])
    }

    fn status_kimi(
        &self,
        spec: &ManagedHookAdapterSpec,
    ) -> Result<ManagedHookStatus, ManagedHookError> {
        let path = self.config_path(spec)?;
        let Some(bytes) = read_optional_bounded(&path)? else {
            return Ok(not_installed(spec, path));
        };
        let text =
            String::from_utf8(bytes).map_err(|_| ManagedHookError::InvalidUtf8(path.clone()))?;
        let block = kimi_managed_block(&text);
        let present = block.map_or(0, |block| {
            spec.events
                .iter()
                .filter(|event| block.contains(&format!("event = \"{}\"", event.name)))
                .count()
        });
        Ok(ManagedHookStatus {
            target: spec.target.to_owned(),
            state: if present == spec.events.len() {
                ManagedHookState::Installed
            } else if present == 0 {
                ManagedHookState::NotInstalled
            } else {
                ManagedHookState::Partial
            },
            config_path: path,
            managed_hooks_present: present > 0,
            detail: (present > 0 && present != spec.events.len()).then(|| {
                format!(
                    "managed hooks present for {present}/{} events",
                    spec.events.len()
                )
            }),
        })
    }

    fn plan_kimi(
        &self,
        spec: &ManagedHookAdapterSpec,
        operation: ManagedHookOperation,
    ) -> Result<Vec<PlannedFileMutation>, ManagedHookError> {
        let config_path = self.config_path(spec)?;
        let original = read_optional_bounded(&config_path)?;
        let text = original
            .as_ref()
            .map(|bytes| {
                String::from_utf8(bytes.clone())
                    .map_err(|_| ManagedHookError::InvalidUtf8(config_path.clone()))
            })
            .transpose()?
            .unwrap_or_default();
        let stripped = strip_kimi_managed_block(&text);
        let script_path = self.script_path(spec)?;
        let script_original = read_optional_bounded(&script_path)?;
        ensure_generated_file_or_absent(&script_path, script_original.as_deref())?;
        match operation {
            ManagedHookOperation::Install => {
                let mut block = vec![KIMI_BLOCK_START.to_owned()];
                for event in spec.events {
                    block.extend([
                        "[[hooks]]".to_owned(),
                        format!("event = \"{}\"", event.name),
                        format!(
                            "command = \"{}\"",
                            toml_escape(&self.managed_command(spec, event)?)
                        ),
                        "timeout = 10".to_owned(),
                    ]);
                }
                block.push(KIMI_BLOCK_END.to_owned());
                let prefix = stripped.trim_end();
                let next = if prefix.is_empty() {
                    format!("{}\n", block.join("\n"))
                } else {
                    format!("{prefix}\n\n{}\n", block.join("\n"))
                };
                Ok(vec![
                    mutation(
                        script_path,
                        script_original,
                        Some(self.managed_script(spec)?.into_bytes()),
                        self.roots.platform != RuntimePlatform::Windows,
                    ),
                    mutation(config_path, original, Some(next.into_bytes()), false),
                ])
            }
            ManagedHookOperation::Remove => Ok(vec![
                mutation(config_path, original, Some(stripped.into_bytes()), false),
                mutation(script_path, script_original, None, false),
            ]),
        }
    }

}

fn checked_join(root: &Path, relative: &str) -> Result<PathBuf, ManagedHookError> {
    let relative = Path::new(relative);
    if relative.is_absolute()
        || relative.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(ManagedHookError::InvalidRelativePath(
            relative.to_path_buf(),
        ));
    }
    Ok(root.join(relative))
}

fn read_optional_bounded(path: &Path) -> Result<Option<Vec<u8>>, ManagedHookError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() {
                return Err(ManagedHookError::SymlinkPath(path.to_path_buf()));
            }
            if !metadata.is_file() {
                return Err(ManagedHookError::NotARegularFile(path.to_path_buf()));
            }
            if metadata.len() > MANAGED_HOOK_FILE_MAX_BYTES as u64 {
                return Err(ManagedHookError::FileTooLarge(path.to_path_buf()));
            }
            Ok(Some(fs::read(path)?))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn mutation(
    path: PathBuf,
    original: Option<Vec<u8>>,
    replacement: Option<Vec<u8>>,
    executable: bool,
) -> PlannedFileMutation {
    PlannedFileMutation {
        path,
        expected: original.map_or(FileExpectation::Absent, FileExpectation::Bytes),
        replacement,
        executable,
    }
}

fn mutation_is_noop(action: &PlannedFileMutation) -> bool {
    match (&action.expected, &action.replacement) {
        (FileExpectation::Absent, None) => true,
        (FileExpectation::Bytes(before), Some(after)) => before == after,
        _ => false,
    }
}

fn verify_expectation(action: &PlannedFileMutation) -> Result<(), ManagedHookError> {
    let actual = read_optional_bounded(&action.path)?;
    let matches = match (&action.expected, actual) {
        (FileExpectation::Absent, None) => true,
        (FileExpectation::Bytes(expected), Some(actual)) => expected == &actual,
        _ => false,
    };
    if !matches {
        return Err(ManagedHookError::PlanDrift(action.path.clone()));
    }
    Ok(())
}

fn apply_mutation(action: &PlannedFileMutation) -> Result<(), ManagedHookError> {
    match &action.replacement {
        Some(bytes) => atomic_write(&action.path, bytes, action.executable),
        None => match fs::remove_file(&action.path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        },
    }
}

fn rollback_mutation(action: &PlannedFileMutation) -> Result<(), ManagedHookError> {
    let actual = read_optional_bounded(&action.path)?;
    let still_ours = match (&action.replacement, actual) {
        (None, None) => true,
        (Some(expected), Some(actual)) => expected == &actual,
        _ => false,
    };
    if !still_ours {
        return Err(ManagedHookError::PlanDrift(action.path.clone()));
    }
    match &action.expected {
        FileExpectation::Absent => match fs::remove_file(&action.path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        },
        FileExpectation::Bytes(bytes) => atomic_write(&action.path, bytes, action.executable),
    }
}

fn ensure_generated_file_or_absent(
    path: &Path,
    bytes: Option<&[u8]>,
) -> Result<(), ManagedHookError> {
    if bytes.is_some_and(|bytes| !String::from_utf8_lossy(bytes).contains(MANAGED_MARKER)) {
        return Err(ManagedHookError::UnmanagedConflict(path.to_path_buf()));
    }
    Ok(())
}

fn atomic_write(path: &Path, bytes: &[u8], executable: bool) -> Result<(), ManagedHookError> {
    atomic_write_inner(path, bytes, executable, true)
}

fn atomic_write_ephemeral(path: &Path, bytes: &[u8]) -> Result<(), ManagedHookError> {
    atomic_write_inner(path, bytes, false, false)
}

fn atomic_write_inner(
    path: &Path,
    bytes: &[u8],
    executable: bool,
    keep_backup: bool,
) -> Result<(), ManagedHookError> {
    let parent = path
        .parent()
        .ok_or_else(|| ManagedHookError::InvalidDerivedPath(path.to_path_buf()))?;
    fs::create_dir_all(parent)?;
    let sequence = NEXT_TEMP_FILE.fetch_add(1, Ordering::Relaxed);
    let temp = parent.join(format!(".gate4agent-{}-{sequence}.tmp", std::process::id()));
    let result = (|| {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(
                &temp,
                fs::Permissions::from_mode(if executable { 0o700 } else { 0o600 }),
            )?;
        }
        #[cfg(not(unix))]
        let _ = executable;
        let backup = keep_backup.then(|| backup_path(path));
        if let Some(backup) = &backup {
            if fs::symlink_metadata(backup).is_ok_and(|metadata| metadata.file_type().is_symlink())
            {
                return Err(ManagedHookError::SymlinkPath(backup.clone()));
            }
            if path.exists() {
                fs::copy(path, backup)?;
            }
        }
        match fs::rename(&temp, path) {
            Ok(()) => Ok(()),
            Err(_) if path.exists() => {
                fs::remove_file(path)?;
                if let Err(error) = fs::rename(&temp, path) {
                    if backup.as_ref().is_some_and(|backup| backup.exists()) {
                        let _ = fs::copy(backup.as_ref().unwrap(), path);
                    }
                    return Err(error.into());
                }
                Ok(())
            }
            Err(error) => Err(error.into()),
        }
    })();
    if temp.exists() {
        let _ = fs::remove_file(&temp);
    }
    result
}

fn backup_path(path: &Path) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(".bak");
    PathBuf::from(value)
}

fn parse_json_config(
    spec: &ManagedHookAdapterSpec,
    bytes: &[u8],
) -> Result<Value, ManagedHookError> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| ManagedHookError::InvalidUtf8(PathBuf::from(spec.target)))?;
    let parsed: Value = serde_json::from_str(text)?;
    if !parsed.is_object() {
        return Err(ManagedHookError::ConfigRootMustBeObject(
            spec.target.to_owned(),
        ));
    }
    Ok(parsed)
}

fn apply_json_install(
    manager: &ManagedHookManager,
    spec: &ManagedHookAdapterSpec,
    config: &mut Value,
) -> Result<(), ManagedHookError> {
    let ManagedHookConfigKind::JsonHooks {
        container,
        require_version_one,
    } = spec.config_kind
    else {
        unreachable!()
    };
    let root = config
        .as_object_mut()
        .ok_or_else(|| ManagedHookError::ConfigRootMustBeObject(spec.target.to_owned()))?;
    let hook_value = root
        .entry(container.to_owned())
        .or_insert_with(|| Value::Object(Map::new()));
    let hook_map = hook_value
        .as_object_mut()
        .ok_or_else(|| ManagedHookError::HookContainerMustBeObject(spec.target.to_owned()))?;
    clean_managed_hook_map(spec, hook_map);
    for event in spec.events {
        let command = manager.managed_command(spec, event)?;
        let definitions = hook_map
            .entry(event.name.to_owned())
            .or_insert_with(|| Value::Array(Vec::new()));
        let definitions =
            definitions
                .as_array_mut()
                .ok_or_else(|| ManagedHookError::HookEventMustBeArray {
                    target: spec.target.to_owned(),
                    event: event.name.to_owned(),
                })?;
        let definition = build_definition(event, command);
        if spec.target == "codex" {
            // Pinned Orca runs status evidence before user hooks so a slow
            // user Stop/PostToolUse hook cannot leave the monitor stale.
            definitions.insert(0, definition);
        } else {
            definitions.push(definition);
        }
    }
    if require_version_one {
        root.insert("version".to_owned(), json!(1));
    }
    if spec.target == "codex" {
        let hooks = root.remove("hooks").unwrap_or_else(|| json!({}));
        root.clear();
        root.insert("hooks".to_owned(), hooks);
    }
    Ok(())
}

fn apply_json_remove(
    spec: &ManagedHookAdapterSpec,
    config: &mut Value,
) -> Result<(), ManagedHookError> {
    let ManagedHookConfigKind::JsonHooks { container, .. } = spec.config_kind else {
        unreachable!()
    };
    let root = config
        .as_object_mut()
        .ok_or_else(|| ManagedHookError::ConfigRootMustBeObject(spec.target.to_owned()))?;
    if let Some(hook_map) = root.get_mut(container).and_then(Value::as_object_mut) {
        clean_managed_hook_map(spec, hook_map);
    }
    Ok(())
}

fn clean_managed_hook_map(spec: &ManagedHookAdapterSpec, hook_map: &mut Map<String, Value>) {
    let event_names = hook_map.keys().cloned().collect::<Vec<_>>();
    for event_name in event_names {
        let Some(definitions) = hook_map.get(&event_name).and_then(Value::as_array) else {
            continue;
        };
        let cleaned = definitions
            .iter()
            .filter_map(|definition| clean_definition(spec, definition))
            .collect::<Vec<_>>();
        if cleaned.is_empty() {
            hook_map.remove(&event_name);
        } else {
            hook_map.insert(event_name, Value::Array(cleaned));
        }
    }
}

fn clean_definition(spec: &ManagedHookAdapterSpec, definition: &Value) -> Option<Value> {
    let mut object = definition.as_object()?.clone();
    for key in ["command", "bash", "powershell"] {
        if object
            .get(key)
            .and_then(Value::as_str)
            .is_some_and(|command| is_managed_command(spec, command))
        {
            object.remove(key);
        }
    }
    if let Some(hooks) = object.get("hooks").and_then(Value::as_array) {
        let filtered = hooks
            .iter()
            .filter(|hook| {
                !hook
                    .get("command")
                    .and_then(Value::as_str)
                    .is_some_and(|command| is_managed_command(spec, command))
            })
            .cloned()
            .collect::<Vec<_>>();
        if filtered.is_empty() {
            object.remove("hooks");
        } else {
            object.insert("hooks".to_owned(), Value::Array(filtered));
        }
    }
    let has_command = ["command", "bash", "powershell"]
        .iter()
        .any(|key| object.get(*key).and_then(Value::as_str).is_some())
        || object
            .get("hooks")
            .and_then(Value::as_array)
            .is_some_and(|hooks| !hooks.is_empty());
    has_command.then_some(Value::Object(object))
}

fn build_definition(event: &ManagedHookEventSpec, command: String) -> Value {
    match event.shape {
        ManagedHookEventShape::NestedCommand { matcher, timeout } => {
            let mut definition = Map::new();
            if let Some(matcher) = matcher {
                definition.insert("matcher".to_owned(), json!(matcher));
            }
            definition.insert(
                "hooks".to_owned(),
                json!([{
                    "type": "command",
                    "command": command,
                    "timeout": timeout,
                }]),
            );
            Value::Object(definition)
        }
        ManagedHookEventShape::DirectCommand { timeout } => json!({
            "type": "command",
            "command": command,
            "timeout": timeout,
        }),
    }
}

fn definition_commands(definition: &Value) -> Vec<&str> {
    let Some(object) = definition.as_object() else {
        return Vec::new();
    };
    let mut commands = ["command", "bash", "powershell"]
        .iter()
        .filter_map(|key| object.get(*key).and_then(Value::as_str))
        .collect::<Vec<_>>();
    if let Some(hooks) = object.get("hooks").and_then(Value::as_array) {
        commands.extend(
            hooks
                .iter()
                .filter_map(|hook| hook.get("command").and_then(Value::as_str)),
        );
    }
    commands
}

fn definition_has_exact_command(definition: &Value, expected: &str) -> bool {
    definition_commands(definition).contains(&expected)
}

fn codex_hooks_are_trusted(
    manager: &ManagedHookManager,
    spec: &ManagedHookAdapterSpec,
    config_path: &Path,
    config: &Value,
) -> Result<bool, ManagedHookError> {
    let trust_path = config_path
        .parent()
        .ok_or_else(|| ManagedHookError::InvalidDerivedPath(config_path.to_path_buf()))?
        .join("config.toml");
    let Some(bytes) = read_optional_bounded(&trust_path)? else {
        return Ok(false);
    };
    let text =
        String::from_utf8(bytes).map_err(|_| ManagedHookError::InvalidUtf8(trust_path.clone()))?;
    let keys = codex_managed_trust_keys(manager, spec, config_path, config)?;
    Ok(!keys.is_empty()
        && keys
            .iter()
            .all(|key| codex_trust_block_is_enabled(&text, key)))
}

fn codex_managed_trust_keys(
    manager: &ManagedHookManager,
    spec: &ManagedHookAdapterSpec,
    config_path: &Path,
    config: &Value,
) -> Result<Vec<String>, ManagedHookError> {
    let hook_map = config
        .get("hooks")
        .and_then(Value::as_object)
        .ok_or_else(|| ManagedHookError::HookContainerMustBeObject(spec.target.to_owned()))?;
    let source = fs::canonicalize(config_path)
        .unwrap_or_else(|_| config_path.to_path_buf())
        .to_string_lossy()
        .to_string();
    let mut keys = Vec::new();
    for event in spec.events {
        let command = manager.managed_command(spec, event)?;
        let Some((group_index, definition)) = hook_map
            .get(event.name)
            .and_then(Value::as_array)
            .and_then(|definitions| {
                definitions
                    .iter()
                    .enumerate()
                    .find(|(_, definition)| definition_has_exact_command(definition, &command))
            })
        else {
            continue;
        };
        let handler_index = definition
            .get("hooks")
            .and_then(Value::as_array)
            .and_then(|hooks| {
                hooks.iter().position(|hook| {
                    hook.get("command").and_then(Value::as_str) == Some(command.as_str())
                })
            })
            .unwrap_or(0);
        keys.push(format!(
            "{}:{}:{group_index}:{handler_index}",
            source,
            codex_event_label(event.name)
        ));
    }
    Ok(keys)
}

fn codex_event_label(event: &str) -> &str {
    match event {
        "SessionStart" => "session_start",
        "UserPromptSubmit" => "user_prompt_submit",
        "PreToolUse" => "pre_tool_use",
        "PermissionRequest" => "permission_request",
        "PostToolUse" => "post_tool_use",
        "Stop" => "stop",
        _ => event,
    }
}

fn normalized_trust_text(value: &str) -> String {
    value
        .replace("\\\\", "/")
        .replace('\\', "/")
        .to_ascii_lowercase()
}

fn codex_trust_block_is_enabled(text: &str, key: &str) -> bool {
    let key = normalized_trust_text(key);
    toml_table_blocks(text).into_iter().any(|block| {
        normalized_trust_text(block.header).contains(&key)
            && block
                .body
                .lines()
                .any(|line| line.trim() == "enabled = true")
            && block
                .body
                .lines()
                .any(|line| line.trim_start().starts_with("trusted_hash = \"sha256:"))
    })
}

fn plan_codex_trust_cleanup(
    manager: &ManagedHookManager,
    spec: &ManagedHookAdapterSpec,
    config_path: &Path,
    config: &Value,
) -> Result<Option<PlannedFileMutation>, ManagedHookError> {
    let trust_path = config_path
        .parent()
        .ok_or_else(|| ManagedHookError::InvalidDerivedPath(config_path.to_path_buf()))?
        .join("config.toml");
    let Some(original) = read_optional_bounded(&trust_path)? else {
        return Ok(None);
    };
    let text = String::from_utf8(original.clone())
        .map_err(|_| ManagedHookError::InvalidUtf8(trust_path.clone()))?;
    let keys = codex_managed_trust_keys(manager, spec, config_path, config)?;
    let next = remove_codex_trust_blocks(&text, &keys);
    Ok(
        (next != text)
            .then(|| mutation(trust_path, Some(original), Some(next.into_bytes()), false)),
    )
}

struct TomlTableBlock<'a> {
    header: &'a str,
    body: &'a str,
    start: usize,
    end: usize,
}

fn toml_table_blocks(text: &str) -> Vec<TomlTableBlock<'_>> {
    let starts = text
        .match_indices('[')
        .filter(|(index, _)| *index == 0 || text.as_bytes().get(index - 1) == Some(&b'\n'))
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    starts
        .iter()
        .enumerate()
        .filter_map(|(position, start)| {
            let end = starts.get(position + 1).copied().unwrap_or(text.len());
            let block = &text[*start..end];
            let header_end = block.find('\n').unwrap_or(block.len());
            let header = &block[..header_end];
            header
                .starts_with("[hooks.state.")
                .then_some(TomlTableBlock {
                    header,
                    body: &block[header_end..],
                    start: *start,
                    end,
                })
        })
        .collect()
}

fn remove_codex_trust_blocks(text: &str, keys: &[String]) -> String {
    let normalized_keys = keys
        .iter()
        .map(|key| normalized_trust_text(key))
        .collect::<Vec<_>>();
    let ranges = toml_table_blocks(text)
        .into_iter()
        .filter(|block| {
            let header = normalized_trust_text(block.header);
            normalized_keys.iter().any(|key| header.contains(key))
        })
        .map(|block| (block.start, block.end))
        .collect::<Vec<_>>();
    if ranges.is_empty() {
        return text.to_owned();
    }
    let mut next = String::with_capacity(text.len());
    let mut cursor = 0;
    for (start, end) in ranges {
        next.push_str(&text[cursor..start]);
        cursor = end;
    }
    next.push_str(&text[cursor..]);
    while next.contains("\n\n\n") {
        next = next.replace("\n\n\n", "\n\n");
    }
    next
}

fn is_managed_command(spec: &ManagedHookAdapterSpec, command: &str) -> bool {
    if command_names_managed_script(spec, command) {
        return true;
    }
    // Windows hook commands wrap the real command as a PowerShell
    // `-EncodedCommand` base64/UTF-16LE blob, so the managed script path
    // never appears as plaintext in `command`. Decode that form the same
    // way `base64_utf16le` produced it and re-run the same substring
    // check against the decoded text.
    decode_encoded_command_argument(command)
        .is_some_and(|decoded| command_names_managed_script(spec, &decoded))
}

fn command_names_managed_script(spec: &ManagedHookAdapterSpec, command: &str) -> bool {
    let normalized = command.replace('\\', "/").to_ascii_lowercase();
    ["cmd", "ps1", "sh"].iter().any(|extension| {
        normalized.contains(&format!(
            ".gate4agent/agent-hooks/{}.{}",
            spec.script_stem, extension
        ))
    })
}

/// Extracts the token following a PowerShell `-EncodedCommand` argument and
/// decodes it as base64 -> UTF-16LE, mirroring `base64_utf16le` in reverse.
/// Any malformed input (bad base64, odd byte count, invalid UTF-16) yields
/// `None` rather than panicking.
fn decode_encoded_command_argument(command: &str) -> Option<String> {
    const MARKER: &str = "-encodedcommand";
    let marker_start = command.to_ascii_lowercase().find(MARKER)?;
    let token = command[marker_start + MARKER.len()..]
        .trim_start()
        .split_whitespace()
        .next()?
        .trim_matches(|character| character == '"' || character == '\'');
    let bytes = base64_decode(token)?;
    if bytes.is_empty() || bytes.len() % 2 != 0 {
        return None;
    }
    let units = bytes
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect::<Vec<_>>();
    String::from_utf16(&units).ok()
}

fn managed_script(platform: RuntimePlatform, target: &str) -> String {
    if platform == RuntimePlatform::Windows && target != "kimi" {
        return managed_cmd_script(target);
    }
    managed_posix_script(target)
}

fn managed_posix_script(target: &str) -> String {
    let skip_devin = if target == "claude" {
        "if [ -n \"$DEVIN_PROJECT_DIR\" ]; then cat >/dev/null; exit 0; fi\n"
    } else {
        ""
    };
    format!(
        "#!/bin/sh\n# {MANAGED_MARKER}\n{skip_devin}payload=$(cat)\nif [ -z \"$GATE4AGENT_HOOK_URL\" ] || [ -z \"$GATE4AGENT_HOOK_TOKEN\" ] || [ -z \"$GATE4AGENT_HOOK_ROUTE\" ]; then exit 0; fi\nprintf '%s' \"$payload\" | curl -sS -X POST \"$GATE4AGENT_HOOK_URL\" --connect-timeout 0.5 --max-time 1.5 -H \"Content-Type: application/x-www-form-urlencoded\" -H \"x-gate4agent-hook-token: $GATE4AGENT_HOOK_TOKEN\" -H \"x-gate4agent-hook-route: $GATE4AGENT_HOOK_ROUTE\" --data-urlencode \"event_name=$GATE4AGENT_HOOK_EVENT\" --data-urlencode \"payload@-\" >/dev/null 2>&1 || true\nexit 0\n"
    )
}

fn managed_cmd_script(target: &str) -> String {
    let skip_devin = if target == "claude" {
        "if not \"%DEVIN_PROJECT_DIR%\"==\"\" goto :drain\r\n"
    } else {
        ""
    };
    format!(
        "@echo off\r\nrem {MANAGED_MARKER}\r\nsetlocal\r\n{skip_devin}if \"%GATE4AGENT_HOOK_URL%\"==\"\" goto :drain\r\nif \"%GATE4AGENT_HOOK_TOKEN%\"==\"\" goto :drain\r\nif \"%GATE4AGENT_HOOK_ROUTE%\"==\"\" goto :drain\r\n\"%SystemRoot%\\System32\\curl.exe\" -sS -X POST \"%GATE4AGENT_HOOK_URL%\" --connect-timeout 0.5 --max-time 1.5 -H \"Content-Type: application/x-www-form-urlencoded\" -H \"x-gate4agent-hook-token: %GATE4AGENT_HOOK_TOKEN%\" -H \"x-gate4agent-hook-route: %GATE4AGENT_HOOK_ROUTE%\" --data-urlencode \"event_name=%GATE4AGENT_HOOK_EVENT%\" --data-urlencode \"payload@-\" >nul 2>nul\r\nexit /b 0\r\n:drain\r\nrem Match POSIX: missing ingress env => exit 0. Do not spawn a console helper.\r\nexit /b 0\r\n"
    )
}

fn posix_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn powershell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn base64_utf16le(value: &str) -> String {
    let bytes = value
        .encode_utf16()
        .flat_map(u16::to_le_bytes)
        .collect::<Vec<_>>();
    base64(&bytes)
}

fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let bits = ((chunk[0] as u32) << 16)
            | ((chunk.get(1).copied().unwrap_or(0) as u32) << 8)
            | chunk.get(2).copied().unwrap_or(0) as u32;
        output.push(TABLE[((bits >> 18) & 63) as usize] as char);
        output.push(TABLE[((bits >> 12) & 63) as usize] as char);
        output.push(if chunk.len() > 1 {
            TABLE[((bits >> 6) & 63) as usize] as char
        } else {
            '='
        });
        output.push(if chunk.len() > 2 {
            TABLE[(bits & 63) as usize] as char
        } else {
            '='
        });
    }
    output
}

/// Decodes the alphabet produced by `base64`. Returns `None` for anything
/// that is not well-formed base64 (wrong length, stray padding, characters
/// outside the alphabet) instead of panicking.
fn base64_decode(value: &str) -> Option<Vec<u8>> {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    if value.is_empty() || value.len() % 4 != 0 {
        return None;
    }
    let body = value.trim_end_matches('=');
    if body.is_empty() {
        return None;
    }
    let mut bits: u32 = 0;
    let mut bit_count: u32 = 0;
    let mut output = Vec::with_capacity(value.len() / 4 * 3);
    for symbol in body.bytes() {
        let sextet = TABLE.iter().position(|&candidate| candidate == symbol)? as u32;
        bits = (bits << 6) | sextet;
        bit_count += 6;
        if bit_count >= 8 {
            bit_count -= 8;
            output.push(((bits >> bit_count) & 0xFF) as u8);
        }
    }
    Some(output)
}

fn not_installed(spec: &ManagedHookAdapterSpec, path: PathBuf) -> ManagedHookStatus {
    ManagedHookStatus {
        target: spec.target.to_owned(),
        state: ManagedHookState::NotInstalled,
        config_path: path,
        managed_hooks_present: false,
        detail: None,
    }
}

fn conflict(spec: &ManagedHookAdapterSpec, path: PathBuf, detail: &str) -> ManagedHookStatus {
    ManagedHookStatus {
        target: spec.target.to_owned(),
        state: ManagedHookState::Conflict,
        config_path: path,
        managed_hooks_present: false,
        detail: Some(detail.to_owned()),
    }
}

fn amp_plugin_source() -> String {
    format!(
        r#"import type {{ PluginAPI }} from '@ampcode/plugin'

// {MANAGED_MARKER}
function previewValue(value: unknown, maxLength = 4000): string | undefined {{
  if (typeof value === 'string') return value.slice(0, maxLength)
  if (value === null || value === undefined) return undefined
  try {{
    return JSON.stringify(value).slice(0, maxLength)
  }} catch {{
    return String(value).slice(0, maxLength)
  }}
}}

function jsonSafe(value: unknown, depth = 0): unknown {{
  if (value === null || value === undefined) return value
  if (typeof value === 'string' || typeof value === 'number' || typeof value === 'boolean') return value
  if (typeof value === 'bigint' || typeof value === 'symbol' || typeof value === 'function') return String(value)
  if (depth >= 4) return previewValue(value)
  if (Array.isArray(value)) return value.slice(0, 20).map((item) => jsonSafe(item, depth + 1))
  if (typeof value === 'object') {{
    const out: Record<string, unknown> = {{}}
    for (const [key, child] of Object.entries(value).slice(0, 20)) {{
      out[key] = jsonSafe(child, depth + 1)
    }}
    return out
  }}
  return String(value)
}}

async function post(event_name: string, payload: Record<string, unknown>): Promise<void> {{
  const url = process.env.GATE4AGENT_HOOK_URL
  const token = process.env.GATE4AGENT_HOOK_TOKEN
  const route = process.env.GATE4AGENT_HOOK_ROUTE
  if (!url || !token || !route) return
  const controller = new AbortController()
  const timeout = setTimeout(() => controller.abort(), 1000)
  try {{
    await fetch(url, {{ method: 'POST', signal: controller.signal, headers: {{
      'Content-Type': 'application/json',
      'x-gate4agent-hook-token': token,
      'x-gate4agent-hook-route': route,
    }}, body: JSON.stringify({{ event_name, payload: {{ event_name, ...payload }} }}) }})
  }} catch {{}} finally {{ clearTimeout(timeout) }}
}}

const MAX_PENDING_POSTS = 50
type QueuedPost = {{ eventName: string; payload: Record<string, unknown> }}
let postQueue: QueuedPost[] = []
let postDraining = false

async function drainPostQueue(): Promise<void> {{
  if (postDraining) return
  postDraining = true
  try {{
    while (postQueue.length > 0) {{
      const next = postQueue.shift()
      if (next) await post(next.eventName, next.payload)
    }}
  }} finally {{
    postDraining = false
    if (postQueue.length > 0) void drainPostQueue()
  }}
}}

function enqueuePost(eventName: string, payload: Record<string, unknown>): void {{
  if (postQueue.length >= MAX_PENDING_POSTS) postQueue.shift()
  postQueue.push({{ eventName, payload }})
  void drainPostQueue()
}}

export default function (amp: PluginAPI) {{
  amp.on('session.start', (event) => {{ enqueuePost('session.start', {{ threadId: event.thread.id }}) }})
  amp.on('agent.start', (event) => {{ enqueuePost('agent.start', {{ threadId: event.thread.id, id: event.id, message: event.message }}) }})
  amp.on('tool.call', (event) => {{ enqueuePost('tool.call', {{ threadId: event.thread.id, toolUseId: event.toolUseID, tool: event.tool, input: jsonSafe(event.input) }}); return {{ action: 'allow' }} }})
  amp.on('tool.result', (event) => {{ enqueuePost('tool.result', {{ threadId: event.thread.id, toolUseId: event.toolUseID, tool: event.tool, input: jsonSafe(event.input), status: event.status, error: event.error, output: previewValue(event.output) }}) }})
  amp.on('agent.end', (event) => {{ enqueuePost('agent.end', {{ threadId: event.thread.id, id: event.id, message: event.message, status: event.status }}) }})
}}
"#
    )
}

fn kimi_managed_block(text: &str) -> Option<&str> {
    let start = text.find(KIMI_BLOCK_START)?;
    let end = text[start..]
        .find(KIMI_BLOCK_END)
        .map(|relative| start + relative + KIMI_BLOCK_END.len())
        .unwrap_or(text.len());
    Some(&text[start..end])
}

fn strip_kimi_managed_block(text: &str) -> String {
    let Some(start) = text.find(KIMI_BLOCK_START) else {
        return text.to_owned();
    };
    let end = text[start..]
        .find(KIMI_BLOCK_END)
        .map(|relative| start + relative + KIMI_BLOCK_END.len())
        .unwrap_or(text.len());
    let mut result = format!("{}{}", &text[..start], &text[end..]);
    while result.contains("\n\n\n") {
        result = result.replace("\n\n\n", "\n\n");
    }
    let trimmed = result.trim_end();
    if trimmed.is_empty() {
        String::new()
    } else {
        format!("{trimmed}\n")
    }
}

fn toml_escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t")
}

#[derive(Debug, Error)]
pub enum ManagedHookError {
    #[error(transparent)]
    Adapter(#[from] ManagedHookAdapterError),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("managed Hook root must be absolute: {0}")]
    RootMustBeAbsolute(PathBuf),
    #[error("Windows managed Hook plans require an absolute system root")]
    MissingWindowsSystemRoot,
    #[error("Windows system root is unsafe to embed in a provider command")]
    UnsafeWindowsSystemRoot,
    #[error("Windows app-data-relative managed Hook configuration requires an explicit app-data root")]
    MissingAppData,
    #[error("invalid managed Hook relative path: {0}")]
    InvalidRelativePath(PathBuf),
    #[error("invalid derived managed Hook path: {0}")]
    InvalidDerivedPath(PathBuf),
    #[error("managed Hook target is unavailable: {0}")]
    UnknownTarget(String),
    #[error("managed Hook path is not a regular file: {0}")]
    NotARegularFile(PathBuf),
    #[error("managed Hook paths may not be symbolic links: {0}")]
    SymlinkPath(PathBuf),
    #[error("managed Hook file exceeds the bounded read limit: {0}")]
    FileTooLarge(PathBuf),
    #[error("managed Hook file is not UTF-8: {0}")]
    InvalidUtf8(PathBuf),
    #[error("managed Hook config root must be an object: {0}")]
    ConfigRootMustBeObject(String),
    #[error("managed Hook container must be an object: {0}")]
    HookContainerMustBeObject(String),
    #[error("managed Hook event bucket must be an array for {target}/{event}")]
    HookEventMustBeArray { target: String, event: String },
    #[error("managed Hook plan exceeds action bound: {0}")]
    PlanTooLarge(usize),
    #[error("managed Hook plan is stale because this file changed: {0}")]
    PlanDrift(PathBuf),
    #[error("managed Hook apply failed ({apply}) and rollback failed ({rollback})")]
    ApplyRollbackFailed { apply: String, rollback: String },
    #[error("refusing to overwrite or remove an unmanaged provider file: {0}")]
    UnmanagedConflict(PathBuf),
}

#[cfg(test)]
mod tests {
    use super::*;
    use gate4agent_adapters::builtin_adapter_registry;
    use gate4agent_types::AdapterFamily;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TestRoot(PathBuf);

    impl TestRoot {
        fn new(name: &str) -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "gate4agent-managed-hooks-{name}-{}-{nonce}",
                std::process::id()
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn manager(&self) -> ManagedHookManager {
            self.manager_for(RuntimePlatform::Linux)
        }

        fn manager_for(&self, platform: RuntimePlatform) -> ManagedHookManager {
            #[cfg(target_os = "windows")]
            let windows_root = PathBuf::from(r"C:\Windows");
            #[cfg(not(target_os = "windows"))]
            let windows_root = PathBuf::from("/windows");
            ManagedHookManager::new(ManagedHookRoots {
                home: self.0.join("home"),
                runtime_data: self.0.join("runtime"),
                app_data: Some(self.0.join("app-data")),
                platform,
                system_root: (platform == RuntimePlatform::Windows).then_some(windows_root),
                environment_homes: BTreeMap::new(),
            })
            .unwrap()
        }
    }

    impl Drop for TestRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn binding(target: &str) -> AdapterBinding {
        builtin_adapter_registry()
            .binding(AdapterFamily::ManagedHook, target)
            .unwrap()
            .clone()
    }

    #[test]
    fn all_pinned_targets_round_trip_through_explicit_plans() {
        let root = TestRoot::new("round-trip");
        let manager = root.manager();
        for target in ["claude", "codex", "grok", "kimi"] {
            let binding = binding(target);
            assert_eq!(
                manager.status(&binding).unwrap().state,
                ManagedHookState::NotInstalled,
                "initial status for {target}"
            );

            let install = manager
                .plan(&binding, ManagedHookOperation::Install)
                .unwrap();
            assert!(!install.is_noop(), "install plan for {target}");
            let expected_installed = if target == "codex" {
                ManagedHookState::ApprovalRequired
            } else {
                ManagedHookState::Installed
            };
            assert_eq!(
                manager.apply(install).unwrap().state,
                expected_installed,
                "installed status for {target}"
            );
            assert!(
                manager
                    .plan(&binding, ManagedHookOperation::Install)
                    .unwrap()
                    .is_noop(),
                "idempotent install for {target}"
            );

            let remove = manager
                .plan(&binding, ManagedHookOperation::Remove)
                .unwrap();
            assert!(!remove.is_noop(), "remove plan for {target}");
            assert_eq!(
                manager.apply(remove).unwrap().state,
                ManagedHookState::NotInstalled,
                "removed status for {target}"
            );
        }
    }

    #[test]
    fn construction_status_and_planning_are_side_effect_free() {
        let root = TestRoot::new("side-effect-free");
        let manager = root.manager();
        let home = root.0.join("home");
        let binding = binding("claude");
        assert_eq!(
            manager.status(&binding).unwrap().state,
            ManagedHookState::NotInstalled
        );
        let plan = manager
            .plan(&binding, ManagedHookOperation::Install)
            .unwrap();
        assert!(!plan.is_noop());
        assert!(!home.exists());
    }

    #[test]
    fn json_install_and_remove_preserve_user_hooks_and_top_level_fields() {
        let root = TestRoot::new("json-preserve");
        let manager = root.manager();
        let path = root.0.join("home/.claude/settings.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            r#"{
  "theme": "dark",
  "hooks": {
    "PreToolUse": [{"matcher":"Bash","hooks":[{"type":"command","command":"user-hook"}]}],
    "Custom": [{"command":"custom-hook"}]
  }
}
"#,
        )
        .unwrap();
        let binding = binding("claude");
        let installed = manager
            .apply(
                manager
                    .plan(&binding, ManagedHookOperation::Install)
                    .unwrap(),
            )
            .unwrap();
        assert_eq!(installed.state, ManagedHookState::Installed);
        let removed = manager
            .apply(
                manager
                    .plan(&binding, ManagedHookOperation::Remove)
                    .unwrap(),
            )
            .unwrap();
        assert_eq!(removed.state, ManagedHookState::NotInstalled);
        let config: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        assert_eq!(config["theme"], "dark");
        assert_eq!(
            config["hooks"]["PreToolUse"][0]["hooks"][0]["command"],
            "user-hook"
        );
        assert_eq!(config["hooks"]["Custom"][0]["command"], "custom-hook");
    }

    #[test]
    fn apply_rejects_config_drift_without_overwriting_it() {
        let root = TestRoot::new("drift");
        let manager = root.manager();
        let binding = binding("claude");
        let plan = manager
            .plan(&binding, ManagedHookOperation::Install)
            .unwrap();
        let config = root.0.join("home/.claude/settings.json");
        fs::create_dir_all(config.parent().unwrap()).unwrap();
        fs::write(&config, b"{\"user\":true}\n").unwrap();
        assert!(matches!(
            manager.apply(plan),
            Err(ManagedHookError::PlanDrift(path)) if path == config
        ));
        assert_eq!(fs::read(&config).unwrap(), b"{\"user\":true}\n");
        assert!(!root
            .0
            .join("home/.gate4agent/agent-hooks/claude-hook.sh")
            .exists());
    }

    #[test]
    fn unmanaged_generated_paths_are_never_overwritten_or_removed() {
        let root = TestRoot::new("owned-file-conflict");
        let manager = root.manager();
        let script = root.0.join("home/.gate4agent/agent-hooks/claude-hook.sh");
        fs::create_dir_all(script.parent().unwrap()).unwrap();
        fs::write(&script, b"#!/bin/sh\necho user\n").unwrap();
        assert!(matches!(
            manager.plan(&binding("claude"), ManagedHookOperation::Install),
            Err(ManagedHookError::UnmanagedConflict(path)) if path == script
        ));

        // The whole-file-ownership conflict shape below (`amp`'s TS plugin:
        // an unmanaged file entirely occupies the config path, reported as
        // `ManagedHookState::Conflict`) has no fleet-relevant example left:
        // `AmpPlugin` is the only config kind that ever produces that state,
        // and no fleet member uses it -- the fleet's JsonHooks and KimiToml
        // kinds both merge in place instead of claiming a whole file, so
        // `status_amp`'s `conflict()` call is unreachable from any declared
        // fleet spec.
    }

    #[test]
    fn json_hooks_fail_closed_on_malformed_json() {
        let root = TestRoot::new("json-malformed");
        let manager = root.manager();
        let broken = root.0.join("home/.codex/hooks.json");
        fs::create_dir_all(broken.parent().unwrap()).unwrap();
        fs::write(&broken, b"{broken").unwrap();
        assert!(matches!(
            manager.plan(&binding("codex"), ManagedHookOperation::Install),
            Err(ManagedHookError::Json(_))
        ));
        assert_eq!(fs::read(broken).unwrap(), b"{broken");
    }

    #[test]
    fn codex_trust_remains_provider_approved_and_exact_entries_are_reversible() {
        let root = TestRoot::new("codex-trust");
        let manager = root.manager();
        let binding = binding("codex");
        assert_eq!(
            manager
                .apply(
                    manager
                        .plan(&binding, ManagedHookOperation::Install)
                        .unwrap(),
                )
                .unwrap()
                .state,
            ManagedHookState::ApprovalRequired
        );

        let config_path = root.0.join("home/.codex/hooks.json");
        let config: Value = serde_json::from_slice(&fs::read(&config_path).unwrap()).unwrap();
        let spec = managed_hook_spec(&binding).unwrap();
        let keys = codex_managed_trust_keys(&manager, spec, &config_path, &config).unwrap();
        assert_eq!(keys.len(), spec.events.len());
        let trust_path = root.0.join("home/.codex/config.toml");
        let mut trust = "model = \"gpt-test\"\n\n[hooks.state.\"unrelated:stop:0:0\"]\nenabled = true\ntrusted_hash = \"sha256:unrelated\"\n\n".to_owned();
        for key in &keys {
            trust.push_str(&format!(
                "[hooks.state.\"{}\"]\nenabled = true\ntrusted_hash = \"sha256:test\"\n\n",
                key.replace('\\', "\\\\").replace('"', "\\\"")
            ));
        }
        fs::write(&trust_path, trust).unwrap();
        assert_eq!(
            manager.status(&binding).unwrap().state,
            ManagedHookState::Installed
        );

        manager
            .apply(
                manager
                    .plan(&binding, ManagedHookOperation::Remove)
                    .unwrap(),
            )
            .unwrap();
        let remaining = fs::read_to_string(trust_path).unwrap();
        assert!(remaining.contains("unrelated:stop:0:0"));
        for key in &keys {
            assert!(!normalized_trust_text(&remaining).contains(&normalized_trust_text(key)));
        }
    }

    #[test]
    fn generated_scripts_keep_provider_specific_safety_contracts() {
        // The other non-fleet-specific script bodies this test used to also
        // assert on were removed along with those vendors; `amp`'s TS plugin
        // body still has no fleet-relevant example -- `claude` is the one
        // fleet-relevant example this test carries.
        let root = TestRoot::new("script-contracts");
        let manager = root.manager();
        for target in ["claude"] {
            let target_binding = binding(target);
            manager
                .apply(
                    manager
                        .plan(&target_binding, ManagedHookOperation::Install)
                        .unwrap(),
                )
                .unwrap();
        }

        let claude =
            fs::read_to_string(root.0.join("home/.gate4agent/agent-hooks/claude-hook.sh")).unwrap();
        assert!(claude.contains("DEVIN_PROJECT_DIR"));
        assert!(claude.contains("cat >/dev/null"));
    }

    #[test]
    fn windows_plans_round_trip_all_targets_without_embedding_authority() {
        let root = TestRoot::new("windows-round-trip");
        let manager = root.manager_for(RuntimePlatform::Windows);
        let mut claude_script = String::new();
        for target in ["claude", "codex", "grok", "kimi"] {
            let target_binding = binding(target);
            let status = manager
                .apply(
                    manager
                        .plan(&target_binding, ManagedHookOperation::Install)
                        .unwrap(),
                )
                .unwrap();
            assert_eq!(
                status.state,
                if target == "codex" {
                    ManagedHookState::ApprovalRequired
                } else {
                    ManagedHookState::Installed
                },
                "Windows install status for {target}"
            );
            if target == "claude" {
                claude_script = fs::read_to_string(
                    root.0
                        .join("home/.gate4agent/agent-hooks/claude-hook.cmd"),
                )
                .unwrap();
            }
            manager
                .apply(
                    manager
                        .plan(&target_binding, ManagedHookOperation::Remove)
                        .unwrap(),
                )
                .unwrap();
        }
        // The `endpoint.cmd`-recovery safety contract asserted here before
        // was specific to a now-removed non-fleet vendor. `claude`'s own
        // Windows-script safety contract (skipping Devin's own hook
        // re-entry) remains fleet-relevant.
        assert!(claude_script.contains("%DEVIN_PROJECT_DIR%"));
        assert!(claude_script.contains("goto :drain"));
        assert!(
            !claude_script.contains("more >nul"),
            "Windows drain must not spawn more.com (visible conhost)"
        );
        assert!(!claude_script.contains("x-gate4agent-hook-token: 00000000"));
        let spec = managed_hook_spec(&binding("claude")).unwrap();
        let event = spec
            .events
            .iter()
            .find(|event| event.name == "PreToolUse")
            .unwrap();
        let pre_tool = manager.managed_command(spec, event).unwrap();
        assert!(
            pre_tool.contains("-WindowStyle Hidden"),
            "native CLIs spawn this command without CREATE_NO_WINDOW"
        );
        assert!(pre_tool.contains("-NonInteractive"));
    }

    #[test]
    fn all_pinned_targets_round_trip_through_explicit_plans_on_windows() {
        let root = TestRoot::new("round-trip-windows");
        let manager = root.manager_for(RuntimePlatform::Windows);
        for target in ["claude", "codex", "grok", "kimi"] {
            let binding = binding(target);
            assert_eq!(
                manager.status(&binding).unwrap().state,
                ManagedHookState::NotInstalled,
                "initial status for {target}"
            );

            let install = manager
                .plan(&binding, ManagedHookOperation::Install)
                .unwrap();
            assert!(!install.is_noop(), "install plan for {target}");
            let expected_installed = if target == "codex" {
                ManagedHookState::ApprovalRequired
            } else {
                ManagedHookState::Installed
            };
            assert_eq!(
                manager.apply(install).unwrap().state,
                expected_installed,
                "installed status for {target}"
            );
            assert!(
                manager
                    .plan(&binding, ManagedHookOperation::Install)
                    .unwrap()
                    .is_noop(),
                "idempotent Windows install for {target}"
            );

            let remove = manager
                .plan(&binding, ManagedHookOperation::Remove)
                .unwrap();
            assert!(!remove.is_noop(), "remove plan for {target}");
            assert_eq!(
                manager.apply(remove).unwrap().state,
                ManagedHookState::NotInstalled,
                "removed status for {target}"
            );
        }
    }

    #[test]
    fn windows_encoded_duplicates_collapse_to_one_on_reinstall() {
        let root = TestRoot::new("windows-encoded-duplicates");
        let manager = root.manager_for(RuntimePlatform::Windows);
        let target_binding = binding("codex");
        let spec = managed_hook_spec(&target_binding).unwrap();
        let event = spec
            .events
            .iter()
            .find(|event| event.name == "PreToolUse")
            .unwrap();
        let managed_command = manager.managed_command(spec, event).unwrap();
        let config_path = manager.config_path(spec).unwrap();
        fs::create_dir_all(config_path.parent().unwrap()).unwrap();

        // Reproduces the observed defect on disk: seven identical copies of
        // the same Windows-encoded managed hook accumulated in one event,
        // the way `apply_json_install` used to append without ever matching
        // `is_managed_command` against its own prior writes.
        let duplicate = json!({
            "hooks": [{"type": "command", "command": managed_command, "timeout": 10}],
        });
        let config = json!({
            "hooks": {
                "PreToolUse": vec![duplicate; 7],
            },
        });
        fs::write(
            &config_path,
            format!("{}\n", serde_json::to_string_pretty(&config).unwrap()),
        )
        .unwrap();

        manager
            .apply(
                manager
                    .plan(&target_binding, ManagedHookOperation::Install)
                    .unwrap(),
            )
            .unwrap();

        let installed: Value = serde_json::from_slice(&fs::read(&config_path).unwrap()).unwrap();
        let pre_tool_use = installed["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(
            pre_tool_use
                .iter()
                .filter(|definition| definition_has_exact_command(definition, &managed_command))
                .count(),
            1,
            "seven accumulated copies of the managed PreToolUse hook must collapse to one"
        );
        for definitions in installed["hooks"].as_object().unwrap().values() {
            assert_eq!(
                definitions.as_array().unwrap().len(),
                1,
                "every codex event keeps exactly one managed definition after reinstall"
            );
        }
    }

    #[test]
    fn windows_install_removes_only_managed_entries_and_keeps_foreign_ones() {
        let root = TestRoot::new("windows-foreign-survives");
        let manager = root.manager_for(RuntimePlatform::Windows);
        let target_binding = binding("claude");
        let spec = managed_hook_spec(&target_binding).unwrap();
        let event = spec
            .events
            .iter()
            .find(|event| event.name == "PreToolUse")
            .unwrap();
        let managed_command = manager.managed_command(spec, event).unwrap();
        let config_path = manager.config_path(spec).unwrap();
        fs::create_dir_all(config_path.parent().unwrap()).unwrap();

        let managed_definition = json!({
            "matcher": "*",
            "hooks": [{"type": "command", "command": managed_command, "timeout": 10}],
        });
        let foreign_definition = json!({
            "matcher": "*",
            "hooks": [{"type": "command", "command": "C:/Users/owner/own-hook.ps1"}],
        });
        let mut pre_tool_use = vec![managed_definition; 3];
        pre_tool_use.push(foreign_definition.clone());
        let config = json!({ "hooks": { "PreToolUse": pre_tool_use } });
        fs::write(
            &config_path,
            format!("{}\n", serde_json::to_string_pretty(&config).unwrap()),
        )
        .unwrap();

        manager
            .apply(
                manager
                    .plan(&target_binding, ManagedHookOperation::Install)
                    .unwrap(),
            )
            .unwrap();

        let installed: Value = serde_json::from_slice(&fs::read(&config_path).unwrap()).unwrap();
        let pre_tool_use = installed["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(
            pre_tool_use
                .iter()
                .filter(|definition| definition_has_exact_command(definition, &managed_command))
                .count(),
            1,
            "duplicate managed PreToolUse entries collapse to one"
        );
        assert!(
            pre_tool_use
                .iter()
                .any(|definition| *definition == foreign_definition),
            "the owner's own PreToolUse hook must survive install untouched"
        );
    }

    #[test]
    fn is_managed_command_decodes_windows_encoded_form_and_tolerates_malformed_input() {
        let root = TestRoot::new("encoded-command-decode");
        let manager = root.manager_for(RuntimePlatform::Windows);
        let target_binding = binding("claude");
        let spec = managed_hook_spec(&target_binding).unwrap();
        let event = spec
            .events
            .iter()
            .find(|event| event.name == "PreToolUse")
            .unwrap();
        let managed_command = manager.managed_command(spec, event).unwrap();
        assert!(is_managed_command(spec, &managed_command));

        assert!(!is_managed_command(
            spec,
            "powershell -NoProfile -ExecutionPolicy Bypass -EncodedCommand not-valid-base64!!"
        ));
        assert!(!is_managed_command(
            spec,
            "powershell -NoProfile -ExecutionPolicy Bypass -EncodedCommand QQ=="
        ));
        let lone_surrogate = base64(&[0x00, 0xD8]);
        assert!(!is_managed_command(
            spec,
            &format!("powershell -EncodedCommand {lone_surrogate}")
        ));
        assert!(!is_managed_command(
            spec,
            "powershell -NoProfile -ExecutionPolicy Bypass -EncodedCommand"
        ));
    }
}
