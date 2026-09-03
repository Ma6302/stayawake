// 全局网速: GetIfTable2 一次拿到全部网卡, 累加字节计数做差分。
// 15s 窗口天然滤掉瞬时突发; 再要求连续 N 个 tick 超阈值才判定为忙。
//
// 关键坑: GetIfTable2 会把每个 LWF/QoS 过滤层驱动也列成独立条目, 与物理网卡
// 共享同一份字节计数。实测一张 Realtek 网卡出现 4 次 -> 速率虚高 4 倍。
// 必须靠 InterfaceAndOperStatusFlags 的 FilterInterface 位排除。
use windows::Win32::NetworkManagement::IpHelper::{
    FreeMibTable, GetIfTable2, IF_TYPE_SOFTWARE_LOOPBACK, MIB_IF_TABLE2,
};
use windows::Win32::NetworkManagement::Ndis::IfOperStatusUp;

use super::{Detector, ProcessTable, RateMeter, Reason};
use crate::config::Config;

/// InterfaceAndOperStatusFlags 位布局 (ifdef.h):
/// bit0 HardwareInterface, bit1 FilterInterface, bit2 ConnectorPresent, ...
const FLAG_FILTER_INTERFACE: u8 = 1 << 1;

#[derive(Default)]
pub struct NetDetector {
    /// tick 专用速率计: 保证完整检测总是拿到 poll_interval 级别的干净窗口
    meter: RateMeter,
    /// probe 专用速率计。必须与 tick 分开 —— 否则 probe 会推进共享基线,
    /// 使紧随其后的 tick 只剩 ~15ms 窗口, RateMeter 的 dt>0.05 门限直接丢弃该样本,
    /// consec 永远无法累积, 网速检测彻底失效(纯下载场景机器照常睡)。
    probe_meter: RateMeter,
    consec: u32,
    kbps: f64,
    /// probe 最近一次读到的速率, 仅供 --status 展示
    probe_kbps: f64,
    /// (网卡别名, 该网卡累计收发字节)
    ifaces: Vec<(String, u64)>,
}

/// 累加所有"真实"网卡的 In+Out 字节 (排除 loopback / 过滤层 / 未启用)
fn sample() -> (u64, Vec<(String, u64)>) {
    unsafe {
        let mut ptr: *mut MIB_IF_TABLE2 = std::ptr::null_mut();
        if GetIfTable2(&mut ptr).is_err() || ptr.is_null() {
            return (0, Vec::new());
        }
        let table = &*ptr;
        let mut total = 0u64;
        let mut ifaces = Vec::new();
        for i in 0..table.NumEntries as usize {
            let row = &*table.Table.as_ptr().add(i);
            let is_filter = row.InterfaceAndOperStatusFlags._bitfield & FLAG_FILTER_INTERFACE != 0;
            if row.OperStatus != IfOperStatusUp
                || row.Type == IF_TYPE_SOFTWARE_LOOPBACK
                || is_filter
            {
                continue;
            }
            let bytes = row.InOctets + row.OutOctets;
            total += bytes;
            ifaces.push((super::wide_to_string(&row.Alias), bytes));
        }
        let _ = FreeMibTable(ptr as *const _);
        (total, ifaces)
    }
}

impl Detector for NetDetector {
    /// 网速差分很便宜(一次 GetIfTable2)且不需要进程快照 -> 参与快速通道。
    /// 用独立的 probe_meter, 不干扰 tick 的采样窗口。
    fn probe(&mut self, cfg: &Config) -> bool {
        if !cfg.net_enabled {
            return false;
        }
        let (total, ifaces) = sample();
        self.ifaces = ifaces;
        let Some(bps) = self.probe_meter.update(total) else {
            return false; // 首轮只建基线
        };
        self.probe_kbps = bps / 1024.0;
        // 快速通道只回答"可能有活动", 连续计数交给完整 tick
        self.probe_kbps >= cfg.net_threshold_kbps as f64
    }

    fn tick(&mut self, cfg: &Config, _table: &ProcessTable) -> Vec<Reason> {
        if !cfg.net_enabled {
            self.consec = 0;
            self.kbps = 0.0;
            self.probe_kbps = 0.0;
            self.ifaces.clear();
            // 清掉基线: 否则重新启用时会把"禁用期间的累计流量"平均成一个巨大速率
            self.meter = RateMeter::default();
            self.probe_meter = RateMeter::default();
            return Vec::new();
        }
        let (total, ifaces) = sample();
        self.ifaces = ifaces;

        let Some(bps) = self.meter.update(total) else {
            return Vec::new(); // 首轮只建基线
        };
        self.kbps = bps / 1024.0;

        if self.kbps < cfg.net_threshold_kbps as f64 {
            self.consec = 0;
            return Vec::new();
        }
        self.consec += 1;
        if self.consec < cfg.net_min_consecutive_tick {
            return Vec::new();
        }
        vec![Reason {
            kind: "net",
            // 超过 1 MB/s 时用 MB/s 显示, 更好读
            detail: if self.kbps >= 1024.0 {
                format!("{:.1}MB/s", self.kbps / 1024.0)
            } else {
                format!("{:.0}KB/s", self.kbps)
            },
        }]
    }

    fn status_lines(&self, cfg: &Config) -> Vec<String> {
        if !cfg.net_enabled {
            return vec!["  (disabled)".to_string()];
        }
        let mut lines = vec![format!(
            "  rate={:.1} KB/s ({:.2} MB/s)  probe={:.1} KB/s  threshold={} KB/s  consec={}/{}",
            self.kbps,
            self.kbps / 1024.0,
            self.probe_kbps,
            cfg.net_threshold_kbps,
            self.consec,
            cfg.net_min_consecutive_tick
        )];
        for (alias, bytes) in &self.ifaces {
            lines.push(format!("    {:<34} total={} B", alias, bytes));
        }
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(threshold: u64, consec: u32) -> Config {
        let mut c = Config::from_text(crate::config::DEFAULT_CONFIG);
        c.net_threshold_kbps = threshold;
        c.net_min_consecutive_tick = consec;
        c
    }

    #[test]
    fn disabled_resets_state_and_baselines() {
        let mut d = NetDetector::default();
        let c0 = cfg(1, 1);
        d.tick(&c0, &ProcessTable::default());
        d.tick(&c0, &ProcessTable::default());

        let mut off = c0.clone();
        off.net_enabled = false;
        assert!(d.tick(&off, &ProcessTable::default()).is_empty());
        assert_eq!(d.consec, 0);
        assert_eq!(d.kbps, 0.0);
        // 基线必须清掉: 否则重新启用时会把"禁用期间的累计流量"平均成一个巨大速率
        assert!(d.meter.prev.is_none(), "tick 基线未清");
        assert!(d.probe_meter.prev.is_none(), "probe 基线未清");
    }

    #[test]
    fn first_tick_only_builds_baseline() {
        let mut d = NetDetector::default();
        // 阈值设成 1 KB/s, 即便有流量也应因"首轮无基线"而不报
        assert!(d.tick(&cfg(1, 1), &ProcessTable::default()).is_empty());
    }

    /// probe 与 tick 必须各用一个 RateMeter。
    /// 共用时 probe 会推进基线, 使紧随其后的 tick 只剩十几毫秒窗口,
    /// 被 RateMeter 的最小窗口门限丢弃 -> consec 永远累积不起来 -> 网速检测失效。
    #[test]
    fn probe_and_tick_have_independent_baselines() {
        let mut d = NetDetector::default();
        let c = cfg(1, 1);
        d.probe(&c);
        assert!(d.probe_meter.prev.is_some(), "probe 应建立自己的基线");
        assert!(d.meter.prev.is_none(), "probe 不该碰 tick 的基线");

        d.tick(&c, &ProcessTable::default());
        assert!(d.meter.prev.is_some());
    }

    /// 高阈值下静默期不该命中(本机总有零散流量, 用 u64::MAX 保证判定)
    #[test]
    fn absurd_threshold_never_fires() {
        let mut d = NetDetector::default();
        let c = cfg(u64::MAX, 1);
        for _ in 0..3 {
            std::thread::sleep(std::time::Duration::from_millis(60));
            assert!(d.tick(&c, &ProcessTable::default()).is_empty());
        }
        assert_eq!(d.consec, 0);
    }

    /// 真实网卡枚举: 不该把 LWF/QoS 过滤层驱动算进去。
    /// 那会让一张物理网卡出现多次, 速率虚高数倍(实测 Realtek 出现 4 次)。
    #[test]
    fn sample_excludes_filter_interfaces() {
        let (total, ifaces) = sample();
        // 同一别名不应重复出现
        let mut names: Vec<&String> = ifaces.iter().map(|(n, _)| n).collect();
        let before = names.len();
        names.sort();
        names.dedup();
        assert_eq!(before, names.len(), "网卡别名重复, 过滤层未排除: {:?}", ifaces);
        // 总量应等于各网卡之和
        let sum: u64 = ifaces.iter().map(|(_, b)| *b).sum();
        assert_eq!(total, sum);
    }
}