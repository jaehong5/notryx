use crate::admin::{AdminError, AdminServer};
use crate::auth::AuthError;
use crate::config::Config;
use crate::runtime::{RuntimeError, WorkspaceHandle, WorkspaceRuntime};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path, Request, State, rejection::JsonRejection},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use hyper::server::conn::http1;
use hyper_util::{
    rt::{TokioIo, TokioTimer},
    service::TowerToHyperService,
};
use notryx_domain::NodeId;
use notryx_protocol::{ApiError, ContentPrecondition, PairRequest, StrongEtag, require_contract};
use sha2::{Digest, Sha256};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::{Semaphore, oneshot, watch};
use tokio::task::JoinSet;

mod create;
#[cfg(test)]
mod permit_tests;
mod read;
mod stream;
mod structure;
mod upload;
mod write;
mod write_timeout;

const HTTP_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(30);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const WRITE_STALL_TIMEOUT: Duration = Duration::from_secs(30);
// Sixteen buffered 25 MiB downloads leave room for writes within the 1 GiB container limit.
const MAX_CONNECTIONS: usize = 16;
const MAX_CONTENT_WRITES: usize = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerError {
    Runtime(RuntimeError),
    Admin(AdminError),
    Listen,
    Task,
    ShutdownTimeout,
}

impl std::fmt::Display for ServerError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for ServerError {}

pub struct RunningServer {
    address: SocketAddr,
    runtime: Option<WorkspaceRuntime>,
    admin: Option<AdminServer>,
    stop: Option<oneshot::Sender<()>>,
    task: Option<tokio::task::JoinHandle<Result<(), ServerError>>>,
}

async fn live() -> &'static str {
    "live\n"
}

async fn ready(State(handle): State<WorkspaceHandle>) -> (StatusCode, &'static str) {
    match handle.health().await {
        Ok(health) if health.ready => (StatusCode::OK, "ready\n"),
        _ => (StatusCode::SERVICE_UNAVAILABLE, "not_ready\n"),
    }
}

fn error_body(code: &str) -> ApiError {
    let message = match code {
        "unauthorized" => "기기 인증을 확인해 주세요.",
        "forbidden" => "이 요청을 수행할 권한이 없습니다.",
        "not_found" => "문서를 찾을 수 없습니다.",
        "content_revision_mismatch" => {
            "문서 본문이 변경되었습니다. 현재 버전을 다시 확인해 주세요."
        }
        "metadata_revision_mismatch" => {
            "문서의 이름, 위치 또는 삭제 상태가 변경되었습니다. 현재 상태를 다시 확인해 주세요."
        }
        "node_revision_mismatch" => "문서 상태가 변경되었습니다. 현재 상태를 다시 확인해 주세요.",
        "node_deleted" => "삭제된 문서입니다.",
        "not_deleted" => "삭제되지 않은 문서는 복원할 수 없습니다.",
        "folder_not_empty" => "비어 있지 않은 폴더는 삭제할 수 없습니다.",
        "history_expired" => "보관된 원문이 없어 문서를 복원할 수 없습니다.",
        "expected_parent_mismatch" => {
            "문서의 기존 위치가 요청과 다릅니다. 현재 위치를 확인해 주세요."
        }
        "cycle_detected" => "폴더를 자기 자신이나 하위 폴더 안으로 이동할 수 없습니다.",
        "root_immutable" => "작업 공간의 루트 폴더는 변경할 수 없습니다.",
        "unsupported_protocol" => "서버와 클라이언트의 프로토콜이 다릅니다.",
        "request_timeout" => "요청 제한 시간을 초과했습니다.",
        "storage_unavailable" => "서버가 요청을 처리할 수 없습니다. 잠시 후 다시 시도해 주세요.",
        "recovery_pending" => "저장소 복구를 마친 뒤 다시 시도해 주세요.",
        "recovery_conflict" => {
            "복구 중 파일 상태가 달라 기존 작업을 종료했습니다. 현재 문서와 초안을 대조한 뒤 새 작업으로 저장해 주세요."
        }
        "epoch_mismatch" => "저장소의 연결 정보가 요청과 다릅니다. 연결 상태를 다시 확인해 주세요.",
        "operation_gap" => "앞선 작업의 결과를 먼저 확인해 주세요.",
        "idempotency_mismatch" => "이 작업 식별자는 다른 요청에 사용되었습니다.",
        "receipt_limit" => "확인하지 않은 작업 결과가 한도에 도달했습니다.",
        "external_change" => "파일의 외부 변경을 확인한 뒤 다시 시도해 주세요.",
        "operation_not_applied" => "이 작업은 적용되지 않았습니다.",
        "digest_mismatch" => "본문과 검증값이 일치하지 않습니다.",
        "destination_exists" => "같은 이름의 파일이나 폴더가 이미 있습니다.",
        "node_id_conflict" => "이미 사용한 문서 식별자입니다.",
        "parent_not_folder" => "대상 위치가 폴더가 아닙니다.",
        "invalid_ack" => "아직 접수되지 않은 작업은 확인 처리할 수 없습니다.",
        "operation_pending" => "완료되지 않은 작업은 확인 처리하거나 취소할 수 없습니다.",
        "upload_quota" => "임시 업로드 보관 한도를 초과했습니다.",
        "insufficient_storage" => "서버 저장 공간이 부족합니다.",
        "upload_unavailable" => "업로드가 만료되었거나 이미 사용되었습니다.",
        "upload_too_large" => "첨부 파일 크기 한도를 초과했습니다.",
        "busy" => "처리 중인 요청이 많습니다. 잠시 뒤 다시 시도해 주세요.",
        "operation_retired" => {
            "이미 확인 처리한 작업입니다. 다시 실행하지 말고 현재 상태를 확인해 주세요."
        }
        _ => "요청 형식과 크기를 확인해 주세요.",
    };
    ApiError {
        code: code.into(),
        message: message.into(),
        request_id: uuid::Uuid::now_v7().to_string(),
    }
}

fn api_error(status: StatusCode, code: &str) -> Response {
    let mut response = (status, Json(error_body(code))).into_response();
    if status == StatusCode::UNAUTHORIZED {
        response.headers_mut().insert(
            header::WWW_AUTHENTICATE,
            HeaderValue::from_static("Bearer realm=\"notryx\""),
        );
    }
    response
}

fn runtime_error(error: RuntimeError) -> Response {
    let status = match error {
        RuntimeError::Auth(AuthError::Unauthorized) => StatusCode::UNAUTHORIZED,
        RuntimeError::Auth(AuthError::InvalidRequest) => StatusCode::BAD_REQUEST,
        RuntimeError::Auth(AuthError::Forbidden) => StatusCode::FORBIDDEN,
        RuntimeError::Auth(AuthError::NoSpace) => StatusCode::INSUFFICIENT_STORAGE,
        RuntimeError::Auth(AuthError::NotFound)
        | RuntimeError::Workspace(crate::Error::NotFound) => StatusCode::NOT_FOUND,
        RuntimeError::Workspace(crate::Error::InvalidRequest | crate::Error::DigestMismatch) => {
            StatusCode::BAD_REQUEST
        }
        RuntimeError::Workspace(
            crate::Error::WorkspaceMismatch
            | crate::Error::UnknownStream
            | crate::Error::OperationGap
            | crate::Error::IdempotencyMismatch
            | crate::Error::InvalidAck
            | crate::Error::OperationPending
            | crate::Error::ExternalChange,
        ) => StatusCode::CONFLICT,
        RuntimeError::Workspace(crate::Error::OperationRetired | crate::Error::NodeDeleted) => {
            StatusCode::GONE
        }
        RuntimeError::Workspace(crate::Error::ReceiptLimit) => StatusCode::TOO_MANY_REQUESTS,
        RuntimeError::Busy => StatusCode::TOO_MANY_REQUESTS,
        RuntimeError::Workspace(crate::Error::NoSpace | crate::Error::UploadQuota) => {
            StatusCode::INSUFFICIENT_STORAGE
        }
        RuntimeError::Workspace(crate::Error::UploadTooLarge) => StatusCode::PAYLOAD_TOO_LARGE,
        RuntimeError::Workspace(crate::Error::UploadUnavailable) => StatusCode::GONE,
        RuntimeError::PreconditionFailed => StatusCode::PRECONDITION_FAILED,
        RuntimeError::Workspace(crate::Error::RecoveryRequired) => {
            return api_error(StatusCode::SERVICE_UNAVAILABLE, "recovery_pending");
        }
        _ => return api_error(StatusCode::SERVICE_UNAVAILABLE, "storage_unavailable"),
    };
    api_error(status, error.code())
}

fn invalid_json(error: JsonRejection) -> Response {
    let status = if error.status() == StatusCode::PAYLOAD_TOO_LARGE {
        StatusCode::PAYLOAD_TOO_LARGE
    } else {
        StatusCode::BAD_REQUEST
    };
    api_error(status, "invalid_request")
}

async fn pair(
    State(handle): State<WorkspaceHandle>,
    body: Result<Json<PairRequest>, JsonRejection>,
) -> Response {
    let request = match body {
        Ok(Json(request)) => request,
        Err(error) => return invalid_json(error),
    };
    if require_contract(&request.protocol).is_err() {
        return api_error(StatusCode::CONFLICT, "unsupported_protocol");
    }
    match handle.pair(request).await {
        Ok(response) => Json(response).into_response(),
        Err(error) => runtime_error(error),
    }
}

fn bearer(headers: &HeaderMap) -> Result<String, RuntimeError> {
    let unauthorized = RuntimeError::Auth(AuthError::Unauthorized);
    let mut values = headers.get_all(header::AUTHORIZATION).iter();
    let value = values.next().ok_or(unauthorized)?;
    if values.next().is_some() {
        return Err(unauthorized);
    }
    let (scheme, token) = value
        .to_str()
        .map_err(|_| unauthorized)?
        .split_once(' ')
        .ok_or(unauthorized)?;
    let token = token.trim_start_matches(' ');
    if !scheme.eq_ignore_ascii_case("bearer")
        || token.is_empty()
        || token.bytes().any(|byte| byte.is_ascii_whitespace())
    {
        return Err(unauthorized);
    }
    Ok(token.into())
}

fn node_id(value: &str) -> Result<NodeId, RuntimeError> {
    value
        .parse()
        .map_err(|_| RuntimeError::Auth(AuthError::InvalidRequest))
}

fn content_precondition(headers: &HeaderMap) -> Result<Option<ContentPrecondition>, RuntimeError> {
    let invalid = RuntimeError::Auth(AuthError::InvalidRequest);
    let mut values = headers.get_all(header::IF_MATCH).iter();
    let Some(value) = values.next() else {
        return Ok(None);
    };
    if values.next().is_some() {
        return Err(invalid);
    }
    ContentPrecondition::parse(value.to_str().map_err(|_| invalid)?)
        .map(Some)
        .map_err(|_| invalid)
}

async fn server_info(State(handle): State<WorkspaceHandle>, headers: HeaderMap) -> Response {
    let token = match bearer(&headers) {
        Ok(token) => token,
        Err(error) => return runtime_error(error),
    };
    match handle.server_info(token).await {
        Ok(info) => Json(info).into_response(),
        Err(error) => runtime_error(error),
    }
}

async fn metadata(
    State(handle): State<WorkspaceHandle>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let token = match bearer(&headers) {
        Ok(token) => token,
        Err(error) => return runtime_error(error),
    };
    let id = match node_id(&id) {
        Ok(id) => id,
        Err(error) => return runtime_error(error),
    };
    match handle.metadata(token, id).await {
        Ok(metadata) => {
            let etag = match HeaderValue::from_str(&metadata.meta_etag) {
                Ok(etag) => etag,
                Err(_) => return api_error(StatusCode::SERVICE_UNAVAILABLE, "storage_unavailable"),
            };
            let mut response = Json(metadata).into_response();
            response.headers_mut().insert(header::ETAG, etag);
            response
        }
        Err(error) => runtime_error(error),
    }
}

async fn content(
    State(handle): State<WorkspaceHandle>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let token = match bearer(&headers) {
        Ok(token) => token,
        Err(error) => return runtime_error(error),
    };
    let id = match node_id(&id) {
        Ok(id) => id,
        Err(error) => return runtime_error(error),
    };
    let precondition = match content_precondition(&headers) {
        Ok(value) => value,
        Err(error) => return runtime_error(error),
    };
    match handle.content(token, id, precondition).await {
        Ok((document, bytes)) => {
            let etag = StrongEtag::from_content_revision(&document.content_rev)
                .ok()
                .and_then(|etag| HeaderValue::from_str(etag.as_str()).ok());
            let Some(etag) = etag else {
                return api_error(StatusCode::SERVICE_UNAVAILABLE, "storage_unavailable");
            };
            let digest = format!("sha-256=:{}:", STANDARD.encode(Sha256::digest(&bytes)));
            let mut response = bytes.into_response();
            response.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/octet-stream"),
            );
            response.headers_mut().insert(header::ETAG, etag);
            response
                .headers_mut()
                .insert("content-digest", HeaderValue::from_str(&digest).unwrap());
            response
        }
        Err(error) => runtime_error(error),
    }
}

async fn bounded_request(request: Request, next: Next) -> Response {
    let api_request = request.uri().path().starts_with("/v1/");
    let deadline =
        if request.method() == axum::http::Method::POST && request.uri().path() == "/v1/uploads" {
            upload::TIMEOUT
        } else {
            REQUEST_TIMEOUT
        };
    let mut response = match tokio::time::timeout(deadline, next.run(request)).await {
        Ok(response) => response,
        Err(_) => api_error(StatusCode::REQUEST_TIMEOUT, "request_timeout"),
    };
    if api_request
        && (response.status().is_client_error() || response.status().is_server_error())
        && !response
            .headers()
            .get(header::CONTENT_TYPE)
            .is_some_and(|value| value.as_bytes().starts_with(b"application/json"))
    {
        let code = if response.status() == StatusCode::NOT_FOUND {
            "not_found"
        } else {
            "invalid_request"
        };
        let allow = response.headers_mut().remove(header::ALLOW);
        response = api_error(response.status(), code);
        if let Some(allow) = allow {
            response.headers_mut().insert(header::ALLOW, allow);
        }
    }
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    response
        .headers_mut()
        .insert(header::X_CONTENT_TYPE_OPTIONS, "nosniff".parse().unwrap());
    response
}

async fn bounded_content_write(
    State((handle, permits)): State<(WorkspaceHandle, Arc<Semaphore>)>,
    mut request: Request,
    next: Next,
) -> Response {
    if request.method() != axum::http::Method::PUT {
        return next.run(request).await;
    }
    let token = match bearer(request.headers()) {
        Ok(token) => token,
        Err(error) => return runtime_error(error),
    };
    // Check identity before reserving a large body; admission still rechecks current permissions.
    if let Err(error) = handle.authenticate(token).await {
        return runtime_error(error);
    }
    // The coordinator retains this permit if HTTP cancellation leaves a queued 25 MiB body.
    let Ok(permit) = permits.try_acquire_owned() else {
        return api_error(StatusCode::TOO_MANY_REQUESTS, "busy");
    };
    let _permit = Arc::new(permit);
    request.extensions_mut().insert(Arc::clone(&_permit));
    next.run(request).await
}

async fn serve(
    listener: TcpListener,
    routes: Router,
    mut stopped: oneshot::Receiver<()>,
) -> Result<(), ServerError> {
    let mut connections = JoinSet::new();
    let (closing, _) = watch::channel(false);
    let mut task_failed = false;
    loop {
        tokio::select! {
            biased;
            _ = &mut stopped => break,
            result = connections.join_next(), if !connections.is_empty() => {
                task_failed |= result.is_some_and(|result| result.is_err());
            }
            accepted = listener.accept(), if connections.len() < MAX_CONNECTIONS => {
                let (stream, _) = match accepted {
                    Ok(accepted) => accepted,
                    Err(_) => {
                        tokio::select! {
                            _ = &mut stopped => break,
                            _ = tokio::time::sleep(Duration::from_secs(1)) => continue,
                        }
                    }
                };
                let service = TowerToHyperService::new(routes.clone());
                let mut closed = closing.subscribe();
                connections.spawn(async move {
                    let mut http = http1::Builder::new();
                    http.timer(TokioTimer::new()).header_read_timeout(HEADER_READ_TIMEOUT);
                    // Hyper may finish polling the body before its buffered bytes reach the socket.
                    let stream = write_timeout::WriteTimeout::new(stream, WRITE_STALL_TIMEOUT);
                    let connection = http.serve_connection(TokioIo::new(stream), service);
                    tokio::pin!(connection);
                    tokio::select! {
                        biased;
                        _ = closed.changed() => {
                            connection.as_mut().graceful_shutdown();
                            let _ = connection.await;
                        }
                        _ = &mut connection => {}
                    }
                });
            }
        }
    }
    drop(listener);
    let _ = closing.send(true);
    let drained = tokio::time::timeout(HTTP_SHUTDOWN_TIMEOUT, async {
        while let Some(result) = connections.join_next().await {
            task_failed |= result.is_err();
        }
    })
    .await;
    if drained.is_err() {
        // Aborting only the accept loop would leave its accepted sockets alive.
        connections.abort_all();
        while connections.join_next().await.is_some() {}
        return Err(ServerError::ShutdownTimeout);
    }
    if task_failed {
        Err(ServerError::Task)
    } else {
        Ok(())
    }
}

impl RunningServer {
    pub async fn start(config: Config) -> Result<Self, ServerError> {
        let runtime = WorkspaceRuntime::start(config.workspace)
            .await
            .map_err(ServerError::Runtime)?;
        let listener = match TcpListener::bind(config.server.listen).await {
            Ok(listener) => listener,
            Err(_) => {
                runtime.shutdown().await.map_err(ServerError::Runtime)?;
                return Err(ServerError::Listen);
            }
        };
        let address = match listener.local_addr() {
            Ok(address) => address,
            Err(_) => {
                runtime.shutdown().await.map_err(ServerError::Runtime)?;
                return Err(ServerError::Listen);
            }
        };
        let admin = match AdminServer::start(&config.server.admin_socket, runtime.handle()).await {
            Ok(admin) => admin,
            Err(error) => {
                runtime.shutdown().await.map_err(ServerError::Runtime)?;
                return Err(ServerError::Admin(error));
            }
        };
        let routes = Router::new()
            .route("/health/live", get(live))
            .route("/health/ready", get(ready))
            .route("/v1/pair", post(pair))
            .route("/v1/server-info", get(server_info))
            .route("/v1/uploads", post(upload::upload))
            .route("/v1/nodes", get(read::inventory).post(create::create_node))
            .route(
                "/v1/nodes/{id}",
                get(read::node).delete(structure::delete_node),
            )
            .route("/v1/nodes/{id}/restore", post(structure::restore_node))
            .route("/v1/trash", get(read::trash))
            .route(
                "/v1/nodes/{id}/metadata",
                get(metadata).patch(structure::move_node),
            )
            .route(
                "/v1/nodes/{id}/content",
                get(content)
                    .put(write::put_content)
                    .layer(DefaultBodyLimit::max(notryx_server_storage::MAX_FILE_BYTES))
                    .layer(middleware::from_fn_with_state(
                        (
                            runtime.handle(),
                            Arc::new(Semaphore::new(MAX_CONTENT_WRITES)),
                        ),
                        bounded_content_write,
                    )),
            )
            .route("/v1/operations/{id}", get(write::operation))
            .route("/v1/operation-stream/ack", post(stream::ack))
            .route("/v1/operation-stream/abandon", post(stream::abandon))
            .layer(DefaultBodyLimit::max(1024 * 1024))
            .layer(middleware::from_fn(bounded_request))
            .with_state(runtime.handle());
        let (stop, stopped) = oneshot::channel();
        let task = tokio::spawn(serve(listener, routes, stopped));
        Ok(Self {
            address,
            runtime: Some(runtime),
            admin: Some(admin),
            stop: Some(stop),
            task: Some(task),
        })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.address
    }

    pub async fn shutdown(mut self) -> Result<(), ServerError> {
        if let Some(runtime) = self.runtime.as_mut() {
            runtime.begin_shutdown();
        }
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        let result = if let Some(task) = self.task.as_mut() {
            match task.await {
                Ok(result) => result,
                Err(_) => Err(ServerError::Task),
            }
        } else {
            Err(ServerError::Task)
        };
        self.task.take();
        let admin_result = if let Some(admin) = self.admin.take() {
            admin.shutdown().await.map_err(ServerError::Admin)
        } else {
            Ok(())
        };
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown().await.map_err(ServerError::Runtime)?;
        }
        result.and(admin_result)
    }
}

impl Drop for RunningServer {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exhausted_filesystem_and_auth_storage_have_distinct_507_code() {
        for error in [
            RuntimeError::Workspace(crate::Error::NoSpace),
            RuntimeError::Auth(AuthError::NoSpace),
        ] {
            assert_eq!(
                runtime_error(error).status(),
                StatusCode::INSUFFICIENT_STORAGE
            );
            let body = error_body(error.code());
            assert_eq!(body.code, "insufficient_storage");
            assert_eq!(body.message, "서버 저장 공간이 부족합니다.");
        }
        assert_eq!(
            runtime_error(RuntimeError::Workspace(crate::Error::UploadQuota)).status(),
            StatusCode::INSUFFICIENT_STORAGE
        );
        assert_eq!(
            RuntimeError::Workspace(crate::Error::UploadQuota).code(),
            "upload_quota"
        );
    }
}
