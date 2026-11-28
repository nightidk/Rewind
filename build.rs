fn main() {
    tauri_build::try_build(tauri_build::Attributes::new().app_manifest(
        tauri_build::AppManifest::new().commands(&[
            "dispatch",
            "browse_path",
            "tray_action",
            "control_action",
        ]),
    ))
    .expect("failed to build Rewind's Tauri configuration");
}
