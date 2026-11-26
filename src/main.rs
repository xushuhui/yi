// 生产代码不 panic：和库里同一条规则，放在 CLI 这层也拦一道。
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

use std::io::{self, IsTerminal, Write};

use std::path::Path;

use yi::agent::Agent;
use yi::clipboard;
use yi::config::{self, Config};
use yi::input::Action;
use yi::program_status;
use yi::provider::Event;
use yi::render::{self, Renderer};
use yi::session::Usage;
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
    // 终端标题：Rex 的标签页、Ghostty 的标题栏都读它。
    let project = Path::new(&cwd)
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| cwd.clone());
    let mut title = Title::new(io::stdout().is_terminal(), project);
    // 只在真终端里上色；管道输出、NO_COLOR 时保持原样。
    let color = io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none();
    // 行编辑器只给真人用：标准输入是管道时一个字节都不加。
    let interactive = io::stdin().is_terminal();
    // 剪贴板图片落在数据目录里，不落 /tmp：会话只记路径，重启后还要能读到。
    let clip_dir = cfg.home.join("clipboard");
    let mut input = if interactive {
        Some(yi::input::Input::new(
            skills,
            agent.prompt_history(),
            agent.plan(),
        )?)
    } else {
        None
    };
    let mut renderer = Renderer::new(color);
    // 处理中的状态行：只在真终端里动，管道输出一个字节都不写。
    let mut status = Status::new(io::stdout().is_terminal(), color);

    println!(
        "yi · {} · {} · {cwd}",
        agent.provider_display(),
        agent.model_display()
    );
    println!("会话：{}", session_path.display());
    println!("输入 /help 查看命令，Ctrl-D 退出。\n");
    // 只给真人看：管道与重定向里保持逐字节稳定。
    if io::stdout().is_terminal() {
        print_replay(&agent, color);
    }

    // 上一轮里敲的字：轮末取出来放回提示符，免得白打。
    let mut draft = String::new();
    loop {
        let prefill = std::mem::take(&mut draft);
        let line = match &mut input {
            Some(input) => {
                // 提示符要反映最新的模式：`/plan` 改过之后也得跟上。
                input.set_planned(agent.plan());
                let mut toggle_plan = || {
                    let planned = !agent.plan();
                    if let Err(err) = agent.set_plan(planned) {
                        println!("{err}");
                    }
                    agent.plan()
                };
                match input.read(&prefill, &mut toggle_plan)? {
                    Action::Line(line) => line,
                    Action::Interrupted => continue,
                    Action::Eof => break,
                }
            }
            None => {
                // 管道与重定向：逐行读，不加任何界面字节。
                print!("> ");
                io::stdout().flush()?;
                let mut line = String::new();
                if io::stdin().read_line(&mut line)? == 0 {
                    break;
                }
                line
            }
        };
        // Ctrl-G 在行里留了标记；这里换回文字，并把图片取出来。
        let (line, has_image) = clipboard::strip_tokens(&line);
        let prompt = line.trim();
        if prompt.is_empty() && !has_image {
            continue;
        }
        agent.note_prompt(prompt)?;
        let mut message = if let Some(rest) = prompt.strip_prefix("/skill:") {
            match yi::skills::expand(rest, &cfg.home, std::path::Path::new(&cwd)) {
                Ok(text) => text,
                Err(err) => {
                    println!("{err}\n");
                    continue;
                }
            }
        } else if prompt.starts_with('/') {
            if let Some(text) = handle_command(prompt, &mut agent, &cfg) {
                draft = text;
            }
            continue;
        } else {
            prompt.to_string()
        };

        yi::cancel::reset();
        yi::cancel::arm();
        // 用户可以直接把图片路径贴进来（拖拽到终端也是路径文本），或用 @路径 提及。
        let mut images = image_paths(&message, Path::new(&cwd));
        if has_image {
            match clipboard::image_path(&clip_dir) {
                Some(path) => images.insert(0, path),
                None => println!(
                    "{}",
                    render::dim_line("剪贴板里没有图片，这一轮只发文字", color)
                ),
            }
        }
        if message.is_empty() && !images.is_empty() {
            // 只挂图不写字：空消息供应商会拒，补一句最小的话。
            message = "看看这张图片里有什么。".to_string();
        }
        for path in &images {
            println!(
                "{}",
                render::dim_line(&format!("图片：{}", path.display()), color)
            );
        }
        // 干活期间藏光标：状态行原地刷新，光标停在行尾会被当成「又在等你输入」。
        hide_cursor();
        status.set("等待模型");
        title.start_working();
        let result = agent.submit_with_images(&message, &images, &mut |event| match event {
            // 空转一下：刷新「等待模型 3.2s」这类状态行，不代表有内容。
            Event::Tick => {
                status.tick();
                title.spin();
            }
            Event::Text(text) => {
                let previous = status.clear();
                title.spin();
                let out = renderer.push(&text);
                print!("{out}");
                status.resume(previous);
                let _ = io::stdout().flush();
            }
            Event::Reasoning(_) => {
                // 只在第一次把阶段换成「思考中」，之后不重置计时。
                if status.label() != Some("思考中") {
                    status.set("思考中");
                }
                title.spin();
            }
            Event::Notice(notice) => {
                let half = renderer.flush_line();
                let previous = status.clear();
                print!("{half}");
                println!();
                println!("· {notice}");
                status.resume(previous);
                let _ = io::stdout().flush();
            }
            Event::ToolCall { name, .. } => {
                // 过程不打印：只在状态行里露一下工具名，失败时才有红行。
                status.set(&format!("运行 {name}"));
                title.spin();
            }
            Event::ToolResult { name, note, failed } => {
                // 成功时没有可打印的内容：只把状态行推回「等待模型」。
                if note.is_empty() {
                    status.set("等待模型");
                    return;
                }
                let half = renderer.flush_line();
                status.clear();
                print!("{half}");
                let short: String = note.chars().take(160).collect();
                if failed {
                    println!("{}", render::tool_line(&name, &short, color));
                } else {
                    println!("{}", render::tool_note(&name, &short, color));
                }
                // 工具跑完必然是再一次模型往返。
                status.set("等待模型");
                let _ = io::stdout().flush();
            }
        });

        status.clear();
        show_cursor();
        draft = yi::cancel::take_draft();
        match &result {
            Ok(usage) => {
                title.ready();
                program_status::done(&format!("完成 · {:.1}s", usage.elapsed_ms as f64 / 1000.0))
            }
            Err(Error::Cancelled) => {
                title.ready();
                program_status::idle()
            }
            Err(err) => {
                title.failed();
                program_status::error(&format!("这一轮失败：{err}"))
            }
        }
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
                        agent.window(),
                        &short_project(&cwd),
                        branch.as_deref(),
                        agent.plan(),
                        agent.provider_display(),
                        agent.model_display(),
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
    // 退出时把还在跑的后台命令整组杀掉，不留孤儿进程。
    yi::jobs::kill_all();
    title.ready();

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

/// 干活期间藏光标：状态行原地刷新，光标停在行尾会被误认成新的输入行。
fn hide_cursor() {
    if io::stdout().is_terminal() {
        print!("\x1b[?25l");
        let _ = io::stdout().flush();
    }
}

/// 把光标交回去。行编辑器会自己再管一次，这里负责两轮之间那段空档。
fn show_cursor() {
    if io::stdout().is_terminal() {
        print!("\x1b[?25h");
        let _ = io::stdout().flush();
    }
}

fn handle_command(line: &str, agent: &mut Agent, cfg: &Config) -> Option<String> {
    let mut parts = line.splitn(2, char::is_whitespace);
    let command = parts.next().unwrap_or("");
    let argument = parts.next().unwrap_or("").trim();

    match command {
        "/help" => print_help(),
        "/new" => match agent.new_session() {
            Ok(archived) => println!("已归档到 {}，开始新会话。\n", archived.display()),
            Err(err) => println!("{err}\n"),
        },
        "/plan" => {
            let planned = !agent.plan();
            match agent.set_plan(planned) {
                Ok(()) if planned => println!(
                    "计划模式 开：工作树只读，bash 只放行只读命令。\n把计划想清楚再敲 /plan 退出。\n"
                ),
                Ok(()) => println!("计划模式 关：可以改文件了。\n"),
                Err(err) => println!("{err}\n"),
            }
        }
        "/effort" => match config::parse_effort(argument) {
            Ok(level) => match agent.set_effort(level.clone()) {
                Ok(()) => println!("推理强度 → {}\n", level.as_deref().unwrap_or("auto")),
                Err(err) => println!("{err}\n"),
            },
            Err(err) => println!("{err}\n"),
        },
        "/switch" => {
            if argument.is_empty() {
                let known: Vec<&str> = config::PRESETS.iter().map(|p| p.id).collect();
                println!(
                    "当前模型：{}/{}\n可选：{}\n",
                    agent.provider_id(),
                    agent.model_display(),
                    known.join(" / ")
                );
                return None;
            }
            if argument == agent.provider_id() {
                println!("已经是 {argument}\n");
                return None;
            }
            match cfg
                .with_provider(argument)
                .and_then(|next| agent.switch(&next))
            {
                Ok(()) => println!(
                    "模型 → {}/{}，下一句生效；重开会话仍然用它\n",
                    agent.provider_id(),
                    agent.model_display()
                ),
                Err(err) => println!("{err}\n"),
            }
        }
        "/tree" => return tree_command(argument, agent),
        other => println!("未知命令 {other}；用 /help 查看\n"),
    }
    None
}

/// `/tree`：不带参数列树；带序号回退到那条用户消息之前。
fn tree_command(argument: &str, agent: &mut Agent) -> Option<String> {
    if argument.is_empty() {
        let nodes = agent.tree();
        if nodes.is_empty() {
            println!("会话里还没有用户消息。\n");
            return None;
        }
        println!("会话树（● 当前路径 · ○ 其它分支 · ⑂ 分支点）：");
        for node in &nodes {
            let mark = if node.on_path { '●' } else { '○' };
            let branch = if node.branch_point { '⑂' } else { ' ' };
            let blocked = node
                .blocked
                .map(|reason| format!("（{reason}）"))
                .unwrap_or_default();
            println!(
                "{:>3}  {mark}{branch} {}  {}{blocked}",
                node.ordinal,
                clock(node.timestamp),
                node.preview
            );
        }
        println!("/tree <序号> 回到那条消息之前；旧路径保留，可以再切回。\n");
        return None;
    }
    let Ok(ordinal) = argument.parse::<usize>() else {
        println!("序号要是数字，例如 /tree 3\n");
        return None;
    };
    match agent.rewind(ordinal) {
        Ok(text) => {
            println!("已回到第 {ordinal} 条之前；原来的路径还在，/tree 可以切回。\n");
            Some(text)
        }
        Err(err) => {
            println!("{err}\n");
            None
        }
    }
}

/// 毫秒时间戳 → 本地 `HH:MM`；取不到时给占位，不让列表断掉。
fn clock(ms: u64) -> String {
    let seconds = (ms / 1000) as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let filled = unsafe { libc::localtime_r(&seconds, &mut tm) };
    if filled.is_null() {
        return "--:--".to_string();
    }
    format!("{:02}:{:02}", tm.tm_hour, tm.tm_min)
}

fn print_help() {
    println!(
        "\
用法：yi [-h] [-v]

  -h, --help    显示这份帮助
  -v, --version 显示版本

斜杠命令：
  /new                      归档当前会话，开始新的一条
  /plan                     切换计划模式：只读调查，不改文件
  /effort <档位>            设置推理强度：auto / low / high / max
  /switch <供应商>          切换供应商与模型：opencode-go / deepseek（记在会话里）
  /tree [序号]              查看会话岔路；带序号回到那条消息之前（旧路径保留）
  /skill:<名字> [要求]      把某个技能的正文注入这一轮
  /help                     显示这份帮助

按键：
  Ctrl-G                    把剪贴板里的图片挂到这一轮
                            （Cmd-V 仍是终端自己的文本粘贴，两者互不影响）

环境变量（密钥只从这里读）：
  OPENCODE_API_KEY     OpenCode Go 的密钥（默认供应商）
  DEEPSEEK_API_KEY     DeepSeek 官方的密钥（provider 设为 deepseek 时）
  YI_HOME              数据目录，默认 ~/.yi

配置文件 ~/.yi/config.json 可选字段：provider、base_url、model、effort。
  provider 选预置：opencode-go（默认）或 deepseek。
"
    );
}

/// 启动时把上次的对话放出来：最近三轮，最多两百行。
///
/// 只放模型当前还记得的部分（压缩边界之后），免得它「忘了」屏幕上却还写着。
fn print_replay(agent: &Agent, color: bool) {
    use yi::session::Replay;

    let items = agent.replay(3);
    if items.is_empty() {
        return;
    }
    println!("{}", render::dim_line("—— 上次的对话 ——", color));
    if agent.compacted() {
        println!(
            "{}",
            render::dim_line("早前对话已压缩，更早的内容不再显示", color)
        );
    }

    // 行数预算：从后往前留，超出的只丢前面的。
    const BUDGET: usize = 200;
    let mut used = 0;
    let mut keep_from = 0;
    for (index, item) in items.iter().enumerate().rev() {
        used += match item {
            // 用户那一条还要上下各留一行底色，按多算两行。
            Replay::User(text) => text.lines().count() + 4,
            Replay::Assistant(text) => text.lines().count() + 2,
            Replay::Tool(_) => 1,
        };
        if used > BUDGET {
            keep_from = index + 1;
            break;
        }
    }
    // 一条消息就超过预算时也要留一条，不能什么都不显示。
    keep_from = keep_from.min(items.len().saturating_sub(1));
    if keep_from > 0 {
        println!(
            "{}",
            render::dim_line(&format!("（前面还有 {keep_from} 条没显示）"), color)
        );
    }

    for item in &items[keep_from..] {
        match item {
            Replay::User(text) => println!("> {text}"),
            Replay::Assistant(text) => {
                // 每条回答单独一个渲染器：代码块、表格的状态不跨条继承。
                let mut renderer = Renderer::new(color);
                let out = format!("{}{}", renderer.push(text), renderer.finish());
                print!("{out}");
                if !out.ends_with('\n') {
                    println!();
                }
            }
            // 工具调用也不重放：与实时输出一样，只看人和模型的对话。
            Replay::Tool(_) => {}
        }
    }
    println!();
}

/// 页脚一行：项目、分支、上下文占比、供应商、模型。
///
/// 缓存与本轮用量是内部数据，只写进会话文件，不占屏幕。
fn footer(
    usage: &Usage,
    window: u64,
    place: &str,
    branch: Option<&str>,
    plan: bool,
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
    // 计划模式是个持续状态，页脚常驻提示，免得忘了自己还在只读里。
    if plan {
        out.push_str(" · plan");
    }
    // 缓存命中率按本轮累计的输入算：命中多少、未命中多少，一眼看得出前缀有没有被保住。
    let input = usage.cache_hit + usage.input_uncached;
    match (usage.cache_hit * 100).checked_div(input) {
        Some(cached) => out.push_str(&format!(" · {percent}% · 缓存 {cached}%")),
        None => out.push_str(&format!(" · {percent}%")),
    }
    out.push_str(&format!(" · {provider} · {model}]"));
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

/// 终端标题里工作状态的转圈字符，和 omp 用同一套盲文点。
const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

/// 把运行状态写进终端标题：干活时转圈，轮到你时 `>`，出错时 `!`。
///
/// 与 OSC 7501 是两条路：那条交给终端自己画图标，这条是文本，标签页和标题栏直接看得到。
struct Title {
    enabled: bool,
    project: String,
    working: bool,
    failed: bool,
    frame: usize,
    last_draw: std::time::Instant,
}

impl Title {
    fn new(enabled: bool, project: String) -> Self {
        let mut title = Self {
            enabled,
            project,
            working: false,
            failed: false,
            frame: 0,
            last_draw: std::time::Instant::now(),
        };
        title.draw();
        title
    }

    /// 一轮开始：从第一帧转起。
    fn start_working(&mut self) {
        self.working = true;
        self.failed = false;
        self.frame = 0;
        self.draw();
    }

    /// 干活期间每 100 毫秒换一帧；再快只是白写字节。
    fn spin(&mut self) {
        if !self.working || self.last_draw.elapsed().as_millis() < 100 {
            return;
        }
        self.frame = self.frame.wrapping_add(1);
        self.draw();
    }

    /// 轮到你说话。
    fn ready(&mut self) {
        if !self.working && !self.failed {
            return;
        }
        self.working = false;
        self.failed = false;
        self.draw();
    }

    /// 这一轮失败了。
    fn failed(&mut self) {
        if self.failed && !self.working {
            return;
        }
        self.working = false;
        self.failed = true;
        self.draw();
    }

    fn draw(&mut self) {
        if !self.enabled {
            return;
        }
        let mark = if self.working {
            SPINNER[self.frame % SPINNER.len()]
        } else if self.failed {
            '!'
        } else {
            '>'
        };
        print!("{}", title_sequence(mark, &self.project));
        let _ = io::stdout().flush();
        self.last_draw = std::time::Instant::now();
    }
}

/// 一条设标题的 OSC 0 序列（以 BEL 结尾，老终端也认）。
fn title_sequence(mark: char, project: &str) -> String {
    format!("\x1b]0;yi {mark} {project}\x07")
}

/// 处理中的单行状态：阶段名 + 已耗时，用回车回到行首原地重画。
///
/// 行式界面的底线仍是「只往下打印」，这里唯一的例外就是这一行：它随时会被擦掉，
/// 不留下历史。见 `docs/adr/0009-progress-line-in-place.md`。
struct Status {
    /// 只有真终端才动；管道、重定向里一个字节都不写。
    enabled: bool,
    /// 上色时整行压暗，避免和正文抢注意力。
    dim: bool,
    stage: Option<(String, std::time::Instant)>,
    /// 这一行现在是否真的画在屏幕上：擦之前得先知道有没有东西可擦。
    drawn: bool,
    last_draw: std::time::Instant,
}

impl Status {
    fn new(enabled: bool, dim: bool) -> Self {
        Self {
            enabled,
            dim,
            stage: None,
            drawn: false,
            last_draw: std::time::Instant::now(),
        }
    }

    fn label(&self) -> Option<&str> {
        self.stage.as_ref().map(|(label, _)| label.as_str())
    }

    /// 换阶段并立刻画一次；计时从这一刻重新开始。
    ///
    /// 阶段一变就顺手告诉终端（OSC 7501）：状态行给人看，程序状态给 Rex 这类
    /// 终端看，两者说的是同一件事。
    fn set(&mut self, stage: &str) {
        let changed = self.label() != Some(stage);
        self.stage = Some((stage.to_string(), std::time::Instant::now()));
        self.draw();
        if changed {
            program_status::working(stage);
        }
    }

    /// 每 200 毫秒最多重画一次：秒数在跳，但不值得为它刷屏。
    ///
    /// 等待期间你可能在打下一句话，草稿变了也要重画，所以条件不只「有阶段」。
    fn tick(&mut self) {
        let busy = self.stage.is_some() || !yi::cancel::draft_text().is_empty();
        if busy && self.last_draw.elapsed().as_millis() >= 100 {
            self.draw();
        }
    }

    /// 擦掉状态行并把它交回来，打印完别的内容再 `resume` 回去。
    fn clear(&mut self) -> Option<(String, std::time::Instant)> {
        let stage = self.stage.take();
        if self.drawn {
            print!("\r\x1b[K");
            self.drawn = false;
            let _ = io::stdout().flush();
        }
        stage
    }

    fn resume(&mut self, stage: Option<(String, std::time::Instant)>) {
        self.stage = stage;
        self.draw();
    }

    fn draw(&mut self) {
        if !self.enabled {
            return;
        }
        let draft = yi::cancel::draft_text();
        let line = match &self.stage {
            Some((label, started)) => {
                let width = render::terminal_width().unwrap_or(80);
                status_line(
                    label,
                    started.elapsed().as_secs_f32(),
                    self.dim,
                    &draft,
                    width,
                )
            }
            None if draft.is_empty() => {
                if self.drawn {
                    print!("\r\x1b[K");
                    self.drawn = false;
                    let _ = io::stdout().flush();
                }
                return;
            }
            // 流式输出期间没有阶段，只把草稿挂在行尾，你才知道字进去了。
            None => {
                let width = render::terminal_width().unwrap_or(80);
                let mut line = String::from("\r\x1b[K");
                if self.dim {
                    line.push_str("\x1b[2m");
                }
                line.push_str("> ");
                line.push_str(&tail_cells(&draft, width.saturating_sub(3)));
                if self.dim {
                    line.push_str("\x1b[0m");
                }
                line
            }
        };
        print!("{line}");
        self.drawn = true;
        let _ = io::stdout().flush();
        self.last_draw = std::time::Instant::now();
    }
}

/// 状态行的可见内容：回车回到行首、清掉整行，再写新的。
///
/// 行尾挂你轮内敲的草稿，随状态行一起重画；宽度不够时只留草稿的尾巴，
/// 因为最新的字在最右边。
fn status_line(label: &str, secs: f32, dim: bool, draft: &str, width: usize) -> String {
    let head = format!("{label} {secs:.1}s");
    let mut out = String::from("\r\x1b[K");
    if dim {
        out.push_str("\x1b[2m");
    }
    out.push_str(&head);
    if !draft.is_empty() {
        // 留一格给行尾：写满整行会触发终端折行。
        let budget = width.saturating_sub(render::display_width(&head) + 3 + 2 + 1);
        if budget >= 6 {
            out.push_str(" · > ");
            out.push_str(&tail_cells(draft, budget));
        }
    }
    if dim {
        out.push_str("\x1b[0m");
    }
    out
}

/// 取显示宽度的末尾若干格；截过就加省略号。
fn tail_cells(text: &str, cells: usize) -> String {
    let mut width = 0;
    let mut kept: Vec<char> = Vec::new();
    for ch in text.chars().rev() {
        let w = render::display_width(&ch.to_string());
        if width + w > cells {
            break;
        }
        width += w;
        kept.push(ch);
    }
    let tail: String = kept.into_iter().rev().collect();
    if tail.chars().count() < text.chars().count() {
        format!("…{tail}")
    } else {
        tail
    }
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
    fn the_title_carries_the_state_glyph() {
        assert_eq!(
            title_sequence('>', "fm-openapi"),
            "\x1b]0;yi > fm-openapi\x07"
        );
        assert_eq!(
            title_sequence('\u{280b}', "fm-openapi"),
            "\x1b]0;yi \u{280b} fm-openapi\x07"
        );
    }

    #[test]
    fn the_status_line_rewrites_itself_in_place() {
        let plain = status_line("等待模型", 3.24, false, "", 80);
        assert_eq!(plain, "\r\x1b[K等待模型 3.2s");
        let dim = status_line("运行 bash", 12.46, true, "", 80);
        assert_eq!(dim, "\r\x1b[K\x1b[2m运行 bash 12.5s\x1b[0m");
    }

    #[test]
    fn the_status_line_carries_the_draft_you_typed() {
        let line = status_line("等待模型", 3.2, false, "帮我看看这个", 80);
        assert_eq!(line, "\r\x1b[K等待模型 3.2s · > 帮我看看这个");
    }

    #[test]
    fn a_long_draft_keeps_its_tail_because_that_is_where_you_type() {
        let draft = "一二三四五六七八九十".repeat(5);
        let line = status_line("等待模型", 3.2, false, &draft, 30);
        assert!(render::display_width(&line) <= 30, "不超宽：{line:?}");
        assert!(line.ends_with("九十"), "保留最新的字：{line:?}");
        assert!(line.contains('…'), "截断要有省略号：{line:?}");
        assert!(line.starts_with("\r\x1b[K等待模型 3.2s · > "), "{line:?}");
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
            false,
            "OpenCode Go",
            "DeepSeek V4.1 Flash",
        );
        assert_eq!(
            line,
            "[~/gp/yi main · 50% · 缓存 50% · OpenCode Go · DeepSeek V4.1 Flash]"
        );
    }

    #[test]
    fn footer_shows_the_cache_hit_rate_of_the_turn() {
        let line = footer(
            &usage(100, 9_000, 1_000),
            1_048_576,
            "~/gp/yi",
            None,
            false,
            "DeepSeek",
            "DeepSeek Flash",
        );
        assert!(line.contains("缓存 10%"), "命中 1000 / 输入 10000：{line}");
    }

    #[test]
    fn footer_omits_the_branch_when_there_is_none() {
        let line = footer(
            &usage(0, 0, 0),
            1_048_576,
            "~/gp/yi",
            None,
            false,
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
