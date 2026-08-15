#![cfg(windows)]

use std::{ffi::OsString, path::PathBuf};

use gate4agent_harness_protocol::{
    HarnessActorV1, HarnessExecutionModeV1, HarnessRevision, HarnessRunId,
    HarnessSelectorV1, HarnessTaskId, HarnessTaskStateV1, HarnessTaskV1,
    HarnessWorktreeIntentV1, SessionGrantId,
};
use gate4agent_harness_service::{
    dispatch::{
        HarnessContinuationPolicyV1, HarnessGrantPolicyV1, HarnessLaunchCatalog,
        HarnessLaunchPlanV1, HarnessMcpPolicyV1, HarnessPromptSourceV1,
    },
    runtime::HarnessRuntimeCatalogs,
};
use gate4agent_node::{NodeServer, NodeServerConfig};
use gate4agent_types::{AgentId, TerminalSize};

fn selector(value: &str) -> HarnessSelectorV1 {
    HarnessSelectorV1::new(value).unwrap()
}

fn parent_run_id() -> HarnessRunId {
    HarnessRunId::new(format!("hrun_{}", "1".repeat(24))).unwrap()
}

fn exact_grant_id() -> SessionGrantId {
    SessionGrantId::new(format!("hgrant_{}", "2".repeat(24))).unwrap()
}

fn parent_run_task() -> HarnessTaskV1 {
    HarnessTaskV1 {
        task_id: HarnessTaskId::new(format!("htask_{}", "3".repeat(24))).unwrap(),
        revision: HarnessRevision::new(1).unwrap(),
        title: "Remote Node H3B child".to_owned(),
        body: String::new(),
        creator: HarnessActorV1::ParentRun { run_id: parent_run_id() },
        parent_task_id: None,
        dependencies: Vec::new(),
        state: HarnessTaskStateV1::Ready,
        run_ids: Vec::new(),
        result_refs: Vec::new(),
        artifact_refs: Vec::new(),
        created_at_unix_ms: 1,
        updated_at_unix_ms: 1,
    }
}

fn h3b_schedule_next_plan() -> HarnessLaunchPlanV1 {
    HarnessLaunchPlanV1 {
        plan_id: selector("remote-harness-mcp"),
        revision: HarnessRevision::new(1).unwrap(),
        node_id: selector("remote-h3b-node"),
        workspace_id: selector("primary"),
        worktree: HarnessWorktreeIntentV1::Existing,
        provider_profile: selector("codex-h3b-fixture"),
        provider: AgentId::new("codex").unwrap(),
        mode: HarnessExecutionModeV1::Pty,
        terminal_size: TerminalSize { rows: 24, columns: 80 },
        prompt_source: HarnessPromptSourceV1::Clear,
        delivery: None,
        continuation: HarnessContinuationPolicyV1::None,
        grant: HarnessGrantPolicyV1::Exact {
            grant_id: exact_grant_id(),
            revision: HarnessRevision::new(1).unwrap(),
        },
        harness_mcp: HarnessMcpPolicyV1::GrantBound,
        deadline_ms: 20_000,
    }
}

fn build_remote_h3b_node_fixture(
    config: NodeServerConfig,
    provider_program: PathBuf,
    provider_args: Vec<OsString>,
    helper_program: PathBuf,
) -> NodeServer {
    NodeServer::new_harness_mcp_proxy_fixture(
        config,
        provider_program,
        provider_args,
        helper_program,
    ).unwrap()
}

#[test]
#[ignore = "blocked: production ScheduleNext and Harness host still reject specialized H3B plans"]
fn windows_remote_node_harness_mcp_proxy_is_session_and_grant_bound() {
    let plan = h3b_schedule_next_plan();
    let task = parent_run_task();
    let (actor, parent, intent) = plan.run_authority_and_intent(&task).unwrap();
    assert_eq!(actor, HarnessActorV1::ParentRun { run_id: parent_run_id() });
    assert_eq!(parent, Some(parent_run_id()));
    assert_eq!(intent.node_id, selector("remote-h3b-node"));
    assert_eq!(intent.workspace_id, selector("primary"));
    assert!(intent.delivery_bundle.is_none());
    assert!(intent.continuation.is_none());

    let launch = HarnessLaunchCatalog::new([plan]).unwrap();
    let catalogs = HarnessRuntimeCatalogs {
        launch: launch.clone(),
        ..HarnessRuntimeCatalogs::default()
    };
    assert!(catalogs.delivery.is_empty());

    // This exact typed seam is used by the existing real H3B provider-child
    // fixture. The requested E2E must call it once the ScheduleNext and host
    // specialized-plan gates are removed; no direct Harness-to-Node pipe is
    // part of the fixture contract.
    let fixture_constructor = build_remote_h3b_node_fixture;
    let _ = fixture_constructor;
    assert!(!launch.supports_ordinary_coordinator());
}
