//! LSP 集成测试：走同一个接缝（Agent::submit），对着真实的 gopls 跑。
//! 机器上没有 gopls 时跳过，不算失败。

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

fn last_tool_output(home: &Path) -> String {
    session_entries(home)
        .iter()
        .filter(|entry| entry["message"]["role"] == "tool")
        .filter_map(|entry| entry["message"]["content"].as_str().map(str::to_string))
        .next_back()
        .unwrap_or_default()
}

fn gopls_available() -> bool {
    std::process::Command::new("gopls")
        .arg("version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

#[test]
fn lsp_finds_a_go_definition() {
    if !gopls_available() {
        eprintln!("跳过：PATH 里没有 gopls");
        return;
    }

    let home = temp_home("lsp-home");
    let ws = temp_home("lsp-ws");
    std::fs::write(ws.join("go.mod"), "module demo\n\ngo 1.22\n").unwrap();
    std::fs::write(
        ws.join("main.go"),
        "package main\n\nfunc helper() int { return 1 }\n\nfunc main() { _ = helper() }\n",
    )
    .unwrap();

    let server = start_server(vec![
        tool_response(&[(
            "c1",
            "lsp",
            json!({"action": "definition", "path": "main.go", "line": 5, "symbol": "helper"}),
        )]),
        plain_response(&["ok"]),
    ]);
    let mut agent = open_agent(server.port, &home, &ws);
    collect(&mut agent, "找 helper 的定义");

    let output = last_tool_output(&home);
    assert!(output.contains("main.go:3"), "应跳到第 3 行：{output}");
    assert!(output.contains("func helper"), "应带上那一行内容：{output}");
}

#[test]
fn lsp_rejects_an_unsupported_extension() {
    let home = temp_home("lsp-bad-home");
    let ws = temp_home("lsp-bad-ws");
    std::fs::write(ws.join("a.txt"), "hello\n").unwrap();

    let server = start_server(vec![
        tool_response(&[(
            "c1",
            "lsp",
            json!({"action": "definition", "path": "a.txt", "line": 1}),
        )]),
        plain_response(&["ok"]),
    ]);
    let mut agent = open_agent(server.port, &home, &ws);
    collect(&mut agent, "查一个 txt");

    let output = last_tool_output(&home);
    assert!(output.starts_with("ERROR:"), "{output}");
    assert!(output.contains("只支持 .go 和 .php"), "{output}");
}

fn phpantom_available() -> bool {
    std::process::Command::new("phpantom_lsp")
        .arg("--version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

#[test]
fn lsp_finds_a_php_definition() {
    if !phpantom_available() {
        eprintln!("跳过：PATH 里没有 phpantom_lsp");
        return;
    }

    let home = temp_home("lsp-php-home");
    let ws = temp_home("lsp-php-ws");
    std::fs::write(ws.join("composer.json"), "{\"name\":\"demo/demo\"}\n").unwrap();
    std::fs::write(
        ws.join("a.php"),
        "<?php\nfunction helper() { return 1; }\nhelper();\n",
    )
    .unwrap();

    let server = start_server(vec![
        tool_response(&[(
            "c1",
            "lsp",
            json!({"action": "definition", "path": "a.php", "line": 3, "symbol": "helper"}),
        )]),
        plain_response(&["ok"]),
    ]);
    let mut agent = open_agent(server.port, &home, &ws);
    collect(&mut agent, "找 PHP 里 helper 的定义");

    let output = last_tool_output(&home);
    assert!(output.contains("a.php:2"), "应跳到第 2 行：{output}");
}

#[test]
fn lsp_finds_references_from_a_nested_directory() {
    if !gopls_available() {
        eprintln!("SKIP: PATH 里没有 gopls");
        return;
    }
    let home = temp_home("lsp-ref-home");
    let ws = temp_home("lsp-ref-ws");
    std::fs::create_dir_all(ws.join("cmd")).unwrap();
    std::fs::write(ws.join("go.mod"), "module demo\n\ngo 1.22\n").unwrap();
    std::fs::write(
        ws.join("cmd/main.go"),
        "package main\n\nfunc helper() int { return 1 }\n\nfunc main() { _ = helper() }\n",
    )
    .unwrap();

    let server = start_server(vec![
        tool_response(&[(
            "c1",
            "lsp",
            json!({"action": "references", "path": "cmd/main.go", "line": 5, "symbol": "helper"}),
        )]),
        plain_response(&["ok"]),
    ]);
    let mut agent = open_agent(server.port, &home, &ws);
    collect(&mut agent, "找 helper 的引用");

    let output = last_tool_output(&home);
    assert!(
        output.contains("main.go:3") || output.contains("main.go:5"),
        "{output}"
    );
}

#[test]
fn lsp_survives_a_non_ascii_path() {
    if !gopls_available() {
        eprintln!("SKIP: PATH 里没有 gopls");
        return;
    }
    let home = temp_home("lsp-cn-home");
    let ws = temp_home("lsp-cn-ws");
    let chinese = ws.join("中文 目录");
    std::fs::create_dir_all(&chinese).unwrap();
    std::fs::write(ws.join("go.mod"), "module demo\n\ngo 1.22\n").unwrap();
    std::fs::write(
        chinese.join("main.go"),
        "package main\n\nfunc helper() int { return 1 }\n\nfunc main() { _ = helper() }\n",
    )
    .unwrap();

    let server = start_server(vec![
        tool_response(&[(
            "c1",
            "lsp",
            json!({"action": "definition", "path": "中文 目录/main.go", "line": 5, "symbol": "helper"}),
        )]),
        plain_response(&["ok"]),
    ]);
    let mut agent = open_agent(server.port, &home, &ws);
    collect(&mut agent, "在中文目录里找定义");

    let output = last_tool_output(&home);
    assert!(
        output.contains("main.go:3"),
        "中文路径下也要能定位：{output}"
    );
    assert!(
        output.contains("func helper"),
        "URI 解码正确才拿得到代码片段：{output}"
    );
}

#[test]
fn lsp_reports_a_timeout_instead_of_hanging() {
    use std::os::unix::fs::PermissionsExt;

    let home = temp_home("lsp-timeout-home");
    let ws = temp_home("lsp-timeout-ws");
    std::fs::write(ws.join("go.mod"), "module demo\n\ngo 1.22\n").unwrap();
    std::fs::write(ws.join("main.go"), "package main\n\nfunc main() {}\n").unwrap();

    // 一个永远不回话的"服务器"：只睡，不读也不写。
    let script = ws.join("silent-server.sh");
    std::fs::write(&script, "#!/bin/sh\nsleep 30\n").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

    let server = start_server(vec![
        tool_response(&[(
            "c1",
            "lsp",
            json!({"action": "definition", "path": "main.go", "line": 3, "symbol": "main"}),
        )]),
        plain_response(&["ok"]),
    ]);
    let mut cfg = config(server.port, &home, None);
    cfg.gopls = script.to_string_lossy().to_string();
    let mut agent =
        Agent::new(&cfg, &ws.to_string_lossy(), &session_path(&home)).expect("打开会话");

    let started = std::time::Instant::now();
    collect(&mut agent, "查定义");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(25),
        "超时必须自己返回，不能一直等"
    );
    let output = last_tool_output(&home);
    assert!(output.starts_with("ERROR:"), "{output}");
    assert!(output.contains("超时"), "{output}");
}

#[test]
fn lsp_does_not_start_a_server_for_an_unknown_action() {
    let home = temp_home("lsp-action-home");
    let ws = temp_home("lsp-action-ws");
    std::fs::write(ws.join("a.go"), "package main\n").unwrap();

    let server = start_server(vec![
        tool_response(&[(
            "c1",
            "lsp",
            json!({"action": "hover", "path": "a.go", "line": 1}),
        )]),
        plain_response(&["ok"]),
    ]);
    let mut agent = open_agent(server.port, &home, &ws);
    collect(&mut agent, "查一个不支持的动作");

    let output = last_tool_output(&home);
    assert!(output.starts_with("ERROR:"), "{output}");
    assert!(output.contains("未知的 lsp 动作"), "{output}");
}

#[test]
fn lsp_reports_go_diagnostics() {
    if !gopls_available() {
        eprintln!("SKIP: PATH 里没有 gopls");
        return;
    }
    let home = temp_home("lsp-diag-home");
    let ws = temp_home("lsp-diag-ws");
    std::fs::write(ws.join("go.mod"), "module demo\n\ngo 1.22\n").unwrap();
    std::fs::write(
        ws.join("main.go"),
        "package main\n\nfunc main() {\n\tvar x int = \"文字\"\n\t_ = x\n}\n",
    )
    .unwrap();

    let server = start_server(vec![
        tool_response(&[(
            "c1",
            "lsp",
            json!({"action": "diagnostics", "path": "main.go"}),
        )]),
        plain_response(&["ok"]),
    ]);
    let mut agent = open_agent(server.port, &home, &ws);
    collect(&mut agent, "看诊断");

    let output = last_tool_output(&home);
    assert!(output.contains("错误"), "应报出类型错误：{output}");
}
