//! Automation permissions (all default false), dispatch rules, cron jobs, and background scheduler.
use crate::{
    auth::AuthUser,
    config::{now_unix, truncate_bytes},
    db::record_audit,
    denied, invalid, nodes, Hub, Result,
};
use axum::{
    extract::{Path, State},
    Json,
};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::HashSet,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tokio::{sync::Semaphore, task::JoinHandle};
use uuid::Uuid;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Permissions {
    #[serde(default)]
    pub auto_accept: bool,
    #[serde(default)]
    pub auto_deliver: bool,
    #[serde(default)]
    pub gitlab_execute: bool,
    #[serde(default)]
    pub gitlab_reply: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct DispatchRule {
    pub id: String,
    pub keyword: String,
    pub target_node_id: String,
    pub require_human: bool,
    pub created_at: i64,
}

#[derive(Clone, Debug, Serialize)]
pub struct CronJob {
    pub id: String,
    pub interval_minutes: i64,
    pub daily_time_cst: Option<String>,
    pub instruction: String,
    pub target: String,
    pub enabled: bool,
    pub last_run_at: Option<i64>,
    /// '' (never run) | 'running' | 'ok' | 'failed'
    pub last_status: String,
    pub last_error: String,
    /// Consecutive failures since the last success.
    pub fail_count: i64,
    /// When set, the job is retried at this time regardless of its normal schedule.
    pub next_retry_at: Option<i64>,
    pub created_at: i64,
}

const CRON_COLUMNS: &str = "id, interval_minutes, daily_time_cst, instruction, target, enabled,
     last_run_at, last_status, last_error, fail_count, next_retry_at, created_at";

fn cron_from_row(r: &rusqlite::Row<'_>, offset: usize) -> rusqlite::Result<CronJob> {
    Ok(CronJob {
        id: r.get(offset)?,
        interval_minutes: r.get(offset + 1)?,
        daily_time_cst: r.get(offset + 2)?,
        instruction: r.get(offset + 3)?,
        target: r.get(offset + 4)?,
        enabled: r.get::<_, i64>(offset + 5)? != 0,
        last_run_at: r.get(offset + 6)?,
        last_status: r.get(offset + 7)?,
        last_error: r.get(offset + 8)?,
        fail_count: r.get(offset + 9)?,
        next_retry_at: r.get(offset + 10)?,
        created_at: r.get(offset + 11)?,
    })
}

pub fn read_permissions_sync(conn: &Connection, user_id: &str) -> Result<Permissions> {
    Ok(conn
        .query_row(
            "SELECT auto_accept, auto_deliver, gitlab_execute, gitlab_reply
             FROM automation_permissions WHERE user_id = ?1",
            [user_id],
            |r| {
                Ok(Permissions {
                    auto_accept: r.get::<_, i64>(0)? != 0,
                    auto_deliver: r.get::<_, i64>(1)? != 0,
                    gitlab_execute: r.get::<_, i64>(2)? != 0,
                    gitlab_reply: r.get::<_, i64>(3)? != 0,
                })
            },
        )
        .optional()?
        .unwrap_or_default())
}

pub fn revoke_disallowed_jobs_sync(
    conn: &Connection,
    user_id: &str,
    perms: &Permissions,
) -> Result<()> {
    conn.execute(
        "UPDATE node_jobs SET status='failed', lease_hash=NULL, lease_until=NULL, updated_at=?4
         WHERE user_id = ?1 AND status IN ('queued','running')
           AND ((authorization='delegation' AND ?2 = 0) OR (authorization='gitlab' AND ?3 = 0))",
        params![
            user_id,
            i64::from(perms.auto_accept),
            i64::from(perms.gitlab_execute),
            now_unix()
        ],
    )?;
    Ok(())
}

pub fn list_rules_sync(conn: &Connection, user_id: &str) -> Result<Vec<DispatchRule>> {
    let mut stmt = conn.prepare(
        "SELECT id, keyword, target_node_id, require_human, created_at
         FROM automation_rules WHERE user_id = ?1 ORDER BY created_at DESC",
    )?;
    let rows = stmt.query_map([user_id], |r| {
        Ok(DispatchRule {
            id: r.get(0)?,
            keyword: r.get(1)?,
            target_node_id: r.get(2)?,
            require_human: r.get::<_, i64>(3)? != 0,
            created_at: r.get(4)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

pub fn list_crons_sync(conn: &Connection, user_id: &str) -> Result<Vec<CronJob>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {CRON_COLUMNS} FROM cron_jobs WHERE user_id = ?1 ORDER BY created_at DESC"
    ))?;
    let rows = stmt.query_map([user_id], |r| cron_from_row(r, 0))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

pub fn match_rule_sync(
    conn: &Connection,
    user_id: &str,
    text: &str,
) -> Result<Option<DispatchRule>> {
    let lower = text.to_ascii_lowercase();
    let rules = list_rules_sync(conn, user_id)?;
    Ok(rules
        .into_iter()
        .find(|r| !r.keyword.is_empty() && lower.contains(&r.keyword.to_ascii_lowercase())))
}

pub async fn get_automation(
    State(hub): State<Hub>,
    AuthUser(user): AuthUser,
) -> Result<Json<Value>> {
    let uid = user.id;
    let payload = hub
        .db
        .read(move |conn| {
            let permissions = read_permissions_sync(conn, &uid)?;
            let rules = list_rules_sync(conn, &uid)?;
            let crons = list_crons_sync(conn, &uid)?;
            Ok(json!({
                "permissions": permissions,
                "rules": rules,
                "crons": crons,
            }))
        })
        .await?;
    Ok(Json(payload))
}

pub async fn update_permissions(
    State(hub): State<Hub>,
    AuthUser(user): AuthUser,
    Json(input): Json<Permissions>,
) -> Result<Json<Permissions>> {
    let uid = user.id.clone();
    let next = input.clone();
    hub.db
        .write(move |conn| {
            let tx = conn.transaction()?;
            tx.execute(
                "INSERT INTO automation_permissions(user_id, auto_accept, auto_deliver, gitlab_execute, gitlab_reply, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(user_id) DO UPDATE SET
                   auto_accept = excluded.auto_accept,
                   auto_deliver = excluded.auto_deliver,
                   gitlab_execute = excluded.gitlab_execute,
                   gitlab_reply = excluded.gitlab_reply,
                   updated_at = excluded.updated_at",
                params![
                    uid,
                    i64::from(next.auto_accept),
                    i64::from(next.auto_deliver),
                    i64::from(next.gitlab_execute),
                    i64::from(next.gitlab_reply),
                    now_unix()
                ],
            )?;
            revoke_disallowed_jobs_sync(&tx, &uid, &next)?;
            record_audit(
                &tx,
                &uid,
                "permissions",
                "ok",
                "Updated automation permissions",
                &format!(
                    "auto_accept={} auto_deliver={} gitlab_execute={} gitlab_reply={}",
                    next.auto_accept, next.auto_deliver, next.gitlab_execute, next.gitlab_reply
                ),
            )?;
            tx.commit()?;
            Ok(())
        })
        .await?;
    hub.events
        .emit(user.id, "automation", json!({"permissions": input}));
    Ok(Json(input))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateRuleInput {
    pub keyword: String,
    #[serde(default = "default_target")]
    pub target_node_id: String,
    #[serde(default)]
    pub require_human: bool,
}

fn default_target() -> String {
    "cloud".into()
}

pub async fn create_rule(
    State(hub): State<Hub>,
    AuthUser(user): AuthUser,
    Json(input): Json<CreateRuleInput>,
) -> Result<Json<DispatchRule>> {
    let kw = input.keyword.trim();
    if kw.is_empty() || kw.len() > 64 {
        return Err(invalid("keyword must be 1..64 bytes"));
    }
    let rule = DispatchRule {
        id: format!("rul_{}", &Uuid::new_v4().simple().to_string()[..8]),
        keyword: kw.to_owned(),
        target_node_id: truncate_bytes(input.target_node_id.trim(), 48),
        require_human: input.require_human,
        created_at: now_unix(),
    };
    let r_db = rule.clone();
    let uid = user.id.clone();
    hub.db
        .write(move |conn| {
            conn.execute(
                "INSERT INTO automation_rules(id, user_id, keyword, target_node_id, require_human, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    r_db.id,
                    uid,
                    r_db.keyword,
                    r_db.target_node_id,
                    i64::from(r_db.require_human),
                    r_db.created_at
                ],
            )?;
            Ok(())
        })
        .await?;
    Ok(Json(rule))
}

pub async fn delete_rule(
    State(hub): State<Hub>,
    AuthUser(user): AuthUser,
    Path(id): Path<String>,
) -> Result<Json<Value>> {
    let uid = user.id;
    let rid = id.clone();
    hub.db
        .write(move |conn| {
            if conn.execute(
                "DELETE FROM automation_rules WHERE id = ?1 AND user_id = ?2",
                params![rid, uid],
            )? == 0
            {
                return Err(denied());
            }
            Ok(())
        })
        .await?;
    Ok(Json(json!({"ok": true, "id": id})))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateCronInput {
    #[serde(default = "default_interval")]
    pub interval_minutes: i64,
    #[serde(default)]
    pub daily_time_cst: Option<String>,
    pub instruction: String,
    #[serde(default = "default_target")]
    pub target: String,
}

fn default_interval() -> i64 {
    60
}

pub fn parse_hhmm(raw: &str) -> Option<String> {
    let mut parts = raw.trim().split(':');
    let h: u32 = parts.next()?.parse().ok()?;
    let m: u32 = parts.next()?.parse().ok()?;
    if parts.next().is_some() || h > 23 || m > 59 {
        return None;
    }
    Some(format!("{h:02}:{m:02}"))
}

pub async fn create_cron(
    State(hub): State<Hub>,
    AuthUser(user): AuthUser,
    Json(input): Json<CreateCronInput>,
) -> Result<Json<CronJob>> {
    let instr = input.instruction.trim();
    if instr.is_empty() || instr.len() > 1000 {
        return Err(invalid("cron instruction must be 1..1000 bytes"));
    }
    let daily = match input.daily_time_cst.as_deref() {
        Some(raw) if !raw.trim().is_empty() => {
            Some(parse_hhmm(raw).ok_or_else(|| invalid("daily_time_cst must be HH:MM"))?)
        }
        _ => None,
    };
    let interval = if daily.is_some() {
        1440
    } else {
        input.interval_minutes.clamp(1, 10_080)
    };
    let job = CronJob {
        id: format!("crn_{}", &Uuid::new_v4().simple().to_string()[..8]),
        interval_minutes: interval,
        daily_time_cst: daily,
        instruction: instr.to_owned(),
        target: truncate_bytes(input.target.trim(), 48),
        enabled: true,
        last_run_at: None,
        last_status: String::new(),
        last_error: String::new(),
        fail_count: 0,
        next_retry_at: None,
        created_at: now_unix(),
    };
    let j_db = job.clone();
    let uid = user.id.clone();
    hub.db
        .write(move |conn| {
            conn.execute(
                "INSERT INTO cron_jobs(id, user_id, interval_minutes, daily_time_cst, instruction, target, enabled, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1, ?7)",
                params![
                    j_db.id,
                    uid,
                    j_db.interval_minutes,
                    j_db.daily_time_cst,
                    j_db.instruction,
                    j_db.target,
                    j_db.created_at
                ],
            )?;
            Ok(())
        })
        .await?;
    hub.scheduler_notify.notify_one();
    Ok(Json(job))
}

pub async fn delete_cron(
    State(hub): State<Hub>,
    AuthUser(user): AuthUser,
    Path(id): Path<String>,
) -> Result<Json<Value>> {
    let uid = user.id;
    let cid = id.clone();
    hub.db
        .write(move |conn| {
            if conn.execute(
                "DELETE FROM cron_jobs WHERE id = ?1 AND user_id = ?2",
                params![cid, uid],
            )? == 0
            {
                return Err(denied());
            }
            Ok(())
        })
        .await?;
    Ok(Json(json!({"ok": true, "id": id})))
}

/// Max concurrent cron executions (LLM / network bound).
const CRON_CONCURRENCY: usize = 4;
/// Retries after a failed run before falling back to the regular schedule.
pub const CRON_MAX_RETRIES: i64 = 3;
const CRON_RETRY_BASE_SECS: i64 = 60;
const CRON_RETRY_MAX_SECS: i64 = 1800;

/// Tools a cron-triggered brain run may call: no self-replicating crons/rules, no memory edits,
/// no cross-user delegation.
const CRON_ALLOWED_TOOLS: &[&str] = &[
    "fetch_gitlab_commits",
    "dispatch_node",
    "update_ledger",
    "close_thread",
];

const CRON_PROMPT_SUFFIX: &str = "

=== 当前运行模式：定时任务自动触发 ===
- 这不是用户的实时发言，用户此刻不在线等待；本轮「用户消息」就是定时任务指令本身。
- 直接执行并输出简洁的 Markdown 汇报结论，不要反问、不要寒暄。
- 需要数据时调用工具（如 fetch_gitlab_commits / dispatch_node），只基于工具真实结果汇报；拿不到数据就如实说明。
- 本模式只允许调用：fetch_gitlab_commits、dispatch_node、update_ledger、close_thread。";

/// In-process scheduler bookkeeping (single Hub instance by design).
pub struct SchedulerState {
    /// Cron ids currently executing; a slow run is never started twice concurrently.
    inflight: Mutex<HashSet<String>>,
    slots: Arc<Semaphore>,
    polling_todos: AtomicBool,
}

impl Default for SchedulerState {
    fn default() -> Self {
        Self {
            inflight: Mutex::new(HashSet::new()),
            slots: Arc::new(Semaphore::new(CRON_CONCURRENCY)),
            polling_todos: AtomicBool::new(false),
        }
    }
}

/// Releases an in-flight cron id even if the run panics.
struct InflightGuard {
    state: Arc<SchedulerState>,
    id: String,
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        if let Ok(mut set) = self.state.inflight.lock() {
            set.remove(&self.id);
        }
    }
}

/// Clears the GitLab todo polling flag even if the poll panics.
struct PollGuard(Arc<SchedulerState>);

impl Drop for PollGuard {
    fn drop(&mut self) {
        self.0.polling_todos.store(false, Ordering::Release);
    }
}

/// Exponential backoff for the n-th consecutive failure: 1m, 2m, 4m, … capped at 30m.
pub fn retry_delay_secs(fail_count: i64) -> i64 {
    let exp = u32::try_from((fail_count - 1).clamp(0, 10)).unwrap_or(0);
    (CRON_RETRY_BASE_SECS * 2_i64.pow(exp)).min(CRON_RETRY_MAX_SECS)
}

fn is_cron_due(job: &CronJob, now: i64) -> bool {
    if !job.enabled {
        return false;
    }
    if job.next_retry_at.is_some_and(|t| t <= now) {
        return true;
    }
    if let Some(hhmm) = &job.daily_time_cst {
        // UTC+8 (Asia/Shanghai)
        let cst = now + 8 * 3600;
        let cur_day = cst.div_euclid(86_400);
        let secs_in_day = cst.rem_euclid(86_400);
        let cur_h = (secs_in_day / 3600) as u32;
        let cur_m = ((secs_in_day % 3600) / 60) as u32;
        let mut parts = hhmm.split(':');
        let (Some(th), Some(tm)) = (
            parts.next().and_then(|v| v.parse::<u32>().ok()),
            parts.next().and_then(|v| v.parse::<u32>().ok()),
        ) else {
            return false;
        };
        if (cur_h, cur_m) < (th, tm) {
            return false;
        }
        if let Some(last) = job.last_run_at {
            let last_day = (last + 8 * 3600).div_euclid(86_400);
            if last_day >= cur_day {
                return false;
            }
        }
        return true;
    }
    let last = job.last_run_at.unwrap_or(job.created_at);
    now.saturating_sub(last) >= job.interval_minutes * 60
}

/// Atomically claims due jobs (skipping ones still in flight) and marks them `running`.
async fn claim_due_crons(hub: &Hub) -> Result<Vec<(String, CronJob)>> {
    let now = now_unix();
    let state = hub.scheduler.clone();
    hub.db
        .write(move |conn| {
            let mut inflight = state
                .inflight
                .lock()
                .map_err(|_| crate::Error(axum::http::StatusCode::INTERNAL_SERVER_ERROR, "scheduler state poisoned"))?;
            let tx = conn.transaction()?;
            let all: Vec<(String, CronJob)> = {
                let mut stmt = tx.prepare(&format!(
                    "SELECT user_id, {CRON_COLUMNS} FROM cron_jobs
                     WHERE enabled = 1 AND user_id IN (SELECT id FROM users WHERE status = 'active')"
                ))?;
                let rows = stmt
                    .query_map([], |r| Ok((r.get::<_, String>(0)?, cron_from_row(r, 1)?)))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                rows
            };
            let mut claimed = Vec::new();
            for (uid, mut job) in all {
                if inflight.contains(&job.id) || !is_cron_due(&job, now) {
                    continue;
                }
                tx.execute(
                    "UPDATE cron_jobs SET last_run_at = ?2, last_status = 'running', next_retry_at = NULL
                     WHERE id = ?1",
                    params![job.id, now],
                )?;
                job.last_run_at = Some(now);
                job.last_status = "running".into();
                job.next_retry_at = None;
                claimed.push((uid, job));
            }
            tx.commit()?;
            for (_, job) in &claimed {
                inflight.insert(job.id.clone());
            }
            Ok(claimed)
        })
        .await
}

fn spawn_cron(hub: &Hub, user_id: String, job: CronJob) -> JoinHandle<()> {
    let hub = hub.clone();
    tokio::spawn(async move {
        let _inflight = InflightGuard {
            state: hub.scheduler.clone(),
            id: job.id.clone(),
        };
        let _permit = hub.scheduler.slots.clone().acquire_owned().await.ok();
        let result = execute_cron_job(&hub, &user_id, &job).await;
        record_cron_result(&hub, &user_id, &job, result).await;
    })
}

/// Claims all due crons and runs them concurrently, waiting until every run has finished.
pub async fn run_due_crons(hub: &Hub) -> Result<usize> {
    let due = claim_due_crons(hub).await?;
    let count = due.len();
    let handles: Vec<_> = due
        .into_iter()
        .map(|(uid, job)| spawn_cron(hub, uid, job))
        .collect();
    for handle in handles {
        let _ = handle.await;
    }
    Ok(count)
}

/// Executes one cron run. `Ok` carries a short summary for the audit log, `Err` the failure reason.
async fn execute_cron_job(
    hub: &Hub,
    user_id: &str,
    job: &CronJob,
) -> std::result::Result<String, String> {
    if job.target == "cloud" {
        // Cloud crons run through the same agent loop as chat (tools + result feedback),
        // restricted to a safe tool set. No keyword routing.
        let out = crate::brain::run_headless(
            hub,
            user_id,
            &job.instruction,
            CRON_PROMPT_SUFFIX,
            CRON_ALLOWED_TOOLS,
        )
        .await
        .map_err(|e| e.1.to_owned())?;
        let report = out.reply;
        let msg = format!("**[定时汇报]** {}\n\n{}", job.instruction, report);
        let uid = user_id.to_owned();
        let thread_id = out.thread_id;
        hub.db
            .write(move |conn| {
                conn.execute(
                    "INSERT INTO messages(user_id, thread_id, role, content, status, created_at)
                     VALUES (?1, ?2, 'assistant', ?3, 'complete', ?4)",
                    params![uid, thread_id, truncate_bytes(&msg, 16_000), now_unix()],
                )?;
                Ok(())
            })
            .await
            .map_err(|e| e.1.to_owned())?;
        hub.events
            .emit(user_id.to_owned(), "message", json!({"cron_id": job.id}));
        crate::integrations::im::maybe_notify(
            hub,
            user_id,
            "delivery",
            &format!("定时汇报：{}", job.instruction),
            &report,
        )
        .await;
        let dispatched = if out.tasks.is_empty() {
            String::new()
        } else {
            format!("（派发 {} 个任务）", out.tasks.len())
        };
        return Ok(truncate_bytes(&format!("{dispatched}{report}"), 240));
    }

    // Explicit node target: enqueue directly on the targeted (or any online) execution node.
    let uid = user_id.to_owned();
    let target = job.target.clone();
    let instr = job.instruction.clone();
    let picked = hub
        .db
        .write(move |conn| {
            let prefer_cloud = target == "cloud-node";
            let explicit =
                (!matches!(target.as_str(), "auto" | "cloud-node")).then_some(target.as_str());
            let Some(node) =
                nodes::pick_node_for_task_sync(conn, &uid, explicit, prefer_cloud, Some(&instr))?
            else {
                return Ok(None);
            };
            nodes::create_task_and_enqueue_sync(
                conn,
                &uid,
                &uid,
                &node.id,
                &format!("定时：{}", truncate_bytes(&instr, 48)),
                &instr,
                "cron",
            )?;
            Ok(Some((node.id, node.name)))
        })
        .await
        .map_err(|e| e.1.to_owned())?;
    match picked {
        Some((node_id, node_name)) => {
            hub.node_waiters.notify(user_id, &node_id);
            hub.events
                .emit(user_id.to_owned(), "task", json!({"cron_id": job.id}));
            Ok(format!("已派发到节点 {node_name}"))
        }
        None => Err(format!("没有可用的执行节点（target={}）", job.target)),
    }
}

/// Records the outcome: success resets the failure streak; failure schedules a backoff retry
/// (up to `CRON_MAX_RETRIES`) and notifies the user once when retries are exhausted.
async fn record_cron_result(
    hub: &Hub,
    user_id: &str,
    job: &CronJob,
    result: std::result::Result<String, String>,
) {
    let now = now_unix();
    let uid = user_id.to_owned();
    let id = job.id.clone();
    let instr = job.instruction.clone();
    let err_text = result.as_ref().err().cloned();
    let gave_up = hub
        .db
        .write(move |conn| {
            let tx = conn.transaction()?;
            let gave_up = match &result {
                Ok(summary) => {
                    tx.execute(
                        "UPDATE cron_jobs SET last_status = 'ok', last_error = '', fail_count = 0,
                                next_retry_at = NULL
                         WHERE id = ?1",
                        [&id],
                    )?;
                    record_audit(&tx, &uid, "cron_run", "ok", &instr, summary)?;
                    false
                }
                Err(err) => {
                    let fails = tx
                        .query_row("SELECT fail_count FROM cron_jobs WHERE id = ?1", [&id], |r| {
                            r.get::<_, i64>(0)
                        })
                        .optional()?
                        .unwrap_or(0)
                        + 1;
                    let next_retry = (fails <= CRON_MAX_RETRIES).then(|| now + retry_delay_secs(fails));
                    tx.execute(
                        "UPDATE cron_jobs SET last_status = 'failed', last_error = ?2, fail_count = ?3,
                                next_retry_at = ?4
                         WHERE id = ?1",
                        params![id, truncate_bytes(err, 300), fails, next_retry],
                    )?;
                    record_audit(&tx, &uid, "cron_run", "failed", &instr, err)?;
                    let gave_up = fails == CRON_MAX_RETRIES + 1;
                    if gave_up {
                        tx.execute(
                            "INSERT INTO messages(user_id, role, content, status, created_at)
                             VALUES (?1, 'assistant', ?2, 'complete', ?3)",
                            params![
                                uid,
                                format!(
                                    "**[定时任务失败]** {instr}\n\n已连续失败 {fails} 次（最近错误：{}），停止重试，将在下一个正常周期再执行。",
                                    truncate_bytes(err, 200)
                                ),
                                now
                            ],
                        )?;
                    }
                    gave_up
                }
            };
            tx.commit()?;
            Ok(gave_up)
        })
        .await
        .unwrap_or(false);

    hub.events.emit(
        user_id.to_owned(),
        "automation",
        json!({"updated": "crons"}),
    );
    if gave_up {
        hub.events
            .emit(user_id.to_owned(), "message", json!({"cron_id": job.id}));
        crate::integrations::im::maybe_notify(
            hub,
            user_id,
            "delivery",
            &format!("定时任务失败：{}", job.instruction),
            err_text.as_deref().unwrap_or(""),
        )
        .await;
    }
}

impl Hub {
    /// Background loop: every 30s (or when poked) claims due crons and starts them as
    /// independent bounded tasks, and kicks off GitLab todo polling if it isn't already running.
    /// Nothing here awaits a slow run, so one stuck job never delays the others.
    pub async fn run_scheduler(&self) {
        let mut interval = tokio::time::interval(Duration::from_secs(30));
        loop {
            tokio::select! {
                _ = interval.tick() => {}
                _ = self.scheduler_notify.notified() => {}
            }
            if let Ok(due) = claim_due_crons(self).await {
                for (uid, job) in due {
                    drop(spawn_cron(self, uid, job));
                }
            }
            if !self.scheduler.polling_todos.swap(true, Ordering::AcqRel) {
                let hub = self.clone();
                tokio::spawn(async move {
                    let _guard = PollGuard(hub.scheduler.clone());
                    crate::integrations::gitlab::poll_all_todos(&hub).await;
                });
            }
        }
    }
}
