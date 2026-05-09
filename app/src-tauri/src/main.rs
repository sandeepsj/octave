// The bin entry. All the real wiring lives in lib.rs so future
// tauri::test integrations can target the same code without invoking
// the binary.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    octave_app_lib::run();
}
