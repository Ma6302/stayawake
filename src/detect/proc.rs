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
