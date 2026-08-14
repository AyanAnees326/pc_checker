// Hide the console window in release builds; keep it in dev for probe logging.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    pc_checker_lib::run();
}
