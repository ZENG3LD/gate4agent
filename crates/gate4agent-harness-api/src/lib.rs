//! Privacy-minimized read wire contract for the harness localhost host.
//!
//! Each loopback TCP connection carries exactly one JSON request frame followed
//! by `\n`. The caller must then half-close its write side; EOF is the request
//! boundary and the host sends exactly one newline-terminated reply. A newline
//! without the write half-close is incomplete and expires categorically as
//! `HarnessReadHostErrorV1::Deadline`.

use std::{collections::BTreeMap, fmt};

pub use gate4agent_harness_protocol::{
    HarnessArtifactRef, HarnessEntityReadScopeV1, HarnessExecutionModeV1,
    HarnessFailureCategoryV1, HarnessIdempotencyRef, HarnessMonitoringVisibilityV1,
    HarnessOperationId,
    HarnessOperationKindV1, HarnessOperationStateV1, HarnessOutcomeUnknownReasonV1,
    HarnessCancelTaskRequestV1, HarnessCreateTaskRequestV1, HarnessMoveTaskRequestV1,
    HarnessOperatorAuthorityV1, HarnessReplaceTaskRequestV1, HarnessRetryTaskRequestV1,
    HarnessScheduleNextRequestV1, HarnessScheduleOutcomeV1,
    HarnessReadPermissionsV1, HarnessReconciliationOutcomeV1, HarnessResultDispositionV1,
    HarnessResultRef, HarnessRevision, HarnessRunId, HarnessRunLifecycleV1, HarnessSelectorV1,
    HarnessTaskId, HarnessTaskStateV1, HarnessValidationError, SessionGrantId,
    HARNESS_ARTIFACTS_MAX, HARNESS_BODY_MAX_BYTES,
    HARNESS_CHILD_COUNT_MAX, HARNESS_CHILD_DEPTH_MAX, HARNESS_DEPENDENCIES_MAX,
    HARNESS_LINKS_MAX, HARNESS_RESULTS_MAX, HARNESS_TITLE_MAX_BYTES,
};
use serde::{Deserialize, Deserializer, Serialize};
use thiserror::Error;

pub const HARNESS_READ_WIRE_VERSION_V1: u16 = 1;
pub const HARNESS_READ_REQUEST_MAX_BYTES: usize = 64 * 1024;
pub const HARNESS_READ_RESPONSE_MAX_BYTES: usize = 1024 * 1024;
pub const HARNESS_READ_LIMIT_MAX: u16 = 256;
pub const HARNESS_ENTITY_PAGE_LIMIT_MAX: u16 = 64;
pub const HARNESS_TIMELINE_PAGE_LIMIT_MAX: u16 = 128;
pub const HARNESS_MONITOR_FACTS_MAX: usize = 128;
pub const HARNESS_READ_CREDENTIAL_MAX_BYTES: usize = 8 * 1024;
pub const HARNESS_MCP_AUDIENCE: &str = "gate4agent-harness-mcp-read-v1";
pub const HARNESS_OPERATOR_WIRE_VERSION_V1: u16 = 1;
pub const HARNESS_OPERATOR_WIRE_VERSION_V2: u16 = 2;
pub const HARNESS_OPERATOR_WIRE_VERSION_V3: u16 = 3;
pub const HARNESS_OPERATOR_REQUEST_MAX_BYTES: usize = 64 * 1024;
pub const HARNESS_OPERATOR_RESPONSE_MAX_BYTES: usize = 1024 * 1024;
pub const HARNESS_OPERATOR_CREDENTIAL_MAX_BYTES: usize = 256;
pub const HARNESS_RUNTIME_INVENTORY_PAGE_LIMIT_MAX: u16 = 64;
pub const HARNESS_NATIVE_SESSION_CATALOG_LIMIT_MAX: u16 = 64;
pub const HARNESS_NATIVE_SESSION_PREVIEW_MESSAGE_LIMIT_MAX: u16 = 24;
pub const HARNESS_NATIVE_SESSION_PREVIEW_TEXT_MAX_BYTES: usize = 4_096;

pub const HARNESS_READ_TOOL_IDS: [&str; 8] = [
    "g4a_context_get",
    "g4a_monitor_get",
    "g4a_timeline_read",
    "g4a_tasks_list",
    "g4a_tasks_get",
    "g4a_runs_list",
    "g4a_runs_get",
    "g4a_operation_get",
];

const TOKEN_PREFIX: &str = "g4ah2_";
const OPERATOR_TOKEN_PREFIX: &str = "g4aho_";
const OPERATOR_REQUEST_REF_PREFIX: &str = "hireq_";

#[derive(Clone, Eq, PartialEq)]
pub struct HarnessReadCredential(String);

impl HarnessReadCredential {
    pub fn parse(value: impl Into<String>) -> Result<Self, HarnessReadApiError> {
        let value = value.into();
        validate_credential(&value)?;
        Ok(Self(value))
    }

    pub fn expose(&self) -> &str { &self.0 }
}

impl fmt::Debug for HarnessReadCredential {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("HarnessReadCredential([REDACTED])")
    }
}

impl Serialize for HarnessReadCredential {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where S: serde::Serializer {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for HarnessReadCredential {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where D: Deserializer<'de> {
        Self::parse(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct HarnessOperatorCredential(String);

impl HarnessOperatorCredential {
    pub fn parse(value: impl Into<String>) -> Result<Self, HarnessOperatorApiError> {
        let value = value.into();
        validate_operator_credential(&value)?;
        Ok(Self(value))
    }

    pub fn expose(&self) -> &str { &self.0 }
}

impl fmt::Debug for HarnessOperatorCredential {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("HarnessOperatorCredential([REDACTED])")
    }
}

impl Serialize for HarnessOperatorCredential {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where S: serde::Serializer {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for HarnessOperatorCredential {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where D: Deserializer<'de> {
        Self::parse(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessOperatorEnvelopeV1 {
    pub version: u16,
    pub credential: HarnessOperatorCredential,
    pub request: HarnessOperatorRequestV1,
}

impl HarnessOperatorEnvelopeV1 {
    pub fn validate(&self) -> Result<(), HarnessOperatorApiError> {
        if !matches!(
            self.version,
            HARNESS_OPERATOR_WIRE_VERSION_V2 | HARNESS_OPERATOR_WIRE_VERSION_V3
        ) || self.request.requires_v3() && self.version != HARNESS_OPERATOR_WIRE_VERSION_V3
        {
            return Err(HarnessOperatorApiError::UnsupportedVersion);
        }
        validate_operator_credential(self.credential.expose())?;
        self.request.validate()
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct HarnessOperatorRequestRefV1(String);

impl HarnessOperatorRequestRefV1 {
    pub fn new(value: impl Into<String>) -> Result<Self, HarnessOperatorApiError> {
        let value = value.into();
        validate_operator_request_ref(&value)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str { &self.0 }

    pub fn validate(&self) -> Result<(), HarnessOperatorApiError> {
        validate_operator_request_ref(&self.0)
    }
}

impl<'de> Deserialize<'de> for HarnessOperatorRequestRefV1 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessOperatorIntentV1 {
    pub request_ref: HarnessOperatorRequestRefV1,
    pub submitted_at_unix_ms: u64,
    pub action: HarnessOperatorActionV1,
}

impl HarnessOperatorIntentV1 {
    pub fn validate(&self) -> Result<(), HarnessOperatorApiError> {
        self.request_ref.validate()?;
        if self.submitted_at_unix_ms == 0 {
            return Err(HarnessOperatorApiError::InvalidSubmittedAt);
        }
        let authority = HarnessOperatorAuthorityV1 {
            operation_id: HarnessOperationId::new(format!("hop_{}", "0".repeat(24)))
                .map_err(HarnessOperatorApiError::Protocol)?,
            idempotency_ref: HarnessIdempotencyRef::new(format!("hidem_{}", "0".repeat(24)))
                .map_err(HarnessOperatorApiError::Protocol)?,
            actor_id: HarnessSelectorV1::new("harness-operator")
                .map_err(HarnessOperatorApiError::Protocol)?,
            now_unix_ms: self.submitted_at_unix_ms,
        };
        let task_id = HarnessTaskId::new(format!("htask_{}", "0".repeat(24)))
            .map_err(HarnessOperatorApiError::Protocol)?;
        self.action.clone().authorize(authority, task_id).validate()
    }

    pub fn authorize(
        self,
        authority: HarnessOperatorAuthorityV1,
        create_task_id: HarnessTaskId,
    ) -> HarnessOperatorRequestV1 {
        self.action.authorize(authority, create_task_id)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum HarnessOperatorActionV1 {
    CreateTask {
        title: String,
        body: String,
        parent_task_id: Option<HarnessTaskId>,
        dependencies: Vec<HarnessTaskId>,
        initial_state: HarnessTaskStateV1,
    },
    ReplaceTask {
        task_id: HarnessTaskId,
        expected_revision: HarnessRevision,
        title: String,
        body: String,
        parent_task_id: Option<HarnessTaskId>,
        dependencies: Vec<HarnessTaskId>,
    },
    MoveTask {
        task_id: HarnessTaskId,
        expected_revision: HarnessRevision,
        state: HarnessTaskStateV1,
    },
    CancelTask {
        task_id: HarnessTaskId,
        expected_revision: HarnessRevision,
    },
    RetryTask {
        task_id: HarnessTaskId,
        expected_revision: HarnessRevision,
    },
    ScheduleNext {
        plan_id: Option<HarnessSelectorV1>,
    },
}

impl HarnessOperatorActionV1 {
    pub fn authorize(
        self,
        authority: HarnessOperatorAuthorityV1,
        create_task_id: HarnessTaskId,
    ) -> HarnessOperatorRequestV1 {
        match self {
            Self::CreateTask {
                title,
                body,
                parent_task_id,
                dependencies,
                initial_state,
            } => HarnessOperatorRequestV1::CreateTask {
                request: HarnessCreateTaskRequestV1 {
                    authority,
                    task_id: create_task_id,
                    title,
                    body,
                    parent_task_id,
                    dependencies,
                    initial_state,
                },
            },
            Self::ReplaceTask {
                task_id,
                expected_revision,
                title,
                body,
                parent_task_id,
                dependencies,
            } => HarnessOperatorRequestV1::ReplaceTask {
                request: HarnessReplaceTaskRequestV1 {
                    authority,
                    task_id,
                    expected_revision,
                    title,
                    body,
                    parent_task_id,
                    dependencies,
                },
            },
            Self::MoveTask { task_id, expected_revision, state } => {
                HarnessOperatorRequestV1::MoveTask {
                    request: HarnessMoveTaskRequestV1 {
                        authority,
                        task_id,
                        expected_revision,
                        state,
                    },
                }
            }
            Self::CancelTask { task_id, expected_revision } => {
                HarnessOperatorRequestV1::CancelTask {
                    request: HarnessCancelTaskRequestV1 {
                        authority,
                        task_id,
                        expected_revision,
                    },
                }
            }
            Self::RetryTask { task_id, expected_revision } => {
                HarnessOperatorRequestV1::RetryTask {
                    request: HarnessRetryTaskRequestV1 {
                        authority,
                        task_id,
                        expected_revision,
                    },
                }
            }
            Self::ScheduleNext { plan_id } => HarnessOperatorRequestV1::ScheduleNext {
                request: HarnessScheduleNextRequestV1 { authority, plan_id },
            },
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum HarnessOperatorRequestV1 {
    MonitorGet { run_id: HarnessRunId },
    TimelineRead {
        run_id: HarnessRunId,
        after_sequence: Option<u64>,
        limit: u16,
    },
    TasksList {
        after_task_id: Option<HarnessTaskId>,
        state: Option<HarnessTaskStateV1>,
        limit: u16,
    },
    TaskGet { task_id: HarnessTaskId },
    RunsList {
        task_id: Option<HarnessTaskId>,
        after_run_id: Option<HarnessRunId>,
        lifecycle: Option<HarnessRunLifecycleV1>,
        limit: u16,
    },
    RunGet { run_id: HarnessRunId },
    RuntimeInventoryList {
        after_node_id: Option<String>,
        limit: u16,
    },
    CatalogNativeSessions {
        route: HarnessNativeSessionRouteV1,
        limit: u16,
    },
    PageNativeSessions {
        route: HarnessNativeSessionRouteV1,
        window: HarnessNativeSessionCatalogWindowV1,
        catalog_revision: u64,
        recent_cutoff_unix_ms: u64,
        after_selection_id: Option<String>,
        limit: u16,
    },
    PreviewNativeSession {
        selection: HarnessNativeSessionSelectionV1,
        message_limit: u16,
    },
    CreateTask { request: HarnessCreateTaskRequestV1 },
    ReplaceTask { request: HarnessReplaceTaskRequestV1 },
    MoveTask { request: HarnessMoveTaskRequestV1 },
    CancelTask { request: HarnessCancelTaskRequestV1 },
    RetryTask { request: HarnessRetryTaskRequestV1 },
    ScheduleNext { request: HarnessScheduleNextRequestV1 },
    SubmitIntent { intent: HarnessOperatorIntentV1 },
}

impl HarnessOperatorRequestV1 {
    pub fn validate(&self) -> Result<(), HarnessOperatorApiError> {
        match self {
            Self::MonitorGet { run_id } => {
                run_id.validate().map_err(HarnessOperatorApiError::Protocol)
            }
            Self::TimelineRead { run_id, after_sequence, limit } => {
                run_id.validate().map_err(HarnessOperatorApiError::Protocol)?;
                if after_sequence == &Some(0) {
                    return Err(HarnessOperatorApiError::InvalidCursor);
                }
                validate_operator_timeline_limit(*limit)
            }
            Self::TasksList { after_task_id, limit, .. } => {
                if let Some(task_id) = after_task_id {
                    task_id.validate().map_err(HarnessOperatorApiError::Protocol)?;
                }
                validate_operator_limit(*limit)
            }
            Self::TaskGet { task_id } => {
                task_id.validate().map_err(HarnessOperatorApiError::Protocol)
            }
            Self::RunsList { task_id, after_run_id, limit, .. } => {
                if let Some(task_id) = task_id {
                    task_id.validate().map_err(HarnessOperatorApiError::Protocol)?;
                }
                if let Some(run_id) = after_run_id {
                    run_id.validate().map_err(HarnessOperatorApiError::Protocol)?;
                }
                validate_operator_limit(*limit)
            }
            Self::RunGet { run_id } => {
                run_id.validate().map_err(HarnessOperatorApiError::Protocol)
            }
            Self::RuntimeInventoryList { after_node_id, limit } => {
                if !(1..=HARNESS_RUNTIME_INVENTORY_PAGE_LIMIT_MAX).contains(limit) {
                    return Err(HarnessOperatorApiError::InvalidLimit);
                }
                if after_node_id.as_deref().is_some_and(|node_id| {
                    !valid_runtime_id(node_id, 128)
                }) {
                    return Err(HarnessOperatorApiError::InvalidCursor);
                }
                Ok(())
            }
            Self::CatalogNativeSessions { route, limit } => {
                route.validate()?;
                validate_native_session_catalog_limit(*limit)
            }
            Self::PageNativeSessions {
                route,
                catalog_revision,
                after_selection_id,
                limit,
                ..
            } => {
                route.validate()?;
                if *catalog_revision == 0
                    || after_selection_id.as_deref().is_some_and(|value| {
                        !valid_native_selection_id(value)
                    })
                {
                    return Err(HarnessOperatorApiError::InvalidNativeHistory);
                }
                validate_native_session_catalog_limit(*limit)
            }
            Self::PreviewNativeSession { selection, message_limit } => {
                selection.validate()?;
                if !(1..=HARNESS_NATIVE_SESSION_PREVIEW_MESSAGE_LIMIT_MAX)
                    .contains(message_limit)
                {
                    return Err(HarnessOperatorApiError::InvalidLimit);
                }
                Ok(())
            }
            Self::CreateTask { request } => {
                request.validate().map_err(HarnessOperatorApiError::Protocol)
            }
            Self::ReplaceTask { request } => {
                request.validate().map_err(HarnessOperatorApiError::Protocol)
            }
            Self::MoveTask { request } => {
                request.validate().map_err(HarnessOperatorApiError::Protocol)
            }
            Self::CancelTask { request } => {
                request.validate().map_err(HarnessOperatorApiError::Protocol)
            }
            Self::RetryTask { request } => {
                request.validate().map_err(HarnessOperatorApiError::Protocol)
            }
            Self::ScheduleNext { request } => {
                request.validate().map_err(HarnessOperatorApiError::Protocol)
            }
            Self::SubmitIntent { intent } => intent.validate(),
        }
    }

    pub fn requires_v3(&self) -> bool {
        matches!(
            self,
            Self::RuntimeInventoryList { .. }
                | Self::CatalogNativeSessions { .. }
                | Self::PageNativeSessions { .. }
                | Self::PreviewNativeSession { .. }
                | Self::SubmitIntent { .. }
        )
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "kebab-case", deny_unknown_fields)]
pub enum HarnessOperatorReplyV1 {
    Ok { response: HarnessOperatorResponseV1 },
    Error { error: HarnessOperatorHostErrorV1 },
}

impl HarnessOperatorReplyV1 {
    pub fn validate(&self) -> Result<(), HarnessOperatorApiError> {
        match self {
            Self::Ok { response } => response.validate(),
            Self::Error { .. } => Ok(()),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", content = "value", rename_all = "kebab-case", deny_unknown_fields)]
pub enum HarnessOperatorResponseV1 {
    Monitor(SessionMonitorV1),
    Timeline(TimelinePageV1),
    Tasks(TaskPageV1),
    Task(RedactedTaskV1),
    Runs(RunPageV1),
    Run(RedactedRunV1),
    RuntimeInventory(HarnessRuntimeInventoryPageV1),
    NativeSessionsCataloged(HarnessNativeSessionsCatalogedV1),
    NativeSessionsPaged(HarnessNativeSessionsPagedV1),
    NativeSessionPreviewed(HarnessNativeSessionPreviewedV1),
    Mutation(HarnessOperatorMutationOutcomeV1),
    Schedule(HarnessScheduleOutcomeV1),
}

impl HarnessOperatorResponseV1 {
    pub fn validate(&self) -> Result<(), HarnessOperatorApiError> {
        match self {
            Self::Monitor(value) => value.validate().map_err(HarnessOperatorApiError::Read),
            Self::Timeline(value) => value.validate().map_err(HarnessOperatorApiError::Read),
            Self::Tasks(value) => value.validate().map_err(HarnessOperatorApiError::Read),
            Self::Task(value) => value.validate().map_err(HarnessOperatorApiError::Read),
            Self::Runs(value) => value.validate().map_err(HarnessOperatorApiError::Read),
            Self::Run(value) => value.validate().map_err(HarnessOperatorApiError::Read),
            Self::RuntimeInventory(value) => value.validate(),
            Self::NativeSessionsCataloged(value) => value.validate(),
            Self::NativeSessionsPaged(value) => value.validate(),
            Self::NativeSessionPreviewed(value) => value.validate(),
            Self::Mutation(_) => Ok(()),
            Self::Schedule(HarnessScheduleOutcomeV1::Idle) => Ok(()),
            Self::Schedule(HarnessScheduleOutcomeV1::Dispatch(value)) => {
                value.validate().map_err(HarnessOperatorApiError::Protocol)
            }
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessRuntimeNodeInventoryV1 {
    pub node_id: String,
    pub incarnation_id: String,
    pub observed_at_unix_ms: u64,
    pub event_sequence: u64,
    pub inventory: HarnessRuntimeInventoryV1,
}

impl HarnessRuntimeNodeInventoryV1 {
    pub fn validate(&self) -> Result<(), HarnessOperatorApiError> {
        if !valid_runtime_id(&self.node_id, 128)
            || self.incarnation_id.len() != 32
            || !self.incarnation_id.bytes().all(is_lower_hex)
            || self.observed_at_unix_ms == 0
        {
            return Err(HarnessOperatorApiError::InvalidRuntimeInventory);
        }
        self.inventory.validate()
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessRuntimeInventoryV1 {
    pub enabled_providers: Vec<String>,
    pub workspaces: BTreeMap<String, HarnessRuntimeWorkspaceV1>,
    pub workspace_count: usize,
    pub workspaces_truncated: bool,
    pub session_count: usize,
    pub sessions_truncated: bool,
    pub managed_sessions: Vec<HarnessRuntimeManagedSessionV1>,
    pub managed_session_count: usize,
    pub managed_sessions_truncated: bool,
}

impl HarnessRuntimeInventoryV1 {
    pub fn validate(&self) -> Result<(), HarnessOperatorApiError> {
        let included_sessions = self.workspaces.values()
            .map(|workspace| workspace.sessions.len())
            .sum::<usize>();
        if self.enabled_providers.len() > 64
            || self.enabled_providers.windows(2).any(|pair| pair[0] >= pair[1])
            || self.enabled_providers.iter().any(|provider| !valid_runtime_id(provider, 128))
            || self.workspaces.len() > 32
            || included_sessions > 128
            || self.managed_sessions.len() > 128
            || self.workspace_count < self.workspaces.len()
            || self.session_count < included_sessions
            || self.managed_session_count < self.managed_sessions.len()
            || self.workspaces_truncated != (self.workspace_count > self.workspaces.len())
            || self.sessions_truncated != (self.session_count > included_sessions)
            || self.managed_sessions_truncated
                != (self.managed_session_count > self.managed_sessions.len())
            || self.managed_sessions.windows(2).any(|pair| pair[0].record_id >= pair[1].record_id)
        {
            return Err(HarnessOperatorApiError::InvalidRuntimeInventory);
        }
        for (workspace_id, workspace) in &self.workspaces {
            if workspace_id != &workspace.workspace_id { return Err(HarnessOperatorApiError::InvalidRuntimeInventory); }
            workspace.validate()?;
        }
        for record in &self.managed_sessions { record.validate()?; }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessRuntimeWorkspaceV1 {
    pub workspace_id: String,
    pub display_root: String,
    pub display_root_truncated: bool,
    pub sessions: Vec<HarnessRuntimeSessionV1>,
    pub session_count: usize,
    pub sessions_truncated: bool,
}

impl HarnessRuntimeWorkspaceV1 {
    fn validate(&self) -> Result<(), HarnessOperatorApiError> {
        if !valid_runtime_id(&self.workspace_id, 128)
            || self.display_root.len() > 1024
            || self.display_root.chars().any(char::is_control)
            || self.sessions.len() > 128
            || self.session_count < self.sessions.len()
            || self.sessions_truncated != (self.session_count > self.sessions.len())
            || self.sessions.windows(2).any(|pair| {
                (pair[0].instance_id, pair[0].generation)
                    >= (pair[1].instance_id, pair[1].generation)
            })
        {
            return Err(HarnessOperatorApiError::InvalidRuntimeInventory);
        }
        for session in &self.sessions { session.validate()?; }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HarnessRuntimeTransportV1 { Pty, Pipe, Acp }

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HarnessRuntimeSessionStatusV1 { Registered, Starting, Running, Stopping, Exited, Failed }

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessRuntimeTerminalSizeV1 {
    pub rows: u16,
    pub columns: u16,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessRuntimeSessionV1 {
    pub instance_id: u64,
    pub generation: u64,
    pub provider: String,
    pub transport: HarnessRuntimeTransportV1,
    pub status: HarnessRuntimeSessionStatusV1,
    pub process_id: Option<u32>,
    pub terminal_size: Option<HarnessRuntimeTerminalSizeV1>,
    pub operation_pending: bool,
    pub input_pending: bool,
}

impl HarnessRuntimeSessionV1 {
    fn validate(&self) -> Result<(), HarnessOperatorApiError> {
        if self.instance_id == 0
            || self.generation == 0
            || !valid_runtime_id(&self.provider, 128)
            || self.terminal_size.is_some_and(|size| size.rows == 0 || size.columns == 0)
        {
            return Err(HarnessOperatorApiError::InvalidRuntimeInventory);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HarnessRuntimeManagedModeV1 { Pty, Inline }

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HarnessRuntimeManagedStateV1 { IdentityPending, Live, Dormant, Unavailable }

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessRuntimeSessionBindingV1 {
    pub workspace_id: String,
    pub instance_id: u64,
    pub generation: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessRuntimeManagedSessionV1 {
    pub record_id: String,
    pub display_name: String,
    pub display_name_truncated: bool,
    pub provider: String,
    pub mode: HarnessRuntimeManagedModeV1,
    pub state: HarnessRuntimeManagedStateV1,
    pub workspace_id: String,
    pub active_binding: Option<HarnessRuntimeSessionBindingV1>,
    pub provider_identity_present: bool,
    pub updated_at_unix_ms: u64,
}

impl HarnessRuntimeManagedSessionV1 {
    fn validate(&self) -> Result<(), HarnessOperatorApiError> {
        if !valid_runtime_id(&self.record_id, 128)
            || !valid_runtime_id(&self.provider, 128)
            || !valid_runtime_id(&self.workspace_id, 128)
            || self.display_name.is_empty()
            || self.display_name.len() > 256
            || self.display_name.chars().any(char::is_control)
            || self.updated_at_unix_ms == 0
            || self.active_binding.as_ref().is_some_and(|binding| {
                !valid_runtime_id(&binding.workspace_id, 128)
                    || binding.instance_id == 0
                    || binding.generation == 0
            })
        {
            return Err(HarnessOperatorApiError::InvalidRuntimeInventory);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessRuntimeInventoryPageV1 {
    pub nodes: Vec<HarnessRuntimeNodeInventoryV1>,
    pub next_cursor: Option<String>,
}

impl HarnessRuntimeInventoryPageV1 {
    pub fn validate(&self) -> Result<(), HarnessOperatorApiError> {
        if self.nodes.len() > usize::from(HARNESS_RUNTIME_INVENTORY_PAGE_LIMIT_MAX)
            || self.nodes.windows(2).any(|pair| pair[0].node_id >= pair[1].node_id)
            || self.next_cursor.as_ref().is_some_and(|cursor| match self.nodes.last() {
                Some(last) => cursor != &last.node_id,
                None => true,
            })
        {
            return Err(HarnessOperatorApiError::InvalidRuntimeInventory);
        }
        for node in &self.nodes { node.validate()?; }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HarnessNativeSessionCatalogScopeV1 { Workspace, Unregistered }

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HarnessNativeSessionCatalogWindowV1 { Recent, Older }

#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessNativeSessionRouteV1 {
    pub node_id: String,
    pub incarnation_id: String,
    pub scope: HarnessNativeSessionCatalogScopeV1,
    pub workspace_id: Option<String>,
    pub provider: String,
}

impl HarnessNativeSessionRouteV1 {
    pub fn validate(&self) -> Result<(), HarnessOperatorApiError> {
        let scope_valid = matches!(
            (self.scope, self.workspace_id.as_ref()),
            (HarnessNativeSessionCatalogScopeV1::Workspace, Some(_))
                | (HarnessNativeSessionCatalogScopeV1::Unregistered, None)
        );
        if !scope_valid
            || !valid_runtime_id(&self.node_id, 128)
            || self.incarnation_id.len() != 32
            || !self.incarnation_id.bytes().all(is_lower_hex)
            || !valid_runtime_id(&self.provider, 128)
            || self.workspace_id.as_deref().is_some_and(|value| {
                !valid_runtime_id(value, 128)
            })
        {
            return Err(HarnessOperatorApiError::InvalidNativeHistory);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessNativeSessionSelectionV1 {
    pub route: HarnessNativeSessionRouteV1,
    pub catalog_revision: u64,
    pub recent_cutoff_unix_ms: u64,
    pub selection_id: String,
}

impl HarnessNativeSessionSelectionV1 {
    pub fn validate(&self) -> Result<(), HarnessOperatorApiError> {
        self.route.validate()?;
        if self.catalog_revision == 0 || !valid_native_selection_id(&self.selection_id) {
            return Err(HarnessOperatorApiError::InvalidNativeHistory);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HarnessNativeSessionExternalGroupKindV1 { Project, Global }

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessNativeSessionExternalGroupV1 {
    pub group_id: String,
    pub kind: HarnessNativeSessionExternalGroupKindV1,
    pub display_name: String,
}

impl HarnessNativeSessionExternalGroupV1 {
    fn validate(&self) -> Result<(), HarnessOperatorApiError> {
        if !valid_native_group_id(&self.group_id)
            || !valid_native_single_line(&self.display_name, 256, true)
            || matches!(self.display_name.as_str(), "." | "..")
            || self.display_name.contains('/')
            || self.display_name.contains('\\')
            || self.display_name.contains(':')
        {
            return Err(HarnessOperatorApiError::InvalidNativeHistory);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessNativeSessionCatalogEntryV1 {
    pub selection_id: String,
    pub title: Option<String>,
    pub modified_at_unix_ms: Option<u64>,
    pub model: Option<String>,
    pub message_count: u64,
    pub completed_turn_count: Option<u64>,
    pub external_group: Option<HarnessNativeSessionExternalGroupV1>,
    pub record_id: Option<String>,
}

impl HarnessNativeSessionCatalogEntryV1 {
    fn validate_for_route(
        &self,
        route: &HarnessNativeSessionRouteV1,
    ) -> Result<(), HarnessOperatorApiError> {
        if !valid_native_selection_id(&self.selection_id)
            || self.title.as_ref().is_some_and(|value| {
                !valid_native_single_line(value, 512, false)
            })
            || self.model.as_ref().is_some_and(|value| {
                !valid_native_single_line(value, 512, true)
            })
            || self.record_id.as_deref().is_some_and(|value| {
                !valid_runtime_id(value, 128)
            })
        {
            return Err(HarnessOperatorApiError::InvalidNativeHistory);
        }
        if let Some(group) = &self.external_group { group.validate()?; }
        let route_fields_valid = match route.scope {
            HarnessNativeSessionCatalogScopeV1::Workspace => self.external_group.is_none(),
            HarnessNativeSessionCatalogScopeV1::Unregistered => {
                self.external_group.is_some() && self.record_id.is_none()
            }
        };
        if !route_fields_valid {
            return Err(HarnessOperatorApiError::InvalidNativeHistory);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessNativeSessionCatalogSummaryV1 {
    pub catalog_revision: u64,
    pub recent_cutoff_unix_ms: u64,
    pub recent_total_count: u32,
    pub older_total_count: u32,
    pub recent_next_after_selection_id: Option<String>,
    pub recent_has_more: bool,
}

impl HarnessNativeSessionCatalogSummaryV1 {
    fn validate(&self, entry_count: usize) -> Result<(), HarnessOperatorApiError> {
        if self.catalog_revision == 0
            || self.recent_next_after_selection_id.as_deref().is_some_and(|value| {
                !valid_native_selection_id(value)
            })
            || self.recent_has_more != self.recent_next_after_selection_id.is_some()
            || usize::try_from(self.recent_total_count).map_or(true, |count| count < entry_count)
            || self.recent_has_more != (usize::try_from(self.recent_total_count)
                .unwrap_or(usize::MAX) > entry_count)
        {
            return Err(HarnessOperatorApiError::InvalidNativeHistory);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessNativeSessionsCatalogedV1 {
    pub route: HarnessNativeSessionRouteV1,
    pub entries: Vec<HarnessNativeSessionCatalogEntryV1>,
    pub summary: Option<HarnessNativeSessionCatalogSummaryV1>,
}

impl HarnessNativeSessionsCatalogedV1 {
    pub fn validate(&self) -> Result<(), HarnessOperatorApiError> {
        self.route.validate()?;
        validate_native_entries(&self.route, &self.entries)?;
        if let Some(summary) = &self.summary { summary.validate(self.entries.len())?; }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessNativeSessionCatalogPageV1 {
    pub window: HarnessNativeSessionCatalogWindowV1,
    pub revision: u64,
    pub entries: Vec<HarnessNativeSessionCatalogEntryV1>,
    pub next_after_selection_id: Option<String>,
    pub remaining_count: u32,
    pub has_more: bool,
}

impl HarnessNativeSessionCatalogPageV1 {
    fn validate_for_route(
        &self,
        route: &HarnessNativeSessionRouteV1,
    ) -> Result<(), HarnessOperatorApiError> {
        validate_native_entries(route, &self.entries)?;
        if self.revision == 0
            || self.next_after_selection_id.as_deref().is_some_and(|value| {
                !valid_native_selection_id(value)
            })
            || self.has_more != self.next_after_selection_id.is_some()
            || self.has_more != (self.remaining_count > 0)
        {
            return Err(HarnessOperatorApiError::InvalidNativeHistory);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessNativeSessionsPagedV1 {
    pub route: HarnessNativeSessionRouteV1,
    pub page: HarnessNativeSessionCatalogPageV1,
}

impl HarnessNativeSessionsPagedV1 {
    pub fn validate(&self) -> Result<(), HarnessOperatorApiError> {
        self.route.validate()?;
        self.page.validate_for_route(&self.route)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HarnessNativeSessionPreviewRoleV1 { User, Assistant }

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessNativeSessionPreviewMessageV1 {
    pub role: HarnessNativeSessionPreviewRoleV1,
    pub text: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessNativeSessionPreviewV1 {
    pub title: Option<String>,
    pub modified_at_unix_ms: Option<u64>,
    pub model: Option<String>,
    pub message_count: u64,
    pub message_count_exact: bool,
    pub completed_turn_count: Option<u64>,
    pub total_tokens: Option<u64>,
    pub truncated: bool,
    pub messages: Vec<HarnessNativeSessionPreviewMessageV1>,
}

impl HarnessNativeSessionPreviewV1 {
    fn validate(&self) -> Result<(), HarnessOperatorApiError> {
        if self.title.as_ref().is_some_and(|value| {
            !valid_native_single_line(value, 512, false)
        })
            || self.model.as_ref().is_some_and(|value| {
                !valid_native_single_line(value, 512, true)
            })
            || self.messages.len()
                > usize::from(HARNESS_NATIVE_SESSION_PREVIEW_MESSAGE_LIMIT_MAX)
            || self.messages.iter().any(|message| {
                message.text.len() > HARNESS_NATIVE_SESSION_PREVIEW_TEXT_MAX_BYTES
                    || message.text.chars().any(|character| {
                        character.is_control()
                            && !matches!(character, '\n' | '\r' | '\t')
                    })
            })
        {
            return Err(HarnessOperatorApiError::InvalidNativeHistory);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessNativeSessionPreviewedV1 {
    pub selection: HarnessNativeSessionSelectionV1,
    pub preview: HarnessNativeSessionPreviewV1,
}

impl HarnessNativeSessionPreviewedV1 {
    pub fn validate(&self) -> Result<(), HarnessOperatorApiError> {
        self.selection.validate()?;
        self.preview.validate()
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HarnessOperatorMutationOutcomeV1 {
    Applied,
    Replayed,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HarnessOperatorHostErrorV1 {
    InvalidRequest,
    Unauthorized,
    NotFound,
    Conflict,
    TooLarge,
    Deadline,
    Busy,
    Unavailable,
    Internal,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessReadEnvelopeV1 {
    pub version: u16,
    pub credential: HarnessReadCredential,
    pub request: HarnessReadRequestV1,
}

impl HarnessReadEnvelopeV1 {
    pub fn validate(&self) -> Result<(), HarnessReadApiError> {
        validate_version(self.version)?;
        validate_credential(self.credential.expose())?;
        self.request.validate()
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum HarnessReadRequestV1 {
    ContextGet,
    MonitorGet { run_id: Option<HarnessRunId> },
    TimelineRead {
        run_id: Option<HarnessRunId>,
        after_sequence: Option<u64>,
        limit: u16,
    },
    TasksList {
        after_task_id: Option<HarnessTaskId>,
        state: Option<HarnessTaskStateV1>,
        limit: u16,
    },
    TaskGet { task_id: HarnessTaskId },
    RunsList {
        task_id: Option<HarnessTaskId>,
        after_run_id: Option<HarnessRunId>,
        lifecycle: Option<HarnessRunLifecycleV1>,
        limit: u16,
    },
    RunGet { run_id: HarnessRunId },
    OperationGet { operation_id: HarnessOperationId },
}

impl HarnessReadRequestV1 {
    pub fn validate(&self) -> Result<(), HarnessReadApiError> {
        match self {
            Self::TimelineRead { run_id, after_sequence, limit } => {
                if let Some(run_id) = run_id {
                    run_id.validate().map_err(HarnessReadApiError::Protocol)?;
                }
                if after_sequence == &Some(0) {
                    return Err(HarnessReadApiError::InvalidCursor);
                }
                validate_limit(*limit, HARNESS_TIMELINE_PAGE_LIMIT_MAX)
            }
            Self::TasksList { after_task_id, limit, .. } => {
                if let Some(task_id) = after_task_id {
                    task_id.validate().map_err(HarnessReadApiError::Protocol)?;
                }
                validate_limit(*limit, HARNESS_ENTITY_PAGE_LIMIT_MAX)
            }
            Self::RunsList { task_id, after_run_id, limit, .. } => {
                if let Some(task_id) = task_id {
                    task_id.validate().map_err(HarnessReadApiError::Protocol)?;
                }
                if let Some(run_id) = after_run_id {
                    run_id.validate().map_err(HarnessReadApiError::Protocol)?;
                }
                validate_limit(*limit, HARNESS_ENTITY_PAGE_LIMIT_MAX)
            }
            Self::TaskGet { task_id } => task_id.validate().map_err(HarnessReadApiError::Protocol),
            Self::RunGet { run_id } => run_id.validate().map_err(HarnessReadApiError::Protocol),
            Self::OperationGet { operation_id } => {
                operation_id.validate().map_err(HarnessReadApiError::Protocol)
            }
            Self::MonitorGet { run_id } => {
                if let Some(run_id) = run_id {
                    run_id.validate().map_err(HarnessReadApiError::Protocol)?;
                }
                Ok(())
            }
            Self::ContextGet => Ok(()),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "kebab-case", deny_unknown_fields)]
pub enum HarnessReadReplyV1 {
    Ok { response: HarnessReadResponseV1 },
    Error { error: HarnessReadHostErrorV1 },
}

impl HarnessReadReplyV1 {
    pub fn validate(&self) -> Result<(), HarnessReadApiError> {
        match self {
            Self::Ok { response } => response.validate(),
            Self::Error { .. } => Ok(()),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", content = "value", rename_all = "kebab-case", deny_unknown_fields)]
pub enum HarnessReadResponseV1 {
    Context(SessionContextV1),
    Monitor(SessionMonitorV1),
    Timeline(TimelinePageV1),
    Tasks(TaskPageV1),
    Task(RedactedTaskV1),
    Runs(RunPageV1),
    Run(RedactedRunV1),
    Operation(RedactedOperationV1),
}

impl HarnessReadResponseV1 {
    pub fn validate(&self) -> Result<(), HarnessReadApiError> {
        match self {
            Self::Context(value) => value.validate(),
            Self::Monitor(value) => value.validate(),
            Self::Timeline(value) => value.validate(),
            Self::Tasks(value) => value.validate(),
            Self::Task(value) => value.validate(),
            Self::Runs(value) => value.validate(),
            Self::Run(value) => value.validate(),
            Self::Operation(value) => value.validate(),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HarnessReadHostErrorV1 {
    InvalidRequest,
    Unauthorized,
    NotFoundOrDenied,
    TooLarge,
    Deadline,
    Internal,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionContextV1 {
    pub grant_id: SessionGrantId,
    pub grant_revision: HarnessRevision,
    pub actor_run: CallerRunV1,
    pub read_permissions: HarnessReadPermissionsV1,
    pub monitoring_visibility: HarnessMonitoringVisibilityV1,
    pub maximum_child_count: u16,
    pub maximum_child_depth: u16,
    pub allowed_tool_ids: Vec<String>,
    pub history_message_count: Option<u64>,
    pub completed_turn_count: Option<u64>,
    pub total_tokens: Option<u64>,
}

impl SessionContextV1 {
    pub fn validate(&self) -> Result<(), HarnessReadApiError> {
        self.grant_id.validate().map_err(HarnessReadApiError::Protocol)?;
        self.grant_revision.validate().map_err(HarnessReadApiError::Protocol)?;
        self.actor_run.validate()?;
        self.read_permissions.validate().map_err(HarnessReadApiError::Protocol)?;
        if self.maximum_child_count > HARNESS_CHILD_COUNT_MAX
            || self.maximum_child_depth > HARNESS_CHILD_DEPTH_MAX
        {
            return Err(HarnessReadApiError::InvalidChildLimits);
        }
        if self.allowed_tool_ids.len() > HARNESS_READ_TOOL_IDS.len()
            || self.allowed_tool_ids.windows(2).any(|pair| pair[0] >= pair[1])
            || self.allowed_tool_ids.iter().any(|id| !HARNESS_READ_TOOL_IDS.contains(&id.as_str()))
            || self.allowed_tool_ids != expected_allowed_tool_ids(self)
        {
            return Err(HarnessReadApiError::InvalidAllowedTools);
        }
        Ok(())
    }
}

fn expected_allowed_tool_ids(context: &SessionContextV1) -> Vec<String> {
    let mut tools = vec!["g4a_context_get"];
    if context.monitoring_visibility != HarnessMonitoringVisibilityV1::None {
        tools.push("g4a_monitor_get");
    }
    if context.monitoring_visibility == HarnessMonitoringVisibilityV1::Timeline {
        tools.push("g4a_timeline_read");
    }
    if context.read_permissions.tasks != HarnessEntityReadScopeV1::None {
        tools.extend(["g4a_tasks_get", "g4a_tasks_list"]);
    }
    if context.read_permissions.runs != HarnessEntityReadScopeV1::None {
        tools.extend(["g4a_runs_get", "g4a_runs_list"]);
    }
    if context.read_permissions.operations != HarnessEntityReadScopeV1::None {
        tools.push("g4a_operation_get");
    }
    tools.sort_unstable();
    tools.into_iter().map(str::to_owned).collect()
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CallerRunV1 {
    pub run_id: HarnessRunId,
    pub task_id: Option<HarnessTaskId>,
    pub parent_run_id: Option<HarnessRunId>,
    pub lifecycle: HarnessRunLifecycleV1,
    pub references_redacted: bool,
}

impl CallerRunV1 {
    pub fn validate(&self) -> Result<(), HarnessReadApiError> {
        self.run_id.validate().map_err(HarnessReadApiError::Protocol)?;
        if let Some(task_id) = &self.task_id {
            task_id.validate().map_err(HarnessReadApiError::Protocol)?;
        }
        if let Some(parent_run_id) = &self.parent_run_id {
            parent_run_id.validate().map_err(HarnessReadApiError::Protocol)?;
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProjectionAvailabilityV1 {
    Unknown,
    NotObserved,
    Current,
    Partial,
    Frozen,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProjectionFreshnessV1 {
    Unavailable,
    Live,
    Stale,
    IncompleteAfterGap,
    LastKnown,
    ReplacedIncarnation,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum FeatureObservationStateV1 {
    Unknown,
    NotSupportedByObservedSources,
    SupportedNotObserved,
    Observed,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MonitorFeatureStatesV1 {
    pub todo: FeatureObservationStateV1,
    pub tools: FeatureObservationStateV1,
    pub subagents: FeatureObservationStateV1,
    pub interactions: FeatureObservationStateV1,
    pub owned_processes: FeatureObservationStateV1,
    pub files: FeatureObservationStateV1,
    pub usage: FeatureObservationStateV1,
    pub history: FeatureObservationStateV1,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionMonitorV1 {
    pub run_id: HarnessRunId,
    pub visibility: HarnessMonitoringVisibilityV1,
    pub availability: ProjectionAvailabilityV1,
    pub freshness: ProjectionFreshnessV1,
    pub transport_incomplete: bool,
    pub features: MonitorFeatureStatesV1,
    pub todo_total: u16,
    pub todo_completed: u16,
    pub active_tools: u16,
    pub active_subagents: u16,
    pub active_interactions: u16,
    pub active_processes: u16,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub reasoning_tokens: u64,
    pub context_window_tokens: Option<u64>,
    pub detail: Option<SessionMonitorDetailV1>,
}

impl SessionMonitorV1 {
    pub fn validate(&self) -> Result<(), HarnessReadApiError> {
        self.run_id.validate().map_err(HarnessReadApiError::Protocol)?;
        if self.todo_completed > self.todo_total {
            return Err(HarnessReadApiError::InvalidMonitorCounts);
        }
        let detail_allowed = matches!(
            self.visibility,
            HarnessMonitoringVisibilityV1::Detail | HarnessMonitoringVisibilityV1::Timeline
        );
        if self.detail.is_some() && !detail_allowed {
            return Err(HarnessReadApiError::InvalidMonitorDetail);
        }
        if matches!(self.availability, ProjectionAvailabilityV1::Unknown | ProjectionAvailabilityV1::NotObserved)
            && self.detail.is_some()
        {
            return Err(HarnessReadApiError::InvalidMonitorDetail);
        }
        if self.freshness == ProjectionFreshnessV1::IncompleteAfterGap
            && !self.transport_incomplete
        {
            return Err(HarnessReadApiError::InvalidMonitorDetail);
        }
        if let Some(detail) = &self.detail {
            detail.validate(&self.features)?;
        }
        validate_feature_counts(self)?;
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionMonitorDetailV1 {
    pub todo_facts: Vec<TodoFactV1>,
    pub tool_facts: Vec<ActivityFactV1>,
    pub subagent_facts: Vec<ActivityFactV1>,
    pub interaction_facts: Vec<InteractionFactV1>,
    pub process_facts: Vec<ActivityFactV1>,
    pub file_facts: Vec<FileFactV1>,
}

impl SessionMonitorDetailV1 {
    pub fn validate(&self, features: &MonitorFeatureStatesV1) -> Result<(), HarnessReadApiError> {
        validate_monitor_facts(&self.todo_facts)?;
        validate_monitor_facts(&self.tool_facts)?;
        validate_monitor_facts(&self.subagent_facts)?;
        validate_monitor_facts(&self.interaction_facts)?;
        validate_monitor_facts(&self.process_facts)?;
        validate_monitor_facts(&self.file_facts)?;
        if self.tool_facts.iter().any(|fact| fact.class != ActivityClassV1::Tool)
            || self.subagent_facts.iter().any(|fact| fact.class != ActivityClassV1::Subagent)
            || self.process_facts.iter().any(|fact| fact.class != ActivityClassV1::OwnedProcess)
        {
            return Err(HarnessReadApiError::InvalidMonitorDetail);
        }
        Ok(())
            .and_then(|_| require_observed_or_empty(features.todo, &self.todo_facts))
            .and_then(|_| require_observed_or_empty(features.tools, &self.tool_facts))
            .and_then(|_| require_observed_or_empty(features.subagents, &self.subagent_facts))
            .and_then(|_| require_observed_or_empty(features.interactions, &self.interaction_facts))
            .and_then(|_| require_observed_or_empty(features.owned_processes, &self.process_facts))
            .and_then(|_| require_observed_or_empty(features.files, &self.file_facts))
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TodoFactV1 {
    pub state: TodoStateV1,
    pub evidence: ObservationEvidenceV1,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum TodoStateV1 { Pending, InProgress, Completed, Unknown }

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ActivityFactV1 {
    pub class: ActivityClassV1,
    pub state: ActivityStateV1,
    pub evidence: ObservationEvidenceV1,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ActivityClassV1 { Tool, Subagent, OwnedProcess }

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ActivityStateV1 { Started, Active, Waiting, Completed, Failed, UnknownAfterGap }

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct InteractionFactV1 {
    pub class: InteractionClassV1,
    pub state: InteractionStateV1,
    pub evidence: ObservationEvidenceV1,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum InteractionClassV1 { Attention, Approval, UserInput }

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum InteractionStateV1 { Required, Responded, Dismissed, UnknownAfterGap }

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FileFactV1 {
    pub action: FileActionV1,
    pub evidence: ObservationEvidenceV1,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum FileActionV1 { Changed }

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ObservationEvidenceV1 {
    StructuredProvider,
    ManagedHook,
    WorkspaceObservation,
    NodeLifecycle,
    History,
    PtyHint,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TimelineEntryV1 {
    pub sequence: u64,
    pub received_at_ms: u64,
    pub category: TimelineCategoryV1,
    pub evidence: ObservationEvidenceV1,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TimelinePageV1 {
    pub run_id: HarnessRunId,
    pub availability: ProjectionAvailabilityV1,
    pub freshness: ProjectionFreshnessV1,
    pub transport_incomplete: bool,
    pub entries: Vec<TimelineEntryV1>,
    pub next_cursor: Option<u64>,
}

impl TimelinePageV1 {
    pub fn validate(&self) -> Result<(), HarnessReadApiError> {
        self.run_id.validate().map_err(HarnessReadApiError::Protocol)?;
        validate_bounded(&self.entries, HARNESS_TIMELINE_PAGE_LIMIT_MAX, TimelineEntryV1::validate)?;
        if self.entries.windows(2).any(|pair| pair[0].sequence >= pair[1].sequence)
            || self.next_cursor == Some(0)
        {
            return Err(HarnessReadApiError::InvalidCursor);
        }
        if self.next_cursor.is_some()
            && self.next_cursor != self.entries.last().map(|entry| entry.sequence)
        {
            return Err(HarnessReadApiError::InvalidCursor);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TaskPageV1 {
    pub tasks: Vec<RedactedTaskV1>,
    pub next_cursor: Option<HarnessTaskId>,
}

impl TaskPageV1 {
    pub fn validate(&self) -> Result<(), HarnessReadApiError> {
        validate_bounded(&self.tasks, HARNESS_ENTITY_PAGE_LIMIT_MAX, RedactedTaskV1::validate)?;
        if self.tasks.windows(2).any(|pair| pair[0].task_id >= pair[1].task_id)
            || self.next_cursor.as_ref() != self.tasks.last().map(|task| &task.task_id)
                && self.next_cursor.is_some()
        {
            return Err(HarnessReadApiError::InvalidCursor);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RunPageV1 {
    pub runs: Vec<RedactedRunV1>,
    pub next_cursor: Option<HarnessRunId>,
}

impl RunPageV1 {
    pub fn validate(&self) -> Result<(), HarnessReadApiError> {
        validate_bounded(&self.runs, HARNESS_ENTITY_PAGE_LIMIT_MAX, RedactedRunV1::validate)?;
        if self.runs.windows(2).any(|pair| pair[0].run_id >= pair[1].run_id)
            || self.next_cursor.as_ref() != self.runs.last().map(|run| &run.run_id)
                && self.next_cursor.is_some()
        {
            return Err(HarnessReadApiError::InvalidCursor);
        }
        Ok(())
    }
}

impl TimelineEntryV1 {
    pub fn validate(&self) -> Result<(), HarnessReadApiError> {
        if self.sequence == 0 || self.received_at_ms == 0 {
            return Err(HarnessReadApiError::InvalidTimelineEntry);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum TimelineCategoryV1 {
    Lifecycle,
    Todo,
    Tool,
    Subagent,
    Interaction,
    Process,
    File,
    Usage,
    History,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RedactedTaskV1 {
    pub task_id: HarnessTaskId,
    pub revision: HarnessRevision,
    pub title: String,
    pub body: String,
    pub creator: TaskCreatorCategoryV1,
    pub parent_task_id: Option<HarnessTaskId>,
    pub dependency_ids: Vec<HarnessTaskId>,
    pub state: HarnessTaskStateV1,
    pub run_ids: Vec<HarnessRunId>,
    pub references_redacted: bool,
    pub result_refs: Vec<HarnessResultRef>,
    pub artifact_refs: Vec<HarnessArtifactRef>,
    pub created_at_unix_ms: u64,
    pub updated_at_unix_ms: u64,
}

impl RedactedTaskV1 {
    pub fn validate(&self) -> Result<(), HarnessReadApiError> {
        self.task_id.validate().map_err(HarnessReadApiError::Protocol)?;
        self.revision.validate().map_err(HarnessReadApiError::Protocol)?;
        validate_text("title", &self.title, HARNESS_TITLE_MAX_BYTES, false)?;
        validate_text("body", &self.body, HARNESS_BODY_MAX_BYTES, true)?;
        if let Some(parent) = &self.parent_task_id {
            parent.validate().map_err(HarnessReadApiError::Protocol)?;
        }
        validate_bounded_ids_with_max(&self.dependency_ids, HARNESS_DEPENDENCIES_MAX)?;
        validate_bounded_ids_with_max(&self.run_ids, HARNESS_LINKS_MAX)?;
        validate_bounded_ids_with_max(&self.result_refs, HARNESS_RESULTS_MAX)?;
        validate_bounded_ids_with_max(&self.artifact_refs, HARNESS_ARTIFACTS_MAX)?;
        validate_timestamps(self.created_at_unix_ms, self.updated_at_unix_ms)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum TaskCreatorCategoryV1 { User, ParentRun }

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RedactedWorktreeIntentV1 { Existing, Managed }

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RedactedRunIntentV1 {
    pub mode: HarnessExecutionModeV1,
    pub worktree: RedactedWorktreeIntentV1,
    pub has_delivery_bundle: bool,
    pub has_continuation: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RedactedRunV1 {
    pub run_id: HarnessRunId,
    pub revision: HarnessRevision,
    pub parent_run_id: Option<HarnessRunId>,
    pub task_id: Option<HarnessTaskId>,
    pub operation_id: Option<HarnessOperationId>,
    pub intent: RedactedRunIntentV1,
    pub lifecycle: HarnessRunLifecycleV1,
    pub binding: RedactedBindingStateV1,
    pub result_disposition: Option<HarnessResultDispositionV1>,
    pub failure_category: Option<HarnessFailureCategoryV1>,
    pub references_redacted: bool,
    pub created_at_unix_ms: u64,
    pub updated_at_unix_ms: u64,
}

impl RedactedRunV1 {
    pub fn validate(&self) -> Result<(), HarnessReadApiError> {
        self.run_id.validate().map_err(HarnessReadApiError::Protocol)?;
        self.revision.validate().map_err(HarnessReadApiError::Protocol)?;
        if let Some(parent) = &self.parent_run_id {
            parent.validate().map_err(HarnessReadApiError::Protocol)?;
        }
        if let Some(task_id) = &self.task_id {
            task_id.validate().map_err(HarnessReadApiError::Protocol)?;
        }
        if let Some(operation_id) = &self.operation_id {
            operation_id.validate().map_err(HarnessReadApiError::Protocol)?;
        }
        if matches!(self.lifecycle, HarnessRunLifecycleV1::Running | HarnessRunLifecycleV1::Waiting)
            && self.binding == RedactedBindingStateV1::None
        {
            return Err(HarnessReadApiError::InvalidRunState);
        }
        if matches!(self.lifecycle, HarnessRunLifecycleV1::Failed) != self.failure_category.is_some() {
            return Err(HarnessReadApiError::InvalidRunState);
        }
        validate_timestamps(self.created_at_unix_ms, self.updated_at_unix_ms)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RedactedBindingStateV1 { None, ManagedDormant, ManagedActive, Inline }

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RedactedOperationV1 {
    pub operation_id: HarnessOperationId,
    pub revision: HarnessRevision,
    pub kind: HarnessOperationKindV1,
    pub state: HarnessOperationStateV1,
    pub task_id: Option<HarnessTaskId>,
    pub run_id: Option<HarnessRunId>,
    pub reconciles_operation_id: Option<HarnessOperationId>,
    pub references_redacted: bool,
    pub failure_category: Option<HarnessFailureCategoryV1>,
    pub outcome_unknown_reason: Option<HarnessOutcomeUnknownReasonV1>,
    pub reconciliation_outcome: Option<HarnessReconciliationOutcomeV1>,
    pub created_at_unix_ms: u64,
    pub updated_at_unix_ms: u64,
    pub dispatched_at_unix_ms: Option<u64>,
    pub finished_at_unix_ms: Option<u64>,
}

impl RedactedOperationV1 {
    pub fn validate(&self) -> Result<(), HarnessReadApiError> {
        self.operation_id.validate().map_err(HarnessReadApiError::Protocol)?;
        self.revision.validate().map_err(HarnessReadApiError::Protocol)?;
        if let Some(task_id) = &self.task_id {
            task_id.validate().map_err(HarnessReadApiError::Protocol)?;
        }
        if let Some(run_id) = &self.run_id {
            run_id.validate().map_err(HarnessReadApiError::Protocol)?;
        }
        if let Some(operation_id) = &self.reconciles_operation_id {
            operation_id.validate().map_err(HarnessReadApiError::Protocol)?;
        }
        if (self.state == HarnessOperationStateV1::Failed) != self.failure_category.is_some()
            || (self.state == HarnessOperationStateV1::OutcomeUnknown)
                != self.outcome_unknown_reason.is_some()
            || (self.state == HarnessOperationStateV1::Reconciled)
                != self.reconciliation_outcome.is_some()
        {
            return Err(HarnessReadApiError::InvalidOperationState);
        }
        validate_timestamps(self.created_at_unix_ms, self.updated_at_unix_ms)?;
        for timestamp in [self.dispatched_at_unix_ms, self.finished_at_unix_ms].into_iter().flatten() {
            if timestamp < self.created_at_unix_ms || timestamp > self.updated_at_unix_ms {
                return Err(HarnessReadApiError::InvalidTimestamp);
            }
        }
        Ok(())
    }
}

#[derive(Debug, Error)]
pub enum HarnessReadApiError {
    #[error("unsupported harness read wire version")]
    UnsupportedVersion,
    #[error("harness read credential is malformed")]
    MalformedCredential,
    #[error("harness read limit is outside the supported range")]
    InvalidLimit,
    #[error("harness read cursor is invalid")]
    InvalidCursor,
    #[error("harness read context has invalid allowed tools")]
    InvalidAllowedTools,
    #[error("harness monitor counts are inconsistent")]
    InvalidMonitorCounts,
    #[error("harness monitor detail is inconsistent with visibility or availability")]
    InvalidMonitorDetail,
    #[error("harness child limits are invalid")]
    InvalidChildLimits,
    #[error("harness timeline entry is invalid")]
    InvalidTimelineEntry,
    #[error("harness read collection is not canonical or bounded")]
    InvalidCollection,
    #[error("harness read timestamp is invalid")]
    InvalidTimestamp,
    #[error("harness redacted run state is invalid")]
    InvalidRunState,
    #[error("harness redacted operation state is invalid")]
    InvalidOperationState,
    #[error("harness read text field is invalid: {0}")]
    InvalidText(&'static str),
    #[error("harness protocol value is invalid: {0}")]
    Protocol(#[from] gate4agent_harness_protocol::HarnessValidationError),
}

#[derive(Debug, Error)]
pub enum HarnessOperatorApiError {
    #[error("unsupported harness operator wire version")]
    UnsupportedVersion,
    #[error("harness operator credential is malformed")]
    MalformedCredential,
    #[error("harness operator limit is outside the supported range")]
    InvalidLimit,
    #[error("harness operator cursor is invalid")]
    InvalidCursor,
    #[error("harness operator request reference is malformed")]
    MalformedRequestRef,
    #[error("harness operator submission timestamp is invalid")]
    InvalidSubmittedAt,
    #[error("harness runtime inventory is invalid")]
    InvalidRuntimeInventory,
    #[error("harness native history value is invalid")]
    InvalidNativeHistory,
    #[error("harness operator response is invalid")]
    Read(#[source] HarnessReadApiError),
    #[error("harness protocol value is invalid: {0}")]
    Protocol(#[source] gate4agent_harness_protocol::HarnessValidationError),
}

fn validate_version(version: u16) -> Result<(), HarnessReadApiError> {
    if version != HARNESS_READ_WIRE_VERSION_V1 {
        return Err(HarnessReadApiError::UnsupportedVersion);
    }
    Ok(())
}

fn validate_credential(value: &str) -> Result<(), HarnessReadApiError> {
    if value.len() > HARNESS_READ_CREDENTIAL_MAX_BYTES || !value.starts_with(TOKEN_PREFIX) {
        return Err(HarnessReadApiError::MalformedCredential);
    }
    let (payload, proof) = value[TOKEN_PREFIX.len()..]
        .split_once('.')
        .ok_or(HarnessReadApiError::MalformedCredential)?;
    if payload.is_empty()
        || payload.len() % 2 != 0
        || proof.len() != 64
        || !payload.bytes().chain(proof.bytes()).all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        return Err(HarnessReadApiError::MalformedCredential);
    }
    Ok(())
}

fn validate_operator_credential(value: &str) -> Result<(), HarnessOperatorApiError> {
    let payload = value.strip_prefix(OPERATOR_TOKEN_PREFIX)
        .ok_or(HarnessOperatorApiError::MalformedCredential)?;
    if value.len() > HARNESS_OPERATOR_CREDENTIAL_MAX_BYTES
        || payload.len() != 64
        || !payload.bytes().all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        return Err(HarnessOperatorApiError::MalformedCredential);
    }
    Ok(())
}

fn validate_operator_request_ref(value: &str) -> Result<(), HarnessOperatorApiError> {
    let payload = value.strip_prefix(OPERATOR_REQUEST_REF_PREFIX)
        .ok_or(HarnessOperatorApiError::MalformedRequestRef)?;
    if payload.len() != 24
        || !payload.bytes().all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        return Err(HarnessOperatorApiError::MalformedRequestRef);
    }
    Ok(())
}

fn valid_runtime_id(value: &str, maximum: usize) -> bool {
    !value.is_empty()
        && value.len() <= maximum
        && value.is_ascii()
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'@' | b'+')
        })
}

fn is_lower_hex(byte: u8) -> bool {
    byte.is_ascii_digit() || matches!(byte, b'a'..=b'f')
}

fn validate_native_session_catalog_limit(limit: u16) -> Result<(), HarnessOperatorApiError> {
    if !(1..=HARNESS_NATIVE_SESSION_CATALOG_LIMIT_MAX).contains(&limit) {
        return Err(HarnessOperatorApiError::InvalidLimit);
    }
    Ok(())
}

fn valid_native_selection_id(value: &str) -> bool {
    !value.trim().is_empty()
        && value.len() <= 1_024
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')
        })
}

fn valid_native_group_id(value: &str) -> bool {
    !value.trim().is_empty()
        && value.len() <= 128
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')
        })
}

fn valid_native_single_line(value: &str, maximum: usize, required: bool) -> bool {
    (!required || !value.trim().is_empty())
        && value.len() <= maximum
        && !value.chars().any(char::is_control)
}

fn validate_native_entries(
    route: &HarnessNativeSessionRouteV1,
    entries: &[HarnessNativeSessionCatalogEntryV1],
) -> Result<(), HarnessOperatorApiError> {
    if entries.len() > usize::from(HARNESS_NATIVE_SESSION_CATALOG_LIMIT_MAX) {
        return Err(HarnessOperatorApiError::InvalidNativeHistory);
    }
    for (index, entry) in entries.iter().enumerate() {
        entry.validate_for_route(route)?;
        if entries[..index].iter().any(|existing| {
            existing.selection_id == entry.selection_id
                || entry.record_id.is_some() && existing.record_id == entry.record_id
        }) {
            return Err(HarnessOperatorApiError::InvalidNativeHistory);
        }
    }
    Ok(())
}

fn validate_operator_limit(limit: u16) -> Result<(), HarnessOperatorApiError> {
    if !(1..=HARNESS_ENTITY_PAGE_LIMIT_MAX).contains(&limit) {
        return Err(HarnessOperatorApiError::InvalidLimit);
    }
    Ok(())
}

fn validate_operator_timeline_limit(limit: u16) -> Result<(), HarnessOperatorApiError> {
    if !(1..=HARNESS_TIMELINE_PAGE_LIMIT_MAX).contains(&limit) {
        return Err(HarnessOperatorApiError::InvalidLimit);
    }
    Ok(())
}

fn validate_limit(limit: u16, maximum: u16) -> Result<(), HarnessReadApiError> {
    if !(1..=maximum).contains(&limit) {
        return Err(HarnessReadApiError::InvalidLimit);
    }
    Ok(())
}

fn validate_bounded<T>(
    values: &[T],
    maximum: u16,
    validate: impl Fn(&T) -> Result<(), HarnessReadApiError>,
) -> Result<(), HarnessReadApiError> {
    if values.len() > maximum as usize {
        return Err(HarnessReadApiError::InvalidCollection);
    }
    for value in values { validate(value)?; }
    Ok(())
}

fn validate_bounded_ids_with_max<T: Ord>(
    values: &[T],
    maximum: usize,
) -> Result<(), HarnessReadApiError> {
    if values.len() > maximum
        || values.windows(2).any(|pair| pair[0] >= pair[1])
    {
        return Err(HarnessReadApiError::InvalidCollection);
    }
    Ok(())
}

fn validate_monitor_facts<T>(values: &[T]) -> Result<(), HarnessReadApiError> {
    if values.len() > HARNESS_MONITOR_FACTS_MAX {
        return Err(HarnessReadApiError::InvalidCollection);
    }
    Ok(())
}

fn require_observed_or_empty<T>(
    state: FeatureObservationStateV1,
    values: &[T],
) -> Result<(), HarnessReadApiError> {
    if state != FeatureObservationStateV1::Observed && !values.is_empty() {
        return Err(HarnessReadApiError::InvalidMonitorDetail);
    }
    Ok(())
}

fn validate_feature_counts(monitor: &SessionMonitorV1) -> Result<(), HarnessReadApiError> {
    if monitor.features.todo != FeatureObservationStateV1::Observed
        && (monitor.todo_total != 0 || monitor.todo_completed != 0)
        || monitor.features.tools != FeatureObservationStateV1::Observed
            && monitor.active_tools != 0
        || monitor.features.subagents != FeatureObservationStateV1::Observed
            && monitor.active_subagents != 0
        || monitor.features.interactions != FeatureObservationStateV1::Observed
            && monitor.active_interactions != 0
        || monitor.features.owned_processes != FeatureObservationStateV1::Observed
            && monitor.active_processes != 0
        || monitor.features.usage != FeatureObservationStateV1::Observed
            && (monitor.input_tokens != 0
                || monitor.output_tokens != 0
                || monitor.cache_read_tokens != 0
                || monitor.cache_write_tokens != 0
                || monitor.reasoning_tokens != 0
                || monitor.context_window_tokens.is_some())
    {
        return Err(HarnessReadApiError::InvalidMonitorDetail);
    }
    if matches!(monitor.availability, ProjectionAvailabilityV1::Unknown | ProjectionAvailabilityV1::NotObserved)
        && [
            monitor.features.todo,
            monitor.features.tools,
            monitor.features.subagents,
            monitor.features.interactions,
            monitor.features.owned_processes,
            monitor.features.files,
            monitor.features.usage,
            monitor.features.history,
        ]
        .contains(&FeatureObservationStateV1::Observed)
    {
        return Err(HarnessReadApiError::InvalidMonitorDetail);
    }
    Ok(())
}

fn validate_text(
    field: &'static str,
    value: &str,
    maximum: usize,
    allow_empty: bool,
) -> Result<(), HarnessReadApiError> {
    if value.len() > maximum || !allow_empty && value.trim().is_empty() || value.contains('\0') {
        return Err(HarnessReadApiError::InvalidText(field));
    }
    Ok(())
}

fn validate_timestamps(created: u64, updated: u64) -> Result<(), HarnessReadApiError> {
    if created == 0 || updated < created {
        return Err(HarnessReadApiError::InvalidTimestamp);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credential_debug_and_errors_never_expose_secret() {
        let credential = HarnessReadCredential::parse(format!("g4ah2_aa.{}", "0".repeat(64)))
            .expect("credential");
        assert_eq!(format!("{credential:?}"), "HarnessReadCredential([REDACTED])");
        assert!(!HarnessReadApiError::MalformedCredential.to_string().contains(credential.expose()));
    }

    #[test]
    fn operator_and_agent_credentials_are_strictly_separated_and_redacted() {
        let operator = HarnessOperatorCredential::parse(format!(
            "g4aho_{}",
            "a".repeat(64),
        )).expect("operator credential");
        let read = format!("g4ah2_aa.{}", "0".repeat(64));
        assert!(HarnessOperatorCredential::parse(&read).is_err());
        assert!(HarnessReadCredential::parse(operator.expose()).is_err());
        assert_eq!(
            format!("{operator:?}"),
            "HarnessOperatorCredential([REDACTED])",
        );
        assert!(!HarnessOperatorApiError::MalformedCredential
            .to_string().contains(operator.expose()));
    }

    #[test]
    fn operator_frames_enforce_exact_cursor_and_collection_bounds() {
        assert!(HarnessOperatorRequestV1::TasksList {
            after_task_id: None,
            state: None,
            limit: HARNESS_ENTITY_PAGE_LIMIT_MAX,
        }.validate().is_ok());
        assert!(HarnessOperatorRequestV1::TasksList {
            after_task_id: None,
            state: None,
            limit: HARNESS_ENTITY_PAGE_LIMIT_MAX + 1,
        }.validate().is_err());
        let run_id = HarnessRunId::new(format!("hrun_{}", "a".repeat(24))).unwrap();
        assert!(HarnessOperatorRequestV1::TimelineRead {
            run_id,
            after_sequence: Some(0),
            limit: 1,
        }.validate().is_err());
    }

    #[test]
    fn operator_v2_rejects_v1_wire_and_legacy_raw_schedule_variant() {
        let credential = HarnessOperatorCredential::parse(format!(
            "g4aho_{}",
            "a".repeat(64),
        )).unwrap();
        let envelope = HarnessOperatorEnvelopeV1 {
            version: HARNESS_OPERATOR_WIRE_VERSION_V1,
            credential,
            request: HarnessOperatorRequestV1::TasksList {
                after_task_id: None,
                state: None,
                limit: 1,
            },
        };
        assert!(matches!(
            envelope.validate(),
            Err(HarnessOperatorApiError::UnsupportedVersion),
        ));
        let legacy = r#"{"kind":"schedule-ready-task","request":{}}"#;
        assert!(serde_json::from_str::<HarnessOperatorRequestV1>(legacy).is_err());
    }

    #[test]
    fn operator_v3_intent_is_authority_free_and_fails_closed_on_v2() {
        let credential = HarnessOperatorCredential::parse(format!(
            "g4aho_{}",
            "a".repeat(64),
        )).unwrap();
        let request = HarnessOperatorRequestV1::SubmitIntent {
            intent: HarnessOperatorIntentV1 {
                request_ref: HarnessOperatorRequestRefV1::new(format!(
                    "hireq_{}",
                    "1".repeat(24),
                )).unwrap(),
                submitted_at_unix_ms: 10,
                action: HarnessOperatorActionV1::CreateTask {
                    title: "Harness-owned identity".to_owned(),
                    body: "Typed user intent".to_owned(),
                    parent_task_id: None,
                    dependencies: Vec::new(),
                    initial_state: HarnessTaskStateV1::Backlog,
                },
            },
        };
        let encoded_request = serde_json::to_string(&request).unwrap();
        assert!(!encoded_request.contains("authority"));
        assert!(!encoded_request.contains("operation_id"));
        assert!(!encoded_request.contains("idempotency_ref"));
        assert!(!encoded_request.contains("\"task_id\":"));
        let v2 = HarnessOperatorEnvelopeV1 {
            version: HARNESS_OPERATOR_WIRE_VERSION_V2,
            credential: credential.clone(),
            request: request.clone(),
        };
        assert!(matches!(
            v2.validate(),
            Err(HarnessOperatorApiError::UnsupportedVersion),
        ));
        let v3 = HarnessOperatorEnvelopeV1 {
            version: HARNESS_OPERATOR_WIRE_VERSION_V3,
            credential,
            request,
        };
        assert!(v3.validate().is_ok());
        let decoded: HarnessOperatorEnvelopeV1 = serde_json::from_slice(
            &serde_json::to_vec(&v3).unwrap(),
        ).unwrap();
        assert_eq!(decoded, v3);
    }

    #[test]
    fn runtime_inventory_request_requires_v3() {
        let credential = HarnessOperatorCredential::parse(format!(
            "g4aho_{}",
            "a".repeat(64),
        )).unwrap();
        let request = HarnessOperatorRequestV1::RuntimeInventoryList {
            after_node_id: None,
            limit: HARNESS_RUNTIME_INVENTORY_PAGE_LIMIT_MAX,
        };
        let v2 = HarnessOperatorEnvelopeV1 {
            version: HARNESS_OPERATOR_WIRE_VERSION_V2,
            credential: credential.clone(),
            request: request.clone(),
        };
        assert!(matches!(
            v2.validate(),
            Err(HarnessOperatorApiError::UnsupportedVersion),
        ));
        assert!(HarnessOperatorEnvelopeV1 {
            version: HARNESS_OPERATOR_WIRE_VERSION_V3,
            credential,
            request,
        }.validate().is_ok());
    }

    #[test]
    fn native_history_v3_is_exact_bounded_and_excludes_sensitive_fields() {
        let credential = HarnessOperatorCredential::parse(format!(
            "g4aho_{}",
            "a".repeat(64),
        )).unwrap();
        let route = HarnessNativeSessionRouteV1 {
            node_id: "node-a".to_owned(),
            incarnation_id: "1".repeat(32),
            scope: HarnessNativeSessionCatalogScopeV1::Workspace,
            workspace_id: Some("workspace-a".to_owned()),
            provider: "codex".to_owned(),
        };
        let selection = HarnessNativeSessionSelectionV1 {
            route: route.clone(),
            catalog_revision: 7,
            recent_cutoff_unix_ms: 9,
            selection_id: "selection-a".to_owned(),
        };
        for request in [
            HarnessOperatorRequestV1::CatalogNativeSessions {
                route: route.clone(),
                limit: HARNESS_NATIVE_SESSION_CATALOG_LIMIT_MAX,
            },
            HarnessOperatorRequestV1::PageNativeSessions {
                route: route.clone(),
                window: HarnessNativeSessionCatalogWindowV1::Recent,
                catalog_revision: 7,
                recent_cutoff_unix_ms: 9,
                after_selection_id: Some("selection-a".to_owned()),
                limit: 1,
            },
            HarnessOperatorRequestV1::PreviewNativeSession {
                selection: selection.clone(),
                message_limit: HARNESS_NATIVE_SESSION_PREVIEW_MESSAGE_LIMIT_MAX,
            },
        ] {
            assert!(request.requires_v3());
            assert!(HarnessOperatorEnvelopeV1 {
                version: HARNESS_OPERATOR_WIRE_VERSION_V2,
                credential: credential.clone(),
                request: request.clone(),
            }.validate().is_err());
            HarnessOperatorEnvelopeV1 {
                version: HARNESS_OPERATOR_WIRE_VERSION_V3,
                credential: credential.clone(),
                request,
            }.validate().unwrap();
        }
        let response = HarnessOperatorResponseV1::NativeSessionPreviewed(
            HarnessNativeSessionPreviewedV1 {
                selection,
                preview: HarnessNativeSessionPreviewV1 {
                    title: Some("Bounded preview".to_owned()),
                    modified_at_unix_ms: Some(10),
                    model: Some("model-a".to_owned()),
                    message_count: 1,
                    message_count_exact: true,
                    completed_turn_count: Some(1),
                    total_tokens: Some(12),
                    truncated: false,
                    messages: vec![HarnessNativeSessionPreviewMessageV1 {
                        role: HarnessNativeSessionPreviewRoleV1::User,
                        text: "bounded history preview".to_owned(),
                    }],
                },
            },
        );
        response.validate().unwrap();
        let encoded = serde_json::to_string(&response).unwrap();
        for forbidden in [
            "g4aho_",
            "credential",
            "provider_identity",
            "session_id",
            "terminal",
            "cwd",
        ] {
            assert!(!encoded.contains(forbidden));
        }
    }

    #[test]
    fn unknown_request_fields_and_unbounded_limits_fail_closed() {
        let unknown = r#"{"kind":"tasks-list","limit":10,"cursor":"secret"}"#;
        assert!(serde_json::from_str::<HarnessReadRequestV1>(unknown).is_err());
        assert!(HarnessReadRequestV1::TasksList { after_task_id: None, state: None, limit: 0 }.validate().is_err());
        assert!(HarnessReadRequestV1::TasksList { after_task_id: None, state: None, limit: 65 }.validate().is_err());
    }

    #[test]
    fn redacted_cross_entity_references_do_not_require_hidden_ids() {
        let redacted_run = r#"{
            "run_id":"hrun_000000000000000000000001",
            "revision":1,
            "parent_run_id":null,
            "task_id":null,
            "operation_id":null,
            "intent":{"mode":"inline","worktree":"existing","has_delivery_bundle":false,"has_continuation":false},
            "lifecycle":"requested",
            "binding":"none",
            "result_disposition":null,
            "failure_category":null,
            "references_redacted":true,
            "created_at_unix_ms":1,
            "updated_at_unix_ms":1
        }"#;
        let run: RedactedRunV1 = serde_json::from_str(redacted_run).expect("redacted run");
        run.validate().expect("valid redacted run");

        let operation_with_grant = r#"{
            "operation_id":"hop_000000000000000000000001",
            "revision":1,
            "kind":"create-task",
            "state":"prepared",
            "task_id":null,
            "run_id":null,
            "grant_id":"hgrant_000000000000000000000001",
            "reconciles_operation_id":null,
            "references_redacted":true,
            "failure_category":null,
            "outcome_unknown_reason":null,
            "reconciliation_outcome":null,
            "created_at_unix_ms":1,
            "updated_at_unix_ms":1,
            "dispatched_at_unix_ms":null,
            "finished_at_unix_ms":null
        }"#;
        assert!(serde_json::from_str::<RedactedOperationV1>(operation_with_grant).is_err());
    }

    fn monitor_with_mixed_capabilities() -> SessionMonitorV1 {
        SessionMonitorV1 {
            run_id: HarnessRunId::new("hrun_000000000000000000000001").unwrap(),
            visibility: HarnessMonitoringVisibilityV1::Detail,
            availability: ProjectionAvailabilityV1::Frozen,
            freshness: ProjectionFreshnessV1::ReplacedIncarnation,
            transport_incomplete: false,
            features: MonitorFeatureStatesV1 {
                todo: FeatureObservationStateV1::Observed,
                tools: FeatureObservationStateV1::NotSupportedByObservedSources,
                subagents: FeatureObservationStateV1::Unknown,
                interactions: FeatureObservationStateV1::SupportedNotObserved,
                owned_processes: FeatureObservationStateV1::Observed,
                files: FeatureObservationStateV1::Observed,
                usage: FeatureObservationStateV1::SupportedNotObserved,
                history: FeatureObservationStateV1::Unknown,
            },
            todo_total: 1,
            todo_completed: 0,
            active_tools: 0,
            active_subagents: 0,
            active_interactions: 0,
            active_processes: 0,
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            reasoning_tokens: 0,
            context_window_tokens: None,
            detail: Some(SessionMonitorDetailV1 {
                todo_facts: vec![TodoFactV1 {
                    state: TodoStateV1::Unknown,
                    evidence: ObservationEvidenceV1::PtyHint,
                }],
                tool_facts: Vec::new(),
                subagent_facts: Vec::new(),
                interaction_facts: Vec::new(),
                process_facts: vec![ActivityFactV1 {
                    class: ActivityClassV1::OwnedProcess,
                    state: ActivityStateV1::UnknownAfterGap,
                    evidence: ObservationEvidenceV1::ManagedHook,
                }],
                file_facts: vec![FileFactV1 {
                    action: FileActionV1::Changed,
                    evidence: ObservationEvidenceV1::StructuredProvider,
                }],
            }),
        }
    }

    #[test]
    fn monitor_preserves_mixed_capability_and_frozen_replaced_truth() {
        let monitor = monitor_with_mixed_capabilities();
        monitor.validate().expect("truthful mixed monitor");
        let encoded = serde_json::to_string(&monitor).expect("encode monitor");
        let decoded: SessionMonitorV1 = serde_json::from_str(&encoded).expect("decode monitor");
        assert_eq!(decoded, monitor);
        assert_eq!(decoded.availability, ProjectionAvailabilityV1::Frozen);
        assert_eq!(decoded.freshness, ProjectionFreshnessV1::ReplacedIncarnation);
        assert_eq!(decoded.features.tools, FeatureObservationStateV1::NotSupportedByObservedSources);
        assert_eq!(decoded.detail.unwrap().todo_facts[0].state, TodoStateV1::Unknown);
    }

    #[test]
    fn monitor_rejects_inference_and_raw_detail_sentinels() {
        let mut monitor = monitor_with_mixed_capabilities();
        monitor.detail.as_mut().unwrap().tool_facts.push(ActivityFactV1 {
            class: ActivityClassV1::Tool,
            state: ActivityStateV1::Active,
            evidence: ObservationEvidenceV1::StructuredProvider,
        });
        assert!(monitor.validate().is_err());

        let raw_file = r#"{"action":"changed","evidence":"managed-hook","path":"C:\\private\\prompt.txt"}"#;
        assert!(serde_json::from_str::<FileFactV1>(raw_file).is_err());
        let encoded = serde_json::to_string(&monitor_with_mixed_capabilities()).unwrap();
        for sentinel in ["private", "prompt.txt", "provider_id", "correlation_id"] {
            assert!(!encoded.contains(sentinel));
        }

        let mut partial = monitor_with_mixed_capabilities();
        partial.availability = ProjectionAvailabilityV1::Partial;
        partial.freshness = ProjectionFreshnessV1::IncompleteAfterGap;
        partial.transport_incomplete = true;
        partial.validate().expect("truthful partial gap");
    }
}
