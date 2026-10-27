#![cfg(target_os = "linux")]

use notryx_server_storage::{FileStore, StorageError};
use rustix::fs::{self, Gid, XattrFlags};
use std::fs::OpenOptions;
use std::os::unix::fs::{MetadataExt, PermissionsExt};

fn fixture() -> (tempfile::TempDir, FileStore) {
    let directory = tempfile::tempdir().unwrap();
    let notes = directory.path().join("notes");
    let state = directory.path().join("state");
    std::fs::create_dir(&notes).unwrap();
    std::fs::create_dir(&state).unwrap();
    let workspace_id = "550e8400-e29b-41d4-a716-446655440000".parse().unwrap();
    let store = FileStore::init(&notes, &state, workspace_id).unwrap();
    (directory, store)
}

fn read_attribute(file: &std::fs::File, name: &str) -> Vec<u8> {
    let mut value = vec![0; 65_536];
    let length = fs::fgetxattr(file, name, &mut value).unwrap();
    value.truncate(length);
    value
}

fn access_acl() -> Vec<u8> {
    let mut bytes = 2u32.to_le_bytes().to_vec();
    for (tag, permission, id) in [
        (1u16, 6u16, u32::MAX),
        (2, 4, 65_534),
        (4, 4, u32::MAX),
        (16, 4, u32::MAX),
        (32, 0, u32::MAX),
    ] {
        bytes.extend_from_slice(&tag.to_le_bytes());
        bytes.extend_from_slice(&permission.to_le_bytes());
        bytes.extend_from_slice(&id.to_le_bytes());
    }
    bytes
}

#[test]
fn replacement_preserves_owner_mode_access_acl_and_user_attributes() {
    let (directory, store) = fixture();
    let path = directory.path().join("notes/document.md");
    std::fs::write(&path, b"original").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
    let original = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    fs::fsetxattr(
        &original,
        "user.notryx-test",
        b"synthetic",
        XattrFlags::empty(),
    )
    .unwrap();
    let acl = access_acl();
    fs::fsetxattr(
        &original,
        "system.posix_acl_access",
        &acl,
        XattrFlags::empty(),
    )
    .unwrap();
    let before = original.metadata().unwrap();
    let original_acl = read_attribute(&original, "system.posix_acl_access");
    let original_user = read_attribute(&original, "user.notryx-test");
    let stamp = store.observe("document.md").unwrap().stamp;
    let object = store.put_object(b"updated").unwrap();
    store.replace("document.md", &stamp, &object).unwrap();
    let updated = OpenOptions::new().read(true).open(&path).unwrap();
    let after = updated.metadata().unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), b"updated");
    assert_eq!(
        (after.uid(), after.gid(), after.mode() & 0o7777),
        (before.uid(), before.gid(), before.mode() & 0o7777)
    );
    assert_eq!(
        read_attribute(&updated, "system.posix_acl_access"),
        original_acl
    );
    assert_eq!(read_attribute(&updated, "user.notryx-test"), original_user);
}

#[test]
fn replacement_rejects_setuid_before_changing_original() {
    let (directory, store) = fixture();
    let path = directory.path().join("notes/document.md");
    std::fs::write(&path, b"original").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o4640)).unwrap();
    let before = store.observe("document.md").unwrap();
    let object = store.put_object(b"updated").unwrap();
    assert_eq!(
        store.replace("document.md", &before.stamp, &object),
        Err(StorageError::UnsafeFile)
    );
    assert_eq!(std::fs::read(&path).unwrap(), b"original");
    assert_eq!(std::fs::metadata(&path).unwrap().mode() & 0o7777, 0o4640);
}

#[test]
fn replacement_does_not_inherit_a_new_default_access_acl() {
    let (directory, store) = fixture();
    let notes = directory.path().join("notes");
    let path = notes.join("document.md");
    std::fs::write(&path, b"original").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
    let source = OpenOptions::new().read(true).open(&path).unwrap();
    let mut value = vec![0; 65_536];
    assert_eq!(
        fs::fgetxattr(&source, "system.posix_acl_access", &mut value),
        Err(rustix::io::Errno::NODATA)
    );
    let notes_fd = OpenOptions::new().read(true).open(&notes).unwrap();
    fs::fsetxattr(
        &notes_fd,
        "system.posix_acl_default",
        &access_acl(),
        XattrFlags::empty(),
    )
    .unwrap();
    let stamp = store.observe("document.md").unwrap().stamp;
    let object = store.put_object(b"updated").unwrap();
    store.replace("document.md", &stamp, &object).unwrap();
    let updated = OpenOptions::new().read(true).open(&path).unwrap();
    assert_eq!(
        fs::fgetxattr(&updated, "system.posix_acl_access", &mut value),
        Err(rustix::io::Errno::NODATA)
    );
    assert_eq!(updated.metadata().unwrap().mode() & 0o7777, 0o640);
}

#[test]
fn replacement_preserves_an_available_supplementary_group() {
    let (directory, store) = fixture();
    let path = directory.path().join("notes/document.md");
    std::fs::write(&path, b"original").unwrap();
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    let group = file.metadata().unwrap().gid();
    let groups = std::fs::read_to_string("/proc/self/status").unwrap();
    let alternate = groups
        .lines()
        .find_map(|line| line.strip_prefix("Groups:"))
        .unwrap()
        .split_whitespace()
        .filter_map(|value| value.parse::<u32>().ok())
        .find(|value| *value != group);
    if let Some(alternate) = alternate {
        fs::fchown(&file, None, Some(Gid::from_raw(alternate))).unwrap();
    }
    let before = file.metadata().unwrap();
    let stamp = store.observe("document.md").unwrap().stamp;
    let object = store.put_object(b"updated").unwrap();
    store.replace("document.md", &stamp, &object).unwrap();
    assert_eq!(std::fs::metadata(&path).unwrap().gid(), before.gid());
    assert_eq!(std::fs::read(&path).unwrap(), b"updated");
}
