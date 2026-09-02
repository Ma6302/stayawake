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

/// 跨 tick 追踪指定 pid 的 CPU 时间, 输出"单核百分比"
/// (Δcpu / Δwall × 100; 单线程打满一核 = 100%)
#[derive(Default)]
pub struct CpuTracker {
    /// pid -> (进程创建时间, 累计 cpu 秒)  —— 创建时间用于防 PID 回收错认
    cache: HashMap<u32, (u64, f64)>,
    last_sample: Option<Instant>,
}

impl CpuTracker {
    /// 首次见到某 pid 时返回 None(还没有差值可算)
    pub fn cpu_percent(&mut self, pid: u32) -> Option<f64> {
        let now = Instant::now();
        let wall = self.last_sample.map(|t| (now - t).as_secs_f64());

        let (create, cpu_now) = read_process_times(pid)?;
        let prev = self.cache.insert(pid, (create, cpu_now));

        let (wall, (prev_create, prev_cpu)) = (wall?, prev?);
        if prev_create != create || wall <= 0.0 {
            return None; // PID 被回收, 或时间未推进
        }
        Some((cpu_now - prev_cpu).max(0.0) / wall * 100.0)
    }

    /// 每 tick 末调用: 淘汰已退出进程, 推进时间基准
    pub fn end_tick(&mut self, table: &ProcessTable) {
        self.cache.retain(|pid, _| table.alive(*pid));
        self.last_sample = Some(Instant::now());
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
        let now = Instant::now();
        let rate = self.prev.and_then(|(prev_total, prev_t)| {
            let dt = (now - prev_t).as_secs_f64();
            (dt > 0.05).then(|| total.saturating_sub(prev_total) as f64 / dt)
        });
        self.prev = Some((total, now));
        rate
    }
}
