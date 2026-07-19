use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use thiserror::Error;

pub const SESSION_OPTION_ID_MAX_BYTES: usize = 128;
pub const SESSION_OPTION_VALUE_MAX_BYTES: usize = 512;
pub const SESSION_OPTION_VALUES_MAX: usize = 64;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SessionOptionValue {
    String(String),
    Boolean(bool),
}

impl SessionOptionValue {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(value) => Some(value),
            Self::Boolean(_) => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Boolean(value) => Some(*value),
            Self::String(_) => None,
        }
    }
}

impl From<String> for SessionOptionValue {
    fn from(value: String) -> Self {
        Self::String(value)
    }
}

impl From<&str> for SessionOptionValue {
    fn from(value: &str) -> Self {
        Self::String(value.to_owned())
    }
}

impl From<bool> for SessionOptionValue {
    fn from(value: bool) -> Self {
        Self::Boolean(value)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SessionOptionSelection {
    pub model: String,
    #[serde(default)]
    pub values: BTreeMap<String, SessionOptionValue>,
}

impl SessionOptionSelection {
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            values: BTreeMap::new(),
        }
    }

    pub fn with_value(
        mut self,
        id: impl Into<String>,
        value: impl Into<SessionOptionValue>,
    ) -> Self {
        self.values.insert(id.into(), value.into());
        self
    }

    pub fn validate(&self) -> Result<(), SessionOptionValidationError> {
        validate_value("model", &self.model)?;
        if self.values.len() > SESSION_OPTION_VALUES_MAX {
            return Err(SessionOptionValidationError::TooManyValues {
                count: self.values.len(),
                max: SESSION_OPTION_VALUES_MAX,
            });
        }
        for (id, value) in &self.values {
            if id == "model" {
                return Err(SessionOptionValidationError::ReservedModelOption);
            }
            if id.is_empty()
                || id.len() > SESSION_OPTION_ID_MAX_BYTES
                || id.chars().any(char::is_control)
            {
                return Err(SessionOptionValidationError::InvalidOptionId);
            }
            if let SessionOptionValue::String(value) = value {
                validate_value("option value", value)?;
            }
        }
        Ok(())
    }
}

fn validate_value(field: &'static str, value: &str) -> Result<(), SessionOptionValidationError> {
    if value.trim().is_empty()
        || value.len() > SESSION_OPTION_VALUE_MAX_BYTES
        || value.chars().any(char::is_control)
    {
        return Err(SessionOptionValidationError::InvalidValue { field });
    }
    Ok(())
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum SessionOptionValidationError {
    #[error("session-option {field} is empty, contains controls, or exceeds its bound")]
    InvalidValue { field: &'static str },
    #[error("session-option ID is empty, contains controls, or exceeds its bound")]
    InvalidOptionId,
    #[error("session-option selection cannot store a second model field")]
    ReservedModelOption,
    #[error("session-option selection has {count} values; the limit is {max}")]
    TooManyValues { count: usize, max: usize },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selection_round_trips_as_a_typed_wire_value() {
        let selection = SessionOptionSelection::new("opus")
            .with_value("effort", "xhigh")
            .with_value("fastMode", true);
        let encoded = serde_json::to_string(&selection).unwrap();
        assert_eq!(
            serde_json::from_str::<SessionOptionSelection>(&encoded).unwrap(),
            selection
        );
        selection.validate().unwrap();
        assert!(SessionOptionSelection::new("bad\nmodel")
            .validate()
            .is_err());
    }
}
