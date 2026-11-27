//! 本轮对话的中断开关，以及轮内打的字。
//!
//! 一轮之内键盘归这里管：按下 Ctrl-C 只把标志置位，网络读取和命令等待在自己的
//! 循环里检查它，不再有第二个地方抢按键。轮到结束时把按键原样还给行编辑器。
//!
//! 这一轮里终端是「不回显、不按行缓冲」的：否则你敲的字会被内核回显到流式正文
//! 中间，而按行缓冲又意味着不按回车就一个字节都拿不到。自己收字节，才能既立刻
//! 拿到、又把回显交给状态行（`Status`）去画——草稿因此不会和正文打架。

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

static CANCELLED: AtomicBool = AtomicBool::new(false);

/// 轮内最多攒这么多字节：够写一句话，又不会被误按的粘贴撑爆。
const MAX_DRAFT_BYTES: usize = 4000;

/// 本轮是否已被要求中断。
pub fn requested() -> bool {
    CANCELLED.load(Ordering::Relaxed)
}

/// 清掉标志和上一轮攒下的草稿，准备开始新的一轮。
pub fn reset() {
    CANCELLED.store(false, Ordering::Relaxed);
    let mut draft = DRAFT.lock().unwrap_or_else(|err| err.into_inner());
    *draft = Draft::default();
}

/// 发一次中断请求。信号处理函数用它，测试也用它。
pub fn request() {
    CANCELLED.store(true, Ordering::Relaxed);
}

extern "C" fn handler(_signal: libc::c_int) {
    CANCELLED.store(true, Ordering::Relaxed);
}

/// 检查是否该中断：先看信号标志，再尝试从终端读一个字节。
///
/// 行编辑器会把终端留在 raw 模式，这时 Ctrl-C 不产生信号，只是字节 0x03。
/// 流式输出期间没有别的读者，直接把这类字节当成取消请求；其他字节按「打字残留」
/// 丢弃——这一轮结束前它们本来也不会被任何人消费。
pub fn check() -> bool {
    poll_terminal();
    requested()
}

/// 轮内打的字，给状态行显示用。
pub fn draft_text() -> String {
    DRAFT
        .lock()
        .unwrap_or_else(|err| err.into_inner())
        .text()
        .to_string()
}

/// 取走轮内打的字：这一轮结束后放回提示符，人接着改。
pub fn take_draft() -> String {
    let mut draft = DRAFT.lock().unwrap_or_else(|err| err.into_inner());
    let text = draft.text().to_string();
    *draft = Draft::default();
    text
}

/// 轮内打的字：只攒着，不打断这一轮、也不自己回显（回显交给状态行）。
///
/// 终端这时是原始模式，字节按 UTF-8 原样到达；方向键、粘贴标记这类
/// 以 ESC 开头的序列整段丢掉，否则会变成草稿里的乱码。
#[derive(Default)]
struct Draft {
    bytes: Vec<u8>,
    escape: Escape,
}

#[derive(Default, Clone, Copy, PartialEq)]
enum Escape {
    #[default]
    None,
    /// 收到 ESC：可能是控制序列的开头，也可能是孤零零一个 ESC。
    Esc,
    /// 收到 ESC [：吞到终止字节为止。
    Csi,
}

impl Draft {
    fn feed(&mut self, byte: u8) {
        match self.escape {
            Escape::Esc => {
                self.escape = match byte {
                    b'[' => Escape::Csi,
                    0x1b => Escape::Esc,
                    _ => Escape::None,
                };
                return;
            }
            Escape::Csi => {
                // 0x40..=0x7e 是控制序列的终止字节（字母、`~` 等）。
                if (0x40..=0x7e).contains(&byte) {
                    self.escape = Escape::None;
                }
                return;
            }
            Escape::None => {}
        }
        match byte {
            0x1b => self.escape = Escape::Esc,
            0x08 | 0x7f => self.backspace(),
            // 回车、Tab、Ctrl-* 这类控制字符不进草稿；
            // Ctrl-C 走中断，不在这里。
            0x00..=0x1f => {}
            _ => {
                if self.bytes.len() < MAX_DRAFT_BYTES {
                    self.bytes.push(byte);
                }
            }
        }
    }

    fn backspace(&mut self) {
        let text = self.text().to_string();
        let mut chars = text.chars();
        chars.next_back();
        self.bytes = chars.as_str().as_bytes().to_vec();
    }

    /// 已完整的部分：末尾半个多字节字符先不显示，免得是「�」。
    fn text(&self) -> &str {
        match std::str::from_utf8(&self.bytes) {
            Ok(text) => text,
            Err(err) => std::str::from_utf8(&self.bytes[..err.valid_up_to()]).unwrap_or(""),
        }
    }
}

static DRAFT: Mutex<Draft> = Mutex::new(Draft {
    bytes: Vec::new(),
    escape: Escape::None,
});

fn poll_terminal() {
    unsafe {
        if libc::isatty(libc::STDIN_FILENO) != 1 {
            return;
        }
        let mut fd = libc::pollfd {
            fd: libc::STDIN_FILENO,
            events: libc::POLLIN,
            revents: 0,
        };
        if libc::poll(&mut fd, 1, 0) <= 0 {
            return;
        }
        // 一次最多吞这么多字节：够一整次粘贴，又不会让调用方迟迟不返回。
        let mut bytes = [0u8; 256];
        let mut draft = DRAFT.lock().unwrap_or_else(|err| err.into_inner());
        for _ in 0..4 {
            let read = libc::read(libc::STDIN_FILENO, bytes.as_mut_ptr().cast(), bytes.len());
            if read <= 0 {
                break;
            }
            for byte in &bytes[..read as usize] {
                if *byte == 0x03 {
                    request();
                } else {
                    draft.feed(*byte);
                }
            }
            if libc::poll(&mut fd, 1, 0) <= 0 {
                break;
            }
        }
    }
}

/// 接管 SIGINT：置位标志，不打断系统调用（`SA_RESTART` 不设，让阻塞读能返回）。
///
/// 终端把 ISIG 打开时，Ctrl-C 走这里；关着时（行编辑器留下的 raw 模式）
/// 走 [`check`] 里的字节路径。两条路同一个标志。
pub fn arm() {
    let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
    action.sa_sigaction = handler as *const () as libc::sighandler_t;
    action.sa_flags = 0;
    unsafe {
        libc::sigemptyset(&mut action.sa_mask);
        libc::sigaction(libc::SIGINT, &action, std::ptr::null_mut());
    }
    quiet_terminal();
}

/// 原来那套终端设置，交给行编辑器之前原样还回去。
static SAVED_TERMIOS: Mutex<Option<libc::termios>> = Mutex::new(None);

/// 关掉回显与行缓冲：按键按字节立即到达，回显由状态行负责。
///
/// 只动这两个标志，不碰 `OPOST`——输出处理一关，正文里的换行就不再回车，
/// 整个界面会错行。Ctrl-C 仍走信号（`ISIG` 保留）。
fn quiet_terminal() {
    unsafe {
        if libc::isatty(libc::STDIN_FILENO) != 1 {
            return;
        }
        let mut current: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(libc::STDIN_FILENO, &mut current) != 0 {
            return;
        }
        let mut saved = SAVED_TERMIOS.lock().unwrap_or_else(|err| err.into_inner());
        *saved = Some(current);
        let mut quiet = current;
        quiet.c_lflag &= !(libc::ICANON | libc::ECHO);
        quiet.c_cc[libc::VMIN] = 1;
        quiet.c_cc[libc::VTIME] = 0;
        if libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &quiet) != 0 {
            *saved = None;
        }
    }
}

/// 恢复默认处理：读输入时按 Ctrl-C 仍交给行编辑器自己处理。
pub fn disarm() {
    unsafe {
        libc::signal(libc::SIGINT, libc::SIG_DFL);
    }
    let mut saved = SAVED_TERMIOS.lock().unwrap_or_else(|err| err.into_inner());
    if let Some(original) = saved.take() {
        unsafe {
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &original);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sigint_reaching_the_handler_sets_the_flag() {
        reset();
        arm();
        // 真的发一次 SIGINT：验证信号处理函数注册生效，而不只是标志位本身。
        unsafe {
            libc::raise(libc::SIGINT);
        }
        assert!(requested(), "SIGINT 应当把中断标志置位");
        disarm();
        reset();
    }

    fn draft_after(bytes: &[u8]) -> String {
        let mut draft = Draft::default();
        for byte in bytes {
            draft.feed(*byte);
        }
        draft.text().to_string()
    }

    #[test]
    fn plain_typing_lands_in_the_draft() {
        assert_eq!(draft_after(b"hello"), "hello");
    }

    #[test]
    fn backspace_removes_one_character_not_one_byte() {
        // 中文一个字三个字节，退格必须整字删掉。
        assert_eq!(draft_after("你好".as_bytes()), "你好");
        let mut draft = Draft::default();
        for byte in "你好".as_bytes() {
            draft.feed(*byte);
        }
        draft.feed(0x7f);
        assert_eq!(draft.text(), "你");
    }

    #[test]
    fn half_a_character_is_not_shown_until_it_is_complete() {
        let full = "你".as_bytes();
        assert_eq!(draft_after(&full[..1]), "");
        assert_eq!(draft_after(&full[..2]), "");
        assert_eq!(draft_after(full), "你");
    }

    #[test]
    fn arrow_keys_do_not_become_text() {
        // 上箭头是 ESC [ A，整段吞掉；后面的字照收。
        assert_eq!(draft_after(b"ab\x1b[Acd"), "abcd");
    }

    #[test]
    fn enter_and_tab_are_ignored_during_a_turn() {
        assert_eq!(draft_after(b"ab\r\n\tcd"), "abcd");
    }

    #[test]
    fn the_draft_is_capped() {
        let mut draft = Draft::default();
        for _ in 0..MAX_DRAFT_BYTES + 100 {
            draft.feed(b'x');
        }
        assert_eq!(draft.bytes.len(), MAX_DRAFT_BYTES);
    }
}
