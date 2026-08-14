//! PC Checker — pre-purchase hardware inspection for used Windows machines.

pub mod model;
pub mod probes;
pub mod win;

/// Collect the full read-only inventory (Quick Scan).
#[tauri::command]
fn collect_inventory() -> probes::Inventory {
    probes::collect()
}

/// Whether the process is elevated, so the UI can explain what is gated.
#[tauri::command]
fn is_elevated() -> bool {
    win::is_elevated()
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![collect_inventory, is_elevated])
        .run(tauri::generate_context!())
        .expect("error while running PC Checker");
}
