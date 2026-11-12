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
    /// 这一轮从发请求到收完流的毫秒数。
    #[serde(default)]
    pub elapsed_ms: u64,
}

impl Usage {
    pub fn prompt_tokens(&self) -> u64 {
        self.input_uncached + self.cache_hit
    }
}

/// 模型请求调用某个工具时的一条记录；`arguments` 保留原始 JSON 字符串。
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
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
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tool_calls: Vec<ToolCall>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<Usage>,
    },
    /// 工具执行结果，`tool_call_id` 对应上面那次调用。
    Tool {
        tool_call_id: String,
        content: String,
    },
    /// 作为输入附给模型的图片，只存路径，请求时再读出来编码。
    Image {
        path: String,
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
    Compaction {
        seq: u64,
        timestamp: u64,
        summary: String,
        #[serde(rename = "firstKeptSeq")]
        first_kept_seq: u64,
        #[serde(rename = "tokensBefore")]
        tokens_before: u64,
    },
}

impl Entry {
    fn seq(&self) -> u64 {
        match self {
            Entry::Message { seq, .. }
            | Entry::EffortChange { seq, .. }
            | Entry::Compaction { seq, .. } => *seq,
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

    /// 最近一次生效的推理强度；`None` 表示 auto。
    pub fn current_effort(&self) -> Option<String> {
        let mut effort = match self.header.effort.as_str() {
            "auto" | "" => None,
            other => Some(other.to_string()),
        };
        for entry in &self.entries {
            if let Entry::EffortChange { effort: value, .. } = entry {
                effort = match value.as_str() {
                    "auto" | "" => None,
                    other => Some(other.to_string()),
                };
            }
        }
        effort
    }

    /// 重建发给模型的对话：最近一次压缩的摘要放在最前，其后只带它保留的消息。
    pub fn messages(&self) -> Vec<Message> {
        let mut out = Vec::new();
        let mut boundary = 0u64;
        if let Some(Entry::Compaction {
            summary,
            first_kept_seq,
            ..
        }) = self
            .entries
            .iter()
            .rev()
            .find(|entry| matches!(entry, Entry::Compaction { .. }))
        {
            out.push(Message::User {
                content: format!("<compaction-summary>\n{summary}\n</compaction-summary>"),
            });
            boundary = *first_kept_seq;
        }
        for entry in &self.entries {
            if entry.seq() > boundary
                && let Entry::Message { message, .. } = entry
            {
                out.push(message.clone());
            }
        }
        out
    }

    /// 规划一次压缩：算出要摘要的消息、保留起点、压缩前的大致用量。
    /// 返回 `None` 表示没什么可压缩的（消息太少，或找不到可切的 user 边界）。
    pub fn plan_compaction(&self, keep_tokens: u64) -> Option<Plan> {
        let previous = self.entries.iter().rev().find_map(|entry| match entry {
            Entry::Compaction {
                summary,
                first_kept_seq,
                ..
            } => Some((summary.clone(), *first_kept_seq)),
            _ => None,
        });
        let boundary = previous.as_ref().map(|(_, seq)| *seq).unwrap_or(0);

        let messages: Vec<&Entry> = self
            .entries
            .iter()
            .filter(|entry| entry.seq() > boundary && matches!(entry, Entry::Message { .. }))
            .collect();
        if messages.len() < 4 {
            return None;
        }

        let mut kept = 0u64;
        let mut cut = messages.len();
        for (index, entry) in messages.iter().enumerate().rev() {
            if let Entry::Message { message, .. } = entry {
                kept += estimate_message(message);
            }
            cut = index;
            if kept >= keep_tokens {
                break;
            }
        }
        // 切点必须是 user 消息，才不会把一次工具调用和它的结果拆开。
        while cut > 0 && !is_user(messages[cut]) {
            cut -= 1;
        }
        if cut == 0 {
            // 退到最前面就没有可摘要的内容了；改为往后找最近的下一个 user 边界，
            // 这样保留的原文少于目标值，但仍然合法。
            cut = messages
                .iter()
                .enumerate()
                .find(|(index, entry)| *index > 0 && is_user(entry))
                .map(|(index, _)| index)
                .unwrap_or(0);
        }
        if cut == 0 || !is_user(messages[cut]) {
            return None;
        }

        let first_kept_seq = messages[cut].seq();
        let to_summarize: Vec<Message> = messages[..cut]
            .iter()
            .filter_map(|entry| match entry {
                Entry::Message { message, .. } => Some(message.clone()),
                _ => None,
            })
            .collect();
        let tokens_before = messages
            .iter()
            .filter_map(|entry| match entry {
                Entry::Message { message, .. } => Some(estimate_message(message)),
                _ => None,
            })
            .sum();

        Some(Plan {
            messages_to_summarize: to_summarize,
            first_kept_seq,
            tokens_before,
            previous_summary: previous.map(|(summary, _)| summary),
        })
    }

    pub fn append_compaction(
        &mut self,
        summary: String,
        first_kept_seq: u64,
        tokens_before: u64,
    ) -> Result<()> {
        self.append(Entry::Compaction {
            seq: self.next_seq,
            timestamp: now_ms(),
            summary,
            first_kept_seq,
            tokens_before,
        })
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
        tool_calls: Vec<ToolCall>,
        usage: Usage,
    ) -> Result<()> {
        self.append(Entry::Message {
            seq: self.next_seq,
            timestamp: now_ms(),
            message: Message::Assistant {
                content,
                reasoning,
                tool_calls,
                usage: Some(usage),
            },
        })
    }

    pub fn append_image(&mut self, path: &std::path::Path) -> Result<()> {
        self.append(Entry::Message {
            seq: self.next_seq,
            timestamp: now_ms(),
            message: Message::Image {
                path: path.display().to_string(),
            },
        })
    }

    pub fn append_tool_result(&mut self, tool_call_id: &str, content: String) -> Result<()> {
        self.append(Entry::Message {
            seq: self.next_seq,
            timestamp: now_ms(),
            message: Message::Tool {
                tool_call_id: tool_call_id.to_string(),
                content,
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

fn is_user(entry: &Entry) -> bool {
    matches!(
        entry,
        Entry::Message {
            message: Message::User { .. },
            ..
        }
    )
}

/// 一次压缩的计划。
pub struct Plan {
    pub messages_to_summarize: Vec<Message>,
    pub first_kept_seq: u64,
    pub tokens_before: u64,
    pub previous_summary: Option<String>,
}

/// 粗估一条消息的 token 数：UTF-8 字节数除以 3，再加固定开销。
/// 只用来决定切点，阈值判断用供应商返回的真实用量。
pub fn estimate_message(message: &Message) -> u64 {
    let bytes = match message {
        Message::User { content } => content.len(),
        Message::Assistant {
            content,
            reasoning,
            tool_calls,
            ..
        } => {
            content.len()
                + reasoning.as_deref().map(str::len).unwrap_or(0)
                + tool_calls
                    .iter()
                    .map(|call| call.arguments.len())
                    .sum::<usize>()
        }
        Message::Tool { content, .. } => content.len(),
        Message::Image { path } => path.len(),
    };
    (bytes as u64) / 3 + 4
}
