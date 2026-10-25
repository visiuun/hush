//! Minimal client for the PawnIO kernel driver.
//!
//! PawnIO is a signed driver that only runs small sandboxed, signed "modules". The modules
//! decide which ports / PCI registers may be touched, so unlike WinRing0 it cannot be abused
//! as a generic kernel read/write primitive.

use windows::Win32::Foundation::{CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE, WAIT_ABANDONED, WAIT_OBJECT_0};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::IO::DeviceIoControl;
use windows::Win32::System::Threading::{CreateMutexW, ReleaseMutex, WaitForSingleObject};
use windows::core::{HSTRING, w};

const DEVICE_TYPE: u32 = 41394 << 16;
const IOCTL_EXECUTE: u32 = DEVICE_TYPE | (0x841 << 2);
const IOCTL_LOAD: u32 = DEVICE_TYPE | (0x821 << 2);
const FN_NAME_LEN: usize = 32;

pub const LPC_IO: &[u8] = include_bytes!("../../assets/pawnio/LpcIO.bin");
pub const AMD_FAMILY_17: &[u8] = include_bytes!("../../assets/pawnio/AMDFamily17.bin");
pub const INTEL_MSR: &[u8] = include_bytes!("../../assets/pawnio/IntelMSR.bin");

/// True when the PawnIO driver service is registered.
pub fn installed() -> bool {
    winreg::RegKey::predef(winreg::enums::HKEY_LOCAL_MACHINE)
        .open_subkey(r"SYSTEM\CurrentControlSet\Services\PawnIO")
        .is_ok()
}

const SETUP_URL: &str = "https://github.com/namazso/PawnIO.Setup/releases/latest/download/PawnIO_setup.exe";

/// Installs the PawnIO driver silently (needs admin). Uses a `PawnIO_setup.exe` next to our
/// executable when there is one, otherwise downloads the official installer.
pub fn install() -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    let local = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.join("PawnIO_setup.exe"))).filter(|p| p.is_file());
    let setup = match local {
        Some(p) => p,
        None => {
            let p = std::env::temp_dir().join(format!("PawnIO_setup-{}.exe", std::process::id()));
            let ok = std::process::Command::new("curl.exe")
                .args(["-fsSL", "--retry", "2", "-o"])
                .arg(&p)
                .arg(SETUP_URL)
                .creation_flags(crate::winutil::CREATE_NO_WINDOW)
                .status()
                .is_ok_and(|s| s.success());
            if !ok || !p.is_file() {
                return Err("could not download the PawnIO installer (offline?)".into());
            }
            p
        }
    };
    let status = std::process::Command::new(&setup).args(["-install", "-silent"]).status();
    if setup.starts_with(std::env::temp_dir()) {
        let _ = std::fs::remove_file(&setup);
    }
    match status {
        Ok(_) if installed() => Ok(()),
        Ok(s) => Err(format!("PawnIO installer exited with {s}")),
        Err(e) => Err(format!("could not run the PawnIO installer: {e}")),
    }
}

/// One loaded module instance (each open handle carries its own module + state).
pub struct Module {
    handle: HANDLE,
}

unsafe impl Send for Module {}
unsafe impl Sync for Module {}

impl Module {
    pub fn load(blob: &[u8]) -> Result<Self, String> {
        unsafe {
            let handle = CreateFileW(
                w!(r"\\?\GLOBALROOT\Device\PawnIO"),
                (GENERIC_READ | GENERIC_WRITE).0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                None,
            )
            .map_err(|e| format!("PawnIO driver not reachable ({}). Is it installed and are we elevated?", e.message()))?;
            let module = Self { handle };
            DeviceIoControl(handle, IOCTL_LOAD, Some(blob.as_ptr().cast()), blob.len() as u32, None, 0, None, None)
                .map_err(|e| format!("PawnIO refused the module: {}", e.message()))?;
            Ok(module)
        }
    }

    /// Runs a module function. `out` receives up to `out.len()` 64-bit results.
    pub fn exec(&self, name: &str, input: &[u64], out: &mut [u64]) -> Result<usize, String> {
        let mut buf = [0u8; FN_NAME_LEN + 8 * 4];
        debug_assert!(input.len() <= 4 && name.len() < FN_NAME_LEN);
        buf[..name.len()].copy_from_slice(name.as_bytes());
        for (i, v) in input.iter().enumerate() {
            buf[FN_NAME_LEN + i * 8..FN_NAME_LEN + i * 8 + 8].copy_from_slice(&v.to_le_bytes());
        }
        let in_len = FN_NAME_LEN + input.len() * 8;
        let mut read = 0u32;
        unsafe {
            DeviceIoControl(
                self.handle,
                IOCTL_EXECUTE,
                Some(buf.as_ptr().cast()),
                in_len as u32,
                (!out.is_empty()).then_some(out.as_mut_ptr().cast()),
                (out.len() * 8) as u32,
                Some(&mut read),
                None,
            )
            .map_err(|e| format!("{name}: {}", e.message()))?;
        }
        Ok(read as usize / 8)
    }

    pub fn call0(&self, name: &str, input: &[u64]) -> Result<(), String> {
        self.exec(name, input, &mut []).map(|_| ())
    }

    pub fn call1(&self, name: &str, input: &[u64]) -> Result<u64, String> {
        let mut out = [0u64; 1];
        self.exec(name, input, &mut out)?;
        Ok(out[0])
    }
}

impl Drop for Module {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.handle);
        }
    }
}

/// A named global mutex shared with other hardware tools (HWiNFO, LibreHardwareMonitor, …)
/// so we never interleave bank/index writes with theirs.
pub struct GlobalMutex(HANDLE);

unsafe impl Send for GlobalMutex {}
unsafe impl Sync for GlobalMutex {}

impl GlobalMutex {
    pub const ISA_BUS: &'static str = r"Global\Access_ISABUS.HTP.Method";
    pub const PCI: &'static str = r"Global\Access_PCI";

    pub fn open(name: &str) -> Option<Self> {
        unsafe {
            // Everyone may synchronize, like the other tools create it.
            let mut sd = PSECURITY_DESCRIPTOR::default();
            let _ = ConvertStringSecurityDescriptorToSecurityDescriptorW(w!("D:(A;;GA;;;WD)"), SDDL_REVISION_1, &mut sd, None);
            let sa = SECURITY_ATTRIBUTES {
                nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: sd.0,
                bInheritHandle: false.into(),
            };
            CreateMutexW(Some(&sa), false, &HSTRING::from(name)).ok().map(Self)
        }
    }

    pub fn lock(&self, timeout_ms: u32) -> Option<MutexGuard<'_>> {
        let r = unsafe { WaitForSingleObject(self.0, timeout_ms) };
        (r == WAIT_OBJECT_0 || r == WAIT_ABANDONED).then_some(MutexGuard(self))
    }
}

impl Drop for GlobalMutex {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

pub struct MutexGuard<'a>(&'a GlobalMutex);

impl Drop for MutexGuard<'_> {
    fn drop(&mut self) {
        unsafe {
            let _ = ReleaseMutex(self.0.0);
        }
    }
}
