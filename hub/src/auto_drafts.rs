//! Durable intent before reservation; never a retry queue for model requests.
use crate::{drafts, profiles, Error, Hub, Result};
use axum::http::StatusCode;
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};
use std::time::Duration;

const MAX_ATTEMPTS: i64 = 40;
const MAX_AGE: i64 = 900;

pub(super) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS auto_draft_intents (
 owner TEXT NOT NULL, task_id TEXT NOT NULL REFERENCES tasks(id),
 task_revision INTEGER NOT NULL CHECK(task_revision>=0), request_id TEXT NOT NULL,
 status TEXT NOT NULL CHECK(status IN ('queued','reserved','skipped')),
 attempts INTEGER NOT NULL DEFAULT 0 CHECK(attempts>=0 AND attempts<=40),
 next_attempt_at INTEGER NOT NULL DEFAULT (unixepoch()),
 created_at INTEGER NOT NULL DEFAULT (unixepoch()), reason TEXT,
 PRIMARY KEY(owner,task_id,task_revision), UNIQUE(owner,request_id));
CREATE INDEX IF NOT EXISTS auto_draft_due ON auto_draft_intents(status,next_attempt_at);
CREATE TRIGGER IF NOT EXISTS auto_draft_binding_immutable
 BEFORE UPDATE OF owner,task_id,task_revision,request_id,created_at ON auto_draft_intents
 BEGIN SELECT RAISE(ABORT,'automatic intent binding is immutable'); END;
CREATE TRIGGER IF NOT EXISTS auto_draft_terminal BEFORE UPDATE ON auto_draft_intents
 WHEN OLD.status!='queued' OR NEW.attempts<OLD.attempts
 BEGIN SELECT RAISE(ABORT,'automatic intent is terminal'); END;
CREATE TRIGGER IF NOT EXISTS auto_draft_no_replace BEFORE INSERT ON auto_draft_intents
 WHEN EXISTS(SELECT 1 FROM auto_draft_intents WHERE owner=NEW.owner AND
 ((task_id=NEW.task_id AND task_revision=NEW.task_revision) OR request_id=NEW.request_id))
 BEGIN SELECT RAISE(ABORT,'automatic intent already exists'); END;
CREATE TRIGGER IF NOT EXISTS auto_draft_no_delete BEFORE DELETE ON auto_draft_intents
 BEGIN SELECT RAISE(ABORT,'automatic intents are durable'); END;
CREATE TRIGGER IF NOT EXISTS auto_draft_opt_out_insert AFTER INSERT ON collaboration_profiles
 WHEN json_extract(NEW.profile_json,'$.permissions.auto_prepare_drafts')=0
 BEGIN UPDATE auto_draft_intents SET status='skipped',reason='auto_disabled'
 WHERE owner=NEW.owner AND status='queued'; END;
CREATE TRIGGER IF NOT EXISTS auto_draft_opt_out_update AFTER UPDATE ON collaboration_profiles
 WHEN json_extract(NEW.profile_json,'$.permissions.auto_prepare_drafts')=0
 BEGIN UPDATE auto_draft_intents SET status='skipped',reason='auto_disabled'
 WHERE owner=NEW.owner AND status='queued'; END;
CREATE TRIGGER IF NOT EXISTS auto_draft_member_insert AFTER INSERT ON member_profiles
 WHEN NEW.status!='active'
 BEGIN UPDATE auto_draft_intents SET status='skipped',reason='owner_disabled'
 WHERE owner=NEW.owner AND status='queued'; END;
CREATE TRIGGER IF NOT EXISTS auto_draft_member_update AFTER UPDATE ON member_profiles
 WHEN NEW.status!='active'
 BEGIN UPDATE auto_draft_intents SET status='skipped',reason='owner_disabled'
 WHERE owner=NEW.owner AND status='queued'; END;";

pub(super) fn enqueue(db: &Connection, owner: &str, task: &str, revision: i64) -> Result<()> {
    db.execute("INSERT INTO auto_draft_intents(owner,task_id,task_revision,request_id,status) VALUES (?1,?2,?3,?4,'queued')",
        params![owner,task,revision,drafts::auto_request_id(task,revision)])?;
    Ok(())
}

pub(super) fn revoke_task(db: &Connection, task: &str) -> Result<()> {
    db.execute("UPDATE auto_draft_intents SET status='skipped',reason='task_changed' WHERE task_id=?1 AND status='queued'", [task])?;
    Ok(())
}

pub(super) fn projection(
    db: &Connection,
    owner: &str,
    task: &str,
    revision: i64,
) -> Result<Option<Value>> {
    Ok(db.query_row("SELECT request_id,status,reason,attempts FROM auto_draft_intents WHERE owner=?1 AND task_id=?2 AND task_revision=?3",
        params![owner,task,revision], |r| {
            let status: String = r.get(1)?;
            // Reserved without a readable receipt is never evidence of no dispatch.
            Ok(json!({"request_id":r.get::<_,String>(0)?,"status":if status=="reserved" {"uncertain"} else {&status},
                "automatic":true,"reason":r.get::<_,Option<String>>(2)?,"attempts":r.get::<_,i64>(3)?}))
        }).optional()?)
}

fn skip(h: &Hub, owner: &str, request: &str, reason: &str) -> Result<()> {
    h.db()?.execute("UPDATE auto_draft_intents SET status='skipped',reason=?3 WHERE owner=?1 AND request_id=?2 AND status='queued'",params![owner,request,reason])?;
    Ok(())
}

/// Consumes one persisted scheduling attempt, not one model retry. A crash before
/// reservation leaves queued work eligible later; the claim delays duplicate polls.
pub(super) async fn dispatch(h: &Hub, owner: &str, task: &str, revision: i64) -> Result<()> {
    let Ok(_capacity) = h.auto_draft_slots.clone().try_acquire_owned() else {
        return Ok(());
    };
    let request = drafts::auto_request_id(task, revision);
    let attempt = {
        let db = h.db()?;
        db.execute("UPDATE auto_draft_intents SET status='skipped',reason='retry_exhausted' WHERE owner=?1 AND request_id=?2 AND status='queued' AND (attempts>=?3 OR created_at+?4<=unixepoch())",
            params![owner,request,MAX_ATTEMPTS,MAX_AGE])?;
        if db.execute("UPDATE auto_draft_intents SET attempts=attempts+1,next_attempt_at=unixepoch()+30 WHERE owner=?1 AND request_id=?2 AND status='queued' AND next_attempt_at<=unixepoch()", params![owner,request])? != 1 {
            return Ok(());
        }
        db.query_row(
            "SELECT attempts FROM auto_draft_intents WHERE owner=?1 AND request_id=?2",
            params![owner, request],
            |r| r.get::<_, i64>(0),
        )?
    };
    let prepared = (|| -> Result<_> {
        drafts::auto_authorized(h, owner, task, revision)?;
        let db = h.db()?;
        let summary: String = db.query_row(
            "SELECT summary FROM tasks WHERE id=?1 AND owner=?2",
            params![task, owner],
            |r| r.get(0),
        )?;
        profiles::recommend(&db, owner, &summary)
    })();
    let result = match prepared {
        Ok(profile) => drafts::generate_auto(h, owner, task, revision, profile).await,
        Err(error) => Err(error),
    };
    // After reservation, every update below is a no-op. Receipt outcome is the
    // only source of truth; generic failures never mean safe to call again.
    match result {
        Err(Error(StatusCode::CONFLICT, drafts::BUSY)) if attempt < MAX_ATTEMPTS => {
            let delay = (2_i64 << (attempt - 1).min(4)).min(30);
            h.db()?.execute("UPDATE auto_draft_intents SET reason='busy',next_attempt_at=unixepoch()+?3 WHERE owner=?1 AND request_id=?2 AND status='queued'",params![owner,request,delay])?;
        }
        Err(error) => skip(
            h,
            owner,
            &request,
            match error.1 {
                drafts::BUSY => "retry_exhausted",
                drafts::AUTO_DISABLED => "auto_disabled",
                drafts::STALE | drafts::STATE => "task_changed",
                drafts::SUPERSEDED => "superseded",
                "not found" => "owner_disabled",
                _ => "unavailable",
            },
        )?,
        Ok(_) => skip(h, owner, &request, "superseded")?,
    }
    Ok(())
}

impl Hub {
    /// Run alongside the HTTP server after all builders and startup validation.
    /// Dropping this future aborts every automatic job it owns. Reserved work
    /// remains uncertain; queued work survives. No detached Hub loop is created.
    pub async fn run_auto_drafts(&self) {
        if self.validate_startup().is_err() {
            return;
        }
        let mut batch = tokio::task::JoinSet::new();
        let mut tick = tokio::time::interval(Duration::from_millis(1500));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = tick.tick() => {},
                _ = self.auto_draft_notify.notified() => {},
                result = batch.join_next(), if !batch.is_empty() => {
                    if !matches!(result, Some(Ok(Ok(())))) {
                        eprintln!("Automatic draft dispatcher could not update durable queue");
                    }
                }
            }
            match self.due_auto_drafts(4 - batch.len()) {
                Ok(due) => {
                    for (owner, task, revision) in due {
                        let h = self.clone();
                        batch.spawn(async move { dispatch(&h, &owner, &task, revision).await });
                    }
                }
                Err(_) => eprintln!("Automatic draft dispatcher could not read durable queue"),
            }
        }
    }

    fn due_auto_drafts(&self, limit: usize) -> Result<Vec<(String, String, i64)>> {
        let db = self.db()?;
        if !self.auto_draft {
            db.execute("UPDATE auto_draft_intents SET status='skipped',reason='auto_disabled' WHERE status='queued'", [])?;
            return Ok(Vec::new());
        }
        let mut q = db.prepare("SELECT owner,task_id,task_revision FROM auto_draft_intents WHERE status='queued' AND next_attempt_at<=unixepoch() ORDER BY next_attempt_at,rowid LIMIT ?1")?;
        let rows = q
            .query_map([limit], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    #[cfg(test)]
    async fn dispatch_auto_draft_batch(&self) -> Result<()> {
        let mut batch = tokio::task::JoinSet::new();
        for (owner, task, revision) in self.due_auto_drafts(4)? {
            let h = self.clone();
            batch.spawn(async move { dispatch(&h, &owner, &task, revision).await });
        }
        while let Some(result) = batch.join_next().await {
            result.map_err(|_| {
                Error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "automatic draft dispatcher interrupted",
                )
            })??;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Config, Runtime, UserConfig};
    use sha2::{Digest, Sha256};
    fn config(path: &std::path::Path) -> Config {
        Config {
            bind: "127.0.0.1:0".parse().unwrap(),
            database: path.into(),
            users: ["alice", "bob"]
                .map(|id| UserConfig {
                    id: id.into(),
                    token_sha256: format!("{:x}", Sha256::digest(id.as_bytes())),
                })
                .into(),
            runtime: Runtime::Mock,
        }
    }
    fn fixture() -> Hub {
        let h = Hub::open(config(std::path::Path::new(":memory:")))
            .unwrap()
            .with_auto_draft(true);
        seed(&h);
        h
    }
    fn seed(h: &Hub) {
        let db = h.db().unwrap();
        db.execute("INSERT INTO tasks(id,requester,owner,summary,status,revision) VALUES ('task','alice','bob','shared','working',1)",[]).unwrap();
        enqueue(&db, "bob", "task", 1).unwrap();
    }
    fn state(h: &Hub) -> Value {
        projection(&h.db().unwrap(), "bob", "task", 1)
            .unwrap()
            .unwrap()
    }
    fn count(h: &Hub) -> i64 {
        h.db()
            .unwrap()
            .query_row("SELECT count(*) FROM draft_turns", [], |r| r.get(0))
            .unwrap()
    }
    fn due(h: &Hub) {
        h.db()
            .unwrap()
            .execute(
                "UPDATE auto_draft_intents SET next_attempt_at=0 WHERE status='queued'",
                [],
            )
            .unwrap();
    }

    #[tokio::test]
    async fn queued_work_survives_restart_but_reserved_work_never_replays() {
        let root = std::env::temp_dir().join(format!("auto-queue-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let path = root.join("hub.db");
        let h = Hub::open(config(&path)).unwrap().with_auto_draft(true);
        seed(&h);
        let gate = h.gate("bob").unwrap().lock_owned().await;
        dispatch(&h, "bob", "task", 1).await.unwrap();
        assert_eq!(state(&h)["status"], "queued");
        assert_eq!(state(&h)["attempts"], 1);
        assert_eq!(count(&h), 0);
        drop(gate);
        drop(h);
        let h = Hub::open(config(&path)).unwrap().with_auto_draft(true);
        assert_eq!(state(&h)["attempts"], 1);
        due(&h);
        h.dispatch_auto_draft_batch().await.unwrap();
        assert_eq!(state(&h)["status"], "uncertain"); // projection defers reserved outcome to receipt
        assert_eq!(count(&h), 1);
        drop(h);
        let h = Hub::open(config(&path)).unwrap().with_auto_draft(true);
        h.dispatch_auto_draft_batch().await.unwrap();
        dispatch(&h, "bob", "task", 1).await.unwrap();
        assert_eq!(count(&h), 1);
        assert_eq!(
            h.db()
                .unwrap()
                .query_row("SELECT status FROM draft_turns", [], |r| r
                    .get::<_, String>(0))
                .unwrap(),
            "complete"
        );
        drop(h);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn concurrent_dispatches_reserve_once_and_never_backfill_old_work() {
        let h = fixture();
        let (a, b) = tokio::join!(
            dispatch(&h, "bob", "task", 1),
            dispatch(&h, "bob", "task", 1)
        );
        a.unwrap();
        b.unwrap();
        assert_eq!(count(&h), 1);
        h.db().unwrap().execute("INSERT INTO tasks(id,requester,owner,summary,status,revision) VALUES ('legacy','alice','bob','old','working',1)",[]).unwrap();
        h.dispatch_auto_draft_batch().await.unwrap();
        dispatch(&h, "bob", "legacy", 1).await.unwrap();
        assert_eq!(count(&h), 1);
    }

    #[tokio::test]
    async fn busy_budget_is_persisted_and_exhaustion_is_terminal() {
        let h = fixture();
        let gate = h.gate("bob").unwrap().lock_owned().await;
        h.db()
            .unwrap()
            .execute("UPDATE auto_draft_intents SET attempts=39", [])
            .unwrap();
        dispatch(&h, "bob", "task", 1).await.unwrap();
        assert_eq!(state(&h)["attempts"], 40);
        assert_eq!(state(&h)["status"], "skipped");
        assert_eq!(state(&h)["reason"], "retry_exhausted");
        drop(gate);
        dispatch(&h, "bob", "task", 1).await.unwrap();
        assert_eq!(count(&h), 0);
    }

    #[tokio::test]
    async fn expiry_and_reserved_without_receipt_are_never_dispatched() {
        for status in ["queued", "reserved"] {
            let h = Hub::open(config(std::path::Path::new(":memory:")))
                .unwrap()
                .with_auto_draft(true);
            h.db().unwrap().execute("INSERT INTO tasks(id,requester,owner,summary,status,revision) VALUES ('task','alice','bob','shared','working',1)",[]).unwrap();
            h.db().unwrap().execute("INSERT INTO auto_draft_intents(owner,task_id,task_revision,request_id,status,created_at) VALUES ('bob','task',1,'auto-task-r1',?1,unixepoch()-901)",[status]).unwrap();
            h.dispatch_auto_draft_batch().await.unwrap();
            assert_eq!(count(&h), 0);
            assert_eq!(
                state(&h)["status"],
                if status == "queued" {
                    "skipped"
                } else {
                    "uncertain"
                }
            );
        }
    }

    #[tokio::test]
    async fn global_disable_optout_member_revocation_and_task_changes_are_terminal() {
        for reason in ["global", "permission", "member", "task"] {
            let h = fixture();
            match reason {
                "global" => h
                    .clone()
                    .with_auto_draft(false)
                    .dispatch_auto_draft_batch()
                    .await
                    .unwrap(),
                "permission" => {
                    let mut p = crate::collaboration::Profile::default();
                    p.permissions.auto_prepare_drafts = false;
                    crate::collaboration::write(&h.db().unwrap(), "bob", &p).unwrap();
                    p.permissions.auto_prepare_drafts = true;
                    crate::collaboration::write(&h.db().unwrap(), "bob", &p).unwrap();
                }
                "member" => {
                    h.db().unwrap().execute("INSERT INTO member_profiles(owner,display_name,status) VALUES ('bob','Bob','disabled')",[]).unwrap();
                    h.db()
                        .unwrap()
                        .execute(
                            "UPDATE member_profiles SET status='active' WHERE owner='bob'",
                            [],
                        )
                        .unwrap();
                }
                _ => {
                    revoke_task(&h.db().unwrap(), "task").unwrap();
                }
            }
            h.dispatch_auto_draft_batch().await.unwrap();
            assert_eq!(count(&h), 0, "{reason}");
            assert_eq!(state(&h)["status"], "skipped", "{reason}");
        }
    }

    #[tokio::test]
    async fn manual_reservation_supersedes_queued_intent_in_the_same_transaction() {
        let h = fixture();
        let input =
            serde_json::from_value(json!({"request_id":"manual","expected_revision":1})).unwrap();
        drafts::generate(&h, "bob", "task", input).await.unwrap();
        assert_eq!(state(&h)["status"], "skipped");
        assert_eq!(state(&h)["reason"], "superseded");
        h.dispatch_auto_draft_batch().await.unwrap();
        assert_eq!(count(&h), 1);
    }

    #[tokio::test]
    async fn cancelling_dispatcher_releases_database_and_recovers_uncertain_without_replay() {
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };
        let calls = Arc::new(AtomicUsize::new(0));
        let started = Arc::new(tokio::sync::Notify::new());
        let (counter, entered) = (calls.clone(), started.clone());
        let adapter = axum::Router::new().route(
            "/v1/turn",
            axum::routing::post(move || {
                let (counter, entered) = (counter.clone(), entered.clone());
                async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    entered.notify_one();
                    std::future::pending::<axum::Json<Value>>().await
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1/turn", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, adapter).await.unwrap() });
        let root = std::env::temp_dir().join(format!("auto-cancel-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let path = root.join("hub.db");
        let mut c = config(&path);
        c.runtime = Runtime::Dsh {
            url,
            token: "fake-internal-token".into(),
        };
        let h = Hub::open(c).unwrap().with_auto_draft(true);
        seed(&h);
        let dispatcher = tokio::spawn(async move { h.run_auto_drafts().await });
        tokio::time::timeout(Duration::from_secs(5), started.notified())
            .await
            .unwrap();
        dispatcher.abort();
        assert!(dispatcher.await.unwrap_err().is_cancelled());
        // JoinSet abort runs child destructors at the next scheduling opportunity.
        let h = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Ok(h) = Hub::open(config(&path)) {
                    break h.with_auto_draft(true);
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            h.db()
                .unwrap()
                .query_row("SELECT status FROM draft_turns", [], |r| r
                    .get::<_, String>(0))
                .unwrap(),
            "uncertain"
        );
        h.dispatch_auto_draft_batch().await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(count(&h), 1);
        drop(h);
        server.abort();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rollback_does_not_leave_an_intent_and_terminal_bindings_cannot_be_reopened() {
        let h = fixture();
        let mut db = h.db().unwrap();
        let tx = db.transaction().unwrap();
        tx.execute("UPDATE tasks SET revision=2 WHERE id='task'", [])
            .unwrap();
        enqueue(&tx, "bob", "task", 2).unwrap();
        tx.rollback().unwrap();
        assert!(projection(&db, "bob", "task", 2).unwrap().is_none());
        assert!(db
            .execute("UPDATE auto_draft_intents SET task_revision=9", [])
            .is_err());
        revoke_task(&db, "task").unwrap();
        assert!(db
            .execute("UPDATE auto_draft_intents SET status='queued'", [])
            .is_err());
        assert!(db.execute("INSERT OR REPLACE INTO auto_draft_intents(owner,task_id,task_revision,request_id,status) VALUES ('bob','task',1,'auto-task-r1','queued')",[]).is_err());
    }
}
