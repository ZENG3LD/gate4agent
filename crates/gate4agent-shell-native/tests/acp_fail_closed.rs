use std::time::Duration;

use gate4agent::acp::{AcpSession, AcpSessionOptions, HostPolicy};
use gate4agent::{AgentEvent, CliTool, HostDecisionAuthority, HostRequestDecision};
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
        // `AcpSessionOptions::default()` is `HostPolicy::Auto` -- this test's
        // whole point is proving `Deny` fidelity, so it must ask for `Deny`
        // explicitly rather than relying on the default.
        host_policy: HostPolicy::Deny,
        ..AcpSessionOptions::default()
    };
    let session = AcpSession::spawn_with_launch(
        CliTool::ClaudeCode,
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

    let mut callbacks = Vec::new();
    let mut received_text = false;
    while let Ok(event) = events.try_recv() {
        match event {
            AgentEvent::RpcIncomingRequest { method, decision, .. } => {
                callbacks.push((method, decision));
            }
            AgentEvent::Text { text, .. } if text == "fixture-acp-response" => {
                received_text = true;
            }
            _ => {}
        }
    }
    let callback_methods: Vec<_> = callbacks.iter().map(|(method, _)| method.clone()).collect();
    assert_eq!(
        callback_methods,
        [
            "fs/read_text_file",
            "terminal/create",
            "session/request_permission",
        ]
    );
    // Every callback in this fixture is a benign call (no dangerous-command
    // gate rule matches any of them) under `HostPolicy::Deny`, so every one
    // must be `Denied { by: Policy }` -- not merely denied, but denied BY
    // THE POLICY, never by the gate and never left `Deferred`.
    assert!(
        callbacks.iter().all(|(_, decision)| *decision
            == HostRequestDecision::Denied { by: HostDecisionAuthority::Policy }),
        "every callback under HostPolicy::Deny must be denied by policy: {callbacks:?}"
    );
    assert!(
        received_text,
        "fixture response must follow verified denials"
    );

    session.kill().await.expect("stop controlled ACP fixture");
}
