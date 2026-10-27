#![cfg(target_os = "linux")]

use notryx_server_storage::{FileStore, ObservedEntry, StorageError};
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

#[test]
fn create_nested_folders_and_files_preserves_bytes_and_existing_names() {
    let (directory, store) = fixture();
    store.create_folder("한글").unwrap();
    store.create_folder("한글/Folder").unwrap();
    let bytes = b"\xef\xbb\xbffrontmatter\r\nuntouched\r";
    let hash = store.put_object(bytes).unwrap();
    let stamp = store.create_file("한글/Folder/Note.md", &hash).unwrap();
    assert_eq!(store.observe("한글/Folder/Note.md").unwrap().bytes, bytes);
    assert_eq!(
        store.create_file("한글/Folder/Note.md", &hash),
        Err(StorageError::AlreadyExists)
    );
    assert_eq!(
        store.create_folder("한글/Folder/Note.md"),
        Err(StorageError::AlreadyExists)
    );
    assert_eq!(
        store.create_file("한글/Folder", &hash),
        Err(StorageError::AlreadyExists)
    );
    store.sync_observed("한글/Folder/Note.md", &stamp).unwrap();
    let replacement = store.put_object(b"replacement\r\n").unwrap();
    store
        .replace("한글/Folder/Note.md", &stamp, &replacement)
        .unwrap();
    assert_eq!(
        std::fs::read(directory.path().join("notes/한글/Folder/Note.md")).unwrap(),
        b"replacement\r\n"
    );
}

#[test]
fn availability_checks_actual_casefolded_and_normalized_entries() {
    let (directory, store) = fixture();
    let notes = directory.path().join("notes");
    std::fs::create_dir(notes.join("Parent")).unwrap();
    std::fs::write(notes.join("Parent/Existing.MD"), b"outside writer").unwrap();
    std::fs::write(notes.join("Parent/e\u{301}.md"), b"external spelling").unwrap();
    assert_eq!(store.ensure_available("Parent/new.md"), Ok(()));
    assert_eq!(
        store.ensure_available("Parent/existing.md"),
        Err(StorageError::AlreadyExists)
    );
    assert_eq!(
        store.ensure_available("Parent/é.md"),
        Err(StorageError::AlreadyExists)
    );
    assert_eq!(
        std::fs::read(notes.join("Parent/Existing.MD")).unwrap(),
        b"outside writer"
    );
}

#[test]
fn recovery_observes_absence_files_and_empty_folders_without_mutating_them() {
    let (directory, store) = fixture();
    assert_eq!(store.observe_entry("missing"), Ok(ObservedEntry::Absent));
    store.sync_absent("missing").unwrap();
    let folder = store.create_folder("Folder").unwrap();
    assert_eq!(
        store.observe_entry("Folder"),
        Ok(ObservedEntry::Folder {
            stamp: folder.clone(),
            empty: true
        })
    );
    store.sync_folder("Folder", &folder).unwrap();
    assert_eq!(store.sync_absent("Folder"), Err(StorageError::Changed));
    std::fs::write(directory.path().join("notes/Folder/file.md"), b"external").unwrap();
    assert!(matches!(
        store.observe_entry("Folder").unwrap(),
        ObservedEntry::Folder { empty: false, .. }
    ));
    assert_eq!(
        store.sync_folder("Folder", &folder),
        Err(StorageError::Changed)
    );
    assert!(
        matches!(store.observe_entry("Folder/file.md").unwrap(), ObservedEntry::File(file) if file.bytes == b"external")
    );
    assert_eq!(
        store.sync_absent("Unknown/missing"),
        Err(StorageError::NotFound)
    );
}

#[test]
fn nested_paths_reject_reserved_segments_links_and_oversized_logical_paths() {
    let (directory, store) = fixture();
    let notes = directory.path().join("notes");
    let outside = directory.path().join("outside");
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("file.md"), b"protected").unwrap();
    symlink(&outside, notes.join("linked")).unwrap();
    std::fs::create_dir(notes.join("Parent")).unwrap();
    std::fs::hard_link(outside.join("file.md"), notes.join("Parent/hard.md")).unwrap();
    assert_eq!(
        store.observe("Parent/missing.md"),
        Err(StorageError::NotFound)
    );
    for path in [
        "../outside/file.md",
        "/outside",
        "Parent//file.md",
        "Parent/../file.md",
        "Parent/.GiT/file.md",
        "Parent/.MDW-x/file.md",
        "Parent/file.md/",
    ] {
        assert_eq!(
            store.ensure_available(path),
            Err(StorageError::InvalidName),
            "{path}"
        );
        assert_eq!(
            store.observe(path),
            Err(StorageError::InvalidName),
            "{path}"
        );
    }
    let too_long = vec!["a".repeat(255); 5].join("/");
    assert_eq!(
        store.ensure_available(&too_long),
        Err(StorageError::InvalidName)
    );
    assert_eq!(
        store.create_folder("linked/new"),
        Err(StorageError::UnsafeFile)
    );
    assert_eq!(
        store.observe_entry("linked/file.md"),
        Err(StorageError::UnsafeFile)
    );
    assert_eq!(
        store.observe_entry("Parent/hard.md"),
        Err(StorageError::UnsafeFile)
    );
    assert_eq!(
        std::fs::read(outside.join("file.md")).unwrap(),
        b"protected"
    );
    assert!(!outside.join("new").exists());
}

#[cfg(feature = "test-hooks")]
mod faults {
    use super::*;
    use notryx_server_storage::StorageStage;

    #[test]
    fn creation_sync_failures_preserve_absent_or_fully_published_entries() {
        let (directory, store) = fixture();
        store.create_folder("Parent").unwrap();
        let hash = store.put_object(b"whole object\r\n").unwrap();
        for folder in [false, true] {
            let stages = [
                if folder {
                    StorageStage::BeforeCreatedFolderSync
                } else {
                    StorageStage::BeforeTemporaryFileSync
                },
                if folder {
                    StorageStage::AfterCreatedFolderSync
                } else {
                    StorageStage::AfterTemporaryFileSync
                },
                StorageStage::BeforeTargetRevalidation,
                StorageStage::AfterTargetRevalidation,
                StorageStage::BeforeRename,
                StorageStage::AfterRename,
                StorageStage::BeforeParentDirectorySync,
                StorageStage::AfterParentDirectorySync,
            ];
            for (index, cut) in stages.into_iter().enumerate() {
                let path = format!("Parent/{folder}-{index}");
                let mut reached = false;
                let hook = |stage| {
                    if stage == cut {
                        reached = true;
                        Err(StorageError::Io)
                    } else {
                        Ok(())
                    }
                };
                let result = if folder {
                    store.create_folder_with_hook(&path, hook).map(|_| ())
                } else {
                    store.create_file_with_hook(&path, &hash, hook).map(|_| ())
                };
                assert!(reached, "{cut:?}");
                assert_eq!(result, Err(StorageError::Io), "{cut:?}");
                // Empty folders are published by mkdir before their fsync; file bytes remain anonymous until publication.
                let absent = if folder {
                    matches!(
                        cut,
                        StorageStage::BeforeTargetRevalidation
                            | StorageStage::AfterTargetRevalidation
                            | StorageStage::BeforeRename
                    )
                } else {
                    index < 5
                };
                if absent {
                    assert_eq!(store.observe_entry(&path).unwrap(), ObservedEntry::Absent);
                } else if folder {
                    assert!(matches!(
                        store.observe_entry(&path).unwrap(),
                        ObservedEntry::Folder { empty: true, .. }
                    ));
                } else {
                    assert_eq!(store.observe(&path).unwrap().bytes, b"whole object\r\n");
                }
                assert_eq!(store.read_object(&hash).unwrap(), b"whole object\r\n");
            }
        }
        assert!(
            !std::fs::read_dir(directory.path().join("notes/Parent"))
                .unwrap()
                .any(|entry| entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".mdw-tmp-"))
        );
    }

    #[test]
    fn late_exact_and_casefold_collisions_are_never_replaced() {
        let (directory, store) = fixture();
        store.create_folder("Parent").unwrap();
        let hash = store.put_object(b"proposed").unwrap();
        for (index, folder) in [false, true].into_iter().enumerate() {
            for exact in [false, true] {
                let path = format!("Parent/new-{index}-{exact}");
                let other = if exact {
                    path.clone()
                } else {
                    path.replace("new", "NEW")
                };
                let target = directory.path().join("notes").join(&other);
                let hook = |stage| {
                    if stage == StorageStage::BeforeRename {
                        std::fs::write(&target, b"external").unwrap();
                    }
                    Ok(())
                };
                let result = if folder {
                    store.create_folder_with_hook(&path, hook).map(|_| ())
                } else {
                    store.create_file_with_hook(&path, &hash, hook).map(|_| ())
                };
                assert_eq!(result, Err(StorageError::AlreadyExists));
                assert_eq!(std::fs::read(&target).unwrap(), b"external");
                if !exact {
                    assert!(!directory.path().join("notes").join(&path).exists());
                }
            }
        }
    }

    #[test]
    fn moved_parent_cannot_receive_create_or_replacement_after_revalidation() {
        for operation in 0..3 {
            let (directory, store) = fixture();
            store.create_folder("Parent").unwrap();
            let notes = directory.path().join("notes");
            std::fs::write(notes.join("Parent/existing"), b"before").unwrap();
            let old = store.observe("Parent/existing").unwrap();
            let hash = store.put_object(b"proposed").unwrap();
            let moved = directory.path().join("moved");
            let hook = |stage| {
                if stage == StorageStage::BeforeRename {
                    std::fs::rename(notes.join("Parent"), &moved).unwrap();
                    std::fs::create_dir(notes.join("Parent")).unwrap();
                    std::fs::write(notes.join("Parent/existing"), b"new parent").unwrap();
                }
                Ok(())
            };
            let result = match operation {
                0 => store
                    .create_file_with_hook("Parent/new", &hash, hook)
                    .map(|_| ()),
                1 => store
                    .create_folder_with_hook("Parent/new", hook)
                    .map(|_| ()),
                _ => store
                    .replace_with_hook("Parent/existing", &old.stamp, &hash, hook)
                    .map(|_| ()),
            };
            assert_eq!(result, Err(StorageError::Changed));
            assert_eq!(std::fs::read(moved.join("existing")).unwrap(), b"before");
            assert_eq!(
                std::fs::read(notes.join("Parent/existing")).unwrap(),
                b"new parent"
            );
            assert!(!moved.join("new").exists());
            assert!(!notes.join("Parent/new").exists());
        }
    }

    #[test]
    fn replaced_created_folders_are_preserved_without_reporting_success() {
        let (directory, store) = fixture();
        let notes = directory.path().join("notes");
        let mut foreign = None;
        let result = store.create_folder_with_hook("new", |stage| {
            if stage == StorageStage::AfterCreatedFolderSync {
                let created = notes.join("new");
                std::fs::rename(&created, directory.path().join("original-created")).unwrap();
                std::fs::create_dir(&created).unwrap();
                std::fs::write(created.join("foreign"), b"preserve").unwrap();
                foreign = Some(created);
            }
            Ok(())
        });
        assert_eq!(result, Err(StorageError::Changed));
        assert!(directory.path().join("original-created").is_dir());
        assert_eq!(
            std::fs::read(foreign.unwrap().join("foreign")).unwrap(),
            b"preserve"
        );
    }

    #[test]
    fn recovery_requires_folder_and_parent_durability_and_rechecks_absence() {
        let (directory, store) = fixture();
        store.create_folder("Parent").unwrap();
        let folder = store.create_folder("Parent/Empty").unwrap();
        for cut in [
            StorageStage::BeforeRecoveryFolderSync,
            StorageStage::AfterRecoveryFolderSync,
            StorageStage::BeforeRecoveryDirectorySync,
            StorageStage::AfterRecoveryDirectorySync,
        ] {
            let mut reached = false;
            assert_eq!(
                store.sync_folder_with_hook("Parent/Empty", &folder, |stage| {
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
        }
        for cut in [
            StorageStage::BeforeRecoveryDirectorySync,
            StorageStage::AfterRecoveryDirectorySync,
        ] {
            let mut reached = false;
            assert_eq!(
                store.sync_absent_with_hook("Parent/Absent", |stage| {
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
        }
        assert_eq!(
            store.sync_absent_with_hook("Parent/Absent", |stage| {
                if stage == StorageStage::AfterRecoveryDirectorySync {
                    std::fs::write(directory.path().join("notes/Parent/Absent"), b"external")
                        .unwrap();
                }
                Ok(())
            }),
            Err(StorageError::Changed)
        );
        assert_eq!(
            store.sync_folder_with_hook("Parent/Empty", &folder, |stage| {
                if stage == StorageStage::AfterRecoveryFolderSync {
                    std::fs::write(
                        directory.path().join("notes/Parent/Empty/file"),
                        b"external",
                    )
                    .unwrap();
                }
                Ok(())
            }),
            Err(StorageError::Changed)
        );
    }
}
