//! Runtime transport and deterministic test double. Neither can persist proposals.
use crate::{invalid, messages::Message, profiles::RuntimeProfile, Error, Result};
use axum::http::StatusCode;
use serde::Deserialize;
use std::future::Future;

pub(super) const DRAFT_MESSAGE: &str = "Draft a delivery for this task for owner review.";
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum TurnKind {
    Chat,
    DraftDelivery,
    ContextSummary,
}
pub(super) struct TurnRequest<'a> {
    pub kind: TurnKind,
    pub message: &'a str,
    pub session_id: &'a str,
    pub history: &'a [Message],
    pub profile: &'a RuntimeProfile,
    pub task_context: Option<&'a serde_json::Value>,
    pub draft_revision: Option<&'a serde_json::Value>,
    /// The caller's role in task_context ("owner" | "requester"), for task actions.
    pub task_role: Option<&'a str>,
    /// Current owner's explicit work context, never permissions or shared task data.
    pub owner_context: Option<&'a serde_json::Value>,
    /// Chat-only editable profile snapshot and pending suggestion; never authority.
    pub profile_context: Option<&'a serde_json::Value>,
}

pub(super) fn owner_context(
    db: &rusqlite::Connection,
    owner: &str,
) -> Result<Option<serde_json::Value>> {
    let profile = crate::collaboration::read(db, owner)?;
    let context = serde_json::json!({"public":profile.public,"private":profile.private});
    let populated = ["public", "private"].iter().any(|group| {
        context[*group].as_object().is_some_and(|fields| {
            fields
                .values()
                .any(|value| value.as_str().is_some_and(|value| !value.is_empty()))
        })
    });
    Ok(populated.then_some(context))
}

/// Static dispatch keeps the transport replaceable without exposing Hub/DB authority.
pub(super) trait Runtime {
    fn turn(&self, request: TurnRequest<'_>) -> impl Future<Output = Result<AdapterReply>> + Send;
}

/// A model's reading of what the user asked to do with the selected task. Only a
/// suggestion: the Hub re-checks it against the caller's role and the task state,
/// and nothing changes until the user confirms (a private draft revision excepted).
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct TaskAction {
    pub action: String,
    #[serde(default)]
    pub instruction: Option<String>,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct AdapterProposal {
    pub owner: String,
    pub summary: String,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct AdapterReply {
    pub reply: String,
    pub backend: String,
    // Explicit null is required; an omitted proposal is a protocol error.
    #[serde(deserialize_with = "Option::deserialize")]
    pub proposal: Option<AdapterProposal>,
    #[serde(default)]
    pub task_action: Option<TaskAction>,
    pub profile_update: Option<serde_json::Value>,
}
impl AdapterReply {
    /// The same boundary checks every runtime's output before Hub persistence.
    pub fn valid(
        &self,
        backend: &str,
        requester: &str,
        message: &str,
        participant: impl Fn(&str) -> bool,
    ) -> bool {
        self.backend == backend
            && !self.reply.trim().is_empty()
            && self.reply.len() <= 32000
            && !(self.proposal.is_some() && self.profile_update.is_some())
            && self
                .profile_update
                .as_ref()
                .is_none_or(|changes| crate::profile_suggestions::validate_changes(changes).is_ok())
            && self.proposal.as_ref().is_none_or(|p| {
                p.owner != requester
                    && participant(&p.owner)
                    && message.contains(&p.owner)
                    && !p.summary.trim().is_empty()
                    && p.summary.len() <= 4000
                    && message.contains(&p.summary)
            })
    }
}

impl AdapterReply {
    /// Recipient-only rejection: the user named someone in the current message who
    /// cannot receive a delegation from them (the model is never told the
    /// participant list). A recipient the user never named is a protocol violation
    /// and stays fail-closed. Returns that untrusted name.
    pub fn unavailable_recipient(
        &self,
        backend: &str,
        requester: &str,
        message: &str,
        participant: impl Fn(&str) -> bool,
    ) -> Option<&str> {
        let proposal = self.proposal.as_ref()?;
        (self.backend == backend
            && !self.reply.trim().is_empty()
            && self.reply.len() <= 32000
            && !proposal.owner.trim().is_empty()
            && message.contains(&proposal.owner)
            && (proposal.owner == requester || !participant(&proposal.owner)))
        .then_some(proposal.owner.as_str())
    }
}

fn validate_request(request: &TurnRequest<'_>) -> Result<()> {
    if request.kind == TurnKind::ContextSummary
        && (request.message != crate::context_memory::SUMMARY_MESSAGE
            || request.history.is_empty()
            || request.task_context.is_some()
            || request.draft_revision.is_some()
            || request.owner_context.is_some()
            || request.profile_context.is_some()
            || uuid::Uuid::parse_str(request.session_id).is_err())
    {
        return Err(invalid("invalid summary runtime context"));
    }
    if request.kind == TurnKind::DraftDelivery
        && (request.message != DRAFT_MESSAGE
            || request.profile_context.is_some()
            || !request.history.is_empty()
            || uuid::Uuid::parse_str(request.session_id).is_err()
            || request
                .task_context
                .is_none_or(|context| context["status"] != "working"))
    {
        return Err(invalid("invalid draft runtime context"));
    }
    Ok(())
}
impl AdapterReply {
    pub fn valid_draft(&self, backend: &str) -> bool {
        self.backend == backend
            && !self.reply.trim().is_empty()
            && self.reply.len() <= 8000
            && self.proposal.is_none()
            && self.profile_update.is_none()
    }
}

pub(super) struct MockRuntime;
impl Runtime for MockRuntime {
    async fn turn(&self, request: TurnRequest<'_>) -> Result<AdapterReply> {
        validate_request(&request)?;
        if request.kind == TurnKind::ContextSummary {
            return Ok(AdapterReply {
                reply: crate::context_memory::mock_summary(request.history),
                backend: "mock".into(),
                proposal: None,
                task_action: None,
                profile_update: None,
            });
        }
        if request.kind == TurnKind::DraftDelivery {
            return Ok(AdapterReply { reply: "[MOCK · draft only] 请根据任务共享摘要核对、编辑并明确提交交付。这不是执行或完成证据。".into(), backend: "mock".into(), proposal: None, task_action: None, profile_update: None });
        }
        if let Some(context) = request.task_context {
            if let Some(task_action) = mock_task_action(request.message, context, request.task_role)
            {
                return Ok(AdapterReply {
                    reply: format!(
                        "[MOCK · deterministic test double] 理解为：{}。",
                        task_action.action
                    ),
                    backend: "mock".into(),
                    proposal: None,
                    task_action: Some(task_action),
                    profile_update: None,
                });
            }
        }
        if let Some(changes) = mock_profile_update(request.message, request.profile_context) {
            return Ok(AdapterReply {
                reply: "准备记住这些内容，请检查后确认。".into(),
                backend: "mock".into(),
                proposal: None,
                task_action: None,
                profile_update: Some(changes),
            });
        }
        if let Some((owner, summary)) = mock_delegation(request.message) {
            return Ok(AdapterReply {
                reply: "[MOCK · deterministic test double] 请检查接收人和将共享的任务内容，再明确确认。确认前对方不会收到任务。".into(),
                backend: "mock".into(), proposal: Some(AdapterProposal { owner, summary }), task_action: None, profile_update: None,
            });
        }
        Ok(AdapterReply {
            reply: format!("[MOCK · no model] 收到你的消息。当前已保存 {} 条已完成上下文消息。真实任务请使用对话中的委派操作；本回复不会执行工具。", request.history.len()),
            backend: "mock".into(), proposal: None, task_action: None, profile_update: None,
        })
    }
}

/// Offline keyword reading of a task instruction; real language understanding is the model's.
fn mock_task_action(
    message: &str,
    context: &serde_json::Value,
    role: Option<&str>,
) -> Option<TaskAction> {
    let has = |words: &[&str]| words.iter().any(|w| message.contains(w));
    // Like a model, it reads intent from words regardless of the task's state;
    // whether the step is possible now is the Hub's decision.
    let action = match (role?, context["status"].as_str()?) {
        ("owner", _) if has(&["接手", "接了", "accept"]) => "accept",
        ("owner", _) if has(&["交付", "发给", "deliver"]) => "deliver",
        ("requester", _) if has(&["退回", "重做", "修改", "return"]) => "return",
        ("requester", _) if has(&["验收", "通过", "可以", "approve"]) => "approve",
        ("owner", "working") => "revise",
        _ => return None,
    };
    Some(TaskAction {
        action: action.into(),
        instruction: (action == "revise").then(|| message.to_owned()),
    })
}

pub(super) struct DshRuntime<'a> {
    pub client: &'a reqwest::Client,
    pub url: &'a str,
    pub token: &'a str,
}
pub(super) fn adapter_failed() -> Error {
    Error(
        StatusCode::BAD_GATEWAY,
        "adapter failed; no mock fallback; task actions were not executed",
    )
}
impl Runtime for DshRuntime<'_> {
    async fn turn(&self, request: TurnRequest<'_>) -> Result<AdapterReply> {
        validate_request(&request)?;
        if self.url.ends_with("/chat/completions") {
            return self.turn_openai_compatible(request).await;
        }
        let mut payload = serde_json::json!({"session_id":request.session_id,
            "runtime_profile":request.profile,
            "history":request.history.iter().map(|m|serde_json::json!({"role":m.role,"content":m.content})).collect::<Vec<_>>(),
            "message":request.message});
        if request.kind == TurnKind::DraftDelivery {
            payload["kind"] = serde_json::json!("draft_delivery");
        }
        if request.kind == TurnKind::ContextSummary {
            payload["kind"] = serde_json::json!("context_summary");
        }
        if let (Some(role), Some(_)) = (request.task_role, request.task_context) {
            payload["task_role"] = serde_json::json!(role);
        }
        if let Some(revision) = request.draft_revision {
            payload["draft_revision"] = revision.clone();
        }
        if let Some(context) = request.task_context {
            payload["task_context"] = context.clone();
        }
        if let Some(context) = request.owner_context {
            payload["owner_context"] = context.clone();
        }
        if let Some(context) = request.profile_context {
            payload["profile_context"] = context.clone();
        }
        if payload["history"].as_array().unwrap().len() > 100
            || serde_json::to_vec(&payload).unwrap().len() > 60000
        {
            return Err(invalid(
                "chat context exceeds adapter limit; summarize before dispatch",
            ));
        }
        let mut response = self
            .client
            .post(self.url)
            .bearer_auth(self.token)
            .json(&payload)
            .send()
            .await
            .map_err(|_| adapter_failed())?;
        if !response.status().is_success() {
            let status = response.status();
            let is_json = response
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .is_some_and(|value| {
                    value
                        .split(';')
                        .next()
                        .unwrap_or("")
                        .trim()
                        .eq_ignore_ascii_case("application/json")
                });
            if is_json && matches!(status.as_u16(), 400 | 502 | 503 | 504) {
                // Error responses are tiny fixed-code objects. Cap reads rather
                // than accepting arbitrary gateway HTML or provider diagnostics.
                let mut body = Vec::new();
                while let Some(chunk) = response.chunk().await.map_err(|_| adapter_failed())? {
                    if body.len() + chunk.len() > 1024 {
                        return Err(adapter_failed());
                    }
                    body.extend_from_slice(&chunk);
                }
                if let Some(failure) = crate::chat_failures::adapter_error(status, &body) {
                    return Err(failure);
                }
            }
            return Err(adapter_failed());
        }
        response
            .json::<AdapterReply>()
            .await
            .map_err(|_| adapter_failed())
    }
}

impl DshRuntime<'_> {
    async fn turn_openai_compatible(&self, request: TurnRequest<'_>) -> Result<AdapterReply> {
        let model = std::env::var("AGENCT_LLM_MODEL")
            .or_else(|_| std::env::var("OPENAI_MODEL"))
            .unwrap_or_else(|_| "gpt-4o-mini".into());
        let sys_prompt = match request.kind {
            TurnKind::ContextSummary => {
                "你是用户的私有会话摘要压缩引擎。请将历史对话浓缩为保留关键事实、决定、数字、偏好与待办的连贯中文摘要。\
                 只输出严格合法的 JSON：{\"reply\":\"摘要正文\",\"proposal\":null}"
                    .to_owned()
            }
            TurnKind::DraftDelivery => {
                format!(
                    "你是用户的 7×24h 管家起草助手。请根据 task_context（及可选的 draft_revision 修改要求）为用户起草一版可直接交付的专业中文草稿（不超过 7500 字节）。\n\
                     task_context: {}\n\
                     draft_revision: {}\n\
                     只输出严格合法的 JSON：{{\"reply\":\"草稿正文\",\"proposal\":null}}",
                    request
                        .task_context
                        .map(|v| v.to_string())
                        .unwrap_or_else(|| "null".into()),
                    request
                        .draft_revision
                        .map(|v| v.to_string())
                        .unwrap_or_else(|| "null".into()),
                )
            }
            TurnKind::Chat => {
                format!(
                    "你是用户的 7×24h 个人超级管家（Super-Agent）。\n\
                     task_context: {}\n\
                     task_role: {}\n\
                     owner_context: {}\n\
                     profile_context: {}\n\
                     请优先输出 JSON 对象：{{\"reply\":\"你的中文回复\",\"proposal\":null,\"task_action\":null,\"profile_update\":null}}。\n\
                     - 请始终自然对话，严禁向用户说教内部提案格式、严禁要求用户按「请让 xxx 做 yyy」复述；像 agy、agy1、codex、claude、tmux、homelab 等属于用户的本地开发 Agent 或节点环境，绝不是跨人协作的外部同事；\n\
                     - 仅当用户当前消息明确要求把任务委派给某个真实人类同事参与者时，设置 proposal: {{\"owner\":\"接收人ID\",\"summary\":\"直接从用户当前消息原样截取的任务摘要子串\"}}；\n\
                     - 若存在 task_context 且用户在操作该任务，可设置 task_action: {{\"action\":\"accept|revise|deliver|approve|return\",\"instruction\":\"可选修订说明\"}}。",
                    request
                        .task_context
                        .map(|v| v.to_string())
                        .unwrap_or_else(|| "null".into()),
                    request.task_role.unwrap_or("none"),
                    request
                        .owner_context
                        .map(|v| v.to_string())
                        .unwrap_or_else(|| "null".into()),
                    request
                        .profile_context
                        .map(|v| v.to_string())
                        .unwrap_or_else(|| "null".into()),
                )
            }
        };
        let mut messages = vec![serde_json::json!({
            "role": "system",
            "content": sys_prompt,
        })];
        for m in request.history.iter().take(60) {
            messages.push(serde_json::json!({
                "role": if m.role == "user" { "user" } else { "assistant" },
                "content": m.content,
            }));
        }
        messages.push(serde_json::json!({
            "role": "user",
            "content": request.message,
        }));
        let body = serde_json::json!({
            "model": model,
            "messages": messages,
            "temperature": 0.2,
        });
        let resp = self
            .client
            .post(self.url)
            .bearer_auth(self.token)
            .json(&body)
            .send()
            .await
            .map_err(|_| adapter_failed())?;
        if !resp.status().is_success() {
            return Err(adapter_failed());
        }
        let val: serde_json::Value = resp.json().await.map_err(|_| adapter_failed())?;
        let raw_content = val["choices"]
            .get(0)
            .and_then(|c| c["message"]["content"].as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(adapter_failed)?;
        Ok(parse_openai_content_to_adapter_reply(raw_content))
    }
}

pub(crate) fn parse_openai_content_to_adapter_reply(raw_content: &str) -> AdapterReply {
    let trimmed = raw_content.trim();
    if let (Some(s), Some(e)) = (trimmed.find('{'), trimmed.rfind('}')) {
        if let Ok(mut obj) = serde_json::from_str::<serde_json::Value>(&trimmed[s..=e]) {
            if obj.is_object() && obj.get("reply").and_then(|r| r.as_str()).is_some() {
                obj["backend"] = serde_json::json!("dsh");
                if obj.get("proposal").is_none() {
                    obj["proposal"] = serde_json::Value::Null;
                }
                if let Ok(parsed) = serde_json::from_value::<AdapterReply>(obj) {
                    return parsed;
                }
            }
        }
    }
    let clean_reply: String = trimmed.chars().take(7800).collect();
    AdapterReply {
        reply: clean_reply,
        backend: "dsh".into(),
        proposal: None,
        task_action: None,
        profile_update: None,
    }
}

// Bounded recognition lives only in the offline test double.
fn mock_profile_update(
    message: &str,
    context: Option<&serde_json::Value>,
) -> Option<serde_json::Value> {
    use serde_json::json;
    let message = message.trim().trim_end_matches(['。', '！']);
    let mut updates = json!({});
    if let Some(value) = message.strip_prefix("把我的职责公开为") {
        if !value.trim().is_empty() {
            updates["public"] = json!({"responsibilities":value.trim()});
        }
    } else if message == "允许助手用公开资料答复" {
        updates["permissions"] = json!({"auto_reply_public":true});
    } else if message == "关闭公开自动答复" {
        updates["permissions"] = json!({"auto_reply_public":false});
    } else if message == "关闭自动起草" {
        updates["permissions"] = json!({"auto_prepare_drafts":false});
    } else if message == "开启自动起草" {
        updates["permissions"] = json!({"auto_prepare_drafts":true});
    } else if let Some(value) = message
        .strip_prefix("记住：")
        .or_else(|| message.strip_prefix("记住:"))
    {
        let mut private = json!({});
        for part in value.split(['，', ',', '。', '；', ';']).map(str::trim) {
            if part.starts_with("我负责") {
                private["work_context"] = json!(part);
            }
            if let Some(preference) = part.strip_prefix("回复") {
                if !preference.trim().is_empty() {
                    private["response_preferences"] = json!(preference.trim());
                }
            }
        }
        if !private.as_object().unwrap().is_empty() {
            updates["private"] = private;
        }
    } else if let Some(preference) = message.strip_prefix("回复改为") {
        if !preference.trim().is_empty() {
            updates["private"] = json!({"response_preferences":preference.trim()});
        }
    }
    if updates.as_object().unwrap().is_empty() {
        return None;
    }
    let mut merged = context
        .and_then(|ctx| ctx.get("pending"))
        .and_then(|pending| pending.get("changes"))
        .filter(|changes| changes.is_object())
        .cloned()
        .unwrap_or(json!({}));
    for (group, fields) in updates.as_object().unwrap() {
        if !merged[group].is_object() {
            merged[group] = json!({});
        }
        for (key, value) in fields.as_object().unwrap() {
            merged[group][key] = value.clone();
        }
    }
    Some(merged)
}

fn mock_delegation(message: &str) -> Option<(String, String)> {
    let rest = message
        .trim()
        .strip_prefix("请让 ")
        .or_else(|| message.trim().strip_prefix("ask "))?;
    let (owner, rest) = rest.split_once(' ')?;
    let summary = rest
        .strip_prefix("帮我")
        .map(str::trim_start)
        .or_else(|| rest.strip_prefix("to "))?;
    Some((owner.to_owned(), summary.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile() -> RuntimeProfile {
        RuntimeProfile {
            revision: 0,
            agent_id: "butler".into(),
            role: "coordinator".into(),
            model_ref: "server-default".into(),
        }
    }
    async fn mock(message: &str) -> AdapterReply {
        MockRuntime
            .turn(TurnRequest {
                kind: TurnKind::Chat,
                message,
                session_id: "",
                history: &[],
                profile: &profile(),
                task_context: None,
                draft_revision: None,
                task_role: None,
                owner_context: None,
                profile_context: None,
            })
            .await
            .unwrap()
    }
    fn valid(reply: &AdapterReply, message: &str) -> bool {
        reply.valid(&reply.backend, "alice", message, |id| {
            matches!(id, "alice" | "bob")
        })
    }
    #[tokio::test]
    async fn both_backends_reject_self_unknown_and_empty_mock_proposals() {
        for message in [
            "ask alice to review",
            "ask mallory to review",
            "请让 bob 帮我   ",
        ] {
            let mut answer = mock(message).await;
            assert!(
                answer.proposal.is_some(),
                "mock must emit unvalidated output"
            );
            for backend in ["mock", "dsh"] {
                answer.backend = backend.into();
                assert!(!valid(&answer, message));
            }
        }
    }
    #[tokio::test]
    async fn both_backends_require_current_message_binding_and_bounded_reply() {
        let message = "ask bob to review";
        for backend in ["mock", "dsh"] {
            let mut answer = mock(message).await;
            answer.backend = backend.into();
            assert!(valid(&answer, message));
            assert!(!valid(&answer, "summarize the selected task"));
            answer.proposal.as_mut().unwrap().summary = "private history".into();
            assert!(!valid(&answer, message));
            answer.proposal = None;
            answer.reply = " ".into();
            assert!(!valid(&answer, message));
            answer.reply = "x".repeat(32001);
            assert!(!valid(&answer, message));
        }
    }
    #[tokio::test]
    async fn backend_identity_is_pinned_by_hub_not_output() {
        let answer = mock("hello").await;
        assert!(!answer.valid("dsh", "alice", "hello", |_| true));
    }
    #[test]
    fn strict_reply_shape_requires_explicit_proposal() {
        for raw in [
            r#"{"reply":"hello","backend":"dsh"}"#,
            r#"{"reply":"hello","backend":"dsh","proposal":null,"execute":true}"#,
            r#"{"reply":"hello","backend":"dsh","proposal":{"owner":"bob","summary":"review","execute":true}}"#,
        ] {
            assert!(serde_json::from_str::<AdapterReply>(raw).is_err());
        }
        assert!(serde_json::from_str::<AdapterReply>(
            r#"{"reply":"hello","backend":"dsh","proposal":null}"#
        )
        .is_ok());
    }
}
