// 下载器检测: 专治"托盘常驻 + I/O 密集 + CPU≈0"的下载器(IDM 实测 CPU 恒为 0)。
//
// 两条独立判据, 任一命中即算忙:
//   1) 进程 Read+Write 传输速率超阈值  —— 正常速度下载
//   2) established TCP 连接数超阈值    —— 服务器卡住、速度接近 0 但下载仍在进行
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

#[derive(Default)]
pub struct DlDetector {
    /// pid -> I/O 速率计
    meters: HashMap<u32, RateMeter>,
    /// 进程名 -> (io KB/s 之和, established 连接数之和)
    last: BTreeMap<String, (f64, u32)>,
}

impl Detector for DlDetector {
    fn tick(&mut self, cfg: &Config, table: &ProcessTable) -> Vec<Reason> {
        self.last.clear();
        if !cfg.dl_enabled || cfg.dl_processes.is_empty() {
            self.meters.clear();
            return Vec::new();
        }

        let conns = established_per_pid();
        let mut seen: Vec<u32> = Vec::new();

        for (pid, name) in table.matching(&cfg.dl_processes) {
            seen.push(pid);
            let kbps = process_io_bytes(pid)
                .and_then(|bytes| self.meters.entry(pid).or_default().update(bytes))
                .map(|bps| bps / 1024.0)
                .unwrap_or(0.0);
            let n_conn = conns.get(&pid).copied().unwrap_or(0);
            let e = self.last.entry(name.to_string()).or_insert((0.0, 0));
            e.0 += kbps;
            e.1 += n_conn;
        }
        self.meters.retain(|pid, _| seen.contains(pid));

        self.last
            .iter()
            .filter(|(_, (kbps, n_conn))| {
                *kbps >= cfg.dl_io_kbps as f64 || *n_conn >= cfg.dl_tcp_conns
            })
            .map(|(name, (kbps, n_conn))| Reason {
                kind: "dl",
                detail: format!("{} io={:.0}KB/s tcp={}", name, kbps, n_conn),
            })
            .collect()
    }

    fn status_lines(&self, cfg: &Config) -> Vec<String> {
        if !cfg.dl_enabled {
            return vec!["  (disabled)".to_string()];
        }
        let mut lines = vec![format!(
            "  io>={}KB/s or tcp>={}  watching=[{}]",
            cfg.dl_io_kbps,
            cfg.dl_tcp_conns,
            cfg.dl_processes.join(", ")
        )];
        if self.last.is_empty() {
            lines.push("    (no downloader running)".to_string());
        }
        for (name, (kbps, n_conn)) in &self.last {
            lines.push(format!(
                "    {:<24} io={:>9.1} KB/s  established_tcp={}",
                name, kbps, n_conn
            ));
        }
        lines
    }
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

/// 取 TCP 表原始字节。两次调用: 先问大小, 再取数据。
fn tcp_table_bytes(family: u32) -> Option<Vec<u8>> {
    unsafe {
        let mut size = 0u32;
        let r = GetExtendedTcpTable(None, &mut size, false, family, TCP_TABLE_OWNER_PID_ALL, 0);
        if r != ERROR_INSUFFICIENT_BUFFER.0 || size == 0 {
            return None;
        }
        let mut buf = vec![0u8; size as usize];
        let r = GetExtendedTcpTable(
            Some(buf.as_mut_ptr() as *mut _),
            &mut size,
            false,
            family,
            TCP_TABLE_OWNER_PID_ALL,
            0,
        );
        (r == NO_ERROR.0).then_some(buf)
    }
}

fn count_v4(map: &mut HashMap<u32, u32>) {
    let Some(buf) = tcp_table_bytes(AF_INET) else { return };
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
    let Some(buf) = tcp_table_bytes(AF_INET6) else { return };
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
