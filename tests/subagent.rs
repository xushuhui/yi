//! 子 agent：独立上下文、只读工具、并行执行、结果回到父对话。

mod common;

use std::path::Path;

use common::*;
use serde_json::{Value, json};
use yi::agent::Agent;

fn open_agent(port: u16, home: &Path, workspace: &Path) -> Agent {
    Agent::new(
        &config(port, home, None),
        &workspace.to_string_lossy(),
        &session_path(home),
    )
    .expect("打开会话")
}

fn tool_names(body: &Value) -> Vec<String> {
    body["tools"]
        .as_array()
        .map(|tools| {
            tools
                .iter()
                .filter_map(|tool| tool["function"]["name"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn two_task_calls_run_in_their_own_contexts_in_parallel() {
    let home = temp_home("sub-agent-home");
    let ws = temp_home("sub-agent-ws");
    let server = start_server(vec![
        tool_response(&[
            ("c1", "task", json!({"task": "审计 A", "name": "a"})),
            ("c2", "task", json!({"task": "审计 B", "name": "b"})),
        ]),
        plain_response(&["完成"]),
        plain_response(&["完成"]),
        plain_response(&["汇总"]),
    ]);
    let mut agent = open_agent(server.port, &home, &ws);

    let (text, _, _) = collect(&mut agent, "派两个子 agent");
    assert_eq!(text, "汇总");

    let bodies = server.bodies.lock();
    assert_eq!(bodies.len(), 4, "父请求 + 两个子 agent + 父收尾");
    assert!(tool_names(&bodies[0]).contains(&"task".to_string()));

    // 子 agent 的请求：系统 + 一条任务，没有父对话历史，也没有 task 工具。
    let mut subagent_requests = 0;
    for body in bodies.iter().skip(1).take(2) {
        assert_eq!(body["messages"].as_array().map(Vec::len), Some(2), "{body}");
        let content = body["messages"][1]["content"].as_str().unwrap_or("");
        assert!(content.starts_with("任务："), "{content}");
        let names = tool_names(body);
        assert!(names.contains(&"read".to_string()), "{names:?}");
        assert!(names.contains(&"bash".to_string()), "{names:?}");
        assert!(!names.contains(&"task".to_string()), "深度只有一层");
        assert!(!names.contains(&"write".to_string()), "子 agent 只读");
        subagent_requests += 1;
    }
    assert_eq!(subagent_requests, 2);

    // 两个结果都回到父对话，带标签。
    let final_messages = bodies[3]["messages"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let results: Vec<String> = final_messages
        .iter()
        .filter(|message| message["role"] == "tool")
        .filter_map(|message| message["content"].as_str().map(str::to_string))
        .collect();
    assert!(
        results.iter().any(|text| text.contains("[子 agent a]")),
        "{results:?}"
    );
    assert!(
        results.iter().any(|text| text.contains("[子 agent b]")),
        "{results:?}"
    );
}

#[test]
fn a_subagent_cannot_write_even_if_it_asks() {
    let home = temp_home("sub-write-home");
    let ws = temp_home("sub-write-ws");
    let server = start_server(vec![
        tool_response(&[("c1", "task", json!({"task": "试着写文件"}))]),
        tool_response(&[("s1", "write", json!({"path": "leak.txt", "content": "x"}))]),
        plain_response(&["写不了"]),
        plain_response(&["好"]),
    ]);
    let mut agent = open_agent(server.port, &home, &ws);

    collect(&mut agent, "派一个子 agent");

    assert!(!ws.join("leak.txt").exists(), "只读工具不该落盘");
    let bodies = server.bodies.lock();
    // 子 agent 拿到拒绝后继续说话；父对话里的工具结果带着这段说明。
    let subagent_second = &bodies[2];
    let tool_messages: Vec<String> = subagent_second["messages"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter(|message| message["role"] == "tool")
        .filter_map(|message| message["content"].as_str().map(str::to_string))
        .collect();
    assert!(
        tool_messages.iter().any(|text| text.contains("只读")),
        "{tool_messages:?}"
    );
}
