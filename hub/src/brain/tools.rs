//! Brain tool execution handlers (memory, ledgers, roles, crons, rules, GitLab, nodes, colleagues).
use super::{
    normalize_avatar_id,
    parser::ToolCall,
    read_profile_sync,
    roles::{self, AgentRole},
    threads::{
        archive_thread, create_thread, delete_empty_thread, resolve_or_create_thread,
        update_thread_meta,
    },
    Ledger, TurnContext,
};
use crate::{
    automation, collab,
    config::{now_unix, truncate_bytes},
    db::record_audit,
    nodes, Hub, Result,
};
use rusqlite::{params, OptionalExtension};
use serde_json::{json, Value};
use uuid::Uuid;

/// Per-tool observation budget fed back into the model.
pub(super) const TOOL_OBSERVATION_MAX_BYTES: usize = 4000;

/// Result of executing one tool call, reported back to the model in the next agent step.
pub(super) struct ToolOutcome {
    pub(super) tool: String,
    pub(super) ok: bool,
    /// Factual result for the model (fed back as a `tool` / `TOOL_RESULTS` message).
    pub(super) observation: String,
    /// Text appended to the user-facing reply if the agent loop ends before the model
    /// could incorporate this outcome itself.
    pub(super) user_note: String,
    /// Whether the model must see this result before the turn can be answered well
    /// (query results and failures). Successful fire-and-forget mutations don't.
    pub(super) needs_followup: bool,
}

impl ToolOutcome {
    pub(super) fn done(tool: &str, observation: impl Into<String>) -> Self {
        Self {
            tool: tool.to_owned(),
            ok: true,
            observation: observation.into(),
            user_note: String::new(),
            needs_followup: false,
        }
    }

    pub(super) fn failed(
        tool: &str,
        observation: impl Into<String>,
        user_note: impl Into<String>,
    ) -> Self {
        Self {
            tool: tool.to_owned(),
            ok: false,
            observation: observation.into(),
            user_note: user_note.into(),
            needs_followup: true,
        }
    }
}

/// Identity of a side-effecting call, used to skip exact repeats within one turn.
fn side_effect_key(call: &ToolCall) -> Option<String> {
    let non_idempotent = matches!(
        call.tool.as_str(),
        "dispatch_node" | "delegate_colleague" | "manage_cron" | "manage_rule"
    );
    non_idempotent.then(|| {
        [
            call.tool.as_str(),
            call.action.as_deref().unwrap_or(""),
            call.role.as_deref().unwrap_or(""),
            call.agent.as_deref().unwrap_or(""),
            call.model.as_deref().unwrap_or(""),
            call.sandbox.as_deref().unwrap_or(""),
            call.peer.as_deref().unwrap_or(""),
            call.keyword.as_deref().unwrap_or(""),
            call.instruction.as_deref().unwrap_or("").trim(),
        ]
        .join("\u{1f}")
    })
}

pub(super) async fn execute_tool(ctx: &mut TurnContext<'_>, call: &ToolCall) -> ToolOutcome {
    if ctx
        .allowed_tools
        .is_some_and(|allowed| !allowed.contains(&call.tool.as_str()))
    {
        return ToolOutcome::failed(
            &call.tool,
            format!(
                "当前运行模式不允许调用 `{}`，请改用其它方式完成。",
                call.tool
            ),
            String::new(),
        );
    }
    let key = side_effect_key(call);
    if key.as_ref().is_some_and(|k| ctx.done_keys.contains(k)) {
        return ToolOutcome::done(&call.tool, "本轮已执行过相同调用，已跳过以避免重复。");
    }
    let outcome = match run_tool(ctx, call).await {
        Ok(o) => o,
        Err(err) => ToolOutcome::failed(
            &call.tool,
            format!("执行出错：{}", err.1),
            format!("（{} 执行失败：{}）", call.tool, err.1),
        ),
    };
    if outcome.ok {
        if let Some(k) = key {
            ctx.done_keys.insert(k);
        }
    }
    outcome
}

async fn run_tool(ctx: &mut TurnContext<'_>, call: &ToolCall) -> Result<ToolOutcome> {
    let hub = ctx.hub;
    let user_id = ctx.user_id;
    let tool = call.tool.as_str();
    match tool {
        "update_memory" => {
            ctx.stage("正在更新长期记忆…");
            apply_update_memory(hub, user_id, call).await?;
            Ok(ToolOutcome::done(tool, "已更新人设/长期记忆。"))
        }
        "update_ledger" => {
            ctx.stage("正在更新持续事项…");
            apply_update_ledger(hub, user_id, call).await?;
            Ok(ToolOutcome::done(tool, "已更新持续事项。"))
        }
        "manage_thread" => apply_manage_thread(ctx, call).await,
        "manage_cron" => {
            ctx.stage("正在更新定时任务…");
            apply_manage_cron(hub, user_id, call).await?;
            Ok(ToolOutcome::done(tool, "已更新定时任务。"))
        }
        "manage_rule" => {
            ctx.stage("正在更新分发规则…");
            apply_manage_rule(hub, user_id, call).await?;
            Ok(ToolOutcome::done(tool, "已更新分发规则。"))
        }
        "manage_role" => {
            ctx.stage("正在更新军团角色…");
            apply_manage_role(hub, user_id, ctx.node_list, call).await
        }
        "manage_node" => {
            ctx.stage("正在更新节点配置…");
            apply_manage_node(hub, user_id, ctx.node_list, call).await
        }
        "fetch_gitlab_commits" => {
            ctx.stage("正在拉取 GitLab 提交记录…");
            if let Some(summary) =
                crate::integrations::gitlab::fetch_commit_summary(hub, user_id).await
            {
                return Ok(ToolOutcome {
                    tool: tool.to_owned(),
                    ok: true,
                    observation: summary.clone(),
                    user_note: summary,
                    needs_followup: true,
                });
            }
            let online_pc: Vec<String> = ctx
                .node_list
                .iter()
                .filter(|n| n.online && n.kind == "pc")
                .map(|n| format!("{}（{}）", n.id, n.name))
                .collect();
            let observation = if online_pc.is_empty() {
                "未能拉取 GitLab 提交：未配置 GitLab 访问令牌或请求失败；当前也没有在线的本机节点可查询本地仓库。".to_owned()
            } else {
                format!(
                    "未能拉取 GitLab 提交：未配置 GitLab 访问令牌或请求失败。在线本机节点：{}。\
                     如用户需要提交记录，可调用 dispatch_node（prefer_cloud=false）让节点在本地仓库运行 git log 并汇总。",
                    online_pc.join("、")
                )
            };
            Ok(ToolOutcome::failed(
                tool,
                observation,
                "（未能拉取 GitLab 提交记录：尚未配置 GitLab 访问令牌或请求失败。）",
            ))
        }
        "dispatch_node" => {
            ctx.stage("正在派发到执行节点…");
            let mut base_instr = call
                .instruction
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or(ctx.message)
                .to_owned();
            // Nodes only receive attachments referenced in the instruction; forward the user's attachments explicitly.
            let missing: Vec<String> = crate::attachments::extract_attachment_ids(ctx.message)
                .into_iter()
                .filter(|id| !base_instr.contains(id.as_str()))
                .collect();
            if !missing.is_empty() {
                base_instr.push_str("\n\n用户附件：");
                for id in missing {
                    base_instr.push_str(&format!("\n![附件](/api/attachments/{id})"));
                }
            }
            let title = call
                .title
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| base_instr.lines().next().unwrap_or("节点任务"))
                .to_owned();

            let role_opt: Option<AgentRole> = if let Some(raw_role) = call
                .role
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                let Some(norm_role) = roles::normalize_role_name(raw_role) else {
                    return Ok(ToolOutcome::failed(
                        tool,
                        format!("非法军团角色名 `{raw_role}`。"),
                        format!("（非法军团角色名 `{raw_role}`。）"),
                    ));
                };
                let uid_r = user_id.to_owned();
                let rname = norm_role.clone();
                let found = hub
                    .db
                    .read(move |conn| roles::get_role_sync(conn, &uid_r, &rname))
                    .await?;
                if found.is_none() {
                    let uid_l = user_id.to_owned();
                    let all_roles = hub
                        .db
                        .read(move |conn| roles::list_roles_sync(conn, &uid_l))
                        .await?;
                    let names = if all_roles.is_empty() {
                        "暂无已配置角色".to_owned()
                    } else {
                        all_roles
                            .iter()
                            .map(|r| r.name.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    };
                    return Ok(ToolOutcome::failed(
                        tool,
                        format!("未找到名为 `{norm_role}` 的军团角色（当前已配置：{names}）。"),
                        format!("（未找到军团角色 `{norm_role}`。）"),
                    ));
                }
                found
            } else {
                None
            };

            let instr = match role_opt.as_ref().filter(|r| !r.prompt.trim().is_empty()) {
                Some(role) => format!(
                    "【角色规范 · {}】\n{}\n\n{base_instr}",
                    role.name,
                    role.prompt.trim()
                ),
                None => base_instr,
            };

            let explicit_agent = match call
                .agent
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                Some(raw) => match nodes::normalize_agent_name(raw) {
                    Some(norm) => Some(norm.to_owned()),
                    None => {
                        return Ok(ToolOutcome::failed(
                            tool,
                            format!(
                                "不支持的执行引擎 `{raw}`，可选引擎为：{}。",
                                nodes::CONFIGURABLE_AGENTS.join(", ")
                            ),
                            format!("（不支持的执行引擎 `{raw}`。）"),
                        ));
                    }
                },
                None => None,
            };
            let role_agent = role_opt
                .as_ref()
                .map(|r| r.agent.trim())
                .filter(|s| !s.is_empty())
                .and_then(nodes::normalize_agent_name);
            let agent_overridden = matches!(
                (explicit_agent.as_deref(), role_agent),
                (Some(exp), Some(ra)) if !exp.eq_ignore_ascii_case(ra)
            );
            let requested_agent = explicit_agent.or_else(|| role_agent.map(str::to_owned));

            let requested_model = match call
                .model
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                Some(raw) => match nodes::normalize_model_name(raw) {
                    Ok(m) => m,
                    Err(err) => {
                        return Ok(ToolOutcome::failed(
                            tool,
                            format!("模型名称无效 `{raw}`：{}", err.1),
                            format!("（模型名称无效：`{raw}`。）"),
                        ));
                    }
                },
                None if agent_overridden => String::new(),
                None => role_opt
                    .as_ref()
                    .map(|r| r.model.clone())
                    .unwrap_or_default(),
            };
            let requested_effort = match call
                .effort
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                Some(raw) => match nodes::normalize_effort(raw) {
                    Ok(e) => e,
                    Err(err) => {
                        return Ok(ToolOutcome::failed(
                            tool,
                            format!("推理强度无效 `{raw}`：{}。", err.1),
                            format!("（推理强度 `{raw}` 无效。）"),
                        ));
                    }
                },
                None if agent_overridden => String::new(),
                None => role_opt
                    .as_ref()
                    .map(|r| r.effort.clone())
                    .unwrap_or_default(),
            };
            let requested_sandbox = match call
                .sandbox
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                Some(raw) => match nodes::normalize_job_sandbox(raw) {
                    Ok(s) => s,
                    Err(err) => {
                        return Ok(ToolOutcome::failed(
                            tool,
                            format!("沙箱级别无效 `{raw}`：{}。", err.1),
                            format!("（沙箱级别 `{raw}` 无效。）"),
                        ));
                    }
                },
                None => role_opt
                    .as_ref()
                    .map(|r| r.sandbox.clone())
                    .unwrap_or_default(),
            };

            let preferred_node = call
                .node_id
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .or_else(|| {
                    role_opt
                        .as_ref()
                        .map(|r| r.node_id.trim())
                        .filter(|s| !s.is_empty())
                        .map(str::to_owned)
                })
                .or_else(|| ctx.matched_rule_target.clone());
            let prefer_cloud = call.prefer_cloud.unwrap_or(false);
            let raw_tid = call.thread_id.as_deref().map(str::trim);
            let topic_title = call
                .thread_title
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or(&title);
            let skip_thread = raw_tid.is_some_and(|s| s.eq_ignore_ascii_case("none"))
                || (!ctx.interactive
                    && raw_tid.is_none_or(|s| s.is_empty())
                    && ctx.active_thread_id.is_none());
            let mut created_fresh_thread: Option<String> = None;
            let task_thread_id = if skip_thread {
                None
            } else {
                let candidate = raw_tid
                    .filter(|s| !s.is_empty())
                    .or(ctx.active_thread_id.as_deref());
                match resolve_or_create_thread(
                    hub,
                    user_id,
                    candidate,
                    Some(topic_title),
                    &title,
                    call.ledger_id.as_deref(),
                )
                .await?
                {
                    Some(tid) => {
                        if candidate.is_some_and(|c| c.eq_ignore_ascii_case("new")) {
                            created_fresh_thread = Some(tid.clone());
                        }
                        Some(tid)
                    }
                    None => {
                        let tid =
                            create_thread(hub, user_id, topic_title, call.ledger_id.as_deref())
                                .await?;
                        created_fresh_thread = Some(tid.clone());
                        Some(tid)
                    }
                }
            };
            let role_name = role_opt.as_ref().map(|r| r.name.as_str()).unwrap_or("");
            let on_complete = if ctx.callback_depth < super::MAX_CALLBACK_DEPTH {
                call.on_complete.as_deref().unwrap_or("").trim()
            } else {
                ""
            };
            match dispatch_to_node(
                hub,
                user_id,
                preferred_node.as_deref(),
                prefer_cloud,
                requested_agent.as_deref(),
                role_name,
                &requested_model,
                &requested_effort,
                &requested_sandbox,
                &title,
                &instr,
                task_thread_id.as_deref(),
                on_complete,
                ctx.callback_depth,
            )
            .await?
            {
                Ok(info) => {
                    if let Some(ref tid) = task_thread_id {
                        ctx.active_thread_id = Some(tid.clone());
                    }
                    let mut meta_parts: Vec<&str> = Vec::new();
                    if let Some(k) = info["node_kind"].as_str().filter(|s| !s.is_empty()) {
                        meta_parts.push(k);
                    }
                    if let Some(r) = info["role"].as_str().filter(|s| !s.is_empty()) {
                        meta_parts.push(r);
                    }
                    if let Some(a) = info["agent"].as_str().filter(|s| !s.is_empty()) {
                        meta_parts.push(a);
                    }
                    if let Some(m) = info["model"].as_str().filter(|s| !s.is_empty()) {
                        meta_parts.push(m);
                    }
                    let meta_str = meta_parts.join(" / ");
                    let observation = format!(
                        "已派发任务 {} 到节点 {}（{}，{}），节点完成后结果会自动发回聊天。",
                        info["task_id"].as_str().unwrap_or(""),
                        info["node_name"].as_str().unwrap_or(""),
                        meta_str,
                        if info["online"].as_bool().unwrap_or(false) {
                            "在线"
                        } else {
                            "离线，上线后执行"
                        }
                    );
                    ctx.dispatched_tasks.push(info);
                    Ok(ToolOutcome::done(tool, observation))
                }
                Err(reason) => {
                    if let Some(ref dead_tid) = created_fresh_thread {
                        let _ = delete_empty_thread(hub, user_id, dead_tid).await;
                    }
                    Ok(ToolOutcome::failed(
                        tool,
                        format!("派发失败：{reason}。"),
                        format!("（{reason}。）"),
                    ))
                }
            }
        }
        "delegate_colleague" => {
            ctx.stage("正在发起同事协作…");
            let Some(peer) = call
                .peer
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            else {
                return Ok(ToolOutcome::failed(
                    tool,
                    "缺少 peer 参数。",
                    "（协作请求缺少同事名称。）",
                ));
            };
            let instr = call
                .instruction
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or(ctx.message);
            match collab::delegate_to_peer(hub, user_id, peer, instr).await {
                Ok((peer_name, task_id, mode)) => {
                    ctx.dispatched_tasks.push(json!({
                        "task_id": task_id,
                        "peer": peer_name,
                        "mode": mode,
                    }));
                    Ok(ToolOutcome::done(
                        tool,
                        format!("已向 {peer_name} 发起协作任务 {task_id}（模式：{mode}）。"),
                    ))
                }
                Err(_) => Ok(ToolOutcome::failed(
                    tool,
                    format!("未找到名为 `{peer}` 的活跃同事。"),
                    format!("（未找到名为 `{peer}` 的活跃同事。）"),
                )),
            }
        }
        other => Ok(ToolOutcome::failed(
            other,
            format!("未知工具 `{other}`，请只使用系统提示中列出的工具。"),
            String::new(),
        )),
    }
}

async fn apply_update_memory(hub: &Hub, user_id: &str, call: &ToolCall) -> Result<()> {
    let uid = user_id.to_owned();
    let name_opt = call.name.clone();
    let avatar_opt = call.avatar_id.clone();
    let soul_opt = call.soul_md.clone();
    let mem_rep = call.memory_replace.clone();
    let mem_app = call.memory_append.clone();
    hub.db
        .write(move |conn| {
            let mut p = read_profile_sync(conn, &uid)?;
            if let Some(n) = name_opt.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                p.name = truncate_bytes(n, 48);
            }
            if let Some(a) = avatar_opt.as_deref().and_then(normalize_avatar_id) {
                p.avatar_id = a.to_owned();
            }
            if let Some(s) = soul_opt.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                p.soul_md = truncate_bytes(s, 6000);
            }
            if let Some(r) = mem_rep.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                p.memory_md = truncate_bytes(r, 8000);
            } else if let Some(a) = mem_app.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                let line = if a.starts_with('-') {
                    a.to_owned()
                } else {
                    format!("- {a}")
                };
                if p.memory_md.trim() == super::prompt::DEFAULT_MEMORY_MD
                    || p.memory_md.contains("在这里记录长期偏好")
                {
                    p.memory_md = truncate_bytes(&line, 8000);
                } else if !p.memory_md.contains(&line) {
                    p.memory_md =
                        truncate_bytes(&format!("{}\n{line}", p.memory_md.trim_end()), 8000);
                }
            }
            p.updated_at = now_unix();
            conn.execute(
                "INSERT INTO agent_profiles(user_id, name, soul_md, memory_md, avatar_id, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(user_id) DO UPDATE SET
                   name=excluded.name, soul_md=excluded.soul_md,
                   memory_md=excluded.memory_md, avatar_id=excluded.avatar_id,
                   updated_at=excluded.updated_at",
                params![uid, p.name, p.soul_md, p.memory_md, p.avatar_id, p.updated_at],
            )?;
            record_audit(conn, &uid, "update_memory", "ok", "Updated SOUL/MEMORY", "")?;
            Ok(())
        })
        .await?;
    hub.events
        .emit(user_id.to_owned(), "brain", json!({"updated": "profile"}));
    Ok(())
}

async fn apply_update_ledger(hub: &Hub, user_id: &str, call: &ToolCall) -> Result<()> {
    let uid = user_id.to_owned();
    let act = call.action.as_deref().unwrap_or("upsert").trim().to_owned();
    let lid = call.ledger_id.clone();
    let title = call.title.clone();
    let status_in = call.status.clone();
    let rep = call.notes_replace.clone();
    let app = call.notes_append.clone();
    hub.db
        .write(move |conn| {
            let now = now_unix();
            if act == "delete" {
                if let Some(id) = lid.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                    conn.execute("DELETE FROM ledgers WHERE id=?1 AND user_id=?2", params![id, uid])?;
                } else if let Some(t) = title.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                    conn.execute("DELETE FROM ledgers WHERE title=?1 AND user_id=?2", params![t, uid])?;
                }
                return Ok(());
            }
            let next_status = match act.as_str() {
                "mute" => Some("muted"),
                "done" => Some("done"),
                _ => status_in
                    .as_deref()
                    .filter(|s| matches!(*s, "active" | "muted" | "done")),
            };
            let existing: Option<Ledger> = if let Some(id) =
                lid.as_deref().map(str::trim).filter(|s| !s.is_empty())
            {
                conn.query_row(
                    "SELECT id, title, status, notes, updated_at FROM ledgers WHERE id=?1 AND user_id=?2",
                    params![id, uid],
                    |r| Ok(Ledger { id: r.get(0)?, title: r.get(1)?, status: r.get(2)?, notes: r.get(3)?, updated_at: r.get(4)? }),
                )
                .optional()?
            } else if let Some(t) = title.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                conn.query_row(
                    "SELECT id, title, status, notes, updated_at FROM ledgers WHERE lower(title)=lower(?1) AND user_id=?2 LIMIT 1",
                    params![t, uid],
                    |r| Ok(Ledger { id: r.get(0)?, title: r.get(1)?, status: r.get(2)?, notes: r.get(3)?, updated_at: r.get(4)? }),
                )
                .optional()?
            } else {
                None
            };

            if let Some(mut cur) = existing {
                if let Some(t) = title.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                    cur.title = truncate_bytes(t, 96);
                }
                if let Some(st) = next_status {
                    cur.status = st.to_owned();
                }
                if let Some(r) = rep.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                    cur.notes = truncate_bytes(r, 6000);
                } else if let Some(a) = app.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                    let line = if a.starts_with('-') { a.to_owned() } else { format!("- {a}") };
                    if !cur.notes.contains(&line) {
                        cur.notes = truncate_bytes(
                            &if cur.notes.trim().is_empty() {
                                line
                            } else {
                                format!("{}\n{line}", cur.notes.trim_end())
                            },
                            6000,
                        );
                    }
                }
                conn.execute(
                    "UPDATE ledgers SET title=?3, status=?4, notes=?5, updated_at=?6 WHERE id=?1 AND user_id=?2",
                    params![cur.id, uid, cur.title, cur.status, cur.notes, now],
                )?;
            } else if let Some(t) = title.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                let id = format!("ldg_{}", &Uuid::new_v4().simple().to_string()[..8]);
                let notes = rep
                    .as_deref()
                    .or(app.as_deref())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(|s| truncate_bytes(s, 6000))
                    .unwrap_or_default();
                conn.execute(
                    "INSERT INTO ledgers(id, user_id, title, status, notes, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![id, uid, truncate_bytes(t, 96), next_status.unwrap_or("active"), notes, now],
                )?;
            }
            record_audit(conn, &uid, "update_ledger", "ok", title.as_deref().unwrap_or("ledger"), "")?;
            Ok(())
        })
        .await?;
    hub.events
        .emit(user_id.to_owned(), "brain", json!({"updated": "ledgers"}));
    Ok(())
}

async fn apply_manage_cron(hub: &Hub, user_id: &str, call: &ToolCall) -> Result<()> {
    let uid = user_id.to_owned();
    let act = call.action.as_deref().unwrap_or("add").trim().to_owned();
    let cid = call.cron_id.clone();
    let daily = call
        .daily_time_cst
        .as_deref()
        .and_then(automation::parse_hhmm);
    let interval = if daily.is_some() {
        1440
    } else {
        call.interval_minutes.unwrap_or(60).clamp(1, 10_080)
    };
    let instr = call.instruction.clone();
    let target = call.target.clone().unwrap_or_else(|| "cloud".into());

    hub.db
        .write(move |conn| {
            if act == "delete" {
                if let Some(id) = cid.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                    conn.execute("DELETE FROM cron_jobs WHERE id=?1 AND user_id=?2", params![id, uid])?;
                }
            } else if let Some(text) = instr.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                let id = format!("crn_{}", &Uuid::new_v4().simple().to_string()[..8]);
                conn.execute(
                    "INSERT INTO cron_jobs(id, user_id, interval_minutes, daily_time_cst, instruction, target, enabled, created_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1, ?7)",
                    params![id, uid, interval, daily, truncate_bytes(text, 1000), truncate_bytes(&target, 48), now_unix()],
                )?;
            }
            Ok(())
        })
        .await?;
    hub.scheduler_notify.notify_one();
    hub.events.emit(
        user_id.to_owned(),
        "automation",
        json!({"updated": "crons"}),
    );
    Ok(())
}

async fn apply_manage_rule(hub: &Hub, user_id: &str, call: &ToolCall) -> Result<()> {
    let uid = user_id.to_owned();
    let act = call.action.as_deref().unwrap_or("add").trim().to_owned();
    let rid = call.rule_id.clone();
    let kw = call.keyword.clone();
    let target = call.node_id.clone().unwrap_or_else(|| "cloud".into());
    hub.db
        .write(move |conn| {
            if act == "delete" {
                if let Some(id) = rid.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                    conn.execute("DELETE FROM automation_rules WHERE id=?1 AND user_id=?2", params![id, uid])?;
                }
            } else if let Some(k) = kw.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                let id = format!("rul_{}", &Uuid::new_v4().simple().to_string()[..8]);
                conn.execute(
                    "INSERT INTO automation_rules(id, user_id, keyword, target_node_id, require_human, created_at)
                     VALUES (?1, ?2, ?3, ?4, 0, ?5)",
                    params![id, uid, truncate_bytes(k, 64), truncate_bytes(&target, 48), now_unix()],
                )?;
            }
            Ok(())
        })
        .await?;
    hub.events.emit(
        user_id.to_owned(),
        "automation",
        json!({"updated": "rules"}),
    );
    Ok(())
}

async fn apply_manage_role(
    hub: &Hub,
    user_id: &str,
    node_list: &[nodes::NodeView],
    call: &ToolCall,
) -> Result<ToolOutcome> {
    let tool = "manage_role";
    let raw_name = call
        .name
        .as_deref()
        .or(call.role.as_deref())
        .unwrap_or("")
        .trim();
    let Some(norm_name) = roles::normalize_role_name(raw_name) else {
        return Ok(ToolOutcome::failed(
            tool,
            "缺少或非法的角色名称（需为 1..32 个字母、数字、下划线或连字符，不能以 `-` 开头）。",
            "（角色名称无效。）",
        ));
    };

    let act = call.action.as_deref().unwrap_or("upsert").trim();
    if act == "delete" {
        let uid = user_id.to_owned();
        let name_db = norm_name.clone();
        let deleted = hub
            .db
            .write(move |conn| roles::delete_role_sync(conn, &uid, &name_db))
            .await?;
        if !deleted {
            return Ok(ToolOutcome::failed(
                tool,
                format!("未找到名为 `{norm_name}` 的军团角色。"),
                format!("（未找到军团角色 `{norm_name}`。）"),
            ));
        }
        hub.events
            .emit(user_id.to_owned(), "brain", json!({"updated": "roles"}));
        return Ok(ToolOutcome::done(
            tool,
            format!("已删除军团角色 `{norm_name}`。"),
        ));
    }

    let next_desc = call
        .description
        .as_deref()
        .map(|s| truncate_bytes(s.trim(), 200));
    let next_agent = match call.agent.as_deref() {
        Some(raw) if raw.trim().is_empty() => Some(String::new()),
        Some(raw) => match nodes::normalize_agent_name(raw) {
            Some(norm) => Some(norm.to_owned()),
            None => {
                return Ok(ToolOutcome::failed(
                    tool,
                    format!(
                        "不支持的执行引擎 `{raw}`，可选引擎为：{}。",
                        nodes::CONFIGURABLE_AGENTS.join(", ")
                    ),
                    format!("（不支持的执行引擎 `{raw}`。）"),
                ));
            }
        },
        None => None,
    };
    let next_model = match call.model.as_deref() {
        Some(raw) => match nodes::normalize_model_name(raw) {
            Ok(m) => Some(m),
            Err(err) => {
                return Ok(ToolOutcome::failed(
                    tool,
                    format!("模型名称无效：{}。", err.1),
                    "（模型名称无效。）",
                ));
            }
        },
        None => None,
    };
    let next_effort = match call.effort.as_deref() {
        Some(raw) => match nodes::normalize_effort(raw) {
            Ok(e) => Some(e),
            Err(_) => {
                return Ok(ToolOutcome::failed(
                    tool,
                    format!(
                        "不支持的推理强度 `{raw}`，可选值：{}。",
                        nodes::VALID_EFFORTS.join(", ")
                    ),
                    format!("（不支持的推理强度 `{raw}`。）"),
                ));
            }
        },
        None => None,
    };
    let next_sandbox = match call.sandbox.as_deref() {
        Some(raw) => match nodes::normalize_job_sandbox(raw) {
            Ok(s) => Some(s),
            Err(_) => {
                return Ok(ToolOutcome::failed(
                    tool,
                    "沙箱级别仅支持 `readonly`、`workspace-write` 或空（默认），不允许通过对话提权到 `unsafe-full`。",
                    "（沙箱级别仅支持 readonly 或 workspace-write。）",
                ));
            }
        },
        None => None,
    };
    let next_node_id = match call.node_id.as_deref() {
        Some(raw) if raw.trim().is_empty() => Some(String::new()),
        Some(raw) => {
            let needle = raw.trim();
            match node_list
                .iter()
                .find(|n| n.id.eq_ignore_ascii_case(needle) || n.name.eq_ignore_ascii_case(needle))
            {
                Some(n) => Some(n.id.clone()),
                None => {
                    return Ok(ToolOutcome::failed(
                        tool,
                        format!(
                            "未找到名为 `{needle}` 的已注册节点；若不限定主机请将 node_id 留空。"
                        ),
                        format!("（未找到节点 `{needle}`。）"),
                    ));
                }
            }
        }
        None => None,
    };
    let next_prompt = call
        .prompt
        .as_deref()
        .map(|s| truncate_bytes(s.trim(), 2000));

    let uid = user_id.to_owned();
    let name_db = norm_name.clone();
    let saved = hub
        .db
        .write(move |conn| {
            let cur = roles::get_role_sync(conn, &uid, &name_db)?;
            let mut merged = cur.unwrap_or_else(|| AgentRole {
                name: name_db.clone(),
                description: String::new(),
                agent: String::new(),
                model: String::new(),
                effort: String::new(),
                sandbox: String::new(),
                node_id: String::new(),
                prompt: String::new(),
                updated_at: 0,
            });
            if let Some(d) = next_desc {
                merged.description = d;
            }
            if let Some(a) = next_agent {
                merged.agent = a;
            }
            if let Some(m) = next_model {
                merged.model = m;
            }
            if let Some(e) = next_effort {
                merged.effort = e;
            }
            if let Some(s) = next_sandbox {
                merged.sandbox = s;
            }
            if let Some(n) = next_node_id {
                merged.node_id = n;
            }
            if let Some(p) = next_prompt {
                merged.prompt = p;
            }
            roles::upsert_role_sync(conn, &uid, &merged)?;
            Ok(merged)
        })
        .await?;

    hub.events
        .emit(user_id.to_owned(), "brain", json!({"updated": "roles"}));

    let mut summary = vec![format!(
        "引擎={}",
        if saved.agent.is_empty() {
            "节点默认"
        } else {
            &saved.agent
        }
    )];
    if !saved.model.is_empty() {
        summary.push(format!("模型={}", saved.model));
    }
    if !saved.effort.is_empty() {
        summary.push(format!("推理={}", saved.effort));
    }
    if !saved.sandbox.is_empty() {
        summary.push(format!("沙箱={}", saved.sandbox));
    }
    if !saved.node_id.is_empty() {
        summary.push(format!("节点={}", saved.node_id));
    }
    let hint = if !saved.agent.is_empty()
        && !node_list
            .iter()
            .any(|n| nodes::node_supports_agent(n, &saved.agent))
    {
        format!(
            "（提示：当前没有节点启用 `{}` 引擎，请确认对应节点已配置并启用该引擎。）",
            saved.agent
        )
    } else {
        String::new()
    };
    Ok(ToolOutcome::done(
        tool,
        format!(
            "已保存军团角色 `{}`（{}）。{hint}",
            saved.name,
            summary.join(" / ")
        ),
    ))
}

async fn apply_manage_node(
    hub: &Hub,
    user_id: &str,
    node_list: &[nodes::NodeView],
    call: &ToolCall,
) -> Result<ToolOutcome> {
    let tool = "manage_node";
    let target = if let Some(raw) = call
        .node_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        match node_list
            .iter()
            .find(|n| n.id.eq_ignore_ascii_case(raw) || n.name.eq_ignore_ascii_case(raw))
        {
            Some(n) => n,
            None => {
                let known = if node_list.is_empty() {
                    "暂无已注册节点".to_owned()
                } else {
                    node_list
                        .iter()
                        .map(|n| format!("{}（{}）", n.id, n.name))
                        .collect::<Vec<_>>()
                        .join("、")
                };
                return Ok(ToolOutcome::failed(
                    tool,
                    format!("未找到节点 `{raw}`（当前节点：{known}）。"),
                    format!("（未找到节点 `{raw}`。）"),
                ));
            }
        }
    } else if node_list.len() == 1 {
        &node_list[0]
    } else {
        let online: Vec<&nodes::NodeView> = node_list.iter().filter(|n| n.online).collect();
        if online.len() == 1 {
            online[0]
        } else {
            let known = if node_list.is_empty() {
                "暂无已注册节点".to_owned()
            } else {
                node_list
                    .iter()
                    .map(|n| format!("{}（{}）", n.id, n.name))
                    .collect::<Vec<_>>()
                    .join("、")
            };
            return Ok(ToolOutcome::failed(
                tool,
                format!("无法唯一确定目标节点，请指定 node_id（当前节点：{known}）。"),
                "（请指定需要修改的节点。）",
            ));
        }
    };

    let next_default = call.default_agent.as_deref().or(call.agent.as_deref());
    let next_allowed = call.allowed_agents.as_deref();
    if next_default.is_none() && next_allowed.is_none() {
        return Ok(ToolOutcome::failed(
            tool,
            "请至少提供 `default_agent` 或 `allowed_agents` 之一。",
            "（缺少要更新的节点引擎配置。）",
        ));
    }

    match nodes::update_node_config(hub, user_id, &target.id, None, next_default, next_allowed)
        .await
    {
        Ok(updated) => {
            let def_label = if updated.effective_default_agent.is_empty() {
                "auto"
            } else {
                updated.effective_default_agent.as_str()
            };
            let allowed_label = if updated.effective_agents.is_empty() {
                "默认".to_owned()
            } else {
                updated.effective_agents.join(",")
            };
            Ok(ToolOutcome::done(
                tool,
                format!(
                    "已更新节点 `{}`（{}）：默认引擎={}，可用引擎={}。",
                    updated.id, updated.name, def_label, allowed_label
                ),
            ))
        }
        Err(err) => Ok(ToolOutcome::failed(
            tool,
            format!("更新节点配置失败：{}。", err.1),
            format!("（更新节点配置失败：{}。）", err.1),
        )),
    }
}

#[allow(clippy::too_many_arguments)]
async fn dispatch_to_node(
    hub: &Hub,
    user_id: &str,
    preferred_node: Option<&str>,
    prefer_cloud: bool,
    requested_agent: Option<&str>,
    role_name: &str,
    model: &str,
    effort: &str,
    sandbox: &str,
    title: &str,
    instruction: &str,
    thread_id: Option<&str>,
    on_complete: &str,
    callback_depth: i64,
) -> Result<std::result::Result<Value, String>> {
    let uid = user_id.to_owned();
    let pref = preferred_node.map(str::to_owned);
    let req_ag = requested_agent.map(str::to_owned);
    let role_db = role_name.to_owned();
    let model_db = model.to_owned();
    let effort_db = effort.to_owned();
    let sandbox_db = sandbox.to_owned();
    let t = title.to_owned();
    let instr = instruction.to_owned();
    let thr = thread_id.map(str::to_owned);
    let cb = on_complete.to_owned();
    let route_hint = format!("{t} {instr}");
    let res = hub
        .db
        .write(move |conn| {
            let Some(node) = nodes::pick_node_for_task_with_agent_sync(
                conn,
                &uid,
                pref.as_deref(),
                prefer_cloud,
                Some(&route_hint),
                req_ag.as_deref(),
            )?
            else {
                let reason = match req_ag.as_deref() {
                    Some(ag) => format!("当前没有启用 `{ag}` 引擎的执行节点"),
                    None => "当前没有已注册的 PC 或云端执行节点".to_owned(),
                };
                return Ok(Err(reason));
            };
            if let Some(ag) = req_ag.as_deref() {
                if !nodes::node_supports_agent(&node, ag) {
                    let enabled = if node.effective_agents.is_empty() {
                        "仅默认引擎".to_owned()
                    } else {
                        node.effective_agents.join(", ")
                    };
                    return Ok(Err(format!(
                        "节点 {} 当前启用引擎为 [{}]，不支持 `{ag}`",
                        node.name, enabled
                    )));
                }
            }
            let job_agent = req_ag
                .as_deref()
                .unwrap_or(node.default_agent.as_str())
                .to_owned();
            let effective_agent = if !job_agent.is_empty() {
                job_agent.clone()
            } else {
                node.effective_default_agent.clone()
            };
            let (task_id, _) = nodes::create_task_and_enqueue_with_options_sync(
                conn,
                &uid,
                &uid,
                &node.id,
                &t,
                &instr,
                "manual",
                nodes::EnqueueOptions {
                    thread_id: thr.as_deref(),
                    on_complete: &cb,
                    callback_depth,
                    agent: &job_agent,
                    role: &role_db,
                    model: &model_db,
                    effort: &effort_db,
                    sandbox: &sandbox_db,
                },
            )?;
            if let Some(ref tid) = thr {
                conn.execute(
                    "UPDATE threads SET node_id = COALESCE(node_id, ?3), updated_at = ?4
                     WHERE id = ?1 AND user_id = ?2",
                    params![tid, uid, node.id, now_unix()],
                )?;
            }
            record_audit(
                conn,
                &uid,
                "dispatch_node",
                "queued",
                &format!("{} -> {}", node.name, t),
                &truncate_bytes(&instr, 200),
            )?;
            Ok(Ok(json!({
                "task_id": task_id,
                "thread_id": thr,
                "node_id": node.id,
                "node_name": node.name,
                "node_kind": node.kind,
                "agent": effective_agent,
                "role": role_db,
                "model": model_db,
                "effort": effort_db,
                "sandbox": sandbox_db,
                "online": node.online,
            })))
        })
        .await?;
    if let Ok(ref info) = res {
        if let Some(nid) = info["node_id"].as_str() {
            hub.node_waiters.notify(user_id, nid);
        }
        hub.events.emit(user_id.to_owned(), "task", info.clone());
    }
    Ok(res)
}

async fn apply_manage_thread(ctx: &mut TurnContext<'_>, call: &ToolCall) -> Result<ToolOutcome> {
    let tool = "manage_thread";
    let hub = ctx.hub;
    let user_id = ctx.user_id;
    let act = call.action.as_deref().unwrap_or("bind").trim();
    let target_tid = call
        .thread_id
        .as_deref()
        .or(ctx.active_thread_id.as_deref())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let title = call
        .title
        .as_deref()
        .or(call.thread_title.as_deref())
        .map(str::trim)
        .filter(|s| !s.is_empty());

    match act {
        "create" => {
            let topic =
                title.unwrap_or_else(|| ctx.message.lines().next().unwrap_or("新话题").trim());
            let tid = create_thread(hub, user_id, topic, call.ledger_id.as_deref()).await?;
            ctx.active_thread_id = Some(tid.clone());
            hub.events
                .emit(user_id.to_owned(), "brain", json!({"updated": "threads"}));
            Ok(ToolOutcome::done(
                tool,
                format!("已新建工作线程 {tid}（{topic}）。"),
            ))
        }
        "bind" => {
            let Some(ref tid) = target_tid else {
                return Ok(ToolOutcome::failed(
                    tool,
                    "缺少要绑定的 thread_id。",
                    "（缺少 thread_id。）",
                ));
            };
            let Some(found) = resolve_or_create_thread(
                hub,
                user_id,
                Some(tid),
                title,
                title.unwrap_or("新话题"),
                call.ledger_id.as_deref(),
            )
            .await?
            else {
                return Ok(ToolOutcome::failed(
                    tool,
                    format!("未找到工作线程 `{tid}`。"),
                    format!("（未找到工作线程 `{tid}`。）"),
                ));
            };
            ctx.active_thread_id = Some(found.clone());
            hub.events
                .emit(user_id.to_owned(), "brain", json!({"updated": "threads"}));
            Ok(ToolOutcome::done(
                tool,
                format!("已将当前对话绑定到工作线程 {found}。"),
            ))
        }
        "rename" => {
            let Some(ref tid) = target_tid else {
                return Ok(ToolOutcome::failed(
                    tool,
                    "缺少要重命名的 thread_id。",
                    "（缺少 thread_id。）",
                ));
            };
            if title.is_none() && call.ledger_id.is_none() {
                return Ok(ToolOutcome::failed(
                    tool,
                    "rename 需要提供新的 title 或 ledger_id。",
                    "（缺少新标题或账本 ID。）",
                ));
            }
            if !update_thread_meta(hub, user_id, tid, title, call.ledger_id.as_deref()).await? {
                return Ok(ToolOutcome::failed(
                    tool,
                    format!("未找到工作线程 `{tid}`。"),
                    format!("（未找到工作线程 `{tid}`。）"),
                ));
            }
            hub.events
                .emit(user_id.to_owned(), "brain", json!({"updated": "threads"}));
            Ok(ToolOutcome::done(tool, format!("已更新工作线程 {tid}。")))
        }
        "archive" => {
            let Some(ref tid) = target_tid else {
                return Ok(ToolOutcome::failed(
                    tool,
                    "缺少要归档的 thread_id。",
                    "（缺少 thread_id。）",
                ));
            };
            archive_thread(hub, user_id, tid).await?;
            hub.events
                .emit(user_id.to_owned(), "brain", json!({"updated": "threads"}));
            Ok(ToolOutcome::done(tool, format!("已归档线程 {tid}。")))
        }
        other => Ok(ToolOutcome::failed(
            tool,
            format!("不支持的线程操作 `{other}`（可选 bind/create/rename/archive）。"),
            format!("（不支持的线程操作 `{other}`。）"),
        )),
    }
}
