// 决策核心: 聚合各检测器 -> 迟滞 -> 供电策略 -> execution state
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::detect::{self, Detector, ProcessTable, Reason};
use crate::power::{self, Held, PowerSource};

/// 用户手动模式
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Mode {
    /// 按活动检测自动决策
    Auto,
    /// 暂停: 完全放手, 允许系统正常休眠
    Paused,
    /// 强制常亮至某时刻
    Force(Instant),
}

/// 一轮决策的完整结果, 供托盘/日志/--status 共用
#[derive(Clone)]
pub struct Snapshot {
    pub held: Held,
    pub mode: Mode,
    pub reasons: Vec<String>,
    pub in_grace: bool,
    pub ac: bool,
    pub display_on: bool,
    pub force_left: Option<Duration>,
}

impl Snapshot {
    /// 一行式说明"为什么不休眠", 给 tooltip / 菜单标题用
    pub fn describe(&self) -> String {
        match self.mode {
            Mode::Paused => "已暂停 · 允许正常休眠".to_string(),
            Mode::Force(_) => {
                let left = self.force_left.unwrap_or_default().as_secs();
                format!("强制常亮 · 剩余 {}分{}秒", left / 60, left % 60)
            }
            Mode::Auto => match self.held {
                Held::None => "空闲 · 未持有请求".to_string(),
                _ => {
                    let what = if self.reasons.is_empty() {
                        "宽限期".to_string()
                    } else {
                        self.reasons.join(" · ")
                    };
                    let how = if self.held == Held::SystemDisplay {
                        "保持屏幕"
                    } else {
                        "仅防睡"
                    };
                    format!("{} · {}", how, what)
                }
            },
        }
    }
}

pub struct Engine {
    detectors: Vec<Box<dyn Detector>>,
    /// 最近一次检测到活动的时刻, 用于宽限期
    last_active: Option<Instant>,
    /// 上一轮是否处于宽限期
    in_grace: bool,
    held: Held,
    /// apply_hold 失败告警闩锁, 防止日志刷屏
    hold_warned: bool,
}

impl Engine {
    pub fn new() -> Engine {
        Engine {
            detectors: vec![
                Box::new(detect::audio::AudioDetector::default()),
                Box::new(detect::net::NetDetector::default()),
                Box::new(detect::proc::ProcDetector::default()),
                Box::new(detect::dl::DlDetector::default()),
                Box::new(detect::hint::HintDetector::default()),
            ],
            last_active: None,
            in_grace: false,
            held: Held::None,
            hold_warned: false,
        }
    }

    pub fn held(&self) -> Held {
        self.held
    }

    /// 上一轮 step 是否处于宽限期。给 sleep_on_release 判断"是否为自然释放"用。
    pub fn in_grace(&self) -> bool {
        self.in_grace
    }

    /// 跑一轮检测, 返回聚合后的理由(同一 kind 合并为一条)
    pub fn detect(&mut self, cfg: &Config, table: &ProcessTable) -> Vec<Reason> {
        let mut all: Vec<Reason> = Vec::new();
        for d in self.detectors.iter_mut() {
            all.extend(d.tick(cfg, table));
        }
        // 同类合并: audio 三个会话在响 -> 一条 "audio: a, b, c"
        let mut merged: Vec<Reason> = Vec::new();
        for r in all {
            match merged.iter_mut().find(|m| m.kind == r.kind) {
                Some(m) => {
                    if m.detail.len() < 80 {
                        m.detail.push_str(", ");
                        m.detail.push_str(&r.detail);
                    }
                }
                None => merged.push(r),
            }
        }
        merged
    }

    /// 廉价快速探测: 只跑参与快速通道的检测器(不打进程快照)。
    /// 返回 true 表示"可能有活动", 应当立刻做一次完整 step。
    ///
    /// 允许假阳性(完整 step 会否掉), 不允许假阴性。
    pub fn probe(&mut self, cfg: &Config) -> bool {
        // 用 any 的短路特性: 一旦命中就不必再探后面的
        self.detectors.iter_mut().any(|d| d.probe(cfg))
    }

    /// 完整一轮: 检测 -> 迟滞 -> 策略 -> 应用。返回快照与"状态是否变化"
    pub fn step(&mut self, cfg: &Config, mode: Mode, display_on: bool) -> (Snapshot, bool) {
        let table = ProcessTable::snapshot();
        let reasons = self.detect(cfg, &table);
        let now = Instant::now();

        if !reasons.is_empty() {
            self.last_active = Some(now);
        }
        let in_grace = reasons.is_empty()
            && self
                .last_active
                .is_some_and(|t| now.duration_since(t) < Duration::from_secs(cfg.grace_secs));
        self.in_grace = in_grace;
        let busy = !reasons.is_empty() || in_grace;

        let ac = power::power_source() != PowerSource::Dc;
        let desired = match mode {
            Mode::Paused => Held::None,
            Mode::Force(until) if now < until => Held::SystemDisplay,
            _ => decide(cfg, ac, display_on, busy),
        };

        let changed = desired != self.held;
        if changed {
            if power::apply_hold(desired) {
                self.held = desired;
                self.hold_warned = false;
            } else {
                // SetThreadExecutionState 失败: 保持旧状态, 下一 tick 重试。
                // 只告警一次 —— 否则持续失败会以 fast_poll 频率刷日志,
                // 1MB 轮转只留一代, 全部历史会被冲掉。
                if !self.hold_warned {
                    self.hold_warned = true;
                    crate::log::event("warn: SetThreadExecutionState failed (后续不再重复告警)");
                }
            }
        }

        let snap = Snapshot {
            held: self.held,
            mode,
            reasons: reasons
                .iter()
                .map(|r| format!("{}:{}", r.kind, r.detail))
                .collect(),
            in_grace,
            ac,
            display_on,
            force_left: match mode {
                Mode::Force(until) => Some(until.saturating_duration_since(now)),
                _ => None,
            },
        };
        (snap, changed && self.held == desired)
    }

    pub fn status_lines(&self, cfg: &Config) -> Vec<String> {
        let names = ["audio", "net", "proc", "dl", "hint"];
        let mut lines = Vec::new();
        for (d, name) in self.detectors.iter().zip(names) {
            lines.push(format!("[{}]", name));
            lines.extend(d.status_lines(cfg));
        }
        lines
    }
}

/// 供电策略。这是全部 Modern Standby 知识的落点:
///
/// - 插电: system-required 可无限阻塞睡眠 -> 只防睡, 让屏幕正常熄掉(省电/不烧屏)
/// - 电池: system/execution-required 在睡眠超时后约 5 分钟会被系统强制清除,
///   只有 display-required 不受此限 -> 想可靠必须保屏幕
/// - never_wake_display: 屏幕已熄时不主动点亮, 降级为仅防睡(电池下接受 5 分钟上限)
fn decide(cfg: &Config, ac: bool, display_on: bool, busy: bool) -> Held {
    if !busy {
        return Held::None;
    }
    let policy = if ac { &cfg.policy_ac } else { &cfg.policy_dc };
    if policy != "display" {
        return Held::System;
    }
    if cfg.never_wake_display && !display_on {
        return Held::System;
    }
    Held::SystemDisplay
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_with(ac: &str, dc: &str, never_wake: bool) -> Config {
        let mut c = Config::from_text(crate::config::DEFAULT_CONFIG);
        c.policy_ac = ac.to_string();
        c.policy_dc = dc.to_string();
        c.never_wake_display = never_wake;
        c
    }

    #[test]
    fn idle_never_holds() {
        for ac in [true, false] {
            for disp in [true, false] {
                let c = cfg_with("display", "display", false);
                assert_eq!(decide(&c, ac, disp, false), Held::None);
            }
        }
    }

    /// 默认策略: 插电只防睡(让屏幕正常熄掉省电), 电池保屏幕
    /// (电池下 system-required 会在睡眠超时后约 5 分钟被系统强制清除)
    #[test]
    fn default_policy_truth_table() {
        let c = Config::from_text(crate::config::DEFAULT_CONFIG);
        assert_eq!(decide(&c, true, true, true), Held::System, "AC 屏幕亮");
        assert_eq!(decide(&c, true, false, true), Held::System, "AC 屏幕已熄");
        assert_eq!(decide(&c, false, true, true), Held::SystemDisplay, "DC 屏幕亮");
        // DC + 屏幕已熄 + never_wake_display -> 不主动点亮, 降级为仅防睡
        assert_eq!(decide(&c, false, false, true), Held::System, "DC 屏幕已熄");
    }

    #[test]
    fn never_wake_display_off_allows_waking_screen() {
        let c = cfg_with("system", "display", false);
        assert_eq!(
            decide(&c, false, false, true),
            Held::SystemDisplay,
            "关掉 never_wake_display 后应主动点亮"
        );
    }

    #[test]
    fn display_policy_on_ac_keeps_screen() {
        let c = cfg_with("display", "system", true);
        assert_eq!(decide(&c, true, true, true), Held::SystemDisplay);
        assert_eq!(decide(&c, false, true, true), Held::System, "DC 用 system 策略");
    }

    #[test]
    fn unknown_policy_falls_back_to_system() {
        let c = cfg_with("乱写", "乱写", true);
        assert_eq!(decide(&c, true, true, true), Held::System);
        assert_eq!(decide(&c, false, true, true), Held::System);
    }

    /// Held 的 flags 必须始终带 ES_CONTINUOUS, 否则只是"戳一下计时器"而非粘性持有
    #[test]
    fn held_flags_are_sticky() {
        const ES_CONTINUOUS: u32 = 0x8000_0000;
        const ES_SYSTEM: u32 = 0x0000_0001;
        const ES_DISPLAY: u32 = 0x0000_0002;
        assert_eq!(Held::None.flags(), ES_CONTINUOUS);
        assert_eq!(Held::System.flags(), ES_CONTINUOUS | ES_SYSTEM);
        assert_eq!(
            Held::SystemDisplay.flags(),
            ES_CONTINUOUS | ES_SYSTEM | ES_DISPLAY
        );
    }

    #[test]
    fn describe_reflects_mode_over_held() {
        let snap = Snapshot {
            held: Held::System,
            mode: Mode::Paused,
            reasons: vec!["audio:foo".into()],
            in_grace: false,
            ac: true,
            display_on: true,
            force_left: None,
        };
        assert!(snap.describe().contains("已暂停"), "暂停时不该显示持有原因");

        let snap2 = Snapshot { mode: Mode::Auto, ..snap.clone() };
        assert!(snap2.describe().contains("audio:foo"));

        let idle = Snapshot {
            held: Held::None,
            reasons: vec![],
            ..snap2.clone()
        };
        assert!(idle.describe().contains("空闲"));

        let grace = Snapshot {
            held: Held::System,
            reasons: vec![],
            in_grace: true,
            ..snap2
        };
        assert!(grace.describe().contains("宽限期"));
    }
}
