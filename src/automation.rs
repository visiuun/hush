//! Picks the active profile: pinned (manual) → first matching rule → base profile.

use crate::config::{Config, Trigger, parse_hm};
use crate::hw::Sensor;
use std::collections::HashSet;
use std::time::{Duration, Instant};

pub struct Automation {
    pub active: String,
    pub reason: String,
    sys: sysinfo::System,
    procs: HashSet<String>,
    procs_at: Option<Instant>,
    /// Latched state of temperature rules (3 °C release hysteresis).
    above: Vec<bool>,
}

impl Default for Automation {
    fn default() -> Self {
        Self {
            active: String::new(),
            reason: String::new(),
            sys: sysinfo::System::new(),
            procs: HashSet::new(),
            procs_at: None,
            above: Vec::new(),
        }
    }
}

fn norm(name: &str) -> String {
    let n = name.trim().to_lowercase();
    n.strip_suffix(".exe").map(str::to_string).unwrap_or(n)
}

impl Automation {
    pub fn evaluate(&mut self, cfg: &Config, sensors: &[Sensor], pinned: Option<&str>) {
        let fallback = || cfg.profile(&cfg.base_profile).or(cfg.profiles.first()).map(|p| p.name.clone()).unwrap_or_default();
        if let Some(p) = pinned.and_then(|p| cfg.profile(p)) {
            self.active = p.name.clone();
            self.reason = "Pinned".into();
            return;
        }
        self.above.resize(cfg.rules.len(), false);
        if cfg.settings.automation {
            let needs_procs = cfg.rules.iter().any(|r| r.enabled && matches!(r.trigger, Trigger::Process { .. }));
            if needs_procs && self.procs_at.is_none_or(|t| t.elapsed() > Duration::from_secs(3)) {
                self.refresh_processes();
            }
            let mut fullscreen: Option<Option<String>> = None;
            for (i, r) in cfg.rules.iter().enumerate() {
                if !r.enabled || cfg.profile(&r.profile).is_none() {
                    continue;
                }
                let hit: Option<String> = match &r.trigger {
                    Trigger::Process { names } => {
                        names.iter().map(|n| norm(n)).find(|n| !n.is_empty() && self.procs.contains(n)).map(|n| format!("{n}.exe running"))
                    }
                    Trigger::Time { from, to } => {
                        let (Some(a), Some(b)) = (parse_hm(from), parse_hm(to)) else { continue };
                        use chrono::Timelike;
                        let now = chrono::Local::now();
                        let m = now.hour() * 60 + now.minute();
                        let inside = if a <= b { m >= a && m < b } else { m >= a || m < b };
                        inside.then(|| format!("{from}–{to}"))
                    }
                    Trigger::Fullscreen => fullscreen.get_or_insert_with(crate::winutil::fullscreen_app).clone().map(|a| format!("{a} fullscreen")),
                    Trigger::Idle { minutes } => {
                        let idle = crate::winutil::idle_seconds();
                        (idle >= *minutes as f32 * 60.0).then(|| format!("idle {:.0} min", idle / 60.0))
                    }
                    Trigger::Above { sensor, temp } => {
                        let v = sensors.iter().find(|s| &s.id == sensor).map(|s| s.value);
                        let on = match v {
                            Some(v) if v >= *temp => true,
                            Some(v) if self.above[i] && v > temp - 3.0 => true,
                            _ => false,
                        };
                        self.above[i] = on;
                        on.then(|| format!("{sensor} ≥ {temp:.0} °C"))
                    }
                };
                if let Some(why) = hit {
                    self.active = cfg.profile(&r.profile).unwrap().name.clone();
                    self.reason = why;
                    return;
                }
            }
        }
        self.active = fallback();
        self.reason = "Default".into();
    }

    fn refresh_processes(&mut self) {
        self.sys.refresh_processes_specifics(sysinfo::ProcessesToUpdate::All, true, sysinfo::ProcessRefreshKind::nothing());
        self.procs = self.sys.processes().values().map(|p| norm(&p.name().to_string_lossy())).collect();
        self.procs_at = Some(Instant::now());
    }
}
