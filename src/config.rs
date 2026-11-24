use std::path::PathBuf;

use serde_json::Value;

use crate::{Error, Result};

/// 没有指定供应商时用这一套。
pub const DEFAULT_PROVIDER: &str = "opencode-go";

/// 一套内置供应商预置：地址、默认模型、上下文窗口、密钥所在的环境变量。
///
/// 密钥只从环境变量读，配置文件不保存密钥——配置文件会被同步、备份、误提交。
#[derive(Debug)]
pub struct Preset {
    /// 配置文件里 `provider` 字段的取值。
    pub id: &'static str,
    /// 给人看和放进页脚的名字。
    pub display: &'static str,
    pub base_url: &'static str,
    pub model: &'static str,
    /// 默认模型给人看的名字；模型被覆盖时退回用模型 id。
    pub model_display: &'static str,
    /// 上下文窗口，按官方文档的精确值：1M 指的是 1,048,576（2 的 20 次方），
    /// 而且是输入与输出合计，所以按一半触发压缩离上限很远。
    pub context_window: u64,
    pub env_key: &'static str,
}

/// DeepSeek 官方的搜索接口：和聊天用的是同一把 key，按次计费。
pub const SEARCH_URL: &str = "https://api.deepseek.com/v1/tools/search";

pub const PRESETS: [Preset; 2] = [
    Preset {
        id: "opencode-go",
        display: "OpenCode Go",
        base_url: "https://opencode.ai/zen/go/v1",
        model: "deepseek-v4.1-flash",
        model_display: "DeepSeek V4.1 Flash",
        // 与 DeepSeek 官方同一个模型版本（V4.1-Flash-1017），窗口相同。
        context_window: 1_048_576,
        env_key: "OPENCODE_API_KEY",
    },
    Preset {
        id: "deepseek",
        display: "DeepSeek",
        base_url: "https://api.deepseek.com",
        model: "deepseek-flash",
        model_display: "DeepSeek Flash",
        context_window: 1_048_576,
        env_key: "DEEPSEEK_API_KEY",
    },
];

/// 按 id 取预置；未知 id 直接报错，不去猜。
fn preset(id: &str) -> Result<&'static Preset> {
    PRESETS.iter().find(|p| p.id == id).ok_or_else(|| {
        let known: Vec<&str> = PRESETS.iter().map(|p| p.id).collect();
        Error::Config(format!("未知的 provider {id}；可选 {}", known.join(" / ")))
    })
}
/// 默认系统提示词，编译期内嵌进二进制，是唯一来源。
pub const DEFAULT_SYSTEM_PROMPT: &str = include_str!("../prompts/system.md");

#[derive(Debug, Clone)]
pub struct Config {
    pub api_key: String,
    /// 预置 id，例如 `opencode-go`。
    pub provider: &'static str,
    /// 供应商给人看的名字，例如 `OpenCode Go`。
    pub display: &'static str,
    pub base_url: String,
    pub model: String,
    /// 模型给人看的名字，进页脚。
    pub model_display: String,
    /// 当前模型的上下文窗口，页脚百分比与压缩阈值都按它算。
    pub context_window: u64,
    /// 出网代理；`None` 表示直连。只在启动时解析一次。
    pub proxy: Option<ureq::Proxy>,
    /// `None` = `auto`，请求里不发送 `reasoning_effort`。
    pub effort: Option<String>,
    pub home: PathBuf,
    /// 搜索用的密钥：固定读 `DEEPSEEK_API_KEY`，与聊天走哪家供应商无关。
    /// `None` 表示没配，`web_search` 不会进工具表。
    pub search_key: Option<String>,
    /// 搜索接口地址；固定指向 DeepSeek 官方，测试里换成假服务器。
    pub search_url: String,
    /// 语言服务器命令：固定取 PATH 里的同名程序，不从配置文件读；保留字段只为测试能换成假服务器。
    pub gopls: String,
    pub phpantom: String,
    pub rust_analyzer: String,
}

impl Config {
    /// 读取顺序：内置默认值 → `~/.yi/config.json` → 环境变量。
    pub fn load() -> Result<Config> {
        let home = std::env::var_os("YI_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                let base = std::env::var_os("HOME")
                    .map(PathBuf::from)
                    .unwrap_or_default();
                base.join(".yi")
            });

        let path = home.join("config.json");
        let value: Value = if path.exists() {
            serde_json::from_str(&std::fs::read_to_string(&path)?)?
        } else {
            Value::Null
        };

        let id = value
            .get("provider")
            .and_then(Value::as_str)
            .unwrap_or(DEFAULT_PROVIDER);
        let chosen = preset(id)?;
        let mut cfg = Config {
            api_key: String::new(),
            provider: chosen.id,
            display: chosen.display,
            base_url: chosen.base_url.to_string(),
            model: chosen.model.to_string(),
            model_display: chosen.model_display.to_string(),
            context_window: chosen.context_window,
            proxy: None,
            effort: None,
            search_key: None,
            search_url: SEARCH_URL.to_string(),
            home: home.clone(),
            gopls: "gopls".to_string(),
            phpantom: "phpantom_lsp".to_string(),
            rust_analyzer: "rust-analyzer".to_string(),
        };
        apply_json(&mut cfg, &value)?;
        cfg.proxy = resolve_proxy(cfg.provider, &cfg.base_url)?;

        // 密钥只从环境变量读：配置文件里写了也不看。
        let key = std::env::var(chosen.env_key).unwrap_or_default();
        if key.trim().is_empty() {
            return Err(Error::Config(format!(
                "缺少 {} 的密钥。设置环境变量 {}",
                chosen.display, chosen.env_key
            )));
        }
        cfg.api_key = key.trim().to_string();

        // 搜索密钥与聊天供应商解耦：聊天走 opencode-go 时也可能配了 DeepSeek 的 key。
        cfg.search_key = std::env::var("DEEPSEEK_API_KEY")
            .ok()
            .map(|key| key.trim().to_string())
            .filter(|key| !key.is_empty());

        Ok(cfg)
    }

    /// 换成另一个预置：通道、模型、窗口、密钥都跟着预置走，其余本机设置照旧。
    ///
    /// 密钥缺失当场报错——不留一个「每句话都失败」的会话状态。
    pub fn with_provider(&self, id: &str) -> Result<Config> {
        let chosen = preset(id)?;
        let key = std::env::var(chosen.env_key).unwrap_or_default();
        if key.trim().is_empty() {
            return Err(Error::Config(format!(
                "缺少 {} 的密钥。设置环境变量 {}",
                chosen.display, chosen.env_key
            )));
        }
        let mut cfg = self.clone();
        cfg.provider = chosen.id;
        cfg.display = chosen.display;
        cfg.base_url = chosen.base_url.to_string();
        cfg.model = chosen.model.to_string();
        cfg.model_display = chosen.model_display.to_string();
        cfg.context_window = chosen.context_window;
        cfg.api_key = key.trim().to_string();
        cfg.proxy = resolve_proxy(cfg.provider, &cfg.base_url)?;
        Ok(cfg)
    }

    /// 系统提示词：正文在仓库的 `prompts/system.md`，编译期内嵌；
    /// 之后拼接用户级与项目级 `AGENTS.md`。
    /// 一条会话里只在开头计算一次，之后不变，保证前缀缓存。
    pub fn system_prompt(&self, cwd: &str) -> String {
        let mut out = DEFAULT_SYSTEM_PROMPT.to_string();
        if !out.ends_with('\n') {
            out.push('\n');
        }

        append_context_file(&mut out, &self.home.join("AGENTS.md"));
        if let Some(path) = nearest_agents_file(std::path::Path::new(cwd), &self.home) {
            append_context_file(&mut out, &path);
        }

        // 技能只列名字和描述，正文按需读取；会话开始时算一次，保证前缀稳定。
        let discovered =
            crate::skills::discover(&crate::skills::roots(&self.home, std::path::Path::new(cwd)));
        out.push_str(&crate::skills::render(&discovered));

        out
    }

    /// 上下文涨到这个量就在本轮结束后压缩：窗口的一半。
    pub fn compact_at_tokens(&self) -> u64 {
        self.context_window / 2
    }
}

/// 按 omp 的规则给供应商取代理变量名：`YI_PROXY_OPENCODE_GO`。
fn provider_proxy_var(id: &str) -> String {
    let mut name = String::from("YI_PROXY_");
    for ch in id.chars() {
        if ch.is_ascii_alphanumeric() {
            name.push(ch.to_ascii_uppercase());
        } else {
            name.push('_');
        }
    }
    name
}

/// 启动时解析一次代理：本机与内网直连，`NO_PROXY` 命中直连，
/// 然后是 `YI_PROXY_<供应商>`，最后是 `YI_PROXY`。
fn resolve_proxy(provider: &str, base_url: &str) -> Result<Option<ureq::Proxy>> {
    let host = host_of(base_url);
    let no_proxy = std::env::var("NO_PROXY")
        .ok()
        .or_else(|| std::env::var("no_proxy").ok());
    let raw = choose_proxy(provider, &host, no_proxy.as_deref(), |key| {
        std::env::var(key).ok()
    })?;
    let Some(raw) = raw else {
        return Ok(None);
    };
    ureq::Proxy::new(&raw)
        .map(Some)
        .map_err(|err| Error::Config(format!("代理地址无效（{raw}）：{err}")))
}

/// 纯逻辑部分，方便单独测：返回该用的代理地址，`None` 表示直连。
fn choose_proxy(
    provider: &str,
    host: &str,
    no_proxy: Option<&str>,
    lookup: impl Fn(&str) -> Option<String>,
) -> Result<Option<String>> {
    // 本机回环、内网私有地址一律直连：给它们套代理由害无益。
    if host.is_empty() || is_private_host(host) || no_proxy_covers(no_proxy, host) {
        return Ok(None);
    }
    let raw = lookup(&provider_proxy_var(provider))
        .or_else(|| lookup("YI_PROXY"))
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    let Some(raw) = raw else {
        return Ok(None);
    };
    if raw.to_ascii_lowercase().starts_with("socks") {
        return Err(Error::Config(format!(
            "不支持 SOCKS 代理（{raw}）；请写成 http://127.0.0.1:端口"
        )));
    }
    Ok(Some(raw))
}

/// 从接口地址里取主机名，够用即可：`https://host:port/path` → `host`。
fn host_of(base_url: &str) -> String {
    let rest = base_url
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(base_url);
    let authority = rest.split('/').next().unwrap_or("");
    let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    match host.rsplit_once(':') {
        Some((head, port)) if port.chars().all(|c| c.is_ascii_digit()) => head.to_string(),
        _ => host.to_string(),
    }
}

/// 本机与内网：回环、link-local、RFC 1918。
fn is_private_host(host: &str) -> bool {
    let host = host.trim_matches(['[', ']']).to_ascii_lowercase();
    if host == "localhost" || host == "::1" {
        return true;
    }
    let parts: Vec<&str> = host.split('.').collect();
    match parts.as_slice() {
        ["127", ..] => true,
        ["10", ..] => true,
        ["192", "168", ..] => true,
        ["169", "254", ..] => true,
        ["172", second, ..] => second.parse::<u8>().is_ok_and(|n| (16..=31).contains(&n)),
        _ => false,
    }
}

/// `NO_PROXY` 的匹配：`*` 全绕过、`.example.com` 与 `*.example.com` 匹配子域、
/// 其余按主机名精确比较（允许带端口写法）。
fn no_proxy_covers(no_proxy: Option<&str>, host: &str) -> bool {
    let Some(patterns) = no_proxy else {
        return false;
    };
    let host = host.to_ascii_lowercase();
    patterns.split(',').any(|pattern| {
        let pattern = pattern.trim().to_ascii_lowercase();
        if pattern.is_empty() {
            return false;
        }
        if pattern == "*" {
            return true;
        }
        let pattern = pattern.split(':').next().unwrap_or(&pattern);
        if let Some(suffix) = pattern
            .strip_prefix("*.")
            .or_else(|| pattern.strip_prefix('.'))
        {
            return host == suffix || host.ends_with(&format!(".{suffix}"));
        }
        host == pattern || host.ends_with(&format!(".{pattern}"))
    })
}

/// 把配置文件里的字段覆盖到预置上；缺的字段保持预置值。
///
/// `provider` 已在 `load` 里先读过（它决定用哪套预置），`api_key` 一律忽略：
/// 密钥只从环境变量读。
fn apply_json(cfg: &mut Config, value: &Value) -> Result<()> {
    if let Some(text) = value.get("base_url").and_then(Value::as_str) {
        cfg.base_url = text.to_string();
    }
    if let Some(text) = value.get("model").and_then(Value::as_str) {
        cfg.model = text.to_string();
        // 换过模型就不再假装它还叫预置里的那个名字。
        cfg.model_display = text.to_string();
    }
    if let Some(text) = value.get("effort").and_then(Value::as_str) {
        cfg.effort = parse_effort(text)?;
    }
    Ok(())
}

/// 仓库级规则：从启动目录往上找最近的 `AGENTS.md`，到家目录为止。
///
/// 在子目录启动时也能用上仓库根的规则；再往上就不看了，避免捡到别的项目的。
fn nearest_agents_file(cwd: &std::path::Path, home: &std::path::Path) -> Option<PathBuf> {
    let boundary = home.parent().unwrap_or(home);
    for dir in cwd.ancestors() {
        let candidate = dir.join("AGENTS.md");
        if candidate.is_file() {
            return Some(candidate);
        }
        if dir == boundary {
            break;
        }
    }
    None
}

fn append_context_file(out: &mut String, path: &std::path::Path) {
    if let Ok(text) = std::fs::read_to_string(path)
        && !text.trim().is_empty()
    {
        out.push_str("\n<context-file path=\"");
        out.push_str(&path.display().to_string());
        out.push_str("\">\n");
        out.push_str(&text);
        if !text.ends_with('\n') {
            out.push('\n');
        }
        out.push_str("</context-file>\n");
    }
}

/// `auto` 映射为 `None`（不发送）；其余三档原样发送。
pub fn parse_effort(raw: &str) -> Result<Option<String>> {
    match raw.trim() {
        "auto" | "" => Ok(None),
        "low" | "high" | "max" => Ok(Some(raw.trim().to_string())),
        other => Err(Error::Config(format!(
            "未知的推理强度 {other}；可选 auto / low / high / max"
        ))),
    }
}

/// 项目目录 → 会话目录名：路径里的 `/` 换成 `-`，末尾接 8 位路径哈希。
pub fn encode_project(cwd: &str) -> String {
    let trimmed = cwd.trim_start_matches('/').replace('/', "-");
    format!("{trimmed}-{:08x}", fnv1a(cwd.as_bytes()) as u32)
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effort_mapping_matches_the_api() {
        assert_eq!(parse_effort("auto").unwrap(), None);
        assert_eq!(parse_effort("low").unwrap().as_deref(), Some("low"));
        assert!(parse_effort("medium").is_err());
    }

    #[test]
    fn config_json_ignores_language_server_commands() {
        let mut cfg = Config {
            api_key: "k".into(),
            provider: "opencode-go",
            display: "OpenCode Go",
            base_url: "https://example.invalid".into(),
            model: "deepseek-v4.1-flash".into(),
            model_display: "DeepSeek V4.1 Flash".into(),
            context_window: 1_048_576,
            proxy: None,
            effort: None,
            search_key: None,
            search_url: SEARCH_URL.to_string(),
            home: PathBuf::from("/tmp"),
            gopls: "gopls".into(),
            phpantom: "phpantom_lsp".into(),
            rust_analyzer: "rust-analyzer".into(),
        };
        apply_json(&mut cfg, &serde_json::json!({"model": "deepseek-flash"})).unwrap();
        assert_eq!(cfg.model, "deepseek-flash");

        apply_json(
            &mut cfg,
            &serde_json::json!({"gopls": "/opt/homebrew/bin/gopls", "phpantom": "/usr/local/bin/phpantom_lsp"}),
        )
        .unwrap();
        assert_eq!(cfg.gopls, "gopls", "语言服务器固定走 PATH");
        assert_eq!(cfg.phpantom, "phpantom_lsp");
    }

    #[test]
    fn the_default_provider_is_opencode_go() {
        let chosen = preset(DEFAULT_PROVIDER).unwrap();
        assert_eq!(chosen.id, "opencode-go");
        assert_eq!(chosen.model, "deepseek-v4.1-flash");
        assert_eq!(chosen.env_key, "OPENCODE_API_KEY");
        assert_eq!(chosen.context_window, 1_048_576);
    }

    #[test]
    fn an_unknown_provider_is_rejected_instead_of_guessed() {
        let err = preset("nope").unwrap_err();
        assert!(format!("{err}").contains("opencode-go"), "{err}");
    }

    #[test]
    fn a_key_in_the_config_file_is_ignored() {
        let mut cfg = Config {
            api_key: String::new(),
            provider: "opencode-go",
            display: "OpenCode Go",
            base_url: "https://example.invalid".into(),
            model: "deepseek-v4.1-flash".into(),
            model_display: "DeepSeek V4.1 Flash".into(),
            context_window: 1_048_576,
            proxy: None,
            effort: None,
            search_key: None,
            search_url: SEARCH_URL.to_string(),
            home: PathBuf::from("/tmp"),
            gopls: "gopls".into(),
            phpantom: "phpantom_lsp".into(),
            rust_analyzer: "rust-analyzer".into(),
        };
        apply_json(
            &mut cfg,
            &serde_json::json!({"api_key": "sk-from-file", "model": "other-model"}),
        )
        .unwrap();
        assert!(cfg.api_key.is_empty(), "配置文件里的密钥必须被忽略");
        assert_eq!(cfg.model, "other-model");
        assert_eq!(
            cfg.model_display, "other-model",
            "换过模型就用模型 id 当显示名"
        );
    }

    #[test]
    fn the_compaction_point_follows_the_window() {
        let mut cfg = Config {
            api_key: "k".into(),
            provider: "opencode-go",
            display: "OpenCode Go",
            base_url: "https://example.invalid".into(),
            model: "deepseek-v4.1-flash".into(),
            model_display: "DeepSeek V4.1 Flash".into(),
            context_window: 1_048_576,
            proxy: None,
            effort: None,
            search_key: None,
            search_url: SEARCH_URL.to_string(),
            home: PathBuf::from("/tmp"),
            gopls: "gopls".into(),
            phpantom: "phpantom_lsp".into(),
            rust_analyzer: "rust-analyzer".into(),
        };
        assert_eq!(cfg.compact_at_tokens(), 524_288);
        cfg.context_window = 1_000_000;
        assert_eq!(cfg.compact_at_tokens(), 500_000);
    }

    #[test]
    fn the_provider_proxy_variable_follows_the_omp_naming() {
        assert_eq!(provider_proxy_var("opencode-go"), "YI_PROXY_OPENCODE_GO");
        assert_eq!(provider_proxy_var("deepseek"), "YI_PROXY_DEEPSEEK");
    }

    #[test]
    fn the_provider_specific_proxy_wins_over_the_generic_one() {
        let lookup = |key: &str| match key {
            "YI_PROXY_OPENCODE_GO" => Some("http://127.0.0.1:7891".into()),
            "YI_PROXY" => Some("http://127.0.0.1:7890".into()),
            _ => None,
        };
        let chosen = choose_proxy("opencode-go", "opencode.ai", None, lookup).unwrap();
        assert_eq!(chosen.as_deref(), Some("http://127.0.0.1:7891"));

        // 换一个供应商，就用不到那条专用变量，落到通用的。
        let chosen = choose_proxy("deepseek", "api.deepseek.com", None, lookup).unwrap();
        assert_eq!(chosen.as_deref(), Some("http://127.0.0.1:7890"));
    }

    #[test]
    fn loopback_and_lan_hosts_bypass_the_proxy() {
        let lookup = |_: &str| Some("http://127.0.0.1:7891".to_string());
        for host in [
            "127.0.0.1",
            "localhost",
            "192.168.1.9",
            "10.0.0.2",
            "172.16.5.1",
        ] {
            let chosen = choose_proxy("opencode-go", host, None, lookup).unwrap();
            assert_eq!(chosen, None, "{host} 不该走代理");
        }
    }

    #[test]
    fn no_proxy_matches_exact_suffix_and_port_forms() {
        let covers = |pattern: &str, host: &str| no_proxy_covers(Some(pattern), host);
        assert!(covers("opencode.ai", "opencode.ai"));
        assert!(
            covers("opencode.ai", "api.opencode.ai"),
            "无点前缀也匹配子域"
        );
        assert!(covers(".opencode.ai", "api.opencode.ai"));
        assert!(covers("*.opencode.ai", "api.opencode.ai"));
        assert!(covers("opencode.ai:443", "opencode.ai"), "允许带端口写法");
        assert!(covers("*", "anything.example"));
        assert!(!covers("opencode.ai", "notopencode.ai"));
        assert!(!covers("", "opencode.ai"));
    }

    #[test]
    fn a_socks_proxy_is_rejected_instead_of_silently_direct() {
        let lookup = |_: &str| Some("socks5://127.0.0.1:7890".to_string());
        let err = choose_proxy("opencode-go", "opencode.ai", None, lookup).unwrap_err();
        assert!(format!("{err}").contains("SOCKS"), "{err}");
    }

    #[test]
    fn the_host_is_taken_from_the_base_url() {
        assert_eq!(host_of("https://opencode.ai/zen/go/v1"), "opencode.ai");
        assert_eq!(host_of("http://127.0.0.1:8080"), "127.0.0.1");
        assert_eq!(host_of("https://user@example.com:8443/v1"), "example.com");
    }

    fn config_at(home: &std::path::Path) -> Config {
        Config {
            api_key: "k".into(),
            provider: "opencode-go",
            display: "OpenCode Go",
            base_url: "https://example.invalid".into(),
            model: "deepseek-v4.1-flash".into(),
            model_display: "DeepSeek V4.1 Flash".into(),
            context_window: 1_048_576,
            proxy: None,
            effort: None,
            search_key: None,
            search_url: SEARCH_URL.to_string(),
            home: home.to_path_buf(),
            gopls: "gopls".into(),
            phpantom: "phpantom_lsp".into(),
            rust_analyzer: "rust-analyzer".into(),
        }
    }

    fn temp_tree(tag: &str) -> (PathBuf, PathBuf) {
        let root = std::env::temp_dir().join(format!("yi-{tag}-{}", std::process::id()));
        let home = root.join("data");
        std::fs::create_dir_all(&home).unwrap();
        (root, home)
    }

    #[test]
    fn both_rule_files_are_injected_with_the_user_one_first() {
        let (root, home) = temp_tree("rules");
        let project = root.join("proj");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(home.join("AGENTS.md"), "用户级规则").unwrap();
        std::fs::write(project.join("AGENTS.md"), "仓库级规则").unwrap();

        let prompt = config_at(&home).system_prompt(&project.to_string_lossy());
        let user = prompt.find("用户级规则").expect("用户级规则要进提示词");
        let repo = prompt.find("仓库级规则").expect("仓库级规则要进提示词");
        assert!(user < repo, "用户级在前、仓库级在后");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_nested_directory_finds_the_repository_rules_above_it() {
        let (root, home) = temp_tree("rules-nested");
        let project = root.join("proj");
        let deep = project.join("src/inner");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(project.join("AGENTS.md"), "仓库级规则").unwrap();

        let prompt = config_at(&home).system_prompt(&deep.to_string_lossy());
        assert!(
            prompt.contains("仓库级规则"),
            "子目录启动要能找到仓库根的规则：{prompt}"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn the_nearest_rule_file_wins() {
        let (root, home) = temp_tree("rules-nearest");
        let project = root.join("proj");
        let deep = project.join("pkg");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(project.join("AGENTS.md"), "外层规则").unwrap();
        std::fs::write(deep.join("AGENTS.md"), "内层规则").unwrap();

        let prompt = config_at(&home).system_prompt(&deep.to_string_lossy());
        assert!(prompt.contains("内层规则"), "{prompt}");
        assert!(!prompt.contains("外层规则"), "只取最近的一个：{prompt}");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn project_encoding_is_readable_and_unique() {
        let a = encode_project("/Users/xsh/fm/fm-cut");
        assert!(a.starts_with("Users-xsh-fm-fm-cut-"));
        assert_ne!(a, encode_project("/Users/xsh/fm/fm-cut2"));
    }
}
