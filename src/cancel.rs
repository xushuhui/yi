//! 本轮对话的中断开关。
//!
//! 只在一次模型请求 + 工具执行期间接管 SIGINT：按下 Ctrl-C 时把标志置位，
//! 网络读取和命令等待在自己的循环里检查它。读输入期间恢复默认处理，交给
//! 行编辑器自己处理 Ctrl-C。

use std::sync::atomic::{AtomicBool, Ordering};

static CANCELLED: AtomicBool = AtomicBool::new(false);

/// 本轮是否已被要求中断。
pub fn requested() -> bool {
    CANCELLED.load(Ordering::Relaxed)
}

/// 清掉标志，准备开始新的一轮。
pub fn reset() {
    CANCELLED.store(false, Ordering::Relaxed);
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
        let mut byte: u8 = 0;
        if libc::read(
            libc::STDIN_FILENO,
            (&mut byte as *mut u8).cast(),
            1,
        ) == 1
            && byte == 0x03
        {
            request();
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
}

/// 恢复默认处理与终端设置：读输入时按 Ctrl-C 仍交给行编辑器自己处理。
pub fn disarm() {
    unsafe {
        libc::signal(libc::SIGINT, libc::SIG_DFL);
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
}
