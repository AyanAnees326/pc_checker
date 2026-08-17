//! Stress kernels and phase orchestration.
//!
//! Everything here puts real load on the machine, unlike `probes::`. Every entry
//! point is designed to fail safe: a cancelled or aborted run always leaves worker
//! threads stopped and telemetry sources closed, and a missing PawnIO driver degrades
//! every affected sample field rather than the run itself.

pub mod cpu_kernel;
pub mod gpu_kernel;
pub mod gpu_orchestrator;
pub mod orchestrator;
