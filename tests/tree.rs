//! `/tree` 的行为：列树、回退、旧分支保留、压缩边界。
//!
//! 断言只看外部可见结果：会话文件里的条目、下一轮请求里的消息、工具的返回值。

mod common;

use std::path::Path;

use common::*;
use yi::agent::Agent;
use yi::session::Session;

fn open_agent(port: u16, home: &Path, workspace: &Path) -> Agent {
    Agent::new(
        &config(port, home, None),
        &workspace.to_string_lossy(),
        &session_path(home),
    )
    .expect("打开会话")
}

fn last_request_text(server: &FakeServer) -> String {
    let bodies = server.bodies.lock();
    bodies
        .iter()
        .rev()
        .find(|body| body.get("messages").is_some())
        .map(|body| body.to_string())
        .unwrap_or_default()
}

#[test]
fn tree_lists_turns_and_rewind_drops_them_from_context() {
    let home = temp_home("tree-home");
    let ws = temp_home("tree-ws");
    let server = start_server(vec![
        plain_response(&["回答一"]),
        plain_response(&["回答二"]),
        plain_response(&["回答三"]),
        plain_response(&["回答四"]),
    ]);
    let mut agent = open_agent(server.port, &home, &ws);
    collect(&mut agent, "第一句");
    collect(&mut agent, "第二句");
    collect(&mut agent, "第三句");

    let nodes = agent.tree();
    assert_eq!(nodes.len(), 3);
    assert!(nodes.iter().all(|node| node.on_path));
    let previews: Vec<&str> = nodes.iter().map(|node| node.preview.as_str()).collect();
    assert_eq!(previews, vec!["第一句", "第二句", "第三句"]);

    let text = agent.rewind(2).expect("回退到第二条之前");
    assert_eq!(text, "第二句", "预填的是原文");
    let nodes = agent.tree();
    assert!(nodes[0].on_path, "第一条仍在新路径上");
    assert!(!nodes[1].on_path, "第二条被回退掉");
    assert!(!nodes[2].on_path, "第三条跟着回退");

    // 文件里留下一条 rewind，旧记录一条没少。
    let entries = session_entries(&home);
    let rewind = entries
        .iter()
        .find(|entry| entry["type"] == "rewind")
        .expect("回退要落一条记录");
    assert!(rewind["toSeq"].as_u64().is_some());
    assert_eq!(
        entries
            .iter()
            .filter(|entry| entry["message"]["role"] == "user")
            .count(),
        3,
        "旧分支不能从文件里消失"
    );

    // 回退后再问一句：请求里只剩第一句和新问题。
    collect(&mut agent, "第四句");
    let request = last_request_text(&server);
    assert!(request.contains("第一句"), "{request}");
    assert!(request.contains("第四句"), "{request}");
    assert!(
        !request.contains("第二句"),
        "被回退的消息不该再进上下文：{request}"
    );
    assert!(
        !request.contains("第三句"),
        "被回退的消息不该再进上下文：{request}"
    );
}

#[test]
fn rewinding_again_switches_back_to_the_old_branch() {
    let home = temp_home("tree-back-home");
    let ws = temp_home("tree-back-ws");
    let server = start_server(vec![
        plain_response(&["回答一"]),
        plain_response(&["回答二"]),
        plain_response(&["回答三"]),
        plain_response(&["改主意的回答"]),
        plain_response(&["原路回来的回答"]),
    ]);
    let mut agent = open_agent(server.port, &home, &ws);
    collect(&mut agent, "第一句");
    collect(&mut agent, "第二句");
    collect(&mut agent, "第三句");

    agent.rewind(3).expect("回到第三条之前");
    collect(&mut agent, "改主意");
    let nodes = agent.tree();
    assert_eq!(nodes.len(), 4, "旧分支仍在树里：{nodes:?}");
    assert!(nodes[3].on_path, "新的一条在当前路径上");
    assert!(!nodes[2].on_path, "第三条留在旧分支");
    assert!(nodes[1].branch_point, "第二条是两条路的分叉点");
    assert!(!nodes[3].branch_point, "分支起点本身不算分叉点");

    // 再切回旧分支的第三条，新分支那条不该进上下文。
    let text = agent.rewind(3).expect("切回旧分支");
    assert_eq!(text, "第三句");
    collect(&mut agent, "原路回来");
    let request = last_request_text(&server);
    assert!(request.contains("第一句"), "{request}");
    assert!(request.contains("第二句"), "{request}");
    assert!(request.contains("原路回来"), "{request}");
    assert!(
        !request.contains("第三句"),
        "被回退的那条不进上下文：{request}"
    );
    assert!(
        !request.contains("改主意"),
        "另一条分支不该混进来：{request}"
    );
}

#[test]
fn tree_marks_compacted_turns_and_refuses_to_cross_the_boundary() {
    let home = temp_home("tree-compact-home");
    std::fs::create_dir_all(home.join("sessions/proj")).unwrap();
    let path = home.join("sessions/proj/session.jsonl");
    let mut session = Session::open(&path, "/tmp", "m", "auto").expect("打开会话");
    for text in ["一", "二", "三", "四", "五"] {
        session.append_user(text).expect("写用户消息");
    }
    // 模拟一次压缩：保留从第三条开始，摘要覆盖它之前的内容。
    session
        .append_compaction("摘要".to_string(), 3, 100)
        .expect("写压缩条目");

    let nodes = session.tree();
    assert_eq!(nodes.len(), 5);
    assert_eq!(nodes[0].blocked, Some("已压缩"));
    assert_eq!(nodes[1].blocked, Some("已压缩"));
    assert!(nodes[2].blocked.is_some(), "边界上的那条也不在上下文里");
    assert_eq!(nodes[3].blocked, None);
    assert_eq!(nodes[4].blocked, None);

    let err = session.rewind(2).expect_err("压缩过的消息不能回退");
    assert!(format!("{err}").contains("已压缩"), "{err}");

    let text = session.rewind(4).expect("最近的一条可以回退");
    assert_eq!(text, "四");

    // 回退后上下文只剩摘要；路径上没有任何消息。
    let context = session.context();
    assert!(context.summary.is_some());
    assert!(context.messages.is_empty(), "回退到压缩边界处应清空原文");
}
