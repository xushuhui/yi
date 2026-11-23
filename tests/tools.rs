//! 里程碑 ② 的工具行为测试：锚点、只改已读区间、bash 上限与超时、grep 之后能接着改。
//!
//! 每条用例都在**同一个 agent 实例**里连续跑多轮——快照表是进程内的，
//! 换进程重新打开会话后必须重新 read，这是刻意的安全行为。

mod common;

use std::path::{Path, PathBuf};

use common::*;
use serde_json::{Value, json};
use yi::agent::Agent;
use yi::provider::Event;

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

fn last_tool_output(home: &Path) -> String {
    tool_outputs(home).pop().unwrap_or_default()
}

/// 从 `[路径#TAG]` 头里取出锚点。
fn anchor_of(output: &str) -> String {
    output
        .lines()
        .next()
        .unwrap_or_default()
        .trim_start_matches('[')
        .rsplit_once('#')
        .expect("头部应含 #TAG")
        .1
        .trim_end_matches(']')
        .to_string()
}

#[test]
fn read_returns_anchor_and_line_numbers() {
    let home = temp_home("read-home");
    let ws = temp_home("read-ws");
    std::fs::write(ws.join("a.txt"), "one\ntwo\nthree\n").unwrap();

    let server = start_server(vec![
        tool_response(&[("c1", "read", json!({"path": "a.txt"}))]),
        plain_response(&["ok"]),
        // 文件在外部被改掉之后再读一次，锚点必须变化。
        tool_response(&[("c2", "read", json!({"path": "a.txt"}))]),
        plain_response(&["ok"]),
    ]);
    let mut agent = open_agent(server.port, &home, &ws);

    collect(&mut agent, "读 a.txt");
    let first = last_tool_output(&home);
    assert!(first.starts_with("[a.txt#"), "缺少锚点头：{first}");
    assert!(first.contains("1:one"), "{first}");
    assert!(first.contains("3:three"), "{first}");
    let first_tag = anchor_of(&first);

    std::fs::write(ws.join("a.txt"), "changed\n").unwrap();
    collect(&mut agent, "再读一次");
    let second_tag = anchor_of(&last_tool_output(&home));
    assert_ne!(first_tag, second_tag, "文件改动后锚点必须变化");
}

#[test]
fn edit_applies_when_the_tag_matches() {
    let home = temp_home("edit-home");
    let ws = temp_home("edit-ws");
    std::fs::write(ws.join("a.txt"), "one\ntwo\nthree\n").unwrap();
    let tag = file_tag(ws.join("a.txt"));
    let patch = format!("[a.txt#{tag}]\nPUT 2.=2:\n+TWO\n");

    let server = start_server(vec![
        tool_response(&[("c1", "read", json!({"path": "a.txt"}))]),
        plain_response(&["ok"]),
        tool_response(&[("c2", "edit", json!({ "input": patch }))]),
        plain_response(&["ok"]),
    ]);
    let mut agent = open_agent(server.port, &home, &ws);
    collect(&mut agent, "读");
    collect(&mut agent, "改第二行");

    assert_eq!(
        std::fs::read_to_string(ws.join("a.txt")).unwrap(),
        "one\nTWO\nthree\n"
    );
    assert!(
        last_tool_output(&home).starts_with("a.txt#"),
        "{}",
        last_tool_output(&home)
    );
}

#[test]
fn edit_keeps_crlf_line_endings() {
    let home = temp_home("crlf-home");
    let ws = temp_home("crlf-ws");
    std::fs::write(ws.join("a.txt"), "one\r\ntwo\r\nthree\r\n").unwrap();
    let tag = file_tag(ws.join("a.txt"));
    let patch = format!("[a.txt#{tag}]\nPUT 2.=2:\n+TWO\n");

    let server = start_server(vec![
        tool_response(&[("c1", "read", json!({"path": "a.txt"}))]),
        plain_response(&["ok"]),
        tool_response(&[("c2", "edit", json!({ "input": patch }))]),
        plain_response(&["ok"]),
    ]);
    let mut agent = open_agent(server.port, &home, &ws);
    collect(&mut agent, "读");
    collect(&mut agent, "改第二行");

    let bytes = std::fs::read(ws.join("a.txt")).unwrap();
    assert_eq!(
        String::from_utf8(bytes).unwrap(),
        "one\r\nTWO\r\nthree\r\n",
        "CRLF 文件改一行不能把整篇转成 LF"
    );
}

#[test]
fn edit_keeps_a_missing_trailing_newline_missing() {
    let home = temp_home("noeol-home");
    let ws = temp_home("noeol-ws");
    std::fs::write(ws.join("a.txt"), "one\ntwo").unwrap();
    let tag = file_tag(ws.join("a.txt"));
    let patch = format!("[a.txt#{tag}]\nPUT 2.=2:\n+TWO\n");

    let server = start_server(vec![
        tool_response(&[("c1", "read", json!({"path": "a.txt"}))]),
        plain_response(&["ok"]),
        tool_response(&[("c2", "edit", json!({ "input": patch }))]),
        plain_response(&["ok"]),
    ]);
    let mut agent = open_agent(server.port, &home, &ws);
    collect(&mut agent, "读");
    collect(&mut agent, "改第二行");

    assert_eq!(
        std::fs::read_to_string(ws.join("a.txt")).unwrap(),
        "one\nTWO",
        "原本没有结尾换行，改完也不该凭空补一个"
    );
}

#[test]
fn one_edit_call_lists_every_changed_file_on_one_line() {
    let home = temp_home("multi-edit-home");
    let ws = temp_home("multi-edit-ws");
    std::fs::write(ws.join("a.txt"), "one\n").unwrap();
    std::fs::write(ws.join("b.txt"), "two\n").unwrap();
    let tag_a = file_tag(ws.join("a.txt"));
    let tag_b = file_tag(ws.join("b.txt"));
    let patch = format!("[a.txt#{tag_a}]\nPUT 1.=1:\n+A\n[b.txt#{tag_b}]\nPUT 1.=1:\n+B\n");

    // 每次 collect 是完整一轮：工具调用之后跟一句普通回答，才会结束这一轮。
    let server = start_server(vec![
        tool_response(&[("c1", "read", json!({ "path": "a.txt" }))]),
        plain_response(&["ok"]),
        tool_response(&[("c2", "read", json!({ "path": "b.txt" }))]),
        plain_response(&["ok"]),
        tool_response(&[("c3", "edit", json!({ "input": patch }))]),
        plain_response(&["ok"]),
    ]);
    let mut agent = open_agent(server.port, &home, &ws);
    collect(&mut agent, "读 a.txt");
    collect(&mut agent, "读 b.txt");
    collect(&mut agent, "两个一起改");

    // 一个文件一段，用顿号连成一行；不报行数，也不带方括号。
    let expected = format!("a.txt#{}、b.txt#{}", tag_of_str("A\n"), tag_of_str("B\n"));
    assert_eq!(last_tool_output(&home), expected);
}

#[test]
fn a_successful_edit_does_not_print_a_result_line() {
    let home = temp_home("silent-edit-home");
    let ws = temp_home("silent-edit-ws");
    std::fs::write(ws.join("a.txt"), "one\ntwo\n").unwrap();
    let tag = file_tag(ws.join("a.txt"));
    let patch = format!("[a.txt#{tag}]\nPUT 2.=2:\n+TWO\n");

    let server = start_server(vec![
        tool_response(&[("c1", "read", json!({ "path": "a.txt" }))]),
        plain_response(&["ok"]),
        tool_response(&[("c2", "edit", json!({ "input": patch }))]),
        plain_response(&["ok"]),
    ]);
    let mut agent = open_agent(server.port, &home, &ws);
    agent.submit("读", &mut |_| {}).expect("读这一轮成功");

    let mut notes = Vec::new();
    agent
        .submit("改", &mut |event| {
            if let Event::ToolResult { name, note, failed } = event {
                notes.push((name, note, failed));
            }
        })
        .expect("改这一轮成功");
    // 事件照发（界面靠它把状态行推回「等待模型」），但提示是空的，什么都不打印。
    assert_eq!(
        notes,
        vec![("edit".to_string(), String::new(), false)],
        "{notes:?}"
    );
}

#[test]
fn a_background_command_returns_at_once_and_reports_on_the_next_turn() {
    let home = temp_home("bg-home");
    let ws = temp_home("bg-ws");

    let server = start_server(vec![
        tool_response(&[(
            "c1",
            "bash",
            json!({ "command": "sleep 1; echo bg-ok", "background": true }),
        )]),
        plain_response(&["ok1"]),
        plain_response(&["ok2"]),
    ]);
    let mut agent = open_agent(server.port, &home, &ws);

    let (text, _, _) = collect(&mut agent, "后台跑一下");
    assert_eq!(text, "ok1", "后台命令不该拖住这一轮");
    let out = last_tool_output(&home);
    assert!(out.contains("已转入后台"), "{out}");

    std::thread::sleep(std::time::Duration::from_millis(1800));
    let (text2, _, _) = collect(&mut agent, "继续");
    assert_eq!(text2, "ok2");

    let delivered = session_entries(&home).iter().any(|entry| {
        entry["message"]["content"]
            .as_str()
            .is_some_and(|content| content.contains("后台作业") && content.contains("bg-ok"))
    });
    assert!(delivered, "下一轮要把后台输出带进会话");
}

#[test]
fn a_search_with_no_match_is_reported_without_a_failure_mark() {
    let home = temp_home("no-match-home");
    let ws = temp_home("no-match-ws");
    std::fs::write(ws.join("a.txt"), "one\ntwo\n").unwrap();

    let server = start_server(vec![
        tool_response(&[("c1", "bash", json!({ "command": "grep -n zzz a.txt" }))]),
        plain_response(&["ok"]),
    ]);
    let mut agent = open_agent(server.port, &home, &ws);
    let mut notes = Vec::new();
    agent
        .submit("找 zzz", &mut |event| {
            if let Event::ToolResult { name, note, failed } = event {
                notes.push((name, note, failed));
            }
        })
        .expect("这一轮成功");

    assert_eq!(
        notes,
        vec![(
            "bash".to_string(),
            "grep -n zzz a.txt — 没有匹配（退出码 1）".to_string(),
            false
        )],
        "没匹配不该标成失败"
    );
}

#[test]
fn repeating_a_search_that_finds_nothing_does_not_stop_the_round() {
    let home = temp_home("no-match-repeat-home");
    let ws = temp_home("no-match-repeat-ws");
    std::fs::write(ws.join("a.txt"), "one\ntwo\n").unwrap();

    // 同一个调用连续三次都没匹配：以前会触发「连续失败三次就停」。
    let server = start_server(vec![
        tool_response(&[("c1", "bash", json!({ "command": "grep -n zzz a.txt" }))]),
        tool_response(&[("c2", "bash", json!({ "command": "grep -n zzz a.txt" }))]),
        tool_response(&[("c3", "bash", json!({ "command": "grep -n zzz a.txt" }))]),
        plain_response(&["ok"]),
    ]);
    let mut agent = open_agent(server.port, &home, &ws);
    let (text, _, _) = collect(&mut agent, "换着搜三次");

    assert_eq!(text, "ok", "这一轮应当正常收尾");
    let stopped = session_entries(&home).iter().any(|entry| {
        entry["message"]["content"]
            .as_str()
            .is_some_and(|content| content.contains("已停止调用工具"))
    });
    assert!(!stopped, "没匹配不该触发放停");
}

#[test]
fn edit_rejects_unseen_lines() {
    let home = temp_home("unseen-home");
    let ws = temp_home("unseen-ws");
    std::fs::write(ws.join("a.txt"), "one\ntwo\nthree\n").unwrap();

    // 同一个进程里：先只读第一行，再拿真锚点去改第二行。
    let server = start_server(vec![
        tool_response(&[("c1", "read", json!({"path": "a.txt", "limit": 1}))]),
        plain_response(&["ok"]),
    ]);
    let mut agent = open_agent(server.port, &home, &ws);
    collect(&mut agent, "读第一行");
    let header = last_tool_output(&home);
    assert!(header.contains("1:one"));
    let tag = anchor_of(&header);

    let patch = format!("[a.txt#{tag}]\nPUT 2.=2:\n+TWO\n");
    let server2 = start_server(vec![
        tool_response(&[("c2", "edit", json!({ "input": patch }))]),
        plain_response(&["ok"]),
    ]);
    drop(agent);
    // 注意：换进程后快照丢失，这里会先报「还没被读过」——同样是拒绝，文件不变。
    let mut agent = open_agent(server2.port, &home, &ws);
    collect(&mut agent, "改第二行");

    let output = last_tool_output(&home);
    assert!(output.starts_with("ERROR:"), "必须拒绝：{output}");
    assert_eq!(
        std::fs::read_to_string(ws.join("a.txt")).unwrap(),
        "one\ntwo\nthree\n"
    );
}

#[test]
fn edit_rejects_unseen_line_in_the_same_process() {
    let home = temp_home("unseen2-home");
    let ws = temp_home("unseen2-ws");
    std::fs::write(ws.join("a.txt"), "one\ntwo\nthree\n").unwrap();
    let tag = file_tag(ws.join("a.txt"));
    let patch = format!("[a.txt#{tag}]\nPUT 2.=2:\n+TWO\n");

    let server = start_server(vec![
        tool_response(&[("c1", "read", json!({"path": "a.txt", "limit": 1}))]),
        plain_response(&["ok"]),
        tool_response(&[("c2", "edit", json!({ "input": patch }))]),
        plain_response(&["ok"]),
    ]);
    let mut agent = open_agent(server.port, &home, &ws);
    collect(&mut agent, "读第一行");
    collect(&mut agent, "改第二行");

    let output = last_tool_output(&home);
    assert!(output.contains("没有读过"), "必须拒绝：{output}");
    assert_eq!(
        std::fs::read_to_string(ws.join("a.txt")).unwrap(),
        "one\ntwo\nthree\n"
    );
}

#[test]
fn edit_rejects_a_stale_anchor() {
    let home = temp_home("stale-home");
    let ws = temp_home("stale-ws");
    std::fs::write(ws.join("a.txt"), "one\ntwo\n").unwrap();
    let tag = file_tag(ws.join("a.txt"));
    let patch = format!("[a.txt#{tag}]\nPUT 2.=2:\n+TWO\n");

    let server = start_server(vec![
        tool_response(&[("c1", "read", json!({"path": "a.txt"}))]),
        plain_response(&["ok"]),
        tool_response(&[("c2", "edit", json!({ "input": patch }))]),
        plain_response(&["ok"]),
    ]);
    let mut agent = open_agent(server.port, &home, &ws);
    collect(&mut agent, "读");
    std::fs::write(ws.join("a.txt"), "one\nchanged\n").unwrap();
    collect(&mut agent, "改");

    assert!(last_tool_output(&home).contains("对不上"));
    assert_eq!(
        std::fs::read_to_string(ws.join("a.txt")).unwrap(),
        "one\nchanged\n"
    );
}

#[test]
fn repeated_identical_patch_is_rejected() {
    let home = temp_home("repeat-home");
    let ws = temp_home("repeat-ws");
    std::fs::write(ws.join("a.txt"), "one\n").unwrap();
    let tag = file_tag(ws.join("a.txt"));
    let patch = format!("[a.txt#{tag}]\nPUT 1.=1:\n+X\n");

    let server = start_server(vec![
        tool_response(&[("c1", "read", json!({"path": "a.txt"}))]),
        plain_response(&["ok"]),
        tool_response(&[("c2", "edit", json!({ "input": patch }))]),
        plain_response(&["ok"]),
        tool_response(&[("c3", "edit", json!({ "input": patch }))]),
        plain_response(&["ok"]),
    ]);
    let mut agent = open_agent(server.port, &home, &ws);
    collect(&mut agent, "读");
    collect(&mut agent, "改一次");
    collect(&mut agent, "再改一次");

    let outputs = tool_outputs(&home);
    assert!(
        outputs[1].starts_with("a.txt#"),
        "第一次应成功：{}",
        outputs[1]
    );
    assert!(
        outputs[2].starts_with("ERROR:"),
        "第二次应失败（锚点已过期）：{}",
        outputs[2]
    );
    assert_eq!(std::fs::read_to_string(ws.join("a.txt")).unwrap(), "X\n");
}

#[test]
fn write_creates_parents_and_returns_an_anchor() {
    let home = temp_home("write-home");
    let ws = temp_home("write-ws");
    let server = start_server(vec![
        tool_response(&[(
            "c1",
            "write",
            json!({"path": "deep/new.txt", "content": "hi\n"}),
        )]),
        plain_response(&["ok"]),
    ]);
    let mut agent = open_agent(server.port, &home, &ws);
    collect(&mut agent, "写个文件");

    assert_eq!(
        std::fs::read_to_string(ws.join("deep/new.txt")).unwrap(),
        "hi\n"
    );
    assert!(last_tool_output(&home).starts_with("[deep/new.txt#"));
}

#[test]
fn bash_truncates_to_the_tail_and_keeps_a_full_log() {
    let home = temp_home("bash-home");
    let ws = temp_home("bash-ws");
    let server = start_server(vec![
        tool_response(&[(
            "c1",
            "bash",
            json!({"command": "yes xxxxxxxxxxxxxxxxxxxx | head -n 6000"}),
        )]),
        plain_response(&["ok"]),
    ]);
    let mut agent = open_agent(server.port, &home, &ws);
    collect(&mut agent, "跑个长输出");

    let output = last_tool_output(&home);
    assert!(output.contains("只显示尾部 50 KB"), "{output}");
    assert!(output.contains("完整输出："), "{output}");

    let log_dir = session_path(&home).parent().unwrap().join("logs");
    let log = std::fs::read_to_string(log_dir.join("1.log")).expect("完整日志应落盘");
    assert!(
        log.len() > 50 * 1024,
        "日志应保存全部输出，实际 {} 字节",
        log.len()
    );
}

#[test]
fn bash_timeout_is_reported() {
    let home = temp_home("timeout-home");
    let ws = temp_home("timeout-ws");
    let server = start_server(vec![
        tool_response(&[("c1", "bash", json!({"command": "sleep 5", "timeout": 1}))]),
        plain_response(&["ok"]),
    ]);
    let mut agent = open_agent(server.port, &home, &ws);

    let started = std::time::Instant::now();
    collect(&mut agent, "跑个会卡住的命令");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(4),
        "应当在 1 秒左右被终止"
    );
    assert!(last_tool_output(&home).contains("超过 1 秒"));
}

#[test]
fn grep_results_can_be_edited_right_away() {
    let home = temp_home("grep-home");
    let ws = temp_home("grep-ws");
    std::fs::write(ws.join("a.go"), "alpha\nbeta\ngamma\n").unwrap();
    let tag = file_tag(ws.join("a.go"));
    let patch = format!("[a.go#{tag}]\nPUT 2.=2:\n+BETA\n");

    let server = start_server(vec![
        tool_response(&[("c1", "grep", json!({"pattern": "^beta$"}))]),
        plain_response(&["ok"]),
        tool_response(&[("c2", "edit", json!({ "input": patch }))]),
        plain_response(&["ok"]),
    ]);
    let mut agent = open_agent(server.port, &home, &ws);
    collect(&mut agent, "搜一下 beta");
    let output = last_tool_output(&home);
    assert!(output.contains("a.go:2:beta"), "{output}");

    collect(&mut agent, "改成大写");
    assert_eq!(
        std::fs::read_to_string(ws.join("a.go")).unwrap(),
        "alpha\nBETA\ngamma\n"
    );
}

#[test]
fn glob_finds_files_by_pattern() {
    let home = temp_home("glob-home");
    let ws = temp_home("glob-ws");
    std::fs::create_dir_all(ws.join("src")).unwrap();
    std::fs::write(ws.join("src/a.go"), "package a\n").unwrap();
    std::fs::write(ws.join("src/b.rs"), "fn main() {}\n").unwrap();

    let server = start_server(vec![
        tool_response(&[("c1", "glob", json!({"pattern": "**/*.go"}))]),
        plain_response(&["ok"]),
    ]);
    let mut agent = open_agent(server.port, &home, &ws);
    collect(&mut agent, "找 go 文件");

    let output = last_tool_output(&home);
    assert!(output.contains("src/a.go"), "{output}");
    assert!(!output.contains("src/b.rs"), "{output}");
}

#[test]
fn unknown_tool_name_is_reported_as_text() {
    let home = temp_home("unknown-home");
    let ws = temp_home("unknown-ws");
    let server = start_server(vec![
        tool_response(&[("c1", "nope", json!({}))]),
        plain_response(&["好"]),
    ]);
    let mut agent = open_agent(server.port, &home, &ws);
    let (text, _, _) = collect(&mut agent, "随便");

    assert_eq!(text, "好");
    assert!(last_tool_output(&home).contains("未知工具"));
}

#[test]
fn tool_error_is_fed_back_instead_of_ending_the_turn() {
    let home = temp_home("tool-error-home");
    let ws = temp_home("tool-error-ws");
    let server = start_server(vec![
        tool_response(&[("c1", "read", json!({"path": "missing.txt"}))]),
        plain_response(&["我换一种方式"]),
    ]);
    let mut agent = open_agent(server.port, &home, &ws);
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

/// 测试里自己算一次锚点，独立于实现。
fn file_tag(path: PathBuf) -> String {
    tag_of_str(&std::fs::read_to_string(path).unwrap())
}

fn tag_of_str(text: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{:04X}", (hash & 0xffff) as u16)
}

#[test]
fn read_image_is_attached_to_the_conversation() {
    let home = temp_home("image-home");
    let ws = temp_home("image-ws");
    std::fs::write(ws.join("pic.png"), b"not-really-a-png").unwrap();

    let server = start_server(vec![
        tool_response(&[("c1", "read", json!({"path": "pic.png"}))]),
        plain_response(&["看到了"]),
    ]);
    let mut agent = open_agent(server.port, &home, &ws);
    let (text, _, _) = collect(&mut agent, "看看这张图");
    assert_eq!(text, "看到了");

    let output = last_tool_output(&home);
    assert!(output.contains("已附"), "{output}");

    let bodies = server.bodies.lock();
    let attached = bodies[1]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| {
            m["content"]
                .as_array()
                .is_some_and(|parts| parts.iter().any(|p| p["type"] == "image_url"))
        })
        .expect("第二轮请求里应带图片内容块");
    let url = attached["content"][1]["image_url"]["url"]
        .as_str()
        .unwrap_or_default();
    assert!(
        url.starts_with("data:image/png;base64,"),
        "图片应以 data URL 发送：{url}"
    );
}

#[test]
fn patch_with_bare_body_rows_is_repaired() {
    let home = temp_home("bare-home");
    let ws = temp_home("bare-ws");
    std::fs::write(ws.join("a.txt"), "one\ntwo\nthree\n").unwrap();
    let tag = file_tag(ws.join("a.txt"));
    // 模型漏了 `+`，按容错规则应当自动补上并提示。
    let patch = format!("[a.txt#{tag}]\nPUT 2.=2:\nTWO\n");

    let server = start_server(vec![
        tool_response(&[("c1", "read", json!({"path": "a.txt"}))]),
        plain_response(&["ok"]),
        tool_response(&[("c2", "edit", json!({ "input": patch }))]),
        plain_response(&["ok"]),
    ]);
    let mut agent = open_agent(server.port, &home, &ws);
    collect(&mut agent, "读");
    collect(&mut agent, "改");

    let output = last_tool_output(&home);
    assert!(output.contains("没有以 + 开头"), "{output}");
    assert!(output.starts_with("（提醒"), "{output}");
    assert!(output.contains("a.txt#"), "容错后仍应返回新锚点：{output}");
    assert_eq!(
        std::fs::read_to_string(ws.join("a.txt")).unwrap(),
        "one\nTWO\nthree\n"
    );
}

#[test]
fn patch_envelope_is_ignored() {
    let home = temp_home("envelope-home");
    let ws = temp_home("envelope-ws");
    std::fs::write(ws.join("a.txt"), "one\ntwo\n").unwrap();
    let tag = file_tag(ws.join("a.txt"));
    let patch = format!("*** Begin Patch\n[a.txt#{tag}]\nPUT 2.=2:\n+TWO\n*** End Patch\n");

    let server = start_server(vec![
        tool_response(&[("c1", "read", json!({"path": "a.txt"}))]),
        plain_response(&["ok"]),
        tool_response(&[("c2", "edit", json!({ "input": patch }))]),
        plain_response(&["ok"]),
    ]);
    let mut agent = open_agent(server.port, &home, &ws);
    collect(&mut agent, "读");
    collect(&mut agent, "改");

    assert!(
        last_tool_output(&home).contains("a.txt#"),
        "带信封的补丁应当成功：{}",
        last_tool_output(&home)
    );
    assert_eq!(
        std::fs::read_to_string(ws.join("a.txt")).unwrap(),
        "one\nTWO\n"
    );
}

#[test]
fn a_patch_without_a_section_header_is_rejected() {
    let home = temp_home("bogus-home");
    let ws = temp_home("bogus-ws");
    std::fs::write(ws.join("a.txt"), "one\n").unwrap();

    let server = start_server(vec![
        tool_response(&[("c1", "edit", json!({"input": "BOGUS 1"}))]),
        plain_response(&["ok"]),
    ]);
    let mut agent = open_agent(server.port, &home, &ws);
    collect(&mut agent, "乱写一个补丁");

    let output = last_tool_output(&home);
    assert!(output.starts_with("ERROR:"), "{output}");
    assert_eq!(std::fs::read_to_string(ws.join("a.txt")).unwrap(), "one\n");
}

#[test]
fn reading_a_second_range_keeps_earlier_lines_editable() {
    let home = temp_home("merge-home");
    let ws = temp_home("merge-ws");
    std::fs::write(ws.join("a.txt"), "one\ntwo\nthree\nfour\n").unwrap();
    let tag = file_tag(ws.join("a.txt"));
    let patch = format!("[a.txt#{tag}]\nPUT 1.=1:\n+ONE\n");

    let server = start_server(vec![
        tool_response(&[("c1", "read", json!({"path": "a.txt", "limit": 2}))]),
        plain_response(&["ok"]),
        tool_response(&[(
            "c2",
            "read",
            json!({"path": "a.txt", "offset": 3, "limit": 2}),
        )]),
        plain_response(&["ok"]),
        tool_response(&[("c3", "edit", json!({ "input": patch }))]),
        plain_response(&["ok"]),
    ]);
    let mut agent = open_agent(server.port, &home, &ws);
    collect(&mut agent, "读前两行");
    collect(&mut agent, "读后两行");
    collect(&mut agent, "改第一行");

    assert!(
        last_tool_output(&home).starts_with("a.txt#"),
        "第二次 read 不应抹掉第一次的已读行：{}",
        last_tool_output(&home)
    );
    assert_eq!(
        std::fs::read_to_string(ws.join("a.txt")).unwrap(),
        "ONE\ntwo\nthree\nfour\n"
    );
}

#[test]
fn multi_file_patch_is_all_or_nothing() {
    let home = temp_home("atomic-home");
    let ws = temp_home("atomic-ws");
    std::fs::write(ws.join("a.txt"), "one\n").unwrap();
    std::fs::write(ws.join("b.txt"), "two\n").unwrap();
    let tag_a = file_tag(ws.join("a.txt"));

    let patch = format!("[a.txt#{tag_a}]\nPUT 1.=1:\n+ONE\n[b.txt#ZZZZ]\nPUT 1.=1:\n+TWO\n");
    let server = start_server(vec![
        tool_response(&[
            ("c1", "read", json!({"path": "a.txt"})),
            ("c2", "read", json!({"path": "b.txt"})),
        ]),
        plain_response(&["ok"]),
        tool_response(&[("c3", "edit", json!({ "input": patch }))]),
        plain_response(&["ok"]),
    ]);
    let mut agent = open_agent(server.port, &home, &ws);
    collect(&mut agent, "读两个文件");
    collect(&mut agent, "一起改");

    assert!(
        last_tool_output(&home).starts_with("ERROR:"),
        "第二段锚点错误应让整份补丁失败：{}",
        last_tool_output(&home)
    );
    assert_eq!(
        std::fs::read_to_string(ws.join("a.txt")).unwrap(),
        "one\n",
        "校验失败时第一个文件也不能写"
    );
}

#[test]
fn bash_does_not_clobber_user_redirection() {
    let home = temp_home("redir-home");
    let ws = temp_home("redir-ws");
    let server = start_server(vec![
        tool_response(&[(
            "c1",
            "bash",
            json!({"command": "echo out; echo err 1>&2; echo side > side.txt"}),
        )]),
        plain_response(&["ok"]),
    ]);
    let mut agent = open_agent(server.port, &home, &ws);
    collect(&mut agent, "跑命令");

    let output = last_tool_output(&home);
    assert!(output.contains("out"), "{output}");
    assert!(output.contains("err"), "stderr 也要合并进来：{output}");
    assert_eq!(
        std::fs::read_to_string(ws.join("side.txt")).unwrap().trim(),
        "side",
        "用户自己的重定向不能被覆盖"
    );
}

#[test]
fn grep_accepts_a_file_path() {
    let home = temp_home("grep-file-home");
    let ws = temp_home("grep-file-ws");
    std::fs::write(ws.join("a.go"), "alpha\nbeta\n").unwrap();
    let server = start_server(vec![
        tool_response(&[("c1", "grep", json!({"pattern": "beta", "path": "a.go"}))]),
        plain_response(&["ok"]),
    ]);
    let mut agent = open_agent(server.port, &home, &ws);
    collect(&mut agent, "搜单个文件");

    assert!(
        last_tool_output(&home).contains("a.go:2:beta"),
        "path 指向文件时也应命中：{}",
        last_tool_output(&home)
    );
}

#[test]
fn glob_matches_relative_to_the_given_path() {
    let home = temp_home("glob-base-home");
    let ws = temp_home("glob-base-ws");
    std::fs::create_dir_all(ws.join("src")).unwrap();
    std::fs::write(ws.join("src/a.go"), "package a\n").unwrap();
    std::fs::write(ws.join("root.go"), "package root\n").unwrap();

    let server = start_server(vec![
        tool_response(&[("c1", "glob", json!({"pattern": "*.go", "path": "src"}))]),
        plain_response(&["ok"]),
    ]);
    let mut agent = open_agent(server.port, &home, &ws);
    collect(&mut agent, "在 src 下找 go 文件");

    let output = last_tool_output(&home);
    assert!(output.contains("src/a.go"), "{output}");
    assert!(
        !output.contains("root.go"),
        "不应越过给定的搜索根：{output}"
    );
}

#[test]
fn bash_timeout_returns_even_with_grandchildren() {
    let home = temp_home("grandchild-home");
    let ws = temp_home("grandchild-ws");
    let server = start_server(vec![
        tool_response(&[(
            "c1",
            "bash",
            json!({"command": "(sleep 30) & sleep 30", "timeout": 1}),
        )]),
        plain_response(&["ok"]),
    ]);
    let mut agent = open_agent(server.port, &home, &ws);

    let started = std::time::Instant::now();
    collect(&mut agent, "跑会派生子进程的命令");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(4),
        "孙进程不能把超时拖成无限等待"
    );
    assert!(last_tool_output(&home).contains("超过 1 秒"));
}

#[test]
fn read_hints_the_next_offset_when_truncated() {
    let home = temp_home("hint-home");
    let ws = temp_home("hint-ws");
    std::fs::write(ws.join("a.txt"), "1\n2\n3\n4\n5\n").unwrap();
    let server = start_server(vec![
        tool_response(&[("c1", "read", json!({"path": "a.txt", "limit": 2}))]),
        plain_response(&["ok"]),
    ]);
    let mut agent = open_agent(server.port, &home, &ws);
    collect(&mut agent, "只读两行");

    let output = last_tool_output(&home);
    assert!(output.contains("共 5 行"), "{output}");
    assert!(
        output.contains("offset=3"),
        "应告诉模型怎么继续读：{output}"
    );
}

#[test]
fn edit_keeps_earlier_read_lines_editable_after_shift() {
    let home = temp_home("shift-home");
    let ws = temp_home("shift-ws");
    std::fs::write(ws.join("a.txt"), "l1\nl2\nl3\nl4\nl5\nl6\n").unwrap();
    let tag0 = tag_of_str("l1\nl2\nl3\nl4\nl5\nl6\n");
    let tag1 = tag_of_str("l1\na\nb\nl3\nl4\nl5\nl6\n");

    let shift_patch = format!("[a.txt#{tag0}]\nPUT 2.=2:\n+a\n+b\n");
    let unread_patch = format!("[a.txt#{tag1}]\nPUT 6.=6:\n+L5\n");
    let read_patch = format!("[a.txt#{tag1}]\nPUT 4.=4:\n+L3\n");

    let server = start_server(vec![
        tool_response(&[("c1", "read", json!({"path": "a.txt", "limit": 3}))]),
        plain_response(&["ok"]),
        tool_response(&[("c2", "edit", json!({ "input": shift_patch }))]),
        plain_response(&["ok"]),
        // 第 6 行从没读过：必须拒绝。
        tool_response(&[("c3", "edit", json!({ "input": unread_patch }))]),
        plain_response(&["ok"]),
        // 原来的第 3 行被挤到第 4 行；它读过，改它应当允许。
        tool_response(&[("c4", "edit", json!({ "input": read_patch }))]),
        plain_response(&["ok"]),
    ]);
    let mut agent = open_agent(server.port, &home, &ws);
    collect(&mut agent, "读前三行");
    collect(&mut agent, "插入两行");
    collect(&mut agent, "改没读过的第六行");
    collect(&mut agent, "改读过、被挤到第四行的那一行");

    let outputs = tool_outputs(&home);
    assert!(
        outputs[1].starts_with("a.txt#"),
        "插入应成功：{}",
        outputs[1]
    );
    assert!(
        outputs[2].starts_with("ERROR:"),
        "没读过的行必须拒绝：{}",
        outputs[2]
    );
    assert!(outputs[2].contains("没有读过"), "{}", outputs[2]);
    assert!(
        outputs[3].starts_with("a.txt#"),
        "读过的行应仍然可改：{}",
        outputs[3]
    );
    assert_eq!(
        std::fs::read_to_string(ws.join("a.txt")).unwrap(),
        "l1\na\nb\nL3\nl4\nl5\nl6\n"
    );
}

#[test]
fn bash_reports_the_exit_code() {
    let home = temp_home("exit-home");
    let ws = temp_home("exit-ws");
    let server = start_server(vec![
        tool_response(&[("c1", "bash", json!({"command": "exit 3"}))]),
        plain_response(&["ok"]),
    ]);
    let mut agent = open_agent(server.port, &home, &ws);
    collect(&mut agent, "跑一个失败的命令");

    assert!(
        last_tool_output(&home).contains("退出码：3"),
        "必须把退出码告诉模型：{}",
        last_tool_output(&home)
    );
}

#[test]
fn bash_runs_in_the_given_cwd() {
    let home = temp_home("cwd-home");
    let ws = temp_home("cwd-ws");
    std::fs::create_dir_all(ws.join("sub")).unwrap();
    let server = start_server(vec![
        tool_response(&[("c1", "bash", json!({"command": "pwd", "cwd": "sub"}))]),
        plain_response(&["ok"]),
    ]);
    let mut agent = open_agent(server.port, &home, &ws);
    collect(&mut agent, "在子目录里跑 pwd");

    let output = last_tool_output(&home);
    assert!(output.contains("/sub"), "应在指定目录执行：{output}");
}

#[test]
fn bash_rejects_a_cwd_that_is_not_a_directory() {
    let home = temp_home("badcwd-home");
    let ws = temp_home("badcwd-ws");
    let server = start_server(vec![
        tool_response(&[("c1", "bash", json!({"command": "pwd", "cwd": "missing"}))]),
        plain_response(&["ok"]),
    ]);
    let mut agent = open_agent(server.port, &home, &ws);
    collect(&mut agent, "在不存在的目录跑");

    assert!(last_tool_output(&home).starts_with("ERROR:"));
}

#[test]
fn grep_skips_snapshots_for_huge_files() {
    let home = temp_home("huge-home");
    let ws = temp_home("huge-ws");
    let mut content = String::from("first\nneedle\n");
    content.push_str(&"x".repeat(4 * 1024 * 1024));
    std::fs::write(ws.join("big.log"), &content).unwrap();
    let tag = tag_of_str(&content);

    let patch = format!("[big.log#{tag}]\nPUT 2.=2:\n+NEEDLE\n");
    let server = start_server(vec![
        tool_response(&[("c1", "grep", json!({"pattern": "needle"}))]),
        plain_response(&["ok"]),
        tool_response(&[("c2", "edit", json!({ "input": patch }))]),
        plain_response(&["ok"]),
    ]);
    let mut agent = open_agent(server.port, &home, &ws);
    collect(&mut agent, "搜大文件");
    assert!(
        last_tool_output(&home).contains("big.log:2:needle"),
        "{}",
        last_tool_output(&home)
    );

    collect(&mut agent, "直接改它");
    assert!(
        last_tool_output(&home).contains("还没被读过"),
        "超过 4 MB 的文件不记快照，改之前必须重读：{}",
        last_tool_output(&home)
    );
}

#[test]
fn describe_survives_a_command_whose_80th_byte_is_inside_a_chinese_character() {
    // 旧实现用 String::truncate(80) 按字节截断，第 80 个字节落在「中」中间时 panic。
    let command = format!("{}中{}", "a".repeat(79), "b".repeat(20));
    let call = yi::session::ToolCall {
        id: "c1".to_string(),
        name: "bash".to_string(),
        arguments: serde_json::json!({ "command": command }).to_string(),
    };
    let tools = yi::tools::Tools::new(
        std::env::temp_dir(),
        std::env::temp_dir().join("yi-logs"),
        "gopls".to_string(),
        "phpantom_lsp".to_string(),
        "rust-analyzer".to_string(),
    );
    let line = tools.describe(&call);
    assert!(line.contains('中'), "整条命令都要留下：{line}");
}

#[test]
fn grep_and_glob_skip_vendored_and_build_directories() {
    let home = temp_home("skipdir-home");
    let ws = temp_home("skipdir-ws");
    for dir in ["node_modules/pkg", "target", "vendor/lib"] {
        std::fs::create_dir_all(ws.join(dir)).unwrap();
        std::fs::write(ws.join(dir).join("hit.go"), "needle\n").unwrap();
    }
    std::fs::create_dir_all(ws.join("src")).unwrap();
    std::fs::write(ws.join("src/keep.go"), "needle\n").unwrap();

    let server = start_server(vec![
        tool_response(&[("c1", "grep", json!({"pattern": "needle"}))]),
        plain_response(&["ok"]),
        tool_response(&[("c2", "glob", json!({"pattern": "**/*.go"}))]),
        plain_response(&["ok"]),
    ]);
    let mut agent = open_agent(server.port, &home, &ws);
    collect(&mut agent, "搜 needle");
    let grep_out = last_tool_output(&home);
    assert!(grep_out.contains("src/keep.go"), "{grep_out}");
    assert!(!grep_out.contains("node_modules"), "{grep_out}");
    assert!(!grep_out.contains("target/"), "{grep_out}");
    assert!(!grep_out.contains("vendor/"), "{grep_out}");

    collect(&mut agent, "列出所有 go 文件");
    let glob_out = last_tool_output(&home);
    assert!(glob_out.contains("src/keep.go"), "{glob_out}");
    assert!(!glob_out.contains("node_modules"), "{glob_out}");
    assert!(!glob_out.contains("target/"), "{glob_out}");
    assert!(!glob_out.contains("vendor/"), "{glob_out}");
}
