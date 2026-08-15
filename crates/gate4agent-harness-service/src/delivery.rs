//! Privacy-safe conversion of resolved Node delivery identities.

use gate4agent_harness_protocol::{
    HarnessDeliveryBundleDigestV1, HarnessDeliveryBundleIdV1,
    HarnessDeliveryBundleRevisionV1, HarnessDeliveryBundleV1,
    HarnessDeliveryManifestDigestV2,
    HarnessDeliveryReceiptV1, HarnessDeliveryStageReceiptV1, HarnessDeliveryV1,
    HarnessReceiptRef, HarnessSelectorV1, HarnessSessionBindingV1,
};
use gate4agent_harness_delivery::CompiledDeliveryBundleV2;
use gate4agent_node_protocol::DeliveryCommitReceiptV1;

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
        grant_id: delivery.grant_id.clone(),
        grant_revision: delivery.grant_revision,
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
