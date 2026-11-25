//! Single-source-of-truth SQLite v1 store with WAL + spawn_blocking isolation.
use crate::{Error, Result};
use axum::http::StatusCode;
use fs2::FileExt;
use rusqlite::{Connection, OpenFlags, TransactionBehavior};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

const SCHEMA_VERSION: i64 = 12;

const SCHEMA: &str = r#"
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

CREATE TABLE IF NOT EXISTS agent_roles (
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    agent TEXT NOT NULL DEFAULT '',
    model TEXT NOT NULL DEFAULT '',
    effort TEXT NOT NULL DEFAULT '',
    sandbox TEXT NOT NULL DEFAULT '' CHECK(sandbox IN ('','readonly','workspace-write')),
    node_id TEXT NOT NULL DEFAULT '',
    prompt TEXT NOT NULL DEFAULT '',
    updated_at INTEGER NOT NULL,
    PRIMARY KEY(user_id, name)
);

CREATE TABLE IF NOT EXISTS threads (
    id TEXT PRIMARY KEY,
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    title TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'active' CHECK(status IN ('active','archived')),
    summary TEXT NOT NULL DEFAULT '',
    node_id TEXT,
    node_session_id TEXT,
    node_session_agent TEXT NOT NULL DEFAULT '',
    node_session_role TEXT NOT NULL DEFAULT '',
    node_session_model TEXT NOT NULL DEFAULT '',
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
    on_complete TEXT NOT NULL DEFAULT '',
    callback_depth INTEGER NOT NULL DEFAULT 0,
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
    default_agent TEXT NOT NULL DEFAULT '',
    allowed_agents TEXT NOT NULL DEFAULT '',
    token_hash TEXT NOT NULL UNIQUE,
    last_seen_at INTEGER,
    created_at INTEGER NOT NULL,
    PRIMARY KEY(user_id, id)
);

CREATE TABLE IF NOT EXISTS node_jobs (
    task_id TEXT PRIMARY KEY REFERENCES tasks(id) ON DELETE CASCADE,
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    node_id TEXT NOT NULL,
    agent TEXT NOT NULL DEFAULT '',
    role TEXT NOT NULL DEFAULT '',
    model TEXT NOT NULL DEFAULT '',
    effort TEXT NOT NULL DEFAULT '',
    sandbox TEXT NOT NULL DEFAULT '',
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

CREATE TABLE IF NOT EXISTS local_auth (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    token_hash TEXT NOT NULL,
    created_at INTEGER NOT NULL
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

CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(
    content,
    context,
    content='messages',
    content_rowid='id',
    tokenize='trigram'
);
CREATE TRIGGER IF NOT EXISTS messages_ai AFTER INSERT ON messages BEGIN
    INSERT INTO messages_fts(rowid, content, context) VALUES (new.id, new.content, new.context);
END;
CREATE TRIGGER IF NOT EXISTS messages_ad AFTER DELETE ON messages BEGIN
    INSERT INTO messages_fts(messages_fts, rowid, content, context) VALUES ('delete', old.id, old.content, old.context);
END;
CREATE TRIGGER IF NOT EXISTS messages_au AFTER UPDATE ON messages BEGIN
    INSERT INTO messages_fts(messages_fts, rowid, content, context) VALUES ('delete', old.id, old.content, old.context);
    INSERT INTO messages_fts(rowid, content, context) VALUES (new.id, new.content, new.context);
END;
"#;

fn migrate(writer: &mut Connection) -> std::result::Result<(), Box<dyn std::error::Error>> {
    let tx = writer.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let current_version: i64 = tx.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if !(0..=SCHEMA_VERSION).contains(&current_version) {
        return Err(format!(
            "unsupported database schema version {current_version} (expected {SCHEMA_VERSION}); v2 requires a compatible database"
        )
        .into());
    }
    if current_version > 0 {
        if current_version <= 1 {
            tx.execute_batch(
            "ALTER TABLE agent_profiles ADD COLUMN rolling_summary TEXT NOT NULL DEFAULT '';
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
             CREATE INDEX IF NOT EXISTS idx_messages_thread ON messages(user_id, thread_id, id DESC);",
        )?;
        }
        if current_version <= 2 {
            tx.execute_batch(
                "ALTER TABLE agent_profiles ADD COLUMN avatar_id TEXT NOT NULL DEFAULT 'agenct';",
            )?;
        }
        if current_version <= 3 {
            tx.execute_batch(
                "ALTER TABLE messages ADD COLUMN reply_to INTEGER;
             CREATE INDEX IF NOT EXISTS idx_messages_reply ON messages(user_id, reply_to);",
            )?;
        }
        if current_version <= 4 {
            let has_crons: bool = tx.query_row(
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
            tx.execute_batch(alter)?;
        }
        if current_version <= 5 {
            // `context` holds model-only text derived from a message (e.g. image captions).
            // `user_models` is created by the idempotent schema batch below.
            tx.execute_batch("ALTER TABLE messages ADD COLUMN context TEXT NOT NULL DEFAULT '';")?;
        }
        if current_version <= 7 {
            let has_tasks: bool = tx.query_row(
                "SELECT count(*) > 0 FROM sqlite_master WHERE type='table' AND name='tasks'",
                [],
                |r| r.get(0),
            )?;
            let alter = if has_tasks {
                "ALTER TABLE tasks ADD COLUMN on_complete TEXT NOT NULL DEFAULT '';
             ALTER TABLE tasks ADD COLUMN callback_depth INTEGER NOT NULL DEFAULT 0;"
            } else {
                ""
            };
            tx.execute_batch(alter)?;
        }
        let table_exists = |name: &str| -> rusqlite::Result<bool> {
            tx.query_row(
                "SELECT count(*) > 0 FROM sqlite_master WHERE type='table' AND name=?1",
                [name],
                |r| r.get(0),
            )
        };
        if current_version <= 8 {
            let mut alter = String::new();
            if table_exists("nodes")? {
                alter.push_str(
                    "ALTER TABLE nodes ADD COLUMN default_agent TEXT NOT NULL DEFAULT '';
                 ALTER TABLE nodes ADD COLUMN allowed_agents TEXT NOT NULL DEFAULT '';",
                );
            }
            if table_exists("node_jobs")? {
                alter.push_str("ALTER TABLE node_jobs ADD COLUMN agent TEXT NOT NULL DEFAULT '';");
            }
            if table_exists("threads")? {
                alter.push_str(
                    "ALTER TABLE threads ADD COLUMN node_session_agent TEXT NOT NULL DEFAULT '';",
                );
            }
            tx.execute_batch(&alter)?;
        }
        if current_version <= 9 {
            let mut alter = String::new();
            if table_exists("node_jobs")? {
                alter.push_str(
                    "ALTER TABLE node_jobs ADD COLUMN role TEXT NOT NULL DEFAULT '';
                 ALTER TABLE node_jobs ADD COLUMN model TEXT NOT NULL DEFAULT '';
                 ALTER TABLE node_jobs ADD COLUMN effort TEXT NOT NULL DEFAULT '';
                 ALTER TABLE node_jobs ADD COLUMN sandbox TEXT NOT NULL DEFAULT '';",
                );
            }
            if table_exists("threads")? {
                alter.push_str(
                    "ALTER TABLE threads ADD COLUMN node_session_role TEXT NOT NULL DEFAULT '';
                 ALTER TABLE threads ADD COLUMN node_session_model TEXT NOT NULL DEFAULT '';",
                );
            }
            tx.execute_batch(&alter)?;
        }
        if current_version <= 10 {
            let mut mig = String::new();
            if table_exists("nodes")? {
                mig.push_str(
                "UPDATE nodes SET default_agent = 'agy' WHERE lower(default_agent) = 'gemini';
                 UPDATE nodes SET allowed_agents = trim(replace(replace(',' || lower(allowed_agents) || ',', ',gemini,', ',agy,'), ',gemini,', ',agy,'), ',')
                   WHERE allowed_agents != '' AND instr(',' || lower(allowed_agents) || ',', ',gemini,') > 0;",
            );
            }
            if table_exists("agent_roles")? {
                mig.push_str("UPDATE agent_roles SET agent = 'agy' WHERE lower(agent) = 'gemini';");
            }
            tx.execute_batch(&mig)?;
        }
    }
    // Idempotent: creates any table/index/trigger an older database never had.
    tx.execute_batch(SCHEMA)?;
    // Older migrations could leave a partial index even after advancing user_version.
    if (1..SCHEMA_VERSION).contains(&current_version) {
        tx.execute_batch("INSERT INTO messages_fts(messages_fts) VALUES('rebuild');")?;
    }
    if current_version < SCHEMA_VERSION {
        tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    }
    tx.commit()?;
    Ok(())
}

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

        let mut writer = Connection::open(path)?;
        writer.busy_timeout(Duration::from_millis(5000))?;
        writer.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA foreign_keys = ON;",
        )?;
        migrate(&mut writer)?;
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

    /// Synchronous write for startup/CLI paths that run outside the async runtime.
    pub fn write_blocking<T>(&self, f: impl FnOnce(&mut Connection) -> Result<T>) -> Result<T> {
        let mut guard = self
            .writer
            .lock()
            .map_err(|_| Error(StatusCode::INTERNAL_SERVER_ERROR, "storage unavailable"))?;
        f(&mut guard)
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
    use std::path::Path;

    fn create_v11_with_partial_fts(path: &Path, rows: i64) {
        drop(Db::open(path).unwrap());
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            "INSERT INTO users(id, username, display_name, created_at) VALUES ('u1', 'alice', 'Alice', 1);
             DROP TRIGGER messages_ai;",
        )
        .unwrap();
        conn.execute(
            "WITH RECURSIVE seq(n) AS (VALUES(1) UNION ALL SELECT n + 1 FROM seq WHERE n < ?1)
             INSERT INTO messages(user_id, role, content, context, created_at)
             SELECT 'u1', 'user', 'backfill ' || hex(randomblob(64)), 'caption', n FROM seq",
            [rows],
        )
        .unwrap();
        conn.execute_batch(super::SCHEMA).unwrap();
        conn.execute_batch(
            "INSERT INTO messages_fts(rowid, content, context)
             SELECT id, content, context FROM messages WHERE id = 1;
             PRAGMA user_version = 11;",
        )
        .unwrap();
    }

    fn indexed_message_count(conn: &Connection) -> i64 {
        // Without MATCH, an external-content FTS table reads through to messages.
        conn.query_row(
            "SELECT count(*) FROM messages_fts WHERE messages_fts MATCH 'content:backfill AND context:caption'",
            [],
            |r| r.get(0),
        )
        .unwrap()
    }

    fn assert_complete_fts(conn: &Connection, expected_rows: i64) {
        let version: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        let rows: i64 = conn
            .query_row("SELECT count(*) FROM messages", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, 12);
        assert_eq!(rows, expected_rows);
        assert_eq!(indexed_message_count(conn), rows);
        conn.execute_batch(
            "INSERT INTO messages_fts(messages_fts, rank) VALUES('integrity-check', 1);",
        )
        .unwrap();
    }

    #[test]
    fn migrates_v11_and_rebuilds_fts_idempotently() {
        let dir = std::env::temp_dir().join(format!("agenct-fts-test-{}", uuid::Uuid::new_v4()));
        let path = dir.join("hub.sqlite3");
        create_v11_with_partial_fts(&path, 8);

        assert_eq!(indexed_message_count(&Connection::open(&path).unwrap()), 1);
        for rebuild_again in [true, false] {
            drop(Db::open(&path).unwrap());
            let conn = Connection::open(&path).unwrap();
            assert_complete_fts(&conn, 8);
            if rebuild_again {
                // Force the same migration to run against an already complete index.
                conn.pragma_update(None, "user_version", 11).unwrap();
            }
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn sigkill_during_fts_rebuild_rolls_back_and_retries() {
        use rusqlite::ffi;
        use std::{ffi::c_void, os::unix::process::ExitStatusExt, path::PathBuf, process::Command};

        const CHILD_DB: &str = "AGENCT_TEST_FTS_KILL_DB";
        const SIGKILL: i32 = 9;
        unsafe extern "C" {
            fn kill(pid: i32, signal: i32) -> i32;
        }

        unsafe extern "C" fn kill_on_wal_spill(context: *mut c_void) -> i32 {
            // SAFETY: The registered callback's PathBuf lives until the connection closes.
            let wal = unsafe { &*context.cast::<PathBuf>() };
            if wal.metadata().is_ok_and(|metadata| metadata.len() > 32) {
                // SAFETY: Send SIGKILL only to this test subprocess.
                unsafe { kill(std::process::id() as i32, SIGKILL) };
                return 1;
            }
            0
        }

        if let Some(path) = std::env::var_os(CHILD_DB) {
            let path = PathBuf::from(path);
            let mut wal = path.with_extension("sqlite3-wal");
            let mut conn = Connection::open(path).unwrap();
            conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE); PRAGMA cache_size = 8;")
                .unwrap();
            // The v11 schema already exists. The first WAL frame after the checkpoint
            // comes from the rebuild spilling dirty pages while the transaction is open.
            // SAFETY: conn outlives the migration and is dropped before wal.
            unsafe {
                ffi::sqlite3_progress_handler(
                    conn.handle(),
                    100,
                    Some(kill_on_wal_spill),
                    (&mut wal as *mut PathBuf).cast(),
                );
            }
            super::migrate(&mut conn).unwrap();
            panic!("migration completed without a SIGKILL");
        }

        let dir =
            std::env::temp_dir().join(format!("agenct-fts-kill-test-{}", uuid::Uuid::new_v4()));
        let path = dir.join("hub.sqlite3");
        create_v11_with_partial_fts(&path, 4096);
        let status = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "db::tests::sigkill_during_fts_rebuild_rolls_back_and_retries",
            ])
            .env(CHILD_DB, &path)
            .status()
            .unwrap();
        assert_eq!(status.signal(), Some(SIGKILL));

        let conn = Connection::open(&path).unwrap();
        let version: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(version, 11);
        assert_eq!(indexed_message_count(&conn), 1);
        drop(conn);

        drop(Db::open(&path).unwrap());
        assert_complete_fts(&Connection::open(&path).unwrap(), 4096);
        std::fs::remove_dir_all(dir).unwrap();
    }

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
        assert_eq!(ver, 12);
        let roles_table: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='agent_roles'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(roles_table, 1);
        let node_agent_cols: i64 = conn
            .query_row(
                "SELECT count(*) FROM pragma_table_info('nodes') WHERE name IN ('default_agent','allowed_agents')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(node_agent_cols, 2);
        let job_agent_cols: i64 = conn
            .query_row(
                "SELECT count(*) FROM pragma_table_info('node_jobs') WHERE name IN ('agent','role','model','effort','sandbox')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(job_agent_cols, 5);
        let thr_agent_cols: i64 = conn
            .query_row(
                "SELECT count(*) FROM pragma_table_info('threads') WHERE name IN ('node_session_agent','node_session_role','node_session_model')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(thr_agent_cols, 3);
        let on_complete_cols: i64 = conn
            .query_row(
                "SELECT count(*) FROM pragma_table_info('tasks') WHERE name='on_complete'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(on_complete_cols, 1);
        let fts_hits: i64 = conn
            .query_row(
                "SELECT count(*) FROM messages_fts WHERE messages_fts MATCH '\"hello\"'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(fts_hits, 1);
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

    #[test]
    fn migrates_v8_database_to_v9() {
        let dir = std::env::temp_dir().join(format!(
            "agenct-mig8-test-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("agenct.sqlite3");
        {
            let conn = Connection::open(&db_path).unwrap();
            conn.execute_batch(
                "CREATE TABLE users (id TEXT PRIMARY KEY, gitlab_id INTEGER UNIQUE, username TEXT NOT NULL UNIQUE, display_name TEXT NOT NULL, is_admin INTEGER NOT NULL DEFAULT 0, status TEXT NOT NULL DEFAULT 'active', created_at INTEGER NOT NULL);
                 CREATE TABLE nodes (id TEXT NOT NULL, user_id TEXT NOT NULL, name TEXT NOT NULL, kind TEXT NOT NULL DEFAULT 'pc', capabilities TEXT NOT NULL DEFAULT '', token_hash TEXT NOT NULL UNIQUE, last_seen_at INTEGER, created_at INTEGER NOT NULL, PRIMARY KEY(user_id, id));
                 CREATE TABLE tasks (id TEXT PRIMARY KEY, requester_id TEXT NOT NULL, owner_id TEXT NOT NULL, thread_id TEXT, title TEXT NOT NULL, instruction TEXT NOT NULL, status TEXT NOT NULL, result TEXT, revision INTEGER NOT NULL DEFAULT 0, on_complete TEXT NOT NULL DEFAULT '', callback_depth INTEGER NOT NULL DEFAULT 0, created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL);
                 CREATE TABLE node_jobs (task_id TEXT PRIMARY KEY, user_id TEXT NOT NULL, node_id TEXT NOT NULL, task_revision INTEGER NOT NULL, authorization TEXT NOT NULL DEFAULT 'manual', status TEXT NOT NULL, lease_hash TEXT, lease_until INTEGER, submission_hash TEXT, stage TEXT NOT NULL DEFAULT '', log_tail TEXT NOT NULL DEFAULT '', updated_at INTEGER NOT NULL);
                 CREATE TABLE threads (id TEXT PRIMARY KEY, user_id TEXT NOT NULL, title TEXT NOT NULL, status TEXT NOT NULL DEFAULT 'active', summary TEXT NOT NULL DEFAULT '', node_id TEXT, node_session_id TEXT, turn_count INTEGER NOT NULL DEFAULT 0, updated_at INTEGER NOT NULL);
                 INSERT INTO users VALUES ('u1', 1, 'alice', 'Alice', 1, 'active', 100);
                 INSERT INTO nodes VALUES ('n1', 'u1', 'HomePC', 'pc', 'agent:opencode', 'tokh1', 100, 100);
                 INSERT INTO threads VALUES ('thr_1', 'u1', 'Work', 'active', 'sum', 'n1', 'ses_1', 1, 100);
                 PRAGMA user_version = 8;",
            )
            .unwrap();
        }
        let db = Db::open(&db_path).unwrap();
        drop(db);
        let conn = Connection::open(&db_path).unwrap();
        let ver: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(ver, 12);
        let (def_ag, allow_ag): (String, String) = conn
            .query_row(
                "SELECT default_agent, allowed_agents FROM nodes WHERE id='n1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((def_ag.as_str(), allow_ag.as_str()), ("", ""));
        let (ses_ag, ses_role, ses_model): (String, String, String) = conn
            .query_row(
                "SELECT node_session_agent, node_session_role, node_session_model FROM threads WHERE id='thr_1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            (ses_ag.as_str(), ses_role.as_str(), ses_model.as_str()),
            ("", "", "")
        );
        let job_ag_cols: i64 = conn
            .query_row(
                "SELECT count(*) FROM pragma_table_info('node_jobs') WHERE name IN ('agent','role','model','effort','sandbox')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(job_ag_cols, 5);

        // Unsupported future schema version must still be rejected.
        conn.pragma_update(None, "user_version", 99).unwrap();
        drop(conn);
        assert!(Db::open(&db_path).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn migrates_v9_database_to_v10() {
        let dir = std::env::temp_dir().join(format!(
            "agenct-mig9-test-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("agenct.sqlite3");
        {
            let conn = Connection::open(&db_path).unwrap();
            conn.execute_batch(
                "CREATE TABLE users (id TEXT PRIMARY KEY, gitlab_id INTEGER UNIQUE, username TEXT NOT NULL UNIQUE, display_name TEXT NOT NULL, is_admin INTEGER NOT NULL DEFAULT 0, status TEXT NOT NULL DEFAULT 'active', created_at INTEGER NOT NULL);
                 CREATE TABLE nodes (id TEXT NOT NULL, user_id TEXT NOT NULL, name TEXT NOT NULL, kind TEXT NOT NULL DEFAULT 'pc', capabilities TEXT NOT NULL DEFAULT '', default_agent TEXT NOT NULL DEFAULT '', allowed_agents TEXT NOT NULL DEFAULT '', token_hash TEXT NOT NULL UNIQUE, last_seen_at INTEGER, created_at INTEGER NOT NULL, PRIMARY KEY(user_id, id));
                 CREATE TABLE tasks (id TEXT PRIMARY KEY, requester_id TEXT NOT NULL, owner_id TEXT NOT NULL, thread_id TEXT, title TEXT NOT NULL, instruction TEXT NOT NULL, status TEXT NOT NULL, result TEXT, revision INTEGER NOT NULL DEFAULT 0, on_complete TEXT NOT NULL DEFAULT '', callback_depth INTEGER NOT NULL DEFAULT 0, created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL);
                 CREATE TABLE node_jobs (task_id TEXT PRIMARY KEY, user_id TEXT NOT NULL, node_id TEXT NOT NULL, agent TEXT NOT NULL DEFAULT '', task_revision INTEGER NOT NULL, authorization TEXT NOT NULL DEFAULT 'manual', status TEXT NOT NULL, lease_hash TEXT, lease_until INTEGER, submission_hash TEXT, stage TEXT NOT NULL DEFAULT '', log_tail TEXT NOT NULL DEFAULT '', updated_at INTEGER NOT NULL);
                 CREATE TABLE threads (id TEXT PRIMARY KEY, user_id TEXT NOT NULL, title TEXT NOT NULL, status TEXT NOT NULL DEFAULT 'active', summary TEXT NOT NULL DEFAULT '', node_id TEXT, node_session_id TEXT, node_session_agent TEXT NOT NULL DEFAULT '', turn_count INTEGER NOT NULL DEFAULT 0, updated_at INTEGER NOT NULL);
                 INSERT INTO users VALUES ('u1', 1, 'alice', 'Alice', 1, 'active', 100);
                 INSERT INTO nodes VALUES ('homelab', 'u1', 'HomeLab', 'pc', 'agent:codex', 'codex', 'codex,opencode', 'tokh9', 100, 100);
                 INSERT INTO threads VALUES ('thr_9', 'u1', 'Deploy', 'active', 'sum', 'homelab', 'ses_9', 'codex', 2, 100);
                 INSERT INTO tasks VALUES ('tsk_9', 'u1', 'u1', 'thr_9', 'Check', 'status', 'done', 'ok', 1, '', 0, 100, 100);
                 INSERT INTO node_jobs VALUES ('tsk_9', 'u1', 'homelab', 'codex', 1, 'manual', 'completed', NULL, NULL, 'subh', 'done', '', 100);
                 PRAGMA user_version = 9;",
            )
            .unwrap();
        }
        let db = Db::open(&db_path).unwrap();
        drop(db);
        let conn = Connection::open(&db_path).unwrap();
        let ver: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(ver, 12);
        let roles_table: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='agent_roles'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(roles_table, 1);
        let (role, model, effort, sandbox): (String, String, String, String) = conn
            .query_row(
                "SELECT role, model, effort, sandbox FROM node_jobs WHERE task_id='tsk_9'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(
            (
                role.as_str(),
                model.as_str(),
                effort.as_str(),
                sandbox.as_str()
            ),
            ("", "", "", "")
        );
        let (thr_agent, thr_role, thr_model): (String, String, String) = conn
            .query_row(
                "SELECT node_session_agent, node_session_role, node_session_model FROM threads WHERE id='thr_9'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            (thr_agent.as_str(), thr_role.as_str(), thr_model.as_str()),
            ("codex", "", "")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn migrates_v10_database_to_v11() {
        let dir = std::env::temp_dir().join(format!(
            "agenct-mig10-test-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("agenct.sqlite3");
        {
            let conn = Connection::open(&db_path).unwrap();
            conn.execute_batch(
                "CREATE TABLE users (id TEXT PRIMARY KEY, gitlab_id INTEGER UNIQUE, username TEXT NOT NULL UNIQUE, display_name TEXT NOT NULL, is_admin INTEGER NOT NULL DEFAULT 0, status TEXT NOT NULL DEFAULT 'active', created_at INTEGER NOT NULL);
                 CREATE TABLE agent_roles (user_id TEXT NOT NULL, name TEXT NOT NULL, description TEXT NOT NULL DEFAULT '', agent TEXT NOT NULL DEFAULT '', model TEXT NOT NULL DEFAULT '', effort TEXT NOT NULL DEFAULT '', sandbox TEXT NOT NULL DEFAULT '', node_id TEXT NOT NULL DEFAULT '', prompt TEXT NOT NULL DEFAULT '', updated_at INTEGER NOT NULL, PRIMARY KEY(user_id, name));
                 CREATE TABLE nodes (id TEXT NOT NULL, user_id TEXT NOT NULL, name TEXT NOT NULL, kind TEXT NOT NULL DEFAULT 'pc', capabilities TEXT NOT NULL DEFAULT '', default_agent TEXT NOT NULL DEFAULT '', allowed_agents TEXT NOT NULL DEFAULT '', token_hash TEXT NOT NULL UNIQUE, last_seen_at INTEGER, created_at INTEGER NOT NULL, PRIMARY KEY(user_id, id));
                 CREATE TABLE tasks (id TEXT PRIMARY KEY, requester_id TEXT NOT NULL, owner_id TEXT NOT NULL, thread_id TEXT, title TEXT NOT NULL, instruction TEXT NOT NULL, status TEXT NOT NULL, result TEXT, revision INTEGER NOT NULL DEFAULT 0, on_complete TEXT NOT NULL DEFAULT '', callback_depth INTEGER NOT NULL DEFAULT 0, created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL);
                 CREATE TABLE node_jobs (task_id TEXT PRIMARY KEY, user_id TEXT NOT NULL, node_id TEXT NOT NULL, agent TEXT NOT NULL DEFAULT '', role TEXT NOT NULL DEFAULT '', model TEXT NOT NULL DEFAULT '', effort TEXT NOT NULL DEFAULT '', sandbox TEXT NOT NULL DEFAULT '', task_revision INTEGER NOT NULL, authorization TEXT NOT NULL DEFAULT 'manual', status TEXT NOT NULL, lease_hash TEXT, lease_until INTEGER, submission_hash TEXT, stage TEXT NOT NULL DEFAULT '', log_tail TEXT NOT NULL DEFAULT '', updated_at INTEGER NOT NULL);
                 CREATE TABLE threads (id TEXT PRIMARY KEY, user_id TEXT NOT NULL, title TEXT NOT NULL, status TEXT NOT NULL DEFAULT 'active', summary TEXT NOT NULL DEFAULT '', node_id TEXT, node_session_id TEXT, node_session_agent TEXT NOT NULL DEFAULT '', node_session_role TEXT NOT NULL DEFAULT '', node_session_model TEXT NOT NULL DEFAULT '', turn_count INTEGER NOT NULL DEFAULT 0, updated_at INTEGER NOT NULL);
                 INSERT INTO users VALUES ('u1', 1, 'alice', 'Alice', 1, 'active', 100);
                 INSERT INTO nodes VALUES ('homelab', 'u1', 'HomeLab', 'pc', 'agent:codex', 'gemini', 'codex,gemini,opencode', 'tokh10', 100, 100);
                 INSERT INTO agent_roles VALUES ('u1', 'planner', 'Plan', 'gemini', 'claude-opus-5-5-high', 'high', 'readonly', '', '', 100);
                 INSERT INTO agent_roles VALUES ('u1', 'executor', 'Exec', 'codex', 'gpt-6.2-sol', 'medium', 'workspace-write', '', '', 100);
                 PRAGMA user_version = 10;",
            )
            .unwrap();
        }
        let db = Db::open(&db_path).unwrap();
        drop(db);
        let conn = Connection::open(&db_path).unwrap();
        let ver: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(ver, 12);
        let (def_ag, allow_ag): (String, String) = conn
            .query_row(
                "SELECT default_agent, allowed_agents FROM nodes WHERE id='homelab'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            (def_ag.as_str(), allow_ag.as_str()),
            ("agy", "codex,agy,opencode")
        );
        let (p_ag, e_ag): (String, String) = (
            conn.query_row(
                "SELECT agent FROM agent_roles WHERE name='planner'",
                [],
                |r| r.get(0),
            )
            .unwrap(),
            conn.query_row(
                "SELECT agent FROM agent_roles WHERE name='executor'",
                [],
                |r| r.get(0),
            )
            .unwrap(),
        );
        assert_eq!((p_ag.as_str(), e_ag.as_str()), ("agy", "codex"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
