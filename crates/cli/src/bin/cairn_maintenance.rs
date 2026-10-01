//! Runs scheduled maintenance for the Task Scheduler task without a console window.
#![windows_subsystem = "windows"]
fn main() {
    std::process::exit(optimizer_core::maintenance::run_scheduled(
        std::env::args_os().skip(1),
    ))
}
