//! SSRF-hardened GitLab integration: Webhook ingress, Todo polling, commit digest, and gated replies.
use crate::{
    auth::sha256_hex,
    automation,
    config::{constant_time_eq, normalize_origin, now_unix, truncate_bytes},
    db::record_audit,
    denied, invalid, nodes, Hub, Result,
};
use axum::{extract::State, http::HeaderMap, Json};
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct GitLabConfig {
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub token: String,
    #[serde(default)]
    pub auto_poll_todos: bool,
    #[serde(default)]
    pub seen_todo_ids: Vec<i64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SaveGitLabInput {
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub token: Option<String>,
    #[serde(default)]
    pub clear_token: bool,
    #[serde(default)]
    pub auto_poll_todos: bool,
}

fn mask_secret(s: &str) -> String {
    let len = s.chars().count();
    if len == 0 {
        return String::new();
    }
    if len <= 8 {
        return "****".into();
    }
    let head: String = s.chars().take(4).collect();
    let tail: String = s.chars().skip(len - 4).collect();
    format!("{head}****{tail}")
}

fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

pub async fn load_config(hub: &Hub, user_id: &str) -> Result<GitLabConfig> {
    let uid = user_id.to_owned();
    let default_base = hub.config.default_gitlab_base();
    hub.db
        .read(move |conn| {
            let raw: Option<String> = conn
                .query_row(
                    "SELECT config_json FROM integrations WHERE user_id = ?1 AND kind = 'gitlab'",
                    [&uid],
                    |r| r.get(0),
                )
                .optional()?;
            let mut cfg: GitLabConfig = raw
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or_default();
            if cfg.base_url.is_empty() {
                cfg.base_url = default_base;
            }
            Ok(cfg)
        })
        .await
}

pub async fn get_config(State(hub): State<Hub>, headers: HeaderMap) -> Result<Json<Value>> {
    let user = hub.require_user(&headers).await?;
    let cfg = load_config(&hub, &user.id).await?;
    Ok(Json(json!({
        "base_url": cfg.base_url,
        "username": if cfg.username.is_empty() { user.username } else { cfg.username },
        "configured": !cfg.token.is_empty(),
        "token_preview": mask_secret(&cfg.token),
        "auto_poll_todos": cfg.auto_poll_todos,
        "allowed_origins": hub.config.allowed_gitlab_origins.iter().collect::<Vec<_>>(),
    })))
}

pub async fn save_config(
    State(hub): State<Hub>,
    headers: HeaderMap,
    Json(input): Json<SaveGitLabInput>,
) -> Result<Json<Value>> {
    let user = hub.require_user(&headers).await?;
    let base_raw = if input.base_url.trim().is_empty() {
        hub.config.default_gitlab_base()
    } else {
        input.base_url.trim().to_owned()
    };
    let base_url = normalize_origin(&base_raw, hub.config.local_http)
        .map_err(|_| invalid("invalid GitLab base_url"))?;
    if !hub.config.allowed_gitlab_origins.contains(&base_url) {
        return Err(invalid("GitLab base_url is not in server allowlist"));
    }

    let mut cfg = load_config(&hub, &user.id).await?;
    cfg.base_url = base_url;
    if !input.username.trim().is_empty() {
        cfg.username = truncate_bytes(input.username.trim(), 64);
    }
    if input.clear_token {
        cfg.token.clear();
    } else if let Some(tok) = input
        .token
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        if tok.len() > 512 {
            return Err(invalid("GitLab token too long"));
        }
        cfg.token = tok.to_owned();
    }
    cfg.auto_poll_todos = input.auto_poll_todos;

    let uid = user.id.clone();
    let encoded = serde_json::to_string(&cfg).map_err(|_| invalid("config encode error"))?;
    hub.db
        .write(move |conn| {
            conn.execute(
                "INSERT INTO integrations(user_id, kind, config_json, updated_at)
                 VALUES (?1, 'gitlab', ?2, ?3)
                 ON CONFLICT(user_id, kind) DO UPDATE SET config_json=excluded.config_json, updated_at=excluded.updated_at",
                params![uid, encoded, now_unix()],
            )?;
            record_audit(conn, &uid, "gitlab_config", "ok", "Updated GitLab integration", "")?;
            Ok(())
        })
        .await?;

    Ok(Json(json!({
        "ok": true,
        "base_url": cfg.base_url,
        "configured": !cfg.token.is_empty(),
        "token_preview": mask_secret(&cfg.token),
        "auto_poll_todos": cfg.auto_poll_todos,
    })))
}

pub async fn fetch_commit_summary(hub: &Hub, user_id: &str) -> Option<String> {
    let cfg = load_config(hub, user_id).await.ok()?;
    if cfg.token.is_empty() || !hub.config.allowed_gitlab_origins.contains(&cfg.base_url) {
        return None;
    }
    let events_url = format!("{}/api/v4/events?action=pushed&per_page=30", cfg.base_url);
    let projects_url = format!(
        "{}/api/v4/projects?membership=true&order_by=last_activity_at&per_page=100&simple=true",
        cfg.base_url
    );

    let (ev_res, proj_res) = tokio::join!(
        hub.http
            .get(&events_url)
            .header("PRIVATE-TOKEN", &cfg.token)
            .send(),
        hub.http
            .get(&projects_url)
            .header("PRIVATE-TOKEN", &cfg.token)
            .send(),
    );

    let ev_resp = ev_res.ok()?;
    if !ev_resp.status().is_success() {
        return None;
    }
    let events: Vec<Value> = ev_resp.json().await.ok()?;
    if events.is_empty() {
        return Some("最近没有新的 GitLab 推送记录。".into());
    }

    let mut project_names: std::collections::HashMap<i64, String> =
        std::collections::HashMap::new();
    if let Ok(pr) = proj_res {
        if pr.status().is_success() {
            if let Ok(projs) = pr.json::<Vec<Value>>().await {
                for p in projs {
                    if let (Some(id), Some(path)) = (
                        p["id"].as_i64(),
                        p["path_with_namespace"]
                            .as_str()
                            .or_else(|| p["name"].as_str()),
                    ) {
                        project_names.insert(id, path.to_owned());
                    }
                }
            }
        }
    }

    // Group non-empty push events by repository while preserving recency order.
    let mut repo_order: Vec<String> = Vec::new();
    let mut by_repo: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();
    let mut kept = 0usize;

    for ev in &events {
        if kept >= 16 {
            break;
        }
        let action = ev["action_name"].as_str().unwrap_or("");
        let count = ev["push_data"]["commit_count"].as_i64().unwrap_or(0);
        if count <= 0 || action == "deleted" {
            continue;
        }
        let repo = ev["project_id"]
            .as_i64()
            .and_then(|pid| project_names.get(&pid).cloned())
            .or_else(|| {
                ev["target_title"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| "未命名仓库".into());
        let author = ev["author_username"].as_str().unwrap_or("unknown");
        let branch = ev["push_data"]["ref"].as_str().unwrap_or("-");
        let title = ev["push_data"]["commit_title"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("（无提交说明）");
        let ts = format_gitlab_ts(ev["created_at"].as_str().unwrap_or(""));
        let ts_prefix = if ts.is_empty() {
            String::new()
        } else {
            format!("{ts} · ")
        };
        let line = format!("- {ts_prefix}`{branch}` ({count} commits) · @{author}：{title}");
        if !by_repo.contains_key(&repo) {
            repo_order.push(repo.clone());
        }
        by_repo.entry(repo).or_default().push(line);
        kept += 1;
    }

    if repo_order.is_empty() {
        return Some("最近没有新的 GitLab 代码提交记录。".into());
    }

    let mut sections = Vec::with_capacity(repo_order.len() + 1);
    sections.push("### 近期 GitLab 提交汇总".to_owned());
    for repo in repo_order {
        if let Some(lines) = by_repo.get(&repo) {
            sections.push(format!("**{repo}**\n{}", lines.join("\n")));
        }
    }
    Some(sections.join("\n\n"))
}

fn format_gitlab_ts(raw: &str) -> String {
    // Converts ISO-8601 like "2026-11-19T16:48:38.612+08:00" into "11-19 16:48".
    let bytes = raw.as_bytes();
    if bytes.len() >= 16
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && (bytes[10] == b'T' || bytes[10] == b' ')
    {
        return format!("{} {}", &raw[5..10], &raw[11..16]);
    }
    String::new()
}

async fn authenticate_webhook(hub: &Hub, headers: &HeaderMap) -> Result<Option<String>> {
    let provided = headers
        .get("x-gitlab-token")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if provided.is_empty() {
        return Err(denied());
    }
    if let Some(secret) = &hub.config.gitlab_webhook_secret {
        if constant_time_eq(provided.as_bytes(), secret.as_bytes()) {
            return Ok(None); // Workspace-scoped secret
        }
    }
    // Otherwise allow a user's own node token, strictly scoped to that owner.
    let hash = sha256_hex(provided);
    let owner: Option<String> = hub
        .db
        .read(move |conn| {
            Ok(conn
                .query_row(
                    "SELECT n.user_id FROM nodes n JOIN users u ON u.id=n.user_id
                     WHERE n.token_hash=?1 AND u.status='active'",
                    [hash],
                    |r| r.get(0),
                )
                .optional()?)
        })
        .await?;
    owner.map(Some).ok_or_else(denied)
}

pub async fn webhook(
    State(hub): State<Hub>,
    headers: HeaderMap,
    Json(payload): Json<Value>,
) -> Result<Json<Value>> {
    let scope_user = authenticate_webhook(&hub, &headers).await?;

    let body = payload["object_attributes"]["note"]
        .as_str()
        .or_else(|| payload["object_attributes"]["description"].as_str())
        .or_else(|| payload["body"].as_str())
        .or_else(|| payload["object_attributes"]["title"].as_str())
        .unwrap_or("")
        .trim();
    if body.is_empty() || body.len() > 8000 {
        return Err(invalid("webhook body must be 1..8000 bytes"));
    }
    if body.contains("[Agenct]") {
        return Ok(Json(
            json!({"ok": true, "ignored": "self-reply", "triggered": []}),
        ));
    }

    let project_path = payload["project"]["path_with_namespace"]
        .as_str()
        .unwrap_or("repo")
        .to_owned();
    let author = payload["user"]["username"]
        .as_str()
        .unwrap_or("gitlab")
        .to_owned();
    let is_mr = payload["object_kind"].as_str() == Some("merge_request")
        || payload["object_attributes"]["noteable_type"].as_str() == Some("MergeRequest")
        || !payload["merge_request"].is_null();
    let noteable_type = if is_mr { "MergeRequest" } else { "Issue" }.to_owned();
    let iid = payload["object_attributes"]["iid"]
        .as_i64()
        .or_else(|| payload["issue"]["iid"].as_i64())
        .or_else(|| payload["merge_request"]["iid"].as_i64());
    let web_url = payload["project"]["web_url"]
        .as_str()
        .or_else(|| payload["object_attributes"]["url"].as_str())
        .unwrap_or("");
    let origin_hint = extract_origin(web_url, hub.config.local_http)
        .filter(|o| hub.config.allowed_gitlab_origins.contains(o));

    let mentioned = find_mentioned_users(&hub, body).await?;
    let mut triggered = Vec::new();
    for uid in mentioned {
        if scope_user.as_ref().is_some_and(|s| s != &uid) {
            continue;
        }
        let status = dispatch_gitlab_mention(
            &hub,
            &uid,
            &project_path,
            &author,
            body,
            &noteable_type,
            iid,
            origin_hint.as_deref(),
        )
        .await?;
        triggered.push(json!({"user_id": uid, "status": status}));
    }

    Ok(Json(json!({"ok": true, "triggered": triggered})))
}

fn extract_origin(url: &str, allow_http: bool) -> Option<String> {
    let parsed = reqwest::Url::parse(url).ok()?;
    normalize_origin(&parsed.origin().ascii_serialization(), allow_http).ok()
}

async fn find_mentioned_users(hub: &Hub, body: &str) -> Result<Vec<String>> {
    let lower = body.to_ascii_lowercase();
    hub.db
        .read(move |conn| {
            let mut stmt = conn.prepare("SELECT id, username FROM users WHERE status='active'")?;
            let rows = stmt
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let mut out = Vec::new();
            for (id, uname) in rows {
                let needle = format!("@{}", uname.to_ascii_lowercase());
                if lower.contains(&needle) {
                    out.push(id);
                }
            }
            Ok(out)
        })
        .await
}

#[allow(clippy::too_many_arguments)]
async fn dispatch_gitlab_mention(
    hub: &Hub,
    user_id: &str,
    project_path: &str,
    author: &str,
    body: &str,
    noteable_type: &str,
    iid: Option<i64>,
    origin_hint: Option<&str>,
) -> Result<&'static str> {
    let uid = user_id.to_owned();
    let proj = project_path.to_owned();
    let auth_name = author.to_owned();
    let body_owned = body.to_owned();
    let ntype = noteable_type.to_owned();
    let origin = origin_hint.map(str::to_owned);

    let result_status = hub
        .db
        .write(move |conn| {
            let perms = automation::read_permissions_sync(conn, &uid)?;
            let now = now_unix();
            let note_msg = format!(
                "**[GitLab @提及 · {proj}]** 来自 @{auth_name}：\n\n{body_owned}"
            );
            conn.execute(
                "INSERT INTO messages(user_id, role, content, status, created_at)
                 VALUES (?1, 'system', ?2, 'complete', ?3)",
                params![uid, note_msg, now],
            )?;

            if !perms.gitlab_execute {
                record_audit(
                    conn,
                    &uid,
                    "gitlab_mention",
                    "notified_only",
                    &format!("{proj} by @{auth_name}"),
                    "gitlab_execute permission is disabled",
                )?;
                return Ok("notified_only");
            }
            let Some(node) = nodes::pick_node_sync(conn, &uid, None, false)? else {
                return Ok("no_node");
            };
            let (task_id, _) = nodes::create_task_and_enqueue_sync(
                conn,
                &uid,
                &uid,
                &node.id,
                &format!("GitLab {proj}: {}", truncate_bytes(&body_owned, 60)),
                &format!("GitLab {proj} ({ntype} #{}) by @{auth_name}:\n\n{body_owned}", iid.unwrap_or(0)),
                "gitlab",
            )?;
            if let (Some(base), Some(num)) = (origin, iid) {
                conn.execute(
                    "INSERT OR REPLACE INTO gitlab_reply_targets(task_id, user_id, base_url, project_path, noteable_type, noteable_iid)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![task_id, uid, base, proj, ntype, num],
                )?;
            }
            Ok("queued")
        })
        .await?;

    hub.events
        .emit(user_id.to_owned(), "message", json!({"source": "gitlab"}));
    if result_status == "queued" {
        hub.events
            .emit(user_id.to_owned(), "task", json!({"source": "gitlab"}));
    }
    Ok(result_status)
}

pub async fn maybe_reply_task(hub: &Hub, task_id: &str, node_name: &str, content: &str) {
    let tid = task_id.to_owned();
    let target: Option<(String, String, String, String, i64)> = hub
        .db
        .read(move |conn| {
            Ok(conn
                .query_row(
                    "SELECT user_id, base_url, project_path, noteable_type, noteable_iid
                     FROM gitlab_reply_targets WHERE task_id = ?1",
                    [tid],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
                )
                .optional()?)
        })
        .await
        .ok()
        .flatten();

    let Some((user_id, base_url, project_path, noteable_type, iid)) = target else {
        return;
    };
    let uid = user_id.clone();
    let allowed = hub
        .db
        .read(move |conn| Ok(automation::read_permissions_sync(conn, &uid)?.gitlab_reply))
        .await
        .unwrap_or(false);
    if !allowed {
        return;
    }
    let Ok(cfg) = load_config(hub, &user_id).await else {
        return;
    };
    if cfg.token.is_empty()
        || cfg.base_url != base_url
        || !hub.config.allowed_gitlab_origins.contains(&base_url)
    {
        return;
    }

    let resource = if noteable_type == "MergeRequest" {
        "merge_requests"
    } else {
        "issues"
    };
    let url = format!(
        "{base_url}/api/v4/projects/{}/{resource}/{iid}/notes",
        percent_encode(&project_path)
    );
    let body = format!(
        "[Agenct] 节点 `{node_name}` 处理结果：\n\n{}",
        truncate_bytes(content, 6000)
    );
    let res = hub
        .http
        .post(&url)
        .header("PRIVATE-TOKEN", &cfg.token)
        .json(&json!({"body": body}))
        .send()
        .await;
    if res.is_ok_and(|r| r.status().is_success()) {
        let tid2 = task_id.to_owned();
        let _ = hub
            .db
            .write(move |conn| {
                conn.execute(
                    "DELETE FROM gitlab_reply_targets WHERE task_id = ?1",
                    [tid2],
                )?;
                Ok(())
            })
            .await;
    }
}

pub async fn poll_all_todos(hub: &Hub) {
    let users: Vec<(String, GitLabConfig)> = hub
        .db
        .read(|conn| {
            let mut stmt = conn.prepare(
                "SELECT i.user_id, i.config_json
                 FROM integrations i JOIN users u ON u.id = i.user_id
                 WHERE i.kind = 'gitlab' AND u.status = 'active'",
            )?;
            let rows =
                stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
            let mut out = Vec::new();
            for row in rows.flatten() {
                if let Ok(cfg) = serde_json::from_str::<GitLabConfig>(&row.1) {
                    if cfg.auto_poll_todos && !cfg.token.is_empty() {
                        out.push((row.0, cfg));
                    }
                }
            }
            Ok(out)
        })
        .await
        .unwrap_or_default();

    for (uid, mut cfg) in users {
        if !hub.config.allowed_gitlab_origins.contains(&cfg.base_url) {
            continue;
        }
        let url = format!("{}/api/v4/todos?state=pending&per_page=10", cfg.base_url);
        let Ok(resp) = hub
            .http
            .get(&url)
            .header("PRIVATE-TOKEN", &cfg.token)
            .send()
            .await
        else {
            continue;
        };
        if !resp.status().is_success() {
            continue;
        }
        let Ok(items) = resp.json::<Vec<Value>>().await else {
            continue;
        };
        let mut changed = false;
        for item in items {
            let Some(todo_id) = item["id"].as_i64() else {
                continue;
            };
            if cfg.seen_todo_ids.contains(&todo_id) {
                continue;
            }
            cfg.seen_todo_ids.push(todo_id);
            if cfg.seen_todo_ids.len() > 200 {
                let drop_n = cfg.seen_todo_ids.len() - 200;
                cfg.seen_todo_ids.drain(0..drop_n);
            }
            changed = true;
            let body = item["body"].as_str().unwrap_or("").trim();
            if body.is_empty() || body.contains("[Agenct]") {
                continue;
            }
            let proj = item["project"]["path_with_namespace"]
                .as_str()
                .unwrap_or("repo");
            let author = item["author"]["username"].as_str().unwrap_or("gitlab");
            let ntype = item["target_type"].as_str().unwrap_or("Issue");
            let iid = item["target"]["iid"].as_i64();
            let _ = dispatch_gitlab_mention(
                hub,
                &uid,
                proj,
                author,
                body,
                ntype,
                iid,
                Some(&cfg.base_url),
            )
            .await;
        }
        if changed {
            if let Ok(encoded) = serde_json::to_string(&cfg) {
                let u_clone = uid.clone();
                let _ = hub
                    .db
                    .write(move |conn| {
                        conn.execute(
                            "UPDATE integrations SET config_json=?2, updated_at=?3
                             WHERE user_id=?1 AND kind='gitlab'",
                            params![u_clone, encoded, now_unix()],
                        )?;
                        Ok(())
                    })
                    .await;
            }
        }
    }
}
