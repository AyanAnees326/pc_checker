//! CPU stress worker: FMA saturation on one pinned logical processor, with a
//! self-check built directly into the workload rather than bolted on afterward.
//!
//! The stress workload *is* the self-check. Each block repeats the exact same
//! deterministic sequence of `v = v*a + b` FMA operations from the same fixed seed,
//! computed once during calibration and compared bit-for-bit on every subsequent
//! block. On correct, stable hardware every block must reproduce the calibration
//! result exactly — floating-point FMA is deterministic for fixed inputs and a fixed
//! instruction sequence. A mismatch means the silicon executing this block produced
//! a different answer to the same sum than it did a moment ago: an actual computation
//! error, not an inferred one from temperature or clock behaviour. This is the same
//! principle production stress tools (Prime95, y-cruncher) use — check the answer,
//! don't just generate heat.
//!
//! A register-resident FMA chain never touches memory, so on its own it leaves the
//! cache hierarchy and memory controller — a real fraction of package power and a real
//! source of instability on marginal RAM/IMC configurations — completely unloaded.
//! Each block therefore also streams through a per-thread buffer well larger than any
//! consumer CPU's per-core L2, forcing real L3/DRAM traffic every checkpoint. It is
//! folded into the same checksum comparison as the register chain, so a fault in
//! either component — ALU/FPU or cache/memory — fails the same single self-check.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

use windows::Win32::System::Threading::{
    GetCurrentThread, SetThreadAffinityMask, SetThreadPriority, THREAD_PRIORITY_ABOVE_NORMAL,
};

/// FMA operations per lane per checkpoint block. Small enough to give sub-100ms fault
/// detection latency at realistic clock speeds, large enough that the loop overhead
/// around each block is negligible next to the FMA work itself.
const CHECKPOINT_ITERS: u32 = 200_000;

/// Fixed multiplier/addend for the deterministic sequence. Chosen so repeated
/// application stays bounded (|a| < 1 pulls values toward 0, b keeps them off exactly
/// 0) rather than overflowing to infinity partway through a multi-minute run, which
/// would make every block "fail" for a reason that has nothing to do with the CPU.
const SEQ_A: f64 = 0.999_999_1;
const SEQ_B: f64 = 1.000_000_3;

/// Per-thread element count for the memory-bandwidth component. 1M `f64` = 8 MiB —
/// comfortably larger than any consumer CPU's per-core L2 (typically 256KB-2MB) and a
/// meaningful fraction of a shared L3, so a full traverse forces real L3/DRAM traffic
/// rather than just bouncing around in cache. With N worker threads each doing this,
/// the aggregate is a genuine, sustained memory-subsystem load.
const MEM_ELEMENTS: usize = 1 << 20;

/// Read-modify-write passes over the buffer per checkpoint. Chosen so the memory
/// component's cost sits well below the register FMA chain's (~2-5ms vs. tens of ms at
/// realistic bandwidths) — enough added traffic to matter, not so much that a single
/// checkpoint's latency threatens fault-detection or cancellation responsiveness.
const MEM_PASSES: u32 = 4;

/// Shared, lock-free counters the orchestrator's sampler reads from any thread.
#[derive(Default)]
pub struct WorkerStats {
    pub iterations: AtomicU64,
    /// Set (and left set) the first time a checkpoint mismatch is observed.
    pub fault: AtomicBool,
}

/// Run FMA saturation on the calling thread until `deadline` or `cancel` is set.
///
/// Pins the OS thread to `logical_processor` first — without this, the scheduler is
/// free to migrate the thread across cores mid-run, which both undermines per-core
/// telemetry attribution and lets a hot core "hide" behind a cooler one's average.
pub fn run_worker(
    logical_processor: u32,
    deadline: Instant,
    cancel: &AtomicBool,
    stats: &WorkerStats,
) {
    pin_to_core(logical_processor);
    // One step above normal, not THREAD_PRIORITY_TIME_CRITICAL: TIME_CRITICAL was
    // tried first and reliably starved the orchestrator's own sampler thread when it
    // shared a core with a worker (both pinned to core 0), since a TIME_CRITICAL
    // thread can dominate a core against anything running at normal priority —
    // caught by this codebase's own test suite before it ever shipped. ABOVE_NORMAL
    // is enough to keep this thread from being treated as idle/background work
    // without starving out the very thread that reports on its progress.
    unsafe {
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_ABOVE_NORMAL);
    }

    let seed = logical_processor as f64 + 1.0;
    let has_fma = is_x86_feature_detected!("fma") && is_x86_feature_detected!("avx2");
    // Allocated once, not per checkpoint: `memory_pass_checksum` resets its contents to
    // a deterministic pattern on every call, so reuse costs nothing in determinism and
    // saves a repeated 8 MiB allocation on every single checkpoint.
    let mut mem_buf = vec![0.0f64; MEM_ELEMENTS];
    let reference = checkpoint_checksum(seed, has_fma) ^ memory_pass_checksum(&mut mem_buf, seed);

    while Instant::now() < deadline && !cancel.load(Ordering::Relaxed) {
        let current = checkpoint_checksum(seed, has_fma) ^ memory_pass_checksum(&mut mem_buf, seed);
        stats.iterations.fetch_add(CHECKPOINT_ITERS as u64, Ordering::Relaxed);

        if current != reference {
            stats.fault.store(true, Ordering::Relaxed);
        }
    }
}

/// Deterministic memory-bandwidth pass: reset the buffer to a fixed pattern derived
/// from `seed`, then stream the same `v = v*a + b` recurrence used by the register
/// chain through it [`MEM_PASSES`] times, read-modify-write. Resetting on every call
/// (rather than letting state evolve across checkpoints) is what keeps this comparable
/// bit-for-bit against a single fixed reference the same way the register chain is —
/// each call is a fully independent, reproducible unit of work.
fn memory_pass_checksum(buf: &mut [f64], seed: f64) -> u64 {
    for (i, x) in buf.iter_mut().enumerate() {
        *x = seed + i as f64 * 1e-3;
    }
    for _ in 0..MEM_PASSES {
        for x in buf.iter_mut() {
            *x = x.mul_add(SEQ_A, SEQ_B);
        }
    }
    buf.iter().fold(0u64, |acc, x| acc ^ x.to_bits())
}

fn pin_to_core(logical_processor: u32) {
    // SetThreadAffinityMask takes a bitmask, which limits this to the first 64
    // logical processors (one processor group) — true of every laptop and the large
    // majority of desktops this tool targets.
    if logical_processor < 64 {
        let mask = 1usize << logical_processor;
        unsafe {
            let _ = SetThreadAffinityMask(GetCurrentThread(), mask);
        }
    }
}

fn checkpoint_checksum(seed: f64, has_fma: bool) -> u64 {
    if has_fma {
        unsafe { checkpoint_avx2_fma(seed) }
    } else {
        checkpoint_scalar(seed)
    }
}

/// Portable fallback for CPUs without AVX2/FMA (rare on anything this tool targets,
/// but must not crash on one). Same deterministic sequence, one lane.
fn checkpoint_scalar(seed: f64) -> u64 {
    let mut v = seed;
    for _ in 0..CHECKPOINT_ITERS {
        v = v.mul_add(SEQ_A, SEQ_B);
    }
    v.to_bits()
}

/// AVX2+FMA path: four independent `f64` lanes, each running the identical sequence
/// from a distinct seed so a fault in any one lane's execution unit is caught rather
/// than averaged away.
///
/// # Safety
/// Caller must have confirmed `avx2` and `fma` target features are present
/// (`run_worker` checks via `is_x86_feature_detected!` before calling this).
#[target_feature(enable = "avx2,fma")]
unsafe fn checkpoint_avx2_fma(seed: f64) -> u64 {
    use std::arch::x86_64::*;

    let mut v = _mm256_set_pd(seed, seed + 1.0, seed + 2.0, seed + 3.0);
    let a = _mm256_set1_pd(SEQ_A);
    let b = _mm256_set1_pd(SEQ_B);

    for _ in 0..CHECKPOINT_ITERS {
        v = _mm256_fmadd_pd(v, a, b);
    }

    let mut lanes = [0f64; 4];
    _mm256_storeu_pd(lanes.as_mut_ptr(), v);
    lanes.iter().fold(0u64, |acc, x| acc ^ x.to_bits())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn scalar_checkpoint_is_deterministic() {
        let a = checkpoint_scalar(1.0);
        let b = checkpoint_scalar(1.0);
        assert_eq!(a, b, "identical seed and sequence must reproduce bit-for-bit");
    }

    #[test]
    fn different_seeds_produce_different_checksums() {
        // Sanity: the check isn't vacuously true because every seed converges to the
        // same fixed point within the iteration budget.
        assert_ne!(checkpoint_scalar(1.0), checkpoint_scalar(2.0));
    }

    #[test]
    fn avx2_checkpoint_is_deterministic_when_available() {
        if !(is_x86_feature_detected!("avx2") && is_x86_feature_detected!("fma")) {
            return; // nothing to verify on hardware without these extensions
        }
        let a = unsafe { checkpoint_avx2_fma(1.0) };
        let b = unsafe { checkpoint_avx2_fma(1.0) };
        assert_eq!(a, b);
    }

    #[test]
    fn sequence_constants_stay_bounded_over_many_blocks() {
        // Run several thousand blocks' worth of the recurrence directly (bypassing
        // the fixed per-block restart) and confirm it never diverges to infinity —
        // if it did, every real run would "fault" on a bookkeeping bug, not a CPU one.
        let mut v = 3.0f64;
        for _ in 0..(CHECKPOINT_ITERS as u64 * 50) {
            v = v.mul_add(SEQ_A, SEQ_B);
            assert!(v.is_finite(), "sequence diverged: {v}");
        }
    }

    #[test]
    fn worker_reports_progress_and_stays_fault_free_on_this_machine() {
        let stats = WorkerStats::default();
        let cancel = AtomicBool::new(false);
        run_worker(0, Instant::now() + Duration::from_millis(200), &cancel, &stats);

        assert!(stats.iterations.load(Ordering::Relaxed) > 0, "no work was recorded");
        assert!(!stats.fault.load(Ordering::Relaxed), "self-check failed on presumed-healthy hardware");
    }

    #[test]
    fn worker_respects_cancellation_before_its_deadline() {
        let stats = WorkerStats::default();
        let cancel = AtomicBool::new(true); // already cancelled
        let start = Instant::now();
        run_worker(0, start + Duration::from_secs(30), &cancel, &stats);
        assert!(start.elapsed() < Duration::from_secs(2), "cancellation was not honoured promptly");
    }
}
