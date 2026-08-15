#![cfg(windows)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use gate4agent_c2::{C2Config, C2NodeConfig, C2Running, C2Timings};
use gate4agent_c2_client::{connect_local, C2Client};
use gate4agent_c2_protocol::C2NodeResponse;
use gate4agent_harness_delivery::{
    compile_reviewed_delivery_bundle_v2, ReviewedDeliverySourceV2,
};
use gate4agent_node::protocol::{
    DeliveryBlobChunkHexV1, DeliveryComponentKindV2, DeliveryRelativePathV2,
    DeliveryScopeV2, NodeFailureCode, NodeId, NodeRequest, SpawnBundleId,
    SpawnBundleRevision, WorkspaceId,
};
use gate4agent_node::{
    protect_bundle_source_tree_fixture, NodeServer, NodeServerConfig, WorkspaceConfig,
};
use tokio::time::{sleep, timeout};

struct FixtureRoot(PathBuf);

impl FixtureRoot {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "gate4agent-h3a-c2-delivery-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos(),
        ));
        std::fs::create_dir(&root).unwrap();
        Self(root)
    }

    fn path(&self) -> &Path { &self.0 }
}

impl Drop for FixtureRoot {
    fn drop(&mut self) {
        if self.0.is_dir() {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

fn endpoint(label: &str) -> String {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    format!(
        r"\\.\pipe\gate4agent-h3a-{label}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed),
    )
}

fn encode_chunk(bytes: &[u8]) -> DeliveryBlobChunkHexV1 {
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        write!(&mut encoded, "{byte:02x}").unwrap();
    }
    DeliveryBlobChunkHexV1::new(encoded).unwrap()
}

fn assert_private_bytes_absent(path: &Path, forbidden: &[&str]) {
    if !path.exists() {
        return;
    }
    if path.is_dir() {
        for entry in std::fs::read_dir(path).unwrap() {
            assert_private_bytes_absent(&entry.unwrap().path(), forbidden);
        }
        return;
    }

    let bytes = std::fs::read(path).unwrap();
    let text = String::from_utf8_lossy(&bytes);
    for value in forbidden {
        assert!(
            !text.contains(value),
            "Node durable plane {} persisted private fixture bytes",
            path.display(),
        );
    }
}

async fn wait_online(client: &C2Client, node_id: &NodeId) {
    timeout(Duration::from_secs(10), async {
        loop {
            let status = client.status().await.unwrap();
            if status.nodes.get(node_id).is_some_and(|node| {
                node.transport == gate4agent_c2::protocol::NodeTransportState::Online
                    && node.cursor.is_some()
            }) {
                break;
            }
            sleep(Duration::from_millis(25)).await;
        }
    }).await.expect("fixture Node did not become online through C2");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn windows_c2_delivery_wire_rejects_invalid_chunks_and_abort_publishes_nothing() {
    assert_eq!(
        std::env::var_os("GATE4AGENT_HEADLESS_SUPERVISOR").as_deref(),
        Some(std::ffi::OsStr::new("1")),
        "Windows integration tests must run through windows-headless-supervisor",
    );
    let fixture = FixtureRoot::new();
    const RAW_CANARY: &str = "H3A_PRIVATE_DELIVERY_CANARY_DO_NOT_PERSIST";
    let workspace_root = fixture.path().join("workspace");
    let source_root = fixture.path().join("reviewed-source");
    std::fs::create_dir(&workspace_root).unwrap();
    std::fs::create_dir_all(source_root.join(".claude-plugin")).unwrap();
    std::fs::create_dir_all(source_root.join("skills/review-code")).unwrap();
    std::fs::write(
        source_root.join("plugin.json"),
        br#"{"$schema":"https://agent-plugins.org/schemas/1.0.0/plugin.schema.json","name":"h3a-review"}"#,
    ).unwrap();
    std::fs::write(
        source_root.join(".claude-plugin/plugin.json"),
        format!(
            r#"{{"name":"h3a-review","description":"{RAW_CANARY}","version":"1.0.0"}}"#,
        ),
    ).unwrap();
    let mut skill = format!(
        "---\nname: review-code\ndescription: Controlled H3A review fixture.\n---\n\n{RAW_CANARY}\n",
    ).into_bytes();
    skill.resize(300 * 1024 + 17, b'x');
    let skill_path = source_root.join("skills/review-code/SKILL.md");
    std::fs::write(&skill_path, &skill).unwrap();
    protect_bundle_source_tree_fixture(&source_root).unwrap();

    let delivery = compile_reviewed_delivery_bundle_v2(
        SpawnBundleId::new("bundle.h3a-review").unwrap(),
        SpawnBundleRevision::new("negative-revision").unwrap(),
        &[
            ReviewedDeliverySourceV2::new(
                source_root.join("plugin.json"),
                None,
                DeliveryComponentKindV2::PluginManifest,
                DeliveryScopeV2::Session,
            ).unwrap(),
            ReviewedDeliverySourceV2::new(
                source_root.join(".claude-plugin/plugin.json"),
                Some(DeliveryRelativePathV2::new(".claude-plugin").unwrap()),
                DeliveryComponentKindV2::PluginManifest,
                DeliveryScopeV2::Session,
            ).unwrap(),
            ReviewedDeliverySourceV2::new(
                &skill_path,
                Some(DeliveryRelativePathV2::new("skills/review-code").unwrap()),
                DeliveryComponentKindV2::Skill,
                DeliveryScopeV2::Session,
            ).unwrap(),
        ],
    ).unwrap();
    assert!(delivery.blobs().iter().any(|blob| blob.bytes().len() > 256 * 1024));
    assert_eq!(
        delivery.manifest().components.iter().map(|component| (
            component.kind,
            component.scope,
            component.relative_path.as_str(),
        )).collect::<Vec<_>>(),
        vec![
            (
                DeliveryComponentKindV2::PluginManifest,
                DeliveryScopeV2::Session,
                ".claude-plugin/plugin.json",
            ),
            (
                DeliveryComponentKindV2::PluginManifest,
                DeliveryScopeV2::Session,
                "plugin.json",
            ),
            (
                DeliveryComponentKindV2::Skill,
                DeliveryScopeV2::Session,
                "skills/review-code/SKILL.md",
            ),
        ],
    );

    let node_endpoint = endpoint("node");
    let control_endpoint = endpoint("control");
    let node_id = NodeId::new("h3a-delivery-node").unwrap();
    let workspace_id = WorkspaceId::new("primary").unwrap();
    let node_token = "h3a-delivery-node-token";
    let c2_token = "h3a-delivery-c2-token";
    let node_state_path = fixture.path().join("node-state.json");
    let delivery_store_root = fixture.path().join("node-state.json.delivery-store");
    let node_config = NodeServerConfig::new(
        &node_endpoint,
        node_token,
        node_id.clone(),
        [WorkspaceConfig::new(workspace_id, &workspace_root).unwrap()],
    ).unwrap()
        .with_state_path(&node_state_path).unwrap();
    let server = NodeServer::new_fixture(node_config).unwrap();
    let node_shutdown = server.shutdown_handle();
    let node_task = tokio::spawn(server.run());

    let c2 = C2Running::start(
        C2Config::new(
            "127.0.0.1:0".parse().unwrap(),
            c2_token,
            vec![C2NodeConfig::new(node_id.clone(), node_endpoint, node_token).unwrap()],
        ).unwrap()
        .with_control_endpoint(control_endpoint.clone()).unwrap()
        .with_timings(C2Timings {
            poll_interval: Duration::from_millis(20),
            fresh_for: Duration::from_secs(2),
            attempt_deadline: Duration::from_secs(2),
            transient_backoffs: [Duration::from_millis(20); 5],
            parked_backoff: Duration::from_millis(100),
            http_io_deadline: Duration::from_secs(1),
        }),
    ).await.unwrap();
    let http = C2Client::new(c2.api_addr(), c2_token).unwrap()
        .with_deadline(Duration::from_secs(1));
    wait_online(&http, &node_id).await;

    let (control, mut events) = connect_local(&control_endpoint, c2_token).await.unwrap();
    let event_drain = tokio::spawn(async move {
        while events.recv().await.is_some() {}
    });
    let route = gate4agent_c2_protocol::NodeRoute {
        node_id: node_id.clone(),
        expected_incarnation_id: http.status().await.unwrap().nodes[&node_id]
            .cursor.as_ref().unwrap().incarnation_id,
    };
    let begun = control.request(
        route.clone(),
        NodeRequest::BeginDeliveryStage { manifest: delivery.manifest().clone() },
    ).await.unwrap();
    let stage_id = match begun.response {
        Ok(C2NodeResponse::DeliveryStageBegun { stage_id, .. }) => stage_id,
        response => panic!("negative delivery stage did not begin: {response:?}"),
    };
    let small_blob = delivery.blobs().iter().find(|blob| {
        blob.bytes().windows(RAW_CANARY.len())
            .any(|window| window == RAW_CANARY.as_bytes())
    }).expect("compiled delivery did not retain the private canary blob");
    for offset in [1, u64::MAX] {
        let rejected = control.request(
            route.clone(),
            NodeRequest::PutDeliveryBlobChunk {
                stage_id: stage_id.clone(),
                blob_digest: small_blob.receipt().digest.clone(),
                offset,
                chunk_hex: encode_chunk(&small_blob.bytes()[..1]),
            },
        ).await.unwrap();
        assert_eq!(rejected.response.unwrap_err().code, NodeFailureCode::DeliveryChunkOutOfOrder);
    }

    let mut overflowing = small_blob.bytes().to_vec();
    overflowing.push(0);
    let rejected = control.request(
        route.clone(),
        NodeRequest::PutDeliveryBlobChunk {
            stage_id: stage_id.clone(),
            blob_digest: small_blob.receipt().digest.clone(),
            offset: 0,
            chunk_hex: encode_chunk(&overflowing),
        },
    ).await.unwrap();
    assert_eq!(
        rejected.response.unwrap_err().code,
        NodeFailureCode::DeliveryChunkOutOfOrder,
        "internal DeliveryStore ChunkOverflow maps to the frozen categorical wire code",
    );

    let mut tampered = small_blob.bytes().to_vec();
    tampered[0] ^= 1;
    let rejected = control.request(
        route.clone(),
        NodeRequest::PutDeliveryBlobChunk {
            stage_id: stage_id.clone(),
            blob_digest: small_blob.receipt().digest.clone(),
            offset: 0,
            chunk_hex: encode_chunk(&tampered),
        },
    ).await.unwrap();
    assert_eq!(rejected.response.unwrap_err().code, NodeFailureCode::DeliveryBlobDigestMismatch);

    let aborted = control.request(
        route.clone(),
        NodeRequest::AbortDeliveryStage { stage_id: stage_id.clone() },
    ).await.unwrap();
    assert!(matches!(
        aborted.response,
        Ok(C2NodeResponse::DeliveryStageAborted { stage_id: actual }) if actual == stage_id
    ));

    let replayed = control.request(
        route.clone(),
        NodeRequest::BeginDeliveryStage { manifest: delivery.manifest().clone() },
    ).await.unwrap();
    let replay_stage_id = match replayed.response {
        Ok(C2NodeResponse::DeliveryStageBegun { stage_id, .. }) => stage_id,
        response => panic!("aborted delivery authority was not reusable: {response:?}"),
    };
    let replay_aborted = control.request(
        route.clone(),
        NodeRequest::AbortDeliveryStage { stage_id: replay_stage_id.clone() },
    ).await.unwrap();
    assert!(matches!(
        replay_aborted.response,
        Ok(C2NodeResponse::DeliveryStageAborted { stage_id }) if stage_id == replay_stage_id
    ));

    let snapshot = control.request(route, NodeRequest::Snapshot).await.unwrap();
    let snapshot = match snapshot.response {
        Ok(C2NodeResponse::Snapshot { snapshot, .. }) => snapshot,
        response => panic!("Node snapshot failed after abort: {response:?}"),
    };
    let published_bundles = snapshot.launch_inventory.as_ref()
        .and_then(|inventory| inventory.bundles.as_ref())
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    assert!(published_bundles.iter().all(|bundle| {
        bundle.id != delivery.manifest().bundle_id
            || bundle.revision != delivery.manifest().revision
    }));

    // The positive production authority, restart, no-resend, materialization, and spawn path is
    // owned by `windows_schedule_next_delivery_c2_e2e::schedule_next_delivery_stages_materializes_and_does_not_resend`.
    drop(control);
    event_drain.abort();
    let _ = event_drain.await;

    let c2_shutdown = c2.shutdown_handle();
    c2_shutdown.shutdown();
    timeout(Duration::from_secs(5), c2.wait()).await.unwrap().unwrap();
    node_shutdown.request_shutdown().await.unwrap();
    timeout(Duration::from_secs(10), node_task).await.unwrap().unwrap().unwrap();
    drop(node_shutdown);

    for durable_plane in [&node_state_path, &delivery_store_root] {
        assert_private_bytes_absent(
            durable_plane,
            &[RAW_CANARY, node_token, c2_token],
        );
    }
}
