//! 在线更新: 查 GitHub Releases, 提示, 下载, 交给用户决定是否安装。
//!
//! 三条刻意的原则:
//!
//! 1. **只查不装** —— 从不静默替换自己的 exe。一个防休眠守护进程若在后台悄悄换掉
//!    自己, 用户没有任何机会知道换了什么。
//! 2. **零新依赖** —— JSON 用一个够用的手写扫描器, HTTP 用系统的 WinHTTP, 下载用
//!    urlmon。为"查个版本号"引入 reqwest + serde 会把 exe 从 0.4 MB 推到 2 MB+,
//!    而体积正是这个项目的卖点。
//! 3. **失败无声** —— 离线、代理没开、GitHub 连不上都是常态。任何失败只写日志,
//!    不弹窗, 也绝不影响托盘主循环。
//!
//! 线程模型: 一个 `stayawake-update` 后台线程 + 一个按需下载线程。UI 线程只读
//! `available()` / `menu_status()` 这些加锁的快照, 从不阻塞在网络上。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use windows::core::PCWSTR;
use windows::Win32::Foundation::HWND;
use windows::Win32::Networking::WinHttp::{
    WinHttpAddRequestHeaders, WinHttpCloseHandle, WinHttpConnect, WinHttpOpen, WinHttpOpenRequest,
    WinHttpQueryDataAvailable, WinHttpQueryHeaders, WinHttpReadData, WinHttpReceiveResponse,
    WinHttpSendRequest, WinHttpSetTimeouts, WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
    WINHTTP_ACCESS_TYPE_DEFAULT_PROXY, WINHTTP_ADDREQ_FLAG_ADD, WINHTTP_FLAG_SECURE,
    WINHTTP_QUERY_FLAG_NUMBER, WINHTTP_QUERY_STATUS_CODE,
};
use windows::Win32::System::Com::Urlmon::URLDownloadToFileW;
use windows::Win32::System::Com::{CoInitializeEx, IBindStatusCallback, COINIT_APARTMENTTHREADED};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::{
    MessageBoxW, IDYES, MB_ICONINFORMATION, MB_ICONWARNING, MB_OK, MB_SETFOREGROUND, MB_YESNO,
    SW_SHOWNORMAL,
};

/// 仓库地址写死, 不做配置 —— 能改仓库地址的更新器等于一个远程代码执行入口。
const API_HOST: &str = "api.github.com";
const API_PATH: &str = "/repos/Ma6302/stayawake/releases?per_page=20";
const API_PORT: u16 = 443;
/// 发布页, 打不开下载时给用户的兜底入口
pub const RELEASES_PAGE: &str = "https://github.com/Ma6302/stayawake/releases/latest";
/// GitHub 的 API 不带 User-Agent 直接 403, 所以这个头不是礼貌而是必需
const USER_AGENT: &str = "stayawake";
/// 响应体上限。20 个 release 的 JSON 约 100 KB, 给足余量但别让畸形响应吃内存。
const MAX_BODY: usize = 512 * 1024;
/// 被 GitHub 限流后多久再试一次。
///
/// GitHub 对匿名请求按 **出口 IP** 限 60 次/小时。走代理时那是代理机房的 IP, 和别人
/// 共用, 配额随时可能被别人用光 —— 实测就是这样: `API rate limit exceeded for
/// 103.62.49.170`。它不是"查不到新版本", 而是"这一刻查不了", 所以等满
/// `update_interval_hours`(默认 24 小时) 再试是最差的选择: 用户可能整整一天收不到更新
/// 提示。10 分钟一次既不激进, 也够在配额空出来的第一时间发现新版本。
const RETRY_AFTER_SECS: u64 = 600;
/// 限流后的下一次检查间隔; 0 = 没有待办的重试(正常节奏)。
static RETRY_SECS: AtomicU64 = AtomicU64::new(0);

/// 这个错误是"被限流"而不是"真出错了"吗。
///
/// 只看响应体里的措辞: GitHub 的 403 会在 JSON 的 `message` 里写明是
/// `API rate limit exceeded`, 而缺 User-Agent 之类是别的措辞 —— 后者重试没用。
fn is_rate_limited(err: &str) -> bool {
    let low = err.to_ascii_lowercase();
    low.contains("rate limit") || low.contains("rate-limit")
}

// ───────────────────────── 版本号 ─────────────────────────

/// 只认 `主.次.修订`, 够用了 —— 这是一个单 exe 的托盘工具, 不是库。
///
/// `released` 参与排序: 同号的正式版必须排在预发布版之上, 否则 `0.2.0-beta.1`
/// 会被判成比 `0.2.0` 新, 跟用户在 stable 通道上的预期正好相反。
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
struct Version {
    major: u64,
    minor: u64,
    patch: u64,
    released: bool,
}

impl Version {
    fn parse(s: &str) -> Option<Version> {
        let s = s.trim();
        let s = s.strip_prefix('v').or_else(|| s.strip_prefix('V')).unwrap_or(s);
        // `-` 之后是预发布标识, `+` 之后是构建元数据(不影响新旧)
        let (core, suffix) = match s.find(['-', '+']) {
            Some(i) => (&s[..i], &s[i..]),
            None => (s, ""),
        };
        let mut it = core.split('.');
        let major = it.next()?.parse().ok()?;
        let minor = it.next().unwrap_or("0").parse().unwrap_or(0);
        let patch = it.next().unwrap_or("0").parse().unwrap_or(0);
        Some(Version {
            major,
            minor,
            patch,
            released: !suffix.starts_with('-'),
        })
    }
}

// ───────────────────────── 极简 JSON 扫描 ─────────────────────────
//
// 手写而不是上 serde_json 的理由见文件头。这里只需要读, 且只读几个已知的字符串键,
// 所以不做通用解析器 —— 只做"括号配对 + 取键值", 大约 150 行, 且全部有测试。

/// `src[open]` 是 `{` 或 `[`, 返回内部区间 `(start, end)`(含首不含尾)。
/// 字符串字面量与转义会被跳过, 所以 URL 里的 `]`、正文里的引号都不会骗到它。
fn match_bracket(src: &[u8], open: usize) -> Option<(usize, usize)> {
    let (o, c) = match src.get(open)? {
        b'{' => (b'{', b'}'),
        b'[' => (b'[', b']'),
        _ => return None,
    };
    let mut depth = 0usize;
    let mut i = open;
    let mut in_str = false;
    while i < src.len() {
        let b = src[i];
        if in_str {
            if b == b'\\' {
                i += 2;
                continue;
            }
            if b == b'"' {
                in_str = false;
            }
        } else if b == b'"' {
            in_str = true;
        } else if b == o {
            depth += 1;
        } else if b == c {
            depth -= 1;
            if depth == 0 {
                return Some((open + 1, i));
            }
        }
        i += 1;
    }
    None
}

/// 取 `"key"` 之后紧跟的字符串值。找不到、类型不对、JSON 截断一律返回 None。
fn json_string(src: &str, key: &str) -> Option<String> {
    let pat = format!("\"{key}\"");
    let at = src.find(&pat)?;
    let rest = src[at + pat.len()..].trim_start();
    let rest = rest.strip_prefix(':')?.trim_start();
    let rest = rest.strip_prefix('"')?;
    let mut out = String::new();
    let mut chars = rest.char_indices();
    while let Some((_, ch)) = chars.next() {
        match ch {
            '"' => return Some(out),
            '\\' => {
                let (_, esc) = chars.next()?;
                match esc {
                    'n' => out.push('\n'),
                    't' => out.push('\t'),
                    'r' => out.push('\r'),
                    'b' => out.push('\u{8}'),
                    'f' => out.push('\u{c}'),
                    'u' => {
                        // \uXXXX。代理对(astral 面)不处理 —— 版本号和下载地址都是
                        // ASCII, 真遇上就当替换字符, 不值得为它写 20 行。
                        let hex: String = chars.by_ref().take(4).map(|(_, c)| c).collect();
                        let cp = u32::from_str_radix(&hex, 16).ok()?;
                        out.push(char::from_u32(cp).unwrap_or('\u{fffd}'));
                    }
                    other => out.push(other),
                }
            }
            other => out.push(other),
        }
    }
    None
}

fn json_bool(src: &str, key: &str) -> Option<bool> {
    let pat = format!("\"{key}\"");
    let at = src.find(&pat)?;
    let rest = src[at + pat.len()..].trim_start();
    let rest = rest.strip_prefix(':')?.trim_start();
    if rest.starts_with("true") {
        Some(true)
    } else if rest.starts_with("false") {
        Some(false)
    } else {
        None
    }
}

/// 把 `[...]` 里的一层 `{...}` 对象逐个切出来。
fn split_objects(inner: &str) -> Vec<&str> {
    let bytes = inner.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'{' {
            match match_bracket(bytes, i) {
                Some((s, e)) => {
                    out.push(&inner[s..e]);
                    i = e + 1;
                    continue;
                }
                // 括号不配对(截断的响应): 与其猜, 不如放弃剩下的
                None => break,
            }
        }
        i += 1;
    }
    out
}

/// 从 releases 数组正文里切出每个 release 对象。
fn split_releases(body: &str) -> Vec<&str> {
    let Some(open) = body.find('[') else {
        return Vec::new();
    };
    match match_bracket(body.as_bytes(), open) {
        Some((s, e)) => split_objects(&body[s..e]),
        None => Vec::new(),
    }
}

// ───────────────────────── 数据模型 ─────────────────────────

/// 一个可用的新版本。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Release {
    /// 已去掉 tag 的 `v` 前缀, 例如 `0.2.0`
    pub version: String,
    /// Inno Setup 安装包 —— 有就优先用它
    pub setup_url: Option<String>,
    /// 便携 exe —— 没有安装包时的备选
    pub exe_url: Option<String>,
    /// 发布页, 给"打开下载页"用
    pub page: String,
}

impl Release {
    pub fn download_url(&self) -> Option<&str> {
        self.setup_url.as_deref().or(self.exe_url.as_deref())
    }

    /// 下载后在本地用的文件名。取自 URL 末段, 这样安装包和便携版不会互相覆盖。
    fn file_name(&self) -> String {
        self.download_url()
            .and_then(|u| u.rsplit('/').next())
            .filter(|n| !n.is_empty() && n.to_ascii_lowercase().ends_with(".exe"))
            .unwrap_or("stayawake-update.exe")
            .to_string()
    }

    /// 给用户看的说明: 明确告诉他这是安装包还是便携版, 因为两者后续步骤不同。
    pub fn kind(&self) -> &'static str {
        if self.setup_url.is_some() {
            "安装包"
        } else {
            "便携版 exe"
        }
    }
}

/// 解析单个 release 对象。`stable_only` 时跳过预发布。
///
/// GitHub 的 release JSON 里 `assets` 排在 `body` 之前(结构体字段序), 而 `body`
/// 是用户可控的正文, 可能包含引号甚至 `"assets"` 字样。所以下面取的**第一个**
/// `"assets"` 一定是真键 —— 这个假设依赖字段序, 但正因为字段序固定才敢这么省。
fn parse_release(obj: &str, stable_only: bool) -> Option<Release> {
    if json_bool(obj, "draft").unwrap_or(false) {
        return None;
    }
    if stable_only && json_bool(obj, "prerelease").unwrap_or(false) {
        return None;
    }
    let tag = json_string(obj, "tag_name")?;
    let page = json_string(obj, "html_url").unwrap_or_else(|| RELEASES_PAGE.to_string());

    let mut setup_url = None;
    let mut exe_url = None;
    if let Some(key) = obj.find("\"assets\"") {
        if let Some(open) = obj[key..].find('[').map(|o| key + o) {
            if let Some((s, e)) = match_bracket(obj.as_bytes(), open) {
                for asset in split_objects(&obj[s..e]) {
                    // 只认资源的 name / browser_download_url; 装不下就跳过
                    let (Some(name), Some(url)) = (
                        json_string(asset, "name"),
                        json_string(asset, "browser_download_url"),
                    ) else {
                        continue;
                    };
                    let lower = name.to_ascii_lowercase();
                    if lower.ends_with("-setup.exe") {
                        setup_url.get_or_insert(url);
                    } else if lower.ends_with(".exe") {
                        exe_url.get_or_insert(url);
                    }
                }
            }
        }
    }
    // 什么都不能下载的 release 没意义(例如只有源码包)
    if setup_url.is_none() && exe_url.is_none() {
        return None;
    }
    Some(Release {
        version: tag
            .strip_prefix('v')
            .or_else(|| tag.strip_prefix('V'))
            .unwrap_or(&tag)
            .to_string(),
        setup_url,
        exe_url,
        page,
    })
}

/// 在 release 列表里挑出比 `local` 新的最高版本。
///
/// 逐条比较取最大, 而不是取第一条: GitHub 按发布时间倒序返回, 但用户完全可能
/// 手动装了一个比列表头更新的构建 —— 那时"取第一条"会把降级当升级推给他。
pub fn newest(body: &str, stable_only: bool, local: &str) -> Option<Release> {
    let mut best: Option<(Version, Release)> = None;
    for obj in split_releases(body) {
        let Some(rel) = parse_release(obj, stable_only) else {
            continue;
        };
        let Some(v) = Version::parse(&rel.version) else {
            continue;
        };
        if best.as_ref().is_none_or(|(bv, _)| v > *bv) {
            best = Some((v, rel));
        }
    }
    let (v, rel) = best?;
    if let Some(local) = Version::parse(local) {
        if v <= local {
            return None;
        }
    }
    Some(rel)
}

// ───────────────────────── 共享状态 ─────────────────────────

#[derive(Default)]
struct State {
    /// 已发现且比当前新的版本; None = 已是最新(或还没查过)
    latest: Option<Release>,
    /// 至少成功查到过一次
    checked: bool,
    /// 上次失败原因。只进日志和状态详情, 不弹窗。
    error: Option<String>,
    /// 正在查询。菜单行要显示"正在检查更新…", 否则点完没有任何反馈。
    checking: bool,
    downloading: bool,
    downloaded: Option<PathBuf>,
}

static STATE: OnceLock<Mutex<State>> = OnceLock::new();

fn state() -> &'static Mutex<State> {
    STATE.get_or_init(|| Mutex::new(State::default()))
}

/// 拿锁时忽略中毒: 更新线程 panic 过也不该让整个菜单读不到状态。
fn lock_state() -> std::sync::MutexGuard<'static, State> {
    state().lock().unwrap_or_else(|e| e.into_inner())
}

struct Settings {
    enabled: bool,
    interval: Duration,
    channel: String,
}

impl Default for Settings {
    fn default() -> Self {
        // 默认关闭: 配置没读进来之前一个包都不要发。
        Settings {
            enabled: false,
            interval: Duration::from_secs(24 * 3600),
            channel: "stable".to_string(),
        }
    }
}

static SETTINGS: OnceLock<(Mutex<Settings>, Condvar)> = OnceLock::new();
/// 「立即检查」按钮。单独用原子量, 免得 configure 的每次调用都要区分"是不是用户按的"。
static FORCE: AtomicBool = AtomicBool::new(false);

fn settings() -> &'static (Mutex<Settings>, Condvar) {
    SETTINGS.get_or_init(|| (Mutex::new(Settings::default()), Condvar::new()))
}

fn lock_settings() -> std::sync::MutexGuard<'static, Settings> {
    settings().0.lock().unwrap_or_else(|e| e.into_inner())
}

/// 由 worker 每次(重新)加载配置后调用。这里只存值并唤醒线程, 不做任何 I/O ——
/// configure 跑在 worker 的循环里, 不能让它去连网。
pub fn configure(enabled: bool, interval_hours: u64, channel: &str) {
    {
        let mut g = lock_settings();
        let changed = g.enabled != enabled
            || g.interval != Duration::from_secs(interval_hours * 3600)
            || g.channel != channel;
        g.enabled = enabled;
        g.interval = Duration::from_secs(interval_hours * 3600);
        g.channel = channel.to_string();
        if !changed {
            return;
        }
    }
    // 开关翻转或通道变化 -> 立刻按新设置重新判断, 不必等满一个间隔
    settings().1.notify_all();
}

// ───────────────────────── 对外快照(UI 线程用) ─────────────────────────

/// 有比当前版本新的版本时返回它。托盘红点也用它。
pub fn available() -> Option<Release> {
    lock_state().latest.clone()
}

/// 是否有更新 —— 给图标画红点用, 比 `available()` 少一次 String 分配。
pub fn has_update() -> bool {
    lock_state().latest.is_some()
}

/// 是否正在查询。菜单用它把「检查更新」暂时改成「正在检查更新…」。
/// 查询是后台线程, 这一步只读一个 bool, 不会卡住 UI。
pub fn checking() -> bool {
    lock_state().checking
}

/// 菜单顶部那行状态。None = 什么都不显示。
pub fn menu_status() -> Option<String> {
    let st = lock_state();
    if st.downloading {
        return Some("正在下载安装包…".to_string());
    }
    if let Some(p) = &st.downloaded {
        return Some(format!("已下载: {} —— 点击安装", p.display()));
    }
    st.latest
        .as_ref()
        .map(|r| format!("发现新版本 {} ({})", r.version, r.kind()))
}

/// 状态详情对话框里的几行。失败也照实说 —— 用户排查"为什么不提示更新"时
/// 最需要的就是这行原因。
pub fn detail_lines() -> Vec<String> {
    let st = lock_state();
    let mut v = Vec::new();
    match &st.latest {
        Some(r) => v.push(format!("发现新版本: {} ({})", r.version, r.kind())),
        None if st.checked => v.push("更新检查: 已是最新版本".to_string()),
        None => v.push("更新检查: 尚未成功查询过".to_string()),
    }
    if let Some(e) = &st.error {
        v.push(format!("上次查询失败: {e}"));
    }
    if let Some(p) = &st.downloaded {
        v.push(format!("已下载: {}", p.display()));
    }
    v
}

/// 请求一次立刻检查。菜单里的「立即检查」调它。
pub fn check_now() {
    FORCE.store(true, Ordering::SeqCst);
    // 顺手把"还在等一个间隔"这件事打断
    settings().1.notify_all();
}

// ───────────────────────── 通知托盘 ─────────────────────────

/// 托盘消息窗口。更新线程拿到它才能把"状态变了"推给 UI ——
/// 否则菜单里那行"正在检查更新…"要等用户下次打开菜单才会变。
static OWNER: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);

/// 由 tray::run 在消息窗口建好后调用一次。更新模块自己不建窗口。
pub fn set_owner(hwnd: HWND) {
    OWNER.store(hwnd.0, Ordering::SeqCst);
}

/// 状态有变化时敲一下 UI 线程。没有 owner 就什么也不做(例如 --status 这类无界面运行)。
fn notify_tray() {
    let raw = OWNER.load(Ordering::SeqCst);
    if raw == 0 {
        return;
    }
    unsafe {
        let _ = windows::Win32::UI::WindowsAndMessaging::PostMessageW(
            HWND(raw),
            crate::WM_STATE_CHANGED,
            windows::Win32::Foundation::WPARAM(0),
            windows::Win32::Foundation::LPARAM(0),
        );
    }
}

// ───────────────────────── 后台线程 ─────────────────────────

/// 启动更新线程。重复调用是空操作。
pub fn spawn() {
    static STARTED: OnceLock<()> = OnceLock::new();
    if STARTED.set(()).is_err() {
        return;
    }
    let _ = std::thread::Builder::new()
        .name("stayawake-update".to_string())
        .stack_size(512 * 1024)
        .spawn(thread_main);
}

fn thread_main() {
    // 首查不立刻做: 启动瞬间托盘图标还没挂上, 没必要跟首轮检测抢 CPU 和 DNS
    let startup_delay = Duration::from_secs(5);
    let mut last: Option<Instant> = None;
    loop {
        // `wait` = 这次在条件变量上睡多久; `due` = 距上次检查要过多久才真的再查一次。
        // 两者在限流重试时相等, 在 interval == 0 时故意不等(睡很久但永不复查)。
        let (wait, due) = {
            let g = lock_settings();
            schedule(
                g.enabled,
                RETRY_SECS.load(Ordering::SeqCst),
                g.interval,
                last.map(|t| t.elapsed()),
                startup_delay,
            )
        };
        {
            let g = lock_settings();
            // wait_timeout 会消费 guard 并返回它; 这里就是要等完重新读一遍设置,
            // 所以直接丢掉返回值, 下一轮重新加锁
            let _ = settings().1.wait_timeout(g, wait);
        }
        let force = FORCE.swap(false, Ordering::SeqCst);
        let (enabled, channel) = {
            let g = lock_settings();
            (g.enabled, g.channel.clone())
        };
        if !enabled && !force {
            continue;
        }
        if !force {
            if let Some(t) = last {
                // due == 0 表示启动后只查一次
                if due.is_zero() || t.elapsed() < due {
                    continue;
                }
            }
        }
        last = Some(Instant::now());
        check_once(&channel);
    }
}

/// 决定"睡多久"和"距上次多久才算到期"。
///
/// 抽成纯函数不是为了好看: 这段逻辑有三个互相盖住的输入(开关 / 限流重试 / 配置间隔),
/// 而它在真实运行里**观察不到** —— 限流重试那一轮如果还是失败, 日志按"同一个错误只记
/// 一次"被去重了, 日志里什么都不会多。所以只能在单元测试里把它钉死。
///
/// 返回 `(wait, due)`:
///
/// * `(300s, 0)` —— 关掉了; 等 `configure` 唤醒, 超时只是兜底
/// * `(retry, retry)` —— 上一轮被限流: 短重试优先于配置的间隔, 但也不能比 retry 更早
/// * `(startup_delay, interval)` —— 首查
/// * `(3600s, 0)` —— `interval == 0` 表示只在启动时查一次; 睡很久且**永不**复查
/// * `(interval - 已过, interval)` —— 正常节奏, 至少睡 1 秒(避免忙等)
fn schedule(
    enabled: bool,
    retry_secs: u64,
    interval: Duration,
    since_last: Option<Duration>,
    startup_delay: Duration,
) -> (Duration, Duration) {
    if !enabled {
        return (Duration::from_secs(300), Duration::ZERO);
    }
    if retry_secs > 0 {
        let d = Duration::from_secs(retry_secs);
        return (d, d);
    }
    match since_last {
        None => (startup_delay, interval),
        Some(_) if interval.is_zero() => (Duration::from_secs(3600), Duration::ZERO),
        Some(elapsed) => (
            interval
                .saturating_sub(elapsed)
                .max(Duration::from_secs(1)),
            interval,
        ),
    }
}

fn check_once(channel: &str) {
    let local = env!("CARGO_PKG_VERSION");
    // stable 通道只用正式版; beta 把预发布也算上
    let stable_only = channel != "beta";
    lock_state().checking = true;
    // 点了「检查更新」菜单还开着, 得立刻让它显示"正在检查更新…"
    notify_tray();
    let result = http_get(API_HOST, API_PATH, API_PORT);
    {
        let mut st = lock_state();
        st.checking = false;
        match result {
            Ok(body) => {
                let found = newest(&body, stable_only, local);
                // 查通了就把限流重试计划清掉, 回到正常节奏
                RETRY_SECS.store(0, Ordering::SeqCst);
                // 之前没查成过 / 上次是失败的 —— 这两种情况才需要记一行"查通了",
                // 否则日志里成功和失败无法区分, 在线更新到底通没通只能靠猜。
                let recovered = !st.checked || st.error.is_some();
                st.checked = true;
                st.error = None;
                match (&found, &st.latest) {
                    (Some(rel), old) if old.as_ref().map(|r| &r.version) != Some(&rel.version) => {
                        crate::log::event(&format!("update: 发现新版本 {}", rel.version));
                    }
                    (None, Some(old)) => {
                        // 之前提示过的版本从列表里消失了(被撤回/转预发布)
                        crate::log::event(&format!("update: {} 已不在发布列表中", old.version));
                    }
                    _ => {
                        if recovered {
                            crate::log::event(&format!(
                                "update: 查询正常, 当前已是最新 (v{local})"
                            ));
                        }
                    }
                }
                st.latest = found;
            }
            Err(e) => {
                let limited = is_rate_limited(&e);
                // 同一个错误只记一次: 离线用户每 24 小时被写一行日志很烦
                if st.error.as_deref() != Some(e.as_str()) {
                    if limited {
                        crate::log::event(&format!(
                            "update: 被 GitHub 限流(匿名配额按出口 IP 算, 走代理时是机房共用 IP), \
                             {RETRY_AFTER_SECS} 秒后再试: {e}"
                        ));
                    } else {
                        crate::log::event(&format!("update: 查询失败: {e}"));
                    }
                }
                st.error = Some(e);
                // 只有限流值得提前重试; 别的错(离线、代理没开)晚点试也一样
                RETRY_SECS.store(if limited { RETRY_AFTER_SECS } else { 0 }, Ordering::SeqCst);
            }
        }
    }
    // 查完再敲一次: 菜单里那行要变成"发现新版本"或者消失, 托盘图标也可能要加红点
    notify_tray();
}

// ───────────────────────── HTTP ─────────────────────────

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// GitHub API 要求的请求头。
///
/// `WinHttpOpen` 的 agent 参数本身就会变成 `User-Agent` 头(微软文档原话:
/// "This name is used as the user agent in the HTTP protocol"), 所以这里显式再写一遍
/// 在运行期是冗余的 —— 保留它是因为"这个请求必须带 User-Agent"是 GitHub 的硬性要求
/// (缺了回 403 "Request forbidden by administrative rules"), 而会话 agent 与请求头
/// 之间隔着一次跨 API 的隐式约定: 哪天换掉建会话的那几行, 这个要求就会不声不响地丢掉。
/// 顺带让它可以被单测钉住。
fn request_headers() -> String {
    format!(
        "Accept: application/vnd.github+json\r\n\
         X-GitHub-Api-Version: 2022-11-28\r\n\
         User-Agent: {USER_AGENT}\r\n"
    )
}

/// 保证句柄一定被关。WinHTTP 的句柄是裸指针, 用 `?` 提前返回很容易漏掉。
struct WinHttpHandle(*mut std::ffi::c_void);

impl Drop for WinHttpHandle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // 关句柄失败没有补救手段, 也不该影响调用方
            let _ = unsafe { WinHttpCloseHandle(self.0) };
        }
    }
}

/// GET 一个 JSON 接口。
///
/// 所有 URL 都是编译期常量, 所以这里没有 URL 解析 —— 少一段 `WinHttpCrackUrl` 的
/// 互操作就少一处出错的地方。代理走 `AUTOMATIC_PROXY`, 于是 Clash 等工具设的
/// 系统代理会自动生效, 不需要用户额外配置。
fn http_get(host: &str, path: &str, port: u16) -> Result<String, String> {
    unsafe {
        let agent = wide(USER_AGENT);
        let mut session = WinHttpOpen(
            PCWSTR(agent.as_ptr()),
            WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
            PCWSTR::null(),
            PCWSTR::null(),
            0,
        );
        if session.is_null() {
            // AUTOMATIC_PROXY 要 Win8+。老系统退回 WinINet 默认代理设置。
            session = WinHttpOpen(
                PCWSTR(agent.as_ptr()),
                WINHTTP_ACCESS_TYPE_DEFAULT_PROXY,
                PCWSTR::null(),
                PCWSTR::null(),
                0,
            );
        }
        if session.is_null() {
            return Err("WinHttpOpen 失败".to_string());
        }
        let _session = WinHttpHandle(session);
        // 连不上/卡住都不能把更新线程挂死: 最坏 5+5+5+8 秒
        let _ = WinHttpSetTimeouts(session, 5000, 5000, 5000, 8000);

        let hostw = wide(host);
        let connect = WinHttpConnect(session, PCWSTR(hostw.as_ptr()), port, 0);
        if connect.is_null() {
            return Err("WinHttpConnect 失败".to_string());
        }
        let _connect = WinHttpHandle(connect);

        let verb = wide("GET");
        let pathw = wide(path);
        let request = WinHttpOpenRequest(
            connect,
            PCWSTR(verb.as_ptr()),
            PCWSTR(pathw.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            std::ptr::null(),
            WINHTTP_FLAG_SECURE,
        );
        if request.is_null() {
            return Err("WinHttpOpenRequest 失败".to_string());
        }
        let _request = WinHttpHandle(request);

        // 长度取自切片, 所以这里**不带**结尾的 NUL
        let headers = wide(&request_headers());
        WinHttpAddRequestHeaders(request, &headers[..headers.len() - 1], WINHTTP_ADDREQ_FLAG_ADD)
            .map_err(|e| format!("WinHttpAddRequestHeaders: {e}"))?;
        WinHttpSendRequest(request, None, None, 0, 0, 0)
            .map_err(|e| format!("WinHttpSendRequest: {e}"))?;
        WinHttpReceiveResponse(request, std::ptr::null_mut())
            .map_err(|e| format!("WinHttpReceiveResponse: {e}"))?;

        let mut code: u32 = 0;
        let mut len = std::mem::size_of::<u32>() as u32;
        WinHttpQueryHeaders(
            request,
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            PCWSTR::null(),
            Some(&mut code as *mut u32 as *mut std::ffi::c_void),
            &mut len,
            std::ptr::null_mut(),
        )
        .map_err(|e| format!("WinHttpQueryHeaders: {e}"))?;

        // 先把响应体读出来再判状态码 —— 出错时响应体才是有用的东西:
        // GitHub 的 403 会在 JSON 里写明原因("API rate limit exceeded" /
        // "Requires authentication"), 日志里只有一句 "HTTP 403" 根本无从下手。
        let mut body: Vec<u8> = Vec::new();
        loop {
            let mut avail: u32 = 0;
            WinHttpQueryDataAvailable(request, &mut avail)
                .map_err(|e| format!("WinHttpQueryDataAvailable: {e}"))?;
            if avail == 0 {
                break;
            }
            let take = (avail as usize).min(MAX_BODY - body.len());
            if take == 0 {
                return Err("响应体过大".to_string());
            }
            let mut chunk = vec![0u8; take];
            let mut read: u32 = 0;
            WinHttpReadData(
                request,
                chunk.as_mut_ptr() as *mut std::ffi::c_void,
                take as u32,
                &mut read,
            )
            .map_err(|e| format!("WinHttpReadData: {e}"))?;
            if read == 0 {
                break;
            }
            body.extend_from_slice(&chunk[..read as usize]);
        }
        // GitHub 的 API 一律 UTF-8。非法字节用替换字符, 不因为一个坏字节整包作废。
        let text = String::from_utf8_lossy(&body).into_owned();

        if code != 200 {
            let hint: String = text
                .trim()
                .replace(['\r', '\n'], " ")
                .chars()
                .take(160)
                .collect();
            return Err(if hint.is_empty() {
                format!("HTTP {code}")
            } else {
                format!("HTTP {code}: {hint}")
            });
        }
        Ok(text)
    }
}

// ───────────────────────── 下载与安装 ─────────────────────────

/// 菜单上那一行「发现新版本 / 正在下载 / 已下载」被点击时该干什么。
///
/// 三种状态三种行为, 全部在这里决定 —— 让菜单去猜状态是最容易出错的做法,
/// 而菜单那一行本来就是 `menu_status()` 拼出来的, 它知道的和这里一样多。
pub fn menu_action(owner: HWND) {
    let (downloaded, latest) = {
        let st = lock_state();
        (st.downloaded.clone(), st.latest.clone())
    };
    match (downloaded, latest) {
        // 已经下好了: 再问一次要不要运行 —— 从来不会自己装上
        (Some(path), Some(rel)) => confirm_and_run(owner, &path, &rel),
        (Some(path), None) => {
            if let Err(e) = open_path(&path) {
                crate::log::event(&format!("update: 无法打开已下载的文件: {e}"));
            }
        }
        // 有可用版本但还没下 -> 开始下载
        (None, Some(_)) => start_download(owner),
        // 什么都没有: 当成"立即检查", 至少给用户一个反馈
        (None, None) => check_now(),
    }
}

/// 开始下载。整个过程在后台线程, 菜单点完立刻关闭。
///
/// `owner` 按 `isize` 传递: `HWND` 不是 `Send`, 直接捕获进线程编译不过。
pub fn start_download(owner: HWND) {
    let Some(rel) = available() else {
        return;
    };
    {
        let mut st = lock_state();
        if st.downloading {
            return;
        }
        st.downloading = true;
        st.downloaded = None;
    }
    notify_tray();
    let owner_raw = owner.0;
    let spawned = std::thread::Builder::new()
        .name("stayawake-download".to_string())
        .stack_size(512 * 1024)
        .spawn(move || {
            let result = download(&rel);
            lock_state().downloading = false;
            notify_tray();
            match result {
                Ok(path) => {
                    lock_state().downloaded = Some(path.clone());
                    crate::log::event(&format!("update: 已下载到 {}", path.display()));
                    confirm_and_run(HWND(owner_raw), &path, &rel);
                }
                Err(e) => {
                    crate::log::event(&format!("update: 下载失败: {e}"));
                    message(
                        HWND(owner_raw),
                        &format!("下载 {} 失败。\n\n{}", rel.version, e),
                        MB_OK | MB_ICONWARNING,
                    );
                }
            }
        })
        .is_ok();
    if !spawned {
        lock_state().downloading = false;
    }
}

fn download(rel: &Release) -> Result<PathBuf, String> {
    let url = rel
        .download_url()
        .ok_or_else(|| "该版本没有可下载的文件".to_string())?;
    let dir = download_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("建目录失败: {e}"))?;
    let path = dir.join(rel.file_name());
    // 上一次的半截文件会让下载器以为已经完成
    let _ = std::fs::remove_file(&path);

    let (u, p) = (wide(url), wide(&path.to_string_lossy()));
    unsafe {
        // urlmon 需要 COM。这是我们自己的线程, 所以 APARTMENTTHREADED 一定成功;
        // 万一失败也不致命, URLDownloadToFileW 自己会报错。
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        URLDownloadToFileW(
            None::<&windows::core::IUnknown>,
            PCWSTR(u.as_ptr()),
            PCWSTR(p.as_ptr()),
            0,
            None::<&IBindStatusCallback>,
        )
        .map_err(|e| format!("{e}"))?;
    }
    if !path.is_file() {
        return Err("下载结束但没有产出文件".to_string());
    }
    Ok(path)
}

fn confirm_and_run(owner: HWND, path: &Path, rel: &Release) {
    let text = format!(
        "已下载 stayawake {} ({})\n\n{}\n\n现在运行吗?",
        rel.version,
        rel.kind(),
        path.display()
    );
    if message(owner, &text, MB_YESNO | MB_ICONINFORMATION) == IDYES {
        if let Err(e) = open_path(path) {
            crate::log::event(&format!("update: 无法运行安装包: {e}"));
        }
    }
}

fn message(owner: HWND, text: &str, style: windows::Win32::UI::WindowsAndMessaging::MESSAGEBOX_STYLE) -> windows::Win32::UI::WindowsAndMessaging::MESSAGEBOX_RESULT {
    let text = wide(text);
    let caption = wide("stayawake 更新");
    unsafe {
        MessageBoxW(
            owner,
            PCWSTR(text.as_ptr()),
            PCWSTR(caption.as_ptr()),
            style | MB_SETFOREGROUND,
        )
    }
}

/// 用系统默认程序打开一个本地文件。
pub fn open_path(path: &Path) -> Result<(), String> {
    let file = wide(&path.to_string_lossy());
    let verb = wide("open");
    let h = unsafe {
        ShellExecuteW(
            None,
            PCWSTR(verb.as_ptr()),
            PCWSTR(file.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    // ShellExecuteW 的返回值 <= 32 表示失败, 而且它不设 last error
    if h.0 as isize <= 32 {
        return Err(format!("ShellExecuteW 返回 {}", h.0 as isize));
    }
    Ok(())
}

/// 在浏览器里打开发布页。
pub fn open_releases_page() -> Result<(), String> {
    let url = wide(RELEASES_PAGE);
    let verb = wide("open");
    let h = unsafe {
        ShellExecuteW(
            None,
            PCWSTR(verb.as_ptr()),
            PCWSTR(url.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    if h.0 as isize <= 32 {
        return Err(format!("ShellExecuteW 返回 {}", h.0 as isize));
    }
    Ok(())
}

/// 已下载文件的存放目录。
///
/// 放临时目录而不是程序目录: 便携版可能被放在只读位置(Program Files / U 盘),
/// 而且"下载了但没装"本来就不该污染安装目录。
fn download_dir() -> PathBuf {
    std::env::temp_dir().join("stayawake-update")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 显式把 User-Agent 写进请求头。微软文档里 `WinHttpOpen` 的 agent 参数原话是
    /// "This name is used as the user agent in the HTTP protocol", 所以它在运行期其实是
    /// 冗余的; 保留并且钉住, 是因为"必须带 User-Agent"是 GitHub 的硬性规定 —— 与其依赖
    /// "会话 agent 会隐式变成请求头"这种跨 API 的约定, 不如让请求自己带上。
    /// (最初以为 403 是缺这个头造成的, 后来抓到响应体才发现是匿名配额被用光。)
    #[test]
    fn request_headers_carry_a_user_agent() {
        let h = request_headers();
        assert!(
            h.split("\r\n")
                .any(|line| line.eq_ignore_ascii_case("user-agent: stayawake")),
            "请求头里没有 User-Agent: {h:?}"
        );
        assert!(h.contains("Accept: application/vnd.github+json"));
        // 头块以 CRLF 结尾(调用方负责切掉尾部的 NUL), 且中间不能出现空行,
        // 否则空行之后的内容会被当成请求体而不是头。
        assert!(h.ends_with("\r\n"), "头块没有以 CRLF 结尾: {h:?}");
        assert!(!h.contains("\r\n\r\n"), "头块里出现了空行: {h:?}");
        // 续行反斜杠不能把缩进带进字符串
        assert!(!h.contains("  "), "头块里混进了缩进: {h:?}");
    }

    /// 响应体是实测抓到的原文(那个 IP 是代理机房, 不是用户自己的)。限流必须能认出来:
    /// 认不出来就只会等满 24 小时才重试, 用户可能整整一天收不到更新提示。
    #[test]
    fn rate_limit_is_recognised_from_the_real_body() {
        let real = "HTTP 403: {\"message\":\"API rate limit exceeded for 103.62.49.170. \
                    (But here's the good news: Authenticated requests get a higher rate limit.)\",\
                    \"documentation_url\":\"https://docs.github.com/rest/overview/resources-in-the-rest-api\"}";
        assert!(is_rate_limited(real), "实测的限流响应体没被认出来: {real}");

        // 措辞不同的 403 不是限流(比如请求头被拒), 提前重试没有意义
        assert!(!is_rate_limited(
            "HTTP 403: {\"message\":\"Request forbidden by administrative rules.\"}"
        ));
        assert!(!is_rate_limited("WinHttpConnect 失败"));
        assert!(!is_rate_limited(""));
    }

    /// 被限流后必须早点重试, 而不是等满配置的 24 小时 —— 否则用户可能整整一天收不到
    /// 更新提示。这段调度在真实运行里**观察不到**: 重试那一轮如果还是失败, 日志按
    /// "同一个错误只记一次"被去重, 什么都不会多出来, 所以只能在这里钉死。
    #[test]
    fn rate_limited_schedule_retries_soon_instead_of_waiting_the_interval() {
        let day = Duration::from_secs(24 * 3600);
        let (wait, due) = schedule(
            true,
            RETRY_AFTER_SECS,
            day,
            Some(Duration::from_secs(1)),
            Duration::from_secs(5),
        );
        assert_eq!(wait, Duration::from_secs(RETRY_AFTER_SECS));
        assert_eq!(
            due, wait,
            "到期判定必须也用重试间隔, 否则睡了 600 秒却因为不满 24 小时又被跳过"
        );
        assert!(wait < day);
    }

    /// 限流重试优先于"只在启动时查一次": 用户把间隔设成 0 也不该被永久拉黑。
    #[test]
    fn rate_limited_schedule_overrides_the_check_once_setting() {
        let (wait, due) = schedule(
            true,
            RETRY_AFTER_SECS,
            Duration::ZERO,
            Some(Duration::from_secs(1)),
            Duration::from_secs(5),
        );
        assert_eq!(wait, Duration::from_secs(RETRY_AFTER_SECS));
        assert_eq!(due, wait);
    }

    /// `interval == 0` = 只在启动时查一次: 查过之后 `due` 归零, 循环里的 `due.is_zero()`
    /// 会一直跳过它; 但 `wait` 仍要给一个长值, 不能忙等。
    #[test]
    fn zero_interval_checks_once_then_never_again() {
        let (wait, due) = schedule(
            true,
            0,
            Duration::ZERO,
            Some(Duration::from_secs(600)),
            Duration::from_secs(5),
        );
        assert_eq!(due, Duration::ZERO);
        assert_eq!(wait, Duration::from_secs(3600));
    }

    /// 首查: 等启动延迟(启动瞬间托盘图标还没挂上)。此时 `last` 还是 None, `due` 用不上。
    #[test]
    fn first_check_uses_the_startup_delay() {
        let day = Duration::from_secs(24 * 3600);
        let (wait, due) = schedule(true, 0, day, None, Duration::from_secs(5));
        assert_eq!(wait, Duration::from_secs(5));
        assert_eq!(due, day);
    }

    /// 正常节奏: 睡"间隔减去已经过去的时间"; 已经超期时也不能睡 0, 否则就成忙等了。
    #[test]
    fn normal_schedule_sleeps_the_remaining_interval() {
        let hour = Duration::from_secs(3600);
        let (wait, due) = schedule(
            true,
            0,
            hour,
            Some(Duration::from_secs(600)),
            Duration::from_secs(5),
        );
        assert_eq!(wait, Duration::from_secs(3000));
        assert_eq!(due, hour);

        let (wait, _) = schedule(
            true,
            0,
            hour,
            Some(Duration::from_secs(99999)),
            Duration::from_secs(5),
        );
        assert_eq!(wait, Duration::from_secs(1));
    }

    /// 关掉更新检查: 睡 5 分钟等 `configure` 唤醒, 超时只是兜底。
    #[test]
    fn disabled_update_check_is_never_due() {
        let (wait, due) = schedule(
            false,
            0,
            Duration::from_secs(3600),
            Some(Duration::from_secs(1)),
            Duration::from_secs(5),
        );
        assert_eq!(wait, Duration::from_secs(300));
        assert_eq!(due, Duration::ZERO);
    }

    /// 真实响应的结构骨架: `assets` 在 `body` 之前, `body` 里故意塞了引号、逗号和
    /// `[`/`]` —— 正文是用户可控的, 解析器不能被它带偏。
    const FIXTURE: &str = r#"[
  {
    "url": "https://api.github.com/repos/Ma6302/stayawake/releases/1",
    "html_url": "https://github.com/Ma6302/stayawake/releases/tag/v0.1.2",
    "id": 1,
    "tag_name": "v0.1.2",
    "name": "stayawake 0.1.2",
    "draft": false,
    "prerelease": false,
    "created_at": "2026-01-01T00:00:00Z",
    "assets": [
      {
        "url": "https://api.github.com/repos/Ma6302/stayawake/releases/assets/1",
        "name": "stayawake-0.1.2-setup.exe",
        "size": 2107626,
        "browser_download_url": "https://github.com/Ma6302/stayawake/releases/download/v0.1.2/stayawake-0.1.2-setup.exe"
      },
      {
        "url": "https://api.github.com/repos/Ma6302/stayawake/releases/assets/2",
        "name": "stayawake.exe",
        "size": 429568,
        "browser_download_url": "https://github.com/Ma6302/stayawake/releases/download/v0.1.2/stayawake.exe"
      }
    ],
    "body": "换行\n与 \"引号\" 还有 [方括号] 都会出现"
  }
]"#;

    /// 把 FIXTURE 的版本号/预发布/草稿换掉, 省得手写四份几乎一样的 JSON。
    fn variant(version: &str, prerelease: bool, draft: bool) -> String {
        FIXTURE
            .replace("v0.1.2", &format!("v{version}"))
            .replace("0.1.2", version)
            .replace("\"prerelease\": false", &format!("\"prerelease\": {prerelease}"))
            .replace("\"draft\": false", &format!("\"draft\": {draft}"))
    }

    fn releases(specs: &[(&str, bool, bool)]) -> String {
        let items: Vec<String> = specs
            .iter()
            .map(|(v, pre, draft)| variant(v, *pre, *draft))
            .collect();
        format!("[{}]", items.join(","))
    }

    #[test]
    fn version_parses_with_and_without_prefix() {
        let a = Version::parse("v1.2.3").unwrap();
        let b = Version::parse("1.2.3").unwrap();
        assert_eq!(a, b);
        assert_eq!(Version::parse("2.0.0").unwrap().major, 2);
        // 缺省字段按 0 处理, `1.2` 不该解析失败
        assert_eq!(Version::parse("1.2").unwrap().patch, 0);
        assert!(Version::parse("nightly").is_none());
        assert!(Version::parse("").is_none());
    }

    /// 10 > 9: 字符串比较会得出相反结论, 这是最容易写错的一处
    #[test]
    fn version_compare_is_numeric_not_lexical() {
        assert!(Version::parse("0.10.0").unwrap() > Version::parse("0.9.9").unwrap());
        assert!(Version::parse("1.0.0").unwrap() > Version::parse("0.99.99").unwrap());
    }

    /// 同号的预发布版必须排在正式版之下, 否则 stable 通道会推 beta
    #[test]
    fn prerelease_sorts_below_its_final_release() {
        assert!(Version::parse("0.2.0-beta.1").unwrap() < Version::parse("0.2.0").unwrap());
        assert!(Version::parse("0.2.0-beta.1").unwrap() > Version::parse("0.1.9").unwrap());
        // 构建元数据不影响新旧
        assert_eq!(
            Version::parse("0.2.0+build5").unwrap(),
            Version::parse("0.2.0").unwrap()
        );
    }

    #[test]
    fn finds_a_newer_version_and_reports_both_assets() {
        // FIXTURE 里是 v0.1.2, 所以本地版本得低于它才谈得上"有更新"
        let body = variant("0.1.3", false, false);
        let rel = newest(&body, true, "0.1.2").unwrap();
        assert_eq!(rel.version, "0.1.3");
        assert_eq!(rel.kind(), "安装包");
        assert!(rel
            .download_url()
            .unwrap()
            .ends_with("stayawake-0.1.3-setup.exe"));
        assert!(rel.exe_url.as_deref().unwrap().ends_with("stayawake.exe"));
        assert_eq!(rel.page, "https://github.com/Ma6302/stayawake/releases/tag/v0.1.3");
    }

    /// 当前版本已经是最新, 就不该提示 —— 这正是"每次启动都弹更新"的成因
    #[test]
    fn same_version_is_not_an_update() {
        assert!(newest(FIXTURE, true, "0.1.2").is_none());
    }

    /// 用户手动装了比列表头更新的构建时, 不能把降级当升级推给他
    #[test]
    fn never_offers_a_downgrade() {
        assert!(newest(FIXTURE, true, "0.5.0").is_none());
        assert!(newest(FIXTURE, true, "0.1.2").is_none());
    }

    /// 列表里有多版时取最大的那个, 而不是第一条
    #[test]
    fn picks_the_highest_version_not_the_first_entry() {
        let body = releases(&[("0.2.0", false, false), ("0.10.0", false, false), ("0.3.0", false, false)]);
        assert_eq!(newest(&body, true, "0.1.0").unwrap().version, "0.10.0");
    }

    #[test]
    fn stable_channel_skips_prereleases() {
        let body = releases(&[("0.3.0", true, false), ("0.2.0", false, false)]);
        assert_eq!(newest(&body, true, "0.1.0").unwrap().version, "0.2.0");
        // beta 通道才看得到预发布版
        assert_eq!(newest(&body, false, "0.1.0").unwrap().version, "0.3.0");
    }

    /// 草稿对任何通道都不可见(草稿的 tag 往往还没定)
    #[test]
    fn draft_releases_are_ignored_on_every_channel() {
        let body = releases(&[("0.9.0", false, true), ("0.2.0", false, false)]);
        assert_eq!(newest(&body, true, "0.1.0").unwrap().version, "0.2.0");
        assert_eq!(newest(&body, false, "0.1.0").unwrap().version, "0.2.0");
    }

    /// 只有源码包的 release 不能下载, 必须跳过 —— 否则菜单会推出一个点不动的版本
    #[test]
    fn release_without_exe_assets_is_skipped() {
        let body = r#"[{"tag_name":"v0.9.0","draft":false,"prerelease":false,
            "html_url":"https://example.invalid","assets":[
              {"name":"source.tar.gz","browser_download_url":"https://example.invalid/s.tar.gz"}]}]"#;
        assert!(newest(body, true, "0.1.0").is_none());
    }

    /// 没有安装包时退回便携 exe, 而不是判定为"没有更新"
    #[test]
    fn portable_exe_is_used_when_there_is_no_setup() {
        let body = r#"[{"tag_name":"v0.2.0","draft":false,"prerelease":false,
            "html_url":"https://example.invalid","assets":[
              {"name":"stayawake.exe","browser_download_url":"https://example.invalid/stayawake.exe"}]}]"#;
        let rel = newest(body, true, "0.1.0").unwrap();
        assert_eq!(rel.kind(), "便携版 exe");
        assert!(rel.setup_url.is_none());
        assert_eq!(rel.download_url(), Some("https://example.invalid/stayawake.exe"));
    }

    /// 文件名按 `-setup.exe` 后缀区分, 大小写不敏感(Scoop 等重打包会改大小写)
    #[test]
    fn asset_kinds_are_matched_case_insensitively() {
        let body = r#"[{"tag_name":"v0.2.0","draft":false,"prerelease":false,
            "html_url":"https://example.invalid","assets":[
              {"name":"StayAwake-0.2.0-SETUP.EXE","browser_download_url":"https://example.invalid/a.exe"}]}]"#;
        let rel = newest(body, true, "0.1.0").unwrap();
        assert_eq!(rel.kind(), "安装包");
        assert_eq!(rel.file_name(), "a.exe");
    }

    /// 截断/畸形响应必须返回 None, 不能 panic —— 它跑在后台线程, panic 会静默杀掉
    /// 整个更新功能
    #[test]
    fn malformed_bodies_never_panic() {
        for bad in [
            "",
            "[]",
            "[",
            "[{",
            "[{\"tag_name\":\"v0.2.0\"",
            "not json at all",
            "{\"message\":\"Not Found\"}",
            "[{\"tag_name\":\"v0.2.0\",\"assets\":[{\"name\":\"a.exe\"}]}]",
        ] {
            assert!(newest(bad, true, "0.1.0").is_none(), "输入 {bad:?}");
        }
    }

    #[test]
    fn json_string_unescapes_and_stops_at_the_quote() {
        let obj = r#"{"a":"x\ny\"z","b":2}"#;
        assert_eq!(json_string(obj, "a").unwrap(), "x\ny\"z");
        assert_eq!(json_string(obj, "b"), None, "数字不是字符串");
        assert_eq!(json_string(obj, "missing"), None);
        // \u 转义与截断的 \u 都不能 panic
        assert_eq!(json_string(r#"{"a":"\u0041"}"#, "a").unwrap(), "A");
        assert_eq!(json_string(r#"{"a":"\u00"}"#, "a"), None);
    }

    /// 正文里的方括号和引号不该影响 assets 的切分
    #[test]
    fn body_content_does_not_confuse_asset_extraction() {
        let rel = parse_release(FIXTURE, true).unwrap();
        assert!(rel.setup_url.is_some());
        assert!(rel.exe_url.is_some());
    }

    #[test]
    fn json_bool_reads_only_true_or_false() {
        assert_eq!(json_bool(FIXTURE, "draft"), Some(false));
        assert_eq!(json_bool(r#"{"a": true}"#, "a"), Some(true));
        assert_eq!(json_bool(r#"{"a": "true"}"#, "a"), None);
        assert_eq!(json_bool("{}", "a"), None);
    }

    /// 默认必须是"关闭": 配置还没读进来之前一个包都不该发出去
    #[test]
    fn default_settings_are_disabled() {
        let s = Settings::default();
        assert!(!s.enabled);
        assert_eq!(s.channel, "stable");
    }
}
