#![cfg(windows)]

use gate4agent_node::protocol::{
    read_json_frame_limited_body_timeout, write_json_frame_limited, ClientAuthentication,
    ClientFrame, ClientHello, ClientRole, NodeFailureCode, NodeId, NodeRequest, NodeResponse,
    RepositoryPath, RequestEnvelope, ServerFrame, WorkspaceFileContent, WorkspaceId,
    MAX_NODE_CLIENT_FRAME_BYTES, MAX_NODE_FRAME_BYTES, MAX_NODE_HELLO_FRAME_BYTES,
    MAX_WORKSPACE_FILE_BYTES, NODE_WORKSPACE_FILE_READ_CAPABILITY,
};
use gate4agent_node::{NodeServer, NodeServerConfig, WorkspaceConfig};
use gate4agent_node_wire::{
    auth_proof, random_nonce, AuthDirection, NamedPipeNodeClient, NodeClientError,
};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeClient};
use tokio::time::{sleep, timeout, Duration};

fn unique_suffix() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{}-{time}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed))
}

fn endpoint() -> String {
    format!(r"\\.\pipe\gate4agent-node-file-read-e2e-{}", unique_suffix())
}

fn node_id() -> NodeId {
    NodeId::new("file-read-fixture-node").unwrap()
}

fn workspace_id() -> WorkspaceId {
    WorkspaceId::new("primary").unwrap()
}

fn repository_path(value: &str) -> RepositoryPath {
    RepositoryPath::utf8(value.to_owned()).unwrap()
}

struct TestTree {
    root: PathBuf,
    final_junction: PathBuf,
    intermediate_junction: PathBuf,
}

impl TestTree {
    fn create() -> Self {
        let base = std::env::temp_dir().join(format!(
            "gate4agent-node-file-read-e2e-{}",
            unique_suffix(),
        ));
        let root = base.join("workspace");
        let outside = base.join("outside");
        std::fs::create_dir_all(root.join("directory")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(root.join("normal.txt"), b"hello from workspace\n").unwrap();
        std::fs::write(root.join("binary.bin"), [0xff, 0xfe, 0x00, 0x61]).unwrap();
        std::fs::write(
            root.join("at-limit.txt"),
            vec![b'a'; MAX_WORKSPACE_FILE_BYTES],
        )
        .unwrap();
        std::fs::write(
            root.join("over-limit.txt"),
            vec![b'b'; MAX_WORKSPACE_FILE_BYTES + 1],
        )
        .unwrap();
        std::fs::write(outside.join("secret.txt"), b"must not cross junction").unwrap();

        let final_junction = root.join("final-junction");
        let intermediate_junction = root.join("through-junction");
        create_junction(&final_junction, &outside);
        create_junction(&intermediate_junction, &outside);

        Self {
            root,
            final_junction,
            intermediate_junction,
        }
    }
}

impl Drop for TestTree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir(&self.final_junction);
        let _ = std::fs::remove_dir(&self.intermediate_junction);
        if self
            .root
            .parent()
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("gate4agent-node-file-read-e2e-"))
        {
            let _ = std::fs::remove_dir_all(self.root.parent().unwrap());
        }
    }
}

fn create_junction(link: &Path, target: &Path) {
    let output = Command::new("cmd.exe")
        .args(["/d", "/c", "mklink", "/J"])
        .arg(link)
        .arg(target)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "junction creation failed: {}",
        String::from_utf8_lossy(&output.stderr),
    );
}

fn server_config(endpoint: &str, token: &str, root: &Path) -> NodeServerConfig {
    NodeServerConfig::new(
        endpoint,
        token,
        node_id(),
        [WorkspaceConfig::new(workspace_id(), root).unwrap()],
    )
    .unwrap()
}

async fn raw_pipe_client(endpoint: &str) -> NamedPipeClient {
    for _ in 0..100 {
        match ClientOptions::new().open(endpoint) {
            Ok(client) => return client,
            Err(error)
                if matches!(error.kind(), std::io::ErrorKind::NotFound)
                    || error.raw_os_error() == Some(231) =>
            {
                sleep(Duration::from_millis(20)).await;
            }
            Err(error) => panic!("raw named-pipe connect failed: {error}"),
        }
    }
    panic!("raw named-pipe endpoint was not available");
}

fn assert_node_failure(result: Result<NodeResponse, NodeClientError>, expected: NodeFailureCode) {
    match result {
        Err(NodeClientError::Node(failure)) => {
            assert_eq!(failure.code, expected);
            assert_eq!(failure.message, failure_message(expected));
        }
        Ok(response) => panic!("request unexpectedly succeeded: {response:?}"),
        Err(error) => panic!("request failed outside the node contract: {error}"),
    }
}

fn failure_message(code: NodeFailureCode) -> &'static str {
    match code {
        NodeFailureCode::RepositoryFileNotFound => "repository-file-not-found",
        NodeFailureCode::RepositoryFileNotRegular => "repository-file-not-regular",
        NodeFailureCode::RepositoryPathUnsafe => "repository-path-unsafe",
        _ => panic!("unexpected test failure code: {code:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn windows_observer_named_pipe_reads_bounded_workspace_files_without_path_escape() {
    let tree = TestTree::create();
    let endpoint = endpoint();
    let token = "workspace-file-read-token";
    let server = NodeServer::new_fixture(server_config(&endpoint, token, &tree.root)).unwrap();
    let shutdown = server.shutdown_handle();
    let server_task = tokio::spawn(server.run());
    let mut observer = NamedPipeNodeClient::connect(
        &endpoint,
        &node_id(),
        ClientRole::Observer,
        token,
    )
    .await
    .unwrap();
    assert!(observer
        .hello()
        .compatibility
        .as_ref()
        .unwrap()
        .capabilities
        .iter()
        .any(|capability| capability.as_str() == NODE_WORKSPACE_FILE_READ_CAPABILITY));

    let NodeResponse::WorkspaceFileRead { file } = observer
        .request(NodeRequest::ReadWorkspaceFile {
            workspace_id: workspace_id(),
            path: repository_path("normal.txt"),
        })
        .await
        .unwrap()
    else {
        panic!("workspace file read returned another response");
    };
    assert_eq!(file.workspace_id, workspace_id());
    assert_eq!(file.path, repository_path("normal.txt"));
    assert_eq!(
        file.content,
        WorkspaceFileContent::Utf8 {
            text: "hello from workspace\n".to_owned(),
            byte_len: 21,
        },
    );

    let NodeResponse::WorkspaceFileRead { file } = observer
        .request(NodeRequest::ReadWorkspaceFile {
            workspace_id: workspace_id(),
            path: repository_path("binary.bin"),
        })
        .await
        .unwrap()
    else {
        panic!("binary workspace file read returned another response");
    };
    assert_eq!(
        file.content,
        WorkspaceFileContent::NonUtf8 { byte_len: 4 },
    );

    let NodeResponse::WorkspaceFileRead { file } = observer
        .request(NodeRequest::ReadWorkspaceFile {
            workspace_id: workspace_id(),
            path: repository_path("at-limit.txt"),
        })
        .await
        .unwrap()
    else {
        panic!("at-limit workspace file read returned another response");
    };
    match file.content {
        WorkspaceFileContent::Utf8 { text, byte_len } => {
            assert_eq!(text.len(), MAX_WORKSPACE_FILE_BYTES);
            assert_eq!(byte_len as usize, MAX_WORKSPACE_FILE_BYTES);
        }
        content => panic!("at-limit workspace file returned {content:?}"),
    }

    let NodeResponse::WorkspaceFileRead { file } = observer
        .request(NodeRequest::ReadWorkspaceFile {
            workspace_id: workspace_id(),
            path: repository_path("over-limit.txt"),
        })
        .await
        .unwrap()
    else {
        panic!("over-limit workspace file read returned another response");
    };
    assert_eq!(
        file.content,
        WorkspaceFileContent::TooLarge {
            limit_bytes: MAX_WORKSPACE_FILE_BYTES as u32,
        },
    );

    for (path, expected) in [
        ("missing.txt", NodeFailureCode::RepositoryFileNotFound),
        ("directory", NodeFailureCode::RepositoryFileNotRegular),
        (r"directory\file.txt", NodeFailureCode::RepositoryPathUnsafe),
        ("normal.txt:hidden", NodeFailureCode::RepositoryPathUnsafe),
        (
            "final-junction",
            NodeFailureCode::RepositoryFileNotRegular,
        ),
        (
            "through-junction/secret.txt",
            NodeFailureCode::RepositoryPathUnsafe,
        ),
    ] {
        let result = observer
            .request(NodeRequest::ReadWorkspaceFile {
                workspace_id: workspace_id(),
                path: repository_path(path),
            })
            .await;
        assert_node_failure(result, expected);
    }

    assert!(matches!(
        observer.request(NodeRequest::Snapshot).await.unwrap(),
        NodeResponse::Snapshot { .. },
    ));

    drop(observer);
    shutdown.request_shutdown().await.unwrap();
    timeout(Duration::from_secs(5), server_task)
        .await
        .expect("workspace file read node did not shut down")
        .expect("workspace file read node task panicked")
        .expect("workspace file read node failed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn windows_raw_legacy_client_is_denied_file_read_before_io_and_connection_stays_usable() {
    let tree = TestTree::create();
    let endpoint = endpoint();
    let token = "legacy-workspace-file-read-token";
    let server = NodeServer::new_fixture(server_config(&endpoint, token, &tree.root)).unwrap();
    let shutdown = server.shutdown_handle();
    let server_task = tokio::spawn(server.run());
    let mut pipe = raw_pipe_client(&endpoint).await;
    let client_nonce = random_nonce().unwrap();

    write_json_frame_limited(
        &mut pipe,
        &ClientFrame::Hello(ClientHello::new(ClientRole::Observer, client_nonce)),
        MAX_NODE_HELLO_FRAME_BYTES,
    )
    .await
    .unwrap();
    let ServerFrame::Challenge(challenge) = read_json_frame_limited_body_timeout(
        &mut pipe,
        MAX_NODE_HELLO_FRAME_BYTES,
        Duration::from_secs(2),
    )
    .await
    .unwrap()
    else {
        panic!("legacy client did not receive challenge");
    };
    assert_eq!(challenge.compatibility, None);
    let client_proof = auth_proof(
        token.as_bytes(),
        AuthDirection::Client,
        ClientRole::Observer,
        &client_nonce,
        &challenge.server_nonce,
    )
    .unwrap();
    write_json_frame_limited(
        &mut pipe,
        &ClientFrame::Authenticate(ClientAuthentication { client_proof }),
        MAX_NODE_HELLO_FRAME_BYTES,
    )
    .await
    .unwrap();
    let ServerFrame::Hello(hello) = read_json_frame_limited_body_timeout(
        &mut pipe,
        MAX_NODE_FRAME_BYTES,
        Duration::from_secs(2),
    )
    .await
    .unwrap()
    else {
        panic!("legacy client did not receive node hello");
    };
    assert_eq!(hello.compatibility, None);

    write_json_frame_limited(
        &mut pipe,
        &ClientFrame::Request(RequestEnvelope {
            request_id: 1,
            request: NodeRequest::ReadWorkspaceFile {
                workspace_id: WorkspaceId::new("missing-workspace").unwrap(),
                path: repository_path(r"unsafe\path:stream"),
            },
        }),
        MAX_NODE_CLIENT_FRAME_BYTES,
    )
    .await
    .unwrap();
    let ServerFrame::Reply(reply) = read_json_frame_limited_body_timeout(
        &mut pipe,
        MAX_NODE_FRAME_BYTES,
        Duration::from_secs(2),
    )
    .await
    .unwrap()
    else {
        panic!("legacy client did not receive file read denial");
    };
    let failure = reply.result.unwrap_err();
    assert_eq!(failure.code, NodeFailureCode::UnsupportedCapability);
    assert_eq!(failure.message, "unsupported-capability");

    write_json_frame_limited(
        &mut pipe,
        &ClientFrame::Request(RequestEnvelope {
            request_id: 2,
            request: NodeRequest::Snapshot,
        }),
        MAX_NODE_CLIENT_FRAME_BYTES,
    )
    .await
    .unwrap();
    let ServerFrame::Reply(reply) = read_json_frame_limited_body_timeout(
        &mut pipe,
        MAX_NODE_FRAME_BYTES,
        Duration::from_secs(2),
    )
    .await
    .unwrap()
    else {
        panic!("legacy client did not receive snapshot reply");
    };
    assert_eq!(reply.request_id, 2);
    assert!(matches!(reply.result.unwrap(), NodeResponse::Snapshot { .. }));

    drop(pipe);
    shutdown.request_shutdown().await.unwrap();
    timeout(Duration::from_secs(5), server_task)
        .await
        .expect("legacy workspace file read node did not shut down")
        .expect("legacy workspace file read node task panicked")
        .expect("legacy workspace file read node failed");
}
