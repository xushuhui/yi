//! Ctrl-C 中断本轮：命令要尽快停下，会话里留下一条说明。

mod common;

use std::time::{Duration, Instant};

use common::*;
use std::sync::Mutex;

use yi::agent::Agent;
use yi::cancel;

/// 取消标志是进程级全局量：两个用例并行会互相 reset，必须排队。
static GATE: Mutex<()> = Mutex::new(());

#[test]
fn ctrl_c_stops_a_running_command_and_keeps_the_session() {
    let _guard = GATE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
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
    assert!(
        result.is_err(),
        "被取消的这一轮应当返回错误，实际 {result:?}"
    );
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

/// 响应头已经回来、正文一个字节都没发：模拟模型「思考中」。
/// 这时候按 Ctrl-C 必须立刻停下，不能等 socket 有数据。
#[test]
fn ctrl_c_cancels_while_the_model_is_still_thinking() {
    let _guard = GATE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    use std::io::{Read, Write};
    use std::net::TcpListener;

    cancel::reset();
    let listener = TcpListener::bind("127.0.0.1:0").expect("绑定端口");
    let port = listener.local_addr().expect("地址").port();
    std::thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let mut buffer = [0u8; 4096];
            let _ = stream.read(&mut buffer);
            let _ = stream.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
            );
            let _ = stream.flush();
            // 故意不说话，直到测试结束。
            std::thread::sleep(Duration::from_secs(5));
        }
    });

    let home = temp_home("cancel-think");
    let workspace = temp_home("cancel-think-ws");
    let mut agent = Agent::new(
        &config(port, &home, None),
        &workspace.to_string_lossy(),
        &session_path(&home),
    )
    .expect("打开会话");

    let trigger = std::thread::spawn(|| {
        std::thread::sleep(Duration::from_millis(300));
        cancel::request();
    });
    let started = Instant::now();
    let result = agent.submit("hi", &mut |_| {});
    let elapsed = started.elapsed();
    trigger.join().expect("触发线程");
    cancel::reset();

    assert!(
        matches!(result, Err(yi::Error::Cancelled)),
        "应当返回取消，而不是继续等：{result:?}"
    );
    assert!(elapsed < Duration::from_secs(2), "取消太慢：{elapsed:?}");
}
