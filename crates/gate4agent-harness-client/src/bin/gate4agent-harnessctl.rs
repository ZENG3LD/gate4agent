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
    HarnessExpectedExecutionSpecRevisionV1, HarnessIdempotencyRef, HarnessMoveTaskRequestV1,
    HarnessOperationId, HarnessOperatorAuthorityV1, HarnessOperatorClient, HarnessOperatorCredential,
    HarnessOperatorMutationOutcomeV1, HarnessOrdinaryLaunchPlanOptionV1,
    HarnessReplaceTaskExecutionSpecRequestV2, HarnessReviewedTaskLaunchSelectionV1,
    HarnessReviewedWorktreeSelectionV1, HarnessRunId, HarnessRunLifecycleV1, HarnessSelectorV1,
    HarnessStartTaskRequestV2, HarnessTaskId, HarnessTaskLaunchOptionsV1, HarnessTaskReviewPolicyV1,
    HarnessTaskStateV1, RedactedRunV1, RedactedTaskV1, HARNESS_ENTITY_PAGE_LIMIT_MAX,
    HARNESS_RUNTIME_INVENTORY_PAGE_LIMIT_MAX,
};
use gate4agent_harness_api::{
    HarnessExecutionModeV1, HarnessRuntimeSessionAddressV1, HarnessRuntimeTerminalSizeV1,
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
     \x20 task create --title TITLE --body BODY [--parent TASK_ID]\n\
     \x20 task move TASK_ID --to STATE\n\
     \x20 launch-options TASK_ID\n\
     \x20 spec save TASK_ID --plan PLAN_ID [--context-source-run RUN_ID] [--delivery BUNDLE_ID] [--review POLICY]\n\
     \x20 task start TASK_ID\n\
     \x20 observe-context RUN_ID\n\
     \x20 transfers RUN_ID\n\
     \x20 runtime-inventory [--after NODE_ID] [--limit N]\n\
     \x20 monitor RUN_ID\n\
     \x20 workspace inspect NODE_ID WORKSPACE_ID\n\
     \x20 session spawn NODE_ID WORKSPACE_ID PROVIDER [--profile ID] [--mode pty|inline] [--rows N] [--cols N]\n\
     \x20 session stop NODE_ID INCARNATION_ID WORKSPACE_ID INSTANCE_ID GENERATION [--force yes]"
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
    TaskCreate { title: String, body: String, parent: Option<HarnessTaskId> },
    TaskMove { task_id: HarnessTaskId, to: HarnessTaskStateV1 },
    LaunchOptions { task_id: HarnessTaskId },
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
    WorkspaceInspect { node_id: String, workspace_id: String },
    SessionSpawn {
        node_id: String,
        workspace_id: String,
        provider: String,
        provider_profile: String,
        mode: HarnessExecutionModeV1,
        rows: u16,
        cols: u16,
    },
    SessionStop { session: HarnessRuntimeSessionAddressV1, force: bool },
}

enum Verb {
    TasksList,
    RunsList,
    TaskGet,
    RunGet,
    Results,
    TaskCreate,
    TaskMove,
    LaunchOptions,
    SpecSave,
    TaskStart,
    ObserveContext,
    Transfers,
    RuntimeInventory,
    Monitor,
    WorkspaceInspect,
    SessionSpawn,
    SessionStop,
}

fn resolve_verb(args: &[String]) -> Result<(Verb, usize), String> {
    match args.get(1).map(String::as_str) {
        Some("tasks") if args.get(2).map(String::as_str) == Some("list") => Ok((Verb::TasksList, 2)),
        Some("runs") if args.get(2).map(String::as_str) == Some("list") => Ok((Verb::RunsList, 2)),
        Some("task") => match args.get(2).map(String::as_str) {
            Some("get") => Ok((Verb::TaskGet, 2)),
            Some("create") => Ok((Verb::TaskCreate, 2)),
            Some("move") => Ok((Verb::TaskMove, 2)),
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
        Some("session") => match args.get(2).map(String::as_str) {
            Some("spawn") => Ok((Verb::SessionSpawn, 2)),
            Some("stop") => Ok((Verb::SessionStop, 2)),
            _ => Err(usage().to_owned()),
        },
        Some("workspace") if args.get(2).map(String::as_str) == Some("inspect") => {
            Ok((Verb::WorkspaceInspect, 2))
        }
        _ => Err(usage().to_owned()),
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

fn parse_task_id(value: String) -> Result<HarnessTaskId, String> {
    HarnessTaskId::new(value).map_err(|_| "invalid task id".to_owned())
}

fn parse_run_id(value: String) -> Result<HarnessRunId, String> {
    HarnessRunId::new(value).map_err(|_| "invalid run id".to_owned())
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
            Ok(Command::TaskCreate { title, body, parent })
        }
        Verb::TaskMove => {
            let task_id = expect_single_positional(positionals, "task-id").and_then(parse_task_id)?;
            let to = parse_kebab::<HarnessTaskStateV1>(&require_flag(flags, "to")?, "--to")?;
            Ok(Command::TaskMove { task_id, to })
        }
        Verb::LaunchOptions => {
            let task_id = expect_single_positional(positionals, "task-id").and_then(parse_task_id)?;
            Ok(Command::LaunchOptions { task_id })
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
                Some(other) => return Err(format!("--mode must be pty or inline, got {other}")),
            };
            let rows = match take_flag(flags, "rows") {
                Some(value) => value.parse().map_err(|_| "--rows must be a u16".to_owned())?,
                None => 40,
            };
            let cols = match take_flag(flags, "cols") {
                Some(value) => value.parse().map_err(|_| "--cols must be a u16".to_owned())?,
                None => 140,
            };
            Ok(Command::SessionSpawn {
                node_id,
                workspace_id,
                provider,
                provider_profile,
                mode,
                rows,
                cols,
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
        Command::TaskCreate { title, body, parent } => {
            let task_id = fresh_task_id()?;
            let authority = fresh_authority()?;
            let outcome = client
                .create_task(HarnessCreateTaskRequestV1 {
                    authority,
                    task_id: task_id.clone(),
                    title,
                    body,
                    parent_task_id: parent,
                    dependencies: Vec::new(),
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
        Command::LaunchOptions { task_id } => {
            let options = client.task_launch_options_get(task_id).map_err(|error| error.to_string())?;
            render(&options)
        }
        Command::SpecSave { task_id, plan, context_source_run, delivery, review } => {
            let options = client
                .task_launch_options_get(task_id.clone())
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
        } => {
            let session = client
                .spawn_session(
                    node_id,
                    workspace_id,
                    provider,
                    provider_profile,
                    mode,
                    HarnessRuntimeTerminalSizeV1 { rows, columns: cols },
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
            Command::TaskCreate { title, body, parent: None } if title == "t" && body == "b"
        ));
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
