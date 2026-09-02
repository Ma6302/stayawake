// 极简 key = value 配置解析。
// 保留原始行以便回写(托盘开关)时不丢注释; 值支持行尾 # / ; 注释。
use std::path::PathBuf;

pub fn app_dir() -> PathBuf {
    let base = std::env::var("LOCALAPPDATA").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(base).join("stayawake")
}

pub fn config_path() -> PathBuf {
    app_dir().join("config.ini")
}

pub fn log_path() -> PathBuf {
    app_dir().join("stayawake.log")
}

pub fn hints_dir() -> PathBuf {
    app_dir().join("hints")
}

pub const DEFAULT_CONFIG: &str = r#"# stayawake 配置
# 改完用托盘菜单「重新加载配置」生效, 不必重启

# 轮询间隔(秒): 完整检测的周期
poll_interval_secs = 15
# 快速探测间隔(秒): 空闲时用廉价手段(设备音频峰值/网速/提示文件)探一次,
# 命中就立刻做完整检测。这让"开始播放音乐"到"状态更新"的延迟从 15s 降到 2s,
# 而额外成本只有约 4ms/次。设为 0 关闭快速通道。
fast_poll_secs = 2
# 活动停止后继续保持的宽限期(秒), 防止音乐换歌/编译间隙来回抖动
grace_secs = 90

# 供电策略: system = 只阻止睡眠(允许熄屏) / display = 同时保持屏幕常亮
# 插电时 system-required 可无限阻塞睡眠, 所以让屏幕正常熄掉最省电
policy_ac = system
# 电池下 system-required 在睡眠超时后约 5 分钟会被系统强制清除,
# 只有 display-required 不受此限 -> 电池默认保屏幕
policy_dc = display

# true = 屏幕已经熄了就不主动点亮(降级为仅防睡)。半夜下载启动不会闪亮屏
never_wake_display = true

# true = 宽限期结束后主动调 SetSuspendState 让机器立刻睡
# 默认 false: 交给 Windows 按正常流程决定, 不替用户按睡眠键
sleep_on_release = false


# ── 音频播放 (WASAPI 逐会话检测) ──
audio_enabled = true
# 会话峰值阈值。数字静音(全 0 采样)不算在放声
audio_peak_threshold = 0.0001
# 峰值保持窗口(秒)。GetPeakValue 是瞬时值, 乐曲安静段落/歌曲间隙会读到 0,
# 直接判定会让状态来回抖动。在这个窗口内响过就仍算它在播放。
audio_hold_secs = 20
# 忽略这些进程的音频。壁纸软件/常驻音效会一直输出, 不该阻止休眠
audio_ignore = wallpaper64.exe, wallpaper32.exe, wallpaperservice32.exe, wallpaperservice64.exe


# ── 全局网速 (GetIfTable2 差分) ──
net_enabled = true
# 阈值(KB/s)。1024 = 1 MB/s: 只有实打实的大流量传输才算"忙",
# 浏览网页、遥测、后台同步这类零碎流量不会阻止休眠
net_threshold_kbps = 1024
# 连续几个 tick 超阈值才算忙, 滤掉瞬时突发
net_min_consecutive_tick = 2


# ── CPU 密集型进程 (高 CPU = 真在干活) ──
proc_enabled = true
# 单核百分比: 单线程打满一核 = 100%, 与核心数无关
proc_cpu_percent_1core = 5.0
proc_busy_when_cpu = cargo.exe, rustc.exe, link.exe, cl.exe, msbuild.exe, node.exe, python.exe, ffmpeg.exe, 7z.exe, WinRAR.exe, git.exe


# ── 下载器 (I/O 密集但 CPU≈0, 需专用规则) ──
# 注意: I/O 阈值只对此名单生效。Electron 类应用的进程间共享内存流量会到 GB/s 级, 全局启用必假阳性
dl_enabled = true
dl_processes = IDMan.exe, idmBroker.exe, aria2c.exe, qbittorrent.exe
# 进程 Read+Write 传输速率阈值(KB/s)
dl_io_kbps = 50
# established TCP 连接数阈值。IDM 默认每文件开 8 条; 空闲时 0-2 条
# 专门抓"服务器卡住、速度接近 0 但下载仍在进行"
dl_tcp_conns = 4


# ── 外部提示文件 ──
# 任何程序在 %LOCALAPPDATA%\stayawake\hints\ 下 touch 一个 .hint 文件即可保持唤醒,
# 删除即释放; 文件 mtime 超过 hint_ttl_secs 自动失效(写方崩溃不会把机器永久卡醒)
hint_enabled = true
hint_ttl_secs = 60
"#;

#[derive(Clone, Default)]
pub struct Config {
    pub poll_interval_secs: u64,
    pub fast_poll_secs: u64,
    pub grace_secs: u64,
    pub policy_ac: String,
    pub policy_dc: String,
    pub never_wake_display: bool,
    pub sleep_on_release: bool,

    pub audio_enabled: bool,
    pub audio_peak_threshold: f64,
    pub audio_hold_secs: u64,
    pub audio_ignore: Vec<String>,

    pub net_enabled: bool,
    pub net_threshold_kbps: u64,
    pub net_min_consecutive_tick: u32,

    pub proc_enabled: bool,
    pub proc_cpu_percent_1core: f64,
    pub proc_busy_when_cpu: Vec<String>,

    pub dl_enabled: bool,
    pub dl_processes: Vec<String>,
    pub dl_io_kbps: u64,
    pub dl_tcp_conns: u32,

    pub hint_enabled: bool,
    pub hint_ttl_secs: u64,

    /// 原始行, 回写时保留注释与未知键
    raw: Vec<String>,
}

fn strip_comment(s: &str) -> &str {
    let end = s
        .find('#')
        .into_iter()
        .chain(s.find(';'))
        .min()
        .unwrap_or(s.len());
    s[..end].trim()
}

fn parse_list(s: &str) -> Vec<String> {
    s.split(',')
        .map(|x| x.trim().to_string())
        .filter(|x| !x.is_empty())
        .collect()
}

/// 旧版配置补齐新键: 只追加缺失项, 不动用户已有内容。
/// 追加后立即落盘, 用户下次打开配置文件就能看到新选项和注释。
fn migrate(text: &str) -> String {
    let existing: Vec<String> = text
        .lines()
        .filter_map(|l| {
            let t = l.trim();
            if t.is_empty() || t.starts_with('#') || t.starts_with(';') {
                return None;
            }
            t.find('=').map(|eq| t[..eq].trim().to_string())
        })
        .collect();

    let mut added: Vec<String> = Vec::new();
    for line in DEFAULT_CONFIG.lines() {
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        let Some(eq) = t.find('=') else { continue };
        let key = t[..eq].trim();
        if !existing.iter().any(|k| k == key) {
            added.push(line.to_string());
        }
    }
    if added.is_empty() {
        return text.to_string();
    }

    let mut merged = text.trim_end().to_string();
    merged.push_str("\n\n# ── 以下为新版本追加的配置项 ──\n");
    merged.push_str(&added.join("\n"));
    merged.push('\n');
    let _ = std::fs::write(config_path(), &merged);
    merged
}

impl Config {
    pub fn load_or_create() -> Config {
        let path = config_path();
        let _ = std::fs::create_dir_all(app_dir());
        let text = match std::fs::read_to_string(&path) {
            Ok(t) if !t.trim().is_empty() => migrate(&t),
            _ => {
                let _ = std::fs::write(&path, DEFAULT_CONFIG);
                DEFAULT_CONFIG.to_string()
            }
        };
        Config::from_text(&text)
    }

    pub fn from_text(text: &str) -> Config {
        let mut raw: Vec<String> = Vec::new();
        let mut map: Vec<(String, String)> = Vec::new();
        for line in text.lines() {
            raw.push(line.to_string());
            let t = line.trim();
            if t.is_empty() || t.starts_with('#') || t.starts_with(';') {
                continue;
            }
            if let Some(eq) = t.find('=') {
                let key = t[..eq].trim().to_string();
                let val = strip_comment(&t[eq + 1..]).to_string();
                match map.iter_mut().find(|(k, _)| *k == key) {
                    Some(slot) => slot.1 = val,
                    None => map.push((key, val)),
                }
            }
        }

        let get = |k: &str| -> &str {
            map.iter()
                .find(|(key, _)| key == k)
                .map(|(_, v)| v.as_str())
                .unwrap_or("")
        };
        let b = |k: &str, d: bool| match get(k).to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" | "on" => true,
            "false" | "0" | "no" | "off" => false,
            _ => d,
        };
        let u = |k: &str, d: u64| get(k).parse::<u64>().unwrap_or(d);
        let f = |k: &str, d: f64| get(k).parse::<f64>().unwrap_or(d);
        let policy = |k: &str, d: &str| -> String {
            let v = get(k).to_ascii_lowercase();
            if v == "system" || v == "display" {
                v
            } else {
                d.to_string()
            }
        };

        Config {
            poll_interval_secs: u("poll_interval_secs", 15).clamp(1, 3600),
            // 0 = 关闭快速通道; 否则至少 1s, 且不超过完整轮询间隔
            fast_poll_secs: {
                let full = u("poll_interval_secs", 15).clamp(1, 3600);
                let fast = u("fast_poll_secs", 2);
                if fast == 0 {
                    0
                } else {
                    fast.clamp(1, full)
                }
            },
            grace_secs: u("grace_secs", 90).min(3600),
            policy_ac: policy("policy_ac", "system"),
            policy_dc: policy("policy_dc", "display"),
            never_wake_display: b("never_wake_display", true),
            sleep_on_release: b("sleep_on_release", false),

            audio_enabled: b("audio_enabled", true),
            audio_peak_threshold: f("audio_peak_threshold", 0.0001),
            audio_hold_secs: u("audio_hold_secs", 20).clamp(1, 600),
            audio_ignore: parse_list(get("audio_ignore")),

            net_enabled: b("net_enabled", true),
            net_threshold_kbps: u("net_threshold_kbps", 1024),
            net_min_consecutive_tick: u("net_min_consecutive_tick", 2).clamp(1, 100) as u32,

            proc_enabled: b("proc_enabled", true),
            proc_cpu_percent_1core: f("proc_cpu_percent_1core", 5.0),
            proc_busy_when_cpu: parse_list(get("proc_busy_when_cpu")),

            dl_enabled: b("dl_enabled", true),
            dl_processes: parse_list(get("dl_processes")),
            dl_io_kbps: u("dl_io_kbps", 50),
            dl_tcp_conns: u("dl_tcp_conns", 4).clamp(1, 10000) as u32,

            hint_enabled: b("hint_enabled", true),
            hint_ttl_secs: u("hint_ttl_secs", 60).max(10),

            raw,
        }
    }

    /// 托盘开关回写: 就地替换该键的值, 其他行(含注释)原样保留
    pub fn set_and_save(&mut self, key: &str, value: &str) {
        let mut found = false;
        for line in self.raw.iter_mut() {
            let t = line.trim();
            if t.is_empty() || t.starts_with('#') || t.starts_with(';') {
                continue;
            }
            let Some(eq) = t.find('=') else { continue };
            if t[..eq].trim() != key {
                continue;
            }
            // 保留行尾注释
            let tail = &t[eq + 1..];
            let comment = tail
                .find('#')
                .into_iter()
                .chain(tail.find(';'))
                .min()
                .map(|i| format!("   {}", tail[i..].trim_end()))
                .unwrap_or_default();
            *line = format!("{} = {}{}", key, value, comment);
            found = true;
            break;
        }
        if !found {
            self.raw.push(format!("{} = {}", key, value));
        }
        let text = self.raw.join("\n") + "\n";
        let _ = std::fs::write(config_path(), &text);
        *self = Config::from_text(&text);
    }
}
