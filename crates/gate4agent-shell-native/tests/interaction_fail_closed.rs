use gate4agent_catalog::AgentRegistry;
use gate4agent_shell_native::NativeEffectShell;
use gate4agent_testkit::acp_agent_spec;
use gate4agent_types::{
    AdapterFamily, AgentInstanceId, ControlEffect, ControlObservation, EffectEnvelope, OperationId,
    ProviderInteractionId, ProviderInteractionKind, ProviderInteractionResponse,
    ProviderInteractionTarget, ProviderSource, SessionGeneration, CONTROL_PROTOCOL_VERSION,
};

#[tokio::test]
async fn native_interaction_resolution_fails_closed_with_exact_correlation() {
    let spec = acp_agent_spec();
    let source = ProviderSource {
        family: AdapterFamily::Acp,
        binding: spec
            .capabilities
            .transports
            .acp
            .as_ref()
            .expect("controlled ACP transport")
            .adapter
            .clone(),
    };
    let mut shell =
        NativeEffectShell::new(AgentRegistry::new([spec]).expect("controlled native catalog"));
    let operation_id = OperationId(701);
    let instance_id = AgentInstanceId(702);
    let generation = SessionGeneration(3);
    let interaction_id = ProviderInteractionId(703);

    let observation = shell
        .execute(EffectEnvelope {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            operation_id,
            instance_id,
            generation,
            effect: ControlEffect::ResolveInteraction {
                target: ProviderInteractionTarget {
                    interaction_id,
                    source,
                    provider_request_id: Some("fixture-request".to_owned()),
                    interaction_kind: ProviderInteractionKind::Approval,
                    tool_name: "fixture-tool".to_owned(),
                    agent_id: None,
                },
                response: ProviderInteractionResponse::Deny,
            },
        })
        .await;

    assert_eq!(observation.operation_id, Some(operation_id));
    assert_eq!(observation.instance_id, instance_id);
    assert_eq!(observation.generation, generation);
    assert!(matches!(
        observation.observation,
        ControlObservation::InteractionResolutionFailed {
            interaction_id: observed_interaction_id,
            ref message,
        } if observed_interaction_id == interaction_id
            && message == "native interaction resolution authority is not configured"
    ));
    assert_eq!(shell.active_session_count(), 0);
}
