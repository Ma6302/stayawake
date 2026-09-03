// 托盘图标 + 右键菜单 + 隐藏消息窗口
//
// 图标用 GDI 运行时绘制(不需要 .ico 资源), 颜色即状态。
// 这个线程同时负责:
//   - GUID_CONSOLE_DISPLAY_STATE 通知 -> 维护"显示器是否点亮", 供 never_wake_display 用
//   - TaskbarCreated 广播 -> Explorer 重启后重新添加图标
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicI8, AtomicIsize, AtomicU32, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, OnceLock};
use std::time::{Duration, Instant};

use windows::core::{w, PCWSTR, PWSTR};
use windows::Win32::Foundation::{COLORREF, HANDLE, HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, CreatePen, CreateSolidBrush, DeleteDC, DeleteObject,
    Ellipse, LineTo, MoveToEx, SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS,
    HBRUSH, HDC, HGDIOBJ, HPEN, PS_SOLID, RedrawWindow, ScreenToClient, RDW_ERASE, RDW_FRAME,
    RDW_INVALIDATE, RDW_UPDATENOW,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::System::Power::{POWERBROADCAST_SETTING, RegisterPowerSettingNotification};
use windows::Win32::System::SystemServices::GUID_CONSOLE_DISPLAY_STATE;
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY,
    NOTIFYICONDATAW, NOTIFY_ICON_MESSAGE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CallNextHookEx, CheckMenuItem, CreateIconIndirect, CreatePopupMenu,
    CreateWindowExW, DefWindowProcW, DestroyIcon, DestroyMenu, DispatchMessageW, GetCursorPos,
    FindWindowExW, GetMenuItemID, GetMessageW, LoadCursorW, MenuItemFromPoint,
    MN_GETHMENU, SendMessageW, WindowFromPoint, MessageBoxW, PostQuitMessage,
    RegisterClassW, RegisterWindowMessageW, SetForegroundWindow, SetMenuItemInfoW,
    SetWindowsHookExW, TrackPopupMenu, TranslateMessage, UnhookWindowsHookEx, CW_USEDEFAULT,
    DEVICE_NOTIFY_WINDOW_HANDLE, HC_ACTION, HHOOK, HICON, HMENU, ICONINFO, IDC_ARROW,
    MB_ICONINFORMATION, MB_ICONWARNING, MB_OK, MENUITEMINFOW, MENU_ITEM_FLAGS, MF_BYCOMMAND, MF_CHECKED, MF_GRAYED,
    MF_SEPARATOR, MF_STRING, MF_UNCHECKED, MIIM_STRING, MOUSEHOOKSTRUCT, MSG,
    PBT_APMPOWERSTATUSCHANGE, PBT_APMRESUMEAUTOMATIC, PBT_POWERSETTINGCHANGE, TPM_LEFTALIGN,
    TPM_NONOTIFY, TPM_RETURNCMD, TPM_RIGHTBUTTON, WH_MOUSE, WINDOW_STYLE, WM_APP, WM_COMMAND,
    WM_CONTEXTMENU, WM_DESTROY, WM_LBUTTONDBLCLK, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE,
    WM_NCPAINT, WM_POWERBROADCAST,
    WM_RBUTTONUP, WNDCLASSW, WS_EX_TOOLWINDOW,
};

use crate::config::{self, Config};
use crate::engine::Mode;
use crate::power::Held;
use crate::{log, Shared, WM_STATE_CHANGED};

/// 托盘图标回调消息
const WM_TRAY: u32 = WM_APP + 2;
const TRAY_UID: u32 = 1;

static SHARED: OnceLock<Arc<Shared>> = OnceLock::new();
static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);

// 菜单项 ID
const ID_TITLE: usize = 100;
const ID_AUTO: usize = 110;
const ID_PAUSE: usize = 111;
const ID_FORCE_30M: usize = 120;
const ID_FORCE_1H: usize = 121;
const ID_FORCE_3H: usize = 122;
const ID_FORCE_INF: usize = 123;
const ID_DET_AUDIO: usize = 130;
const ID_DET_NET: usize = 131;
const ID_DET_PROC: usize = 132;
const ID_DET_DL: usize = 133;
const ID_DET_HINT: usize = 134;
const ID_POLICY_AC: usize = 140;
const ID_POLICY_DC: usize = 141;
const ID_AUTOSTART: usize = 142;
const ID_OPEN_LOG: usize = 150;
const ID_OPEN_CFG: usize = 151;
const ID_RELOAD: usize = 152;
const ID_DETAILS: usize = 153;
const ID_EXIT: usize = 160;

pub fn run(shared: Arc<Shared>, hwnd_tx: mpsc::Sender<HWND>) {
    let _ = SHARED.set(shared);
    TASKBAR_CREATED.store(
        unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) },
        Ordering::SeqCst,
    );
    // 预热自启状态缓存: 这两个子进程要跑数百 ms, 放在这里比第一次开菜单时跑好
    std::thread::spawn(|| {
        AUTOSTART_CACHE.store(crate::autostart::is_installed() as i8, Ordering::SeqCst);
    });

    unsafe {
        let hinstance = HINSTANCE(GetModuleHandleW(None).unwrap().0);
        let class = WNDCLASSW {
            lpfnWndProc: Some(wnd_proc),
            hInstance: hinstance,
            lpszClassName: w!("stayawake_msgwnd"),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            ..Default::default()
        };
        RegisterClassW(&class);

        // 消息专用窗口: 不可见, 只用来收消息
        let hwnd = CreateWindowExW(
            WS_EX_TOOLWINDOW,
            w!("stayawake_msgwnd"),
            w!("stayawake"),
            WINDOW_STYLE::default(),
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            0,
            0,
            HWND::default(),
            HMENU::default(),
            hinstance,
            None,
        );
        let _ = hwnd_tx.send(hwnd);

        // 跟踪显示器真实开关状态。
        // 注意 flags 必须是 DEVICE_NOTIFY_WINDOW_HANDLE(=0); 误传 2(DEVICE_NOTIFY_CALLBACK)
        // 会让系统把 HWND 当函数指针调用, 直接 0xC0000005。
        let notify = RegisterPowerSettingNotification(
            HANDLE(hwnd.0),
            &GUID_CONSOLE_DISPLAY_STATE,
            DEVICE_NOTIFY_WINDOW_HANDLE.0,
        );
        if notify.is_err() {
            log::event("warn: RegisterPowerSettingNotification failed, display state unavailable");
        }

        add_icon(hwnd);

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        remove_icon(hwnd);
    }
}

fn shared() -> &'static Arc<Shared> {
    SHARED.get().expect("SHARED not initialized")
}

// ───────────────────────────── 托盘图标 ─────────────────────────────

#[derive(Clone, Copy, PartialEq)]
enum Look {
    Idle,          // 灰
    System,        // 琥珀: 仅防睡
    SystemDisplay, // 蓝: 保持屏幕
    Force,         // 蓝 + 绿点
    Paused,        // 红 + 白斜杠
}

fn current_look() -> Look {
    let s = shared();
    // 两个锁分开取, 不嵌套 —— 避免建立隐式锁序(match 的临时值会活到整个 match 结束)
    let mode = *s.mode.lock().unwrap();
    match mode {
        Mode::Paused => Look::Paused,
        Mode::Force(_) => Look::Force,
        Mode::Auto => {
            let held = s.snapshot.lock().unwrap().as_ref().map(|x| x.held);
            match held {
                Some(Held::System) => Look::System,
                Some(Held::SystemDisplay) => Look::SystemDisplay,
                _ => Look::Idle,
            }
        }
    }
}

fn current_tip() -> String {
    let s = shared();
    let desc = match s.snapshot.lock().unwrap().as_ref() {
        Some(snap) => snap.describe(),
        None => "启动中...".to_string(),
    };
    let mut tip = format!("stayawake — {}", desc);
    // szTip 上限 128 wchar, 留余量按字符截断
    if tip.chars().count() > 120 {
        tip = tip.chars().take(119).collect::<String>() + "…";
    }
    tip
}

fn base_nid(hwnd: HWND) -> NOTIFYICONDATAW {
    let mut nid: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
    nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    nid.hWnd = hwnd;
    nid.uID = TRAY_UID;
    nid.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
    nid.uCallbackMessage = WM_TRAY;
    nid
}

fn fill_tip(nid: &mut NOTIFYICONDATAW, text: &str) {
    for (i, c) in text.encode_utf16().take(nid.szTip.len() - 1).enumerate() {
        nid.szTip[i] = c;
    }
}

fn add_icon(hwnd: HWND) {
    notify(hwnd, NIM_ADD);
}

fn refresh_icon(hwnd: HWND) {
    notify(hwnd, NIM_MODIFY);
}

/// 更新托盘项。图标绘制失败(GDI 耗尽)时只更新 tooltip, 保留旧图标 ——
/// 总比整个进程 abort 好。
fn notify(hwnd: HWND, msg: NOTIFY_ICON_MESSAGE) {
    unsafe {
        let mut nid = base_nid(hwnd);
        let icon = draw_icon(current_look());
        match icon {
            Some(h) => nid.hIcon = h,
            None => nid.uFlags &= !NIF_ICON,
        }
        fill_tip(&mut nid, &current_tip());
        let _ = Shell_NotifyIconW(msg, &nid);
        if let Some(h) = icon {
            let _ = DestroyIcon(h);
        }
    }
}

fn remove_icon(hwnd: HWND) {
    unsafe {
        let nid = base_nid(hwnd);
        let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
    }
}

/// 32x32 32bpp DIB 上画个实心圆, 颜色即状态。掩码位图必须与颜色位图同尺寸,
/// 传 1x1 会让 CreateIconIndirect 返回 E_INVALIDARG。
///
/// 全程用 `?` 而非 `expect`: 这个函数的失败原因恰恰是 GDI 句柄耗尽,
/// 若在中途 panic 会漏掉已创建的对象 -> 下次更容易失败 -> 泄漏自我强化。
/// 而且 panic 会跨 `extern "system"` 的 wnd_proc 边界导致整个进程 abort。
/// 返回 None 时调用方降级为"不更新图标", 托盘保留上一个图标。
fn draw_icon(look: Look) -> Option<HICON> {
    const SIZE: i32 = 32;
    unsafe {
        let hdc: HDC = CreateCompatibleDC(None);
        if hdc.is_invalid() {
            return None;
        }
        // 任何一步失败都要把已经拿到的对象还回去
        let cleanup = |bmps: &[HGDIOBJ]| {
            for b in bmps {
                let _ = DeleteObject(*b);
            }
            let _ = DeleteDC(hdc);
        };

        let mut bmi: BITMAPINFO = std::mem::zeroed();
        bmi.bmiHeader = BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: SIZE,
            biHeight: SIZE,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        };
        let mut bits: *mut c_void = std::ptr::null_mut();
        let Ok(color_bmp) = CreateDIBSection(hdc, &bmi, DIB_RGB_COLORS, &mut bits, None, 0) else {
            cleanup(&[]);
            return None;
        };
        if bits.is_null() {
            cleanup(&[HGDIOBJ(color_bmp.0)]);
            return None;
        }
        let old_bmp = SelectObject(hdc, color_bmp);

        // 全透明底 (alpha = 0)
        std::slice::from_raw_parts_mut(bits as *mut u32, (SIZE * SIZE) as usize).fill(0);

        let rgb = match look {
            Look::Idle => 0x00A0A0A0,
            Look::System => 0x0000B3FF,          // 琥珀 (GDI 是 BGR)
            Look::SystemDisplay | Look::Force => 0x00FF7B3D, // 蓝
            Look::Paused => 0x003333CC,          // 红
        };
        let brush: HBRUSH = CreateSolidBrush(COLORREF(rgb));
        let old_brush = SelectObject(hdc, brush);
        let (c, r) = (SIZE / 2, SIZE * 10 / 32);
        let _ = Ellipse(hdc, c - r, c - r, c + r, c + r);
        SelectObject(hdc, old_brush);
        let _ = DeleteObject(HGDIOBJ(brush.0));

        if look == Look::Force {
            let dot: HBRUSH = CreateSolidBrush(COLORREF(0x0000E000));
            let old = SelectObject(hdc, dot);
            let _ = Ellipse(hdc, c + r - 6, c - r, c + r + 2, c - r + 8);
            SelectObject(hdc, old);
            let _ = DeleteObject(HGDIOBJ(dot.0));
        }
        if look == Look::Paused {
            let pen: HPEN = CreatePen(PS_SOLID, 4, COLORREF(0x00FFFFFF));
            let old = SelectObject(hdc, pen);
            let _ = MoveToEx(hdc, 7, 25, None);
            let _ = LineTo(hdc, 25, 7);
            SelectObject(hdc, old);
            let _ = DeleteObject(HGDIOBJ(pen.0));
        }
        SelectObject(hdc, old_bmp);

        // 掩码: 同尺寸单色位图, 内容全 0 -> 由颜色位图的 alpha 决定形状
        let mut mask_bmi: BITMAPINFO = std::mem::zeroed();
        mask_bmi.bmiHeader = BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: SIZE,
            biHeight: SIZE,
            biPlanes: 1,
            biBitCount: 1,
            ..Default::default()
        };
        let mut mask_bits: *mut c_void = std::ptr::null_mut();
        let Ok(mask_bmp) =
            CreateDIBSection(hdc, &mask_bmi, DIB_RGB_COLORS, &mut mask_bits, None, 0)
        else {
            cleanup(&[HGDIOBJ(color_bmp.0)]);
            return None;
        };

        let info = ICONINFO {
            fIcon: true.into(),
            xHotspot: 0,
            yHotspot: 0,
            hbmMask: mask_bmp,
            hbmColor: color_bmp,
        };
        // CreateIconIndirect 会复制位图, 所以之后删掉它们是对的
        let icon = CreateIconIndirect(&info).ok();
        cleanup(&[HGDIOBJ(mask_bmp.0), HGDIOBJ(color_bmp.0)]);
        icon
    }
}

// ───────────────────────────── 菜单 ─────────────────────────────

/// 选中该项后菜单是否保持打开。
/// 只有"会把焦点交出去"的项才关闭 —— 其余(模式、各种开关)可以连续点。
fn keeps_menu_open(id: usize) -> bool {
    !matches!(id, ID_EXIT | ID_OPEN_LOG | ID_OPEN_CFG | ID_DETAILS)
}

/// 菜单打开期间的上下文, 供鼠标钩子就地更新用
static MENU_HANDLE: AtomicIsize = AtomicIsize::new(0);
static MENU_WINDOW: AtomicIsize = AtomicIsize::new(0);
/// 上次选的是哪个"强制常亮"时长, 用于把勾画在正确的那一项上
static FORCE_PICK: AtomicUsize = AtomicUsize::new(0);
/// autostart 查询要跑 schtasks + reg 两个子进程(数百 ms), 绝不能在
/// UI 线程/鼠标钩子里同步做 -> 全程走缓存。
/// (-1 未知, 0 否, 1 是); 启动时由后台线程预热, 只有我们自己会改它。
static AUTOSTART_CACHE: AtomicI8 = AtomicI8::new(-1);

/// 读缓存。未知时返回 false 并触发一次后台查询 —— 绝不阻塞调用方。
fn autostart_installed() -> bool {
    match AUTOSTART_CACHE.load(Ordering::SeqCst) {
        1 => true,
        0 => false,
        _ => {
            // 未知: 后台查一次, 本次先按"未安装"显示。下次开菜单就是准的。
            std::thread::spawn(|| {
                AUTOSTART_CACHE.store(crate::autostart::is_installed() as i8, Ordering::SeqCst);
            });
            false
        }
    }
}

/// 顶部状态行。用**实时** mode 而非快照里的, 这样点完立刻能看到变化
/// (worker 虽然会被立即踢醒, 但快照更新总比这里晚一点)
fn menu_title() -> String {
    let s = shared();
    let mode = *s.mode.lock().unwrap();
    match s.snapshot.lock().unwrap().clone() {
        Some(mut snap) => {
            snap.mode = mode;
            snap.describe()
        }
        None => "启动中...".to_string(),
    }
}

fn build_menu() -> HMENU {
    let cfg = Config::load_or_create();
    let mode = *shared().mode.lock().unwrap();
    let forced = matches!(mode, Mode::Force(_));
    let force_pick = FORCE_PICK.load(Ordering::SeqCst);

    unsafe {
        let menu = CreatePopupMenu().expect("CreatePopupMenu");
        let check = |on: bool| if on { MF_CHECKED } else { MENU_ITEM_FLAGS(0) };
        let mut title_w: Vec<u16> = menu_title().encode_utf16().collect();
        title_w.push(0);

        let _ = AppendMenuW(menu, MF_STRING | MF_GRAYED, ID_TITLE, PCWSTR(title_w.as_ptr()));
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());

        let _ = AppendMenuW(menu, MF_STRING | check(mode == Mode::Auto), ID_AUTO, w!("自动 (按活动检测)"));
        let _ = AppendMenuW(menu, MF_STRING | check(mode == Mode::Paused), ID_PAUSE, w!("暂停 (允许正常休眠)"));
        for (id, label) in FORCE_ITEMS {
            let on = forced && force_pick == id;
            let _ = AppendMenuW(menu, MF_STRING | check(on), id, label);
        }
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());

        let _ = AppendMenuW(menu, MF_STRING | check(cfg.audio_enabled), ID_DET_AUDIO, w!("检测: 音频播放"));
        let _ = AppendMenuW(menu, MF_STRING | check(cfg.net_enabled), ID_DET_NET, w!("检测: 网络速率"));
        let _ = AppendMenuW(menu, MF_STRING | check(cfg.proc_enabled), ID_DET_PROC, w!("检测: 进程 CPU"));
        let _ = AppendMenuW(menu, MF_STRING | check(cfg.dl_enabled), ID_DET_DL, w!("检测: 下载器"));
        let _ = AppendMenuW(menu, MF_STRING | check(cfg.hint_enabled), ID_DET_HINT, w!("检测: 外部提示文件"));
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());

        let _ = AppendMenuW(menu, MF_STRING | check(cfg.policy_ac == "display"), ID_POLICY_AC, w!("插电时保持屏幕常亮"));
        let _ = AppendMenuW(menu, MF_STRING | check(cfg.policy_dc == "display"), ID_POLICY_DC, w!("电池时保持屏幕常亮"));
        let _ = AppendMenuW(menu, MF_STRING | check(autostart_installed()), ID_AUTOSTART, w!("开机自启"));
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());

        let _ = AppendMenuW(menu, MF_STRING, ID_DETAILS, w!("当前状态详情..."));
        let _ = AppendMenuW(menu, MF_STRING, ID_OPEN_LOG, w!("打开日志"));
        let _ = AppendMenuW(menu, MF_STRING, ID_OPEN_CFG, w!("打开配置文件"));
        let _ = AppendMenuW(menu, MF_STRING, ID_RELOAD, w!("重新加载配置"));
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        let _ = AppendMenuW(menu, MF_STRING, ID_EXIT, w!("退出"));
        menu
    }
}

const FORCE_ITEMS: [(usize, PCWSTR); 4] = [
    (ID_FORCE_30M, w!("强制常亮 · 30 分钟")),
    (ID_FORCE_1H, w!("强制常亮 · 1 小时")),
    (ID_FORCE_3H, w!("强制常亮 · 3 小时")),
    (ID_FORCE_INF, w!("强制常亮 · 直到手动关闭")),
];

/// 找到承载指定 HMENU 的那个 `#32768` 菜单窗口。
/// 菜单窗口不是 owner 的子窗口, 只能全局遍历顶层窗口, 用 MN_GETHMENU 问它挂的是哪个菜单。
fn find_menu_window(menu: HMENU) -> Option<HWND> {
    unsafe {
        let mut prev = HWND::default();
        for _ in 0..32 {
            let w = FindWindowExW(HWND::default(), prev, w!("#32768"), PCWSTR::null());
            if w.0 == 0 {
                return None;
            }
            let hm = SendMessageW(w, MN_GETHMENU, WPARAM(0), LPARAM(0));
            if hm.0 == menu.0 {
                return Some(w);
            }
            prev = w;
        }
        None
    }
}

/// 就地刷新已显示菜单的勾选与标题, 不销毁不重建 —— 所以不会闪。
/// Windows 不会因为 CheckMenuItem 自动重绘, 必须显式 RedrawWindow。
fn sync_menu_state() {
    let handle = MENU_HANDLE.load(Ordering::SeqCst);
    if handle == 0 {
        return;
    }
    let menu = HMENU(handle);
    let cfg = Config::load_or_create();
    let mode = *shared().mode.lock().unwrap();
    let forced = matches!(mode, Mode::Force(_));
    let force_pick = FORCE_PICK.load(Ordering::SeqCst);

    unsafe {
        let set = |id: usize, on: bool| {
            let flag = MF_BYCOMMAND | if on { MF_CHECKED } else { MF_UNCHECKED };
            CheckMenuItem(menu, id as u32, flag.0);
        };
        set(ID_AUTO, mode == Mode::Auto);
        set(ID_PAUSE, mode == Mode::Paused);
        for (id, _) in FORCE_ITEMS {
            set(id, forced && force_pick == id);
        }
        set(ID_DET_AUDIO, cfg.audio_enabled);
        set(ID_DET_NET, cfg.net_enabled);
        set(ID_DET_PROC, cfg.proc_enabled);
        set(ID_DET_DL, cfg.dl_enabled);
        set(ID_DET_HINT, cfg.hint_enabled);
        set(ID_POLICY_AC, cfg.policy_ac == "display");
        set(ID_POLICY_DC, cfg.policy_dc == "display");
        set(ID_AUTOSTART, autostart_installed());

        // 标题文字也可能变(模式、原因、倒计时)
        let mut title: Vec<u16> = menu_title().encode_utf16().collect();
        title.push(0);
        let mii = MENUITEMINFOW {
            cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
            fMask: MIIM_STRING,
            dwTypeData: PWSTR(title.as_mut_ptr()),
            ..Default::default()
        };
        let _ = SetMenuItemInfoW(menu, ID_TITLE as u32, false, &mii);

        // 菜单窗口句柄可能还没拿到(第一次点击前), 兜底再找一次
        let mut win = MENU_WINDOW.load(Ordering::SeqCst);
        if win == 0 {
            if let Some(w) = find_menu_window(menu) {
                win = w.0;
                MENU_WINDOW.store(win, Ordering::SeqCst);
            }
        }
        if win == 0 {
            return;
        }
        let hwnd = HWND(win);

        // 只 RedrawWindow 的话, 高亮项(鼠标正悬停的那一条)不会重画自己的勾:
        // 菜单绘制走的是"当前热点项单独一次 owner-draw", 不受整窗失效影响。
        // 让菜单自己重新计算一次尺寸与热点项, 它就会把所有项(含高亮项)重绘。
        let _ = SendMessageW(hwnd, WM_NCPAINT, WPARAM(1), LPARAM(0));
        let _ = RedrawWindow(
            hwnd,
            None,
            None,
            RDW_INVALIDATE | RDW_ERASE | RDW_UPDATENOW | RDW_FRAME,
        );

        // 用一次"鼠标移到当前位置"的假动作强制菜单刷新热点项。
        // 菜单窗口的 WM_MOUSEMOVE 处理会重绘旧热点项与新热点项 —— 即使坐标没变,
        // 它也会走一遍完整的 item 重绘路径, 从而带上刚改过的勾选状态。
        let mut cursor = POINT::default();
        if GetCursorPos(&mut cursor).is_ok() {
            let mut client = cursor;
            if ScreenToClient(hwnd, &mut client).as_bool() {
                let packed = ((client.y as u32 as isize) << 16) | (client.x as u32 as isize & 0xFFFF);
                let _ = SendMessageW(hwnd, WM_MOUSEMOVE, WPARAM(0), LPARAM(packed));
            }
        }
    }
}

/// 菜单期间的鼠标钩子: 点在"可连续操作"的项上时, 就地执行并吞掉这次点击,
/// 菜单因此完全不知道被点过, 保持打开且不重绘整个窗口 —— 没有闪烁。
///
/// 这是 Win32 下让标准菜单项不关闭菜单的唯一办法(没有对应的样式或标志)。
unsafe extern "system" fn mouse_hook(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let hook = HHOOK(MENU_HOOK.load(Ordering::SeqCst));
    let pass = |_| CallNextHookEx(hook, code, wp, lp);

    if code != HC_ACTION as i32 {
        return pass(());
    }
    let msg = wp.0 as u32;
    if msg != WM_LBUTTONDOWN && msg != WM_LBUTTONUP {
        return pass(());
    }
    let info = lp.0 as *const MOUSEHOOKSTRUCT;
    if info.is_null() {
        return pass(());
    }
    let handle = MENU_HANDLE.load(Ordering::SeqCst);
    if handle == 0 {
        return pass(());
    }
    let menu = HMENU(handle);
    let pt = (*info).pt;

    // 钩子给的 hwnd 是 owner(我们的消息窗口), 不是菜单窗口 —— 实测确认。
    // 菜单窗口只能靠 WindowFromPoint 或全局找 #32768 拿到。
    let mw = WindowFromPoint(pt);
    if mw.0 != 0 {
        MENU_WINDOW.store(mw.0, Ordering::SeqCst);
    } else if MENU_WINDOW.load(Ordering::SeqCst) == 0 {
        if let Some(w) = find_menu_window(menu) {
            MENU_WINDOW.store(w.0, Ordering::SeqCst);
        }
    }

    // < 0 表示没落在任何菜单项上(边框/菜单之外) -> 放行, 让菜单正常收起
    let pos = MenuItemFromPoint(None, menu, pt);
    if pos < 0 {
        return pass(());
    }
    let id = GetMenuItemID(menu, pos) as usize;
    // 分隔条返回 0; 标题行是 disabled, 两者都当"什么也不做"处理并吞掉
    if id != 0 && !keeps_menu_open(id) {
        return pass(()); // 需要关闭菜单的项走原生路径
    }

    // 按下时只吞掉, 抬起时才执行 —— 与菜单原本的行为一致
    if msg == WM_LBUTTONUP && id != 0 && id != ID_TITLE {
        let owner = HWND(MENU_OWNER.load(Ordering::SeqCst));
        handle_command(owner, id);
        sync_menu_state();
        refresh_icon(owner);
    }
    LRESULT(1) // 非 0 = 丢弃该消息, 菜单永远不会收到
}

static MENU_HOOK: AtomicIsize = AtomicIsize::new(0);
static MENU_OWNER: AtomicIsize = AtomicIsize::new(0);

/// 用 TPM_RETURNCMD 取返回值。可连续操作的项由鼠标钩子就地处理(菜单不关闭),
/// 只有需要交出焦点的项才会让 TrackPopupMenu 返回。
fn show_menu(hwnd: HWND) {
    // TrackPopupMenu 内部跑自己的消息循环, 期间仍会派发消息 -> 防重入
    static OPEN: AtomicBool = AtomicBool::new(false);
    if OPEN.swap(true, Ordering::SeqCst) {
        return;
    }

    let mut pt = POINT::default();
    let picked = unsafe {
        let _ = GetCursorPos(&mut pt);
        let menu = build_menu();
        MENU_HANDLE.store(menu.0, Ordering::SeqCst);
        MENU_OWNER.store(hwnd.0, Ordering::SeqCst);
        MENU_WINDOW.store(0, Ordering::SeqCst);

        let hook = SetWindowsHookExW(WH_MOUSE, Some(mouse_hook), None, GetCurrentThreadId()).ok();
        MENU_HOOK.store(hook.map(|h| h.0).unwrap_or(0), Ordering::SeqCst);

        // 必须置前台, 否则点菜单外部不会自动收起
        let _ = SetForegroundWindow(hwnd);
        let r = TrackPopupMenu(
            menu,
            TPM_LEFTALIGN | TPM_RIGHTBUTTON | TPM_RETURNCMD | TPM_NONOTIFY,
            pt.x,
            pt.y,
            0,
            hwnd,
            None,
        );

        if let Some(h) = hook {
            let _ = UnhookWindowsHookEx(h);
        }
        MENU_HOOK.store(0, Ordering::SeqCst);
        MENU_HANDLE.store(0, Ordering::SeqCst);
        MENU_WINDOW.store(0, Ordering::SeqCst);
        let _ = DestroyMenu(menu);
        r.0 as usize
    };

    // 0 = 点了菜单外部或按了 Esc。非 0 只可能是需要关闭菜单的那几项,
    // 或键盘 Enter 选中的任意项(键盘路径不走鼠标钩子)
    if picked != 0 {
        handle_command(hwnd, picked);
        refresh_icon(hwnd);
    }
    OPEN.store(false, Ordering::SeqCst);
}

fn handle_command(hwnd: HWND, id: usize) {
    let cfg = Config::load_or_create();
    let force = |secs: u64| {
        FORCE_PICK.store(id, Ordering::SeqCst);
        set_mode(Mode::Force(Instant::now() + Duration::from_secs(secs)));
    };
    match id {
        ID_AUTO => {
            FORCE_PICK.store(0, Ordering::SeqCst);
            set_mode(Mode::Auto);
        }
        ID_PAUSE => {
            FORCE_PICK.store(0, Ordering::SeqCst);
            set_mode(Mode::Paused);
        }
        ID_FORCE_30M => force(30 * 60),
        ID_FORCE_1H => force(3600),
        ID_FORCE_3H => force(3 * 3600),
        ID_FORCE_INF => force(3650 * 24 * 3600),
        ID_DET_AUDIO => toggle("audio_enabled", cfg.audio_enabled),
        ID_DET_NET => toggle("net_enabled", cfg.net_enabled),
        ID_DET_PROC => toggle("proc_enabled", cfg.proc_enabled),
        ID_DET_DL => toggle("dl_enabled", cfg.dl_enabled),
        ID_DET_HINT => toggle("hint_enabled", cfg.hint_enabled),
        ID_POLICY_AC => toggle_policy("policy_ac", &cfg.policy_ac),
        ID_POLICY_DC => toggle_policy("policy_dc", &cfg.policy_dc),
        ID_AUTOSTART => {
            // schtasks/reg 子进程要跑数百 ms 到数秒。这里可能在 WH_MOUSE 钩子里
            // (菜单打开期间), 同步执行会冻结整个线程的输入队列。
            //
            // 做法: 立刻把缓存乐观地翻转(勾选马上响应), 真正的操作丢到后台线程,
            // 完成后用实际结果校正缓存。这样 sync_menu_state 也不会再去跑子进程。
            let known = AUTOSTART_CACHE.load(Ordering::SeqCst);
            let installing = known != 1; // -1(未知) 视作未安装 -> 本次是安装
            AUTOSTART_CACHE.store(installing as i8, Ordering::SeqCst);
            std::thread::spawn(move || {
                let msg = if installing {
                    crate::autostart::install()
                } else {
                    crate::autostart::uninstall()
                };
                log::event(&msg);
                // 用真实状态校正乐观值(操作可能失败)
                AUTOSTART_CACHE.store(crate::autostart::is_installed() as i8, Ordering::SeqCst);
            });
        }
        ID_OPEN_LOG => log::open_in_editor(&config::log_path()),
        ID_OPEN_CFG => log::open_in_editor(&config::config_path()),
        ID_RELOAD => request_reload(),
        ID_DETAILS => show_details(hwnd),
        ID_EXIT => unsafe { PostQuitMessage(0) },
        _ => {}
    }
}

fn set_mode(mode: Mode) {
    let s = shared();
    *s.mode.lock().unwrap() = mode;
    s.wake_worker();
}

fn toggle(key: &str, current: bool) {
    save_setting(key, if current { "false" } else { "true" });
}

fn toggle_policy(key: &str, current: &str) {
    save_setting(key, if current == "display" { "system" } else { "display" });
}

/// 写配置项并让 worker 重载。落盘失败要记日志 + 弹窗:
/// 否则勾看起来变了、下次加载又变回去, 用户无从知晓。
fn save_setting(key: &str, value: &str) {
    let mut cfg = Config::load_or_create();
    match cfg.set_and_save(key, value) {
        Ok(()) => request_reload(),
        Err(e) => {
            let msg = format!("无法保存配置项 {} = {}\r\n\r\n{}", key, value, e);
            log::event(&format!("warn: set_and_save({}) 失败: {}", key, e));
            let mut w: Vec<u16> = msg.encode_utf16().collect();
            w.push(0);
            unsafe {
                MessageBoxW(
                    HWND::default(),
                    PCWSTR(w.as_ptr()),
                    w!("stayawake"),
                    MB_OK | MB_ICONWARNING,
                );
            }
        }
    }
}

fn request_reload() {
    let s = shared();
    s.reload.store(true, Ordering::SeqCst);
    s.wake_worker();
}

fn show_details(hwnd: HWND) {
    let s = shared();
    let cfg = Config::load_or_create();
    let snap = s.snapshot.lock().unwrap().clone();

    let mut lines = vec!["stayawake 当前状态".to_string(), String::new()];
    match snap {
        Some(snap) => {
            lines.push(format!(
                "供电: {}      显示器: {}",
                if snap.ac { "AC (插电)" } else { "DC (电池)" },
                if snap.display_on { "开" } else { "关" }
            ));
            lines.push(format!("持有: {}", snap.held.label()));
            lines.push(format!("模式: {:?}", snap.mode));
            lines.push(format!(
                "原因: {}",
                if snap.reasons.is_empty() {
                    if snap.in_grace { "(宽限期内)".to_string() } else { "(无)".to_string() }
                } else {
                    snap.reasons.join("\r\n      ")
                }
            ));
        }
        None => lines.push("(worker 尚未完成首轮检测)".to_string()),
    }
    lines.push(String::new());
    lines.push(format!("轮询 {}s   宽限 {}s", cfg.poll_interval_secs, cfg.grace_secs));
    lines.push(format!(
        "策略  插电={}  电池={}  never_wake_display={}",
        cfg.policy_ac, cfg.policy_dc, cfg.never_wake_display
    ));
    lines.push(format!(
        "检测  音频{} 网络{} 进程{} 下载器{} 提示{}",
        on(cfg.audio_enabled),
        on(cfg.net_enabled),
        on(cfg.proc_enabled),
        on(cfg.dl_enabled),
        on(cfg.hint_enabled)
    ));
    lines.push(String::new());
    lines.push("完整读数请运行:  stayawake.exe --status".to_string());

    let mut text: Vec<u16> = lines.join("\r\n").encode_utf16().collect();
    text.push(0);
    unsafe {
        MessageBoxW(hwnd, PCWSTR(text.as_ptr()), w!("stayawake"), MB_OK | MB_ICONINFORMATION);
    }
}

fn on(b: bool) -> &'static str {
    if b { "✓" } else { "✗" }
}

// ───────────────────────────── 窗口过程 ─────────────────────────────

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if msg == TASKBAR_CREATED.load(Ordering::SeqCst) && msg != 0 {
        add_icon(hwnd); // Explorer 重启, 重新添加
        return LRESULT(0);
    }
    match msg {
        WM_TRAY => {
            match lp.0 as u32 {
                // 左键单击: 自动 <-> 暂停 快速切换
                WM_LBUTTONUP => {
                    let cur = *shared().mode.lock().unwrap();
                    set_mode(if cur == Mode::Auto { Mode::Paused } else { Mode::Auto });
                    refresh_icon(hwnd);
                }
                WM_LBUTTONDBLCLK => show_details(hwnd),
                WM_RBUTTONUP | WM_CONTEXTMENU => show_menu(hwnd),
                _ => {}
            }
            LRESULT(0)
        }

        // 菜单本身走 TPM_RETURNCMD, 不会到这里。
        // 保留是为了外部 SendMessage(WM_COMMAND, id) 的可脚本化控制。
        WM_COMMAND => {
            handle_command(hwnd, wp.0 & 0xFFFF);
            refresh_icon(hwnd);
            LRESULT(0)
        }

        WM_STATE_CHANGED => {
            refresh_icon(hwnd);
            LRESULT(0)
        }

        WM_POWERBROADCAST => {
            let s = shared();
            match wp.0 as u32 {
                // 显示器开关: never_wake_display 依赖这个真值
                PBT_POWERSETTINGCHANGE => {
                    let setting = lp.0 as *const POWERBROADCAST_SETTING;
                    if !setting.is_null() && (*setting).PowerSetting == GUID_CONSOLE_DISPLAY_STATE {
                        // Data[0]: 0=off, 1=on, 2=dimmed
                        let on = (*setting).Data[0] != 0;
                        if s.display_on.swap(on, Ordering::SeqCst) != on {
                            s.wake_worker();
                        }
                    }
                }
                // 供电切换 / 从睡眠唤醒: 立即重算, 不等下个 tick
                PBT_APMPOWERSTATUSCHANGE => s.wake_worker(),
                PBT_APMRESUMEAUTOMATIC => {
                    s.display_on.store(true, Ordering::SeqCst);
                    s.wake_worker();
                }
                _ => {}
            }
            LRESULT(1)
        }

        WM_DESTROY => {
            remove_icon(hwnd);
            PostQuitMessage(0);
            LRESULT(0)
        }

        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}
