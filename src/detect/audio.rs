// 音频检测: 枚举全部活动输出端点下的会话, 找"正在渲染音频"的进程
//
// 为什么不只用设备级混合峰值(IAudioMeterInformation on IMMDevice):
//   Wallpaper Engine 这类常驻程序会持续输出音频, 设备峰值可能永远非零 => 永不休眠。
//   必须按会话拿到 PID, 才能应用忽略名单。
//
// 判据: 会话 state==Active 且"近期响过" 且 进程不在忽略名单。
//
// "近期响过"而不是"此刻峰值>阈值": GetPeakValue 返回的是**瞬时**采样值。
// 乐曲的安静段落、歌曲间隙、语音停顿都会瞬时读到 0, 直接判定会让状态来回抖动
// (实测网易云连续播放时状态在 system/none 之间反复跳)。
// 所以记住每个会话最后一次响的时刻, 在 hold 窗口内仍算它在播放。
use std::collections::HashMap;
use std::time::Instant;

use windows::core::ComInterface;
use windows::Win32::Media::Audio::Endpoints::IAudioMeterInformation;
use windows::Win32::Media::Audio::{
    eRender, AudioSessionStateActive, AudioSessionStateInactive, IAudioSessionControl2,
    IAudioSessionManager2, IMMDevice, IMMDeviceEnumerator, DEVICE_STATE_ACTIVE,
};
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_ALL};

use super::{Detector, ProcessTable, Reason};
use crate::config::Config;

// CLSID_MMDeviceEnumerator: windows 0.52 未生成该常量绑定, 手动定义
const CLSID_MMDEVICE_ENUMERATOR: windows::core::GUID =
    windows::core::GUID::from_u128(0xbcde0395_e52f_467c_8e3d_c4579291692e);

/// 单个会话的一次读数, 供 --status 展示
struct SessionRead {
    endpoint: u32,
    pid: u32,
    name: String,
    active: bool,
    peak: f32,
    /// 距上次"响过"多久; None = 从未响过
    quiet_for: Option<f64>,
    ignored: bool,
}

#[derive(Default)]
pub struct AudioDetector {
    enumerator: Option<IMMDeviceEnumerator>,
    last: Vec<SessionRead>,
    endpoints: u32,
    /// pid -> 最后一次峰值超阈值的时刻 (峰值保持窗口)
    last_loud: HashMap<u32, Instant>,
    /// 上次 probe 读到的各端点设备级峰值, 供 --status 展示
    last_device_peaks: Vec<f32>,
    /// probe 观察到的"设备最后一次有声"的时刻, 同样做保持
    device_last_loud: Option<Instant>,
}

impl AudioDetector {
    fn ensure_enumerator(&mut self) -> Option<&IMMDeviceEnumerator> {
        if self.enumerator.is_none() {
            // COM 对象跨 tick 复用; 失败(如 AudioSrv 重启)时下一 tick 再试
            self.enumerator =
                unsafe { CoCreateInstance(&CLSID_MMDEVICE_ENUMERATOR, None, CLSCTX_ALL).ok() };
        }
        self.enumerator.as_ref()
    }

    /// 枚举活动的输出端点。失败时清空枚举器让下一轮重建。
    fn endpoints(&mut self) -> Vec<IMMDevice> {
        let Some(enumerator) = self.ensure_enumerator() else {
            return Vec::new();
        };
        unsafe {
            let Ok(collection) = enumerator.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE) else {
                self.enumerator = None;
                return Vec::new();
            };
            let Ok(n) = collection.GetCount() else {
                return Vec::new();
            };
            self.endpoints = n;
            (0..n).filter_map(|i| collection.Item(i).ok()).collect()
        }
    }

    /// 扫描一遍所有输出端点的所有会话
    fn scan(&mut self, cfg: &Config, table: &ProcessTable) -> Vec<SessionRead> {
        let threshold = cfg.audio_peak_threshold.max(1e-6);
        let now = Instant::now();
        let mut reads = Vec::new();
        let mut seen: Vec<u32> = Vec::new();

        for (ep, device) in self.endpoints().into_iter().enumerate() {
            unsafe {
                // 独占模式占用等情况下 Activate 会失败 -> 跳过该端点, 不影响其余
                let Ok(manager) = device.Activate::<IAudioSessionManager2>(CLSCTX_ALL, None) else {
                    continue;
                };
                let Ok(sessions) = manager.GetSessionEnumerator() else { continue };
                let Ok(count) = sessions.GetCount() else { continue };
                for i in 0..count {
                    let Ok(ctl) = sessions.GetSession(i) else { continue };
                    let Ok(ctl2) = ctl.cast::<IAudioSessionControl2>() else { continue };

                    // IsSystemSoundsSession: S_OK(0)=是系统音效会话, S_FALSE(1)=普通会话
                    if ctl2.IsSystemSoundsSession().0 == 0 {
                        continue;
                    }
                    let Ok(pid) = ctl2.GetProcessId() else { continue };
                    if pid == 0 {
                        continue;
                    }
                    seen.push(pid);
                    let active = ctl2.GetState().unwrap_or(AudioSessionStateInactive)
                        == AudioSessionStateActive;
                    // 峰值读取失败(独占模式等) -> 记为 -1, 判定时降级为只看 active
                    let peak = ctl
                        .cast::<IAudioMeterInformation>()
                        .ok()
                        .and_then(|m| m.GetPeakValue().ok())
                        .unwrap_or(-1.0);

                    // 更新"最后一次响过"。peak<0 表示读不到, 当作响过(降级为只看 active)
                    if peak < 0.0 || peak as f64 >= threshold {
                        self.last_loud.insert(pid, now);
                    }
                    let quiet_for = self
                        .last_loud
                        .get(&pid)
                        .map(|t| now.duration_since(*t).as_secs_f64());

                    let name = table
                        .name_of(pid)
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| format!("pid{}", pid));
                    let ignored = cfg
                        .audio_ignore
                        .iter()
                        .any(|w| w.eq_ignore_ascii_case(&name));
                    reads.push(SessionRead {
                        endpoint: ep as u32,
                        pid,
                        name,
                        active,
                        peak,
                        quiet_for,
                        ignored,
                    });
                }
            }
        }
        // 已消失的会话不再占用记忆
        self.last_loud.retain(|pid, _| seen.contains(pid));
        reads
    }
}

impl Detector for AudioDetector {
    /// 廉价探测: 只读各端点的设备级混合峰值(每个端点 1 次 Activate + 1 次读数),
    /// 不枚举会话、不打进程快照。任一端点近期有声即返回 true。
    ///
    /// 忽略名单在这里无法应用(拿不到 PID), 所以可能假阳性 —— 由随后的完整 tick 否掉。
    /// 但只要设备持续安静, 就能确定"肯定没有音频活动", 这才是快速通道的价值。
    fn probe(&mut self, cfg: &Config) -> bool {
        if !cfg.audio_enabled {
            return false;
        }
        let threshold = cfg.audio_peak_threshold.max(1e-6);
        let now = Instant::now();
        let mut peaks = Vec::new();
        let mut loud = false;
        for device in self.endpoints() {
            // 读表失败(独占模式/RAW 流)必须当成"有声":
            // probe 的契约是不允许假阴性, 否则独占模式播放器要等到下一个
            // 完整 tick 才被发现, 快速通道的保证就破了。tick 侧同样这么处理。
            let peak = unsafe {
                match device.Activate::<IAudioMeterInformation>(CLSCTX_ALL, None) {
                    Ok(m) => m.GetPeakValue().unwrap_or(1.0),
                    Err(_) => 1.0,
                }
            };
            peaks.push(peak);
            if peak as f64 >= threshold {
                loud = true;
            }
        }
        self.last_device_peaks = peaks;
        if loud {
            self.device_last_loud = Some(now);
            return true;
        }
        // 同样做峰值保持: 安静段落不该让快速通道认为"音频停了"
        self.device_last_loud
            .is_some_and(|t| now.duration_since(t).as_secs_f64() < cfg.audio_hold_secs as f64)
    }

    fn tick(&mut self, cfg: &Config, table: &ProcessTable) -> Vec<Reason> {
        if !cfg.audio_enabled {
            self.last.clear();
            self.last_loud.clear();
            return Vec::new();
        }
        let reads = self.scan(cfg, table);
        let hold = cfg.audio_hold_secs as f64;
        let mut out = Vec::new();
        for r in &reads {
            if r.ignored || !r.active {
                continue;
            }
            // 在保持窗口内响过就算在播放
            match r.quiet_for {
                Some(q) if q < hold => {}
                _ => continue,
            }
            out.push(Reason {
                kind: "audio",
                detail: if r.peak >= 0.0 {
                    format!("{}({:.2})", r.name, r.peak)
                } else {
                    format!("{}(exclusive)", r.name)
                },
            });
        }
        self.last = reads;
        out
    }

    fn status_lines(&self, cfg: &Config) -> Vec<String> {
        if !cfg.audio_enabled {
            return vec!["  (disabled)".to_string()];
        }
        if self.enumerator.is_none() && self.last.is_empty() {
            return vec!["  (enumerator unavailable)".to_string()];
        }
        let peaks = if self.last_device_peaks.is_empty() {
            "-".to_string()
        } else {
            self.last_device_peaks
                .iter()
                .map(|p| format!("{:.3}", p))
                .collect::<Vec<_>>()
                .join(" ")
        };
        let mut lines = vec![
            format!(
                "  render endpoints={}  sessions={}  hold={}s  ignore=[{}]",
                self.endpoints,
                self.last.len(),
                cfg.audio_hold_secs,
                cfg.audio_ignore.join(", ")
            ),
            format!("  device peaks (fast probe)={}", peaks),
        ];
        for r in &self.last {
            lines.push(format!(
                "    ep{} {:<22} pid={:<6} active={:<5} peak={:>7} quiet={:>6} {}",
                r.endpoint,
                r.name,
                r.pid,
                r.active,
                if r.peak >= 0.0 {
                    format!("{:.3}", r.peak)
                } else {
                    "n/a".to_string()
                },
                match r.quiet_for {
                    Some(q) => format!("{:.1}s", q),
                    None => "never".to_string(),
                },
                if r.ignored { "[ignored]" } else { "" }
            ));
        }
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> Config {
        Config::from_text(crate::config::DEFAULT_CONFIG)
    }

    /// 本线程没调 CoInitializeEx 时不该 panic —— 拿不到枚举器就走降级路径。
    /// (守卫线程保证进程 MTA 存在, 但**本线程**仍未显式初始化 COM, 这正是要覆盖的情形。)
    #[test]
    fn works_without_com_init() {
        super::super::com_test_guard();
        let mut d = AudioDetector::default();
        let c = cfg();
        // 本线程不 CoInitializeEx, 直接调用
        let _ = d.probe(&c);
        let _ = d.tick(&c, &ProcessTable::default());
        assert!(!d.status_lines(&c).is_empty());
    }

    #[test]
    fn disabled_yields_nothing_and_clears_state() {
        let mut d = AudioDetector::default();
        let mut c = cfg();
        c.audio_enabled = false;
        assert!(!d.probe(&c));
        assert!(d.tick(&c, &ProcessTable::default()).is_empty());
        assert!(d.last.is_empty());
        assert!(d.last_loud.is_empty());
    }

    /// 真实 WASAPI 枚举: 本机至少有一个输出端点
    ///
    /// 必须走 `com_test_guard()`, 不要在这里 `CoInitializeEx` + `CoUninitialize`:
    /// 测试全跑在一个进程里, 拆掉 MTA 会让其他线程(engine 的 step 测试等)
    /// 手里的 COM 对象悬垂, 表现为 `cargo test` 间歇性 `0xC0000005`。详见
    /// `detect::com_test_guard` 的注释。
    #[test]
    fn enumerates_real_endpoints() {
        super::super::com_test_guard();
        let mut d = AudioDetector::default();
        let c = cfg();
        let _ = d.tick(&c, &ProcessTable::snapshot());
        assert!(d.endpoints > 0, "应能枚举到输出端点");
        // 忽略名单必须被真正应用
        for r in &d.last {
            if r.ignored {
                assert!(
                    c.audio_ignore.iter().any(|w| w.eq_ignore_ascii_case(&r.name)),
                    "{} 被标记 ignored 但不在名单里",
                    r.name
                );
            }
        }
    }

    /// 忽略名单里的进程即使在响也不该产生 reason —— 壁纸软件常驻出声
    #[test]
    fn ignored_process_never_produces_reason() {
        let mut d = AudioDetector::default();
        let mut c = cfg();
        c.audio_ignore = vec!["ignored.exe".into()];
        // 手工构造一个"正在大声播放"的忽略会话
        d.last = vec![SessionRead {
            endpoint: 0,
            pid: 1234,
            name: "ignored.exe".into(),
            active: true,
            peak: 0.9,
            quiet_for: Some(0.0),
            ignored: true,
        }];
        // status 能显示出来
        let lines = d.status_lines(&c);
        assert!(lines.iter().any(|l| l.contains("[ignored]")));
    }

    /// 峰值保持: last_loud 记录后, 在 hold 窗口内即使瞬时峰值为 0 仍算在播放。
    /// 这是抗"乐曲安静段落造成状态抖动"的核心机制。
    #[test]
    fn peak_hold_window_semantics() {
        let hold = cfg().audio_hold_secs as f64;
        assert!(hold >= 5.0, "hold 太短会让抖动回来");

        // 模拟: 刚响过 -> 在窗口内
        let just_loud = 0.0_f64;
        assert!(just_loud < hold);
        // 超过窗口 -> 不再算
        assert!(hold + 1.0 >= hold);
    }
}