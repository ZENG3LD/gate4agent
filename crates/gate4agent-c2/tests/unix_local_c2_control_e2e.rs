#![cfg(unix)]

use gate4agent_c2::{C2Config, C2NodeConfig, C2Running, C2Timings};
use gate4agent_c2::protocol::{
    NodeId, NodeTransportState, PathEncoding, PathStyle,
};
use gate4agent_c2_client::connect_local;
use std::fs;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

static NEXT_DIR: AtomicU64 = AtomicU64::new(1);

struct PrivateSocketDir(PathBuf);

impl PrivateSocketDir {
    fn new() -> Self {
        let root = std::env::temp_dir();
        for _ in 0..100 {
            let serial = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
            let path = root.join(format!("g4a-c2-{}-{serial}", std::process::id()));
            let mut builder = fs::DirBuilder::new();
            builder.mode(0o700);
            match builder.create(&path) {
                Ok(()) => {
                    assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o7777, 0o700);
                    return Self(path);
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("create private socket directory: {error}"),
            }
        }
        panic!("could not allocate private socket directory")
    }

    fn endpoint(&self, name: &str) -> String {
        self.0.join(name).to_string_lossy().into_owned()
    }
}

impl Drop for PrivateSocketDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unix_c2_daemon_exposes_authenticated_control_over_owner_only_uds() {
    let sockets = PrivateSocketDir::new();
    let node_endpoint = sockets.endpoint("node.sock");
    let control_endpoint = sockets.endpoint("c2.sock");
    let node_id = NodeId::new("unix-local").unwrap();
    let node = C2NodeConfig::new(node_id.clone(), node_endpoint.clone(), "node-token").unwrap();
    let timings = C2Timings {
        attempt_deadline: Duration::from_millis(50),
        transient_backoffs: [Duration::from_millis(10); 5],
        parked_backoff: Duration::from_millis(50),
        ..C2Timings::default()
    };
    let config = C2Config::new("127.0.0.1:0".parse().unwrap(), "c2-token", vec![node])
        .unwrap()
        .with_control_endpoint(control_endpoint.clone())
        .unwrap()
        .with_timings(timings);
    let running = C2Running::start(config).await.unwrap();

    let (control, _events) = tokio::time::timeout(
        Duration::from_secs(5),
        connect_local(&control_endpoint, "c2-token"),
    ).await.expect("C2 control UDS became available").unwrap();
    let compatibility = control.hello().compatibility.as_ref().unwrap();
    assert_eq!(compatibility.host.operating_system.as_str(), std::env::consts::OS);
    assert_eq!(compatibility.host.architecture.as_str(), std::env::consts::ARCH);
    assert_eq!(compatibility.path_semantics.style, PathStyle::Posix);
    assert_eq!(compatibility.path_semantics.encoding, PathEncoding::UnixBytes);
    let topology = control.current_topology();
    assert_eq!(topology.nodes.len(), 1);
    assert_eq!(topology.nodes[0].node_id, node_id);
    assert_eq!(topology.nodes[0].endpoint, node_endpoint);
    assert_eq!(topology.nodes[0].transport, NodeTransportState::Offline);
    assert!(Path::new(&control_endpoint).exists());
    assert_eq!(fs::metadata(&control_endpoint).unwrap().permissions().mode() & 0o7777, 0o600);

    let shutdown = running.shutdown_handle();
    shutdown.shutdown();
    tokio::time::timeout(Duration::from_secs(5), running.wait())
        .await.expect("C2 shutdown completed").unwrap();
    assert!(!Path::new(&control_endpoint).exists());
}
