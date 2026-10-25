//! CPU die temperatures.
//!
//! * AMD Zen (family 17h–1Ah): Tctl and per-CCD temperatures over SMN (`AMDFamily17` module).
//! * Intel Core (Nehalem and later): package temperature from the thermal MSRs (`IntelMSR` module).

use super::pawnio::{AMD_FAMILY_17, GlobalMutex, INTEL_MSR, Module};

const THM_TCON_CUR_TMP: u64 = 0x0005_9800;
const CCD_TEMP_BASE: u64 = 0x0005_9954;
const RANGE_SEL: u64 = 0x8_0000;
const CCD_VALID: u64 = 0x800;

const IA32_TEMPERATURE_TARGET: u64 = 0x1A2;
const IA32_PACKAGE_THERM_STATUS: u64 = 0x1B1;

pub struct CpuReading {
    /// Package / control temperature.
    pub main: f32,
    /// Extra dies (Ryzen CCDs).
    pub dies: Vec<f32>,
}

pub enum CpuTemps {
    Zen { module: Module, pci: GlobalMutex, ccds: Vec<u64> },
    Intel { module: Module, tjmax: f32 },
}

impl CpuTemps {
    pub fn open() -> Result<Self, String> {
        let zen = Self::open_zen();
        if zen.is_ok() {
            return zen;
        }
        let intel = Self::open_intel();
        if intel.is_ok() {
            return intel;
        }
        Err(match (zen, intel) {
            (Err(a), Err(b)) => format!("no supported CPU sensor (AMD: {a}; Intel: {b})"),
            _ => unreachable!(),
        })
    }

    fn open_zen() -> Result<Self, String> {
        let module = Module::load(AMD_FAMILY_17)?;
        let pci = GlobalMutex::open(GlobalMutex::PCI).ok_or("cannot open PCI mutex")?;
        let mut ccds = Vec::new();
        {
            let _g = pci.lock(200);
            for i in 0..8 {
                let addr = CCD_TEMP_BASE + i * 4;
                if module.call1("ioctl_read_smn", &[addr]).is_ok_and(|v| v & CCD_VALID != 0) {
                    ccds.push(addr);
                }
            }
        }
        let me = Self::Zen { module, pci, ccds };
        me.read()?;
        Ok(me)
    }

    fn open_intel() -> Result<Self, String> {
        let module = Module::load(INTEL_MSR)?;
        let target = module.call1("ioctl_read_msr", &[IA32_TEMPERATURE_TARGET])?;
        let tjmax = match ((target >> 16) & 0xFF) as f32 {
            t if (60.0..=130.0).contains(&t) => t,
            _ => 100.0,
        };
        let me = Self::Intel { module, tjmax };
        me.read()?;
        Ok(me)
    }

    pub fn describe(&self) -> String {
        match self {
            Self::Zen { ccds, .. } => format!("AMD Ryzen · Tctl + {} CCD", ccds.len()),
            Self::Intel { tjmax, .. } => format!("Intel · package temperature (TjMax {tjmax:.0} °C)"),
        }
    }

    pub fn read(&self) -> Result<CpuReading, String> {
        match self {
            Self::Zen { module, pci, ccds } => {
                let _g = pci.lock(100).ok_or("PCI bus busy")?;
                let raw = module.call1("ioctl_read_smn", &[THM_TCON_CUR_TMP])?;
                let mut tctl = ((raw >> 21) & 0x7FF) as f32 * 0.125;
                if raw & RANGE_SEL != 0 {
                    tctl -= 49.0;
                }
                let mut dies = Vec::with_capacity(ccds.len());
                for &a in ccds {
                    let v = module.call1("ioctl_read_smn", &[a])?;
                    dies.push((v & 0x7FF) as f32 * 0.125 - 49.0);
                }
                if !(0.0..=125.0).contains(&tctl) {
                    return Err(format!("implausible Tctl {tctl:.1} °C"));
                }
                Ok(CpuReading { main: tctl, dies })
            }
            Self::Intel { module, tjmax } => {
                // Bits 22:16 = degrees below TjMax.
                let v = module.call1("ioctl_read_msr", &[IA32_PACKAGE_THERM_STATUS])?;
                let t = tjmax - ((v >> 16) & 0x7F) as f32;
                Ok(CpuReading { main: t, dies: Vec::new() })
            }
        }
    }
}
