//! Durable, output-only worker protocol. This module never executes code or publishes changes.
//! Callers must authenticate users/runners and provide server-owned policy and clock values.
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use uuid::Uuid;

#[derive(Debug)]
pub enum Error {
    Database(rusqlite::Error),
    Invalid(&'static str),
    Denied,
    Conflict,
    LeaseLost,
    ExecutorUnavailable,
}
impl From<rusqlite::Error> for Error {
    fn from(value: rusqlite::Error) -> Self {
        Self::Database(value)
    }
}
pub type Result<T> = std::result::Result<T, Error>;

/// Construct only from trusted server configuration, never from model or browser input.
/// Selecting an executor is NOT proof of isolation. The operator must deploy and audit it.
#[derive(Clone, Debug)]
pub struct Policy {
    pub executor_id: Option<String>,
    pub repositories: BTreeSet<String>,
    pub required_checks: BTreeSet<String>,
}
impl Policy {
    fn executor(&self) -> Result<&str> {
        self.executor_id
            .as_deref()
            .filter(|s| !s.is_empty())
            .ok_or(Error::ExecutorUnavailable)
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct JobRequest {
    pub task_id: String,
    pub task_revision: i64,
    /// Server-defined repository key, not a URL or filesystem path.
    pub repository: String,
    pub base_commit: String,
    pub request_id: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    pub id: String,
    pub owner: String,
    pub request: JobRequest,
    pub status: String,
    pub attempt: i64,
}
#[derive(Debug, Clone)]
pub struct Lease {
    pub job: Job,
    pub executor: String,
    pub token: String,
    pub expires_at: i64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Check {
    pub name: String,
    pub commit: String,
    pub passed: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Candidate {
    pub repository: String,
    pub base_commit: String,
    pub head_commit: String,
    pub patch: String,
    pub checks: Vec<Check>,
}
fn hash(s: &str) -> String {
    format!("{:x}", Sha256::digest(s.as_bytes()))
}
fn commit(s: &str) -> bool {
    matches!(s.len(), 40 | 64)
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn valid_key(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-.".contains(&b))
}
fn job_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Job> {
    Ok(Job {
        id: r.get(0)?,
        owner: r.get(1)?,
        request: JobRequest {
            task_id: r.get(2)?,
            task_revision: r.get(3)?,
            repository: r.get(4)?,
            base_commit: r.get(5)?,
            request_id: r.get(6)?,
        },
        status: r.get(7)?,
        attempt: r.get(8)?,
    })
}
const SELECT: &str = "SELECT id,owner,task_id,task_revision,repository,base_commit,request_id,status,attempt FROM worker_jobs";

pub fn migrate(db: &Connection) -> Result<()> {
    db.execute_batch("CREATE TABLE IF NOT EXISTS worker_jobs (
      id TEXT PRIMARY KEY, owner TEXT NOT NULL, task_id TEXT NOT NULL,
      task_revision INTEGER NOT NULL, repository TEXT NOT NULL, base_commit TEXT NOT NULL,
      request_id TEXT NOT NULL, status TEXT NOT NULL CHECK(status IN ('queued','running','review_ready','failed','cancelled')),
      attempt INTEGER NOT NULL DEFAULT 0, executor TEXT, lease_hash TEXT, lease_until INTEGER,
      candidate TEXT, candidate_sha256 TEXT, failure TEXT, created_at INTEGER NOT NULL,
      UNIQUE(owner,request_id));
      CREATE UNIQUE INDEX IF NOT EXISTS worker_one_active_task ON worker_jobs(task_id)
      WHERE status IN ('queued','running','review_ready');")?;
    Ok(())
}
fn authorized(db: &Connection, job: &Job) -> Result<bool> {
    Ok(db.query_row("SELECT EXISTS(SELECT 1 FROM tasks WHERE id=?1 AND owner=?2 AND revision=?3 AND status='working')",params![job.request.task_id,job.owner,job.request.task_revision],|r|r.get(0))?)
}
/// Explicit owner request, after task acceptance; not called automatically on model output.
/// request_id is owner-scoped and immutable. A replay returns the original job, never requeues it.
pub fn enqueue(
    db: &mut Connection,
    actor: &str,
    request: JobRequest,
    policy: &Policy,
    now: i64,
) -> Result<Job> {
    policy.executor()?;
    if !valid_key(&request.request_id)
        || !valid_key(&request.repository)
        || !commit(&request.base_commit)
        || request.task_revision < 0
        || now < 0
    {
        return Err(Error::Invalid("invalid job request"));
    }
    if !policy.repositories.contains(&request.repository) {
        return Err(Error::Denied);
    }
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    if let Some(old) = tx
        .query_row(
            &format!("{SELECT} WHERE owner=?1 AND request_id=?2"),
            params![actor, request.request_id],
            job_row,
        )
        .optional()?
    {
        return if old.request == request {
            Ok(old)
        } else {
            Err(Error::Conflict)
        };
    }
    let job = Job {
        id: Uuid::new_v4().to_string(),
        owner: actor.to_owned(),
        request,
        status: "queued".into(),
        attempt: 0,
    };
    if !authorized(&tx, &job)? {
        return Err(Error::Denied);
    }
    // A new accepted task revision supersedes any old candidate/lease before uniqueness checks.
    tx.execute("UPDATE worker_jobs SET status='cancelled',lease_hash=NULL,lease_until=NULL WHERE task_id=?1 AND status IN ('queued','running','review_ready') AND NOT EXISTS(SELECT 1 FROM tasks t WHERE t.id=worker_jobs.task_id AND t.owner=worker_jobs.owner AND t.revision=worker_jobs.task_revision AND t.status='working')", [&job.request.task_id])?;
    if tx.query_row("SELECT EXISTS(SELECT 1 FROM worker_jobs WHERE task_id=?1 AND status IN ('queued','running','review_ready'))",[&job.request.task_id],|r|r.get::<_,bool>(0))? {return Err(Error::Conflict);}
    tx.execute("INSERT INTO worker_jobs(id,owner,task_id,task_revision,repository,base_commit,request_id,status,created_at) VALUES (?1,?2,?3,?4,?5,?6,?7,'queued',?8)",params![job.id,job.owner,job.request.task_id,job.request.task_revision,job.request.repository,job.request.base_commit,job.request.request_id,now])?;
    tx.commit()?;
    Ok(job)
}
/// Runner identity must come from a separately authenticated internal channel, not a body field.
/// Lease duration is bounded; 3 expired/failed attempts terminate a job. No external effects are allowed.
pub fn claim(
    db: &mut Connection,
    runner: &str,
    policy: &Policy,
    now: i64,
    duration: i64,
) -> Result<Option<Lease>> {
    if policy.executor()? != runner {
        return Err(Error::Denied);
    }
    if now < 0 || !(1..=300).contains(&duration) || now.checked_add(duration).is_none() {
        return Err(Error::Invalid("invalid lease time"));
    }
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute("UPDATE worker_jobs SET status='cancelled',lease_hash=NULL,lease_until=NULL WHERE status IN ('queued','running','review_ready') AND NOT EXISTS(SELECT 1 FROM tasks t WHERE t.id=worker_jobs.task_id AND t.owner=worker_jobs.owner AND t.revision=worker_jobs.task_revision AND t.status='working')",[])?;
    tx.execute("UPDATE worker_jobs SET status='failed',failure='attempt limit',lease_hash=NULL,lease_until=NULL WHERE status='running' AND lease_until<=?1 AND attempt>=3",[now])?;
    let jobs = {
        let mut q=tx.prepare(&format!("{SELECT} WHERE status='queued' OR (status='running' AND lease_until<=?1 AND attempt<3) ORDER BY created_at,id"))?;
        let rows = q.query_map([now], job_row)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()?
    };
    for mut job in jobs {
        if !policy.repositories.contains(&job.request.repository) {
            tx.execute("UPDATE worker_jobs SET status='cancelled',lease_hash=NULL,lease_until=NULL WHERE id=?1",[&job.id])?;
            continue;
        }
        let token = Uuid::new_v4().to_string();
        let expires_at = now + duration;
        tx.execute("UPDATE worker_jobs SET status='running',attempt=attempt+1,executor=?2,lease_hash=?3,lease_until=?4 WHERE id=?1",params![job.id,runner,hash(&token),expires_at])?;
        job.attempt += 1;
        job.status = "running".into();
        tx.commit()?;
        return Ok(Some(Lease {
            job,
            executor: runner.into(),
            token,
            expires_at,
        }));
    }
    tx.commit()?;
    Ok(None)
}
fn live_job(db: &Connection, lease: &Lease, policy: &Policy, now: i64) -> Result<Job> {
    if now < 0 || policy.executor()? != lease.executor {
        return Err(Error::Denied);
    }
    let job=db.query_row(&format!("{SELECT} WHERE id=?1 AND status='running' AND executor=?2 AND lease_hash=?3 AND lease_until>?4"),params![lease.job.id,lease.executor,hash(&lease.token),now],job_row).optional()?.ok_or(Error::LeaseLost)?;
    if !policy.repositories.contains(&job.request.repository) || !authorized(db, &job)? {
        return Err(Error::Denied);
    }
    Ok(job)
}
/// For a trusted consumer's cancellation loop; never exposes lease secrets.
pub fn lease_is_live(db: &Connection, lease: &Lease, policy: &Policy, now: i64) -> Result<bool> {
    match live_job(db, lease, policy, now) {
        Ok(_) => Ok(true),
        Err(Error::Denied | Error::LeaseLost | Error::ExecutorUnavailable) => Ok(false),
        Err(error) => Err(error),
    }
}
/// Stores a candidate for HUMAN review. Passing checks never approves, publishes or completes a task.
/// Evidence must be obtained by the trusted executor, never accepted directly from an LLM.
pub(crate) fn finish(
    db: &mut Connection,
    lease: &Lease,
    policy: &Policy,
    candidate: &Candidate,
    now: i64,
) -> Result<String> {
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let job = live_job(&tx, lease, policy, now)?;
    if candidate.repository != job.request.repository
        || candidate.base_commit != job.request.base_commit
        || !commit(&candidate.head_commit)
        || candidate.head_commit == candidate.base_commit
        || candidate.patch.is_empty()
        || candidate.patch.len() > 1_048_576
        || candidate.checks.len() > 100
    {
        return Err(Error::Invalid("candidate binding or size"));
    }
    let mut names = BTreeSet::new();
    for check in &candidate.checks {
        if !valid_key(&check.name)
            || !names.insert(check.name.clone())
            || !check.passed
            || check.commit != candidate.head_commit
        {
            return Err(Error::Invalid(
                "checks must uniquely pass on exact head commit",
            ));
        }
    }
    if policy.required_checks.is_empty() || !policy.required_checks.is_subset(&names) {
        return Err(Error::Invalid("required checks missing"));
    }
    let payload =
        serde_json::to_string(candidate).map_err(|_| Error::Invalid("candidate serialization"))?;
    let digest = hash(&payload);
    tx.execute("UPDATE worker_jobs SET status='review_ready',candidate=?2,candidate_sha256=?3,lease_hash=NULL,lease_until=NULL WHERE id=?1",params![job.id,payload,digest])?;
    tx.commit()?;
    Ok(digest)
}
pub fn fail(db: &mut Connection, lease: &Lease, policy: &Policy, now: i64) -> Result<()> {
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let job = live_job(&tx, lease, policy, now)?;
    // No untrusted logs/secrets stored as failure text. Retry is only after a lease expires.
    tx.execute("UPDATE worker_jobs SET status='failed',failure='executor failed',lease_hash=NULL,lease_until=NULL WHERE id=?1",[job.id])?;
    tx.commit()?;
    Ok(())
}
pub fn cancel(db: &Connection, actor: &str, id: &str) -> Result<()> {
    let n=db.execute("UPDATE worker_jobs SET status='cancelled',lease_hash=NULL,lease_until=NULL WHERE id=?1 AND owner=?2 AND status IN ('queued','running','review_ready','cancelled')",params![id,actor])?;
    if n == 0 {
        return Err(Error::Denied);
    }
    Ok(())
}
/// Only the enqueuing owner can inspect a candidate. Task delivery is a separate explicit action.
pub fn inspect(db: &Connection, actor: &str, id: &str) -> Result<(Job, Option<Candidate>)> {
    let job = db
        .query_row(
            &format!("{SELECT} WHERE id=?1 AND owner=?2"),
            params![id, actor],
            job_row,
        )
        .optional()?
        .ok_or(Error::Denied)?;
    let payload: Option<String> =
        db.query_row("SELECT candidate FROM worker_jobs WHERE id=?1", [id], |r| {
            r.get(0)
        })?;
    let candidate = payload
        .map(|s| serde_json::from_str(&s).map_err(|_| Error::Invalid("stored candidate corrupt")))
        .transpose()?;
    Ok((job, candidate))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn setup() -> (Connection, Policy) {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE tasks(id TEXT PRIMARY KEY,owner TEXT,status TEXT,revision INTEGER); INSERT INTO tasks VALUES('t','bob','working',1)").unwrap();
        migrate(&db).unwrap();
        (
            db,
            Policy {
                executor_id: Some("sandbox".into()),
                repositories: BTreeSet::from(["repo".into()]),
                required_checks: BTreeSet::from(["unit".into(), "lint".into()]),
            },
        )
    }
    fn request() -> JobRequest {
        JobRequest {
            task_id: "t".into(),
            task_revision: 1,
            repository: "repo".into(),
            base_commit: "a".repeat(40),
            request_id: "req-1".into(),
        }
    }
    fn candidate() -> Candidate {
        Candidate {
            repository: "repo".into(),
            base_commit: "a".repeat(40),
            head_commit: "b".repeat(40),
            patch: "diff --git a/a b/a\n".into(),
            checks: ["unit", "lint"]
                .map(|name| Check {
                    name: name.into(),
                    commit: "b".repeat(40),
                    passed: true,
                })
                .to_vec(),
        }
    }
    fn lease(db: &mut Connection, p: &Policy) -> Lease {
        enqueue(db, "bob", request(), p, 10).unwrap();
        claim(db, "sandbox", p, 11, 30).unwrap().unwrap()
    }
    #[test]
    fn fail_closed_and_owner_revision_authorization() {
        let (mut db, mut p) = setup();
        assert!(matches!(
            enqueue(&mut db, "alice", request(), &p, 0),
            Err(Error::Denied)
        ));
        let mut r = request();
        r.task_revision = 0;
        assert!(matches!(
            enqueue(&mut db, "bob", r, &p, 0),
            Err(Error::Denied)
        ));
        p.executor_id = None;
        assert!(matches!(
            enqueue(&mut db, "bob", request(), &p, 0),
            Err(Error::ExecutorUnavailable)
        ));
        p.executor_id = Some("sandbox".into());
        let mut r = request();
        r.repository = "../../etc".into();
        assert!(matches!(
            enqueue(&mut db, "bob", r, &p, 0),
            Err(Error::Invalid(_))
        ));
        db.execute("UPDATE tasks SET status='awaiting_owner'", [])
            .unwrap();
        assert!(matches!(
            enqueue(&mut db, "bob", request(), &p, 0),
            Err(Error::Denied)
        ));
    }
    #[test]
    fn idempotency_and_one_active_job() {
        let (mut db, p) = setup();
        let first = enqueue(&mut db, "bob", request(), &p, 0).unwrap();
        assert_eq!(
            enqueue(&mut db, "bob", request(), &p, 1).unwrap().id,
            first.id
        );
        let mut r = request();
        r.base_commit = "c".repeat(40);
        assert!(matches!(
            enqueue(&mut db, "bob", r, &p, 1),
            Err(Error::Conflict)
        ));
        let mut r = request();
        r.request_id = "other".into();
        assert!(matches!(
            enqueue(&mut db, "bob", r, &p, 1),
            Err(Error::Conflict)
        ));
        cancel(&db, "bob", &first.id).unwrap();
        cancel(&db, "bob", &first.id).unwrap();
        assert_eq!(
            enqueue(&mut db, "bob", request(), &p, 2).unwrap().status,
            "cancelled"
        );
    }
    #[test]
    fn durable_restart_and_exclusive_claim() {
        let path = std::env::temp_dir().join(format!("worker-test-{}.sqlite", Uuid::new_v4()));
        let p = setup().1;
        {
            let mut db = Connection::open(&path).unwrap();
            db.execute_batch("CREATE TABLE tasks(id TEXT PRIMARY KEY,owner TEXT,status TEXT,revision INTEGER); INSERT INTO tasks VALUES('t','bob','working',1)").unwrap();
            migrate(&db).unwrap();
            enqueue(&mut db, "bob", request(), &p, 0).unwrap();
        }
        let mut db = Connection::open(&path).unwrap();
        migrate(&db).unwrap();
        let l = claim(&mut db, "sandbox", &p, 1, 30).unwrap().unwrap();
        let mut other = Connection::open(&path).unwrap();
        assert!(claim(&mut other, "sandbox", &p, 2, 30).unwrap().is_none());
        finish(&mut other, &l, &p, &candidate(), 3).unwrap();
        drop(other);
        drop(db);
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn expired_lease_is_fenced_and_attempts_bounded() {
        let (mut db, p) = setup();
        let one = lease(&mut db, &p);
        assert!(matches!(
            finish(&mut db, &one, &p, &candidate(), 41),
            Err(Error::LeaseLost)
        ));
        let two = claim(&mut db, "sandbox", &p, 41, 1).unwrap().unwrap();
        assert_ne!(one.token, two.token);
        assert!(matches!(
            finish(&mut db, &one, &p, &candidate(), 41),
            Err(Error::LeaseLost)
        ));
        let three = claim(&mut db, "sandbox", &p, 42, 1).unwrap().unwrap();
        assert_eq!(three.job.attempt, 3);
        assert!(claim(&mut db, "sandbox", &p, 43, 1).unwrap().is_none());
        assert_eq!(inspect(&db, "bob", &one.job.id).unwrap().0.status, "failed");
    }
    #[test]
    fn cancellation_and_changed_task_revoke_results() {
        let (mut db, p) = setup();
        let l = lease(&mut db, &p);
        assert!(matches!(
            cancel(&db, "alice", &l.job.id),
            Err(Error::Denied)
        ));
        db.execute(
            "UPDATE tasks SET revision=revision+1,status='cancelled'",
            [],
        )
        .unwrap();
        assert!(matches!(
            finish(&mut db, &l, &p, &candidate(), 12),
            Err(Error::Denied)
        ));
        assert!(claim(&mut db, "sandbox", &p, 12, 30).unwrap().is_none());
        assert_eq!(
            inspect(&db, "bob", &l.job.id).unwrap().0.status,
            "cancelled"
        );
    }
    #[test]
    fn runner_and_repository_revocation() {
        let (mut db, mut p) = setup();
        let l = lease(&mut db, &p);
        assert!(matches!(
            claim(&mut db, "rogue", &p, 12, 1),
            Err(Error::Denied)
        ));
        p.repositories.clear();
        assert!(matches!(
            finish(&mut db, &l, &p, &candidate(), 12),
            Err(Error::Denied)
        ));
    }
    #[test]
    fn evidence_must_bind_exact_commit_and_all_checks() {
        let (mut db, p) = setup();
        let l = lease(&mut db, &p);
        let mut c = candidate();
        c.checks[0].commit = "c".repeat(40);
        assert!(matches!(
            finish(&mut db, &l, &p, &c, 12),
            Err(Error::Invalid(_))
        ));
        c = candidate();
        c.checks.pop();
        assert!(matches!(
            finish(&mut db, &l, &p, &c, 12),
            Err(Error::Invalid(_))
        ));
        c = candidate();
        c.checks[0].passed = false;
        assert!(matches!(
            finish(&mut db, &l, &p, &c, 12),
            Err(Error::Invalid(_))
        ));
        c = candidate();
        c.checks.push(c.checks[0].clone());
        assert!(matches!(
            finish(&mut db, &l, &p, &c, 12),
            Err(Error::Invalid(_))
        ));
        c = candidate();
        c.base_commit = "c".repeat(40);
        assert!(matches!(
            finish(&mut db, &l, &p, &c, 12),
            Err(Error::Invalid(_))
        ));
        c = candidate();
        c.repository = "other".into();
        assert!(matches!(
            finish(&mut db, &l, &p, &c, 12),
            Err(Error::Invalid(_))
        ));
        let digest = finish(&mut db, &l, &p, &candidate(), 12).unwrap();
        assert_eq!(digest.len(), 64);
        let (job, artifact) = inspect(&db, "bob", &l.job.id).unwrap();
        assert_eq!(job.status, "review_ready");
        assert!(artifact.is_some());
        assert!(matches!(
            inspect(&db, "alice", &l.job.id),
            Err(Error::Denied)
        ));
        assert!(matches!(
            finish(&mut db, &l, &p, &candidate(), 13),
            Err(Error::LeaseLost)
        ));
        let status: String = db
            .query_row("SELECT status FROM tasks WHERE id='t'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(status, "working");
    }
    #[test]
    fn tampered_lease_cannot_change_bound_request() {
        let (mut db, p) = setup();
        let mut l = lease(&mut db, &p);
        l.job.request.base_commit = "c".repeat(40);
        let mut c = candidate();
        c.base_commit = "c".repeat(40);
        assert!(matches!(
            finish(&mut db, &l, &p, &c, 12),
            Err(Error::Invalid(_))
        ));
        l.token = "fake".into();
        assert!(matches!(
            finish(&mut db, &l, &p, &candidate(), 12),
            Err(Error::LeaseLost)
        ));
    }
    #[test]
    fn explicit_failure_and_cancellation_are_terminal() {
        let (mut db, p) = setup();
        let l = lease(&mut db, &p);
        fail(&mut db, &l, &p, 12).unwrap();
        assert_eq!(inspect(&db, "bob", &l.job.id).unwrap().0.status, "failed");
        assert!(claim(&mut db, "sandbox", &p, 100, 30).unwrap().is_none());
        let mut r = request();
        r.request_id = "retry-explicit".into();
        let j = enqueue(&mut db, "bob", r, &p, 101).unwrap();
        cancel(&db, "bob", &j.id).unwrap();
        assert!(claim(&mut db, "sandbox", &p, 102, 30).unwrap().is_none());
    }
    #[test]
    fn new_revision_supersedes_old_job_without_consumer_pass() {
        let (mut db, p) = setup();
        let old = lease(&mut db, &p);
        db.execute("UPDATE tasks SET revision=2", []).unwrap();
        let mut r = request();
        r.task_revision = 2;
        r.request_id = "new-round".into();
        let new = enqueue(&mut db, "bob", r, &p, 12).unwrap();
        assert_ne!(new.id, old.job.id);
        assert_eq!(
            inspect(&db, "bob", &old.job.id).unwrap().0.status,
            "cancelled"
        );
        assert!(matches!(
            finish(&mut db, &old, &p, &candidate(), 13),
            Err(Error::LeaseLost)
        ));
    }
    #[test]
    fn concurrent_consumers_receive_only_one_lease() {
        let path = std::env::temp_dir().join(format!("worker-race-{}.sqlite", Uuid::new_v4()));
        let p = setup().1;
        {
            let mut db = Connection::open(&path).unwrap();
            db.execute_batch("CREATE TABLE tasks(id TEXT PRIMARY KEY,owner TEXT,status TEXT,revision INTEGER); INSERT INTO tasks VALUES('t','bob','working',1)").unwrap();
            migrate(&db).unwrap();
            enqueue(&mut db, "bob", request(), &p, 0).unwrap();
        }
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let threads: Vec<_> = (0..2)
            .map(|_| {
                let path = path.clone();
                let policy = p.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    let mut db = Connection::open(path).unwrap();
                    db.busy_timeout(std::time::Duration::from_secs(5)).unwrap();
                    barrier.wait();
                    claim(&mut db, "sandbox", &policy, 1, 30).unwrap().is_some()
                })
            })
            .collect();
        let count = threads
            .into_iter()
            .filter_map(|thread| thread.join().unwrap().then_some(()))
            .count();
        assert_eq!(count, 1);
        std::fs::remove_file(path).unwrap();
    }
}
