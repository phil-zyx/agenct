//! Brain response parsing, native tool_call normalization, and tag cleanup.
use crate::llm::LlmToolCall;
use serde::Deserialize;
use serde_json::{Map, Value};

#[derive(Deserialize)]
pub(super) struct BrainResponse {
    #[serde(default)]
    pub(super) reply: String,
    #[serde(default)]
    pub(super) thread_id: Option<String>,
    #[serde(default)]
    pub(super) thread_title: Option<String>,
    #[serde(default)]
    pub(super) tools: Vec<ToolCall>,
}

#[derive(Deserialize)]
pub(super) struct ToolCall {
    pub(super) tool: String,
    #[serde(default)]
    pub(super) action: Option<String>,
    #[serde(default)]
    pub(super) name: Option<String>,
    #[serde(default)]
    pub(super) avatar_id: Option<String>,
    #[serde(default)]
    pub(super) soul_md: Option<String>,
    #[serde(default)]
    pub(super) memory_append: Option<String>,
    #[serde(default)]
    pub(super) memory_replace: Option<String>,
    #[serde(default)]
    pub(super) ledger_id: Option<String>,
    #[serde(default)]
    pub(super) thread_id: Option<String>,
    #[serde(default)]
    pub(super) title: Option<String>,
    #[serde(default)]
    pub(super) status: Option<String>,
    #[serde(default)]
    pub(super) notes_append: Option<String>,
    #[serde(default)]
    pub(super) notes_replace: Option<String>,
    #[serde(default)]
    pub(super) cron_id: Option<String>,
    #[serde(default)]
    pub(super) daily_time_cst: Option<String>,
    #[serde(default)]
    pub(super) interval_minutes: Option<i64>,
    #[serde(default)]
    pub(super) instruction: Option<String>,
    #[serde(default)]
    pub(super) target: Option<String>,
    #[serde(default)]
    pub(super) rule_id: Option<String>,
    #[serde(default)]
    pub(super) keyword: Option<String>,
    #[serde(default)]
    pub(super) node_id: Option<String>,
    #[serde(default)]
    pub(super) agent: Option<String>,
    #[serde(default)]
    pub(super) role: Option<String>,
    #[serde(default)]
    pub(super) description: Option<String>,
    #[serde(default)]
    pub(super) model: Option<String>,
    #[serde(default)]
    pub(super) effort: Option<String>,
    #[serde(default)]
    pub(super) sandbox: Option<String>,
    #[serde(default)]
    pub(super) prompt: Option<String>,
    #[serde(default)]
    pub(super) prefer_cloud: Option<bool>,
    #[serde(default)]
    pub(super) on_complete: Option<String>,
    #[serde(default)]
    pub(super) peer: Option<String>,
}

fn minimal_tool_call(name: &str) -> ToolCall {
    ToolCall {
        tool: name.to_owned(),
        action: None,
        name: None,
        avatar_id: None,
        soul_md: None,
        memory_append: None,
        memory_replace: None,
        ledger_id: None,
        thread_id: None,
        title: None,
        status: None,
        notes_append: None,
        notes_replace: None,
        cron_id: None,
        daily_time_cst: None,
        interval_minutes: None,
        instruction: None,
        target: None,
        rule_id: None,
        keyword: None,
        node_id: None,
        agent: None,
        role: None,
        description: None,
        model: None,
        effort: None,
        sandbox: None,
        prompt: None,
        prefer_cloud: None,
        on_complete: None,
        peer: None,
    }
}

/// Converts OpenAI native `LlmToolCall` items into internal `ToolCall` structs.
pub(super) fn native_calls_to_tool_calls(calls: &[LlmToolCall]) -> Vec<ToolCall> {
    calls
        .iter()
        .map(|call| {
            let mut obj = match serde_json::from_str::<Value>(call.arguments.trim()) {
                Ok(Value::Object(map)) => map,
                _ if call.arguments.trim().is_empty() => Map::new(),
                _ => return minimal_tool_call(&call.name),
            };
            obj.insert("tool".into(), Value::String(call.name.clone()));
            serde_json::from_value::<ToolCall>(Value::Object(obj))
                .unwrap_or_else(|_| minimal_tool_call(&call.name))
        })
        .collect()
}

/// Removes internal `[thr_xxx]` thread tags the model may echo at the start of lines.
pub(super) fn strip_thread_tags(reply: &str) -> String {
    reply
        .lines()
        .map(|line| {
            let mut rest = line;
            loop {
                let trimmed = rest.trim_start();
                let Some(after) = trimmed.strip_prefix("[thr_") else {
                    break;
                };
                let Some(end) = after.find(']') else { break };
                if end == 0 || !after[..end].bytes().all(|b| b.is_ascii_alphanumeric()) {
                    break;
                }
                rest = after[end + 1..].trim_start();
            }
            rest
        })
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_owned()
}

pub(super) fn default_reply_for_tools(tools: &[ToolCall]) -> String {
    if tools.iter().any(|t| t.tool == "dispatch_node") {
        return "已派发给执行节点处理，完成后我会把结果发回来。".into();
    }
    if tools.iter().any(|t| t.tool == "fetch_gitlab_commits") {
        return "好的，我来查一下 GitLab 提交记录。".into();
    }
    if tools.iter().any(|t| t.tool == "delegate_colleague") {
        return "已发起协作请求。".into();
    }
    if tools.iter().any(|t| t.tool == "manage_cron") {
        return "已更新定时任务。".into();
    }
    if tools.iter().any(|t| t.tool == "manage_role") {
        return "已更新军团角色配置。".into();
    }
    if tools
        .iter()
        .any(|t| matches!(t.tool.as_str(), "update_memory" | "update_ledger"))
    {
        return "好的，已记录。".into();
    }
    "好的，已处理。".into()
}

fn strip_hallucinated_tags(raw: &str) -> String {
    let mut s = raw.trim().to_owned();
    // Strip <think>...</think> blocks if any reasoning model emits them.
    while let (Some(a), Some(b)) = (s.find("<think>"), s.find("</think>")) {
        if b > a {
            s.replace_range(a..b + "</think>".len(), "");
        } else {
            break;
        }
    }
    for tag in [
        "<tool_call>",
        "</tool_call>",
        "<function_call>",
        "</function_call>",
    ] {
        s = s.replace(tag, "");
    }
    s.trim().to_owned()
}

pub(super) fn parse_brain_response(raw: &str) -> BrainResponse {
    let cleaned = strip_hallucinated_tags(raw);
    if let (Some(s), Some(e)) = (cleaned.find('{'), cleaned.rfind('}')) {
        let json_slice = &cleaned[s..=e];
        if let Ok(mut parsed) = serde_json::from_str::<BrainResponse>(json_slice) {
            if !parsed.reply.trim().is_empty() || !parsed.tools.is_empty() {
                if parsed.reply.trim().is_empty() {
                    parsed.reply = default_reply_for_tools(&parsed.tools);
                }
                return parsed;
            }
        }
        // Fallback 1: model emitted a bare tool object like `{"tool":"fetch_gitlab_commits"}`.
        if let Ok(single_tool) = serde_json::from_str::<ToolCall>(json_slice) {
            if !single_tool.tool.trim().is_empty() {
                let prose = format!("{} {}", cleaned[..s].trim(), cleaned[e + 1..].trim())
                    .trim()
                    .to_owned();
                let tools = vec![single_tool];
                let reply = if prose.is_empty() {
                    default_reply_for_tools(&tools)
                } else {
                    prose
                };
                return BrainResponse {
                    reply,
                    thread_id: None,
                    thread_title: None,
                    tools,
                };
            }
        }
    }
    // Fallback 2: model emitted a bare JSON array of ToolCalls `[{"tool":"..."}]`.
    if let (Some(s), Some(e)) = (cleaned.find('['), cleaned.rfind(']')) {
        if let Ok(tools) = serde_json::from_str::<Vec<ToolCall>>(&cleaned[s..=e]) {
            if !tools.is_empty() && tools.iter().all(|t| !t.tool.trim().is_empty()) {
                let prose = format!("{} {}", cleaned[..s].trim(), cleaned[e + 1..].trim())
                    .trim()
                    .to_owned();
                let reply = if prose.is_empty() {
                    default_reply_for_tools(&tools)
                } else {
                    prose
                };
                return BrainResponse {
                    reply,
                    thread_id: None,
                    thread_title: None,
                    tools,
                };
            }
        }
    }
    BrainResponse {
        reply: cleaned,
        thread_id: None,
        thread_title: None,
        tools: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::{native_calls_to_tool_calls, parse_brain_response, strip_thread_tags};
    use crate::llm::LlmToolCall;

    #[test]
    fn strips_leaked_thread_tags_only_at_line_start() {
        assert_eq!(strip_thread_tags("[thr_a4e693df] 图我收到了"), "图我收到了");
        assert_eq!(
            strip_thread_tags("[thr_a1] [thr_b2] 你好\n第二行"),
            "你好\n第二行"
        );
        assert_eq!(
            strip_thread_tags("引用 [thr_a1] 保留"),
            "引用 [thr_a1] 保留"
        );
        assert_eq!(
            strip_thread_tags("[thr_] 非法标记保留"),
            "[thr_] 非法标记保留"
        );
        assert_eq!(
            strip_thread_tags("- 列表\n  缩进保留"),
            "- 列表\n  缩进保留"
        );
    }

    #[test]
    fn recovers_bare_tool_call_with_hallucinated_xml_tag() {
        let parsed = parse_brain_response(r#"{"tool":"fetch_gitlab_commits"}</tool_call>"#);
        assert_eq!(parsed.tools.len(), 1);
        assert_eq!(parsed.tools[0].tool, "fetch_gitlab_commits");
        assert!(!parsed.reply.contains("</tool_call>"));
        assert!(!parsed.reply.contains('{'));
    }

    #[test]
    fn recovers_prose_plus_tool_call_tag_and_strips_think() {
        let raw = r#"<think>internal</think>好的，我来查一下。<tool_call>{"tool":"dispatch_node","title":"查 Atlas 提交","instruction":"git log"}</tool_call>"#;
        let parsed = parse_brain_response(raw);
        assert_eq!(parsed.reply, "好的，我来查一下。");
        assert_eq!(parsed.tools.len(), 1);
        assert_eq!(parsed.tools[0].tool, "dispatch_node");
    }

    #[test]
    fn synthesizes_reply_when_tools_envelope_omits_reply() {
        let parsed = parse_brain_response(
            r#"{"reply":"","tools":[{"tool":"dispatch_node","title":"t","instruction":"i"}]}"#,
        );
        assert_eq!(parsed.tools.len(), 1);
        assert!(parsed.reply.contains("已派发给执行节点"));
    }

    #[test]
    fn native_calls_to_tool_calls_parses_and_handles_invalid_json() {
        let calls = vec![
            LlmToolCall {
                id: "c1".into(),
                name: "dispatch_node".into(),
                arguments: r#"{"title":"Build","instruction":"cargo test","prefer_cloud":false}"#
                    .into(),
            },
            LlmToolCall {
                id: "c2".into(),
                name: "delegate_colleague".into(),
                arguments: "{not valid json".into(),
            },
        ];
        let parsed = native_calls_to_tool_calls(&calls);
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].tool, "dispatch_node");
        assert_eq!(parsed[0].title.as_deref(), Some("Build"));
        assert_eq!(parsed[0].prefer_cloud, Some(false));
        assert_eq!(parsed[1].tool, "delegate_colleague");
        assert!(parsed[1].peer.is_none());
    }
}
