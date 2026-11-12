use std::path::PathBuf;

use serde_json::Value;

use crate::{Error, Result};

pub const DEFAULT_BASE_URL: &str = "https://api.deepseek.com";
pub const DEFAULT_MODEL: &str = "deepseek-flash";
/// `deepseek-flash` 的上下文窗口，用来算页脚里的占用百分比。
pub const CONTEXT_WINDOW: u64 = 1_048_576;
/// 默认系统提示词，编译期内嵌进二进制，是唯一来源。
pub const DEFAULT_SYSTEM_PROMPT: &str = include_str!("../prompts/system.md");

#[derive(Debug, Clone)]
pub struct Config {
    pub api_key: String,
    pub base_url: String,
    pub model: String,
    /// `None` = `auto`，请求里不发送 `reasoning_effort`。
    pub effort: Option<String>,
    pub home: PathBuf,
    /// 语言服务器命令，默认取 PATH 里的 gopls / phpantom_lsp。
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

        let mut cfg = Config {
            api_key: String::new(),
            base_url: DEFAULT_BASE_URL.to_string(),
            model: DEFAULT_MODEL.to_string(),
            effort: None,
            home: home.clone(),
            gopls: "gopls".to_string(),
            phpantom: "phpantom_lsp".to_string(),
        };

        let path = home.join("config.json");
        if path.exists() {
            let text = std::fs::read_to_string(&path)?;
            let value: Value = serde_json::from_str(&text)?;
            apply_json(&mut cfg, &value)?;
        }

        if let Ok(s) = std::env::var("DEEPSEEK_API_KEY")
            && !s.trim().is_empty()
        {
            cfg.api_key = s;
        }

        if cfg.api_key.trim().is_empty() {
            return Err(Error::Config(format!(
                "缺少 API key。设置环境变量 DEEPSEEK_API_KEY，或在 {} 里写 {{\"api_key\":\"...\"}}",
                path.display()
            )));
        }

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
}

/// 把配置文件里的字段覆盖到默认值上；缺的字段保持默认。
fn apply_json(cfg: &mut Config, value: &Value) -> Result<()> {
    for key in ["api_key", "base_url", "model", "gopls", "phpantom"] {
        if let Some(text) = value.get(key).and_then(Value::as_str) {
            match key {
                "api_key" => cfg.api_key = text.to_string(),
                "base_url" => cfg.base_url = text.to_string(),
                "model" => cfg.model = text.to_string(),
                "gopls" => cfg.gopls = text.to_string(),
                _ => cfg.phpantom = text.to_string(),
            }
        }
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
    fn config_json_can_override_the_language_servers() {
        let mut cfg = Config {
            api_key: "k".into(),
            base_url: DEFAULT_BASE_URL.into(),
            model: DEFAULT_MODEL.into(),
            effort: None,
            home: PathBuf::from("/tmp"),
            gopls: "gopls".into(),
            phpantom: "phpantom_lsp".into(),
        };
        apply_json(
            &mut cfg,
            &serde_json::json!({"gopls": "/opt/homebrew/bin/gopls", "phpantom": "/usr/local/bin/phpantom_lsp"}),
        )
        .unwrap();
        assert_eq!(cfg.gopls, "/opt/homebrew/bin/gopls");
        assert_eq!(cfg.phpantom, "/usr/local/bin/phpantom_lsp");

        apply_json(&mut cfg, &serde_json::json!({})).unwrap();
        assert_eq!(cfg.gopls, "/opt/homebrew/bin/gopls", "缺字段时保持原值");
    }

    #[test]
    fn project_encoding_is_readable_and_unique() {
        let a = encode_project("/Users/xsh/fm/fm-cut");
        assert!(a.starts_with("Users-xsh-fm-fm-cut-"));
        assert_ne!(a, encode_project("/Users/xsh/fm/fm-cut2"));
    }
}
