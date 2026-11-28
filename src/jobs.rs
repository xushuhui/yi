//! 后台命令的登记处：`bash` 带 `background: true` 时，命令在这里挂个号，
//! 跑完由下一轮对话取走结果。
//!
//! 只有进程内的一份登记表：yi 单进程单会话，不需要跨进程的作业队列。

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{LazyLock, Mutex};

/// 同时最多几个后台命令。超过就拒绝，免得一句话甩出十条命令把机器拖死。
pub const MAX_ACTIVE: usize = 3;

/// 一条已经跑完、还没交回对话的后台作业。
pub struct Finished {
    pub id: String,
    pub command: String,
    pub secs: f32,
    pub output: String,
}

struct Slot {
    id: String,
    command: String,
    pid: u32,
    /// `None` 表示还在跑。
    done: Option<(f32, String)>,
}

static SLOTS: LazyLock<Mutex<Vec<Slot>>> = LazyLock::new(|| Mutex::new(Vec::new()));
static SEQ: AtomicUsize = AtomicUsize::new(0);

/// 登记一条后台命令；已经到并发上限时返回 `Err(原因)`。
pub fn register(command: &str, pid: u32) -> Result<String, String> {
    let mut slots = SLOTS.lock().unwrap_or_else(|err| err.into_inner());
    let active = slots.iter().filter(|slot| slot.done.is_none()).count();
    if active >= MAX_ACTIVE {
        return Err(format!(
            "后台命令最多同时跑 {MAX_ACTIVE} 条，等前面的结束再试"
        ));
    }
    let id = format!("bash-{}", SEQ.fetch_add(1, Ordering::Relaxed) + 1);
    slots.push(Slot {
        id: id.clone(),
        command: command.to_string(),
        pid,
        done: None,
    });
    Ok(id)
}

/// 记下结果。作业已经被取走时什么也不做。
pub fn finish(id: &str, output: String, secs: f32) {
    let mut slots = SLOTS.lock().unwrap_or_else(|err| err.into_inner());
    if let Some(slot) = slots.iter_mut().find(|slot| slot.id == id) {
        slot.done = Some((secs, output));
    }
}

/// 取走已经跑完的作业；还在跑的留着。
pub fn take_finished() -> Vec<Finished> {
    let mut slots = SLOTS.lock().unwrap_or_else(|err| err.into_inner());
    let mut finished = Vec::new();
    let mut index = 0;
    while index < slots.len() {
        match slots[index].done.take() {
            Some((secs, output)) => {
                let slot = slots.remove(index);
                finished.push(Finished {
                    id: slot.id,
                    command: slot.command,
                    secs,
                    output,
                });
            }
            // 还在跑：take 之后要放回去，否则会丢掉「没跑完」这个状态。
            None => {
                index += 1;
            }
        }
    }
    finished
}

/// 退出时把还在跑的命令整组杀掉，别留孤儿进程占着端口或文件。
pub fn kill_all() {
    let slots = SLOTS.lock().unwrap_or_else(|err| err.into_inner());
    for slot in slots.iter().filter(|slot| slot.done.is_none()) {
        let group = -(slot.pid as i32);
        // SAFETY: 只发信号，不读内存；进程号来自我们自己起的子进程。
        unsafe {
            libc::kill(group, libc::SIGTERM);
            libc::kill(group, libc::SIGKILL);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 作业表是进程级全局状态：这两条用例都会占用条目，必须串行跑，
    /// 否则并行的「满员」用例会把另一条挤到「最多同时跑 3 条」。
    static TABLE: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn hold_table() -> std::sync::MutexGuard<'static, ()> {
        TABLE.lock().unwrap_or_else(|err| err.into_inner())
    }

    #[test]
    fn finished_jobs_are_handed_over_once() {
        let _table = hold_table();
        let id = register("echo hi", 1).expect("登记成功");
        // 作业表是进程级全局状态，并行跑时别的用例也会交作业；只认自己这条。
        let early = take_finished();
        assert!(!early.iter().any(|job| job.id == id), "还在跑，不该被取走");
        finish(&id, "hi".to_string(), 0.5);
        let finished = take_finished();
        let mine: Vec<_> = finished.iter().filter(|job| job.id == id).collect();
        assert_eq!(mine.len(), 1, "自己这条要交上来一次");
        assert_eq!(mine[0].output, "hi");
        let again = take_finished();
        assert!(!again.iter().any(|job| job.id == id), "取过一次就不该再来");
    }

    #[test]
    fn registration_stops_at_the_concurrency_limit() {
        let _table = hold_table();
        let first = register("sleep 1", 2).expect("第一条");
        // 前面的用例可能留下作业，这里只验证「满员后拒绝」这条规则。
        let mut ids = vec![first];
        for _ in 0..MAX_ACTIVE {
            match register("sleep 2", 3) {
                Ok(id) => ids.push(id),
                Err(err) => {
                    assert!(err.contains("最多同时跑"), "{err}");
                    break;
                }
            }
        }
        assert!(ids.len() <= MAX_ACTIVE, "不该超过上限：{}", ids.len());
        for id in ids {
            finish(&id, String::new(), 0.0);
        }
        take_finished();
    }
}
