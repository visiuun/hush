//! Any laptop whose fans are run by the ACPI embedded controller, driven with a NoteBook
//! FanControl model config (the JSON format of nbfc-linux, which carries every NBFC model).
//!
//! The config says which EC registers hold the fan speed and how to take and release
//! control; the EC itself is reached through PawnIO's `LpcACPIEC` module (ports 0x62 / 0x66
//! only) under the shared `Access_EC` mutex. Writing the wrong registers can confuse the EC,
//! so this backend only runs when the user put a config in place on purpose.

use super::lpc::BiosState;
use super::pawnio::{GlobalMutex, LPC_ACPI_EC, Module, MutexGuard};
use super::{ChipReading, SuperIo};
use serde::Deserialize;
use std::path::PathBuf;

const DATA: u64 = 0x62;
const CMD: u64 = 0x66;
const OBF: u8 = 0x01;
const IBF: u8 = 0x02;
const RD_EC: u8 = 0x80;
const WR_EC: u8 = 0x81;

/// `%ProgramData%\Hush\laptop.json`.
pub fn config_path() -> PathBuf {
    let base = std::env::var_os("ProgramData").map(PathBuf::from).unwrap_or_else(|| ".".into());
    base.join("Hush").join("laptop.json")
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Config {
    #[serde(default)]
    notebook_model: String,
    #[serde(default)]
    read_write_words: bool,
    fan_configurations: Vec<FanConfig>,
    #[serde(default)]
    register_write_configurations: Vec<RegisterWrite>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct FanConfig {
    read_register: u8,
    write_register: u8,
    min_speed_value: u16,
    max_speed_value: u16,
    #[serde(default)]
    independent_read_min_max_values: bool,
    #[serde(default)]
    min_speed_value_read: u16,
    #[serde(default)]
    max_speed_value_read: u16,
    #[serde(default)]
    reset_required: bool,
    #[serde(default)]
    fan_speed_reset_value: u16,
    #[serde(default)]
    fan_display_name: Option<String>,
    #[serde(default)]
    fan_speed_percentage_overrides: Vec<Override>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Override {
    fan_speed_percentage: f32,
    fan_speed_value: u16,
    #[serde(default)]
    target_operation: Target,
}

#[derive(Deserialize, Default, PartialEq, Clone, Copy)]
enum Target {
    Read,
    Write,
    #[default]
    ReadWrite,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct RegisterWrite {
    #[serde(default)]
    write_mode: WriteMode,
    #[serde(default)]
    write_occasion: Occasion,
    register: u8,
    value: u16,
    #[serde(default)]
    reset_required: bool,
    #[serde(default)]
    reset_value: u16,
    #[serde(default)]
    reset_write_mode: WriteMode,
}

#[derive(Deserialize, Default, Clone, Copy)]
enum WriteMode {
    #[default]
    Set,
    And,
    Or,
}

#[derive(Deserialize, Default, PartialEq, Clone, Copy)]
enum Occasion {
    #[default]
    OnInitialization,
    OnWriteFanSpeed,
}

struct Io {
    module: Module,
    mutex: GlobalMutex,
    words: bool,
}

impl Io {
    fn lock(&self) -> Result<MutexGuard<'_>, String> {
        self.mutex.lock(500).ok_or_else(|| "EC busy (another tool is holding it)".to_string())
    }

    fn status(&self) -> Result<u8, String> {
        self.module.call1("ioctl_pio_read", &[CMD]).map(|v| v as u8)
    }

    fn wait(&self, ready: impl Fn(u8) -> bool) -> Result<(), String> {
        for _ in 0..2000 {
            if ready(self.status()?) {
                return Ok(());
            }
        }
        Err("EC timeout".into())
    }

    fn out(&self, port: u64, v: u8) -> Result<(), String> {
        self.module.call0("ioctl_pio_write", &[port, v as u64])
    }

    fn read_byte(&self, reg: u8) -> Result<u8, String> {
        self.wait(|s| s & IBF == 0)?;
        self.out(CMD, RD_EC)?;
        self.wait(|s| s & IBF == 0)?;
        self.out(DATA, reg)?;
        self.wait(|s| s & OBF != 0)?;
        self.module.call1("ioctl_pio_read", &[DATA]).map(|v| v as u8)
    }

    fn write_byte(&self, reg: u8, v: u8) -> Result<(), String> {
        self.wait(|s| s & IBF == 0)?;
        self.out(CMD, WR_EC)?;
        self.wait(|s| s & IBF == 0)?;
        self.out(DATA, reg)?;
        self.wait(|s| s & IBF == 0)?;
        self.out(DATA, v)?;
        self.wait(|s| s & IBF == 0)
    }

    /// The OS's own EC driver can interleave with us: retry a couple of times.
    fn read(&self, reg: u8) -> Result<u16, String> {
        let mut last = Err(String::new());
        for _ in 0..3 {
            last = (|| {
                let lo = self.read_byte(reg)? as u16;
                Ok(if self.words { lo | (self.read_byte(reg.wrapping_add(1))? as u16) << 8 } else { lo })
            })();
            if last.is_ok() {
                break;
            }
        }
        last
    }

    fn write(&self, reg: u8, v: u16) -> Result<(), String> {
        let mut last = Err(String::new());
        for _ in 0..3 {
            last = (|| {
                self.write_byte(reg, v as u8)?;
                if self.words { self.write_byte(reg.wrapping_add(1), (v >> 8) as u8) } else { Ok(()) }
            })();
            if last.is_ok() {
                break;
            }
        }
        last
    }

    fn write_mode(&self, reg: u8, v: u16, mode: WriteMode) -> Result<(), String> {
        let v = match mode {
            WriteMode::Set => v,
            WriteMode::And => self.read(reg)? & v,
            WriteMode::Or => self.read(reg)? | v,
        };
        self.write(reg, v)
    }
}

pub struct Ec {
    io: Io,
    cfg: Config,
    manual: Vec<bool>,
    initialized: bool,
    state: BiosState,
    mismatch: Option<String>,
}

/// `Ok(None)` = no laptop config in place.
/// Checks that a config is one Hush can run; returns its model name.
pub fn validate(text: &str) -> Result<String, String> {
    parse(text).map(|c| c.notebook_model)
}

fn parse(text: &str) -> Result<Config, String> {
    let json: serde_json::Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
    if json["FanConfigurations"].as_array().is_some_and(|f| f.iter().any(|f| f.get("ReadRegister").is_none())) {
        return Err("this config drives the fans through ACPI methods, which Hush doesn't support".into());
    }
    let cfg: Config = serde_json::from_value(json).map_err(|e| e.to_string())?;
    if cfg.fan_configurations.is_empty() {
        return Err("no FanConfigurations".into());
    }
    Ok(cfg)
}

pub fn probe(driver: bool) -> Result<Option<Box<dyn SuperIo>>, String> {
    let path = config_path();
    let Ok(text) = std::fs::read_to_string(&path) else { return Ok(None) };
    let cfg = parse(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    if !driver {
        return Err("laptop EC config found, but the PawnIO driver is not installed".into());
    }
    let mutex = GlobalMutex::open(GlobalMutex::EC).ok_or("cannot open EC mutex")?;
    let io = Io { module: Module::load(LPC_ACPI_EC)?, mutex, words: cfg.read_write_words };
    let product = system_product();
    let (model, prod) = (cfg.notebook_model.to_ascii_lowercase(), product.to_ascii_lowercase());
    let mismatch = (!model.is_empty() && !prod.is_empty() && !prod.contains(&model) && !model.contains(&prod))
        .then(|| format!("config is for {}, this is {product}", cfg.notebook_model));
    let n = cfg.fan_configurations.len();
    let mut ec = Ec { io, cfg, manual: vec![false; n], initialized: false, state: BiosState::load("ec".into(), n), mismatch };
    // Left in manual mode by a run that didn't exit cleanly.
    if (0..n).any(|i| ec.state.is_taken(i)) {
        let _ = ec.release_all();
    }
    {
        let _g = ec.io.lock()?;
        ec.io.read(ec.cfg.fan_configurations[0].read_register).map_err(|e| format!("EC not answering: {e}"))?;
    }
    Ok(Some(Box::new(ec)))
}

/// SMBIOS system product name ("ZenBook UX3402ZA", "HP Pavilion Laptop 15-eg0xxx", …).
pub fn system_product() -> String {
    winreg::RegKey::predef(winreg::enums::HKEY_LOCAL_MACHINE)
        .open_subkey(r"HARDWARE\DESCRIPTION\System\BIOS")
        .and_then(|k| k.get_value::<String, _>("SystemProductName"))
        .unwrap_or_default()
        .trim()
        .to_string()
}

impl FanConfig {
    fn to_value(&self, pct: f32) -> u16 {
        if let Some(o) = self.fan_speed_percentage_overrides.iter().find(|o| o.target_operation != Target::Read && (o.fan_speed_percentage - pct).abs() < 0.5) {
            return o.fan_speed_value;
        }
        let (lo, hi) = (self.min_speed_value as f32, self.max_speed_value as f32);
        (lo + (hi - lo) * pct.clamp(0.0, 100.0) / 100.0).round() as u16
    }

    fn to_pct(&self, v: u16) -> f32 {
        if let Some(o) = self.fan_speed_percentage_overrides.iter().find(|o| o.target_operation != Target::Write && o.fan_speed_value == v) {
            return o.fan_speed_percentage;
        }
        let (lo, hi) = if self.independent_read_min_max_values {
            (self.min_speed_value_read, self.max_speed_value_read)
        } else {
            (self.min_speed_value, self.max_speed_value)
        };
        if lo == hi {
            return 0.0;
        }
        ((v as f32 - lo as f32) / (hi as f32 - lo as f32) * 100.0).clamp(0.0, 100.0)
    }
}

impl Ec {
    fn register_writes(&self, occasion: Occasion) -> Result<(), String> {
        for w in self.cfg.register_write_configurations.iter().filter(|w| w.write_occasion == occasion) {
            self.io.write_mode(w.register, w.value, w.write_mode)?;
        }
        Ok(())
    }

    fn release(&mut self, i: usize) -> Result<(), String> {
        let f = &self.cfg.fan_configurations[i];
        if f.reset_required {
            let _g = self.io.lock()?;
            self.io.write(f.write_register, f.fan_speed_reset_value)?;
        }
        self.manual[i] = false;
        self.state.release(i);
        if !self.manual.iter().any(|&m| m) {
            self.reset_registers()?;
        }
        Ok(())
    }

    fn reset_registers(&mut self) -> Result<(), String> {
        let _g = self.io.lock()?;
        for w in self.cfg.register_write_configurations.iter().filter(|w| w.reset_required) {
            self.io.write_mode(w.register, w.reset_value, w.reset_write_mode)?;
        }
        self.initialized = false;
        Ok(())
    }

    fn release_all(&mut self) -> Result<(), String> {
        let mut res = Ok(());
        for i in 0..self.manual.len() {
            self.manual[i] = true;
            if let Err(e) = self.release(i) {
                res = Err(e);
            }
        }
        res
    }
}

impl SuperIo for Ec {
    fn name(&self) -> String {
        let model = if self.cfg.notebook_model.is_empty() { "laptop".to_string() } else { self.cfg.notebook_model.clone() };
        match &self.mismatch {
            Some(m) => format!("EC · {model} (NBFC config; {m})"),
            None => format!("EC · {model} (NBFC config)"),
        }
    }

    fn fans(&self) -> usize {
        self.cfg.fan_configurations.len()
    }

    fn label(&self, i: usize) -> String {
        self.cfg.fan_configurations[i].fan_display_name.clone().filter(|s| !s.trim().is_empty()).unwrap_or_else(|| format!("Fan {}", i + 1))
    }

    fn builtin(&self) -> bool {
        true
    }

    fn read(&mut self) -> Result<ChipReading, String> {
        let _g = self.io.lock()?;
        let mut r = ChipReading::default();
        for f in &self.cfg.fan_configurations {
            let v = self.io.read(f.read_register)?;
            // EC speed values aren't RPM; the duty is what the config maps them to.
            r.rpm.push(None);
            r.duty.push(f.to_pct(v));
        }
        Ok(r)
    }

    fn apply(&mut self, i: usize, duty: Option<f32>) -> Result<(), String> {
        let Some(d) = duty else {
            return if self.manual[i] { self.release(i) } else { Ok(()) };
        };
        self.state.take(i, vec![1]);
        let _g = self.io.lock()?;
        if !self.initialized {
            self.register_writes(Occasion::OnInitialization)?;
            self.initialized = true;
        }
        self.register_writes(Occasion::OnWriteFanSpeed)?;
        let f = &self.cfg.fan_configurations[i];
        self.io.write(f.write_register, f.to_value(d))?;
        self.manual[i] = true;
        Ok(())
    }

    fn is_manual(&self, i: usize) -> bool {
        self.manual[i]
    }

    fn restore_all(&mut self) {
        if self.manual.iter().any(|&m| m) || self.initialized {
            let _ = self.release_all();
        }
    }

    fn dump(&mut self) -> Result<String, String> {
        let _g = self.io.lock()?;
        let mut out = String::new();
        for (i, f) in self.cfg.fan_configurations.iter().enumerate() {
            out += &format!("fan{}: read[{:#04x}]={:?} write[{:#04x}]={:?}\n", i + 1, f.read_register, self.io.read(f.read_register), f.write_register, self.io.read(f.write_register));
        }
        Ok(out)
    }
}

impl Drop for Ec {
    fn drop(&mut self) {
        self.restore_all();
    }
}
