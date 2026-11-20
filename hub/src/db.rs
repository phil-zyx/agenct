//! Single-source-of-truth SQLite v1 store with WAL + spawn_blocking isolation.
use crate::{Error, Result};
use axum::http::StatusCode;
use fs2::FileExt;
use rusqlite::{Connection, OpenFlags};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

const SCHEMA_VERSION: i64 = 6;

const SCHEMA_V6: &str = r#"
CREATE TABLE IF NOT EXISTS users (
    id TEXT PRIMARY KEY,
    gitlab_id INTEGER UNIQUE,
    username TEXT NOT NULL UNIQUE,
    display_name TEXT NOT NULL,
    is_admin INTEGER NOT NULL DEFAULT 0 CHECK(is_admin IN (0,1)),
    status TEXT NOT NULL DEFAULT 'active' CHECK(status IN ('active','disabled')),
    created_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS sessions (
    session_hash TEXT PRIMARY KEY,
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    csrf_token TEXT NOT NULL,
    expires_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_sessions_expires ON sessions(expires_at);

CREATE TABLE IF NOT EXISTS agent_profiles (
    user_id TEXT PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    soul_md TEXT NOT NULL,
    memory_md TEXT NOT NULL,
    avatar_id TEXT NOT NULL DEFAULT 'agenct',
    rolling_summary TEXT NOT NULL DEFAULT '',
    compacted_msg_id INTEGER NOT NULL DEFAULT 0,
    updated_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS threads (
    id TEXT PRIMARY KEY,
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    title TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'active' CHECK(status IN ('active','archived')),
    summary TEXT NOT NULL DEFAULT '',
    node_id TEXT,
    node_session_id TEXT,
    turn_count INTEGER NOT NULL DEFAULT 0,
    updated_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_threads_user ON threads(user_id, status, updated_at DESC);

CREATE TABLE IF NOT EXISTS ledgers (
    id TEXT PRIMARY KEY,
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    title TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'active' CHECK(status IN ('active','muted','done')),
    notes TEXT NOT NULL DEFAULT '',
    updated_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_ledgers_user ON ledgers(user_id, updated_at DESC);

CREATE TABLE IF NOT EXISTS messages (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    peer_id TEXT REFERENCES users(id) ON DELETE SET NULL,
    thread_id TEXT,
    role TEXT NOT NULL CHECK(role IN ('user','assistant','system')),
    content TEXT NOT NULL,
    request_id TEXT,
    status TEXT NOT NULL DEFAULT 'complete' CHECK(status IN ('pending','complete','failed')),
    reply_to INTEGER,
    context TEXT NOT NULL DEFAULT '',
    created_at INTEGER NOT NULL,
    UNIQUE(user_id, request_id)
);
CREATE INDEX IF NOT EXISTS idx_messages_user ON messages(user_id, id DESC);
CREATE INDEX IF NOT EXISTS idx_messages_thread ON messages(user_id, thread_id, id DESC);
CREATE INDEX IF NOT EXISTS idx_messages_reply ON messages(user_id, reply_to);

CREATE TABLE IF NOT EXISTS tasks (
    id TEXT PRIMARY KEY,
    requester_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    owner_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    thread_id TEXT,
    title TEXT NOT NULL,
    instruction TEXT NOT NULL,
    status TEXT NOT NULL CHECK(status IN ('pending','running','done','failed','canceled')),
    result TEXT,
    revision INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_tasks_owner ON tasks(owner_id, updated_at DESC);
CREATE INDEX IF NOT EXISTS idx_tasks_requester ON tasks(requester_id, updated_at DESC);

CREATE TABLE IF NOT EXISTS nodes (
    id TEXT NOT NULL,
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    kind TEXT NOT NULL DEFAULT 'pc' CHECK(kind IN ('pc','cloud')),
    capabilities TEXT NOT NULL DEFAULT '',
    token_hash TEXT NOT NULL UNIQUE,
    last_seen_at INTEGER,
    created_at INTEGER NOT NULL,
    PRIMARY KEY(user_id, id)
);

CREATE TABLE IF NOT EXISTS node_jobs (
    task_id TEXT PRIMARY KEY REFERENCES tasks(id) ON DELETE CASCADE,
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    node_id TEXT NOT NULL,
    task_revision INTEGER NOT NULL,
    authorization TEXT NOT NULL DEFAULT 'manual' CHECK(authorization IN ('manual','delegation','gitlab','cron')),
    status TEXT NOT NULL CHECK(status IN ('queued','running','completed','failed')),
    lease_hash TEXT,
    lease_until INTEGER,
    submission_hash TEXT,
    stage TEXT NOT NULL DEFAULT '',
    log_tail TEXT NOT NULL DEFAULT '',
    updated_at INTEGER NOT NULL,
    FOREIGN KEY(user_id, node_id) REFERENCES nodes(user_id, id) ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS idx_node_jobs_poll ON node_jobs(user_id, node_id, status, updated_at);

CREATE TABLE IF NOT EXISTS automation_permissions (
    user_id TEXT PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
    auto_accept INTEGER NOT NULL DEFAULT 0 CHECK(auto_accept IN (0,1)),
    auto_deliver INTEGER NOT NULL DEFAULT 0 CHECK(auto_deliver IN (0,1)),
    gitlab_execute INTEGER NOT NULL DEFAULT 0 CHECK(gitlab_execute IN (0,1)),
    gitlab_reply INTEGER NOT NULL DEFAULT 0 CHECK(gitlab_reply IN (0,1)),
    updated_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS automation_rules (
    id TEXT PRIMARY KEY,
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    keyword TEXT NOT NULL,
    target_node_id TEXT NOT NULL,
    require_human INTEGER NOT NULL DEFAULT 0 CHECK(require_human IN (0,1)),
    created_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS cron_jobs (
    id TEXT PRIMARY KEY,
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    interval_minutes INTEGER NOT NULL CHECK(interval_minutes BETWEEN 1 AND 10080),
    daily_time_cst TEXT,
    instruction TEXT NOT NULL,
    target TEXT NOT NULL DEFAULT 'cloud',
    enabled INTEGER NOT NULL DEFAULT 1 CHECK(enabled IN (0,1)),
    last_run_at INTEGER,
    last_status TEXT NOT NULL DEFAULT '',
    last_error TEXT NOT NULL DEFAULT '',
    fail_count INTEGER NOT NULL DEFAULT 0,
    next_retry_at INTEGER,
    created_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS integrations (
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    kind TEXT NOT NULL CHECK(kind IN ('gitlab','im')),
    config_json TEXT NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY(user_id, kind)
);

CREATE TABLE IF NOT EXISTS gitlab_reply_targets (
    task_id TEXT PRIMARY KEY REFERENCES tasks(id) ON DELETE CASCADE,
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    base_url TEXT NOT NULL,
    project_path TEXT NOT NULL,
    noteable_type TEXT NOT NULL,
    noteable_iid INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS user_models (
    user_id TEXT PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
    config_json TEXT NOT NULL,
    updated_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS attachments (
    id TEXT PRIMARY KEY,
    owner_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    mime_type TEXT NOT NULL,
    size_bytes INTEGER NOT NULL CHECK(size_bytes > 0),
    created_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS attachment_grants (
    attachment_id TEXT NOT NULL REFERENCES attachments(id) ON DELETE CASCADE,
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    PRIMARY KEY(attachment_id, user_id)
);

CREATE TABLE IF NOT EXISTS audit_logs (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    action TEXT NOT NULL,
    status TEXT NOT NULL,
    title TEXT NOT NULL,
    detail TEXT NOT NULL,
    created_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_audit_user ON audit_logs(user_id, id DESC);

CREATE TRIGGER IF NOT EXISTS audit_logs_no_update BEFORE UPDATE ON audit_logs
BEGIN SELECT RAISE(ABORT, 'audit_logs are append-only'); END;
CREATE TRIGGER IF NOT EXISTS audit_logs_no_delete BEFORE DELETE ON audit_logs
BEGIN SELECT RAISE(ABORT, 'audit_logs are append-only'); END;
"#;

#[derive(Clone)]
pub struct Db {
    writer: Arc<Mutex<Connection>>,
    reader: Option<Arc<Mutex<Connection>>>,
    _lock: Option<Arc<std::fs::File>>,
    path: PathBuf,
}

impl Db {
    pub fn open(path: &Path) -> std::result::Result<Self, Box<dyn std::error::Error>> {
        let is_memory = path.to_str() == Some(":memory:");
        let lock = if !is_memory {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mut opts = std::fs::OpenOptions::new();
            opts.read(true).write(true).create(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                opts.mode(0o600);
            }
            let guard = opts.open(path.with_extension("lock"))?;
            guard
                .try_lock_exclusive()
                .map_err(|_| "database already in use by another Hub process")?;
            Some(Arc::new(guard))
        } else {
            None
        };

        let writer = Connection::open(path)?;
        writer.busy_timeout(Duration::from_millis(5000))?;
        let mut current_version: i64 = writer.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if !matches!(current_version, 0 | 1 | 2 | 3 | 4 | 5 | SCHEMA_VERSION) {
            return Err(format!(
                "unsupported database schema version {current_version} (expected {SCHEMA_VERSION}); v2 requires a compatible database"
            )
            .into());
        }
        writer.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA foreign_keys = ON;",
        )?;
        if current_version == 0 {
            writer.execute_batch(SCHEMA_V6)?;
            writer.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        } else {
            if current_version == 1 {
                writer.execute_batch(
                    "BEGIN IMMEDIATE;
                     ALTER TABLE agent_profiles ADD COLUMN rolling_summary TEXT NOT NULL DEFAULT '';
                     ALTER TABLE agent_profiles ADD COLUMN compacted_msg_id INTEGER NOT NULL DEFAULT 0;
                     ALTER TABLE messages ADD COLUMN thread_id TEXT;
                     ALTER TABLE tasks ADD COLUMN thread_id TEXT;
                     CREATE TABLE IF NOT EXISTS threads (
                         id TEXT PRIMARY KEY,
                         user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
                         title TEXT NOT NULL,
                         status TEXT NOT NULL DEFAULT 'active' CHECK(status IN ('active','archived')),
                         summary TEXT NOT NULL DEFAULT '',
                         node_id TEXT,
                         node_session_id TEXT,
                         turn_count INTEGER NOT NULL DEFAULT 0,
                         updated_at INTEGER NOT NULL
                     );
                     CREATE INDEX IF NOT EXISTS idx_threads_user ON threads(user_id, status, updated_at DESC);
                     CREATE INDEX IF NOT EXISTS idx_messages_thread ON messages(user_id, thread_id, id DESC);
                     PRAGMA user_version = 2;
                     COMMIT;",
                )?;
                current_version = 2;
            }
            if current_version == 2 {
                writer.execute_batch(
                    "BEGIN IMMEDIATE;
                     ALTER TABLE agent_profiles ADD COLUMN avatar_id TEXT NOT NULL DEFAULT 'agenct';
                     PRAGMA user_version = 3;
                     COMMIT;",
                )?;
                current_version = 3;
            }
            if current_version == 3 {
                writer.execute_batch(
                    "BEGIN IMMEDIATE;
                     ALTER TABLE messages ADD COLUMN reply_to INTEGER;
                     CREATE INDEX IF NOT EXISTS idx_messages_reply ON messages(user_id, reply_to);
                     PRAGMA user_version = 4;
                     COMMIT;",
                )?;
                current_version = 4;
            }
            if current_version == 4 {
                let has_crons: bool = writer.query_row(
                    "SELECT count(*) > 0 FROM sqlite_master WHERE type='table' AND name='cron_jobs'",
                    [],
                    |r| r.get(0),
                )?;
                let alter = if has_crons {
                    "ALTER TABLE cron_jobs ADD COLUMN last_status TEXT NOT NULL DEFAULT '';
                     ALTER TABLE cron_jobs ADD COLUMN last_error TEXT NOT NULL DEFAULT '';
                     ALTER TABLE cron_jobs ADD COLUMN fail_count INTEGER NOT NULL DEFAULT 0;
                     ALTER TABLE cron_jobs ADD COLUMN next_retry_at INTEGER;"
                } else {
                    ""
                };
                writer.execute_batch(&format!(
                    "BEGIN IMMEDIATE; {alter} PRAGMA user_version = 5; COMMIT;"
                ))?;
                current_version = 5;
            }
            if current_version == 5 {
                // `context` holds model-only text derived from a message (e.g. image captions).
                // `user_models` is created by the idempotent schema batch below.
                writer.execute_batch(
                    "BEGIN IMMEDIATE;
                     ALTER TABLE messages ADD COLUMN context TEXT NOT NULL DEFAULT '';
                     PRAGMA user_version = 6;
                     COMMIT;",
                )?;
            }
            // Idempotent: creates any table/index/trigger an older database never had.
            writer.execute_batch(SCHEMA_V6)?;
        }
        // On startup, mark any orphaned running node jobs without a valid future lease as failed.
        let now = crate::config::now_unix();
        writer.execute(
            "UPDATE node_jobs SET status='failed', updated_at=?1
             WHERE status='running' AND (lease_until IS NULL OR lease_until <= ?1)",
            [now],
        )?;
        // A chat turn still `pending` at startup was interrupted by a crash/restart: mark it failed
        // so the client can retry with the same request_id instead of hanging forever.
        writer.execute(
            "UPDATE messages SET status='failed' WHERE status='pending'",
            [],
        )?;
        // Cron runs interrupted by a restart are failed and retried right away.
        writer.execute(
            "UPDATE cron_jobs SET last_status='failed', last_error='interrupted by restart',
                    fail_count=fail_count+1, next_retry_at=?1
             WHERE last_status='running'",
            [now],
        )?;

        let reader = if !is_memory {
            let conn = Connection::open_with_flags(
                path,
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )?;
            conn.busy_timeout(Duration::from_millis(5000))?;
            conn.execute_batch("PRAGMA query_only = ON;")?;
            Some(Arc::new(Mutex::new(conn)))
        } else {
            None
        };

        Ok(Self {
            writer: Arc::new(Mutex::new(writer)),
            reader,
            _lock: lock,
            path: path.to_path_buf(),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Execute a read-only query off the Tokio async worker pool.
    pub async fn read<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&Connection) -> Result<T> + Send + 'static,
    {
        let conn = self.reader.clone().unwrap_or_else(|| self.writer.clone());
        tokio::task::spawn_blocking(move || {
            let guard = conn
                .lock()
                .map_err(|_| Error(StatusCode::INTERNAL_SERVER_ERROR, "storage unavailable"))?;
            f(&guard)
        })
        .await
        .map_err(|_| Error(StatusCode::INTERNAL_SERVER_ERROR, "storage task failed"))?
    }

    /// Execute a write/transaction off the Tokio async worker pool.
    pub async fn write<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T> + Send + 'static,
    {
        let conn = self.writer.clone();
        tokio::task::spawn_blocking(move || {
            let mut guard = conn
                .lock()
                .map_err(|_| Error(StatusCode::INTERNAL_SERVER_ERROR, "storage unavailable"))?;
            f(&mut guard)
        })
        .await
        .map_err(|_| Error(StatusCode::INTERNAL_SERVER_ERROR, "storage task failed"))?
    }
}

pub fn record_audit(
    conn: &Connection,
    user_id: &str,
    action: &str,
    status: &str,
    title: &str,
    detail: &str,
) -> Result<()> {
    conn.execute(
        "INSERT INTO audit_logs(user_id, action, status, title, detail, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![
            user_id,
            crate::config::truncate_bytes(action.trim(), 48),
            crate::config::truncate_bytes(status.trim(), 32),
            crate::config::truncate_bytes(title.trim(), 120),
            crate::config::truncate_bytes(detail.trim(), 400),
            crate::config::now_unix(),
        ],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::Db;
    use rusqlite::Connection;

    #[test]
    fn migrates_v1_database_to_v2_preserving_data() {
        let dir =
            std::env::temp_dir().join(format!("agenct-mig-test-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("agenct.sqlite3");
        {
            let conn = Connection::open(&db_path).unwrap();
            conn.execute_batch(
                "CREATE TABLE users (id TEXT PRIMARY KEY, gitlab_id INTEGER UNIQUE, username TEXT NOT NULL UNIQUE, display_name TEXT NOT NULL, is_admin INTEGER NOT NULL DEFAULT 0, status TEXT NOT NULL DEFAULT 'active', created_at INTEGER NOT NULL);
                 CREATE TABLE agent_profiles (user_id TEXT PRIMARY KEY, name TEXT NOT NULL, soul_md TEXT NOT NULL, memory_md TEXT NOT NULL, updated_at INTEGER NOT NULL);
                 CREATE TABLE messages (id INTEGER PRIMARY KEY AUTOINCREMENT, user_id TEXT NOT NULL, peer_id TEXT, role TEXT NOT NULL, content TEXT NOT NULL, request_id TEXT, status TEXT NOT NULL DEFAULT 'complete', created_at INTEGER NOT NULL);
                 CREATE TABLE tasks (id TEXT PRIMARY KEY, requester_id TEXT NOT NULL, owner_id TEXT NOT NULL, title TEXT NOT NULL, instruction TEXT NOT NULL, status TEXT NOT NULL, result TEXT, revision INTEGER NOT NULL DEFAULT 0, created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL);
                 CREATE TABLE node_jobs (task_id TEXT PRIMARY KEY, user_id TEXT NOT NULL, node_id TEXT NOT NULL, task_revision INTEGER NOT NULL, authorization TEXT NOT NULL DEFAULT 'manual', status TEXT NOT NULL, lease_hash TEXT, lease_until INTEGER, submission_hash TEXT, stage TEXT NOT NULL DEFAULT '', log_tail TEXT NOT NULL DEFAULT '', updated_at INTEGER NOT NULL);
                 INSERT INTO users VALUES ('u1', 1, 'alice', 'Alice', 1, 'active', 100);
                 INSERT INTO agent_profiles VALUES ('u1', 'Agenct', 'soul', 'mem', 100);
                 INSERT INTO messages(user_id, role, content, created_at) VALUES ('u1', 'user', 'hello v1', 100);
                 PRAGMA user_version = 1;",
            )
            .unwrap();
        }
        let db = Db::open(&db_path).unwrap();
        drop(db);
        let conn = Connection::open(&db_path).unwrap();
        let ver: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(ver, 6);
        let reply_to_cols: i64 = conn
            .query_row(
                "SELECT count(*) FROM pragma_table_info('messages') WHERE name='reply_to'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(reply_to_cols, 1);
        let avatar: String = conn
            .query_row(
                "SELECT avatar_id FROM agent_profiles WHERE user_id='u1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(avatar, "agenct");
        let content: String = conn
            .query_row("SELECT content FROM messages WHERE user_id='u1'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(content, "hello v1");
        let summary: String = conn
            .query_row(
                "SELECT rolling_summary FROM agent_profiles WHERE user_id='u1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(summary, "");
        let thr_count: i64 = conn
            .query_row("SELECT count(*) FROM threads", [], |r| r.get(0))
            .unwrap();
        assert_eq!(thr_count, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pending_chat_turns_are_failed_on_restart() {
        let dir = std::env::temp_dir().join(format!(
            "agenct-pend-test-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("agenct.sqlite3");
        drop(Db::open(&db_path).unwrap());
        {
            let conn = Connection::open(&db_path).unwrap();
            conn.execute_batch(
                "INSERT INTO users(id, username, display_name, created_at) VALUES ('u1', 'alice', 'Alice', 1);
                 INSERT INTO messages(user_id, role, content, request_id, status, created_at)
                 VALUES ('u1', 'user', 'interrupted', 'req-1', 'pending', 1);
                 INSERT INTO cron_jobs(id, user_id, interval_minutes, instruction, last_status, created_at)
                 VALUES ('crn_1', 'u1', 60, 'report', 'running', 1);",
            )
            .unwrap();
        }
        drop(Db::open(&db_path).unwrap());
        let conn = Connection::open(&db_path).unwrap();
        let status: String = conn
            .query_row(
                "SELECT status FROM messages WHERE request_id='req-1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(status, "failed");
        let (cron_status, fails, retry): (String, i64, Option<i64>) = conn
            .query_row(
                "SELECT last_status, fail_count, next_retry_at FROM cron_jobs WHERE id='crn_1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!((cron_status.as_str(), fails), ("failed", 1));
        assert!(retry.is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
