//! PC Checker — pre-purchase hardware inspection for used Windows machines.

pub mod analysis;
pub mod model;
pub mod pawnio;
pub mod probes;
pub mod stress;
pub mod telemetry;
pub mod win;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, State};

use model::{Finding, Reading};

/// Emit an event to the UI, reporting failures rather than discarding them.
///
/// A dropped `emit` is completely invisible from the UI's side — the stream just
/// never arrives, which looks identical to "the backend had nothing to send". That
/// silence is not hypothetical: it hid a missing Tauri capability (`core:event`)
/// that blocked *every* stream in this app from the day streaming was added, while
/// the non-streaming `scan_*` commands kept working and masked the problem.
fn emit_event<S: Serialize>(app: &AppHandle, event: &str, payload: &S) {
    if let Err(e) = app.emit(event, payload) {
        eprintln!("pc-checker: failed to emit {event}: {e}");
    }
}

/// The result of scanning one component: what was measured, and what it means.
///
/// Every component is scannable on its own. Nothing here depends on having scanned
/// any other part of the machine, so a buyer can check just the battery, or just the
/// drive, without waiting for a full sweep.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComponentScan<T> {
    pub data: T,
    pub findings: Vec<Finding>,
    pub scanned_at: String,
}

impl<T> ComponentScan<T> {
    fn new(data: T, findings: Vec<Finding>) -> Self {
        Self {
            data,
            findings,
            scanned_at: chrono::Utc::now().to_rfc3339(),
        }
    }
}

/// Machine identity and firmware-persistence checks.
#[tauri::command]
fn scan_firmware() -> ComponentScan<probes::firmware::FirmwareReport> {
    let report = probes::firmware::probe();
    let findings = analysis::findings::firmware(&report);
    ComponentScan::new(report, findings)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatteryScanData {
    pub packs: Reading<Vec<probes::battery::BatteryReport>>,
    pub history: Reading<probes::battery_history::BatteryHistory>,
}

/// Battery registers plus the capacity-degradation history.
#[tauri::command]
fn scan_battery() -> ComponentScan<BatteryScanData> {
    let packs = match probes::battery::probe() {
        Ok(list) if !list.is_empty() => Reading::value(list),
        Ok(_) => Reading::missing(model::Unavailable::NotApplicable),
        Err(e) => Reading::failed(e),
    };

    // The powercfg round-trip is only worth it when there is a pack to have a history.
    let history = if packs.is_ok() {
        probes::battery_history::probe()
    } else {
        Reading::missing(model::Unavailable::NotApplicable)
    };

    let findings = analysis::findings::battery(&packs, &history);
    ComponentScan::new(BatteryScanData { packs, history }, findings)
}

/// Drive health via NVMe or ATA SMART.
#[tauri::command]
fn scan_storage() -> ComponentScan<Vec<probes::storage::DriveReport>> {
    let drives = probes::storage::probe();
    let findings = analysis::findings::storage(&drives);
    ComponentScan::new(drives, findings)
}

/// Memory configuration from SMBIOS. This is not the destructive pattern test.
#[tauri::command]
fn scan_memory() -> ComponentScan<probes::memory::MemoryReport> {
    let (smbios, _) = probes::firmware::read_smbios();
    let report = probes::memory::from_smbios(&smbios);
    let findings = analysis::findings::memory(&report);
    ComponentScan::new(report, findings)
}

/// Display adapter identity.
#[tauri::command]
fn scan_gpu() -> ComponentScan<Reading<Vec<probes::gpu::GpuReport>>> {
    let gpus: Reading<Vec<probes::gpu::GpuReport>> = probes::gpu::probe().into();
    let findings = gpus.get().map(|g| analysis::findings::gpu(g)).unwrap_or_default();
    ComponentScan::new(gpus, findings)
}

/// Crash and hardware-error history.
#[tauri::command]
fn scan_stability() -> ComponentScan<probes::crashes::CrashHistory> {
    let history = probes::crashes::probe();
    let findings = analysis::findings::stability(&history);
    ComponentScan::new(history, findings)
}

/// Whether the process is elevated, so the UI can explain what is gated.
#[tauri::command]
fn is_elevated() -> bool {
    win::is_elevated()
}

/// Whether PawnIO is installed, so the UI can show install guidance before a CPU
/// stress test rather than failing partway through one.
#[tauri::command]
fn pawnio_status() -> pawnio::PawnIoStatus {
    pawnio::status()
}

/// CPU identity and topology — vendor, brand string, core counts, base clock. No
/// load, no findings of its own (there is nothing to flag about topology alone);
/// exists so the CPU section can show this without requiring the stress test to have
/// run first, the same way every other inventory section works.
#[tauri::command]
fn scan_cpu() -> ComponentScan<probes::cpu::CpuTopology> {
    ComponentScan::new(probes::cpu::probe(), Vec::new())
}

/// Holds the cancellation flag for whatever stress run is currently active, so a
/// separate `cancel_*` command can reach a run started by an earlier command
/// invocation. `None` when nothing is running.
///
/// `cpu_stress_running` and `cpu_monitor` provide mutual exclusion between the CPU
/// stress test and the standalone live CPU monitor: PawnIO session behavior under two
/// concurrent open handles from this process is unverified, and this codebase already
/// hit exactly this class of bug once (the AMD ADL global-session access violation,
/// fixed with `probes::adl`'s `ADL_LOCK`). Unlike `cpu_cancel`/`gpu_cancel` (which are
/// set once and never reset, so `is_some()` only means "has ever started"),
/// `cpu_stress_running` is explicitly cleared when the stress thread finishes, so it
/// reliably answers "is a stress run active right now" — the monitor checks it before
/// starting. `start_cpu_stress` conversely joins any active monitor thread before
/// proceeding, guaranteeing its PawnIO session is actually closed rather than merely
/// signaled to close, before the stress test opens its own.
#[derive(Default)]
struct StressState {
    cpu_cancel: Mutex<Option<Arc<AtomicBool>>>,
    gpu_cancel: Mutex<Option<Arc<AtomicBool>>>,
    cpu_stress_running: Arc<AtomicBool>,
    cpu_monitor: Mutex<Option<CpuMonitorHandle>>,
}

struct CpuMonitorHandle {
    cancel: Arc<AtomicBool>,
    join: std::thread::JoinHandle<()>,
}

/// Stop the live CPU monitor (if any) and block until its thread has actually exited
/// — used both by the explicit `stop_cpu_monitor` command and internally by
/// `start_cpu_stress` to enforce mutual exclusion (see `StressState`'s doc comment).
/// The join is a bounded, brief wait: the monitor loop checks its cancel flag at most
/// once per ~1 Hz tick.
fn stop_cpu_monitor_and_wait(state: &State<StressState>) -> Result<(), String> {
    let handle = state
        .cpu_monitor
        .lock()
        .map_err(|_| "stress state lock poisoned".to_string())?
        .take();
    if let Some(h) = handle {
        h.cancel.store(true, Ordering::Relaxed);
        let _ = h.join.join();
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize)]
struct StressStarted {
    topology: probes::cpu::CpuTopology,
}

/// Start a CPU stress run. Streams `stress://cpu/sample` events at ~4 Hz for the
/// duration of the twelve-minute phase schedule, then a single terminal
/// `stress://cpu/complete` event carrying the full result and findings — the same
/// `ComponentScan` shape every other scan command returns, just delivered at the end
/// of a stream instead of as the command's own return value, since a multi-minute
/// test cannot be a single request/response.
#[tauri::command]
fn start_cpu_stress(app: AppHandle, state: State<StressState>) -> Result<(), String> {
    // Mutual exclusion with the live monitor — see `StressState`'s doc comment.
    stop_cpu_monitor_and_wait(&state)?;

    let cancel = Arc::new(AtomicBool::new(false));
    *state
        .cpu_cancel
        .lock()
        .map_err(|_| "stress state lock poisoned".to_string())? = Some(Arc::clone(&cancel));

    let running_flag = Arc::clone(&state.cpu_stress_running);
    running_flag.store(true, Ordering::Relaxed);

    std::thread::Builder::new()
        .name("pc-checker-cpu-stress-orchestrator".into())
        .spawn(move || {
            let topology = probes::cpu::probe();
            emit_event(&app, "stress://cpu/started", &StressStarted { topology: topology.clone() });

            let config = stress::orchestrator::StressConfig::standard();
            let result = stress::orchestrator::run(&config, &cancel, |sample| {
                emit_event(&app, "stress://cpu/sample", &sample);
            });

            let findings = analysis::stress_findings::evaluate_cpu(&result, &topology);
            let scan = ComponentScan::new(result, findings);
            emit_event(&app, "stress://cpu/complete", &scan);
            running_flag.store(false, Ordering::Relaxed);
        })
        .map_err(|e| e.to_string())?;

    Ok(())
}

/// Signal the active CPU stress run (if any) to stop. The orchestrator's own abort
/// path takes it from there: workers are told to stop, telemetry sources are closed,
/// and a `stress://cpu/complete` event still fires with `aborted: true`.
#[tauri::command]
fn cancel_cpu_stress(state: State<StressState>) -> Result<(), String> {
    let guard = state
        .cpu_cancel
        .lock()
        .map_err(|_| "stress state lock poisoned".to_string())?;
    if let Some(cancel) = guard.as_ref() {
        cancel.store(true, Ordering::Relaxed);
    }
    Ok(())
}

/// Start the standalone live CPU metrics monitor — clock, wattage, temperature at
/// ~1 Hz, reviewable without committing to the twelve-minute stress test. Streams
/// `telemetry://cpu/live` events until `stop_cpu_monitor` is called.
#[tauri::command]
fn start_cpu_monitor(app: AppHandle, state: State<StressState>) -> Result<(), String> {
    // Mutual exclusion with the stress test — see `StressState`'s doc comment. A
    // stress run already in progress takes priority; the monitor simply does not
    // start rather than silently stealing the PawnIO session out from under it.
    if state.cpu_stress_running.load(Ordering::Relaxed) {
        return Err("a CPU stress test is already running".to_string());
    }

    // Starting again while already running just restarts cleanly from zero rather
    // than leaking the previous thread.
    stop_cpu_monitor_and_wait(&state)?;

    let cancel = Arc::new(AtomicBool::new(false));
    let cancel_for_thread = Arc::clone(&cancel);
    let join = std::thread::Builder::new()
        .name("pc-checker-cpu-monitor".into())
        .spawn(move || {
            telemetry::live_monitor::run(&cancel_for_thread, |sample| {
                emit_event(&app, "telemetry://cpu/live", &sample);
            });
        })
        .map_err(|e| e.to_string())?;

    *state
        .cpu_monitor
        .lock()
        .map_err(|_| "stress state lock poisoned".to_string())? = Some(CpuMonitorHandle { cancel, join });

    Ok(())
}

/// Signal the active live CPU monitor (if any) to stop.
#[tauri::command]
fn stop_cpu_monitor(state: State<StressState>) -> Result<(), String> {
    stop_cpu_monitor_and_wait(&state)
}

#[derive(Debug, Clone, Serialize)]
struct GpuStressStarted {
    gpu: probes::gpu::GpuReport,
}

/// Start a GPU stress run against the first hardware (non-software) display adapter.
/// Same streaming shape as `start_cpu_stress`: `stress://gpu/sample` events at ~4 Hz,
/// then a terminal `stress://gpu/complete` carrying the full result and findings.
#[tauri::command]
fn start_gpu_stress(app: AppHandle, state: State<StressState>) -> Result<(), String> {
    let gpus = probes::gpu::probe().map_err(|e| e.to_string())?;
    let gpu = gpus
        .into_iter()
        .find(|g| !g.is_software)
        .ok_or_else(|| "no hardware display adapter was found".to_string())?;

    let cancel = Arc::new(AtomicBool::new(false));
    *state
        .gpu_cancel
        .lock()
        .map_err(|_| "stress state lock poisoned".to_string())? = Some(Arc::clone(&cancel));

    std::thread::Builder::new()
        .name("pc-checker-gpu-stress-orchestrator".into())
        .spawn(move || {
            emit_event(&app, "stress://gpu/started", &GpuStressStarted { gpu: gpu.clone() });

            let config = stress::gpu_orchestrator::GpuStressConfig::standard();
            let result = stress::gpu_orchestrator::run(gpu.vendor_id, gpu.device_id, &config, &cancel, |sample| {
                emit_event(&app, "stress://gpu/sample", &sample);
            });

            let findings = analysis::stress_findings::evaluate_gpu(&result);
            let scan = ComponentScan::new(result, findings);
            emit_event(&app, "stress://gpu/complete", &scan);
        })
        .map_err(|e| e.to_string())?;

    Ok(())
}

/// Signal the active GPU stress run (if any) to stop.
#[tauri::command]
fn cancel_gpu_stress(state: State<StressState>) -> Result<(), String> {
    let guard = state
        .gpu_cancel
        .lock()
        .map_err(|_| "stress state lock poisoned".to_string())?;
    if let Some(cancel) = guard.as_ref() {
        cancel.store(true, Ordering::Relaxed);
    }
    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .manage(StressState::default())
        .invoke_handler(tauri::generate_handler![
            scan_firmware,
            scan_battery,
            scan_storage,
            scan_memory,
            scan_gpu,
            scan_stability,
            scan_cpu,
            is_elevated,
            pawnio_status,
            start_cpu_stress,
            cancel_cpu_stress,
            start_cpu_monitor,
            stop_cpu_monitor,
            start_gpu_stress,
            cancel_gpu_stress,
        ])
        .run(tauri::generate_context!())
        .expect("error while running PC Checker");
}
