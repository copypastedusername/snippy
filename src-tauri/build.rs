fn main() {
    // Snippy's own commands, declared so each window's capability can grant only the ones that
    // window's page calls (see capabilities/*.json); without this every page could call them all.
    let commands = &[
        "cancel_snip",
        "check_for_updates",
        "config_recovery",
        "copy_capture",
        "delete_capture",
        "editor_export",
        "editor_info",
        "editor_ocr",
        "editor_pixels",
        "editor_unsaved",
        "finish_freeform",
        "finish_monitor",
        "finish_rect",
        "finish_window",
        "get_autostart",
        "get_config",
        "hide_flyout",
        "install_update",
        "is_elevated",
        "main_ready",
        "ocr_languages",
        "open_default_apps",
        "open_editor",
        "open_keyboard_settings",
        "open_output_dir",
        "open_path",
        "overlay_metadata",
        "overlay_pixels",
        "overlay_ready",
        "pause_shortcuts",
        "pick_output_directory",
        "preview_sound",
        "recent_captures",
        "reset_config",
        "resume_shortcuts",
        "reveal_path",
        "set_autostart",
        "set_config",
        "set_windows_hotkey_off",
        "set_windows_overlay_disabled",
        "start_snip",
        "windows_shortcut_status",
    ];
    tauri_build::try_build(
        tauri_build::Attributes::new()
            .app_manifest(tauri_build::AppManifest::new().commands(commands)),
    )
    .expect("tauri-build failed");
}
