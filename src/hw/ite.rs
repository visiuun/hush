//! ITE IT86xx / IT87xx environment controllers (Gigabyte and many OEM boards).
//!
//! Only the chips with 16-bit fan counters and extended PWM duty registers are supported,
//! which covers everything from roughly 2012 on. Register map follows LibreHardwareMonitor and
//! the Linux `it87` driver.

use super::lpc::{BiosState, Lpc};
use super::{ChipReading, SuperIo, TempReading};

const ENV_LDN: u8 = 0x04;
const CONFIG: u8 = 0x00;
const BANK: u8 = 0x06;
const FAN_16BIT: u8 = 0x0C;
const FAN_16BIT_ALT: u8 = 0x0B;
const MAIN_CTRL: u8 = 0x13;
const TEMP_BASE: u8 = 0x29;
const VENDOR: u8 = 0x58;

const TACH: [u8; 6] = [0x0D, 0x0E, 0x0F, 0x80, 0x82, 0x4C];
const TACH_EXT: [u8; 6] = [0x18, 0x19, 0x1A, 0x81, 0x83, 0x4D];
const TACH_ALT: [u8; 6] = [0x0D, 0x0E, 0x0F, 0x80, 0x82, 0x93];
const TACH_EXT_ALT: [u8; 6] = [0x18, 0x19, 0x1A, 0x81, 0x83, 0x94];
const PWM_EXT: [u8; 6] = [0x63, 0x6B, 0x73, 0x7B, 0xA3, 0xAB];
const PWM_CTRL: [u8; 6] = [0x15, 0x16, 0x17, 0x7F, 0xA7, 0xAF];
const PWM_CTRL_ALT: [u8; 6] = [0x15, 0x16, 0x17, 0x1E, 0x1F, 0x92];

struct Spec {
    id: u16,
    name: &'static str,
    fans: usize,
    controls: usize,
    temps: u8,
}

const CHIPS: &[Spec] = &[
    Spec { id: 0x8613, name: "ITE IT8613E", fans: 5, controls: 4, temps: 4 },
    Spec { id: 0x8620, name: "ITE IT8620E", fans: 5, controls: 5, temps: 3 },
    Spec { id: 0x8625, name: "ITE IT8625E", fans: 6, controls: 6, temps: 3 },
    Spec { id: 0x8628, name: "ITE IT8628E", fans: 6, controls: 6, temps: 6 },
    Spec { id: 0x8631, name: "ITE IT8631E", fans: 2, controls: 2, temps: 2 },
    Spec { id: 0x8638, name: "ITE IT8638E", fans: 2, controls: 2, temps: 2 },
    Spec { id: 0x8655, name: "ITE IT8655E", fans: 3, controls: 3, temps: 6 },
    Spec { id: 0x8665, name: "ITE IT8665E", fans: 6, controls: 6, temps: 6 },
    Spec { id: 0x8686, name: "ITE IT8686E", fans: 6, controls: 5, temps: 7 },
    Spec { id: 0x8688, name: "ITE IT8688E", fans: 6, controls: 5, temps: 6 },
    Spec { id: 0x8689, name: "ITE IT8689E", fans: 6, controls: 6, temps: 6 },
    Spec { id: 0x8696, name: "ITE IT8696E", fans: 6, controls: 6, temps: 6 },
    Spec { id: 0x8721, name: "ITE IT8721F", fans: 5, controls: 3, temps: 3 },
    Spec { id: 0x8728, name: "ITE IT8728F", fans: 5, controls: 3, temps: 3 },
    Spec { id: 0x8772, name: "ITE IT8772E", fans: 5, controls: 3, temps: 3 },
    Spec { id: 0x8733, name: "ITE IT8792E", fans: 3, controls: 3, temps: 3 },
    Spec { id: 0x8695, name: "ITE IT87952E", fans: 3, controls: 3, temps: 3 },
];

/// Probes one slot for an ITE chip. `Ok(None)` = nothing ITE there.
pub fn probe(slot: u8) -> Result<Option<Box<dyn SuperIo>>, String> {
    let lpc = Lpc::open(slot)?;
    let (spec, base) = {
        let _g = lpc.lock(500)?;
        let secondary = lpc.port == 0x4E;
        // A secondary chip that is already in config mode must not be entered again (it
        // answers with a bogus id afterwards), and is never taken out of config mode.
        let mut id = if secondary { lpc.cfg_inw(0x20)? } else { 0xFFFF };
        if id == 0xFFFF || id == 0 {
            lpc.key(&[0x87, 0x01, 0x55, if secondary { 0xAA } else { 0x55 }])?;
            id = lpc.cfg_inw(0x20)?;
        }
        let exit = || if secondary { Ok(()) } else { lpc.cfg_outb(0x02, 0x02) };
        if id == 0 || id == 0xFFFF {
            return Ok(None);
        }
        let Some(spec) = CHIPS.iter().find(|s| s.id == id) else {
            let _ = exit();
            return if id >> 12 == 8 { Err(format!("unsupported ITE chip {id:04X}")) } else { Ok(None) };
        };
        lpc.find_bars()?;
        let base = lpc.ldn_base(ENV_LDN, 0x60);
        exit()?;
        (spec, base.map_err(|e| format!("{}: {e}", spec.name))?)
    };
    Ok(Some(Box::new(Ite::new(lpc, spec, base)?)))
}

pub struct Ite {
    lpc: Lpc,
    spec: &'static Spec,
    base: u16,
    tach: (&'static [u8; 6], &'static [u8; 6]),
    pwm_ctrl: &'static [u8; 6],
    needs_bank0: bool,
    disabled: Vec<bool>,
    /// Per header: (control register, extended duty, main-control enable bit) before takeover.
    state: BiosState,
}

struct Io<'a> {
    ite: &'a Ite,
    _g: super::pawnio::MutexGuard<'a>,
}

impl Io<'_> {
    fn read(&self, reg: u8) -> Result<u8, String> {
        let (lpc, base) = (&self.ite.lpc, self.ite.base);
        lpc.outb(base + 5, reg)?;
        let v = lpc.inb(base + 6)?;
        // The address port reads back the register when the access went through (the
        // IT8688E is the exception and always returns something else).
        if self.ite.spec.id != 0x8688 && lpc.inb(base + 5)? != reg {
            return Err(format!("{}: register {reg:#04x} read failed", self.ite.spec.name));
        }
        Ok(v)
    }

    fn write(&self, reg: u8, v: u8) -> Result<(), String> {
        let (lpc, base) = (&self.ite.lpc, self.ite.base);
        lpc.outb(base + 5, reg)?;
        lpc.outb(base + 6, v)?;
        lpc.inb(base + 5).map(|_| ())
    }
}

impl Ite {
    fn new(lpc: Lpc, spec: &'static Spec, base: u16) -> Result<Self, String> {
        let alt = matches!(spec.id, 0x8665 | 0x8625);
        let mut me = Self {
            lpc,
            spec,
            base,
            tach: if alt { (&TACH_ALT, &TACH_EXT_ALT) } else { (&TACH, &TACH_EXT) },
            pwm_ctrl: if alt { &PWM_CTRL_ALT } else { &PWM_CTRL },
            needs_bank0: matches!(spec.id, 0x8655 | 0x8665),
            disabled: vec![false; spec.fans],
            state: BiosState::load(format!("{}@{base:#x}", spec.name), spec.controls),
        };
        {
            let io = me.io()?;
            let vendor = io.read(VENDOR)?;
            if vendor != 0x90 && vendor != 0x7F {
                return Err(format!("{} at {base:#x}: vendor id {vendor:#04x} is not ITE", spec.name));
            }
            if io.read(CONFIG)? & 0x10 == 0 && !me.needs_bank0 {
                return Err(format!("{} at {base:#x}: monitoring is disabled", spec.name));
            }
            // Headers 4–6 only count when their 16-bit tachometer is enabled.
            let modes = io.read(FAN_16BIT)?;
            let mut dis = vec![false; spec.fans];
            if spec.fans >= 5 {
                dis[3] = modes & 0x10 == 0;
                dis[4] = modes & 0x20 == 0;
            }
            if spec.fans >= 6 {
                dis[5] = if spec.id == 0x8665 { io.read(FAN_16BIT_ALT)? & 0x08 == 0 } else { modes & 0x04 == 0 };
            }
            drop(io);
            me.disabled = dis;
        }
        Ok(me)
    }

    fn io(&self) -> Result<Io<'_>, String> {
        let io = Io { ite: self, _g: self.lpc.lock(250)? };
        if self.needs_bank0 {
            let b = io.read(BANK)?;
            if b & 0x60 != 0 {
                io.write(BANK, b & 0x9F)?;
            }
        }
        Ok(io)
    }

    fn set_duty(&mut self, i: usize, pct: f32) -> Result<(), String> {
        let raw = (pct.clamp(0.0, 100.0) * 2.55).round() as u8;
        let io = self.io()?;
        let main = io.read(MAIN_CTRL)?;
        let cur = if self.state.is_taken(i) {
            None
        } else {
            let enabled = if i < 3 { (main >> i) & 1 } else { 1 };
            Some(vec![io.read(self.pwm_ctrl[i])?, io.read(PWM_EXT[i])?, enabled])
        };
        let orig_ctrl = cur.as_ref().map(|c| c[0]).or_else(|| self.state.get(i).map(|s| s[0])).unwrap_or(0);
        if i < 3 && main & (1 << i) == 0 {
            io.write(MAIN_CTRL, main | (1 << i))?;
        }
        // Bit 7 clear = software (manual) mode; the duty then comes from the extended register.
        io.write(self.pwm_ctrl[i], if self.spec.id == 0x8689 { 0x7F } else { orig_ctrl & 0x7F })?;
        io.write(PWM_EXT[i], raw)?;
        drop(io);
        if let Some(c) = cur {
            self.state.take(i, c);
        }
        Ok(())
    }

    fn restore(&mut self, i: usize) -> Result<(), String> {
        let Some(&[ctrl, ext, enabled]) = self.state.get(i) else { return Ok(()) };
        let io = self.io()?;
        io.write(self.pwm_ctrl[i], ctrl)?;
        if i < 3 {
            let main = io.read(MAIN_CTRL)?;
            let want = if enabled != 0 { main | (1 << i) } else { main & !(1 << i) };
            if want != main {
                io.write(MAIN_CTRL, want)?;
            }
        }
        io.write(PWM_EXT[i], ext)?;
        drop(io);
        self.state.release(i);
        Ok(())
    }
}

impl SuperIo for Ite {
    fn name(&self) -> String {
        format!("{} @ {:#x}", self.spec.name, self.base)
    }

    fn fans(&self) -> usize {
        self.spec.fans
    }

    fn controllable(&self, i: usize) -> bool {
        i < self.spec.controls
    }

    fn read(&mut self) -> Result<ChipReading, String> {
        let io = self.io()?;
        let mut r = ChipReading::default();
        for i in 0..self.spec.fans {
            if self.disabled[i] {
                r.rpm.push(None);
            } else {
                let v = io.read(self.tach.0[i])? as u32 | (io.read(self.tach.1[i])? as u32) << 8;
                r.rpm.push(match v {
                    0xFFFF => Some(0.0),
                    0x40.. => Some(1.35e6 / (v as f32 * 2.0)),
                    _ => None,
                });
            }
            r.duty.push(if i < self.spec.controls { io.read(PWM_EXT[i])? as f32 / 2.55 } else { 0.0 });
        }
        for i in 0..self.spec.temps {
            let v = io.read(TEMP_BASE + i)? as i8;
            if v > 0 && v < 127 {
                r.temps.push(TempReading {
                    key: format!("temp{}", i + 1),
                    name: format!("Temperature {}", i + 1),
                    value: v as f32,
                });
            }
        }
        Ok(r)
    }

    fn apply(&mut self, i: usize, duty: Option<f32>) -> Result<(), String> {
        if i >= self.spec.controls {
            return Err("this header has no PWM output".into());
        }
        match duty {
            Some(d) => self.set_duty(i, d),
            None => self.restore(i),
        }
    }

    fn is_manual(&self, i: usize) -> bool {
        self.state.is_taken(i)
    }

    fn restore_all(&mut self) {
        for i in 0..self.spec.controls {
            let _ = self.restore(i);
        }
    }

    fn dump(&mut self) -> Result<String, String> {
        let io = self.io()?;
        let mut out = format!("main={:#04x}\n", io.read(MAIN_CTRL)?);
        for i in 0..self.spec.controls {
            out += &format!("fan{}: ctrl={:#04x} ext={:#04x}\n", i + 1, io.read(self.pwm_ctrl[i])?, io.read(PWM_EXT[i])?);
        }
        Ok(out)
    }
}

impl Drop for Ite {
    fn drop(&mut self) {
        self.restore_all();
    }
}
