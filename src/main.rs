use std::io::{self, Write};

use yi::agent::Agent;
use yi::config::{self, CONTEXT_WINDOW, Config};
use yi::provider::Event;
use yi::session::Usage;
use yi::{Error, Result};

fn main() {
    if let Err(err) = run() {
        eprintln!("yi: {err}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
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

    println!("yi · {} · {cwd}", cfg.model);
    println!("会话：{}", agent.session_path().display());
    println!("输入 /help 查看命令，Ctrl-D 退出。\n");

    loop {
        print!("> ");
        io::stdout().flush()?;

        let mut line = String::new();
        if io::stdin().read_line(&mut line)? == 0 {
            break;
        }
        let prompt = line.trim();
        if prompt.is_empty() {
            continue;
        }
        if prompt.starts_with('/') {
            handle_command(prompt, &mut agent);
            continue;
        }

        let mut thinking_shown = false;
        let result = agent.submit(prompt, &mut |event| match event {
            Event::Text(text) => {
                print!("{text}");
                let _ = io::stdout().flush();
            }
            Event::Reasoning(_) => {
                if !thinking_shown {
                    thinking_shown = true;
                    println!("思考中…");
                    let _ = io::stdout().flush();
                }
            }
        });

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
  /help                     显示这份帮助

环境变量：
  DEEPSEEK_API_KEY  API key（也可写在 ~/.yi/config.json 的 api_key）
  YI_HOME           数据目录，默认 ~/.yi

配置文件 ~/.yi/config.json 可选字段：api_key、base_url、model、effort。
"
    );
}

fn footer(usage: &Usage) -> String {
    let prompt = usage.prompt_tokens();
    let percent = prompt * 100 / CONTEXT_WINDOW;
    let rate = (usage.cache_hit * 100).checked_div(prompt).unwrap_or(0);
    format!(
        "[上下文 {percent}% · 缓存命中 {rate}% · 本轮 {} in / {} out]",
        human(prompt),
        human(usage.output)
    )
}

fn human(tokens: u64) -> String {
    if tokens >= 1_000 {
        format!("{:.1}k", tokens as f64 / 1_000.0)
    } else {
        tokens.to_string()
    }
}
