use std::time::Duration;

use gate4agent::acp::{AcpSession, AcpSessionOptions};
use gate4agent::{AgentEvent, CliTool};
use gate4agent_testkit::acp_agent_spec;

#[tokio::test]
async fn acp_session_handshake_and_host_callbacks_are_fail_closed() {
    let spec = acp_agent_spec();
    let launch = spec
        .capabilities
        .transports
        .acp
        .as_ref()
        .and_then(|transport| transport.launch_override.as_ref())
        .expect("controlled ACP launch")
        .clone();
    let options = AcpSessionOptions {
        handshake_timeout: Duration::from_secs(10),
        prompt_timeout: Duration::from_secs(10),
        ..AcpSessionOptions::default()
    };
    let session = AcpSession::spawn_with_launch(
        CliTool::Gemini,
        &std::env::current_dir().expect("current directory"),
        options,
        &launch,
    )
    .await
    .expect("fail-closed ACP handshake");
    let mut events = session.subscribe();

    session
        .prompt("exercise fail-closed host callbacks")
        .await
        .expect("fixture accepted every denial");

    let mut callback_methods = Vec::new();
    let mut received_text = false;
    while let Ok(event) = events.try_recv() {
        match event {
            AgentEvent::RpcIncomingRequest { method, .. } => callback_methods.push(method),
            AgentEvent::Text { text, .. } if text == "fixture-acp-response" => {
                received_text = true;
            }
            _ => {}
        }
    }
    assert_eq!(
        callback_methods,
        [
            "fs/read_text_file",
            "terminal/create",
            "session/request_permission",
        ]
    );
    assert!(
        received_text,
        "fixture response must follow verified denials"
    );

    session.kill().await.expect("stop controlled ACP fixture");
}
