use notryx_domain::ContentRevision;
use notryx_domain::mutation::{
    RecoveryDecision, check_content_revision, decide_content_recovery, normalized_name_key,
    validate_name,
};

#[test]
fn content_write_requires_the_same_opaque_revision() {
    let current = ContentRevision::new("revision:A:10").unwrap();
    assert!(check_content_revision(&current, &current));
    for stale in ["revision:A:9", "revision:A:11", "REVISION:A:10", "*"] {
        assert!(!check_content_revision(
            &current,
            &ContentRevision::new(stale).unwrap()
        ));
    }
}

#[test]
fn content_recovery_classifies_observed_bytes_without_rolling_them_back() {
    assert_eq!(
        decide_content_recovery("before", "proposed", Some("proposed")),
        RecoveryDecision::Applied
    );
    assert_eq!(
        decide_content_recovery("before", "proposed", Some("before")),
        RecoveryDecision::NotApplied
    );
    assert_eq!(
        decide_content_recovery("before", "proposed", Some("external edit")),
        RecoveryDecision::Conflict
    );
    assert_eq!(
        decide_content_recovery("before", "proposed", None),
        RecoveryDecision::Conflict
    );
}

#[test]
fn unchanged_content_matches_proposed_without_claiming_a_rename_happened() {
    assert_eq!(
        decide_content_recovery("same", "same", Some("same")),
        RecoveryDecision::Applied
    );
    assert_eq!(
        decide_content_recovery("same", "same", Some("external edit")),
        RecoveryDecision::Conflict
    );
}

#[test]
fn accepts_single_nfc_names_and_measures_utf8_bytes() {
    for name in [
        "문서.md",
        "café.md",
        "한글 😀.md",
        ".hidden.md",
        "node_modules.md",
        "COM10.md",
        ".git-backup",
        "ordinary space.md",
    ] {
        assert!(validate_name(name).is_ok(), "{name:?}");
    }
    assert!(validate_name(&"a".repeat(255)).is_ok());
    assert!(validate_name(&"가".repeat(85)).is_ok());
    assert!(validate_name(&"a".repeat(256)).is_err());
    assert!(validate_name(&"가".repeat(86)).is_err());
    assert!(validate_name("cafe\u{301}.md").is_err());
}

#[test]
fn rejects_path_segments_and_control_characters() {
    for name in [
        "",
        ".",
        "..",
        "a/b.md",
        "a\\b.md",
        "name.",
        "name ",
        "\0.md",
        "line\nbreak.md",
        "line\rbreak.md",
        "tab\t.md",
        "delete\u{7f}.md",
        "control\u{85}.md",
    ] {
        assert!(validate_name(name).is_err(), "{name:?}");
    }
}

#[test]
fn reserved_and_excluded_names_cannot_be_bypassed_with_case_folding() {
    for name in [
        ".git",
        ".GIT",
        ".mdw-",
        ".MDW-workspace.json",
        ".mdw-tmp",
        ".obsidian",
        ".OBSIDIAN",
        ".obſidian",
        "node_modules",
        "NODE_MODULES",
    ] {
        assert!(validate_name(name).is_err(), "{name:?}");
    }
}

#[test]
fn windows_device_basenames_stay_reserved_with_extensions() {
    for name in [
        "CON",
        "con.md",
        "PrN.tar.gz",
        "aux",
        "NuL.txt",
        "COM1",
        "com9.md",
        "LPT1",
        "lpt9.backup.md",
        "COM¹.md",
        "COM²",
        "com³.txt",
        "LPT¹",
        "lpt².md",
        "LPT³",
        "CONIN$",
        "conout$",
        "con .md",
    ] {
        assert!(validate_name(name).is_err(), "{name:?}");
    }
}

#[test]
fn collision_keys_use_nfc_and_full_default_case_folding() {
    for (left, right) in [
        ("Straße.md", "STRASSE.MD"),
        ("cafe\u{301}.md", "CAFÉ.MD"),
        ("ὈΔΥΣΣΕΎΣ.md", "ὀδυσσεύς.md"),
        ("ς.md", "Σ.MD"),
        ("ﬃ.md", "FFI.MD"),
        ("\u{1100}\u{1161}.md", "가.MD"),
    ] {
        assert_eq!(
            normalized_name_key(left),
            normalized_name_key(right),
            "{left:?} / {right:?}"
        );
    }
    assert_eq!(normalized_name_key("Straße.MD"), "strasse.md");
    assert_eq!(normalized_name_key("CAFE\u{301}.MD"), "café.md");
    assert_ne!(
        normalized_name_key("café.md"),
        normalized_name_key("cafe.md")
    );
}
