pub mod audio;
pub mod dl;
pub mod hint;
pub mod net;
pub mod proc;

use std::collections::HashMap;
use std::time::Instant;

use windows::Win32::Foundation::{CloseHandle, FILETIME};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Threading::{
    GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
};

use crate::config::Config;

/// 一条"为什么不该休眠"的理由
#[derive(Debug, Clone)]
pub struct Reason {
    pub kind: &'static str,
    pub detail: String,
}

pub trait Detector {
    /// 每 tick 调用一次; 返回本轮命中的理由(可多条)
    fn tick(&mut self, cfg: &Config, table: &ProcessTable) -> Vec<Reason>;

    /// --status / 状态详情用的原始读数
    fn status_lines(&self, cfg: &Config) -> Vec<String>;

    /// 廉价快速探测: **不打进程快照**, 只回答"是否可能有活动"。
    ///
    /// 用于快速通道: 空闲时每隔 fast_poll_secs 探一次, 命中就提前做完整 tick,
    /// 把"音乐开始播放"到"状态更新"的延迟从 poll_interval 降到 fast_poll。
    ///
    /// 允许假阳性(完整 tick 会否掉), 不允许假阴性。
    /// 默认 false = 该检测器不参与快速通道(成本太高, 如需进程快照的那些)。
    fn probe(&mut self, _cfg: &Config) -> bool {
        false
    }
}

// ───────────────────────── 进程快照 ─────────────────────────

/// 每 tick 打一次 Toolhelp 快照, 各检测器共用
#[derive(Default)]
pub struct ProcessTable {
    /// (pid, exe_name)
    procs: Vec<(u32, String)>,
}

impl ProcessTable {
    pub fn snapshot() -> ProcessTable {
        let mut t = ProcessTable::default();
        unsafe {
            let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
                return t;
            };
            let mut pe: PROCESSENTRY32W = std::mem::zeroed();
            pe.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
            if Process32FirstW(snap, &mut pe).is_ok() {
                loop {
                    t.procs
                        .push((pe.th32ProcessID, wide_to_string(&pe.szExeFile)));
                    if Process32NextW(snap, &mut pe).is_err() {
                        break;
                    }
                }
            }
            let _ = CloseHandle(snap);
        }
        t
    }

    pub fn name_of(&self, pid: u32) -> Option<&str> {
        self.procs
            .iter()
            .find(|(p, _)| *p == pid)
            .map(|(_, n)| n.as_str())
    }

    pub fn count(&self) -> usize {
        self.procs.len()
    }

    /// 名单内(不区分大小写)的所有 (pid, name)
    pub fn matching<'a>(&'a self, names: &'a [String]) -> impl Iterator<Item = (u32, &'a str)> + 'a {
        self.procs.iter().filter_map(move |(pid, name)| {
            names
                .iter()
                .any(|w| w.eq_ignore_ascii_case(name))
                .then_some((*pid, name.as_str()))
        })
    }

    pub fn alive(&self, pid: u32) -> bool {
        self.procs.iter().any(|(p, _)| *p == pid)
    }
}

pub fn wide_to_string(w: &[u16]) -> String {
    let end = w.iter().position(|&c| c == 0).unwrap_or(w.len());
    String::from_utf16_lossy(&w[..end])
}

fn filetime_u64(ft: FILETIME) -> u64 {
    ((ft.dwHighDateTime as u64) << 32) | ft.dwLowDateTime as u64
}

// ───────────────────────── CPU 采样 ─────────────────────────

/// 单个进程的一次 CPU 采样
#[derive(Clone, Copy)]
struct CpuSample {
    /// 进程创建时间, 用于防 PID 回收错认
    create: u64,
    /// 内核+用户态累计 CPU 秒
    cpu: f64,
    /// 该样本的采集时刻。**必须与样本一起存** —— 用全局时间戳会导致:
    ///   - 检测器被关闭一段时间后重新启用时, Δcpu 跨越数小时而 wall 只有一个 tick
    ///     -> 虚假的几百 %; 而托盘开关是文档化功能, 完全可复现
    ///   - 某进程连续几个 tick 读不到(提权/AV 干扰)再恢复时同样过报
    at: Instant,
}

/// 跨 tick 追踪指定 pid 的 CPU 时间, 输出"单核百分比"
/// (Δcpu / Δwall × 100; 单线程打满一核 = 100%)
#[derive(Default)]
pub struct CpuTracker {
    cache: HashMap<u32, CpuSample>,
}

/// 最小采样窗口(秒)。GetProcessTimes 的量化粒度是调度器 tick(~15.6ms),
/// 窗口太短会把一个量化单位放大成几百 % (5ms 窗口 -> 312%)。
/// 背靠背的完整 tick 是可达状态(连续两次 kick), 必须挡住。
const MIN_CPU_WINDOW: f64 = 0.3;

impl CpuTracker {
    /// 首次见到某 pid、窗口过短、或 PID 被回收时返回 None
    pub fn cpu_percent(&mut self, pid: u32) -> Option<f64> {
        let (create, cpu) = read_process_times(pid)?;
        let now = Instant::now();
        let prev = self.cache.insert(pid, CpuSample { create, cpu, at: now });

        let prev = prev?;
        if prev.create != create {
            return None; // PID 被回收, 上一个样本属于别的进程
        }
        let wall = now.duration_since(prev.at).as_secs_f64();
        if wall < MIN_CPU_WINDOW {
            return None;
        }
        Some((cpu - prev.cpu).max(0.0) / wall * 100.0)
    }

    /// 每 tick 末调用: 淘汰已退出进程
    pub fn end_tick(&mut self, table: &ProcessTable) {
        self.cache.retain(|pid, _| table.alive(*pid));
    }
}

/// (创建时间, 内核+用户态 CPU 秒)
fn read_process_times(pid: u32) -> Option<(u64, f64)> {
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut ct = FILETIME::default();
        let mut et = FILETIME::default();
        let mut kt = FILETIME::default();
        let mut ut = FILETIME::default();
        let ok = GetProcessTimes(h, &mut ct, &mut et, &mut kt, &mut ut).is_ok();
        let _ = CloseHandle(h);
        if !ok {
            return None;
        }
        let cpu = (filetime_u64(kt) + filetime_u64(ut)) as f64 / 10_000_000.0;
        Some((filetime_u64(ct), cpu))
    }
}

// ───────────────────────── 速率差分工具 ─────────────────────────

/// 累计计数器 -> 速率 (单位/秒)。首次调用返回 None。
#[derive(Default)]
pub struct RateMeter {
    prev: Option<(u64, Instant)>,
}

impl RateMeter {
    pub fn update(&mut self, total: u64) -> Option<f64> {
        self.update_at(total, Instant::now())
    }

    /// 供单测注入时刻。
    fn update_at(&mut self, total: u64, now: Instant) -> Option<f64> {
        let rate = self.prev.and_then(|(prev_total, prev_t)| {
            let dt = now.duration_since(prev_t).as_secs_f64();
            (dt > 0.05).then(|| total.saturating_sub(prev_total) as f64 / dt)
        });
        self.prev = Some((total, now));
        rate
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn rate_meter_first_call_has_no_baseline() {
        let mut m = RateMeter::default();
        assert_eq!(m.update_at(1000, Instant::now()), None);
    }

    #[test]
    fn rate_meter_computes_per_second_rate() {
        let t0 = Instant::now();
        let mut m = RateMeter::default();
        m.update_at(0, t0);
        let r = m.update_at(2048, t0 + Duration::from_secs(2)).unwrap();
        assert!((r - 1024.0).abs() < 0.001, "得到 {}", r);
    }

    /// 窗口过短要返回 None 而不是一个被放大的数字。
    /// 这是 1.1 缺陷的根源: probe 推进基线后 tick 只剩十几毫秒。
    #[test]
    fn rate_meter_rejects_tiny_window() {
        let t0 = Instant::now();
        let mut m = RateMeter::default();
        m.update_at(0, t0);
        assert_eq!(m.update_at(1_000_000, t0 + Duration::from_millis(15)), None);
    }

    /// 计数器回绕/重置时不能给出负数或天文数字
    #[test]
    fn rate_meter_survives_counter_reset() {
        let t0 = Instant::now();
        let mut m = RateMeter::default();
        m.update_at(10_000, t0);
        let r = m.update_at(5, t0 + Duration::from_secs(1)).unwrap();
        assert_eq!(r, 0.0);
    }

    /// 独立的两个计量器互不干扰 —— net.rs 依赖这一点(probe / tick 各一个)
    #[test]
    fn separate_meters_do_not_share_baseline() {
        let t0 = Instant::now();
        let mut probe = RateMeter::default();
        let mut tick = RateMeter::default();
        probe.update_at(0, t0);
        tick.update_at(0, t0);

        // probe 每 2s 采一次
        probe.update_at(1024, t0 + Duration::from_secs(2));
        // tick 在 probe 之后 15ms 采样, 但它有自己的基线, 窗口是完整的 2.015s
        let r = tick
            .update_at(1024, t0 + Duration::from_millis(2015))
            .expect("tick 应拿到完整窗口");
        assert!(r > 400.0 && r < 600.0, "得到 {}", r);
    }

    #[test]
    fn cpu_tracker_first_sighting_returns_none() {
        let mut t = CpuTracker::default();
        let me = std::process::id();
        assert_eq!(t.cpu_percent(me), None, "首次见到某 pid 无法算差值");
    }

    /// 最小窗口保护: 背靠背两次采样必须返回 None, 否则
    /// GetProcessTimes 的 15.6ms 量化会被放大成几百 %
    #[test]
    fn cpu_tracker_rejects_back_to_back_samples() {
        let mut t = CpuTracker::default();
        let me = std::process::id();
        t.cpu_percent(me);
        assert_eq!(t.cpu_percent(me), None, "窗口远小于 MIN_CPU_WINDOW");
    }

    #[test]
    fn cpu_tracker_reports_after_min_window() {
        let mut t = CpuTracker::default();
        let me = std::process::id();
        t.cpu_percent(me);
        // 睡过最小窗口, 期间本进程几乎不耗 CPU
        std::thread::sleep(Duration::from_secs_f64(MIN_CPU_WINDOW + 0.1));
        let pct = t.cpu_percent(me).expect("窗口足够, 应能算出");
        assert!((0.0..300.0).contains(&pct), "得到 {}", pct);
    }

    #[test]
    fn cpu_tracker_ignores_unknown_pid() {
        let mut t = CpuTracker::default();
        // pid 0 是 System Idle Process, OpenProcess 必失败
        assert_eq!(t.cpu_percent(0), None);
    }

    /// PID 被回收时必须丢弃旧样本, 否则会把别的进程的 CPU 算进来
    #[test]
    fn cpu_tracker_detects_pid_reuse() {
        let mut t = CpuTracker::default();
        let me = std::process::id();
        t.cpu_percent(me);
        // 手动把创建时间改掉, 模拟"同一 pid 换了进程"
        if let Some(s) = t.cache.get_mut(&me) {
            s.create ^= 0xDEAD_BEEF;
            s.at = Instant::now() - Duration::from_secs(10);
        }
        assert_eq!(t.cpu_percent(me), None, "创建时间不同应判为 PID 复用");
    }
}
