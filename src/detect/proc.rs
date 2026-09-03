// 进程白名单 + CPU 占用: 只有"高 CPU"才算忙。
// 这样常驻但空闲的进程(编辑器、语言服务器)不会导致永不休眠。
use std::collections::BTreeMap;

use super::{CpuTracker, Detector, ProcessTable, Reason};
use crate::config::Config;

#[derive(Default)]
pub struct ProcDetector {
    cpu: CpuTracker,
    /// 进程名 -> 同名进程 CPU 之和(单核%), 供 --status 展示
    last: BTreeMap<String, f64>,
}

impl Detector for ProcDetector {
    fn tick(&mut self, cfg: &Config, table: &ProcessTable) -> Vec<Reason> {
        self.last.clear();
        if !cfg.proc_enabled || cfg.proc_busy_when_cpu.is_empty() {
            self.cpu.end_tick(table);
            return Vec::new();
        }

        // 同名进程(如多个 rustc)的 CPU 相加, 按进程名聚合
        for (pid, name) in table.matching(&cfg.proc_busy_when_cpu) {
            if let Some(pct) = self.cpu.cpu_percent(pid) {
                *self.last.entry(name.to_string()).or_insert(0.0) += pct;
            } else {
                // 首次见到: 建条目但不计数, 下一 tick 才有差值
                self.last.entry(name.to_string()).or_insert(0.0);
            }
        }
        self.cpu.end_tick(table);

        let threshold = cfg.proc_cpu_percent_1core.max(0.1);
        self.last
            .iter()
            .filter(|(_, pct)| **pct >= threshold)
            .map(|(name, pct)| Reason {
                kind: "proc",
                detail: format!("{} {:.0}%", name, pct),
            })
            .collect()
    }

    fn status_lines(&self, cfg: &Config) -> Vec<String> {
        if !cfg.proc_enabled {
            return vec!["  (disabled)".to_string()];
        }
        let mut lines = vec![format!(
            "  threshold={:.1}%/core  watching={} name(s)",
            cfg.proc_cpu_percent_1core,
            cfg.proc_busy_when_cpu.len()
        )];
        if self.last.is_empty() {
            lines.push("    (no watched process running)".to_string());
        }
        for (name, pct) in &self.last {
            lines.push(format!("    {:<24} cpu={:>7.1}%/core", name, pct));
        }
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(threshold: f64) -> Config {
        let mut c = Config::from_text(crate::config::DEFAULT_CONFIG);
        c.proc_cpu_percent_1core = threshold;
        c
    }

    /// 名单为空时不该做任何事(也不该 panic)
    #[test]
    fn empty_whitelist_yields_nothing() {
        let mut d = ProcDetector::default();
        let mut c = cfg(5.0);
        c.proc_busy_when_cpu.clear();
        assert!(d.tick(&c, &ProcessTable::snapshot()).is_empty());
    }

    #[test]
    fn disabled_yields_nothing() {
        let mut d = ProcDetector::default();
        let mut c = cfg(5.0);
        c.proc_enabled = false;
        assert!(d.tick(&c, &ProcessTable::snapshot()).is_empty());
    }

    /// 首次 tick 只建立基线, 不该凭空报出 CPU 占用 ——
    /// 否则刚启动就会误判"有编译在跑"
    #[test]
    fn first_tick_never_reports() {
        let mut d = ProcDetector::default();
        let mut c = cfg(0.1);
        // 把本测试进程自己加进名单, 保证名单必定命中
        c.proc_busy_when_cpu = vec!["stayawake.exe".into(), current_exe_name()];
        let out = d.tick(&c, &ProcessTable::snapshot());
        assert!(out.is_empty(), "首轮无差值可算, 得到 {:?}", out);
    }

    /// proc 不参与快速通道(需要进程快照, 太贵)
    #[test]
    fn does_not_participate_in_fast_probe() {
        let mut d = ProcDetector::default();
        assert!(!d.probe(&cfg(5.0)));
    }

    fn current_exe_name() -> String {
        std::env::current_exe()
            .ok()
            .and_then(|p| p.file_name().map(|s| s.to_string_lossy().into_owned()))
            .unwrap_or_default()
    }
}