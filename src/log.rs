// 变更日志: 只在状态跃变时写一行, 1 MB 轮转
use std::io::Write;
use std::sync::Mutex;

static LOCK: Mutex<()> = Mutex::new(());
const MAX_SIZE: u64 = 1024 * 1024;

pub fn event(msg: &str) {
    let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let path = crate::config::log_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // 超过上限就转存为 .1, 只保留一代
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > MAX_SIZE) {
        let _ = std::fs::rename(&path, path.with_extension("log.1"));
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "{}  {}", crate::power::now_local(), msg);
    }
}

pub fn open_in_editor(path: &std::path::Path) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if !path.exists() {
        let _ = std::fs::write(path, "");
    }
    let _ = std::process::Command::new("notepad.exe").arg(path).spawn();
}
