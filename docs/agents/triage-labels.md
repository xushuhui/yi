# 分类标签

技能们用五个固定的分类角色说话。这张表把角色映射到本仓库问题单里实际使用的标签字符串。

| 角色（技能内部叫法） | 本仓库使用的标签 | 含义 |
| -------------------- | ---------------- | ---- |
| `needs-triage` | `needs-triage` | 需要维护者评估这条问题 |
| `needs-info` | `needs-info` | 等待提出者补充信息 |
| `ready-for-agent` | `ready-for-agent` | 规格完整，可以交给无人值守的 agent |
| `ready-for-human` | `ready-for-human` | 必须人来实现 |
| `wontfix` | `wontfix` | 决定不做 |

当某个技能提到一个角色（例如「打上可以交给 agent 的标签」）时，使用上表「本仓库使用的标签」一栏的字符串。

要换词汇时，直接改这一栏。本地 Markdown 问题单里，标签写成票据文件靠上的 `Status:` 行的取值。
