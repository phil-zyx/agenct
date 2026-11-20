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

/// An image passed inline (base64 data URL) to a vision-capable model.
#[derive(Clone, Debug)]
pub struct ImageInput {
    pub mime_type: String,
    pub data_base64: String,
}

/// Marker in the system prompt of image-captioning calls (also used by the offline stub).
pub const VISION_CAPTION_MARKER: &str = "VISION_CAPTION_TASK";

/// Prefix of the synthetic message that feeds tool execution results back into the model.
pub const TOOL_RESULTS_MARKER: &str = "TOOL_RESULTS";
/// Stub-only command that simulates an upstream LLM failure (for offline tests).
const STUB_FAIL_COMMAND: &str = "/stub_fail";

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

    /// Model name shown to users (never includes the API key).
    pub fn model_name(&self) -> &str {
        match &self.config {
            LlmConfig::OpenAi { model, .. } => model,
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
        self.complete_with_images(system, history, user_message, &[], json_mode)
            .await
    }

    /// Like `complete`, attaching `images` to the final user message as OpenAI-style
    /// `image_url` content parts (base64 data URLs, so private Hub URLs never leave the box).
    pub async fn complete_with_images(
        &self,
        system: &str,
        history: &[ChatMessage],
        user_message: &str,
        images: &[ImageInput],
        json_mode: bool,
    ) -> Result<String> {
        match &self.config {
            LlmConfig::Stub if user_message.trim() == STUB_FAIL_COMMAND => Err(Error(
                StatusCode::BAD_GATEWAY,
                "LLM provider returned error",
            )),
            LlmConfig::Stub => Ok(stub_reply(system, user_message, images.len(), json_mode)),
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
                if images.is_empty() {
                    messages.push(json!({"role": "user", "content": user_message}));
                } else {
                    let mut parts = vec![json!({"type": "text", "text": user_message})];
                    for img in images {
                        parts.push(json!({
                            "type": "image_url",
                            "image_url": {"url": format!("data:{};base64,{}", img.mime_type, img.data_base64)}
                        }));
                    }
                    messages.push(json!({"role": "user", "content": parts}));
                }

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
                    let msg = match resp.status().as_u16() {
                        401 | 403 => "LLM 认证失败：API Key 无效或无权限",
                        404 => "LLM 接口地址或模型名不存在",
                        400 | 422 => "LLM 拒绝了请求：模型可能不支持该输入（如图片）或参数",
                        429 => "LLM 限流或额度不足",
                        _ => "LLM provider returned error",
                    };
                    return Err(Error(StatusCode::BAD_GATEWAY, msg));
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
fn stub_reply(system: &str, user_message: &str, images: usize, json_mode: bool) -> String {
    if system.contains(VISION_CAPTION_MARKER) {
        return format!("[STUB 识图] {images} 张图片：终端里显示 error: connection refused");
    }
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
    if images > 0 {
        return json!({
            "reply": format!("（看到 {images} 张图片）收到你的消息：{trimmed}"),
            "tools": []
        })
        .to_string();
    }
    if let Some(rest) = trimmed.strip_prefix(TOOL_RESULTS_MARKER) {
        let first = rest
            .lines()
            .map(str::trim)
            .find(|l| l.starts_with("- "))
            .unwrap_or("")
            .trim_start_matches("- ");
        let snippet: String = first.chars().take(160).collect();
        return json!({
            "reply": format!("已根据工具结果整理：{snippet}"),
            "thread_id": Value::Null,
            "tools": []
        })
        .to_string();
    }
    if trimmed.starts_with("/gitlab") {
        return json!({
            "reply": "我来查一下 GitLab 提交记录。",
            "tools": [{"tool": "fetch_gitlab_commits"}]
        })
        .to_string();
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
