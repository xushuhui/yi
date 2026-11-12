use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, UNIX_EPOCH};

use regex::Regex;
use serde_json::{Value, json};
use std::os::unix::process::CommandExt;

use crate::session::ToolCall;

const DEFAULT_READ_LINES: usize = 300;
const MAX_READ_LINES: usize = 3000;
const MAX_READ_BYTES: usize = 50 * 1024;
const GREP_PER_FILE: usize = 20;
const GREP_TOTAL: usize = 200;
const GREP_LINE_CHARS: usize = 512;
const GLOB_LIMIT: usize = 200;
const SKIP_DIRS: [&str; 4] = [".git", "node_modules", "target", "vendor"];

/// 工具执行的结果；图片要作为消息附带，不能塞进文本结果里。
pub enum Outcome {
    Text(String),
    Image { path: PathBuf, note: String },
}

/// 一次 `read` 或 `grep` 之后记下的东西：文件当时的内容标识 + 实际显示过的行。
pub struct Snapshot {
    tag: String,
    seen: BTreeSet<usize>,
}

pub struct Tools {
    root: PathBuf,
    log_dir: PathBuf,
    snapshots: HashMap<PathBuf, Snapshot>,
    log_seq: u64,
}

impl Tools {
    pub fn new(root: PathBuf, log_dir: PathBuf) -> Tools {
        Tools {
            root,
            log_dir,
            snapshots: HashMap::new(),
            log_seq: 0,
        }
    }

    /// 六个工具的 JSON Schema。顺序固定，保证请求前缀稳定。
    pub fn definitions() -> Value {
        json!([
            {
                "type": "function",
                "function": {
                    "name": "read",
                    "description": "读取文本文件（带行号和内容校验锚点），或列出目录的一层内容。",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "path": { "type": "string", "description": "相对项目根的路径" },
                            "offset": { "type": "integer", "description": "从第几行开始，1 起算" },
                            "limit": { "type": "integer", "description": "最多读多少行，默认 300" }
                        },
                        "required": ["path"]
                    }
                }
            },
            {
                "type": "function",
                "function": {
                    "name": "write",
                    "description": "新建或覆盖一个文件，自动创建父目录。",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "path": { "type": "string" },
                            "content": { "type": "string" }
                        },
                        "required": ["path", "content"]
                    }
                }
            },
            {
                "type": "function",
                "function": {
                    "name": "edit",
                    "description": "对读过的文件提交补丁。第一行写 [路径#TAG]（TAG 取最近一次 read/grep/edit 返回头里的 4 位），下面写操作：PUT N.=M: 替换第 N~M 行，PUT <N: 在第 N 行前插入，PUT >N: 在第 N 行后插入，CUT N.=M 删除。替换和插入的正文每行以 + 开头，+ 后面是这一行的最终内容。例：\n[src/a.go#1A2B]\nPUT 12.=13:\n+func main() {}\n+\n只能改读过的行；文件被外部改过会拒绝，需要重新 read。",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "input": { "type": "string", "description": "补丁文本" }
                        },
                        "required": ["input"]
                    }
                }
            },
            {
                "type": "function",
                "function": {
                    "name": "bash",
                    "description": "在项目根执行 shell 命令，返回合并后的输出尾部。",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "command": { "type": "string" },
                            "timeout": { "type": "integer", "description": "秒，默认 300" }
                        },
                        "required": ["command"]
                    }
                }
            },
            {
                "type": "function",
                "function": {
                    "name": "grep",
                    "description": "按正则搜索文件内容，返回 文件:行号:内容。",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "pattern": { "type": "string" },
                            "path": { "type": "string", "description": "目录或文件，默认项目根" },
                            "glob": { "type": "string", "description": "仅匹配这些文件，如 *.go" },
                            "case": { "type": "boolean", "description": "默认 true（区分大小写）" }
                        },
                        "required": ["pattern"]
                    }
                }
            },
            {
                "type": "function",
                "function": {
                    "name": "glob",
                    "description": "按通配符找文件，按修改时间倒序，最多 200 条。",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "pattern": { "type": "string", "description": "如 **/*.go 或 src/*.rs" },
                            "path": { "type": "string", "description": "搜索根，默认项目根" }
                        },
                        "required": ["pattern"]
                    }
                }
            }
        ])
    }

    /// 一行人类可读的调用摘要，用于终端回显。
    pub fn describe(call: &ToolCall) -> String {
        let args: Value = serde_json::from_str(&call.arguments).unwrap_or(Value::Null);
        let pick = |key: &str| {
            args.get(key)
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
        };
        match call.name.as_str() {
            "read" | "write" => format!("{} {}", call.name, pick("path")),
            "edit" => {
                let first = pick("input");
                let head = first.lines().next().unwrap_or("").to_string();
                format!("edit {head}")
            }
            "bash" => {
                let mut command = pick("command");
                command.truncate(80);
                format!("bash {command}")
            }
            "grep" => format!("grep {}", pick("pattern")),
            "glob" => format!("glob {}", pick("pattern")),
            other => other.to_string(),
        }
    }

    /// 执行工具。失败一律以 `ERROR: ...` 文本返回给模型，绝不中断循环。
    pub fn execute(&mut self, call: &ToolCall) -> Outcome {
        let args: Value = match serde_json::from_str(&call.arguments) {
            Ok(args) => args,
            Err(_) => return Outcome::Text("ERROR: 参数不是合法 JSON".to_string()),
        };
        let text = match call.name.as_str() {
            "read" => return self.read(&args),
            "write" => self.write(&args),
            "edit" => self.edit(&args),
            "bash" => self.bash(&args),
            "grep" => self.grep(&args),
            "glob" => self.glob(&args),
            other => format!("ERROR: 未知工具 {other}"),
        };
        Outcome::Text(text)
    }

    fn read(&mut self, args: &Value) -> Outcome {
        let Some(path) = args.get("path").and_then(Value::as_str) else {
            return Outcome::Text("ERROR: 缺少 path".to_string());
        };
        let full = self.resolve(path);
        let meta = match fs::metadata(&full) {
            Ok(meta) => meta,
            Err(err) => return Outcome::Text(format!("ERROR: 打不开 {path}：{err}")),
        };

        if meta.is_dir() {
            return Outcome::Text(self.list_dir(&full, path));
        }

        if let Some(mime) = image_mime(&full) {
            if meta.len() > 20 * 1024 * 1024 {
                return Outcome::Text(format!("ERROR: {path} 超过 20 MB"));
            }
            return Outcome::Image {
                path: full,
                note: format!("[图片 {path}（{mime}）已附在本条工具结果之后]"),
            };
        }

        let bytes = match fs::read(&full) {
            Ok(bytes) => bytes,
            Err(err) => return Outcome::Text(format!("ERROR: 读不了 {path}：{err}")),
        };
        let text = match String::from_utf8(bytes) {
            Ok(text) => text,
            Err(_) => return Outcome::Text(format!("ERROR: {path} 不是 UTF-8 文本")),
        };

        let lines: Vec<&str> = text.lines().collect();
        let offset = args
            .get("offset")
            .and_then(Value::as_u64)
            .map(|n| (n.max(1) as usize) - 1)
            .unwrap_or(0)
            .min(lines.len());
        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .map(|n| n as usize)
            .unwrap_or(DEFAULT_READ_LINES)
            .clamp(1, MAX_READ_LINES);

        let tag = tag_of(&text);
        let mut body = String::new();
        let mut seen = BTreeSet::new();
        let mut shown = 0usize;
        for (index, line) in lines.iter().enumerate().skip(offset).take(limit) {
            let row = format!("{}:{line}\n", index + 1);
            if body.len() + row.len() > MAX_READ_BYTES {
                body.push_str("… 输出已截断，用 offset/limit 继续读\n");
                break;
            }
            body.push_str(&row);
            seen.insert(index + 1);
            shown += 1;
        }
        if shown == 0 && !lines.is_empty() {
            body.push_str("（该范围没有内容）\n");
        }

        self.snapshots.insert(
            full.clone(),
            Snapshot {
                tag: tag.clone(),
                seen,
            },
        );
        Outcome::Text(format!("[{path}#{tag}]\n{body}"))
    }

    fn list_dir(&self, full: &Path, shown_path: &str) -> String {
        let mut entries: Vec<String> = Vec::new();
        let Ok(dir) = fs::read_dir(full) else {
            return format!("ERROR: 打不开目录 {shown_path}");
        };
        for entry in dir.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
            entries.push(if is_dir { format!("{name}/") } else { name });
            if entries.len() >= 200 {
                break;
            }
        }
        entries.sort();
        format!("{shown_path}/\n{}", entries.join("\n"))
    }

    fn write(&mut self, args: &Value) -> String {
        let (Some(path), Some(content)) = (
            args.get("path").and_then(Value::as_str),
            args.get("content").and_then(Value::as_str),
        ) else {
            return "ERROR: 需要 path 和 content".to_string();
        };
        let full = self.resolve(path);
        if let Some(parent) = full.parent()
            && let Err(err) = fs::create_dir_all(parent)
        {
            return format!("ERROR: 建目录失败：{err}");
        }
        if let Err(err) = fs::write(&full, content) {
            return format!("ERROR: 写入失败：{err}");
        }
        let tag = tag_of(content);
        self.mark_all_seen(&full, &tag, content);
        let line_count = content.lines().count();
        format!("[{path}#{tag}]\n写入 {line_count} 行")
    }

    fn edit(&mut self, args: &Value) -> String {
        let Some(input) = args.get("input").and_then(Value::as_str) else {
            return "ERROR: 缺少 input".to_string();
        };
        let (sections, bare_rows) = match parse_patch(input) {
            Ok(parsed) => parsed,
            Err(err) => return format!("ERROR: 补丁格式错误：{err}"),
        };
        if sections.is_empty() {
            return "ERROR: 补丁里没有任何改动".to_string();
        }

        let mut report = String::new();
        if bare_rows > 0 {
            report.push_str(&format!(
                "（提醒：有 {bare_rows} 行正文没有以 + 开头，已自动补上；正文请统一写成 +内容）\n"
            ));
        }
        for section in sections {
            match self.apply_section(&section) {
                Ok(line) => report.push_str(&line),
                Err(err) => return format!("ERROR: {err}"),
            }
        }
        report
    }

    fn apply_section(&mut self, section: &Section) -> std::result::Result<String, String> {
        let full = self.resolve(&section.path);
        let content =
            fs::read_to_string(&full).map_err(|err| format!("读不了 {}：{err}", section.path))?;
        let current_tag = tag_of(&content);
        if current_tag != section.tag {
            return Err(format!(
                "{} 的内容和锚点对不上（补丁写的是 {}，文件现在是 {}）。重新 read 之后再改。",
                section.path, section.tag, current_tag
            ));
        }
        let snapshot = self
            .snapshots
            .get(&full)
            .ok_or_else(|| format!("{} 还没被读过，先 read 再改", section.path))?;
        if snapshot.tag != section.tag {
            return Err(format!("{} 的快照已过期，重新 read 之后再改", section.path));
        }

        let mut lines: Vec<String> = content.lines().map(str::to_string).collect();
        let line_count = lines.len();

        // 校验：区间合法、行都在已读区间内、互不重叠。
        let mut taken: Vec<(usize, usize)> = Vec::new();
        for op in &section.ops {
            match op.kind {
                OpKind::Replace | OpKind::Cut => {
                    if op.start == 0 || op.end < op.start || op.end > line_count {
                        return Err(format!(
                            "{}:{}-{} 超出文件范围（共 {} 行）",
                            section.path, op.start, op.end, line_count
                        ));
                    }
                    for line in op.start..=op.end {
                        if !snapshot.seen.contains(&line) {
                            return Err(format!(
                                "{} 第 {} 行没有读过，不能改。先 read 这一段。",
                                section.path, line
                            ));
                        }
                    }
                    for (start, end) in &taken {
                        if op.start <= *end && op.end >= *start {
                            return Err(format!(
                                "{} 的 {}:{}-{} 与另一段改动重叠",
                                section.path, op.start, op.start, op.end
                            ));
                        }
                    }
                    taken.push((op.start, op.end));
                }
                OpKind::InsertBefore | OpKind::InsertAfter => {
                    if op.start == 0 || op.start > line_count {
                        return Err(format!(
                            "{}:{} 超出文件范围（共 {} 行）",
                            section.path, op.start, line_count
                        ));
                    }
                    if !snapshot.seen.contains(&op.start) {
                        return Err(format!(
                            "{} 第 {} 行没有读过，不能在这里插入。先 read 这一段。",
                            section.path, op.start
                        ));
                    }
                }
            }
        }

        // 从后往前改，前面的行号才不会失效。
        let mut ordered = section.ops.clone();
        ordered.sort_by(|a, b| b.start.cmp(&a.start).then(b.end.cmp(&a.end)));
        for op in ordered {
            match op.kind {
                OpKind::Replace => {
                    lines.splice(op.start - 1..op.end, op.body);
                }
                OpKind::Cut => {
                    lines.drain(op.start - 1..op.end);
                }
                OpKind::InsertBefore => {
                    for (offset, line) in op.body.into_iter().enumerate() {
                        lines.insert(op.start - 1 + offset, line);
                    }
                }
                OpKind::InsertAfter => {
                    for (offset, line) in op.body.into_iter().enumerate() {
                        lines.insert(op.start + offset, line);
                    }
                }
            }
        }

        let mut next = lines.join("\n");
        next.push('\n');
        if next == content {
            return Err(format!("{} 的改动没有产生任何变化", section.path));
        }
        fs::write(&full, &next).map_err(|err| format!("写入 {} 失败：{err}", section.path))?;

        let tag = tag_of(&next);
        let count = next.lines().count();
        self.snapshots.insert(
            full,
            Snapshot {
                tag: tag.clone(),
                seen: (1..=count).collect(),
            },
        );
        Ok(format!("[{}#{tag}] 改完共 {count} 行\n", section.path))
    }

    fn bash(&mut self, args: &Value) -> String {
        let Some(command) = args.get("command").and_then(Value::as_str) else {
            return "ERROR: 缺少 command".to_string();
        };
        let timeout = args
            .get("timeout")
            .and_then(Value::as_u64)
            .unwrap_or(300)
            .clamp(1, 3600);

        let mut child = match Command::new("/bin/sh")
            .arg("-c")
            .arg(format!("{command} 2>&1"))
            .current_dir(&self.root)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
        {
            Ok(child) => child,
            Err(err) => return format!("ERROR: 起不了进程：{err}"),
        };
        let pid = child.id();
        let mut stdout = match child.stdout.take() {
            Some(stdout) => stdout,
            None => return "ERROR: 拿不到命令输出".to_string(),
        };
        let collector = std::thread::spawn(move || {
            let mut buffer = Vec::new();
            // 读失败（管道被关掉）时返回已读到的部分，不中断这一轮。
            let _ = stdout.read_to_end(&mut buffer);
            buffer
        });

        let deadline = Instant::now() + Duration::from_secs(timeout);
        let mut timed_out = false;
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) => {}
                Err(err) => return format!("ERROR: 等待进程失败：{err}"),
            }
            if Instant::now() >= deadline {
                timed_out = true;
                // 进程可能已经自己退出，这里的失败没有补救动作。
                let _ = Command::new("kill")
                    .arg("-TERM")
                    .arg(format!("-{pid}"))
                    .status();
                let _ = child.kill();
                let _ = child.wait();
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        drop(child);

        let bytes = collector.join().unwrap_or_default();
        let text = String::from_utf8_lossy(&bytes).to_string();
        let log_path = self.write_log(&text);

        let tail_limit = 50 * 1024;
        let body = if text.len() > tail_limit {
            let mut start = text.len() - tail_limit;
            while !text.is_char_boundary(start) {
                start += 1;
            }
            format!("…（只显示尾部 50 KB）\n{}", &text[start..])
        } else {
            text.clone()
        };

        if timed_out {
            format!("ERROR: 超过 {timeout} 秒，已终止\n{body}\n完整输出：{log_path}")
        } else {
            format!("{body}\n完整输出：{log_path}")
        }
    }

    fn write_log(&mut self, text: &str) -> String {
        self.log_seq += 1;
        // 目录建不出来时下面的写文件会失败，结果里会带上原因。
        let _ = fs::create_dir_all(&self.log_dir);
        let path = self.log_dir.join(format!("{}.log", self.log_seq));
        match fs::write(&path, text) {
            Ok(()) => path.display().to_string(),
            Err(err) => format!("（日志写入失败：{err}）"),
        }
    }

    fn grep(&mut self, args: &Value) -> String {
        let Some(pattern) = args.get("pattern").and_then(Value::as_str) else {
            return "ERROR: 缺少 pattern".to_string();
        };
        let case_sensitive = args.get("case").and_then(Value::as_bool).unwrap_or(true);
        let expression = if case_sensitive {
            pattern.to_string()
        } else {
            format!("(?i){pattern}")
        };
        let re = match Regex::new(&expression) {
            Ok(re) => re,
            Err(err) => return format!("ERROR: 正则不合法：{err}"),
        };

        let base = args
            .get("path")
            .and_then(Value::as_str)
            .map(|p| self.resolve(p))
            .unwrap_or_else(|| self.root.clone());
        let file_filter = args.get("glob").and_then(Value::as_str);

        let mut files = Vec::new();
        collect_files(&base, &mut files);
        files.sort();

        let mut out = String::new();
        let mut total = 0usize;
        for file in files {
            let relative = self.relative(&file);
            if let Some(filter) = file_filter
                && !glob_match(filter, &relative)
            {
                continue;
            }
            let Ok(text) = fs::read_to_string(&file) else {
                continue;
            };
            let tag = tag_of(&text);
            let mut per_file = 0usize;
            let mut seen = BTreeSet::new();
            for (index, line) in text.lines().enumerate() {
                if !re.is_match(line) {
                    continue;
                }
                let mut trimmed = line.to_string();
                if trimmed.chars().count() > GREP_LINE_CHARS {
                    trimmed = trimmed.chars().take(GREP_LINE_CHARS).collect();
                    trimmed.push('…');
                }
                out.push_str(&format!("{relative}:{}:{trimmed}\n", index + 1));
                seen.insert(index + 1);
                per_file += 1;
                total += 1;
                if per_file >= GREP_PER_FILE || total >= GREP_TOTAL {
                    break;
                }
            }
            if !seen.is_empty() {
                self.snapshots.insert(file, Snapshot { tag, seen });
            }
            if total >= GREP_TOTAL {
                out.push_str("… 结果过多，只显示前 200 条\n");
                break;
            }
        }

        if out.is_empty() {
            "没有匹配".to_string()
        } else {
            out
        }
    }

    fn glob(&mut self, args: &Value) -> String {
        let Some(pattern) = args.get("pattern").and_then(Value::as_str) else {
            return "ERROR: 缺少 pattern".to_string();
        };
        let base = args
            .get("path")
            .and_then(Value::as_str)
            .map(|p| self.resolve(p))
            .unwrap_or_else(|| self.root.clone());

        let mut files = Vec::new();
        collect_files(&base, &mut files);

        let mut matched: Vec<(u128, String)> = files
            .into_iter()
            .filter_map(|file| {
                let relative = self.relative(&file);
                if !glob_match(pattern, &relative) {
                    return None;
                }
                let modified = fs::metadata(&file)
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_millis())
                    .unwrap_or(0);
                Some((modified, relative))
            })
            .collect();

        matched.sort_by_key(|a| std::cmp::Reverse(a.0));
        matched.truncate(GLOB_LIMIT);

        if matched.is_empty() {
            "没有匹配".to_string()
        } else {
            matched
                .into_iter()
                .map(|(_, path)| path)
                .collect::<Vec<_>>()
                .join("\n")
        }
    }

    fn resolve(&self, raw: &str) -> PathBuf {
        let path = PathBuf::from(raw);
        let full = if path.is_absolute() {
            path
        } else {
            self.root.join(path)
        };
        normalize(&full)
    }

    fn relative(&self, full: &Path) -> String {
        full.strip_prefix(&self.root)
            .unwrap_or(full)
            .display()
            .to_string()
    }

    fn mark_all_seen(&mut self, full: &Path, tag: &str, content: &str) {
        let count = content.lines().count();
        self.snapshots.insert(
            full.to_path_buf(),
            Snapshot {
                tag: tag.to_string(),
                seen: (1..=count).collect(),
            },
        );
    }
}

struct Section {
    path: String,
    tag: String,
    ops: Vec<Op>,
}

#[derive(Clone)]
struct Op {
    kind: OpKind,
    start: usize,
    end: usize,
    body: Vec<String>,
}

#[derive(Clone, Copy, PartialEq)]
enum OpKind {
    Replace,
    InsertBefore,
    InsertAfter,
    Cut,
}

fn parse_patch(input: &str) -> std::result::Result<(Vec<Section>, usize), String> {
    let mut sections: Vec<Section> = Vec::new();
    let mut current_op: Option<Op> = None;
    let mut bare_rows = 0usize;

    fn flush(
        sections: &mut [Section],
        current: &mut Option<Op>,
    ) -> std::result::Result<(), String> {
        if let Some(op) = current.take() {
            sections
                .last_mut()
                .ok_or("补丁缺少 [路径#TAG] 开头")?
                .ops
                .push(op);
        }
        Ok(())
    }

    for raw in input.lines() {
        let line = raw.trim_end();

        // 容忍 `*** Begin Patch` / `*** End Patch` 这类信封。
        if line.starts_with("***") && line.ends_with("Patch") {
            continue;
        }

        if line.starts_with('[') && line.ends_with(']') {
            flush(&mut sections, &mut current_op)?;
            let inner = &line[1..line.len() - 1];
            let (path, tag) = inner
                .rsplit_once('#')
                .ok_or_else(|| format!("{inner} 缺少 #TAG"))?;
            sections.push(Section {
                path: path.to_string(),
                tag: tag.to_string(),
                ops: Vec::new(),
            });
            continue;
        }

        if let Some(rest) = line.strip_prefix('+') {
            let Some(op) = current_op.as_mut() else {
                return Err("+ 行前面没有操作".to_string());
            };
            op.body.push(rest.to_string());
            continue;
        }

        if line.is_empty() {
            continue;
        }

        if let Some(rest) = line.strip_prefix("PUT ") {
            flush(&mut sections, &mut current_op)?;
            let Some(rest) = rest.strip_suffix(':') else {
                return Err(format!("PUT 需要一个冒号：{line}"));
            };
            let op = if let Some(number) = rest.strip_prefix('<') {
                Op {
                    kind: OpKind::InsertBefore,
                    start: parse_line_no(number)?,
                    end: 0,
                    body: Vec::new(),
                }
            } else if let Some(number) = rest.strip_prefix('>') {
                Op {
                    kind: OpKind::InsertAfter,
                    start: parse_line_no(number)?,
                    end: 0,
                    body: Vec::new(),
                }
            } else {
                let (start, end) = parse_range(rest)?;
                Op {
                    kind: OpKind::Replace,
                    start,
                    end,
                    body: Vec::new(),
                }
            };
            current_op = Some(op);
            continue;
        }

        if let Some(rest) = line.strip_prefix("CUT ") {
            flush(&mut sections, &mut current_op)?;
            let (start, end) = parse_range(rest)?;
            sections
                .last_mut()
                .ok_or("补丁缺少 [路径#TAG] 开头")?
                .ops
                .push(Op {
                    kind: OpKind::Cut,
                    start,
                    end,
                    body: Vec::new(),
                });
            continue;
        }

        // 容错：PUT 后面直接写正文、漏了 `+` 的行，按正文处理并计数。
        if let Some(op) = current_op.as_mut() {
            op.body.push(line.to_string());
            bare_rows += 1;
            continue;
        }

        return Err(format!("看不懂这一行：{line}"));
    }

    flush(&mut sections, &mut current_op)?;
    Ok((sections, bare_rows))
}

fn parse_line_no(raw: &str) -> std::result::Result<usize, String> {
    raw.trim()
        .parse::<usize>()
        .map_err(|_| format!("行号不合法：{raw}"))
}

fn parse_range(raw: &str) -> std::result::Result<(usize, usize), String> {
    let (start, end) = raw
        .split_once(".=")
        .ok_or_else(|| format!("区间要写成 起始.=结束：{raw}"))?;
    Ok((parse_line_no(start)?, parse_line_no(end)?))
}

/// 文件内容的 4 位十六进制摘要，用作锚点。
fn tag_of(content: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in content.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{:04X}", (hash & 0xffff) as u16)
}

fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn collect_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            let name = entry.file_name().to_string_lossy().to_string();
            if SKIP_DIRS.contains(&name.as_str()) {
                continue;
            }
            collect_files(&path, out);
        } else if file_type.is_file() {
            out.push(path);
        }
    }
}

/// 只支持 `*` 和 `**` 的最小通配符匹配。
fn glob_match(pattern: &str, path: &str) -> bool {
    let pattern_parts: Vec<&str> = pattern.split('/').collect();
    let path_parts: Vec<&str> = path.split('/').collect();
    match_parts(&pattern_parts, &path_parts)
}

fn match_parts(pattern: &[&str], path: &[&str]) -> bool {
    match pattern.split_first() {
        None => path.is_empty(),
        Some((&"**", rest)) => {
            if match_parts(rest, path) {
                return true;
            }
            match path.split_first() {
                Some((_, tail)) => match_parts(pattern, tail),
                None => false,
            }
        }
        Some((segment, rest)) => match path.split_first() {
            Some((candidate, tail)) => segment_match(segment, candidate) && match_parts(rest, tail),
            None => false,
        },
    }
}

fn segment_match(pattern: &str, text: &str) -> bool {
    if !pattern.contains('*') {
        return pattern == text;
    }
    let mut remainder = text;
    let mut parts = pattern.split('*').peekable();
    if let Some(first) = parts.next()
        && !first.is_empty()
    {
        let Some(stripped) = remainder.strip_prefix(first) else {
            return false;
        };
        remainder = stripped;
    }
    while let Some(part) = parts.next() {
        if part.is_empty() {
            continue;
        }
        if parts.peek().is_none() {
            return remainder.ends_with(part);
        }
        let Some(position) = remainder.find(part) else {
            return false;
        };
        remainder = &remainder[position + part.len()..];
    }
    true
}

fn image_mime(path: &Path) -> Option<&'static str> {
    match path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        _ => None,
    }
}
