//! 仓库自己的两条硬约束：工具顺序、直接依赖上限。
//!
//! 这两条原来只写在 `AGENTS.md` 里，改错了没有任何东西会失败：
//! 工具顺序变了会打掉供应商的前缀缓存，依赖超了会违背「能不加就不加」的约定。

use serde_json::Value;

fn tool_names(definitions: &Value) -> Vec<String> {
    definitions
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
fn the_tool_order_stays_stable_for_the_prefix_cache() {
    let without_search = tool_names(&yi::tools::Tools::definitions(false));
    assert_eq!(
        without_search,
        [
            "read", "write", "edit", "bash", "grep", "lsp", "glob", "task"
        ],
        "工具顺序变化会让每次请求的前缀失效；顺序是有意固定的"
    );
    let with_search = tool_names(&yi::tools::Tools::definitions(true));
    assert_eq!(
        with_search,
        [
            "read",
            "write",
            "edit",
            "bash",
            "grep",
            "lsp",
            "glob",
            "task",
            "web_search"
        ],
        "web_search 只在有密钥时出现，并且永远追加在最后"
    );
}

#[test]
fn direct_dependencies_stay_within_the_budget() {
    const MANIFEST: &str = include_str!("../Cargo.toml");
    let mut in_dependencies = false;
    let mut count = 0;
    for line in MANIFEST.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_dependencies = line == "[dependencies]";
            continue;
        }
        if !in_dependencies || line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.contains('=') {
            count += 1;
        }
    }
    assert!(
        count <= 6,
        "直接依赖 {count} 个，超过 6 个上限；加依赖之前先问，并优先用标准库"
    );
}
