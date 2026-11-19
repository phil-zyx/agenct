//! 7x24h personal super-agent (`sagent`) orchestration:
//! - Module B: Cross-user `sagent <-> sagent` negotiation, clarification checks,
//!   evidence-pack gatekeeping, and mobile conversational acceptance.
//! - Module A: System-wide live workload & node pulse (`/api/pulse` & chat).
//! - Module C: Personal dispatch rules and 7x24h cloud/node cron jobs.
//! - Module D: External GitLab / IM `@user` webhook ingress (`/webhooks/gitlab`).
use crate::{denied, invalid, nodes, Error, Hub, Result};
use axum::{extract::State, http::HeaderMap, Json};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet},
    io::Write,
    path::Path,
    time::Duration,
};
use uuid::Uuid;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct DispatchRule {
    pub id: String,
    pub owner: String,
    pub keyword: String,
    pub target_node_id: String,
    #[serde(default)]
    pub require_clarification: Option<String>,
    #[serde(default)]
    pub require_human_approval: bool,
    pub created_at: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct CronJob {
    pub id: String,
    pub owner: String,
    pub interval_minutes: u64,
    #[serde(default)]
    pub daily_time_cst: Option<String>,
    pub instruction: String,
    #[serde(default = "default_target")]
    pub target: String,
    #[serde(default)]
    pub last_run_unix: Option<i64>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub created_at: i64,
}

fn default_target() -> String {
    "auto".to_owned()
}

fn default_true() -> bool {
    true
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct GitLabUserConfig {
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub gitlab_username: String,
    #[serde(default)]
    pub token: String,
    #[serde(default)]
    pub auto_poll_todos: bool,
    #[serde(default)]
    pub seen_todo_ids: Vec<i64>,
    #[serde(default)]
    pub updated_at: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct GitLabReplyTarget {
    pub owner: String,
    pub base_url: String,
    pub project_path: String,
    pub noteable_type: String,
    pub noteable_iid: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct ImWebhookConfig {
    #[serde(default = "default_im_provider")]
    pub provider: String, // "feishu" | "wecom" | "dingtalk" | "generic"
    #[serde(default)]
    pub webhook_url: String,
    #[serde(default = "default_true")]
    pub notify_takeover: bool,
    #[serde(default = "default_true")]
    pub notify_delegation: bool,
    #[serde(default = "default_true")]
    pub notify_delivery: bool,
    #[serde(default)]
    pub updated_at: i64,
}

fn default_im_provider() -> String {
    "feishu".to_owned()
}

impl Default for ImWebhookConfig {
    fn default() -> Self {
        Self {
            provider: default_im_provider(),
            webhook_url: String::new(),
            notify_takeover: true,
            notify_delegation: true,
            notify_delivery: true,
            updated_at: 0,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct MuseProfileConfig {
    #[serde(default = "default_muse_name")]
    pub name: String,
    #[serde(default = "default_soul_md")]
    pub soul_md: String,
    #[serde(default = "default_memory_md")]
    pub memory_md: String,
    #[serde(default)]
    pub updated_at: i64,
}

fn default_muse_name() -> String {
    "Agenct".to_owned()
}

fn default_soul_md() -> String {
    "# SOUL.md · 核心人设与行事准则\n- 身份：我是你的 7×24h 个人超级智能体 Agenct。\n- 沟通风格：干练、直接、结论先行，绝不说生硬的客服套话。\n- 自主性准则：能直接调用工具或联动本机节点解决的事务一步到位；涉及生产高危操作或跨人承诺时主动请示。".to_owned()
}

fn default_memory_md() -> String {
    "# MEMORY.md · 长期记忆与环境事实\n- 主力开发节点：办公主力机（homelab），常驻 tmux 会话与本地开发 Agent（agy / codex）。\n- 汇报偏好：GitLab 每日定时提交汇总默认采用精简速览格式，按项目提炼 1~2 条核心进展。".to_owned()
}

impl Default for MuseProfileConfig {
    fn default() -> Self {
        Self {
            name: default_muse_name(),
            soul_md: default_soul_md(),
            memory_md: default_memory_md(),
            updated_at: 0,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct MuseAuditRecord {
    pub id: String,
    pub owner: String,
    pub action: String,
    pub status: String,
    pub title: String,
    pub detail: String,
    pub created_at: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct MuseLedgerEntry {
    pub id: String,
    pub owner: String,
    pub title: String,
    /// "active" | "muted" | "completed"
    #[serde(default = "default_ledger_status")]
    pub status: String,
    /// Structured notes / rules / latest progress / numbered records
    pub notes: String,
    pub updated_at: i64,
}

fn default_ledger_status() -> String {
    "active".to_owned()
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct AutomationStore {
    #[serde(default)]
    pub rules: Vec<DispatchRule>,
    #[serde(default)]
    pub crons: Vec<CronJob>,
    #[serde(default)]
    pub managed_tasks: HashSet<String>,
    #[serde(default)]
    pub gitlab_configs: HashMap<String, GitLabUserConfig>,
    #[serde(default)]
    pub gitlab_targets: HashMap<String, GitLabReplyTarget>,
    #[serde(default)]
    pub im_configs: HashMap<String, ImWebhookConfig>,
    #[serde(default)]
    pub muse_profiles: HashMap<String, MuseProfileConfig>,
    #[serde(default)]
    pub topic_outcomes: HashMap<String, String>,
    #[serde(default)]
    pub audit_logs: Vec<MuseAuditRecord>,
    #[serde(default)]
    pub ledgers: Vec<MuseLedgerEntry>,
}

pub(crate) fn record_muse_audit(
    h: &Hub,
    owner: &str,
    action: &str,
    status: &str,
    title: &str,
    detail: &str,
) {
    let _ = with_store_mut(h, |store| {
        store.audit_logs.push(MuseAuditRecord {
            id: format!("aud-{}", &Uuid::new_v4().simple().to_string()[..8]),
            owner: owner.to_owned(),
            action: action.to_owned(),
            status: status.to_owned(),
            title: nodes::truncate_bytes(title.trim(), 120),
            detail: nodes::truncate_bytes(detail.trim(), 300),
            created_at: nodes::now_unix(),
        });
        if store.audit_logs.len() > 120 {
            let drain = store.audit_logs.len() - 120;
            store.audit_logs.drain(0..drain);
        }
    });
}

pub(crate) fn persist_automation_db(db: &Connection, store: &AutomationStore) {
    if let Ok(state_json) = serde_json::to_string(store) {
        let _ = db.execute(
            "INSERT OR REPLACE INTO automation_state(id, state_json, updated_at)
             VALUES (1, ?1, ?2)",
            params![state_json, nodes::now_unix()],
        );
    }
}

pub(crate) fn load_automation_merged(db: &Connection, path: Option<&Path>) -> AutomationStore {
    let from_db: Option<AutomationStore> = db
        .query_row(
            "SELECT state_json FROM automation_state WHERE id = 1",
            [],
            |r| r.get::<_, String>(0),
        )
        .optional()
        .ok()
        .flatten()
        .and_then(|raw| serde_json::from_str(&raw).ok());
    let from_file: Option<AutomationStore> = path
        .and_then(|p| std::fs::read(p).ok())
        .and_then(|bytes| serde_json::from_slice(&bytes).ok());
    let mut store = match (from_file, from_db) {
        (Some(file_store), _) => file_store,
        (None, Some(db_store)) => db_store,
        (None, None) => AutomationStore::default(),
    };
    for prof in store.muse_profiles.values_mut() {
        if prof.name == "Muse" {
            prof.name = default_muse_name();
        }
        if prof.soul_md.contains("个人超级智能体 Muse。") {
            prof.soul_md = prof
                .soul_md
                .replace("个人超级智能体 Muse。", "个人超级智能体 Agenct。");
        }
    }
    persist_automation_db(db, &store);
    store
}

fn persist_automation(h: &Hub, store: &AutomationStore) {
    if let Ok(db) = h.db.try_lock() {
        persist_automation_db(&db, store);
    }
    let Some(path) = &h.automation_path else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let Ok(bytes) = serde_json::to_vec_pretty(store) else {
        return;
    };
    let tmp = path.with_extension("json.tmp");
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    if let Ok(mut fh) = opts.open(&tmp) {
        if fh.write_all(&bytes).is_ok() {
            let _ = std::fs::rename(&tmp, path);
        }
    }
}

fn with_store_mut<R>(h: &Hub, f: impl FnOnce(&mut AutomationStore) -> R) -> Result<R> {
    let mut guard = h.automation.lock().map_err(|_| {
        Error(
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "automation lock error",
        )
    })?;
    let res = f(&mut guard);
    persist_automation(h, &guard);
    Ok(res)
}

pub(crate) fn match_dispatch_rule(h: &Hub, owner: &str, message: &str) -> Option<DispatchRule> {
    let guard = h.automation.lock().ok()?;
    guard
        .rules
        .iter()
        .filter(|r| r.owner == owner && !r.target_node_id.is_empty())
        .find(|r| rule_keyword_matches(&r.keyword, message))
        .cloned()
}

fn semantic_synonyms(term: &str) -> &'static [&'static str] {
    match term {
        "前端" | "ui" | "页面" | "界面" => &[
            "前端", "ui", "页面", "界面", "样式", "组件", "css", "布局", "按钮", "弹窗",
        ],
        "后端" | "服务端" | "接口" | "api" => {
            &["后端", "服务端", "接口", "api", "rust", "hub", "路由"]
        }
        "数据库" | "db" | "sql" => &[
            "数据库",
            "db",
            "sql",
            "sqlite",
            "postgres",
            "mysql",
            "表结构",
            "迁移",
        ],
        "生产" | "线上" | "prod" => &[
            "生产",
            "线上",
            "prod",
            "production",
            "正式环境",
            "主干",
            "master",
        ],
        "生产数据库" => &[
            "生产数据库",
            "线上数据库",
            "正式库",
            "生产库",
            "prod db",
            "删库",
            "drop table",
        ],
        "合同" | "报价" | "商务" => &[
            "合同", "报价", "商务", "法务", "打款", "付款", "采购", "协议",
        ],
        _ => &[],
    }
}

fn rule_keyword_matches(rule_kw: &str, text: &str) -> bool {
    let kw = rule_kw.trim();
    if kw.is_empty() {
        return false;
    }
    if kw == "*" {
        return true;
    }
    let lower = text.to_ascii_lowercase();
    kw.split(['/', '、', '或', ',', '，', '|'])
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .any(|part| {
            let p_low = part.to_ascii_lowercase();
            if lower.contains(&p_low) {
                return true;
            }
            semantic_synonyms(&p_low)
                .iter()
                .any(|syn| lower.contains(syn))
        })
}

fn find_peer_task_for_requester(
    db: &Connection,
    requester: &str,
    status: &str,
    text: &str,
) -> Result<Option<(String, String, String, i64)>> {
    let mut stmt = db.prepare(
        "SELECT id, owner, summary, revision FROM tasks
         WHERE requester=?1 AND owner!=?1 AND status=?2
         ORDER BY rowid DESC LIMIT 20",
    )?;
    let rows = stmt
        .query_map(params![requester, status], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let lower = text.to_ascii_lowercase();
    if let Some(matched) = rows.iter().find(|(id, own, sum, _)| {
        let sid = short_id(id).to_ascii_lowercase();
        (sid.len() >= 6 && lower.contains(&sid))
            || nodes::contains_alias(text, own)
            || sum
                .chars()
                .collect::<Vec<_>>()
                .windows(2)
                .any(|w| text.contains(&w.iter().collect::<String>()))
    }) {
        return Ok(Some(matched.clone()));
    }
    Ok(rows.into_iter().next())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HumanTakeoverReason {
    pub layer: &'static str,
    pub reason: String,
}

/// Evaluates Layer 1 (Built-in System Hard Guardrails) + Layer 2 (Per-User `DispatchRule`s)
/// to determine whether an incoming message/task MUST be escalated to the human owner.
pub(crate) fn evaluate_human_takeover_policy(
    h: &Hub,
    owner: &str,
    text: &str,
) -> Option<HumanTakeoverReason> {
    let lower = text.to_ascii_lowercase();
    // Layer 1A: Explicit request for the human owner (including natural-language variations)
    if [
        "找你本人",
        "你本人",
        "真人",
        "亲自",
        "别让管家",
        "不要管家",
        "找本人",
        "本人回",
        "急事找你",
        "别用机器人",
        "不要机器人",
        "别让ai",
        "别让agent",
        "不要自动回复",
        "找活人",
        "电话沟通",
        "语音聊",
        "当面聊",
    ]
    .iter()
    .any(|cue| text.contains(cue) || lower.contains(cue))
    {
        return Some(HumanTakeoverReason {
            layer: "system-base",
            reason: "对方显式要求你本人直接沟通".to_owned(),
        });
    }

    // Layer 1B: Schedule commitments, deadlines, or high-risk production actions
    if [
        "能发版吗",
        "能不能发版",
        "几点发版",
        "几点上线",
        "什么时候上线",
        "今天能上线吗",
        "今晚能发吗",
        "什么时候能好",
        "什么时候交付",
        "几点能好",
        "多久能交",
        "保证完成",
        "立个军令状",
        "回滚生产",
        "删库",
        "线上回滚",
        "合入master",
        "合并到master",
        "合并进主干",
        "合到主分支",
        "推到主干",
        "直接发生产",
        "drop table",
        "truncate table",
        "rm -rf",
        "打款",
        "批预算",
        "签合同",
    ]
    .iter()
    .any(|cue| text.contains(cue) || lower.contains(cue))
    {
        return Some(HumanTakeoverReason {
            layer: "system-base",
            reason: "涉及排期承诺或高危生产决策（系统底线需本人拍板）".to_owned(),
        });
    }

    // Layer 2: Per-user custom human-approval rules
    let Ok(guard) = h.automation.lock() else {
        return Some(HumanTakeoverReason {
            layer: "system-base",
            reason: "规则状态锁异常，默认转交本人确认".to_owned(),
        });
    };
    for r in guard
        .rules
        .iter()
        .filter(|r| r.owner == owner && r.require_human_approval)
    {
        if rule_keyword_matches(&r.keyword, text) {
            return Some(HumanTakeoverReason {
                layer: "user-rule",
                reason: format!(
                    "命中个人规则 `{}`（关键词 `{}` 需本人确认）",
                    r.id, r.keyword
                ),
            });
        }
    }
    None
}

fn has_rules_for_owner(h: &Hub, owner: &str) -> bool {
    h.automation
        .lock()
        .map(|g| g.rules.iter().any(|r| r.owner == owner))
        .unwrap_or(false)
}

pub(super) fn requires_human_approval(h: &Hub, owner: &str, summary: &str) -> bool {
    if h.automation.lock().is_err() {
        return true;
    }
    evaluate_human_takeover_policy(h, owner, summary).is_some()
}

fn mark_task_managed(h: &Hub, tx: &Connection, task_id: &str) -> Result<()> {
    let res = with_store_mut(h, |store| {
        store.managed_tasks.insert(task_id.to_owned());
    });
    if let Ok(guard) = h.automation.lock() {
        persist_automation_db(tx, &guard);
    }
    res
}

fn is_task_managed(h: &Hub, task_id: &str) -> bool {
    h.automation
        .lock()
        .map(|g| g.managed_tasks.contains(task_id))
        .unwrap_or(false)
}

fn short_id(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

fn strip_prefix_any<'a>(text: &'a str, prefixes: &[&str]) -> Option<&'a str> {
    for p in prefixes {
        if let Some(rest) = text.strip_prefix(p) {
            return Some(rest.trim());
        }
    }
    None
}

fn next_chat_msg_target<'a>(owner: &'a str, requester: &'a str) -> (&'a str, &'a str) {
    // Contact messages table has CHECK(sender != recipient).
    (owner, requester)
}

fn insert_contact_assistant_msg(
    tx: &Connection,
    sender: &str,
    recipient: &str,
    body: &str,
) -> Result<()> {
    if sender == recipient {
        return Ok(());
    }
    let req_id = format!("sagent-{}", Uuid::new_v4().simple());
    tx.execute(
        "INSERT INTO contact_messages(sender,recipient,author_kind,body,request_id,created_at)
         VALUES (?1,?2,'assistant',?3,?4,CURRENT_TIMESTAMP)",
        params![sender, recipient, body, req_id],
    )?;
    let msg_id = tx.last_insert_rowid();
    tx.execute(
        "INSERT INTO contact_reads(owner,contact,through_id) VALUES (?1,?2,?3)
         ON CONFLICT(owner,contact) DO UPDATE SET through_id=MAX(through_id,excluded.through_id)",
        params![recipient, sender, msg_id],
    )?;
    Ok(())
}

fn insert_chat_message(tx: &Connection, owner: &str, role: &str, content: &str) -> Result<i64> {
    tx.execute(
        "INSERT INTO messages(owner,role,content,status,created_at)
         VALUES (?1,?2,?3,'complete',CURRENT_TIMESTAMP)",
        params![owner, role, content],
    )?;
    Ok(tx.last_insert_rowid())
}

fn complete_chat_reply(
    h: &Hub,
    owner: &str,
    input: &crate::messages::ChatInput,
    runtime_profile: &crate::profiles::RuntimeProfile,
    task_context: Option<&Value>,
    reply: &str,
) -> Result<Json<Value>> {
    let mut db = h.db()?;
    let tx = db.transaction()?;
    let msg_id = insert_chat_message(&tx, owner, "user", &input.message)?;
    insert_chat_message(&tx, owner, "assistant", reply)?;
    if let Some(key) = input.request_id.as_deref() {
        tx.execute(
            "UPDATE chat_turns SET message_id=?3 WHERE owner=?1 AND request_id=?2 AND status='pending'",
            params![owner, key, msg_id],
        )?;
    }
    let mut response = json!({
        "reply": reply,
        "backend": h.runtime.label(),
        "source": "sagent",
        "proposal": Value::Null,
        "runtime_profile": runtime_profile,
        "task_context": task_context.cloned().unwrap_or(Value::Null),
        "repository_context": Value::Null,
    });
    crate::chat_turns::complete(&tx, owner, input.request_id.as_deref(), &mut response)?;
    tx.commit()?;
    Ok(Json(response))
}

fn requirement_clause_satisfied(tok: &str, combined: &str, has_attachment: bool) -> bool {
    let lower_tok = tok.to_ascii_lowercase();
    let lower_comb = combined.to_ascii_lowercase();
    if combined.contains(tok) || lower_comb.contains(&lower_tok) {
        return true;
    }
    if ["截图", "图片", "画面", "录屏", "screenshot", "image"]
        .iter()
        .any(|k| lower_tok.contains(k))
    {
        return has_attachment;
    }
    if ["复现", "步骤", "重现", "操作"]
        .iter()
        .any(|k| lower_tok.contains(k))
    {
        return [
            "复现",
            "重现",
            "步骤",
            "第一步",
            "1.",
            "1、",
            "先",
            "点击",
            "打开",
            "输入",
            "访问",
            "操作",
            "when ",
            "steps",
        ]
        .iter()
        .any(|cue| lower_comb.contains(cue));
    }
    if ["日志", "报错", "错误", "异常", "log", "error"]
        .iter()
        .any(|k| lower_tok.contains(k))
    {
        return [
            "日志",
            "报错",
            "错误",
            "异常",
            "失败",
            "error",
            "exception",
            "panic",
            "trace",
            "500",
            "404",
            "400",
            "```",
        ]
        .iter()
        .any(|cue| lower_comb.contains(cue));
    }
    if ["环境", "版本", "分支", "链接", "地址"]
        .iter()
        .any(|k| lower_tok.contains(k))
    {
        return [
            "环境", "版本", "分支", "测试", "生产", "线上", "本地", "staging", "prod", "master",
            "main", "http://", "https://",
        ]
        .iter()
        .any(|cue| lower_comb.contains(cue));
    }
    false
}

fn requirement_missing_reason(
    h: &Hub,
    tx: &Connection,
    owner: &str,
    summary: &str,
    details: Option<&str>,
) -> Result<Option<String>> {
    let combined = format!("{summary}\n{}", details.unwrap_or(""));
    let has_attachment =
        combined.contains("/api/attachments/att_") || combined.contains("[图片附件:");

    let mut reqs: Vec<String> = Vec::new();
    if let Ok(guard) = h.automation.lock() {
        for r in &guard.rules {
            if r.owner == owner && rule_keyword_matches(&r.keyword, &combined) {
                if let Some(req) = &r.require_clarification {
                    let trimmed = req.trim();
                    if !trimmed.is_empty() {
                        reqs.push(trimmed.to_owned());
                    }
                }
            }
        }
    }
    let collab = crate::collaboration::read(tx, owner)?;
    let pub_req = collab.public.request_requirements.trim();
    if !pub_req.is_empty() && !reqs.iter().any(|r| r == pub_req) {
        reqs.push(pub_req.to_owned());
    }

    for req in reqs {
        let needs_image = ["截图", "图片", "画面", "screenshot", "image"]
            .iter()
            .any(|k| req.to_ascii_lowercase().contains(k));
        if needs_image && !has_attachment {
            return Ok(Some(req));
        }
        // Check if any meaningful clause from requirement is already satisfied in text or semantically.
        let tokens: Vec<&str> = req
            .split(|c: char| {
                c.is_whitespace() || matches!(c, ',' | '，' | '、' | ';' | '；' | '+' | '/')
            })
            .map(str::trim)
            .filter(|t| t.chars().count() >= 2)
            .collect();
        if !tokens.is_empty()
            && !tokens
                .iter()
                .any(|tok| requirement_clause_satisfied(tok, &combined, has_attachment))
        {
            return Ok(Some(req));
        }
    }
    Ok(None)
}

/// Module B (Step 1): When a delegated task is created (`requester -> owner`),
/// `owner`'s `sagent` inspects clarification requirements and automatically either
/// asks `requester`'s `sagent` to clarify, OR accepts and dispatches to `owner`'s PC node.
pub(crate) fn on_task_delegated(
    tx: &Connection,
    h: &Hub,
    task_id: &str,
    requester: &str,
    owner: &str,
    summary: &str,
    details: Option<&str>,
) -> Result<()> {
    if requester == owner || !crate::automation_policy::read(tx, owner)?.auto_accept {
        return Ok(());
    }
    let has_node: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM user_nodes WHERE owner=?1)",
        [owner],
        |r| r.get(0),
    )?;
    if !has_node && !has_rules_for_owner(h, owner) {
        return Ok(());
    }
    mark_task_managed(h, tx, task_id)?;

    if let Some(takeover) = evaluate_human_takeover_policy(h, owner, summary) {
        let notice = format!(
            "🚨 **[需你本人接手 · {}]**：收到 {requester} 委派的任务「{summary}」（#{}）。管家未自动接手，请在右侧「任务」点击「接手」或直接 `@{requester}` 回复。",
            takeover.reason,
            short_id(task_id)
        );
        insert_chat_message(tx, owner, "assistant", &notice)?;
        let req_msg = format!(
            "🤝 [{owner} 的管家 → 你的管家] 已收到委派任务「{summary}」（#{}），因「{}」需由 {owner} 本人确认，已置顶通知本人接手。",
            short_id(task_id),
            takeover.reason
        );
        insert_contact_assistant_msg(tx, owner, requester, &req_msg)?;
        insert_chat_message(tx, requester, "assistant", &req_msg)?;
        notify_im_card(
            h,
            owner,
            "takeover",
            &format!("🚨 需你本人接手（#{}）", short_id(task_id)),
            &format!(
                "**发起人**：`{requester}`\n**任务**：「{summary}」\n**原因**：{}",
                takeover.reason
            ),
        );
        return Ok(());
    }

    if let Some(missing) = requirement_missing_reason(h, tx, owner, summary, details)? {
        let ask = format!(
            "🤝 [{owner} 的管家 → 你的管家] 收到委派任务「{summary}」（#{}），但按 {owner} 的协作要求，开始前需补充核对：{missing}。\n\n请直接回复「补充：你的说明（支持粘贴截图）」，我收到后会立刻转交 {owner} 的管家启动执行。",
            short_id(task_id)
        );
        insert_contact_assistant_msg(tx, owner, requester, &ask)?;
        insert_chat_message(tx, requester, "assistant", &ask)?;

        let owner_note = format!(
            "🤵 管家：收到 {requester} 委派的「{summary}」（#{}），发现缺少「{missing}」，已自动联系 {requester} 的管家补充核对，信息补齐后会自动派发节点处理。",
            short_id(task_id)
        );
        insert_chat_message(tx, owner, "assistant", &owner_note)?;
        notify_im_card(
            h,
            requester,
            "delegation",
            &format!("📋 管家追问补充材料（#{}）", short_id(task_id)),
            &format!("**接收人**：`{owner}`\n**任务**：「{summary}」\n**需补充**：{missing}"),
        );
        return Ok(());
    }

    notify_im_card(
        h,
        owner,
        "delegation",
        &format!("🤝 新委派已由管家自动接手（#{}）", short_id(task_id)),
        &format!("**发起人**：`{requester}`\n**任务**：「{summary}」"),
    );
    accept_and_assign_owner_node(tx, h, task_id, 0, requester, owner, summary, details)
}

fn accept_and_assign_owner_node(
    tx: &Connection,
    h: &Hub,
    task_id: &str,
    current_rev: i64,
    requester: &str,
    owner: &str,
    summary: &str,
    instruction: Option<&str>,
) -> Result<()> {
    if !crate::automation_policy::read(tx, owner)?.auto_accept
        || requires_human_approval(h, owner, summary)
    {
        return Err(invalid("automatic acceptance is not authorized"));
    }
    let next_rev = current_rev + 1;
    tx.execute(
        "UPDATE tasks SET status='working', revision=?2 WHERE id=?1",
        params![task_id, next_rev],
    )?;
    crate::audit::record(
        tx,
        task_id,
        owner,
        "accept",
        Some("awaiting_owner"),
        "working",
        next_rev,
    )?;

    let combined = match instruction {
        Some(extra) if !extra.trim().is_empty() => format!("{summary}\n{extra}"),
        _ => summary.to_owned(),
    };
    let node_picked = nodes::select_node_with_rule(h, tx, owner, &combined)?;
    let instr = nodes::truncate_bytes(instruction.unwrap_or("").trim(), 4000);
    let now = nodes::now_unix();

    let (dispatch_note_req, dispatch_note_owner) = if let Some((node_id, node_name, online, _)) =
        node_picked
    {
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
               draft_request_id=NULL,
               updated_at=excluded.updated_at",
            params![task_id, owner, node_id, next_rev, instr, now],
        )?;
        tx.execute(
            "UPDATE node_assignments SET authorization='delegation' WHERE task_id=?1",
            [task_id],
        )?;
        let state_hint = if online {
            "节点正在自动执行"
        } else {
            "节点当前离线，上线后将自动拉取执行"
        };
        (
            format!(
                "🤝 [{owner} 的管家 → 你的管家] 信息已齐全，我已自动接手任务「{summary}」（#{}），并按规则调度至 {owner} 的 PC 节点「{node_name}（{node_id}）」执行（{state_hint}），完成后将回传结果与验收证据。",
                short_id(task_id)
            ),
            format!(
                "🤵 管家：收到 {requester} 委派的「{summary}」（#{}），信息核对无误，已自动接手并调度至你的节点「{node_name}（{node_id}）」（{state_hint}）。",
                short_id(task_id)
            ),
        )
    } else {
        (
            format!(
                "🤝 [{owner} 的管家 → 你的管家] 我已代 {owner} 接手任务「{summary}」（#{}），当前无可用资源 PC，已通知 {owner} 在端上确认处理。",
                short_id(task_id)
            ),
            format!(
                "🤵 管家（需你关注）：收到 {requester} 委派的「{summary}」（#{}），已为你接手但暂无匹配的 PC 节点执行，请打开任务查看或补充规则。",
                short_id(task_id)
            ),
        )
    };

    insert_contact_assistant_msg(tx, owner, requester, &dispatch_note_req)?;
    insert_chat_message(tx, requester, "assistant", &dispatch_note_req)?;
    insert_chat_message(tx, owner, "assistant", &dispatch_note_owner)?;
    Ok(())
}

fn is_ui_verification_task(summary: &str) -> bool {
    let lower = summary.to_ascii_lowercase();
    [
        "界面", "页面", "样式", "按钮", "弹窗", "布局", "截图", "前端", "ui", "css", "组件",
    ]
    .iter()
    .any(|kw| summary.contains(kw) || lower.contains(kw))
}

fn has_evidence_pack(content: &str) -> bool {
    if content.contains("/api/attachments/att_") || content.contains("[图片附件:") {
        return true;
    }
    let lower = content.to_ascii_lowercase();
    ["验收证据", "验证", "测试", "pass", "screenshot", "diff"]
        .iter()
        .any(|kw| content.contains(kw) || lower.contains(kw))
}

/// Module B (Step 2): Called when `owner`'s local PC node submits a completed draft
/// for a cross-user task (`requester != owner`). When managed by `sagent`, checks
/// the Evidence Pack (screenshot/verification) and auto-delivers to `requester`.
pub(crate) fn on_node_draft_completed(
    tx: &Connection,
    h: &Hub,
    task_id: &str,
    requester: &str,
    owner: &str,
    node_id: &str,
    node_name: &str,
    summary: &str,
    content: &str,
    task_revision: i64,
    draft_id: &str,
) -> Result<()> {
    if requester == owner
        || !is_task_managed(h, task_id)
        || !crate::automation_policy::read(tx, owner)?.auto_deliver
    {
        return Ok(());
    }
    if is_ui_verification_task(summary) && !has_evidence_pack(content) {
        let hold_msg = format!(
            "🤵 管家（需你审核）：节点「{node_name}（{node_id}）」已完成界面相关任务「{summary}」（#{}）初稿，但未附带界面截图或验证说明。\n为保证 {requester} 能在手机上顺利验收，请检查右侧任务草稿并补充截图或验证结论后再手动交付。",
            short_id(task_id)
        );
        insert_chat_message(tx, owner, "assistant", &hold_msg)?;
        return Ok(());
    }

    let next_rev = task_revision + 1;
    tx.execute(
        "INSERT INTO deliveries(id,task_id,seq,author,content,status,task_revision,source_draft_id)
         SELECT ?1,?2,COALESCE(MAX(seq),0)+1,?3,?4,'submitted',?5,?6
         FROM deliveries WHERE task_id=?2 AND status='submitted'",
        params![
            Uuid::new_v4().to_string(),
            task_id,
            owner,
            content,
            next_rev,
            draft_id,
        ],
    )?;
    tx.execute(
        "UPDATE tasks SET status='ready_for_acceptance', result=?2, revision=?3 WHERE id=?1",
        params![task_id, content, next_rev],
    )?;
    crate::audit::record(
        tx,
        task_id,
        owner,
        "deliver",
        Some("working"),
        "ready_for_acceptance",
        next_rev,
    )?;

    let req_msg = format!(
        "🤝 [{owner} 的管家 → 你的管家] 任务「{summary}」（#{}）已由节点「{node_name}」完成，已按授权自动交付【验收证据包】（尚未经独立核验）：\n\n{content}\n\n---\n💡 请打开这项任务核对交付物，再验收或退回。",
        short_id(task_id)
    );
    insert_contact_assistant_msg(tx, owner, requester, &req_msg)?;
    insert_chat_message(tx, requester, "assistant", &req_msg)?;

    let owner_msg = format!(
        "🤵 管家：节点「{node_name}」已完成 {requester} 委派的「{summary}」（#{}），已按你的授权自动交付，证据尚未经独立核验，等待 {requester} 验收。",
        short_id(task_id)
    );
    insert_chat_message(tx, owner, "assistant", &owner_msg)?;
    notify_im_card(
        h,
        requester,
        "delivery",
        &format!("✅ 收到待验收交付（#{}）", short_id(task_id)),
        &format!(
            "**执行方**：`{owner}`（节点 `{node_name}`）\n**任务**：「{summary}」\n**摘要**：{}",
            nodes::truncate_bytes(content.trim(), 240)
        ),
    );
    Ok(())
}

/// Module A: Collect live nodes, tasks, Muse topics, rules, and crons across the workspace.
pub(crate) fn build_system_pulse(h: &Hub, db: &Connection, caller: &str) -> Result<Value> {
    let now = nodes::now_unix();
    let cutoff = now.saturating_sub(nodes::ONLINE_WINDOW_SECONDS);

    let mut node_stmt = db.prepare(
        "SELECT owner, id, name, capabilities, last_seen_unix FROM user_nodes WHERE owner=?1 ORDER BY owner ASC, id ASC",
    )?;
    let node_rows = node_stmt
        .query_map([caller], |r| {
            let last_seen: Option<i64> = r.get(4)?;
            let online = last_seen.is_some_and(|t| t >= cutoff);
            Ok(json!({
                "owner": r.get::<_, String>(0)?,
                "id": r.get::<_, String>(1)?,
                "name": r.get::<_, String>(2)?,
                "capabilities": r.get::<_, String>(3)?,
                "online": online,
            }))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut task_stmt = db.prepare(
        "SELECT t.id, t.requester, t.owner, t.summary, t.status, t.revision,
                a.node_id, a.status
         FROM tasks t
         LEFT JOIN node_assignments a ON a.task_id = t.id AND a.task_revision = t.revision AND a.owner=?1
         WHERE t.requester=?1 OR t.owner=?1
         ORDER BY t.rowid DESC",
    )?;
    let task_rows = task_stmt
        .query_map([caller], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, i64>(5)?,
                r.get::<_, Option<String>>(6)?,
                r.get::<_, Option<String>>(7)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut active_collab = Vec::new();
    let mut needs_attention = Vec::new();
    let mut member_load: HashMap<String, (usize, usize, usize)> = HashMap::new();
    let mut muse_topics = 0usize;
    let mut completed_total = 0usize;

    for (id, req, owner, summary, status, _rev, node_id, node_status) in &task_rows {
        let entry = member_load.entry(owner.clone()).or_default();
        if status == "completed" {
            completed_total += 1;
            entry.2 += 1;
            if req == owner && nodes::is_topic_summary(summary) {
                muse_topics += 1;
            }
            continue;
        }
        if status == "canceled" {
            continue;
        }
        if req == owner {
            muse_topics += 1;
        }
        if status == "working" {
            entry.0 += 1;
        } else {
            entry.1 += 1;
        }
        if req != owner {
            active_collab.push(json!({
                "id": id,
                "short_id": short_id(id),
                "requester": req,
                "owner": owner,
                "summary": summary,
                "status": status,
                "node_id": node_id,
                "node_status": node_status,
            }));
        }
        let caller_waiting = (*req == caller && *status == "ready_for_acceptance")
            || (*owner == caller && matches!(status.as_str(), "awaiting_owner" | "working"));
        if caller_waiting {
            needs_attention.push(json!({
                "id": id,
                "short_id": short_id(id),
                "summary": summary,
                "status": status,
                "with": if *req == caller { owner } else { req },
            }));
        }
    }

    let (rules_count, crons_count) = h
        .automation
        .lock()
        .map(|g| {
            (
                g.rules.iter().filter(|r| r.owner == caller).count(),
                g.crons
                    .iter()
                    .filter(|c| c.enabled && c.owner == caller)
                    .count(),
            )
        })
        .unwrap_or((0, 0));

    let online_nodes = node_rows.iter().filter(|n| n["online"] == true).count();
    Ok(json!({
        "generated_at_unix": now,
        "nodes_total": node_rows.len(),
        "nodes_online": online_nodes,
        "nodes": node_rows,
        "active_collaborations": active_collab,
        "needs_attention": needs_attention,
        "muse_topics_count": muse_topics,
        "completed_total": completed_total,
        "rules_total": rules_count,
        "crons_active": crons_count,
        "member_workload": member_load.into_iter().map(|(m, (w, p, d))| json!({
            "member": m, "working": w, "pending": p, "completed": d
        })).collect::<Vec<_>>(),
    }))
}

pub(crate) fn format_system_pulse(pulse: &Value) -> String {
    let nodes = pulse["nodes"].as_array().cloned().unwrap_or_default();
    let collabs = pulse["active_collaborations"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let attention = pulse["needs_attention"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let workloads = pulse["member_workload"]
        .as_array()
        .cloned()
        .unwrap_or_default();

    let mut lines = vec![
        "### 📊 我的任务与节点态势".to_owned(),
        String::new(),
        format!(
            "- **资源 PC 节点**：在线 `{}` / 总计 `{}` 台",
            pulse["nodes_online"], pulse["nodes_total"]
        ),
        format!(
            "- **Muse 归并主题**：`{}` 个（累计完成闭环 `{}` 项）",
            pulse["muse_topics_count"], pulse["completed_total"]
        ),
        format!(
            "- **7×24h 自动化**：生效分发规则 `{}` 条 · 运行中定时任务 `{}` 项",
            pulse["rules_total"], pulse["crons_active"]
        ),
    ];

    if !nodes.is_empty() {
        lines.push(String::new());
        lines.push("#### 🖥️ 节点在线状态".to_owned());
        for n in &nodes {
            let dot = if n["online"] == true {
                "🟢 在线"
            } else {
                "⚪ 离线"
            };
            let cap = n["capabilities"].as_str().unwrap_or("");
            let cap_short: String = cap.chars().take(72).collect();
            lines.push(format!(
                "- **{}** · `{} ({})` {} {}",
                n["owner"].as_str().unwrap_or(""),
                n["name"].as_str().unwrap_or(""),
                n["id"].as_str().unwrap_or(""),
                dot,
                if cap_short.is_empty() {
                    String::new()
                } else {
                    format!("— `{cap_short}`")
                }
            ));
        }
    }

    if !workloads.is_empty() {
        lines.push(String::new());
        lines.push("#### 👥 成员任务负载".to_owned());
        for w in &workloads {
            lines.push(format!(
                "- `{}`：进行中 `{}` · 待核对/待验收 `{}` · 已完成 `{}`",
                w["member"].as_str().unwrap_or(""),
                w["working"],
                w["pending"],
                w["completed"]
            ));
        }
    }

    if !collabs.is_empty() {
        lines.push(String::new());
        lines.push("#### 🤝 进行中的跨人协作".to_owned());
        for c in collabs.iter().take(10) {
            let node_info = c["node_id"]
                .as_str()
                .map(|id| {
                    format!(
                        " → 节点 `{id}` ({})",
                        c["node_status"].as_str().unwrap_or("")
                    )
                })
                .unwrap_or_default();
            lines.push(format!(
                "- `#{}` **{} → {}**：{}（状态：`{}`{}）",
                c["short_id"].as_str().unwrap_or(""),
                c["requester"].as_str().unwrap_or(""),
                c["owner"].as_str().unwrap_or(""),
                c["summary"].as_str().unwrap_or(""),
                c["status"].as_str().unwrap_or(""),
                node_info
            ));
        }
    }

    if !attention.is_empty() {
        lines.push(String::new());
        lines.push("#### 🔔 需要你关注的事项".to_owned());
        for a in attention.iter().take(8) {
            lines.push(format!(
                "- `#{}` [{}] {}（协作方：`{}`）",
                a["short_id"].as_str().unwrap_or(""),
                a["status"].as_str().unwrap_or(""),
                a["summary"].as_str().unwrap_or(""),
                a["with"].as_str().unwrap_or("")
            ));
        }
    }
    lines.join("\n")
}

fn is_node_fleet_query(trimmed: &str) -> bool {
    let lower = trimmed.to_ascii_lowercase();
    [
        "几台机器",
        "几台pc",
        "几台电脑",
        "哪些机器",
        "哪些pc",
        "哪些节点",
        "机器列表",
        "节点列表",
        "我有几台",
        "你有几台",
    ]
    .iter()
    .any(|kw| trimmed.contains(kw) || lower.contains(kw))
}

fn format_user_node_fleet(db: &Connection, owner: &str) -> Result<String> {
    let now = nodes::now_unix();
    let mut stmt = db.prepare(
        "SELECT id, name, capabilities, last_seen_unix FROM user_nodes WHERE owner=?1 ORDER BY rowid ASC",
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

    if rows.is_empty() {
        return Ok("你当前还没有绑定任何本地资源 PC 节点。你可以在左侧「我的 → 本机 Agent 节点」复制一键接入命令绑定电脑；在无 PC 节点时，我仍会在云端 7×24h 为你处理消息、定时任务与协作对接。".to_owned());
    }

    let online_count = rows
        .iter()
        .filter(|(_, _, _, seen)| {
            seen.is_some_and(|t| now.saturating_sub(t) <= nodes::ONLINE_WINDOW_SECONDS)
        })
        .count();
    let mut lines = vec![
        format!(
            "你当前共绑定了 **{} 台** PC 节点（当前在线 **{} 台**）：",
            rows.len(),
            online_count
        ),
        String::new(),
    ];
    for (id, name, capabilities, seen) in rows {
        let online = seen.is_some_and(|t| now.saturating_sub(t) <= nodes::ONLINE_WINDOW_SECONDS);
        let dot = if online { "🟢 在线" } else { "⚪ 离线" };
        let cap_display = if capabilities.trim().is_empty() {
            "暂无环境上报".to_owned()
        } else {
            capabilities.trim().to_owned()
        };
        lines.push(format!("- **{name}**（`{id}`）· {dot} · `{cap_display}`"));
    }
    lines.push(String::new());
    lines.push("💡 使用说明：连续提问时我会默认保持在同一台节点上处理，避免来回跳机器；如需切换，直接说节点名（如 `@cc-team ...` / `让 cc-pro-team ...`）或说「看下另外一台机器」，也可以设置 `规则：...` 固定路由。".to_owned());
    Ok(lines.join("\n"))
}

fn is_pulse_query(trimmed: &str) -> bool {
    let lower = trimmed.to_ascii_lowercase();
    trimmed == "/pulse"
        || [
            "系统运转",
            "运转情况",
            "大家都在忙",
            "全局状态",
            "整体进展",
            "全局总结",
            "system pulse",
        ]
        .iter()
        .any(|kw| trimmed.contains(kw) || lower.contains(kw))
}

fn parse_rule_command(db: &Connection, owner: &str, body: &str) -> Result<DispatchRule> {
    let body = body.trim();
    if body.is_empty() || body.len() > 500 {
        return Err(invalid("规则内容长度需在 1..500 字节内"));
    }
    let id = format!("rule-{}", &Uuid::new_v4().simple().to_string()[..6]);
    let now = nodes::now_unix();

    // Support human-approval / human-takeover rule:
    // e.g. "别人委派任务需要我先审核" / "规则：包含 数据库迁移 的任务需要真人接手" / "规则：涉及 合同 的消息由我本人确认"
    if [
        "审核",
        "人工确认",
        "本人确认",
        "真人接手",
        "本人接手",
        "我本人回",
        "我来回",
        "找我本人",
        "需要我确认",
        "由我拍板",
    ]
    .iter()
    .any(|cue| body.contains(cue))
    {
        let kw = extract_between(body, "包含", "的")
            .or_else(|| extract_between(body, "涉及", "的"))
            .or_else(|| extract_between(body, "关于", "的"))
            .unwrap_or_else(|| "*".to_owned());
        return Ok(DispatchRule {
            id,
            owner: owner.to_owned(),
            keyword: kw,
            target_node_id: String::new(),
            require_clarification: None,
            require_human_approval: true,
            created_at: now,
        });
    }

    // Support clarification rule: "别人委派任务必须提供 <requirement>" or "包含 <kw> 的任务必须提供 <requirement>"
    for cue in ["必须提供", "需提供", "缺少时询问", "必须附带", "要先提供"] {
        if let Some(pos) = body.find(cue) {
            let req = body[pos + cue.len()..]
                .trim()
                .trim_start_matches(['：', ':'])
                .trim();
            if !req.is_empty() {
                let kw = extract_between(&body[..pos], "包含", "的")
                    .or_else(|| extract_between(&body[..pos], "涉及", "的"))
                    .unwrap_or_else(|| "*".to_owned());
                return Ok(DispatchRule {
                    id,
                    owner: owner.to_owned(),
                    keyword: kw,
                    target_node_id: String::new(),
                    require_clarification: Some(req.to_owned()),
                    require_human_approval: false,
                    created_at: now,
                });
            }
        }
    }

    // Parse dispatch target: "包含 <keyword> 的任务派给 <target>" or "<keyword> -> <target>"
    let mut node_stmt = db.prepare("SELECT id, name FROM user_nodes WHERE owner=?1")?;
    let user_nodes = node_stmt
        .query_map([owner], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut target_node_id = if body.contains("云端") || body.to_ascii_lowercase().contains("cloud")
    {
        "cloud".to_owned()
    } else {
        user_nodes
            .iter()
            .find(|(nid, name)| {
                nodes::contains_alias(body, nid)
                    || (!name.eq_ignore_ascii_case(nid) && nodes::contains_alias(body, name))
            })
            .map(|(nid, _)| nid.clone())
            .unwrap_or_default()
    };

    if target_node_id.is_empty() {
        for cue in ["派给", "分配给", "交给", "发给", "->"] {
            if let Some(pos) = body.find(cue) {
                let candidate = body[pos + cue.len()..]
                    .trim()
                    .split_whitespace()
                    .next()
                    .unwrap_or("");
                if !candidate.is_empty() {
                    target_node_id = candidate.to_owned();
                    break;
                }
            }
        }
    }
    if target_node_id.is_empty() {
        return Err(invalid(
            "请指明目标节点或规则要求，例如：「规则：包含 atlas 的任务派给 homelab」或「规则：别人委派任务必须提供 复现步骤与截图」",
        ));
    }

    let keyword = extract_between(body, "包含", "的")
        .or_else(|| extract_between(body, "涉及", "的"))
        .or_else(|| extract_between(body, "关于", "的"))
        .or_else(|| {
            body.split("->")
                .next()
                .filter(|_| body.contains("->"))
                .map(|s| s.trim().to_owned())
        })
        .unwrap_or_else(|| "*".to_owned());

    Ok(DispatchRule {
        id,
        owner: owner.to_owned(),
        keyword,
        target_node_id,
        require_clarification: None,
        require_human_approval: false,
        created_at: now,
    })
}

fn try_parse_natural_rule(
    db: &Connection,
    owner: &str,
    trimmed: &str,
) -> Result<Option<DispatchRule>> {
    if trimmed.starts_with('/') || trimmed.contains('@') || trimmed.len() > 360 {
        return Ok(None);
    }
    if [
        "查看",
        "列出",
        "显示",
        "删除",
        "取消",
        "为什么",
        "怎么",
        "哪些规则",
    ]
    .iter()
    .any(|kw| trimmed.contains(kw))
    {
        return Ok(None);
    }
    let has_standing_cue = [
        "以后凡是",
        "以后只要",
        "凡是包含",
        "凡是涉及",
        "只要包含",
        "只要涉及",
        "以后别人委派",
        "别人委派任务必须",
        "把所有包含",
        "把所有涉及",
    ]
    .iter()
    .any(|cue| trimmed.contains(cue));
    if !has_standing_cue {
        return Ok(None);
    }
    Ok(parse_rule_command(db, owner, trimmed).ok())
}

fn extract_between(text: &str, start: &str, end: &str) -> Option<String> {
    let s = text.find(start)? + start.len();
    let rest = &text[s..];
    let e = rest.find(end)?;
    let mid = rest[..e].trim();
    (!mid.is_empty()).then(|| mid.to_owned())
}

fn unix_to_utc_ymdhms(unix: i64) -> (i64, u32, u32, u32, u32, u32) {
    let days = unix.div_euclid(86400);
    let secs = unix.rem_euclid(86400) as u32;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = (yoe as i64) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = y + if m <= 2 { 1 } else { 0 };
    (
        year,
        m as u32,
        d as u32,
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60,
    )
}

fn format_unix_utc_iso(unix: i64) -> String {
    let (y, m, d, h, min, s) = unix_to_utc_ymdhms(unix);
    format!("{y:04}-{m:02}-{d:02}T{h:02}:{min:02}:{s:02}Z")
}

fn cst_day_and_hhmm(unix: i64) -> (i64, u32, u32) {
    let cst = unix.saturating_add(8 * 3600);
    let day = cst.div_euclid(86400);
    let sec_in_day = cst.rem_euclid(86400) as u32;
    (day, sec_in_day / 3600, (sec_in_day % 3600) / 60)
}

fn parse_hhmm(s: &str) -> Option<(u32, u32)> {
    let normalized = s.trim().replace('：', ":");
    let (h_str, m_str) = normalized.split_once(':')?;
    let h: u32 = h_str.trim().parse().ok()?;
    let m: u32 = m_str.trim().parse().ok()?;
    (h <= 23 && m <= 59).then_some((h, m))
}

fn parse_zh_or_ascii_num(s: &str) -> Option<(u32, usize)> {
    let digit_len: usize = s
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .map(char::len_utf8)
        .sum();
    if digit_len > 0 {
        let val: u32 = s[..digit_len].parse().ok()?;
        return Some((val, digit_len));
    }
    let zh_pairs: [(&str, u32); 16] = [
        ("二十四", 24),
        ("二十三", 23),
        ("二十二", 22),
        ("二十一", 21),
        ("二十", 20),
        ("十二", 12),
        ("十一", 11),
        ("十", 10),
        ("零", 0),
        ("一", 1),
        ("二", 2),
        ("两", 2),
        ("三", 3),
        ("四", 4),
        ("五", 5),
        ("六", 6),
    ];
    for (pat, val) in zh_pairs {
        if s.starts_with(pat) {
            return Some((val, pat.len()));
        }
    }
    for (pat, val) in [("七", 7), ("八", 8), ("九", 9)] {
        if s.starts_with(pat) {
            return Some((val, pat.len()));
        }
    }
    None
}

fn parse_daily_time_prefix(s: &str) -> Option<(String, &str)> {
    let mut rest = s.trim_start();
    let mut is_pm: Option<bool> = None;
    for (label, pm) in [
        ("晚上", Some(true)),
        ("夜间", Some(true)),
        ("夜里", Some(true)),
        ("傍晚", Some(true)),
        ("晚间", Some(true)),
        ("晚", Some(true)),
        ("下午", Some(true)),
        ("午后", Some(true)),
        ("中午", None),
        ("早上", Some(false)),
        ("早晨", Some(false)),
        ("清晨", Some(false)),
        ("上午", Some(false)),
        ("凌晨", Some(false)),
        ("早", Some(false)),
    ] {
        if let Some(after) = rest.strip_prefix(label) {
            is_pm = pm;
            rest = after.trim_start();
            break;
        }
    }

    let (raw_h, h_bytes) = parse_zh_or_ascii_num(rest)?;
    let after_h = &rest[h_bytes..];
    let (mut hour, min, after_time) = if let Some(after_colon) = after_h
        .strip_prefix(':')
        .or_else(|| after_h.strip_prefix('：'))
    {
        let m_bytes: usize = after_colon
            .chars()
            .take(2)
            .take_while(|c| c.is_ascii_digit())
            .map(char::len_utf8)
            .sum();
        if m_bytes == 0 {
            return None;
        }
        let m_val: u32 = after_colon[..m_bytes].parse().ok()?;
        (raw_h, m_val, &after_colon[m_bytes..])
    } else if let Some(after_dian) = after_h
        .strip_prefix("点钟")
        .or_else(|| after_h.strip_prefix('点'))
        .or_else(|| after_h.strip_prefix('时'))
    {
        let after_d = after_dian.trim_start();
        if let Some(after_half) = after_d.strip_prefix('半') {
            (raw_h, 30, after_half)
        } else if let Some((m_val, m_bytes)) = parse_zh_or_ascii_num(after_d) {
            let after_m = &after_d[m_bytes..];
            if let Some(after_unit) = after_m
                .strip_prefix("分钟")
                .or_else(|| after_m.strip_prefix('分'))
            {
                (raw_h, m_val, after_unit)
            } else if m_val <= 59 && !after_m.starts_with(['个', '天', '次', '条']) {
                (raw_h, m_val, after_m)
            } else {
                (raw_h, 0, after_dian)
            }
        } else if let Some(after_exact) = after_d.strip_prefix('整') {
            (raw_h, 0, after_exact)
        } else {
            (raw_h, 0, after_dian)
        }
    } else {
        return None;
    };

    if is_pm == Some(true) && (1..=11).contains(&hour) {
        hour += 12;
    } else if is_pm == Some(false) && hour == 12 {
        hour = 0;
    }
    if hour > 23 || min > 59 {
        return None;
    }
    let tail = after_time
        .trim_start_matches([
            ' ', '，', ',', '、', '准', '时', '整', '左', '右', '自', '动',
        ])
        .trim_start();
    Some((format!("{hour:02}:{min:02}"), tail))
}

fn is_gitlab_commit_query(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    let mentions_gitlab = lower.contains("gitlab") || text.contains("代码仓库");
    let asks_commits = text.contains("提交")
        || lower.contains("commit")
        || text.contains("代码改动")
        || text.contains("开发动态");
    mentions_gitlab && asks_commits
}

fn is_cron_due(job: &CronJob, now: i64) -> bool {
    if let Some(hhmm) = &job.daily_time_cst {
        let Some((target_h, target_m)) = parse_hhmm(hhmm) else {
            return false;
        };
        let (cur_day, cur_h, cur_m) = cst_day_and_hhmm(now);
        if (cur_h, cur_m) < (target_h, target_m) {
            return false;
        }
        if let Some(last) = job.last_run_unix {
            let (last_day, _, _) = cst_day_and_hhmm(last);
            if last_day >= cur_day {
                return false;
            }
        }
        return true;
    }
    let interval_secs = (job.interval_minutes.max(1) as i64) * 60;
    job.last_run_unix
        .is_none_or(|last| now.saturating_sub(last) >= interval_secs)
}

fn build_cron_job(
    owner: &str,
    interval_minutes: u64,
    daily_time_cst: Option<String>,
    instruction: &str,
) -> CronJob {
    let now = nodes::now_unix();
    let target = if instruction.contains("只在云端")
        || instruction.contains("云端执行")
        || is_gitlab_commit_query(instruction)
    {
        "cloud".to_owned()
    } else {
        "auto".to_owned()
    };
    let last_run_unix = daily_time_cst
        .as_deref()
        .and_then(parse_hhmm)
        .and_then(|(th, tm)| {
            let (_day, ch, cm) = cst_day_and_hhmm(now);
            ((ch, cm) >= (th, tm)).then_some(now)
        });
    CronJob {
        id: format!("cron-{}", &Uuid::new_v4().simple().to_string()[..6]),
        owner: owner.to_owned(),
        interval_minutes,
        daily_time_cst,
        instruction: instruction.trim().to_owned(),
        target,
        last_run_unix,
        enabled: true,
        created_at: now,
    }
}

fn parse_cron_command(owner: &str, body: &str) -> Result<CronJob> {
    let trimmed = body.trim();
    if trimmed.is_empty() || trimmed.len() > 1000 {
        return Err(invalid("定时任务指令不能为空"));
    }
    if let Some(rest) = trimmed
        .strip_prefix("每天")
        .or_else(|| trimmed.strip_prefix("每日"))
        .or_else(|| trimmed.strip_prefix("每晚"))
        .or_else(|| trimmed.strip_prefix("每早"))
    {
        let normalized_in = if trimmed.starts_with("每晚") {
            format!("晚上{}", rest.trim())
        } else if trimmed.starts_with("每早") {
            format!("早上{}", rest.trim())
        } else {
            rest.trim().to_owned()
        };
        if let Some((hhmm, tail)) = parse_daily_time_prefix(&normalized_in) {
            let instr = tail.trim();
            if instr.is_empty() {
                return Err(invalid("请填写每日定时任务的具体执行内容"));
            }
            return Ok(build_cron_job(owner, 1440, Some(hhmm), instr));
        }
        let instr = rest.trim();
        if instr.is_empty() {
            return Err(invalid("请填写每日定时任务的具体执行内容"));
        }
        return Ok(build_cron_job(owner, 1440, None, instr));
    }
    let (interval_minutes, instruction) = if let Some(rest) = trimmed.strip_prefix("每小时") {
        (60u64, rest.trim())
    } else if let Some(rest) = trimmed
        .strip_prefix("每隔")
        .or_else(|| trimmed.strip_prefix('每'))
    {
        let rest = rest.trim_start();
        if let Some((n, digits_len)) = parse_zh_or_ascii_num(rest) {
            let after_num = rest[digits_len..].trim_start();
            if let Some(instr) = after_num
                .strip_prefix("分钟")
                .or_else(|| after_num.strip_prefix('分'))
            {
                if n == 0 || n > 10080 {
                    return Err(invalid("定时分钟数需在 1..10080 之间"));
                }
                (n as u64, instr.trim())
            } else if let Some(instr) = after_num
                .strip_prefix("个小时")
                .or_else(|| after_num.strip_prefix("小时"))
            {
                if n == 0 || n > 168 {
                    return Err(invalid("定时小时数需在 1..168 之间"));
                }
                ((n as u64) * 60, instr.trim())
            } else {
                (60u64, trimmed)
            }
        } else {
            (60u64, trimmed)
        }
    } else {
        (60u64, trimmed)
    };
    if instruction.is_empty() {
        return Err(invalid(
            "请填写定时任务具体内容，例如：「定时任务：每 60 分钟 总结系统运转情况」",
        ));
    }
    Ok(build_cron_job(owner, interval_minutes, None, instruction))
}

fn try_parse_natural_cron(owner: &str, trimmed: &str) -> Result<Option<CronJob>> {
    let lower = trimmed.to_ascii_lowercase();
    if [
        "查看",
        "列出",
        "显示",
        "删除",
        "取消",
        "为什么",
        "怎么",
        "在哪",
        "还没",
        "失败",
        "多少条",
        "哪些定时",
    ]
    .iter()
    .any(|kw| trimmed.contains(kw))
        || trimmed.starts_with('/')
        || lower.starts_with("http")
    {
        return Ok(None);
    }

    for prefix in ["每天", "每日", "每晚", "每早"] {
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            let normalized = if prefix == "每晚" {
                format!("晚上{}", rest.trim())
            } else if prefix == "每早" {
                format!("早上{}", rest.trim())
            } else {
                rest.trim().to_owned()
            };
            if let Some((hhmm, tail)) = parse_daily_time_prefix(&normalized) {
                let instr = tail.trim();
                if instr.chars().count() >= 2 {
                    return Ok(Some(build_cron_job(owner, 1440, Some(hhmm), instr)));
                }
            }
        }
    }

    if let Some(rest) = trimmed.strip_prefix("每小时") {
        let instr = rest
            .trim_start_matches([' ', '，', ',', '都', '帮', '我'])
            .trim();
        if instr.chars().count() >= 2
            && (trimmed.contains("汇总")
                || trimmed.contains("总结")
                || trimmed.contains("检查")
                || trimmed.contains("推送")
                || trimmed.contains("发给"))
        {
            return Ok(Some(build_cron_job(owner, 60, None, instr)));
        }
    }
    if let Some(rest) = trimmed
        .strip_prefix("每隔")
        .or_else(|| trimmed.strip_prefix('每'))
    {
        let rest_tr = rest.trim_start();
        if let Some((n, len)) = parse_zh_or_ascii_num(rest_tr) {
            let after_n = rest_tr[len..].trim_start();
            if let Some(after_u) = after_n
                .strip_prefix("分钟")
                .or_else(|| after_n.strip_prefix('分'))
            {
                let instr = after_u.trim();
                if (1..=10080).contains(&n) && instr.chars().count() >= 2 {
                    return Ok(Some(build_cron_job(owner, n as u64, None, instr)));
                }
            } else if let Some(after_u) = after_n
                .strip_prefix("个小时")
                .or_else(|| after_n.strip_prefix("小时"))
            {
                let instr = after_u.trim();
                if (1..=168).contains(&n) && instr.chars().count() >= 2 {
                    return Ok(Some(build_cron_job(owner, (n as u64) * 60, None, instr)));
                }
            }
        }
    }
    Ok(None)
}

fn format_rules_list(rules: &[DispatchRule], owner: &str) -> String {
    let mine: Vec<&DispatchRule> = rules.iter().filter(|r| r.owner == owner).collect();
    let mut lines = vec![
        "### 🛡️ 管家三层规则引擎（系统底线 + 你的个人定制规则）".to_owned(),
        String::new(),
        "**Layer 1 · 系统内置底线规则（默认生效）**：".to_owned(),
        "- 🚨 **点名找本人**：消息含「本人 / 真人 / 亲自 / 急事找你」→ 强制转交你本人接手".to_owned(),
        "- 🚨 **排期与高危拍板**：消息含「能发版吗 / 几点上线 / 回滚生产 / 合入master」→ 管家不代承诺，转交本人拍板".to_owned(),
        "- 🚨 **连续退回熔断**：同一任务连续退回 ≥ 2 次 → 自动暂停节点重跑，升级双方真人对齐".to_owned(),
        "- 🚨 **能力边界升级**：需跑本机环境但无在线 PC 节点，或大模型自检 `[NEED_HUMAN]` → 自动升级本人接手".to_owned(),
        String::new(),
        format!("**Layer 2 · 你的个人定制规则（{} 条）**：", mine.len()),
    ];
    if mine.is_empty() {
        lines.push("- 暂未添加个人定制规则。你可以随时在聊天框发送：".to_owned());
        lines.push("  - `规则：包含 生产数据库 的任务需要真人接手`".to_owned());
        lines.push("  - `规则：别人委派任务必须提供 复现步骤与截图`".to_owned());
        lines.push("  - `规则：包含 atlas 的任务派给 homelab`".to_owned());
    } else {
        for r in mine {
            let desc = if r.require_human_approval {
                format!(
                    "🚨 关键词 `{}` → 必须由你本人接手确认（管家不代拍板）",
                    r.keyword
                )
            } else if let Some(req) = &r.require_clarification {
                format!("📎 关键词 `{}` → 管家先核对必填材料：`{req}`", r.keyword)
            } else {
                format!(
                    "💻 关键词 `{}` → 优先路由至节点 `{}`",
                    r.keyword, r.target_node_id
                )
            };
            lines.push(format!("- `{}`：{desc}", r.id));
        }
        lines.push(String::new());
        lines.push("回复 `删除规则 <规则ID>` 可移除对应个人规则。".to_owned());
    }
    lines.join("\n")
}

fn format_crons_list(crons: &[CronJob], owner: &str) -> String {
    let mine: Vec<&CronJob> = crons.iter().filter(|c| c.owner == owner).collect();
    if mine.is_empty() {
        return "当前还没有配置 7×24h 定时任务。\n\n你可以随时说：\n- `每天晚上10点汇总一下gitlab上的提交总结给我`\n- `定时任务：每 60 分钟 总结系统运转情况`".to_owned();
    }
    let mut lines = vec![
        format!("### ⏰ 你的 7×24h 定时任务（{} 项）", mine.len()),
        String::new(),
    ];
    for c in mine {
        let last = c
            .last_run_unix
            .map(|ts| {
                let (_, m, d, h, min, _) = unix_to_utc_ymdhms(ts + 8 * 3600);
                format!("上次记录 {m:02}-{d:02} {h:02}:{min:02}")
            })
            .unwrap_or_else(|| "等待触发".to_owned());
        let sched = match &c.daily_time_cst {
            Some(hhmm) => format!("每天北京时间 {hhmm}"),
            None => format!("每 {} 分钟", c.interval_minutes),
        };
        lines.push(format!(
            "- `{}`：{sched} · [{}] `{}`（目标：`{}`，{last}）",
            c.id,
            if c.enabled { "启用" } else { "停用" },
            c.instruction,
            c.target
        ));
    }
    lines.push(String::new());
    lines.push("支持口令：`立即执行定时任务 <ID>` 或 `删除定时任务 <ID>`。".to_owned());
    lines.join("\n")
}

async fn fetch_gitlab_commit_summary(h: &Hub, owner: &str) -> Option<String> {
    let cfg = h
        .automation
        .lock()
        .ok()
        .and_then(|g| g.gitlab_configs.get(owner).cloned())
        .filter(|c| !c.token.trim().is_empty())?;
    let base_url = normalize_gitlab_base_url(&cfg.base_url)?;
    let now = nodes::now_unix();
    let since_iso = format_unix_utc_iso(now - 86400);
    let (_, sm, sd, sh, smin, _) = unix_to_utc_ymdhms(now - 86400 + 8 * 3600);
    let (_, em, ed, eh, emin, _) = unix_to_utc_ymdhms(now + 8 * 3600);

    let proj_url =
        format!("{base_url}/api/v4/projects?order_by=last_activity_at&sort=desc&per_page=10");
    let proj_resp = h
        .client
        .get(&proj_url)
        .header("PRIVATE-TOKEN", &cfg.token)
        .send()
        .await
        .ok()?;
    if !proj_resp.status().is_success() {
        return Some(format!(
            "⚠️ 连接 GitLab（{base_url}）拉取项目提交失败（HTTP {}），请确认 Token 具备 `api` / `read_api` 权限。",
            proj_resp.status().as_u16()
        ));
    }
    let projects: Vec<Value> = proj_resp.json().await.ok()?;

    let mut sections: Vec<String> = Vec::new();
    let mut total_commits = 0usize;
    for proj in projects.iter().take(10) {
        let Some(pid) = proj["id"].as_i64() else {
            continue;
        };
        let path = proj["path_with_namespace"].as_str().unwrap_or("unknown");
        let commits_url = format!(
            "{base_url}/api/v4/projects/{pid}/repository/commits?all=true&since={}&per_page=15",
            percent_encode_component(&since_iso)
        );
        let Ok(c_resp) = h
            .client
            .get(&commits_url)
            .header("PRIVATE-TOKEN", &cfg.token)
            .send()
            .await
        else {
            continue;
        };
        if !c_resp.status().is_success() {
            continue;
        }
        let Ok(commits) = c_resp.json::<Vec<Value>>().await else {
            continue;
        };
        if commits.is_empty() {
            continue;
        }
        total_commits += commits.len();
        let mut lines = vec![format!("#### 📁 `{path}`（{} 条提交）", commits.len())];
        for c in commits.iter().take(15) {
            let sha = c["short_id"].as_str().unwrap_or("-");
            let author = c["author_name"].as_str().unwrap_or("unknown");
            let title = c["title"].as_str().unwrap_or("(无标题)").trim();
            let web_url = c["web_url"].as_str().unwrap_or("");
            let link = if web_url.is_empty() {
                format!("`{sha}`")
            } else {
                format!("[`{sha}`]({web_url})")
            };
            lines.push(format!("- {link} · **{author}**：{title}"));
        }
        sections.push(lines.join("\n"));
    }

    if total_commits == 0 {
        return Some(format!(
            "📊 过去 24 小时（北京时间 `{sm:02}-{sd:02} {sh:02}:{smin:02}` ～ `{em:02}-{ed:02} {eh:02}:{emin:02}`）内 GitLab 活跃项目暂无新增提交。"
        ));
    }
    Some(format!(
        "### 📊 GitLab 最近 24 小时提交汇总（`{sm:02}-{sd:02} {sh:02}:{smin:02}` ～ `{em:02}-{ed:02} {eh:02}:{emin:02}` · 共 {total_commits} 条）\n\n{}",
        sections.join("\n\n")
    ))
}

async fn execute_cloud_cron_report(h: &Hub, owner: &str, instruction: &str) -> Result<String> {
    if is_gitlab_commit_query(instruction) {
        let Some(raw_summary) = fetch_gitlab_commit_summary(h, owner).await else {
            return Ok(
                "⚠️ 当前账号尚未配置 GitLab Personal Access Token，无法从云端拉取 GitLab 提交。请先在左下角 **我的 → 账号与设置 → 🦊 GitLab 双向协同** 保存有效 Token。"
                    .to_owned(),
            );
        };
        if let crate::Runtime::Dsh { url, token } = &h.runtime {
            if !raw_summary.starts_with("⚠️") && !raw_summary.starts_with("📊") {
                let wants_raw_detail = ["明细", "详细", "原文", "全部提交", "完整列表"]
                    .iter()
                    .any(|kw| instruction.contains(kw));
                let prompt = format!(
                    "你是用户的 7×24h 云端 Super-Agent（管家）。用户的定时汇总要求是：「{instruction}」。\n\
                     请根据下面从公司 GitLab 实时抓取的最近 24 小时真实代码提交记录，严格按照用户的汇总要求按项目提炼核心改动（默认力求精炼易读、每个项目 1~3 条核心要点，不要编造任何未列出的提交）：\n\n{raw_summary}"
                );
                let runtime_profile = {
                    let db = h.db()?;
                    crate::profiles::route(&db, owner, None, None)?
                };
                let dsh = crate::runtime::DshRuntime {
                    client: &h.client,
                    url,
                    token,
                };
                let sid = Uuid::new_v4().to_string();
                if let Ok(ans) = crate::runtime::Runtime::turn(
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
                {
                    if !ans.reply.trim().is_empty() {
                        if wants_raw_detail {
                            return Ok(format!("{}\n\n---\n\n{raw_summary}", ans.reply.trim()));
                        }
                        return Ok(ans.reply.trim().to_owned());
                    }
                }
            }
        }
        return Ok(raw_summary);
    }

    let pulse_text = {
        let db = h.db()?;
        let pulse = build_system_pulse(h, &db, owner)?;
        format_system_pulse(&pulse)
    };
    if is_pulse_query(instruction) || instruction.contains("巡检") || instruction.contains("报告")
    {
        return Ok(pulse_text);
    }
    if let crate::Runtime::Dsh { url, token } = &h.runtime {
        let (mem_md, ledgers_summary) = h
            .automation
            .lock()
            .ok()
            .map(|g| {
                let m = g
                    .muse_profiles
                    .get(owner)
                    .map(|p| p.memory_md.clone())
                    .unwrap_or_default();
                let l_lines: Vec<String> = g
                    .ledgers
                    .iter()
                    .filter(|l| l.owner == owner)
                    .map(|l| format!("- [{}] {}：{}", l.status, l.title, l.notes))
                    .collect();
                (m, l_lines.join("\n"))
            })
            .unwrap_or_default();
        let prompt = format!(
            "[MUSE_AGENT_TURN]\n\
             你是用户的 7×24h 云端个人超级智能体（Meta Muse）。现在要自动执行用户设定的定时任务：「{instruction}」。\n\
             请严格遵守用户的长期规矩（MEMORY.md）与专项活账本状态（注意：状态为 `muted` 静默挂起的事项绝对不要在报告中主动提起或催促！状态为 `active` 的主线按顺延/验收规矩汇报）：\n\n\
             【MEMORY.md】\n{mem_md}\n\n\
             【专项活账本 (ledgers)】\n{ledgers_summary}\n\n\
             【系统快照】\n{pulse_text}"
        );
        let runtime_profile = {
            let db = h.db()?;
            crate::profiles::route(&db, owner, None, None)?
        };
        let dsh = crate::runtime::DshRuntime {
            client: &h.client,
            url,
            token,
        };
        let sid = Uuid::new_v4().to_string();
        if let Ok(ans) = crate::runtime::Runtime::turn(
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
        {
            if !ans.reply.trim().is_empty() {
                return Ok(ans.reply);
            }
        }
    }
    Ok(format!(
        "云端管家已完成 7×24h 例行执行：「{instruction}」。\n\n{pulse_text}"
    ))
}

/// Runs any scheduled cron jobs that are due (or matching `only_job_id` for manual trigger).
pub(crate) async fn run_due_crons(
    h: &Hub,
    force_owner: Option<&str>,
    only_job_id: Option<&str>,
) -> Result<usize> {
    let now = nodes::now_unix();
    let due: Vec<CronJob> = with_store_mut(h, |store| {
        let mut out = Vec::new();
        for job in &mut store.crons {
            if !job.enabled {
                continue;
            }
            if let Some(o) = force_owner {
                if job.owner != o {
                    continue;
                }
            }
            if let Some(jid) = only_job_id {
                if job.id != jid {
                    continue;
                }
            } else if !is_cron_due(job, now) {
                continue;
            }
            job.last_run_unix = Some(now);
            out.push(job.clone());
        }
        out
    })?;

    let mut executed = 0usize;
    for job in due {
        if job.instruction.eq_ignore_ascii_case("Heartbeat") || job.id.starts_with("cron-heartbeat")
        {
            let failed_count: i64 = h
                .db()
                .ok()
                .and_then(|db| {
                    db.query_row(
                        "SELECT COUNT(*) FROM node_assignments WHERE owner=?1 AND status='failed' AND updated_at >= ?2",
                        params![job.owner, now.saturating_sub(1800)],
                        |r| r.get(0),
                    )
                    .ok()
                })
                .unwrap_or(0);
            if failed_count > 0 {
                if let Ok(db) = h.db() {
                    let msg = format!(
                        "刚才 30 分钟例行巡检发现你有 {failed_count} 项本机节点任务执行异常中断，我已经记录在右侧任务脉络里，需要重跑或改由云端处理跟我说一声。"
                    );
                    let _ = insert_chat_message(&db, &job.owner, "assistant", &msg);
                }
                record_muse_audit(
                    h,
                    &job.owner,
                    "heartbeat",
                    "awaiting_human",
                    "Heartbeat 发现异常任务并主动提醒",
                    &format!("检测到最近 30 分钟内有 {failed_count} 项失败作业"),
                );
            } else {
                record_muse_audit(
                    h,
                    &job.owner,
                    "heartbeat",
                    "completed",
                    "Heartbeat 30分钟静默自检",
                    "HEARTBEAT_OK · 节点与任务主线正常，保持静默不打扰对话",
                );
            }
            executed += 1;
            continue;
        }
        let online_node = if job.target != "cloud" {
            let db = h.db()?;
            if job.target == "auto" {
                nodes::best_online_node(&db, &job.owner, &job.instruction)?
            } else {
                nodes::select_best_node(&db, &job.owner, &job.target)?
                    .filter(|(nid, _, online, _)| *nid == job.target && *online)
            }
        } else {
            None
        };

        if let Some((node_id, node_name, online, _)) = online_node {
            let mut db = h.db()?;
            let tx = db.transaction()?;
            let summary_prompt = format!("定时任务：{}", job.instruction);
            let (_task_id, fallback_reply) = nodes::dispatch_chat_task(
                &tx,
                &job.owner,
                &node_id,
                &node_name,
                online,
                &summary_prompt,
                None,
            )?;
            let note = format!(
                "⏰ 定时任务「{}」（`{}`）已自动派发给在线节点「{node_name}」：{fallback_reply}",
                job.instruction, job.id
            );
            insert_chat_message(&tx, &job.owner, "assistant", &note)?;
            tx.commit()?;
            executed += 1;
            continue;
        }

        // Fallback / explicit cloud 7x24h execution when no PC node is online!
        let report = execute_cloud_cron_report(h, &job.owner, &job.instruction).await?;
        let db = h.db()?;
        let msg = format!(
            "⏰ 管家 7×24h 云端定时任务报告（「{}」· `{}`）：\n\n{report}",
            job.instruction, job.id
        );
        insert_chat_message(&db, &job.owner, "assistant", &msg)?;
        executed += 1;
    }
    Ok(executed)
}

/// Handles `sagent` orchestration chat commands (System Pulse, Rules, Crons,
/// Cross-user Clarification replies, and Mobile Acceptance/Return).
pub(crate) async fn answer_chat(
    h: &Hub,
    owner: &str,
    input: &crate::messages::ChatInput,
    runtime_profile: &crate::profiles::RuntimeProfile,
    task_context: Option<&Value>,
) -> Result<Option<Json<Value>>> {
    if input.selected_project.is_some() {
        return Ok(None);
    }
    let trimmed = input.message.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }

    // 0. Meta Muse Unified Agent Loop (Single Brain + Combinable Tools in Dsh mode)
    if !trimmed.starts_with('/') {
        if let Some(resp) =
            try_handle_muse_agent_loop(h, owner, input, runtime_profile, task_context).await?
        {
            return Ok(Some(resp));
        }
    }

    // 1. Module C: Personal Dispatch Rules (explicit prefix or natural-language standing rule)
    let parsed_rule = if let Some(body) = strip_prefix_any(
        trimmed,
        &[
            "设置规则：",
            "设置规则:",
            "添加规则：",
            "添加规则:",
            "规则：",
            "规则:",
        ],
    ) {
        let db = h.db()?;
        Some(parse_rule_command(&db, owner, body)?)
    } else {
        let db = h.db()?;
        try_parse_natural_rule(&db, owner, trimmed)?
    };
    if let Some(rule) = parsed_rule {
        let total_for_owner = with_store_mut(h, |store| {
            store.rules.push(rule.clone());
            store.rules.iter().filter(|r| r.owner == owner).count()
        })?;
        let desc = if rule.require_human_approval {
            format!("关键词 `{}` 委派先提醒你本人审核", rule.keyword)
        } else if let Some(req) = &rule.require_clarification {
            format!("他人委派时自动核对必填信息「{req}」")
        } else {
            format!(
                "关键词 `{}` 自动调度节点 `{}`",
                rule.keyword, rule.target_node_id
            )
        };
        let reply = format!(
            "✅ 已保存个人分发规则 `{}`（当前共 {total_for_owner} 条）：\n- {desc}",
            rule.id
        );
        return complete_chat_reply(h, owner, input, runtime_profile, task_context, &reply)
            .map(Some);
    }
    if matches!(trimmed, "查看规则" | "我的规则" | "/rules") {
        let reply = {
            let guard = h
                .automation
                .lock()
                .map_err(|_| invalid("automation unavailable"))?;
            format_rules_list(&guard.rules, owner)
        };
        return complete_chat_reply(h, owner, input, runtime_profile, task_context, &reply)
            .map(Some);
    }
    if let Some(rule_id) = strip_prefix_any(trimmed, &["删除规则：", "删除规则:", "删除规则 "])
    {
        let removed = with_store_mut(h, |store| {
            let before = store.rules.len();
            store
                .rules
                .retain(|r| !(r.owner == owner && r.id == rule_id));
            before != store.rules.len()
        })?;
        let reply = if removed {
            format!("🗑️ 已删除规则 `{rule_id}`。")
        } else {
            format!("未找到属于你的规则 `{rule_id}`。")
        };
        return complete_chat_reply(h, owner, input, runtime_profile, task_context, &reply)
            .map(Some);
    }

    // 2. Module C: 7x24h Scheduled Cron Jobs (explicit prefix or natural-language time)
    let explicit_cron_body = strip_prefix_any(
        trimmed,
        &["添加定时任务：", "添加定时任务:", "定时任务：", "定时任务:"],
    );
    let parsed_cron = if let Some(body) = explicit_cron_body {
        Some(parse_cron_command(owner, body)?)
    } else {
        try_parse_natural_cron(owner, trimmed)?
    };
    if let Some(job) = parsed_cron {
        let sched_desc = match &job.daily_time_cst {
            Some(hhmm) => format!("每天 **北京时间 {hhmm}**"),
            None => format!("每 **{}** 分钟", job.interval_minutes),
        };
        let mode_desc = if job.target == "cloud" {
            "云端 X 管家 7×24h 自动执行（直接调用你的 GitLab 凭证汇聚总结，不依赖 PC 在线或沙盒权限）"
        } else {
            "PC 节点在线时优先由节点执行，离线或无节点时由云端 X 7×24h 自动执行"
        };
        let preview = if is_gitlab_commit_query(&job.instruction) {
            execute_cloud_cron_report(h, owner, &job.instruction)
                .await
                .ok()
                .map(|s| format!("\n\n---\n\n{s}"))
                .unwrap_or_default()
        } else {
            String::new()
        };
        let reply = format!(
            "⏰ 已创建 7×24h 定时任务 `{}`：\n- 触发周期：{sched_desc}\n- 内容：{}\n- 执行方式：{mode_desc}\n\n可随时直接用自然语言说「把这个定时汇总简化一下」「改到晚上 9 点半」或发送 `查看定时任务` 管理。{preview}",
            job.id, job.instruction
        );
        with_store_mut(h, |store| {
            // Auto-deduplicate any existing identical or same-time GitLab commit cron for this owner.
            let is_gl = is_gitlab_commit_query(&job.instruction);
            store.crons.retain(|c| {
                !(c.owner == owner
                    && c.daily_time_cst == job.daily_time_cst
                    && c.interval_minutes == job.interval_minutes
                    && (c.instruction == job.instruction
                        || (is_gl && is_gitlab_commit_query(&c.instruction))))
            });
            store.crons.push(job);
        })?;
        h.automation_notify.notify_one();
        return complete_chat_reply(h, owner, input, runtime_profile, task_context, &reply)
            .map(Some);
    }
    if matches!(trimmed, "查看定时任务" | "我的定时任务" | "/crons") {
        let reply = {
            let guard = h
                .automation
                .lock()
                .map_err(|_| invalid("automation unavailable"))?;
            format_crons_list(&guard.crons, owner)
        };
        return complete_chat_reply(h, owner, input, runtime_profile, task_context, &reply)
            .map(Some);
    }
    if let Some(cron_id) = strip_prefix_any(
        trimmed,
        &["删除定时任务：", "删除定时任务:", "删除定时任务 "],
    ) {
        let removed = with_store_mut(h, |store| {
            let before = store.crons.len();
            store
                .crons
                .retain(|c| !(c.owner == owner && c.id == cron_id));
            before != store.crons.len()
        })?;
        let reply = if removed {
            format!("🗑️ 已删除定时任务 `{cron_id}`。")
        } else {
            format!("未找到属于你的定时任务 `{cron_id}`。")
        };
        return complete_chat_reply(h, owner, input, runtime_profile, task_context, &reply)
            .map(Some);
    }
    if let Some(cron_id) = strip_prefix_any(
        trimmed,
        &[
            "立即执行定时任务：",
            "立即执行定时任务:",
            "立即执行定时任务 ",
            "立即运行定时任务 ",
        ],
    ) {
        let count = run_due_crons(h, Some(owner), Some(cron_id)).await?;
        let reply = if count > 0 {
            format!("✅ 已立即触发定时任务 `{cron_id}`，执行结果已同步写入对话流。")
        } else {
            format!("未找到属于你且启用的定时任务 `{cron_id}`。")
        };
        return complete_chat_reply(h, owner, input, runtime_profile, task_context, &reply)
            .map(Some);
    }
    if matches!(
        trimmed,
        "同步 GitLab" | "立即同步 GitLab" | "拉取 GitLab" | "拉取 GitLab 待办" | "/gitlab-sync"
    ) {
        let has_token = h
            .automation
            .lock()
            .ok()
            .and_then(|g| g.gitlab_configs.get(owner).map(|c| !c.token.is_empty()))
            .unwrap_or(false);
        let reply = if !has_token {
            "尚未配置 GitLab Personal Access Token。请打开左下角 **我的 → 账号与设置** 中的「🦊 GitLab 双向协同」卡片配置 Token 后即可自动拉取并回写评论。".to_owned()
        } else {
            match sync_gitlab_todos_for_owner(h, owner).await {
                Ok((fetched, dispatched)) => format!(
                    "🦊 GitLab 待办同步完成：本次扫描 **{fetched}** 条待办，新接管并派发 **{dispatched}** 条请求。"
                ),
                Err(_) => "⚠️ GitLab 待办同步失败，请检查「我的 → 账号与设置」中的 GitLab 地址与 Personal Access Token 是否有效。".to_owned(),
            }
        };
        return complete_chat_reply(h, owner, input, runtime_profile, task_context, &reply)
            .map(Some);
    }
    if is_gitlab_commit_query(trimmed) && !trimmed.starts_with('/') {
        let reply = execute_cloud_cron_report(h, owner, trimmed).await?;
        return complete_chat_reply(h, owner, input, runtime_profile, task_context, &reply)
            .map(Some);
    }

    // 3. Module A: Global System Pulse & User Node Fleet Query
    if is_node_fleet_query(trimmed) && !trimmed.starts_with('/') {
        let reply = {
            let db = h.db()?;
            format_user_node_fleet(&db, owner)?
        };
        return complete_chat_reply(h, owner, input, runtime_profile, task_context, &reply)
            .map(Some);
    }
    if (is_pulse_query(trimmed) && !trimmed.starts_with('/')) || trimmed == "/pulse" {
        let pulse = {
            let db = h.db()?;
            build_system_pulse(h, &db, owner)?
        };
        let reply = format_system_pulse(&pulse);
        return complete_chat_reply(h, owner, input, runtime_profile, task_context, &reply)
            .map(Some);
    }

    // 4. Module B: Clarification reply ("补充：..." or natural clarification supplement) for an awaiting cross-user task
    let natural_clarify = strip_prefix_any(
        trimmed,
        &[
            "补充信息：",
            "补充信息:",
            "补充说明：",
            "补充说明:",
            "补充：",
            "补充:",
            "补一下：",
            "补一下:",
            "补发：",
            "补发:",
            "复现步骤：",
            "复现步骤:",
            "报错日志：",
            "报错日志:",
        ],
    );
    if let Some(extra) = natural_clarify {
        if extra.is_empty() {
            return Ok(None);
        }
        let target_task: Option<(String, String, String, i64)> = {
            let db = h.db()?;
            if let Some(sel) = &input.selected_task {
                db.query_row(
                    "SELECT id, owner, summary, revision FROM tasks
                     WHERE id=?1 AND requester=?2 AND owner!=?2 AND status='awaiting_owner'",
                    params![sel.task_id, owner],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                )
                .optional()?
            } else {
                find_peer_task_for_requester(&db, owner, "awaiting_owner", extra)?
            }
        };
        if let Some((tid, task_owner, summary, rev)) = target_task {
            let reply_opt = {
                let mut db = h.db()?;
                let tx = db.transaction()?;
                let participates: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM user_nodes WHERE owner=?1)",
                    [&task_owner],
                    |r| r.get(0),
                )? || has_rules_for_owner(h, &task_owner);
                if (!participates && !is_task_managed(h, &tid))
                    || !crate::automation_policy::read(&tx, &task_owner)?.auto_accept
                    || requires_human_approval(h, &task_owner, &summary)
                {
                    None
                } else {
                    mark_task_managed(h, &tx, &tid)?;
                    let existing_details: Option<String> = tx
                        .query_row(
                            "SELECT content FROM task_details WHERE task_id=?1",
                            [&tid],
                            |r| r.get(0),
                        )
                        .optional()?;
                    let combined_instr = match existing_details {
                        Some(d) => format!("{d}\n补充信息：{extra}"),
                        None => {
                            let _ = tx.execute(
                                "INSERT OR IGNORE INTO task_details(task_id,author,content) VALUES (?1,?2,?3)",
                                params![tid, owner, extra],
                            );
                            extra.to_owned()
                        }
                    };
                    let (_from, _to) = next_chat_msg_target(owner, &task_owner);
                    insert_contact_assistant_msg(
                        &tx,
                        owner,
                        &task_owner,
                        &format!("🤝 [{owner} 的管家 → {task_owner} 的管家] 已收到对「{summary}」的补充信息：{extra}"),
                    )?;
                    accept_and_assign_owner_node(
                        &tx,
                        h,
                        &tid,
                        rev,
                        owner,
                        &task_owner,
                        &summary,
                        Some(&combined_instr),
                    )?;
                    let user_reply = format!(
                        "🤵 管家：已将你的补充信息（含附件）转交给 {task_owner} 的管家，对方已自动接手「{summary}」（#{}）并调度节点执行。",
                        short_id(&tid)
                    );
                    let msg_id = insert_chat_message(&tx, owner, "user", &input.message)?;
                    insert_chat_message(&tx, owner, "assistant", &user_reply)?;
                    if let Some(key) = input.request_id.as_deref() {
                        tx.execute(
                            "UPDATE chat_turns SET message_id=?3 WHERE owner=?1 AND request_id=?2 AND status='pending'",
                            params![owner, key, msg_id],
                        )?;
                    }
                    let mut response = json!({
                        "reply": user_reply,
                        "backend": h.runtime.label(),
                        "source": "sagent",
                        "proposal": Value::Null,
                        "runtime_profile": runtime_profile,
                        "task_context": task_context.cloned().unwrap_or(Value::Null),
                        "repository_context": Value::Null,
                    });
                    crate::chat_turns::complete(
                        &tx,
                        owner,
                        input.request_id.as_deref(),
                        &mut response,
                    )?;
                    tx.commit()?;
                    Some(Json(response))
                }
            };
            if let Some(resp) = reply_opt {
                return Ok(Some(resp));
            }
        }
    }

    // Explicit commands or natural-language acceptance/return bound to the selected revision
    // OR routed by the 3-Stage Historical Task Context Router when the user names the task.
    let exact_accept = matches!(
        trimmed,
        "/approve" | "验收通过" | "确认验收" | "确认验收通过" | "通过验收"
    );
    let named_accept = !exact_accept
        && (trimmed.ends_with("验收通过")
            || trimmed.ends_with("确认验收")
            || trimmed.ends_with("通过验收"));
    let accept_intent = exact_accept || named_accept;
    let return_reason = trimmed
        .strip_prefix("/return ")
        .or_else(|| {
            strip_prefix_any(
                trimmed,
                &[
                    "退回：",
                    "退回:",
                    "打回：",
                    "打回:",
                    "重做：",
                    "重做:",
                    "需要修改：",
                    "需要修改:",
                ],
            )
        })
        .map(str::trim)
        .filter(|reason| !reason.is_empty());
    let resolved_selection: Option<crate::task_context::Selection> = if accept_intent
        || return_reason.is_some()
    {
        if let Some(sel) = input.selected_task.clone() {
            Some(sel)
        } else if named_accept || return_reason.is_some() {
            let db = h.db()?;
            find_peer_task_for_requester(&db, owner, "ready_for_acceptance", trimmed)?.map(
                |(task_id, _, _, revision)| crate::task_context::Selection {
                    task_id,
                    expected_revision: revision,
                },
            )
        } else {
            None
        }
    } else {
        None
    };
    if let Some(selection) = resolved_selection {
        let mut db = h.db()?;
        let tx = db.transaction()?;
        let ready_task = tx.query_row(
            "SELECT t.owner,t.summary,a.node_id FROM tasks t JOIN node_assignments a ON a.task_id=t.id
             WHERE t.id=?1 AND t.requester=?2 AND t.owner!=?2 AND t.status='ready_for_acceptance' AND t.revision=?3",
            params![selection.task_id, owner, selection.expected_revision],
            |r| Ok((r.get::<_, String>(0)?,r.get::<_, String>(1)?,r.get::<_, Option<String>>(2)?)),
        ).optional()?;
        if let Some((task_owner, summary, node_id)) = ready_task {
            let tid = &selection.task_id;
            let next_rev = selection.expected_revision + 1;
            let user_reply = if accept_intent {
                tx.execute(
                    "UPDATE tasks SET status='completed', revision=?2 WHERE id=?1",
                    params![tid, next_rev],
                )?;
                crate::audit::record(
                    &tx,
                    &tid,
                    owner,
                    "approve",
                    Some("ready_for_acceptance"),
                    "completed",
                    next_rev,
                )?;
                let note = format!(
                    "🎉 管家：{owner} 已验收通过委派任务「{summary}」（#{}），协作已完成闭环！",
                    short_id(&tid)
                );
                insert_chat_message(&tx, &task_owner, "assistant", &note)?;
                format!(
                    "🤵 管家：已为你确认验收通过任务「{summary}」（#{}），已通知 {task_owner} 的管家完成归档闭环。",
                    short_id(&tid)
                )
            } else {
                let reason = return_reason.unwrap_or("请根据反馈继续修改");
                let prior_returns: i64 = tx
                    .query_row(
                        "SELECT COUNT(*) FROM events WHERE task_id=?1 AND action='return'",
                        [&tid],
                        |r| r.get(0),
                    )
                    .unwrap_or(0);
                tx.execute(
                    "UPDATE tasks SET status='working', result=NULL, revision=?2 WHERE id=?1",
                    params![tid, next_rev],
                )?;
                crate::audit::record(
                    &tx,
                    &tid,
                    owner,
                    "return",
                    Some("ready_for_acceptance"),
                    "working",
                    next_rev,
                )?;
                let total_returns = prior_returns + 1;
                if total_returns >= 2 {
                    let owner_msg = format!(
                        "🚨 **[需你本人接手 · 连续退回熔断]**：任务「{summary}」（#{}）已被 @{owner} 第 {total_returns} 次退回（本次意见：「{reason}」）。为防止管家自动重跑陷入死循环，已暂停自动调度节点，请你本人核对或直接在聊天框 `@{owner}` 对齐需求。",
                        short_id(&tid)
                    );
                    insert_chat_message(&tx, &task_owner, "assistant", &owner_msg)?;
                    format!(
                        "🤵 管家：已将任务「{summary}」（#{}）退回给 {task_owner}（原因：{reason}）。由于该任务已连续 {total_returns} 次退回，已触发系统底线熔断并升级为 **【🚨 需本人接手】** 通知 {task_owner} 本人直接与你对齐。",
                        short_id(&tid)
                    )
                } else if let Some(nid) = node_id.filter(|_| {
                    crate::automation_policy::read(&tx, &task_owner).is_ok_and(|p| p.auto_accept)
                        && !requires_human_approval(h, &task_owner, &summary)
                }) {
                    let instr = nodes::truncate_bytes(reason, 4000);
                    tx.execute(
                        "UPDATE node_assignments
                         SET task_revision=?2, instruction=?3, status='queued', authorization='delegation', lease_hash=NULL, lease_until=NULL, submission_hash=NULL, draft_request_id=NULL, updated_at=?4
                         WHERE task_id=?1",
                        params![tid, next_rev, instr, nodes::now_unix()],
                    )?;
                    let owner_msg = format!(
                        "🔁 管家：{owner} 退回了任务「{summary}」（#{}），修改意见：「{reason}」。已自动重新调度至你的节点 `{nid}` 返工。",
                        short_id(&tid)
                    );
                    insert_chat_message(&tx, &task_owner, "assistant", &owner_msg)?;
                    format!(
                        "🤵 管家：已将任务「{summary}」（#{}）退回给 {task_owner} 的管家（原因：{reason}），对方可按修改意见继续处理；仅在已授权时自动调度节点。",
                        short_id(&tid)
                    )
                } else {
                    let owner_msg = format!(
                        "🚨 **[需你本人接手 · 退回待处理]**：@{owner} 退回了任务「{summary}」（#{}），修改意见：「{reason}」。请你本人查看或调度节点处理。",
                        short_id(&tid)
                    );
                    insert_chat_message(&tx, &task_owner, "assistant", &owner_msg)?;
                    format!(
                        "🤵 管家：已将任务「{summary}」（#{}）退回给 {task_owner} 的管家（原因：{reason}），对方可按修改意见继续处理；仅在已授权时自动调度节点。",
                        short_id(&tid)
                    )
                }
            };
            let msg_id = insert_chat_message(&tx, owner, "user", &input.message)?;
            insert_chat_message(&tx, owner, "assistant", &user_reply)?;
            if let Some(key) = input.request_id.as_deref() {
                tx.execute(
                    "UPDATE chat_turns SET message_id=?3 WHERE owner=?1 AND request_id=?2 AND status='pending'",
                    params![owner, key, msg_id],
                )?;
            }
            let mut response = json!({
                "reply": user_reply,
                "backend": h.runtime.label(),
                "source": "sagent",
                "proposal": Value::Null,
                "runtime_profile": runtime_profile,
                "task_context": task_context.cloned().unwrap_or(Value::Null),
                "repository_context": Value::Null,
            });
            crate::chat_turns::complete(&tx, owner, input.request_id.as_deref(), &mut response)?;
            tx.commit()?;
            return Ok(Some(Json(response)));
        }
    }

    // 6. Direct `@colleague` messaging & cross-sagent task delegation from main chat
    if !trimmed.starts_with('/') && trimmed.contains('@') {
        if let Some(resp) =
            try_handle_colleague_at_mention(h, owner, input, runtime_profile, task_context).await?
        {
            return Ok(Some(resp));
        }
    }

    Ok(None)
}

#[derive(Deserialize)]
struct MuseAgentResponse {
    reply: String,
    #[serde(default)]
    tools: Vec<MuseToolCall>,
}

#[derive(Deserialize)]
struct MuseToolCall {
    tool: String,
    #[serde(default)]
    action: Option<String>,
    #[serde(default)]
    cron_id: Option<String>,
    #[serde(default)]
    daily_time_cst: Option<String>,
    #[serde(default)]
    interval_minutes: Option<u64>,
    #[serde(default)]
    instruction: Option<String>,
    #[serde(default)]
    deduplicate_ids: Vec<String>,
    #[serde(default)]
    rule_id: Option<String>,
    #[serde(default)]
    rule_body: Option<String>,
    #[serde(default)]
    node_id: Option<String>,
    #[serde(default)]
    topic_id: Option<String>,
    #[serde(default)]
    topic_title: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    soul_md: Option<String>,
    #[serde(default)]
    memory_append: Option<String>,
    #[serde(default)]
    memory_replace: Option<String>,
    #[serde(default)]
    ledger_id: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    notes_append: Option<String>,
    #[serde(default)]
    notes_replace: Option<String>,
}

async fn try_handle_muse_agent_loop(
    h: &Hub,
    owner: &str,
    input: &crate::messages::ChatInput,
    runtime_profile: &crate::profiles::RuntimeProfile,
    task_context: Option<&Value>,
) -> Result<Option<Json<Value>>> {
    let crate::Runtime::Dsh { url, token } = &h.runtime else {
        return Ok(None);
    };
    let selected_self_topic = input.selected_task.as_ref().filter(|_| {
        task_context
            .as_ref()
            .is_some_and(|c| c["requester"] == owner && c["owner"] == owner)
    });
    let blocking_shared_task = task_context
        .as_ref()
        .is_some_and(|c| c["requester"] != owner || c["owner"] != owner);
    if blocking_shared_task || input.selected_project.is_some() {
        return Ok(None);
    }
    let trimmed = input.message.trim();
    if [
        "记住：",
        "记住:",
        "把我的职责公开为",
        "允许助手用公开资料答复",
        "关闭公开自动答复",
        "关闭自动起草",
        "开启自动起草",
        "回复改为",
    ]
    .iter()
    .any(|kw| trimmed.starts_with(kw))
    {
        return Ok(None);
    }

    let (user_crons, user_rules, has_gitlab, muse_prof, user_ledgers, topic_outcomes_map) = h
        .automation
        .lock()
        .map(|g| {
            let crons: Vec<CronJob> = g
                .crons
                .iter()
                .filter(|c| c.owner == owner)
                .cloned()
                .collect();
            let rules: Vec<DispatchRule> = g
                .rules
                .iter()
                .filter(|r| r.owner == owner)
                .cloned()
                .collect();
            let gl = g
                .gitlab_configs
                .get(owner)
                .is_some_and(|c| !c.token.is_empty());
            let mp = g.muse_profiles.get(owner).cloned().unwrap_or_default();
            let ledgers: Vec<MuseLedgerEntry> = g
                .ledgers
                .iter()
                .filter(|l| l.owner == owner)
                .cloned()
                .collect();
            let outcomes = g.topic_outcomes.clone();
            (crons, rules, gl, mp, ledgers, outcomes)
        })
        .unwrap_or_default();

    let (nodes, contacts, recent_msgs, topics, recent_node_jobs, default_best_node) = {
        let db = h.db()?;
        let now = nodes::now_unix();
        let mut n_stmt = db.prepare(
            "SELECT id, name, capabilities, last_seen_unix FROM user_nodes WHERE owner=?1 ORDER BY rowid ASC",
        )?;
        let nodes: Vec<(String, String, String, bool)> = n_stmt
            .query_map([owner], |r| {
                let seen: Option<i64> = r.get(3)?;
                let online =
                    seen.is_some_and(|t| now.saturating_sub(t) <= nodes::ONLINE_WINDOW_SECONDS);
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, online))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        if nodes.is_empty()
            && user_crons.is_empty()
            && user_rules.is_empty()
            && user_ledgers.is_empty()
            && !has_gitlab
        {
            return Ok(None);
        }

        let mut contacts = Vec::new();
        for c in h.directory.contacts(owner) {
            let display: Option<String> = db
                .query_row(
                    "SELECT display_name FROM member_profiles WHERE owner=?1 AND status='active'",
                    [&c],
                    |r| r.get(0),
                )
                .optional()?;
            match display.filter(|d| !d.trim().is_empty()) {
                Some(d) => contacts.push(format!("{c}（{d}）")),
                None => contacts.push(c),
            }
        }

        let mut m_stmt = db.prepare(
            "SELECT role, content FROM (
                SELECT id, role, content FROM messages
                WHERE owner=?1 AND status='complete'
                ORDER BY id DESC LIMIT 10
             ) ORDER BY id ASC",
        )?;
        let recent_msgs: Vec<(String, String)> = m_stmt
            .query_map([owner], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        let mut t_stmt = db.prepare(
            "SELECT id, summary, status, COALESCE(result, '') FROM tasks
             WHERE requester=?1 AND owner=?1
             ORDER BY rowid DESC LIMIT 20",
        )?;
        let topics: Vec<(String, String, String, String)> = t_stmt
            .query_map([owner], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
            .into_iter()
            .filter(|(_, sum, _, _)| nodes::is_topic_summary(sum))
            .collect();

        let mut j_stmt = db.prepare(
            "SELECT task_id, node_id, status, instruction
             FROM node_assignments
             WHERE owner=?1
             ORDER BY updated_at DESC LIMIT 4",
        )?;
        let recent_node_jobs: Vec<(String, String, String, String, String)> = j_stmt
            .query_map([owner], |r| {
                let tid: String = r.get(0)?;
                let stage = h
                    .node_progress
                    .lock()
                    .ok()
                    .and_then(|g| g.get(&tid).map(|p| p.stage.clone()))
                    .unwrap_or_default();
                Ok((
                    tid,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    stage,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        let default_best = nodes::select_node_with_rule(h, &db, owner, trimmed)
            .ok()
            .flatten();
        (
            nodes,
            contacts,
            recent_msgs,
            topics,
            recent_node_jobs,
            default_best,
        )
    };

    let nodes_block = if nodes.is_empty() {
        "（当前未绑定 PC 节点）".to_owned()
    } else {
        nodes
            .iter()
            .map(|(id, name, caps, online)| {
                format!(
                    "- node_id=`{id}`, 名称=「{name}」, 状态={}, 能力=`{caps}`",
                    if *online { "在线" } else { "离线" }
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    let crons_block = if user_crons.is_empty() {
        "（当前无定时任务）".to_owned()
    } else {
        user_crons
            .iter()
            .map(|c| {
                let sched = match &c.daily_time_cst {
                    Some(hhmm) => format!("每天北京时间 {hhmm}"),
                    None => format!("每 {} 分钟", c.interval_minutes),
                };
                format!(
                    "- cron_id=`{}` | 周期={sched} | target=`{}` | 指令=「{}」",
                    c.id, c.target, c.instruction
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    let rules_block = if user_rules.is_empty() {
        "（当前无自定义分发规则）".to_owned()
    } else {
        user_rules
            .iter()
            .map(|r| format!("- rule_id=`{}` | 关键词=`{}` | 目标=`{}`", r.id, r.keyword, r.target_node_id))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let contacts_block = if contacts.is_empty() {
        "（无外部同事）".to_owned()
    } else {
        contacts.join("、")
    };
    let history_block = if recent_msgs.is_empty() {
        "（暂无历史对话）".to_owned()
    } else {
        recent_msgs
            .iter()
            .map(|(role, content)| {
                let speaker = if role == "user" { "用户" } else { "Agenct" };
                let snippet: String = nodes::strip_attachment_markers(content)
                    .chars()
                    .take(360)
                    .collect();
                format!("[{speaker}]\n{snippet}")
            })
            .collect::<Vec<_>>()
            .join("\n\n")
    };
    let topics_block = if topics.is_empty() {
        "（尚无任务脉络）".to_owned()
    } else {
        topics
            .iter()
            .map(|(id, sum, st, res)| {
                let outcome = topic_outcomes_map
                    .get(id)
                    .map(String::as_str)
                    .unwrap_or(res.as_str());
                let short_res: String = outcome
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
                    .chars()
                    .take(80)
                    .collect();
                if short_res.is_empty() {
                    format!("- `{id}` [{st}]：{sum}")
                } else {
                    format!("- `{id}` [{st}]：{sum}（进展/结论：{short_res}）")
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    let ledgers_block = if user_ledgers.is_empty() {
        "（当前尚无专项活账本，可在对话中随时通过 `update_ledger` 创建或追加项目台账、欠账队列、静默挂起项或健康/工作清单）".to_owned()
    } else {
        user_ledgers
            .iter()
            .map(|l| {
                let st_label = match l.status.as_str() {
                    "muted" => "🔇 静默挂起（除非用户主动提及或硬期限触发，否则不在早报/周报/顺带提醒中主动催促）",
                    "completed" => "✅ 已完结归档",
                    _ => "🟢 活跃跟进中",
                };
                let notes_preview: String = l.notes.chars().take(360).collect();
                format!(
                    "- ledger_id=`{}` | 专项=「{}」 | 状态={st_label}\n  台账内容：{notes_preview}",
                    l.id, l.title
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    let active_jobs_block = if recent_node_jobs.is_empty() {
        "（当前无近期节点作业记录）".to_owned()
    } else {
        recent_node_jobs
            .iter()
            .map(|(tid, nid, st, instr, stage)| {
                let short_instr: String = instr.chars().take(72).collect();
                if stage.is_empty() {
                    format!("- task=`{tid}` | node=`{nid}` | status=`{st}` | 指令=「{short_instr}」")
                } else {
                    format!(
                        "- task=`{tid}` | node=`{nid}` | status=`{st}` ({stage}) | 指令=「{short_instr}」"
                    )
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    };

    let clean_msg = nodes::strip_attachment_markers(trimmed);
    let msg_for_prompt = if clean_msg.is_empty() {
        trimmed
    } else {
        clean_msg.as_str()
    };

    let soul_snippet: String = muse_prof.soul_md.chars().take(800).collect();
    let memory_snippet: String = muse_prof.memory_md.chars().take(1200).collect();

    let prompt = format!(
        "[MUSE_AGENT_TURN]\n\
         你是用户的 7×24h 个人超级智能体「{}」（Meta Muse 核心大脑：多线意图消歧 + 活账本状态感知 + 单轮多工具联动）。\n\
         你是唯一与用户直接对话的大脑，你的 `reply` 字段会直接展示给用户，请始终用第一人称（「我」）、自然、干练、结论先行的中文与用户交流，绝不要使用冷冰冰的客服模板，也绝不要要求用户按特定格式复述。\n\n\
         === 核心认知纪律（Meta Muse 精髓，必须严格遵守） ===\n\
         1. **单会话多线精准消歧（跨时间/无主语指代解析）**：\n\
            用户在同一个主对话流里会同时交叉推进多件完全不同的事（代码开发、定时早报、商务项目、个人生活/健康、长期计划），且说话常省略主语或跨了几小时突然指代前面某条消息里的一个词（如「有些不用你继续提醒的怎么处理，比如这个抖音」「而且昨日没做的咋整」「答题试卷这个可以删掉了」「部署到线上」）。\n\
            你必须综合对比【最近对话历史】、【专项活账本 (ledgers)】、【已有任务脉络 (topics)】与【7×24h 定时任务 (crons)】，精准锁定用户当前这句话到底在指哪条线，绝不张冠李戴！\n\
         2. **个人约束与老规矩跨线自动套用 + 顺手后台收尾**：\n\
            回答任何问题（如安排事情、排查方案、日常规划）时，必须自动检索并套用 `SOUL.md`、`MEMORY.md` 和 `ledgers` 里的隐性个人约束与老规矩；如果用户的话里透露出某条专项的新进展或待办（例如聊日常时提到身体状况、提到某个项目的新线索、定下新规矩），你不仅要正面回答当前问题，还必须在同一轮的 `tools` 里顺手调用 `update_ledger` 或 `update_memory` 把它记进对应台账，并在回复末尾用一句话自然交代（「顺带交代一句后台收尾：已记入《...》」）。\n\
         3. **新规矩与「静默挂起 (muted)」立竿见影**：\n\
            每当用户纠正你的行为、立下新规矩（如「以后只顺延一条不催债」「必须有可核验的证据才算完成」）或要求某件事别再主动提（「这个别再提了」「静默挂起」），你必须在同一轮里同时调用 `update_memory`（写入长期规矩）、`update_ledger`（将该事项状态设为 `\"muted\"` 或更新台账规则）、以及必要时调用 `manage_cron`（同步修改定时任务的执行规则）。\n\n\
         === 核心人设 (SOUL.md) ===\n{soul_snippet}\n\n\
         === 长期记忆与个人规矩 (MEMORY.md) ===\n{memory_snippet}\n\n\
         === 全景工作台实时快照 (Workbench Snapshot) ===\n\
         【专项活账本与长期台账 (ledgers)】\n{ledgers_block}\n\n\
         【7×24h 定时任务 (crons)】\n{crons_block}\n\n\
         【已有任务脉络与最近成果 (topics)】\n{topics_block}\n\n\
         【近期节点作业执行状态 (active_node_jobs)】\n{active_jobs_block}\n\n\
         【绑定的 PC 节点与本地 Agent (nodes)】\n{nodes_block}\n\
         （注：agy、agy1、agy2、codex、claude、tmux、screen 等均是运行在用户 PC 节点上的本地开发 Agent 或终端窗口，不是外部人类同事。）\n\n\
         【个人管家规则 (rules)】\n{rules_block}\n\n\
         【通讯录同事】\n{contacts_block}\n\n\
         【最近对话历史】\n{history_block}\n\n\
         【用户最新消息】\n「{msg_for_prompt}」\n\n\
         === 可在单轮自由组合调用的工具列表 (`tools` 数组可含 0、1 或多个工具) ===\n\
         1. **管理专项活账本与台账 (`update_ledger`)**：\n\
            `{{\"tool\":\"update_ledger\",\"action\":\"upsert|mute|complete|delete\",\"ledger_id\":\"已有ledger_id或null\",\"title\":\"专项或台账名称（如：中标项目信息 / Go回炉计划 / 待问清单 / 抖音植入报价）\",\"status\":\"active|muted|completed\",\"notes_append\":\"要增量追加到该台账的一条记录或新规矩（保留历史）或null\",\"notes_replace\":\"当需要重排整份台账/盘清欠账队列时覆写的完整台账内容或null\"}}`\n\
            - 当用户说某件事「别再主动提醒 / 静默挂起」时，调用 `update_ledger` 将该事项设为 `\"status\":\"muted\"`；\n\
            - 当用户跟进某个专项、新增记录、盘清欠账、或顺带提到需记录的事项时，调用 `update_ledger` 落盘台账。\n\
         2. **更新管家命名、SOUL.md 或 MEMORY.md (`update_memory`)**：\n\
            `{{\"tool\":\"update_memory\",\"name\":\"新名字或null\",\"soul_md\":\"更新后的完整SOUL.md或null\",\"memory_append\":\"要追加写入MEMORY.md的一条长期规矩/个人偏好/环境事实或null\",\"memory_replace\":\"重整后的完整MEMORY.md或null\"}}`\n\
         3. **管理定时任务 (`manage_cron`)**：\n\
            `{{\"tool\":\"manage_cron\",\"action\":\"add|update|delete\",\"cron_id\":\"目标cron_id或null\",\"daily_time_cst\":\"HH:MM或null\",\"interval_minutes\":1440,\"instruction\":\"融合用户新规矩/顺延规则/精简要求后的完整定时任务指令\",\"deduplicate_ids\":[\"同时间段重复的cron_id\"]}}`\n\
            - 当用户要求简化/修改定时报告，或定下涉及每日推送的新规矩（如「只顺延一条不覆盖」「静默挂起某项不在早报提」）时，直接在 `reply` 给出处理结果，并在 `tools` 中同步调用 `manage_cron` 更新对应定时任务的 `instruction`（切勿派给 `dispatch_pc_node`）。\n\
         4. **实时拉取云端 GitLab 提交 (`fetch_gitlab_commits`)**：\n\
            `{{\"tool\":\"fetch_gitlab_commits\",\"instruction\":\"汇总要求\"}}`\n\
         5. **管理个人分发规则 (`manage_rule`)**：\n\
            `{{\"tool\":\"manage_rule\",\"action\":\"add|delete\",\"rule_id\":\"要删除的ID或null\",\"rule_body\":\"包含 <关键词> 的任务需要真人接手 | 别人委派任务必须提供 <材料> | 包含 <关键词> 的任务派给 <节点>\"}}`\n\
         6. **调度 PC 节点执行本机/代码/部署/终端任务 (`dispatch_pc_node`)**：\n\
            `{{\"tool\":\"dispatch_pc_node\",\"node_id\":\"homelab\",\"instruction\":\"结合对话历史、活账本与MEMORY补全所有省略指代后的完整自包含本机指令\",\"topic_id\":\"匹配的已有脉络ID或null\",\"topic_title\":\"匹配的脉络标题或新提炼的6~14字标题（以 📁 / 🖥️ / 💬 开头）\"}}`\n\
            - 仅当确实需要操作、查看、构建、测试或部署用户的 PC 节点 / 代码仓库 / 终端窗口（tmux） / 本地 Agent（agy/agy1/codex）时调用；\n\
            - 当用户说简短的「更新」「部署到线上」「继续」「帮我改一下」时，**必须**从【最近对话历史】与【已有任务脉络】中把指代的提交号（如 13dba7c）、仓库路径、具体问题和验收要求完整补全进 `instruction`！\n\n\
         请保持内部思考极度简短（最多两三句，不要反复起草），并立刻只输出一个严格合法的 JSON 对象（不要 Markdown 代码块包裹）：\n\
         `{{\"reply\":\"你直接对用户说的自然中文回复\",\"tools\":[]}}`",
        muse_prof.name
    );
    let prompt: String = prompt.chars().take(8000).collect();

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
            profile: runtime_profile,
            task_context: None,
            draft_revision: None,
            task_role: None,
            owner_context: None,
            profile_context: None,
        },
    )
    .await
    else {
        return Ok(None);
    };

    let raw = ans.reply.trim();
    let (Some(s), Some(e)) = (raw.find('{'), raw.rfind('}')) else {
        return Ok(None);
    };
    let Ok(parsed) = serde_json::from_str::<MuseAgentResponse>(&raw[s..=e]) else {
        return Ok(None);
    };
    if parsed.reply.trim().is_empty() {
        return Ok(None);
    }

    // If the user explicitly asked to delegate to a configured human colleague in Contacts without tools,
    // let the colleague delegation handler run if needed.
    if parsed.tools.is_empty()
        && contacts
            .iter()
            .any(|c| c.split('（').next().is_some_and(|id| trimmed.contains(id)))
        && (trimmed.contains("请让") || trimmed.contains("委托") || trimmed.contains("交给") || trimmed.contains('@'))
    {
        return Ok(None);
    }

    let mut final_reply = parsed.reply.trim().to_owned();
    let mut node_dispatch: Option<(String, String, bool, String, Option<String>, Option<String>)> =
        None;

    for call in &parsed.tools {
        match call.tool.as_str() {
            "manage_cron" => {
                let act = call.action.as_deref().unwrap_or("update").trim();
                if act == "update" && !user_crons.is_empty() {
                    let target_id = call
                        .cron_id
                        .as_deref()
                        .map(str::trim)
                        .filter(|id| user_crons.iter().any(|c| c.id == *id))
                        .map(str::to_owned)
                        .unwrap_or_else(|| user_crons[0].id.clone());
                    with_store_mut(h, |store| {
                        let mut updated_job: Option<CronJob> = None;
                        if let Some(c) = store
                            .crons
                            .iter_mut()
                            .find(|c| c.owner == owner && c.id == target_id)
                        {
                            if let Some(instr) = call
                                .instruction
                                .as_deref()
                                .map(str::trim)
                                .filter(|s| !s.is_empty() && *s != "null")
                            {
                                c.instruction = nodes::truncate_bytes(instr, 300);
                            }
                            if let Some(hhmm) = call
                                .daily_time_cst
                                .as_deref()
                                .map(str::trim)
                                .filter(|s| !s.is_empty() && *s != "null" && s.contains(':'))
                            {
                                c.daily_time_cst = Some(hhmm.to_owned());
                                c.interval_minutes = 1440;
                            } else if let Some(mins) =
                                call.interval_minutes.filter(|m| (1..=10080).contains(m))
                            {
                                if c.daily_time_cst.is_none() {
                                    c.interval_minutes = mins;
                                }
                            }
                            if is_gitlab_commit_query(&c.instruction) {
                                c.target = "cloud".into();
                            }
                            updated_job = Some(c.clone());
                        }
                        if let Some(ref up) = updated_job {
                            let is_gl = is_gitlab_commit_query(&up.instruction);
                            store.crons.retain(|c| {
                                if c.owner != owner || c.id == up.id {
                                    return true;
                                }
                                let explicit_dup =
                                    call.deduplicate_ids.iter().any(|d| d.trim() == c.id);
                                let auto_gl_dup = is_gl
                                    && is_gitlab_commit_query(&c.instruction)
                                    && c.daily_time_cst == up.daily_time_cst;
                                !(explicit_dup || auto_gl_dup)
                            });
                        }
                    })?;
                    h.automation_notify.notify_one();
                } else if act == "add" {
                    if let Some(instr) = call
                        .instruction
                        .as_deref()
                        .map(str::trim)
                        .filter(|s| !s.is_empty() && *s != "null")
                    {
                        let hhmm = call
                            .daily_time_cst
                            .as_deref()
                            .map(str::trim)
                            .filter(|s| !s.is_empty() && *s != "null" && s.contains(':'))
                            .map(str::to_owned);
                        let mins = if hhmm.is_some() {
                            1440
                        } else {
                            call.interval_minutes.unwrap_or(60).clamp(1, 10080)
                        };
                        let job = build_cron_job(owner, mins, hhmm, instr);
                        with_store_mut(h, |store| {
                            let is_gl = is_gitlab_commit_query(&job.instruction);
                            store.crons.retain(|c| {
                                !(c.owner == owner
                                    && c.daily_time_cst == job.daily_time_cst
                                    && c.interval_minutes == job.interval_minutes
                                    && (c.instruction == job.instruction
                                        || (is_gl && is_gitlab_commit_query(&c.instruction))))
                            });
                            store.crons.push(job);
                        })?;
                        h.automation_notify.notify_one();
                    }
                } else if act == "delete" {
                    let mut to_remove: Vec<String> = call
                        .deduplicate_ids
                        .iter()
                        .map(|s| s.trim().to_owned())
                        .filter(|s| !s.is_empty())
                        .collect();
                    if let Some(cid) = call
                        .cron_id
                        .as_deref()
                        .map(str::trim)
                        .filter(|s| !s.is_empty() && *s != "null")
                    {
                        if !to_remove.iter().any(|x| x == cid) {
                            to_remove.push(cid.to_owned());
                        }
                    }
                    if !to_remove.is_empty() {
                        with_store_mut(h, |store| {
                            store
                                .crons
                                .retain(|c| !(c.owner == owner && to_remove.iter().any(|id| id == &c.id)));
                        })?;
                    }
                }
            }
            "fetch_gitlab_commits" => {
                let instr = call
                    .instruction
                    .as_deref()
                    .unwrap_or("汇总最近 24 小时 GitLab 提交");
                if let Ok(report) = execute_cloud_cron_report(h, owner, instr).await {
                    if !report.trim().is_empty() && !final_reply.contains(report.trim()) {
                        final_reply = format!("{final_reply}\n\n---\n\n{}", report.trim());
                    }
                }
            }
            "manage_rule" => {
                let act = call.action.as_deref().unwrap_or("add").trim();
                if act == "add" {
                    if let Some(body) = call
                        .rule_body
                        .as_deref()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                    {
                        let rule_opt = {
                            let db = h.db()?;
                            parse_rule_command(&db, owner, body).ok()
                        };
                        if let Some(rule) = rule_opt {
                            with_store_mut(h, |store| store.rules.push(rule))?;
                        }
                    }
                } else if act == "delete" {
                    if let Some(rid) = call
                        .rule_id
                        .as_deref()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                    {
                        with_store_mut(h, |store| {
                            store.rules.retain(|r| !(r.owner == owner && r.id == rid));
                        })?;
                    }
                }
            }
            "dispatch_pc_node" => {
                let picked_node = call
                    .node_id
                    .as_deref()
                    .map(str::trim)
                    .and_then(|nid| {
                        nodes
                            .iter()
                            .find(|(id, name, _, _)| id.eq_ignore_ascii_case(nid) || name == nid)
                            .map(|(id, name, _, online)| (id.clone(), name.clone(), *online))
                    })
                    .or_else(|| {
                        default_best_node
                            .as_ref()
                            .map(|(id, name, online, _)| (id.clone(), name.clone(), *online))
                    })
                    .or_else(|| {
                        nodes
                            .iter()
                            .find(|(_, _, _, online)| *online)
                            .or_else(|| nodes.first())
                            .map(|(id, name, _, online)| (id.clone(), name.clone(), *online))
                    });
                if let Some((node_id, node_name, online)) = picked_node {
                    let base_instr = call
                        .instruction
                        .as_deref()
                        .map(str::trim)
                        .filter(|s| !s.is_empty() && *s != "null")
                        .unwrap_or(trimmed);
                    let instr = nodes::preserve_attachment_markers(trimmed, base_instr);
                    let explicit_sel = selected_self_topic.map(|s| s.task_id.to_owned());
                    let tid = explicit_sel.or_else(|| {
                        call.topic_id
                            .as_deref()
                            .map(str::trim)
                            .filter(|id| {
                                !id.is_empty()
                                    && *id != "null"
                                    && topics.iter().any(|(t, _, _, _)| t == id)
                            })
                            .map(str::to_owned)
                    });
                    let title = call
                        .topic_title
                        .as_deref()
                        .map(str::trim)
                        .filter(|t| !t.is_empty() && *t != "null")
                        .map(|t| {
                            let clean: String = t.chars().take(28).collect();
                            if nodes::is_topic_summary(&clean) {
                                clean
                            } else {
                                format!("💬 {clean}")
                            }
                        });
                    node_dispatch = Some((node_id, node_name, online, instr, tid, title));
                }
            }
            "update_ledger" => {
                let act = call.action.as_deref().unwrap_or("upsert").trim();
                let target_id = call
                    .ledger_id
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty() && *s != "null")
                    .map(str::to_owned);
                let target_title = call
                    .title
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty() && *s != "null")
                    .map(|s| nodes::truncate_bytes(s, 96));

                if act == "delete" {
                    with_store_mut(h, |store| {
                        store.ledgers.retain(|l| {
                            if l.owner != owner {
                                return true;
                            }
                            let id_match = target_id.as_deref().is_some_and(|id| l.id == id);
                            let title_match =
                                target_title.as_deref().is_some_and(|t| l.title == t);
                            !(id_match || title_match)
                        });
                    })?;
                } else {
                    let next_status = match act {
                        "mute" => Some("muted".to_owned()),
                        "complete" => Some("completed".to_owned()),
                        _ => call
                            .status
                            .as_deref()
                            .map(str::trim)
                            .filter(|s| matches!(*s, "active" | "muted" | "completed"))
                            .map(str::to_owned),
                    };
                    with_store_mut(h, |store| {
                        let existing = store.ledgers.iter_mut().find(|l| {
                            l.owner == owner
                                && (target_id.as_deref().is_some_and(|id| l.id == id)
                                    || target_title.as_deref().is_some_and(|t| {
                                        l.title.eq_ignore_ascii_case(t)
                                            || l.title.contains(t)
                                            || t.contains(&l.title)
                                    }))
                        });
                        if let Some(entry) = existing {
                            if let Some(ref t) = target_title {
                                entry.title = t.clone();
                            }
                            if let Some(ref st) = next_status {
                                entry.status = st.clone();
                            }
                            if let Some(rep) = call
                                .notes_replace
                                .as_deref()
                                .map(str::trim)
                                .filter(|s| !s.is_empty() && *s != "null")
                            {
                                entry.notes = nodes::truncate_bytes(rep, 6000);
                            } else if let Some(app) = call
                                .notes_append
                                .as_deref()
                                .map(str::trim)
                                .filter(|s| !s.is_empty() && *s != "null")
                            {
                                let line = if app.starts_with('-') || app.starts_with('#') {
                                    app.to_owned()
                                } else {
                                    format!("- {app}")
                                };
                                if !entry.notes.contains(&line) {
                                    entry.notes = if entry.notes.trim().is_empty() {
                                        nodes::truncate_bytes(&line, 6000)
                                    } else {
                                        nodes::truncate_bytes(
                                            &format!("{}\n{line}", entry.notes.trim_end()),
                                            6000,
                                        )
                                    };
                                }
                            }
                            entry.updated_at = nodes::now_unix();
                        } else if let Some(title) = target_title.clone() {
                            let notes = call
                                .notes_replace
                                .as_deref()
                                .or(call.notes_append.as_deref())
                                .map(str::trim)
                                .filter(|s| !s.is_empty() && *s != "null")
                                .map(|s| nodes::truncate_bytes(s, 6000))
                                .unwrap_or_default();
                            store.ledgers.push(MuseLedgerEntry {
                                id: format!("ldg-{}", &Uuid::new_v4().simple().to_string()[..6]),
                                owner: owner.to_owned(),
                                title,
                                status: next_status.unwrap_or_else(|| "active".to_owned()),
                                notes,
                                updated_at: nodes::now_unix(),
                            });
                        }
                    })?;
                }
                record_muse_audit(
                    h,
                    owner,
                    "update_ledger",
                    "auto_approved",
                    &format!(
                        "更新专项活账本「{}」",
                        call.title
                            .as_deref()
                            .or(call.ledger_id.as_deref())
                            .unwrap_or("专项台账")
                    ),
                    call.notes_append
                        .as_deref()
                        .or(call.notes_replace.as_deref())
                        .or(call.status.as_deref())
                        .unwrap_or("已同步更新台账状态"),
                );
            }
            "update_memory" => {
                with_store_mut(h, |store| {
                    let entry = store.muse_profiles.entry(owner.to_owned()).or_default();
                    if let Some(n) = call
                        .name
                        .as_deref()
                        .map(str::trim)
                        .filter(|s| !s.is_empty() && *s != "null")
                    {
                        entry.name = nodes::truncate_bytes(n, 48);
                    }
                    if let Some(s) = call
                        .soul_md
                        .as_deref()
                        .map(str::trim)
                        .filter(|s| !s.is_empty() && *s != "null")
                    {
                        entry.soul_md = nodes::truncate_bytes(s, 8000);
                    }
                    if let Some(rep) = call
                        .memory_replace
                        .as_deref()
                        .map(str::trim)
                        .filter(|s| !s.is_empty() && *s != "null")
                    {
                        entry.memory_md = nodes::truncate_bytes(rep, 12000);
                    } else if let Some(m) = call
                        .memory_append
                        .as_deref()
                        .map(str::trim)
                        .filter(|s| !s.is_empty() && *s != "null")
                    {
                        let bullet = if m.starts_with('-') {
                            m.to_owned()
                        } else {
                            format!("- {m}")
                        };
                        if !entry.memory_md.contains(&bullet) {
                            entry.memory_md = nodes::truncate_bytes(
                                &format!("{}\n{bullet}", entry.memory_md.trim_end()),
                                12000,
                            );
                        }
                    }
                    entry.updated_at = nodes::now_unix();
                })?;
                record_muse_audit(
                    h,
                    owner,
                    "update_memory",
                    "auto_approved",
                    "更新管家命名与 SOUL / MEMORY",
                    call.memory_append
                        .as_deref()
                        .or(call.memory_replace.as_deref())
                        .or(call.name.as_deref())
                        .unwrap_or("已同步更新长期记忆与人设"),
                );
            }
            _ => {}
        }
    }
    for call in &parsed.tools {
        match call.tool.as_str() {
            "manage_cron" => {
                record_muse_audit(
                    h,
                    owner,
                    "manage_cron",
                    "auto_approved",
                    &format!(
                        "管理预定任务（{}）",
                        call.action.as_deref().unwrap_or("update")
                    ),
                    call.instruction
                        .as_deref()
                        .or(call.cron_id.as_deref())
                        .unwrap_or("已更新定时任务配置"),
                );
            }
            "manage_rule" => {
                record_muse_audit(
                    h,
                    owner,
                    "manage_rule",
                    "auto_approved",
                    "更新个人安全与分发规则",
                    call.rule_body
                        .as_deref()
                        .or(call.rule_id.as_deref())
                        .unwrap_or("已更新规则"),
                );
            }
            "dispatch_pc_node" => {
                record_muse_audit(
                    h,
                    owner,
                    "dispatch_pc_node",
                    "auto_approved",
                    &format!(
                        "调度节点「{}」执行任务",
                        call.node_id.as_deref().unwrap_or("homelab")
                    ),
                    call.instruction.as_deref().unwrap_or(trimmed),
                );
            }
            _ => {}
        }
    }

    if let Some((node_id, node_name, online, summary, tid, title)) = node_dispatch {
        let task_id = {
            let mut db = h.db()?;
            let tx = db.transaction()?;
            let (task_id, _) = nodes::dispatch_chat_task_with_hint(
                &tx,
                owner,
                &node_id,
                &node_name,
                online,
                &summary,
                tid.as_deref(),
                title.as_deref(),
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
            task_id
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
        return nodes::finalize_chat_node_turn(
            h,
            owner,
            &task_id,
            &final_reply,
            input.request_id.as_deref(),
            online,
            base_response,
        )
        .await
        .map(Some);
    }

    complete_chat_reply(h, owner, input, runtime_profile, task_context, &final_reply).map(Some)
}

fn member_display_name(db: &Connection, owner: &str) -> String {
    db.query_row(
        "SELECT display_name FROM member_profiles WHERE owner=?1 AND status='active'",
        [owner],
        |r| r.get::<_, String>(0),
    )
    .optional()
    .ok()
    .flatten()
    .map(|s| s.trim().to_owned())
    .filter(|s| !s.is_empty())
    .unwrap_or_else(|| owner.to_owned())
}

fn starts_with_ignore_ascii_case_utf8(haystack: &str, needle: &str) -> Option<usize> {
    let mut h_chars = haystack.chars();
    for nc in needle.chars() {
        let hc = h_chars.next()?;
        if hc != nc && !hc.eq_ignore_ascii_case(&nc) {
            return None;
        }
    }
    Some(haystack.len() - h_chars.as_str().len())
}

fn extract_colleague_at_mention(
    h: &Hub,
    db: &Connection,
    sender: &str,
    text: &str,
) -> Result<Option<(String, String, String)>> {
    let mut aliases: Vec<(String, String, String)> = Vec::new();
    let gl_usernames: HashMap<String, String> = h
        .automation
        .lock()
        .ok()
        .map(|g| {
            g.gitlab_configs
                .iter()
                .filter_map(|(k, v)| {
                    let u = v.gitlab_username.trim();
                    (!u.is_empty()).then(|| (k.clone(), u.to_owned()))
                })
                .collect()
        })
        .unwrap_or_default();

    for contact in h.directory.contacts(sender) {
        let active: bool = db
            .query_row(
                "SELECT COALESCE((SELECT status='active' FROM member_profiles WHERE owner=?1), 1)",
                [&contact],
                |r| r.get(0),
            )
            .unwrap_or(false);
        if !active {
            continue;
        }
        let disp = member_display_name(db, &contact);
        if !disp.is_empty() {
            aliases.push((disp.clone(), contact.clone(), disp.clone()));
        }
        if let Some(gl_u) = gl_usernames.get(&contact) {
            if !gl_u.eq_ignore_ascii_case(&disp) {
                aliases.push((gl_u.clone(), contact.clone(), disp.clone()));
            }
        }
        if !contact.eq_ignore_ascii_case(&disp) {
            aliases.push((contact.clone(), contact.clone(), disp));
        }
    }
    if aliases.is_empty() {
        return Ok(None);
    }
    aliases.sort_by(|a, b| b.0.len().cmp(&a.0.len()));

    for (byte_idx, ch) in text.char_indices() {
        if ch != '@' {
            continue;
        }
        if byte_idx > 0 {
            if let Some(prev) = text[..byte_idx].chars().next_back() {
                if prev.is_ascii_alphanumeric() {
                    continue;
                }
            }
        }
        let after_at = &text[byte_idx + 1..];
        for (alias, target_id, target_name) in &aliases {
            let Some(matched_bytes) = starts_with_ignore_ascii_case_utf8(after_at, alias) else {
                continue;
            };
            let rest = &after_at[matched_bytes..];
            let alias_ends_ascii = alias
                .chars()
                .next_back()
                .is_some_and(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'));
            if alias_ends_ascii {
                if let Some(next_ch) = rest.chars().next() {
                    if next_ch.is_ascii_alphanumeric() || matches!(next_ch, '_' | '-') {
                        continue;
                    }
                }
            }
            let before = text[..byte_idx].trim();
            let before_stripped = [
                "帮我发消息给",
                "帮我转告",
                "帮我通知",
                "帮我告诉",
                "帮我找",
                "帮我给",
                "帮我向",
                "帮我对",
                "发消息给",
                "发个消息给",
                "委派给",
                "派活给",
                "转达给",
                "转告",
                "通知",
                "告诉",
                "交给",
                "派给",
                "发给",
                "麻烦",
                "给",
                "找",
                "向",
                "对",
                "跟",
                "和",
                "请",
                "让",
                "叫",
            ]
            .iter()
            .find_map(|p| before.strip_prefix(p))
            .unwrap_or(before)
            .trim();

            let after_trimmed = rest.trim_start();
            let after_stripped = [
                "说一声：",
                "说一声:",
                "说一声",
                "说一下：",
                "说一下:",
                "说一下",
                "发个消息：",
                "发个消息:",
                "发消息：",
                "发消息:",
                "留言：",
                "留言:",
                "说：",
                "说:",
                "讲：",
                "讲:",
            ]
            .iter()
            .find_map(|p| after_trimmed.strip_prefix(p))
            .unwrap_or(after_trimmed)
            .trim_start_matches(['：', ':', '，', ',', '、', ' '])
            .trim();

            let cleaned = match (before_stripped.is_empty(), after_stripped.is_empty()) {
                (true, true) => String::new(),
                (true, false) => after_stripped.to_owned(),
                (false, true) => before_stripped.to_owned(),
                (false, false) => format!("{before_stripped} {after_stripped}"),
            };
            return Ok(Some((target_id.clone(), target_name.clone(), cleaned)));
        }
    }
    Ok(None)
}

fn is_colleague_task_request(raw_input: &str, clean_body: &str) -> bool {
    let trimmed = clean_body.trim();
    if trimmed.is_empty() {
        return false;
    }
    for ack in [
        "回复",
        "好的",
        "收到",
        "明白",
        "谢谢",
        "多谢",
        "没问题",
        "知道了",
        "辛苦",
        "ok",
        "OK",
    ] {
        if trimmed.starts_with(ack) && trimmed.chars().count() <= 36 {
            return false;
        }
    }
    let lower = trimmed.to_ascii_lowercase();
    [
        "帮",
        "查",
        "看",
        "修",
        "改",
        "做",
        "处理",
        "排查",
        "分析",
        "测试",
        "确认",
        "核对",
        "检查",
        "解决",
        "实现",
        "开发",
        "部署",
        "构建",
        "脚本",
        "报错",
        "失败",
        "问题",
        "原因",
        "为什么",
        "怎么",
        "任务",
        "派活",
        "委派",
        "[图片附件:",
    ]
    .iter()
    .any(|kw| trimmed.contains(kw) || raw_input.contains(kw))
        || ["bug", "issue", "mr", "api", "fix", "error", "review"]
            .iter()
            .any(|kw| lower.contains(kw))
}

async fn generate_peer_sagent_reply(
    h: &Hub,
    sender_name: &str,
    target_id: &str,
    target_name: &str,
    clean_body: &str,
    is_task: bool,
    auto_node_name: Option<&str>,
    nodes_online: usize,
    nodes_total: usize,
    active_tasks: usize,
    public_info: &str,
    user_rules_summary: &str,
    pre_takeover: Option<&HumanTakeoverReason>,
    target_profile: &crate::profiles::RuntimeProfile,
) -> (String, Option<HumanTakeoverReason>) {
    if let Some(takeover) = pre_takeover {
        let reply = format!(
            "已收到来自 @{sender_name} 的消息：「{clean_body}」。\n由于该事项 **{}**，我作为 {target_name} 的管家不越权代为承诺或拍板，已标记为 **【🚨 需本人接手】** 置顶通知 @{target_name} 本人，待其确认后直接回复你。",
            takeover.reason
        );
        return (reply, Some(takeover.clone()));
    }

    let action_context = if let Some(node_name) = auto_node_name {
        format!("已自动接手该协作任务，并调度 {target_name} 的资源节点「{node_name}」开始执行")
    } else if is_task {
        format!("已为 {target_name} 创建待办协作任务，并同步通知到 {target_name} 的主对话流")
    } else {
        format!("已收到该消息并同步至 {target_name} 的主对话流")
    };

    if let crate::Runtime::Dsh { url, token } = &h.runtime {
        let prompt = format!(
            "你是「{target_name}」的 7×24h 专属 Super-Agent（云端管家）。现在同事「@{sender_name}」通过管家向你发来消息：\n\
             「{clean_body}」\n\n\
             【你主人 {target_name} 的当前公开协作状态與个人规则】\n\
             - 公开协作资料：{public_info}\n\
             - 个人定制规则：{user_rules_summary}\n\
             - 资源 PC 节点：{nodes_online} 台在线 / 共 {nodes_total} 台\n\
             - 当前进行中任务：{active_tasks} 项\n\
             - 系统已执行动作：{action_context}\n\n\
             【重要输出协议（Layer 3 置信度与边界自检）】\n\
             1) 如果该消息是打招呼、连通性测试、状态问询、或可以依据上述公开资料与系统动作直接回应，请直接以「{target_name} 的 7×24h 管家」第一人称口吻给出自然、礼貌、切题的中文回复（2～4 句话）。\n\
             2) 如果该消息需要 {target_name} 本人做出主观决策、排期/交付承诺、线上变更授权，或依赖你不知道的线下私有信息，你绝不能替主人拍板答应！请必须在回复最开头输出 `[NEED_HUMAN]` 标记，紧接着用 1～2 句话礼貌说明该问题需 {target_name} 本人确认并已为你转交本人。"
        );
        let dsh = crate::runtime::DshRuntime {
            client: &h.client,
            url,
            token,
        };
        let sid = Uuid::new_v4().to_string();
        if let Ok(ans) = crate::runtime::Runtime::turn(
            &dsh,
            crate::runtime::TurnRequest {
                kind: crate::runtime::TurnKind::Chat,
                message: &prompt,
                session_id: &sid,
                history: &[],
                profile: target_profile,
                task_context: None,
                draft_revision: None,
                task_role: None,
                owner_context: None,
                profile_context: None,
            },
        )
        .await
        {
            let trimmed = ans.reply.trim();
            if !trimmed.is_empty() {
                if let Some(after_tag) = trimmed.strip_prefix("[NEED_HUMAN]") {
                    let clean_reply = after_tag.trim();
                    let final_reply = if clean_reply.is_empty() {
                        format!(
                            "已收到来自 @{sender_name} 的消息：「{clean_body}」。该事项需 @{target_name} 本人确认，已标记为【🚨 需本人接手】并通知本人。"
                        )
                    } else {
                        clean_reply.to_owned()
                    };
                    return (
                        final_reply,
                        Some(HumanTakeoverReason {
                            layer: "llm-self-check",
                            reason: "管家语义自检判定需本人拍板确认".to_owned(),
                        }),
                    );
                }
                return (trimmed.to_owned(), None);
            }
        }
    }
    let _ = target_id;
    if let Some(node_name) = auto_node_name {
        (
            format!(
                "收到！我是 {target_name} 的 7×24h 管家，已收到来自 @{sender_name} 的协作请求「{clean_body}」，并已自动接手调度至 {target_name} 的节点「{node_name}」执行，完成后会第一时间将结果同步给你。"
            ),
            None,
        )
    } else if is_task {
        let takeover = (nodes_online == 0).then(|| HumanTakeoverReason {
            layer: "system-base",
            reason: "需要处理协作任务但当前无在线 PC 节点".to_owned(),
        });
        (
            format!(
                "收到！我是 {target_name} 的 7×24h 管家，已记录来自 @{sender_name} 的协作请求「{clean_body}」并通知 {target_name} 本人跟进（当前在线 PC 节点 {nodes_online}/{nodes_total} 台，进行中任务 {active_tasks} 项），有进展会立即同步给你。"
            ),
            takeover,
        )
    } else {
        (
            format!(
                "收到！我是 {target_name} 的 7×24h 云端管家，已收到 @{sender_name} 发来的消息：「{clean_body}」，并已同步至 {target_name} 的主聊天窗口（当前在线 PC 节点 {nodes_online}/{nodes_total} 台）。有需要协作的任务或问题随时 @我！"
            ),
            None,
        )
    }
}

async fn try_handle_colleague_at_mention(
    h: &Hub,
    owner: &str,
    input: &crate::messages::ChatInput,
    runtime_profile: &crate::profiles::RuntimeProfile,
    task_context: Option<&Value>,
) -> Result<Option<Json<Value>>> {
    let matched = {
        let db = h.db()?;
        extract_colleague_at_mention(h, &db, owner, input.message.trim())?
    };
    let Some((target_id, target_name, clean_body)) = matched else {
        return Ok(None);
    };
    if clean_body.is_empty() {
        let hint = format!(
            "🤵 管家：请在 **@{target_name}** 后面直接输入要发送的消息或要委派的任务，例如：\n- `@{target_name} 帮我看下登录接口报错`\n- `@{target_name} 今晚十点准时发版`"
        );
        return complete_chat_reply(h, owner, input, runtime_profile, task_context, &hint)
            .map(Some);
    }

    let is_task = is_colleague_task_request(&input.message, &clean_body);
    let pre_takeover = evaluate_human_takeover_policy(h, &target_id, &clean_body);
    let user_rules_summary = h
        .automation
        .lock()
        .ok()
        .map(|g| {
            let list: Vec<String> = g
                .rules
                .iter()
                .filter(|r| r.owner == target_id)
                .map(|r| {
                    if r.require_human_approval {
                        format!("关键词 `{}` 必须本人确认", r.keyword)
                    } else if let Some(req) = &r.require_clarification {
                        format!("关键词 `{}` 需提供 `{req}`", r.keyword)
                    } else {
                        format!("关键词 `{}` 派给 `{}`", r.keyword, r.target_node_id)
                    }
                })
                .collect();
            if list.is_empty() {
                "使用系统默认底线规则".to_owned()
            } else {
                list.join("；")
            }
        })
        .unwrap_or_else(|| "使用系统默认底线规则".to_owned());

    let (
        sender_name,
        nodes_online,
        nodes_total,
        active_tasks,
        public_info,
        target_profile,
        auto_node_name,
    ) = {
        let db = h.db()?;
        let s_name = member_display_name(&db, owner);
        let pulse = build_system_pulse(h, &db, &target_id)?;
        let n_online = pulse["nodes_online"].as_u64().unwrap_or(0) as usize;
        let n_total = pulse["nodes_total"].as_u64().unwrap_or(0) as usize;
        let a_tasks = pulse["active_tasks_count"].as_u64().unwrap_or(0) as usize;
        let collab = crate::collaboration::read(&db, &target_id).unwrap_or_default();
        let mut pub_parts = Vec::new();
        if !collab.public.team.trim().is_empty() {
            pub_parts.push(format!("团队：{}", collab.public.team.trim()));
        }
        if !collab.public.responsibilities.trim().is_empty() {
            pub_parts.push(format!("职责：{}", collab.public.responsibilities.trim()));
        }
        if !collab.public.help_with.trim().is_empty() {
            pub_parts.push(format!("可协助：{}", collab.public.help_with.trim()));
        }
        let pub_str = if pub_parts.is_empty() {
            "未设置公开职责说明".to_owned()
        } else {
            pub_parts.join("；")
        };
        let t_prof = crate::profiles::route(&db, &target_id, None, None)?;
        let can_auto = is_task
            && pre_takeover.is_none()
            && crate::automation_policy::read(&db, &target_id).is_ok_and(|p| p.auto_accept)
            && !requires_human_approval(h, &target_id, &clean_body)
            && requirement_missing_reason(h, &db, &target_id, &clean_body, Some(&clean_body))?
                .is_none();
        let picked_name = if can_auto {
            nodes::select_node_with_rule(h, &db, &target_id, &clean_body)?
                .map(|(_, n_name, _, _)| n_name)
        } else {
            None
        };
        (
            s_name,
            n_online,
            n_total,
            a_tasks,
            pub_str,
            t_prof,
            picked_name,
        )
    };

    let (peer_sagent_reply, escalated_reason) = generate_peer_sagent_reply(
        h,
        &sender_name,
        &target_id,
        &target_name,
        &clean_body,
        is_task,
        auto_node_name.as_deref(),
        nodes_online,
        nodes_total,
        active_tasks,
        &public_info,
        &user_rules_summary,
        pre_takeover.as_ref(),
        &target_profile,
    )
    .await;

    let mut db = h.db()?;
    let tx = db.transaction()?;
    let contact_req_id = format!("at-{}", Uuid::new_v4().simple());
    tx.execute(
        "INSERT INTO contact_messages(sender,recipient,author_kind,body,request_id,created_at)
         VALUES (?1,?2,'human',?3,?4,CURRENT_TIMESTAMP)",
        params![owner, target_id, clean_body, contact_req_id],
    )?;
    let human_msg_id = tx.last_insert_rowid();
    tx.execute(
        "INSERT INTO contact_reads(owner,contact,through_id) VALUES (?1,?2,?3)
         ON CONFLICT(owner,contact) DO UPDATE SET through_id=MAX(through_id,excluded.through_id)",
        params![target_id, owner, human_msg_id],
    )?;
    insert_contact_assistant_msg(&tx, &target_id, owner, &peer_sagent_reply)?;

    let reply = if is_task {
        let first_line = clean_body
            .lines()
            .find(|l| !l.trim().is_empty() && !l.trim_start().starts_with("[图片附件:"))
            .unwrap_or(&clean_body)
            .trim();
        let summary_clean = nodes::truncate_bytes(first_line, 240).to_owned();
        let details_opt = (clean_body.contains('\n') || clean_body.contains("[图片附件:"))
            .then_some(clean_body.as_str());
        let task_id = Uuid::new_v4().to_string();
        tx.execute(
            "INSERT INTO tasks(id,requester,owner,summary,status,revision)
             VALUES (?1,?2,?3,?4,'awaiting_owner',0)",
            params![task_id, owner, target_id, summary_clean],
        )?;
        if let Some(det) = details_opt {
            tx.execute(
                "INSERT INTO task_details(task_id,author,content) VALUES (?1,?2,?3)",
                params![task_id, owner, det],
            )?;
        }
        crate::audit::record(&tx, &task_id, owner, "delegate", None, "awaiting_owner", 0)?;
        on_task_delegated(
            &tx,
            h,
            &task_id,
            owner,
            &target_id,
            &summary_clean,
            details_opt,
        )?;
        let current_status: String =
            tx.query_row("SELECT status FROM tasks WHERE id=?1", [&task_id], |r| {
                r.get(0)
            })?;
        if current_status == "working" {
            format!(
                "🤵 管家：已直接转达给 **@{target_name}**，对方管家已自动接手任务「{summary_clean}」（#{sid}）并调度节点处理。\n\n---\n\n🤖 **[@{target_name} 的管家 → 你的管家]**：\n{peer_sagent_reply}",
                sid = short_id(&task_id)
            )
        } else if let Some(ref esc) = escalated_reason {
            // If `on_task_delegated` did not already insert the takeover notice, insert one now
            if pre_takeover.is_none()
                || !crate::automation_policy::read(&tx, &target_id).is_ok_and(|p| p.auto_accept)
            {
                let notify_target = format!(
                    "🚨 **[需你本人接手 · {}]** 来自 **@{sender_name}**（任务 #{sid}）：\n> {clean_body}\n\n🤵 **[你的管家回执]**：\n{peer_sagent_reply}\n\n👉 请在下方聊天框直接输入 `@{sender_name} 你的回复` 由你本人拍板或处理。",
                    esc.reason,
                    sid = short_id(&task_id)
                );
                insert_chat_message(&tx, &target_id, "assistant", &notify_target)?;
            }
            format!(
                "🤵 管家：已发给 **@{target_name}** 并创建协作任务「{summary_clean}」（#{sid}）。\n\n---\n\n🚨 **[@{target_name} 的管家 · 已转交本人接手]**：\n{peer_sagent_reply}",
                sid = short_id(&task_id)
            )
        } else {
            let notify_target = format!(
                "🤖 **[管家已代为响应 · 来自 @{sender_name} 的协作消息]**（任务 #{sid}）：\n> {clean_body}\n\n🤵 **[你的管家已代答]**：\n{peer_sagent_reply}\n\n💡 如需本人补充，可直接在下方聊天框输入 `@{sender_name} 回复内容`。",
                sid = short_id(&task_id)
            );
            insert_chat_message(&tx, &target_id, "assistant", &notify_target)?;
            format!(
                "🤵 管家：已直接发给 **@{target_name}** 并创建协作任务「{summary_clean}」（#{sid}）。\n\n---\n\n🤖 **[@{target_name} 的管家 → 你的管家]**：\n{peer_sagent_reply}",
                sid = short_id(&task_id)
            )
        }
    } else if let Some(ref esc) = escalated_reason {
        let notify_target = format!(
            "🚨 **[需你本人接手 · {}]** 来自 **@{sender_name}**：\n> {clean_body}\n\n🤵 **[你的管家已挡回越权承诺并通知你]**：\n{peer_sagent_reply}\n\n👉 请直接在下方聊天框输入 `@{sender_name} 你的回复` 由你本人拍板回复。",
            esc.reason
        );
        insert_chat_message(&tx, &target_id, "assistant", &notify_target)?;
        notify_im_card(
            h,
            &target_id,
            "takeover",
            &format!("🚨 需你本人接手 · 来自 @{sender_name}"),
            &format!("**消息**：「{clean_body}」\n**拦截原因**：{}", esc.reason),
        );
        format!(
            "🤵 管家：已把消息转达给 **@{target_name}** 的管家。\n\n---\n\n🚨 **[@{target_name} 的管家 · 已转交本人接手]**：\n{peer_sagent_reply}"
        )
    } else {
        let notify_target = format!(
            "🤖 **[管家已代为响应 · 来自 @{sender_name} 的消息]**：\n> {clean_body}\n\n🤵 **[你的管家已代答]**：\n{peer_sagent_reply}\n\n💡 如需本人补充，可直接在下方聊天框输入 `@{sender_name} 回复内容`。"
        );
        insert_chat_message(&tx, &target_id, "assistant", &notify_target)?;
        format!(
            "🤵 管家：已直接把消息发给 **@{target_name}** 的管家。\n\n---\n\n🤖 **[@{target_name} 的管家 → 你的管家]**：\n{peer_sagent_reply}"
        )
    };

    let msg_id = insert_chat_message(&tx, owner, "user", &input.message)?;
    insert_chat_message(&tx, owner, "assistant", &reply)?;
    if let Some(key) = input.request_id.as_deref() {
        tx.execute(
            "UPDATE chat_turns SET message_id=?3 WHERE owner=?1 AND request_id=?2 AND status='pending'",
            params![owner, key, msg_id],
        )?;
    }
    let mut response = json!({
        "reply": reply,
        "backend": h.runtime.label(),
        "source": "sagent",
        "proposal": Value::Null,
        "runtime_profile": runtime_profile,
        "task_context": task_context.cloned().unwrap_or(Value::Null),
        "repository_context": Value::Null,
    });
    crate::chat_turns::complete(&tx, owner, input.request_id.as_deref(), &mut response)?;
    tx.commit()?;
    Ok(Some(Json(response)))
}

pub(crate) fn build_im_card_payload(
    provider: &str,
    event_kind: &str,
    title: &str,
    markdown: &str,
) -> Value {
    let header_color = match event_kind {
        "takeover" => "red",
        "delivery" => "green",
        _ => "blue",
    };
    match provider {
        "wecom" => json!({
            "msgtype": "markdown",
            "markdown": {
                "content": format!("### {title}\n{markdown}\n> 来自 **Agenct 7×24h 管家**")
            }
        }),
        "dingtalk" => json!({
            "msgtype": "markdown",
            "markdown": {
                "title": title,
                "text": format!("### {title}\n\n{markdown}\n\n---\n*来自 Agenct 7×24h 管家*")
            }
        }),
        "generic" => json!({
            "event": event_kind,
            "title": title,
            "markdown": markdown,
            "source": "agenct-sagent",
            "timestamp": nodes::now_unix(),
        }),
        _ => json!({
            "msg_type": "interactive",
            "card": {
                "config": { "wide_screen_mode": true },
                "header": {
                    "template": header_color,
                    "title": { "tag": "plain_text", "content": title }
                },
                "elements": [
                    {
                        "tag": "div",
                        "text": { "tag": "lark_md", "content": markdown }
                    },
                    {
                        "tag": "note",
                        "elements": [
                            { "tag": "plain_text", "content": "Agenct 7×24h 个人超级管家实时推送" }
                        ]
                    }
                ]
            }
        }),
    }
}

pub(crate) fn notify_im_card(h: &Hub, owner: &str, event_kind: &str, title: &str, markdown: &str) {
    let cfg = h
        .automation
        .lock()
        .ok()
        .and_then(|g| g.im_configs.get(owner).cloned());
    let Some(cfg) = cfg.filter(|c| !c.webhook_url.trim().is_empty()) else {
        return;
    };
    let enabled = match event_kind {
        "takeover" => cfg.notify_takeover,
        "delegation" => cfg.notify_delegation,
        "delivery" => cfg.notify_delivery,
        _ => true,
    };
    if !enabled {
        return;
    }
    let payload = build_im_card_payload(&cfg.provider, event_kind, title, markdown);
    let client = h.client.clone();
    let url = cfg.webhook_url;
    tokio::spawn(async move {
        let _ = client.post(&url).json(&payload).send().await;
    });
}

fn validate_im_webhook_url(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.len() > 512 {
        return None;
    }
    let url = reqwest::Url::parse(trimmed).ok()?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || trimmed.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return None;
    }
    Some(trimmed.to_owned())
}

fn format_im_config_json(cfg: &ImWebhookConfig) -> Value {
    let configured = !cfg.webhook_url.trim().is_empty();
    json!({
        "configured": configured,
        "provider": if cfg.provider.is_empty() { "feishu" } else { &cfg.provider },
        "webhook_url_preview": mask_gitlab_token(&cfg.webhook_url),
        "notify_takeover": cfg.notify_takeover,
        "notify_delegation": cfg.notify_delegation,
        "notify_delivery": cfg.notify_delivery,
        "updated_at": cfg.updated_at,
    })
}

pub(super) async fn get_im_config(State(h): State<Hub>, headers: HeaderMap) -> Result<Json<Value>> {
    let owner = h.identity(&headers)?;
    let cfg = h
        .automation
        .lock()
        .map_err(|_| invalid("automation unavailable"))?
        .im_configs
        .get(&owner)
        .cloned()
        .unwrap_or_default();
    Ok(Json(format_im_config_json(&cfg)))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SaveImConfigInput {
    #[serde(default = "default_im_provider")]
    provider: String,
    #[serde(default)]
    webhook_url: Option<String>,
    #[serde(default)]
    clear_webhook: bool,
    #[serde(default = "default_true")]
    notify_takeover: bool,
    #[serde(default = "default_true")]
    notify_delegation: bool,
    #[serde(default = "default_true")]
    notify_delivery: bool,
}

pub(super) async fn save_im_config(
    State(h): State<Hub>,
    headers: HeaderMap,
    Json(input): Json<SaveImConfigInput>,
) -> Result<Json<Value>> {
    let owner = h.identity(&headers)?;
    let provider = input.provider.trim().to_ascii_lowercase();
    if !matches!(
        provider.as_str(),
        "feishu" | "wecom" | "dingtalk" | "generic"
    ) {
        return Err(invalid(
            "provider must be feishu, wecom, dingtalk, or generic",
        ));
    }
    let next_url = match input.webhook_url.as_deref().map(str::trim) {
        Some(raw) if !raw.is_empty() => Some(
            validate_im_webhook_url(raw)
                .ok_or_else(|| invalid("Webhook 地址须为合法的 http(s):// URL"))?,
        ),
        _ => None,
    };
    let updated = with_store_mut(&h, |store| {
        let entry = store.im_configs.entry(owner.clone()).or_default();
        entry.provider = provider;
        if input.clear_webhook {
            entry.webhook_url.clear();
        } else if let Some(u) = next_url {
            entry.webhook_url = u;
        }
        entry.notify_takeover = input.notify_takeover;
        entry.notify_delegation = input.notify_delegation;
        entry.notify_delivery = input.notify_delivery;
        entry.updated_at = nodes::now_unix();
        entry.clone()
    })?;
    Ok(Json(format_im_config_json(&updated)))
}

pub(super) async fn test_im_webhook(
    State(h): State<Hub>,
    headers: HeaderMap,
) -> Result<Json<Value>> {
    let owner = h.identity(&headers)?;
    let cfg = h
        .automation
        .lock()
        .map_err(|_| invalid("automation unavailable"))?
        .im_configs
        .get(&owner)
        .cloned()
        .unwrap_or_default();
    if cfg.webhook_url.trim().is_empty() {
        return Err(invalid("请先保存飞书 / 企微 / 钉钉 Webhook 地址"));
    }
    let payload = build_im_card_payload(
        &cfg.provider,
        "takeover",
        "🔔 Agenct 7×24h 管家通知卡片测试",
        &format!(
            "**账号**：`{owner}`\n**状态**：实时通知通道已连通！当有【🚨 需本人接手】、【🤝 新任务委派】或【✅ 待验收交付】时，你的管家将第一时间推送卡片提醒。"
        ),
    );
    let resp = h
        .client
        .post(&cfg.webhook_url)
        .json(&payload)
        .send()
        .await
        .map_err(|_| {
            Error(
                axum::http::StatusCode::BAD_GATEWAY,
                "无法连接 Webhook 目标地址",
            )
        })?;
    if !resp.status().is_success() {
        return Err(Error(
            axum::http::StatusCode::BAD_GATEWAY,
            "Webhook 目标返回非 2xx 状态码，请核对机器人地址",
        ));
    }
    Ok(Json(json!({ "ok": true, "provider": cfg.provider })))
}

fn extract_im_inbound_text(payload: &Value) -> Option<String> {
    if let Some(s) = payload["text"].as_str() {
        return Some(s.trim().to_owned());
    }
    if let Some(s) = payload["text"]["content"].as_str() {
        return Some(s.trim().to_owned());
    }
    if let Some(s) = payload["content"].as_str() {
        return Some(s.trim().to_owned());
    }
    if let Some(raw_content) = payload["event"]["message"]["content"].as_str() {
        if let Ok(parsed) = serde_json::from_str::<Value>(raw_content) {
            if let Some(t) = parsed["text"].as_str() {
                return Some(t.trim().to_owned());
            }
        }
        return Some(raw_content.trim().to_owned());
    }
    None
}

pub(super) async fn im_webhook(
    State(h): State<Hub>,
    headers: HeaderMap,
    Json(payload): Json<Value>,
) -> Result<Json<Value>> {
    // 1. Feishu / Lark event subscription URL verification handshake
    if let Some(challenge) = payload.get("challenge").and_then(Value::as_str) {
        return Ok(Json(json!({ "challenge": challenge })));
    }
    let owner = h.identity(&headers)?;
    let raw_text =
        extract_im_inbound_text(&payload).ok_or_else(|| invalid("missing IM message text"))?;
    let clean_text = raw_text
        .trim_start_matches(|c: char| c.is_whitespace())
        .trim();
    if clean_text.is_empty() || clean_text.len() > 4000 {
        return Err(invalid("IM message text must be 1..4000 bytes"));
    }

    let auto_selected: Option<crate::task_context::Selection> = {
        let db = h.db()?;
        db.query_row(
            "SELECT id, revision FROM tasks
             WHERE requester=?1 AND owner!=?1 AND status='ready_for_acceptance'
             ORDER BY rowid DESC LIMIT 1",
            [&owner],
            |r| {
                Ok(crate::task_context::Selection {
                    task_id: r.get(0)?,
                    expected_revision: r.get(1)?,
                })
            },
        )
        .optional()?
    };
    let runtime_profile = {
        let db = h.db()?;
        crate::profiles::route(&db, &owner, None, None)?
    };
    let input = crate::messages::ChatInput {
        message: clean_text.to_owned(),
        agent_id: None,
        expected_profile_revision: None,
        request_id: None,
        selected_task: auto_selected,
        selected_project: None,
    };
    if let Some(Json(resp)) = answer_chat(&h, &owner, &input, &runtime_profile, None).await? {
        return Ok(Json(json!({
            "ok": true,
            "owner": owner,
            "handled_by": "sagent",
            "reply": resp["reply"],
        })));
    }
    if let Some(Json(resp)) =
        crate::nodes::answer_chat(&h, &owner, &input, &runtime_profile, None).await?
    {
        return Ok(Json(json!({
            "ok": true,
            "owner": owner,
            "handled_by": "pc-node",
            "reply": resp["reply"],
        })));
    }
    Ok(Json(json!({
        "ok": true,
        "owner": owner,
        "handled_by": "queued",
        "reply": format!("🤵 管家已收到你的 IM 指令：「{clean_text}」"),
    })))
}

pub(super) async fn pulse(State(h): State<Hub>, headers: HeaderMap) -> Result<Json<Value>> {
    let caller = h.identity(&headers)?;
    let db = h.db()?;
    let mut data = build_system_pulse(&h, &db, &caller)?;
    data["summary_markdown"] = Value::String(format_system_pulse(&data));
    Ok(Json(data))
}

pub(super) async fn get_automation(
    State(h): State<Hub>,
    headers: HeaderMap,
) -> Result<Json<Value>> {
    let owner = h.identity(&headers)?;
    let guard = h
        .automation
        .lock()
        .map_err(|_| invalid("automation unavailable"))?;
    let rules: Vec<_> = guard
        .rules
        .iter()
        .filter(|r| r.owner == owner)
        .cloned()
        .collect();
    let crons: Vec<_> = guard
        .crons
        .iter()
        .filter(|c| c.owner == owner)
        .cloned()
        .collect();
    let muse = guard
        .muse_profiles
        .get(&owner)
        .cloned()
        .unwrap_or_default();
    let topic_outcomes = guard.topic_outcomes.clone();
    let mut audit_logs: Vec<_> = guard
        .audit_logs
        .iter()
        .filter(|a| a.owner == owner)
        .cloned()
        .collect();
    audit_logs.reverse();
    audit_logs.truncate(30);
    let ledgers: Vec<_> = guard
        .ledgers
        .iter()
        .filter(|l| l.owner == owner)
        .cloned()
        .collect();
    Ok(Json(json!({
        "rules": rules,
        "crons": crons,
        "muse": muse,
        "topic_outcomes": topic_outcomes,
        "audit_logs": audit_logs,
        "ledgers": ledgers,
    })))
}

pub(crate) fn spawn_muse_post_completion_distillation(
    h: Hub,
    owner: String,
    task_id: String,
    instruction: String,
    final_content: String,
) {
    let crate::Runtime::Dsh { url, token } = h.runtime.clone() else {
        return;
    };
    tokio::spawn(async move {
        let Ok(runtime_profile) = ({
            let db = match h.db() {
                Ok(d) => d,
                Err(_) => return,
            };
            crate::profiles::route(&db, &owner, None, None)
        }) else {
            return;
        };
        let instr_short: String = nodes::strip_attachment_markers(&instruction)
            .chars()
            .take(300)
            .collect();
        let res_short: String = nodes::strip_attachment_markers(&final_content)
            .chars()
            .take(1000)
            .collect();
        let prompt = format!(
            "[MUSE_AGENT_TURN]\n\
             你是用户的 Muse 智能体归档与记忆提炼引擎。刚才一项任务已执行完毕，请根据【任务指令】与【完成结果】提炼三个字段：\n\
             1. `title`：动宾结构的干净中文短标题（6~14字，如「重排任务流程，简化作答路径」「修复移动端顶部气泡布局」「排查 Atlas 远端提交状态」，不要加任何 emoji 或标点前缀）；\n\
             2. `outcome_summary`：一句话（14~44字）高度概括实际完成了什么与验证结论（直接写成果，如「已将话题栏移为独立 flex 区域并通过 390×844 验收」）；\n\
             3. `memory_fact`：如果本次任务反映出值得长期写入 `MEMORY.md` 的项目架构、机器环境或用户偏好新事实，提炼为一条简短事实（不超过 60 字），否则设为 `null`。\n\n\
             【任务指令】\n{instr_short}\n\n\
             【完成结果】\n{res_short}\n\n\
             请保持内部思考极度简短，立即只输出一行严格合法的 JSON（不要代码块）：\n\
             `{{\"title\":\"动宾短标题\",\"outcome_summary\":\"一句话成果摘要\",\"memory_fact\":null}}`"
        );
        let dsh = crate::runtime::DshRuntime {
            client: &h.client,
            url: &url,
            token: &token,
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
            return;
        };
        let raw = ans.reply.trim();
        let (Some(s), Some(e)) = (raw.find('{'), raw.rfind('}')) else {
            return;
        };
        let Ok(val) = serde_json::from_str::<Value>(&raw[s..=e]) else {
            return;
        };
        let title = val["title"]
            .as_str()
            .map(str::trim)
            .filter(|t| !t.is_empty() && *t != "null")
            .map(|t| {
                let clean: String = t
                    .trim_start_matches(['💬', '🖥', '📁', '🦊', '⏰', '\u{fe0f}', ' '])
                    .chars()
                    .take(22)
                    .collect();
                clean
            });
        let outcome = val["outcome_summary"]
            .as_str()
            .map(str::trim)
            .filter(|o| !o.is_empty() && *o != "null")
            .map(|o| nodes::truncate_bytes(o, 160));
        let mem_fact = val["memory_fact"]
            .as_str()
            .map(str::trim)
            .filter(|m| !m.is_empty() && *m != "null")
            .map(|m| nodes::truncate_bytes(m, 200));

        if let Some(ref t) = title {
            if let Ok(db) = h.db() {
                let stored_summary = format!("💬 {t}");
                let _ = db.execute(
                    "UPDATE tasks SET summary=?1 WHERE id=?2 AND requester=?3 AND owner=?3",
                    params![stored_summary, task_id, owner],
                );
            }
        }
        let _ = with_store_mut(&h, |store| {
            if let Some(ref o) = outcome {
                store.topic_outcomes.insert(task_id.clone(), o.clone());
            }
            if let Some(ref m) = mem_fact {
                let entry = store.muse_profiles.entry(owner.clone()).or_default();
                let bullet = if m.starts_with('-') {
                    m.clone()
                } else {
                    format!("- {m}")
                };
                if !entry.memory_md.contains(&bullet) {
                    entry.memory_md = nodes::truncate_bytes(
                        &format!("{}\n{bullet}", entry.memory_md.trim_end()),
                        12000,
                    );
                    entry.updated_at = nodes::now_unix();
                }
            }
        });
        record_muse_audit(
            &h,
            &owner,
            "task_distill",
            "completed",
            title.as_deref().unwrap_or("任务完结归档"),
            outcome.as_deref().unwrap_or("已提炼任务成果摘要并归档"),
        );
    });
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SaveMuseProfileInput {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    soul_md: Option<String>,
    #[serde(default)]
    memory_md: Option<String>,
}

pub(super) async fn save_muse_profile(
    State(h): State<Hub>,
    headers: HeaderMap,
    Json(input): Json<SaveMuseProfileInput>,
) -> Result<Json<MuseProfileConfig>> {
    let owner = h.identity(&headers)?;
    let updated = with_store_mut(&h, |store| {
        let entry = store.muse_profiles.entry(owner.clone()).or_default();
        if let Some(name) = input.name.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            entry.name = nodes::truncate_bytes(name, 48);
        }
        if let Some(soul) = input.soul_md.as_deref() {
            entry.soul_md = nodes::truncate_bytes(soul.trim(), 8000);
        }
        if let Some(mem) = input.memory_md.as_deref() {
            entry.memory_md = nodes::truncate_bytes(mem.trim(), 12000);
        }
        entry.updated_at = nodes::now_unix();
        entry.clone()
    })?;
    Ok(Json(updated))
}

pub(super) async fn remove_cron(
    State(h): State<Hub>,
    headers: HeaderMap,
    axum::extract::Path(cron_id): axum::extract::Path<String>,
) -> Result<Json<Value>> {
    let owner = h.identity(&headers)?;
    let removed = with_store_mut(&h, |store| {
        let before = store.crons.len();
        store
            .crons
            .retain(|c| !(c.owner == owner && c.id == cron_id));
        before != store.crons.len()
    })?;
    if !removed {
        return Err(denied());
    }
    Ok(Json(json!({ "ok": true, "removed": cron_id })))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CreateRuleInput {
    #[serde(default = "default_star")]
    keyword: String,
    #[serde(default)]
    target_node_id: String,
    #[serde(default)]
    require_clarification: Option<String>,
    #[serde(default)]
    require_human_approval: bool,
}

fn default_star() -> String {
    "*".to_owned()
}

pub(super) async fn create_rule(
    State(h): State<Hub>,
    headers: HeaderMap,
    Json(input): Json<CreateRuleInput>,
) -> Result<Json<DispatchRule>> {
    let owner = h.identity(&headers)?;
    let keyword = input.keyword.trim();
    if keyword.is_empty() || keyword.len() > 120 {
        return Err(invalid("invalid rule keyword"));
    }
    let rule = DispatchRule {
        id: format!("rule-{}", &Uuid::new_v4().simple().to_string()[..6]),
        owner,
        keyword: keyword.to_owned(),
        target_node_id: input.target_node_id.trim().to_owned(),
        require_clarification: input.require_clarification.map(|s| s.trim().to_owned()),
        require_human_approval: input.require_human_approval,
        created_at: nodes::now_unix(),
    };
    with_store_mut(&h, |store| store.rules.push(rule.clone()))?;
    Ok(Json(rule))
}

pub(super) async fn remove_rule(
    State(h): State<Hub>,
    headers: HeaderMap,
    axum::extract::Path(rule_id): axum::extract::Path<String>,
) -> Result<Json<Value>> {
    let owner = h.identity(&headers)?;
    let removed = with_store_mut(&h, |store| {
        let before = store.rules.len();
        store
            .rules
            .retain(|r| !(r.owner == owner && r.id == rule_id));
        before != store.rules.len()
    })?;
    if !removed {
        return Err(denied());
    }
    Ok(Json(json!({ "ok": true, "removed": rule_id })))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CreateCronInput {
    interval_minutes: u64,
    #[serde(default)]
    daily_time_cst: Option<String>,
    instruction: String,
    #[serde(default = "default_target")]
    target: String,
}

pub(super) async fn create_cron(
    State(h): State<Hub>,
    headers: HeaderMap,
    Json(input): Json<CreateCronInput>,
) -> Result<Json<CronJob>> {
    let owner = h.identity(&headers)?;
    if input.interval_minutes == 0 || input.interval_minutes > 10080 {
        return Err(invalid("interval_minutes must be 1..10080"));
    }
    let daily_time_cst = match input
        .daily_time_cst
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(raw) => {
            let (hh, mm) = parse_hhmm(raw)
                .ok_or_else(|| invalid("daily_time_cst must be HH:MM (00:00..23:59)"))?;
            Some(format!("{hh:02}:{mm:02}"))
        }
        None => None,
    };
    let instr = input.instruction.trim();
    if instr.is_empty() || instr.len() > 1000 {
        return Err(invalid("instruction must be 1..1000 bytes"));
    }
    let job = CronJob {
        id: format!("cron-{}", &Uuid::new_v4().simple().to_string()[..6]),
        owner,
        interval_minutes: input.interval_minutes,
        daily_time_cst,
        instruction: instr.to_owned(),
        target: input.target.trim().to_owned(),
        last_run_unix: None,
        enabled: true,
        created_at: nodes::now_unix(),
    };
    with_store_mut(&h, |store| store.crons.push(job.clone()))?;
    h.automation_notify.notify_one();
    Ok(Json(job))
}

pub(super) async fn tick_crons(State(h): State<Hub>, headers: HeaderMap) -> Result<Json<Value>> {
    let owner = h.identity(&headers)?;
    let ran = run_due_crons(&h, Some(&owner), None).await?;
    Ok(Json(json!({ "executed": ran })))
}

fn percent_encode_component(s: &str) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

fn normalize_gitlab_base_url(raw: &str) -> Option<String> {
    let trimmed = raw.trim().trim_end_matches('/');
    if trimmed.is_empty() || trimmed.len() > 256 {
        return None;
    }
    let url = reqwest::Url::parse(trimmed).ok()?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path() != "/"
        || trimmed
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || c == '\\')
    {
        return None;
    }
    Some(url.origin().ascii_serialization())
}

fn gitlab_base_url_from_web_url(web_url: &str) -> Option<String> {
    let trimmed = web_url.trim();
    let scheme_end = trimmed.find("://")? + 3;
    let origin = match trimmed[scheme_end..].find('/') {
        Some(idx) => &trimmed[..scheme_end + idx],
        None => trimmed,
    };
    normalize_gitlab_base_url(origin)
}

fn default_gitlab_base_url(db: &Connection, owner: &str) -> String {
    let from_binding: Option<String> = db
        .query_row(
            "SELECT issuer FROM identity_bindings WHERE owner=?1",
            [owner],
            |r| r.get(0),
        )
        .optional()
        .ok()
        .flatten();
    if let Some(url) = from_binding
        .as_deref()
        .and_then(gitlab_base_url_from_web_url)
    {
        return url;
    }
    if let Ok(issuer) = std::env::var("OIDC_ISSUER") {
        if let Some(url) = gitlab_base_url_from_web_url(&issuer) {
            return url;
        }
    }
    "https://git.example.com".to_owned()
}

fn default_gitlab_username(db: &Connection, owner: &str) -> String {
    db.query_row(
        "SELECT display_name FROM member_profiles WHERE owner=?1 AND status='active'",
        [owner],
        |r| r.get::<_, String>(0),
    )
    .optional()
    .ok()
    .flatten()
    .map(|s| s.trim().to_owned())
    .filter(|s| !s.is_empty() && !s.starts_with("gitlab-"))
    .unwrap_or_default()
}

fn mask_gitlab_token(token: &str) -> String {
    if token.is_empty() {
        return String::new();
    }
    let chars: Vec<char> = token.chars().collect();
    if chars.len() >= 12 {
        let prefix: String = chars[..6].iter().collect();
        let suffix: String = chars[chars.len() - 4..].iter().collect();
        format!("{prefix}***{suffix}")
    } else {
        "***".to_owned()
    }
}

fn format_gitlab_config_json(
    h: &Hub,
    headers: &HeaderMap,
    db: &Connection,
    owner: &str,
) -> Result<Value> {
    let saved = h
        .automation
        .lock()
        .map_err(|_| invalid("automation unavailable"))?
        .gitlab_configs
        .get(owner)
        .cloned();
    let cfg = saved.unwrap_or_else(|| GitLabUserConfig {
        auto_poll_todos: true,
        ..Default::default()
    });
    let base_url = if cfg.base_url.is_empty() {
        default_gitlab_base_url(db, owner)
    } else {
        cfg.base_url.clone()
    };
    let gitlab_username = if cfg.gitlab_username.is_empty() {
        default_gitlab_username(db, owner)
    } else {
        cfg.gitlab_username.clone()
    };
    let has_token = !cfg.token.is_empty();
    let webhook_url = format!("{}/webhooks/gitlab", nodes::hub_origin(headers));
    Ok(json!({
        "configured": has_token,
        "base_url": base_url,
        "gitlab_username": gitlab_username,
        "has_token": has_token,
        "token_preview": mask_gitlab_token(&cfg.token),
        "auto_poll_todos": cfg.auto_poll_todos,
        "webhook_url": webhook_url,
        "updated_at": cfg.updated_at,
    }))
}

#[derive(Deserialize)]
pub(super) struct SaveGitLabConfigInput {
    base_url: String,
    #[serde(default)]
    gitlab_username: String,
    #[serde(default)]
    token: Option<String>,
    #[serde(default)]
    clear_token: bool,
    #[serde(default = "default_true")]
    auto_poll_todos: bool,
}

pub(super) async fn get_gitlab_config(
    State(h): State<Hub>,
    headers: HeaderMap,
) -> Result<Json<Value>> {
    let owner = h.identity(&headers)?;
    let db = h.db()?;
    Ok(Json(format_gitlab_config_json(&h, &headers, &db, &owner)?))
}

pub(super) async fn save_gitlab_config(
    State(h): State<Hub>,
    headers: HeaderMap,
    Json(input): Json<SaveGitLabConfigInput>,
) -> Result<Json<Value>> {
    let owner = h.identity(&headers)?;
    let base_url = normalize_gitlab_base_url(&input.base_url)
        .ok_or_else(|| invalid("GitLab 地址须为合法的 http(s):// URL"))?;
    let gitlab_username = input
        .gitlab_username
        .trim()
        .trim_start_matches('@')
        .to_owned();
    if gitlab_username.len() > 64
        || gitlab_username
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || c == '@')
    {
        return Err(invalid("GitLab 用户名格式无效"));
    }
    let next_token = match input.token.as_deref().map(str::trim) {
        Some(raw) if !raw.is_empty() => {
            if raw.len() > 256 || raw.chars().any(|c| c.is_whitespace() || c.is_control()) {
                return Err(invalid("GitLab Personal Access Token 格式无效"));
            }
            Some(raw.to_owned())
        }
        _ => None,
    };

    with_store_mut(&h, |store| {
        let entry = store.gitlab_configs.entry(owner.clone()).or_default();
        // Credentials are bound to the configured origin, never carried to a new host.
        if normalize_gitlab_base_url(&entry.base_url).as_ref() != Some(&base_url) {
            entry.token.clear();
        }
        entry.base_url = base_url;
        entry.gitlab_username = gitlab_username;
        if input.clear_token {
            entry.token.clear();
        } else if let Some(tok) = next_token {
            entry.token = tok;
        }
        entry.auto_poll_todos = input.auto_poll_todos;
        entry.updated_at = nodes::now_unix();
    })?;
    h.automation_notify.notify_one();

    let db = h.db()?;
    Ok(Json(format_gitlab_config_json(&h, &headers, &db, &owner)?))
}

struct IncomingGitLabEvent<'a> {
    target_owner: &'a str,
    source_label: &'a str,
    project_path: &'a str,
    author: &'a str,
    body: &'a str,
    noteable_type: &'a str,
    issue_iid: Option<i64>,
    web_url: &'a str,
    base_url_hint: Option<&'a str>,
}

fn dispatch_gitlab_event(h: &Hub, ev: IncomingGitLabEvent<'_>) -> Result<Value> {
    let issue_ref = ev
        .issue_iid
        .map(|iid| format!("{} #{iid}", ev.project_path))
        .unwrap_or_else(|| ev.project_path.to_owned());
    let incoming = format!(
        "🦊 [{}] 来自 `{issue_ref}`（{}）：\n> {}{}",
        ev.source_label,
        ev.author,
        ev.body,
        if ev.web_url.is_empty() {
            String::new()
        } else {
            format!("\n\n来源链接：{}", ev.web_url)
        }
    );
    let topic_label = ev
        .issue_iid
        .map(|iid| format!("🦊 GitLab #{iid} · {}", ev.project_path))
        .unwrap_or_else(|| format!("🦊 GitLab · {}", ev.project_path));
    let picked_node = {
        let db = h.db()?;
        nodes::select_node_with_rule(
            h,
            &db,
            ev.target_owner,
            &format!("{}\n{}", ev.project_path, ev.body),
        )?
    };
    let mut db = h.db()?;
    let tx = db.transaction()?;
    let permissions = crate::automation_policy::read(&tx, ev.target_owner)?;
    let picked_node = picked_node.filter(|_| permissions.gitlab_execute);
    insert_chat_message(&tx, ev.target_owner, "user", &incoming)?;

    let mut registered_task_id: Option<String> = None;
    let status_label = if let Some((node_id, node_name, online, _)) = picked_node {
        let full_q = format!(
            "[{topic_label}] 来自 GitLab {} 的 @ 请求：{}",
            ev.author, ev.body
        );
        let (task_id, reply) = nodes::dispatch_chat_task(
            &tx,
            ev.target_owner,
            &node_id,
            &node_name,
            online,
            &full_q,
            None,
        )?;
        tx.execute(
            "UPDATE node_assignments SET authorization='gitlab' WHERE task_id=?1",
            [&task_id],
        )?;
        let reply_msg = format!("🦊 管家已自动响应来自 `{issue_ref}` 的 @ 请求：\n{reply}");
        insert_chat_message(&tx, ev.target_owner, "assistant", &reply_msg)?;
        registered_task_id = Some(task_id.clone());
        json!({
            "user": ev.target_owner,
            "mode": "pc-node",
            "node_id": node_id,
            "task_id": task_id,
            "online": online,
        })
    } else {
        let reply_msg = format!(
            "🦊 管家已收到来自 `{issue_ref}`（{}）的 @ 请求：\n「{}」\n\n尚未派发执行。请检查自动化权限及节点，或在对话中明确指派。",
            ev.author, ev.body
        );
        insert_chat_message(&tx, ev.target_owner, "assistant", &reply_msg)?;
        json!({
            "user": ev.target_owner,
            "mode": "cloud-sagent",
        })
    };
    tx.commit()?;
    drop(db);

    if let (Some(task_id), Some(iid)) = (registered_task_id, ev.issue_iid.filter(|&n| n > 0)) {
        if !ev.project_path.is_empty() && ev.project_path != "repo" {
            let configured = h
                .automation
                .lock()
                .ok()
                .and_then(|g| g.gitlab_configs.get(ev.target_owner).cloned());
            let Some(base_url) = configured
                .as_ref()
                .and_then(|c| normalize_gitlab_base_url(&c.base_url))
            else {
                return Ok(status_label);
            };
            // Incoming metadata can restrict a reply, but never choose its destination.
            if ev
                .base_url_hint
                .and_then(normalize_gitlab_base_url)
                .as_ref()
                != Some(&base_url)
            {
                return Ok(status_label);
            }
            let _ = with_store_mut(h, |store| {
                if store.gitlab_targets.len() >= 500 {
                    if let Some(oldest_key) = store.gitlab_targets.keys().next().cloned() {
                        store.gitlab_targets.remove(&oldest_key);
                    }
                }
                store.gitlab_targets.insert(
                    task_id,
                    GitLabReplyTarget {
                        owner: ev.target_owner.to_owned(),
                        base_url,
                        project_path: ev.project_path.to_owned(),
                        noteable_type: ev.noteable_type.to_owned(),
                        noteable_iid: iid,
                    },
                );
            });
        }
    }
    Ok(status_label)
}

async fn sync_gitlab_todos_for_owner(h: &Hub, owner: &str) -> Result<(usize, usize)> {
    if !h.member_active(owner)? {
        return Ok((0, 0));
    }
    let cfg = h
        .automation
        .lock()
        .map_err(|_| invalid("automation unavailable"))?
        .gitlab_configs
        .get(owner)
        .cloned();
    let Some(cfg) = cfg.filter(|c| !c.token.trim().is_empty()) else {
        return Err(invalid("请先配置 GitLab Personal Access Token"));
    };
    let base_url = normalize_gitlab_base_url(&cfg.base_url).unwrap_or_else(|| {
        h.db()
            .map(|db| default_gitlab_base_url(&db, owner))
            .unwrap_or_else(|_| "https://git.example.com".to_owned())
    });
    let endpoint = format!("{base_url}/api/v4/todos?state=pending&per_page=10");
    let resp = h
        .client
        .get(&endpoint)
        .header("PRIVATE-TOKEN", &cfg.token)
        .send()
        .await
        .map_err(|_| {
            Error(
                axum::http::StatusCode::BAD_GATEWAY,
                "无法连接 GitLab 服务器",
            )
        })?;
    if !resp.status().is_success() {
        return Err(Error(
            axum::http::StatusCode::BAD_GATEWAY,
            "GitLab API 返回错误，请确认地址和 Personal Access Token 有效",
        ));
    }
    let todos: Vec<Value> = resp.json().await.map_err(|_| {
        Error(
            axum::http::StatusCode::BAD_GATEWAY,
            "无法解析 GitLab 待办响应",
        )
    })?;
    let fetched = todos.len();
    if fetched == 0 {
        return Ok((0, 0));
    }

    let fresh_items: Vec<Value> = with_store_mut(h, |store| {
        let entry = store.gitlab_configs.entry(owner.to_owned()).or_default();
        let mut fresh = Vec::new();
        for item in todos {
            let Some(todo_id) = item["id"].as_i64() else {
                continue;
            };
            if entry.seen_todo_ids.contains(&todo_id) {
                continue;
            }
            entry.seen_todo_ids.push(todo_id);
            fresh.push(item);
        }
        if entry.seen_todo_ids.len() > 200 {
            let excess = entry.seen_todo_ids.len() - 200;
            entry.seen_todo_ids.drain(..excess);
        }
        fresh
    })?;

    let mut dispatched = 0usize;
    for item in fresh_items {
        let project_path = item["project"]["path_with_namespace"]
            .as_str()
            .unwrap_or("repo");
        let hook_base = item["project"]["web_url"]
            .as_str()
            .and_then(gitlab_base_url_from_web_url);
        let author = item["author"]["username"]
            .as_str()
            .or_else(|| item["author"]["name"].as_str())
            .unwrap_or("gitlab");
        let target_type = item["target_type"].as_str().unwrap_or("Issue");
        let noteable_type = if target_type == "MergeRequest" {
            "MergeRequest"
        } else {
            "Issue"
        };
        let issue_iid = item["target"]["iid"].as_i64();
        let web_url = item["target_url"]
            .as_str()
            .or_else(|| item["target"]["web_url"].as_str())
            .unwrap_or("");
        let raw_body = item["body"].as_str().unwrap_or("").trim();
        let title = item["target"]["title"].as_str().unwrap_or("").trim();
        let body = if raw_body.is_empty() { title } else { raw_body };
        if body.is_empty() || body.contains("[Agenct 管家") {
            continue;
        }
        let body_trimmed: String = body.chars().take(4000).collect();
        let _ = dispatch_gitlab_event(
            h,
            IncomingGitLabEvent {
                target_owner: owner,
                source_label: "GitLab 待办同步",
                project_path,
                author,
                body: &body_trimmed,
                noteable_type,
                issue_iid,
                web_url,
                base_url_hint: hook_base.as_deref().or(Some(&base_url)),
            },
        )?;
        dispatched += 1;
    }

    Ok((fetched, dispatched))
}

pub(super) async fn sync_gitlab_now(
    State(h): State<Hub>,
    headers: HeaderMap,
) -> Result<Json<Value>> {
    let owner = h.identity(&headers)?;
    let (fetched, dispatched) = sync_gitlab_todos_for_owner(&h, &owner).await?;
    Ok(Json(json!({
        "ok": true,
        "fetched": fetched,
        "dispatched": dispatched,
    })))
}

async fn sync_all_gitlab_todos(h: &Hub) {
    let owners: Vec<String> = {
        let Ok(guard) = h.automation.lock() else {
            return;
        };
        guard
            .gitlab_configs
            .iter()
            .filter(|(_, cfg)| cfg.auto_poll_todos && !cfg.token.trim().is_empty())
            .map(|(owner, _)| owner.clone())
            .collect()
    };
    for owner in owners {
        let _ = sync_gitlab_todos_for_owner(h, &owner).await;
    }
}

pub(crate) fn notify_gitlab_task_completed(
    h: &Hub,
    task_id: &str,
    node_name: &str,
    final_content: &str,
) {
    let authorized_owner = h
        .automation
        .lock()
        .ok()
        .and_then(|g| g.gitlab_targets.get(task_id).map(|t| t.owner.clone()));
    let allowed = authorized_owner.as_deref().is_some_and(|owner| {
        h.member_active(owner).unwrap_or(false)
            && h.db()
                .ok()
                .and_then(|db| crate::automation_policy::read(&db, owner).ok())
                .is_some_and(|p| p.gitlab_reply)
    });
    if !allowed {
        return;
    }
    let target_and_token = {
        let Ok(mut guard) = h.automation.lock() else {
            return;
        };
        let Some(target) = guard.gitlab_targets.remove(task_id) else {
            return;
        };
        persist_automation(h, &guard);
        let token = guard
            .gitlab_configs
            .get(&target.owner)
            .filter(|c| reply_target_matches_config(&target, c))
            .map(|c| c.token.clone())
            .unwrap_or_default();
        (target, token)
    };
    let (target, token) = target_and_token;
    if token.is_empty() || target.base_url.is_empty() || target.noteable_iid <= 0 {
        return;
    }
    let hub = h.clone();
    let node = node_name.to_owned();
    let content = final_content.trim().to_owned();
    tokio::spawn(async move {
        if !hub.member_active(&target.owner).unwrap_or(false)
            || !hub
                .db()
                .ok()
                .and_then(|db| crate::automation_policy::read(&db, &target.owner).ok())
                .is_some_and(|p| p.gitlab_reply)
        {
            return;
        }
        let resource = if target.noteable_type == "MergeRequest" {
            "merge_requests"
        } else {
            "issues"
        };
        let encoded_project = percent_encode_component(&target.project_path);
        let url = format!(
            "{}/api/v4/projects/{}/{}/{}/notes",
            target.base_url.trim_end_matches('/'),
            encoded_project,
            resource,
            target.noteable_iid
        );
        let comment = format!("🤵 **[Agenct 管家 · 节点「{node}」自动处理回执]**\n\n{content}");
        let res = hub
            .client
            .post(&url)
            .header("PRIVATE-TOKEN", &token)
            .json(&json!({ "body": comment }))
            .send()
            .await;
        let feedback = match res {
            Ok(resp) if resp.status().is_success() => format!(
                "🦊 已自动将处理结果回写至 GitLab `{} #{}` 评论区。",
                target.project_path, target.noteable_iid
            ),
            _ => format!(
                "⚠️ 任务已完成，但回写 GitLab `{} #{}` 评论失败（请确认 Personal Access Token 具备 `api` 写权限）。",
                target.project_path, target.noteable_iid
            ),
        };
        if let Ok(mut db) = hub.db() {
            if let Ok(tx) = db.transaction() {
                let _ = insert_chat_message(&tx, &target.owner, "assistant", &feedback);
                let _ = tx.commit();
            }
        }
    });
}

fn resolve_at_mentions(h: &Hub, db: &Connection, text: &str) -> Vec<String> {
    let mut alias_map: HashMap<String, String> = HashMap::new();
    if let Ok(mut stmt) =
        db.prepare("SELECT owner, display_name FROM member_profiles WHERE status='active'")
    {
        if let Ok(rows) =
            stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        {
            for row in rows.flatten() {
                let (owner, name) = row;
                let trimmed = name.trim();
                if !trimmed.is_empty() && h.directory.ids().any(|id| id == owner) {
                    alias_map.insert(trimmed.to_ascii_lowercase(), owner);
                }
            }
        }
    }
    if let Ok(guard) = h.automation.lock() {
        for (owner, cfg) in &guard.gitlab_configs {
            let uname = cfg.gitlab_username.trim();
            if !uname.is_empty() && h.directory.ids().any(|id| id == owner.as_str()) {
                alias_map.insert(uname.to_ascii_lowercase(), owner.clone());
            }
        }
    }
    for id in h.directory.ids() {
        alias_map.insert(id.to_ascii_lowercase(), id.to_owned());
    }

    let mut users = Vec::new();
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '@' && (i == 0 || !chars[i - 1].is_ascii_alphanumeric()) {
            let mut j = i + 1;
            let mut tok = String::new();
            while j < chars.len()
                && (chars[j].is_ascii_alphanumeric() || matches!(chars[j], '-' | '_' | '.'))
            {
                tok.push(chars[j]);
                j += 1;
            }
            let normalized = tok.trim_end_matches('.');
            if !normalized.is_empty() {
                if let Some(canonical) = alias_map.get(&normalized.to_ascii_lowercase()) {
                    if !users.contains(canonical) {
                        users.push(canonical.clone());
                    }
                }
            }
            i = j;
        } else {
            i += 1;
        }
    }
    users
}

fn reply_target_matches_config(target: &GitLabReplyTarget, cfg: &GitLabUserConfig) -> bool {
    normalize_gitlab_base_url(&cfg.base_url).is_some_and(|base| base == target.base_url)
}

// A shared operator secret authorizes workspace ingress; user/node credentials
// authorize only their own mentions. Missing credentials always fail closed.
fn webhook_scope(h: &Hub, headers: &HeaderMap, secret: Option<&str>) -> Result<Option<String>> {
    let provided = headers
        .get("x-gitlab-token")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if let Some(expected) = secret.filter(|s| !s.trim().is_empty()) {
        if provided == expected {
            return Ok(None);
        }
    }
    if !provided.is_empty() {
        let owner: Option<String> = h
            .db()?
            .query_row(
                "SELECT owner FROM user_nodes WHERE token_hash=?1",
                [nodes::hash_token(provided)],
                |r| r.get(0),
            )
            .optional()?;
        let owner = owner.ok_or_else(denied)?;
        if !h.directory.contains(&owner) || !h.member_active(&owner)? {
            return Err(denied());
        }
        return Ok(Some(owner));
    }
    Ok(Some(h.identity(headers)?))
}

/// Module D: GitLab Issue / Comment / IM Webhook ingress.
/// Parses `@user` mentions, alerts that user's `sagent`, and routes work either
/// to an online local PC node or cloud `sagent`.
pub(super) async fn gitlab_webhook(
    State(h): State<Hub>,
    headers: HeaderMap,
    Json(payload): Json<Value>,
) -> Result<Json<Value>> {
    let secret = std::env::var("AGENCT_GITLAB_WEBHOOK_SECRET").ok();
    let scope = webhook_scope(&h, &headers, secret.as_deref())?;

    let project = payload["project"]["path_with_namespace"]
        .as_str()
        .or_else(|| payload["project"].as_str())
        .unwrap_or("repo");
    let author = payload["user"]["username"]
        .as_str()
        .or_else(|| payload["author"].as_str())
        .unwrap_or("gitlab");
    let is_mr = payload["object_kind"].as_str() == Some("merge_request")
        || payload["object_attributes"]["noteable_type"].as_str() == Some("MergeRequest")
        || !payload["merge_request"].is_null();
    let noteable_type = if is_mr { "MergeRequest" } else { "Issue" };
    let issue_iid = payload["object_attributes"]["iid"]
        .as_i64()
        .or_else(|| payload["issue"]["iid"].as_i64())
        .or_else(|| payload["merge_request"]["iid"].as_i64())
        .or_else(|| payload["issue_iid"].as_i64());
    let body = payload["object_attributes"]["note"]
        .as_str()
        .or_else(|| payload["object_attributes"]["description"].as_str())
        .or_else(|| payload["body"].as_str())
        .or_else(|| payload["text"].as_str())
        .or_else(|| payload["object_attributes"]["title"].as_str())
        .unwrap_or("")
        .trim();
    if body.is_empty() || body.len() > 8000 {
        return Err(invalid("webhook body is empty or too large"));
    }
    if body.contains("[Agenct 管家") {
        return Ok(Json(json!({
            "ok": true,
            "triggered": [],
            "ignored": "self-reply",
        })));
    }
    let url = payload["object_attributes"]["url"]
        .as_str()
        .or_else(|| payload["url"].as_str())
        .unwrap_or("");
    let hook_base = payload["project"]["web_url"]
        .as_str()
        .and_then(gitlab_base_url_from_web_url)
        .or_else(|| gitlab_base_url_from_web_url(url));

    let mentioned = {
        let db = h.db()?;
        resolve_at_mentions(&h, &db, body)
    };
    let mut triggered = Vec::new();
    for target in mentioned {
        if scope.as_ref().is_some_and(|owner| owner != &target) {
            continue;
        }
        if !h.member_active(&target)? {
            continue;
        }
        let status_label = dispatch_gitlab_event(
            &h,
            IncomingGitLabEvent {
                target_owner: &target,
                source_label: "GitLab 协同消息",
                project_path: project,
                author,
                body,
                noteable_type,
                issue_iid,
                web_url: url,
                base_url_hint: hook_base.as_deref(),
            },
        )?;
        triggered.push(status_label);
    }

    Ok(Json(json!({
        "ok": true,
        "triggered": triggered,
    })))
}

impl Hub {
    pub async fn run_sagent_scheduler(&self) {
        let mut interval = tokio::time::interval(Duration::from_secs(30));
        loop {
            tokio::select! {
                _ = interval.tick() => {}
                _ = self.automation_notify.notified() => {}
            }
            let _ = run_due_crons(self, None, None).await;
            sync_all_gitlab_todos(self).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{app, Config, Hub, Runtime};
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use base64::{engine::general_purpose::STANDARD, Engine};
    use serde_json::{json, Value};
    use sha2::{Digest, Sha256};
    use tower::ServiceExt;

    #[tokio::test]
    async fn webhook_requires_credentials_and_node_credentials_are_owner_scoped() {
        let h = test_hub();
        let r = app(h.clone());
        let body = json!({"body":"@alice @bob inspect this"});
        let (status, _) = call(&r, "", "POST", "/webhooks/gitlab", Some(body.clone())).await;
        assert!(!status.is_success());
        assert_eq!(
            h.db()
                .unwrap()
                .query_row("SELECT count(*) FROM messages", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            0
        );
        let (_, node) = call(
            &r,
            &token("bob"),
            "POST",
            "/api/nodes",
            Some(json!({"id":"private-node","name":"Private"})),
        )
        .await;
        let request = Request::builder()
            .method("POST")
            .uri("/webhooks/gitlab")
            .header("content-type", "application/json")
            .header("x-gitlab-token", node["token"].as_str().unwrap())
            .body(Body::from(body.to_string()))
            .unwrap();
        let response = r.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), 100000)
            .await
            .unwrap();
        let reply: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(reply["triggered"].as_array().unwrap().len(), 1);
        assert_eq!(reply["triggered"][0]["user"], "bob");
        let mut headers = axum::http::HeaderMap::new();
        assert!(super::webhook_scope(&h, &headers, Some("")).is_err());
        headers.insert("x-gitlab-token", "wrong".parse().unwrap());
        assert!(super::webhook_scope(&h, &headers, Some("trusted-secret")).is_err());
        headers.insert("x-gitlab-token", "trusted-secret".parse().unwrap());
        assert_eq!(
            super::webhook_scope(&h, &headers, Some("trusted-secret")).unwrap(),
            None
        );
        headers.insert(
            "x-gitlab-token",
            node["token"].as_str().unwrap().parse().unwrap(),
        );
        h.db()
            .unwrap()
            .execute(
                "INSERT INTO member_profiles(owner,display_name,status) VALUES ('bob','Bob','disabled')",
                [],
            )
            .unwrap();
        assert!(super::webhook_scope(&h, &headers, None).is_err());
    }

    #[tokio::test]
    async fn pulse_excludes_other_users_private_tasks_nodes_and_automation() {
        let h = test_hub();
        let r = app(h.clone());
        call(
            &r,
            &token("bob"),
            "POST",
            "/api/nodes",
            Some(json!({"id":"secret-node","name":"Secret"})),
        )
        .await;
        {
            let db = h.db().unwrap();
            db.execute("INSERT INTO tasks(id,requester,owner,summary,status,revision) VALUES ('hidden','bob','bob','private-topic','working',0)", []).unwrap();
            db.execute("INSERT INTO tasks(id,requester,owner,summary,status,revision) VALUES ('shared','alice','bob','shared-topic','working',0)", []).unwrap();
        }
        call(
            &r,
            &token("bob"),
            "POST",
            "/api/messages",
            Some(json!({"message":"定时任务：每 15 分钟 总结系统运转情况"})),
        )
        .await;
        let (status, pulse) = call(&r, &token("alice"), "GET", "/api/pulse", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(pulse["nodes_total"], 0);
        assert_eq!(pulse["crons_active"], 0);
        assert_eq!(pulse["muse_topics_count"], 0);
        assert_eq!(pulse["active_collaborations"].as_array().unwrap().len(), 1);
        assert_eq!(pulse["active_collaborations"][0]["summary"], "shared-topic");
        assert!(!pulse.to_string().contains("private-topic"));
        assert!(!pulse.to_string().contains("secret-node"));
    }

    #[tokio::test]
    async fn untrusted_reply_origin_and_changed_config_cannot_reuse_pat() {
        let h = test_hub();
        enable_automation(&h, "bob");
        let r = app(h.clone());
        let (_, node) = call(
            &r,
            &token("bob"),
            "POST",
            "/api/nodes",
            Some(json!({"id":"pc","name":"PC"})),
        )
        .await;
        call(
            &r,
            node["token"].as_str().unwrap(),
            "GET",
            "/node/v1/work",
            None,
        )
        .await;
        call(&r, &token("bob"), "POST", "/api/gitlab/config", Some(json!({"base_url":"https://trusted.test", "token":"private-pat", "auto_poll_todos":false}))).await;
        let (status, hook) = call(&r, &token("bob"), "POST", "/webhooks/gitlab", Some(json!({
            "project":{"path_with_namespace":"team/app", "web_url":"https://attacker.test/team/app"},
            "body":"@bob inspect app", "issue_iid":1
        }))).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(hook["triggered"].as_array().unwrap().len(), 1);
        assert!(h.automation.lock().unwrap().gitlab_targets.is_empty());
        let target = super::GitLabReplyTarget {
            owner: "bob".into(),
            base_url: "https://attacker.test".into(),
            project_path: "team/app".into(),
            noteable_type: "Issue".into(),
            noteable_iid: 1,
        };
        assert!(!super::reply_target_matches_config(
            &target,
            &h.automation.lock().unwrap().gitlab_configs["bob"]
        ));
        call(
            &r,
            &token("bob"),
            "POST",
            "/api/gitlab/config",
            Some(json!({"base_url":"https://new.test", "auto_poll_todos":false})),
        )
        .await;
        assert!(h.automation.lock().unwrap().gitlab_configs["bob"]
            .token
            .is_empty());
    }

    #[test]
    fn gitlab_origins_reject_credentials_paths_and_ambiguous_urls() {
        for bad in [
            "https://user:secret@host",
            "https://host/path",
            "https://host?query",
            "https://host#fragment",
            "https://host\\evil",
            "file:///tmp/foo",
            "https://",
        ] {
            assert!(super::normalize_gitlab_base_url(bad).is_none(), "{bad}");
        }
        assert_eq!(
            super::normalize_gitlab_base_url("https://HOST:443/"),
            Some("https://host".into())
        );
    }

    fn enable_automation(h: &Hub, owner: &str) {
        h.db().unwrap().execute("INSERT INTO automation_permissions(owner,revision,auto_accept,auto_deliver,gitlab_execute,gitlab_reply) VALUES (?1,1,1,1,1,1)",[owner]).unwrap();
    }

    fn hash(token: &str) -> String {
        format!("{:x}", Sha256::digest(token.as_bytes()))
    }

    fn token(user: &str) -> String {
        format!("dev-token-{user}-0123456789abcdef0123456789abcdef")
    }

    fn test_hub() -> Hub {
        Hub::open(Config {
            bind: "127.0.0.1:0".parse().unwrap(),
            database: ":memory:".into(),
            users: vec![
                crate::UserConfig {
                    id: "alice".into(),
                    token_sha256: hash(&token("alice")),
                },
                crate::UserConfig {
                    id: "bob".into(),
                    token_sha256: hash(&token("bob")),
                },
            ],
            runtime: Runtime::Mock,
        })
        .unwrap()
    }

    async fn call(
        router: &axum::Router,
        bearer: &str,
        method: &str,
        uri: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let mut builder = Request::builder()
            .method(method)
            .header("x-agenct-node-protocol", "2")
            .uri(uri);
        if !bearer.is_empty() {
            builder = builder.header("authorization", format!("Bearer {bearer}"));
        }
        let req = match body {
            Some(v) => builder
                .header("content-type", "application/json")
                .body(Body::from(v.to_string()))
                .unwrap(),
            None => builder.body(Body::empty()).unwrap(),
        };
        let resp = router.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let val = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, val)
    }

    #[tokio::test]
    async fn module_b_cross_user_sagent_negotiation_clarification_evidence_pack_and_mobile_accept()
    {
        let h = test_hub();
        enable_automation(&h, "bob");
        let r = app(h.clone());

        // Bob registers a PC node and brings it online.
        let (s, bob_node) = call(
            &r,
            &token("bob"),
            "POST",
            "/api/nodes",
            Some(json!({ "id": "bob-pc", "name": "Bob主力机" })),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        let node_token = bob_node["token"].as_str().unwrap().to_owned();
        let (s, _) = call(&r, &node_token, "GET", "/node/v1/work", None).await;
        assert_eq!(s, StatusCode::OK);

        // Bob configures a clarification rule: anyone delegating must provide 截图 / 复现步骤.
        let (s, rule_chat) = call(
            &r,
            &token("bob"),
            "POST",
            "/api/messages",
            Some(json!({ "message": "规则：别人委派任务必须提供 复现步骤与截图" })),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert!(rule_chat["reply"]
            .as_str()
            .unwrap()
            .contains("复现步骤与截图"));

        // Alice creates and confirms a delegation to Bob WITHOUT details/screenshots -> UI bug!
        let (s, prop) = call(
            &r,
            &token("alice"),
            "POST",
            "/api/proposals",
            Some(json!({ "owner": "bob", "summary": "修复登录页面按钮错位bug" })),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        let prop_id = prop["id"].as_str().unwrap().to_owned();

        let (s, confirmed) = call(
            &r,
            &token("alice"),
            "POST",
            &format!("/api/proposals/{prop_id}/actions"),
            Some(json!({ "action": "confirm", "expected_revision": 0 })),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        let task_id = confirmed["task_id"].as_str().unwrap().to_owned();

        // Task stays awaiting_owner until Alice clarifies; Alice sees clarification request from Bob's sagent!
        let (s, alice_msgs) = call(&r, &token("alice"), "GET", "/api/messages", None).await;
        assert_eq!(s, StatusCode::OK);
        assert!(alice_msgs.as_array().unwrap().iter().any(|m| m["content"]
            .as_str()
            .unwrap()
            .contains("需要补充核对")
            || m["content"].as_str().unwrap().contains("补充核对")));

        // Bob's node has no job yet.
        let (s, w0) = call(&r, &node_token, "GET", "/node/v1/work", None).await;
        assert_eq!(s, StatusCode::OK);
        assert!(w0["job"].is_null());

        // Alice replies via chat with "补充：..." + uploaded bug screenshot!
        let png_bytes: &[u8] = &[
            0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00,
            0x00, 0x1f, 0x15, 0xc4, 0x89, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x78,
            0x9c, 0x63, 0x00, 0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0d, 0x0a, 0x2d, 0xb4, 0x00,
            0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
        ];
        let png_b64 = STANDARD.encode(png_bytes);
        let (s, att) = call(
            &r,
            &token("alice"),
            "POST",
            "/api/attachments",
            Some(json!({ "name": "bug.png", "mime_type": "image/png", "data_base64": png_b64 })),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        let att_id = att["id"].as_str().unwrap().to_owned();

        let (s, clarify) = call(
            &r,
            &token("alice"),
            "POST",
            "/api/messages",
            Some(json!({
                "message": format!("补充：在移动端375px下登录按钮溢出\n[图片附件: bug.png (/api/attachments/{att_id})]")
            })),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert!(clarify["reply"].as_str().unwrap().contains("自动接手"));

        // Now Bob's node receives the job AND Alice's bug screenshot attachment!
        let (s, w1) = call(&r, &node_token, "GET", "/node/v1/work", None).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(w1["job"]["task_id"], task_id);
        assert_eq!(w1["job"]["attachments"].as_array().unwrap().len(), 1);
        assert_eq!(w1["job"]["attachments"][0]["id"], att_id);

        // Case 1: Node submits a draft WITHOUT screenshot/verification for a UI task -> sagent holds it for Bob's review.
        let (s, _) = call(
            &r,
            &node_token,
            "POST",
            &format!("/node/v1/tasks/{task_id}/drafts"),
            Some(json!({
                "lease_token": w1["job"]["lease_token"], "request_id": "try-1",
                "expected_revision": w1["job"]["task_revision"],
                "status": "completed",
                "content": "改了一行CSS"
            })),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        let (s, bob_msgs) = call(&r, &token("bob"), "GET", "/api/messages", None).await;
        assert_eq!(s, StatusCode::OK);
        assert!(bob_msgs.as_array().unwrap().iter().any(|m| m["content"]
            .as_str()
            .unwrap()
            .contains("未附带界面截图或验证说明")));

        // Case 2: Re-assign & submit WITH screenshot evidence attachment -> sagent auto-delivers to Alice!
        let (s, _) = call(
            &r,
            &token("bob"),
            "POST",
            &format!("/api/tasks/{task_id}/node-assignment"),
            Some(json!({ "node_id": "bob-pc", "expected_revision": 1, "instruction": "补截图并验证" })),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        let (_, retried_work) = call(&r, &node_token, "GET", "/node/v1/work", None).await;
        let (s, _) = call(
            &r,
            &node_token,
            "POST",
            &format!("/node/v1/tasks/{task_id}/drafts"),
            Some(json!({
                "lease_token": retried_work["job"]["lease_token"], "request_id": "try-2",
                "expected_revision": 1,
                "status": "completed",
                "content": "### 🔍 验收证据包\n已修复移动端按钮样式，Playwright 实测通过。",
                "attachments": [{
                    "name": "fixed.png",
                    "mime_type": "image/png",
                    "data_base64": png_b64
                }]
            })),
        )
        .await;
        assert_eq!(s, StatusCode::OK);

        // Alice receives the evidence pack with fixed.png right in her chat stream AND can fetch fixed.png!
        let (s, alice_msgs2) = call(&r, &token("alice"), "GET", "/api/messages", None).await;
        assert_eq!(s, StatusCode::OK);
        let delivery_msg = alice_msgs2
            .as_array()
            .unwrap()
            .iter()
            .rfind(|m| m["content"].as_str().unwrap().contains("验收证据包"))
            .unwrap()["content"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(delivery_msg.contains("/api/attachments/att_"));
        let fixed_id = delivery_msg
            .split("/api/attachments/")
            .nth(1)
            .unwrap()
            .split(')')
            .next()
            .unwrap()
            .to_owned();
        let (s, fetched_img) = call(
            &r,
            &token("alice"),
            "GET",
            &format!("/api/attachments/{fixed_id}"),
            None,
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(fetched_img["name"], "fixed.png");

        let (s, _) = call(
            &r,
            &token("alice"),
            "POST",
            "/api/messages",
            Some(json!({"message":"可以"})),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        let (status, revision): (String, i64) = h
            .db()
            .unwrap()
            .query_row(
                "SELECT status,revision FROM tasks WHERE id=?1",
                [&task_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(status, "ready_for_acceptance");
        let (s, accept_chat) = call(
            &r,
            &token("alice"),
            "POST",
            "/api/messages",
            Some(json!({ "message": "/approve", "selected_task":{"task_id":task_id,"expected_revision":revision} })),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert!(accept_chat["reply"].as_str().unwrap().contains("验收通过"));

        let (s, final_task) = call(
            &r,
            &token("alice"),
            "GET",
            &format!("/api/tasks/{task_id}"),
            None,
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(final_task["task"]["status"], "completed");
    }

    #[tokio::test]
    async fn modules_a_c_d_pulse_cloud_cron_and_gitlab_webhook_work_end_to_end() {
        let h = test_hub();
        enable_automation(&h, "bob");
        let r = app(h);

        // Module C: Alice (with ZERO PC nodes!) creates a 7x24h cron job and triggers it -> runs in cloud!
        let (s, cron_chat) = call(
            &r,
            &token("alice"),
            "POST",
            "/api/messages",
            Some(json!({ "message": "定时任务：每 15 分钟 总结系统运转情况" })),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert!(cron_chat["reply"]
            .as_str()
            .unwrap()
            .contains("7×24h 定时任务"));

        let (s, tick) = call(&r, &token("alice"), "POST", "/api/automation/tick", None).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(tick["executed"], 1);

        let (s, alice_msgs) = call(&r, &token("alice"), "GET", "/api/messages", None).await;
        assert_eq!(s, StatusCode::OK);
        assert!(alice_msgs
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["content"].as_str().unwrap().contains("云端定时任务报告")));

        // Module D: GitLab issue comment @alice and @bob routes to each sagent (Bob has online node, Alice uses cloud).
        let (s, bob_node) = call(
            &r,
            &token("bob"),
            "POST",
            "/api/nodes",
            Some(json!({ "id": "bob-pc", "name": "Bob主力机" })),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        let node_token = bob_node["token"].as_str().unwrap().to_owned();
        let _ = call(&r, &node_token, "GET", "/node/v1/work", None).await;

        let (s, hook_res) = call(
            &r,
            &token("bob"),
            "POST",
            "/webhooks/gitlab",
            Some(json!({
                "project": { "path_with_namespace": "team/atlas" },
                "user": { "username": "pm" },
                "object_attributes": {
                    "iid": 42,
                    "note": "@bob 请检查 atlas 构建日志，@alice 请知悉跟进",
                    "url": "https://git.local/team/atlas/-/issues/42#note_1"
                }
            })),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        let triggered = hook_res["triggered"].as_array().unwrap();
        assert_eq!(triggered.len(), 1);
        assert!(triggered
            .iter()
            .any(|t| t["user"] == "bob" && t["mode"] == "pc-node"));
        assert!(!triggered.iter().any(|t| t["user"] == "alice"));

        // Module A: Asking "现在系统里大家运转情况怎么样" in chat or GET /api/pulse returns live summary.
        let (s, pulse_chat) = call(
            &r,
            &token("alice"),
            "POST",
            "/api/messages",
            Some(json!({ "message": "现在系统里大家运转情况怎么样" })),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        let reply = pulse_chat["reply"].as_str().unwrap();
        assert!(reply.contains("我的任务与节点态势"));
        assert!(!reply.contains("bob-pc"));

        let (s, pulse_api) = call(&r, &token("alice"), "GET", "/api/pulse", None).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(pulse_api["nodes_online"], 0);
    }

    #[tokio::test]
    async fn gitlab_config_masking_display_name_resolution_and_reply_target_work() {
        assert_eq!(
            super::percent_encode_component("team/sub/agenct"),
            "team%2Fsub%2Fagenct"
        );
        let h = test_hub();
        enable_automation(&h, "bob");
        {
            let db = h.db().unwrap();
            db.execute(
                "INSERT OR REPLACE INTO member_profiles(owner,display_name,status) VALUES ('bob','ZhouYaoxu','active')",
                [],
            )
            .unwrap();
        }
        let r = app(h.clone());

        let (s, cfg0) = call(&r, &token("bob"), "GET", "/api/gitlab/config", None).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(cfg0["configured"], false);
        assert_eq!(cfg0["gitlab_username"], "ZhouYaoxu");
        assert!(cfg0["webhook_url"]
            .as_str()
            .unwrap()
            .ends_with("/webhooks/gitlab"));

        let (s, cfg1) = call(
            &r,
            &token("bob"),
            "POST",
            "/api/gitlab/config",
            Some(json!({
                "base_url": "https://git.example.com/",
                "gitlab_username": "@zhou.gl",
                "token": "glpat-1234567890abcd",
                "auto_poll_todos": true
            })),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(cfg1["configured"], true);
        assert_eq!(cfg1["has_token"], true);
        assert_eq!(cfg1["gitlab_username"], "zhou.gl");
        assert_eq!(cfg1["base_url"], "https://git.example.com");
        let preview = cfg1["token_preview"].as_str().unwrap();
        assert!(preview.starts_with("glpat-"));
        assert!(preview.ends_with("abcd"));
        assert!(!preview.contains("1234567890"));

        // Register online node for Bob and send a note webhook mentioning both @zhouyaoxu (display_name) and @zhou.gl
        let (s, bob_node) = call(
            &r,
            &token("bob"),
            "POST",
            "/api/nodes",
            Some(json!({ "id": "bob-gl", "name": "Bob开发机" })),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        let _ = call(
            &r,
            bob_node["token"].as_str().unwrap(),
            "GET",
            "/node/v1/work",
            None,
        )
        .await;

        let (s, hook) = call(
            &r,
            &token("bob"),
            "POST",
            "/webhooks/gitlab",
            Some(json!({
                "object_kind": "note",
                "project": {
                    "path_with_namespace": "team/agenct",
                    "web_url": "https://git.example.com/team/agenct"
                },
                "user": { "username": "luojie" },
                "merge_request": { "iid": 88 },
                "object_attributes": {
                    "noteable_type": "MergeRequest",
                    "note": "@zhouyaoxu 请审查这个 MR",
                    "url": "https://git.example.com/team/agenct/-/merge_requests/88#note_9"
                }
            })),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        let triggered = hook["triggered"].as_array().unwrap();
        assert_eq!(triggered.len(), 1);
        assert_eq!(triggered[0]["user"], "bob");
        assert_eq!(triggered[0]["mode"], "pc-node");
        let tid = triggered[0]["task_id"].as_str().unwrap();

        {
            let guard = h.automation.lock().unwrap();
            let target = guard.gitlab_targets.get(tid).expect("reply target stored");
            assert_eq!(target.owner, "bob");
            assert_eq!(target.project_path, "team/agenct");
            assert_eq!(target.noteable_type, "MergeRequest");
            assert_eq!(target.noteable_iid, 88);
            assert_eq!(target.base_url, "https://git.example.com");
        }

        // Self-reply guard prevents loops
        let (s, loop_res) = call(
            &r,
            &token("bob"),
            "POST",
            "/webhooks/gitlab",
            Some(json!({
                "project": { "path_with_namespace": "team/agenct" },
                "object_attributes": {
                    "iid": 88,
                    "note": "🤵 **[Agenct 管家 · 节点「Bob开发机」自动处理回执]** @zhouyaoxu 已完成"
                }
            })),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(loop_res["triggered"].as_array().unwrap().len(), 0);
        assert_eq!(loop_res["ignored"], "self-reply");

        // Updating without token keeps existing token; clear_token removes it.
        let (s, cfg2) = call(
            &r,
            &token("bob"),
            "POST",
            "/api/gitlab/config",
            Some(json!({
                "base_url": "https://git.example.com",
                "gitlab_username": "zhou.gl",
                "auto_poll_todos": false
            })),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(cfg2["has_token"], true);
        assert_eq!(cfg2["auto_poll_todos"], false);

        let (s, cfg3) = call(
            &r,
            &token("bob"),
            "POST",
            "/api/gitlab/config",
            Some(json!({
                "base_url": "https://git.example.com",
                "gitlab_username": "zhou.gl",
                "clear_token": true,
                "auto_poll_todos": false
            })),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(cfg3["has_token"], false);
        assert_eq!(cfg3["token_preview"], "");
    }

    #[tokio::test]
    async fn natural_language_daily_gitlab_cron_parses_beijing_time_and_skips_pc_dispatch() {
        // Verify Beijing time daily wall-clock check logic:
        // 2026-11-17 21:59 CST (13:59 UTC) -> 1794837540
        // Let's compute exact unix for 2026-11-17 13:59:00Z via cst_day_and_hhmm round-trip:
        let base_day_utc = 20_774_i64 * 86400; // 2026-11-17 00:00:00Z
        let cst_2159 = base_day_utc + 13 * 3600 + 59 * 60; // 21:59 CST
        let cst_2200 = base_day_utc + 14 * 3600; // 22:00 CST
        let cst_next_2201 = base_day_utc + 86400 + 14 * 3600 + 60; // next day 22:01 CST
        let (_, ch, cm) = super::cst_day_and_hhmm(cst_2159);
        assert_eq!((ch, cm), (21, 59));

        let mut job = super::CronJob {
            id: "cron-test".into(),
            owner: "alice".into(),
            interval_minutes: 1440,
            daily_time_cst: Some("22:00".into()),
            instruction: "汇总一下gitlab上的提交总结给我".into(),
            target: "cloud".into(),
            last_run_unix: None,
            enabled: true,
            created_at: cst_2159,
        };
        assert!(!super::is_cron_due(&job, cst_2159));
        assert!(super::is_cron_due(&job, cst_2200));
        job.last_run_unix = Some(cst_2200);
        assert!(!super::is_cron_due(&job, cst_2200 + 600));
        assert!(super::is_cron_due(&job, cst_next_2201));

        // Ensure question-like queries do not falsely register crons
        assert!(
            super::try_parse_natural_cron("alice", "为什么每天的定时任务没执行？")
                .unwrap()
                .is_none()
        );

        // End-to-end chat turn with online PC node still handles natural daily cron in sagent
        let h = test_hub();
        let r = app(h.clone());
        let (s, node) = call(
            &r,
            &token("alice"),
            "POST",
            "/api/nodes",
            Some(json!({ "id": "cc-pro-team", "name": "cc-pro-team" })),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        let _ = call(
            &r,
            node["token"].as_str().unwrap(),
            "GET",
            "/node/v1/work",
            None,
        )
        .await;

        let (s, chat) = call(
            &r,
            &token("alice"),
            "POST",
            "/api/messages",
            Some(json!({ "message": "每天晚上10点汇总一下gitlab上的提交总结给我" })),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        let reply = chat["reply"].as_str().unwrap();
        assert!(reply.contains("北京时间 22:00"));
        assert!(reply.contains("已创建 7×24h 定时任务"));

        // Verify cron stored with target="cloud" and zero PC tasks dispatched
        let (s, auto_state) = call(&r, &token("alice"), "GET", "/api/automation", None).await;
        assert_eq!(s, StatusCode::OK);
        let crons = auto_state["crons"].as_array().unwrap();
        assert_eq!(crons.len(), 1);
        assert_eq!(crons[0]["daily_time_cst"], "22:00");
        assert_eq!(crons[0]["target"], "cloud");

        let (s, tasks) = call(&r, &token("alice"), "GET", "/api/tasks", None).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(tasks.as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn chat_at_mention_colleague_sends_direct_message_and_delegates_task_by_display_name() {
        let h = test_hub();
        enable_automation(&h, "bob");
        {
            let db = h.db().unwrap();
            db.execute(
                "INSERT OR REPLACE INTO member_profiles(owner,display_name,status) VALUES ('alice','zhouyaoxu','active')",
                [],
            )
            .unwrap();
            db.execute(
                "INSERT OR REPLACE INTO member_profiles(owner,display_name,status) VALUES ('bob','luojie','active')",
                [],
            )
            .unwrap();
        }
        let r = app(h.clone());

        // 1. Pure chat message via `给@luojie ...` delivers directly to Bob's main chat & contact_messages
        let (s, chat1) = call(
            &r,
            &token("alice"),
            "POST",
            "/api/messages",
            Some(json!({ "message": "给@luojie 说一声今晚十点发版" })),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        let reply1 = chat1["reply"].as_str().unwrap();
        assert!(reply1.contains("[@luojie 的管家 → 你的管家]"));
        assert!(reply1.contains("我是 luojie 的 7×24h 云端管家"));

        let (s, bob_msgs) = call(&r, &token("bob"), "GET", "/api/messages", None).await;
        assert_eq!(s, StatusCode::OK);
        assert!(bob_msgs.as_array().unwrap().iter().any(|m| {
            let c = m["content"].as_str().unwrap_or("");
            c.contains("来自 @zhouyaoxu 的消息") && c.contains("今晚十点发版")
        }));

        // 2. Actionable task via `@luojie 帮我排查登录接口报错` creates task and auto-dispatches to Bob's online node
        let (s, bob_node) = call(
            &r,
            &token("bob"),
            "POST",
            "/api/nodes",
            Some(json!({ "id": "bob-pc", "name": "Bob主力机" })),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        let _ = call(
            &r,
            bob_node["token"].as_str().unwrap(),
            "GET",
            "/node/v1/work",
            None,
        )
        .await;

        let (s, chat2) = call(
            &r,
            &token("alice"),
            "POST",
            "/api/messages",
            Some(json!({ "message": "@luojie 帮我排查一下登录接口报错" })),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert!(chat2["reply"].as_str().unwrap().contains("自动接手任务"));

        let (s, bob_tasks) = call(&r, &token("bob"), "GET", "/api/tasks", None).await;
        assert_eq!(s, StatusCode::OK);
        let tasks = bob_tasks.as_array().unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0]["requester"], "alice");
        assert_eq!(tasks[0]["owner"], "bob");
        assert_eq!(tasks[0]["status"], "working");
    }

    #[tokio::test]
    async fn three_layer_human_takeover_rules_and_circuit_breaker_work() {
        let h = test_hub();
        enable_automation(&h, "bob");
        {
            let db = h.db().unwrap();
            db.execute(
                "INSERT OR REPLACE INTO member_profiles(owner,display_name,status) VALUES ('alice','zhouyaoxu','active')",
                [],
            )
            .unwrap();
            db.execute(
                "INSERT OR REPLACE INTO member_profiles(owner,display_name,status) VALUES ('bob','luojie','active')",
                [],
            )
            .unwrap();
        }
        let r = app(h.clone());

        // Bring Bob's PC node online
        let (s, bob_node) = call(
            &r,
            &token("bob"),
            "POST",
            "/api/nodes",
            Some(json!({ "id": "bob-pc", "name": "Bob主力机" })),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        let _ = call(
            &r,
            bob_node["token"].as_str().unwrap(),
            "GET",
            "/node/v1/work",
            None,
        )
        .await;

        // Layer 1A: Explicit human request ("找你本人") triggers human takeover
        let (s, chat_human) = call(
            &r,
            &token("alice"),
            "POST",
            "/api/messages",
            Some(json!({ "message": "@luojie 找你本人聊一下" })),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        let reply_human = chat_human["reply"].as_str().unwrap();
        assert!(reply_human.contains("已转交本人接手"));
        assert!(reply_human.contains("对方显式要求你本人直接沟通"));

        // Layer 1B: High-risk / schedule commitment ("几点上线") triggers human takeover
        let (s, chat_sched) = call(
            &r,
            &token("alice"),
            "POST",
            "/api/messages",
            Some(json!({ "message": "@luojie 今晚几点上线？" })),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        let reply_sched = chat_sched["reply"].as_str().unwrap();
        assert!(reply_sched.contains("已转交本人接手"));
        assert!(reply_sched.contains("排期承诺或高危生产决策"));

        // Layer 2: Bob configures personal multi-keyword human-takeover rule via chat + API removal
        let (s, add_rule_res) = call(
            &r,
            &token("bob"),
            "POST",
            "/api/messages",
            Some(json!({ "message": "规则：包含 生产数据库/核心账务 的任务需要真人接手" })),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert!(add_rule_res["reply"]
            .as_str()
            .unwrap()
            .contains("已保存个人分发规则"));

        // Alice delegates a task matching Bob's custom rule ("核心账务") -> held in awaiting_owner for Bob's human approval, NOT auto-dispatched to node
        let (s, chat_custom) = call(
            &r,
            &token("alice"),
            "POST",
            "/api/messages",
            Some(json!({ "message": "@luojie 帮我排查核心账务对账异常" })),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        let reply_custom = chat_custom["reply"].as_str().unwrap();
        assert!(reply_custom.contains("已转交本人接手"));
        assert!(reply_custom.contains("个人规则"));

        let (s, bob_tasks) = call(&r, &token("bob"), "GET", "/api/tasks", None).await;
        assert_eq!(s, StatusCode::OK);
        let tasks = bob_tasks.as_array().unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0]["status"], "awaiting_owner");

        // Verify rule list & remove_rule endpoint
        let (s, auto_state) = call(&r, &token("bob"), "GET", "/api/automation", None).await;
        assert_eq!(s, StatusCode::OK);
        let rule_id = auto_state["rules"][0]["id"].as_str().unwrap().to_owned();
        let (s, rm_res) = call(
            &r,
            &token("bob"),
            "POST",
            &format!("/api/automation/rules/{rule_id}/remove"),
            None,
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(rm_res["ok"], true);

        // Layer 1C: Repeated return circuit breaker (>= 2 returns pauses auto re-dispatch and escalates to human)
        let (s, normal_task) = call(
            &r,
            &token("alice"),
            "POST",
            "/api/messages",
            Some(json!({ "message": "@luojie 帮我修复导出按钮样式问题" })),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert!(normal_task["reply"]
            .as_str()
            .unwrap()
            .contains("自动接手任务"));

        // Simulate node delivery so task is ready_for_acceptance before first return
        {
            let db = h.db().unwrap();
            db.execute(
                "UPDATE tasks SET status='ready_for_acceptance' WHERE summary LIKE '%导出按钮%'",
                [],
            )
            .unwrap();
        }

        let (task_id, revision): (String, i64) = h
            .db()
            .unwrap()
            .query_row(
                "SELECT id,revision FROM tasks WHERE summary LIKE '%导出按钮%'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        // First return -> re-dispatches automatically
        let (s, ret1) = call(
            &r,
            &token("alice"),
            "POST",
            "/api/messages",
            Some(json!({ "message": "/return 移动端圆角还是不对", "selected_task":{"task_id":task_id,"expected_revision":revision} })),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert!(ret1["reply"]
            .as_str()
            .unwrap()
            .contains("按修改意见继续处理"));

        // Simulate second delivery so task is ready_for_acceptance again
        {
            let db = h.db().unwrap();
            db.execute(
                "UPDATE tasks SET status='ready_for_acceptance' WHERE summary LIKE '%导出按钮%'",
                [],
            )
            .unwrap();
        }

        // Second return -> circuit breaker trips! Escalates to human takeover
        let (s, ret2) = call(
            &r,
            &token("alice"),
            "POST",
            "/api/messages",
            Some(json!({ "message": "/return 暗色模式下边框依然发白", "selected_task":{"task_id":task_id,"expected_revision":revision+1} })),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        let reply_ret2 = ret2["reply"].as_str().unwrap();
        assert!(reply_ret2.contains("触发系统底线熔断"));
        assert!(reply_ret2.contains("需本人接手"));

        let (s, bob_msgs) = call(&r, &token("bob"), "GET", "/api/messages", None).await;
        assert_eq!(s, StatusCode::OK);
        assert!(bob_msgs
            .as_array()
            .unwrap()
            .iter()
            .any(|m| { m["content"].as_str().unwrap_or("").contains("连续退回熔断") }));
    }

    #[tokio::test]
    async fn semantic_rules_live_node_progress_and_im_webhook_cards_work() {
        let h = test_hub();
        enable_automation(&h, "bob");
        let r = app(h.clone());

        // 1. Configure Feishu IM notification card & verify card payload builders
        let (s, im0) = call(&r, &token("bob"), "GET", "/api/im/config", None).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(im0["configured"], false);

        let (s, im1) = call(
            &r,
            &token("bob"),
            "POST",
            "/api/im/config",
            Some(json!({
                "provider": "feishu",
                "webhook_url": "https://open.feishu.cn/open-apis/bot/v2/hook/secret123456",
                "notify_takeover": true,
                "notify_delegation": true,
                "notify_delivery": true
            })),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(im1["configured"], true);
        assert_eq!(im1["provider"], "feishu");

        let feishu_card = super::build_im_card_payload("feishu", "takeover", "标题", "正文");
        assert_eq!(feishu_card["msg_type"], "interactive");
        assert_eq!(feishu_card["card"]["header"]["template"], "red");
        let wecom_card = super::build_im_card_payload("wecom", "delivery", "标题", "正文");
        assert_eq!(wecom_card["msgtype"], "markdown");

        // 2. Natural-language standing rule without "规则：" prefix + semantic synonym matching ("数据库" <-> "postgres")
        let (s, nat_rule) = call(
            &r,
            &token("bob"),
            "POST",
            "/api/messages",
            Some(json!({ "message": "以后凡是包含 数据库 的任务都需要真人接手" })),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert!(nat_rule["reply"]
            .as_str()
            .unwrap()
            .contains("已保存个人分发规则"));
        assert!(super::evaluate_human_takeover_policy(&h, "bob", "迁移 postgres 表结构").is_some());
        assert!(super::evaluate_human_takeover_policy(&h, "bob", "把这个改动合并进主干").is_some());

        // 3. Semantic requirement satisfaction ("第一步点击登录报错 500" satisfies "复现步骤")
        assert!(super::requirement_clause_satisfied(
            "复现步骤",
            "第一步打开首页，点击登录按钮报错 500",
            false
        ));
        assert!(super::requirement_clause_satisfied(
            "报错日志",
            "panic: connection refused (500)",
            false
        ));

        // 4. Live PC node progress via lease renewal appears on owner's GET /api/tasks
        let (s, bob_node) = call(
            &r,
            &token("bob"),
            "POST",
            "/api/nodes",
            Some(json!({ "id": "bob-pc", "name": "Bob主力机" })),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        let node_token = bob_node["token"].as_str().unwrap().to_owned();
        {
            let db = h.db().unwrap();
            db.execute(
                "INSERT INTO tasks(id,requester,owner,summary,status,revision) VALUES ('live-t','alice','bob','测试进度','working',1)",
                [],
            )
            .unwrap();
            db.execute(
                "INSERT INTO node_assignments(task_id,owner,node_id,task_revision,instruction,status,updated_at)
                 VALUES ('live-t','bob','bob-pc',1,'run test','queued',1)",
                [],
            )
            .unwrap();
        }
        let (s, work) = call(&r, &node_token, "GET", "/node/v1/work", None).await;
        assert_eq!(s, StatusCode::OK);
        let lease_token = work["job"]["lease_token"].as_str().unwrap().to_owned();
        let (s, _) = call(
            &r,
            &node_token,
            "POST",
            "/node/v1/tasks/live-t/lease",
            Some(json!({
                "lease_token": lease_token,
                "expected_revision": 1,
                "stage": "本机 codex 正在运行测试…",
                "log_tail": "test 12/20 passed",
                "elapsed_secs": 9
            })),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        let (s, bob_tasks) = call(&r, &token("bob"), "GET", "/api/tasks", None).await;
        assert_eq!(s, StatusCode::OK);
        let t_item = bob_tasks
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["id"] == "live-t")
            .unwrap();
        assert_eq!(
            t_item["my_node_assignment"]["progress"]["stage"],
            "本机 codex 正在运行测试…"
        );
        assert_eq!(
            t_item["my_node_assignment"]["progress"]["log_tail"],
            "test 12/20 passed"
        );
        assert_eq!(t_item["my_node_assignment"]["progress"]["elapsed_secs"], 9);

        // 5. SQLite automation_state convergence: state is persisted directly inside SQLite
        let raw_db_state: String = h
            .db()
            .unwrap()
            .query_row(
                "SELECT state_json FROM automation_state WHERE id = 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(raw_db_state.contains("feishu"));
        assert!(raw_db_state.contains("数据库"));

        // 7. OpenAI-compatible response parser & Enterprise IM inbound webhook (/webhooks/im)
        let parsed_json = crate::runtime::parse_openai_content_to_adapter_reply(
            r#"{"reply":"来自通用大模型的回答","proposal":null}"#,
        );
        assert_eq!(parsed_json.reply, "来自通用大模型的回答");
        assert_eq!(parsed_json.backend, "dsh");
        let parsed_plain =
            crate::runtime::parse_openai_content_to_adapter_reply("直接返回的纯文本回答");
        assert_eq!(parsed_plain.reply, "直接返回的纯文本回答");

        // Feishu challenge handshake
        let (s, challenge_resp) = call(
            &r,
            &token("alice"),
            "POST",
            "/webhooks/im",
            Some(json!({ "challenge": "feishu-verify-123" })),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(challenge_resp["challenge"], "feishu-verify-123");

        // Mobile IM direct approval via /webhooks/im
        {
            let db = h.db().unwrap();
            db.execute(
                "INSERT INTO tasks(id,requester,owner,summary,status,result,revision)
                 VALUES ('im-approve-task','alice','bob','移动端IM直接验收','ready_for_acceptance','已修好',2)",
                [],
            )
            .unwrap();
            db.execute(
                "INSERT INTO node_assignments(task_id,owner,node_id,task_revision,instruction,status,updated_at)
                 VALUES ('im-approve-task','bob','bob-pc',2,'fix','completed',1)",
                [],
            )
            .unwrap();
        }
        let (s, im_in) = call(
            &r,
            &token("alice"),
            "POST",
            "/webhooks/im",
            Some(json!({
                "event": {
                    "message": {
                        "content": "{\"text\":\"验收通过\"}"
                    }
                }
            })),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(im_in["handled_by"], "sagent");
        assert!(im_in["reply"].as_str().unwrap().contains("确认验收通过"));
    }

    #[tokio::test]
    async fn muse_unified_agent_loop_handles_compound_cron_and_node_turns() {
        let adapter = axum::Router::new().route(
            "/v1/turn",
            axum::routing::post(|axum::Json(body): axum::Json<Value>| async move {
                let msg = body["message"].as_str().unwrap_or("");
                let reply = if msg.starts_with("[MUSE_AGENT_TURN]") {
                    assert!(msg.contains("cron-7d2629"));
                    assert!(msg.contains("node_id=`homelab`"));
                    if msg.contains("【用户最新消息】\n「提交汇总这个简化一下」") {
                        r#"{"reply":"已经帮你把刚才那份汇总精简成以下 3 点，同时也把每晚 22:00 的定时任务改成了精简速览模式：\n- xixing：完成 A/B 反馈与 0.1.5 部署\n- orbit：注册 lab3 CLI\n- ansible：更新网络配置","tools":[{"tool":"manage_cron","action":"update","cron_id":"cron-7d2629","daily_time_cst":"22:00","instruction":"汇总一下 GitLab 上的提交总结给我（精简速览版：每个项目只列 1~2 条核心进展）","deduplicate_ids":["cron-1f1b46"]}]}"#
                    } else if msg.contains("【用户最新消息】\n「有些不用你继续去提醒的怎么处理，比如这个抖音」") {
                        r#"{"reply":"处理办法我定成一条规矩，叫**静默挂起**，已经记下来了：记录保留、事情不关闭，但我不再在主动消息、早报和顺带提醒里带它。抖音这件已经挂起了。","tools":[{"tool":"update_memory","memory_append":"静默挂起规矩：用户点名不需继续提醒的事项标为 muted，保留记录但绝不在早报或顺带提醒中主动催促"},{"tool":"update_ledger","action":"mute","title":"抖音植入报价","status":"muted","notes_append":"待确认账号/粉丝/播放三个数，平时静默不提醒"}]}"#
                    } else if msg.contains("【用户最新消息】\n「部署到线上」") {
                        r#"{"reply":"明白，正在让办公主力机（homelab）把刚完成的本地提交 13dba7c 同步部署到线上 Hub，完成后结果会自动发回这里。","tools":[{"tool":"dispatch_pc_node","node_id":"homelab","instruction":"将 agy1 刚在本地完成的提交 13dba7c 同步部署到线上运行的 Hub 服务","topic_id":"t-agenct","topic_title":"📁 agenct 仓库上下文"}]}"#
                    } else {
                        "ok"
                    }
                } else {
                    "ok"
                };
                axum::Json(json!({ "reply": reply, "backend": "dsh", "proposal": Value::Null }))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1/turn", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, adapter).await.unwrap() });

        let mut h = test_hub();
        h.runtime = crate::Runtime::Dsh {
            url,
            token: "test-token".into(),
        };
        super::with_store_mut(&h, |store| {
            store.crons.push(super::CronJob {
                id: "cron-7d2629".into(),
                owner: "alice".into(),
                interval_minutes: 1440,
                daily_time_cst: Some("22:00".into()),
                instruction: "汇总一下gitlab上的提交总结给我".into(),
                target: "cloud".into(),
                last_run_unix: Some(1795010404),
                enabled: true,
                created_at: 1794886396,
            });
            store.crons.push(super::CronJob {
                id: "cron-1f1b46".into(),
                owner: "alice".into(),
                interval_minutes: 1440,
                daily_time_cst: Some("22:00".into()),
                instruction: "汇总一下gitlab上的提交给我".into(),
                target: "cloud".into(),
                last_run_unix: Some(1795010404),
                enabled: true,
                created_at: 1794888996,
            });
        })
        .unwrap();
        {
            let db = h.db().unwrap();
            db.execute(
                "INSERT INTO user_nodes (id, owner, name, token_hash, capabilities, last_seen_unix, created_at)
                 VALUES ('homelab', 'alice', '办公主力机', 'h-muse', 'cli:codex,agy | repos:agenct', 1, '2026-11-18')",
                [],
            )
            .unwrap();
            db.execute(
                "INSERT INTO tasks (id, requester, owner, summary, status, result, revision)
                 VALUES ('t-agenct', 'alice', 'alice', '📁 agenct 仓库上下文', 'completed', '已完成附件按钮改造', 1)",
                [],
            )
            .unwrap();
            db.execute(
                "INSERT INTO messages (owner, role, content, status)
                 VALUES ('alice', 'assistant', '⏰ 管家 7×24h 云端定时任务报告：zhq/xixing (6条)...', 'complete')",
                [],
            )
            .unwrap();
        }

        let r = app(h.clone());
        // 1. Compound turn: simplify report in natural reply AND update/deduplicate cron in background without PC node dispatch
        let (s, chat1) = call(
            &r,
            &token("alice"),
            "POST",
            "/api/messages",
            Some(json!({ "message": "提交汇总这个简化一下" })),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        let reply1 = chat1["reply"].as_str().unwrap();
        assert!(reply1.contains("已经帮你把刚才那份汇总精简成以下 3 点"));
        assert!(!reply1.contains("⏰ 已为你更新"));

        let (s, auto_state) = call(&r, &token("alice"), "GET", "/api/automation", None).await;
        assert_eq!(s, StatusCode::OK);
        let crons = auto_state["crons"].as_array().unwrap();
        assert_eq!(crons.len(), 1);
        assert_eq!(crons[0]["id"], "cron-7d2629");
        assert!(crons[0]["instruction"]
            .as_str()
            .unwrap()
            .contains("精简速览版"));

        // 2. Contextual node dispatch turn: "部署到线上" expands instruction and queues on homelab with Muse's natural reply
        let (s, chat2) = call(
            &r,
            &token("alice"),
            "POST",
            "/api/messages",
            Some(json!({ "message": "部署到线上" })),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        let reply2 = chat2["reply"].as_str().unwrap();
        assert!(reply2.contains("正在让办公主力机（homelab）把刚完成的本地提交 13dba7c 同步部署到线上 Hub"));

        let instr: String = h
            .db()
            .unwrap()
            .query_row(
                "SELECT instruction FROM node_assignments WHERE task_id='t-agenct'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            instr,
            "将 agy1 刚在本地完成的提交 13dba7c 同步部署到线上运行的 Hub 服务"
        );

        // 3. Multi-action rule + live ledger mute turn ("有些不用你继续去提醒的怎么处理，比如这个抖音")
        let (s, chat3) = call(
            &r,
            &token("alice"),
            "POST",
            "/api/messages",
            Some(json!({ "message": "有些不用你继续去提醒的怎么处理，比如这个抖音" })),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert!(chat3["reply"].as_str().unwrap().contains("静默挂起"));

        let (s, auto_state2) = call(&r, &token("alice"), "GET", "/api/automation", None).await;
        assert_eq!(s, StatusCode::OK);
        let mem_md = auto_state2["muse"]["memory_md"].as_str().unwrap();
        assert!(mem_md.contains("静默挂起规矩"));
        let ledgers = auto_state2["ledgers"].as_array().unwrap();
        assert_eq!(ledgers.len(), 1);
        assert_eq!(ledgers[0]["title"], "抖音植入报价");
        assert_eq!(ledgers[0]["status"], "muted");
    }
}
