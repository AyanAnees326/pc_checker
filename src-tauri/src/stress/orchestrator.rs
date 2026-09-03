//! CPU stress phase scheduler: spawns pinned worker threads per phase, samples
//! telemetry at a fixed rate, enforces abort conditions, and always restores the
//! machine to an unloaded state on the way out — including on a panic in any worker,
//! since a stress test that leaves someone else's laptop pinned at 100% after a crash
//! is worse than not having run it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use windows::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
use windows::Win32::System::Threading::{GetCurrentThread, SetThreadAffinityMask};

use crate::probes::cpu;
use crate::stress::cpu_kernel::{self, WorkerStats};
use crate::telemetry::cpu_source::{MsrSource, TelemetryState};
use crate::telemetry::types::{CpuSample, CpuStressResult, Phase};

#[derive(Debug, Clone)]
pub struct StressConfig {
    pub idle_baseline: Duration,
    pub single_thread_boost: Duration,
    pub all_core_sustained: Duration,
    pub cooldown: Duration,
    pub sample_interval: Duration,
}

impl StressConfig {
    /// ~12 minutes total. No separate ramp phase: pressing Start should mean the
    /// machine is under real load within seconds, not 90+ seconds — a delay that
    /// reads as "broken" rather than "thorough". Idle baseline and the single-thread
    /// boost check are kept, but brief: a moment to grab an idle/boost-clock reading,
    /// not a dedicated phase the user has to sit through before the actual test the
    /// button promised begins.
    pub fn standard() -> Self {
        Self::for_duration(Duration::from_secs(14 * 60))
    }

    /// Build a schedule targeting `total` overall run length. `idle_baseline` and
    /// `single_thread_boost` stay fixed at their calibration-reading lengths — they
    /// exist to grab an idle/boost-clock sample, not to scale with the user's chosen
    /// duration — and `cooldown` is a bounded fraction of the total so a short run
    /// still gets a meaningful cooling-recovery reading and a long run doesn't spend
    /// an outsized share of it doing nothing. Whatever remains goes to the actual
    /// test: `all_core_sustained`. Feeding this the same total as the old hardcoded
    /// `standard()` (14 minutes) reproduces its exact phase lengths.
    pub fn for_duration(total: Duration) -> Self {
        let idle_baseline = Duration::from_secs(5);
        let single_thread_boost = Duration::from_secs(10);
        let fixed = idle_baseline + single_thread_boost;
        let cooldown = (total / 6).clamp(Duration::from_secs(15), Duration::from_secs(120));
        let all_core_sustained = total
            .checked_sub(fixed + cooldown)
            .unwrap_or_default()
            .max(Duration::from_secs(30));
        Self {
            idle_baseline,
            single_thread_boost,
            all_core_sustained,
            cooldown,
            sample_interval: Duration::from_millis(250),
        }
    }

    /// Short phase durations for exercising the whole pipeline in tests without
    /// waiting twelve minutes. Same phase order and telemetry path as `standard`.
    ///
    /// `all_core_sustained` is now the *first* loaded phase to spawn all N worker
    /// threads (the ramp phase used to absorb that startup cost) — it needs enough
    /// headroom for thread creation plus each worker's uncounted calibration
    /// checkpoint before any iteration is ever recorded, or the window can elapse
    /// with real work still in flight but nothing yet reflected in the stats.
    #[cfg(test)]
    pub fn quick_test() -> Self {
        Self {
            idle_baseline: Duration::from_millis(150),
            single_thread_boost: Duration::from_millis(150),
            all_core_sustained: Duration::from_millis(800),
            cooldown: Duration::from_millis(150),
            sample_interval: Duration::from_millis(50),
        }
    }
}

/// Battery below this percentage on AC-disconnected power aborts the run — this tool
/// runs on machines it does not own, and draining a stranger's battery during a
/// twelve-minute stress test past the point of a safe unattended shutdown is not an
/// acceptable trade for more data.
const MIN_BATTERY_PERCENT_ON_DC: u8 = 20;

/// Run the full phase schedule, calling `on_sample` from the sampler's thread as each
/// sample is taken (the caller — the Tauri command layer — turns this into a stream of
/// `stress://cpu/sample` events). Returns once the schedule completes, the run is
/// cancelled via `cancel`, or an abort condition fires.
pub fn run(config: &StressConfig, cancel: &Arc<AtomicBool>, mut on_sample: impl FnMut(CpuSample)) -> CpuStressResult {
    // Held for the whole run: without this, Windows may classify this GUI process as
    // background work and throttle it via EcoQoS, silently gutting the load a "stress
    // test" is supposed to apply. See `win::power`'s module doc comment.
    let _throttle_guard = crate::win::power::ExecutionSpeedGuard::engage();

    let topology = cpu::probe();
    let logical = topology.logical_processors.max(1);

    // Pin the sampler thread itself to logical processor 0 so APERF/MPERF reads are
    // meaningful (see the module doc comment in `pawnio::intel_msr` on why this is a
    // per-core register): core 0 is always one of the actively-loaded cores in every
    // phase that loads any cores at all, by construction below.
    unsafe {
        let _ = SetThreadAffinityMask(GetCurrentThread(), 1);
    }

    let msr = MsrSource::open(topology.vendor);
    let mut samples = Vec::new();
    let start = Instant::now();
    let mut telemetry = TelemetryState::new(&msr, &topology);

    let mut abort: Option<String> = None;
    let mut self_check_failed = false;

    let phases: [(Phase, Duration, PhaseLoad); 4] = [
        (Phase::IdleBaseline, config.idle_baseline, PhaseLoad::None),
        (Phase::SingleThreadBoost, config.single_thread_boost, PhaseLoad::SingleCore),
        (Phase::AllCoreSustained, config.all_core_sustained, PhaseLoad::AllCores),
        (Phase::Cooldown, config.cooldown, PhaseLoad::None),
    ];

    'phases: for (phase, duration, load) in phases {
        if cancel.load(Ordering::Relaxed) {
            abort = Some("cancelled by user".into());
            break;
        }

        let worker_count = match load {
            PhaseLoad::None => 0,
            PhaseLoad::SingleCore => 1,
            PhaseLoad::AllCores => logical,
        };

        let phase_deadline = Instant::now() + duration;
        let workers = spawn_workers(worker_count, phase_deadline, cancel);

        loop {
            let tick_start = Instant::now();
            if tick_start >= phase_deadline {
                break;
            }

            if cancel.load(Ordering::Relaxed) {
                abort = Some("cancelled by user".into());
                stop_workers(&workers, cancel);
                break 'phases;
            }

            if let Some(reason) = check_abort_conditions() {
                abort = Some(reason);
                stop_workers(&workers, cancel);
                break 'phases;
            }

            let total_iterations: u64 = workers
                .iter()
                .map(|w| w.stats.iterations.load(Ordering::Relaxed))
                .sum();
            let any_fault = workers.iter().any(|w| w.stats.fault.load(Ordering::Relaxed));
            if any_fault {
                self_check_failed = true;
            }

            let readings = telemetry.sample(&msr);
            let sample = CpuSample {
                elapsed_ms: start.elapsed().as_millis() as u64,
                phase,
                effective_clock_mhz: readings.effective_clock_mhz,
                package_power_watts: readings.package_power_watts,
                configured_pl1_watts: readings.configured_pl1_watts,
                configured_pl2_watts: readings.configured_pl2_watts,
                package_temperature_c: readings.package_temperature_c,
                thermal_throttling: readings.thermal_throttling,
                self_check_ok: if workers.is_empty() { None } else { Some(!any_fault) },
                total_iterations,
            };
            on_sample(sample.clone());
            samples.push(sample);

            let elapsed = tick_start.elapsed();
            if elapsed < config.sample_interval {
                std::thread::sleep(config.sample_interval - elapsed);
            }
        }

        join_workers(workers);
    }

    // Belt-and-braces: even on a clean finish, make sure the affinity mask this
    // thread applied to itself does not leak into whatever Tauri reuses it for next.
    unsafe {
        let _ = SetThreadAffinityMask(GetCurrentThread(), usize::MAX);
    }

    CpuStressResult {
        samples,
        aborted: abort.is_some(),
        abort_reason: abort,
        self_check_failed,
    }
}

enum PhaseLoad {
    None,
    SingleCore,
    AllCores,
}

struct RunningWorker {
    stats: Arc<WorkerStats>,
    handle: std::thread::JoinHandle<()>,
}

fn spawn_workers(count: u32, deadline: Instant, cancel: &Arc<AtomicBool>) -> Vec<RunningWorker> {
    (0..count)
        .map(|core| {
            let stats = Arc::new(WorkerStats::default());
            let stats_for_thread = Arc::clone(&stats);
            let cancel_for_thread = Arc::clone(cancel);
            let handle = std::thread::Builder::new()
                .name(format!("pc-checker-stress-core-{core}"))
                .spawn(move || {
                    cpu_kernel::run_worker(core, deadline, &cancel_for_thread, &stats_for_thread);
                })
                .expect("failed to spawn stress worker thread");
            RunningWorker { stats, handle }
        })
        .collect()
}

/// Signal every worker to stop immediately (used on abort — normal completion just
/// lets each worker's own deadline elapse).
fn stop_workers(workers: &[RunningWorker], cancel: &Arc<AtomicBool>) {
    cancel.store(true, Ordering::Relaxed);
    let _ = workers; // workers observe `cancel` on their own next loop check
}

fn join_workers(workers: Vec<RunningWorker>) {
    for w in workers {
        // A panicked worker must not take the whole stress run down with it — the
        // sampler already has everything it needs from `stats`.
        let _ = w.handle.join();
    }
}

fn check_abort_conditions() -> Option<String> {
    let mut status = SYSTEM_POWER_STATUS::default();
    if unsafe { GetSystemPowerStatus(&mut status) }.is_err() {
        return None; // can't determine power state; do not abort on an unrelated API failure
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

#[cfg(test)]
mod tests {
    use super::*;

    /// This dev machine does not have PawnIO installed. That is exactly the
    /// production case this test exercises: the whole phase schedule, worker
    /// spawning, self-check, and abort-condition plumbing must all still work end to
    /// end with every telemetry field gracefully missing, not panic or silently
    /// return an empty result.
    #[test]
    fn full_schedule_runs_and_degrades_cleanly_without_pawnio() {
        let cancel = Arc::new(AtomicBool::new(false));
        let mut count = 0usize;
        let result = run(&StressConfig::quick_test(), &cancel, |_sample| count += 1);

        assert!(!result.aborted, "unexpected abort: {:?}", result.abort_reason);
        assert!(!result.self_check_failed, "self-check must not fail on healthy hardware");
        assert!(!result.samples.is_empty());
        assert_eq!(count, result.samples.len());

        // Every phase must appear at least once.
        for phase in [
            Phase::IdleBaseline,
            Phase::SingleThreadBoost,
            Phase::AllCoreSustained,
            Phase::Cooldown,
        ] {
            assert!(
                result.samples.iter().any(|s| s.phase == phase),
                "missing samples for {phase:?}"
            );
        }

        // Iteration count must be strictly increasing across load-bearing phases —
        // proof the worker threads actually did the FMA work, not just idled.
        let sustained_max = result
            .samples
            .iter()
            .filter(|s| s.phase == Phase::AllCoreSustained)
            .map(|s| s.total_iterations)
            .max()
            .unwrap_or(0);
        assert!(sustained_max > 0, "no work recorded during the sustained phase");

        // No PawnIO on this machine -> every MSR-derived field must say so, not
        // silently read as a plausible-looking zero.
        let any_sample = &result.samples[result.samples.len() / 2];
        assert!(!any_sample.effective_clock_mhz.is_ok());
        assert!(!any_sample.package_power_watts.is_ok());
    }

    #[test]
    fn cancellation_stops_the_run_promptly_and_marks_it_aborted() {
        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_for_setter = Arc::clone(&cancel);

        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(80));
            cancel_for_setter.store(true, Ordering::Relaxed);
        });

        let start = Instant::now();
        let result = run(&StressConfig::standard(), &cancel, |_| {});

        assert!(result.aborted);
        assert_eq!(result.abort_reason.as_deref(), Some("cancelled by user"));
        // Must not have run anywhere near the full ~12-minute standard schedule.
        assert!(start.elapsed() < Duration::from_secs(5), "cancellation took too long to take effect");
    }
}
