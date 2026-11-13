//! 斜杠命令与技能的补全、行内提示。
//!
//! 匹配是「子序列」：按顺序出现即可，`/nw` 命中 `/new`，`dvrs` 命中
//! `adversarial-review`。只补第一段（命令或命令后面的技能名），不猜自然语言。

use rustyline::completion::{Completer, Pair};
use rustyline::highlight::Highlighter;
use rustyline::hint::Hinter;
use rustyline::validate::Validator;
use rustyline::{Context, Helper as RustylineHelper};

/// 一次最多列这么多条候选，其余收成一行「还有 N 个」。
pub const MAX_CANDIDATES: usize = 8;

const COMMANDS: &[(&str, &str)] = &[
    ("/help", "显示命令、环境变量和配置字段"),
    ("/new", "归档当前会话，开始新的一条"),
    ("/effort", "设置推理强度：auto / low / high / max"),
    ("/skill:", "调用一个技能：/skill:<名字> [要求]"),
];

const EFFORTS: &[&str] = &["auto", "low", "high", "max"];

pub struct Helper {
    skills: Vec<(String, String)>,
}

impl Helper {
    pub fn new(skills: Vec<(String, String)>) -> Helper {
        Helper { skills }
    }
}

impl Completer for Helper {
    type Candidate = Pair;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        _ctx: &Context<'_>,
    ) -> rustyline::Result<(usize, Vec<Pair>)> {
        Ok(complete(line, pos, &self.skills).unwrap_or((pos, Vec::new())))
    }
}

impl Hinter for Helper {
    type Hint = String;

    fn hint(&self, line: &str, pos: usize, _ctx: &Context<'_>) -> Option<String> {
        hint(line, pos, &self.skills)
    }
}

impl Highlighter for Helper {}
impl Validator for Helper {}
impl RustylineHelper for Helper {}

/// 当前光标处的补全：返回替换起点和候选。
pub fn complete(line: &str, pos: usize, skills: &[(String, String)]) -> Option<(usize, Vec<Pair>)> {
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
        let mut pairs: Vec<(i32, Pair)> = EFFORTS
            .iter()
            .filter_map(|effort| {
                fuzzy(partial, effort).map(|score| (score, pair(*effort, *effort)))
            })
            .collect();
        return Some((start, finish(&mut pairs, partial)));
    }

    // 第一个词还没打完：补命令本身。
    let first = head.trim_start_matches('/');
    // `/skill:` 之后允许一个或多个空格再写技能名。
    if let Some(rest) = first.strip_prefix("skill:") {
        let partial = rest.trim_start();
        if !partial.contains(' ') {
            let start = pos - partial.len();
            let mut pairs: Vec<(i32, Pair)> = skills
                .iter()
                .filter_map(|(name, _description)| {
                    // 技能描述是整句话，列进候选会折行淹没屏幕；名字足够选。
                    fuzzy(partial, name).map(|score| (score, pair(name.clone(), name.clone())))
                })
                .collect();
            return Some((start, finish(&mut pairs, partial)));
        }
    }
    if !first.contains(' ') {
        // 还没输入命令名时，全部候选同分，按名字长短排；一旦输入了，才用模糊分。
        let typed = first;
        let mut pairs: Vec<(i32, Pair)> = COMMANDS
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
                    pair(format!("{command} — {description}"), replacement),
                ))
            })
            .collect();
        let start = pos - typed.len();
        return Some((start, finish(&mut pairs, typed)));
    }

    None
}

fn pair(display: impl Into<String>, replacement: impl Into<String>) -> Pair {
    Pair {
        display: display.into(),
        replacement: replacement.into(),
    }
}

/// 排序、截断，并在超出上限时追加一行不可选中的提示。
/// 没有候选以已输入内容开头时只返回最佳一条：rustyline 的列表模式对
/// 「无法延长公共前缀」的输入会先响铃，模糊匹配就会显得没反应。
fn finish(scored: &mut [(i32, Pair)], typed: &str) -> Vec<Pair> {
    scored.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| a.1.replacement.len().cmp(&b.1.replacement.len()))
    });
    let total = scored.len();
    let mut pairs: Vec<Pair> = scored.iter().map(|(_, pair)| pair.clone()).collect();
    let extendable = pairs.iter().any(|pair| pair.replacement.starts_with(typed));
    if !extendable && pairs.len() > 1 {
        pairs.truncate(1);
        return pairs;
    }
    if total > MAX_CANDIDATES {
        pairs.truncate(MAX_CANDIDATES - 1);
        // 选中这一行不改变输入：replacement 原样返回已经打出的部分。
        pairs.push(pair(
            format!(
                "… 还有 {} 个，继续输入缩小范围",
                total - (MAX_CANDIDATES - 1)
            ),
            typed.to_string(),
        ));
    }
    pairs
}

/// 行内灰字：给出最可能补全的剩余部分。
pub fn hint(line: &str, pos: usize, skills: &[(String, String)]) -> Option<String> {
    let (start, pairs) = complete(line, pos, skills)?;
    let typed = &line[start..pos];
    let best = pairs.first()?;
    let rest = best.replacement.strip_prefix(typed)?;
    if rest.is_empty() {
        return None;
    }
    Some(rest.to_string())
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

    fn replacements(pairs: &[Pair]) -> Vec<&str> {
        pairs.iter().map(|pair| pair.replacement.as_str()).collect()
    }

    #[test]
    fn a_command_prefix_completes() {
        let (start, pairs) = complete("/ne", 3, &skills()).expect("有候选");
        assert_eq!(start, 1);
        assert!(
            replacements(&pairs).contains(&"new"),
            "{:?}",
            replacements(&pairs)
        );
    }

    #[test]
    fn a_bare_slash_hints_the_shortest_matching_command() {
        assert_eq!(hint("/", 1, &skills()).as_deref(), Some("new"));
    }

    #[test]
    fn fuzzy_subsequence_finds_the_command() {
        let (_, pairs) = complete("/nw", 3, &skills()).expect("有候选");
        assert_eq!(replacements(&pairs).first(), Some(&"new"));
    }

    #[test]
    fn a_space_after_skill_colon_still_completes() {
        let (start, pairs) = complete("/skill: go", 10, &skills()).expect("有候选");
        assert_eq!(start, 8);
        assert_eq!(pairs[0].replacement, "go-rules");
    }

    #[test]
    fn a_fuzzy_match_that_is_not_a_prefix_still_completes() {
        // "rev" 不是任何技能名的前缀；此时只给最佳一条，Tab 才能直接替换。
        let (_, pairs) = complete("/skill:rev", 10, &skills()).expect("有候选");
        assert_eq!(pairs.len(), 1, "只给一条：{}", pairs[0].display);
        assert!(
            pairs[0].replacement.contains("review"),
            "{}",
            pairs[0].replacement
        );

        // 命令同理：/nw 不是 /new 的前缀。
        let (_, single) = complete("/nw", 3, &skills()).expect("有候选");
        assert_eq!(single.len(), 1);
        assert_eq!(single[0].replacement, "new");
    }

    #[test]
    fn skill_candidates_list_names_without_descriptions() {
        let (start, pairs) = complete("/skill:cr", 9, &skills()).expect("有候选");
        assert_eq!(start, 7);
        let names = replacements(&pairs);
        assert_eq!(names.first(), Some(&"code-review"), "{names:?}");
        assert_eq!(pairs[0].display, "code-review", "候选只列名字");
        assert!(
            !pairs[0].display.contains('—'),
            "不要把技能描述带进候选：{}",
            pairs[0].display
        );
    }

    #[test]
    fn effort_arguments_complete() {
        let (start, pairs) = complete("/effort h", 9, &skills()).expect("有候选");
        assert_eq!(start, 8);
        assert_eq!(replacements(&pairs), vec!["high"]);
    }

    #[test]
    fn long_skill_lists_are_capped_with_a_footer() {
        let many: Vec<(String, String)> = (0..12)
            .map(|index| (format!("code-{index}"), format!("第 {index} 个")))
            .collect();
        let (_, pairs) = complete("/skill:code", 11, &many).expect("有候选");
        // "/skill:code" 共 11 个字符：7 个固定前缀 + 4 个已输入字符。
        assert_eq!(pairs.len(), MAX_CANDIDATES);
        let last = pairs.last().expect("末条");
        assert!(last.display.contains("还有 5 个"), "{}", last.display);
    }

    #[test]
    fn the_hint_shows_only_the_remaining_characters() {
        assert_eq!(hint("/ne", 3, &skills()).as_deref(), Some("w"));
        assert_eq!(hint("/skill:go-", 10, &skills()).as_deref(), Some("rules"));
        assert_eq!(hint("你好", 6, &skills()), None);
    }

    #[test]
    fn plain_text_gets_no_completion() {
        assert!(complete("你好", 6, &skills()).is_none());
        assert!(complete("/effort high now", 16, &skills()).is_none());
    }
}
