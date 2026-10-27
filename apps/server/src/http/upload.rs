use super::{api_error, bearer, runtime_error, write::required_header};
use crate::auth::AuthError;
use crate::runtime::{CHUNK_BYTES, RuntimeError, UploadSession, WorkspaceHandle};
use axum::{
    Json,
    body::Body,
    extract::{Request, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use http_body_util::BodyExt;
use std::time::Duration;

pub(super) const TIMEOUT: Duration = Duration::from_secs(120);
const PROGRESS_WINDOW: Duration = Duration::from_secs(5);
const MIN_WINDOW_BYTES: usize = 5 * 1024;

fn parameters(headers: &HeaderMap) -> Result<(String, Option<u64>), RuntimeError> {
    let invalid = RuntimeError::Auth(AuthError::InvalidRequest);
    if headers.contains_key(header::CONTENT_ENCODING)
        && !required_header(headers, "content-encoding")?.eq_ignore_ascii_case("identity")
    {
        return Err(invalid);
    }
    let digest = required_header(headers, "content-digest")?;
    let encoded = digest
        .strip_prefix("sha-256=:")
        .and_then(|value| value.strip_suffix(':'))
        .ok_or(invalid)?;
    let bytes = STANDARD.decode(encoded).map_err(|_| invalid)?;
    if bytes.len() != 32 {
        return Err(invalid);
    }
    let hash = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    let size = if headers.contains_key(header::CONTENT_LENGTH) {
        let value = required_header(headers, "content-length")?;
        if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(invalid);
        }
        Some(value.parse::<u64>().map_err(|_| invalid)?)
    } else {
        None
    };
    if size.is_some_and(|size| size > notryx_server_storage::MAX_FILE_BYTES as u64) {
        return Err(RuntimeError::Workspace(crate::Error::UploadTooLarge));
    }
    Ok((hash, size))
}

pub(super) async fn upload(State(handle): State<WorkspaceHandle>, request: Request) -> Response {
    let token = match bearer(request.headers()) {
        Ok(token) => token,
        Err(error) => return runtime_error(error),
    };
    let (hash, size) = match parameters(request.headers()) {
        Ok(value) => value,
        Err(error) => return runtime_error(error),
    };
    let session = match handle.begin_upload_session(token, hash, size).await {
        Ok(session) => session,
        Err(error) => return runtime_error(error),
    };
    receive(request.into_body(), session).await
}

async fn receive(mut body: Body, session: UploadSession) -> Response {
    let mut cancelled = session.cancelled.clone();
    let mut progress = tokio::time::interval_at(
        tokio::time::Instant::now() + PROGRESS_WINDOW,
        PROGRESS_WINDOW,
    );
    progress.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut received = 0_usize;
    let mut window_bytes = 0_usize;
    loop {
        if *cancelled.borrow() {
            return runtime_error(RuntimeError::Closed);
        }
        let frame = tokio::select! { biased;
            _ = cancelled.changed() => return runtime_error(RuntimeError::Closed),
            _ = progress.tick() => {
                if window_bytes < MIN_WINDOW_BYTES { return api_error(StatusCode::REQUEST_TIMEOUT, "request_timeout"); }
                window_bytes = 0;
                continue;
            }
            frame = body.frame() => frame,
        };
        let data = match frame {
            None => break,
            Some(Err(_)) => return api_error(StatusCode::BAD_REQUEST, "invalid_request"),
            Some(Ok(frame)) => match frame.into_data() {
                Ok(data) => data,
                Err(_) => continue,
            },
        };
        received = match received.checked_add(data.len()) {
            Some(size) if size <= notryx_server_storage::MAX_FILE_BYTES => size,
            _ => return api_error(StatusCode::PAYLOAD_TOO_LARGE, "upload_too_large"),
        };
        window_bytes += data.len();
        for chunk in data.chunks(CHUNK_BYTES) {
            loop {
                tokio::select! { biased;
                    _ = cancelled.changed() => return runtime_error(RuntimeError::Closed),
                    _ = progress.tick() => {
                        if window_bytes < MIN_WINDOW_BYTES { return api_error(StatusCode::REQUEST_TIMEOUT, "request_timeout"); }
                        window_bytes = 0;
                    }
                    sent = session.send(chunk) => {
                        if sent.is_err() {
                            return match session.result().await { Ok(_) => runtime_error(RuntimeError::Worker), Err(error) => runtime_error(error) };
                        }
                        break;
                    }
                }
            }
        }
    }
    match session.finish().await {
        Ok(uploaded) => (StatusCode::CREATED, Json(uploaded)).into_response(),
        Err(error) => runtime_error(error),
    }
}
