#[test]
fn content_conflict_comparison_has_explicit_main_window_capability() {
    let manifest = include_str!("../build.rs");
    let capabilities: serde_json::Value =
        serde_json::from_str(include_str!("../capabilities/main.json")).unwrap();
    let permissions = capabilities["permissions"].as_array().unwrap();
    assert!(manifest.contains("\"workspace_prepare_content_conflict\""));
    assert!(
        permissions
            .iter()
            .any(|entry| entry == "allow-workspace-prepare-content-conflict")
    );
}
