#![cfg(all(target_os = "linux", feature = "test-hooks"))]

use notryx_server_storage::{FileStore, StorageError, StorageStage};

fn fixture() -> (tempfile::TempDir, FileStore) {
    let directory = tempfile::tempdir().unwrap();
    let notes = directory.path().join("notes");
    let state = directory.path().join("state");
    std::fs::create_dir(&notes).unwrap();
    std::fs::create_dir(&state).unwrap();
    let store = FileStore::init(
        &notes,
        &state,
        "550e8400-e29b-41d4-a716-446655440000".parse().unwrap(),
    )
    .unwrap();
    std::fs::write(state.join("state.db"), b"state adapter fixture").unwrap();
    (directory, store)
}

#[test]
fn replacement_failure_at_each_boundary_never_returns_success_or_loses_both_versions() {
    let (directory, store) = fixture();
    let path = directory.path().join("notes/document.md");
    let before_object = store.put_object(b"before").unwrap();
    let proposed_object = store.put_object(b"proposed").unwrap();
    let stages = [
        StorageStage::BeforeTemporaryFileSync,
        StorageStage::AfterTemporaryFileSync,
        StorageStage::BeforeTargetRevalidation,
        StorageStage::AfterTargetRevalidation,
        StorageStage::BeforeRename,
        StorageStage::AfterRename,
        StorageStage::BeforeParentDirectorySync,
        StorageStage::AfterParentDirectorySync,
    ];
    for (index, cut) in stages.into_iter().enumerate() {
        std::fs::write(&path, b"before").unwrap();
        let before = store.observe("document.md").unwrap();
        let mut reached = false;
        let result =
            store.replace_with_hook("document.md", &before.stamp, &proposed_object, |stage| {
                if stage == cut {
                    reached = true;
                    Err(StorageError::Io)
                } else {
                    Ok(())
                }
            });
        assert!(reached, "{cut:?}");
        assert_eq!(result, Err(StorageError::Io), "{cut:?}");
        assert_eq!(
            std::fs::read(&path).unwrap(),
            if index < 5 {
                b"before".as_slice()
            } else {
                b"proposed".as_slice()
            },
            "{cut:?}"
        );
        assert_eq!(store.read_object(&before_object).unwrap(), b"before");
        assert_eq!(store.read_object(&proposed_object).unwrap(), b"proposed");
    }
}

#[test]
fn external_changes_after_temporary_fsync_are_revalidated_before_rename() {
    let (directory, store) = fixture();
    let path = directory.path().join("notes/document.md");
    std::fs::write(&path, b"before").unwrap();
    let before = store.observe("document.md").unwrap();
    let proposed = store.put_object(b"proposed").unwrap();
    let result = store.replace_with_hook("document.md", &before.stamp, &proposed, |stage| {
        if stage == StorageStage::AfterTemporaryFileSync {
            std::fs::write(&path, b"external").unwrap();
        }
        Ok(())
    });
    assert_eq!(result, Err(StorageError::Changed));
    assert_eq!(std::fs::read(&path).unwrap(), b"external");
}

#[test]
fn symlink_swap_after_temporary_fsync_cannot_redirect_the_write() {
    let (directory, store) = fixture();
    let path = directory.path().join("notes/document.md");
    let outside = directory.path().join("outside.md");
    std::fs::write(&outside, b"outside").unwrap();
    std::fs::write(&path, b"before").unwrap();
    let before = store.observe("document.md").unwrap();
    let proposed = store.put_object(b"proposed").unwrap();
    let result = store.replace_with_hook("document.md", &before.stamp, &proposed, |stage| {
        if stage == StorageStage::AfterTemporaryFileSync {
            std::fs::remove_file(&path).unwrap();
            std::os::unix::fs::symlink(&outside, &path).unwrap();
        }
        Ok(())
    });
    assert_eq!(result, Err(StorageError::UnsafeFile));
    assert_eq!(std::fs::read(&outside).unwrap(), b"outside");
}

#[test]
fn a_replaced_temporary_file_is_rejected_before_it_can_replace_the_document() {
    let (directory, store) = fixture();
    let notes = directory.path().join("notes");
    let path = notes.join("document.md");
    std::fs::write(&path, b"before").unwrap();
    let before = store.observe("document.md").unwrap();
    let proposed = store.put_object(b"proposed").unwrap();
    let result = store.replace_with_hook("document.md", &before.stamp, &proposed, |stage| {
        if stage == StorageStage::AfterTemporaryFileSync {
            let temporary = std::fs::read_dir(&notes)
                .unwrap()
                .map(Result::unwrap)
                .find(|entry| entry.file_name().to_string_lossy().starts_with(".mdw-tmp-"))
                .unwrap()
                .path();
            std::fs::remove_file(&temporary).unwrap();
            std::fs::write(&temporary, b"foreign bytes").unwrap();
        }
        Ok(())
    });
    assert_eq!(result, Err(StorageError::Changed));
    assert_eq!(std::fs::read(&path).unwrap(), b"before");
}

#[test]
fn object_publication_propagates_each_durability_failure_and_retry_verifies_bytes() {
    let (_directory, store) = fixture();
    for cut in [
        StorageStage::BeforeObjectFileSync,
        StorageStage::AfterObjectFileSync,
        StorageStage::BeforeObjectPublish,
        StorageStage::AfterObjectPublish,
        StorageStage::BeforeObjectDirectorySync,
        StorageStage::AfterObjectDirectorySync,
    ] {
        let bytes = format!("object fixture {cut:?}").into_bytes();
        let result = store.put_object_with_hook(&bytes, |stage| {
            if stage == cut {
                Err(StorageError::Io)
            } else {
                Ok(())
            }
        });
        assert_eq!(result, Err(StorageError::Io), "{cut:?}");
        let hash = store.put_object(&bytes).unwrap();
        assert_eq!(store.read_object(&hash).unwrap(), bytes);
    }
}

#[test]
fn an_existing_object_is_synced_again_before_recovery_can_finalize() {
    let (_directory, store) = fixture();
    let hash = store.put_object(b"existing object").unwrap();
    for cut in [
        StorageStage::BeforeObjectFileSync,
        StorageStage::BeforeObjectDirectorySync,
    ] {
        let result = store.put_object_with_hook(b"existing object", |stage| {
            if stage == cut {
                Err(StorageError::Io)
            } else {
                Ok(())
            }
        });
        assert_eq!(result, Err(StorageError::Io));
        assert_eq!(store.read_object(&hash).unwrap(), b"existing object");
    }
}

#[test]
fn recovery_requires_file_and_parent_sync_and_revalidates_observed_state() {
    let (directory, store) = fixture();
    let path = directory.path().join("notes/document.md");
    std::fs::write(&path, b"before").unwrap();
    let before = store.observe("document.md").unwrap();
    for cut in [
        StorageStage::BeforeRecoveryFileSync,
        StorageStage::AfterRecoveryFileSync,
        StorageStage::BeforeRecoveryDirectorySync,
        StorageStage::AfterRecoveryDirectorySync,
    ] {
        let result = store.sync_observed_with_hook("document.md", &before.stamp, |stage| {
            if stage == cut {
                Err(StorageError::Io)
            } else {
                Ok(())
            }
        });
        assert_eq!(result, Err(StorageError::Io));
        assert_eq!(store.observe("document.md").unwrap().stamp, before.stamp);
    }
    assert_eq!(
        store.sync_observed_with_hook("document.md", &before.stamp, |stage| {
            if stage == StorageStage::AfterRecoveryFileSync {
                std::fs::write(&path, b"external").unwrap();
            }
            Ok(())
        }),
        Err(StorageError::Changed)
    );
    assert_eq!(std::fs::read(&path).unwrap(), b"external");
}
