use crate::{Runtime, RuntimeState, registration_binding};
use notryx_client_runtime::{Session, TokenExpiry};
use notryx_client_store::RegistrationBinding;
use notryx_protocol::WorkspaceCredentialRenewalStatus;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tauri::Emitter;

const DAY: u64 = 24 * 60 * 60;

#[derive(Default)]
pub(crate) struct RenewalSchedule {
    next_attempt: Option<std::time::Instant>,
}

pub(crate) fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn expiry_status(expiry: Option<(u64, u64)>, now: u64) -> WorkspaceCredentialRenewalStatus {
    match expiry {
        Some((idle, absolute)) if idle.min(absolute) <= now => {
            WorkspaceCredentialRenewalStatus::Expired
        }
        Some((idle, absolute)) if idle.min(absolute) - now <= 14 * DAY => {
            WorkspaceCredentialRenewalStatus::Expiring
        }
        _ => WorkspaceCredentialRenewalStatus::None,
    }
}

pub(crate) async fn status(
    state: &mut RuntimeState,
    binding: &RegistrationBinding,
) -> Result<WorkspaceCredentialRenewalStatus, String> {
    if state
        .transmission_gate
        .has_block(binding, crate::send::TransmissionBlock::AuthRequired)
    {
        return Ok(WorkspaceCredentialRenewalStatus::AuthRequired);
    }
    if state.credential_renewal_unsupported.as_ref() == Some(binding) {
        return Ok(WorkspaceCredentialRenewalStatus::Unsupported);
    }
    let expiry = state
        .store
        .credential_expiry(binding)
        .await
        .map_err(|error| error.to_string())?;
    Ok(expiry_status(expiry, now()))
}

pub(crate) async fn run(
    runtime: Runtime,
    session: Arc<Session>,
    foreground: Arc<crate::sync_lifecycle::ForegroundSignal>,
    app: Option<tauri::AppHandle>,
) {
    let mut schedule = RenewalSchedule::default();
    loop {
        if foreground.is_active() {
            let keep_running = poll_at(&runtime, &session, &mut schedule, now(), &app).await;
            if let Some(app) = &app {
                let _ = app.emit_to("main", "workspace-credential-status-changed", ());
            }
            if !keep_running {
                return;
            }
        }
        tokio::time::sleep(Duration::from_secs(60)).await;
    }
}

async fn poll_at(
    runtime: &Runtime,
    session: &Arc<Session>,
    schedule: &mut RenewalSchedule,
    now: u64,
    app: &Option<tauri::AppHandle>,
) -> bool {
    let generation = {
        let Ok(mut state) = crate::sync_lifecycle::checked_registration(runtime, session).await
        else {
            return false;
        };
        let binding = registration_binding(session.binding());
        let generation = state.transmission_gate.response_generation(&binding);
        if !state
            .transmission_gate
            .metadata_response_trusted(&binding, generation)
        {
            return false;
        }
        let Ok(mut expiry) = state.store.credential_expiry(&binding).await else {
            return true;
        };
        if expiry.is_none()
            && let Some(initial) = session.token_expiry()
        {
            let _ = state
                .store
                .set_credential_expiry(
                    &binding,
                    initial.idle_expires_at,
                    initial.absolute_expires_at,
                )
                .await;
            expiry = Some((initial.idle_expires_at, initial.absolute_expires_at));
        }
        if schedule
            .next_attempt
            .is_some_and(|next| Instant::now() < next)
        {
            return true;
        }
        if let Some((idle, absolute)) = expiry
            && idle > now
            && (idle - now > 30 * DAY || idle == absolute)
        {
            return true;
        }
        generation
    };
    schedule.next_attempt = Some(Instant::now() + Duration::from_secs(300));
    let result = session.renew_device().await;
    if result == Err(notryx_client_runtime::Error::RenewalUnsupported) {
        schedule.next_attempt = Some(Instant::now() + Duration::from_secs(3600));
    }
    finish_renewal(runtime, session, generation, result, app).await
}

async fn finish_renewal(
    runtime: &Runtime,
    session: &Arc<Session>,
    generation: u64,
    result: Result<TokenExpiry, notryx_client_runtime::Error>,
    app: &Option<tauri::AppHandle>,
) -> bool {
    use notryx_client_runtime::Error;
    let Ok(mut state) = crate::sync_lifecycle::checked_registration(runtime, session).await else {
        return false;
    };
    let binding = registration_binding(session.binding());
    if !state
        .transmission_gate
        .metadata_response_trusted(&binding, generation)
    {
        // Resync may advance the generation while this renewal is in flight.
        return state.transmission_gate.metadata_response_trusted(
            &binding,
            state.transmission_gate.response_generation(&binding),
        );
    }
    let error = match result {
        Ok(expiry) => {
            let Ok(previous) = state.store.credential_expiry(&binding).await else {
                return true;
            };
            let previous = previous.or_else(|| {
                session
                    .token_expiry()
                    .map(|initial| (initial.idle_expires_at, initial.absolute_expires_at))
            });
            if previous.is_some_and(|(idle, absolute)| {
                absolute != expiry.absolute_expires_at || expiry.idle_expires_at < idle
            }) {
                Error::Contract
            } else {
                let _ = state
                    .store
                    .set_credential_expiry(
                        &binding,
                        expiry.idle_expires_at,
                        expiry.absolute_expires_at,
                    )
                    .await;
                state.credential_renewal_unsupported = None;
                return true;
            }
        }
        Err(Error::RenewalUnsupported) => {
            state.credential_renewal_unsupported = Some(binding);
            return true;
        }
        Err(Error::Network | Error::Unavailable | Error::Response) => return true,
        Err(error) => error,
    };
    drop(state);
    crate::sync_lifecycle::handle_remote_error(runtime, session, error, app, Some(generation)).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use notryx_protocol::{AdminRequest, AdminResponse};
    use notryx_server::admin;
    use sqlx::Connection;

    const DAY: u64 = 24 * 60 * 60;

    #[test]
    fn credential_expiry_warning_starts_at_fourteen_days() {
        let current = 1_800_000_000;
        assert_eq!(
            expiry_status(None, current),
            WorkspaceCredentialRenewalStatus::None
        );
        assert_eq!(
            expiry_status(Some((current + 14 * DAY + 1, current + 365 * DAY)), current),
            WorkspaceCredentialRenewalStatus::None
        );
        assert_eq!(
            expiry_status(Some((current + 14 * DAY, current + 365 * DAY)), current),
            WorkspaceCredentialRenewalStatus::Expiring
        );
        assert_eq!(
            expiry_status(Some((current, current + 365 * DAY)), current),
            WorkspaceCredentialRenewalStatus::Expired
        );
    }

    #[tokio::test]
    async fn automatic_renewal_updates_expiry_and_preserves_pending_edits() {
        let (directory, server, runtime, session, document) =
            crate::sync_bootstrap::tests::setup().await;
        let binding = registration_binding(session.binding());
        let current = now();
        let absolute = session.token_expiry().unwrap().absolute_expires_at;
        let before = {
            let mut state = runtime.0.lock().await;
            state
                .store
                .set_credential_expiry(&binding, current + DAY, absolute)
                .await
                .unwrap();
            (
                state
                    .store
                    .read_registered_checkpoint(&binding, &document)
                    .await
                    .unwrap(),
                state.store.list_outbox(&binding, 0, 100).await.unwrap(),
            )
        };
        let mut db = sqlx::SqliteConnection::connect_with(
            &sqlx::sqlite::SqliteConnectOptions::new()
                .filename(directory.path().join("server-state/state.db")),
        )
        .await
        .unwrap();
        sqlx::query("UPDATE devices SET idle_expires_at=? WHERE device_id=?")
            .bind((current + DAY) as i64)
            .bind(&binding.device_id)
            .execute(&mut db)
            .await
            .unwrap();
        db.close().await.unwrap();
        let mut schedule = RenewalSchedule::default();
        assert!(poll_at(&runtime, &session, &mut schedule, current, &None).await);
        let mut state = runtime.0.lock().await;
        let expiry = state
            .store
            .credential_expiry(&binding)
            .await
            .unwrap()
            .unwrap();
        assert!(expiry.0 >= current + 90 * DAY);
        assert_eq!(expiry.1, absolute);
        assert_eq!(
            state
                .store
                .read_registered_checkpoint(&binding, &document)
                .await
                .unwrap(),
            before.0
        );
        assert_eq!(
            state.store.list_outbox(&binding, 0, 100).await.unwrap(),
            before.1
        );
        drop(state);
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn revoked_renewal_requires_registration_and_preserves_local_data() {
        let (directory, server, runtime, session, document) =
            crate::sync_bootstrap::tests::setup().await;
        let binding = registration_binding(session.binding());
        let current = now();
        let before = {
            let mut state = runtime.0.lock().await;
            state
                .store
                .set_credential_expiry(&binding, current + DAY, current + 365 * DAY)
                .await
                .unwrap();
            (
                state
                    .store
                    .read_registered_checkpoint(&binding, &document)
                    .await
                    .unwrap(),
                state.store.list_outbox(&binding, 0, 100).await.unwrap(),
            )
        };
        assert!(matches!(
            admin::request(
                &directory.path().join("admin.sock"),
                &AdminRequest::RevokeDevice {
                    device_id: binding.device_id.clone()
                }
            )
            .await
            .unwrap(),
            AdminResponse::Ok { .. }
        ));
        assert!(
            !poll_at(
                &runtime,
                &session,
                &mut RenewalSchedule::default(),
                current,
                &None
            )
            .await
        );
        let mut state = runtime.0.lock().await;
        assert!(
            state
                .transmission_gate
                .has_block(&binding, crate::send::TransmissionBlock::AuthRequired)
        );
        assert_eq!(
            state
                .store
                .read_registered_checkpoint(&binding, &document)
                .await
                .unwrap(),
            before.0
        );
        assert_eq!(
            state.store.list_outbox(&binding, 0, 100).await.unwrap(),
            before.1
        );
        drop(state);
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn late_renewal_does_not_update_a_replaced_registration() {
        let (_directory, server, runtime, session, _) = crate::sync_bootstrap::tests::setup().await;
        let binding = registration_binding(session.binding());
        let mut replacement = binding.clone();
        replacement.device_id = uuid::Uuid::new_v4().to_string();
        replacement.device_stream_id = uuid::Uuid::new_v4().to_string();
        let generation = {
            let mut state = runtime.0.lock().await;
            state
                .store
                .activate_registration(&replacement)
                .await
                .unwrap();
            state.transmission_gate.response_generation(&binding)
        };
        assert!(
            !finish_renewal(
                &runtime,
                &session,
                generation,
                Ok(session.token_expiry().unwrap()),
                &None
            )
            .await
        );
        assert_eq!(
            runtime
                .0
                .lock()
                .await
                .store
                .credential_expiry(&replacement)
                .await
                .unwrap(),
            None
        );
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn offline_renewal_backs_off_without_changing_expiry_or_pending_edits() {
        let (_directory, server, runtime, session, document) =
            crate::sync_bootstrap::tests::setup().await;
        let binding = registration_binding(session.binding());
        let current = now();
        let expiry = (current + DAY, current + 365 * DAY);
        let before = {
            let mut state = runtime.0.lock().await;
            state
                .store
                .set_credential_expiry(&binding, expiry.0, expiry.1)
                .await
                .unwrap();
            (
                state
                    .store
                    .read_registered_checkpoint(&binding, &document)
                    .await
                    .unwrap(),
                state.store.list_outbox(&binding, 0, 100).await.unwrap(),
            )
        };
        server.shutdown().await.unwrap();
        let mut schedule = RenewalSchedule::default();
        assert!(poll_at(&runtime, &session, &mut schedule, current, &None).await);
        assert!(
            schedule
                .next_attempt
                .is_some_and(|next| next > std::time::Instant::now())
        );
        let mut state = runtime.0.lock().await;
        assert_eq!(
            state.store.credential_expiry(&binding).await.unwrap(),
            Some(expiry)
        );
        assert!(!state.transmission_gate.blocked(&binding));
        assert_eq!(
            state
                .store
                .read_registered_checkpoint(&binding, &document)
                .await
                .unwrap(),
            before.0
        );
        assert_eq!(
            state.store.list_outbox(&binding, 0, 100).await.unwrap(),
            before.1
        );
    }

    #[tokio::test]
    async fn late_renewal_during_resync_keeps_the_current_sync_running() {
        let (_directory, server, runtime, session, document) =
            crate::sync_bootstrap::tests::setup().await;
        let binding = registration_binding(session.binding());
        let (generation, draft) = {
            let mut state = runtime.0.lock().await;
            let generation = state.transmission_gate.response_generation(&binding);
            state
                .transmission_gate
                .begin_initial_sync(&binding)
                .unwrap();
            (
                generation,
                state
                    .store
                    .read_registered_checkpoint(&binding, &document)
                    .await
                    .unwrap(),
            )
        };
        for result in [
            Ok(session.token_expiry().unwrap()),
            Err(notryx_client_runtime::Error::Unauthorized),
        ] {
            assert!(finish_renewal(&runtime, &session, generation, result, &None).await);
        }
        let mut state = runtime.0.lock().await;
        assert_eq!(state.store.credential_expiry(&binding).await.unwrap(), None);
        assert_eq!(
            state
                .store
                .read_registered_checkpoint(&binding, &document)
                .await
                .unwrap(),
            draft
        );
        assert!(
            !state
                .transmission_gate
                .has_block(&binding, crate::send::TransmissionBlock::AuthRequired)
        );
        drop(state);
        server.shutdown().await.unwrap();
    }
}
