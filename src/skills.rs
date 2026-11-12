use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::{Error, Result};

/// 一个技能：装 `SKILL.md` 的目录。
pub struct Skill {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
    /// `disable-model-invocation: true` 的技能不出现在系统提示词里，只能由用户用 `/skill:` 调用。
    pub model_invocable: bool,
}

/// 技能目录的查找顺序，先找到的同名技能优先。
/// `home` 是数据目录（默认 `~/.yi`）；它的同级 `.agents/skills` 是既有约定的共享位置。
pub fn roots(home: &Path, cwd: &Path) -> Vec<PathBuf> {
    let mut roots = vec![cwd.join(".yi/skills"), home.join("skills")];
    if let Some(parent) = home.parent() {
        roots.push(parent.join(".agents/skills"));
    }
    roots
}

/// 扫描每个根目录下一层的 `<名字>/SKILL.md`。
pub fn discover(roots: &[PathBuf]) -> Vec<Skill> {
    let mut found: Vec<Skill> = Vec::new();
    for root in roots {
        let Ok(entries) = std::fs::read_dir(root) else {
            continue;
        };
        let mut dirs: Vec<PathBuf> = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.is_dir())
            .collect();
        dirs.sort();

        for dir in dirs {
            let file = dir.join("SKILL.md");
            let Ok(text) = std::fs::read_to_string(&file) else {
                continue;
            };
            let meta = frontmatter(&text);
            let Some(description) = meta.get("description").cloned() else {
                continue;
            };
            let name = meta.get("name").cloned().unwrap_or_else(|| {
                dir.file_name()
                    .map(|name| name.to_string_lossy().to_string())
                    .unwrap_or_default()
            });
            if found.iter().any(|skill| skill.name == name) {
                continue;
            }
            let model_invocable = meta
                .get("disable-model-invocation")
                .map(|value| value != "true")
                .unwrap_or(true);
            found.push(Skill {
                name,
                description,
                path: file,
                model_invocable,
            });
        }
    }
    found
}

/// 系统提示词里只放名字、描述和正文路径，正文按需读取。
pub fn render(skills: &[Skill]) -> String {
    let visible: Vec<&Skill> = skills.iter().filter(|s| s.model_invocable).collect();
    if visible.is_empty() {
        return String::new();
    }
    let mut out = String::from(
        "\n<available-skills>\n需要某个技能时，用 read 读它的正文，按里面的步骤做。\n",
    );
    for skill in visible {
        out.push_str(&format!(
            "- {}：{}（正文：{}）\n",
            skill.name,
            skill.description,
            skill.path.display()
        ));
    }
    out.push_str("</available-skills>\n");
    out
}

/// `/skill:<名字> [要求]` 展开成一条普通消息，作为用户输入提交。
pub fn expand(input: &str, home: &Path, cwd: &Path) -> Result<String> {
    // `/skill: name` 和 `/skill:name` 等价：冒号后面允许空格。
    let mut parts = input.trim_start().splitn(2, char::is_whitespace);
    let name = parts.next().unwrap_or("").trim();
    let request = parts.next().unwrap_or("").trim();
    if name.is_empty() {
        return Err(Error::Config("用法：/skill:<名字> [要求]".to_string()));
    }

    let skills = discover(&roots(home, cwd));
    let Some(skill) = skills.iter().find(|skill| skill.name == name) else {
        let names: Vec<&str> = skills.iter().map(|skill| skill.name.as_str()).collect();
        return Err(Error::Config(format!(
            "没有叫 {name} 的技能。可用的有：{}",
            names.join("、")
        )));
    };
    let text = std::fs::read_to_string(&skill.path)?;

    let mut message = format!("[技能 {}]\n{}\n", skill.name, body(&text).trim_end());
    if !request.is_empty() {
        message.push_str(&format!("\n用户要求：{request}\n"));
    }
    Ok(message)
}

/// 跳过 `---` 包起来的头部，返回正文。
pub fn body(text: &str) -> &str {
    let trimmed = text.strip_prefix("---").unwrap_or(text);
    if text.starts_with("---")
        && let Some(end) = trimmed.find("\n---")
    {
        return &trimmed[end + 4..];
    }
    text
}

/// 解析 `---` 之间的简单 `键: 值`。只支持一层，够 SKILL.md 用。
fn frontmatter(text: &str) -> HashMap<String, String> {
    let mut meta = HashMap::new();
    let Some(rest) = text.strip_prefix("---") else {
        return meta;
    };
    let Some(end) = rest.find("\n---") else {
        return meta;
    };
    for line in rest[..end].lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty() {
            continue;
        }
        let mut value = value.trim().to_string();
        if value.len() >= 2
            && ((value.starts_with('"') && value.ends_with('"'))
                || (value.starts_with('\'') && value.ends_with('\'')))
        {
            value = value[1..value.len() - 1].to_string();
        }
        if !value.is_empty() {
            meta.insert(key.to_string(), value);
        }
    }
    meta
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("yi-skills-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn parses_quoted_and_plain_values() {
        let text = "---\nname: demo\ndescription: \"带引号的描述\"\ndisable-model-invocation: true\n---\n\n正文\n";
        let meta = frontmatter(text);
        assert_eq!(meta.get("name").unwrap(), "demo");
        assert_eq!(meta.get("description").unwrap(), "带引号的描述");
        assert_eq!(meta.get("disable-model-invocation").unwrap(), "true");
        assert_eq!(body(text).trim(), "正文");
    }

    #[test]
    fn discovers_hidden_skills_but_keeps_them_out_of_the_prompt() {
        let home = temp_dir("home");
        let root = home.join("skills");
        std::fs::create_dir_all(root.join("visible")).unwrap();
        std::fs::create_dir_all(root.join("hidden")).unwrap();
        std::fs::create_dir_all(root.join("broken")).unwrap();
        std::fs::write(
            root.join("visible/SKILL.md"),
            "---\nname: visible\ndescription: 看得见\n---\n正文\n",
        )
        .unwrap();
        std::fs::write(
            root.join("hidden/SKILL.md"),
            "---\nname: hidden\ndescription: 看不见\ndisable-model-invocation: true\n---\n正文\n",
        )
        .unwrap();
        std::fs::write(root.join("broken/SKILL.md"), "没有 frontmatter").unwrap();

        let skills = discover(&roots(&home, Path::new("/nonexistent")));
        assert_eq!(skills.len(), 2, "缺 description 的目录应被跳过");
        let rendered = render(&skills);
        assert!(rendered.contains("visible：看得见"));
        assert!(!rendered.contains("hidden"), "隐藏技能不应进提示词");
        assert!(rendered.contains("正文："), "应给出正文路径");
    }

    #[test]
    fn expand_includes_the_body_and_the_user_request() {
        let home = temp_dir("expand");
        let root = home.join("skills");
        std::fs::create_dir_all(root.join("demo")).unwrap();
        std::fs::write(
            root.join("demo/SKILL.md"),
            "---\nname: demo\ndescription: 演示\n---\n第一步\n",
        )
        .unwrap();

        let text = expand("demo 帮我做点事", &home, Path::new("/nonexistent")).unwrap();
        assert!(text.contains("第一步"));
        assert!(text.contains("用户要求：帮我做点事"));
        assert!(!text.contains("description:"), "frontmatter 不应进入消息");

        let err = expand("nope", &home, Path::new("/nonexistent")).unwrap_err();
        assert!(err.to_string().contains("没有叫 nope 的技能"));
    }

    #[test]
    fn a_space_after_the_colon_is_allowed() {
        let home = temp_dir("space");
        let root = home.join("skills");
        std::fs::create_dir_all(root.join("go-rules")).unwrap();
        std::fs::write(
            root.join("go-rules/SKILL.md"),
            "---\nname: go-rules\ndescription: 规范\n---\n正文内容\n",
        )
        .unwrap();

        for input in ["go-rules 看一下", " go-rules 看一下"] {
            let text = expand(input, &home, Path::new("/nonexistent")).expect("应当找到技能");
            assert!(text.contains("[技能 go-rules]"), "{text}");
            assert!(text.contains("正文内容"), "{text}");
        }
    }
}

