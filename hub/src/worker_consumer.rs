//! Output-only worker consumer contracts. Built-in Docker execution is disabled pending trusted verification. Never run on the Hub request thread.
use crate::worker::{self, Candidate, Error, Job, Lease, Policy, Result};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckCommand {
    pub name: String,
    pub argv: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunInput {
    pub repository: String,
    pub base_commit: String,
    /// A trusted provider's git bundle, not a repository-supplied .git directory.
    pub bundle_base64: String,
    pub patch: String,
    /// Trusted operator commands. Never accept these from the model or chat request.
    pub checks: Vec<CheckCommand>,
}
pub trait InputProvider {
    /// Fetch only the allowlisted repository at the job's exact base, and obtain a candidate patch.
    /// No credentials or private conversation history may be included in RunInput.
    fn prepare(&mut self, job: &Job) -> Result<RunInput>;
}
// External implementations cannot assert that repository-controlled test evidence is trusted.
// Reopen this boundary only with independent verifier isolation and adversarial regression tests.
mod sealed {
    pub trait TrustedEvidence {}
}
pub trait Executor: sealed::TrustedEvidence {
    /// Must terminate work when still_authorized returns false, and return only observed evidence.
    fn run(
        &mut self,
        input: &RunInput,
        still_authorized: &mut dyn FnMut() -> bool,
    ) -> Result<Candidate>;
}
#[derive(Debug)]
pub enum Outcome {
    Idle,
    ReviewReady { job_id: String, digest: String },
    Failed { job_id: String },
}
fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}
fn live(db: &Connection, lease: &Lease, policy: &Policy) -> bool {
    worker::lease_is_live(db, lease, policy, now()).unwrap_or(false)
}
/// One bounded iteration; no external publication. The caller owns scheduling and a dedicated DB connection.
pub fn consume_one(
    db: &mut Connection,
    policy: &Policy,
    provider: &mut dyn InputProvider,
    executor: &mut dyn Executor,
) -> Result<Outcome> {
    let runner = policy
        .executor_id
        .as_deref()
        .ok_or(Error::ExecutorUnavailable)?;
    let Some(lease) = worker::claim(db, runner, policy, now(), 300)? else {
        return Ok(Outcome::Idle);
    };
    let result = (|| {
        let input = provider.prepare(&lease.job)?;
        if input.repository != lease.job.request.repository
            || input.base_commit != lease.job.request.base_commit
        {
            return Err(Error::Invalid("source binding mismatch"));
        }
        validate_input(&input)?;
        if !live(db, &lease, policy) {
            return Err(Error::LeaseLost);
        }
        let candidate = executor.run(&input, &mut || live(db, &lease, policy))?;
        worker::finish(db, &lease, policy, &candidate, now())
    })();
    match result {
        Ok(digest) => Ok(Outcome::ReviewReady {
            job_id: lease.job.id,
            digest,
        }),
        Err(error) => match worker::fail(db, &lease, policy, now()) {
            Ok(()) => Ok(Outcome::Failed {
                job_id: lease.job.id,
            }),
            Err(Error::Denied | Error::LeaseLost) => Err(error),
            Err(storage_error) => Err(storage_error),
        },
    }
}
fn validate_input(input: &RunInput) -> Result<()> {
    if input.bundle_base64.is_empty()
        || input.bundle_base64.len() > 12 * 1024 * 1024
        || input.patch.is_empty()
        || input.patch.len() > 1024 * 1024
        || input.checks.is_empty()
        || input.checks.len() > 16
    {
        return Err(Error::Invalid("sandbox input size"));
    }
    let mut names = std::collections::BTreeSet::new();
    for c in &input.checks {
        if c.name.is_empty()
            || c.name.len() > 128
            || !names.insert(&c.name)
            || c.argv.is_empty()
            || c.argv.len() > 32
            || c.argv
                .iter()
                .any(|s| s.is_empty() || s.len() > 1024 || s.contains('\0'))
        {
            return Err(Error::Invalid("invalid trusted check"));
        }
    }
    Ok(())
}
/// Disabled executor prototype. Configuration/argument validation is retained for review only.
/// No image configuration can enable execution before independent verifier isolation exists.
/// Its digest is mandatory. No docker pull/build occurs here, and no host mounts or secrets are passed.
#[derive(Debug, Clone)]
pub struct DockerExecutor {
    pub docker: PathBuf,
    pub image: String,
    pub timeout: Duration,
    /// Trusted check name -> exact argv allowlist, never derived from a model response.
    pub checks: BTreeMap<String, Vec<String>>,
}
impl DockerExecutor {
    pub fn arguments(&self, name: &str) -> Result<Vec<String>> {
        let digest = self
            .image
            .rsplit_once("@sha256:")
            .map(|(_, d)| d)
            .ok_or(Error::Invalid("pin executor image digest"))?;
        if !self.docker.is_absolute()
            || digest.len() != 64
            || !digest.bytes().all(|b| b.is_ascii_hexdigit())
            || self.image.chars().any(char::is_whitespace)
            || self.image.starts_with('-')
            || self.timeout.is_zero()
            || self.timeout > Duration::from_secs(240)
        {
            return Err(Error::Invalid("unsafe executor configuration"));
        }
        Ok([
            "run",
            "--label=agenct.worker=true",
            "--pull=never",
            "--name",
            name,
            "--interactive",
            "--network=none",
            "--read-only",
            "--cap-drop=ALL",
            "--security-opt=no-new-privileges",
            "--pids-limit=64",
            "--memory=512m",
            "--memory-swap=512m",
            "--cpus=1",
            "--user=65534:65534",
            "--tmpfs=/work:rw,nosuid,nodev,size=134217728,mode=1777",
            "--tmpfs=/tmp:rw,nosuid,nodev,noexec,size=33554432,mode=1777",
            "--workdir=/work",
            "--env=HOME=/work",
            "--env=PATH=/usr/local/bin:/usr/bin:/bin",
            "--entrypoint=/usr/bin/python3",
            &self.image,
            "-I",
            "/opt/agenct/runner.py",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect())
    }
}
impl sealed::TrustedEvidence for DockerExecutor {}
impl Executor for DockerExecutor {
    fn run(
        &mut self,
        input: &RunInput,
        still_authorized: &mut dyn FnMut() -> bool,
    ) -> Result<Candidate> {
        validate_input(input)?;
        if input
            .checks
            .iter()
            .any(|check| self.checks.get(&check.name) != Some(&check.argv))
        {
            return Err(Error::Denied);
        }
        if !still_authorized() {
            return Err(Error::Denied);
        }
        // Intentionally no process launch. A shared-UID runner cannot attest which tree tests
        // used: tests can rewrite .git/index, assume-unchanged flags or replacement refs.
        // Container limits do not isolate evidence from the tested code. Fail closed until
        // an independently trusted verifier owns immutable input and observes isolated checks.
        Err(Error::ExecutorUnavailable)
    }
}

/// Static, server-owned provider for preprepared bundles/patches. No URL, credential or shell execution.
/// Used for operator-run integration and offline tests, not autonomous source/model retrieval.
pub struct PreparedInputs {
    pub by_job: BTreeMap<String, RunInput>,
}
impl InputProvider for PreparedInputs {
    fn prepare(&mut self, job: &Job) -> Result<RunInput> {
        self.by_job
            .remove(&job.id)
            .ok_or(Error::Invalid("no prepared input for job"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn docker_plan_has_no_network_mounts_or_ambient_credentials() {
        let d = DockerExecutor {
            docker: "/usr/bin/docker".into(),
            image: format!("internal/worker@sha256:{}", "a".repeat(64)),
            timeout: Duration::from_secs(120),
            checks: BTreeMap::new(),
        };
        let args = d.arguments("agenct-test").unwrap();
        for required in [
            "--network=none",
            "--read-only",
            "--cap-drop=ALL",
            "--security-opt=no-new-privileges",
            "--pull=never",
            "--user=65534:65534",
        ] {
            assert!(args.contains(&required.into()));
        }
        assert!(!args.iter().any(|a| a == "--privileged"
            || a.starts_with("--volume")
            || a.starts_with("--mount")
            || a == "-v"));
        let mut d = d.clone();
        d.image = "image:latest".into();
        assert!(d.arguments("test").is_err());
    }
    #[test]
    fn checks_must_be_bounded_and_unique() {
        let mut input = RunInput {
            repository: "r".into(),
            base_commit: "a".repeat(40),
            bundle_base64: "YWJj".into(),
            patch: "patch".into(),
            checks: vec![CheckCommand {
                name: "unit".into(),
                argv: vec!["cargo".into(), "test".into()],
            }],
        };
        assert!(validate_input(&input).is_ok());
        input.checks.push(input.checks[0].clone());
        assert!(validate_input(&input).is_err());
    }
    struct FakeExecutor {
        called: bool,
        fail: bool,
    }
    impl sealed::TrustedEvidence for FakeExecutor {}
    impl Executor for FakeExecutor {
        fn run(&mut self, input: &RunInput, valid: &mut dyn FnMut() -> bool) -> Result<Candidate> {
            self.called = true;
            assert!(valid());
            if self.fail {
                return Err(Error::ExecutorUnavailable);
            }
            Ok(Candidate {
                repository: input.repository.clone(),
                base_commit: input.base_commit.clone(),
                head_commit: "b".repeat(40),
                patch: "diff".into(),
                checks: vec![worker::Check {
                    name: "unit".into(),
                    commit: "b".repeat(40),
                    passed: true,
                }],
            })
        }
    }
    fn fixture() -> (Connection, Policy, PreparedInputs, String) {
        let mut db = Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE tasks(id TEXT PRIMARY KEY,owner TEXT,status TEXT,revision INTEGER);INSERT INTO tasks VALUES('t','bob','working',1)").unwrap();
        worker::migrate(&db).unwrap();
        let policy = Policy {
            executor_id: Some("sandbox".into()),
            repositories: std::collections::BTreeSet::from(["repo".into()]),
            required_checks: std::collections::BTreeSet::from(["unit".into()]),
        };
        let job = worker::enqueue(
            &mut db,
            "bob",
            worker::JobRequest {
                task_id: "t".into(),
                task_revision: 1,
                repository: "repo".into(),
                base_commit: "a".repeat(40),
                request_id: "run".into(),
            },
            &policy,
            now(),
        )
        .unwrap();
        let input = RunInput {
            repository: "repo".into(),
            base_commit: "a".repeat(40),
            bundle_base64: "YWJj".into(),
            patch: "diff".into(),
            checks: vec![CheckCommand {
                name: "unit".into(),
                argv: vec!["true".into()],
            }],
        };
        (
            db,
            policy,
            PreparedInputs {
                by_job: BTreeMap::from([(job.id.clone(), input)]),
            },
            job.id,
        )
    }
    #[test]
    fn consumer_finishes_only_review_candidate_and_then_idles() {
        let (mut db, p, mut source, id) = fixture();
        let mut exec = FakeExecutor {
            called: false,
            fail: false,
        };
        assert!(matches!(
            consume_one(&mut db, &p, &mut source, &mut exec).unwrap(),
            Outcome::ReviewReady { .. }
        ));
        assert!(exec.called);
        assert_eq!(
            worker::inspect(&db, "bob", &id).unwrap().0.status,
            "review_ready"
        );
        assert!(matches!(
            consume_one(&mut db, &p, &mut source, &mut exec).unwrap(),
            Outcome::Idle
        ));
    }
    #[test]
    fn consumer_rejects_wrong_source_before_execution() {
        let (mut db, p, mut source, id) = fixture();
        source.by_job.get_mut(&id).unwrap().base_commit = "c".repeat(40);
        let mut exec = FakeExecutor {
            called: false,
            fail: false,
        };
        assert!(matches!(
            consume_one(&mut db, &p, &mut source, &mut exec).unwrap(),
            Outcome::Failed { .. }
        ));
        assert!(!exec.called);
        assert_eq!(worker::inspect(&db, "bob", &id).unwrap().0.status, "failed");
    }
    #[test]
    fn consumer_records_executor_failure_without_candidate() {
        let (mut db, p, mut source, id) = fixture();
        let mut exec = FakeExecutor {
            called: false,
            fail: true,
        };
        assert!(matches!(
            consume_one(&mut db, &p, &mut source, &mut exec).unwrap(),
            Outcome::Failed { .. }
        ));
        assert!(exec.called);
        assert!(worker::inspect(&db, "bob", &id).unwrap().1.is_none());
    }
    #[test]
    fn unconfigured_check_never_starts_docker() {
        let (_, _, source, _) = fixture();
        let input = source.by_job.values().next().unwrap();
        let mut executor = DockerExecutor {
            docker: "/not/installed/docker".into(),
            image: format!("trusted@sha256:{}", "a".repeat(64)),
            timeout: Duration::from_secs(120),
            checks: BTreeMap::new(),
        };
        assert!(matches!(
            executor.run(input, &mut || true),
            Err(Error::Denied)
        ));
    }
    #[test]
    fn configured_docker_is_disabled_until_independent_verification_exists() {
        let (_, _, source, _) = fixture();
        let input = source.by_job.values().next().unwrap();
        let mut executor = DockerExecutor {
            docker: "/usr/bin/docker".into(),
            image: format!("trusted@sha256:{}", "a".repeat(64)),
            timeout: Duration::from_secs(120),
            checks: BTreeMap::from([("unit".into(), vec!["true".into()])]),
        };
        assert!(matches!(
            executor.run(input, &mut || true),
            Err(Error::ExecutorUnavailable)
        ));
    }
    #[test]
    fn built_in_consumer_cannot_mark_untrusted_job_review_ready() {
        let (mut db, policy, mut source, id) = fixture();
        let mut executor = DockerExecutor {
            docker: "/usr/bin/docker".into(),
            image: format!("trusted@sha256:{}", "a".repeat(64)),
            timeout: Duration::from_secs(120),
            checks: BTreeMap::from([("unit".into(), vec!["true".into()])]),
        };
        assert!(matches!(
            consume_one(&mut db, &policy, &mut source, &mut executor).unwrap(),
            Outcome::Failed { .. }
        ));
        let (job, candidate) = worker::inspect(&db, "bob", &id).unwrap();
        assert_eq!(job.status, "failed");
        assert!(candidate.is_none());
    }
}
