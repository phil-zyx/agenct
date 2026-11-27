//! Brain system prompt construction and native OpenAI tool schema definitions.
use super::{roles::AgentRole, threads::ThreadView, AgentProfile, Ledger};
use crate::{
    automation,
    config::{truncate_bytes, truncate_head_tail_bytes},
    nodes,
};
use serde_json::{json, Value};

pub(super) const DEFAULT_AGENT_NAME: &str = "Agenct";
pub(super) const DEFAULT_SOUL_MD: &str = "- 身份：你的 7×24h 个人超级智能体 Agenct。\n- 风格：像靠谱的人类助理一样自然、干练、简短（默认 3～5 句、≤6 行以内，除非用户明确要求详细展开）；结论先行，有具体出处或链接时文末顺手带一句；不要套「待审批：」「结论：」「来源：」这类死板的填表标签，也不堆大标题和大表格。\n- 原则：云端能直接解决的直接解决；涉及本机代码或终端时调度在线节点；跨人承诺或高危操作主动说明边界。";
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
                    "agent": {"type": "string", "enum": ["", "claude", "codex", "agy", "opencode"], "description": "执行引擎（空字符串表示跟随节点默认）"},
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
            "manage_node",
            fn_schema(
                "manage_node",
                "修改执行节点（如 homelab）的默认执行引擎或启用引擎白名单并立即持久化",
                json!({
                    "node_id": {"type": "string", "description": "目标节点 ID 或名称（仅有一个节点或唯一在线节点时可省略）"},
                    "default_agent": {"type": "string", "enum": ["", "claude", "codex", "agy", "opencode"], "description": "节点默认执行引擎（空字符串表示跟随节点自动检测）"},
                    "allowed_agents": {"type": "array", "items": {"type": "string"}, "description": "可选：允许使用的执行引擎白名单；传空数组清空自定义限制"}
                }),
                &[],
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
                "派发任务给在线 PC 或云端执行节点执行代码/命令/仓库查询；支持按军团角色与 thread_id（单个具体 Topic 维度）续接会话",
                json!({
                    "thread_id": {"type": "string", "description": "沿用同子话题的活跃 thr_xxx；开启新 Topic 传 new；一次性只读查询或定时检查传 none（不建线程）"},
                    "thread_title": {"type": "string", "description": "仅当 thread_id=new 时可选：该 Topic 的简短主题名（如「移动端输入框修复」），省略则用 title"},
                    "ledger_id": {"type": "string", "description": "可选：关联所属宏观项目账本 ID（ldg_xxx）"},
                    "role": {"type": "string", "description": "可选：按已配置的军团角色名派发（自动套用引擎/模型/推理强度/沙箱/提示词）"},
                    "node_id": {"type": "string", "description": "指定节点 ID 或名称，留空自动匹配"},
                    "agent": {"type": "string", "enum": ["claude", "codex", "agy", "opencode"], "description": "可选：指定或覆盖执行引擎"},
                    "model": {"type": "string", "description": "可选：临时指定或覆盖模型名"},
                    "effort": {"type": "string", "enum": ["", "minimal", "low", "medium", "high", "xhigh", "max"], "description": "可选：临时指定或覆盖推理强度"},
                    "sandbox": {"type": "string", "enum": ["", "readonly", "workspace-write"], "description": "可选：临时收紧沙箱（纯代码/git 只读核查可用 readonly；需改文件、跑构建/测试或 git commit 时留空保持 workspace-write）"},
                    "prefer_cloud": {"type": "boolean", "description": "本机代码/git/tmux 设 false；通用云端任务设 true"},
                    "title": {"type": "string", "description": "本次具体执行任务的简短标题"},
                    "instruction": {"type": "string", "description": "补全上下文指代后的自包含执行指令"},
                    "on_complete": {"type": "string", "description": "可选：节点完成后需要自动回流大脑继续验收/更新账本/衔接后续的跟进目标"}
                }),
                &["title", "instruction"],
            ),
        ),
        (
            "manage_thread",
            fn_schema(
                "manage_thread",
                "在纯讨论、规划或 Topic 切换时主动绑定/新建/重命名/归档工作线程（避免讨论断链或把多个子问题混在同一线程里）",
                json!({
                    "action": {"type": "string", "enum": ["bind", "create", "rename", "archive"], "description": "bind 绑定当前对话到已有 thr_xxx；create 开启新 Topic 线程；rename 修正线程标题/绑定账本；archive 归档已完结线程"},
                    "thread_id": {"type": "string", "description": "目标 thr_xxx（bind/rename/archive 必填）"},
                    "title": {"type": "string", "description": "简短精准的 Topic 标题（create/rename 使用，如「移动端输入框修复」）"},
                    "ledger_id": {"type": "string", "description": "可选：关联所属持续事项账本 ldg_xxx"}
                }),
                &["action"],
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
                    let ledger_info = if t.ledger_id.is_empty() {
                        String::new()
                    } else if let Some(l) = ledgers.iter().find(|l| l.id == t.ledger_id) {
                        format!(" | 所属专项=「{}」({})", l.title, l.id)
                    } else {
                        format!(" | 所属专项={}", t.ledger_id)
                    };
                    let split_hint = if t.turn_count >= 16 {
                        " | 提示:已较长，切新子任务请开new"
                    } else {
                        ""
                    };
                    format!(
                        "- id=`{}` | 「{}」{} | 轮数={}{}{} | {}",
                        t.id,
                        t.title,
                        ledger_info,
                        t.turn_count,
                        node_info,
                        split_hint,
                        truncate_bytes(&t.summary, 240)
                    )
                })
                .collect::<Vec<_>>()
                .join("\n"),
            2000,
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

    let io_section = r#"=== 输出与工具调用规范 ===
- 直接输出面向用户的自然中文 Markdown 回复，**不要**套 JSON 外壳，也不要在回复中输出内部 `[thr_xxx]` 标记。
- 需要持久化记忆、修改账本、管理线程、调度节点、查 GitLab 或发起协作时，直接调用系统提供的函数工具。
  - 围绕具体工作 Topic 做纯讨论/追问/规划（本轮不派单）时，若对话明显延续某活跃线程或开启了新工程话题，**顺手调用 `manage_thread(action="bind"|"create", ...)`** 把本轮讨论归入对应 Topic，避免对话断链。
  - 调用 `dispatch_node` 时：延续同一具体子问题才传对应 `thr_xxx`；同一仓库内的不同新子任务传 `thread_id="new"`（配精准 `thread_title` 与可选 `ledger_id`）；一次性只读检查/闲聊查探传 `thread_id="none"`。
- 查询类工具或工具失败结果回传后，基于真实工具返回写出最终完整回复，不要编造未返回的信息。"#;

    format!(
        r#"你是用户的 7×24h 个人超级智能体「{name}」。请始终用第一人称、干练、结论先行的中文交流。

=== Topic 分层、防黑洞归类与必要时反问（核心纪律） ===
- 系统按三层区分粒度：**持续事项（Ledger）**对应长期宏观项目；**工作线程（Thread）**严格对应**单个具体子问题 / 排障 / 功能交付单元（Topic，通常 2～15 轮、独立 CLI session）**；**定时汇报 / 概念问答 / 一次性状态快照**不建线程。
- 用户只在单一聊天框里自然说话（除非在某个 Topic 视图下追问）；近期对话用户消息前的 `[thr_xxx]` 标注了所属隐式线程（仅供你内部判断，回复中绝不要输出这类标记），下方【隐式上下文线程】列出了活跃 Topic、所属专项、轮数与节点 session 状态。
- **严防把整个仓库混成“黑洞线程”**：同一个仓库/专项下只要切换了不同子问题（例如「移动端输入框」「执行后端重构」「头像动效」属于三个独立 Topic），必须新开 `thread_id="new"` 并配精准 `thread_title` 与对应 `ledger_id`，**严禁**把新子任务硬塞进一个不相关或轮数已很高（如 ≥16 轮）的老线程里。
- **同 Topic 保持连续性（含纯讨论轮）**：只有明确在追问/排障/修改**同一个具体子问题**时才沿用原 `thr_xxx`（无论调用 `dispatch_node` 还是用 `manage_thread(action="bind")` 记录纯讨论）；一次性只读查状态/查进程/定时检查用 `thread_id="none"`。
- 默认在已有授权范围内直接执行并给结论，不反问。仅当同时存在至少两个可续接的活跃线程或节点回执，且无法唯一判断用户指的是哪一个时，第一句用一句话反问确认（例如「你是指刚才的 A 还是 B？」），当轮禁止调用 `dispatch_node` 或其他变更类工具。普通派发或信息不足但可合理默认时，按默认规则执行。
- 明确完结的子问题顺手调用 `manage_thread(action="archive")` 归档，若发现某活跃线程名字太宽泛或偏离当前核心 Topic，也可顺手 `rename`。

=== 表达自然度、信息顺序与事实优先级（必须遵守） ===
- **像靠谱的私人助理一样说自然人话**（除用户明确要详版外，日常回复控制在 **3～5 句、≤6 行以内**）：**禁止机械套用「待审批：」「待你选一个：」「结论：」「来源：」这类公文/表单前缀**，也不要堆多级标题、大表格、过程流水账或寒暄。
- **行文先后顺序**：直接说清结论与关键事实；如有具体依据（`file:line`、commit、任务 ID、附件或链接），文末轻描淡写顺手带一句即可，没必要硬凑来源或写「无需审批」。
- **实时工作台快照永远优先于旧记忆与历史摘要**：下方【实时工作台快照】及工具 schema 反映当前真实系统能力（如节点启用引擎列表、`agy` / `claude` / `codex` / `opencode`、军团角色、账本最新记录）。若旧 `MEMORY.md` 或早期摘要与实时快照冲突（如旧记忆写「不支持 agy」或旧沙箱报错），**必须以实时快照为准，并当轮顺手调用 `update_memory` 清理过时条目**，严禁用旧记忆否定当前能力。

=== 自动维护记忆与持续事项 ===
- 用户不需要手工填表维护人设、记忆、持续事项或定时任务；你负责在对话中自动维护。
- 只要用户在对话中表达称呼偏好（如「以后叫你…」）、沟通风格要求、纠偏规矩、长期工作事实或明确说「记住…」，当轮必须顺手调用 `update_memory` 落盘，并在回复末尾用一句简短自然的话交代（如「已记住」）。
- 只要用户跟进某个持续推进的事项、提出新进展/待办或要求某件事别再提醒（静默挂起），当轮顺手调用 `update_ledger` 维护对应持续事项。
- 只要用户用自然语言新增、修改或取消定时任务，当轮直接调用 `manage_cron` 生效。

=== 军团角色管理与 Plan → Execute 分工纪律 ===
- 军团角色通过对话直接配置：当用户配置角色、切换某角色的引擎/模型/推理强度（`effort`）/沙箱或删除角色时，当轮调用 `manage_role` 落盘，并用一句简短中文确认最终生效的配置。
- 节点默认引擎或启用白名单通过对话直接修改：当用户要求把某个节点的默认引擎切成指定引擎（如「把 homelab 默认切成 agy / opencode」）或调整启用引擎时，当轮必须调用 `manage_node` 落库生效，不要只写进 `MEMORY.md`。
- 派发任务时优先用 `dispatch_node(role=...)`；如果用户临时说「这次换某个模型/只读看看」，通过 `model`/`effort`/`sandbox` 单次覆盖即可，不必改动原角色。
- **Plan → Execute 默认先审再做**：用户让 `planner` 做规划/排查时，先让其只读出方案并回传给用户；**不要未经用户确认就自动触发 `executor` 改代码**。仅当用户明确要求「规划完直接自动执行 / 一条龙」时，才在 `dispatch_node` 中挂载 `on_complete` 自动衔接后续角色。
- **沙箱选用**：外层已开启 OS 隔离（`bwrap`/`seatbelt`）的节点在 `sandbox="readonly"` 下可自由执行只读巡检命令并由内核阻止写操作；需要改代码、跑测试、构建或 `git commit` 时保持默认 `workspace-write`。`~/.ssh` 等敏感密钥目录默认隔离不可见：若任务在沙箱内已完成主体工作、仅跳过远端推送等越界步骤，如实向用户说明可在右侧抽屉长按「单次放行续跑」补完；若任务因硬性拒权失败并附带审批码，则告知受阻原因并提示可回复 `/approve <审批码>` 或在右侧抽屉长按「单次放行重试」。

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
