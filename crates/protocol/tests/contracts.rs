use notryx_domain::{ContentRevision, MetaRevision};
use notryx_protocol::{
    ContentPrecondition, HeadHint, M0SaveRequest, MetadataPrecondition, StrongEtag,
    verify_content_digest,
};

#[test]
fn protocol_contract_mismatch_never_silently_downgrades() {
    assert!(notryx_protocol::require_contract("mdw-sync/1.2").is_ok());
    for unsupported in ["mdw-sync/1.1", "mdw-sync/1.3", "", " mdw-sync/1.2"] {
        assert!(notryx_protocol::require_contract(unsupported).is_err());
    }
}

#[test]
fn one_strong_opaque_etag_is_preserved_verbatim() {
    let supplied = "\"opaque:content:A-7\"";
    assert_eq!(StrongEtag::parse(supplied).unwrap().as_str(), supplied);
    for invalid in [
        "opaque",
        "W/\"opaque\"",
        "*",
        "\"a\", \"b\"",
        "\"line\nbreak\"",
        "\"a\"b\"",
        " \"a\"",
        "\"a\" ",
    ] {
        assert!(StrongEtag::parse(invalid).is_err(), "{invalid:?}");
    }
}

#[test]
fn typed_preconditions_do_not_confuse_content_and_metadata() {
    let content = ContentRevision::new("content-A").unwrap();
    let metadata = MetaRevision::new("metadata-B").unwrap();
    let content_condition = ContentPrecondition::parse("\"content-A\"").unwrap();
    let metadata_condition = MetadataPrecondition::parse("\"metadata-B\"").unwrap();
    assert!(content_condition.matches(&content).unwrap());
    assert!(metadata_condition.matches(&metadata).unwrap());
    assert!(
        !content_condition
            .matches(&ContentRevision::new("content-C").unwrap())
            .unwrap()
    );
    assert!(
        !metadata_condition
            .matches(&MetaRevision::new("metadata-C").unwrap())
            .unwrap()
    );
    assert_eq!(content_condition.etag().as_str(), "\"content-A\"");
}

#[test]
fn revisions_that_cannot_be_etags_are_errors_not_silent_precondition_mismatches() {
    for invalid in [
        "bad\"revision",
        "bad\nrevision",
        "bad revision",
        "bad\rrevision",
        "bad\trevision",
    ] {
        let content = ContentRevision::new(invalid).unwrap();
        let metadata = MetaRevision::new(invalid).unwrap();
        assert!(
            StrongEtag::from_content_revision(&content).is_err(),
            "{invalid:?}"
        );
        assert!(
            StrongEtag::from_meta_revision(&metadata).is_err(),
            "{invalid:?}"
        );
        assert!(
            ContentPrecondition::parse("\"valid\"")
                .unwrap()
                .matches(&content)
                .is_err()
        );
        assert!(
            MetadataPrecondition::parse("\"valid\"")
                .unwrap()
                .matches(&metadata)
                .is_err()
        );
    }
    let content = ContentRevision::new("opaque:A:1").unwrap();
    let metadata = MetaRevision::new("opaque:M:1").unwrap();
    assert_eq!(
        StrongEtag::from_content_revision(&content)
            .unwrap()
            .as_str(),
        "\"opaque:A:1\""
    );
    assert_eq!(
        StrongEtag::from_meta_revision(&metadata).unwrap().as_str(),
        "\"opaque:M:1\""
    );
}

#[test]
fn sha256_digest_checks_exact_bytes_and_structured_field_encoding() {
    let digest = "sha-256=:ungWv48Bz+pBQUDeXa4iI7ADYaOWF3qctBD/YfIAFa0=:";
    assert!(verify_content_digest(digest, b"abc").is_ok());
    assert!(verify_content_digest(digest, b"abc\n").is_err());
    for invalid in [
        "sha256=:ungWv48Bz+pBQUDeXa4iI7ADYaOWF3qctBD/YfIAFa0=:",
        "sha-256=ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
        "sha-256=:AAAA:",
        "sha-256=:ungWv48Bz-pBQUDeXa4iI7ADYaOWF3qctBD_YfIAFa0=:",
    ] {
        assert!(verify_content_digest(invalid, b"abc").is_err(), "{invalid}");
    }
}

#[test]
fn head_hint_requires_valid_epoch_decimal_sequence_and_declared_fields() {
    let valid = r#"{"workspace_epoch":"550e8400-e29b-41d4-a716-446655440000","head_seq":"9007199254740993"}"#;
    let hint: HeadHint = serde_json::from_str(valid).unwrap();
    assert_eq!(hint.head_seq.get(), 9_007_199_254_740_993);
    assert_eq!(serde_json::to_string(&hint).unwrap(), valid);
    assert!(
        serde_json::from_str::<HeadHint>(
            &valid.replace("\"9007199254740993\"", "9007199254740993")
        )
        .is_err()
    );
    assert!(serde_json::from_str::<HeadHint>(&valid.replace("41d4", "71d4")).is_err());
    assert!(
        serde_json::from_str::<HeadHint>(&valid.replace('}', ",\"body\":\"unexpected\"}")).is_err()
    );
}

#[test]
fn m0_ipc_generation_must_be_a_decimal_string() {
    let valid = r#"{"document_id":"sample","edit_session_id":"session","generation":"18446744073709551615","bytes_base64":"YWJj"}"#;
    let saved: M0SaveRequest = serde_json::from_str(valid).unwrap();
    assert_eq!(saved.generation, u64::MAX.to_string());
    for invalid in [
        "\"+1\"",
        "\"-1\"",
        "\"1.1\"",
        "\"1e2\"",
        "1",
        "\"18446744073709551616\"",
    ] {
        assert!(
            serde_json::from_str::<M0SaveRequest>(
                &valid.replace("\"18446744073709551615\"", invalid)
            )
            .is_err(),
            "{invalid}"
        );
    }
    assert!(
        serde_json::from_str::<M0SaveRequest>(&valid.replace('}', ",\"path\":\"forbidden\"}"))
            .is_err()
    );
}
