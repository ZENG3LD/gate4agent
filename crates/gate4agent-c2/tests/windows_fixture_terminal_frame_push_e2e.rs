#![cfg(windows)]

use gate4agent_c2::protocol::{
    C2NodeEvent, C2NodeResponse, NodeId, NodeRoute, NodeTransportState,
    C2_TERMINAL_FRAME_EVENTS_CAPABILITY,
};
use gate4agent_c2::{C2Config, C2NodeConfig, C2Running, C2Timings};
use gate4agent_c2_client::{connect_local, C2Client, C2EventReceiver};
use gate4agent_node::protocol::{
    ClientRole, NodeEvent, NodeRequest, ServerFrame, SessionAddress, SessionMode, WorkspaceId,
    NODE_TERMINAL_FRAME_EVENTS_CAPABILITY,
};
use gate4agent_node::{NodeServer, NodeServerConfig, WorkspaceConfig};
use gate4agent_node_wire::NamedPipeNodeClient;
use gate4agent_types::{AgentId, TerminalControl, TerminalFrame, TerminalSize};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::time::{sleep, timeout};

fn endpoint(label: &str) -> String {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!(
        r"\\.\pipe\gate4agent-terminal-push-{label}-{}-{nonce}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed),
    )
}

fn node_config(endpoint: &str, token: &str, node_id: &NodeId) -> NodeServerConfig {
    NodeServerConfig::new(
        endpoint,
        token,
        node_id.clone(),
        [WorkspaceConfig::new(
            WorkspaceId::new("primary").unwrap(),
            std::env::current_dir().unwrap(),
        )
        .unwrap()],
    )
    .unwrap()
}

async fn wait_online(client: &C2Client, node_id: &NodeId) -> NodeRoute {
    timeout(Duration::from_secs(10), async {
        loop {
            if let Ok(status) = client.status().await {
                if status.nodes[node_id].transport == NodeTransportState::Online {
                    return NodeRoute {
                        node_id: node_id.clone(),
                        expected_incarnation_id: status.nodes[node_id]
                            .cursor
                            .expect("online fixture node has no cursor")
                            .incarnation_id,
                    };
                }
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("fixture node did not become online through C2")
}

async fn wait_source_terminal_frame(
    client: &mut NamedPipeNodeClient,
    address: &SessionAddress,
    expected_text: &str,
) -> (u64, TerminalFrame) {
    timeout(Duration::from_secs(15), async {
        loop {
            match client.recv().await.expect("source node event stream failed") {
                ServerFrame::Event(envelope) => match envelope.event {
                    NodeEvent::TerminalFrame {
                        address: event_address,
                        frame,
                    } if event_address == *address && frame.contents.contains(expected_text) => {
                        return (envelope.sequence, frame);
                    }
                    NodeEvent::ResyncRequired { .. } => {
                        panic!("source node terminal stream lagged before the expected frame")
                    }
                    _ => {}
                },
                frame => panic!("source node sent an unexpected idle frame: {frame:?}"),
            }
        }
    })
    .await
    .expect("source node did not push the expected terminal frame")
}

async fn wait_routed_terminal_frame(
    events: &mut C2EventReceiver,
    node_id: &NodeId,
    address: &SessionAddress,
    expected_text: &str,
) -> gate4agent_c2::protocol::RoutedNodeEvent {
    timeout(Duration::from_secs(15), async {
        loop {
            let event = events
                .recv()
                .await
                .expect("authenticated C2 event stream closed early");
            match &event.event {
                C2NodeEvent::TerminalFrame {
                    address: event_address,
                    frame,
                } if &event.node_id == node_id
                    && event_address == address
                    && frame.contents.contains(expected_text) => return event,
                C2NodeEvent::ResyncRequired { .. } if &event.node_id == node_id => {
                    panic!("C2 terminal stream required resync before the expected frame")
                }
                _ => {}
            }
        }
    })
    .await
    .expect("C2 did not push the expected routed terminal frame")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn windows_fixture_terminal_frame_push_routes_exact_event_and_preserves_commands() {
    let node_endpoint = endpoint("node");
    let control_endpoint = endpoint("control");
    let node_id = NodeId::new("terminal-push-node").unwrap();
    let node_token = "terminal-push-node-token";
    let c2_token = "terminal-push-c2-token";

    let server = NodeServer::new_fixture(node_config(
        &node_endpoint,
        node_token,
        &node_id,
    ))
    .unwrap();
    let node_shutdown = server.shutdown_handle();
    let node_task = tokio::spawn(server.run());

    let timings = C2Timings {
        poll_interval: Duration::from_millis(20),
        fresh_for: Duration::from_secs(2),
        attempt_deadline: Duration::from_secs(2),
        transient_backoffs: [Duration::from_millis(20); 5],
        parked_backoff: Duration::from_millis(100),
        http_io_deadline: Duration::from_secs(1),
    };
    let config = C2Config::new(
        "127.0.0.1:0".parse().unwrap(),
        c2_token,
        vec![C2NodeConfig::new(
            node_id.clone(),
            node_endpoint.clone(),
            node_token,
        )
        .unwrap()],
    )
    .unwrap()
    .with_control_endpoint(control_endpoint.clone())
    .unwrap()
    .with_timings(timings);
    let running = C2Running::start(config).await.unwrap();
    let http = C2Client::new(running.api_addr(), c2_token)
        .unwrap()
        .with_deadline(Duration::from_secs(1));
    let route = wait_online(&http, &node_id).await;

    let (control, mut events) = connect_local(&control_endpoint, c2_token).await.unwrap();
    assert!(control
        .hello()
        .compatibility
        .as_ref()
        .unwrap()
        .capabilities
        .iter()
        .any(|capability| capability.as_str() == C2_TERMINAL_FRAME_EVENTS_CAPABILITY));

    let mut source = NamedPipeNodeClient::connect(
        &node_endpoint,
        &node_id,
        ClientRole::Observer,
        node_token,
    )
    .await
    .unwrap();
    assert!(source
        .hello()
        .compatibility
        .as_ref()
        .unwrap()
        .capabilities
        .iter()
        .any(|capability| capability.as_str() == NODE_TERMINAL_FRAME_EVENTS_CAPABILITY));

    let spawned = control
        .request(
            route.clone(),
            NodeRequest::Spawn {
                workspace_id: WorkspaceId::new("primary").unwrap(),
                provider: AgentId::new("claude").unwrap(),
                mode: SessionMode::Pty,
                terminal_size: TerminalSize {
                    rows: 24,
                    columns: 80,
                },
                initial_prompt: None,
            },
        )
        .await
        .unwrap();
    let address = match spawned.response {
        Ok(C2NodeResponse::SpawnAccepted { session }) => session,
        response => panic!("fixture spawn returned an unexpected response: {response:?}"),
    };

    let source_ready = wait_source_terminal_frame(&mut source, &address, "fixture-ready>");
    let routed_ready = wait_routed_terminal_frame(
        &mut events,
        &node_id,
        &address,
        "fixture-ready>",
    );
    let ((ready_source_sequence, ready_source_frame), ready_routed) =
        tokio::join!(source_ready, routed_ready);
    let C2NodeEvent::TerminalFrame {
        address: ready_address,
        frame: ready_frame,
    } = ready_routed.event
    else {
        unreachable!("routed ready waiter returned a different event")
    };
    assert_eq!(ready_routed.node_id, node_id);
    assert_eq!(ready_routed.cursor.sequence, ready_source_sequence);
    assert_eq!(ready_address, address);
    assert_eq!(ready_frame, ready_source_frame);

    let prompt = "terminal-frame-push-e2e";
    assert!(matches!(
        control
            .request(
                route.clone(),
                NodeRequest::Input {
                    session: address.clone(),
                    text: prompt.to_owned(),
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
                NodeRequest::TerminalControl {
                    session: address.clone(),
                    control: TerminalControl::Enter,
                },
            )
            .await
            .unwrap()
            .response,
        Ok(C2NodeResponse::Accepted)
    ));

    let expected_text = format!("fixture-echo:{prompt}");
    let source_event = wait_source_terminal_frame(&mut source, &address, &expected_text);
    let routed_event = wait_routed_terminal_frame(
        &mut events,
        &node_id,
        &address,
        &expected_text,
    );
    let ((source_sequence, source_frame), routed) = tokio::join!(source_event, routed_event);
    let C2NodeEvent::TerminalFrame {
        address: routed_address,
        frame: routed_frame,
    } = routed.event
    else {
        unreachable!("routed terminal waiter returned a different event")
    };
    assert_eq!(routed.node_id, node_id);
    assert_eq!(routed.cursor.incarnation_id, route.expected_incarnation_id);
    assert_eq!(routed.cursor.sequence, source_sequence);
    assert_eq!(routed_address, address);
    assert_eq!(&routed_frame.formatted, &source_frame.formatted);
    assert_eq!(routed_frame, source_frame);
    assert!(!routed_frame.formatted.is_empty());
    assert!(routed_frame.contents.contains(&expected_text));
    drop(source);
    let event_drain = tokio::spawn(async move {
        while events.recv().await.is_some() {}
    });

    assert!(matches!(
        control
            .request(
                route.clone(),
                NodeRequest::Resize {
                    session: address.clone(),
                    size: TerminalSize {
                        rows: 30,
                        columns: 100,
                    },
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
                route,
                NodeRequest::Stop {
                    session: address,
                    force: true,
                },
            )
            .await
            .unwrap()
            .response,
        Ok(C2NodeResponse::Accepted)
    ));

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
}
