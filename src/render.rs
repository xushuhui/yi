//! 行式 markdown 渲染。
//!
//! 输出直接进终端滚动区，不能重绘（见 ADR-0002），所以只能「收齐一行再渲染」：
//! 普通行随时打印；代码块先攒着，遇到结束围栏再整块交给 `bat` 着色（见 ADR-0004）。
//! 颜色关闭时（管道输出、`NO_COLOR`）完全原样透传，脚本行为不变。

use std::io::Write;
use std::process::{Command, Stdio};

const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const ITALIC: &str = "\x1b[3m";
const RED: &str = "\x1b[31m";
const GREEN: &str = "\x1b[32m";
const CYAN: &str = "\x1b[36m";
const YELLOW: &str = "\x1b[33m";

/// 终端够不够宽、能不能着色，由调用方判断一次，之后固定。
pub struct Renderer {
    color: bool,
    use_bat: bool,
    /// 还没换行的半行。
    pending: String,
    in_code: bool,
    code_lang: String,
    code: String,
}

impl Renderer {
    pub fn new(color: bool) -> Renderer {
        Renderer::with_highlighter(color, color && bat_available())
    }

    /// 测试用：显式决定要不要调用 bat。
    pub fn with_highlighter(color: bool, use_bat: bool) -> Renderer {
        Renderer {
            color,
            use_bat,
            pending: String::new(),
            in_code: false,
            code_lang: String::new(),
            code: String::new(),
        }
    }

    /// 喂一段流式文本，返回现在就该打印的内容。
    pub fn push(&mut self, chunk: &str) -> String {
        if !self.color {
            return chunk.to_string();
        }
        self.pending.push_str(chunk);
        let mut out = String::new();
        while let Some(end) = self.pending.find('\n') {
            let line: String = self.pending.drain(..=end).collect();
            let line = line.trim_end_matches('\n');
            if let Some(rendered) = self.line(line) {
                out.push_str(&rendered);
                out.push('\n');
            }
        }
        out
    }

    /// 只把半行放出来；工具提示插进来之前用它避免文字被截在一半。
    pub fn flush_line(&mut self) -> String {
        if !self.color || self.pending.is_empty() {
            return String::new();
        }
        let line = std::mem::take(&mut self.pending);
        let rendered = self.line(&line).unwrap_or_default();
        format!("{rendered}\n")
    }

    /// 一轮结束时调用：把残留的半行、未闭合的代码块都放出来。
    pub fn finish(&mut self) -> String {
        if !self.color {
            return String::new();
        }
        let mut out = String::new();
        if !self.pending.is_empty() {
            let line = std::mem::take(&mut self.pending);
            if let Some(rendered) = self.line(&line) {
                out.push_str(&rendered);
                out.push('\n');
            }
        }
        if self.in_code {
            let lang = std::mem::take(&mut self.code_lang);
            let code = std::mem::take(&mut self.code);
            self.in_code = false;
            out.push_str(&self.code_block(&lang, &code));
        }
        out
    }

    /// 处理一整行（不含换行）。返回 `None` 表示这行被代码块吞了，不该单独输出。
    fn line(&mut self, line: &str) -> Option<String> {
        let trimmed = line.trim_start();
        if self.in_code {
            if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
                let lang = std::mem::take(&mut self.code_lang);
                let code = std::mem::take(&mut self.code);
                self.in_code = false;
                return Some(self.code_block(&lang, &code));
            }
            self.code.push_str(line);
            self.code.push('\n');
            return None;
        }
        if let Some(rest) = fence(trimmed) {
            self.in_code = true;
            self.code_lang = rest.to_string();
            self.code.clear();
            return Some(self.code_header(rest));
        }

        // 标题：去掉井号，前两级上色。
        if let Some((level, text)) = heading(trimmed) {
            let style = if level <= 2 { format!("{BOLD}{CYAN}") } else { BOLD.to_string() };
            return Some(format!("{style}{}{RESET}", inline(text, self.color)));
        }
        // 分隔线：整行调暗。
        if is_rule(trimmed) {
            return Some(format!("{DIM}{line}{RESET}"));
        }
        // 表格的分隔行（|---|:--:|）调暗，内容行不动，避免中英文混排算错宽度。
        if is_table_rule(trimmed) {
            return Some(format!("{DIM}{line}{RESET}"));
        }
        // 引用：把 `>` 换成竖线，只把竖线调暗。
        if let Some(rest) = trimmed.strip_prefix('>') {
            let indent = &line[..line.len() - trimmed.len()];
            return Some(format!("{indent}{DIM}│{RESET}{}", inline(rest, self.color)));
        }
        // 列表：项目符号换成圆点。
        if let Some((indent, rest)) = bullet(line) {
            return Some(format!("{indent}{CYAN}•{RESET} {}", inline(rest, self.color)));
        }
        // 有序列表：序号上色，内容照常。
        if let Some((indent, marker, rest)) = ordered(line) {
            return Some(format!("{indent}{CYAN}{marker}{RESET} {}", inline(rest, self.color)));
        }
        Some(inline(line, self.color))
    }

    fn code_header(&self, lang: &str) -> String {
        let label = if lang.is_empty() { "代码" } else { lang };
        format!("{DIM}── {label} \u{2500}\u{2500}{RESET}")
    }

    fn code_block(&self, lang: &str, code: &str) -> String {
        let text = if self.use_bat {
            // 没有 bat、语言不认识、bat 报错：退回原文，不能因为着色丢代码。
            highlight(code, lang).unwrap_or_else(|| code.to_string())
        } else {
            code.to_string()
        };
        // 行尾统一由 push 补一个换行，这里去掉，避免代码块下方多出空行。
        text.trim_end_matches('\n').to_string()
    }
}

/// 三个反引号或三个波浪线开头，返回围栏后面的语言名。
fn fence(line: &str) -> Option<&str> {
    let rest = line.strip_prefix("```").or_else(|| line.strip_prefix("~~~"))?;
    Some(rest.trim())
}

fn heading(line: &str) -> Option<(usize, &str)> {
    let level = line.chars().take_while(|c| *c == '#').count();
    if level == 0 || level > 6 {
        return None;
    }
    let rest = &line[level..];
    if !rest.starts_with(' ') {
        return None;
    }
    Some((level, rest.trim_start()))
}

fn is_rule(line: &str) -> bool {
    let stripped: String = line.chars().filter(|c| !c.is_whitespace()).collect();
    stripped.len() >= 3
        && (stripped.chars().all(|c| c == '-')
            || stripped.chars().all(|c| c == '*')
            || stripped.chars().all(|c| c == '_'))
}

fn is_table_rule(line: &str) -> bool {
    line.starts_with('|')
        && line.chars().any(|c| c == '-')
        && line
            .chars()
            .all(|c| matches!(c, '|' | '-' | ':' | ' '))
}

/// `- 内容` / `* 内容` / `+ 内容`，返回缩进和内容。
fn bullet(line: &str) -> Option<(&str, &str)> {
    let indent_len = line.len() - line.trim_start().len();
    let (indent, rest) = line.split_at(indent_len);
    let mut chars = rest.chars();
    let marker = chars.next()?;
    if !matches!(marker, '-' | '*' | '+') {
        return None;
    }
    let rest = chars.as_str();
    let body = rest.strip_prefix(' ')?;
    Some((indent, body))
}

/// `1. 内容` / `12) 内容`，返回缩进、序号和内容。
fn ordered(line: &str) -> Option<(&str, &str, &str)> {
    let indent_len = line.len() - line.trim_start().len();
    let (indent, rest) = line.split_at(indent_len);
    let digits = rest.chars().take_while(|c| c.is_ascii_digit()).count();
    if digits == 0 {
        return None;
    }
    let after = &rest[digits..];
    let marker_end = if after.starts_with('.') || after.starts_with(')') { 1 } else { 0 };
    if marker_end == 0 {
        return None;
    }
    let body = after[marker_end..].strip_prefix(' ')?;
    Some((indent, &rest[..digits + marker_end], body))
}

/// 行内样式：`代码`、**粗体**、*斜体*、[文字](链接)。
fn inline(text: &str, color: bool) -> String {
    if !color {
        return text.to_string();
    }
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::new();
    let mut index = 0;
    while index < chars.len() {
        let rest = &chars[index..];
        if rest[0] == '`'
            && let Some(end) = find(&chars, index + 1, &['`'])
        {
            let body: String = chars[index + 1..end].iter().collect();
            out.push_str(&format!("{YELLOW}{body}{RESET}"));
            index = end + 1;
            continue;
        }
        if rest.starts_with(&['*', '*'])
            && let Some(end) = find(&chars, index + 2, &['*', '*'])
        {
            let body: String = chars[index + 2..end].iter().collect();
            out.push_str(&format!("{BOLD}{body}{RESET}"));
            index = end + 2;
            continue;
        }
        if rest[0] == '['
            && let Some((body, target, next)) = link(&chars, index)
        {
            out.push_str(&body);
            if bold_target(&body) != target {
                out.push_str(&format!("{DIM}（{target}）{RESET}"));
            }
            index = next;
            continue;
        }
        if (rest[0] == '*' || rest[0] == '_')
            && rest.len() > 1
            && rest[1] != ' '
            && let Some(end) = find(&chars, index + 1, &[chars[index]])
        {
            let body: String = chars[index + 1..end].iter().collect();
            out.push_str(&format!("{ITALIC}{body}{RESET}"));
            index = end + 1;
            continue;
        }
        out.push(rest[0]);
        index += 1;
    }
    out
}

/// 链接文字里可能又有行内样式，比较时用去掉反引号/星号的原文。
fn bold_target(body: &str) -> String {
    body.replace(['`', '*', '_'], "")
}

fn link(chars: &[char], start: usize) -> Option<(String, String, usize)> {
    let close = find(chars, start + 1, &[']'])?;
    if chars.get(close + 1) != Some(&'(') {
        return None;
    }
    let end = find(chars, close + 2, &[')'])?;
    let body: String = chars[start + 1..close].iter().collect();
    let target: String = chars[close + 2..end].iter().collect();
    Some((body, target, end + 1))
}

fn find(chars: &[char], from: usize, needle: &[char]) -> Option<usize> {
    (from..chars.len()).find(|index| chars[*index..].starts_with(needle))
}

fn bat_available() -> bool {
    Command::new("bat")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// 整块交给 bat 着色；任何失败都返回 None，由调用方退回原文。
fn highlight(code: &str, lang: &str) -> Option<String> {
    let mut command = Command::new("bat");
    command
        .args([
            "--color=always",
            "--style=plain",
            "--paging=never",
            "--theme=ansi",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    // 语言名只允许字母数字和几个符号，避免拼出奇怪的参数。
    if !lang.is_empty()
        && lang
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '#' | '-' | '_' | '.'))
    {
        command.arg(format!("--language={lang}"));
    }
    let mut child = command.spawn().ok()?;
    child.stdin.take()?.write_all(code.as_bytes()).ok()?;
    let output = child.wait_with_output().ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).to_string())
}

/// 工具结果的一行提示：失败给红色，`edit` 成功给绿色摘要。
pub fn tool_line(name: &str, failed: bool, note: &str, color: bool) -> String {
    if !color {
        let mark = if failed { "✗" } else { "✓" };
        return format!("{mark} {name} {note}");
    }
    if failed {
        let note = note.strip_prefix("ERROR: ").unwrap_or(note);
        format!("{RED}✗ {name}{RESET} {note}")
    } else {
        format!("{GREEN}✓{RESET} {DIM}{name}{RESET} {note}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain() -> Renderer {
        Renderer::with_highlighter(true, false)
    }

    #[test]
    fn without_color_everything_passes_through() {
        let mut renderer = Renderer::with_highlighter(false, false);
        let text = "## 标题\n```go\nfunc main() {}\n```\n";
        assert_eq!(renderer.push(text), text);
        assert_eq!(renderer.finish(), "");
    }

    #[test]
    fn headings_and_inline_styles_are_rendered() {
        let mut renderer = plain();
        let out = renderer.push("## 小结 和 `code` 与 **重点**\n");
        assert!(out.contains(BOLD) && out.contains(CYAN), "{out}");
        assert!(!out.contains("##"), "井号要去掉：{out}");
        assert!(out.contains(&format!("{YELLOW}code{RESET}")), "{out}");
        assert!(out.contains(&format!("{BOLD}重点{RESET}")), "{out}");
    }

    #[test]
    fn a_half_line_waits_for_its_newline() {
        let mut renderer = plain();
        assert_eq!(renderer.push("半个"), "");
        assert_eq!(renderer.push("句子\n"), "半个句子\n");
        assert_eq!(renderer.finish(), "");
    }

    #[test]
    fn code_is_buffered_until_the_fence_closes() {
        let mut renderer = plain();
        let first = renderer.push("```go\nfunc main() {\n");
        assert!(first.contains("go"), "围栏处给出语言标签：{first}");
        assert!(!first.contains("func main"), "代码要等块结束：{first}");
        let second = renderer.push("}\n```\n");
        assert!(second.contains("func main() {"), "{second}");
        assert!(!second.contains("```"), "围栏标记不外露：{second}");
    }

    #[test]
    fn an_unclosed_block_is_flushed_at_the_end() {
        let mut renderer = plain();
        renderer.push("```sh\nls\n");
        let out = renderer.finish();
        assert!(out.contains("ls"), "{out}");
    }

    #[test]
    fn lists_quotes_and_tables_get_styles() {
        let mut renderer = plain();
        let out = renderer.push("- 第一条\n> 引用\n|---|---|\n");
        assert!(out.contains('•'), "{out}");
        assert!(out.contains('│'), "{out}");
        assert!(out.contains(DIM), "表格分隔线要调暗：{out}");
    }

    #[test]
    fn a_code_block_does_not_leave_extra_blank_lines() {
        let mut renderer = plain();
        let out = renderer.push("```go\nfmt.Println(1)\n```\n");
        let lines: Vec<&str> = out.split('\n').collect();
        assert!(lines[0].contains("go"), "{out:?}");
        assert_eq!(lines[1], "fmt.Println(1)", "{out:?}");
        assert_eq!(lines[2], "", "代码后只留一个换行：{out:?}");
    }

    #[test]
    fn a_tool_failure_gets_a_red_line() {
        let line = tool_line("bash", true, "ERROR: 退出码 1", true);
        assert!(line.contains(RED), "{line}");
        assert!(!line.contains("ERROR: "), "去掉前缀只留原因：{line}");
    }
}
