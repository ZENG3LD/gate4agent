use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::borrow::Borrow;
use std::fmt;
use std::str::FromStr;
use thiserror::Error;

const MAX_ADAPTER_ID_LEN: usize = 96;
pub const MAX_ADAPTER_REVISION_LEN: usize = 256;

/// Stable identifier for one provider adapter implementation.
///
/// Adapter identity is deliberately independent from `AgentId`: several agents
/// may share a reviewed wire shape without being re-identified as one another.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct AdapterId(String);

impl AdapterId {
    pub fn new(value: impl Into<String>) -> Result<Self, AdapterIdError> {
        let value = value.into();
        validate_id(&value)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

fn validate_id(value: &str) -> Result<(), AdapterIdError> {
    if value.is_empty() {
        return Err(AdapterIdError::Empty);
    }
    if value.len() > MAX_ADAPTER_ID_LEN {
        return Err(AdapterIdError::TooLong {
            len: value.len(),
            max: MAX_ADAPTER_ID_LEN,
        });
    }
    if !value.bytes().all(|byte| {
        byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
    }) {
        return Err(AdapterIdError::InvalidCharacters(value.to_owned()));
    }
    if matches!(value.as_bytes().first(), Some(b'-' | b'_'))
        || matches!(value.as_bytes().last(), Some(b'-' | b'_'))
    {
        return Err(AdapterIdError::InvalidBoundary(value.to_owned()));
    }
    Ok(())
}

impl AsRef<str> for AdapterId {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl Borrow<str> for AdapterId {
    fn borrow(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Display for AdapterId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for AdapterId {
    type Err = AdapterIdError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl Serialize for AdapterId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for AdapterId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Error, Clone, Eq, PartialEq)]
pub enum AdapterIdError {
    #[error("adapter ID cannot be empty")]
    Empty,
    #[error("adapter ID length {len} exceeds the {max}-byte limit")]
    TooLong { len: usize, max: usize },
    #[error("adapter ID must contain only lowercase ASCII letters, digits, '-' or '_': {0}")]
    InvalidCharacters(String),
    #[error("adapter ID cannot start or end with '-' or '_': {0}")]
    InvalidBoundary(String),
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AdapterFamily {
    PtySemantic,
    Pipe,
    OneShot,
    Acp,
    Hook,
    ManagedHook,
    History,
    Resume,
    SessionOptions,
    CapabilityProbe,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AdapterVerification {
    Reference,
    SyntheticFixture,
    CapturedFixture,
    VendorCanary,
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct AdapterBinding {
    pub id: AdapterId,
    pub revision: String,
    pub verification: AdapterVerification,
}

impl AdapterBinding {
    pub fn new(
        id: AdapterId,
        revision: impl Into<String>,
        verification: AdapterVerification,
    ) -> Result<Self, AdapterBindingError> {
        let revision = revision.into();
        validate_revision(&revision)?;
        Ok(Self {
            id,
            revision,
            verification,
        })
    }

    pub fn validate(&self) -> Result<(), AdapterBindingError> {
        validate_revision(&self.revision)
    }
}

fn validate_revision(revision: &str) -> Result<(), AdapterBindingError> {
    if revision.trim().is_empty() {
        return Err(AdapterBindingError::EmptyRevision);
    }
    if revision.len() > MAX_ADAPTER_REVISION_LEN {
        return Err(AdapterBindingError::RevisionTooLong {
            len: revision.len(),
            max: MAX_ADAPTER_REVISION_LEN,
        });
    }
    if revision.chars().any(char::is_control) {
        return Err(AdapterBindingError::InvalidRevision(revision.to_owned()));
    }
    Ok(())
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum AdapterBindingError {
    #[error("adapter revision cannot be empty")]
    EmptyRevision,
    #[error("adapter revision length {len} exceeds the {max}-byte limit")]
    RevisionTooLong { len: usize, max: usize },
    #[error("adapter revision contains control characters: {0:?}")]
    InvalidRevision(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adapter_ids_are_extensible_and_wire_safe() {
        let id = AdapterId::new("grok-hook-v1").unwrap();
        assert_eq!(id.as_str(), "grok-hook-v1");
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(serde_json::from_str::<AdapterId>(&json).unwrap(), id);
    }

    #[test]
    fn invalid_ids_and_revisions_are_rejected() {
        assert!(matches!(
            AdapterId::new("Grok"),
            Err(AdapterIdError::InvalidCharacters(_))
        ));
        let id = AdapterId::new("grok").unwrap();
        assert!(matches!(
            AdapterBinding::new(id, "", AdapterVerification::Reference),
            Err(AdapterBindingError::EmptyRevision)
        ));
    }
}
