use notryx_server::{
    Workspace,
    config::{Config, ConfigError},
};
use std::{fs, os::unix::fs::PermissionsExt, path::Path};

#[path = "../src/cold_backup.rs"]
mod cold_backup;

const MINIMAL: &str =
    "[workspace]\nnotes_root='/tmp/notryx-config-notes'\nstate_root='/tmp/notryx-config-state'\n";
const DEFAULT_DAYS: u64 = 30;
const DEFAULT_BUDGET: u64 = 512 * 1024 * 1024;

fn retention_policy(config: &Config) -> (u64, u64) {
    (
        config.sync.journal_retention_days,
        config.sync.journal_budget_bytes,
    )
}

#[test]
fn journal_retention_config_legacy_and_partial_use_documented_defaults() {
    let legacy = Config::from_toml(MINIMAL).unwrap();
    assert_eq!(retention_policy(&legacy), (DEFAULT_DAYS, DEFAULT_BUDGET));

    let days = Config::from_toml(&format!("{MINIMAL}[sync]\njournal_retention_days=14\n")).unwrap();
    assert_eq!(retention_policy(&days), (14, DEFAULT_BUDGET));
    let budget =
        Config::from_toml(&format!("{MINIMAL}[sync]\njournal_budget_bytes=1234567\n")).unwrap();
    assert_eq!(retention_policy(&budget), (DEFAULT_DAYS, 1234567));
}

#[test]
fn journal_retention_config_accepts_explicit_values() {
    let config = Config::from_toml(&format!(
        "{MINIMAL}[sync]\njournal_retention_days=7\njournal_budget_bytes=1048576\n"
    ))
    .expect("documented sync settings must be accepted");
    assert_eq!(retention_policy(&config), (7, 1048576));
}

#[test]
fn journal_retention_config_rejects_invalid_fields_and_overflow() {
    let valid =
        format!("{MINIMAL}[sync]\njournal_retention_days=7\njournal_budget_bytes=1048576\n");
    assert!(Config::from_toml(&valid).is_ok());
    for setting in [
        "journal_retention_days=-1",
        "journal_budget_bytes=-1",
        "journal_retention_days='30'",
        "journal_budget_bytes='512'",
        "journal_retention_day=30",
        "journal_retention_days=106751991167301",
        "journal_retention_days=1\njournal_retention_days=2",
    ] {
        let config = format!("{MINIMAL}[sync]\n{setting}\n");
        assert!(
            matches!(Config::from_toml(&config), Err(ConfigError::Invalid)),
            "invalid sync setting accepted: {setting}"
        );
    }
}

fn config_text(root: &Path, days: u64, budget: u64) -> String {
    format!(
        "[server]\nlisten='127.0.0.1:0'\nadmin_socket='{}'\n[workspace]\nnotes_root='{}'\nstate_root='{}'\n[sync]\njournal_retention_days={days}\njournal_budget_bytes={budget}\n",
        root.join("admin.sock").display(),
        root.join("notes").display(),
        root.join("state").display()
    )
}

#[tokio::test]
async fn journal_retention_config_backup_rejects_changed_policy_and_restore_preserves_it() {
    let directory = tempfile::Builder::new()
        .permissions(fs::Permissions::from_mode(0o700))
        .tempdir()
        .unwrap();
    let root = directory.path();
    let notes = root.join("notes");
    let state = root.join("state");
    fs::create_dir(&notes).unwrap();
    fs::create_dir(&state).unwrap();
    let mut workspace = Workspace::initialize(&notes, &state).await.unwrap();
    fs::write(notes.join("Synthetic.md"), b"# synthetic\n").unwrap();
    workspace.adopt_root_document("Synthetic.md").await.unwrap();
    workspace.close().await.unwrap();

    let config_file = root.join("server.toml");
    let original_text = config_text(root, 17, 1_234_567);
    let config = Config::from_toml(&original_text).unwrap();
    fs::write(&config_file, &original_text).unwrap();
    let backup = root.join("backup");
    cold_backup::create(&config, &config_file, &backup)
        .await
        .expect("matching config should create backup");
    cold_backup::verify(&backup).unwrap();
    let copied_before = fs::read(backup.join("config/server.toml")).unwrap();
    assert_eq!(copied_before, original_text.as_bytes());

    for (days, budget, name) in [
        (18, 1_234_567, "different-days"),
        (17, 1_234_568, "different-budget"),
    ] {
        fs::write(&config_file, config_text(root, days, budget)).unwrap();
        let target = root.join(name);
        let error = cold_backup::create(&config, &config_file, &target)
            .await
            .expect_err("backup must reject a different policy");
        assert_eq!(error.to_string(), "backup_invalid");
        assert!(!target.exists());
        assert_eq!(
            fs::read(notes.join("Synthetic.md")).unwrap(),
            b"# synthetic\n"
        );
        assert_eq!(
            fs::read(backup.join("config/server.toml")).unwrap(),
            copied_before
        );
    }

    fs::write(&config_file, &original_text).unwrap();
    let backup_again = root.join("backup-again");
    cold_backup::create(&config, &config_file, &backup_again)
        .await
        .expect("restored matching policy should still create a backup");
    cold_backup::verify(&backup_again).unwrap();

    let restored = root.join("restore");
    cold_backup::restore(&backup, &restored)
        .await
        .expect("restore must succeed");
    let restored_text = fs::read_to_string(restored.join("config/server.toml")).unwrap();
    let value: toml::Value = toml::from_str(&restored_text).unwrap();
    assert_eq!(
        value["sync"]["journal_retention_days"].as_integer(),
        Some(17)
    );
    assert_eq!(
        value["sync"]["journal_budget_bytes"].as_integer(),
        Some(1_234_567)
    );
    assert_eq!(
        fs::read(restored.join("notes/Synthetic.md")).unwrap(),
        b"# synthetic\n"
    );
    assert_eq!(
        fs::read(backup.join("config/server.toml")).unwrap(),
        copied_before
    );
}
