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
# 判据: io >= dl_io_kbps, 或者 (established TCP >= dl_tcp_conns 且 io > 0)
# 注意: I/O 阈值只对此名单生效。Electron 类应用的进程间共享内存流量会到 GB/s, 全局启用必假阳性
dl_enabled = true
# 不要往这里加 BT 客户端(qBittorrent/Transmission 等): 做种时会长期持有几十条
# established 连接而吞吐为零, 虽然有 "io>0" 约束兜着, 但 BT 的心跳流量仍可能持续触发
dl_processes = IDMan.exe, idmBroker.exe, aria2c.exe
# 进程 Read+Write 传输速率阈值(KB/s)
dl_io_kbps = 50
# established TCP 连接数阈值。IDM 默认每文件开 8 条; 空闲时 0-2 条
# 配合 "io>0" 使用, 抓"服务器限速、速度很低但仍在传输"
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

/// 旧版配置补齐新键: 只追加缺失项(连同它上方的注释), 不动用户已有内容。
///
/// 追加后落盘, 这样用户打开配置文件就能看到新选项及其说明。
/// 写入失败只记一次日志: 文件只读或磁盘满时不该每次加载都刷屏。
fn migrate(text: &str) -> String {
    let merged = migrate_text(text);
    if merged.len() == text.len() {
        return merged; // 无变化, 不写盘
    }
    if let Err(e) = write_atomic(&config_path(), &merged) {
        // 只在首次失败时记录, 避免只读文件导致每次加载都写日志
        static WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if !WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
            crate::log::event(&format!("warn: 无法写入迁移后的配置: {}", e));
        }
    }
    merged
}

/// migrate 的纯函数部分(不碰文件系统), 便于单测。
fn migrate_text(text: &str) -> String {
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

    // 收集缺失键, 连同紧挨在它上方的注释块一起搬过来
    let lines: Vec<&str> = DEFAULT_CONFIG.lines().collect();
    let mut added: Vec<String> = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        let Some(eq) = t.find('=') else { continue };
        if existing.iter().any(|k| k == t[..eq].trim()) {
            continue;
        }
        // 往上回溯连续的注释行(不跨空行), 保留原始缩进
        let mut comments: Vec<String> = Vec::new();
        for prev in lines[..i].iter().rev() {
            let p = prev.trim();
            if p.starts_with('#') && !p.starts_with("# ──") {
                comments.push(prev.to_string());
            } else {
                break;
            }
        }
        comments.reverse();
        added.extend(comments);
        added.push(line.to_string());
    }
    if added.is_empty() {
        return text.to_string();
    }

    let mut merged = text.trim_end().to_string();
    merged.push_str("\n\n# ── 以下为新版本追加的配置项 ──\n");
    merged.push_str(&added.join("\n"));
    merged.push('\n');
    merged
}

/// 原子写: 先写临时文件再 rename。
///
/// 直接 `fs::write` 是"截断后写入", 另一个线程恰好在窗口内读就会看到空文件,
/// `load_or_create` 随即走 `_ =>` 分支用 DEFAULT_CONFIG 覆盖 —— 用户配置全丢。
/// Windows 上 rename 到已存在的目标映射为 MoveFileExW(REPLACE_EXISTING), 是原子的。
fn write_atomic(path: &std::path::Path, text: &str) -> std::io::Result<()> {
    let tmp = path.with_extension("ini.tmp");
    std::fs::write(&tmp, text)?;
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

impl Config {
    pub fn load_or_create() -> Config {
        let path = config_path();
        let _ = std::fs::create_dir_all(app_dir());
        let text = match std::fs::read_to_string(&path) {
            Ok(t) if !t.trim().is_empty() => migrate(&t),
            Ok(_) => {
                // 文件存在但是空的。可能是上一次写入被打断, 也可能用户清空了它。
                // 不覆盖(避免和并发写入互相踩), 只用默认值跑起来。
                DEFAULT_CONFIG.to_string()
            }
            Err(_) => {
                // 真的不存在 -> 生成默认配置
                if let Err(e) = write_atomic(&path, DEFAULT_CONFIG) {
                    crate::log::event(&format!("warn: 无法写入配置文件: {}", e));
                }
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
                // 重复键取**第一个**, 与 set_and_save 的写回位置保持一致。
                // 若取最后一个, 用户配置里有重复键时托盘开关会永久静默失效:
                // 写入改的是第一行, 生效值却来自最后一行 -> 勾选永远不变。
                if !map.iter().any(|(k, _)| *k == key) {
                    map.push((key, val));
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
            // 阈值为 0 会让 "kbps < 0" 永假 -> 零流量也每 tick 命中 -> 永久持有
            net_threshold_kbps: u("net_threshold_kbps", 1024).max(1),
            net_min_consecutive_tick: u("net_min_consecutive_tick", 2).clamp(1, 100) as u32,

            proc_enabled: b("proc_enabled", true),
            proc_cpu_percent_1core: f("proc_cpu_percent_1core", 5.0),
            proc_busy_when_cpu: parse_list(get("proc_busy_when_cpu")),

            dl_enabled: b("dl_enabled", true),
            dl_processes: parse_list(get("dl_processes")),
            // 阈值不能为 0: 那会让"零流量也判定为忙"从而永久持有
            dl_io_kbps: u("dl_io_kbps", 50).max(1),
            dl_tcp_conns: u("dl_tcp_conns", 4).clamp(1, 10000) as u32,

            hint_enabled: b("hint_enabled", true),
            hint_ttl_secs: u("hint_ttl_secs", 60).max(10),

            raw,
        }
    }

    /// 就地替换某键的值, 返回完整的新文件内容。**不落盘**, 便于单测。
    ///
    /// 只改第一次出现的位置 —— 与 `from_text` 的"重复键取第一个"必须一致,
    /// 否则用户配置里有重复键时开关会永久静默失效。
    fn rewrite(&mut self, key: &str, value: &str) -> String {
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
        self.raw.join("\n") + "\n"
    }

    /// 托盘开关回写: 就地替换该键的值, 其他行(含注释)原样保留。
    ///
    /// 返回是否成功落盘。失败(只读文件/磁盘满/ACL)必须让调用方知道 ——
    /// 否则托盘上的勾看起来变了, 下次加载又变回去, 用户完全不明白发生了什么。
    pub fn set_and_save(&mut self, key: &str, value: &str) -> Result<(), String> {
        let text = self.rewrite(key, value);
        let result = write_atomic(&config_path(), &text).map_err(|e| e.to_string());
        // 无论是否落盘成功, 内存里的 self 都更新为新值:
        // 失败时至少本次运行的行为符合用户意图
        *self = Config::from_text(&text);
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 重复键必须取第一个 —— 与 rewrite 改写的位置一致。
    /// 若取最后一个, 用户配置里有重复键时托盘开关会永久静默失效。
    #[test]
    fn duplicate_key_takes_first_and_rewrite_matches() {
        let text = "audio_enabled = false\nnet_enabled = true\naudio_enabled = true\n";
        let mut cfg = Config::from_text(text);
        assert!(!cfg.audio_enabled, "重复键应取第一个(false)");

        // 改写后必须真的生效, 否则就是"点了没反应"的静默失效
        let out = cfg.rewrite("audio_enabled", "true");
        assert!(Config::from_text(&out).audio_enabled);
        assert_eq!(out.matches("audio_enabled").count(), 2, "不该新增行");
    }

    #[test]
    fn strips_inline_comments_and_keeps_them_on_rewrite() {
        let text = "poll_interval_secs = 30   # 我的注释\ngrace_secs = 45 ; 分号也算\n";
        let cfg = Config::from_text(text);
        assert_eq!(cfg.poll_interval_secs, 30);
        assert_eq!(cfg.grace_secs, 45);

        let mut cfg2 = Config::from_text(text);
        let out = cfg2.rewrite("poll_interval_secs", "10");
        assert!(out.contains("# 我的注释"), "行尾注释必须保留: {}", out);
        assert_eq!(Config::from_text(&out).poll_interval_secs, 10);
    }

    #[test]
    fn rewrite_preserves_comment_lines_and_order() {
        let text = "# 顶部说明\naudio_enabled = true\n\n# 另一段\nnet_enabled = true\n";
        let mut cfg = Config::from_text(text);
        let out = cfg.rewrite("net_enabled", "false");
        assert!(out.contains("# 顶部说明"));
        assert!(out.contains("# 另一段"));
        assert!(out.starts_with("# 顶部说明"));
    }

    #[test]
    fn missing_key_is_appended() {
        let mut cfg = Config::from_text("audio_enabled = true\n");
        let out = cfg.rewrite("hint_enabled", "false");
        assert!(out.contains("hint_enabled = false"));
        assert!(!Config::from_text(&out).hint_enabled);
    }

    /// 阈值为 0 会让"零流量也判定为忙"从而永久持有, 必须被夹到 >=1
    #[test]
    fn zero_thresholds_are_clamped() {
        let cfg = Config::from_text("net_threshold_kbps = 0\ndl_io_kbps = 0\n");
        assert!(cfg.net_threshold_kbps >= 1);
        assert!(cfg.dl_io_kbps >= 1);
    }

    #[test]
    fn fast_poll_never_exceeds_poll_interval() {
        let cfg = Config::from_text("poll_interval_secs = 5\nfast_poll_secs = 60\n");
        assert!(cfg.fast_poll_secs <= cfg.poll_interval_secs);
        // 0 表示显式关闭, 必须保留
        assert_eq!(Config::from_text("fast_poll_secs = 0\n").fast_poll_secs, 0);
    }

    #[test]
    fn malformed_values_fall_back_to_defaults() {
        let cfg = Config::from_text("poll_interval_secs = abc\npolicy_ac = 乱写\n");
        assert_eq!(cfg.poll_interval_secs, 15);
        assert_eq!(cfg.policy_ac, "system");
    }

    #[test]
    fn bool_accepts_common_spellings() {
        for (s, want) in [("on", true), ("YES", true), ("1", true), ("off", false), ("no", false)] {
            let cfg = Config::from_text(&format!("audio_enabled = {}\n", s));
            assert_eq!(cfg.audio_enabled, want, "输入 {}", s);
        }
    }

    #[test]
    fn list_parsing_trims_and_drops_empties() {
        let cfg = Config::from_text("dl_processes =  a.exe , , b.exe ,\n");
        assert_eq!(cfg.dl_processes, vec!["a.exe", "b.exe"]);
    }

    /// 默认配置必须自洽: 每个键都能被解析出预期值
    #[test]
    fn default_config_parses_to_documented_values() {
        let cfg = Config::from_text(DEFAULT_CONFIG);
        assert_eq!(cfg.poll_interval_secs, 15);
        assert_eq!(cfg.fast_poll_secs, 2);
        assert_eq!(cfg.grace_secs, 90);
        assert_eq!(cfg.policy_ac, "system");
        assert_eq!(cfg.policy_dc, "display");
        assert!(cfg.never_wake_display);
        assert!(!cfg.sleep_on_release);
        assert_eq!(cfg.net_threshold_kbps, 1024);
        assert_eq!(cfg.audio_hold_secs, 20);
        assert!(cfg.audio_ignore.iter().any(|s| s.eq_ignore_ascii_case("wallpaper64.exe")));
        // BT 客户端不该在默认名单里: 做种时连接数高而吞吐为零
        assert!(
            !cfg.dl_processes.iter().any(|s| s.to_lowercase().contains("qbittorrent")),
            "默认下载器名单不应含 BT 客户端"
        );
    }

    /// migrate 只补缺失键, 不动已有值, 且要带上注释
    #[test]
    fn migrate_adds_missing_keys_with_comments() {
        let old = "poll_interval_secs = 7\n";
        let merged = migrate_text(old);
        let cfg = Config::from_text(&merged);
        assert_eq!(cfg.poll_interval_secs, 7, "已有值不能被覆盖");
        assert!(merged.contains("audio_hold_secs"), "缺失键要补上");
        assert!(merged.contains('#'), "补的键要带注释");
    }

    #[test]
    fn migrate_is_idempotent() {
        let once = migrate_text(DEFAULT_CONFIG);
        assert_eq!(once, DEFAULT_CONFIG, "键齐全时不该改动");
    }
}
