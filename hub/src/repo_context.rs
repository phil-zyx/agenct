//! Opt-in, owner-allowlisted immutable Git-object reads. No shell, Git or repository code.
use super::*;
use std::{
    io::{Read, Write},
    process::{Command, Stdio},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Repository {
    id: String,
    label: String,
    owners: Vec<String>,
    objects_dir: PathBuf,
}
#[derive(Clone)]
pub(super) struct Repositories {
    entries: Vec<Repository>,
    gate: PathBuf,
    permits: Arc<tokio::sync::Semaphore>,
}
impl Repositories {
    pub(super) fn validate_tenants(
        &self,
        directory: &crate::directory::Directory,
    ) -> std::result::Result<(), &'static str> {
        if directory.tenants_enabled() {
            for entry in &self.entries {
                let tenant = directory
                    .tenant(&entry.owners[0])
                    .ok_or("repository owner is not assigned to a tenant")?;
                if entry
                    .owners
                    .iter()
                    .any(|owner| directory.tenant(owner) != Some(tenant))
                {
                    return Err("a repository allowlist cannot cross tenant boundaries");
                }
            }
        }
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Selection {
    repository: String,
    commit: String,
    path: Option<String>,
    query: Option<String>,
}

fn private_file(
    path: &std::path::Path,
) -> std::result::Result<Vec<u8>, Box<dyn std::error::Error>> {
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.len() > 65536 {
        return Err("bounded regular private file required".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err("owner-only file required".into());
        }
    }
    Ok(std::fs::read(path)?)
}
fn check_gate(path: &std::path::Path) -> std::result::Result<(), Box<dyn std::error::Error>> {
    let value: serde_json::Value = serde_json::from_slice(&private_file(path)?)?;
    let checked = value["checked_at"]
        .as_u64()
        .ok_or("boundary check timestamp required")?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let hash = format!(
        "{:x}",
        Sha256::digest(include_bytes!("../../sandbox/docker_smoke/probe.c"))
    );
    if value["version"] != 1
        || value["scope"] != "fixed-readonly-context-boundary"
        || value["production_execution_enabled"] != false
        || value["probe_sha256"] != hash
        || checked > now
        || now - checked > 7 * 24 * 3600
        || value["checks"]
            != serde_json::json!([
                "network",
                "filesystem",
                "pid",
                "cpu",
                "memory",
                "timeout",
                "revocation",
                "cleanup"
            ])
    {
        return Err("current local fixed boundary verification required".into());
    }
    Ok(())
}
impl Hub {
    /// This enables only bounded object reads, never worker execution or model tools.
    pub fn with_repositories(
        mut self,
        config: PathBuf,
        gate: PathBuf,
    ) -> std::result::Result<Self, Box<dyn std::error::Error>> {
        check_gate(&gate)?;
        let entries: Vec<Repository> = serde_json::from_slice(&private_file(&config)?)?;
        if entries.is_empty() || entries.len() > 32 {
            return Err("configure 1..32 repositories".into());
        }
        let mut ids = std::collections::HashSet::new();
        for repository in &entries {
            if repository.id.is_empty()
                || repository.id.len() > 64
                || !repository
                    .id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
                || !ids.insert(&repository.id)
                || repository.label.trim().is_empty()
                || repository.label.len() > 128
                || repository.owners.is_empty()
                || repository
                    .owners
                    .iter()
                    .any(|owner| !self.directory.contains(owner))
                || !repository.objects_dir.is_absolute()
                || !repository.objects_dir.is_dir()
                || repository.objects_dir.canonicalize()? != repository.objects_dir
            {
                return Err("invalid repository allowlist".into());
            }
        }
        let repositories = Repositories {
            entries,
            gate,
            permits: Arc::new(tokio::sync::Semaphore::new(2)),
        };
        repositories.validate_tenants(&self.directory)?;
        self.repositories = Some(Arc::new(repositories));
        Ok(self)
    }
}
fn configured(h: &Hub) -> Result<&Repositories> {
    let repositories = h.repositories.as_deref().ok_or(Error(
        StatusCode::SERVICE_UNAVAILABLE,
        "read-only repositories are not configured",
    ))?;
    check_gate(&repositories.gate).map_err(|_| {
        Error(
            StatusCode::SERVICE_UNAVAILABLE,
            "repository boundary verification missing or expired",
        )
    })?;
    Ok(repositories)
}
pub(super) async fn list(
    State(h): State<Hub>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>> {
    let owner = h.identity(&headers)?;
    let Some(_) = h.repositories else {
        return Ok(Json(serde_json::json!([])));
    };
    Ok(Json(serde_json::json!(configured(&h)?
        .entries
        .iter()
        .filter(|entry| entry.owners.contains(&owner))
        .map(|entry| serde_json::json!({"id":entry.id,"label":entry.label}))
        .collect::<Vec<_>>())))
}
pub(super) async fn read(
    State(h): State<Hub>,
    headers: HeaderMap,
    Json(selection): Json<Selection>,
) -> Result<Json<serde_json::Value>> {
    let owner = h.identity(&headers)?;
    Ok(Json(capture(&h, &owner, selection).await?))
}
async fn capture(h: &Hub, owner: &str, selection: Selection) -> Result<serde_json::Value> {
    let repositories = configured(h)?;
    let permit = repositories
        .permits
        .clone()
        .try_acquire_owned()
        .map_err(|_| Error(StatusCode::TOO_MANY_REQUESTS, "repository reader busy"))?;
    let entry = repositories
        .entries
        .iter()
        .find(|entry| entry.id == selection.repository && entry.owners.iter().any(|id| id == owner))
        .ok_or_else(denied)?;
    if !matches!(selection.commit.len(), 40 | 64)
        || !selection
            .commit
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        || selection.path.is_some() == selection.query.is_some()
        || selection.path.as_ref().is_some_and(|s| s.len() > 1024)
        || selection.query.as_ref().is_some_and(|s| s.len() > 200)
    {
        return Err(invalid(
            "select a full commit and one file or literal query",
        ));
    }
    let request = serde_json::json!({"objects_dir":entry.objects_dir,"commit":selection.commit,"path":selection.path,"query":selection.query});
    let repository = entry.id.clone();
    let raw = serde_json::to_vec(&request).map_err(|_| invalid("invalid repository request"))?;
    let mut output = tokio::task::spawn_blocking(move || {
        // Keep admission until the bounded child is reaped, even if the HTTP
        // request future is canceled while this blocking worker is still running.
        let _permit = permit;
        helper(&raw)
    })
    .await
    .map_err(|_| Error(StatusCode::BAD_GATEWAY, "repository read failed"))??;
    output["repository"] = serde_json::json!(repository);
    Ok(output)
}
fn helper(input: &[u8]) -> Result<serde_json::Value> {
    let failed = || {
        Error(StatusCode::BAD_REQUEST,"repository context unavailable: select a bounded UTF-8 file at a valid loose-object commit")
    };
    if input.len() > 8192 {
        return Err(failed());
    }
    // Embed both trusted parser modules: installed binaries do not depend on a
    // build-machine source path, Python import path, or current working directory.
    let program = format!(
        "import sys,types\nm=types.ModuleType('independent_verifier')\nsys.modules[m.__name__]=m\nexec({},m.__dict__)\nexec({},{{'__name__':'__main__'}})\n",
        serde_json::to_string(include_str!("../../sandbox/independent_verifier.py")).expect("embedded verifier string"),
        serde_json::to_string(include_str!("../../sandbox/repo_context.py")).expect("embedded reader string"),
    );
    let mut child = Command::new("/usr/bin/python3")
        .arg("-I")
        .arg("-c")
        .arg(program)
        .current_dir("/")
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("LANG", "C.UTF-8")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| failed())?;
    let mut stdin = child.stdin.take().ok_or_else(failed)?;
    if stdin.write_all(input).is_err() {
        let _ = child.kill();
        let _ = child.wait();
        return Err(failed());
    }
    drop(stdin);
    let stdout = child.stdout.take().ok_or_else(failed)?;
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout.take(12001).read_to_end(&mut bytes).map(|_| bytes)
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    let bytes = reader.join().map_err(|_| failed())?.map_err(|_| failed())?;
    if status.is_none_or(|status| !status.success()) || bytes.len() > 12000 {
        return Err(failed());
    }
    let value: serde_json::Value = serde_json::from_slice(&bytes).map_err(|_| failed())?;
    if value["read_only"] != true || !value["text"].is_string() {
        return Err(failed());
    }
    Ok(value)
}
/// Commands are user input stored in the ordinary request-id binding. The model
/// receives only this explicit selection; it cannot request another repository.
pub(super) async fn from_command(
    h: &Hub,
    owner: &str,
    message: &str,
) -> Result<Option<serde_json::Value>> {
    let search = message.starts_with("/repo-search ");
    if !search && !message.starts_with("/repo ") {
        return Ok(None);
    }
    let (first, question) = message
        .split_once('\n')
        .ok_or_else(|| invalid("put your repository question on the next line"))?;
    if question.trim().is_empty() {
        return Err(invalid("repository question required"));
    }
    let mut parts = first.splitn(4, ' ');
    parts.next();
    let repository = parts
        .next()
        .ok_or_else(|| invalid("repository ID required"))?
        .to_owned();
    let commit = parts
        .next()
        .ok_or_else(|| invalid("full commit required"))?
        .to_owned();
    let selector = parts
        .next()
        .ok_or_else(|| invalid("file path or literal query required"))?
        .to_owned();
    let selection = Selection {
        repository,
        commit,
        path: if search { None } else { Some(selector.clone()) },
        query: if search { Some(selector) } else { None },
    };
    Ok(Some(capture(h, owner, selection).await?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::Request};
    use base64::{engine::general_purpose::STANDARD, Engine};
    use http_body_util::BodyExt;
    use tower::ServiceExt;
    async fn request(
        router: &Router,
        owner: &str,
        method: &str,
        path: &str,
        body: serde_json::Value,
    ) -> (StatusCode, serde_json::Value) {
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(path)
                    .header(
                        "authorization",
                        format!("Bearer {owner}-{}", "x".repeat(40)),
                    )
                    .header("content-type", "application/json")
                    .body(if method == "GET" {
                        Body::empty()
                    } else {
                        Body::from(body.to_string())
                    })
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap())
    }
    fn write_private(path: &std::path::Path, value: serde_json::Value) {
        std::fs::write(path, value.to_string()).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
    }
    #[test]
    fn repository_owner_allowlists_cannot_span_organizations() {
        let directory = crate::directory::Directory::new(["alice".into(), "bob".into()])
            .unwrap()
            .with_tenants(HashMap::from([
                ("alice".into(), "one".into()),
                ("bob".into(), "two".into()),
            ]))
            .unwrap();
        let mut repos = Repositories {
            entries: vec![Repository {
                id: "shared".into(),
                label: "Shared".into(),
                owners: vec!["alice".into(), "bob".into()],
                objects_dir: "/unused".into(),
            }],
            gate: "/unused".into(),
            permits: Arc::new(tokio::sync::Semaphore::new(2)),
        };
        assert!(repos.validate_tenants(&directory).is_err());
        repos.entries[0].owners.pop();
        assert!(repos.validate_tenants(&directory).is_ok());
    }

    #[tokio::test]
    async fn explicit_selection_is_owner_scoped_next_turn_only_and_receipt_bound() {
        let root = std::env::temp_dir().join(format!("agenct-repo-test-{}", Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let objects = root.join("objects");
        std::fs::create_dir(&objects).unwrap();
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/readonly-objects.json")).unwrap();
        for (id, content) in fixture["objects"].as_object().unwrap() {
            std::fs::create_dir_all(objects.join(&id[..2])).unwrap();
            std::fs::write(
                objects.join(&id[..2]).join(&id[2..]),
                STANDARD.decode(content.as_str().unwrap()).unwrap(),
            )
            .unwrap();
        }
        let gate = root.join("gate.json");
        let config = root.join("repositories.json");
        // Offline contract fixture only; actual Docker verification is a separate test.
        write_private(
            &gate,
            serde_json::json!({"version":1,"scope":"fixed-readonly-context-boundary","production_execution_enabled":false,"checked_at":SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs(),"probe_sha256":format!("{:x}",Sha256::digest(include_bytes!("../../sandbox/docker_smoke/probe.c"))),"checks":["network","filesystem","pid","cpu","memory","timeout","revocation","cleanup"]}),
        );
        write_private(
            &config,
            serde_json::json!([{"id":"chosen","label":"Selected repository","owners":["alice"],"objects_dir":objects}]),
        );
        let calls = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
        let seen = calls.clone();
        let adapter=Router::new().route("/v1/turn",post(move |Json(value):Json<serde_json::Value>|{let seen=seen.clone();async move {seen.lock().unwrap().push(value);Json(serde_json::json!({"reply":"Selected file reviewed as quoted text.","backend":"dsh","proposal":null}))}}));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1/turn", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, adapter).await.unwrap() });
        let hub = Hub::open(Config {
            bind: "127.0.0.1:0".parse().unwrap(),
            database: ":memory:".into(),
            runtime: Runtime::Dsh {
                url,
                token: "t".repeat(40),
            },
            users: ["alice", "bob"]
                .map(|id| UserConfig {
                    id: id.into(),
                    token_sha256: format!(
                        "{:x}",
                        Sha256::digest(format!("{id}-{}", "x".repeat(40)).as_bytes())
                    ),
                })
                .into(),
        })
        .unwrap()
        .with_repositories(config, gate.clone())
        .unwrap();
        let router = app(hub);
        assert_eq!(
            request(
                &router,
                "bob",
                "GET",
                "/api/repositories",
                serde_json::Value::Null
            )
            .await
            .1,
            serde_json::json!([])
        );
        assert_eq!(
            request(
                &router,
                "alice",
                "GET",
                "/api/repositories",
                serde_json::Value::Null
            )
            .await
            .1[0]["id"],
            "chosen"
        );
        let selection = serde_json::json!({"repository":"chosen","commit":fixture["commit"],"path":"notes.txt"});
        assert_eq!(
            request(
                &router,
                "bob",
                "POST",
                "/api/repositories/context",
                selection.clone()
            )
            .await
            .0,
            StatusCode::NOT_FOUND
        );
        let read = request(
            &router,
            "alice",
            "POST",
            "/api/repositories/context",
            selection,
        )
        .await;
        assert_eq!(read.0, StatusCode::OK);
        assert!(read.1["text"].as_str().unwrap().contains("budget is 2400"));
        let command = format!(
            "/repo chosen {} notes.txt\nExplain the budget.",
            fixture["commit"].as_str().unwrap()
        );
        let turn = serde_json::json!({"request_id":"selected","message":command});
        let first = request(&router, "alice", "POST", "/api/messages", turn.clone()).await;
        assert_eq!(first.0, StatusCode::OK);
        assert_eq!(first.1["repository_context"]["commit"], fixture["commit"]);
        assert!(calls.lock().unwrap()[0]["history"]
            .to_string()
            .contains("budget is 2400"));
        std::fs::remove_dir_all(&objects).unwrap();
        assert_eq!(
            request(&router, "alice", "POST", "/api/messages", turn).await,
            first,
            "replay does not reread repository or call model"
        );
        assert_eq!(calls.lock().unwrap().len(), 1);
        assert_eq!(request(&router,"alice","POST","/api/messages",serde_json::json!({"request_id":"ordinary","message":"Now answer without repository context"})).await.0,StatusCode::OK);
        assert!(!calls.lock().unwrap()[1]["history"]
            .to_string()
            .contains("Only selected repository context."));
        assert_eq!(
            request(
                &router,
                "alice",
                "POST",
                "/api/messages",
                serde_json::json!({"request_id":"selected","message":"changed selection"})
            )
            .await
            .0,
            StatusCode::CONFLICT
        );
        std::fs::remove_file(gate).unwrap();
        assert_eq!(
            request(
                &router,
                "alice",
                "GET",
                "/api/repositories",
                serde_json::Value::Null
            )
            .await
            .0,
            StatusCode::SERVICE_UNAVAILABLE
        );
        server.abort();
        drop(router);
        std::fs::remove_dir_all(root).unwrap();
    }
}
