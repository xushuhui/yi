//! 崩溃记录：进程 panic 时，把原因和位置写进当前会话文件。
//!
//! 会话文件是事后唯一能查的地方。panic 会直接杀掉进程，此时只有这里
//! 还来得及留下一条 `notice`，其余状态（本轮的工具结果、用户输入）已经落盘。

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static SESSION: OnceLock<PathBuf> = OnceLock::new();
static LAST_SEQ: AtomicU64 = AtomicU64::new(0);

/// 装上 panic 钩子。必须在打开会话之前调用，路径还没设定时钩子什么也不做。
pub fn install_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let location = info
            .location()
            .map(|here| format!("{}:{}:{}", here.file(), here.line(), here.column()))
            .unwrap_or_else(|| "未知位置".to_string());
        record(&format!(
            "yi 崩溃了（{location}）：{}",
            payload(info.payload())
        ));
        previous(info);
    }));
}

/// 记住会话文件路径，以及当前最后一条记录的序号。
pub fn install(path: &Path, last_seq: u64) {
    let _ = SESSION.set(path.to_path_buf());
    LAST_SEQ.store(last_seq, Ordering::Relaxed);
}

/// 每写一条会话记录就同步一次序号，钩子才能接在后面写。
pub fn observe(seq: u64) {
    LAST_SEQ.store(seq, Ordering::Relaxed);
}

/// 追加一条 `notice`。写不进去也没别的办法，调用方都在崩溃路径上。
pub fn record(text: &str) {
    let Some(path) = SESSION.get() else {
        return;
    };
    let seq = LAST_SEQ.fetch_add(1, Ordering::Relaxed) + 1;
    let entry = serde_json::json!({
        "type": "notice",
        "seq": seq,
        "timestamp": now_ms(),
        "text": text,
    });
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(path)
    {
        let _ = writeln!(file, "{entry}");
    }
}

fn payload(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(text) = payload.downcast_ref::<&str>() {
        return (*text).to_string();
    }
    if let Some(text) = payload.downcast_ref::<String>() {
        return text.clone();
    }
    "未知原因".to_string()
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_recorded_crash_lands_in_the_session_file_as_a_notice() {
        let dir = std::env::temp_dir().join("yi-crash-test");
        std::fs::create_dir_all(&dir).expect("建目录");
        let path = dir.join("session.jsonl");
        std::fs::write(&path, "").expect("建文件");
        install(&path, 7);

        record("yi 崩溃了（src/tools.rs:197:25）：assertion failed");

        let text = std::fs::read_to_string(&path).expect("读文件");
        let entry: serde_json::Value = serde_json::from_str(text.trim()).expect("是合法 JSON");
        assert_eq!(entry["type"], "notice");
        assert_eq!(entry["seq"], 8, "接在已有记录后面");
        assert!(
            entry["text"]
                .as_str()
                .unwrap_or("")
                .contains("src/tools.rs:197:25"),
            "{entry}"
        );
    }
}
