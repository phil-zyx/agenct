//! Owner-managed private staging membership and content-free HTTP event logs.
use super::*;
use std::io::Read;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PrivateConfig {
    version: u32,
    port: u16,
    runtime: String,
    users: Vec<UserConfig>,
}

impl Hub {
    pub fn with_private_config(
        mut self,
        path: PathBuf,
    ) -> std::result::Result<Self, Box<dyn std::error::Error>> {
        if self.oidc.is_some() || self.browser_auth.is_some() || self.directory.tenants_enabled() {
            return Err("private configuration cannot override OIDC membership".into());
        }
        self.validate_startup()?;
        self.private_config = Some(path);
        self.reload_private_users()
            .map_err(|_| "invalid private configuration")?;
        Ok(self)
    }

    pub(super) fn gate(&self, id: &str) -> Result<Arc<tokio::sync::Mutex<()>>> {
        if let Some(gate) = self.gates.get(id) {
            return Ok(gate.clone());
        }
        let mut gates = self
            .extra_gates
            .lock()
            .map_err(|_| Error(StatusCode::INTERNAL_SERVER_ERROR, "gate unavailable"))?;
        Ok(gates
            .entry(id.to_owned())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone())
    }

    pub(super) fn reload_private_users(&self) -> Result<Option<Roster>> {
        self.reload_private_users_before_read(|| {})
    }

    fn reload_private_users_before_read(
        &self,
        before_read: impl FnOnce(),
    ) -> Result<Option<Roster>> {
        let Some(path) = &self.private_config else {
            return Ok(None);
        };
        let unavailable = || {
            Error(
                StatusCode::SERVICE_UNAVAILABLE,
                "private configuration unavailable",
            )
        };
        // Serialize the snapshot read as well as its application. Otherwise an
        // older reader can overwrite a newer rotation after waiting for this lock.
        let mut cache = self.private_users.lock().map_err(|_| unavailable())?;
        before_read();
        let metadata = std::fs::symlink_metadata(path).map_err(|_| unavailable())?;
        if !metadata.is_file() {
            return Err(unavailable());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if metadata.permissions().mode() & 0o077 != 0 {
                return Err(unavailable());
            }
        }
        let mut bytes = Vec::new();
        std::fs::File::open(path)
            .map_err(|_| unavailable())?
            .take(131073)
            .read_to_end(&mut bytes)
            .map_err(|_| unavailable())?;
        if bytes.len() > 131072 {
            return Err(unavailable());
        }
        // Permissions and content are still checked on every request (rotation and
        // revocation take effect immediately, a broken file fails closed), but an
        // unchanged roster is not re-parsed, re-applied or re-written to the database.
        let digest = format!("{:x}", Sha256::digest(&bytes));
        if let Some((cached, users)) = cache.as_ref() {
            if *cached == digest {
                return Ok(Some(users.clone()));
            }
        }
        let config: PrivateConfig = serde_json::from_slice(&bytes).map_err(|_| unavailable())?;
        if config.version != 1
            || config.port < 1024
            || !matches!(config.runtime.as_str(), "mock" | "dsh")
        {
            return Err(unavailable());
        }
        let directory = directory::Directory::new(config.users.iter().map(|user| user.id.clone()))
            .map_err(|_| unavailable())?;
        let mut users = HashMap::new();
        for user in config.users {
            if user.token_sha256.len() != 64
                || !user
                    .token_sha256
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                || users.insert(user.token_sha256, user.id).is_some()
            {
                return Err(unavailable());
            }
        }
        let db = self.db()?;
        for id in users.values() {
            db.execute(
                "INSERT OR IGNORE INTO sessions(owner,session_id) VALUES (?1,?2)",
                params![id, Uuid::new_v4().to_string()],
            )?;
        }
        self.directory.replace(directory);
        let users = Arc::new(users);
        *cache = Some((digest, users.clone()));
        Ok(Some(users))
    }
}

pub(super) async fn event_log(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    // Matched route templates contain no user-provided IDs, queries, headers or bodies.
    let route = request
        .extensions()
        .get::<axum::extract::MatchedPath>()
        .map(|p| p.as_str().to_owned())
        .unwrap_or_else(|| "static".into());
    let method = request.method().clone();
    let started = std::time::Instant::now();
    let response = next.run(request).await;
    eprintln!(
        "{}",
        serde_json::json!({"event":"http_request", "at_unix_ms":std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis(), "method":method.as_str(), "route":route, "status":response.status().as_u16(), "elapsed_ms":started.elapsed().as_millis()})
    );
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn live_add_rotation_and_invalid_configuration_fail_closed() {
        let root = std::env::temp_dir().join(format!("agenct-live-users-{}", Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let path = root.join("config.json");
        let alice = "a".repeat(40);
        let bob = "b".repeat(40);
        let rotated = "c".repeat(40);
        let user = |id: &str, token: &str| serde_json::json!({"id":id,"token_sha256":format!("{:x}",Sha256::digest(token.as_bytes()))});
        let write = |users| {
            std::fs::write(
                &path,
                serde_json::json!({"version":1,"port":8080,"runtime":"mock","users":users})
                    .to_string(),
            )
            .unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
            }
        };
        write(vec![user("alice", &alice)]);
        let hub = Hub::open(Config {
            bind: "127.0.0.1:0".parse().unwrap(),
            database: root.join("hub.sqlite3"),
            runtime: Runtime::Mock,
            users: vec![UserConfig {
                id: "alice".into(),
                token_sha256: format!("{:x}", Sha256::digest(alice.as_bytes())),
            }],
        })
        .unwrap()
        .with_private_config(path.clone())
        .unwrap();
        let headers = |token: &str| {
            let mut h = HeaderMap::new();
            h.insert("authorization", format!("Bearer {token}").parse().unwrap());
            h
        };
        assert_eq!(hub.identity(&headers(&alice)).unwrap(), "alice");
        write(vec![user("alice", &alice), user("bob", &bob)]);
        assert_eq!(hub.identity(&headers(&bob)).unwrap(), "bob");
        assert!(hub.directory.can_delegate("alice", "bob"));
        let gate = hub.gate("bob").unwrap().try_lock_owned().unwrap();
        assert!(hub.clone().gate("bob").unwrap().try_lock_owned().is_err());
        drop(gate);
        use axum::{body::Body, http::Request};
        use tower::ServiceExt;
        let router = app(hub.clone());
        hub.db().unwrap().execute("INSERT INTO tasks(id,requester,owner,summary,status) VALUES ('live-task','alice','bob','Draft a short note','working')", []).unwrap();
        for (endpoint, body) in [
            (
                "/api/messages",
                serde_json::json!({"message":"hello","request_id":"live-chat"}),
            ),
            (
                "/api/tasks/live-task/drafts",
                serde_json::json!({"request_id":"live-draft","expected_revision":0}),
            ),
        ] {
            let response = router
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri(endpoint)
                        .header("authorization", format!("Bearer {bob}"))
                        .header("content-type", "application/json")
                        .body(Body::from(body.to_string()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                StatusCode::OK,
                "new user's chat and draft must use a shared execution gate"
            );
        }
        drop(router);
        assert!(hub
            .db()
            .unwrap()
            .query_row(
                "SELECT session_id FROM sessions WHERE owner='bob'",
                [],
                |r| r.get::<_, String>(0)
            )
            .is_ok());
        // A queued reload must read after the preceding update, never capture an
        // old file before waiting and then overwrite a completed token rotation.
        let cache = hub.private_users.lock().unwrap();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (read_tx, read_rx) = std::sync::mpsc::channel();
        let reader_hub = hub.clone();
        let reader = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            reader_hub.reload_private_users_before_read(|| {
                assert!(reader_hub.private_users.try_lock().is_err());
                read_tx.send(()).unwrap();
            })
        });
        started_rx.recv().unwrap();
        assert!(read_rx.try_recv().is_err());
        write(vec![user("alice", &rotated), user("bob", &bob)]);
        drop(cache);
        read_rx.recv().unwrap();
        let roster = reader.join().unwrap().unwrap().unwrap();
        assert!(!roster.contains_key(&format!("{:x}", Sha256::digest(alice.as_bytes()))));
        assert_eq!(
            roster.get(&format!("{:x}", Sha256::digest(rotated.as_bytes()))),
            Some(&"alice".to_owned())
        );
        assert_eq!(
            hub.identity(&headers(&alice)).unwrap_err().0,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(hub.identity(&headers(&rotated)).unwrap(), "alice");
        std::fs::write(&path, "invalid").unwrap();
        assert_eq!(
            hub.identity(&headers(&rotated)).unwrap_err().0,
            StatusCode::SERVICE_UNAVAILABLE
        );
        drop(hub);
        std::fs::remove_dir_all(root).unwrap();
    }
}
