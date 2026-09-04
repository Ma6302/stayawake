; stayawake 安装脚本 (Inno Setup 6)
;
; 编译:  ISCC.exe installer\stayawake.iss
; 产物:  dist\stayawake-<版本>-setup.exe
;
; 编译前必须先 cargo build --release, 并用 --write-ico 生成图标:
;   cargo build --release
;   .\target\release\stayawake.exe --write-ico installer\stayawake.ico
;
; 本文件必须以 **UTF-8 with BOM** 保存 —— Inno 6 靠 BOM 判断脚本编码,
; 没有 BOM 的话下面所有中文都会变成乱码。

#define AppName "stayawake"
; 必须与 Cargo.toml 的 version 一致。改版本号时两处一起改。
#define AppVersion "0.1.1"
#define AppPublisher "Ma6302"
#define AppURL "https://github.com/Ma6302/stayawake"
#define ExeName "stayawake.exe"
; 守护进程的隐藏消息窗口类名, 见 src/tray.rs。用来在安装/卸载前请它自己退出。
#define WndClass "stayawake_msgwnd"

[Setup]
; AppId 是升级识别的唯一依据, **永远不要改** —— 改了就会变成并列装两份
AppId={{1FF4F9D2-49D6-4F59-B621-425379C0FBFF}
AppName={#AppName}
AppVersion={#AppVersion}
AppVerName={#AppName} {#AppVersion}
AppPublisher={#AppPublisher}
AppPublisherURL={#AppURL}
AppSupportURL={#AppURL}/issues
AppUpdatesURL={#AppURL}/releases
VersionInfoVersion={#AppVersion}
VersionInfoDescription={#AppName} 安装程序
VersionInfoCopyright=Copyright (c) 2026 {#AppPublisher}

; 默认装到 C:\Program Files\stayawake。
; PrivilegesRequired=admin 时 {autopf} 就是 {commonpf}, 即 64 位的 Program Files。
DefaultDirName={autopf}\{#AppName}
; 不显示"选择开始菜单文件夹"那一页, 快捷方式直接放在开始菜单根下
DisableProgramGroupPage=yes
; 保留欢迎页 —— Inno 6 的 modern 风格默认会跳过它, 而这里要的是完整引导
DisableWelcomePage=no
LicenseFile=..\LICENSE

; 装到 Program Files 需要管理员。允许用户在 UAC 弹窗里改成"仅为我安装",
; 那种情况下 {autopf} 会变成 %LOCALAPPDATA%\Programs, 不需要提权。
; commandline 让脚本化安装能显式传 /ALLUSERS 或 /CURRENTUSER。
PrivilegesRequired=admin
PrivilegesRequiredOverridesAllowed=dialog commandline

; 程序是 x64 的。x64compatible 也允许 ARM64(它能模拟执行 x64)
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible

; 装/卸之前先让正在跑的实例退出 —— 见 [Code] 里的 StopRunningInstance。
;
; AppMutex 是**兜底**: 万一优雅停止失败(比如以后改了窗口类名), Inno 会提示用户
; "请先关闭程序", 而不是静默地覆盖失败。名字见 src/main.rs 的单实例互斥体。
; 注意它的检查发生在 InitializeSetup **之后**, 所以停止逻辑放在那里才来得及。
AppMutex=Local\stayawake_single_instance
; CloseApplications 对本程序实际不起作用 —— 实测 RestartManager 的 RmGetList
; 报 "found no applications using one of our files", 即便守护进程正从目标路径运行。
; 留着是为了以后万一多出 DLL 之类的文件, 不是当前的停止手段。
CloseApplications=yes
CloseApplicationsFilter=*.exe
RestartApplications=no

OutputDir=..\dist
OutputBaseFilename={#AppName}-{#AppVersion}-setup
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
SetupIconFile=stayawake.ico
; exe 里没有内嵌图标资源, 所以控制面板的卸载项指向单独的 .ico
UninstallDisplayIcon={app}\stayawake.ico
UninstallDisplayName={#AppName} {#AppVersion}

[Languages]
; 只装中文 —— 只有一种语言时 Inno 不会弹语言选择框, 直接进中文向导
Name: "chinese"; MessagesFile: "compiler:Languages\ChineseSimplified.isl"

[Messages]
; 本机的 ChineseSimplified.isl 是 6.0 时代的翻译(208 条), 而 ISCC 是 6.5(296 条)。
; 缺的 88 条 Inno 会静默回落到英文 —— 卸载确认框、卸载进度页这些**正常流程**
; 也在其中, 于是"中文安装引导"会在卸载时露出英文。
;
; 下面补齐用户在正常路径和常见失败路径上真的会看到的那些。纯内部错误
; (Archive*/Download*/Verification*/Reg*) 不补: 本安装包不下载、不解压外部
; 归档、不写注册表也不做签名校验, 那些分支到不了。
;
; 升级 Inno 或换用更新的 .isl 之后, 这一节可以整体删掉 —— 编译时
; "has not been defined" 的警告数就是判据。
chinese.ConfirmUninstall=你确定要完全移除 %1 及其所有组件吗?
chinese.UninstalledAll=%1 已成功从你的电脑中移除。
chinese.UninstalledMost=%1 卸载完成。%n%n有些内容无法被删除，你可以手动删除它们。
chinese.UninstalledAndNeedsRestart=要完成 %1 的卸载，必须重启你的电脑。%n%n现在就重启吗?
chinese.StatusUninstalling=正在卸载 %1...
chinese.WizardUninstalling=卸载状态
chinese.OnlyAdminCanUninstall=本程序只能由具有管理员权限的用户卸载。
chinese.UninstallNotFound=文件“%1”不存在，无法卸载。
chinese.UninstallOpenError=文件“%1”无法打开，无法卸载。
chinese.UninstallUnknownEntry=卸载日志中遇到无法识别的条目 (%1)
chinese.UninstallUnsupportedVer=卸载日志文件“%1”的格式不被此版本的卸载程序识别，无法卸载。
chinese.UninstallOnlyOnWin64=本程序只能在 64 位 Windows 上卸载。
chinese.SetupAlreadyRunning=安装程序已在运行。
chinese.SetupFileMissing=安装目录中缺少文件 %1。请修正此问题，或重新获取一份程序副本。
chinese.SourceDoesntExist=源文件“%1”不存在
chinese.InvalidParameter=命令行传入了无效参数:%n%n%1
chinese.WindowsVersionNotSupported=本程序不支持你的电脑正在运行的 Windows 版本。
chinese.ShutdownBlockReasonInstallingApp=正在安装 %1。
chinese.ShutdownBlockReasonUninstallingApp=正在卸载 %1。
chinese.PrepareToInstallNeedsRestart=安装程序必须重启你的电脑。重启后请再次运行安装程序以完成 [name] 的安装。%n%n现在就重启吗?
chinese.ExistingFileReadOnly2=现有文件是只读的，无法替换。
chinese.ExistingFileReadOnlyRetry=移除只读属性并重试(&R)
chinese.ExistingFileReadOnlyKeepExisting=保留现有文件(&K)
chinese.ErrorReplacingExistingFile=替换现有文件时出错:
chinese.ErrorRestartReplace=RestartReplace 失败:
chinese.ErrorCreatingTemp=在目标目录中创建文件时出错:
chinese.ErrorCopying=复制文件时出错:
chinese.ErrorChangingAttr=更改现有文件的属性时出错:
chinese.ErrorExecutingProgram=无法执行文件:%n%1
chinese.RetryCancelSelectAction=请选择操作
chinese.RetryCancelRetry=重试(&T)
chinese.RetryCancelCancel=取消
chinese.AbortRetryIgnoreSelectAction=请选择操作
chinese.AbortRetryIgnoreRetry=重试(&T)
chinese.AbortRetryIgnoreIgnore=忽略错误并继续(&I)
chinese.AbortRetryIgnoreCancel=取消安装
chinese.FileAbortRetryIgnoreSkipNotRecommended=跳过此文件(&S)（不推荐）
chinese.FileAbortRetryIgnoreIgnoreNotRecommended=忽略错误并继续(&I)（不推荐）
chinese.FileExists2=文件已存在。
chinese.FileExistsSelectAction=请选择操作
chinese.FileExistsOverwriteExisting=覆盖现有文件(&O)
chinese.FileExistsKeepExisting=保留现有文件(&K)
chinese.FileExistsOverwriteOrKeepAll=后续冲突都这样处理(&D)
chinese.ExistingFileNewer2=现有文件比安装程序要装的那个更新。
chinese.ExistingFileNewerSelectAction=请选择操作
chinese.ExistingFileNewerOverwriteExisting=覆盖现有文件(&O)
chinese.ExistingFileNewerKeepExisting=保留现有文件(&K)（推荐）
chinese.ExistingFileNewerOverwriteOrKeepAll=后续冲突都这样处理(&D)
chinese.ErrorReadingExistingDest=读取现有文件时出错:
chinese.ErrorReadingSource=读取源文件时出错:
chinese.ErrorRenamingTemp=重命名目标目录中的文件时出错:
chinese.ErrorRestartingComputer=安装程序无法重启电脑，请手动重启。
; 这几条拼的是控制面板卸载列表里的显示名。本脚本用 UninstallDisplayName 写死了
; 名称, 所以走不到 —— 但翻译它们的成本是零, 而漏掉的话万一走到就是英文。
chinese.UninstallDisplayNameMark=%1 (%2)
chinese.UninstallDisplayNameMarks=%1 (%2, %3)
chinese.UninstallDisplayNameMark32Bit=32 位
chinese.UninstallDisplayNameMark64Bit=64 位
chinese.UninstallDisplayNameMarkAllUsers=所有用户
chinese.UninstallDisplayNameMarkCurrentUser=当前用户

[CustomMessages]
chinese.AutostartTask=开机自动启动 (登录时由计划任务拉起)
chinese.DesktopIconTask=创建桌面快捷方式
chinese.OptionalTasks=附加任务:
chinese.LaunchAfterInstall=立即运行 {#AppName}
chinese.RemoveUserData=是否同时删除配置与日志?%n%n%1%n%n选「否」将保留它们，下次安装可继续使用原有设置。
chinese.AutostartLeftover=检测到开机自启项仍然存在（可能是用其他账号提权卸载所致）。%n%n请在当前账号下手动执行:%n  schtasks /delete /tn stayawake /f

[Tasks]
; 默认勾选自启: 这是个守护进程, 不常驻就没意义
Name: "autostart"; Description: "{cm:AutostartTask}"; GroupDescription: "{cm:OptionalTasks}"
Name: "desktopicon"; Description: "{cm:DesktopIconTask}"; GroupDescription: "{cm:OptionalTasks}"; Flags: unchecked

[Files]
Source: "..\target\release\{#ExeName}"; DestDir: "{app}"; Flags: ignoreversion
Source: "stayawake.ico"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\README.md"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\LICENSE"; DestDir: "{app}"; Flags: ignoreversion
; OpenCode 插件。装进来是为了让用户能找到它, 复制到 ~\.config\opencode\plugins\
; 这一步得用户自己做 —— 我们不去动别的程序的配置目录。
Source: "..\plugins\stayawake-hint.js"; DestDir: "{app}\plugins"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\{#AppName}"; Filename: "{app}\{#ExeName}"; IconFilename: "{app}\stayawake.ico"
Name: "{autodesktop}\{#AppName}"; Filename: "{app}\{#ExeName}"; IconFilename: "{app}\stayawake.ico"; Tasks: desktopicon

[Run]
; 注册开机自启。
;
; runasoriginaluser 是必须的: 计划任务是**按用户**注册的, 而 --install-autostart
; 靠 USERNAME/USERDOMAIN 决定注册给谁。若用户用另一个管理员账号提权,
; 不加这个标志就会把任务注册到那个管理员名下, 当前用户登录时不会启动。
Filename: "{app}\{#ExeName}"; Parameters: "--install-autostart"; \
  Flags: runhidden runasoriginaluser waituntilterminated; Tasks: autostart; \
  StatusMsg: "正在注册开机自启..."

; 安装完成页的"立即运行"。
;
; runasoriginaluser 同样必须: 安装程序是提权的, 直接启动会让守护进程也跑在
; 高权限下 —— 那样 UIPI 会挡掉普通权限进程发给它的窗口消息(README 里
; 那段 SendMessage 脚本就不灵了), 而且它并不需要管理员权限。
Filename: "{app}\{#ExeName}"; Description: "{cm:LaunchAfterInstall}"; \
  Flags: nowait postinstall skipifsilent runasoriginaluser

[UninstallRun]
; 必须在删文件之前跑 —— [UninstallRun] 正是在卸载开始时执行的。
; 反过来先删了 exe 就没人能移除计划任务, 会留下一个指向空路径的任务。
;
; **这里不能用 runasoriginaluser** —— 实测该标志在 [UninstallRun] 不被支持
; (编译期报 "Parameter Flags includes a flag that is not supported in this section")。
; 后果: 用户若用另一个管理员账号提权卸载, 移除的是那个账号的计划任务,
; 当前用户的任务会残留。所以下面 [Code] 里的 CurUninstallStepChanged 会检查
; 残留并提示用户手动清理。
Filename: "{app}\{#ExeName}"; Parameters: "--uninstall-autostart"; \
  Flags: runhidden waituntilterminated; RunOnceId: "RemoveAutostart"

[Code]
// 注意: 本节的注释一律用 `//`。Inno 的 Pascal 块注释 `{ }` **不嵌套**,
// 而这里免不了要在注释里写 {app} / {localappdata} 这类常量 —— 其中的 `}`
// 会提前终止注释, 后面的中文就被当成代码去解析了(实测报 "Syntax error")。

const
  WM_CLOSE = $0010;

// 请正在运行的实例自己退出, 最多等 5 秒。
//
// 为什么不用 taskkill: 守护进程收到 WM_CLOSE 后走 DefWindowProc -> WM_DESTROY,
// 那里会摘掉托盘图标、释放 execution state 并写一行 "stopped" 日志。
// 直接杀进程会留下一个托盘幽灵图标, 直到鼠标划过去才消失。
//
// 返回 False 表示等超时了 —— 调用方据此决定是否要提示用户。
function StopRunningInstance: Boolean;
var
  Wnd: HWND;
  I: Integer;
begin
  Wnd := FindWindowByClassName('{#WndClass}');
  if Wnd = 0 then
  begin
    Result := True;
    Exit;
  end;

  PostMessage(Wnd, WM_CLOSE, 0, 0);
  for I := 1 to 50 do
  begin
    Sleep(100);
    if FindWindowByClassName('{#WndClass}') = 0 then
    begin
      // 窗口没了, 再给进程一点时间把日志写完并退出
      Sleep(300);
      Result := True;
      Exit;
    end;
  end;
  Result := False;
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
begin
  // 到这一步实例其实已经在 InitializeSetup 里停掉了。这里再兜一次:
  // 用户在向导页上停留期间可能又从开始菜单把它点起来了。
  StopRunningInstance;
  Result := '';
end;

// 停止实例必须放在 InitializeSetup —— 这是 [Code] 里**最早**的入口。
//
// 实测顺序: InitializeSetup -> (AppMutex 检查) -> 向导 -> PrepareToInstall。
// 起初把停止逻辑只放在 PrepareToInstall, 结果静默安装直接失败(退出码 1):
// AppMutex 早在向导之前就发现互斥体还在, 弹出"请关闭它的所有实例"并中止,
// PrepareToInstall 根本没机会执行。
function InitializeSetup: Boolean;
begin
  StopRunningInstance;
  Result := True;
end;

function InitializeUninstall: Boolean;
begin
  StopRunningInstance;
  Result := True;
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var
  DataDir: String;
  ResultCode: Integer;
begin
  if CurUninstallStep = usPostUninstall then
  begin
    // [UninstallRun] 那条移除自启的命令跑在卸载者的身份下, 而计划任务是按用户
    // 注册的。用另一个管理员账号提权卸载时, 当前用户的任务不会被移除。
    // 查一下还在不在, 在就告诉用户怎么手动清 —— 悄悄留一个开机项最糟。
    //
    // schtasks /query 找不到任务时返回非 0, 所以"能查到"就等于"有残留"。
    if Exec(ExpandConstant('{sys}\schtasks.exe'),
            '/query /tn {#AppName}', '', SW_HIDE,
            ewWaitUntilTerminated, ResultCode) and (ResultCode = 0) then
      SuppressibleMsgBox(CustomMessage('AutostartLeftover'), mbInformation, MB_OK, IDOK);

    // 配置和日志在 %LOCALAPPDATA%\stayawake, 不在安装目录里, 所以默认不会被删。
    // 问一句再删: 里面是用户自己调的阈值和忽略名单, 重装后往往还想用。
    // 默认按钮故意选"否"。
    //
    // **必须用 SuppressibleMsgBox 而不是 MsgBox**: 静默卸载(/VERYSILENT)时
    // 普通 MsgBox 照样会弹出来并无限期阻塞 —— 脚本化卸载会直接卡死。
    // 最后一个参数是静默时采用的答案, 这里取 IDNO = 不删用户数据。
    //
    // 注意: 若用户用**另一个**管理员账号提权卸载, 这里取到的是那个账号的目录。
    // 所以提示里把完整路径打出来, 让用户看清要删的到底是哪个。
    DataDir := ExpandConstant('{localappdata}\{#AppName}');
    if DirExists(DataDir) then
      if SuppressibleMsgBox(FmtMessage(CustomMessage('RemoveUserData'), [DataDir]),
                            mbConfirmation, MB_YESNO or MB_DEFBUTTON2, IDNO) = IDYES then
        DelTree(DataDir, True, True, True);
  end;
end;
