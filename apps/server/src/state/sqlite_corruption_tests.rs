use super::db_error;
use crate::Error;
use sqlx::{
    Connection, SqliteConnection,
    error::{DatabaseError, ErrorKind},
    sqlite::SqliteConnectOptions,
};
use std::{
    borrow::Cow,
    error::Error as StdError,
    fmt,
    fs::{self, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::Path,
    time::Duration,
};

#[derive(Debug)]
struct SyntheticCode(i32);

impl fmt::Display for SyntheticCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "synthetic SQLite code {}", self.0)
    }
}

impl StdError for SyntheticCode {}

impl DatabaseError for SyntheticCode {
    fn message(&self) -> &str {
        "synthetic SQLite code"
    }

    fn code(&self) -> Option<Cow<'_, str>> {
        Some(self.0.to_string().into())
    }

    fn as_error(&self) -> &(dyn StdError + Send + Sync + 'static) {
        self
    }

    fn as_error_mut(&mut self) -> &mut (dyn StdError + Send + Sync + 'static) {
        self
    }

    fn into_error(self: Box<Self>) -> Box<dyn StdError + Send + Sync + 'static> {
        self
    }

    fn kind(&self) -> ErrorKind {
        ErrorKind::Other
    }
}

fn synthetic(code: i32) -> sqlx::Error {
    sqlx::Error::Database(Box::new(SyntheticCode(code)))
}

fn sqlite_primary(error: &sqlx::Error) -> i32 {
    error
        .as_database_error()
        .expect("expected an actual SQLite database error")
        .code()
        .expect("SQLite supplies its extended code")
        .parse::<i32>()
        .expect("SQLite code must be decimal")
        & 0xff
}

async fn connect(path: &Path) -> Result<SqliteConnection, sqlx::Error> {
    SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(path)
            .read_only(true)
            .busy_timeout(Duration::ZERO),
    )
    .await
}

async fn read_failure(path: &Path) -> sqlx::Error {
    match connect(path).await {
        Ok(mut connection) => {
            let failure = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM synthetic_records")
                .fetch_one(&mut connection)
                .await
                .expect_err("damaged SQLite input must not return a successful row");
            let _ = connection.close().await;
            failure
        }
        Err(failure) => failure,
    }
}

#[tokio::test]
async fn sqlite_corruption_mapping_rejects_actual_not_a_database_without_changing_bytes() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("not-a-database.sqlite3");
    let bytes = vec![0x5a; 512];
    fs::write(&path, &bytes).unwrap();
    let error = read_failure(&path).await;
    assert_eq!(
        sqlite_primary(&error),
        26,
        "fixture must produce SQLITE_NOTADB"
    );
    assert_eq!(fs::read(path).unwrap(), bytes);
    assert_eq!(db_error(error), Error::InvalidFormat);
}

#[tokio::test]
async fn sqlite_corruption_mapping_rejects_actual_corrupt_btree_without_changing_source() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("valid.sqlite3");
    let copy = directory.path().join("damaged-copy.sqlite3");
    let options = SqliteConnectOptions::new()
        .filename(&source)
        .create_if_missing(true);
    let mut connection = SqliteConnection::connect_with(&options).await.unwrap();
    sqlx::raw_sql("CREATE TABLE synthetic_records (value TEXT NOT NULL); INSERT INTO synthetic_records VALUES ('synthetic data');")
        .execute(&mut connection).await.unwrap();
    let page_size: i64 = sqlx::query_scalar("PRAGMA page_size")
        .fetch_one(&mut connection)
        .await
        .unwrap();
    let root_page: i64 = sqlx::query_scalar(
        "SELECT rootpage FROM sqlite_schema WHERE type='table' AND name='synthetic_records'",
    )
    .fetch_one(&mut connection)
    .await
    .unwrap();
    assert!(page_size >= 1024 && root_page > 1);
    connection.close().await.unwrap();
    let original = fs::read(&source).unwrap();
    fs::copy(&source, &copy).unwrap();
    let offset = u64::try_from((root_page - 1) * page_size).unwrap();
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&copy)
        .unwrap();
    file.seek(SeekFrom::Start(offset)).unwrap();
    let mut original_type = [0_u8; 1];
    file.read_exact(&mut original_type).unwrap();
    assert!(matches!(original_type[0], 0x05 | 0x0d));
    file.seek(SeekFrom::Start(offset)).unwrap();
    file.write_all(&[0xff]).unwrap();
    file.sync_all().unwrap();
    drop(file);
    let damaged = fs::read(&copy).unwrap();
    let error = read_failure(&copy).await;
    assert_eq!(
        sqlite_primary(&error),
        11,
        "fixture must produce SQLITE_CORRUPT"
    );
    assert_eq!(fs::read(&copy).unwrap(), damaged);
    assert_eq!(fs::read(&source).unwrap(), original);
    let mut control = connect(&source).await.unwrap();
    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM synthetic_records")
        .fetch_one(&mut control)
        .await
        .unwrap();
    assert_eq!(rows, 1);
    control.close().await.unwrap();
    assert_eq!(db_error(error), Error::InvalidFormat);
}

#[test]
fn sqlite_corruption_mapping_uses_primary_code_for_extended_results() {
    for code in [11, 267, 523, 779, 26] {
        assert_eq!(
            db_error(synthetic(code)),
            Error::InvalidFormat,
            "code {code}"
        );
    }
    for code in [5, 6, 10, 266, 778, 19] {
        assert_eq!(db_error(synthetic(code)), Error::Storage, "code {code}");
    }
    for code in [13, 269] {
        assert_eq!(db_error(synthetic(code)), Error::NoSpace, "code {code}");
    }
    assert_eq!(
        db_error(sqlx::Error::Io(std::io::Error::other("synthetic I/O"))),
        Error::Storage
    );
}

#[tokio::test]
async fn sqlite_corruption_mapping_preserves_actual_busy_as_storage() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("busy.sqlite3");
    let options = SqliteConnectOptions::new()
        .filename(&path)
        .create_if_missing(true)
        .busy_timeout(Duration::ZERO);
    let mut owner = SqliteConnection::connect_with(&options).await.unwrap();
    sqlx::query("CREATE TABLE synthetic_records (value INTEGER NOT NULL)")
        .execute(&mut owner)
        .await
        .unwrap();
    let mut other = SqliteConnection::connect_with(&options).await.unwrap();
    sqlx::query("BEGIN EXCLUSIVE")
        .execute(&mut owner)
        .await
        .unwrap();
    let error = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM synthetic_records")
        .fetch_one(&mut other)
        .await
        .expect_err("exclusive lock must prevent the second read");
    assert_eq!(
        sqlite_primary(&error),
        5,
        "fixture must produce SQLITE_BUSY"
    );
    assert_eq!(db_error(error), Error::Storage);
    sqlx::query("ROLLBACK").execute(&mut owner).await.unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM synthetic_records")
            .fetch_one(&mut other)
            .await
            .unwrap(),
        0
    );
    owner.close().await.unwrap();
    other.close().await.unwrap();
}

#[tokio::test]
async fn sqlite_corruption_mapping_preserves_actual_full_as_no_space() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("full.sqlite3");
    let options = SqliteConnectOptions::new()
        .filename(&path)
        .create_if_missing(true);
    let mut connection = SqliteConnection::connect_with(&options).await.unwrap();
    sqlx::query("CREATE TABLE synthetic_records (value BLOB NOT NULL)")
        .execute(&mut connection)
        .await
        .unwrap();
    sqlx::query("PRAGMA max_page_count=1")
        .execute(&mut connection)
        .await
        .unwrap();
    let error = sqlx::query("INSERT INTO synthetic_records VALUES (zeroblob(1048576))")
        .execute(&mut connection)
        .await
        .expect_err("limited SQLite pages must fill");
    assert_eq!(
        sqlite_primary(&error),
        13,
        "fixture must produce SQLITE_FULL"
    );
    assert_eq!(db_error(error), Error::NoSpace);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM synthetic_records")
            .fetch_one(&mut connection)
            .await
            .unwrap(),
        0
    );
    connection.close().await.unwrap();
}
