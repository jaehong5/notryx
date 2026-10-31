#[path = "support/legacy.rs"]
mod legacy;
use notryx_protocol::{CONTRACT, PairRequest, PairResponse, WorkspaceRole};
use notryx_server::auth::{AuthError, PairingGrant};
use notryx_server::{Error, Outcome, SaveContent, Workspace};
use sha2::{Digest, Sha256};
use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
use std::{collections::BTreeMap, path::PathBuf};
use uuid::Uuid;

const NOW: i64 = 1_800_000_000;

struct Fixture {
    _directory: tempfile::TempDir,
    notes: PathBuf,
    state: PathBuf,
}

impl Fixture {
    async fn new() -> (Self, Workspace) {
        let directory = tempfile::tempdir().unwrap();
        let notes = directory.path().join("notes");
        let state = directory.path().join("state");
        std::fs::create_dir(&notes).unwrap();
        std::fs::create_dir(&state).unwrap();
        let workspace = Workspace::initialize(&notes, &state).await.unwrap();
        (
            Self {
                _directory: directory,
                notes,
                state,
            },
            workspace,
        )
    }

    async fn database(&self) -> SqliteConnection {
        SqliteConnection::connect_with(
            &SqliteConnectOptions::new()
                .filename(self.state.join("state.db"))
                .foreign_keys(true),
        )
        .await
        .unwrap()
    }

    async fn downgrade_to_v1(&self) {
        let mut db = self.database().await;
        legacy::nodes_before_deletion(&mut db).await;
        sqlx::raw_sql(
            "ALTER TABLE operation_receipts DROP COLUMN node_etag;
             DROP TABLE retired_operations;
             ALTER TABLE operation_streams DROP COLUMN retired_through;
             DROP TABLE IF EXISTS devices;
             DROP TABLE IF EXISTS pairing_secrets;
             DROP TABLE IF EXISTS principals;
             DROP INDEX IF EXISTS auth_workspace_id;
             PRAGMA user_version=1;",
        )
        .execute(&mut db)
        .await
        .unwrap();
        db.close().await.unwrap();
    }
}

async fn legacy_rows(db: &mut SqliteConnection) -> BTreeMap<String, Vec<String>> {
    let mut result = BTreeMap::new();
    for (table, query) in [
        (
            "workspaces",
            "SELECT json_array(singleton,workspace_id,epoch,root_id,head_seq) FROM workspaces",
        ),
        (
            "nodes",
            "SELECT json_array(node_id,parent_id,name,normalized_name_key,kind,content_rev,meta_rev,content_hash,size,children_generation,last_change_seq) FROM nodes",
        ),
        (
            "change_sets",
            "SELECT json_array(seq,operation_id) FROM change_sets",
        ),
        (
            "node_changes",
            "SELECT json_array(seq,node_id,post_state) FROM node_changes",
        ),
        (
            "operation_streams",
            "SELECT json_array(stream_id,last_seq) FROM operation_streams",
        ),
        (
            "operation_receipts",
            "SELECT json_array(operation_id,stream_id,operation_seq,fingerprint,outcome,pending) FROM operation_receipts",
        ),
        (
            "write_intents",
            "SELECT json_array(operation_id,intent,observed_hash,recovery_conflict) FROM write_intents",
        ),
    ] {
        let mut rows = sqlx::query_scalar::<_, String>(query)
            .fetch_all(&mut *db)
            .await
            .unwrap();
        rows.sort();
        result.insert(table.to_owned(), rows);
    }
    result
}

fn digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn pairing_request(grant: &PairingGrant) -> PairRequest {
    PairRequest {
        protocol: CONTRACT.to_owned(),
        workspace_id: grant.workspace_id,
        pairing_secret: grant.pairing_secret.clone(),
    }
}

async fn pair_owner(workspace: &mut Workspace) -> PairResponse {
    let owner = workspace.auth_status().await.unwrap().owner_principal_id;
    let grant = workspace.issue_pairing(owner, NOW).await.unwrap();
    workspace.pair(pairing_request(&grant), NOW).await.unwrap()
}

#[tokio::test]
async fn renewal_window_and_absolute_expiry_have_exact_boundaries() {
    const DAY: i64 = 24 * 60 * 60;
    let (fixture, mut workspace) = Fixture::new().await;
    let paired = pair_owner(&mut workspace).await;
    let identity = workspace
        .authenticate(&paired.device_token, NOW)
        .await
        .unwrap();
    let early = workspace
        .renew_device(&paired.device_token, NOW + 60 * DAY - 1)
        .await
        .unwrap();
    assert_eq!(early.idle_expires_at, paired.idle_expires_at);
    for day in [60, 120, 180, 240, 300] {
        let renewed = workspace
            .renew_device(&paired.device_token, NOW + day * DAY)
            .await
            .unwrap();
        assert_eq!(
            renewed.idle_expires_at,
            (NOW + (day + 90).min(365) * DAY).to_string()
        );
        assert_eq!(renewed.absolute_expires_at, paired.absolute_expires_at);
        assert_eq!(
            workspace
                .authenticate(&paired.device_token, NOW + day * DAY)
                .await
                .unwrap(),
            identity
        );
    }
    workspace.close().await.unwrap();
    let mut workspace = Workspace::open(&fixture.notes, &fixture.state)
        .await
        .unwrap();
    let last_valid = workspace
        .renew_device(&paired.device_token, NOW + 365 * DAY - 1)
        .await
        .unwrap();
    assert_eq!(last_valid.idle_expires_at, paired.absolute_expires_at);
    assert_eq!(
        workspace
            .renew_device(&paired.device_token, NOW + 365 * DAY)
            .await,
        Err(AuthError::Unauthorized)
    );
    workspace.close().await.unwrap();
}

#[tokio::test]
async fn renewal_cannot_revive_an_expired_or_revoked_token() {
    const DAY: i64 = 24 * 60 * 60;
    let (_fixture, mut workspace) = Fixture::new().await;
    let paired = pair_owner(&mut workspace).await;
    for time in [NOW - 1, NOW + 90 * DAY] {
        assert_eq!(
            workspace.renew_device(&paired.device_token, time).await,
            Err(AuthError::Unauthorized)
        );
    }
    let mut wrong_secret = paired.device_token.clone();
    let secret_start = wrong_secret.rfind('.').unwrap() + 1;
    let replacement = if wrong_secret.as_bytes()[secret_start] == b'A' {
        "B"
    } else {
        "A"
    };
    wrong_secret.replace_range(secret_start..secret_start + 1, replacement);
    assert_eq!(
        workspace.renew_device(&wrong_secret, NOW + 60 * DAY).await,
        Err(AuthError::Unauthorized)
    );
    workspace
        .revoke_device(paired.device_id.parse().unwrap(), NOW + 60 * DAY)
        .await
        .unwrap();
    assert_eq!(
        workspace
            .renew_device(&paired.device_token, NOW + 60 * DAY)
            .await,
        Err(AuthError::Unauthorized)
    );
    workspace.close().await.unwrap();
}

#[tokio::test]
async fn failed_renewal_keeps_the_previous_expiry_and_can_be_retried() {
    const DAY: i64 = 24 * 60 * 60;
    let (fixture, mut workspace) = Fixture::new().await;
    let paired = pair_owner(&mut workspace).await;
    let mut db = fixture.database().await;
    sqlx::raw_sql("CREATE TRIGGER fail_renewal BEFORE UPDATE OF idle_expires_at ON devices BEGIN SELECT RAISE(ABORT, 'synthetic renewal failure'); END;").execute(&mut db).await.unwrap();
    assert_eq!(
        workspace
            .renew_device(&paired.device_token, NOW + 60 * DAY)
            .await,
        Err(AuthError::Storage)
    );
    let idle: i64 = sqlx::query_scalar("SELECT idle_expires_at FROM devices")
        .fetch_one(&mut db)
        .await
        .unwrap();
    assert_eq!(idle.to_string(), paired.idle_expires_at);
    sqlx::raw_sql("DROP TRIGGER fail_renewal;")
        .execute(&mut db)
        .await
        .unwrap();
    db.close().await.unwrap();
    let renewed = workspace
        .renew_device(&paired.device_token, NOW + 60 * DAY)
        .await
        .unwrap();
    assert_eq!(renewed.idle_expires_at, (NOW + 150 * DAY).to_string());
    workspace.close().await.unwrap();
}

#[tokio::test]
async fn fresh_init_has_one_credential_free_owner() {
    let (fixture, mut workspace) = Fixture::new().await;
    let status = workspace.auth_status().await.unwrap();
    assert_eq!(status.workspace_id, workspace.workspace_id());
    assert_eq!(status.workspace_epoch, workspace.epoch());
    let mut db = fixture.database().await;
    let version: i64 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(&mut db)
        .await
        .unwrap();
    assert_eq!(version, 10);
    let owners: Vec<String> =
        sqlx::query_scalar("SELECT principal_id FROM principals WHERE workspace_role='owner'")
            .fetch_all(&mut db)
            .await
            .unwrap();
    assert_eq!(owners, vec![status.owner_principal_id.to_string()]);
    for (table, query) in [
        ("devices", "SELECT COUNT(*) FROM devices"),
        ("pairing_secrets", "SELECT COUNT(*) FROM pairing_secrets"),
        (
            "operation_streams",
            "SELECT COUNT(*) FROM operation_streams",
        ),
    ] {
        let count: i64 = sqlx::query_scalar(query).fetch_one(&mut db).await.unwrap();
        assert_eq!(count, 0, "fresh initialization created rows in {table}");
    }
    db.close().await.unwrap();
    workspace.close().await.unwrap();
}

#[tokio::test]
async fn pairing_is_one_use_and_token_survives_restart() {
    let (fixture, mut workspace) = Fixture::new().await;
    let owner = workspace.auth_status().await.unwrap().owner_principal_id;
    let grant = workspace.issue_pairing(owner, NOW).await.unwrap();
    assert_eq!(grant.workspace_id, workspace.workspace_id());
    assert_eq!(grant.principal_id, owner);
    assert_eq!(grant.expires_at, (NOW + 300).to_string());
    let request = || PairRequest {
        protocol: CONTRACT.to_owned(),
        workspace_id: grant.workspace_id,
        pairing_secret: grant.pairing_secret.clone(),
    };
    let paired = workspace.pair(request(), NOW + 1).await.unwrap();
    assert_eq!(paired.protocol, CONTRACT);
    assert_eq!(paired.workspace_id, workspace.workspace_id());
    assert_eq!(paired.workspace_epoch, workspace.epoch());
    assert_eq!(paired.principal_id.parse::<Uuid>().unwrap(), owner);
    assert_eq!(paired.role, WorkspaceRole::Owner);
    assert!(matches!(
        workspace.pair(request(), NOW + 2).await,
        Err(AuthError::Unauthorized)
    ));
    let identity = workspace
        .authenticate(&paired.device_token, NOW + 2)
        .await
        .unwrap();
    assert_eq!(identity.principal_id, owner);
    assert_eq!(
        identity.device_id,
        paired.device_id.parse::<Uuid>().unwrap()
    );
    assert_eq!(
        identity.stream_id,
        paired.device_stream_id.parse::<Uuid>().unwrap()
    );
    assert_eq!(identity.role, WorkspaceRole::Owner);
    workspace.close().await.unwrap();
    let mut reopened = Workspace::open(&fixture.notes, &fixture.state)
        .await
        .unwrap();
    assert_eq!(
        reopened
            .authenticate(&paired.device_token, NOW + 3)
            .await
            .unwrap(),
        identity
    );
    reopened.close().await.unwrap();
}

#[tokio::test]
async fn migration_preserves_legacy_state_and_is_explicit() {
    let (fixture, mut workspace) = Fixture::new().await;
    std::fs::write(fixture.notes.join("Legacy.md"), b"\xef\xbb\xbf# Legacy\r\n").unwrap();
    let document = workspace.adopt_root_document("Legacy.md").await.unwrap();
    let bytes = b"\xef\xbb\xbf# Saved legacy\r\n".to_vec();
    let request = SaveContent {
        epoch: workspace.epoch(),
        stream_id: workspace.register_stream().await.unwrap(),
        operation_seq: 1,
        operation_id: Uuid::now_v7().to_string().parse().unwrap(),
        node_id: document.node_id,
        expected_content_rev: document.content_rev,
        content_hash: digest(&bytes),
        bytes: bytes.clone(),
    };
    let receipt = workspace.save(request.clone()).await.unwrap();
    assert!(matches!(&receipt.outcome, Outcome::Saved(_)));
    workspace.close().await.unwrap();
    fixture.downgrade_to_v1().await;
    let marker = std::fs::read(fixture.notes.join(".mdw-workspace.json")).unwrap();
    let mut db = fixture.database().await;
    let before = legacy_rows(&mut db).await;
    db.close().await.unwrap();
    assert!(matches!(
        Workspace::open(&fixture.notes, &fixture.state).await,
        Err(Error::MigrationRequired)
    ));
    Workspace::migrate(&fixture.notes, &fixture.state)
        .await
        .unwrap();
    let mut db = fixture.database().await;
    assert_eq!(legacy_rows(&mut db).await, before);
    let owners: Vec<String> =
        sqlx::query_scalar("SELECT principal_id FROM principals WHERE workspace_role='owner'")
            .fetch_all(&mut db)
            .await
            .unwrap();
    assert_eq!(owners.len(), 1);
    let owned_legacy_streams: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM devices")
        .fetch_one(&mut db)
        .await
        .unwrap();
    assert_eq!(owned_legacy_streams, 0);
    db.close().await.unwrap();
    Workspace::migrate(&fixture.notes, &fixture.state)
        .await
        .unwrap();
    let mut reopened = Workspace::open(&fixture.notes, &fixture.state)
        .await
        .unwrap();
    assert_eq!(
        reopened
            .auth_status()
            .await
            .unwrap()
            .owner_principal_id
            .to_string(),
        owners[0]
    );
    assert_eq!(reopened.save(request).await.unwrap(), receipt);
    assert_eq!(reopened.read(document.node_id).await.unwrap().1, bytes);
    assert_eq!(
        std::fs::read(fixture.notes.join(".mdw-workspace.json")).unwrap(),
        marker
    );
    reopened.close().await.unwrap();
}

#[tokio::test]
async fn owner_changes_require_atomic_transfer() {
    let (fixture, mut workspace) = Fixture::new().await;
    let owner = workspace.auth_status().await.unwrap().owner_principal_id;
    assert_eq!(
        workspace.create_principal(WorkspaceRole::Owner).await,
        Err(AuthError::Forbidden)
    );
    assert_eq!(
        workspace.set_role(owner, WorkspaceRole::Reader).await,
        Err(AuthError::Forbidden)
    );
    let editor = workspace
        .create_principal(WorkspaceRole::Editor)
        .await
        .unwrap();
    assert_eq!(
        workspace.set_role(editor, WorkspaceRole::Owner).await,
        Err(AuthError::Forbidden)
    );
    let mut db = fixture.database().await;
    sqlx::raw_sql("CREATE TRIGGER reject_owner BEFORE UPDATE OF workspace_role ON principals WHEN NEW.workspace_role='owner' BEGIN SELECT RAISE(ABORT,'injected owner transfer failure'); END;")
        .execute(&mut db).await.unwrap();
    assert_eq!(
        workspace
            .transfer_owner(editor, WorkspaceRole::Reader)
            .await,
        Err(AuthError::Storage)
    );
    assert_eq!(
        workspace.auth_status().await.unwrap().owner_principal_id,
        owner
    );
    let unchanged: String =
        sqlx::query_scalar("SELECT workspace_role FROM principals WHERE principal_id=?")
            .bind(editor.to_string())
            .fetch_one(&mut db)
            .await
            .unwrap();
    assert_eq!(unchanged, "editor");
    sqlx::raw_sql("DROP TRIGGER reject_owner")
        .execute(&mut db)
        .await
        .unwrap();
    workspace
        .transfer_owner(editor, WorkspaceRole::Reader)
        .await
        .unwrap();
    assert_eq!(
        workspace.auth_status().await.unwrap().owner_principal_id,
        editor
    );
    let previous: String =
        sqlx::query_scalar("SELECT workspace_role FROM principals WHERE principal_id=?")
            .bind(owner.to_string())
            .fetch_one(&mut db)
            .await
            .unwrap();
    assert_eq!(previous, "reader");
    let owners: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM principals WHERE workspace_role='owner'")
            .fetch_one(&mut db)
            .await
            .unwrap();
    assert_eq!(owners, 1);
    db.close().await.unwrap();
    workspace.close().await.unwrap();
}

#[tokio::test]
async fn pairing_checks_workspace_secret_role_and_expiry_without_consuming_valid_grants() {
    let (_fixture, mut workspace) = Fixture::new().await;
    let reader = workspace
        .create_principal(WorkspaceRole::Reader)
        .await
        .unwrap();
    let grant = workspace.issue_pairing(reader, NOW).await.unwrap();
    let mut wrong_workspace = pairing_request(&grant);
    wrong_workspace.workspace_id = Uuid::new_v4().to_string().parse().unwrap();
    assert!(matches!(
        workspace.pair(wrong_workspace, NOW + 1).await,
        Err(AuthError::Unauthorized)
    ));
    let mut wrong_secret = pairing_request(&grant);
    wrong_secret.pairing_secret.push('x');
    assert!(matches!(
        workspace.pair(wrong_secret, NOW + 1).await,
        Err(AuthError::Unauthorized)
    ));
    let mut wrong_protocol = pairing_request(&grant);
    wrong_protocol.protocol = "mdw-sync/unsupported".into();
    assert!(matches!(
        workspace.pair(wrong_protocol, NOW + 1).await,
        Err(AuthError::InvalidRequest)
    ));
    let paired = workspace
        .pair(pairing_request(&grant), NOW + 299)
        .await
        .unwrap();
    assert_eq!(paired.role, WorkspaceRole::Reader);
    let expired = workspace.issue_pairing(reader, NOW).await.unwrap();
    assert!(matches!(
        workspace.pair(pairing_request(&expired), NOW + 300).await,
        Err(AuthError::Unauthorized)
    ));
    let changed = workspace.issue_pairing(reader, NOW).await.unwrap();
    workspace
        .set_role(reader, WorkspaceRole::Editor)
        .await
        .unwrap();
    assert!(matches!(
        workspace.pair(pairing_request(&changed), NOW + 1).await,
        Err(AuthError::Unauthorized)
    ));
    let replacement = workspace.issue_pairing(reader, NOW + 1).await.unwrap();
    assert_eq!(
        workspace
            .pair(pairing_request(&replacement), NOW + 2)
            .await
            .unwrap()
            .role,
        WorkspaceRole::Editor
    );
    workspace.close().await.unwrap();
}

#[tokio::test]
async fn tokens_follow_current_roles_and_revocation_survives_restart() {
    let (fixture, mut workspace) = Fixture::new().await;
    let editor = workspace
        .create_principal(WorkspaceRole::Editor)
        .await
        .unwrap();
    let grant = workspace.issue_pairing(editor, NOW).await.unwrap();
    let paired = workspace.pair(pairing_request(&grant), NOW).await.unwrap();
    assert_eq!(
        workspace
            .authenticate(&paired.device_token, NOW + 1)
            .await
            .unwrap()
            .role,
        WorkspaceRole::Editor
    );
    workspace
        .set_role(editor, WorkspaceRole::Reader)
        .await
        .unwrap();
    assert_eq!(
        workspace
            .authenticate(&paired.device_token, NOW + 2)
            .await
            .unwrap()
            .role,
        WorkspaceRole::Reader
    );
    workspace
        .revoke_device(paired.device_id.parse().unwrap(), NOW + 3)
        .await
        .unwrap();
    assert_eq!(
        workspace.authenticate(&paired.device_token, NOW + 3).await,
        Err(AuthError::Unauthorized)
    );
    workspace.close().await.unwrap();
    let mut reopened = Workspace::open(&fixture.notes, &fixture.state)
        .await
        .unwrap();
    assert_eq!(
        reopened.authenticate(&paired.device_token, NOW + 4).await,
        Err(AuthError::Unauthorized)
    );
    reopened.close().await.unwrap();
}

#[tokio::test]
async fn credentials_are_hashed_and_devices_receive_independent_streams() {
    let (fixture, mut workspace) = Fixture::new().await;
    let legacy_stream = workspace.register_stream().await.unwrap();
    let owner = workspace.auth_status().await.unwrap().owner_principal_id;
    let first_grant = workspace.issue_pairing(owner, NOW).await.unwrap();
    let second_grant = workspace.issue_pairing(owner, NOW).await.unwrap();
    assert!(
        first_grant.pairing_secret != second_grant.pairing_secret,
        "pairing grants reused a credential"
    );
    let first = workspace
        .pair(pairing_request(&first_grant), NOW)
        .await
        .unwrap();
    let second = workspace
        .pair(pairing_request(&second_grant), NOW)
        .await
        .unwrap();
    assert!(
        first.device_token != second.device_token,
        "devices reused a credential"
    );
    assert_ne!(first.device_stream_id, second.device_stream_id);
    assert_ne!(first.device_stream_id, legacy_stream.to_string());
    assert_ne!(second.device_stream_id, legacy_stream.to_string());
    let mut db = fixture.database().await;
    let binding: (String, String) =
        sqlx::query_as("SELECT principal_id,stream_id FROM devices WHERE device_id=?")
            .bind(&first.device_id)
            .fetch_one(&mut db)
            .await
            .unwrap();
    assert_eq!(binding, (owner.to_string(), first.device_stream_id.clone()));
    let streams: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM operation_streams")
        .fetch_one(&mut db)
        .await
        .unwrap();
    assert_eq!(streams, 3);
    for query in [
        "SELECT typeof(token_hash),length(token_hash) FROM devices",
        "SELECT typeof(secret_hash),length(secret_hash) FROM pairing_secrets",
    ] {
        let hashes: Vec<(String, i64)> = sqlx::query_as(query).fetch_all(&mut db).await.unwrap();
        assert_eq!(hashes, vec![("blob".into(), 32), ("blob".into(), 32)]);
    }
    db.close().await.unwrap();
    workspace.close().await.unwrap();
    for name in ["state.db", "state.db-wal"] {
        let path = fixture.state.join(name);
        if path.exists() {
            let bytes = std::fs::read(path).unwrap();
            for credential in [
                &first_grant.pairing_secret,
                &second_grant.pairing_secret,
                &first.device_token,
                &second.device_token,
            ] {
                assert!(
                    !bytes
                        .windows(credential.len())
                        .any(|window| window == credential.as_bytes()),
                    "plaintext credential reached SQLite storage"
                );
            }
        }
    }
}

#[tokio::test]
async fn token_expiry_does_not_slide_on_reads_and_invalid_tokens_are_unauthorized() {
    let (_fixture, mut workspace) = Fixture::new().await;
    let paired = pair_owner(&mut workspace).await;
    let idle = NOW + 90 * 24 * 60 * 60;
    let absolute = NOW + 365 * 24 * 60 * 60;
    assert_eq!(paired.idle_expires_at, idle.to_string());
    assert_eq!(paired.absolute_expires_at, absolute.to_string());
    assert!(
        workspace
            .authenticate(&paired.device_token, idle - 1)
            .await
            .is_ok()
    );
    assert_eq!(
        workspace.authenticate(&paired.device_token, idle).await,
        Err(AuthError::Unauthorized)
    );
    assert_eq!(
        workspace.authenticate(&paired.device_token, absolute).await,
        Err(AuthError::Unauthorized)
    );
    for token in ["", "not-a-device-token"] {
        assert_eq!(
            workspace.authenticate(token, NOW + 1).await,
            Err(AuthError::Unauthorized)
        );
    }
    let mut wrong = paired.device_token.clone();
    wrong.push('x');
    assert_eq!(
        workspace.authenticate(&wrong, NOW + 1).await,
        Err(AuthError::Unauthorized)
    );
    let (_other_fixture, mut other) = Fixture::new().await;
    assert_eq!(
        other.authenticate(&paired.device_token, NOW + 1).await,
        Err(AuthError::Unauthorized)
    );
    other.close().await.unwrap();
    workspace.close().await.unwrap();
}

#[tokio::test]
async fn failed_pairing_transaction_preserves_secret_and_creates_no_device_or_stream() {
    let (fixture, mut workspace) = Fixture::new().await;
    let owner = workspace.auth_status().await.unwrap().owner_principal_id;
    let grant = workspace.issue_pairing(owner, NOW).await.unwrap();
    let mut db = fixture.database().await;
    sqlx::raw_sql("CREATE TRIGGER reject_device BEFORE INSERT ON devices BEGIN SELECT RAISE(ABORT,'injected device registration failure'); END;")
        .execute(&mut db).await.unwrap();
    assert!(matches!(
        workspace.pair(pairing_request(&grant), NOW + 1).await,
        Err(AuthError::Storage)
    ));
    for query in [
        "SELECT COUNT(*) FROM devices",
        "SELECT COUNT(*) FROM operation_streams",
        "SELECT COUNT(*) FROM pairing_secrets WHERE consumed_at IS NOT NULL",
    ] {
        let count: i64 = sqlx::query_scalar(query).fetch_one(&mut db).await.unwrap();
        assert_eq!(count, 0);
    }
    sqlx::raw_sql("DROP TRIGGER reject_device")
        .execute(&mut db)
        .await
        .unwrap();
    let paired = workspace
        .pair(pairing_request(&grant), NOW + 2)
        .await
        .unwrap();
    assert!(
        workspace
            .authenticate(&paired.device_token, NOW + 3)
            .await
            .is_ok()
    );
    db.close().await.unwrap();
    workspace.close().await.unwrap();
}

#[tokio::test]
async fn migration_preserves_pending_intent_without_resolving_it() {
    let (fixture, mut workspace) = Fixture::new().await;
    std::fs::write(fixture.notes.join("Pending.md"), b"before\n").unwrap();
    let document = workspace.adopt_root_document("Pending.md").await.unwrap();
    let proposed = b"pending proposed\n".to_vec();
    let request = SaveContent {
        epoch: workspace.epoch(),
        stream_id: workspace.register_stream().await.unwrap(),
        operation_seq: 1,
        operation_id: Uuid::now_v7().to_string().parse().unwrap(),
        node_id: document.node_id,
        expected_content_rev: document.content_rev,
        content_hash: digest(&proposed),
        bytes: proposed.clone(),
    };
    let mut db = fixture.database().await;
    sqlx::raw_sql("CREATE TRIGGER reject_content_commit BEFORE UPDATE OF content_rev ON nodes BEGIN SELECT RAISE(ABORT,'injected finalization failure'); END;")
        .execute(&mut db).await.unwrap();
    assert_eq!(workspace.save(request.clone()).await, Err(Error::Storage));
    sqlx::raw_sql("DROP TRIGGER reject_content_commit")
        .execute(&mut db)
        .await
        .unwrap();
    db.close().await.unwrap();
    workspace.close().await.unwrap();
    fixture.downgrade_to_v1().await;
    let mut db = fixture.database().await;
    let before = legacy_rows(&mut db).await;
    assert_eq!(before["write_intents"].len(), 1);
    db.close().await.unwrap();
    Workspace::migrate(&fixture.notes, &fixture.state)
        .await
        .unwrap();
    let mut db = fixture.database().await;
    assert_eq!(legacy_rows(&mut db).await, before);
    db.close().await.unwrap();
    assert_eq!(
        std::fs::read(fixture.notes.join("Pending.md")).unwrap(),
        proposed
    );
    let mut reopened = Workspace::open(&fixture.notes, &fixture.state)
        .await
        .unwrap();
    assert!(reopened.ready());
    assert!(matches!(
        reopened.save(request).await.unwrap().outcome,
        Outcome::Saved(_)
    ));
    reopened.close().await.unwrap();
}

#[tokio::test]
async fn failed_migration_rolls_back_auth_schema_and_preserves_colliding_table() {
    let (fixture, workspace) = Fixture::new().await;
    workspace.close().await.unwrap();
    fixture.downgrade_to_v1().await;
    let mut db = fixture.database().await;
    let before = legacy_rows(&mut db).await;
    sqlx::raw_sql("CREATE TABLE pairing_secrets(sentinel TEXT); INSERT INTO pairing_secrets VALUES('preserve synthetic evidence');")
        .execute(&mut db).await.unwrap();
    db.close().await.unwrap();
    assert!(
        Workspace::migrate(&fixture.notes, &fixture.state)
            .await
            .is_err()
    );
    let mut db = fixture.database().await;
    let version: i64 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(&mut db)
        .await
        .unwrap();
    assert_eq!(version, 1);
    assert_eq!(legacy_rows(&mut db).await, before);
    let added: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sqlite_schema WHERE name IN ('principals','devices','auth_workspace_id','auth_one_owner')")
        .fetch_one(&mut db).await.unwrap();
    assert_eq!(added, 0);
    let evidence: String = sqlx::query_scalar("SELECT sentinel FROM pairing_secrets")
        .fetch_one(&mut db)
        .await
        .unwrap();
    assert_eq!(evidence, "preserve synthetic evidence");
    db.close().await.unwrap();
}

#[tokio::test]
async fn migration_requires_workspace_locks_and_preserves_unknown_versions() {
    let (fixture, workspace) = Fixture::new().await;
    assert_eq!(
        Workspace::migrate(&fixture.notes, &fixture.state).await,
        Err(Error::Locked)
    );
    workspace.close().await.unwrap();
    for query in ["PRAGMA user_version=0", "PRAGMA user_version=999"] {
        let mut db = fixture.database().await;
        sqlx::raw_sql(query).execute(&mut db).await.unwrap();
        db.close().await.unwrap();
        let path = fixture.state.join("state.db");
        let before = std::fs::read(&path).unwrap();
        assert!(
            Workspace::migrate(&fixture.notes, &fixture.state)
                .await
                .is_err()
        );
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }
}

#[tokio::test]
async fn invalid_auth_rows_fail_startup_without_rewriting_evidence() {
    for corruption in [
        "UPDATE principals SET workspace_role='reader'",
        "UPDATE devices SET device_id='not-a-uuid'",
        "UPDATE devices SET token_hash=zeroblob(31)",
        "UPDATE devices SET token_hash='0123456789abcdef0123456789abcdef'",
        "UPDATE devices SET idle_expires_at=absolute_expires_at+1",
        "UPDATE pairing_secrets SET consumed_at=expires_at",
    ] {
        let (fixture, mut workspace) = Fixture::new().await;
        let _paired = pair_owner(&mut workspace).await;
        workspace.close().await.unwrap();
        let mut db = fixture.database().await;
        sqlx::raw_sql("PRAGMA ignore_check_constraints=ON")
            .execute(&mut db)
            .await
            .unwrap();
        sqlx::raw_sql(corruption).execute(&mut db).await.unwrap();
        db.close().await.unwrap();
        let path = fixture.state.join("state.db");
        let before = std::fs::read(&path).unwrap();
        assert!(
            Workspace::open(&fixture.notes, &fixture.state)
                .await
                .is_err(),
            "invalid auth state was accepted: {corruption}"
        );
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }
}

#[tokio::test]
async fn well_formed_wrong_secrets_are_rejected_without_consuming_pairing() {
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};

    fn altered_secret(credential: &str) -> String {
        let (binding, encoded) = credential.rsplit_once('.').unwrap();
        let mut secret = URL_SAFE_NO_PAD.decode(encoded).unwrap();
        assert_eq!(secret.len(), 32);
        secret[0] ^= 1;
        let altered = format!("{binding}.{}", URL_SAFE_NO_PAD.encode(secret));
        assert_eq!(altered.len(), credential.len());
        altered
    }

    let (_fixture, mut workspace) = Fixture::new().await;
    let owner = workspace.auth_status().await.unwrap().owner_principal_id;
    let grant = workspace.issue_pairing(owner, NOW).await.unwrap();
    let mut wrong_pairing = pairing_request(&grant);
    wrong_pairing.pairing_secret = altered_secret(&grant.pairing_secret);
    assert!(matches!(
        workspace.pair(wrong_pairing, NOW + 1).await,
        Err(AuthError::Unauthorized)
    ));
    let paired = workspace
        .pair(pairing_request(&grant), NOW + 2)
        .await
        .unwrap();
    let wrong_token = altered_secret(&paired.device_token);
    assert_eq!(
        workspace.authenticate(&wrong_token, NOW + 3).await,
        Err(AuthError::Unauthorized)
    );
    assert!(
        workspace
            .authenticate(&paired.device_token, NOW + 3)
            .await
            .is_ok()
    );
    workspace.close().await.unwrap();
}
