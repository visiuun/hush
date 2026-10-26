//! ASUS laptops (Zenbook, Vivobook, ROG, TUF) through the firmware's own WMI/ACPI interface,
//! the `\\.\ATKACPI` device that ships with the "ASUS System Control Interface" driver.
//!
//! Fans are driven by uploading a flat 8-point curve at the wanted duty, or the fan range when
//! the firmware refuses the curve, the same sequence G-Helper uses. Factory curves often can't
//! be read back, so support is only known after the first write: when both are refused (Zenbook
//! 14 UM3406, for one) Hush falls back to the firmware fan modes (Whisper / Standard /
//! Performance) and profiles pick the mode; Max, boost and critical temperature also set the
//! hidden force-full-speed switch (`DEVS` on the RPM device, bit 6 of the EC fan control byte),
//! which the Zenbook 14 UM3406 has for its CPU fan. Handing back = setting the original mode again,
//! which makes the firmware reload its factory curves. Device ids follow the Linux `asus-wmi`
//! driver and G-Helper.

use super::lpc::BiosState;
use super::{ChipReading, PlatformMode, SuperIo, TempReading};
use windows::Win32::Foundation::{CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE};
use windows::Win32::Storage::FileSystem::{CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING};
use windows::Win32::System::IO::DeviceIoControl;
use windows::core::w;

const IOCTL: u32 = 0x0022_240C;
const INIT: u32 = 0x5449_4E49;
const DSTS: u32 = 0x5354_5344;
const DEVS: u32 = 0x5356_4544;
const PRESENT: u32 = 0x0001_0000;
const UNSUPPORTED: u32 = 0xFFFF_FFFE;

/// (label, RPM device, curve device, range device, temperature that best matches it).
const FANS: [(&str, u32, u32, Option<u32>, usize); 3] = [
    ("CPU fan", 0x0011_0013, 0x0011_0024, Some(0x0011_0022), 0),
    ("GPU fan", 0x0011_0014, 0x0011_0025, Some(0x0011_0023), 1),
    ("Mid fan", 0x0011_0031, 0x0011_0032, None, 0),
];

/// G-Helper's balanced curves, for fans whose factory curve can't be read back.
const DEFAULT_CURVE: [[u8; 16]; 2] = [
    [0x3A, 0x3D, 0x40, 0x44, 0x48, 0x4D, 0x51, 0x62, 0x08, 0x11, 0x16, 0x1A, 0x22, 0x29, 0x30, 0x45],
    [0x3A, 0x3D, 0x40, 0x44, 0x48, 0x4D, 0x51, 0x62, 0x0C, 0x16, 0x1D, 0x1F, 0x26, 0x2D, 0x34, 0x4A],
];
const TEMPS: [(&str, &str, u32); 2] = [("cpu", "CPU (EC)", 0x0012_0094), ("gpu", "GPU (EC)", 0x0012_0097)];

/// ROG / TUF thermal policy: 0 balanced, 1 turbo, 2 silent.
const POLICY: u32 = 0x0012_0075;
/// Vivobook / Zenbook fan mode: 0 standard, 1 whisper, 2 performance.
const POLICY_VIVO: u32 = 0x0011_0019;

/// Curve temperatures. The last point is always 100 %, so even if Hush dies with a curve
/// uploaded the firmware still ramps the fans up when the machine gets really hot.
const CURVE_T: [u8; 8] = [20, 30, 40, 50, 60, 70, 85, 95];

struct Acpi(HANDLE);

unsafe impl Send for Acpi {}

impl Acpi {
    fn open() -> Option<Self> {
        unsafe {
            CreateFileW(w!(r"\\.\ATKACPI"), (GENERIC_READ | GENERIC_WRITE).0, FILE_SHARE_READ | FILE_SHARE_WRITE, None, OPEN_EXISTING, FILE_ATTRIBUTE_NORMAL, None)
                .ok()
                .map(Self)
        }
    }

    fn call(&self, method: u32, args: &[u8]) -> Result<[u8; 16], String> {
        let mut buf = Vec::with_capacity(8 + args.len());
        buf.extend_from_slice(&method.to_le_bytes());
        buf.extend_from_slice(&(args.len() as u32).to_le_bytes());
        buf.extend_from_slice(args);
        let mut out = [0u8; 16];
        unsafe {
            DeviceIoControl(self.0, IOCTL, Some(buf.as_ptr().cast()), buf.len() as u32, Some(out.as_mut_ptr().cast()), 16, None, None)
                .map_err(|e| format!("ATKACPI: {}", e.message()))?;
        }
        Ok(out)
    }

    fn dsts_buf(&self, dev: u32, arg: u32) -> Result<[u8; 16], String> {
        self.call(DSTS, &[dev.to_le_bytes(), arg.to_le_bytes()].concat())
    }

    /// The device's value, `None` when the firmware doesn't have it.
    fn get(&self, dev: u32) -> Option<u32> {
        let v = u32::from_le_bytes(self.dsts_buf(dev, 0).ok()?[..4].try_into().ok()?);
        // The high bits can carry capability flags (0x28070002 on a Zenbook 14 fan mode):
        // only the presence bit counts, like in `asus-wmi`.
        (v != UNSUPPORTED && v & PRESENT != 0).then_some(v & 0xFFFF)
    }

    fn set(&self, dev: u32, args: &[u8]) -> Result<u32, String> {
        let out = self.call(DEVS, &[&dev.to_le_bytes()[..], args].concat())?;
        Ok(u32::from_le_bytes(out[..4].try_into().unwrap_or_default()))
    }
}

impl Drop for Acpi {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Policy {
    Rog,
    Vivo,
}

impl Policy {
    fn dev(self) -> u32 {
        match self {
            Self::Rog => POLICY,
            Self::Vivo => POLICY_VIVO,
        }
    }

    fn value(self, m: PlatformMode) -> u32 {
        match (self, m) {
            (_, PlatformMode::Balanced) => 0,
            (Self::Rog, PlatformMode::Performance | PlatformMode::Full) | (Self::Vivo, PlatformMode::Silent) => 1,
            (Self::Rog, PlatformMode::Silent) | (Self::Vivo, PlatformMode::Performance | PlatformMode::Full) => 2,
        }
    }

    /// Fan curve tables are indexed balanced / silent / turbo, whatever the policy says.
    fn curve_index(self, v: u32) -> u32 {
        match (self, v) {
            (Self::Rog, 1) | (Self::Vivo, 2) => 2,
            (Self::Rog, 2) | (Self::Vivo, 1) => 1,
            _ => 0,
        }
    }
}

struct Fan {
    label: &'static str,
    rpm_dev: u32,
    curve_dev: u32,
    range_dev: Option<u32>,
    temp: usize,
    /// Some firmwares always answer 0 RPM: until a real reading shows up, there is none.
    rpm_seen: bool,
    /// Force-full-speed switch is on.
    full: bool,
    /// Factory curve of the original mode (temps, duties), used to show the firmware's duty.
    factory: Option<[u8; 16]>,
    /// Last duty uploaded while Hush drives it.
    duty: Option<f32>,
}

pub struct Asus {
    acpi: Acpi,
    fans: Vec<Fan>,
    policy: Option<Policy>,
    /// The mode the firmware was in before Hush touched anything.
    original: u32,
    mode: Option<PlatformMode>,
    curves: bool,
    state: BiosState,
}

/// `Ok(None)` = not an ASUS machine with the ATKACPI driver.
pub fn probe() -> Result<Option<Box<dyn SuperIo>>, String> {
    let Some(acpi) = Acpi::open() else { return Ok(None) };
    let _ = acpi.call(INIT, &0u32.to_le_bytes());
    let policy = if acpi.get(POLICY).is_some() {
        Some(Policy::Rog)
    } else if acpi.get(POLICY_VIVO).is_some() {
        Some(Policy::Vivo)
    } else {
        None
    };
    let current = policy.and_then(|p| acpi.get(p.dev())).filter(|&v| v <= 2).unwrap_or(0);
    let mut state = BiosState::load("asus".into(), 1);
    // A curve or mode left behind by a previous run that didn't exit cleanly: the saved mode
    // is the real original one.
    let original = state.get(0).and_then(|s| s.first()).map(|&v| v as u32).unwrap_or(current);
    if let (Some(p), true) = (policy, state.is_taken(0)) {
        let _ = acpi.set(p.dev(), &original.to_le_bytes());
        for f in &FANS[..2] {
            let _ = acpi.set(f.1, &0u32.to_le_bytes());
        }
        state.release(0);
    }
    let curve_idx = policy.map(|p| p.curve_index(original)).unwrap_or(0);
    let fans: Vec<Fan> = FANS
        .iter()
        .filter(|f| acpi.get(f.1).is_some())
        .map(|&(label, rpm_dev, curve_dev, range_dev, temp)| {
            let factory = acpi.dsts_buf(curve_dev, curve_idx).ok().filter(valid_curve);
            Fan { label, rpm_dev, curve_dev, range_dev, temp, rpm_seen: false, full: false, factory, duty: None }
        })
        .collect();
    if fans.is_empty() {
        return Err("ASUS firmware interface found, but it reports no fans".into());
    }
    // Without a mode to go back to, an uploaded curve could never be handed back.
    let curves = policy.is_some();
    Ok(Some(Box::new(Asus { acpi, fans, policy, original, mode: None, curves, state })))
}

fn valid_curve(c: &[u8; 16]) -> bool {
    let (t, d) = c.split_at(8);
    t.windows(2).all(|w| w[0] < w[1]) && d.iter().all(|&v| v <= 100) && d.iter().any(|&v| v > 0)
}

/// Piecewise-linear duty of a firmware curve at `temp`.
fn curve_duty(c: &[u8; 16], temp: f32) -> f32 {
    let pts: Vec<(f32, f32)> = (0..8).map(|i| (c[i] as f32, c[i + 8] as f32)).collect();
    if temp <= pts[0].0 {
        return pts[0].1;
    }
    for w in pts.windows(2) {
        if temp <= w[1].0 {
            return w[0].1 + (w[1].1 - w[0].1) * (temp - w[0].0) / (w[1].0 - w[0].0);
        }
    }
    pts[7].1
}

impl Asus {
    /// Uploads a curve; when the firmware refuses it, the fan range G-Helper derives from it
    /// (first / last duty). The firmware answers 1 when it took the value.
    fn upload(&self, i: usize, curve: &[u8; 16]) -> Result<(), String> {
        let f = &self.fans[i];
        if self.acpi.set(f.curve_dev, curve) == Ok(1) {
            return Ok(());
        }
        let scale = |d: u8| (d.min(100) as u32 * 255 / 100) as u8;
        match f.range_dev.map(|r| self.acpi.set(r, &[scale(curve[8]), scale(curve[15])])) {
            Some(Ok(1)) => Ok(()),
            _ => Err("custom fan curves refused".into()),
        }
    }

    fn any_manual(&self) -> bool {
        self.fans.iter().any(|f| f.duty.is_some())
    }

    /// Remembers the original mode on disk before the first change.
    fn take(&mut self) {
        self.state.take(0, vec![self.original as u8]);
    }

    /// Flips the force-full-speed switch on every fan that has one (the firmware answers 1).
    fn set_full(&mut self, on: bool) {
        for f in self.fans.iter_mut().filter(|f| f.full != on && FANS[..2].iter().any(|d| d.1 == f.rpm_dev)) {
            if self.acpi.set(f.rpm_dev, &(on as u32).to_le_bytes()) == Ok(1) {
                f.full = on;
            }
        }
    }

    /// Sets the original mode again: the firmware reloads its own curves for every fan.
    fn hand_back(&mut self) -> Result<(), String> {
        self.set_full(false);
        if let Some(p) = self.policy {
            self.acpi.set(p.dev(), &self.original.to_le_bytes())?;
        }
        // The mode switch reloads the curves; give the range back its full span too.
        for f in self.fans.iter().filter(|f| f.duty.is_some()) {
            if let Some(r) = f.range_dev {
                let _ = self.acpi.set(r, &[0, 255]);
            }
        }
        for f in &mut self.fans {
            f.duty = None;
        }
        self.mode = None;
        self.state.release(0);
        Ok(())
    }
}

impl SuperIo for Asus {
    fn name(&self) -> String {
        let what = if self.curves {
            "fan curves"
        } else if self.policy.is_some() {
            "firmware fan modes (profiles pick Whisper / Standard / Performance, Max and boost force full speed)"
        } else {
            "monitoring only"
        };
        format!("ASUS laptop · {what}")
    }

    fn fans(&self) -> usize {
        self.fans.len()
    }

    fn label(&self, i: usize) -> String {
        self.fans[i].label.to_string()
    }

    fn controllable(&self, _i: usize) -> bool {
        self.curves
    }

    fn builtin(&self) -> bool {
        true
    }

    fn read(&mut self) -> Result<ChipReading, String> {
        let temps: Vec<TempReading> = TEMPS
            .iter()
            .filter_map(|&(key, name, dev)| {
                let v = self.acpi.get(dev)? as f32;
                (v > 0.0 && v < 120.0).then(|| TempReading { key: key.into(), name: name.into(), value: v })
            })
            .collect();
        let temp_of = |i: usize| temps.iter().find(|t| t.key == TEMPS[i].0).map(|t| t.value);
        let mut r = ChipReading::default();
        let mut any = false;
        for f in &mut self.fans {
            let raw = self.acpi.get(f.rpm_dev);
            any |= raw.is_some();
            f.rpm_seen |= raw.is_some_and(|v| v > 0);
            let rpm = raw.filter(|_| f.rpm_seen).map(|v| v as f32 * 100.0);
            let duty = match (f.duty, &f.factory, temp_of(f.temp)) {
                _ if f.full => 100.0,
                (Some(d), _, _) => d,
                (None, Some(c), Some(t)) if self.mode.is_none() => curve_duty(c, t),
                _ => 0.0,
            };
            r.rpm.push(rpm);
            r.duty.push(duty);
        }
        if !any {
            return Err("ATKACPI stopped answering".into());
        }
        r.temps = temps;
        Ok(r)
    }

    fn apply(&mut self, i: usize, duty: Option<f32>) -> Result<(), String> {
        if !self.curves {
            return Err("this model has no custom fan curves".into());
        }
        match duty {
            Some(d) => {
                self.take();
                let d = d.clamp(0.0, 100.0);
                let mut curve = [0u8; 16];
                curve[..8].copy_from_slice(&CURVE_T);
                for (k, v) in curve[8..].iter_mut().enumerate() {
                    *v = if k == 7 { 100 } else { d.round() as u8 };
                }
                if let Err(e) = self.upload(i, &curve) {
                    // Nothing was applied: fall back to the firmware modes for good.
                    self.curves = false;
                    let _ = self.hand_back();
                    return Err(format!("{e}: this model has no custom fan curves, profiles now switch the ASUS fan mode"));
                }
                self.fans[i].duty = Some(d);
                Ok(())
            }
            // The firmware can only take all of its curves back at once: re-upload the
            // factory curve for this fan while others stay with Hush.
            None if self.fans.iter().enumerate().any(|(k, f)| k != i && f.duty.is_some()) => {
                let c = self.fans[i].factory.unwrap_or(DEFAULT_CURVE[i.min(1)]);
                self.upload(i, &c)?;
                self.fans[i].duty = None;
                Ok(())
            }
            None if self.fans[i].duty.is_some() => self.hand_back(),
            None => Ok(()),
        }
    }

    fn is_manual(&self, i: usize) -> bool {
        self.fans[i].duty.is_some()
    }

    fn set_mode(&mut self, mode: Option<PlatformMode>) -> Result<(), String> {
        let Some(p) = self.policy else { return Ok(()) };
        if self.curves {
            return Ok(());
        }
        if mode == self.mode {
            return Ok(());
        }
        match mode {
            Some(m) => {
                self.take();
                if self.mode.map(|o| p.value(o)) != Some(p.value(m)) {
                    self.acpi.set(p.dev(), &p.value(m).to_le_bytes())?;
                }
                self.set_full(m == PlatformMode::Full);
                self.mode = Some(m);
                Ok(())
            }
            None => self.hand_back(),
        }
    }

    fn restore_all(&mut self) {
        if self.any_manual() || self.mode.is_some() || self.state.is_taken(0) {
            let _ = self.hand_back();
        }
    }

    fn dump(&mut self) -> Result<String, String> {
        let mut out = format!("policy={:?} original={} mode={:?} curves={}\n", self.policy.map(|p| p.dev()), self.original, self.mode, self.curves);
        for f in &self.fans {
            out += &format!("{}: rpm={:?} factory={:?}\n", f.label, self.acpi.get(f.rpm_dev), f.factory);
        }
        Ok(out)
    }
}

impl Drop for Asus {
    fn drop(&mut self) {
        self.restore_all();
    }
}
