#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    let builder = tauri::Builder::default();
    let builder = synergy_ui::register_handlers(builder);

    builder.run(tauri::generate_context!())
        .expect("error while running tauri application");
}
