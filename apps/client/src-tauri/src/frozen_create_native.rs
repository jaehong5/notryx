use crate::{
    Runtime, RuntimeState, check_caller, from_contract, registration_binding, to_contract,
};
use notryx_client_store::{FrozenCreateChoice, OutboxStatus, RegistrationBinding};
use notryx_domain::mutation::normalized_name_key;
use notryx_protocol::{
    WorkspaceCreateStatus, WorkspaceFrozenCreateChoice, WorkspaceFrozenCreateDecisionRequest,
    WorkspaceFrozenCreateEntry, WorkspaceFrozenCreateListRequest, WorkspaceFrozenCreatePage,
};
use std::sync::Arc;
use tokio::sync::Mutex;

#[tauri::command]
pub(crate) async fn workspace_frozen_creates(
    webview: tauri::Webview,
    runtime: tauri::State<'_, Runtime>,
    request: WorkspaceFrozenCreateListRequest,
) -> Result<WorkspaceFrozenCreatePage, String> {
    check_caller(&webview)?;
    let binding = from_contract(&request.binding);
    let page = list_for_renderer(
        &runtime.0,
        &request.renderer_session_id,
        &binding,
        request.after_id.as_deref(),
        request.limit,
    )
    .await?;
    check_caller(&webview)?;
    Ok(page)
}

pub(crate) async fn list_for_renderer(
    runtime: &Arc<Mutex<RuntimeState>>,
    owner: &str,
    binding: &RegistrationBinding,
    after: Option<&str>,
    limit: u16,
) -> Result<WorkspaceFrozenCreatePage, String> {
    let mut state = runtime.lock().await;
    state.gate.snapshot(owner).map_err(str::to_owned)?;
    if state
        .store
        .current_registration()
        .await
        .map_err(|error| error.to_string())?
        .as_ref()
        != Some(binding)
        || state
            .session
            .as_ref()
            .is_some_and(|session| registration_binding(session.binding()) != *binding)
    {
        return Err("stale_registration".into());
    }
    let (_, nodes) = state
        .store
        .sync_index(binding)
        .await
        .map_err(|error| error.to_string())?
        .ok_or("not_connected")?;
    let page = state
        .store
        .list_frozen_creates(binding, after, limit)
        .await
        .map_err(|error| error.to_string())?;
    let mut entries = Vec::with_capacity(page.entries.len());
    for item in page.entries {
        let operation = item.operation;
        let status = match operation.status {
            OutboxStatus::Queued => WorkspaceCreateStatus::Queued,
            OutboxStatus::Sent => WorkspaceCreateStatus::Sent,
            OutboxStatus::Abandoning => WorkspaceCreateStatus::Abandoning,
            OutboxStatus::Failed => WorkspaceCreateStatus::Failed,
            _ => return Err("UnsupportedFormat".into()),
        };
        let same_id = nodes
            .iter()
            .find(|node| node.node_id == operation.request.node_id)
            .cloned();
        let key = normalized_name_key(&operation.request.name);
        let same_name = nodes
            .iter()
            .filter(|node| {
                !node.deleted
                    && node.node_id != operation.request.node_id
                    && node.parent_id == Some(operation.request.parent_id)
                    && normalized_name_key(&node.name) == key
            })
            .cloned()
            .collect();
        entries.push(WorkspaceFrozenCreateEntry {
            old_binding: to_contract(operation.binding),
            operation_id: operation.operation_id,
            operation_seq: operation.operation_seq.to_string(),
            request: operation.request,
            status,
            receipt: item.receipt,
            current_same_id: same_id,
            current_same_name: same_name,
        });
    }
    Ok(WorkspaceFrozenCreatePage {
        binding: to_contract(binding.clone()),
        entries,
        next_after_id: page.next_after_id,
    })
}

#[tauri::command]
pub(crate) async fn workspace_resolve_frozen_create(
    webview: tauri::Webview,
    runtime: tauri::State<'_, Runtime>,
    request: WorkspaceFrozenCreateDecisionRequest,
) -> Result<(), String> {
    check_caller(&webview)?;
    let current = from_contract(&request.binding);
    let old = from_contract(&request.old_binding);
    decide_for_renderer(
        &runtime.0,
        &request.renderer_session_id,
        &current,
        &old,
        &request.operation_id,
        &request.choice,
    )
    .await
}

pub(crate) async fn decide_for_renderer(
    runtime: &Arc<Mutex<RuntimeState>>,
    owner: &str,
    current: &RegistrationBinding,
    old: &RegistrationBinding,
    operation_id: &str,
    choice: &WorkspaceFrozenCreateChoice,
) -> Result<(), String> {
    let mut state = runtime.lock().await;
    state.gate.check_queue(owner).map_err(str::to_owned)?;
    if state
        .store
        .current_registration()
        .await
        .map_err(|error| error.to_string())?
        .as_ref()
        != Some(current)
        || state
            .session
            .as_ref()
            .is_some_and(|session| registration_binding(session.binding()) != *current)
    {
        return Err("stale_registration".into());
    }
    let choice = match choice {
        WorkspaceFrozenCreateChoice::Apply { parent_id, name } => FrozenCreateChoice::Apply {
            parent_id: *parent_id,
            name: name.clone(),
        },
        WorkspaceFrozenCreateChoice::Dismiss => FrozenCreateChoice::Dismiss,
    };
    state
        .store
        .resolve_frozen_create(old, current, operation_id, &choice)
        .await
        .map_err(|error| {
            if error == notryx_client_store::StoreError::ReconciliationRequired {
                "dependent_operations".into()
            } else {
                error.to_string()
            }
        })
}
// Proposed private unit tests for the future frozen-create native module.
// Assumes list_for_renderer returns WorkspaceFrozenCreatePage and
// decide_for_renderer accepts &WorkspaceFrozenCreateChoice.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::{RuntimeState, to_contract};
    use notryx_client_store::{ClientStore, PendingOutboxOperation, RegistrationBinding};
    use notryx_protocol::{
        CreateNodeRequest, NodeKind, NodeState, OperationReceipt, WorkspaceCreateStatus,
        WorkspaceFrozenCreateChoice,
    };
    use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
    use std::{path::Path, sync::Arc};
    use tokio::sync::Mutex;
    use uuid::Uuid;

    const OWNER: &str = "owner";
    const ROOT: &str = "019a1000-0000-7000-8000-000000000001";
    const RECEIPT_NODE: &str = "019a1000-0000-7000-8000-000000000002";
    const UNRESOLVED_NODE: &str = "019a1000-0000-7000-8000-000000000003";
    const OTHER_NODE: &str = "019a1000-0000-7000-8000-000000000004";

    fn binding() -> RegistrationBinding {
        RegistrationBinding {
            server_origin: "https://example.invalid".into(),
            workspace_id: Uuid::new_v4().to_string(),
            workspace_epoch: Uuid::new_v4().to_string(),
            device_id: Uuid::new_v4().to_string(),
            device_stream_id: Uuid::new_v4().to_string(),
        }
    }
    fn replacement(old: &RegistrationBinding) -> RegistrationBinding {
        RegistrationBinding {
            device_id: Uuid::new_v4().to_string(),
            device_stream_id: Uuid::new_v4().to_string(),
            ..old.clone()
        }
    }
    fn node(id: &str, parent: Option<&str>, name: &str) -> NodeState {
        serde_json::from_value(serde_json::json!({"node_id":id,"parent_id":parent,"name":name,"kind":"folder","deleted":false,
            "content_rev":null,"content_etag":null,"meta_rev":"meta","meta_etag":"\"meta\"","node_etag":"\"node\"",
            "content_hash":null,"size":null,"children_generation":"0","last_change_seq":"1"})).unwrap()
    }
    async fn db(path: &Path) -> SqliteConnection {
        SqliteConnection::connect_with(
            &SqliteConnectOptions::new()
                .filename(path)
                .foreign_keys(true),
        )
        .await
        .unwrap()
    }
    async fn seed(path: &Path, binding: &RegistrationBinding, nodes: &[NodeState]) {
        let mut db = db(path).await;
        let registration: i64 = sqlx::query_scalar("SELECT id FROM client_registrations WHERE server_origin=? AND workspace_id=? AND workspace_epoch=? AND device_id=? AND device_stream_id=?")
            .bind(&binding.server_origin).bind(&binding.workspace_id).bind(&binding.workspace_epoch).bind(&binding.device_id).bind(&binding.device_stream_id).fetch_one(&mut db).await.unwrap();
        sqlx::query("INSERT INTO remote_index(registration_id,root_id,cursor) VALUES (?,?,'1')")
            .bind(registration)
            .bind(ROOT)
            .execute(&mut db)
            .await
            .unwrap();
        for node in nodes {
            sqlx::query("INSERT INTO remote_nodes(registration_id,node_id,state,content_ready) VALUES (?,?,?,0)")
                .bind(registration).bind(node.node_id.to_string()).bind(serde_json::to_string(node).unwrap()).execute(&mut db).await.unwrap();
        }
        db.close().await.unwrap();
    }
    struct Rig {
        dir: tempfile::TempDir,
        state: Arc<Mutex<RuntimeState>>,
        old: RegistrationBinding,
        current: RegistrationBinding,
        conflict_id: String,
        pending_id: String,
        prior_receipt: OperationReceipt,
    }
    async fn fixture() -> Rig {
        fixture_kind(NodeKind::Folder).await
    }
    async fn fixture_kind(pending_kind: NodeKind) -> Rig {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("client.sqlite");
        let old = binding();
        let current = replacement(&old);
        let mut store = ClientStore::open(&path).await.unwrap();
        store.activate_registration(&old).await.unwrap();
        seed(&path, &old, &[node(ROOT, None, "")]).await;
        let request = |id: &str, name: &str| CreateNodeRequest {
            node_id: id.parse().unwrap(),
            parent_id: ROOT.parse().unwrap(),
            name: name.into(),
            kind: if id == UNRESOLVED_NODE {
                pending_kind
            } else {
                NodeKind::Folder
            },
            upload_id: None,
        };
        let conflict = store
            .queue_create(&old, &request(RECEIPT_NODE, "occupied"))
            .await
            .unwrap();
        assert!(matches!(
            store.prepare_pending_outbox(&old).await.unwrap(),
            Some(PendingOutboxOperation::Create(_))
        ));
        let prior_receipt: OperationReceipt = serde_json::from_value(serde_json::json!({"status":"destination_exists", "operation_id":conflict.operation_id, "operation_seq":"1", "workspace_epoch":old.workspace_epoch})).unwrap();
        store
            .apply_create_receipt(&old, &prior_receipt)
            .await
            .unwrap();
        let pending = store
            .queue_create(&old, &request(UNRESOLVED_NODE, "Straße"))
            .await
            .unwrap();
        assert!(matches!(
            store.prepare_pending_outbox(&old).await.unwrap(),
            Some(PendingOutboxOperation::Create(_))
        ));
        store.activate_registration(&current).await.unwrap();
        seed(
            &path,
            &current,
            &[
                node(ROOT, None, ""),
                node(UNRESOLVED_NODE, Some(ROOT), "same id, different name"),
                node(OTHER_NODE, Some(ROOT), "STRASSE"),
            ],
        )
        .await;
        Rig {
            dir,
            state: Arc::new(Mutex::new(RuntimeState::new(store, OWNER.into()))),
            old,
            current,
            conflict_id: conflict.operation_id,
            pending_id: pending.operation_id,
            prior_receipt,
        }
    }
    async fn snapshot(
        path: &Path,
        operation: &str,
    ) -> (Vec<u8>, String, String, Option<String>, Option<String>) {
        let mut db = db(path).await;
        let value = sqlx::query_as("SELECT bytes,payload_hash,status,terminal_kind,receipt_json FROM client_outbox WHERE operation_id=?")
            .bind(operation).fetch_one(&mut db).await.unwrap();
        db.close().await.unwrap();
        value
    }

    #[tokio::test]
    async fn frozen_file_reapply_rejects_non_markdown_without_losing_original() {
        let rig = fixture_kind(NodeKind::File).await;
        let path = rig.dir.path().join("client.sqlite");
        let before = snapshot(&path, &rig.pending_id).await;
        let bad = WorkspaceFrozenCreateChoice::Apply {
            parent_id: ROOT.parse().unwrap(),
            name: "plain.txt".into(),
        };
        assert!(
            decide_for_renderer(
                &rig.state,
                OWNER,
                &rig.current,
                &rig.old,
                &rig.pending_id,
                &bad
            )
            .await
            .is_err()
        );
        assert_eq!(snapshot(&path, &rig.pending_id).await, before);
        let good = WorkspaceFrozenCreateChoice::Apply {
            parent_id: ROOT.parse().unwrap(),
            name: "UPPER.MD".into(),
        };
        decide_for_renderer(
            &rig.state,
            OWNER,
            &rig.current,
            &rig.old,
            &rig.pending_id,
            &good,
        )
        .await
        .unwrap();
        assert_eq!(snapshot(&path, &rig.pending_id).await, before);
        let mut state = rig.state.lock().await;
        let Some(PendingOutboxOperation::Create(item)) =
            state.store.next_pending_outbox(&rig.current).await.unwrap()
        else {
            panic!("file create required")
        };
        assert_eq!(item.request.kind, NodeKind::File);
        assert_eq!(item.request.name, "UPPER.MD");
        assert_eq!(item.operation_seq, 1);
    }

    #[tokio::test]
    async fn unresolved_sent_create_shows_both_current_candidates_without_assuming_success() {
        let rig = fixture().await;
        let before = snapshot(&rig.dir.path().join("client.sqlite"), &rig.pending_id).await;
        let page = list_for_renderer(&rig.state, OWNER, &rig.current, None, 100)
            .await
            .unwrap();
        assert_eq!(page.binding, to_contract(rig.current.clone()));
        let item = page
            .entries
            .iter()
            .find(|item| item.operation_id == rig.pending_id)
            .unwrap();
        assert_eq!(item.old_binding, to_contract(rig.old.clone()));
        assert_eq!(item.status, WorkspaceCreateStatus::Sent);
        assert_eq!(item.request.name, "Straße");
        assert_eq!(
            item.current_same_id.as_ref().unwrap().node_id.to_string(),
            UNRESOLVED_NODE
        );
        assert_eq!(
            item.current_same_id.as_ref().unwrap().name,
            "same id, different name"
        );
        assert!(
            item.current_same_name
                .iter()
                .any(|node| node.node_id.to_string() == OTHER_NODE && node.name == "STRASSE")
        );
        assert_eq!(
            snapshot(&rig.dir.path().join("client.sqlite"), &rig.pending_id).await,
            before
        );
    }

    #[tokio::test]
    async fn active_registration_and_renderer_owner_are_enforced() {
        let rig = fixture().await;
        assert!(
            list_for_renderer(&rig.state, "wrong", &rig.current, None, 100)
                .await
                .is_err()
        );
        assert!(
            decide_for_renderer(
                &rig.state,
                "wrong",
                &rig.current,
                &rig.old,
                &rig.pending_id,
                &WorkspaceFrozenCreateChoice::Dismiss
            )
            .await
            .is_err()
        );
        rig.state
            .lock()
            .await
            .gate
            .replace_owner("replacement".into());
        assert!(
            list_for_renderer(&rig.state, OWNER, &rig.current, None, 100)
                .await
                .is_err()
        );
        assert!(
            list_for_renderer(&rig.state, "replacement", &rig.current, None, 100)
                .await
                .is_ok()
        );
        let newer = replacement(&rig.current);
        {
            let mut state = rig.state.lock().await;
            let pair = state.gate.begin_pair("replacement").unwrap();
            state.store.activate_registration(&newer).await.unwrap();
            state.gate.commit_pair(&pair).unwrap();
        }
        assert!(
            list_for_renderer(&rig.state, "replacement", &rig.current, None, 100)
                .await
                .is_err()
        );
        assert!(
            decide_for_renderer(
                &rig.state,
                "replacement",
                &rig.current,
                &rig.old,
                &rig.pending_id,
                &WorkspaceFrozenCreateChoice::Dismiss
            )
            .await
            .is_err()
        );
        seed(
            &rig.dir.path().join("client.sqlite"),
            &newer,
            &[node(ROOT, None, "")],
        )
        .await;
        assert!(
            list_for_renderer(&rig.state, "replacement", &newer, None, 100)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn different_workspace_never_lists_or_decides_other_workspace_operations() {
        let rig = fixture().await;
        let before = snapshot(&rig.dir.path().join("client.sqlite"), &rig.pending_id).await;
        let other = binding();
        {
            let mut state = rig.state.lock().await;
            let pair = state.gate.begin_pair(OWNER).unwrap();
            state.store.activate_registration(&other).await.unwrap();
            state.gate.commit_pair(&pair).unwrap();
        }
        seed(
            &rig.dir.path().join("client.sqlite"),
            &other,
            &[node(ROOT, None, "")],
        )
        .await;
        assert!(
            list_for_renderer(&rig.state, OWNER, &other, None, 100)
                .await
                .unwrap()
                .entries
                .is_empty()
        );
        assert!(
            decide_for_renderer(
                &rig.state,
                OWNER,
                &other,
                &rig.old,
                &rig.pending_id,
                &WorkspaceFrozenCreateChoice::Dismiss
            )
            .await
            .is_err()
        );
        assert_eq!(
            snapshot(&rig.dir.path().join("client.sqlite"), &rig.pending_id).await,
            before
        );
    }

    #[tokio::test]
    async fn same_decision_after_lost_response_does_not_duplicate_and_preserves_old_receipt_and_payload()
     {
        let rig = fixture().await;
        let path = rig.dir.path().join("client.sqlite");
        let pending_before = snapshot(&path, &rig.pending_id).await;
        let conflict_before = snapshot(&path, &rig.conflict_id).await;
        assert_eq!(
            serde_json::from_str::<OperationReceipt>(conflict_before.4.as_ref().unwrap()).unwrap(),
            rig.prior_receipt
        );
        let choice = WorkspaceFrozenCreateChoice::Apply {
            parent_id: ROOT.parse().unwrap(),
            name: "chosen new folder".into(),
        };
        decide_for_renderer(
            &rig.state,
            OWNER,
            &rig.current,
            &rig.old,
            &rig.pending_id,
            &choice,
        )
        .await
        .unwrap();
        // Discarding the first result models a caller that never received it.
        decide_for_renderer(
            &rig.state,
            OWNER,
            &rig.current,
            &rig.old,
            &rig.pending_id,
            &choice,
        )
        .await
        .unwrap();
        let mut db = db(&path).await;
        let (decisions, operations, next_seq): (i64, i64, i64) = sqlx::query_as("SELECT (SELECT COUNT(*) FROM create_reconciliations WHERE old_operation_id=?), (SELECT COUNT(*) FROM client_outbox o JOIN client_registrations r ON r.id=o.registration_id WHERE r.status='active'), (SELECT next_seq FROM outbox_streams s JOIN client_registrations r ON r.id=s.registration_id WHERE r.status='active')")
            .bind(&rig.pending_id).fetch_one(&mut db).await.unwrap();
        assert_eq!((decisions, operations, next_seq), (1, 1, 2));
        db.close().await.unwrap();
        assert_eq!(snapshot(&path, &rig.pending_id).await, pending_before);
        assert_eq!(snapshot(&path, &rig.conflict_id).await, conflict_before);
        let mut state = rig.state.lock().await;
        let Some(PendingOutboxOperation::Create(created)) =
            state.store.next_pending_outbox(&rig.current).await.unwrap()
        else {
            panic!("create");
        };
        assert_ne!(created.operation_id, rig.pending_id);
        assert_ne!(created.request.node_id.to_string(), UNRESOLVED_NODE);
        assert_eq!(created.request.name, "chosen new folder");
    }
}
