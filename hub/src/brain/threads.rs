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
    pub ledger_id: String,
    pub node_id: Option<String>,
    pub has_session: bool,
    pub turn_count: i64,
    pub updated_at: i64,
}

pub fn list_active_threads_sync(conn: &Connection, user_id: &str) -> Result<Vec<ThreadView>> {
    let cutoff = now_unix().saturating_sub(THREAD_IDLE_ARCHIVE_SECS);
    let mut stmt = conn.prepare(
        "SELECT id, title, status, summary, ledger_id, node_id, node_session_id, turn_count, updated_at
         FROM threads
         WHERE user_id = ?1 AND status = 'active' AND updated_at >= ?2
         ORDER BY updated_at DESC LIMIT 6",
    )?;
    let rows = stmt.query_map(params![user_id, cutoff], |r| {
        let sid: Option<String> = r.get(6)?;
        Ok(ThreadView {
            id: r.get(0)?,
            title: r.get(1)?,
            status: r.get(2)?,
            summary: r.get(3)?,
            ledger_id: r.get(4)?,
            node_id: r.get(5)?,
            has_session: sid.as_deref().is_some_and(|s| !s.is_empty()),
            turn_count: r.get(7)?,
            updated_at: r.get(8)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

pub fn list_threads_sync(conn: &Connection, user_id: &str) -> Result<Vec<ThreadView>> {
    let mut stmt = conn.prepare(
        "SELECT id, title, status, summary, ledger_id, node_id, node_session_id, turn_count, updated_at
         FROM threads
         WHERE user_id = ?1
         ORDER BY CASE status WHEN 'active' THEN 0 ELSE 1 END, updated_at DESC
         LIMIT 30",
    )?;
    let rows = stmt.query_map([user_id], |r| {
        let sid: Option<String> = r.get(6)?;
        Ok(ThreadView {
            id: r.get(0)?,
            title: r.get(1)?,
            status: r.get(2)?,
            summary: r.get(3)?,
            ledger_id: r.get(4)?,
            node_id: r.get(5)?,
            has_session: sid.as_deref().is_some_and(|s| !s.is_empty()),
            turn_count: r.get(7)?,
            updated_at: r.get(8)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

fn owned_ledger_id_sync(conn: &Connection, user_id: &str, raw: Option<&str>) -> Result<String> {
    let Some(cand) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(String::new());
    };
    let found: Option<String> = conn
        .query_row(
            "SELECT id FROM ledgers WHERE id = ?1 AND user_id = ?2",
            params![cand, user_id],
            |r| r.get(0),
        )
        .optional()?;
    Ok(found.unwrap_or_default())
}

pub(super) async fn create_thread(
    hub: &Hub,
    user_id: &str,
    title: &str,
    ledger_id: Option<&str>,
) -> Result<String> {
    let uid = user_id.to_owned();
    let clean_title = truncate_bytes(title.lines().next().unwrap_or("新话题").trim(), 80);
    let final_title = if clean_title.is_empty() {
        "新话题".to_owned()
    } else {
        clean_title
    };
    let tid = format!("thr_{}", &Uuid::new_v4().simple().to_string()[..8]);
    let tid_db = tid.clone();
    let lid_opt = ledger_id.map(str::to_owned);
    hub.db
        .write(move |conn| {
            let valid_ledger = owned_ledger_id_sync(conn, &uid, lid_opt.as_deref())?;
            conn.execute(
                "INSERT INTO threads(id, user_id, title, status, summary, ledger_id, turn_count, updated_at)
                 VALUES (?1, ?2, ?3, 'active', '', ?4, 0, ?5)",
                params![tid_db, uid, final_title, valid_ledger, now_unix()],
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
    ledger_id: Option<&str>,
) -> Result<Option<String>> {
    let Some(candidate) = raw_thread_id.map(str::trim).filter(|s| {
        !s.is_empty() && !s.eq_ignore_ascii_case("null") && !s.eq_ignore_ascii_case("none")
    }) else {
        return Ok(None);
    };
    if candidate.eq_ignore_ascii_case("new") {
        let title = thread_title
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(fallback_title);
        return Ok(Some(create_thread(hub, user_id, title, ledger_id).await?));
    }
    let uid = user_id.to_owned();
    let cid = candidate.to_owned();
    let lid_opt = ledger_id.map(str::to_owned);
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
                let valid_ledger = owned_ledger_id_sync(conn, &uid, lid_opt.as_deref())?;
                conn.execute(
                    "UPDATE threads
                     SET status = 'active',
                         ledger_id = CASE WHEN ledger_id = '' AND ?4 != '' THEN ?4 ELSE ledger_id END,
                         updated_at = ?3
                     WHERE id = ?1 AND user_id = ?2",
                    params![id, uid, now_unix(), valid_ledger],
                )?;
            }
            Ok(exists)
        })
        .await?;
    Ok(found)
}

pub(super) async fn update_thread_meta(
    hub: &Hub,
    user_id: &str,
    thread_id: &str,
    title: Option<&str>,
    ledger_id: Option<&str>,
) -> Result<bool> {
    let uid = user_id.to_owned();
    let tid = thread_id.trim().to_owned();
    if tid.is_empty() {
        return Ok(false);
    }
    let title_opt = title
        .map(|s| truncate_bytes(s.lines().next().unwrap_or("").trim(), 80))
        .filter(|s| !s.is_empty());
    let ledger_in = ledger_id.map(|s| s.trim().to_owned());
    hub.db
        .write(move |conn| {
            let cur: Option<(String, String)> = conn
                .query_row(
                    "SELECT title, ledger_id FROM threads WHERE id = ?1 AND user_id = ?2",
                    params![tid, uid],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            let Some((cur_title, cur_ledger)) = cur else {
                return Ok(false);
            };
            let next_title = title_opt.unwrap_or(cur_title);
            let next_ledger = match ledger_in.as_deref() {
                Some("") => String::new(),
                Some(raw) => owned_ledger_id_sync(conn, &uid, Some(raw))?,
                None => cur_ledger,
            };
            let changed = conn.execute(
                "UPDATE threads SET title = ?3, ledger_id = ?4, updated_at = ?5
                 WHERE id = ?1 AND user_id = ?2",
                params![tid, uid, next_title, next_ledger, now_unix()],
            )?;
            Ok(changed > 0)
        })
        .await
}

pub(super) async fn delete_empty_thread(hub: &Hub, user_id: &str, thread_id: &str) -> Result<()> {
    let uid = user_id.to_owned();
    let tid = thread_id.to_owned();
    hub.db
        .write(move |conn| {
            conn.execute(
                "DELETE FROM threads
                 WHERE id = ?1 AND user_id = ?2 AND turn_count = 0
                   AND NOT EXISTS (SELECT 1 FROM messages m WHERE m.thread_id = threads.id)
                   AND NOT EXISTS (SELECT 1 FROM tasks t WHERE t.thread_id = threads.id)",
                params![tid, uid],
            )?;
            Ok(())
        })
        .await?;
    Ok(())
}

/// Archives idle active threads (>48h without pending/running tasks) and prunes >10m empty shells.
pub async fn archive_stale_threads(hub: &Hub) {
    let now = now_unix();
    let archive_cutoff = now.saturating_sub(THREAD_IDLE_ARCHIVE_SECS);
    let empty_cutoff = now.saturating_sub(600);
    let _ = hub
        .db
        .write(move |conn| {
            conn.execute(
                "UPDATE threads
                 SET status = 'archived'
                 WHERE status = 'active'
                   AND updated_at < ?1
                   AND NOT EXISTS (
                       SELECT 1 FROM tasks t
                       WHERE t.thread_id = threads.id AND t.status IN ('pending', 'running')
                   )",
                [archive_cutoff],
            )?;
            conn.execute(
                "DELETE FROM threads
                 WHERE turn_count = 0
                   AND updated_at < ?1
                   AND NOT EXISTS (SELECT 1 FROM messages m WHERE m.thread_id = threads.id)
                   AND NOT EXISTS (SELECT 1 FROM tasks t WHERE t.thread_id = threads.id)",
                [empty_cutoff],
            )?;
            Ok(())
        })
        .await;
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
