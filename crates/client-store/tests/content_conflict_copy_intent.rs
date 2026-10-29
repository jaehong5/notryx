include!("content_conflict_intent.rs");

const COPY_NAME: &str = "document-copy.md";

async fn record_copy(
    store: &mut ClientStore,
    rig: &Rig,
    request: &ContentConflictIntentRequest,
    parent_id: &str,
    name: &str,
) -> Result<notryx_client_store::ContentConflictCopyIntent, StoreError> {
    store
        .record_content_conflict_copy(
            &rig.binding,
            DOC,
            &rig.operation,
            request,
            parent_id,
            name,
            None,
        )
        .await
}

fn copy_request(rig: &Rig) -> ContentConflictIntentRequest {
    let mut request = rig.request.clone();
    request.choice = ContentConflictChoice::KeepLocalCopy;
    request
}

async fn next_sequence(rig: &Rig) -> i64 {
    let mut connection = connect(&rig.path).await;
    sqlx::query_scalar("SELECT s.next_seq FROM outbox_streams s JOIN client_registrations r ON r.id=s.registration_id WHERE r.status='active'")
        .fetch_one(&mut connection)
        .await
        .unwrap()
}

async fn assert_no_copy(store: &mut ClientStore, rig: &Rig) {
    assert!(
        store
            .read_content_conflict_copy(&rig.binding, &rig.operation)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .read_content_conflict_intent(&rig.binding, &rig.operation)
            .await
            .unwrap()
            .is_none()
    );
    assert_originals(store, rig).await;
}

#[tokio::test]
async fn copy_confirmation_is_durable_idempotent_and_does_not_enqueue_or_change_original() {
    let (rig, mut store) = rig().await;
    let request = copy_request(&rig);
    let before_sequence = next_sequence(&rig).await;
    let copy = record_copy(&mut store, &rig, &request, ROOT, COPY_NAME)
        .await
        .unwrap();
    assert_eq!(copy.binding, rig.binding);
    assert_eq!(copy.document_id, DOC);
    assert_eq!(copy.conflict_operation_id, rig.operation);
    assert_eq!(copy.intent_version, 0);
    assert_eq!(copy.generation, 2);
    assert_eq!(copy.parent_id, ROOT);
    assert_eq!(copy.name, COPY_NAME);
    assert_eq!(copy.local_bytes, LOCAL);
    assert_ne!(copy.node_id, DOC);
    assert_ne!(copy.create_operation_id, rig.operation);
    assert_ne!(copy.node_id, copy.create_operation_id);
    for id in [&copy.node_id, &copy.create_operation_id] {
        assert_eq!(Uuid::parse_str(id).unwrap().get_version_num(), 7);
    }
    let recorded = store
        .read_content_conflict_intent(&rig.binding, &rig.operation)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(recorded.choice, ContentConflictChoice::KeepLocalCopy);
    assert_eq!(recorded.local_bytes, LOCAL);
    assert_eq!(recorded.base_bytes, BASE);
    assert_eq!(recorded.remote_bytes, REMOTE);
    assert_eq!(
        record_copy(&mut store, &rig, &request, ROOT, COPY_NAME)
            .await
            .unwrap(),
        copy
    );
    assert_eq!(
        store
            .list_outbox(&rig.binding, 0, 100)
            .await
            .unwrap()
            .entries
            .len(),
        2
    );
    assert_eq!(next_sequence(&rig).await, before_sequence);
    assert_originals(&mut store, &rig).await;
    store.close().await.unwrap();

    let mut reopened = ClientStore::open(&rig.path).await.unwrap();
    assert_eq!(
        reopened
            .read_content_conflict_copy(&rig.binding, &rig.operation)
            .await
            .unwrap(),
        Some(copy.clone())
    );
    assert_eq!(
        record_copy(&mut reopened, &rig, &request, ROOT, COPY_NAME)
            .await
            .unwrap(),
        copy
    );
    assert_eq!(
        reopened
            .list_outbox(&rig.binding, 0, 100)
            .await
            .unwrap()
            .entries
            .len(),
        2
    );
    assert_eq!(next_sequence(&rig).await, before_sequence);
    assert_originals(&mut reopened, &rig).await;
}

#[tokio::test]
async fn changing_confirmed_destination_or_payload_is_rejected_without_replacing_copy() {
    let (rig, mut store) = rig().await;
    let request = copy_request(&rig);
    let copy = record_copy(&mut store, &rig, &request, ROOT, COPY_NAME)
        .await
        .unwrap();
    assert!(
        record_copy(&mut store, &rig, &request, ROOT, "another.md")
            .await
            .is_err()
    );
    assert!(
        record_copy(&mut store, &rig, &request, OTHER, COPY_NAME)
            .await
            .is_err()
    );
    let mut altered = request.clone();
    altered.generation += 1;
    assert!(
        record_copy(&mut store, &rig, &altered, ROOT, COPY_NAME)
            .await
            .is_err()
    );
    assert_eq!(
        store
            .read_content_conflict_copy(&rig.binding, &rig.operation)
            .await
            .unwrap(),
        Some(copy.clone())
    );
    assert_eq!(
        record_copy(&mut store, &rig, &request, ROOT, COPY_NAME)
            .await
            .unwrap(),
        copy
    );
    assert_originals(&mut store, &rig).await;
}

#[tokio::test]
async fn copy_confirmation_requires_matching_intent_and_valid_destination() {
    for case in ["choice", "generation", "parent", "name"] {
        let (rig, mut store) = rig().await;
        let mut request = copy_request(&rig);
        let mut parent = ROOT;
        let mut name = COPY_NAME;
        match case {
            "choice" => request.choice = ContentConflictChoice::ChooseRemote,
            "generation" => request.generation += 1,
            "parent" => parent = DOC,
            "name" => name = "../invalid.md",
            _ => unreachable!(),
        }
        assert!(
            record_copy(&mut store, &rig, &request, parent, name)
                .await
                .is_err(),
            "{case}"
        );
        assert_no_copy(&mut store, &rig).await;
    }
}

#[tokio::test]
async fn copy_insert_failure_rolls_back_choice_and_preserves_original_data() {
    let (rig, mut store) = rig().await;
    let mut connection = connect(&rig.path).await;
    sqlx::query("CREATE TRIGGER fail_copy BEFORE INSERT ON content_conflict_copies BEGIN SELECT RAISE(ABORT, 'injected copy failure'); END")
        .execute(&mut connection).await.unwrap();
    let request = copy_request(&rig);
    assert!(
        record_copy(&mut store, &rig, &request, ROOT, COPY_NAME)
            .await
            .is_err()
    );
    assert_no_copy(&mut store, &rig).await;
    sqlx::query("DROP TRIGGER fail_copy")
        .execute(&mut connection)
        .await
        .unwrap();
    drop(connection);
    store.close().await.unwrap();
    let mut reopened = ClientStore::open(&rig.path).await.unwrap();
    assert_no_copy(&mut reopened, &rig).await;
    let copy = record_copy(&mut reopened, &rig, &request, ROOT, COPY_NAME)
        .await
        .unwrap();
    assert_eq!(copy.local_bytes, LOCAL);
    assert_originals(&mut reopened, &rig).await;
}
