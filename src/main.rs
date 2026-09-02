#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod autostart;
mod config;
mod detect;
mod engine;
mod log;
mod power;
mod tray;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use engine::{Engine, Mode, Snapshot};
use power::{Held, PowerSource};
use windows::core::w;
use windows::Win32::Foundation::{
    ERROR_ALREADY_EXISTS, HANDLE, HWND, LPARAM, WAIT_OBJECT_0, WPARAM,
};
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};
use windows::Win32::System::Threading::{
    CreateEventW, CreateMutexW, CreateWaitableTimerExW, GetCurrentProcess, ResetEvent,
    SetPriorityClass, SetProcessInformation, SetWaitableTimer, WaitForMultipleObjects,
    BELOW_NORMAL_PRIORITY_CLASS, CREATE_WAITABLE_TIMER_MANUAL_RESET, PROCESS_POWER_THROTTLING_STATE,
    PROCESS_POWER_THROTTLING_EXECUTION_SPEED, ProcessPowerThrottling, SYNCHRONIZATION_SYNCHRONIZE,
    TIMER_MODIFY_STATE,
};
use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_APP};

/// worker -> UI: 状态已更新, 请刷新托盘
pub const WM_STATE_CHANGED: u32 = WM_APP + 1;

/// UI 与 worker 之间的共享状态
pub struct Shared {
    pub mode: Mutex<Mode>,
    pub snapshot: Mutex<Option<Snapshot>>,
    /// 显示器是否点亮 (由 UI 线程的 GUID_CONSOLE_DISPLAY_STATE 通知维护)
    pub display_on: AtomicBool,
    /// 置位表示配置需重载
    pub reload: AtomicBool,
    /// UI 线程用它踢醒 worker (模式切换、电源事件、重载配置)
    pub kick: HANDLE,
}

impl Shared {
    pub fn wake_worker(&self) {
        unsafe {
            let _ = windows::Win32::System::Threading::SetEvent(self.kick);
        }
    }
}

fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("--status") => print_status(),
        Some("--install-autostart") => println!("{}", autostart::install()),
        Some("--uninstall-autostart") => println!("{}", autostart::uninstall()),
        Some("--help" | "-h") => print!("{}", HELP),
        Some(other) => {
            eprintln!("未知参数: {}\n", other);
            print!("{}", HELP);
        }
        None => run_daemon(),
    }
}

const HELP: &str = r#"stayawake - 基于真实活动检测的休眠抑制守护进程

用法:
  stayawake                       以守护进程运行(托盘图标)
  stayawake --status              打印一轮全部检测器读数后退出
  stayawake --install-autostart   注册开机自启(登录触发的计划任务)
  stayawake --uninstall-autostart 移除开机自启

路径:
  配置  %LOCALAPPDATA%\stayawake\config.ini   (首次运行自动生成)
  日志  %LOCALAPPDATA%\stayawake\stayawake.log (仅记录状态跃变)
  提示  %LOCALAPPDATA%\stayawake\hints\*.hint  (touch 即保持唤醒, 删除即释放)
"#;

// ───────────────────────────── 守护进程 ─────────────────────────────

fn run_daemon() {
    if !claim_single_instance() {
        return;
    }
    let (eco, prio) = lower_footprint();

    let kick = unsafe { CreateEventW(None, false, false, None) }.expect("CreateEventW");
    let shared = Arc::new(Shared {
        mode: Mutex::new(Mode::Auto),
        snapshot: Mutex::new(None),
        display_on: AtomicBool::new(true),
        reload: AtomicBool::new(false),
        kick,
    });

    log::event(&format!(
        "=== stayawake started === (EcoQoS={} priority={})",
        eco, prio
    ));

    let (hwnd_tx, hwnd_rx) = mpsc::channel::<HWND>();
    {
        let shared = shared.clone();
        std::thread::Builder::new()
            .name("stayawake-worker".into())
            .spawn(move || worker(shared, hwnd_rx))
            .expect("spawn worker");
    }

    // 主线程跑消息循环(托盘图标必须在有消息泵的线程上)
    tray::run(shared, hwnd_tx);
    log::event("=== stayawake stopped ===");
}

/// 已有实例在跑就返回 false。
/// 句柄故意不关闭: 互斥体需活到进程结束, 由系统回收。
fn claim_single_instance() -> bool {
    unsafe {
        let Ok(_mutex) = CreateMutexW(None, true, w!("Local\\stayawake_single_instance")) else {
            return false;
        };
        windows::Win32::Foundation::GetLastError() != Err(ERROR_ALREADY_EXISTS.into())
    }
}

/// EcoQoS + 低于正常优先级: 12700H 有 E-core, 明确要求调度器把我们丢到小核低频跑。
/// 绝不调 timeBeginPeriod —— 那才是同类工具伤害游戏帧生成时间的真正原因。
fn lower_footprint() -> (bool, bool) {
    unsafe {
        let state = PROCESS_POWER_THROTTLING_STATE {
            Version: 1,
            ControlMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
            StateMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
        };
        let eco = SetProcessInformation(
            GetCurrentProcess(),
            ProcessPowerThrottling,
            &state as *const _ as *const core::ffi::c_void,
            std::mem::size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
        )
        .is_ok();
        // 不用 PROCESS_MODE_BACKGROUND_BEGIN: 它会把 I/O 与内存优先级压得过死
        let prio = SetPriorityClass(GetCurrentProcess(), BELOW_NORMAL_PRIORITY_CLASS).is_ok();
        (eco, prio)
    }
}

/// worker 必须是唯一持有 execution state 的线程 —— SetThreadExecutionState 是线程级的,
/// 持有者线程退出即释放。所以它常驻并独占调用 apply_hold。
fn worker(shared: Arc<Shared>, hwnd_rx: mpsc::Receiver<HWND>) {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    let mut cfg = config::Config::load_or_create();
    let mut engine = Engine::new();
    let mut hwnd: Option<HWND> = None;
    // 上次完整检测的时刻。None = 还没做过, 立刻做一次。
    // 不能用 `Instant::now() - 很久` 当哨兵值: Instant 是单调时钟,
    // 开机时长不足该值时会下溢 panic (实测开机 1.25h 时 -86400s 直接崩)。
    let mut last_full: Option<Instant> = None;

    let timer = unsafe {
        CreateWaitableTimerExW(
            None,
            None,
            CREATE_WAITABLE_TIMER_MANUAL_RESET,
            (TIMER_MODIFY_STATE | SYNCHRONIZATION_SYNCHRONIZE).0,
        )
    }
    .expect("CreateWaitableTimerExW");

    loop {
        if shared.reload.swap(false, Ordering::SeqCst) {
            cfg = config::Config::load_or_create();
            last_full = None; // 配置变了, 立刻重做完整检测
            log::event("config reloaded");
        }
        if hwnd.is_none() {
            hwnd = hwnd_rx.try_recv().ok();
        }

        // ── 快速通道 ──
        // 空闲时用廉价手段(设备音频峰值 / 网卡字节数 / 提示文件)每 fast_poll_secs 探一次,
        // 不打进程快照(那是每轮 ~11ms 里的大头)。探到疑似活动就立刻做完整检测。
        // 这把"音乐开始播放"到"状态更新"的延迟从 poll_interval 降到 fast_poll。
        //
        // 已持有请求时不启用: 那时关心的是"活动何时结束", 由宽限期兜着, 早晚几秒无所谓。
        let full_due = last_full
            .map(|t| t.elapsed() >= Duration::from_secs(cfg.poll_interval_secs))
            .unwrap_or(true);

        if watching(&cfg, &engine, &shared) && !full_due && !engine.probe(&cfg) {
            // 没有活动迹象, 继续快速探测
            wait_next(timer, shared.kick, cfg.fast_poll_secs);
            continue;
        }

        let mode = *shared.mode.lock().unwrap();
        let display_on = shared.display_on.load(Ordering::SeqCst);
        let prev_held = engine.held();
        let (snap, changed) = engine.step(&cfg, mode, display_on);
        last_full = Some(Instant::now());

        // Force 到点自动回 Auto
        if let Mode::Force(until) = mode {
            if Instant::now() >= until {
                *shared.mode.lock().unwrap() = Mode::Auto;
            }
        }

        if changed {
            let what = if snap.reasons.is_empty() {
                if snap.in_grace { "(grace)".into() } else { "(idle)".into() }
            } else {
                snap.reasons.join(" · ")
            };
            log::event(&format!(
                "{:<14} ac={} disp={} {}",
                snap.held.label(),
                if snap.ac { 1 } else { 0 },
                if snap.display_on { "on" } else { "off" },
                what
            ));
            // 可选: 宽限期结束后主动让机器睡(默认关闭)
            if cfg.sleep_on_release && prev_held != Held::None && snap.held == Held::None {
                log::event("sleep_on_release -> SetSuspendState");
                power::suspend_now();
            }
        }

        *shared.snapshot.lock().unwrap() = Some(snap);
        if let Some(h) = hwnd {
            unsafe {
                let _ = PostMessageW(h, WM_STATE_CHANGED, WPARAM(0), LPARAM(0));
            }
        }

        // 一次性定时器每轮重新武装。周期模式在本机 Insider 上会连续触发导致 tick 风暴,
        // 一次性 + 手动重武装可靠。
        //
        // 关键: 这里也要用快速间隔。否则"完整检测发现无活动"之后会睡满 poll_interval,
        // 快速通道形同虚设 —— 只有紧跟在 probe 之后的那一轮才快。
        let next = if watching(&cfg, &engine, &shared) {
            cfg.fast_poll_secs
        } else {
            cfg.poll_interval_secs
        };
        wait_next(timer, shared.kick, next);
    }
}

/// 是否处于"快速盯梢"状态: 空闲 + 自动模式 + 启用了快速通道。
/// 此时用廉价 probe 高频探测; 一旦持有请求或用户手动切模式就回到常规轮询。
fn watching(cfg: &config::Config, engine: &Engine, shared: &Arc<Shared>) -> bool {
    cfg.fast_poll_secs > 0
        && cfg.fast_poll_secs < cfg.poll_interval_secs
        && engine.held() == Held::None
        && matches!(*shared.mode.lock().unwrap(), Mode::Auto)
}

/// 等到定时器到期或被 UI 线程踢醒。定时器用一次性模式, 每次调用重新武装。
fn wait_next(timer: HANDLE, kick: HANDLE, secs: u64) {
    let due = -(secs as i64) * 10_000_000;
    unsafe {
        let _ = SetWaitableTimer(timer, &due, 0, None, None, false);
        let r = WaitForMultipleObjects(&[timer, kick], false, u32::MAX);
        if r == WAIT_OBJECT_0 {
            let _ = ResetEvent(timer);
        }
    }
}

// ───────────────────────────── --status ─────────────────────────────

fn print_status() {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    let cfg = config::Config::load_or_create();
    let mut engine = Engine::new();

    // 两轮: 第一轮建立 CPU/网速/IO 基线, 隔 2s 第二轮才有差值
    let table = detect::ProcessTable::snapshot();
    engine.detect(&cfg, &table);
    std::thread::sleep(Duration::from_secs(2));
    let table = detect::ProcessTable::snapshot();
    let reasons = engine.detect(&cfg, &table);

    let ac = power::power_source() != PowerSource::Dc;
    // 独立进程读不到守护进程的显示器状态, 用最近输入时间近似
    let display_on = power::user_active_recently(90);

    let mut out = vec![
        format!("=== stayawake --status   {}", power::now_local()),
        format!(
            "power={}  display≈{}  processes={}",
            if ac { "AC" } else { "DC" },
            if display_on { "on" } else { "off" },
            table.count()
        ),
        format!(
            "poll={}s fast={}s grace={}s policy_ac={} policy_dc={} never_wake_display={} sleep_on_release={}",
            cfg.poll_interval_secs,
            if cfg.fast_poll_secs > 0 {
                cfg.fast_poll_secs.to_string()
            } else {
                "off".to_string()
            },
            cfg.grace_secs,
            cfg.policy_ac,
            cfg.policy_dc,
            cfg.never_wake_display,
            cfg.sleep_on_release
        ),
        String::new(),
    ];
    out.extend(engine.status_lines(&cfg));
    out.push(String::new());
    out.push("[结论]".to_string());
    if reasons.is_empty() {
        out.push("  本轮无活动命中".to_string());
    } else {
        for r in &reasons {
            out.push(format!("  {}: {}", r.kind, r.detail));
        }
    }
    out.push(format!(
        "  desired = {}",
        engine
            .step(&cfg, Mode::Auto, display_on)
            .0
            .held
            .label()
    ));

    write_console(&(out.join("\n") + "\n"));
    unsafe {
        // 释放刚才 step() 可能设下的 execution state
        let _ = power::apply_hold(Held::None);
        CoUninitialize();
    }
}

/// 优先走 stdout(管道/重定向), 失败再附加父控制台。
/// windows_subsystem="windows" 的程序默认没有控制台。
fn write_console(s: &str) {
    use std::io::Write;
    if std::io::stdout().write_all(s.as_bytes()).is_ok() {
        let _ = std::io::stdout().flush();
        return;
    }
    use windows::Win32::System::Console::{
        AllocConsole, AttachConsole, FreeConsole, GetStdHandle, WriteConsoleW, STD_OUTPUT_HANDLE,
    };
    unsafe {
        if AttachConsole(u32::MAX).is_err() && AllocConsole().is_err() {
            return;
        }
        if let Ok(h) = GetStdHandle(STD_OUTPUT_HANDLE) {
            let utf16: Vec<u16> = s.encode_utf16().collect();
            let bytes =
                std::slice::from_raw_parts(utf16.as_ptr() as *const u8, utf16.len() * 2);
            let mut written = 0u32;
            let _ = WriteConsoleW(h, bytes, Some(&mut written), None);
        }
        let _ = FreeConsole();
    }
}
