//! 计划模式：工作树只读。走真 HTTP 打本地假服务器，断言工具结果与会话文件。

mod common;

use std::path::Path;

use common::*;
use serde_json::json;
use yi::agent::Agent;

fn open_agent(port: u16, home: &Path, workspace: &Path) -> Agent {
    Agent::new(
        &config(port, home, None),
        &workspace.to_string_lossy(),
        &session_path(home),
    )
    .expect("打开会话")
}

fn tool_outputs(home: &Path) -> Vec<String> {
    session_entries(home)
        .iter()
        .filter(|entry| entry["message"]["role"] == "tool")
        .map(|entry| {
            entry["message"]["content"]
                .as_str()
                .unwrap_or_default()
                .to_string()
        })
        .collect()
}

#[test]
fn plan_mode_refuses_a_write_until_it_is_switched_off() {
    let home = temp_home("plan-home");
    let ws = temp_home("plan-ws");
    let server = start_server(vec![
        tool_response(&[("call_1", "write", json!({"path": "a.txt", "content": "x"}))]),
        plain_response(&["好"]),
        tool_response(&[("call_2", "write", json!({"path": "a.txt", "content": "x"}))]),
        plain_response(&["好"]),
    ]);
    let mut agent = open_agent(server.port, &home, &ws);

    agent.set_plan(true).expect("开计划模式");
    collect(&mut agent, "写个文件");
    let refused = tool_outputs(&home).pop().expect("有工具结果");
    assert!(refused.starts_with("ERROR:"), "{refused}");
    assert!(refused.contains("只读"), "{refused}");
    assert!(!ws.join("a.txt").exists(), "计划模式下不该落盘");

    agent.set_plan(false).expect("关计划模式");
    collect(&mut agent, "现在可以写了");
    assert_eq!(
        std::fs::read_to_string(ws.join("a.txt")).expect("关掉后能写"),
        "x"
    );
}

#[test]
fn plan_mode_refuses_a_bash_command_that_would_write() {
    let home = temp_home("plan-bash-home");
    let ws = temp_home("plan-bash-ws");
    let server = start_server(vec![
        tool_response(&[("call_1", "bash", json!({"command": "echo hi > out.txt"}))]),
        plain_response(&["好"]),
        tool_response(&[("call_2", "bash", json!({"command": "ls"}))]),
        plain_response(&["好"]),
    ]);
    let mut agent = open_agent(server.port, &home, &ws);

    agent.set_plan(true).expect("开计划模式");
    collect(&mut agent, "写一下");
    let refused = tool_outputs(&home).pop().expect("有工具结果");
    assert!(refused.starts_with("ERROR:"), "{refused}");
    assert!(!ws.join("out.txt").exists(), "写命令不该执行");

    collect(&mut agent, "看看目录");
    let listed = tool_outputs(&home).pop().expect("有工具结果");
    assert!(!listed.starts_with("ERROR:"), "{listed}");
}

#[test]
fn plan_mode_is_restored_when_the_same_session_reopens() {
    let home = temp_home("plan-reopen-home");
    let ws = temp_home("plan-reopen-ws");
    let server = start_server(vec![plain_response(&["好"])]);
    {
        let mut agent = open_agent(server.port, &home, &ws);
        agent.set_plan(true).expect("开计划模式");
        collect(&mut agent, "先问一句");
    }
    let reopened = open_agent(server.port, &home, &ws);
    assert!(reopened.plan(), "重开同一条会话仍然记得计划模式是开的");
}
