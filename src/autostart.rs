// 开机自启: 优先登录触发的计划任务(免管理员, 运行于交互会话), 失败回退 HKCU Run。
//
// 必须运行在交互会话: session 0 的服务看不到用户的音频会话。
//
// 计划任务有三个会静默坏事的默认值, 全部显式关掉:
//   DisallowStartIfOnBatteries  默认 true  -> 电池下根本不启动
//   StopIfGoingOnBatteries      默认 true  -> 拔电就被杀
//   ExecutionTimeLimit          默认 3 天  -> 到点被杀
use std::os::windows::process::CommandExt;
use std::process::Command;

const TASK_NAME: &str = "stayawake";
const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_VALUE: &str = "stayawake";
/// 不要弹出控制台窗口 (我们是 windows 子系统程序)
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

fn run(program: &str, args: &[&str]) -> bool {
    Command::new(program)
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .is_ok_and(|o| o.status.success())
}

fn current_exe() -> String {
    std::env::current_exe()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn account_name() -> String {
    let user = std::env::var("USERNAME").unwrap_or_default();
    match std::env::var("USERDOMAIN") {
        Ok(d) if !d.is_empty() && !user.is_empty() => format!("{}\\{}", d, user),
        _ => user,
    }
}

/// XML 文本节点转义
fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// schtasks /xml 要求 UTF-16LE + BOM
fn write_utf16(path: &std::path::Path, s: &str) -> std::io::Result<()> {
    let mut bytes = vec![0xFF, 0xFE];
    for u in s.encode_utf16() {
        bytes.extend_from_slice(&u.to_le_bytes());
    }
    std::fs::write(path, bytes)
}

fn task_xml() -> String {
    let exe = current_exe();
    let dir = std::path::Path::new(&exe)
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let user = esc(&account_name());
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>stayawake - activity-based sleep suppression daemon</Description>
  </RegistrationInfo>
  <Triggers>
    <LogonTrigger>
      <Enabled>true</Enabled>
      <UserId>{user}</UserId>
    </LogonTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <UserId>{user}</UserId>
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>LeastPrivilege</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>true</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <IdleSettings>
      <StopOnIdleEnd>false</StopOnIdleEnd>
      <RestartOnIdle>false</RestartOnIdle>
    </IdleSettings>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <Hidden>false</Hidden>
    <RunOnlyIfIdle>false</RunOnlyIfIdle>
    <WakeToRun>false</WakeToRun>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>7</Priority>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{exe}</Command>
      <WorkingDirectory>{dir}</WorkingDirectory>
    </Exec>
  </Actions>
</Task>
"#,
        user = user,
        exe = esc(&exe),
        dir = esc(&dir)
    )
}

pub fn install() -> String {
    let tmp = std::env::temp_dir().join("stayawake_task.xml");
    let scheduled = write_utf16(&tmp, &task_xml()).is_ok()
        && run(
            "schtasks",
            &["/create", "/tn", TASK_NAME, "/xml", &tmp.to_string_lossy(), "/f"],
        );
    let _ = std::fs::remove_file(&tmp);

    if scheduled {
        // 计划任务成功时清掉可能存在的 Run 键, 避免开机起两次
        let _ = run("reg", &["delete", RUN_KEY, "/v", RUN_VALUE, "/f"]);
        return format!("autostart: 计划任务已注册 ({})", current_exe());
    }

    let quoted = format!("\"{}\"", current_exe());
    if run(
        "reg",
        &["add", RUN_KEY, "/v", RUN_VALUE, "/t", "REG_SZ", "/d", &quoted, "/f"],
    ) {
        format!("autostart: 计划任务失败, 已回退注册表 Run ({})", quoted)
    } else {
        "autostart: 注册失败(计划任务与注册表均未成功)".to_string()
    }
}

pub fn uninstall() -> String {
    let a = run("schtasks", &["/delete", "/tn", TASK_NAME, "/f"]);
    let b = run("reg", &["delete", RUN_KEY, "/v", RUN_VALUE, "/f"]);
    if a || b {
        "autostart: 已移除".to_string()
    } else {
        "autostart: 未找到需要移除的项".to_string()
    }
}

pub fn is_installed() -> bool {
    run("schtasks", &["/query", "/tn", TASK_NAME])
        || run("reg", &["query", RUN_KEY, "/v", RUN_VALUE])
}
