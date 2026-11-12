# yi

跑在终端里的编程 agent：一个二进制、一个进程、一个供应商（DeepSeek）、一个模型（`deepseek-flash`）。
它只服务一个使用者，目标是常驻终端、随手切项目，而不是每次开一个新工具。

名字取自《易经》。日常用法：

```sh
cd ~/gp/some-project
yi
```

## 它刻意不做的事

- 不做多角色、多 agent 编排：一次只跑一条对话线。
- 不做记忆、学习、自动检索历史会话。
- 不接多家供应商，不接多个模型；换供应商要改代码，不是改配置。
- 不做全屏 TUI：输入输出逐行打印，重定向到文件也能看。

这些取舍写在 [docs/adr/0001-single-process-single-provider.md](docs/adr/0001-single-process-single-provider.md) 和 [docs/adr/0002-line-oriented-ui.md](docs/adr/0002-line-oriented-ui.md)；术语见 [GLOSSARY.md](GLOSSARY.md)。

## 环境要求

- macOS 或 Linux。
- Rust 1.85 以上（项目用 2024 版语言标准）。
- [ripgrep](https://github.com/BurntSushi/ripgrep)：`grep` 和 `glob` 两个工具直接调用 `rg`，没有它这两个工具会报错。
- DeepSeek API key。
- 可选：`gopls`（Go 语言服务器）、`phpantom_lsp`（PHP 语言服务器），只有用到 `lsp` 工具时才需要。
- 可选：`bat`，用来给代码块着色；没装时代码块以纯文本显示，功能不受影响。

## 安装

```sh
git clone git@github.com:xushuhui/yi.git
cd yi
cargo install --path .
```

`cargo install` 会把可执行文件放到 `~/.cargo/bin/yi`；确认这个目录在 `PATH` 里。

## 配置

配置文件 `~/.yi/config.json`，全部字段可选：

```json
{
  "api_key": "sk-...",
  "base_url": "https://api.deepseek.com",
  "model": "deepseek-flash",
  "effort": "auto"
}
```

- `effort` 是推理强度，可选 `auto` / `low` / `high` / `max`；`auto` 表示请求里不发送这个参数，由供应商决定。
- 语言服务器命令固定取 `PATH` 里的 `gopls` / `phpantom_lsp`，不写进配置文件。
- 环境变量会覆盖文件：`DEEPSEEK_API_KEY` 覆盖 `api_key`，`YI_HOME` 覆盖数据目录（默认 `~/.yi`）。

## 用法

```sh
yi          # 进入当前项目的会话
yi -c       # 同上；继续会话是默认行为，-c 只是为了保留手感
```

启动时打印模型和会话文件路径，然后进入 `> ` 提示符。输入一句话回车即可；Ctrl-D 退出。

终端里输入由行编辑器接管：上下键翻当前会话的历史（最近 200 条，连续重复只留一条），左右键移动光标，Home/End、Ctrl-A/E 可用。历史是你敲的原文，`/skill:` 不会把展开后的技能正文存进去。输入行里按 Ctrl-C 放弃这一行并回到提示符；模型正在输出时按 Ctrl-C 仍然直接结束进程。

从管道或脚本喂输入时（`echo "..." | yi`）不接管按键，行为与以前一致：逐行读取。

## 输出长什么样

模型的回答按 markdown 渲染：标题加粗上色，列表换成圆点，引用画成竖线，行内代码反色显示，链接后面跟一个暗色的地址。表格保留原样，只把分隔线调暗——中英文混排重新对齐容易算错宽度。

代码块会等模型写完整个围栏再显示，整块交给 `bat` 着色，头上带一行语言标签：

```
── go ──
func main() { ... }
```

代码本身不加任何前缀，复制粘贴不受影响。工具调用失败时打一行红色原因；`edit` 成功时打一行绿色摘要（改了多少行）；成功的命令输出不回显。

颜色只在标准输出是终端时开启。重定向到文件、或设置 `NO_COLOR=1` 时，输出原样透传，不含任何转义序列。

斜杠命令：

| 命令 | 作用 |
| --- | --- |
| `/new` | 把当前会话归档、开一条新的 |
| `/effort <档位>` | 切换推理强度：`auto`、`low`、`high`、`max` |
| `/skill:<名字> [要求]` | 手动调用某个技能，把它的正文注入这一轮 |
| `/help` | 显示命令、环境变量和配置字段 |

### 一个项目一条会话

会话按「启动时所在目录的绝对路径」区分，精确匹配：

- 在项目根启动 → 一条会话；在它的子目录启动 → **另一条**会话。
- 进程崩溃（panic）时，崩溃原因和代码位置会作为一条 `notice` 写进会话文件，事后直接 grep 就能看到。
- 会话文件固定在 `~/.yi/sessions/<目录编码>/session.jsonl`，一条会话就是这一个文件，追加写、不分叉。
- `yi` 永远续写同一个文件，所以不需要"结束会话"。想从头开始就 `/new`，旧文件会改名归档。

### 上下文与压缩

页脚每轮显示一行用量：

```
[上下文 12% · 缓存命中 80%（3.2k / 4.0k）· 本轮 4.0k in / 232 out]
```

- `上下文` 是这一轮请求的输入 token 占模型窗口（`deepseek-flash` 为 1,048,576）的百分比。
- `缓存命中` 是供应商报告的命中 token 数除以输入总数——输入前缀越稳定，命中越高。
- 输入超过窗口的 50%（524,288 token）时，本轮结束后自动把最早的一段对话压成摘要，只保留最近约 20,000 token 的原文，然后继续。压缩对使用者无感，但会让前缀缓存失效一次。

## 工具

模型只拿到七个工具。所有路径都相对项目根（启动目录）。

| 工具 | 做什么 |
| --- | --- |
| `read` | 读文本文件（带行号和内容校验锚点），或列目录一层；也接受图片路径（png/jpg/jpeg/webp/gif，≤ 20 MB），图片直接作为输入发给模型 |
| `write` | 新建或覆盖文件，自动创建父目录 |
| `edit` | 按行号打补丁，只能改这次会话里真正读过的行 |
| `bash` | 在项目根执行 shell 命令，返回合并后的输出尾部，默认 300 秒超时 |
| `grep` | 正则搜内容，返回 `文件:行号:内容`；底层是 `rg` |
| `glob` | 按文件名通配查找；底层是 `rg --files`，遵守 `.gitignore` |
| `lsp` | 查 Go/PHP 的跳定义、找引用、看诊断 |

一轮对话不限制总步数：只要每次调用有变化就继续做，长任务不会被步数掐断。但如果同一个调用（工具名和参数完全相同）连续失败 3 次，会停止调用工具，让模型说明现状后再结束——这是唯一会自动中断的情况。

### 锚点：为什么 `edit` 不会改错行

`read` 返回的第一行长这样：

```
[src/main.rs#1A2B]
```

`#1A2B` 是根据整个文件内容算出的 4 位十六进制锚点。之后 `edit` 必须带同一个锚点：

```
[src/main.rs#1A2B]
PUT 12.=13:
+func main() {}
+
```

文件在这期间被别的程序改过，锚点就对不上，`edit` 会拒绝并要求重新 `read`。补丁语法只有四种：`PUT N.=M:` 替换第 N~M 行、`PUT <N:` 在第 N 行前插入、`PUT >N:` 在第 N 行后插入、`CUT N.=M` 删除第 N~M 行。

### 语言服务（LSP）

- 支持 `.go`（`gopls`）和 `.php`（`phpantom_lsp`），其它扩展名直接报错。
- 按「语言 + 项目根」懒启动：第一次调用才起进程，项目根是最近的 `go.mod` / `composer.json` 所在目录。
- 后台每 60 秒清一次，空闲超过 10 分钟的服务器会被终止；`yi` 退出时全部终止；服务器自己崩了，下次调用自动重起。
- 三个动作：`definition`、`references`、`diagnostics`。前两个要给行号（从 1 开始），诊断不需要。
- 单次请求 15 秒超时，超时作为普通错误文本返回给模型，不会卡住终端。

## 系统提示词、AGENTS.md 与技能

- 系统提示词在 `prompts/system.md`，编译时内嵌进二进制，是唯一来源。
- 启动时再追加两份 `AGENTS.md`（如果存在）：`~/.yi/AGENTS.md`（用户级）和项目根的 `AGENTS.md`（项目级），以 `<context-file>` 块原样拼在后面。
- 技能是带 YAML 头部的 `SKILL.md`，扫描顺序：`<项目>/.yi/skills`、`~/.yi/skills`、`~/.agents/skills`。
  - 系统提示词里只列技能的名字、描述和路径，模型需要时用 `read` 读正文。
  - 头部写 `disable-model-invocation: true` 的技能不出现在提示词里，只能由人用 `/skill:<名字>` 调用。

## 开发

```sh
cargo test          # 单元 + 集成测试；集成测试用本地假服务器代替 DeepSeek
cargo clippy --all-targets
cargo fmt
```

- `src/agent.rs` 是核心：一轮对话的循环、工具分发、压缩。
- `src/provider.rs` 是 DeepSeek 的流式 HTTP/SSE 客户端。
- `src/tools.rs` 是七个工具的实现；`src/lsp.rs` 是语言服务器客户端。
- `src/session.rs` 负责 JSONL 会话文件的追加与重建。
- 第一期规格与验收用例在 [.scratch/yi-phase-1/spec.md](.scratch/yi-phase-1/spec.md)。

LSP 的集成测试需要本机装了 `gopls` / `phpantom_lsp`；缺哪个就打印 `SKIP` 并返回，不算失败。
