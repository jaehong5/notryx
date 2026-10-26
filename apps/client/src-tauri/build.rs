fn main() {
    tauri_build::try_build(tauri_build::Attributes::new().app_manifest(
        tauri_build::AppManifest::new().commands(&[
            "m0_renderer_session",
            "m0_open",
            "m0_save",
            "m0_close",
            "m0_exit",
            "m0_open_isolation",
        ]),
    ))
    .expect("Tauri build configuration failed");
}
