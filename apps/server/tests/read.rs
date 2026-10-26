use notryx_protocol::{CONTRACT, NodeKind, StrongEtag};
use notryx_server::{Document, Error, Outcome, SaveContent, Workspace};
use sha2::{Digest, Sha256};
use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
use std::path::PathBuf;
use uuid::Uuid;

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
            &SqliteConnectOptions::new().filename(self.state.join("state.db")),
        )
        .await
        .unwrap()
    }
}

async fn save_request(workspace: &mut Workspace, document: &Document, bytes: &[u8]) -> SaveContent {
    SaveContent {
        epoch: workspace.epoch(),
        stream_id: workspace.register_stream().await.unwrap(),
        operation_seq: 1,
        operation_id: Uuid::now_v7().to_string().parse().unwrap(),
        node_id: document.node_id,
        expected_content_rev: document.content_rev.clone(),
        content_hash: Sha256::digest(bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect(),
        bytes: bytes.to_vec(),
    }
}

#[tokio::test]
async fn server_info_tracks_commits_while_metadata_etags_ignore_content_and_children() {
    let (fixture, mut workspace) = Fixture::new().await;
    let initial_info = workspace.server_info().await.unwrap();
    assert_eq!(initial_info.protocol, CONTRACT);
    assert_eq!(initial_info.workspace_id, workspace.workspace_id());
    assert_eq!(initial_info.workspace_epoch, workspace.epoch());
    assert_eq!(initial_info.head_seq.get(), 0);
    assert_eq!(initial_info.journal_floor_seq.get(), 0);
    assert_eq!(initial_info.max_document_bytes, 5 * 1024 * 1024);
    assert!(initial_info.ready);
    let root = workspace.node_metadata(initial_info.root_id).await.unwrap();
    assert_eq!(root.node_id, initial_info.root_id);
    assert_eq!(root.parent_id, None);
    assert!(root.name.is_empty());
    assert_eq!(root.kind, NodeKind::Folder);
    assert!(!root.deleted);
    assert_eq!(
        root.meta_etag,
        StrongEtag::from_meta_revision(&root.meta_rev)
            .unwrap()
            .as_str()
    );
    let root_representation = serde_json::to_value(&root).unwrap();

    let original = b"\xef\xbb\xbf# Original\r\n<!-- synthetic fixture -->\r\n";
    std::fs::write(fixture.notes.join("Document.md"), original).unwrap();
    let document = workspace.adopt_root_document("Document.md").await.unwrap();
    assert_eq!(workspace.server_info().await.unwrap().head_seq.get(), 1);
    let file = workspace.node_metadata(document.node_id).await.unwrap();
    assert_eq!(file.node_id, document.node_id);
    assert_eq!(file.parent_id, Some(initial_info.root_id));
    assert_eq!(file.name, "Document.md");
    assert_eq!(file.kind, NodeKind::File);
    assert!(!file.deleted);
    assert_eq!(file.meta_rev, document.meta_rev);
    assert_eq!(
        file.meta_etag,
        StrongEtag::from_meta_revision(&document.meta_rev)
            .unwrap()
            .as_str()
    );
    let file_representation = serde_json::to_value(&file).unwrap();
    assert_eq!(
        serde_json::to_value(workspace.node_metadata(initial_info.root_id).await.unwrap()).unwrap(),
        root_representation
    );
    let unknown = Uuid::now_v7().to_string().parse().unwrap();
    assert!(matches!(
        workspace.node_metadata(unknown).await,
        Err(Error::NotFound)
    ));
    assert_eq!(workspace.read(unknown).await, Err(Error::NotFound));
    assert_eq!(workspace.read(document.node_id).await.unwrap().1, original);

    let replacement = b"\xef\xbb\xbf# A different body and size\r\n<!-- unchanged syntax -->\r\n";
    let request = save_request(&mut workspace, &document, replacement).await;
    assert!(matches!(
        workspace.save(request).await.unwrap().outcome,
        Outcome::Saved(_)
    ));
    let current_info = workspace.server_info().await.unwrap();
    assert_eq!(current_info.root_id, initial_info.root_id);
    assert_eq!(current_info.head_seq.get(), 2);
    assert_eq!(current_info.journal_floor_seq.get(), 0);
    assert!(current_info.ready);
    assert_eq!(
        serde_json::to_value(workspace.node_metadata(document.node_id).await.unwrap()).unwrap(),
        file_representation
    );
    assert_eq!(
        serde_json::to_value(workspace.node_metadata(initial_info.root_id).await.unwrap()).unwrap(),
        root_representation
    );
    let (saved, bytes) = workspace.read(document.node_id).await.unwrap();
    assert_ne!(saved.content_rev, document.content_rev);
    assert_eq!(saved.meta_rev, document.meta_rev);
    assert_eq!(bytes, replacement);
    workspace.close().await.unwrap();
}

#[tokio::test]
async fn recovery_conflict_exposes_last_committed_metadata_and_bytes_with_readiness_false() {
    let (fixture, mut workspace) = Fixture::new().await;
    let original = b"\xef\xbb\xbf# Last committed\r\n<!-- keep -->\r\n";
    let path = fixture.notes.join("Document.md");
    std::fs::write(&path, original).unwrap();
    let document = workspace.adopt_root_document("Document.md").await.unwrap();
    let committed_info = workspace.server_info().await.unwrap();
    let root_representation = serde_json::to_value(
        workspace
            .node_metadata(committed_info.root_id)
            .await
            .unwrap(),
    )
    .unwrap();
    let file_representation =
        serde_json::to_value(workspace.node_metadata(document.node_id).await.unwrap()).unwrap();
    let mut db = fixture.database().await;
    sqlx::raw_sql("CREATE TRIGGER reject_content_commit BEFORE UPDATE OF content_rev ON nodes BEGIN SELECT RAISE(ABORT,'injected finalization failure'); END;")
        .execute(&mut db).await.unwrap();
    let request = save_request(&mut workspace, &document, b"proposed content\n").await;
    assert_eq!(workspace.save(request.clone()).await, Err(Error::Storage));
    sqlx::raw_sql("DROP TRIGGER reject_content_commit")
        .execute(&mut db)
        .await
        .unwrap();
    db.close().await.unwrap();
    workspace.close().await.unwrap();
    let third = b"third content: preserve outside the committed view\n";
    std::fs::write(&path, third).unwrap();

    let mut reopened = Workspace::open(&fixture.notes, &fixture.state)
        .await
        .unwrap();
    assert!(!reopened.ready());
    let info = reopened.server_info().await.unwrap();
    assert!(!info.ready);
    assert_eq!(info.workspace_id, committed_info.workspace_id);
    assert_eq!(info.workspace_epoch, committed_info.workspace_epoch);
    assert_eq!(info.root_id, committed_info.root_id);
    assert_eq!(info.head_seq, committed_info.head_seq);
    assert_eq!(
        serde_json::to_value(reopened.node_metadata(info.root_id).await.unwrap()).unwrap(),
        root_representation
    );
    assert_eq!(
        serde_json::to_value(reopened.node_metadata(document.node_id).await.unwrap()).unwrap(),
        file_representation
    );
    let (read_document, bytes) = reopened.read(document.node_id).await.unwrap();
    assert_eq!(read_document, document);
    assert_eq!(bytes, original);
    assert_eq!(std::fs::read(&path).unwrap(), third);
    let mut db = fixture.database().await;
    let conflict: i64 =
        sqlx::query_scalar("SELECT recovery_conflict FROM write_intents WHERE operation_id=?")
            .bind(request.operation_id.to_string())
            .fetch_one(&mut db)
            .await
            .unwrap();
    assert_eq!(conflict, 1);
    db.close().await.unwrap();
    reopened.close().await.unwrap();
}
