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
            // mtime 可能因时钟调整落在未来, duration_since 会失败 -> 视为刚刷新
            let age = now.duration_since(mtime).unwrap_or(Duration::ZERO);
            self.last.push(HintRead {
                name: e.file_name().to_string_lossy().to_string(),
                note: read_note(&path),
                age_secs: age.as_secs(),
                fresh: age <= ttl,
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
                if h.fresh { "fresh " } else { "STALE " },
                h.note
            ));
        }
        lines
    }
}
