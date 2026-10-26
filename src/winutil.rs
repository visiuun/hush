//! Win32 helpers: elevation, startup task, fullscreen / idle detection, single instance.

use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use windows::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, HWND, RECT};
use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow};
use windows::Win32::Security::{GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation};
use windows::Win32::System::Threading::{
    CreateMutexW, GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
    QueryFullProcessImageNameW,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
use windows::Win32::UI::Shell::{QUNS_PRESENTATION_MODE, QUNS_RUNNING_D3D_FULL_SCREEN, SHQueryUserNotificationState, ShellExecuteW};
use windows::Win32::UI::WindowsAndMessaging::{
    GWL_STYLE, GetClassNameW, GetForegroundWindow, GetWindowLongW, GetWindowRect, GetWindowThreadProcessId, SW_HIDE, SW_SHOWNORMAL,
    WS_CAPTION, WS_MAXIMIZE, WS_POPUP,
};
use windows::core::{HSTRING, PCWSTR, PWSTR, w};

pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;
pub const TASK_NAME: &str = "Hush Fan Control";

/// True on machines with a system battery (laptops, tablets).
pub fn has_battery() -> bool {
    let mut s = windows::Win32::System::Power::SYSTEM_POWER_STATUS::default();
    // 128 = no system battery, 255 = unknown.
    unsafe { windows::Win32::System::Power::GetSystemPowerStatus(&mut s) }.is_ok() && s.BatteryFlag != 128 && s.BatteryFlag != 255
}

pub fn is_elevated() -> bool {
    unsafe {
        let mut tok = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut tok).is_err() {
            return false;
        }
        let mut e = TOKEN_ELEVATION::default();
        let mut len = 0u32;
        let ok = GetTokenInformation(tok, TokenElevation, Some(&mut e as *mut _ as *mut _), size_of::<TOKEN_ELEVATION>() as u32, &mut len).is_ok();
        let _ = CloseHandle(tok);
        ok && e.TokenIsElevated != 0
    }
}

/// Relaunches `exe` elevated through UAC. Returns false if the user declined.
pub fn run_elevated(exe: &std::path::Path, args: &str) -> bool {
    unsafe {
        let r = ShellExecuteW(None, w!("runas"), &HSTRING::from(exe.as_os_str()), &HSTRING::from(args), PCWSTR::null(), SW_SHOWNORMAL);
        r.0 as isize > 32
    }
}

pub fn app_exe() -> PathBuf {
    let me = std::env::current_exe().unwrap_or_default();
    me.with_file_name("hush-app.exe")
}

// ---------------------------------------------------------------------------------------------
// Startup: a logon task with highest privileges, so Hush starts elevated without a UAC prompt.
// ---------------------------------------------------------------------------------------------

fn schtasks(args: &[&str]) -> std::io::Result<std::process::Output> {
    std::process::Command::new("schtasks").args(args).creation_flags(CREATE_NO_WINDOW).output()
}

pub fn startup_enabled() -> bool {
    schtasks(&["/Query", "/TN", TASK_NAME]).is_ok_and(|o| o.status.success())
}

/// Starts the app through the logon task (elevated, no UAC prompt). False if no task exists.
pub fn start_via_task() -> bool {
    schtasks(&["/Run", "/TN", TASK_NAME]).is_ok_and(|o| o.status.success())
}

pub fn set_startup(enable: bool) -> Result<(), String> {
    if !enable {
        let o = schtasks(&["/Delete", "/TN", TASK_NAME, "/F"]).map_err(|e| e.to_string())?;
        return if o.status.success() || !startup_enabled() { Ok(()) } else { Err(String::from_utf8_lossy(&o.stderr).trim().to_string()) };
    }
    let exe = app_exe();
    let user = std::env::var("USERNAME").unwrap_or_default();
    let domain = std::env::var("USERDOMAIN").unwrap_or_default();
    let esc = |s: &str| s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    // No execution time limit, runs on battery, restarts if it ever crashes.
    let xml = format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo><Description>Hush fan control (starts elevated at logon)</Description></RegistrationInfo>
  <Triggers><LogonTrigger><Enabled>true</Enabled><UserId>{domain}\{user}</UserId></LogonTrigger></Triggers>
  <Principals><Principal id="Author"><UserId>{domain}\{user}</UserId><LogonType>InteractiveToken</LogonType><RunLevel>HighestAvailable</RunLevel></Principal></Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>true</StartWhenAvailable>
    <IdleSettings><StopOnIdleEnd>false</StopOnIdleEnd><RestartOnIdle>false</RestartOnIdle></IdleSettings>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <Hidden>false</Hidden>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>4</Priority>
    <RestartOnFailure><Interval>PT1M</Interval><Count>5</Count></RestartOnFailure>
  </Settings>
  <Actions Context="Author"><Exec><Command>{}</Command><Arguments>--tray</Arguments><WorkingDirectory>{}</WorkingDirectory></Exec></Actions>
</Task>"#,
        esc(&exe.display().to_string()),
        esc(&exe.parent().map(|p| p.display().to_string()).unwrap_or_default()),
    );
    let path = std::env::temp_dir().join("hush-task.xml");
    let mut bytes: Vec<u8> = vec![0xFF, 0xFE];
    bytes.extend(xml.encode_utf16().flat_map(|c| c.to_le_bytes()));
    std::fs::write(&path, bytes).map_err(|e| e.to_string())?;
    let o = schtasks(&["/Create", "/TN", TASK_NAME, "/XML", &path.display().to_string(), "/F"]).map_err(|e| e.to_string())?;
    let _ = std::fs::remove_file(&path);
    if o.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&o.stderr).trim().to_string())
    }
}

// ---------------------------------------------------------------------------------------------
// Automation probes
// ---------------------------------------------------------------------------------------------

pub fn idle_seconds() -> f32 {
    unsafe {
        let mut li = LASTINPUTINFO { cbSize: size_of::<LASTINPUTINFO>() as u32, dwTime: 0 };
        if !GetLastInputInfo(&mut li).as_bool() {
            return 0.0;
        }
        let now = windows::Win32::System::SystemInformation::GetTickCount();
        now.wrapping_sub(li.dwTime) as f32 / 1000.0
    }
}

pub fn process_image_path(pid: u32) -> Option<PathBuf> {
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len).is_ok();
        let _ = CloseHandle(h);
        ok.then(|| PathBuf::from(String::from_utf16_lossy(&buf[..len as usize])))
    }
}

/// The foreground app's exe name when it is truly fullscreen (exclusive or borderless).
pub fn fullscreen_app() -> Option<String> {
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.is_invalid() {
            return None;
        }
        let mut cls = [0u16; 256];
        let n = GetClassNameW(hwnd, &mut cls).max(0) as usize;
        let cls = String::from_utf16_lossy(&cls[..n]).to_lowercase();
        if matches!(cls.as_str(), "progman" | "workerw" | "shell_traywnd" | "shell_secondarytraywnd" | "windows.ui.core.corewindow") {
            return None;
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid == std::process::id() {
            return None;
        }
        let name = || {
            process_image_path(pid)
                .and_then(|p| p.file_name().map(|f| f.to_string_lossy().into_owned()))
                .unwrap_or_else(|| "Fullscreen app".into())
        };
        let ignored = |n: &str| matches!(n.to_lowercase().as_str(), "explorer.exe" | "searchhost.exe" | "startmenuexperiencehost.exe");
        if let Ok(state) = SHQueryUserNotificationState() {
            if state == QUNS_RUNNING_D3D_FULL_SCREEN || state == QUNS_PRESENTATION_MODE {
                let n = name();
                return (!ignored(&n)).then_some(n);
            }
        }
        let style = GetWindowLongW(hwnd, GWL_STYLE) as u32;
        let caption = style & WS_CAPTION.0;
        if style & WS_MAXIMIZE.0 != 0 && caption != 0 {
            return None;
        }
        let mut r = RECT::default();
        GetWindowRect(hwnd, &mut r).ok()?;
        let mut mi = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
        if !GetMonitorInfoW(MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST), &mut mi).as_bool() {
            return None;
        }
        let m = mi.rcMonitor;
        let covers = r.left <= m.left && r.top <= m.top && r.right >= m.right && r.bottom >= m.bottom;
        let borderless = caption != WS_CAPTION.0 || style & WS_POPUP.0 != 0;
        let n = name();
        (covers && borderless && !ignored(&n)).then_some(n)
    }
}

// ---------------------------------------------------------------------------------------------
// Single instance + window helpers
// ---------------------------------------------------------------------------------------------

pub struct SingleInstance(#[allow(dead_code)] HANDLE);

unsafe impl Send for SingleInstance {}
unsafe impl Sync for SingleInstance {}

impl SingleInstance {
    /// `None` if another instance already holds the lock.
    pub fn acquire() -> Option<Self> {
        unsafe {
            let m = CreateMutexW(None, true, w!("Global\\HushFanControl.Instance")).ok()?;
            if GetLastError() == ERROR_ALREADY_EXISTS {
                let _ = CloseHandle(m);
                return None;
            }
            Some(Self(m))
        }
    }
}

pub fn hwnd(h: isize) -> HWND {
    HWND(h as *mut core::ffi::c_void)
}

/// Shows / hides a window directly (works even while eframe isn't repainting).
pub fn show_window(h: isize, show: bool) {
    use windows::Win32::UI::WindowsAndMessaging::{SetForegroundWindow, ShowWindow};
    unsafe {
        let _ = ShowWindow(hwnd(h), if show { SW_SHOWNORMAL } else { SW_HIDE });
        if show {
            let _ = SetForegroundWindow(hwnd(h));
        }
    }
}

/// DWM-cloaks a window: it stays "shown" for the window manager but is never drawn.
pub fn set_cloaked(h: isize, cloak: bool) {
    use windows::Win32::Graphics::Dwm::{DWMWA_CLOAK, DwmSetWindowAttribute};
    let v: windows::core::BOOL = cloak.into();
    unsafe {
        let _ = DwmSetWindowAttribute(hwnd(h), DWMWA_CLOAK, &v as *const _ as *const core::ffi::c_void, size_of_val(&v) as u32);
    }
}

/// Dark title bar + rounded corners (Windows 11).
pub fn style_window(h: isize) {
    use windows::Win32::Graphics::Dwm::{DWMWA_BORDER_COLOR, DWMWA_CAPTION_COLOR, DWMWA_USE_IMMERSIVE_DARK_MODE, DwmSetWindowAttribute};
    unsafe {
        let dark: windows::core::BOOL = true.into();
        let _ = DwmSetWindowAttribute(hwnd(h), DWMWA_USE_IMMERSIVE_DARK_MODE, &dark as *const _ as *const _, size_of_val(&dark) as u32);
        let black: u32 = 0x0000_0000;
        let _ = DwmSetWindowAttribute(hwnd(h), DWMWA_CAPTION_COLOR, &black as *const _ as *const _, 4);
        let border: u32 = 0x001F_1A1A;
        let _ = DwmSetWindowAttribute(hwnd(h), DWMWA_BORDER_COLOR, &border as *const _ as *const _, 4);
    }
}
