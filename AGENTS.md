# yi

单人使用的终端编程 agent：Rust、单进程、只接 DeepSeek 的 `deepseek-flash`、一个项目一条会话线。

## Agent skills

### Issue tracker

问题单和规格以 Markdown 文件放在 `.scratch/` 下。见 `docs/agents/issue-tracker.md`。

### Triage labels

沿用默认五个标签：`needs-triage`、`needs-info`、`ready-for-agent`、`ready-for-human`、`wontfix`。见 `docs/agents/triage-labels.md`。

### Domain docs

单上下文：仓库根 `GLOSSARY.md`，决定记录在 `docs/adr/`。见 `docs/agents/domain.md`。
