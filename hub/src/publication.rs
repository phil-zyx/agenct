//! Explicit, journaled GitLab merge-request publication boundary. No built-in network transport.
//! Transport implementations own service credentials and MUST NOT expose them to the model.
use crate::worker::{Error, Result};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Target {
    pub project_id: u64,
    pub origin: String,
    pub target_branch: String,
}
#[derive(Debug, Clone)]
pub struct PublishPolicy {
    pub repositories: BTreeMap<String, Target>,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PublishRequest {
    pub job_id: String,
    /// Must match the immutable candidate shown to the authorizing owner.
    pub candidate_digest: String,
    pub source_branch: String,
    pub title: String,
    pub request_id: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Intent {
    pub id: String,
    pub owner: String,
    pub request: PublishRequest,
    pub target: Target,
    pub base_commit: String,
    pub head_commit: String,
    pub status: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MergeRequest {
    pub project_id: u64,
    pub iid: u64,
    pub draft: bool,
    pub source_branch: String,
    pub target_branch: String,
    pub head_commit: String,
    pub web_url: String,
}
#[derive(Debug, Clone)]
pub struct PipelineCheck {
    pub name: String,
    pub head_commit: String,
    pub passed: bool,
}
/// Every method must use authenticated TLS to the configured origin, no redirects to other hosts,
/// bounded timeouts/response sizes, and explicit numeric project IDs. A provider error never proves
/// a write did not happen. find must match this intent's unguessable marker, source AND target.
pub trait GitLabTransport {
    fn branch_head(&mut self, target: &Target, branch: &str)
        -> std::result::Result<String, String>;
    fn create_draft_merge_request(
        &mut self,
        intent: &Intent,
    ) -> std::result::Result<MergeRequest, String>;
    fn find_merge_request(
        &mut self,
        intent: &Intent,
    ) -> std::result::Result<Option<MergeRequest>, String>;
    fn pipeline_checks(
        &mut self,
        target: &Target,
        mr: &MergeRequest,
    ) -> std::result::Result<Vec<PipelineCheck>, String>;
}
pub fn migrate(db: &Connection) -> Result<()> {
    db.execute_batch("CREATE TABLE IF NOT EXISTS publication_intents(id TEXT PRIMARY KEY,owner TEXT NOT NULL,request_id TEXT NOT NULL,request TEXT NOT NULL,target TEXT NOT NULL,base_commit TEXT NOT NULL,head_commit TEXT NOT NULL,status TEXT NOT NULL CHECK(status IN ('authorized','attempting','uncertain','published')),remote TEXT,UNIQUE(owner,request_id))")?;
    Ok(())
}
fn encoded<T: Serialize>(value: &T) -> Result<String> {
    serde_json::to_string(value).map_err(|_| Error::Invalid("publication serialization"))
}
fn parse<T: serde::de::DeserializeOwned>(s: &str) -> Result<T> {
    serde_json::from_str(s).map_err(|_| Error::Invalid("stored publication corrupt"))
}
fn read(db: &Connection, id: &str) -> Result<Intent> {
    let row = db
        .query_row(
            "SELECT owner,request,target,base_commit,head_commit,status FROM publication_intents WHERE id=?1",
            [id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                r.get::<_, String>(5)?,
                ))
            },
        )
        .optional()?
        .ok_or(Error::Denied)?;
    Ok(Intent {
        id: id.into(),
        owner: row.0,
        request: parse(&row.1)?,
        target: parse(&row.2)?,
        base_commit: row.3,
        head_commit: row.4,
        status: row.5,
    })
}
fn candidate(
    db: &Connection,
    owner: &str,
    request: &PublishRequest,
) -> Result<(String, String, String)> {
    let row=db.query_row("SELECT w.repository,w.candidate FROM worker_jobs w JOIN tasks t ON t.id=w.task_id WHERE w.id=?1 AND w.owner=?2 AND w.status='review_ready' AND w.candidate_sha256=?3 AND t.owner=w.owner AND t.status='working' AND t.revision=w.task_revision",params![request.job_id,owner,request.candidate_digest],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?))).optional()?.ok_or(Error::Denied)?;
    let artifact: crate::worker::Candidate = parse(&row.1)?;
    Ok((row.0, artifact.base_commit, artifact.head_commit))
}
fn valid_branch(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && !s.starts_with('-')
        && !s.starts_with('/')
        && !s.ends_with('/')
        && !s.contains("..")
        && !s.contains("//")
        && !s.ends_with(".lock")
        && !s.ends_with('.')
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-/.".contains(&b))
}
fn validate_target(t: &Target) -> Result<()> {
    let url = reqwest::Url::parse(&t.origin).map_err(|_| Error::Invalid("GitLab origin"))?;
    if t.project_id == 0
        || url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
        || !valid_branch(&t.target_branch)
    {
        return Err(Error::Invalid("GitLab target"));
    }
    Ok(())
}
/// Call only after authenticated owner explicitly approves this exact MR target and artifact.
/// This grant authorizes draft MR creation only: no push, merge, approval, settings or credentials.
pub fn authorize(
    db: &mut Connection,
    owner: &str,
    request: PublishRequest,
    policy: &PublishPolicy,
) -> Result<Intent> {
    if !valid_branch(&request.source_branch)
        || request.title.trim().is_empty()
        || request.title.len() > 200
        || request.title.contains(['\n', '\r'])
        || request.request_id.is_empty()
        || request.request_id.len() > 128
    {
        return Err(Error::Invalid("publication request"));
    }
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    if let Some(id) = tx
        .query_row(
            "SELECT id FROM publication_intents WHERE owner=?1 AND request_id=?2",
            params![owner, request.request_id],
            |r| r.get::<_, String>(0),
        )
        .optional()?
    {
        let existing = read(&tx, &id)?;
        return if existing.request == request {
            Ok(existing)
        } else {
            Err(Error::Conflict)
        };
    }
    let (repo, base, head) = candidate(&tx, owner, &request)?;
    let target = policy.repositories.get(&repo).ok_or(Error::Denied)?.clone();
    validate_target(&target)?;
    if target.target_branch == request.source_branch {
        return Err(Error::Invalid("source equals target"));
    }
    let intent = Intent {
        id: Uuid::new_v4().to_string(),
        owner: owner.into(),
        request,
        target,
        base_commit: base,
        head_commit: head,
        status: "authorized".into(),
    };
    tx.execute("INSERT INTO publication_intents(id,owner,request_id,request,target,base_commit,head_commit,status) VALUES(?1,?2,?3,?4,?5,?6,?7,'authorized')",params![intent.id,owner,intent.request.request_id,encoded(&intent.request)?,encoded(&intent.target)?,intent.base_commit,intent.head_commit])?;
    tx.commit()?;
    Ok(intent)
}
fn validate_remote(intent: &Intent, mr: &MergeRequest) -> Result<()> {
    let remote = reqwest::Url::parse(&mr.web_url).map_err(|_| Error::Invalid("MR URL"))?;
    let origin =
        reqwest::Url::parse(&intent.target.origin).map_err(|_| Error::Invalid("GitLab origin"))?;
    if mr.project_id != intent.target.project_id
        || mr.iid == 0
        || !mr.draft
        || mr.source_branch != intent.request.source_branch
        || mr.target_branch != intent.target.target_branch
        || mr.head_commit != intent.head_commit
        || remote.origin() != origin.origin()
        || !remote.username().is_empty()
        || remote.password().is_some()
        || !remote
            .path()
            .ends_with(&format!("/-/merge_requests/{}", mr.iid))
    {
        return Err(Error::Invalid("MR binding mismatch"));
    }
    Ok(())
}
fn current_authorization(db: &Connection, intent: &Intent, policy: &PublishPolicy) -> Result<()> {
    let (repo, base, head) = candidate(db, &intent.owner, &intent.request)?;
    if head != intent.head_commit
        || base != intent.base_commit
        || policy.repositories.get(&repo) != Some(&intent.target)
    {
        return Err(Error::Denied);
    }
    Ok(())
}
/// One send attempt only. Any uncertain/crashed attempt is reconciled, never blindly repeated.
/// Before this call the exact candidate commit must already be on source_branch by a separately
/// authorized process. This library does not create/push that branch or transmit source patches.
pub fn publish(
    db: &mut Connection,
    id: &str,
    owner: &str,
    policy: &PublishPolicy,
    transport: &mut dyn GitLabTransport,
) -> Result<MergeRequest> {
    let intent = read(db, id)?;
    if intent.owner != owner {
        return Err(Error::Denied);
    }
    current_authorization(db, &intent, policy)?;
    if intent.status == "published" {
        let raw: String = db.query_row(
            "SELECT remote FROM publication_intents WHERE id=?1",
            [id],
            |r| r.get(0),
        )?;
        return parse(&raw);
    }
    if intent.status != "authorized" {
        let mr = transport
            .find_merge_request(&intent)
            .map_err(|_| Error::ExecutorUnavailable)?
            .ok_or(Error::Conflict)?;
        validate_remote(&intent, &mr)?;
        db.execute(
            "UPDATE publication_intents SET status='published',remote=?2 WHERE id=?1",
            params![id, encoded(&mr)?],
        )?;
        return Ok(mr);
    }
    if transport
        .branch_head(&intent.target, &intent.target.target_branch)
        .map_err(|_| Error::ExecutorUnavailable)?
        != intent.base_commit
    {
        return Err(Error::Conflict);
    }
    if transport
        .branch_head(&intent.target, &intent.request.source_branch)
        .map_err(|_| Error::ExecutorUnavailable)?
        != intent.head_commit
    {
        return Err(Error::Conflict);
    }
    // Atomically reserve the only attempt, rechecking authorization after provider reads.
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    current_authorization(&tx, &intent, policy)?;
    let count = tx.execute(
        "UPDATE publication_intents SET status='attempting' WHERE id=?1 AND status='authorized'",
        [id],
    )?;
    if count != 1 {
        return Err(Error::Conflict);
    }
    tx.commit()?;
    match transport.create_draft_merge_request(&intent) {
        Ok(mr) if validate_remote(&intent, &mr).is_ok() => {
            db.execute(
                "UPDATE publication_intents SET status='published',remote=?2 WHERE id=?1",
                params![id, encoded(&mr)?],
            )?;
            Ok(mr)
        }
        _ => {
            db.execute(
                "UPDATE publication_intents SET status='uncertain' WHERE id=?1",
                [id],
            )?;
            Err(Error::ExecutorUnavailable)
        }
    }
}
/// Read-only verification of all required CI checks on the *exact* approved candidate SHA.
/// Success never merges, approves a PR/MR, or completes the Hub task.
pub fn verify(
    transport: &mut dyn GitLabTransport,
    intent: &Intent,
    mr: &MergeRequest,
    required: &std::collections::BTreeSet<String>,
) -> Result<()> {
    validate_remote(intent, mr)?;
    if required.is_empty()
        || transport
            .branch_head(&intent.target, &intent.request.source_branch)
            .map_err(|_| Error::ExecutorUnavailable)?
            != intent.head_commit
    {
        return Err(Error::Conflict);
    }
    let checks = transport
        .pipeline_checks(&intent.target, mr)
        .map_err(|_| Error::ExecutorUnavailable)?;
    let mut passed = std::collections::BTreeSet::new();
    for check in checks {
        if !check.passed || check.head_commit != intent.head_commit || !passed.insert(check.name) {
            return Err(Error::Invalid("CI evidence mismatch"));
        }
    }
    if !required.is_subset(&passed) {
        return Err(Error::Invalid("required CI missing"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::worker::{self, Candidate, Check, JobRequest, Policy};
    use std::collections::BTreeSet;
    fn setup() -> (Connection, PublishPolicy, PublishRequest) {
        let mut db = Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE tasks(id TEXT PRIMARY KEY,owner TEXT,status TEXT,revision INTEGER);INSERT INTO tasks VALUES('t','bob','working',1)").unwrap();
        worker::migrate(&db).unwrap();
        migrate(&db).unwrap();
        let wp = Policy {
            executor_id: Some("sandbox".into()),
            repositories: BTreeSet::from(["repo".into()]),
            required_checks: BTreeSet::from(["unit".into()]),
        };
        let job = worker::enqueue(
            &mut db,
            "bob",
            JobRequest {
                task_id: "t".into(),
                task_revision: 1,
                repository: "repo".into(),
                base_commit: "a".repeat(40),
                request_id: "run".into(),
            },
            &wp,
            0,
        )
        .unwrap();
        let lease = worker::claim(&mut db, "sandbox", &wp, 1, 30)
            .unwrap()
            .unwrap();
        let digest = worker::finish(
            &mut db,
            &lease,
            &wp,
            &Candidate {
                repository: "repo".into(),
                base_commit: "a".repeat(40),
                head_commit: "b".repeat(40),
                patch: "diff".into(),
                checks: vec![Check {
                    name: "unit".into(),
                    commit: "b".repeat(40),
                    passed: true,
                }],
            },
            2,
        )
        .unwrap();
        let policy = PublishPolicy {
            repositories: BTreeMap::from([(
                "repo".into(),
                Target {
                    project_id: 12,
                    origin: "https://gitlab.example/".into(),
                    target_branch: "main".into(),
                },
            )]),
        };
        (
            db,
            policy,
            PublishRequest {
                job_id: job.id,
                candidate_digest: digest,
                source_branch: "agenct/candidate".into(),
                title: "Implement requested fix".into(),
                request_id: "publish-1".into(),
            },
        )
    }
    #[derive(Default)]
    struct Fake {
        creates: usize,
        lose_response: bool,
        remote: Option<MergeRequest>,
        wrong_head: bool,
        bad_checks: bool,
    }
    impl GitLabTransport for Fake {
        fn branch_head(&mut self, _: &Target, branch: &str) -> std::result::Result<String, String> {
            Ok(if self.wrong_head {
                "c"
            } else if branch == "main" {
                "a"
            } else {
                "b"
            }
            .repeat(40))
        }
        fn create_draft_merge_request(
            &mut self,
            intent: &Intent,
        ) -> std::result::Result<MergeRequest, String> {
            self.creates += 1;
            let mr = MergeRequest {
                project_id: intent.target.project_id,
                iid: 42,
                draft: true,
                source_branch: intent.request.source_branch.clone(),
                target_branch: intent.target.target_branch.clone(),
                head_commit: intent.head_commit.clone(),
                web_url: "https://gitlab.example/group/repo/-/merge_requests/42".into(),
            };
            self.remote = Some(mr.clone());
            if self.lose_response {
                Err("timeout after accepted".into())
            } else {
                Ok(mr)
            }
        }
        fn find_merge_request(
            &mut self,
            _: &Intent,
        ) -> std::result::Result<Option<MergeRequest>, String> {
            Ok(self.remote.clone())
        }
        fn pipeline_checks(
            &mut self,
            _: &Target,
            _: &MergeRequest,
        ) -> std::result::Result<Vec<PipelineCheck>, String> {
            Ok(vec![PipelineCheck {
                name: "unit".into(),
                head_commit: if self.bad_checks { "c" } else { "b" }.repeat(40),
                passed: true,
            }])
        }
    }
    #[test]
    fn explicit_owner_and_exact_artifact_required() {
        let (mut db, policy, request) = setup();
        assert!(matches!(
            authorize(&mut db, "alice", request.clone(), &policy),
            Err(Error::Denied)
        ));
        let mut altered = request.clone();
        altered.candidate_digest = "wrong".into();
        assert!(matches!(
            authorize(&mut db, "bob", altered, &policy),
            Err(Error::Denied)
        ));
        let grant = authorize(&mut db, "bob", request.clone(), &policy).unwrap();
        assert_eq!(
            authorize(&mut db, "bob", request.clone(), &policy)
                .unwrap()
                .id,
            grant.id
        );
        let mut altered = request;
        altered.title = "other".into();
        assert!(matches!(
            authorize(&mut db, "bob", altered, &policy),
            Err(Error::Conflict)
        ));
    }
    #[test]
    fn publishes_once_and_does_not_merge_or_complete() {
        let (mut db, policy, request) = setup();
        let grant = authorize(&mut db, "bob", request, &policy).unwrap();
        let mut fake = Fake::default();
        let mr = publish(&mut db, &grant.id, "bob", &policy, &mut fake).unwrap();
        assert_eq!(
            publish(&mut db, &grant.id, "bob", &policy, &mut fake).unwrap(),
            mr
        );
        assert_eq!(fake.creates, 1);
        verify(&mut fake, &grant, &mr, &BTreeSet::from(["unit".into()])).unwrap();
        assert_eq!(
            db.query_row("SELECT status FROM tasks", [], |r| r.get::<_, String>(0))
                .unwrap(),
            "working"
        );
    }
    #[test]
    fn uncertain_send_reconciles_without_duplicate_write() {
        let (mut db, policy, request) = setup();
        let grant = authorize(&mut db, "bob", request, &policy).unwrap();
        let mut fake = Fake {
            lose_response: true,
            ..Default::default()
        };
        assert!(publish(&mut db, &grant.id, "bob", &policy, &mut fake).is_err());
        let saved = fake.remote.take();
        assert!(matches!(
            publish(&mut db, &grant.id, "bob", &policy, &mut fake),
            Err(Error::Conflict)
        ));
        assert_eq!(fake.creates, 1);
        fake.remote = saved;
        assert!(publish(&mut db, &grant.id, "bob", &policy, &mut fake).is_ok());
        assert_eq!(fake.creates, 1);
    }
    #[test]
    fn stale_branch_and_revoked_task_block_remote_writes() {
        let (mut db, policy, request) = setup();
        let grant = authorize(&mut db, "bob", request, &policy).unwrap();
        let mut fake = Fake {
            wrong_head: true,
            ..Default::default()
        };
        assert!(matches!(
            publish(&mut db, &grant.id, "bob", &policy, &mut fake),
            Err(Error::Conflict)
        ));
        assert_eq!(fake.creates, 0);
        fake.wrong_head = false;
        db.execute("UPDATE tasks SET status='cancelled'", [])
            .unwrap();
        assert!(matches!(
            publish(&mut db, &grant.id, "bob", &policy, &mut fake),
            Err(Error::Denied)
        ));
        assert_eq!(fake.creates, 0);
    }
    #[test]
    fn wrong_sha_ci_and_wrong_origin_are_rejected() {
        let (mut db, policy, request) = setup();
        let grant = authorize(&mut db, "bob", request, &policy).unwrap();
        let mut fake = Fake::default();
        let mut mr = publish(&mut db, &grant.id, "bob", &policy, &mut fake).unwrap();
        fake.bad_checks = true;
        assert!(verify(&mut fake, &grant, &mr, &BTreeSet::from(["unit".into()])).is_err());
        fake.bad_checks = false;
        assert!(verify(&mut fake, &grant, &mr, &BTreeSet::from(["missing".into()])).is_err());
        mr.web_url = "https://evil.example/group/repo/-/merge_requests/42".into();
        assert!(verify(&mut fake, &grant, &mr, &BTreeSet::from(["unit".into()])).is_err());
    }
    #[test]
    fn crash_after_reserving_send_never_resends() {
        let (mut db, policy, request) = setup();
        let grant = authorize(&mut db, "bob", request, &policy).unwrap();
        db.execute("UPDATE publication_intents SET status='attempting'", [])
            .unwrap();
        let mut fake = Fake::default();
        assert!(matches!(
            publish(&mut db, &grant.id, "bob", &policy, &mut fake),
            Err(Error::Conflict)
        ));
        assert_eq!(fake.creates, 0);
    }
}
