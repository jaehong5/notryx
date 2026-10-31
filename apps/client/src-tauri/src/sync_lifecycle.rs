use crate::{Runtime, RuntimeState, registration_binding};
use notryx_client_runtime::Session;
use std::sync::Arc;
use tauri::Emitter;
use tokio::sync::MutexGuard;

#[cfg(test)]
mod retention_rebootstrap_tests;

pub(crate) struct ForegroundSignal {
    active: std::sync::atomic::AtomicBool,
    wake: tokio::sync::Notify,
}

impl ForegroundSignal {
    pub(crate) fn new() -> Self {
        Self {
            active: std::sync::atomic::AtomicBool::new(true),
            wake: tokio::sync::Notify::new(),
        }
    }

    pub(crate) fn set(&self, active: bool) {
        self.active
            .store(active, std::sync::atomic::Ordering::Release);
        self.wake.notify_one();
    }

    pub(crate) fn is_active(&self) -> bool {
        self.active.load(std::sync::atomic::Ordering::Acquire)
    }

    async fn wait_until_active(&self) {
        loop {
            let notified = self.wake.notified();
            if self.is_active() {
                return;
            }
            notified.await;
        }
    }
}

pub(crate) async fn sync_state_for_renderer(
    runtime: &Runtime,
    owner: &str,
) -> Result<notryx_protocol::WorkspaceSyncState, String> {
    let mut state = runtime.0.lock().await;
    state.gate.snapshot(owner).map_err(str::to_owned)?;
    let Some(session) = state.session.clone() else {
        let binding = state
            .store
            .current_registration()
            .await
            .map_err(|error| error.to_string())?
            .ok_or("not_connected")?;
        if state
            .store
            .sync_index(&binding)
            .await
            .map_err(|error| error.to_string())?
            .is_none()
        {
            return Err("not_connected".into());
        }
        return Ok(notryx_protocol::WorkspaceSyncState {
            binding: crate::to_contract(binding),
            status: notryx_protocol::WorkspaceSyncStatus::Offline,
        });
    };
    let binding = registration_binding(session.binding());
    if state
        .store
        .current_registration()
        .await
        .map_err(|error| error.to_string())?
        .as_ref()
        != Some(&binding)
    {
        return Err("stale_registration".into());
    }
    let mut current = state
        .sync_state
        .as_ref()
        .filter(|current| current.binding == crate::to_contract(binding.clone()))
        .cloned()
        .ok_or_else(|| "stale_registration".to_string())?;
    if state.transmission_gate.blocked(&binding) {
        current.status = match state.transmission_gate.blocked_status(&binding) {
            notryx_protocol::WorkspaceSendStatus::AuthRequired => {
                notryx_protocol::WorkspaceSyncStatus::AuthRequired
            }
            notryx_protocol::WorkspaceSendStatus::SyncPending
                if current.status == notryx_protocol::WorkspaceSyncStatus::Ready =>
            {
                notryx_protocol::WorkspaceSyncStatus::Connecting
            }
            notryx_protocol::WorkspaceSendStatus::SyncPending => current.status,
            _ => notryx_protocol::WorkspaceSyncStatus::NeedsReview,
        };
    }
    Ok(current)
}

async fn set_sync_status(
    runtime: &Runtime,
    session: &Arc<Session>,
    status: notryx_protocol::WorkspaceSyncStatus,
    app: &Option<tauri::AppHandle>,
) {
    let Ok(mut state) = checked_registration(runtime, session).await else {
        return;
    };
    let status = if status == notryx_protocol::WorkspaceSyncStatus::Ready
        && state
            .transmission_gate
            .blocked(&registration_binding(session.binding()))
    {
        notryx_protocol::WorkspaceSyncStatus::NeedsReview
    } else {
        status
    };
    let value = notryx_protocol::WorkspaceSyncState {
        binding: crate::to_contract(registration_binding(session.binding())),
        status,
    };
    if state.sync_state.as_ref() == Some(&value) {
        return;
    }
    state.sync_state = Some(value);
    drop(state);
    notify(app, "workspace-sync-status-changed");
}

pub(crate) async fn start_background_sync(
    runtime: &Runtime,
    session: Arc<Session>,
    app: Option<tauri::AppHandle>,
) {
    let mut state = match checked_registration(runtime, &session).await {
        Ok(state) => state,
        Err(_) => return,
    };
    let Some(token) = state.initial_sync.clone() else {
        return;
    };
    let foreground = state.foreground.clone();
    if let Some(previous) = state.sync_task.take() {
        previous.abort();
    }
    let runtime = Runtime(runtime.0.clone());
    state.sync_task = Some(tokio::spawn(async move {
        let renewal = crate::credential_renewal::run(
            Runtime(runtime.0.clone()),
            session.clone(),
            foreground.clone(),
            app.clone(),
        );
        tokio::select! {
            _ = run_background(runtime, session, token, foreground, app) => {},
            _ = renewal => {},
        }
    }));
}

fn notify(app: &Option<tauri::AppHandle>, name: &str) {
    if let Some(app) = app {
        let _ = app.emit_to("main", name, ());
    }
}

fn retry_delay(attempt: u32) -> std::time::Duration {
    let cap = (1_000u64 << attempt.min(5)).min(30_000);
    let minimum = (cap * 3 / 4).max(1_000);
    let jitter = uuid::Uuid::new_v4().as_u128() as u64 % (cap - minimum + 1);
    std::time::Duration::from_millis(minimum + jitter)
}

async fn run_background(
    runtime: Runtime,
    session: Arc<Session>,
    token: crate::send::InitialSyncToken,
    foreground: Arc<ForegroundSignal>,
    app: Option<tauri::AppHandle>,
) {
    let mut attempt = 0;
    let (mut cursor, mut events, mut stream_generation) = loop {
        foreground.wait_until_active().await;
        match run_initial_sync(&runtime, &session, &token).await {
            Ok(result) => break result,
            Err(
                error @ (InitialSyncError::Bootstrap(
                    crate::sync_bootstrap::BootstrapError::Remote(
                        notryx_client_runtime::Error::ResyncRequired,
                        _,
                    ),
                )
                | InitialSyncError::Bootstrap(crate::sync_bootstrap::BootstrapError::Store(
                    notryx_client_store::StoreError::BootstrapHistoryExpired,
                ))
                | InitialSyncError::CatchUp(
                    crate::sync_catchup::CatchUpError::ResyncRequired,
                )),
            ) => {
                if matches!(error, InitialSyncError::Bootstrap(_)) {
                    let Ok(mut state) = checked_registration(&runtime, &session).await else {
                        return;
                    };
                    let binding = registration_binding(session.binding());
                    if state.initial_sync.as_ref() != Some(&token)
                        || !state
                            .transmission_gate
                            .initial_sync_current(&binding, &token)
                    {
                        return;
                    }
                    state
                        .transmission_gate
                        .block(&binding, crate::send::TransmissionBlock::ResyncRequired);
                    drop(state);
                }
                foreground.wait_until_active().await;
                set_sync_status(
                    &runtime,
                    &session,
                    notryx_protocol::WorkspaceSyncStatus::Connecting,
                    &app,
                )
                .await;
                match recover_resync(&runtime, &session, &foreground).await {
                    Ok(result) => break result,
                    Err(error) => {
                        handle_initial_error(&runtime, &session, &error, &app).await;
                        return;
                    }
                }
            }
            Err(error) => {
                if !handle_initial_error(&runtime, &session, &error, &app).await {
                    return;
                }
                set_sync_status(
                    &runtime,
                    &session,
                    notryx_protocol::WorkspaceSyncStatus::Offline,
                    &app,
                )
                .await;
                tokio::time::sleep(retry_delay(attempt)).await;
                attempt = attempt.saturating_add(1);
            }
        }
    };
    set_sync_status(
        &runtime,
        &session,
        notryx_protocol::WorkspaceSyncStatus::Ready,
        &app,
    )
    .await;
    notify(&app, "workspace-metadata-changed");
    let mut heartbeat = tokio::time::interval_at(
        tokio::time::Instant::now() + std::time::Duration::from_secs(30),
        std::time::Duration::from_secs(30),
    );
    loop {
        let mut needs_resync = false;
        let mut stale_stream = false;
        let disconnected = tokio::select! {
            event = events.next_head() => match event {
                Ok(Some(hint)) => {
                    if hint.head_seq > cursor {
                        match crate::sync_catchup::catch_up_for_generation(&runtime, None, &session, Some(stream_generation)).await {
                            Ok(next) => { cursor = next; set_sync_status(&runtime, &session, notryx_protocol::WorkspaceSyncStatus::Ready, &app).await; notify(&app, "workspace-metadata-changed"); }
                            Err(crate::sync_catchup::CatchUpError::ResyncRequired) => { needs_resync = true; }
                            Err(crate::sync_catchup::CatchUpError::StaleSync) => { stale_stream = true; }
                            Err(error) => { if !handle_catch_up_error(&runtime, &session, error, &app).await { return; } }
                        }
                    }
                    needs_resync || stale_stream
                }
                Ok(None) | Err(notryx_client_runtime::Error::Network) => true,
                Err(error) => {
                    if !handle_remote_error(&runtime, &session, error, &app, Some(stream_generation)).await { return; }
                    true
                }
            },
            _ = heartbeat.tick(), if foreground.is_active() => {
                match catch_up_background(&runtime, &session).await {
                    Ok(next) => { if next > cursor { cursor = next; notify(&app, "workspace-metadata-changed"); } set_sync_status(&runtime, &session, notryx_protocol::WorkspaceSyncStatus::Ready, &app).await; }
                    Err(crate::sync_catchup::CatchUpError::ResyncRequired) => { needs_resync = true; }
                    Err(error) => { if !handle_catch_up_error(&runtime, &session, error, &app).await { return; } }
                }
                needs_resync
            },
            _ = foreground.wake.notified() => {
                if foreground.is_active() {
                    match catch_up_background(&runtime, &session).await {
                        Ok(next) => {
                            if next > cursor { cursor = next; notify(&app, "workspace-metadata-changed"); }
                            set_sync_status(&runtime, &session, notryx_protocol::WorkspaceSyncStatus::Ready, &app).await;
                        }
                        Err(crate::sync_catchup::CatchUpError::ResyncRequired) => { needs_resync = true; }
                        Err(error) => { if !handle_catch_up_error(&runtime, &session, error, &app).await { return; } }
                    }
                    needs_resync
                } else {
                    true
                }
            }
        };
        if !disconnected {
            continue;
        }
        drop(events);
        let mut resumed = !foreground.is_active();
        foreground.wait_until_active().await;
        if needs_resync {
            set_sync_status(
                &runtime,
                &session,
                notryx_protocol::WorkspaceSyncStatus::Connecting,
                &app,
            )
            .await;
            match recover_resync(&runtime, &session, &foreground).await {
                Ok((next, stream, generation)) => {
                    cursor = next;
                    events = stream;
                    stream_generation = generation;
                    set_sync_status(
                        &runtime,
                        &session,
                        notryx_protocol::WorkspaceSyncStatus::Ready,
                        &app,
                    )
                    .await;
                    notify(&app, "workspace-metadata-changed");
                    continue;
                }
                Err(error) => {
                    handle_initial_error(&runtime, &session, &error, &app).await;
                    return;
                }
            }
        }
        set_sync_status(
            &runtime,
            &session,
            notryx_protocol::WorkspaceSyncStatus::Offline,
            &app,
        )
        .await;
        let mut retries = 0;
        (events, stream_generation) = 'reconnect: loop {
            if !foreground.is_active() {
                foreground.wait_until_active().await;
                resumed = true;
            }
            if !resumed {
                tokio::select! {
                _ = tokio::time::sleep(retry_delay(retries)) => {},
                _ = foreground.wake.notified() => {
                    if !foreground.is_active() {
                        foreground.wait_until_active().await;
                    }
                },
                _ = heartbeat.tick(), if foreground.is_active() => {
                    match catch_up_background(&runtime, &session).await {
                        Ok(next) => { if next > cursor { cursor = next; notify(&app, "workspace-metadata-changed"); } }
                        Err(crate::sync_catchup::CatchUpError::ResyncRequired) => {
                            set_sync_status(&runtime, &session, notryx_protocol::WorkspaceSyncStatus::Connecting, &app).await;
                            match recover_resync(&runtime, &session, &foreground).await {
                                Ok((next, stream, generation)) => {
                                    cursor = next;
                                    set_sync_status(&runtime, &session, notryx_protocol::WorkspaceSyncStatus::Ready, &app).await;
                                    notify(&app, "workspace-metadata-changed");
                                    break 'reconnect (stream, generation);
                                }
                                Err(error) => { handle_initial_error(&runtime, &session, &error, &app).await; return; }
                            }
                        }
                        Err(error) => { if !handle_catch_up_error(&runtime, &session, error, &app).await { return; } }
                    }
                    continue;
                }
                }
            }
            resumed = false;
            let request_generation = match checked_registration(&runtime, &session).await {
                Ok(state) => {
                    let binding = registration_binding(session.binding());
                    let generation = state.transmission_gate.response_generation(&binding);
                    if !state
                        .transmission_gate
                        .metadata_response_trusted(&binding, generation)
                    {
                        return;
                    }
                    generation
                }
                Err(_) => return,
            };
            match session.events().await {
                Ok(stream) => {
                    match crate::sync_catchup::catch_up_for_generation(
                        &runtime,
                        None,
                        &session,
                        Some(request_generation),
                    )
                    .await
                    {
                        Ok(next) => {
                            if next > cursor {
                                cursor = next;
                                notify(&app, "workspace-metadata-changed");
                            }
                            set_sync_status(
                                &runtime,
                                &session,
                                notryx_protocol::WorkspaceSyncStatus::Ready,
                                &app,
                            )
                            .await;
                        }
                        Err(crate::sync_catchup::CatchUpError::ResyncRequired) => {
                            drop(stream);
                            set_sync_status(
                                &runtime,
                                &session,
                                notryx_protocol::WorkspaceSyncStatus::Connecting,
                                &app,
                            )
                            .await;
                            match recover_resync(&runtime, &session, &foreground).await {
                                Ok((next, recovered, generation)) => {
                                    cursor = next;
                                    set_sync_status(
                                        &runtime,
                                        &session,
                                        notryx_protocol::WorkspaceSyncStatus::Ready,
                                        &app,
                                    )
                                    .await;
                                    notify(&app, "workspace-metadata-changed");
                                    break 'reconnect (recovered, generation);
                                }
                                Err(error) => {
                                    handle_initial_error(&runtime, &session, &error, &app).await;
                                    return;
                                }
                            }
                        }
                        Err(crate::sync_catchup::CatchUpError::StaleSync) => {
                            drop(stream);
                            retries = retries.saturating_add(1);
                            continue 'reconnect;
                        }
                        Err(error) => {
                            if !handle_catch_up_error(&runtime, &session, error, &app).await {
                                return;
                            }
                        }
                    }
                    break (stream, request_generation);
                }
                Err(error) => {
                    if !handle_remote_error(
                        &runtime,
                        &session,
                        error,
                        &app,
                        Some(request_generation),
                    )
                    .await
                    {
                        return;
                    }
                }
            }
            retries = retries.saturating_add(1);
        };
    }
}

async fn block_sync(
    runtime: &Runtime,
    session: &Arc<Session>,
    reason: crate::send::TransmissionBlock,
    app: &Option<tauri::AppHandle>,
) {
    if let Ok(mut state) = checked_registration(runtime, session).await {
        let binding = registration_binding(session.binding());
        if !state.transmission_gate.has_block(&binding, reason) {
            state.transmission_gate.block(&binding, reason);
        }
        state.sync_state = Some(notryx_protocol::WorkspaceSyncState {
            binding: crate::to_contract(binding),
            status: if reason == crate::send::TransmissionBlock::AuthRequired {
                notryx_protocol::WorkspaceSyncStatus::AuthRequired
            } else {
                notryx_protocol::WorkspaceSyncStatus::NeedsReview
            },
        });
        drop(state);
        notify(app, "workspace-sync-status-changed");
    }
}

pub(crate) async fn handle_remote_error(
    runtime: &Runtime,
    session: &Arc<Session>,
    error: notryx_client_runtime::Error,
    app: &Option<tauri::AppHandle>,
    expected_generation: Option<u64>,
) -> bool {
    use crate::send::TransmissionBlock;
    use notryx_client_runtime::Error;
    if let Some(generation) = expected_generation {
        let Ok(mut state) = checked_registration(runtime, session).await else {
            return false;
        };
        let binding = registration_binding(session.binding());
        if !state
            .transmission_gate
            .metadata_response_trusted(&binding, generation)
        {
            return !state.transmission_gate.blocked(&binding);
        }
        let (status, reason, retry) = match error {
            Error::Network | Error::Unavailable | Error::Response => {
                (notryx_protocol::WorkspaceSyncStatus::Offline, None, true)
            }
            Error::Binding | Error::EpochMismatch => (
                notryx_protocol::WorkspaceSyncStatus::NeedsReview,
                Some(TransmissionBlock::BindingMismatch),
                false,
            ),
            Error::Contract | Error::TooLarge => (
                notryx_protocol::WorkspaceSyncStatus::NeedsReview,
                Some(TransmissionBlock::ProtocolMismatch),
                false,
            ),
            Error::ResyncRequired => (
                notryx_protocol::WorkspaceSyncStatus::NeedsReview,
                Some(TransmissionBlock::ResyncRequired),
                false,
            ),
            Error::Unauthorized => (
                notryx_protocol::WorkspaceSyncStatus::AuthRequired,
                Some(TransmissionBlock::AuthRequired),
                false,
            ),
            Error::GatewayAuthRequired => (
                notryx_protocol::WorkspaceSyncStatus::GatewayAuthRequired,
                None,
                false,
            ),
            _ => (
                notryx_protocol::WorkspaceSyncStatus::NeedsReview,
                None,
                false,
            ),
        };
        if let Some(reason) = reason
            && !state.transmission_gate.has_block(&binding, reason)
        {
            state.transmission_gate.block(&binding, reason);
        }
        state.sync_state = Some(notryx_protocol::WorkspaceSyncState {
            binding: crate::to_contract(binding),
            status,
        });
        drop(state);
        notify(app, "workspace-sync-status-changed");
        return retry;
    }
    match error {
        Error::Network | Error::Unavailable | Error::Response => {
            set_sync_status(
                runtime,
                session,
                notryx_protocol::WorkspaceSyncStatus::Offline,
                app,
            )
            .await;
            true
        }
        Error::Binding | Error::EpochMismatch => {
            block_sync(runtime, session, TransmissionBlock::BindingMismatch, app).await;
            false
        }
        Error::Contract | Error::TooLarge => {
            block_sync(runtime, session, TransmissionBlock::ProtocolMismatch, app).await;
            false
        }
        Error::ResyncRequired => {
            block_sync(runtime, session, TransmissionBlock::ResyncRequired, app).await;
            false
        }
        Error::Unauthorized => {
            block_sync(runtime, session, TransmissionBlock::AuthRequired, app).await;
            false
        }
        Error::GatewayAuthRequired => {
            set_sync_status(
                runtime,
                session,
                notryx_protocol::WorkspaceSyncStatus::GatewayAuthRequired,
                app,
            )
            .await;
            false
        }
        _ => {
            set_sync_status(
                runtime,
                session,
                notryx_protocol::WorkspaceSyncStatus::NeedsReview,
                app,
            )
            .await;
            false
        }
    }
}

async fn handle_catch_up_error(
    runtime: &Runtime,
    session: &Arc<Session>,
    error: crate::sync_catchup::CatchUpError,
    app: &Option<tauri::AppHandle>,
) -> bool {
    use crate::sync_catchup::CatchUpError;
    match error {
        CatchUpError::Network(error, generation) => {
            handle_remote_error(runtime, session, error, app, Some(generation)).await
        }
        CatchUpError::StaleSync => true,
        CatchUpError::ServerNotReady(generation) => {
            handle_remote_error(
                runtime,
                session,
                notryx_client_runtime::Error::Unavailable,
                app,
                Some(generation),
            )
            .await
        }
        CatchUpError::Guard(_) => false,
        CatchUpError::Store(_) | CatchUpError::BootstrapRequired => {
            set_sync_status(
                runtime,
                session,
                notryx_protocol::WorkspaceSyncStatus::NeedsReview,
                app,
            )
            .await;
            false
        }
        CatchUpError::HistoryRegression
        | CatchUpError::ResyncRequired
        | CatchUpError::BindingMismatch
        | CatchUpError::ProtocolMismatch => {
            set_sync_status(
                runtime,
                session,
                notryx_protocol::WorkspaceSyncStatus::NeedsReview,
                app,
            )
            .await;
            false
        }
    }
}

async fn handle_initial_error(
    runtime: &Runtime,
    session: &Arc<Session>,
    error: &InitialSyncError,
    app: &Option<tauri::AppHandle>,
) -> bool {
    use crate::sync_bootstrap::BootstrapError;
    use notryx_client_store::StoreError;
    match error {
        InitialSyncError::Bootstrap(BootstrapError::Remote(error, generation)) => {
            handle_remote_error(runtime, session, *error, app, Some(*generation)).await
        }
        InitialSyncError::Bootstrap(BootstrapError::ServerNotReady(generation)) => {
            handle_remote_error(
                runtime,
                session,
                notryx_client_runtime::Error::Unavailable,
                app,
                Some(*generation),
            )
            .await
        }
        InitialSyncError::Bootstrap(BootstrapError::Store(StoreError::ReconciliationRequired)) => {
            set_sync_status(
                runtime,
                session,
                notryx_protocol::WorkspaceSyncStatus::NeedsReview,
                app,
            )
            .await;
            false
        }
        InitialSyncError::Bootstrap(BootstrapError::Store(_)) => {
            set_sync_status(
                runtime,
                session,
                notryx_protocol::WorkspaceSyncStatus::NeedsReview,
                app,
            )
            .await;
            false
        }
        InitialSyncError::CatchUp(error) => match error {
            crate::sync_catchup::CatchUpError::Network(error, generation) => {
                handle_remote_error(runtime, session, *error, app, Some(*generation)).await
            }
            crate::sync_catchup::CatchUpError::StaleSync => true,
            crate::sync_catchup::CatchUpError::ServerNotReady(generation) => {
                handle_remote_error(
                    runtime,
                    session,
                    notryx_client_runtime::Error::Unavailable,
                    app,
                    Some(*generation),
                )
                .await
            }
            _ => {
                set_sync_status(
                    runtime,
                    session,
                    notryx_protocol::WorkspaceSyncStatus::NeedsReview,
                    app,
                )
                .await;
                false
            }
        },
        InitialSyncError::Bootstrap(BootstrapError::Local(error))
        | InitialSyncError::Guard(error) => {
            if error != "stale_registration" && error != "stale_renderer_session" {
                set_sync_status(
                    runtime,
                    session,
                    notryx_protocol::WorkspaceSyncStatus::NeedsReview,
                    app,
                )
                .await;
            }
            false
        }
        InitialSyncError::Superseded => false,
    }
}

#[cfg(test)]
pub(crate) async fn bootstrap_background(
    runtime: &Runtime,
    session: &Arc<Session>,
) -> Result<
    (notryx_domain::ChangeSeq, notryx_client_runtime::EventStream),
    crate::sync_bootstrap::BootstrapError,
> {
    bootstrap_background_for_generation(runtime, session, None).await
}

async fn bootstrap_background_for_generation(
    runtime: &Runtime,
    session: &Arc<Session>,
    expected_generation: Option<u64>,
) -> Result<
    (notryx_domain::ChangeSeq, notryx_client_runtime::EventStream),
    crate::sync_bootstrap::BootstrapError,
> {
    use crate::sync_bootstrap::{BootstrapError, bootstrap_with_initial, metadata_network_result};

    let state = checked_registration(runtime, session)
        .await
        .map_err(BootstrapError::Local)?;
    let binding = registration_binding(session.binding());
    let generation = expected_generation
        .unwrap_or_else(|| state.transmission_gate.response_generation(&binding));
    if !state
        .transmission_gate
        .metadata_response_trusted(&binding, generation)
    {
        return Err(BootstrapError::Local("stale_sync".into()));
    }
    drop(state);
    let initial = metadata_network_result(
        runtime,
        None,
        session,
        generation,
        session.server_info().await,
    )
    .await?;
    drop(
        checked_registration(runtime, session)
            .await
            .map_err(BootstrapError::Local)?,
    );
    let events =
        metadata_network_result(runtime, None, session, generation, session.events().await).await?;
    let cursor = bootstrap_with_initial(runtime, None, session, initial, generation).await?;
    Ok((cursor, events))
}

pub(crate) async fn catch_up_background(
    runtime: &Runtime,
    session: &Arc<Session>,
) -> Result<notryx_domain::ChangeSeq, crate::sync_catchup::CatchUpError> {
    crate::sync_catchup::catch_up(runtime, None, session).await
}

pub(crate) async fn run_initial_sync(
    runtime: &Runtime,
    session: &Arc<Session>,
    token: &crate::send::InitialSyncToken,
) -> Result<
    (
        notryx_domain::ChangeSeq,
        notryx_client_runtime::EventStream,
        u64,
    ),
    InitialSyncError,
> {
    run_initial_sync_inner(runtime, session, token, false).await
}

async fn run_initial_sync_inner(
    runtime: &Runtime,
    session: &Arc<Session>,
    token: &crate::send::InitialSyncToken,
    recovering_resync: bool,
) -> Result<
    (
        notryx_domain::ChangeSeq,
        notryx_client_runtime::EventStream,
        u64,
    ),
    InitialSyncError,
> {
    let state = checked_registration(runtime, session)
        .await
        .map_err(InitialSyncError::Guard)?;
    let binding = registration_binding(session.binding());
    if state.initial_sync.as_ref() != Some(token)
        || !state
            .transmission_gate
            .initial_sync_current(&binding, token)
    {
        return Err(InitialSyncError::Superseded);
    }
    let generation = state.transmission_gate.response_generation(&binding);
    drop(state);
    let (_, events) = bootstrap_background_for_generation(runtime, session, Some(generation))
        .await
        .map_err(InitialSyncError::Bootstrap)?;
    let cursor =
        crate::sync_catchup::catch_up_for_generation(runtime, None, session, Some(generation))
            .await
            .map_err(InitialSyncError::CatchUp)?;
    let mut state = checked_registration(runtime, session)
        .await
        .map_err(InitialSyncError::Guard)?;
    let binding = registration_binding(session.binding());
    if state.initial_sync.as_ref() != Some(token)
        || (recovering_resync
            && !state
                .transmission_gate
                .has_block(&binding, crate::send::TransmissionBlock::ResyncRequired))
        || !state
            .transmission_gate
            .complete_initial_sync(&binding, token)
    {
        return Err(InitialSyncError::Superseded);
    }
    if recovering_resync {
        state
            .transmission_gate
            .clear(&binding, crate::send::TransmissionBlock::ResyncRequired);
    }
    state.initial_sync = None;
    Ok((cursor, events, generation))
}

async fn recover_resync(
    runtime: &Runtime,
    session: &Arc<Session>,
    foreground: &ForegroundSignal,
) -> Result<
    (
        notryx_domain::ChangeSeq,
        notryx_client_runtime::EventStream,
        u64,
    ),
    InitialSyncError,
> {
    let mut token = begin_resync(runtime, session).await?;
    let mut attempt = 0;
    loop {
        foreground.wait_until_active().await;
        match run_initial_sync_inner(runtime, session, &token, true).await {
            Err(InitialSyncError::Bootstrap(crate::sync_bootstrap::BootstrapError::Remote(
                notryx_client_runtime::Error::Network
                | notryx_client_runtime::Error::Unavailable
                | notryx_client_runtime::Error::Response,
                _,
            )))
            | Err(InitialSyncError::Bootstrap(
                crate::sync_bootstrap::BootstrapError::ServerNotReady(_),
            ))
            | Err(InitialSyncError::CatchUp(crate::sync_catchup::CatchUpError::Network(
                notryx_client_runtime::Error::Network
                | notryx_client_runtime::Error::Unavailable
                | notryx_client_runtime::Error::Response,
                _,
            )))
            | Err(InitialSyncError::CatchUp(
                crate::sync_catchup::CatchUpError::ServerNotReady(_)
                | crate::sync_catchup::CatchUpError::StaleSync,
            )) => {}
            Err(InitialSyncError::Bootstrap(crate::sync_bootstrap::BootstrapError::Remote(
                notryx_client_runtime::Error::ResyncRequired,
                _,
            )))
            | Err(InitialSyncError::Bootstrap(crate::sync_bootstrap::BootstrapError::Store(
                notryx_client_store::StoreError::BootstrapHistoryExpired,
            ))) => {
                let mut state = checked_registration(runtime, session)
                    .await
                    .map_err(InitialSyncError::Guard)?;
                let binding = registration_binding(session.binding());
                if state.initial_sync.as_ref() != Some(&token)
                    || !state
                        .transmission_gate
                        .initial_sync_current(&binding, &token)
                {
                    return Err(InitialSyncError::Superseded);
                }
                state
                    .transmission_gate
                    .block(&binding, crate::send::TransmissionBlock::ResyncRequired);
                token = state
                    .transmission_gate
                    .begin_initial_sync(&binding)
                    .ok_or(InitialSyncError::Superseded)?;
                state.initial_sync = Some(token.clone());
            }
            Err(InitialSyncError::CatchUp(crate::sync_catchup::CatchUpError::ResyncRequired)) => {
                token = begin_resync(runtime, session).await?;
            }
            result => return result,
        }
        tokio::time::sleep(retry_delay(attempt)).await;
        attempt = attempt.saturating_add(1);
    }
}

async fn begin_resync(
    runtime: &Runtime,
    session: &Arc<Session>,
) -> Result<crate::send::InitialSyncToken, InitialSyncError> {
    let mut state = checked_registration(runtime, session)
        .await
        .map_err(InitialSyncError::Guard)?;
    let binding = registration_binding(session.binding());
    if !state
        .transmission_gate
        .has_block(&binding, crate::send::TransmissionBlock::ResyncRequired)
    {
        return Err(InitialSyncError::Superseded);
    }
    let token = state
        .transmission_gate
        .begin_initial_sync(&binding)
        .ok_or(InitialSyncError::Superseded)?;
    state.initial_sync = Some(token.clone());
    Ok(token)
}

#[derive(Debug)]
pub(crate) enum InitialSyncError {
    Bootstrap(crate::sync_bootstrap::BootstrapError),
    CatchUp(crate::sync_catchup::CatchUpError),
    Guard(String),
    Superseded,
}

pub(crate) async fn checked_registration<'a>(
    runtime: &'a Runtime,
    session: &Arc<Session>,
) -> Result<MutexGuard<'a, RuntimeState>, String> {
    let mut state = runtime.0.lock().await;
    if !state
        .session
        .as_ref()
        .is_some_and(|current| Arc::ptr_eq(current, session))
        || state
            .store
            .current_registration()
            .await
            .map_err(|error| error.to_string())?
            .as_ref()
            != Some(&registration_binding(session.binding()))
    {
        return Err("stale_registration".into());
    }
    Ok(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync_bootstrap::tests::setup;
    use uuid::Uuid;

    async fn complete_test_pair(
        runtime: &Runtime,
        pair: crate::gate::PairSnapshot,
        session: Arc<Session>,
        info: notryx_protocol::ServerInfo,
    ) -> Result<notryx_protocol::ServerInfo, String> {
        {
            let mut state = runtime.0.lock().await;
            state.pending_pair = Some(session.clone());
            state.gate.validate_pair(&pair).map_err(str::to_owned)?;
            let binding = registration_binding(session.binding());
            if state
                .store
                .current_registration()
                .await
                .map_err(|error| error.to_string())?
                .as_ref()
                == Some(&binding)
            {
                state.gate.commit_pair(&pair).map_err(str::to_owned)?;
                let token = state
                    .transmission_gate
                    .begin_initial_sync(&binding)
                    .ok_or("initial_sync_unavailable")?;
                state.initial_sync = Some(token);
                state.sync_state = Some(notryx_protocol::WorkspaceSyncState {
                    binding: crate::to_contract(binding),
                    status: notryx_protocol::WorkspaceSyncStatus::Connecting,
                });
                state.session = Some(session);
                state.pending_pair = None;
                return Ok(info);
            }
        }
        crate::credential_native::install_paired_for_test(runtime, &pair, session).await?;
        Ok(info)
    }

    async fn resync_proxy(
        target: std::net::SocketAddr,
    ) -> (
        std::net::SocketAddr,
        Arc<std::sync::atomic::AtomicUsize>,
        Arc<std::sync::atomic::AtomicUsize>,
        Arc<tokio::sync::Notify>,
        Arc<std::sync::atomic::AtomicBool>,
        Arc<tokio::sync::Notify>,
        Arc<tokio::sync::Notify>,
        tokio::task::JoinHandle<()>,
    ) {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::{TcpListener, TcpStream};
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let inject = Arc::new(AtomicUsize::new(0));
        let injected = Arc::new(AtomicUsize::new(0));
        let disconnect = Arc::new(tokio::sync::Notify::new());
        let pause_inventory = Arc::new(AtomicBool::new(false));
        let observed = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let flag = inject.clone();
        let count = injected.clone();
        let disconnect_stream = disconnect.clone();
        let pause = pause_inventory.clone();
        let observed_inventory = observed.clone();
        let release_inventory = release.clone();
        let task = tokio::spawn(async move {
            let mut connections = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let Ok((mut client, _)) = accepted else { break };
                        let flag = flag.clone();
                        let count = count.clone();
                        let disconnect_stream = disconnect_stream.clone();
                        let pause = pause.clone();
                        let observed_inventory = observed_inventory.clone();
                        let release_inventory = release_inventory.clone();
                        connections.spawn(async move {
                            let mut request = Vec::new();
                            let mut buffer = [0u8; 4096];
                            let header_end = loop {
                                let Ok(length) = client.read(&mut buffer).await else { return };
                                if length == 0 { return; }
                                request.extend_from_slice(&buffer[..length]);
                                if let Some(end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") { break end; }
                                if request.len() > 65536 { return; }
                            };
                            let Ok(header) = std::str::from_utf8(&request[..header_end]) else { return };
                            if header.starts_with("GET /v1/nodes?") && pause.swap(false, Ordering::SeqCst) {
                                observed_inventory.notify_one();
                                release_inventory.notified().await;
                            }
                            let mode = flag.load(Ordering::SeqCst);
                            if (101..=104).contains(&mode) && header.starts_with("GET /v1/server-info") {
                                let next = if mode == 101 { 2 } else { mode - 1 };
                                let _ = flag.compare_exchange(mode, next, Ordering::SeqCst, Ordering::SeqCst);
                            }
                            if (201..=204).contains(&mode) && header.starts_with("GET /v1/server-info") {
                                let next = if mode == 201 { 5 } else { mode - 1 };
                                let _ = flag.compare_exchange(mode, next, Ordering::SeqCst, Ordering::SeqCst);
                            }
                            let mode = flag.load(Ordering::SeqCst);
                            let expire_floor = mode == 6
                                && header.starts_with("GET /v1/server-info")
                                && flag.compare_exchange(6, 0, Ordering::SeqCst, Ordering::SeqCst).is_ok();
                            let not_ready = mode == 7 && header.starts_with("GET /v1/server-info");
                            let should_inject = (mode == 1 && header.starts_with("GET /v1/changes?"))
                                || (mode == 3 && header.starts_with("GET /v1/changes?"))
                                || (mode == 4 && header.starts_with("GET /v1/changes?"))
                                || ((mode == 2 || mode == 5) && header.starts_with("GET /v1/server-info"));
                            let next = match mode { 3 => 104, 4 => 204, _ => 0 };
                            if should_inject && flag.compare_exchange(mode, next, Ordering::SeqCst, Ordering::SeqCst).is_ok() {
                                count.fetch_add(1, Ordering::SeqCst);
                                let (status, code) = if mode != 2 { ("410 Gone", "resync_required") } else { ("503 Service Unavailable", "unavailable") };
                                let body = serde_json::json!({"code":code,"message":"synthetic","request_id":Uuid::now_v7().to_string()}).to_string();
                                let response = format!("HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                                let _ = client.write_all(response.as_bytes()).await;
                                return;
                            }
                            let is_stream = header.starts_with("GET /v1/events");
                            let mut forwarded = header.lines()
                                .filter(|line| !line.to_ascii_lowercase().starts_with("connection:"))
                                .map(|line| format!("{line}\r\n"))
                                .collect::<String>().into_bytes();
                            forwarded.extend_from_slice(b"Connection: close\r\n\r\n");
                            forwarded.extend_from_slice(&request[header_end + 4..]);
                            let Ok(mut server) = TcpStream::connect(target).await else { return };
                            if server.write_all(&forwarded).await.is_err() { return; }
                            if expire_floor || not_ready {
                                let mut response = Vec::new();
                                if server.read_to_end(&mut response).await.is_err() { return; }
                                let Some(end) = response.windows(4).position(|bytes| bytes == b"\r\n\r\n") else { return };
                                let Ok(mut body) = serde_json::from_slice::<serde_json::Value>(&response[end + 4..]) else { return };
                                if expire_floor {
                                    body["journal_floor_seq"] = body["head_seq"].clone();
                                } else {
                                    body["ready"] = serde_json::json!(false);
                                }
                                let body = body.to_string();
                                let reply = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                                count.fetch_add(1, Ordering::SeqCst);
                                let _ = client.write_all(reply.as_bytes()).await;
                                return;
                            }
                            if is_stream {
                                tokio::select! {
                                    _ = tokio::io::copy_bidirectional(&mut client, &mut server) => {},
                                    _ = disconnect_stream.notified() => {},
                                }
                            } else {
                                let _ = tokio::io::copy_bidirectional(&mut client, &mut server).await;
                            }
                        });
                    }
                    Some(_) = connections.join_next(), if !connections.is_empty() => {}
                }
            }
        });
        (
            address,
            inject,
            injected,
            disconnect,
            pause_inventory,
            observed,
            release,
            task,
        )
    }

    #[tokio::test]
    async fn background_task_rebootstraps_after_http_410_and_preserves_new_registration_draft() {
        use notryx_client_runtime::ContentMutation;
        use notryx_server::config::{Config, ServerConfig, WorkspaceConfig};
        use std::sync::atomic::Ordering;
        use std::time::Duration;

        let (directory, server, runtime, original, _) = setup().await;
        let node = original
            .inventory(None)
            .await
            .unwrap()
            .nodes
            .into_iter()
            .find(|node| node.name == "move.md")
            .unwrap();
        let config = Config {
            sync: Default::default(),
            server: ServerConfig {
                listen: server.local_addr(),
                admin_socket: directory.path().join("admin.sock"),
            },
            workspace: WorkspaceConfig {
                notes_root: directory.path().join("notes"),
                state_root: directory.path().join("server-state"),
                uploads: Default::default(),
            },
        };
        let (address, inject, injected, _disconnect, _pause, _observed, _release, proxy) =
            resync_proxy(server.local_addr()).await;
        let pairing = crate::sync_bootstrap::server_test_support::pairing_request(&config)
            .await
            .0;
        let session = Arc::new(
            Session::pair(
                &format!("http://{address}"),
                pairing.workspace_id,
                &pairing.pairing_secret,
            )
            .await
            .unwrap(),
        );
        let info = session.server_info().await.unwrap();
        let pair = runtime.0.lock().await.gate.begin_pair("owner").unwrap();
        crate::complete_pair_and_start(&runtime, pair, session.clone(), Ok(info), true, None)
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                if sync_state_for_renderer(&runtime, "owner")
                    .await
                    .unwrap()
                    .status
                    == notryx_protocol::WorkspaceSyncStatus::Ready
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .unwrap();
        let binding = registration_binding(session.binding());
        let document = node.node_id.to_string();
        let operation = {
            let mut state = runtime.0.lock().await;
            let opened = state
                .store
                .open_registered_document(&binding, &document, b"peer base", "peer-base-revision")
                .await
                .unwrap();
            state
                .store
                .save_registered_checkpoint(
                    &binding,
                    &document,
                    &opened.edit_session_id,
                    1,
                    b"peer draft",
                )
                .await
                .unwrap();
            state
                .store
                .close_registered_document(&binding, &document, &opened.edit_session_id)
                .await
                .unwrap();
            state
                .store
                .queue_checkpoint(&binding, &document, 1)
                .await
                .unwrap();
            let entry = state
                .store
                .list_outbox(&binding, 0, 100)
                .await
                .unwrap()
                .entries[0]
                .clone();
            state
                .store
                .read_outbox_operation(&binding, &entry.operation_id)
                .await
                .unwrap()
                .unwrap()
        };
        let pairing = crate::sync_bootstrap::server_test_support::pairing_request(&config)
            .await
            .0;
        let writer = Session::pair(
            &format!("http://{}", server.local_addr()),
            pairing.workspace_id,
            &pairing.pairing_secret,
        )
        .await
        .unwrap();
        inject.store(1, Ordering::SeqCst);
        writer
            .put_content(&ContentMutation {
                binding: writer.binding().clone(),
                operation_id: Uuid::now_v7().to_string().parse().unwrap(),
                operation_seq: 1,
                node_id: node.node_id,
                expected_revision: node.content_rev.unwrap(),
                bytes: b"remote change\n".to_vec(),
            })
            .await
            .unwrap();
        let head = writer.server_info().await.unwrap().head_seq;
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let mut state = runtime.0.lock().await;
                let done = injected.load(Ordering::SeqCst) == 1
                    && state
                        .store
                        .sync_index(&binding)
                        .await
                        .unwrap()
                        .is_some_and(|index| index.0 == head)
                    && state.sync_state.as_ref().is_some_and(|status| {
                        status.status == notryx_protocol::WorkspaceSyncStatus::Ready
                    })
                    && !state.transmission_gate.blocked(&binding);
                drop(state);
                if done {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .unwrap();
        let mut state = runtime.0.lock().await;
        assert_eq!(
            state
                .store
                .read_outbox_operation(&binding, &operation.operation_id)
                .await
                .unwrap(),
            Some(operation)
        );
        let draft = state
            .store
            .read_registered_checkpoint(&binding, &document)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(draft.bytes, b"peer draft");
        assert_eq!(draft.base_bytes, b"peer base");
        if let Some(task) = state.sync_task.take() {
            task.abort();
        }
        drop(state);
        proxy.abort();
        let _ = proxy.await;
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn initial_bootstrap_restarts_after_http_410_without_losing_local_data() {
        initial_bootstrap_restarts_after_injected_failures(1, 1, false).await;
    }

    #[tokio::test]
    async fn resync_retries_transient_catch_up_failure_without_losing_local_data() {
        initial_bootstrap_restarts_after_injected_failures(3, 2, false).await;
    }

    #[tokio::test]
    async fn resync_retries_repeated_http_410_without_losing_local_data() {
        initial_bootstrap_restarts_after_injected_failures(4, 2, false).await;
    }

    #[tokio::test]
    async fn resync_waits_until_foreground_before_retrying() {
        initial_bootstrap_restarts_after_injected_failures(3, 2, true).await;
    }

    #[tokio::test]
    async fn bootstrap_restarts_after_floor_advances_past_h0() {
        initial_bootstrap_restarts_after_injected_failures(6, 1, false).await;
    }

    async fn initial_bootstrap_restarts_after_injected_failures(
        mode: usize,
        expected: usize,
        pause_retry: bool,
    ) {
        use notryx_client_runtime::ContentMutation;
        use notryx_server::config::{Config, ServerConfig, WorkspaceConfig};
        use std::sync::atomic::Ordering;
        use std::time::Duration;

        let (directory, server, runtime, original, _) = setup().await;
        let node = original
            .inventory(None)
            .await
            .unwrap()
            .nodes
            .into_iter()
            .find(|node| node.name == "move.md")
            .unwrap();
        let config = Config {
            sync: Default::default(),
            server: ServerConfig {
                listen: server.local_addr(),
                admin_socket: directory.path().join("admin.sock"),
            },
            workspace: WorkspaceConfig {
                notes_root: directory.path().join("notes"),
                state_root: directory.path().join("server-state"),
                uploads: Default::default(),
            },
        };
        let (address, inject, injected, _disconnect, pause, observed, release, proxy) =
            resync_proxy(server.local_addr()).await;
        let pairing = crate::sync_bootstrap::server_test_support::pairing_request(&config)
            .await
            .0;
        let session = Arc::new(
            Session::pair(
                &format!("http://{address}"),
                pairing.workspace_id,
                &pairing.pairing_secret,
            )
            .await
            .unwrap(),
        );
        let info = session.server_info().await.unwrap();
        let pair = runtime.0.lock().await.gate.begin_pair("owner").unwrap();
        pause.store(true, Ordering::SeqCst);
        crate::complete_pair_and_start(&runtime, pair, session.clone(), Ok(info), true, None)
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(15), observed.notified())
            .await
            .unwrap();
        let binding = registration_binding(session.binding());
        let document = node.node_id.to_string();
        let operation = {
            let mut state = runtime.0.lock().await;
            let opened = state
                .store
                .open_registered_document(&binding, &document, b"peer base", "peer-base-revision")
                .await
                .unwrap();
            state
                .store
                .save_registered_checkpoint(
                    &binding,
                    &document,
                    &opened.edit_session_id,
                    1,
                    b"peer draft",
                )
                .await
                .unwrap();
            state
                .store
                .close_registered_document(&binding, &document, &opened.edit_session_id)
                .await
                .unwrap();
            state
                .store
                .queue_checkpoint(&binding, &document, 1)
                .await
                .unwrap();
            let id = state
                .store
                .list_outbox(&binding, 0, 100)
                .await
                .unwrap()
                .entries[0]
                .operation_id
                .clone();
            state
                .store
                .read_outbox_operation(&binding, &id)
                .await
                .unwrap()
                .unwrap()
        };
        let pairing = crate::sync_bootstrap::server_test_support::pairing_request(&config)
            .await
            .0;
        let writer = Session::pair(
            &format!("http://{}", server.local_addr()),
            pairing.workspace_id,
            &pairing.pairing_secret,
        )
        .await
        .unwrap();
        writer
            .put_content(&ContentMutation {
                binding: writer.binding().clone(),
                operation_id: Uuid::now_v7().to_string().parse().unwrap(),
                operation_seq: 1,
                node_id: node.node_id,
                expected_revision: node.content_rev.unwrap(),
                bytes: b"during initial inventory\n".to_vec(),
            })
            .await
            .unwrap();
        let head = writer.server_info().await.unwrap().head_seq;
        inject.store(mode, Ordering::SeqCst);
        release.notify_one();
        tokio::time::timeout(Duration::from_secs(15), async {
            while injected.load(Ordering::SeqCst) < 1 {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("initial bootstrap must receive the injected error or expired floor");
        if expected > 1 {
            tokio::time::timeout(Duration::from_secs(15), async {
                while injected.load(Ordering::SeqCst) < expected {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .expect("recovery catch-up must receive the second injected error");
        }
        let expected = if pause_retry {
            let foreground = runtime.0.lock().await.foreground.clone();
            foreground.set(false);
            inject.store(2, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_secs(3)).await;
            assert_eq!(injected.load(Ordering::SeqCst), expected);
            foreground.set(true);
            expected + 1
        } else {
            expected
        };
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let mut state = runtime.0.lock().await;
                let done = injected.load(Ordering::SeqCst) == expected
                    && state
                        .store
                        .sync_index(&binding)
                        .await
                        .unwrap()
                        .is_some_and(|index| index.0 == head)
                    && state.sync_state.as_ref().is_some_and(|status| {
                        status.status == notryx_protocol::WorkspaceSyncStatus::Ready
                    })
                    && !state.transmission_gate.blocked(&binding);
                drop(state);
                if done {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .unwrap();
        let mut state = runtime.0.lock().await;
        assert_eq!(
            state
                .store
                .read_outbox_operation(&binding, &operation.operation_id)
                .await
                .unwrap(),
            Some(operation)
        );
        let draft = state
            .store
            .read_registered_checkpoint(&binding, &document)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(draft.bytes, b"peer draft");
        assert_eq!(draft.base_bytes, b"peer base");
        if let Some(task) = state.sync_task.take() {
            task.abort();
        }
        drop(state);
        proxy.abort();
        let _ = proxy.await;
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn successful_catchup_restores_ready_after_reconnect_temporarily_fails() {
        use notryx_client_runtime::ContentMutation;
        use notryx_server::config::{Config, ServerConfig, WorkspaceConfig};
        use std::sync::atomic::Ordering;
        use std::time::Duration;

        let (directory, server, runtime, original, _) = setup().await;
        let node = original
            .inventory(None)
            .await
            .unwrap()
            .nodes
            .into_iter()
            .find(|node| node.name == "move.md")
            .unwrap();
        let config = Config {
            sync: Default::default(),
            server: ServerConfig {
                listen: server.local_addr(),
                admin_socket: directory.path().join("admin.sock"),
            },
            workspace: WorkspaceConfig {
                notes_root: directory.path().join("notes"),
                state_root: directory.path().join("server-state"),
                uploads: Default::default(),
            },
        };
        let (address, inject, injected, disconnect, _pause, _observed, _release, proxy) =
            resync_proxy(server.local_addr()).await;
        let pairing = crate::sync_bootstrap::server_test_support::pairing_request(&config)
            .await
            .0;
        let session = Arc::new(
            Session::pair(
                &format!("http://{address}"),
                pairing.workspace_id,
                &pairing.pairing_secret,
            )
            .await
            .unwrap(),
        );
        let info = session.server_info().await.unwrap();
        let pair = runtime.0.lock().await.gate.begin_pair("owner").unwrap();
        crate::complete_pair_and_start(&runtime, pair, session.clone(), Ok(info), true, None)
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                if sync_state_for_renderer(&runtime, "owner")
                    .await
                    .unwrap()
                    .status
                    == notryx_protocol::WorkspaceSyncStatus::Ready
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .unwrap();
        inject.store(2, Ordering::SeqCst);
        disconnect.notify_waiters();
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                if injected.load(Ordering::SeqCst) == 1
                    && sync_state_for_renderer(&runtime, "owner")
                        .await
                        .unwrap()
                        .status
                        == notryx_protocol::WorkspaceSyncStatus::Offline
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .unwrap();
        let pairing = crate::sync_bootstrap::server_test_support::pairing_request(&config)
            .await
            .0;
        let writer = Session::pair(
            &format!("http://{}", server.local_addr()),
            pairing.workspace_id,
            &pairing.pairing_secret,
        )
        .await
        .unwrap();
        writer
            .put_content(&ContentMutation {
                binding: writer.binding().clone(),
                operation_id: Uuid::now_v7().to_string().parse().unwrap(),
                operation_seq: 1,
                node_id: node.node_id,
                expected_revision: node.content_rev.unwrap(),
                bytes: b"changed after reconnect\n".to_vec(),
            })
            .await
            .unwrap();
        let head = writer.server_info().await.unwrap().head_seq;
        let binding = registration_binding(session.binding());
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let mut state = runtime.0.lock().await;
                let done = state
                    .store
                    .sync_index(&binding)
                    .await
                    .unwrap()
                    .is_some_and(|index| index.0 == head)
                    && state.sync_state.as_ref().is_some_and(|status| {
                        status.status == notryx_protocol::WorkspaceSyncStatus::Ready
                    });
                drop(state);
                if done {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .unwrap();
        if let Some(task) = runtime.0.lock().await.sync_task.take() {
            task.abort();
        }
        proxy.abort();
        let _ = proxy.await;
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn foreground_resume_reconnects_and_catches_up_after_background_disconnect() {
        use notryx_client_runtime::ContentMutation;
        use notryx_server::config::{Config, ServerConfig, WorkspaceConfig};
        use std::time::Duration;
        let (directory, server, runtime, original, _) = setup().await;
        let node = original
            .inventory(None)
            .await
            .unwrap()
            .nodes
            .into_iter()
            .find(|node| node.name == "move.md")
            .unwrap();
        let config = Config {
            sync: Default::default(),
            server: ServerConfig {
                listen: server.local_addr(),
                admin_socket: directory.path().join("admin.sock"),
            },
            workspace: WorkspaceConfig {
                notes_root: directory.path().join("notes"),
                state_root: directory.path().join("server-state"),
                uploads: Default::default(),
            },
        };
        let (address, _inject, _injected, disconnect, _pause, _observed, _release, proxy) =
            resync_proxy(server.local_addr()).await;
        let pairing = crate::sync_bootstrap::server_test_support::pairing_request(&config)
            .await
            .0;
        let session = Arc::new(
            Session::pair(
                &format!("http://{address}"),
                pairing.workspace_id,
                &pairing.pairing_secret,
            )
            .await
            .unwrap(),
        );
        let info = session.server_info().await.unwrap();
        let pair = runtime.0.lock().await.gate.begin_pair("owner").unwrap();
        crate::complete_pair_and_start(&runtime, pair, session.clone(), Ok(info), true, None)
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                if sync_state_for_renderer(&runtime, "owner")
                    .await
                    .unwrap()
                    .status
                    == notryx_protocol::WorkspaceSyncStatus::Ready
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .unwrap();
        let binding = registration_binding(session.binding());
        let (foreground, previous) = {
            let mut state = runtime.0.lock().await;
            (
                state.foreground.clone(),
                state.store.sync_index(&binding).await.unwrap().unwrap().0,
            )
        };
        foreground.set(false);
        disconnect.notify_waiters();
        let pairing = crate::sync_bootstrap::server_test_support::pairing_request(&config)
            .await
            .0;
        let writer = Session::pair(
            &format!("http://{}", server.local_addr()),
            pairing.workspace_id,
            &pairing.pairing_secret,
        )
        .await
        .unwrap();
        writer
            .put_content(&ContentMutation {
                binding: writer.binding().clone(),
                operation_id: Uuid::now_v7().to_string().parse().unwrap(),
                operation_seq: 1,
                node_id: node.node_id,
                expected_revision: node.content_rev.unwrap(),
                bytes: b"changed while background\n".to_vec(),
            })
            .await
            .unwrap();
        let head = writer.server_info().await.unwrap().head_seq;
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert_eq!(
            runtime
                .0
                .lock()
                .await
                .store
                .sync_index(&binding)
                .await
                .unwrap()
                .unwrap()
                .0,
            previous
        );
        foreground.set(true);
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                if runtime
                    .0
                    .lock()
                    .await
                    .store
                    .sync_index(&binding)
                    .await
                    .unwrap()
                    .is_some_and(|index| index.0 == head)
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .unwrap();
        if let Some(task) = runtime.0.lock().await.sync_task.take() {
            task.abort();
        }
        proxy.abort();
        let _ = proxy.await;
        server.shutdown().await.unwrap();
    }

    struct NotReadyFixture {
        _directory: tempfile::TempDir,
        server: notryx_server::http::RunningServer,
        runtime: Runtime,
        session: Arc<Session>,
        binding: notryx_client_store::RegistrationBinding,
        document: String,
        operation: notryx_client_store::OutboxOperation,
        inject: Arc<std::sync::atomic::AtomicUsize>,
        injected: Arc<std::sync::atomic::AtomicUsize>,
        proxy: tokio::task::JoinHandle<()>,
    }

    async fn not_ready_fixture() -> NotReadyFixture {
        use notryx_server::config::{Config, ServerConfig, WorkspaceConfig};
        let (directory, server, runtime, original, _) = setup().await;
        let node = original
            .inventory(None)
            .await
            .unwrap()
            .nodes
            .into_iter()
            .find(|node| node.name == "move.md")
            .unwrap();
        let config = Config {
            sync: Default::default(),
            server: ServerConfig {
                listen: server.local_addr(),
                admin_socket: directory.path().join("admin.sock"),
            },
            workspace: WorkspaceConfig {
                notes_root: directory.path().join("notes"),
                state_root: directory.path().join("server-state"),
                uploads: Default::default(),
            },
        };
        let (address, inject, injected, _, _, _, _, proxy) =
            resync_proxy(server.local_addr()).await;
        let pairing = crate::sync_bootstrap::server_test_support::pairing_request(&config)
            .await
            .0;
        let session = Arc::new(
            Session::pair(
                &format!("http://{address}"),
                pairing.workspace_id,
                &pairing.pairing_secret,
            )
            .await
            .unwrap(),
        );
        let info = session.server_info().await.unwrap();
        let pair = runtime.0.lock().await.gate.begin_pair("owner").unwrap();
        complete_test_pair(&runtime, pair, session.clone(), info)
            .await
            .unwrap();
        let binding = registration_binding(session.binding());
        let document = node.node_id.to_string();
        let operation = {
            let mut state = runtime.0.lock().await;
            let opened = state
                .store
                .open_registered_document(&binding, &document, b"peer base", "peer-base-revision")
                .await
                .unwrap();
            state
                .store
                .save_registered_checkpoint(
                    &binding,
                    &document,
                    &opened.edit_session_id,
                    1,
                    b"peer draft",
                )
                .await
                .unwrap();
            state
                .store
                .close_registered_document(&binding, &document, &opened.edit_session_id)
                .await
                .unwrap();
            state
                .store
                .queue_checkpoint(&binding, &document, 1)
                .await
                .unwrap()
        };
        NotReadyFixture {
            _directory: directory,
            server,
            runtime,
            session,
            binding,
            document,
            operation,
            inject,
            injected,
            proxy,
        }
    }

    async fn wait_sync_status(runtime: &Runtime, status: notryx_protocol::WorkspaceSyncStatus) {
        tokio::time::timeout(std::time::Duration::from_secs(15), async {
            loop {
                if sync_state_for_renderer(runtime, "owner")
                    .await
                    .unwrap()
                    .status
                    == status
                {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        })
        .await
        .unwrap();
    }

    async fn assert_not_ready_data_preserved(fixture: &NotReadyFixture) {
        let mut state = fixture.runtime.0.lock().await;
        let draft = state
            .store
            .read_registered_checkpoint(&fixture.binding, &fixture.document)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(draft.bytes, b"peer draft");
        assert_eq!(draft.base_bytes, b"peer base");
        assert_eq!(
            state
                .store
                .read_outbox_operation(&fixture.binding, &fixture.operation.operation_id)
                .await
                .unwrap(),
            Some(fixture.operation.clone())
        );
    }

    #[tokio::test]
    async fn temporary_not_ready_after_sync_is_not_reported_ready_and_recovers() {
        use std::sync::atomic::Ordering;
        let fixture = not_ready_fixture().await;
        start_background_sync(&fixture.runtime, fixture.session.clone(), None).await;
        wait_sync_status(
            &fixture.runtime,
            notryx_protocol::WorkspaceSyncStatus::Ready,
        )
        .await;
        fixture.inject.store(7, Ordering::SeqCst);
        let foreground = fixture.runtime.0.lock().await.foreground.clone();
        foreground.set(true);
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while fixture.injected.load(Ordering::SeqCst) == 0 {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        })
        .await
        .unwrap();
        wait_sync_status(
            &fixture.runtime,
            notryx_protocol::WorkspaceSyncStatus::Offline,
        )
        .await;
        assert_not_ready_data_preserved(&fixture).await;
        fixture.inject.store(0, Ordering::SeqCst);
        foreground.set(true);
        wait_sync_status(
            &fixture.runtime,
            notryx_protocol::WorkspaceSyncStatus::Ready,
        )
        .await;
        assert_not_ready_data_preserved(&fixture).await;
        if let Some(task) = fixture.runtime.0.lock().await.sync_task.take() {
            task.abort();
        }
        fixture.proxy.abort();
        fixture.server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn temporary_not_ready_during_initial_sync_retries_and_preserves_draft() {
        use std::sync::atomic::Ordering;
        let fixture = not_ready_fixture().await;
        fixture.inject.store(7, Ordering::SeqCst);
        start_background_sync(&fixture.runtime, fixture.session.clone(), None).await;
        wait_sync_status(
            &fixture.runtime,
            notryx_protocol::WorkspaceSyncStatus::Offline,
        )
        .await;
        {
            let state = fixture.runtime.0.lock().await;
            assert!(state.transmission_gate.has_block(
                &fixture.binding,
                crate::send::TransmissionBlock::InitialSync
            ));
        }
        assert_not_ready_data_preserved(&fixture).await;
        fixture.inject.store(0, Ordering::SeqCst);
        wait_sync_status(
            &fixture.runtime,
            notryx_protocol::WorkspaceSyncStatus::Ready,
        )
        .await;
        assert_not_ready_data_preserved(&fixture).await;
        if let Some(task) = fixture.runtime.0.lock().await.sync_task.take() {
            task.abort();
        }
        fixture.proxy.abort();
        fixture.server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn externally_requested_resync_recovers_with_live_sse_and_foreground_check() {
        use crate::send::TransmissionBlock;
        use std::time::Duration;

        let (_directory, server, runtime, session, document) = setup().await;
        let info = session.server_info().await.unwrap();
        let pair = runtime.0.lock().await.gate.begin_pair("owner").unwrap();
        complete_test_pair(&runtime, pair, session.clone(), info)
            .await
            .unwrap();
        start_background_sync(&runtime, session.clone(), None).await;
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                if sync_state_for_renderer(&runtime, "owner")
                    .await
                    .unwrap()
                    .status
                    == notryx_protocol::WorkspaceSyncStatus::Ready
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .unwrap();
        let binding = registration_binding(session.binding());
        let (foreground, draft, operation) = {
            let mut state = runtime.0.lock().await;
            let draft = state
                .store
                .read_registered_checkpoint(&binding, &document)
                .await
                .unwrap()
                .unwrap();
            let entry = state
                .store
                .list_outbox(&binding, 0, 100)
                .await
                .unwrap()
                .entries[0]
                .clone();
            let operation = state
                .store
                .read_outbox_operation(&binding, &entry.operation_id)
                .await
                .unwrap()
                .unwrap();
            state
                .transmission_gate
                .block(&binding, TransmissionBlock::ResyncRequired);
            (state.foreground.clone(), draft, operation)
        };
        foreground.set(true);
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                let state = runtime.0.lock().await;
                let complete = !state.transmission_gate.blocked(&binding)
                    && state.sync_state.as_ref().is_some_and(|value| {
                        value.status == notryx_protocol::WorkspaceSyncStatus::Ready
                    });
                drop(state);
                if complete {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("foreground check must restart an externally requested resync");
        let mut state = runtime.0.lock().await;
        assert_eq!(
            state
                .store
                .read_registered_checkpoint(&binding, &document)
                .await
                .unwrap()
                .unwrap(),
            draft
        );
        assert_eq!(
            state
                .store
                .read_outbox_operation(&binding, &operation.operation_id)
                .await
                .unwrap(),
            Some(operation)
        );
        if let Some(task) = state.sync_task.take() {
            task.abort();
        }
        drop(state);
        assert_eq!(
            crate::send::send_one(runtime.0.clone(), "owner")
                .await
                .unwrap()
                .status,
            notryx_protocol::WorkspaceSendStatus::Conflict
        );
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn resync_rebootstraps_without_discarding_local_edits_or_outbox() {
        let (_directory, server, runtime, session, document) = setup().await;
        let info = session.server_info().await.unwrap();
        let pair = runtime.0.lock().await.gate.begin_pair("owner").unwrap();
        complete_test_pair(&runtime, pair, session.clone(), info)
            .await
            .unwrap();
        let token = runtime.0.lock().await.initial_sync.clone().unwrap();
        let (_, events, _) = run_initial_sync(&runtime, &session, &token).await.unwrap();
        drop(events);
        let binding = registration_binding(session.binding());
        let operation = {
            let mut state = runtime.0.lock().await;
            let id = state
                .store
                .list_outbox(&binding, 0, 100)
                .await
                .unwrap()
                .entries[0]
                .operation_id
                .clone();
            let operation = state
                .store
                .read_outbox_operation(&binding, &id)
                .await
                .unwrap()
                .unwrap();
            state
                .transmission_gate
                .block(&binding, crate::send::TransmissionBlock::ResyncRequired);
            operation
        };
        let (cursor, recovered_events, _) =
            recover_resync(&runtime, &session, &ForegroundSignal::new())
                .await
                .unwrap();
        let mut state = runtime.0.lock().await;
        assert_eq!(
            state.store.sync_index(&binding).await.unwrap().unwrap().0,
            cursor
        );
        assert_eq!(
            state
                .store
                .read_outbox_operation(&binding, &operation.operation_id)
                .await
                .unwrap(),
            Some(operation)
        );
        let draft = state
            .store
            .read_registered_checkpoint(&binding, &document)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(draft.bytes, b"local draft");
        assert_eq!(draft.base_bytes, b"base");
        drop(state);
        let result = crate::send::send_one(runtime.0.clone(), "owner")
            .await
            .unwrap();
        assert_eq!(
            result.status,
            notryx_protocol::WorkspaceSendStatus::Conflict
        );
        drop(recovered_events);
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn sync_error_is_available_after_lost_event_and_renderer_reload() {
        use notryx_protocol::WorkspaceSyncStatus;
        let (_directory, server, runtime, session, _document) = setup().await;
        let info = session.server_info().await.unwrap();
        let pair = runtime.0.lock().await.gate.begin_pair("owner").unwrap();
        complete_test_pair(&runtime, pair, session.clone(), info)
            .await
            .unwrap();
        set_sync_status(&runtime, &session, WorkspaceSyncStatus::NeedsReview, &None).await;
        runtime
            .0
            .lock()
            .await
            .gate
            .replace_owner("new-owner".into());
        assert!(sync_state_for_renderer(&runtime, "owner").await.is_err());
        let current = sync_state_for_renderer(&runtime, "new-owner")
            .await
            .unwrap();
        assert_eq!(current.status, WorkspaceSyncStatus::NeedsReview);
        assert_eq!(
            current.binding,
            crate::to_contract(registration_binding(session.binding()))
        );
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn ready_cannot_hide_a_remaining_history_regression_block() {
        use crate::send::TransmissionBlock;
        use notryx_protocol::WorkspaceSyncStatus;
        let (_directory, server, runtime, session, _document) = setup().await;
        let info = session.server_info().await.unwrap();
        let pair = runtime.0.lock().await.gate.begin_pair("owner").unwrap();
        complete_test_pair(&runtime, pair, session.clone(), info)
            .await
            .unwrap();
        let binding = registration_binding(session.binding());
        runtime
            .0
            .lock()
            .await
            .transmission_gate
            .block(&binding, TransmissionBlock::HistoryRegression);
        set_sync_status(&runtime, &session, WorkspaceSyncStatus::Ready, &None).await;
        let current = sync_state_for_renderer(&runtime, "owner").await.unwrap();
        assert_eq!(current.status, WorkspaceSyncStatus::NeedsReview);
        assert!(
            runtime
                .0
                .lock()
                .await
                .transmission_gate
                .has_block(&binding, TransmissionBlock::HistoryRegression)
        );
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn send_review_gate_is_visible_after_renderer_reload() {
        use crate::send::TransmissionBlock;
        use notryx_protocol::WorkspaceSyncStatus;
        let (_directory, server, runtime, session, _document) = setup().await;
        let info = session.server_info().await.unwrap();
        let pair = runtime.0.lock().await.gate.begin_pair("owner").unwrap();
        complete_test_pair(&runtime, pair, session.clone(), info)
            .await
            .unwrap();
        let token = runtime.0.lock().await.initial_sync.clone().unwrap();
        let (_, events, _) = run_initial_sync(&runtime, &session, &token).await.unwrap();
        drop(events);
        set_sync_status(&runtime, &session, WorkspaceSyncStatus::Ready, &None).await;
        let binding = registration_binding(session.binding());
        for reason in [
            TransmissionBlock::ProtocolMismatch,
            TransmissionBlock::EpochMismatch,
            TransmissionBlock::UnverifiedResponse,
        ] {
            runtime
                .0
                .lock()
                .await
                .transmission_gate
                .block(&binding, reason);
            assert_eq!(
                sync_state_for_renderer(&runtime, "owner")
                    .await
                    .unwrap()
                    .status,
                WorkspaceSyncStatus::NeedsReview
            );
            assert!(
                runtime
                    .0
                    .lock()
                    .await
                    .transmission_gate
                    .clear(&binding, reason)
            );
        }
        runtime
            .0
            .lock()
            .await
            .transmission_gate
            .block(&binding, TransmissionBlock::UnverifiedResponse);
        runtime.0.lock().await.gate.replace_owner("reloaded".into());
        assert!(sync_state_for_renderer(&runtime, "owner").await.is_err());
        assert_eq!(
            sync_state_for_renderer(&runtime, "reloaded")
                .await
                .unwrap()
                .status,
            WorkspaceSyncStatus::NeedsReview
        );
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn ambiguous_reconciliation_error_keeps_initial_sync_blocked() {
        use crate::send::TransmissionBlock;
        use notryx_client_store::StoreError;
        use notryx_protocol::WorkspaceSyncStatus;
        let (_directory, server, runtime, session, _document) = setup().await;
        let info = session.server_info().await.unwrap();
        let pair = runtime.0.lock().await.gate.begin_pair("owner").unwrap();
        complete_test_pair(&runtime, pair, session.clone(), info)
            .await
            .unwrap();
        let error = InitialSyncError::Bootstrap(crate::sync_bootstrap::BootstrapError::Store(
            StoreError::ReconciliationRequired,
        ));
        assert!(!handle_initial_error(&runtime, &session, &error, &None).await);
        let binding = registration_binding(session.binding());
        let state = runtime.0.lock().await;
        assert!(
            state
                .transmission_gate
                .has_block(&binding, TransmissionBlock::InitialSync)
        );
        assert!(
            !state
                .transmission_gate
                .has_block(&binding, TransmissionBlock::ResyncRequired)
        );
        assert_eq!(
            state.sync_state.as_ref().unwrap().status,
            WorkspaceSyncStatus::NeedsReview
        );
        drop(state);
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn old_session_cannot_replace_new_registration_sync_status() {
        use notryx_protocol::WorkspaceSyncStatus;
        use notryx_server::config::{Config, ServerConfig, WorkspaceConfig};
        let (directory, server, runtime, old, _document) = setup().await;
        let info = old.server_info().await.unwrap();
        let pair = runtime.0.lock().await.gate.begin_pair("owner").unwrap();
        complete_test_pair(&runtime, pair, old.clone(), info)
            .await
            .unwrap();
        let config = Config {
            sync: Default::default(),
            server: ServerConfig {
                listen: server.local_addr(),
                admin_socket: directory.path().join("admin.sock"),
            },
            workspace: WorkspaceConfig {
                notes_root: directory.path().join("notes"),
                state_root: directory.path().join("server-state"),
                uploads: Default::default(),
            },
        };
        let pairing = crate::sync_bootstrap::server_test_support::pairing_request(&config)
            .await
            .0;
        let current = Arc::new(
            Session::pair(
                &format!("http://{}", server.local_addr()),
                pairing.workspace_id,
                &pairing.pairing_secret,
            )
            .await
            .unwrap(),
        );
        let info = current.server_info().await.unwrap();
        let pair = runtime.0.lock().await.gate.begin_pair("owner").unwrap();
        complete_test_pair(&runtime, pair, current.clone(), info)
            .await
            .unwrap();
        set_sync_status(&runtime, &old, WorkspaceSyncStatus::NeedsReview, &None).await;
        let current_state = sync_state_for_renderer(&runtime, "owner").await.unwrap();
        assert_eq!(current_state.status, WorkspaceSyncStatus::Connecting);
        assert_eq!(
            current_state.binding,
            crate::to_contract(registration_binding(current.binding()))
        );
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn old_sse_error_cannot_block_new_generation() {
        let (_directory, server, runtime, session, _document) = setup().await;
        let binding = registration_binding(session.binding());
        let stream_generation = runtime
            .0
            .lock()
            .await
            .transmission_gate
            .response_generation(&binding);
        {
            let mut state = runtime.0.lock().await;
            state
                .transmission_gate
                .block(&binding, crate::send::TransmissionBlock::EpochMismatch);
            assert!(
                state
                    .transmission_gate
                    .clear(&binding, crate::send::TransmissionBlock::EpochMismatch)
            );
        }
        handle_remote_error(
            &runtime,
            &session,
            notryx_client_runtime::Error::Contract,
            &None,
            Some(stream_generation),
        )
        .await;
        assert!(
            !runtime
                .0
                .lock()
                .await
                .transmission_gate
                .has_block(&binding, crate::send::TransmissionBlock::ProtocolMismatch)
        );
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn old_sse_reconnect_error_cannot_block_new_generation() {
        let (_directory, server, runtime, session, _document) = setup().await;
        let binding = registration_binding(session.binding());
        let request_generation = runtime
            .0
            .lock()
            .await
            .transmission_gate
            .response_generation(&binding);
        {
            let mut state = runtime.0.lock().await;
            state
                .transmission_gate
                .block(&binding, crate::send::TransmissionBlock::EpochMismatch);
            assert!(
                state
                    .transmission_gate
                    .clear(&binding, crate::send::TransmissionBlock::EpochMismatch)
            );
        }
        handle_remote_error(
            &runtime,
            &session,
            notryx_client_runtime::Error::Unauthorized,
            &None,
            Some(request_generation),
        )
        .await;
        assert!(
            !runtime
                .0
                .lock()
                .await
                .transmission_gate
                .has_block(&binding, crate::send::TransmissionBlock::AuthRequired)
        );
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn old_catch_up_auth_error_cannot_block_new_generation() {
        let (_directory, server, runtime, session, _document) = setup().await;
        let binding = registration_binding(session.binding());
        let issued_generation = runtime
            .0
            .lock()
            .await
            .transmission_gate
            .response_generation(&binding);
        let error = crate::sync_catchup::CatchUpError::Network(
            notryx_client_runtime::Error::Unauthorized,
            issued_generation,
        );
        {
            let mut state = runtime.0.lock().await;
            state
                .transmission_gate
                .block(&binding, crate::send::TransmissionBlock::ProtocolMismatch);
            assert!(
                state
                    .transmission_gate
                    .clear(&binding, crate::send::TransmissionBlock::ProtocolMismatch)
            );
        }
        handle_catch_up_error(&runtime, &session, error, &None).await;
        assert!(
            !runtime
                .0
                .lock()
                .await
                .transmission_gate
                .has_block(&binding, crate::send::TransmissionBlock::AuthRequired)
        );
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn old_bootstrap_contract_error_cannot_block_new_generation() {
        let (_directory, server, runtime, session, _document) = setup().await;
        let binding = registration_binding(session.binding());
        let issued_generation = runtime
            .0
            .lock()
            .await
            .transmission_gate
            .response_generation(&binding);
        let error = InitialSyncError::Bootstrap(crate::sync_bootstrap::BootstrapError::Remote(
            notryx_client_runtime::Error::Contract,
            issued_generation,
        ));
        {
            let mut state = runtime.0.lock().await;
            state
                .transmission_gate
                .block(&binding, crate::send::TransmissionBlock::EpochMismatch);
            assert!(
                state
                    .transmission_gate
                    .clear(&binding, crate::send::TransmissionBlock::EpochMismatch)
            );
        }
        handle_initial_error(&runtime, &session, &error, &None).await;
        assert!(
            !runtime
                .0
                .lock()
                .await
                .transmission_gate
                .has_block(&binding, crate::send::TransmissionBlock::ProtocolMismatch)
        );
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn invalidated_initial_token_cannot_publish_before_first_request() {
        let (_directory, server, runtime, session, _document) = setup().await;
        let binding = registration_binding(session.binding());
        let token = {
            let mut state = runtime.0.lock().await;
            let token = state
                .transmission_gate
                .begin_initial_sync(&binding)
                .unwrap();
            state.initial_sync = Some(token.clone());
            state
                .transmission_gate
                .block(&binding, crate::send::TransmissionBlock::ProtocolMismatch);
            assert!(
                state
                    .transmission_gate
                    .clear(&binding, crate::send::TransmissionBlock::ProtocolMismatch)
            );
            token
        };
        assert!(matches!(
            run_initial_sync(&runtime, &session, &token).await,
            Err(InitialSyncError::Superseded)
        ));
        assert!(
            runtime
                .0
                .lock()
                .await
                .store
                .sync_index(&binding)
                .await
                .unwrap()
                .is_none()
        );
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn verified_initial_sync_releases_only_its_own_send_block() {
        let (_directory, server, runtime, session, _document) = setup().await;
        let info = session.server_info().await.unwrap();
        let pair = runtime.0.lock().await.gate.begin_pair("owner").unwrap();
        complete_test_pair(&runtime, pair, session.clone(), info)
            .await
            .unwrap();
        let token = runtime.0.lock().await.initial_sync.clone().unwrap();
        let (_, events, _) = run_initial_sync(&runtime, &session, &token).await.unwrap();
        let result = crate::send::send_one(runtime.0.clone(), "owner")
            .await
            .unwrap();
        assert_eq!(
            result.status,
            notryx_protocol::WorkspaceSendStatus::Conflict
        );
        drop(events);
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn background_task_initializes_before_releasing_transmission() {
        let (_directory, server, runtime, session, _document) = setup().await;
        let info = session.server_info().await.unwrap();
        let pair = runtime.0.lock().await.gate.begin_pair("owner").unwrap();
        complete_test_pair(&runtime, pair, session.clone(), info)
            .await
            .unwrap();
        start_background_sync(&runtime, session.clone(), None).await;
        tokio::time::timeout(std::time::Duration::from_secs(30), async {
            loop {
                if runtime.0.lock().await.initial_sync.is_none() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        })
        .await
        .unwrap();
        let result = crate::send::send_one(runtime.0.clone(), "owner")
            .await
            .unwrap();
        assert_eq!(
            result.status,
            notryx_protocol::WorkspaceSendStatus::Conflict
        );
        if let Some(task) = runtime.0.lock().await.sync_task.take() {
            task.abort();
        }
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn product_pair_path_starts_sync_and_preserves_previous_registration_draft() {
        use notryx_server::config::{Config, ServerConfig, WorkspaceConfig};
        let (directory, server, runtime, previous, document) = setup().await;
        let previous_binding = registration_binding(previous.binding());
        let config = Config {
            sync: Default::default(),
            server: ServerConfig {
                listen: server.local_addr(),
                admin_socket: directory.path().join("admin.sock"),
            },
            workspace: WorkspaceConfig {
                notes_root: directory.path().join("notes"),
                state_root: directory.path().join("server-state"),
                uploads: Default::default(),
            },
        };
        let pairing = crate::sync_bootstrap::server_test_support::pairing_request(&config)
            .await
            .0;
        let session = Arc::new(
            Session::pair(
                &format!("http://{}", server.local_addr()),
                pairing.workspace_id,
                &pairing.pairing_secret,
            )
            .await
            .unwrap(),
        );
        let info = session.server_info().await.unwrap();
        let pair = runtime.0.lock().await.gate.begin_pair("owner").unwrap();
        crate::credential_native::with_test_vault(crate::complete_pair_and_start(
            &runtime,
            pair,
            session.clone(),
            Ok(info),
            true,
            None,
        ))
        .await
        .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(30), async {
            loop {
                if sync_state_for_renderer(&runtime, "owner")
                    .await
                    .unwrap()
                    .status
                    == notryx_protocol::WorkspaceSyncStatus::Ready
                {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        })
        .await
        .unwrap();
        let result = crate::send::send_one(runtime.0.clone(), "owner")
            .await
            .unwrap();
        assert_eq!(result.status, notryx_protocol::WorkspaceSendStatus::Idle);
        let mut state = runtime.0.lock().await;
        let draft = state
            .store
            .read_registered_checkpoint(&previous_binding, &document)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(draft.bytes, b"local draft");
        assert_eq!(draft.base_bytes, b"base");
        let entries = state
            .store
            .list_outbox(&previous_binding, 0, 100)
            .await
            .unwrap()
            .entries;
        assert_eq!(entries.len(), 1);
        assert!(entries[0].frozen);
        if let Some(task) = state.sync_task.take() {
            task.abort();
        }
        drop(state);
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn background_catchup_after_renderer_reload_keeps_draft_and_applies_server_changes() {
        use notryx_client_runtime::ContentMutation;
        use notryx_server::config::{Config, ServerConfig, WorkspaceConfig};

        let (directory, server, runtime, session, document) = setup().await;
        let (previous, events) = bootstrap_background(&runtime, &session).await.unwrap();
        let node = session
            .inventory(None)
            .await
            .unwrap()
            .nodes
            .into_iter()
            .find(|node| node.node_id.to_string() == document)
            .unwrap();
        let config = Config {
            sync: Default::default(),
            server: ServerConfig {
                listen: server.local_addr(),
                admin_socket: directory.path().join("admin.sock"),
            },
            workspace: WorkspaceConfig {
                notes_root: directory.path().join("notes"),
                state_root: directory.path().join("server-state"),
                uploads: Default::default(),
            },
        };
        let pairing = crate::sync_bootstrap::server_test_support::pairing_request(&config)
            .await
            .0;
        let writer = Session::pair(
            &format!("http://{}", server.local_addr()),
            pairing.workspace_id,
            &pairing.pairing_secret,
        )
        .await
        .unwrap();
        writer
            .put_content(&ContentMutation {
                binding: writer.binding().clone(),
                operation_id: Uuid::now_v7().to_string().parse().unwrap(),
                operation_seq: 1,
                node_id: node.node_id,
                expected_revision: node.content_rev.clone().unwrap(),
                bytes: b"remote change\n".to_vec(),
            })
            .await
            .unwrap();
        runtime
            .0
            .lock()
            .await
            .gate
            .replace_owner("new-owner".into());
        let cursor = catch_up_background(&runtime, &session).await.unwrap();
        assert!(cursor > previous);
        let binding = registration_binding(session.binding());
        let mut state = runtime.0.lock().await;
        let index = state.store.sync_index(&binding).await.unwrap().unwrap();
        assert_eq!(index.0, cursor);
        let changed = index
            .1
            .iter()
            .find(|item| item.node_id == node.node_id)
            .unwrap();
        assert_ne!(changed.content_rev, node.content_rev);
        let draft = state
            .store
            .read_registered_checkpoint(&binding, &document)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(draft.bytes, b"local draft");
        assert_eq!(draft.base_bytes, b"base");
        assert_eq!(
            state
                .store
                .list_outbox(&binding, 0, 100)
                .await
                .unwrap()
                .entries
                .len(),
            1
        );
        drop(state);
        drop(events);
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn registered_session_cannot_send_before_initial_sync() {
        let (_directory, server, runtime, session, document) = setup().await;
        let binding = registration_binding(session.binding());
        let info = session.server_info().await.unwrap();
        let before = session
            .inventory(None)
            .await
            .unwrap()
            .nodes
            .into_iter()
            .find(|node| node.node_id.to_string() == document)
            .unwrap();
        let pair = runtime.0.lock().await.gate.begin_pair("owner").unwrap();
        complete_test_pair(&runtime, pair, session.clone(), info)
            .await
            .unwrap();
        let result = crate::send::send_one(runtime.0.clone(), "owner")
            .await
            .unwrap();
        assert_eq!(
            result.status,
            notryx_protocol::WorkspaceSendStatus::SyncPending
        );
        let mut state = runtime.0.lock().await;
        assert_eq!(
            state
                .store
                .list_outbox(&binding, 0, 100)
                .await
                .unwrap()
                .entries
                .len(),
            1
        );
        drop(state);
        let after = session
            .inventory(None)
            .await
            .unwrap()
            .nodes
            .into_iter()
            .find(|node| node.node_id.to_string() == document)
            .unwrap();
        assert_eq!(before.content_rev, after.content_rev);
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn background_bootstrap_keeps_an_authenticated_event_stream_after_renderer_reload() {
        let (_directory, server, runtime, session, _document) = setup().await;
        runtime
            .0
            .lock()
            .await
            .gate
            .replace_owner("new-owner".into());
        let (cursor, mut events) = bootstrap_background(&runtime, &session).await.unwrap();
        let initial = events.next_head().await.unwrap().unwrap();
        assert_eq!(initial.workspace_epoch, session.binding().workspace_epoch);
        assert!(initial.head_seq <= cursor);
        assert!(session.events().await.is_err());
        let binding = registration_binding(session.binding());
        let mut state = runtime.0.lock().await;
        let index = state.store.sync_index(&binding).await.unwrap().unwrap();
        assert_eq!(index.0, cursor);
        drop(state);
        drop(events);
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn background_bootstrap_does_not_publish_before_event_stream_connects() {
        let (_directory, server, runtime, session, _document) = setup().await;
        let existing = session.events().await.unwrap();
        assert!(bootstrap_background(&runtime, &session).await.is_err());
        let binding = registration_binding(session.binding());
        assert!(
            runtime
                .0
                .lock()
                .await
                .store
                .sync_index(&binding)
                .await
                .unwrap()
                .is_none()
        );
        drop(existing);
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn background_sync_remains_bound_to_registration_after_renderer_reload() {
        let (_directory, server, runtime, session, _document) = setup().await;
        runtime
            .0
            .lock()
            .await
            .gate
            .replace_owner("new-owner".into());
        assert!(checked_registration(&runtime, &session).await.is_ok());
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn background_sync_rejects_replaced_or_removed_session_and_registration() {
        let (_directory, server, runtime, session, _document) = setup().await;
        runtime.0.lock().await.session = None;
        assert!(
            matches!(checked_registration(&runtime, &session).await, Err(error) if error == "stale_registration")
        );
        {
            let mut state = runtime.0.lock().await;
            state.session = Some(session.clone());
            let mut replacement = registration_binding(session.binding());
            replacement.device_id = Uuid::new_v4().to_string();
            replacement.device_stream_id = Uuid::new_v4().to_string();
            state
                .store
                .activate_registration(&replacement)
                .await
                .unwrap();
        }
        assert!(
            matches!(checked_registration(&runtime, &session).await, Err(error) if error == "stale_registration")
        );
        server.shutdown().await.unwrap();
    }
}
