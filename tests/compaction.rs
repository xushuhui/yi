//! 里程碑③ 的压缩行为：触发、落盘、重建后的请求内容。

mod common;

use common::*;
use yi::agent::Agent;

#[test]
fn compaction_triggers_above_half_the_window() {
    let big = "x".repeat(30_000);
    let server = start_server(vec![
        plain_response(&[big.as_str()]),
        plain_response(&[big.as_str()]),
        big_usage_response(&["ok"], 600_000),
        json_response("## 目标\n测试压缩"),
        plain_response(&["after"]),
    ]);
    let home = temp_home("compact-home");
    let workspace = temp_home("compact-ws");
    let mut agent = Agent::new(
        &config(server.port, &home, None),
        &workspace.to_string_lossy(),
        &session_path(&home),
    )
    .expect("打开会话");

    collect(&mut agent, "Q1");
    collect(&mut agent, "Q2");
    collect(&mut agent, "Q3");
    collect(&mut agent, "Q4");

    let entries = session_entries(&home);
    let compaction = entries
        .iter()
        .find(|entry| entry["type"] == "compaction")
        .expect("超过阈值后应写入压缩条目");
    assert!(
        compaction["summary"].as_str().unwrap().contains("测试压缩"),
        "摘要内容应落盘：{compaction}"
    );
    assert!(compaction["firstKeptSeq"].as_u64().unwrap() > 0);
    assert!(compaction["tokensBefore"].as_u64().unwrap() > 0);

    let bodies = server.bodies.lock();
    let compaction_request = &bodies[3];
    assert_eq!(compaction_request["stream"], false, "压缩请求必须是非流式");
    assert!(
        compaction_request.get("tools").is_none(),
        "压缩请求不应带工具定义"
    );

    let after = &bodies[4]["messages"];
    assert!(
        after[1]["content"]
            .as_str()
            .unwrap()
            .contains("<compaction-summary>"),
        "压缩后的请求第二条应是摘要：{after}"
    );
    let flattened = serde_json::to_string(after).unwrap();
    assert!(
        !flattened.contains("Q1"),
        "最早的对话应被摘要替代：{flattened}"
    );
}

#[test]
fn compaction_failure_keeps_the_turn() {
    let big = "x".repeat(30_000);
    let server = start_server(vec![
        plain_response(&[big.as_str()]),
        plain_response(&[big.as_str()]),
        big_usage_response(&["ok"], 600_000),
        // 摘要请求失败（500），本轮内容必须保留。
        "HTTP/1.1 500 Internal Server Error\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{\"error\":{\"message\":\"boom\"}}".to_string(),
        "HTTP/1.1 500 Internal Server Error\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{\"error\":{\"message\":\"boom\"}}".to_string(),
        "HTTP/1.1 500 Internal Server Error\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{\"error\":{\"message\":\"boom\"}}".to_string(),
        "HTTP/1.1 500 Internal Server Error\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{\"error\":{\"message\":\"boom\"}}".to_string(),
    ]);
    let home = temp_home("compact-fail-home");
    let workspace = temp_home("compact-fail-ws");
    let mut agent = Agent::new(
        &config(server.port, &home, None),
        &workspace.to_string_lossy(),
        &session_path(&home),
    )
    .expect("打开会话");

    collect(&mut agent, "Q1");
    collect(&mut agent, "Q2");
    let (text, _, _) = collect(&mut agent, "Q3");
    assert_eq!(text, "ok", "压缩失败不应影响本轮回答");

    let entries = session_entries(&home);
    assert!(
        !entries.iter().any(|entry| entry["type"] == "compaction"),
        "失败时不应写入压缩条目"
    );
    let user_count = entries
        .iter()
        .filter(|entry| entry["message"]["role"] == "user")
        .count();
    assert_eq!(user_count, 3, "三句用户消息都应保留");
}
