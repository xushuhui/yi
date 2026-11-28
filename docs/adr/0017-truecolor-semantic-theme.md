# 界面配色改用真彩色语义主题

## Status

accepted

## 背景

`render.rs` 原来直接写死 16 色 ANSI（`31` 红、`36` 青、`33` 黄、`92` 亮绿、`2` 暗），落到用户终端的调色板上：不同终端差别很大，青+黄+亮绿凑出来的观感是「终端默认色」，而且 `DIM` 在部分终端里要么不生效、要么整体变灰。用户要求参考 omp 把配色做好看。

omp 的做法是主题文件：`vars` 基础色板 + `colors` 语义映射（accent / border / success / error / warning / muted，以及 statusLine 的细分色），内置 dark-dracula、dark-catppuccin、light-catppuccin、dark-aurora 等，`config set theme` 切换。

## 决定

- 颜色集中到一个 `Theme` 结构体（`render::Theme`），字段是语义名：`heading`、`marker`、`link`、`inline_code`、`muted`、`rule`、`success`、`error`、`warning`；值是真彩色转义序列，取 Dracula 原始 RGB（与 omp 默认主题一致）。
- 只内置一套 `DRACULA`，不做主题切换与配置文件；`Renderer` 持有主题副本，自由函数显式接收 `&Theme`。
- `DIM`（SGR 2）换成 `muted` 的具体颜色：不依赖终端对「压暗」的实现，浓淡一致。
- 覆盖面：markdown 标题/列表符号/引用条/表格线/行内代码/链接、shell 高亮、工具行、状态行；页脚保持无色，只有计划模式的 `plan` 标记用 `warning` 橙色。
- 行内代码取 omp 的 `mdCode` 粉色（`#FF79C6`），不取黄色：与 omp 并排时同一段文字的颜色顺序（标题紫、圆点粉、行内代码粉、链接青）完全一致。
- 链接照 omp 的 `mdLink` / `mdLinkUrl` 分工：文字青色，地址收进灰蓝括号；不给地址用青色，否则一长串 URL 抢走正文的注意力。
- 管道、`NO_COLOR` 仍是一个颜色字节都不写。

## 考虑过的选项

- **继续用 16 色，只重新分配语义**：零成本，但观感仍由终端调色板决定，达不到「统一色感」的目标。否决。
- **256 色近似**：兼容性比真彩好，但需要一张 RGB→256 的映射表，颜色也不准；现代终端都支持真彩。否决。
- **照 omp 做主题文件（多套 + 自定义 + `config set theme`）**：能力强，但 yi 只有一个使用场景（深色终端）；多一套加载、校验、回退路径。否决，`Theme` 结构留作以后的缝。
- **代码块也改用 `bat --theme=Dracula`**：本机 bat 有该主题，会比现在的 `ansi` 更接近 omp，但 bat 的 token 分配和 omp 自己的映射（关键字粉、函数绿、字符串黄、数字紫）不等价，只能「接近」。暂缓：先让正文与 omp 一致，代码块留在 `ansi`。
- **自研语法高亮逐 token 复刻 omp**：能完全一致，但要维护一套语言高亮，违背轻量原则。否决。
- **页脚也上色**：每轮都出现的信息行，上色会抢注意力。否决。
- **深色/浅色自动适配**：没有真实使用场景，判断逻辑反而多一处不确定。否决。

## 影响

- 观感由 yi 决定，不再受终端 16 色调色板影响；换主题只需改 `DRACULA` 或新增一个 `const Theme`。
- 真彩色要求终端支持 24-bit（iTerm2、Ghostty、Rex、macOS Terminal 都支持）；不支持时会退化成终端对未知 SGR 的忽略，最坏情况是颜色丢失，正文不受影响。
- 测试从断言 `CYAN`/`YELLOW` 常量改为断言 `DRACULA` 的字段，颜色改动不再牵动断言里的魔数。
