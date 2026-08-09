#![cfg(any(windows, unix))]

use gate4agent_c2::protocol::{
    C2NodeResponse, C2NodeSnapshot, C2SessionSnapshot, C2SessionStatus, NodeId, NodeRoute,
    NodeTransportState,
};
use gate4agent_c2::{C2Config, C2NodeConfig, C2Running, C2Timings};
use gate4agent_c2_client::{connect_local, C2Client, C2ControlHandle};
use gate4agent_node::protocol::{NodeRequest, SessionMode, WorkspaceId};
use gate4agent_node::{NodeServer, NodeServerConfig, WorkspaceConfig};
use gate4agent_types::{AgentId, AgentInstanceId, TerminalFrame, TerminalSize};
#[cfg(windows)]
use std::ffi::OsStr;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(windows)]
use std::time::{Duration, SystemTime, UNIX_EPOCH};
#[cfg(unix)]
use std::time::Duration;
use tokio::time::{sleep, timeout};

const LIVE_CANARY_ENV: &str = "GATE4AGENT_VENDOR_PTY_SCREEN_CANARY";
const LIVE_LAUNCHER_ENV: &str = "GATE4AGENT_VENDOR_PTY_SCREEN_LAUNCHER";
const LIVE_AGENT_ENV: &str = "GATE4AGENT_VENDOR_PTY_SCREEN_AGENT";
const LIVE_VERSION_ENV: &str = "GATE4AGENT_VENDOR_PTY_SCREEN_VERSION";

static NEXT_ENDPOINT: AtomicU64 = AtomicU64::new(1);

struct LocalTestEndpoints {
    root: PathBuf,
    node: String,
    control: String,
}

impl LocalTestEndpoints {
    fn new() -> Self {
        let root = Self::private_root();
        #[cfg(windows)]
        {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let serial = NEXT_ENDPOINT.fetch_add(1, Ordering::Relaxed);
            return Self {
                root,
                node: format!(
                    r"\\.\pipe\gate4agent-real-screen-node-{}-{nonce}-{serial}",
                    std::process::id(),
                ),
                control: format!(
                    r"\\.\pipe\gate4agent-real-screen-c2-{}-{nonce}-{serial}",
                    std::process::id(),
                ),
            };
        }
        #[cfg(unix)]
        {
            return Self {
                node: root.join("node.sock").to_string_lossy().into_owned(),
                control: root.join("c2.sock").to_string_lossy().into_owned(),
                root,
            };
        }
    }

    fn private_root() -> PathBuf {
        #[cfg(unix)]
        let temp = PathBuf::from("/tmp");
        #[cfg(windows)]
        let temp = std::env::temp_dir();
        for _ in 0..100 {
            let serial = NEXT_ENDPOINT.fetch_add(1, Ordering::Relaxed);
            let root = temp.join(format!(
                "g4a-real-screen-{}-{serial}",
                std::process::id(),
            ));
            #[cfg(unix)]
            let result = {
                use std::os::unix::fs::DirBuilderExt;

                let mut builder = fs::DirBuilder::new();
                builder.mode(0o700);
                builder.create(&root)
            };
            #[cfg(windows)]
            let result = fs::create_dir(&root);
            match result {
                Ok(()) => {
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;

                        assert_eq!(
                            fs::metadata(&root).unwrap().permissions().mode() & 0o7777,
                            0o700,
                        );
                    }
                    return root;
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("failed to create private test directory: {error}"),
            }
        }
        panic!("could not allocate private test directory")
    }

    fn private_directory(&self, name: &str) -> PathBuf {
        let directory = self.root.join(name);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;

            let mut builder = fs::DirBuilder::new();
            builder.mode(0o700);
            builder.create(&directory).unwrap();
        }
        #[cfg(windows)]
        fs::create_dir(&directory).unwrap();
        directory
    }
}

impl Drop for LocalTestEndpoints {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

struct EnvironmentRestore {
    original: Vec<(OsString, OsString)>,
}

impl EnvironmentRestore {
    fn isolate(endpoints: &LocalTestEndpoints, launcher: &Path) -> Self {
        let original = std::env::vars_os().collect::<Vec<_>>();
        #[cfg(windows)]
        let original_system_root = original_value(&original, "SystemRoot");
        #[cfg(windows)]
        let original_windir = original_value(&original, "WINDIR");
        #[cfg(windows)]
        let original_comspec = original_value(&original, "ComSpec");
        #[cfg(windows)]
        let original_pathext = original_value(&original, "PATHEXT");
        for (key, _) in &original {
            std::env::remove_var(key);
        }

        let home = endpoints.private_directory("home");
        let temporary = endpoints.private_directory("tmp");
        let xdg_config = endpoints.private_directory("xdg-config");
        let xdg_cache = endpoints.private_directory("xdg-cache");
        let xdg_data = endpoints.private_directory("xdg-data");
        set_path_environment("HOME", &home);
        set_path_environment("TMPDIR", &temporary);
        set_path_environment("TMP", &temporary);
        set_path_environment("TEMP", &temporary);
        set_path_environment("XDG_CONFIG_HOME", &xdg_config);
        set_path_environment("XDG_CACHE_HOME", &xdg_cache);
        set_path_environment("XDG_DATA_HOME", &xdg_data);
        #[cfg(unix)]
        set_path_environment("XDG_RUNTIME_DIR", &endpoints.root);
        std::env::set_var("TERM", "xterm-256color");
        std::env::set_var("LANG", "C");
        std::env::set_var("DISABLE_AUTOUPDATER", "1");

        #[cfg(unix)]
        {
            let _ = launcher;
            std::env::set_var("PATH", "/usr/bin:/bin:/usr/sbin:/sbin");
            std::env::set_var("SHELL", "/bin/sh");
        }
        #[cfg(windows)]
        {
            let system_root = original_system_root
                .or(original_windir.clone())
                .expect("Windows system root is unavailable");
            let windir = original_windir.unwrap_or_else(|| system_root.clone());
            let comspec = original_comspec.unwrap_or_else(|| {
                PathBuf::from(&system_root)
                    .join("System32")
                    .join("cmd.exe")
                    .into_os_string()
            });
            std::env::set_var("SystemRoot", &system_root);
            std::env::set_var("WINDIR", &windir);
            std::env::set_var("ComSpec", &comspec);
            std::env::set_var(
                "PATHEXT",
                original_pathext.unwrap_or_else(|| OsString::from(".COM;.EXE;.BAT;.CMD")),
            );
            let launcher_parent = launcher
                .parent()
                .expect("exact launcher has no parent directory");
            let system32 = PathBuf::from(&system_root).join("System32");
            let system_root_path = PathBuf::from(&system_root);
            let path = std::env::join_paths([
                launcher_parent,
                system32.as_path(),
                system_root_path.as_path(),
            ])
            .expect("minimal Windows PATH cannot be constructed");
            std::env::set_var("PATH", path);
            set_path_environment("USERPROFILE", &home);
            set_path_environment("APPDATA", &home.join("AppData/Roaming"));
            set_path_environment("LOCALAPPDATA", &home.join("AppData/Local"));
            fs::create_dir_all(home.join("AppData/Roaming")).unwrap();
            fs::create_dir_all(home.join("AppData/Local")).unwrap();
        }

        Self { original }
    }
}

impl Drop for EnvironmentRestore {
    fn drop(&mut self) {
        let current = std::env::vars_os().collect::<Vec<_>>();
        for (key, _) in current {
            std::env::remove_var(key);
        }
        for (key, value) in &self.original {
            std::env::set_var(key, value);
        }
    }
}

#[cfg(windows)]
fn original_value(original: &[(OsString, OsString)], key: &str) -> Option<OsString> {
    original
        .iter()
        .find(|(candidate, _)| candidate.eq_ignore_ascii_case(OsStr::new(key)))
        .map(|(_, value)| value.clone())
}

fn set_path_environment(key: &str, value: &Path) {
    std::env::set_var(key, value.as_os_str());
}

fn required_live_inputs() -> (AgentId, AgentId, String, String) {
    assert_eq!(
        std::env::var_os(LIVE_CANARY_ENV).as_deref(),
        Some(std::ffi::OsStr::new("1")),
        "live PTY canary is not explicitly enabled",
    );
    let agent_text = std::env::var(LIVE_AGENT_ENV)
        .expect("exact live agent ID is required");
    let agent_id = AgentId::new(agent_text).expect("exact live agent ID is invalid");
    let provider = agent_id.clone();
    let launcher = std::env::var(LIVE_LAUNCHER_ENV)
        .expect("exact live launcher path is required and must be Unicode");
    let launcher_path = Path::new(&launcher);
    assert!(launcher_path.is_absolute(), "exact live launcher must be absolute");
    assert!(launcher_path.is_file(), "exact live launcher must be a regular file");
    assert!(!launcher.contains('\0'), "exact live launcher contains NUL");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        assert!(
            fs::metadata(launcher_path).unwrap().permissions().mode() & 0o111 != 0,
            "exact live launcher must be executable",
        );
    }
    let version = std::env::var(LIVE_VERSION_ENV)
        .expect("exact live launcher version is required");
    assert!(
        !version.is_empty()
            && version.len() <= 128
            && version.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'+' | b'-')
            }),
        "exact live launcher version is unsafe for sanitized evidence",
    );
    (agent_id, provider, launcher, version)
}

fn node_config(
    endpoint: &str,
    token: &str,
    node_id: &NodeId,
    workspace_root: &Path,
) -> NodeServerConfig {
    NodeServerConfig::new(
        endpoint,
        token,
        node_id.clone(),
        [WorkspaceConfig::new(
            WorkspaceId::new("primary").unwrap(),
            workspace_root,
        )
        .unwrap()],
    )
    .unwrap()
}

async fn wait_online(client: &C2Client, node_id: &NodeId) -> NodeRoute {
    timeout(Duration::from_secs(30), async {
        loop {
            if let Ok(status) = client.status().await {
                if status.nodes[node_id].transport == NodeTransportState::Online {
                    let cursor = status.nodes[node_id]
                        .cursor
                        .expect("online node has a cursor");
                    return NodeRoute {
                        node_id: node_id.clone(),
                        expected_incarnation_id: cursor.incarnation_id,
                    };
                }
            }
            sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("real node did not become online through C2")
}

async fn snapshot(control: &C2ControlHandle, route: &NodeRoute) -> C2NodeSnapshot {
    let response = control
        .request(route.clone(), NodeRequest::Snapshot)
        .await
        .expect("C2 snapshot route failed");
    match response.response {
        Ok(C2NodeResponse::Snapshot { snapshot, .. }) => snapshot,
        _ => panic!("C2 snapshot route returned an unexpected response"),
    }
}

fn session<'a>(
    snapshot: &'a C2NodeSnapshot,
    instance_id: AgentInstanceId,
) -> Option<&'a C2SessionSnapshot> {
    snapshot
        .workspaces
        .iter()
        .flat_map(|workspace| &workspace.sessions)
        .find(|session| session.instance_id == instance_id)
}

fn frame_is_renderable(frame: &TerminalFrame) -> bool {
    if frame.formatted.is_empty()
        || !frame
            .contents
            .chars()
            .any(|character| !character.is_whitespace() && !character.is_control())
    {
        return false;
    }
    let mut parser = vt100::Parser::new(frame.size.rows, frame.size.columns, 0);
    parser.process(&frame.formatted);
    parser
        .screen()
        .contents()
        .chars()
        .any(|character| !character.is_whitespace() && !character.is_control())
}

fn assert_renderable(frame: &TerminalFrame) {
    assert!(frame_is_renderable(frame), "terminal frame produced an empty screen");
}

async fn wait_renderable_frame(
    control: &C2ControlHandle,
    route: &NodeRoute,
    instance_id: AgentInstanceId,
) -> TerminalFrame {
    timeout(Duration::from_secs(60), async {
        loop {
            let snapshot = snapshot(control, route).await;
            let current = session(&snapshot, instance_id)
                .expect("spawned session is missing from C2 snapshot");
            if let Some(frame) = current
                .terminal_frame
                .as_ref()
                .filter(|frame| frame_is_renderable(frame))
            {
                return frame.clone();
            }
            assert!(
                !matches!(current.status, C2SessionStatus::Exited { .. } | C2SessionStatus::Failed),
                "real provider exited before publishing a renderable terminal frame",
            );
            sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("renderable terminal frame did not reach C2")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an explicitly selected installed vendor PTY launcher"]
async fn real_node_c2_snapshot_renders_vendor_terminal_and_reaps_process() {
    let (agent_id, provider, launcher, version) = required_live_inputs();
    let agent_label = agent_id.as_str().to_owned();
    let endpoints = LocalTestEndpoints::new();
    let workspace = endpoints.private_directory("workspace");
    let _environment = EnvironmentRestore::isolate(&endpoints, Path::new(&launcher));
    let node_id = NodeId::new("real-screen-node").unwrap();
    let node_token = "real-screen-node-token";
    let c2_token = "real-screen-c2-token";

    let server = NodeServer::new_exact_launcher_fixture(
        node_config(&endpoints.node, node_token, &node_id, &workspace),
        agent_id,
        launcher,
    )
    .unwrap();
    let node_shutdown = server.shutdown_handle();
    let node_task = tokio::spawn(server.run());

    let timings = C2Timings {
        poll_interval: Duration::from_millis(25),
        fresh_for: Duration::from_secs(2),
        attempt_deadline: Duration::from_secs(2),
        transient_backoffs: [Duration::from_millis(25); 5],
        parked_backoff: Duration::from_millis(100),
        http_io_deadline: Duration::from_secs(1),
    };
    let config = C2Config::new(
        "127.0.0.1:0".parse().unwrap(),
        c2_token,
        vec![C2NodeConfig::new(
            node_id.clone(),
            endpoints.node.clone(),
            node_token,
        )
        .unwrap()],
    )
    .unwrap()
    .with_control_endpoint(endpoints.control.clone())
    .unwrap()
    .with_timings(timings);
    let running = C2Running::start(config).await.unwrap();
    let http = C2Client::new(running.api_addr(), c2_token)
        .unwrap()
        .with_deadline(Duration::from_secs(1));
    let route = wait_online(&http, &node_id).await;
    let (control, mut events) = connect_local(&endpoints.control, c2_token)
        .await
        .unwrap();
    let event_drain = tokio::spawn(async move {
        while events.recv().await.is_some() {}
    });

    let initial_size = TerminalSize {
        rows: 24,
        columns: 80,
    };
    let spawned = control
        .request(
            route.clone(),
            NodeRequest::Spawn {
                workspace_id: WorkspaceId::new("primary").unwrap(),
                provider,
                mode: SessionMode::Pty,
                terminal_size: initial_size,
                initial_prompt: None,
            },
        )
        .await
        .unwrap();
    let address = match spawned.response {
        Ok(C2NodeResponse::SpawnAccepted { session }) => session,
        _ => panic!("real provider spawn returned an unexpected response"),
    };
    let first_frame = wait_renderable_frame(&control, &route, address.session.instance_id).await;
    let first_snapshot = snapshot(&control, &route).await;
    assert!(
        session(&first_snapshot, address.session.instance_id)
            .and_then(|session| session.process_id)
            .is_some(),
        "running provider has no process identity",
    );

    let resized = TerminalSize {
        rows: 30,
        columns: 100,
    };
    assert!(matches!(
        control
            .request(
                route.clone(),
                NodeRequest::Resize {
                    session: address.clone(),
                    size: resized,
                },
            )
            .await
            .unwrap()
            .response,
        Ok(C2NodeResponse::Accepted)
    ));
    timeout(Duration::from_secs(15), async {
        loop {
            let current = snapshot(&control, &route).await;
            let frame = session(&current, address.session.instance_id)
                .and_then(|session| session.terminal_frame.as_ref());
            if let Some(frame) = frame.filter(|frame| {
                frame.size == resized
                    && frame.sequence > first_frame.sequence
                    && frame_is_renderable(frame)
            }) {
                assert_renderable(frame);
                break;
            }
            sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("resized terminal frame did not reach C2");

    assert!(matches!(
        control
            .request(
                route.clone(),
                NodeRequest::Interrupt {
                    session: address.clone(),
                },
            )
            .await
            .unwrap()
            .response,
        Ok(C2NodeResponse::Accepted)
    ));
    assert!(matches!(
        control
            .request(
                route.clone(),
                NodeRequest::Stop {
                    session: address.clone(),
                    force: true,
                },
            )
            .await
            .unwrap()
            .response,
        Ok(C2NodeResponse::Accepted)
    ));
    timeout(Duration::from_secs(30), async {
        loop {
            let current = snapshot(&control, &route).await;
            if session(&current, address.session.instance_id).is_some_and(|session| {
                matches!(session.status, C2SessionStatus::Exited { .. })
                    && session.process_id.is_none()
            }) {
                break;
            }
            sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("stopped provider process was not reaped through C2 snapshot");

    let c2_shutdown = running.shutdown_handle();
    c2_shutdown.shutdown();
    timeout(Duration::from_secs(5), running.wait())
        .await
        .expect("C2 shutdown timed out")
        .expect("C2 shutdown failed");
    drop(control);
    timeout(Duration::from_secs(2), event_drain)
        .await
        .expect("C2 event stream did not close")
        .expect("C2 event drain failed");
    node_shutdown.request_shutdown().await.unwrap();
    timeout(Duration::from_secs(10), node_task)
        .await
        .expect("node shutdown timed out")
        .expect("node task panicked")
        .expect("node shutdown failed");
    #[cfg(unix)]
    {
        assert!(!Path::new(&endpoints.node).exists());
        assert!(!Path::new(&endpoints.control).exists());
    }
    println!(
        "real_node_c2_terminal_screen agent={agent_label} version={version} screen_nonempty=true resize=true interrupt=true reaped=true",
    );
}
