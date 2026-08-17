//! Read-only inventory probes.
//!
//! Everything in this module is safe to run on a machine you do not own: no writes, no
//! configuration changes, no sustained load. This is the whole of Quick Scan and the
//! foundation of the M1 report.

pub mod adl;
pub mod ata_smart;
pub mod battery;
pub mod battery_history;
pub mod cpu;
pub mod crashes;
pub mod firmware;
pub mod gpu;
pub mod gpu_d3d12;
pub mod memory;
pub mod nvml;
pub mod storage;

use serde::{Deserialize, Serialize};

use crate::model::{Reading, Unavailable};

/// The complete read-only picture of a machine.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Inventory {
    pub firmware: firmware::FirmwareReport,
    pub cpu: cpu::CpuTopology,
    pub batteries: Reading<Vec<battery::BatteryReport>>,
    pub battery_history: Reading<battery_history::BatteryHistory>,
    pub memory: memory::MemoryReport,
    pub drives: Vec<storage::DriveReport>,
    pub gpus: Reading<Vec<gpu::GpuReport>>,
    pub crashes: crashes::CrashHistory,
    pub elevated: bool,
    pub collected_at: String,
}

pub fn collect() -> Inventory {
    // One firmware read feeds both the identity report and the memory decode.
    let (smbios, smbios_version) = firmware::read_smbios();
    let firmware = firmware::probe_from(&smbios, smbios_version);
    let memory = memory::from_smbios(&smbios);

    // A desktop having no battery is expected, not a failure.
    let is_portable = matches!(
        firmware.identity.form_factor,
        firmware::FormFactor::Laptop | firmware::FormFactor::Tablet
    );

    let batteries = match battery::probe() {
        Ok(list) if !list.is_empty() => Reading::value(list),
        Ok(_) if !is_portable => Reading::missing(Unavailable::NotApplicable),
        // A portable that reports no battery is itself worth surfacing.
        Ok(_) => Reading::missing(Unavailable::QueryFailed(
            "portable chassis reports no battery — pack removed or failed".into(),
        )),
        Err(e) => Reading::failed(e),
    };

    // Only worth the powercfg round-trip when there is a pack to have a history.
    let battery_history = if batteries.is_ok() {
        battery_history::probe()
    } else {
        Reading::missing(Unavailable::NotApplicable)
    };

    Inventory {
        firmware,
        cpu: cpu::probe(),
        batteries,
        battery_history,
        memory,
        drives: storage::probe(),
        gpus: gpu::probe().into(),
        crashes: crashes::probe(),
        elevated: crate::win::is_elevated(),
        collected_at: chrono::Utc::now().to_rfc3339(),
    }
}
