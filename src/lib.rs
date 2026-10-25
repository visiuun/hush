//! Hush core: hardware access, smart control engine, automation and IPC.
//! Shared by the `hush` CLI and the `hush-app` tray/GUI daemon.

pub mod automation;
pub mod config;
pub mod engine;
pub mod hw;
pub mod ipc;
pub mod winutil;

/// Appends a timestamped line to `%APPDATA%\Hush\hush.log` (kept under ~256 KB).
pub fn log(msg: impl AsRef<str>) {
    use std::io::Write;
    let path = config::dir().join("hush.log");
    let _ = std::fs::create_dir_all(config::dir());
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > 256 * 1024) {
        let _ = std::fs::rename(&path, config::dir().join("hush.old.log"));
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "{} {}", chrono::Local::now().format("%Y-%m-%d %H:%M:%S"), msg.as_ref());
    }
}
