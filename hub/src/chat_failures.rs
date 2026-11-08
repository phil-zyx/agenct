//! Explicit adapter failures, distinct from an unknown transport outcome.
//! A recorded failure never proves the provider did no work and never authorizes retry.
use super::*;

pub(super) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS chat_failures (
 owner TEXT NOT NULL, request_id TEXT NOT NULL,
 code TEXT NOT NULL CHECK(code IN ('runtime_failed','runtime_timeout','invalid_turn','busy','provider_not_configured','sdk_version_mismatch','sdk_unavailable','model_protocol_error','runtime_incomplete')),
 PRIMARY KEY(owner,request_id),
 FOREIGN KEY(owner,request_id) REFERENCES chat_turns(owner,request_id));
CREATE TRIGGER IF NOT EXISTS chat_failures_valid_turn BEFORE INSERT ON chat_failures
WHEN NOT EXISTS(SELECT 1 FROM chat_turns WHERE owner=NEW.owner AND request_id=NEW.request_id AND status='uncertain' AND response IS NULL)
BEGIN SELECT RAISE(ABORT,'failure requires an unresolved turn'); END;
CREATE TRIGGER IF NOT EXISTS chat_failures_no_replace BEFORE INSERT ON chat_failures
WHEN EXISTS(SELECT 1 FROM chat_failures WHERE owner=NEW.owner AND request_id=NEW.request_id)
BEGIN SELECT RAISE(ABORT,'failure receipt already exists'); END;
CREATE TRIGGER IF NOT EXISTS chat_failures_no_update BEFORE UPDATE ON chat_failures
BEGIN SELECT RAISE(ABORT,'failure receipts are immutable'); END;
CREATE TRIGGER IF NOT EXISTS chat_failures_no_delete BEFORE DELETE ON chat_failures
BEGIN SELECT RAISE(ABORT,'failure receipts are durable'); END;";

// Both the HTTP status and the entire error code are checked. These markers are
// Hub-owned static strings: provider bodies, exception messages and HTML never
// become either stored failure metadata or user-facing messages.
const CODES: &[(u16, &str, &str)] = &[
    (
        502,
        "runtime_failed",
        "adapter explicitly reported runtime_failed",
    ),
    (
        504,
        "runtime_timeout",
        "adapter explicitly reported runtime_timeout",
    ),
    (
        400,
        "invalid_turn",
        "adapter explicitly reported invalid_turn",
    ),
    (503, "busy", "adapter explicitly reported busy"),
    (
        502,
        "provider_not_configured",
        "adapter explicitly reported provider_not_configured",
    ),
    (
        502,
        "sdk_version_mismatch",
        "adapter explicitly reported sdk_version_mismatch",
    ),
    (
        502,
        "sdk_unavailable",
        "adapter explicitly reported sdk_unavailable",
    ),
    (
        502,
        "model_protocol_error",
        "adapter explicitly reported model_protocol_error",
    ),
    (
        502,
        "runtime_incomplete",
        "adapter explicitly reported runtime_incomplete",
    ),
];

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AdapterFailure {
    error: String,
}

pub(super) fn adapter_error(status: StatusCode, body: &[u8]) -> Option<Error> {
    let failure: AdapterFailure = serde_json::from_slice(body).ok()?;
    CODES.iter().find_map(|(expected_status, code, marker)| {
        (status.as_u16() == *expected_status && failure.error == *code)
            .then_some(Error(status, marker))
    })
}

pub(super) fn code(error: &Error) -> Option<&'static str> {
    CODES.iter().find_map(|(status, code, marker)| {
        (error.0.as_u16() == *status && error.1 == *marker).then_some(*code)
    })
}

fn public_failure(code: &str) -> Result<serde_json::Value> {
    if !CODES.iter().any(|(_, allowed, _)| *allowed == code) {
        return Err(Error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "invalid stored chat failure",
        ));
    }
    Ok(serde_json::json!({"code":code,"message":"这次没有取得回复，可重新提问。"}))
}

pub(super) fn read(db: &Connection, owner: &str, key: &str) -> Result<Option<serde_json::Value>> {
    let found: Option<String> = db
        .query_row(
            "SELECT code FROM chat_failures WHERE owner=?1 AND request_id=?2",
            params![owner, key],
            |row| row.get(0),
        )
        .optional()?;
    found.map(|code| public_failure(&code)).transpose()
}

/// Only the dispatch that still owns a pending reservation can classify it.
/// Legacy unknown outcomes and completed turns cannot be rewritten by this path.
pub(super) fn record(db: &mut Connection, owner: &str, key: &str, code: &str) -> Result<bool> {
    public_failure(code)?;
    let tx = db.transaction()?;
    let updated = tx.execute(
        "UPDATE chat_turns SET status='uncertain' WHERE owner=?1 AND request_id=?2 AND status='pending' AND response IS NULL",
        params![owner, key],
    )?;
    if updated == 0 {
        return Ok(false);
    }
    tx.execute(
        "INSERT INTO chat_failures(owner,request_id,code) VALUES (?1,?2,?3)",
        params![owner, key, code],
    )?;
    tx.execute(
        "UPDATE messages SET status='failed' WHERE owner=?1 AND status IN ('pending','failed','interrupted','uncertain') AND id IN (SELECT message_id FROM chat_turns WHERE owner=?1 AND request_id=?2)",
        params![owner, key],
    )?;
    tx.commit()?;
    Ok(true)
}

pub(super) fn response(key: &str, failure: serde_json::Value) -> serde_json::Value {
    serde_json::json!({"reply":null,"turn":{"request_id":key,"status":"failed"},"failure":failure})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_exact_safe_status_and_json_pairs_are_classified() {
        for (status, code, _) in CODES {
            let body = serde_json::json!({"error":code}).to_string();
            let error =
                adapter_error(StatusCode::from_u16(*status).unwrap(), body.as_bytes()).unwrap();
            assert_eq!(super::code(&error), Some(*code));
        }
        for (status, body) in [
            (501, r#"{"error":"runtime_failed"}"#),
            (502, "<html>gateway failed</html>"),
            (502, ""),
            (502, r#"{"error":"runtime_timeout"}"#),
            (502, r#"{"error":"arbitrary secret exception"}"#),
            (502, r#"{"error":"runtime_failed","detail":"secret"}"#),
            (
                502,
                r#"{"error":"runtime_failed","error":"runtime_failed"}"#,
            ),
        ] {
            assert!(
                adapter_error(StatusCode::from_u16(status).unwrap(), body.as_bytes()).is_none()
            );
        }
    }

    #[test]
    fn failure_recording_is_atomic_and_cannot_reclassify_complete_or_legacy_turns() {
        let mut db = Connection::open_in_memory().unwrap();
        crate::schema::migrate(&mut db).unwrap();
        db.execute(
            "INSERT INTO sessions(owner,session_id) VALUES ('alice','session')",
            [],
        )
        .unwrap();
        for (key, status) in [
            ("pending", "pending"),
            ("legacy", "uncertain"),
            ("finished", "complete"),
        ] {
            db.execute(
                "INSERT INTO messages(owner,role,content,status) VALUES ('alice','user',?1,?2)",
                params![key, status],
            )
            .unwrap();
            let message = db.last_insert_rowid();
            db.execute("INSERT INTO chat_turns(owner,request_id,message,runtime_profile,status,message_id,response) VALUES ('alice',?1,'hello','{}',?2,?3,?4)", params![key,status,message,(status=="complete").then_some("{}")]).unwrap();
        }
        assert!(!record(&mut db, "alice", "finished", "runtime_failed").unwrap());
        assert!(!record(&mut db, "alice", "legacy", "runtime_failed").unwrap());
        // An insert failure must roll back the pending -> uncertain change too.
        db.execute_batch("CREATE TRIGGER reject_test_failure BEFORE INSERT ON chat_failures BEGIN SELECT RAISE(ABORT,'test storage failure'); END;").unwrap();
        assert!(record(&mut db, "alice", "pending", "runtime_failed").is_err());
        let state: String = db
            .query_row(
                "SELECT status FROM chat_turns WHERE request_id='pending'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(state, "pending");
        db.execute_batch("DROP TRIGGER reject_test_failure")
            .unwrap();
        assert!(record(&mut db, "alice", "pending", "runtime_failed").unwrap());
        assert!(!record(&mut db, "alice", "pending", "runtime_timeout").unwrap());
        assert_eq!(
            read(&db, "alice", "pending").unwrap().unwrap()["code"],
            "runtime_failed"
        );
        assert!(read(&db, "bob", "pending").unwrap().is_none());
        assert!(read(&db, "alice", "finished").unwrap().is_none());
        assert!(read(&db, "alice", "legacy").unwrap().is_none());
        let state: String = db
            .query_row(
                "SELECT status FROM messages WHERE content='pending'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(state, "failed");
    }
}
