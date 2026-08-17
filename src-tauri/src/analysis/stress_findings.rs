//! Turning a completed CPU stress run into findings.
//!
//! Three rules, each grounded in what was actually measured rather than inferred:
//!
//! 1. **Self-check failure** — the stress kernel's checksum replay (see
//!    `stress::cpu_kernel`) mismatched at least once. This is a hardware fault caught
//!    directly, not a threshold judgement call.
//! 2. **Sustained clock below rated base clock** — the manufacturer's own guarantee
//!    at TDP. Needs no cohort data; either the chip held its rated floor or it did
//!    not. Falls back to a lower-confidence heuristic when the part is not in the
//!    local `cpu_specs.json` seed dataset.
//! 3. **Thermal throttling observed** (Intel only, when PawnIO is installed) — the
//!    package thermal-status bit is silicon-reported ground truth, not an inference
//!    from a temperature threshold we picked.
//!
//! What this deliberately does *not* do: treat "PawnIO not installed" as a defect
//! finding. A buyer negotiates over what is wrong with the machine, and a scan
//! limitation is not that — the UI surfaces that gap directly from the sample data's
//! `Reading` states instead of it being smuggled in here as a scored finding.

use serde::Deserialize;

use crate::model::{Confidence, Finding, Severity};
use crate::probes::cpu::CpuTopology;
use crate::telemetry::types::{CpuSample, CpuStressResult, GpuPhase, GpuStressResult, Phase};

const SPEC_DB_JSON: &str = include_str!("../../data/cpu_specs.json");

/// Sustained clock below base by more than this fraction counts as out-of-spec —
/// a small buffer above 0% absorbs normal measurement noise in the APERF/MPERF
/// sampling without hiding a real cooling problem.
const CLOCK_FLOOR_TOLERANCE: f64 = 0.05;

/// Package thermal-throttle bit set for at least this fraction of sustained-phase
/// samples counts as "observed", rather than one noisy tick.
const THROTTLE_OBSERVED_FRACTION: f64 = 0.10;

#[derive(Debug, Clone, Deserialize)]
struct CpuSpec {
    match_key: String,
    display_name: String,
    base_clock_mhz: u32,
    #[allow(dead_code)] // not yet used by a rule; kept for the boost-clock rule this dataset anticipates
    boost_clock_mhz: u32,
    #[allow(dead_code)]
    tdp_watts: u32,
}

#[derive(Debug, Deserialize)]
struct CpuSpecDb {
    cpus: Vec<CpuSpec>,
}

fn spec_db() -> CpuSpecDb {
    serde_json::from_str(SPEC_DB_JSON).expect("bundled cpu_specs.json must parse")
}

fn find_spec(brand_string: &str) -> Option<CpuSpec> {
    let normalized = brand_string.to_lowercase();
    spec_db()
        .cpus
        .into_iter()
        .find(|s| normalized.contains(&s.match_key))
}

/// The rated base clock for a CPU brand string, from the bundled spec dataset.
///
/// Exposed for the telemetry layer, which needs it for a different reason than the
/// findings rules below do: APERF/MPERF yields a *ratio*, and converting that ratio
/// into MHz requires the nominal frequency. AMD implements no equivalent of Intel's
/// CPUID leaf 0x16, so on those parts this dataset is the only source — and without
/// it the "effective clock" figure is a bare ratio near 1.0 that must never be
/// rendered as if it were megahertz.
pub fn base_clock_mhz_for(brand_string: &str) -> Option<u32> {
    find_spec(brand_string).map(|s| s.base_clock_mhz)
}

pub fn evaluate_cpu(result: &CpuStressResult, topology: &CpuTopology) -> Vec<Finding> {
    let mut out = Vec::new();

    if result.self_check_failed {
        out.push(
            Finding::new(
                "cpu.self_check_failed",
                "CPU produced an incorrect result under sustained load",
                Severity::Critical,
                Confidence::SpecGrounded,
            )
            .observed("the stress kernel's deterministic checkpoint replay did not match its own reference result")
            .expected("bit-identical results every time, since the computation is deterministic")
            .basis("a fixed FMA sequence run from the same seed must always produce the same answer on correct hardware — a mismatch is a computation error, not a threshold judgement")
            .recommend("Treat this as a hardware fault, not a cooling issue. Do not buy this machine on the assumption it is stable, even if temperatures looked fine."),
        );
    }

    if let Some(f) = clock_floor_finding(result, topology) {
        out.push(f);
    }

    if let Some(f) = thermal_throttle_finding(result) {
        out.push(f);
    }

    sorted(out)
}

fn sustained_samples(result: &CpuStressResult) -> impl Iterator<Item = &CpuSample> {
    result.samples.iter().filter(|s| s.phase == Phase::AllCoreSustained)
}

fn clock_floor_finding(result: &CpuStressResult, topology: &CpuTopology) -> Option<Finding> {
    let clocks: Vec<f64> = sustained_samples(result)
        .filter_map(|s| s.effective_clock_mhz.get().copied())
        .collect();

    // Nothing to judge without a telemetry source (PawnIO missing) or a base clock
    // to compare against (neither the spec dataset nor CPUID leaf 0x16 answered) —
    // this is a scan-coverage gap, not a finding, per the module doc comment.
    if clocks.is_empty() {
        return None;
    }

    let brand = topology.brand_string.get()?;
    let avg_clock = clocks.iter().sum::<f64>() / clocks.len() as f64;

    let (base_clock_mhz, confidence, basis) = match find_spec(brand) {
        Some(spec) => (
            spec.base_clock_mhz as f64,
            Confidence::SpecGrounded,
            format!("{}'s manufacturer-rated base clock ({} MHz)", spec.display_name, spec.base_clock_mhz),
        ),
        None => {
            let heuristic_base = topology.base_clock_mhz.get().copied()? as f64;
            (
                heuristic_base,
                Confidence::Heuristic,
                format!(
                    "{brand} is not in the local spec dataset yet — compared against CPUID's own reported base clock ({heuristic_base} MHz) instead of a verified manufacturer figure"
                ),
            )
        }
    };

    let floor = base_clock_mhz * (1.0 - CLOCK_FLOOR_TOLERANCE);

    if avg_clock < floor {
        let deficit_pct = (1.0 - avg_clock / base_clock_mhz) * 100.0;
        Some(
            Finding::new(
                "cpu.sustained_below_base",
                "CPU could not sustain its own rated base clock",
                Severity::Problem,
                confidence,
            )
            .observed(format!("averaged {avg_clock:.0} MHz across the sustained all-core phase"))
            .expected(format!("at least {base_clock_mhz:.0} MHz — base clock is the manufacturer's guarantee at rated TDP"))
            .basis(basis)
            .recommend(format!(
                "{deficit_pct:.0}% below its own rated floor under sustained load points at a cooling or power-delivery fault, not normal turbo behaviour. Budget for a repaste/cleaning, or negotiate accordingly."
            )),
        )
    } else {
        Some(
            Finding::new(
                "cpu.sustained_below_base",
                "CPU held its rated base clock under sustained load",
                Severity::Ok,
                confidence,
            )
            .observed(format!("averaged {avg_clock:.0} MHz across the sustained all-core phase"))
            .expected(format!("at least {base_clock_mhz:.0} MHz"))
            .basis(basis)
            .recommend("No cooling or power-delivery concern from this check."),
        )
    }
}

fn thermal_throttle_finding(result: &CpuStressResult) -> Option<Finding> {
    let samples: Vec<bool> = sustained_samples(result)
        .filter_map(|s| s.thermal_throttling.get().copied())
        .collect();

    if samples.is_empty() {
        return None; // AMD (no register for this) or PawnIO unavailable — not a finding
    }

    let throttled = samples.iter().filter(|&&b| b).count();
    let fraction = throttled as f64 / samples.len() as f64;

    if fraction >= THROTTLE_OBSERVED_FRACTION {
        Some(
            Finding::new(
                "cpu.thermal_throttling_observed",
                "CPU package reported thermal throttling during the sustained phase",
                Severity::Problem,
                Confidence::SpecGrounded,
            )
            .observed(format!("thermal-throttle bit set on {:.0}% of sustained-phase samples", fraction * 100.0))
            .expected("the bit clear for the great majority of a sustained run")
            .basis("Intel package thermal status register (MSR 0x1B1) — reported by the silicon itself, not inferred from a temperature threshold")
            .recommend("This is the CPU's own hardware confirming it is thermally limited. Cooling service is warranted before relying on sustained performance."),
        )
    } else {
        None // brief, isolated ticks are normal turbo behaviour and not worth a finding
    }
}

// ---------------------------------------------------------------------------
// GPU
// ---------------------------------------------------------------------------

/// Two rules, deliberately narrower than the CPU set for this pass:
///
/// 1. **Self-check failure** — same principle as the CPU kernel, and here it also
///    stands in for VRAM integrity: the checked buffer round-trips through VRAM
///    every checkpoint, so a corrupted cell (the classic ex-mining-GPU failure mode)
///    surfaces the same way a compute fault would. See `stress::gpu_kernel`'s module
///    doc comment.
/// 2. **Throttling observed** (NVIDIA only, via NVML's `nvmlDeviceGetCurrentClocksEventReasons`
///    — a bitmask NVIDIA's own driver reports, not a threshold this app picked).
///
/// No clock-floor-vs-spec rule yet: that needs a `gpu_specs.json` seed dataset
/// analogous to the CPU one, which is reasonable follow-up work rather than something
/// this pass claims to already have.
pub fn evaluate_gpu(result: &GpuStressResult) -> Vec<Finding> {
    let mut out = Vec::new();

    if result.self_check_failed {
        out.push(
            Finding::new(
                "gpu.self_check_failed",
                "GPU produced an incorrect result under sustained load",
                Severity::Critical,
                Confidence::SpecGrounded,
            )
            .observed("the compute kernel's checkpoint readback did not match its own reference result")
            .expected("bit-identical results every time — the shader and input are fixed and deterministic")
            .basis("the checked buffer is written, computed on, and read back through VRAM every checkpoint, so this catches both a compute-unit fault and VRAM corruption")
            .recommend("Treat this as a hardware fault. This is the same failure signature a degraded-memory ex-mining GPU produces — do not buy on the assumption this GPU is reliable."),
        );
    }

    let sustained: Vec<&crate::telemetry::types::GpuSample> = result
        .samples
        .iter()
        .filter(|s| s.phase == GpuPhase::ComputeSustained)
        .collect();

    let throttled = sustained
        .iter()
        .filter(|s| {
            [&s.sw_thermal_slowdown, &s.hw_thermal_slowdown, &s.sw_power_cap, &s.hw_power_brake]
                .iter()
                .any(|r| r.get().copied() == Some(true))
        })
        .count();
    let with_telemetry = sustained.iter().filter(|s| s.sw_thermal_slowdown.is_ok()).count();

    if with_telemetry > 0 {
        let fraction = throttled as f64 / with_telemetry as f64;
        if fraction >= THROTTLE_OBSERVED_FRACTION {
            let hw_thermal = sustained.iter().any(|s| s.hw_thermal_slowdown.get().copied() == Some(true));
            let cause = if hw_thermal { "thermal limiting" } else { "power-cap limiting" };
            out.push(
                Finding::new(
                    "gpu.throttling_observed",
                    format!("GPU reported {cause} during the sustained phase"),
                    Severity::Problem,
                    Confidence::SpecGrounded,
                )
                .observed(format!("a throttle-reason bit was set on {:.0}% of sustained-phase samples", fraction * 100.0))
                .expected("throttle reasons clear for the great majority of a sustained run")
                .basis("NVIDIA's own nvmlDeviceGetCurrentClocksEventReasons bitmask — the driver naming the exact cause, not an inferred threshold")
                .recommend("This is the GPU's own driver confirming it is limited during sustained load. If thermal, cooling service is warranted; if power-cap, this may simply be the card's configured limit rather than a defect — check against the model's rated behaviour."),
            );
        }
    }

    sorted(out)
}

fn sorted(mut findings: Vec<Finding>) -> Vec<Finding> {
    findings.sort_by_key(|f| std::cmp::Reverse(f.severity));
    findings
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Reading, Unavailable};
    use crate::probes::cpu::CpuVendor;

    fn topology(brand: &str) -> CpuTopology {
        CpuTopology {
            vendor: CpuVendor::Amd,
            brand_string: Reading::value(brand.to_string()),
            physical_cores: Reading::value(6),
            logical_processors: 6,
            base_clock_mhz: Reading::unsupported(),
        }
    }

    fn sample_with_clock(mhz: f64) -> CpuSample {
        CpuSample {
            elapsed_ms: 0,
            phase: Phase::AllCoreSustained,
            effective_clock_mhz: Reading::value(mhz),
            package_power_watts: Reading::unsupported(),
            configured_pl1_watts: Reading::unsupported(),
            configured_pl2_watts: Reading::unsupported(),
            package_temperature_c: Reading::unsupported(),
            thermal_throttling: Reading::unsupported(),
            self_check_ok: Some(true),
            total_iterations: 1_000_000,
        }
    }

    fn result_with_clocks(mhz_values: &[f64]) -> CpuStressResult {
        CpuStressResult {
            samples: mhz_values.iter().map(|&m| sample_with_clock(m)).collect(),
            aborted: false,
            abort_reason: None,
            self_check_failed: false,
        }
    }

    #[test]
    fn spec_dataset_parses_and_matches_the_dev_machine_cpu() {
        let spec = find_spec("amd ryzen 5 7500f 6-core processor");
        let spec = spec.expect("Ryzen 5 7500F must be in the seed dataset");
        assert_eq!(spec.base_clock_mhz, 3700);
        assert_eq!(spec.boost_clock_mhz, 5000);
    }

    #[test]
    fn unknown_cpu_does_not_match() {
        assert!(find_spec("some future cpu nobody has heard of yet").is_none());
    }

    #[test]
    fn below_rated_base_clock_is_flagged_as_a_problem() {
        // Rated base 3700 MHz; sustained average well below it.
        let result = result_with_clocks(&[2900.0, 2950.0, 2880.0]);
        let topo = topology("AMD Ryzen 5 7500F 6-Core Processor");
        let findings = evaluate_cpu(&result, &topo);
        let f = findings
            .iter()
            .find(|f| f.id == "cpu.sustained_below_base")
            .expect("expected a clock-floor finding");
        assert_eq!(f.severity, Severity::Problem);
        assert_eq!(f.confidence, Confidence::SpecGrounded);
    }

    #[test]
    fn holding_base_clock_is_reported_ok_not_omitted() {
        let result = result_with_clocks(&[3750.0, 3800.0, 3720.0]);
        let topo = topology("AMD Ryzen 5 7500F 6-Core Processor");
        let findings = evaluate_cpu(&result, &topo);
        let f = findings.iter().find(|f| f.id == "cpu.sustained_below_base").unwrap();
        assert_eq!(f.severity, Severity::Ok);
    }

    #[test]
    fn small_deficit_within_tolerance_is_not_flagged_as_a_problem() {
        // 3700 * 0.96 = 3552, inside the 5% tolerance band.
        let result = result_with_clocks(&[3600.0, 3610.0, 3590.0]);
        let topo = topology("AMD Ryzen 5 7500F 6-Core Processor");
        let findings = evaluate_cpu(&result, &topo);
        let f = findings.iter().find(|f| f.id == "cpu.sustained_below_base").unwrap();
        assert_eq!(f.severity, Severity::Ok);
    }

    #[test]
    fn unknown_cpu_without_any_base_clock_produces_no_clock_finding() {
        let result = result_with_clocks(&[2000.0]);
        let topo = topology("Some Unreleased Future CPU");
        let findings = evaluate_cpu(&result, &topo);
        assert!(findings.iter().all(|f| f.id != "cpu.sustained_below_base"));
    }

    #[test]
    fn unknown_cpu_falls_back_to_cpuid_reported_base_clock_as_heuristic() {
        let mut topo = topology("Some Unreleased Future CPU");
        topo.base_clock_mhz = Reading::value(3000);
        let result = result_with_clocks(&[2000.0, 2050.0]); // well below 3000
        let findings = evaluate_cpu(&result, &topo);
        let f = findings.iter().find(|f| f.id == "cpu.sustained_below_base").unwrap();
        assert_eq!(f.confidence, Confidence::Heuristic);
        assert_eq!(f.severity, Severity::Problem);
    }

    #[test]
    fn missing_telemetry_produces_no_clock_finding_rather_than_a_false_defect() {
        let mut result = result_with_clocks(&[]);
        result.samples.push(CpuSample {
            elapsed_ms: 0,
            phase: Phase::AllCoreSustained,
            effective_clock_mhz: Reading::missing(Unavailable::DriverMissing("PawnIO not installed".into())),
            package_power_watts: Reading::unsupported(),
            configured_pl1_watts: Reading::unsupported(),
            configured_pl2_watts: Reading::unsupported(),
            package_temperature_c: Reading::unsupported(),
            thermal_throttling: Reading::unsupported(),
            self_check_ok: Some(true),
            total_iterations: 100,
        });
        let topo = topology("AMD Ryzen 5 7500F 6-Core Processor");
        let findings = evaluate_cpu(&result, &topo);
        assert!(findings.iter().all(|f| f.id != "cpu.sustained_below_base"));
    }

    #[test]
    fn self_check_failure_is_always_critical() {
        let mut result = result_with_clocks(&[3750.0]);
        result.self_check_failed = true;
        let topo = topology("AMD Ryzen 5 7500F 6-Core Processor");
        let findings = evaluate_cpu(&result, &topo);
        let f = findings.iter().find(|f| f.id == "cpu.self_check_failed").unwrap();
        assert_eq!(f.severity, Severity::Critical);
    }

    #[test]
    fn sustained_thermal_throttling_is_flagged_when_persistent() {
        let mut result = result_with_clocks(&[3750.0; 10]);
        for s in result.samples.iter_mut().take(5) {
            s.thermal_throttling = Reading::value(true);
        }
        for s in result.samples.iter_mut().skip(5) {
            s.thermal_throttling = Reading::value(false);
        }
        let topo = topology("AMD Ryzen 5 7500F 6-Core Processor");
        let findings = evaluate_cpu(&result, &topo);
        assert!(findings.iter().any(|f| f.id == "cpu.thermal_throttling_observed"));
    }

    #[test]
    fn brief_isolated_throttle_ticks_are_not_flagged() {
        let mut result = result_with_clocks(&[3750.0; 20]);
        result.samples[0].thermal_throttling = Reading::value(true);
        for s in result.samples.iter_mut().skip(1) {
            s.thermal_throttling = Reading::value(false);
        }
        let topo = topology("AMD Ryzen 5 7500F 6-Core Processor");
        let findings = evaluate_cpu(&result, &topo);
        assert!(findings.iter().all(|f| f.id != "cpu.thermal_throttling_observed"));
    }

    #[test]
    fn findings_are_sorted_worst_first() {
        let mut result = result_with_clocks(&[2000.0]); // below base -> Problem
        result.self_check_failed = true; // -> Critical
        let topo = topology("AMD Ryzen 5 7500F 6-Core Processor");
        let findings = evaluate_cpu(&result, &topo);
        assert_eq!(findings[0].severity, Severity::Critical);
    }

    // --- GPU ---------------------------------------------------------------

    use crate::telemetry::types::GpuSample;

    fn gpu_sample(hw_thermal: Option<bool>) -> GpuSample {
        GpuSample {
            elapsed_ms: 0,
            phase: GpuPhase::ComputeSustained,
            graphics_clock_mhz: Reading::unsupported(),
            power_watts: Reading::unsupported(),
            edge_temperature_c: Reading::unsupported(),
            hotspot_temperature_c: Reading::unsupported(),
            fan_rpm: Reading::unsupported(),
            fan_percent: Reading::unsupported(),
            sw_thermal_slowdown: hw_thermal.map(|_| Reading::value(false)).unwrap_or_else(Reading::unsupported),
            hw_thermal_slowdown: hw_thermal.map(Reading::value).unwrap_or_else(Reading::unsupported),
            sw_power_cap: hw_thermal.map(|_| Reading::value(false)).unwrap_or_else(Reading::unsupported),
            hw_power_brake: hw_thermal.map(|_| Reading::value(false)).unwrap_or_else(Reading::unsupported),
            self_check_ok: Some(true),
            dispatches_completed: 10,
        }
    }

    #[test]
    fn gpu_self_check_failure_is_critical() {
        let result = GpuStressResult {
            samples: vec![gpu_sample(None)],
            aborted: false,
            abort_reason: None,
            self_check_failed: true,
        };
        let findings = evaluate_gpu(&result);
        assert!(findings.iter().any(|f| f.id == "gpu.self_check_failed" && f.severity == Severity::Critical));
    }

    #[test]
    fn gpu_persistent_thermal_throttle_is_flagged() {
        let samples = (0..10).map(|i| gpu_sample(Some(i < 5))).collect();
        let result = GpuStressResult { samples, aborted: false, abort_reason: None, self_check_failed: false };
        let findings = evaluate_gpu(&result);
        assert!(findings.iter().any(|f| f.id == "gpu.throttling_observed"));
    }

    #[test]
    fn gpu_no_telemetry_produces_no_throttle_finding() {
        let samples = vec![gpu_sample(None); 5];
        let result = GpuStressResult { samples, aborted: false, abort_reason: None, self_check_failed: false };
        let findings = evaluate_gpu(&result);
        assert!(findings.iter().all(|f| f.id != "gpu.throttling_observed"));
    }

    #[test]
    fn gpu_brief_throttle_tick_is_not_flagged() {
        let samples = (0..20).map(|i| gpu_sample(Some(i == 0))).collect();
        let result = GpuStressResult { samples, aborted: false, abort_reason: None, self_check_failed: false };
        let findings = evaluate_gpu(&result);
        assert!(findings.iter().all(|f| f.id != "gpu.throttling_observed"));
    }
}
