//! The control loop. Owns the hardware; everything else talks to it through `Shared`.
//!
//! Per fan, every tick:
//!   source temps → mix → EMA smoothing → hysteresis hold → slope look-ahead
//!   → curve → zero-RPM / floor → ramp limiting → start kick → write (only on change)
//! with global overrides for boost, critical temperature, lost sensors and calibration.

use crate::automation::Automation;
use crate::config::{Calibration, Config, Curve, FanMode, Mix};
use crate::hw::{self, Hub};
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::time::{Duration, Instant};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Cmd {
    /// Pin a profile (None = back to automatic selection).
    SetProfile(Option<String>),
    /// Temporary per-fan override on top of the profile (None = clear).
    Override { fan: String, mode: Option<FanMode> },
    ClearOverrides,
    /// All fans to 100 % for N seconds (None = cancel).
    Boost(Option<f32>),
    /// Hand every fan back to the BIOS while paused.
    Pause(bool),
    Calibrate(Vec<String>),
    CancelCalibration,
    Quit,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SensorView {
    pub id: String,
    pub name: String,
    pub group: String,
    pub value: f32,
    pub hidden: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct FanView {
    pub id: String,
    pub name: String,
    pub rpm: Option<f32>,
    /// Duty the chip reports right now.
    pub duty: f32,
    /// Duty Hush is aiming for (None when the BIOS is in charge).
    pub target: Option<f32>,
    pub mode: String,
    /// Temperature the curve is currently looking at (after smoothing / look-ahead).
    pub source_temp: Option<f32>,
    pub overridden: bool,
    pub stalled: bool,
    pub hidden: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CalibStatus {
    pub fan: String,
    pub phase: String,
    pub progress: f32,
    pub queue: usize,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Snapshot {
    pub seq: u64,
    pub sensors: Vec<SensorView>,
    pub fans: Vec<FanView>,
    pub profile: String,
    pub profile_reason: String,
    pub pinned_profile: Option<String>,
    pub boost_left: Option<f32>,
    pub paused: bool,
    pub critical: bool,
    pub calibration: Option<CalibStatus>,
    pub chip: Option<String>,
    pub backends: Vec<(String, bool, String)>,
    pub cpu_load: Option<f32>,
    pub gpu_load: Option<f32>,
    pub gpu_power: Option<f32>,
    pub alerts: Vec<String>,
    pub tick_ms: f32,
}

impl Snapshot {
    pub fn sensor(&self, id: &str) -> Option<&SensorView> {
        self.sensors.iter().find(|s| s.id == id)
    }
    pub fn fan(&self, id: &str) -> Option<&FanView> {
        self.fans.iter().find(|f| f.id == id)
    }
}

/// Rolling history for graphs. Series keys: `t:<sensor>`, `rpm:<fan>`, `duty:<fan>`.
#[derive(Default)]
pub struct History {
    pub t: VecDeque<f64>,
    pub series: HashMap<String, VecDeque<f32>>,
    pub cap: usize,
}

impl History {
    fn push(&mut self, t: f64, values: impl Iterator<Item = (String, f32)>) {
        self.t.push_back(t);
        let n = self.t.len();
        for (k, v) in values {
            let s = self.series.entry(k).or_default();
            // Series that appeared late are back-filled so indices line up with `t`.
            while s.len() + 1 < n {
                s.push_back(f32::NAN);
            }
            s.push_back(v);
        }
        for s in self.series.values_mut() {
            while s.len() < n {
                s.push_back(f32::NAN);
            }
        }
        while self.t.len() > self.cap.max(60) {
            self.t.pop_front();
            for s in self.series.values_mut() {
                s.pop_front();
            }
        }
    }

    pub fn get(&self, key: &str) -> Option<&VecDeque<f32>> {
        self.series.get(key)
    }
}

pub struct Shared {
    pub snap: RwLock<Arc<Snapshot>>,
    pub history: RwLock<History>,
    pub config: RwLock<Config>,
    /// Bump after editing `config` so the engine re-reads and persists it.
    pub config_rev: AtomicU64,
    pub tx: Mutex<Sender<Cmd>>,
    pub quit: AtomicBool,
    /// Set once every fan has been handed back to the BIOS on exit.
    pub stopped: AtomicBool,
    pub started: Instant,
    on_update: Mutex<Option<Box<dyn Fn() + Send>>>,
}

impl Shared {
    pub fn send(&self, c: Cmd) {
        let _ = self.tx.lock().send(c);
    }

    pub fn snapshot(&self) -> Arc<Snapshot> {
        self.snap.read().clone()
    }

    pub fn edit_config(&self, f: impl FnOnce(&mut Config)) {
        f(&mut self.config.write());
        self.config_rev.fetch_add(1, Ordering::AcqRel);
    }

    pub fn set_on_update(&self, f: impl Fn() + Send + 'static) {
        *self.on_update.lock() = Some(Box::new(f));
    }
}

#[derive(Default)]
struct Ctl {
    ema: Option<f32>,
    slope: f32,
    held: f32,
    /// Last commanded duty (None = BIOS).
    out: Option<f32>,
    stopped: bool,
    kick_until: Option<Instant>,
    written: Option<Option<u8>>,
    written_at: Option<Instant>,
    lost_ticks: u32,
    stall_ticks: u32,
    seen_spinning: bool,
    eff_temp: Option<f32>,
}

enum CalPhase {
    Max { until: Instant },
    Sweep { step: usize, until: Instant },
    Start { duty: f32, until: Instant },
}

const SWEEP: [f32; 15] = [90.0, 80.0, 70.0, 60.0, 50.0, 40.0, 35.0, 30.0, 25.0, 20.0, 15.0, 10.0, 6.0, 3.0, 0.0];

struct CalRun {
    fan: String,
    phase: CalPhase,
    samples: Vec<f32>,
    result: Calibration,
}

pub struct Engine {
    sh: Arc<Shared>,
    rx: Receiver<Cmd>,
    hub: Hub,
    cfg: Config,
    cfg_rev: u64,
    dirty_since: Option<Instant>,
    ctl: HashMap<String, Ctl>,
    overrides: BTreeMap<String, FanMode>,
    pinned: Option<String>,
    boost_until: Option<Instant>,
    paused: bool,
    critical: bool,
    cal_queue: VecDeque<String>,
    cal: Option<CalRun>,
    auto: Automation,
    sys: sysinfo::System,
    alerts: VecDeque<(Instant, String)>,
    seq: u64,
}

impl Engine {
    pub fn start() -> Arc<Shared> {
        let (tx, rx) = channel();
        let cfg = Config::load();
        let sh = Arc::new(Shared {
            snap: RwLock::new(Arc::new(Snapshot::default())),
            history: RwLock::new(History { cap: cfg.settings.history_minutes as usize * 60, ..Default::default() }),
            config: RwLock::new(cfg.clone()),
            config_rev: AtomicU64::new(0),
            tx: Mutex::new(tx),
            quit: AtomicBool::new(false),
            stopped: AtomicBool::new(false),
            started: Instant::now(),
            on_update: Mutex::new(None),
        });
        let sh2 = sh.clone();
        std::thread::Builder::new()
            .name("hush-engine".into())
            .spawn(move || {
                let mut e = Engine {
                    sh: sh2,
                    rx,
                    hub: Hub::open(),
                    cfg,
                    cfg_rev: 0,
                    dirty_since: None,
                    ctl: HashMap::new(),
                    overrides: BTreeMap::new(),
                    pinned: None,
                    boost_until: None,
                    paused: false,
                    critical: false,
                    cal_queue: VecDeque::new(),
                    cal: None,
                    auto: Automation::default(),
                    sys: sysinfo::System::new(),
                    alerts: VecDeque::new(),
                    seq: 0,
                };
                e.run();
            })
            .expect("engine thread");
        sh
    }

    fn run(&mut self) {
        let (mut sensors, mut fans, mut extra) = (Vec::new(), Vec::new(), hw::Extra::default());
        let mut last = Instant::now();
        let mut next = Instant::now();
        loop {
            // Commands are handled the moment they arrive; the tick keeps its own cadence.
            loop {
                let wait = next.saturating_duration_since(Instant::now());
                match self.rx.recv_timeout(wait) {
                    Ok(Cmd::Quit) => return self.shutdown(),
                    Ok(c) => {
                        self.handle(c);
                        // Re-evaluate right away so CLI/GUI changes feel instant.
                        next = Instant::now();
                    }
                    Err(RecvTimeoutError::Timeout) => break,
                    Err(RecvTimeoutError::Disconnected) => return self.shutdown(),
                }
            }
            if self.sh.quit.load(Ordering::Acquire) {
                return self.shutdown();
            }
            let tick = Duration::from_millis(self.cfg.settings.tick_ms.clamp(250, 5000));
            next = Instant::now() + tick;
            let now = Instant::now();
            let dt = (now - last).as_secs_f32().clamp(0.05, 5.0);
            last = now;

            self.sync_config(now);
            self.hub.read(&mut sensors, &mut fans, &mut extra);
            self.adopt(&fans);
            self.auto.evaluate(&self.cfg, &sensors, self.pinned.as_deref());
            self.update_critical(&sensors);
            let views = self.control(&sensors, &fans, dt, now);
            self.platform_mode();
            self.publish(&sensors, views, &extra, now);
        }
    }

    fn shutdown(&mut self) {
        self.hub.restore_all();
        self.sh.stopped.store(true, Ordering::Release);
        if self.dirty_since.is_some() {
            let _ = self.cfg.save();
        }
    }

    fn alert(&mut self, msg: String) {
        if !self.alerts.iter().any(|(_, m)| *m == msg) {
            self.alerts.push_back((Instant::now(), msg));
        }
    }

    fn handle(&mut self, c: Cmd) {
        match c {
            Cmd::SetProfile(p) => self.pinned = p.and_then(|p| self.cfg.profile(&p).map(|p| p.name.clone())),
            Cmd::Override { fan, mode: Some(m) } => {
                self.overrides.insert(fan, m);
            }
            Cmd::Override { fan, mode: None } => {
                self.overrides.remove(&fan);
            }
            Cmd::ClearOverrides => self.overrides.clear(),
            Cmd::Boost(s) => self.boost_until = s.map(|s| Instant::now() + Duration::from_secs_f32(s.clamp(1.0, 3600.0))),
            Cmd::Pause(p) => self.paused = p,
            Cmd::Calibrate(fans) => {
                for f in fans {
                    if !self.cal_queue.contains(&f) && self.cal.as_ref().is_none_or(|c| c.fan != f) {
                        self.cal_queue.push_back(f);
                    }
                }
            }
            Cmd::CancelCalibration => {
                self.cal_queue.clear();
                self.cal = None;
            }
            Cmd::Quit => {}
        }
    }

    fn sync_config(&mut self, now: Instant) {
        let rev = self.sh.config_rev.load(Ordering::Acquire);
        if rev != self.cfg_rev {
            self.cfg_rev = rev;
            self.cfg = self.sh.config.read().clone();
            self.sh.history.write().cap = self.cfg.settings.history_minutes as usize * 60_000 / self.cfg.settings.tick_ms.max(250) as usize;
            self.dirty_since = Some(now);
        }
        if self.dirty_since.is_some_and(|t| now - t > Duration::from_millis(800)) {
            self.dirty_since = None;
            if let Err(e) = self.cfg.save() {
                self.alert(format!("Could not save config: {e}"));
            }
        }
    }

    fn edit_cfg(&mut self, f: impl FnOnce(&mut Config)) {
        self.sh.edit_config(f);
        self.cfg = self.sh.config.read().clone();
        self.cfg_rev = self.sh.config_rev.load(Ordering::Acquire);
        self.dirty_since = Some(Instant::now());
    }

    /// Registers fans the first time we see them (with spin-aware defaults).
    fn adopt(&mut self, fans: &[hw::Fan]) {
        let new: Vec<(String, bool, bool)> = fans
            .iter()
            .filter(|f| !self.cfg.fans.contains_key(&f.id))
            .map(|f| (f.id.clone(), f.builtin || f.rpm.unwrap_or(0.0) > 100.0, f.controllable))
            .collect();
        if !new.is_empty() {
            self.edit_cfg(|c| {
                for (id, spinning, controllable) in &new {
                    c.adopt_fan(id, *spinning, *controllable);
                }
            });
        }
    }

    /// Laptops without direct fan control: the firmware's own mode follows the profile.
    fn platform_mode(&mut self) {
        let mode = if self.paused {
            None
        } else if self.critical || self.boost_until.is_some() {
            Some(hw::PlatformMode::Full)
        } else {
            Some(hw::PlatformMode::for_profile(&self.auto.active))
        };
        for e in self.hub.set_mode(mode) {
            self.alert(e);
        }
    }

    fn update_critical(&mut self, sensors: &[hw::Sensor]) {
        let crit = self.cfg.settings.critical_temp;
        let hot = sensors.iter().filter(|s| s.group == "CPU" || s.group == "GPU").map(|s| s.value).fold(f32::MIN, f32::max);
        if !self.critical && hot >= crit {
            self.critical = true;
            self.alert(format!("Critical temperature {hot:.0} °C — all fans at 100 %"));
        } else if self.critical && hot < crit - 5.0 {
            self.critical = false;
        }
    }

    fn mode_for(&self, fan: &str) -> (FanMode, bool) {
        if let Some(m) = self.overrides.get(fan) {
            return (m.clone(), true);
        }
        let mode = self.cfg.profile(&self.auto.active).and_then(|p| p.fans.get(fan)).cloned().unwrap_or(FanMode::Bios);
        (mode, false)
    }

    fn source_temp(sensors: &[hw::Sensor], c: &Curve) -> Option<f32> {
        let vals: Vec<f32> = c.source.sensors.iter().filter_map(|id| sensors.iter().find(|s| &s.id == id)).map(|s| s.value).collect();
        if vals.is_empty() {
            return None;
        }
        Some(match c.source.mix {
            Mix::Max => vals.iter().cloned().fold(f32::MIN, f32::max),
            Mix::Avg => vals.iter().sum::<f32>() / vals.len() as f32,
        })
    }

    fn control(&mut self, sensors: &[hw::Sensor], fans: &[hw::Fan], dt: f32, now: Instant) -> Vec<FanView> {
        let boost = self.boost_until.is_some_and(|t| t > now);
        if !boost {
            self.boost_until = None;
        }
        self.step_calibration(fans, now);
        let calibrating = self.cal.as_ref().map(|c| c.fan.clone());

        let mut views = Vec::with_capacity(fans.len());
        let mut writes: Vec<(String, Option<f32>)> = Vec::new();
        let mut new_alerts = Vec::new();
        for f in fans {
            let info = self.cfg.fans.get(&f.id).cloned().unwrap_or_default();
            let calib = info.calibration.clone();
            let (mode, overridden) = if f.controllable { self.mode_for(&f.id) } else { (FanMode::Bios, false) };
            let ctl = self.ctl.entry(f.id.clone()).or_default();
            let rpm = f.rpm.unwrap_or(0.0);
            if rpm > 100.0 {
                ctl.seen_spinning = true;
            }
            let mut mode_label = mode.label().to_string();

            // 1. What the profile / override wants (before global overrides).
            let wanted: Option<f32> = match &mode {
                FanMode::Bios => {
                    ctl.ema = None;
                    ctl.eff_temp = None;
                    None
                }
                FanMode::Fixed { duty } => {
                    ctl.ema = None;
                    ctl.eff_temp = None;
                    Some(*duty)
                }
                FanMode::Curve(c) => match Self::source_temp(sensors, c) {
                    Some(t) => {
                        ctl.lost_ticks = 0;
                        let tau = c.smoothing.max(0.0);
                        let a = if tau < 0.05 { 1.0 } else { 1.0 - (-dt / tau).exp() };
                        let prev = ctl.ema.unwrap_or(t);
                        let ema = prev + a * (t - prev);
                        if ctl.ema.is_none() {
                            ctl.held = ema;
                        }
                        let a2 = 1.0 - (-dt / 3.0f32).exp();
                        ctl.slope += a2 * ((ema - prev) / dt - ctl.slope);
                        ctl.ema = Some(ema);
                        ctl.held = ctl.held.clamp(ema, ema + c.hysteresis.max(0.0));
                        let eff = ctl.held + (ctl.slope.max(0.0) * c.lookahead).min(10.0);
                        ctl.eff_temp = Some(eff);
                        let mut d = c.eval(eff);
                        // Zero-RPM with its own hysteresis band.
                        if c.zero_rpm {
                            if d < c.stop_below || (ctl.stopped && d < c.stop_below + 5.0) {
                                d = 0.0;
                            }
                        } else {
                            let floor = c.min_duty.max(calib.as_ref().map_or(0.0, |k| k.stop_duty + 3.0));
                            d = d.max(floor);
                        }
                        Some(d.clamp(0.0, 100.0))
                    }
                    None => {
                        ctl.lost_ticks += 1;
                        if ctl.lost_ticks == 3 {
                            new_alerts.push(format!("{}: curve sensors unavailable — failsafe {:.0} %", info.name_or(&f.default_name), self.cfg.settings.failsafe_duty));
                        }
                        Some(self.cfg.settings.failsafe_duty)
                    }
                },
            };

            // 2. Global overrides.
            let forced = if !f.controllable {
                mode_label = "Read-only".into();
                None
            } else if calibrating.as_deref() == Some(&f.id) {
                mode_label = "Calibrating".into();
                self.cal.as_ref().map(|c| c.duty())
            } else if self.paused {
                mode_label = "Paused".into();
                None
            } else if self.critical && !info.hidden {
                mode_label = "Critical".into();
                Some(100.0)
            } else if boost && !info.hidden {
                mode_label = "Boost".into();
                Some(100.0)
            } else {
                None
            };
            let instant = forced.is_some() && !self.paused;
            let target = if self.paused { None } else { forced.or(wanted) };

            // 3. Ramp limiting + start kick.
            let out = match target {
                None => None,
                // Full speed is a safety action (Max profile, 100 % fixed): never ramp into it.
                Some(t) if instant || t >= 99.5 => Some(t),
                Some(t) => {
                    let (up, down) = match &mode {
                        FanMode::Curve(c) => (c.ramp_up, c.ramp_down),
                        _ => (40.0, 20.0),
                    };
                    let cur = ctl.out.unwrap_or(f.duty);
                    let step = if t > cur { (up.max(0.5) * dt).min(t - cur) } else { -(down.max(0.5) * dt).min(cur - t) };
                    Some(cur + step)
                }
            };
            let mut cmd = out;
            if let Some(o) = out {
                let was_stopped = ctl.stopped || (ctl.out == Some(0.0));
                if o <= 0.0 {
                    ctl.stopped = true;
                } else {
                    if was_stopped && ctl.seen_spinning {
                        let kick = calib.as_ref().map_or(45.0, |k| (k.start_duty + 5.0).max(30.0));
                        if o < kick {
                            ctl.kick_until = Some(now + Duration::from_secs(2));
                        }
                    }
                    ctl.stopped = false;
                }
                if ctl.kick_until.is_some_and(|k| k > now) {
                    let kick = calib.as_ref().map_or(45.0, |k| (k.start_duty + 5.0).max(30.0));
                    cmd = Some(o.max(kick));
                }
            } else {
                ctl.stopped = false;
                ctl.kick_until = None;
            }
            ctl.out = out;

            // 4. Stall detection (only for fans known to have a tachometer).
            let expect_spin = cmd.unwrap_or(f.duty) >= calib.as_ref().map_or(35.0, |k| k.stop_duty + 12.0);
            if ctl.seen_spinning && expect_spin && rpm < 1.0 && ctl.kick_until.is_none_or(|k| k < now) {
                ctl.stall_ticks += 1;
            } else {
                ctl.stall_ticks = 0;
            }
            let stalled = ctl.stall_ticks as f32 * dt > 8.0;
            if stalled && ctl.stall_ticks as f32 * dt <= 8.0 + dt * 1.5 && self.cfg.settings.notify_stall {
                new_alerts.push(format!("{} looks stalled (0 RPM at {:.0} %)", info.name_or(&f.default_name), cmd.unwrap_or(f.duty)));
            }

            // 5. Write only when the register value changes (or every 10 s to reassert).
            let raw = cmd.map(|d| (d.clamp(0.0, 100.0) * 2.55).round() as u8);
            let stale = ctl.written_at.is_none_or(|t| now - t > Duration::from_secs(10));
            if f.controllable && (ctl.written != Some(raw) || (raw.is_some() && stale)) {
                writes.push((f.id.clone(), cmd));
                ctl.written = Some(raw);
                ctl.written_at = Some(now);
            }

            views.push(FanView {
                id: f.id.clone(),
                name: info.name_or(&f.default_name),
                rpm: f.rpm,
                duty: f.duty,
                target: cmd,
                mode: mode_label,
                source_temp: ctl.eff_temp,
                overridden,
                stalled,
                hidden: info.hidden,
            });
        }
        for a in new_alerts {
            self.alert(a);
        }
        for (id, e) in self.hub.set_many(&writes) {
            self.alert(format!("{id}: {e}"));
            if let Some(c) = self.ctl.get_mut(&id) {
                c.written = None;
            }
        }
        views
    }

    fn step_calibration(&mut self, fans: &[hw::Fan], now: Instant) {
        if self.cal.is_none() {
            if let Some(f) = self.cal_queue.pop_front() {
                if let Some(x) = fans.iter().find(|x| x.id == f && x.builtin && x.rpm.is_none()) {
                    let name = self.cfg.fans.get(&x.id).map(|i| i.name_or(&x.default_name)).unwrap_or(x.default_name.clone());
                    self.alert(format!("{name}: no tachometer, nothing to calibrate"));
                } else if fans.iter().any(|x| x.id == f) {
                    self.cal = Some(CalRun {
                        fan: f,
                        phase: CalPhase::Max { until: now + Duration::from_secs(8) },
                        samples: Vec::new(),
                        result: Calibration::default(),
                    });
                }
            }
        }
        let Some(run) = &mut self.cal else { return };
        let Some(rpm) = fans.iter().find(|f| f.id == run.fan).map(|f| f.rpm.unwrap_or(0.0)) else {
            self.cal = None;
            return;
        };
        run.samples.push(rpm);
        let settled = |s: &Vec<f32>| {
            let n = s.len().min(3);
            s[s.len() - n..].iter().sum::<f32>() / n.max(1) as f32
        };
        let step_s = Duration::from_secs(4);
        let mut done = false;
        match run.phase {
            CalPhase::Max { until } if now >= until => {
                run.result.max_rpm = settled(&run.samples);
                run.result.table.push([100.0, run.result.max_rpm]);
                run.samples.clear();
                if run.result.max_rpm < 100.0 {
                    done = true; // no tachometer / nothing connected
                } else {
                    run.phase = CalPhase::Sweep { step: 0, until: now + step_s };
                }
            }
            CalPhase::Sweep { step, until } if now >= until => {
                let r = settled(&run.samples);
                run.samples.clear();
                run.result.table.push([SWEEP[step], r]);
                if r < 50.0 || step + 1 == SWEEP.len() {
                    run.result.stop_duty = if r < 50.0 { SWEEP[step.saturating_sub(1)].min(SWEEP[step] + 3.0) } else { 0.0 };
                    if r < 50.0 {
                        run.phase = CalPhase::Start { duty: SWEEP[step], until: now + step_s };
                    } else {
                        run.result.start_duty = 0.0;
                        done = true;
                    }
                } else {
                    run.phase = CalPhase::Sweep { step: step + 1, until: now + step_s };
                }
            }
            CalPhase::Start { duty, until } if now >= until => {
                let r = settled(&run.samples);
                run.samples.clear();
                if r > 50.0 || duty >= 100.0 {
                    run.result.start_duty = duty;
                    done = true;
                } else {
                    run.phase = CalPhase::Start { duty: (duty + 4.0).min(100.0), until: now + step_s };
                }
            }
            _ => {}
        }
        if done {
            let run = self.cal.take().unwrap();
            let id = run.fan.clone();
            let has_fan = run.result.max_rpm >= 100.0;
            let builtin = fans.iter().any(|f| f.id == id && f.builtin);
            let name = self.cfg.fans.get(&id).map(|f| f.name_or(&id)).unwrap_or(id.clone());
            self.edit_cfg(|c| {
                let e = c.fans.entry(id.clone()).or_default();
                e.calibration = has_fan.then_some(run.result.clone());
                if !has_fan && !builtin {
                    e.hidden = true;
                }
            });
            if let Some(c) = self.ctl.get_mut(&id) {
                c.written = None;
                c.out = None;
            }
            self.alert(if has_fan {
                format!(
                    "{name} calibrated: max {:.0} RPM · stops below {:.0} % · starts at {:.0} %",
                    run.result.max_rpm, run.result.stop_duty, run.result.start_duty
                )
            } else if builtin {
                format!("{name}: no RPM reading, calibration skipped")
            } else {
                format!("{name}: no fan detected on this header (hidden)")
            });
        }
    }

    fn publish(&mut self, sensors: &[hw::Sensor], fans: Vec<FanView>, extra: &hw::Extra, now: Instant) {
        self.seq += 1;
        self.sys.refresh_cpu_usage();
        let cpu_load = self.sys.global_cpu_usage();
        while self.alerts.front().is_some_and(|(t, _)| now - *t > Duration::from_secs(20)) {
            self.alerts.pop_front();
        }
        let b = &self.hub.backends;
        let backend = |n: &str, r: &Result<String, String>| match r {
            Ok(s) => (n.to_string(), true, s.clone()),
            Err(e) => (n.to_string(), false, e.clone()),
        };
        let sensors_v: Vec<SensorView> = sensors
            .iter()
            .map(|s| SensorView {
                id: s.id.clone(),
                name: self.cfg.sensor_name(&s.id, &s.name),
                group: s.group.clone(),
                value: s.value,
                hidden: self.cfg.hidden_sensors.contains(&s.id),
            })
            .collect();
        let t = (now - self.sh.started).as_secs_f64();
        {
            let mut h = self.sh.history.write();
            let vals = sensors_v
                .iter()
                .map(|s| (format!("t:{}", s.id), s.value))
                .chain(fans.iter().flat_map(|f| [(format!("rpm:{}", f.id), f.rpm.unwrap_or(f32::NAN)), (format!("duty:{}", f.id), f.duty)]));
            h.push(t, vals);
        }
        let cal = self.cal.as_ref().map(|c| CalibStatus {
            fan: c.fan.clone(),
            phase: match c.phase {
                CalPhase::Max { .. } => "Measuring max speed".into(),
                CalPhase::Sweep { step, .. } => format!("Sweeping down · {:.0} %", SWEEP[step]),
                CalPhase::Start { duty, .. } => format!("Finding start duty · {duty:.0} %"),
            },
            progress: match c.phase {
                CalPhase::Max { .. } => 0.05,
                CalPhase::Sweep { step, .. } => 0.1 + 0.6 * step as f32 / SWEEP.len() as f32,
                CalPhase::Start { duty, .. } => 0.7 + 0.3 * (duty / 60.0).min(1.0),
            },
            queue: self.cal_queue.len(),
        });
        let snap = Snapshot {
            seq: self.seq,
            sensors: sensors_v,
            fans,
            profile: self.auto.active.clone(),
            profile_reason: self.auto.reason.clone(),
            pinned_profile: self.pinned.clone(),
            boost_left: self.boost_until.map(|t| t.saturating_duration_since(now).as_secs_f32()),
            paused: self.paused,
            critical: self.critical,
            calibration: cal,
            chip: self.hub.chip_name(),
            backends: vec![backend("Fans", &b.superio), backend("CPU", &b.cpu), backend("GPU", &b.gpu)],
            cpu_load: Some(cpu_load),
            gpu_load: extra.gpu_util,
            gpu_power: extra.gpu_power_w,
            alerts: self.alerts.iter().map(|(_, m)| m.clone()).collect(),
            tick_ms: self.cfg.settings.tick_ms as f32,
        };
        *self.sh.snap.write() = Arc::new(snap);
        if let Some(f) = &*self.sh.on_update.lock() {
            f();
        }
    }
}

impl CalRun {
    fn duty(&self) -> f32 {
        match self.phase {
            CalPhase::Max { .. } => 100.0,
            CalPhase::Sweep { step, .. } => SWEEP[step],
            CalPhase::Start { duty, .. } => duty,
        }
    }
}

impl crate::config::FanInfo {
    pub fn name_or(&self, default: &str) -> String {
        if self.name.trim().is_empty() { default.to_string() } else { self.name.trim().to_string() }
    }
}
