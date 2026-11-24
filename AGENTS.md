# Repository Guidelines

`yi` 是单人使用的终端编程 agent：Rust 单二进制、单进程、行式界面，一个项目一条会话线。默认供应商 `opencode-go`（模型 `deepseek-v4.1-flash`），另一套是 DeepSeek 官方；九个工具（含只读子 agent 的 `task`、按密钥出现的 `web_search`）；不做记忆、多供应商抽象、更深层的子 agent、MCP、会话树、审批沙箱。

模块职责、测试基建、运行环境等细节见 `docs/agents/codebase.md`，需要时再读。

## 架构要点

- 二进制是薄壳，逻辑都在库里；唯一测试接缝是 `Agent::submit` / `submit_with_images`。
- 一轮对话 = `Agent::run_turn` 的循环：`Session::context` → `Provider::stream` → 处理事件 → 有工具调用就 `Tools::execute` 并把结果回灌 → 直到某次回复不再调用工具。
- 没有异步运行时：线程 + `mpsc` + 原子标志。不要引入 tokio。
- 会话是只追加的 JSONL；前缀不改写才能命中供应商的缓存。
- 工具错误以 `ERROR: ...` 文本回灌给模型，不中断循环。

## 硬约束

- 不要调整 `Tools::definitions()` 里的工具顺序；不要随意给请求体加字段（前者破坏缓存，后者可能被供应商 400）。
- 新增直接依赖要先问：上限 6 个，现在 5 个（`serde`、`serde_json`、`ureq`、`reedline`、`libc`）。
- 危险命令必须人确认：`rm`、`dd`、`mkfs`、`sudo`、`kill` 等（含 `mke2fs`、`newfs_*`、`diskutil eraseDisk`）走 `cancel::confirm`，30 秒超时、非终端一律拒绝；不要加跳过开关。
- 生产代码不 panic：`unwrap` / `expect` / `panic!` 只允许出现在 `#[cfg(test)]` 与 `tests/`。
- 改了 `prompts/*.md` 必须重新 `cargo build` / `cargo install`（两个提示词是 `include_str!` 内嵌的）。
- 测试不改进程环境变量；每个用例必须用不同的会话路径，否则文件锁冲突。
- 改工具行为时同步改 `README.md` 的工具表与 `.scratch/yi-phase-1/spec.md` 的验收用例。
- 已知上限的地方写 `SIMPLIFICATION:` 注释，说明上限与升级路径。

## 常用命令

```sh
cargo test                              # 全量
cargo test --test tools                 # 单文件
cargo test --test lsp lsp_finds_a_go_definition -- --nocapture
cargo clippy --all-targets              # 保持零警告
cargo fmt
cargo install --path . --root ~/.local --force --offline
```

交付前至少跑四条：`cargo fmt`、`cargo clippy --all-targets`（零警告）、`cargo test`，最后 `cargo install --path . --root ~/.local --force --offline`——每次改完都装，别让常驻的 yi 跑旧二进制。界面改动还要用 `scripts/tui.py` 在伪终端里真跑一遍（用法见 `docs/agents/codebase.md`），测试通过不算完成。

## 风格

- 标识符英文（`snake_case` / `CamelCase` / `SCREAMING_SNAKE`）；注释、断言消息、文档正文用中文。
- 模块头 `//!` 讲**为什么这样设计**并引用 ADR，不复述代码。
- 只在跨模块使用时 `pub`；单元测试放文件末尾的 `#[cfg(test)] mod tests`。
- 用 edition 2024 的 let-chains（`if let ... && ...`）与 `let-else`。
- 测试用例名是英文完整句子描述行为（`edit_rejects_a_stale_anchor`），不用 `test_` 前缀。

## Agent skills

### Issue tracker

问题单和规格以 Markdown 文件放在 `.scratch/` 下。见 `docs/agents/issue-tracker.md`。

### Triage labels

沿用默认五个标签：`needs-triage`、`needs-info`、`ready-for-agent`、`ready-for-human`、`wontfix`。见 `docs/agents/triage-labels.md`。

### Domain docs

单上下文：仓库根 `GLOSSARY.md`，决定记录在 `docs/adr/`。见 `docs/agents/domain.md`。
