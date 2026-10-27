use super::{Command, RuntimeError, WorkspaceHandle};
use notryx_protocol::UploadResponse;
use notryx_server_storage::{SealedUpload, UploadWriter};
use std::sync::{Arc, atomic::Ordering};
use tokio::sync::{Semaphore, mpsc, oneshot, watch};

pub(crate) const CHUNK_BYTES: usize = 64 * 1024;

pub(crate) struct UploadState {
    permits: Arc<Semaphore>,
    capacity: u32,
    stopping: watch::Sender<bool>,
}

impl UploadState {
    pub(super) fn new(capacity: u16) -> Self {
        let (stopping, _) = watch::channel(false);
        Self {
            permits: Arc::new(Semaphore::new(usize::from(capacity))),
            capacity: u32::from(capacity),
            stopping,
        }
    }

    pub(super) fn cancel(&self) {
        self.stopping.send_replace(true);
    }

    pub(super) async fn drain(&self) {
        let _all = self.permits.acquire_many(self.capacity).await;
    }
}

enum Chunk {
    Data(Vec<u8>),
    Finish,
    #[cfg(test)]
    Panic,
}

pub(crate) struct UploadSession {
    sender: mpsc::Sender<Chunk>,
    response: oneshot::Receiver<Result<UploadResponse, RuntimeError>>,
    pub(crate) cancelled: watch::Receiver<bool>,
}

impl UploadSession {
    pub(crate) async fn send(&self, bytes: &[u8]) -> Result<(), RuntimeError> {
        if bytes.len() > CHUNK_BYTES {
            return Err(RuntimeError::Workspace(crate::Error::UploadTooLarge));
        }
        self.sender
            .send(Chunk::Data(bytes.to_vec()))
            .await
            .map_err(|_| RuntimeError::Closed)
    }

    pub(crate) async fn finish(self) -> Result<UploadResponse, RuntimeError> {
        let _ = self.sender.send(Chunk::Finish).await;
        self.result().await
    }

    pub(crate) async fn result(self) -> Result<UploadResponse, RuntimeError> {
        drop(self.sender);
        self.response.await.map_err(|_| RuntimeError::Worker)?
    }
}

impl WorkspaceHandle {
    pub(crate) async fn begin_upload_session(
        &self,
        token: String,
        hash: String,
        size: Option<u64>,
    ) -> Result<UploadSession, RuntimeError> {
        if self.stopping.load(Ordering::Acquire) {
            return Err(RuntimeError::Closed);
        }
        let permit = Arc::clone(&self.uploads.permits)
            .try_acquire_owned()
            .map_err(|_| RuntimeError::Busy)?;
        let (sender, receiver) = mpsc::channel(2);
        let (ready, admitted) = oneshot::channel();
        let (reply, response) = oneshot::channel();
        let handle = self.clone();
        let cancelled = self.uploads.stopping.subscribe();
        let worker_cancelled = cancelled.clone();
        // The supervisor owns admission until the blocking writer and DB cleanup have both ended.
        tokio::spawn(async move {
            let _permit = permit;
            let (begin_reply, begin_response) = oneshot::channel();
            let grant = async {
                handle.enqueue(Command::BeginUpload {
                    token: token.clone(),
                    hash,
                    size,
                    reply: begin_reply,
                })?;
                begin_response.await.map_err(|_| RuntimeError::Closed)?
            }
            .await;
            let grant = match grant {
                Ok(grant) => grant,
                Err(error) => {
                    let _ = ready.send(Err(error));
                    return;
                }
            };
            let id = grant.upload_id;
            let worker = tokio::task::spawn_blocking(move || {
                write_upload(grant.writer, receiver, worker_cancelled)
            });
            let _ = ready.send(Ok(()));
            let result = match worker.await {
                Ok(Ok(sealed)) => {
                    let (finish_reply, finish_response) = oneshot::channel();
                    let command = Command::FinishUpload {
                        token,
                        id,
                        sealed,
                        reply: finish_reply,
                    };
                    // Accepted completion must survive loss of the HTTP response and shutdown admission closure.
                    match handle.commands.send(command).await {
                        Ok(()) => finish_response
                            .await
                            .map_err(|_| RuntimeError::Closed)
                            .and_then(|result| result),
                        Err(error) => {
                            let _ = tokio::task::spawn_blocking(move || drop(error)).await;
                            Err(RuntimeError::Closed)
                        }
                    }
                }
                Ok(Err(error)) => Err(error),
                Err(_) => Err(RuntimeError::Worker),
            };
            if result.is_err() {
                let (abort_reply, abort_response) = oneshot::channel();
                if handle
                    .commands
                    .send(Command::AbortUpload {
                        id,
                        reply: abort_reply,
                    })
                    .await
                    .is_ok()
                {
                    let _ = abort_response.await;
                }
            }
            let result = result.map(|uploaded| UploadResponse {
                upload_id: uploaded.upload_id.to_string(),
                content_hash: format!("sha256:{}", uploaded.content_hash),
                size: uploaded.size.to_string(),
                expires_at: uploaded.expires_at.to_string(),
            });
            let _ = reply.send(result);
        });
        admitted.await.map_err(|_| RuntimeError::Worker)??;
        Ok(UploadSession {
            sender,
            response,
            cancelled,
        })
    }
}

fn write_upload(
    mut writer: UploadWriter,
    mut receiver: mpsc::Receiver<Chunk>,
    mut cancelled: watch::Receiver<bool>,
) -> Result<SealedUpload, RuntimeError> {
    let runtime = tokio::runtime::Handle::current();
    loop {
        if *cancelled.borrow() {
            return Err(RuntimeError::Closed);
        }
        let chunk = runtime.block_on(async {
            tokio::select! { biased;
                _ = cancelled.changed() => None,
                chunk = receiver.recv() => chunk,
            }
        });
        match chunk {
            Some(Chunk::Data(bytes)) => writer
                .write_chunk(&bytes)
                .map_err(|error| RuntimeError::Workspace(error.into()))?,
            Some(Chunk::Finish) => {
                return writer
                    .seal()
                    .map_err(|error| RuntimeError::Workspace(error.into()));
            }
            #[cfg(test)]
            Some(Chunk::Panic) => panic!("injected upload writer panic"),
            None => return Err(RuntimeError::Closed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::WorkspaceConfig;
    use crate::runtime::WorkspaceRuntime;
    use notryx_protocol::{AdminRequest, AdminResult, PairRequest};
    use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};

    async fn setup(
        capacity: u16,
    ) -> (tempfile::TempDir, WorkspaceConfig, WorkspaceRuntime, String) {
        let directory = tempfile::tempdir().unwrap();
        let config = WorkspaceConfig {
            notes_root: directory.path().join("notes"),
            state_root: directory.path().join("state"),
            uploads: crate::UploadLimits {
                max_receiving: capacity,
                ..Default::default()
            },
        };
        std::fs::create_dir(&config.notes_root).unwrap();
        std::fs::create_dir(&config.state_root).unwrap();
        WorkspaceRuntime::initialize(config.clone()).await.unwrap();
        let runtime = WorkspaceRuntime::start(config.clone()).await.unwrap();
        let handle = runtime.handle();
        let AdminResult::Status {
            owner_principal_id, ..
        } = handle.admin(AdminRequest::Status {}).await.unwrap()
        else {
            panic!("owner")
        };
        let AdminResult::Pairing {
            workspace_id,
            pairing_secret,
            ..
        } = handle
            .admin(AdminRequest::IssuePairing {
                principal_id: owner_principal_id,
            })
            .await
            .unwrap()
        else {
            panic!("pairing")
        };
        let device = handle
            .pair(PairRequest {
                protocol: notryx_protocol::CONTRACT.into(),
                workspace_id,
                pairing_secret,
            })
            .await
            .unwrap();
        (directory, config, runtime, device.device_token)
    }

    async fn receiving_count(config: &WorkspaceConfig) -> i64 {
        let mut connection = SqliteConnection::connect_with(
            &SqliteConnectOptions::new().filename(config.state_root.join("state.db")),
        )
        .await
        .unwrap();
        let count = sqlx::query_scalar("SELECT count(*) FROM uploads WHERE status='receiving'")
            .fetch_one(&mut connection)
            .await
            .unwrap();
        connection.close().await.unwrap();
        count
    }

    #[tokio::test]
    async fn writer_panic_releases_database_reservation_permit_and_store_lease() {
        let (_directory, config, runtime, token) = setup(1).await;
        let handle = runtime.handle();
        let session = handle
            .begin_upload_session(token.clone(), crate::hash(b""), Some(0))
            .await
            .unwrap();
        assert_eq!(receiving_count(&config).await, 1);
        session.sender.send(Chunk::Panic).await.unwrap();
        assert_eq!(session.result().await, Err(RuntimeError::Worker));
        assert_eq!(receiving_count(&config).await, 0);
        let session = handle
            .begin_upload_session(token, crate::hash(b""), Some(0))
            .await
            .unwrap();
        assert!(session.finish().await.is_ok());
        runtime.shutdown().await.unwrap();
        crate::Workspace::open(&config.notes_root, &config.state_root)
            .await
            .unwrap()
            .close()
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn shutdown_cancels_all_configured_upload_slots_with_live_senders() {
        let (_directory, config, runtime, token) = setup(3).await;
        let handle = runtime.handle();
        let mut sessions = Vec::new();
        for _ in 0..3 {
            sessions.push(
                handle
                    .begin_upload_session(token.clone(), crate::hash(b""), Some(0))
                    .await
                    .unwrap(),
            );
        }
        assert!(matches!(
            handle
                .begin_upload_session(token, crate::hash(b""), Some(0))
                .await,
            Err(RuntimeError::Busy)
        ));
        assert_eq!(receiving_count(&config).await, 3);
        tokio::time::timeout(std::time::Duration::from_secs(5), runtime.shutdown())
            .await
            .unwrap()
            .unwrap();
        for session in sessions {
            assert_eq!(session.result().await, Err(RuntimeError::Closed));
        }
        assert_eq!(receiving_count(&config).await, 0);
        crate::Workspace::open(&config.notes_root, &config.state_root)
            .await
            .unwrap()
            .close()
            .await
            .unwrap();
    }
}
