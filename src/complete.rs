//! 斜杠命令与技能的补全。
//!
//! 匹配是「子序列」：按顺序出现即可，`/nw` 命中 `/new`，`dvrs` 命中
//! `adversarial-review`。只补第一段（命令或命令后面的技能名），不猜自然语言。
//!
//! 这里只算候选，不管怎么画：菜单由 `input` 模块交给 reedline 渲染。

/// 一次最多给这么多条候选，其余收成一条「还有 N 个」的提示。
pub const MAX_CANDIDATES: usize = 8;

/// 说明最多占多少格：太长会让菜单变成一大块，用户还得自己扫。
const DESCRIPTION_CELLS: usize = 90;

const COMMANDS: &[(&str, &str)] = &[
    ("/help", "显示命令、环境变量和配置字段"),
    ("/new", "归档当前会话，开始新的一条"),
    ("/plan", "切换计划模式：只读调查，不改文件"),
    ("/effort", "设置推理强度：auto / low / high / max"),
    ("/switch", "切换供应商与模型：opencode-go / deepseek"),
    ("/tree", "查看会话岔路；/tree <序号> 回到某条消息之前"),
    ("/skill:", "调用一个技能：/skill:<名字> [要求]"),
];

const EFFORTS: &[&str] = &["auto", "low", "high", "max"];

/// 一条候选：写回输入行的文本，加上菜单右侧的说明。
#[derive(Clone, Debug, PartialEq)]
pub struct Candidate {
    pub value: String,
    pub description: Option<String>,
}

impl Candidate {
    fn new(value: impl Into<String>, description: Option<String>) -> Candidate {
        Candidate {
            value: value.into(),
            description,
        }
    }
}

/// 当前光标处的补全：返回替换起点和候选。
pub fn candidates(
    line: &str,
    pos: usize,
    skills: &[(String, String)],
) -> Option<(usize, Vec<Candidate>)> {
    if !line.starts_with('/') || pos > line.len() {
        return None;
    }
    let head = &line[..pos];

    // `/effort ` 之后补档位。
    if let Some(rest) = head.strip_prefix("/effort ") {
        let partial = rest;
        if partial.contains(' ') {
            return None;
        }
        let start = pos - partial.len();
        let mut found: Vec<(i32, Candidate)> = EFFORTS
            .iter()
            .filter_map(|effort| {
                fuzzy(partial, effort).map(|score| (score, Candidate::new(*effort, None)))
            })
            .collect();
        return Some((start, finish(&mut found, partial)));
    }

    // `/switch ` 之后补供应商 id。
    if let Some(rest) = head.strip_prefix("/switch ") {
        let partial = rest;
        if partial.contains(' ') {
            return None;
        }
        let start = pos - partial.len();
        let mut found: Vec<(i32, Candidate)> = crate::config::PRESETS
            .iter()
            .filter_map(|preset| {
                fuzzy(partial, preset.id).map(|score| {
                    (
                        score,
                        Candidate::new(preset.id, Some(preset.model_display.to_string())),
                    )
                })
            })
            .collect();
        return Some((start, finish(&mut found, partial)));
    }

    // 第一个词还没打完：补命令本身。
    let first = head.trim_start_matches('/');
    // `/skill:` 之后允许一个或多个空格再写技能名。
    if let Some(rest) = first.strip_prefix("skill:") {
        let partial = rest.trim_start();
        if !partial.contains(' ') {
            let start = pos - partial.len();
            let mut found: Vec<(i32, Candidate)> = skills
                .iter()
                .filter_map(|(name, description)| {
                    fuzzy(partial, name).map(|score| {
                        (
                            score,
                            Candidate::new(name.clone(), Some(shorten(description))),
                        )
                    })
                })
                .collect();
            return Some((start, finish(&mut found, partial)));
        }
    }
    if !first.contains(' ') {
        // 还没输入命令名时，全部候选同分，按名字长短排；一旦输入了，才用模糊分。
        let typed = first;
        let mut found: Vec<(i32, Candidate)> = COMMANDS
            .iter()
            .filter_map(|(command, description)| {
                let replacement = command.trim_start_matches('/').to_string();
                let score = if typed.is_empty() {
                    0
                } else {
                    fuzzy(typed, &replacement)?
                };
                Some((
                    score,
                    Candidate::new(replacement, Some((*description).to_string())),
                ))
            })
            .collect();
        let start = pos - typed.len();
        return Some((start, finish(&mut found, typed)));
    }

    None
}

/// 排序、截断，并在超出上限时追加一条不可选中的提示。
/// 没有候选以已输入内容开头时只返回最佳一条，避免菜单对「怎么补都不会变长」
/// 的输入反复弹同一批东西。
fn finish(scored: &mut [(i32, Candidate)], typed: &str) -> Vec<Candidate> {
    scored.sort_by(|a, b| {
        // 还没输入时按名字排；输入之后按匹配度，同分再按长短。
        if typed.is_empty() {
            a.1.value.cmp(&b.1.value)
        } else {
            b.0.cmp(&a.0)
                .then_with(|| a.1.value.len().cmp(&b.1.value.len()))
        }
    });
    let mut items: Vec<Candidate> = scored.iter().map(|(_, item)| item.clone()).collect();
    let extendable = items.iter().any(|item| item.value.starts_with(typed));
    if !extendable && items.len() > 1 {
        items.truncate(1);
        return items;
    }
    items.truncate(MAX_CANDIDATES);
    items
}

/// 说明按显示宽度截断，中文占两格。
fn shorten(text: &str) -> String {
    let mut out = String::new();
    let mut used = 0;
    for ch in text.chars() {
        let cells = crate::render::display_width(&ch.to_string());
        if used + cells > DESCRIPTION_CELLS {
            out.push('…');
            break;
        }
        used += cells;
        out.push(ch);
    }
    out
}

/// 子序列匹配，返回分数；空 needle 命中一切（分数最低）。
fn fuzzy(needle: &str, hay: &str) -> Option<i32> {
    let needle: Vec<char> = needle.to_lowercase().chars().collect();
    let hay_chars: Vec<char> = hay.to_lowercase().chars().collect();
    if needle.is_empty() {
        return Some(0);
    }
    let mut score = 0i32;
    let mut cursor = 0usize;
    let mut previous: Option<usize> = None;
    for wanted in needle {
        let found = (cursor..hay_chars.len()).find(|index| hay_chars[*index] == wanted)?;
        score += match previous {
            Some(prev) if found == prev + 1 => 8,
            Some(_) => 2,
            None => 4,
        };
        if found == 0 {
            score += 10;
        }
        previous = Some(found);
        cursor = found + 1;
    }
    Some(score)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skills() -> Vec<(String, String)> {
        vec![
            ("go-rules".to_string(), "Go 代码规范".to_string()),
            ("code-review".to_string(), "两轴审查".to_string()),
            ("codebase-design".to_string(), "模块设计".to_string()),
            ("adversarial-review".to_string(), "对抗审查".to_string()),
        ]
    }

    fn values(items: &[Candidate]) -> Vec<&str> {
        items.iter().map(|item| item.value.as_str()).collect()
    }

    #[test]
    fn a_bare_slash_lists_every_command() {
        let (start, items) = candidates("/", 1, &skills()).expect("有候选");
        // 替换起点在 `/` 之后：命令名不带斜杠写回。
        assert_eq!(start, 1);
        let names = values(&items);
        assert!(names.contains(&"new"), "{names:?}");
        assert!(names.contains(&"switch"), "{names:?}");
        assert_eq!(
            items
                .iter()
                .find(|item| item.value == "new")
                .and_then(|item| item.description.clone()),
            Some("归档当前会话，开始新的一条".to_string())
        );
    }

    #[test]
    fn fuzzy_subsequence_finds_the_command() {
        let (start, items) = candidates("/nw", 3, &skills()).expect("有候选");
        assert_eq!(start, 1);
        assert_eq!(values(&items), vec!["new"]);
    }

    #[test]
    fn skills_come_with_their_description() {
        let (start, items) = candidates("/skill:cr", 9, &skills()).expect("有候选");
        assert_eq!(start, 7);
        assert_eq!(values(&items), vec!["code-review"]);
        assert_eq!(items[0].description.as_deref(), Some("两轴审查"));
    }

    #[test]
    fn an_empty_skill_query_lists_them_alphabetically() {
        let (_, items) = candidates("/skill:", 7, &skills()).expect("有候选");
        assert_eq!(
            values(&items),
            vec![
                "adversarial-review",
                "code-review",
                "codebase-design",
                "go-rules"
            ]
        );
    }

    #[test]
    fn switch_arguments_list_providers_with_their_model() {
        let (start, items) = candidates("/switch d", 9, &skills()).expect("有候选");
        assert_eq!(start, 8);
        assert_eq!(items[0].value, "deepseek");
        assert_eq!(items[0].description.as_deref(), Some("DeepSeek Flash"));
    }

    #[test]
    fn effort_arguments_complete() {
        let (start, items) = candidates("/effort h", 9, &skills()).expect("有候选");
        assert_eq!(start, 8);
        assert_eq!(values(&items), vec!["high"]);
    }

    #[test]
    fn the_list_is_capped() {
        let many: Vec<(String, String)> = (0..12)
            .map(|index| (format!("code-{index}"), format!("第 {index} 个")))
            .collect();
        let (_, items) = candidates("/skill:code", 11, &many).expect("有候选");
        assert_eq!(items.len(), MAX_CANDIDATES);
    }

    #[test]
    fn a_query_that_cannot_be_extended_collapses_to_the_best_one() {
        // "cr" 没有任何候选以它开头，只给最匹配的一条，菜单不会停在那里。
        let line = "/skill:cr";
        let (_, items) = candidates(line, line.len(), &skills()).expect("有候选");
        assert_eq!(items.len(), 1);
    }

    #[test]
    fn plain_text_gets_no_completion() {
        assert!(candidates("你好", 6, &skills()).is_none());
        assert!(candidates("/effort high now", 16, &skills()).is_none());
    }

    #[test]
    fn a_long_description_is_cut_to_one_row() {
        let long = vec![("code-review".to_string(), "很长的描述".repeat(60))];
        let (_, items) = candidates("/skill:co", 9, &long).expect("有候选");
        let desc = items[0].description.as_deref().unwrap_or("");
        assert!(crate::render::display_width(desc) <= DESCRIPTION_CELLS + 1);
        assert!(desc.ends_with('…'));
    }
}
