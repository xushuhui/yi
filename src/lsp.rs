use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::{Error, Result};

const INIT_TIMEOUT: Duration = Duration::from_secs(15);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const IDLE_TIMEOUT: Duration = Duration::from_secs(600);
const REAP_INTERVAL: Duration = Duration::from_secs(60);
/// 一次结果里最多列这么多条，超出部分在末尾说明还有多少。
const DEFINITION_CAP: usize = 10;
const REFERENCE_CAP: usize = 50;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Language {
    Go,
    Php,
}

impl Language {
    fn id(self) -> &'static str {
        match self {
            Language::Go => "go",
            Language::Php => "php",
        }
    }

    fn command(self, gopls: &str, phpantom: &str) -> String {
        match self {
            Language::Go => gopls.to_string(),
            Language::Php => phpantom.to_string(),
        }
    }

    /// 判断项目根用的标记文件。
    fn marker(self) -> &'static str {
        match self {
            Language::Go => "go.mod",
            Language::Php => "composer.json",
        }
    }
}

type Key = (Language, PathBuf);

/// 按 (语言, 项目根) 缓存语言服务器进程：空闲 10 分钟由后台线程回收，进程退出时全部终止。
pub struct Lsp {
    clients: Arc<Mutex<HashMap<Key, Client>>>,
    gopls: String,
    phpantom: String,
}

impl Lsp {
    pub fn new(gopls: String, phpantom: String) -> Lsp {
        let clients: Arc<Mutex<HashMap<Key, Client>>> = Arc::new(Mutex::new(HashMap::new()));
        spawn_reaper(Arc::downgrade(&clients));
        Lsp {
            clients,
            gopls,
            phpantom,
        }
    }

    /// 三个动作的入口：先校验动作，再决定要不要启动服务器。
    pub fn call(
        &mut self,
        action: &str,
        path: &Path,
        line: Option<usize>,
        symbol: Option<&str>,
    ) -> Result<String> {
        if !matches!(action, "definition" | "references") {
            return Err(Error::Config(format!(
                "未知的 lsp 动作 {action}；可选 definition / references"
            )));
        }
        let language = language_of(path)?;
        let root = project_root(path, language);

        // 锁中毒只发生在别处 panic 时；取回数据继续用，总比整轮失败好。
        let mut clients = self.clients.lock().unwrap_or_else(|e| e.into_inner());
        let key = (language, root.clone());
        let needs_spawn = match clients.get_mut(&key) {
            // 服务器自己崩了就重起，否则后面每一步都会报超时。
            Some(client) => client.child.try_wait().ok().flatten().is_some(),
            None => true,
        };
        if needs_spawn {
            let command = language.command(&self.gopls, &self.phpantom);
            let client = Client::spawn(&command, &root, language)?;
            clients.insert(key.clone(), client);
        }
        let client = clients
            .get_mut(&key)
            .ok_or_else(|| Error::Config("语言服务器初始化失败".to_string()))?;
        client.last_used = Instant::now();

        let line = require_line(action, line)?;
        let target = if action == "definition" {
            Target::Definition
        } else {
            Target::References
        };
        client.goto(target, path, line, symbol)
    }
}

fn require_line(action: &str, line: Option<usize>) -> Result<usize> {
    line.ok_or_else(|| Error::Config(format!("{action} 需要 line（行号，从 1 开始）")))
}

/// 后台每 60 秒清一次空闲服务器；`Lsp` 被释放后线程自然退出。
fn spawn_reaper(clients: Weak<Mutex<HashMap<Key, Client>>>) {
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(REAP_INTERVAL);
            let Some(clients) = clients.upgrade() else {
                return;
            };
            let mut guard = clients.lock().unwrap_or_else(|e| e.into_inner());
            guard.retain(|_, client| client.last_used.elapsed() < IDLE_TIMEOUT);
        }
    });
}

#[derive(Clone, Copy)]
enum Target {
    Definition,
    References,
}

impl Target {
    fn method(self) -> &'static str {
        match self {
            Target::Definition => "textDocument/definition",
            Target::References => "textDocument/references",
        }
    }

    fn empty(self) -> &'static str {
        match self {
            Target::Definition => "没有找到定义",
            Target::References => "没有找到引用",
        }
    }

    fn cap(self) -> usize {
        match self {
            Target::Definition => DEFINITION_CAP,
            Target::References => REFERENCE_CAP,
        }
    }
}

struct Client {
    child: Child,
    stdin: ChildStdin,
    rx: Receiver<Value>,
    next_id: u64,
    /// 已经 didOpen 过的文件 → 版本号。
    opened: HashMap<PathBuf, i64>,
    last_used: Instant,
}

impl Drop for Client {
    fn drop(&mut self) {
        // 进程可能已经自己退出，这里的失败没有补救动作。
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Client {
    fn spawn(command: &str, root: &Path, language: Language) -> Result<Client> {
        let mut child = Command::new(command)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .current_dir(root)
            .spawn()
            .map_err(|err| Error::Config(format!("起不了语言服务器 {command}：{err}")))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| Error::Config("拿不到语言服务器的输入".to_string()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| Error::Config("拿不到语言服务器的输出".to_string()))?;

        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || read_loop(stdout, tx));

        let mut client = Client {
            child,
            stdin,
            rx,
            next_id: 1,
            opened: HashMap::new(),
            last_used: Instant::now(),
        };
        client.initialize(root, language)?;
        Ok(client)
    }

    fn initialize(&mut self, root: &Path, _language: Language) -> Result<()> {
        self.request(
            "initialize",
            json!({
                "processId": null,
                "rootUri": file_uri(root),
                "capabilities": {},
            }),
            INIT_TIMEOUT,
        )?;
        self.notify("initialized", json!({}))
    }

    fn goto(
        &mut self,
        target: Target,
        path: &Path,
        line: usize,
        symbol: Option<&str>,
    ) -> Result<String> {
        let (line_index, character) = position_in(path, line, symbol)?;
        self.ensure_open(path)?;
        let uri = file_uri(path);
        let mut params = json!({
            "textDocument": { "uri": uri },
            "position": { "line": line_index, "character": character },
        });
        if matches!(target, Target::References) {
            params["context"] = json!({ "includeDeclaration": true });
        }

        let result = self.request(target.method(), params, REQUEST_TIMEOUT)?;
        let locations = collect_locations(&result);
        if locations.is_empty() {
            return Ok(target.empty().to_string());
        }

        let total = locations.len();
        let cap = target.cap();
        let mut out = String::new();
        if total > cap {
            out.push_str(&format!("共 {total} 处，先列前 {cap} 处：\n"));
        } else if matches!(target, Target::References) {
            out.push_str(&format!("共 {total} 处引用：\n"));
        }
        for (file, line, column) in locations.iter().take(cap) {
            out.push_str(&format_location(file, *line, *column));
        }
        if total > cap {
            out.push_str(&format!("… 还有 {} 处未显示\n", total - cap));
        }
        Ok(out)
    }

    fn ensure_open(&mut self, path: &Path) -> Result<()> {
        let text = std::fs::read_to_string(path)
            .map_err(|err| Error::Config(format!("读不了 {}：{err}", path.display())))?;
        let uri = file_uri(path);

        match self.opened.get(path).copied() {
            Some(version) => {
                let next = version + 1;
                self.opened.insert(path.to_path_buf(), next);
                self.notify(
                    "textDocument/didChange",
                    json!({
                        "textDocument": { "uri": uri, "version": next },
                        "contentChanges": [{ "text": text }],
                    }),
                )
            }
            None => {
                self.opened.insert(path.to_path_buf(), 1);
                let language = language_of(path)?;
                self.notify(
                    "textDocument/didOpen",
                    json!({
                        "textDocument": {
                            "uri": uri,
                            "languageId": language.id(),
                            "version": 1,
                            "text": text,
                        },
                    }),
                )
            }
        }
    }

    fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        write_message(
            &mut self.stdin,
            &json!({ "jsonrpc": "2.0", "method": method, "params": params }),
        )
    }

    fn request(&mut self, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        write_message(
            &mut self.stdin,
            &json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }),
        )?;

        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(Error::Config(format!("语言服务器超时：{method}")));
            }
            match self.rx.recv_timeout(remaining) {
                Ok(message) => {
                    if message.get("id").and_then(Value::as_u64) == Some(id) {
                        if let Some(error) = message.get("error") {
                            return Err(Error::Config(format!("语言服务器报错：{error}")));
                        }
                        return Ok(message.get("result").cloned().unwrap_or(Value::Null));
                    }
                    // 服务器推来的通知（诊断、日志等）用不到，丢弃。
                }
                Err(_) => return Err(Error::Config(format!("语言服务器超时：{method}"))),
            }
        }
    }
}

fn read_loop(stdout: impl Read, tx: mpsc::Sender<Value>) {
    let mut reader = BufReader::new(stdout);
    loop {
        let mut length = 0usize;
        let mut line = String::new();
        loop {
            line.clear();
            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                return;
            }
            let head = line.trim_end();
            if head.is_empty() {
                break;
            }
            let lower = head.to_ascii_lowercase();
            if let Some(value) = lower.strip_prefix("content-length:") {
                length = value.trim().parse().unwrap_or(0);
            }
        }
        if length == 0 {
            continue;
        }
        let mut body = vec![0u8; length];
        if reader.read_exact(&mut body).is_err() {
            return;
        }
        match serde_json::from_slice::<Value>(&body) {
            Ok(message) => {
                if tx.send(message).is_err() {
                    return;
                }
            }
            // 不是合法 JSON 的消息直接跳过：服务器还会继续发，不能因此断掉整条通道。
            Err(_) => continue,
        }
    }
}

fn write_message(stdin: &mut ChildStdin, message: &Value) -> Result<()> {
    let body = serde_json::to_vec(message)?;
    stdin.write_all(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes())?;
    stdin.write_all(&body)?;
    stdin.flush()?;
    Ok(())
}

pub fn language_of(path: &Path) -> Result<Language> {
    match path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "go" => Ok(Language::Go),
        "php" => Ok(Language::Php),
        other => Err(Error::Config(format!(
            "LSP 只支持 .go 和 .php，这个文件是 .{other}"
        ))),
    }
}

fn project_root(path: &Path, language: Language) -> PathBuf {
    let marker = language.marker();
    let start = if path.is_dir() {
        path.to_path_buf()
    } else {
        path.parent().map(Path::to_path_buf).unwrap_or_default()
    };
    for dir in start.ancestors() {
        if dir.join(marker).exists() {
            return dir.to_path_buf();
        }
    }
    std::env::current_dir().unwrap_or(start)
}

/// LSP 的 URI 必须是百分号编码的；只转义空格会让中文路径彻底失效。
fn file_uri(path: &Path) -> String {
    let mut out = String::from("file://");
    for byte in path.to_string_lossy().as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                out.push(*byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn uri_to_path(uri: &str) -> PathBuf {
    let raw = uri.trim_start_matches("file://");
    let bytes = raw.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && index + 2 < bytes.len()
            && let Ok(value) = u8::from_str_radix(&raw[index + 1..index + 3], 16)
        {
            decoded.push(value);
            index += 3;
            continue;
        }
        decoded.push(bytes[index]);
        index += 1;
    }
    PathBuf::from(String::from_utf8_lossy(&decoded).to_string())
}

fn position_in(path: &Path, line: usize, symbol: Option<&str>) -> Result<(u64, u64)> {
    if line == 0 {
        return Err(Error::Config("line 从 1 开始".to_string()));
    }
    let text = std::fs::read_to_string(path)
        .map_err(|err| Error::Config(format!("读不了 {}：{err}", path.display())))?;
    let Some(content) = text.lines().nth(line - 1) else {
        return Err(Error::Config(format!(
            "{} 只有 {} 行，找不到第 {line} 行",
            path.display(),
            text.lines().count()
        )));
    };
    let byte_index = match symbol {
        Some(symbol) => content
            .find(symbol)
            .ok_or_else(|| Error::Config(format!("第 {line} 行找不到 {symbol}")))?,
        None => content.len() - content.trim_start().len(),
    };
    // LSP 的列号按 UTF-16 计算，不是字节。
    let character: usize = content[..byte_index].chars().map(char::len_utf16).sum();
    Ok(((line - 1) as u64, character as u64))
}

fn collect_locations(result: &Value) -> Vec<(PathBuf, u64, u64)> {
    let mut out = Vec::new();
    let items: Vec<&Value> = match result {
        Value::Array(items) => items.iter().collect(),
        Value::Object(_) => vec![result],
        _ => Vec::new(),
    };
    for item in items {
        let Some(uri) = item.get("uri").and_then(Value::as_str) else {
            continue;
        };
        let line = item["range"]["start"]["line"].as_u64().unwrap_or(0) + 1;
        let character = item["range"]["start"]["character"].as_u64().unwrap_or(0) + 1;
        out.push((uri_to_path(uri), line, character));
    }
    out
}

fn format_location(path: &PathBuf, line: u64, column: u64) -> String {
    let snippet = std::fs::read_to_string(path)
        .ok()
        .and_then(|text| text.lines().nth(line as usize - 1).map(str::to_string))
        .unwrap_or_default();
    let mut snippet = snippet.trim().to_string();
    if snippet.chars().count() > 160 {
        snippet = snippet.chars().take(160).collect::<String>() + "…";
    }
    format!("{}:{}:{}  {}\n", path.display(), line, column, snippet)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_round_trips_non_ascii_and_spaces() {
        let path = PathBuf::from("/tmp/中文 目录/main.go");
        let uri = file_uri(&path);
        assert!(uri.starts_with("file:///tmp/%E4%B8%AD%E6%96%87%20"));
        assert_eq!(uri_to_path(&uri), path);
    }
}
