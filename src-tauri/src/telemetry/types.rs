//! Shared telemetry types for a live stress session.
//!
//! Mirrors the `Reading<T>` convention from `model.rs`: a stress sample taken without
//! PawnIO installed still has real values (elapsed time, iteration count, checksum
//! status) and legitimately missing ones (clock, power, temperature) — the frontend
//! must be able to tell those apart mid-run, not just at the end.

use serde::{Deserialize, Serialize};

use crate::model::Reading;

/// No separate "ramp" phase: it added a delay before real load with marginal
/// diagnostic value, and a stress test whose Start button doesn't start stressing
/// anything for 90+ seconds reads as broken, not as thorough. `AllCoreSustained`
/// begins immediately after the (now brief) single-thread check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    IdleBaseline,
    SingleThreadBoost,
    AllCoreSustained,
    Cooldown,
}

impl Phase {
    pub fn label(self) -> &'static str {
        match self {
            Phase::IdleBaseline => "Idle baseline",
            Phase::SingleThreadBoost => "Single-thread boost",
            Phase::AllCoreSustained => "All-core sustained",
            Phase::Cooldown => "Cooldown",
        }
    }
}

/// One telemetry sample, taken at the sampler's tick rate (~4 Hz).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CpuSample {
    pub elapsed_ms: u64,
    pub phase: Phase,

    /// Effective clock per logical processor actively loaded this phase, from
    /// APERF/MPERF deltas — true time-at-frequency, not the OS's reported clock.
    pub effective_clock_mhz: Reading<f64>,
    /// Measured package power from the RAPL/PWR_UNIT energy-counter delta.
    pub package_power_watts: Reading<f64>,
    /// The firmware's own configured sustained-power cap (Intel only — see
    /// `pawnio::intel_msr`'s doc comment on why this exists but is not paired with
    /// per-reason throttle attribution).
    pub configured_pl1_watts: Reading<f64>,
    pub configured_pl2_watts: Reading<f64>,
    pub package_temperature_c: Reading<i16>,
    /// Intel only: whether the package thermal-throttle bit is set right now.
    pub thermal_throttling: Reading<bool>,

    /// Whether the self-check (deterministic checksum replay) matched this tick.
    /// `None` before the first checkpoint of a run.
    pub self_check_ok: Option<bool>,
    pub total_iterations: u64,
}

/// One sample from the standalone CPU metrics monitor (`telemetry::live_monitor`) —
/// the same clock/power/temperature telemetry a stress run produces, but reviewable
/// on its own at a lighter ~1 Hz rate without running the full stress schedule.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CpuLiveSample {
    pub elapsed_ms: u64,
    pub effective_clock_mhz: Reading<f64>,
    pub package_power_watts: Reading<f64>,
    pub package_temperature_c: Reading<i16>,
    pub thermal_throttling: Reading<bool>,
}

/// The final result of a completed (or aborted) CPU stress run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CpuStressResult {
    pub samples: Vec<CpuSample>,
    pub aborted: bool,
    pub abort_reason: Option<String>,
    /// True if any worker thread's self-check ever failed — a real hardware fault,
    /// not just an out-of-spec clock/temperature.
    pub self_check_failed: bool,
}

/// GPU stress has no per-core concept — one compute queue drives its own internal
/// parallelism across thousands of shader threads, so there is no "single-thread vs
/// all-core" distinction the way there is on a CPU. No separate ramp phase either,
/// for the same reason `Phase` above dropped one — full compute load starts right
/// after the brief idle baseline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GpuPhase {
    IdleBaseline,
    ComputeSustained,
    Cooldown,
}

impl GpuPhase {
    pub fn label(self) -> &'static str {
        match self {
            GpuPhase::IdleBaseline => "Idle baseline",
            GpuPhase::ComputeSustained => "Compute sustained",
            GpuPhase::Cooldown => "Cooldown",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GpuSample {
    pub elapsed_ms: u64,
    pub phase: GpuPhase,

    pub graphics_clock_mhz: Reading<u32>,
    pub power_watts: Reading<f64>,
    /// AMD calls this "GPU Temperature"; NVML's single `NVML_TEMPERATURE_GPU` sensor
    /// maps here for NVIDIA too, so this field means "the primary/board temperature"
    /// on either vendor.
    pub edge_temperature_c: Reading<u32>,
    /// Junction/die temperature. AMD-only — NVML does not expose an equivalent on
    /// consumer cards, so this reads `Unavailable` rather than falling back to the
    /// edge reading, which would silently misrepresent it as the hotter of the two.
    pub hotspot_temperature_c: Reading<u32>,
    /// AMD-only — no RPM equivalent exists in NVIDIA's public NVML API.
    pub fan_rpm: Reading<u32>,
    pub fan_percent: Reading<u32>,
    pub sw_thermal_slowdown: Reading<bool>,
    pub hw_thermal_slowdown: Reading<bool>,
    pub sw_power_cap: Reading<bool>,
    pub hw_power_brake: Reading<bool>,

    /// Whether the checkpoint buffer read back matched its reference — the same
    /// principle as the CPU kernel, and here it verifies VRAM integrity as well as
    /// compute correctness, since the checked buffer round-trips through VRAM every
    /// checkpoint. See `stress::gpu_kernel`'s module doc comment.
    pub self_check_ok: Option<bool>,
    pub dispatches_completed: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GpuStressResult {
    pub samples: Vec<GpuSample>,
    pub aborted: bool,
    pub abort_reason: Option<String>,
    pub self_check_failed: bool,
}
