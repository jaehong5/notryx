#![cfg(target_os = "linux")]

use notryx_server_storage::{FileStore, StorageError};
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
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
    (directory, store)
}

#[test]
fn listing_is_sorted_repeatable_and_preserves_actual_spelling() {
    let (directory, store) = fixture();
    let notes = directory.path().join("notes");
    std::fs::create_dir(notes.join("Folder")).unwrap();
    for name in ["한글.md", ".visible", "z.md", "A.md"] {
        std::fs::write(notes.join(name), b"unchanged\r\n").unwrap();
    }
    std::fs::write(notes.join("Folder/child.md"), b"child").unwrap();
    for _ in 0..3 {
        assert_eq!(
            store.list_children("").unwrap(),
            [".visible", "A.md", "Folder", "z.md", "한글.md"]
        );
        assert_eq!(store.list_children("Folder").unwrap(), ["child.md"]);
    }
    assert_eq!(std::fs::read(notes.join("A.md")).unwrap(), b"unchanged\r\n");
}

#[test]
fn listing_omits_only_fixed_exclusions_and_leaves_them_untouched() {
    let (directory, store) = fixture();
    let notes = directory.path().join("notes");
    for name in [".GiT", ".ObSiDiAn", "NoDe_MoDuLeS", ".MDW-private"] {
        std::fs::create_dir(notes.join(name)).unwrap();
        std::fs::write(notes.join(name).join("retained"), b"private").unwrap();
    }
    std::fs::write(notes.join(".gitignore"), b"retain").unwrap();
    assert_eq!(store.list_children("").unwrap(), [".gitignore"]);
    for name in [".GiT", ".ObSiDiAn", "NoDe_MoDuLeS", ".MDW-private"] {
        assert_eq!(
            std::fs::read(notes.join(name).join("retained")).unwrap(),
            b"private"
        );
    }
}

#[test]
fn invalid_names_fail_instead_of_being_silently_excluded() {
    for name in ["trailing.", "CON", "e\u{301}.md", "line\nbreak"] {
        let (directory, store) = fixture();
        let path = directory.path().join("notes").join(name);
        std::fs::write(&path, b"retained").unwrap();
        assert_eq!(store.list_children(""), Err(StorageError::InvalidName));
        assert_eq!(std::fs::read(path).unwrap(), b"retained");
    }
    let (directory, store) = fixture();
    let path = directory
        .path()
        .join("notes")
        .join(OsStr::from_bytes(b"invalid-\xff"));
    std::fs::write(&path, b"retained").unwrap();
    assert_eq!(store.list_children(""), Err(StorageError::InvalidName));
    assert_eq!(std::fs::read(path).unwrap(), b"retained");
}

#[test]
fn listing_rejects_unsafe_paths_and_does_not_follow_child_links() {
    let (directory, store) = fixture();
    let notes = directory.path().join("notes");
    let outside = directory.path().join("outside");
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("protected"), b"outside").unwrap();
    symlink(&outside, notes.join("linked")).unwrap();
    std::fs::hard_link(outside.join("protected"), notes.join("hard")).unwrap();
    assert_eq!(store.list_children("").unwrap(), ["hard", "linked"]);
    for path in ["linked", "linked/child"] {
        assert_eq!(store.list_children(path), Err(StorageError::UnsafeFile));
    }
    for path in [
        "..",
        "../outside",
        "/outside",
        ".git",
        ".MDW-private",
        "folder//child",
        "folder/",
    ] {
        assert_eq!(
            store.list_children(path),
            Err(StorageError::InvalidName),
            "{path}"
        );
    }
    let too_long = vec!["a".repeat(255); 5].join("/");
    assert_eq!(
        store.list_children(&too_long),
        Err(StorageError::InvalidName)
    );
    assert_eq!(store.observe_entry("hard"), Err(StorageError::UnsafeFile));
    assert_eq!(
        std::fs::read(outside.join("protected")).unwrap(),
        b"outside"
    );
}

#[cfg(feature = "test-hooks")]
#[test]
fn listing_rejects_directory_changes_and_path_replacement_during_the_scan() {
    use notryx_server_storage::StorageStage;

    for replace in [false, true] {
        let (directory, store) = fixture();
        let notes = directory.path().join("notes");
        std::fs::create_dir(notes.join("Folder")).unwrap();
        std::fs::write(notes.join("Folder/before"), b"before").unwrap();
        let mut reached = false;
        let result = store.list_children_with_hook("Folder", |stage| {
            if stage == StorageStage::AfterDirectoryRead {
                reached = true;
                if replace {
                    std::fs::rename(notes.join("Folder"), notes.join("Moved")).unwrap();
                    std::fs::create_dir(notes.join("Folder")).unwrap();
                }
                std::fs::write(notes.join("Folder/after"), b"after").unwrap();
            }
            Ok(())
        });
        assert!(reached);
        assert_eq!(result, Err(StorageError::Changed));
        assert_eq!(std::fs::read(notes.join("Folder/after")).unwrap(), b"after");
    }
}
