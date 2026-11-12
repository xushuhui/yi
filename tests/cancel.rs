//! Ctrl-C 中断本轮：命令要尽快停下，会话里留下一条说明。

mod common;

use std::time::{Duration, Instant};

use common::*;
use yi::agent::Agent;
use yi::cancel;

#[test]
fn ctrl_c_stops_a_running_command_and_keeps_the_session() {
    cancel::reset();
    let server = start_server(vec![
        tool_response(&[(
            "c1",
            "bash",
            serde_json::json!({"command": "sleep 5; echo 不该到这里"}),
        )]),
        plain_response(&["不该到这里"]),
    ]);
    let home = temp_home("cancel-home");
    let workspace = temp_home("cancel-ws");
    let mut agent = Agent::new(
        &config(server.port, &home, None),
        &workspace.to_string_lossy(),
        &session_path(&home),
    )
    .expect("打开会话");

    let trigger = std::thread::spawn(|| {
        std::thread::sleep(Duration::from_millis(300));
        cancel::request();
    });
    let started = Instant::now();
    let result = agent.submit("跑一个长命令", &mut |_| {});
    let elapsed = started.elapsed();
    trigger.join().expect("触发线程");
    cancel::reset();

    let entries = session_entries(&home);
    assert!(result.is_err(), "被取消的这一轮应当返回错误，实际 {result:?}");
    assert!(
        elapsed < Duration::from_secs(3),
        "命令要被提前杀掉，实际用了 {elapsed:?}"
    );
    let notice = entries
        .iter()
        .rev()
        .find(|entry| entry["type"] == "notice")
        .expect("要有取消说明");
    assert!(
        notice["text"].as_str().unwrap_or("").contains("取消"),
        "{notice}"
    );
}
