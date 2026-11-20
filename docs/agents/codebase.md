# 代码库细节（按需读取）

`AGENTS.md` 只留每轮都需要的规则；这里是展开细节，需要时再读，不必进系统提示词。

## 模块与职责

| 路径 | 作用 |
|---|---|
| `src/main.rs` | 薄壳：参数、读输入、斜杠命令、页脚、图片路径识别 |
| `src/agent.rs` | 一轮对话的编排：循环、工具分发、连续失败检测、压缩触发 |
| `src/provider.rs` | OpenAI 兼容的 HTTP/SSE 客户端；请求体、重试、用量解析、`x-opencode-session` 头 |
| `src/session.rs` | JSONL 会话：追加、上下文重建、压缩规划、中断修复、`/new` 归档 |
| `src/tools.rs` | 八个工具与 JSON Schema；锚点快照、bash 日志与进程组 |
| `src/db.rs` | 按项目的数据库配置（`$YI_HOME/projects/<编码>/db.json`）与 profile 解析 |
| `src/config.rs` | 供应商预置、配置文件、密钥与代理、系统提示词组装 |
| `src/render.rs` | 行式 markdown：标题/列表/引用/表格对齐、代码块交 `bat` |
| `src/complete.rs` | 斜杠命令与技能名补全、行内提示 |
| `src/lsp.rs` | `gopls` / `phpantom_lsp` / `rust-analyzer` 客户端，按（语言, 项目根）缓存并回收 |
| `src/skills.rs` | 扫描 `SKILL.md`，生成提示词条目，展开 `/skill:` |
| `src/cancel.rs` | 本轮取消标志：SIGINT 处理器 + 终端 `0x03` 字节，两条入口同一个原子量 |
| `src/crash.rs` | panic 钩子，把崩溃位置写成会话里的 `notice` |

## 数据流

```
main::run
  → input::Input::read（reedline 或管道逐行）
  → Agent::submit_with_images(prompt, images)
      → Session::append_user / append_image
      → loop:
          Session::context()            # 摘要 + 未越界消息，丢掉残缺的 tool_calls 对
          Provider::stream(...)         # 工作线程发请求、读 SSE，经 mpsc 回主线程
          on(Event::{Text, Reasoning, ToolCall, ToolResult, Notice})
          有 tool_calls → Tools::execute → Session::append_tool_result → 回到 loop
          没有          → 退出 loop
      → maybe_compact（超过窗口一半就写 compaction 条目）
      → Usage
  → 打印页脚、cancel::disarm、Renderer::finish
```

结构性事实：没有异步运行时；取消有两条入口（SIGINT / 终端字节）共用一个原子标志；会话前缀只追加才能命中供应商缓存；工具错误以 `ERROR: ...` 回灌而不中断循环。

## 错误处理

- 统一 `crate::Result<T>`；`Error` 变体：`Io`、`Json`、`Http`、`Api`、`Config`、`Cancelled`，`Display` 是中文。
- `Error::Cancelled` 是控制流信号，不是失败：`Agent::submit_with_images` 特判它并写 `notice`。
- 锁中毒用 `unwrap_or_else(|e| e.into_inner())` 降级；生产代码不出现 `unwrap` / `expect` / `panic!`。

## 测试基建

- `scripts/tui.py`：在伪终端里跑 yi、发按键、打印屏幕；界面改动用它验证，`|` 表示「发一段、截一次屏」。首次运行会在 `~/.cache/yi-tui/venv` 里装好 pyte，不碰系统 Python。
- `tests/common/mod.rs`：`start_server(Vec<String>)` 是手写 HTTP/SSE 服务器（标准库 `TcpListener`），记录 `FakeServer.bodies` / `.headers`；响应构造器 `sse_response`、`plain_response`、`tool_response`、`json_response`、`big_usage_response`；`temp_home(tag)` 生成唯一临时目录。
- 配置通过 `config(port, home, effort)` 显式注入，**不改进程环境变量**；每个用例必须用不同会话路径（文件锁会冲突）。
- 进程级全局状态要串行化：`tests/cancel.rs` 用 `static GATE: Mutex<()>`，`tests/crash.rs` 只有一个用例（`OnceLock`）。
- 需要外部程序时先探测再跳过：`gopls_available()` / `rust_analyzer_available()` 失败就打印 `SKIP: ...` 后 return。
- 用例名是英文完整句子描述行为（`edit_rejects_a_stale_anchor`），一个用例证明一条行为；断言消息用中文并带实际值。
- 已知较慢：`lsp_reports_a_timeout_instead_of_hanging`（约 15 秒）、`bash_timeout_*`；没有 `#[ignore]`。

## 运行环境与配置

- 平台：macOS / Linux（`src/tools.rs` 用了 `std::os::unix::process::CommandExt`），不支持 Windows。
- Rust 1.89+（会话文件锁用到 `File::try_lock`）；edition 2024。
- 外部命令：`rg`（`grep` / `glob` 必需）、`/bin/sh`；可选 `bat`、`gopls`、`phpantom_lsp`、`rust-analyzer`、`git`。
- 环境变量：`YI_HOME`（默认 `~/.yi`）、`OPENCODE_API_KEY` / `DEEPSEEK_API_KEY`、`YI_PROXY_<供应商>` → `YI_PROXY`（不读通用 `HTTPS_PROXY` / `ALL_PROXY`）、`NO_COLOR`。配置文件里的 `api_key` 会被忽略。
- 数据目录：`~/.yi/sessions/<编码路径>/session.jsonl`；同一项目只允许一个实例（文件锁）。

## 文档地图

- `README.md`：使用者视角。
- `.scratch/yi-phase-1/spec.md`：第一期规格与验收用例。
- `docs/adr/`：决定记录，编号递增；推翻旧决定时在新 ADR 里声明，并在旧条目上标注取代。
- `docs/dev-notes.md`：为什么这样设计 + 踩过的坑。
- `GLOSSARY.md`：领域词汇，写文档与命名时用它，不用 `_Avoid_` 里的同义词。
- `prompts/`：`system.md` 与 `compaction.md`，编译期内嵌。
