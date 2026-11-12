use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::provider::{Event, Provider};
use crate::session::{Message, Session, Usage};
use crate::tools::{Outcome, Tools};
use crate::{Error, Result};

/// 一轮对话里最多允许模型连续调用工具的步数，防止死循环烧钱。
const MAX_TOOL_STEPS: usize = 40;
/// 输入 token 超过这个数就在本轮结束后压缩（上下文 1,048,576 的 50%）。
const COMPACT_AT_TOKENS: u64 = 524_288;
/// 压缩时保留的最近原文，按 UTF-8 字节数除以 3 估算。
const KEEP_RECENT_TOKENS: u64 = 20_000;
const COMPACTION_PROMPT: &str = include_str!("../prompts/compaction.md");

pub struct Agent {
    provider: Provider,
    session: Session,
    tools: Tools,
    system: String,
    effort: Option<String>,
}

impl Agent {
    pub fn new(cfg: &Config, cwd: &str, session_path: &Path) -> Result<Agent> {
        let session = Session::open(
            session_path,
            cwd,
            &cfg.model,
            cfg.effort.as_deref().unwrap_or("auto"),
        )?;
        let effort = session.current_effort().or(cfg.effort.clone());
        let log_dir = session_path.parent().unwrap_or(Path::new(".")).join("logs");

        Ok(Agent {
            provider: Provider::new(cfg),
            session,
            tools: Tools::new(PathBuf::from(cwd), log_dir),
            system: cfg.system_prompt(cwd),
            effort,
        })
    }

    /// 一轮对话：追加用户消息，然后反复「请求模型 → 执行工具」直到模型不再调用工具。
    pub fn submit(&mut self, prompt: &str, on: &mut dyn FnMut(Event)) -> Result<Usage> {
        self.session.append_user(prompt)?;
        let definitions = Tools::definitions();
        let mut total = Usage::default();

        for _ in 0..=MAX_TOOL_STEPS {
            let messages = self.session.messages();
            let started = std::time::Instant::now();
            let mut reply = self.provider.stream(
                &self.system,
                &messages,
                &definitions,
                self.effort.as_deref(),
                on,
            )?;
            reply.usage.elapsed_ms = started.elapsed().as_millis() as u64;

            total.input_uncached += reply.usage.input_uncached;
            total.cache_hit += reply.usage.cache_hit;
            total.output += reply.usage.output;
            total.reasoning += reply.usage.reasoning;
            total.elapsed_ms += reply.usage.elapsed_ms;

            let turn_usage = reply.usage.clone();
            self.session.append_assistant(
                reply.content,
                reply.reasoning,
                reply.tool_calls.clone(),
                reply.usage,
            )?;

            if reply.tool_calls.is_empty() {
                self.maybe_compact(&turn_usage, on)?;
                return Ok(total);
            }

            for call in reply.tool_calls {
                on(Event::ToolCall {
                    name: call.name.clone(),
                    summary: Tools::describe(&call),
                });
                match self.tools.execute(&call) {
                    Outcome::Text(text) => {
                        self.session.append_tool_result(&call.id, text)?;
                    }
                    Outcome::Image { path, note } => {
                        self.session.append_tool_result(&call.id, note)?;
                        self.session.append_image(&path)?;
                    }
                }
            }
        }

        Err(Error::Api(format!(
            "一轮里工具调用超过 {MAX_TOOL_STEPS} 次，已停止；检查是不是陷入循环了"
        )))
    }

    /// 上下文超过阈值就压缩一次。压缩失败只提示，不影响这一轮的回答。
    fn maybe_compact(&mut self, usage: &Usage, on: &mut dyn FnMut(Event)) -> Result<()> {
        if usage.prompt_tokens() <= COMPACT_AT_TOKENS {
            return Ok(());
        }
        let Some(plan) = self.session.plan_compaction(KEEP_RECENT_TOKENS) else {
            return Ok(());
        };

        let mut prompt = String::new();
        if let Some(previous) = &plan.previous_summary {
            prompt.push_str("<previous-summary>\n");
            prompt.push_str(previous);
            prompt.push_str("\n</previous-summary>\n\n");
        }
        prompt.push_str("<conversation>\n");
        prompt.push_str(&serialize_for_summary(&plan.messages_to_summarize));
        prompt.push_str("</conversation>\n");

        match self.provider.complete(COMPACTION_PROMPT, &prompt) {
            Ok(summary) => {
                let count = plan.messages_to_summarize.len();
                let tokens_before = plan.tokens_before;
                self.session
                    .append_compaction(summary, plan.first_kept_seq, tokens_before)?;
                on(Event::Notice(format!(
                    "已压缩上下文：{count} 条消息（约 {tokens_before} token）→ 摘要"
                )));
            }
            Err(err) => on(Event::Notice(format!("压缩失败，本轮内容保留：{err}"))),
        }
        Ok(())
    }

    pub fn set_effort(&mut self, effort: Option<String>) -> Result<()> {
        self.session
            .append_effort(effort.as_deref().unwrap_or("auto"))?;
        self.effort = effort;
        Ok(())
    }

    pub fn new_session(&mut self) -> Result<PathBuf> {
        self.session.archive()
    }
}

/// 把要摘要的消息压成纯文本，每个块截断到 2000 字符，避免摘要请求本身过长。
fn serialize_for_summary(messages: &[Message]) -> String {
    let mut out = String::new();
    for message in messages {
        match message {
            Message::User { content } => push_block(&mut out, "用户", content),
            Message::Assistant {
                content,
                tool_calls,
                ..
            } => {
                push_block(&mut out, "助手", content);
                for call in tool_calls {
                    push_block(
                        &mut out,
                        "调用工具",
                        &format!("{} {}", call.name, call.arguments),
                    );
                }
            }
            Message::Tool { content, .. } => push_block(&mut out, "工具结果", content),
            Message::Image { path } => push_block(&mut out, "图片", path),
        }
    }
    out
}

fn push_block(out: &mut String, label: &str, body: &str) {
    out.push('[');
    out.push_str(label);
    out.push_str("] ");
    out.push_str(&truncate_chars(body, 2000));
    out.push('\n');
}

fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max).collect();
    out.push('…');
    out
}
