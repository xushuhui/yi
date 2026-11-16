//! OSC 7501「程序状态协议」：把 yi 正在做什么告诉终端。
//!
//! Rex（<https://www.superlogical.com/rex/docs/build/program-status>）据此给标签页
//! 画转圈、给会话列表画「有新结果」。协议规定不认识的终端必须忽略这条 OSC，所以
//! 对 Ghostty、iTerm 这些终端写出去是安全的，不会打印也不会乱码。
//!
//! 只在标准输出是终端时才写；管道与重定向里一个字节都不发。

use std::io::{IsTerminal, Write};

/// 状态记录挂在哪个 `app` 下。Rex 用它分组、取图标、写给 Lua 事件。
const APP: &str = "yi";

/// 正在干活；`msg` 是一句人话（例如「等待模型」「运行 bash」）。
pub fn working(msg: &str) {
    send("working", Some(msg));
}

/// 干完了，结果还没被人看过。协议要求保留它，等人回到标签页再消失。
pub fn done(msg: &str) {
    send("done", Some(msg));
}

/// 这一轮失败并停下了。
pub fn error(msg: &str) {
    send("error", Some(msg));
}

/// 回到闲置：用户按 Ctrl-C 取消了这一轮。
pub fn idle() {
    send("idle", None);
}

/// 删掉这条记录：`done`/`error` 会一直留着，退出时清掉，免得下次看见旧状态。
pub fn clear() {
    send("clear", None);
}

fn send(state: &str, msg: Option<&str>) {
    if !std::io::stdout().is_terminal() {
        return;
    }
    print!("{}", sequence(state, msg));
    let _ = std::io::stdout().flush();
}

/// 一条完整的 OSC 7501 报告。
///
/// `msg` 会先截成单行、去掉控制字符再转 base64：协议要求 `msg` 是单行文本，
/// 含控制字符的整条报告会被终端丢弃。
pub fn sequence(state: &str, msg: Option<&str>) -> String {
    let mut body = format!("state={state}:app={APP}");
    if let Some(msg) = msg {
        let line: String = msg
            .lines()
            .next()
            .unwrap_or_default()
            .chars()
            .filter(|ch| !ch.is_control())
            .take(120)
            .collect();
        if !line.is_empty() {
            body.push_str(":msg=");
            body.push_str(&base64(&line));
        }
    }
    format!("\x1b]7501;{body}\x1b\\")
}

/// 标准 base64（带填充）。只用来编码一句短中文，不值得为此引入依赖。
fn base64(text: &str) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = u32::from(chunk[0]);
        let b1 = u32::from(chunk.get(1).copied().unwrap_or(0));
        let b2 = u32::from(chunk.get(2).copied().unwrap_or(0));
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(char::from(TABLE[(n >> 18) as usize & 63]));
        out.push(char::from(TABLE[(n >> 12) as usize & 63]));
        out.push(if chunk.len() > 1 {
            char::from(TABLE[(n >> 6) as usize & 63])
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            char::from(TABLE[n as usize & 63])
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_working_report_carries_the_app_and_the_message() {
        assert_eq!(
            sequence("working", Some("等待模型")),
            "\x1b]7501;state=working:app=yi:msg=562J5b6F5qih5Z6L\x1b\\"
        );
    }

    #[test]
    fn an_idle_report_has_no_message() {
        assert_eq!(sequence("idle", None), "\x1b]7501;state=idle:app=yi\x1b\\");
        assert_eq!(
            sequence("clear", None),
            "\x1b]7501;state=clear:app=yi\x1b\\"
        );
    }

    #[test]
    fn a_multi_line_message_keeps_only_the_first_line() {
        assert_eq!(
            sequence("error", Some("这一轮失败：接口返回错误\n第二行")),
            "\x1b]7501;state=error:app=yi:msg=6L+Z5LiA6L2u5aSx6LSl77ya5o6l5Y+j6L+U5Zue6ZSZ6K+v\x1b\\"
        );
    }

    #[test]
    fn control_characters_never_reach_the_report() {
        let out = sequence("working", Some("运行\tbash\u{7}"));
        assert!(!out.contains('\t'), "{out:?}");
        assert!(!out.contains('\u{7}'), "{out:?}");
        // 「运行bash」的 base64。
        assert!(out.contains("6L+Q6KGMYmFzaA=="), "{out:?}");
    }

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(base64("a"), "YQ==");
        assert_eq!(base64("ab"), "YWI=");
        assert_eq!(base64("abc"), "YWJj");
        assert_eq!(base64("完成 · 42.0s"), "5a6M5oiQIMK3IDQyLjBz");
    }
}
