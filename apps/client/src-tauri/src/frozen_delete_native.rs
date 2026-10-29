use crate::{
    Runtime, RuntimeState, check_caller, from_contract, registration_binding, to_contract,
};
use notryx_client_store::{FrozenDeleteChoice, OutboxStatus, RegistrationBinding};
use notryx_protocol::{
    WorkspaceDeleteStatus, WorkspaceFrozenDeleteChoice, WorkspaceFrozenDeleteDecisionRequest,
    WorkspaceFrozenDeleteEntry, WorkspaceFrozenDeleteListRequest, WorkspaceFrozenDeletePage,
};
use std::sync::Arc;
use tokio::sync::Mutex;

pub(crate) async fn list_for_renderer(
    runtime: &Arc<Mutex<RuntimeState>>,
    owner: &str,
    binding: &RegistrationBinding,
    after: Option<&str>,
    limit: u16,
) -> Result<WorkspaceFrozenDeletePage, String> {
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
        .list_frozen_deletes(binding, after, limit)
        .await
        .map_err(|error| error.to_string())?;
    let mut entries = Vec::with_capacity(page.entries.len());
    for item in page.entries {
        let operation = item.operation;
        let status = match operation.status {
            OutboxStatus::Queued => WorkspaceDeleteStatus::Queued,
            OutboxStatus::Sent => WorkspaceDeleteStatus::Sent,
            OutboxStatus::Abandoning => WorkspaceDeleteStatus::Abandoning,
            OutboxStatus::Failed => WorkspaceDeleteStatus::Failed,
            _ => return Err("UnsupportedFormat".into()),
        };
        let current_node = nodes
            .iter()
            .find(|node| node.node_id == operation.node_id)
            .cloned();
        entries.push(WorkspaceFrozenDeleteEntry {
            old_binding: to_contract(operation.binding),
            operation_id: operation.operation_id,
            operation_seq: operation.operation_seq.to_string(),
            node_id: operation.node_id,
            expected_node_etag: operation.expected_node_etag,
            status,
            receipt: item.receipt,
            current_node,
        });
    }
    Ok(WorkspaceFrozenDeletePage {
        binding: to_contract(binding.clone()),
        entries,
        next_after_id: page.next_after_id,
    })
}

pub(crate) async fn decide_for_renderer(
    runtime: &Arc<Mutex<RuntimeState>>,
    owner: &str,
    current: &RegistrationBinding,
    old: &RegistrationBinding,
    operation_id: &str,
    choice: &WorkspaceFrozenDeleteChoice,
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
        WorkspaceFrozenDeleteChoice::Apply { expected_node_etag } => FrozenDeleteChoice::Apply {
            expected_node_etag: expected_node_etag.clone(),
        },
        WorkspaceFrozenDeleteChoice::Dismiss => FrozenDeleteChoice::Dismiss,
    };
    state
        .store
        .resolve_frozen_delete(old, current, operation_id, &choice)
        .await
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub(crate) async fn workspace_frozen_deletes(
    webview: tauri::Webview,
    runtime: tauri::State<'_, Runtime>,
    request: WorkspaceFrozenDeleteListRequest,
) -> Result<WorkspaceFrozenDeletePage, String> {
    check_caller(&webview)?;
    let page = list_for_renderer(
        &runtime.0,
        &request.renderer_session_id,
        &from_contract(&request.binding),
        request.after_id.as_deref(),
        request.limit,
    )
    .await?;
    check_caller(&webview)?;
    Ok(page)
}

#[tauri::command]
pub(crate) async fn workspace_resolve_frozen_delete(
    webview: tauri::Webview,
    runtime: tauri::State<'_, Runtime>,
    request: WorkspaceFrozenDeleteDecisionRequest,
) -> Result<(), String> {
    check_caller(&webview)?;
    decide_for_renderer(
        &runtime.0,
        &request.renderer_session_id,
        &from_contract(&request.binding),
        &from_contract(&request.old_binding),
        &request.operation_id,
        &request.choice,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{registration_binding, sync_bootstrap};
    use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
    use uuid::Uuid;

    #[tokio::test]
    async fn frozen_delete_requires_renderer_binding_and_user_confirmation() {
        let (directory, server, runtime, session, _) = sync_bootstrap::tests::setup().await;
        let snapshot = runtime.0.lock().await.gate.snapshot("owner").unwrap();
        sync_bootstrap::bootstrap_metadata(&runtime, &snapshot, &session)
            .await
            .unwrap();
        let node = session
            .inventory(None)
            .await
            .unwrap()
            .nodes
            .into_iter()
            .find(|node| node.name == "move.md")
            .unwrap();
        let old = registration_binding(session.binding());
        let mut current = old.clone();
        current.device_id = Uuid::new_v4().to_string();
        current.device_stream_id = Uuid::new_v4().to_string();
        let old_item = {
            let mut state = runtime.0.lock().await;
            let item = state
                .store
                .queue_delete(&old, node.node_id, &node.node_etag)
                .await
                .unwrap();
            state.store.activate_registration(&current).await.unwrap();
            state.session = None;
            item
        };
        let path = directory.path().join("client.sqlite");
        let mut db = SqliteConnection::connect_with(
            &SqliteConnectOptions::new()
                .filename(&path)
                .foreign_keys(true),
        )
        .await
        .unwrap();
        sqlx::raw_sql("INSERT INTO remote_index SELECT 2,root_id,cursor FROM remote_index WHERE registration_id=1; INSERT INTO remote_nodes SELECT 2,node_id,state,content_ready FROM remote_nodes WHERE registration_id=1").execute(&mut db).await.unwrap();
        db.close().await.unwrap();
        assert!(
            list_for_renderer(&runtime.0, "wrong-owner", &current, None, 10)
                .await
                .is_err()
        );
        assert!(
            list_for_renderer(&runtime.0, "owner", &old, None, 10)
                .await
                .is_err()
        );
        let page = list_for_renderer(&runtime.0, "owner", &current, None, 10)
            .await
            .unwrap();
        assert_eq!(page.entries.len(), 1);
        assert_eq!(page.entries[0].operation_id, old_item.operation_id);
        assert_eq!(
            page.entries[0].current_node.as_ref().unwrap().node_etag,
            node.node_etag
        );
        assert!(
            decide_for_renderer(
                &runtime.0,
                "wrong-owner",
                &current,
                &old,
                &old_item.operation_id,
                &WorkspaceFrozenDeleteChoice::Dismiss
            )
            .await
            .is_err()
        );
        decide_for_renderer(
            &runtime.0,
            "owner",
            &current,
            &old,
            &old_item.operation_id,
            &WorkspaceFrozenDeleteChoice::Dismiss,
        )
        .await
        .unwrap();
        assert!(
            list_for_renderer(&runtime.0, "owner", &current, None, 10)
                .await
                .unwrap()
                .entries
                .is_empty()
        );
        server.shutdown().await.unwrap();
    }
    #[tokio::test]
    async fn frozen_delete_rejects_choice_after_registration_replacement() {
        let (directory, server, runtime, session, _) = sync_bootstrap::tests::setup().await;
        let snapshot = runtime.0.lock().await.gate.snapshot("owner").unwrap();
        sync_bootstrap::bootstrap_metadata(&runtime, &snapshot, &session)
            .await
            .unwrap();
        let node = session
            .inventory(None)
            .await
            .unwrap()
            .nodes
            .into_iter()
            .find(|node| node.name == "move.md")
            .unwrap();
        let old = registration_binding(session.binding());
        let mut current = old.clone();
        current.device_id = Uuid::new_v4().to_string();
        current.device_stream_id = Uuid::new_v4().to_string();
        let old_item = {
            let mut state = runtime.0.lock().await;
            let item = state
                .store
                .queue_delete(&old, node.node_id, &node.node_etag)
                .await
                .unwrap();
            state.store.activate_registration(&current).await.unwrap();
            state.session = None;
            item
        };
        let path = directory.path().join("client.sqlite");
        let mut db = SqliteConnection::connect_with(
            &SqliteConnectOptions::new()
                .filename(&path)
                .foreign_keys(true),
        )
        .await
        .unwrap();
        sqlx::raw_sql("INSERT INTO remote_index SELECT 2,root_id,cursor FROM remote_index WHERE registration_id=1; INSERT INTO remote_nodes SELECT 2,node_id,state,content_ready FROM remote_nodes WHERE registration_id=1").execute(&mut db).await.unwrap();
        db.close().await.unwrap();
        let page = list_for_renderer(&runtime.0, "owner", &current, None, 10)
            .await
            .unwrap();
        assert_eq!(page.entries[0].operation_id, old_item.operation_id);
        let mut replacement = current.clone();
        replacement.device_id = Uuid::new_v4().to_string();
        replacement.device_stream_id = Uuid::new_v4().to_string();
        {
            let mut state = runtime.0.lock().await;
            state
                .store
                .activate_registration(&replacement)
                .await
                .unwrap();
        }
        assert!(
            list_for_renderer(&runtime.0, "owner", &current, None, 10)
                .await
                .is_err()
        );
        assert!(
            decide_for_renderer(
                &runtime.0,
                "owner",
                &current,
                &old,
                &old_item.operation_id,
                &WorkspaceFrozenDeleteChoice::Apply {
                    expected_node_etag: node.node_etag
                }
            )
            .await
            .is_err()
        );
        let mut db = SqliteConnection::connect_with(
            &SqliteConnectOptions::new()
                .filename(&path)
                .foreign_keys(true),
        )
        .await
        .unwrap();
        let decision: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM delete_reconciliations")
            .fetch_one(&mut db)
            .await
            .unwrap();
        assert_eq!(decision, 0);
        let old_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM client_outbox WHERE operation_id=? AND registration_id=1",
        )
        .bind(old_item.operation_id)
        .fetch_one(&mut db)
        .await
        .unwrap();
        assert_eq!(old_count, 1);
        db.close().await.unwrap();
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn frozen_delete_repeated_apply_keeps_one_new_operation() {
        let (directory, server, runtime, session, _) = sync_bootstrap::tests::setup().await;
        let snapshot = runtime.0.lock().await.gate.snapshot("owner").unwrap();
        sync_bootstrap::bootstrap_metadata(&runtime, &snapshot, &session)
            .await
            .unwrap();
        let node = session
            .inventory(None)
            .await
            .unwrap()
            .nodes
            .into_iter()
            .find(|node| node.name == "move.md")
            .unwrap();
        let old = registration_binding(session.binding());
        let mut current = old.clone();
        current.device_id = Uuid::new_v4().to_string();
        current.device_stream_id = Uuid::new_v4().to_string();
        let old_item = {
            let mut state = runtime.0.lock().await;
            let item = state
                .store
                .queue_delete(&old, node.node_id, &node.node_etag)
                .await
                .unwrap();
            state.store.activate_registration(&current).await.unwrap();
            state.session = None;
            item
        };
        let path = directory.path().join("client.sqlite");
        let mut db = SqliteConnection::connect_with(
            &SqliteConnectOptions::new()
                .filename(&path)
                .foreign_keys(true),
        )
        .await
        .unwrap();
        sqlx::raw_sql("INSERT INTO remote_index SELECT 2,root_id,cursor FROM remote_index WHERE registration_id=1; INSERT INTO remote_nodes SELECT 2,node_id,state,content_ready FROM remote_nodes WHERE registration_id=1").execute(&mut db).await.unwrap();
        db.close().await.unwrap();
        let choice = WorkspaceFrozenDeleteChoice::Apply {
            expected_node_etag: node.node_etag.clone(),
        };
        decide_for_renderer(
            &runtime.0,
            "owner",
            &current,
            &old,
            &old_item.operation_id,
            &choice,
        )
        .await
        .unwrap();
        decide_for_renderer(
            &runtime.0,
            "owner",
            &current,
            &old,
            &old_item.operation_id,
            &choice,
        )
        .await
        .unwrap();
        let mut state = runtime.0.lock().await;
        let page = state
            .store
            .list_delete_operations(&current, 0, 10)
            .await
            .unwrap();
        assert_eq!(page.entries.len(), 1);
        assert_ne!(
            page.entries[0].operation.operation_id,
            old_item.operation_id
        );
        assert_eq!(page.entries[0].operation.expected_node_etag, node.node_etag);
        drop(state);
        server.shutdown().await.unwrap();
    }
}
