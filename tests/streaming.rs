//! 里程碑 ① 的集成测试：唯一接缝是 `Agent::submit`，供应商走真 HTTP 打到本地假服务器。
//!
//! 断言只看外部可见的东西：累积出来的文本、会话 JSONL、假服务器收到的请求体。

use parking_lot::Mutex;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use yi::agent::Agent;
use yi::config::Config;
use yi::provider::Event;

struct FakeServer {
    port: u16,
    bodies: Arc<Mutex<Vec<Value>>>,
}

/// 按给定顺序回应每个请求，并把收到的请求体留档。
fn start_server(responses: Vec<String>) -> FakeServer {
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

fn sse_response(content: &[&str], reasoning: &[&str], usage: Value) -> String {
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

fn plain_response(content: &[&str]) -> String {
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

fn temp_home(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let dir = std::env::temp_dir().join(format!("yi-{tag}-{}", nanos));
    std::fs::create_dir_all(&dir).expect("建临时目录");
    dir
}

fn config(port: u16, home: &Path, effort: Option<&str>) -> Config {
    Config {
        api_key: "test-key".to_string(),
        base_url: format!("http://127.0.0.1:{port}"),
        model: "deepseek-flash".to_string(),
        effort: effort.map(str::to_string),
        home: home.to_path_buf(),
    }
}

fn session_path(home: &Path) -> PathBuf {
    home.join("sessions").join("proj").join("session.jsonl")
}

fn collect(agent: &mut Agent, prompt: &str) -> (String, Vec<String>) {
    let mut text = String::new();
    let mut reasoning = Vec::new();
    agent
        .submit(prompt, &mut |event| match event {
            Event::Text(piece) => text.push_str(&piece),
            Event::Reasoning(piece) => reasoning.push(piece),
        })
        .expect("本轮对话成功");
    (text, reasoning)
}

#[test]
fn streams_text_and_records_the_session() {
    let server = start_server(vec![plain_response(&["Hello", " world"])]);
    let home = temp_home("stream");
    let mut agent = Agent::new(
        &config(server.port, &home, None),
        "/tmp/proj",
        &session_path(&home),
    )
    .expect("打开会话");

    let (text, reasoning) = collect(&mut agent, "hi");
    assert_eq!(text, "Hello world");
    assert!(reasoning.is_empty());

    let request = server.bodies.lock()[0].clone();
    assert_eq!(request["model"], "deepseek-flash");
    assert_eq!(request["messages"][0]["role"], "system");
    assert_eq!(request["messages"][1]["content"], "hi");

    let raw = std::fs::read_to_string(session_path(&home)).expect("读会话文件");
    let lines: Vec<&str> = raw.lines().filter(|l| !l.trim().is_empty()).collect();
    assert_eq!(lines.len(), 3, "头 + 用户 + 助手：{raw}");
    let header: Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(header["type"], "session");
    assert_eq!(header["version"].as_u64(), Some(1));
    let assistant: Value = serde_json::from_str(lines[2]).unwrap();
    assert_eq!(assistant["message"]["role"], "assistant");
    assert_eq!(assistant["message"]["content"], "Hello world");
    assert_eq!(
        assistant["message"]["usage"]["cache_hit"].as_u64(),
        Some(100)
    );
    assert_eq!(
        assistant["message"]["usage"]["input_uncached"].as_u64(),
        Some(20)
    );
}

#[test]
fn second_turn_reuses_the_prefix_byte_for_byte() {
    let server = start_server(vec![
        plain_response(&["first"]),
        plain_response(&["second"]),
    ]);
    let home = temp_home("prefix");
    let mut agent = Agent::new(
        &config(server.port, &home, None),
        "/tmp/proj",
        &session_path(&home),
    )
    .expect("打开会话");

    collect(&mut agent, "one");
    collect(&mut agent, "two");

    let bodies = server.bodies.lock();
    let first = &bodies[0]["messages"];
    let second = &bodies[1]["messages"];
    assert_eq!(second[0], first[0], "系统提示词必须逐字节一致");
    assert_eq!(second[1], first[1], "历史用户消息必须逐字节一致");
    assert_eq!(second[2]["role"], "assistant");
    assert_eq!(second[2]["content"], "first");
    assert_eq!(second[3]["content"], "two");
}

#[test]
fn effort_is_sent_only_when_set() {
    let server = start_server(vec![plain_response(&["a"]), plain_response(&["b"])]);

    let with_effort_home = temp_home("effort-on");
    let mut low = Agent::new(
        &config(server.port, &with_effort_home, Some("low")),
        "/tmp/proj",
        &session_path(&with_effort_home),
    )
    .expect("打开会话");
    collect(&mut low, "hi");

    let auto_home = temp_home("effort-auto");
    let mut auto = Agent::new(
        &config(server.port, &auto_home, None),
        "/tmp/proj",
        &session_path(&auto_home),
    )
    .expect("打开会话");
    collect(&mut auto, "hi");

    let bodies = server.bodies.lock();
    assert_eq!(bodies[0]["reasoning_effort"], "low");
    assert!(
        bodies[1].get("reasoning_effort").is_none(),
        "auto 不应发送 reasoning_effort：{}",
        bodies[1]
    );
}

#[test]
fn reasoning_streams_separately_from_content() {
    let server = start_server(vec![sse_response(
        &["answer"],
        &["thinking", " harder"],
        json!({"prompt_tokens": 10, "completion_tokens": 2}),
    )]);
    let home = temp_home("reasoning");
    let mut agent = Agent::new(
        &config(server.port, &home, None),
        "/tmp/proj",
        &session_path(&home),
    )
    .expect("打开会话");

    let (text, reasoning) = collect(&mut agent, "hi");
    assert_eq!(text, "answer");
    assert_eq!(reasoning.concat(), "thinking harder");
}

#[test]
fn api_error_is_surfaced_without_retrying() {
    let response = "HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{\"error\":{\"message\":\"bad key\"}}".to_string();
    let server = start_server(vec![response]);
    let home = temp_home("error");
    let mut agent = Agent::new(
        &config(server.port, &home, None),
        "/tmp/proj",
        &session_path(&home),
    )
    .expect("打开会话");

    let err = agent.submit("hi", &mut |_| {}).expect_err("401 必须报错");
    let message = err.to_string();
    assert!(message.contains("401"), "错误里应带状态码：{message}");
    assert!(message.contains("bad key"), "错误里应带响应体：{message}");
}

#[test]
fn a_second_instance_in_the_same_project_is_refused() {
    let home = temp_home("lock");
    let path = session_path(&home);
    let first = Agent::new(&config(1, &home, None), "/tmp/proj", &path).expect("第一个实例");
    let second = Agent::new(&config(1, &home, None), "/tmp/proj", &path);
    assert!(second.is_err(), "同一项目不允许两个实例");
    drop(first);
}
