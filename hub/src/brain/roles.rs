//! Chat-managed agent squad roles (`planner`, `executor`, `reviewer`, etc.).
use crate::{
    auth::AuthUser, config::now_unix, db::record_audit, denied, invalid, Error, Hub, Result,
};
use axum::{
    extract::{Path, State},
    Json,
};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub const MAX_ROLES_PER_USER: i64 = 32;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgentRole {
    pub name: String,
    pub description: String,
    pub agent: String,
    pub model: String,
    pub effort: String,
    pub sandbox: String,
    pub node_id: String,
    pub prompt: String,
    pub updated_at: i64,
}

/// Validates and normalizes a squad role identifier (`1..32` chars, not starting with `-`).
pub fn normalize_role_name(raw: &str) -> Option<String> {
    let trimmed = raw.trim().to_lowercase();
    if !trimmed.is_empty()
        && trimmed.chars().count() <= 32
        && trimmed.len() <= 64
        && !trimmed.starts_with('-')
        && trimmed
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '-' | '_'))
    {
        Some(trimmed)
    } else {
        None
    }
}

fn row_to_role(r: &rusqlite::Row<'_>) -> rusqlite::Result<AgentRole> {
    Ok(AgentRole {
        name: r.get(0)?,
        description: r.get(1)?,
        agent: r.get(2)?,
        model: r.get(3)?,
        effort: r.get(4)?,
        sandbox: r.get(5)?,
        node_id: r.get(6)?,
        prompt: r.get(7)?,
        updated_at: r.get(8)?,
    })
}

pub fn list_roles_sync(conn: &Connection, user_id: &str) -> Result<Vec<AgentRole>> {
    let mut stmt = conn.prepare(
        "SELECT name, description, agent, model, effort, sandbox, node_id, prompt, updated_at
         FROM agent_roles WHERE user_id = ?1 ORDER BY name ASC LIMIT 32",
    )?;
    let rows = stmt.query_map([user_id], row_to_role)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

pub fn get_role_sync(conn: &Connection, user_id: &str, name: &str) -> Result<Option<AgentRole>> {
    let Some(norm) = normalize_role_name(name) else {
        return Ok(None);
    };
    Ok(conn
        .query_row(
            "SELECT name, description, agent, model, effort, sandbox, node_id, prompt, updated_at
             FROM agent_roles WHERE user_id = ?1 AND name = ?2",
            params![user_id, norm],
            row_to_role,
        )
        .optional()?)
}

pub fn upsert_role_sync(conn: &Connection, user_id: &str, role: &AgentRole) -> Result<()> {
    let existing: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM agent_roles WHERE user_id = ?1 AND name = ?2)",
        params![user_id, role.name],
        |r| r.get(0),
    )?;
    if !existing {
        let count: i64 = conn.query_row(
            "SELECT count(*) FROM agent_roles WHERE user_id = ?1",
            [user_id],
            |r| r.get(0),
        )?;
        if count >= MAX_ROLES_PER_USER {
            return Err(Error(
                axum::http::StatusCode::BAD_REQUEST,
                "maximum number of roles reached (32)",
            ));
        }
    }
    let now = now_unix();
    conn.execute(
        "INSERT INTO agent_roles(user_id, name, description, agent, model, effort, sandbox, node_id, prompt, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
         ON CONFLICT(user_id, name) DO UPDATE SET
           description=excluded.description,
           agent=excluded.agent,
           model=excluded.model,
           effort=excluded.effort,
           sandbox=excluded.sandbox,
           node_id=excluded.node_id,
           prompt=excluded.prompt,
           updated_at=excluded.updated_at",
        params![
            user_id,
            role.name,
            role.description,
            role.agent,
            role.model,
            role.effort,
            role.sandbox,
            role.node_id,
            role.prompt,
            now,
        ],
    )?;
    record_audit(
        conn,
        user_id,
        "manage_role",
        "ok",
        &format!("Saved role {}", role.name),
        &format!(
            "agent={} model={} effort={} sandbox={}",
            role.agent, role.model, role.effort, role.sandbox
        ),
    )?;
    Ok(())
}

pub fn delete_role_sync(conn: &Connection, user_id: &str, name: &str) -> Result<bool> {
    let changed = conn.execute(
        "DELETE FROM agent_roles WHERE user_id = ?1 AND name = ?2",
        params![user_id, name],
    )?;
    if changed > 0 {
        record_audit(
            conn,
            user_id,
            "manage_role",
            "ok",
            &format!("Deleted role {name}"),
            "",
        )?;
        Ok(true)
    } else {
        Ok(false)
    }
}

pub async fn list_roles(
    State(hub): State<Hub>,
    AuthUser(user): AuthUser,
) -> Result<Json<Vec<AgentRole>>> {
    let uid = user.id;
    let roles = hub.db.read(move |conn| list_roles_sync(conn, &uid)).await?;
    Ok(Json(roles))
}

pub async fn delete_role(
    State(hub): State<Hub>,
    AuthUser(user): AuthUser,
    Path(name): Path<String>,
) -> Result<Json<Value>> {
    let norm = normalize_role_name(&name).ok_or_else(|| invalid("invalid role name"))?;
    let uid = user.id.clone();
    let norm_db = norm.clone();
    let deleted = hub
        .db
        .write(move |conn| delete_role_sync(conn, &uid, &norm_db))
        .await?;
    if !deleted {
        return Err(denied());
    }
    hub.events
        .emit(user.id, "brain", json!({"updated": "roles"}));
    Ok(Json(json!({"ok": true, "name": norm})))
}
