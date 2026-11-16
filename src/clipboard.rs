//! 剪贴板图片：`Cmd+V` 是终端自己的文本粘贴，程序收不到这个键；图片剪贴板又
//! 只有 Ghostty 1.4 起的 Kitty 协议（OSC 5522）才有标准通道。这里绕开终端，
//! 由一个 yi 能收到的按键（Ctrl-G）触发，用 `osascript` 直接向系统要剪贴板里的 PNG。
//!
//! 文本粘贴不受影响：仍由终端原生处理，走括号粘贴进输入行。

use std::path::{Path, PathBuf};
use std::process::Command;

/// 输入行里代表「这里挂着一张剪贴板图片」的标记。
pub const IMAGE_TOKEN: &str = "[图片]";
/// 按了挂图键但剪贴板里没有图片时的标记。
pub const EMPTY_TOKEN: &str = "[剪贴板无图片]";

/// 把剪贴板里的 PNG 写进 `dir`，返回文件路径；剪贴板里没有图片就返回 `None`。
///
/// SIMPLIFICATION: 每次挂图写一个新文件，取用后不删——会话只记路径，删了老会话
/// 里的图就没了。文件很小，需要清理时手动删数据目录下的 `clipboard/` 即可。
pub fn image_path(dir: &Path) -> Option<PathBuf> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    std::fs::create_dir_all(dir).ok()?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis())
        .unwrap_or(0);
    let path = dir.join(format!("paste-{stamp}.png"));
    // 剪贴板里没有 PNG 时 osascript 抛错，脚本在 try 里吞掉并回 "none"。
    let script = format!(
        r#"try
    set d to the clipboard as «class PNGf»
on error
    return "none"
end try
set f to open for access POSIX file "{}" with write permission
set eof f to 0
write d to f
close access f
return "ok""#,
        path.display()
    );
    let output = Command::new("osascript")
        .arg("-e")
        .arg(script)
        .output()
        .ok()?;
    if String::from_utf8_lossy(&output.stdout).trim() != "ok" {
        let _ = std::fs::remove_file(&path);
        return None;
    }
    let written = std::fs::metadata(&path)
        .map(|meta| meta.len() > 0)
        .unwrap_or(false);
    if !written {
        let _ = std::fs::remove_file(&path);
        return None;
    }
    Some(path)
}

/// 摘掉输入行里的图片标记，返回（剩下的文字，是否挂着图）。
pub fn strip_tokens(line: &str) -> (String, bool) {
    let mut image = false;
    let mut text = line.to_string();
    while let Some(index) = text.find(IMAGE_TOKEN) {
        image = true;
        text.replace_range(index..index + IMAGE_TOKEN.len(), "");
    }
    while let Some(index) = text.find(EMPTY_TOKEN) {
        text.replace_range(index..index + EMPTY_TOKEN.len(), "");
    }
    (text.trim().to_string(), image)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_token_is_removed_and_the_image_flagged() {
        let (text, image) = strip_tokens("[图片] 这两行有什么问题？");
        assert_eq!(text, "这两行有什么问题？");
        assert!(image);
    }

    #[test]
    fn a_token_in_the_middle_leaves_the_rest_alone() {
        let (text, image) = strip_tokens("看[图片]这里");
        assert_eq!(text, "看这里");
        assert!(image);
    }

    #[test]
    fn the_empty_clipboard_token_is_removed_without_attaching() {
        let (text, image) = strip_tokens("[剪贴板无图片] 解释一下");
        assert_eq!(text, "解释一下");
        assert!(!image, "没有图片时不该挂图");
    }

    #[test]
    fn ordinary_text_is_untouched() {
        let (text, image) = strip_tokens("  普通的一句话  ");
        assert_eq!(text, "普通的一句话");
        assert!(!image);
    }
}
