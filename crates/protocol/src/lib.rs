use std::fmt;

use base64::{Engine, engine::general_purpose::STANDARD};
use notryx_domain::{ChangeSeq, ContentRevision, MetaRevision, WorkspaceEpoch};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use ts_rs::TS;

pub mod admin;
pub mod create;
pub mod http;
pub mod node;
pub mod operation;
pub mod structure;
pub mod upload;
pub use admin::{AdminRecoverySummary, AdminRequest, AdminResponse, AdminResult};
pub use create::{CreateNodeRequest, CreatedNode};
pub use http::{
    ApiError, NodeKind, NodeMetadata, PairRequest, PairResponse, ServerInfo, WorkspaceRole,
};
pub use node::{InventoryPage, NodeState};
pub use operation::{AbandonRequest, AckRequest, AckResponse, OperationError, OperationReceipt};
pub use structure::{MoveNodeRequest, RestoreNodeRequest, TrashNode, TrashPage};
pub use upload::UploadResponse;

pub const CONTRACT: &str = "mdw-sync/1.2";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvalidContract {
    Protocol,
    Etag,
    Digest,
}

impl fmt::Display for InvalidContract {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Protocol => "unsupported protocol contract",
            Self::Etag => "invalid strong ETag",
            Self::Digest => "invalid content digest",
        })
    }
}

impl std::error::Error for InvalidContract {}

pub fn require_contract(value: &str) -> Result<(), InvalidContract> {
    if value == CONTRACT {
        Ok(())
    } else {
        Err(InvalidContract::Protocol)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StrongEtag(String);

impl StrongEtag {
    pub fn parse(value: &str) -> Result<Self, InvalidContract> {
        let inner = value
            .strip_prefix('"')
            .and_then(|value| value.strip_suffix('"'))
            .ok_or(InvalidContract::Etag)?;
        if !inner
            .bytes()
            .all(|byte| byte == 0x21 || (0x23..=0x7e).contains(&byte) || byte >= 0x80)
        {
            return Err(InvalidContract::Etag);
        }
        Ok(Self(value.into()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn from_content_revision(revision: &ContentRevision) -> Result<Self, InvalidContract> {
        Self::parse(&format!("\"{}\"", revision.as_str()))
    }

    pub fn from_meta_revision(revision: &MetaRevision) -> Result<Self, InvalidContract> {
        Self::parse(&format!("\"{}\"", revision.as_str()))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContentPrecondition(StrongEtag);

impl ContentPrecondition {
    pub fn parse(value: &str) -> Result<Self, InvalidContract> {
        StrongEtag::parse(value).map(Self)
    }

    pub fn etag(&self) -> &StrongEtag {
        &self.0
    }

    pub fn revision(&self) -> Result<ContentRevision, InvalidContract> {
        let value = self
            .0
            .as_str()
            .strip_prefix('"')
            .and_then(|value| value.strip_suffix('"'))
            .ok_or(InvalidContract::Etag)?;
        if value.len() > 256 {
            return Err(InvalidContract::Etag);
        }
        ContentRevision::new(value).map_err(|_| InvalidContract::Etag)
    }

    pub fn matches(&self, current: &ContentRevision) -> Result<bool, InvalidContract> {
        Ok(self.0 == StrongEtag::from_content_revision(current)?)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetadataPrecondition(StrongEtag);

impl MetadataPrecondition {
    pub fn parse(value: &str) -> Result<Self, InvalidContract> {
        StrongEtag::parse(value).map(Self)
    }

    pub fn etag(&self) -> &StrongEtag {
        &self.0
    }

    pub fn revision(&self) -> Result<MetaRevision, InvalidContract> {
        let value = self
            .0
            .as_str()
            .strip_prefix('"')
            .and_then(|value| value.strip_suffix('"'))
            .ok_or(InvalidContract::Etag)?;
        if value.len() > 256 {
            return Err(InvalidContract::Etag);
        }
        MetaRevision::new(value).map_err(|_| InvalidContract::Etag)
    }

    pub fn matches(&self, current: &MetaRevision) -> Result<bool, InvalidContract> {
        Ok(self.0 == StrongEtag::from_meta_revision(current)?)
    }
}

pub fn verify_content_digest(header: &str, bytes: &[u8]) -> Result<(), InvalidContract> {
    let encoded = header
        .strip_prefix("sha-256=:")
        .and_then(|value| value.strip_suffix(':'))
        .ok_or(InvalidContract::Digest)?;
    let expected = STANDARD
        .decode(encoded)
        .map_err(|_| InvalidContract::Digest)?;
    if expected.as_slice() != &Sha256::digest(bytes)[..] {
        return Err(InvalidContract::Digest);
    }
    Ok(())
}

fn decimal_generation<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<String, D::Error> {
    let value = String::deserialize(deserializer)?;
    ChangeSeq::try_from(value.clone()).map_err(serde::de::Error::custom)?;
    Ok(value)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct HeadHint {
    #[schemars(with = "String")]
    #[ts(type = "string")]
    pub workspace_epoch: WorkspaceEpoch,
    #[schemars(with = "String", regex(pattern = "^[0-9]+$"))]
    #[ts(type = "string")]
    pub head_seq: ChangeSeq,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct M0OpenRequest {
    pub document_id: String,
    pub renderer_session_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct M0CloseRequest {
    pub document_id: String,
    pub edit_session_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct M0OpenedDocument {
    pub document_id: String,
    pub edit_session_id: String,
    #[serde(deserialize_with = "decimal_generation")]
    #[schemars(regex(pattern = "^[0-9]+$"))]
    pub generation: String,
    pub bytes_base64: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct M0SaveRequest {
    pub document_id: String,
    pub edit_session_id: String,
    #[serde(deserialize_with = "decimal_generation")]
    #[schemars(regex(pattern = "^[0-9]+$"))]
    pub generation: String,
    pub bytes_base64: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct M0SavedCheckpoint {
    pub edit_session_id: String,
    #[serde(deserialize_with = "decimal_generation")]
    #[schemars(regex(pattern = "^[0-9]+$"))]
    pub generation: String,
}
