// 外部提示文件: %LOCALAPPDATA%\stayawake\hints\*.hint
//
// 协议极简: 任何程序想保持机器唤醒就 touch 一个 .hint 文件, 忙碌期间定期刷新
// mtime, 结束时删除。mtime 超过 TTL 自动失效 —— 写方崩溃不会把机器永久卡醒。
//
// 这是给"CPU 占用无法反映真实忙碌"的程序留的精确通道。
// 实测 OpenCode(Electron) 工具执行中 CPU 52-70% 几乎全是 UI 重绘, 真正干活的
// node 进程只有 0.3%, 纯 CPU 判据在这里不可用, 必须让程序自己报告。
use std::time::{Duration, SystemTime};

use super::{Detector, ProcessTable, Reason};
use crate::config::Config;

/// 单个 hint 的读数
struct HintRead {
    name: String,
    note: String,
    age_secs: u64,
    /// mtime 在未来(时钟回跳) —— 判定为 stale, 但要在 --status 里看得见
    future: bool,
    fresh: bool,
}

#[derive(Default)]
pub struct HintDetector {
    last: Vec<HintRead>,
}

/// 读文件首行作为说明(截断到 60 字节, 避免超长内容进 tooltip)
fn read_note(path: &std::path::Path) -> String {
    let Ok(bytes) = std::fs::read(path) else {
        return String::new();
    };
    let end = bytes
        .iter()
        .position(|&c| c == b'\n' || c == b'\r')
        .unwrap_or(bytes.len())
        .min(60);
    String::from_utf8_lossy(&bytes[..end]).trim().to_string()
}

/// 判定一个 mtime 的新鲜度。返回 (年龄秒, 是否新鲜, 是否在未来)。
///
/// mtime 落在未来 => 时钟被回调过(CMOS 电池耗尽、双系统写 RTC、虚拟机快照恢复)。
/// 必须视为 stale 而不是 fresh: 否则所有 hint 都永久"新鲜", 机器再也不睡,
/// 而这恰恰是 TTL 机制存在的目的(写方崩溃不该把机器永久卡醒)。
fn classify(now: SystemTime, mtime: SystemTime, ttl: Duration) -> (u64, bool, bool) {
    match now.duration_since(mtime) {
        Ok(age) => (age.as_secs(), age <= ttl, false),
        Err(_) => (0, false, true),
    }
}

impl Detector for HintDetector {
    /// 目录枚举 + 读首行, 微秒级且不需要进程快照 -> 参与快速通道
    fn probe(&mut self, cfg: &Config) -> bool {
        !self.tick(cfg, &ProcessTable::default()).is_empty()
    }

    fn tick(&mut self, cfg: &Config, _table: &ProcessTable) -> Vec<Reason> {
        self.last.clear();
        if !cfg.hint_enabled {
            return Vec::new();
        }
        let dir = crate::config::hints_dir();
        let _ = std::fs::create_dir_all(&dir);
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return Vec::new();
        };

        let ttl = Duration::from_secs(cfg.hint_ttl_secs);
        let now = SystemTime::now();
        for e in entries.flatten() {
            let path = e.path();
            if path.extension().and_then(|x| x.to_str()) != Some("hint") {
                continue;
            }
            let Ok(mtime) = e.metadata().and_then(|m| m.modified()) else {
                continue;
            };
            let (age_secs, fresh, future) = classify(now, mtime, ttl);
            self.last.push(HintRead {
                name: e.file_name().to_string_lossy().to_string(),
                note: read_note(&path),
                age_secs,
                future,
                fresh,
            });
        }

        self.last
            .iter()
            .filter(|h| h.fresh)
            .map(|h| Reason {
                kind: "hint",
                detail: if h.note.is_empty() {
                    h.name.clone()
                } else {
                    format!("{}({})", h.name, h.note)
                },
            })
            .collect()
    }

    fn status_lines(&self, cfg: &Config) -> Vec<String> {
        if !cfg.hint_enabled {
            return vec!["  (disabled)".to_string()];
        }
        let mut lines = vec![format!(
            "  dir={}  ttl={}s",
            crate::config::hints_dir().display(),
            cfg.hint_ttl_secs
        )];
        if self.last.is_empty() {
            lines.push("    (no hint files)".to_string());
        }
        for h in &self.last {
            lines.push(format!(
                "    {:<28} age={:>4}s {} {}",
                h.name,
                h.age_secs,
                if h.future {
                    "FUTURE"
                } else if h.fresh {
                    "fresh "
                } else {
                    "STALE "
                },
                h.note
            ));
        }
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TTL: Duration = Duration::from_secs(60);

    #[test]
    fn fresh_within_ttl() {
        let now = SystemTime::now();
        let (age, fresh, future) = classify(now, now - Duration::from_secs(10), TTL);
        assert_eq!(age, 10);
        assert!(fresh);
        assert!(!future);
    }

    #[test]
    fn stale_beyond_ttl() {
        let now = SystemTime::now();
        let (age, fresh, future) = classify(now, now - Duration::from_secs(300), TTL);
        assert_eq!(age, 300);
        assert!(!fresh, "超过 TTL 必须过期 —— 写方崩溃不该把机器永久卡醒");
        assert!(!future);
    }

    #[test]
    fn exactly_at_ttl_is_still_fresh() {
        let now = SystemTime::now();
        let (_, fresh, _) = classify(now, now - TTL, TTL);
        assert!(fresh);
    }

    /// 核心回归: 时钟回跳(CMOS 电池耗尽/双系统写 RTC/快照恢复)后
    /// mtime 落在未来, 必须判为 stale。否则机器永远不睡。
    #[test]
    fn future_mtime_is_stale_not_fresh() {
        let now = SystemTime::now();
        let (_, fresh, future) = classify(now, now + Duration::from_secs(8 * 3600), TTL);
        assert!(!fresh, "未来 mtime 必须视为过期");
        assert!(future, "要标记出来以便 --status 里看得见");
    }

    #[test]
    fn note_reads_first_line_only() {
        let dir = std::env::temp_dir().join("stayawake_hint_test");
        let _ = std::fs::create_dir_all(&dir);
        let f = dir.join("t.hint");
        std::fs::write(&f, "first line\nsecond line\n").unwrap();
        assert_eq!(read_note(&f), "first line");
        let _ = std::fs::remove_file(&f);
    }

    #[test]
    fn note_truncates_long_content() {
        let dir = std::env::temp_dir().join("stayawake_hint_test");
        let _ = std::fs::create_dir_all(&dir);
        let f = dir.join("long.hint");
        std::fs::write(&f, "x".repeat(500)).unwrap();
        assert!(read_note(&f).len() <= 60);
        let _ = std::fs::remove_file(&f);
    }

    #[test]
    fn note_of_missing_file_is_empty() {
        assert_eq!(read_note(std::path::Path::new("Z:\\nope\\nope.hint")), "");
    }
}