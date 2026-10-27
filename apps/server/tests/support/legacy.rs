use sqlx::{Connection, SqliteConnection};

pub async fn nodes_before_deletion(connection: &mut SqliteConnection) {
    sqlx::raw_sql("DROP TABLE IF EXISTS uploads; DROP TABLE IF EXISTS recovery_objects; DROP TABLE IF EXISTS recovery_cases;")
        .execute(&mut *connection)
        .await
        .unwrap();
    let deleted: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM pragma_table_info('nodes') WHERE name='deleted'")
            .fetch_one(&mut *connection)
            .await
            .unwrap();
    if deleted == 0 {
        return;
    }
    let tombstones: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM nodes WHERE deleted<>0")
        .fetch_one(&mut *connection)
        .await
        .unwrap();
    assert_eq!(tombstones, 0);
    sqlx::query("PRAGMA foreign_keys=OFF")
        .execute(&mut *connection)
        .await
        .unwrap();
    let mut tx = connection.begin().await.unwrap();
    sqlx::raw_sql("CREATE TABLE legacy_nodes (
        node_id TEXT PRIMARY KEY NOT NULL, parent_id TEXT REFERENCES nodes(node_id),
        name TEXT NOT NULL, normalized_name_key TEXT NOT NULL, kind TEXT NOT NULL,
        content_rev TEXT, meta_rev TEXT NOT NULL, content_hash TEXT, size TEXT,
        children_generation TEXT NOT NULL, last_change_seq TEXT NOT NULL,
        UNIQUE(parent_id, normalized_name_key));
        INSERT INTO legacy_nodes SELECT node_id,parent_id,name,normalized_name_key,kind,content_rev,meta_rev,content_hash,size,children_generation,last_change_seq FROM nodes;
        DROP TABLE nodes;
        ALTER TABLE legacy_nodes RENAME TO nodes;")
        .execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    sqlx::query("PRAGMA foreign_keys=ON")
        .execute(&mut *connection)
        .await
        .unwrap();
    assert!(
        sqlx::query("PRAGMA foreign_key_check")
            .fetch_all(connection)
            .await
            .unwrap()
            .is_empty()
    );
}
