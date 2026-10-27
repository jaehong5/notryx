use notryx_domain::{NodeId, WorkspaceEpoch, WorkspaceId};
use notryx_protocol::{
    ApiError, CONTRACT, InventoryPage, NodeKind, NodeState, PairRequest, PairResponse, ServerInfo,
    StrongEtag, require_contract, verify_content_digest,
};
use reqwest::{Client, Response, Url, header};
use serde::de::DeserializeOwned;
use sha2::{Digest, Sha256};
use std::{net::IpAddr, time::Duration};
use uuid::Uuid;

mod write;
pub use write::{AbandonMutation, ContentMutation, OperationLookup};
mod preview_image;
pub use preview_image::{PreviewImageError, ValidatedPreviewImage, validate_preview_image};
mod external_url;
pub use external_url::{ExternalUrl, ExternalUrlError, open_external_url, validate_external_url};

const MAX_JSON_BYTES: usize = 1024 * 1024;
pub const MAX_EDITABLE_DOCUMENT_BYTES: usize = 5 * 1024 * 1024;
const MAX_CONTENT_BYTES: usize = 25 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    InvalidOrigin,
    InvalidRequest,
    Network,
    Unauthorized,
    GatewayAuthRequired,
    Response,
    Contract,
    Binding,
    Changed,
    TooLarge,
    OperationRetired,
    OperationNotFound,
    IdempotencyMismatch,
    OperationGap,
    EpochMismatch,
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binding {
    pub origin: String,
    pub workspace_id: WorkspaceId,
    pub workspace_epoch: WorkspaceEpoch,
    pub device_id: Uuid,
    pub device_stream_id: Uuid,
}

pub struct Session {
    client: Client,
    origin: Url,
    authorization: header::HeaderValue,
    binding: Binding,
}

fn origin(value: &str) -> Result<Url, Error> {
    let url = Url::parse(value).map_err(|_| Error::InvalidOrigin)?;
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path() != "/"
        || url.host_str().is_none()
    {
        return Err(Error::InvalidOrigin);
    }
    let local_ip = url
        .host_str()
        .and_then(|host| host.trim_matches(['[', ']']).parse::<IpAddr>().ok())
        .is_some_and(|ip| ip.is_loopback());
    if url.scheme() != "https" && !(url.scheme() == "http" && local_ip) {
        return Err(Error::InvalidOrigin);
    }
    Ok(url)
}

fn device_id(value: &str) -> Result<Uuid, Error> {
    let id = Uuid::parse_str(value).map_err(|_| Error::Contract)?;
    if id.get_version_num() != 4 || id.get_variant() != uuid::Variant::RFC4122 {
        return Err(Error::Contract);
    }
    Ok(id)
}

fn single_header<'a>(headers: &'a header::HeaderMap, name: &str) -> Result<Option<&'a str>, Error> {
    let mut values = headers.get_all(name).iter();
    let Some(value) = values.next() else {
        return Ok(None);
    };
    if values.next().is_some() {
        return Err(Error::Contract);
    }
    Ok(Some(value.to_str().map_err(|_| Error::Contract)?))
}

fn content_type(response: &Response) -> Result<Option<&str>, Error> {
    Ok(single_header(response.headers(), "content-type")?
        .and_then(|value| value.split(';').next())
        .map(str::trim))
}

async fn check_response(response: Response) -> Result<Response, Error> {
    let status = response.status();
    let media_type = content_type(&response)?;
    if status.is_redirection()
        || media_type.is_some_and(|value| {
            value.eq_ignore_ascii_case("text/html")
                || value.eq_ignore_ascii_case("application/xhtml+xml")
        })
    {
        return Err(Error::GatewayAuthRequired);
    }
    if status.as_u16() == 401 || status.as_u16() == 403 {
        let json_type =
            media_type.is_some_and(|value| value.eq_ignore_ascii_case("application/json"));
        let code = if json_type {
            serde_json::from_slice::<ApiError>(&bounded_body(response, MAX_JSON_BYTES).await?)
                .ok()
                .map(|error| error.code)
        } else {
            None
        };
        return if matches!(
            code.as_deref(),
            Some("unauthorized" | "device_revoked" | "forbidden")
        ) {
            Err(Error::Unauthorized)
        } else if status.as_u16() == 403 {
            Err(Error::GatewayAuthRequired)
        } else {
            Err(Error::Response)
        };
    }
    match status.as_u16() {
        200 => Ok(response),
        412 => Err(Error::Changed),
        _ => Err(Error::Response),
    }
}

async fn bounded_body(mut response: Response, limit: usize) -> Result<Vec<u8>, Error> {
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return Err(Error::TooLarge);
    }
    let mut result = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| Error::Network)? {
        if chunk.len() > limit.saturating_sub(result.len()) {
            return Err(Error::TooLarge);
        }
        result.extend_from_slice(&chunk);
    }
    Ok(result)
}

async fn json<T: DeserializeOwned>(response: Response) -> Result<T, Error> {
    let response = check_response(response).await?;
    if !content_type(&response)?.is_some_and(|value| value.eq_ignore_ascii_case("application/json"))
    {
        return Err(Error::Contract);
    }
    serde_json::from_slice(&bounded_body(response, MAX_JSON_BYTES).await?)
        .map_err(|_| Error::Contract)
}

impl Session {
    pub async fn pair(
        origin_value: &str,
        workspace_id: WorkspaceId,
        secret: &str,
    ) -> Result<Self, Error> {
        if secret.is_empty() || secret.len() > 1024 {
            return Err(Error::InvalidRequest);
        }
        let origin = origin(origin_value)?;
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .no_gzip()
            .no_brotli()
            .no_deflate()
            .no_zstd()
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|_| Error::Network)?;
        let request = PairRequest {
            protocol: CONTRACT.into(),
            workspace_id,
            pairing_secret: secret.into(),
        };
        let response = client
            .post(origin.join("/v1/pair").map_err(|_| Error::InvalidOrigin)?)
            .json(&request)
            .send()
            .await
            .map_err(|_| Error::Network)?;
        let paired: PairResponse = json(response).await?;
        require_contract(&paired.protocol).map_err(|_| Error::Contract)?;
        if paired.workspace_id != workspace_id {
            return Err(Error::Binding);
        }
        let _principal = device_id(&paired.principal_id)?;
        let binding = Binding {
            origin: origin.origin().ascii_serialization(),
            workspace_id,
            workspace_epoch: paired.workspace_epoch,
            device_id: device_id(&paired.device_id)?,
            device_stream_id: device_id(&paired.device_stream_id)?,
        };
        let idle = paired
            .idle_expires_at
            .parse::<u64>()
            .map_err(|_| Error::Contract)?;
        let absolute = paired
            .absolute_expires_at
            .parse::<u64>()
            .map_err(|_| Error::Contract)?;
        if idle > absolute || paired.device_token.is_empty() || paired.device_token.len() > 1024 {
            return Err(Error::Contract);
        }
        let mut authorization =
            header::HeaderValue::from_str(&format!("Bearer {}", paired.device_token))
                .map_err(|_| Error::Contract)?;
        authorization.set_sensitive(true);
        Ok(Self {
            client,
            origin,
            authorization,
            binding,
        })
    }

    pub fn binding(&self) -> &Binding {
        &self.binding
    }

    async fn get(&self, path: &str) -> Result<Response, Error> {
        self.client
            .get(self.origin.join(path).map_err(|_| Error::InvalidRequest)?)
            .header(header::AUTHORIZATION, self.authorization.clone())
            .send()
            .await
            .map_err(|_| Error::Network)
    }

    pub async fn server_info(&self) -> Result<ServerInfo, Error> {
        let info: ServerInfo = json(self.get("/v1/server-info").await?).await?;
        require_contract(&info.protocol).map_err(|_| Error::Contract)?;
        if info.workspace_id != self.binding.workspace_id
            || info.workspace_epoch != self.binding.workspace_epoch
        {
            return Err(Error::Binding);
        }
        Ok(info)
    }

    pub async fn inventory(&self, after: Option<NodeId>) -> Result<InventoryPage, Error> {
        let path = match after {
            Some(id) => format!("/v1/nodes?after_id={id}&limit=100"),
            None => "/v1/nodes?limit=100".into(),
        };
        let page: InventoryPage = json(self.get(&path).await?).await?;
        if page.workspace_epoch != self.binding.workspace_epoch {
            return Err(Error::Binding);
        }
        if page.nodes.len() > 100 {
            return Err(Error::Contract);
        }
        let mut previous = after.map(|id| id.to_string());
        for node in &page.nodes {
            let current = node.node_id.to_string();
            if node.deleted || previous.as_ref().is_some_and(|last| last >= &current) {
                return Err(Error::Contract);
            }
            previous = Some(current);
        }
        if let Some(next) = page.next_after_id
            && page.nodes.last().map(|node| node.node_id) != Some(next)
        {
            return Err(Error::Contract);
        }
        Ok(page)
    }

    pub async fn content(&self, node: &NodeState) -> Result<Vec<u8>, Error> {
        if node.kind != NodeKind::File || node.deleted {
            return Err(Error::InvalidRequest);
        }
        let revision = node.content_rev.as_ref().ok_or(Error::Contract)?;
        let etag = StrongEtag::from_content_revision(revision).map_err(|_| Error::Contract)?;
        if node.content_etag.as_deref() != Some(etag.as_str()) {
            return Err(Error::Contract);
        }
        let size = node
            .size
            .as_deref()
            .ok_or(Error::Contract)?
            .parse::<u64>()
            .map_err(|_| Error::Contract)?;
        if size > MAX_CONTENT_BYTES as u64 {
            return Err(Error::TooLarge);
        }
        let path = format!("/v1/nodes/{}/content", node.node_id);
        let response = self
            .client
            .get(self.origin.join(&path).map_err(|_| Error::InvalidRequest)?)
            .header(header::AUTHORIZATION, self.authorization.clone())
            .header(header::IF_MATCH, etag.as_str())
            .header(header::ACCEPT_ENCODING, "identity")
            .send()
            .await
            .map_err(|_| Error::Network)?;
        let response = check_response(response).await?;
        if !content_type(&response)?
            .is_some_and(|value| value.eq_ignore_ascii_case("application/octet-stream"))
        {
            return Err(Error::Contract);
        }
        if single_header(response.headers(), "content-encoding")?
            .is_some_and(|value| !value.trim().eq_ignore_ascii_case("identity"))
        {
            return Err(Error::Contract);
        }
        let response_etag = single_header(response.headers(), "etag")?.ok_or(Error::Contract)?;
        if response_etag != etag.as_str() {
            return Err(Error::Contract);
        }
        let digest = single_header(response.headers(), "content-digest")?
            .ok_or(Error::Contract)?
            .to_owned();
        let bytes = bounded_body(response, MAX_CONTENT_BYTES).await?;
        if bytes.len() as u64 != size {
            return Err(Error::Contract);
        }
        verify_content_digest(&digest, &bytes).map_err(|_| Error::Contract)?;
        let actual_hash = format!(
            "sha256:{}",
            Sha256::digest(&bytes)
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        );
        if node.content_hash.as_deref() != Some(actual_hash.as_str()) {
            return Err(Error::Contract);
        }
        Ok(bytes)
    }
}
