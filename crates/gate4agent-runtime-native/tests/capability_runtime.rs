use gate4agent_catalog::{builtin_registry, AgentRegistry};
use gate4agent_runtime_native::{NativeRuntime, NativeRuntimeConfig};
use gate4agent_types::{
    AgentInstanceId, CapabilityProbeRequest, CommandEnvelope, CommandId, ControlCommand,
    TransportKind, CONTROL_PROTOCOL_VERSION,
};
use std::fs;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn command(id: u64, command: ControlCommand) -> CommandEnvelope {
    CommandEnvelope {
        protocol_version: CONTROL_PROTOCOL_VERSION,
        id: CommandId(id),
        command,
    }
}

fn fixture_dir() -> std::path::PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "gate4agent-capability-runtime-{}-{nonce}",
        std::process::id()
    ));
    fs::create_dir(&path).unwrap();
    path
}

#[tokio::test]
async fn cursor_probe_crosses_kernel_native_shell_and_full_snapshot_with_host_cache() {
    let directory = fixture_dir();
    let counter = directory.join("count.txt");
    let script_path = directory.join(if cfg!(windows) {
        "probe.cmd"
    } else {
        "probe.sh"
    });
    #[cfg(windows)]
    let script = format!(
        "@echo off\r\n>>\"{}\" echo x\r\necho auto - Auto ^(default^)\r\necho account-model - Account Model\r\n",
        counter.display()
    );
    #[cfg(not(windows))]
    let script = format!(
        "printf x >> '{}'; printf 'auto - Auto (default)\\naccount-model - Account Model\\n'",
        counter.display().to_string().replace('\'', "'\\''")
    );
    fs::write(&script_path, script).unwrap();

    let mut cursor = builtin_registry().get_by_id("cursor").unwrap().clone();
    #[cfg(windows)]
    {
        cursor.launch.program = script_path.display().to_string();
        cursor.launch.fixed_args = Vec::new();
    }
    #[cfg(not(windows))]
    {
        cursor.launch.program = "sh".to_owned();
        cursor.launch.fixed_args = vec![script_path.display().to_string()];
    }
    let catalog = AgentRegistry::new([cursor]).unwrap();
    let (handle, mut runtime) = NativeRuntime::new(catalog, NativeRuntimeConfig::default());

    for (command_id, instance_id) in [(1, 41), (3, 42)] {
        handle
            .dispatch(command(
                command_id,
                ControlCommand::Register {
                    instance_id: AgentInstanceId(instance_id),
                    agent_id: gate4agent_types::AgentId::new("cursor").unwrap(),
                    transport: TransportKind::Pty,
                },
            ))
            .unwrap();
        handle
            .dispatch(command(
                command_id + 1,
                ControlCommand::ProbeCapabilities {
                    instance_id: AgentInstanceId(instance_id),
                    request: CapabilityProbeRequest {
                        working_directory: directory.display().to_string(),
                    },
                },
            ))
            .unwrap();

        for _ in 0..100 {
            runtime.tick().await;
            let settled = handle
                .snapshot()
                .sessions
                .iter()
                .find(|session| session.instance_id == AgentInstanceId(instance_id))
                .is_some_and(|session| session.capabilities.settled);
            if settled {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let snapshot = handle.snapshot();
        let session = snapshot
            .sessions
            .iter()
            .find(|session| session.instance_id == AgentInstanceId(instance_id))
            .unwrap();
        assert!(session.capabilities.settled);
        assert_eq!(
            session
                .capabilities
                .session_option_models
                .iter()
                .map(|model| model.id.as_str())
                .collect::<Vec<_>>(),
            ["auto", "account-model"]
        );
    }

    assert_eq!(fs::read_to_string(&counter).unwrap().lines().count(), 1);
    drop(runtime);
    fs::remove_dir_all(directory).unwrap();
}
