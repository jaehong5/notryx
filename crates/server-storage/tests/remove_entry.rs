#![cfg(target_os = "linux")]

use notryx_server_storage::{EntryStamp, FileStore, ObservedEntry, StorageError};
use std::os::unix::fs::symlink;

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

fn stamp(store: &FileStore, path: &str) -> EntryStamp {
    match store.observe_entry(path).unwrap() {
        ObservedEntry::File(file) => EntryStamp::File(file.stamp),
        ObservedEntry::Folder { stamp, .. } => EntryStamp::Folder(stamp),
        ObservedEntry::Absent => panic!("fixture entry missing"),
    }
}

#[cfg(feature = "test-hooks")]
fn sync_before(store: &FileStore, path: &str, expected: &EntryStamp) {
    match expected {
        EntryStamp::File(stamp) => store.sync_observed(path, stamp).unwrap(),
        EntryStamp::Folder(stamp) => store.sync_folder(path, stamp).unwrap(),
    }
}

#[test]
fn removal_preserves_retained_bytes_and_restoration_never_overwrites() {
    let (directory, store) = fixture();
    let notes = directory.path().join("notes");
    store.create_folder("Parent").unwrap();
    let bytes = b"\xef\xbb\xbf# Original\r\nfrontmatter and unknown syntax\r";
    let hash = store.put_object(bytes).unwrap();
    store.create_file("Parent/source.md", &hash).unwrap();
    let before = stamp(&store, "Parent/source.md");
    store.remove_entry("Parent/source.md", &before).unwrap();
    assert_eq!(
        store.observe_entry("Parent/source.md").unwrap(),
        ObservedEntry::Absent
    );
    assert_eq!(store.read_object(&hash).unwrap(), bytes);
    store.sync_absent("Parent/source.md").unwrap();
    store.create_file("Parent/restored.md", &hash).unwrap();
    assert_eq!(store.observe("Parent/restored.md").unwrap().bytes, bytes);
    std::fs::write(notes.join("Parent/source.md"), b"new occupant").unwrap();
    assert_eq!(
        store.create_file("Parent/source.md", &hash),
        Err(StorageError::AlreadyExists)
    );
    assert_eq!(
        std::fs::read(notes.join("Parent/source.md")).unwrap(),
        b"new occupant"
    );

    store.create_folder("Empty").unwrap();
    let folder = stamp(&store, "Empty");
    store.remove_entry("Empty", &folder).unwrap();
    store.sync_absent("Empty").unwrap();
    store.create_folder("RestoredEmpty").unwrap();
    assert!(matches!(
        store.observe_entry("RestoredEmpty").unwrap(),
        ObservedEntry::Folder { empty: true, .. }
    ));
}

#[test]
fn folders_with_hidden_excluded_or_untracked_children_are_never_removed() {
    let (directory, store) = fixture();
    let notes = directory.path().join("notes");
    for (index, child) in ["file.md", ".hidden", ".git", ".mdw-tmp-leftover"]
        .into_iter()
        .enumerate()
    {
        let folder = format!("Folder{index}");
        store.create_folder(&folder).unwrap();
        std::fs::write(notes.join(&folder).join(child), b"must stay").unwrap();
        let current = stamp(&store, &folder);
        assert_eq!(
            store.remove_entry(&folder, &current),
            Err(StorageError::NotEmpty)
        );
        assert_eq!(
            std::fs::read(notes.join(&folder).join(child)).unwrap(),
            b"must stay"
        );
    }
}

#[test]
fn deletion_rejects_root_reserved_paths_links_and_changed_entries() {
    let (directory, store) = fixture();
    let notes = directory.path().join("notes");
    let outside = directory.path().join("outside");
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("protected.md"), b"outside").unwrap();
    let hash = store.put_object(b"before").unwrap();
    store.create_file("source.md", &hash).unwrap();
    let before = stamp(&store, "source.md");
    for path in [
        "",
        ".",
        "..",
        "/",
        "../outside/protected.md",
        ".git/file.md",
        ".mdw-workspace.json",
        "folder//file.md",
    ] {
        assert_eq!(
            store.remove_entry(path, &before),
            Err(StorageError::InvalidName),
            "{path}"
        );
    }
    symlink(&outside, notes.join("linked")).unwrap();
    symlink(outside.join("protected.md"), notes.join("symlink.md")).unwrap();
    std::fs::hard_link(outside.join("protected.md"), notes.join("hard.md")).unwrap();
    for path in ["linked/protected.md", "symlink.md", "hard.md"] {
        assert_eq!(
            store.remove_entry(path, &before),
            Err(StorageError::UnsafeFile),
            "{path}"
        );
    }
    assert_eq!(
        store.remove_entry("missing.md", &before),
        Err(StorageError::NotFound)
    );
    std::fs::write(notes.join("source.md"), b"external edit").unwrap();
    assert_eq!(
        store.remove_entry("source.md", &before),
        Err(StorageError::Changed)
    );
    assert_eq!(
        std::fs::read(notes.join("source.md")).unwrap(),
        b"external edit"
    );
    assert_eq!(
        std::fs::read(outside.join("protected.md")).unwrap(),
        b"outside"
    );
    assert!(notes.is_dir());
}

#[cfg(feature = "test-hooks")]
mod faults {
    use super::*;
    use notryx_server_storage::StorageStage;

    #[test]
    fn deletion_failures_preserve_before_or_durable_absence_and_retained_objects() {
        let (_directory, store) = fixture();
        store.create_folder("Parent").unwrap();
        let bytes = b"whole original\r\n";
        let hash = store.put_object(bytes).unwrap();
        let stages = [
            StorageStage::BeforeTargetRevalidation,
            StorageStage::AfterTargetRevalidation,
            StorageStage::BeforeUnlink,
            StorageStage::AfterUnlink,
            StorageStage::BeforeParentDirectorySync,
            StorageStage::AfterParentDirectorySync,
        ];
        for folder in [false, true] {
            for (index, cut) in stages.into_iter().enumerate() {
                let path = format!("Parent/{folder}-{index}");
                if folder {
                    store.create_folder(&path).unwrap();
                } else {
                    store.create_file(&path, &hash).unwrap();
                }
                let before = stamp(&store, &path);
                let mut reached = false;
                assert_eq!(
                    store.remove_entry_with_hook(&path, &before, |stage| {
                        if stage == cut {
                            reached = true;
                            Err(StorageError::Io)
                        } else {
                            Ok(())
                        }
                    }),
                    Err(StorageError::Io)
                );
                assert!(reached, "{folder} {cut:?}");
                if index < 3 {
                    assert_eq!(stamp(&store, &path), before);
                    sync_before(&store, &path, &before);
                } else {
                    assert_eq!(store.observe_entry(&path).unwrap(), ObservedEntry::Absent);
                    store.sync_absent(&path).unwrap();
                }
                assert_eq!(store.read_object(&hash).unwrap(), bytes);
            }
        }
    }

    #[test]
    fn late_children_and_source_or_parent_replacements_are_preserved() {
        for action in ["late_child", "folder_source", "file_source", "parent"] {
            let (directory, store) = fixture();
            let notes = directory.path().join("notes");
            store.create_folder("Parent").unwrap();
            let hash = store.put_object(b"original").unwrap();
            let folder = action == "late_child" || action == "folder_source";
            if folder {
                store.create_folder("Parent/target").unwrap();
            } else {
                store.create_file("Parent/target", &hash).unwrap();
            }
            let before = stamp(&store, "Parent/target");
            let mut reached = false;
            let result = store.remove_entry_with_hook("Parent/target", &before, |stage| {
                if stage == StorageStage::BeforeUnlink {
                    reached = true;
                    match action {
                        "late_child" => {
                            std::fs::write(notes.join("Parent/target/.hidden"), b"late child")
                                .unwrap()
                        }
                        "folder_source" => {
                            std::fs::rename(
                                notes.join("Parent/target"),
                                notes.join("Parent/original"),
                            )
                            .unwrap();
                            std::fs::create_dir(notes.join("Parent/target")).unwrap();
                        }
                        "file_source" => {
                            std::fs::rename(
                                notes.join("Parent/target"),
                                notes.join("Parent/original"),
                            )
                            .unwrap();
                            std::fs::write(notes.join("Parent/target"), b"replacement").unwrap();
                        }
                        "parent" => {
                            std::fs::rename(notes.join("Parent"), notes.join("OriginalParent"))
                                .unwrap();
                            std::fs::create_dir(notes.join("Parent")).unwrap();
                        }
                        _ => unreachable!(),
                    }
                }
                Ok(())
            });
            assert!(reached);
            assert_eq!(
                result,
                Err(if action == "late_child" {
                    StorageError::NotEmpty
                } else {
                    StorageError::Changed
                }),
                "{action}"
            );
            match action {
                "late_child" => assert_eq!(
                    std::fs::read(notes.join("Parent/target/.hidden")).unwrap(),
                    b"late child"
                ),
                "folder_source" => {
                    assert!(notes.join("Parent/original").is_dir());
                    assert!(notes.join("Parent/target").is_dir());
                }
                "file_source" => {
                    assert_eq!(
                        std::fs::read(notes.join("Parent/original")).unwrap(),
                        b"original"
                    );
                    assert_eq!(
                        std::fs::read(notes.join("Parent/target")).unwrap(),
                        b"replacement"
                    );
                }
                "parent" => assert_eq!(
                    std::fs::read(notes.join("OriginalParent/target")).unwrap(),
                    b"original"
                ),
                _ => unreachable!(),
            }
        }
    }

    #[test]
    fn external_recreation_after_unlink_is_preserved_and_not_reported_as_success() {
        let (directory, store) = fixture();
        let notes = directory.path().join("notes");
        let hash = store.put_object(b"original").unwrap();
        store.create_file("target.md", &hash).unwrap();
        let before = stamp(&store, "target.md");
        let result = store.remove_entry_with_hook("target.md", &before, |stage| {
            if stage == StorageStage::AfterUnlink {
                std::fs::write(notes.join("target.md"), b"external recreation").unwrap();
            }
            Ok(())
        });
        assert_eq!(result, Err(StorageError::Changed));
        assert_eq!(
            store.observe("target.md").unwrap().bytes,
            b"external recreation"
        );
        assert_eq!(store.read_object(&hash).unwrap(), b"original");
        assert_eq!(store.sync_absent("target.md"), Err(StorageError::Changed));
    }

    #[test]
    fn absence_recovery_propagates_parent_sync_failure_and_can_retry() {
        let (_directory, store) = fixture();
        let hash = store.put_object(b"original").unwrap();
        store.create_file("target.md", &hash).unwrap();
        let before = stamp(&store, "target.md");
        store.remove_entry("target.md", &before).unwrap();
        for cut in [
            StorageStage::BeforeRecoveryDirectorySync,
            StorageStage::AfterRecoveryDirectorySync,
        ] {
            let mut reached = false;
            assert_eq!(
                store.sync_absent_with_hook("target.md", |stage| {
                    if stage == cut {
                        reached = true;
                        Err(StorageError::Io)
                    } else {
                        Ok(())
                    }
                }),
                Err(StorageError::Io)
            );
            assert!(reached);
            assert_eq!(
                store.observe_entry("target.md").unwrap(),
                ObservedEntry::Absent
            );
            assert_eq!(store.read_object(&hash).unwrap(), b"original");
        }
        store.sync_absent("target.md").unwrap();
    }
}
