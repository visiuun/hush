//! Nuvoton Super I/O hardware monitors.
//!
//! * NCT6779D … NCT6799D (ASUS, ASRock, some Gigabyte/Biostar): banked registers behind
//!   base+5 / base+6, 13-bit fan counters, per-header Smart Fan mode registers.
//! * NCT6683D / NCT6686D / NCT6687D (MSI): an embedded controller with a paged register
//!   window at base+4..6; fan writes go through a request / complete handshake.
//!
//! Register maps follow LibreHardwareMonitor and the Linux `nct6775` / `nct6683` drivers.

use super::lpc::{BiosState, Lpc, board};
use super::{ChipReading, SuperIo, TempReading};
use std::time::{Duration, Instant};

const CHIP_ID: u8 = 0x20;
const CHIP_REV: u8 = 0x21;
const HWM_LDN: u8 = 0x0B;
const IO_SPACE_LOCK: u8 = 0x28;
const NUVOTON_VENDOR: u16 = 0x5CA3;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Model {
    Nct6779D,
    Nct6791D,
    Nct6792D,
    Nct6793D,
    Nct6795D,
    Nct6796D,
    Nct6796DR,
    Nct6797D,
    Nct6798D,
    Nct6799D,
    Nct6683D,
    Nct6686D,
    Nct6687D,
    /// MSI 800-series boards: different EC firmware, monitored read-only.
    Nct6687DR,
}

impl Model {
    fn identify(id: u8, rev: u8) -> Option<Self> {
        Some(match (id, rev) {
            (0xC5, r) if r & 0xF0 == 0x60 => Self::Nct6779D,
            (0xC8, 0x03) => Self::Nct6791D,
            (0xC9, 0x11 | 0x13) => Self::Nct6792D,
            (0xD1, 0x21) => Self::Nct6793D,
            (0xD3, 0x52) => Self::Nct6795D,
            (0xD4, 0x23) => Self::Nct6796D,
            (0xD4, 0x2A) => Self::Nct6796DR,
            (0xD4, 0x51) => Self::Nct6797D,
            (0xD4, 0x2B) => Self::Nct6798D,
            (0xD8, 0x02) => Self::Nct6799D,
            (0xC7, 0x32) => Self::Nct6683D,
            (0xD4, 0x40 | 0x41) => Self::Nct6686D,
            (0xD5, 0x92) => {
                let (vendor, product) = board();
                let msi = vendor.contains("Micro-Star") || vendor.eq_ignore_ascii_case("MSI");
                let p = product.to_uppercase();
                if msi && ["B840", "B850", "B860", "X870", "Z890"].iter().any(|s| p.contains(s)) { Self::Nct6687DR } else { Self::Nct6687D }
            }
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Nct6779D => "Nuvoton NCT6779D",
            Self::Nct6791D => "Nuvoton NCT6791D",
            Self::Nct6792D => "Nuvoton NCT6792D",
            Self::Nct6793D => "Nuvoton NCT6793D",
            Self::Nct6795D => "Nuvoton NCT6795D",
            Self::Nct6796D => "Nuvoton NCT6796D",
            Self::Nct6796DR => "Nuvoton NCT6796D-R",
            Self::Nct6797D => "Nuvoton NCT6797D",
            Self::Nct6798D => "Nuvoton NCT6798D",
            Self::Nct6799D => "Nuvoton NCT6799D",
            Self::Nct6683D => "Nuvoton NCT6683D",
            Self::Nct6686D => "Nuvoton NCT6686D",
            Self::Nct6687D => "Nuvoton NCT6687D",
            Self::Nct6687DR => "Nuvoton NCT6687D-R",
        }
    }

    fn is_ec(self) -> bool {
        matches!(self, Self::Nct6683D | Self::Nct6686D | Self::Nct6687D | Self::Nct6687DR)
    }
}

/// Probes one slot for a Nuvoton chip. `Ok(None)` = nothing Nuvoton there.
pub fn probe(slot: u8) -> Result<Option<Box<dyn SuperIo>>, String> {
    let lpc = Lpc::open(slot)?;
    let (model, base) = {
        let _g = lpc.lock(500)?;
        lpc.key(&[0x87, 0x87])?;
        let exit = || lpc.key(&[0xAA]);
        let id = lpc.cfg_inb(CHIP_ID)?;
        let rev = lpc.cfg_inb(CHIP_REV)?;
        if id == 0 || id == 0xFF {
            let _ = exit();
            return Ok(None);
        }
        let Some(model) = Model::identify(id, rev) else {
            let _ = exit();
            // Winbond/Nuvoton/Fintek all answer this key; only report ids that look Nuvoton.
            return if (0xC0..=0xDF).contains(&id) { Err(format!("unsupported Nuvoton chip {id:02X}{rev:02X}")) } else { Ok(None) };
        };
        lpc.find_bars()?;
        let base = lpc.ldn_base(HWM_LDN, 0x60);
        if base.is_ok() && !model.is_ec() && model != Model::Nct6779D {
            let opt = lpc.cfg_inb(IO_SPACE_LOCK)?;
            if opt & 0x10 != 0 {
                lpc.cfg_outb(IO_SPACE_LOCK, opt & !0x10)?;
            }
        }
        exit()?;
        (model, base.map_err(|e| format!("{}: {e}", model.name()))?)
    };
    if model.is_ec() {
        Ok(Some(Box::new(NctEc::new(lpc, model, base)?)))
    } else {
        Ok(Some(Box::new(Nct::new(lpc, model, base)?)))
    }
}

// =============================================================================================
// NCT67xx
// =============================================================================================

const FAN_COUNT: [u16; 7] = [0x4B0, 0x4B2, 0x4B4, 0x4B6, 0x4B8, 0x4BA, 0x4CC];
const PWM_CMD: [u16; 7] = [0x109, 0x209, 0x309, 0x809, 0x909, 0xA09, 0xB09];
const FAN_MODE: [u16; 7] = [0x102, 0x202, 0x302, 0x802, 0x902, 0xA02, 0xB02];
const PWM_OUT_A: [u16; 7] = [0x001, 0x003, 0x011, 0x013, 0x015, 0x017, 0x029];
const PWM_OUT_B: [u16; 7] = [0x001, 0x003, 0x011, 0x013, 0x015, 0xA09, 0xB09];

/// (value register, half-degree register, half bit (-1 = none), source-select register)
type Slot = (u16, u16, i8, u16);

const SLOTS_A: &[Slot] = &[
    (0x027, 0, -1, 0x621),
    (0x073, 0x074, 7, 0x100),
    (0x075, 0x076, 7, 0x200),
    (0x077, 0x078, 7, 0x300),
    (0x079, 0x07A, 7, 0x800),
    (0x07B, 0x07C, 7, 0x900),
    (0x150, 0x151, 7, 0x622),
];

const SLOTS_B: &[Slot] = &[
    (0x073, 0x074, 7, 0x100),
    (0x075, 0x076, 7, 0x200),
    (0x077, 0x078, 7, 0x300),
    (0x079, 0x07A, 7, 0x800),
    (0x07B, 0x07C, 7, 0x900),
    (0x07D, 0x07E, 7, 0xA00),
    (0x4A0, 0x49E, 6, 0xB00),
    (0x4A2, 0x4A1, 7, 0xC00),
    (0x027, 0, -1, 0x621),
    (0x150, 0x151, 7, 0x622),
    (0x670, 0, -1, 0xC26),
    (0x672, 0, -1, 0xC27),
];

/// Stable key + human name for a temperature source id.
fn source_name(src: u8) -> Option<(&'static str, &'static str)> {
    Some(match src {
        1 => ("systin", "Motherboard"),
        2 => ("cputin", "CPU Socket"),
        3 => ("auxtin0", "Auxiliary 0"),
        4 => ("auxtin1", "Auxiliary 1"),
        5 => ("auxtin2", "Auxiliary 2"),
        6 => ("auxtin3", "Auxiliary 3"),
        7 => ("auxtin4", "Auxiliary 4"),
        8 => ("tsi0", "CPU (SB-TSI)"),
        9 => ("tsi1", "SMBus Master 1"),
        10 => ("tsensor", "T-Sensor"),
        16 => ("peci0", "CPU (PECI)"),
        17 => ("peci1", "PECI 1"),
        18..=21 => ("pch", "Chipset"),
        26 => ("byte0", "Byte Temp 0"),
        27 => ("byte1", "Byte Temp 1"),
        _ => return None,
    })
}

pub struct Nct {
    lpc: Lpc,
    model: Model,
    base: u16,
    fans: usize,
    pwm_out: &'static [u16; 7],
    slots: &'static [Slot],
    /// Per header: (mode register, PWM command) before takeover.
    state: BiosState,
}

struct Bank<'a> {
    nct: &'a Nct,
    bank: Option<u8>,
    _g: super::pawnio::MutexGuard<'a>,
}

impl Bank<'_> {
    fn select(&mut self, addr: u16) -> Result<(), String> {
        let (lpc, base) = (&self.nct.lpc, self.nct.base);
        let bank = (addr >> 8) as u8;
        if self.bank != Some(bank) {
            lpc.outb(base + 5, 0x4E)?;
            lpc.outb(base + 6, bank)?;
            self.bank = Some(bank);
        }
        lpc.outb(base + 5, (addr & 0xFF) as u8)
    }

    fn read(&mut self, addr: u16) -> Result<u8, String> {
        self.select(addr)?;
        self.nct.lpc.inb(self.nct.base + 6)
    }

    fn write(&mut self, addr: u16, v: u8) -> Result<(), String> {
        self.select(addr)?;
        self.nct.lpc.outb(self.nct.base + 6, v)
    }
}

impl Nct {
    fn new(lpc: Lpc, model: Model, base: u16) -> Result<Self, String> {
        let fans = match model {
            Model::Nct6779D => 5,
            Model::Nct6791D | Model::Nct6792D | Model::Nct6793D | Model::Nct6795D | Model::Nct6796D => 6,
            _ => 7,
        };
        let pwm_out = if matches!(model, Model::Nct6797D | Model::Nct6798D | Model::Nct6799D) { &PWM_OUT_B } else { &PWM_OUT_A };
        let slots = match model {
            Model::Nct6779D | Model::Nct6791D | Model::Nct6792D | Model::Nct6793D | Model::Nct6795D => SLOTS_A,
            _ => SLOTS_B,
        };
        let key = format!("{}@{base:#x}", model.name());
        let me = Self { lpc, model, base, fans, pwm_out, slots, state: BiosState::load(key, fans) };
        let vendor = {
            let mut s = me.bank()?;
            ((s.read(0x804F)? as u16) << 8) | s.read(0x004F)? as u16
        };
        if vendor != NUVOTON_VENDOR {
            return Err(format!("{} at {base:#x}: vendor id {vendor:#06x} is not Nuvoton", model.name()));
        }
        Ok(me)
    }

    fn bank(&self) -> Result<Bank<'_>, String> {
        Ok(Bank { nct: self, bank: None, _g: self.lpc.lock(250)? })
    }

    fn set_duty(&mut self, i: usize, pct: f32) -> Result<(), String> {
        let raw = (pct.clamp(0.0, 100.0) * 2.55).round() as u8;
        let mut s = self.bank()?;
        let mode = s.read(FAN_MODE[i])?;
        let cur = if self.state.is_taken(i) { None } else { Some(vec![mode, s.read(PWM_CMD[i])?]) };
        // Manual mode = high nibble 0; the low nibble holds unrelated tolerance bits.
        if mode >> 4 != 0 {
            s.write(FAN_MODE[i], mode & 0x0F)?;
        }
        s.write(PWM_CMD[i], raw)?;
        drop(s);
        if let Some(c) = cur {
            self.state.take(i, c);
        }
        Ok(())
    }

    fn restore(&mut self, i: usize) -> Result<(), String> {
        let Some(&[mode, cmd]) = self.state.get(i) else { return Ok(()) };
        let mut s = self.bank()?;
        s.write(FAN_MODE[i], mode)?;
        s.write(PWM_CMD[i], cmd)?;
        drop(s);
        self.state.release(i);
        Ok(())
    }
}

impl SuperIo for Nct {
    fn name(&self) -> String {
        format!("{} @ {:#x}", self.model.name(), self.base)
    }

    fn fans(&self) -> usize {
        self.fans
    }

    fn read(&mut self) -> Result<ChipReading, String> {
        let mut s = self.bank()?;
        let mut r = ChipReading::default();
        for i in 0..self.fans {
            let hi = s.read(FAN_COUNT[i])? as u32;
            let lo = s.read(FAN_COUNT[i] + 1)? as u32;
            let count = (hi << 5) | (lo & 0x1F);
            r.rpm.push(match count {
                0x1FFF.. => Some(0.0),
                0x15.. => Some(1.35e6 / count as f32),
                _ => None,
            });
            r.duty.push(s.read(self.pwm_out[i])? as f32 / 2.55);
        }
        let mut seen = Vec::new();
        for &(reg, half_reg, half_bit, src_reg) in self.slots {
            let src = s.read(src_reg)? & 0x1F;
            if src == 0 || seen.contains(&src) {
                continue;
            }
            let mut v = ((s.read(reg)? as i8) as i32) << 1;
            if half_bit > 0 {
                v |= ((s.read(half_reg)? >> half_bit) & 1) as i32;
            }
            let t = v as f32 * 0.5;
            if (-55.0..=125.0).contains(&t) && t != 0.0 {
                if let Some((key, name)) = source_name(src) {
                    seen.push(src);
                    r.temps.push(TempReading { key: key.into(), name: name.into(), value: t });
                }
            }
        }
        Ok(r)
    }

    fn apply(&mut self, i: usize, duty: Option<f32>) -> Result<(), String> {
        match duty {
            Some(d) => self.set_duty(i, d),
            None => self.restore(i),
        }
    }

    fn is_manual(&self, i: usize) -> bool {
        self.state.is_taken(i)
    }

    fn restore_all(&mut self) {
        for i in 0..self.fans {
            let _ = self.restore(i);
        }
    }

    fn dump(&mut self) -> Result<String, String> {
        let mut s = self.bank()?;
        let mut out = String::new();
        for i in 0..self.fans {
            let mode = s.read(FAN_MODE[i])?;
            let cmd = s.read(PWM_CMD[i])?;
            let pwm = s.read(self.pwm_out[i])?;
            let src = s.read(FAN_MODE[i] - 2)?;
            out += &format!("fan{}: mode={mode:#04x} cmd={cmd:#04x} out={pwm:#04x} src={src:#04x}\n", i + 1);
        }
        Ok(out)
    }
}

impl Drop for Nct {
    fn drop(&mut self) {
        self.restore_all();
    }
}

// =============================================================================================
// NCT668x (MSI embedded controller)
// =============================================================================================

const EC_FANS: usize = 8;
const EC_RPM: u16 = 0x140;
const EC_PWM_OUT: u16 = 0x160;
const EC_TEMP: u16 = 0x100;
const EC_MODE: u16 = 0xA00;
const EC_REQUEST: u16 = 0xA01;
const EC_CMD: u16 = 0xA28;
const EC_REQ_BEGIN: u8 = 0x80;
const EC_REQ_DONE: u8 = 0x40;
const EC_LABELS: [&str; EC_FANS] = ["CPU Fan", "Pump Fan", "System Fan 1", "System Fan 2", "System Fan 3", "System Fan 4", "System Fan 5", "System Fan 6"];
const EC_TEMPS: [(&str, &str); 7] =
    [("cpu", "CPU"), ("system", "Motherboard"), ("mos", "VRM MOS"), ("pch", "Chipset"), ("socket", "CPU Socket"), ("pcie1", "PCIe 1"), ("m2_1", "M.2 1")];

pub struct NctEc {
    lpc: Lpc,
    model: Model,
    base: u16,
    /// Per header: (manual bit as it was, PWM command) before takeover.
    state: BiosState,
}

struct Page<'a> {
    ec: &'a NctEc,
    _g: super::pawnio::MutexGuard<'a>,
}

impl Page<'_> {
    /// The EC owns the page register while it is anything but 0xFF; wait for it to let go.
    fn acquire(&self) -> Result<(), String> {
        let (lpc, base) = (&self.ec.lpc, self.ec.base);
        let t0 = Instant::now();
        while lpc.inb(base + 4)? != 0xFF {
            if t0.elapsed() > Duration::from_millis(500) {
                lpc.outb(base + 4, 0xFF)?;
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        Ok(())
    }

    fn read(&self, addr: u16) -> Result<u8, String> {
        let (lpc, base) = (&self.ec.lpc, self.ec.base);
        self.acquire()?;
        lpc.outb(base + 4, (addr >> 8) as u8)?;
        lpc.outb(base + 5, addr as u8)?;
        let v = lpc.inb(base + 6);
        lpc.outb(base + 4, 0xFF)?;
        v
    }

    fn write(&self, addr: u16, v: u8) -> Result<(), String> {
        let (lpc, base) = (&self.ec.lpc, self.ec.base);
        self.acquire()?;
        lpc.outb(base + 4, (addr >> 8) as u8)?;
        lpc.outb(base + 5, addr as u8)?;
        let r = lpc.outb(base + 6, v);
        lpc.outb(base + 4, 0xFF)?;
        r
    }
}

impl NctEc {
    fn new(lpc: Lpc, model: Model, base: u16) -> Result<Self, String> {
        let key = format!("{}@{base:#x}", model.name());
        let me = Self { lpc, model, base, state: BiosState::load(key, EC_FANS) };
        {
            let p = me.page()?;
            // Start the monitoring engine if firmware left it off.
            let init = p.read(0x180)?;
            if init & 0x80 == 0 {
                p.write(0x180, init | 0x80)?;
            }
        }
        Ok(me)
    }

    fn page(&self) -> Result<Page<'_>, String> {
        Ok(Page { ec: self, _g: self.lpc.lock(250)? })
    }
}

impl SuperIo for NctEc {
    fn name(&self) -> String {
        format!("{} @ {:#x}", self.model.name(), self.base)
    }

    fn fans(&self) -> usize {
        EC_FANS
    }

    fn label(&self, i: usize) -> String {
        EC_LABELS[i].to_string()
    }

    fn controllable(&self, _i: usize) -> bool {
        self.model != Model::Nct6687DR
    }

    fn read(&mut self) -> Result<ChipReading, String> {
        let p = self.page()?;
        let mut r = ChipReading::default();
        for i in 0..EC_FANS as u16 {
            let v = ((p.read(EC_RPM + i * 2)? as u16) << 8) | p.read(EC_RPM + i * 2 + 1)? as u16;
            r.rpm.push(Some(if v >= 0xFFF0 { 0.0 } else { v as f32 }));
            r.duty.push(p.read(EC_PWM_OUT + i)? as f32 / 2.55);
        }
        for (i, (key, name)) in EC_TEMPS.iter().enumerate() {
            let reg = EC_TEMP + i as u16 * 2;
            let t = p.read(reg)? as i8 as f32 + 0.5 * (p.read(reg + 1)? >> 7) as f32;
            if t > 0.0 && t < 125.0 {
                r.temps.push(TempReading { key: (*key).into(), name: (*name).into(), value: t });
            }
        }
        Ok(r)
    }

    fn apply(&mut self, i: usize, duty: Option<f32>) -> Result<(), String> {
        self.apply_many(&[(i, duty)]).pop().unwrap_or(Ok(()))
    }

    /// All changes of one tick go through a single request / complete handshake.
    fn apply_many(&mut self, changes: &[(usize, Option<f32>)]) -> Vec<Result<(), String>> {
        if !self.controllable(0) {
            return changes.iter().map(|_| Err("fan control is not supported on this chip yet".into())).collect();
        }
        let work: Vec<_> = changes.iter().filter(|(i, d)| d.is_some() || self.state.is_taken(*i)).copied().collect();
        if work.is_empty() {
            return changes.iter().map(|_| Ok(())).collect();
        }
        let res = (|| -> Result<Vec<(usize, Option<Vec<u8>>, bool)>, String> {
            let p = self.page()?;
            p.write(EC_REQUEST, EC_REQ_BEGIN)?;
            std::thread::sleep(Duration::from_millis(50));
            let mut mode = p.read(EC_MODE)?;
            let mut done = Vec::new();
            let mut cmds = Vec::new();
            for &(i, duty) in &work {
                let bit = 1u8 << i;
                match duty {
                    Some(d) => {
                        let first = (!self.state.is_taken(i)).then(|| p.read(EC_CMD + i as u16).map(|c| vec![mode & bit, c])).transpose()?;
                        mode |= bit;
                        cmds.push((i, (d.clamp(0.0, 100.0) * 2.55).round() as u8));
                        done.push((i, first, false));
                    }
                    None => {
                        if let Some(&[was, cmd]) = self.state.get(i) {
                            mode = (mode & !bit) | (was & bit);
                            cmds.push((i, cmd));
                            done.push((i, None, true));
                        }
                    }
                }
            }
            p.write(EC_MODE, mode)?;
            for (i, c) in cmds {
                p.write(EC_CMD + i as u16, c)?;
            }
            p.write(EC_REQUEST, EC_REQ_DONE)?;
            std::thread::sleep(Duration::from_millis(50));
            Ok(done)
        })();
        match res {
            Ok(done) => {
                for (i, first, released) in done {
                    if let Some(f) = first {
                        self.state.take(i, f);
                    }
                    if released {
                        self.state.release(i);
                    }
                }
                changes.iter().map(|_| Ok(())).collect()
            }
            Err(e) => changes.iter().map(|_| Err(e.clone())).collect(),
        }
    }

    fn is_manual(&self, i: usize) -> bool {
        self.state.is_taken(i)
    }

    fn restore_all(&mut self) {
        let all: Vec<_> = (0..EC_FANS).filter(|&i| self.state.is_taken(i)).map(|i| (i, None)).collect();
        if !all.is_empty() {
            let _ = self.apply_many(&all);
        }
    }

    fn dump(&mut self) -> Result<String, String> {
        let p = self.page()?;
        let mut out = format!("mode={:#04x}\n", p.read(EC_MODE)?);
        for i in 0..EC_FANS as u16 {
            out += &format!("fan{}: cmd={:#04x} out={:#04x}\n", i + 1, p.read(EC_CMD + i)?, p.read(EC_PWM_OUT + i)?);
        }
        Ok(out)
    }
}

impl Drop for NctEc {
    fn drop(&mut self) {
        self.restore_all();
    }
}
