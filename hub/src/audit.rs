//! Public task lifecycle audit. Never serializes raw rows or private draft events.
use rusqlite::{params, Connection};
use serde::Serialize;

#[derive(Serialize)]
pub(super) struct Event {
    seq: usize,
    actor: String,
    action: String,
    at: String,
    from_status: Option<String>,
    to_status: Option<String>,
    revision: Option<i64>,
    delivery_seq: Option<i64>,
    provenance: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
}

/// Must be called in the transaction that changes the task. No private content.
pub(super) fn record(
    db: &Connection,
    task: &str,
    actor: &str,
    action: &str,
    from: Option<&str>,
    to: &str,
    revision: i64,
) -> rusqlite::Result<()> {
    record_event(db, task, actor, action, (from, to, revision), None)
}

pub(super) fn record_cancellation(
    db: &Connection,
    task: &str,
    actor: &str,
    from: &str,
    revision: i64,
    reason: &str,
) -> rusqlite::Result<()> {
    record_event(
        db,
        task,
        actor,
        "cancel",
        (Some(from), "canceled", revision),
        Some(reason),
    )
}

fn record_event(
    db: &Connection,
    task: &str,
    actor: &str,
    action: &str,
    transition: (Option<&str>, &str, i64),
    reason: Option<&str>,
) -> rusqlite::Result<()> {
    let (from, to, revision) = transition;
    let reason = if action == "cancel" {
        Some(
            crate::policy::cancellation_reason(reason)
                .map_err(|_| rusqlite::Error::InvalidQuery)?,
        )
    } else {
        if reason.is_some() {
            return Err(rusqlite::Error::InvalidQuery);
        }
        None
    };
    let delivery_seq = if action == "deliver" {
        let seq: Option<i64> = db.query_row(
            "SELECT CASE WHEN count(*)=1 THEN min(seq) ELSE NULL END FROM deliveries
             WHERE task_id=?1 AND status='submitted' AND task_revision=?2 AND author=?3",
            params![task, revision, actor],
            |row| row.get(0),
        )?;
        Some(seq.ok_or(rusqlite::Error::InvalidQuery)?)
    } else {
        None
    };
    db.execute(
        "INSERT INTO events(task_id,actor,action,from_status,to_status,task_revision,revision,delivery_seq,reason)
         VALUES (?1,?2,?3,?4,?5,?6,?6,?7,?8)",
        params![task, actor, action, from, to, revision, delivery_seq, reason],
    )?;
    Ok(())
}

/// Call only after participant authorization while holding the same DB lock.
/// Sequence numbers are derived AFTER filtering and local to this task, so
/// unrelated tasks and private/unknown events cannot create visible gaps.
pub(super) fn read(
    db: &Connection,
    task: &str,
    current_revision: i64,
) -> rusqlite::Result<(Vec<Event>, bool)> {
    let mut q = db.prepare(
        "SELECT e.actor,e.action,e.at,e.from_status,e.to_status,e.revision,e.delivery_seq,CASE WHEN e.action='cancel' THEN e.reason ELSE NULL END
         FROM events e JOIN tasks t ON t.id=e.task_id
         WHERE e.task_id=?1 AND e.actor IN (t.requester,t.owner)
         AND e.action IN ('created','accept','deliver','approve','return','cancel')
         ORDER BY e.id",
    )?;
    let rows = q.query_map([task], |row| {
        let revision: Option<i64> = row.get(5)?;
        Ok(Event {
            seq: 0,
            actor: row.get(0)?,
            action: row.get(1)?,
            at: row.get(2)?,
            from_status: row.get(3)?,
            to_status: row.get(4)?,
            revision,
            delivery_seq: row.get(6)?,
            reason: row.get(7)?,
            provenance: if revision.is_some() {
                "recorded"
            } else {
                "legacy"
            },
        })
    })?;
    let mut events = rows.collect::<rusqlite::Result<Vec<_>>>()?;
    let incomplete = events.last().and_then(|e| e.revision) != Some(current_revision)
        || events.is_empty()
        || events
            .iter()
            .enumerate()
            .any(|(index, e)| e.revision != Some(index as i64));
    for (index, event) in events.iter_mut().enumerate() {
        event.seq = index + 1;
    }
    Ok((events, incomplete))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn database() -> Connection {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch(
            "CREATE TABLE tasks(id TEXT,requester TEXT,owner TEXT);
          INSERT INTO tasks VALUES ('one','alice','bob'),('two','alice','eve');
          CREATE TABLE events(id INTEGER PRIMARY KEY,task_id TEXT,actor TEXT,action TEXT,
          at TEXT DEFAULT CURRENT_TIMESTAMP,from_status TEXT,to_status TEXT,task_revision INTEGER,revision INTEGER,delivery_seq INTEGER,reason TEXT);
          CREATE TABLE deliveries(task_id TEXT,status TEXT,task_revision INTEGER,seq INTEGER,author TEXT);",
        )
        .unwrap();
        db
    }
    #[test]
    fn public_sequences_hide_private_unknown_and_other_task_rows() {
        let db = database();
        record(&db, "one", "alice", "created", None, "awaiting_owner", 0).unwrap();
        record(&db, "one", "bob", "private_draft", None, "working", 99).unwrap();
        record(&db, "two", "alice", "created", None, "awaiting_owner", 0).unwrap();
        record(&db, "one", "eve", "accept", None, "working", 99).unwrap();
        record(
            &db,
            "one",
            "bob",
            "accept",
            Some("awaiting_owner"),
            "working",
            1,
        )
        .unwrap();
        let (events, incomplete) = read(&db, "one", 1).unwrap();
        assert!(!incomplete);
        assert_eq!(events.len(), 2);
        assert_eq!(events.iter().map(|e| e.seq).collect::<Vec<_>>(), vec![1, 2]);
        let json = serde_json::to_string(&events).unwrap();
        assert!(!json.contains("private_draft"));
        assert!(!json.contains("eve"));
        assert!(!json.contains("99"));
    }
    #[test]
    fn links_only_submitted_deliveries_at_recorded_revision() {
        let db = database();
        db.execute_batch("INSERT INTO deliveries VALUES ('one','draft',2,42,'bob'),('one','submitted',2,1,'bob'),('two','submitted',2,3,'bob');").unwrap();
        record(
            &db,
            "one",
            "bob",
            "deliver",
            Some("working"),
            "ready_for_acceptance",
            2,
        )
        .unwrap();
        let (events, incomplete) = read(&db, "one", 2).unwrap();
        assert!(incomplete); // No creation record: do not claim a complete history.
        assert_eq!(events[0].delivery_seq, Some(1));
        assert_eq!(events[0].revision, Some(2));
    }
    #[test]
    fn legacy_history_is_not_fabricated() {
        let db = database();
        db.execute_batch(
            "INSERT INTO events(task_id,actor,action) VALUES ('one','bob','deliver');
          INSERT INTO deliveries VALUES ('one','submitted',2,1,'bob');",
        )
        .unwrap();
        let (events, incomplete) = read(&db, "one", 2).unwrap();
        assert!(incomplete);
        assert_eq!(events[0].provenance, "legacy");
        assert_eq!(events[0].revision, None);
        assert_eq!(events[0].from_status, None);
        assert_eq!(events[0].to_status, None);
        assert_eq!(events[0].delivery_seq, None);
    }
    #[test]
    fn rolled_back_mutation_leaves_no_audit_record() {
        let mut db = database();
        {
            let tx = db.transaction().unwrap();
            record(&tx, "one", "alice", "created", None, "awaiting_owner", 0).unwrap();
        }
        assert!(read(&db, "one", 0).unwrap().0.is_empty());
    }
    #[test]
    fn audit_cancel_writer_requires_reason_and_read_never_projects_other_action_reasons() {
        let db = database();
        assert!(record(
            &db,
            "one",
            "alice",
            "cancel",
            Some("working"),
            "canceled",
            1
        )
        .is_err());
        assert!(record_cancellation(&db, "one", "alice", "working", 1, "  ").is_err());
        record_cancellation(
            &db,
            "one",
            "alice",
            "working",
            1,
            " explicit shared reason ",
        )
        .unwrap();
        db.execute_batch("INSERT INTO events(task_id,actor,action,revision,reason) VALUES ('one','bob','accept',2,'PRIVATE FIELD MUST NOT PROJECT')").unwrap();
        let json = serde_json::to_value(read(&db, "one", 2).unwrap().0).unwrap();
        assert_eq!(json[0]["reason"], "explicit shared reason");
        assert!(json[1].get("reason").is_none());
        assert!(!json.to_string().contains("PRIVATE FIELD"));
    }
}
