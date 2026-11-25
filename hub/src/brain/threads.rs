//! Implicit backend thread lifecycle management for context and CLI session continuity.
use crate::{config::now_unix, config::truncate_bytes, Hub, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub(super) const THREAD_IDLE_ARCHIVE_SECS: i64 = 48 * 3600;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ThreadView {
    pub id: String,
    pub title: String,
    pub status: String,
    pub summary: String,
    pub node_id: Option<String>,
    pub has_session: bool,
    pub turn_count: i64,
    pub updated_at: i64,
}

pub fn list_active_threads_sync(conn: &Connection, user_id: &str) -> Result<Vec<ThreadView>> {
    let cutoff = now_unix().saturating_sub(THREAD_IDLE_ARCHIVE_SECS);
    let mut stmt = conn.prepare(
        "SELECT id, title, status, summary, node_id, node_session_id, turn_count, updated_at
         FROM threads
         WHERE user_id = ?1 AND status = 'active' AND updated_at >= ?2
         ORDER BY updated_at DESC LIMIT 6",
    )?;
    let rows = stmt.query_map(params![user_id, cutoff], |r| {
        let sid: Option<String> = r.get(5)?;
        Ok(ThreadView {
            id: r.get(0)?,
            title: r.get(1)?,
            status: r.get(2)?,
            summary: r.get(3)?,
            node_id: r.get(4)?,
            has_session: sid.as_deref().is_some_and(|s| !s.is_empty()),
            turn_count: r.get(6)?,
            updated_at: r.get(7)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

pub fn list_threads_sync(conn: &Connection, user_id: &str) -> Result<Vec<ThreadView>> {
    let mut stmt = conn.prepare(
        "SELECT id, title, status, summary, node_id, node_session_id, turn_count, updated_at
         FROM threads
         WHERE user_id = ?1
         ORDER BY CASE status WHEN 'active' THEN 0 ELSE 1 END, updated_at DESC
         LIMIT 30",
    )?;
    let rows = stmt.query_map([user_id], |r| {
        let sid: Option<String> = r.get(5)?;
        Ok(ThreadView {
            id: r.get(0)?,
            title: r.get(1)?,
            status: r.get(2)?,
            summary: r.get(3)?,
            node_id: r.get(4)?,
            has_session: sid.as_deref().is_some_and(|s| !s.is_empty()),
            turn_count: r.get(6)?,
            updated_at: r.get(7)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

pub(super) async fn create_thread(hub: &Hub, user_id: &str, title: &str) -> Result<String> {
    let uid = user_id.to_owned();
    let clean_title = truncate_bytes(title.lines().next().unwrap_or("新话题").trim(), 80);
    let final_title = if clean_title.is_empty() {
        "新话题".to_owned()
    } else {
        clean_title
    };
    let tid = format!("thr_{}", &Uuid::new_v4().simple().to_string()[..8]);
    let tid_db = tid.clone();
    hub.db
        .write(move |conn| {
            conn.execute(
                "INSERT INTO threads(id, user_id, title, status, summary, turn_count, updated_at)
                 VALUES (?1, ?2, ?3, 'active', '', 0, ?4)",
                params![tid_db, uid, final_title, now_unix()],
            )?;
            Ok(())
        })
        .await?;
    Ok(tid)
}

pub(super) async fn resolve_or_create_thread(
    hub: &Hub,
    user_id: &str,
    raw_thread_id: Option<&str>,
    thread_title: Option<&str>,
    fallback_title: &str,
) -> Result<Option<String>> {
    let Some(candidate) = raw_thread_id
        .map(str::trim)
        .filter(|s| !s.is_empty() && !s.eq_ignore_ascii_case("null"))
    else {
        return Ok(None);
    };
    if candidate.eq_ignore_ascii_case("new") {
        let title = thread_title
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(fallback_title);
        return Ok(Some(create_thread(hub, user_id, title).await?));
    }
    let uid = user_id.to_owned();
    let cid = candidate.to_owned();
    let found: Option<String> = hub
        .db
        .write(move |conn| {
            let exists: Option<String> = conn
                .query_row(
                    "SELECT id FROM threads WHERE id = ?1 AND user_id = ?2",
                    params![cid, uid],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(ref id) = exists {
                conn.execute(
                    "UPDATE threads SET status = 'active', updated_at = ?3 WHERE id = ?1 AND user_id = ?2",
                    params![id, uid, now_unix()],
                )?;
            }
            Ok(exists)
        })
        .await?;
    Ok(found)
}

pub(super) async fn archive_thread(hub: &Hub, user_id: &str, thread_id: &str) -> Result<()> {
    let uid = user_id.to_owned();
    let tid = thread_id.to_owned();
    hub.db
        .write(move |conn| {
            conn.execute(
                "UPDATE threads SET status = 'archived', updated_at = ?3 WHERE id = ?1 AND user_id = ?2",
                params![tid, uid, now_unix()],
            )?;
            Ok(())
        })
        .await?;
    Ok(())
}
