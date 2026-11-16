use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};
use std::time::Duration;

use serde_json::{Value, json};

use crate::config::Config;
use crate::session::{Message, ToolCall, Usage};
use std::sync::mpsc::{self, RecvTimeoutError};

use crate::{Error, Result};

/// 流式过程中对外抛出的增量事件。
pub enum Event {
    Text(String),
    Reasoning(String),
    /// 开始执行某个工具，`summary` 是一行人类可读的简述。
    ToolCall {
        name: String,
        summary: String,
    },
    /// 工具结束。`note` 为空表示成功且无需打印；`failed` 决定界面用失败样式还是普通样式。
    ToolResult {
        name: String,
        note: String,
        failed: bool,
    },
    /// 给用户看的一行提示（压缩完成、压缩失败等）。
    Notice(String),
    /// 时间过去了，不代表有内容：界面用它刷新处理中的状态行。
    Tick,
}

pub struct Reply {
    pub content: String,
    pub reasoning: Option<String>,
    pub tool_calls: Vec<ToolCall>,
    pub usage: Usage,
}

pub struct Provider {
    agent: ureq::Agent,
    url: String,
    api_key: String,
    model: String,
    /// 会话标识，随每个请求发出去。
    session: String,
}

impl Provider {
    /// `session` 是会话标识：opencode-go 要求每个会话带一个稳定的
    /// `x-opencode-session` 头，否则直接 400（MissingSessionID）。
    pub fn new(cfg: &Config, session: &str) -> Provider {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            // 启动时解析好的代理；`None` 就是直连。
            .proxy(cfg.proxy.clone())
            // 供应商要求客户端自报家门，而不是露出 HTTP 库的名字。
            .user_agent(format!("yi/{}", env!("CARGO_PKG_VERSION")))
            .timeout_connect(Some(Duration::from_secs(10)))
            .timeout_per_read(Some(Duration::from_secs(120)))
            .build()
            .into();
        Provider {
            agent,
            url: format!("{}/chat/completions", cfg.base_url.trim_end_matches('/')),
            api_key: cfg.api_key.clone(),
            model: cfg.model.clone(),
            session: session.to_string(),
        }
    }

    pub fn stream(
        &self,
        system: &str,
        summary: Option<&str>,
        messages: &[&Message],
        tools: &Value,
        effort: Option<&str>,
        on: &mut dyn FnMut(Event),
    ) -> Result<Reply> {
        let body = build_body(&self.model, system, summary, messages, tools, effort);
        if crate::cancel::requested() {
            return Err(Error::Cancelled);
        }

        // 请求和读响应都在工作线程里：等响应头、等正文都可能持续几十秒，
        // 主线程每 100 毫秒轮询一次取消标志，模型「思考中」也能立刻停下。
        let agent = self.agent.clone();
        let url = self.url.clone();
        let api_key = self.api_key.clone();
        let session = self.session.clone();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let result = send_with_retry(&agent, &url, &api_key, &session, &body)
                .and_then(|resp| read_stream(resp, &tx));
            let item = match result {
                Ok(reply) => StreamItem::Done(reply),
                Err(err) => StreamItem::Failed(err),
            };
            let _ = tx.send(item);
        });

        loop {
            if crate::cancel::check() {
                return Err(Error::Cancelled);
            }
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(StreamItem::Event(event)) => on(event),
                Ok(StreamItem::Done(reply)) => return Ok(reply),
                Ok(StreamItem::Failed(err)) => return Err(err),
                // 空转一次：界面靠这个刷新「等待模型 3.2s」这类状态行。
                Err(RecvTimeoutError::Timeout) => on(Event::Tick),
                // 读线程没了却没给结果，只可能是它自己 panic 了。
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(Error::Http("流式响应中断".to_string()));
                }
            }
        }
    }

    /// 非流式的一次性补全，只用于生成压缩摘要。
    pub fn complete(&self, system: &str, prompt: &str) -> Result<String> {
        let body = json!({
            "model": self.model,
            "messages": [
                { "role": "system", "content": system },
                { "role": "user", "content": prompt },
            ],
            "stream": false,
        });
        let resp = send_with_retry(&self.agent, &self.url, &self.api_key, &self.session, &body)?;
        let text = resp
            .into_body()
            .read_to_string()
            .map_err(|e| Error::Http(e.to_string()))?;
        let value: Value = serde_json::from_str(&text)?;
        value["choices"][0]["message"]["content"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| Error::Api(format!("压缩请求没有返回内容：{}", truncate(&text, 300))))
    }
}

fn send_with_retry(
    agent: &ureq::Agent,
    url: &str,
    api_key: &str,
    session: &str,
    body: &Value,
) -> Result<ureq::http::Response<ureq::Body>> {
    let delays = [1u64, 2, 4];
    for attempt in 0..=delays.len() {
        let sent = agent
            .post(url)
            .header("Authorization", &format!("Bearer {api_key}"))
            .header("Content-Type", "application/json")
            // DeepSeek 官方会忽略这个头；opencode-go 缺了它直接 400。
            .header("x-opencode-session", session)
            .send_json(body);

        match sent {
            Ok(resp) => {
                let code = resp.status().as_u16();
                if (200..300).contains(&code) {
                    return Ok(resp);
                }
                let text = resp
                    .into_body()
                    .read_to_string()
                    .unwrap_or_else(|_| String::new());
                let retryable = code == 429 || (500..600).contains(&code);
                if retryable && attempt < delays.len() {
                    std::thread::sleep(Duration::from_secs(delays[attempt]));
                    continue;
                }
                return Err(Error::Api(format!("HTTP {code}：{}", truncate(&text, 800))));
            }
            Err(e) => {
                if attempt < delays.len() {
                    std::thread::sleep(Duration::from_secs(delays[attempt]));
                    continue;
                }
                return Err(Error::Http(e.to_string()));
            }
        }
    }
    Err(Error::Http("重试三次仍失败".to_string()))
}

fn build_body(
    model: &str,
    system: &str,
    summary: Option<&str>,
    messages: &[&Message],
    tools: &Value,
    effort: Option<&str>,
) -> Value {
    let mut wire = Vec::with_capacity(messages.len() + 2);
    wire.push(json!({ "role": "system", "content": system }));
    if let Some(summary) = summary {
        wire.push(json!({
            "role": "user",
            "content": format!("<compaction-summary>\n{summary}\n</compaction-summary>"),
        }));
    }

    for message in messages {
        match *message {
            Message::User { content } => {
                wire.push(json!({ "role": "user", "content": content }));
            }
            Message::Assistant {
                content,
                reasoning,
                tool_calls,
                ..
            } => {
                let mut value = json!({ "role": "assistant", "content": content });
                if let Some(reasoning) = reasoning
                    && !reasoning.is_empty()
                {
                    value["reasoning_content"] = json!(reasoning);
                }
                if !tool_calls.is_empty() {
                    let calls: Vec<Value> = tool_calls
                        .iter()
                        .map(|call| {
                            json!({
                                "id": call.id,
                                "type": "function",
                                "function": { "name": call.name, "arguments": call.arguments },
                            })
                        })
                        .collect();
                    value["tool_calls"] = Value::Array(calls);
                }
                wire.push(value);
            }
            Message::Image { path } => match image_data_url(path) {
                Some(url) => wire.push(json!({
                    "role": "user",
                    "content": [
                        { "type": "text", "text": format!("[图片] {path}") },
                        { "type": "image_url", "image_url": { "url": url } },
                    ],
                })),
                None => wire.push(json!({
                    "role": "user",
                    "content": format!("[图片 {path} 读不到了]"),
                })),
            },
            Message::Tool {
                tool_call_id,
                content,
            } => {
                wire.push(json!({
                    "role": "tool",
                    "tool_call_id": tool_call_id,
                    "content": content,
                }));
            }
        }
    }

    let mut body = json!({
        "model": model,
        "messages": wire,
        "stream": true,
        "stream_options": { "include_usage": true },
    });
    if tools.as_array().is_some_and(|list| !list.is_empty()) {
        body["tools"] = tools.clone();
    }
    if let Some(effort) = effort {
        body["reasoning_effort"] = json!(effort);
    }
    body
}

enum StreamItem {
    Event(Event),
    Done(Reply),
    Failed(Error),
}

fn read_stream(
    resp: ureq::http::Response<ureq::Body>,
    tx: &mpsc::Sender<StreamItem>,
) -> Result<Reply> {
    let mut content = String::new();
    let mut reasoning = String::new();
    let mut usage = Usage::default();
    // 按 index 累积工具调用的 id / 名称 / 参数片段。
    let mut partial: BTreeMap<u64, ToolCall> = BTreeMap::new();

    let mut reader = BufReader::new(resp.into_body().into_reader());
    let mut line = String::new();
    loop {
        if crate::cancel::requested() {
            return Err(Error::Cancelled);
        }
        line.clear();
        let read = reader
            .read_line(&mut line)
            .map_err(|e| Error::Http(e.to_string()))?;
        if read == 0 {
            break;
        }
        let trimmed = line.trim();
        let Some(data) = trimmed.strip_prefix("data:") else {
            continue;
        };
        let data = data.trim();
        if data.is_empty() {
            continue;
        }
        if data == "[DONE]" {
            break;
        }
        let Ok(chunk) = serde_json::from_str::<Value>(data) else {
            continue;
        };

        if let Some(delta) = chunk
            .get("choices")
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("delta"))
        {
            if let Some(text) = delta.get("content").and_then(Value::as_str)
                && !text.is_empty()
            {
                content.push_str(text);
                if tx
                    .send(StreamItem::Event(Event::Text(text.to_string())))
                    .is_err()
                {
                    // 主线程已经取消，读线程没有继续读的必要。
                    return Err(Error::Cancelled);
                }
            }
            if let Some(text) = delta.get("reasoning_content").and_then(Value::as_str)
                && !text.is_empty()
            {
                reasoning.push_str(text);
                if tx
                    .send(StreamItem::Event(Event::Reasoning(text.to_string())))
                    .is_err()
                {
                    return Err(Error::Cancelled);
                }
            }
            if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
                for call in calls {
                    let index = call.get("index").and_then(Value::as_u64).unwrap_or(0);
                    let slot = partial.entry(index).or_insert_with(|| ToolCall {
                        id: String::new(),
                        name: String::new(),
                        arguments: String::new(),
                    });
                    if let Some(id) = call.get("id").and_then(Value::as_str) {
                        slot.id = id.to_string();
                    }
                    if let Some(function) = call.get("function") {
                        if let Some(name) = function.get("name").and_then(Value::as_str) {
                            slot.name.push_str(name);
                        }
                        if let Some(args) = function.get("arguments").and_then(Value::as_str) {
                            slot.arguments.push_str(args);
                        }
                    }
                }
            }
        }

        if let Some(raw) = chunk.get("usage").filter(|u| !u.is_null()) {
            usage = parse_usage(raw);
        }
    }

    let tool_calls: Vec<ToolCall> = partial
        .into_values()
        .filter(|call| !call.name.is_empty())
        .collect();

    Ok(Reply {
        content,
        reasoning: if reasoning.is_empty() {
            None
        } else {
            Some(reasoning)
        },
        tool_calls,
        usage,
    })
}

fn parse_usage(raw: &Value) -> Usage {
    let cache_hit = raw
        .get("prompt_cache_hit_tokens")
        .and_then(Value::as_u64)
        .or_else(|| {
            raw.get("prompt_tokens_details")
                .and_then(|d| d.get("cached_tokens"))
                .and_then(Value::as_u64)
        })
        .unwrap_or(0);
    let prompt_total = raw
        .get("prompt_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(cache_hit);
    let input_uncached = raw
        .get("prompt_cache_miss_tokens")
        .and_then(Value::as_u64)
        .unwrap_or_else(|| prompt_total.saturating_sub(cache_hit));
    Usage {
        input_uncached,
        cache_hit,
        output: raw
            .get("completion_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        reasoning: raw
            .get("completion_tokens_details")
            .and_then(|d| d.get("reasoning_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0),
        elapsed_ms: 0,
        context_prompt: prompt_total,
    }
}

fn truncate(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

/// 把本地图片读成 `data:` URL；读不到就返回 `None`，由调用方降级成文本。
fn image_data_url(path: &str) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    if bytes.len() > 20 * 1024 * 1024 {
        return None;
    }
    let mime = match std::path::Path::new(path)
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        _ => return None,
    };
    Some(format!("data:{mime};base64,{}", base64(&bytes)))
}

fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0];
        let b1 = *chunk.get(1).unwrap_or(&0);
        let b2 = *chunk.get(2).unwrap_or(&0);
        out.push(TABLE[(b0 >> 2) as usize] as char);
        out.push(TABLE[(((b0 & 0b11) << 4) | (b1 >> 4)) as usize] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(((b1 & 0b1111) << 2) | (b2 >> 6)) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[(b2 & 0b11_1111) as usize] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::truncate;

    #[test]
    fn truncate_never_splits_a_character() {
        // 79 个 ASCII 字节后是「中」的首字节：上限 80 落在字中间，必须退回到 79。
        let text = format!("{}中{}", "a".repeat(79), "b".repeat(20));
        assert_eq!(truncate(&text, 80), format!("{}…", "a".repeat(79)));

        // 上限正好落在「中」之后（82），可以完整保留它。
        let kept = truncate(&text, 82);
        assert!(kept.starts_with(&format!("{}中", "a".repeat(79))), "{kept}");
    }
}
