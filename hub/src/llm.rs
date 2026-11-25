//! Single OpenAI-compatible LLM client (DeepSeek / OpenAI / Ollama) + deterministic test stub.
use crate::{config::LlmConfig, Error, Result};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;

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

/// A native OpenAI function-calling tool invocation returned by the model.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LlmToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

/// Result of one streaming or non-streaming tool-capable LLM step.
#[derive(Clone, Debug, Default)]
pub struct LlmTurnResponse {
    pub content: String,
    pub tool_calls: Vec<LlmToolCall>,
}

/// Error returned by `complete_turn_stream` so unsupported native `tools` parameters can fall back cleanly.
#[derive(Debug)]
pub enum LlmTurnError {
    UnsupportedParams(Error),
    Other(Error),
}

impl From<LlmTurnError> for Error {
    fn from(e: LlmTurnError) -> Self {
        match e {
            LlmTurnError::UnsupportedParams(err) | LlmTurnError::Other(err) => err,
        }
    }
}

/// Marker in the system prompt of image-captioning calls (also used by the offline stub).
pub const VISION_CAPTION_MARKER: &str = "VISION_CAPTION_TASK";

/// Prefix of the synthetic message that feeds tool execution results back into the model.
pub const TOOL_RESULTS_MARKER: &str = "TOOL_RESULTS";
/// Marker in headless async task callback prompts (also used by the offline stub).
pub const TASK_CALLBACK_MARKER: &str = "TASK_CALLBACK_EVENT";
/// Stub-only command that simulates an upstream LLM failure (for offline tests).
const STUB_FAIL_COMMAND: &str = "/stub_fail";

#[derive(Default)]
struct ThinkStreamFilter {
    in_think: bool,
    pending: String,
}

impl ThinkStreamFilter {
    fn push<F: FnMut(&str)>(&mut self, chunk: &str, on_text: &mut F) {
        if chunk.is_empty() {
            return;
        }
        self.pending.push_str(chunk);
        loop {
            if self.in_think {
                if let Some(end) = self.pending.find("</think>") {
                    self.pending.drain(..end + "</think>".len());
                    self.in_think = false;
                    continue;
                }
                let keep_from = self.pending.len().saturating_sub("</think>".len() - 1);
                if let Some((drain_to, _)) = self
                    .pending
                    .char_indices()
                    .find(|(idx, _)| *idx >= keep_from)
                {
                    if drain_to > 0 {
                        self.pending.drain(..drain_to);
                    }
                }
                break;
            }
            if let Some(start) = self.pending.find("<think>") {
                if start > 0 {
                    on_text(&self.pending[..start]);
                }
                self.pending.drain(..start + "<think>".len());
                self.in_think = true;
                continue;
            }
            let keep = trailing_partial_prefix_len(&self.pending, "<think>");
            let emit_len = self.pending.len() - keep;
            if emit_len > 0 {
                on_text(&self.pending[..emit_len]);
                self.pending.drain(..emit_len);
            }
            break;
        }
    }

    fn finish<F: FnMut(&str)>(&mut self, on_text: &mut F) {
        if !self.in_think && !self.pending.is_empty() {
            on_text(&self.pending);
        }
        self.pending.clear();
    }
}

fn trailing_partial_prefix_len(text: &str, tag: &str) -> usize {
    let max_k = text.len().min(tag.len().saturating_sub(1));
    for k in (1..=max_k).rev() {
        let prefix = &tag[..k];
        if text.ends_with(prefix) {
            return k;
        }
    }
    0
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
                    return Err(map_http_error(resp.status().as_u16()));
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

    /// Tool-capable OpenAI-compatible turn with optional SSE streaming (`text/event-stream`).
    pub async fn complete_turn_stream<F>(
        &self,
        messages: &[Value],
        tools: &[Value],
        stream: bool,
        mut on_delta: F,
    ) -> std::result::Result<LlmTurnResponse, LlmTurnError>
    where
        F: FnMut(&str),
    {
        match &self.config {
            LlmConfig::Stub => stub_turn(messages, stream, &mut on_delta),
            LlmConfig::OpenAi {
                url,
                api_key,
                model,
            } => {
                let mut body = json!({
                    "model": model,
                    "messages": messages,
                    "temperature": 0.2,
                    "stream": stream,
                });
                if !tools.is_empty() {
                    body["tools"] = Value::Array(tools.to_vec());
                    body["tool_choice"] = json!("auto");
                }

                let mut resp = self
                    .http
                    .post(url)
                    .bearer_auth(api_key)
                    .json(&body)
                    .send()
                    .await
                    .map_err(|_| {
                        LlmTurnError::Other(Error(StatusCode::BAD_GATEWAY, "LLM request failed"))
                    })?;

                let status = resp.status().as_u16();
                if !resp.status().is_success() {
                    let err = map_http_error(status);
                    return if matches!(status, 400 | 422) {
                        Err(LlmTurnError::UnsupportedParams(err))
                    } else {
                        Err(LlmTurnError::Other(err))
                    };
                }

                let is_sse = resp
                    .headers()
                    .get(reqwest::header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok())
                    .is_some_and(|ct| ct.to_ascii_lowercase().contains("text/event-stream"));

                let mut filter = ThinkStreamFilter::default();
                let mut visible_content = String::new();
                let mut saw_non_ws = false;

                let mut emit = |piece: &str| {
                    let slice = if saw_non_ws {
                        piece
                    } else {
                        let trimmed = piece.trim_start();
                        if trimmed.is_empty() {
                            return;
                        }
                        saw_non_ws = true;
                        trimmed
                    };
                    visible_content.push_str(slice);
                    if stream {
                        on_delta(slice);
                    }
                };

                if stream && is_sse {
                    let mut byte_buf: Vec<u8> = Vec::new();
                    let mut tool_acc: BTreeMap<usize, LlmToolCall> = BTreeMap::new();

                    while let Some(chunk) = resp.chunk().await.map_err(|_| {
                        LlmTurnError::Other(Error(StatusCode::BAD_GATEWAY, "LLM stream failed"))
                    })? {
                        byte_buf.extend_from_slice(&chunk);
                        while let Some(nl) = byte_buf.iter().position(|b| *b == b'\n') {
                            let line_bytes: Vec<u8> = byte_buf.drain(..=nl).collect();
                            let line = String::from_utf8_lossy(&line_bytes);
                            consume_sse_line(&line, &mut filter, &mut emit, &mut tool_acc);
                        }
                    }
                    if !byte_buf.is_empty() {
                        let line = String::from_utf8_lossy(&byte_buf);
                        consume_sse_line(&line, &mut filter, &mut emit, &mut tool_acc);
                    }
                    filter.finish(&mut emit);

                    let tool_calls = finalize_tool_calls(tool_acc.into_values().collect());
                    if visible_content.trim().is_empty() && tool_calls.is_empty() {
                        return Err(LlmTurnError::Other(Error(
                            StatusCode::BAD_GATEWAY,
                            "empty LLM completion",
                        )));
                    }
                    return Ok(LlmTurnResponse {
                        content: visible_content.trim().to_owned(),
                        tool_calls,
                    });
                }

                let val: Value = resp.json().await.map_err(|_| {
                    LlmTurnError::Other(Error(StatusCode::BAD_GATEWAY, "invalid LLM JSON response"))
                })?;
                let msg = &val["choices"][0]["message"];
                if let Some(raw_text) = msg["content"].as_str() {
                    filter.push(raw_text, &mut emit);
                    filter.finish(&mut emit);
                }
                let raw_tools: Vec<LlmToolCall> = msg["tool_calls"]
                    .as_array()
                    .map(|arr| {
                        arr.iter()
                            .map(|item| {
                                let func = &item["function"];
                                let args = match &func["arguments"] {
                                    Value::String(s) => s.clone(),
                                    Value::Null => String::new(),
                                    other => other.to_string(),
                                };
                                LlmToolCall {
                                    id: item["id"].as_str().unwrap_or("").to_owned(),
                                    name: func["name"].as_str().unwrap_or("").to_owned(),
                                    arguments: args,
                                }
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                let tool_calls = finalize_tool_calls(raw_tools);
                if visible_content.trim().is_empty() && tool_calls.is_empty() {
                    return Err(LlmTurnError::Other(Error(
                        StatusCode::BAD_GATEWAY,
                        "empty LLM completion",
                    )));
                }
                Ok(LlmTurnResponse {
                    content: visible_content.trim().to_owned(),
                    tool_calls,
                })
            }
        }
    }
}

fn map_http_error(status: u16) -> Error {
    let msg = match status {
        401 | 403 => "LLM 认证失败：API Key 无效或无权限",
        404 => "LLM 接口地址或模型名不存在",
        400 | 422 => "LLM 拒绝了请求：模型可能不支持该输入（如图片）或参数",
        429 => "LLM 限流或额度不足",
        _ => "LLM provider returned error",
    };
    Error(StatusCode::BAD_GATEWAY, msg)
}

fn consume_sse_line<F: FnMut(&str)>(
    raw_line: &str,
    filter: &mut ThinkStreamFilter,
    emit: &mut F,
    tool_acc: &mut BTreeMap<usize, LlmToolCall>,
) {
    let line = raw_line.trim();
    if line.is_empty() || line.starts_with(':') {
        return;
    }
    let Some(payload) = line.strip_prefix("data:") else {
        return;
    };
    let data = payload.trim();
    if data.is_empty() || data == "[DONE]" {
        return;
    }
    let Ok(val) = serde_json::from_str::<Value>(data) else {
        return;
    };
    let delta = &val["choices"][0]["delta"];
    if let Some(piece) = delta["content"].as_str() {
        filter.push(piece, emit);
    }
    if let Some(tcs) = delta["tool_calls"].as_array() {
        for (fallback_idx, tc) in tcs.iter().enumerate() {
            let idx = tc["index"]
                .as_u64()
                .and_then(|n| usize::try_from(n).ok())
                .unwrap_or(fallback_idx);
            let entry = tool_acc.entry(idx).or_insert_with(|| LlmToolCall {
                id: String::new(),
                name: String::new(),
                arguments: String::new(),
            });
            if let Some(id) = tc["id"].as_str() {
                if !id.is_empty() && entry.id.is_empty() {
                    entry.id = id.to_owned();
                }
            }
            if let Some(name) = tc["function"]["name"].as_str() {
                entry.name.push_str(name);
            }
            if let Some(args) = tc["function"]["arguments"].as_str() {
                entry.arguments.push_str(args);
            }
        }
    }
}

fn finalize_tool_calls(calls: Vec<LlmToolCall>) -> Vec<LlmToolCall> {
    calls
        .into_iter()
        .enumerate()
        .filter(|(_, tc)| !tc.name.trim().is_empty())
        .map(|(idx, mut tc)| {
            tc.name = tc.name.trim().to_owned();
            if tc.id.trim().is_empty() {
                tc.id = format!("call_{idx}");
            }
            tc
        })
        .collect()
}

#[derive(Default)]
struct StubNodeArgs<'a> {
    agent: Option<&'a str>,
    role: Option<&'a str>,
    model: Option<&'a str>,
    effort: Option<&'a str>,
    sandbox: Option<&'a str>,
    thread_id: Option<&'a str>,
    node_id: Option<&'a str>,
    instruction: &'a str,
}

fn parse_stub_node_flags(mut rest: &str) -> StubNodeArgs<'_> {
    let mut out = StubNodeArgs::default();
    loop {
        rest = rest.trim_start();
        let flag = [
            "--agent ",
            "--role ",
            "--model ",
            "--effort ",
            "--sandbox ",
            "--thread ",
            "--node ",
        ]
        .into_iter()
        .find(|f| rest.starts_with(f));
        let Some(flag) = flag else {
            break;
        };
        let after = rest[flag.len()..].trim_start();
        let Some((val, tail)) = after.split_once(char::is_whitespace) else {
            break;
        };
        let val = val.trim();
        if val.is_empty() {
            break;
        }
        match flag {
            "--agent " => out.agent = Some(val),
            "--role " => out.role = Some(val),
            "--model " => out.model = Some(val),
            "--effort " => out.effort = Some(val),
            "--sandbox " => out.sandbox = Some(val),
            "--thread " => out.thread_id = Some(val),
            "--node " => out.node_id = Some(val),
            _ => {}
        }
        rest = tail;
    }
    out.instruction = rest.trim();
    out
}

fn split_stub_tokens(input: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    for ch in input.chars() {
        match quote {
            Some(q) if ch == q => quote = None,
            Some(_) => cur.push(ch),
            None if matches!(ch, '"' | '\'') => quote = Some(ch),
            None if ch.is_whitespace() => {
                if !cur.is_empty() {
                    tokens.push(std::mem::take(&mut cur));
                }
            }
            None => cur.push(ch),
        }
    }
    if !cur.is_empty() {
        tokens.push(cur);
    }
    tokens
}

fn parse_stub_role_cmd(rest: &str) -> Option<(String, Value)> {
    let tokens = split_stub_tokens(rest.trim());
    let first = tokens.first()?.trim();
    if first.is_empty() {
        return None;
    }
    if first == "delete" {
        let name = tokens.get(1)?.trim().to_owned();
        if name.is_empty() {
            return None;
        }
        return Some((
            format!("已删除军团角色 {name}"),
            json!({"action": "delete", "name": name}),
        ));
    }
    let name = first.to_owned();
    let mut args = json!({"action": "upsert", "name": name});
    for tok in tokens.iter().skip(1) {
        if let Some((k, v)) = tok.split_once('=') {
            let key = match k.trim() {
                "agent" => "agent",
                "model" => "model",
                "effort" => "effort",
                "sandbox" => "sandbox",
                "node" | "node_id" => "node_id",
                "desc" | "description" => "description",
                "prompt" => "prompt",
                _ => continue,
            };
            args[key] = json!(v.trim());
        }
    }
    Some((format!("已配置军团角色 {name}"), args))
}

fn stub_turn<F: FnMut(&str)>(
    messages: &[Value],
    stream: bool,
    on_delta: &mut F,
) -> std::result::Result<LlmTurnResponse, LlmTurnError> {
    let Some(last) = messages.last() else {
        return Err(LlmTurnError::Other(Error(
            StatusCode::BAD_GATEWAY,
            "empty stub messages",
        )));
    };
    let last_role = last["role"].as_str().unwrap_or("");
    if last_role == "tool" {
        let mut idx = messages.len();
        while idx > 0 && messages[idx - 1]["role"].as_str() == Some("tool") {
            idx -= 1;
        }
        let first_obs = messages[idx]["content"].as_str().unwrap_or("");
        let first_line = first_obs
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap_or("");
        let snippet: String = first_line.chars().take(160).collect();
        let reply = format!("已根据工具结果整理：{snippet}");
        emit_stub_stream(stream, &reply, on_delta);
        return Ok(LlmTurnResponse {
            content: reply,
            tool_calls: Vec::new(),
        });
    }

    let (user_text, images) = extract_user_text_and_images(last);
    let trimmed = user_text.trim();
    if trimmed == STUB_FAIL_COMMAND {
        return Err(LlmTurnError::Other(Error(
            StatusCode::BAD_GATEWAY,
            "LLM provider returned error",
        )));
    }

    let resp = if images > 0 {
        LlmTurnResponse {
            content: format!("（看到 {images} 张图片）收到你的消息：{trimmed}"),
            tool_calls: Vec::new(),
        }
    } else if trimmed.starts_with(TASK_CALLBACK_MARKER) {
        let mut tool_calls = Vec::new();
        if trimmed.contains("账本") {
            tool_calls.push(LlmToolCall {
                id: "call_stub_cb_1".into(),
                name: "update_ledger".into(),
                arguments: json!({
                    "action": "upsert",
                    "title": "自动跟进事项",
                    "notes_append": "节点已返回结果并完成验收"
                })
                .to_string(),
            });
        }
        LlmTurnResponse {
            content: "已验收节点结果并完成跟进。".into(),
            tool_calls,
        }
    } else if trimmed.starts_with("/gitlab") {
        LlmTurnResponse {
            content: "我来查一下 GitLab 提交记录。".into(),
            tool_calls: vec![LlmToolCall {
                id: "call_stub_1".into(),
                name: "fetch_gitlab_commits".into(),
                arguments: "{}".into(),
            }],
        }
    } else if let Some(rest) = trimmed.strip_prefix("/node_cb ") {
        let (cb, instr) = rest.split_once("::").unwrap_or(("总结节点执行结果", rest));
        let cb = cb.trim();
        let instr = instr.trim();
        LlmTurnResponse {
            content: format!("已将任务派发给在线执行节点（完成后自动验收）：{instr}"),
            tool_calls: vec![LlmToolCall {
                id: "call_stub_1".into(),
                name: "dispatch_node".into(),
                arguments: json!({"instruction": instr, "title": instr, "on_complete": cb})
                    .to_string(),
            }],
        }
    } else if let Some(rest) = trimmed.strip_prefix("/node_thread ") {
        let mut parts = rest.splitn(2, char::is_whitespace);
        let tid = parts.next().unwrap_or("").trim();
        let instr = parts.next().unwrap_or("").trim();
        if !tid.is_empty() && !instr.is_empty() {
            LlmTurnResponse {
                content: format!("已沿用上下文线程 {tid} 派发任务：{instr}"),
                tool_calls: vec![LlmToolCall {
                    id: "call_stub_1".into(),
                    name: "dispatch_node".into(),
                    arguments: json!({"thread_id": tid, "instruction": instr, "title": instr})
                        .to_string(),
                }],
            }
        } else {
            LlmTurnResponse {
                content: format!("收到你的消息：{trimmed}"),
                tool_calls: Vec::new(),
            }
        }
    } else if let Some(rest) = trimmed.strip_prefix("/role ") {
        if let Some((reply, args)) = parse_stub_role_cmd(rest) {
            LlmTurnResponse {
                content: reply,
                tool_calls: vec![LlmToolCall {
                    id: "call_stub_role_1".into(),
                    name: "manage_role".into(),
                    arguments: args.to_string(),
                }],
            }
        } else {
            LlmTurnResponse {
                content: format!("收到你的消息：{trimmed}"),
                tool_calls: Vec::new(),
            }
        }
    } else if let Some(rest) = trimmed.strip_prefix("/node ") {
        let parsed = parse_stub_node_flags(rest);
        let mut args = json!({"instruction": parsed.instruction, "title": parsed.instruction});
        if let Some(ag) = parsed.agent {
            args["agent"] = json!(ag);
        }
        if let Some(r) = parsed.role {
            args["role"] = json!(r);
        }
        if let Some(m) = parsed.model {
            args["model"] = json!(m);
        }
        if let Some(e) = parsed.effort {
            args["effort"] = json!(e);
        }
        if let Some(sb) = parsed.sandbox {
            args["sandbox"] = json!(sb);
        }
        if let Some(tid) = parsed.thread_id {
            args["thread_id"] = json!(tid);
        }
        if let Some(nid) = parsed.node_id {
            args["node_id"] = json!(nid);
        }
        LlmTurnResponse {
            content: format!("已将任务派发给在线执行节点：{}", parsed.instruction),
            tool_calls: vec![LlmToolCall {
                id: "call_stub_1".into(),
                name: "dispatch_node".into(),
                arguments: args.to_string(),
            }],
        }
    } else if let Some(rest) = trimmed.strip_prefix("/remember ") {
        LlmTurnResponse {
            content: format!("好的，已写入长期记忆：{rest}"),
            tool_calls: vec![LlmToolCall {
                id: "call_stub_1".into(),
                name: "update_memory".into(),
                arguments: json!({"memory_append": rest}).to_string(),
            }],
        }
    } else if let Some(rest) = trimmed.strip_prefix("/ledger ") {
        LlmTurnResponse {
            content: format!("已更新专项活账本：{rest}"),
            tool_calls: vec![LlmToolCall {
                id: "call_stub_1".into(),
                name: "update_ledger".into(),
                arguments: json!({"action": "upsert", "title": rest, "notes_append": rest})
                    .to_string(),
            }],
        }
    } else if let Some(rest) = trimmed.strip_prefix("/cron ") {
        LlmTurnResponse {
            content: format!("已创建定时任务：{rest}"),
            tool_calls: vec![LlmToolCall {
                id: "call_stub_1".into(),
                name: "manage_cron".into(),
                arguments: json!({"action": "add", "interval_minutes": 60, "instruction": rest})
                    .to_string(),
            }],
        }
    } else if let Some(rest) = trimmed.strip_prefix('@') {
        let mut parts = rest.splitn(2, char::is_whitespace);
        let peer = parts.next().unwrap_or("").trim();
        let body = parts.next().unwrap_or("").trim();
        if !peer.is_empty() && !body.is_empty() {
            LlmTurnResponse {
                content: format!("已转告 @{peer}：{body}"),
                tool_calls: vec![LlmToolCall {
                    id: "call_stub_1".into(),
                    name: "delegate_colleague".into(),
                    arguments: json!({"peer": peer, "instruction": body}).to_string(),
                }],
            }
        } else {
            LlmTurnResponse {
                content: format!("收到你的消息：{trimmed}"),
                tool_calls: Vec::new(),
            }
        }
    } else {
        LlmTurnResponse {
            content: format!("收到你的消息：{trimmed}"),
            tool_calls: Vec::new(),
        }
    };

    emit_stub_stream(stream, &resp.content, on_delta);
    Ok(resp)
}

fn emit_stub_stream<F: FnMut(&str)>(stream: bool, text: &str, on_delta: &mut F) {
    if !stream || text.is_empty() {
        return;
    }
    let mut buf = String::new();
    for ch in text.chars() {
        buf.push(ch);
        if buf.chars().count() >= 12 {
            on_delta(&buf);
            buf.clear();
        }
    }
    if !buf.is_empty() {
        on_delta(&buf);
    }
}

fn extract_user_text_and_images(msg: &Value) -> (String, usize) {
    match &msg["content"] {
        Value::String(s) => (s.clone(), 0),
        Value::Array(parts) => {
            let mut text = String::new();
            let mut images = 0;
            for p in parts {
                match p["type"].as_str() {
                    Some("text") => {
                        if let Some(t) = p["text"].as_str() {
                            text.push_str(t);
                        }
                    }
                    Some("image_url") => images += 1,
                    _ => {}
                }
            }
            (text, images)
        }
        _ => (String::new(), 0),
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
    if let Some(rest) = trimmed.strip_prefix("/role ") {
        if let Some((reply, mut args)) = parse_stub_role_cmd(rest) {
            args["tool"] = json!("manage_role");
            return json!({
                "reply": reply,
                "tools": [args]
            })
            .to_string();
        }
    }
    if let Some(rest) = trimmed.strip_prefix("/node ") {
        let parsed = parse_stub_node_flags(rest);
        let mut call = json!({"tool": "dispatch_node", "instruction": parsed.instruction, "title": parsed.instruction});
        if let Some(ag) = parsed.agent {
            call["agent"] = json!(ag);
        }
        if let Some(r) = parsed.role {
            call["role"] = json!(r);
        }
        if let Some(m) = parsed.model {
            call["model"] = json!(m);
        }
        if let Some(e) = parsed.effort {
            call["effort"] = json!(e);
        }
        if let Some(sb) = parsed.sandbox {
            call["sandbox"] = json!(sb);
        }
        if let Some(tid) = parsed.thread_id {
            call["thread_id"] = json!(tid);
        }
        if let Some(nid) = parsed.node_id {
            call["node_id"] = json!(nid);
        }
        return json!({
            "reply": format!("已将任务派发给在线执行节点：{}", parsed.instruction),
            "tools": [call]
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

#[cfg(test)]
mod tests {
    use super::ThinkStreamFilter;

    #[test]
    fn think_stream_filter_strips_split_tags_and_preserves_multibyte_utf8() {
        let mut filter = ThinkStreamFilter::default();
        let mut streamed = String::new();
        for chunk in [
            "<thi",
            "nk>内部推理中文",
            "内容</thi",
            "nk>\n你好，",
            "世界！",
            "<t",
            "not-think",
        ] {
            filter.push(chunk, &mut |s| streamed.push_str(s));
        }
        filter.finish(&mut |s| streamed.push_str(s));
        assert_eq!(streamed, "\n你好，世界！<tnot-think");
    }
}
