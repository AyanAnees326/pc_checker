//! Dump the read-only inventory as JSON.
//!
//! `cargo run --example dump` — the fastest way to see exactly what the probes report
//! on a real machine without launching the UI.

fn main() {
    let inventory = pc_checker_lib::probes::collect();
    let findings = pc_checker_lib::analysis::findings::evaluate(&inventory);

    #[derive(serde::Serialize)]
    struct Dump {
        inventory: pc_checker_lib::probes::Inventory,
        findings: Vec<pc_checker_lib::model::Finding>,
    }

    match serde_json::to_string_pretty(&Dump { inventory, findings }) {
        Ok(json) => println!("{json}"),
        Err(e) => eprintln!("failed to serialise scan result: {e}"),
    }
}
