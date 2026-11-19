//! Automation permissions (all default false), dispatch rules, cron jobs, and background scheduler.
use crate::{
    config::{now_unix, truncate_bytes},
    db::record_audit,
    denied, invalid, nodes, Hub, Result,
};
use axum::{
    extract::{Path, State},
    http::HeaderMap,
    Json,
};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::time::Duration;
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
    pub created_at: i64,
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
    let mut stmt = conn.prepare(
        "SELECT id, interval_minutes, daily_time_cst, instruction, target, enabled, last_run_at, created_at
         FROM cron_jobs WHERE user_id = ?1 ORDER BY created_at DESC",
    )?;
    let rows = stmt.query_map([user_id], |r| {
        Ok(CronJob {
            id: r.get(0)?,
            interval_minutes: r.get(1)?,
            daily_time_cst: r.get(2)?,
            instruction: r.get(3)?,
            target: r.get(4)?,
            enabled: r.get::<_, i64>(5)? != 0,
            last_run_at: r.get(6)?,
            created_at: r.get(7)?,
        })
    })?;
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

pub async fn get_automation(State(hub): State<Hub>, headers: HeaderMap) -> Result<Json<Value>> {
    let user = hub.require_user(&headers).await?;
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
    headers: HeaderMap,
    Json(input): Json<Permissions>,
) -> Result<Json<Permissions>> {
    let user = hub.require_user(&headers).await?;
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
    headers: HeaderMap,
    Json(input): Json<CreateRuleInput>,
) -> Result<Json<DispatchRule>> {
    let user = hub.require_user(&headers).await?;
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
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Value>> {
    let user = hub.require_user(&headers).await?;
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
    headers: HeaderMap,
    Json(input): Json<CreateCronInput>,
) -> Result<Json<CronJob>> {
    let user = hub.require_user(&headers).await?;
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
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Value>> {
    let user = hub.require_user(&headers).await?;
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

fn is_cron_due(job: &CronJob, now: i64) -> bool {
    if !job.enabled {
        return false;
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

pub async fn run_due_crons(hub: &Hub) -> Result<usize> {
    let now = now_unix();
    let due: Vec<(String, CronJob)> = hub
        .db
        .write(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT c.user_id, c.id, c.interval_minutes, c.daily_time_cst, c.instruction,
                        c.target, c.enabled, c.last_run_at, c.created_at
                 FROM cron_jobs c
                 JOIN users u ON u.id = c.user_id
                 WHERE c.enabled = 1 AND u.status = 'active'",
            )?;
            let all = stmt
                .query_map([], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        CronJob {
                            id: r.get(1)?,
                            interval_minutes: r.get(2)?,
                            daily_time_cst: r.get(3)?,
                            instruction: r.get(4)?,
                            target: r.get(5)?,
                            enabled: r.get::<_, i64>(6)? != 0,
                            last_run_at: r.get(7)?,
                            created_at: r.get(8)?,
                        },
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;

            let mut claimed = Vec::new();
            for (uid, job) in all {
                if is_cron_due(&job, now) {
                    conn.execute(
                        "UPDATE cron_jobs SET last_run_at = ?2 WHERE id = ?1",
                        params![job.id, now],
                    )?;
                    claimed.push((uid, job));
                }
            }
            Ok(claimed)
        })
        .await?;

    let count = due.len();
    for (uid, job) in due {
        execute_cron_job(hub, &uid, &job).await;
    }
    Ok(count)
}

async fn execute_cron_job(hub: &Hub, user_id: &str, job: &CronJob) {
    let lower = job.instruction.to_ascii_lowercase();
    let wants_gitlab = lower.contains("gitlab") || job.instruction.contains("提交");

    if job.target == "cloud" || wants_gitlab {
        let report = if wants_gitlab {
            crate::integrations::gitlab::fetch_commit_summary(hub, user_id)
                .await
                .unwrap_or_else(|| "暂无新的 GitLab 提交记录或尚未配置 GitLab PAT。".into())
        } else {
            hub.llm
                .complete(
                    "你是用户的 7x24h 个人超级管家，请简洁执行以下定时汇报指令并输出 Markdown 结论。",
                    &[],
                    &job.instruction,
                    false,
                )
                .await
                .unwrap_or_else(|_| format!("定时任务执行失败：{}", job.instruction))
        };
        let msg = format!("**[定时汇报]** {}\n\n{}", job.instruction, report);
        let uid = user_id.to_owned();
        let instr = job.instruction.clone();
        let _ = hub
            .db
            .write(move |conn| {
                conn.execute(
                    "INSERT INTO messages(user_id, role, content, status, created_at)
                     VALUES (?1, 'assistant', ?2, 'complete', ?3)",
                    params![uid, msg, now_unix()],
                )?;
                record_audit(conn, &uid, "cron_run", "ok", &instr, "Cloud cron completed")?;
                Ok(())
            })
            .await;
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
        return;
    }

    // Dispatch to targeted or online execution node (PC or Cloud Runner).
    let uid = user_id.to_owned();
    let target = job.target.clone();
    let instr = job.instruction.clone();
    let _ = hub
        .db
        .write(move |conn| {
            let prefer_cloud = target == "cloud-node";
            let explicit =
                (!matches!(target.as_str(), "auto" | "cloud-node")).then_some(target.as_str());
            if let Some(node) = nodes::pick_node_sync(conn, &uid, explicit, prefer_cloud)? {
                nodes::create_task_and_enqueue_sync(
                    conn,
                    &uid,
                    &uid,
                    &node.id,
                    &format!("定时：{}", truncate_bytes(&instr, 48)),
                    &instr,
                    "cron",
                )?;
            }
            Ok(())
        })
        .await;
    hub.events
        .emit(user_id.to_owned(), "task", json!({"cron_id": job.id}));
}

impl Hub {
    pub async fn run_scheduler(&self) {
        let mut interval = tokio::time::interval(Duration::from_secs(30));
        loop {
            tokio::select! {
                _ = interval.tick() => {}
                _ = self.scheduler_notify.notified() => {}
            }
            let _ = run_due_crons(self).await;
            crate::integrations::gitlab::poll_all_todos(self).await;
        }
    }
}
