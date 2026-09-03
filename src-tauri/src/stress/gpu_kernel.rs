//! GPU stress worker: keeps the compute queue from `probes::gpu_d3d12` continuously
//! full, self-checking the result on a sampled cadence the same way `cpu_kernel` does —
//! a fixed deterministic input must always produce the same output on correct
//! hardware, so a mismatch is a real fault, not an inferred one.
//!
//! **Sustained load is the whole design goal.** A GPU drops to idle clocks and idle
//! power within milliseconds of its queue running dry, so anything that makes the CPU
//! and GPU take turns produces a sawtooth instead of a stress test. This worker
//! therefore never waits on the GPU in its steady state: it submits batches ahead
//! (`GpuComputeContext` keeps three in flight), and *polls* for the self-check result
//! rather than blocking for it, so verification costs no load. The one blocking wait
//! that remains is the backpressure inside `submit_batch` when the ring is full —
//! which is the correct place to wait, because at that moment the GPU has a full queue.
//!
//! Batch size is calibrated against the hardware at startup rather than hardcoded: one
//! dispatch is timed, and batches are sized to roughly [`TARGET_BATCH_MICROS`] of GPU
//! work. A hardcoded batch count that keeps an RX 6700 XT saturated would either
//! trip the TDR watchdog on a weak integrated GPU or leave a 5090 half idle.
//!
//! As with the compute self-check, this same mechanism stands in for a VRAM integrity
//! pass: both the source and destination buffers are 64 MiB and VRAM-resident, and
//! every dispatch reads one in full and rewrites the other in full. A corrupted VRAM
//! cell — the classic failure mode of an ex-mining GPU — shows up as a checksum
//! mismatch exactly the same way a compute-unit fault would. A dedicated full-VRAM
//! sweep independent of compute load remains a reasonable future addition.
//!
//! Compute-only load leaves real chip area idle: the rasterizer, the render-target
//! output/blend hardware (ROP), and pixel-shader ALUs are all different hardware from
//! the compute units above and a pure `Dispatch` loop never touches them, even though
//! this is exactly the mix a game workload spends much of its time on. Every batch
//! `submit_batch` records now also draws a fullscreen triangle into an off-screen
//! render target (see `probes::gpu_d3d12`'s "Graphics (raster) pass" comment), so this
//! worker keeps both the compute and the raster/ROP side of the chip loaded, and both
//! are self-checked the same way — a fault in either one fails the run.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use windows::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_ABOVE_NORMAL};

use crate::probes::gpu_d3d12::{select_adapter, GpuComputeContext};
use crate::win::WinResult;

/// Target GPU work per submitted batch. Long enough that per-batch CPU bookkeeping is
/// negligible against it, short enough that cancellation still feels immediate (the
/// worst case for noticing a cancel is roughly one batch).
const TARGET_BATCH_MICROS: u128 = 250_000;

/// Ceiling on batch size, so a very fast GPU cannot end up with a command list so long
/// that cancellation and the phase deadline both overshoot badly.
const MAX_DISPATCHES_PER_BATCH: u32 = 64;

/// How often to verify the result. The check is a 64 MiB fold on the CPU; at this
/// cadence it is free relative to the GPU work happening concurrently, and it still
/// samples the run hundreds of times over a 10-minute phase.
const SELF_CHECK_INTERVAL: Duration = Duration::from_secs(2);

#[derive(Default)]
pub struct GpuWorkerStats {
    /// Dispatches submitted. Each one reads the full 64 MiB source buffer and rewrites
    /// the full 64 MiB destination buffer, so this is a real throughput figure. Up to
    /// three batches may still be in flight, so it leads actual completion slightly.
    /// Correctness is reported separately via `fault` — verification is sampled at
    /// [`SELF_CHECK_INTERVAL`] rather than performed on every dispatch, because
    /// verifying every dispatch is exactly what starved the GPU before.
    pub dispatches: AtomicU64,
    pub fault: AtomicBool,
}

/// Deterministic seed pattern — a small repeating cycle rather than all-identical
/// values, so a shader bug that only manifests on certain inputs is not masked by
/// every lane happening to compute the same thing.
fn deterministic_pattern(n: u32) -> Vec<f32> {
    (0..n).map(|i| 1.0 + (i % 97) as f32 * 0.01).collect()
}

/// Dispatches per batch such that a batch is about [`TARGET_BATCH_MICROS`] of work on
/// hardware where one dispatch takes `per_dispatch`.
fn batch_size_for(per_dispatch: Duration) -> u32 {
    let micros = per_dispatch.as_micros().max(1);
    let n = (TARGET_BATCH_MICROS / micros) as u32;
    n.clamp(1, MAX_DISPATCHES_PER_BATCH)
}

/// Drive the GPU matching `vendor_id`/`device_id` at full load until `deadline` or
/// `cancel`. Returns an error only for setup failures (no such adapter, D3D12 device
/// creation failed) — a mid-run failure is recorded as a fault rather than propagated,
/// since surfacing an unstable GPU is the point of a stress test.
pub fn run_worker(
    vendor_id: u32,
    device_id: u32,
    deadline: Instant,
    cancel: &AtomicBool,
    stats: &GpuWorkerStats,
) -> WinResult<()> {
    // ABOVE_NORMAL, not TIME_CRITICAL — see `cpu_kernel::run_worker`'s comment for
    // why: TIME_CRITICAL reliably starved the orchestrator's sampler thread in this
    // codebase's own tests.
    unsafe {
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_ABOVE_NORMAL);
    }

    // Setup is not instant — device creation, shader compilation, a 64 MiB seed upload
    // and two calibration dispatches add up to roughly a second. Checking `cancel`
    // between each step matters: without it, Stop pressed during that window is simply
    // ignored, and the run keeps going until setup finishes. Bailing out here is a
    // clean no-op — the orchestrator reports the phase as cancelled either way.
    let adapter = select_adapter(vendor_id, device_id)?;
    if cancel.load(Ordering::Relaxed) {
        return Ok(());
    }

    let mut ctx = GpuComputeContext::create(&adapter)?;
    if cancel.load(Ordering::Relaxed) {
        return Ok(());
    }

    ctx.upload_seed(&deterministic_pattern(ctx.element_count))?;
    if cancel.load(Ordering::Relaxed) {
        return Ok(());
    }

    // First batch establishes the reference results — compute *and* graphics, since
    // `submit_batch` now records one fullscreen draw alongside the dispatches every
    // time (see `probes::gpu_d3d12`'s "Graphics (raster) pass" comment) — and warms
    // the pipeline.
    ctx.submit_batch(1, true)?;
    ctx.wait_for_submitted()?;
    let reference = ctx.read_checksum()?;
    let graphics_reference = ctx.read_graphics_checksum()?;
    if cancel.load(Ordering::Relaxed) {
        return Ok(());
    }

    // Second, now-warm dispatch is the calibration sample.
    let timed_at = Instant::now();
    ctx.submit_batch(1, false)?;
    ctx.wait_for_submitted()?;
    let batch = batch_size_for(timed_at.elapsed());

    let mut pending_check: Option<u64> = None;
    let mut last_check = Instant::now();

    while Instant::now() < deadline && !cancel.load(Ordering::Relaxed) {
        let capture = pending_check.is_none() && last_check.elapsed() >= SELF_CHECK_INTERVAL;

        match ctx.submit_batch(batch, capture) {
            Ok(fence) => {
                stats.dispatches.fetch_add(batch as u64, Ordering::Relaxed);
                if capture {
                    pending_check = Some(fence);
                }
            }
            Err(_) => {
                // Submission only fails when the device is gone or reset (TDR). More
                // submissions cannot succeed, and spinning on them would report a
                // full-length run that did no work — stop and let the fault stand.
                stats.fault.store(true, Ordering::Relaxed);
                break;
            }
        }

        // Poll, never block: the readback landed at the end of a batch the GPU was
        // already going to run, so by the time its fence passes the result is sitting
        // there for free while later batches keep the GPU busy.
        if let Some(fence) = pending_check {
            if ctx.is_complete(fence) {
                match ctx.read_checksum() {
                    Ok(sum) if sum == reference => {}
                    _ => stats.fault.store(true, Ordering::Relaxed),
                }
                // Graphics is checked the same way as compute — a fault in either the
                // raster/pixel-shader/ROP path or the compute path fails the same
                // single self-check, since both are real hardware this stress test is
                // meant to exercise.
                match ctx.read_graphics_checksum() {
                    Ok(sum) if sum == graphics_reference => {}
                    _ => stats.fault.store(true, Ordering::Relaxed),
                }
                pending_check = None;
                last_check = Instant::now();
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::probes::gpu;

    fn first_hardware_gpu() -> (u32, u32) {
        let gpus = gpu::probe().expect("DXGI enumeration failed");
        let g = gpus.iter().find(|g| !g.is_software).expect("no hardware GPU on this machine");
        (g.vendor_id, g.device_id)
    }

    #[test]
    fn batch_size_targets_a_quarter_second_of_gpu_work() {
        // 25ms dispatch -> ~10 per batch.
        assert_eq!(batch_size_for(Duration::from_millis(25)), 10);
        // 250ms dispatch -> exactly one.
        assert_eq!(batch_size_for(Duration::from_millis(250)), 1);
    }

    #[test]
    fn batch_size_never_collapses_to_zero_or_runs_away() {
        // A dispatch slower than the whole batch target must still submit one, not zero.
        assert_eq!(batch_size_for(Duration::from_secs(1)), 1);
        assert_eq!(batch_size_for(Duration::from_nanos(1)), MAX_DISPATCHES_PER_BATCH);
    }

    /// Real hardware, real deadline: proves the worker completes dispatches and stays
    /// fault-free on this machine's actual GPU within a short window.
    #[test]
    fn worker_completes_dispatches_without_faulting_on_this_machines_gpu() {
        let _gpu = crate::probes::gpu_d3d12::gpu_test_guard();
        let (vendor_id, device_id) = first_hardware_gpu();
        let stats = GpuWorkerStats::default();
        let cancel = AtomicBool::new(false);

        run_worker(vendor_id, device_id, Instant::now() + Duration::from_secs(4), &cancel, &stats)
            .expect("GPU worker setup failed");

        assert!(stats.dispatches.load(Ordering::Relaxed) > 0, "no dispatches completed");
        assert!(!stats.fault.load(Ordering::Relaxed), "self-check failed on presumed-healthy hardware");
    }

    /// The property this whole module exists for, measured rather than assumed: while
    /// the worker runs, the GPU clock must not sawtooth. This is the regression test
    /// for the original bug — the previous design round-tripped 64 MiB through the CPU
    /// per dispatch and left this machine's RX 6700 XT at ~86 W against a ~230 W board
    /// rating, with the reported clock dipping to 0 MHz between batches.
    ///
    /// Deliberately does *not* compare against a separately-measured "idle" baseline.
    /// An earlier version of this test did, and was flaky for a real reason: this is a
    /// desktop machine with other software that can be using the GPU (the browser,
    /// the Adrenalin overlay, anything with hardware acceleration), so "idle" power
    /// measured a few seconds before the load starts is not under this test's control
    /// and drifted between runs — 7 W once, 90 W another, 119 W a third, none of them
    /// wrong, just whatever else was happening on the machine at the time.
    ///
    /// Sampling *during* the load window sidesteps that: whatever the ambient floor is,
    /// the defect this guards against is dips *within* a run that should be flat, and
    /// that signal doesn't depend on knowing the floor at all. Skips (rather than
    /// fails) where ADL telemetry is unavailable.
    #[test]
    fn worker_actually_loads_the_gpu_rather_than_idling_between_batches() {
        use crate::probes::adl::AdlSession;

        let _gpu = crate::probes::gpu_d3d12::gpu_test_guard();
        let Ok(session) = AdlSession::open() else {
            eprintln!("skipping: ADL telemetry unavailable (non-AMD GPU or driver absent)");
            return;
        };
        if session.power_watts().is_none() || session.graphics_clock_mhz().is_none() {
            eprintln!("skipping: this adapter does not report power/clock through ADL");
            return;
        }

        let (vendor_id, device_id) = first_hardware_gpu();
        let stats = std::sync::Arc::new(GpuWorkerStats::default());
        let cancel = std::sync::Arc::new(AtomicBool::new(false));

        let (s, c) = (std::sync::Arc::clone(&stats), std::sync::Arc::clone(&cancel));
        let worker = std::thread::spawn(move || {
            let _ = run_worker(vendor_id, device_id, Instant::now() + Duration::from_secs(12), &c, &s);
        });

        // Skip the first second so setup (device creation, seed upload, calibration
        // dispatches) doesn't register as a dip in what should be the sustained window.
        std::thread::sleep(Duration::from_secs(1));

        let mut clocks = Vec::new();
        let mut powers = Vec::new();
        for _ in 0..24 {
            if let Some(c) = session.graphics_clock_mhz() {
                clocks.push(c);
            }
            if let Some(w) = session.power_watts() {
                powers.push(w);
            }
            std::thread::sleep(Duration::from_millis(250));
        }

        cancel.store(true, Ordering::Relaxed);
        worker.join().expect("worker thread panicked");

        assert!(clocks.len() >= 12, "too few clock samples captured to judge stability");
        let max_clock = *clocks.iter().max().expect("checked non-empty above");
        let min_clock = *clocks.iter().min().expect("checked non-empty above");
        let max_power = powers.iter().cloned().fold(0.0f64, f64::max);
        let min_power = powers.iter().cloned().fold(f64::MAX, f64::min);

        println!("during load: clock {min_clock}-{max_clock} MHz, power {min_power:.0}-{max_power:.0} W");

        // 60%, not "never drops": real workloads legitimately vary clock somewhat, and
        // this only needs to catch the sawtooth-to-near-zero pattern the queue-draining
        // bug produced, not police normal boost-clock jitter.
        assert!(
            min_clock as f64 > max_clock as f64 * 0.6,
            "clock dipped to {min_clock} MHz against a {max_clock} MHz peak during sustained load — \
             the queue is draining between batches, not staying full"
        );
        assert!(
            min_power > max_power * 0.5,
            "power dipped to {min_power:.0} W against a {max_power:.0} W peak during sustained load"
        );
    }
}
