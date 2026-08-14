//! GPU inventory via DXGI, with driver details from the display class registry.
//!
//! DXGI is the right source for adapter identity because it is vendor-neutral, needs no
//! elevation and no vendor SDK, and reports memory as a 64-bit value.
//! `Win32_VideoController.AdapterRAM` is a 32-bit field and saturates: it reports the
//! 12 GB card in this development machine as 4095 MB. A buyer being told a 12 GB card
//! has 4 GB would be a serious error, so WMI is not used for this.
//!
//! Live sensor telemetry (temperature, hotspot, board power, clocks, throttle reasons)
//! needs the vendor libraries — NVML, AMD ADL, Intel IGCL — and lands with the stress
//! milestone, where it is actually consumed. This module is identity only.
//!
//! Deliberately absent: system-wide VRAM usage. DXGI's `QueryVideoMemoryInfo` reports
//! the *calling process's* budget, so wiring it up here yields our own consumption
//! (zero) under a name that implies the machine's. Real VRAM occupancy needs either the
//! GPU performance counters or the vendor libraries, and arrives with them.

use serde::{Deserialize, Serialize};

use crate::model::Reading;
use crate::win::{wide_to_string, WinError, WinResult};

use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIFactory1, DXGI_ADAPTER_DESC1, DXGI_ADAPTER_FLAG,
    DXGI_ADAPTER_FLAG_SOFTWARE,
};

/// PCI vendor IDs.
fn vendor_name(id: u32) -> &'static str {
    match id {
        0x10DE => "NVIDIA",
        0x1002 | 0x1022 => "AMD",
        0x8086 => "Intel",
        0x1414 => "Microsoft",
        0x5143 => "Qualcomm",
        _ => "Unknown",
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GpuReport {
    pub name: String,
    pub vendor: String,
    pub vendor_id: u32,
    pub device_id: u32,
    pub subsystem_id: u32,
    pub revision: u32,
    /// True 64-bit dedicated VRAM, in bytes.
    pub dedicated_vram_bytes: u64,
    pub shared_memory_bytes: u64,
    /// A software rasteriser (WARP), not real hardware.
    pub is_software: bool,
    /// Whether this adapter drives a display. A laptop's discrete GPU often does not.
    pub has_output: bool,

    pub driver_version: Reading<String>,
    pub driver_date: Reading<String>,
}

pub fn probe() -> WinResult<Vec<GpuReport>> {
    let drivers = read_display_drivers();
    let mut out = Vec::new();

    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1()
            .map_err(|e| WinError::from_win("CreateDXGIFactory1", &e))?;

        let mut index = 0u32;
        while let Ok(adapter) = factory.EnumAdapters1(index) {
            index += 1;

            let desc: DXGI_ADAPTER_DESC1 = match adapter.GetDesc1() {
                Ok(d) => d,
                Err(_) => continue,
            };

            let is_software = DXGI_ADAPTER_FLAG(desc.Flags as i32) == DXGI_ADAPTER_FLAG_SOFTWARE;

            // Does this adapter own any outputs?
            let has_output = adapter.EnumOutputs(0).is_ok();

            let driver = drivers
                .iter()
                .find(|d| d.vendor_id == desc.VendorId && d.device_id == desc.DeviceId);

            out.push(GpuReport {
                name: wide_to_string(&desc.Description).trim().to_string(),
                vendor: vendor_name(desc.VendorId).to_string(),
                vendor_id: desc.VendorId,
                device_id: desc.DeviceId,
                subsystem_id: desc.SubSysId,
                revision: desc.Revision,
                dedicated_vram_bytes: desc.DedicatedVideoMemory as u64,
                shared_memory_bytes: desc.SharedSystemMemory as u64,
                is_software,
                has_output,
                driver_version: driver
                    .and_then(|d| d.version.clone())
                    .map(Reading::value)
                    .unwrap_or_else(Reading::unsupported),
                driver_date: driver
                    .and_then(|d| d.date.clone())
                    .map(Reading::value)
                    .unwrap_or_else(Reading::unsupported),
            });
        }
    }

    Ok(out)
}

// ---------------------------------------------------------------------------
// Driver details from the display adapter class registry
// ---------------------------------------------------------------------------

struct DriverEntry {
    vendor_id: u32,
    device_id: u32,
    version: Option<String>,
    date: Option<String>,
}

/// `{4d36e968-e325-11ce-bfc1-08002be10318}` is the display adapter setup class.
const DISPLAY_CLASS_KEY: &str =
    r"SYSTEM\CurrentControlSet\Control\Class\{4d36e968-e325-11ce-bfc1-08002be10318}";

fn read_display_drivers() -> Vec<DriverEntry> {
    use windows::core::{PCWSTR, PWSTR};
    use windows::Win32::System::Registry::{
        RegCloseKey, RegEnumKeyExW, RegOpenKeyExW, RegQueryValueExW, HKEY,
        HKEY_LOCAL_MACHINE, KEY_READ, REG_VALUE_TYPE,
    };

    let mut entries = Vec::new();

    unsafe {
        let mut class_key = HKEY::default();
        let path = crate::win::to_wide(DISPLAY_CLASS_KEY);
        if RegOpenKeyExW(
            HKEY_LOCAL_MACHINE,
            PCWSTR(path.as_ptr()),
            0,
            KEY_READ,
            &mut class_key,
        )
        .is_err()
        {
            return entries;
        }

        // Subkeys are the four-digit instance numbers 0000, 0001, ...
        for i in 0..64u32 {
            let mut name = [0u16; 256];
            let mut name_len = name.len() as u32;
            if RegEnumKeyExW(
                class_key,
                i,
                PWSTR(name.as_mut_ptr()),
                &mut name_len,
                None,
                PWSTR::null(),
                None,
                None,
            )
            .is_err()
            {
                break;
            }

            let subkey_path = format!(
                "{DISPLAY_CLASS_KEY}\\{}",
                wide_to_string(&name[..name_len as usize])
            );
            let subkey_wide = crate::win::to_wide(&subkey_path);
            let mut subkey = HKEY::default();
            if RegOpenKeyExW(
                HKEY_LOCAL_MACHINE,
                PCWSTR(subkey_wide.as_ptr()),
                0,
                KEY_READ,
                &mut subkey,
            )
            .is_err()
            {
                continue;
            }

            let read_string = |value: &str| -> Option<String> {
                let v = crate::win::to_wide(value);
                let mut buf = [0u8; 512];
                let mut len = buf.len() as u32;
                let mut kind = REG_VALUE_TYPE::default();
                let ok = RegQueryValueExW(
                    subkey,
                    PCWSTR(v.as_ptr()),
                    None,
                    Some(&mut kind),
                    Some(buf.as_mut_ptr()),
                    Some(&mut len),
                )
                .is_ok();
                if !ok || len < 2 {
                    return None;
                }
                let chars: Vec<u16> = buf[..len as usize]
                    .chunks_exact(2)
                    .map(|c| u16::from_le_bytes([c[0], c[1]]))
                    .collect();
                let s = wide_to_string(&chars).trim().to_string();
                if s.is_empty() {
                    None
                } else {
                    Some(s)
                }
            };

            // MatchingDeviceId looks like "pci\ven_1002&dev_73df".
            if let Some(matching) = read_string("MatchingDeviceId") {
                if let Some((vendor_id, device_id)) = parse_matching_device_id(&matching) {
                    entries.push(DriverEntry {
                        vendor_id,
                        device_id,
                        version: read_string("DriverVersion"),
                        date: read_string("DriverDate"),
                    });
                }
            }

            let _ = RegCloseKey(subkey);
        }

        let _ = RegCloseKey(class_key);
    }

    entries
}

/// Extract vendor and device IDs from a `MatchingDeviceId` string.
pub fn parse_matching_device_id(s: &str) -> Option<(u32, u32)> {
    let lower = s.to_ascii_lowercase();
    let ven = lower.find("ven_")?;
    let dev = lower.find("dev_")?;
    let vendor = u32::from_str_radix(lower.get(ven + 4..ven + 8)?, 16).ok()?;
    let device = u32::from_str_radix(lower.get(dev + 4..dev + 8)?, 16).ok()?;
    Some((vendor, device))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vendor_ids_map_to_names() {
        assert_eq!(vendor_name(0x10DE), "NVIDIA");
        assert_eq!(vendor_name(0x1002), "AMD");
        assert_eq!(vendor_name(0x8086), "Intel");
        assert_eq!(vendor_name(0x1414), "Microsoft");
    }

    #[test]
    fn parses_matching_device_id() {
        assert_eq!(
            parse_matching_device_id(r"pci\ven_1002&dev_73df"),
            Some((0x1002, 0x73DF))
        );
        // Case and trailing subsystem data must not matter.
        assert_eq!(
            parse_matching_device_id(r"PCI\VEN_10DE&DEV_2484&SUBSYS_38851043"),
            Some((0x10DE, 0x2484))
        );
        assert_eq!(parse_matching_device_id("not a device id"), None);
    }

    #[test]
    fn enumerates_at_least_one_adapter_on_this_machine() {
        let gpus = probe().expect("DXGI enumeration failed");
        assert!(!gpus.is_empty(), "no display adapters enumerated");
    }

    /// Guards the reason DXGI is used at all: `Win32_VideoController.AdapterRAM` is a
    /// 32-bit field that saturates at 4095 MB. Any hardware adapter reporting exactly
    /// that from DXGI would mean we had reintroduced the truncation.
    #[test]
    fn dedicated_vram_is_not_truncated_to_32_bits() {
        let gpus = probe().expect("DXGI enumeration failed");
        for g in gpus.iter().filter(|g| !g.is_software) {
            assert_ne!(
                g.dedicated_vram_bytes, 4_293_918_720,
                "{} reported the classic 32-bit truncated VRAM value",
                g.name
            );
        }
    }
}
