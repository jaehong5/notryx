#![cfg(target_os = "linux")]

use notryx_domain::WorkspaceId;
use notryx_server_storage::{FileStore, StorageError};
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};

#[test]
fn explicit_init_creates_a_bound_store_that_reopens_without_reinitializing() {
    let directory = tempfile::tempdir().unwrap();
    let notes = directory.path().join("notes");
    let state = directory.path().join("state");
    std::fs::create_dir(&notes).unwrap();
    std::fs::create_dir(&state).unwrap();
    let workspace_id: WorkspaceId = "550e8400-e29b-41d4-a716-446655440000".parse().unwrap();
    let store = FileStore::init(&notes, &state, workspace_id)
        .expect("explicit init must establish durable marker and roots");
    assert_eq!(store.workspace_id(), workspace_id);
    assert!(state.join("objects/sha256").is_dir());
    assert!(notes.join(".mdw-server.lock").is_file());
    assert!(state.join("server.lock").is_file());
    std::fs::write(
        state.join("state.db"),
        b"schema validation belongs to state adapter",
    )
    .unwrap();
    let marker = std::fs::read(notes.join(".mdw-workspace.json")).unwrap();
    drop(store);
    let reopened = FileStore::open(&notes, &state).unwrap();
    assert_eq!(reopened.workspace_id(), workspace_id);
    assert_eq!(
        std::fs::read(notes.join(".mdw-workspace.json")).unwrap(),
        marker
    );
}

#[test]
fn opening_a_store_with_a_missing_database_never_initializes_it() {
    let directory = tempfile::tempdir().unwrap();
    let notes = directory.path().join("notes");
    let state = directory.path().join("state");
    std::fs::create_dir(&notes).unwrap();
    std::fs::create_dir(&state).unwrap();
    let workspace_id = "550e8400-e29b-41d4-a716-446655440000".parse().unwrap();
    let store = FileStore::init(&notes, &state, workspace_id).unwrap();
    assert_eq!(store.validate_state_database(), Err(StorageError::NotFound));
    drop(store);
    assert!(matches!(
        FileStore::open(&notes, &state),
        Err(StorageError::NotFound)
    ));
    assert!(!state.join("state.db").exists());
}

fn fixture() -> (tempfile::TempDir, FileStore) {
    let directory = tempfile::tempdir().unwrap();
    let notes = directory.path().join("notes");
    let state = directory.path().join("state");
    std::fs::create_dir(&notes).unwrap();
    std::fs::create_dir(&state).unwrap();
    let workspace_id = "550e8400-e29b-41d4-a716-446655440000".parse().unwrap();
    let store = FileStore::init(&notes, &state, workspace_id).unwrap();
    std::fs::write(state.join("state.db"), b"state adapter fixture").unwrap();
    (directory, store)
}

#[test]
fn replacing_existing_source_preserves_exact_bytes_and_before_object() {
    let (directory, store) = fixture();
    let path = directory.path().join("notes/document.md");
    let original = "\u{feff}---\r\ntitle: 한글\r\n---\r\n<!-- keep -->".as_bytes();
    let proposed = "\u{feff}---\r\ntitle: 수정\r\n---\r\n<!-- keep -->\r\n🙂".as_bytes();
    std::fs::write(&path, original).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
    let before = store.observe("document.md").unwrap();
    assert_eq!(before.bytes, original);
    let before_hash = store.put_object(original).unwrap();
    assert_eq!(before_hash, before.stamp.hash);
    let proposed_hash = store.put_object(proposed).unwrap();
    let after = store
        .replace("document.md", &before.stamp, &proposed_hash)
        .unwrap();
    assert_eq!(after.hash, proposed_hash);
    assert_eq!(std::fs::read(&path).unwrap(), proposed);
    assert_eq!(std::fs::metadata(&path).unwrap().mode() & 0o777, 0o640);
    assert_eq!(store.read_object(&before_hash).unwrap(), original);
    assert_eq!(store.read_object(&proposed_hash).unwrap(), proposed);
    store.sync_observed("document.md", &after).unwrap();
}

#[test]
fn open_never_creates_a_marker_or_lock_in_uninitialized_roots() {
    let directory = tempfile::tempdir().unwrap();
    let notes = directory.path().join("notes");
    let state = directory.path().join("state");
    std::fs::create_dir(&notes).unwrap();
    std::fs::create_dir(&state).unwrap();
    std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(FileStore::open(&notes, &state).is_err());
    assert_eq!(std::fs::read_dir(&notes).unwrap().count(), 0);
    assert_eq!(std::fs::read_dir(&state).unwrap().count(), 0);
}

#[test]
fn init_rejects_nonempty_roots_without_modifying_the_existing_file() {
    let directory = tempfile::tempdir().unwrap();
    let notes = directory.path().join("notes");
    let state = directory.path().join("state");
    std::fs::create_dir(&notes).unwrap();
    std::fs::create_dir(&state).unwrap();
    std::fs::write(notes.join("existing.md"), b"preserve").unwrap();
    assert!(matches!(
        FileStore::init(
            &notes,
            &state,
            "550e8400-e29b-41d4-a716-446655440000".parse().unwrap()
        ),
        Err(StorageError::NotEmpty)
    ));
    assert_eq!(
        std::fs::read(notes.join("existing.md")).unwrap(),
        b"preserve"
    );
    assert_eq!(std::fs::read_dir(&notes).unwrap().count(), 1);
    assert_eq!(std::fs::read_dir(&state).unwrap().count(), 0);
}

#[test]
fn same_notes_or_state_cannot_be_used_by_a_second_store() {
    let (first, _store) = fixture();
    let (second, other) = fixture();
    drop(other);
    assert!(matches!(
        FileStore::open(first.path().join("notes"), first.path().join("state")),
        Err(StorageError::Locked)
    ));
    assert!(matches!(
        FileStore::open(first.path().join("notes"), second.path().join("state")),
        Err(StorageError::Locked)
    ));
    assert!(matches!(
        FileStore::open(second.path().join("notes"), first.path().join("state")),
        Err(StorageError::Locked)
    ));
}

#[test]
fn symlink_hardlink_and_special_files_are_rejected_without_reading_their_targets() {
    let (directory, store) = fixture();
    let notes = directory.path().join("notes");
    let outside = directory.path().join("outside.md");
    std::fs::write(&outside, b"outside").unwrap();
    symlink(&outside, notes.join("symbolic.md")).unwrap();
    std::fs::hard_link(&outside, notes.join("hard.md")).unwrap();
    rustix::fs::mknodat(
        rustix::fs::CWD,
        notes.join("fifo.md"),
        rustix::fs::FileType::Fifo,
        rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
        0,
    )
    .unwrap();
    for name in ["symbolic.md", "hard.md", "fifo.md"] {
        assert_eq!(store.observe(name), Err(StorageError::UnsafeFile), "{name}");
    }
    assert_eq!(std::fs::read(&outside).unwrap(), b"outside");
    assert_eq!(store.observe("missing.md"), Err(StorageError::NotFound));
}

#[test]
fn document_paths_and_reserved_names_cannot_bypass_root_confinement() {
    let (_directory, store) = fixture();
    for name in [
        "../outside",
        "/etc/passwd",
        ".git",
        ".GiT",
        ".mdw-workspace.json",
        ".MDW-tmp-example",
        ".obsidian",
        "node_modules",
        "CON.txt",
        "e\u{301}.md",
    ] {
        assert_eq!(
            store.observe(name),
            Err(StorageError::InvalidName),
            "{name}"
        );
    }
    assert_eq!(
        store.observe("nested/document.md"),
        Err(StorageError::NotFound)
    );
    assert_eq!(
        store.read_object("../../outside"),
        Err(StorageError::InvalidHash)
    );
}

#[test]
fn root_symlinks_are_rejected_and_directory_renames_keep_the_open_roots_pinned() {
    let (directory, store) = fixture();
    let notes = directory.path().join("notes");
    let state = directory.path().join("state");
    std::fs::write(notes.join("document.md"), b"pinned").unwrap();
    symlink(&notes, directory.path().join("notes-link")).unwrap();
    assert!(matches!(
        FileStore::open(directory.path().join("notes-link"), &state),
        Err(StorageError::UnsafeFile)
    ));
    std::fs::rename(&notes, directory.path().join("moved-notes")).unwrap();
    std::fs::rename(&state, directory.path().join("moved-state")).unwrap();
    std::fs::create_dir(&notes).unwrap();
    std::fs::create_dir(&state).unwrap();
    std::fs::write(notes.join("document.md"), b"replacement root").unwrap();
    assert_eq!(store.observe("document.md").unwrap().bytes, b"pinned");
    assert_eq!(
        std::fs::read(store.state_db_path()).unwrap(),
        b"state adapter fixture"
    );
}

#[test]
fn database_links_and_unsafe_sidecars_are_rejected() {
    for filename in ["state.db", "state.db-wal", "state.db-shm"] {
        let (directory, store) = fixture();
        let path = directory.path().join("state").join(filename);
        if path.exists() {
            std::fs::remove_file(&path).unwrap();
        }
        let outside = directory.path().join("outside");
        std::fs::write(&outside, b"do not follow").unwrap();
        symlink(&outside, &path).unwrap();
        assert_eq!(
            store.validate_state_database(),
            Err(StorageError::UnsafeFile)
        );
        std::fs::remove_file(&path).unwrap();
        std::fs::hard_link(&outside, &path).unwrap();
        assert_eq!(
            store.validate_state_database(),
            Err(StorageError::UnsafeFile)
        );
    }
}

#[test]
fn objects_have_a_verified_sha256_name_and_are_never_overwritten() {
    let (directory, store) = fixture();
    let expected = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
    assert_eq!(store.put_object(b"abc").unwrap(), expected);
    assert_eq!(store.put_object(b"abc").unwrap(), expected);
    let object = directory.path().join("state/objects/sha256").join(expected);
    std::fs::set_permissions(&object, std::fs::Permissions::from_mode(0o600)).unwrap();
    std::fs::write(&object, b"tampered").unwrap();
    assert_eq!(
        store.read_object(expected),
        Err(StorageError::ChecksumMismatch)
    );
    assert_eq!(
        store.put_object(b"abc"),
        Err(StorageError::ChecksumMismatch)
    );
    assert_eq!(std::fs::read(&object).unwrap(), b"tampered");
}

#[test]
fn changed_or_missing_targets_cannot_be_silently_recreated_or_overwritten() {
    let (directory, store) = fixture();
    let path = directory.path().join("notes/document.md");
    std::fs::write(&path, b"before").unwrap();
    let before = store.observe("document.md").unwrap();
    let proposed = store.put_object(b"proposed").unwrap();
    std::fs::write(&path, b"external").unwrap();
    assert_eq!(
        store.replace("document.md", &before.stamp, &proposed),
        Err(StorageError::Changed)
    );
    assert_eq!(std::fs::read(&path).unwrap(), b"external");
    std::fs::remove_file(&path).unwrap();
    assert_eq!(
        store.replace("document.md", &before.stamp, &proposed),
        Err(StorageError::NotFound)
    );
    assert!(!path.exists());
}

#[test]
fn malformed_or_missing_marker_is_preserved_and_never_recreated_on_open() {
    let (directory, store) = fixture();
    let notes = directory.path().join("notes");
    let state = directory.path().join("state");
    let marker = notes.join(".mdw-workspace.json");
    drop(store);
    for bytes in [
        b"".as_slice(),
        b"not json".as_slice(),
        br#"{"format_version":99,"workspace_id":"550e8400-e29b-41d4-a716-446655440000"}"#
            .as_slice(),
        br#"{"format_version":1,"workspace_id":"invalid"}"#.as_slice(),
    ] {
        std::fs::write(&marker, bytes).unwrap();
        assert!(matches!(
            FileStore::open(&notes, &state),
            Err(StorageError::InvalidMarker)
        ));
        assert_eq!(std::fs::read(&marker).unwrap(), bytes);
    }
    std::fs::remove_file(&marker).unwrap();
    assert!(matches!(
        FileStore::open(&notes, &state),
        Err(StorageError::NotFound)
    ));
    assert!(!marker.exists());
}

#[test]
fn object_symlinks_are_rejected_even_when_the_target_bytes_match_the_hash() {
    let (directory, store) = fixture();
    let hash = store.put_object(b"abc").unwrap();
    let object = directory.path().join("state/objects/sha256").join(&hash);
    let outside = directory.path().join("outside");
    std::fs::write(&outside, b"abc").unwrap();
    std::fs::remove_file(&object).unwrap();
    symlink(&outside, &object).unwrap();
    assert_eq!(store.read_object(&hash), Err(StorageError::UnsafeFile));
    assert_eq!(store.put_object(b"abc"), Err(StorageError::UnsafeFile));
    assert_eq!(std::fs::read(&outside).unwrap(), b"abc");
}

#[test]
fn document_and_object_reads_are_bounded_by_the_supported_document_size() {
    let (directory, store) = fixture();
    let path = directory.path().join("notes/document.md");
    let maximum = vec![0_u8; notryx_server_storage::MAX_FILE_BYTES];
    std::fs::write(&path, &maximum).unwrap();
    assert_eq!(store.observe("document.md").unwrap().bytes, maximum);
    let hash = store.put_object(&maximum).unwrap();
    assert_eq!(store.read_object(&hash).unwrap(), maximum);
    std::fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap()
        .set_len(notryx_server_storage::MAX_FILE_BYTES as u64 + 1)
        .unwrap();
    assert_eq!(store.observe("document.md"), Err(StorageError::TooLarge));
    assert_eq!(
        store.put_object(&vec![0; notryx_server_storage::MAX_FILE_BYTES + 1]),
        Err(StorageError::TooLarge)
    );
}
