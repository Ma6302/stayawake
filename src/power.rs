// 电源状态: execution state 持有 / AC-DC 检测 / 主动睡眠 / 本地时间
use windows::Win32::System::Power::{
    GetSystemPowerStatus, SetSuspendState, SetThreadExecutionState, SYSTEM_POWER_STATUS,
    EXECUTION_STATE, ES_CONTINUOUS, ES_DISPLAY_REQUIRED, ES_SYSTEM_REQUIRED,
};
use windows::Win32::System::SystemInformation::GetLocalTime;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Held {
    None,
    System,        // 仅阻止睡眠(允许熄屏)
    SystemDisplay, // 阻止睡眠 + 保持屏幕
}

impl Held {
    pub fn flags(self) -> u32 {
        let mut f = ES_CONTINUOUS.0;
        match self {
            Held::None => {}
            Held::System => f |= ES_SYSTEM_REQUIRED.0,
            Held::SystemDisplay => f |= ES_SYSTEM_REQUIRED.0 | ES_DISPLAY_REQUIRED.0,
        }
        f
    }
    pub fn label(self) -> &'static str {
        match self {
            Held::None => "none",
            Held::System => "system",
            Held::SystemDisplay => "system+display",
        }
    }
}

/// 必须由常驻 worker 线程调用: execution state 是线程级的, 持有者线程退出即释放。
/// 返回 false 表示 API 调用失败(此时系统并未接受我们的请求)。
pub fn apply_hold(hold: Held) -> bool {
    // 返回值是"之前的状态"; NULL(0) 表示失败
    let prev = unsafe { SetThreadExecutionState(EXECUTION_STATE(hold.flags())) };
    prev.0 != 0
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PowerSource {
    Ac,
    Dc,
    Unknown,
}

pub fn power_source() -> PowerSource {
    let mut s: SYSTEM_POWER_STATUS = unsafe { std::mem::zeroed() };
    if unsafe { GetSystemPowerStatus(&mut s) }.is_ok() {
        match s.ACLineStatus {
            1 => PowerSource::Ac,
            0 => PowerSource::Dc,
            _ => PowerSource::Unknown,
        }
    } else {
        PowerSource::Unknown
    }
}

pub fn now_local() -> String {
    let st = unsafe { GetLocalTime() };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        st.wYear, st.wMonth, st.wDay, st.wHour, st.wMinute, st.wSecond
    )
}

/// sleep_on_release 用: 宽限期到点后主动让机器睡 (默认关闭)
pub fn suspend_now() -> bool {
    unsafe { SetSuspendState(false, false, false).0 != 0 }
}

/// 用户最近是否有输入。仅用于 --status 的独立进程(读不到守护进程的显示器状态)。
/// 守护进程本身用 GUID_CONSOLE_DISPLAY_STATE 通知拿真实值。
pub fn user_active_recently(window_secs: u64) -> bool {
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
    unsafe {
        let mut lii: LASTINPUTINFO = std::mem::zeroed();
        lii.cbSize = std::mem::size_of::<LASTINPUTINFO>() as u32;
        if GetLastInputInfo(&mut lii).as_bool() {
            // dwTime 是 32 位 tick 计数, 必须在 32 位域内相减:
            // 开机超过 49.7 天后 dwTime 已回绕而 GetTickCount64 没有, 直接混用会永远算成"很久没输入"
            let ticks = windows::Win32::System::SystemInformation::GetTickCount64() as u32;
            let idle_ms = ticks.wrapping_sub(lii.dwTime) as u64;
            return idle_ms < window_secs * 1000;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn held_labels_are_distinct() {
        let labels = [
            Held::None.label(),
            Held::System.label(),
            Held::SystemDisplay.label(),
        ];
        let mut sorted = labels;
        sorted.sort_unstable();
        let mut dedup = sorted.to_vec();
        dedup.dedup();
        assert_eq!(dedup.len(), 3);
    }

    /// 真实调用: 拿到当前供电状态, 不该 panic 也不该返回 Unknown
    /// (笔记本/台式机都应能识别)
    #[test]
    fn power_source_is_readable() {
        assert_ne!(power_source(), PowerSource::Unknown);
    }

    /// apply_hold 的返回值语义: 首次调用也应成功。
    /// 若这里失败, 说明 "prev.0 != 0 表示成功" 的假设不成立,
    /// 那么托盘状态和日志从第一秒起就是错的。
    #[test]
    fn apply_hold_succeeds_and_can_be_released() {
        assert!(apply_hold(Held::System), "首次 apply_hold 应成功");
        assert!(apply_hold(Held::SystemDisplay));
        assert!(apply_hold(Held::None), "释放也应成功");
    }

    #[test]
    fn now_local_has_expected_shape() {
        let s = now_local();
        assert_eq!(s.len(), 19, "应为 YYYY-MM-DD HH:MM:SS, 得到 {}", s);
        assert_eq!(&s[4..5], "-");
        assert_eq!(&s[10..11], " ");
        assert_eq!(&s[13..14], ":");
    }

    /// 测试进程刚被启动, 必然"最近有输入"(测试运行器本身算)。
    /// 主要目的是确认不会 panic、也不会因 32/64 位 tick 混用而永远返回 false。
    #[test]
    fn user_active_recently_does_not_panic() {
        let _ = user_active_recently(1);
        // 给一个极大窗口, 必然为 true
        assert!(user_active_recently(u32::MAX as u64 / 1000));
    }
}