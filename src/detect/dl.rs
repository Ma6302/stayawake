// 下载器检测: 专治"托盘常驻 + I/O 密集 + CPU≈0"的下载器(IDM 实测 CPU 恒为 0)。
//
// 判据(两者取或, 但都要求有真实数据流动):
//   1) I/O 速率 >= dl_io_kbps                          —— 正常速度下载
//   2) established TCP >= dl_tcp_conns 且 I/O >= 下限   —— 低速但仍在传输
//
// 第 2 条为什么不能只看连接数: 那会把"持有大量连接但零吞吐"的程序永久判为忙。
// BT 客户端做种时正是这样(几十条 established, 吞吐为 0), 会导致机器再也不睡 ——
// 与 audio.rs 里 Wallpaper Engine 的问题同一类。
//
// 第 2 条为什么也不能用 "io > 0": 实测代理/常驻网络程序的心跳流量稳定在
// 约 0.09 KB/s(verge-mihomo 空闲, 5 条 established), 严格大于 0 就会永久命中。
// 所以取一个明确高于心跳、明确低于真实下载的下限。
const LOW_RATE_FLOOR_KBPS: f64 = 5.0;
//
// I/O 阈值只对配置里的名单生效: Electron 类应用的进程间共享内存流量能到 GB/s,
// 全局启用必然假阳性(实测 OpenCode renderer↔gpu 有 7.3 MB/s 且一字节未落盘)。
use std::collections::{BTreeMap, HashMap};

use windows::Win32::Foundation::{CloseHandle, ERROR_INSUFFICIENT_BUFFER, NO_ERROR};
use windows::Win32::NetworkManagement::IpHelper::{
    GetExtendedTcpTable, MIB_TCP6TABLE_OWNER_PID, MIB_TCPTABLE_OWNER_PID, TCP_TABLE_OWNER_PID_ALL,
};
use windows::Win32::System::Threading::{
    GetProcessIoCounters, OpenProcess, IO_COUNTERS, PROCESS_QUERY_LIMITED_INFORMATION,
};

use super::{Detector, ProcessTable, RateMeter, Reason};
use crate::config::Config;

const AF_INET: u32 = 2;
const AF_INET6: u32 = 23;
const MIB_TCP_STATE_ESTAB: u32 = 5;

/// 单个下载器进程名的聚合读数
#[derive(Default, Clone, Copy)]
struct DlRead {
    /// I/O 速率 (KB/s); 读取失败的进程不计入
    kbps: f64,
    /// established TCP 连接数
    conns: u32,
    /// 有多少个同名进程的 I/O 读取失败(提权进程等) —— 与"速率为 0"必须区分
    io_unreadable: u32,
}

#[derive(Default)]
pub struct DlDetector {
    /// pid -> I/O 速率计
    meters: HashMap<u32, RateMeter>,
    /// 进程名 -> 聚合读数
    last: BTreeMap<String, DlRead>,
}

impl Detector for DlDetector {
    fn tick(&mut self, cfg: &Config, table: &ProcessTable) -> Vec<Reason> {
        self.last.clear();
        if !cfg.dl_enabled || cfg.dl_processes.is_empty() {
            self.meters.clear();
            return Vec::new();
        }

        // 名单内没有进程在跑时不必枚举全系统 TCP 表(4 次 syscall + 数百条记录)
        let watched: Vec<(u32, String)> = table
            .matching(&cfg.dl_processes)
            .map(|(pid, name)| (pid, name.to_string()))
            .collect();
        if watched.is_empty() {
            self.meters.clear();
            return Vec::new();
        }
        let conns = established_per_pid();

        for (pid, name) in &watched {
            let e = self.last.entry(name.clone()).or_default();
            e.conns += conns.get(pid).copied().unwrap_or(0);
            match process_io_bytes(*pid) {
                // 读到了累计字节: 交给速率计。它可能因窗口太短/首轮返回 None,
                // 那是"暂时算不出速率", 与"读不到"是两件事, 绝不能混为一谈 ——
                // 混淆会让 io_unreadable 兜底路径在每次背靠背 tick 时误触发。
                Some(bytes) => {
                    if let Some(bps) = self.meters.entry(*pid).or_default().update(bytes) {
                        e.kbps += bps / 1024.0;
                    }
                }
                // OpenProcess/GetProcessIoCounters 失败(提权进程等):
                // 不能当成 0 KB/s, 否则 --status 会显示得像是测量到的
                None => e.io_unreadable += 1,
            }
        }
        self.meters.retain(|pid, _| watched.iter().any(|(p, _)| p == pid));

        self.last
            .iter()
            .filter(|(_, r)| is_busy(r, cfg))
            .map(|(name, r)| Reason {
                kind: "dl",
                detail: format!("{} io={:.0}KB/s tcp={}", name, r.kbps, r.conns),
            })
            .collect()
    }

    fn status_lines(&self, cfg: &Config) -> Vec<String> {
        if !cfg.dl_enabled {
            return vec!["  (disabled)".to_string()];
        }
        let mut lines = vec![format!(
            "  busy if io>={}KB/s, or (tcp>={} and io>={:.0}KB/s)  watching=[{}]",
            cfg.dl_io_kbps,
            cfg.dl_tcp_conns,
            LOW_RATE_FLOOR_KBPS,
            cfg.dl_processes.join(", ")
        )];
        if self.last.is_empty() {
            lines.push("    (no downloader running)".to_string());
        }
        for (name, r) in &self.last {
            lines.push(format!(
                "    {:<24} io={:>9.1} KB/s  established_tcp={}{}",
                name,
                r.kbps,
                r.conns,
                if r.io_unreadable > 0 {
                    format!("  [{} proc io unreadable]", r.io_unreadable)
                } else {
                    String::new()
                }
            ));
        }
        lines
    }
}

/// 判据见文件头注释。关键: 连接数多但吞吐低于心跳量级 **不算** 忙。
fn is_busy(r: &DlRead, cfg: &Config) -> bool {
    // 主判据: I/O 速率达标
    if r.kbps >= cfg.dl_io_kbps as f64 {
        return true;
    }
    // 补充判据: 连接数多 + 吞吐明显高于常驻心跳(低速下载/服务器限速)
    if r.conns >= cfg.dl_tcp_conns && r.kbps >= LOW_RATE_FLOOR_KBPS {
        return true;
    }
    // 所有同名进程的 I/O 都读不到(提权等)时无法用速率判据,
    // 退化为只看连接数 —— 否则这类下载器完全检测不到
    if r.io_unreadable > 0 && r.kbps == 0.0 && r.conns >= cfg.dl_tcp_conns {
        return true;
    }
    false
}

/// 进程累计 Read+Write 传输字节
fn process_io_bytes(pid: u32) -> Option<u64> {
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut io = IO_COUNTERS::default();
        let ok = GetProcessIoCounters(h, &mut io).is_ok();
        let _ = CloseHandle(h);
        ok.then_some(io.ReadTransferCount + io.WriteTransferCount)
    }
}

/// 全表扫一次, 统计每个 pid 的 established 连接数 (IPv4 + IPv6)
fn established_per_pid() -> HashMap<u32, u32> {
    let mut map = HashMap::new();
    count_v4(&mut map);
    count_v6(&mut map);
    map
}

/// 取 TCP 表原始字节。
///
/// 返回 `Vec<u32>` 而不是 `Vec<u8>`: `MIB_TCPTABLE_OWNER_PID` 要求 4 字节对齐,
/// 而 `Vec<u8>` 的对齐是 1。把 u8 指针转成结构体指针再解引用会构造未对齐引用,
/// 是 UB —— 目前"能用"只是因为 Windows 分配器恰好返回 ≥8 字节对齐的块。
///
/// 表可能在两次调用之间变大, 所以带有限重试。
fn tcp_table_words(family: u32) -> Option<Vec<u32>> {
    unsafe {
        let mut size = 0u32;
        let r = GetExtendedTcpTable(None, &mut size, false, family, TCP_TABLE_OWNER_PID_ALL, 0);
        if r != ERROR_INSUFFICIENT_BUFFER.0 || size == 0 {
            return None;
        }
        // 表在两次调用之间增长时 size 会被更新, 重试即可; 3 次足够
        for _ in 0..3 {
            let words = (size as usize).div_ceil(4);
            let mut buf = vec![0u32; words];
            let mut cap = (words * 4) as u32;
            let r = GetExtendedTcpTable(
                Some(buf.as_mut_ptr() as *mut _),
                &mut cap,
                false,
                family,
                TCP_TABLE_OWNER_PID_ALL,
                0,
            );
            if r == NO_ERROR.0 {
                return Some(buf);
            }
            if r != ERROR_INSUFFICIENT_BUFFER.0 {
                return None;
            }
            size = cap; // 表变大了, 用新尺寸再试
        }
        None
    }
}

fn count_v4(map: &mut HashMap<u32, u32>) {
    let Some(buf) = tcp_table_words(AF_INET) else { return };
    unsafe {
        let t = buf.as_ptr() as *const MIB_TCPTABLE_OWNER_PID;
        let n = (*t).dwNumEntries as usize;
        for row in std::slice::from_raw_parts((*t).table.as_ptr(), n) {
            if row.dwState == MIB_TCP_STATE_ESTAB {
                *map.entry(row.dwOwningPid).or_insert(0) += 1;
            }
        }
    }
}

fn count_v6(map: &mut HashMap<u32, u32>) {
    let Some(buf) = tcp_table_words(AF_INET6) else { return };
    unsafe {
        // IPv6 行布局与 IPv4 完全不同(dwState 在倒数第二个字段), 必须用各自的类型
        let t = buf.as_ptr() as *const MIB_TCP6TABLE_OWNER_PID;
        let n = (*t).dwNumEntries as usize;
        for row in std::slice::from_raw_parts((*t).table.as_ptr(), n) {
            if row.dwState == MIB_TCP_STATE_ESTAB {
                *map.entry(row.dwOwningPid).or_insert(0) += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> Config {
        Config::from_text(crate::config::DEFAULT_CONFIG)
    }

    #[test]
    fn high_throughput_is_busy() {
        let r = DlRead { kbps: 500.0, conns: 0, io_unreadable: 0 };
        assert!(is_busy(&r, &cfg()));
    }

    /// 核心回归: 连接数多但吞吐是心跳量级(实测代理软件 0.09 KB/s) 不算忙。
    /// 否则 BT 做种 / 常驻代理会让机器永远不睡。
    #[test]
    fn many_connections_with_heartbeat_only_is_idle() {
        let r = DlRead { kbps: 0.09, conns: 50, io_unreadable: 0 };
        assert!(!is_busy(&r, &cfg()), "心跳流量不该算忙");
    }

    #[test]
    fn many_connections_with_zero_io_is_idle() {
        let r = DlRead { kbps: 0.0, conns: 50, io_unreadable: 0 };
        assert!(!is_busy(&r, &cfg()));
    }

    /// 低速但真实的下载: 连接数够 + 吞吐高于心跳下限
    #[test]
    fn slow_but_real_download_is_busy() {
        let r = DlRead { kbps: 20.0, conns: 8, io_unreadable: 0 };
        assert!(is_busy(&r, &cfg()), "20KB/s + 8 连接应算忙");
    }

    /// 吞吐够但连接数不足(如单线程下载), 靠主判据兜住
    #[test]
    fn low_conns_needs_main_threshold() {
        let c = cfg();
        // 恰好低于 dl_io_kbps(50) 且连接数不足 -> 不忙
        assert!(!is_busy(&DlRead { kbps: 30.0, conns: 1, io_unreadable: 0 }, &c));
        // 达到主阈值 -> 忙
        assert!(is_busy(&DlRead { kbps: 60.0, conns: 1, io_unreadable: 0 }, &c));
    }

    /// I/O 读不到(提权进程)时退化为只看连接数, 否则这类下载器完全检测不到
    #[test]
    fn unreadable_io_falls_back_to_conns() {
        let c = cfg();
        assert!(is_busy(&DlRead { kbps: 0.0, conns: 8, io_unreadable: 1 }, &c));
        // 但连接数也不够就还是不忙
        assert!(!is_busy(&DlRead { kbps: 0.0, conns: 1, io_unreadable: 1 }, &c));
    }

    /// 心跳下限必须明显高于实测值(0.09)、明显低于任何真实下载。
    /// 常量断言在 clippy 下会被抱怨, 所以改为验证它在判据里的实际效果。
    #[test]
    fn heartbeat_floor_separates_idle_from_download() {
        let c = cfg();
        // 实测的代理心跳量级
        assert!(!is_busy(&DlRead { kbps: 0.09, conns: 20, io_unreadable: 0 }, &c));
        assert!(!is_busy(&DlRead { kbps: 1.0, conns: 20, io_unreadable: 0 }, &c));
        // 明确的低速下载
        assert!(is_busy(&DlRead { kbps: 10.0, conns: 20, io_unreadable: 0 }, &c));
    }

    /// 本机真实调用: 不崩溃、返回的映射里没有荒谬的计数
    #[test]
    fn established_per_pid_works_on_real_system() {
        let map = established_per_pid();
        for (&pid, &n) in &map {
            assert!(pid != 0 || n > 0);
            assert!(n < 100_000, "pid {} 有 {} 条连接?", pid, n);
        }
    }
}
