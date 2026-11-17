use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant, UNIX_EPOCH};

use serde_json::{Value, json};
use std::os::unix::process::CommandExt;

use crate::jobs;
use crate::lsp::Lsp;
use crate::provider::Event;
use crate::session::ToolCall;

/// 搜索类命令没找到东西时的提示。grep 约定：0 有匹配、1 没匹配、2 真出错。
pub const NO_MATCH: &str = "没有匹配（退出码 1）";

/// 命令里是否在跑搜索工具。只看命令词，避免把 `false`、编译错误这类
/// 退出码 1 也当成「没找到」。
fn searches(command: &str) -> bool {
    command
        .split(|ch: char| ch.is_whitespace() || matches!(ch, ';' | '|' | '&' | '(' | ')'))
        .any(|word| matches!(word, "grep" | "egrep" | "fgrep" | "rg" | "ag" | "ack"))
}

const DEFAULT_READ_LINES: usize = 300;
const MAX_READ_LINES: usize = 3000;
const MAX_READ_BYTES: usize = 50 * 1024;
const GREP_PER_FILE: usize = 20;
const GREP_TOTAL: usize = 200;
/// 遍历时一律跳过的目录：依赖与构建产物，即使没写进 .gitignore 也不该被搜、被列。
const SKIP_DIRS: [&str; 3] = ["node_modules", "target", "vendor"];
const GREP_LINE_CHARS: usize = 512;
const GLOB_LIMIT: usize = 200;

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
    lsp: Lsp,
}

impl Tools {
    pub fn new(
        root: PathBuf,
        log_dir: PathBuf,
        gopls: String,
        phpantom: String,
        rust_analyzer: String,
    ) -> Tools {
        Tools {
            root,
            log_dir,
            snapshots: HashMap::new(),
            log_seq: 0,
            lsp: Lsp::new(gopls, phpantom, rust_analyzer),
        }
    }

    /// 七个工具的 JSON Schema。顺序固定，保证请求前缀稳定。
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
                    "description": "执行 shell 命令，返回合并后的输出尾部。命令默认已在项目根，不要在开头写 cd；要去子目录或别处就设 cwd。",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "command": { "type": "string" },
                            "cwd": {
                                "type": "string",
                                "description": "在哪个目录执行；相对路径按项目根解析，绝对路径原样使用；默认项目根"
                            },
                            "timeout": { "type": "integer", "description": "秒，默认 300" },
                            "background": {
                                "type": "boolean",
                                "description": "设为 true 就把命令丢后台，立刻返回；跑完的输出会在下一轮对话开始时自动带上"
                            }
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
                    "name": "lsp",
                    "description": "查询 Go、PHP、Rust 代码的语言服务：跳定义、找引用。需要服务器已安装（gopls / phpantom_lsp / rust-analyzer）。",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "action": {
                                "type": "string",
                                "enum": ["definition", "references"],
                                "description": "要做什么"
                            },
                            "path": { "type": "string", "description": "相对项目根的 .go、.php 或 .rs 文件" },
                            "line": {
                                "type": "integer",
                                "description": "行号，从 1 开始；必填"
                            },
                            "symbol": { "type": "string", "description": "这一行里的符号名；省略则用第一个非空白字符" }
                        },
                        "required": ["action", "path"]
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
    /// 把命令开头多余的 `cd <项目根> && ` 去掉：项目根本来就是工作目录。
    /// 只认完全等于项目根的路径；别的目录保持原样。
    fn strip_redundant_cd(&self, command: &str) -> String {
        let root = self.root.to_string_lossy();
        let root = root.trim_end_matches('/');
        for prefix in [
            format!("cd {root} && "),
            format!("cd {root}/ && "),
            format!("cd \"{root}\" && "),
            format!("cd '{root}' && "),
        ] {
            if let Some(rest) = command.strip_prefix(&prefix) {
                return rest.to_string();
            }
        }
        command.to_string()
    }

    pub fn describe(&self, call: &ToolCall) -> String {
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
            // bash 只给命令本身，界面会加上 `$` 前缀；不截断，长命令照原样显示。
            "bash" => self.strip_redundant_cd(&pick("command")),
            "grep" => format!("grep {}", pick("pattern")),
            "glob" => format!("glob {}", pick("pattern")),
            "lsp" => format!("lsp {} {}", pick("action"), pick("path")),
            other => other.to_string(),
        }
    }

    /// 执行工具。失败一律以 `ERROR: ...` 文本返回给模型，绝不中断循环。
    pub fn execute(&mut self, call: &ToolCall, on: &mut dyn FnMut(Event)) -> Outcome {
        let args: Value = match serde_json::from_str(&call.arguments) {
            Ok(args) => args,
            Err(_) => return Outcome::Text("ERROR: 参数不是合法 JSON".to_string()),
        };
        let text = match call.name.as_str() {
            "read" => return self.read(&args),
            "write" => self.write(&args),
            "edit" => self.edit(&args),
            "bash" => self.bash(&args, on),
            "grep" => self.grep(&args),
            "glob" => self.glob(&args),
            "lsp" => return Outcome::Text(self.lsp_call(&args)),
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
        let total = lines.len();
        let mut body = String::new();
        let mut seen = BTreeSet::new();
        let mut shown = 0usize;
        let mut cut_by_bytes = false;
        for (index, line) in lines.iter().enumerate().skip(offset).take(limit) {
            let row = format!("{}:{line}\n", index + 1);
            if body.len() + row.len() > MAX_READ_BYTES {
                cut_by_bytes = true;
                break;
            }
            body.push_str(&row);
            seen.insert(index + 1);
            shown += 1;
        }
        if offset + shown < total {
            let reason = if cut_by_bytes {
                "输出过大"
            } else {
                "这一段先到这里"
            };
            body.push_str(&format!(
                "… {reason}，共 {total} 行；用 offset={} 继续读\n",
                offset + shown + 1
            ));
        }
        if shown == 0 && !lines.is_empty() {
            body.push_str("（该范围没有内容）\n");
        }

        self.record_snapshot(full.clone(), tag.clone(), seen);
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

        // 同一个文件的多个段落先合并，保证它们共用同一个锚点、一次校验。
        let mut groups: Vec<(PathBuf, String, String, Vec<Op>)> = Vec::new();
        for section in sections {
            let full = self.resolve(&section.path);
            match groups.iter_mut().find(|group| group.0 == full) {
                Some(group) => {
                    if group.2 != section.tag {
                        return format!(
                            "ERROR: {} 在同一份补丁里用了两个不同的锚点（{} 和 {}）",
                            section.path, group.2, section.tag
                        );
                    }
                    group.3.extend(section.ops);
                }
                None => groups.push((full, section.path.clone(), section.tag.clone(), section.ops)),
            }
        }

        // 第一阶段：全部校验、算出结果。任何一处失败都不写盘。
        let mut prepared = Vec::with_capacity(groups.len());
        for (full, shown, tag, ops) in groups {
            match self.prepare_file(&full, &shown, &tag, ops) {
                Ok(item) => prepared.push(item),
                Err(err) => return format!("ERROR: {err}"),
            }
        }

        // 第二阶段：统一落盘，然后更新快照。
        let mut report = String::new();
        if bare_rows > 0 {
            report.push_str(&format!(
                "（提醒：有 {bare_rows} 行正文没有以 + 开头，已自动补上；正文请统一写成 +内容）\n"
            ));
        }
        let mut written: Vec<String> = Vec::new();
        let mut changed: Vec<String> = Vec::new();
        for item in prepared {
            if let Err(err) = fs::write(&item.path, &item.content) {
                let done = if written.is_empty() {
                    "无".to_string()
                } else {
                    written.join("、")
                };
                return format!("ERROR: 写入 {} 失败：{err}；已写入：{done}", item.shown);
            }
            self.record_snapshot(item.path, item.tag.clone(), item.seen);
            changed.push(format!("{}#{}", item.shown, item.tag));
            written.push(item.shown);
        }
        // 改动清单一行写完：每个文件只留「路径#新锚点」，不报行数。
        report.push_str(&changed.join("、"));
        report
    }

    /// 校验一个文件的全部操作并算出新内容；不写盘。
    fn prepare_file(
        &self,
        full: &Path,
        shown: &str,
        tag: &str,
        ops: Vec<Op>,
    ) -> std::result::Result<Prepared, String> {
        let content = fs::read_to_string(full).map_err(|err| format!("读不了 {shown}：{err}"))?;
        let current_tag = tag_of(&content);
        if current_tag != tag {
            return Err(format!(
                "{shown} 的内容和锚点对不上（补丁写的是 {tag}，文件现在是 {current_tag}）。重新 read 之后再改。"
            ));
        }
        let snapshot = self
            .snapshots
            .get(full)
            .ok_or_else(|| format!("{shown} 还没被读过，先 read 再改"))?;
        if snapshot.tag != tag {
            return Err(format!("{shown} 的快照已过期，重新 read 之后再改"));
        }

        let lines: Vec<String> = content.lines().map(str::to_string).collect();
        let line_count = lines.len();

        let mut taken: Vec<(usize, usize)> = Vec::new();
        for op in &ops {
            match op.kind {
                OpKind::Replace | OpKind::Cut => {
                    if op.start == 0 || op.end < op.start || op.end > line_count {
                        return Err(format!(
                            "{shown}:{}-{} 超出文件范围（共 {} 行）",
                            op.start, op.end, line_count
                        ));
                    }
                    for line in op.start..=op.end {
                        if !snapshot.seen.contains(&line) {
                            return Err(format!(
                                "{shown} 第 {line} 行没有读过，不能改。先 read 这一段。"
                            ));
                        }
                    }
                    for (start, end) in &taken {
                        if op.start <= *end && op.end >= *start {
                            return Err(format!(
                                "{shown} 的 {}:{}-{} 与另一段改动重叠",
                                op.start, op.start, op.end
                            ));
                        }
                    }
                    taken.push((op.start, op.end));
                }
                OpKind::InsertBefore | OpKind::InsertAfter => {
                    if op.start == 0 || op.start > line_count {
                        return Err(format!(
                            "{shown}:{} 超出文件范围（共 {} 行）",
                            op.start, line_count
                        ));
                    }
                    if !snapshot.seen.contains(&op.start) {
                        return Err(format!(
                            "{shown} 第 {} 行没有读过，不能在这里插入。先 read 这一段。",
                            op.start
                        ));
                    }
                }
            }
        }

        let mut ordered = ops;
        ordered.sort_by_key(|op| (op.start, op.end));

        // 升序单遍：一边产出新内容，一边把旧的已读行映射到新行号。
        let total = lines.len();
        let mut out: Vec<String> = Vec::with_capacity(total + 8);
        let mut seen_out: BTreeSet<usize> = BTreeSet::new();
        let mut cursor = 0usize;
        for op in &ordered {
            match op.kind {
                OpKind::Replace | OpKind::Cut => {
                    copy_seen(
                        &lines,
                        &snapshot.seen,
                        cursor,
                        op.start - 1,
                        &mut out,
                        &mut seen_out,
                    );
                    if op.kind == OpKind::Replace {
                        for line in &op.body {
                            out.push(line.clone());
                            seen_out.insert(out.len());
                        }
                    }
                    cursor = op.end;
                }
                OpKind::InsertBefore => {
                    copy_seen(
                        &lines,
                        &snapshot.seen,
                        cursor,
                        op.start - 1,
                        &mut out,
                        &mut seen_out,
                    );
                    cursor = op.start - 1;
                    for line in &op.body {
                        out.push(line.clone());
                        seen_out.insert(out.len());
                    }
                }
                OpKind::InsertAfter => {
                    copy_seen(
                        &lines,
                        &snapshot.seen,
                        cursor,
                        op.start,
                        &mut out,
                        &mut seen_out,
                    );
                    cursor = op.start;
                    for line in &op.body {
                        out.push(line.clone());
                        seen_out.insert(out.len());
                    }
                }
            }
        }
        copy_seen(
            &lines,
            &snapshot.seen,
            cursor,
            total,
            &mut out,
            &mut seen_out,
        );

        let lines = out;
        let mut next = lines.join("\n");
        next.push('\n');
        if next == content {
            return Err(format!("{shown} 的改动没有产生任何变化"));
        }
        Ok(Prepared {
            path: full.to_path_buf(),
            shown: shown.to_string(),
            tag: tag_of(&next),
            content: next,
            seen: seen_out,
        })
    }

    /// 记录快照：同一份内容（锚点相同）时把新看到的行并进去，不同才替换。
    fn record_snapshot(&mut self, path: PathBuf, tag: String, seen: BTreeSet<usize>) {
        let merged = self
            .snapshots
            .get(&path)
            .filter(|existing| existing.tag == tag)
            .map(|existing| {
                let mut merged = existing.seen.clone();
                merged.extend(seen.iter().copied());
                merged
            });
        let seen = merged.unwrap_or(seen);
        self.snapshots.insert(path, Snapshot { tag, seen });
    }

    fn bash(&mut self, args: &Value, on: &mut dyn FnMut(Event)) -> String {
        let Some(command) = args.get("command").and_then(Value::as_str) else {
            return "ERROR: 缺少 command".to_string();
        };
        // 模型常把项目根写成 `cd <根> && ...`：工作目录本来就是项目根，剥掉再跑。
        let command = self.strip_redundant_cd(command);
        let timeout = args
            .get("timeout")
            .and_then(Value::as_u64)
            .unwrap_or(300)
            .clamp(1, 3600);
        let working_dir = args
            .get("cwd")
            .and_then(Value::as_str)
            .map(|dir| self.resolve(dir))
            .unwrap_or_else(|| self.root.clone());
        if !working_dir.is_dir() {
            return format!("ERROR: cwd 不是目录：{}", working_dir.display());
        }

        let log_path = match self.next_log_path() {
            Ok(path) => path,
            Err(err) => return format!("ERROR: 建日志失败：{err}"),
        };
        let log_file = match fs::File::create(&log_path) {
            Ok(file) => file,
            Err(err) => return format!("ERROR: 建日志失败：{err}"),
        };

        let mut child = match Command::new("/bin/sh")
            .arg("-c")
            .arg(&command)
            .current_dir(&working_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .spawn()
        {
            Ok(child) => child,
            Err(err) => return format!("ERROR: 起不了进程：{err}"),
        };
        let pid = child.id();
        let Some(stdout) = child.stdout.take() else {
            return "ERROR: 拿不到命令输出".to_string();
        };
        let Some(stderr) = child.stderr.take() else {
            return "ERROR: 拿不到命令错误输出".to_string();
        };
        // 输出边读边写进日志，内存里只留最后 50 KB，避免大输出撑爆内存。
        // 后台进程（`cmd &`）会一直占着管道写端，所以读线程只负责往里灌数据，
        // 主流程用「有限等待」取快照，绝不能无限 join。
        let stdout_tail: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let stderr_tail: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let stdout_total = Arc::new(AtomicU64::new(0));
        let (done_tx, done_rx) = mpsc::channel();

        let tail = Arc::clone(&stdout_tail);
        let total = Arc::clone(&stdout_total);
        let tx = done_tx.clone();
        std::thread::spawn(move || {
            stream_into_log(log_file, stdout, &tail, &total);
            let _ = tx.send(());
        });
        let tail = Arc::clone(&stderr_tail);
        std::thread::spawn(move || {
            read_tail(stderr, &tail);
            let _ = done_tx.send(());
        });

        if args
            .get("background")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            let label = match jobs::register(&command, pid) {
                Ok(label) => label,
                Err(err) => {
                    signal_group(pid, libc::SIGTERM);
                    let _ = child.kill();
                    let _ = child.wait();
                    return format!("ERROR: {err}");
                }
            };
            let thread_label = label.clone();
            std::thread::spawn(move || {
                let started = Instant::now();
                let output = Self::wait_for_command(
                    child,
                    done_rx,
                    stdout_tail,
                    stderr_tail,
                    stdout_total,
                    pid,
                    timeout,
                    log_path,
                    command,
                    None,
                );
                jobs::finish(&thread_label, output, started.elapsed().as_secs_f32());
            });
            return format!(
                "已转入后台：{label}。命令跑完后，输出会在下一轮对话开始时自动带上，不用再问。"
            );
        }

        Self::wait_for_command(
            child,
            done_rx,
            stdout_tail,
            stderr_tail,
            stdout_total,
            pid,
            timeout,
            log_path,
            command,
            Some(on),
        )
    }

    /// 等命令结束并整理成工具输出；前台边等边发心跳，后台什么都不打扰。
    #[allow(clippy::too_many_arguments)]
    fn wait_for_command(
        mut child: std::process::Child,
        done_rx: mpsc::Receiver<()>,
        stdout_tail: Arc<Mutex<Vec<u8>>>,
        stderr_tail: Arc<Mutex<Vec<u8>>>,
        stdout_total: Arc<AtomicU64>,
        pid: u32,
        timeout: u64,
        log_path: PathBuf,
        command: String,
        mut tick: Option<&mut dyn FnMut(Event)>,
    ) -> String {
        let deadline = Instant::now() + Duration::from_secs(timeout);
        let mut status = None;
        let mut timed_out = false;
        loop {
            // 只有前台才响应 Ctrl-C：后台命令是用户明确放走的，不该被这一轮连坐。
            if tick.is_some() && crate::cancel::check() {
                // 用户要中断：按进程组杀，避免只杀掉 sh 留下子进程。
                signal_group(pid, libc::SIGTERM);
                let _ = child.kill();
                let _ = child.wait();
                return "ERROR: 已取消这条命令".to_string();
            }
            match child.try_wait() {
                Ok(Some(finished)) => {
                    status = Some(finished);
                    break;
                }
                Ok(None) => {}
                Err(err) => return format!("ERROR: 等待进程失败：{err}"),
            }
            if Instant::now() >= deadline {
                timed_out = true;
                signal_group(pid, libc::SIGTERM);
                std::thread::sleep(Duration::from_millis(300));
                // 孙进程可能忽略 TERM；补一次 KILL，管道才会关闭，收集线程才不会一直等。
                signal_group(pid, libc::SIGKILL);
                let _ = child.kill();
                let _ = child.wait();
                break;
            }
            // 命令跑多久，状态行就跳多久；界面自己决定要不要显示。
            if let Some(on) = tick.as_deref_mut() {
                on(Event::Tick);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        drop(child);

        // shell 退了不代表管道关了：后台进程还拿着写端。最多再等 300 毫秒收尾巴。
        let grace = Instant::now() + Duration::from_millis(300);
        let mut finished = 0;
        while finished < 2 {
            let left = grace.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            if done_rx.recv_timeout(left).is_ok() {
                finished += 1;
            }
        }
        let stdout_tail = lock_copy(&stdout_tail);
        let stderr_tail = lock_copy(&stderr_tail);
        let stdout_total = stdout_total.load(Ordering::Relaxed);
        if !stderr_tail.is_empty() {
            // stderr 补进日志末尾；写失败不影响本轮的返回值。
            if let Ok(mut file) = fs::OpenOptions::new().append(true).open(&log_path) {
                let _ = file.write_all(b"\n[stderr]\n");
                let _ = file.write_all(&stderr_tail);
            }
        }

        let mut bytes = stdout_tail;
        if !stderr_tail.is_empty() {
            bytes.push(b'\n');
            bytes.extend_from_slice(&stderr_tail);
        }
        let text = String::from_utf8_lossy(&bytes).to_string();

        const TAIL: usize = 50 * 1024;
        let body = if stdout_total as usize > TAIL {
            format!("…（只显示尾部 50 KB）\n{text}")
        } else {
            text
        };

        // 搜索工具用 1 表示「没找到」，不是出错：换个写法，别让上层当成失败。
        let code = match &status {
            Some(finished) => match finished.code() {
                Some(1) if searches(&command) => NO_MATCH.to_string(),
                Some(code) => format!("退出码：{code}"),
                None => "退出码：被信号终止".to_string(),
            },
            None => "退出码：被信号终止".to_string(),
        };

        if timed_out {
            format!(
                "ERROR: 超过 {timeout} 秒，已终止\n{body}\n{code}\n完整输出：{}",
                log_path.display()
            )
        } else {
            format!("{body}\n{code}\n完整输出：{}", log_path.display())
        }
    }

    fn next_log_path(&mut self) -> std::io::Result<PathBuf> {
        self.log_seq += 1;
        fs::create_dir_all(&self.log_dir)?;
        Ok(self.log_dir.join(format!("{}.log", self.log_seq)))
    }

    fn grep(&mut self, args: &Value) -> String {
        let Some(pattern) = args.get("pattern").and_then(Value::as_str) else {
            return "ERROR: 缺少 pattern".to_string();
        };
        let case_sensitive = args.get("case").and_then(Value::as_bool).unwrap_or(true);
        let base = args
            .get("path")
            .and_then(Value::as_str)
            .map(|p| self.resolve(p))
            .unwrap_or_else(|| self.root.clone());

        let mut command = Command::new("rg");
        command
            .arg("--json")
            .arg("--line-number")
            .current_dir(&self.root)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        if !case_sensitive {
            command.arg("--ignore-case");
        }
        for dir in SKIP_DIRS {
            command.arg("--glob").arg(format!("!{dir}"));
        }
        if let Some(filter) = args.get("glob").and_then(Value::as_str) {
            command.arg("--glob").arg(filter);
        }
        command.arg("--").arg(pattern).arg(self.rg_target(&base));

        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(err) => {
                return format!("ERROR: 起不了 rg（需要安装 ripgrep）：{err}");
            }
        };
        let Some(stdout) = child.stdout.take() else {
            return "ERROR: 拿不到 rg 的输出".to_string();
        };

        let mut out = String::new();
        let mut total = 0usize;
        let mut per_file: HashMap<String, usize> = HashMap::new();
        let mut snapshots: Vec<(PathBuf, String, BTreeSet<usize>)> = Vec::new();
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        loop {
            line.clear();
            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                break;
            }
            let Ok(event) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if event.get("type").and_then(Value::as_str) != Some("match") {
                continue;
            }
            let Some(path) = event["data"]["path"]["text"].as_str() else {
                continue;
            };
            let Some(number) = event["data"]["line_number"].as_u64() else {
                continue;
            };
            let text = event["data"]["lines"]["text"]
                .as_str()
                .unwrap_or("")
                .trim_end_matches('\n');

            let count = per_file.entry(path.to_string()).or_insert(0);
            if *count >= GREP_PER_FILE {
                continue;
            }
            *count += 1;
            total += 1;
            out.push_str(&format!(
                "{path}:{number}:{}\n",
                truncate_chars(text, GREP_LINE_CHARS)
            ));

            match snapshots
                .iter_mut()
                .find(|entry| entry.0 == self.root.join(path))
            {
                Some(entry) => {
                    entry.2.insert(number as usize);
                }
                None => {
                    let full = self.root.join(path);
                    // 大文件不记快照：读它只为算锚点，代价太高；改它之前重新 read 即可。
                    let too_big = fs::metadata(&full)
                        .map(|meta| meta.len() > 4 * 1024 * 1024)
                        .unwrap_or(true);
                    if !too_big && let Ok(content) = fs::read_to_string(&full) {
                        let mut seen = BTreeSet::new();
                        seen.insert(number as usize);
                        snapshots.push((full, tag_of(&content), seen));
                    }
                }
            }

            if total >= GREP_TOTAL {
                out.push_str("… 结果过多，只显示前 200 条\n");
                break;
            }
        }
        // 提前收工时把 rg 收掉，避免它继续扫整个仓库。
        let _ = child.kill();
        let _ = child.wait();

        for (path, tag, seen) in snapshots {
            self.record_snapshot(path, tag, seen);
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
        let base_is_file = base.is_file();

        // `rg --files` 只列未被 .gitignore 忽略的文件，比我们自己遍历快两个数量级。
        let mut command = Command::new("rg");
        command.arg("--files").current_dir(&self.root);
        for dir in SKIP_DIRS {
            command.arg("--glob").arg(format!("!{dir}"));
        }
        if !base_is_file {
            command.arg(self.rg_target(&base));
        }
        let output = match command.stdin(Stdio::null()).stderr(Stdio::null()).output() {
            Ok(output) => output,
            Err(err) => return format!("ERROR: 起不了 rg（需要安装 ripgrep）：{err}"),
        };

        let mut candidates: Vec<String> = Vec::new();
        if base_is_file {
            if let Some(name) = base.file_name().map(|n| n.to_string_lossy().to_string())
                && glob_match(pattern, &name)
            {
                candidates.push(self.relative(&base));
            }
        } else {
            for line in String::from_utf8_lossy(&output.stdout).lines() {
                let relative = line.trim();
                if relative.is_empty() {
                    continue;
                }
                let target = self
                    .root
                    .join(relative)
                    .strip_prefix(&base)
                    .map(|rest| rest.display().to_string())
                    .unwrap_or_else(|_| relative.to_string());
                if glob_match(pattern, &target) {
                    candidates.push(relative.to_string());
                }
            }
        }

        let mut matched: Vec<(u128, String)> = candidates
            .into_iter()
            .map(|relative| {
                let modified = fs::metadata(self.root.join(&relative))
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_millis())
                    .unwrap_or(0);
                (modified, relative)
            })
            .collect();

        matched.sort_by_key(|entry| std::cmp::Reverse(entry.0));
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

    fn lsp_call(&mut self, args: &Value) -> String {
        let Some(action) = args.get("action").and_then(Value::as_str) else {
            return "ERROR: 缺少 action".to_string();
        };
        let Some(path) = args.get("path").and_then(Value::as_str) else {
            return "ERROR: 缺少 path".to_string();
        };
        let line = args
            .get("line")
            .and_then(Value::as_u64)
            .map(|value| value as usize);
        let symbol = args.get("symbol").and_then(Value::as_str);
        let full = self.resolve(path);
        match self.lsp.call(action, &full, line, symbol) {
            Ok(text) => text,
            Err(err) => format!("ERROR: {err}"),
        }
    }

    fn rg_target(&self, base: &Path) -> String {
        match base.strip_prefix(&self.root) {
            Ok(rest) if !rest.as_os_str().is_empty() => rest.display().to_string(),
            _ => ".".to_string(),
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
        self.record_snapshot(full.to_path_buf(), tag.to_string(), (1..=count).collect());
    }
}

/// 已校验、待写盘的一个文件。
struct Prepared {
    path: PathBuf,
    shown: String,
    tag: String,
    content: String,
    seen: BTreeSet<usize>,
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

/// 原样拷贝 [from, to) 区间的旧行，并保留它们原来的"已读"状态。
fn copy_seen(
    lines: &[String],
    seen: &BTreeSet<usize>,
    from: usize,
    to: usize,
    out: &mut Vec<String>,
    seen_out: &mut BTreeSet<usize>,
) {
    for (index, line) in lines.iter().enumerate().take(to).skip(from) {
        out.push(line.clone());
        if seen.contains(&(index + 1)) {
            seen_out.insert(out.len());
        }
    }
}

/// 边读边写日志，返回（最后 50 KB，总字节数）。
fn stream_into_log(
    file: fs::File,
    mut source: impl Read,
    tail: &Arc<Mutex<Vec<u8>>>,
    total: &Arc<AtomicU64>,
) {
    let mut file = std::io::BufWriter::with_capacity(64 * 1024, file);
    let mut buffer = [0u8; 8192];
    loop {
        match source.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => {
                total.fetch_add(read as u64, Ordering::Relaxed);
                // 日志写入失败不中断命令：返回值里仍然带上已经读到的内容。
                let _ = file.write_all(&buffer[..read]);
                push_tail(tail, &buffer[..read]);
            }
            Err(_) => break,
        }
    }
    let _ = file.flush();
}

/// 往「只留最后 50 KB」的共享缓冲里追加。
/// 向整个进程组发信号。组已经不存在（进程都退光了）时 `kill` 返回 ESRCH，这里不关心。
fn signal_group(pgid: u32, signal: libc::c_int) {
    // SAFETY: 只是一次系统调用；负的 pid 表示进程组。pid_t 是 i32，进程号远小于它的上限。
    unsafe {
        libc::kill(-(pgid as libc::pid_t), signal);
    }
}

fn push_tail(tail: &Arc<Mutex<Vec<u8>>>, data: &[u8]) {
    const TAIL: usize = 50 * 1024;
    let Ok(mut guard) = tail.lock() else {
        return;
    };
    guard.extend_from_slice(data);
    if guard.len() > TAIL {
        let excess = guard.len() - TAIL;
        guard.drain(..excess);
    }
}

/// 取共享缓冲的快照；锁中毒时给空值，崩溃路径上不 panic。
fn lock_copy(tail: &Arc<Mutex<Vec<u8>>>) -> Vec<u8> {
    tail.lock().map(|guard| guard.clone()).unwrap_or_default()
}

/// 只保留最后 50 KB，用于 stderr。
fn read_tail(mut source: impl Read, tail: &Arc<Mutex<Vec<u8>>>) {
    let mut buffer = [0u8; 8192];
    loop {
        match source.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => push_tail(tail, &buffer[..read]),
            Err(_) => break,
        }
    }
}

fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tools(root: &std::path::Path) -> Tools {
        Tools::new(
            root.to_path_buf(),
            root.join("logs"),
            "gopls".to_string(),
            "phpantom_lsp".to_string(),
            "rust-analyzer".to_string(),
        )
    }

    fn bash_call(command: &str) -> ToolCall {
        ToolCall {
            id: "c1".to_string(),
            name: "bash".to_string(),
            arguments: json!({ "command": command }).to_string(),
        }
    }

    #[test]
    fn a_search_with_no_match_says_so_instead_of_an_exit_code() {
        let root = std::env::temp_dir().join("yi-no-match");
        std::fs::create_dir_all(&root).expect("建目录");
        std::fs::write(root.join("a.txt"), "one\ntwo\n").expect("写文件");
        let mut tools = tools(&root);

        // grep 的 1 是「没找到」，换个说法，别让上层当成失败。
        let out = tools.bash(&json!({ "command": "grep -n zzz a.txt" }), &mut |_| {});
        assert!(out.contains(NO_MATCH), "{out}");

        // 文件不存在是 exit 2，仍然算真失败。
        let out = tools.bash(&json!({ "command": "grep -n zzz nope.txt" }), &mut |_| {});
        assert!(out.contains("退出码：2"), "{out}");

        // 不是搜索命令的退出码 1 保持原样。
        let out = tools.bash(&json!({ "command": "false" }), &mut |_| {});
        assert!(out.contains("退出码：1"), "{out}");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn describe_drops_a_redundant_cd_to_the_project_root() {
        let root = std::env::temp_dir().join("yi-strip-proj");
        let line = tools(&root).describe(&bash_call(&format!(
            "cd {} && go test ./...",
            root.display()
        )));
        assert_eq!(line, "go test ./...");
    }

    #[test]
    fn describe_keeps_a_cd_to_another_directory() {
        let root = std::env::temp_dir().join("yi-strip-proj");
        let line = tools(&root).describe(&bash_call("cd /tmp/elsewhere && ls"));
        assert_eq!(line, "cd /tmp/elsewhere && ls");
    }

    #[test]
    fn strip_is_exact_and_also_handles_quotes() {
        let root = std::env::temp_dir().join("yi-strip-proj");
        let tools = tools(&root);
        let plain = format!("cd {} && ls", root.display());
        assert_eq!(tools.strip_redundant_cd(&plain), "ls");
        let quoted = format!("cd \"{}\" && ls", root.display());
        assert_eq!(tools.strip_redundant_cd(&quoted), "ls");
        // 别的目录、或者只是长得像，都不能动。
        assert_eq!(tools.strip_redundant_cd("cd /tmp && ls"), "cd /tmp && ls");
    }

    #[test]
    fn a_background_process_does_not_hang_the_bash_tool() {
        let root = std::env::temp_dir().join("yi-bg-hang");
        std::fs::create_dir_all(&root).expect("建目录");
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut tools = tools(&root);
            let out = tools.bash(&json!({ "command": "sleep 30 & echo ok" }), &mut |_| {});
            let _ = tx.send(out);
        });
        match rx.recv_timeout(std::time::Duration::from_secs(3)) {
            Ok(out) => assert!(out.contains("ok"), "{out}"),
            Err(_) => panic!("后台进程占着管道，bash 工具卡住了"),
        }
    }

    #[test]
    fn execution_drops_the_same_prefix() {
        let root = std::env::temp_dir().join("yi-strip-exec");
        std::fs::create_dir_all(&root).expect("建目录");
        let mut tools = tools(&root);
        let out = tools.bash(
            &json!({
                "command": format!("cd {} && echo 还在项目根", root.display()),
            }),
            &mut |_| {},
        );
        assert!(out.contains("还在项目根"), "{out}");
    }
}
