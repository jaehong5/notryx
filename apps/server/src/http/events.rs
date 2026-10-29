use super::{bearer, runtime_error};
use crate::runtime::WorkspaceHandle;
use axum::{
    body::Body,
    extract::{RawQuery, State},
    http::{HeaderMap, HeaderValue, header},
    response::{IntoResponse, Response},
};
use futures_util::stream;
use notryx_domain::ChangeSeq;
use notryx_protocol::{HeadHint, ServerInfo};
use std::{convert::Infallible, time::Duration};
use tokio::sync::broadcast;
use tokio::time::{Instant, Interval};

const HEARTBEAT: Duration = Duration::from_secs(15);

struct EventStream {
    handle: WorkspaceHandle,
    token: String,
    notices: broadcast::Receiver<()>,
    heartbeat: Interval,
    last_head: ChangeSeq,
    initial: Option<String>,
    _permit: crate::runtime::events::EventPermit,
}

fn head_event(info: &ServerInfo) -> Result<String, serde_json::Error> {
    let hint = HeadHint {
        workspace_epoch: info.workspace_epoch,
        head_seq: info.head_seq,
    };
    Ok(format!(
        "id: {}\nevent: head\ndata: {}\n\n",
        info.head_seq.get(),
        serde_json::to_string(&hint)?
    ))
}

pub(super) async fn events(
    State(handle): State<WorkspaceHandle>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
) -> Response {
    let token = match bearer(&headers) {
        Ok(token) => token,
        Err(error) => return runtime_error(error),
    };
    if query.as_deref().is_some_and(|value| !value.is_empty()) {
        return super::api_error(axum::http::StatusCode::BAD_REQUEST, "invalid_request");
    }
    // Subscribe before reading head so a commit during connection setup cannot be missed.
    let notices = handle.event_subscription();
    let (identity, info) = match handle.event_info(token.clone()).await {
        Ok(result) => result,
        Err(error) => return runtime_error(error),
    };
    let permit = match handle.event_permit(&identity) {
        Ok(permit) => permit,
        Err(error) => return runtime_error(error),
    };
    let initial = match head_event(&info) {
        Ok(initial) => initial,
        Err(_) => {
            return super::api_error(
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                "storage_unavailable",
            );
        }
    };
    let state = EventStream {
        handle,
        token,
        notices,
        heartbeat: tokio::time::interval_at(Instant::now() + HEARTBEAT, HEARTBEAT),
        last_head: info.head_seq,
        initial: Some(initial),
        _permit: permit,
    };
    let body = Body::from_stream(stream::unfold(state, |mut state| async move {
        if let Some(initial) = state.initial.take() {
            return Some((Ok::<_, Infallible>(initial), state));
        }
        loop {
            let heartbeat = tokio::select! {
                notice = state.notices.recv() => match notice {
                    Ok(()) => false,
                    Err(_) => return None,
                },
                _ = state.heartbeat.tick() => true,
            };
            let info = match state.handle.server_info(state.token.clone()).await {
                Ok(info) => info,
                Err(_) => return None,
            };
            if info.head_seq != state.last_head {
                state.last_head = info.head_seq;
                let event = match head_event(&info) {
                    Ok(event) => event,
                    Err(_) => return None,
                };
                return Some((Ok(event), state));
            }
            if heartbeat {
                return Some((Ok(": heartbeat\n\n".to_string()), state));
            }
        }
    }));
    let mut response = body.into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream"),
    );
    response
        .headers_mut()
        .insert("x-accel-buffering", HeaderValue::from_static("no"));
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Workspace, config::WorkspaceConfig, runtime::WorkspaceRuntime};
    use http_body_util::BodyExt;
    use notryx_protocol::{CONTRACT, PairRequest};

    #[tokio::test]
    async fn lagged_event_receiver_closes_and_releases_device_permit() {
        let dir = tempfile::tempdir().unwrap();
        let config = WorkspaceConfig {
            notes_root: dir.path().join("notes"),
            state_root: dir.path().join("state"),
            uploads: Default::default(),
        };
        std::fs::create_dir(&config.notes_root).unwrap();
        std::fs::create_dir(&config.state_root).unwrap();
        let mut workspace = Workspace::initialize(&config.notes_root, &config.state_root)
            .await
            .unwrap();
        let principal = workspace.auth_status().await.unwrap().owner_principal_id;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let grant = workspace.issue_pairing(principal, now).await.unwrap();
        let device = workspace
            .pair(
                PairRequest {
                    protocol: CONTRACT.into(),
                    workspace_id: grant.workspace_id,
                    pairing_secret: grant.pairing_secret,
                },
                now,
            )
            .await
            .unwrap();
        workspace.close().await.unwrap();
        let runtime = WorkspaceRuntime::start(config).await.unwrap();
        let handle = runtime.handle();
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {}", device.device_token)).unwrap(),
        );
        let response = events(State(handle.clone()), headers.clone(), RawQuery(None)).await;
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        for _ in 0..33 {
            handle.test_notice();
        }
        let mut body = response.into_body();
        let first = body.frame().await.unwrap().unwrap().into_data().unwrap();
        assert!(String::from_utf8_lossy(&first).contains("event: head"));
        assert!(body.frame().await.is_none());
        drop(body);
        let reconnected = events(State(handle), headers, RawQuery(None)).await;
        assert_eq!(reconnected.status(), axum::http::StatusCode::OK);
        drop(reconnected);
        runtime.shutdown().await.unwrap();
    }
}
