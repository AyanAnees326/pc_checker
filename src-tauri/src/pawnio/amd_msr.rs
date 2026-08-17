//! AMD MSR access via the `AMDFamily17` PawnIO module (Zen/Zen+/Zen2/Zen3/Zen4).
//!
//! Same allow-list safety boundary as `intel_msr` (see that module's doc comment for
//! why this matters), read from the actual `AMDFamily17.p` source rather than assumed.
//! AMD's allow-list is narrower in a specific way: there is no thermal-*throttle*
//! status register exposed at all (Intel's package thermal status has no AMD analogue
//! here), so the plan's "AMD throttle attribution is weaker than Intel's" is correct,
//! but the actual gap is "no throttle signal", not "a worse version of one".
//!
//! Allow-listed MSRs relevant here: `MSR_APERF_RO`/`MSR_MPERF_RO` (0xC00000E8/E7),
//! `MSR_PWR_UNIT` (0xC0010299), `MSR_PKG_ENERGY_STAT` (0xC001029B).
//!
//! Temperature is *not* an MSR on Zen — it lives in the SMN block, reached through
//! this module's separate `ioctl_read_smn` entry point. That call carries no address
//! allow-list of its own and drives machine-global PCI config-space registers, so
//! [`AmdMsr::read_smn`] holds [`crate::win::pci_lock`] and only ever passes constants
//! taken from the AMD PPR.

use std::rc::Rc;

use serde::{Deserialize, Serialize};
use windows::Win32::Foundation::HANDLE;

use super::ffi::PawnIoLib;
use crate::win::pci_lock::{self, PciBusLock};
use crate::win::{WinError, WinResult};

const MODULE_BLOB: &[u8] = include_bytes!("../../data/pawnio_modules/AMDFamily17.bin");

pub const MSR_APERF_RO: u32 = 0xC000_00E8;
pub const MSR_MPERF_RO: u32 = 0xC000_00E7;
pub const MSR_PWR_UNIT: u32 = 0xC001_0299;
pub const MSR_PKG_ENERGY_STAT: u32 = 0xC001_029B;

/// `Rc`, not `Arc`: only ever used from the single orchestrator thread that opens it
/// — see `IntelMsr`'s doc comment for why this matters.
pub struct AmdMsr {
    lib: Rc<PawnIoLib>,
    handle: HANDLE,
}

impl AmdMsr {
    pub fn open(lib: Rc<PawnIoLib>) -> WinResult<Self> {
        let handle = lib.open()?;
        lib.load_blob(handle, MODULE_BLOB)?;
        Ok(Self { lib, handle })
    }

    pub fn read_msr(&self, msr: u32) -> WinResult<u64> {
        let mut out = [0u64; 1];
        let written = self.lib.execute(self.handle, "ioctl_read_msr", &[msr as u64], &mut out)?;
        if written < 1 {
            return Err(WinError::new("ioctl_read_msr returned no data", 0));
        }
        Ok(out[0])
    }

    /// Same three-nibble power/energy/time-unit encoding AMD Zen shares with Intel
    /// RAPL (AMD PPR §2.1.13 "Power Reporting"), so the decode is structurally
    /// identical — reimplemented locally rather than shared with `intel_msr` so each
    /// vendor path stays independently correct if either encoding ever diverges.
    pub fn power_units(&self) -> WinResult<PowerUnits> {
        let raw = self.read_msr(MSR_PWR_UNIT)?;
        Ok(decode_power_units(raw))
    }

    pub fn package_energy_raw(&self) -> WinResult<u32> {
        Ok((self.read_msr(MSR_PKG_ENERGY_STAT)? & 0xFFFF_FFFF) as u32)
    }

    pub fn aperf(&self) -> WinResult<u64> {
        self.read_msr(MSR_APERF_RO)
    }

    pub fn mperf(&self) -> WinResult<u64> {
        self.read_msr(MSR_MPERF_RO)
    }

    /// Read one SMN register.
    ///
    /// Holds `Global\Access_PCI` for the duration: an SMN read is an index/data
    /// transaction through PCI config space, which is machine-global shared state.
    /// `AMDFamily17.p` applies no address allow-list to this call (unlike its MSR
    /// paths), so the address is entirely this crate's responsibility — every caller
    /// must use a constant from the AMD PPR, never a value derived from input.
    pub fn read_smn(&self, address: u32) -> WinResult<u32> {
        let _pci = PciBusLock::acquire(pci_lock::DEFAULT_TIMEOUT_MS)?;
        let mut out = [0u64; 1];
        let written = self
            .lib
            .execute(self.handle, "ioctl_read_smn", &[address as u64], &mut out)?;
        if written < 1 {
            return Err(WinError::new("ioctl_read_smn returned no data", 0));
        }
        Ok((out[0] & 0xFFFF_FFFF) as u32)
    }

    /// Package temperature (Tdie) in degrees Celsius.
    ///
    /// `tctl_offset_c` comes from [`tctl_offset_celsius`] — pass `0.0` for any part
    /// that does not need the correction.
    pub fn package_temperature_celsius(&self, tctl_offset_c: f64) -> WinResult<f64> {
        let raw = self.read_smn(ZEN_REPORTED_TEMP_CTRL_BASE)?;
        Ok(zen_temperature_celsius(raw, tctl_offset_c))
    }
}

impl Drop for AmdMsr {
    fn drop(&mut self) {
        self.lib.close(self.handle);
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct PowerUnits {
    pub watts_per_unit: f64,
    pub joules_per_unit: f64,
}

fn decode_power_units(raw: u64) -> PowerUnits {
    let power_bits = raw & 0xF;
    let energy_bits = (raw >> 8) & 0x1F;
    PowerUnits {
        watts_per_unit: 1.0 / (1u64 << power_bits) as f64,
        joules_per_unit: 1.0 / (1u64 << energy_bits) as f64,
    }
}

// --- Temperature -------------------------------------------------------------
// Zen reports temperature through SMN rather than an MSR, which is why the module
// doc comment's "no thermal register" is true of the MSR allow-list but not of the
// chip. Constants and decode below mirror the Linux kernel's `k10temp` driver
// (drivers/hwmon/k10temp.c), the authoritative open implementation, rather than
// being reverse-engineered here.

/// `ZEN_REPORTED_TEMP_CTRL_BASE` in k10temp — the reported-temperature control
/// register, valid across Family 17h and 19h (Zen 1 through Zen 4).
pub const ZEN_REPORTED_TEMP_CTRL_BASE: u32 = 0x0005_9800;

/// Raw temperature occupies bits 31:21, in 1/8 °C steps.
const ZEN_CUR_TEMP_SHIFT: u32 = 21;
/// `ZEN_CUR_TEMP_RANGE_SEL_MASK` — bit 19 selects the -49..206 °C range.
const ZEN_CUR_TEMP_RANGE_SEL_MASK: u32 = 1 << 19;
/// `ZEN_CUR_TEMP_TJ_SEL_MASK` — bits 17:16; both set also implies the offset range.
const ZEN_CUR_TEMP_TJ_SEL_MASK: u32 = 0b11 << 16;

/// Decode `THM_TCON_CURTMP` into degrees Celsius.
pub fn zen_temperature_celsius(raw: u32, tctl_offset_c: f64) -> f64 {
    let mut celsius = (raw >> ZEN_CUR_TEMP_SHIFT) as f64 * 0.125;
    if (raw & ZEN_CUR_TEMP_RANGE_SEL_MASK) != 0
        || (raw & ZEN_CUR_TEMP_TJ_SEL_MASK) == ZEN_CUR_TEMP_TJ_SEL_MASK
    {
        celsius -= 49.0;
    }
    celsius - tctl_offset_c
}

/// Parts whose reported Tctl is deliberately biased above true Tdie, so their fan
/// curves ramp earlier. Values from k10temp's `tctl_offset_table`.
///
/// This matters more here than in a general monitoring tool: these are all 2017-2018
/// parts, exactly the age of machine this app exists to inspect, and reporting a
/// 1800X 20 °C hotter than it really runs would manufacture a cooling "problem" that
/// is not there.
const TCTL_OFFSETS: &[(&str, f64)] = &[
    ("amd ryzen 5 1600x", 20.0),
    ("amd ryzen 7 1700x", 20.0),
    ("amd ryzen 7 1800x", 20.0),
    ("amd ryzen 7 2700x", 10.0),
    ("amd ryzen threadripper 19", 27.0),
    ("amd ryzen threadripper 29", 27.0),
];

/// The Tctl→Tdie correction for a CPU brand string; `0.0` when none applies.
pub fn tctl_offset_celsius(brand_string: &str) -> f64 {
    let normalized = brand_string.to_lowercase();
    TCTL_OFFSETS
        .iter()
        .find(|(id, _)| normalized.contains(id))
        .map(|(_, offset)| *offset)
        .unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn power_units_decode_the_shared_amd_intel_encoding() {
        let synth = 3u64 | (16u64 << 8);
        let u = decode_power_units(synth);
        assert!((u.watts_per_unit - 0.125).abs() < 1e-9);
        assert!((u.joules_per_unit - 1.0 / 65536.0).abs() < 1e-12);
    }

    /// Build a raw THM_TCON_CURTMP value for a given temperature in the default range.
    fn raw_for(celsius: f64) -> u32 {
        ((celsius / 0.125) as u32) << ZEN_CUR_TEMP_SHIFT
    }

    #[test]
    fn decodes_the_default_range_at_eighth_degree_resolution() {
        assert!((zen_temperature_celsius(raw_for(45.0), 0.0) - 45.0).abs() < 1e-9);
        assert!((zen_temperature_celsius(raw_for(88.625), 0.0) - 88.625).abs() < 1e-9);
    }

    #[test]
    fn range_select_bit_shifts_the_scale_down_by_49() {
        // Same 45 °C, encoded in the -49..206 range: the raw counts 94 °C worth of
        // eighths and bit 19 marks the offset range.
        let raw = raw_for(94.0) | ZEN_CUR_TEMP_RANGE_SEL_MASK;
        assert!((zen_temperature_celsius(raw, 0.0) - 45.0).abs() < 1e-9);
    }

    #[test]
    fn both_tj_sel_bits_set_also_selects_the_offset_range() {
        // k10temp treats TjSel == 0b11 as equivalent to the range-select bit; without
        // this branch such a part would read 49 °C too hot.
        let raw = raw_for(94.0) | ZEN_CUR_TEMP_TJ_SEL_MASK;
        assert!((zen_temperature_celsius(raw, 0.0) - 45.0).abs() < 1e-9);

        // Only one of the two bits is not enough.
        let partial = raw_for(94.0) | (1 << 16);
        assert!((zen_temperature_celsius(partial, 0.0) - 94.0).abs() < 1e-9);
    }

    #[test]
    fn tctl_offset_is_subtracted_so_biased_parts_report_true_tdie() {
        // An 1800X reporting Tctl 65 °C is really running at 45 °C.
        assert!((zen_temperature_celsius(raw_for(65.0), 20.0) - 45.0).abs() < 1e-9);
    }

    #[test]
    fn tctl_offsets_match_only_the_affected_parts() {
        assert_eq!(tctl_offset_celsius("AMD Ryzen 7 1800X Eight-Core Processor"), 20.0);
        assert_eq!(tctl_offset_celsius("AMD Ryzen 7 2700X Eight-Core Processor"), 10.0);
        assert_eq!(
            tctl_offset_celsius("AMD Ryzen Threadripper 1950X 16-Core Processor"),
            27.0
        );
        // Non-X and modern parts report Tdie directly and must not be shifted.
        assert_eq!(tctl_offset_celsius("AMD Ryzen 7 1700 Eight-Core Processor"), 0.0);
        assert_eq!(tctl_offset_celsius("AMD Ryzen 5 7500F 6-Core Processor"), 0.0);
    }

    #[test]
    fn the_temperature_register_address_matches_the_kernel_driver() {
        // ZEN_REPORTED_TEMP_CTRL_BASE in drivers/hwmon/k10temp.c.
        assert_eq!(ZEN_REPORTED_TEMP_CTRL_BASE, 0x0005_9800);
    }

    #[test]
    fn module_blob_is_bundled_and_nonempty() {
        assert!(!MODULE_BLOB.is_empty());
        assert_eq!(MODULE_BLOB.len(), 10652, "bundled AMDFamily17.bin size changed unexpectedly");
    }
}
