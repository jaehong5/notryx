use crate::{
    Workspace,
    state::auth::{DeviceRecord, PairingRecord},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use notryx_domain::{WorkspaceEpoch, WorkspaceId};
use notryx_protocol::{CONTRACT, PairRequest, PairResponse, WorkspaceRole};
use serde::Serialize;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthError {
    Unauthorized,
    InvalidRequest,
    Forbidden,
    NotFound,
    Storage,
}

impl std::fmt::Display for AuthError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for AuthError {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AuthIdentity {
    pub principal_id: Uuid,
    pub device_id: Uuid,
    pub stream_id: Uuid,
    pub role: WorkspaceRole,
}

#[derive(Debug, Clone, Serialize)]
pub struct AuthStatus {
    pub workspace_id: WorkspaceId,
    pub workspace_epoch: WorkspaceEpoch,
    pub owner_principal_id: Uuid,
}

#[derive(Serialize)]
pub struct PairingGrant {
    pub workspace_id: WorkspaceId,
    pub principal_id: Uuid,
    pub pairing_secret: String,
    pub expires_at: String,
}

const PAIRING_SECONDS: i64 = 300;
const IDLE_SECONDS: i64 = 90 * 24 * 60 * 60;
const ABSOLUTE_SECONDS: i64 = 365 * 24 * 60 * 60;

fn expiry(now: i64, lifetime: i64) -> Result<i64, AuthError> {
    if now < 0 {
        return Err(AuthError::InvalidRequest);
    }
    now.checked_add(lifetime).ok_or(AuthError::InvalidRequest)
}

fn credential(prefix: &str) -> Result<(Uuid, String, [u8; 32]), AuthError> {
    let mut secret = [0; 32];
    getrandom::fill(&mut secret).map_err(|_| AuthError::Storage)?;
    let id = Uuid::new_v4();
    let encoded = format!("{prefix}.{id}.{}", URL_SAFE_NO_PAD.encode(secret));
    Ok((id, encoded, Sha256::digest(secret).into()))
}

fn credential_hash(value: &str, prefix: &str) -> Result<(Uuid, [u8; 32]), AuthError> {
    if value.len() != prefix.len() + 1 + 36 + 1 + 43 {
        return Err(AuthError::Unauthorized);
    }
    let mut parts = value.split('.');
    if parts.next() != Some(prefix) {
        return Err(AuthError::Unauthorized);
    }
    let id_text = parts.next().ok_or(AuthError::Unauthorized)?;
    let id = Uuid::parse_str(id_text).map_err(|_| AuthError::Unauthorized)?;
    if id.get_version_num() != 4
        || id.get_variant() != uuid::Variant::RFC4122
        || id.to_string() != id_text
    {
        return Err(AuthError::Unauthorized);
    }
    let encoded = parts.next().ok_or(AuthError::Unauthorized)?;
    if parts.next().is_some() {
        return Err(AuthError::Unauthorized);
    }
    let secret: [u8; 32] = URL_SAFE_NO_PAD
        .decode(encoded)
        .map_err(|_| AuthError::Unauthorized)?
        .try_into()
        .map_err(|_| AuthError::Unauthorized)?;
    Ok((id, Sha256::digest(secret).into()))
}

impl Workspace {
    pub async fn auth_status(&mut self) -> Result<AuthStatus, AuthError> {
        Ok(AuthStatus {
            workspace_id: self.state.workspace_id,
            workspace_epoch: self.state.epoch,
            owner_principal_id: self.state.owner_principal().await?,
        })
    }

    pub async fn create_principal(&mut self, role: WorkspaceRole) -> Result<Uuid, AuthError> {
        if role == WorkspaceRole::Owner {
            return Err(AuthError::Forbidden);
        }
        let id = Uuid::new_v4();
        self.state.insert_principal(id, role).await?;
        Ok(id)
    }

    pub async fn set_role(
        &mut self,
        principal: Uuid,
        role: WorkspaceRole,
    ) -> Result<(), AuthError> {
        if role == WorkspaceRole::Owner {
            return Err(AuthError::Forbidden);
        }
        let current = self
            .state
            .principal_role(principal)
            .await?
            .ok_or(AuthError::NotFound)?;
        if current == WorkspaceRole::Owner {
            return Err(AuthError::Forbidden);
        }
        self.state.update_role(principal, role).await
    }

    pub async fn transfer_owner(
        &mut self,
        new_owner: Uuid,
        previous_role: WorkspaceRole,
    ) -> Result<(), AuthError> {
        if previous_role == WorkspaceRole::Owner {
            return Err(AuthError::Forbidden);
        }
        let current = self
            .state
            .principal_role(new_owner)
            .await?
            .ok_or(AuthError::NotFound)?;
        if current == WorkspaceRole::Owner {
            return Err(AuthError::InvalidRequest);
        }
        let owner = self.state.owner_principal().await?;
        self.state
            .transfer_owner(owner, new_owner, previous_role)
            .await
    }

    pub async fn issue_pairing(
        &mut self,
        principal: Uuid,
        now: i64,
    ) -> Result<PairingGrant, AuthError> {
        let expires_at = expiry(now, PAIRING_SECONDS)?;
        let role = self
            .state
            .principal_role(principal)
            .await?
            .ok_or(AuthError::NotFound)?;
        let (id, secret, secret_hash) = credential("pair")?;
        self.state
            .insert_pairing(&PairingRecord {
                pairing_id: id,
                principal_id: principal,
                workspace_id: self.state.workspace_id,
                issued_role: role,
                secret_hash,
                issued_at: now,
                expires_at,
                consumed_at: None,
            })
            .await?;
        Ok(PairingGrant {
            workspace_id: self.state.workspace_id,
            principal_id: principal,
            pairing_secret: secret,
            expires_at: expires_at.to_string(),
        })
    }

    pub async fn pair(
        &mut self,
        request: PairRequest,
        now: i64,
    ) -> Result<PairResponse, AuthError> {
        if request.protocol != CONTRACT {
            return Err(AuthError::InvalidRequest);
        }
        if request.workspace_id != self.state.workspace_id {
            return Err(AuthError::Unauthorized);
        }
        let (id, actual_hash) = credential_hash(&request.pairing_secret, "pair")?;
        let pairing = self.state.pairing(id).await?;
        let expected_hash = pairing
            .as_ref()
            .map_or([0; 32], |record| record.secret_hash);
        if !bool::from(actual_hash.ct_eq(&expected_hash)) {
            return Err(AuthError::Unauthorized);
        }
        let pairing = pairing.ok_or(AuthError::Unauthorized)?;
        if pairing.workspace_id != self.state.workspace_id
            || pairing.consumed_at.is_some()
            || now < pairing.issued_at
            || now >= pairing.expires_at
            || self.state.principal_role(pairing.principal_id).await? != Some(pairing.issued_role)
        {
            return Err(AuthError::Unauthorized);
        }
        let idle_expires_at = expiry(now, IDLE_SECONDS)?;
        let absolute_expires_at = expiry(now, ABSOLUTE_SECONDS)?;
        let (token_id, token, token_hash) = credential("device")?;
        let device = DeviceRecord {
            identity: AuthIdentity {
                principal_id: pairing.principal_id,
                device_id: Uuid::new_v4(),
                stream_id: Uuid::new_v4(),
                role: pairing.issued_role,
            },
            token_id,
            token_hash,
            issued_at: now,
            idle_expires_at,
            absolute_expires_at,
            revoked_at: None,
        };
        self.state.consume_pairing(&pairing, &device).await?;
        Ok(PairResponse {
            protocol: CONTRACT.into(),
            workspace_id: self.state.workspace_id,
            workspace_epoch: self.state.epoch,
            principal_id: device.identity.principal_id.to_string(),
            device_id: device.identity.device_id.to_string(),
            device_stream_id: device.identity.stream_id.to_string(),
            device_token: token,
            role: device.identity.role,
            idle_expires_at: idle_expires_at.to_string(),
            absolute_expires_at: absolute_expires_at.to_string(),
        })
    }

    pub async fn authenticate(&mut self, token: &str, now: i64) -> Result<AuthIdentity, AuthError> {
        let (id, actual_hash) = credential_hash(token, "device")?;
        let device = self.state.device(id).await?;
        let expected_hash = device.as_ref().map_or([0; 32], |record| record.token_hash);
        if !bool::from(actual_hash.ct_eq(&expected_hash)) {
            return Err(AuthError::Unauthorized);
        }
        let device = device.ok_or(AuthError::Unauthorized)?;
        if device.revoked_at.is_some()
            || now < device.issued_at
            || now >= device.idle_expires_at
            || now >= device.absolute_expires_at
        {
            return Err(AuthError::Unauthorized);
        }
        Ok(device.identity)
    }

    pub async fn revoke_device(&mut self, device: Uuid, now: i64) -> Result<(), AuthError> {
        if now < 0 {
            return Err(AuthError::InvalidRequest);
        }
        self.state.revoke_device(device, now).await
    }
}
