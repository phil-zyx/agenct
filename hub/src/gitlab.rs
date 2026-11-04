//! Bounded GitLab API v4 adapter. No branch push, merge, approval or general HTTP entry point.
use crate::publication::{GitLabTransport, Intent, MergeRequest, PipelineCheck, Target};
use reqwest::Url;
use serde_json::{json, Value};
use std::{
    collections::BTreeSet,
    io::{Read, Write},
    process::{Command, Stdio},
};

const LIMIT: usize = 1024 * 1024;
type Result<T> = std::result::Result<T, String>;

// Private boundary: callers cannot inject arbitrary URLs or replace TLS enforcement.
trait Http {
    fn request(&mut self, method: &str, url: &Url, body: Option<&Value>) -> Result<Value>;
}
struct Curl {
    token: String,
}
impl Http for Curl {
    fn request(&mut self, method: &str, url: &Url, body: Option<&Value>) -> Result<Value> {
        self.request_with_command(Command::new("/usr/bin/curl"), method, url, body)
    }
}
impl Curl {
    fn request_with_command(
        &mut self,
        mut command: Command,
        method: &str,
        url: &Url,
        body: Option<&Value>,
    ) -> Result<Value> {
        // Disable curlrc and proxy environment; never put credentials in process arguments.
        let mut child = command
            .env_clear()
            .args([
                "--disable",
                "--proto",
                "=https",
                "--tlsv1.2",
                "--proxy",
                "",
                "--noproxy",
                "*",
                "--connect-timeout",
                "5",
                "--max-time",
                "20",
                "--max-filesize",
                "1048576",
                "--silent",
                "--request",
                method,
                "--url",
                url.as_str(),
                "--config",
                "-",
                "--write-out",
                "\n%{http_code}",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|_| "GitLab TLS client unavailable")?;
        let mut config = format!(
            "header = \"PRIVATE-TOKEN: {}\"\nheader = \"Content-Type: application/json\"\n",
            self.token
        );
        if let Some(body) = body {
            config.push_str(&format!(
                "data-binary = {}\n",
                serde_json::to_string(&body.to_string()).map_err(|_| "request encoding")?
            ));
        }
        let write = child.stdin.take().unwrap().write_all(config.as_bytes());
        if write.is_err() {
            let _ = child.kill();
            let _ = child.wait();
            return Err("GitLab transport failed".into());
        }
        let mut output = Vec::new();
        let read = child
            .stdout
            .take()
            .unwrap()
            .take((LIMIT + 1) as u64)
            .read_to_end(&mut output);
        if read.is_err() || output.len() > LIMIT {
            let _ = child.kill();
            let _ = child.wait();
            return Err("GitLab response limit".into());
        }
        if !child
            .wait()
            .map_err(|_| "GitLab transport failed")?
            .success()
        {
            return Err("GitLab transport failed; outcome may be uncertain".into());
        }
        let split = output
            .iter()
            .rposition(|b| *b == b'\n')
            .ok_or("GitLab malformed response")?;
        let status = std::str::from_utf8(&output[split + 1..])
            .ok()
            .and_then(|s| s.parse::<u16>().ok())
            .ok_or("GitLab status missing")?;
        if !(200..300).contains(&status) {
            return Err(format!("GitLab HTTP {status}; outcome may be uncertain"));
        }
        serde_json::from_slice(&output[..split]).map_err(|_| "GitLab invalid JSON".into())
    }
}

/// Construct only in the trusted publisher process. Never pass token to a model or worker.
/// Origin and project IDs are operator policy, not request parameters. Requires system curl.
pub struct GitLabClient {
    origin: Url,
    projects: BTreeSet<u64>,
    http: Box<dyn Http>,
}
impl GitLabClient {
    pub fn new(origin: &str, projects: BTreeSet<u64>, token: String) -> Result<Self> {
        if token.is_empty()
            || token.len() > 512
            || !token
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
        {
            return Err("invalid GitLab token format".into());
        }
        Self::build(origin, projects, Box::new(Curl { token }))
    }
    fn build(origin: &str, projects: BTreeSet<u64>, http: Box<dyn Http>) -> Result<Self> {
        let origin = Url::parse(origin).map_err(|_| "invalid GitLab origin")?;
        if origin.scheme() != "https"
            || origin.host_str().is_none()
            || origin.path() != "/"
            || !origin.username().is_empty()
            || origin.password().is_some()
            || origin.query().is_some()
            || origin.fragment().is_some()
            || projects.is_empty()
            || projects.contains(&0)
        {
            return Err("invalid GitLab allowlist".into());
        }
        Ok(Self {
            origin,
            projects,
            http,
        })
    }
    fn url(&self, target: &Target, segments: &[&str]) -> Result<Url> {
        let origin = Url::parse(&target.origin).map_err(|_| "target denied")?;
        if origin != self.origin || !self.projects.contains(&target.project_id) {
            return Err("target denied".into());
        }
        let mut url = self.origin.clone();
        url.path_segments_mut()
            .map_err(|_| "invalid origin")?
            .extend(["api", "v4", "projects", &target.project_id.to_string()])
            .extend(segments.iter().copied());
        Ok(url)
    }
    fn get(&mut self, url: Url) -> Result<Value> {
        self.http.request("GET", &url, None)
    }
    fn mr(&self, value: &Value, intent: &Intent) -> Result<MergeRequest> {
        let mr = MergeRequest {
            project_id: number(value, "project_id")?,
            iid: number(value, "iid")?,
            draft: value
                .get("draft")
                .and_then(Value::as_bool)
                .ok_or("draft flag missing")?,
            source_branch: string(value, "source_branch")?,
            target_branch: string(value, "target_branch")?,
            head_commit: string(value, "sha")?,
            web_url: string(value, "web_url")?,
        };
        let url = Url::parse(&mr.web_url).map_err(|_| "MR URL invalid")?;
        if value.get("state").and_then(Value::as_str) != Some("opened")
            || mr.project_id != intent.target.project_id
            || mr.iid == 0
            || !mr.draft
            || mr.source_branch != intent.request.source_branch
            || mr.target_branch != intent.target.target_branch
            || mr.head_commit != intent.head_commit
            || url.origin() != self.origin.origin()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || !url
                .path()
                .ends_with(&format!("/-/merge_requests/{}", mr.iid))
        {
            return Err("MR binding mismatch".into());
        }
        Ok(mr)
    }
}
fn string(v: &Value, key: &str) -> Result<String> {
    v.get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| format!("missing {key}"))
}
fn number(v: &Value, key: &str) -> Result<u64> {
    v.get(key)
        .and_then(Value::as_u64)
        .ok_or_else(|| format!("missing {key}"))
}
fn marker(intent: &Intent) -> Result<String> {
    uuid::Uuid::parse_str(&intent.id).map_err(|_| "invalid intent ID")?;
    Ok(format!("<!-- agenct-publication:{} -->", intent.id))
}
impl GitLabTransport for GitLabClient {
    fn branch_head(&mut self, target: &Target, branch: &str) -> Result<String> {
        if branch.is_empty() || branch.len() > 256 || branch == "." || branch == ".." {
            return Err("invalid branch".into());
        }
        let url = self.url(target, &["repository", "branches", branch])?;
        string(&self.get(url)?["commit"], "id")
    }
    fn create_draft_merge_request(&mut self, intent: &Intent) -> Result<MergeRequest> {
        let url = self.url(&intent.target, &["merge_requests"])?;
        if intent.request.title.is_empty() || intent.request.title.len() > 200 {
            return Err("invalid title".into());
        }
        let body = json!({"source_branch":intent.request.source_branch,"target_branch":intent.target.target_branch,
            "title":format!("Draft: {}", intent.request.title),"description":marker(intent)?,
            "remove_source_branch":false,"squash":false});
        let value = self.http.request("POST", &url, Some(&body))?;
        self.mr(&value, intent)
    }
    fn find_merge_request(&mut self, intent: &Intent) -> Result<Option<MergeRequest>> {
        let marker = marker(intent)?;
        let mut found = None;
        // Bounded pagination; reaching cap is uncertainty, never evidence of absence.
        for page in 1..=10 {
            let mut url = self.url(&intent.target, &["merge_requests"])?;
            url.query_pairs_mut().extend_pairs([
                ("scope", "all"),
                ("state", "all"),
                ("source_branch", &intent.request.source_branch),
                ("target_branch", &intent.target.target_branch),
                ("per_page", "100"),
                ("page", &page.to_string()),
            ]);
            let data = self.get(url)?;
            let rows = data.as_array().ok_or("MR list invalid")?;
            if rows.len() > 100 {
                return Err("MR page oversized".into());
            }
            for row in rows {
                if row.get("description").and_then(Value::as_str) == Some(marker.as_str()) {
                    if found.is_some() {
                        return Err("ambiguous MR reconciliation".into());
                    }
                    found = Some(self.mr(row, intent)?);
                }
            }
            if rows.len() < 100 {
                return Ok(found);
            }
        }
        Err("MR reconciliation pagination limit".into())
    }
    fn pipeline_checks(
        &mut self,
        target: &Target,
        mr: &MergeRequest,
    ) -> Result<Vec<PipelineCheck>> {
        if mr.project_id != target.project_id || mr.iid == 0 {
            return Err("MR target mismatch".into());
        }
        // Use only latest MR pipeline, then verify both the pipeline and every job SHA.
        let url = self.url(target, &["merge_requests", &mr.iid.to_string()])?;
        let detail = self.get(url)?;
        if string(&detail, "sha")? != mr.head_commit {
            return Err("MR head changed".into());
        }
        let pipeline = &detail["head_pipeline"];
        if string(pipeline, "sha")? != mr.head_commit || string(pipeline, "status")? != "success" {
            return Err("pipeline not successful on candidate".into());
        }
        let id = number(pipeline, "id")?;
        let mut checks = Vec::new();
        for page in 1..=10 {
            let mut url = self.url(target, &["pipelines", &id.to_string(), "jobs"])?;
            url.query_pairs_mut().extend_pairs([
                ("include_retried", "false"),
                ("per_page", "100"),
                ("page", &page.to_string()),
            ]);
            let data = self.get(url)?;
            let jobs = data.as_array().ok_or("jobs invalid")?;
            if jobs.len() > 100 {
                return Err("jobs page oversized".into());
            }
            for job in jobs {
                checks.push(PipelineCheck {
                    name: string(job, "name")?,
                    head_commit: string(&job["commit"], "id")?,
                    passed: string(job, "status")? == "success",
                });
            }
            if jobs.len() < 100 {
                return Ok(checks);
            }
        }
        Err("CI pagination limit".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::publication::PublishRequest;
    use std::{
        collections::VecDeque,
        sync::{Arc, Mutex},
    };
    type Calls = Arc<Mutex<Vec<(String, String, Option<Value>)>>>;
    struct Fake {
        responses: VecDeque<Result<Value>>,
        calls: Calls,
    }
    impl Http for Fake {
        fn request(&mut self, method: &str, url: &Url, body: Option<&Value>) -> Result<Value> {
            self.calls
                .lock()
                .unwrap()
                .push((method.into(), url.to_string(), body.cloned()));
            self.responses.pop_front().expect("unexpected HTTP call")
        }
    }
    fn intent() -> Intent {
        Intent {
            id: "96776975-2fa3-4d43-bc5b-b5b688c8b28c".into(),
            owner: "owner".into(),
            request: PublishRequest {
                job_id: "job".into(),
                candidate_digest: "digest".into(),
                source_branch: "agent/topic".into(),
                title: "Feature".into(),
                request_id: "request".into(),
            },
            target: Target {
                project_id: 12,
                origin: "https://gitlab.example/".into(),
                target_branch: "main".into(),
            },
            base_commit: "a".repeat(40),
            head_commit: "b".repeat(40),
            status: "authorized".into(),
        }
    }
    fn remote(i: &Intent) -> Value {
        json!({"project_id":12,"iid":5,"draft":true,"state":"opened","source_branch":i.request.source_branch,
        "target_branch":"main","sha":i.head_commit,"web_url":"https://gitlab.example/team/project/-/merge_requests/5","description":marker(i).unwrap()})
    }
    fn client(responses: Vec<Result<Value>>) -> (GitLabClient, Calls) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        (
            GitLabClient::build(
                "https://gitlab.example",
                BTreeSet::from([12]),
                Box::new(Fake {
                    responses: responses.into(),
                    calls: calls.clone(),
                }),
            )
            .unwrap(),
            calls,
        )
    }
    #[test]
    fn origin_and_project_are_exact_allowlists() {
        for origin in [
            "http://gitlab.example",
            "https://user@gitlab.example",
            "https://gitlab.example/api",
            "https://gitlab.example?x=1",
            "https://gitlab.example#x",
        ] {
            assert!(GitLabClient::new(origin, BTreeSet::from([12]), "fake-token".into()).is_err());
        }
        assert!(GitLabClient::new(
            "https://gitlab.example",
            BTreeSet::from([12]),
            "secret\nheader=x".into()
        )
        .is_err());
        let (mut c, calls) = client(vec![]);
        for origin in [
            "https://evil.example",
            "https://gitlab.example:444",
            "https://gitlab.example/api",
        ] {
            let mut t = intent().target;
            t.origin = origin.into();
            assert!(c.branch_head(&t, "main").is_err());
        }
        let mut t = intent().target;
        t.project_id = 13;
        assert!(c.branch_head(&t, "main").is_err());
        assert!(calls.lock().unwrap().is_empty());
    }
    #[test]
    fn branch_is_encoded_as_one_segment() {
        let (mut c, calls) = client(vec![Ok(json!({"commit":{"id":"abc"}}))]);
        assert_eq!(
            c.branch_head(&intent().target, "topic/foo?x=https://evil")
                .unwrap(),
            "abc"
        );
        let seen = calls.lock().unwrap();
        assert!(seen[0]
            .1
            .contains("branches/topic%2Ffoo%3Fx=https:%2F%2Fevil"));
        assert_eq!(
            Url::parse(&seen[0].1).unwrap().host_str(),
            Some("gitlab.example")
        );
    }
    #[test]
    fn creation_only_sends_draft_and_marker_no_auto_merge() {
        let i = intent();
        let (mut c, calls) = client(vec![Ok(remote(&i))]);
        c.create_draft_merge_request(&i).unwrap();
        let seen = calls.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].0, "POST");
        assert_eq!(
            seen[0].1,
            "https://gitlab.example/api/v4/projects/12/merge_requests"
        );
        let body = seen[0].2.as_ref().unwrap();
        assert_eq!(body["title"], "Draft: Feature");
        assert_eq!(body["description"], marker(&i).unwrap());
        assert_eq!(body.as_object().unwrap().len(), 6);
    }
    #[test]
    fn rejects_remote_binding_changes_and_non_draft() {
        let i = intent();
        for (key, value) in [
            ("sha", json!("wrong")),
            ("draft", json!(false)),
            ("project_id", json!(13)),
            ("source_branch", json!("other")),
            ("web_url", json!("https://evil.example/-/merge_requests/5")),
        ] {
            let mut row = remote(&i);
            row[key] = value;
            let (mut c, _) = client(vec![Ok(row)]);
            assert!(c.create_draft_merge_request(&i).is_err());
        }
    }
    #[test]
    fn reconciliation_requires_unique_exact_marker() {
        let i = intent();
        let mut unrelated = remote(&i);
        unrelated["description"] = json!("not marker");
        let (mut c, _) = client(vec![Ok(json!([unrelated, remote(&i)]))]);
        assert_eq!(c.find_merge_request(&i).unwrap().unwrap().iid, 5);
        let (mut c, _) = client(vec![Ok(json!([remote(&i), remote(&i)]))]);
        assert!(c.find_merge_request(&i).is_err());
    }
    #[test]
    fn unsuccessful_write_is_never_retried() {
        let (mut c, calls) = client(vec![Err("timeout".into())]);
        assert!(c.create_draft_merge_request(&intent()).is_err());
        assert_eq!(calls.lock().unwrap().len(), 1);
    }
    #[test]
    fn ci_reads_latest_successful_pipeline_at_exact_sha() {
        let i = intent();
        let (mut c, calls) = client(vec![
            Ok(
                json!({"sha":i.head_commit,"head_pipeline":{"id":7,"sha":i.head_commit,"status":"success"}}),
            ),
            Ok(json!([{"name":"unit","status":"success","commit":{"id":i.head_commit}}])),
        ]);
        let mr = c.mr(&remote(&i), &i).unwrap();
        let checks = c.pipeline_checks(&i.target, &mr).unwrap();
        assert_eq!(checks[0].head_commit, i.head_commit);
        assert!(checks[0].passed);
        assert!(calls.lock().unwrap()[1].1.contains("/pipelines/7/jobs?"));
        let (mut c, calls) = client(vec![Ok(
            json!({"sha":i.head_commit,"head_pipeline":{"id":7,"sha":"other","status":"success"}}),
        )]);
        assert!(c.pipeline_checks(&i.target, &mr).is_err());
        assert_eq!(calls.lock().unwrap().len(), 1);
    }
    #[test]
    fn reconciliation_page_limit_is_not_absence() {
        let i = intent();
        let mut unrelated = remote(&i);
        unrelated["description"] = json!("unrelated");
        let page = Value::Array(vec![unrelated; 100]);
        let (mut c, calls) = client(vec![Ok(page); 10]);
        assert!(c.find_merge_request(&i).unwrap_err().contains("pagination"));
        assert_eq!(calls.lock().unwrap().len(), 10);
    }
    #[test]
    fn closed_mr_and_invalid_marker_fail_closed() {
        let mut i = intent();
        let mut row = remote(&i);
        row["state"] = json!("closed");
        let (mut c, _) = client(vec![Ok(json!([row]))]);
        assert!(c.find_merge_request(&i).is_err());
        let (mut c, calls) = client(vec![]);
        i.id = "not-a-uuid".into();
        assert!(c.create_draft_merge_request(&i).is_err());
        assert!(calls.lock().unwrap().is_empty());
    }
}

#[cfg(all(test, unix))]
mod process_tests {
    use super::*;
    use std::{fs, path::PathBuf};

    // A real subprocess with piped stdin/stdout exercises the production transport code.
    // The executable is injectable only through the private method, never public config.
    struct Fixture {
        dir: PathBuf,
    }
    impl Fixture {
        fn new(behavior: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("agenct-curl-{}", uuid::Uuid::new_v4()));
            fs::create_dir(&dir).unwrap();
            let script = format!("#!/usr/bin/python3\nimport sys,json,os\nconfig=sys.stdin.read()\nwith open({},'w') as f: json.dump({{'argv':sys.argv[1:],'config':config,'env':dict(os.environ)}},f)\n{}\n", serde_json::to_string(&dir.join("request.json").to_str().unwrap()).unwrap(), behavior);
            fs::write(dir.join("curl"), script).unwrap();
            Self { dir }
        }
        fn request(&self, body: Option<&Value>) -> Result<Value> {
            Curl {
                token: "fake-glpat-secret".into(),
            }
            .request_with_command(
                {
                    // Do not exec a freshly written script: a concurrent fork can
                    // transiently inherit its writable fd and cause ETXTBSY.
                    // Execute the stable interpreter, reading the script as data.
                    let mut command = Command::new("/usr/bin/python3");
                    command.arg(self.dir.join("curl"));
                    command
                },
                "POST",
                &Url::parse("https://gitlab.example/api/v4/projects/12/merge_requests").unwrap(),
                body,
            )
        }
        fn capture(&self) -> Value {
            serde_json::from_slice(&fs::read(self.dir.join("request.json")).unwrap()).unwrap()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn real_subprocess_receives_tls_limits_and_stdin_secret_not_argv() {
        let fixture = Fixture::new("sys.stdout.write('{\"ok\":true}\\n201')");
        let body = json!({"title":"Draft: 中文 \"quotes\" \\ slash\nline", "description":"<!-- marker -->"});
        assert_eq!(fixture.request(Some(&body)).unwrap(), json!({"ok":true}));
        let capture = fixture.capture();
        let args: Vec<&str> = capture["argv"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(
            args,
            vec![
                "--disable",
                "--proto",
                "=https",
                "--tlsv1.2",
                "--proxy",
                "",
                "--noproxy",
                "*",
                "--connect-timeout",
                "5",
                "--max-time",
                "20",
                "--max-filesize",
                "1048576",
                "--silent",
                "--request",
                "POST",
                "--url",
                "https://gitlab.example/api/v4/projects/12/merge_requests",
                "--config",
                "-",
                "--write-out",
                "\n%{http_code}"
            ]
        );
        assert!(!args.join(" ").contains("fake-glpat-secret"));
        let config = capture["config"].as_str().unwrap();
        let lines: Vec<_> = config.lines().collect();
        assert_eq!(lines.len(), 3); // Embedded quotes/newlines cannot inject curl directives.
        assert_eq!(lines[0], "header = \"PRIVATE-TOKEN: fake-glpat-secret\"");
        let decoded: String =
            serde_json::from_str(lines[2].strip_prefix("data-binary = ").unwrap()).unwrap();
        assert_eq!(serde_json::from_str::<Value>(&decoded).unwrap(), body);
        let env = capture["env"].as_object().unwrap();
        // Python may synthesize LC_CTYPE but the parent's credentials/proxies are absent.
        assert!(env.keys().all(|key| key == "LC_CTYPE"));
    }
    #[test]
    fn subprocess_tls_failure_and_timeout_are_uncertain_and_sanitized() {
        for code in [28, 60] {
            // curl timeout and certificate-validation errors.
            let fixture = Fixture::new(&format!("sys.stderr.write('fake-glpat-secret sensitive server detail')\nsys.stdout.write('sensitive body')\nsys.exit({code})"));
            assert_eq!(
                fixture.request(None).unwrap_err(),
                "GitLab transport failed; outcome may be uncertain"
            );
        }
    }
    #[test]
    fn subprocess_redirect_and_http_error_never_return_provider_body() {
        for status in [301, 302, 401, 403, 429, 500] {
            let fixture = Fixture::new(&format!(
                "sys.stdout.write('sensitive response\\n{status}')"
            ));
            assert_eq!(
                fixture.request(None).unwrap_err(),
                format!("GitLab HTTP {status}; outcome may be uncertain")
            );
        }
    }
    #[test]
    fn streamed_oversize_subprocess_is_killed_without_reading_unbounded_data() {
        let fixture = Fixture::new(
            "while True:\n sys.stdout.buffer.write(b'x'*65536)\n sys.stdout.buffer.flush()",
        );
        let start = std::time::Instant::now();
        assert_eq!(fixture.request(None).unwrap_err(), "GitLab response limit");
        assert!(start.elapsed() < std::time::Duration::from_secs(5));
    }
    #[test]
    fn subprocess_malformed_json_and_status_fail_closed() {
        for (output, expected) in [
            ("missing status", "GitLab malformed response"),
            ("{}\nxyz", "GitLab status missing"),
            ("bad json\n200", "GitLab invalid JSON"),
        ] {
            let fixture = Fixture::new(&format!(
                "sys.stdout.write({})",
                serde_json::to_string(output).unwrap()
            ));
            assert_eq!(fixture.request(None).unwrap_err(), expected);
        }
    }
    #[test]
    fn missing_executable_fails_without_a_request() {
        let error = Curl {
            token: "fake-token".into(),
        }
        .request_with_command(
            Command::new("/definitely-absent-agenct-curl"),
            "GET",
            &Url::parse("https://gitlab.example/").unwrap(),
            None,
        )
        .unwrap_err();
        assert_eq!(error, "GitLab TLS client unavailable");
    }
    #[test]
    fn fixture_script_open_for_writing_does_not_prevent_subprocess_start() {
        let fixture = Fixture::new("sys.stdout.write('{}\\n200')");
        // Deterministically reproduce the descriptor state that parallel forks
        // can briefly cause. Direct script exec would fail with ETXTBSY.
        let _writer = fs::OpenOptions::new()
            .write(true)
            .open(fixture.dir.join("curl"))
            .unwrap();
        assert_eq!(fixture.request(None).unwrap(), json!({}));
    }
}
