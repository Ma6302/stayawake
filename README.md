# stayawake

[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)
[![Platform: Windows](https://img.shields.io/badge/platform-Windows%2010%2F11-0078d4.svg)](#)
[![Rust](https://img.shields.io/badge/rust-2021%20edition-orange.svg)](Cargo.toml)

基于**真实活动检测**的 Windows 休眠抑制守护进程。

Windows 自带的空闲判定只认键鼠输入，不认「在放音乐」「在下载」「Agent 在跑任务」，
于是这些场景下机器照样熄屏休眠。stayawake 补上这一层判断。

- 单个原生 exe，**373 KB**，无运行时依赖
- 实测常驻开销：**私有内存 2.0 MB，平均 CPU 0.17%/单核**（20 核机器上约占总算力 0.008%）
- 对游戏无影响：EcoQoS 丢到 E-core、BelowNormal 优先级、**绝不调 `timeBeginPeriod`**

---

## 快速开始

```powershell
cargo build --release
cargo test --release          # 70 个单元测试

# 看一眼各检测器读数（不常驻，调阈值用）
.\target\release\stayawake.exe --status

# 以守护进程运行（托盘图标）
.\target\release\stayawake.exe

# 开机自启
.\target\release\stayawake.exe --install-autostart
```

首次运行自动在 `%LOCALAPPDATA%\stayawake\` 下生成带中文注释的 `config.ini`。

---

## 检测器

| 检测器 | 判据 | 说明 |
|---|---|---|
| **音频** | WASAPI 逐会话：`state==Active` 且 20s 内响过 | 枚举全部输出端点（扬声器/蓝牙/HDMI/USB 都算）。峰值保持窗口避免乐曲安静段落造成状态抖动 |
| **网络** | `GetIfTable2` 全网卡字节差分 ≥ 1 MB/s，连续 2 tick | 15s 窗口天然滤掉遥测突发 |
| **进程 CPU** | 白名单进程单核占用 ≥ 5% | 只针对 cargo/rustc/ffmpeg 这类「高 CPU = 真在干活」的工具 |
| **下载器** | 进程 I/O ≥ 50 KB/s，**或**（established TCP ≥ 4 且 I/O ≥ 5 KB/s） | 专治 IDM（实测 CPU 恒为 0，靠 CPU 判不出来） |
| **提示文件** | `hints\*.hint` 的 mtime 在 TTL 内 | 给任何程序留的精确通道 |

任一命中即视为"忙"。

### 为什么下载器要单独一条规则

实测 IDM 空闲 45 分钟累计 CPU 只有 2.55 秒，**下载时 CPU 也几乎为 0**（I/O 密集而非计算密集）。
所以既不能靠"进程存在"（开机就永不休眠），也不能靠 CPU 阈值。

TCP 连接数那条针对「服务器限速、速度很低但下载并未结束」：
IDM 默认每文件开 8 条连接，空闲时 0–2 条，阈值 4 可干净区分。

但**连接数不能单独成为判据**。做种中的 BT 客户端会长期持有几十条 established 连接而吞吐为零，
只看连接数就会让机器再也不睡（和 Wallpaper Engine 一直输出音频是同一类问题）。
所以这条要求同时有 ≥ 5 KB/s 的吞吐。

5 KB/s 这个下限是实测定的：代理软件（verge-mihomo，5 条 established）空闲时的心跳流量稳定在
**0.09 KB/s**，用 `> 0` 会永久命中。默认名单也不含 BT 客户端。

> `dl_io_kbps` 只对 `dl_processes` 名单生效，绝不能做成全局规则。
> 实测 OpenCode 的 renderer↔gpu 共享内存流量达 **7.3 MB/s 且一字节未落盘** ——
> `GetProcessIoCounters` 统计的是全部 I/O，不只磁盘。

### 提示文件（hint）

```powershell
# 保持唤醒
"why I'm busy" > "$env:LOCALAPPDATA\stayawake\hints\mytask.hint"
# 忙碌期间每 20s touch 一次即可（TTL 60s）
# 结束后删除
Remove-Item "$env:LOCALAPPDATA\stayawake\hints\mytask.hint"
```

mtime 超过 TTL 自动失效 —— 写方崩溃不会把机器永久卡醒。

**OpenCode 用户**：`plugins/stayawake-hint.js` 复制到 `~/.config/opencode/plugins/`，然后**重启 OpenCode**
（插件只在启动时加载一次）。

插件覆盖整个会话生命周期 —— 从 `session.status: busy` 到 `session.idle`，包括**模型思考**
（此时既没有工具在跑、CPU 也不高，任何被动检测都抓不到）、流式输出、工具执行。
多会话并发时用引用计数，全部 idle 才释放；`session.error` / `session.deleted` 也会释放，不会卡住。

必须用 hint 而不是 CPU 判断的原因（实测）：

| OpenCode 进程 | 工具执行中的单核 CPU |
|---|---|
| gpu | 23–31% |
| renderer | 20–30% |
| main | 4–10% |
| **node（真正干活的）** | **0.0–0.3%** |

CPU 几乎全被 UI 重绘吃掉，和"是否真在工作"无关。转圈动画 ≠ 在工作。

---

## 供电策略

这是全部 Modern Standby 知识的落点。默认值：

```ini
policy_ac = system     # 插电: 只阻止睡眠, 允许屏幕正常熄掉
policy_dc = display    # 电池: 保持屏幕
```

依据（微软官方文档）：

- **插电**：`PowerRequestSystemRequired` 可**无限期**阻塞 No-CS 阶段 → 只防睡就够，让屏幕熄掉省电
- **电池**：system / execution request 在睡眠超时后**约 5 分钟被强制清除**；
  只有 `PowerRequestDisplayRequired` 不受此限 → 想可靠必须保屏幕

`never_wake_display = true`（默认）：屏幕已经熄了就不主动点亮，降级为仅防睡。
半夜下载启动不会突然闪亮屏。代价是电池下此时只能拿到约 5 分钟。

---

## 托盘

**左键单击** = 自动 ↔ 暂停快速切换 · **双击** = 状态详情 · **右键** = 完整菜单

菜单**支持连续操作**：切模式、开关检测器、改策略、开机自启这些项点完菜单不收起也不闪，
勾选状态就地更新，可以一次右键连点好几个。只有「当前状态详情 / 打开日志 / 打开配置文件 / 退出」
会收起（这些本来就要把焦点交给别的窗口）。点菜单外部、切到别的程序或按 Esc 收起。

> 实现上装一个线程级 `WH_MOUSE` 钩子。点在"可连续操作"的项上时钩子就地执行命令、
> 用 `CheckMenuItem` + `SetMenuItemInfoW` 改勾选和标题、`RedrawWindow` 重绘，
> 然后**吞掉这次点击** —— 菜单完全不知道被点过，所以既不关闭也不重建，没有闪烁。
> 需要关闭菜单的项则放行走原生路径，由 `TPM_RETURNCMD` 返回。
> 键盘（方向键 + Enter）不经过鼠标钩子，走 `TPM_RETURNCMD` 分支，行为与普通菜单一致。

图标颜色即状态：

| 颜色 | 含义 |
|---|---|
| 灰 | 空闲，未持有请求 |
| 琥珀 | 仅阻止睡眠（允许熄屏） |
| 蓝 | 同时保持屏幕 |
| 蓝 + 绿点 | 强制常亮（含倒计时） |
| 红 + 斜杠 | 已暂停 |

菜单里每个检测器都能独立开关，改动立即生效并回写配置（保留注释）。

也可以从脚本控制（`WM_COMMAND` + 菜单项 ID，见 `src/tray.rs` 顶部常量）：

```powershell
Add-Type -Namespace T -Name W -MemberDefinition '
[DllImport("user32.dll",CharSet=CharSet.Unicode)] public static extern IntPtr FindWindowW(string c,string w);
[DllImport("user32.dll")] public static extern IntPtr SendMessageW(IntPtr h,uint m,IntPtr wp,IntPtr lp);'
$h = [T.W]::FindWindowW("stayawake_msgwnd","stayawake")
[T.W]::SendMessageW($h, 0x0111, [IntPtr]111, [IntPtr]::Zero)   # 111 = 暂停
```

---

## 已验证

**70 个单元测试**，`cargo test --release` 全绿。覆盖的都是"改一处坏一处"风险最高的纯逻辑：

| 模块 | 测试重点 |
|---|---|
| `config` | 重复键取第一个（与回写位置一致）、行尾注释保留、阈值下界夹取、`migrate` 幂等 |
| `detect::mod` | `RateMeter` 窗口语义与最小窗口门限、`CpuTracker` PID 复用与背靠背采样 |
| `detect::dl` | 判据真值表（心跳量级 vs 真实下载）、I/O 不可读时的退化路径 |
| `detect::hint` | TTL 边界、**未来 mtime 判为过期** |
| `detect::net` | probe/tick 基线独立、禁用时清基线、网卡过滤层去重 |
| `engine` | 供电策略真值表、`Held` flags 必带 `ES_CONTINUOUS` |
| `power` | `apply_hold` 返回值语义（首次调用即成功） |
| `autostart` | 计划任务 XML 的三个坑全部关闭、路径转义、UTF-16 BOM |

真机验证（提权 `powercfg /requests` 是唯一真值来源）：

```
SYSTEM:
[PROCESS] \Device\HarddiskVolume7\...\stayawake.exe
```

其余实测项：

- 音频：播放 440 Hz 测试音 → 命中 `audio: ...(0.87)`，连续播放 60 秒零抖动，停止后按宽限期释放
- 网速：清华镜像 ISO 下载 → **6 秒内命中 `net:10.0MB/s`**，停止后释放
- 下载器：代理软件空闲（5 条 established，心跳 0.09 KB/s）→ 正确判为不忙
- hint：新鲜文件命中；mtime 改到 5 分钟前 → `STALE`；改到 8 小时后 → `FUTURE` 且不命中
- 检测延迟：**2 秒**（4 次测量 2.1 / 2.0 / 1.8 / 2.0）
- 开销：CPU 0.12–0.18% 单核，私有内存 1.6 MB
- 压测：100 次图标重绘 + 200 次开关 toggle → GDI / USER / 句柄计数**零增长**
- 自启：计划任务 XML 三个默认坑已确认关闭；开关点击 **8 ms 返回**（异步化前会阻塞数百 ms）
- 单实例、配置热重载、托盘开关回写、Explorer 重启后重加图标

---

## 已知限制

- **用户主动合盖 / 按电源键 / 点睡眠一定会睡**。这是 Windows 设计也是正确行为，不去对抗。
- `ES_DISPLAY_REQUIRED` 按文档**不阻止屏保**。
- 网络检测是全机聚合，不区分进程；VPN / 局域网流量同样计入。
- 一旦已进入 Modern Standby，桌面应用被 DAM 挂起，我们也跑不动 —— 所以整个设计是**提前阻止进入**。

### 本机（Win11 Insider 26340）踩到的三个坑

1. **`RegisterPowerSettingNotification` 的 flags 必须是 `DEVICE_NOTIFY_WINDOW_HANDLE`（= 0）**。
   误传 2（`DEVICE_NOTIFY_CALLBACK`）会让系统把 HWND 当函数指针调用，直接 `0xC0000005`。

2. **`SetWaitableTimer` 的周期模式在本机会连续触发**，造成每秒数十次 tick 的风暴。
   改成一次性定时器 + 每轮手动重新武装后正常。

3. **`Instant::now() - Duration::from_secs(86400)` 会 panic**。`Instant` 是单调时钟，
   开机时长不足该值时直接下溢（实测开机 1.25 小时时进程静默崩溃，`0xC0000409`）。
   哨兵值要用 `Option<Instant>`，不要用"很久以前"。

### 设计上必须记住的三条

这三点是审计时发现的真实缺陷，改法都不显然：

- **廉价探测（probe）与完整检测（tick）不能共用速率计**。probe 会推进基线，
  使紧随其后的 tick 只剩十几毫秒窗口，被 `RateMeter` 的最小窗口门限丢弃 ——
  `consec` 永远无法累积，网速检测彻底失效。
- **CPU 采样的时间戳必须与样本一起存**，不能用一个全局 `last_sample`。
  检测器被关掉一段时间再开启时，Δcpu 会跨越数小时而分母只有一个 tick，
  算出几百 % 的假 CPU。同理需要最小采样窗口（背靠背 tick 是可达状态）。
- **文件 mtime 落在未来必须视为过期**，不能视为"刚刷新"。时钟回跳
  （CMOS 电池、双系统写 RTC、快照恢复）会让所有 hint 永久新鲜，机器再也不睡。

---

## 配置

`%LOCALAPPDATA%\stayawake\config.ini`

```ini
poll_interval_secs = 15
fast_poll_secs     = 2      # 空闲时的廉价探测间隔; 0 = 关闭
grace_secs         = 90     # 活动停止后的宽限期, 防抖
policy_ac          = system
policy_dc          = display
never_wake_display = true
sleep_on_release   = false  # true = 宽限期结束后主动让机器睡

audio_enabled        = true
audio_peak_threshold = 0.0001
audio_hold_secs      = 20   # 峰值保持窗口, 抗乐曲安静段落
audio_ignore         = wallpaper64.exe, wallpaper32.exe, ...

net_enabled              = true
net_threshold_kbps       = 1024   # 1 MB/s
net_min_consecutive_tick = 2

proc_enabled           = true
proc_cpu_percent_1core = 5.0
proc_busy_when_cpu     = cargo.exe, rustc.exe, ffmpeg.exe, ...

dl_enabled   = true
dl_processes = IDMan.exe, idmBroker.exe, aria2c.exe
dl_io_kbps   = 50
dl_tcp_conns = 4
hint_enabled  = true
hint_ttl_secs = 60
```

升级版本后旧配置缺失的新键会被**自动追加**（带注释），已有值不动。

`audio_ignore` 用来排除常驻输出音频的程序 —— Wallpaper Engine 已预置。
遇到别的"永不休眠"元凶，用 `--status` 看 `[audio]` 段哪个进程 `active=true peak>0`，填进去即可。

---

## 构建

```powershell
cargo build --release   # 产物: target\release\stayawake.exe
```

仅支持 Windows（依赖 Win32 API：WASAPI、IP Helper、Power Management）。

---

## License

[MIT](LICENSE)
