//! Hub-owned SQLite schema. Publication storage has a separate, opt-in lifecycle.
use rusqlite::{Connection, OptionalExtension, TransactionBehavior};
use std::collections::BTreeMap;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
pub(super) const VERSION: i64 = 4;

const BASE_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS sessions (owner TEXT PRIMARY KEY, session_id TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS messages (id INTEGER PRIMARY KEY, owner TEXT NOT NULL, role TEXT NOT NULL, content TEXT NOT NULL, status TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS tasks (id TEXT PRIMARY KEY, requester TEXT NOT NULL, owner TEXT NOT NULL, summary TEXT NOT NULL, status TEXT NOT NULL, result TEXT);
CREATE TABLE IF NOT EXISTS proposals (id TEXT PRIMARY KEY, requester TEXT NOT NULL, owner TEXT NOT NULL, summary TEXT NOT NULL, status TEXT NOT NULL DEFAULT 'pending', revision INTEGER NOT NULL DEFAULT 0, task_id TEXT UNIQUE, request_id TEXT NOT NULL, source TEXT NOT NULL, UNIQUE(requester,request_id));
CREATE TABLE IF NOT EXISTS events (id INTEGER PRIMARY KEY, task_id TEXT NOT NULL, actor TEXT NOT NULL, action TEXT NOT NULL, at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP);
CREATE TABLE IF NOT EXISTS personal_profiles (owner TEXT PRIMARY KEY, revision INTEGER NOT NULL, agents_json TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS chat_turns (
    owner TEXT NOT NULL, request_id TEXT NOT NULL, message TEXT NOT NULL,
    agent_id TEXT, runtime_profile TEXT NOT NULL,
    status TEXT NOT NULL CHECK(status IN ('pending','uncertain','complete')),
    response TEXT, message_id INTEGER, PRIMARY KEY(owner,request_id));";

const WORKER_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS worker_jobs (
    id TEXT PRIMARY KEY, owner TEXT NOT NULL, task_id TEXT NOT NULL,
    task_revision INTEGER NOT NULL, repository TEXT NOT NULL, base_commit TEXT NOT NULL,
    request_id TEXT NOT NULL, status TEXT NOT NULL CHECK(status IN ('queued','running','review_ready','failed','cancelled')),
    attempt INTEGER NOT NULL DEFAULT 0, executor TEXT, lease_hash TEXT, lease_until INTEGER,
    candidate TEXT, candidate_sha256 TEXT, failure TEXT, created_at INTEGER NOT NULL,
    UNIQUE(owner,request_id));
CREATE UNIQUE INDEX IF NOT EXISTS worker_one_active_task ON worker_jobs(task_id)
    WHERE status IN ('queued','running','review_ready');";

const TABLES: &[&str] = &[
    "sessions",
    "messages",
    "tasks",
    "proposals",
    "events",
    "personal_profiles",
    "chat_turns",
    "worker_jobs",
];

pub(super) fn migrate(db: &mut Connection) -> Result<()> {
    migrate_with_hook(db, |_| Ok(()))
}

fn version(db: &Connection) -> rusqlite::Result<i64> {
    db.query_row("PRAGMA user_version", [], |row| row.get(0))
}

fn supported(version: i64) -> Result<()> {
    if !(0..=VERSION).contains(&version) {
        return Err(format!(
            "unsupported Hub database schema version {version} (maximum {VERSION})"
        )
        .into());
    }
    Ok(())
}

fn migrate_with_hook(db: &mut Connection, after_step: impl Fn(i64) -> Result<()>) -> Result<()> {
    // In particular, do not switch journal mode on an unsupported database.
    supported(version(db)?)?;
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let current = version(&tx)?;
    supported(current)?;
    validate(&tx, current)?;
    for next in (current + 1)..=VERSION {
        step(&tx, next)?;
        tx.pragma_update(None, "user_version", next)?;
        after_step(next)?;
    }
    validate(&tx, VERSION)?;
    tx.commit()?;
    Ok(())
}

fn step(db: &Connection, version: i64) -> rusqlite::Result<()> {
    match version {
        1 => {
            db.execute_batch(BASE_SCHEMA)?;
            db.execute_batch(WORKER_SCHEMA)?;
        }
        2 => add_column(db, "tasks", "revision", "INTEGER NOT NULL DEFAULT 0")?,
        3 => {
            add_column(db, "chat_turns", "selected_task", "TEXT")?;
            add_column(db, "chat_turns", "task_context", "TEXT")?;
            add_column(db, "chat_turns", "expected_profile_revision", "INTEGER")?;
        }
        4 => add_column(
            db,
            "worker_jobs",
            "execution_authorized",
            "INTEGER NOT NULL DEFAULT 0",
        )?,
        _ => unreachable!("only ordered, supported schema versions are applied"),
    }
    Ok(())
}

fn add_column(
    db: &Connection,
    table: &str,
    column: &str,
    definition: &str,
) -> rusqlite::Result<()> {
    if !columns(db, table)?.contains_key(column) {
        // All identifiers and definitions are constants owned by this module.
        db.execute_batch(&format!(
            "ALTER TABLE {table} ADD COLUMN {column} {definition}"
        ))?;
    }
    Ok(())
}

/// Retain the public worker module's independent setup API without assigning a
/// Hub user_version or initializing any other subsystem (notably publication).
pub(crate) fn migrate_worker(db: &Connection) -> rusqlite::Result<()> {
    db.execute_batch(WORKER_SCHEMA)?;
    step(db, 4)
}

/// Recovery is per start, not per schema version. All related statuses move
/// together, including receipts from databases that already have this version.
pub(super) fn recover(db: &mut Connection) -> rusqlite::Result<()> {
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute_batch(
        "UPDATE messages SET status='interrupted' WHERE status='pending';
         UPDATE messages SET status='uncertain'
         WHERE status IN ('pending','interrupted') AND id IN
             (SELECT message_id FROM chat_turns WHERE status='pending');
         UPDATE chat_turns SET status='uncertain' WHERE status='pending';",
    )?;
    tx.commit()
}

#[derive(Debug, PartialEq)]
struct Column {
    kind: String,
    not_null: bool,
    default: Option<String>,
    primary_key: i64,
    hidden: i64,
}

fn columns(db: &Connection, table: &str) -> rusqlite::Result<BTreeMap<String, Column>> {
    let mut query = db.prepare(&format!("PRAGMA table_xinfo({table})"))?;
    let rows = query.query_map([], |row| {
        Ok((
            row.get(1)?,
            Column {
                kind: row.get::<_, String>(2)?.to_ascii_uppercase(),
                not_null: row.get(3)?,
                default: row.get(4)?,
                primary_key: row.get(5)?,
                hidden: row.get(6)?,
            },
        ))
    })?;
    rows.collect()
}

fn introduced(table: &str, column: &str) -> i64 {
    match (table, column) {
        ("tasks", "revision") => 2,
        ("chat_turns", "selected_task" | "task_context" | "expected_profile_revision") => 3,
        ("worker_jobs", "execution_authorized") => 4,
        _ => 1,
    }
}

type UniqueKey = Vec<(String, String, bool)>;

fn unique_keys(db: &Connection, table: &str) -> rusqlite::Result<Vec<UniqueKey>> {
    let mut query =
        db.prepare("SELECT name FROM pragma_index_list(?1) WHERE \"unique\"=1 AND partial=0")?;
    let names = query
        .query_map([table], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut keys = names
        .into_iter()
        .map(|name| {
            let mut query = db.prepare(
                "SELECT name,coll,desc FROM pragma_index_xinfo(?1) WHERE key=1 ORDER BY seqno",
            )?;
            let rows =
                query.query_map([name], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?;
            rows.collect::<rusqlite::Result<UniqueKey>>()
        })
        .collect::<rusqlite::Result<Vec<_>>>()?;
    keys.sort();
    Ok(keys)
}

fn normalized(sql: &str) -> String {
    let mut result = String::new();
    let mut quote = None;
    for ch in sql.chars() {
        if let Some(end) = quote {
            result.push(ch);
            if ch == end {
                quote = None;
            }
        } else if ch == '\'' || ch == '"' {
            quote = Some(ch);
            result.push(ch);
        } else if !ch.is_ascii_whitespace() {
            result.push(ch.to_ascii_lowercase());
        }
    }
    result
}

fn validate(db: &Connection, version: i64) -> Result<()> {
    // Derive the contract from the migrations themselves, avoiding a second
    // hand-maintained list of column types, nullability, defaults and keys.
    let expected = Connection::open_in_memory()?;
    for next in 1..=VERSION {
        step(&expected, next)?;
    }
    for &table in TABLES {
        let object: Option<(String, String)> = db
            .query_row(
                "SELECT type,sql FROM sqlite_schema WHERE name=?1",
                [table],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((kind, sql)) = object else {
            if version == 0 {
                continue;
            }
            return Err(format!("missing Hub table {table} at schema version {version}").into());
        };
        if kind != "table" {
            return Err(format!("incompatible Hub schema: {table} is not a table").into());
        }
        let actual = columns(db, table)?;
        let wanted = columns(&expected, table)?;
        for (name, shape) in &actual {
            if wanted.get(name) != Some(shape) {
                return Err(format!("incompatible Hub column {table}.{name}").into());
            }
        }
        for name in wanted.keys() {
            // Version zero may contain any mix of the historical additive
            // upgrades, but all baseline columns of each present table must exist.
            if !actual.contains_key(name) && introduced(table, name) <= version.max(1) {
                return Err(format!("missing Hub column {table}.{name}").into());
            }
        }
        if unique_keys(db, table)? != unique_keys(&expected, table)? {
            return Err(format!("incompatible Hub unique keys on {table}").into());
        }
        let extra_partial_keys: i64 = db.query_row(
            "SELECT count(*) FROM pragma_index_list(?1)
             WHERE \"unique\"=1 AND partial=1 AND name!='worker_one_active_task'",
            [table],
            |row| row.get(0),
        )?;
        if extra_partial_keys != 0 {
            return Err(format!("unsupported Hub partial unique key on {table}").into());
        }
        // Some supported old receipt schemas lack a status CHECK. Preserve
        // them as-is; do not rebuild populated tables just to add constraints.
        let normalized = normalized(&sql);
        let check = match table {
            "chat_turns" if normalized.contains("check(") => {
                Some("check(statusin('pending','uncertain','complete'))")
            }
            "worker_jobs" => {
                Some("check(statusin('queued','running','review_ready','failed','cancelled'))")
            }
            _ => None,
        };
        if normalized.matches("check(").count() != usize::from(check.is_some())
            || check.is_some_and(|check| !normalized.contains(check))
        {
            return Err(format!("incompatible Hub status constraint on {table}").into());
        }
        // These were never part of a supported Hub layout and are not exposed
        // in table_info. Do not silently adopt different write/comparison rules.
        if normalized.contains("onconflict") || normalized.contains("collate") {
            return Err(format!("incompatible Hub constraint on {table}").into());
        }
        let foreign_keys: i64 = db.query_row(
            "SELECT count(*) FROM pragma_foreign_key_list(?1)",
            [table],
            |row| row.get(0),
        )?;
        if foreign_keys != 0 {
            return Err(format!("unsupported Hub foreign key on {table}").into());
        }
    }
    let index: Option<String> = db
        .query_row(
            "SELECT sql FROM sqlite_schema WHERE name='worker_one_active_task' AND type='index'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    let wanted: String = expected.query_row(
        "SELECT sql FROM sqlite_schema WHERE name='worker_one_active_task'",
        [],
        |row| row.get(0),
    )?;
    match index {
        Some(sql) if normalized(&sql) != normalized(&wanted) => {
            return Err("incompatible Hub index worker_one_active_task".into());
        }
        None if version != 0 => return Err("missing Hub index worker_one_active_task".into()),
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const LEGACY: &str = "
        CREATE TABLE tasks (id TEXT PRIMARY KEY, requester TEXT NOT NULL, owner TEXT NOT NULL,
            summary TEXT NOT NULL, status TEXT NOT NULL, result TEXT);
        INSERT INTO tasks VALUES ('task','alice','bob','keep this task','working',NULL);
        CREATE TABLE messages (id INTEGER PRIMARY KEY, owner TEXT NOT NULL, role TEXT NOT NULL,
            content TEXT NOT NULL, status TEXT NOT NULL);
        INSERT INTO messages VALUES (1,'alice','user','keep this message','pending');
        CREATE TABLE chat_turns (owner TEXT NOT NULL, request_id TEXT NOT NULL, message TEXT NOT NULL,
            agent_id TEXT, runtime_profile TEXT NOT NULL, status TEXT NOT NULL,
            response TEXT, message_id INTEGER, PRIMARY KEY(owner,request_id));
        INSERT INTO chat_turns VALUES ('alice','receipt','keep this message',NULL,'{}','pending',NULL,1);
        CREATE INDEX keep_message_index ON messages(owner);";

    fn scalar(db: &Connection, sql: &str) -> i64 {
        db.query_row(sql, [], |row| row.get(0)).unwrap()
    }

    fn text(db: &Connection, sql: &str) -> String {
        db.query_row(sql, [], |row| row.get(0)).unwrap()
    }

    fn schema(db: &Connection) -> Vec<(String, String, Option<String>)> {
        let mut query = db
            .prepare("SELECT type,name,sql FROM sqlite_schema ORDER BY type,name")
            .unwrap();
        let rows = query
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .unwrap();
        rows.collect::<rusqlite::Result<_>>().unwrap()
    }

    #[test]
    fn empty_database_migrates_once_without_initializing_publication() {
        let mut db = Connection::open_in_memory().unwrap();
        let seen = std::cell::RefCell::new(Vec::new());
        migrate_with_hook(&mut db, |next| {
            seen.borrow_mut().push(next);
            Ok(())
        })
        .unwrap();
        assert_eq!(*seen.borrow(), vec![1, 2, 3, 4]);
        assert_eq!(version(&db).unwrap(), VERSION);
        let before = schema(&db);
        migrate_with_hook(&mut db, |_| panic!("must not rerun a migration")).unwrap();
        assert_eq!(schema(&db), before);
        assert_eq!(
            scalar(
                &db,
                "SELECT count(*) FROM sqlite_schema WHERE name='publication_intents'"
            ),
            0
        );
    }

    #[test]
    fn populated_legacy_layouts_preserve_data_bindings_and_worker_authority() {
        // Exercise every independently present additive column combination.
        for mask in 0..32 {
            let mut db = Connection::open_in_memory().unwrap();
            db.execute_batch(LEGACY).unwrap();
            db.execute_batch(WORKER_SCHEMA).unwrap();
            db.execute_batch("INSERT INTO worker_jobs(id,owner,task_id,task_revision,repository,base_commit,request_id,status,created_at) VALUES ('job','bob','task',0,'repo','base','work','queued',1)").unwrap();
            for (bit, table, column, definition) in [
                (1, "tasks", "revision", "INTEGER NOT NULL DEFAULT 0"),
                (2, "chat_turns", "selected_task", "TEXT"),
                (4, "chat_turns", "task_context", "TEXT"),
                (8, "chat_turns", "expected_profile_revision", "INTEGER"),
                (
                    16,
                    "worker_jobs",
                    "execution_authorized",
                    "INTEGER NOT NULL DEFAULT 0",
                ),
            ] {
                if mask & bit != 0 {
                    add_column(&db, table, column, definition).unwrap();
                }
            }
            if mask & 1 != 0 {
                db.execute_batch("UPDATE tasks SET revision=7").unwrap();
            }
            if mask & 2 != 0 {
                db.execute_batch("UPDATE chat_turns SET selected_task='frozen selection'")
                    .unwrap();
            }
            if mask & 4 != 0 {
                db.execute_batch("UPDATE chat_turns SET task_context='frozen context'")
                    .unwrap();
            }
            if mask & 8 != 0 {
                db.execute_batch("UPDATE chat_turns SET expected_profile_revision=9")
                    .unwrap();
            }
            if mask & 16 != 0 {
                db.execute_batch("UPDATE worker_jobs SET execution_authorized=1")
                    .unwrap();
            }
            migrate(&mut db).unwrap();
            assert_eq!(version(&db).unwrap(), VERSION);
            assert_eq!(text(&db, "SELECT summary FROM tasks"), "keep this task");
            assert_eq!(
                scalar(&db, "SELECT revision FROM tasks"),
                if mask & 1 == 0 { 0 } else { 7 }
            );
            assert_eq!(
                scalar(&db, "SELECT execution_authorized FROM worker_jobs"),
                if mask & 16 == 0 { 0 } else { 1 }
            );
            assert_eq!(text(&db, "SELECT status FROM chat_turns"), "pending");
            assert_eq!(
                text(&db, "SELECT message FROM chat_turns"),
                "keep this message"
            );
            for (bit, column, value) in [
                (2, "selected_task", "frozen selection"),
                (4, "task_context", "frozen context"),
            ] {
                let stored: Option<String> = db
                    .query_row(&format!("SELECT {column} FROM chat_turns"), [], |row| {
                        row.get(0)
                    })
                    .unwrap();
                assert_eq!(
                    stored.as_deref(),
                    if mask & bit == 0 { None } else { Some(value) }
                );
            }
            let revision: Option<i64> = db
                .query_row(
                    "SELECT expected_profile_revision FROM chat_turns",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(revision, if mask & 8 == 0 { None } else { Some(9) });
            assert_eq!(
                scalar(
                    &db,
                    "SELECT count(*) FROM sqlite_schema WHERE name='keep_message_index'"
                ),
                1
            );
            migrate(&mut db).unwrap();
        }
    }

    #[test]
    fn every_version_resumes_at_the_next_step() {
        for current in 1..=VERSION {
            let mut db = Connection::open_in_memory().unwrap();
            for next in 1..=current {
                step(&db, next).unwrap();
            }
            db.pragma_update(None, "user_version", current).unwrap();
            let seen = std::cell::RefCell::new(Vec::new());
            migrate_with_hook(&mut db, |next| {
                seen.borrow_mut().push(next);
                Ok(())
            })
            .unwrap();
            assert_eq!(
                *seen.borrow(),
                ((current + 1)..=VERSION).collect::<Vec<_>>()
            );
            assert_eq!(version(&db).unwrap(), VERSION);
        }
    }

    #[test]
    fn failure_at_each_step_rolls_back_schema_data_and_version_then_retries() {
        for populated in [false, true] {
            for fail_at in 1..=VERSION {
                let mut db = Connection::open_in_memory().unwrap();
                if populated {
                    db.execute_batch(LEGACY).unwrap();
                }
                let before = schema(&db);
                let error = migrate_with_hook(&mut db, |next| {
                    if next == fail_at {
                        return Err("injected migration failure".into());
                    }
                    Ok(())
                });
                assert!(error.is_err());
                assert!(db.is_autocommit());
                assert_eq!(schema(&db), before);
                assert_eq!(version(&db).unwrap(), 0);
                if populated {
                    assert_eq!(text(&db, "SELECT status FROM messages"), "pending");
                    assert_eq!(text(&db, "SELECT status FROM chat_turns"), "pending");
                    assert_eq!(text(&db, "SELECT summary FROM tasks"), "keep this task");
                }
                migrate(&mut db).unwrap();
                assert_eq!(version(&db).unwrap(), VERSION);
            }
        }
    }

    #[test]
    fn incompatible_schemas_fail_closed_without_partial_upgrades() {
        for incompatible in [
            "CREATE TABLE tasks(id TEXT PRIMARY KEY)",
            "CREATE VIEW tasks AS SELECT 'not a table' AS id",
            "CREATE TABLE chat_turns(owner TEXT NOT NULL,request_id TEXT NOT NULL,message TEXT NOT NULL,agent_id TEXT,runtime_profile TEXT NOT NULL,status TEXT NOT NULL,response TEXT,message_id INTEGER)",
            "CREATE TABLE worker_one_active_task(id INTEGER)",
            "CREATE TABLE messages(id INTEGER PRIMARY KEY DESC,owner TEXT NOT NULL,role TEXT NOT NULL,content TEXT NOT NULL,status TEXT NOT NULL)",
            "CREATE TABLE messages(id INTEGER PRIMARY KEY,owner TEXT NOT NULL COLLATE NOCASE,role TEXT NOT NULL,content TEXT NOT NULL,status TEXT NOT NULL)",
            "CREATE TABLE messages(id INTEGER PRIMARY KEY ON CONFLICT REPLACE,owner TEXT NOT NULL,role TEXT NOT NULL,content TEXT NOT NULL,status TEXT NOT NULL)",
            "CREATE TABLE messages(id INTEGER PRIMARY KEY,owner TEXT NOT NULL,role TEXT NOT NULL,content TEXT NOT NULL,status TEXT NOT NULL CHECK(status='pending'))",
            "CREATE TABLE sessions(owner TEXT PRIMARY KEY,session_id TEXT NOT NULL UNIQUE)",
        ] {
            let mut db = Connection::open_in_memory().unwrap();
            db.execute_batch(incompatible).unwrap();
            let before = schema(&db);
            assert!(migrate(&mut db).is_err());
            assert_eq!(schema(&db), before);
            assert_eq!(version(&db).unwrap(), 0);
        }
        let mut db = Connection::open_in_memory().unwrap();
        db.execute_batch(WORKER_SCHEMA).unwrap();
        db.execute_batch("DROP INDEX worker_one_active_task; CREATE INDEX worker_one_active_task ON worker_jobs(owner)").unwrap();
        let before = schema(&db);
        assert!(migrate(&mut db).is_err());
        assert_eq!(schema(&db), before);
        db.execute_batch("DROP INDEX worker_one_active_task; CREATE UNIQUE INDEX worker_one_active_task ON worker_jobs(task_id) WHERE status IN ('QUEUED','RUNNING','REVIEW_READY')").unwrap();
        let before = schema(&db);
        assert!(migrate(&mut db).is_err());
        assert_eq!(schema(&db), before);
    }

    #[test]
    fn recovery_is_atomic_and_runs_independently_of_schema_upgrades() {
        let mut db = Connection::open_in_memory().unwrap();
        db.execute_batch(LEGACY).unwrap();
        migrate(&mut db).unwrap();
        db.execute_batch(
            "INSERT INTO messages VALUES (2,'bob','user','untracked','pending');
            CREATE TRIGGER fail_recovery BEFORE UPDATE ON chat_turns
            BEGIN SELECT RAISE(ABORT,'injected recovery failure'); END;",
        )
        .unwrap();
        assert!(recover(&mut db).is_err());
        assert_eq!(
            scalar(&db, "SELECT count(*) FROM messages WHERE status='pending'"),
            2
        );
        assert_eq!(text(&db, "SELECT status FROM chat_turns"), "pending");
        assert_eq!(version(&db).unwrap(), VERSION);
        db.execute_batch("DROP TRIGGER fail_recovery").unwrap();
        for _ in 0..2 {
            migrate(&mut db).unwrap();
            recover(&mut db).unwrap();
            assert_eq!(
                text(&db, "SELECT status FROM messages WHERE id=1"),
                "uncertain"
            );
            assert_eq!(
                text(&db, "SELECT status FROM messages WHERE id=2"),
                "interrupted"
            );
            assert_eq!(text(&db, "SELECT status FROM chat_turns"), "uncertain");
            db.execute_batch(
                "UPDATE messages SET status='pending'; UPDATE chat_turns SET status='pending'",
            )
            .unwrap();
        }
    }

    #[test]
    fn fresh_and_existing_constraints_and_indexes_remain_enforced() {
        let mut db = Connection::open_in_memory().unwrap();
        migrate(&mut db).unwrap();
        let before = schema(&db);
        db.execute_batch("PRAGMA user_version=0").unwrap();
        migrate(&mut db).unwrap();
        assert_eq!(schema(&db), before);
        db.execute_batch("INSERT INTO chat_turns(owner,request_id,message,runtime_profile,status) VALUES ('alice','receipt','hello','{}','complete')").unwrap();
        assert!(db
            .execute_batch("UPDATE chat_turns SET status='invalid'")
            .is_err());
        assert!(db
            .execute_batch("INSERT INTO chat_turns SELECT * FROM chat_turns")
            .is_err());
        db.execute_batch("INSERT INTO worker_jobs(id,owner,task_id,task_revision,repository,base_commit,request_id,status,created_at) VALUES ('job','bob','task',0,'repo','base','work','queued',1)").unwrap();
        assert!(db
            .execute_batch("UPDATE worker_jobs SET status='invalid'")
            .is_err());
        assert!(db.execute_batch("INSERT INTO worker_jobs(id,owner,task_id,task_revision,repository,base_commit,request_id,status,created_at) VALUES ('other','bob','task',0,'repo','base','other','queued',1)").is_err());
        assert!(db.execute_batch("INSERT INTO worker_jobs(id,owner,task_id,task_revision,repository,base_commit,request_id,status,created_at) VALUES ('other','bob','different',0,'repo','base','work','failed',1)").is_err());
        assert_eq!(
            scalar(&db, "SELECT execution_authorized FROM worker_jobs"),
            0
        );
    }

    #[test]
    fn standalone_worker_setup_does_not_assign_a_hub_version_or_enable_subsystems() {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch("PRAGMA user_version=57").unwrap();
        crate::worker::migrate(&db).unwrap();
        crate::worker::migrate(&db).unwrap();
        assert_eq!(version(&db).unwrap(), 57);
        assert_eq!(
            scalar(&db, "SELECT count(*) FROM sqlite_schema WHERE type='table'"),
            1
        );
        assert!(columns(&db, "worker_jobs")
            .unwrap()
            .contains_key("execution_authorized"));
    }
}
