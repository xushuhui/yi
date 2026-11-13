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

pub const PRESETS: [Preset; 2] = [
    Preset {
        id: "opencode-go",
        display: "OpenCode Go",
        base_url: "https://opencode.ai/zen/go/v1",
        model: "deepseek-v4.1-flash",
        model_display: "DeepSeek V4.1 Flash",
        // 与 DeepSeek 官方同一个模型版本（V4.1-Flash-1017），窗口相同。
        context_window: 1_048_576,
        env_key: "OPENCODE_GO_API_KEY",
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
    /// `None` = `auto`，请求里不发送 `reasoning_effort`。
    pub effort: Option<String>,
    pub home: PathBuf,
    /// 语言服务器命令：固定取 PATH 里的同名程序，不从配置文件读；保留字段只为测试能换成假服务器。
    pub gopls: String,
    pub phpantom: String,
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
            effort: None,
            home: home.clone(),
            gopls: "gopls".to_string(),
            phpantom: "phpantom_lsp".to_string(),
        };
        apply_json(&mut cfg, &value)?;

        // 密钥只从环境变量读：配置文件里写了也不看。
        let key = std::env::var(chosen.env_key).unwrap_or_default();
        if key.trim().is_empty() {
            return Err(Error::Config(format!(
                "缺少 {} 的密钥。设置环境变量 {}",
                chosen.display, chosen.env_key
            )));
        }
        cfg.api_key = key.trim().to_string();

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
        append_context_file(
            &mut out,
            std::path::Path::new(cwd).join("AGENTS.md").as_path(),
        );

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
            effort: None,
            home: PathBuf::from("/tmp"),
            gopls: "gopls".into(),
            phpantom: "phpantom_lsp".into(),
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
        assert_eq!(chosen.env_key, "OPENCODE_GO_API_KEY");
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
            effort: None,
            home: PathBuf::from("/tmp"),
            gopls: "gopls".into(),
            phpantom: "phpantom_lsp".into(),
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
            effort: None,
            home: PathBuf::from("/tmp"),
            gopls: "gopls".into(),
            phpantom: "phpantom_lsp".into(),
        };
        assert_eq!(cfg.compact_at_tokens(), 524_288);
        cfg.context_window = 1_000_000;
        assert_eq!(cfg.compact_at_tokens(), 500_000);
    }

    #[test]
    fn project_encoding_is_readable_and_unique() {
        let a = encode_project("/Users/xsh/fm/fm-cut");
        assert!(a.starts_with("Users-xsh-fm-fm-cut-"));
        assert_ne!(a, encode_project("/Users/xsh/fm/fm-cut2"));
    }
}
