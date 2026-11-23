use std::path::{Path, PathBuf};

use crate::Result;
use crate::config::Config;
use crate::provider::{Event, Provider};
use crate::session::{Message, Session, ToolCall, Usage};
use crate::tools::{Outcome, Tools};

/// 同一个调用连续失败这么多次就停：挡住真死循环，又不掐断正常的长任务。
const REPEAT_FAILURES: usize = 3;
/// 压缩时保留的最近原文，按 UTF-8 字节数除以 3 估算。
const KEEP_RECENT_TOKENS: u64 = 20_000;
const COMPACTION_PROMPT: &str = include_str!("../prompts/compaction.md");

/// 计划模式追加在系统提示词后面：模型从第一句就知道边界，不靠撞拒绝猜。
/// 切到/切出计划模式会让前缀缓存失效一次，人工切换频率低，可以接受。
/// 子 agent 的单次输出上限：它会作为工具结果进入父对话，不能无限长。
const TASK_OUTPUT_BYTES: usize = 16 * 1024;
const TASK_OUTPUT_LINES: usize = 400;
/// 同一轮里最多同时跑几个子 agent。
const MAX_PARALLEL_TASKS: usize = 4;
/// 子 agent 最多几步。SIMPLIFICATION: 没有按「失败三次」收尾，先给一个硬上限；
/// 真出现反复失败时再补和主循环一样的规则。
const MAX_TASK_STEPS: usize = 30;

const PLAN_PROMPT: &str = "\n\n<plan-mode>
现在是计划模式：工作树只读。write / edit 会被拒绝，bash 只放行只读命令。
先用工具把能查的事实查清楚；能自己查到的事不要问人。
信息不足就一次把要确认的问题问完，给出具体选项；信息已经够就直接给计划。
计划要能直接执行：改哪些文件、每处改什么、先做什么后做什么、怎么验证、取舍与风险。
计划不落盘；需要新增或修改的 ADR、术语，列进计划，等用户退出计划模式后再写。
</plan-mode>
";

pub struct Agent {
    provider: Provider,
    session: Session,
    tools: Tools,
    system: String,
    effort: Option<String>,
    /// 上下文涨到这个量就在本轮结束后压缩；按当前模型的窗口算。
    compact_at: u64,
    /// 当前窗口大小，页脚按它算占比；切换后跟着换。
    window: u64,
    provider_id: String,
    provider_display: String,
    model_display: String,
    /// 计划模式：只能读，改文件与写命令会被工具层拒绝。
    plan: bool,
    /// 子 agent 需要：自己造 Provider、造只读工具、写日志。
    cfg: Config,
    cwd: String,
    session_id: String,
    log_dir: PathBuf,
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

        // 上次在这个会话里 /switch 过：以会话记录为准，配置只作新会话的默认。
        let mut chosen = cfg.clone();
        if let Some((provider, model)) = session.current_model() {
            if provider != chosen.provider {
                chosen = chosen.with_provider(&provider)?;
            }
            if model != chosen.model {
                chosen.model = model.clone();
                chosen.model_display = model;
            }
        }
        let cfg = &chosen;

        let session_id = session.id().to_string();
        let plan = session.current_plan();
        let mut tools = Tools::new(
            PathBuf::from(cwd),
            log_dir.clone(),
            cfg.gopls.clone(),
            cfg.phpantom.clone(),
            cfg.rust_analyzer.clone(),
        );
        tools.set_plan(plan);
        Ok(Agent {
            provider: Provider::new(cfg, &session_id),
            session,
            tools,
            system: cfg.system_prompt(cwd),
            effort,
            compact_at: cfg.compact_at_tokens(),
            window: cfg.context_window,
            provider_id: cfg.provider.to_string(),
            provider_display: cfg.display.to_string(),
            model_display: cfg.model_display.clone(),
            plan,
            cfg: chosen.clone(),
            cwd: cwd.to_string(),
            session_id,
            log_dir,
        })
    }

    /// 当前生效的系统提示词：计划模式追加一段说明。
    fn system_prompt(&self) -> std::borrow::Cow<'_, str> {
        if self.plan {
            std::borrow::Cow::Owned(format!("{}{PLAN_PROMPT}", self.system))
        } else {
            std::borrow::Cow::Borrowed(&self.system)
        }
    }

    pub fn plan(&self) -> bool {
        self.plan
    }

    /// 切计划模式：先落会话，再改工具层——顺序反了的话，写盘失败会留下
    /// 「界面说只读、工具却没锁」的状态。
    pub fn set_plan(&mut self, planned: bool) -> Result<()> {
        self.session.append_plan(planned)?;
        self.plan = planned;
        self.tools.set_plan(planned);
        Ok(())
    }

    pub fn provider_id(&self) -> &str {
        &self.provider_id
    }

    pub fn provider_display(&self) -> &str {
        &self.provider_display
    }

    pub fn model_display(&self) -> &str {
        &self.model_display
    }

    pub fn window(&self) -> u64 {
        self.window
    }

    /// 换供应商：重建请求通道，并把这件事写进会话，重开时照它恢复。
    ///
    /// 系统提示与历史不动——前缀能命中的部分尽量保住；换模型必然掉一段缓存，
    /// 这是换通道的固有代价。
    pub fn switch(&mut self, cfg: &Config) -> Result<()> {
        let session_id = self.session.id().to_string();
        self.provider = Provider::new(cfg, &session_id);
        self.compact_at = cfg.compact_at_tokens();
        self.window = cfg.context_window;
        self.provider_id = cfg.provider.to_string();
        self.provider_display = cfg.display.to_string();
        self.model_display = cfg.model_display.clone();
        self.session.append_model(cfg.provider, &cfg.model)
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

    /// 启动时重放的最近几轮。
    pub fn replay(&self, turns: usize) -> Vec<crate::session::Replay> {
        self.session.replay(turns)
    }

    /// 会话是否被压缩过。
    pub fn compacted(&self) -> bool {
        self.session.compacted()
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
        self.submit_with_images(prompt, &[], on)
    }

    /// 和 [`Agent::submit`] 一样，另外把用户手上的图片一起发出去。
    pub fn submit_with_images(
        &mut self,
        prompt: &str,
        images: &[std::path::PathBuf],
        on: &mut dyn FnMut(Event),
    ) -> Result<Usage> {
        let result = self.run_turn(prompt, images, on);
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
    fn run_turn(
        &mut self,
        prompt: &str,
        images: &[std::path::PathBuf],
        on: &mut dyn FnMut(Event),
    ) -> Result<Usage> {
        // 后台命令跑完的结果先落进会话，模型这一轮就能看到；界面上只留一行提示。
        for job in crate::jobs::take_finished() {
            self.session.append_user(&format!(
                "[后台作业 {} 完成，用时 {:.1}s，命令：{}]\n{}",
                job.id, job.secs, job.command, job.output
            ))?;
            on(Event::Notice(format!(
                "后台作业 {} 完成（{:.1}s）：{}",
                job.id, job.secs, job.command
            )));
        }
        self.session.append_user(prompt)?;
        // 用户贴的图跟着这句话一起进上下文；读不出来的图会让下一轮请求缺内容，先在这里挡住。
        for path in images {
            if let Err(err) = self.session.append_image(path) {
                let _ = self
                    .session
                    .append_notice(&format!("图片没放进会话：{err}"));
            }
        }
        let definitions = Tools::definitions();
        let mut total = Usage::default();

        // 同一个「工具名 + 参数」连续失败计数；成功一次或换了调用就清零。
        let mut last_call: Option<(String, String)> = None;
        let mut last_summary = String::new();
        let mut failures = 0usize;

        loop {
            let context = self.session.context();
            let started = std::time::Instant::now();
            let system = self.system_prompt();
            let mut reply = self.provider.stream(
                system.as_ref(),
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

            // 一轮里可能有多个 task：它们各自独立，可以并行；其余工具仍按原顺序执行。
            let mut task_results: Vec<Option<String>> = vec![None; reply.tool_calls.len()];
            let task_slots: Vec<usize> = reply
                .tool_calls
                .iter()
                .enumerate()
                .filter(|(_, call)| call.name == "task")
                .map(|(index, _)| index)
                .collect();
            if !task_slots.is_empty() {
                let cfg = self.cfg.clone();
                let session_id = self.session_id.clone();
                let cwd = self.cwd.clone();
                let log_dir = self.log_dir.clone();
                let system = self.system_prompt().into_owned();
                let effort = self.effort.clone();
                std::thread::scope(|scope| {
                    for chunk in task_slots.chunks(MAX_PARALLEL_TASKS) {
                        let mut handles = Vec::new();
                        for &index in chunk {
                            let cfg = cfg.clone();
                            let session_id = session_id.clone();
                            let cwd = cwd.clone();
                            let log_dir = log_dir.clone();
                            let system = system.clone();
                            let effort = effort.clone();
                            let call = reply.tool_calls[index].clone();
                            handles.push((
                                index,
                                scope.spawn(move || {
                                    run_subagent(
                                        &cfg,
                                        &session_id,
                                        &cwd,
                                        &log_dir,
                                        &system,
                                        effort.as_deref(),
                                        &call,
                                    )
                                }),
                            ));
                        }
                        for (index, handle) in handles {
                            task_results[index] = Some(
                                handle
                                    .join()
                                    .unwrap_or_else(|_| "ERROR: 子 agent 线程崩溃".to_string()),
                            );
                        }
                    }
                });
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
                let outcome = if call.name == "task" {
                    Outcome::Text(
                        task_results[index]
                            .take()
                            .unwrap_or_else(|| "ERROR: 子 agent 没有返回结果".to_string()),
                    )
                } else {
                    self.tools.execute(call, on)
                };
                match outcome {
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
                        // 每个工具结束都发一条：界面靠它把状态行从「运行 X」推回
                        // 「等待模型」。成功时提示为空，界面什么都不打印——输出与改动
                        // 清单仍在会话里，模型下一轮读得到。
                        on(Event::ToolResult {
                            name: call.name.clone(),
                            // 工具调用行不再打印，所以提示里补上是哪条命令 / 哪个文件。
                            note: if failed {
                                let reason = failure_note(&text);
                                let reason = reason.strip_prefix("ERROR: ").unwrap_or(reason);
                                format!("{last_summary} — {reason}")
                            } else if text.contains(crate::tools::NO_MATCH) {
                                format!("{last_summary} — {}", crate::tools::NO_MATCH)
                            } else {
                                String::new()
                            },
                            failed,
                        });
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
        let system = self.system_prompt();
        let reply = self.provider.stream(
            &format!("{stop}\n\n{system}"),
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
        if usage.prompt_tokens() <= self.compact_at {
            return Ok(());
        }
        let Some(plan) = self.session.plan_compaction(KEEP_RECENT_TOKENS) else {
            return Ok(());
        };

        // 压缩请求尽量复刻主请求的前缀：先放上一份摘要（如果压缩过），再原样带上
        // 要压缩的这段历史，最后才是一句指令。这样除了指令本身，前面的 token 都能命中缓存。
        let mut history: Vec<Message> = Vec::new();
        if let Some(previous) = &plan.previous_summary {
            history.push(Message::User {
                content: format!("<compaction-summary>\n{previous}\n</compaction-summary>"),
            });
        }
        history.extend(plan.messages_to_summarize.iter().cloned());
        // 系统提示保持和主请求一样，压缩要求放进最后一句用户消息里——
        // 只要 system 一变，前缀缓存从第一个 token 就失效了。
        let instruction =
            format!("{COMPACTION_PROMPT}\n\n把上面的对话压缩成摘要，只输出摘要本身。");

        let system = self.system_prompt();
        match self
            .provider
            .complete(system.as_ref(), &history, &instruction)
        {
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

/// 跑一个子 agent：独立上下文、只读工具，结果只有最后一段话。
fn run_subagent(
    cfg: &Config,
    session_id: &str,
    cwd: &str,
    log_dir: &Path,
    system: &str,
    effort: Option<&str>,
    call: &ToolCall,
) -> String {
    let Ok(args) = serde_json::from_str::<serde_json::Value>(&call.arguments) else {
        return "ERROR: 子 agent 的参数不是合法 JSON".to_string();
    };
    let Some(task) = args.get("task").and_then(serde_json::Value::as_str) else {
        return "ERROR: task 缺少 task 描述".to_string();
    };
    let label = args
        .get("name")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("task");
    let provider = Provider::new(cfg, &format!("{session_id}-sub-{}", sanitize(&call.id)));
    let mut tools = Tools::subagent(
        PathBuf::from(cwd),
        log_dir.to_path_buf(),
        cfg.gopls.clone(),
        cfg.phpantom.clone(),
        cfg.rust_analyzer.clone(),
    );
    let definitions = Tools::subagent_definitions();
    let mut messages = vec![Message::User {
        content: format!("任务：{task}"),
    }];
    let mut log = format!(
        "# 子 agent {label}
任务：{task}
"
    );

    for _ in 0..MAX_TASK_STEPS {
        if crate::cancel::requested() {
            return finish_subagent(log_dir, call, log, "ERROR: 已取消".to_string());
        }
        let refs: Vec<&Message> = messages.iter().collect();
        let reply = match provider.stream(system, None, &refs, &definitions, effort, &mut |_| {}) {
            Ok(reply) => reply,
            Err(err) => {
                return finish_subagent(log_dir, call, log, format!("ERROR: 子 agent 失败：{err}"));
            }
        };
        log.push_str(&format!(
            "
## 助手
{}
",
            reply.content
        ));
        if reply.tool_calls.is_empty() {
            let text = format!(
                "[子 agent {label}]
{}",
                cap_task_output(&reply.content)
            );
            return finish_subagent(log_dir, call, log, text);
        }
        messages.push(Message::Assistant {
            content: reply.content,
            reasoning: reply.reasoning,
            tool_calls: reply.tool_calls.clone(),
            usage: None,
        });
        for called in &reply.tool_calls {
            let output = match tools.execute(called, &mut |_| {}) {
                Outcome::Text(text) => text,
                Outcome::Image { note, .. } => note,
            };
            log.push_str(&format!(
                "
### 工具 {} {}
{}
",
                called.name, called.arguments, output
            ));
            messages.push(Message::Tool {
                tool_call_id: called.id.clone(),
                content: output,
            });
        }
    }
    finish_subagent(
        log_dir,
        call,
        log,
        format!("ERROR: 子 agent 超过 {MAX_TASK_STEPS} 步仍未收尾"),
    )
}

fn finish_subagent(log_dir: &Path, call: &ToolCall, log: String, text: String) -> String {
    let path = log_dir.join(format!("task-{}.log", sanitize(&call.id)));
    let _ = std::fs::create_dir_all(log_dir);
    // 日志写不进去不影响这次结果：中间过程只是排查用的。
    let _ = std::fs::write(&path, log);
    text
}

fn sanitize(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

/// 子 agent 的结果会作为工具结果进入父对话，按行数和字节数截断。
fn cap_task_output(text: &str) -> String {
    let mut out = String::new();
    let mut truncated = false;
    for (index, line) in text.lines().enumerate() {
        if index >= TASK_OUTPUT_LINES || out.len() + line.len() + 1 > TASK_OUTPUT_BYTES {
            truncated = true;
            break;
        }
        out.push_str(line);
        out.push('\n');
    }
    if out.is_empty() {
        out = text.to_string();
    }
    if out.len() > TASK_OUTPUT_BYTES {
        let mut cut = TASK_OUTPUT_BYTES;
        while cut > 0 && !out.is_char_boundary(cut) {
            cut -= 1;
        }
        out.truncate(cut);
        truncated = true;
    }
    if truncated {
        out.push_str("…（子 agent 输出已截断）");
    }
    out.trim_end().to_string()
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
