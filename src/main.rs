use std::io::{self, IsTerminal, Write};

use std::path::Path;

use yi::agent::Agent;
use yi::complete::Helper;
use yi::config::{self, CONTEXT_WINDOW, Config};
use yi::provider::Event;
use yi::render::{self, Renderer};
use yi::session::{HISTORY_LIMIT, Usage};
use yi::{Error, Result};

fn main() {
    if let Err(err) = run() {
        eprintln!("yi: {err}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    yi::crash::install_hook();
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            // 默认行为就是继续本项目会话，`-c` 只为保留手感。
            "-c" => {}
            "-h" | "--help" => {
                print_help();
                return Ok(());
            }
            "-v" | "--version" => {
                println!("yi {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            other => {
                return Err(Error::Config(format!("未知参数 {other}；用 -h 查看用法")));
            }
        }
    }

    let cwd = std::env::current_dir()?.to_string_lossy().to_string();
    let cfg = Config::load()?;
    let session_path = cfg
        .home
        .join("sessions")
        .join(config::encode_project(&cwd))
        .join("session.jsonl");
    let mut agent = Agent::new(&cfg, &cwd, &session_path)?;
    let skills: Vec<(String, String)> =
        yi::skills::discover(&yi::skills::roots(&cfg.home, Path::new(&cwd)))
            .into_iter()
            .map(|skill| (skill.name, skill.description))
            .collect();
    let mut editor = make_editor(&agent, skills)?;
    // 只在真终端里上色；管道输出、NO_COLOR 时保持原样。
    let color = io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none();
    let mut renderer = Renderer::new(color);

    println!("yi · {} · {cwd}", cfg.model);
    println!("会话：{}", session_path.display());
    println!("输入 /help 查看命令，Ctrl-D 退出。\n");

    while let Some(line) = read_input(&mut editor)? {
        let prompt = line.trim();
        if prompt.is_empty() {
            continue;
        }
        agent.note_prompt(prompt)?;
        let message = if let Some(rest) = prompt.strip_prefix("/skill:") {
            match yi::skills::expand(rest, &cfg.home, std::path::Path::new(&cwd)) {
                Ok(text) => text,
                Err(err) => {
                    println!("{err}\n");
                    continue;
                }
            }
        } else if prompt.starts_with('/') {
            handle_command(prompt, &mut agent);
            continue;
        } else {
            prompt.to_string()
        };

        let mut thinking_shown = false;
        yi::cancel::reset();
        yi::cancel::arm();
        let result = agent.submit(&message, &mut |event| match event {
            Event::Text(text) => {
                let out = renderer.push(&text);
                print!("{out}");
                let _ = io::stdout().flush();
            }
            Event::Reasoning(_) => {
                if !thinking_shown {
                    thinking_shown = true;
                    println!("思考中…");
                    // 终端已经不在了就没必要报错，后续输出自然会失败。
                    let _ = io::stdout().flush();
                }
            }
            Event::Notice(notice) => {
                let half = renderer.flush_line();
                print!("{half}");
                println!();
                println!("· {notice}");
            }
            Event::ToolCall { name, summary } => {
                let half = renderer.flush_line();
                print!("{half}");
                println!();
                // 命令按 shell 的样子显示（`$ 命令`），其余工具用箭头加名字。
                if name == "bash" {
                    if color {
                        println!("\x1b[32m$\x1b[0m {}", render::shell_line(&summary, true));
                    } else {
                        println!("$ {summary}");
                    }
                } else if color {
                    println!("\x1b[36m→\x1b[0m \x1b[2m{summary}\x1b[0m");
                } else {
                    println!("→ {summary}");
                }
                // 同上：刷新失败不影响本轮结果。
                let _ = io::stdout().flush();
            }
            Event::ToolResult { name, failed, note } => {
                let half = renderer.flush_line();
                print!("{half}");
                let short: String = note.chars().take(160).collect();
                println!("{}", render::tool_line(&name, failed, &short, color));
                let _ = io::stdout().flush();
            }
        });

        yi::cancel::disarm();
        // 把没换行的半行、没闭合的代码块放出来，再打页脚。
        let tail = renderer.finish();
        print!("{tail}");
        match result {
            Ok(usage) => {
                println!();
                println!("{}", footer(&usage));
                println!();
            }
            Err(err) => {
                println!();
                println!("{err}");
                println!();
            }
        }
    }

    Ok(())
}

/// 真终端才接管按键；管道输入保持逐行读取，脚本和冒烟测试都靠它。
type Editor = rustyline::Editor<Helper, rustyline::history::DefaultHistory>;

fn make_editor(agent: &Agent, skills: Vec<(String, String)>) -> Result<Option<Editor>> {
    if !io::stdin().is_terminal() {
        return Ok(None);
    }
    let builder = rustyline::Config::builder()
        .max_history_size(HISTORY_LIMIT)
        .and_then(|builder| builder.history_ignore_dups(true))
        .map_err(|err| Error::Config(format!("配置输入行失败：{err}")))?;
    // 候选按列表展示（带描述），不是在输入行里循环替换。
    let config = builder
        .completion_type(rustyline::config::CompletionType::List)
        .auto_add_history(true)
        .build();
    let mut editor = Editor::with_config(config)
        .map_err(|err| Error::Config(format!("初始化输入行失败：{err}")))?;
    editor.set_helper(Some(Helper::new(skills)));
    for line in agent.prompt_history() {
        // 历史只用来召回：某一条加不进去不影响输入。
        let _ = editor.add_history_entry(line);
    }
    Ok(Some(editor))
}

/// 读一行输入；返回 `None` 表示该退出了（Ctrl-D 或标准输入结束）。
fn read_input(editor: &mut Option<Editor>) -> Result<Option<String>> {
    let Some(editor) = editor else {
        print!("> ");
        io::stdout().flush()?;
        let mut line = String::new();
        if io::stdin().read_line(&mut line)? == 0 {
            return Ok(None);
        }
        return Ok(Some(line));
    };
    match editor.readline("> ") {
        Ok(line) => Ok(Some(line)),
        // Ctrl-C 只放弃这一行；rustyline 不给出缓冲区内容，所以无法区分空行。
        Err(rustyline::error::ReadlineError::Interrupted) => {
            println!();
            Ok(Some(String::new()))
        }
        Err(rustyline::error::ReadlineError::Eof) => {
            println!();
            Ok(None)
        }
        Err(err) => Err(Error::Config(format!("读输入失败：{err}"))),
    }
}

fn handle_command(line: &str, agent: &mut Agent) {
    let mut parts = line.splitn(2, char::is_whitespace);
    let command = parts.next().unwrap_or("");
    let argument = parts.next().unwrap_or("").trim();

    match command {
        "/help" => print_help(),
        "/new" => match agent.new_session() {
            Ok(archived) => println!("已归档到 {}，开始新会话。\n", archived.display()),
            Err(err) => println!("{err}\n"),
        },
        "/effort" => match config::parse_effort(argument) {
            Ok(level) => {
                if let Err(err) = agent.set_effort(level.clone()) {
                    println!("{err}\n");
                    return;
                }
                println!("推理强度 → {}\n", level.as_deref().unwrap_or("auto"));
            }
            Err(err) => println!("{err}\n"),
        },
        other => println!("未知命令 {other}；用 /help 查看\n"),
    }
}

fn print_help() {
    println!(
        "\
用法：yi [-c] [-h] [-v]

  -c            继续当前项目的会话（默认行为，保留手感）
  -h, --help    显示这份帮助
  -v, --version 显示版本

斜杠命令：
  /new                      归档当前会话，开始新的一条
  /effort <档位>            设置推理强度：auto / low / high / max
  /skill:<名字> [要求]      把某个技能的正文注入这一轮
  /help                     显示这份帮助

环境变量：
  DEEPSEEK_API_KEY  API key（也可写在 ~/.yi/config.json 的 api_key）
  YI_HOME           数据目录，默认 ~/.yi

配置文件 ~/.yi/config.json 可选字段：api_key、base_url、model、effort。
"
    );
}

/// 页脚只报上下文占比：缓存和本轮用量是内部数据，不占屏幕。
fn footer(usage: &Usage) -> String {
    // 百分比用最后一次请求的输入量：那才是「上下文现在多大」。
    // 整轮累加的 prompt 会随步数增长，和多轮工具调用的实际上下文无关。
    let percent = usage.context_prompt * 100 / CONTEXT_WINDOW;
    format!("[上下文 {percent}%]")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(context_prompt: u64, input_uncached: u64, cache_hit: u64) -> Usage {
        Usage {
            input_uncached,
            cache_hit,
            output: 0,
            reasoning: 0,
            elapsed_ms: 0,
            context_prompt,
        }
    }

    #[test]
    fn footer_percent_uses_the_last_request_not_the_whole_turn() {
        // 这一轮累计输入 1,000,000，但最后一次请求只有 524,288（窗口的一半）。
        let line = footer(&usage(524_288, 500_000, 500_000));
        assert!(line.contains("上下文 50%"), "{line}");
        assert_eq!(line, "[上下文 50%]");
    }

    #[test]
    fn footer_handles_an_empty_turn() {
        assert_eq!(footer(&usage(0, 0, 0)), "[上下文 0%]");
    }
}
