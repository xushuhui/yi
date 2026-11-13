use std::collections::HashSet;
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
    /// 这一轮最后一次请求的输入 token 数，也就是「上下文此刻有多大」。
    /// 其余字段都是整轮累加，只有这个不是。
    #[serde(default)]
    pub context_prompt: u64,
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
    /// 用户敲进终端的原文（技能展开前的样子），只用来做上键历史。
    Prompt {
        seq: u64,
        timestamp: u64,
        text: String,
    },
    /// 运行时事件（失败原因、请求次数上限）：只给人看，不进入模型上下文。
    Notice {
        seq: u64,
        timestamp: u64,
        text: String,
    },
}

impl Entry {
    fn seq(&self) -> u64 {
        match self {
            Entry::Message { seq, .. }
            | Entry::EffortChange { seq, .. }
            | Entry::Compaction { seq, .. }
            | Entry::Prompt { seq, .. }
            | Entry::Notice { seq, .. } => *seq,
        }
    }
}

/// 上键能翻多少条历史。
pub const HISTORY_LIMIT: usize = 200;

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

    /// 会话标识：第一次打开时生成，之后固定。发给供应商做路由与缓存亲和。
    pub fn id(&self) -> &str {
        &self.header.id
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

    /// 重建发给模型的对话：最近一次压缩的摘要 + 它保留的消息。
    /// 返回借用，避免每次工具步都克隆整段历史。
    pub fn context(&self) -> Context<'_> {
        let mut summary = None;
        let mut boundary = 0u64;
        if let Some(Entry::Compaction {
            summary: text,
            first_kept_seq,
            ..
        }) = self
            .entries
            .iter()
            .rev()
            .find(|entry| matches!(entry, Entry::Compaction { .. }))
        {
            summary = Some(text.as_str());
            boundary = *first_kept_seq;
        }
        let messages: Vec<&Message> = self
            .entries
            .iter()
            .filter(|entry| entry.seq() > boundary)
            .filter_map(|entry| match entry {
                Entry::Message { message, .. } => Some(message),
                _ => None,
            })
            .collect();

        // 供应商要求：带 tool_calls 的助手消息后面必须跟着每个 id 的工具结果。
        // 中断留下的半截消息（以及它那几条孤立的工具结果）整条丢掉，
        // 否则下一次请求会直接 400。
        let mut cleaned = Vec::with_capacity(messages.len());
        let mut index = 0;
        while index < messages.len() {
            if let Message::Assistant { tool_calls, .. } = messages[index]
                && !tool_calls.is_empty()
            {
                let mut answered: HashSet<&str> = HashSet::new();
                let mut look = index + 1;
                while look < messages.len() {
                    if let Message::Tool { tool_call_id, .. } = messages[look] {
                        answered.insert(tool_call_id.as_str());
                        look += 1;
                    } else {
                        break;
                    }
                }
                if tool_calls
                    .iter()
                    .all(|call| answered.contains(call.id.as_str()))
                {
                    cleaned.push(messages[index]);
                } else {
                    index = look;
                    continue;
                }
                index += 1;
                continue;
            }
            cleaned.push(messages[index]);
            index += 1;
        }
        Context {
            summary,
            messages: cleaned,
        }
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

    /// 记下用户敲的原文：技能展开后原文就没了，历史要靠它。
    pub fn append_prompt(&mut self, text: &str) -> Result<()> {
        self.append(Entry::Prompt {
            seq: self.next_seq,
            timestamp: now_ms(),
            text: text.to_string(),
        })
    }

    /// 最近 [`HISTORY_LIMIT`] 条用户原文，按时间顺序返回；连续重复只留一条。
    /// 老会话没有 `prompt` 条目，退回用用户消息本身；`/skill:` 展开过的消息
    /// 以 `[技能 ` 开头，回填时跳过——那不是用户敲的原话。
    pub fn prompts(&self) -> Vec<String> {
        let mut kept: Vec<String> = Vec::new();
        let mut has_prompts = false;
        for entry in &self.entries {
            match entry {
                Entry::Prompt { text, .. } => {
                    has_prompts = true;
                    if kept.last() != Some(text) {
                        kept.push(text.clone());
                    }
                }
                // 回填只发生在老会话：一旦本会话写过 `prompt`，就不再猜。
                // `/skill:` 展开过的消息以 `[技能 ` 开头，那不是用户敲的原话。
                Entry::Message {
                    message: Message::User { content },
                    ..
                } if !has_prompts
                    && !content.starts_with("[技能 ")
                    && kept.last() != Some(content) =>
                {
                    kept.push(content.clone());
                }
                _ => {}
            }
        }
        let start = kept.len().saturating_sub(HISTORY_LIMIT);
        kept.split_off(start)
    }

    /// 记一条只给人看的事件；不参与上下文重建，压缩也不会把它带走。
    pub fn append_notice(&mut self, text: &str) -> Result<()> {
        self.append(Entry::Notice {
            seq: self.next_seq,
            timestamp: now_ms(),
            text: text.to_string(),
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
        // 让崩溃钩子知道下一条该用哪个序号。
        crate::crash::observe(self.next_seq - 1);
        Ok(())
    }

    /// 修掉「有 tool_calls、却没有对应工具结果」的尾巴。
    /// 中断或崩溃会留下这种半截记录，原样发给供应商会被 400 拒绝。
    pub fn repair_interrupted(&mut self) -> Result<()> {
        // 只修尾巴：最后一条带 tool_calls 的助手消息后面如果直接是工具结果，
        // 缺的那几条可以在末尾补上（顺序仍然合法）。中途断开的交给 context() 丢弃。
        let mut answered: HashSet<String> = HashSet::new();
        let mut missing: Vec<String> = Vec::new();
        for entry in self.entries.iter().rev() {
            let Entry::Message { message, .. } = entry else {
                continue;
            };
            match message {
                Message::Tool { tool_call_id, .. } => {
                    answered.insert(tool_call_id.clone());
                }
                Message::Assistant { tool_calls, .. } if !tool_calls.is_empty() => {
                    missing = tool_calls
                        .iter()
                        .filter(|call| !answered.contains(&call.id))
                        .map(|call| call.id.clone())
                        .collect();
                    break;
                }
                _ => break,
            }
        }
        for id in missing {
            self.append_tool_result(
                &id,
                "ERROR: 上一轮被中断，这个工具调用没有执行。".to_string(),
            )?;
        }
        Ok(())
    }

    /// 当前最后一条记录的序号，交给崩溃钩子接续。
    pub fn last_seq(&self) -> u64 {
        self.next_seq.saturating_sub(1)
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

/// 一次请求要用的上下文：可选的压缩摘要，加上保留的消息（借用）。
pub struct Context<'a> {
    pub summary: Option<&'a str>,
    pub messages: Vec<&'a Message>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_session(tag: &str) -> Session {
        let dir = std::env::temp_dir().join(format!("yi-session-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建目录");
        Session::open(&dir.join("session.jsonl"), "/tmp/proj", "m", "auto").expect("开会话")
    }

    fn call(id: &str) -> ToolCall {
        ToolCall {
            id: id.to_string(),
            name: "read".to_string(),
            arguments: "{}".to_string(),
        }
    }

    fn answered(ctx: &Context<'_>) -> Vec<String> {
        ctx.messages
            .iter()
            .filter_map(|message| match message {
                Message::Tool { tool_call_id, .. } => Some(tool_call_id.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn repair_appends_results_for_unanswered_calls() {
        let mut session = temp_session("repair");
        session
            .append_assistant(
                "".into(),
                None,
                vec![call("a"), call("b")],
                Usage::default(),
            )
            .expect("助手");
        session
            .append_tool_result("a", "ok".into())
            .expect("工具结果");

        session.repair_interrupted().expect("修复");

        assert_eq!(answered(&session.context()), vec!["a", "b"]);
    }

    #[test]
    fn repair_leaves_a_mid_history_gap_to_the_context_filter() {
        let mut session = temp_session("midway");
        session
            .append_assistant("".into(), None, vec![call("a")], Usage::default())
            .expect("助手");
        session.append_user("后来的一句").expect("用户");

        session.repair_interrupted().expect("修复");

        let ctx = session.context();
        assert_eq!(ctx.messages.len(), 1, "中途的缺口不补，交给过滤");
        assert!(matches!(ctx.messages[0], Message::User { .. }));
    }

    #[test]
    fn context_drops_an_assistant_turn_that_is_missing_tool_results() {
        let mut session = temp_session("drop");
        session.append_user("第一句").expect("用户");
        session
            .append_assistant(
                "".into(),
                None,
                vec![call("a"), call("b")],
                Usage::default(),
            )
            .expect("助手");
        session
            .append_tool_result("a", "ok".into())
            .expect("半截结果");
        session.append_user("第二句").expect("用户");

        let ctx = session.context();
        assert!(
            !ctx.messages
                .iter()
                .any(|message| matches!(message, Message::Assistant { .. })),
            "半截助手消息必须丢掉"
        );
        assert!(answered(&ctx).is_empty(), "孤立的工具结果也要丢掉");
        assert_eq!(ctx.messages.len(), 2, "只留两句用户消息");
    }
}
