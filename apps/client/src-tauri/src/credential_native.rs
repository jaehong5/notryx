use notryx_client_runtime::Session;
#[cfg(test)]
use notryx_client_store::ClientStore;
use notryx_client_store::{CredentialPending, RegistrationBinding};
use std::sync::Arc;
use uuid::Uuid;

#[derive(Clone)]
struct StoredCredential {
    binding: RegistrationBinding,
    token: Vec<u8>,
}

type PendingCredential = CredentialPending;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CredentialError {
    Storage,
    Unavailable,
    BindingMismatch,
    Ambiguous,
    Missing,
    WriteUnknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CredentialMode {
    Stored,
    SessionOnly,
}

trait CredentialVault {
    async fn put(&mut self, id: Uuid, credential: &StoredCredential)
    -> Result<(), CredentialError>;
    async fn get(&mut self, id: Uuid) -> Result<Vec<StoredCredential>, CredentialError>;
    async fn delete(
        &mut self,
        _id: Uuid,
        _binding: &RegistrationBinding,
    ) -> Result<(), CredentialError> {
        Err(CredentialError::Storage)
    }
}

trait CredentialRegistry {
    async fn prepare(
        &mut self,
        binding: &RegistrationBinding,
        id: Uuid,
    ) -> Result<PendingCredential, CredentialError>;
    async fn pending(&mut self) -> Result<Option<PendingCredential>, CredentialError>;
    async fn commit(
        &mut self,
        pending: &PendingCredential,
        stored: bool,
    ) -> Result<(), CredentialError>;
    async fn active(
        &mut self,
    ) -> Result<Option<(RegistrationBinding, Option<Uuid>)>, CredentialError>;
    async fn discard(&mut self, _pending: &PendingCredential) -> Result<(), CredentialError> {
        Err(CredentialError::Storage)
    }
}

#[cfg(test)]
impl CredentialRegistry for ClientStore {
    async fn prepare(
        &mut self,
        binding: &RegistrationBinding,
        id: Uuid,
    ) -> Result<PendingCredential, CredentialError> {
        self.prepare_credential_pending(binding, id)
            .await
            .map_err(|_| CredentialError::Storage)
    }
    async fn pending(&mut self) -> Result<Option<PendingCredential>, CredentialError> {
        self.pending_credential()
            .await
            .map_err(|_| CredentialError::Storage)
    }
    async fn commit(
        &mut self,
        pending: &PendingCredential,
        stored: bool,
    ) -> Result<(), CredentialError> {
        self.commit_credential_pending(pending, stored)
            .await
            .map_err(|_| CredentialError::Storage)
    }
    async fn active(
        &mut self,
    ) -> Result<Option<(RegistrationBinding, Option<Uuid>)>, CredentialError> {
        self.active_credential()
            .await
            .map_err(|_| CredentialError::Storage)
    }
    async fn discard(&mut self, pending: &PendingCredential) -> Result<(), CredentialError> {
        self.discard_credential_pending(pending)
            .await
            .map_err(|_| CredentialError::Storage)
    }
}

struct PairRegistry<'a> {
    runtime: &'a crate::Runtime,
    snapshot: &'a crate::gate::PairSnapshot,
    session: Arc<Session>,
}

impl CredentialRegistry for PairRegistry<'_> {
    async fn prepare(
        &mut self,
        binding: &RegistrationBinding,
        id: Uuid,
    ) -> Result<PendingCredential, CredentialError> {
        let mut state = self.runtime.0.lock().await;
        state
            .gate
            .validate_pair(self.snapshot)
            .map_err(|_| CredentialError::Storage)?;
        if crate::registration_binding(self.session.binding()) != *binding {
            return Err(CredentialError::BindingMismatch);
        }
        state
            .store
            .prepare_credential_pending(binding, id)
            .await
            .map_err(|_| CredentialError::Storage)
    }

    async fn pending(&mut self) -> Result<Option<PendingCredential>, CredentialError> {
        let mut state = self.runtime.0.lock().await;
        state
            .gate
            .validate_pair(self.snapshot)
            .map_err(|_| CredentialError::Storage)?;
        state
            .store
            .pending_credential()
            .await
            .map_err(|_| CredentialError::Storage)
    }

    async fn commit(
        &mut self,
        pending: &PendingCredential,
        stored: bool,
    ) -> Result<(), CredentialError> {
        let mut state = self.runtime.0.lock().await;
        state
            .gate
            .validate_pair(self.snapshot)
            .map_err(|_| CredentialError::Storage)?;
        if crate::registration_binding(self.session.binding()) != pending.binding {
            return Err(CredentialError::BindingMismatch);
        }
        if state
            .transmission_gate
            .response_generation(&pending.binding)
            >= u64::MAX - 1
        {
            return Err(CredentialError::Storage);
        }
        state
            .store
            .commit_credential_pending(pending, stored)
            .await
            .map_err(|_| CredentialError::Storage)?;
        let token = state
            .transmission_gate
            .begin_initial_sync(&pending.binding)
            .ok_or(CredentialError::Storage)?;
        state
            .gate
            .commit_pair(self.snapshot)
            .map_err(|_| CredentialError::Storage)?;
        crate::search_native::invalidate_search(&mut state);
        if let Some(previous) = state.sync_task.take() {
            previous.abort();
        }
        state.initial_sync = Some(token);
        state.sync_state = Some(notryx_protocol::WorkspaceSyncState {
            binding: crate::to_contract(pending.binding.clone()),
            status: notryx_protocol::WorkspaceSyncStatus::Connecting,
        });
        state.session = Some(self.session.clone());
        state.pending_pair = None;
        state.credential_identity_unsupported = None;
        Ok(())
    }

    async fn active(
        &mut self,
    ) -> Result<Option<(RegistrationBinding, Option<Uuid>)>, CredentialError> {
        let mut state = self.runtime.0.lock().await;
        state
            .gate
            .validate_pair(self.snapshot)
            .map_err(|_| CredentialError::Storage)?;
        state
            .store
            .active_credential()
            .await
            .map_err(|_| CredentialError::Storage)
    }
}

struct ActiveRegistry<'a>(&'a crate::Runtime);

impl CredentialRegistry for ActiveRegistry<'_> {
    async fn prepare(
        &mut self,
        _binding: &RegistrationBinding,
        _id: Uuid,
    ) -> Result<PendingCredential, CredentialError> {
        Err(CredentialError::Storage)
    }
    async fn pending(&mut self) -> Result<Option<PendingCredential>, CredentialError> {
        self.0
            .0
            .lock()
            .await
            .store
            .pending_credential()
            .await
            .map_err(|_| CredentialError::Storage)
    }
    async fn commit(
        &mut self,
        _pending: &PendingCredential,
        _stored: bool,
    ) -> Result<(), CredentialError> {
        Err(CredentialError::Storage)
    }
    async fn active(
        &mut self,
    ) -> Result<Option<(RegistrationBinding, Option<Uuid>)>, CredentialError> {
        self.0
            .0
            .lock()
            .await
            .store
            .active_credential()
            .await
            .map_err(|_| CredentialError::Storage)
    }
    async fn discard(&mut self, pending: &PendingCredential) -> Result<(), CredentialError> {
        self.0
            .0
            .lock()
            .await
            .store
            .discard_credential_pending(pending)
            .await
            .map_err(|_| CredentialError::Storage)
    }
}

struct OwnerRegistry<'a> {
    runtime: &'a crate::Runtime,
    snapshot: crate::gate::RequestSnapshot,
}

impl CredentialRegistry for OwnerRegistry<'_> {
    async fn prepare(
        &mut self,
        _binding: &RegistrationBinding,
        _id: Uuid,
    ) -> Result<PendingCredential, CredentialError> {
        Err(CredentialError::Storage)
    }
    async fn pending(&mut self) -> Result<Option<PendingCredential>, CredentialError> {
        let mut state = self.runtime.0.lock().await;
        state
            .gate
            .validate(&self.snapshot)
            .map_err(|_| CredentialError::Storage)?;
        state
            .store
            .pending_credential()
            .await
            .map_err(|_| CredentialError::Storage)
    }
    async fn commit(
        &mut self,
        _pending: &PendingCredential,
        _stored: bool,
    ) -> Result<(), CredentialError> {
        Err(CredentialError::Storage)
    }
    async fn active(
        &mut self,
    ) -> Result<Option<(RegistrationBinding, Option<Uuid>)>, CredentialError> {
        Err(CredentialError::Storage)
    }
    async fn discard(&mut self, pending: &PendingCredential) -> Result<(), CredentialError> {
        let mut state = self.runtime.0.lock().await;
        state
            .gate
            .validate(&self.snapshot)
            .map_err(|_| CredentialError::Storage)?;
        state
            .store
            .discard_credential_pending(pending)
            .await
            .map_err(|_| CredentialError::Storage)
    }
}

fn select_one(
    mut records: Vec<StoredCredential>,
    binding: &RegistrationBinding,
) -> Result<StoredCredential, CredentialError> {
    match records.len() {
        0 => return Err(CredentialError::Missing),
        1 => {}
        _ => return Err(CredentialError::Ambiguous),
    }
    let record = records.pop().expect("one credential was checked");
    if &record.binding != binding || record.token.is_empty() || record.token.len() > 8192 {
        return Err(CredentialError::BindingMismatch);
    }
    Ok(record)
}

fn session_credential(session: &Session) -> Result<StoredCredential, CredentialError> {
    let credential = session
        .stored_credential()
        .map_err(|_| CredentialError::BindingMismatch)?;
    let token = serde_json::to_vec(&credential).map_err(|_| CredentialError::Storage)?;
    if token.len() > 8192 {
        return Err(CredentialError::Storage);
    }
    Ok(StoredCredential {
        binding: crate::registration_binding(session.binding()),
        token,
    })
}

fn decode_credential(bytes: Vec<u8>) -> Result<StoredCredential, CredentialError> {
    if bytes.is_empty() || bytes.len() > 8192 {
        return Err(CredentialError::BindingMismatch);
    }
    let credential: notryx_client_runtime::StoredCredential =
        serde_json::from_slice(&bytes).map_err(|_| CredentialError::BindingMismatch)?;
    Session::from_stored_credential(credential.binding(), &credential)
        .map_err(|_| CredentialError::BindingMismatch)?;
    Ok(StoredCredential {
        binding: crate::registration_binding(credential.binding()),
        token: bytes,
    })
}

struct PlatformVault;

async fn guarded_create<F: std::future::Future<Output = Result<(), CredentialError>>>(
    future: F,
    duration: std::time::Duration,
) -> Result<(), CredentialError> {
    match tokio::time::timeout(duration, future).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(_)) | Err(_) => Err(CredentialError::WriteUnknown),
    }
}

#[cfg(target_os = "linux")]
fn service_error(error: secret_service::Error) -> CredentialError {
    match error {
        secret_service::Error::Locked
        | secret_service::Error::NoResult
        | secret_service::Error::Prompt
        | secret_service::Error::PromptDisconnected
        | secret_service::Error::Unavailable
        | secret_service::Error::Zbus(_)
        | secret_service::Error::ZbusFdo(_) => CredentialError::Unavailable,
        _ => CredentialError::Storage,
    }
}

impl CredentialVault for PlatformVault {
    async fn put(
        &mut self,
        id: Uuid,
        credential: &StoredCredential,
    ) -> Result<(), CredentialError> {
        #[cfg(target_os = "linux")]
        {
            use secret_service::{EncryptionType, SecretService};
            use std::{
                collections::HashMap,
                sync::atomic::{AtomicBool, Ordering},
                time::Duration,
            };
            let decoded = decode_credential(credential.token.clone())?;
            if decoded.binding != credential.binding {
                return Err(CredentialError::BindingMismatch);
            }
            let item_id = id.to_string();
            let submitted = AtomicBool::new(false);
            let result = tokio::time::timeout(Duration::from_secs(5), async {
                let service = SecretService::connect(EncryptionType::Dh)
                    .await
                    .map_err(service_error)?;
                let attributes =
                    HashMap::from([("application", "notryx"), ("item_id", item_id.as_str())]);
                let existing = service
                    .search_items(attributes.clone())
                    .await
                    .map_err(service_error)?;
                if !existing.locked.is_empty() {
                    return Err(CredentialError::Unavailable);
                }
                if !existing.unlocked.is_empty() {
                    return Err(CredentialError::Ambiguous);
                }
                let collection = service
                    .get_default_collection()
                    .await
                    .map_err(service_error)?;
                if collection.is_locked().await.map_err(service_error)? {
                    return Err(CredentialError::Unavailable);
                }
                submitted.store(true, Ordering::Release);
                guarded_create(
                    async {
                        collection
                            .create_item(
                                "Notryx device credential",
                                attributes,
                                &credential.token,
                                false,
                                "application/octet-stream",
                            )
                            .await
                            .map_err(service_error)?;
                        Ok(())
                    },
                    Duration::from_secs(5),
                )
                .await
            })
            .await;
            match result {
                Ok(result) => result,
                Err(_) if submitted.load(Ordering::Acquire) => Err(CredentialError::WriteUnknown),
                Err(_) => Err(CredentialError::Unavailable),
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (id, credential);
            Err(CredentialError::Unavailable)
        }
    }

    async fn get(&mut self, id: Uuid) -> Result<Vec<StoredCredential>, CredentialError> {
        #[cfg(target_os = "linux")]
        {
            use secret_service::{EncryptionType, SecretService};
            use std::{collections::HashMap, time::Duration};
            let item_id = id.to_string();
            return tokio::time::timeout(Duration::from_secs(5), async {
                let service = SecretService::connect(EncryptionType::Dh)
                    .await
                    .map_err(service_error)?;
                let found = service
                    .search_items(HashMap::from([
                        ("application", "notryx"),
                        ("item_id", item_id.as_str()),
                    ]))
                    .await
                    .map_err(service_error)?;
                if !found.locked.is_empty() {
                    return Err(CredentialError::Unavailable);
                }
                if found.unlocked.len() > 1 {
                    return Err(CredentialError::Ambiguous);
                }
                let Some(item) = found.unlocked.first() else {
                    return Ok(Vec::new());
                };
                let secret = item.get_secret().await.map_err(service_error)?;
                Ok(vec![decode_credential(secret)?])
            })
            .await
            .map_err(|_| CredentialError::Unavailable)?;
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = id;
            Err(CredentialError::Unavailable)
        }
    }

    async fn delete(
        &mut self,
        id: Uuid,
        binding: &RegistrationBinding,
    ) -> Result<(), CredentialError> {
        #[cfg(target_os = "linux")]
        {
            use secret_service::{EncryptionType, SecretService};
            use std::{
                collections::HashMap,
                sync::atomic::{AtomicBool, Ordering},
                time::Duration,
            };
            let item_id = id.to_string();
            let submitted = AtomicBool::new(false);
            let result = tokio::time::timeout(Duration::from_secs(5), async {
                let service = SecretService::connect(EncryptionType::Dh)
                    .await
                    .map_err(service_error)?;
                let found = service
                    .search_items(HashMap::from([
                        ("application", "notryx"),
                        ("item_id", item_id.as_str()),
                    ]))
                    .await
                    .map_err(service_error)?;
                if !found.locked.is_empty() {
                    return Err(CredentialError::Unavailable);
                }
                if found.unlocked.len() != 1 {
                    return Err(if found.unlocked.is_empty() {
                        CredentialError::Missing
                    } else {
                        CredentialError::Ambiguous
                    });
                }
                let item = &found.unlocked[0];
                let record = decode_credential(item.get_secret().await.map_err(service_error)?)?;
                if &record.binding != binding {
                    return Err(CredentialError::BindingMismatch);
                }
                submitted.store(true, Ordering::Release);
                guarded_create(
                    async {
                        item.delete().await.map_err(service_error)?;
                        Ok(())
                    },
                    Duration::from_secs(5),
                )
                .await
            })
            .await;
            match result {
                Ok(result) => result,
                Err(_) if submitted.load(Ordering::Acquire) => Err(CredentialError::WriteUnknown),
                Err(_) => Err(CredentialError::Unavailable),
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (id, binding);
            Err(CredentialError::Unavailable)
        }
    }
}

async fn stage_pair<R: CredentialRegistry, V: CredentialVault>(
    store: &mut R,
    vault: &mut V,
    credential: &StoredCredential,
) -> Result<CredentialMode, CredentialError> {
    let pending = store.prepare(&credential.binding, Uuid::new_v4()).await?;
    let mode = match vault.put(pending.id, credential).await {
        Ok(()) => {
            let verified = select_one(vault.get(pending.id).await?, &pending.binding)?;
            if verified.token != credential.token {
                return Err(CredentialError::BindingMismatch);
            }
            CredentialMode::Stored
        }
        Err(CredentialError::Unavailable) => CredentialMode::SessionOnly,
        Err(error) => return Err(error),
    };
    store
        .commit(&pending, mode == CredentialMode::Stored)
        .await?;
    Ok(mode)
}

async fn restore_active<R: CredentialRegistry, V: CredentialVault>(
    store: &mut R,
    vault: &mut V,
) -> Result<Option<StoredCredential>, CredentialError> {
    let Some((binding, Some(id))) = store.active().await? else {
        return Ok(None);
    };
    let credential = select_one(vault.get(id).await?, &binding)?;
    if store.active().await? != Some((binding, Some(id))) {
        return Err(CredentialError::BindingMismatch);
    }
    Ok(Some(credential))
}

#[cfg(test)]
async fn pending_credentials<R: CredentialRegistry>(
    store: &mut R,
) -> Result<Vec<PendingCredential>, CredentialError> {
    Ok(store.pending().await?.into_iter().collect())
}

#[cfg(test)]
async fn adopt_pending<R: CredentialRegistry, V: CredentialVault>(
    store: &mut R,
    vault: &mut V,
    pending: &PendingCredential,
) -> Result<(), CredentialError> {
    if store.pending().await?.as_ref() != Some(pending) {
        return Err(CredentialError::BindingMismatch);
    }
    select_one(vault.get(pending.id).await?, &pending.binding)?;
    store.commit(pending, true).await
}

async fn finish_existing<R: CredentialRegistry, V: CredentialVault>(
    store: &mut R,
    vault: &mut V,
    pending: &PendingCredential,
    record: &StoredCredential,
) -> Result<CredentialMode, CredentialError> {
    if pending.binding != record.binding {
        return Err(CredentialError::BindingMismatch);
    }
    let mode = match vault.get(pending.id).await {
        Ok(records) if records.is_empty() => match vault.put(pending.id, record).await {
            Ok(()) => {
                let verified = select_one(vault.get(pending.id).await?, &pending.binding)?;
                if verified.token != record.token {
                    return Err(CredentialError::BindingMismatch);
                }
                CredentialMode::Stored
            }
            Err(CredentialError::Unavailable) => return Err(CredentialError::Unavailable),
            Err(error) => return Err(error),
        },
        Ok(records) => {
            let verified = select_one(records, &pending.binding)?;
            if verified.token != record.token {
                return Err(CredentialError::BindingMismatch);
            }
            CredentialMode::Stored
        }
        Err(CredentialError::Unavailable) => return Err(CredentialError::Unavailable),
        Err(error) => return Err(error),
    };
    store
        .commit(pending, mode == CredentialMode::Stored)
        .await?;
    Ok(mode)
}

async fn discard_pending<R: CredentialRegistry, V: CredentialVault>(
    store: &mut R,
    vault: &mut V,
    pending: &PendingCredential,
) -> Result<(), CredentialError> {
    if store.pending().await?.as_ref() != Some(pending) {
        return Err(CredentialError::BindingMismatch);
    }
    let records = vault.get(pending.id).await?;
    if !records.is_empty() {
        select_one(records, &pending.binding)?;
        if store.pending().await?.as_ref() != Some(pending) {
            return Err(CredentialError::BindingMismatch);
        }
        vault.delete(pending.id, &pending.binding).await?;
        if !vault.get(pending.id).await?.is_empty() {
            return Err(CredentialError::WriteUnknown);
        }
    }
    store.discard(pending).await
}

#[cfg(test)]
struct TestUnavailableVault;

#[cfg(test)]
#[derive(Default)]
struct TestMemoryVault(std::collections::HashMap<Uuid, StoredCredential>);

#[cfg(test)]
tokio::task_local! { static USE_MEMORY_VAULT: (); }

#[cfg(test)]
pub(crate) async fn with_test_vault<F: std::future::Future>(future: F) -> F::Output {
    USE_MEMORY_VAULT.scope((), future).await
}

#[cfg(test)]
impl CredentialVault for TestMemoryVault {
    async fn put(
        &mut self,
        id: Uuid,
        credential: &StoredCredential,
    ) -> Result<(), CredentialError> {
        self.0.insert(id, credential.clone());
        Ok(())
    }
    async fn get(&mut self, id: Uuid) -> Result<Vec<StoredCredential>, CredentialError> {
        Ok(self.0.get(&id).cloned().into_iter().collect())
    }
}

#[cfg(test)]
pub(crate) async fn install_paired_for_test(
    runtime: &crate::Runtime,
    snapshot: &crate::gate::PairSnapshot,
    session: Arc<Session>,
) -> Result<(), String> {
    install_paired_with_vault(runtime, snapshot, session, &mut TestMemoryVault::default()).await
}

#[cfg(test)]
impl CredentialVault for TestUnavailableVault {
    async fn put(
        &mut self,
        _id: Uuid,
        _credential: &StoredCredential,
    ) -> Result<(), CredentialError> {
        Err(CredentialError::Unavailable)
    }
    async fn get(&mut self, _id: Uuid) -> Result<Vec<StoredCredential>, CredentialError> {
        Err(CredentialError::Unavailable)
    }
}

pub(crate) async fn install_paired(
    runtime: &crate::Runtime,
    snapshot: &crate::gate::PairSnapshot,
    session: Arc<Session>,
) -> Result<(), String> {
    #[cfg(test)]
    if USE_MEMORY_VAULT.try_with(|_| ()).is_ok() {
        return install_paired_with_vault(
            runtime,
            snapshot,
            session,
            &mut TestMemoryVault::default(),
        )
        .await;
    }
    #[cfg(test)]
    let mut vault = TestUnavailableVault;
    #[cfg(not(test))]
    let mut vault = PlatformVault;
    install_paired_with_vault(runtime, snapshot, session, &mut vault).await
}

async fn install_paired_with_vault<V: CredentialVault>(
    runtime: &crate::Runtime,
    snapshot: &crate::gate::PairSnapshot,
    session: Arc<Session>,
    vault: &mut V,
) -> Result<(), String> {
    let io = runtime.0.lock().await.credential_io.clone();
    let _guard = io.lock().await;
    let record = session_credential(&session).map_err(|_| "pairing_pending_adoption")?;
    let mut registry = PairRegistry {
        runtime,
        snapshot,
        session,
    };
    let pending = registry
        .pending()
        .await
        .map_err(|_| "pairing_pending_adoption")?;
    if let Some(pending) = pending {
        finish_existing(&mut registry, vault, &pending, &record)
            .await
            .map_err(|_| "pairing_pending_adoption")?;
    } else {
        stage_pair(&mut registry, vault, &record)
            .await
            .map_err(|_| "pairing_pending_adoption")?;
    }
    Ok(())
}

async fn activate_verified_session(
    runtime: &crate::Runtime,
    expected: &(RegistrationBinding, Option<Uuid>),
    session: &Arc<Session>,
) -> Result<bool, notryx_client_runtime::Error> {
    if crate::registration_binding(session.binding()) != expected.0 {
        return Err(notryx_client_runtime::Error::Binding);
    }
    session.verify_binding().await?;
    let mut state = runtime.0.lock().await;
    if state
        .store
        .active_credential()
        .await
        .ok()
        .flatten()
        .as_ref()
        != Some(expected)
        || !matches!(state.store.pending_credential().await, Ok(None))
        || state.session.is_some()
        || state.pending_pair.is_some()
        || state.gate.check_queue(state.gate.owner()).is_err()
    {
        return Ok(false);
    }
    let Some(token) = state.transmission_gate.begin_initial_sync(&expected.0) else {
        return Ok(false);
    };
    state.initial_sync = Some(token);
    state.sync_state = Some(notryx_protocol::WorkspaceSyncState {
        binding: crate::to_contract(expected.0.clone()),
        status: notryx_protocol::WorkspaceSyncStatus::Connecting,
    });
    state.session = Some(session.clone());
    state.credential_identity_unsupported = None;
    Ok(true)
}

async fn mark_identity_unsupported(
    runtime: &crate::Runtime,
    expected: &(RegistrationBinding, Option<Uuid>),
) -> bool {
    let mut state = runtime.0.lock().await;
    if state
        .store
        .active_credential()
        .await
        .ok()
        .flatten()
        .as_ref()
        != Some(expected)
        || !matches!(state.store.pending_credential().await, Ok(None))
        || state.session.is_some()
        || state.pending_pair.is_some()
        || state.gate.check_queue(state.gate.owner()).is_err()
    {
        return false;
    }
    let (binding, Some(id)) = expected else {
        return false;
    };
    state.credential_identity_unsupported = Some((binding.clone(), *id));
    true
}

pub(crate) async fn resume(runtime: &crate::Runtime, app: tauri::AppHandle) {
    let io = runtime.0.lock().await.credential_io.clone();
    let guard = io.lock().await;
    let mut registry = ActiveRegistry(runtime);
    let Ok(None) = registry.pending().await else {
        return;
    };
    let Ok(Some(expected @ (_, Some(_)))) = registry.active().await else {
        return;
    };
    let mut vault = PlatformVault;
    let Ok(Some(record)) = restore_active(&mut registry, &mut vault).await else {
        return;
    };
    if record.binding != expected.0 {
        return;
    }
    let Ok(credential) =
        serde_json::from_slice::<notryx_client_runtime::StoredCredential>(&record.token)
    else {
        return;
    };
    let Ok(session) = Session::from_stored_credential(credential.binding(), &credential) else {
        return;
    };
    let session = Arc::new(session);
    drop(guard);
    loop {
        match activate_verified_session(runtime, &expected, &session).await {
            Ok(true) => break,
            Ok(false) => return,
            Err(
                notryx_client_runtime::Error::Network | notryx_client_runtime::Error::Unavailable,
            ) => {
                {
                    let mut state = runtime.0.lock().await;
                    if state
                        .store
                        .active_credential()
                        .await
                        .ok()
                        .flatten()
                        .as_ref()
                        != Some(&expected)
                        || !matches!(state.store.pending_credential().await, Ok(None))
                        || state.session.is_some()
                        || state.pending_pair.is_some()
                        || state.gate.check_queue(state.gate.owner()).is_err()
                    {
                        return;
                    }
                }
                tokio::time::sleep(std::time::Duration::from_secs(10)).await;
            }
            Err(notryx_client_runtime::Error::IdentityUnsupported) => {
                if mark_identity_unsupported(runtime, &expected).await {
                    use tauri::Emitter;
                    let _ = app.emit_to("main", "workspace-credential-status-changed", ());
                }
                return;
            }
            Err(_) => return,
        }
    }
    use tauri::Emitter;
    let _ = app.emit_to("main", "workspace-sync-status-changed", ());
    let _ = app.emit_to("main", "workspace-credential-status-changed", ());
    crate::sync_lifecycle::start_background_sync(runtime, session, Some(app)).await;
}

pub(crate) async fn recover_pending(runtime: &crate::Runtime, owner: &str) -> Result<(), String> {
    let io = runtime.0.lock().await.credential_io.clone();
    let guard = io.lock().await;
    let mut registry = ActiveRegistry(runtime);
    let pending = registry
        .pending()
        .await
        .map_err(|_| "pairing_pending_adoption")?
        .ok_or("no_pending_pair")?;
    let mut vault = PlatformVault;
    let record = select_one(
        vault
            .get(pending.id)
            .await
            .map_err(|_| "credential_unavailable")?,
        &pending.binding,
    )
    .map_err(|_| "credential_unavailable")?;
    let credential: notryx_client_runtime::StoredCredential =
        serde_json::from_slice(&record.token).map_err(|_| "credential_unavailable")?;
    let session = Arc::new(
        Session::from_stored_credential(credential.binding(), &credential)
            .map_err(|_| "credential_unavailable")?,
    );
    drop(guard);
    session
        .verify_binding()
        .await
        .map_err(|_| "credential_unavailable")?;
    let mut state = runtime.0.lock().await;
    state.gate.snapshot(owner).map_err(str::to_owned)?;
    if state
        .store
        .pending_credential()
        .await
        .map_err(|_| "pairing_pending_adoption")?
        .as_ref()
        != Some(&pending)
        || state.pending_pair.is_some()
    {
        return Err("stale_registration".into());
    }
    state.gate.begin_pair(owner).map_err(str::to_owned)?;
    state.pending_pair = Some(session);
    Ok(())
}

pub(crate) async fn discard_for_renderer(
    runtime: &crate::Runtime,
    owner: &str,
) -> Result<(), String> {
    let mut vault = PlatformVault;
    discard_for_renderer_with_vault(runtime, owner, &mut vault).await
}

async fn discard_for_renderer_with_vault<V: CredentialVault>(
    runtime: &crate::Runtime,
    owner: &str,
    vault: &mut V,
) -> Result<(), String> {
    let (io, snapshot) = {
        let mut state = runtime.0.lock().await;
        let request_snapshot = state.gate.snapshot(owner).map_err(str::to_owned)?;
        if state
            .store
            .pending_credential()
            .await
            .map_err(|error| error.to_string())?
            .is_none()
        {
            return Err("no_pending_pair".into());
        }
        if state.pending_pair.is_some() {
            let snapshot = state.gate.adopt_pair(owner).map_err(str::to_owned)?;
            state.gate.fail_pair(&snapshot).map_err(str::to_owned)?;
            state.pending_pair = None;
        }
        (state.credential_io.clone(), request_snapshot)
    };
    let _guard = io.lock().await;
    let mut registry = OwnerRegistry { runtime, snapshot };
    let pending = registry
        .pending()
        .await
        .map_err(|_| "pairing_pending_adoption")?
        .ok_or("no_pending_pair")?;
    {
        let state = runtime.0.lock().await;
        state.gate.snapshot(owner).map_err(str::to_owned)?;
    }
    discard_pending(&mut registry, vault, &pending)
        .await
        .map_err(|_| "credential_unavailable".to_owned())
}

#[cfg(test)]
#[path = "credential_native_tests.rs"]
mod tests;
