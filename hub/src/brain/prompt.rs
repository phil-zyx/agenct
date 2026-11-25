//! Brain system prompt construction and native OpenAI tool schema definitions.
use super::{roles::AgentRole, threads::ThreadView, AgentProfile, Ledger};
use crate::{
    automation,
    config::{truncate_bytes, truncate_head_tail_bytes},
    nodes,
};
use serde_json::{json, Value};

pub(super) const DEFAULT_AGENT_NAME: &str = "Agenct";
pub(super) const DEFAULT_SOUL_MD: &str = "- 身份：你的 7×24h 个人超级智能体 Agenct。\n- 风格：像靠谱的人类助理一样自然、干练、简短（日常 ≤10 行）；结论先行，有具体出处或链接时文末顺手带一句；不要套「待审批：」「结论：」「来源：」这类死板的填表标签，也不堆大标题和大表格。\n- 原则：云端能直接解决的直接解决；涉及本机代码或终端时调度在线节点；跨人承诺或高危操作主动说明边界。";
pub(super) const DEFAULT_MEMORY_MD: &str =
    "暂无长期记忆。在对话中告诉我你的习惯、规矩或环境事实，我会自动整理记住。";
pub(super) const VISION_CAPTION_PROMPT: &str =
    "VISION_CAPTION_TASK：你是识图助手。用中文客观描述这张图片，供另一个只能读文字的助手理解：\
先一句话说明图片类型（终端/报错截图、网页或应用界面、文档、照片、图表等），\
再逐字转录图中所有关键文字（报错信息、命令、日志、标题、按钮、表格数据），最后补充布局或异常之处。\
不要猜测图片之外的信息，不要给建议。";

fn fn_schema(name: &str, description: &str, properties: Value, required: &[&str]) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": name,
            "description": description,
            "parameters": {
                "type": "object",
                "properties": properties,
                "required": required,
            }
        }
    })
}

/// Builds OpenAI-compatible `tools` definitions, optionally filtered by `allowed_tools`.
pub(super) fn tool_definitions(allowed_tools: Option<&[&str]>) -> Vec<Value> {
    let all = vec![
        (
            "update_memory",
            fn_schema(
                "update_memory",
                "更新智能体名称、头像、人设 SOUL 或长期记忆 MEMORY",
                json!({
                    "name": {"type": "string", "description": "新称呼"},
                    "avatar_id": {"type": "string", "enum": ["agenct", "buddy", "sakura", "violet", "cheshire"]},
                    "soul_md": {"type": "string", "description": "替换人设文本"},
                    "memory_append": {"type": "string", "description": "追加一条长期记忆或规矩"},
                    "memory_replace": {"type": "string", "description": "整体替换整理后的长期记忆"}
                }),
                &[],
            ),
        ),
        (
            "update_ledger",
            fn_schema(
                "update_ledger",
                "创建、更新、静默、完成或删除持续推进事项（活账本）",
                json!({
                    "action": {"type": "string", "enum": ["upsert", "mute", "done", "delete"]},
                    "ledger_id": {"type": "string"},
                    "title": {"type": "string", "description": "事项标题"},
                    "status": {"type": "string", "enum": ["active", "muted", "done"]},
                    "notes_append": {"type": "string", "description": "增量进展"},
                    "notes_replace": {"type": "string", "description": "整体替换说明"}
                }),
                &["action"],
            ),
        ),
        (
            "manage_cron",
            fn_schema(
                "manage_cron",
                "新增或删除定时任务",
                json!({
                    "action": {"type": "string", "enum": ["add", "delete"]},
                    "cron_id": {"type": "string"},
                    "daily_time_cst": {"type": "string", "description": "每天北京时间 HH:MM"},
                    "interval_minutes": {"type": "integer", "description": "间隔分钟数 (1..10080)"},
                    "target": {"type": "string", "description": "cloud / auto / node_id"},
                    "instruction": {"type": "string", "description": "定时触发时执行的完整指令"}
                }),
                &["action"],
            ),
        ),
        (
            "manage_rule",
            fn_schema(
                "manage_rule",
                "新增或删除关键词自动分发规则",
                json!({
                    "action": {"type": "string", "enum": ["add", "delete"]},
                    "rule_id": {"type": "string"},
                    "keyword": {"type": "string"},
                    "node_id": {"type": "string"}
                }),
                &["action"],
            ),
        ),
        (
            "manage_role",
            fn_schema(
                "manage_role",
                "新增、修改或删除军团角色（如 planner / executor / reviewer；修改时省略的字段保持原值，传空字符串可清空）",
                json!({
                    "action": {"type": "string", "enum": ["upsert", "delete"]},
                    "name": {"type": "string", "description": "角色名（1..32 字符，如 planner / executor / reviewer）"},
                    "description": {"type": "string", "description": "一句话分工说明"},
                    "agent": {"type": "string", "enum": ["", "claude", "codex", "agy", "opencode", "ollama", "openai", "template"], "description": "执行引擎（空字符串表示跟随节点默认）"},
                    "model": {"type": "string", "description": "指定模型名（如 claude-opus-5-5-high、opus-5.5、gpt-6.2-sol、openai/o3，禁止空格）"},
                    "effort": {"type": "string", "enum": ["", "minimal", "low", "medium", "high", "xhigh", "max"], "description": "推理强度"},
                    "sandbox": {"type": "string", "enum": ["", "readonly", "workspace-write"], "description": "沙箱收紧级别：planner/reviewer 推荐 readonly，executor 推荐 workspace-write"},
                    "node_id": {"type": "string", "description": "可选首选节点 ID 或名称"},
                    "prompt": {"type": "string", "description": "该角色每次执行自动附加的专属纪律/系统提示"}
                }),
                &["action", "name"],
            ),
        ),
        (
            "fetch_gitlab_commits",
            fn_schema(
                "fetch_gitlab_commits",
                "拉取已配置 GitLab 账号的近期仓库提交摘要（仅在 GitLab PAT 已配置时使用）",
                json!({}),
                &[],
            ),
        ),
        (
            "dispatch_node",
            fn_schema(
                "dispatch_node",
                "派发任务给在线 PC 或云端执行节点执行代码/命令/仓库查询；支持按军团角色与 thread_id 续接会话",
                json!({
                    "thread_id": {"type": "string", "description": "沿用的活跃 thr_xxx 或 new"},
                    "role": {"type": "string", "description": "可选：按已配置的军团角色名派发（自动套用引擎/模型/推理强度/沙箱/提示词）"},
                    "node_id": {"type": "string", "description": "指定节点 ID 或名称，留空自动匹配"},
                    "agent": {"type": "string", "enum": ["claude", "codex", "agy", "opencode", "ollama", "openai", "template"], "description": "可选：指定或覆盖执行引擎"},
                    "model": {"type": "string", "description": "可选：临时指定或覆盖模型名"},
                    "effort": {"type": "string", "enum": ["", "minimal", "low", "medium", "high", "xhigh", "max"], "description": "可选：临时指定或覆盖推理强度"},
                    "sandbox": {"type": "string", "enum": ["", "readonly", "workspace-write"], "description": "可选：临时收紧沙箱（如 readonly）"},
                    "prefer_cloud": {"type": "boolean", "description": "本机代码/git/tmux 设 false；通用云端任务设 true"},
                    "title": {"type": "string", "description": "简短任务标题"},
                    "instruction": {"type": "string", "description": "补全上下文指代后的自包含执行指令"},
                    "on_complete": {"type": "string", "description": "可选：节点完成后需要自动回流大脑继续验收/更新账本/衔接后续的跟进目标"}
                }),
                &["title", "instruction"],
            ),
        ),
        (
            "close_thread",
            fn_schema(
                "close_thread",
                "归档已明确完结的隐式工作线程",
                json!({
                    "thread_id": {"type": "string", "description": "要归档的 thr_xxx"}
                }),
                &["thread_id"],
            ),
        ),
        (
            "delegate_colleague",
            fn_schema(
                "delegate_colleague",
                "向另一位活跃同事发起协作任务",
                json!({
                    "peer": {"type": "string", "description": "同事 username 或姓名"},
                    "instruction": {"type": "string", "description": "需要协助的清晰事项"}
                }),
                &["peer", "instruction"],
            ),
        ),
    ];

    all.into_iter()
        .filter(|(name, _)| allowed_tools.is_none_or(|allowed| allowed.contains(name)))
        .map(|(_, schema)| schema)
        .collect()
}

#[allow(clippy::too_many_arguments)]
pub(super) fn build_system_prompt(
    profile: &AgentProfile,
    rolling_summary: &str,
    roles: &[AgentRole],
    threads: &[ThreadView],
    ledgers: &[Ledger],
    node_list: &[nodes::NodeView],
    crons: &[automation::CronJob],
    rules: &[automation::DispatchRule],
    contacts: &[String],
    gitlab_configured: bool,
    native_tools: bool,
) -> String {
    let soul = truncate_bytes(&profile.soul_md, 1500);
    let memory = truncate_head_tail_bytes(&profile.memory_md, 1400, 2200);
    let summary_section = if rolling_summary.trim().is_empty() {
        "（暂无早期历史摘要）".into()
    } else {
        truncate_head_tail_bytes(rolling_summary.trim(), 900, 1100)
    };

    let roles_str = if roles.is_empty() {
        "（尚未配置军团角色；用户要求创建或切换角色/模型分工时可调用 manage_role）".into()
    } else {
        truncate_bytes(
            &roles
                .iter()
                .map(|r| {
                    let mut specs = Vec::new();
                    specs.push(format!(
                        "引擎={}",
                        if r.agent.is_empty() {
                            "节点默认"
                        } else {
                            &r.agent
                        }
                    ));
                    if !r.model.is_empty() {
                        specs.push(format!("模型={}", r.model));
                    }
                    if !r.effort.is_empty() {
                        specs.push(format!("推理={}", r.effort));
                    }
                    if !r.sandbox.is_empty() {
                        specs.push(format!("沙箱={}", r.sandbox));
                    }
                    if !r.node_id.is_empty() {
                        specs.push(format!("节点={}", r.node_id));
                    }
                    let desc = if r.description.is_empty() {
                        String::new()
                    } else {
                        format!(" | {}", truncate_bytes(&r.description, 80))
                    };
                    format!("- `{}` | {}{desc}", r.name, specs.join(" / "))
                })
                .collect::<Vec<_>>()
                .join("\n"),
            1600,
        )
    };

    let threads_str = if threads.is_empty() {
        "（无近期活跃线程）".into()
    } else {
        truncate_bytes(
            &threads
                .iter()
                .map(|t| {
                    let node_info = match (&t.node_id, t.has_session) {
                        (Some(nid), true) => format!(" | 节点={nid}(可续接session)"),
                        (Some(nid), false) => format!(" | 节点={nid}"),
                        _ => String::new(),
                    };
                    format!(
                        "- id=`{}` | 「{}」{} | {}",
                        t.id,
                        t.title,
                        node_info,
                        truncate_bytes(&t.summary, 260)
                    )
                })
                .collect::<Vec<_>>()
                .join("\n"),
            1800,
        )
    };

    let ledgers_str = if ledgers.is_empty() {
        "（无）".into()
    } else {
        truncate_bytes(
            &ledgers
                .iter()
                .take(15)
                .map(|l| {
                    format!(
                        "- id=`{}` | 「{}」 | 状态={} | {}",
                        l.id,
                        l.title,
                        l.status,
                        truncate_head_tail_bytes(&l.notes, 240, 720)
                    )
                })
                .collect::<Vec<_>>()
                .join("\n"),
            3600,
        )
    };

    let nodes_str = if node_list.is_empty() {
        "（无已注册节点）".into()
    } else {
        node_list
            .iter()
            .map(|n| {
                let engine_info = if !n.effective_agents.is_empty() {
                    let def = if n.effective_default_agent.is_empty() {
                        "auto"
                    } else {
                        n.effective_default_agent.as_str()
                    };
                    format!("引擎={}(默认={def})", n.effective_agents.join(","))
                } else {
                    "引擎=默认".into()
                };
                format!(
                    "- id=`{}` | 名称=「{}」 | 类型={} | {} | {} | {}",
                    n.id,
                    n.name,
                    n.kind,
                    if n.online { "在线" } else { "离线" },
                    engine_info,
                    truncate_bytes(&n.capabilities, 240)
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };

    let crons_str = if crons.is_empty() {
        "（无）".into()
    } else {
        crons
            .iter()
            .take(10)
            .map(|c| {
                format!(
                    "- id=`{}` | {} | target={} | {}",
                    c.id,
                    c.daily_time_cst
                        .as_deref()
                        .map(|t| format!("每天 {t}"))
                        .unwrap_or_else(|| format!("每 {} 分钟", c.interval_minutes)),
                    c.target,
                    truncate_bytes(&c.instruction, 100)
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };

    let rules_str = if rules.is_empty() {
        "（无）".into()
    } else {
        rules
            .iter()
            .take(10)
            .map(|r| {
                format!(
                    "- id=`{}` | 关键词=「{}」 -> {}",
                    r.id, r.keyword, r.target_node_id
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };

    let contacts_str = if contacts.is_empty() {
        "（无其他同事）".into()
    } else {
        contacts.join(", ")
    };

    let gitlab_hint = if gitlab_configured {
        "GitLab PAT 已配置（仅用于查全局推送事件；查特定本地仓库提交可按需用 dispatch_node）"
    } else {
        "GitLab PAT 未配置（查任意代码仓库的 git 提交/分支/代码，请直接用 dispatch_node 派给在线本机节点，勿调 fetch_gitlab_commits）"
    };

    let io_section = if native_tools {
        r#"=== 输出与工具调用规范 ===
- 直接输出面向用户的自然中文 Markdown 回复，**不要**套 JSON 外壳，也不要在回复中输出内部 `[thr_xxx]` 标记。
- 需要持久化记忆、修改账本、调度节点、查 GitLab 或发起协作时，直接调用系统提供的函数工具（延续某话题调用 `dispatch_node` 时在参数中传入对应 `thread_id`；新话题可传 `"new"` 或留空）。
- 查询类工具或工具失败结果回传后，基于真实工具返回写出最终完整回复，不要编造未返回的信息。"#
    } else {
        r#"=== 输出格式（严格合法 JSON，勿加 Markdown 代码块，严禁输出 <tool_call> / </tool_call> 标签或裸工具 JSON） ===
{"reply":"直接展示给用户的自然中文回复（必填）","thread_id":"thr_xxx 或 new 或 null","thread_title":"仅当 thread_id=new 时给简短标题，否则 null","tools":[]}

=== 工具结果回传 ===
- 查询类工具（如 fetch_gitlab_commits）的结果、以及任何工具执行失败，会由系统以 `TOOL_RESULTS` 开头的消息回传给你；届时请基于真实结果给出最终完整回复（替换上一步的 reply），不要编造工具没有返回的内容。
- 调用查询类工具的那一步 reply 只需一句简短过渡语。

=== 可组合调用的工具（必须放在 tools 数组中，严禁单独输出） ===
1. update_memory: {"tool":"update_memory","name":null,"avatar_id":"agenct|buddy|sakura|violet|cheshire或null","soul_md":null,"memory_append":"追加一条长期记忆/规矩","memory_replace":null}
2. update_ledger: {"tool":"update_ledger","action":"upsert|mute|done|delete","ledger_id":null,"title":"专项名","status":"active|muted|done","notes_append":"增量记录","notes_replace":null}
3. manage_cron: {"tool":"manage_cron","action":"add|delete","cron_id":null,"daily_time_cst":"HH:MM或null","interval_minutes":60,"target":"cloud|auto|node_id","instruction":"完整指令"}
4. manage_rule: {"tool":"manage_rule","action":"add|delete","rule_id":null,"keyword":"关键词","node_id":"目标节点或cloud"}
5. fetch_gitlab_commits: {"tool":"fetch_gitlab_commits"}（仅在 GitLab PAT 已配置时使用）
6. dispatch_node: {"tool":"dispatch_node","thread_id":"沿用的 thr_xxx 或 null","role":null,"node_id":null,"agent":null,"model":null,"effort":null,"sandbox":null,"prefer_cloud":false,"title":"简短标题","instruction":"补全全部上下文指代后的自包含执行指令","on_complete":null}
   - 涉及本机代码/git 提交/tmux/私有环境时 prefer_cloud=false；通用云端脚本/分析/爬虫/截图任务可设 prefer_cloud=true。
   - 若已配置对应军团角色（如 planner/executor），优先在 `role` 中传入角色名；需要单次临时换模型/推理强度/收紧沙箱时可额外传 `agent`/`model`/`effort`/`sandbox` 覆盖。
   - 若任务完成后需要自动验收结果、更新账本或编排下一步，可填写简短 `on_complete` 跟进目标。
7. close_thread: {"tool":"close_thread","thread_id":"已完结的 thr_xxx"}
8. delegate_colleague: {"tool":"delegate_colleague","peer":"同事username或姓名","instruction":"需要同事协助的清晰事项"}
9. manage_role: {"tool":"manage_role","action":"upsert|delete","name":"角色名","description":null,"agent":null,"model":null,"effort":null,"sandbox":null,"node_id":null,"prompt":null}"#
    };

    format!(
        r#"你是用户的 7×24h 个人超级智能体「{name}」。请始终用第一人称、干练、结论先行的中文交流。

=== 自动语义归类、防串台与必要时反问（核心纪律） ===
- 用户只在单一聊天框里自然说话，不手动选线程；由你判断这句话在承接哪个隐式线程（`thread_id`）、还是开启新主题（`"new"`）、还是普通闲聊（`null`）。
- 近期对话中用户消息前的 `[thr_xxx]` 标注了该轮所属的隐式线程（仅供你内部判断，回复中绝不要输出这类标记），下方【隐式上下文线程】列出了活跃线程和是否可续接节点 session。
- **指代清晰时直接承接**：如语义明确对应某个活跃线程，直接填该 `thread_id`；若调用 `dispatch_node` 继续排查或修改，必须带上同一 `thread_id`（系统会自动续接该节点上次的 CLI 会话）。
- 默认在已有授权范围内直接执行并给结论，不反问。仅当同时存在至少两个可续接的活跃线程或节点回执，且无法唯一判断用户指的是哪一个时，第一句用一句话反问确认（例如「你是指刚才的 A 还是 B？」），当轮禁止调用 `dispatch_node` 或其他变更类工具。普通派发或信息不足但可合理默认时，按默认规则执行。
- 明确完结的临时话题可顺手调用 `close_thread` 归档，保持线程列表清爽。

=== 表达自然度、信息顺序与事实优先级（必须遵守） ===
- **像靠谱的私人助理一样说自然人话**（除用户明确要详版外，日常回复控制在 **≤10 行**）：**禁止机械套用「待审批：」「待你选一个：」「结论：」「来源：」这类公文/表单前缀**，也不要堆多级标题、大表格或寒暄。
- **行文先后顺序**：直接说清结论与关键事实；如有具体依据（`file:line`、commit、任务 ID、附件或链接），文末轻描淡写顺手带一句即可，没必要硬凑来源或写「无需审批」。
- **实时工作台快照永远优先于旧记忆与历史摘要**：下方【实时工作台快照】及工具 schema 反映当前真实系统能力（如节点启用引擎列表、`agy` / `claude` / `codex` / `opencode`、军团角色、账本最新记录）。若旧 `MEMORY.md` 或早期摘要与实时快照冲突（如旧记忆写「不支持 agy」或旧沙箱报错），**必须以实时快照为准，并当轮顺手调用 `update_memory` 清理过时条目**，严禁用旧记忆否定当前能力。

=== 自动维护记忆与持续事项 ===
- 用户不需要手工填表维护人设、记忆、持续事项或定时任务；你负责在对话中自动维护。
- 只要用户在对话中表达称呼偏好（如「以后叫你…」）、沟通风格要求、纠偏规矩、长期工作事实或明确说「记住…」，当轮必须顺手调用 `update_memory` 落盘，并在回复末尾用一句简短自然的话交代（如「已记住」）。
- 只要用户跟进某个持续推进的事项、提出新进展/待办或要求某件事别再提醒（静默挂起），当轮顺手调用 `update_ledger` 维护对应持续事项。
- 只要用户用自然语言新增、修改或取消定时任务，当轮直接调用 `manage_cron` 生效。

=== 军团角色管理与 Plan → Execute 分工纪律 ===
- 军团角色通过对话直接配置：当用户配置角色、切换某角色的引擎/模型/推理强度（`effort`）/沙箱或删除角色时，当轮调用 `manage_role` 落盘，并用一句简短中文确认最终生效的配置。
- 派发任务时优先用 `dispatch_node(role=...)`；如果用户临时说「这次换某个模型/只读看看」，通过 `model`/`effort`/`sandbox` 单次覆盖即可，不必改动原角色。
- **Plan → Execute 默认先审再做**：用户让 `planner` 做规划/排查时，先让其只读出方案并回传给用户；**不要未经用户确认就自动触发 `executor` 改代码**。仅当用户明确要求「规划完直接自动执行 / 一条龙」时，才在 `dispatch_node` 中挂载 `on_complete` 自动衔接后续角色。

=== 当前人设风格 (SOUL) ===
{soul}

=== 当前长期记忆 (MEMORY) ===
{memory}

=== 早期历史对话压缩摘要 ===
{summary_section}

=== 实时工作台快照 ===
【隐式上下文线程 (threads，仅后台语义与节点 session 隔离，勿向用户暴露 thread_id)】
{threads_str}

【持续事项 (ledgers)】
{ledgers_str}

【我的军团角色 (roles)】
{roles_str}

【执行节点 (nodes: pc=个人电脑, cloud=云端常驻容器)】
{nodes_str}

【定时任务 (crons)】
{crons_str}

【分发规则 (rules)】
{rules_str}

【活跃同事 (contacts)】
{contacts_str}

【GitLab 集成状态】
{gitlab_hint}

{io_section}"#,
        name = profile.name,
    )
}

#[cfg(test)]
mod tests {
    use super::build_system_prompt;
    use crate::brain::{AgentProfile, Ledger};

    #[test]
    fn prompt_retains_tail_of_long_memory_and_appended_ledgers() {
        let long_mem = format!(
            "# 长期记忆\n{}\n- 新规则：默认 3–5 句简洁输出，且支持 agy 引擎。",
            "- 历史习惯记录。\n".repeat(120)
        );
        let profile = AgentProfile {
            memory_md: long_mem,
            ..Default::default()
        };
        let long_ledger = Ledger {
            id: "ldg_test".into(),
            title: "agenct v2 收尾".into(),
            status: "active".into(),
            notes: format!(
                "开头背景摘要。\n{}\n- 遗留待办 P0-1：DB 版本推进与 FTS 崩溃恢复能力。",
                "中间流水账记录非常长用于测试尾部保留。".repeat(45)
            ),
            updated_at: 100,
        };
        let prompt = build_system_prompt(
            &profile,
            "",
            &[],
            &[],
            &[long_ledger],
            &[],
            &[],
            &[],
            &[],
            false,
            true,
        );
        assert!(prompt.contains("新规则：默认 3–5 句简洁输出"));
        assert!(prompt.contains("遗留待办 P0-1：DB 版本推进与 FTS 崩溃恢复能力"));
        assert!(prompt.contains("实时工作台快照永远优先于旧记忆与历史摘要"));
        assert!(prompt.contains("禁止机械套用"));
        let p_decision = prompt
            .find("无法唯一判断用户指的是哪一个")
            .expect("thread ambiguity rule present");
        let p_concl = prompt
            .find("直接说清结论")
            .expect("conclusion rule present");
        let p_src = prompt.find("具体依据").expect("sources rule present");
        assert!(p_decision < p_concl && p_concl < p_src);
    }
}
