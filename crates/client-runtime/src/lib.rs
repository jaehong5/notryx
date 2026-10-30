use notryx_domain::{ChangeSeq, NodeId, WorkspaceEpoch, WorkspaceId};
use notryx_protocol::{
    ApiError, CONTRACT, ChangesPage, DeviceCurrentRole, DeviceIdentity, InventoryPage, NodeKind,
    NodeState, PairRequest, PairResponse, ServerInfo, StrongEtag, WorkspaceRole, require_contract,
    verify_content_digest,
};
use reqwest::{Client, Response, Url, header};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use std::{collections::HashSet, net::IpAddr, time::Duration};
use uuid::Uuid;

mod events;
pub use events::EventStream;
mod create;
pub use create::CreateMutation;
mod delete;
pub use delete::DeleteMutation;
mod move_node;
pub use move_node::MoveMutation;
mod restore;
mod trash;
pub use restore::RestoreMutation;
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
    Forbidden,
    GatewayAuthRequired,
    IdentityUnsupported,
    RoleUnsupported,
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
    ResyncRequired,
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Binding {
    pub origin: String,
    pub workspace_id: WorkspaceId,
    pub workspace_epoch: WorkspaceEpoch,
    pub device_id: Uuid,
    pub device_stream_id: Uuid,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoredCredential {
    binding: Binding,
    token: String,
}

impl StoredCredential {
    pub fn binding(&self) -> &Binding {
        &self.binding
    }
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

fn client() -> Result<Client, Error> {
    Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .no_gzip()
        .no_brotli()
        .no_deflate()
        .no_zstd()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|_| Error::Network)
}

fn authorization(token: &str) -> Result<header::HeaderValue, Error> {
    if token.is_empty() || token.len() > 1024 {
        return Err(Error::Contract);
    }
    let mut value =
        header::HeaderValue::from_str(&format!("Bearer {token}")).map_err(|_| Error::Contract)?;
    value.set_sensitive(true);
    Ok(value)
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
        return if status.as_u16() == 403 && code.as_deref() == Some("forbidden") {
            Err(Error::Forbidden)
        } else if matches!(code.as_deref(), Some("unauthorized" | "device_revoked")) {
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
        429 | 503 => Err(Error::Unavailable),
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
    pub fn from_stored_credential(
        expected: &Binding,
        credential: &StoredCredential,
    ) -> Result<Self, Error> {
        if credential.binding != *expected {
            return Err(Error::Binding);
        }
        let origin = origin(&expected.origin)?;
        if origin.origin().ascii_serialization() != expected.origin {
            return Err(Error::InvalidOrigin);
        }
        device_id(&expected.device_id.to_string())?;
        device_id(&expected.device_stream_id.to_string())?;
        let authorization = authorization(&credential.token)?;
        let client = client()?;
        Ok(Self {
            client,
            origin,
            authorization,
            binding: expected.clone(),
        })
    }

    pub fn stored_credential(&self) -> Result<StoredCredential, Error> {
        let token = self
            .authorization
            .as_bytes()
            .strip_prefix(b"Bearer ")
            .ok_or(Error::Contract)?;
        let token = std::str::from_utf8(token)
            .map_err(|_| Error::Contract)?
            .to_owned();
        Ok(StoredCredential {
            binding: self.binding.clone(),
            token,
        })
    }

    pub async fn pair(
        origin_value: &str,
        workspace_id: WorkspaceId,
        secret: &str,
    ) -> Result<Self, Error> {
        if secret.is_empty() || secret.len() > 1024 {
            return Err(Error::InvalidRequest);
        }
        let origin = origin(origin_value)?;
        let client = client()?;
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
        if idle > absolute {
            return Err(Error::Contract);
        }
        let authorization = authorization(&paired.device_token)?;
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

    pub async fn verify_binding(&self) -> Result<(), Error> {
        let response = self.get("/v1/devices/self").await?;
        if matches!(response.status().as_u16(), 404 | 429 | 503) {
            if content_type(&response)?.is_some_and(|value| {
                value.eq_ignore_ascii_case("text/html")
                    || value.eq_ignore_ascii_case("application/xhtml+xml")
            }) {
                return Err(Error::GatewayAuthRequired);
            }
            return Err(if response.status().as_u16() == 404 {
                Error::IdentityUnsupported
            } else {
                Error::Unavailable
            });
        }
        let identity: DeviceIdentity = json(response).await?;
        require_contract(&identity.protocol).map_err(|_| Error::Contract)?;
        let device = device_id(&identity.device_id)?;
        let stream = device_id(&identity.device_stream_id)?;
        if identity.workspace_id != self.binding.workspace_id
            || identity.workspace_epoch != self.binding.workspace_epoch
            || device != self.binding.device_id
            || stream != self.binding.device_stream_id
        {
            return Err(Error::Binding);
        }
        Ok(())
    }

    pub async fn current_role(&self) -> Result<WorkspaceRole, Error> {
        let response = self.get("/v1/devices/self/role").await?;
        if matches!(response.status().as_u16(), 404 | 429 | 503) {
            if content_type(&response)?.is_some_and(|value| {
                value.eq_ignore_ascii_case("text/html")
                    || value.eq_ignore_ascii_case("application/xhtml+xml")
            }) {
                return Err(Error::GatewayAuthRequired);
            }
            return Err(if response.status().as_u16() == 404 {
                Error::RoleUnsupported
            } else {
                Error::Unavailable
            });
        }
        let current: DeviceCurrentRole = json(response).await?;
        require_contract(&current.protocol).map_err(|_| Error::Contract)?;
        let device = device_id(&current.device_id)?;
        let stream = device_id(&current.device_stream_id)?;
        if current.workspace_id != self.binding.workspace_id
            || current.workspace_epoch != self.binding.workspace_epoch
            || device != self.binding.device_id
            || stream != self.binding.device_stream_id
        {
            return Err(Error::Binding);
        }
        Ok(current.role)
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

    pub async fn changes(
        &self,
        after: ChangeSeq,
        through: Option<ChangeSeq>,
    ) -> Result<ChangesPage, Error> {
        if through.is_some_and(|end| end < after) {
            return Err(Error::InvalidRequest);
        }
        let mut path = format!("/v1/changes?after={}", after.get());
        if let Some(end) = through {
            path.push_str(&format!("&through={}", end.get()));
        }
        let response = self.get(&path).await?;
        if response.status().as_u16() == 410
            && content_type(&response)?
                .is_some_and(|value| value.eq_ignore_ascii_case("application/json"))
        {
            let error: ApiError =
                serde_json::from_slice(&bounded_body(response, MAX_JSON_BYTES).await?)
                    .map_err(|_| Error::Contract)?;
            return Err(if error.code == "resync_required" {
                Error::ResyncRequired
            } else {
                Error::Response
            });
        }
        let page: ChangesPage = json(response).await?;
        if page.workspace_epoch != self.binding.workspace_epoch {
            return Err(Error::Binding);
        }
        let start = after.get();
        let end = page.through_seq.get();
        let covered = page.covered_through.get();
        if page.from_exclusive != after
            || through.is_some_and(|requested| requested != page.through_seq)
            || end < start
            || covered < start
            || covered > end
            || covered - start > 100
            || page.has_more != (covered < end)
            || (covered == start && covered != end)
        {
            return Err(Error::Contract);
        }

        let mut sequence = start;
        let mut nodes_at_sequence = HashSet::new();
        for node in &page.nodes {
            let next = node.last_change_seq.get();
            if next != sequence {
                if sequence.checked_add(1) != Some(next) {
                    return Err(Error::Contract);
                }
                sequence = next;
                nodes_at_sequence.clear();
            }
            if next <= start || next > covered || !nodes_at_sequence.insert(node.node_id) {
                return Err(Error::Contract);
            }
            let meta =
                StrongEtag::from_meta_revision(&node.meta_rev).map_err(|_| Error::Contract)?;
            if node.meta_etag != meta.as_str() || StrongEtag::parse(&node.node_etag).is_err() {
                return Err(Error::Contract);
            }
            if let Some(revision) = &node.content_rev {
                let etag =
                    StrongEtag::from_content_revision(revision).map_err(|_| Error::Contract)?;
                let hash = node
                    .content_hash
                    .as_deref()
                    .and_then(|value| value.strip_prefix("sha256:"));
                if node.content_etag.as_deref() != Some(etag.as_str())
                    || !hash.is_some_and(|value| {
                        value.len() == 64
                            && value
                                .bytes()
                                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
                    })
                {
                    return Err(Error::Contract);
                }
            }
        }
        if sequence != covered {
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
        if response.status().as_u16() == 410
            && content_type(&response)?
                .is_some_and(|value| value.eq_ignore_ascii_case("application/json"))
        {
            let body = bounded_body(response, MAX_JSON_BYTES).await?;
            let deleted = serde_json::from_slice::<ApiError>(&body)
                .ok()
                .is_some_and(|error| error.code == "node_deleted");
            return Err(if deleted {
                Error::Changed
            } else {
                Error::Response
            });
        }
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
        let bytes =
            bounded_body(response, MAX_CONTENT_BYTES)
                .await
                .map_err(|error| match error {
                    Error::TooLarge => Error::Contract,
                    other => other,
                })?;
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

#[cfg(test)]
mod credential_tests {
    use super::*;

    const SYNTHETIC_TOKEN: &str = "synthetic-credential-8ccbfe85";

    fn binding() -> Binding {
        Binding {
            origin: "https://workspace.example".into(),
            workspace_id: Uuid::new_v4().to_string().parse().unwrap(),
            workspace_epoch: Uuid::new_v4().to_string().parse().unwrap(),
            device_id: Uuid::new_v4(),
            device_stream_id: Uuid::new_v4(),
        }
    }

    fn credential(binding: &Binding) -> StoredCredential {
        StoredCredential {
            binding: binding.clone(),
            token: SYNTHETIC_TOKEN.into(),
        }
    }

    #[tokio::test]
    async fn credential_restores_the_same_binding_and_marks_header_sensitive() {
        let binding = binding();
        let expected = credential(&binding);
        let serialized = serde_json::to_vec(&expected).unwrap();
        let recovered: StoredCredential = serde_json::from_slice(&serialized).unwrap();
        let session = Session::from_stored_credential(&binding, &recovered)
            .expect("valid credential must restore");
        assert_eq!(session.binding(), &binding);
        assert!(session.authorization.is_sensitive());
        assert!(!format!("{:?}", session.authorization).contains(SYNTHETIC_TOKEN));
        let exported = session.stored_credential().unwrap();
        assert_eq!(exported.binding(), &binding);
        assert_eq!(serde_json::to_vec(&exported).unwrap(), serialized);
    }

    #[tokio::test]
    async fn credential_exports_and_restores_the_actual_pair_response() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let mut expected = binding();
        expected.origin.clone_from(&origin);
        let response = serde_json::json!({
            "protocol": CONTRACT,
            "workspace_id": expected.workspace_id,
            "workspace_epoch": expected.workspace_epoch,
            "principal_id": Uuid::new_v4().to_string(),
            "device_id": expected.device_id.to_string(),
            "device_stream_id": expected.device_stream_id.to_string(),
            "device_token": SYNTHETIC_TOKEN,
            "role": "editor",
            "idle_expires_at": "1000",
            "absolute_expires_at": "2000",
        })
        .to_string();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut chunk = [0; 1024];
            let header_end = loop {
                let read = stream.read(&mut chunk).await.unwrap();
                assert!(read > 0 && request.len() + read <= 8192);
                request.extend_from_slice(&chunk[..read]);
                if let Some(position) = request.windows(4).position(|part| part == b"\r\n\r\n") {
                    break position + 4;
                }
            };
            assert!(request.starts_with(b"POST /v1/pair "));
            let headers = std::str::from_utf8(&request[..header_end]).unwrap();
            let length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().unwrap())
                })
                .unwrap();
            while request.len() < header_end + length {
                let read = stream.read(&mut chunk).await.unwrap();
                assert!(read > 0 && request.len() + read <= 8192);
                request.extend_from_slice(&chunk[..read]);
            }
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                response.len()
            );
            stream.write_all(head.as_bytes()).await.unwrap();
            stream.write_all(response.as_bytes()).await.unwrap();
        });
        let session = Session::pair(&origin, expected.workspace_id, "synthetic-pairing")
            .await
            .unwrap();
        server.await.unwrap();
        let credential = session.stored_credential().unwrap();
        assert_eq!(credential.binding(), &expected);
        let restored = Session::from_stored_credential(&expected, &credential).unwrap();
        assert_eq!(restored.binding(), &expected);
        assert!(restored.authorization.is_sensitive());
        assert!(restored.authorization.as_bytes() == session.authorization.as_bytes());
    }

    #[tokio::test]
    async fn credential_rejects_every_mismatched_binding_without_network() {
        let binding = binding();
        let credential = credential(&binding);
        let mut mismatches = Vec::new();
        let mut different = binding.clone();
        different.origin = "https://different.example".into();
        mismatches.push(different);
        let mut different = binding.clone();
        different.workspace_id = Uuid::new_v4().to_string().parse().unwrap();
        mismatches.push(different);
        let mut different = binding.clone();
        different.workspace_epoch = Uuid::new_v4().to_string().parse().unwrap();
        mismatches.push(different);
        let mut different = binding.clone();
        different.device_id = Uuid::new_v4();
        mismatches.push(different);
        let mut different = binding.clone();
        different.device_stream_id = Uuid::new_v4();
        mismatches.push(different);
        for different in mismatches {
            assert!(matches!(
                Session::from_stored_credential(&different, &credential),
                Err(Error::Binding)
            ));
        }
    }

    #[tokio::test]
    async fn credential_rejects_bad_token_invalid_origin_and_device() {
        let valid = binding();
        for invalid in ["".to_owned(), "bad\nheader".into(), "x".repeat(1025)] {
            let mut record = credential(&valid);
            record.token = invalid;
            assert!(matches!(
                Session::from_stored_credential(&valid, &record),
                Err(Error::Contract)
            ));
        }
        for invalid in [
            "http://workspace.example",
            "https://workspace.example/path",
            "https://user:pass@workspace.example",
            "https://workspace.example/",
        ] {
            let mut invalid_binding = valid.clone();
            invalid_binding.origin = invalid.into();
            let record = credential(&invalid_binding);
            assert!(matches!(
                Session::from_stored_credential(&invalid_binding, &record),
                Err(Error::InvalidOrigin)
            ));
        }
        let mut invalid_binding = valid.clone();
        invalid_binding.device_id = Uuid::nil();
        let record = credential(&invalid_binding);
        assert!(matches!(
            Session::from_stored_credential(&invalid_binding, &record),
            Err(Error::Contract)
        ));
        let mut invalid_binding = valid.clone();
        invalid_binding.device_stream_id = Uuid::nil();
        let record = credential(&invalid_binding);
        assert!(matches!(
            Session::from_stored_credential(&invalid_binding, &record),
            Err(Error::Contract)
        ));
    }

    #[test]
    fn credential_deserialization_rejects_unknown_fields() {
        let record = credential(&binding());
        let mut json = serde_json::to_value(&record).unwrap();
        json.as_object_mut()
            .unwrap()
            .insert("unexpected".into(), true.into());
        assert!(serde_json::from_value::<StoredCredential>(json).is_err());
    }
}
