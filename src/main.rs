use std::io::{self, IsTerminal, Write};

use std::path::Path;

use yi::agent::Agent;
use yi::complete::Helper;
use yi::config::{self, Config};
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
    // 只认第一个参数：`-h` 与 `-v`，其余一律报错。没有参数就是默认行为。
    if let Some(arg) = std::env::args().nth(1) {
        if arg == "-h" || arg == "--help" {
            print_help();
            return Ok(());
        }
        if arg == "-v" || arg == "--version" {
            println!("yi {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        return Err(Error::Config(format!("未知参数 {arg}；用 -h 查看用法")));
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

    println!("yi · {} · {} · {cwd}", cfg.display, cfg.model_display);
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
        // 用户可以直接把图片路径贴进来（拖拽到终端也是路径文本），或用 @路径 提及。
        let images = image_paths(&message, Path::new(&cwd));
        for path in &images {
            println!(
                "{}",
                render::dim_line(&format!("图片：{}", path.display()), color)
            );
        }
        let result = agent.submit_with_images(&message, &images, &mut |event| match event {
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
                let branch = git_branch(&cwd);
                println!(
                    "{}",
                    footer(
                        &usage,
                        cfg.context_window,
                        &short_project(&cwd),
                        branch.as_deref(),
                        cfg.display,
                        &cfg.model_display,
                    )
                );
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

/// 从一句话里找出要附给模型的图片：整行就是一个图片路径，或用 `@路径` 提及。
///
/// 只认真正存在的图片文件；路径相对当前项目解析。找不到就当普通文字，不当错误。
fn image_paths(prompt: &str, cwd: &Path) -> Vec<std::path::PathBuf> {
    const EXTENSIONS: [&str; 5] = ["png", "jpg", "jpeg", "gif", "webp"];
    let looks_like_image = |raw: &str| -> Option<std::path::PathBuf> {
        let candidate = raw.trim().trim_matches(['"', '\'', '@']);
        if candidate.is_empty() {
            return None;
        }
        let path = cwd.join(candidate);
        let extension = path
            .extension()
            .and_then(|ext| ext.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if EXTENSIONS.contains(&extension.as_str()) && path.is_file() {
            Some(path)
        } else {
            None
        }
    };

    let trimmed = prompt.trim();
    if !trimmed.contains(char::is_whitespace)
        && let Some(path) = looks_like_image(trimmed)
    {
        return vec![path];
    }
    trimmed
        .split_whitespace()
        .filter(|word| word.starts_with('@'))
        .filter_map(looks_like_image)
        .collect()
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
用法：yi [-h] [-v]

  -h, --help    显示这份帮助
  -v, --version 显示版本

斜杠命令：
  /new                      归档当前会话，开始新的一条
  /effort <档位>            设置推理强度：auto / low / high / max
  /skill:<名字> [要求]      把某个技能的正文注入这一轮
  /help                     显示这份帮助

环境变量（密钥只从这里读）：
  OPENCODE_API_KEY     OpenCode Go 的密钥（默认供应商）
  DEEPSEEK_API_KEY     DeepSeek 官方的密钥（provider 设为 deepseek 时）
  YI_HOME              数据目录，默认 ~/.yi

配置文件 ~/.yi/config.json 可选字段：provider、base_url、model、effort。
  provider 选预置：opencode-go（默认）或 deepseek。
"
    );
}

/// 页脚一行：项目、分支、上下文占比、供应商、模型。
///
/// 缓存与本轮用量是内部数据，只写进会话文件，不占屏幕。
fn footer(
    usage: &Usage,
    window: u64,
    place: &str,
    branch: Option<&str>,
    provider: &str,
    model: &str,
) -> String {
    // 百分比用最后一次请求的输入量：那才是「上下文现在多大」。
    // 整轮累加的 prompt 会随步数增长，和多轮工具调用的实际上下文无关。
    let percent = usage.context_prompt * 100 / window.max(1);
    let mut out = String::from("[");
    out.push_str(place);
    if let Some(branch) = branch {
        out.push(' ');
        out.push_str(branch);
    }
    out.push_str(&format!(" · {percent}% · {provider} · {model}]"));
    out
}

/// 项目路径缩写：家目录换成 `~`，其余原样。
fn short_project(cwd: &str) -> String {
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    shorten(cwd, home.as_deref())
}

fn shorten(cwd: &str, home: Option<&Path>) -> String {
    let Some(home) = home else {
        return cwd.to_string();
    };
    match Path::new(cwd).strip_prefix(home) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".to_string(),
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => cwd.to_string(),
    }
}

/// 当前分支；不是 git 仓库、或处在游离 HEAD 时返回 `None`，页脚就省掉这一段。
fn git_branch(cwd: &str) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(cwd)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let branch = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if branch.is_empty() || branch == "HEAD" {
        return None;
    }
    Some(branch)
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
    fn a_bare_image_path_is_attached() {
        let dir = std::env::temp_dir().join(format!("yi-img-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let pic = dir.join("shot.png");
        std::fs::write(&pic, b"x").unwrap();

        assert_eq!(image_paths("shot.png", &dir), vec![pic.clone()]);
        assert_eq!(image_paths("  shot.png  ", &dir), vec![pic]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_at_mention_is_attached_and_the_rest_is_ignored() {
        let dir = std::env::temp_dir().join(format!("yi-img2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let pic = dir.join("a.jpg");
        std::fs::write(&pic, b"x").unwrap();

        assert_eq!(image_paths("@a.jpg 看看这个", &dir), vec![pic]);
        // 不是图片、不存在、没有 @ 的都不算。
        assert!(image_paths("@a.go 看看这个", &dir).is_empty());
        assert!(image_paths("@nope.png", &dir).is_empty());
        assert!(image_paths("看看这个", &dir).is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn footer_percent_uses_the_last_request_not_the_whole_turn() {
        // 这一轮累计输入 1,000,000，但最后一次请求只有窗口的一半。
        let line = footer(
            &usage(524_288, 500_000, 500_000),
            1_048_576,
            "~/gp/yi",
            Some("main"),
            "OpenCode Go",
            "DeepSeek V4.1 Flash",
        );
        assert_eq!(
            line,
            "[~/gp/yi main · 50% · OpenCode Go · DeepSeek V4.1 Flash]"
        );
    }

    #[test]
    fn footer_omits_the_branch_when_there_is_none() {
        let line = footer(
            &usage(0, 0, 0),
            1_048_576,
            "~/gp/yi",
            None,
            "DeepSeek",
            "DeepSeek Flash",
        );
        assert_eq!(line, "[~/gp/yi · 0% · DeepSeek · DeepSeek Flash]");
    }

    #[test]
    fn project_paths_under_home_get_a_tilde() {
        let home = Path::new("/Users/xsh");
        assert_eq!(shorten("/Users/xsh/gp/yi", Some(home)), "~/gp/yi");
        assert_eq!(shorten("/Users/xsh", Some(home)), "~");
        assert_eq!(shorten("/opt/tmp", Some(home)), "/opt/tmp");
        assert_eq!(shorten("/opt/tmp", None), "/opt/tmp");
    }
}
