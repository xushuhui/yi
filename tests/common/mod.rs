//! 两个集成测试文件共用的小工具：手写 HTTP 假服务器、SSE 响应构造、临时目录。
//!
//! 这里只用标准库 + parking_lot，没有任何网络依赖。
//! 每个测试文件都会编译整份模块，只用到其中一部分辅助函数，因此允许未使用的项。
#![allow(dead_code)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;
use serde_json::{Value, json};
use yi::agent::Agent;
use yi::config::Config;
use yi::provider::Event;

pub struct FakeServer {
    pub port: u16,
    pub bodies: Arc<Mutex<Vec<Value>>>,
}

/// 按给定顺序回应每个请求，并把收到的请求体留档。
pub fn start_server(responses: Vec<String>) -> FakeServer {
    let listener = TcpListener::bind("127.0.0.1:0").expect("绑定本地端口");
    let port = listener.local_addr().expect("取端口").port();
    let bodies = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&bodies);

    thread::spawn(move || {
        for response in responses {
            let Ok((stream, _)) = listener.accept() else {
                return;
            };
            let body = read_request(&stream);
            if let Ok(json) = serde_json::from_str::<Value>(&body) {
                recorded.lock().push(json);
            }
            let mut stream = stream;
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    });

    FakeServer { port, bodies }
}

fn read_request(stream: &TcpStream) -> String {
    let mut reader = BufReader::new(stream);
    let mut content_length = 0usize;
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            break;
        }
        let head = line.trim_end();
        if head.is_empty() {
            break;
        }
        let lower = head.to_ascii_lowercase();
        if let Some(value) = lower.strip_prefix("content-length:") {
            content_length = value.trim().parse().unwrap_or(0);
        }
    }
    let mut body = vec![0u8; content_length];
    let _ = reader.read_exact(&mut body);
    String::from_utf8_lossy(&body).to_string()
}

pub fn sse_response(content: &[&str], reasoning: &[&str], usage: Value) -> String {
    let mut out = String::from(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
    );
    for piece in reasoning {
        out.push_str(&format!(
            "data: {}\n\n",
            json!({"choices": [{"index": 0, "delta": {"reasoning_content": piece}}]})
        ));
    }
    for piece in content {
        out.push_str(&format!(
            "data: {}\n\n",
            json!({"choices": [{"index": 0, "delta": {"content": piece}}]})
        ));
    }
    out.push_str(&format!(
        "data: {}\n\n",
        json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}], "usage": usage})
    ));
    out.push_str("data: [DONE]\n\n");
    out
}

pub fn plain_response(content: &[&str]) -> String {
    sse_response(
        content,
        &[],
        json!({
            "prompt_tokens": 120,
            "completion_tokens": 7,
            "prompt_cache_hit_tokens": 100,
            "prompt_cache_miss_tokens": 20,
        }),
    )
}

/// 一条只调用工具的响应：`calls` 是 (id, 工具名, 参数 JSON)。
pub fn tool_response(calls: &[(&str, &str, Value)]) -> String {
    let mut out = String::from(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
    );
    for (index, (id, name, arguments)) in calls.iter().enumerate() {
        out.push_str(&format!(
            "data: {}\n\n",
            json!({"choices": [{"index": 0, "delta": {"tool_calls": [{
                "index": index,
                "id": id,
                "type": "function",
                "function": { "name": name, "arguments": arguments.to_string() }
            }]}}]})
        ));
    }
    out.push_str(&format!(
        "data: {}\n\n",
        json!({
            "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}],
            "usage": {"prompt_tokens": 50, "completion_tokens": 10, "prompt_cache_hit_tokens": 0, "prompt_cache_miss_tokens": 50}
        })
    ));
    out.push_str("data: [DONE]\n\n");
    out
}

pub fn temp_home(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("yi-{tag}-{nanos}-{unique}"));
    std::fs::create_dir_all(&dir).expect("建临时目录");
    dir
}

pub fn config(port: u16, home: &Path, effort: Option<&str>) -> Config {
    Config {
        api_key: "test-key".to_string(),
        base_url: format!("http://127.0.0.1:{port}"),
        model: "deepseek-flash".to_string(),
        effort: effort.map(str::to_string),
        home: home.to_path_buf(),
        gopls: "gopls".to_string(),
        phpantom: "phpantom_lsp".to_string(),
    }
}

pub fn session_path(home: &Path) -> PathBuf {
    home.join("sessions").join("proj").join("session.jsonl")
}

/// 跑一轮，返回（正文, 思考, 工具摘要）。
pub fn collect(agent: &mut Agent, prompt: &str) -> (String, Vec<String>, Vec<String>) {
    let mut text = String::new();
    let mut reasoning = Vec::new();
    let mut tools = Vec::new();
    agent
        .submit(prompt, &mut |event| match event {
            Event::Text(piece) => text.push_str(&piece),
            Event::Reasoning(piece) => reasoning.push(piece),
            Event::ToolCall { summary, .. } => tools.push(summary),
            // 压缩提示只影响界面显示，断言看会话文件。
            Event::Notice(_) => {}
        })
        .expect("本轮对话成功");
    (text, reasoning, tools)
}

/// 会话文件里所有条目，按行解析成 JSON。
pub fn session_entries(home: &Path) -> Vec<Value> {
    let raw = std::fs::read_to_string(session_path(home)).expect("读会话文件");
    raw.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("会话行是合法 JSON"))
        .collect()
}

/// 非流式响应，供压缩摘要请求使用。
pub fn json_response(content: &str) -> String {
    let body = json!({
        "choices": [{"index": 0, "message": {"role": "assistant", "content": content}}],
        "usage": {"prompt_tokens": 10, "completion_tokens": 5},
    });
    format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{body}")
}

/// 带指定输入用量的流式响应，用来越过压缩阈值。
pub fn big_usage_response(content: &[&str], prompt_tokens: u64) -> String {
    sse_response(
        content,
        &[],
        json!({
            "prompt_tokens": prompt_tokens,
            "completion_tokens": 7,
            "prompt_cache_hit_tokens": 0,
            "prompt_cache_miss_tokens": prompt_tokens,
        }),
    )
}
