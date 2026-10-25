//! Persistent configuration: fans, profiles, curves, automation rules, app settings.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Mix {
    #[default]
    Max,
    Avg,
}

/// Which temperature drives a curve. Several sensors combine with `mix`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Source {
    pub sensors: Vec<String>,
    pub mix: Mix,
}

impl Default for Source {
    fn default() -> Self {
        Self { sensors: vec!["cpu".into(), "gpu".into()], mix: Mix::Max }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Curve {
    pub source: Source,
    /// (°C, duty %) sorted by temperature.
    pub points: Vec<[f32; 2]>,
    /// Temperature must fall this far before the fan slows down (°C).
    pub hysteresis: f32,
    /// Exponential smoothing time constant for the source temperature (s).
    pub smoothing: f32,
    /// Rising temperatures are extrapolated this far ahead (s), so fans react before heat soaks.
    pub lookahead: f32,
    /// Max duty change per second when speeding up / slowing down (%/s).
    pub ramp_up: f32,
    pub ramp_down: f32,
    /// Floor while running (%).
    pub min_duty: f32,
    /// Allow the fan to stop when the curve asks for less than `stop_below`.
    pub zero_rpm: bool,
    pub stop_below: f32,
}

impl Default for Curve {
    fn default() -> Self {
        Self::preset(Preset::Balanced)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Preset {
    Silent,
    Balanced,
    Performance,
}

impl Curve {
    pub fn preset(p: Preset) -> Self {
        let points = match p {
            Preset::Silent => vec![[30.0, 20.0], [50.0, 25.0], [65.0, 38.0], [75.0, 60.0], [85.0, 100.0]],
            Preset::Balanced => vec![[30.0, 25.0], [50.0, 35.0], [65.0, 55.0], [75.0, 75.0], [85.0, 100.0]],
            Preset::Performance => vec![[30.0, 35.0], [45.0, 50.0], [60.0, 70.0], [70.0, 90.0], [80.0, 100.0]],
        };
        let (smoothing, ramp_down) = match p {
            Preset::Silent => (6.0, 3.0),
            Preset::Balanced => (4.0, 5.0),
            Preset::Performance => (2.0, 8.0),
        };
        Self {
            source: Source::default(),
            points,
            hysteresis: 3.0,
            smoothing,
            lookahead: 3.0,
            ramp_up: 25.0,
            ramp_down,
            min_duty: 20.0,
            zero_rpm: false,
            stop_below: 15.0,
        }
    }

    /// Piecewise-linear lookup, flat beyond the end points.
    pub fn eval(&self, t: f32) -> f32 {
        let p = &self.points;
        match p.len() {
            0 => 100.0,
            1 => p[0][1],
            _ => {
                if t <= p[0][0] {
                    return p[0][1];
                }
                for w in p.windows(2) {
                    if t <= w[1][0] {
                        let span = (w[1][0] - w[0][0]).max(1e-3);
                        return w[0][1] + (w[1][1] - w[0][1]) * (t - w[0][0]) / span;
                    }
                }
                p[p.len() - 1][1]
            }
        }
    }

    pub fn sort(&mut self) {
        self.points.sort_by(|a, b| a[0].total_cmp(&b[0]));
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum FanMode {
    /// Handed back to the BIOS Smart Fan.
    Bios,
    Fixed { duty: f32 },
    Curve(Curve),
}

impl FanMode {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Bios => "BIOS",
            Self::Fixed { .. } => "Fixed",
            Self::Curve(_) => "Curve",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Profile {
    pub name: String,
    pub fans: BTreeMap<String, FanMode>,
}

impl Default for Profile {
    fn default() -> Self {
        Self { name: "Balanced".into(), fans: BTreeMap::new() }
    }
}

/// Measured behaviour of one fan (from calibration).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Calibration {
    /// Duty at which a spinning fan stalls.
    pub stop_duty: f32,
    /// Duty needed to start a stopped fan.
    pub start_duty: f32,
    pub max_rpm: f32,
    /// (duty %, rpm) samples, descending sweep.
    pub table: Vec<[f32; 2]>,
}

impl Calibration {
    pub fn rpm_at(&self, duty: f32) -> Option<f32> {
        let mut t = self.table.clone();
        t.sort_by(|a, b| a[0].total_cmp(&b[0]));
        if t.len() < 2 {
            return None;
        }
        let d = duty.clamp(t[0][0], t[t.len() - 1][0]);
        for w in t.windows(2) {
            if d <= w[1][0] {
                let span = (w[1][0] - w[0][0]).max(1e-3);
                return Some(w[0][1] + (w[1][1] - w[0][1]) * (d - w[0][0]) / span);
            }
        }
        t.last().map(|p| p[1])
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FanInfo {
    pub name: String,
    pub hidden: bool,
    pub calibration: Option<Calibration>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Trigger {
    /// Any of these processes is running (exe names, case-insensitive, `.exe` optional).
    Process { names: Vec<String> },
    /// Local time window, "HH:MM" – "HH:MM" (may wrap midnight).
    Time { from: String, to: String },
    /// A fullscreen / exclusive app is in the foreground.
    Fullscreen,
    /// No keyboard/mouse input for `minutes`.
    Idle { minutes: u32 },
    /// A sensor is above a threshold.
    Above { sensor: String, temp: f32 },
}

impl Trigger {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Process { .. } => "App running",
            Self::Time { .. } => "Time of day",
            Self::Fullscreen => "Fullscreen app",
            Self::Idle { .. } => "Idle",
            Self::Above { .. } => "Temperature above",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Rule {
    pub enabled: bool,
    pub trigger: Trigger,
    pub profile: String,
}

impl Default for Rule {
    fn default() -> Self {
        Self { enabled: true, trigger: Trigger::Fullscreen, profile: "Performance".into() }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub tick_ms: u64,
    /// Any CPU/GPU sensor above this forces every Hush-controlled fan to 100 %.
    pub critical_temp: f32,
    /// Duty used when a curve's sensors disappear.
    pub failsafe_duty: f32,
    pub start_minimized: bool,
    pub close_to_tray: bool,
    /// Automation rules on/off (manual profile picks always win until cleared).
    pub automation: bool,
    pub notify_stall: bool,
    pub history_minutes: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            tick_ms: 1000,
            critical_temp: 90.0,
            failsafe_duty: 80.0,
            start_minimized: false,
            close_to_tray: true,
            automation: true,
            notify_stall: true,
            history_minutes: 30,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub version: u32,
    /// Profile used when no automation rule matches.
    pub base_profile: String,
    pub profiles: Vec<Profile>,
    pub rules: Vec<Rule>,
    pub fans: BTreeMap<String, FanInfo>,
    /// User names for sensors (id → name) and hidden sensors.
    pub sensor_names: BTreeMap<String, String>,
    pub hidden_sensors: Vec<String>,
    pub settings: Settings,
}

impl Default for Config {
    fn default() -> Self {
        let mk = |name: &str| Profile { name: name.into(), fans: BTreeMap::new() };
        Self {
            version: CONFIG_VERSION,
            base_profile: "Balanced".into(),
            profiles: vec![mk("Silent"), mk("Balanced"), mk("Performance"), mk(MAX_PROFILE)],
            rules: Vec::new(),
            fans: BTreeMap::new(),
            sensor_names: BTreeMap::new(),
            hidden_sensors: vec!["mb.auxtin0".into(), "mb.auxtin1".into(), "mb.auxtin2".into(), "mb.auxtin3".into(), "mb.auxtin4".into()],
            settings: Settings::default(),
        }
    }
}

pub fn dir() -> PathBuf {
    let base = std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
    base.join("Hush")
}

pub fn path() -> PathBuf {
    dir().join("config.json")
}

pub const CONFIG_VERSION: u32 = 2;
pub const MAX_PROFILE: &str = "Max";

impl Config {
    pub fn load() -> Self {
        let mut c: Self = std::fs::read_to_string(path()).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
        c.migrate();
        c
    }

    /// One-time upgrades for configs written by older versions.
    fn migrate(&mut self) {
        if self.version < 2 {
            // v2: "Max" profile — every visible fan pinned at 100 % (a permanent boost).
            if self.profile(MAX_PROFILE).is_none() {
                let fans = self.fans.iter().map(|(id, f)| (id.clone(), if f.hidden { FanMode::Bios } else { FanMode::Fixed { duty: 100.0 } })).collect();
                let at = self.profiles.iter().position(|p| p.name.eq_ignore_ascii_case("Performance")).map_or(self.profiles.len(), |i| i + 1);
                self.profiles.insert(at, Profile { name: MAX_PROFILE.into(), fans });
            }
        }
        self.version = CONFIG_VERSION;
    }

    /// Atomic write (temp file + rename) so a crash never leaves half a config.
    pub fn save(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(dir())?;
        let tmp = dir().join("config.json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(tmp, path())
    }

    pub fn profile(&self, name: &str) -> Option<&Profile> {
        self.profiles.iter().find(|p| p.name.eq_ignore_ascii_case(name))
    }

    pub fn profile_mut(&mut self, name: &str) -> Option<&mut Profile> {
        self.profiles.iter_mut().find(|p| p.name.eq_ignore_ascii_case(name))
    }

    pub fn fan_name(&self, id: &str, default: &str) -> String {
        self.fans.get(id).map(|f| f.name.trim()).filter(|n| !n.is_empty()).unwrap_or(default).to_string()
    }

    pub fn sensor_name(&self, id: &str, default: &str) -> String {
        self.sensor_names.get(id).map(|n| n.trim()).filter(|n| !n.is_empty()).unwrap_or(default).to_string()
    }

    /// First-run defaults for a newly seen fan: a curve per profile if it spins (and can be
    /// controlled), BIOS otherwise.
    pub fn adopt_fan(&mut self, id: &str, spinning: bool, controllable: bool) -> bool {
        if self.fans.contains_key(id) {
            return false;
        }
        self.fans.insert(id.into(), FanInfo { hidden: !spinning, ..Default::default() });
        for p in &mut self.profiles {
            let mode = if !spinning || !controllable {
                FanMode::Bios
            } else if p.name.eq_ignore_ascii_case(MAX_PROFILE) {
                FanMode::Fixed { duty: 100.0 }
            } else {
                FanMode::Curve(Curve::preset(match p.name.as_str() {
                    "Silent" => Preset::Silent,
                    "Performance" => Preset::Performance,
                    _ => Preset::Balanced,
                }))
            };
            p.fans.entry(id.into()).or_insert(mode);
        }
        true
    }
}

/// Parses "HH:MM" into minutes after midnight.
pub fn parse_hm(s: &str) -> Option<u32> {
    let (h, m) = s.trim().split_once(':')?;
    let (h, m): (u32, u32) = (h.parse().ok()?, m.parse().ok()?);
    (h < 24 && m < 60).then_some(h * 60 + m)
}
