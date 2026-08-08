//! Bounded wire contract for the local Gate4Agent node.

use gate4agent_types::{
    AgentInstanceId, ControlEvent, ProviderSessionIdentity, SessionGeneration, SessionSnapshot,
    TerminalControl, TerminalSize,
};
use serde::de::{DeserializeOwned, MapAccess, SeqAccess, Visitor};
use serde::ser::SerializeStruct;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::borrow::{Borrow, Cow};
use std::fmt;
use std::io;
use std::str::FromStr;
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::time::{timeout, Duration};

pub const NODE_PROTOCOL_VERSION: u16 = 8;
pub const NODE_STATE_SCHEMA_V1: u16 = 1;
pub const NODE_STATE_SCHEMA_V2: u16 = 2;
pub const NODE_COMPATIBILITY_METADATA_CAPABILITY: &str = "compatibility.metadata";
pub const NODE_OPAQUE_UNIX_PATH_CAPABILITY: &str = "path.opaque-unix-bytes-v1";
pub const MAX_NODE_IDENTIFIER_BYTES: usize = 64;
pub const MAX_COMPATIBILITY_IDENTIFIER_BYTES: usize = 64;
pub const MAX_PROVIDER_CONTRACT_REVISION_BYTES: usize = 128;
pub const NODE_INCARNATION_ID_BYTES: usize = 16;
pub const MAX_NODE_FRAME_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_NODE_CLIENT_FRAME_BYTES: usize = 256 * 1024;
pub const MAX_NODE_TEXT_BYTES: usize = 32 * 1024;
pub const MAX_NODE_TERMINAL_BYTES: usize = 64;
pub const MAX_SESSION_DISPLAY_NAME_BYTES: usize = 256;
pub const MAX_WORKSPACE_ROOT_BYTES: usize = gate4agent_types::WORKING_DIRECTORY_MAX_BYTES;
pub const MAX_NODE_HELLO_FRAME_BYTES: usize = 8 * 1024;
pub const NODE_AUTH_NONCE_BYTES: usize = 32;
pub const NODE_AUTH_PROOF_BYTES: usize = 32;
pub const MAX_CONTROLLER_LEASE_MS: u64 = 60_000;
pub const MIN_CONTROLLER_LEASE_MS: u64 = 1_000;
pub const DEFAULT_CONTROLLER_LEASE_MS: u64 = 15_000;
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ClientRole {
    Operator,
    Observer,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum AgentProvider {
    Claude,
    Codex,
    Kimi,
}

impl AgentProvider {
    pub fn agent_id(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Kimi => "kimi",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SessionMode {
    Pty,
    Inline,
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct NodeId(String);

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct WorkspaceId(String);

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SessionRecordId(String);

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct NodeIncarnationId([u8; NODE_INCARNATION_ID_BYTES]);

impl NodeIncarnationId {
    pub fn from_bytes(bytes: [u8; NODE_INCARNATION_ID_BYTES]) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; NODE_INCARNATION_ID_BYTES] {
        &self.0
    }
}

impl fmt::Display for NodeIncarnationId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(formatter, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl FromStr for NodeIncarnationId {
    type Err = NodeIncarnationIdError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.len() != NODE_INCARNATION_ID_BYTES * 2 {
            return Err(NodeIncarnationIdError::InvalidLength {
                len: value.len(),
                expected: NODE_INCARNATION_ID_BYTES * 2,
            });
        }
        if !value.bytes().all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f')) {
            return Err(NodeIncarnationIdError::InvalidHex(value.to_owned()));
        }
        let mut bytes = [0; NODE_INCARNATION_ID_BYTES];
        for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
            bytes[index] = (decode_lower_hex(pair[0]) << 4) | decode_lower_hex(pair[1]);
        }
        Ok(Self(bytes))
    }
}

impl Serialize for NodeIncarnationId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for NodeIncarnationId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        value.parse().map_err(serde::de::Error::custom)
    }
}

fn decode_lower_hex(byte: u8) -> u8 {
    match byte {
        b'0'..=b'9' => byte - b'0',
        b'a'..=b'f' => byte - b'a' + 10,
        _ => unreachable!("lowercase hexadecimal input was validated"),
    }
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum NodeIncarnationIdError {
    #[error("node incarnation ID length {len} does not match the required {expected} lowercase hexadecimal characters")]
    InvalidLength { len: usize, expected: usize },
    #[error("node incarnation ID must contain exactly 32 lowercase hexadecimal characters: {0}")]
    InvalidHex(String),
}

macro_rules! identifier_impl {
    ($type:ident, $label:literal) => {
        impl $type {
            pub fn new(value: impl Into<String>) -> Result<Self, NodeIdentifierError> {
                let value = value.into();
                validate_identifier($label, &value)?;
                Ok(Self(value))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl AsRef<str> for $type {
            fn as_ref(&self) -> &str {
                self.as_str()
            }
        }

        impl Borrow<str> for $type {
            fn borrow(&self) -> &str {
                self.as_str()
            }
        }

        impl fmt::Display for $type {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(self.as_str())
            }
        }

        impl FromStr for $type {
            type Err = NodeIdentifierError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::new(value)
            }
        }

        impl Serialize for $type {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                serializer.serialize_str(self.as_str())
            }
        }

        impl<'de> Deserialize<'de> for $type {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let value = String::deserialize(deserializer)?;
                Self::new(value).map_err(serde::de::Error::custom)
            }
        }
    };
}

identifier_impl!(NodeId, "node");
identifier_impl!(WorkspaceId, "workspace");
identifier_impl!(SessionRecordId, "session record");

fn validate_identifier(label: &'static str, value: &str) -> Result<(), NodeIdentifierError> {
    if value.is_empty() {
        return Err(NodeIdentifierError::Empty { label });
    }
    if value.len() > MAX_NODE_IDENTIFIER_BYTES {
        return Err(NodeIdentifierError::TooLong {
            label,
            len: value.len(),
            max: MAX_NODE_IDENTIFIER_BYTES,
        });
    }
    if !value.bytes().all(|byte| {
        byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
    }) {
        return Err(NodeIdentifierError::InvalidCharacters {
            label,
            value: value.to_owned(),
        });
    }
    if matches!(value.as_bytes().first(), Some(b'-' | b'_'))
        || matches!(value.as_bytes().last(), Some(b'-' | b'_'))
    {
        return Err(NodeIdentifierError::InvalidBoundary {
            label,
            value: value.to_owned(),
        });
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum NodeIdentifierError {
    #[error("{label} ID cannot be empty")]
    Empty { label: &'static str },
    #[error("{label} ID length {len} exceeds the {max}-byte limit")]
    TooLong {
        label: &'static str,
        len: usize,
        max: usize,
    },
    #[error("{label} ID must contain only lowercase ASCII letters, digits, '-' or '_': {value}")]
    InvalidCharacters { label: &'static str, value: String },
    #[error("{label} ID cannot start or end with '-' or '_': {value}")]
    InvalidBoundary { label: &'static str, value: String },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct ProtocolRange {
    minimum: u16,
    maximum: u16,
}

impl ProtocolRange {
    pub fn new(minimum: u16, maximum: u16) -> Result<Self, ProtocolNegotiationError> {
        if minimum == 0 || maximum == 0 || minimum > maximum {
            return Err(ProtocolNegotiationError::InvalidRange { minimum, maximum });
        }
        Ok(Self { minimum, maximum })
    }

    pub fn exact(version: u16) -> Result<Self, ProtocolNegotiationError> {
        Self::new(version, version)
    }

    pub fn minimum(self) -> u16 {
        self.minimum
    }

    pub fn maximum(self) -> u16 {
        self.maximum
    }

    pub fn contains(self, version: u16) -> bool {
        self.minimum <= version && version <= self.maximum
    }

    pub fn highest_common(self, other: Self) -> Result<u16, ProtocolNegotiationError> {
        let minimum = self.minimum.max(other.minimum);
        let maximum = self.maximum.min(other.maximum);
        if minimum > maximum {
            return Err(ProtocolNegotiationError::Disjoint {
                local: self,
                remote: other,
            });
        }
        Ok(maximum)
    }
}

impl<'de> Deserialize<'de> for ProtocolRange {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct WireRange {
            minimum: u16,
            maximum: u16,
        }

        let wire = WireRange::deserialize(deserializer)?;
        Self::new(wire.minimum, wire.maximum).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum ProtocolNegotiationError {
    #[error("protocol range {minimum}..={maximum} is invalid")]
    InvalidRange { minimum: u16, maximum: u16 },
    #[error("protocol ranges {local:?} and {remote:?} do not overlap")]
    Disjoint {
        local: ProtocolRange,
        remote: ProtocolRange,
    },
    #[error("active wire protocol {active} is not contained in both ranges {local:?} and {remote:?}")]
    ActiveVersionUnsupported {
        active: u16,
        local: ProtocolRange,
        remote: ProtocolRange,
    },
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct CapabilityId(String);

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct OperatingSystemId(String);

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ArchitectureId(String);

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ProviderContractRevision(String);

macro_rules! compatibility_identifier_impl {
    ($type:ident, $label:literal) => {
        impl $type {
            pub fn new(value: impl Into<String>) -> Result<Self, CompatibilityIdentifierError> {
                let value = value.into();
                validate_compatibility_identifier($label, &value)?;
                Ok(Self(value))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $type {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(self.as_str())
            }
        }

        impl FromStr for $type {
            type Err = CompatibilityIdentifierError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::new(value)
            }
        }

        impl Serialize for $type {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                serializer.serialize_str(self.as_str())
            }
        }

        impl<'de> Deserialize<'de> for $type {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let value = String::deserialize(deserializer)?;
                Self::new(value).map_err(serde::de::Error::custom)
            }
        }
    };
}

compatibility_identifier_impl!(CapabilityId, "capability");
compatibility_identifier_impl!(OperatingSystemId, "operating system");
compatibility_identifier_impl!(ArchitectureId, "architecture");

impl ProviderContractRevision {
    pub fn new(value: impl Into<String>) -> Result<Self, CompatibilityIdentifierError> {
        let value = value.into();
        if value.is_empty() {
            return Err(CompatibilityIdentifierError::Empty {
                label: "provider contract revision",
            });
        }
        if value.len() > MAX_PROVIDER_CONTRACT_REVISION_BYTES {
            return Err(CompatibilityIdentifierError::TooLong {
                label: "provider contract revision",
                len: value.len(),
                max: MAX_PROVIDER_CONTRACT_REVISION_BYTES,
            });
        }
        if !value.bytes().all(|byte| byte.is_ascii_graphic()) {
            return Err(CompatibilityIdentifierError::InvalidCharacters {
                label: "provider contract revision",
                value,
            });
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ProviderContractRevision {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for ProviderContractRevision {
    type Err = CompatibilityIdentifierError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl Serialize for ProviderContractRevision {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ProviderContractRevision {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

fn validate_compatibility_identifier(
    label: &'static str,
    value: &str,
) -> Result<(), CompatibilityIdentifierError> {
    if value.is_empty() {
        return Err(CompatibilityIdentifierError::Empty { label });
    }
    if value.len() > MAX_COMPATIBILITY_IDENTIFIER_BYTES {
        return Err(CompatibilityIdentifierError::TooLong {
            label,
            len: value.len(),
            max: MAX_COMPATIBILITY_IDENTIFIER_BYTES,
        });
    }
    if !value.bytes().all(|byte| {
        byte.is_ascii_lowercase()
            || byte.is_ascii_digit()
            || matches!(byte, b'-' | b'_' | b'.')
    }) || !value.as_bytes().first().is_some_and(u8::is_ascii_alphanumeric)
        || !value.as_bytes().last().is_some_and(u8::is_ascii_alphanumeric)
    {
        return Err(CompatibilityIdentifierError::InvalidCharacters {
            label,
            value: value.to_owned(),
        });
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum CompatibilityIdentifierError {
    #[error("{label} identifier cannot be empty")]
    Empty { label: &'static str },
    #[error("{label} identifier length {len} exceeds the {max}-byte limit")]
    TooLong {
        label: &'static str,
        len: usize,
        max: usize,
    },
    #[error("{label} identifier must be bounded lowercase ASCII: {value}")]
    InvalidCharacters { label: &'static str, value: String },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum PathStyle {
    Windows,
    Posix,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum PathEncoding {
    Utf8,
    UnixBytes,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct OpaqueHostPath(OpaqueHostPathRepr);

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
enum OpaqueHostPathRepr {
    Utf8(String),
    UnixBytes(Vec<u8>),
}

impl OpaqueHostPath {
    pub fn utf8(value: String) -> Result<Self, OpaqueHostPathError> {
        validate_opaque_host_path(&value.as_bytes())?;
        Ok(Self(OpaqueHostPathRepr::Utf8(value)))
    }

    pub fn unix_bytes(value: Vec<u8>) -> Result<Self, OpaqueHostPathError> {
        validate_opaque_host_path(&value)?;
        Ok(Self(OpaqueHostPathRepr::UnixBytes(value)))
    }

    pub fn byte_len(&self) -> usize {
        match &self.0 {
            OpaqueHostPathRepr::Utf8(value) => value.len(),
            OpaqueHostPathRepr::UnixBytes(value) => value.len(),
        }
    }

    pub fn display_text(&self) -> Cow<'_, str> {
        match &self.0 {
            OpaqueHostPathRepr::Utf8(value) => Cow::Borrowed(value),
            OpaqueHostPathRepr::UnixBytes(value) => String::from_utf8_lossy(value),
        }
    }

    pub fn as_utf8(&self) -> Option<&str> {
        match &self.0 {
            OpaqueHostPathRepr::Utf8(value) => Some(value),
            OpaqueHostPathRepr::UnixBytes(_) => None,
        }
    }

    pub fn as_unix_bytes(&self) -> Option<&[u8]> {
        match &self.0 {
            OpaqueHostPathRepr::Utf8(_) => None,
            OpaqueHostPathRepr::UnixBytes(value) => Some(value),
        }
    }
}

fn validate_opaque_host_path(value: &[u8]) -> Result<(), OpaqueHostPathError> {
    if value.is_empty() {
        return Err(OpaqueHostPathError::Empty);
    }
    if value.len() > MAX_WORKSPACE_ROOT_BYTES {
        return Err(OpaqueHostPathError::TooLong {
            len: value.len(),
            max: MAX_WORKSPACE_ROOT_BYTES,
        });
    }
    if value.contains(&0) {
        return Err(OpaqueHostPathError::ContainsNul);
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum OpaqueHostPathError {
    #[error("host path cannot be empty")]
    Empty,
    #[error("host path length {len} exceeds the {max}-byte limit")]
    TooLong { len: usize, max: usize },
    #[error("host path cannot contain a NUL byte")]
    ContainsNul,
}

impl Serialize for OpaqueHostPath {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match &self.0 {
            OpaqueHostPathRepr::Utf8(value) => serializer.serialize_str(value),
            OpaqueHostPathRepr::UnixBytes(value) => {
                let mut state = serializer.serialize_struct("OpaqueHostPath", 2)?;
                state.serialize_field("kind", "unix-bytes")?;
                state.serialize_field("bytes", value)?;
                state.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for OpaqueHostPath {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(OpaqueHostPathVisitor)
    }
}

struct OpaqueHostPathVisitor;

impl<'de> Visitor<'de> for OpaqueHostPathVisitor {
    type Value = OpaqueHostPath;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a bounded UTF-8 path string or strict unix-bytes path object")
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        OpaqueHostPath::utf8(value.to_owned()).map_err(E::custom)
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        OpaqueHostPath::utf8(value).map_err(E::custom)
    }

    fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
    where
        M: MapAccess<'de>,
    {
        let mut kind = None;
        let mut bytes = None;
        while let Some(field) = map.next_key::<String>()? {
            match field.as_str() {
                "kind" => {
                    if kind.is_some() {
                        return Err(serde::de::Error::duplicate_field("kind"));
                    }
                    kind = Some(map.next_value::<String>()?);
                }
                "bytes" => {
                    if bytes.is_some() {
                        return Err(serde::de::Error::duplicate_field("bytes"));
                    }
                    bytes = Some(map.next_value::<BoundedOpaquePathBytes>()?.0);
                }
                _ => {
                    return Err(serde::de::Error::unknown_field(&field, &["kind", "bytes"]));
                }
            }
        }
        let kind = kind.ok_or_else(|| serde::de::Error::missing_field("kind"))?;
        if kind != "unix-bytes" {
            return Err(serde::de::Error::unknown_variant(&kind, &["unix-bytes"]));
        }
        let bytes = bytes.ok_or_else(|| serde::de::Error::missing_field("bytes"))?;
        OpaqueHostPath::unix_bytes(bytes).map_err(serde::de::Error::custom)
    }
}

struct BoundedOpaquePathBytes(Vec<u8>);

impl<'de> Deserialize<'de> for BoundedOpaquePathBytes {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_seq(BoundedOpaquePathBytesVisitor)
    }
}

struct BoundedOpaquePathBytesVisitor;

impl<'de> Visitor<'de> for BoundedOpaquePathBytesVisitor {
    type Value = BoundedOpaquePathBytes;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "at most {MAX_WORKSPACE_ROOT_BYTES} path bytes")
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut bytes = Vec::with_capacity(
            sequence.size_hint().unwrap_or(0).min(MAX_WORKSPACE_ROOT_BYTES),
        );
        while let Some(byte) = sequence.next_element::<u8>()? {
            if bytes.len() == MAX_WORKSPACE_ROOT_BYTES {
                return Err(serde::de::Error::invalid_length(
                    bytes.len() + 1,
                    &self,
                ));
            }
            bytes.push(byte);
        }
        Ok(BoundedOpaquePathBytes(bytes))
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PathSemantics {
    pub style: PathStyle,
    pub encoding: PathEncoding,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum LocalTransportKind {
    WindowsNamedPipe,
    UnixDomainSocket,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct HostDescriptor {
    pub operating_system: OperatingSystemId,
    pub architecture: ArchitectureId,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct StateSchemaSupport {
    pub versions: ProtocolRange,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProviderContractSupport {
    pub provider: AgentProvider,
    pub revision: ProviderContractRevision,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ClientCompatibilityOffer {
    pub protocol_versions: ProtocolRange,
    #[serde(default)]
    pub capabilities: Vec<CapabilityId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_schema: Option<StateSchemaSupport>,
}

impl ClientCompatibilityOffer {
    pub fn exact(protocol_version: u16) -> Result<Self, ProtocolNegotiationError> {
        Ok(Self {
            protocol_versions: ProtocolRange::exact(protocol_version)?,
            capabilities: Vec::new(),
            state_schema: None,
        })
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct NodeCompatibilitySupport {
    pub protocol_versions: ProtocolRange,
    #[serde(default)]
    pub capabilities: Vec<CapabilityId>,
    pub host: HostDescriptor,
    pub path_semantics: PathSemantics,
    pub local_transport: LocalTransportKind,
    pub state_schema: StateSchemaSupport,
    #[serde(default)]
    pub provider_contracts: Vec<ProviderContractSupport>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct NegotiatedNodeCompatibility {
    pub protocol_version: u16,
    #[serde(default)]
    pub capabilities: Vec<CapabilityId>,
    pub host: HostDescriptor,
    pub path_semantics: PathSemantics,
    pub local_transport: LocalTransportKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_schema_version: Option<u16>,
    #[serde(default)]
    pub provider_contracts: Vec<ProviderContractSupport>,
}

impl NodeCompatibilitySupport {
    pub fn negotiate(
        &self,
        active_protocol_version: u16,
        client: &ClientCompatibilityOffer,
    ) -> Result<NegotiatedNodeCompatibility, ProtocolNegotiationError> {
        if !self.protocol_versions.contains(active_protocol_version)
            || !client.protocol_versions.contains(active_protocol_version)
        {
            return Err(ProtocolNegotiationError::ActiveVersionUnsupported {
                active: active_protocol_version,
                local: self.protocol_versions,
                remote: client.protocol_versions,
            });
        }
        let capabilities = self
            .capabilities
            .iter()
            .filter(|capability| client.capabilities.contains(capability))
            .cloned()
            .collect();
        let state_schema_version = match client.state_schema {
            Some(client_state) => Some(
                self.state_schema
                    .versions
                    .highest_common(client_state.versions)?,
            ),
            None => None,
        };
        Ok(NegotiatedNodeCompatibility {
            protocol_version: active_protocol_version,
            capabilities,
            host: self.host.clone(),
            path_semantics: self.path_semantics.clone(),
            local_transport: self.local_transport,
            state_schema_version,
            provider_contracts: self.provider_contracts.clone(),
        })
    }
}

#[derive(Serialize)]
struct NodeCompatibilityAuthBinding<'a> {
    offer: &'a ClientCompatibilityOffer,
    selected: &'a NegotiatedNodeCompatibility,
}

pub fn encode_node_compatibility_auth_binding(
    offer: &ClientCompatibilityOffer,
    selected: &NegotiatedNodeCompatibility,
) -> Result<Vec<u8>, NodeCompatibilityAuthBindingError> {
    let encoded = serde_json::to_vec(&NodeCompatibilityAuthBinding { offer, selected })?;
    if encoded.len() > MAX_NODE_HELLO_FRAME_BYTES {
        return Err(NodeCompatibilityAuthBindingError::TooLarge {
            len: encoded.len(),
            max: MAX_NODE_HELLO_FRAME_BYTES,
        });
    }
    Ok(encoded)
}

#[derive(Debug, Error)]
pub enum NodeCompatibilityAuthBindingError {
    #[error("node compatibility authentication binding serialization failed: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("node compatibility authentication binding length {len} exceeds the {max}-byte limit")]
    TooLarge { len: usize, max: usize },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct SessionKey {
    pub instance_id: AgentInstanceId,
    pub generation: SessionGeneration,
}

#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct SessionAddress {
    pub workspace_id: WorkspaceId,
    pub session: SessionKey,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ManagedSessionState {
    IdentityPending,
    Live,
    Dormant,
    Unavailable,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ManagedSessionRecord {
    pub record_id: SessionRecordId,
    pub display_name: String,
    pub provider: AgentProvider,
    pub mode: SessionMode,
    pub state: ManagedSessionState,
    pub workspace_id: WorkspaceId,
    pub canonical_root: OpaqueHostPath,
    pub provider_session: Option<ProviderSessionIdentity>,
    pub active_session: Option<SessionAddress>,
    pub created_at_unix_ms: u64,
    pub updated_at_unix_ms: u64,
    pub last_error: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkspaceSnapshot {
    pub workspace_id: WorkspaceId,
    pub canonical_root: OpaqueHostPath,
    pub sessions: Vec<SessionSnapshot>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum WorkspaceEntryKind {
    File,
    Directory,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkspaceEntry {
    pub relative_path: String,
    pub kind: WorkspaceEntryKind,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct GitStatusEntry {
    pub index_status: String,
    pub worktree_status: String,
    pub path: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct GitCommitSummary {
    pub id: String,
    pub summary: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct GitWorktreeSnapshot {
    pub path: OpaqueHostPath,
    pub head: String,
    pub branch: Option<String>,
    pub is_bare: bool,
    pub is_main: bool,
    pub locked: bool,
    pub lock_reason: Option<String>,
    pub prunable: bool,
    pub prunable_reason: Option<String>,
    pub workspace_id: Option<WorkspaceId>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct GitSnapshot {
    pub is_repository: bool,
    pub branch: Option<String>,
    pub status: Vec<GitStatusEntry>,
    pub recent_commits: Vec<GitCommitSummary>,
    #[serde(default)]
    pub worktrees: Vec<GitWorktreeSnapshot>,
    pub truncated: bool,
    pub diagnostic: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkspaceInspection {
    pub workspace_id: WorkspaceId,
    pub entries: Vec<WorkspaceEntry>,
    pub tree_truncated: bool,
    pub git: GitSnapshot,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct NodeSnapshot {
    pub node_id: NodeId,
    pub enabled_providers: Vec<AgentProvider>,
    pub workspaces: Vec<WorkspaceSnapshot>,
    #[serde(default)]
    pub session_records: Vec<ManagedSessionRecord>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ClientHello {
    pub protocol_version: u16,
    pub role: ClientRole,
    pub client_nonce: [u8; NODE_AUTH_NONCE_BYTES],
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compatibility: Option<ClientCompatibilityOffer>,
}

impl ClientHello {
    pub fn new(role: ClientRole, client_nonce: [u8; NODE_AUTH_NONCE_BYTES]) -> Self {
        Self {
            protocol_version: NODE_PROTOCOL_VERSION,
            role,
            client_nonce,
            compatibility: None,
        }
    }

    pub fn negotiating(
        role: ClientRole,
        client_nonce: [u8; NODE_AUTH_NONCE_BYTES],
        compatibility: ClientCompatibilityOffer,
    ) -> Self {
        Self {
            protocol_version: NODE_PROTOCOL_VERSION,
            role,
            client_nonce,
            compatibility: Some(compatibility),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ServerChallenge {
    pub protocol_version: u16,
    pub server_nonce: [u8; NODE_AUTH_NONCE_BYTES],
    pub server_proof: [u8; NODE_AUTH_PROOF_BYTES],
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compatibility: Option<NegotiatedNodeCompatibility>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ClientAuthentication {
    pub client_proof: [u8; NODE_AUTH_PROOF_BYTES],
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ControllerState {
    pub connection_id: u64,
    pub lease_remaining_ms: u64,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct NodeCursor {
    pub incarnation_id: NodeIncarnationId,
    pub sequence: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct NodeHello {
    pub protocol_version: u16,
    pub incarnation_id: NodeIncarnationId,
    pub connection_id: u64,
    pub role: ClientRole,
    pub event_sequence: u64,
    pub controller: Option<ControllerState>,
    pub snapshot: NodeSnapshot,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compatibility: Option<NegotiatedNodeCompatibility>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RequestEnvelope {
    pub request_id: u64,
    pub request: NodeRequest,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum NodeRequest {
    Snapshot,
    Resync { after_sequence: u64 },
    InspectWorkspace { workspace_id: WorkspaceId },
    AcquireController { lease_ms: u64 },
    ReleaseController,
    RegisterWorkspace {
        workspace_id: WorkspaceId,
        root: OpaqueHostPath,
    },
    UnregisterWorkspace {
        workspace_id: WorkspaceId,
    },
    CreateWorktree {
        source_workspace_id: WorkspaceId,
        workspace_id: WorkspaceId,
        target_root: OpaqueHostPath,
        branch: String,
        base: Option<String>,
    },
    RemoveWorktree {
        source_workspace_id: WorkspaceId,
        target_root: OpaqueHostPath,
    },
    Spawn {
        workspace_id: WorkspaceId,
        provider: AgentProvider,
        mode: SessionMode,
        terminal_size: TerminalSize,
        initial_prompt: Option<String>,
    },
    Resume {
        session: SessionAddress,
        terminal_size: TerminalSize,
        initial_prompt: Option<String>,
    },
    RenameSessionRecord {
        record_id: SessionRecordId,
        display_name: String,
    },
    ResumeSessionRecord {
        record_id: SessionRecordId,
        terminal_size: TerminalSize,
        initial_prompt: Option<String>,
    },
    ForgetSessionRecord {
        record_id: SessionRecordId,
    },
    Prompt { session: SessionAddress, text: String },
    Paste { session: SessionAddress, text: String },
    Input { session: SessionAddress, text: String },
    TerminalBytes { session: SessionAddress, bytes: Vec<u8> },
    TerminalControl { session: SessionAddress, control: TerminalControl },
    Resize { session: SessionAddress, size: TerminalSize },
    Interrupt { session: SessionAddress },
    Stop { session: SessionAddress, force: bool },
    Remove { session: SessionAddress },
    Shutdown,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ResponseEnvelope {
    pub request_id: u64,
    pub result: Result<NodeResponse, NodeFailure>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum NodeResponse {
    Snapshot {
        event_sequence: u64,
        controller: Option<ControllerState>,
        snapshot: NodeSnapshot,
    },
    Resync {
        event_sequence: u64,
        snapshot: NodeSnapshot,
        events: Vec<NodeEventEnvelope>,
    },
    WorkspaceInspected {
        inspection: WorkspaceInspection,
    },
    Controller {
        controller: Option<ControllerState>,
    },
    SpawnAccepted {
        session: SessionAddress,
    },
    SessionRecordUpdated {
        record: ManagedSessionRecord,
    },
    SessionRecordResumed {
        record: ManagedSessionRecord,
        session: SessionAddress,
    },
    SessionRecordForgotten {
        record_id: SessionRecordId,
    },
    WorkspaceRegistered {
        workspace: WorkspaceSnapshot,
    },
    WorkspaceUnregistered {
        workspace_id: WorkspaceId,
    },
    WorktreeCreated {
        worktree: GitWorktreeSnapshot,
        workspace: WorkspaceSnapshot,
    },
    WorktreeRemoved {
        target_root: OpaqueHostPath,
        workspace_id: Option<WorkspaceId>,
    },
    Accepted,
    ShuttingDown,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct NodeFailure {
    pub code: NodeFailureCode,
    pub message: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum NodeFailureCode {
    InvalidRequest,
    Unauthorized,
    ObserverReadOnly,
    ControllerBusy,
    ControllerRequired,
    UnknownWorkspace,
    InvalidWorkspaceRoot,
    DuplicateWorkspaceId,
    DuplicateWorkspaceRoot,
    WorkspaceBusy,
    LastWorkspace,
    NotGitRepository,
    WorktreeConflict,
    WorktreeProtected,
    WorktreeDirty,
    WorktreeLocked,
    UnknownSession,
    UnknownSessionRecord,
    SessionRecordNotResumable,
    SessionRecordBusy,
    SessionRecordConflict,
    SessionWorkspaceMismatch,
    StaleGeneration,
    BackendBusy,
    BackendDisconnected,
    BackendOperationFailed,
    ShuttingDown,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct NodeEventEnvelope {
    pub sequence: u64,
    pub event: NodeEvent,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum NodeEvent {
    Control { address: SessionAddress, event: ControlEvent },
    ControllerChanged { controller: Option<ControllerState> },
    WorkspaceAdded { workspace: WorkspaceSnapshot },
    WorkspaceRemoved { workspace_id: WorkspaceId },
    SessionRecordUpserted { record: ManagedSessionRecord },
    SessionRecordRemoved { record_id: SessionRecordId },
    ResyncRequired { oldest_available_sequence: u64 },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", content = "payload", rename_all = "kebab-case")]
pub enum ClientFrame {
    Hello(ClientHello),
    Authenticate(ClientAuthentication),
    Request(RequestEnvelope),
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", content = "payload", rename_all = "kebab-case")]
pub enum ServerFrame {
    Challenge(ServerChallenge),
    Hello(NodeHello),
    Reply(ResponseEnvelope),
    Event(NodeEventEnvelope),
}

pub async fn read_json_frame<R, T>(reader: &mut R) -> Result<T, FrameError>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    read_json_frame_limited(reader, MAX_NODE_FRAME_BYTES).await
}

pub async fn read_json_frame_limited<R, T>(reader: &mut R, max_bytes: usize) -> Result<T, FrameError>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    let length = reader.read_u32_le().await? as usize;
    if length == 0 || length > max_bytes {
        return Err(FrameError::InvalidLength {
            length,
            max: max_bytes,
        });
    }
    let mut payload = vec![0; length];
    reader.read_exact(&mut payload).await?;
    Ok(serde_json::from_slice(&payload)?)
}

pub async fn read_json_frame_limited_body_timeout<R, T>(
    reader: &mut R,
    max_bytes: usize,
    body_timeout: Duration,
) -> Result<T, FrameError>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    let mut length_prefix = [0_u8; std::mem::size_of::<u32>()];
    reader.read_exact(&mut length_prefix[..1]).await?;
    timeout(body_timeout, reader.read_exact(&mut length_prefix[1..]))
        .await
        .map_err(|_| FrameError::PrefixTimedOut)??;
    let length = u32::from_le_bytes(length_prefix) as usize;
    if length == 0 || length > max_bytes {
        return Err(FrameError::InvalidLength {
            length,
            max: max_bytes,
        });
    }
    let mut payload = vec![0; length];
    timeout(body_timeout, reader.read_exact(&mut payload))
        .await
        .map_err(|_| FrameError::BodyTimedOut { length })??;
    Ok(serde_json::from_slice(&payload)?)
}

pub async fn write_json_frame<W, T>(writer: &mut W, value: &T) -> Result<(), FrameError>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    write_json_frame_limited(writer, value, MAX_NODE_FRAME_BYTES).await
}

pub async fn write_json_frame_limited<W, T>(
    writer: &mut W,
    value: &T,
    max_bytes: usize,
) -> Result<(), FrameError>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let payload = serde_json::to_vec(value)?;
    if payload.is_empty() || payload.len() > max_bytes {
        return Err(FrameError::InvalidLength {
            length: payload.len(),
            max: max_bytes,
        });
    }
    writer.write_u32_le(payload.len() as u32).await?;
    writer.write_all(&payload).await?;
    writer.flush().await?;
    Ok(())
}

#[derive(Debug, Error)]
pub enum FrameError {
    #[error("node frame I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("node frame JSON failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("node frame length {length} is outside 1..={max}")]
    InvalidLength { length: usize, max: usize },
    #[error("node frame body of {length} bytes was not received before the bounded deadline")]
    BodyTimedOut { length: usize },
    #[error("node frame length prefix was not completed before the bounded deadline")]
    PrefixTimedOut,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host_path(value: &str) -> OpaqueHostPath {
        OpaqueHostPath::utf8(value.to_owned()).unwrap()
    }

    fn portable_node_support() -> NodeCompatibilitySupport {
        NodeCompatibilitySupport {
            protocol_versions: ProtocolRange::new(7, 9).unwrap(),
            capabilities: vec![
                CapabilityId::new("workspace.inspect").unwrap(),
                CapabilityId::new("session.spawn").unwrap(),
            ],
            host: HostDescriptor {
                operating_system: OperatingSystemId::new("windows").unwrap(),
                architecture: ArchitectureId::new("x86_64").unwrap(),
            },
            path_semantics: PathSemantics {
                style: PathStyle::Windows,
                encoding: PathEncoding::Utf8,
            },
            local_transport: LocalTransportKind::WindowsNamedPipe,
            state_schema: StateSchemaSupport {
                versions: ProtocolRange::new(3, 5).unwrap(),
            },
            provider_contracts: vec![ProviderContractSupport {
                provider: AgentProvider::Codex,
                revision: ProviderContractRevision::new("codex.2026-08").unwrap(),
            }],
        }
    }

    #[test]
    fn legacy_hello_json_remains_exactly_protocol_v8() {
        let client = ClientHello::new(ClientRole::Observer, [0; NODE_AUTH_NONCE_BYTES]);
        assert_eq!(
            serde_json::to_string(&client).unwrap(),
            r#"{"protocol_version":8,"role":"observer","client_nonce":[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0]}"#,
        );

        let challenge = ServerChallenge {
            protocol_version: NODE_PROTOCOL_VERSION,
            server_nonce: [0; NODE_AUTH_NONCE_BYTES],
            server_proof: [0; NODE_AUTH_PROOF_BYTES],
            compatibility: None,
        };
        let json = serde_json::to_string(&challenge).unwrap();
        assert!(!json.contains("compatibility"));
        assert_eq!(serde_json::from_str::<ServerChallenge>(&json).unwrap(), challenge);
    }

    #[test]
    fn legacy_hello_omits_compatibility_instead_of_synthesizing_a_selection() {
        let hello = ClientHello::new(ClientRole::Observer, [0; NODE_AUTH_NONCE_BYTES]);
        assert_eq!(hello.compatibility, None);
    }

    #[test]
    fn compatibility_negotiation_keeps_the_active_wire_and_selects_highest_state_schema() {
        let offer = ClientCompatibilityOffer {
            protocol_versions: ProtocolRange::new(8, 10).unwrap(),
            capabilities: vec![
                CapabilityId::new("session.spawn").unwrap(),
                CapabilityId::new("unknown.future").unwrap(),
            ],
            state_schema: Some(StateSchemaSupport {
                versions: ProtocolRange::new(4, 6).unwrap(),
            }),
        };
        let hello = ClientHello::negotiating(
            ClientRole::Operator,
            [1; NODE_AUTH_NONCE_BYTES],
            offer.clone(),
        );
        assert_eq!(hello.compatibility, Some(offer.clone()));

        let negotiated = portable_node_support()
            .negotiate(NODE_PROTOCOL_VERSION, &offer)
            .unwrap();
        assert_eq!(negotiated.protocol_version, NODE_PROTOCOL_VERSION);
        assert_eq!(negotiated.state_schema_version, Some(5));
        assert_eq!(
            negotiated.capabilities,
            vec![CapabilityId::new("session.spawn").unwrap()],
        );
    }

    #[test]
    fn compatibility_negotiation_rejects_an_active_wire_outside_either_range() {
        let offer = ClientCompatibilityOffer {
            protocol_versions: ProtocolRange::new(9, 10).unwrap(),
            capabilities: Vec::new(),
            state_schema: None,
        };
        let error = portable_node_support()
            .negotiate(NODE_PROTOCOL_VERSION, &offer)
            .unwrap_err();
        assert!(matches!(
            error,
            ProtocolNegotiationError::ActiveVersionUnsupported {
                active: NODE_PROTOCOL_VERSION,
                ..
            },
        ));
    }

    #[test]
    fn compatibility_auth_binding_has_an_exact_bounded_encoding() {
        let offer = ClientCompatibilityOffer {
            protocol_versions: ProtocolRange::new(8, 10).unwrap(),
            capabilities: vec![CapabilityId::new("session.spawn").unwrap()],
            state_schema: Some(StateSchemaSupport {
                versions: ProtocolRange::new(4, 6).unwrap(),
            }),
        };
        let selected = portable_node_support()
            .negotiate(NODE_PROTOCOL_VERSION, &offer)
            .unwrap();
        let encoded = encode_node_compatibility_auth_binding(&offer, &selected).unwrap();
        assert_eq!(
            String::from_utf8(encoded).unwrap(),
            r#"{"offer":{"protocol_versions":{"minimum":8,"maximum":10},"capabilities":["session.spawn"],"state_schema":{"versions":{"minimum":4,"maximum":6}}},"selected":{"protocol_version":8,"capabilities":["session.spawn"],"host":{"operating_system":"windows","architecture":"x86_64"},"path_semantics":{"style":"windows","encoding":"utf8"},"local_transport":"windows-named-pipe","state_schema_version":5,"provider_contracts":[{"provider":"codex","revision":"codex.2026-08"}]}}"#,
        );
    }

    #[test]
    fn protocol_ranges_reject_invalid_and_disjoint_inputs() {
        assert!(matches!(
            ProtocolRange::new(0, 8),
            Err(ProtocolNegotiationError::InvalidRange { minimum: 0, maximum: 8 }),
        ));
        assert!(matches!(
            ProtocolRange::new(9, 8),
            Err(ProtocolNegotiationError::InvalidRange { minimum: 9, maximum: 8 }),
        ));
        let error = ProtocolRange::new(7, 8)
            .unwrap()
            .highest_common(ProtocolRange::new(9, 10).unwrap())
            .unwrap_err();
        assert!(matches!(error, ProtocolNegotiationError::Disjoint { .. }));
        assert!(serde_json::from_str::<ProtocolRange>(
            r#"{"minimum":10,"maximum":9}"#,
        )
        .is_err());
        assert!(ProviderContractRevision::new(
            "gate4agent-inline/codex-cli-0.144/v1",
        )
        .is_ok());
        assert!(ProviderContractRevision::new(
            "orca:d8629c41c832436463d5f0b4e4deb95f867fdc42",
        )
        .is_ok());
    }

    #[test]
    fn foreign_path_metadata_round_trips_without_normalization() {
        #[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
        struct ForeignPath {
            raw: String,
            semantics: PathSemantics,
        }

        let foreign = ForeignPath {
            raw: r"C:\Users\operator\repo\src\lib.rs".to_owned(),
            semantics: portable_node_support().path_semantics,
        };
        let json = serde_json::to_string(&foreign).unwrap();
        assert!(json.contains(r#""raw":"C:\\Users\\operator\\repo\\src\\lib.rs""#));
        assert_eq!(serde_json::from_str::<ForeignPath>(&json).unwrap(), foreign);
    }

    #[test]
    fn opaque_host_path_utf8_preserves_legacy_json_string_shape() {
        let path = host_path(r"C:\Users\operator\repo");
        assert_eq!(path.byte_len(), 22);
        assert_eq!(path.as_utf8(), Some(r"C:\Users\operator\repo"));
        assert_eq!(path.as_unix_bytes(), None);
        assert_eq!(path.display_text(), r"C:\Users\operator\repo");

        let json = serde_json::to_string(&path).unwrap();
        assert_eq!(json, r#""C:\\Users\\operator\\repo""#);
        assert_eq!(serde_json::from_str::<OpaqueHostPath>(&json).unwrap(), path);
    }

    #[test]
    fn opaque_host_path_unix_bytes_has_strict_bounded_tagged_wire_shape() {
        let raw = vec![b'/', b'r', b'e', b'p', b'o', b'/', 0xff];
        let path = OpaqueHostPath::unix_bytes(raw.clone()).unwrap();
        assert_eq!(path.byte_len(), raw.len());
        assert_eq!(path.as_utf8(), None);
        assert_eq!(path.as_unix_bytes(), Some(raw.as_slice()));

        let json = serde_json::to_string(&path).unwrap();
        assert_eq!(
            json,
            r#"{"kind":"unix-bytes","bytes":[47,114,101,112,111,47,255]}"#,
        );
        assert_eq!(serde_json::from_str::<OpaqueHostPath>(&json).unwrap(), path);

        for invalid in [
            r#"{"kind":"future","bytes":[47]}"#,
            r#"{"kind":"unix-bytes","bytes":[47],"extra":true}"#,
            r#"{"kind":"unix-bytes"}"#,
            r#"{"bytes":[47]}"#,
            r#"{"kind":"unix-bytes","bytes":[]}"#,
            r#"{"kind":"unix-bytes","bytes":[47,0]}"#,
        ] {
            assert!(serde_json::from_str::<OpaqueHostPath>(invalid).is_err(), "{invalid}");
        }
        assert!(OpaqueHostPath::utf8(String::new()).is_err());
        assert!(OpaqueHostPath::utf8("a\0b".to_owned()).is_err());
        assert!(OpaqueHostPath::utf8("x".repeat(MAX_WORKSPACE_ROOT_BYTES + 1)).is_err());
        assert!(OpaqueHostPath::unix_bytes(vec![b'x'; MAX_WORKSPACE_ROOT_BYTES + 1]).is_err());
    }

    #[tokio::test]
    async fn json_frame_round_trips_a_client_hello_without_the_access_token() {
        let expected = ClientFrame::Hello(ClientHello::new(ClientRole::Operator, [7; NODE_AUTH_NONCE_BYTES]));
        let mut wire = Vec::new();
        write_json_frame(&mut wire, &expected).await.unwrap();

        assert!(!String::from_utf8_lossy(&wire).contains("local-token"));

        let actual: ClientFrame = read_json_frame(&mut wire.as_slice()).await.unwrap();
        assert_eq!(actual, expected);
    }

    #[tokio::test]
    async fn reader_rejects_zero_length_before_allocating() {
        let bytes = 0_u32.to_le_bytes();
        let mut wire = bytes.as_slice();
        let error = read_json_frame::<_, ClientFrame>(&mut wire).await.unwrap_err();
        assert!(matches!(
            error,
            FrameError::InvalidLength { length: 0, max: MAX_NODE_FRAME_BYTES }
        ));
    }

    #[tokio::test]
    async fn hello_reader_rejects_an_oversized_frame_before_allocating() {
        let declared = (MAX_NODE_HELLO_FRAME_BYTES + 1) as u32;
        let bytes = declared.to_le_bytes();
        let mut wire = bytes.as_slice();
        let error = read_json_frame_limited::<_, ClientFrame>(
            &mut wire,
            MAX_NODE_HELLO_FRAME_BYTES,
        )
        .await
        .unwrap_err();
        assert!(matches!(
            error,
            FrameError::InvalidLength {
                length,
                max: MAX_NODE_HELLO_FRAME_BYTES
            } if length == MAX_NODE_HELLO_FRAME_BYTES + 1
        ));
    }

    #[tokio::test]
    async fn client_writer_enforces_the_smaller_request_limit() {
        let oversized = ClientFrame::Request(RequestEnvelope {
            request_id: 1,
            request: NodeRequest::Input {
                session: SessionAddress {
                    workspace_id: WorkspaceId::new("primary").unwrap(),
                    session: SessionKey {
                        instance_id: AgentInstanceId(1),
                        generation: SessionGeneration(1),
                    },
                },
                text: "x".repeat(MAX_NODE_CLIENT_FRAME_BYTES),
            },
        });
        let mut wire = Vec::new();
        let error = write_json_frame_limited(
            &mut wire,
            &oversized,
            MAX_NODE_CLIENT_FRAME_BYTES,
        )
        .await
        .unwrap_err();
        assert!(matches!(
            error,
            FrameError::InvalidLength {
                length,
                max: MAX_NODE_CLIENT_FRAME_BYTES
            } if length > MAX_NODE_CLIENT_FRAME_BYTES
        ));
        assert!(wire.is_empty());
    }

    #[tokio::test]
    async fn maximum_node_text_fits_the_client_frame_under_worst_case_json_escaping() {
        let frame = ClientFrame::Request(RequestEnvelope {
            request_id: 2,
            request: NodeRequest::Paste {
                session: SessionAddress {
                    workspace_id: WorkspaceId::new("primary").unwrap(),
                    session: SessionKey {
                        instance_id: AgentInstanceId(1),
                        generation: SessionGeneration(1),
                    },
                },
                text: "\0".repeat(MAX_NODE_TEXT_BYTES),
            },
        });
        let mut wire = Vec::new();
        write_json_frame_limited(&mut wire, &frame, MAX_NODE_CLIENT_FRAME_BYTES)
            .await
            .unwrap();
        assert!(wire.len() <= MAX_NODE_CLIENT_FRAME_BYTES + std::mem::size_of::<u32>());
    }

    #[tokio::test]
    async fn body_timeout_starts_after_the_bounded_length_prefix() {
        let (mut writer, mut reader) = tokio::io::duplex(64);
        writer.write_u32_le(32).await.unwrap();
        writer.write_all(b"{").await.unwrap();
        let error = read_json_frame_limited_body_timeout::<_, ClientFrame>(
            &mut reader,
            MAX_NODE_CLIENT_FRAME_BYTES,
            Duration::from_millis(10),
        )
        .await
        .unwrap_err();
        assert!(matches!(error, FrameError::BodyTimedOut { length: 32 }));
    }

    #[tokio::test]
    async fn partial_length_prefix_cannot_pin_a_connection_slot() {
        let (mut writer, mut reader) = tokio::io::duplex(64);
        writer.write_all(&[32]).await.unwrap();
        let error = read_json_frame_limited_body_timeout::<_, ClientFrame>(
            &mut reader,
            MAX_NODE_CLIENT_FRAME_BYTES,
            Duration::from_millis(10),
        )
        .await
        .unwrap_err();
        assert!(matches!(error, FrameError::PrefixTimedOut));
    }

    #[test]
    fn resume_wire_does_not_accept_a_replacement_working_directory() {
        let frame = ClientFrame::Request(RequestEnvelope {
            request_id: 9,
            request: NodeRequest::Resume {
                session: SessionAddress {
                    workspace_id: WorkspaceId::new("primary").unwrap(),
                    session: SessionKey {
                        instance_id: AgentInstanceId(3),
                        generation: SessionGeneration(2),
                    },
                },
                terminal_size: TerminalSize { rows: 24, columns: 80 },
                initial_prompt: Some("continue".to_owned()),
            },
        });
        let json = serde_json::to_string(&frame).unwrap();
        assert!(!json.contains("working_directory"));

        let mut malicious = serde_json::to_value(match frame {
            ClientFrame::Request(envelope) => envelope.request,
            _ => unreachable!("constructed request frame"),
        })
        .unwrap();
        malicious
            .as_object_mut()
            .unwrap()
            .insert(
                "working_directory".to_owned(),
                serde_json::Value::String(r"C:\attacker-selected-root".to_owned()),
            );
        let error = serde_json::from_value::<NodeRequest>(malicious).unwrap_err();
        assert!(error.to_string().contains("unknown field `working_directory`"));
    }

    #[test]
    fn protocol_v8_workspace_and_worktree_mutations_have_exact_bounded_wire_shapes() {
        assert_eq!(NODE_PROTOCOL_VERSION, 8);
        assert_eq!(MAX_WORKSPACE_ROOT_BYTES, gate4agent_types::WORKING_DIRECTORY_MAX_BYTES);

        let register = NodeRequest::RegisterWorkspace {
            workspace_id: WorkspaceId::new("repo-2").unwrap(),
            root: host_path(r"C:\repo-2"),
        };
        let register_json = serde_json::to_string(&register).unwrap();
        assert_eq!(
            register_json,
            r#"{"kind":"register-workspace","workspace_id":"repo-2","root":"C:\\repo-2"}"#,
        );
        assert_eq!(serde_json::from_str::<NodeRequest>(&register_json).unwrap(), register);

        let unregister = NodeRequest::UnregisterWorkspace {
            workspace_id: WorkspaceId::new("repo-2").unwrap(),
        };
        let unregister_json = serde_json::to_string(&unregister).unwrap();
        assert_eq!(
            unregister_json,
            r#"{"kind":"unregister-workspace","workspace_id":"repo-2"}"#,
        );
        assert_eq!(serde_json::from_str::<NodeRequest>(&unregister_json).unwrap(), unregister);

        let create = NodeRequest::CreateWorktree {
            source_workspace_id: WorkspaceId::new("primary").unwrap(),
            workspace_id: WorkspaceId::new("topic-one").unwrap(),
            target_root: host_path(r"C:\trees\topic-one"),
            branch: "codex/topic-one".to_owned(),
            base: Some("main".to_owned()),
        };
        let create_json = serde_json::to_string(&create).unwrap();
        assert_eq!(
            create_json,
            r#"{"kind":"create-worktree","source_workspace_id":"primary","workspace_id":"topic-one","target_root":"C:\\trees\\topic-one","branch":"codex/topic-one","base":"main"}"#,
        );
        assert_eq!(serde_json::from_str::<NodeRequest>(&create_json).unwrap(), create);

        let remove = NodeRequest::RemoveWorktree {
            source_workspace_id: WorkspaceId::new("primary").unwrap(),
            target_root: host_path(r"C:\trees\topic-one"),
        };
        let remove_json = serde_json::to_string(&remove).unwrap();
        assert_eq!(
            remove_json,
            r#"{"kind":"remove-worktree","source_workspace_id":"primary","target_root":"C:\\trees\\topic-one"}"#,
        );
        assert_eq!(serde_json::from_str::<NodeRequest>(&remove_json).unwrap(), remove);
    }

    #[test]
    fn incarnation_id_and_cursor_have_exact_lowercase_hex_wire_shapes() {
        let incarnation_id = NodeIncarnationId::from_bytes([
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77,
            0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff,
        ]);
        assert_eq!(
            incarnation_id.to_string(),
            "00112233445566778899aabbccddeeff",
        );
        let json = serde_json::to_string(&incarnation_id).unwrap();
        assert_eq!(json, r#""00112233445566778899aabbccddeeff""#);
        assert_eq!(
            serde_json::from_str::<NodeIncarnationId>(&json).unwrap(),
            incarnation_id,
        );
        assert!("00112233445566778899AABBCCDDEEFF"
            .parse::<NodeIncarnationId>()
            .is_err());
        assert!("00112233445566778899aabbccddeef"
            .parse::<NodeIncarnationId>()
            .is_err());
        assert!("00112233445566778899aabbccddeefg"
            .parse::<NodeIncarnationId>()
            .is_err());

        let cursor = NodeCursor {
            incarnation_id,
            sequence: 17,
        };
        let cursor_json = serde_json::to_string(&cursor).unwrap();
        assert_eq!(
            cursor_json,
            r#"{"incarnation_id":"00112233445566778899aabbccddeeff","sequence":17}"#,
        );
        assert_eq!(serde_json::from_str::<NodeCursor>(&cursor_json).unwrap(), cursor);
    }

    #[test]
    fn node_hello_v8_carries_the_incarnation_sequence_domain() {
        let hello = NodeHello {
            protocol_version: NODE_PROTOCOL_VERSION,
            incarnation_id: NodeIncarnationId::from_bytes([0; NODE_INCARNATION_ID_BYTES]),
            connection_id: 42,
            role: ClientRole::Observer,
            event_sequence: 9,
            controller: None,
            snapshot: NodeSnapshot {
                node_id: NodeId::new("fixture-node").unwrap(),
                enabled_providers: Vec::new(),
                workspaces: Vec::new(),
                session_records: Vec::new(),
            },
            compatibility: None,
        };
        let json = serde_json::to_string(&hello).unwrap();
        assert_eq!(
            json,
            r#"{"protocol_version":8,"incarnation_id":"00000000000000000000000000000000","connection_id":42,"role":"observer","event_sequence":9,"controller":null,"snapshot":{"node_id":"fixture-node","enabled_providers":[],"workspaces":[],"session_records":[]}}"#,
        );
        assert_eq!(serde_json::from_str::<NodeHello>(&json).unwrap(), hello);
    }

    #[test]
    fn terminal_bytes_round_trip_as_an_exact_byte_array() {
        let request = NodeRequest::TerminalBytes {
            session: SessionAddress {
                workspace_id: WorkspaceId::new("primary").unwrap(),
                session: SessionKey {
                    instance_id: AgentInstanceId(7),
                    generation: SessionGeneration(3),
                },
            },
            bytes: b"\x1b[1;5D".to_vec(),
        };
        let json = serde_json::to_string(&request).unwrap();
        assert_eq!(
            json,
            r#"{"kind":"terminal-bytes","session":{"workspace_id":"primary","session":{"instance_id":7,"generation":3}},"bytes":[27,91,49,59,53,68]}"#,
        );
        assert_eq!(serde_json::from_str::<NodeRequest>(&json).unwrap(), request);
    }

    #[test]
    fn workspace_responses_and_events_round_trip_without_client_only_state() {
        let workspace = WorkspaceSnapshot {
            workspace_id: WorkspaceId::new("repo-2").unwrap(),
            canonical_root: host_path(r"C:\repo-2"),
            sessions: Vec::new(),
        };
        let registered = NodeResponse::WorkspaceRegistered {
            workspace: workspace.clone(),
        };
        let registered_json = serde_json::to_string(&registered).unwrap();
        assert_eq!(
            serde_json::from_str::<NodeResponse>(&registered_json).unwrap(),
            registered,
        );
        let added = NodeEventEnvelope {
            sequence: 19,
            event: NodeEvent::WorkspaceAdded {
                workspace: workspace.clone(),
            },
        };
        let added_json = serde_json::to_string(&added).unwrap();
        assert_eq!(
            serde_json::from_str::<NodeEventEnvelope>(&added_json).unwrap(),
            added,
        );
        let removed = NodeEventEnvelope {
            sequence: 20,
            event: NodeEvent::WorkspaceRemoved {
                workspace_id: workspace.workspace_id.clone(),
            },
        };
        let removed_json = serde_json::to_string(&removed).unwrap();
        assert_eq!(
            serde_json::from_str::<NodeEventEnvelope>(&removed_json).unwrap(),
            removed,
        );

        let created = NodeResponse::WorktreeCreated {
            worktree: GitWorktreeSnapshot {
                path: host_path(r"C:\trees\topic-one"),
                head: "abc1234".to_owned(),
                branch: Some("codex/topic-one".to_owned()),
                is_bare: false,
                is_main: false,
                locked: false,
                lock_reason: None,
                prunable: false,
                prunable_reason: None,
                workspace_id: Some(workspace.workspace_id.clone()),
            },
            workspace: workspace.clone(),
        };
        let created_json = serde_json::to_string(&created).unwrap();
        assert_eq!(serde_json::from_str::<NodeResponse>(&created_json).unwrap(), created);
        let removed = NodeResponse::WorktreeRemoved {
            target_root: host_path(r"C:\trees\topic-one"),
            workspace_id: Some(workspace.workspace_id),
        };
        let removed_json = serde_json::to_string(&removed).unwrap();
        assert_eq!(serde_json::from_str::<NodeResponse>(&removed_json).unwrap(), removed);
    }

    #[test]
    fn workspace_inspection_round_trips_as_a_workspace_scoped_read_only_request() {
        let workspace_id = WorkspaceId::new("primary").unwrap();
        let request = NodeRequest::InspectWorkspace {
            workspace_id: workspace_id.clone(),
        };
        let request_json = serde_json::to_string(&request).unwrap();
        assert_eq!(
            request_json,
            r#"{"kind":"inspect-workspace","workspace_id":"primary"}"#,
        );
        assert_eq!(serde_json::from_str::<NodeRequest>(&request_json).unwrap(), request);

        let response = NodeResponse::WorkspaceInspected {
            inspection: WorkspaceInspection {
                workspace_id,
                entries: vec![
                    WorkspaceEntry {
                        relative_path: "src".to_owned(),
                        kind: WorkspaceEntryKind::Directory,
                    },
                    WorkspaceEntry {
                        relative_path: "src/lib.rs".to_owned(),
                        kind: WorkspaceEntryKind::File,
                    },
                ],
                tree_truncated: false,
                git: GitSnapshot {
                    is_repository: true,
                    branch: Some("main".to_owned()),
                    status: vec![GitStatusEntry {
                        index_status: " ".to_owned(),
                        worktree_status: "M".to_owned(),
                        path: "src/lib.rs".to_owned(),
                    }],
                    recent_commits: vec![GitCommitSummary {
                        id: "abc1234".to_owned(),
                        summary: "bounded summary".to_owned(),
                    }],
                    worktrees: vec![GitWorktreeSnapshot {
                        path: host_path(r"C:\repo"),
                        head: "abc1234".to_owned(),
                        branch: Some("main".to_owned()),
                        is_bare: false,
                        is_main: true,
                        locked: false,
                        lock_reason: None,
                        prunable: false,
                        prunable_reason: None,
                        workspace_id: Some(WorkspaceId::new("primary").unwrap()),
                    }],
                    truncated: false,
                    diagnostic: None,
                },
            },
        };
        let response_json = serde_json::to_string(&response).unwrap();
        assert_eq!(serde_json::from_str::<NodeResponse>(&response_json).unwrap(), response);
    }

    #[test]
    fn git_snapshot_defaults_worktrees_for_legacy_inspection_payloads() {
        let json = r#"{"is_repository":true,"branch":"main","status":[],"recent_commits":[],"truncated":false,"diagnostic":null}"#;
        let snapshot = serde_json::from_str::<GitSnapshot>(json).unwrap();
        assert!(snapshot.worktrees.is_empty());
    }

    #[test]
    fn promptless_resume_round_trips_as_null() {
        let request = NodeRequest::Resume {
            session: SessionAddress {
                workspace_id: WorkspaceId::new("primary").unwrap(),
                session: SessionKey {
                    instance_id: AgentInstanceId(7),
                    generation: SessionGeneration(2),
                },
            },
            terminal_size: TerminalSize { rows: 24, columns: 80 },
            initial_prompt: None,
        };
        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains(r#""initial_prompt":null"#));
        assert_eq!(serde_json::from_str::<NodeRequest>(&json).unwrap(), request);
    }

    #[test]
    fn node_and_workspace_ids_are_bounded_validated_wire_values() {
        assert_eq!(NodeId::new("node-1").unwrap().as_str(), "node-1");
        assert_eq!(WorkspaceId::new("repo_main").unwrap().as_str(), "repo_main");
        assert!(NodeId::new("Node-1").is_err());
        assert!(WorkspaceId::new("-repo").is_err());
        assert!(WorkspaceId::new("x".repeat(MAX_NODE_IDENTIFIER_BYTES + 1)).is_err());

        let encoded = serde_json::to_string(&WorkspaceId::new("repo-1").unwrap()).unwrap();
        assert_eq!(encoded, "\"repo-1\"");
        assert!(serde_json::from_str::<WorkspaceId>("\"Repo-1\"").is_err());
    }

    #[test]
    fn durable_session_wire_contract_round_trips() {
        assert_eq!(MAX_SESSION_DISPLAY_NAME_BYTES, 256);
        let record = ManagedSessionRecord {
            record_id: SessionRecordId::new("session-001").unwrap(),
            display_name: "release shepherd".to_owned(),
            provider: AgentProvider::Claude,
            mode: SessionMode::Pty,
            state: ManagedSessionState::Dormant,
            workspace_id: WorkspaceId::new("primary").unwrap(),
            canonical_root: host_path(r"C:\repo"),
            provider_session: Some(ProviderSessionIdentity {
                key: gate4agent_types::ProviderSessionKey::SessionId,
                id: "b1ef3250-47a2-42ca-9076-cc241487ea22".to_owned(),
                transcript_path: Some(r"C:\provider\sessions\b1ef3250.jsonl".to_owned()),
            }),
            active_session: None,
            created_at_unix_ms: 1_723_000_000_000,
            updated_at_unix_ms: 1_723_000_000_123,
            last_error: None,
        };

        let requests = [
            NodeRequest::RenameSessionRecord {
                record_id: record.record_id.clone(),
                display_name: "release verification".to_owned(),
            },
            NodeRequest::ResumeSessionRecord {
                record_id: record.record_id.clone(),
                terminal_size: TerminalSize { rows: 40, columns: 120 },
                initial_prompt: None,
            },
            NodeRequest::ForgetSessionRecord {
                record_id: record.record_id.clone(),
            },
        ];
        for request in requests {
            let json = serde_json::to_string(&request).unwrap();
            assert_eq!(serde_json::from_str::<NodeRequest>(&json).unwrap(), request);
        }

        let responses = [
            NodeResponse::SessionRecordUpdated {
                record: record.clone(),
            },
            NodeResponse::SessionRecordResumed {
                record: record.clone(),
                session: SessionAddress {
                    workspace_id: record.workspace_id.clone(),
                    session: SessionKey {
                        instance_id: AgentInstanceId(8),
                        generation: SessionGeneration(2),
                    },
                },
            },
            NodeResponse::SessionRecordForgotten {
                record_id: record.record_id.clone(),
            },
        ];
        for response in responses {
            let json = serde_json::to_string(&response).unwrap();
            assert_eq!(serde_json::from_str::<NodeResponse>(&json).unwrap(), response);
        }

        let events = [
            NodeEvent::SessionRecordUpserted {
                record: record.clone(),
            },
            NodeEvent::SessionRecordRemoved {
                record_id: record.record_id.clone(),
            },
        ];
        for event in events {
            let json = serde_json::to_string(&event).unwrap();
            assert_eq!(serde_json::from_str::<NodeEvent>(&json).unwrap(), event);
        }
    }

    #[test]
    fn session_record_ids_are_bounded_validated_wire_values() {
        let record_id = SessionRecordId::new("01j4k0jta3eynt5kxef132kr39").unwrap();
        assert_eq!(record_id.as_str(), "01j4k0jta3eynt5kxef132kr39");
        assert!(SessionRecordId::new("").is_err());
        assert!(SessionRecordId::new("Session-1").is_err());
        assert!(SessionRecordId::new("-session-1").is_err());
        assert!(SessionRecordId::new("x".repeat(MAX_NODE_IDENTIFIER_BYTES + 1)).is_err());

        let json = serde_json::to_string(&record_id).unwrap();
        assert_eq!(serde_json::from_str::<SessionRecordId>(&json).unwrap(), record_id);
        assert!(serde_json::from_str::<SessionRecordId>("\"Session-1\"").is_err());
    }

    #[test]
    fn node_snapshot_defaults_managed_sessions_for_legacy_wire_payloads() {
        let legacy = r#"{"node_id":"fixture-node","enabled_providers":[],"workspaces":[]}"#;
        let snapshot = serde_json::from_str::<NodeSnapshot>(legacy).unwrap();
        assert!(snapshot.session_records.is_empty());
    }
}
