//! GPU stress phase scheduler — the same shape as `stress::orchestrator` (CPU), with
//! one structural difference: a GPU has no per-core concept, so there is exactly one
//! worker thread driving the compute queue rather than one thread per core.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use windows::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};

use crate::model::{Reading, Unavailable};
use crate::probes::adl::AdlSession;
use crate::probes::nvml::{NvmlLib, ThrottleReasons};
use crate::stress::gpu_kernel::{self, GpuWorkerStats};
use crate::telemetry::types::{GpuPhase, GpuSample, GpuStressResult};

#[derive(Debug, Clone)]
pub struct GpuStressConfig {
    pub idle_baseline: Duration,
    pub compute_sustained: Duration,
    pub cooldown: Duration,
    pub sample_interval: Duration,
}

impl GpuStressConfig {
    /// ~10 minutes total. No separate ramp phase — full compute load starts right
    /// after a brief idle-baseline reading, not 60+ seconds in. See `StressConfig`
    /// (CPU orchestrator) for the same reasoning: a Start button that doesn't
    /// visibly start anything for a minute reads as broken.
    pub fn standard() -> Self {
        Self {
            idle_baseline: Duration::from_secs(5),
            compute_sustained: Duration::from_secs(9 * 60) + Duration::from_secs(25),
            cooldown: Duration::from_secs(90),
            sample_interval: Duration::from_millis(250),
        }
    }

    /// A fresh `GpuComputeContext` is created per phase (device + shader compile),
    /// so the loaded phase needs enough headroom to pay that setup cost and still
    /// complete at least one checkpoint — 2s is comfortably above what device
    /// creation plus one 64 MiB round trip takes on real hardware.
    #[cfg(test)]
    pub fn quick_test() -> Self {
        Self {
            idle_baseline: Duration::from_millis(100),
            compute_sustained: Duration::from_secs(2),
            cooldown: Duration::from_millis(100),
            sample_interval: Duration::from_millis(50),
        }
    }
}

const MIN_BATTERY_PERCENT_ON_DC: u8 = 20;
const NVIDIA_VENDOR_ID: u32 = 0x10DE;

pub fn run(
    vendor_id: u32,
    device_id: u32,
    config: &GpuStressConfig,
    cancel: &Arc<AtomicBool>,
    mut on_sample: impl FnMut(GpuSample),
) -> GpuStressResult {
    // See `win::power`'s module doc comment — same reasoning as the CPU orchestrator.
    let _throttle_guard = crate::win::power::ExecutionSpeedGuard::engage();

    let telemetry = GpuTelemetry::open(vendor_id, device_id);
    let start = Instant::now();
    let mut samples = Vec::new();
    let mut abort: Option<String> = None;
    let mut self_check_failed = false;

    let phases: [(GpuPhase, Duration, bool); 3] = [
        (GpuPhase::IdleBaseline, config.idle_baseline, false),
        (GpuPhase::ComputeSustained, config.compute_sustained, true),
        (GpuPhase::Cooldown, config.cooldown, false),
    ];

    'phases: for (phase, duration, load) in phases {
        if cancel.load(Ordering::Relaxed) {
            abort = Some("cancelled by user".into());
            break;
        }

        let phase_deadline = Instant::now() + duration;
        let worker = if load {
            Some(spawn_worker(vendor_id, device_id, phase_deadline, cancel))
        } else {
            None
        };

        loop {
            let tick_start = Instant::now();
            if tick_start >= phase_deadline {
                break;
            }
            if cancel.load(Ordering::Relaxed) {
                abort = Some("cancelled by user".into());
                break 'phases;
            }
            if let Some(reason) = check_abort_conditions() {
                abort = Some(reason);
                cancel.store(true, Ordering::Relaxed);
                break 'phases;
            }

            let (dispatches, any_fault) = worker
                .as_ref()
                .map(|w| {
                    (
                        w.stats.dispatches.load(Ordering::Relaxed),
                        w.stats.fault.load(Ordering::Relaxed),
                    )
                })
                .unwrap_or((0, false));
            if any_fault {
                self_check_failed = true;
            }

            let sample = GpuSample {
                elapsed_ms: start.elapsed().as_millis() as u64,
                phase,
                self_check_ok: if worker.is_some() { Some(!any_fault) } else { None },
                dispatches_completed: dispatches,
                ..telemetry.sample()
            };
            on_sample(sample.clone());
            samples.push(sample);

            let elapsed = tick_start.elapsed();
            if elapsed < config.sample_interval {
                std::thread::sleep(config.sample_interval - elapsed);
            }
        }

        if let Some(w) = worker {
            let _ = w.handle.join();
        }
    }

    GpuStressResult {
        samples,
        aborted: abort.is_some(),
        abort_reason: abort,
        self_check_failed,
    }
}

struct RunningWorker {
    stats: Arc<GpuWorkerStats>,
    handle: std::thread::JoinHandle<()>,
}

fn spawn_worker(vendor_id: u32, device_id: u32, deadline: Instant, cancel: &Arc<AtomicBool>) -> RunningWorker {
    let stats = Arc::new(GpuWorkerStats::default());
    let stats_for_thread = Arc::clone(&stats);
    let cancel_for_thread = Arc::clone(cancel);

    let handle = std::thread::Builder::new()
        .name("pc-checker-gpu-stress".into())
        .spawn(move || {
            // Setup failure (no such adapter, device creation failed) surfaces as a
            // fault on the very first sample rather than a silent no-op run — a
            // buyer seeing "0 dispatches, self-check: unavailable" for a whole
            // sustained phase needs to know that happened, not read it as a pass.
            if gpu_kernel::run_worker(vendor_id, device_id, deadline, &cancel_for_thread, &stats_for_thread).is_err() {
                stats_for_thread.fault.store(true, Ordering::Relaxed);
            }
        })
        .expect("failed to spawn GPU stress worker thread");

    RunningWorker { stats, handle }
}

fn check_abort_conditions() -> Option<String> {
    let mut status = SYSTEM_POWER_STATUS::default();
    if unsafe { GetSystemPowerStatus(&mut status) }.is_err() {
        return None;
    }
    let on_battery = status.ACLineStatus == 0;
    let battery_known = status.BatteryLifePercent != 255;
    if on_battery && battery_known && status.BatteryLifePercent < MIN_BATTERY_PERCENT_ON_DC {
        return Some(format!(
            "battery at {}% on DC power — aborting before it drops further",
            status.BatteryLifePercent
        ));
    }
    None
}

// ---------------------------------------------------------------------------
// Telemetry
// ---------------------------------------------------------------------------

const AMD_VENDOR_ID: u32 = 0x1002;

enum GpuTelemetry {
    Nvidia { lib: NvmlLib, device: *mut std::ffi::c_void },
    Amd(AdlSession),
    /// NVML/ADL absent, or the vendor is neither (Intel — IGCL integration is a
    /// documented future addition, not attempted this pass, same reasoning as ADLX
    /// in `probes::nvml`'s module doc comment on not guessing an unfamiliar ABI).
    Unavailable,
}

impl GpuTelemetry {
    fn open(vendor_id: u32, device_id: u32) -> Self {
        if vendor_id == NVIDIA_VENDOR_ID {
            return match NvmlLib::open().and_then(|lib| {
                let device = lib.find_device(vendor_id, device_id)?;
                Ok((lib, device))
            }) {
                Ok((lib, device)) => GpuTelemetry::Nvidia { lib, device },
                Err(_) => GpuTelemetry::Unavailable,
            };
        }
        if vendor_id == AMD_VENDOR_ID {
            return match AdlSession::open() {
                Ok(session) => GpuTelemetry::Amd(session),
                Err(_) => GpuTelemetry::Unavailable,
            };
        }
        GpuTelemetry::Unavailable
    }

    fn sample(&self) -> GpuSample {
        match self {
            GpuTelemetry::Nvidia { lib, device } => {
                // Safety: `device` was obtained from this same `lib` via
                // `find_device` in `GpuTelemetry::open` and does not outlive it.
                let (reasons, clock, power, temp, fan) = unsafe {
                    (
                        lib.throttle_reasons(*device).ok(),
                        lib.graphics_clock_mhz(*device),
                        lib.power_watts(*device),
                        lib.temperature_c(*device),
                        lib.fan_percent(*device),
                    )
                };
                blank_sample(TelemetryReadings {
                    graphics_clock_mhz: clock.into(),
                    power_watts: power.into(),
                    edge_temperature_c: temp.into(),
                    // NVML exposes no per-die junction sensor on consumer cards —
                    // see `GpuSample::hotspot_temperature_c`'s doc comment.
                    hotspot_temperature_c: Reading::unsupported(),
                    // No RPM equivalent in base NVML — see `NvmlLib::fan_percent`.
                    fan_rpm: Reading::unsupported(),
                    fan_percent: fan.into(),
                    sw_thermal_slowdown: reading_bool(reasons, |r| r.sw_thermal_slowdown),
                    hw_thermal_slowdown: reading_bool(reasons, |r| r.hw_thermal_slowdown),
                    sw_power_cap: reading_bool(reasons, |r| r.sw_power_cap),
                    hw_power_brake: reading_bool(reasons, |r| r.hw_power_brake),
                })
            }
            GpuTelemetry::Amd(session) => {
                // ADL's PMLog throttler-status sensor is a single coarse bit, not
                // broken into distinct causes the way NVML's bitmask is — mapped onto
                // `hw_thermal_slowdown` as the closest honest fit (GPU throttling is
                // most commonly thermal-driven), not a claim this app actually knows
                // the specific cause on AMD the way it does on NVIDIA.
                let throttling = session.throttle_status();
                blank_sample(TelemetryReadings {
                    graphics_clock_mhz: session.graphics_clock_mhz().into(),
                    power_watts: session.power_watts().into(),
                    edge_temperature_c: session.edge_temperature_c().into(),
                    hotspot_temperature_c: session.hotspot_temperature_c().into(),
                    fan_rpm: session.fan_rpm().into(),
                    fan_percent: session.fan_percent().into(),
                    sw_thermal_slowdown: Reading::unsupported(),
                    hw_thermal_slowdown: throttling.map(|t| t.any_throttling).into(),
                    sw_power_cap: Reading::unsupported(),
                    hw_power_brake: Reading::unsupported(),
                })
            }
            GpuTelemetry::Unavailable => {
                fn reason<T>() -> Reading<T> {
                    Reading::missing(Unavailable::VendorLibraryMissing(
                        "no vendor telemetry available (NVML/ADL absent, or GPU is neither NVIDIA nor AMD)".into(),
                    ))
                }
                blank_sample(TelemetryReadings {
                    graphics_clock_mhz: reason(),
                    power_watts: reason(),
                    edge_temperature_c: reason(),
                    hotspot_temperature_c: reason(),
                    fan_rpm: reason(),
                    fan_percent: reason(),
                    sw_thermal_slowdown: reason(),
                    hw_thermal_slowdown: reason(),
                    sw_power_cap: reason(),
                    hw_power_brake: reason(),
                })
            }
        }
    }
}

fn reading_bool(reasons: Option<ThrottleReasons>, f: impl Fn(&ThrottleReasons) -> bool) -> Reading<bool> {
    match reasons {
        Some(r) => Reading::value(f(&r)),
        None => Reading::unsupported(),
    }
}

/// One vendor-agnostic snapshot of everything `GpuTelemetry::sample` can read, before
/// the fixed `elapsed_ms`/`phase`/`self_check_ok`/`dispatches_completed` fields the
/// orchestrator's own sampler loop fills in are attached. A plain struct rather than
/// ten positional arguments to `blank_sample` — past ~7 same-typed arguments,
/// positional passing is how two get silently swapped.
struct TelemetryReadings {
    graphics_clock_mhz: Reading<u32>,
    power_watts: Reading<f64>,
    edge_temperature_c: Reading<u32>,
    hotspot_temperature_c: Reading<u32>,
    fan_rpm: Reading<u32>,
    fan_percent: Reading<u32>,
    sw_thermal_slowdown: Reading<bool>,
    hw_thermal_slowdown: Reading<bool>,
    sw_power_cap: Reading<bool>,
    hw_power_brake: Reading<bool>,
}

fn blank_sample(r: TelemetryReadings) -> GpuSample {
    GpuSample {
        elapsed_ms: 0,
        phase: GpuPhase::IdleBaseline,
        graphics_clock_mhz: r.graphics_clock_mhz,
        power_watts: r.power_watts,
        edge_temperature_c: r.edge_temperature_c,
        hotspot_temperature_c: r.hotspot_temperature_c,
        fan_rpm: r.fan_rpm,
        fan_percent: r.fan_percent,
        sw_thermal_slowdown: r.sw_thermal_slowdown,
        hw_thermal_slowdown: r.hw_thermal_slowdown,
        sw_power_cap: r.sw_power_cap,
        hw_power_brake: r.hw_power_brake,
        self_check_ok: None,
        dispatches_completed: 0,
    }
}

// Safety: NVML device handles are opaque pointers owned by the driver, valid for the
// process lifetime after nvmlInit — reading through them from the single orchestrator
// thread that opened them is the only use here, matching NVML's own documented
// thread-safety contract (a device handle may be used from any thread).
unsafe impl Send for GpuTelemetry {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::probes::gpu;

    fn first_hardware_gpu() -> (u32, u32) {
        let gpus = gpu::probe().expect("DXGI enumeration failed");
        let g = gpus.iter().find(|g| !g.is_software).expect("no hardware GPU on this machine");
        (g.vendor_id, g.device_id)
    }

    /// This dev machine has an AMD GPU: telemetry must degrade cleanly to
    /// `VendorLibraryMissing` on every field while the compute self-check (which
    /// needs no vendor library at all) still runs and passes.
    #[test]
    fn full_schedule_runs_and_reads_real_amd_telemetry_on_this_machine() {
        let _gpu = crate::probes::gpu_d3d12::gpu_test_guard();
        let (vendor_id, device_id) = first_hardware_gpu();
        let cancel = Arc::new(AtomicBool::new(false));
        let mut count = 0usize;

        let result = run(vendor_id, device_id, &GpuStressConfig::quick_test(), &cancel, |_| count += 1);

        assert!(!result.aborted, "unexpected abort: {:?}", result.abort_reason);
        assert!(!result.self_check_failed, "self-check must not fail on healthy hardware");
        assert!(!result.samples.is_empty());
        assert_eq!(count, result.samples.len());

        for phase in [GpuPhase::IdleBaseline, GpuPhase::ComputeSustained, GpuPhase::Cooldown] {
            assert!(result.samples.iter().any(|s| s.phase == phase), "missing samples for {phase:?}");
        }

        let sustained_max = result
            .samples
            .iter()
            .filter(|s| s.phase == GpuPhase::ComputeSustained)
            .map(|s| s.dispatches_completed)
            .max()
            .unwrap_or(0);
        assert!(sustained_max > 0, "no dispatches recorded during the sustained phase");

        // This dev machine's real AMD GPU: telemetry now comes from `probes::adl`
        // (see that module for why the "AMD is always Unavailable" assumption this
        // test used to encode was corrected), so clock should be a real, plausible
        // reading rather than missing.
        let mid = &result.samples[result.samples.len() / 2];
        let clock = mid
            .graphics_clock_mhz
            .get()
            .copied()
            .expect("expected a real ADL clock reading on this machine's AMD GPU");
        assert!(clock > 0 && clock < 5000, "implausible graphics clock: {clock} MHz");
    }

    #[test]
    fn cancellation_stops_the_run_promptly() {
        let _gpu = crate::probes::gpu_d3d12::gpu_test_guard();
        let (vendor_id, device_id) = first_hardware_gpu();
        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_setter = Arc::clone(&cancel);

        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(80));
            cancel_setter.store(true, Ordering::Relaxed);
        });

        let start = Instant::now();
        let result = run(vendor_id, device_id, &GpuStressConfig::standard(), &cancel, |_| {});

        assert!(result.aborted);
        assert!(start.elapsed() < Duration::from_secs(5), "cancellation took too long");
    }
}
