use notryx_server_storage::{FileStore, MAX_DOCUMENT_BYTES, MAX_FILE_BYTES, StorageError};
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use uuid::Uuid;

const ABC_HASH: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

fn fixture() -> (tempfile::TempDir, FileStore) {
    let dir = tempfile::tempdir().unwrap();
    let notes = dir.path().join("notes");
    let state = dir.path().join("state");
    std::fs::create_dir(&notes).unwrap();
    std::fs::create_dir(&state).unwrap();
    let store =
        FileStore::init(&notes, &state, Uuid::new_v4().to_string().parse().unwrap()).unwrap();
    std::fs::write(state.join("state.db"), b"storage fixture").unwrap();
    (dir, store)
}

#[test]
fn upload_stream_seals_then_coordinator_publishes_and_attaches_exact_bytes() {
    let (dir, store) = fixture();
    let id = Uuid::new_v4();
    let mut writer = store.begin_upload(id, 3).unwrap();
    let alive = writer.liveness();
    writer.write_chunk(b"ab").unwrap();
    writer.write_chunk(b"c").unwrap();
    let sealed = writer.seal().unwrap();
    assert_eq!(
        (sealed.id(), sealed.hash(), sealed.size()),
        (id, ABC_HASH, 3)
    );
    assert!(alive.upgrade().is_some());
    assert!(
        !dir.path()
            .join(format!("state/objects/sha256/{ABC_HASH}"))
            .exists()
    );
    let stored = store.commit_upload(sealed).unwrap();
    assert!(alive.upgrade().is_none());
    assert_eq!(store.list_upload_staging().unwrap().len(), 1);
    let hash = store.prepare_upload_object(&stored).unwrap();
    assert_eq!(hash, ABC_HASH);
    store.create_file("attachment.bin", &hash).unwrap();
    assert_eq!(
        std::fs::read(dir.path().join("notes/attachment.bin")).unwrap(),
        b"abc"
    );
    store.remove_upload(&stored).unwrap();
    assert!(store.list_upload_staging().unwrap().is_empty());
    assert_eq!(store.read_object(&hash).unwrap(), b"abc");
}

#[test]
fn cancelled_and_sealed_uploads_hold_store_locks_until_cleanup_finishes() {
    for seal in [false, true] {
        let (dir, store) = fixture();
        let writer = store.begin_upload(Uuid::new_v4(), 0).unwrap();
        let alive = writer.liveness();
        let owned: Box<dyn Send> = if seal {
            Box::new(writer.seal().unwrap())
        } else {
            Box::new(writer)
        };
        drop(store);
        let notes = dir.path().join("notes");
        let state = dir.path().join("state");
        assert!(matches!(
            FileStore::open(&notes, &state),
            Err(StorageError::Locked)
        ));
        drop(owned);
        assert!(alive.upgrade().is_none());
        let reopened = FileStore::open(&notes, &state).unwrap();
        assert!(reopened.list_upload_staging().unwrap().is_empty());
    }
}

#[test]
fn reserved_size_is_enforced_and_failed_stream_cannot_be_sealed() {
    let (_dir, store) = fixture();
    let mut writer = store.begin_upload(Uuid::new_v4(), 2).unwrap();
    writer.write_chunk(b"ab").unwrap();
    assert_eq!(writer.write_chunk(b"c"), Err(StorageError::TooLarge));
    assert!(matches!(writer.seal(), Err(StorageError::TooLarge)));
    assert!(store.list_upload_staging().unwrap().is_empty());
    assert!(matches!(
        store.begin_upload(Uuid::new_v4(), 25 * 1024 * 1024 + 1),
        Err(StorageError::TooLarge)
    ));
}

#[test]
fn raw_files_allow_25_mib_while_document_editing_limit_stays_5_mib() {
    assert_eq!(MAX_DOCUMENT_BYTES, 5 * 1024 * 1024);
    assert_eq!(MAX_FILE_BYTES, 25 * 1024 * 1024);
    let (dir, store) = fixture();
    let mut writer = store
        .begin_upload(Uuid::new_v4(), MAX_FILE_BYTES as u64)
        .unwrap();
    for _ in 0..400 {
        writer.write_chunk(&[0xa5; 65536]).unwrap();
    }
    let sealed = writer.seal().unwrap();
    assert_eq!(sealed.size(), MAX_FILE_BYTES as u64);
    let stored = store.commit_upload(sealed).unwrap();
    let hash = store.prepare_upload_object(&stored).unwrap();
    let stamp = store.create_file("large.bin", &hash).unwrap();
    assert_eq!(stamp.size, MAX_FILE_BYTES as u64);
    let small_hash = store.put_object(b"abc").unwrap();
    store.replace("large.bin", &stamp, &small_hash).unwrap();
    assert_eq!(
        std::fs::read(dir.path().join("notes/large.bin")).unwrap(),
        b"abc"
    );
    let small = store.observe("large.bin").unwrap().stamp;
    assert_eq!(
        store.replace("large.bin", &small, &hash).unwrap().size,
        MAX_FILE_BYTES as u64
    );
}

#[test]
fn cancelled_upload_never_removes_replacement_inode() {
    let (dir, store) = fixture();
    let id = Uuid::new_v4();
    let writer = store.begin_upload(id, 3).unwrap();
    let path = dir.path().join(format!("state/staging/upload-{id}.part"));
    let moved = dir.path().join("original-upload");
    std::fs::rename(&path, &moved).unwrap();
    std::fs::write(&path, b"replacement").unwrap();
    drop(writer);
    assert_eq!(std::fs::read(&path).unwrap(), b"replacement");
    assert!(moved.exists());
}

#[test]
fn sealed_upload_cannot_be_published_by_another_store() {
    let (_dir, store) = fixture();
    let (_other_dir, other) = fixture();
    let sealed = store
        .begin_upload(Uuid::new_v4(), 0)
        .unwrap()
        .seal()
        .unwrap();
    assert!(matches!(
        other.commit_upload(sealed),
        Err(StorageError::Changed)
    ));
    assert!(store.list_upload_staging().unwrap().is_empty());
    assert!(other.list_upload_staging().unwrap().is_empty());
}

#[test]
fn opening_legacy_store_does_not_create_staging_until_explicit_preparation() {
    let (dir, store) = fixture();
    drop(store);
    let staging = dir.path().join("state/staging");
    if staging.exists() {
        std::fs::remove_dir(&staging).unwrap();
    }
    let store = FileStore::open(dir.path().join("notes"), dir.path().join("state")).unwrap();
    assert!(!staging.exists());
    assert!(matches!(
        store.begin_upload(Uuid::new_v4(), 3),
        Err(StorageError::NotFound)
    ));
    store.prepare_upload_staging().unwrap();
    assert_eq!(staging.metadata().unwrap().mode() & 0o777, 0o700);
}

#[test]
fn orphan_cleanup_preserves_unknown_names_and_replaced_inodes() {
    let (dir, store) = fixture();
    let staging = dir.path().join("state/staging");
    let id = Uuid::new_v4();
    let known = staging.join(format!("upload-{id}.part"));
    std::fs::write(&known, b"orphan").unwrap();
    std::fs::write(staging.join("unrecognized"), b"preserve").unwrap();
    let entries = store.list_upload_staging().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].id, id);
    assert!(!entries[0].complete);
    std::fs::rename(&known, staging.join("moved")).unwrap();
    std::fs::write(&known, b"replacement").unwrap();
    assert_eq!(
        store.remove_staged_entry(&entries[0]),
        Err(StorageError::Changed)
    );
    let current = store.list_upload_staging().unwrap().remove(0);
    store.remove_staged_entry(&current).unwrap();
    assert_eq!(
        std::fs::read(staging.join("unrecognized")).unwrap(),
        b"preserve"
    );
}

#[test]
fn canonical_staging_links_and_modified_sealed_bytes_are_rejected() {
    let (dir, store) = fixture();
    let id = Uuid::new_v4();
    let mut writer = store.begin_upload(id, 3).unwrap();
    writer.write_chunk(b"abc").unwrap();
    let sealed = writer.seal().unwrap();
    let path = dir.path().join(format!("state/staging/upload-{id}.part"));
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    std::fs::write(&path, b"def").unwrap();
    assert!(matches!(
        store.commit_upload(sealed),
        Err(StorageError::Changed)
    ));
    let external = dir.path().join("external");
    std::fs::write(&external, b"preserve").unwrap();
    symlink(&external, &path).unwrap();
    assert!(matches!(
        store.list_upload_staging(),
        Err(StorageError::UnsafeFile)
    ));
    assert_eq!(std::fs::read(&external).unwrap(), b"preserve");
}

#[test]
fn corrupted_completed_staging_is_preserved_and_never_copied_to_cas() {
    let (dir, store) = fixture();
    let id = Uuid::new_v4();
    let mut writer = store.begin_upload(id, 3).unwrap();
    writer.write_chunk(b"abc").unwrap();
    let stored = store.commit_upload(writer.seal().unwrap()).unwrap();
    let path = dir.path().join(format!("state/staging/upload-{id}.ready"));
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    std::fs::write(&path, b"def").unwrap();
    assert_eq!(
        store.prepare_upload_object(&stored),
        Err(StorageError::ChecksumMismatch)
    );
    assert_eq!(
        store.remove_upload(&stored),
        Err(StorageError::ChecksumMismatch)
    );
    assert_eq!(std::fs::read(&path).unwrap(), b"def");
    assert!(
        !dir.path()
            .join(format!("state/objects/sha256/{ABC_HASH}"))
            .exists()
    );
}

#[test]
fn concurrent_uploads_keep_independent_liveness_and_no_replace_publication() {
    let (_dir, store) = fixture();
    let id = Uuid::new_v4();
    let first = store.begin_upload(id, 0).unwrap();
    let first_alive = first.liveness();
    let second = store.begin_upload(Uuid::new_v4(), 0).unwrap();
    let second_alive = second.liveness();
    assert!(matches!(
        store.begin_upload(id, 0),
        Err(StorageError::AlreadyExists)
    ));
    let stored = store.commit_upload(first.seal().unwrap()).unwrap();
    assert!(first_alive.upgrade().is_none());
    assert!(second_alive.upgrade().is_some());
    let duplicate = store.begin_upload(id, 0).unwrap().seal().unwrap();
    assert_eq!(
        store.commit_upload(duplicate),
        Err(StorageError::AlreadyExists)
    );
    assert_eq!(store.prepare_upload_object(&stored).unwrap(), stored.hash);
    drop(second);
    assert!(second_alive.upgrade().is_none());
    assert_eq!(store.list_upload_staging().unwrap().len(), 1);
}

#[cfg(feature = "test-hooks")]
#[test]
fn staging_publication_failure_never_reports_success_and_releases_owned_handles() {
    use notryx_server_storage::StorageStage;
    let (_dir, store) = fixture();
    for cut in [
        StorageStage::BeforeUploadPublish,
        StorageStage::AfterUploadPublish,
        StorageStage::BeforeUploadFileSync,
        StorageStage::AfterUploadFileSync,
        StorageStage::BeforeUploadDirectorySync,
        StorageStage::AfterUploadDirectorySync,
    ] {
        let mut writer = store.begin_upload(Uuid::new_v4(), 3).unwrap();
        writer.write_chunk(b"abc").unwrap();
        let alive = writer.liveness();
        let mut reached = false;
        let result = store.commit_upload_with_hook(writer.seal().unwrap(), |stage| {
            if stage == cut {
                reached = true;
                Err(StorageError::NoSpace)
            } else {
                Ok(())
            }
        });
        assert!(reached, "{cut:?}");
        assert_eq!(result, Err(StorageError::NoSpace));
        assert!(alive.upgrade().is_none());
        assert!(store.list_upload_staging().unwrap().is_empty());
    }
}

#[cfg(feature = "test-hooks")]
#[test]
fn failed_staging_cas_copy_preserves_ready_upload_for_retry() {
    use notryx_server_storage::StorageStage;
    for cut in [
        StorageStage::BeforeObjectFileSync,
        StorageStage::AfterObjectFileSync,
        StorageStage::BeforeObjectPublish,
        StorageStage::AfterObjectPublish,
        StorageStage::BeforeObjectDirectorySync,
        StorageStage::AfterObjectDirectorySync,
    ] {
        let (_dir, store) = fixture();
        let mut writer = store.begin_upload(Uuid::new_v4(), 3).unwrap();
        writer.write_chunk(b"abc").unwrap();
        let stored = store.commit_upload(writer.seal().unwrap()).unwrap();
        let mut reached = false;
        let result = store.prepare_upload_object_with_hook(&stored, |stage| {
            if stage == cut {
                reached = true;
                Err(StorageError::NoSpace)
            } else {
                Ok(())
            }
        });
        assert!(reached, "{cut:?}");
        assert_eq!(result, Err(StorageError::NoSpace));
        assert_eq!(store.list_upload_staging().unwrap().len(), 1);
        assert_eq!(store.prepare_upload_object(&stored).unwrap(), ABC_HASH);
        assert_eq!(store.read_object(ABC_HASH).unwrap(), b"abc");
    }
}

#[test]
fn replaced_staging_directory_cannot_redirect_publication_or_drop_cleanup() {
    let (dir, store) = fixture();
    let id = Uuid::new_v4();
    let writer = store.begin_upload(id, 0).unwrap();
    let staging = dir.path().join("state/staging");
    let original = dir.path().join("original-staging");
    std::fs::rename(&staging, &original).unwrap();
    std::fs::create_dir(&staging).unwrap();
    std::fs::set_permissions(&staging, std::fs::Permissions::from_mode(0o700)).unwrap();
    let replacement = staging.join(format!("upload-{id}.part"));
    std::fs::write(&replacement, b"preserve").unwrap();
    assert_eq!(
        store.commit_upload(writer.seal().unwrap()),
        Err(StorageError::Changed)
    );
    assert_eq!(std::fs::read(&replacement).unwrap(), b"preserve");
    assert!(std::fs::read_dir(original).unwrap().next().is_none());
}

#[test]
fn startup_validation_checks_staging_bytes_without_creating_cas_objects() {
    let (dir, store) = fixture();
    let id = Uuid::new_v4();
    let mut writer = store.begin_upload(id, 3).unwrap();
    writer.write_chunk(b"abc").unwrap();
    let stored = store.commit_upload(writer.seal().unwrap()).unwrap();
    drop(store);
    let reopened = FileStore::open(dir.path().join("notes"), dir.path().join("state")).unwrap();
    reopened.validate_upload(&stored).unwrap();
    assert!(
        std::fs::read_dir(dir.path().join("state/objects/sha256"))
            .unwrap()
            .next()
            .is_none()
    );
    let path = dir.path().join(format!("state/staging/upload-{id}.ready"));
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    std::fs::write(&path, b"def").unwrap();
    assert_eq!(
        reopened.validate_upload(&stored),
        Err(StorageError::ChecksumMismatch)
    );
    assert_eq!(std::fs::read(path).unwrap(), b"def");
}
