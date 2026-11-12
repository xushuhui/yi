use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::{Error, Result};

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Usage {
    /// 未命中缓存、按原价计费的输入 token。
    #[serde(default)]
    pub input_uncached: u64,
    /// 命中前缀缓存、按五十分之一计费的输入 token。
    #[serde(default)]
    pub cache_hit: u64,
    #[serde(default)]
    pub output: u64,
    #[serde(default)]
    pub reasoning: u64,
}

impl Usage {
    pub fn prompt_tokens(&self) -> u64 {
        self.input_uncached + self.cache_hit
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "role", rename_all = "lowercase")]
pub enum Message {
    User {
        content: String,
    },
    Assistant {
        content: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reasoning: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<Usage>,
    },
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Header {
    #[serde(rename = "type")]
    pub kind: String,
    pub version: u32,
    pub id: String,
    pub timestamp: u64,
    pub cwd: String,
    pub model: String,
    pub effort: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Entry {
    Message {
        seq: u64,
        timestamp: u64,
        message: Message,
    },
    EffortChange {
        seq: u64,
        timestamp: u64,
        effort: String,
    },
    ModelChange {
        seq: u64,
        timestamp: u64,
        model: String,
    },
}

impl Entry {
    fn seq(&self) -> u64 {
        match self {
            Entry::Message { seq, .. }
            | Entry::EffortChange { seq, .. }
            | Entry::ModelChange { seq, .. } => *seq,
        }
    }
}

/// 一条会话 = 一个追加写的 JSONL 文件 + 进程持有的排他锁。
pub struct Session {
    path: PathBuf,
    file: File,
    header: Header,
    entries: Vec<Entry>,
    next_seq: u64,
}

impl Session {
    pub fn open(path: &Path, cwd: &str, model: &str, effort: &str) -> Result<Session> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let file = OpenOptions::new()
            .read(true)
            .append(true)
            .create(true)
            .open(path)?;
        file.try_lock().map_err(|e| {
            Error::Config(format!(
                "这个项目已经有 yi 在运行（{e}）。会话文件：{}",
                path.display()
            ))
        })?;

        let text = std::fs::read_to_string(path)?;
        let mut lines = text.lines().filter(|line| !line.trim().is_empty());

        let Some(first) = lines.next() else {
            let header = Header {
                kind: "session".to_string(),
                version: 1,
                id: short_id(),
                timestamp: now_ms(),
                cwd: cwd.to_string(),
                model: model.to_string(),
                effort: effort.to_string(),
            };
            let mut session = Session {
                path: path.to_path_buf(),
                file,
                header: header.clone(),
                entries: Vec::new(),
                next_seq: 1,
            };
            session.write_json(&header)?;
            return Ok(session);
        };

        let header: Header = serde_json::from_str(first)?;
        let mut entries = Vec::new();
        for line in lines {
            entries.push(serde_json::from_str::<Entry>(line)?);
        }
        let next_seq = entries.iter().map(Entry::seq).max().unwrap_or(0) + 1;
        Ok(Session {
            path: path.to_path_buf(),
            file,
            header,
            entries,
            next_seq,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn header(&self) -> &Header {
        &self.header
    }

    /// 重建发给模型的对话：目前是全部消息，压缩接入后这里会变。
    pub fn messages(&self) -> Vec<Message> {
        self.entries
            .iter()
            .filter_map(|entry| match entry {
                Entry::Message { message, .. } => Some(message.clone()),
                _ => None,
            })
            .collect()
    }

    pub fn append_user(&mut self, content: &str) -> Result<()> {
        self.append(Entry::Message {
            seq: self.next_seq,
            timestamp: now_ms(),
            message: Message::User {
                content: content.to_string(),
            },
        })
    }

    pub fn append_assistant(
        &mut self,
        content: String,
        reasoning: Option<String>,
        usage: Usage,
    ) -> Result<()> {
        self.append(Entry::Message {
            seq: self.next_seq,
            timestamp: now_ms(),
            message: Message::Assistant {
                content,
                reasoning,
                usage: Some(usage),
            },
        })
    }

    pub fn append_effort(&mut self, effort: &str) -> Result<()> {
        self.append(Entry::EffortChange {
            seq: self.next_seq,
            timestamp: now_ms(),
            effort: effort.to_string(),
        })
    }

    /// `/new`：把当前文件改名归档，然后从空白开始；不删除任何东西。
    pub fn archive(&mut self) -> Result<PathBuf> {
        let archived = self
            .path
            .with_file_name(format!("session-{}.jsonl", now_ms() / 1000));
        std::fs::rename(&self.path, &archived)?;
        let file = OpenOptions::new()
            .read(true)
            .append(true)
            .create(true)
            .open(&self.path)?;
        file.try_lock()
            .map_err(|e| Error::Config(format!("无法重新打开会话文件（{e}）")))?;
        self.file = file;
        self.header.id = short_id();
        self.header.timestamp = now_ms();
        self.entries.clear();
        self.next_seq = 1;
        let header = self.header.clone();
        self.write_json(&header)?;
        Ok(archived)
    }

    fn append(&mut self, entry: Entry) -> Result<()> {
        self.next_seq += 1;
        self.write_json(&entry)?;
        self.entries.push(entry);
        Ok(())
    }

    fn write_json<T: Serialize>(&mut self, value: &T) -> Result<()> {
        let mut line = serde_json::to_string(value)?;
        line.push('\n');
        self.file.write_all(line.as_bytes())?;
        self.file.flush()?;
        Ok(())
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

/// 8 位会话 id：毫秒时间戳的低 32 位 + 进程号低 16 位，够区分同机会话。
fn short_id() -> String {
    format!(
        "{:08x}{:04x}",
        (now_ms() & 0xffff_ffff) as u32,
        (std::process::id() & 0xffff) as u16
    )
}
