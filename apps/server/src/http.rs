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
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::{oneshot, watch};
use tokio::task::JoinSet;

const HTTP_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(30);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_CONNECTIONS: usize = 128;

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

fn api_error(status: StatusCode, code: &str) -> Response {
    let message = match code {
        "unauthorized" => "기기 인증을 확인해 주세요.",
        "forbidden" => "이 요청을 수행할 권한이 없습니다.",
        "not_found" => "문서를 찾을 수 없습니다.",
        "content_revision_mismatch" => {
            "문서 본문이 변경되었습니다. 현재 버전을 다시 확인해 주세요."
        }
        "unsupported_protocol" => "서버와 클라이언트의 프로토콜이 다릅니다.",
        "request_timeout" => "요청 제한 시간을 초과했습니다.",
        "storage_unavailable" => "서버가 요청을 처리할 수 없습니다. 잠시 후 다시 시도해 주세요.",
        "recovery_pending" => "저장소 복구를 마친 뒤 다시 시도해 주세요.",
        _ => "요청 형식과 크기를 확인해 주세요.",
    };
    let mut response = (
        status,
        Json(ApiError {
            code: code.into(),
            message: message.into(),
            request_id: uuid::Uuid::now_v7().to_string(),
        }),
    )
        .into_response();
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
        RuntimeError::Auth(AuthError::NotFound)
        | RuntimeError::Workspace(crate::Error::NotFound) => StatusCode::NOT_FOUND,
        RuntimeError::Workspace(crate::Error::InvalidRequest) => StatusCode::BAD_REQUEST,
        RuntimeError::PreconditionFailed => StatusCode::PRECONDITION_FAILED,
        RuntimeError::Workspace(crate::Error::RecoveryRequired) => {
            return api_error(StatusCode::SERVICE_UNAVAILABLE, "recovery_pending");
        }
        _ => return api_error(StatusCode::SERVICE_UNAVAILABLE, "storage_unavailable"),
    };
    api_error(status, error.code())
}

async fn pair(
    State(handle): State<WorkspaceHandle>,
    body: Result<Json<PairRequest>, JsonRejection>,
) -> Response {
    let request = match body {
        Ok(Json(request)) => request,
        Err(error) => {
            let status = if error.status() == StatusCode::PAYLOAD_TOO_LARGE {
                StatusCode::PAYLOAD_TOO_LARGE
            } else {
                StatusCode::BAD_REQUEST
            };
            return api_error(status, "invalid_request");
        }
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
    let mut response = match tokio::time::timeout(REQUEST_TIMEOUT, next.run(request)).await {
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
            .route("/v1/nodes/{id}/metadata", get(metadata))
            .route("/v1/nodes/{id}/content", get(content))
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
