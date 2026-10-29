#[test]
fn sync_and_trash_commands_have_explicit_main_window_capabilities() {
    let manifest = include_str!("../build.rs");
    let capabilities: serde_json::Value =
        serde_json::from_str(include_str!("../capabilities/main.json")).unwrap();
    let permissions = capabilities["permissions"].as_array().unwrap();
    for (command, permission) in [
        ("workspace_sync_status", "allow-workspace-sync-status"),
        ("workspace_trash", "allow-workspace-trash"),
        ("workspace_frozen_deletes", "allow-workspace-frozen-deletes"),
        ("workspace_frozen_moves", "allow-workspace-frozen-moves"),
        (
            "workspace_resolve_frozen_move",
            "allow-workspace-resolve-frozen-move",
        ),
        (
            "workspace_resolve_frozen_delete",
            "allow-workspace-resolve-frozen-delete",
        ),
        ("workspace_queue_delete", "allow-workspace-queue-delete"),
        ("workspace_delete_status", "allow-workspace-delete-status"),
        (
            "workspace_request_delete_abandon",
            "allow-workspace-request-delete-abandon",
        ),
        ("workspace_search", "allow-workspace-search"),
        ("workspace_cancel_search", "allow-workspace-cancel-search"),
        (
            "workspace_request_restore_abandon",
            "allow-workspace-request-restore-abandon",
        ),
    ] {
        assert!(
            manifest.contains(&format!("\"{command}\"")),
            "missing command {command}"
        );
        assert!(
            permissions.iter().any(|entry| entry == permission),
            "missing capability {permission}"
        );
    }
}
