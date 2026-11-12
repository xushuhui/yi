# 问题单：本地 Markdown

这个仓库的问题单和规格都以 Markdown 文件的形式放在 `.scratch/` 下。

## 约定

- 一个功能一个目录：`.scratch/<功能名>/`
- 规格文件：`.scratch/<功能名>/spec.md`
- 实现票据一人一个文件：`.scratch/<功能名>/issues/<NN>-<slug>.md`，从 `01` 开始编号，永远不要把所有票据合并成一个文件
- 分类状态写在每个票据文件靠上的 `Status:` 一行（角色字符串见 `triage-labels.md`）
- 评论和讨论追加到文件底部的 `## Comments` 标题下

## 当技能说「发布到问题单」

在 `.scratch/<功能名>/` 下新建一个文件（目录不存在就创建）。

## 当技能说「取出相关票据」

读取被引用的那个文件。用户通常会直接给出路径或票据编号。

## 寻路操作（供 `/wayfinder` 使用）

「地图」是一个文件，每个票据是它的一个子文件。

- **地图**：`.scratch/<专项>/map.md`（正文是 Notes / Decisions-so-far / Fog）
- **子票据**：`.scratch/<专项>/issues/NN-<slug>.md`，从 `01` 开始编号，正文写要回答的问题。`Type:` 一行记录票据类型（`research`/`prototype`/`grilling`/`task`）；`Status:` 一行记录 `claimed`/`resolved`
- **阻塞**：靠上的 `Blocked by: NN, NN` 一行。当它列出的每个文件都是 `resolved` 时，票据才算解除阻塞
- **前沿**：扫描 `.scratch/<专项>/issues/`，找未关闭、未阻塞、未被认领的文件；编号最小者优先
- **认领**：动手前把状态改成 `Status: claimed` 并保存
- **解决**：在 `## Answer` 标题下追加答案，把状态改成 `Status: resolved`，再把一段上下文指针（要点 + 链接）追加到 `map.md` 的 Decisions-so-far
