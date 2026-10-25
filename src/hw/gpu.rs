//! GPU telemetry. Read-only by design: GPU fans always stay with the graphics driver.
//!
//! * NVIDIA: NVML (ships with the driver) for temperature, power and load.
//! * AMD / Intel / anything else: the temperature WDDM 2.4+ drivers report to Windows (the same
//!   number Task Manager shows), read through `D3DKMTQueryAdapterInfo`.

use nvml_wrapper::Nvml;
use nvml_wrapper::enum_wrappers::device::TemperatureSensor;
use windows::Wdk::Graphics::Direct3D::{
    D3DKMT_ADAPTER_PERFDATA, D3DKMT_CLOSEADAPTER, D3DKMT_OPENADAPTERFROMLUID, D3DKMT_QUERYADAPTERINFO, D3DKMTCloseAdapter,
    D3DKMTOpenAdapterFromLuid, D3DKMTQueryAdapterInfo, KMTQAITYPE_ADAPTERPERFDATA,
};
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE, IDXGIFactory1};

const NVIDIA: u32 = 0x10DE;

pub struct GpuReading {
    pub temp: f32,
    pub power_w: Option<f32>,
    pub util: Option<f32>,
}

enum Backend {
    Nvml(nvml_wrapper::Device<'static>),
    Kmt(u32),
}

pub struct Gpu {
    backend: Backend,
    pub name: String,
}

// The D3DKMT adapter handle is a plain kernel handle value, valid from any thread.
unsafe impl Send for Gpu {}

impl Gpu {
    /// Every GPU we can read a temperature from, discrete cards first.
    pub fn open_all() -> Result<Vec<Self>, String> {
        let mut out = Vec::new();
        let mut errors = Vec::new();
        let nvml = Nvml::init().map(|n| &*Box::leak(Box::new(n)));
        match &nvml {
            Ok(nvml) => {
                for i in 0..nvml.device_count().unwrap_or(0) {
                    if let Ok(dev) = nvml.device_by_index(i) {
                        if dev.temperature(TemperatureSensor::Gpu).is_ok() {
                            let name = dev.name().unwrap_or_else(|_| "NVIDIA GPU".into()).replace("NVIDIA GeForce ", "");
                            out.push(Self { backend: Backend::Nvml(dev), name });
                        }
                    }
                }
            }
            Err(e) => errors.push(format!("NVML: {e}")),
        }
        let skip_nvidia = !out.is_empty();
        match kmt_adapters() {
            Ok(list) => {
                for (vendor, name, luid) in list {
                    if vendor == NVIDIA && skip_nvidia {
                        continue;
                    }
                    if let Some(h) = open_kmt(luid) {
                        if kmt_temp(h).is_some() {
                            out.push(Self { backend: Backend::Kmt(h), name: tidy(&name) });
                            continue;
                        }
                        close_kmt(h);
                    }
                }
            }
            Err(e) => errors.push(e),
        }
        if out.is_empty() {
            Err(if errors.is_empty() { "no GPU reports a temperature".into() } else { errors.join("; ") })
        } else {
            Ok(out)
        }
    }

    pub fn describe(&self) -> String {
        match self.backend {
            Backend::Nvml(_) => format!("{} · NVML", self.name),
            Backend::Kmt(_) => format!("{} · WDDM", self.name),
        }
    }

    pub fn read(&self) -> Result<GpuReading, String> {
        match &self.backend {
            Backend::Nvml(dev) => Ok(GpuReading {
                temp: dev.temperature(TemperatureSensor::Gpu).map_err(|e| e.to_string())? as f32,
                power_w: dev.power_usage().ok().map(|mw| mw as f32 / 1000.0),
                util: dev.utilization_rates().ok().map(|u| u.gpu as f32),
            }),
            Backend::Kmt(h) => Ok(GpuReading { temp: kmt_temp(*h).ok_or("temperature unavailable")?, power_w: None, util: None }),
        }
    }
}

impl Drop for Gpu {
    fn drop(&mut self) {
        if let Backend::Kmt(h) = self.backend {
            close_kmt(h);
        }
    }
}

fn tidy(name: &str) -> String {
    name.replace("AMD Radeon ", "Radeon ").replace("Intel(R) ", "Intel ").replace("NVIDIA GeForce ", "").trim().to_string()
}

/// (vendor id, description, LUID) of every hardware adapter.
fn kmt_adapters() -> Result<Vec<(u32, String, windows::Win32::Foundation::LUID)>, String> {
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }.map_err(|e| format!("DXGI: {}", e.message()))?;
    let mut out = Vec::new();
    for i in 0.. {
        let Ok(a) = (unsafe { factory.EnumAdapters1(i) }) else { break };
        let Ok(d) = (unsafe { a.GetDesc1() }) else { continue };
        if d.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0 {
            continue;
        }
        let n = d.Description.iter().position(|&c| c == 0).unwrap_or(d.Description.len());
        let name = String::from_utf16_lossy(&d.Description[..n]);
        if !out.iter().any(|(_, _, l): &(u32, String, windows::Win32::Foundation::LUID)| *l == d.AdapterLuid) {
            out.push((d.VendorId, name, d.AdapterLuid));
        }
    }
    Ok(out)
}

fn open_kmt(luid: windows::Win32::Foundation::LUID) -> Option<u32> {
    let mut o = D3DKMT_OPENADAPTERFROMLUID { AdapterLuid: luid, hAdapter: 0 };
    (unsafe { D3DKMTOpenAdapterFromLuid(&mut o) }.is_ok()).then_some(o.hAdapter)
}

fn close_kmt(h: u32) {
    let c = D3DKMT_CLOSEADAPTER { hAdapter: h };
    unsafe {
        let _ = D3DKMTCloseAdapter(&c);
    }
}

/// Temperature in °C, or None when the driver doesn't report one (integrated GPUs, old drivers).
fn kmt_temp(h: u32) -> Option<f32> {
    let mut perf = D3DKMT_ADAPTER_PERFDATA::default();
    let mut q = D3DKMT_QUERYADAPTERINFO {
        hAdapter: h,
        Type: KMTQAITYPE_ADAPTERPERFDATA,
        pPrivateDriverData: &mut perf as *mut _ as *mut core::ffi::c_void,
        PrivateDriverDataSize: size_of::<D3DKMT_ADAPTER_PERFDATA>() as u32,
    };
    unsafe { D3DKMTQueryAdapterInfo(&mut q) }.ok().ok()?;
    // Deci-degrees Celsius.
    let t = perf.Temperature as f32 / 10.0;
    (t > 0.0 && t < 150.0).then_some(t)
}
