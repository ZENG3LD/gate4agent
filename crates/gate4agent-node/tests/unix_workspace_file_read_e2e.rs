#![cfg(unix)]

use gate4agent_node::protocol::{
    ClientRole, NodeFailureCode, NodeId, NodeRequest, NodeResponse, RepositoryPath,
    WorkspaceFileContent, WorkspaceId, MAX_WORKSPACE_FILE_BYTES,
    NODE_WORKSPACE_FILE_READ_CAPABILITY,
};
use gate4agent_node::{NodeServer, NodeServerConfig, WorkspaceConfig};
use gate4agent_node_wire::{LocalNodeClient, NodeClientError};
use std::ffi::{CString, OsString};
use std::fs;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{symlink, DirBuilderExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

static NEXT_TREE: AtomicU64 = AtomicU64::new(1);

struct TestTree {
    base: PathBuf,
    root: PathBuf,
    endpoint: String,
}

impl TestTree {
    fn create() -> Self {
        let serial = NEXT_TREE.fetch_add(1, Ordering::Relaxed);
        let base = std::env::temp_dir().join(format!(
            "g4a-unix-file-read-{}-{serial}",
            std::process::id(),
        ));
        let mut builder = fs::DirBuilder::new();
        builder.mode(0o700).create(&base).unwrap();
        let root = base.join("workspace");
        let outside = base.join("outside");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::create_dir(root.join("directory")).unwrap();
        fs::write(root.join("normal.txt"), b"hello from unix\n").unwrap();
        fs::write(root.join("binary.bin"), [0xff, 0xfe, 0x00, 0x61]).unwrap();
        fs::write(
            root.join("at-limit.txt"),
            vec![b'a'; MAX_WORKSPACE_FILE_BYTES],
        )
        .unwrap();
        fs::write(
            root.join("over-limit.txt"),
            vec![b'b'; MAX_WORKSPACE_FILE_BYTES + 1],
        )
        .unwrap();
        fs::write(outside.join("secret.txt"), b"outside").unwrap();

        let non_utf8_name = OsString::from_vec(vec![b'n', b'o', b'n', b'-', 0xff]);
        fs::write(root.join(non_utf8_name), b"unix bytes path").unwrap();
        symlink(outside.join("secret.txt"), root.join("final-link")).unwrap();
        symlink(&outside, root.join("through-link")).unwrap();

        let fifo = root.join("fifo");
        let fifo = CString::new(fifo.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);

        let endpoint = base.join("node.sock").to_string_lossy().into_owned();
        Self {
            base,
            root,
            endpoint,
        }
    }
}

impl Drop for TestTree {
    fn drop(&mut self) {
        if self
            .base
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("g4a-unix-file-read-"))
        {
            let _ = fs::remove_dir_all(&self.base);
        }
    }
}

fn node_id() -> NodeId {
    NodeId::new("unix-file-read-node").unwrap()
}

fn workspace_id() -> WorkspaceId {
    WorkspaceId::new("primary").unwrap()
}

fn utf8_path(value: &str) -> RepositoryPath {
    RepositoryPath::utf8(value.to_owned()).unwrap()
}

fn assert_failure(result: Result<NodeResponse, NodeClientError>, expected: NodeFailureCode) {
    match result {
        Err(NodeClientError::Node(failure)) => assert_eq!(failure.code, expected),
        Ok(response) => panic!("request unexpectedly succeeded: {response:?}"),
        Err(error) => panic!("request failed outside node contract: {error}"),
    }
}

async fn read(
    client: &mut LocalNodeClient,
    path: RepositoryPath,
) -> gate4agent_node::protocol::WorkspaceFileContent {
    let NodeResponse::WorkspaceFileRead { file } = client
        .request(NodeRequest::ReadWorkspaceFile {
            workspace_id: workspace_id(),
            path,
        })
        .await
        .unwrap()
    else {
        panic!("workspace read returned another response");
    };
    file.content
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unix_node_reads_utf8_and_byte_paths_without_symlink_escape_or_unbounded_io() {
    let tree = TestTree::create();
    let token = "unix-workspace-file-read-token";
    let config = NodeServerConfig::new(
        &tree.endpoint,
        token,
        node_id(),
        [WorkspaceConfig::new(workspace_id(), &tree.root).unwrap()],
    )
    .unwrap();
    let server = NodeServer::new_fixture(config).unwrap();
    let shutdown = server.shutdown_handle();
    let server_task = tokio::spawn(server.run());
    let mut client = LocalNodeClient::connect(
        Path::new(&tree.endpoint),
        &node_id(),
        ClientRole::Observer,
        token,
    )
    .await
    .unwrap();
    assert!(client
        .hello()
        .compatibility
        .as_ref()
        .unwrap()
        .capabilities
        .iter()
        .any(|capability| capability.as_str() == NODE_WORKSPACE_FILE_READ_CAPABILITY));

    assert_eq!(
        read(&mut client, utf8_path("normal.txt")).await,
        WorkspaceFileContent::Utf8 {
            text: "hello from unix\n".to_owned(),
            byte_len: 16,
        },
    );
    assert_eq!(
        read(
            &mut client,
            RepositoryPath::unix_bytes(vec![b'n', b'o', b'n', b'-', 0xff]).unwrap(),
        )
        .await,
        WorkspaceFileContent::Utf8 {
            text: "unix bytes path".to_owned(),
            byte_len: 15,
        },
    );
    assert_eq!(
        read(&mut client, utf8_path("binary.bin")).await,
        WorkspaceFileContent::NonUtf8 { byte_len: 4 },
    );
    match read(&mut client, utf8_path("at-limit.txt")).await {
        WorkspaceFileContent::Utf8 { text, byte_len } => {
            assert_eq!(text.len(), MAX_WORKSPACE_FILE_BYTES);
            assert_eq!(byte_len as usize, MAX_WORKSPACE_FILE_BYTES);
        }
        content => panic!("at-limit read returned {content:?}"),
    }
    assert_eq!(
        read(&mut client, utf8_path("over-limit.txt")).await,
        WorkspaceFileContent::TooLarge {
            limit_bytes: MAX_WORKSPACE_FILE_BYTES as u32,
        },
    );

    for (path, expected) in [
        ("missing.txt", NodeFailureCode::RepositoryFileNotFound),
        ("directory", NodeFailureCode::RepositoryFileNotRegular),
        ("fifo", NodeFailureCode::RepositoryFileNotRegular),
        ("final-link", NodeFailureCode::RepositoryPathUnsafe),
        (
            "through-link/secret.txt",
            NodeFailureCode::RepositoryPathUnsafe,
        ),
    ] {
        let result = client
            .request(NodeRequest::ReadWorkspaceFile {
                workspace_id: workspace_id(),
                path: utf8_path(path),
            })
            .await;
        assert_failure(result, expected);
    }

    drop(client);
    shutdown.request_shutdown().await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), server_task)
        .await
        .expect("Unix workspace file node did not stop")
        .expect("Unix workspace file node task panicked")
        .expect("Unix workspace file node failed");
}
