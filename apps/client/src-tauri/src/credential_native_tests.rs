use super::*;
use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
use std::collections::HashMap;

const TEST_TOKEN: &[u8] = b"synthetic-secret-service-token-4bcaebd2";

#[derive(Default)]
struct FakeVault {
    items: HashMap<Uuid, Vec<StoredCredential>>,
    unavailable: bool,
    lose_after_write: bool,
    lose_after_delete: bool,
}

impl CredentialVault for FakeVault {
    async fn put(
        &mut self,
        id: Uuid,
        credential: &StoredCredential,
    ) -> Result<(), CredentialError> {
        if self.unavailable {
            return Err(CredentialError::Unavailable);
        }
        if self.lose_after_write {
            return guarded_create(
                async {
                    self.items.entry(id).or_default().push(credential.clone());
                    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
                    Ok(())
                },
                std::time::Duration::from_millis(1),
            )
            .await;
        }
        self.items.entry(id).or_default().push(credential.clone());
        Ok(())
    }

    async fn get(&mut self, id: Uuid) -> Result<Vec<StoredCredential>, CredentialError> {
        if self.unavailable {
            return Err(CredentialError::Unavailable);
        }
        Ok(self.items.get(&id).cloned().unwrap_or_default())
    }

    async fn delete(
        &mut self,
        id: Uuid,
        binding: &RegistrationBinding,
    ) -> Result<(), CredentialError> {
        if self.unavailable {
            return Err(CredentialError::Unavailable);
        }
        let records = self.items.get(&id).cloned().unwrap_or_default();
        select_one(records, binding)?;
        self.items.remove(&id);
        if self.lose_after_delete {
            return Err(CredentialError::WriteUnknown);
        }
        Ok(())
    }
}

fn binding() -> RegistrationBinding {
    RegistrationBinding {
        server_origin: "https://workspace.example".into(),
        workspace_id: Uuid::new_v4().to_string(),
        workspace_epoch: Uuid::new_v4().to_string(),
        device_id: Uuid::new_v4().to_string(),
        device_stream_id: Uuid::new_v4().to_string(),
    }
}

async fn fixture() -> (
    tempfile::TempDir,
    ClientStore,
    RegistrationBinding,
    StoredCredential,
) {
    let directory = tempfile::tempdir().unwrap();
    let mut store = ClientStore::open(directory.path().join("client.sqlite"))
        .await
        .unwrap();
    let old = binding();
    store.activate_registration(&old).await.unwrap();
    let opened = store
        .open_registered_document(&old, "document", b"old base", "old-revision")
        .await
        .unwrap();
    store
        .save_registered_checkpoint(
            &old,
            "document",
            &opened.edit_session_id,
            1,
            b"pending draft",
        )
        .await
        .unwrap();
    store
        .close_registered_document(&old, "document", &opened.edit_session_id)
        .await
        .unwrap();
    store.queue_checkpoint(&old, "document", 1).await.unwrap();
    let next = StoredCredential {
        binding: RegistrationBinding {
            device_id: Uuid::new_v4().to_string(),
            device_stream_id: Uuid::new_v4().to_string(),
            ..old.clone()
        },
        token: TEST_TOKEN.to_vec(),
    };
    (directory, store, old, next)
}

async fn assert_old_preserved(store: &mut ClientStore, old: &RegistrationBinding, frozen: bool) {
    let draft = store
        .read_registered_checkpoint(old, "document")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.bytes, b"pending draft");
    assert_eq!(draft.base_bytes, b"old base");
    assert_eq!(draft.base_revision, "old-revision");
    let outbox = store.list_outbox(old, 0, 100).await.unwrap();
    assert_eq!(outbox.entries.len(), 1);
    assert_eq!(outbox.entries[0].frozen, frozen);
}

#[tokio::test]
async fn credential_secure_pair_survives_restart_without_putting_token_in_sqlite() {
    let (directory, mut store, old, next) = fixture().await;
    let mut vault = FakeVault::default();
    assert_eq!(
        stage_pair(&mut store, &mut vault, &next).await.unwrap(),
        CredentialMode::Stored
    );
    assert_eq!(
        store.current_registration().await.unwrap(),
        Some(next.binding.clone())
    );
    assert_old_preserved(&mut store, &old, true).await;
    store.close().await.unwrap();
    let bytes = std::fs::read(directory.path().join("client.sqlite")).unwrap();
    assert!(
        !bytes
            .windows(TEST_TOKEN.len())
            .any(|value| value == TEST_TOKEN)
    );
    let mut store = ClientStore::open(directory.path().join("client.sqlite"))
        .await
        .unwrap();
    let restored = restore_active(&mut store, &mut vault)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(restored.binding, next.binding);
    assert_eq!(restored.token, TEST_TOKEN);
    assert_old_preserved(&mut store, &old, true).await;
}

#[tokio::test]
async fn credential_unavailable_store_is_explicit_session_only_and_preserves_data() {
    let (directory, mut store, old, next) = fixture().await;
    let mut vault = FakeVault {
        unavailable: true,
        ..Default::default()
    };
    assert_eq!(
        stage_pair(&mut store, &mut vault, &next).await.unwrap(),
        CredentialMode::SessionOnly
    );
    assert_eq!(
        store.current_registration().await.unwrap(),
        Some(next.binding)
    );
    assert_old_preserved(&mut store, &old, true).await;
    assert!(vault.items.is_empty());
    store.close().await.unwrap();
    let mut store = ClientStore::open(directory.path().join("client.sqlite"))
        .await
        .unwrap();
    assert!(
        restore_active(&mut store, &mut vault)
            .await
            .unwrap()
            .is_none()
    );
    assert_old_preserved(&mut store, &old, true).await;
}

#[tokio::test]
async fn credential_sqlite_activation_failure_keeps_old_registration_and_pending_secret() {
    let (directory, mut store, old, next) = fixture().await;
    let path = directory.path().join("client.sqlite");
    let mut sabotage = SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(&path))
        .await
        .unwrap();
    sqlx::query("CREATE TRIGGER fail_credential_activation BEFORE INSERT ON client_registrations BEGIN SELECT RAISE(ABORT, 'synthetic'); END").execute(&mut sabotage).await.unwrap();
    let mut vault = FakeVault::default();
    assert_eq!(
        stage_pair(&mut store, &mut vault, &next).await,
        Err(CredentialError::Storage)
    );
    assert_eq!(
        store.current_registration().await.unwrap(),
        Some(old.clone())
    );
    assert_old_preserved(&mut store, &old, false).await;
    let pending = pending_credentials(&mut store).await.unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].binding, next.binding);
    assert_eq!(vault.get(pending[0].id).await.unwrap().len(), 1);
    sqlx::query("DROP TRIGGER fail_credential_activation")
        .execute(&mut sabotage)
        .await
        .unwrap();
    sabotage.close().await.unwrap();
    store.close().await.unwrap();
    let mut store = ClientStore::open(&path).await.unwrap();
    let recovered = pending_credentials(&mut store).await.unwrap();
    assert_eq!(recovered, pending);
    adopt_pending(&mut store, &mut vault, &recovered[0])
        .await
        .unwrap();
    assert_eq!(
        store.current_registration().await.unwrap(),
        Some(next.binding)
    );
    assert_old_preserved(&mut store, &old, true).await;
}

#[tokio::test]
async fn credential_binding_mismatch_or_duplicate_never_restores_session() {
    let (_directory, mut store, old, next) = fixture().await;
    let mut vault = FakeVault::default();
    assert_eq!(
        stage_pair(&mut store, &mut vault, &next).await.unwrap(),
        CredentialMode::Stored
    );
    let values = vault.items.values_mut().next().unwrap();
    values[0].binding.workspace_epoch = Uuid::new_v4().to_string();
    assert!(matches!(
        restore_active(&mut store, &mut vault).await,
        Err(CredentialError::BindingMismatch)
    ));
    assert_old_preserved(&mut store, &old, true).await;
    let values = vault.items.values_mut().next().unwrap();
    values[0] = next.clone();
    values.push(next);
    assert!(matches!(
        restore_active(&mut store, &mut vault).await,
        Err(CredentialError::Ambiguous)
    ));
    assert_old_preserved(&mut store, &old, true).await;
}

#[tokio::test]
async fn credential_lost_write_response_preserves_pending_and_is_recoverable() {
    let (directory, mut store, old, next) = fixture().await;
    let mut vault = FakeVault {
        lose_after_write: true,
        ..Default::default()
    };
    assert_eq!(
        stage_pair(&mut store, &mut vault, &next).await,
        Err(CredentialError::WriteUnknown)
    );
    assert_eq!(
        store.current_registration().await.unwrap(),
        Some(old.clone())
    );
    assert_old_preserved(&mut store, &old, false).await;
    let pending = pending_credentials(&mut store).await.unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(vault.items.get(&pending[0].id).unwrap().len(), 1);
    store.close().await.unwrap();
    let mut store = ClientStore::open(directory.path().join("client.sqlite"))
        .await
        .unwrap();
    vault.lose_after_write = false;
    adopt_pending(&mut store, &mut vault, &pending[0])
        .await
        .unwrap();
    assert_eq!(
        store.current_registration().await.unwrap(),
        Some(next.binding)
    );
    assert_old_preserved(&mut store, &old, true).await;
}

#[tokio::test]
async fn credential_unavailable_existing_pending_is_not_discarded_or_activated() {
    let (_directory, mut store, old, next) = fixture().await;
    let pending = store
        .prepare_credential_pending(&next.binding, Uuid::new_v4())
        .await
        .unwrap();
    let mut vault = FakeVault {
        unavailable: true,
        ..Default::default()
    };
    assert_eq!(
        finish_existing(&mut store, &mut vault, &pending, &next).await,
        Err(CredentialError::Unavailable)
    );
    assert_eq!(
        store.current_registration().await.unwrap(),
        Some(old.clone())
    );
    assert_eq!(store.pending_credential().await.unwrap(), Some(pending));
    assert_old_preserved(&mut store, &old, false).await;
}

#[tokio::test]
async fn credential_discard_removes_only_confirmed_pending_item() {
    let (_directory, mut store, old, next) = fixture().await;
    let pending = store
        .prepare_credential_pending(&next.binding, Uuid::new_v4())
        .await
        .unwrap();
    let mut vault = FakeVault::default();
    vault.put(pending.id, &next).await.unwrap();
    let unrelated_id = Uuid::new_v4();
    let unrelated = StoredCredential {
        binding: old.clone(),
        token: b"another synthetic token".to_vec(),
    };
    vault.put(unrelated_id, &unrelated).await.unwrap();
    discard_pending(&mut store, &mut vault, &pending)
        .await
        .unwrap();
    assert!(!vault.items.contains_key(&pending.id));
    assert_eq!(vault.items.get(&unrelated_id).unwrap().len(), 1);
    assert!(store.pending_credential().await.unwrap().is_none());
    assert_eq!(
        store.current_registration().await.unwrap(),
        Some(old.clone())
    );
    assert_old_preserved(&mut store, &old, false).await;
}

#[tokio::test]
async fn credential_discard_lost_delete_response_keeps_pending_until_verified() {
    let (_directory, mut store, old, next) = fixture().await;
    let pending = store
        .prepare_credential_pending(&next.binding, Uuid::new_v4())
        .await
        .unwrap();
    let mut vault = FakeVault {
        lose_after_delete: true,
        ..Default::default()
    };
    vault.put(pending.id, &next).await.unwrap();
    assert_eq!(
        discard_pending(&mut store, &mut vault, &pending).await,
        Err(CredentialError::WriteUnknown)
    );
    assert_eq!(
        store.pending_credential().await.unwrap(),
        Some(pending.clone())
    );
    assert_eq!(
        store.current_registration().await.unwrap(),
        Some(old.clone())
    );
    assert_old_preserved(&mut store, &old, false).await;
    vault.lose_after_delete = false;
    discard_pending(&mut store, &mut vault, &pending)
        .await
        .unwrap();
    assert!(store.pending_credential().await.unwrap().is_none());
}

#[tokio::test]
async fn credential_pending_blocks_new_pair_before_pairing_secret_is_consumed() {
    use notryx_protocol::{AdminRequest, AdminResponse, AdminResult};
    let (directory, server, runtime, session, document) =
        crate::sync_bootstrap::tests::setup().await;
    let old = crate::registration_binding(session.binding());
    let next = RegistrationBinding {
        device_id: Uuid::new_v4().to_string(),
        device_stream_id: Uuid::new_v4().to_string(),
        ..old.clone()
    };
    {
        let mut state = runtime.0.lock().await;
        state
            .store
            .prepare_credential_pending(&next, Uuid::new_v4())
            .await
            .unwrap();
    }
    let socket = directory.path().join("admin.sock");
    let AdminResponse::Ok {
        result: AdminResult::Status {
            owner_principal_id, ..
        },
    } = notryx_server::admin::request(&socket, &AdminRequest::Status {})
        .await
        .unwrap()
    else {
        panic!("synthetic status");
    };
    let AdminResponse::Ok {
        result:
            AdminResult::Pairing {
                workspace_id,
                pairing_secret,
                ..
            },
    } = notryx_server::admin::request(
        &socket,
        &AdminRequest::IssuePairing {
            principal_id: owner_principal_id,
        },
    )
    .await
    .unwrap()
    else {
        panic!("synthetic pairing grant");
    };
    assert!(
        matches!(crate::begin_workspace_pair(&runtime, "owner").await, Err(error) if error == "pairing_pending_adoption")
    );
    assert!(
        Session::pair(&old.server_origin, workspace_id, &pairing_secret)
            .await
            .is_ok()
    );
    let mut state = runtime.0.lock().await;
    assert_eq!(
        state.store.current_registration().await.unwrap(),
        Some(old.clone())
    );
    assert!(state.store.pending_credential().await.unwrap().is_some());
    assert_eq!(
        state
            .store
            .read_registered_checkpoint(&old, &document)
            .await
            .unwrap()
            .unwrap()
            .bytes,
        b"local draft"
    );
    assert_eq!(
        state
            .store
            .list_outbox(&old, 0, 100)
            .await
            .unwrap()
            .entries
            .len(),
        1
    );
    drop(state);
    server.shutdown().await.unwrap();
}

#[derive(Clone)]
struct BlockingVault {
    items: std::sync::Arc<tokio::sync::Mutex<HashMap<Uuid, StoredCredential>>>,
    written: std::sync::Arc<tokio::sync::Notify>,
    resume: std::sync::Arc<tokio::sync::Notify>,
    block_put: bool,
}

impl CredentialVault for BlockingVault {
    async fn put(
        &mut self,
        id: Uuid,
        credential: &StoredCredential,
    ) -> Result<(), CredentialError> {
        self.items.lock().await.insert(id, credential.clone());
        if self.block_put {
            self.written.notify_one();
            self.resume.notified().await;
        }
        Ok(())
    }
    async fn get(&mut self, id: Uuid) -> Result<Vec<StoredCredential>, CredentialError> {
        Ok(self
            .items
            .lock()
            .await
            .get(&id)
            .cloned()
            .into_iter()
            .collect())
    }
    async fn delete(
        &mut self,
        id: Uuid,
        binding: &RegistrationBinding,
    ) -> Result<(), CredentialError> {
        let mut items = self.items.lock().await;
        if items
            .get(&id)
            .is_none_or(|record| &record.binding != binding)
        {
            return Err(CredentialError::BindingMismatch);
        }
        items.remove(&id);
        Ok(())
    }
}

#[tokio::test]
async fn credential_discard_during_vault_write_keeps_runtime_responsive_and_old_outbox() {
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::sync::{Mutex, Notify};
    let (_directory, store, old, next) = fixture().await;
    let value = serde_json::json!({"binding": {"origin": next.binding.server_origin, "workspace_id": next.binding.workspace_id, "workspace_epoch": next.binding.workspace_epoch, "device_id": next.binding.device_id, "device_stream_id": next.binding.device_stream_id}, "token":"synthetic-pair-token"});
    let credential: notryx_client_runtime::StoredCredential =
        serde_json::from_value(value).unwrap();
    let session =
        Arc::new(Session::from_stored_credential(credential.binding(), &credential).unwrap());
    let mut state = crate::RuntimeState::new(store, "owner".into());
    let pair = state.gate.begin_pair("owner").unwrap();
    state.pending_pair = Some(session.clone());
    let runtime = crate::Runtime(Arc::new(Mutex::new(state)));
    let items = Arc::new(Mutex::new(HashMap::new()));
    let written = Arc::new(Notify::new());
    let resume = Arc::new(Notify::new());
    let vault = BlockingVault {
        items: items.clone(),
        written: written.clone(),
        resume: resume.clone(),
        block_put: true,
    };
    let worker_runtime = crate::Runtime(runtime.0.clone());
    let mut worker_vault = vault.clone();
    let writer = tokio::spawn(async move {
        install_paired_with_vault(&worker_runtime, &pair, session, &mut worker_vault).await
    });
    tokio::time::timeout(Duration::from_secs(2), written.notified())
        .await
        .unwrap();
    {
        let mut state = tokio::time::timeout(Duration::from_secs(1), runtime.0.lock())
            .await
            .expect("vault I/O must not hold the runtime lock");
        assert_eq!(
            state.store.current_registration().await.unwrap(),
            Some(old.clone())
        );
        assert_old_preserved(&mut state.store, &old, false).await;
    }
    let discard_runtime = crate::Runtime(runtime.0.clone());
    let mut discard_vault = BlockingVault {
        block_put: false,
        ..vault
    };
    let discard = tokio::spawn(async move {
        discard_for_renderer_with_vault(&discard_runtime, "owner", &mut discard_vault).await
    });
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if runtime.0.lock().await.pending_pair.is_none() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    resume.notify_one();
    assert!(writer.await.unwrap().is_err());
    discard.await.unwrap().unwrap();
    let mut state = runtime.0.lock().await;
    assert!(state.store.pending_credential().await.unwrap().is_none());
    assert_eq!(
        state.store.current_registration().await.unwrap(),
        Some(old.clone())
    );
    assert_old_preserved(&mut state.store, &old, false).await;
    assert!(items.lock().await.is_empty());
}

#[tokio::test]
async fn credential_wrong_device_token_never_activates_restored_session() {
    use notryx_protocol::{AdminRequest, AdminResponse, AdminResult};
    use std::sync::Arc;
    let (directory, server, runtime, first, document) = crate::sync_bootstrap::tests::setup().await;
    let old = crate::registration_binding(first.binding());
    let socket = directory.path().join("admin.sock");
    let AdminResponse::Ok {
        result: AdminResult::Status {
            owner_principal_id, ..
        },
    } = notryx_server::admin::request(&socket, &AdminRequest::Status {})
        .await
        .unwrap()
    else {
        panic!("synthetic status");
    };
    let AdminResponse::Ok {
        result:
            AdminResult::Pairing {
                workspace_id,
                pairing_secret,
                ..
            },
    } = notryx_server::admin::request(
        &socket,
        &AdminRequest::IssuePairing {
            principal_id: owner_principal_id,
        },
    )
    .await
    .unwrap()
    else {
        panic!("synthetic pairing grant");
    };
    let second = Session::pair(&old.server_origin, workspace_id, &pairing_secret)
        .await
        .unwrap();
    let record = session_credential(&second).unwrap();
    let mut vault = FakeVault::default();
    let expected = {
        let mut state = runtime.0.lock().await;
        stage_pair(&mut state.store, &mut vault, &record)
            .await
            .unwrap();
        state.session = None;
        state.store.active_credential().await.unwrap().unwrap()
    };
    let first_saved = serde_json::to_value(first.stored_credential().unwrap()).unwrap();
    let mut forged = serde_json::to_value(second.stored_credential().unwrap()).unwrap();
    forged["token"] = first_saved["token"].clone();
    let fake_record = vault
        .items
        .values_mut()
        .next()
        .unwrap()
        .first_mut()
        .unwrap();
    fake_record.token = serde_json::to_vec(&forged).unwrap();
    let saved = {
        let mut state = runtime.0.lock().await;
        restore_active(&mut state.store, &mut vault)
            .await
            .unwrap()
            .unwrap()
    };
    let stored: notryx_client_runtime::StoredCredential =
        serde_json::from_slice(&saved.token).unwrap();
    let restored = Arc::new(Session::from_stored_credential(stored.binding(), &stored).unwrap());
    assert_eq!(
        activate_verified_session(&runtime, &expected, &restored).await,
        Err(notryx_client_runtime::Error::Binding)
    );
    let mut state = runtime.0.lock().await;
    assert!(state.session.is_none());
    assert_eq!(
        state
            .store
            .read_registered_checkpoint(&old, &document)
            .await
            .unwrap()
            .unwrap()
            .bytes,
        b"local draft"
    );
    assert!(state.store.list_outbox(&old, 0, 100).await.unwrap().entries[0].frozen);
    assert_eq!(
        std::fs::read(directory.path().join("notes/example.md")).unwrap(),
        b"server source\n"
    );
    drop(state);
    let valid = Arc::new(second);
    assert!(
        activate_verified_session(&runtime, &expected, &valid)
            .await
            .unwrap()
    );
    let state = runtime.0.lock().await;
    assert!(state.session.is_some());
    assert!(state.transmission_gate.blocked(&expected.0));
    drop(state);
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn credential_identity_response_cannot_activate_after_registration_changes() {
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::sync::{Mutex, oneshot};
    let (_directory, mut store, old, mut next) = fixture().await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    next.binding.server_origin = format!("http://{}", listener.local_addr().unwrap());
    let (started_tx, started_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    let identity = serde_json::json!({"protocol":"mdw-sync/1.2", "workspace_id":next.binding.workspace_id, "workspace_epoch":next.binding.workspace_epoch, "device_id":next.binding.device_id, "device_stream_id":next.binding.device_stream_id}).to_string();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        let mut chunk = [0u8; 1024];
        while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
            let read = stream.read(&mut chunk).await.unwrap();
            assert!(read > 0 && request.len() < 16384);
            request.extend_from_slice(&chunk[..read]);
        }
        let _ = started_tx.send(());
        let _ = release_rx.await;
        let header = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            identity.len()
        );
        stream.write_all(header.as_bytes()).await.unwrap();
        stream.write_all(identity.as_bytes()).await.unwrap();
    });
    let mut vault = FakeVault::default();
    stage_pair(&mut store, &mut vault, &next).await.unwrap();
    let expected = store.active_credential().await.unwrap().unwrap();
    let json = serde_json::json!({"binding":{"origin":next.binding.server_origin,"workspace_id":next.binding.workspace_id,"workspace_epoch":next.binding.workspace_epoch,"device_id":next.binding.device_id,"device_stream_id":next.binding.device_stream_id},"token":"synthetic-token"});
    let stored: notryx_client_runtime::StoredCredential = serde_json::from_value(json).unwrap();
    let session = Arc::new(Session::from_stored_credential(stored.binding(), &stored).unwrap());
    let runtime = crate::Runtime(Arc::new(Mutex::new(crate::RuntimeState::new(
        store,
        "owner".into(),
    ))));
    let worker_runtime = crate::Runtime(runtime.0.clone());
    let worker_expected = expected.clone();
    let worker = tokio::spawn(async move {
        activate_verified_session(&worker_runtime, &worker_expected, &session).await
    });
    started_rx.await.unwrap();
    let replacement = RegistrationBinding {
        device_id: Uuid::new_v4().to_string(),
        device_stream_id: Uuid::new_v4().to_string(),
        ..next.binding
    };
    {
        let mut state = runtime.0.lock().await;
        state
            .store
            .activate_registration(&replacement)
            .await
            .unwrap();
    }
    release_tx.send(()).unwrap();
    assert!(!worker.await.unwrap().unwrap());
    server.await.unwrap();
    let mut state = runtime.0.lock().await;
    assert!(state.session.is_none());
    assert_eq!(
        state.store.current_registration().await.unwrap(),
        Some(replacement)
    );
    assert_old_preserved(&mut state.store, &old, true).await;
}

#[tokio::test]
async fn credential_missing_identity_endpoint_is_reported_without_restoring_session() {
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::sync::Mutex;
    let (_directory, mut store, old, mut next) = fixture().await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    next.binding.server_origin = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = [0u8; 2048];
        assert!(stream.read(&mut request).await.unwrap() > 0);
        stream
            .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
    });
    let mut vault = FakeVault::default();
    stage_pair(&mut store, &mut vault, &next).await.unwrap();
    let expected = store.active_credential().await.unwrap().unwrap();
    let json = serde_json::json!({"binding":{"origin":next.binding.server_origin,"workspace_id":next.binding.workspace_id,"workspace_epoch":next.binding.workspace_epoch,"device_id":next.binding.device_id,"device_stream_id":next.binding.device_stream_id},"token":"synthetic-token"});
    let stored: notryx_client_runtime::StoredCredential = serde_json::from_value(json).unwrap();
    let session = Arc::new(Session::from_stored_credential(stored.binding(), &stored).unwrap());
    let runtime = crate::Runtime(Arc::new(Mutex::new(crate::RuntimeState::new(
        store,
        "owner".into(),
    ))));
    assert_eq!(
        activate_verified_session(&runtime, &expected, &session).await,
        Err(notryx_client_runtime::Error::IdentityUnsupported)
    );
    assert!(mark_identity_unsupported(&runtime, &expected).await);
    assert_eq!(
        crate::credential_status_for_renderer(&runtime, "owner")
            .await
            .unwrap()
            .status,
        notryx_protocol::WorkspaceCredentialStatus::IdentityUnsupported
    );
    let mut state = runtime.0.lock().await;
    assert!(state.session.is_none());
    assert_old_preserved(&mut state.store, &old, true).await;
    drop(state);
    server.await.unwrap();
}

#[tokio::test]
#[ignore = "requires a dedicated D-Bus session with no service autostart"]
async fn credential_platform_absent_fails_closed_on_isolated_bus() {
    let prefix =
        std::env::var("NOTRYX_TEST_SECRET_BUS_PREFIX").expect("isolated bus prefix required");
    let address = std::env::var("DBUS_SESSION_BUS_ADDRESS").expect("isolated bus address required");
    assert!(address.starts_with(&format!("unix:path={prefix}/")));
    let sample = binding();
    let json = serde_json::json!({"binding":{"origin":sample.server_origin,"workspace_id":sample.workspace_id,"workspace_epoch":sample.workspace_epoch,"device_id":sample.device_id,"device_stream_id":sample.device_stream_id},"token":"synthetic-token"});
    let saved: notryx_client_runtime::StoredCredential = serde_json::from_value(json).unwrap();
    let session = Session::from_stored_credential(saved.binding(), &saved).unwrap();
    let record = session_credential(&session).unwrap();
    let mut vault = PlatformVault;
    assert!(matches!(
        vault.get(Uuid::new_v4()).await,
        Err(CredentialError::Unavailable)
    ));
    assert!(matches!(
        vault.put(Uuid::new_v4(), &record).await,
        Err(CredentialError::Unavailable)
    ));
}

fn daemon_test_directory() -> std::path::PathBuf {
    let prefix =
        std::env::var("NOTRYX_TEST_SECRET_BUS_PREFIX").expect("isolated test bus required");
    let address = std::env::var("DBUS_SESSION_BUS_ADDRESS").expect("isolated test bus required");
    let data = std::env::var("NOTRYX_TEST_SECRET_DIR").expect("isolated test directory required");
    assert_eq!(prefix, "/work");
    assert!(
        address == "unix:path=/work/run/bus.sock"
            || address.starts_with("unix:path=/work/run/bus.sock,")
    );
    assert_eq!(data, "/work/data");
    assert_eq!(std::env::var("XDG_DATA_HOME").unwrap(), "/work/xdg-data");
    assert_eq!(
        std::env::var("XDG_CONFIG_HOME").unwrap(),
        "/work/xdg-config"
    );
    assert_eq!(std::env::var("XDG_CACHE_HOME").unwrap(), "/work/xdg-cache");
    assert_eq!(std::env::var("XDG_RUNTIME_DIR").unwrap(), "/work/run");
    std::path::PathBuf::from(data)
}

#[cfg(target_os = "linux")]
#[tokio::test]
#[ignore = "requires an isolated Secret Service daemon and synthetic test directory"]
async fn credential_platform_daemon_put() {
    eprintln!("NOTRYX_TEST_PROGRESS=entered");
    let directory = daemon_test_directory();
    eprintln!("NOTRYX_TEST_PROGRESS=isolation_ok");
    let database = directory.join("client.sqlite");
    let mut store = ClientStore::open(&database).await.unwrap();
    eprintln!("NOTRYX_TEST_PROGRESS=sqlite_open");
    let old = binding();
    store.activate_registration(&old).await.unwrap();
    let opened = store
        .open_registered_document(&old, "document", b"old base", "old-revision")
        .await
        .unwrap();
    store
        .save_registered_checkpoint(
            &old,
            "document",
            &opened.edit_session_id,
            1,
            b"pending draft",
        )
        .await
        .unwrap();
    store
        .close_registered_document(&old, "document", &opened.edit_session_id)
        .await
        .unwrap();
    store.queue_checkpoint(&old, "document", 1).await.unwrap();
    eprintln!("NOTRYX_TEST_PROGRESS=draft_saved");
    let next = RegistrationBinding {
        server_origin: "http://127.0.0.1:1".into(),
        device_id: Uuid::new_v4().to_string(),
        device_stream_id: Uuid::new_v4().to_string(),
        ..old.clone()
    };
    let json = serde_json::json!({"binding":{"origin":next.server_origin,"workspace_id":next.workspace_id,"workspace_epoch":next.workspace_epoch,"device_id":next.device_id,"device_stream_id":next.device_stream_id},"token":std::str::from_utf8(TEST_TOKEN).unwrap()});
    let stored: notryx_client_runtime::StoredCredential = serde_json::from_value(json)
        .unwrap_or_else(|_| panic!("NOTRYX_TEST_CREDENTIAL_ERROR=decode"));
    eprintln!("NOTRYX_TEST_PROGRESS=payload_decoded");
    let session = match Session::from_stored_credential(stored.binding(), &stored) {
        Ok(session) => session,
        Err(error) => {
            let label = match error {
                notryx_client_runtime::Error::InvalidOrigin => "invalid_origin",
                notryx_client_runtime::Error::Binding => "binding",
                notryx_client_runtime::Error::Contract => "contract",
                notryx_client_runtime::Error::Network => "network",
                _ => "other",
            };
            panic!("NOTRYX_TEST_CREDENTIAL_ERROR={label}");
        }
    };
    eprintln!("NOTRYX_TEST_PROGRESS=session_built");
    let record = session_credential(&session)
        .unwrap_or_else(|_| panic!("NOTRYX_TEST_CREDENTIAL_ERROR=encode"));
    eprintln!("NOTRYX_TEST_PROGRESS=credential_ready");
    let mut vault = PlatformVault;
    let service =
        match secret_service::SecretService::connect(secret_service::EncryptionType::Dh).await {
            Ok(service) => service,
            Err(_) => panic!("NOTRYX_TEST_PROBE_ERROR=connect"),
        };
    let collection = match service.get_default_collection().await {
        Ok(collection) => collection,
        Err(_) => panic!("NOTRYX_TEST_PROBE_ERROR=no_default"),
    };
    match collection.is_locked().await {
        Ok(false) => {}
        Ok(true) => panic!("NOTRYX_TEST_PROBE_ERROR=locked"),
        Err(_) => panic!("NOTRYX_TEST_PROBE_ERROR=state"),
    }
    drop(collection);
    drop(service);
    eprintln!("NOTRYX_TEST_PROGRESS=probe_ok");
    match stage_pair(&mut store, &mut vault, &record).await {
        Ok(CredentialMode::Stored) => {}
        Ok(CredentialMode::SessionOnly) => panic!("NOTRYX_TEST_STAGE_ERROR=session_only"),
        Err(error) => {
            let label = match error {
                CredentialError::Storage => "storage",
                CredentialError::Unavailable => "unavailable",
                CredentialError::BindingMismatch => "binding_mismatch",
                CredentialError::Ambiguous => "ambiguous",
                CredentialError::Missing => "missing",
                CredentialError::WriteUnknown => "write_unknown",
            };
            panic!("NOTRYX_TEST_STAGE_ERROR={label}");
        }
    }
    eprintln!("NOTRYX_TEST_PROGRESS=stored");
    assert_old_preserved(&mut store, &old, true).await;
    store.close().await.unwrap();
    let bytes = std::fs::read(&database).expect("synthetic SQLite file must remain readable");
    assert!(
        !bytes
            .windows(TEST_TOKEN.len())
            .any(|value| value == TEST_TOKEN)
    );
    match std::fs::read(directory.join("client.sqlite-wal")) {
        Ok(bytes) => assert!(
            !bytes
                .windows(TEST_TOKEN.len())
                .any(|value| value == TEST_TOKEN)
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => panic!("synthetic SQLite WAL could not be checked"),
    }
    eprintln!("NOTRYX_TEST_PROGRESS=sqlite_checked");
}

#[tokio::test]
#[ignore = "requires a restarted isolated Secret Service daemon and synthetic test directory"]
async fn credential_platform_daemon_get_delete() {
    let directory = daemon_test_directory();
    let mut store = ClientStore::open(directory.join("client.sqlite"))
        .await
        .unwrap();
    let old = store
        .list_registered_drafts()
        .await
        .unwrap()
        .into_iter()
        .next()
        .unwrap()
        .binding;
    let (active, Some(id)) = store.active_credential().await.unwrap().unwrap() else {
        panic!("missing stored synthetic registration")
    };
    let mut vault = PlatformVault;
    let record = restore_active(&mut store, &mut vault)
        .await
        .unwrap()
        .unwrap();
    assert!(record.binding == active);
    let saved: notryx_client_runtime::StoredCredential =
        serde_json::from_slice(&record.token).unwrap();
    let session = Session::from_stored_credential(saved.binding(), &saved).unwrap();
    assert!(matches!(
        session.verify_binding().await,
        Err(notryx_client_runtime::Error::Network)
    ));
    let value: serde_json::Value = serde_json::from_slice(&record.token).unwrap();
    assert!(
        value
            .get("token")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|value| value.as_bytes() == TEST_TOKEN)
    );
    assert_old_preserved(&mut store, &old, true).await;
    vault.delete(id, &active).await.unwrap();
    assert!(vault.get(id).await.unwrap().is_empty());
    assert_old_preserved(&mut store, &old, true).await;
    store.close().await.unwrap();
}
