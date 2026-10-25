//! Plumbing shared by the Super I/O drivers: slot access through PawnIO's `LpcIO` module,
//! the ISA bus lock, and the persisted BIOS fan state.

use super::pawnio::{GlobalMutex, LPC_IO, Module, MutexGuard};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// One Super I/O slot (0 = 0x2E/0x2F, 1 = 0x4E/0x4F) with its own module instance.
pub struct Lpc {
    module: Module,
    isa: GlobalMutex,
    /// Config index port (0x2E or 0x4E).
    pub port: u16,
}

impl Lpc {
    pub fn open(slot: u8) -> Result<Self, String> {
        let isa = GlobalMutex::open(GlobalMutex::ISA_BUS).ok_or("cannot open ISA bus mutex")?;
        let module = Module::load(LPC_IO)?;
        let lpc = Self { module, isa, port: if slot == 0 { 0x2E } else { 0x4E } };
        {
            let _g = lpc.lock(500)?;
            lpc.module.call0("ioctl_select_slot", &[slot as u64])?;
        }
        Ok(lpc)
    }

    pub fn lock(&self, timeout_ms: u32) -> Result<MutexGuard<'_>, String> {
        self.isa.lock(timeout_ms).ok_or_else(|| "ISA bus busy (another monitoring tool is holding it)".to_string())
    }

    pub fn outb(&self, port: u16, v: u8) -> Result<(), String> {
        self.module.call0("ioctl_pio_outb", &[port as u64, v as u64])
    }

    pub fn inb(&self, port: u16) -> Result<u8, String> {
        self.module.call1("ioctl_pio_inb", &[port as u64]).map(|v| v as u8)
    }

    /// Writes the config index port itself (enter / exit key sequences).
    pub fn key(&self, seq: &[u8]) -> Result<(), String> {
        seq.iter().try_for_each(|&b| self.outb(self.port, b))
    }

    pub fn cfg_inb(&self, reg: u8) -> Result<u8, String> {
        self.module.call1("ioctl_superio_inb", &[reg as u64]).map(|v| v as u8)
    }

    pub fn cfg_inw(&self, reg: u8) -> Result<u16, String> {
        self.module.call1("ioctl_superio_inw", &[reg as u64]).map(|v| v as u16)
    }

    pub fn cfg_outb(&self, reg: u8, v: u8) -> Result<(), String> {
        self.module.call0("ioctl_superio_outb", &[reg as u64, v as u64])
    }

    /// Lets the module whitelist the chip's runtime I/O ranges. Must be in config mode.
    pub fn find_bars(&self) -> Result<(), String> {
        self.module.call0("ioctl_find_bars", &[])
    }

    /// Reads a logical device's base address twice (it must be stable) and validates it.
    pub fn ldn_base(&self, ldn: u8, reg: u8) -> Result<u16, String> {
        self.cfg_outb(0x07, ldn)?;
        let a = self.cfg_inw(reg)?;
        std::thread::sleep(std::time::Duration::from_millis(1));
        let b = self.cfg_inw(reg)?;
        if a != b || a < 0x100 || a & 0xF007 != 0 {
            return Err(format!("invalid base address {a:#x}/{b:#x}"));
        }
        Ok(a)
    }
}

// ---------------------------------------------------------------------------------------------
// BIOS state
// ---------------------------------------------------------------------------------------------

fn bios_state_path() -> PathBuf {
    let base = std::env::var_os("ProgramData").map(PathBuf::from).unwrap_or_else(|| ".".into());
    base.join("Hush").join("bios-fan-state.json")
}

type BiosFile = BTreeMap<String, BTreeMap<String, Vec<u8>>>;

fn load_file() -> BiosFile {
    std::fs::read_to_string(bios_state_path()).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

fn save_file(f: &BiosFile) {
    let path = bios_state_path();
    if let Some(d) = path.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    if f.values().all(|m| m.is_empty()) {
        let _ = std::fs::remove_file(path);
    } else {
        let _ = std::fs::write(path, serde_json::to_vec_pretty(f).unwrap_or_default());
    }
}

/// The registers a header had before Hush took it over.
///
/// The state is written to disk the moment a header is taken and removed again once it has
/// been handed back. If Hush is killed in between (headers left in manual mode), the next
/// start finds the file and restores the real BIOS configuration instead of the leftover
/// manual one.
pub struct BiosState {
    key: String,
    saved: Vec<Option<Vec<u8>>>,
}

impl BiosState {
    pub fn load(key: String, headers: usize) -> Self {
        let f = load_file();
        let m = f.get(&key);
        let saved = (0..headers).map(|i| m.and_then(|m| m.get(&format!("fan{}", i + 1))).cloned()).collect();
        Self { key, saved }
    }

    pub fn is_taken(&self, i: usize) -> bool {
        self.saved.get(i).is_some_and(|s| s.is_some())
    }

    /// Records `current` as header `i`'s original state (unless one is already on record) and
    /// returns the state to restore later.
    pub fn take(&mut self, i: usize, current: Vec<u8>) -> &[u8] {
        if self.saved[i].is_none() {
            self.saved[i] = Some(current);
            self.persist();
        }
        self.saved[i].as_deref().unwrap_or_default()
    }

    pub fn get(&self, i: usize) -> Option<&[u8]> {
        self.saved.get(i).and_then(|s| s.as_deref())
    }

    /// Forgets header `i` after it was restored.
    pub fn release(&mut self, i: usize) {
        if self.saved[i].take().is_some() {
            self.persist();
        }
    }

    fn persist(&self) {
        let mut f = load_file();
        let m: BTreeMap<String, Vec<u8>> =
            self.saved.iter().enumerate().filter_map(|(i, s)| s.clone().map(|s| (format!("fan{}", i + 1), s))).collect();
        if m.is_empty() {
            f.remove(&self.key);
        } else {
            f.insert(self.key.clone(), m);
        }
        save_file(&f);
    }
}

// ---------------------------------------------------------------------------------------------
// Board identity (some chips need it to pick the right register map)
// ---------------------------------------------------------------------------------------------

/// (manufacturer, product) of the motherboard, from the SMBIOS copy Windows keeps in the registry.
pub fn board() -> (String, String) {
    let k = winreg::RegKey::predef(winreg::enums::HKEY_LOCAL_MACHINE).open_subkey(r"HARDWARE\DESCRIPTION\System\BIOS");
    let get = |n: &str| k.as_ref().ok().and_then(|k| k.get_value::<String, _>(n).ok()).unwrap_or_default().trim().to_string();
    (get("BaseBoardManufacturer"), get("BaseBoardProduct"))
}
