//! Background rolling context compaction, Active Memory pruning, and idle thread archival.
use super::{
    read_compaction_state_sync, read_profile_sync,
    threads::{list_active_threads_sync, THREAD_IDLE_ARCHIVE_SECS},
};
use crate::{
    config::{now_unix, truncate_bytes},
    db::record_audit,
    Hub, Result,
};
use rusqlite::params;
use serde::Deserialize;
use serde_json::json;

pub(super) const COMPACT_KEEP_RECENT: usize = 8;
pub(super) const COMPACT_MIN_BATCH: usize = 10;
const COMPACT_MAX_BATCH: usize = 24;

#[derive(Deserialize)]
struct CompactionOutput {
    rolling_summary: String,
    #[serde(default)]
    memory_md: Option<String>,
    #[serde(default)]
    archive_thread_ids: Vec<String>,
}

const COMPACTION_SYSTEM_PROMPT: &str = r#"CONTEXT_COMPACTION_TASK: 你是个人智能体的后台上下文压缩与 Active Memory 剪枝器。
任务：
1. 把「旧历史摘要」与「待压缩对话批次」合并成一份紧凑的 `rolling_summary`（中文，不超过 1200 字）：
   - 保留：未完结事项、明确做出的决定、正在排查的项目上下文。
   - 剔除：寒暄、已经过期的一次性查询输出（如旧的 tmux 列表或临时日志）、重复过程。
2. 审视当前 `MEMORY`：
   - 仅当待压缩对话中出现新的长期偏好/规矩/环境事实，或 `MEMORY` 中有明显重复/过时条目时，输出整理后的完整 `memory_md`；否则填 `null`。
   - **主动剔除已过时或被后续推翻的系统/环境限制结论**（例如旧的支持引擎清单、已修复的沙箱/socket/cli 报错，以免与实时系统能力冲突）。
3. 审视活跃线程列表：把已经明确结束或过时的线程 ID 放进 `archive_thread_ids`（无则 `[]`）。

严格输出合法 JSON（勿加 Markdown 代码块）：
{"rolling_summary":"...","memory_md":null,"archive_thread_ids":[]}"#;

pub(super) fn maybe_spawn_compaction(hub: Hub, user_id: String) {
    if !hub.try_begin_compaction(&user_id) {
        return;
    }
    tokio::spawn(async move {
        let _ = compact_user_context(&hub, &user_id).await;
        hub.end_compaction(&user_id);
    });
}

/// Incrementally compacts older completed messages into `agent_profiles.rolling_summary`,
/// prunes stale Active Memory entries, and archives finished/idle implicit threads.
pub async fn compact_user_context(hub: &Hub, user_id: &str) -> Result<bool> {
    let uid = user_id.to_owned();
    let candidate = hub
        .db
        .read(move |conn| {
            let profile = read_profile_sync(conn, &uid)?;
            let (old_summary, compacted_id) = read_compaction_state_sync(conn, &uid)?;
            let threads = list_active_threads_sync(conn, &uid)?;

            let mut stmt = conn.prepare(
                "SELECT id, role, content, thread_id FROM messages
                 WHERE user_id = ?1 AND status = 'complete' AND id > ?2
                 ORDER BY id ASC",
            )?;
            let uncompacted: Vec<(i64, String, String, Option<String>)> = stmt
                .query_map(params![uid, compacted_id], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;

            if uncompacted.len() < COMPACT_KEEP_RECENT + COMPACT_MIN_BATCH {
                return Ok(None);
            }
            let eligible = uncompacted.len() - COMPACT_KEEP_RECENT;
            let take_n = eligible.min(COMPACT_MAX_BATCH);
            let batch: Vec<(i64, String, String, Option<String>)> =
                uncompacted.into_iter().take(take_n).collect();
            let new_watermark = batch.last().map(|r| r.0).unwrap_or(compacted_id);

            Ok(Some((profile, old_summary, threads, batch, new_watermark)))
        })
        .await?;

    let Some((profile, old_summary, threads, batch, new_watermark)) = candidate else {
        // Even if message batch isn't large enough yet, quietly archive stale threads >48h idle.
        let uid = user_id.to_owned();
        let cutoff = now_unix().saturating_sub(THREAD_IDLE_ARCHIVE_SECS);
        let _ = hub
            .db
            .write(move |conn| {
                conn.execute(
                    "UPDATE threads SET status = 'archived'
                     WHERE user_id = ?1 AND status = 'active' AND updated_at < ?2",
                    params![uid, cutoff],
                )?;
                Ok(())
            })
            .await;
        return Ok(false);
    };

    let transcript = batch
        .iter()
        .map(|(_, role, content, thr_id)| {
            let tag = thr_id
                .as_deref()
                .map(|t| format!("[{t}] "))
                .unwrap_or_default();
            format!("{role}: {tag}{}", truncate_bytes(content.trim(), 480))
        })
        .collect::<Vec<_>>()
        .join("\n");

    let threads_txt = if threads.is_empty() {
        "（无）".into()
    } else {
        threads
            .iter()
            .map(|t| {
                format!(
                    "- {} | {} | {}",
                    t.id,
                    t.title,
                    truncate_bytes(&t.summary, 120)
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };

    let payload = format!(
        "【旧历史摘要】\n{}\n\n【当前 MEMORY】\n{}\n\n【活跃线程】\n{}\n\n【待压缩对话批次（共 {} 条）】\n{}",
        if old_summary.trim().is_empty() { "（空）" } else { old_summary.trim() },
        truncate_bytes(&profile.memory_md, 3000),
        threads_txt,
        batch.len(),
        truncate_bytes(&transcript, 8000),
    );

    let raw = crate::models::resolve(hub, user_id)
        .await?
        .brain
        .complete(COMPACTION_SYSTEM_PROMPT, &[], &payload, true)
        .await?;

    let trimmed = raw.trim();
    let parsed: Option<CompactionOutput> = trimmed
        .find('{')
        .zip(trimmed.rfind('}'))
        .and_then(|(s, e)| serde_json::from_str::<CompactionOutput>(&trimmed[s..=e]).ok());

    let Some(out) = parsed else {
        return Ok(false);
    };
    let clean_summary = truncate_bytes(out.rolling_summary.trim(), 2000);
    if clean_summary.is_empty() {
        return Ok(false);
    }
    let pruned_memory = out
        .memory_md
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| truncate_bytes(s, 8000));
    let archive_ids = out.archive_thread_ids;

    let uid = user_id.to_owned();
    let default_name = profile.name.clone();
    let default_soul = profile.soul_md.clone();
    let fallback_mem = profile.memory_md.clone();
    let memory_changed = pruned_memory.is_some();

    hub.db
        .write(move |conn| {
            let now = now_unix();
            let final_mem = pruned_memory.unwrap_or(fallback_mem);
            conn.execute(
                "INSERT INTO agent_profiles(user_id, name, soul_md, memory_md, rolling_summary, compacted_msg_id, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(user_id) DO UPDATE SET
                   memory_md = excluded.memory_md,
                   rolling_summary = excluded.rolling_summary,
                   compacted_msg_id = MAX(agent_profiles.compacted_msg_id, excluded.compacted_msg_id),
                   updated_at = excluded.updated_at",
                params![
                    uid,
                    default_name,
                    default_soul,
                    final_mem,
                    clean_summary,
                    new_watermark,
                    now
                ],
            )?;
            for tid in archive_ids {
                let t = tid.trim();
                if !t.is_empty() {
                    conn.execute(
                        "UPDATE threads SET status = 'archived', updated_at = ?3 WHERE id = ?1 AND user_id = ?2",
                        params![t, uid, now],
                    )?;
                }
            }
            let cutoff = now.saturating_sub(THREAD_IDLE_ARCHIVE_SECS);
            conn.execute(
                "UPDATE threads SET status = 'archived'
                 WHERE user_id = ?1 AND status = 'active' AND updated_at < ?2",
                params![uid, cutoff],
            )?;
            record_audit(
                conn,
                &uid,
                "context_compact",
                "ok",
                "自动压缩上下文摘要",
                &format!("watermark={new_watermark}"),
            )?;
            Ok(())
        })
        .await?;

    if memory_changed {
        hub.events
            .emit(user_id.to_owned(), "brain", json!({"updated": "profile"}));
    }
    Ok(true)
}
