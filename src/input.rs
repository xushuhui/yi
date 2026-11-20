//! 输入行：编辑交给 reedline，补全菜单在输入行下方原地刷新。
//!
//! 用 reedline 而不是自己实现，是因为「边打字边更新候选菜单」这件事必须由
//! 编辑器自己重画那一块区域；rustyline 只能把清单往下追加（见 ADR-0003 的修订）。
//!
//! 这一层只管「读一行」：候选怎么算是 `complete` 的事，按键怎么解释是 reedline
//! 的事，读完之后做什么是 `main` 的事。

use std::borrow::Cow;

use reedline::{
    Completer, CompletionResult, DescriptionMode, EditCommand, Emacs, FileBackedHistory, History,
    HistoryItem, IdeMenu, KeyCode, KeyModifiers, Keybindings, MenuBuilder, Prompt, PromptEditMode,
    PromptHistorySearch, Reedline, ReedlineEvent, ReedlineMenu, Signal, Span, Suggestion,
    default_emacs_keybindings,
};

use crate::Error;
use crate::Result;

/// 菜单在 reedline 里的名字，按键绑定和菜单本身靠它对上。
const MENU: &str = "yi_completion";

/// 读完一行之后发生的事。
pub enum Action {
    /// 用户按了回车，交上来这一行。
    Line(String),
    /// Ctrl-C：放弃这一行，回到提示符。
    Interrupted,
    /// Ctrl-D 或输入结束：该退出了。
    Eof,
}

pub struct Input {
    editor: Reedline,
    /// 计划模式当前状态：Shift+Tab 切换后立刻改写提示符，不用等下一轮。
    planned: bool,
}

impl Input {
    /// `prompts` 是这条会话里人敲过的原文，用来喂上键历史。
    pub fn new(
        skills: Vec<(String, String)>,
        prompts: Vec<String>,
        planned: bool,
    ) -> Result<Input> {
        let mut history = FileBackedHistory::new(200)
            .map_err(|err| Error::Config(format!("初始化历史失败：{err}")))?;
        for prompt in prompts {
            // 历史只用来召回：某一条加不进去不影响输入。
            let _ = history.save(HistoryItem::from_command_line(prompt));
        }

        let keys = bindings();

        let menu = IdeMenu::default()
            .with_name(MENU)
            .with_min_completion_width(0)
            .with_max_completion_width(40)
            .with_max_completion_height(u16::MAX)
            .with_padding(0)
            .with_cursor_offset(0)
            .with_description_mode(DescriptionMode::PreferRight)
            .with_min_description_width(0)
            .with_max_description_width(80)
            .with_description_offset(1)
            .with_correct_cursor_pos(false);

        let editor = Reedline::create()
            .with_completer(Box::new(Skills(skills)))
            .with_history(Box::new(history))
            .with_menu(ReedlineMenu::EngineCompleter(Box::new(menu)))
            .with_edit_mode(Box::new(Emacs::new(keys)));

        Ok(Input { editor, planned })
    }

    /// 命令行走过 `/plan` 之后把提示符同步过来。
    pub fn set_planned(&mut self, planned: bool) {
        self.planned = planned;
    }

    /// 读一行；`draft` 非空时先把它放进输入行（轮内打的字，轮末接回来）。
    pub fn read(&mut self, draft: &str, toggle_plan: &mut dyn FnMut() -> bool) -> Result<Action> {
        if !draft.is_empty() {
            self.editor
                .run_edit_commands(&[EditCommand::InsertString(draft.to_string())]);
        }
        loop {
            let prompt = MarkerPrompt {
                planned: self.planned,
            };
            match self.editor.read_line(&prompt) {
                Ok(Signal::Success(line)) => return Ok(Action::Line(line)),
                Ok(Signal::CtrlC(_)) => return Ok(Action::Interrupted),
                Ok(Signal::CtrlD) => return Ok(Action::Eof),
                // Shift+Tab：read_line 挂起，输入行原样留着；切完接着读。
                Ok(Signal::HostCommand(command)) => {
                    if command == "plan" {
                        self.planned = toggle_plan();
                    }
                }
                // 断线、外部中断之类：当作放弃这一行，不留残缺状态。
                Ok(_) => return Ok(Action::Interrupted),
                Err(err) => return Err(Error::Config(format!("读输入失败：{err}"))),
            }
        }
    }
}

/// yi 自己的按键：补全、Shift+Tab 切计划模式、Ctrl-G 挂图片。
fn bindings() -> Keybindings {
    let mut keys = default_emacs_keybindings();
    keys.add_binding(
        KeyModifiers::NONE,
        KeyCode::Tab,
        ReedlineEvent::UntilFound(vec![
            ReedlineEvent::Menu(MENU.to_string()),
            ReedlineEvent::MenuNext,
        ]),
    );
    // Shift+Tab 切计划模式：和 Claude Code / Gemini 的「循环权限模式」同一只手感。
    // 终端把它发成 CSI Z，crossterm 解析成 BackTab + SHIFT。
    keys.add_binding(
        KeyModifiers::SHIFT,
        KeyCode::BackTab,
        ReedlineEvent::ExecuteHostCommand("plan".to_string()),
    );
    // Ctrl-G 挂剪贴板图片：终端不会把 Cmd-V 交给程序，这个键是 yi 自己能收到的。
    // 标记在提交时换成真正的图片，所以这里只插文字。
    keys.add_binding(
        KeyModifiers::CONTROL,
        KeyCode::Char('g'),
        ReedlineEvent::Edit(vec![EditCommand::InsertString(
            crate::clipboard::IMAGE_TOKEN.to_string(),
        )]),
    );
    keys
}

/// 补全适配器：把 `complete::candidates` 算出来的东西翻译成 reedline 的候选。
struct Skills(Vec<(String, String)>);

impl Completer for Skills {
    fn complete(&mut self, line: &str, pos: usize) -> CompletionResult {
        let Some((start, items)) = crate::complete::candidates(line, pos, &self.0) else {
            return CompletionResult::fresh(Vec::new());
        };
        let suggestions: Vec<Suggestion> = items
            .into_iter()
            .map(|item| Suggestion {
                value: item.value,
                description: item.description,
                span: Span::new(start, pos),
                ..Default::default()
            })
            .collect();
        CompletionResult::fresh(suggestions)
    }
}

/// 提示符：只有模式标记，不再给输入行加底色。
struct MarkerPrompt {
    planned: bool,
}

impl Prompt for MarkerPrompt {
    fn render_prompt_left(&self) -> Cow<'_, str> {
        Cow::Borrowed("")
    }

    fn render_prompt_right(&self) -> Cow<'_, str> {
        Cow::Borrowed("")
    }

    fn render_prompt_indicator(&self, _: PromptEditMode) -> Cow<'_, str> {
        Cow::Borrowed(if self.planned { "plan> " } else { "> " })
    }

    fn render_prompt_multiline_indicator(&self) -> Cow<'_, str> {
        Cow::Borrowed("::: ")
    }

    fn render_prompt_history_search_indicator(&self, _: PromptHistorySearch) -> Cow<'_, str> {
        Cow::Borrowed("(搜索) ")
    }
}

/// 供测试和文档用的按键表：补全只在 Tab 上触发，不做行内灰字。
#[allow(dead_code)]
pub fn keybindings() -> Keybindings {
    default_emacs_keybindings()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_prompt_announces_plan_mode() {
        let planned = MarkerPrompt { planned: true };
        assert_eq!(
            planned.render_prompt_indicator(PromptEditMode::Default),
            "plan> "
        );
        let plain = MarkerPrompt { planned: false };
        assert_eq!(plain.render_prompt_indicator(PromptEditMode::Default), "> ");
    }

    #[test]
    fn shift_tab_toggles_plan_mode() {
        let keys = bindings();
        let event = keys.find_binding(KeyModifiers::SHIFT, KeyCode::BackTab);
        assert!(
            matches!(&event, Some(ReedlineEvent::ExecuteHostCommand(cmd)) if cmd == "plan"),
            "{event:?}"
        );
    }
}
