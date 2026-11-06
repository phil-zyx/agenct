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
}
pub(super) struct TurnRequest<'a> {
    pub kind: TurnKind,
    pub message: &'a str,
    pub session_id: &'a str,
    pub history: &'a [Message],
    pub profile: &'a RuntimeProfile,
    pub task_context: Option<&'a serde_json::Value>,
}

/// Static dispatch keeps the transport replaceable without exposing Hub/DB authority.
pub(super) trait Runtime {
    fn turn(&self, request: TurnRequest<'_>) -> impl Future<Output = Result<AdapterReply>> + Send;
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

fn validate_request(request: &TurnRequest<'_>) -> Result<()> {
    if request.kind == TurnKind::DraftDelivery
        && (request.message != DRAFT_MESSAGE
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
    }
}

pub(super) struct MockRuntime;
impl Runtime for MockRuntime {
    async fn turn(&self, request: TurnRequest<'_>) -> Result<AdapterReply> {
        validate_request(&request)?;
        if request.kind == TurnKind::DraftDelivery {
            return Ok(AdapterReply { reply: "[MOCK · draft only] 请根据任务共享摘要核对、编辑并明确提交交付。这不是执行或完成证据。".into(), backend: "mock".into(), proposal: None });
        }
        if let Some((owner, summary)) = mock_delegation(request.message) {
            return Ok(AdapterReply {
                reply: "[MOCK · deterministic test double] 请检查接收人和将共享的任务内容，再明确确认。确认前对方不会收到任务。".into(),
                backend: "mock".into(), proposal: Some(AdapterProposal { owner, summary }),
            });
        }
        Ok(AdapterReply {
            reply: format!("[MOCK · no model] 收到你的消息。当前已保存 {} 条已完成上下文消息。真实任务请使用对话中的委派操作；本回复不会执行工具。", request.history.len()),
            backend: "mock".into(), proposal: None,
        })
    }
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
        let mut payload = serde_json::json!({"session_id":request.session_id,
            "runtime_profile":request.profile,
            "history":request.history.iter().map(|m|serde_json::json!({"role":m.role,"content":m.content})).collect::<Vec<_>>(),
            "message":request.message});
        if request.kind == TurnKind::DraftDelivery {
            payload["kind"] = serde_json::json!("draft_delivery");
        }
        if let Some(context) = request.task_context {
            payload["task_context"] = context.clone();
        }
        while payload["history"].as_array().unwrap().len() > 100
            || serde_json::to_vec(&payload).unwrap().len() > 60000
        {
            if payload["history"].as_array().unwrap().is_empty() {
                return Err(invalid("chat context exceeds adapter limit"));
            }
            payload["history"].as_array_mut().unwrap().remove(0);
        }
        let response = self
            .client
            .post(self.url)
            .bearer_auth(self.token)
            .json(&payload)
            .send()
            .await
            .map_err(|_| adapter_failed())?;
        if !response.status().is_success() {
            return Err(adapter_failed());
        }
        response
            .json::<AdapterReply>()
            .await
            .map_err(|_| adapter_failed())
    }
}

// Bounded recognition lives only in the offline test double.
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
