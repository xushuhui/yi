use std::io::{BufRead, BufReader};
use std::time::Duration;

use serde_json::{Value, json};

use crate::config::Config;
use crate::session::{Message, Usage};
use crate::{Error, Result};

/// 流式过程中对外抛出的增量事件。
pub enum Event {
    Text(String),
    Reasoning(String),
}

pub struct Reply {
    pub content: String,
    pub reasoning: Option<String>,
    pub usage: Usage,
}

pub struct Provider {
    agent: ureq::Agent,
    url: String,
    api_key: String,
    model: String,
}

impl Provider {
    pub fn new(cfg: &Config) -> Provider {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_connect(Some(Duration::from_secs(10)))
            .timeout_per_read(Some(Duration::from_secs(120)))
            .build()
            .into();
        Provider {
            agent,
            url: format!("{}/chat/completions", cfg.base_url.trim_end_matches('/')),
            api_key: cfg.api_key.clone(),
            model: cfg.model.clone(),
        }
    }

    pub fn stream(
        &self,
        system: &str,
        messages: &[Message],
        effort: Option<&str>,
        on: &mut dyn FnMut(Event),
    ) -> Result<Reply> {
        let body = build_body(&self.model, system, messages, effort);
        let resp = self.send_with_retry(&body)?;
        read_stream(resp, on)
    }

    /// 连接失败、5xx、429 退避重试三次；其余状态立即报错。
    fn send_with_retry(&self, body: &Value) -> Result<ureq::http::Response<ureq::Body>> {
        let mut delay = Duration::from_secs(1);
        for attempt in 0..3 {
            let sent = self
                .agent
                .post(&self.url)
                .header("Authorization", &format!("Bearer {}", self.api_key))
                .header("Content-Type", "application/json")
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
                    if retryable && attempt < 2 {
                        std::thread::sleep(delay);
                        delay *= 2;
                        continue;
                    }
                    return Err(Error::Api(format!("HTTP {code}：{}", truncate(&text, 800))));
                }
                Err(e) => {
                    if attempt < 2 {
                        std::thread::sleep(delay);
                        delay *= 2;
                        continue;
                    }
                    return Err(Error::Http(e.to_string()));
                }
            }
        }
        unreachable!("循环最多三次，最后一次一定返回")
    }
}

fn build_body(model: &str, system: &str, messages: &[Message], effort: Option<&str>) -> Value {
    let mut wire = Vec::with_capacity(messages.len() + 1);
    wire.push(json!({ "role": "system", "content": system }));
    for message in messages {
        match message {
            Message::User { content } => {
                wire.push(json!({ "role": "user", "content": content }));
            }
            Message::Assistant {
                content, reasoning, ..
            } => {
                let mut value = json!({ "role": "assistant", "content": content });
                if let Some(reasoning) = reasoning
                    && !reasoning.is_empty()
                {
                    value["reasoning_content"] = json!(reasoning);
                }
                wire.push(value);
            }
        }
    }

    let mut body = json!({
        "model": model,
        "messages": wire,
        "stream": true,
        "stream_options": { "include_usage": true },
    });
    if let Some(effort) = effort {
        body["reasoning_effort"] = json!(effort);
    }
    body
}

fn read_stream(resp: ureq::http::Response<ureq::Body>, on: &mut dyn FnMut(Event)) -> Result<Reply> {
    let mut content = String::new();
    let mut reasoning = String::new();
    let mut usage = Usage::default();

    let mut reader = BufReader::new(resp.into_body().into_reader());
    let mut line = String::new();
    loop {
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
                on(Event::Text(text.to_string()));
            }
            if let Some(text) = delta.get("reasoning_content").and_then(Value::as_str)
                && !text.is_empty()
            {
                reasoning.push_str(text);
                on(Event::Reasoning(text.to_string()));
            }
        }

        if let Some(raw) = chunk.get("usage").filter(|u| !u.is_null()) {
            usage = parse_usage(raw);
        }
    }

    Ok(Reply {
        content,
        reasoning: if reasoning.is_empty() {
            None
        } else {
            Some(reasoning)
        },
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
