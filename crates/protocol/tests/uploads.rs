use notryx_protocol::{CreateNodeRequest, ServerInfo, UploadResponse};
use serde_json::{Value, json};
use ts_rs::{Config, TS};

const UPLOAD: &str = "550e8400-e29b-41d4-a716-446655440001";

fn response() -> Value {
    json!({"upload_id": UPLOAD, "content_hash": format!("sha256:{}", "a".repeat(64)),
        "size": "26214400", "expires_at": "1800000000"})
}

#[test]
fn upload_response_validates_identifiers_hash_and_decimal_fields() {
    let parsed: UploadResponse = serde_json::from_value(response()).unwrap();
    assert_eq!(serde_json::to_value(parsed).unwrap(), response());
    for (field, invalid) in [
        ("upload_id", "019a1000-0000-7000-8000-000000000001"),
        ("upload_id", "550e8400-e29b-41d4-7716-446655440001"),
        ("content_hash", "sha256:bad"),
        ("content_hash", "a"),
        ("size", "-1"),
        ("size", "18446744073709551616"),
        ("expires_at", "-1"),
        ("expires_at", ""),
    ] {
        let mut value = response();
        value[field] = json!(invalid);
        assert!(
            serde_json::from_value::<UploadResponse>(value).is_err(),
            "{field}"
        );
    }
    let mut value = response();
    value["content_hash"] = json!(format!("sha256:{}", "A".repeat(64)));
    assert!(serde_json::from_value::<UploadResponse>(value).is_err());
}

#[test]
fn file_create_accepts_upload_and_folder_rejects_it() {
    let request = json!({"node_id":"019a1000-0000-7000-8000-000000000001",
        "parent_id":"019a1000-0000-7000-8000-000000000002", "name":"asset.bin",
        "kind":"file", "upload_id":UPLOAD});
    let parsed: CreateNodeRequest = serde_json::from_value(request.clone()).unwrap();
    assert_eq!(serde_json::to_value(parsed).unwrap(), request);
    let mut folder = request.clone();
    folder["kind"] = json!("folder");
    assert!(serde_json::from_value::<CreateNodeRequest>(folder).is_err());
    for id in [
        "bad",
        "019a1000-0000-7000-8000-000000000001",
        "550e8400-e29b-41d4-7716-446655440001",
    ] {
        let mut value = request.clone();
        value["upload_id"] = json!(id);
        assert!(serde_json::from_value::<CreateNodeRequest>(value).is_err());
    }
    let declaration = CreateNodeRequest::decl(&Config::default());
    assert!(declaration.contains("upload_id?: string"));
}

#[test]
fn server_limits_keep_document_editing_separate_from_attachments() {
    let value = json!({"protocol":"mdw-sync/1.2", "workspace_id":UPLOAD,
        "workspace_epoch":UPLOAD, "root_id":"019a1000-0000-7000-8000-000000000001",
        "head_seq":"0", "journal_floor_seq":"0", "ready":true,
        "max_document_bytes":5242880, "max_attachment_bytes":26214400});
    let info: ServerInfo = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(serde_json::to_value(info).unwrap(), value);
    assert!(ServerInfo::decl(&Config::default()).contains("max_attachment_bytes: number"));
}
