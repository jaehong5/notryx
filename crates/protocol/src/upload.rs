use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use ts_rs::TS;
use uuid::{Uuid, Variant};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct UploadResponse {
    #[serde(deserialize_with = "upload_id")]
    #[schemars(extend("format" = "uuid"))]
    pub upload_id: String,
    #[serde(deserialize_with = "content_hash")]
    #[schemars(regex(pattern = "^sha256:[0-9a-f]{64}$"))]
    pub content_hash: String,
    #[serde(deserialize_with = "crate::http::decimal_u64")]
    #[schemars(regex(pattern = "^[0-9]+$"))]
    pub size: String,
    #[serde(deserialize_with = "crate::http::decimal_u64")]
    #[schemars(regex(pattern = "^[0-9]+$"))]
    pub expires_at: String,
}

pub(crate) fn valid_id(id: Uuid) -> bool {
    id.get_version_num() == 4 && id.get_variant() == Variant::RFC4122
}

fn upload_id<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    let value = String::deserialize(deserializer)?;
    let id = value.parse::<Uuid>().map_err(serde::de::Error::custom)?;
    if !valid_id(id) {
        return Err(serde::de::Error::custom("invalid upload identifier"));
    }
    Ok(value)
}

fn content_hash<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    let value = String::deserialize(deserializer)?;
    if !value.strip_prefix("sha256:").is_some_and(|hash| {
        hash.len() == 64
            && hash
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    }) {
        return Err(serde::de::Error::custom("invalid content hash"));
    }
    Ok(value)
}
