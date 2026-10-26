//! External control: one JSON request line → one JSON response line over `\\.\pipe\hush`.
//!
//! ```text
//! {"cmd":"status"}
//! {"cmd":"profile","name":"Silent"}         name:null → automatic
//! {"cmd":"set","fan":"fan2","mode":{"kind":"fixed","duty":60}}
//! {"cmd":"reset","fan":null}
//! {"cmd":"boost","seconds":30}
//! ```

use crate::config::FanMode;
use crate::engine::{Cmd, Shared};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Write};
use std::os::windows::io::FromRawHandle;
use std::sync::Arc;
use windows::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE};
use windows::Win32::Security::Authorization::{ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1};
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows::Win32::Storage::FileSystem::{FlushFileBuffers, PIPE_ACCESS_DUPLEX};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
};
use windows::core::w;

pub const PIPE: &str = r"\\.\pipe\hush";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    Ping,
    Status,
    Profiles,
    Profile { name: Option<String> },
    Set { fan: String, mode: FanMode },
    Reset { fan: Option<String> },
    Boost { seconds: Option<f32> },
    Pause { on: bool },
    Calibrate { fans: Vec<String> },
    CancelCalibration,
    Automation { on: bool },
    Show {
        #[serde(default)]
        page: Option<String>,
    },
    /// nbfc-linux laptop config: `status`, `find` (search again), `use` (switch on the found one), `off`.
    Laptop { action: String },
    Quit,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

impl Response {
    fn ok(msg: impl Into<String>) -> Self {
        Self { ok: true, message: msg.into(), data: None }
    }
    fn err(msg: impl Into<String>) -> Self {
        Self { ok: false, message: msg.into(), data: None }
    }
}

pub struct Hooks {
    pub show: Box<dyn Fn(Option<String>) + Send + Sync>,
    pub quit: Box<dyn Fn() + Send + Sync>,
}

/// Resolves "fan2", "2", or a user name ("Pump", case-insensitive) to a fan id.
fn resolve_fan(sh: &Shared, s: &str) -> Option<String> {
    let snap = sh.snapshot();
    let s = s.trim();
    let lower = s.to_lowercase();
    if let Some(f) = snap.fans.iter().find(|f| f.id == lower || f.name.to_lowercase() == lower) {
        return Some(f.id.clone());
    }
    if let Ok(n) = s.parse::<usize>() {
        let id = format!("fan{n}");
        return snap.fans.iter().any(|f| f.id == id).then_some(id);
    }
    snap.fans.iter().find(|f| f.name.to_lowercase().starts_with(&lower)).map(|f| f.id.clone())
}

pub fn handle(sh: &Arc<Shared>, hooks: &Hooks, req: Request) -> Response {
    match req {
        Request::Ping => Response::ok("pong"),
        Request::Status => Response { ok: true, message: String::new(), data: serde_json::to_value(&*sh.snapshot()).ok() },
        Request::Profiles => {
            let c = sh.config.read();
            let names: Vec<&str> = c.profiles.iter().map(|p| p.name.as_str()).collect();
            Response { ok: true, message: String::new(), data: Some(serde_json::json!({ "profiles": names, "base": c.base_profile })) }
        }
        Request::Profile { name: None } => {
            sh.send(Cmd::SetProfile(None));
            Response::ok("Profile selection is automatic again")
        }
        Request::Profile { name: Some(n) } => {
            let found = sh.config.read().profile(&n).map(|p| p.name.clone());
            match found {
                Some(p) => {
                    sh.send(Cmd::SetProfile(Some(p.clone())));
                    Response::ok(format!("Pinned profile {p}"))
                }
                None => Response::err(format!("No profile named '{n}'")),
            }
        }
        Request::Set { fan, mode } => match resolve_fan(sh, &fan) {
            Some(id) => {
                let msg = match &mode {
                    FanMode::Fixed { duty } => format!("{id} → {duty:.0} %"),
                    FanMode::Bios => format!("{id} → BIOS control"),
                    FanMode::Curve(_) => format!("{id} → custom curve"),
                };
                sh.send(Cmd::Override { fan: id, mode: Some(mode) });
                Response::ok(msg)
            }
            None => Response::err(format!("Unknown fan '{fan}'")),
        },
        Request::Reset { fan: None } => {
            sh.send(Cmd::ClearOverrides);
            Response::ok("All overrides cleared")
        }
        Request::Reset { fan: Some(f) } => match resolve_fan(sh, &f) {
            Some(id) => {
                sh.send(Cmd::Override { fan: id.clone(), mode: None });
                Response::ok(format!("{id} follows its profile again"))
            }
            None => Response::err(format!("Unknown fan '{f}'")),
        },
        Request::Boost { seconds } => {
            sh.send(Cmd::Boost(seconds));
            Response::ok(match seconds {
                Some(s) => format!("Boost for {s:.0} s"),
                None => "Boost cancelled".into(),
            })
        }
        Request::Pause { on } => {
            sh.send(Cmd::Pause(on));
            Response::ok(if on { "Paused — BIOS has control" } else { "Resumed" })
        }
        Request::Calibrate { fans } => {
            let ids: Vec<String> = if fans.is_empty() || fans.iter().any(|f| f == "all") {
                sh.snapshot().fans.iter().filter(|f| !f.hidden).map(|f| f.id.clone()).collect()
            } else {
                match fans.iter().map(|f| resolve_fan(sh, f).ok_or(f.clone())).collect::<Result<Vec<_>, _>>() {
                    Ok(v) => v,
                    Err(f) => return Response::err(format!("Unknown fan '{f}'")),
                }
            };
            let msg = format!("Calibrating {} (~90 s each)", ids.join(", "));
            sh.send(Cmd::Calibrate(ids));
            Response::ok(msg)
        }
        Request::CancelCalibration => {
            sh.send(Cmd::CancelCalibration);
            Response::ok("Calibration cancelled")
        }
        Request::Automation { on } => {
            sh.edit_config(|c| c.settings.automation = on);
            Response::ok(if on { "Automation on" } else { "Automation off" })
        }
        Request::Show { page } => {
            (hooks.show)(page);
            Response::ok("")
        }
        Request::Laptop { action } => match action.as_str() {
            "find" => {
                sh.send(Cmd::NbfcSearch);
                Response::ok("Searching nbfc-linux for this laptop's config (see hush laptop)")
            }
            "use" => match crate::hw::nbfc::install() {
                Ok(name) => {
                    sh.send(Cmd::Rescan);
                    Response::ok(format!("Using the NBFC config for {name}. Hush now drives the fans through the EC; hush laptop off undoes it"))
                }
                Err(e) => Response::err(e),
            },
            "off" => match crate::hw::nbfc::uninstall() {
                Ok(()) => {
                    sh.send(Cmd::Rescan);
                    Response::ok("Laptop config removed, fans are back with the firmware")
                }
                Err(e) => Response::err(e),
            },
            _ => {
                let s = sh.snapshot();
                match s.backends.iter().find(|(n, _, _)| n == "NBFC") {
                    Some((_, ok, d)) => Response { ok: *ok, message: d.clone(), data: None },
                    None => Response::ok("No search needed: Hush already controls these fans (or this isn't a laptop). hush laptop find searches anyway"),
                }
            }
        },
        Request::Quit => {
            (hooks.quit)();
            Response::ok("Hush is exiting — fans returned to BIOS control")
        }
    }
}

/// Serves the pipe forever on a background thread.
pub fn serve(sh: Arc<Shared>, hooks: Hooks) {
    std::thread::Builder::new()
        .name("hush-ipc".into())
        .spawn(move || {
            // SYSTEM + Administrators full, interactive/authenticated users read/write, so the
            // non-elevated CLI can talk to the elevated app.
            let mut sd = PSECURITY_DESCRIPTOR::default();
            unsafe {
                let _ = ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    w!("D:(A;;GA;;;SY)(A;;GA;;;BA)(A;;GRGW;;;AU)"),
                    SDDL_REVISION_1,
                    &mut sd,
                    None,
                );
            }
            let sa = SECURITY_ATTRIBUTES { nLength: size_of::<SECURITY_ATTRIBUTES>() as u32, lpSecurityDescriptor: sd.0, bInheritHandle: false.into() };
            loop {
                let h: HANDLE = unsafe {
                    CreateNamedPipeW(
                        w!(r"\\.\pipe\hush"),
                        PIPE_ACCESS_DUPLEX,
                        PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                        PIPE_UNLIMITED_INSTANCES,
                        1 << 16,
                        1 << 16,
                        0,
                        Some(&sa),
                    )
                };
                if h == INVALID_HANDLE_VALUE || h.is_invalid() {
                    crate::log(format!("CreateNamedPipe failed: {:?}", unsafe { windows::Win32::Foundation::GetLastError() }));
                    std::thread::sleep(std::time::Duration::from_secs(1));
                    continue;
                }
                if unsafe { ConnectNamedPipe(h, None) }.is_err() && unsafe { windows::Win32::Foundation::GetLastError() }.0 != 535 {
                    let _ = unsafe { windows::Win32::Foundation::CloseHandle(h) };
                    continue;
                }
                let file = unsafe { std::fs::File::from_raw_handle(h.0) };
                let mut reader = BufReader::new(&file);
                let mut line = String::new();
                let resp = match reader.read_line(&mut line) {
                    Ok(_) => match serde_json::from_str::<Request>(line.trim()) {
                        Ok(req) => handle(&sh, &hooks, req),
                        Err(e) => Response::err(format!("bad request: {e}")),
                    },
                    Err(e) => Response::err(e.to_string()),
                };
                let mut w = &file;
                let mut out = serde_json::to_vec(&resp).unwrap_or_default();
                out.push(b'\n');
                let _ = w.write_all(&out);
                unsafe {
                    let _ = FlushFileBuffers(h);
                    let _ = DisconnectNamedPipe(h);
                }
                drop(file);
            }
        })
        .expect("ipc thread");
}

/// Client side: sends one request and waits for the answer.
pub fn call(req: &Request) -> Result<Response, String> {
    let t0 = std::time::Instant::now();
    let mut f = loop {
        match std::fs::OpenOptions::new().read(true).write(true).open(PIPE) {
            Ok(f) => break f,
            // All instances busy (231) or between instances (2 briefly) → retry.
            Err(e) if e.raw_os_error() == Some(231) && t0.elapsed().as_secs_f32() < 3.0 => std::thread::sleep(std::time::Duration::from_millis(15)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err("Hush is not running".into()),
            Err(e) => return Err(format!("cannot reach Hush: {e}")),
        }
    };
    let mut line = serde_json::to_vec(req).map_err(|e| e.to_string())?;
    line.push(b'\n');
    f.write_all(&line).map_err(|e| e.to_string())?;
    // One line back. (Reading to EOF would fail with ERROR_PIPE_NOT_CONNECTED once the
    // server disconnects, so stop at the newline.)
    let mut buf = String::new();
    BufReader::new(&f).read_line(&mut buf).map_err(|e| e.to_string())?;
    serde_json::from_str(buf.trim()).map_err(|e| format!("bad response: {e}"))
}
