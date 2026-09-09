//! `gate4agent-harnessctl` — synchronous dev control plane for the harness
//! operator socket.
//!
//! Thin argv wrapper over the already-existing, blocking
//! `HarnessOperatorClient` (no tokio, no new server surface). Every
//! subcommand maps to exactly one typed client method; the response is
//! printed as pretty JSON on stdout so the caller can drive this from
//! scripts. Credentials are read only from `GATE4AGENT_HARNESS_OPERATOR_TOKEN`
//! — never accepted via argv.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::time::{SystemTime, UNIX_EPOCH};

use gate4agent_harness_client::{
    HarnessCreateTaskRequestV1, HarnessDeliveryBundleIdV1, HarnessDeliveryBundleSelectionV1,
    HarnessExpectedExecutionSpecRevisionV1, HarnessIdempotencyRef, HarnessMailAckOperatorRequestV1,
    HarnessMailMessageId, HarnessMailRefV1, HarnessMoveTaskRequestV1,
    HarnessOperationId, HarnessOperatorAuthorityV1, HarnessOperatorClient, HarnessOperatorCredential,
    HarnessOperatorMutationOutcomeV1, HarnessOrdinaryLaunchPlanOptionV1,
    HarnessReplaceTaskExecutionSpecRequestV2, HarnessReviewedTaskLaunchSelectionV1,
    HarnessReviewedWorktreeSelectionV1, HarnessRunId, HarnessRunLifecycleV1, HarnessSelectorV1,
    HarnessStartTaskRequestV2, HarnessTaskId, HarnessTaskLaunchOptionsV1, HarnessTaskReviewPolicyV1,
    HarnessTaskStateV1, RedactedRunV1, RedactedTaskV1, HARNESS_ENTITY_PAGE_LIMIT_MAX,
    HARNESS_RUNTIME_INVENTORY_PAGE_LIMIT_MAX,
};
use gate4agent_harness_api::{
    HarnessAgentStreamChunkKindV1, HarnessAgentStreamNamedIdV1, HarnessApprovalLevelV1,
    HarnessBlockAuthorityV1, HarnessExecutionModeV1,
    HarnessOperatorAgentEventV1, HarnessOperatorTerminalEventV1, HarnessProviderInteractionResponseV1,
    HarnessRuntimeSessionAddressV1,
    HarnessRuntimeTerminalSizeV1, HarnessTerminalControlV1,
};

const HARNESS_OPERATOR_TOKEN_ENV: &str = "GATE4AGENT_HARNESS_OPERATOR_TOKEN";

fn usage() -> &'static str {
    "usage: gate4agent-harnessctl <command> [args] --harness-operator HOST:PORT\n\
     credential env: GATE4AGENT_HARNESS_OPERATOR_TOKEN\n\
     \n\
     commands:\n\
     \x20 tasks list [--state STATE] [--after TASK_ID] [--limit N]\n\
     \x20 runs list [--task TASK_ID] [--lifecycle LIFECYCLE] [--after RUN_ID] [--limit N]\n\
     \x20 task get TASK_ID\n\
     \x20 run get RUN_ID\n\
     \x20 results TASK_ID\n\
     \x20 task create --title TITLE --body BODY [--parent TASK_ID] [--depends TASK_ID,TASK_ID]\n\
     \x20 task move TASK_ID --to STATE\n\
     \x20 task operations TASK_ID [--limit N]\n\
     \x20 launch-options TASK_ID [--provider ID] [--workspace ID] [--plan ID] [--after PLAN_ID]\n\
     \x20 spec save TASK_ID --plan PLAN_ID [--context-source-run RUN_ID] [--delivery BUNDLE_ID] [--review POLICY]\n\
     \x20 task start TASK_ID\n\
     \x20 observe-context RUN_ID\n\
     \x20 transfers RUN_ID\n\
     \x20 runtime-inventory [--after NODE_ID] [--limit N]\n\
     \x20 monitor RUN_ID\n\
     \x20 mail inbox RECORD_ID [--since MS] [--limit N]\n\
     \x20 mail ack RECORD_ID MESSAGE_ID\n\
     \x20 workspace inspect NODE_ID WORKSPACE_ID\n\
     \x20 session spawn NODE_ID WORKSPACE_ID PROVIDER [--profile ID] [--mode pty|inline|acp] [--rows N] [--cols N] [--approval moderate|full-auto|read-only|unmanaged]\n\
     \x20 session stop NODE_ID INCARNATION_ID WORKSPACE_ID INSTANCE_ID GENERATION [--force yes]\n\
     \x20 session resolve-interaction NODE_ID INCARNATION_ID WORKSPACE_ID INSTANCE_ID GENERATION CORRELATION_ID --response approve|deny|answer [--answer TEXT]\n\
     \x20 session set-mode NODE_ID INCARNATION_ID WORKSPACE_ID INSTANCE_ID GENERATION MODE_ID\n\
     \x20 session set-model NODE_ID INCARNATION_ID WORKSPACE_ID INSTANCE_ID GENERATION MODEL_ID\n\
     \x20 session set-config-option NODE_ID INCARNATION_ID WORKSPACE_ID INSTANCE_ID GENERATION OPTION_ID --value-json JSON\n\
     \x20 session prompt NODE_ID INCARNATION_ID WORKSPACE_ID INSTANCE_ID GENERATION TEXT (ACP/inline sessions only -- refused for a PTY session)\n\
     \x20 session input NODE_ID INCARNATION_ID WORKSPACE_ID INSTANCE_ID GENERATION TEXT (raw terminal input; the PTY counterpart of prompt)\n\
     \x20 session control NODE_ID INCARNATION_ID WORKSPACE_ID INSTANCE_ID GENERATION CONTROL (enter|interrupt|eof -- submit a typed PTY line)\n\
     \x20 session terminal NODE_ID INCARNATION_ID WORKSPACE_ID INSTANCE_ID GENERATION [...more session groups] (stream the PTY screen)\n\
     \x20 session subscribe NODE_ID INCARNATION_ID WORKSPACE_ID INSTANCE_ID GENERATION [...more session groups] [--verbose yes]"
}

#[derive(Debug)]
enum ParseOutcome {
    Run(Invocation),
    Help,
}

#[derive(Debug)]
struct Invocation {
    endpoint: SocketAddr,
    credential: HarnessOperatorCredential,
    command: Command,
}

#[derive(Debug)]
enum Command {
    TasksList { state: Option<HarnessTaskStateV1>, after: Option<HarnessTaskId>, limit: u16 },
    RunsList {
        task: Option<HarnessTaskId>,
        lifecycle: Option<HarnessRunLifecycleV1>,
        after: Option<HarnessRunId>,
        limit: u16,
    },
    TaskGet { task_id: HarnessTaskId },
    RunGet { run_id: HarnessRunId },
    Results { task_id: HarnessTaskId },
    TaskCreate {
        title: String,
        body: String,
        parent: Option<HarnessTaskId>,
        dependencies: Vec<HarnessTaskId>,
    },
    TaskMove { task_id: HarnessTaskId, to: HarnessTaskStateV1 },
    TaskOperations { task_id: HarnessTaskId, limit: u16 },
    LaunchOptions {
        task_id: HarnessTaskId,
        provider: Option<HarnessSelectorV1>,
        workspace: Option<HarnessSelectorV1>,
        plan_id: Option<HarnessSelectorV1>,
        after: Option<HarnessSelectorV1>,
    },
    SpecSave {
        task_id: HarnessTaskId,
        plan: String,
        context_source_run: Option<HarnessRunId>,
        delivery: Option<HarnessDeliveryBundleIdV1>,
        review: HarnessTaskReviewPolicyV1,
    },
    TaskStart { task_id: HarnessTaskId },
    ObserveContext { run_id: HarnessRunId },
    Transfers { run_id: HarnessRunId },
    RuntimeInventory { after: Option<String>, limit: u16 },
    Monitor { run_id: HarnessRunId },
    MailInbox { record_id: HarnessSelectorV1, since_unix_ms: Option<u64>, limit: u16 },
    MailAck { record_id: HarnessSelectorV1, message_id: HarnessMailMessageId },
    WorkspaceInspect { node_id: String, workspace_id: String },
    SessionSpawn {
        node_id: String,
        workspace_id: String,
        provider: String,
        provider_profile: String,
        mode: HarnessExecutionModeV1,
        rows: u16,
        cols: u16,
        approval_level: Option<HarnessApprovalLevelV1>,
    },
    SessionStop { session: HarnessRuntimeSessionAddressV1, force: bool },
    SessionResolveInteraction {
        session: HarnessRuntimeSessionAddressV1,
        correlation_id: String,
        response: HarnessProviderInteractionResponseV1,
    },
    SessionSetMode { session: HarnessRuntimeSessionAddressV1, mode_id: String },
    SessionPrompt { session: HarnessRuntimeSessionAddressV1, text: String },
    SessionInput { session: HarnessRuntimeSessionAddressV1, text: String },
    SessionControl { session: HarnessRuntimeSessionAddressV1, control: HarnessTerminalControlV1 },
    SessionTerminal { sessions: Vec<HarnessRuntimeSessionAddressV1> },
    SessionSetConfigOption {
        session: HarnessRuntimeSessionAddressV1,
        option_id: String,
        value_json: String,
    },
    SessionSetModel { session: HarnessRuntimeSessionAddressV1, model_id: String },
    SessionSubscribe { sessions: Vec<HarnessRuntimeSessionAddressV1>, verbose: bool },
}

enum Verb {
    TasksList,
    RunsList,
    TaskGet,
    RunGet,
    Results,
    TaskCreate,
    TaskMove,
    TaskOperations,
    LaunchOptions,
    SpecSave,
    TaskStart,
    ObserveContext,
    Transfers,
    RuntimeInventory,
    Monitor,
    MailInbox,
    MailAck,
    WorkspaceInspect,
    SessionSpawn,
    SessionStop,
    SessionResolveInteraction,
    SessionSetMode,
    SessionPrompt,
    SessionInput,
    SessionControl,
    SessionTerminal,
    SessionSetConfigOption,
    SessionSetModel,
    SessionSubscribe,
}

fn resolve_verb(args: &[String]) -> Result<(Verb, usize), String> {
    match args.get(1).map(String::as_str) {
        Some("tasks") if args.get(2).map(String::as_str) == Some("list") => Ok((Verb::TasksList, 2)),
        Some("runs") if args.get(2).map(String::as_str) == Some("list") => Ok((Verb::RunsList, 2)),
        Some("task") => match args.get(2).map(String::as_str) {
            Some("get") => Ok((Verb::TaskGet, 2)),
            Some("create") => Ok((Verb::TaskCreate, 2)),
            Some("move") => Ok((Verb::TaskMove, 2)),
            Some("operations") => Ok((Verb::TaskOperations, 2)),
            Some("start") => Ok((Verb::TaskStart, 2)),
            _ => Err(usage().to_owned()),
        },
        Some("run") if args.get(2).map(String::as_str) == Some("get") => Ok((Verb::RunGet, 2)),
        Some("results") => Ok((Verb::Results, 1)),
        Some("spec") if args.get(2).map(String::as_str) == Some("save") => Ok((Verb::SpecSave, 2)),
        Some("launch-options") => Ok((Verb::LaunchOptions, 1)),
        Some("observe-context") => Ok((Verb::ObserveContext, 1)),
        Some("transfers") => Ok((Verb::Transfers, 1)),
        Some("runtime-inventory") => Ok((Verb::RuntimeInventory, 1)),
        Some("monitor") => Ok((Verb::Monitor, 1)),
        Some("mail") => match args.get(2).map(String::as_str) {
            Some("inbox") => Ok((Verb::MailInbox, 2)),
            Some("ack") => Ok((Verb::MailAck, 2)),
            _ => Err(usage().to_owned()),
        },
        Some("session") => match args.get(2).map(String::as_str) {
            Some("spawn") => Ok((Verb::SessionSpawn, 2)),
            Some("stop") => Ok((Verb::SessionStop, 2)),
            Some("resolve-interaction") => Ok((Verb::SessionResolveInteraction, 2)),
            Some("set-mode") => Ok((Verb::SessionSetMode, 2)),
            Some("prompt") => Ok((Verb::SessionPrompt, 2)),
            Some("input") => Ok((Verb::SessionInput, 2)),
            Some("control") => Ok((Verb::SessionControl, 2)),
            Some("terminal") => Ok((Verb::SessionTerminal, 2)),
            Some("set-config-option") => Ok((Verb::SessionSetConfigOption, 2)),
            Some("set-model") => Ok((Verb::SessionSetModel, 2)),
            Some("subscribe") => Ok((Verb::SessionSubscribe, 2)),
            _ => Err(usage().to_owned()),
        },
        Some("workspace") if args.get(2).map(String::as_str) == Some("inspect") => {
            Ok((Verb::WorkspaceInspect, 2))
        }
        _ => Err(usage().to_owned()),
    }
}

/// The special keys `session control` accepts, by the name an operator would
/// say out loud. `enter` is the one this exists for: `session input` carries
/// printable text only -- the wire refuses a control character inside it --
/// so a PTY session's typed line is submitted by a separate control verb, not
/// by a trailing newline.
fn parse_terminal_control(key: &str) -> Result<HarnessTerminalControlV1, String> {
    match key.to_ascii_lowercase().as_str() {
        "enter" | "return" | "cr" | "ctrl-m" => Ok(HarnessTerminalControlV1::ControlM),
        "interrupt" | "ctrl-c" => Ok(HarnessTerminalControlV1::Interrupt),
        "eof" | "ctrl-d" => Ok(HarnessTerminalControlV1::EndOfFile),
        other => Err(format!(
            "unknown control key {other:?}; known: enter, interrupt, eof"
        )),
    }
}

struct ParsedArgs {
    positionals: Vec<String>,
    flags: BTreeMap<String, String>,
}

fn parse_flags_and_positionals(args: &[String]) -> Result<ParsedArgs, String> {
    let mut positionals = Vec::new();
    let mut flags = BTreeMap::new();
    let mut index = 0;
    while index < args.len() {
        let token = &args[index];
        match token.strip_prefix("--") {
            Some(flag) if !flag.is_empty() => {
                index += 1;
                let value = args
                    .get(index)
                    .cloned()
                    .ok_or_else(|| format!("--{flag} requires a value"))?;
                if flags.insert(flag.to_owned(), value).is_some() {
                    return Err(format!("--{flag} can be specified only once"));
                }
            }
            _ => positionals.push(token.clone()),
        }
        index += 1;
    }
    Ok(ParsedArgs { positionals, flags })
}

fn take_flag(flags: &mut BTreeMap<String, String>, name: &str) -> Option<String> {
    flags.remove(name)
}

fn require_flag(flags: &mut BTreeMap<String, String>, name: &str) -> Result<String, String> {
    take_flag(flags, name).ok_or_else(|| format!("--{name} is required"))
}

fn reject_unknown_flags(flags: &BTreeMap<String, String>) -> Result<(), String> {
    if let Some(name) = flags.keys().next() {
        return Err(format!("unknown flag --{name}"));
    }
    Ok(())
}

fn expect_no_positionals(positionals: &[String], verb_label: &str) -> Result<(), String> {
    if !positionals.is_empty() {
        return Err(format!("{verb_label} takes no positional arguments"));
    }
    Ok(())
}

fn expect_single_positional(positionals: &mut Vec<String>, label: &str) -> Result<String, String> {
    if positionals.len() != 1 {
        return Err(format!("expected exactly one <{label}> argument"));
    }
    Ok(positionals.remove(0))
}

fn expect_two_positionals(
    positionals: &mut Vec<String>,
    labels: (&str, &str),
) -> Result<(String, String), String> {
    if positionals.len() != 2 {
        return Err(format!("expected exactly two <{}> <{}> arguments", labels.0, labels.1));
    }
    let second = positionals.remove(1);
    let first = positionals.remove(0);
    Ok((first, second))
}

/// Drains the leading `NODE_ID INCARNATION_ID WORKSPACE_ID INSTANCE_ID
/// GENERATION` quintuple every session-address subcommand shares --
/// `session spawn`/`session stop`'s own addressing, factored out so the four
/// new session verbs and `session subscribe`'s repeatable groups address a
/// session the same way.
fn take_session_address_prefix(
    positionals: &mut Vec<String>,
) -> Result<HarnessRuntimeSessionAddressV1, String> {
    if positionals.len() < 5 {
        return Err(
            "expected at least <node-id> <incarnation-id> <workspace-id> <instance-id> <generation>"
                .to_owned(),
        );
    }
    let mut drained = positionals.drain(..5);
    let node_id = drained.next().expect("length checked above");
    let incarnation_id = drained.next().expect("length checked above");
    let workspace_id = drained.next().expect("length checked above");
    let instance_id = drained
        .next()
        .expect("length checked above")
        .parse()
        .map_err(|_| "INSTANCE_ID must be a u64".to_owned())?;
    let generation = drained
        .next()
        .expect("length checked above")
        .parse()
        .map_err(|_| "GENERATION must be a u64".to_owned())?;
    Ok(HarnessRuntimeSessionAddressV1 { node_id, incarnation_id, workspace_id, instance_id, generation })
}

fn parse_task_id(value: String) -> Result<HarnessTaskId, String> {
    HarnessTaskId::new(value).map_err(|_| "invalid task id".to_owned())
}

/// `--depends a,b` -> the wire's `dependencies`, which
/// `validate_sorted_ids` requires to be strictly sorted and duplicate-free.
/// Sorting and de-duplicating here rather than refusing keeps the operator
/// from having to hand-order ids, and a duplicate is a typo, not an intent.
/// Until this flag existed the CLI hardcoded `dependencies: Vec::new()`, so
/// a dependency could not be expressed from any live client at all -- which
/// is why, across 146 live tasks, not one had ever carried one.
fn parse_dependencies(value: Option<String>) -> Result<Vec<HarnessTaskId>, String> {
    let Some(value) = value else { return Ok(Vec::new()) };
    let mut ids = value
        .split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(|entry| parse_task_id(entry.to_owned()))
        .collect::<Result<Vec<_>, _>>()?;
    ids.sort();
    ids.dedup();
    Ok(ids)
}

fn parse_run_id(value: String) -> Result<HarnessRunId, String> {
    HarnessRunId::new(value).map_err(|_| "invalid run id".to_owned())
}

fn parse_record_id(value: String) -> Result<HarnessSelectorV1, String> {
    HarnessSelectorV1::new(value).map_err(|_| "invalid record id".to_owned())
}

/// `--provider`/`--workspace`/`--plan`/`--after` on `launch-options` all name
/// a `HarnessSelectorV1` (a provider id, workspace id, or plan id -- the
/// three fields `HarnessOrdinaryLaunchPlanOptionV1` filters on).
fn parse_selector(value: String) -> Result<HarnessSelectorV1, String> {
    HarnessSelectorV1::new(value.clone()).map_err(|_| format!("invalid selector value: {value}"))
}

fn parse_mail_message_id(value: String) -> Result<HarnessMailMessageId, String> {
    HarnessMailMessageId::new(value).map_err(|_| "invalid mail message id".to_owned())
}

/// D3/Slice C: names one attached ref's kind for `mail inbox`'s one-line
/// summary -- never the ref's own content (a run id, a digest, a path), just
/// which of the four fixed kinds it is.
fn mail_ref_kind_name(reference: &HarnessMailRefV1) -> &'static str {
    match reference {
        HarnessMailRefV1::Run { .. } => "run",
        HarnessMailRefV1::ContextPack { .. } => "context-pack",
        HarnessMailRefV1::Result { .. } => "result",
        HarnessMailRefV1::WorkspacePath { .. } => "workspace-path",
    }
}

fn parse_kebab<T: serde::de::DeserializeOwned>(value: &str, flag: &str) -> Result<T, String> {
    serde_json::from_value(serde_json::Value::String(value.to_owned()))
        .map_err(|_| format!("{flag} has an invalid value: {value}"))
}

fn parse_limit(flags: &mut BTreeMap<String, String>, default: u16) -> Result<u16, String> {
    match take_flag(flags, "limit") {
        Some(value) => value
            .parse::<u16>()
            .map_err(|_| format!("--limit must be a non-negative integer: {value}")),
        None => Ok(default),
    }
}

fn parse_review_policy(flags: &mut BTreeMap<String, String>) -> Result<HarnessTaskReviewPolicyV1, String> {
    match take_flag(flags, "review") {
        Some(value) => parse_kebab(&value, "--review"),
        None => Ok(HarnessTaskReviewPolicyV1::OperatorReview),
    }
}

fn parse_harness_operator_endpoint(value: &str) -> Result<SocketAddr, String> {
    let endpoint = value
        .parse::<SocketAddr>()
        .map_err(|_| "--harness-operator must be an IP socket address".to_owned())?;
    if !endpoint.ip().is_loopback() || endpoint.port() == 0 {
        return Err("--harness-operator must be a concrete loopback socket address".to_owned());
    }
    Ok(endpoint)
}

fn build_command(
    verb: Verb,
    positionals: &mut Vec<String>,
    flags: &mut BTreeMap<String, String>,
) -> Result<Command, String> {
    match verb {
        Verb::TasksList => {
            expect_no_positionals(positionals, "tasks list")?;
            let state = take_flag(flags, "state")
                .map(|value| parse_kebab::<HarnessTaskStateV1>(&value, "--state"))
                .transpose()?;
            let after = take_flag(flags, "after").map(parse_task_id).transpose()?;
            let limit = parse_limit(flags, HARNESS_ENTITY_PAGE_LIMIT_MAX)?;
            Ok(Command::TasksList { state, after, limit })
        }
        Verb::RunsList => {
            expect_no_positionals(positionals, "runs list")?;
            let task = take_flag(flags, "task").map(parse_task_id).transpose()?;
            let lifecycle = take_flag(flags, "lifecycle")
                .map(|value| parse_kebab::<HarnessRunLifecycleV1>(&value, "--lifecycle"))
                .transpose()?;
            let after = take_flag(flags, "after").map(parse_run_id).transpose()?;
            let limit = parse_limit(flags, HARNESS_ENTITY_PAGE_LIMIT_MAX)?;
            Ok(Command::RunsList { task, lifecycle, after, limit })
        }
        Verb::TaskGet => {
            let task_id = expect_single_positional(positionals, "task-id").and_then(parse_task_id)?;
            Ok(Command::TaskGet { task_id })
        }
        Verb::RunGet => {
            let run_id = expect_single_positional(positionals, "run-id").and_then(parse_run_id)?;
            Ok(Command::RunGet { run_id })
        }
        Verb::Results => {
            let task_id = expect_single_positional(positionals, "task-id").and_then(parse_task_id)?;
            Ok(Command::Results { task_id })
        }
        Verb::TaskCreate => {
            expect_no_positionals(positionals, "task create")?;
            let title = require_flag(flags, "title")?;
            let body = require_flag(flags, "body")?;
            let parent = take_flag(flags, "parent").map(parse_task_id).transpose()?;
            let dependencies = parse_dependencies(take_flag(flags, "depends"))?;
            Ok(Command::TaskCreate { title, body, parent, dependencies })
        }
        Verb::TaskMove => {
            let task_id = expect_single_positional(positionals, "task-id").and_then(parse_task_id)?;
            let to = parse_kebab::<HarnessTaskStateV1>(&require_flag(flags, "to")?, "--to")?;
            Ok(Command::TaskMove { task_id, to })
        }
        Verb::TaskOperations => {
            let task_id = expect_single_positional(positionals, "task-id").and_then(parse_task_id)?;
            let limit = parse_limit(flags, HARNESS_ENTITY_PAGE_LIMIT_MAX)?;
            Ok(Command::TaskOperations { task_id, limit })
        }
        Verb::LaunchOptions => {
            let task_id = expect_single_positional(positionals, "task-id").and_then(parse_task_id)?;
            let provider = take_flag(flags, "provider").map(parse_selector).transpose()?;
            let workspace = take_flag(flags, "workspace").map(parse_selector).transpose()?;
            let plan_id = take_flag(flags, "plan").map(parse_selector).transpose()?;
            let after = take_flag(flags, "after").map(parse_selector).transpose()?;
            Ok(Command::LaunchOptions { task_id, provider, workspace, plan_id, after })
        }
        Verb::SpecSave => {
            let task_id = expect_single_positional(positionals, "task-id").and_then(parse_task_id)?;
            let plan = require_flag(flags, "plan")?;
            let context_source_run = take_flag(flags, "context-source-run")
                .map(parse_run_id)
                .transpose()?;
            let delivery = take_flag(flags, "delivery")
                .map(|value| HarnessDeliveryBundleIdV1::new(value).map_err(|_| "invalid --delivery id".to_owned()))
                .transpose()?;
            let review = parse_review_policy(flags)?;
            Ok(Command::SpecSave { task_id, plan, context_source_run, delivery, review })
        }
        Verb::TaskStart => {
            let task_id = expect_single_positional(positionals, "task-id").and_then(parse_task_id)?;
            Ok(Command::TaskStart { task_id })
        }
        Verb::ObserveContext => {
            let run_id = expect_single_positional(positionals, "run-id").and_then(parse_run_id)?;
            Ok(Command::ObserveContext { run_id })
        }
        Verb::Transfers => {
            let run_id = expect_single_positional(positionals, "run-id").and_then(parse_run_id)?;
            Ok(Command::Transfers { run_id })
        }
        Verb::RuntimeInventory => {
            expect_no_positionals(positionals, "runtime-inventory")?;
            let after = take_flag(flags, "after");
            let limit = parse_limit(flags, HARNESS_RUNTIME_INVENTORY_PAGE_LIMIT_MAX)?;
            Ok(Command::RuntimeInventory { after, limit })
        }
        Verb::Monitor => {
            let run_id = expect_single_positional(positionals, "run-id").and_then(parse_run_id)?;
            Ok(Command::Monitor { run_id })
        }
        Verb::MailInbox => {
            let record_id = expect_single_positional(positionals, "record-id").and_then(parse_record_id)?;
            let since_unix_ms = take_flag(flags, "since")
                .map(|value| value.parse::<u64>().map_err(|_| "--since must be a u64".to_owned()))
                .transpose()?;
            let limit = parse_limit(flags, HARNESS_ENTITY_PAGE_LIMIT_MAX)?;
            Ok(Command::MailInbox { record_id, since_unix_ms, limit })
        }
        Verb::MailAck => {
            let (record_id, message_id) =
                expect_two_positionals(positionals, ("record-id", "message-id"))?;
            let record_id = parse_record_id(record_id)?;
            let message_id = parse_mail_message_id(message_id)?;
            Ok(Command::MailAck { record_id, message_id })
        }
        Verb::WorkspaceInspect => {
            let (node_id, workspace_id) =
                expect_two_positionals(positionals, ("node-id", "workspace-id"))?;
            Ok(Command::WorkspaceInspect { node_id, workspace_id })
        }
        Verb::SessionSpawn => {
            if positionals.len() != 3 {
                return Err(
                    "session spawn expects exactly NODE_ID WORKSPACE_ID PROVIDER".to_owned()
                );
            }
            let mut positionals = positionals.drain(..);
            let node_id = positionals.next().expect("length checked above");
            let workspace_id = positionals.next().expect("length checked above");
            let provider = positionals.next().expect("length checked above");
            let provider_profile =
                take_flag(flags, "profile").unwrap_or_else(|| "default".to_owned());
            let mode = match take_flag(flags, "mode").as_deref() {
                None | Some("pty") => HarnessExecutionModeV1::Pty,
                Some("inline") => HarnessExecutionModeV1::Inline,
                Some("acp") => HarnessExecutionModeV1::Acp,
                Some(other) => {
                    return Err(format!("--mode must be pty, inline, or acp, got {other}"))
                }
            };
            let rows = match take_flag(flags, "rows") {
                Some(value) => value.parse().map_err(|_| "--rows must be a u16".to_owned())?,
                None => 40,
            };
            let cols = match take_flag(flags, "cols") {
                Some(value) => value.parse().map_err(|_| "--cols must be a u16".to_owned())?,
                None => 140,
            };
            // Absent by default -- `None` on the wire is the axis default
            // (`FullAuto`), unchanged from before this flag existed.
            let approval_level = match take_flag(flags, "approval").as_deref() {
                None => None,
                Some("full-auto") => Some(HarnessApprovalLevelV1::FullAuto),
                Some("moderate") => Some(HarnessApprovalLevelV1::Moderate),
                Some("read-only") => Some(HarnessApprovalLevelV1::ReadOnly),
                Some("unmanaged") => Some(HarnessApprovalLevelV1::Unmanaged),
                Some(other) => {
                    return Err(format!(
                        "--approval must be moderate, full-auto, read-only, or unmanaged, got {other}"
                    ))
                }
            };
            Ok(Command::SessionSpawn {
                node_id,
                workspace_id,
                provider,
                provider_profile,
                mode,
                rows,
                cols,
                approval_level,
            })
        }
        Verb::SessionStop => {
            if positionals.len() != 5 {
                return Err("session stop expects exactly NODE_ID INCARNATION_ID WORKSPACE_ID INSTANCE_ID GENERATION".to_owned());
            }
            let mut positionals = positionals.drain(..);
            let node_id = positionals.next().expect("length checked above");
            let incarnation_id = positionals.next().expect("length checked above");
            let workspace_id = positionals.next().expect("length checked above");
            let instance_id = positionals
                .next()
                .expect("length checked above")
                .parse()
                .map_err(|_| "INSTANCE_ID must be a u64".to_owned())?;
            let generation = positionals
                .next()
                .expect("length checked above")
                .parse()
                .map_err(|_| "GENERATION must be a u64".to_owned())?;
            let force = take_flag(flags, "force").is_some();
            Ok(Command::SessionStop {
                session: HarnessRuntimeSessionAddressV1 {
                    node_id,
                    incarnation_id,
                    workspace_id,
                    instance_id,
                    generation,
                },
                force,
            })
        }
        Verb::SessionResolveInteraction => {
            let session = take_session_address_prefix(positionals)?;
            let correlation_id = expect_single_positional(positionals, "correlation-id")?;
            let response_kind = require_flag(flags, "response")?;
            let response = match response_kind.as_str() {
                "approve" => HarnessProviderInteractionResponseV1::ApproveOnce,
                "deny" => HarnessProviderInteractionResponseV1::Deny,
                "answer" => {
                    let text = require_flag(flags, "answer")?;
                    HarnessProviderInteractionResponseV1::Answer { text }
                }
                other => {
                    return Err(format!("--response must be approve, deny, or answer, got {other}"))
                }
            };
            Ok(Command::SessionResolveInteraction { session, correlation_id, response })
        }
        Verb::SessionSetMode => {
            let session = take_session_address_prefix(positionals)?;
            let mode_id = expect_single_positional(positionals, "mode-id")?;
            Ok(Command::SessionSetMode { session, mode_id })
        }
        Verb::SessionPrompt => {
            let session = take_session_address_prefix(positionals)?;
            let text = expect_single_positional(positionals, "text")?;
            Ok(Command::SessionPrompt { session, text })
        }
        Verb::SessionInput => {
            let session = take_session_address_prefix(positionals)?;
            let text = expect_single_positional(positionals, "text")?;
            Ok(Command::SessionInput { session, text })
        }
        Verb::SessionControl => {
            let session = take_session_address_prefix(positionals)?;
            let key = expect_single_positional(positionals, "control")?;
            let control = parse_terminal_control(&key)?;
            Ok(Command::SessionControl { session, control })
        }
        Verb::SessionSetConfigOption => {
            let session = take_session_address_prefix(positionals)?;
            let option_id = expect_single_positional(positionals, "option-id")?;
            let value_json = require_flag(flags, "value-json")?;
            Ok(Command::SessionSetConfigOption { session, option_id, value_json })
        }
        Verb::SessionSetModel => {
            let session = take_session_address_prefix(positionals)?;
            let model_id = expect_single_positional(positionals, "model-id")?;
            Ok(Command::SessionSetModel { session, model_id })
        }
        Verb::SessionSubscribe => {
            if positionals.is_empty() || positionals.len() % 5 != 0 {
                return Err(
                    "session subscribe expects one or more NODE_ID INCARNATION_ID WORKSPACE_ID \
                     INSTANCE_ID GENERATION groups"
                        .to_owned(),
                );
            }
            let mut sessions = Vec::new();
            while !positionals.is_empty() {
                sessions.push(take_session_address_prefix(positionals)?);
            }
            let verbose = take_flag(flags, "verbose").is_some();
            Ok(Command::SessionSubscribe { sessions, verbose })
        }
        Verb::SessionTerminal => {
            if positionals.is_empty() || positionals.len() % 5 != 0 {
                return Err(
                    "session terminal expects one or more NODE_ID INCARNATION_ID WORKSPACE_ID \
                     INSTANCE_ID GENERATION groups"
                        .to_owned(),
                );
            }
            let mut sessions = Vec::new();
            while !positionals.is_empty() {
                sessions.push(take_session_address_prefix(positionals)?);
            }
            Ok(Command::SessionTerminal { sessions })
        }
    }
}

fn parse_args_from(
    args: &[String],
    mut read_secret: impl FnMut(&str) -> Result<String, String>,
) -> Result<ParseOutcome, String> {
    if args.iter().skip(1).any(|argument| argument == "--help" || argument == "-h") {
        return Ok(ParseOutcome::Help);
    }
    let (verb, consumed) = resolve_verb(args)?;
    let rest = &args[1 + consumed..];
    let ParsedArgs { mut positionals, mut flags } = parse_flags_and_positionals(rest)?;
    let endpoint = require_flag(&mut flags, "harness-operator")
        .and_then(|value| parse_harness_operator_endpoint(&value))?;
    let command = build_command(verb, &mut positionals, &mut flags)?;
    reject_unknown_flags(&flags)?;
    let token = read_secret(HARNESS_OPERATOR_TOKEN_ENV)?;
    let credential = HarnessOperatorCredential::parse(token)
        .map_err(|_| format!("{HARNESS_OPERATOR_TOKEN_ENV} is malformed"))?;
    Ok(ParseOutcome::Run(Invocation { endpoint, credential, command }))
}

fn parse_args() -> Result<ParseOutcome, String> {
    let args = std::env::args().collect::<Vec<_>>();
    parse_args_from(&args, |name| {
        let value = std::env::var(name)
            .map_err(|_| format!("{name} is required and must be valid Unicode"))?;
        std::env::remove_var(name);
        Ok(value)
    })
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

/// Per-invocation, non-cryptographic hex identifier for optimistic-concurrency
/// authority fields (`operation_id`/`idempotency_ref`/generated `task_id`).
/// `harnessctl` is an interactively-run dev tool, not a replay-safe client:
/// each invocation is a fresh, one-off operator action, so uniqueness (not
/// unpredictability) is all that is required here.
fn random_hex24(salt: u64) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut hasher = DefaultHasher::new();
    now_unix_ms().hash(&mut hasher);
    std::process::id().hash(&mut hasher);
    salt.hash(&mut hasher);
    let first = hasher.finish();
    first.hash(&mut hasher);
    let second = hasher.finish();
    format!("{first:016x}{second:016x}")[..24].to_owned()
}

fn fresh_authority() -> Result<HarnessOperatorAuthorityV1, String> {
    Ok(HarnessOperatorAuthorityV1 {
        operation_id: HarnessOperationId::new(format!("hop_{}", random_hex24(1)))
            .map_err(|_| "failed to construct a fresh operation id".to_owned())?,
        idempotency_ref: HarnessIdempotencyRef::new(format!("hidem_{}", random_hex24(2)))
            .map_err(|_| "failed to construct a fresh idempotency reference".to_owned())?,
        actor_id: HarnessSelectorV1::new("harnessctl")
            .map_err(|_| "failed to construct the harnessctl actor id".to_owned())?,
        now_unix_ms: now_unix_ms(),
    })
}

fn fresh_task_id() -> Result<HarnessTaskId, String> {
    HarnessTaskId::new(format!("htask_{}", random_hex24(3)))
        .map_err(|_| "failed to construct a fresh task id".to_owned())
}

fn resolve_plan(
    options: &HarnessTaskLaunchOptionsV1,
    plan_id: &str,
) -> Result<HarnessOrdinaryLaunchPlanOptionV1, String> {
    options
        .plans
        .iter()
        .find(|option| option.plan.plan_id.as_str() == plan_id)
        .cloned()
        .ok_or_else(|| format!("--plan {plan_id} is not a current launch option; re-run launch-options"))
}

fn resolve_context_source(
    options: &HarnessTaskLaunchOptionsV1,
    run_id: &HarnessRunId,
) -> Result<gate4agent_harness_client::HarnessContextSourceSelectionV1, String> {
    options
        .context_sources
        .iter()
        .find(|source| &source.source_run_id == run_id)
        .cloned()
        .ok_or_else(|| {
            format!("--context-source-run {run_id} is not a current launch option; re-run launch-options")
        })
}

fn resolve_delivery(
    options: &HarnessTaskLaunchOptionsV1,
    bundle_id: &HarnessDeliveryBundleIdV1,
) -> Result<HarnessDeliveryBundleSelectionV1, String> {
    options
        .delivery_bundles
        .iter()
        .find(|bundle| &bundle.bundle.bundle_id == bundle_id)
        .cloned()
        .ok_or_else(|| {
            format!(
                "--delivery {} is not a current launch option; re-run launch-options",
                bundle_id.as_str(),
            )
        })
}

#[derive(serde::Serialize)]
struct TaskCreated {
    task_id: HarnessTaskId,
    outcome: HarnessOperatorMutationOutcomeV1,
}

/// `results TASK_ID` aggregate — zero new server RPC, a client-side loop
/// over the two already-existing `task_get`/`run_get` methods (same
/// "zero new server surface" discipline as every other subcommand here).
#[derive(serde::Serialize)]
struct TaskResults {
    task: RedactedTaskV1,
    runs: Vec<RedactedRunV1>,
}

fn render<T: serde::Serialize>(value: &T) -> Result<String, String> {
    serde_json::to_string_pretty(value).map_err(|error| error.to_string())
}

fn execute(invocation: Invocation) -> Result<String, String> {
    let client = HarnessOperatorClient::new(invocation.endpoint, invocation.credential)
        .map_err(|error| error.to_string())?;
    match invocation.command {
        Command::TasksList { state, after, limit } => {
            let page = client.tasks_list(after, state, limit).map_err(|error| error.to_string())?;
            render(&page)
        }
        Command::RunsList { task, lifecycle, after, limit } => {
            let page = client
                .runs_list(task, after, lifecycle, limit)
                .map_err(|error| error.to_string())?;
            render(&page)
        }
        Command::TaskGet { task_id } => {
            let task = client.task_get(task_id).map_err(|error| error.to_string())?;
            render(&task)
        }
        Command::RunGet { run_id } => {
            let run = client.run_get(run_id).map_err(|error| error.to_string())?;
            render(&run)
        }
        Command::Results { task_id } => {
            let task = client.task_get(task_id).map_err(|error| error.to_string())?;
            let mut runs = task
                .run_ids
                .iter()
                .map(|run_id| client.run_get(run_id.clone()).map_err(|error| error.to_string()))
                .collect::<Result<Vec<_>, _>>()?;
            runs.sort_by_key(|run| run.created_at_unix_ms);
            render(&TaskResults { task, runs })
        }
        Command::TaskCreate { title, body, parent, dependencies } => {
            let task_id = fresh_task_id()?;
            let authority = fresh_authority()?;
            let outcome = client
                .create_task(HarnessCreateTaskRequestV1 {
                    authority,
                    task_id: task_id.clone(),
                    title,
                    body,
                    parent_task_id: parent,
                    dependencies,
                    initial_state: HarnessTaskStateV1::Backlog,
                })
                .map_err(|error| error.to_string())?;
            render(&TaskCreated { task_id, outcome })
        }
        Command::TaskMove { task_id, to } => {
            let task = client.task_get(task_id.clone()).map_err(|error| error.to_string())?;
            let authority = fresh_authority()?;
            let outcome = client
                .move_task(HarnessMoveTaskRequestV1 {
                    authority,
                    task_id,
                    expected_revision: task.revision,
                    state: to,
                })
                .map_err(|error| error.to_string())?;
            render(&outcome)
        }
        Command::TaskOperations { task_id, limit } => {
            let ledger = client.task_operations(task_id, limit).map_err(|error| error.to_string())?;
            render(&ledger)
        }
        Command::LaunchOptions { task_id, provider, workspace, plan_id, after } => {
            let options = client
                .task_launch_options_get_page(task_id, provider, workspace, plan_id, after)
                .map_err(|error| error.to_string())?;
            render(&options)
        }
        Command::SpecSave { task_id, plan, context_source_run, delivery, review } => {
            // One request, no paging loop: the catalogue's derived plan list
            // can exceed a single page (`HARNESS_TASK_LAUNCH_OPTIONS_MAX`),
            // so resolve `--plan` through the `plan_id` filter rather than
            // scanning `launch-options`'s own unfiltered first page -- a
            // plan sitting past that page boundary must still resolve here.
            let plan_selector = parse_selector(plan.clone())?;
            let options = client
                .task_launch_options_get_page(
                    task_id.clone(), None, None, Some(plan_selector), None,
                )
                .map_err(|error| error.to_string())?;
            let plan = resolve_plan(&options, &plan)?;
            let context_source = context_source_run
                .map(|run_id| resolve_context_source(&options, &run_id))
                .transpose()?;
            let delivery = delivery
                .map(|bundle_id| resolve_delivery(&options, &bundle_id))
                .transpose()?;
            let expected_execution_spec_revision = match &options.current_issued_spec {
                Some(current) => HarnessExpectedExecutionSpecRevisionV1::Exact(current.revision),
                None => HarnessExpectedExecutionSpecRevisionV1::Absent,
            };
            let authority = fresh_authority()?;
            let outcome = client
                .replace_task_execution_spec_v2(HarnessReplaceTaskExecutionSpecRequestV2 {
                    authority,
                    task_id,
                    expected_task_revision: options.task_revision,
                    expected_execution_spec_revision,
                    selection: HarnessReviewedTaskLaunchSelectionV1 {
                        plan,
                        worktree: HarnessReviewedWorktreeSelectionV1::Existing,
                        context_source,
                        delivery,
                        review_policy: review,
                    },
                })
                .map_err(|error| error.to_string())?;
            render(&outcome)
        }
        Command::TaskStart { task_id } => {
            let options = client
                .task_launch_options_get(task_id.clone())
                .map_err(|error| error.to_string())?;
            let current = options
                .current_issued_spec
                .ok_or_else(|| "task has no issued execution spec; run `spec save` first".to_owned())?;
            let authority = fresh_authority()?;
            let outcome = client
                .start_task_v2(HarnessStartTaskRequestV2 {
                    authority,
                    task_id,
                    expected_task_revision: options.task_revision,
                    expected_execution_spec_revision: current.revision,
                    expected_launch_issuance: current.launch_issuance,
                })
                .map_err(|error| error.to_string())?;
            render(&outcome)
        }
        Command::ObserveContext { run_id } => {
            let observation = client
                .observe_run_context_source(run_id)
                .map_err(|error| error.to_string())?;
            render(&observation)
        }
        Command::Transfers { run_id } => {
            let transfer = client.run_transfer_get(run_id).map_err(|error| error.to_string())?;
            render(&transfer)
        }
        Command::RuntimeInventory { after, limit } => {
            let page = client
                .runtime_inventory_list(after, limit)
                .map_err(|error| error.to_string())?;
            render(&page)
        }
        Command::Monitor { run_id } => {
            let monitor = client.monitor_get(run_id).map_err(|error| error.to_string())?;
            render(&monitor)
        }
        Command::MailInbox { record_id, since_unix_ms, limit } => {
            let page = client
                .mail_inbox(record_id, since_unix_ms, limit)
                .map_err(|error| error.to_string())?;
            let mut lines = Vec::with_capacity(page.messages.len() * 2 + 1);
            for message in &page.messages {
                lines.push(serde_json::to_string(message).map_err(|error| error.to_string())?);
                if !message.refs.is_empty() {
                    let kinds = message.refs.iter().map(mail_ref_kind_name).collect::<Vec<_>>().join(",");
                    lines.push(format!("  refs[{}]: {kinds}", message.refs.len()));
                }
            }
            lines.push(format!("{{\"unacked\":{}}}", page.unacked));
            Ok(lines.join("\n"))
        }
        Command::MailAck { record_id, message_id } => {
            let authority = fresh_authority()?;
            let outcome = client
                .ack_mail(HarnessMailAckOperatorRequestV1 { authority, record_id, message_id })
                .map_err(|error| error.to_string())?;
            render(&outcome)
        }
        Command::WorkspaceInspect { node_id, workspace_id } => {
            let inspection = client
                .inspect_node_workspace(node_id, workspace_id)
                .map_err(|error| error.to_string())?;
            render(&inspection)
        }
        Command::SessionSpawn {
            node_id,
            workspace_id,
            provider,
            provider_profile,
            mode,
            rows,
            cols,
            approval_level,
        } => {
            let session = client
                .spawn_session(
                    node_id,
                    workspace_id,
                    provider,
                    provider_profile,
                    mode,
                    HarnessRuntimeTerminalSizeV1 { rows, columns: cols },
                    approval_level,
                )
                .map_err(|error| error.to_string())?;
            render(&session)
        }
        Command::SessionStop { session, force } => {
            client
                .stop_session(session, force)
                .map_err(|error| error.to_string())?;
            render(&serde_json::json!({ "stopped": true }))
        }
        Command::SessionResolveInteraction { session, correlation_id, response } => {
            client
                .resolve_interaction(session, correlation_id, response)
                .map_err(|error| error.to_string())?;
            render(&serde_json::json!({ "resolved": true }))
        }
        Command::SessionSetMode { session, mode_id } => {
            client.set_session_mode(session, mode_id).map_err(|error| error.to_string())?;
            render(&serde_json::json!({ "mode_set": true }))
        }
        Command::SessionPrompt { session, text } => {
            client.prompt_session(session, text).map_err(|error| error.to_string())?;
            render(&serde_json::json!({ "prompted": true }))
        }
        Command::SessionInput { session, text } => {
            client.write_session_input(session, text).map_err(|error| error.to_string())?;
            render(&serde_json::json!({ "input_written": true }))
        }
        Command::SessionControl { session, control } => {
            client.control_session(session, control).map_err(|error| error.to_string())?;
            render(&serde_json::json!({ "controlled": true }))
        }
        Command::SessionSetConfigOption { session, option_id, value_json } => {
            client
                .set_session_config_option(session, option_id, value_json)
                .map_err(|error| error.to_string())?;
            render(&serde_json::json!({ "config_option_set": true }))
        }
        Command::SessionSetModel { session, model_id } => {
            client.set_session_model(session, model_id).map_err(|error| error.to_string())?;
            render(&serde_json::json!({ "model_set": true }))
        }
        Command::SessionSubscribe { sessions, verbose } => {
            let mut subscription = client
                .subscribe_agent_stream(sessions)
                .map_err(|error| error.to_string())?;
            loop {
                let event = subscription.next_event().map_err(|error| error.to_string())?;
                print_agent_stream_event(&event, verbose);
            }
        }
        Command::SessionTerminal { sessions } => {
            let mut subscription = client
                .subscribe_terminal(sessions)
                .map_err(|error| error.to_string())?;
            loop {
                let event = subscription.next_event().map_err(|error| error.to_string())?;
                print_terminal_event(&event);
            }
        }
    }
}

/// Renders a terminal subscription event as the screen itself. A
/// `TerminalFrame` is always a FULL screen (never a delta), so printing
/// `formatted` verbatim is the whole picture -- which is the point: driving a
/// PTY agent blind, by `session input` and `session control` with no way to
/// read what the pane actually shows, is how a swallowed keystroke or an
/// unexpected onboarding screen stays invisible.
fn print_terminal_event(event: &HarnessOperatorTerminalEventV1) {
    match event {
        HarnessOperatorTerminalEventV1::TerminalFrame { sequence, session, frame, coalesced_since_last } => {
            println!(
                "--- frame seq={sequence} session={} cursor={},{} size={}x{} alt={} coalesced={coalesced_since_last}",
                format_session_address(session),
                frame.cursor_row,
                frame.cursor_column,
                frame.size.rows,
                frame.size.columns,
                frame.alternate_screen,
            );
            println!("{}", String::from_utf8_lossy(&frame.formatted));
        }
        HarnessOperatorTerminalEventV1::Ping { sequence } => println!("ping seq={sequence}"),
    }
}

/// Colon-joined rendering of a session address for one-line-per-event
/// output -- the same five fields `session spawn`/`session stop` take as
/// positionals, in the same order.
fn format_session_address(session: &HarnessRuntimeSessionAddressV1) -> String {
    format!(
        "{}:{}:{}:{}:{}",
        session.node_id, session.incarnation_id, session.workspace_id, session.instance_id, session.generation,
    )
}

/// Escapes the three control bytes `HarnessAgentStreamChunkKindV1::validate`
/// still allows through free text (`\n`, `\r`, `\t`) so every printed event
/// stays exactly one line -- greppability is the whole point of this
/// printer, and an unescaped newline inside e.g. a `Text` chunk would split
/// one event across two lines.
fn sanitize_line(text: &str) -> String {
    text.replace('\\', "\\\\").replace('\n', "\\n").replace('\r', "\\r").replace('\t', "\\t")
}

fn render_named_id_catalog(available: &[HarnessAgentStreamNamedIdV1]) -> String {
    available.iter().map(|entry| format!("{}:{}", entry.id, entry.name)).collect::<Vec<_>>().join(",")
}

/// The kebab-case slug `HarnessBlockAuthorityV1` already carries on the wire
/// (`#[serde(rename_all = "kebab-case")]`) -- spelled out here rather than
/// routed through a JSON round trip because this printer's whole contract is
/// one grep-friendly plain-text line, not JSON.
fn block_authority_slug(authority: &HarnessBlockAuthorityV1) -> &'static str {
    match authority {
        HarnessBlockAuthorityV1::HarnessGate => "harness-gate",
        HarnessBlockAuthorityV1::HarnessPolicy => "harness-policy",
        HarnessBlockAuthorityV1::HarnessDeadline => "harness-deadline",
        HarnessBlockAuthorityV1::Operator => "operator",
        HarnessBlockAuthorityV1::ProviderClassifier => "provider-classifier",
        HarnessBlockAuthorityV1::ProviderPermissionRule => "provider-permission-rule",
        HarnessBlockAuthorityV1::ProviderSandbox => "provider-sandbox",
        HarnessBlockAuthorityV1::ProviderRefusal => "provider-refusal",
        HarnessBlockAuthorityV1::ProviderHook => "provider-hook",
        HarnessBlockAuthorityV1::UserRejected => "user-rejected",
        HarnessBlockAuthorityV1::ProviderQuota => "provider-quota",
        HarnessBlockAuthorityV1::Unknown => "unknown",
    }
}

/// One line per `SubscribeAgentStream` frame, printed as it arrives -- see
/// the module-level usage text for the subcommand this backs
/// (`session subscribe`). `ts` is `now_unix_ms()`'s wall-clock reading at
/// print time for every kind except `AgentChunk`, which instead prints its
/// own `published_at_ms` off the wire -- see `format_agent_stream_event`'s
/// own doc comment for why: a replayed chunk must print the moment it was
/// originally published, not the moment `harnessctl` happened to receive it.
fn print_agent_stream_event(event: &HarnessOperatorAgentEventV1, verbose: bool) {
    if let Some(line) = format_agent_stream_event(event, now_unix_ms(), verbose) {
        println!("{line}");
    }
}

/// The formatting half of `print_agent_stream_event`, split out so the exact
/// line text is unit-testable without capturing process stdout -- `ts` is
/// passed in rather than read from the wall clock so a test gets a
/// deterministic line, and is used for every kind EXCEPT `AgentChunk`: that
/// one instead prints its own `published_at_ms` field, so a chunk replayed
/// long after it was first published (`ReplayBoundary`'s own doc comment)
/// prints the moment it actually happened, identically whether it arrived
/// live or was replayed -- there is no separate "replay" print path.
/// `None` means nothing prints for this event (a `Ping` outside `verbose`),
/// matching the caller's previous `if verbose` guard exactly.
fn format_agent_stream_event(
    event: &HarnessOperatorAgentEventV1,
    ts: u64,
    verbose: bool,
) -> Option<String> {
    match event {
        HarnessOperatorAgentEventV1::AgentChunk { sequence, session, chunk, published_at_ms } => {
            let session = format_session_address(session);
            let ts = published_at_ms;
            Some(match &chunk.kind {
                HarnessAgentStreamChunkKindV1::Text { text, is_delta } => {
                    format!(
                        "ts={ts} seq={sequence} session={session} kind=text is_delta={is_delta} text={}",
                        sanitize_line(text),
                    )
                }
                HarnessAgentStreamChunkKindV1::Thinking { text } => {
                    format!(
                        "ts={ts} seq={sequence} session={session} kind=thinking text={}",
                        sanitize_line(text),
                    )
                }
                HarnessAgentStreamChunkKindV1::InteractionPrompt {
                    correlation_id,
                    interaction_kind,
                    tool_name,
                    title,
                    prompt,
                    options,
                } => {
                    let title = title.as_deref().map(sanitize_line).unwrap_or_default();
                    let options = options
                        .iter()
                        .map(|option| format!("{}:{}:{}", option.option_id, option.name, option.kind))
                        .collect::<Vec<_>>()
                        .join(",");
                    format!(
                        "ts={ts} seq={sequence} session={session} kind=interaction-prompt \
                         correlation_id={correlation_id} interaction_kind={interaction_kind:?} \
                         tool={tool_name} title={title} prompt={} options=[{options}]",
                        sanitize_line(prompt),
                    )
                }
                HarnessAgentStreamChunkKindV1::ModeCatalog { current, available } => {
                    format!(
                        "ts={ts} seq={sequence} session={session} kind=mode-catalog current={} available=[{}]",
                        current.as_deref().unwrap_or("-"),
                        render_named_id_catalog(available),
                    )
                }
                HarnessAgentStreamChunkKindV1::ModelCatalog { current, available } => {
                    format!(
                        "ts={ts} seq={sequence} session={session} kind=model-catalog current={} available=[{}]",
                        current.as_deref().unwrap_or("-"),
                        render_named_id_catalog(available),
                    )
                }
                HarnessAgentStreamChunkKindV1::ConfigOptions { options } => {
                    let rendered = options
                        .iter()
                        .map(|option| format!("{}:{}={}", option.id, option.name, sanitize_line(&option.value_json)))
                        .collect::<Vec<_>>()
                        .join(",");
                    format!(
                        "ts={ts} seq={sequence} session={session} kind=config-options options=[{rendered}]"
                    )
                }
                HarnessAgentStreamChunkKindV1::Blocked {
                    correlation_id,
                    tool_class,
                    authority,
                    reason_kind,
                    reason,
                    help,
                } => {
                    format!(
                        "ts={ts} seq={sequence} session={session} kind=blocked \
                         authority={} tool={tool_class} reason={} reason_kind={} help={} \
                         correlation_id={}",
                        block_authority_slug(authority),
                        sanitize_line(reason),
                        reason_kind.as_deref().unwrap_or("-"),
                        help.as_deref().map(sanitize_line).unwrap_or_else(|| "-".to_owned()),
                        correlation_id.as_deref().unwrap_or("-"),
                    )
                }
            })
        }
        HarnessOperatorAgentEventV1::Lagged { sequence, session, dropped } => {
            let session = format_session_address(session);
            Some(format!("ts={ts} seq={sequence} session={session} LAGGED dropped={dropped}"))
        }
        // No `ts=`/`seq=`: `ReplayBoundary` carries neither (see that
        // variant's own doc comment in `gate4agent-harness-api`) -- it is a
        // marker between two bursts of chunks that already carry their own
        // timestamps, not an instant of its own.
        HarnessOperatorAgentEventV1::ReplayBoundary { session, replayed, dropped_before_replay } => {
            let session = format_session_address(session);
            Some(format!(
                "session={session} kind=replay-boundary replayed={replayed} \
                 dropped_before_replay={dropped_before_replay}",
            ))
        }
        HarnessOperatorAgentEventV1::Ping { sequence } => {
            verbose.then(|| format!("ts={ts} seq={sequence} kind=ping"))
        }
    }
}

fn main() {
    let outcome = match parse_args() {
        Ok(outcome) => outcome,
        Err(message) => {
            eprintln!("{message}");
            std::process::exit(2);
        }
    };
    let ParseOutcome::Run(invocation) = outcome else {
        println!("{}", usage());
        return;
    };
    match execute(invocation) {
        Ok(json) => println!("{json}"),
        Err(error) => {
            eprintln!("gate4agent-harnessctl: {error}");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gate4agent_harness_api::HarnessAgentStreamChunkV1;
    use gate4agent_harness_client::{
        HarnessContextSourceAvailabilityV1, HarnessContextSourceSelectionV1,
        HarnessDeliveryBundleDigestV1, HarnessDeliveryBundleRevisionV1, HarnessDeliveryBundleV1,
        HarnessDeliveryComponentCountV1, HarnessDeliveryComponentKindV1, HarnessDeliveryManifestDigestV2,
        HarnessExecutionModeV1, HarnessLaunchPlanRefV1, HarnessRequestDigest, HarnessRevision,
    };
    use std::collections::BTreeMap as StdBTreeMap;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    fn parse(args: &[&str], secrets: &[(&str, &str)]) -> Result<ParseOutcome, String> {
        let args = self::args(args);
        let secrets = secrets
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect::<StdBTreeMap<_, _>>();
        parse_args_from(&args, |name| {
            secrets.get(name).cloned().ok_or_else(|| format!("missing {name}"))
        })
    }

    fn token() -> String {
        format!("g4aho_{}", "0".repeat(64))
    }

    fn sample_session_address() -> HarnessRuntimeSessionAddressV1 {
        HarnessRuntimeSessionAddressV1 {
            node_id: "node-a".to_owned(),
            incarnation_id: "1".repeat(32),
            workspace_id: "workspace-a".to_owned(),
            instance_id: 7,
            generation: 3,
        }
    }

    #[test]
    fn blocked_chunk_prints_one_greppable_line_with_every_field() {
        let event = HarnessOperatorAgentEventV1::AgentChunk {
            sequence: 42,
            session: sample_session_address(),
            chunk: HarnessAgentStreamChunkV1 {
                source_sequence: 7,
                kind: HarnessAgentStreamChunkKindV1::Blocked {
                    correlation_id: Some("corr-1".to_owned()),
                    tool_class: "bash".to_owned(),
                    authority: HarnessBlockAuthorityV1::ProviderClassifier,
                    reason_kind: Some("permission-rule".to_owned()),
                    reason: "Blocked by classifier".to_owned(),
                    help: Some("add a Bash permission rule".to_owned()),
                },
            },
            published_at_ms: 1_000,
        };
        let line = format_agent_stream_event(&event, 1_000, false).unwrap();
        let session = format_session_address(&sample_session_address());
        assert_eq!(
            line,
            format!(
                "ts=1000 seq=42 session={session} kind=blocked authority=provider-classifier \
                 tool=bash reason=Blocked by classifier reason_kind=permission-rule \
                 help=add a Bash permission rule correlation_id=corr-1",
            ),
        );
    }

    #[test]
    fn blocked_chunk_renders_absent_optionals_as_a_dash() {
        let event = HarnessOperatorAgentEventV1::AgentChunk {
            sequence: 1,
            session: sample_session_address(),
            chunk: HarnessAgentStreamChunkV1 {
                source_sequence: 1,
                kind: HarnessAgentStreamChunkKindV1::Blocked {
                    correlation_id: None,
                    tool_class: "write".to_owned(),
                    authority: HarnessBlockAuthorityV1::HarnessGate,
                    reason_kind: None,
                    reason: "rule=deny-write".to_owned(),
                    help: None,
                },
            },
            published_at_ms: 1_000,
        };
        let line = format_agent_stream_event(&event, 1_000, false).unwrap();
        let session = format_session_address(&sample_session_address());
        assert_eq!(
            line,
            format!(
                "ts=1000 seq=1 session={session} kind=blocked authority=harness-gate \
                 tool=write reason=rule=deny-write reason_kind=- help=- correlation_id=-",
            ),
        );
    }

    /// A replayed chunk prints exactly like a live one -- through the same
    /// match arm, the same fields -- except its `ts` is the wire's own
    /// `published_at_ms`, never the print-time `ts` `format_agent_stream_event`
    /// was called with (`5_000_000` here, deliberately far from
    /// `published_at_ms`, so the two could never be confused for one
    /// another by coincidence).
    #[test]
    fn agent_chunk_prints_its_own_published_at_ms_not_the_print_time_ts() {
        let event = HarnessOperatorAgentEventV1::AgentChunk {
            sequence: 9,
            session: sample_session_address(),
            chunk: HarnessAgentStreamChunkV1 {
                source_sequence: 9,
                kind: HarnessAgentStreamChunkKindV1::Text { text: "hello".to_owned(), is_delta: false },
            },
            published_at_ms: 42,
        };
        let line = format_agent_stream_event(&event, 5_000_000, false).unwrap();
        let session = format_session_address(&sample_session_address());
        assert_eq!(line, format!("ts=42 seq=9 session={session} kind=text is_delta=false text=hello"));
    }

    /// `kind=replay-boundary` names its own two fields, session-scoped like
    /// every other line -- and, unlike every other line, carries no `ts=`/
    /// `seq=` at all: `ReplayBoundary` is a marker between two bursts of
    /// chunks that already carry their own timestamps, not an instant of
    /// its own (see that variant's own doc comment in
    /// `gate4agent-harness-api`).
    #[test]
    fn replay_boundary_prints_replayed_and_dropped_before_replay_with_no_ts_or_seq() {
        let event = HarnessOperatorAgentEventV1::ReplayBoundary {
            session: sample_session_address(),
            replayed: 256,
            dropped_before_replay: 44,
        };
        let line = format_agent_stream_event(&event, 1_000, false).unwrap();
        let session = format_session_address(&sample_session_address());
        assert_eq!(
            line,
            format!("session={session} kind=replay-boundary replayed=256 dropped_before_replay=44"),
        );
    }

    #[test]
    fn help_short_circuits_before_any_flag_or_secret_validation() {
        let mut requested_secrets = Vec::new();
        let outcome = parse_args_from(&args(&["gate4agent-harnessctl", "--help"]), |name| {
            requested_secrets.push(name.to_owned());
            Err("unexpected secret read".to_owned())
        })
        .unwrap();
        assert!(matches!(outcome, ParseOutcome::Help));
        assert!(requested_secrets.is_empty());
    }

    #[test]
    fn unknown_command_is_a_usage_error() {
        let error = parse(&["gate4agent-harnessctl", "bogus"], &[]).unwrap_err();
        assert_eq!(error, usage());
    }

    #[test]
    fn endpoint_is_required_and_loopback_only_before_secret_access() {
        let mut requested_secrets = Vec::new();
        let error = parse_args_from(
            &args(&["gate4agent-harnessctl", "runtime-inventory"]),
            |name| {
                requested_secrets.push(name.to_owned());
                Err("unexpected secret read".to_owned())
            },
        )
        .unwrap_err();
        assert_eq!(error, "--harness-operator is required");
        assert!(requested_secrets.is_empty());

        let error = parse(
            &["gate4agent-harnessctl", "runtime-inventory", "--harness-operator", "192.0.2.1:18080"],
            &[],
        )
        .unwrap_err();
        assert_eq!(error, "--harness-operator must be a concrete loopback socket address");
    }

    #[test]
    fn tasks_list_parses_state_after_and_limit() {
        let outcome = parse(
            &[
                "gate4agent-harnessctl",
                "tasks",
                "list",
                "--state",
                "ready",
                "--after",
                &format!("htask_{}", "a".repeat(24)),
                "--limit",
                "5",
                "--harness-operator",
                "127.0.0.1:18080",
            ],
            &[(HARNESS_OPERATOR_TOKEN_ENV, &token())],
        )
        .unwrap();
        let ParseOutcome::Run(invocation) = outcome else { panic!("expected run") };
        let Command::TasksList { state, after, limit } = invocation.command else {
            panic!("expected tasks list")
        };
        assert_eq!(state, Some(HarnessTaskStateV1::Ready));
        assert_eq!(after, Some(HarnessTaskId::new(format!("htask_{}", "a".repeat(24))).unwrap()));
        assert_eq!(limit, 5);
    }

    #[test]
    fn task_create_requires_title_and_body_and_rejects_unknown_flags() {
        let error = parse(
            &[
                "gate4agent-harnessctl",
                "task",
                "create",
                "--body",
                "b",
                "--harness-operator",
                "127.0.0.1:18080",
            ],
            &[(HARNESS_OPERATOR_TOKEN_ENV, &token())],
        )
        .unwrap_err();
        assert_eq!(error, "--title is required");

        let error = parse(
            &[
                "gate4agent-harnessctl",
                "task",
                "create",
                "--title",
                "t",
                "--body",
                "b",
                "--bogus",
                "x",
                "--harness-operator",
                "127.0.0.1:18080",
            ],
            &[(HARNESS_OPERATOR_TOKEN_ENV, &token())],
        )
        .unwrap_err();
        assert_eq!(error, "unknown flag --bogus");

        let outcome = parse(
            &[
                "gate4agent-harnessctl",
                "task",
                "create",
                "--title",
                "t",
                "--body",
                "b",
                "--harness-operator",
                "127.0.0.1:18080",
            ],
            &[(HARNESS_OPERATOR_TOKEN_ENV, &token())],
        )
        .unwrap();
        let ParseOutcome::Run(invocation) = outcome else { panic!("expected run") };
        assert!(matches!(
            invocation.command,
            Command::TaskCreate { title, body, parent: None, dependencies }
                if title == "t" && body == "b" && dependencies.is_empty()
        ));
    }

    /// `--depends` is the only way an operator can express a task dependency:
    /// until it existed the CLI hardcoded an empty list, which is why no live
    /// task had ever carried one. The wire requires the ids strictly sorted
    /// and duplicate-free, so the flag canonicalises rather than refusing.
    #[test]
    fn task_create_depends_is_canonicalised_before_it_reaches_the_wire() {
        let b = format!("htask_{}", "b".repeat(24));
        let a = format!("htask_{}", "a".repeat(24));
        let outcome = parse(
            &[
                "gate4agent-harnessctl", "task", "create",
                "--title", "t", "--body", "b",
                "--depends", &format!("{b},{a},{b}"),
                "--harness-operator", "127.0.0.1:18080",
            ],
            &[(HARNESS_OPERATOR_TOKEN_ENV, &token())],
        )
        .unwrap();
        let ParseOutcome::Run(invocation) = outcome else { panic!("expected run") };
        let Command::TaskCreate { dependencies, .. } = invocation.command else {
            panic!("expected task create")
        };
        assert_eq!(
            dependencies.iter().map(|id| id.as_str().to_owned()).collect::<Vec<_>>(),
            vec![a, b],
        );
    }

    #[test]
    fn task_get_and_run_get_require_exactly_one_positional() {
        let error = parse(
            &["gate4agent-harnessctl", "task", "get", "--harness-operator", "127.0.0.1:18080"],
            &[(HARNESS_OPERATOR_TOKEN_ENV, &token())],
        )
        .unwrap_err();
        assert_eq!(error, "expected exactly one <task-id> argument");

        let task_id = format!("htask_{}", "b".repeat(24));
        let outcome = parse(
            &["gate4agent-harnessctl", "task", "get", &task_id, "--harness-operator", "127.0.0.1:18080"],
            &[(HARNESS_OPERATOR_TOKEN_ENV, &token())],
        )
        .unwrap();
        let ParseOutcome::Run(invocation) = outcome else { panic!("expected run") };
        assert!(matches!(
            invocation.command,
            Command::TaskGet { task_id: actual } if actual.as_str() == task_id
        ));
    }

    #[test]
    fn task_move_requires_to_and_parses_kebab_target_state() {
        let task_id = format!("htask_{}", "1".repeat(24));
        let error = parse(
            &["gate4agent-harnessctl", "task", "move", &task_id, "--harness-operator", "127.0.0.1:18080"],
            &[(HARNESS_OPERATOR_TOKEN_ENV, &token())],
        )
        .unwrap_err();
        assert_eq!(error, "--to is required");

        let outcome = parse(
            &[
                "gate4agent-harnessctl",
                "task",
                "move",
                &task_id,
                "--to",
                "ready",
                "--harness-operator",
                "127.0.0.1:18080",
            ],
            &[(HARNESS_OPERATOR_TOKEN_ENV, &token())],
        )
        .unwrap();
        let ParseOutcome::Run(invocation) = outcome else { panic!("expected run") };
        assert!(matches!(
            invocation.command,
            Command::TaskMove { task_id: actual, to: HarnessTaskStateV1::Ready }
                if actual.as_str() == task_id
        ));

        let error = parse(
            &[
                "gate4agent-harnessctl",
                "task",
                "move",
                &task_id,
                "--to",
                "bogus-state",
                "--harness-operator",
                "127.0.0.1:18080",
            ],
            &[(HARNESS_OPERATOR_TOKEN_ENV, &token())],
        )
        .unwrap_err();
        assert_eq!(error, "--to has an invalid value: bogus-state");
    }

    #[test]
    fn task_operations_defaults_limit_and_parses_explicit_limit() {
        let task_id = format!("htask_{}", "2".repeat(24));
        let outcome = parse(
            &["gate4agent-harnessctl", "task", "operations", &task_id, "--harness-operator", "127.0.0.1:18080"],
            &[(HARNESS_OPERATOR_TOKEN_ENV, &token())],
        )
        .unwrap();
        let ParseOutcome::Run(invocation) = outcome else { panic!("expected run") };
        assert!(matches!(
            invocation.command,
            Command::TaskOperations { task_id: actual, limit: HARNESS_ENTITY_PAGE_LIMIT_MAX }
                if actual.as_str() == task_id
        ));

        let outcome = parse(
            &[
                "gate4agent-harnessctl",
                "task",
                "operations",
                &task_id,
                "--limit",
                "5",
                "--harness-operator",
                "127.0.0.1:18080",
            ],
            &[(HARNESS_OPERATOR_TOKEN_ENV, &token())],
        )
        .unwrap();
        let ParseOutcome::Run(invocation) = outcome else { panic!("expected run") };
        assert!(matches!(invocation.command, Command::TaskOperations { limit: 5, .. }));
    }

    #[test]
    fn spec_save_defaults_worktree_to_existing_and_review_to_operator_review() {
        let task_id = format!("htask_{}", "c".repeat(24));
        let outcome = parse(
            &[
                "gate4agent-harnessctl",
                "spec",
                "save",
                &task_id,
                "--plan",
                "ordinary-codex",
                "--harness-operator",
                "127.0.0.1:18080",
            ],
            &[(HARNESS_OPERATOR_TOKEN_ENV, &token())],
        )
        .unwrap();
        let ParseOutcome::Run(invocation) = outcome else { panic!("expected run") };
        let Command::SpecSave { task_id: actual, plan, context_source_run, delivery, review } =
            invocation.command
        else {
            panic!("expected spec save")
        };
        assert_eq!(actual.as_str(), task_id);
        assert_eq!(plan, "ordinary-codex");
        assert!(context_source_run.is_none());
        assert!(delivery.is_none());
        assert_eq!(review, HarnessTaskReviewPolicyV1::OperatorReview);
    }

    #[test]
    fn launch_options_parses_provider_workspace_plan_and_after_filters() {
        let task_id = format!("htask_{}", "f".repeat(24));
        let outcome = parse(
            &[
                "gate4agent-harnessctl",
                "launch-options",
                &task_id,
                "--provider",
                "codex",
                "--workspace",
                "workspace-a",
                "--plan",
                "ordinary-codex",
                "--after",
                "ordinary-a",
                "--harness-operator",
                "127.0.0.1:18080",
            ],
            &[(HARNESS_OPERATOR_TOKEN_ENV, &token())],
        )
        .unwrap();
        let ParseOutcome::Run(invocation) = outcome else { panic!("expected run") };
        let Command::LaunchOptions { task_id: actual, provider, workspace, plan_id, after } =
            invocation.command
        else {
            panic!("expected launch-options")
        };
        assert_eq!(actual.as_str(), task_id);
        assert_eq!(provider.unwrap().as_str(), "codex");
        assert_eq!(workspace.unwrap().as_str(), "workspace-a");
        assert_eq!(plan_id.unwrap().as_str(), "ordinary-codex");
        assert_eq!(after.unwrap().as_str(), "ordinary-a");
    }

    #[test]
    fn launch_options_filters_all_default_to_absent() {
        let task_id = format!("htask_{}", "g".repeat(24));
        let outcome = parse(
            &[
                "gate4agent-harnessctl",
                "launch-options",
                &task_id,
                "--harness-operator",
                "127.0.0.1:18080",
            ],
            &[(HARNESS_OPERATOR_TOKEN_ENV, &token())],
        )
        .unwrap();
        let ParseOutcome::Run(invocation) = outcome else { panic!("expected run") };
        assert!(matches!(
            invocation.command,
            Command::LaunchOptions { provider: None, workspace: None, plan_id: None, after: None, .. },
        ));
    }

    #[test]
    fn resolve_plan_context_source_and_delivery_match_exactly_against_current_options() {
        let task_id = HarnessTaskId::new(format!("htask_{}", "d".repeat(24))).unwrap();
        let run_id = HarnessRunId::new(format!("hrun_{}", "e".repeat(24))).unwrap();
        let bundle_id = HarnessDeliveryBundleIdV1::new("bundle.review-kit").unwrap();
        let options = HarnessTaskLaunchOptionsV1 {
            task_id: task_id.clone(),
            task_revision: HarnessRevision::new(1).unwrap(),
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
            context_sources: vec![HarnessContextSourceSelectionV1 {
                source_run_id: run_id.clone(),
                source_run_revision: HarnessRevision::new(7).unwrap(),
                observed_at_unix_ms: 90,
                metadata_digest: HarnessRequestDigest::new("c".repeat(64)).unwrap(),
                node_id: HarnessSelectorV1::new("node-a").unwrap(),
                node_incarnation: HarnessSelectorV1::new("07".repeat(16)).unwrap(),
                workspace_id: HarnessSelectorV1::new("workspace-a").unwrap(),
                session_record_id: HarnessSelectorV1::new("record-a").unwrap(),
                active_session: Some(gate4agent_harness_client::HarnessRuntimeIdentityV1 {
                    instance_id: 1,
                    generation: 1,
                }),
                message_count: 4,
                message_count_exact: true,
                completed_turn_count: Some(2),
                total_tokens: Some(128),
                availability: HarnessContextSourceAvailabilityV1::Live,
                context_pack: None,
            }],
            delivery_bundles: vec![HarnessDeliveryBundleSelectionV1 {
                bundle: HarnessDeliveryBundleV1 {
                    selector: HarnessSelectorV1::new("review-kit").unwrap(),
                    bundle_id: bundle_id.clone(),
                    revision: HarnessDeliveryBundleRevisionV1::new("revision-1").unwrap(),
                    digest: HarnessDeliveryBundleDigestV1::new(format!("sha256:{}", "d".repeat(64))).unwrap(),
                    manifest_digest: HarnessDeliveryManifestDigestV2::new(format!("sha256:{}", "e".repeat(64)))
                        .unwrap(),
                },
                component_counts: vec![HarnessDeliveryComponentCountV1 {
                    kind: HarnessDeliveryComponentKindV1::Skill,
                    workspace_count: 1,
                    session_count: 1,
                }],
            }],
            current_issued_spec: None,
            truncated: false,
            next_after: None,
            context_source_exclusions: Vec::new(),
        };
        options.validate().unwrap();

        assert_eq!(resolve_plan(&options, "ordinary-codex").unwrap().plan.plan_id.as_str(), "ordinary-codex");
        assert!(resolve_plan(&options, "stale-plan").is_err());

        assert_eq!(resolve_context_source(&options, &run_id).unwrap().source_run_id, run_id);
        let other_run = HarnessRunId::new(format!("hrun_{}", "f".repeat(24))).unwrap();
        assert!(resolve_context_source(&options, &other_run).is_err());

        assert_eq!(resolve_delivery(&options, &bundle_id).unwrap().bundle.bundle_id, bundle_id);
        let other_bundle = HarnessDeliveryBundleIdV1::new("bundle.other").unwrap();
        assert!(resolve_delivery(&options, &other_bundle).is_err());
    }

    #[test]
    fn random_hex24_produces_distinct_24_char_lowercase_hex_per_salt() {
        let first = random_hex24(1);
        let second = random_hex24(2);
        assert_eq!(first.len(), 24);
        assert_eq!(second.len(), 24);
        assert!(first.bytes().all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f')));
        assert_ne!(first, second);
    }

    #[test]
    fn workspace_inspect_requires_exactly_two_positionals() {
        let error = parse(
            &["gate4agent-harnessctl", "workspace", "inspect", "node-a", "--harness-operator", "127.0.0.1:18080"],
            &[(HARNESS_OPERATOR_TOKEN_ENV, &token())],
        )
        .unwrap_err();
        assert_eq!(error, "expected exactly two <node-id> <workspace-id> arguments");

        let outcome = parse(
            &[
                "gate4agent-harnessctl",
                "workspace",
                "inspect",
                "node-a",
                "workspace-a",
                "--harness-operator",
                "127.0.0.1:18080",
            ],
            &[(HARNESS_OPERATOR_TOKEN_ENV, &token())],
        )
        .unwrap();
        let ParseOutcome::Run(invocation) = outcome else { panic!("expected run") };
        assert!(matches!(
            invocation.command,
            Command::WorkspaceInspect { node_id, workspace_id }
                if node_id == "node-a" && workspace_id == "workspace-a"
        ));
    }

    #[test]
    fn fresh_authority_and_task_id_satisfy_protocol_validation() {
        let authority = fresh_authority().unwrap();
        authority.validate().unwrap();
        let task_id = fresh_task_id().unwrap();
        task_id.validate().unwrap();
    }
}
