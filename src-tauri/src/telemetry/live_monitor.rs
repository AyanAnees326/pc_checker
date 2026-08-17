//! Standalone CPU metrics monitor: clock speed, wattage, and temperature reviewable
//! on demand, independent of the twelve-minute stress test.
//!
//! Before this existed, `CpuSample` (clock/power/temperature) was only ever produced
//! from inside `stress::orchestrator::run`'s phase schedule — there was no way to
//! just look at current CPU telemetry without committing to the full stress run. This
//! reuses the exact same MSR-sampling core (`telemetry::cpu_source`) at a lighter
//! ~1 Hz rate, since a monitoring view has no need for the stress sampler's 4 Hz.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::probes::cpu;
use crate::telemetry::cpu_source::{MsrSource, TelemetryState};
use crate::telemetry::types::CpuLiveSample;

const SAMPLE_INTERVAL: Duration = Duration::from_secs(1);

/// Sample CPU telemetry at ~1 Hz until `cancel` fires, calling `on_sample` from this
/// thread as each sample is taken. Never returns an error: exactly like the stress
/// orchestrator, a machine without PawnIO installed still produces samples — every
/// telemetry field is just `Reading::missing` on all of them, not a failure to start.
pub fn run(cancel: &AtomicBool, on_sample: impl FnMut(CpuLiveSample)) {
    run_with_interval(SAMPLE_INTERVAL, cancel, on_sample);
}

fn run_with_interval(interval: Duration, cancel: &AtomicBool, mut on_sample: impl FnMut(CpuLiveSample)) {
    let topology = cpu::probe();
    let msr = MsrSource::open(topology.vendor);
    let mut telemetry = TelemetryState::new(&msr, &topology);
    let start = Instant::now();

    while !cancel.load(Ordering::Relaxed) {
        let tick_start = Instant::now();
        let readings = telemetry.sample(&msr);

        on_sample(CpuLiveSample {
            elapsed_ms: start.elapsed().as_millis() as u64,
            effective_clock_mhz: readings.effective_clock_mhz,
            package_power_watts: readings.package_power_watts,
            package_temperature_c: readings.package_temperature_c,
            thermal_throttling: readings.thermal_throttling,
        });

        let elapsed = tick_start.elapsed();
        if elapsed < interval {
            std::thread::sleep(interval - elapsed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    /// This dev machine does not have PawnIO installed — the same production case
    /// `orchestrator`'s equivalent test exercises. The monitor must still tick and
    /// produce samples with every telemetry field gracefully missing, not stall.
    #[test]
    fn monitor_produces_samples_and_stops_on_cancel() {
        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_for_setter = Arc::clone(&cancel);

        thread::spawn(move || {
            thread::sleep(Duration::from_millis(120));
            cancel_for_setter.store(true, Ordering::Relaxed);
        });

        let mut samples = Vec::new();
        run_with_interval(Duration::from_millis(20), &cancel, |s| samples.push(s));

        assert!(!samples.is_empty(), "monitor produced no samples before cancellation");
        assert!(!samples[0].effective_clock_mhz.is_ok());
    }
}
