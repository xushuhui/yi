# 读 omp 的内部实现

对齐 omp 的观感或接口时（主题色、额度、状态上报），先从这里查，不要从零猜位置。

## omp 是什么形态

- 安装在 `/opt/homebrew/Cellar/omp/<版本>/bin/omp`，是一个 **Bun 单文件可执行包**：TypeScript 源码和依赖都打进同一个二进制（本机 18.3.0 约 200 MB）。
- 没有可读的 `node_modules`；源码以文本形式嵌在二进制里，`strings -a` 能按行号取出来，片段里保留 `// packages/...` 这样的源文件注释。

## 取一段源码

```sh
bin=/opt/homebrew/Cellar/omp/18.3.0/bin/omp
strings -a "$bin" | grep -n "packages/ai/src/usage/opencode-go.ts"   # 先找文件头行号
strings -a "$bin" | sed -n '480224,480400p'                         # 再按窗口读
```

- 只读需要的窗口，不要整份导出（大几十 MB，而且会被截断）。
- 行号随版本变化；文档里记的是路径与字段，不是行号。
- 找不到行为时，先按界面文案反查：`strings -a "$bin" | grep -n "<屏幕上看到的字>"`，命中后往上下各读一两百行。

## 已知位置

| 想看什么 | 在包里的位置 |
| --- | --- |
| 内置主题、主题字段映射 | `packages/tui/src/theme/defaults/*.json`（`dark-dracula.json` 等）；`colors` 里是语义角色，`vars` 里是原始 RGB |
| 状态栏组件 | `packages/tui/src/components/StatusBar.tsx` |
| 各供应商额度接口 | `packages/ai/src/usage/*.ts`；opencode-go 在同名文件里 |
| 会话/压缩/分支摘要 | 搜 `compaction`、`branch summary` 命中的 `packages/coding-agent/...` |

## opencode-go 额度（yi 的 `src/quota.rs` 按此实现）

- `GET {base}/v1/usage`，去掉 base 末尾的 `/v1` 再拼，等价于 `https://opencode.ai/zen/go/v1/usage`。
- 请求头：`accept: application/json`、`authorization: Bearer <OPENCODE_API_KEY>`、`x-opencode-session: <会话标识>`。
- 响应：

```json
{"usage": {
  "rolling": {"status": "ok", "percent": 5, "resetsAt": "2026-11-27T18:31:04.000Z"},
  "weekly":  {"status": "ok", "percent": 16, "resetsAt": "2026-11-30T00:00:00.000Z"},
  "monthly": {"status": "ok", "percent": 44, "resetsAt": "2026-12-12T10:07:59.000Z"}
}}
```

- `percent` 是**已用**百分比（0–100）；`status` 只认 `ok` / `rate-limited`；三个窗口缺一个就整段不显示（omp 的做法）。
- 本地验证一次即可：`curl -s -H "authorization: Bearer $OPENCODE_API_KEY" -H "x-opencode-session: probe" https://opencode.ai/zen/go/v1/usage`。

## 已知坑

- 二进制里也有大量运行时字符串（V8 错误文案、CSS 关键字），`grep` 命中后先看有没有 `// packages/...` 文件头，再判断是不是目标代码。
- 压缩过的 JS 变量名（`r0i`、`YAo`）不要抄进 yi；只抄行为、字段和常量。
- omp 的主题 JSON 与 yi 的 `render::Theme` 不是一一对应：以实际渲染元素为准（见 ADR-0017、ADR-0018）。
