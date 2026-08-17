//! Intel MSR access via the `IntelMSR` PawnIO module.
//!
//! The module's compiled bytecode is bundled with the app (`data/pawnio_modules/IntelMSR.bin`,
//! LGPL-2.1-or-later, built from the `IntelMSR.p` source in `namazso/PawnIO.Modules`) so a
//! stress test works offline. That source hardcodes an MSR allow-list
//! (`is_allowed_msr_read`/`is_allowed_msr_write`) as a safety boundary; reads outside it fail
//! with `STATUS_ACCESS_DENIED`. Notably, **`MSR_CORE_PERF_LIMIT_REASONS` (0x64F) is not on the
//! allow-list** — the original plan assumed byte-level PL1/PL2/VR/EDP throttle attribution from
//! that register, which turned out not to be reachable through the real module. What follows
//! instead is built entirely from what the allow-list actually permits.
//!
//! Bit layouts below are the stable, cross-generation encodings from Intel SDM Vol. 3B,
//! §14.9 (RAPL) and §16.6 (thermal monitoring).

use std::rc::Rc;

use serde::{Deserialize, Serialize};
use windows::Win32::Foundation::HANDLE;

use super::ffi::PawnIoLib;
use crate::win::{WinError, WinResult};

const MODULE_BLOB: &[u8] = include_bytes!("../../data/pawnio_modules/IntelMSR.bin");

// Allow-listed MSRs (see module doc comment for why this specific set).
pub const MSR_IA32_MPERF: u32 = 0x0000_00E7;
pub const MSR_IA32_APERF: u32 = 0x0000_00E8;
pub const MSR_RAPL_POWER_UNIT: u32 = 0x0000_0606;
pub const MSR_PKG_POWER_LIMIT: u32 = 0x0000_0610;
pub const MSR_PKG_ENERGY_STATUS: u32 = 0x0000_0611;
pub const MSR_IA32_TEMPERATURE_TARGET: u32 = 0x0000_01A2;
pub const MSR_IA32_PACKAGE_THERM_STATUS: u32 = 0x0000_01B1;

/// One open executor with `IntelMSR.bin` loaded. Closes itself on drop.
///
/// Owns an `Rc<PawnIoLib>` rather than borrowing one: the alternative (a lifetime
/// parameter tying this to its library) forces every caller that wants to store an
/// `IntelMsr` alongside its library in the same struct into a self-referential type,
/// which is exactly the kind of thing that produces a subtle double-free — as it did
/// here during development, via a `Box::leak` + `ptr::read` workaround that silently
/// duplicated ownership of the same driver handle. `Rc` (not `Arc`: this is only ever
/// used from the single orchestrator thread that opens it, never shared across
/// threads) sidesteps the whole problem.
pub struct IntelMsr {
    lib: Rc<PawnIoLib>,
    handle: HANDLE,
}

impl IntelMsr {
    pub fn open(lib: Rc<PawnIoLib>) -> WinResult<Self> {
        let handle = lib.open()?;
        lib.load_blob(handle, MODULE_BLOB)?;
        Ok(Self { lib, handle })
    }

    /// Read a raw MSR. `STATUS_ACCESS_DENIED` (from the module's own allow-list) surfaces
    /// as a normal `WinResult` error rather than a panic — callers outside the allow-list
    /// are a programming mistake in this codebase, not a hardware fault, but should still
    /// degrade cleanly rather than crash a multi-minute stress run.
    pub fn read_msr(&self, msr: u32) -> WinResult<u64> {
        let mut out = [0u64; 1];
        let written = self.lib.execute(self.handle, "ioctl_read_msr", &[msr as u64], &mut out)?;
        if written < 1 {
            return Err(WinError::new("ioctl_read_msr returned no data", 0));
        }
        Ok(out[0])
    }

    fn write_msr(&self, msr: u32, value: u64) -> WinResult<()> {
        let mut out = [0u64; 0];
        self.lib
            .execute(self.handle, "ioctl_write_msr", &[msr as u64, value], &mut out)?;
        Ok(())
    }

    /// Clear the sticky thermal-status log bit (bit 1 of 0x1B1) so the next sample window
    /// only reflects throttling that happened *since* this call, rather than accumulating
    /// forever across the whole stress run.
    pub fn clear_thermal_log(&self) -> WinResult<()> {
        let current = self.read_msr(MSR_IA32_PACKAGE_THERM_STATUS)?;
        self.write_msr(MSR_IA32_PACKAGE_THERM_STATUS, current & !0b10)
    }

    pub fn rapl_units(&self) -> WinResult<RaplUnits> {
        Ok(decode_rapl_units(self.read_msr(MSR_RAPL_POWER_UNIT)?))
    }

    pub fn package_energy_raw(&self) -> WinResult<u32> {
        // Bits 31:0 hold the counter; upper bits are reserved.
        Ok((self.read_msr(MSR_PKG_ENERGY_STATUS)? & 0xFFFF_FFFF) as u32)
    }

    pub fn power_limits(&self, units: &RaplUnits) -> WinResult<PowerLimits> {
        Ok(decode_power_limits(self.read_msr(MSR_PKG_POWER_LIMIT)?, units))
    }

    pub fn tjmax_celsius(&self) -> WinResult<u8> {
        Ok(decode_tjmax(self.read_msr(MSR_IA32_TEMPERATURE_TARGET)?))
    }

    pub fn thermal_status(&self, tjmax: u8) -> WinResult<ThermalStatus> {
        Ok(decode_thermal_status(
            self.read_msr(MSR_IA32_PACKAGE_THERM_STATUS)?,
            tjmax,
        ))
    }

    pub fn aperf(&self) -> WinResult<u64> {
        self.read_msr(MSR_IA32_APERF)
    }

    pub fn mperf(&self) -> WinResult<u64> {
        self.read_msr(MSR_IA32_MPERF)
    }
}

impl Drop for IntelMsr {
    fn drop(&mut self) {
        self.lib.close(self.handle);
    }
}

// ---------------------------------------------------------------------------
// Decoding
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct RaplUnits {
    pub watts_per_unit: f64,
    pub joules_per_unit: f64,
    pub seconds_per_unit: f64,
}

/// MSR_RAPL_POWER_UNIT (0x606): bits 3:0 power unit, bits 12:8 energy unit,
/// bits 19:16 time unit — each is `1 / 2^n` in its respective SI unit.
fn decode_rapl_units(raw: u64) -> RaplUnits {
    let power_bits = raw & 0xF;
    let energy_bits = (raw >> 8) & 0x1F;
    let time_bits = (raw >> 16) & 0xF;
    RaplUnits {
        watts_per_unit: 1.0 / (1u64 << power_bits) as f64,
        joules_per_unit: 1.0 / (1u64 << energy_bits) as f64,
        seconds_per_unit: 1.0 / (1u64 << time_bits) as f64,
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct PowerLimits {
    /// The firmware's own configured sustained-power cap, in watts.
    pub pl1_watts: f64,
    pub pl1_enabled: bool,
    /// The firmware's own configured short-term boost cap, in watts.
    pub pl2_watts: f64,
    pub pl2_enabled: bool,
}

/// MSR_PKG_POWER_LIMIT (0x610): bits 14:0 = PL1 power (power units), bit 15 = PL1 enable,
/// bits 46:32 = PL2 power (power units), bit 47 = PL2 enable. Time-window fields are not
/// decoded — the wattage caps are what the throttle analysis needs.
fn decode_power_limits(raw: u64, units: &RaplUnits) -> PowerLimits {
    let pl1_raw = raw & 0x7FFF;
    let pl1_enabled = (raw >> 15) & 1 != 0;
    let pl2_raw = (raw >> 32) & 0x7FFF;
    let pl2_enabled = (raw >> 47) & 1 != 0;
    PowerLimits {
        pl1_watts: pl1_raw as f64 * units.watts_per_unit,
        pl1_enabled,
        pl2_watts: pl2_raw as f64 * units.watts_per_unit,
        pl2_enabled,
    }
}

/// MSR_IA32_TEMPERATURE_TARGET (0x1A2): bits 23:16 hold Tjmax in degrees Celsius.
fn decode_tjmax(raw: u64) -> u8 {
    ((raw >> 16) & 0xFF) as u8
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct ThermalStatus {
    /// The package is throttling for thermal reasons right now.
    pub throttling_now: bool,
    /// The package has throttled for thermal reasons at some point since the log was
    /// last cleared — see [`IntelMsr::clear_thermal_log`].
    pub throttled_since_clear: bool,
    pub temperature_celsius: i16,
}

/// MSR_IA32_PACKAGE_THERM_STATUS (0x1B1): bit 0 = live thermal-throttle status, bit 1 =
/// sticky log, bits 22:16 = digital readout as `Tjmax - value`.
fn decode_thermal_status(raw: u64, tjmax: u8) -> ThermalStatus {
    let throttling_now = raw & 0b1 != 0;
    let throttled_since_clear = (raw >> 1) & 0b1 != 0;
    let readout = ((raw >> 16) & 0x7F) as i16;
    ThermalStatus {
        throttling_now,
        throttled_since_clear,
        temperature_celsius: tjmax as i16 - readout,
    }
}

/// True effective CPU frequency over a sampling window, from the ratio of APERF to
/// MPERF deltas times the nominal (base) frequency — this reflects actual time spent
/// executing at each P-state, including C-state residency, unlike the raw reported
/// clock. Both counters wrap; callers must use wrapping subtraction across samples.
pub fn effective_clock_mhz(aperf_delta: u64, mperf_delta: u64, base_clock_mhz: u32) -> f64 {
    if mperf_delta == 0 {
        return 0.0;
    }
    (aperf_delta as f64 / mperf_delta as f64) * base_clock_mhz as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rapl_units_decode_the_common_intel_encoding() {
        // A very common real encoding: power unit 1/8 W (bits=3), energy unit 1/2^14 J
        // (bits=14), time unit 1/2^10 s (bits=10) — Skylake-era client parts.
        let raw = 0x0000_0000_000A_1003u64; // power=3, energy=0x10=16... constructed below instead
        let _ = raw;
        let synth = (3u64) | (16u64 << 8) | (10u64 << 16);
        let u = decode_rapl_units(synth);
        assert!((u.watts_per_unit - 0.125).abs() < 1e-9);
        assert!((u.joules_per_unit - 1.0 / 65536.0).abs() < 1e-12);
        assert!((u.seconds_per_unit - 1.0 / 1024.0).abs() < 1e-9);
    }

    #[test]
    fn power_limits_decode_wattage_from_raw_units() {
        let units = RaplUnits {
            watts_per_unit: 0.125,
            joules_per_unit: 1.0,
            seconds_per_unit: 1.0,
        };
        // PL1 = 200 units (25W), enabled; PL2 = 400 units (50W), enabled.
        let raw = 200u64 | (1u64 << 15) | (400u64 << 32) | (1u64 << 47);
        let limits = decode_power_limits(raw, &units);
        assert!((limits.pl1_watts - 25.0).abs() < 1e-9);
        assert!(limits.pl1_enabled);
        assert!((limits.pl2_watts - 50.0).abs() < 1e-9);
        assert!(limits.pl2_enabled);
    }

    #[test]
    fn power_limit_disabled_bit_is_read_correctly() {
        let units = RaplUnits { watts_per_unit: 1.0, joules_per_unit: 1.0, seconds_per_unit: 1.0 };
        let raw = 100u64; // PL1 enable bit (15) left clear
        let limits = decode_power_limits(raw, &units);
        assert!(!limits.pl1_enabled);
    }

    #[test]
    fn tjmax_reads_the_correct_byte() {
        let raw = 100u64 << 16; // Tjmax = 100 C
        assert_eq!(decode_tjmax(raw), 100);
    }

    #[test]
    fn thermal_status_decodes_live_and_sticky_bits_independently() {
        let tjmax = 100u8;
        // Not throttling, never has, readout=20 -> 80 C.
        let idle = decode_thermal_status(20u64 << 16, tjmax);
        assert!(!idle.throttling_now);
        assert!(!idle.throttled_since_clear);
        assert_eq!(idle.temperature_celsius, 80);

        // Throttling now AND sticky log set, readout=2 -> 98 C (near Tjmax, as expected
        // when actually thermal-throttling).
        let hot = decode_thermal_status(0b11 | (2u64 << 16), tjmax);
        assert!(hot.throttling_now);
        assert!(hot.throttled_since_clear);
        assert_eq!(hot.temperature_celsius, 98);

        // Sticky log set from an earlier spike, but not throttling right now — this is
        // exactly the case `clear_thermal_log` exists to distinguish from "still hot".
        let recovered = decode_thermal_status(0b10 | (30u64 << 16), tjmax);
        assert!(!recovered.throttling_now);
        assert!(recovered.throttled_since_clear);
    }

    #[test]
    fn effective_clock_matches_base_when_fully_active() {
        // APERF == MPERF means the core spent the whole window at the nominal ratio.
        let mhz = effective_clock_mhz(1_000_000, 1_000_000, 2_500);
        assert!((mhz - 2_500.0).abs() < 0.01);
    }

    #[test]
    fn effective_clock_reflects_partial_c_state_residency() {
        // Half the MPERF ticks resulted in APERF progress -> half the nominal clock,
        // e.g. because the core spent half the window in a C-state.
        let mhz = effective_clock_mhz(500_000, 1_000_000, 2_500);
        assert!((mhz - 1_250.0).abs() < 0.01);
    }

    #[test]
    fn zero_mperf_delta_does_not_divide_by_zero() {
        assert_eq!(effective_clock_mhz(0, 0, 2_500), 0.0);
    }

    #[test]
    fn module_blob_is_bundled_and_nonempty() {
        assert!(!MODULE_BLOB.is_empty());
        assert_eq!(MODULE_BLOB.len(), 5324, "bundled IntelMSR.bin size changed unexpectedly");
    }
}
