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
const HERMES_PLUGIN_NAME: &str = "gate4agent-status";
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
            ManagedHookConfigKind::HermesPlugin => self.plan_hermes(spec, operation)?,
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
        } else if self.roots.platform == RuntimePlatform::Windows && spec.target == "copilot" {
            "ps1"
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
        let endpoint_paths = self.endpoint_paths()?;
        Ok(managed_script(
            self.roots.platform,
            spec.target,
            &endpoint_paths,
        ))
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
                "{} -NoProfile -ExecutionPolicy Bypass -EncodedCommand {}",
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
            ManagedHookConfigKind::HermesPlugin => self.status_hermes(spec),
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
        let definitions_complete = present == spec.events.len() && !json_disabled(spec, &config);
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
            ManagedHookState::Partial if json_disabled(spec, &config) => {
                Some("provider configuration disables managed hooks".to_owned())
            }
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

    fn hermes_paths(
        &self,
        spec: &ManagedHookAdapterSpec,
    ) -> Result<(PathBuf, PathBuf, PathBuf), ManagedHookError> {
        let config = self.config_path(spec)?;
        let home = config
            .parent()
            .ok_or_else(|| ManagedHookError::InvalidDerivedPath(config.clone()))?;
        let plugin_dir = checked_join(home, &format!("plugins/{HERMES_PLUGIN_NAME}"))?;
        Ok((
            config,
            plugin_dir.join("plugin.yaml"),
            plugin_dir.join("__init__.py"),
        ))
    }

    fn status_hermes(
        &self,
        spec: &ManagedHookAdapterSpec,
    ) -> Result<ManagedHookStatus, ManagedHookError> {
        let (config, manifest, init) = self.hermes_paths(spec)?;
        let config_text = read_optional_bounded(&config)?
            .map(|bytes| {
                String::from_utf8(bytes).map_err(|_| ManagedHookError::InvalidUtf8(config.clone()))
            })
            .transpose()?
            .unwrap_or_default();
        let yaml = inspect_hermes_yaml(&config_text)?;
        let manifest_managed = read_optional_bounded(&manifest)?
            .is_some_and(|bytes| String::from_utf8_lossy(&bytes).contains(MANAGED_MARKER));
        let init_managed = read_optional_bounded(&init)?
            .is_some_and(|bytes| String::from_utf8_lossy(&bytes).contains(MANAGED_MARKER));
        let managed = manifest_managed && init_managed;
        let state = if managed && yaml.enabled && !yaml.disabled {
            ManagedHookState::Installed
        } else if !managed && !yaml.enabled {
            ManagedHookState::NotInstalled
        } else {
            ManagedHookState::Partial
        };
        Ok(ManagedHookStatus {
            target: spec.target.to_owned(),
            state,
            config_path: config,
            managed_hooks_present: managed,
            detail: (state == ManagedHookState::Partial)
                .then(|| "Hermes plugin files or YAML enablement are incomplete".to_owned()),
        })
    }

    fn plan_hermes(
        &self,
        spec: &ManagedHookAdapterSpec,
        operation: ManagedHookOperation,
    ) -> Result<Vec<PlannedFileMutation>, ManagedHookError> {
        let (config, manifest, init) = self.hermes_paths(spec)?;
        let config_original = read_optional_bounded(&config)?;
        let config_text = config_original
            .as_ref()
            .map(|bytes| {
                String::from_utf8(bytes.clone())
                    .map_err(|_| ManagedHookError::InvalidUtf8(config.clone()))
            })
            .transpose()?
            .unwrap_or_default();
        let manifest_original = read_optional_bounded(&manifest)?;
        let init_original = read_optional_bounded(&init)?;
        for (path, bytes) in [(&manifest, &manifest_original), (&init, &init_original)] {
            if bytes
                .as_ref()
                .is_some_and(|bytes| !String::from_utf8_lossy(bytes).contains(MANAGED_MARKER))
            {
                return Err(ManagedHookError::UnmanagedConflict(path.clone()));
            }
        }
        match operation {
            ManagedHookOperation::Install => Ok(vec![
                mutation(
                    manifest,
                    manifest_original,
                    Some(hermes_manifest(spec).into_bytes()),
                    false,
                ),
                mutation(
                    init,
                    init_original,
                    Some(hermes_plugin_source(spec).into_bytes()),
                    false,
                ),
                mutation(
                    config,
                    config_original,
                    Some(update_hermes_yaml(&config_text, true)?.into_bytes()),
                    false,
                ),
            ]),
            ManagedHookOperation::Remove => Ok(vec![
                mutation(
                    config,
                    config_original,
                    Some(update_hermes_yaml(&config_text, false)?.into_bytes()),
                    false,
                ),
                mutation(init, init_original, None, false),
                mutation(manifest, manifest_original, None, false),
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
    let parsed: Value = if spec.target == "devin" {
        serde_json::from_str(&strip_jsonc_comments(text))?
    } else {
        serde_json::from_str(text)?
    };
    if !parsed.is_object() {
        return Err(ManagedHookError::ConfigRootMustBeObject(
            spec.target.to_owned(),
        ));
    }
    Ok(parsed)
}

fn strip_jsonc_comments(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    let mut in_string = false;
    let mut escaped = false;
    while let Some(character) = chars.next() {
        if in_string {
            out.push(character);
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '"' {
                in_string = false;
            }
            continue;
        }
        if character == '"' {
            in_string = true;
            out.push(character);
        } else if character == '/' && chars.peek() == Some(&'/') {
            chars.next();
            for next in chars.by_ref() {
                if next == '\n' {
                    out.push('\n');
                    break;
                }
            }
        } else if character == '/' && chars.peek() == Some(&'*') {
            chars.next();
            let mut previous = '\0';
            for next in chars.by_ref() {
                if next == '\n' {
                    out.push('\n');
                }
                if previous == '*' && next == '/' {
                    break;
                }
                previous = next;
            }
        } else {
            out.push(character);
        }
    }
    strip_jsonc_trailing_commas(&out)
}

fn strip_jsonc_trailing_commas(input: &str) -> String {
    let characters = input.chars().collect::<Vec<_>>();
    let mut out = String::with_capacity(input.len());
    let mut in_string = false;
    let mut escaped = false;
    for (index, character) in characters.iter().copied().enumerate() {
        if in_string {
            out.push(character);
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '"' {
                in_string = false;
            }
            continue;
        }
        if character == '"' {
            in_string = true;
            out.push(character);
            continue;
        }
        if character == ','
            && characters[index + 1..]
                .iter()
                .copied()
                .find(|candidate| !candidate.is_whitespace())
                .is_some_and(|candidate| matches!(candidate, '}' | ']'))
        {
            continue;
        }
        out.push(character);
    }
    out
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
        let definition = build_definition(manager.roots.platform, event, command);
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
    if spec.target == "copilot" {
        root.remove("disableAllHooks");
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

fn build_definition(
    platform: RuntimePlatform,
    event: &ManagedHookEventSpec,
    command: String,
) -> Value {
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
        ManagedHookEventShape::CopilotCommand { timeout_seconds } => {
            if platform == RuntimePlatform::Windows {
                json!({"type": "command", "powershell": command, "timeoutSec": timeout_seconds})
            } else {
                json!({"type": "command", "bash": command, "timeoutSec": timeout_seconds})
            }
        }
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
    let normalized = command.replace('\\', "/").to_ascii_lowercase();
    ["cmd", "ps1", "sh"].iter().any(|extension| {
        normalized.contains(&format!(
            ".gate4agent/agent-hooks/{}.{}",
            spec.script_stem, extension
        ))
    })
}

fn json_disabled(spec: &ManagedHookAdapterSpec, config: &Value) -> bool {
    (spec.target == "droid" && config.get("hooksDisabled") == Some(&Value::Bool(true)))
        || (spec.target == "copilot" && config.get("disableAllHooks") == Some(&Value::Bool(true)))
}

fn managed_script(
    platform: RuntimePlatform,
    target: &str,
    endpoints: &PublishedHookEndpoint,
) -> String {
    if platform == RuntimePlatform::Windows && target == "copilot" {
        return managed_powershell_script();
    }
    if platform == RuntimePlatform::Windows && target != "kimi" {
        return managed_cmd_script(target, &endpoints.windows_path);
    }
    managed_posix_script(target, &endpoints.posix_path)
}

fn managed_posix_script(target: &str, endpoint_path: &Path) -> String {
    let response = match target {
        "antigravity" => "if [ \"$GATE4AGENT_HOOK_EVENT\" = \"Stop\" ]; then printf '{\"decision\":\"\"}\\n'; else printf '{}\\n'; fi\n",
        "gemini" | "copilot" => "printf '{}\\n'\n",
        _ => "",
    };
    let skip_devin = if target == "claude" {
        "if [ -n \"$DEVIN_PROJECT_DIR\" ]; then cat >/dev/null; exit 0; fi\n"
    } else {
        ""
    };
    let command_code_recovery = if target == "command-code" {
        let endpoint = posix_quote(&endpoint_path.to_string_lossy().replace('\\', "/"));
        format!(
            "if [ -z \"$GATE4AGENT_HOOK_TOKEN\" ] && [ -r {endpoint} ]; then\n  while IFS='=' read -r key value; do\n    case \"$key\" in GATE4AGENT_HOOK_PORT|GATE4AGENT_HOOK_TOKEN|GATE4AGENT_HOOK_VERSION) export \"$key=$value\" ;; esac\n  done < {endpoint}\nfi\nif [ -z \"$GATE4AGENT_HOOK_URL\" ] && [ -n \"$GATE4AGENT_HOOK_PORT\" ]; then GATE4AGENT_HOOK_URL=\"http://127.0.0.1:$GATE4AGENT_HOOK_PORT/hook/command-code\"; fi\n"
        )
    } else {
        String::new()
    };
    format!(
        "#!/bin/sh\n# {MANAGED_MARKER}\n{response}{skip_devin}payload=$(cat)\n{command_code_recovery}if [ -z \"$GATE4AGENT_HOOK_URL\" ] || [ -z \"$GATE4AGENT_HOOK_TOKEN\" ] || [ -z \"$GATE4AGENT_HOOK_ROUTE\" ]; then exit 0; fi\nprintf '%s' \"$payload\" | curl -sS -X POST \"$GATE4AGENT_HOOK_URL\" --connect-timeout 0.5 --max-time 1.5 -H \"Content-Type: application/x-www-form-urlencoded\" -H \"x-gate4agent-hook-token: $GATE4AGENT_HOOK_TOKEN\" -H \"x-gate4agent-hook-route: $GATE4AGENT_HOOK_ROUTE\" --data-urlencode \"event_name=$GATE4AGENT_HOOK_EVENT\" --data-urlencode \"payload@-\" >/dev/null 2>&1 || true\nexit 0\n"
    )
}

fn managed_cmd_script(target: &str, endpoint_path: &Path) -> String {
    let response = match target {
        "antigravity" => "if /I \"%GATE4AGENT_HOOK_EVENT%\"==\"Stop\" (echo {\"decision\":\"\"}) else (echo {})\r\n",
        "gemini" => "echo {}\r\n",
        _ => "",
    };
    let skip_devin = if target == "claude" {
        "if not \"%DEVIN_PROJECT_DIR%\"==\"\" goto :drain\r\n"
    } else {
        ""
    };
    let command_code_recovery = if target == "command-code" {
        format!(
            "if \"%GATE4AGENT_HOOK_TOKEN%\"==\"\" if exist \"{}\" for /f \"usebackq tokens=1,* delims==\" %%A in (\"{}\") do call :endpointValue \"%%A\" \"%%B\"\r\nif \"%GATE4AGENT_HOOK_URL%\"==\"\" if not \"%GATE4AGENT_HOOK_PORT%\"==\"\" set \"GATE4AGENT_HOOK_URL=http://127.0.0.1:%GATE4AGENT_HOOK_PORT%/hook/command-code\"\r\n",
            endpoint_path.display(),
            endpoint_path.display()
        )
    } else {
        String::new()
    };
    let endpoint_label = if target == "command-code" {
        ":endpointValue\r\nif /I \"%~1\"==\"GATE4AGENT_HOOK_PORT\" set \"GATE4AGENT_HOOK_PORT=%~2\"\r\nif /I \"%~1\"==\"GATE4AGENT_HOOK_TOKEN\" set \"GATE4AGENT_HOOK_TOKEN=%~2\"\r\nif /I \"%~1\"==\"GATE4AGENT_HOOK_VERSION\" set \"GATE4AGENT_HOOK_VERSION=%~2\"\r\nexit /b 0\r\n"
    } else {
        ""
    };
    format!(
        "@echo off\r\nrem {MANAGED_MARKER}\r\nsetlocal\r\n{response}{skip_devin}{command_code_recovery}if \"%GATE4AGENT_HOOK_URL%\"==\"\" goto :drain\r\nif \"%GATE4AGENT_HOOK_TOKEN%\"==\"\" goto :drain\r\nif \"%GATE4AGENT_HOOK_ROUTE%\"==\"\" goto :drain\r\n\"%SystemRoot%\\System32\\curl.exe\" -sS -X POST \"%GATE4AGENT_HOOK_URL%\" --connect-timeout 0.5 --max-time 1.5 -H \"Content-Type: application/x-www-form-urlencoded\" -H \"x-gate4agent-hook-token: %GATE4AGENT_HOOK_TOKEN%\" -H \"x-gate4agent-hook-route: %GATE4AGENT_HOOK_ROUTE%\" --data-urlencode \"event_name=%GATE4AGENT_HOOK_EVENT%\" --data-urlencode \"payload@-\" >nul 2>nul\r\nexit /b 0\r\n:drain\r\nmore >nul\r\nexit /b 0\r\n{endpoint_label}"
    )
}

fn managed_powershell_script() -> String {
    format!("# {MANAGED_MARKER}\r\nWrite-Output '{{}}'\r\n$payload = [Console]::In.ReadToEnd()\r\nif (-not $env:GATE4AGENT_HOOK_URL -or -not $env:GATE4AGENT_HOOK_TOKEN -or -not $env:GATE4AGENT_HOOK_ROUTE) {{ exit 0 }}\r\ntry {{\r\n  $body = @{{ event_name = $env:GATE4AGENT_HOOK_EVENT; payload = $payload }} | ConvertTo-Json -Compress\r\n  Invoke-WebRequest -UseBasicParsing -Method Post -Uri $env:GATE4AGENT_HOOK_URL -Headers @{{ 'Content-Type'='application/json'; 'x-gate4agent-hook-token'=$env:GATE4AGENT_HOOK_TOKEN; 'x-gate4agent-hook-route'=$env:GATE4AGENT_HOOK_ROUTE }} -Body $body -TimeoutSec 2 | Out-Null\r\n}} catch {{}}\r\nexit 0\r\n")
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

#[derive(Clone, Copy)]
struct HermesYamlState {
    enabled: bool,
    disabled: bool,
}

fn inspect_hermes_yaml(text: &str) -> Result<HermesYamlState, ManagedHookError> {
    let enabled = yaml_list_contains(text, "enabled")?;
    let disabled = yaml_list_contains(text, "disabled")?;
    Ok(HermesYamlState { enabled, disabled })
}

fn yaml_list_contains(text: &str, key: &str) -> Result<bool, ManagedHookError> {
    let lines = text.lines().collect::<Vec<_>>();
    let Some(plugins) = lines
        .iter()
        .position(|line| line.trim_end() == "plugins:" && !line.starts_with(char::is_whitespace))
    else {
        return Ok(false);
    };
    let end = lines[plugins + 1..]
        .iter()
        .position(|line| !line.trim().is_empty() && !line.starts_with(char::is_whitespace))
        .map(|relative| plugins + 1 + relative)
        .unwrap_or(lines.len());
    for index in plugins + 1..end {
        let line = lines[index];
        if line.starts_with("  ") && line.trim_start().starts_with(&format!("{key}:")) {
            let tail = line.trim_start()[key.len() + 1..].trim();
            if tail.starts_with('[') {
                return Ok(tail
                    .trim_matches(|c| c == '[' || c == ']')
                    .split(',')
                    .any(|item| item.trim().trim_matches(['\'', '"']) == HERMES_PLUGIN_NAME));
            }
            if !tail.is_empty() {
                return Err(ManagedHookError::UnsupportedHermesYaml);
            }
            return Ok(lines[index + 1..end]
                .iter()
                .take_while(|candidate| {
                    candidate.starts_with("    ") || candidate.trim().is_empty()
                })
                .any(|candidate| {
                    candidate
                        .trim_start()
                        .strip_prefix("- ")
                        .is_some_and(|value| value.trim_matches(['\'', '"']) == HERMES_PLUGIN_NAME)
                }));
        }
    }
    Ok(false)
}

fn update_hermes_yaml(text: &str, enable: bool) -> Result<String, ManagedHookError> {
    let mut lines = text.lines().map(str::to_owned).collect::<Vec<_>>();
    let plugins = lines.iter().position(|line| line == "plugins:");
    if plugins.is_none() {
        if !enable {
            return Ok(text.to_owned());
        }
        if !lines.is_empty() && !lines.last().is_some_and(|line| line.is_empty()) {
            lines.push(String::new());
        }
        lines.extend([
            "plugins:".to_owned(),
            "  enabled:".to_owned(),
            format!("    - {HERMES_PLUGIN_NAME}"),
        ]);
        return Ok(format!("{}\n", lines.join("\n")));
    }
    let plugins = plugins.unwrap();
    let end = lines[plugins + 1..]
        .iter()
        .position(|line| !line.trim().is_empty() && !line.starts_with(char::is_whitespace))
        .map(|relative| plugins + 1 + relative)
        .unwrap_or(lines.len());
    update_yaml_list(&mut lines, plugins + 1, end, "disabled", false)?;
    let end = lines[plugins + 1..]
        .iter()
        .position(|line| !line.trim().is_empty() && !line.starts_with(char::is_whitespace))
        .map(|relative| plugins + 1 + relative)
        .unwrap_or(lines.len());
    update_yaml_list(&mut lines, plugins + 1, end, "enabled", enable)?;
    Ok(format!("{}\n", lines.join("\n").trim_end()))
}

fn update_yaml_list(
    lines: &mut Vec<String>,
    start: usize,
    end: usize,
    key: &str,
    include: bool,
) -> Result<(), ManagedHookError> {
    let key_line = lines[start..end]
        .iter()
        .position(|line| {
            line.starts_with("  ") && line.trim_start().starts_with(&format!("{key}:"))
        })
        .map(|relative| start + relative);
    let Some(index) = key_line else {
        if include {
            lines.splice(
                end..end,
                [format!("  {key}:"), format!("    - {HERMES_PLUGIN_NAME}")],
            );
        }
        return Ok(());
    };
    let tail = lines[index].trim_start()[key.len() + 1..].trim().to_owned();
    if tail.starts_with('[') {
        let mut values = tail
            .trim_matches(|c| c == '[' || c == ']')
            .split(',')
            .map(|item| item.trim().trim_matches(['\'', '"']).to_owned())
            .filter(|item| !item.is_empty() && item != HERMES_PLUGIN_NAME)
            .collect::<Vec<_>>();
        if include {
            values.push(HERMES_PLUGIN_NAME.to_owned());
        }
        lines[index] = format!("  {key}: [{}]", values.join(", "));
        return Ok(());
    }
    if !tail.is_empty() {
        return Err(ManagedHookError::UnsupportedHermesYaml);
    }
    let list_end = lines[index + 1..end]
        .iter()
        .position(|line| !line.trim().is_empty() && !line.starts_with("    "))
        .map(|relative| index + 1 + relative)
        .unwrap_or(end);
    let retained = lines[index + 1..list_end]
        .iter()
        .filter(|line| {
            line.trim_start()
                .strip_prefix("- ")
                .is_none_or(|value| value.trim_matches(['\'', '"']) != HERMES_PLUGIN_NAME)
        })
        .cloned()
        .collect::<Vec<_>>();
    lines.splice(index + 1..list_end, retained);
    if include {
        lines.insert(index + 1, format!("    - {HERMES_PLUGIN_NAME}"));
    }
    Ok(())
}

fn hermes_manifest(spec: &ManagedHookAdapterSpec) -> String {
    let hooks = spec
        .events
        .iter()
        .map(|event| format!("  - {}", event.name))
        .collect::<Vec<_>>()
        .join("\n");
    format!("# {MANAGED_MARKER}\nname: {HERMES_PLUGIN_NAME}\nversion: 1.0.0\ndescription: \"Reports Hermes lifecycle events to Gate4Agent.\"\nauthor: \"Gate4Agent\"\nkind: standalone\nprovides_hooks:\n{hooks}\n")
}

fn hermes_plugin_source(spec: &ManagedHookAdapterSpec) -> String {
    let events = spec
        .events
        .iter()
        .map(|event| format!("\"{}\"", event.name))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        r#"# {MANAGED_MARKER}
from __future__ import annotations
import json
import os
import urllib.error
import urllib.request
from typing import Any, Callable, Optional

EVENTS = [{events}]
MAX_JSONABLE_DEPTH = 5
MAX_JSONABLE_ITEMS = 50
MAX_JSONABLE_NODES = 500
MAX_JSONABLE_STRING = 8192
TRUNCATED = "...[truncated]"
SELECTED_KEYS = {{
    "on_session_start": ("session_id", "model", "platform"),
    "pre_llm_call": ("session_id", "user_message", "is_first_turn", "model", "platform", "sender_id"),
    "post_llm_call": ("session_id", "user_message", "assistant_response", "model", "platform"),
    "pre_tool_call": ("session_id", "task_id", "tool_call_id", "tool_name", "args"),
    "post_tool_call": ("session_id", "task_id", "tool_call_id", "tool_name", "args", "result", "duration_ms"),
    "pre_approval_request": ("command", "description", "pattern_key", "pattern_keys", "session_key", "surface"),
    "post_approval_response": ("command", "description", "pattern_key", "pattern_keys", "session_key", "surface", "choice"),
    "on_session_end": ("session_id",),
    "on_session_finalize": ("session_id", "platform"),
    "on_session_reset": ("session_id", "platform"),
}}

def _truncate_string(value: str) -> str:
    if len(value) <= MAX_JSONABLE_STRING:
        return value
    return value[:MAX_JSONABLE_STRING] + TRUNCATED

def _jsonable(value: Any, depth: int = 0, budget: Optional[list[int]] = None) -> Any:
    if budget is None:
        budget = [MAX_JSONABLE_NODES]
    if budget[0] <= 0:
        return TRUNCATED
    budget[0] -= 1
    if depth > MAX_JSONABLE_DEPTH:
        return _truncate_string(repr(value))
    if value is None or isinstance(value, (int, float, bool)):
        return value
    if isinstance(value, str):
        return _truncate_string(value)
    if isinstance(value, dict):
        out: dict[str, Any] = {{}}
        for index, (key, child) in enumerate(value.items()):
            if index >= MAX_JSONABLE_ITEMS:
                out[TRUNCATED] = True
                break
            out[_truncate_string(str(key))] = _jsonable(child, depth + 1, budget)
        return out
    if isinstance(value, (list, tuple, set)):
        out = []
        for index, item in enumerate(value):
            if index >= MAX_JSONABLE_ITEMS:
                out.append(TRUNCATED)
                break
            out.append(_jsonable(item, depth + 1, budget))
        return out
    return _truncate_string(repr(value))

def _post(event_name: str, payload: dict[str, Any]) -> None:
    url = os.environ.get("GATE4AGENT_HOOK_URL", "")
    token = os.environ.get("GATE4AGENT_HOOK_TOKEN", "")
    route = os.environ.get("GATE4AGENT_HOOK_ROUTE", "")
    if not url or not token or not route:
        return
    try:
        body = json.dumps({{"event_name": event_name, "payload": payload}}, separators=(",", ":")).encode("utf-8")
        request = urllib.request.Request(url, data=body, method="POST", headers={{
            "Content-Type": "application/json",
            "x-gate4agent-hook-token": token,
            "x-gate4agent-hook-route": route,
        }})
        with urllib.request.urlopen(request, timeout=0.75):
            pass
    except (OSError, TypeError, ValueError, urllib.error.URLError):
        return

def _make_hook(event_name: str) -> Callable[..., None]:
    def _hook(**kwargs: Any) -> None:
        payload = {{"hook_event_name": event_name, "cwd": os.getcwd()}}
        for key in SELECTED_KEYS.get(event_name, ()):
            if key in kwargs:
                payload[key] = _jsonable(kwargs[key])
        _post(event_name, payload)
    return _hook

def register(ctx: Any) -> None:
    for event_name in EVENTS:
        ctx.register_hook(event_name, _make_hook(event_name))
"#
    )
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
    #[error("Windows Devin configuration requires an explicit app-data root")]
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
    #[error("Hermes config.yaml uses an unsupported plugins list shape")]
    UnsupportedHermesYaml,
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
        for target in [
            "claude",
            "openclaude",
            "codex",
            "gemini",
            "antigravity",
            "amp",
            "cursor",
            "droid",
            "command-code",
            "grok",
            "copilot",
            "hermes",
            "devin",
            "kimi",
        ] {
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
        let binding = binding("cursor");
        let plan = manager
            .plan(&binding, ManagedHookOperation::Install)
            .unwrap();
        let config = root.0.join("home/.cursor/hooks.json");
        fs::create_dir_all(config.parent().unwrap()).unwrap();
        fs::write(&config, b"{\"user\":true}\n").unwrap();
        assert!(matches!(
            manager.apply(plan),
            Err(ManagedHookError::PlanDrift(path)) if path == config
        ));
        assert_eq!(fs::read(&config).unwrap(), b"{\"user\":true}\n");
        assert!(!root
            .0
            .join("home/.gate4agent/agent-hooks/cursor-hook.sh")
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

        let amp = root
            .0
            .join("home/.config/amp/plugins/gate4agent-agent-status.ts");
        fs::create_dir_all(amp.parent().unwrap()).unwrap();
        fs::write(&amp, b"export default userPlugin\n").unwrap();
        assert_eq!(
            manager.status(&binding("amp")).unwrap().state,
            ManagedHookState::Conflict
        );
        assert!(matches!(
            manager.plan(&binding("amp"), ManagedHookOperation::Remove),
            Err(ManagedHookError::UnmanagedConflict(path)) if path == amp
        ));
    }

    #[test]
    fn hermes_yaml_preserves_other_plugin_memberships() {
        let root = TestRoot::new("hermes-yaml");
        let manager = root.manager();
        let path = root.0.join("home/.hermes/config.yaml");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            "model: test\nplugins:\n  enabled:\n    - user-plugin\n  disabled:\n    - blocked-plugin\nother: value\n",
        )
        .unwrap();
        let binding = binding("hermes");
        manager
            .apply(
                manager
                    .plan(&binding, ManagedHookOperation::Install)
                    .unwrap(),
            )
            .unwrap();
        let installed = fs::read_to_string(&path).unwrap();
        assert!(installed.contains("- user-plugin"));
        assert!(installed.contains("- blocked-plugin"));
        assert!(installed.contains("- gate4agent-status"));
        assert!(installed.contains("other: value"));

        manager
            .apply(
                manager
                    .plan(&binding, ManagedHookOperation::Remove)
                    .unwrap(),
            )
            .unwrap();
        let removed = fs::read_to_string(&path).unwrap();
        assert!(removed.contains("- user-plugin"));
        assert!(removed.contains("- blocked-plugin"));
        assert!(!removed.contains("- gate4agent-status"));
        assert!(removed.contains("other: value"));
    }

    #[test]
    fn devin_jsonc_is_accepted_but_invalid_json_is_fail_closed() {
        let root = TestRoot::new("devin-jsonc");
        let manager = root.manager();
        let path = root.0.join("home/.config/devin/config.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            "{\n // user setting\n \"read_config_from\": false,\n}\n",
        )
        .unwrap();
        let devin_binding = binding("devin");
        manager
            .apply(
                manager
                    .plan(&devin_binding, ManagedHookOperation::Install)
                    .unwrap(),
            )
            .unwrap();
        let config: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(config["read_config_from"], false);

        let broken = root.0.join("home/.gemini/settings.json");
        fs::create_dir_all(broken.parent().unwrap()).unwrap();
        fs::write(&broken, b"{broken").unwrap();
        assert!(matches!(
            manager.plan(&binding("gemini"), ManagedHookOperation::Install),
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
        let root = TestRoot::new("script-contracts");
        let manager = root.manager();
        for target in ["command-code", "antigravity", "claude", "amp", "hermes"] {
            let target_binding = binding(target);
            manager
                .apply(
                    manager
                        .plan(&target_binding, ManagedHookOperation::Install)
                        .unwrap(),
                )
                .unwrap();
        }
        let command_code = fs::read_to_string(
            root.0
                .join("home/.gate4agent/agent-hooks/command-code-hook.sh"),
        )
        .unwrap();
        assert!(command_code.contains("endpoint.env"));
        assert!(command_code.contains("GATE4AGENT_HOOK_TOKEN"));
        assert!(command_code.contains("/hook/command-code"));
        assert!(command_code.contains("x-gate4agent-hook-route"));

        let antigravity = fs::read_to_string(
            root.0
                .join("home/.gate4agent/agent-hooks/antigravity-hook.sh"),
        )
        .unwrap();
        assert!(antigravity.contains("{\"decision\":\"\"}"));
        assert!(antigravity.contains("GATE4AGENT_HOOK_EVENT"));

        let claude =
            fs::read_to_string(root.0.join("home/.gate4agent/agent-hooks/claude-hook.sh")).unwrap();
        assert!(claude.contains("DEVIN_PROJECT_DIR"));
        assert!(claude.contains("cat >/dev/null"));

        let amp = fs::read_to_string(
            root.0
                .join("home/.config/amp/plugins/gate4agent-agent-status.ts"),
        )
        .unwrap();
        assert!(amp.contains("const MAX_PENDING_POSTS = 50"));
        assert!(amp.contains("jsonSafe(event.input)"));
        assert!(amp.contains("previewValue(event.output)"));

        let hermes = fs::read_to_string(
            root.0
                .join("home/.hermes/plugins/gate4agent-status/__init__.py"),
        )
        .unwrap();
        assert!(hermes.contains("MAX_JSONABLE_NODES = 500"));
        assert!(hermes.contains("payload[key] = _jsonable(kwargs[key])"));
    }

    #[test]
    fn windows_plans_round_trip_all_targets_without_embedding_authority() {
        let root = TestRoot::new("windows-round-trip");
        let manager = root.manager_for(RuntimePlatform::Windows);
        let mut command_code_script = String::new();
        for target in [
            "claude",
            "openclaude",
            "codex",
            "gemini",
            "antigravity",
            "amp",
            "cursor",
            "droid",
            "command-code",
            "grok",
            "copilot",
            "hermes",
            "devin",
            "kimi",
        ] {
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
            if target == "command-code" {
                command_code_script = fs::read_to_string(
                    root.0
                        .join("home/.gate4agent/agent-hooks/command-code-hook.cmd"),
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
        assert!(command_code_script.contains("endpoint.cmd"));
        assert!(command_code_script.contains("GATE4AGENT_HOOK_TOKEN=%~2"));
        assert!(!command_code_script.contains("x-gate4agent-hook-token: 00000000"));
    }
}
