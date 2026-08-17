//! NVIDIA GPU telemetry via `nvml.dll`, dynamically loaded (ships with the NVIDIA
//! driver, not with this app — absent entirely on machines with no NVIDIA GPU).
//!
//! Signatures and constants below are copied from NVIDIA's own `nvml.h`
//! (NVIDIA/nvidia-settings on GitHub), not guessed. Three functions matter here for
//! why: `nvmlInit`, `nvmlDeviceGetHandleByIndex` and `nvmlDeviceGetPciInfo` are
//! *macros* in the real header that redirect to `nvmlInit_v2`,
//! `nvmlDeviceGetHandleByIndex_v2` and `nvmlDeviceGetPciInfo_v3` — the unversioned
//! names are not actually exported symbols in a current `nvml.dll`, so binding to
//! them directly would silently fail every call with "symbol not found" on real
//! hardware. `nvmlDeviceGetCurrentClocksEventReasons`'s bitmask constants are
//! likewise copied verbatim, since this is the one genuinely ground-truth signal
//! this whole project has: NVIDIA's own driver naming the exact cause of a clock
//! drop, not an inference from a threshold this app picked.

use std::ffi::c_void;

use serde::{Deserialize, Serialize};

use crate::win::dynlib::Library;
use crate::win::{WinError, WinResult};

type NvmlDevice = *mut c_void;

const NVML_SUCCESS: i32 = 0;

// nvmlClockType_t
const NVML_CLOCK_GRAPHICS: u32 = 0;
// nvmlTemperatureSensors_t
const NVML_TEMPERATURE_GPU: u32 = 0;

// nvmlClocksEventReasons — bit values from nvml.h, unchanged since NVML's earliest
// throttle-reasons API and still current.
const REASON_SW_POWER_CAP: u64 = 0x0000_0000_0000_0004;
const REASON_HW_SLOWDOWN: u64 = 0x0000_0000_0000_0008;
const REASON_SW_THERMAL_SLOWDOWN: u64 = 0x0000_0000_0000_0020;
const REASON_HW_THERMAL_SLOWDOWN: u64 = 0x0000_0000_0000_0040;
const REASON_HW_POWER_BRAKE: u64 = 0x0000_0000_0000_0080;

#[repr(C)]
struct NvmlPciInfo {
    bus_id_legacy: [i8; 16],
    domain: u32,
    bus: u32,
    device: u32,
    pci_device_id: u32,
    pci_sub_system_id: u32,
    bus_id: [i8; 32],
}

type FnInit = unsafe extern "system" fn() -> i32;
type FnShutdown = unsafe extern "system" fn() -> i32;
type FnDeviceGetCount = unsafe extern "system" fn(*mut u32) -> i32;
type FnDeviceGetHandleByIndex = unsafe extern "system" fn(u32, *mut NvmlDevice) -> i32;
type FnDeviceGetPciInfo = unsafe extern "system" fn(NvmlDevice, *mut NvmlPciInfo) -> i32;
type FnDeviceGetName = unsafe extern "system" fn(NvmlDevice, *mut i8, u32) -> i32;
type FnDeviceGetClockInfo = unsafe extern "system" fn(NvmlDevice, u32, *mut u32) -> i32;
type FnDeviceGetPowerUsage = unsafe extern "system" fn(NvmlDevice, *mut u32) -> i32;
type FnDeviceGetTemperature = unsafe extern "system" fn(NvmlDevice, u32, *mut u32) -> i32;
type FnDeviceGetFanSpeed = unsafe extern "system" fn(NvmlDevice, *mut u32) -> i32;
type FnDeviceGetThrottleReasons = unsafe extern "system" fn(NvmlDevice, *mut u64) -> i32;

pub struct NvmlLib {
    _lib: Library,
    shutdown: FnShutdown,
    device_get_count: FnDeviceGetCount,
    device_get_handle: FnDeviceGetHandleByIndex,
    device_get_pci_info: FnDeviceGetPciInfo,
    device_get_name: FnDeviceGetName,
    device_get_clock_info: FnDeviceGetClockInfo,
    device_get_power_usage: FnDeviceGetPowerUsage,
    device_get_temperature: FnDeviceGetTemperature,
    device_get_fan_speed: FnDeviceGetFanSpeed,
    device_get_throttle_reasons: FnDeviceGetThrottleReasons,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct ThrottleReasons {
    pub sw_power_cap: bool,
    pub hw_slowdown: bool,
    pub sw_thermal_slowdown: bool,
    pub hw_thermal_slowdown: bool,
    pub hw_power_brake: bool,
}

impl ThrottleReasons {
    fn from_bits(bits: u64) -> Self {
        Self {
            sw_power_cap: bits & REASON_SW_POWER_CAP != 0,
            hw_slowdown: bits & REASON_HW_SLOWDOWN != 0,
            sw_thermal_slowdown: bits & REASON_SW_THERMAL_SLOWDOWN != 0,
            hw_thermal_slowdown: bits & REASON_HW_THERMAL_SLOWDOWN != 0,
            hw_power_brake: bits & REASON_HW_POWER_BRAKE != 0,
        }
    }

    pub fn any(&self) -> bool {
        self.sw_power_cap || self.hw_slowdown || self.sw_thermal_slowdown || self.hw_thermal_slowdown || self.hw_power_brake
    }
}

impl NvmlLib {
    /// Load `nvml.dll` and call `nvmlInit_v2`. Fails cleanly (no NVIDIA driver
    /// present is the common case on AMD/Intel-only machines).
    pub fn open() -> WinResult<Self> {
        let lib = Library::load("nvml.dll")?;

        macro_rules! resolve {
            ($name:literal) => {
                unsafe { lib.proc_as($name) }.ok_or_else(|| WinError::new(concat!($name, " missing"), 0))?
            };
        }

        let init: FnInit = resolve!("nvmlInit_v2");
        let shutdown: FnShutdown = resolve!("nvmlShutdown");
        let device_get_count: FnDeviceGetCount = resolve!("nvmlDeviceGetCount_v2");
        let device_get_handle: FnDeviceGetHandleByIndex = resolve!("nvmlDeviceGetHandleByIndex_v2");
        let device_get_pci_info: FnDeviceGetPciInfo = resolve!("nvmlDeviceGetPciInfo_v3");
        let device_get_name: FnDeviceGetName = resolve!("nvmlDeviceGetName");
        let device_get_clock_info: FnDeviceGetClockInfo = resolve!("nvmlDeviceGetClockInfo");
        let device_get_power_usage: FnDeviceGetPowerUsage = resolve!("nvmlDeviceGetPowerUsage");
        let device_get_temperature: FnDeviceGetTemperature = resolve!("nvmlDeviceGetTemperature");
        let device_get_fan_speed: FnDeviceGetFanSpeed = resolve!("nvmlDeviceGetFanSpeed");
        let device_get_throttle_reasons: FnDeviceGetThrottleReasons =
            resolve!("nvmlDeviceGetCurrentClocksEventReasons");

        let hr = unsafe { init() };
        if hr != NVML_SUCCESS {
            return Err(WinError::new("nvmlInit_v2 failed", hr as u32));
        }

        Ok(Self {
            _lib: lib,
            shutdown,
            device_get_count,
            device_get_handle,
            device_get_pci_info,
            device_get_name,
            device_get_clock_info,
            device_get_power_usage,
            device_get_temperature,
            device_get_fan_speed,
            device_get_throttle_reasons,
        })
    }

    fn device_count(&self) -> WinResult<u32> {
        let mut count = 0u32;
        let hr = unsafe { (self.device_get_count)(&mut count) };
        if hr != NVML_SUCCESS {
            return Err(WinError::new("nvmlDeviceGetCount_v2 failed", hr as u32));
        }
        Ok(count)
    }

    fn device_handle(&self, index: u32) -> WinResult<NvmlDevice> {
        let mut dev: NvmlDevice = std::ptr::null_mut();
        let hr = unsafe { (self.device_get_handle)(index, &mut dev) };
        if hr != NVML_SUCCESS {
            return Err(WinError::new("nvmlDeviceGetHandleByIndex_v2 failed", hr as u32));
        }
        Ok(dev)
    }

    fn pci_ids(&self, dev: NvmlDevice) -> WinResult<(u32, u32)> {
        let mut info = NvmlPciInfo {
            bus_id_legacy: [0; 16],
            domain: 0,
            bus: 0,
            device: 0,
            pci_device_id: 0,
            pci_sub_system_id: 0,
            bus_id: [0; 32],
        };
        let hr = unsafe { (self.device_get_pci_info)(dev, &mut info) };
        if hr != NVML_SUCCESS {
            return Err(WinError::new("nvmlDeviceGetPciInfo_v3 failed", hr as u32));
        }
        // Standard PCI config space layout: vendor ID occupies the low 16 bits of the
        // combined 32-bit device/vendor word, device ID the high 16 bits.
        let vendor_id = info.pci_device_id & 0xFFFF;
        let device_id = (info.pci_device_id >> 16) & 0xFFFF;
        Ok((vendor_id, device_id))
    }

    /// Find the NVML device handle matching a DXGI-reported vendor/device ID, so the
    /// stress test reads telemetry for the same adapter the inventory scan showed.
    pub fn find_device(&self, vendor_id: u32, device_id: u32) -> WinResult<NvmlDevice> {
        let count = self.device_count()?;
        for i in 0..count {
            let dev = self.device_handle(i)?;
            if let Ok((v, d)) = self.pci_ids(dev) {
                if v == vendor_id && d == device_id {
                    return Ok(dev);
                }
            }
        }
        Err(WinError::new("no NVML device matched the requested vendor/device id", 0))
    }

    /// # Safety
    /// `dev` must be a handle obtained from this same `NvmlLib` instance (via
    /// [`NvmlLib::find_device`]) and not outlive it — NVML device handles are only
    /// valid between `nvmlInit` and `nvmlShutdown`.
    pub unsafe fn name(&self, dev: NvmlDevice) -> WinResult<String> {
        let mut buf = [0i8; 96];
        let hr = unsafe { (self.device_get_name)(dev, buf.as_mut_ptr(), buf.len() as u32) };
        if hr != NVML_SUCCESS {
            return Err(WinError::new("nvmlDeviceGetName failed", hr as u32));
        }
        let bytes: Vec<u8> = buf.iter().take_while(|&&c| c != 0).map(|&c| c as u8).collect();
        Ok(String::from_utf8_lossy(&bytes).to_string())
    }

    /// # Safety
    /// See [`NvmlLib::name`].
    pub unsafe fn graphics_clock_mhz(&self, dev: NvmlDevice) -> WinResult<u32> {
        let mut clock = 0u32;
        let hr = unsafe { (self.device_get_clock_info)(dev, NVML_CLOCK_GRAPHICS, &mut clock) };
        if hr != NVML_SUCCESS {
            return Err(WinError::new("nvmlDeviceGetClockInfo failed", hr as u32));
        }
        Ok(clock)
    }

    /// NVML reports power in milliwatts; converted to watts here so every telemetry
    /// field in the rest of this app shares the same unit.
    /// # Safety
    /// See [`NvmlLib::name`].
    pub unsafe fn power_watts(&self, dev: NvmlDevice) -> WinResult<f64> {
        let mut mw = 0u32;
        let hr = unsafe { (self.device_get_power_usage)(dev, &mut mw) };
        if hr != NVML_SUCCESS {
            return Err(WinError::new("nvmlDeviceGetPowerUsage failed", hr as u32));
        }
        Ok(mw as f64 / 1000.0)
    }

    /// # Safety
    /// See [`NvmlLib::name`].
    pub unsafe fn temperature_c(&self, dev: NvmlDevice) -> WinResult<u32> {
        let mut temp = 0u32;
        let hr = unsafe { (self.device_get_temperature)(dev, NVML_TEMPERATURE_GPU, &mut temp) };
        if hr != NVML_SUCCESS {
            return Err(WinError::new("nvmlDeviceGetTemperature failed", hr as u32));
        }
        Ok(temp)
    }

    /// Percent of maximum fan speed. No RPM equivalent exists in base NVML — vendor
    /// tools that show a tachometer reading for NVIDIA cards read it through a
    /// private/vendor-specific path, not this API, so this app reports percent only
    /// for NVIDIA (AMD's ADL exposes both, see `probes::adl::AdlSession::fan_rpm`).
    /// Fails with `NVML_ERROR_NOT_SUPPORTED` on fanless/liquid-cooled cards — that
    /// surfaces as an `Err` here and degrades to `Reading::missing` the same way every
    /// other per-device sensor call in this module does.
    /// # Safety
    /// See [`NvmlLib::name`].
    pub unsafe fn fan_percent(&self, dev: NvmlDevice) -> WinResult<u32> {
        let mut percent = 0u32;
        let hr = unsafe { (self.device_get_fan_speed)(dev, &mut percent) };
        if hr != NVML_SUCCESS {
            return Err(WinError::new("nvmlDeviceGetFanSpeed failed", hr as u32));
        }
        Ok(percent)
    }

    /// # Safety
    /// See [`NvmlLib::name`].
    pub unsafe fn throttle_reasons(&self, dev: NvmlDevice) -> WinResult<ThrottleReasons> {
        let mut bits = 0u64;
        let hr = unsafe { (self.device_get_throttle_reasons)(dev, &mut bits) };
        if hr != NVML_SUCCESS {
            return Err(WinError::new("nvmlDeviceGetCurrentClocksEventReasons failed", hr as u32));
        }
        Ok(ThrottleReasons::from_bits(bits))
    }
}

impl Drop for NvmlLib {
    fn drop(&mut self) {
        let _ = unsafe { (self.shutdown)() };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn throttle_bits_decode_the_flags_that_matter_for_the_stress_report() {
        let r = ThrottleReasons::from_bits(REASON_HW_THERMAL_SLOWDOWN | REASON_SW_POWER_CAP);
        assert!(r.hw_thermal_slowdown);
        assert!(r.sw_power_cap);
        assert!(!r.hw_slowdown);
        assert!(r.any());
    }

    #[test]
    fn no_reasons_set_is_not_any() {
        assert!(!ThrottleReasons::from_bits(0).any());
    }

    #[test]
    fn missing_nvml_on_a_non_nvidia_machine_is_a_clean_error_not_a_panic() {
        // This dev machine has an AMD GPU — nvml.dll is genuinely absent, which is
        // exactly the production case this exercises.
        let result = NvmlLib::open();
        assert!(result.is_err(), "expected nvml.dll to be absent on this machine");
    }
}
