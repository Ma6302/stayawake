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
            let peak = unsafe {
                device
                    .Activate::<IAudioMeterInformation>(CLSCTX_ALL, None)
                    .ok()
                    .and_then(|m| m.GetPeakValue().ok())
                    .unwrap_or(0.0)
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
