// Prevents an extra console window on Windows in release builds, do NOT
// remove!! (Binaries names referenced from the workspace manifests.)
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    nicewatch_gui::run();
}