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
    meter: RateMeter,
    consec: u32,
    kbps: f64,
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
    /// 网速差分本身就很便宜(一次 GetIfTable2), 而且不需要进程快照 -> 参与快速通道。
    /// 注意: probe 与 tick 共用同一个 RateMeter, 所以快速探测也会推进基线,
    /// 这正是我们要的 —— 更短的采样窗口能更快发现流量。
    fn probe(&mut self, cfg: &Config) -> bool {
        if !cfg.net_enabled {
            return false;
        }
        let (total, ifaces) = sample();
        self.ifaces = ifaces;
        let Some(bps) = self.meter.update(total) else {
            return false;
        };
        self.kbps = bps / 1024.0;
        // 快速通道只回答"可能有活动", 连续计数交给完整 tick
        self.kbps >= cfg.net_threshold_kbps as f64
    }

    fn tick(&mut self, cfg: &Config, _table: &ProcessTable) -> Vec<Reason> {
        if !cfg.net_enabled {
            self.consec = 0;
            self.kbps = 0.0;
            self.ifaces.clear();
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
            "  rate={:.1} KB/s ({:.2} MB/s)  threshold={} KB/s  consec={}/{}",
            self.kbps,
            self.kbps / 1024.0,
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
