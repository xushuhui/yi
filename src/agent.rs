use std::path::{Path, PathBuf};

use crate::Result;
use crate::config::Config;
use crate::provider::{Event, Provider};
use crate::session::{Message, Session, Usage};
use crate::tools::{Outcome, Tools};

/// 同一个调用连续失败这么多次就停：挡住真死循环，又不掐断正常的长任务。
const REPEAT_FAILURES: usize = 3;
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
        let mut session = session;
        // 上次被中断可能留下没配对的工具调用，先补平再发请求。
        session.repair_interrupted()?;
        // panic 时会话文件要靠这两个信息才能接一条 notice。
        crate::crash::install(session_path, session.last_seq());
        let effort = session.current_effort().or(cfg.effort.clone());
        let log_dir = session_path.parent().unwrap_or(Path::new(".")).join("logs");

        Ok(Agent {
            provider: Provider::new(cfg),
            session,
            tools: Tools::new(
                PathBuf::from(cwd),
                log_dir,
                cfg.gopls.clone(),
                cfg.phpantom.clone(),
            ),
            system: cfg.system_prompt(cwd),
            effort,
        })
    }

    /// 工具结果算不算失败：显式的 `ERROR`，或 bash 的非零退出码。
    /// 只看末尾的退出码行，避免命令输出里恰好出现「退出码：」被误判。
    fn looks_failed(text: &str) -> bool {
        if text.starts_with("ERROR") {
            return true;
        }
        text.lines()
            .rev()
            .find(|line| line.starts_with("退出码："))
            .is_some_and(|line| line.trim() != "退出码：0")
    }

    /// 记下用户敲的原文，供上键历史使用；不进入模型上下文。
    pub fn note_prompt(&mut self, text: &str) -> Result<()> {
        self.session.append_prompt(text)
    }

    /// 当前会话的历史原文，按时间顺序。
    pub fn prompt_history(&self) -> Vec<String> {
        self.session.prompts()
    }

    /// 一轮对话的入口：失败原因写进会话文件再返回，事后才查得到。
    pub fn submit(&mut self, prompt: &str, on: &mut dyn FnMut(Event)) -> Result<Usage> {
        let result = self.run_turn(prompt, on);
        if let Err(crate::Error::Cancelled) = &result {
            let _ = self
                .session
                .append_notice("用户按 Ctrl-C 取消了这一轮，会话保留");
        } else if let Err(err) = &result {
            // 写不进去也不能盖住真正的错误，屏幕上那份照样打印。
            let _ = self.session.append_notice(&format!("这一轮失败：{err}"));
        }
        result
    }

    /// 一轮对话：追加用户消息，然后反复「请求模型 → 执行工具」直到模型不再调用工具。
    fn run_turn(&mut self, prompt: &str, on: &mut dyn FnMut(Event)) -> Result<Usage> {
        self.session.append_user(prompt)?;
        let definitions = Tools::definitions();
        let mut total = Usage::default();

        // 同一个「工具名 + 参数」连续失败计数；成功一次或换了调用就清零。
        let mut last_call: Option<(String, String)> = None;
        let mut last_summary = String::new();
        let mut failures = 0usize;

        loop {
            let context = self.session.context();
            let started = std::time::Instant::now();
            let mut reply = self.provider.stream(
                &self.system,
                context.summary,
                &context.messages,
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
            total.context_prompt = reply.usage.prompt_tokens();

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

            for (index, call) in reply.tool_calls.iter().enumerate() {
                if crate::cancel::requested() {
                    // 这一批里还没跑的调用要补上结果，否则下次请求会被供应商拒绝。
                    for skipped in &reply.tool_calls[index..] {
                        self.session.append_tool_result(
                            &skipped.id,
                            "ERROR: 这一轮被 Ctrl-C 取消，这个工具调用没有执行。".to_string(),
                        )?;
                    }
                    return Err(crate::Error::Cancelled);
                }
                on(Event::ToolCall {
                    name: call.name.clone(),
                    summary: self.tools.describe(call),
                });
                let signature = (call.name.clone(), call.arguments.clone());
                last_summary = self.tools.describe(call);
                match self.tools.execute(call) {
                    Outcome::Text(text) => {
                        let failed = Self::looks_failed(&text);
                        if failed && last_call.as_ref() == Some(&signature) {
                            failures += 1;
                        } else if failed {
                            failures = 1;
                        } else {
                            failures = 0;
                        }
                        last_call = Some(signature);
                        // 失败原因和 edit 改动量值得让人看见；成功的大段输出不回显。
                        if failed {
                            on(Event::ToolResult {
                                name: call.name.clone(),
                                failed: true,
                                note: failure_note(&text).to_string(),
                            });
                        } else if call.name == "edit" {
                            on(Event::ToolResult {
                                name: call.name.clone(),
                                failed: false,
                                note: failure_note(&text).to_string(),
                            });
                        }
                        self.session.append_tool_result(&call.id, text)?;
                    }
                    Outcome::Image { path, note } => {
                        failures = 0;
                        self.session.append_tool_result(&call.id, note)?;
                        self.session.append_image(&path)?;
                    }
                }
            }

            if failures >= REPEAT_FAILURES {
                // 同一件事失败三次还不换路，继续跑只会烧钱。
                self.session.append_notice(&format!(
                    "同一个调用连续失败 {REPEAT_FAILURES} 次，已停止调用工具：{last_summary}"
                ))?;
                on(Event::Notice(format!(
                    "同一个调用连续失败 {REPEAT_FAILURES} 次，停止调用工具，先收尾"
                )));
                break;
            }
        }

        self.finish_with_summary(total, on)
    }

    /// 强行停止后的一次收尾：不带工具，只让模型说明现状，别把话停在半句。
    fn finish_with_summary(
        &mut self,
        mut total: Usage,
        on: &mut dyn FnMut(Event),
    ) -> Result<Usage> {
        let context = self.session.context();
        let stop = "工具调用已被强行停止，不能再调用任何工具。用几句话说明：已经改了什么、验证了什么、为什么卡住、还剩什么没做完。";
        let reply = self.provider.stream(
            &format!("{stop}\n\n{}", self.system),
            context.summary,
            &context.messages,
            &serde_json::json!([]),
            self.effort.as_deref(),
            on,
        )?;
        total.input_uncached += reply.usage.input_uncached;
        total.cache_hit += reply.usage.cache_hit;
        total.output += reply.usage.output;
        total.reasoning += reply.usage.reasoning;
        total.context_prompt = reply.usage.prompt_tokens();
        self.session
            .append_assistant(reply.content, reply.reasoning, Vec::new(), reply.usage)?;
        Ok(total)
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

/// 取第一行，给界面用。
fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or("")
}

/// 失败时给人看的那一行：优先退出码，其次第一行。
/// 命令输出很多时，第一行往往只是普通输出，未必是失败原因。
fn failure_note(text: &str) -> &str {
    text.lines()
        .rev()
        .find(|line| line.starts_with("退出码："))
        .unwrap_or_else(|| first_line(text))
}

#[cfg(test)]
mod tests {
    use super::failure_note;

    #[test]
    fn a_non_zero_exit_reports_the_exit_code_not_the_first_output_line() {
        let text = "import sys\n\n退出码：1\n完整输出：/tmp/log";
        assert_eq!(failure_note(text), "退出码：1");
    }

    #[test]
    fn an_error_line_is_reported_as_is() {
        let text = "ERROR: 第 398 行没有读过，不能改。先 read 这一段。";
        assert!(failure_note(text).starts_with("ERROR: 第 398 行"));
    }
}
