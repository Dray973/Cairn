//! Cairn.exe: see the `cairn_launcher` library.

#![windows_subsystem = "windows"]

fn main() {
    std::process::exit(cairn_launcher::run())
}
