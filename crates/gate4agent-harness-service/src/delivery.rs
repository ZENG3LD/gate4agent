//! Privacy-safe conversion of resolved Node delivery identities.

use gate4agent_harness_protocol::{
    HarnessDeliveryBundleSelectionV1, HarnessDeliveryComponentCountV1,
    HarnessDeliveryComponentKindV1,
    HarnessDeliveryBundleDigestV1, HarnessDeliveryBundleIdV1,
    HarnessDeliveryBundleRevisionV1, HarnessDeliveryBundleV1,
    HarnessDeliveryManifestDigestV2,
    HarnessDeliveryReceiptV1, HarnessDeliveryStageReceiptV1, HarnessDeliveryV1,
    HarnessReceiptRef, HarnessSelectorV1, HarnessSessionBindingV1,
};
use gate4agent_harness_delivery::CompiledDeliveryBundleV2;
use gate4agent_node_protocol::{
    DeliveryCommitReceiptV1, DeliveryComponentKindV2, DeliveryScopeV2,
};
use std::collections::BTreeMap;

use crate::HarnessServiceError;

pub fn compiled_bundle_identity(
    selector: HarnessSelectorV1,
    compiled: &CompiledDeliveryBundleV2,
) -> Result<HarnessDeliveryBundleV1, HarnessServiceError> {
    compiled.verify().map_err(|_| HarnessServiceError::DeliveryCompilationInvalid)?;
    let manifest = compiled.manifest();
    Ok(HarnessDeliveryBundleV1 {
        selector,
        bundle_id: HarnessDeliveryBundleIdV1::new(manifest.bundle_id.as_str())?,
        revision: HarnessDeliveryBundleRevisionV1::new(manifest.revision.as_str())?,
        digest: HarnessDeliveryBundleDigestV1::new(manifest.bundle_digest.as_str())?,
        manifest_digest: HarnessDeliveryManifestDigestV2::new(
            manifest.manifest_digest.as_str(),
        )?,
    })
}

pub fn compiled_bundle_selection(
    selector: HarnessSelectorV1,
    compiled: &CompiledDeliveryBundleV2,
) -> Result<HarnessDeliveryBundleSelectionV1, HarnessServiceError> {
    compiled.verify().map_err(|_| HarnessServiceError::DeliveryCompilationInvalid)?;
    let mut counts = BTreeMap::<HarnessDeliveryComponentKindV1, (u32, u32)>::new();
    for component in &compiled.manifest().components {
        let kind = component_kind(component.kind);
        let count = counts.entry(kind).or_default();
        let target = match component.scope {
            DeliveryScopeV2::Workspace => &mut count.0,
            DeliveryScopeV2::Session => &mut count.1,
        };
        *target = target.checked_add(1)
            .ok_or(HarnessServiceError::DeliveryCompilationInvalid)?;
    }
    let selection = HarnessDeliveryBundleSelectionV1 {
        bundle: compiled_bundle_identity(selector, compiled)?,
        component_counts: counts.into_iter().map(
            |(kind, (workspace_count, session_count))| HarnessDeliveryComponentCountV1 {
                kind,
                workspace_count,
                session_count,
            },
        ).collect(),
    };
    selection.validate()?;
    Ok(selection)
}

fn component_kind(kind: DeliveryComponentKindV2) -> HarnessDeliveryComponentKindV1 {
    match kind {
        DeliveryComponentKindV2::Skill => HarnessDeliveryComponentKindV1::Skill,
        DeliveryComponentKindV2::PluginManifest => {
            HarnessDeliveryComponentKindV1::PluginManifest
        }
        DeliveryComponentKindV2::Prompt => HarnessDeliveryComponentKindV1::Prompt,
        DeliveryComponentKindV2::Instructions => HarnessDeliveryComponentKindV1::Instructions,
        DeliveryComponentKindV2::AgentDefinition => {
            HarnessDeliveryComponentKindV1::AgentDefinition
        }
        DeliveryComponentKindV2::Command => HarnessDeliveryComponentKindV1::Command,
        DeliveryComponentKindV2::File => HarnessDeliveryComponentKindV1::File,
        DeliveryComponentKindV2::Template => HarnessDeliveryComponentKindV1::Template,
        DeliveryComponentKindV2::McpDeclaration => {
            HarnessDeliveryComponentKindV1::McpDeclaration
        }
    }
}

pub fn resolved_bundle_identity(
    selector: HarnessSelectorV1,
    receipt: &DeliveryCommitReceiptV1,
) -> Result<HarnessDeliveryBundleV1, HarnessServiceError> {
    Ok(HarnessDeliveryBundleV1 {
        selector,
        bundle_id: HarnessDeliveryBundleIdV1::new(receipt.bundle_id.as_str())?,
        revision: HarnessDeliveryBundleRevisionV1::new(receipt.revision.as_str())?,
        digest: HarnessDeliveryBundleDigestV1::new(receipt.bundle_digest.as_str())?,
        manifest_digest: HarnessDeliveryManifestDigestV2::new(
            receipt.manifest_digest.as_str(),
        )?,
    })
}

pub(crate) fn staged_receipt(
    node_id: HarnessSelectorV1,
    node_incarnation: HarnessSelectorV1,
    workspace_id: HarnessSelectorV1,
    selector: HarnessSelectorV1,
    receipt: &DeliveryCommitReceiptV1,
    staged_at_unix_ms: u64,
) -> Result<HarnessDeliveryStageReceiptV1, HarnessServiceError> {
    let receipt = HarnessDeliveryStageReceiptV1 {
        node_id,
        node_incarnation,
        workspace_id,
        bundle: resolved_bundle_identity(selector, receipt)?,
        staged_at_unix_ms,
    };
    receipt.validate()?;
    Ok(receipt)
}

pub fn terminal_receipt(
    delivery: &HarnessDeliveryV1,
    receipt_ref: HarnessReceiptRef,
    binding: HarnessSessionBindingV1,
    committed_at_unix_ms: u64,
) -> Result<HarnessDeliveryReceiptV1, HarnessServiceError> {
    let receipt = HarnessDeliveryReceiptV1 {
        receipt_ref,
        delivery_ref: delivery.delivery_ref.clone(),
        authority: delivery.authority.clone(),
        task_id: delivery.task_id.clone(),
        run_id: delivery.run_id.clone(),
        operation_id: delivery.operation_id.clone(),
        binding,
        bundle: delivery.bundle.clone(),
        committed_at_unix_ms,
    };
    receipt.validate()?;
    Ok(receipt)
}
