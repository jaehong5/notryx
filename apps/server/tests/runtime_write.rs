use notryx_domain::OperationId;
use notryx_protocol::{AdminRequest, CONTRACT, PairRequest, PairResponse, WorkspaceRole};
use notryx_server::auth::AuthError;
use notryx_server::config::WorkspaceConfig;
use notryx_server::runtime::{RuntimeError, WorkspaceRuntime};
use notryx_server::{Document, Error, Outcome, SaveContent, Workspace};
use sha2::{Digest, Sha256};
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

struct Fixture {
    _directory: tempfile::TempDir,
    config: WorkspaceConfig,
    document: Document,
    device: PairResponse,
    other_device: PairResponse,
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        .try_into()
        .unwrap()
}

fn operation_id() -> OperationId {
    Uuid::now_v7().to_string().parse().unwrap()
}

fn digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

async fn pair(workspace: &mut Workspace, principal: Uuid, issued_at: i64) -> PairResponse {
    let grant = workspace.issue_pairing(principal, issued_at).await.unwrap();
    workspace
        .pair(
            PairRequest {
                protocol: CONTRACT.into(),
                workspace_id: grant.workspace_id,
                pairing_secret: grant.pairing_secret,
            },
            issued_at,
        )
        .await
        .unwrap()
}

impl Fixture {
    async fn new(role: WorkspaceRole) -> Self {
        Self::at(role, now()).await
    }

    async fn at(role: WorkspaceRole, issued_at: i64) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let config = WorkspaceConfig {
            notes_root: directory.path().join("notes"),
            state_root: directory.path().join("state"),
            uploads: Default::default(),
        };
        std::fs::create_dir(&config.notes_root).unwrap();
        std::fs::create_dir(&config.state_root).unwrap();
        let mut workspace = Workspace::initialize(&config.notes_root, &config.state_root)
            .await
            .unwrap();
        std::fs::write(config.notes_root.join("Document.md"), b"original\n").unwrap();
        let document = workspace.adopt_root_document("Document.md").await.unwrap();
        let principal = workspace.create_principal(role).await.unwrap();
        let device = pair(&mut workspace, principal, issued_at).await;
        let other_device = pair(&mut workspace, principal, issued_at).await;
        workspace.close().await.unwrap();
        Self {
            _directory: directory,
            config,
            document,
            device,
            other_device,
        }
    }

    async fn start(&self) -> WorkspaceRuntime {
        WorkspaceRuntime::start(self.config.clone()).await.unwrap()
    }

    fn request(&self, device: &PairResponse, sequence: u64, bytes: &[u8]) -> SaveContent {
        SaveContent {
            epoch: device.workspace_epoch,
            stream_id: device.device_stream_id.parse().unwrap(),
            operation_seq: sequence,
            operation_id: operation_id(),
            node_id: self.document.node_id,
            expected_content_rev: self.document.content_rev.clone(),
            content_hash: digest(bytes),
            bytes: bytes.to_vec(),
        }
    }
}

#[tokio::test]
async fn authenticated_save_and_own_result_survive_restart_without_reapplying() {
    let fixture = Fixture::new(WorkspaceRole::Editor).await;
    let runtime = fixture.start().await;
    let handle = runtime.handle();
    let bytes = b"\xef\xbb\xbf# Preserved\r\n\xff\r\n";
    let request = fixture.request(&fixture.device, 1, bytes);
    let record = handle
        .save_content(fixture.device.device_token.clone(), request.clone())
        .await
        .unwrap();
    assert_eq!(record.operation_id, request.operation_id);
    assert_eq!(record.operation_seq, 1);
    assert_eq!(record.workspace_epoch, request.epoch);
    assert!(matches!(record.outcome, Outcome::Saved(_)));
    assert_eq!(
        handle
            .operation(fixture.device.device_token.clone(), request.operation_id)
            .await
            .unwrap(),
        record
    );
    assert_eq!(
        handle
            .content(fixture.device.device_token.clone(), request.node_id, None)
            .await
            .unwrap()
            .1,
        bytes
    );
    runtime.shutdown().await.unwrap();

    let restarted = fixture.start().await;
    let handle = restarted.handle();
    assert_eq!(
        handle
            .operation(fixture.device.device_token.clone(), request.operation_id)
            .await
            .unwrap(),
        record
    );
    assert_eq!(
        handle
            .save_content(fixture.device.device_token.clone(), request)
            .await
            .unwrap(),
        record
    );
    assert_eq!(
        handle
            .server_info(fixture.device.device_token.clone())
            .await
            .unwrap()
            .head_seq
            .get(),
        2
    );
    assert_eq!(
        std::fs::read(fixture.config.notes_root.join("Document.md")).unwrap(),
        bytes
    );
    restarted.shutdown().await.unwrap();
}

#[tokio::test]
async fn role_changes_apply_to_new_writes_but_keep_owned_retries_and_results_readable() {
    let fixture = Fixture::new(WorkspaceRole::Reader).await;
    let runtime = fixture.start().await;
    let handle = runtime.handle();
    let request = fixture.request(&fixture.device, 1, b"saved after promotion\n");
    assert_eq!(
        handle
            .save_content(fixture.device.device_token.clone(), request.clone())
            .await,
        Err(RuntimeError::Auth(AuthError::Forbidden))
    );
    handle
        .admin(AdminRequest::SetRole {
            principal_id: fixture.device.principal_id.clone(),
            role: WorkspaceRole::Editor,
        })
        .await
        .unwrap();
    let record = handle
        .save_content(fixture.device.device_token.clone(), request.clone())
        .await
        .unwrap();
    handle
        .admin(AdminRequest::SetRole {
            principal_id: fixture.device.principal_id.clone(),
            role: WorkspaceRole::Reader,
        })
        .await
        .unwrap();
    assert_eq!(
        handle
            .operation(fixture.device.device_token.clone(), request.operation_id)
            .await
            .unwrap(),
        record
    );
    assert_eq!(
        handle
            .save_content(fixture.device.device_token.clone(), request.clone())
            .await
            .unwrap(),
        record
    );
    let mut invalid_digest = request.clone();
    invalid_digest.bytes.push(b'!');
    assert_eq!(
        handle
            .save_content(fixture.device.device_token.clone(), invalid_digest)
            .await,
        Err(RuntimeError::Workspace(Error::DigestMismatch))
    );
    let mut altered = request.clone();
    altered.bytes = b"different payload\n".to_vec();
    altered.content_hash = digest(&altered.bytes);
    assert_eq!(
        handle
            .save_content(fixture.device.device_token.clone(), altered)
            .await,
        Err(RuntimeError::Workspace(Error::IdempotencyMismatch))
    );
    let mut next = fixture.request(&fixture.device, 2, b"new write is forbidden\n");
    let Outcome::Saved(saved) = record.outcome else {
        panic!("promotion must allow the initial write");
    };
    next.expected_content_rev = saved.content_rev;
    assert_eq!(
        handle
            .save_content(fixture.device.device_token.clone(), next)
            .await,
        Err(RuntimeError::Auth(AuthError::Forbidden))
    );
    assert_eq!(
        std::fs::read(fixture.config.notes_root.join("Document.md")).unwrap(),
        request.bytes
    );
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn streams_results_and_revocation_are_bound_to_the_requesting_device() {
    let fixture = Fixture::new(WorkspaceRole::Editor).await;
    let runtime = fixture.start().await;
    let handle = runtime.handle();
    let request = fixture.request(&fixture.device, 1, b"owned result\n");
    let mut foreign_stream = request.clone();
    foreign_stream.stream_id = fixture.other_device.device_stream_id.parse().unwrap();
    assert_eq!(
        handle
            .save_content(fixture.device.device_token.clone(), foreign_stream)
            .await,
        Err(RuntimeError::Auth(AuthError::Forbidden))
    );
    let mut foreign_epoch = request.clone();
    foreign_epoch.epoch = Uuid::new_v4().to_string().parse().unwrap();
    assert_eq!(
        handle
            .save_content(fixture.device.device_token.clone(), foreign_epoch)
            .await,
        Err(RuntimeError::Workspace(Error::WorkspaceMismatch))
    );
    handle
        .save_content(fixture.device.device_token.clone(), request.clone())
        .await
        .unwrap();
    for id in [request.operation_id, operation_id()] {
        assert_eq!(
            handle
                .operation(fixture.other_device.device_token.clone(), id)
                .await,
            Err(RuntimeError::Workspace(Error::NotFound))
        );
    }
    handle
        .admin(AdminRequest::RevokeDevice {
            device_id: fixture.device.device_id.clone(),
        })
        .await
        .unwrap();
    assert_eq!(
        handle
            .operation(fixture.device.device_token.clone(), request.operation_id)
            .await,
        Err(RuntimeError::Auth(AuthError::Unauthorized))
    );
    assert_eq!(
        handle
            .save_content(fixture.device.device_token.clone(), request)
            .await,
        Err(RuntimeError::Auth(AuthError::Unauthorized))
    );
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn expired_credentials_cannot_retry_or_read_a_previously_committed_operation() {
    let fixture = Fixture::at(WorkspaceRole::Editor, now() - 91 * 24 * 60 * 60).await;
    let request = fixture.request(&fixture.device, 1, b"committed before expiry\n");
    let mut workspace = Workspace::open(&fixture.config.notes_root, &fixture.config.state_root)
        .await
        .unwrap();
    workspace.save(request.clone()).await.unwrap();
    workspace.close().await.unwrap();
    let runtime = fixture.start().await;
    let handle = runtime.handle();
    assert_eq!(
        handle
            .operation(fixture.device.device_token.clone(), request.operation_id)
            .await,
        Err(RuntimeError::Auth(AuthError::Unauthorized))
    );
    assert_eq!(
        handle
            .save_content(fixture.device.device_token.clone(), request.clone())
            .await,
        Err(RuntimeError::Auth(AuthError::Unauthorized))
    );
    assert_eq!(
        std::fs::read(fixture.config.notes_root.join("Document.md")).unwrap(),
        request.bytes
    );
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn sequence_gaps_and_reused_identifiers_never_apply_a_different_request() {
    let fixture = Fixture::new(WorkspaceRole::Editor).await;
    let runtime = fixture.start().await;
    let handle = runtime.handle();
    let request = fixture.request(&fixture.device, 1, b"the only applied write\n");
    let mut gap = request.clone();
    gap.operation_seq = 2;
    assert_eq!(
        handle
            .save_content(fixture.device.device_token.clone(), gap)
            .await,
        Err(RuntimeError::Workspace(Error::OperationGap))
    );
    let record = handle
        .save_content(fixture.device.device_token.clone(), request.clone())
        .await
        .unwrap();
    let mut changed_payload = request.clone();
    changed_payload.bytes = b"different\n".to_vec();
    changed_payload.content_hash = digest(&changed_payload.bytes);
    let mut changed_operation = request.clone();
    changed_operation.operation_id = operation_id();
    let mut changed_sequence = request.clone();
    changed_sequence.operation_seq = 2;
    for altered in [changed_payload, changed_operation, changed_sequence] {
        assert_eq!(
            handle
                .save_content(fixture.device.device_token.clone(), altered)
                .await,
            Err(RuntimeError::Workspace(Error::IdempotencyMismatch))
        );
    }
    assert_eq!(
        handle
            .operation(fixture.device.device_token.clone(), request.operation_id)
            .await
            .unwrap(),
        record
    );
    assert_eq!(
        std::fs::read(fixture.config.notes_root.join("Document.md")).unwrap(),
        request.bytes
    );
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn two_devices_saving_the_same_base_commit_once_and_keep_both_results() {
    let fixture = Fixture::new(WorkspaceRole::Editor).await;
    let runtime = fixture.start().await;
    let handle = runtime.handle();
    let first = fixture.request(&fixture.device, 1, b"first device\n");
    let second = fixture.request(&fixture.other_device, 1, b"second device\n");
    let (first_result, second_result) = tokio::join!(
        handle.save_content(fixture.device.device_token.clone(), first.clone()),
        handle.save_content(fixture.other_device.device_token.clone(), second.clone()),
    );
    let first_result = first_result.unwrap();
    let second_result = second_result.unwrap();
    let (saved, rejected, bytes) = match (&first_result.outcome, &second_result.outcome) {
        (Outcome::Saved(saved), Outcome::PreconditionFailed { current }) => {
            (saved, current, &first.bytes)
        }
        (Outcome::PreconditionFailed { current }, Outcome::Saved(saved)) => {
            (saved, current, &second.bytes)
        }
        outcomes => panic!("expected exactly one committed write and one conflict: {outcomes:?}"),
    };
    assert_eq!(&saved.content_rev, rejected);
    assert_eq!(
        std::fs::read(fixture.config.notes_root.join("Document.md")).unwrap(),
        *bytes
    );
    assert_eq!(
        handle
            .operation(fixture.device.device_token.clone(), first.operation_id)
            .await
            .unwrap(),
        first_result
    );
    assert_eq!(
        handle
            .operation(
                fixture.other_device.device_token.clone(),
                second.operation_id
            )
            .await
            .unwrap(),
        second_result
    );
    assert_eq!(
        handle
            .server_info(fixture.device.device_token.clone())
            .await
            .unwrap()
            .head_seq
            .get(),
        2
    );
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn old_receipts_do_not_require_current_revisions_objects_or_readiness() {
    let fixture = Fixture::new(WorkspaceRole::Editor).await;
    let runtime = fixture.start().await;
    let handle = runtime.handle();
    let first = fixture.request(&fixture.device, 1, b"first committed content\n");
    let first_result = handle
        .save_content(fixture.device.device_token.clone(), first.clone())
        .await
        .unwrap();
    let Outcome::Saved(first_document) = &first_result.outcome else {
        panic!("initial save must commit");
    };
    let mut second = fixture.request(&fixture.device, 2, b"current committed content\n");
    second.expected_content_rev = first_document.content_rev.clone();
    let second_result = handle
        .save_content(fixture.device.device_token.clone(), second.clone())
        .await
        .unwrap();
    let Outcome::Saved(current) = second_result.outcome else {
        panic!("subsequent save must commit");
    };
    let old_object = fixture
        .config
        .state_root
        .join("objects/sha256")
        .join(&first_document.content_hash);
    let current_object = fixture
        .config
        .state_root
        .join("objects/sha256")
        .join(&current.content_hash);
    std::fs::remove_file(&old_object).unwrap();
    std::fs::remove_file(&current_object).unwrap();
    assert_eq!(
        handle
            .content(fixture.device.device_token.clone(), current.node_id, None)
            .await,
        Err(RuntimeError::Workspace(Error::Storage))
    );
    assert!(!handle.health().await.unwrap().ready);
    assert_eq!(
        handle
            .save_content(fixture.device.device_token.clone(), first.clone())
            .await
            .unwrap(),
        first_result
    );
    assert_eq!(
        handle
            .operation(fixture.device.device_token.clone(), first.operation_id)
            .await
            .unwrap(),
        first_result
    );
    assert!(!old_object.exists());
    assert!(!current_object.exists());
    assert_eq!(
        std::fs::read(fixture.config.notes_root.join("Document.md")).unwrap(),
        second.bytes
    );
    runtime.shutdown().await.unwrap();
}
