use notryx_domain::{
    ChangeSeq, ContentRevision, MetaRevision, NodeId, OperationId, WorkspaceEpoch, WorkspaceId,
};

const V4: &str = "550e8400-e29b-41d4-a716-446655440000";
const V7: &str = "018f2e6a-9ac0-7a43-97d2-f054a8be3901";

#[test]
fn uuid_roles_enforce_their_version_and_rfc_variant() {
    assert!(V4.parse::<WorkspaceId>().is_ok());
    assert!(V4.parse::<WorkspaceEpoch>().is_ok());
    assert!(V7.parse::<NodeId>().is_ok());
    assert!(V7.parse::<OperationId>().is_ok());
    assert!(V7.parse::<WorkspaceId>().is_err());
    assert!(V4.parse::<NodeId>().is_err());
    assert!(V4.parse::<OperationId>().is_err());
    assert!(
        "018f2e6a-9ac0-7a43-17d2-f054a8be3901"
            .parse::<NodeId>()
            .is_err()
    );
    assert!(
        "00000000-0000-0000-0000-000000000000"
            .parse::<NodeId>()
            .is_err()
    );
}

#[test]
fn untrusted_json_cannot_bypass_id_validation() {
    assert!(serde_json::from_str::<NodeId>(&format!("\"{V4}\"")).is_err());
    let id: NodeId = serde_json::from_str(&format!("\"{V7}\"")).unwrap();
    assert_eq!(serde_json::to_string(&id).unwrap(), format!("\"{V7}\""));
}

#[test]
fn revisions_are_nonempty_opaque_values_compared_only_for_equality() {
    assert!(ContentRevision::new("").is_err());
    assert!(MetaRevision::new("").is_err());
    assert!(serde_json::from_str::<ContentRevision>("\"\"").is_err());
    let before = ContentRevision::new("opaque:A:1").unwrap();
    let after = ContentRevision::new("opaque:A:2").unwrap();
    assert_ne!(before, after);
    assert_eq!(before.as_str(), "opaque:A:1");
}

#[test]
fn sequence_preserves_values_above_javascript_integer_precision() {
    let value = ChangeSeq::new(9_007_199_254_740_993);
    assert_eq!(
        serde_json::to_string(&value).unwrap(),
        "\"9007199254740993\""
    );
    assert_eq!(
        serde_json::from_str::<ChangeSeq>("\"9007199254740993\"").unwrap(),
        value
    );
    assert_eq!(
        serde_json::from_str::<ChangeSeq>("\"18446744073709551615\"")
            .unwrap()
            .get(),
        u64::MAX
    );
}

#[test]
fn sequence_accepts_only_decimal_strings_in_range() {
    for value in [
        "1",
        "-1",
        "1.0",
        "null",
        "\"+1\"",
        "\"-1\"",
        "\"1.0\"",
        "\"1e2\"",
        "\" 1\"",
        "\"18446744073709551616\"",
    ] {
        assert!(serde_json::from_str::<ChangeSeq>(value).is_err(), "{value}");
    }
    assert_eq!(serde_json::from_str::<ChangeSeq>("\"0\"").unwrap().get(), 0);
}
