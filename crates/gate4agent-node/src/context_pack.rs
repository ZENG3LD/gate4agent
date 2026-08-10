use crate::protocol::{
    ContextPackLineageReceipt, ResolvedContextPackReceipt, SpawnContextDigest,
    SpawnContextId, MAX_CONTEXT_PACK_BYTES,
};
use gate4agent_types::{AgentId, HistoryMessageRecord, HistorySessionRecord};
use ring::digest::{Context, SHA256};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use thiserror::Error;

pub(crate) const MAX_CONTEXT_PACK_CATALOG_ENTRIES: usize = 128;
const CONTEXT_PACK_SCHEMA: &str = "g4a-context-pack-v1";
const CONTEXT_PACK_DIGEST_DOMAIN: &[u8] = b"g4a-context-pack-v1\0";

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct ContextPackDocumentV1 {
    schema: String,
    source_provider: AgentId,
    history_session_id: String,
    title: Option<String>,
    model: Option<String>,
    source_message_count: u64,
    retained_messages: Vec<HistoryMessageRecord>,
    truncated: bool,
}

#[derive(Clone, Eq, PartialEq)]
pub(crate) struct NodeContextPack {
    receipt: ResolvedContextPackReceipt,
    bytes: Vec<u8>,
}

impl fmt::Debug for NodeContextPack {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("NodeContextPack")
            .field("receipt", &self.receipt)
            .field("bytes", &format_args!("[REDACTED; {} bytes]", self.bytes.len()))
            .finish()
    }
}

impl NodeContextPack {
    pub(crate) fn export(
        lineage: ContextPackLineageReceipt,
        history: &HistorySessionRecord,
    ) -> Result<Self, ContextPackError> {
        history.validate().map_err(|_| ContextPackError::InvalidHistory)?;
        if history.messages.is_empty()
            || history.message_count
                < u64::try_from(history.messages.len()).unwrap_or(u64::MAX)
        {
            return Err(ContextPackError::Empty);
        }

        let mut retained_messages = history.messages.clone();
        let mut truncated = history.message_count
            > u64::try_from(retained_messages.len()).unwrap_or(u64::MAX);
        let bytes = loop {
            let document = ContextPackDocumentV1 {
                schema: CONTEXT_PACK_SCHEMA.to_owned(),
                source_provider: lineage.source_provider.clone(),
                history_session_id: history.session_id.clone(),
                title: history.title.clone(),
                model: history.model.clone(),
                source_message_count: history.message_count,
                retained_messages: retained_messages.clone(),
                truncated,
            };
            let bytes = serde_json::to_vec(&document)
                .map_err(|_| ContextPackError::Serialization)?;
            if bytes.len() <= MAX_CONTEXT_PACK_BYTES as usize {
                break bytes;
            }
            if retained_messages.len() <= 1 {
                return Err(ContextPackError::TooLarge);
            }
            retained_messages.remove(0);
            truncated = true;
        };

        let digest = context_digest(&lineage, &bytes)?;
        let context_id = context_id(&digest)?;
        let receipt = ResolvedContextPackReceipt {
            id: context_id,
            digest,
            lineage,
            source_message_count: history.message_count,
            retained_message_count: u64::try_from(retained_messages.len())
                .map_err(|_| ContextPackError::TooLarge)?,
            byte_len: u32::try_from(bytes.len()).map_err(|_| ContextPackError::TooLarge)?,
            truncated,
        };
        Ok(Self { receipt, bytes })
    }

    pub(crate) fn from_materialized(
        receipt: ResolvedContextPackReceipt,
        bytes: Vec<u8>,
    ) -> Result<Self, ContextPackError> {
        if bytes.len() != receipt.byte_len as usize
            || bytes.is_empty()
            || bytes.len() > MAX_CONTEXT_PACK_BYTES as usize
            || context_digest(&receipt.lineage, &bytes)? != receipt.digest
            || context_id(&receipt.digest)? != receipt.id
        {
            return Err(ContextPackError::ReceiptMismatch);
        }
        let document: ContextPackDocumentV1 = serde_json::from_slice(&bytes)
            .map_err(|_| ContextPackError::Serialization)?;
        let normalized = HistorySessionRecord {
            session_id: document.history_session_id.clone(),
            title: document.title.clone(),
            cwd: None,
            model: document.model.clone(),
            message_count: document.source_message_count,
            total_tokens: 0,
            messages: document.retained_messages.clone(),
        };
        if document.schema != CONTEXT_PACK_SCHEMA
            || document.source_provider != receipt.lineage.source_provider
            || document.source_message_count != receipt.source_message_count
            || u64::try_from(document.retained_messages.len()).ok()
                != Some(receipt.retained_message_count)
            || document.truncated != receipt.truncated
            || document.retained_messages.is_empty()
            || normalized.validate().is_err()
        {
            return Err(ContextPackError::ReceiptMismatch);
        }
        Ok(Self { receipt, bytes })
    }

    pub(crate) fn receipt(&self) -> &ResolvedContextPackReceipt {
        &self.receipt
    }

    pub(crate) fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

#[derive(Clone, Default)]
pub(crate) struct ContextPackCatalog {
    packs: BTreeMap<SpawnContextId, NodeContextPack>,
}

impl fmt::Debug for ContextPackCatalog {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ContextPackCatalog")
            .field("receipts", &self.packs.values().map(NodeContextPack::receipt).collect::<Vec<_>>())
            .finish()
    }
}

impl ContextPackCatalog {
    pub(crate) fn insert(
        &mut self,
        pack: NodeContextPack,
    ) -> Result<ResolvedContextPackReceipt, ContextPackError> {
        if let Some(existing) = self.packs.get(&pack.receipt().id) {
            if existing == &pack {
                return Ok(existing.receipt().clone());
            }
            return Err(ContextPackError::IdentityConflict);
        }
        if self.packs.len() >= MAX_CONTEXT_PACK_CATALOG_ENTRIES {
            return Err(ContextPackError::CatalogFull);
        }
        let receipt = pack.receipt().clone();
        self.packs.insert(receipt.id.clone(), pack);
        Ok(receipt)
    }

    pub(crate) fn get(&self, id: &SpawnContextId) -> Option<&NodeContextPack> {
        self.packs.get(id)
    }

    pub(crate) fn remove(&mut self, id: &SpawnContextId) -> Option<NodeContextPack> {
        self.packs.remove(id)
    }
}

fn context_digest(
    lineage: &ContextPackLineageReceipt,
    bytes: &[u8],
) -> Result<SpawnContextDigest, ContextPackError> {
    let lineage = serde_json::to_vec(lineage).map_err(|_| ContextPackError::Serialization)?;
    let mut context = Context::new(&SHA256);
    context.update(CONTEXT_PACK_DIGEST_DOMAIN);
    context.update(&lineage);
    context.update(&[0]);
    context.update(bytes);
    let hex = context
        .finish()
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    SpawnContextDigest::new(format!("sha256:{hex}"))
        .map_err(|_| ContextPackError::Serialization)
}

fn context_id(digest: &SpawnContextDigest) -> Result<SpawnContextId, ContextPackError> {
    let hex = digest
        .as_str()
        .strip_prefix("sha256:")
        .ok_or(ContextPackError::Serialization)?;
    SpawnContextId::new(format!("ctx-{hex}"))
        .map_err(|_| ContextPackError::Serialization)
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub(crate) enum ContextPackError {
    #[error("history is invalid")]
    InvalidHistory,
    #[error("history does not contain retained messages")]
    Empty,
    #[error("context pack exceeds the bounded size")]
    TooLarge,
    #[error("context pack serialization failed")]
    Serialization,
    #[error("context pack receipt does not match materialized bytes")]
    ReceiptMismatch,
    #[error("context pack catalog is full")]
    CatalogFull,
    #[error("context pack identity conflicts with existing bytes")]
    IdentityConflict,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{NodeId, SessionAddress, SessionKey, WorkspaceId};
    use gate4agent_types::{AgentInstanceId, HistoryMessageRole, SessionGeneration};

    fn lineage() -> ContextPackLineageReceipt {
        ContextPackLineageReceipt {
            source_node_id: NodeId::new("node-local").unwrap(),
            source_session: SessionAddress {
                workspace_id: WorkspaceId::new("source").unwrap(),
                session: SessionKey {
                    instance_id: AgentInstanceId(7),
                    generation: SessionGeneration(2),
                },
            },
            source_provider: AgentId::new("qwen-code").unwrap(),
        }
    }

    #[test]
    fn export_is_deterministic_bounded_and_roundtrips_materialized_bytes() {
        let history = HistorySessionRecord {
            session_id: "vendor-session".to_owned(),
            title: Some("review".to_owned()),
            cwd: Some(r"C:\private\repo".to_owned()),
            model: Some("qwen3-coder".to_owned()),
            message_count: 2,
            total_tokens: 19,
            messages: vec![
                HistoryMessageRecord {
                    role: HistoryMessageRole::User,
                    text: "inspect the patch".to_owned(),
                },
                HistoryMessageRecord {
                    role: HistoryMessageRole::Assistant,
                    text: "the patch is bounded".to_owned(),
                },
            ],
        };
        let first = NodeContextPack::export(lineage(), &history).unwrap();
        let second = NodeContextPack::export(lineage(), &history).unwrap();

        assert_eq!(first, second);
        assert!(!String::from_utf8_lossy(first.bytes()).contains(r"C:\private\repo"));
        let restored = NodeContextPack::from_materialized(
            first.receipt().clone(),
            first.bytes().to_vec(),
        )
        .unwrap();
        assert_eq!(restored, first);
    }

    #[test]
    fn export_drops_oldest_messages_to_the_protocol_byte_limit() {
        let messages = (0..gate4agent_types::HISTORY_MESSAGES_MAX)
            .map(|index| HistoryMessageRecord {
                role: if index % 2 == 0 {
                    HistoryMessageRole::User
                } else {
                    HistoryMessageRole::Assistant
                },
                text: format!("message-{index:03}-{}", "x".repeat(8_000)),
            })
            .collect::<Vec<_>>();
        let history = HistorySessionRecord {
            session_id: "large-session".to_owned(),
            title: None,
            cwd: None,
            model: None,
            message_count: messages.len() as u64,
            total_tokens: 0,
            messages,
        };
        let pack = NodeContextPack::export(lineage(), &history).unwrap();
        let document: ContextPackDocumentV1 = serde_json::from_slice(pack.bytes()).unwrap();

        assert!(pack.bytes().len() <= MAX_CONTEXT_PACK_BYTES as usize);
        assert!(pack.receipt().truncated);
        assert!(document.retained_messages.first().unwrap().text.starts_with("message-"));
        assert!(document.retained_messages.last().unwrap().text.starts_with("message-255"));
    }
}
