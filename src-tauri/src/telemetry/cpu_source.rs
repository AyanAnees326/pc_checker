//! Shared CPU MSR telemetry: opening a PawnIO MSR session and turning raw MSR reads
//! into effective clock / power / temperature readings.
//!
//! Extracted out of `stress::orchestrator` so the same PawnIO-session lifecycle and
//! APERF/MPERF/RAPL delta-tracking math is not duplicated between the stress
//! orchestrator's 4 Hz sampler and the standalone live CPU monitor (`live_monitor`) —
//! both need the exact same stateful math (each sample is a delta against the
//! previous one), and a second hand-copied version of it would be a real correctness
//! hazard, not just duplication.

use std::rc::Rc;
use std::time::Instant;

use crate::model::{Reading, Unavailable};
use crate::pawnio::{amd_msr::AmdMsr, intel_msr::IntelMsr, PawnIoLib};
use crate::probes::cpu::{CpuTopology, CpuVendor};

/// The four telemetry values one MSR sample produces, independent of whatever the
/// caller wraps them in (`stress::telemetry::types::CpuSample` for a stress run,
/// `CpuLiveSample` for the standalone monitor).
pub struct CpuTelemetryReadings {
    pub effective_clock_mhz: Reading<f64>,
    pub package_power_watts: Reading<f64>,
    pub configured_pl1_watts: Reading<f64>,
    pub configured_pl2_watts: Reading<f64>,
    pub package_temperature_c: Reading<i16>,
    pub thermal_throttling: Reading<bool>,
}

pub enum MsrSource {
    Intel(IntelMsr),
    Amd(AmdMsr),
    /// Carries the real reason telemetry is unavailable rather than a generic label —
    /// "the DLL never loaded" and "the DLL loaded but opening a session/module failed"
    /// are different failures with different fixes, and collapsing them into one
    /// hardcoded "PawnIO not installed" message (the previous behavior) makes a real
    /// problem indistinguishable from PawnIO simply not being present.
    Unavailable(Unavailable),
}

impl MsrSource {
    pub fn open(vendor: CpuVendor) -> Self {
        let lib = match PawnIoLib::load() {
            Ok(l) => Rc::new(l),
            Err(e) => {
                return MsrSource::Unavailable(Unavailable::DriverMissing(format!("PawnIOLib.dll: {e}")))
            }
        };

        match vendor {
            CpuVendor::Intel => IntelMsr::open(lib).map(MsrSource::Intel).unwrap_or_else(|e| {
                MsrSource::Unavailable(Unavailable::QueryFailed(format!(
                    "PawnIO session/module load failed: {e}"
                )))
            }),
            CpuVendor::Amd => AmdMsr::open(lib).map(MsrSource::Amd).unwrap_or_else(|e| {
                MsrSource::Unavailable(Unavailable::QueryFailed(format!(
                    "PawnIO session/module load failed: {e}"
                )))
            }),
            CpuVendor::Other => MsrSource::Unavailable(Unavailable::NotSupportedByHardware),
        }
    }
}

pub struct TelemetryState {
    prev_aperf: Option<u64>,
    prev_mperf: Option<u64>,
    prev_energy_raw: Option<u32>,
    prev_energy_at: Option<Instant>,
    tjmax: Option<u8>,
    /// `None` when no rated base clock could be established for this part, in which
    /// case the effective-clock reading reports *why* rather than emitting a number.
    base_clock_mhz: Option<u32>,
    /// AMD Tctl→Tdie correction for this specific part; `0.0` for most.
    tctl_offset_c: f64,
}

impl TelemetryState {
    pub fn new(msr: &MsrSource, topology: &CpuTopology) -> Self {
        let tjmax = match msr {
            MsrSource::Intel(m) => m.tjmax_celsius().ok(),
            _ => None,
        };

        // APERF/MPERF only yields a ratio; turning it into MHz needs the nominal
        // frequency. CPUID leaf 0x16 is authoritative where it exists (Intel), but
        // AMD does not implement it, so the bundled spec dataset is the fallback —
        // without which this used to multiply by a placeholder `1` and report the
        // bare ratio (e.g. "1.28") in a field labelled MHz.
        let base_clock_mhz = topology.base_clock_mhz.get().copied().or_else(|| {
            topology
                .brand_string
                .get()
                .and_then(|brand| crate::analysis::stress_findings::base_clock_mhz_for(brand))
        });

        let tctl_offset_c = topology
            .brand_string
            .get()
            .map(|brand| crate::pawnio::amd_msr::tctl_offset_celsius(brand))
            .unwrap_or(0.0);

        Self {
            prev_aperf: None,
            prev_mperf: None,
            prev_energy_raw: None,
            prev_energy_at: None,
            tjmax,
            base_clock_mhz,
            tctl_offset_c,
        }
    }

    pub fn sample(&mut self, msr: &MsrSource) -> CpuTelemetryReadings {
        match msr {
            MsrSource::Intel(m) => self.sample_intel(m),
            MsrSource::Amd(m) => self.sample_amd(m),
            MsrSource::Unavailable(reason) => self.unavailable_sample(reason),
        }
    }

    fn sample_intel(&mut self, m: &IntelMsr) -> CpuTelemetryReadings {
        let effective_clock_mhz = self.effective_clock(m.aperf().ok(), m.mperf().ok());
        let power_watts = self.package_power(m.rapl_units().ok(), m.package_energy_raw().ok());
        let limits = m.rapl_units().ok().and_then(|u| m.power_limits(&u).ok());
        let thermal = self.tjmax.and_then(|tj| m.thermal_status(tj).ok());
        // Best-effort: a failed clear must not stop the caller, only leave the sticky
        // log slightly stale for one tick.
        let _ = m.clear_thermal_log();

        blank_readings_with(
            effective_clock_mhz,
            power_watts,
            limits.map(|l| l.pl1_watts).into(),
            limits.map(|l| l.pl2_watts).into(),
            thermal.map(|t| t.temperature_celsius).into(),
            thermal.map(|t| t.throttling_now).into(),
        )
    }

    fn sample_amd(&mut self, m: &AmdMsr) -> CpuTelemetryReadings {
        let effective_clock_mhz = self.effective_clock(m.aperf().ok(), m.mperf().ok());
        let power_watts = self.package_power_amd(m.power_units().ok(), m.package_energy_raw().ok());

        // Zen exposes temperature through SMN rather than an MSR, so unlike the power
        // limits below this one *is* reachable. A failure here is reported with its
        // cause (most plausibly PCI-bus contention with another monitoring tool)
        // rather than as "unsupported", which would be a different claim entirely.
        let temperature = match m.package_temperature_celsius(self.tctl_offset_c) {
            Ok(c) => Reading::value(c.round() as i16),
            Err(e) => Reading::failed(e),
        };

        // AMDFamily17's allow-list genuinely has no power-limit or thermal-*throttle*
        // register (see `pawnio::amd_msr`'s doc comment) — reported as unsupported,
        // not silently omitted.
        blank_readings_with(
            effective_clock_mhz,
            power_watts,
            Reading::unsupported(),
            Reading::unsupported(),
            temperature,
            Reading::unsupported(),
        )
    }

    fn unavailable_sample(&self, reason: &Unavailable) -> CpuTelemetryReadings {
        // A closure here would monomorphize to a single `Reading<T>` at its first use
        // site and fail to type-check at the rest — each field needs its own call so
        // `Reading::missing`'s generic parameter is inferred separately per site.
        fn reading_for<T>(reason: &Unavailable) -> Reading<T> {
            Reading::missing(reason.clone())
        }
        blank_readings_with(
            reading_for(reason),
            reading_for(reason),
            reading_for(reason),
            reading_for(reason),
            reading_for(reason),
            reading_for(reason),
        )
    }

    fn effective_clock(&mut self, aperf: Option<u64>, mperf: Option<u64>) -> Reading<f64> {
        let (Some(aperf), Some(mperf)) = (aperf, mperf) else {
            return Reading::unsupported();
        };
        let result = match (self.prev_aperf, self.prev_mperf, self.base_clock_mhz) {
            (Some(pa), Some(pm), Some(base)) => {
                let da = aperf.wrapping_sub(pa);
                let dm = mperf.wrapping_sub(pm);
                Reading::value(crate::pawnio::intel_msr::effective_clock_mhz(da, dm, base))
            }
            // The counters read fine, but with no rated base clock the ratio cannot
            // be converted to MHz. Reporting the bare ratio in a megahertz field
            // would be a fabricated absolute value — exactly what `Reading` exists to
            // prevent — so this says what is missing instead.
            (Some(_), Some(_), None) => Reading::missing(Unavailable::QueryFailed(
                "no rated base clock for this CPU, so the APERF/MPERF ratio cannot be converted to MHz"
                    .into(),
            )),
            _ => Reading::missing(Unavailable::QueryFailed("first sample of the run".into())),
        };
        self.prev_aperf = Some(aperf);
        self.prev_mperf = Some(mperf);
        result
    }

    fn package_power(
        &mut self,
        units: Option<crate::pawnio::intel_msr::RaplUnits>,
        energy_raw: Option<u32>,
    ) -> Reading<f64> {
        let (Some(units), Some(raw)) = (units, energy_raw) else {
            return Reading::unsupported();
        };
        self.package_power_generic(raw, units.joules_per_unit)
    }

    fn package_power_amd(
        &mut self,
        units: Option<crate::pawnio::amd_msr::PowerUnits>,
        energy_raw: Option<u32>,
    ) -> Reading<f64> {
        let (Some(units), Some(raw)) = (units, energy_raw) else {
            return Reading::unsupported();
        };
        self.package_power_generic(raw, units.joules_per_unit)
    }

    fn package_power_generic(&mut self, raw: u32, joules_per_unit: f64) -> Reading<f64> {
        let now = Instant::now();
        let result = match (self.prev_energy_raw, self.prev_energy_at) {
            (Some(prev_raw), Some(prev_at)) => {
                let delta = raw.wrapping_sub(prev_raw); // 32-bit counter, wraps in place
                let dt = now.duration_since(prev_at).as_secs_f64();
                if dt > 0.0 {
                    Reading::value((delta as f64 * joules_per_unit) / dt)
                } else {
                    Reading::missing(Unavailable::QueryFailed("zero-duration sample interval".into()))
                }
            }
            _ => Reading::missing(Unavailable::QueryFailed("first sample of the run".into())),
        };
        self.prev_energy_raw = Some(raw);
        self.prev_energy_at = Some(now);
        result
    }
}

fn blank_readings_with(
    effective_clock_mhz: Reading<f64>,
    package_power_watts: Reading<f64>,
    configured_pl1_watts: Reading<f64>,
    configured_pl2_watts: Reading<f64>,
    package_temperature_c: Reading<i16>,
    thermal_throttling: Reading<bool>,
) -> CpuTelemetryReadings {
    CpuTelemetryReadings {
        effective_clock_mhz,
        package_power_watts,
        configured_pl1_watts,
        configured_pl2_watts,
        package_temperature_c,
        thermal_throttling,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn topology(brand: Option<&str>, base_clock: Option<u32>) -> CpuTopology {
        CpuTopology {
            vendor: CpuVendor::Amd,
            brand_string: match brand {
                Some(b) => Reading::value(b.to_string()),
                None => Reading::unsupported(),
            },
            physical_cores: Reading::value(6),
            logical_processors: 6,
            base_clock_mhz: match base_clock {
                Some(c) => Reading::value(c),
                None => Reading::unsupported(),
            },
        }
    }

    fn unavailable_source() -> MsrSource {
        MsrSource::Unavailable(Unavailable::NotApplicable)
    }

    #[test]
    fn cpuid_base_clock_wins_when_present() {
        let state = TelemetryState::new(&unavailable_source(), &topology(Some("whatever"), Some(2500)));
        assert_eq!(state.base_clock_mhz, Some(2500));
    }

    #[test]
    fn amd_base_clock_falls_back_to_the_spec_dataset() {
        // AMD implements no CPUID leaf 0x16, so the dataset is the only source. This
        // is the exact case that previously multiplied the APERF/MPERF ratio by a
        // placeholder of 1 and reported "1.28" in a field labelled MHz.
        let state = TelemetryState::new(
            &unavailable_source(),
            &topology(Some("AMD Ryzen 5 7500F 6-Core Processor"), None),
        );
        assert_eq!(state.base_clock_mhz, Some(3700));
    }

    #[test]
    fn an_unknown_part_has_no_base_clock_rather_than_a_placeholder() {
        let state = TelemetryState::new(
            &unavailable_source(),
            &topology(Some("Some Unreleased CPU 9999X"), None),
        );
        assert_eq!(state.base_clock_mhz, None);
    }

    #[test]
    fn without_a_base_clock_the_clock_reading_explains_itself_instead_of_reporting_a_ratio() {
        let mut state = TelemetryState::new(&unavailable_source(), &topology(None, None));

        // First sample only primes the deltas.
        let first = state.effective_clock(Some(1_000), Some(1_000));
        assert!(!first.is_ok());

        // Second sample has both deltas but still no base clock: a 1.28x ratio must
        // not surface as "1.28 MHz".
        let second = state.effective_clock(Some(2_280), Some(2_000));
        match second {
            Reading::Missing { note, .. } => {
                assert!(note.contains("base clock"), "note should name what is missing: {note}");
            }
            Reading::Ok { value, .. } => panic!("expected no reading, got {value}"),
        }
    }

    #[test]
    fn with_a_base_clock_the_ratio_becomes_real_megahertz() {
        let mut state =
            TelemetryState::new(&unavailable_source(), &topology(Some("ryzen 5 7500f"), None));

        let _ = state.effective_clock(Some(1_000), Some(1_000));
        // A 1.28x APERF/MPERF ratio against a 3700 MHz base is ~4736 MHz — a boosting
        // 7500F, not a 1.28 MHz one.
        let reading = state.effective_clock(Some(2_280), Some(2_000));
        let mhz = *reading.get().expect("expected a real clock reading");
        assert!(
            (4700.0..4800.0).contains(&mhz),
            "expected ~4736 MHz from a 1.28x ratio on a 3700 MHz base, got {mhz}"
        );
    }
}
