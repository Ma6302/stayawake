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
    held: Held,
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
            held: Held::None,
        }
    }

    pub fn held(&self) -> Held {
        self.held
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
            } else {
                // SetThreadExecutionState 失败: 保持旧状态, 下一 tick 重试
                crate::log::event("warn: SetThreadExecutionState failed");
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
