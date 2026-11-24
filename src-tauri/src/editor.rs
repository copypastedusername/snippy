//! The annotation editor window: opens a capture, serves its pixels, reads its text and saves or
//! copies the edited result. Drawing itself happens in `ui/editor.html`; the page sends back the
//! finished image as PNG bytes.

use crate::config::Format;
use crate::storage::{self, CaptureItem};
use crate::{clipboard, imageops, AppState};
use image::RgbaImage;
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tauri::{Emitter, Manager, WebviewUrl};
use tauri_plugin_dialog::DialogExt;

pub const EDITOR: &str = "editor";

/// What the editor shows: a capture file, or the latest snip when it was not saved.
pub struct EditorDocument {
    pub path: Option<PathBuf>,
    pub image: Arc<RgbaImage>,
}

#[derive(Debug, Clone, Serialize)]
pub struct EditorInfo {
    pub name: String,
    pub path: Option<String>,
    pub width: u32,
    pub height: u32,
}

/// Opens `path` (a capture in the output folder), or the latest snip when `path` is `None`.
pub fn open(app: &tauri::AppHandle, path: Option<&Path>) -> Result<(), String> {
    let state = app.state::<AppState>();
    let document = match path {
        Some(path) => {
            let directory = state.config()?.output.directory;
            let path = storage::resolve_capture(path, &directory)?;
            let image = image::open(&path)
                .map_err(|error| format!("Failed to read {}: {error}", path.display()))?
                .into_rgba8();
            EditorDocument {
                path: Some(path),
                image: Arc::new(image),
            }
        }
        None => {
            let image = crate::lock(&state.last_capture, "last capture")?
                .clone()
                .ok_or_else(|| "There is no snip to edit yet".to_owned())?;
            EditorDocument { path: None, image }
        }
    };
    *crate::lock(&state.editor, "editor")? = Some(document);
    show_window(app)
}

/// Creates the editor window on first use, otherwise reloads its document. Windows are built on a
/// separate thread: building one from a synchronous handler on the main thread can deadlock.
fn show_window(app: &tauri::AppHandle) -> Result<(), String> {
    if let Some(window) = app.get_webview_window(EDITOR) {
        app.emit_to(EDITOR, "editor-load", ())
            .map_err(|error| format!("Could not refresh the editor: {error}"))?;
        let _ = window.unminimize();
        window
            .show()
            .and_then(|()| window.set_focus())
            .map_err(|error| format!("Could not show the editor: {error}"))?;
        return Ok(());
    }
    let app = app.clone();
    std::thread::Builder::new()
        .name("snippy-editor-window".to_owned())
        .spawn(move || {
            let profile = crate::config::local_data_dir().join("webview");
            let built = tauri::WebviewWindowBuilder::new(
                &app,
                EDITOR,
                WebviewUrl::App("editor.html".into()),
            )
            .title("Snippy | Editor")
            .inner_size(1100.0, 760.0)
            .min_inner_size(640.0, 420.0)
            .center()
            .visible(false)
            .theme(crate::overlay::NATIVE_THEME)
            .background_color(crate::overlay::BACKGROUND)
            .data_directory(profile)
            .build();
            if let Err(error) = built {
                log::error!("Could not open the editor: {error}");
            }
        })
        .map(|_| ())
        .map_err(|error| format!("Could not open the editor: {error}"))
}

fn document_image(app: &tauri::AppHandle) -> Result<(Option<PathBuf>, Arc<RgbaImage>), String> {
    let state = app.state::<AppState>();
    let editor = crate::lock(&state.editor, "editor")?;
    let document = editor
        .as_ref()
        .ok_or_else(|| "Nothing is open in the editor".to_owned())?;
    Ok((document.path.clone(), document.image.clone()))
}

pub fn info(app: &tauri::AppHandle) -> Result<EditorInfo, String> {
    let (path, image) = document_image(app)?;
    Ok(EditorInfo {
        name: path.as_deref().and_then(Path::file_name).map_or_else(
            || "Unsaved snip".to_owned(),
            |name| name.to_string_lossy().into_owned(),
        ),
        path: path.map(|path| path.to_string_lossy().into_owned()),
        width: image.width(),
        height: image.height(),
    })
}

/// RGBA8 pixels of the open image, `width * height * 4` bytes.
pub fn pixels(app: &tauri::AppHandle) -> Result<Vec<u8>, String> {
    Ok(document_image(app)?.1.as_raw().clone())
}

/// What to do with the edited image the page sends back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportAction {
    Copy,
    /// A new file next to the original: `name-edited.png`.
    Save,
    /// A file the user picks.
    SaveAs,
}

impl std::str::FromStr for ExportAction {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "copy" => Ok(Self::Copy),
            "save" => Ok(Self::Save),
            "save-as" => Ok(Self::SaveAs),
            other => Err(format!("Unknown editor action: {other}")),
        }
    }
}

/// Copies or saves the edited image. Returns the saved path, if any.
pub fn export(
    app: &tauri::AppHandle,
    png: &[u8],
    action: ExportAction,
) -> Result<Option<String>, String> {
    let image = image::load_from_memory_with_format(png, image::ImageFormat::Png)
        .map_err(|error| format!("The edited image is not a valid PNG: {error}"))?
        .into_rgba8();
    match action {
        ExportAction::Copy => {
            clipboard::copy_image_to_clipboard(&image)?;
            Ok(None)
        }
        ExportAction::Save => {
            let (original, _) = document_image(app)?;
            let config = app.state::<AppState>().config()?;
            let directory = original
                .as_deref()
                .and_then(Path::parent)
                .map_or_else(|| config.output.directory.clone(), Path::to_path_buf);
            let stem = original
                .as_deref()
                .and_then(Path::file_stem)
                .map(|stem| format!("{}-edited", stem.to_string_lossy()));
            let path = save_next_to(&image, &directory, stem.as_deref(), &config.output)?;
            remember(app, &path, &image, &config);
            Ok(Some(path.to_string_lossy().into_owned()))
        }
        ExportAction::SaveAs => {
            let Some(target) = app
                .dialog()
                .file()
                .add_filter("PNG image", &["png"])
                .add_filter("JPEG image", &["jpg", "jpeg"])
                .set_file_name("snip.png")
                .blocking_save_file()
            else {
                return Ok(None);
            };
            let path = target
                .into_path()
                .map_err(|error| format!("Invalid file name: {error}"))?;
            let format = if path.extension().is_some_and(|ext| {
                ext.eq_ignore_ascii_case("jpg") || ext.eq_ignore_ascii_case("jpeg")
            }) {
                Format::Jpeg
            } else {
                Format::Png
            };
            let quality = app.state::<AppState>().config()?.output.jpeg_quality;
            let bytes = imageops::encode(&image, format, quality)?;
            std::fs::write(&path, bytes)
                .map_err(|error| format!("Failed to save {}: {error}", path.display()))?;
            Ok(Some(path.to_string_lossy().into_owned()))
        }
    }
}

/// Saves `image` into `directory` named `stem` (escaped for the strftime pattern), or by the
/// configured pattern when there is no original to name it after.
fn save_next_to(
    image: &RgbaImage,
    directory: &Path,
    stem: Option<&str>,
    output: &crate::config::OutputConfig,
) -> Result<PathBuf, String> {
    let mut target = output.clone();
    target.directory = directory.to_path_buf();
    target.format = Format::Png;
    if let Some(stem) = stem {
        target.filename_pattern = stem.replace('%', "%%");
    }
    storage::save(image, &target)
}

fn remember(
    app: &tauri::AppHandle,
    path: &Path,
    image: &RgbaImage,
    config: &crate::config::Config,
) {
    let item: CaptureItem = storage::capture_item(path, image, imageops::thumbnail_data_url(image));
    crate::capture::remember(app, item, config);
    if let Err(error) = app.emit("recents-changed", ()) {
        log::warn!("Could not announce the saved edit: {error}");
    }
}

#[cfg(test)]
mod tests {
    use super::{save_next_to, ExportAction};
    use crate::config::OutputConfig;
    use image::{Rgba, RgbaImage};

    #[test]
    fn export_actions_parse_exactly() {
        assert_eq!("copy".parse(), Ok(ExportAction::Copy));
        assert_eq!("save".parse(), Ok(ExportAction::Save));
        assert_eq!("save-as".parse(), Ok(ExportAction::SaveAs));
        assert!("delete".parse::<ExportAction>().is_err());
    }

    #[test]
    fn edits_are_saved_beside_the_original_without_overwriting() {
        let directory = std::env::temp_dir().join(format!("snippy-edit-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap_or_else(|error| panic!("{error}"));
        let image = RgbaImage::from_pixel(2, 2, Rgba([1, 2, 3, 255]));
        let output = OutputConfig::default();

        let first = save_next_to(&image, &directory, Some("shot%Y-edited"), &output)
            .unwrap_or_else(|error| panic!("{error}"));
        let second = save_next_to(&image, &directory, Some("shot%Y-edited"), &output)
            .unwrap_or_else(|error| panic!("{error}"));

        assert_eq!(
            first.file_name().and_then(|n| n.to_str()),
            Some("shot%Y-edited.png")
        );
        assert_eq!(
            second.file_name().and_then(|n| n.to_str()),
            Some("shot%Y-edited_2.png")
        );
        std::fs::remove_dir_all(&directory).unwrap_or_else(|error| panic!("{error}"));
    }
}
