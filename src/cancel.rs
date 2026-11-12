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

/// 接管 SIGINT：置位标志，不打断系统调用（`SA_RESTART` 不设，让阻塞读能返回）。
pub fn arm() {
    let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
    action.sa_sigaction = handler as *const () as libc::sighandler_t;
    action.sa_flags = 0;
    unsafe {
        libc::sigemptyset(&mut action.sa_mask);
        libc::sigaction(libc::SIGINT, &action, std::ptr::null_mut());
    }
}

/// 恢复默认处理：读输入时按 Ctrl-C 由行编辑器负责。
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
