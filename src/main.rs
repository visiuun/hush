//! `hush` — command-line control for the Hush fan-control daemon.

use clap::{Parser, Subcommand};
use hushcore::config::{Curve, FanMode, Preset};
use hushcore::engine::Snapshot;
use hushcore::ipc::{self, Request, Response};
use hushcore::winutil;
use std::time::{Duration, Instant};

#[derive(Parser)]
#[command(name = "hush", version, about = "Smart, quiet fan control. Run without arguments to open the app.")]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Temperatures, fans and the active profile
    Status {
        /// Machine-readable output
        #[arg(long)]
        json: bool,
        /// Refresh continuously
        #[arg(short, long)]
        watch: bool,
    },
    /// Show profiles, pin one (`hush profile silent`) or return to automatic (`hush profile auto`)
    Profile { name: Option<String> },
    /// Override a fan: a duty 0–100, `max`, `bios`, or a preset curve (`silent|balanced|performance`)
    Set { fan: String, value: String },
    /// Clear overrides (one fan, or all)
    Reset { fan: Option<String> },
    /// All fans to 100 % for a while (`hush boost 30`, `hush boost off`)
    Boost {
        #[arg(default_value = "60")]
        seconds: String,
    },
    /// Hand every fan back to the BIOS until `hush resume`
    Pause,
    /// Take control back after `hush pause`
    Resume,
    /// Measure start/stop duty and RPM range (`hush calibrate all`, `hush calibrate fan2 --cancel`)
    Calibrate {
        fans: Vec<String>,
        #[arg(long)]
        cancel: bool,
    },
    /// Turn automation rules on or off
    Automation { state: OnOff },
    /// Start Hush at logon (elevated, no UAC prompt)
    Startup { state: Option<OnOff> },
    /// Start the background app (tray only)
    Start,
    /// Open the window, optionally on a page (overview, fans, automation, settings)
    Show { page: Option<String> },
    /// Exit Hush; every fan returns to BIOS control
    Quit,
    /// Send a raw JSON request (see docs) and print the JSON reply
    Raw { json: String },
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum OnOff {
    On,
    Off,
}

const RESET: &str = "\x1b[0m";
const DIM: &str = "\x1b[90m";
const BOLD: &str = "\x1b[1m";
const RED: &str = "\x1b[38;5;203m";
const GREEN: &str = "\x1b[38;5;114m";
const AMBER: &str = "\x1b[38;5;221m";
const CYAN: &str = "\x1b[38;5;117m";

fn enable_vt() {
    use windows::Win32::System::Console::{ENABLE_VIRTUAL_TERMINAL_PROCESSING, GetConsoleMode, GetStdHandle, STD_OUTPUT_HANDLE, SetConsoleMode};
    unsafe {
        if let Ok(h) = GetStdHandle(STD_OUTPUT_HANDLE) {
            let mut m = Default::default();
            if GetConsoleMode(h, &mut m).is_ok() {
                let _ = SetConsoleMode(h, m | ENABLE_VIRTUAL_TERMINAL_PROCESSING);
            }
        }
    }
}

/// Lets the (elevated) app take the foreground when we ask it to show itself.
fn allow_foreground() {
    unsafe {
        let _ = windows::Win32::UI::WindowsAndMessaging::AllowSetForegroundWindow(windows::Win32::UI::WindowsAndMessaging::ASFW_ANY);
    }
}

fn fail(msg: &str) -> ! {
    eprintln!("{RED}✗{RESET} {msg}");
    std::process::exit(1);
}

fn call(req: Request) -> Response {
    match ipc::call(&req) {
        Ok(r) => r,
        Err(e) if e == "Hush is not running" => fail("Hush is not running — start it with `hush start`"),
        Err(e) => fail(&e),
    }
}

fn say(req: Request) {
    let r = call(req);
    if r.ok {
        if !r.message.is_empty() {
            println!("{GREEN}✓{RESET} {}", r.message);
        }
    } else {
        fail(&r.message);
    }
}

/// Starts the daemon if needed and waits until its pipe answers.
fn ensure_running() -> bool {
    if ipc::call(&Request::Ping).is_ok() {
        return true;
    }
    let exe = winutil::app_exe();
    if !exe.exists() {
        fail(&format!("{} not found", exe.display()));
    }
    if !winutil::start_via_task() && !winutil::run_elevated(&exe, "--tray") {
        fail("Hush needs administrator rights to talk to the fan controller (UAC was declined)");
    }
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(15) {
        if ipc::call(&Request::Ping).is_ok() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(150));
    }
    fail("Hush did not come up in time");
}

fn bar(pct: f32, width: usize) -> String {
    let filled = ((pct / 100.0).clamp(0.0, 1.0) * width as f32).round() as usize;
    format!("{}{DIM}{}{RESET}", "━".repeat(filled), "─".repeat(width - filled))
}

fn temp_col(t: f32) -> &'static str {
    match t {
        t if t >= 80.0 => RED,
        t if t >= 65.0 => AMBER,
        _ => CYAN,
    }
}

fn print_status(s: &Snapshot) {
    let mut flags = Vec::new();
    if s.paused {
        flags.push(format!("{AMBER}PAUSED{RESET}"));
    }
    if s.critical {
        flags.push(format!("{RED}CRITICAL{RESET}"));
    }
    if let Some(b) = s.boost_left {
        flags.push(format!("{AMBER}BOOST {b:.0}s{RESET}"));
    }
    println!(
        "{BOLD}hush{RESET}  profile {BOLD}{}{RESET} {DIM}({}){RESET}  {}",
        s.profile,
        s.profile_reason,
        flags.join(" ")
    );
    println!();
    for x in s.sensors.iter().filter(|x| !x.hidden) {
        println!("  {:<16} {}{:>5.1} °C{RESET}", x.name, temp_col(x.value), x.value);
    }
    println!();
    for f in s.fans.iter().filter(|f| !f.hidden) {
        let rpm = f.rpm.map(|r| format!("{r:>5.0} rpm")).unwrap_or_else(|| "    — rpm".into());
        let src = f.source_temp.map(|t| format!("{DIM}@ {t:.0} °C{RESET}")).unwrap_or_default();
        let stall = if f.stalled { format!(" {RED}STALL{RESET}") } else { String::new() };
        let ovr = if f.overridden { format!(" {AMBER}override{RESET}") } else { String::new() };
        println!(
            "  {:<16} {rpm}  {} {:>3.0} %  {DIM}{:<8}{RESET} {src}{ovr}{stall}",
            f.name,
            bar(f.duty, 16),
            f.duty,
            f.mode
        );
    }
    if let Some(c) = &s.calibration {
        println!("\n  {AMBER}calibrating{RESET} {} · {} · {:.0} %", c.fan, c.phase, c.progress * 100.0);
    }
    for a in &s.alerts {
        println!("  {AMBER}!{RESET} {a}");
    }
}

fn status(json: bool, watch: bool) {
    loop {
        // Right after start the engine may not have finished its first tick.
        let t0 = Instant::now();
        let data = loop {
            let d = call(Request::Status).data.unwrap_or_default();
            if d["seq"].as_u64().unwrap_or(0) > 0 || t0.elapsed() > Duration::from_secs(4) {
                break d;
            }
            std::thread::sleep(Duration::from_millis(150));
        };
        if json {
            println!("{}", if watch { data.to_string() } else { serde_json::to_string_pretty(&data).unwrap() });
        } else {
            let s: Snapshot = serde_json::from_value(data).unwrap_or_default();
            if watch {
                print!("\x1b[2J\x1b[H");
            }
            print_status(&s);
        }
        if !watch {
            break;
        }
        std::thread::sleep(Duration::from_millis(1000));
    }
}

fn parse_mode(v: &str) -> FanMode {
    let l = v.trim().trim_end_matches('%').to_lowercase();
    match l.as_str() {
        "bios" | "auto" => FanMode::Bios,
        "max" | "full" => FanMode::Fixed { duty: 100.0 },
        "silent" => FanMode::Curve(Curve::preset(Preset::Silent)),
        "balanced" => FanMode::Curve(Curve::preset(Preset::Balanced)),
        "performance" | "perf" => FanMode::Curve(Curve::preset(Preset::Performance)),
        _ => match l.parse::<f32>() {
            Ok(d) if (0.0..=100.0).contains(&d) => FanMode::Fixed { duty: d },
            _ => fail(&format!("'{v}' is not a duty (0–100), max, bios, silent, balanced or performance")),
        },
    }
}

fn main() {
    enable_vt();
    let cli = Cli::parse();
    let Some(cmd) = cli.cmd else {
        ensure_running();
        allow_foreground();
        let _ = ipc::call(&Request::Show { page: None });
        return;
    };
    match cmd {
        Command::Status { json, watch } => status(json, watch),
        Command::Profile { name: None } => {
            let r = call(Request::Profiles);
            let snap: Snapshot = call(Request::Status).data.and_then(|d| serde_json::from_value(d).ok()).unwrap_or_default();
            let names: Vec<String> = r.data.and_then(|d| serde_json::from_value(d["profiles"].clone()).ok()).unwrap_or_default();
            for n in names {
                let mark = if n == snap.profile { format!("{GREEN}●{RESET}") } else { format!("{DIM}○{RESET}") };
                println!("  {mark} {n}");
            }
            println!("{DIM}  active: {} ({}){RESET}", snap.profile, snap.profile_reason);
        }
        Command::Profile { name: Some(n) } => {
            let auto = matches!(n.to_lowercase().as_str(), "auto" | "automatic" | "none");
            say(Request::Profile { name: (!auto).then_some(n) })
        }
        Command::Set { fan, value } => say(Request::Set { fan, mode: parse_mode(&value) }),
        Command::Reset { fan } => say(Request::Reset { fan: fan.filter(|f| f != "all") }),
        Command::Boost { seconds } => {
            let s = match seconds.to_lowercase().as_str() {
                "off" | "stop" | "0" => None,
                v => Some(v.trim_end_matches('s').parse::<f32>().unwrap_or_else(|_| fail("boost takes seconds or `off`"))),
            };
            say(Request::Boost { seconds: s })
        }
        Command::Pause => say(Request::Pause { on: true }),
        Command::Resume => say(Request::Pause { on: false }),
        Command::Calibrate { fans, cancel } => {
            if cancel {
                say(Request::CancelCalibration)
            } else {
                say(Request::Calibrate { fans })
            }
        }
        Command::Automation { state } => say(Request::Automation { on: matches!(state, OnOff::On) }),
        Command::Startup { state: None } => {
            println!("start at logon: {}", if winutil::startup_enabled() { "on" } else { "off" });
        }
        Command::Startup { state: Some(s) } => {
            let on = matches!(s, OnOff::On);
            if winutil::is_elevated() {
                match winutil::set_startup(on) {
                    Ok(()) => println!("{GREEN}✓{RESET} start at logon {}", if on { "on" } else { "off" }),
                    Err(e) => fail(&e),
                }
            } else {
                // Creating a highest-privilege task needs admin once; the app does it for us.
                let exe = winutil::app_exe();
                if !winutil::run_elevated(&exe, if on { "--startup-on" } else { "--startup-off" }) {
                    fail("UAC was declined");
                }
                std::thread::sleep(Duration::from_secs(2));
                println!("start at logon: {}", if winutil::startup_enabled() { "on" } else { "off" });
            }
        }
        Command::Start => {
            ensure_running();
            println!("{GREEN}✓{RESET} Hush is running");
        }
        Command::Show { page } => {
            ensure_running();
            allow_foreground();
            say(Request::Show { page })
        }
        Command::Quit => match ipc::call(&Request::Quit) {
            Ok(r) => println!("{GREEN}✓{RESET} {}", r.message),
            Err(_) => println!("Hush is not running"),
        },
        Command::Raw { json } => {
            let req: Request = serde_json::from_str(&json).unwrap_or_else(|e| fail(&format!("bad request: {e}")));
            println!("{}", serde_json::to_string_pretty(&call(req)).unwrap());
        }
    }
}
