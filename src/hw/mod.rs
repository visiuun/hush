//! Hardware hub: one owner for every backend (Super I/O chips, laptop firmware / EC, CPU, GPUs).
//! Lives on the engine thread only, so no locking around register access.

pub mod asus;
pub mod cpu;
pub mod ec;
pub mod gpu;
pub mod ite;
pub mod lpc;
pub mod nbfc;
pub mod nct;
pub mod pawnio;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Sensor {
    pub id: String,
    pub name: String,
    pub group: String,
    pub value: f32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Fan {
    pub id: String,
    pub default_name: String,
    pub rpm: Option<f32>,
    pub duty: f32,
    /// True while Hush is driving it (false = BIOS / firmware automatic).
    pub manual: bool,
    /// False for tachometer-only headers.
    pub controllable: bool,
    /// Reported by the platform (laptop firmware / EC config) rather than found on a header:
    /// always real, even while it sits at 0 RPM or has no tachometer at all.
    #[serde(default)]
    pub builtin: bool,
}

/// Firmware performance mode for platforms where the fans can't be driven directly
/// (most ASUS Zenbooks / Vivobooks): Hush profiles map onto it. `Full` is the performance mode
/// plus the firmware's force-full-speed switch, where there is one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlatformMode {
    Silent,
    Balanced,
    Performance,
    Full,
}

impl PlatformMode {
    pub fn for_profile(name: &str) -> Self {
        let n = name.to_ascii_lowercase();
        if n.contains("silent") || n.contains("quiet") || n.contains("whisper") {
            Self::Silent
        } else if n == crate::config::MAX_PROFILE.to_ascii_lowercase() {
            Self::Full
        } else if n.contains("performance") || n.contains("turbo") {
            Self::Performance
        } else {
            Self::Balanced
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Backends {
    pub superio: Result<String, String>,
    pub cpu: Result<String, String>,
    pub gpu: Result<String, String>,
}

/// Load/power figures shown in the UI (not used as fan sources unless picked).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Extra {
    pub gpu_power_w: Option<f32>,
    pub gpu_util: Option<f32>,
}

#[derive(Clone, Debug)]
pub struct TempReading {
    /// Stable key, unique within the chip.
    pub key: String,
    pub name: String,
    pub value: f32,
}

#[derive(Clone, Debug, Default)]
pub struct ChipReading {
    /// RPM per header; `None` = no tachometer / transient invalid count.
    pub rpm: Vec<Option<f32>>,
    /// Current PWM output per header, 0–100 %.
    pub duty: Vec<f32>,
    pub temps: Vec<TempReading>,
}

/// A motherboard fan controller.
pub trait SuperIo: Send {
    fn name(&self) -> String;
    fn fans(&self) -> usize;
    fn label(&self, i: usize) -> String {
        format!("Fan {}", i + 1)
    }
    fn controllable(&self, _i: usize) -> bool {
        true
    }
    fn read(&mut self) -> Result<ChipReading, String>;
    /// `Some(pct)` = manual duty, `None` = hand the header back to the BIOS.
    fn apply(&mut self, i: usize, duty: Option<f32>) -> Result<(), String>;
    fn apply_many(&mut self, changes: &[(usize, Option<f32>)]) -> Vec<Result<(), String>> {
        changes.iter().map(|&(i, d)| self.apply(i, d)).collect()
    }
    fn is_manual(&self, i: usize) -> bool;
    /// Laptop backends: the fans are part of the machine, not probed headers.
    fn builtin(&self) -> bool {
        false
    }
    /// `Some` = follow the active profile with the firmware's own mode, `None` = put the
    /// original mode back. Only backends without direct fan control act on it.
    fn set_mode(&mut self, _mode: Option<PlatformMode>) -> Result<(), String> {
        Ok(())
    }
    fn restore_all(&mut self);
    fn dump(&mut self) -> Result<String, String>;
}

struct Board {
    chip: Box<dyn SuperIo>,
    /// Global fan number of this chip's first header (fan ids are numbered across chips).
    first: usize,
    /// Sensor id prefix ("mb", "mb2", …).
    prefix: String,
    last: Option<ChipReading>,
    failures: u32,
}

pub struct Hub {
    boards: Vec<Board>,
    cpu: Option<cpu::CpuTemps>,
    gpus: Vec<gpu::Gpu>,
    pub backends: Backends,
}

/// Finds every supported Super I/O chip on both LPC slots.
fn detect_chips() -> (Vec<Box<dyn SuperIo>>, Vec<String>) {
    let mut chips = Vec::new();
    let mut report = Vec::new();
    for slot in 0..2u8 {
        match nct::probe(slot) {
            Ok(Some(c)) => {
                chips.push(c);
                continue;
            }
            Ok(None) => {}
            Err(e) => {
                report.push(e);
                continue;
            }
        }
        match ite::probe(slot) {
            Ok(Some(c)) => chips.push(c),
            Ok(None) => {}
            Err(e) => report.push(e),
        }
    }
    (chips, report)
}

/// Laptops: an NBFC-style EC config the user dropped in wins (it's an explicit choice), then
/// the ASUS firmware interface when no desktop controller was found.
fn detect_laptop(driver: bool, chips: &mut Vec<Box<dyn SuperIo>>, report: &mut Vec<String>) {
    match ec::probe(driver) {
        Ok(Some(c)) => {
            chips.insert(0, c);
            return;
        }
        Ok(None) => {}
        Err(e) => report.push(e),
    }
    if !chips.is_empty() {
        return;
    }
    match asus::probe() {
        Ok(Some(c)) => chips.push(c),
        Ok(None) => report.push(format!("laptop or OEM board? Add an NBFC config for your model at {}", ec::config_path().display())),
        Err(e) => report.push(e),
    }
}

fn describe(boards: &[Board]) -> String {
    boards.iter().map(|b| format!("{} · {} headers", b.chip.name(), b.chip.fans())).collect::<Vec<_>>().join(" + ")
}

impl Hub {
    pub fn open() -> Self {
        let driver = pawnio::installed();
        let no_driver = || Err("PawnIO driver is not installed".to_string());
        let (boards, sio) = {
            let (mut chips, mut report) = if driver { detect_chips() } else { (Vec::new(), vec!["PawnIO driver is not installed".to_string()]) };
            detect_laptop(driver, &mut chips, &mut report);
            let mut first = 0;
            let boards: Vec<Board> = chips
                .into_iter()
                .enumerate()
                .map(|(i, chip)| {
                    let b = Board { first, prefix: if i == 0 { "mb".into() } else { format!("mb{}", i + 1) }, chip, last: None, failures: 0 };
                    first += b.chip.fans();
                    b
                })
                .collect();
            if boards.is_empty() {
                let why = if report.is_empty() { "no supported fan controller found".to_string() } else { report.join("; ") };
                (boards, Err(why))
            } else {
                let d = describe(&boards);
                (boards, Ok(d))
            }
        };
        let (cpu, cpu_s) = if driver {
            match cpu::CpuTemps::open() {
                Ok(c) => {
                    let d = c.describe();
                    (Some(c), Ok(d))
                }
                Err(e) => (None, Err(e)),
            }
        } else {
            (None, no_driver())
        };
        let (gpus, gpu_s) = match gpu::Gpu::open_all() {
            Ok(g) => {
                let d = format!("{} · temperature only, fans stay with the driver", g.iter().map(|g| g.describe()).collect::<Vec<_>>().join(", "));
                (g, Ok(d))
            }
            Err(e) => (Vec::new(), Err(e)),
        };
        Self { boards, cpu, gpus, backends: Backends { superio: sio, cpu: cpu_s, gpu: gpu_s } }
    }

    pub fn chip_name(&self) -> Option<String> {
        (!self.boards.is_empty()).then(|| self.boards.iter().map(|b| b.chip.name()).collect::<Vec<_>>().join(" + "))
    }

    pub fn read(&mut self, sensors: &mut Vec<Sensor>, fans: &mut Vec<Fan>, extra: &mut Extra) {
        sensors.clear();
        fans.clear();
        let mut push = |id: &str, name: &str, group: &str, value: f32| {
            sensors.push(Sensor { id: id.into(), name: name.into(), group: group.into(), value });
        };
        if let Some(c) = &self.cpu {
            if let Ok(r) = c.read() {
                push("cpu", "CPU", "CPU", r.main);
                for (i, t) in r.dies.iter().enumerate() {
                    push(&format!("ccd{}", i + 1), &format!("CCD{}", i + 1), "CPU", *t);
                }
            }
        }
        for (i, g) in self.gpus.iter().enumerate() {
            if let Ok(r) = g.read() {
                let id = if i == 0 { "gpu".to_string() } else { format!("gpu{}", i + 1) };
                push(&id, &g.name, "GPU", r.temp);
                if i == 0 {
                    extra.gpu_power_w = r.power_w;
                    extra.gpu_util = r.util;
                }
            }
        }
        let mut status: Option<Result<(), String>> = None;
        for b in &mut self.boards {
            // Brief bus contention with other monitoring tools: reuse the last good reading.
            let reading = match b.chip.read() {
                Ok(r) => {
                    b.failures = 0;
                    b.last = Some(r.clone());
                    Ok(r)
                }
                Err(e) => {
                    b.failures += 1;
                    match &b.last {
                        Some(r) if b.failures < 5 => Ok(r.clone()),
                        _ => Err(e),
                    }
                }
            };
            match reading {
                Ok(r) => {
                    status.get_or_insert(Ok(()));
                    for t in &r.temps {
                        push(&format!("{}.{}", b.prefix, t.key), &t.name, "Motherboard", t.value);
                    }
                    for i in 0..b.chip.fans() {
                        fans.push(Fan {
                            id: format!("fan{}", b.first + i + 1),
                            default_name: if b.first == 0 { b.chip.label(i) } else { format!("Fan {}", b.first + i + 1) },
                            rpm: r.rpm.get(i).copied().flatten(),
                            duty: r.duty.get(i).copied().unwrap_or(0.0),
                            manual: b.chip.is_manual(i),
                            controllable: b.chip.controllable(i),
                            builtin: b.chip.builtin(),
                        });
                    }
                }
                Err(e) => status = Some(Err(format!("{}: {e}", b.chip.name()))),
            }
        }
        match status {
            // Re-described every time: a laptop backend can drop to firmware modes at runtime.
            Some(Ok(())) => self.backends.superio = Ok(describe(&self.boards)),
            Some(Err(e)) => self.backends.superio = Err(e),
            None => {}
        }
    }

    fn locate(&self, fan: &str) -> Result<(usize, usize), String> {
        let n = fan.strip_prefix("fan").and_then(|n| n.parse::<usize>().ok()).filter(|&n| n >= 1).ok_or(format!("unknown fan '{fan}'"))? - 1;
        self.boards
            .iter()
            .enumerate()
            .find(|(_, b)| n >= b.first && n < b.first + b.chip.fans())
            .map(|(bi, b)| (bi, n - b.first))
            .ok_or_else(|| if self.boards.is_empty() { "no fan controller".into() } else { format!("unknown fan '{fan}'") })
    }

    /// Applies one tick's worth of changes (`Some(pct)` = manual duty, `None` = BIOS).
    /// Returns the fans that failed.
    pub fn set_many(&mut self, changes: &[(String, Option<f32>)]) -> Vec<(String, String)> {
        let mut failed = Vec::new();
        let mut per: Vec<Vec<(usize, Option<f32>, &str)>> = (0..self.boards.len()).map(|_| Vec::new()).collect();
        for (id, d) in changes {
            match self.locate(id) {
                Ok((b, i)) => per[b].push((i, *d, id)),
                Err(e) => failed.push((id.clone(), e)),
            }
        }
        for (b, list) in per.into_iter().enumerate() {
            if list.is_empty() {
                continue;
            }
            let args: Vec<(usize, Option<f32>)> = list.iter().map(|(i, d, _)| (*i, *d)).collect();
            for (r, (_, _, id)) in self.boards[b].chip.apply_many(&args).into_iter().zip(&list) {
                if let Err(e) = r {
                    failed.push((id.to_string(), e));
                }
            }
        }
        failed
    }

    /// Forwards the wanted firmware mode to every backend; returns the errors.
    pub fn set_mode(&mut self, mode: Option<PlatformMode>) -> Vec<String> {
        self.boards.iter_mut().filter_map(|b| b.chip.set_mode(mode).err().map(|e| format!("{}: {e}", b.chip.name()))).collect()
    }

    pub fn restore_all(&mut self) {
        for b in &mut self.boards {
            b.chip.restore_all();
        }
    }

    pub fn dump(&mut self) -> String {
        self.boards.iter_mut().map(|b| format!("{}\n{}", b.chip.name(), b.chip.dump().unwrap_or_else(|e| e))).collect::<Vec<_>>().join("\n")
    }
}

impl Drop for Hub {
    fn drop(&mut self) {
        self.restore_all();
    }
}
