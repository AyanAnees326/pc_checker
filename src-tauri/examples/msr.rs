//! Check whether CPU MSR telemetry actually works on this machine.
//!
//! `cargo run --example msr` — **must be run from an elevated shell**; opening a
//! PawnIO session requires administrator, so an unelevated run reports a permission
//! failure rather than a missing driver.
//!
//! Exists because "is PawnIO working here?" was previously only answerable by
//! launching the full UI and starting a 12-minute stress test. Two samples a second
//! apart is enough: every telemetry value is a *delta* against the previous sample,
//! so the first one is expected to be blank and the second is the real answer.

use std::thread;
use std::time::Duration;

use pc_checker_lib::model::Reading;
use pc_checker_lib::pawnio;
use pc_checker_lib::probes::cpu;
use pc_checker_lib::telemetry::cpu_source::{MsrSource, TelemetryState};

fn show<T: std::fmt::Debug>(label: &str, reading: &Reading<T>) {
    match reading {
        Reading::Ok { value, .. } => println!("  {label:<24} {value:?}"),
        Reading::Missing { note, .. } => println!("  {label:<24} -- {note}"),
    }
}

fn main() {
    println!("elevated:      {}", pc_checker_lib::win::is_elevated());
    println!("pawnio status: {:?}\n", pawnio::status());

    let topology = cpu::probe();
    println!("cpu vendor:    {:?}", topology.vendor);

    let msr = MsrSource::open(topology.vendor);
    match &msr {
        MsrSource::Intel(_) => println!("msr source:    Intel MSR session open"),
        MsrSource::Amd(_) => println!("msr source:    AMD MSR session open"),
        MsrSource::Unavailable(reason) => {
            println!("msr source:    UNAVAILABLE -- {}", reason.describe());
        }
    }

    let mut state = TelemetryState::new(&msr, &topology);
    // First sample primes the deltas and is expected to be blank; the second is the
    // one worth reading.
    let _ = state.sample(&msr);
    thread::sleep(Duration::from_secs(1));
    let s = state.sample(&msr);

    println!("\nreadings after 1s:");
    show("effective clock (MHz)", &s.effective_clock_mhz);
    show("package power (W)", &s.package_power_watts);
    show("configured PL1 (W)", &s.configured_pl1_watts);
    show("configured PL2 (W)", &s.configured_pl2_watts);
    show("package temp (C)", &s.package_temperature_c);
    show("thermal throttling", &s.thermal_throttling);
}
