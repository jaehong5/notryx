use notryx_protocol::WorkspaceDeletedDraftCopyConfirmStatus;
use ts_rs::{Config, TS};

#[test]
fn deleted_copy_confirm_status_has_same_wire_and_typescript_spelling() {
    let status = WorkspaceDeletedDraftCopyConfirmStatus::Pending;
    assert_eq!(serde_json::to_value(status).unwrap(), "pending");
    let declaration = WorkspaceDeletedDraftCopyConfirmStatus::decl(&Config::default());
    assert!(declaration.contains("\"pending\""));
    assert!(!declaration.contains("\"Pending\""));
}
