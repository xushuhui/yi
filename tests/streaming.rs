//! 里程碑 ① 的集成测试：唯一接缝是 `Agent::submit`，供应商走真 HTTP 打到本地假服务器。
//!
//! 断言只看外部可见的东西：累积出来的文本、会话 JSONL、假服务器收到的请求体。

mod common;

use common::*;
use serde_json::Value;
use yi::agent::Agent;

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

    let (text, reasoning, tools) = collect(&mut agent, "hi");
    assert_eq!(text, "Hello world");
    assert!(reasoning.is_empty());
    assert!(tools.is_empty());

    let request = server.bodies.lock()[0].clone();
    assert_eq!(request["model"], "deepseek-flash");
    assert_eq!(request["messages"][0]["role"], "system");
    assert_eq!(request["messages"][1]["content"], "hi");
    assert!(
        request["tools"].as_array().is_some_and(|t| t.len() == 7),
        "七个工具：read/write/edit/bash/grep/glob/lsp"
    );

    let entries = session_entries(&home);
    assert_eq!(entries.len(), 3, "头 + 用户 + 助手：{entries:?}");
    assert_eq!(entries[0]["type"], "session");
    assert_eq!(entries[0]["version"].as_u64(), Some(1));
    assert_eq!(entries[2]["message"]["role"], "assistant");
    assert_eq!(entries[2]["message"]["content"], "Hello world");
    assert_eq!(
        entries[2]["message"]["usage"]["cache_hit"].as_u64(),
        Some(100)
    );
    assert_eq!(
        entries[2]["message"]["usage"]["input_uncached"].as_u64(),
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
    assert_eq!(bodies[0]["tools"], bodies[1]["tools"], "工具定义必须一致");
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
        serde_json::json!({"prompt_tokens": 10, "completion_tokens": 2}),
    )]);
    let home = temp_home("reasoning");
    let mut agent = Agent::new(
        &config(server.port, &home, None),
        "/tmp/proj",
        &session_path(&home),
    )
    .expect("打开会话");

    let (text, reasoning, _) = collect(&mut agent, "hi");
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

#[test]
fn tool_call_round_trip_keeps_the_conversation_paired() {
    let server = start_server(vec![
        tool_response(&[("call_1", "read", serde_json::json!({"path": "a.txt"}))]),
        plain_response(&["done"]),
    ]);
    let home = temp_home("paired");
    let workspace = temp_home("workspace");
    std::fs::write(workspace.join("a.txt"), "hello\n").unwrap();
    let mut agent = Agent::new(
        &config(server.port, &home, None),
        &workspace.to_string_lossy(),
        &session_path(&home),
    )
    .expect("打开会话");

    let (text, _, tools) = collect(&mut agent, "读一下 a.txt");
    assert_eq!(text, "done");
    assert_eq!(tools.len(), 1);
    assert!(tools[0].starts_with("read a.txt"));

    let bodies = server.bodies.lock();
    let second = &bodies[1]["messages"];
    let assistant = second
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "assistant" && m.get("tool_calls").is_some())
        .expect("第二轮应带工具调用记录");
    assert_eq!(assistant["tool_calls"][0]["id"], "call_1");
    let tool = second
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "tool")
        .expect("第二轮应带工具结果");
    assert_eq!(tool["tool_call_id"], "call_1");
    assert!(
        tool["content"].as_str().unwrap().contains("hello"),
        "工具结果应包含文件内容：{}",
        tool["content"]
    );
}

#[test]
fn a_tool_error_is_fed_back_instead_of_ending_the_turn() {
    let server = start_server(vec![
        tool_response(&[("call_1", "read", serde_json::json!({"path": "missing.txt"}))]),
        plain_response(&["我换一种方式"]),
    ]);
    let home = temp_home("tool-error");
    let workspace = temp_home("workspace-error");
    let mut agent = Agent::new(
        &config(server.port, &home, None),
        &workspace.to_string_lossy(),
        &session_path(&home),
    )
    .expect("打开会话");

    let (text, _, _) = collect(&mut agent, "读一个不存在的文件");
    assert_eq!(text, "我换一种方式");

    let bodies = server.bodies.lock();
    let tool: &Value = bodies[1]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "tool")
        .expect("错误也要作为工具结果回给模型");
    assert!(tool["content"].as_str().unwrap().starts_with("ERROR:"));
}

#[test]
fn unknown_tool_name_is_reported_as_text() {
    let server = start_server(vec![
        tool_response(&[("call_1", "nope", serde_json::json!({}))]),
        plain_response(&["好"]),
    ]);
    let home = temp_home("unknown-tool");
    let workspace = temp_home("workspace-unknown");
    let mut agent = Agent::new(
        &config(server.port, &home, None),
        &workspace.to_string_lossy(),
        &session_path(&home),
    )
    .expect("打开会话");

    let (text, _, _) = collect(&mut agent, "随便");
    assert_eq!(text, "好");
}

#[test]
fn event_stream_handles_empty_deltas() {
    let server = start_server(vec![sse_response(
        &[],
        &[],
        serde_json::json!({"prompt_tokens": 3, "completion_tokens": 0}),
    )]);
    let home = temp_home("empty");
    let mut agent = Agent::new(
        &config(server.port, &home, None),
        "/tmp/proj",
        &session_path(&home),
    )
    .expect("打开会话");
    let (text, _, _) = collect(&mut agent, "hi");
    assert!(text.is_empty());
}

#[test]
fn a_failed_turn_is_also_written_to_the_session_file() {
    let bad = "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{\"error\":{\"message\":\"bad request\"}}".to_string();
    let server = start_server(vec![bad]);
    let home = temp_home("err-home");
    let mut agent = Agent::new(
        &config(server.port, &home, None),
        "/tmp/proj",
        &session_path(&home),
    )
    .expect("打开会话");

    let result = agent.submit("hi", &mut |_| {});
    assert!(result.is_err(), "400 应当让这一轮失败");

    let entries = session_entries(&home);
    let last = entries.last().expect("至少有一条");
    assert_eq!(last["type"], "notice", "失败原因要落盘：{entries:?}");
    let text = last["text"].as_str().unwrap_or("");
    assert!(text.contains("这一轮失败"), "{text}");
    assert!(text.contains("400"), "错误文本要带上状态码：{text}");
}

#[test]
fn repeated_failures_end_with_a_summary_instead_of_a_truncated_turn() {
    let failing = || tool_response(&[("c1", "read", serde_json::json!({"path": "missing.txt"}))]);
    let server = start_server(vec![
        failing(),
        failing(),
        failing(),
        plain_response(&["收尾总结"]),
    ]);
    let home = temp_home("repeat-home");
    let mut agent = Agent::new(
        &config(server.port, &home, None),
        "/tmp/proj",
        &session_path(&home),
    )
    .expect("打开会话");

    let (text, _, tools) = collect(&mut agent, "一直读不存在的文件");
    assert_eq!(text, "收尾总结");
    assert_eq!(tools.len(), 3, "同一个失败调用第三次就停");

    let bodies = server.bodies.lock();
    let last_request = bodies.last().expect("有请求");
    assert!(
        last_request.get("tools").is_none(),
        "收尾那次不能带工具：{last_request}"
    );

    let entries = session_entries(&home);
    let notice = entries
        .iter()
        .rev()
        .find(|entry| entry["type"] == "notice")
        .expect("要有停止提示");
    assert!(
        notice["text"].as_str().unwrap_or("").contains("连续失败"),
        "{notice}"
    );
    let last = entries.last().expect("至少有一条");
    assert_eq!(last["message"]["role"], "assistant", "最后一条是收尾回答");
    assert_eq!(last["message"]["content"], "收尾总结");
}

#[test]
fn a_command_that_keeps_failing_counts_as_a_failure() {
    let failing = || tool_response(&[("c1", "bash", serde_json::json!({"command": "exit 7"}))]);
    let server = start_server(vec![
        failing(),
        failing(),
        failing(),
        plain_response(&["卡住了"]),
    ]);
    let home = temp_home("bash-fail-home");
    let mut agent = Agent::new(
        &config(server.port, &home, None),
        "/tmp/proj",
        &session_path(&home),
    )
    .expect("打开会话");

    let (text, _, tools) = collect(&mut agent, "反复执行失败命令");
    assert_eq!(text, "卡住了");
    assert_eq!(tools.len(), 3, "非零退出码也要算失败");
}

#[test]
fn a_failure_followed_by_different_calls_does_not_stop_the_turn() {
    let missing = tool_response(&[("c1", "read", serde_json::json!({"path": "missing.txt"}))]);
    let server = start_server(vec![
        missing.clone(),
        missing.clone(),
        tool_response(&[("c2", "bash", serde_json::json!({"command": "true"}))]),
        plain_response(&["继续做完了"]),
    ]);
    let home = temp_home("repeat-ok-home");
    let mut agent = Agent::new(
        &config(server.port, &home, None),
        "/tmp/proj",
        &session_path(&home),
    )
    .expect("打开会话");

    let (text, _, tools) = collect(&mut agent, "先失败两次再换个办法");
    assert_eq!(text, "继续做完了");
    assert_eq!(tools.len(), 3, "换调用就重新计数，不该被拦");
}
