//! 上键历史：只记用户原文，且绝不进入模型上下文。

mod common;

use common::*;
use yi::agent::Agent;

fn open(home: &std::path::Path) -> Agent {
    Agent::new(
        &config(0, home, None),
        "/tmp/proj",
        &session_path(home),
    )
    .expect("打开会话")
}

#[test]
fn prompt_history_keeps_order_and_drops_consecutive_duplicates() {
    let home = temp_home("hist-order");
    let mut agent = open(&home);

    agent.note_prompt("看下 main.go").expect("记一条");
    agent.note_prompt("看下 main.go").expect("重复一条");
    agent.note_prompt("/effort high").expect("记命令");
    agent.note_prompt("看下 main.go").expect("隔开后不算重复");

    assert_eq!(
        agent.prompt_history(),
        vec!["看下 main.go", "/effort high", "看下 main.go"]
    );

    let entries = session_entries(&home);
    assert!(
        entries.iter().any(|entry| entry["type"] == "prompt"),
        "要落进会话文件：{entries:?}"
    );
}

#[test]
fn prompt_history_keeps_only_the_newest_entries() {
    let home = temp_home("hist-cap");
    let mut agent = open(&home);
    for index in 0..205 {
        agent.note_prompt(&format!("第 {index} 条")).expect("记一条");
    }

    let history = agent.prompt_history();
    assert_eq!(history.len(), 200, "上限是 200 条");
    assert_eq!(history.first().map(String::as_str), Some("第 5 条"));
    assert_eq!(history.last().map(String::as_str), Some("第 204 条"));
}

#[test]
fn an_old_session_without_prompt_entries_falls_back_to_user_messages() {
    let home = temp_home("hist-legacy");
    std::fs::create_dir_all(session_path(&home).parent().expect("目录")).expect("建目录");
    let rows = [
        serde_json::json!({"type": "session", "version": 1, "id": "x", "timestamp": 1, "cwd": "/tmp/proj", "model": "deepseek-flash", "effort": "auto"}),
        serde_json::json!({"type": "message", "seq": 1, "timestamp": 2, "message": {"role": "user", "content": "改一下 README"}}),
        serde_json::json!({"type": "message", "seq": 2, "timestamp": 3, "message": {"role": "assistant", "content": "好", "reasoning": null, "tool_calls": [], "usage": null}}),
        serde_json::json!({"type": "message", "seq": 3, "timestamp": 4, "message": {"role": "user", "content": "[技能 code-review]\n正文"}}),
        serde_json::json!({"type": "message", "seq": 4, "timestamp": 5, "message": {"role": "user", "content": "/effort high"}}),
    ];
    let text: Vec<String> = rows.iter().map(|row| row.to_string()).collect();
    std::fs::write(session_path(&home), text.join("\n") + "\n").expect("写会话");

    let agent = open(&home);
    assert_eq!(
        agent.prompt_history(),
        vec!["改一下 README", "/effort high"],
        "老会话回填，但跳过技能展开的正文"
    );
}

#[test]
fn prompt_history_never_reaches_the_model() {
    let server = start_server(vec![plain_response(&["ok"])]);
    let home = temp_home("hist-model");
    let mut agent = Agent::new(
        &config(server.port, &home, None),
        "/tmp/proj",
        &session_path(&home),
    )
    .expect("打开会话");

    agent.note_prompt("内部标记-不该出现").expect("记一条");
    collect(&mut agent, "hi");

    let request = server.bodies.lock()[0].clone();
    let text = serde_json::to_string(&request).expect("序列化请求");
    assert!(
        !text.contains("内部标记-不该出现"),
        "历史不能进上下文：{text}"
    );
}
