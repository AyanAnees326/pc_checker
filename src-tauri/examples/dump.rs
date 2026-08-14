//! Dump the read-only inventory as JSON.
//!
//! `cargo run --example dump` — the fastest way to see exactly what the probes report
//! on a real machine without launching the UI.

fn main() {
    let inventory = pc_checker_lib::probes::collect();
    match serde_json::to_string_pretty(&inventory) {
        Ok(json) => println!("{json}"),
        Err(e) => eprintln!("failed to serialise inventory: {e}"),
    }
}
