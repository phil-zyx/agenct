//! Private, immutable profile change proposals. Only an authenticated confirm action applies one.
use super::*;
use serde_json::{json, Value};

pub(super) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS profile_suggestions (
 id TEXT PRIMARY KEY, owner TEXT NOT NULL REFERENCES sessions(owner),
 request_id TEXT NOT NULL, expected_revision INTEGER NOT NULL CHECK(expected_revision>=0),
 changes_json TEXT NOT NULL,
 status TEXT NOT NULL CHECK(status IN ('pending','confirmed','rejected','superseded')),
 created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP, UNIQUE(owner,request_id));
CREATE INDEX IF NOT EXISTS profile_suggestions_owner_status ON profile_suggestions(owner,status);";

const COLUMNS: &str = "id,expected_revision,changes_json,status,created_at";
fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<(String, u64, String, String, String)> {
    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
}
fn value(saved: (String, u64, String, String, String)) -> Result<Value> {
    let (id, expected_revision, raw, status, created_at) = saved;
    let changes: Value = serde_json::from_str(&raw).map_err(|_| {
        Error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "invalid saved profile suggestion",
        )
    })?;
    Ok(
        json!({"id":id,"expected_revision":expected_revision,"changes":changes,"status":status,"created_at":created_at}),
    )
}
pub(super) fn validate_changes(changes: &Value) -> Result<()> {
    let sections = changes
        .as_object()
        .ok_or(invalid("profile changes must be an object"))?;
    let mut count = 0;
    for (section, fields) in sections {
        let allowed: &[&str] = match section.as_str() {
            "public" => &[
                "team",
                "responsibilities",
                "help_with",
                "request_requirements",
            ],
            "private" => &["work_context", "response_preferences"],
            "permissions" => &["auto_reply_public", "auto_prepare_drafts"],
            _ => return Err(invalid("unknown profile change section")),
        };
        let fields = fields
            .as_object()
            .ok_or(invalid("profile change section must be an object"))?;
        for (field, proposed) in fields {
            if !allowed.contains(&field.as_str()) {
                return Err(invalid("unknown profile change field"));
            }
            if section == "permissions" {
                if !proposed.is_boolean() {
                    return Err(invalid("profile permission must be a boolean"));
                }
            } else if !proposed.is_string() {
                return Err(invalid("profile value must be a string"));
            }
            count += 1;
        }
    }
    if count == 0 {
        return Err(invalid("profile changes must include at least one field"));
    }
    let merged = merge(&crate::collaboration::Profile::default(), changes)?;
    crate::collaboration::validate_profile(&merged.public, &merged.private)
}
fn merge(
    profile: &crate::collaboration::Profile,
    changes: &Value,
) -> Result<crate::collaboration::Profile> {
    let mut merged =
        serde_json::to_value(profile).map_err(|_| invalid("invalid collaboration profile"))?;
    for (section, fields) in changes
        .as_object()
        .ok_or(invalid("profile changes must be an object"))?
    {
        for (field, proposed) in fields
            .as_object()
            .ok_or(invalid("profile change section must be an object"))?
        {
            merged[section][field] = proposed.clone();
        }
    }
    serde_json::from_value(merged).map_err(|_| invalid("invalid collaboration profile changes"))
}

pub(super) fn context(db: &Connection, owner: &str) -> Result<Value> {
    let profile = crate::collaboration::read(db, owner)?;
    let pending = db.query_row(&format!("SELECT {COLUMNS} FROM profile_suggestions WHERE owner=?1 AND status='pending' ORDER BY rowid DESC LIMIT 1"), [owner], row).optional()?.map(value).transpose()?;
    Ok(json!({"profile":profile,"pending":pending}))
}

/// The caller's chat transaction also persists its response/receipt. This never updates a profile.
pub(super) fn save(
    tx: &rusqlite::Transaction<'_>,
    owner: &str,
    request_id: &str,
    base_revision: u64,
    changes: &Value,
) -> Result<Value> {
    validate_changes(changes)?;
    if request_id.is_empty()
        || request_id.len() > 128
        || !request_id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c))
    {
        return Err(invalid("invalid profile suggestion request_id"));
    }
    if base_revision > i64::MAX as u64 {
        return Err(invalid("invalid profile suggestion revision"));
    }
    if let Some(existing) = tx
        .query_row(
            &format!("SELECT {COLUMNS} FROM profile_suggestions WHERE owner=?1 AND request_id=?2"),
            params![owner, request_id],
            row,
        )
        .optional()?
    {
        let existing = value(existing)?;
        if existing["expected_revision"] != json!(base_revision) || &existing["changes"] != changes
        {
            return Err(Error(
                StatusCode::CONFLICT,
                "request_id already used for different profile changes",
            ));
        }
        return Ok(existing);
    }
    // A refined suggestion replaces the earlier proposal, never its underlying saved profile.
    tx.execute(
        "UPDATE profile_suggestions SET status='superseded' WHERE owner=?1 AND status='pending'",
        [owner],
    )?;
    let id = Uuid::new_v4().to_string();
    tx.execute("INSERT INTO profile_suggestions(id,owner,request_id,expected_revision,changes_json,status) VALUES (?1,?2,?3,?4,?5,'pending')", params![id,owner,request_id,base_revision,changes.to_string()])?;
    value(tx.query_row(
        &format!("SELECT {COLUMNS} FROM profile_suggestions WHERE owner=?1 AND id=?2"),
        params![owner, id],
        row,
    )?)
}
pub(super) async fn list(State(h): State<Hub>, headers: HeaderMap) -> Result<Json<Vec<Value>>> {
    let owner = h.identity(&headers)?;
    let db = h.db()?;
    let mut query = db.prepare(&format!(
        "SELECT {COLUMNS} FROM profile_suggestions WHERE owner=?1 ORDER BY rowid DESC LIMIT 20"
    ))?;
    let rows = query
        .query_map([owner], row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(Json(rows.into_iter().map(value).collect::<Result<_>>()?))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Action {
    action: String,
}
pub(super) async fn action(
    State(h): State<Hub>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(input): Json<Action>,
) -> Result<Json<Value>> {
    let owner = h.identity(&headers)?;
    let target = match input.action.as_str() {
        "confirm" => "confirmed",
        "reject" => "rejected",
        _ => return Err(invalid("unknown profile suggestion action")),
    };
    let mut db = h.db()?;
    let tx = db.transaction()?;
    let mut suggestion = tx
        .query_row(
            &format!("SELECT {COLUMNS} FROM profile_suggestions WHERE owner=?1 AND id=?2"),
            params![owner, id],
            row,
        )
        .optional()?
        .map(value)
        .transpose()?
        .ok_or(Error(
            StatusCode::NOT_FOUND,
            "profile suggestion unavailable",
        ))?;
    if suggestion["status"] == target {
        return Ok(Json(suggestion));
    }
    if suggestion["status"] != "pending" {
        return Err(Error(
            StatusCode::CONFLICT,
            "profile suggestion is no longer pending",
        ));
    }
    if target == "confirmed" {
        let current = crate::collaboration::read(&tx, &owner)?;
        if suggestion["expected_revision"] != json!(current.revision) {
            return Err(Error(
                StatusCode::CONFLICT,
                "个人资料已更新，请让助手根据最新资料重新整理建议。",
            ));
        }
        // Revalidate durable proposal data at the authority boundary.
        validate_changes(&suggestion["changes"])?;
        let mut updated = merge(&current, &suggestion["changes"])?;
        updated.revision = current
            .revision
            .checked_add(1)
            .filter(|r| *r <= i64::MAX as u64)
            .ok_or(Error(
                StatusCode::CONFLICT,
                "collaboration revision exhausted",
            ))?;
        crate::collaboration::write(&tx, &owner, &updated)?;
    }
    tx.execute(
        "UPDATE profile_suggestions SET status=?3 WHERE owner=?1 AND id=?2 AND status='pending'",
        params![owner, id, target],
    )?;
    suggestion["status"] = json!(target);
    tx.commit()?;
    Ok(Json(suggestion))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::Request};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    fn token(id: &str) -> String {
        format!("{id}-{}", "x".repeat(40))
    }
    fn config(path: &str) -> crate::Config {
        crate::Config {
            bind: "127.0.0.1:0".parse().unwrap(),
            database: path.into(),
            runtime: Runtime::Mock,
            users: ["alice", "bob"]
                .map(|id| UserConfig {
                    id: id.into(),
                    token_sha256: format!("{:x}", Sha256::digest(token(id).as_bytes())),
                })
                .into(),
        }
    }
    fn setup() -> Hub {
        Hub::open(config(":memory:")).unwrap()
    }
    async fn request(
        a: &Router,
        who: &str,
        method: &str,
        path: &str,
        body: Value,
    ) -> (StatusCode, Value) {
        let response = a
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(path)
                    .header("authorization", format!("Bearer {}", token(who)))
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
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }
    fn propose(h: &Hub, who: &str, key: &str, revision: u64, changes: Value) -> Value {
        let mut db = h.db().unwrap();
        let tx = db.transaction().unwrap();
        let proposal = save(&tx, who, key, revision, &changes).unwrap();
        tx.commit().unwrap();
        proposal
    }
    async fn act(a: &Router, who: &str, suggestion: &Value, action: &str) -> (StatusCode, Value) {
        request(
            a,
            who,
            "POST",
            &format!(
                "/api/profile-suggestions/{}/actions",
                suggestion["id"].as_str().unwrap()
            ),
            json!({"action":action}),
        )
        .await
    }
    #[tokio::test]
    async fn model_proposal_does_not_change_profile_until_owner_confirms() {
        let h = setup();
        let a = app(h.clone());
        let suggestion = propose(
            &h,
            "alice",
            "turn-one",
            0,
            json!({"public":{"team":"支付团队"},"private":{"work_context":"内部背景"},"permissions":{"auto_reply_public":true}}),
        );
        let current = crate::collaboration::read(&*h.db().unwrap(), "alice").unwrap();
        assert_eq!(current.revision, 0);
        assert!(!current.permissions.auto_reply_public);
        assert!(current.public.team.is_empty());
        assert_eq!(
            request(&a, "bob", "GET", "/api/profile-suggestions", Value::Null)
                .await
                .1,
            json!([])
        );
        assert_eq!(
            act(&a, "bob", &suggestion, "confirm").await.0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            request(
                &a,
                "unknown",
                "GET",
                "/api/profile-suggestions",
                Value::Null
            )
            .await
            .0,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            act(&a, "alice", &suggestion, "confirm").await.1["status"],
            "confirmed"
        );
        let current = crate::collaboration::read(&*h.db().unwrap(), "alice").unwrap();
        assert_eq!(current.revision, 1);
        assert_eq!(current.public.team, "支付团队");
        assert_eq!(current.private.work_context, "内部背景");
        assert!(current.permissions.auto_reply_public);
        assert!(current.permissions.auto_prepare_drafts);
        assert!(current.public.responsibilities.is_empty());
        let public = request(&a, "bob", "GET", "/api/contacts/alice/profile", Value::Null)
            .await
            .1;
        assert!(!public.to_string().contains("内部背景"));
        assert!(!public.to_string().contains("permissions"));
    }
    #[tokio::test]
    async fn repeated_confirmation_and_rejection_are_recoverable_without_extra_writes() {
        let h = setup();
        let a = app(h.clone());
        let suggestion = propose(&h, "alice", "first", 0, json!({"public":{"team":"one"}}));
        let first = act(&a, "alice", &suggestion, "confirm").await.1;
        assert_eq!(act(&a, "alice", &suggestion, "confirm").await.1, first);
        assert_eq!(
            crate::collaboration::read(&*h.db().unwrap(), "alice")
                .unwrap()
                .revision,
            1
        );
        assert_eq!(
            act(&a, "alice", &suggestion, "reject").await.0,
            StatusCode::CONFLICT
        );
        let second = propose(&h, "alice", "second", 1, json!({"public":{"team":"two"}}));
        let rejected = act(&a, "alice", &second, "reject").await.1;
        assert_eq!(act(&a, "alice", &second, "reject").await.1, rejected);
        assert_eq!(rejected["status"], "rejected");
        assert_eq!(
            act(&a, "alice", &second, "confirm").await.0,
            StatusCode::CONFLICT
        );
        assert_eq!(
            crate::collaboration::read(&*h.db().unwrap(), "alice")
                .unwrap()
                .public
                .team,
            "one"
        );
        assert!(context(&*h.db().unwrap(), "alice").unwrap()["pending"].is_null());
    }
    #[tokio::test]
    async fn stale_confirmation_preserves_pending_and_rejection_still_works() {
        let h = setup();
        let a = app(h.clone());
        let suggestion = propose(&h, "alice", "one", 0, json!({"public":{"team":"stale"}}));
        let mut current = crate::collaboration::Profile::default();
        current.revision = 1;
        current.public.team = "newer".into();
        crate::collaboration::write(&*h.db().unwrap(), "alice", &current).unwrap();
        assert_eq!(
            act(&a, "alice", &suggestion, "confirm").await.0,
            StatusCode::CONFLICT
        );
        assert_eq!(
            context(&*h.db().unwrap(), "alice").unwrap()["pending"]["status"],
            "pending"
        );
        assert_eq!(
            crate::collaboration::read(&*h.db().unwrap(), "alice")
                .unwrap()
                .public
                .team,
            "newer"
        );
        assert_eq!(
            act(&a, "alice", &suggestion, "reject").await.0,
            StatusCode::OK
        );
        assert_eq!(
            act(&a, "alice", &suggestion, "reject").await.1["status"],
            "rejected"
        );
    }
    #[tokio::test]
    async fn refined_proposals_supersede_only_owners_pending_and_replays_keep_identity() {
        let h = setup();
        let a = app(h.clone());
        let first = propose(&h, "alice", "first", 0, json!({"public":{"team":"one"}}));
        let bob = propose(&h, "bob", "first", 0, json!({"public":{"team":"bob"}}));
        let second = propose(&h, "alice", "second", 0, json!({"public":{"team":"two"}}));
        assert_eq!(
            act(&a, "alice", &first, "confirm").await.0,
            StatusCode::CONFLICT
        );
        assert_eq!(context(&*h.db().unwrap(), "bob").unwrap()["pending"], bob);
        assert_eq!(
            context(&*h.db().unwrap(), "alice").unwrap()["pending"],
            second
        );
        assert_eq!(
            propose(&h, "alice", "second", 0, json!({"public":{"team":"two"}})),
            second
        );
        let replay = propose(&h, "alice", "first", 0, json!({"public":{"team":"one"}}));
        assert_eq!(replay["status"], "superseded");
        let mut db = h.db().unwrap();
        let tx = db.transaction().unwrap();
        for (revision, changes) in [
            (0, json!({"public":{"team":"different"}})),
            (1, json!({"public":{"team":"two"}})),
        ] {
            assert!(save(&tx, "alice", "second", revision, &changes).is_err());
        }
        tx.commit().unwrap();
        drop(db);
        assert_eq!(
            request(&a, "alice", "GET", "/api/profile-suggestions", Value::Null)
                .await
                .1
                .as_array()
                .unwrap()
                .len(),
            2
        );
    }
    #[test]
    fn invalid_fields_types_and_lengths_cannot_supersede_or_save_anything() {
        let h = setup();
        let prior = propose(
            &h,
            "alice",
            "first",
            0,
            json!({"private":{"response_preferences":"简洁"}}),
        );
        for changes in [
            json!({}),
            json!({"public":{}}),
            json!({"public":null}),
            json!([]),
            json!({"owner":"bob"}),
            json!({"public":{"unknown":"x"}}),
            json!({"private":{"work_context":null}}),
            json!({"public":{"team":true}}),
            json!({"permissions":{"auto_reply_public":"true"}}),
            json!({"permissions":{"auto_execute":true}}),
            json!({"public":{"team":"字".repeat(121)}}),
            json!({"private":{"work_context":"字".repeat(2001)}}),
            json!({"private":{"response_preferences":"a\u{0000}"}}),
        ] {
            let mut db = h.db().unwrap();
            let tx = db.transaction().unwrap();
            assert!(
                save(&tx, "alice", "invalid", 0, &changes).is_err(),
                "{changes}"
            );
            tx.commit().unwrap();
        }
        assert_eq!(
            context(&*h.db().unwrap(), "alice").unwrap()["pending"],
            prior
        );
        assert_eq!(
            crate::collaboration::read(&*h.db().unwrap(), "alice")
                .unwrap()
                .revision,
            0
        );
        assert!(validate_changes(
            &json!({"public":{"team":""},"permissions":{"auto_prepare_drafts":false}})
        )
        .is_ok());
    }
    #[tokio::test]
    async fn actions_cannot_change_payload_and_reads_are_capped() {
        let h = setup();
        let a = app(h.clone());
        let mut latest = Value::Null;
        for i in 0..25 {
            latest = propose(
                &h,
                "alice",
                &format!("turn-{i}"),
                0,
                json!({"public":{"team":i.to_string()}}),
            );
        }
        let results = request(&a, "alice", "GET", "/api/profile-suggestions", Value::Null)
            .await
            .1;
        assert_eq!(results.as_array().unwrap().len(), 20);
        assert_eq!(results[0], latest);
        let path = format!(
            "/api/profile-suggestions/{}/actions",
            latest["id"].as_str().unwrap()
        );
        for extra in [
            json!({"action":"confirm","changes":{}}),
            json!({"action":"confirm","owner":"bob"}),
            json!({"action":"confirm","expected_revision":0}),
        ] {
            assert_eq!(
                request(&a, "alice", "POST", &path, extra).await.0,
                StatusCode::UNPROCESSABLE_ENTITY
            );
        }
        assert_eq!(
            act(&a, "alice", &latest, "save").await.0,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            request(
                &a,
                "alice",
                "POST",
                "/api/profile-suggestions",
                json!({"changes":{"public":{"team":"x"}}})
            )
            .await
            .0,
            StatusCode::METHOD_NOT_ALLOWED
        );
        assert_eq!(
            crate::collaboration::read(&*h.db().unwrap(), "alice")
                .unwrap()
                .revision,
            0
        );
    }
    #[tokio::test]
    async fn confirmed_receipts_survive_restart_without_applying_twice() {
        let dir =
            std::env::temp_dir().join(format!("agenct-profile-suggestion-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("hub.db");
        let h = Hub::open(config(file.to_str().unwrap())).unwrap();
        let a = app(h.clone());
        let suggestion = propose(
            &h,
            "alice",
            "one",
            0,
            json!({"private":{"response_preferences":"先给结论"}}),
        );
        assert_eq!(
            act(&a, "alice", &suggestion, "confirm").await.0,
            StatusCode::OK
        );
        drop(a);
        drop(h);
        let h = Hub::open(config(file.to_str().unwrap())).unwrap();
        let a = app(h.clone());
        assert_eq!(
            act(&a, "alice", &suggestion, "confirm").await.1["status"],
            "confirmed"
        );
        let current = crate::collaboration::read(&*h.db().unwrap(), "alice").unwrap();
        assert_eq!(current.revision, 1);
        assert_eq!(current.private.response_preferences, "先给结论");
        drop(a);
        drop(h);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
