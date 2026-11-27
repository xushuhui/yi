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
    /// 计划模式开关：只追加，不进入模型上下文，重开时照最后一条恢复。
    PlanChange {
        seq: u64,
        timestamp: u64,
        planned: bool,
    },
    /// 中途换了供应商：从这条之后发出的请求用它，重开时也照它恢复。
    ModelChange {
        seq: u64,
        timestamp: u64,
        provider: String,
        model: String,
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
    /// 回退到一个分支点：`to_seq` 是目标条目的序号（0 表示空会话）。
    /// 它把「当前路径」切到旧分支上，文件本身不动。
    Rewind {
        seq: u64,
        timestamp: u64,
        #[serde(rename = "toSeq")]
        to_seq: u64,
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
            | Entry::ModelChange { seq, .. }
            | Entry::PlanChange { seq, .. }
            | Entry::Compaction { seq, .. }
            | Entry::Rewind { seq, .. }
            | Entry::Prompt { seq, .. }
            | Entry::Notice { seq, .. } => *seq,
        }
    }
}

/// 启动时重放的一行：用户敲的原文、回答正文，或一次工具调用。
#[derive(Clone, Debug, PartialEq)]
pub enum Replay {
    User(String),
    Assistant(String),
    Tool(ToolCall),
}

/// `/tree` 的一行：一条用户消息，以及它在树里的位置。
#[derive(Clone, Debug)]
pub struct TreeNode {
    /// 给人看的编号，从 1 开始；`/tree <ordinal>` 用它。
    pub ordinal: usize,
    pub seq: u64,
    pub timestamp: u64,
    /// 首行前 60 字；回退时预填的是原文，不是这个。
    pub preview: String,
    pub on_path: bool,
    /// 有两条以上子条目：从这里分出了岔路。
    pub branch_point: bool,
    /// 不能回退的原因（已压缩 / 跨压缩边界）。
    pub blocked: Option<&'static str>,
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

        // 先扫一遍找最后一条压缩的边界：第二遍读到旧条目时可以立刻丢正文，
        // 不必把整份会话（可能几十上百 MB）先读进内存。
        let boundary = scan_boundary(path)?;

        use std::io::BufRead;
        let file_reader = std::fs::File::open(path)?;
        let mut lines = std::io::BufReader::new(file_reader).lines();

        let mut first = None;
        for line in &mut lines {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            first = Some(line);
            break;
        }

        let Some(first) = first else {
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

        let header: Header = serde_json::from_str(&first)?;
        let mut entries = Vec::new();
        for line in lines {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let mut entry: Entry = serde_json::from_str(&line)?;
            trim_entry(&mut entry, boundary);
            entries.push(entry);
        }
        let next_seq = entries.iter().map(Entry::seq).max().unwrap_or(0) + 1;
        let mut session = Session {
            path: path.to_path_buf(),
            file,
            header,
            entries,
            next_seq,
        };
        session.trim_compacted();
        Ok(session)
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

    /// 计划模式是否打开；从没记过就是关。
    pub fn current_plan(&self) -> bool {
        self.entries
            .iter()
            .rev()
            .find_map(|entry| match entry {
                Entry::PlanChange { planned, .. } => Some(*planned),
                _ => None,
            })
            .unwrap_or(false)
    }

    /// 最近一次切换后的供应商与模型；从没切过就是 `None`，用配置文件。
    pub fn current_model(&self) -> Option<(String, String)> {
        self.entries.iter().rev().find_map(|entry| match entry {
            Entry::ModelChange {
                provider, model, ..
            } => Some((provider.clone(), model.clone())),
            _ => None,
        })
    }

    /// 重建发给模型的对话：最近一次压缩的摘要 + 它保留的消息。
    /// 返回借用，避免每次工具步都克隆整段历史。
    /// 最近 `turns` 轮的重放内容：用户敲的原文、回答正文、工具调用。
    ///
    /// 只取压缩边界之后的条目——边界之前的连模型都不记得了，重放出来只会
    /// 让人以为它还记得。
    pub fn replay(&self, turns: usize) -> Vec<Replay> {
        if turns == 0 {
            return Vec::new();
        }
        let boundary = self.boundary();

        let mut items = Vec::new();
        let mut typed = None;
        for index in self.active_path() {
            let entry = &self.entries[index];
            if entry.seq() <= boundary {
                continue;
            }
            match entry {
                Entry::Prompt { text, .. } => typed = Some(text.clone()),
                Entry::Message { message, .. } => match message {
                    Message::User { content } => {
                        // 后台命令的回灌不是人打的字，重放时跳过。
                        if content.starts_with("[后台作业 ") {
                            continue;
                        }
                        // 技能调用那一轮显示用户敲的原文，展开后的正文太长。
                        let text = typed.take().unwrap_or_else(|| content.clone());
                        items.push(Replay::User(text));
                    }
                    Message::Assistant {
                        content,
                        tool_calls,
                        ..
                    } => {
                        if !content.trim().is_empty() {
                            items.push(Replay::Assistant(content.clone()));
                        }
                        for call in tool_calls {
                            items.push(Replay::Tool(call.clone()));
                        }
                    }
                    Message::Tool { .. } | Message::Image { .. } => {}
                },
                Entry::EffortChange { .. }
                | Entry::ModelChange { .. }
                | Entry::PlanChange { .. }
                | Entry::Compaction { .. }
                | Entry::Rewind { .. }
                | Entry::Notice { .. } => {}
            }
        }

        // 从后往前数用户消息，只留最后 `turns` 轮。
        let mut seen = 0;
        let mut start = 0;
        for (index, item) in items.iter().enumerate().rev() {
            if matches!(item, Replay::User(_)) {
                seen += 1;
                if seen == turns {
                    start = index;
                    break;
                }
            }
        }
        items.split_off(start)
    }

    /// 会话被压缩过没有：重放时据此说明「早前对话已压缩」。
    pub fn compacted(&self) -> bool {
        self.entries
            .iter()
            .any(|entry| matches!(entry, Entry::Compaction { .. }))
    }

    /// 压缩之后丢掉边界之前的正文：历史仍留在文件里，但不再占内存。
    ///
    /// 用户消息和 prompt 原文保留——`/tree` 要用它们显示旧分支；
    /// assistant 的正文、推理、工具调用和工具结果都清空，它们本来就不在上下文里。
    fn trim_compacted(&mut self) {
        let boundary = self.boundary();
        if boundary == 0 {
            return;
        }
        for entry in &mut self.entries {
            trim_entry(entry, boundary);
        }
    }

    /// 最近一次压缩的保留起点；0 表示没压缩过。
    fn boundary(&self) -> u64 {
        self.entries
            .iter()
            .rev()
            .find_map(|entry| match entry {
                Entry::Compaction { first_kept_seq, .. } => Some(*first_kept_seq),
                _ => None,
            })
            .unwrap_or(0)
    }

    /// 一条记录的父是谁：`rewind` 指向它切过去的分支点，其余都是前一条。
    fn parent_seq(&self, index: usize) -> u64 {
        if let Entry::Rewind { to_seq, .. } = &self.entries[index] {
            return *to_seq;
        }
        if index == 0 {
            0
        } else {
            self.entries[index - 1].seq()
        }
    }

    /// 一条用户消息之前最近的那条用户消息；没有就回到 0。
    ///
    /// 中间夹着的 assistant、工具结果、prompt、rewind 都跳过——分支是从
    /// 「上一句话」分出来的，不是从某条工具结果分出来的。
    fn previous_user(&self, index: usize) -> u64 {
        let mut parent = self.parent_seq(index);
        let mut guard = 0;
        while parent != 0 {
            let Some(parent_index) = self.index_of(parent) else {
                break;
            };
            match &self.entries[parent_index] {
                Entry::Message {
                    seq,
                    message: Message::User { .. },
                    ..
                } => return *seq,
                _ => parent = self.parent_seq(parent_index),
            }
            guard += 1;
            if guard > self.entries.len() {
                break;
            }
        }
        0
    }

    fn index_of(&self, seq: u64) -> Option<usize> {
        self.entries
            .binary_search_by_key(&seq, |entry| entry.seq())
            .ok()
    }

    /// 当前路径：从最后一条记录沿父链走到根，返回 root→leaf 的下标。
    ///
    /// 父链只往序号小的方向走，损坏的文件最多让路径提前结束，不会死循环。
    pub fn active_path(&self) -> Vec<usize> {
        let Some(mut index) = self.entries.len().checked_sub(1) else {
            return Vec::new();
        };
        let mut path = Vec::new();
        loop {
            path.push(index);
            let parent = self.parent_seq(index);
            if parent == 0 {
                break;
            }
            match self.index_of(parent) {
                Some(next) if next < index => index = next,
                _ => break,
            }
        }
        path.reverse();
        path
    }

    /// `/tree` 的一行：一条用户消息在树里的位置。
    pub fn tree(&self) -> Vec<TreeNode> {
        let boundary = self.boundary();
        let on_path: HashSet<u64> = self
            .active_path()
            .into_iter()
            .map(|index| self.entries[index].seq())
            .collect();
        // 同一个「上一句话」后面接了两条以上用户消息，那句话就是分叉点。
        let mut branches: std::collections::HashMap<u64, usize> = std::collections::HashMap::new();
        for index in 0..self.entries.len() {
            if !matches!(
                &self.entries[index],
                Entry::Message {
                    message: Message::User { .. },
                    ..
                }
            ) {
                continue;
            }
            *branches.entry(self.previous_user(index)).or_default() += 1;
        }

        let mut nodes = Vec::new();
        for (index, entry) in self.entries.iter().enumerate() {
            let Entry::Message {
                seq,
                timestamp,
                message: Message::User { content },
            } = entry
            else {
                continue;
            };
            // 后台作业的回灌不是人打的字，不进树。
            if content.starts_with("[后台作业 ") {
                continue;
            }
            let parent = self.parent_seq(index);
            let blocked = if *seq <= boundary {
                Some("已压缩")
            } else if parent < boundary {
                Some("跨压缩边界")
            } else {
                None
            };
            nodes.push(TreeNode {
                ordinal: nodes.len() + 1,
                seq: *seq,
                timestamp: *timestamp,
                preview: one_line(&self.raw_user_text(index, content), 60),
                on_path: on_path.contains(seq),
                branch_point: branches.get(seq).is_some_and(|count| *count >= 2),
                blocked,
            });
        }
        nodes
    }

    /// 回退到第 `ordinal` 条用户消息之前，返回它的原文（给输入行预填）。
    ///
    /// 只追加一条 `rewind`，旧分支原样留在文件里。
    pub fn rewind(&mut self, ordinal: usize) -> Result<String> {
        let nodes = self.tree();
        let node = ordinal
            .checked_sub(1)
            .and_then(|index| nodes.get(index))
            .ok_or_else(|| Error::Config(format!("没有第 {ordinal} 条；先敲 /tree 看编号")))?;
        if let Some(reason) = node.blocked {
            return Err(Error::Config(format!("第 {ordinal} 条不能回退：{reason}")));
        }
        let index = self
            .index_of(node.seq)
            .ok_or_else(|| Error::Config("会话文件里的序号对不上".to_string()))?;
        let target = self.parent_seq(index);
        if target != 0 && self.index_of(target).is_none() {
            return Err(Error::Config("会话文件里的父链断了".to_string()));
        }
        let text = self.raw_user_text(
            index,
            match &self.entries[index] {
                Entry::Message {
                    message: Message::User { content },
                    ..
                } => content,
                _ => "",
            },
        );
        self.append(Entry::Rewind {
            seq: self.next_seq,
            timestamp: now_ms(),
            to_seq: target,
        })?;
        Ok(text)
    }

    /// 用户消息对应的原文：技能调用那一轮取敲进终端的原文，其余取正文。
    fn raw_user_text(&self, index: usize, content: &str) -> String {
        if index > 0
            && let Entry::Prompt { text, .. } = &self.entries[index - 1]
        {
            return text.clone();
        }
        content.to_string()
    }

    pub fn context(&self) -> Context<'_> {
        let mut summary = None;
        let boundary = self.boundary();
        if let Some(Entry::Compaction { summary: text, .. }) = self
            .entries
            .iter()
            .rev()
            .find(|entry| matches!(entry, Entry::Compaction { .. }))
        {
            summary = Some(text.as_str());
        }
        let messages: Vec<&Message> = self
            .active_path()
            .into_iter()
            .map(|index| &self.entries[index])
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
            .active_path()
            .into_iter()
            .map(|index| &self.entries[index])
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
        })?;
        self.trim_compacted();
        Ok(())
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

    pub fn append_plan(&mut self, planned: bool) -> Result<()> {
        self.append(Entry::PlanChange {
            seq: self.next_seq,
            timestamp: now_ms(),
            planned,
        })
    }

    pub fn append_model(&mut self, provider: &str, model: &str) -> Result<()> {
        self.append(Entry::ModelChange {
            seq: self.next_seq,
            timestamp: now_ms(),
            provider: provider.to_string(),
            model: model.to_string(),
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
        for index in self.active_path().into_iter().rev() {
            let Entry::Message { message, .. } = &self.entries[index] else {
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

/// 把一段文字压成一行并截断，用于 `/tree` 的预览。
fn one_line(text: &str, limit: usize) -> String {
    let mut flat = String::new();
    for word in text.split_whitespace() {
        if !flat.is_empty() {
            flat.push(' ');
        }
        flat.push_str(word);
    }
    if flat.chars().count() <= limit {
        return flat;
    }
    let mut out: String = flat.chars().take(limit.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// 扫一遍会话文件，取最后一条压缩的保留起点；没有压缩就是 0。
///
/// 只解析 `compaction` 那几行，别的行用子串判断跳过，不把整份历史反序列化两次。
fn scan_boundary(path: &Path) -> Result<u64> {
    use std::io::BufRead;
    let file = std::fs::File::open(path)?;
    let mut boundary = 0;
    for line in std::io::BufReader::new(file).lines() {
        let line = line?;
        // 只解析疑似压缩的行：真实写入是紧凑 JSON，这里放宽到子串包含。
        if !line.contains("compaction") {
            continue;
        }
        if let Ok(Entry::Compaction { first_kept_seq, .. }) = serde_json::from_str::<Entry>(&line) {
            boundary = boundary.max(first_kept_seq);
        }
    }
    Ok(boundary)
}

/// 边界之前只留「谁说过什么」，正文、推理、工具调用与工具结果就地释放。
fn trim_entry(entry: &mut Entry, boundary: u64) {
    if boundary == 0 || entry.seq() > boundary {
        return;
    }
    let Entry::Message { message, .. } = entry else {
        return;
    };
    match message {
        Message::User { .. } | Message::Image { .. } => {}
        Message::Assistant {
            content,
            reasoning,
            tool_calls,
            usage,
        } => {
            if !content.is_empty() {
                *content = String::new();
            }
            *reasoning = None;
            if !tool_calls.is_empty() {
                *tool_calls = Vec::new();
            }
            *usage = None;
        }
        Message::Tool { content, .. } => {
            if !content.is_empty() {
                *content = String::new();
            }
        }
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

    #[test]
    fn a_model_switch_survives_reopening() {
        let mut session = temp_session("model-switch");
        session
            .append_model("deepseek", "deepseek-flash")
            .expect("记切换");
        session
            .append_model("opencode-go", "deepseek-v4.1-flash")
            .expect("再切回");
        assert_eq!(
            session.current_model(),
            Some(("opencode-go".to_string(), "deepseek-v4.1-flash".to_string())),
            "以最后一次切换为准"
        );
        // 复制到新路径再打开：全量测试并行跑时，同进程里别的用例会拉起子进程，
        // 文件锁偶发被抢；这里要验的是「记录写进了文件、重开能读回来」。
        let path = session.path().to_path_buf();
        drop(session);
        let copy = path.with_file_name("reopened.jsonl");
        std::fs::copy(&path, &copy).expect("复制会话文件");
        let reopened = Session::open(&copy, "/tmp/proj", "别的模型", "auto").expect("重开");
        assert_eq!(
            reopened.current_model(),
            Some(("opencode-go".to_string(), "deepseek-v4.1-flash".to_string())),
            "重开仍然记得切换后的模型"
        );
    }

    #[test]
    fn a_model_switch_does_not_enter_the_model_context() {
        let mut session = temp_session("model-switch-context");
        session.append_user("你好").expect("记一句");
        session
            .append_model("deepseek", "deepseek-flash")
            .expect("记切换");
        assert_eq!(session.context().messages.len(), 1, "切换条目不该发给模型");
    }

    #[test]
    fn plan_mode_survives_reopening_and_stays_out_of_the_context() {
        let mut session = temp_session("plan-mode");
        session.append_user("你好").expect("记一句");
        session.append_plan(true).expect("开计划");
        assert!(session.current_plan());
        session.append_plan(false).expect("关计划");
        assert!(!session.current_plan(), "以最后一次为准");
        assert_eq!(session.context().messages.len(), 1, "开关不该发给模型");

        session.append_plan(true).expect("再开");
        let path = session.path().to_path_buf();
        drop(session);
        let copy = path.with_file_name("reopened.jsonl");
        std::fs::copy(&path, &copy).expect("复制会话文件");
        let reopened = Session::open(&copy, "/tmp/proj", "别的模型", "auto").expect("重开");
        assert!(reopened.current_plan(), "重开仍然记得计划模式是开的");
    }

    #[test]
    fn replay_shows_the_last_turns_with_the_typed_line() {
        let mut session = temp_session("replay-turns");
        for (index, question) in ["第一问", "第二问", "第三问"].iter().enumerate() {
            session
                .append_prompt(&format!("/skill:demo {question}"))
                .unwrap();
            session.append_user("展开后的技能正文").unwrap();
            session
                .append_assistant(
                    format!("回答{index}"),
                    None,
                    vec![call("c")],
                    Usage::default(),
                )
                .unwrap();
            session
                .append_tool_result("c", "工具输出".to_string())
                .unwrap();
        }

        // 只留最后两轮；用户那句用敲的原文，工具输出不进来。
        assert_eq!(
            session.replay(2),
            vec![
                Replay::User("/skill:demo 第二问".to_string()),
                Replay::Assistant("回答1".to_string()),
                Replay::Tool(call("c")),
                Replay::User("/skill:demo 第三问".to_string()),
                Replay::Assistant("回答2".to_string()),
                Replay::Tool(call("c")),
            ]
        );
    }

    #[test]
    fn replay_starts_after_the_compaction_boundary() {
        let mut session = temp_session("replay-compaction");
        session.append_prompt("旧问题").unwrap();
        session.append_user("旧问题").unwrap();
        session
            .append_assistant("旧回答".to_string(), None, vec![], Usage::default())
            .unwrap();
        let boundary = session.last_seq() + 1;
        session
            .append_compaction("先是旧问题，然后旧回答".to_string(), boundary, 100)
            .unwrap();
        session.append_prompt("新问题").unwrap();
        session.append_user("新问题").unwrap();
        session
            .append_assistant("新回答".to_string(), None, vec![], Usage::default())
            .unwrap();

        assert!(session.compacted());
        // 压缩掉的部分不再重放：模型也不记得了。
        assert_eq!(
            session.replay(3),
            vec![
                Replay::User("新问题".to_string()),
                Replay::Assistant("新回答".to_string()),
            ]
        );
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

    #[test]
    fn compaction_drops_old_bodies_but_keeps_user_text() {
        let dir = std::env::temp_dir().join(format!("yi-trim-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session.jsonl");
        let mut session = Session::open(&path, "/tmp", "m", "auto").expect("打开会话");
        session.append_user("第一句").expect("用户");
        session
            .append_assistant(
                "回答一".into(),
                Some("推理".into()),
                vec![call("a")],
                Usage::default(),
            )
            .expect("助手");
        session
            .append_tool_result("a", "工具结果".into())
            .expect("结果");
        session.append_user("第二句").expect("用户");
        session
            .append_assistant("回答二".into(), None, Vec::new(), Usage::default())
            .expect("助手");

        // 保留从「第二句」开始；它之前的正文都该被清掉。
        let second_user_seq = 4;
        session
            .append_compaction("摘要".into(), second_user_seq, 100)
            .expect("压缩");

        let trimmed: Vec<&Entry> = session
            .entries
            .iter()
            .filter(|entry| entry.seq() <= second_user_seq)
            .collect();
        for entry in trimmed {
            match entry {
                Entry::Message {
                    message:
                        Message::Assistant {
                            content,
                            reasoning,
                            tool_calls,
                            usage,
                            ..
                        },
                    ..
                } => {
                    assert!(content.is_empty(), "助手正文要释放");
                    assert!(reasoning.is_none(), "推理要释放");
                    assert!(tool_calls.is_empty(), "工具调用要释放");
                    assert!(usage.is_none(), "用量要释放");
                }
                Entry::Message {
                    message: Message::Tool { content, .. },
                    ..
                } => assert!(content.is_empty(), "工具结果要释放"),
                Entry::Message {
                    message: Message::User { content },
                    ..
                } => assert!(!content.is_empty(), "用户原文要保留给 /tree"),
                _ => {}
            }
        }

        // 边界之后的消息一个字都不能动。
        let kept = session
            .entries
            .iter()
            .find_map(|entry| match entry {
                Entry::Message {
                    seq,
                    message: Message::Assistant { content, .. },
                    ..
                } if *seq > second_user_seq && seq != &0 => Some(content.clone()),
                _ => None,
            })
            .expect("边界后的助手消息");
        assert_eq!(kept, "回答二");
        std::fs::remove_dir_all(&dir).ok();
    }
}
