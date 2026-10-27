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

fn identity(stamp: &EntryStamp) -> (u64, u64) {
    match stamp {
        EntryStamp::File(stamp) => (stamp.device, stamp.inode),
        EntryStamp::Folder(stamp) => (stamp.device, stamp.inode),
    }
}

#[test]
fn renaming_and_moving_preserve_bytes_identity_and_directory_contents() {
    let (_directory, store) = fixture();
    store.create_folder("Parent").unwrap();
    store.create_folder("Other").unwrap();
    let bytes = b"\xef\xbb\xbf# Heading\r\nunknown syntax\r";
    let hash = store.put_object(bytes).unwrap();
    store.create_file("Parent/Note.md", &hash).unwrap();
    let before = stamp(&store, "Parent/Note.md");
    assert_eq!(
        store.ensure_move_available("Parent/Note.md", "Parent/note.md"),
        Ok(())
    );
    let renamed = store
        .move_entry("Parent/Note.md", "Parent/note.md", &before)
        .unwrap();
    assert_eq!(identity(&renamed), identity(&before));
    assert_eq!(
        store.observe_entry("Parent/Note.md").unwrap(),
        ObservedEntry::Absent
    );
    assert_eq!(store.observe("Parent/note.md").unwrap().bytes, bytes);
    store
        .sync_move("Parent/Note.md", "Parent/note.md", &before, true)
        .unwrap();

    let parent = stamp(&store, "Parent");
    let moved = store.move_entry("Parent", "Other/Moved", &parent).unwrap();
    assert_eq!(identity(&moved), identity(&parent));
    assert_eq!(
        store.observe_entry("Parent").unwrap(),
        ObservedEntry::Absent
    );
    assert_eq!(store.observe("Other/Moved/note.md").unwrap().bytes, bytes);
    assert_eq!(
        identity(&stamp(&store, "Other/Moved/note.md")),
        identity(&before)
    );
    store
        .sync_move("Parent", "Other/Moved", &parent, true)
        .unwrap();
}

#[test]
fn destination_checks_exclude_only_the_exact_source_and_never_replace_collisions() {
    let (directory, store) = fixture();
    let notes = directory.path().join("notes");
    let hash = store.put_object(b"source").unwrap();
    store.create_file("Note.md", &hash).unwrap();
    let before = stamp(&store, "Note.md");
    std::fs::write(notes.join("NOTE.MD"), b"external collision").unwrap();
    for destination in ["note.md", "NOTE.MD"] {
        assert_eq!(
            store.ensure_move_available("Note.md", destination),
            Err(StorageError::AlreadyExists)
        );
        assert_eq!(
            store.move_entry("Note.md", destination, &before),
            Err(StorageError::AlreadyExists)
        );
    }
    std::fs::write(notes.join("e\u{301}.md"), b"external normalization").unwrap();
    assert_eq!(
        store.move_entry("Note.md", "é.md", &before),
        Err(StorageError::AlreadyExists)
    );
    store.create_folder("Folder").unwrap();
    assert_eq!(
        store.move_entry("Note.md", "Folder", &before),
        Err(StorageError::AlreadyExists)
    );
    assert_eq!(store.observe("Note.md").unwrap().bytes, b"source");
    assert_eq!(
        std::fs::read(notes.join("NOTE.MD")).unwrap(),
        b"external collision"
    );
    assert_eq!(
        std::fs::read(notes.join("e\u{301}.md")).unwrap(),
        b"external normalization"
    );
}

#[test]
fn movement_rejects_unsafe_paths_links_and_changed_sources_without_writes() {
    let (directory, store) = fixture();
    let notes = directory.path().join("notes");
    let outside = directory.path().join("outside");
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("file.md"), b"outside").unwrap();
    symlink(&outside, notes.join("linked")).unwrap();
    let hash = store.put_object(b"source").unwrap();
    store.create_file("source.md", &hash).unwrap();
    let before = stamp(&store, "source.md");
    for destination in [
        "../outside/new.md",
        "/absolute",
        "a//b",
        ".git/new.md",
        "source.md",
    ] {
        assert_eq!(
            store.move_entry("source.md", destination, &before),
            Err(StorageError::InvalidName),
            "{destination}"
        );
    }
    assert_eq!(
        store.move_entry("source.md", "linked/new.md", &before),
        Err(StorageError::UnsafeFile)
    );
    std::fs::hard_link(outside.join("file.md"), notes.join("hard.md")).unwrap();
    assert_eq!(
        store.move_entry("hard.md", "new.md", &before),
        Err(StorageError::UnsafeFile)
    );
    std::fs::write(notes.join("source.md"), b"external changed").unwrap();
    assert_eq!(
        store.move_entry("source.md", "new.md", &before),
        Err(StorageError::Changed)
    );
    assert_eq!(
        std::fs::read(notes.join("source.md")).unwrap(),
        b"external changed"
    );
    assert_eq!(std::fs::read(outside.join("file.md")).unwrap(), b"outside");
    assert!(!outside.join("new.md").exists());
    assert!(!notes.join("new.md").exists());
}

#[test]
fn recovery_requires_one_expected_entry_and_one_absent_path() {
    let (directory, store) = fixture();
    let notes = directory.path().join("notes");
    store.create_folder("Source").unwrap();
    store.create_folder("Destination").unwrap();
    let hash = store.put_object(b"source").unwrap();
    store.create_file("Source/file.md", &hash).unwrap();
    let before = stamp(&store, "Source/file.md");
    store
        .sync_move("Source/file.md", "Destination/file.md", &before, false)
        .unwrap();
    assert_eq!(
        store.sync_move("Source/file.md", "Destination/file.md", &before, true),
        Err(StorageError::Changed)
    );
    std::fs::write(notes.join("Destination/file.md"), b"source").unwrap();
    assert_eq!(
        store.sync_move("Source/file.md", "Destination/file.md", &before, false),
        Err(StorageError::Changed)
    );
    std::fs::remove_file(notes.join("Source/file.md")).unwrap();
    assert_eq!(
        store.sync_move("Source/file.md", "Destination/file.md", &before, true),
        Err(StorageError::Changed)
    );
    assert_eq!(
        std::fs::read(notes.join("Destination/file.md")).unwrap(),
        b"source"
    );
}

#[cfg(feature = "test-hooks")]
mod faults {
    use super::*;
    use notryx_server_storage::StorageStage;

    #[test]
    fn move_failures_preserve_an_entry_on_exactly_one_side_and_allow_recovery() {
        let (_directory, store) = fixture();
        store.create_folder("Source").unwrap();
        store.create_folder("Destination").unwrap();
        let hash = store.put_object(b"whole\r\n").unwrap();
        let stages = [
            StorageStage::BeforeRename,
            StorageStage::AfterRename,
            StorageStage::BeforeMoveEntrySync,
            StorageStage::AfterMoveEntrySync,
            StorageStage::BeforeMoveSourceDirectorySync,
            StorageStage::AfterMoveSourceDirectorySync,
            StorageStage::BeforeMoveDestinationDirectorySync,
            StorageStage::AfterMoveDestinationDirectorySync,
        ];
        for folder in [false, true] {
            for (index, cut) in stages.into_iter().enumerate() {
                let source = format!("Source/{folder}-{index}");
                let destination = format!("Destination/{folder}-{index}");
                if folder {
                    store.create_folder(&source).unwrap();
                } else {
                    store.create_file(&source, &hash).unwrap();
                }
                let before = stamp(&store, &source);
                let mut reached = false;
                let result = store.move_entry_with_hook(&source, &destination, &before, |stage| {
                    if stage == cut {
                        reached = true;
                        Err(StorageError::Io)
                    } else {
                        Ok(())
                    }
                });
                assert!(reached, "{folder} {cut:?}");
                assert_eq!(result, Err(StorageError::Io));
                let applied = cut != StorageStage::BeforeRename;
                let existing = if applied { &destination } else { &source };
                let absent = if applied { &source } else { &destination };
                assert_eq!(identity(&stamp(&store, existing)), identity(&before));
                assert_eq!(store.observe_entry(absent).unwrap(), ObservedEntry::Absent);
                store
                    .sync_move(&source, &destination, &before, applied)
                    .unwrap();
                if !folder {
                    assert_eq!(store.observe(existing).unwrap().bytes, b"whole\r\n");
                }
            }
        }
    }

    #[test]
    fn source_and_parent_replacement_and_late_collisions_are_rejected() {
        for action in [
            "source",
            "source_parent",
            "destination_parent",
            "destination",
        ] {
            let (directory, store) = fixture();
            let notes = directory.path().join("notes");
            store.create_folder("Source").unwrap();
            store.create_folder("Destination").unwrap();
            let hash = store.put_object(b"original").unwrap();
            store.create_file("Source/file.md", &hash).unwrap();
            let before = stamp(&store, "Source/file.md");
            let mut reached = false;
            let result = store.move_entry_with_hook(
                "Source/file.md",
                "Destination/new.md",
                &before,
                |stage| {
                    if stage == StorageStage::BeforeRename {
                        reached = true;
                        match action {
                            "source" => {
                                std::fs::write(notes.join("Source/file.md"), b"changed").unwrap()
                            }
                            "source_parent" => {
                                std::fs::rename(notes.join("Source"), notes.join("OriginalSource"))
                                    .unwrap();
                                std::fs::create_dir(notes.join("Source")).unwrap();
                            }
                            "destination_parent" => {
                                std::fs::rename(
                                    notes.join("Destination"),
                                    notes.join("OriginalDestination"),
                                )
                                .unwrap();
                                std::fs::create_dir(notes.join("Destination")).unwrap();
                            }
                            "destination" => {
                                std::fs::write(notes.join("Destination/NEW.MD"), b"collision")
                                    .unwrap()
                            }
                            _ => unreachable!(),
                        }
                    }
                    Ok(())
                },
            );
            assert!(reached);
            assert_eq!(
                result,
                Err(if action == "destination" {
                    StorageError::AlreadyExists
                } else {
                    StorageError::Changed
                }),
                "{action}"
            );
            assert!(!notes.join("Destination/new.md").exists());
            let source = if action == "source_parent" {
                "OriginalSource/file.md"
            } else {
                "Source/file.md"
            };
            assert_eq!(
                std::fs::read(notes.join(source)).unwrap(),
                if action == "source" {
                    b"changed".as_slice()
                } else {
                    b"original".as_slice()
                }
            );
        }
    }

    #[test]
    fn recovery_syncs_the_entry_then_both_parents_and_propagates_each_failure() {
        let (_directory, store) = fixture();
        store.create_folder("Source").unwrap();
        store.create_folder("Destination").unwrap();
        let hash = store.put_object(b"whole").unwrap();
        for applied in [false, true] {
            let source = format!("Source/{applied}.md");
            let destination = format!("Destination/{applied}.md");
            store.create_file(&source, &hash).unwrap();
            let before = stamp(&store, &source);
            if applied {
                store.move_entry(&source, &destination, &before).unwrap();
            }
            let mut stages = Vec::new();
            store
                .sync_move_with_hook(&source, &destination, &before, applied, |stage| {
                    stages.push(stage);
                    Ok(())
                })
                .unwrap();
            assert_eq!(
                stages,
                [
                    StorageStage::BeforeRecoveryFileSync,
                    StorageStage::AfterRecoveryFileSync,
                    StorageStage::BeforeRecoverySourceDirectorySync,
                    StorageStage::AfterRecoverySourceDirectorySync,
                    StorageStage::BeforeRecoveryDestinationDirectorySync,
                    StorageStage::AfterRecoveryDestinationDirectorySync,
                ]
            );
            for cut in stages {
                let mut reached = false;
                assert_eq!(
                    store.sync_move_with_hook(&source, &destination, &before, applied, |stage| {
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
            store
                .sync_move(&source, &destination, &before, applied)
                .unwrap();
        }
    }

    #[test]
    fn a_shared_parent_is_synced_once_and_changed_recovery_state_is_preserved() {
        let (directory, store) = fixture();
        let hash = store.put_object(b"source").unwrap();
        store.create_file("source.md", &hash).unwrap();
        let before = stamp(&store, "source.md");
        let mut stages = Vec::new();
        store
            .move_entry_with_hook("source.md", "destination.md", &before, |stage| {
                stages.push(stage);
                Ok(())
            })
            .unwrap();
        assert_eq!(
            stages
                .iter()
                .filter(|stage| **stage == StorageStage::BeforeMoveSourceDirectorySync)
                .count(),
            1
        );
        assert!(!stages.contains(&StorageStage::BeforeMoveDestinationDirectorySync));
        let notes = directory.path().join("notes");
        let result =
            store.sync_move_with_hook("source.md", "destination.md", &before, true, |stage| {
                if stage == StorageStage::AfterRecoverySourceDirectorySync {
                    std::fs::write(notes.join("source.md"), b"new external source").unwrap();
                }
                Ok(())
            });
        assert_eq!(result, Err(StorageError::Changed));
        assert_eq!(
            std::fs::read(notes.join("source.md")).unwrap(),
            b"new external source"
        );
        assert_eq!(store.observe("destination.md").unwrap().bytes, b"source");
    }
}
