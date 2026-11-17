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
const CYAN: &str = "\x1b[36m";
const YELLOW: &str = "\x1b[33m";
const GREEN_BRIGHT: &str = "\x1b[92m";

/// 终端够不够宽、能不能着色，由调用方判断一次，之后固定。
pub struct Renderer {
    color: bool,
    use_bat: bool,
    /// 还没换行的半行。
    pending: String,
    in_code: bool,
    code_lang: String,
    code: String,
    /// 攒着的表格行：整块到齐才能算列宽。
    table: Vec<String>,
    /// 终端宽度，用来判断表格会不会超宽；拿不到就是 80。
    width: usize,
}

impl Renderer {
    pub fn new(color: bool) -> Renderer {
        let mut renderer = Renderer::with_highlighter(color, color && bat_available());
        renderer.width = terminal_width().unwrap_or(80);
        renderer
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
            table: Vec::new(),
            width: 80,
        }
    }

    /// 测试用：固定终端宽度。
    pub fn with_width(mut self, width: usize) -> Renderer {
        self.width = width;
        self
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

    /// 一轮结束时调用：把残留的半行、未闭合的代码块、没等到下一行的表格都放出来。
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
        let table = self.flush_table();
        if !table.is_empty() {
            out.push_str(&table);
            out.push('\n');
        }
        out
    }

    /// 处理一整行（不含换行）。返回 `None` 表示这行被代码块或表格吞了，不该单独输出。
    fn line(&mut self, line: &str) -> Option<String> {
        let trimmed = line.trim_start();
        // 表格要整块到齐才能对齐，先攒着；遇到非表格行再一次性输出。
        if !self.in_code && trimmed.starts_with('|') {
            self.table.push(line.to_string());
            return None;
        }
        let table = self.flush_table();
        let body = self.plain_line(line);
        match (table.is_empty(), body) {
            (true, body) => body,
            (false, None) => Some(table),
            (false, Some(body)) => Some(format!("{table}\n{body}")),
        }
    }

    /// 把攒着的表格画出来；不成表或太宽就原样吐回，只把分隔行调暗。
    fn flush_table(&mut self) -> String {
        if self.table.is_empty() {
            return String::new();
        }
        let rows = std::mem::take(&mut self.table);
        render_table(&rows, self.color, self.width).unwrap_or_else(|| {
            rows.iter()
                .map(|row| {
                    if is_table_rule(row.trim_start()) {
                        format!("{DIM}{row}{RESET}")
                    } else {
                        inline(row, self.color)
                    }
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
    }

    fn plain_line(&mut self, line: &str) -> Option<String> {
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
            let style = if level <= 2 {
                format!("{BOLD}{CYAN}")
            } else {
                BOLD.to_string()
            };
            return Some(format!("{style}{}{RESET}", inline(text, self.color)));
        }
        // 分隔线：整行调暗。
        if is_rule(trimmed) {
            return Some(format!("{DIM}{line}{RESET}"));
        }
        // 单独出现的表格分隔行（没有表头的残片）同样调暗。
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
            return Some(format!(
                "{indent}{CYAN}•{RESET} {}",
                inline(rest, self.color)
            ));
        }
        // 有序列表：序号上色，内容照常。
        if let Some((indent, marker, rest)) = ordered(line) {
            return Some(format!(
                "{indent}{CYAN}{marker}{RESET} {}",
                inline(rest, self.color)
            ));
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
    let rest = line
        .strip_prefix("```")
        .or_else(|| line.strip_prefix("~~~"))?;
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

/// 终端宽度；不是终端或取不到就返回 `None`，由调用方退到 80。
pub fn terminal_width() -> Option<usize> {
    // SAFETY: 只把窗口大小结构体交给内核填写。
    unsafe {
        let mut size: libc::winsize = std::mem::zeroed();
        if libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut size) == 0 && size.ws_col > 0 {
            return Some(size.ws_col as usize);
        }
    }
    None
}

#[derive(Clone, Copy)]
enum Align {
    Left,
    Center,
    Right,
}

/// 把攒好的表格画成对齐的列；不成表或宽过终端就返回 `None`，由调用方原样吐回。
fn render_table(rows: &[String], color: bool, width: usize) -> Option<String> {
    if rows.len() < 2 || !is_table_rule(rows[1].trim_start()) {
        return None;
    }
    let cells: Vec<Vec<String>> = rows.iter().map(|row| split_cells(row)).collect();
    let columns = cells.iter().map(Vec::len).max().unwrap_or(0);
    if columns == 0 {
        return None;
    }
    let aligns: Vec<Align> = (0..columns)
        .map(|index| {
            cells[1]
                .get(index)
                .map(|cell| align_of(cell))
                .unwrap_or(Align::Left)
        })
        .collect();

    // 列宽按显示宽度算：中文占两格，颜色控制符不算。
    let mut widths = vec![0usize; columns];
    for (row_index, row) in cells.iter().enumerate() {
        if row_index == 1 {
            continue;
        }
        for (index, cell) in row.iter().enumerate() {
            let text = inline(cell, color);
            widths[index] = widths[index].max(display_width(&text));
        }
    }
    let total: usize = widths.iter().sum::<usize>() + 2 * columns.saturating_sub(1);
    // 比终端宽就收缩列宽再折行，而不是整表退回原文：表还是表，只是每格可能占几行。
    let widths = if total > width {
        let room = width.saturating_sub(2 * columns.saturating_sub(1));
        if room < columns {
            return None;
        }
        shrink_widths(&widths, room)
    } else {
        widths
    };

    let mut out: Vec<String> = Vec::new();
    for (row_index, row) in cells.iter().enumerate() {
        if row_index == 1 {
            continue;
        }
        let wrapped: Vec<Vec<String>> = (0..columns)
            .map(|index| {
                let raw = row.get(index).map(String::as_str).unwrap_or("");
                wrap_cell(raw, widths[index], color)
            })
            .collect();
        let height = wrapped.iter().map(Vec::len).max().unwrap_or(1);
        for line_index in 0..height {
            let mut line = String::new();
            for (index, width) in widths.iter().enumerate() {
                if index > 0 {
                    line.push_str("  ");
                }
                let raw = wrapped[index]
                    .get(line_index)
                    .map(String::as_str)
                    .unwrap_or("");
                let text = inline(raw, color);
                let used = display_width(&text);
                line.push_str(&pad(&text, used, *width, aligns[index]));
            }
            out.push(line.trim_end().to_string());
        }
        if row_index == 0 {
            let rule = widths
                .iter()
                .map(|width| "\u{2500}".repeat(*width))
                .collect::<Vec<_>>()
                .join("  ");
            out.push(format!("{DIM}{rule}{RESET}"));
        }
    }
    Some(out.join("\n"))
}

/// 把自然列宽压进 `room`：先从最窄的列开始给，保证宽列吃掉剩下的空间。
fn shrink_widths(natural: &[usize], room: usize) -> Vec<usize> {
    let mut order: Vec<usize> = (0..natural.len()).collect();
    order.sort_by_key(|index| natural[*index]);
    let mut widths = vec![0usize; natural.len()];
    let mut left = natural.len();
    let mut remaining = room;
    for index in order {
        let share = remaining / left.max(1);
        let width = natural[index].min(share);
        widths[index] = width;
        remaining -= width;
        left -= 1;
    }
    widths
}

/// 按显示宽度折行，但不在行内标记（反引号、粗体）中间断开。
///
/// 宽度按「去掉标记之后」算：`**粗体**` 在屏幕上只占粗体那两个字的宽度。
fn wrap_cell(raw: &str, width: usize, color: bool) -> Vec<String> {
    if width == 0 {
        return vec![String::new()];
    }
    let mut lines: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut used = 0usize;
    for token in markdown_tokens(raw) {
        let token_width = display_width(&inline(&token, color));
        if used > 0 && used + token_width > width {
            lines.push(std::mem::take(&mut current));
            used = 0;
            if token.trim().is_empty() {
                continue;
            }
        }
        if token_width > width {
            // 单个词比整列还宽：只能硬切，宁可断样式也不让行超宽。
            for ch in token.chars() {
                let ch_width = char_width(ch);
                if used > 0 && used + ch_width > width {
                    lines.push(std::mem::take(&mut current));
                    used = 0;
                }
                current.push(ch);
                used += ch_width;
            }
            continue;
        }
        current.push_str(&token);
        used += token_width;
    }
    if !current.is_empty() || lines.is_empty() {
        lines.push(current);
    }
    lines
        .into_iter()
        .map(|line| line.trim_end().to_string())
        .collect()
}

/// 把一格拆成折行单位：行内标记整段算一个，其余按空白切。
fn markdown_tokens(raw: &str) -> Vec<String> {
    let chars: Vec<char> = raw.chars().collect();
    let mut tokens = Vec::new();
    let mut index = 0;
    while index < chars.len() {
        if chars[index] == '`'
            && let Some(end) = find(&chars, index + 1, &['`'])
        {
            tokens.push(chars[index..=end].iter().collect());
            index = end + 1;
            continue;
        }
        if chars[index] == '*'
            && chars.get(index + 1) == Some(&'*')
            && let Some(end) = find_double_star(&chars, index + 2)
        {
            tokens.push(chars[index..end + 2].iter().collect());
            index = end + 2;
            continue;
        }
        if chars[index].is_whitespace() {
            let start = index;
            while index < chars.len() && chars[index].is_whitespace() {
                index += 1;
            }
            tokens.push(chars[start..index].iter().collect());
            continue;
        }
        let start = index;
        while index < chars.len() && !chars[index].is_whitespace() && chars[index] != '`' {
            index += 1;
        }
        tokens.push(chars[start..index].iter().collect());
    }
    tokens
}

/// 找闭合的 `**`。
fn find_double_star(chars: &[char], from: usize) -> Option<usize> {
    (from..chars.len().saturating_sub(1))
        .find(|&index| chars[index] == '*' && chars[index + 1] == '*')
}

fn split_cells(row: &str) -> Vec<String> {
    let trimmed = row.trim();
    let inner = trimmed.strip_prefix('|').unwrap_or(trimmed);
    let inner = inner.strip_suffix('|').unwrap_or(inner);
    inner
        .split('|')
        .map(|cell| cell.trim().to_string())
        .collect()
}

/// `---:` 右对齐、`:---:` 居中，其余左对齐。
fn align_of(cell: &str) -> Align {
    let cell = cell.trim();
    let left = cell.starts_with(':');
    let right = cell.ends_with(':');
    match (left, right) {
        (true, true) => Align::Center,
        (false, true) => Align::Right,
        _ => Align::Left,
    }
}

fn pad(text: &str, used: usize, width: usize, align: Align) -> String {
    let gap = width.saturating_sub(used);
    match align {
        Align::Left => format!("{text}{}", " ".repeat(gap)),
        Align::Right => format!("{}{text}", " ".repeat(gap)),
        Align::Center => {
            let left = gap / 2;
            format!("{}{text}{}", " ".repeat(left), " ".repeat(gap - left))
        }
    }
}

/// 显示宽度：跳过 ANSI 转义，东亚宽字符按两格算。
pub fn display_width(text: &str) -> usize {
    let mut width = 0;
    let mut chars = text.chars();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' {
            for next in chars.by_ref() {
                if next.is_ascii_alphabetic() {
                    break;
                }
            }
            continue;
        }
        width += char_width(ch);
    }
    width
}

fn char_width(ch: char) -> usize {
    let code = ch as u32;
    let wide = matches!(
        code,
        0x1100..=0x115F
            | 0x2E80..=0x303E
            | 0x3041..=0x33FF
            | 0x3400..=0x4DBF
            | 0x4E00..=0x9FFF
            | 0xA000..=0xA4CF
            | 0xAC00..=0xD7A3
            | 0xF900..=0xFAFF
            | 0xFE30..=0xFE6F
            | 0xFF00..=0xFF60
            | 0xFFE0..=0xFFE6
            | 0x1F300..=0x1F64F
            | 0x20000..=0x3FFFD
    );
    if wide { 2 } else { 1 }
}

fn is_table_rule(line: &str) -> bool {
    line.starts_with('|')
        && line.chars().any(|c| c == '-')
        && line.chars().all(|c| matches!(c, '|' | '-' | ':' | ' '))
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
    let marker_end = if after.starts_with('.') || after.starts_with(')') {
        1
    } else {
        0
    };
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

/// 命令本身的着色：命令词绿色、`-` 开头的参数青色、引号里的内容黄色、
/// 管道和分号调暗。只用于显示，命令怎么执行完全不变。
/// 一行淡淡的附注（例如「图片：/path」）。
pub fn dim_line(text: &str, color: bool) -> String {
    if color {
        format!("{DIM}{text}{RESET}")
    } else {
        text.to_string()
    }
}

pub fn shell_line(command: &str, color: bool) -> String {
    if !color {
        return command.to_string();
    }
    let mut out = String::new();
    // heredoc 正文要攒到结束标记，再整段交给 bat。
    let mut pending: Option<(String, String)> = None;
    let mut body = String::new();

    for segment in command.split_inclusive('\n') {
        let (line, had_newline) = match segment.strip_suffix('\n') {
            Some(text) => (text, true),
            None => (segment, false),
        };

        if let Some((label, lang)) = pending.clone() {
            if line.trim() == label {
                out.push_str(&highlight(&body, &lang).unwrap_or_else(|| body.clone()));
                body.clear();
                out.push_str(line);
                if had_newline {
                    out.push('\n');
                }
                pending = None;
                continue;
            }
            body.push_str(line);
            body.push('\n');
            continue;
        }

        out.push_str(&color_shell_line(line));
        if had_newline {
            out.push('\n');
        }
        if let Some(found) = heredoc(line) {
            pending = Some(found);
        }
    }

    // 没有结束标记：正文照样输出，不能吞掉命令内容。
    if let Some((_, lang)) = pending {
        out.push_str(&highlight(&body, &lang).unwrap_or(body));
    }
    out
}

/// 单行 shell 着色：命令词绿色、`-` 选项青色、引号内容黄色、`;`/`|` 调暗。
fn color_shell_line(command: &str) -> String {
    let chars: Vec<char> = command.chars().collect();
    let mut out = String::new();
    let mut index = 0;
    let mut expect_command = true;
    while index < chars.len() {
        let ch = chars[index];
        if ch.is_whitespace() {
            out.push(ch);
            index += 1;
            continue;
        }
        if ch == '"' || ch == '\'' {
            let quote = ch;
            let mut end = index + 1;
            while end < chars.len() && chars[end] != quote {
                end += 1;
            }
            let text: String = chars[index..(end + 1).min(chars.len())].iter().collect();
            out.push_str(&format!("{YELLOW}{text}{RESET}"));
            index = end + 1;
            expect_command = false;
            continue;
        }
        if matches!(ch, ';' | '|' | '&') {
            out.push_str(&format!("{DIM}{ch}{RESET}"));
            expect_command = true;
            index += 1;
            continue;
        }
        let mut end = index;
        while end < chars.len()
            && !chars[end].is_whitespace()
            && !matches!(chars[end], ';' | '|' | '&')
        {
            end += 1;
        }
        let word: String = chars[index..end].iter().collect();
        if expect_command && !word.contains('=') {
            out.push_str(&format!("{GREEN_BRIGHT}{word}{RESET}"));
            expect_command = false;
        } else if word.starts_with('-') {
            out.push_str(&format!("{CYAN}{word}{RESET}"));
        } else {
            out.push_str(&word);
        }
        index = end;
    }
    out
}

/// 找出 heredoc 的结束标记和正文该用什么语言高亮。
fn heredoc(line: &str) -> Option<(String, String)> {
    let at = line.find("<<")?;
    let mut rest = line[at + 2..].trim_start();
    // `<<-` 只影响缩进，标记解析相同。
    if let Some(stripped) = rest.strip_prefix('-') {
        rest = stripped.trim_start();
    }
    let quote = rest.chars().next().filter(|c| *c == '"' || *c == '\'');
    if quote.is_some() {
        rest = &rest[1..];
    }
    let end = rest
        .find(|c: char| c.is_whitespace() || matches!(c, ';' | '|' | '&' | '>'))
        .unwrap_or(rest.len());
    let label = rest[..end].trim_end_matches(['"', '\'']).to_string();
    if label.is_empty() {
        return None;
    }
    let lang = language_hint(line, &label)?;
    Some((label, lang))
}

/// 语言提示的优先级：解释器 → 目标文件后缀 → 结束标记本身。
fn language_hint(line: &str, label: &str) -> Option<String> {
    let mut words = line.split_whitespace();
    if let Some(first) = words.next() {
        let name = first.rsplit('/').next().unwrap_or(first);
        if let Some(lang) = interpreter(name) {
            return Some(lang.to_string());
        }
    }
    for word in line.split_whitespace() {
        let token = word.trim_matches(|c: char| matches!(c, '>' | '"' | '\'' | ';'));
        if let Some(ext) = token.rsplit('.').next()
            && token.contains('.')
            && let Some(lang) = extension(ext)
        {
            return Some(lang.to_string());
        }
    }
    extension(&label.to_lowercase()).map(str::to_string)
}

fn interpreter(name: &str) -> Option<&'static str> {
    let name = name.trim_end_matches(|c: char| c.is_ascii_digit());
    match name {
        "python" | "python3" | "py" => Some("python"),
        "node" | "nodejs" => Some("javascript"),
        "bash" | "sh" | "zsh" | "fish" => Some("bash"),
        "ruby" | "rb" => Some("ruby"),
        "php" => Some("php"),
        "go" => Some("go"),
        _ => None,
    }
}

fn extension(ext: &str) -> Option<&'static str> {
    match ext {
        "py" => Some("python"),
        "js" | "mjs" | "cjs" | "jsx" => Some("javascript"),
        "ts" | "tsx" => Some("typescript"),
        "go" => Some("go"),
        "sh" | "bash" => Some("bash"),
        "rb" => Some("ruby"),
        "php" => Some("php"),
        "sql" => Some("sql"),
        "json" => Some("json"),
        "yaml" | "yml" => Some("yaml"),
        "rs" => Some("rust"),
        _ => None,
    }
}

/// 工具失败的一行提示：红色 `✗`、工具名、原因。成功不打印这一行。
pub fn tool_line(name: &str, note: &str, color: bool) -> String {
    if !color {
        return format!("✗ {name} {note}");
    }
    let note = note.strip_prefix("ERROR: ").unwrap_or(note);
    format!("{RED}✗ {name}{RESET} {note}")
}

/// 不算失败的一行提示：暗色工具名加说明，例如「bash 没有匹配（退出码 1）」。
pub fn tool_note(name: &str, note: &str, color: bool) -> String {
    if !color {
        return format!("{name} {note}");
    }
    format!("{DIM}{name}{RESET} {note}")
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
    fn a_table_becomes_aligned_columns_without_pipes() {
        let mut renderer = plain().with_width(80);
        let mut out = renderer.push("| 名称 | 数量 |\n|:---|---:|\n| 苹果 | 12 |\n| pear | 3 |\n");
        out.push_str(&renderer.finish());
        let text = strip_ansi(&out);
        assert!(!text.contains('|'), "渲染后不该再有竖线：{text}");
        let lines: Vec<&str> = text
            .lines()
            .filter(|line| !line.trim().is_empty())
            .collect();
        assert_eq!(lines.len(), 4, "{text}");
        assert!(
            lines[1].contains('\u{2500}'),
            "表头下面要有一条分隔线：{text}"
        );
        assert!(lines[0].starts_with("名称"), "{text}");
        // 数字右对齐：两行的显示宽度相同（中文占两格，不能用字符数比）。
        assert_eq!(
            display_width(lines[2]),
            display_width(lines[3]),
            "右对齐后两行等宽：{text}"
        );
    }

    #[test]
    fn a_table_wider_than_the_terminal_wraps_instead_of_leaking_pipes() {
        let mut renderer = plain().with_width(20);
        let mut out = renderer.push("| 名称 | 数量 |\n|---|---|\n| 苹果很甜很好吃 | 12345 |\n");
        out.push_str(&renderer.finish());
        let text = strip_ansi(&out);
        assert!(!text.contains('|'), "折行后不该再有竖线：{text}");
        // 折行会把「吃」挤到下一行，但一个字都不能丢。
        assert!(text.contains("苹果很甜很好"), "一个字都不能丢：{text}");
        assert!(text.lines().any(|line| line.trim() == "吃"), "{text}");
        for line in text.lines() {
            assert!(display_width(line) <= 20, "每行都不超过终端宽度：{line:?}");
        }
    }

    #[test]
    fn wrapping_never_splits_an_inline_span() {
        let mut renderer = Renderer::with_highlighter(true, false).with_width(20);
        let mut out =
            renderer.push("| a | b |\n|---|---|\n| 甲 **丙丁戊己** | 乙乙乙乙乙乙乙乙 |\n");
        out.push_str(&renderer.finish());
        // 粗体整段算一个折行单位：真被切开就会剩下没配对的 `**`。
        assert!(!out.contains("**"), "行内标记必须成对：{out:?}");
        let text = strip_ansi(&out);
        assert!(text.contains("丙丁戊己"), "{text}");
        for line in text.lines() {
            assert!(display_width(line) <= 20, "每行都不超过终端宽度：{line:?}");
        }
    }

    #[test]
    fn a_table_too_narrow_for_any_column_still_falls_back() {
        let mut renderer = plain().with_width(1);
        let mut out = renderer.push("| 名称 | 数量 |\n|---|---|\n| 苹果 | 1 |\n");
        out.push_str(&renderer.finish());
        let text = strip_ansi(&out);
        assert!(text.contains('|'), "连一列都放不下时原样输出：{text}");
    }

    #[test]
    fn alignment_markers_move_the_text_inside_the_column() {
        let mut centre = plain().with_width(80);
        let mut out = centre.push("| abcd |\n|:---:|\n| x |\n");
        out.push_str(&centre.finish());
        let text = strip_ansi(&out);
        let lines: Vec<&str> = text
            .lines()
            .filter(|line| !line.trim().is_empty())
            .collect();
        assert_eq!(lines[2], " x", "居中：{text}");

        let mut right = plain().with_width(80);
        let mut out = right.push("| abcd |\n|---:|\n| x |\n");
        out.push_str(&right.finish());
        let text = strip_ansi(&out);
        let lines: Vec<&str> = text
            .lines()
            .filter(|line| !line.trim().is_empty())
            .collect();
        assert_eq!(lines[2], "   x", "右对齐：{text}");
    }

    #[test]
    fn a_table_without_a_separator_row_is_left_alone() {
        let mut renderer = plain().with_width(80);
        let mut out = renderer.push("| 随便 |\n| 两行 |\n");
        out.push_str(&renderer.finish());
        let text = strip_ansi(&out);
        assert!(text.contains('|'), "不成表就原样输出：{text}");
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

    fn strip_ansi(text: &str) -> String {
        let mut out = String::new();
        let mut in_escape = false;
        for ch in text.chars() {
            if in_escape {
                if ch.is_ascii_alphabetic() {
                    in_escape = false;
                }
            } else if ch == '\x1b' {
                in_escape = true;
            } else {
                out.push(ch);
            }
        }
        out
    }

    #[test]
    fn heredoc_language_comes_from_the_interpreter_file_or_label() {
        assert_eq!(heredoc("python3 - <<'PY'").unwrap().1, "python");
        assert_eq!(heredoc("cat /tmp/srv.py <<'EOF'").unwrap().1, "python");
        assert_eq!(heredoc("cat > run.go <<-EOF").unwrap().1, "go");
        assert_eq!(heredoc("node <<'JS'").unwrap().1, "javascript");
        assert!(heredoc("echo hi").is_none());
    }

    #[test]
    fn a_heredoc_body_is_kept_verbatim() {
        let command = "python3 - <<'PY'\nimport os\nprint('x')\nPY";
        let out = shell_line(command, true);
        assert_eq!(strip_ansi(&out), command, "着色不能改动原文");
    }

    #[test]
    fn a_python_heredoc_body_gets_colored() {
        if !bat_available() {
            return;
        }
        let out = shell_line("python3 - <<'PY'\nimport os\nPY", true);
        let body = out.split('\n').nth(1).unwrap_or("");
        assert!(body.contains('\x1b'), "正文应当有语法颜色：{body:?}");
    }

    #[test]
    fn an_unterminated_heredoc_still_prints_its_body() {
        let out = shell_line("cat <<'EOF'\nhello", true);
        assert!(strip_ansi(&out).contains("hello"), "{out}");
    }

    #[test]
    fn shell_line_separates_command_flags_and_strings() {
        let out = shell_line("git status --short; echo \"done\" | sed -n '1p'", true);
        assert!(out.contains(&format!("{GREEN_BRIGHT}git{RESET}")), "{out}");
        assert!(out.contains(&format!("{CYAN}--short{RESET}")), "{out}");
        assert!(out.contains(&format!("{YELLOW}\"done\"{RESET}")), "{out}");
        assert!(
            out.contains(&format!("{GREEN_BRIGHT}echo{RESET}")),
            "分号后是新命令：{out}"
        );
        assert!(out.contains(&format!("{DIM};{RESET}")), "{out}");
        // 关闭颜色时原样返回。
        assert_eq!(shell_line("git status", false), "git status");
    }

    #[test]
    fn a_tool_note_is_dim_and_carries_no_failure_mark() {
        let line = tool_note("bash", "没有匹配（退出码 1）", true);
        assert_eq!(line, format!("{DIM}bash{RESET} 没有匹配（退出码 1）"));
        assert!(!line.contains('✗'), "{line}");
        assert_eq!(
            tool_note("bash", "没有匹配（退出码 1）", false),
            "bash 没有匹配（退出码 1）"
        );
    }

    #[test]
    fn a_tool_failure_gets_a_red_line() {
        let line = tool_line("bash", "ERROR: 退出码 1", true);
        assert!(line.contains(RED), "{line}");
        assert!(!line.contains("ERROR: "), "去掉前缀只留原因：{line}");
    }
}
