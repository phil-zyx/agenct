//! Single OpenAI-compatible LLM client (DeepSeek / OpenAI / Ollama) + deterministic test stub.
use crate::{config::LlmConfig, Error, Result};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

#[derive(Clone)]
pub struct LlmClient {
    config: LlmConfig,
    http: reqwest::Client,
}

impl LlmClient {
    pub fn new(config: LlmConfig, http: reqwest::Client) -> Self {
        Self { config, http }
    }

    pub fn label(&self) -> &'static str {
        match &self.config {
            LlmConfig::OpenAi { .. } => "openai-compatible",
            LlmConfig::Stub => "stub",
        }
    }

    pub async fn complete(
        &self,
        system: &str,
        history: &[ChatMessage],
        user_message: &str,
        json_mode: bool,
    ) -> Result<String> {
        match &self.config {
            LlmConfig::Stub => Ok(stub_reply(system, user_message, json_mode)),
            LlmConfig::OpenAi {
                url,
                api_key,
                model,
            } => {
                let mut messages = Vec::with_capacity(history.len() + 2);
                messages.push(json!({"role": "system", "content": system}));
                for m in history {
                    let role = if m.role == "user" {
                        "user"
                    } else {
                        "assistant"
                    };
                    messages.push(json!({"role": role, "content": m.content}));
                }
                messages.push(json!({"role": "user", "content": user_message}));

                let mut body = json!({
                    "model": model,
                    "messages": messages,
                    "temperature": 0.2,
                });
                if json_mode {
                    body["response_format"] = json!({"type": "json_object"});
                }

                let resp = self
                    .http
                    .post(url)
                    .bearer_auth(api_key)
                    .json(&body)
                    .send()
                    .await
                    .map_err(|_| Error(StatusCode::BAD_GATEWAY, "LLM request failed"))?;

                if !resp.status().is_success() {
                    return Err(Error(
                        StatusCode::BAD_GATEWAY,
                        "LLM provider returned error",
                    ));
                }

                let val: Value = resp
                    .json()
                    .await
                    .map_err(|_| Error(StatusCode::BAD_GATEWAY, "invalid LLM JSON response"))?;

                val["choices"][0]["message"]["content"]
                    .as_str()
                    .map(|s| s.trim().to_owned())
                    .filter(|s| !s.is_empty())
                    .ok_or(Error(StatusCode::BAD_GATEWAY, "empty LLM completion"))
            }
        }
    }
}

/// Deterministic stub for offline unit/integration tests without hitting external APIs.
fn stub_reply(system: &str, user_message: &str, json_mode: bool) -> String {
    if system.contains("CONTEXT_COMPACTION_TASK") {
        let snippet: String = user_message.chars().take(120).collect();
        return json!({
            "rolling_summary": format!("已压缩历史摘要：{}", snippet.replace('\n', " ")),
            "memory_md": Value::Null,
            "archive_thread_ids": []
        })
        .to_string();
    }
    let trimmed = user_message.trim();
    if !json_mode {
        return format!("[STUB] 收到：{trimmed}");
    }
    if let Some(rest) = trimmed.strip_prefix("/node_thread ") {
        let mut parts = rest.splitn(2, char::is_whitespace);
        let tid = parts.next().unwrap_or("").trim();
        let instr = parts.next().unwrap_or("").trim();
        if !tid.is_empty() && !instr.is_empty() {
            return json!({
                "reply": format!("已沿用上下文线程 {tid} 派发任务：{instr}"),
                "thread_id": tid,
                "tools": [{"tool": "dispatch_node", "thread_id": tid, "instruction": instr, "title": instr}]
            })
            .to_string();
        }
    }
    if let Some(rest) = trimmed.strip_prefix("/node ") {
        return json!({
            "reply": format!("已将任务派发给在线执行节点：{rest}"),
            "tools": [{"tool": "dispatch_node", "instruction": rest, "title": rest}]
        })
        .to_string();
    }
    if let Some(rest) = trimmed.strip_prefix("/remember ") {
        return json!({
            "reply": format!("好的，已写入长期记忆：{rest}"),
            "tools": [{"tool": "update_memory", "memory_append": rest}]
        })
        .to_string();
    }
    if let Some(rest) = trimmed.strip_prefix("/ledger ") {
        return json!({
            "reply": format!("已更新专项活账本：{rest}"),
            "tools": [{"tool": "update_ledger", "action": "upsert", "title": rest, "notes_append": rest}]
        })
        .to_string();
    }
    if let Some(rest) = trimmed.strip_prefix("/cron ") {
        return json!({
            "reply": format!("已创建定时任务：{rest}"),
            "tools": [{"tool": "manage_cron", "action": "add", "interval_minutes": 60, "instruction": rest}]
        })
        .to_string();
    }
    if let Some(rest) = trimmed.strip_prefix('@') {
        let mut parts = rest.splitn(2, char::is_whitespace);
        let peer = parts.next().unwrap_or("").trim();
        let body = parts.next().unwrap_or("").trim();
        if !peer.is_empty() && !body.is_empty() {
            return json!({
                "reply": format!("已转告 @{peer}：{body}"),
                "tools": [{"tool": "delegate_colleague", "peer": peer, "instruction": body}]
            })
            .to_string();
        }
    }
    json!({
        "reply": format!("收到你的消息：{trimmed}"),
        "tools": []
    })
    .to_string()
}
