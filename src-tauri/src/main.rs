// Prevents an additional console window on Windows in release builds.
// Harmless on macOS; kept so the same entrypoint works across targets later.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    botbox_lib::run()
}
