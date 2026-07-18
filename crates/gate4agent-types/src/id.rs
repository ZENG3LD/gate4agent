use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::borrow::Borrow;
use std::fmt;
use std::str::FromStr;
use thiserror::Error;

const MAX_AGENT_ID_LEN: usize = 64;

/// Stable, extensible identifier for an agent CLI.
///
/// Unlike a consumer's legacy closed tool enum, this type preserves IDs added
/// after a consumer was compiled. IDs use lowercase ASCII slugs so they remain
/// safe as registry keys and serialized protocol values.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct AgentId(String);

impl AgentId {
    pub fn new(value: impl Into<String>) -> Result<Self, AgentIdError> {
        let value = value.into();
        validate(&value)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

fn validate(value: &str) -> Result<(), AgentIdError> {
    if value.is_empty() {
        return Err(AgentIdError::Empty);
    }
    if value.len() > MAX_AGENT_ID_LEN {
        return Err(AgentIdError::TooLong {
            len: value.len(),
            max: MAX_AGENT_ID_LEN,
        });
    }
    if !value.bytes().all(|byte| {
        byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
    }) {
        return Err(AgentIdError::InvalidCharacters(value.to_owned()));
    }
    if matches!(value.as_bytes().first(), Some(b'-' | b'_'))
        || matches!(value.as_bytes().last(), Some(b'-' | b'_'))
    {
        return Err(AgentIdError::InvalidBoundary(value.to_owned()));
    }
    Ok(())
}

impl AsRef<str> for AgentId {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl Borrow<str> for AgentId {
    fn borrow(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Display for AgentId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for AgentId {
    type Err = AgentIdError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl Serialize for AgentId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for AgentId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Error, Clone, Eq, PartialEq)]
pub enum AgentIdError {
    #[error("agent ID cannot be empty")]
    Empty,
    #[error("agent ID length {len} exceeds the {max}-byte limit")]
    TooLong { len: usize, max: usize },
    #[error("agent ID must contain only lowercase ASCII letters, digits, '-' or '_': {0}")]
    InvalidCharacters(String),
    #[error("agent ID cannot start or end with '-' or '_': {0}")]
    InvalidBoundary(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_stable_slugs() {
        assert_eq!(AgentId::new("qwen-code").unwrap().as_str(), "qwen-code");
        assert_eq!(AgentId::new("agent_2").unwrap().as_str(), "agent_2");
    }

    #[test]
    fn rejects_ambiguous_ids() {
        assert!(matches!(AgentId::new(""), Err(AgentIdError::Empty)));
        assert!(matches!(
            AgentId::new("Claude"),
            Err(AgentIdError::InvalidCharacters(_))
        ));
        assert!(matches!(
            AgentId::new("-claude"),
            Err(AgentIdError::InvalidBoundary(_))
        ));
    }
}
