// 变更日志: 只在状态跃变时写一行, 1 MB 轮转
//
// 两处必须跨进程正确, 否则表现为"日志莫名缺行", 而且全被 `let _ =` 吞掉:
//
// 1) **整行一次写出**。`writeln!(f, "{}  {}", a, b)` 对 File 会发起多次 write
//    syscall(每个格式片段一次), 两个写入方交错时拼出半行。先 format 成 String
//    再 write_all 一次, 才能让"追加"与"一行"是同一个原子单位。
//
// 2) **轮转与追加要在同一把跨进程锁内**。第二个实例是可达状态(用户手动跑
//    --status、计划任务与手动启动撞车)。只有进程内 Mutex 时两方会同时走到
//    "rename + append": 一方 rename 失败, 或写入落进刚被改名的旧文件。
//    命名互斥体是 Windows 上不必自己设计文件锁协议的可靠办法。
use std::io::Write;
use std::path::Path;

use windows::core::w;
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_ABANDONED, WAIT_OBJECT_0};
use windows::Win32::System::Threading::{CreateMutexW, ReleaseMutex, WaitForSingleObject};

const MAX_SIZE: u64 = 1024 * 1024;
/// rename 一直失败时(.1 被记事本独占)的兜底截断线。
/// 不设界的话主日志会无限增长, 而轮转永远做不成。
const HARD_CAP: u64 = MAX_SIZE * 4;
/// 拿不到锁就放弃这一行 —— 日志绝不能拖住 worker 循环
const LOCK_TIMEOUT_MS: u32 = 2000;

/// 跨进程日志锁。持有期间独占"轮转 + 追加"这一段。
struct LogLock(HANDLE);

impl LogLock {
    /// 拿不到锁返回 None: 宁可丢一行日志, 也不让 worker 阻塞。
    fn acquire() -> Option<LogLock> {
        unsafe {
            let h = CreateMutexW(None, false, w!("Local\\stayawake_log")).ok()?;
            let r = WaitForSingleObject(h, LOCK_TIMEOUT_MS);
            // WAIT_ABANDONED: 上一个持有者在持锁时进程退出了。所有权仍归我们,
            // 而"被保护的数据"只是一个追加位置, 没有需要修复的不变量 -> 照常继续。
            if r == WAIT_OBJECT_0 || r == WAIT_ABANDONED {
                Some(LogLock(h))
            } else {
                let _ = CloseHandle(h);
                None
            }
        }
    }
}

impl Drop for LogLock {
    fn drop(&mut self) {
        unsafe {
            let _ = ReleaseMutex(self.0);
            let _ = CloseHandle(self.0);
        }
    }
}

pub fn event(msg: &str) {
    event_to(&crate::config::log_path(), msg);
}

/// event 的可注入路径版本, 便于单测(不去碰用户真实日志)。
fn event_to(path: &Path, msg: &str) {
    let line = format!("{}  {}\n", crate::power::now_local(), msg);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    let Some(_lock) = LogLock::acquire() else {
        return;
    };
    rotate_if_needed(path);
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        // 一次 write_all: 半行拼接在日志里比缺行更难排查
        let _ = f.write_all(line.as_bytes());
    }
}

/// 超过上限就转存为 .1, 只保留一代。必须在锁内调用。
fn rotate_if_needed(path: &Path) {
    let Ok(size) = std::fs::metadata(path).map(|m| m.len()) else {
        return;
    };
    if size <= MAX_SIZE {
        return;
    }
    if std::fs::rename(path, path.with_extension("log.1")).is_ok() {
        return;
    }
    // rename 只会因为目标被独占打开而失败(Windows 上 rename 自带 REPLACE_EXISTING)。
    // 那种情况下轮转永远做不成, 到硬上限就地截断, 不让主日志无界增长。
    if size > HARD_CAP {
        let _ = std::fs::write(path, "");
    }
}

pub fn open_in_editor(path: &std::path::Path) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if !path.exists() {
        let _ = std::fs::write(path, "");
    }
    let _ = std::process::Command::new("notepad.exe").arg(path).spawn();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_log(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join("stayawake_log_test");
        let _ = std::fs::create_dir_all(&dir);
        let p = dir.join(name);
        let _ = std::fs::remove_file(&p);
        let _ = std::fs::remove_file(p.with_extension("log.1"));
        p
    }

    #[test]
    fn writes_one_line_with_timestamp() {
        let p = temp_log("basic.log");
        event_to(&p, "hello");
        let text = std::fs::read_to_string(&p).unwrap();
        assert_eq!(text.lines().count(), 1);
        // "YYYY-MM-DD HH:MM:SS" + 两空格 + msg
        assert!(text.ends_with("  hello\n"), "得到 {:?}", text);
        assert_eq!(text.len(), 19 + 2 + 5 + 1);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn appends_without_truncating() {
        let p = temp_log("append.log");
        for i in 0..5 {
            event_to(&p, &format!("line{}", i));
        }
        let text = std::fs::read_to_string(&p).unwrap();
        assert_eq!(text.lines().count(), 5);
        assert!(text.contains("line0") && text.contains("line4"));
        let _ = std::fs::remove_file(&p);
    }

    /// 超过 1 MB 要转存为 .1, 主日志重新从新行开始。
    /// 轮转做不成的话日志会一直涨到把磁盘写满。
    #[test]
    fn rotates_past_max_size() {
        let p = temp_log("rotate.log");
        std::fs::write(&p, "x".repeat((MAX_SIZE + 1) as usize)).unwrap();
        event_to(&p, "after rotate");

        let rolled = p.with_extension("log.1");
        assert!(rolled.exists(), "旧日志应转存为 .1");
        let text = std::fs::read_to_string(&p).unwrap();
        assert_eq!(text.lines().count(), 1, "主日志应只剩新写的那行");
        assert!(text.contains("after rotate"));
        let _ = std::fs::remove_file(&p);
        let _ = std::fs::remove_file(&rolled);
    }

    #[test]
    fn does_not_rotate_below_max_size() {
        let p = temp_log("small.log");
        std::fs::write(&p, "x".repeat(1024)).unwrap();
        event_to(&p, "still here");
        assert!(!p.with_extension("log.1").exists(), "未到上限不该轮转");
        let _ = std::fs::remove_file(&p);
    }

    /// 并发写入必须行行完整: 每行都应以时间戳开头、以自己的 msg 结尾。
    /// `writeln!` 的多次 write 在这里会拼出半行。
    #[test]
    fn concurrent_writes_keep_lines_intact() {
        let p = temp_log("concurrent.log");
        let threads: Vec<_> = (0..4)
            .map(|t| {
                let p = p.clone();
                std::thread::spawn(move || {
                    for i in 0..25 {
                        event_to(&p, &format!("t{}-{:02}", t, i));
                    }
                })
            })
            .collect();
        for h in threads {
            h.join().unwrap();
        }

        let text = std::fs::read_to_string(&p).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 100, "一行都不该丢");
        for l in &lines {
            // 时间戳(19) + 两空格 + "tN-NN"(5)
            assert_eq!(l.len(), 19 + 2 + 5, "半行/拼接行: {:?}", l);
            assert_eq!(&l[19..21], "  ");
            assert!(l[21..].starts_with('t'));
        }
        let _ = std::fs::remove_file(&p);
    }

    /// 锁必须可重复获取(Drop 里 ReleaseMutex 生效), 否则第二行起就会超时丢弃
    #[test]
    fn lock_is_reentrant_across_calls() {
        for _ in 0..3 {
            assert!(LogLock::acquire().is_some(), "上一次的锁未释放");
        }
    }
}
