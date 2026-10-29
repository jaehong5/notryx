use sha2::{Digest, Sha256};
use sqlx::SqliteConnection;

pub async fn record_fixture_baseline(db: &mut SqliteConnection, floor: &str) {
    assert_eq!(floor.parse::<u64>().unwrap().to_string(), floor);
    let rows: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT node_id,seq,post_state FROM journal_baseline ORDER BY node_id COLLATE BINARY",
    )
    .fetch_all(&mut *db)
    .await
    .unwrap();
    assert!(!rows.is_empty());
    let mut digest = Sha256::new();
    fn add(digest: &mut Sha256, value: &[u8]) {
        digest.update(u64::try_from(value.len()).unwrap().to_be_bytes());
        digest.update(value);
    }
    add(&mut digest, b"notryx-journal-baseline-v1");
    add(&mut digest, floor.as_bytes());
    for (node_id, seq, post_state) in &rows {
        add(&mut digest, node_id.as_bytes());
        add(&mut digest, seq.as_bytes());
        add(&mut digest, post_state.as_bytes());
    }
    let hash: String = digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let changed = sqlx::query("UPDATE workspaces SET journal_floor_seq=?,journal_baseline_count=?,journal_baseline_hash=? WHERE singleton=1")
        .bind(floor)
        .bind(i64::try_from(rows.len()).unwrap())
        .bind(hash)
        .execute(&mut *db)
        .await
        .unwrap();
    assert_eq!(changed.rows_affected(), 1);
}
