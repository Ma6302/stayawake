//! 自绘现代托盘菜单。
//!
//! 为什么要自绘: `TrackPopupMenu` 渲染的是系统类 `#32768` 的窗口, 像素全由系统画,
//! 圆角/背景/字体/间距一个都改不了。想要"更现代"的外观, 自己画是唯一的路。
//! `menu_style = system` 永远保留作为兜底: 远程桌面、多屏异 DPI、老系统上自绘未必更好。
//!
//! 分工是刻意的:
//!   * 纯逻辑(`Theme`/`Model`/`Metrics`/`layout`/命中测试/`place`/`fit`)完全不碰 Win32,
//!     可以单元测试 —— 这部分才是容易写错的地方;
//!   * Win32 外壳只负责建窗/绘制/消息, 薄到一眼能看完。
//!
//! 子菜单用"就地展开"的手风琴, 而不是级联弹出: 一个窗口一条绘制路径、一次命中测试,
//! 没有子弹出的生命周期、hover 桥接、失焦收起这些坑, 展开状态也成了可以纯函数测试的数据。

use std::cell::Cell;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{
    COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM,
};
use windows::Win32::Graphics::Dwm::{
    DwmSetWindowAttribute, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND,
};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, CreateFontW, CreatePen,
    CreateRoundRectRgn, CreateSolidBrush, DeleteDC, DeleteObject, DrawTextW, Ellipse, EndPaint,
    FillRect, GetMonitorInfoW, GetStockObject, GetTextExtentPoint32W, InvalidateRect, LineTo,
    MonitorFromPoint, MoveToEx, RoundRect, SelectObject, SetBkMode, SetTextColor, SetWindowRgn,
    HDC, HFONT, MONITORINFO, MONITOR_DEFAULTTONEAREST, NULL_BRUSH, PAINTSTRUCT,
    DT_LEFT, DT_NOPREFIX, DT_RIGHT, DT_SINGLELINE, DT_VCENTER, PS_SOLID, SRCCOPY, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};
use windows::Win32::UI::HiDpi::{GetDpiForSystem, GetDpiForWindow};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SetFocus, TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT, VK_DOWN, VK_ESCAPE, VK_LEFT, VK_RETURN,
    VK_RIGHT, VK_UP,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetClientRect,
    GetCursorPos, GetForegroundWindow, GetMessageW, HMENU, IsWindow, KillTimer, LoadCursorW, MSG,
    PostQuitMessage, RegisterClassW, SetForegroundWindow, SetTimer, SetWindowPos, ShowWindow,
    TranslateMessage, CS_DROPSHADOW, IDC_ARROW, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOZORDER,
    SW_SHOW, WA_INACTIVE, WM_ACTIVATE, WM_DESTROY, WM_KEYDOWN, WM_KILLFOCUS, WM_LBUTTONUP,
    WM_MOUSEMOVE, WM_PAINT, WM_SIZE, WM_TIMER, WNDCLASSW, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
    WS_POPUP,
};

/// 窗口类名。
const CLASS: PCWSTR = w!("stayawake_modern_menu");
/// `WM_MOUSELEAVE` 不在 windows-rs 的 WAM 导出里(它在 KeyboardAndMouse), 直接写字面值更省事。
const WM_MOUSELEAVE: u32 = 0x02A3;
/// 前台激活看门狗的定时器 id。
const WATCHDOG_ID: usize = 1;

static OPEN: AtomicBool = AtomicBool::new(false);
static REGISTERED: AtomicBool = AtomicBool::new(false);
/// 回调期间(可能弹出 MessageBox 并开启嵌套消息循环)的闸门: 此时除了绘制什么都不做。
static ACTING: AtomicBool = AtomicBool::new(false);

// ---------------------------------------------------------------------------
// 颜色与主题
// ---------------------------------------------------------------------------

/// 一个颜色。和 `src/icon.rs` 保持一致的 `(r,g,b)` 约定。
pub type Rgb = (u8, u8, u8);

/// `COLORREF` 是 `0x00BBGGRR`, 不是 RGB —— 项目里已经踩过一次这个坑。
fn colorref(c: Rgb) -> COLORREF {
    COLORREF(((c.2 as u32) << 16) | ((c.1 as u32) << 8) | c.0 as u32)
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Theme {
    pub bg: Rgb,
    pub bg_alt: Rgb,
    pub fg: Rgb,
    pub fg_dim: Rgb,
    pub accent: Rgb,
    pub border: Rgb,
    pub danger: Rgb,
}

impl Theme {
    pub fn dark() -> Self {
        Self {
            bg: (32, 32, 34),
            bg_alt: (54, 54, 58),
            fg: (238, 238, 240),
            fg_dim: (150, 150, 156),
            accent: (61, 123, 255),
            border: (66, 66, 72),
            danger: (240, 68, 56),
        }
    }

    pub fn light() -> Self {
        Self {
            bg: (250, 250, 251),
            bg_alt: (228, 229, 233),
            fg: (24, 24, 28),
            fg_dim: (110, 110, 118),
            accent: (0, 95, 204),
            border: (214, 215, 220),
            danger: (200, 40, 32),
        }
    }
}

/// `mode` 是 config 里的 `menu_theme`(auto / light / dark)。
pub fn resolve(mode: &str, system_dark: bool) -> Theme {
    match mode.trim().to_ascii_lowercase().as_str() {
        "dark" => Theme::dark(),
        "light" => Theme::light(),
        _ => {
            if system_dark {
                Theme::dark()
            } else {
                Theme::light()
            }
        }
    }
}

/// 读系统的"应用使用深色模式"。读不到就按浅色 —— 那是 Windows 的默认值, 猜错也只是对比度差一点。
fn system_dark() -> bool {
    unsafe {
        let mut data: u32 = 1;
        let mut size = std::mem::size_of::<u32>() as u32;
        let r = RegGetValueW(
            HKEY_CURRENT_USER,
            w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"),
            w!("AppsUseLightTheme"),
            RRF_RT_REG_DWORD,
            None,
            Some(&mut data as *mut u32 as *mut c_void),
            Some(&mut size),
        );
        r.is_ok() && data == 0
    }
}

// ---------------------------------------------------------------------------
// 纯逻辑: 菜单内容
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct Item {
    pub id: usize,
    pub label: String,
    pub checked: bool,
    pub enabled: bool,
    pub danger: bool,
}

impl Item {
    pub fn new(id: usize, label: impl Into<String>) -> Self {
        Self {
            id,
            label: label.into(),
            checked: false,
            enabled: true,
            danger: false,
        }
    }

    pub fn checked(mut self, on: bool) -> Self {
        self.checked = on;
        self
    }

    pub fn danger(mut self) -> Self {
        self.danger = true;
        self
    }
}

#[derive(Clone, Debug)]
pub enum Row {
    /// 顶部状态卡: 几个彩色圆点 + 标题 + 副标题 + 可选角标(比如"更新可用 v0.2.0")。
    Card {
        dots: Vec<Rgb>,
        title: String,
        subtitle: String,
        badge: Option<String>,
    },
    /// 分组小标题, 只显示不可点。
    Head(String),
    /// 可展开的子菜单(就地手风琴)。
    Sub { label: String, items: Vec<Item> },
    /// 叶子项。
    Item(Item),
    Sep,
}

#[derive(Clone, Debug)]
pub struct Model {
    pub rows: Vec<Row>,
}

impl Model {
    pub fn new(rows: Vec<Row>) -> Self {
        Self { rows }
    }

    /// 所有可点的 id, 顺序与界面一致。测试与"有没有漏掉命令"的断言都用它。
    pub fn leaf_ids(&self) -> Vec<usize> {
        let mut out = Vec::new();
        for r in &self.rows {
            match r {
                Row::Item(it) if it.enabled => out.push(it.id),
                Row::Sub { items, .. } => {
                    for it in items {
                        if it.enabled {
                            out.push(it.id);
                        }
                    }
                }
                _ => {}
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------
// 纯逻辑: 几何与布局
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    pub fn right(&self) -> i32 {
        self.x + self.w
    }
    pub fn bottom(&self) -> i32 {
        self.y + self.h
    }
    pub fn center_y(&self) -> i32 {
        self.y + self.h / 2
    }
    pub fn contains(&self, px: i32, py: i32) -> bool {
        px >= self.x && px < self.right() && py >= self.y && py < self.bottom()
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Target {
    Card,
    Head,
    Sep,
    /// 子菜单头; 值是它在 `Model::rows` 里的下标。
    Sub(usize),
    /// 叶子项。`sub` 为 `Some(i)` 表示它是从第 `i` 行子菜单展开出来的。
    Item { id: usize, sub: Option<usize> },
}

#[derive(Clone, Debug)]
pub struct Placed {
    pub rect: Rect,
    pub target: Target,
    pub hoverable: bool,
    /// 对应 `Model::rows` 的下标(展开出来的子项记的是父 Sub 的下标)。
    pub row: usize,
    /// 子项在 `items` 里的下标。
    pub item: Option<usize>,
}

#[derive(Clone, Debug)]
pub struct Layout {
    pub width: i32,
    pub height: i32,
    pub rows: Vec<Placed>,
}

impl Layout {
    /// 命中测试只认可交互行; 死区(状态卡/分隔线/分组小标题/页脚/上下留白)返回 `None`。
    pub fn hit(&self, x: i32, y: i32) -> Option<usize> {
        self.rows
            .iter()
            .position(|p| p.hoverable && p.rect.contains(x, y))
    }

    /// 点击落点 -> 要派发的目标; `None` = 死区, 调用方必须"当没点过", 菜单保持打开。
    ///
    /// 关菜单只属于两种情况: 点到菜单窗口外面(`WM_ACTIVATE`/`WM_KILLFOCUS`),
    /// 或点中了会交出焦点的项(`handle_target` 的 keep=false)。
    /// 此前 `WM_LBUTTONUP` 把 `None` 当成"点在菜单外面"直接 DestroyWindow ——
    /// 那正是用户报的"二级子项点击三四次后, 最后一次点击无效且菜单自动消失"。
    pub fn click(&self, x: i32, y: i32) -> Option<Target> {
        self.hit(x, y)
            .and_then(|h| self.rows.get(h))
            .map(|r| r.target)
    }

    /// 可键盘选中的行下标。
    pub fn selectable(&self) -> Vec<usize> {
        self.rows
            .iter()
            .enumerate()
            .filter(|(_, p)| p.hoverable)
            .map(|(i, _)| i)
            .collect()
    }

    /// 上下键移动, 到头回绕。
    pub fn step(&self, cur: Option<usize>, dir: i32) -> Option<usize> {
        let s = self.selectable();
        if s.is_empty() {
            return None;
        }
        let next = match cur.and_then(|c| s.iter().position(|&i| i == c)) {
            None => {
                if dir >= 0 {
                    0
                } else {
                    s.len() - 1
                }
            }
            Some(p) => ((p as i32 + dir).rem_euclid(s.len() as i32)) as usize,
        };
        Some(s[next])
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Metrics {
    pub pad: i32,
    pub row_h: i32,
    pub head_h: i32,
    pub card_h: i32,
    pub sep_h: i32,
    pub width: i32,
    pub radius: i32,
    pub text: i32,
    pub small: i32,
    pub indent: i32,
}

impl Metrics {
    /// 96 DPI 下的基准尺寸, 按 `scale` 缩放。
    pub fn at(scale: f32) -> Self {
        let s = |v: i32| ((v as f32) * scale).round() as i32;
        Self {
            pad: s(8),
            row_h: s(30),
            head_h: s(26),
            card_h: s(58),
            sep_h: s(9),
            width: s(268),
            radius: s(9),
            text: s(13),
            small: s(11),
            indent: s(16),
        }
    }
}

/// 文字量宽。测试里用确定性的假实现, 运行时用 GDI。
pub trait TextMeasure {
    fn width(&self, s: &str, size_px: i32) -> i32;
}

/// 超宽就截断并加省略号。
pub fn fit(s: &str, max_w: i32, size_px: i32, measure: &dyn TextMeasure) -> String {
    if max_w <= 0 {
        return String::new();
    }
    if measure.width(s, size_px) <= max_w {
        return s.to_string();
    }
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::new();
    for i in 0..chars.len() {
        let mut probe: String = chars[..=i].iter().collect();
        probe.push('…');
        if measure.width(&probe, size_px) > max_w {
            break;
        }
        out = probe;
    }
    if out.is_empty() {
        "…".to_string()
    } else {
        out
    }
}

/// 从内容算出布局。`expanded` 是当前展开的 `Sub` 行**标签**(不是下标):
/// 模型里会按需插拔行(比如更新提示行), 下标会漂移, 用标签才能稳定指着同一个子菜单。
pub fn layout(
    model: &Model,
    expanded: Option<&str>,
    m: &Metrics,
    measure: &dyn TextMeasure,
) -> Layout {
    // 宽度做成内容自适应: 固定宽度会让"强制常亮 · 直到手动关闭"这种长标签被截断
    let mut widest = 0;
    for row in &model.rows {
        let w = match row {
            Row::Card {
                title,
                subtitle,
                badge,
                dots,
            } => {
                let mut t = measure
                    .width(title, m.text)
                    .max(measure.width(subtitle, m.small));
                if let Some(b) = badge {
                    t = t.max(measure.width(b, m.small) + m.row_h);
                }
                t + (dots.len() as i32) * 12 + 16
            }
            Row::Head(s) => measure.width(s, m.small),
            Row::Sub { label, items } => {
                let mut w = measure.width(label, m.text) + m.row_h / 2;
                for it in items {
                    w = w.max(measure.width(&it.label, m.text) + m.indent + 4);
                }
                w
            }
            Row::Item(it) => measure.width(&it.label, m.text) + 4,
            Row::Sep => 0,
        };
        widest = widest.max(w);
    }
    let width = (widest + m.pad * 2 + m.row_h + m.row_h / 2).max(m.width);

    let mut rows = Vec::new();
    let mut y = m.pad;
    let full = |y: i32, h: i32| Rect {
        x: 0,
        y,
        w: width,
        h,
    };
    for (i, row) in model.rows.iter().enumerate() {
        match row {
            Row::Card { .. } => {
                rows.push(Placed {
                    rect: full(y, m.card_h),
                    target: Target::Card,
                    hoverable: false,
                    row: i,
                    item: None,
                });
                y += m.card_h;
            }
            Row::Head(_) => {
                rows.push(Placed {
                    rect: full(y, m.head_h),
                    target: Target::Head,
                    hoverable: false,
                    row: i,
                    item: None,
                });
                y += m.head_h;
            }
            Row::Sep => {
                rows.push(Placed {
                    rect: full(y, m.sep_h),
                    target: Target::Sep,
                    hoverable: false,
                    row: i,
                    item: None,
                });
                y += m.sep_h;
            }
            Row::Item(it) => {
                rows.push(Placed {
                    rect: full(y, m.row_h),
                    target: Target::Item {
                        id: it.id,
                        sub: None,
                    },
                    hoverable: it.enabled,
                    row: i,
                    item: None,
                });
                y += m.row_h;
            }
            Row::Sub { label, items } => {
                rows.push(Placed {
                    rect: full(y, m.row_h),
                    target: Target::Sub(i),
                    hoverable: true,
                    row: i,
                    item: None,
                });
                y += m.row_h;
                if expanded == Some(label.as_str()) {
                    for (k, it) in items.iter().enumerate() {
                        rows.push(Placed {
                            rect: full(y, m.row_h),
                            target: Target::Item {
                                id: it.id,
                                sub: Some(i),
                            },
                            hoverable: it.enabled,
                            row: i,
                            item: Some(k),
                        });
                        y += m.row_h;
                    }
                }
            }
        }
    }
    y += m.pad;

    Layout {
        width,
        height: y,
        rows,
    }
}

/// 把菜单放在鼠标附近; 碰到工作区边缘就朝反方向翻, 再不行就夹紧。
pub fn place(cursor: POINT, w: i32, h: i32, work: RECT, margin: i32) -> POINT {
    let left = work.left + margin;
    let top = work.top + margin;
    let right = work.right - margin - w;
    let bottom = work.bottom - margin - h;

    let mut x = cursor.x + margin;
    if x + w > work.right - margin {
        x = cursor.x - w - margin;
    }
    if x + w > work.right - margin {
        x = right.max(left);
    }
    if x < left {
        x = left;
    }

    let mut y = cursor.y + margin;
    if y + h > work.bottom - margin {
        y = cursor.y - h - margin;
    }
    if y + h > work.bottom - margin {
        y = bottom.max(top);
    }
    if y < top {
        y = top;
    }

    POINT { x, y }
}

// ---------------------------------------------------------------------------
// 运行时: GDI 量宽
// ---------------------------------------------------------------------------

struct GdiMeasure {
    hdc: HDC,
}

impl TextMeasure for GdiMeasure {
    fn width(&self, s: &str, size_px: i32) -> i32 {
        if s.is_empty() {
            return 0;
        }
        unsafe {
            let font = make_font(size_px);
            if font.0 == 0 {
                return estimate(s, size_px);
            }
            let old = SelectObject(self.hdc, font);
            let wide: Vec<u16> = s.encode_utf16().collect();
            let mut sz = SIZE::default();
            let ok = GetTextExtentPoint32W(self.hdc, &wide, &mut sz);
            SelectObject(self.hdc, old);
            let _ = DeleteObject(font);
            if ok.as_bool() {
                sz.cx
            } else {
                estimate(s, size_px)
            }
        }
    }
}

fn estimate(s: &str, size_px: i32) -> i32 {
    (s.chars().count() as f32 * 0.55 * size_px as f32).round() as i32
}

/// `CreateFontW` 的后半段参数全用字面值, 免得在 `FONT_CHARSET`/`FONT_QUALITY` 的
/// newtype 之间来回转: 1=DEFAULT_CHARSET, 4=OUT_TT_PRECIS, 0=CLIP_DEFAULT_PRECIS,
/// 5=CLEARTYPE_QUALITY, 0=DEFAULT_PITCH|FF_DONTCARE。中文由系统字体链接回落到雅黑。
unsafe fn make_font(size_px: i32) -> HFONT {
    CreateFontW(
        -size_px, 0, 0, 0, 400, 0, 0, 0, 1, 4, 0, 5, 0,
        w!("Segoe UI"),
    )
}

// ---------------------------------------------------------------------------
// 运行时: 会话与窗口
// ---------------------------------------------------------------------------

/// tray.rs 侧要实现的东西。`activate` 返回 true = 菜单继续开着(并重新取一遍 model)。
pub trait Host {
    fn activate(&mut self, id: usize) -> bool;
    fn model(&self) -> Model;
}

struct View {
    model: Model,
    layout: Layout,
    theme: Theme,
    metrics: Metrics,
    expanded: Option<String>,
    /// 宽度下限(本次打开内只增不减)。状态卡副标题是活文本(网速/宽限期/剩余时间),
    /// 每次刷新都可能让"内容自适应宽度"来回抖; 配合 resize_to_layout 的 SWP_NOMOVE,
    /// 右边缘会缩回去 —— 靠右的点落到窗口外, 变成 WM_ACTIVATE 关菜单。冻结宽度消灭这个抖动。
    width_floor: i32,
    hover: Option<usize>,
    selected: Option<usize>,
    tracking_leave: bool,
    /// 我们已经拿到过前台激活。看门狗据此区分"用户点走了"和"从来没激活成功"。
    activated: bool,
    shown_at: Instant,
}

struct Session {
    /// 菜单自己那个窗口。绝不要拿它和托盘的消息窗口(`show()` 的 `owner` 参数)混用 ——
    /// 拿去 resize/repaint 的话动的会是不相干的窗口, 菜单本身毫无反应。
    hwnd: HWND,
    host: Box<dyn Host>,
    view: View,
    dc: HDC,
}

// 用裸指针 + `Cell` 而不是 `RefCell`: 窗口过程会被重入(嵌套消息循环), RefCell 的双重借用
// 会变成运行期 panic。窗口在 `show()` 返回前就已销毁, 所以指针在生命周期上是成立的。
thread_local! {
    static SESSION: Cell<*mut Session> = const { Cell::new(std::ptr::null_mut()) };
}

fn session_ptr() -> *mut Session {
    SESSION.with(|c| c.get())
}

/// 菜单是否正开着。`show_menu` 用它防止 `WM_TRAY` 在模态循环里重入。
pub fn is_open() -> bool {
    OPEN.load(Ordering::SeqCst)
}

/// 外部状态变了(比如更新线程查完了): 菜单开着就重新取一遍内容。
///
/// 只会由菜单自己的消息循环经 `DispatchMessageW` 调进来, 所以不会和正在进行的
/// `handle_target` 抢 `Session`。
pub fn refresh() {
    if ACTING.load(Ordering::SeqCst) {
        return;
    }
    let p = session_ptr();
    if p.is_null() {
        return;
    }
    unsafe {
        let s = &mut *p;
        let hwnd = s.hwnd;
        if hwnd.0 == 0 || !IsWindow(hwnd).as_bool() {
            return;
        }
        s.view.model = s.host.model();
        relayout(s);
        let l = s.view.layout.clone();
        resize_to_layout(hwnd, &l);
        repaint(hwnd);
    }
}

/// 弹出自绘菜单, 直到用户点选/失焦关闭才返回。
///
/// 返回值: `true` = 自绘菜单已经接管这次弹出(包括"已经开着一个"这种空操作);
/// `false` = 窗口没建出来 —— 调用方**必须回落到系统菜单**, 否则用户点了右键什么都不出现。
/// 兜底方向永远是"能用的那个", 不是"好看的那个"。
pub fn show<H: Host + 'static>(owner: HWND, host: H) -> bool {
    if OPEN.swap(true, Ordering::SeqCst) {
        // 已经开着一个: 当作已接管, 别再叠一个系统菜单上去
        return true;
    }
    struct Guard;
    impl Drop for Guard {
        fn drop(&mut self) {
            OPEN.store(false, Ordering::SeqCst);
        }
    }
    let _guard = Guard;

    unsafe {
        register_class();
        let dc = CreateCompatibleDC(HDC::default());
        let scale = dpi_scale(owner);
        let theme_mode = crate::config::Config::load_or_create().menu_theme.clone();
        let metrics = Metrics::at(scale);

        let mut session = Box::new(Session {
            hwnd: HWND::default(),
            host: Box::new(host),
            view: View {
                model: Model::new(Vec::new()),
                layout: Layout {
                    width: 0,
                    height: 0,
                    rows: Vec::new(),
                },
                theme: resolve(&theme_mode, system_dark()),
                metrics,
                expanded: None,
                width_floor: 0,
                hover: None,
                selected: None,
                tracking_leave: false,
                activated: false,
                shown_at: Instant::now(),
            },
            dc,
        });
        session.view.model = session.host.model();
        relayout(&mut session);
        let p: *mut Session = &mut *session;
        SESSION.with(|c| c.set(p));

        let hinstance = HINSTANCE(GetModuleHandleW(None).map(|h| h.0).unwrap_or(0));
        let w = session.view.layout.width;
        let h = session.view.layout.height;
        let mut cursor = POINT::default();
        let _ = GetCursorPos(&mut cursor);
        let pos = place(cursor, w, h, work_area(cursor), session.view.metrics.pad);

        let hwnd = CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
            CLASS,
            w!("stayawake"),
            WS_POPUP,
            pos.x,
            pos.y,
            w,
            h,
            owner,
            HMENU::default(),
            hinstance,
            None,
        );
        if hwnd.0 == 0 {
            SESSION.with(|c| c.set(std::ptr::null_mut()));
            let _ = DeleteDC(session.dc);
            crate::log::event("warn: 自绘菜单建窗失败, 本次回落到系统菜单");
            return false;
        }

        // DWM 圆角(Win11), 外加窗口区域兜底(老系统上没有 corner 属性)
        session.hwnd = hwnd;
        apply_round(hwnd, session.view.metrics.radius);

        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
        SetFocus(hwnd);

        // 看门狗: 自绘菜单一旦拿不到前台激活, 就再没有别的办法把它关掉
        // (WM_ACTIVATE/WM_KILLFOCUS 都不会来)。宁可 2 秒后自己消失, 也不要留一个
        // 点不掉、盖在所有窗口上的面板。
        SetTimer(hwnd, WATCHDOG_ID, 250, None);

        let mut msg = MSG::default();
        while IsWindow(hwnd).as_bool() {
            let r = GetMessageW(&mut msg, None, 0, 0);
            if r.0 <= 0 {
                // 主消息循环也拿这个队列的 WM_QUIT, 不能吞掉
                if r.0 == 0 {
                    PostQuitMessage(msg.wParam.0 as i32);
                }
                break;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        SESSION.with(|c| c.set(std::ptr::null_mut()));
        let _ = DeleteDC(session.dc);
        if IsWindow(hwnd).as_bool() {
            let _ = DestroyWindow(hwnd);
        }
    }
    true
}

unsafe fn register_class() {
    if REGISTERED.swap(true, Ordering::SeqCst) {
        return;
    }
    let hinstance = HINSTANCE(GetModuleHandleW(None).map(|h| h.0).unwrap_or(0));
    let class = WNDCLASSW {
        style: CS_DROPSHADOW,
        lpfnWndProc: Some(menu_proc),
        hInstance: hinstance,
        hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
        lpszClassName: CLASS,
        ..Default::default()
    };
    RegisterClassW(&class);
}

unsafe fn dpi_scale(hwnd: HWND) -> f32 {
    // 进程是 per-monitor-v2 感知的(见 main.rs), 所以这里拿到的是真实 DPI。
    let mut dpi = GetDpiForWindow(hwnd);
    if dpi == 0 {
        dpi = GetDpiForSystem();
    }
    if dpi == 0 {
        dpi = 96;
    }
    dpi as f32 / 96.0
}

unsafe fn work_area(pt: POINT) -> RECT {
    let mon = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
    let mut mi = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    if GetMonitorInfoW(mon, &mut mi).as_bool() && mi.rcWork.right > mi.rcWork.left {
        mi.rcWork
    } else {
        RECT {
            left: 0,
            top: 0,
            right: 1280,
            bottom: 720,
        }
    }
}

unsafe fn apply_round(hwnd: HWND, radius: i32) {
    let pref = DWMWCP_ROUND;
    let _ = DwmSetWindowAttribute(
        hwnd,
        DWMWA_WINDOW_CORNER_PREFERENCE,
        &pref as *const _ as *const c_void,
        std::mem::size_of::<i32>() as u32,
    );
    let mut rc = RECT::default();
    if GetClientRect(hwnd, &mut rc).is_ok() && rc.right > 0 && rc.bottom > 0 {
        // SetWindowRgn 接管这块区域的所有权, 不要自己删
        let rgn = CreateRoundRectRgn(0, 0, rc.right + 1, rc.bottom + 1, radius * 2, radius * 2);
        if rgn.0 != 0 {
            SetWindowRgn(hwnd, rgn, true);
        }
    }
}

fn relayout(s: &mut Session) {
    let measure = GdiMeasure { hdc: s.dc };
    // Bug C: 菜单开着的时候宽度只增不减(见 View.width_floor 的注释)。
    // 把下限灌进 metrics.width, layout() 内部所有行矩形都会跟着铺满整个宽度。
    s.view.metrics.width = s.view.metrics.width.max(s.view.width_floor);
    s.view.layout = layout(&s.view.model, s.view.expanded.as_deref(), &s.view.metrics, &measure);
    s.view.width_floor = s.view.layout.width;
    s.view.hover = None;
    s.view.tracking_leave = false;
}

unsafe fn resize_to_layout(hwnd: HWND, l: &Layout) {
    let _ = SetWindowPos(
        hwnd,
        HWND::default(),
        0,
        0,
        l.width,
        l.height,
        SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
    );
}

unsafe fn repaint(hwnd: HWND) {
    let _ = InvalidateRect(hwnd, None, false);
}

/// 诊断用: 把一行追踪写进 `%LOCALAPPDATA%\stayawake\menu_trace.log`。
/// 只用于排查"菜单莫名消失", 不影响功能; 文件不存在时会创建。
fn trace(msg: &str) {
    use std::io::Write as _;
    let dir = match std::env::var_os("LOCALAPPDATA") {
        Some(d) => d,
        None => return,
    };
    let mut path = std::path::PathBuf::from(dir);
    path.push("stayawake");
    path.push("menu_trace.log");
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let _ = writeln!(f, "{t} {msg}");
    }
}

unsafe fn close(hwnd: HWND) {
    trace("close: DestroyWindow");
    let _ = DestroyWindow(hwnd);
}

/// 关菜单的唯一入口 + 记下为什么关, 免得再靠猜。
unsafe fn close_reason(hwnd: HWND, why: &str) {
    trace(&format!("close: {why}"));
    close(hwnd);
}

// ---------------------------------------------------------------------------
// 运行时: 消息
// ---------------------------------------------------------------------------

unsafe extern "system" fn menu_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    // `ID_DETAILS` / 更新提示会弹 MessageBox, 那会开启嵌套消息循环。这期间只画, 别的都别碰:
    // 绝不能在回调期间再借一次 Session。
    if ACTING.load(Ordering::SeqCst) {
        if msg == WM_PAINT {
            paint(hwnd);
            return LRESULT(0);
        }
        return DefWindowProcW(hwnd, msg, wp, lp);
    }

    match msg {
        WM_PAINT => {
            paint(hwnd);
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            on_mouse_move(hwnd, lp);
            LRESULT(0)
        }
        WM_MOUSELEAVE => {
            let p = session_ptr();
            if !p.is_null() {
                let s = &mut *p;
                s.view.hover = None;
                s.view.tracking_leave = false;
                repaint(hwnd);
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            let p = session_ptr();
            if !p.is_null() {
                // 用点击那一刻的坐标现场命中测试, 绝不能用缓存的 `hover`。
                // 每次 `relayout` 都会把 hover 清成 None, 而"手不动地连点同一个按钮"
                // 不会再产生 WM_MOUSEMOVE —— 第二次点击的 hover 仍是 None, 会被误判成
                // "点在菜单外面"而把窗口关掉。(用户报的: 点过的按钮再点一下窗口就没了)
                let x = (lp.0 as i32 & 0xFFFF) as i16 as i32;
                let y = ((lp.0 as i32 >> 16) & 0xFFFF) as i16 as i32;
                let target = {
                    let s = &*p;
                    s.view.layout.click(x, y)
                };
                if let Some(t) = target {
                    handle_target(hwnd, t);
                } else {
                    // 死区(状态卡/分隔线/分组小标题/页脚/上下留白): 当没点过, 菜单继续开着。
                    // 绝不能在这里关窗口 —— 那正是"点了没反应, 菜单还自己消失"的元凶。
                    trace(&format!("LBUTTONUP hit=None at ({x},{y}) -> ignored"));
                    let s = &mut *p;
                    s.view.hover = None;
                    repaint(hwnd);
                }
            }
            LRESULT(0)
        }
        WM_KEYDOWN => {
            on_key(hwnd, wp);
            LRESULT(0)
        }
        WM_ACTIVATE => {
            let inactive = (wp.0 & 0xFFFF) as u32 == WA_INACTIVE;
            let p = session_ptr();
            if !p.is_null() && !inactive {
                let s = &mut *p;
                s.view.activated = true;
            }
            if inactive {
                close_reason(hwnd, "WM_ACTIVATE WA_INACTIVE");
            }
            LRESULT(0)
        }
        WM_KILLFOCUS => {
            close_reason(hwnd, "WM_KILLFOCUS");
            LRESULT(0)
        }
        WM_DESTROY => {
            let _ = KillTimer(hwnd, WATCHDOG_ID);
            LRESULT(0)
        }
        WM_SIZE => {
            // 展开子菜单会加高窗口。`SetWindowRgn` 拿的是当时那一次的尺寸, 不会跟着长,
            // 不重设的话新长出来的那几行会被裁掉 —— 手风琴看着就像"点不开"。
            let p = session_ptr();
            if !p.is_null() {
                let radius = (&*p).view.metrics.radius;
                apply_round(hwnd, radius);
            }
            LRESULT(0)
        }
        WM_TIMER => {
            on_timer(hwnd);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

/// 看门狗: `WM_ACTIVATE` 是主路径, 这里只是兜底。
///
/// 三种情况:
///   * 我们就是前台窗口 —— 正常, 记下"激活过";
///   * 曾经激活过、现在不是了 —— 说明有别的窗口抢走了焦点(可能是 `WM_ACTIVATE` 漏了,
///     也可能激活的是另一个线程的窗口), 收起来;
///   * 从来没激活成功过, 且已经过了 2 秒 —— `SetForegroundWindow` 被系统拒绝了。
///     这时键盘收不到、点外面也收不到, 继续留着只会变成一个盖住屏幕的僵尸面板。
unsafe fn on_timer(hwnd: HWND) {
    let p = session_ptr();
    if p.is_null() {
        return;
    }
    let s = &mut *p;
    if GetForegroundWindow() == hwnd {
        s.view.activated = true;
        return;
    }
    if s.view.activated || s.view.shown_at.elapsed() > std::time::Duration::from_secs(2) {
        close_reason(hwnd, "watchdog timer fg != hwnd");
    }
}

unsafe fn on_mouse_move(hwnd: HWND, lp: LPARAM) {
    let p = session_ptr();
    if p.is_null() {
        return;
    }
    let s = &mut *p;
    let x = (lp.0 as i32 & 0xFFFF) as i16 as i32;
    let y = ((lp.0 as i32 >> 16) & 0xFFFF) as i16 as i32;
    let hit = s.view.layout.hit(x, y);
    if hit != s.view.hover {
        s.view.hover = hit;
        repaint(hwnd);
    }
    if !s.view.tracking_leave {
        // 只登记一次; WM_MOUSELEAVE 到了之后要重新登记
        let mut tme = TRACKMOUSEEVENT {
            cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
            dwFlags: TME_LEAVE,
            hwndTrack: hwnd,
            dwHoverTime: 0,
        };
        if TrackMouseEvent(&mut tme).is_ok() {
            s.view.tracking_leave = true;
        }
    }
}

unsafe fn on_key(hwnd: HWND, wp: WPARAM) {
    let vk = wp.0 as u16;
    let p = session_ptr();
    if p.is_null() {
        return;
    }
    if vk == VK_ESCAPE.0 {
        close(hwnd);
        return;
    }

    let mut target = None;
    {
        let s = &mut *p;
        if vk == VK_DOWN.0 {
            s.view.selected = s.view.layout.step(s.view.selected, 1);
            repaint(hwnd);
        } else if vk == VK_UP.0 {
            s.view.selected = s.view.layout.step(s.view.selected, -1);
            repaint(hwnd);
        } else if vk == VK_RETURN.0 {
            let sel = s
                .view
                .selected
                .or_else(|| s.view.layout.step(None, 1));
            s.view.selected = sel;
            target = sel
                .and_then(|i| s.view.layout.rows.get(i))
                .map(|r| r.target);
        } else if vk == VK_RIGHT.0 || vk == VK_LEFT.0 {
            // 右: 展开; 左: 收起。光标停在子菜单头上才有意义。
            let sel = s.view.selected;
            if let Some(Target::Sub(idx)) = sel
                .and_then(|i| s.view.layout.rows.get(i))
                .map(|r| r.target)
            {
                // 展开状态用 Sub 的标签记, 模型行增删不会让下标漂移
                let label = match s.view.model.rows.get(idx) {
                    Some(Row::Sub { label, .. }) => label.clone(),
                    _ => return,
                };
                let open = vk == VK_RIGHT.0;
                if (s.view.expanded.as_deref() == Some(label.as_str())) != open {
                    s.view.expanded = if open { Some(label) } else { None };
                    relayout(s);
                    let l = s.view.layout.clone();
                    resize_to_layout(hwnd, &l);
                    repaint(hwnd);
                }
            }
        }
    }
    if let Some(t) = target {
        handle_target(hwnd, t);
    }
}

unsafe fn handle_target(hwnd: HWND, target: Target) {
    let p = session_ptr();
    if p.is_null() {
        return;
    }
    match target {
        Target::Sub(idx) => {
            let s = &mut *p;
            // 用标签而不是行下标记展开状态: 模型行(如更新提示)插拔会让下标漂移,
            // 旧代码里 expanded=Some(3) 会突然指着另一个子菜单 —— 手风琴自己跳。
            let label = match s.view.model.rows.get(idx) {
                Some(Row::Sub { label, .. }) => label.clone(),
                _ => return,
            };
            s.view.expanded = if s.view.expanded.as_deref() == Some(label.as_str()) {
                None
            } else {
                Some(label)
            };
            s.view.selected = Some(s.view.layout.rows.iter().position(|r| r.target == Target::Sub(idx)).unwrap_or(0));
            relayout(s);
            let l = s.view.layout.clone();
            resize_to_layout(hwnd, &l);
            repaint(hwnd);
        }
        Target::Item { id, .. } => {
            // 只派发现在这份模型里真实存在的命令。菜单开着的时候 `refresh()` 可能刚把模型
            // 换掉(比如更新查完了), 而 hover/selected 还指着旧行 —— 这个校验挡住过期的 id。
            if !(&*p).view.model.leaf_ids().contains(&id) {
                return;
            }
            ACTING.store(true, Ordering::SeqCst);
            let keep = {
                let s = &mut *p;
                s.host.activate(id)
            };
            ACTING.store(false, Ordering::SeqCst);
            if !IsWindow(hwnd).as_bool() {
                return;
            }
            let s = &mut *p;
            if keep {
                s.view.model = s.host.model();
                relayout(s);
                let l = s.view.layout.clone();
                resize_to_layout(hwnd, &l);
                repaint(hwnd);
            } else {
                close_reason(hwnd, "keep=false");
            }
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// 运行时: 绘制
// ---------------------------------------------------------------------------

unsafe fn paint(hwnd: HWND) {
    let mut ps = PAINTSTRUCT::default();
    let hdc = BeginPaint(hwnd, &mut ps);
    let p = session_ptr();
    if p.is_null() {
        let _ = EndPaint(hwnd, &ps);
        return;
    }
    let s = &mut *p;
    let (w, h) = (s.view.layout.width, s.view.layout.height);
    if w <= 0 || h <= 0 {
        let _ = EndPaint(hwnd, &ps);
        return;
    }

    // 双缓冲: 直接画到窗口 DC 上会闪
    let mem = CreateCompatibleDC(hdc);
    let bmp = CreateCompatibleBitmap(hdc, w, h);
    let old_bmp = SelectObject(mem, bmp);

    let bg = CreateSolidBrush(colorref(s.view.theme.bg));
    let full = RECT {
        left: 0,
        top: 0,
        right: w,
        bottom: h,
    };
    FillRect(mem, &full, bg);
    let _ = DeleteObject(bg);
    SetBkMode(mem, TRANSPARENT);

    let font = make_font(s.view.metrics.text);
    let font_small = make_font(s.view.metrics.small);
    let old_font = SelectObject(mem, font);

    for i in 0..s.view.layout.rows.len() {
        draw_row(mem, s, i, font_small);
    }

    SelectObject(mem, old_font);
    let _ = DeleteObject(font);
    let _ = DeleteObject(font_small);

    // 1px 边框: 圆角区域裁掉阴影之后, 没有描边会糊在浅色背景上
    let pen = CreatePen(PS_SOLID, 1, colorref(s.view.theme.border));
    let old_pen = SelectObject(mem, pen);
    let old_brush = SelectObject(mem, GetStockObject(NULL_BRUSH));
    let r = s.view.metrics.radius * 2;
    RoundRect(mem, 0, 0, w - 1, h - 1, r, r);
    SelectObject(mem, old_brush);
    SelectObject(mem, old_pen);
    let _ = DeleteObject(pen);

    let _ = BitBlt(hdc, 0, 0, w, h, mem, 0, 0, SRCCOPY);

    SelectObject(mem, old_bmp);
    let _ = DeleteObject(bmp);
    let _ = DeleteDC(mem);
    let _ = EndPaint(hwnd, &ps);
}

unsafe fn draw_row(mem: HDC, s: &Session, i: usize, font_small: HFONT) {
    let v = &s.view;
    let placed = &v.layout.rows[i];
    let rect = placed.rect;
    let hot = v.hover == Some(i) || v.selected == Some(i);

    // 悬停底: 内缩一点, 免得贴到面板描边
    if hot && placed.hoverable {
        let b = CreateSolidBrush(colorref(v.theme.bg_alt));
        let p = CreatePen(PS_SOLID, 1, colorref(v.theme.bg_alt));
        let ob = SelectObject(mem, b);
        let op = SelectObject(mem, p);
        let rr = (v.metrics.radius - 2).max(2);
        RoundRect(
            mem,
            rect.x + 4,
            rect.y + 1,
            rect.right() - 4,
            rect.bottom() - 1,
            rr * 2,
            rr * 2,
        );
        SelectObject(mem, ob);
        SelectObject(mem, op);
        let _ = DeleteObject(b);
        let _ = DeleteObject(p);
    }

    match placed.target {
        Target::Sep => {
            let pen = CreatePen(PS_SOLID, 1, colorref(v.theme.border));
            let old = SelectObject(mem, pen);
            MoveToEx(mem, rect.x + v.metrics.pad, rect.center_y(), None);
            LineTo(mem, rect.right() - v.metrics.pad, rect.center_y());
            SelectObject(mem, old);
            let _ = DeleteObject(pen);
        }
        Target::Card => {
            let (title, subtitle, badge, dots) = match &v.model.rows[placed.row] {
                Row::Card {
                    title,
                    subtitle,
                    badge,
                    dots,
                } => (title, subtitle, badge, dots),
                _ => return,
            };
            let mut dx = v.metrics.pad + 6;
            let dy = rect.y + (v.metrics.card_h - 8) / 2;
            for d in dots {
                let b = CreateSolidBrush(colorref(*d));
                let p = CreatePen(PS_SOLID, 0, colorref(*d));
                let ob = SelectObject(mem, b);
                let op = SelectObject(mem, p);
                Ellipse(mem, dx, dy, dx + 8, dy + 8);
                SelectObject(mem, ob);
                SelectObject(mem, op);
                let _ = DeleteObject(b);
                let _ = DeleteObject(p);
                dx += 12;
            }
            if !dots.is_empty() {
                dx += 2;
            }

            let right = rect.right() - v.metrics.pad;
            let mut badge_left = right;
            if let Some(b) = badge {
                let mut t: Vec<u16> = b.encode_utf16().collect();
                badge_left = right - (GdiMeasure { hdc: mem }).width(b, v.metrics.small) - 4;
                let mut br = RECT {
                    left: badge_left,
                    top: rect.y + 6,
                    right,
                    bottom: rect.y + 6 + v.metrics.small + 6,
                };
                SetTextColor(mem, colorref(v.theme.accent));
                let of = SelectObject(mem, font_small);
                DrawTextW(mem, &mut t, &mut br, DT_SINGLELINE | DT_VCENTER | DT_RIGHT | DT_NOPREFIX);
                SelectObject(mem, of);
            }

            let max_w = (badge_left - dx - 6).max(16);
            let measure = GdiMeasure { hdc: mem };
            let t = fit(title, max_w, v.metrics.text, &measure);
            let sub = fit(subtitle, max_w, v.metrics.small, &measure);

            let mut tw: Vec<u16> = t.encode_utf16().collect();
            let mut tr = RECT {
                left: dx,
                top: rect.y + 7,
                right: dx + max_w,
                bottom: rect.y + 7 + v.metrics.text + 6,
            };
            SetTextColor(mem, colorref(v.theme.fg));
            DrawTextW(mem, &mut tw, &mut tr, DT_SINGLELINE | DT_VCENTER | DT_LEFT | DT_NOPREFIX);

            let mut sw: Vec<u16> = sub.encode_utf16().collect();
            let mut sr = RECT {
                left: dx,
                top: rect.y + 7 + v.metrics.text + 6,
                right: dx + max_w,
                bottom: rect.y + 7 + v.metrics.text + 6 + v.metrics.small + 6,
            };
            SetTextColor(mem, colorref(v.theme.fg_dim));
            let of = SelectObject(mem, font_small);
            DrawTextW(mem, &mut sw, &mut sr, DT_SINGLELINE | DT_VCENTER | DT_LEFT | DT_NOPREFIX);
            SelectObject(mem, of);
        }
        Target::Head => {
            let label = match &v.model.rows[placed.row] {
                Row::Head(s) => s.clone(),
                _ => return,
            };
            let mut t: Vec<u16> = label.encode_utf16().collect();
            let mut r = RECT {
                left: v.metrics.pad,
                top: rect.y,
                right: rect.right() - v.metrics.pad,
                bottom: rect.bottom(),
            };
            SetTextColor(mem, colorref(v.theme.fg_dim));
            let of = SelectObject(mem, font_small);
            DrawTextW(mem, &mut t, &mut r, DT_SINGLELINE | DT_VCENTER | DT_LEFT | DT_NOPREFIX);
            SelectObject(mem, of);
        }
        Target::Sub(idx) => {
            let label = match &v.model.rows[idx] {
                Row::Sub { label, .. } => label.clone(),
                _ => return,
            };
            let mut t: Vec<u16> = label.encode_utf16().collect();
            let mut r = RECT {
                left: v.metrics.pad + 4,
                top: rect.y,
                right: rect.right() - v.metrics.pad - 18,
                bottom: rect.bottom(),
            };
            SetTextColor(mem, colorref(v.theme.fg));
            DrawTextW(mem, &mut t, &mut r, DT_SINGLELINE | DT_VCENTER | DT_LEFT | DT_NOPREFIX);

            let cx = rect.right() - v.metrics.pad - 10;
            let cy = rect.center_y();
            let open = v.expanded.as_deref() == Some(label.as_str());
            let pen = CreatePen(PS_SOLID, 2, colorref(v.theme.fg_dim));
            let old = SelectObject(mem, pen);
            if open {
                MoveToEx(mem, cx - 4, cy - 2, None);
                LineTo(mem, cx, cy + 2);
                LineTo(mem, cx + 4, cy - 2);
            } else {
                MoveToEx(mem, cx - 2, cy - 4, None);
                LineTo(mem, cx + 2, cy);
                LineTo(mem, cx - 2, cy + 4);
            }
            SelectObject(mem, old);
            let _ = DeleteObject(pen);
        }
        Target::Item { sub, .. } => {
            let it = match (sub, placed.item) {
                (Some(si), Some(k)) => match &v.model.rows[si] {
                    Row::Sub { items, .. } => match items.get(k) {
                        Some(it) => it,
                        None => return,
                    },
                    _ => return,
                },
                _ => match &v.model.rows[placed.row] {
                    Row::Item(it) => it,
                    _ => return,
                },
            };
            let indent = if sub.is_some() { v.metrics.indent } else { 0 };
            let left = v.metrics.pad + 4 + indent;

            if it.checked {
                let pen = CreatePen(PS_SOLID, 2, colorref(v.theme.accent));
                let old = SelectObject(mem, pen);
                MoveToEx(mem, left, rect.center_y(), None);
                LineTo(mem, left + 4, rect.center_y() + 4);
                LineTo(mem, left + 11, rect.center_y() - 5);
                SelectObject(mem, old);
                let _ = DeleteObject(pen);
            }

            let mut t: Vec<u16> = it.label.encode_utf16().collect();
            let mut r = RECT {
                left: left + 20,
                top: rect.y,
                right: rect.right() - v.metrics.pad,
                bottom: rect.bottom(),
            };
            let color = if !it.enabled {
                v.theme.fg_dim
            } else if it.danger {
                v.theme.danger
            } else {
                v.theme.fg
            };
            SetTextColor(mem, colorref(color));
            DrawTextW(mem, &mut t, &mut r, DT_SINGLELINE | DT_VCENTER | DT_LEFT | DT_NOPREFIX);
        }
    }
}

// ---------------------------------------------------------------------------
// 测试: 全是纯逻辑, 不碰 Win32
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeMeasure;

    impl TextMeasure for FakeMeasure {
        fn width(&self, s: &str, size_px: i32) -> i32 {
            estimate(s, size_px)
        }
    }

    fn sample() -> Model {
        Model::new(vec![
            Row::Card {
                dots: vec![(61, 123, 255), (0, 224, 0)],
                title: "stayawake".into(),
                subtitle: "插电 · 正在等待".into(),
                badge: Some("更新可用 v0.2.0".into()),
            },
            Row::Head("模式".into()),
            Row::Sub {
                label: "模式".into(),
                items: vec![
                    Item::new(110, "自动 (按活动检测)").checked(true),
                    Item::new(111, "暂停 (允许正常休眠)"),
                ],
            },
            Row::Sub {
                label: "检测条件".into(),
                items: vec![Item::new(130, "音频播放"), Item::new(131, "网络速率")],
            },
            Row::Sep,
            Row::Item(Item::new(153, "当前状态详情...")),
            Row::Item(Item::new(160, "退出").danger()),
        ])
    }

    fn m() -> Metrics {
        Metrics::at(1.0)
    }

    #[test]
    fn theme_auto_follows_system() {
        assert_eq!(resolve("auto", true), Theme::dark());
        assert_eq!(resolve("auto", false), Theme::light());
        assert_eq!(resolve("dark", false), Theme::dark());
        assert_eq!(resolve("light", true), Theme::light());
        // 大小写和空白不该改变结果
        assert_eq!(resolve(" DARK ", false), Theme::dark());
    }

    #[test]
    fn collapsed_submenu_hides_children() {
        let model = sample();
        let closed = layout(&model, None, &m(), &FakeMeasure);
        assert!(closed
            .rows
            .iter()
            .all(|r| !matches!(r.target, Target::Item { sub: Some(_), .. })));
        // 2 个子菜单头 + 「详情」+「退出」
        assert_eq!(closed.selectable().len(), 4);
    }

    #[test]
    fn layout_grows_when_a_submenu_expands() {
        let model = sample();
        let closed = layout(&model, None, &m(), &FakeMeasure);
        let open = layout(&model, Some("模式"), &m(), &FakeMeasure);
        assert_eq!(open.height, closed.height + 2 * m().row_h);
        assert_eq!(open.width, closed.width); // 宽度已经按最宽子项预留, 展开不该跳变
    }

    #[test]
    fn hit_test_maps_rows_to_targets() {
        let model = sample();
        let l = layout(&model, Some("检测条件"), &m(), &FakeMeasure);
        for (i, p) in l.rows.iter().enumerate() {
            let hit = l.hit(p.rect.x + 2, p.rect.center_y());
            if p.hoverable {
                assert_eq!(hit, Some(i), "第 {i} 行应可命中: {:?}", p.target);
            } else {
                assert_ne!(hit, Some(i), "第 {i} 行不该可命中: {:?}", p.target);
            }
        }
        // 展开出来的子项也能命中, 并且带着父下标
        let child = l
            .rows
            .iter()
            .find(|r| r.target == Target::Item { id: 130, sub: Some(3) })
            .expect("展开后应该有 音频播放");
        assert!(child.hoverable);
    }

    #[test]
    fn card_and_head_rows_are_not_clickable() {
        let model = sample();
        let l = layout(&model, None, &m(), &FakeMeasure);
        let card = l.rows.iter().find(|r| matches!(r.target, Target::Card)).unwrap();
        let head = l.rows.iter().find(|r| matches!(r.target, Target::Head)).unwrap();
        assert!(!card.hoverable);
        assert!(!head.hoverable);
        assert_eq!(l.hit(1, card.rect.center_y()), None);
        assert_eq!(l.hit(1, head.rect.center_y()), None);
    }

    #[test]
    fn keyboard_step_walks_hoverable_rows_and_wraps() {
        let model = sample();
        let l = layout(&model, None, &m(), &FakeMeasure);
        let first = l.step(None, 1).unwrap();
        let last = l.step(None, -1).unwrap();
        assert_eq!(l.step(Some(last), 1), Some(first));
        assert_eq!(l.step(Some(first), -1), Some(last));
        assert!(l.rows[first].hoverable);
    }

    /// 用户报的 bug 的回归测试: 死区上的那次点击必须"没有目标" —— 不派发, 也绝不许关菜单。
    /// 修复前 `menu_proc` 把 `None` 当成"点在菜单外面"直接 DestroyWindow,
    /// 表现就是"最后一次点击无效且菜单自动消失"。
    #[test]
    fn dead_zone_click_resolves_to_no_target() {
        let model = sample();
        let l = layout(&model, Some("检测条件"), &m(), &FakeMeasure);
        // 上下 8px 留白
        assert_eq!(l.click(l.width / 2, 0), None);
        assert_eq!(l.click(l.width / 2, l.height - 1), None);
        // 状态卡 / 分组小标题 / 分隔线: 行本身不可交互
        for p in l.rows.iter().filter(|p| !p.hoverable) {
            assert_eq!(
                l.click(p.rect.x + 2, p.rect.center_y()),
                None,
                "死区行不该派发目标: {:?}",
                p.target
            );
        }
        // 对照: 可交互行必须派发(否则又是"点了没反应")
        for p in l.rows.iter().filter(|p| p.hoverable) {
            assert!(
                l.click(p.rect.x + 2, p.rect.center_y()).is_some(),
                "可交互行必须派发目标: {:?}",
                p.target
            );
        }
    }

    #[test]
    fn place_clamps_inside_work_area() {
        let work = RECT {
            left: 0,
            top: 0,
            right: 1000,
            bottom: 800,
        };
        // 正常情况: 右下偏移
        let p = place(POINT { x: 20, y: 20 }, 200, 150, work, 8);
        assert_eq!((p.x, p.y), (28, 28));

        // 右下角: 往左上翻
        let p = place(POINT { x: 990, y: 790 }, 300, 400, work, 8);
        assert!(p.x >= 8 && p.x + 300 <= 992, "x={}", p.x);
        assert!(p.y >= 8 && p.y + 400 <= 792, "y={}", p.y);

        // 菜单比工作区还大: 夹紧到左上
        let p = place(POINT { x: 500, y: 400 }, 1200, 900, work, 8);
        assert_eq!((p.x, p.y), (8, 8));
    }

    #[test]
    fn truncate_adds_ellipsis_and_respects_width() {
        let f = FakeMeasure;
        let short = "短";
        assert_eq!(fit(short, 200, 13, &f), short);

        let long = "强制常亮 · 直到手动关闭再加一点尾巴";
        let out = fit(long, 60, 13, &f);
        assert!(out.ends_with('…'));
        assert!(f.width(&out, 13) <= 60);
        assert!(out.chars().count() < long.chars().count());

        // 连一个字符都放不下时也得给个省略号, 而不是空串
        assert_eq!(fit(long, 1, 13, &f), "…");
    }

    #[test]
    fn model_leaf_ids_lists_every_clickable_item() {
        let model = sample();
        let ids = model.leaf_ids();
        assert_eq!(ids, vec![110, 111, 130, 131, 153, 160]);

        // 界面上的可交互行 = 子菜单头 + 叶子项
        let l = layout(&model, Some("模式"), &m(), &FakeMeasure);
        let leaf_rows = l.rows.iter().filter(|r| matches!(r.target, Target::Item { .. })).count();
        assert_eq!(leaf_rows + 2, l.selectable().len());
    }

    /// Bug B 回归: 展开状态按**标签**记忆。模型行在菜单开着时可能插拔
    /// (tray.rs 的 `modern_model()` 会按更新状态在索引 1 处插入/移除提示行),
    /// 旧代码用行下标记 expanded, 提示行一出现, `Some(3)` 就从"检测条件"漂到
    /// "模式" —— 手风琴自己收起/跳组, 用户正在点的行全部移位。
    /// 新布局(哪怕插了一行)必须仍展开同一个子菜单。
    #[test]
    fn expanded_submenu_survives_model_row_insertion() {
        let model = sample();
        let before = layout(&model, Some("检测条件"), &m(), &FakeMeasure);
        assert!(before
            .rows
            .iter()
            .any(|r| r.target == Target::Item { id: 130, sub: Some(3) }));

        // 在索引 1 处插入一行 —— 检测条件的行下标从 3 变成 4
        let mut shifted = model.rows.clone();
        shifted.insert(1, Row::Item(Item::new(99, "更新可用, 点击查看")));
        let model2 = Model::new(shifted);
        let after = layout(&model2, Some("检测条件"), &m(), &FakeMeasure);
        // 标签没变 -> 子项仍然展开, 且挂在(已移动的)父行下标 4 下
        let child = after
            .rows
            .iter()
            .find(|r| r.target == Target::Item { id: 130, sub: Some(4) })
            .expect("插入一行后 检测条件 的子项必须仍然展开");
        assert!(child.hoverable);
        // 对拍: 旧的下标语义(用 3)在新模型里会指向别的行, 这正是当年的 bug
        assert!(!matches!(
            model2.rows.get(3),
            Some(Row::Sub { label, .. }) if label == "检测条件"
        ));
    }
}
