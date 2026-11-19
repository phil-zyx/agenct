//! Owner-scoped local PC nodes and manual task routing for hybrid cloud/PC workflows.
use crate::{denied, invalid, task_context, tasks, Error, Hub, Result};
use axum::{
    extract::{Path, State},
    http::{header, HeaderMap, StatusCode},
    response::IntoResponse,
    Json,
};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use uuid::Uuid;

const MAX_NODES_PER_USER: i64 = 16;
pub(super) const ONLINE_WINDOW_SECONDS: i64 = 45;

pub(super) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS user_nodes (
    owner TEXT NOT NULL REFERENCES sessions(owner),
    id TEXT NOT NULL,
    name TEXT NOT NULL,
    capabilities TEXT NOT NULL DEFAULT '',
    token_hash TEXT NOT NULL UNIQUE,
    last_seen_unix INTEGER,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY(owner, id)
);
CREATE TABLE IF NOT EXISTS node_assignments (
    task_id TEXT PRIMARY KEY REFERENCES tasks(id),
    owner TEXT NOT NULL REFERENCES sessions(owner),
    node_id TEXT NOT NULL,
    task_revision INTEGER NOT NULL,
    instruction TEXT NOT NULL DEFAULT '',
    status TEXT NOT NULL CHECK(status IN ('queued','running','completed','failed')),
    draft_request_id TEXT,
    updated_at INTEGER NOT NULL,
    FOREIGN KEY(owner, node_id) REFERENCES user_nodes(owner, id) ON DELETE CASCADE
);";

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct NodeLiveProgress {
    pub node_id: String,
    pub task_revision: i64,
    pub stage: String,
    pub log_tail: String,
    pub elapsed_secs: u64,
    pub updated_at: i64,
}

pub(crate) fn clear_live_progress(h: &Hub, task_id: &str) {
    if let Ok(mut guard) = h.node_progress.lock() {
        guard.remove(task_id);
    }
}

pub(crate) fn attach_live_progress(
    h: &Hub,
    task_id: &str,
    task_revision: i64,
    assigned: &mut Value,
) {
    if assigned["status"] != "running" {
        return;
    }
    if let Ok(guard) = h.node_progress.lock() {
        if let Some(p) = guard
            .get(task_id)
            .filter(|p| p.task_revision == task_revision)
        {
            assigned["progress"] = json!({
                "stage": p.stage,
                "log_tail": p.log_tail,
                "elapsed_secs": p.elapsed_secs,
                "updated_at": p.updated_at,
            });
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CreateNode {
    #[serde(default)]
    id: String,
    name: String,
    #[serde(default)]
    capabilities: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct AssignTask {
    node_id: Option<String>,
    expected_revision: i64,
    #[serde(default)]
    instruction: String,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct DraftAttachment {
    name: String,
    mime_type: String,
    data_base64: String,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SubmitDraft {
    #[serde(default)]
    lease_token: String,
    request_id: String,
    expected_revision: i64,
    status: String,
    #[serde(default)]
    content: String,
    #[serde(default)]
    attachments: Vec<DraftAttachment>,
}

pub(super) fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn valid_node_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 32
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn valid_key(value: &str, max_len: usize) -> bool {
    !value.is_empty()
        && value.len() <= max_len
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn valid_text(value: &str, max_chars: usize) -> bool {
    value.chars().count() <= max_chars && !value.chars().any(char::is_control)
}

fn valid_instruction(value: &str) -> bool {
    value.len() <= 4000
        && !value
            .chars()
            .any(|c| c.is_control() && c != '\n' && c != '\t')
}

pub(super) fn truncate_bytes(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_owned();
    }
    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

fn host_from_capabilities(capabilities: &str) -> Option<&str> {
    capabilities
        .split(|c: char| c.is_whitespace() || c == '|')
        .find_map(|part| part.strip_prefix("host:"))
        .filter(|h| !h.is_empty())
}

fn strip_alias<'a>(text: &'a str, alias: &str, require_sep: bool) -> Option<&'a str> {
    if alias.is_empty() || alias.len() > text.len() {
        return None;
    }
    let head = text.get(..alias.len())?;
    let matched = if alias.is_ascii() {
        head.eq_ignore_ascii_case(alias)
    } else {
        head == alias
    };
    if !matched {
        return None;
    }
    let rest = text.get(alias.len()..)?;
    if require_sep {
        let first = rest.chars().next()?;
        if !matches!(first, ':' | '：' | ',' | '，' | ' ' | '\t') {
            return None;
        }
    } else if alias.ends_with(|c: char| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        if let Some(first) = rest.chars().next() {
            if first.is_ascii_alphanumeric() || first == '-' || first == '_' {
                return None;
            }
        }
    }
    Some(rest.trim_start_matches([':', '：', ',', '，', ' ', '\t']))
}

/// Resolves a caller-owned PC node by ID, display name, or reported `host:<name>`.
pub(super) fn resolve_user_node(
    db: &Connection,
    owner: &str,
    target: &str,
) -> Result<Option<(String, String, bool)>> {
    let needle = target.trim();
    if needle.is_empty() {
        return Ok(None);
    }
    let now = now_unix();
    let mut stmt = db.prepare(
        "SELECT id,name,capabilities,last_seen_unix
         FROM user_nodes WHERE owner=?1 ORDER BY created_at, id",
    )?;
    let rows = stmt
        .query_map([owner], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<i64>>(3)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    for (id, name, _, last_seen) in &rows {
        if id.eq_ignore_ascii_case(needle) {
            let online = last_seen.is_some_and(|t| now.saturating_sub(t) <= ONLINE_WINDOW_SECONDS);
            return Ok(Some((id.clone(), name.clone(), online)));
        }
    }
    for (id, name, _, last_seen) in &rows {
        if name.eq_ignore_ascii_case(needle) {
            let online = last_seen.is_some_and(|t| now.saturating_sub(t) <= ONLINE_WINDOW_SECONDS);
            return Ok(Some((id.clone(), name.clone(), online)));
        }
    }
    for (id, name, capabilities, last_seen) in &rows {
        if host_from_capabilities(capabilities)
            .is_some_and(|host| host.eq_ignore_ascii_case(needle))
        {
            let online = last_seen.is_some_and(|t| now.saturating_sub(t) <= ONLINE_WINDOW_SECONDS);
            return Ok(Some((id.clone(), name.clone(), online)));
        }
    }
    let needle_low = needle.to_ascii_lowercase();
    let is_local_agent_alias = needle_low.starts_with("agy")
        || matches!(
            needle_low.as_str(),
            "codex" | "claude" | "gemini" | "opencode" | "本机" | "本地" | "线上"
        );
    if is_local_agent_alias {
        if let Some((id, name, _, last_seen)) = rows
            .iter()
            .find(|(_, _, _, seen)| {
                seen.is_some_and(|t| now.saturating_sub(t) <= ONLINE_WINDOW_SECONDS)
            })
            .or_else(|| rows.first())
        {
            let online = last_seen.is_some_and(|t| now.saturating_sub(t) <= ONLINE_WINDOW_SECONDS);
            return Ok(Some((id.clone(), name.clone(), online)));
        }
    }
    Ok(None)
}

pub(super) fn contains_alias(text: &str, alias: &str) -> bool {
    if alias.is_empty() || alias.len() > text.len() {
        return false;
    }
    let check_left = alias.starts_with(|c: char| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    let check_right = alias.ends_with(|c: char| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    for (idx, _) in text.char_indices() {
        let Some(head) = text.get(idx..idx + alias.len()) else {
            continue;
        };
        let matched = if alias.is_ascii() {
            head.eq_ignore_ascii_case(alias)
        } else {
            head == alias
        };
        if !matched {
            continue;
        }
        if check_left {
            if let Some(prev) = text[..idx].chars().next_back() {
                if prev.is_ascii_alphanumeric() || prev == '-' || prev == '_' {
                    continue;
                }
            }
        }
        if check_right {
            if let Some(next) = text[idx + alias.len()..].chars().next() {
                if next.is_ascii_alphanumeric() || next == '-' || next == '_' {
                    continue;
                }
            }
        }
        return true;
    }
    false
}

/// Parses direct chat commands addressed to one of the caller's registered PC nodes,
/// such as `让homelab看看Atlas仓库当前所在分支`, `@homelab 跑一下测试`, or `查下homelab的内存使用情况`.
pub(super) fn match_chat_command(
    db: &Connection,
    owner: &str,
    message: &str,
) -> Result<Option<(String, String, bool, String)>> {
    let trimmed = message.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let now = now_unix();
    let mut stmt = db.prepare(
        "SELECT id,name,capabilities,last_seen_unix
         FROM user_nodes WHERE owner=?1 ORDER BY created_at, id",
    )?;
    let nodes = stmt
        .query_map([owner], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<i64>>(3)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if nodes.is_empty() {
        return Ok(None);
    }

    let mut aliases: Vec<(String, String, String, bool)> = Vec::new();
    for (id, name, capabilities, last_seen) in nodes {
        let online = last_seen.is_some_and(|t| now.saturating_sub(t) <= ONLINE_WINDOW_SECONDS);
        aliases.push((id.clone(), id.clone(), name.clone(), online));
        if !name.eq_ignore_ascii_case(&id) {
            aliases.push((name.clone(), id.clone(), name.clone(), online));
        }
        if let Some(host) = host_from_capabilities(&capabilities) {
            if !host.eq_ignore_ascii_case(&id) && !host.eq_ignore_ascii_case(&name) {
                aliases.push((host.to_owned(), id, name, online));
            }
        }
    }
    aliases.sort_by(|a, b| b.0.len().cmp(&a.0.len()));

    let mut body = trimmed;
    for polite in ["请帮我", "帮我", "麻烦帮我", "麻烦"] {
        if let Some(rest) = body.strip_prefix(polite) {
            body = rest.trim_start();
            break;
        }
    }

    let (candidate_text, require_sep) = if let Some(after_verb) =
        ["派给", "交给", "让", "叫", "请", "@"]
            .iter()
            .find_map(|prefix| body.strip_prefix(prefix))
    {
        (after_verb.trim_start(), false)
    } else {
        (trimmed, true)
    };

    for (alias, node_id, node_name, online) in &aliases {
        if let Some(summary) = strip_alias(candidate_text, alias, require_sep) {
            let summary = summary.trim();
            if !summary.is_empty() {
                return Ok(Some((
                    node_id.clone(),
                    node_name.clone(),
                    *online,
                    truncate_bytes(summary, 4000),
                )));
            }
        }
    }
    for (alias, node_id, node_name, online) in &aliases {
        if contains_alias(trimmed, alias) {
            return Ok(Some((
                node_id.clone(),
                node_name.clone(),
                *online,
                truncate_bytes(trimmed, 4000),
            )));
        }
    }
    Ok(None)
}

fn capability_match_tokens(capabilities: &str) -> Vec<&str> {
    let mut tokens = Vec::new();
    for segment in capabilities.split('|') {
        let part = segment.trim();
        if part.is_empty() {
            continue;
        }
        if let Some(items) = part
            .strip_prefix("repos:")
            .or_else(|| part.strip_prefix("procs:"))
        {
            for item in items.split(',') {
                let item = item.trim();
                if item.chars().count() >= 2 {
                    tokens.push(item);
                }
            }
            continue;
        }
        if part.starts_with("host:") || part.starts_with("agent:") {
            continue;
        }
        if matches!(
            part.split('/').next(),
            Some("linux" | "darwin" | "windows" | "freebsd")
        ) && part.contains('/')
        {
            continue;
        }
        for tok in part.split(|c: char| c.is_whitespace() || matches!(c, '+' | ',' | ';' | '/')) {
            let tok = tok.trim();
            if tok.chars().count() >= 2 {
                tokens.push(tok);
            }
        }
    }
    tokens
}

fn cap_hits(capabilities: &str, text: &str) -> i64 {
    capability_match_tokens(capabilities)
        .into_iter()
        .filter(|tok| contains_alias(text, tok))
        .count() as i64
}

pub(super) fn select_best_node(
    db: &Connection,
    owner: &str,
    message: &str,
) -> Result<Option<(String, String, bool, i64)>> {
    let trimmed = message.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let now = now_unix();
    let last_node_id: Option<String> = db
        .query_row(
            "SELECT node_id FROM node_assignments WHERE owner=?1 ORDER BY updated_at DESC, rowid DESC LIMIT 1",
            [owner],
            |r| r.get(0),
        )
        .optional()?;
    let wants_other_node = ["另一台", "另外一台", "别的机器", "其他机器", "换一台"]
        .iter()
        .any(|k| trimmed.contains(k));

    let mut stmt = db.prepare(
        "SELECT u.id, u.name, u.capabilities, u.last_seen_unix,
                EXISTS(SELECT 1 FROM node_assignments a WHERE a.owner=u.owner AND a.node_id=u.id AND a.status IN ('queued','running')),
                COALESCE((SELECT MAX(a.rowid) FROM node_assignments a WHERE a.owner=u.owner AND a.node_id=u.id), 0)
         FROM user_nodes u WHERE u.owner=?1 ORDER BY u.rowid ASC",
    )?;
    let rows = stmt
        .query_map([owner], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<i64>>(3)?,
                r.get::<_, bool>(4)?,
                r.get::<_, i64>(5)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut best: Option<(i64, bool, i64, isize, String, String)> = None;
    for (idx, (id, name, capabilities, last_seen, busy, last_assigned)) in
        rows.into_iter().enumerate()
    {
        let online = last_seen.is_some_and(|t| now.saturating_sub(t) <= ONLINE_WINDOW_SECONDS);
        let mut score: i64 = 0;
        let explicit_hit = contains_alias(trimmed, &id)
            || (!name.eq_ignore_ascii_case(&id) && contains_alias(trimmed, &name))
            || host_from_capabilities(&capabilities).is_some_and(|h| contains_alias(trimmed, h));
        if explicit_hit {
            score += 100;
        }
        score += cap_hits(&capabilities, trimmed) * 60;
        if online {
            score += 20;
        }
        if !busy {
            score += 10;
        }
        if wants_other_node {
            if last_node_id.as_deref() == Some(id.as_str()) {
                score -= 100;
            } else {
                score += 50;
            }
        }
        // When no explicit rule/node/capability differentiates nodes, prefer idle online
        // nodes and break ties by least-recently-assigned so rule-undefined tasks share load.
        let candidate = (score, online, -last_assigned, -(idx as isize), id, name);
        let replace = match &best {
            None => true,
            Some(prev) => {
                (candidate.0, candidate.1, candidate.2, candidate.3)
                    > (prev.0, prev.1, prev.2, prev.3)
            }
        };
        if replace {
            best = Some(candidate);
        }
    }
    Ok(best.map(|(score, online, _, _, id, name)| (id, name, online, score)))
}

pub(super) fn select_node_with_rule(
    h: &Hub,
    db: &Connection,
    owner: &str,
    message: &str,
) -> Result<Option<(String, String, bool, i64)>> {
    if let Some(rule) = crate::sagent::match_dispatch_rule(h, owner, message) {
        if rule.target_node_id == "cloud" {
            return Ok(None);
        }
        if let Some((id, name, last_seen)) = db
            .query_row(
                "SELECT id, name, last_seen_unix FROM user_nodes WHERE owner=?1 AND id=?2",
                params![owner, rule.target_node_id],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, Option<i64>>(2)?,
                    ))
                },
            )
            .optional()?
        {
            let online =
                last_seen.is_some_and(|t| now_unix().saturating_sub(t) <= ONLINE_WINDOW_SECONDS);
            return Ok(Some((id, name, online, 200)));
        }
    }
    select_best_node(db, owner, message)
}

pub(super) fn best_online_node(
    db: &Connection,
    owner: &str,
    message: &str,
) -> Result<Option<(String, String, bool, i64)>> {
    let now = now_unix();
    let cutoff = now.saturating_sub(ONLINE_WINDOW_SECONDS);
    let has_online: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM user_nodes WHERE owner=?1 AND last_seen_unix IS NOT NULL AND last_seen_unix >= ?2)",
        params![owner, cutoff],
        |r| r.get(0),
    )?;
    if !has_online {
        return Ok(None);
    }
    Ok(select_best_node(db, owner, message)?.filter(|(_, _, online, _)| *online))
}

fn should_auto_route_to_node(
    h: &Hub,
    db: &Connection,
    owner: &str,
    message: &str,
    score: i64,
    online: bool,
) -> Result<bool> {
    if [
        "记住",
        "把刚才的个人设置",
        "我的职责",
        "回复偏好",
        "修改个人资料",
        "公开资料",
    ]
    .iter()
    .any(|kw| message.contains(kw))
    {
        return Ok(false);
    }
    for contact in h.directory.contacts(owner) {
        if contains_alias(message, &contact) {
            return Ok(false);
        }
        let display: Option<String> = db
            .query_row(
                "SELECT display_name FROM member_profiles WHERE owner=?1 AND status='active'",
                [&contact],
                |r| r.get(0),
            )
            .optional()?;
        if display
            .as_deref()
            .is_some_and(|name| !name.trim().is_empty() && contains_alias(message, name.trim()))
        {
            return Ok(false);
        }
    }
    if message.contains("/api/attachments/att_")
        || message.contains("[图片附件:")
        || message.contains("[文件附件:")
    {
        return Ok(true);
    }
    if score >= 60 {
        return Ok(true);
    }
    if !online {
        return Ok(false);
    }
    let lower = message.to_ascii_lowercase();
    const LOCAL_INTENT_KEYWORDS: &[&str] = &[
        "内存",
        "磁盘",
        "cpu",
        "gpu",
        "显卡",
        "负载",
        "进程",
        "端口",
        "容器",
        "docker",
        "分支",
        "branch",
        "commit",
        "提交",
        "git",
        "仓库",
        "代码",
        "文件",
        "目录",
        "路径",
        "日志",
        "log",
        "测试",
        "test",
        "编译",
        "build",
        "运行",
        "执行",
        "跑",
        "查看",
        "查下",
        "查一下",
        "看看",
        "看下",
        "检查",
        "脚本",
        "状态",
        "环境",
        "继续",
        "接着",
        "让他",
        "叫他",
        "改一下",
        "修改",
        "去掉",
        "删掉",
        "删除",
        "修复",
        "优化",
        "输入框",
        "界面",
        "页面",
        "组件",
        "前端",
        "样式",
        "diff",
        "status",
        "top",
        "free",
        "df",
        "ps",
        "tmux",
        "screen",
        "窗口",
        "终端",
        "会话",
        "服务",
        "后台",
        "活跃",
        "挂了",
        "报错",
        "更新",
        "部署",
        "上线",
        "同步",
        "发布",
        "推送",
        "重启",
        "构建",
        "打包",
        "发版",
        "deploy",
        "push",
        "agy",
        "agy1",
        "codex",
        "claude",
    ];
    if LOCAL_INTENT_KEYWORDS
        .iter()
        .any(|kw| lower.contains(kw) || message.contains(kw))
    {
        return Ok(true);
    }
    let recent_node_active: bool = db
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM node_assignments WHERE owner=?1 AND updated_at >= ?2)",
            params![owner, now_unix().saturating_sub(600)],
            |r| r.get(0),
        )
        .unwrap_or(false);
    if recent_node_active {
        let is_followup_or_confirm = message.trim().chars().count() <= 24
            || [
                "哪些",
                "哪个",
                "那",
                "再",
                "它",
                "这个",
                "刚才",
                "继续",
                "还有",
                "顺便",
                "为什么",
                "怎么",
                "多少",
                "什么",
                "要",
                "好",
                "好的",
                "可以",
                "行",
                "去查",
                "试试",
            ]
            .iter()
            .any(|cue| message.contains(cue));
        if is_followup_or_confirm {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(super) fn is_topic_summary(summary: &str) -> bool {
    summary.starts_with("📁 ")
        || summary.starts_with("🖥️ ")
        || summary.starts_with("💬 ")
        || summary.starts_with("🦊 ")
        || summary.starts_with("⏰ ")
}

pub(super) fn topic_turns(db: &Connection, task_id: &str) -> Result<Vec<Value>> {
    let merged_prefix = format!("[归并:{task_id}]%");
    let mut stmt = db.prepare(
        "SELECT COALESCE(json_extract(t.response, '$.draft.instruction'), ''),
                d.content,
                d.created_at
         FROM deliveries d
         LEFT JOIN draft_turns t ON t.draft_id = d.source_draft_id
         WHERE (d.task_id=?1 OR d.task_id IN (SELECT id FROM tasks WHERE summary LIKE ?2))
           AND d.status='submitted'
         ORDER BY d.created_at ASC, d.rowid ASC",
    )?;
    let raw_rows = stmt
        .query_map(params![task_id, merged_prefix], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let rows = raw_rows
        .into_iter()
        .enumerate()
        .map(|(idx, (question, answer, created_at))| {
            json!({
                "seq": (idx + 1) as i64,
                "question": question,
                "answer": answer,
                "created_at": created_at,
            })
        })
        .collect();
    Ok(rows)
}

pub(crate) fn consolidate_self_topics(db: &Connection, owner: &str) -> Result<()> {
    let mut stmt = db.prepare(
        "SELECT id, summary, result FROM tasks
         WHERE requester=?1 AND owner=?1 AND summary LIKE '💬 %' AND status != 'working'
         ORDER BY rowid ASC",
    )?;
    let chat_topics = stmt
        .query_map([owner], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if chat_topics.is_empty() {
        return Ok(());
    }

    let mut sys_fragments = Vec::new();
    let mut cron_fragments = Vec::new();
    for (id, summary, result) in chat_topics {
        let body = summary.strip_prefix("💬 ").unwrap_or(&summary).trim();
        let lower = body.to_ascii_lowercase();
        let is_cron_setup = [
            "每天",
            "每日",
            "每晚",
            "每早",
            "每周",
            "每小时",
            "每隔",
            "定时",
        ]
        .iter()
        .any(|p| body.starts_with(p));
        let is_sys_fragment = [
            "tmux",
            "screen",
            "窗口",
            "终端",
            "会话",
            "路由",
            "homelab",
            "这台pc",
            "这台机器",
            "agy",
            "systemd",
        ]
        .iter()
        .any(|kw| lower.contains(kw) || body.contains(kw));
        if is_cron_setup {
            cron_fragments.push((id, summary));
        } else if is_sys_fragment {
            sys_fragments.push((id, summary, result));
        }
    }

    if !cron_fragments.is_empty() {
        let canon_cron: Option<String> = db
            .query_row(
                "SELECT id FROM tasks
                 WHERE requester=?1 AND owner=?1 AND summary LIKE '⏰ %'
                 ORDER BY rowid DESC LIMIT 1",
                [owner],
                |r| r.get(0),
            )
            .optional()?;
        let tag = canon_cron.as_deref().unwrap_or("cron");
        for (frag_id, old_summary) in cron_fragments {
            let archived = format!("[归并:{tag}] {old_summary}");
            db.execute(
                "UPDATE tasks SET summary=?1 WHERE id=?2",
                params![archived, frag_id],
            )?;
        }
    }

    if !sys_fragments.is_empty() {
        let existing_sys: Option<String> = db
            .query_row(
                "SELECT id FROM tasks
                 WHERE requester=?1 AND owner=?1 AND summary LIKE '🖥️ % 系统与环境'
                 ORDER BY rowid DESC LIMIT 1",
                [owner],
                |r| r.get(0),
            )
            .optional()?;
        let canon_id = if let Some(cid) = existing_sys {
            cid
        } else {
            let node_name = db
                .query_row(
                    "SELECT name FROM user_nodes WHERE owner=?1 ORDER BY online DESC, rowid ASC LIMIT 1",
                    [owner],
                    |r| r.get::<_, String>(0),
                )
                .optional()?
                .unwrap_or_else(|| "本机".to_owned());
            let (first_id, _, _) = &sys_fragments[0];
            let new_summary = format!("🖥️ {node_name} 系统与环境");
            db.execute(
                "UPDATE tasks SET summary=?1 WHERE id=?2",
                params![new_summary, first_id],
            )?;
            first_id.clone()
        };
        let mut latest_result: Option<String> = None;
        for (frag_id, old_summary, frag_result) in sys_fragments {
            if let Some(res) = frag_result.filter(|s| !s.trim().is_empty()) {
                latest_result = Some(res);
            }
            if frag_id == canon_id {
                continue;
            }
            let archived = format!("[归并:{canon_id}] {old_summary}");
            db.execute(
                "UPDATE tasks SET summary=?1 WHERE id=?2",
                params![archived, frag_id],
            )?;
        }
        if let Some(res) = latest_result {
            db.execute(
                "UPDATE tasks SET result=?1 WHERE id=?2",
                params![res, canon_id],
            )?;
        }
    }

    // Consolidate multiple completed `🦊 GitLab #... · <repo>` topics for the same repo into `🦊 GitLab · <repo>`
    let mut gl_stmt = db.prepare(
        "SELECT id, summary, result FROM tasks
         WHERE requester=?1 AND owner=?1 AND summary LIKE '🦊 GitLab %· %' AND status != 'working'
         ORDER BY rowid ASC",
    )?;
    let gl_rows = gl_stmt
        .query_map([owner], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut by_repo: std::collections::HashMap<String, Vec<(String, String, Option<String>)>> =
        std::collections::HashMap::new();
    for (id, summary, result) in gl_rows {
        if let Some((_, repo)) = summary.split_once("· ") {
            let repo = repo.trim().to_owned();
            if !repo.is_empty() {
                by_repo.entry(repo).or_default().push((id, summary, result));
            }
        }
    }
    for (repo, group) in by_repo {
        if group.len() < 2 {
            continue;
        }
        let canon_title = format!("🦊 GitLab · {repo}");
        let (canon_id, _, _) = &group[group.len() - 1];
        let canon_id = canon_id.clone();
        db.execute(
            "UPDATE tasks SET summary=?1 WHERE id=?2",
            params![canon_title, canon_id],
        )?;
        let mut latest_res: Option<String> = None;
        for (frag_id, old_sum, res) in group {
            if let Some(r) = res.filter(|s| !s.trim().is_empty()) {
                latest_res = Some(r);
            }
            if frag_id == canon_id {
                continue;
            }
            let archived = format!("[归并:{canon_id}] {old_sum}");
            db.execute(
                "UPDATE tasks SET summary=?1 WHERE id=?2",
                params![archived, frag_id],
            )?;
        }
        if let Some(r) = latest_res {
            db.execute(
                "UPDATE tasks SET result=?1 WHERE id=?2",
                params![r, canon_id],
            )?;
        }
    }
    Ok(())
}

static LAST_TOPIC_CONSOLIDATION_SIG: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashMap<String, String>>,
> = std::sync::OnceLock::new();

fn topic_consolidation_cache(
) -> &'static std::sync::Mutex<std::collections::HashMap<String, String>> {
    LAST_TOPIC_CONSOLIDATION_SIG.get_or_init(|| std::sync::Mutex::new(Default::default()))
}

fn compute_self_topics_signature(db: &Connection, owner: &str) -> Result<String> {
    let mut stmt = db.prepare(
        "SELECT id, summary FROM tasks
         WHERE requester=?1 AND owner=?1 AND status != 'working'
         ORDER BY rowid DESC LIMIT 30",
    )?;
    let rows = stmt
        .query_map([owner], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let parts: Vec<String> = rows
        .into_iter()
        .filter(|(_, s)| is_topic_summary(s))
        .map(|(id, s)| format!("{id}:{s}"))
        .collect();
    Ok(parts.join("|"))
}

pub(crate) async fn llm_consolidate_self_topics(h: &Hub, owner: &str) -> Result<()> {
    {
        let db = h.db()?;
        consolidate_self_topics(&db, owner)?;
    }
    let crate::Runtime::Dsh { url, token } = &h.runtime else {
        return Ok(());
    };

    let (topics, runtime_profile) = {
        let db = h.db()?;
        let mut stmt = db.prepare(
            "SELECT id, summary, COALESCE(result, '') FROM tasks
             WHERE requester=?1 AND owner=?1 AND status != 'working'
             ORDER BY rowid DESC LIMIT 30",
        )?;
        let raw = stmt
            .query_map([owner], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let filtered: Vec<(String, String, String)> = raw
            .into_iter()
            .filter(|(_, sum, _)| is_topic_summary(sum))
            .collect();
        let has_chat_frag = filtered.iter().any(|(_, sum, _)| sum.starts_with("💬 "));
        if !has_chat_frag || filtered.len() < 2 {
            return Ok(());
        }
        let sig = filtered
            .iter()
            .map(|(id, sum, _)| format!("{id}:{sum}"))
            .collect::<Vec<_>>()
            .join("|");
        if let Ok(mut guard) = topic_consolidation_cache().lock() {
            if guard.get(owner) == Some(&sig) {
                return Ok(());
            }
            guard.insert(owner.to_owned(), sig);
        }
        let prof = crate::profiles::route(&db, owner, None, None)?;
        (filtered, prof)
    };

    let topic_lines: Vec<String> = topics
        .iter()
        .map(|(id, sum, res)| {
            let short_res: String = res
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .chars()
                .take(80)
                .collect();
            if short_res.is_empty() {
                format!("- ID `{id}` | 标题：{sum}")
            } else {
                format!("- ID `{id}` | 标题：{sum} | 最近内容：{short_res}")
            }
        })
        .collect();

    let prompt = format!(
        "你是用户的个人 Super-Agent 历史脉络整理器（LLM Topic Consolidator）。\n\
         请审视用户当前的任务脉络列表，根据**真实语义**找出属于同一件事、同一台机器/终端环境、同一个代码仓库或同一个定时任务的散装 `💬` 话题，将它们归并到对应的目标主脉络中。\n\n\
         【当前任务脉络列表】\n{}\n\n\
         【归并要求】\n\
         1. `from_id` 必须是待归并的散装碎片脉络 ID，`into_id` 必须是保留的目标主脉络 ID（两者不能相同，且都必须在上述列表中）。\n\
         2. 如果两个 `💬` 话题聊的是同一件事，可以将较旧或较零散的一个 `from_id` 归并到另一个 `into_id`，并在 `new_title` 中给出一个更干净的归纳标题（以 `🖥️ `、`📁 ` 或 `💬 ` 开头，6～16 字）。\n\
         3. 如果话题之间语义确实相互独立，不要强行归并。\n\n\
         请只输出一行严格合法的 JSON（不要代码块）：\n\
         `{{\"merges\":[{{\"from_id\":\"碎片ID\",\"into_id\":\"目标主脉络ID\",\"new_title\":null}}]}}`",
        topic_lines.join("\n")
    );

    let dsh = crate::runtime::DshRuntime {
        client: &h.client,
        url,
        token,
    };
    let sid = Uuid::new_v4().to_string();
    let Ok(ans) = crate::runtime::Runtime::turn(
        &dsh,
        crate::runtime::TurnRequest {
            kind: crate::runtime::TurnKind::Chat,
            message: &prompt,
            session_id: &sid,
            history: &[],
            profile: &runtime_profile,
            task_context: None,
            draft_revision: None,
            task_role: None,
            owner_context: None,
            profile_context: None,
        },
    )
    .await
    else {
        return Ok(());
    };

    let raw = ans.reply.trim();
    let (Some(s), Some(e)) = (raw.find('{'), raw.rfind('}')) else {
        return Ok(());
    };
    let Ok(parsed) = serde_json::from_str::<Value>(&raw[s..=e]) else {
        return Ok(());
    };
    let Some(merges) = parsed["merges"].as_array() else {
        return Ok(());
    };
    if merges.is_empty() {
        return Ok(());
    }

    let db = h.db()?;
    let valid_ids: std::collections::HashMap<String, (String, String)> = topics
        .into_iter()
        .map(|(id, sum, res)| (id, (sum, res)))
        .collect();
    let mut merged_from = std::collections::HashSet::new();

    for m in merges {
        let Some(from_id) = m["from_id"].as_str() else {
            continue;
        };
        let Some(into_id) = m["into_id"].as_str() else {
            continue;
        };
        if from_id == into_id
            || merged_from.contains(from_id)
            || merged_from.contains(into_id)
            || !valid_ids.contains_key(from_id)
            || !valid_ids.contains_key(into_id)
        {
            continue;
        }
        let (old_from_sum, from_res) = &valid_ids[from_id];
        let archived = format!("[归并:{into_id}] {old_from_sum}");
        db.execute(
            "UPDATE tasks SET summary=?1 WHERE id=?2 AND requester=?3 AND owner=?3",
            params![archived, from_id, owner],
        )?;
        if !from_res.trim().is_empty() {
            db.execute(
                "UPDATE tasks SET result=?1 WHERE id=?2 AND requester=?3 AND owner=?3 AND (result IS NULL OR result='')",
                params![from_res, into_id, owner],
            )?;
        }
        if let Some(new_title) = m["new_title"]
            .as_str()
            .map(str::trim)
            .filter(|t| !t.is_empty() && is_topic_summary(t))
        {
            db.execute(
                "UPDATE tasks SET summary=?1 WHERE id=?2 AND requester=?3 AND owner=?3",
                params![new_title, into_id, owner],
            )?;
        }
        merged_from.insert(from_id.to_owned());
    }

    if !merged_from.is_empty() {
        if let Ok(new_sig) = compute_self_topics_signature(&db, owner) {
            if let Ok(mut guard) = topic_consolidation_cache().lock() {
                guard.insert(owner.to_owned(), new_sig);
            }
        }
    }
    Ok(())
}

pub(crate) async fn llm_resolve_topic_hint(
    h: &Hub,
    owner: &str,
    node_name: &str,
    question: &str,
    selected_task_id: Option<&str>,
) -> (Option<String>, Option<String>) {
    if selected_task_id.is_some() {
        return (None, None);
    }
    let crate::Runtime::Dsh { url, token } = &h.runtime else {
        return (None, None);
    };
    let clean_q = strip_attachment_markers(question);
    let q_match = if clean_q.is_empty() {
        question.trim()
    } else {
        clean_q.as_str()
    };
    if q_match.is_empty() || q_match.starts_with("[🦊 GitLab") || q_match.starts_with("定时任务：")
    {
        return (None, None);
    }

    let Ok((topics, runtime_profile)) = ({
        let db = match h.db() {
            Ok(db) => db,
            Err(_) => return (None, None),
        };
        let mut stmt = match db.prepare(
            "SELECT id, summary, COALESCE(result, '') FROM tasks
             WHERE requester=?1 AND owner=?1
             ORDER BY rowid DESC LIMIT 25",
        ) {
            Ok(s) => s,
            Err(_) => return (None, None),
        };
        let rows = stmt
            .query_map([owner], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })
            .ok()
            .and_then(|it| it.collect::<rusqlite::Result<Vec<_>>>().ok())
            .unwrap_or_default();
        let filtered: Vec<(String, String, String)> = rows
            .into_iter()
            .filter(|(_, sum, _)| is_topic_summary(sum))
            .collect();
        crate::profiles::route(&db, owner, None, None).map(|prof| (filtered, prof))
    }) else {
        return (None, None);
    };

    let candidates_block = if topics.is_empty() {
        "（当前尚无历史脉络）".to_owned()
    } else {
        topics
            .iter()
            .map(|(id, sum, res)| {
                let short_res: String = res
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
                    .chars()
                    .take(60)
                    .collect();
                if short_res.is_empty() {
                    format!("- `{id}`：{sum}")
                } else {
                    format!("- `{id}`：{sum}（最近结果：{short_res}）")
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    };

    let prompt = format!(
        "你是用户的个人 Super-Agent 任务脉络归并器（LLM Topic Router）。\n\
         用户正在向节点「{node_name}」发送新指令，请根据**语义**判断它应该归并到哪个已有任务脉络，或者为它提炼一个精炼的新脉络标题。\n\n\
         【已有任务脉络列表】\n{candidates_block}\n\n\
         【用户最新指令】\n「{q_match}」\n\n\
         【判断准则】\n\
         1. 凡是涉及本机系统状态、终端窗口（如 tmux/screen）、后台进程/Agent/容器、网络路由或机器环境检查的，优先归并到已有的 `🖥️ ... 系统与环境` 脉络（返回其 `task_id`；若列表中还没有，则返回 `task_id: null, title: \"🖥️ {node_name} 系统与环境\"`）。\n\
         2. 凡是与已有某个 `📁 ...`、`🦊 ...` 或 `💬 ...` 语义上围绕同一项目、同一问题或上下文追问的，返回该脉络的 `task_id`。\n\
         3. 若确属全新的独立话题，返回 `task_id: null`，并在 `title` 中提炼一个 6～14 字的干净主题名（以 `📁 `、`🖥️ ` 或 `💬 ` 开头，不要直接照抄原句）。\n\n\
         请只输出一行严格合法的 JSON（不要代码块）：\n\
         `{{\"task_id\":\"匹配的已有ID或null\",\"title\":\"匹配的标题或新提炼的主题名\"}}`"
    );

    let dsh = crate::runtime::DshRuntime {
        client: &h.client,
        url,
        token,
    };
    let sid = Uuid::new_v4().to_string();
    let Ok(ans) = crate::runtime::Runtime::turn(
        &dsh,
        crate::runtime::TurnRequest {
            kind: crate::runtime::TurnKind::Chat,
            message: &prompt,
            session_id: &sid,
            history: &[],
            profile: &runtime_profile,
            task_context: None,
            draft_revision: None,
            task_role: None,
            owner_context: None,
            profile_context: None,
        },
    )
    .await
    else {
        return (None, None);
    };

    let raw = ans.reply.trim();
    let (Some(s), Some(e)) = (raw.find('{'), raw.rfind('}')) else {
        return (None, None);
    };
    let Ok(val) = serde_json::from_str::<Value>(&raw[s..=e]) else {
        return (None, None);
    };
    let matched_id = val["task_id"]
        .as_str()
        .map(str::trim)
        .filter(|id| !id.is_empty() && *id != "null" && topics.iter().any(|(tid, _, _)| tid == id))
        .map(str::to_owned);
    let suggested_title = val["title"]
        .as_str()
        .map(str::trim)
        .filter(|t| !t.is_empty() && *t != "null")
        .map(|t| {
            let clean: String = t.chars().take(28).collect();
            if is_topic_summary(&clean) {
                clean
            } else {
                format!("💬 {clean}")
            }
        });
    (matched_id, suggested_title)
}

pub(super) fn preserve_attachment_markers(original: &str, expanded: &str) -> String {
    let mut out = expanded.trim().to_owned();
    for prefix in ["[图片附件:", "[文件附件:"] {
        let mut search = original;
        while let Some(start) = search.find(prefix) {
            let tail = &search[start..];
            let token_end = tail
                .find(')')
                .map(|idx| idx + 1)
                .or_else(|| tail.find(']').map(|idx| idx + 1))
                .unwrap_or(tail.len());
            let marker = &tail[..token_end];
            if !out.contains(marker) {
                if !out.is_empty() {
                    out.push(' ');
                }
                out.push_str(marker);
            }
            search = &tail[token_end..];
        }
    }
    truncate_bytes(out.trim(), 4000)
}

fn explicit_named_repo(message: &str) -> Option<String> {
    for suffix in ["仓库", "项目"] {
        if let Some(pos) = message.find(suffix) {
            let prefix = message[..pos].trim_end();
            let tok: String = prefix
                .chars()
                .rev()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_' || *c == '.')
                .collect::<String>()
                .chars()
                .rev()
                .collect();
            if tok.len() >= 2 {
                return Some(tok.to_ascii_lowercase());
            }
        }
    }
    None
}

pub(super) fn strip_attachment_markers(text: &str) -> String {
    let mut current = text.to_owned();
    for marker in ["[图片附件:", "[文件附件:"] {
        let mut out = String::with_capacity(current.len());
        let mut rest = current.as_str();
        while let Some(start) = rest.find(marker) {
            out.push_str(&rest[..start]);
            if let Some(end) = rest[start..].find(']') {
                rest = &rest[start + end + 1..];
            } else {
                rest = "";
                break;
            }
        }
        out.push_str(rest);
        current = out;
    }
    current.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn classify_chat_topic(
    db: &Connection,
    owner: &str,
    node_name: &str,
    question: &str,
    selected_task_id: Option<&str>,
    llm_suggested_title: Option<&str>,
) -> Result<(Option<(String, String, i64)>, String)> {
    if let Some(tid) = selected_task_id {
        if let Some((id, summary, status, revision)) = db
            .query_row(
                "SELECT id, summary, status, revision FROM tasks
                 WHERE id=?1 AND requester=?2 AND owner=?2",
                params![tid, owner],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, i64>(3)?,
                    ))
                },
            )
            .optional()?
        {
            return Ok((Some((id, status, revision)), summary));
        }
    }

    let mut repos: Vec<String> = Vec::new();
    let mut cap_stmt = db.prepare("SELECT capabilities FROM user_nodes WHERE owner=?1")?;
    for row in cap_stmt.query_map([owner], |r| r.get::<_, String>(0))? {
        let caps = row?;
        for seg in caps.split('|') {
            let seg = seg.trim();
            if let Some(list) = seg.strip_prefix("repos:") {
                for repo in list.split(',') {
                    let repo = repo.trim();
                    if repo.len() >= 2 && !repos.iter().any(|r| r.eq_ignore_ascii_case(repo)) {
                        repos.push(repo.to_owned());
                    }
                }
            }
        }
    }
    repos.sort_by(|a, b| b.len().cmp(&a.len()));

    let clean_question = strip_attachment_markers(question);
    let question_for_match = if clean_question.is_empty() {
        question.trim()
    } else {
        clean_question.as_str()
    };

    let topic_title = if let Some(rest) = question_for_match.strip_prefix("[🦊 GitLab") {
        let title: String = rest
            .split(']')
            .next()
            .unwrap_or("")
            .trim()
            .chars()
            .take(36)
            .collect();
        if title.is_empty() {
            "🦊 GitLab 协同上下文".to_owned()
        } else {
            format!("🦊 GitLab {title}")
        }
    } else if let Some(rest) = question_for_match.strip_prefix("定时任务：") {
        let short: String = rest.trim().chars().take(24).collect();
        format!(
            "⏰ 定时任务：{}",
            if short.is_empty() { "巡检" } else { &short }
        )
    } else if let Some(llm_title) = llm_suggested_title.filter(|t| !t.trim().is_empty()) {
        llm_title.trim().to_owned()
    } else if let Some(repo) = repos.iter().find(|r| contains_alias(question_for_match, r)) {
        format!("📁 {} 仓库上下文", repo.to_ascii_lowercase())
    } else if let Some(repo) = explicit_named_repo(question_for_match) {
        format!("📁 {repo} 仓库上下文")
    } else if {
        let lower = question_for_match.to_ascii_lowercase();
        [
            "内存",
            "磁盘",
            "cpu",
            "gpu",
            "显卡",
            "负载",
            "进程",
            "端口",
            "容器",
            "docker",
            "top",
            "free",
            "df",
            "ps",
            "状态",
            "环境",
            "tmux",
            "screen",
            "窗口",
            "终端",
            "会话",
            "路由",
            "这台pc",
            "这台机器",
            "agy",
            "systemd",
        ]
        .iter()
        .any(|kw| lower.contains(kw) || question_for_match.contains(kw))
    } {
        let wants_other = ["另一台", "另外一台", "别的机器", "其他机器", "换一台"]
            .iter()
            .any(|k| question_for_match.contains(k));
        let explicit_node = match_chat_command(db, owner, question_for_match)?.is_some();
        let has_specific_cap_hit = db
            .query_row(
                "SELECT capabilities FROM user_nodes WHERE owner=?1 AND name=?2 LIMIT 1",
                params![owner, node_name],
                |r| r.get::<_, String>(0),
            )
            .optional()?
            .is_some_and(|c| cap_hits(&c, question_for_match) > 0);
        if !wants_other && !explicit_node && !has_specific_cap_hit {
            if let Some((id, summary, status, revision)) = db
                .query_row(
                    "SELECT id, summary, status, revision FROM tasks
                     WHERE requester=?1 AND owner=?1 AND summary LIKE '🖥️ % 系统与环境'
                     ORDER BY rowid DESC LIMIT 1",
                    [owner],
                    |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, String>(2)?,
                            r.get::<_, i64>(3)?,
                        ))
                    },
                )
                .optional()?
            {
                return Ok((Some((id, status, revision)), summary));
            }
        }
        format!("🖥️ {node_name} 系统与环境")
    } else {
        let trimmed = question_for_match.trim();
        let is_followup = [
            "那",
            "再",
            "它",
            "这个",
            "刚才",
            "继续",
            "还有",
            "顺便",
            "为什么",
            "怎么",
        ]
        .iter()
        .any(|cue| trimmed.starts_with(cue));
        if let Some(llm_title) = llm_suggested_title.filter(|t| !t.trim().is_empty()) {
            llm_title.trim().to_owned()
        } else {
            if let Some((id, summary, status, revision)) =
                crate::sagent::match_existing_self_topic(db, owner, trimmed)?
            {
                return Ok((Some((id, status, revision)), summary));
            }
            if is_followup {
                if let Some((id, summary, status, revision)) = db
                    .query_row(
                        "SELECT id, summary, status, revision FROM tasks
                         WHERE requester=?1 AND owner=?1
                           AND (summary LIKE '📁 %' OR summary LIKE '🖥️ %' OR summary LIKE '💬 %')
                         ORDER BY rowid DESC LIMIT 1",
                        [owner],
                        |r| {
                            Ok((
                                r.get::<_, String>(0)?,
                                r.get::<_, String>(1)?,
                                r.get::<_, String>(2)?,
                                r.get::<_, i64>(3)?,
                            ))
                        },
                    )
                    .optional()?
                {
                    return Ok((Some((id, status, revision)), summary));
                }
            }
            if clean_question.is_empty() || clean_question == "请帮我查看并分析这张图片"
            {
                "💬 图片分析与解答".to_owned()
            } else {
                let clean: String = trimmed
                    .trim_end_matches(['?', '？', '!', '！', '.', '。'])
                    .chars()
                    .take(24)
                    .collect();
                format!("💬 {}", if clean.is_empty() { trimmed } else { &clean })
            }
        }
    };

    let existing = db
        .query_row(
            "SELECT id, status, revision FROM tasks
             WHERE requester=?1 AND owner=?1 AND summary=?2
             ORDER BY rowid DESC LIMIT 1",
            params![owner, topic_title],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            },
        )
        .optional()?;
    Ok((existing, topic_title))
}

/// Groups chat-routed work into canonical Muse-style Topic tasks (`requester = owner`)
/// and queues the current question on the selected local PC node.
pub(super) fn dispatch_chat_task(
    tx: &Connection,
    owner: &str,
    node_id: &str,
    node_name: &str,
    online: bool,
    question: &str,
    selected_task_id: Option<&str>,
) -> Result<(String, String)> {
    dispatch_chat_task_with_hint(
        tx,
        owner,
        node_id,
        node_name,
        online,
        question,
        selected_task_id,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
pub(super) fn dispatch_chat_task_with_hint(
    tx: &Connection,
    owner: &str,
    node_id: &str,
    node_name: &str,
    online: bool,
    question: &str,
    selected_task_id: Option<&str>,
    llm_suggested_title: Option<&str>,
) -> Result<(String, String)> {
    let question = truncate_bytes(question.trim(), 4000);
    if question.is_empty() {
        return Err(invalid("请说明要让该 PC 执行的具体任务内容"));
    }
    let (existing, topic_title) = classify_chat_topic(
        tx,
        owner,
        node_name,
        &question,
        selected_task_id,
        llm_suggested_title,
    )?;
    let mut final_node_id = node_id.to_owned();
    let mut final_node_name = node_name.to_owned();
    let mut final_online = online;
    let (task_id, target_rev) = match existing {
        Some((tid, status, revision)) => {
            let wants_other = ["另一台", "另外一台", "别的机器", "其他机器", "换一台"]
                .iter()
                .any(|k| question.contains(k));
            let explicit_node_mentioned = match_chat_command(tx, owner, &question)?.is_some();
            if !wants_other && !explicit_node_mentioned {
                let target_caps: String = tx
                    .query_row(
                        "SELECT capabilities FROM user_nodes WHERE owner=?1 AND id=?2",
                        params![owner, final_node_id],
                        |r| r.get(0),
                    )
                    .optional()?
                    .unwrap_or_default();
                let target_cap_hits = cap_hits(&target_caps, &question);
                if let Some((prev_id, prev_name, prev_caps, prev_seen)) = tx
                    .query_row(
                        "SELECT n.id, n.name, n.capabilities, n.last_seen_unix
                         FROM node_assignments a
                         JOIN user_nodes n ON n.owner=a.owner AND n.id=a.node_id
                         WHERE a.task_id=?1 AND a.owner=?2",
                        params![tid, owner],
                        |r| {
                            Ok((
                                r.get::<_, String>(0)?,
                                r.get::<_, String>(1)?,
                                r.get::<_, String>(2)?,
                                r.get::<_, Option<i64>>(3)?,
                            ))
                        },
                    )
                    .optional()?
                {
                    let prev_online = prev_seen
                        .is_some_and(|t| now_unix().saturating_sub(t) <= ONLINE_WINDOW_SECONDS);
                    let prev_hits = cap_hits(&prev_caps, &question);
                    if (prev_online || !final_online) && prev_hits >= target_cap_hits {
                        final_node_id = prev_id;
                        final_node_name = prev_name;
                        final_online = prev_online;
                    }
                }
            }
            let next_rev = revision + 1;
            tx.execute(
                "UPDATE tasks SET status='working', revision=?2 WHERE id=?1",
                params![tid, next_rev],
            )?;
            let action = if matches!(status.as_str(), "completed" | "ready_for_acceptance") {
                "return"
            } else {
                "accept"
            };
            crate::audit::record(
                tx,
                &tid,
                owner,
                action,
                Some(status.as_str()),
                "working",
                next_rev,
            )?;
            (tid, next_rev)
        }
        None => {
            let tid = Uuid::new_v4().to_string();
            tx.execute(
                "INSERT INTO tasks(id,requester,owner,summary,status,revision) VALUES (?1,?2,?2,?3,'working',1)",
                params![tid, owner, topic_title],
            )?;
            crate::audit::record(tx, &tid, owner, "created", None, "awaiting_owner", 0)?;
            crate::audit::record(
                tx,
                &tid,
                owner,
                "accept",
                Some("awaiting_owner"),
                "working",
                1,
            )?;
            (tid, 1)
        }
    };
    tx.execute(
        "INSERT INTO node_assignments(task_id,owner,node_id,task_revision,instruction,status,draft_request_id,updated_at)
         VALUES (?1,?2,?3,?4,?5,'queued',NULL,?6)
         ON CONFLICT(task_id) DO UPDATE SET
            owner=excluded.owner,
            node_id=excluded.node_id,
            task_revision=excluded.task_revision,
            instruction=excluded.instruction,
            status='queued',
            lease_hash=NULL, lease_until=NULL, submission_hash=NULL,
            authorization='manual',
            draft_request_id=NULL,
            updated_at=excluded.updated_at",
        params![task_id, owner, final_node_id, target_rev, question, now_unix()],
    )?;
    let state_note = if final_online {
        "节点正在后台继续执行，完成后结果会自动发回本对话。"
    } else {
        "节点当前显示离线，上线后会自动拉取并执行（如还没启动，请先在对应电脑运行一键启动命令）。"
    };
    let clean_question = strip_attachment_markers(&question);
    let display_question = if clean_question.is_empty() {
        "图片附件分析"
    } else {
        clean_question.as_str()
    };
    let reply = format!(
        "已归入主题「{topic_title}」，并交由「{final_node_name}（{final_node_id}）」处理：{display_question}\n{state_note}"
    );
    Ok((task_id, reply))
}

pub(super) fn auto_assign_on_accept(
    tx: &Connection,
    owner: &str,
    task_id: &str,
    revision: i64,
) -> Result<bool> {
    if !crate::automation_policy::read(tx, owner)?.auto_accept {
        return Ok(false);
    }
    let now = now_unix();
    let cutoff = now.saturating_sub(ONLINE_WINDOW_SECONDS);
    let node_id: Option<String> = tx
        .query_row(
            "SELECT id FROM user_nodes
             WHERE owner=?1
             ORDER BY CASE WHEN last_seen_unix IS NOT NULL AND last_seen_unix >= ?2 THEN 0 ELSE 1 END,
                      rowid ASC
             LIMIT 1",
            params![owner, cutoff],
            |r| r.get(0),
        )
        .optional()?;
    let Some(node_id) = node_id else {
        return Ok(false);
    };
    tx.execute(
        "INSERT INTO node_assignments(task_id,owner,node_id,task_revision,instruction,status,draft_request_id,updated_at)
         VALUES (?1,?2,?3,?4,'','queued',NULL,?5)
         ON CONFLICT(task_id) DO UPDATE SET
            owner=excluded.owner,
            node_id=excluded.node_id,
            task_revision=excluded.task_revision,
            instruction=excluded.instruction,
            status='queued',
            lease_hash=NULL, lease_until=NULL, submission_hash=NULL,
            authorization='manual',
            draft_request_id=NULL,
            updated_at=excluded.updated_at",
        params![task_id, owner, node_id, revision, now],
    )?;
    tx.execute(
        "UPDATE node_assignments SET authorization='delegation' WHERE task_id=?1",
        [task_id],
    )?;
    Ok(true)
}

fn completed_node_reply(db: &Connection, task_id: &str) -> Result<Option<String>> {
    let state: Option<(String, String, String, String, String)> = db
        .query_row(
            "SELECT a.status, a.node_id, n.name, t.summary, COALESCE(t.result, '')
             FROM node_assignments a
             JOIN tasks t ON t.id=a.task_id
             JOIN user_nodes n ON n.owner=a.owner AND n.id=a.node_id
             WHERE a.task_id=?1",
            [task_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .optional()?;
    Ok(
        state.and_then(|(status, node_id, node_name, summary, result)| {
            if status == "completed" {
                Some(format!(
                    "🤵 管家（已调度「{node_name}」· 归入主题「{summary}」）：\n\n{result}"
                ))
            } else if status == "failed" {
                Some(format!(
                    "⚠️ 节点「{node_name}（{node_id}）」执行「{summary}」失败。请在对应电脑终端查看日志后重试。"
                ))
            } else {
                None
            }
        }),
    )
}

pub(super) async fn wait_for_node_completion(h: &Hub, task_id: &str) -> Result<Option<String>> {
    for _ in 0..8 {
        tokio::time::sleep(Duration::from_millis(150)).await;
        let reply = {
            let db = h.db()?;
            completed_node_reply(&db, task_id)?
        };
        if reply.is_some() {
            return Ok(reply);
        }
    }
    Ok(None)
}

pub(super) async fn finalize_chat_node_turn(
    h: &Hub,
    owner: &str,
    task_id: &str,
    fallback_reply: &str,
    request_id: Option<&str>,
    online: bool,
    base_response: Value,
) -> Result<Json<Value>> {
    let inline_reply = if online && matches!(h.runtime, crate::Runtime::Dsh { .. }) {
        wait_for_node_completion(h, task_id).await?
    } else {
        None
    };
    let mut db = h.db()?;
    let tx = db.transaction()?;
    let settled = match inline_reply {
        Some(reply) => Some(reply),
        None => completed_node_reply(&tx, task_id)?,
    };
    let reply = match settled {
        Some(done_reply) => done_reply,
        None => {
            tx.execute(
                "INSERT INTO messages(owner,role,content,status,created_at) VALUES (?1,'assistant',?2,'complete',CURRENT_TIMESTAMP)",
                params![owner, fallback_reply],
            )?;
            fallback_reply.to_owned()
        }
    };
    let mut response = base_response;
    response["reply"] = Value::String(reply);
    crate::chat_turns::complete(&tx, owner, request_id, &mut response)?;
    tx.commit()?;
    Ok(Json(response))
}

pub(super) async fn answer_chat(
    h: &Hub,
    owner: &str,
    input: &crate::messages::ChatInput,
    runtime_profile: &crate::profiles::RuntimeProfile,
    task_context: Option<&Value>,
) -> Result<Option<Json<Value>>> {
    let selected_self_topic = input.selected_task.as_ref().filter(|_| {
        task_context
            .as_ref()
            .is_some_and(|c| c["requester"] == owner && c["owner"] == owner)
    });
    let blocking_task = task_context
        .as_ref()
        .is_none_or(|c| c["requester"] != owner || c["owner"] != owner);
    if (input.selected_task.is_some() && blocking_task)
        || input.selected_project.is_some()
        || input.message.trim_start().starts_with('/')
    {
        return Ok(None);
    }
    let explicit_sel = selected_self_topic.map(|s| s.task_id.as_str());
    let has_attachment = input.message.contains("/api/attachments/att_")
        || input.message.contains("[图片附件:")
        || input.message.contains("[文件附件:");

    let forced = {
        let db = h.db()?;
        if let Some(explicit) = match_chat_command(&db, owner, &input.message)? {
            Some(explicit)
        } else if let Some(sel) = selected_self_topic {
            let assigned_node: Option<(String, String, Option<i64>)> = db
                .query_row(
                    "SELECT n.id, n.name, n.last_seen_unix
                     FROM node_assignments a
                     JOIN user_nodes n ON n.owner=a.owner AND n.id=a.node_id
                     WHERE a.task_id=?1 AND a.owner=?2",
                    params![sel.task_id, owner],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()?;
            if let Some((node_id, node_name, last_seen)) = assigned_node {
                let online = last_seen
                    .is_some_and(|t| now_unix().saturating_sub(t) <= ONLINE_WINDOW_SECONDS);
                Some((
                    node_id,
                    node_name,
                    online,
                    truncate_bytes(input.message.trim(), 4000),
                ))
            } else if let Some((node_id, node_name, online, _)) =
                select_best_node(&db, owner, &input.message)?
            {
                Some((
                    node_id,
                    node_name,
                    online,
                    truncate_bytes(input.message.trim(), 4000),
                ))
            } else {
                None
            }
        } else if has_attachment {
            select_node_with_rule(h, &db, owner, &input.message)?.map(
                |(node_id, node_name, online, _)| {
                    (
                        node_id,
                        node_name,
                        online,
                        truncate_bytes(input.message.trim(), 4000),
                    )
                },
            )
        } else {
            None
        }
    };

    let matched = if let Some(f) = forced {
        Some(f)
    } else {
        let db = h.db()?;
        if let Some((node_id, node_name, online, score)) =
            select_node_with_rule(h, &db, owner, &input.message)?
        {
            if should_auto_route_to_node(h, &db, owner, &input.message, score, online)? {
                Some((
                    node_id,
                    node_name,
                    online,
                    truncate_bytes(input.message.trim(), 4000),
                ))
            } else {
                None
            }
        } else {
            None
        }
    };
    let Some((node_id, node_name, online, summary)) = matched else {
        return Ok(None);
    };
    let (llm_tid, llm_title) =
        llm_resolve_topic_hint(h, owner, &node_name, &summary, explicit_sel).await;

    if h.directory.can_delegate(owner, &node_id) {
        return Ok(None);
    }

    let (task_id, fallback_reply) = {
        let mut db = h.db()?;
        let tx = db.transaction()?;
        let (task_id, fallback_reply) = dispatch_chat_task_with_hint(
            &tx,
            owner,
            &node_id,
            &node_name,
            online,
            &summary,
            explicit_sel.or(llm_tid.as_deref()),
            llm_title.as_deref(),
        )?;
        tx.execute(
            "INSERT INTO messages(owner,role,content,status,created_at) VALUES (?1,'user',?2,'complete',CURRENT_TIMESTAMP)",
            params![owner, input.message],
        )?;
        let message_id = tx.last_insert_rowid();
        if let Some(key) = input.request_id.as_deref() {
            tx.execute(
                "UPDATE chat_turns SET message_id=?3 WHERE owner=?1 AND request_id=?2 AND status='pending'",
                params![owner, key, message_id],
            )?;
        }
        tx.commit()?;
        (task_id, fallback_reply)
    };

    let base_response = json!({
        "reply": Value::Null,
        "backend": h.runtime.label(),
        "source": "pc-node",
        "proposal": Value::Null,
        "runtime_profile": runtime_profile,
        "task_context": task_context.cloned().unwrap_or(Value::Null),
        "repository_context": Value::Null,
    });
    finalize_chat_node_turn(
        h,
        owner,
        &task_id,
        &fallback_reply,
        input.request_id.as_deref(),
        online,
        base_response,
    )
    .await
    .map(Some)
}

fn generate_token() -> String {
    format!("agn_{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}

pub(super) fn hash_token(token: &str) -> String {
    format!("{:x}", Sha256::digest(token.as_bytes()))
}

pub(super) async fn list(State(h): State<Hub>, headers: HeaderMap) -> Result<Json<Vec<Value>>> {
    let owner = h.identity(&headers)?;
    let db = h.db()?;
    let now = now_unix();
    let mut query = db.prepare(
        "SELECT id,name,capabilities,last_seen_unix,created_at
         FROM user_nodes WHERE owner=?1 ORDER BY created_at, id",
    )?;
    let rows = query
        .query_map([&owner], |r| {
            let last_seen: Option<i64> = r.get(3)?;
            Ok(json!({
                "id": r.get::<_, String>(0)?,
                "name": r.get::<_, String>(1)?,
                "capabilities": r.get::<_, String>(2)?,
                "last_seen_unix": last_seen,
                "online": last_seen.is_some_and(|t| now.saturating_sub(t) <= ONLINE_WINDOW_SECONDS),
                "created_at": r.get::<_, String>(4)?,
            }))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(Json(rows))
}

fn allocate_node_id(tx: &Connection, owner: &str, requested: &str) -> Result<String> {
    let count: i64 = tx.query_row(
        "SELECT count(*) FROM user_nodes WHERE owner=?1",
        [owner],
        |r| r.get(0),
    )?;
    if count >= MAX_NODES_PER_USER {
        return Err(invalid("最多可注册 16 个 PC 节点"));
    }
    let trimmed = requested.trim();
    if trimmed.is_empty() {
        for n in 1..=64 {
            let candidate = format!("pc-{n}");
            let taken: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM user_nodes WHERE owner=?1 AND id=?2)",
                params![owner, candidate],
                |r| r.get(0),
            )?;
            if !taken {
                return Ok(candidate);
            }
        }
        return Err(invalid("无法生成节点标识，请手动填写"));
    }
    if !valid_node_id(trimmed) {
        return Err(invalid("节点标识需为 1–32 位字母/数字/-/_"));
    }
    let exists: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM user_nodes WHERE owner=?1 AND id=?2)",
        params![owner, trimmed],
        |r| r.get(0),
    )?;
    if exists {
        return Err(Error(
            StatusCode::CONFLICT,
            "节点标识已存在，请换一个标识或直接重置令牌",
        ));
    }
    Ok(trimmed.to_owned())
}

pub(super) async fn create(
    State(h): State<Hub>,
    headers: HeaderMap,
    Json(input): Json<CreateNode>,
) -> Result<Json<Value>> {
    let owner = h.identity(&headers)?;
    let name = input.name.trim();
    let capabilities = input.capabilities.trim();
    if name.is_empty() || !valid_text(name, 64) || !valid_text(capabilities, 200) {
        return Err(invalid("节点名称需为 1–64 字，环境说明不超过 200 字"));
    }
    let token = generate_token();
    let digest = hash_token(&token);
    let mut db = h.db()?;
    let tx = db.transaction()?;
    let id = allocate_node_id(&tx, &owner, &input.id)?;
    tx.execute(
        "INSERT INTO user_nodes(owner,id,name,capabilities,token_hash) VALUES (?1,?2,?3,?4,?5)",
        params![owner, id, name, capabilities, digest],
    )?;
    tx.commit()?;
    Ok(Json(json!({
        "node": {
            "id": id,
            "name": name,
            "capabilities": capabilities,
            "last_seen_unix": Value::Null,
            "online": false,
        },
        "token": token,
    })))
}

pub(super) async fn rotate_token(
    State(h): State<Hub>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Value>> {
    let owner = h.identity(&headers)?;
    if !valid_node_id(&id) {
        return Err(denied());
    }
    let token = generate_token();
    let digest = hash_token(&token);
    let db = h.db()?;
    if db.execute(
        "UPDATE user_nodes SET token_hash=?3 WHERE owner=?1 AND id=?2",
        params![owner, id, digest],
    )? != 1
    {
        return Err(denied());
    }
    db.execute("UPDATE node_assignments SET status='failed', lease_hash=NULL WHERE owner=?1 AND node_id=?2 AND status='running'",params![owner,id])?;
    Ok(Json(json!({ "id": id, "token": token })))
}

pub(super) async fn remove(
    State(h): State<Hub>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Value>> {
    let owner = h.identity(&headers)?;
    if !valid_node_id(&id) {
        return Err(denied());
    }
    let mut db = h.db()?;
    let tx = db.transaction()?;
    tx.execute(
        "DELETE FROM node_assignments WHERE owner=?1 AND node_id=?2",
        params![owner, id],
    )?;
    if tx.execute(
        "DELETE FROM user_nodes WHERE owner=?1 AND id=?2",
        params![owner, id],
    )? != 1
    {
        return Err(denied());
    }
    tx.commit()?;
    Ok(Json(json!({ "removed": id })))
}

pub(super) fn task_assignment(
    db: &Connection,
    owner: &str,
    task_id: &str,
    task_revision: i64,
) -> Result<Option<Value>> {
    Ok(db
        .query_row(
            "SELECT a.node_id, n.name, a.task_revision, a.instruction, a.status, a.draft_request_id, a.updated_at
             FROM node_assignments a
             JOIN user_nodes n ON n.owner=a.owner AND n.id=a.node_id
             WHERE a.task_id=?1 AND a.owner=?2 AND a.task_revision=?3",
            params![task_id, owner, task_revision],
            |r| {
                Ok(json!({
                    "node_id": r.get::<_, String>(0)?,
                    "node_name": r.get::<_, String>(1)?,
                    "task_revision": r.get::<_, i64>(2)?,
                    "instruction": r.get::<_, String>(3)?,
                    "status": r.get::<_, String>(4)?,
                    "draft_request_id": r.get::<_, Option<String>>(5)?,
                    "updated_at": r.get::<_, i64>(6)?,
                }))
            },
        )
        .optional()?)
}

pub(super) async fn assign_task(
    State(h): State<Hub>,
    headers: HeaderMap,
    Path(task_id): Path<String>,
    Json(input): Json<AssignTask>,
) -> Result<Json<Value>> {
    let owner = h.identity(&headers)?;
    if !valid_key(&task_id, 128)
        || input.expected_revision < 0
        || !valid_instruction(&input.instruction)
    {
        return Err(invalid("invalid node assignment"));
    }
    if let Some(ref node_id) = input.node_id {
        if !valid_node_id(node_id) {
            return Err(invalid("invalid node id"));
        }
    }
    let instruction = input.instruction.trim().to_owned();
    let mut db = h.db()?;
    let tx = db.transaction()?;
    let (status, revision): (String, i64) = tx
        .query_row(
            "SELECT status,revision FROM tasks WHERE id=?1 AND owner=?2",
            params![task_id, owner],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?
        .ok_or_else(denied)?;
    if status != "working" || revision != input.expected_revision {
        return Err(Error(
            StatusCode::CONFLICT,
            "任务状态或版本已变更，请刷新后重试",
        ));
    }
    let Some(node_id) = input.node_id else {
        tx.execute(
            "DELETE FROM node_assignments WHERE task_id=?1 AND owner=?2",
            params![task_id, owner],
        )?;
        tx.commit()?;
        clear_live_progress(&h, &task_id);
        return Ok(Json(json!({ "assigned": Value::Null })));
    };
    let node_exists: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM user_nodes WHERE owner=?1 AND id=?2)",
        params![owner, node_id],
        |r| r.get(0),
    )?;
    if !node_exists {
        return Err(denied());
    }
    tx.execute(
        "UPDATE auto_draft_intents SET status='skipped',reason='superseded'
         WHERE owner=?1 AND task_id=?2 AND task_revision=?3 AND status='queued'",
        params![owner, task_id, revision],
    )?;
    let now = now_unix();
    tx.execute(
        "INSERT INTO node_assignments(task_id,owner,node_id,task_revision,instruction,status,draft_request_id,updated_at)
         VALUES (?1,?2,?3,?4,?5,'queued',NULL,?6)
         ON CONFLICT(task_id) DO UPDATE SET
            owner=excluded.owner,
            node_id=excluded.node_id,
            task_revision=excluded.task_revision,
            instruction=excluded.instruction,
            status='queued',
            lease_hash=NULL, lease_until=NULL, submission_hash=NULL,
            authorization='manual',
            draft_request_id=NULL,
            updated_at=excluded.updated_at",
        params![task_id, owner, node_id, revision, instruction, now],
    )?;
    let assigned = task_assignment(&tx, &owner, &task_id, revision)?;
    tx.commit()?;
    clear_live_progress(&h, &task_id);
    Ok(Json(json!({ "assigned": assigned })))
}

fn authenticate_node(h: &Hub, headers: &HeaderMap) -> Result<(String, String, String)> {
    let startup = match h.startup_ok {
        Some(ok) => ok,
        None => h.validate_startup().is_ok(),
    };
    if !startup {
        return Err(Error(
            StatusCode::SERVICE_UNAVAILABLE,
            "tenant configuration required for this database",
        ));
    }
    if headers.get_all("authorization").iter().count() != 1 {
        return Err(Error(StatusCode::UNAUTHORIZED, "node token required"));
    }
    let token = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or(Error(
            StatusCode::UNAUTHORIZED,
            "node bearer token required",
        ))?;
    if !token.starts_with("agn_")
        || token.len() != 68
        || !token[4..].bytes().all(|b| b.is_ascii_hexdigit())
    {
        return Err(Error(StatusCode::UNAUTHORIZED, "invalid node token"));
    }
    let digest = hash_token(token);
    let (owner, node_id, node_name): (String, String, String) = {
        let db = h.db()?;
        db.query_row(
            "SELECT owner,id,name FROM user_nodes WHERE token_hash=?1",
            [&digest],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?
        .ok_or(Error(StatusCode::UNAUTHORIZED, "invalid node token"))?
    };
    h.reload_private_users()?;
    if !h.member_active(&owner)? {
        return Err(Error(
            StatusCode::UNAUTHORIZED,
            "account disabled or invitation pending",
        ));
    }
    let node_env = headers
        .get("x-agenct-node-env")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| {
            !s.is_empty() && s.is_ascii() && s.len() <= 400 && !s.chars().any(char::is_control)
        })
        .map(str::to_owned);
    let db = h.db()?;
    match node_env {
        Some(env_info) => {
            db.execute(
                "UPDATE user_nodes
                 SET last_seen_unix=?3,
                     capabilities=CASE
                         WHEN capabilities='' OR capabilities LIKE '% | host:% | agent:%' THEN ?4
                         ELSE capabilities
                     END
                 WHERE owner=?1 AND id=?2",
                params![owner, node_id, now_unix(), env_info],
            )?;
        }
        None => {
            db.execute(
                "UPDATE user_nodes SET last_seen_unix=?3 WHERE owner=?1 AND id=?2",
                params![owner, node_id, now_unix()],
            )?;
        }
    }
    Ok((owner, node_id, node_name))
}

pub(super) async fn work(State(h): State<Hub>, headers: HeaderMap) -> Result<Json<Value>> {
    let (owner, node_id, node_name) = authenticate_node(&h, &headers)?;
    if headers
        .get("x-agenct-node-protocol")
        .and_then(|v| v.to_str().ok())
        != Some("2")
    {
        return Err(Error(
            StatusCode::UPGRADE_REQUIRED,
            "update the node client: exclusive lease protocol 2 required",
        ));
    }
    let mut db = h.db()?;
    let tx = db.transaction()?;
    crate::automation_policy::revoke(&tx, &owner, &crate::automation_policy::read(&tx, &owner)?)?;
    tx.execute("UPDATE node_assignments SET status='failed' WHERE owner=?1 AND node_id=?2 AND status='running' AND (lease_until IS NULL OR lease_until<=?3)", params![owner,node_id,now_unix()])?;
    let pending: Option<(String, i64, String, String, String)> = tx
        .query_row(
            "SELECT a.task_id, a.task_revision, a.instruction, t.requester, t.summary
             FROM node_assignments a
             JOIN tasks t ON t.id = a.task_id
             WHERE a.owner=?1 AND a.node_id=?2
               AND t.owner=?1 AND t.status='working' AND t.revision=a.task_revision
               AND a.status='queued'
             ORDER BY a.updated_at ASC
             LIMIT 1",
            params![owner, node_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .optional()?;
    let Some((task_id, task_revision, instruction, requester, summary)) = pending else {
        tx.commit()?;
        return Ok(Json(json!({
            "node": { "id": node_id, "name": node_name, "owner": owner },
            "job": Value::Null,
        })));
    };
    let lease_token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    let lease_until = now_unix() + 120;
    tx.execute(
        "UPDATE node_assignments SET status='running', updated_at=?2, lease_hash=?3, lease_until=?4, submission_hash=NULL WHERE task_id=?1",
        params![task_id, now_unix(), hash_token(&lease_token), lease_until],
    )?;
    let details = tasks::details(&tx, &task_id)?
        .map(|d| d["content"].clone())
        .unwrap_or(Value::Null);
    let base_draft: Option<String> = if requester == owner {
        let turns = topic_turns(&tx, &task_id)?;
        let recent: Vec<String> = turns
            .iter()
            .rev()
            .take(6)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .map(|t| {
                let seq = t["seq"].as_i64().unwrap_or(0);
                let q = t["question"].as_str().unwrap_or("");
                let a = t["answer"].as_str().unwrap_or("");
                let a_short: String = a.chars().take(600).collect();
                if q.is_empty() {
                    format!("[第 {seq} 轮] 答：{a_short}")
                } else {
                    format!("[第 {seq} 轮] 问：{q}\n答：{a_short}")
                }
            })
            .collect();
        if recent.is_empty() {
            None
        } else {
            Some(truncate_bytes(&recent.join("\n\n"), 4000))
        }
    } else {
        tx.query_row(
            "SELECT content FROM deliveries
             WHERE task_id=?1 AND author=?2 AND status='draft' AND task_revision=?3
             ORDER BY seq DESC LIMIT 1",
            params![task_id, owner, task_revision],
            |r| r.get(0),
        )
        .optional()?
    };
    let session_context: Option<String> = {
        let mut msg_stmt = tx.prepare(
            "SELECT role, content FROM messages
             WHERE owner=?1 AND status='complete'
               AND content NOT LIKE '已归入主题「%'
             ORDER BY id DESC LIMIT 10",
        )?;
        let rows = msg_stmt
            .query_map([&owner], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let lines: Vec<String> = rows
            .into_iter()
            .rev()
            .map(|(role, content)| {
                let speaker = if role == "user" {
                    "用户"
                } else {
                    "Agenct"
                };
                let snippet: String = strip_attachment_markers(&content)
                    .chars()
                    .take(500)
                    .collect();
                format!("[{speaker}] {snippet}")
            })
            .filter(|line| !line.ends_with("] "))
            .collect();
        let muse_persona_note = h
            .automation
            .lock()
            .ok()
            .and_then(|g| g.muse_profiles.get(&owner).cloned())
            .map(|mp| {
                format!(
                    "\n\n【Muse 核心人设 (SOUL.md) 与汇报要求】\n{}\n\n【Muse 长期记忆 (MEMORY.md)】\n{}\n（请直接以第一人称跟用户自然汇报结果与验证情况，不要输出『### 本地节点初稿』或任何机械套话抬头。）",
                    truncate_bytes(&mp.soul_md, 600),
                    truncate_bytes(&mp.memory_md, 800)
                )
            })
            .unwrap_or_default();
        if lines.is_empty() && muse_persona_note.is_empty() {
            None
        } else {
            Some(truncate_bytes(
                &format!("{}{}", lines.join("\n"), muse_persona_note),
                3800,
            ))
        }
    };
    tx.commit()?;
    let attachments = crate::materials::collect_attachments(
        &h,
        &owner,
        Some(&requester),
        &[&instruction, &summary, base_draft.as_deref().unwrap_or("")],
    );
    Ok(Json(json!({
        "node": { "id": node_id, "name": node_name, "owner": owner },
        "job": {
            "task_id": task_id,
            "task_revision": task_revision,
            "lease_token": lease_token,
            "lease_until": lease_until,
            "requester": requester,
            "summary": summary,
            "details": details,
            "instruction": instruction,
            "base_draft": base_draft,
            "session_context": session_context,
            "attachments": attachments,
        }
    })))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RenewLease {
    lease_token: String,
    expected_revision: i64,
    #[serde(default)]
    stage: String,
    #[serde(default)]
    log_tail: String,
    #[serde(default)]
    elapsed_secs: u64,
}

pub(super) async fn renew_lease(
    State(h): State<Hub>,
    headers: HeaderMap,
    Path(task_id): Path<String>,
    Json(input): Json<RenewLease>,
) -> Result<Json<Value>> {
    let (owner, node_id, _) = authenticate_node(&h, &headers)?;
    let db = h.db()?;
    crate::automation_policy::revoke(&db, &owner, &crate::automation_policy::read(&db, &owner)?)?;
    let now = now_unix();
    let changed=db.execute("UPDATE node_assignments SET lease_until=?6 WHERE task_id=?1 AND owner=?2 AND node_id=?3 AND lease_hash=?4 AND task_revision=?5 AND status='running' AND lease_until>?7 AND EXISTS(SELECT 1 FROM tasks WHERE id=?1 AND status='working' AND revision=?5)", params![task_id,owner,node_id,hash_token(&input.lease_token),input.expected_revision,now+120,now])?;
    if changed != 1 {
        clear_live_progress(&h, &task_id);
        return Err(Error(
            StatusCode::CONFLICT,
            "node execution lease is no longer valid",
        ));
    }
    if !input.stage.trim().is_empty() || !input.log_tail.trim().is_empty() {
        let clean_stage: String = input
            .stage
            .trim()
            .chars()
            .filter(|c| !c.is_control())
            .take(120)
            .collect();
        let clean_tail: String = input
            .log_tail
            .trim()
            .chars()
            .filter(|c| !c.is_control() || *c == '\n')
            .take(500)
            .collect();
        if let Ok(mut guard) = h.node_progress.lock() {
            if guard.len() >= 256 && !guard.contains_key(&task_id) {
                guard.clear();
            }
            guard.insert(
                task_id,
                NodeLiveProgress {
                    node_id,
                    task_revision: input.expected_revision,
                    stage: clean_stage,
                    log_tail: clean_tail,
                    elapsed_secs: input.elapsed_secs.min(86400),
                    updated_at: now,
                },
            );
        }
    }
    Ok(Json(json!({"lease_until":now+120})))
}

pub(super) async fn submit_draft(
    State(h): State<Hub>,
    headers: HeaderMap,
    Path(task_id): Path<String>,
    Json(input): Json<SubmitDraft>,
) -> Result<Json<Value>> {
    let (owner, node_id, node_name) = authenticate_node(&h, &headers)?;
    if !valid_key(&task_id, 128)
        || !valid_key(&input.request_id, 80)
        || input.expected_revision < 0
        || !matches!(input.status.as_str(), "completed" | "failed")
    {
        return Err(invalid("invalid node draft submission"));
    }
    if input.status == "completed"
        && (input.content.trim().is_empty() || input.content.len() > 8000)
    {
        return Err(invalid("draft content must be 1..8000 bytes"));
    }
    let mut db = h.db()?;
    let tx = db.transaction()?;
    let assigned: Option<(i64, String)> = tx
        .query_row(
            "SELECT task_revision, instruction FROM node_assignments
             WHERE task_id=?1 AND owner=?2 AND node_id=?3",
            params![task_id, owner, node_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((assigned_revision, instruction)) = assigned else {
        return Err(denied());
    };
    if assigned_revision != input.expected_revision {
        return Err(Error(
            StatusCode::CONFLICT,
            "task assignment changed before draft submission",
        ));
    }
    let (lease_hash, lease_until, submission_hash): (Option<String>, Option<i64>, Option<String>) =
        tx.query_row(
            "SELECT lease_hash,lease_until,submission_hash FROM node_assignments WHERE task_id=?1",
            [&task_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
    if input.lease_token.is_empty()
        || lease_hash.as_deref() != Some(hash_token(&input.lease_token).as_str())
    {
        return Err(Error(
            StatusCode::CONFLICT,
            "node execution lease does not match",
        ));
    }
    let payload_hash =
        hash_token(&serde_json::to_string(&input).map_err(|_| invalid("invalid node submission"))?);
    if submission_hash
        .as_ref()
        .is_some_and(|old| old != &payload_hash)
    {
        return Err(Error(
            StatusCode::CONFLICT,
            "lease already submitted different output",
        ));
    }
    if input.status == "failed" && submission_hash.as_ref() == Some(&payload_hash) {
        return Ok(Json(json!({"status":"failed"})));
    }
    let (task_status, task_revision, requester, summary): (String, i64, String, String) = tx
        .query_row(
            "SELECT status,revision,requester,summary FROM tasks WHERE id=?1 AND owner=?2",
            params![task_id, owner],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()?
        .ok_or_else(denied)?;
    let turn_request_id = format!("node-{node_id}-{}", input.request_id);
    if input.status == "completed" {
        if let Some((existing_task, existing_draft_id, existing_binding)) = tx
            .query_row(
                "SELECT task_id, draft_id, binding FROM draft_turns WHERE owner=?1 AND request_id=?2",
                params![owner, turn_request_id],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?, r.get::<_, String>(2)?)),
            )
            .optional()?
        {
            let existing_binding: Value = serde_json::from_str(&existing_binding).map_err(|_| invalid("invalid node receipt"))?;
            if existing_task != task_id
                || existing_binding["lease_hash"].as_str() != lease_hash.as_deref()
                || existing_binding["submission_hash"].as_str() != Some(payload_hash.as_str()) {
                return Err(Error(
                    StatusCode::CONFLICT,
                    "request_id already bound to another task, lease or output",
                ));
            }
            let draft_id = existing_draft_id.ok_or_else(denied)?;
            let seq: i64 =
                tx.query_row("SELECT seq FROM deliveries WHERE id=?1", [&draft_id], |r| {
                    r.get(0)
                })?;
            tx.commit()?;
            return Ok(Json(json!({
                "status": "completed",
                "request_id": turn_request_id,
                "draft_id": draft_id,
                "seq": seq,
            })));
        }
    }
    if lease_until.is_none_or(|until| until <= now_unix()) {
        return Err(Error(
            StatusCode::CONFLICT,
            "node execution lease expired; manual reassignment required",
        ));
    }
    let assignment_active: bool = tx.query_row(
        "SELECT status='running' FROM node_assignments WHERE task_id=?1",
        [&task_id],
        |r| r.get(0),
    )?;
    if !assignment_active {
        return Err(Error(
            StatusCode::CONFLICT,
            "node assignment is no longer active",
        ));
    }
    if task_status != "working" || task_revision != input.expected_revision {
        return Err(Error(
            StatusCode::CONFLICT,
            "task must be working at expected revision",
        ));
    }
    if input.status == "failed" {
        tx.execute(
            "UPDATE node_assignments SET status='failed', updated_at=?2, submission_hash=?3 WHERE task_id=?1",
            params![task_id, now_unix(), payload_hash],
        )?;
        if requester == owner {
            let reply = format!(
                "⚠️ 节点「{node_name}（{node_id}）」执行「{summary}」失败。请在对应电脑终端查看日志后重试。"
            );
            tx.execute(
                "INSERT INTO messages(owner,role,content,status,created_at)
                 VALUES (?1,'assistant',?2,'complete',CURRENT_TIMESTAMP)",
                params![owner, reply],
            )?;
        }
        tx.commit()?;
        clear_live_progress(&h, &task_id);
        return Ok(Json(json!({ "status": "failed" })));
    }
    let mut attachment_markers = Vec::new();
    for att in input.attachments.iter().take(4) {
        let (stored, _) = crate::materials::store_attachment_raw(
            &h,
            &owner,
            &att.name,
            &att.mime_type,
            &att.data_base64,
        )?;
        let kind_label = if stored.mime_type.starts_with("image/") {
            "图片附件"
        } else {
            "文件附件"
        };
        attachment_markers.push(format!(
            "[{kind_label}: {} (/api/attachments/{})]",
            stored.name, stored.id
        ));
    }
    let final_content = if attachment_markers.is_empty() {
        input.content.clone()
    } else {
        let suffix = format!("\n{}", attachment_markers.join("\n"));
        let max_body = 8000usize.saturating_sub(suffix.len());
        format!(
            "{}{}",
            truncate_bytes(input.content.trim_end(), max_body),
            suffix
        )
    };
    let selection = task_context::Selection {
        task_id: task_id.clone(),
        expected_revision: input.expected_revision,
    };
    let snapshot = task_context::capture(&tx, &owner, Some(&selection))?.ok_or_else(denied)?;
    let draft_id = Uuid::new_v4().to_string();
    tx.execute(
        "INSERT INTO deliveries(id,task_id,seq,author,content,status,task_revision)
         SELECT ?1,?2,COALESCE(MAX(seq),0)+1,?3,?4,'draft',?5
         FROM deliveries WHERE task_id=?2 AND status='draft'",
        params![
            draft_id,
            task_id,
            owner,
            final_content,
            input.expected_revision
        ],
    )?;
    let seq: i64 = tx.query_row("SELECT seq FROM deliveries WHERE id=?1", [&draft_id], |r| {
        r.get(0)
    })?;
    let created_at: String = tx.query_row(
        "SELECT created_at FROM deliveries WHERE id=?1",
        [&draft_id],
        |r| r.get(0),
    )?;
    let runtime_profile = json!({
        "revision": 0,
        "agent_id": format!("node-{node_id}"),
        "role": "node",
        "model_ref": "local-pc",
    });
    let binding = json!({
        "request_id": turn_request_id,
        "expected_revision": input.expected_revision,
        "node_id": node_id,
        "lease_hash": lease_hash,
        "submission_hash": payload_hash,
    });
    let response = json!({
        "draft": {
            "id": draft_id,
            "seq": seq,
            "base_draft_id": Value::Null,
            "instruction": if instruction.is_empty() { Value::Null } else { json!(instruction) },
            "content": final_content,
            "created_at": created_at,
            "task_revision": input.expected_revision,
            "stale": false,
            "node_id": node_id,
            "node_name": node_name,
        },
        "task_context": snapshot,
        "source_task_context": snapshot,
        "runtime_profile": runtime_profile,
        "backend": "local-node",
    });
    let session_id = Uuid::new_v4().to_string();
    tx.execute(
        "INSERT INTO draft_turns(owner,request_id,task_id,binding,status,task_context,runtime_profile,session_id,response,draft_id)
         VALUES (?1,?2,?3,?4,'complete',?5,?6,?7,?8,?9)",
        params![
            owner,
            turn_request_id,
            task_id,
            binding.to_string(),
            snapshot.to_string(),
            runtime_profile.to_string(),
            session_id,
            response.to_string(),
            draft_id,
        ],
    )?;
    tx.execute(
        "UPDATE node_assignments SET status='completed', draft_request_id=?2, updated_at=?3, submission_hash=?4 WHERE task_id=?1",
        params![task_id, turn_request_id, now_unix(), payload_hash],
    )?;
    if requester == owner {
        tx.execute(
            "INSERT INTO deliveries(id,task_id,seq,author,content,status,task_revision,source_draft_id)
             SELECT ?1,?2,COALESCE(MAX(seq),0)+1,?3,?4,'submitted',?5,?6
             FROM deliveries WHERE task_id=?2 AND status='submitted'",
            params![
                Uuid::new_v4().to_string(),
                task_id,
                owner,
                final_content,
                input.expected_revision + 1,
                draft_id,
            ],
        )?;
        tx.execute(
            "UPDATE tasks SET status='completed', result=?2, revision=?3 WHERE id=?1",
            params![task_id, final_content, input.expected_revision + 2],
        )?;
        crate::audit::record(
            &tx,
            &task_id,
            &owner,
            "deliver",
            Some("working"),
            "ready_for_acceptance",
            input.expected_revision + 1,
        )?;
        crate::audit::record(
            &tx,
            &task_id,
            &owner,
            "approve",
            Some("ready_for_acceptance"),
            "completed",
            input.expected_revision + 2,
        )?;
        let reply = if matches!(h.runtime, crate::Runtime::Dsh { .. }) {
            final_content.clone()
        } else {
            format!(
                "🤵 管家（已调度「{node_name}」· 归入主题「{summary}」）：\n\n{}",
                final_content
            )
        };
        let placeholder_id: Option<i64> = tx
            .query_row(
                "SELECT id FROM messages
                 WHERE owner=?1 AND id=(SELECT MAX(id) FROM messages WHERE owner=?1)
                   AND role='assistant' AND status='complete'
                   AND (content LIKE '已归入主题「%' OR content LIKE '%完成后结果会自动发回%')",
                [&owner],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(pid) = placeholder_id {
            tx.execute(
                "UPDATE messages SET content=?1, created_at=CURRENT_TIMESTAMP WHERE id=?2",
                params![reply, pid],
            )?;
        } else {
            tx.execute(
                "INSERT INTO messages(owner,role,content,status,created_at)
                 VALUES (?1,'assistant',?2,'complete',CURRENT_TIMESTAMP)",
                params![owner, reply],
            )?;
        }
    } else {
        crate::sagent::on_node_draft_completed(
            &tx,
            &h,
            &task_id,
            &requester,
            &owner,
            &node_id,
            &node_name,
            &summary,
            &final_content,
            input.expected_revision,
            &draft_id,
        )?;
    }
    tx.commit()?;
    drop(db);
    clear_live_progress(&h, &task_id);
    if requester == owner {
        crate::sagent::notify_gitlab_task_completed(&h, &task_id, &node_name, &final_content);
        crate::sagent::spawn_muse_post_completion_distillation(
            h.clone(),
            owner.clone(),
            task_id.clone(),
            instruction.clone(),
            final_content.clone(),
        );
    }
    Ok(Json(json!({
        "status": "completed",
        "request_id": turn_request_id,
        "draft_id": draft_id,
        "seq": seq,
    })))
}

pub(crate) fn hub_origin(headers: &HeaderMap) -> String {
    if let Some(host) = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
    {
        if !host.is_empty()
            && host.len() <= 128
            && host
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b':' | b'[' | b']'))
        {
            return format!("http://{host}");
        }
    }
    "http://10.0.0.11:5176".to_owned()
}

pub(super) async fn agent_script() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/x-python; charset=utf-8")],
        include_str!("../../scripts/agenct-node.py"),
    )
}

pub(super) async fn install_sh(headers: HeaderMap) -> impl IntoResponse {
    let origin = hub_origin(&headers);
    let body = INSTALL_SH_TEMPLATE.replace("__HUB_ORIGIN__", &origin);
    (
        [(header::CONTENT_TYPE, "text/x-shellscript; charset=utf-8")],
        body,
    )
}

pub(super) async fn install_ps1(headers: HeaderMap) -> impl IntoResponse {
    let origin = hub_origin(&headers);
    let body = INSTALL_PS1_TEMPLATE.replace("__HUB_ORIGIN__", &origin);
    ([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], body)
}

const INSTALL_SH_TEMPLATE: &str = r##"#!/usr/bin/env sh
set -eu

HUB="${AGENCT_HUB:-__HUB_ORIGIN__}"
if [ -n "${1:-}" ] && [ -z "${AGENCT_NODE_TOKEN:-}" ]; then
  TOKEN="$1"
  shift
else
  TOKEN="${AGENCT_NODE_TOKEN:-}"
fi

if [ -z "$TOKEN" ]; then
  echo "用法: curl -fsSL $HUB/node/install.sh | sh -s -- <你的节点令牌>" >&2
  exit 2
fi

case "$TOKEN" in
  agn_*) ;;
  *)
    echo "错误: 节点令牌应以 agn_ 开头，请回 Agenct 网页重新复制。" >&2
    exit 2
    ;;
esac
case "$TOKEN" in
  *[!A-Za-z0-9_]*)
    echo "错误: 节点令牌格式无效。" >&2
    exit 2
    ;;
esac

if ! command -v python3 >/dev/null 2>&1; then
  echo "错误: 未找到 python3，请先安装 Python 3。" >&2
  exit 1
fi

DIR="$HOME/.agenct"
mkdir -p "$DIR"
chmod 700 "$DIR"

SCRIPT="$DIR/agenct-node.py"
echo "正在从 $HUB 下载节点程序..."
if command -v curl >/dev/null 2>&1; then
  curl -fsSL "$HUB/node/agenct-node.py" -o "$SCRIPT"
else
  python3 -c 'import sys, urllib.request; urllib.request.urlretrieve(sys.argv[1], sys.argv[2])' "$HUB/node/agenct-node.py" "$SCRIPT"
fi
chmod 700 "$SCRIPT"

START_SH="$DIR/start-node.sh"
cat > "$START_SH" <<'EOF'
#!/usr/bin/env sh
set -eu
DIR="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
EOF
printf 'export AGENCT_NODE_TOKEN=%s\n' "'$TOKEN'" >> "$START_SH"
printf 'export AGENCT_HUB=%s\n' "'$HUB'" >> "$START_SH"
cat >> "$START_SH" <<'EOF'
case "${1:-}" in
  daemon)
    shift
    exec python3 "$DIR/agenct-node.py" --hub "$AGENCT_HUB" --daemon "$@"
    ;;
  status)
    if [ -f "$DIR/node.pid" ] && kill -0 "$(cat "$DIR/node.pid")" 2>/dev/null; then
      echo "Agenct 节点正在后台运行 (PID: $(cat "$DIR/node.pid"))"
      exit 0
    else
      echo "Agenct 节点未在后台运行"
      exit 1
    fi
    ;;
  logs)
    exec tail -n 50 -f "$DIR/node.log"
    ;;
  stop)
    if [ -f "$DIR/node.pid" ]; then
      kill "$(cat "$DIR/node.pid")" 2>/dev/null || true
      rm -f "$DIR/node.pid"
      echo "已停止后台节点"
    fi
    exit 0
    ;;
esac
exec python3 "$DIR/agenct-node.py" --hub "$AGENCT_HUB" "$@"
EOF
chmod 700 "$START_SH"

echo "已保存启动脚本: $START_SH（支持: $START_SH daemon | status | logs | stop）"
exec "$START_SH" "$@"
"##;

const INSTALL_PS1_TEMPLATE: &str = r##"$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8

$hub = if ($env:AGENCT_HUB) { $env:AGENCT_HUB } else { '__HUB_ORIGIN__' }
$token = $env:AGENCT_NODE_TOKEN
if (-not $token -or $token -notmatch '^agn_[0-9a-fA-F_]+$') {
  throw "请先设置 `$env:AGENCT_NODE_TOKEN='agn_...' 后再运行一键安装命令。"
}

$pyExe = $null
$pyArgs = @()
foreach ($cand in @('python', 'python3')) {
  $cmd = Get-Command $cand -ErrorAction SilentlyContinue
  if ($cmd) {
    try {
      $ver = & $cmd.Source -c "import sys; print(sys.version_info[0])" 2>$null
      if ($LASTEXITCODE -eq 0 -and "$ver".Trim() -eq '3') { $pyExe = $cmd.Source; break }
    } catch {}
  }
}
if (-not $pyExe) {
  $pyLauncher = Get-Command 'py' -ErrorAction SilentlyContinue
  if ($pyLauncher) {
    try {
      $ver = & $pyLauncher.Source -3 -c "import sys; print(sys.version_info[0])" 2>$null
      if ($LASTEXITCODE -eq 0 -and "$ver".Trim() -eq '3') { $pyExe = $pyLauncher.Source; $pyArgs = @('-3') }
    } catch {}
  }
}
if (-not $pyExe) {
  throw '未找到 Python 3，请先安装 Python 3（安装时勾选 Add Python to PATH）。'
}

$dir = Join-Path $HOME '.agenct'
New-Item -ItemType Directory -Force -Path $dir | Out-Null
$script = Join-Path $dir 'agenct-node.py'

Write-Host "正在从 $hub 下载节点程序..."
Invoke-WebRequest -UseBasicParsing -Uri "$hub/node/agenct-node.py" -OutFile $script

$startPs1 = Join-Path $dir 'start-node.ps1'
$pyArgsExpr = if ($pyArgs.Count -gt 0) { '@(' + (($pyArgs | ForEach-Object { "'$_'" }) -join ', ') + ')' } else { '@()' }
$pyExeEsc = $pyExe.Replace("'", "''")
$scriptEsc = $script.Replace("'", "''")
$hubEsc = $hub.Replace("'", "''")
$startLines = @(
  "`$ErrorActionPreference = 'Stop'",
  "[Console]::OutputEncoding = [System.Text.Encoding]::UTF8",
  "`$env:AGENCT_NODE_TOKEN = '$token'",
  "`$env:AGENCT_HUB = '$hubEsc'",
  "`$pyExe = '$pyExeEsc'",
  "`$pyArgs = $pyArgsExpr",
  "`$script = '$scriptEsc'",
  "& `$pyExe @pyArgs `$script --hub `$env:AGENCT_HUB @args"
) -join "`r`n"
[System.IO.File]::WriteAllText($startPs1, $startLines, [System.Text.UTF8Encoding]::new($true))

Write-Host "已保存启动脚本: $startPs1（以后可直接运行重连）"
& $pyExe @pyArgs $script --hub $hub
"##;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{app, Config, Runtime, UserConfig};
    use axum::{body::Body, http::Request, Router};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    fn token(who: &str) -> String {
        format!("{who}-{}", "x".repeat(40))
    }

    fn hub() -> Hub {
        let h = Hub::open(Config {
            bind: "127.0.0.1:0".parse().unwrap(),
            database: ":memory:".into(),
            users: ["alice", "bob"]
                .map(|id| UserConfig {
                    id: id.into(),
                    token_sha256: format!("{:x}", Sha256::digest(token(id).as_bytes())),
                })
                .into(),
            runtime: Runtime::Mock,
        })
        .unwrap();
        h.db()
            .unwrap()
            .execute(
                "INSERT INTO tasks(id,requester,owner,summary,status,revision)
                 VALUES ('task-1','alice','bob','Run local benchmark','working',1)",
                [],
            )
            .unwrap();
        h
    }

    async fn call(
        router: &Router,
        bearer: &str,
        method: &str,
        path: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let builder = Request::builder()
            .method(method)
            .header("x-agenct-node-protocol", "2")
            .uri(path)
            .header("authorization", format!("Bearer {bearer}"))
            .header("content-type", "application/json");
        let req = match body {
            Some(v) => builder.body(Body::from(v.to_string())).unwrap(),
            None => builder.body(Body::empty()).unwrap(),
        };
        let resp = router.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    async fn assigned_node(h: &Hub) -> (Router, String) {
        let router = app(h.clone());
        let (status, node) = call(
            &router,
            &token("bob"),
            "POST",
            "/api/nodes",
            Some(json!({"id":"pc","name":"PC"})),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (status, _) = call(
            &router,
            &token("bob"),
            "POST",
            "/api/tasks/task-1/node-assignment",
            Some(json!({"node_id":"pc","expected_revision":1})),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        (router, node["token"].as_str().unwrap().into())
    }

    #[tokio::test]
    async fn exclusive_lease_blocks_duplicate_claims_and_binds_replayed_output() {
        let h = hub();
        let (r, node) = assigned_node(&h).await;
        let (first, second) = tokio::join!(
            call(&r, &node, "GET", "/node/v1/work", None),
            call(&r, &node, "GET", "/node/v1/work", None)
        );
        assert_eq!(first.0, StatusCode::OK);
        assert_eq!(second.0, StatusCode::OK);
        assert_ne!(first.1["job"].is_null(), second.1["job"].is_null());
        let job = if first.1["job"].is_null() {
            &second.1["job"]
        } else {
            &first.1["job"]
        };
        let lease = job["lease_token"].as_str().unwrap();
        assert_eq!(lease.len(), 64);
        let (status, _) = call(
            &r,
            &node,
            "POST",
            "/node/v1/tasks/task-1/lease",
            Some(json!({"lease_token":"wrong","expected_revision":1})),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        let (status, _) = call(
            &r,
            &node,
            "POST",
            "/node/v1/tasks/task-1/lease",
            Some(json!({"lease_token":lease,"expected_revision":1})),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let body = json!({"lease_token":lease,"request_id":"stable","expected_revision":1,"status":"completed","content":"result"});
        let (status, done) = call(
            &r,
            &node,
            "POST",
            "/node/v1/tasks/task-1/drafts",
            Some(body.clone()),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        h.db()
            .unwrap()
            .execute("UPDATE node_assignments SET lease_until=0", [])
            .unwrap();
        let (status, replay) = call(
            &r,
            &node,
            "POST",
            "/node/v1/tasks/task-1/drafts",
            Some(body.clone()),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(replay["draft_id"], done["draft_id"]);
        let mut altered = body;
        altered["content"] = json!("different result");
        assert_eq!(
            call(
                &r,
                &node,
                "POST",
                "/node/v1/tasks/task-1/drafts",
                Some(altered)
            )
            .await
            .0,
            StatusCode::CONFLICT
        );
        assert_eq!(
            h.db()
                .unwrap()
                .query_row("SELECT count(*) FROM deliveries", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            1
        );
        call(
            &r,
            &token("bob"),
            "POST",
            "/api/tasks/task-1/node-assignment",
            Some(json!({"node_id":"pc","expected_revision":1})),
        )
        .await;
        let (_, next) = call(&r, &node, "GET", "/node/v1/work", None).await;
        let collision = json!({"lease_token":next["job"]["lease_token"],"request_id":"stable","expected_revision":1,"status":"completed","content":"result"});
        assert_eq!(
            call(
                &r,
                &node,
                "POST",
                "/node/v1/tasks/task-1/drafts",
                Some(collision)
            )
            .await
            .0,
            StatusCode::CONFLICT
        );
    }

    #[tokio::test]
    async fn expired_lease_never_requeues_and_reassignment_rejects_the_old_executor() {
        let h = hub();
        let (r, node) = assigned_node(&h).await;
        let (_, first) = call(&r, &node, "GET", "/node/v1/work", None).await;
        h.db()
            .unwrap()
            .execute("UPDATE node_assignments SET lease_until=0", [])
            .unwrap();
        let (_, idle) = call(&r, &node, "GET", "/node/v1/work", None).await;
        assert!(idle["job"].is_null());
        assert_eq!(
            h.db()
                .unwrap()
                .query_row("SELECT status FROM node_assignments", [], |r| r
                    .get::<_, String>(0))
                .unwrap(),
            "failed"
        );
        let old = json!({"lease_token":first["job"]["lease_token"],"request_id":"old","expected_revision":1,"status":"completed","content":"old output"});
        assert_eq!(
            call(
                &r,
                &node,
                "POST",
                "/node/v1/tasks/task-1/drafts",
                Some(old.clone())
            )
            .await
            .0,
            StatusCode::CONFLICT
        );
        call(
            &r,
            &token("bob"),
            "POST",
            "/api/tasks/task-1/node-assignment",
            Some(json!({"node_id":"pc","expected_revision":1})),
        )
        .await;
        let (_, next) = call(&r, &node, "GET", "/node/v1/work", None).await;
        assert_ne!(next["job"]["lease_token"], first["job"]["lease_token"]);
        assert_eq!(
            call(&r, &node, "POST", "/node/v1/tasks/task-1/drafts", Some(old))
                .await
                .0,
            StatusCode::CONFLICT
        );
    }

    #[tokio::test]
    async fn old_clients_cannot_claim_and_rotation_invalidates_claimed_work() {
        let h = hub();
        let (r, node) = assigned_node(&h).await;
        let response = r
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/node/v1/work")
                    .header("authorization", format!("Bearer {node}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UPGRADE_REQUIRED);
        assert_eq!(
            h.db()
                .unwrap()
                .query_row("SELECT status FROM node_assignments", [], |r| r
                    .get::<_, String>(0))
                .unwrap(),
            "queued"
        );
        let (_, claimed) = call(&r, &node, "GET", "/node/v1/work", None).await;
        let (_, rotated) = call(&r, &token("bob"), "POST", "/api/nodes/pc/token", None).await;
        let new_token = rotated["token"].as_str().unwrap();
        assert_eq!(
            call(
                &r,
                new_token,
                "POST",
                "/node/v1/tasks/task-1/lease",
                Some(json!({"lease_token":claimed["job"]["lease_token"],"expected_revision":1}))
            )
            .await
            .0,
            StatusCode::CONFLICT
        );
        assert!(call(&r, new_token, "GET", "/node/v1/work", None).await.1["job"].is_null());
    }

    #[tokio::test]
    async fn multi_pc_registration_assignment_work_pull_and_draft_submit() {
        let h = hub();
        let r = app(h.clone());

        let (s, created) = call(
            &r,
            &token("bob"),
            "POST",
            "/api/nodes",
            Some(json!({
                "id": "gpu-box",
                "name": "算力机",
                "capabilities": "RTX 4090 + 本地军团"
            })),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        let node_token = created["token"].as_str().unwrap().to_owned();
        assert!(node_token.starts_with("agn_"));
        assert_eq!(node_token.len(), 68);

        // Duplicate id fails with 409.
        let (s, _) = call(
            &r,
            &token("bob"),
            "POST",
            "/api/nodes",
            Some(json!({ "id": "gpu-box", "name": "dup" })),
        )
        .await;
        assert_eq!(s, StatusCode::CONFLICT);

        // Alice cannot see Bob's node or assign Bob's task.
        let (s, alice_nodes) = call(&r, &token("alice"), "GET", "/api/nodes", None).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(alice_nodes, json!([]));
        let (s, _) = call(
            &r,
            &token("alice"),
            "POST",
            "/api/tasks/task-1/node-assignment",
            Some(json!({ "node_id": "gpu-box", "expected_revision": 1 })),
        )
        .await;
        assert_eq!(s, StatusCode::NOT_FOUND);

        // Bob polls before assignment -> job: null, online flips to true.
        let (s, work) = call(&r, &node_token, "GET", "/node/v1/work", None).await;
        assert_eq!(s, StatusCode::OK);
        assert!(work["job"].is_null());
        let (s, bob_nodes) = call(&r, &token("bob"), "GET", "/api/nodes", None).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(bob_nodes[0]["online"], true);

        // Bob assigns task-1 to gpu-box.
        let (s, assigned) = call(
            &r,
            &token("bob"),
            "POST",
            "/api/tasks/task-1/node-assignment",
            Some(json!({
                "node_id": "gpu-box",
                "expected_revision": 1,
                "instruction": "重点核对吞吐指标"
            })),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(assigned["assigned"]["node_id"], "gpu-box");
        assert_eq!(assigned["assigned"]["status"], "queued");

        // Node pulls the assigned job.
        let (s, work) = call(&r, &node_token, "GET", "/node/v1/work", None).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(work["job"]["task_id"], "task-1");
        assert_eq!(work["job"]["instruction"], "重点核对吞吐指标");

        // User's regular bearer token cannot authenticate to /node/v1/work.
        let (s, _) = call(&r, &token("bob"), "GET", "/node/v1/work", None).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED);

        // Node submits completed draft.
        let (s, submitted) = call(
            &r,
            &node_token,
            "POST",
            "/node/v1/tasks/task-1/drafts",
            Some(json!({
                "lease_token": work["job"]["lease_token"], "request_id": "run-1",
                "expected_revision": 1,
                "status": "completed",
                "content": "来自 gpu-box 本地军团的压测结论"
            })),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        let req_id = submitted["request_id"].as_str().unwrap().to_owned();
        assert_eq!(submitted["seq"], 1);

        // Idempotent replay returns same draft_id and seq.
        let (s, replay) = call(
            &r,
            &node_token,
            "POST",
            "/node/v1/tasks/task-1/drafts",
            Some(json!({
                "lease_token": work["job"]["lease_token"], "request_id": "run-1",
                "expected_revision": 1,
                "status": "completed",
                "content": "来自 gpu-box 本地军团的压测结论"
            })),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(replay["draft_id"], submitted["draft_id"]);

        // Bob's /api/tasks sees my_draft + my_node_assignment; Alice does not.
        let (_, bob_tasks) = call(&r, &token("bob"), "GET", "/api/tasks", None).await;
        assert_eq!(bob_tasks[0]["my_draft"]["status"], "complete");
        assert_eq!(bob_tasks[0]["my_draft"]["request_id"], req_id);
        assert_eq!(bob_tasks[0]["my_node_assignment"]["status"], "completed");
        let (_, alice_tasks) = call(&r, &token("alice"), "GET", "/api/tasks", None).await;
        assert!(alice_tasks[0].get("my_draft").is_none());
        assert!(alice_tasks[0].get("my_node_assignment").is_none());

        // Draft receipt endpoint returns node metadata for draft-ui / task-chat.
        let (s, receipt) = call(
            &r,
            &token("bob"),
            "GET",
            &format!("/api/tasks/task-1/drafts/turns/{req_id}"),
            None,
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(receipt["response"]["draft"]["node_id"], "gpu-box");
        assert_eq!(receipt["response"]["draft"]["node_name"], "算力机");

        // Rotating token immediately invalidates old token.
        let (s, rot) = call(&r, &token("bob"), "POST", "/api/nodes/gpu-box/token", None).await;
        assert_eq!(s, StatusCode::OK);
        let new_token = rot["token"].as_str().unwrap().to_owned();
        assert_ne!(new_token, node_token);
        let (s, _) = call(&r, &node_token, "GET", "/node/v1/work", None).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED);
        let (s, _) = call(&r, &new_token, "GET", "/node/v1/work", None).await;
        assert_eq!(s, StatusCode::OK);
    }

    #[tokio::test]
    async fn optional_id_auto_env_and_installer_endpoints() {
        let h = hub();
        let r = app(h);

        // Omitting id auto-generates pc-1, pc-2 and populates capabilities from X-Agenct-Node-Env.
        let (s, first) = call(
            &r,
            &token("bob"),
            "POST",
            "/api/nodes",
            Some(json!({ "name": "办公主力机" })),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(first["node"]["id"], "pc-1");
        let first_token = first["token"].as_str().unwrap().to_owned();

        let (s, second) = call(
            &r,
            &token("bob"),
            "POST",
            "/api/nodes",
            Some(json!({ "id": "", "name": "家里电脑" })),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(second["node"]["id"], "pc-2");

        let work_req = Request::builder()
            .method("GET")
            .uri("/node/v1/work")
            .header("x-agenct-node-protocol", "2")
            .header("authorization", format!("Bearer {first_token}"))
            .header(
                "x-agenct-node-env",
                "linux/x86_64 | host:devbox | agent:claude",
            )
            .body(Body::empty())
            .unwrap();
        let work_resp = r.clone().oneshot(work_req).await.unwrap();
        assert_eq!(work_resp.status(), StatusCode::OK);
        let (_, nodes) = call(&r, &token("bob"), "GET", "/api/nodes", None).await;
        let pc1 = nodes
            .as_array()
            .unwrap()
            .iter()
            .find(|n| n["id"] == "pc-1")
            .unwrap();
        assert_eq!(
            pc1["capabilities"],
            "linux/x86_64 | host:devbox | agent:claude"
        );

        // Unauthenticated installers and Python agent script are served directly.
        let py_resp = r
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/node/agenct-node.py")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(py_resp.status(), StatusCode::OK);
        let py_body = String::from_utf8(
            py_resp
                .into_body()
                .collect()
                .await
                .unwrap()
                .to_bytes()
                .to_vec(),
        )
        .unwrap();
        assert!(py_body.contains("def detect_agent"));

        let sh_resp = r
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/node/install.sh")
                    .header("host", "10.0.0.11:5176")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(sh_resp.status(), StatusCode::OK);
        let sh_body = String::from_utf8(
            sh_resp
                .into_body()
                .collect()
                .await
                .unwrap()
                .to_bytes()
                .to_vec(),
        )
        .unwrap();
        assert!(sh_body.contains("http://10.0.0.11:5176"));
        assert!(sh_body.contains("/node/agenct-node.py"));

        // Unsafe Host header falls back instead of injecting shell syntax.
        let bad_sh = r
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/node/install.sh")
                    .header("host", "evil.local;id")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bad_body = String::from_utf8(
            bad_sh
                .into_body()
                .collect()
                .await
                .unwrap()
                .to_bytes()
                .to_vec(),
        )
        .unwrap();
        assert!(!bad_body.contains("evil.local;id"));
        assert!(bad_body.contains("http://10.0.0.11:5176"));

        let ps_resp = r
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/node/install.ps1")
                    .header("host", "10.0.0.11:5176")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(ps_resp.status(), StatusCode::OK);
        let ps_body = String::from_utf8(
            ps_resp
                .into_body()
                .collect()
                .await
                .unwrap()
                .to_bytes()
                .to_vec(),
        )
        .unwrap();
        assert!(ps_body.contains("AGENCT_NODE_TOKEN"));
        assert!(ps_body.contains("http://10.0.0.11:5176"));
    }

    #[tokio::test]
    async fn chat_command_dispatches_to_pc_node_and_posts_completion_to_chat() {
        let h = hub();
        let r = app(h);

        let (s, created) = call(
            &r,
            &token("bob"),
            "POST",
            "/api/nodes",
            Some(json!({ "id": "homelab", "name": "办公主力机" })),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        let node_token = created["token"].as_str().unwrap().to_owned();

        // Direct chat command addressing the caller's PC node creates a queued self-task without creating a colleague proposal.
        let (s, chat) = call(
            &r,
            &token("bob"),
            "POST",
            "/api/messages",
            Some(json!({
                "message": "让homelab看看Atlas仓库当前所在分支",
                "request_id": "chat-node-1"
            })),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(chat["source"], "pc-node");
        assert!(chat["proposal"].is_null());
        assert!(chat["reply"]
            .as_str()
            .unwrap()
            .contains("办公主力机（homelab）"));

        // Node picks up the dispatched task.
        let (s, work) = call(&r, &node_token, "GET", "/node/v1/work", None).await;
        assert_eq!(s, StatusCode::OK);
        let job = &work["job"];
        assert_eq!(job["summary"], "📁 atlas 仓库上下文");
        assert_eq!(job["instruction"], "看看Atlas仓库当前所在分支");
        let task_id = job["task_id"].as_str().unwrap().to_owned();

        // Node submits result; Hub auto-completes the self-task and posts reply back into Bob's chat.
        let (s, done) = call(
            &r,
            &node_token,
            "POST",
            &format!("/node/v1/tasks/{task_id}/drafts"),
            Some(json!({
                "lease_token": job["lease_token"], "request_id": "exec-1",
                "expected_revision": job["task_revision"],
                "status": "completed",
                "content": "当前分支：dev"
            })),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(done["status"], "completed");

        // Idempotent retry on a completed self-task also returns 200 OK without duplicating messages.
        let (s, replay) = call(
            &r,
            &node_token,
            "POST",
            &format!("/node/v1/tasks/{task_id}/drafts"),
            Some(json!({
                "lease_token": job["lease_token"], "request_id": "exec-1",
                "expected_revision": job["task_revision"],
                "status": "completed",
                "content": "当前分支：dev"
            })),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(replay["draft_id"], done["draft_id"]);

        let (s, msgs) = call(&r, &token("bob"), "GET", "/api/messages", None).await;
        assert_eq!(s, StatusCode::OK);
        let completion_msgs: Vec<_> = msgs
            .as_array()
            .unwrap()
            .iter()
            .filter(|m| {
                m["role"] == "assistant"
                    && m["content"]
                        .as_str()
                        .unwrap_or("")
                        .contains("当前分支：dev")
            })
            .collect();
        assert_eq!(completion_msgs.len(), 1);

        let (s, detail) = call(
            &r,
            &token("bob"),
            "GET",
            &format!("/api/tasks/{task_id}"),
            None,
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(detail["task"]["status"], "completed");
        assert_eq!(detail["audit"]["legacy_history_incomplete"], false);

        // Self-dispatched PC turns group into canonical topic tasks in /api/tasks & /api/activity.
        let (s, bob_tasks) = call(&r, &token("bob"), "GET", "/api/tasks", None).await;
        assert_eq!(s, StatusCode::OK);
        let topic = bob_tasks
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["id"] == task_id)
            .expect("topic task projected in /api/tasks");
        assert_eq!(topic["is_topic"], true);
        assert_eq!(topic["turn_count"], 1);
        assert_eq!(topic["recent_turns"][0]["answer"], "当前分支：dev");
        let (s, bob_activity) = call(&r, &token("bob"), "GET", "/api/activity", None).await;
        assert_eq!(s, StatusCode::OK);
        let act = bob_activity
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["id"] == task_id)
            .expect("topic projected in /api/activity");
        assert_eq!(act["latest"]["action"], "topic_turn");
    }

    #[tokio::test]
    async fn accepting_incoming_task_auto_assigns_primary_online_node() {
        let h = hub();
        h.db().unwrap().execute("INSERT INTO automation_permissions(owner,revision,auto_accept,auto_deliver,gitlab_execute,gitlab_reply) VALUES ('bob',1,1,0,0,0)",[]).unwrap();
        h.db()
            .unwrap()
            .execute(
                "INSERT INTO tasks(id,requester,owner,summary,status,revision)
                 VALUES ('task-accept','alice','bob','Inspect logs','awaiting_owner',0)",
                [],
            )
            .unwrap();
        let r = app(h);

        let (s, created) = call(
            &r,
            &token("bob"),
            "POST",
            "/api/nodes",
            Some(json!({ "id": "homelab", "name": "办公主力机" })),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        let node_token = created["token"].as_str().unwrap().to_owned();
        // Bring node online via work heartbeat.
        let (s, _) = call(&r, &node_token, "GET", "/node/v1/work", None).await;
        assert_eq!(s, StatusCode::OK);

        // Bob accepts the incoming task -> primary online node is automatically assigned.
        let (s, accepted) = call(
            &r,
            &token("bob"),
            "POST",
            "/api/tasks/task-accept/actions",
            Some(json!({ "action": "accept", "expected_revision": 0 })),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(accepted["status"], "working");

        let (s, work) = call(&r, &node_token, "GET", "/node/v1/work", None).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(work["job"]["task_id"], "task-accept");
        assert_eq!(work["job"]["summary"], "Inspect logs");
    }

    #[tokio::test]
    async fn chat_command_matches_node_name_or_id_anywhere_with_word_boundaries() {
        let h = hub();
        let r = app(h.clone());

        let (s, _) = call(
            &r,
            &token("bob"),
            "POST",
            "/api/nodes",
            Some(json!({ "id": "homelab", "name": "办公主力机" })),
        )
        .await;
        assert_eq!(s, StatusCode::OK);

        // Node name/id inside ordinary phrasing matches directly.
        let (s, mem) = call(
            &r,
            &token("bob"),
            "POST",
            "/api/messages",
            Some(json!({ "message": "查下homelab的内存使用情况" })),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(mem["source"], "pc-node");

        let (s, disk) = call(
            &r,
            &token("bob"),
            "POST",
            "/api/messages",
            Some(json!({ "message": "在办公主力机上看看磁盘" })),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(disk["source"], "pc-node");

        // Partial identifier like "myhomelab2" must NOT match "homelab".
        {
            let db = h.db().unwrap();
            assert!(match_chat_command(&db, "bob", "查下myhomelab2的内存")
                .unwrap()
                .is_none());
        }
    }

    #[tokio::test]
    async fn butler_auto_routes_implicit_local_and_repo_questions_and_skips_memory_or_colleagues() {
        let h = hub();
        let r = app(h);

        let (s, homelab) = call(
            &r,
            &token("bob"),
            "POST",
            "/api/nodes",
            Some(json!({ "id": "homelab", "name": "办公主力机" })),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        let homelab_token = homelab["token"].as_str().unwrap().to_owned();

        let (s, gpu) = call(
            &r,
            &token("bob"),
            "POST",
            "/api/nodes",
            Some(json!({ "id": "gpu-box", "name": "算力机", "capabilities": "RTX 4090 | repos:benthic" })),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        let gpu_token = gpu["token"].as_str().unwrap().to_owned();

        // Bring homelab online with discovered repos; keep gpu-box offline/lower priority unless its repo matches.
        let req = Request::builder()
            .method("GET")
            .uri("/node/v1/work")
            .header("x-agenct-node-protocol", "2")
            .header("authorization", format!("Bearer {homelab_token}"))
            .header(
                "x-agenct-node-env",
                "linux/x86_64 | host:HomeLab | agent:codex | repos:agenct,atlas,orbit",
            )
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            r.clone().oneshot(req).await.unwrap().status(),
            StatusCode::OK
        );

        // 1. Implicit local inspection question WITHOUT naming homelab routes to online homelab.
        let (s, mem) = call(
            &r,
            &token("bob"),
            "POST",
            "/api/messages",
            Some(json!({ "message": "查下内存使用情况" })),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(mem["source"], "pc-node");
        let (s, work1) = call(&r, &homelab_token, "GET", "/node/v1/work", None).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(work1["job"]["summary"], "🖥️ 办公主力机 系统与环境");
        assert_eq!(work1["job"]["instruction"], "查下内存使用情况");

        // 2. Question mentioning repo "Atlas" (without naming homelab) routes to homelab.
        let (s, repo_q) = call(
            &r,
            &token("bob"),
            "POST",
            "/api/messages",
            Some(json!({ "message": "看看Atlas仓库当前所在分支" })),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(repo_q["source"], "pc-node");

        // 3. Question matching gpu-box's repo/GPU routes to gpu-box rather than homelab.
        let (s, gpu_q) = call(
            &r,
            &token("bob"),
            "POST",
            "/api/messages",
            Some(json!({ "message": "在benthic里跑一下4090测试" })),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(gpu_q["source"], "pc-node");
        let (s, gpu_work) = call(&r, &gpu_token, "GET", "/node/v1/work", None).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(gpu_work["job"]["summary"], "📁 benthic 仓库上下文");
        assert_eq!(gpu_work["job"]["instruction"], "在benthic里跑一下4090测试");

        // 4. Memory updates and colleague requests are NOT hijacked to PC nodes.
        let (s, mem_save) = call(
            &r,
            &token("bob"),
            "POST",
            "/api/messages",
            Some(json!({ "message": "记住：我负责支付服务" })),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert!(mem_save.get("source").is_none());

        let (s, colleague) = call(
            &r,
            &token("bob"),
            "POST",
            "/api/messages",
            Some(json!({ "message": "请alice查一下Atlas仓库" })),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert!(colleague.get("source").is_none());
    }

    #[tokio::test]
    async fn muse_topic_indexing_groups_related_turns_and_accumulates_history() {
        let h = hub();
        let r = app(h);

        let (s, homelab) = call(
            &r,
            &token("bob"),
            "POST",
            "/api/nodes",
            Some(json!({ "id": "homelab", "name": "办公主力机" })),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        let homelab_token = homelab["token"].as_str().unwrap().to_owned();

        let req = Request::builder()
            .method("GET")
            .uri("/node/v1/work")
            .header("x-agenct-node-protocol", "2")
            .header("authorization", format!("Bearer {homelab_token}"))
            .header(
                "x-agenct-node-env",
                "linux/x86_64 | host:HomeLab | agent:codex | repos:agenct,atlas,orbit",
            )
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            r.clone().oneshot(req).await.unwrap().status(),
            StatusCode::OK
        );

        // Turn 1: Atlas branch question creates topic "📁 atlas 仓库上下文".
        let (s, _) = call(
            &r,
            &token("bob"),
            "POST",
            "/api/messages",
            Some(json!({ "message": "看看Atlas当前分支" })),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        let (s, w1) = call(&r, &homelab_token, "GET", "/node/v1/work", None).await;
        assert_eq!(s, StatusCode::OK);
        let tid = w1["job"]["task_id"].as_str().unwrap().to_owned();
        assert_eq!(w1["job"]["summary"], "📁 atlas 仓库上下文");
        assert_eq!(w1["job"]["instruction"], "看看Atlas当前分支");

        let (s, _) = call(
            &r,
            &homelab_token,
            "POST",
            &format!("/node/v1/tasks/{tid}/drafts"),
            Some(json!({
                "lease_token": w1["job"]["lease_token"], "request_id": "t1",
                "expected_revision": w1["job"]["task_revision"],
                "status": "completed",
                "content": "当前分支：dev"
            })),
        )
        .await;
        assert_eq!(s, StatusCode::OK);

        // Turn 2: Short follow-up WITHOUT naming atlas or homelab reuses SAME topic task & receives turn 1 context.
        let (s, _) = call(
            &r,
            &token("bob"),
            "POST",
            "/api/messages",
            Some(json!({ "message": "那最近3次commit呢" })),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        let (s, w2) = call(&r, &homelab_token, "GET", "/node/v1/work", None).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(w2["job"]["task_id"], tid);
        assert_eq!(w2["job"]["summary"], "📁 atlas 仓库上下文");
        assert_eq!(w2["job"]["instruction"], "那最近3次commit呢");
        let base_draft = w2["job"]["base_draft"].as_str().unwrap();
        assert!(base_draft.contains("看看Atlas当前分支"));
        assert!(base_draft.contains("当前分支：dev"));

        let (s, _) = call(
            &r,
            &homelab_token,
            "POST",
            &format!("/node/v1/tasks/{tid}/drafts"),
            Some(json!({
                "lease_token": w2["job"]["lease_token"], "request_id": "t2",
                "expected_revision": w2["job"]["task_revision"],
                "status": "completed",
                "content": "abc123 feat: muse topic grouping"
            })),
        )
        .await;
        assert_eq!(s, StatusCode::OK);

        // Turn 3: Switching to a different repo opens its own distinct topic.
        let (s, _) = call(
            &r,
            &token("bob"),
            "POST",
            "/api/messages",
            Some(json!({ "message": "看看orbit最近改动" })),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        let (s, w3) = call(&r, &homelab_token, "GET", "/node/v1/work", None).await;
        assert_eq!(s, StatusCode::OK);
        assert_ne!(w3["job"]["task_id"], tid);
        assert_eq!(w3["job"]["summary"], "📁 orbit 仓库上下文");

        // /api/tasks shows 1 grouped atlas topic with turn_count=2 and complete audit history.
        let (s, bob_tasks) = call(&r, &token("bob"), "GET", "/api/tasks", None).await;
        assert_eq!(s, StatusCode::OK);
        let topic = bob_tasks
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["id"] == tid)
            .unwrap();
        assert_eq!(topic["is_topic"], true);
        assert_eq!(topic["turn_count"], 2);
        assert_eq!(topic["recent_turns"].as_array().unwrap().len(), 2);

        let (s, detail) = call(&r, &token("bob"), "GET", &format!("/api/tasks/{tid}"), None).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(detail["audit"]["legacy_history_incomplete"], false);
    }

    #[tokio::test]
    async fn image_attachment_uploads_routes_to_node_and_delivers_in_work_payload() {
        use base64::{engine::general_purpose::STANDARD, Engine};

        let h = hub();
        let r = app(h);

        let (s, homelab) = call(
            &r,
            &token("bob"),
            "POST",
            "/api/nodes",
            Some(json!({ "id": "homelab", "name": "办公主力机" })),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        let homelab_token = homelab["token"].as_str().unwrap().to_owned();

        // Mark homelab online.
        let (s, _) = call(&r, &homelab_token, "GET", "/node/v1/work", None).await;
        assert_eq!(s, StatusCode::OK);

        // Minimal 1x1 transparent PNG.
        let png_bytes: &[u8] = &[
            0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00,
            0x00, 0x1f, 0x15, 0xc4, 0x89, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x78,
            0x9c, 0x63, 0x00, 0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0d, 0x0a, 0x2d, 0xb4, 0x00,
            0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
        ];
        let png_b64 = STANDARD.encode(png_bytes);
        let (s, uploaded) = call(
            &r,
            &token("bob"),
            "POST",
            "/api/attachments",
            Some(json!({
                "name": "ui.png",
                "mime_type": "image/png",
                "data_base64": png_b64,
            })),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        let att_id = uploaded["id"].as_str().unwrap().to_owned();
        assert!(att_id.starts_with("att_"));

        // Alice (not owner and not shared) cannot read bob's private chat image.
        let (s, _) = call(
            &r,
            &token("alice"),
            "GET",
            &format!("/api/attachments/{att_id}"),
            None,
        )
        .await;
        assert_eq!(s, StatusCode::NOT_FOUND);

        // Bob sends chat message containing image marker -> routes to homelab automatically and populates job.attachments.
        let (s, chat) = call(
            &r,
            &token("bob"),
            "POST",
            "/api/messages",
            Some(json!({
                "message": format!("请帮我查看并分析这张图片\n[图片附件: ui.png (/api/attachments/{att_id})]")
            })),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert!(chat["reply"].as_str().unwrap().contains("图片分析与解答"));

        let (s, work_payload) = call(&r, &homelab_token, "GET", "/node/v1/work", None).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(work_payload["job"]["summary"], "💬 图片分析与解答");
        let job_atts = work_payload["job"]["attachments"].as_array().unwrap();
        assert_eq!(job_atts.len(), 1);
        assert_eq!(job_atts[0]["id"], att_id);
        assert_eq!(job_atts[0]["name"], "ui.png");
        assert_eq!(job_atts[0]["mime_type"], "image/png");
        assert_eq!(job_atts[0]["data_base64"], png_b64);
    }

    #[tokio::test]
    async fn multi_pc_routing_is_sticky_switches_on_another_machine_and_answers_fleet_queries() {
        let h = hub();
        let r = app(h);

        // Register two PCs for bob with identical capabilities.
        let (s, pc1) = call(
            &r,
            &token("bob"),
            "POST",
            "/api/nodes",
            Some(json!({ "id": "homelab", "name": "办公主力机" })),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        let tok1 = pc1["token"].as_str().unwrap().to_owned();

        let (s, pc2) = call(
            &r,
            &token("bob"),
            "POST",
            "/api/nodes",
            Some(json!({ "id": "cc-team", "name": "cc-pro-team" })),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        let tok2 = pc2["token"].as_str().unwrap().to_owned();

        // Both PCs poll /node/v1/work with shared caps; tok2 polls AFTER tok1 (simulating 1s poll jitter).
        for (tok, _name) in [(&tok1, "pc1"), (&tok2, "pc2")] {
            let req = Request::builder()
                .method("GET")
                .uri("/node/v1/work")
                .header("x-agenct-node-protocol", "2")
                .header("authorization", format!("Bearer {tok}"))
                .header(
                    "x-agenct-node-env",
                    "linux/x86_64 | host:HomeLab | agent:codex | repos:agenct,atlas",
                )
                .body(Body::empty())
                .unwrap();
            assert_eq!(
                r.clone().oneshot(req).await.unwrap().status(),
                StatusCode::OK
            );
        }

        // 1. Fleet meta-question is answered directly by the Cloud Butler with both PCs listed.
        let (s, fleet) = call(
            &r,
            &token("bob"),
            "POST",
            "/api/messages",
            Some(json!({ "message": "看下我有几台PC调度" })),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        let fleet_reply = fleet["reply"].as_str().unwrap();
        assert!(fleet_reply.contains("2 台"));
        assert!(fleet_reply.contains("homelab"));
        assert!(fleet_reply.contains("cc-team"));

        // 2. Generic question 1 goes deterministically to registered first node (homelab), NOT flip-flopping on tok2 heartbeat.
        let (s, q1) = call(
            &r,
            &token("bob"),
            "POST",
            "/api/messages",
            Some(json!({ "message": "看看内存情况" })),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert!(q1["reply"].as_str().unwrap().contains("homelab"));
        let (s, w1) = call(&r, &tok1, "GET", "/node/v1/work", None).await;
        assert_eq!(s, StatusCode::OK);
        let tid1 = w1["job"]["task_id"].as_str().unwrap().to_owned();
        let _ = call(
            &r,
            &tok1,
            "POST",
            &format!("/node/v1/tasks/{tid1}/drafts"),
            Some(json!({ "lease_token": w1["job"]["lease_token"], "request_id": "d1", "expected_revision": 1, "status": "completed", "content": "mem 16G" })),
        )
        .await;

        // 3. Same-topic follow-up ("看看磁盘占用") stays locked to homelab even if tok2 just polled later.
        let _ = call(&r, &tok2, "GET", "/node/v1/work", None).await;
        let (s, q2) = call(
            &r,
            &token("bob"),
            "POST",
            "/api/messages",
            Some(json!({ "message": "看看磁盘占用" })),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert!(q2["reply"].as_str().unwrap().contains("homelab"));
        let (s, w2) = call(&r, &tok1, "GET", "/node/v1/work", None).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(w2["job"]["task_id"], tid1);
        let _ = call(
            &r,
            &tok1,
            "POST",
            &format!("/node/v1/tasks/{tid1}/drafts"),
            Some(json!({ "lease_token": w2["job"]["lease_token"], "request_id": "d2", "expected_revision": w2["job"]["task_revision"], "status": "completed", "content": "disk 50%" })),
        )
        .await;

        // 4. Brand-new rule-undefined generic task distributes freely to idle/least-recently-used cc-team
        //    AND inherits global session_context from prior homelab turns.
        let (s, q_new) = call(
            &r,
            &token("bob"),
            "POST",
            "/api/messages",
            Some(json!({ "message": "运行脚本生成一份测试数据" })),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert!(q_new["reply"].as_str().unwrap().contains("cc-team"));
        let (s, w_new) = call(&r, &tok2, "GET", "/node/v1/work", None).await;
        assert_eq!(s, StatusCode::OK);
        assert!(w_new["job"]["session_context"]
            .as_str()
            .unwrap()
            .contains("看看内存情况"));
        let tid_new = w_new["job"]["task_id"].as_str().unwrap().to_owned();
        let _ = call(
            &r,
            &tok2,
            "POST",
            &format!("/node/v1/tasks/{tid_new}/drafts"),
            Some(json!({ "lease_token": w_new["job"]["lease_token"], "request_id": "d3", "expected_revision": w_new["job"]["task_revision"], "status": "completed", "content": "data ready" })),
        )
        .await;

        // 5. Asking about "另外一台机器" deterministically switches back to homelab.
        let (s, q3) = call(
            &r,
            &token("bob"),
            "POST",
            "/api/messages",
            Some(json!({ "message": "看下另外一台机器的内存情况" })),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert!(q3["reply"].as_str().unwrap().contains("homelab"));
        let (s, w3) = call(&r, &tok1, "GET", "/node/v1/work", None).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(w3["job"]["summary"], "🖥️ 办公主力机 系统与环境");

        // 6. Env/process discovery routing: tok2 reports unique proc/repo -> tasks mentioning it route specifically to tok2.
        let env_req = Request::builder()
            .method("GET")
            .uri("/node/v1/work").header("x-agenct-node-protocol", "2")
            .header("authorization", format!("Bearer {tok2}"))
            .header(
                "x-agenct-node-env",
                "linux/x86_64 | host:BuildBox | agent:codex | repos:agenct,unique_app | procs:ollama,redis-server",
            )
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            r.clone().oneshot(env_req).await.unwrap().status(),
            StatusCode::OK
        );

        let (s, q_proc) = call(
            &r,
            &token("bob"),
            "POST",
            "/api/messages",
            Some(json!({ "message": "查看 ollama 服务运行状态" })),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert!(q_proc["reply"].as_str().unwrap().contains("cc-team"));
    }

    #[tokio::test]
    async fn consolidates_fragmented_tmux_routing_and_cron_chat_topics() {
        let h = hub();
        let db = h.db().unwrap();
        db.execute(
            "INSERT INTO tasks (id, requester, owner, summary, status, result, revision)
             VALUES ('t-sys', 'alice', 'alice', '🖥️ homelab 系统与环境', 'completed', 'CPU 正常', 1),
                    ('t-tmux1', 'alice', 'alice', '💬 看下这台PC上tmux有几个窗口', 'completed', '3 个窗口', 1),
                    ('t-route', 'alice', 'alice', '💬 你机器路由错了 去homelab', 'completed', '已切到 homelab', 1),
                    ('t-agy', 'alice', 'alice', '💬 Tmux上有agy的agent 才修改了', 'completed', 'agy 运行中', 1),
                    ('t-cron', 'alice', 'alice', '⏰ 定时任务：每晚汇总 GitLab', 'completed', '已汇总', 1),
                    ('t-cron-msg', 'alice', 'alice', '💬 每天晚上10点汇总一下gitlab上的提交总结给', 'completed', '已设置定时任务', 1)",
            [],
        )
        .unwrap();
        db.execute(
            "INSERT INTO deliveries (id, task_id, seq, author, content, status, task_revision, created_at)
             VALUES ('d-1', 't-sys', 1, 'alice', 'CPU 正常', 'submitted', 1, '2026-11-18 10:00:00'),
                    ('d-2', 't-tmux1', 1, 'alice', '3 个窗口', 'submitted', 1, '2026-11-18 10:05:00'),
                    ('d-3', 't-route', 1, 'alice', '已切到 homelab', 'submitted', 1, '2026-11-18 10:10:00'),
                    ('d-4', 't-agy', 1, 'alice', 'agy 运行中', 'submitted', 1, '2026-11-18 10:15:00')",
            [],
        )
        .unwrap();
        drop(db);

        let r = app(h);
        let (s, tasks) = call(&r, &token("alice"), "GET", "/api/tasks", None).await;
        assert_eq!(s, StatusCode::OK);
        let list = tasks.as_array().unwrap();
        let summaries: Vec<&str> = list.iter().filter_map(|t| t["summary"].as_str()).collect();
        assert!(summaries.contains(&"🖥️ homelab 系统与环境"));
        assert!(summaries.contains(&"⏰ 定时任务：每晚汇总 GitLab"));
        assert!(!summaries.iter().any(|s| s.starts_with("💬 ")));

        let sys_task = list
            .iter()
            .find(|t| t["summary"] == "🖥️ homelab 系统与环境")
            .unwrap();
        assert_eq!(sys_task["turn_count"], 4);
        assert_eq!(sys_task["result"], "agy 运行中");
    }

    #[tokio::test]
    async fn llm_semantic_topic_consolidation_and_routing_merges_arbitrary_fragments() {
        let adapter = axum::Router::new().route(
            "/v1/turn",
            axum::routing::post(|Json(body): Json<Value>| async move {
                let msg = body["message"].as_str().unwrap_or("");
                let reply = if msg.contains("LLM Topic Consolidator") {
                    r#"{"merges":[{"from_id":"t-frag2","into_id":"t-frag1","new_title":"💬 鉴权网关超时排查"}]}"#
                } else if msg.contains("LLM Topic Router") {
                    r#"{"task_id":"t-frag1","title":"💬 鉴权网关超时排查"}"#
                } else {
                    "ok"
                };
                Json(json!({ "reply": reply, "backend": "dsh", "proposal": Value::Null }))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1/turn", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, adapter).await.unwrap() });

        let mut h = hub();
        h.runtime = crate::Runtime::Dsh {
            url,
            token: "test-token".into(),
        };
        let db = h.db().unwrap();
        db.execute(
            "INSERT INTO tasks (id, requester, owner, summary, status, result, revision)
             VALUES ('t-frag1', 'bob', 'bob', '💬 上游证书握手偶发断连', 'completed', 'TLS 1.3 协商异常', 1),
                    ('t-frag2', 'bob', 'bob', '💬 网关偶发 504 原因分析', 'completed', '上游握手超时导致', 1)",
            [],
        )
        .unwrap();
        db.execute(
            "INSERT INTO deliveries (id, task_id, seq, author, content, status, task_revision, created_at)
             VALUES ('d-f1', 't-frag1', 1, 'bob', 'TLS 1.3 协商异常', 'submitted', 1, '2026-11-18 11:00:00'),
                    ('d-f2', 't-frag2', 1, 'bob', '上游握手超时导致', 'submitted', 1, '2026-11-18 11:05:00')",
            [],
        )
        .unwrap();
        drop(db);

        let r = app(h.clone());
        let (s, tasks) = call(&r, &token("bob"), "GET", "/api/tasks", None).await;
        assert_eq!(s, StatusCode::OK);
        let list: Vec<&Value> = tasks
            .as_array()
            .unwrap()
            .iter()
            .filter(|t| t["is_topic"] == true)
            .collect();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0]["id"], "t-frag1");
        assert_eq!(list[0]["summary"], "💬 鉴权网关超时排查");
        assert_eq!(list[0]["turn_count"], 2);

        let (matched_id, suggested_title) =
            llm_resolve_topic_hint(&h, "bob", "homelab", "把重试次数调成 2 次再观察下", None).await;
        assert_eq!(matched_id.as_deref(), Some("t-frag1"));
        assert_eq!(suggested_title.as_deref(), Some("💬 鉴权网关超时排查"));
    }
}
