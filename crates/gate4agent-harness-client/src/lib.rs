//! Typed localhost client for the harness read host.
//!
//! The client implements the wire contract as one newline-terminated JSON
//! request per loopback connection, followed by `Shutdown::Write`; the host
//! replies only after that EOF request boundary.

use std::{
    io::{BufRead, BufReader, Write},
    net::{Shutdown, SocketAddr, TcpStream},
    time::Duration,
};

pub use gate4agent_harness_api::*;
use thiserror::Error;

pub const HARNESS_READ_DEADLINE: Duration = Duration::from_secs(3);
pub const HARNESS_OPERATOR_DEADLINE: Duration = Duration::from_secs(3);
pub const HARNESS_RUN_WORKSPACE_READ_DEADLINE: Duration = Duration::from_secs(14);
pub const HARNESS_NATIVE_HISTORY_DEADLINE: Duration = Duration::from_secs(42);
pub const HARNESS_CONTEXT_SOURCE_OBSERVATION_DEADLINE: Duration = Duration::from_secs(14);

#[derive(Clone, Debug)]
pub struct HarnessReadClient {
    endpoint: SocketAddr,
    credential: HarnessReadCredential,
    deadline: Duration,
}

impl HarnessReadClient {
    pub fn new(
        endpoint: SocketAddr,
        credential: HarnessReadCredential,
    ) -> Result<Self, HarnessReadClientError> {
        if !endpoint.ip().is_loopback() || endpoint.port() == 0 {
            return Err(HarnessReadClientError::NonLoopbackEndpoint);
        }
        Ok(Self { endpoint, credential, deadline: HARNESS_READ_DEADLINE })
    }

    pub fn context_get(&self) -> Result<SessionContextV1, HarnessReadClientError> {
        match self.send(HarnessReadRequestV1::ContextGet)? {
            HarnessReadResponseV1::Context(value) => Ok(value),
            _ => Err(HarnessReadClientError::UnexpectedResponse),
        }
    }

    pub fn monitor_get(
        &self,
        run_id: Option<HarnessRunId>,
    ) -> Result<SessionMonitorV1, HarnessReadClientError> {
        let expected_run_id = run_id.clone();
        match self.send(HarnessReadRequestV1::MonitorGet { run_id })? {
            HarnessReadResponseV1::Monitor(value) => {
                if let Some(expected_run_id) = expected_run_id.as_ref() {
                    value.validate_for(expected_run_id)?;
                }
                Ok(value)
            }
            _ => Err(HarnessReadClientError::UnexpectedResponse),
        }
    }

    pub fn timeline_read(
        &self,
        run_id: Option<HarnessRunId>,
        after_sequence: Option<u64>,
        limit: u16,
    ) -> Result<TimelinePageV1, HarnessReadClientError> {
        let expected_run_id = run_id.clone();
        match self.send(HarnessReadRequestV1::TimelineRead { run_id, after_sequence, limit })? {
            HarnessReadResponseV1::Timeline(value) => {
                if let Some(expected_run_id) = expected_run_id.as_ref() {
                    value.validate_for(expected_run_id)?;
                }
                Ok(value)
            }
            _ => Err(HarnessReadClientError::UnexpectedResponse),
        }
    }

    pub fn tasks_list(
        &self,
        after_task_id: Option<HarnessTaskId>,
        state: Option<HarnessTaskStateV1>,
        limit: u16,
    ) -> Result<TaskPageV1, HarnessReadClientError> {
        match self.send(HarnessReadRequestV1::TasksList { after_task_id, state, limit })? {
            HarnessReadResponseV1::Tasks(value) => Ok(value),
            _ => Err(HarnessReadClientError::UnexpectedResponse),
        }
    }

    pub fn task_get(&self, task_id: HarnessTaskId) -> Result<RedactedTaskV1, HarnessReadClientError> {
        match self.send(HarnessReadRequestV1::TaskGet { task_id })? {
            HarnessReadResponseV1::Task(value) => Ok(value),
            _ => Err(HarnessReadClientError::UnexpectedResponse),
        }
    }

    pub fn runs_list(
        &self,
        task_id: Option<HarnessTaskId>,
        after_run_id: Option<HarnessRunId>,
        lifecycle: Option<HarnessRunLifecycleV1>,
        limit: u16,
    ) -> Result<RunPageV1, HarnessReadClientError> {
        match self.send(HarnessReadRequestV1::RunsList { task_id, after_run_id, lifecycle, limit })? {
            HarnessReadResponseV1::Runs(value) => Ok(value),
            _ => Err(HarnessReadClientError::UnexpectedResponse),
        }
    }

    pub fn run_get(&self, run_id: HarnessRunId) -> Result<RedactedRunV1, HarnessReadClientError> {
        match self.send(HarnessReadRequestV1::RunGet { run_id })? {
            HarnessReadResponseV1::Run(value) => Ok(value),
            _ => Err(HarnessReadClientError::UnexpectedResponse),
        }
    }

    pub fn operation_get(
        &self,
        operation_id: HarnessOperationId,
    ) -> Result<RedactedOperationV1, HarnessReadClientError> {
        match self.send(HarnessReadRequestV1::OperationGet { operation_id })? {
            HarnessReadResponseV1::Operation(value) => Ok(value),
            _ => Err(HarnessReadClientError::UnexpectedResponse),
        }
    }

    fn send(&self, request: HarnessReadRequestV1) -> Result<HarnessReadResponseV1, HarnessReadClientError> {
        request.validate()?;
        let envelope = HarnessReadEnvelopeV1 {
            version: HARNESS_READ_WIRE_VERSION_V1,
            credential: self.credential.clone(),
            request,
        };
        envelope.validate()?;
        let mut encoded = serde_json::to_vec(&envelope).map_err(|_| HarnessReadClientError::Encoding)?;
        if encoded.len() >= HARNESS_READ_REQUEST_MAX_BYTES {
            return Err(HarnessReadClientError::RequestTooLarge);
        }
        encoded.push(b'\n');

        let mut stream = TcpStream::connect_timeout(&self.endpoint, self.deadline)
            .map_err(map_connect_error)?;
        stream.set_read_timeout(Some(self.deadline)).map_err(|_| HarnessReadClientError::Transport)?;
        stream.set_write_timeout(Some(self.deadline)).map_err(|_| HarnessReadClientError::Transport)?;
        stream.write_all(&encoded).map_err(map_io_error)?;
        stream.shutdown(Shutdown::Write).map_err(|_| HarnessReadClientError::Transport)?;

        let mut reader = BufReader::new(stream);
        let response = read_bounded_line(&mut reader, HARNESS_READ_RESPONSE_MAX_BYTES)?;
        let reply: HarnessReadReplyV1 =
            serde_json::from_slice(&response).map_err(|_| HarnessReadClientError::InvalidResponse)?;
        reply.validate()?;
        match reply {
            HarnessReadReplyV1::Ok { response } => Ok(response),
            HarnessReadReplyV1::Error { error } => Err(HarnessReadClientError::Host(error)),
        }
    }
}

#[derive(Clone, Debug)]
pub struct HarnessOperatorClient {
    endpoint: SocketAddr,
    credential: HarnessOperatorCredential,
    deadline: Duration,
}

impl HarnessOperatorClient {
    pub fn new(
        endpoint: SocketAddr,
        credential: HarnessOperatorCredential,
    ) -> Result<Self, HarnessOperatorClientError> {
        if !endpoint.ip().is_loopback() || endpoint.port() == 0 {
            return Err(HarnessOperatorClientError::NonLoopbackEndpoint);
        }
        Ok(Self { endpoint, credential, deadline: HARNESS_OPERATOR_DEADLINE })
    }

    pub fn monitor_get(
        &self,
        run_id: HarnessRunId,
    ) -> Result<SessionMonitorV1, HarnessOperatorClientError> {
        let expected_run_id = run_id.clone();
        match self.send(HarnessOperatorRequestV1::MonitorGet { run_id })? {
            HarnessOperatorResponseV1::Monitor(value) => {
                value.validate_for(&expected_run_id)
                    .map_err(HarnessOperatorApiError::Read)?;
                Ok(value)
            }
            _ => Err(HarnessOperatorClientError::UnexpectedResponse),
        }
    }

    pub fn timeline_read(
        &self,
        run_id: HarnessRunId,
        after_sequence: Option<u64>,
        limit: u16,
    ) -> Result<TimelinePageV1, HarnessOperatorClientError> {
        let expected_run_id = run_id.clone();
        match self.send(HarnessOperatorRequestV1::TimelineRead {
            run_id,
            after_sequence,
            limit,
        })? {
            HarnessOperatorResponseV1::Timeline(value) => {
                value.validate_for(&expected_run_id)
                    .map_err(HarnessOperatorApiError::Read)?;
                Ok(value)
            }
            _ => Err(HarnessOperatorClientError::UnexpectedResponse),
        }
    }

    pub fn terminal_read(
        &self,
        session: HarnessRuntimeSessionAddressV1,
        after_sequence: Option<u64>,
        limit: u16,
    ) -> Result<HarnessRuntimeTerminalPageV1, HarnessOperatorClientError> {
        let expected_session = session.clone();
        match self.send(HarnessOperatorRequestV1::TerminalRead {
            session,
            after_sequence,
            limit,
        })? {
            HarnessOperatorResponseV1::TerminalRead(value) => {
                value.validate_for(&expected_session)?;
                Ok(value)
            }
            _ => Err(HarnessOperatorClientError::UnexpectedResponse),
        }
    }

    pub fn tasks_list(
        &self,
        after_task_id: Option<HarnessTaskId>,
        state: Option<HarnessTaskStateV1>,
        limit: u16,
    ) -> Result<TaskPageV1, HarnessOperatorClientError> {
        match self.send(HarnessOperatorRequestV1::TasksList {
            after_task_id,
            state,
            limit,
        })? {
            HarnessOperatorResponseV1::Tasks(value) => Ok(value),
            _ => Err(HarnessOperatorClientError::UnexpectedResponse),
        }
    }

    pub fn task_get(
        &self,
        task_id: HarnessTaskId,
    ) -> Result<RedactedTaskV1, HarnessOperatorClientError> {
        match self.send(HarnessOperatorRequestV1::TaskGet { task_id })? {
            HarnessOperatorResponseV1::Task(value) => Ok(value),
            _ => Err(HarnessOperatorClientError::UnexpectedResponse),
        }
    }

    pub fn runs_list(
        &self,
        task_id: Option<HarnessTaskId>,
        after_run_id: Option<HarnessRunId>,
        lifecycle: Option<HarnessRunLifecycleV1>,
        limit: u16,
    ) -> Result<RunPageV1, HarnessOperatorClientError> {
        match self.send(HarnessOperatorRequestV1::RunsList {
            task_id,
            after_run_id,
            lifecycle,
            limit,
        })? {
            HarnessOperatorResponseV1::Runs(value) => Ok(value),
            _ => Err(HarnessOperatorClientError::UnexpectedResponse),
        }
    }

    pub fn run_get(
        &self,
        run_id: HarnessRunId,
    ) -> Result<RedactedRunV1, HarnessOperatorClientError> {
        match self.send(HarnessOperatorRequestV1::RunGet { run_id })? {
            HarnessOperatorResponseV1::Run(value) => Ok(value),
            _ => Err(HarnessOperatorClientError::UnexpectedResponse),
        }
    }

    pub fn run_correlation_get(
        &self,
        run_id: HarnessRunId,
    ) -> Result<HarnessRunCorrelationV1, HarnessOperatorClientError> {
        match self.send(HarnessOperatorRequestV1::RunCorrelationGet { run_id })? {
            HarnessOperatorResponseV1::RunCorrelation(value) => Ok(value),
            _ => Err(HarnessOperatorClientError::UnexpectedResponse),
        }
    }

    pub fn run_transfer_get(
        &self,
        run_id: HarnessRunId,
    ) -> Result<HarnessRunTransferSummaryV1, HarnessOperatorClientError> {
        let expected_run_id = run_id.clone();
        match self.send(HarnessOperatorRequestV1::RunTransferGet { run_id })? {
            HarnessOperatorResponseV1::RunTransfer(value) => {
                value.validate_for(&expected_run_id)?;
                Ok(value)
            }
            _ => Err(HarnessOperatorClientError::UnexpectedResponse),
        }
    }

    pub fn reverse_attribution_get(
        &self,
        subject: HarnessReverseAttributionSubjectV1,
    ) -> Result<HarnessReverseAttributionV1, HarnessOperatorClientError> {
        let expected_subject = subject.clone();
        match self.send(HarnessOperatorRequestV1::ReverseAttributionGet { subject })? {
            HarnessOperatorResponseV1::ReverseAttribution(value) => {
                value.validate_for(&expected_subject)?;
                Ok(value)
            }
            _ => Err(HarnessOperatorClientError::UnexpectedResponse),
        }
    }

    pub fn observe_run_context_source(
        &self,
        run_id: HarnessRunId,
    ) -> Result<HarnessRunContextSourceObservationV1, HarnessOperatorClientError> {
        let expected_run_id = run_id.clone();
        match self.send(HarnessOperatorRequestV1::ObserveRunContextSource { run_id })? {
            HarnessOperatorResponseV1::RunContextSourceObserved(value) => {
                value.validate_for(&expected_run_id)?;
                Ok(value)
            }
            _ => Err(HarnessOperatorClientError::UnexpectedResponse),
        }
    }

    pub fn inspect_run_workspace(
        &self,
        run_id: HarnessRunId,
    ) -> Result<HarnessRunWorkspaceInspectionV1, HarnessOperatorClientError> {
        let expected_run_id = run_id.clone();
        match self.send(HarnessOperatorRequestV1::InspectRunWorkspace { run_id })? {
            HarnessOperatorResponseV1::RunWorkspaceInspected(value) => {
                value.validate_for(&expected_run_id)?;
                Ok(value)
            }
            _ => Err(HarnessOperatorClientError::UnexpectedResponse),
        }
    }

    pub fn read_run_workspace_file(
        &self,
        run_id: HarnessRunId,
        path: HarnessRepositoryPathV1,
    ) -> Result<HarnessRunWorkspaceFileV1, HarnessOperatorClientError> {
        let expected_run_id = run_id.clone();
        let expected_path = path.clone();
        match self.send(HarnessOperatorRequestV1::ReadRunWorkspaceFile { run_id, path })? {
            HarnessOperatorResponseV1::RunWorkspaceFileRead(value) => {
                value.validate_for(&expected_run_id, &expected_path)?;
                Ok(value)
            }
            _ => Err(HarnessOperatorClientError::UnexpectedResponse),
        }
    }

    pub fn read_run_git_history(
        &self,
        run_id: HarnessRunId,
        path: Option<HarnessRepositoryPathV1>,
        before: Option<HarnessGitObjectIdV1>,
        limit: u16,
    ) -> Result<HarnessRunGitHistoryPageV1, HarnessOperatorClientError> {
        let expected_run_id = run_id.clone();
        let expected_path = path.clone();
        match self.send(HarnessOperatorRequestV1::ReadRunGitHistory {
            run_id,
            path,
            before,
            limit,
        })? {
            HarnessOperatorResponseV1::RunGitHistoryRead(value) => {
                value.validate_for(&expected_run_id, expected_path.as_ref(), limit)?;
                Ok(value)
            }
            _ => Err(HarnessOperatorClientError::UnexpectedResponse),
        }
    }

    pub fn read_run_git_diff(
        &self,
        run_id: HarnessRunId,
        mode: HarnessGitDiffModeV1,
        path: Option<HarnessRepositoryPathV1>,
    ) -> Result<HarnessRunGitDiffV1, HarnessOperatorClientError> {
        let expected_run_id = run_id.clone();
        let expected_mode = mode.clone();
        let expected_path = path.clone();
        match self.send(HarnessOperatorRequestV1::ReadRunGitDiff { run_id, mode, path })? {
            HarnessOperatorResponseV1::RunGitDiffRead(value) => {
                value.validate_for(
                    &expected_run_id,
                    &expected_mode,
                    expected_path.as_ref(),
                )?;
                Ok(value)
            }
            _ => Err(HarnessOperatorClientError::UnexpectedResponse),
        }
    }

    /// Node-scoped sibling of `inspect_run_workspace`: needs no run in
    /// flight, only a node/workspace pair.
    pub fn inspect_node_workspace(
        &self,
        node_id: String,
        workspace_id: String,
    ) -> Result<HarnessNodeWorkspaceInspectionV1, HarnessOperatorClientError> {
        let expected_node_id = node_id.clone();
        let expected_workspace_id = workspace_id.clone();
        match self.send(HarnessOperatorRequestV1::InspectNodeWorkspace {
            node_id,
            workspace_id,
        })? {
            HarnessOperatorResponseV1::NodeWorkspaceInspected(value) => {
                value.validate_for(&expected_node_id, &expected_workspace_id)?;
                Ok(value)
            }
            _ => Err(HarnessOperatorClientError::UnexpectedResponse),
        }
    }

    /// Node-scoped sibling of `read_run_workspace_file`.
    pub fn read_node_workspace_file(
        &self,
        node_id: String,
        workspace_id: String,
        path: HarnessRepositoryPathV1,
    ) -> Result<HarnessNodeWorkspaceFileV1, HarnessOperatorClientError> {
        let expected_node_id = node_id.clone();
        let expected_workspace_id = workspace_id.clone();
        let expected_path = path.clone();
        match self.send(HarnessOperatorRequestV1::ReadNodeWorkspaceFile {
            node_id,
            workspace_id,
            path,
        })? {
            HarnessOperatorResponseV1::NodeWorkspaceFileRead(value) => {
                value.validate_for(&expected_node_id, &expected_workspace_id, &expected_path)?;
                Ok(value)
            }
            _ => Err(HarnessOperatorClientError::UnexpectedResponse),
        }
    }

    /// Node-scoped sibling of `read_run_git_history`.
    pub fn read_node_git_history(
        &self,
        node_id: String,
        workspace_id: String,
        path: Option<HarnessRepositoryPathV1>,
        before: Option<HarnessGitObjectIdV1>,
        limit: u16,
    ) -> Result<HarnessNodeGitHistoryPageV1, HarnessOperatorClientError> {
        let expected_node_id = node_id.clone();
        let expected_workspace_id = workspace_id.clone();
        let expected_path = path.clone();
        match self.send(HarnessOperatorRequestV1::ReadNodeGitHistory {
            node_id,
            workspace_id,
            path,
            before,
            limit,
        })? {
            HarnessOperatorResponseV1::NodeGitHistoryRead(value) => {
                value.validate_for(
                    &expected_node_id,
                    &expected_workspace_id,
                    expected_path.as_ref(),
                    limit,
                )?;
                Ok(value)
            }
            _ => Err(HarnessOperatorClientError::UnexpectedResponse),
        }
    }

    /// Node-scoped sibling of `read_run_git_diff`.
    pub fn read_node_git_diff(
        &self,
        node_id: String,
        workspace_id: String,
        mode: HarnessGitDiffModeV1,
        path: Option<HarnessRepositoryPathV1>,
    ) -> Result<HarnessNodeGitDiffV1, HarnessOperatorClientError> {
        let expected_node_id = node_id.clone();
        let expected_workspace_id = workspace_id.clone();
        let expected_mode = mode.clone();
        let expected_path = path.clone();
        match self.send(HarnessOperatorRequestV1::ReadNodeGitDiff {
            node_id,
            workspace_id,
            mode,
            path,
        })? {
            HarnessOperatorResponseV1::NodeGitDiffRead(value) => {
                value.validate_for(
                    &expected_node_id,
                    &expected_workspace_id,
                    &expected_mode,
                    expected_path.as_ref(),
                )?;
                Ok(value)
            }
            _ => Err(HarnessOperatorClientError::UnexpectedResponse),
        }
    }

    pub fn launch_plans_list(
        &self,
        after_plan_id: Option<HarnessSelectorV1>,
        limit: u16,
    ) -> Result<HarnessLaunchPlanPageV1, HarnessOperatorClientError> {
        match self.send(HarnessOperatorRequestV1::LaunchPlansList {
            after_plan_id,
            limit,
        })? {
            HarnessOperatorResponseV1::LaunchPlans(value) => Ok(value),
            _ => Err(HarnessOperatorClientError::UnexpectedResponse),
        }
    }

    pub fn task_execution_spec_get(
        &self,
        task_id: HarnessTaskId,
    ) -> Result<Option<HarnessTaskExecutionSpecV1>, HarnessOperatorClientError> {
        match self.send(HarnessOperatorRequestV1::TaskExecutionSpecGet { task_id })? {
            HarnessOperatorResponseV1::TaskExecutionSpec(value) => Ok(value),
            _ => Err(HarnessOperatorClientError::UnexpectedResponse),
        }
    }

    pub fn task_launch_options_get(
        &self,
        task_id: HarnessTaskId,
    ) -> Result<HarnessTaskLaunchOptionsV1, HarnessOperatorClientError> {
        let expected_task_id = task_id.clone();
        match self.send(HarnessOperatorRequestV1::TaskLaunchOptionsGet { task_id })? {
            HarnessOperatorResponseV1::TaskLaunchOptions(value) => {
                value.validate_for(&expected_task_id)?;
                Ok(value)
            }
            _ => Err(HarnessOperatorClientError::UnexpectedResponse),
        }
    }

    pub fn runtime_inventory_list(
        &self,
        after_node_id: Option<String>,
        limit: u16,
    ) -> Result<HarnessRuntimeInventoryPageV1, HarnessOperatorClientError> {
        match self.send(HarnessOperatorRequestV1::RuntimeInventoryList {
            after_node_id,
            limit,
        })? {
            HarnessOperatorResponseV1::RuntimeInventory(value) => Ok(value),
            _ => Err(HarnessOperatorClientError::UnexpectedResponse),
        }
    }

    pub fn catalog_native_sessions(
        &self,
        route: HarnessNativeSessionRouteV1,
        limit: u16,
    ) -> Result<HarnessNativeSessionsCatalogedV1, HarnessOperatorClientError> {
        match self.send(HarnessOperatorRequestV1::CatalogNativeSessions { route, limit })? {
            HarnessOperatorResponseV1::NativeSessionsCataloged(value) => Ok(value),
            _ => Err(HarnessOperatorClientError::UnexpectedResponse),
        }
    }

    pub fn page_native_sessions(
        &self,
        route: HarnessNativeSessionRouteV1,
        window: HarnessNativeSessionCatalogWindowV1,
        catalog_revision: u64,
        recent_cutoff_unix_ms: u64,
        after_selection_id: Option<String>,
        limit: u16,
    ) -> Result<HarnessNativeSessionsPagedV1, HarnessOperatorClientError> {
        match self.send(HarnessOperatorRequestV1::PageNativeSessions {
            route,
            window,
            catalog_revision,
            recent_cutoff_unix_ms,
            after_selection_id,
            limit,
        })? {
            HarnessOperatorResponseV1::NativeSessionsPaged(value) => Ok(value),
            _ => Err(HarnessOperatorClientError::UnexpectedResponse),
        }
    }

    pub fn preview_native_session(
        &self,
        selection: HarnessNativeSessionSelectionV1,
        message_limit: u16,
    ) -> Result<HarnessNativeSessionPreviewedV1, HarnessOperatorClientError> {
        match self.send(HarnessOperatorRequestV1::PreviewNativeSession {
            selection,
            message_limit,
        })? {
            HarnessOperatorResponseV1::NativeSessionPreviewed(value) => Ok(value),
            _ => Err(HarnessOperatorClientError::UnexpectedResponse),
        }
    }

    pub fn submit_intent(
        &self,
        intent: HarnessOperatorIntentV1,
    ) -> Result<HarnessOperatorResponseV1, HarnessOperatorClientError> {
        self.send(HarnessOperatorRequestV1::SubmitIntent { intent })
    }

    pub fn create_task(
        &self,
        request: HarnessCreateTaskRequestV1,
    ) -> Result<HarnessOperatorMutationOutcomeV1, HarnessOperatorClientError> {
        self.send_mutation(HarnessOperatorRequestV1::CreateTask { request })
    }

    pub fn replace_task(
        &self,
        request: HarnessReplaceTaskRequestV1,
    ) -> Result<HarnessOperatorMutationOutcomeV1, HarnessOperatorClientError> {
        self.send_mutation(HarnessOperatorRequestV1::ReplaceTask { request })
    }

    pub fn move_task(
        &self,
        request: HarnessMoveTaskRequestV1,
    ) -> Result<HarnessOperatorMutationOutcomeV1, HarnessOperatorClientError> {
        self.send_mutation(HarnessOperatorRequestV1::MoveTask { request })
    }

    pub fn cancel_task(
        &self,
        request: HarnessCancelTaskRequestV1,
    ) -> Result<HarnessOperatorMutationOutcomeV1, HarnessOperatorClientError> {
        self.send_mutation(HarnessOperatorRequestV1::CancelTask { request })
    }

    pub fn retry_task(
        &self,
        request: HarnessRetryTaskRequestV1,
    ) -> Result<HarnessOperatorMutationOutcomeV1, HarnessOperatorClientError> {
        self.send_mutation(HarnessOperatorRequestV1::RetryTask { request })
    }

    pub fn schedule_next(
        &self,
        request: HarnessScheduleNextRequestV1,
    ) -> Result<HarnessScheduleOutcomeV1, HarnessOperatorClientError> {
        match self.send(HarnessOperatorRequestV1::ScheduleNext { request })? {
            HarnessOperatorResponseV1::Schedule(value) => Ok(value),
            _ => Err(HarnessOperatorClientError::UnexpectedResponse),
        }
    }

    pub fn replace_task_execution_spec(
        &self,
        request: HarnessReplaceTaskExecutionSpecRequestV1,
    ) -> Result<HarnessOperatorMutationOutcomeV1, HarnessOperatorClientError> {
        match self.send(HarnessOperatorRequestV1::ReplaceTaskExecutionSpec { request })? {
            HarnessOperatorResponseV1::ExecutionSpecMutation(value) => Ok(value),
            _ => Err(HarnessOperatorClientError::UnexpectedResponse),
        }
    }

    pub fn start_task(
        &self,
        request: HarnessStartTaskRequestV1,
    ) -> Result<HarnessTaskStartOutcomeV1, HarnessOperatorClientError> {
        match self.send(HarnessOperatorRequestV1::StartTask { request })? {
            HarnessOperatorResponseV1::TaskStarted(value) => Ok(value),
            _ => Err(HarnessOperatorClientError::UnexpectedResponse),
        }
    }

    pub fn replace_task_execution_spec_v2(
        &self,
        request: HarnessReplaceTaskExecutionSpecRequestV2,
    ) -> Result<HarnessOperatorMutationOutcomeV1, HarnessOperatorClientError> {
        match self.send(HarnessOperatorRequestV1::ReplaceTaskExecutionSpecV2 { request })? {
            HarnessOperatorResponseV1::ExecutionSpecMutation(value) => Ok(value),
            _ => Err(HarnessOperatorClientError::UnexpectedResponse),
        }
    }

    pub fn start_task_v2(
        &self,
        request: HarnessStartTaskRequestV2,
    ) -> Result<HarnessTaskStartOutcomeV1, HarnessOperatorClientError> {
        let expected_task_id = request.task_id.clone();
        let expected_operation_id = request.authority.operation_id.clone();
        let expected_idempotency_ref = request.authority.idempotency_ref.clone();
        match self.send(HarnessOperatorRequestV1::StartTaskV2 { request })? {
            HarnessOperatorResponseV1::TaskStarted(value) => {
                if value.dispatch.task_id != expected_task_id
                    || value.dispatch.operation_id != expected_operation_id
                    || value.dispatch.idempotency_ref != expected_idempotency_ref
                {
                    return Err(HarnessOperatorClientError::Api(
                        HarnessOperatorApiError::InvalidTaskLaunchSelection,
                    ));
                }
                Ok(value)
            }
            _ => Err(HarnessOperatorClientError::UnexpectedResponse),
        }
    }

    fn send_mutation(
        &self,
        request: HarnessOperatorRequestV1,
    ) -> Result<HarnessOperatorMutationOutcomeV1, HarnessOperatorClientError> {
        match self.send(request)? {
            HarnessOperatorResponseV1::Mutation(value) => Ok(value),
            _ => Err(HarnessOperatorClientError::UnexpectedResponse),
        }
    }

    fn send(
        &self,
        request: HarnessOperatorRequestV1,
    ) -> Result<HarnessOperatorResponseV1, HarnessOperatorClientError> {
        let response_deadline = if matches!(
            &request,
            HarnessOperatorRequestV1::ObserveRunContextSource { .. }
        ) {
            HARNESS_CONTEXT_SOURCE_OBSERVATION_DEADLINE
        } else if matches!(
            &request,
            HarnessOperatorRequestV1::CatalogNativeSessions { .. }
                | HarnessOperatorRequestV1::PageNativeSessions { .. }
                | HarnessOperatorRequestV1::PreviewNativeSession { .. }
        ) {
            HARNESS_NATIVE_HISTORY_DEADLINE
        } else if matches!(
            &request,
            HarnessOperatorRequestV1::InspectRunWorkspace { .. }
                | HarnessOperatorRequestV1::ReadRunWorkspaceFile { .. }
                | HarnessOperatorRequestV1::ReadRunGitHistory { .. }
                | HarnessOperatorRequestV1::ReadRunGitDiff { .. }
                | HarnessOperatorRequestV1::InspectNodeWorkspace { .. }
                | HarnessOperatorRequestV1::ReadNodeWorkspaceFile { .. }
                | HarnessOperatorRequestV1::ReadNodeGitHistory { .. }
                | HarnessOperatorRequestV1::ReadNodeGitDiff { .. }
        ) {
            HARNESS_RUN_WORKSPACE_READ_DEADLINE
        } else {
            self.deadline
        };
        request.validate()?;
        let version = request.minimum_wire_version();
        let envelope = HarnessOperatorEnvelopeV1 {
            version,
            credential: self.credential.clone(),
            request,
        };
        envelope.validate()?;
        let mut encoded = serde_json::to_vec(&envelope)
            .map_err(|_| HarnessOperatorClientError::Encoding)?;
        if encoded.len() >= HARNESS_OPERATOR_REQUEST_MAX_BYTES {
            return Err(HarnessOperatorClientError::RequestTooLarge);
        }
        encoded.push(b'\n');

        let mut stream = TcpStream::connect_timeout(&self.endpoint, self.deadline)
            .map_err(map_operator_connect_error)?;
        stream.set_read_timeout(Some(response_deadline))
            .map_err(|_| HarnessOperatorClientError::Transport)?;
        stream.set_write_timeout(Some(self.deadline))
            .map_err(|_| HarnessOperatorClientError::Transport)?;
        stream.write_all(&encoded).map_err(map_operator_io_error)?;
        stream.shutdown(Shutdown::Write)
            .map_err(|_| HarnessOperatorClientError::Transport)?;

        let mut reader = BufReader::new(stream);
        let response = read_operator_bounded_line(
            &mut reader,
            HARNESS_OPERATOR_RESPONSE_MAX_BYTES,
        )?;
        let reply: HarnessOperatorReplyV1 = serde_json::from_slice(&response)
            .map_err(|_| HarnessOperatorClientError::InvalidResponse)?;
        reply.validate()?;
        match reply {
            HarnessOperatorReplyV1::Ok { response } => Ok(response),
            HarnessOperatorReplyV1::Error { error } => {
                Err(HarnessOperatorClientError::Host(error))
            }
        }
    }
}

fn read_operator_bounded_line(
    reader: &mut impl BufRead,
    max_bytes: usize,
) -> Result<Vec<u8>, HarnessOperatorClientError> {
    let mut response = Vec::with_capacity(4096);
    loop {
        let available = reader.fill_buf().map_err(map_operator_io_error)?;
        if available.is_empty() {
            return Err(if response.is_empty() {
                HarnessOperatorClientError::ConnectionClosed
            } else {
                HarnessOperatorClientError::IncompleteResponse
            });
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let consumed = newline.map_or(available.len(), |position| position + 1);
        if response.len().saturating_add(consumed) > max_bytes {
            return Err(HarnessOperatorClientError::ResponseTooLarge);
        }
        response.extend_from_slice(&available[..consumed]);
        reader.consume(consumed);
        if newline.is_some() { break; }
    }
    response.pop();
    if response.last() == Some(&b'\r') { response.pop(); }
    if response.is_empty() {
        return Err(HarnessOperatorClientError::InvalidResponse);
    }
    Ok(response)
}

fn map_operator_connect_error(error: std::io::Error) -> HarnessOperatorClientError {
    if matches!(error.kind(), std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock) {
        HarnessOperatorClientError::Deadline
    } else {
        HarnessOperatorClientError::Unavailable
    }
}

fn map_operator_io_error(error: std::io::Error) -> HarnessOperatorClientError {
    if matches!(error.kind(), std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock) {
        HarnessOperatorClientError::Deadline
    } else {
        HarnessOperatorClientError::Transport
    }
}

#[derive(Debug, Error)]
pub enum HarnessOperatorClientError {
    #[error("harness operator endpoint must be a concrete loopback address")]
    NonLoopbackEndpoint,
    #[error("harness operator request is invalid")]
    Api(#[from] HarnessOperatorApiError),
    #[error("harness operator request encoding failed")]
    Encoding,
    #[error("harness operator request is too large")]
    RequestTooLarge,
    #[error("harness operator response is too large")]
    ResponseTooLarge,
    #[error("harness operator response is incomplete")]
    IncompleteResponse,
    #[error("harness operator response is invalid")]
    InvalidResponse,
    #[error("harness operator response type is unexpected")]
    UnexpectedResponse,
    #[error("harness operator host closed the connection")]
    ConnectionClosed,
    #[error("harness operator deadline exceeded")]
    Deadline,
    #[error("harness operator host is unavailable")]
    Unavailable,
    #[error("harness operator transport failed")]
    Transport,
    #[error("harness operator host rejected the request: {0:?}")]
    Host(HarnessOperatorHostErrorV1),
}

fn read_bounded_line(
    reader: &mut impl BufRead,
    max_bytes: usize,
) -> Result<Vec<u8>, HarnessReadClientError> {
    let mut response = Vec::with_capacity(4096);
    loop {
        let available = reader.fill_buf().map_err(map_io_error)?;
        if available.is_empty() {
            return Err(if response.is_empty() {
                HarnessReadClientError::ConnectionClosed
            } else {
                HarnessReadClientError::IncompleteResponse
            });
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let consumed = newline.map_or(available.len(), |position| position + 1);
        if response.len().saturating_add(consumed) > max_bytes {
            return Err(HarnessReadClientError::ResponseTooLarge);
        }
        response.extend_from_slice(&available[..consumed]);
        reader.consume(consumed);
        if newline.is_some() { break; }
    }
    response.pop();
    if response.last() == Some(&b'\r') { response.pop(); }
    if response.is_empty() {
        return Err(HarnessReadClientError::InvalidResponse);
    }
    Ok(response)
}

fn map_connect_error(error: std::io::Error) -> HarnessReadClientError {
    if matches!(error.kind(), std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock) {
        HarnessReadClientError::Deadline
    } else {
        HarnessReadClientError::Unavailable
    }
}

fn map_io_error(error: std::io::Error) -> HarnessReadClientError {
    if matches!(error.kind(), std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock) {
        HarnessReadClientError::Deadline
    } else {
        HarnessReadClientError::Transport
    }
}

#[derive(Debug, Error)]
pub enum HarnessReadClientError {
    #[error("harness read endpoint must be a concrete loopback address")]
    NonLoopbackEndpoint,
    #[error("harness read request is invalid")]
    Api(#[from] HarnessReadApiError),
    #[error("harness read request encoding failed")]
    Encoding,
    #[error("harness read request is too large")]
    RequestTooLarge,
    #[error("harness read response is too large")]
    ResponseTooLarge,
    #[error("harness read response is incomplete")]
    IncompleteResponse,
    #[error("harness read response is invalid")]
    InvalidResponse,
    #[error("harness read response type is unexpected")]
    UnexpectedResponse,
    #[error("harness read host closed the connection")]
    ConnectionClosed,
    #[error("harness read deadline exceeded")]
    Deadline,
    #[error("harness read host is unavailable")]
    Unavailable,
    #[error("harness read transport failed")]
    Transport,
    #[error("harness read host rejected the request: {0:?}")]
    Host(HarnessReadHostErrorV1),
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{io::Read, net::TcpListener, thread};

    fn credential() -> HarnessReadCredential {
        HarnessReadCredential::parse(format!("g4ah2_aa.{}", "0".repeat(64))).expect("credential")
    }

    fn operator_credential() -> HarnessOperatorCredential {
        HarnessOperatorCredential::parse(format!("g4aho_{}", "a".repeat(64)))
            .expect("operator credential")
    }

    fn run_transfer_summary(run_id: HarnessRunId) -> HarnessRunTransferSummaryV1 {
        HarnessRunTransferSummaryV1 {
            run_id,
            run_revision: HarnessRevision::new(5).unwrap(),
            delivery: None,
            continuation: None,
        }
    }

    fn context_source_observation(
        run_id: HarnessRunId,
    ) -> HarnessRunContextSourceObservationV1 {
        HarnessRunContextSourceObservationV1 {
            run_id,
            run_revision: HarnessRevision::new(8).unwrap(),
            feature_state: FeatureObservationStateV1::Observed,
            message_count: 17,
            message_count_exact: true,
            completed_turn_count: Some(6),
            total_tokens: Some(4_096),
            observed_at_unix_ms: Some(1_000),
        }
    }

    fn reverse_attribution_workspace() -> HarnessReverseAttributionWorkspaceV1 {
        HarnessReverseAttributionWorkspaceV1 {
            node_id: HarnessSelectorV1::new("node-a").unwrap(),
            node_incarnation_id: HarnessNodeIncarnationV1::new("07".repeat(16)).unwrap(),
            workspace_id: HarnessSelectorV1::new("workspace-a").unwrap(),
        }
    }

    fn empty_monitor(run_id: HarnessRunId) -> SessionMonitorV1 {
        SessionMonitorV1 {
            run_id,
            visibility: HarnessMonitoringVisibilityV1::Timeline,
            availability: ProjectionAvailabilityV1::Unknown,
            freshness: ProjectionFreshnessV1::Unavailable,
            transport_incomplete: false,
            features: MonitorFeatureStatesV1 {
                todo: FeatureObservationStateV1::Unknown,
                tools: FeatureObservationStateV1::Unknown,
                subagents: FeatureObservationStateV1::Unknown,
                interactions: FeatureObservationStateV1::Unknown,
                owned_processes: FeatureObservationStateV1::Unknown,
                files: FeatureObservationStateV1::Unknown,
                usage: FeatureObservationStateV1::Unknown,
                history: FeatureObservationStateV1::Unknown,
            },
            todo_total: 0,
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
            history: None,
            detail: None,
        }
    }

    #[test]
    fn client_uses_one_bounded_ndjson_request_per_loopback_connection() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
        let endpoint = listener.local_addr().expect("address");
        let host = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut request = String::new();
            stream.read_to_string(&mut request).expect("request");
            assert_eq!(request.matches('\n').count(), 1);
            let envelope: HarnessReadEnvelopeV1 =
                serde_json::from_str(request.trim_end()).expect("envelope");
            assert!(matches!(envelope.request, HarnessReadRequestV1::ContextGet));
            let reply = HarnessReadReplyV1::Error { error: HarnessReadHostErrorV1::NotFoundOrDenied };
            let mut encoded = serde_json::to_vec(&reply).expect("reply");
            encoded.push(b'\n');
            stream.write_all(&encoded).expect("write reply");
        });
        let client = HarnessReadClient::new(endpoint, credential()).expect("client");
        assert!(matches!(client.context_get(), Err(HarnessReadClientError::Host(HarnessReadHostErrorV1::NotFoundOrDenied))));
        host.join().expect("host");
    }

    #[test]
    fn client_rejects_non_loopback_and_oversized_frames_without_secret_errors() {
        let secret = credential();
        let error = HarnessReadClient::new("192.0.2.1:1".parse().expect("address"), secret.clone())
            .expect_err("non-loopback must fail");
        assert!(!error.to_string().contains(secret.expose()));

        let oversized = vec![b'x'; HARNESS_READ_RESPONSE_MAX_BYTES + 1];
        let mut reader = BufReader::new(oversized.as_slice());
        assert!(matches!(read_bounded_line(&mut reader, HARNESS_READ_RESPONSE_MAX_BYTES), Err(HarnessReadClientError::ResponseTooLarge)));
    }

    #[test]
    fn operator_client_uses_the_same_bounded_socket_with_a_distinct_envelope() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
        let endpoint = listener.local_addr().expect("address");
        let host = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut request = String::new();
            stream.read_to_string(&mut request).expect("request");
            let envelope: HarnessOperatorEnvelopeV1 =
                serde_json::from_str(request.trim_end()).expect("operator envelope");
            assert!(matches!(
                envelope.request,
                HarnessOperatorRequestV1::TasksList { limit: 1, .. },
            ));
            let reply = HarnessOperatorReplyV1::Error {
                error: HarnessOperatorHostErrorV1::Conflict,
            };
            let mut encoded = serde_json::to_vec(&reply).expect("reply");
            encoded.push(b'\n');
            stream.write_all(&encoded).expect("write reply");
        });
        let client = HarnessOperatorClient::new(endpoint, operator_credential())
            .expect("operator client");
        assert!(matches!(
            client.tasks_list(None, None, 1),
            Err(HarnessOperatorClientError::Host(
                HarnessOperatorHostErrorV1::Conflict,
            )),
        ));
        host.join().expect("host");
    }

    #[test]
    fn operator_client_rejects_monitor_for_a_different_run() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
        let endpoint = listener.local_addr().expect("address");
        let requested_run_id = HarnessRunId::new(format!("hrun_{}", "a".repeat(24))).unwrap();
        let other_run_id = HarnessRunId::new(format!("hrun_{}", "b".repeat(24))).unwrap();
        let host = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut request = String::new();
            stream.read_to_string(&mut request).expect("request");
            let reply = HarnessOperatorReplyV1::Ok {
                response: HarnessOperatorResponseV1::Monitor(empty_monitor(other_run_id)),
            };
            let mut encoded = serde_json::to_vec(&reply).expect("reply");
            encoded.push(b'\n');
            stream.write_all(&encoded).expect("write reply");
        });
        let client = HarnessOperatorClient::new(endpoint, operator_credential())
            .expect("operator client");
        assert!(matches!(
            client.monitor_get(requested_run_id),
            Err(HarnessOperatorClientError::Api(HarnessOperatorApiError::Read(
                HarnessReadApiError::InvalidRunState,
            ))),
        ));
        host.join().expect("host");
    }

    #[test]
    fn operator_client_submits_v3_user_intent_without_durable_authority() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
        let endpoint = listener.local_addr().expect("address");
        let host = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut request = String::new();
            stream.read_to_string(&mut request).expect("request");
            assert!(!request.contains("authority"));
            assert!(!request.contains("operation_id"));
            assert!(!request.contains("idempotency_ref"));
            assert!(!request.contains("\"task_id\":"));
            let envelope: HarnessOperatorEnvelopeV1 =
                serde_json::from_str(request.trim_end()).expect("operator envelope");
            assert_eq!(envelope.version, HARNESS_OPERATOR_WIRE_VERSION_V3);
            assert!(matches!(
                envelope.request,
                HarnessOperatorRequestV1::SubmitIntent {
                    intent: HarnessOperatorIntentV1 {
                        action: HarnessOperatorActionV1::CreateTask { .. },
                        ..
                    },
                },
            ));
            let reply = HarnessOperatorReplyV1::Ok {
                response: HarnessOperatorResponseV1::Mutation(
                    HarnessOperatorMutationOutcomeV1::Applied,
                ),
            };
            let mut encoded = serde_json::to_vec(&reply).expect("reply");
            encoded.push(b'\n');
            stream.write_all(&encoded).expect("write reply");
        });
        let client = HarnessOperatorClient::new(endpoint, operator_credential())
            .expect("operator client");
        let response = client.submit_intent(HarnessOperatorIntentV1 {
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
        }).unwrap();
        assert_eq!(
            response,
            HarnessOperatorResponseV1::Mutation(HarnessOperatorMutationOutcomeV1::Applied),
        );
        host.join().expect("host");
    }

    #[test]
    fn operator_client_native_history_uses_v3_exact_route_and_extended_read_deadline() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
        let endpoint = listener.local_addr().expect("address");
        let route = HarnessNativeSessionRouteV1 {
            node_id: "node-a".to_owned(),
            incarnation_id: "1".repeat(32),
            scope: HarnessNativeSessionCatalogScopeV1::Workspace,
            workspace_id: Some("workspace-a".to_owned()),
            provider: "codex".to_owned(),
        };
        let echoed_route = route.clone();
        let host = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut request = String::new();
            stream.read_to_string(&mut request).expect("request");
            for forbidden in ["provider_identity", "session_id", "terminal", "cwd"] {
                assert!(!request.contains(forbidden));
            }
            let envelope: HarnessOperatorEnvelopeV1 =
                serde_json::from_str(request.trim_end()).expect("operator envelope");
            assert_eq!(envelope.version, HARNESS_OPERATOR_WIRE_VERSION_V3);
            assert!(matches!(
                envelope.request,
                HarnessOperatorRequestV1::CatalogNativeSessions {
                    route: ref actual,
                    limit: 16,
                } if actual == &echoed_route,
            ));
            let reply = HarnessOperatorReplyV1::Ok {
                response: HarnessOperatorResponseV1::NativeSessionsCataloged(
                    HarnessNativeSessionsCatalogedV1 {
                        route: echoed_route,
                        entries: Vec::new(),
                        summary: None,
                    },
                ),
            };
            let mut encoded = serde_json::to_vec(&reply).expect("reply");
            encoded.push(b'\n');
            stream.write_all(&encoded).expect("write reply");
        });
        assert!(HARNESS_NATIVE_HISTORY_DEADLINE > Duration::from_secs(35));
        let client = HarnessOperatorClient::new(endpoint, operator_credential())
            .expect("operator client");
        let response = client.catalog_native_sessions(route.clone(), 16).unwrap();
        assert_eq!(response.route, route);
        assert!(response.entries.is_empty());
        host.join().expect("host");
    }

    #[test]
    fn operator_client_gets_v4_run_correlation_without_identity_loss() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
        let endpoint = listener.local_addr().expect("address");
        let run_id = HarnessRunId::new(format!("hrun_{}", "a".repeat(24))).unwrap();
        let expected_run_id = run_id.clone();
        let correlation = HarnessRunCorrelationV1 {
            run_id: run_id.clone(),
            run_revision: HarnessRevision::new(9).unwrap(),
            task_id: HarnessTaskId::new(format!("htask_{}", "b".repeat(24))).unwrap(),
            node_id: HarnessSelectorV1::new("node-a").unwrap(),
            node_incarnation_id: HarnessNodeIncarnationV1::new("07".repeat(16)).unwrap(),
            workspace_id: HarnessSelectorV1::new("workspace-a").unwrap(),
            provider_profile: HarnessSelectorV1::new("codex-default").unwrap(),
            mode: HarnessExecutionModeV1::Pty,
            worktree: HarnessRunWorktreeViewV1::Managed {
                worktree_ref: HarnessSelectorV1::new("worktree-a").unwrap(),
            },
            session: HarnessRunSessionViewV1::Managed(HarnessManagedRunSessionV1 {
                record_id: HarnessSelectorV1::new("record-a").unwrap(),
                active_session: Some(HarnessRuntimeIdentityV1 {
                    instance_id: 19,
                    generation: 4,
                }),
            }),
            availability: HarnessRunCorrelationAvailabilityV1::Available,
            observed_at_unix_ms: Some(200),
        };
        let echoed = correlation.clone();
        let host = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut request = String::new();
            stream.read_to_string(&mut request).expect("request");
            let envelope: HarnessOperatorEnvelopeV1 =
                serde_json::from_str(request.trim_end()).expect("operator envelope");
            assert_eq!(envelope.version, HARNESS_OPERATOR_WIRE_VERSION_V4);
            assert!(matches!(
                envelope.request,
                HarnessOperatorRequestV1::RunCorrelationGet { run_id }
                    if run_id == expected_run_id,
            ));
            let reply = HarnessOperatorReplyV1::Ok {
                response: HarnessOperatorResponseV1::RunCorrelation(echoed),
            };
            let mut encoded = serde_json::to_vec(&reply).expect("reply");
            encoded.push(b'\n');
            stream.write_all(&encoded).expect("write reply");
        });
        let client = HarnessOperatorClient::new(endpoint, operator_credential())
            .expect("operator client");
        assert_eq!(client.run_correlation_get(run_id).unwrap(), correlation);
        host.join().expect("host");
    }

    #[test]
    fn operator_client_gets_exact_v5_run_transfer() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
        let endpoint = listener.local_addr().expect("address");
        let run_id = HarnessRunId::new(format!("hrun_{}", "a".repeat(24))).unwrap();
        let expected_run_id = run_id.clone();
        let summary = run_transfer_summary(run_id.clone());
        let echoed = summary.clone();
        let host = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut request = String::new();
            stream.read_to_string(&mut request).expect("request");
            let envelope: HarnessOperatorEnvelopeV1 =
                serde_json::from_str(request.trim_end()).expect("operator envelope");
            assert_eq!(envelope.version, HARNESS_OPERATOR_WIRE_VERSION_V5);
            assert!(matches!(
                envelope.request,
                HarnessOperatorRequestV1::RunTransferGet { run_id }
                    if run_id == expected_run_id,
            ));
            let reply = HarnessOperatorReplyV1::Ok {
                response: HarnessOperatorResponseV1::RunTransfer(echoed),
            };
            let mut encoded = serde_json::to_vec(&reply).expect("reply");
            encoded.push(b'\n');
            stream.write_all(&encoded).expect("write reply");
        });
        let client = HarnessOperatorClient::new(endpoint, operator_credential())
            .expect("operator client");
        assert_eq!(client.run_transfer_get(run_id).unwrap(), summary);
        host.join().expect("host");
    }

    #[test]
    fn operator_client_rejects_mismatched_v5_run_transfer() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
        let endpoint = listener.local_addr().expect("address");
        let requested_run_id =
            HarnessRunId::new(format!("hrun_{}", "a".repeat(24))).unwrap();
        let other_run_id = HarnessRunId::new(format!("hrun_{}", "b".repeat(24))).unwrap();
        let host = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut request = String::new();
            stream.read_to_string(&mut request).expect("request");
            let reply = HarnessOperatorReplyV1::Ok {
                response: HarnessOperatorResponseV1::RunTransfer(
                    run_transfer_summary(other_run_id),
                ),
            };
            let mut encoded = serde_json::to_vec(&reply).expect("reply");
            encoded.push(b'\n');
            stream.write_all(&encoded).expect("write reply");
        });
        let client = HarnessOperatorClient::new(endpoint, operator_credential())
            .expect("operator client");
        assert!(matches!(
            client.run_transfer_get(requested_run_id),
            Err(HarnessOperatorClientError::Api(
                HarnessOperatorApiError::InvalidRunTransfer,
            )),
        ));
        host.join().expect("host");
    }

    fn client_task_launch_options(task_id: HarnessTaskId) -> HarnessTaskLaunchOptionsV1 {
        HarnessTaskLaunchOptionsV1 {
            task_id,
            task_revision: HarnessRevision::new(7).unwrap(),
            policy_digest: HarnessRequestDigest::new("a".repeat(64)).unwrap(),
            plans: vec![HarnessOrdinaryLaunchPlanOptionV1 {
                plan: HarnessLaunchPlanRefV1 {
                    plan_id: HarnessSelectorV1::new("ordinary-codex").unwrap(),
                    revision: HarnessRevision::new(4).unwrap(),
                    digest: HarnessRequestDigest::new("b".repeat(64)).unwrap(),
                },
                node_id: HarnessSelectorV1::new("node-a").unwrap(),
                source_workspace_id: HarnessSelectorV1::new("workspace-a").unwrap(),
                provider_profile: HarnessSelectorV1::new("codex-default").unwrap(),
                provider_id: HarnessSelectorV1::new("codex").unwrap(),
                mode: HarnessExecutionModeV1::Pty,
            }],
            managed_worktree_profiles: Vec::new(),
            context_sources: Vec::new(),
            delivery_bundles: Vec::new(),
            current_issued_spec: None,
            truncated: false,
        }
    }

    fn client_v6_authority() -> HarnessOperatorAuthorityV1 {
        HarnessOperatorAuthorityV1 {
            operation_id: HarnessOperationId::new(format!("hop_{}", "c".repeat(24))).unwrap(),
            idempotency_ref: HarnessIdempotencyRef::new(format!(
                "hidem_{}",
                "d".repeat(24),
            )).unwrap(),
            actor_id: HarnessSelectorV1::new("operator").unwrap(),
            now_unix_ms: 100,
        }
    }

    #[test]
    fn operator_client_round_trips_exact_v6_launch_options_replace_and_start() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
        let endpoint = listener.local_addr().expect("address");
        let task_id = HarnessTaskId::new(format!("htask_{}", "1".repeat(24))).unwrap();
        let options = client_task_launch_options(task_id.clone());
        let authority = client_v6_authority();
        let issuance = HarnessTaskLaunchIssuanceRefV1 {
            issuance_id: HarnessTaskLaunchIssuanceId::new(format!(
                "hissue_{}",
                "2".repeat(24),
            )).unwrap(),
            revision: HarnessRevision::new(2).unwrap(),
            digest: HarnessRequestDigest::new("e".repeat(64)).unwrap(),
        };
        let outcome = HarnessTaskStartOutcomeV1 {
            dispatch: HarnessDispatchIntentV1 {
                task_id: task_id.clone(),
                task_revision: HarnessRevision::new(8).unwrap(),
                run_id: HarnessRunId::new(format!("hrun_{}", "3".repeat(24))).unwrap(),
                run_revision: HarnessRevision::new(1).unwrap(),
                operation_id: authority.operation_id.clone(),
                operation_revision: HarnessRevision::new(1).unwrap(),
                idempotency_ref: authority.idempotency_ref.clone(),
                parent_run_id: None,
                intent: HarnessRunIntentV1 {
                    node_id: HarnessSelectorV1::new("node-a").unwrap(),
                    workspace_id: HarnessSelectorV1::new("workspace-a").unwrap(),
                    worktree: HarnessWorktreeIntentV1::Existing,
                    provider_profile: HarnessSelectorV1::new("codex-default").unwrap(),
                    mode: HarnessExecutionModeV1::Pty,
                    delivery_bundle: None,
                    continuation: None,
                },
            },
            replayed: false,
        };
        let expected_options = options.clone();
        let expected_task_id = task_id.clone();
        let expected_outcome = outcome.clone();
        let host = thread::spawn(move || {
            for index in 0..3 {
                let (mut stream, _) = listener.accept().expect("accept");
                let mut request = String::new();
                stream.read_to_string(&mut request).expect("request");
                for forbidden in ["provider_home", "credential_path", "canonical_root", "prompt"] {
                    assert!(!request.contains(forbidden), "request exposed {forbidden}");
                }
                let envelope: HarnessOperatorEnvelopeV1 =
                    serde_json::from_str(request.trim_end()).expect("operator envelope");
                assert_eq!(envelope.version, HARNESS_OPERATOR_WIRE_VERSION_V6);
                let response = match (index, envelope.request) {
                    (0, HarnessOperatorRequestV1::TaskLaunchOptionsGet { task_id }) => {
                        assert_eq!(task_id, expected_task_id);
                        HarnessOperatorResponseV1::TaskLaunchOptions(expected_options.clone())
                    }
                    (1, HarnessOperatorRequestV1::ReplaceTaskExecutionSpecV2 { request }) => {
                        assert_eq!(request.task_id, expected_task_id);
                        assert_eq!(request.selection.plan, expected_options.plans[0]);
                        HarnessOperatorResponseV1::ExecutionSpecMutation(
                            HarnessOperatorMutationOutcomeV1::Applied,
                        )
                    }
                    (2, HarnessOperatorRequestV1::StartTaskV2 { request }) => {
                        assert_eq!(request.task_id, expected_task_id);
                        HarnessOperatorResponseV1::TaskStarted(expected_outcome.clone())
                    }
                    _ => panic!("unexpected V6 request"),
                };
                let reply = HarnessOperatorReplyV1::Ok { response };
                let mut encoded = serde_json::to_vec(&reply).expect("reply");
                encoded.push(b'\n');
                stream.write_all(&encoded).expect("write reply");
            }
        });
        let client = HarnessOperatorClient::new(endpoint, operator_credential())
            .expect("operator client");
        assert_eq!(client.task_launch_options_get(task_id.clone()).unwrap(), options);
        assert_eq!(
            client.replace_task_execution_spec_v2(HarnessReplaceTaskExecutionSpecRequestV2 {
                authority: authority.clone(),
                task_id: task_id.clone(),
                expected_task_revision: HarnessRevision::new(7).unwrap(),
                expected_execution_spec_revision: HarnessExpectedExecutionSpecRevisionV1::Absent,
                selection: HarnessReviewedTaskLaunchSelectionV1 {
                    plan: options.plans[0].clone(),
                    worktree: HarnessReviewedWorktreeSelectionV1::Existing,
                    context_source: None,
                    delivery: None,
                    review_policy: HarnessTaskReviewPolicyV1::OperatorReview,
                },
            }).unwrap(),
            HarnessOperatorMutationOutcomeV1::Applied,
        );
        assert_eq!(
            client.start_task_v2(HarnessStartTaskRequestV2 {
                authority,
                task_id,
                expected_task_revision: HarnessRevision::new(7).unwrap(),
                expected_execution_spec_revision: HarnessRevision::new(2).unwrap(),
                expected_launch_issuance: issuance,
            }).unwrap(),
            outcome,
        );
        host.join().expect("host");
    }

    #[test]
    fn operator_client_rejects_mismatched_v6_task_launch_options() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
        let endpoint = listener.local_addr().expect("address");
        let requested = HarnessTaskId::new(format!("htask_{}", "1".repeat(24))).unwrap();
        let other = HarnessTaskId::new(format!("htask_{}", "2".repeat(24))).unwrap();
        let host = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut request = String::new();
            stream.read_to_string(&mut request).expect("request");
            let reply = HarnessOperatorReplyV1::Ok {
                response: HarnessOperatorResponseV1::TaskLaunchOptions(
                    client_task_launch_options(other),
                ),
            };
            let mut encoded = serde_json::to_vec(&reply).expect("reply");
            encoded.push(b'\n');
            stream.write_all(&encoded).expect("write reply");
        });
        let client = HarnessOperatorClient::new(endpoint, operator_credential())
            .expect("operator client");
        assert!(matches!(
            client.task_launch_options_get(requested),
            Err(HarnessOperatorClientError::Api(
                HarnessOperatorApiError::InvalidTaskLaunchOptions,
            )),
        ));
        host.join().expect("host");
    }

    #[test]
    fn operator_client_rejects_mismatched_v6_start_correlation() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
        let endpoint = listener.local_addr().expect("address");
        let task_id = HarnessTaskId::new(format!("htask_{}", "1".repeat(24))).unwrap();
        let authority = client_v6_authority();
        let mut mismatched_authority = authority.clone();
        mismatched_authority.operation_id = HarnessOperationId::new(format!(
            "hop_{}",
            "f".repeat(24),
        )).unwrap();
        let other_task = HarnessTaskId::new(format!("htask_{}", "9".repeat(24))).unwrap();
        let host = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut request = String::new();
            stream.read_to_string(&mut request).expect("request");
            let reply = HarnessOperatorReplyV1::Ok {
                response: HarnessOperatorResponseV1::TaskStarted(HarnessTaskStartOutcomeV1 {
                    dispatch: HarnessDispatchIntentV1 {
                        task_id: other_task,
                        task_revision: HarnessRevision::new(8).unwrap(),
                        run_id: HarnessRunId::new(format!("hrun_{}", "3".repeat(24))).unwrap(),
                        run_revision: HarnessRevision::new(1).unwrap(),
                        operation_id: mismatched_authority.operation_id,
                        operation_revision: HarnessRevision::new(1).unwrap(),
                        idempotency_ref: mismatched_authority.idempotency_ref,
                        parent_run_id: None,
                        intent: HarnessRunIntentV1 {
                            node_id: HarnessSelectorV1::new("node-a").unwrap(),
                            workspace_id: HarnessSelectorV1::new("workspace-a").unwrap(),
                            worktree: HarnessWorktreeIntentV1::Existing,
                            provider_profile: HarnessSelectorV1::new("codex-default").unwrap(),
                            mode: HarnessExecutionModeV1::Pty,
                            delivery_bundle: None,
                            continuation: None,
                        },
                    },
                    replayed: false,
                }),
            };
            let mut encoded = serde_json::to_vec(&reply).expect("reply");
            encoded.push(b'\n');
            stream.write_all(&encoded).expect("write reply");
        });
        let client = HarnessOperatorClient::new(endpoint, operator_credential())
            .expect("operator client");
        assert!(matches!(
            client.start_task_v2(HarnessStartTaskRequestV2 {
                authority,
                task_id,
                expected_task_revision: HarnessRevision::new(7).unwrap(),
                expected_execution_spec_revision: HarnessRevision::new(2).unwrap(),
                expected_launch_issuance: HarnessTaskLaunchIssuanceRefV1 {
                    issuance_id: HarnessTaskLaunchIssuanceId::new(format!(
                        "hissue_{}",
                        "2".repeat(24),
                    )).unwrap(),
                    revision: HarnessRevision::new(2).unwrap(),
                    digest: HarnessRequestDigest::new("e".repeat(64)).unwrap(),
                },
            }),
            Err(HarnessOperatorClientError::Api(
                HarnessOperatorApiError::InvalidTaskLaunchSelection,
            )),
        ));
        host.join().expect("host");
    }

    #[test]
    fn operator_client_round_trips_v4_execution_spec_and_exact_start() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
        let endpoint = listener.local_addr().expect("address");
        let task_id = HarnessTaskId::new(format!("htask_{}", "1".repeat(24))).unwrap();
        let scheduled_launch = HarnessScheduledLaunchRefV2 {
            plan: HarnessLaunchPlanRefV1 {
                plan_id: HarnessSelectorV1::new("ordinary-codex").unwrap(),
                revision: HarnessRevision::new(4).unwrap(),
                digest: HarnessRequestDigest::new("a".repeat(64)).unwrap(),
            },
            authority: HarnessLaunchAuthorityRefV1::OrdinaryOperator,
        };
        let summary = HarnessLaunchPlanSummaryV1 {
            scheduled_launch: scheduled_launch.clone(),
            node_id: HarnessSelectorV1::new("node-a").unwrap(),
            workspace_id: HarnessSelectorV1::new("workspace-a").unwrap(),
            worktree: HarnessWorktreeIntentV1::Existing,
            provider_profile: HarnessSelectorV1::new("codex-default").unwrap(),
            provider_id: HarnessSelectorV1::new("codex").unwrap(),
            mode: HarnessExecutionModeV1::Pty,
        };
        let page = HarnessLaunchPlanPageV1 {
            plans: vec![summary],
            next_plan_id: None,
        };
        let spec = HarnessTaskExecutionSpecV1 {
            execution_spec_id: HarnessExecutionSpecId::new(format!(
                "hespec_{}",
                "2".repeat(24),
            )).unwrap(),
            revision: HarnessRevision::new(2).unwrap(),
            task_id: task_id.clone(),
            scheduled_launch: scheduled_launch.clone(),
            scheduled_launch_digest: HarnessRequestDigest::new("b".repeat(64)).unwrap(),
            review_policy: HarnessTaskReviewPolicyV1::OperatorReview,
            created_at_unix_ms: 10,
            updated_at_unix_ms: 20,
        };
        let outcome = HarnessTaskStartOutcomeV1 {
            dispatch: HarnessDispatchIntentV1 {
                task_id: task_id.clone(),
                task_revision: HarnessRevision::new(8).unwrap(),
                run_id: HarnessRunId::new(format!("hrun_{}", "3".repeat(24))).unwrap(),
                run_revision: HarnessRevision::new(1).unwrap(),
                operation_id: HarnessOperationId::new(format!(
                    "hop_{}",
                    "4".repeat(24),
                )).unwrap(),
                operation_revision: HarnessRevision::new(1).unwrap(),
                idempotency_ref: HarnessIdempotencyRef::new(format!(
                    "hidem_{}",
                    "5".repeat(24),
                )).unwrap(),
                parent_run_id: None,
                intent: HarnessRunIntentV1 {
                    node_id: HarnessSelectorV1::new("node-a").unwrap(),
                    workspace_id: HarnessSelectorV1::new("workspace-a").unwrap(),
                    worktree: HarnessWorktreeIntentV1::Existing,
                    provider_profile: HarnessSelectorV1::new("codex-default").unwrap(),
                    mode: HarnessExecutionModeV1::Pty,
                    delivery_bundle: None,
                    continuation: None,
                },
            },
            replayed: false,
        };
        let expected_page = page.clone();
        let expected_spec = spec.clone();
        let expected_outcome = outcome.clone();
        let host = thread::spawn(move || {
            for index in 0..4 {
                let (mut stream, _) = listener.accept().expect("accept");
                let mut request = String::new();
                stream.read_to_string(&mut request).expect("request");
                for forbidden in ["provider_home", "environment", "spawn_spec"] {
                    assert!(!request.contains(forbidden));
                }
                let envelope: HarnessOperatorEnvelopeV1 =
                    serde_json::from_str(request.trim_end()).expect("operator envelope");
                assert_eq!(envelope.version, HARNESS_OPERATOR_WIRE_VERSION_V4);
                let response = match (index, envelope.request) {
                    (0, HarnessOperatorRequestV1::LaunchPlansList { limit: 16, .. }) => {
                        HarnessOperatorResponseV1::LaunchPlans(expected_page.clone())
                    }
                    (1, HarnessOperatorRequestV1::TaskExecutionSpecGet { .. }) => {
                        HarnessOperatorResponseV1::TaskExecutionSpec(Some(expected_spec.clone()))
                    }
                    (2, HarnessOperatorRequestV1::ReplaceTaskExecutionSpec { .. }) => {
                        HarnessOperatorResponseV1::ExecutionSpecMutation(
                            HarnessOperatorMutationOutcomeV1::Replayed,
                        )
                    }
                    (3, HarnessOperatorRequestV1::StartTask { .. }) => {
                        HarnessOperatorResponseV1::TaskStarted(expected_outcome.clone())
                    }
                    _ => panic!("unexpected V4 request"),
                };
                let reply = HarnessOperatorReplyV1::Ok { response };
                let mut encoded = serde_json::to_vec(&reply).expect("reply");
                encoded.push(b'\n');
                stream.write_all(&encoded).expect("write reply");
            }
        });
        let client = HarnessOperatorClient::new(endpoint, operator_credential())
            .expect("operator client");
        assert_eq!(client.launch_plans_list(None, 16).unwrap(), page);
        assert_eq!(client.task_execution_spec_get(task_id.clone()).unwrap(), Some(spec.clone()));
        let authority = HarnessOperatorAuthorityV1 {
            operation_id: HarnessOperationId::new(format!("hop_{}", "6".repeat(24))).unwrap(),
            idempotency_ref: HarnessIdempotencyRef::new(format!(
                "hidem_{}",
                "7".repeat(24),
            )).unwrap(),
            actor_id: HarnessSelectorV1::new("operator").unwrap(),
            now_unix_ms: 30,
        };
        assert_eq!(
            client.replace_task_execution_spec(HarnessReplaceTaskExecutionSpecRequestV1 {
                authority: authority.clone(),
                task_id: task_id.clone(),
                expected_task_revision: HarnessRevision::new(7).unwrap(),
                expected_execution_spec_revision:
                    HarnessExpectedExecutionSpecRevisionV1::Absent,
                spec: HarnessTaskExecutionSpecInputV1 {
                    scheduled_launch,
                    review_policy: HarnessTaskReviewPolicyV1::OperatorReview,
                },
            }).unwrap(),
            HarnessOperatorMutationOutcomeV1::Replayed,
        );
        assert_eq!(
            client.start_task(HarnessStartTaskRequestV1 {
                authority,
                task_id,
                expected_task_revision: HarnessRevision::new(7).unwrap(),
                expected_execution_spec_revision: HarnessRevision::new(2).unwrap(),
                expected_scheduled_launch_digest: HarnessRequestDigest::new(
                    "b".repeat(64),
                ).unwrap(),
            }).unwrap(),
            outcome,
        );
        host.join().expect("host");
    }

    #[test]
    fn operator_client_round_trips_v4_run_workspace_reads_over_one_shot_sockets() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
        let endpoint = listener.local_addr().expect("address");
        let run_id = HarnessRunId::new(format!("hrun_{}", "a".repeat(24))).unwrap();
        let path = HarnessRepositoryPathV1::new("src/lib.rs").unwrap();
        let origin = HarnessRunWorkspaceOriginV1 {
            run_id: run_id.clone(),
            run_revision: HarnessRevision::new(9).unwrap(),
            node_id: HarnessSelectorV1::new("node-a").unwrap(),
            node_incarnation_id: HarnessNodeIncarnationV1::new("07".repeat(16)).unwrap(),
            workspace_id: HarnessSelectorV1::new("workspace-a").unwrap(),
        };
        let inspection = HarnessRunWorkspaceInspectionV1 {
            origin: origin.clone(),
            entries: vec![HarnessWorkspaceTreeEntryV1 {
                relative_path: path.clone(),
                kind: HarnessWorkspaceEntryKindV1::File,
            }],
            tree_truncated: false,
            git: HarnessGitSummaryV1 {
                is_repository: false,
                branch: None,
                status: Vec::new(),
                recent_commits: Vec::new(),
                truncated: false,
            },
        };
        let file = HarnessRunWorkspaceFileV1 {
            origin: origin.clone(),
            path: path.clone(),
            content: HarnessWorkspaceFileContentV1::Utf8 {
                text: "fn main() {}\n".to_owned(),
                byte_len: 13,
            },
            revision: Some(HarnessWorkspaceFileRevisionV1::new("b".repeat(64)).unwrap()),
        };
        let history = HarnessRunGitHistoryPageV1 {
            origin: origin.clone(),
            path: Some(path.clone()),
            commits: Vec::new(),
            next_before: None,
            truncated: false,
        };
        let diff = HarnessRunGitDiffV1 {
            origin,
            mode: HarnessGitDiffModeV1::Working,
            path: Some(path.clone()),
            text: "diff --git a/src/lib.rs b/src/lib.rs\n".to_owned(),
            truncated: false,
        };
        let expected_inspection = inspection.clone();
        let expected_file = file.clone();
        let expected_history = history.clone();
        let expected_diff = diff.clone();
        let expected_path = path.clone();
        let host = thread::spawn(move || {
            for index in 0..4 {
                let (mut stream, _) = listener.accept().expect("accept");
                let mut request = String::new();
                stream.read_to_string(&mut request).expect("request");
                for forbidden in [
                    "node_id",
                    "workspace_id",
                    "endpoint",
                    "root",
                    "worktree",
                    "environment",
                    "spawn_spec",
                    "provider_home",
                ] {
                    assert!(!request.contains(forbidden), "request exposed {forbidden}");
                }
                let envelope: HarnessOperatorEnvelopeV1 =
                    serde_json::from_str(request.trim_end()).expect("operator envelope");
                assert_eq!(envelope.version, HARNESS_OPERATOR_WIRE_VERSION_V4);
                let response = match (index, envelope.request) {
                    (0, HarnessOperatorRequestV1::InspectRunWorkspace { .. }) => {
                        HarnessOperatorResponseV1::RunWorkspaceInspected(inspection.clone())
                    }
                    (1, HarnessOperatorRequestV1::ReadRunWorkspaceFile { path, .. })
                        if path == expected_path =>
                    {
                        HarnessOperatorResponseV1::RunWorkspaceFileRead(file.clone())
                    }
                    (2, HarnessOperatorRequestV1::ReadRunGitHistory {
                        path: Some(path),
                        before: None,
                        limit: 16,
                        ..
                    }) if path == expected_path => {
                        HarnessOperatorResponseV1::RunGitHistoryRead(history.clone())
                    }
                    (3, HarnessOperatorRequestV1::ReadRunGitDiff {
                        mode: HarnessGitDiffModeV1::Working,
                        path: Some(path),
                        ..
                    }) if path == expected_path => {
                        HarnessOperatorResponseV1::RunGitDiffRead(diff.clone())
                    }
                    _ => panic!("unexpected V4 run workspace request"),
                };
                let reply = HarnessOperatorReplyV1::Ok { response };
                let mut encoded = serde_json::to_vec(&reply).expect("reply");
                encoded.push(b'\n');
                stream.write_all(&encoded).expect("write reply");
            }
        });
        let client = HarnessOperatorClient::new(endpoint, operator_credential())
            .expect("operator client");
        assert_eq!(client.inspect_run_workspace(run_id.clone()).unwrap(), expected_inspection);
        assert_eq!(
            client.read_run_workspace_file(run_id.clone(), path.clone()).unwrap(),
            expected_file,
        );
        assert_eq!(
            client.read_run_git_history(
                run_id.clone(),
                Some(path.clone()),
                None,
                16,
            ).unwrap(),
            expected_history,
        );
        assert_eq!(
            client.read_run_git_diff(
                run_id,
                HarnessGitDiffModeV1::Working,
                Some(path),
            ).unwrap(),
            expected_diff,
        );
        host.join().expect("host");
    }

    #[test]
    fn operator_client_round_trips_v9_node_workspace_reads_over_one_shot_sockets() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
        let endpoint = listener.local_addr().expect("address");
        let path = HarnessRepositoryPathV1::new("src/lib.rs").unwrap();
        let origin = HarnessNodeWorkspaceOriginV1 {
            node_id: "node-a".to_owned(),
            node_incarnation_id: "07".repeat(16),
            workspace_id: "workspace-a".to_owned(),
        };
        let inspection = HarnessNodeWorkspaceInspectionV1 {
            origin: origin.clone(),
            entries: vec![HarnessWorkspaceTreeEntryV1 {
                relative_path: path.clone(),
                kind: HarnessWorkspaceEntryKindV1::File,
            }],
            tree_truncated: false,
            git: HarnessGitSummaryV1 {
                is_repository: false,
                branch: None,
                status: Vec::new(),
                recent_commits: Vec::new(),
                truncated: false,
            },
        };
        let file = HarnessNodeWorkspaceFileV1 {
            origin: origin.clone(),
            path: path.clone(),
            content: HarnessWorkspaceFileContentV1::Utf8 {
                text: "fn main() {}\n".to_owned(),
                byte_len: 13,
            },
            revision: Some(HarnessWorkspaceFileRevisionV1::new("b".repeat(64)).unwrap()),
        };
        let history = HarnessNodeGitHistoryPageV1 {
            origin: origin.clone(),
            path: Some(path.clone()),
            commits: Vec::new(),
            next_before: None,
            truncated: false,
        };
        let diff = HarnessNodeGitDiffV1 {
            origin,
            mode: HarnessGitDiffModeV1::Working,
            path: Some(path.clone()),
            text: "diff --git a/src/lib.rs b/src/lib.rs\n".to_owned(),
            truncated: false,
        };
        let expected_inspection = inspection.clone();
        let expected_file = file.clone();
        let expected_history = history.clone();
        let expected_diff = diff.clone();
        let expected_path = path.clone();
        let host = thread::spawn(move || {
            for index in 0..4 {
                let (mut stream, _) = listener.accept().expect("accept");
                let mut request = String::new();
                stream.read_to_string(&mut request).expect("request");
                for forbidden in ["run_id", "endpoint", "root", "worktree", "environment"] {
                    assert!(!request.contains(forbidden), "request exposed {forbidden}");
                }
                let envelope: HarnessOperatorEnvelopeV1 =
                    serde_json::from_str(request.trim_end()).expect("operator envelope");
                assert_eq!(envelope.version, HARNESS_OPERATOR_WIRE_VERSION_V9);
                let response = match (index, envelope.request) {
                    (0, HarnessOperatorRequestV1::InspectNodeWorkspace { .. }) => {
                        HarnessOperatorResponseV1::NodeWorkspaceInspected(inspection.clone())
                    }
                    (1, HarnessOperatorRequestV1::ReadNodeWorkspaceFile { path, .. })
                        if path == expected_path =>
                    {
                        HarnessOperatorResponseV1::NodeWorkspaceFileRead(file.clone())
                    }
                    (2, HarnessOperatorRequestV1::ReadNodeGitHistory {
                        path: Some(path),
                        before: None,
                        limit: 16,
                        ..
                    }) if path == expected_path => {
                        HarnessOperatorResponseV1::NodeGitHistoryRead(history.clone())
                    }
                    (3, HarnessOperatorRequestV1::ReadNodeGitDiff {
                        mode: HarnessGitDiffModeV1::Working,
                        path: Some(path),
                        ..
                    }) if path == expected_path => {
                        HarnessOperatorResponseV1::NodeGitDiffRead(diff.clone())
                    }
                    _ => panic!("unexpected V9 node workspace request"),
                };
                let reply = HarnessOperatorReplyV1::Ok { response };
                let mut encoded = serde_json::to_vec(&reply).expect("reply");
                encoded.push(b'\n');
                stream.write_all(&encoded).expect("write reply");
            }
        });
        let client = HarnessOperatorClient::new(endpoint, operator_credential())
            .expect("operator client");
        assert_eq!(
            client.inspect_node_workspace("node-a".to_owned(), "workspace-a".to_owned()).unwrap(),
            expected_inspection,
        );
        assert_eq!(
            client.read_node_workspace_file(
                "node-a".to_owned(),
                "workspace-a".to_owned(),
                path.clone(),
            ).unwrap(),
            expected_file,
        );
        assert_eq!(
            client.read_node_git_history(
                "node-a".to_owned(),
                "workspace-a".to_owned(),
                Some(path.clone()),
                None,
                16,
            ).unwrap(),
            expected_history,
        );
        assert_eq!(
            client.read_node_git_diff(
                "node-a".to_owned(),
                "workspace-a".to_owned(),
                HarnessGitDiffModeV1::Working,
                Some(path),
            ).unwrap(),
            expected_diff,
        );
        host.join().expect("host");
    }

    #[test]
    fn run_workspace_client_deadline_exceeds_host_and_c2_floors() {
        const HARNESS_RUN_WORKSPACE_HOST_RESPONSE_DEADLINE: Duration = Duration::from_secs(12);
        const C2_RUN_READ_FLOOR: Duration = Duration::from_secs(10);
        assert!(HARNESS_RUN_WORKSPACE_READ_DEADLINE > HARNESS_RUN_WORKSPACE_HOST_RESPONSE_DEADLINE);
        assert!(HARNESS_RUN_WORKSPACE_READ_DEADLINE > C2_RUN_READ_FLOOR);
    }

    #[test]
    fn operator_client_rejects_mismatched_v4_run_workspace_response() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
        let endpoint = listener.local_addr().expect("address");
        let requested_run_id = HarnessRunId::new(format!("hrun_{}", "a".repeat(24))).unwrap();
        let requested_path = HarnessRepositoryPathV1::new("src/lib.rs").unwrap();
        let host = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut request = String::new();
            stream.read_to_string(&mut request).expect("request");
            let reply = HarnessOperatorReplyV1::Ok {
                response: HarnessOperatorResponseV1::RunWorkspaceFileRead(
                    HarnessRunWorkspaceFileV1 {
                        origin: HarnessRunWorkspaceOriginV1 {
                            run_id: HarnessRunId::new(format!("hrun_{}", "b".repeat(24))).unwrap(),
                            run_revision: HarnessRevision::new(1).unwrap(),
                            node_id: HarnessSelectorV1::new("node-a").unwrap(),
                            node_incarnation_id: HarnessNodeIncarnationV1::new("07".repeat(16)).unwrap(),
                            workspace_id: HarnessSelectorV1::new("workspace-a").unwrap(),
                        },
                        path: HarnessRepositoryPathV1::new("other.rs").unwrap(),
                        content: HarnessWorkspaceFileContentV1::Utf8 {
                            text: String::new(),
                            byte_len: 0,
                        },
                        revision: Some(
                            HarnessWorkspaceFileRevisionV1::new("c".repeat(64)).unwrap(),
                        ),
                    },
                ),
            };
            let mut encoded = serde_json::to_vec(&reply).expect("reply");
            encoded.push(b'\n');
            stream.write_all(&encoded).expect("write reply");
        });
        let client = HarnessOperatorClient::new(endpoint, operator_credential())
            .expect("operator client");
        assert!(matches!(
            client.read_run_workspace_file(requested_run_id, requested_path),
            Err(HarnessOperatorClientError::Api(
                HarnessOperatorApiError::InvalidWorkspaceOrigin,
            )),
        ));
        host.join().expect("host");
    }

    #[test]
    fn operator_client_round_trips_exact_v7_reverse_attribution() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
        let endpoint = listener.local_addr().expect("address");
        let subject = HarnessReverseAttributionSubjectV1::CommitScope {
            workspace: reverse_attribution_workspace(),
            object_id: HarnessGitObjectIdV1::new("b".repeat(40)).unwrap(),
        };
        let response = HarnessReverseAttributionV1 {
            subject: subject.clone(),
            outcome: HarnessReverseAttributionOutcomeV1::Attributed,
            links: vec![HarnessReverseAttributionLinkV1 {
                task_id: HarnessTaskId::new(format!("htask_{}", "1".repeat(24))).unwrap(),
                run_id: HarnessRunId::new(format!("hrun_{}", "2".repeat(24))).unwrap(),
                run_revision: HarnessRevision::new(3).unwrap(),
                binding: HarnessReverseAttributionBindingV1::Workspace {
                    workspace: reverse_attribution_workspace(),
                },
                relation: HarnessReverseAttributionRelationV1::WorkspaceScope,
            }],
        };
        let expected_subject = subject.clone();
        let expected_response = response.clone();
        let host = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut request = String::new();
            stream.read_to_string(&mut request).expect("request");
            for forbidden in [
                "provider_session",
                "provider_profile",
                "provider_home",
                "canonical_root",
                "display_root",
                "credential_path",
            ] {
                assert!(!request.contains(forbidden), "request exposed {forbidden}");
            }
            let envelope: HarnessOperatorEnvelopeV1 =
                serde_json::from_str(request.trim_end()).expect("operator envelope");
            assert_eq!(envelope.version, HARNESS_OPERATOR_WIRE_VERSION_V7);
            match envelope.request {
                HarnessOperatorRequestV1::ReverseAttributionGet { subject } => {
                    assert_eq!(subject, expected_subject);
                }
                _ => panic!("unexpected V7 request"),
            }
            let reply = HarnessOperatorReplyV1::Ok {
                response: HarnessOperatorResponseV1::ReverseAttribution(expected_response),
            };
            let mut encoded = serde_json::to_vec(&reply).expect("reply");
            encoded.push(b'\n');
            stream.write_all(&encoded).expect("write reply");
        });
        let client = HarnessOperatorClient::new(endpoint, operator_credential())
            .expect("operator client");
        assert_eq!(client.reverse_attribution_get(subject).unwrap(), response);
        host.join().expect("host");
    }

    #[test]
    fn operator_client_rejects_mismatched_v7_reverse_attribution_subject() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
        let endpoint = listener.local_addr().expect("address");
        let requested = HarnessReverseAttributionSubjectV1::Workspace {
            workspace: reverse_attribution_workspace(),
        };
        let host = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut request = String::new();
            stream.read_to_string(&mut request).expect("request");
            let reply = HarnessOperatorReplyV1::Ok {
                response: HarnessOperatorResponseV1::ReverseAttribution(
                    HarnessReverseAttributionV1 {
                        subject: HarnessReverseAttributionSubjectV1::Workspace {
                            workspace: HarnessReverseAttributionWorkspaceV1 {
                                workspace_id: HarnessSelectorV1::new("workspace-other").unwrap(),
                                ..reverse_attribution_workspace()
                            },
                        },
                        outcome: HarnessReverseAttributionOutcomeV1::Unattributed,
                        links: Vec::new(),
                    },
                ),
            };
            let mut encoded = serde_json::to_vec(&reply).expect("reply");
            encoded.push(b'\n');
            stream.write_all(&encoded).expect("write reply");
        });
        let client = HarnessOperatorClient::new(endpoint, operator_credential())
            .expect("operator client");
        assert!(matches!(
            client.reverse_attribution_get(requested),
            Err(HarnessOperatorClientError::Api(
                HarnessOperatorApiError::InvalidReverseAttribution,
            )),
        ));
        host.join().expect("host");
    }

    #[test]
    fn operator_client_frame_bounds_and_errors_never_expose_secret() {
        let secret = operator_credential();
        let error = HarnessOperatorClient::new(
            "192.0.2.1:1".parse().expect("address"),
            secret.clone(),
        ).expect_err("non-loopback must fail");
        assert!(!error.to_string().contains(secret.expose()));
        let oversized = vec![b'x'; HARNESS_OPERATOR_RESPONSE_MAX_BYTES + 1];
        let mut reader = BufReader::new(oversized.as_slice());
        assert!(matches!(
            read_operator_bounded_line(&mut reader, HARNESS_OPERATOR_RESPONSE_MAX_BYTES),
            Err(HarnessOperatorClientError::ResponseTooLarge),
        ));
    }

    #[test]
    fn operator_client_round_trips_exact_v8_context_source_observation() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
        let endpoint = listener.local_addr().expect("address");
        let run_id = HarnessRunId::new(format!("hrun_{}", "8".repeat(24))).unwrap();
        let response = context_source_observation(run_id.clone());
        let expected_run_id = run_id.clone();
        let expected_response = response.clone();
        let host = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut request = String::new();
            stream.read_to_string(&mut request).expect("request");
            assert_eq!(request.matches('\n').count(), 1);
            for forbidden in [
                "authority",
                "operation_id",
                "idempotency_ref",
                "provider_session",
                "provider_home",
                "auth",
                "model",
            ] {
                assert!(!request.contains(forbidden), "request exposed {forbidden}");
            }
            let envelope: HarnessOperatorEnvelopeV1 =
                serde_json::from_str(request.trim_end()).expect("operator envelope");
            assert_eq!(envelope.version, HARNESS_OPERATOR_WIRE_VERSION_V8);
            assert!(matches!(
                envelope.request,
                HarnessOperatorRequestV1::ObserveRunContextSource { run_id }
                    if run_id == expected_run_id,
            ));
            let reply = HarnessOperatorReplyV1::Ok {
                response: HarnessOperatorResponseV1::RunContextSourceObserved(
                    expected_response,
                ),
            };
            let mut encoded = serde_json::to_vec(&reply).expect("reply");
            encoded.push(b'\n');
            stream.write_all(&encoded).expect("write reply");
        });
        let client = HarnessOperatorClient::new(endpoint, operator_credential())
            .expect("operator client");
        assert_eq!(client.observe_run_context_source(run_id).unwrap(), response);
        host.join().expect("host");
    }

    #[test]
    fn operator_client_rejects_mismatched_v8_context_source_run() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
        let endpoint = listener.local_addr().expect("address");
        let requested_run_id =
            HarnessRunId::new(format!("hrun_{}", "8".repeat(24))).unwrap();
        let other_run_id = HarnessRunId::new(format!("hrun_{}", "9".repeat(24))).unwrap();
        let host = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut request = String::new();
            stream.read_to_string(&mut request).expect("request");
            let reply = HarnessOperatorReplyV1::Ok {
                response: HarnessOperatorResponseV1::RunContextSourceObserved(
                    context_source_observation(other_run_id),
                ),
            };
            let mut encoded = serde_json::to_vec(&reply).expect("reply");
            encoded.push(b'\n');
            stream.write_all(&encoded).expect("write reply");
        });
        let client = HarnessOperatorClient::new(endpoint, operator_credential())
            .expect("operator client");
        assert!(matches!(
            client.observe_run_context_source(requested_run_id),
            Err(HarnessOperatorClientError::Api(
                HarnessOperatorApiError::InvalidRunContextSourceObservation,
            )),
        ));
        host.join().expect("host");
    }

    #[test]
    fn context_source_observation_deadline_exceeds_host_and_c2_floors() {
        const HOST_CONTEXT_SOURCE_OBSERVATION_DEADLINE: Duration = Duration::from_secs(12);
        const C2_CONTEXT_SOURCE_OBSERVATION_FLOOR: Duration = Duration::from_secs(10);
        assert!(
            HARNESS_CONTEXT_SOURCE_OBSERVATION_DEADLINE
                > HOST_CONTEXT_SOURCE_OBSERVATION_DEADLINE,
        );
        assert!(
            HARNESS_CONTEXT_SOURCE_OBSERVATION_DEADLINE
                > C2_CONTEXT_SOURCE_OBSERVATION_FLOOR,
        );
    }

}
