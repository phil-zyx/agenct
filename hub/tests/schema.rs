//! Offline storage tests: no listener, runtime dispatch or provider credentials.
use agenct_hub::{Config, Hub, Runtime, UserConfig};
use rusqlite::Connection;
use std::path::PathBuf;

struct Database(PathBuf);

impl Database {
    fn new() -> Self {
        let directory =
            std::env::temp_dir().join(format!("agenct-schema-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        Self(directory.join("hub.db"))
    }

    fn config(&self, owner: &str) -> Config {
        Config {
            bind: "127.0.0.1:0".parse().unwrap(),
            database: self.0.clone(),
            users: vec![UserConfig {
                id: owner.into(),
                token_sha256: "a".repeat(64),
            }],
            runtime: Runtime::Mock,
        }
    }
}

impl Drop for Database {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(self.0.parent().unwrap());
    }
}

fn status(db: &Connection, sql: &str) -> String {
    db.query_row(sql, [], |row| row.get(0)).unwrap()
}

#[test]
fn hub_rejects_future_version_without_wal_schema_session_or_recovery_writes() {
    let file = Database::new();
    drop(Hub::open(file.config("alice")).unwrap());
    let db = Connection::open(&file.0).unwrap();
    db.execute_batch(
        "PRAGMA journal_mode=DELETE;
         INSERT INTO messages VALUES (1,'alice','user','pending message','pending');
         INSERT INTO chat_turns(owner,request_id,message,runtime_profile,status,message_id)
             VALUES ('alice','receipt','pending message','{}','pending',1);
         PRAGMA user_version=999;",
    )
    .unwrap();
    drop(db);
    let before = std::fs::read(&file.0).unwrap();

    let error = Hub::open(file.config("bob")).err().unwrap().to_string();
    assert!(error.contains("schema version 999"), "{error}");
    assert_eq!(std::fs::read(&file.0).unwrap(), before);

    let db = Connection::open(&file.0).unwrap();
    assert_eq!(status(&db, "PRAGMA journal_mode"), "delete");
    assert_eq!(status(&db, "SELECT status FROM messages"), "pending");
    assert_eq!(status(&db, "SELECT status FROM chat_turns"), "pending");
    assert_eq!(status(&db, "SELECT owner FROM sessions"), "alice");
    assert_eq!(
        db.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        999
    );
}

#[test]
fn repeated_hub_opens_recover_new_pending_work_without_remigrating() {
    let file = Database::new();
    drop(Hub::open(file.config("alice")).unwrap());
    let db = Connection::open(&file.0).unwrap();
    let version: i64 = db
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert!(version > 0);
    db.execute_batch(
        "INSERT INTO messages VALUES (1,'alice','user','tracked','pending');
         INSERT INTO messages VALUES (2,'alice','user','untracked','pending');
         INSERT INTO messages VALUES (3,'alice','assistant','finished','complete');
         INSERT INTO chat_turns(owner,request_id,message,runtime_profile,status,message_id)
             VALUES ('alice','receipt','tracked','{}','pending',1);",
    )
    .unwrap();
    drop(db);
    for _ in 0..2 {
        drop(Hub::open(file.config("alice")).unwrap());
        let db = Connection::open(&file.0).unwrap();
        assert_eq!(
            status(&db, "SELECT status FROM messages WHERE id=1"),
            "uncertain"
        );
        assert_eq!(
            status(&db, "SELECT status FROM messages WHERE id=2"),
            "interrupted"
        );
        assert_eq!(
            status(&db, "SELECT status FROM messages WHERE id=3"),
            "complete"
        );
        assert_eq!(status(&db, "SELECT status FROM chat_turns"), "uncertain");
        assert_eq!(
            db.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            version
        );
        db.execute_batch(
            "UPDATE messages SET status='pending' WHERE id IN (1,2);
             UPDATE chat_turns SET status='pending';",
        )
        .unwrap();
    }
}
