//! The annotation editor window: opens a capture, serves its pixels, reads its text and saves or
//! copies the edited result. Drawing itself happens in `ui/editor.html`; the page sends back the
//! finished image as PNG bytes.

use crate::config::Format;
use crate::storage::{self, CaptureItem};
use crate::{clipboard, imageops, AppState};
use image::RgbaImage;
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use tauri::{Emitter, Manager, WebviewUrl};
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};

pub const EDITOR: &str = "editor";

/// What the editor shows: a capture file, or the latest snip when it was not saved. `id` tells
/// documents apart, so pixels and exports never mix two of them.
pub struct EditorDocument {
    pub id: u64,
    pub path: Option<PathBuf>,
    pub image: Arc<RgbaImage>,
}

static NEXT_DOCUMENT: AtomicU64 = AtomicU64::new(1);
/// The page reports whether the editor holds changes that were not saved or copied.
static UNSAVED: AtomicBool = AtomicBool::new(false);
/// A "discard changes?" question is on screen; further opens and closes wait for its answer.
static ASKING: AtomicBool = AtomicBool::new(false);
/// The user agreed to close despite unsaved changes: the close that follows must not ask again.
static CLOSE_APPROVED: AtomicBool = AtomicBool::new(false);
/// The document shown when the editor began closing (0: not closing). If another picture was
/// opened by the time the window is gone, it is kept and shown in a new window.
static CLOSING_DOCUMENT: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Serialize)]
pub struct EditorInfo {
    pub id: u64,
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
                id: NEXT_DOCUMENT.fetch_add(1, Ordering::Relaxed),
                path: Some(path),
                image: Arc::new(image),
            }
        }
        None => {
            let image = crate::lock(&state.last_capture, "last capture")?
                .clone()
                .ok_or_else(|| "There is no snip to edit yet".to_owned())?;
            EditorDocument {
                id: NEXT_DOCUMENT.fetch_add(1, Ordering::Relaxed),
                path: None,
                image,
            }
        }
    };
    let editing = app
        .get_webview_window(EDITOR)
        .is_some_and(|window| window.is_visible().unwrap_or(false));
    if editing && ASKING.load(Ordering::Acquire) {
        // Already asking about the unsaved changes: that answer decides; show the question.
        return focus(app);
    }
    if editing && UNSAVED.load(Ordering::Acquire) {
        // The editor holds unsaved changes: replace them only if the user agrees.
        let app = app.clone();
        confirm_discard(
            &app.clone(),
            "Open this picture in the editor? The changes to the picture there are not saved.",
            "Open and discard changes",
            move |discard| {
                let shown = if discard {
                    replace(&app, document)
                } else {
                    focus(&app)
                };
                if let Err(error) = shown {
                    log::warn!("{error}");
                }
            },
        );
        return Ok(());
    }
    replace(app, document)
}

fn replace(app: &tauri::AppHandle, document: EditorDocument) -> Result<(), String> {
    *crate::lock(&app.state::<AppState>().editor, "editor")? = Some(document);
    UNSAVED.store(false, Ordering::Release);
    show_window(app)
}

fn focus(app: &tauri::AppHandle) -> Result<(), String> {
    if let Some(window) = app.get_webview_window(EDITOR) {
        let _ = window.unminimize();
        window
            .set_focus()
            .map_err(|error| format!("Could not show the editor: {error}"))?;
    }
    Ok(())
}

/// Asks whether to drop the editor's unsaved changes; `then(true)` means drop them. The question
/// is not blocking, so it is safe from any thread.
fn confirm_discard(
    app: &tauri::AppHandle,
    question: &str,
    discard_label: &str,
    then: impl FnOnce(bool) + Send + 'static,
) {
    ASKING.store(true, Ordering::Release);
    let mut dialog = app
        .dialog()
        .message(question)
        .title("Snippy")
        .kind(MessageDialogKind::Warning)
        .buttons(MessageDialogButtons::OkCancelCustom(
            discard_label.to_owned(),
            "Keep editing".to_owned(),
        ));
    if let Some(window) = app.get_webview_window(EDITOR) {
        dialog = dialog.parent(&window);
    }
    dialog.show(move |discard| {
        ASKING.store(false, Ordering::Release);
        then(discard);
    });
}

/// The page's report: true while it holds changes that were not saved or copied.
pub fn set_unsaved(unsaved: bool) {
    UNSAVED.store(unsaved, Ordering::Release);
}

/// The editor's close button (or Alt+F4): with unsaved changes, close only if the user agrees.
pub fn close_requested(window: &tauri::Window, api: &tauri::CloseRequestApi) {
    if ASKING.load(Ordering::Acquire) {
        api.prevent_close();
        return;
    }
    if CLOSE_APPROVED.swap(false, Ordering::AcqRel) || !UNSAVED.load(Ordering::Acquire) {
        let current = current_document(window.app_handle()).unwrap_or(0);
        CLOSING_DOCUMENT.store(current, Ordering::Release);
        return;
    }
    api.prevent_close();
    let app = window.app_handle().clone();
    confirm_discard(
        &app.clone(),
        "Close the editor? Your changes are not saved.",
        "Close and discard changes",
        move |discard| {
            if discard {
                CLOSE_APPROVED.store(true, Ordering::Release);
                if let Some(window) = app.get_webview_window(EDITOR) {
                    let _ = window.close();
                }
            }
        },
    );
}

fn current_document(app: &tauri::AppHandle) -> Option<u64> {
    crate::lock(&app.state::<AppState>().editor, "editor")
        .ok()?
        .as_ref()
        .map(|document| document.id)
}

/// The editor window is gone: let go of its picture, unless another one was opened while it was
/// closing; that one gets a new window.
pub fn closed(app: &tauri::AppHandle) {
    UNSAVED.store(false, Ordering::Release);
    CLOSE_APPROVED.store(false, Ordering::Release);
    let closing = CLOSING_DOCUMENT.swap(0, Ordering::AcqRel);
    let reopened = current_document(app).is_some_and(|current| current != closing);
    if reopened {
        let app = app.clone();
        // Once this window is fully gone, so the new one can take its label.
        let _ = app.clone().run_on_main_thread(move || {
            if let Err(error) = show_window(&app) {
                log::warn!("{error}");
            }
        });
        return;
    }
    if let Ok(mut editor) = crate::lock(&app.state::<AppState>().editor, "editor") {
        *editor = None;
    }
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

/// The open document's id, path and image. With `expected`, a different document (another picture
/// opened meanwhile) is an error, so its pixels or name never go with the wrong picture.
fn document_image(
    app: &tauri::AppHandle,
    expected: Option<u64>,
) -> Result<(u64, Option<PathBuf>, Arc<RgbaImage>), String> {
    let state = app.state::<AppState>();
    let editor = crate::lock(&state.editor, "editor")?;
    let document = editor
        .as_ref()
        .ok_or_else(|| "Nothing is open in the editor".to_owned())?;
    if expected.is_some_and(|expected| expected != document.id) {
        return Err("Another picture was opened in the editor meanwhile".to_owned());
    }
    Ok((document.id, document.path.clone(), document.image.clone()))
}

pub fn info(app: &tauri::AppHandle) -> Result<EditorInfo, String> {
    let (id, path, image) = document_image(app, None)?;
    Ok(EditorInfo {
        id,
        name: path.as_deref().and_then(Path::file_name).map_or_else(
            || "Unsaved snip".to_owned(),
            |name| name.to_string_lossy().into_owned(),
        ),
        path: path.map(|path| path.to_string_lossy().into_owned()),
        width: image.width(),
        height: image.height(),
    })
}

/// RGBA8 pixels of document `id`, `width * height * 4` bytes.
pub fn pixels(app: &tauri::AppHandle, id: Option<u64>) -> Result<Vec<u8>, String> {
    Ok(document_image(app, id)?.2.as_raw().clone())
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
    document: Option<u64>,
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
            let (_, original, _) = document_image(app, document)?;
            let config = app.state::<AppState>().config()?;
            let directory = original
                .as_deref()
                .and_then(Path::parent)
                .map_or_else(|| config.output.directory.clone(), Path::to_path_buf);
            let stem = original
                .as_deref()
                .and_then(Path::file_stem)
                .map(|stem| edited_stem(&stem.to_string_lossy()));
            let path = save_next_to(&image, &directory, stem.as_deref(), &config.output)?;
            remember(app, &path, &image, &config);
            Ok(Some(path.to_string_lossy().into_owned()))
        }
        ExportAction::SaveAs => {
            let (_, original, _) = document_image(app, document)?;
            let suggested = original.as_deref().and_then(Path::file_stem).map_or_else(
                || "snip".to_owned(),
                |stem| edited_stem(&stem.to_string_lossy()),
            );
            let mut dialog = app
                .dialog()
                .file()
                .add_filter("PNG image", &["png"])
                .add_filter("JPEG image", &["jpg", "jpeg"])
                .set_file_name(format!("{suggested}.png"));
            if let Some(folder) = original.as_deref().and_then(Path::parent) {
                dialog = dialog.set_directory(folder);
            }
            let Some(target) = dialog.blocking_save_file() else {
                return Ok(None);
            };
            let chosen = target
                .into_path()
                .map_err(|error| format!("Invalid file name: {error}"))?;
            let (path, format) = save_as_target(chosen.clone());
            let config = app.state::<AppState>().config()?;
            let bytes = imageops::encode(&image, format, config.output.jpeg_quality)?;
            if path == chosen {
                // Windows already asked before replacing this name.
                write_replacing(&path, &bytes)?;
            } else {
                // `.png` was added to the chosen name: never replace a file nobody agreed to.
                write_new(&path, &bytes)?;
            }
            if path.parent() == Some(config.output.directory.as_path()) {
                remember(app, &path, &image, &config);
            }
            Ok(Some(path.to_string_lossy().into_owned()))
        }
    }
}

/// Writes the whole file beside the target first and then puts it in place, so a full disk or a
/// lost drive leaves an existing file as it was instead of cut short.
fn write_replacing(path: &Path, bytes: &[u8]) -> Result<(), String> {
    static SAVES: AtomicU64 = AtomicU64::new(0);
    let name = path
        .file_name()
        .ok_or_else(|| format!("Invalid file name: {}", path.display()))?;
    // A name of its own for every save, so two saves (or a stray file) never share it.
    let mut temporary_name = std::ffi::OsString::from(".");
    temporary_name.push(name);
    temporary_name.push(format!(
        ".{}-{}.snippy-saving",
        std::process::id(),
        SAVES.fetch_add(1, Ordering::Relaxed)
    ));
    let temporary = path.with_file_name(temporary_name);
    let written = write_new(&temporary, bytes).and_then(|()| {
        std::fs::rename(&temporary, path)
            .map_err(|error| format!("Failed to save {}: {error}", path.display()))
    });
    if written.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    written
}

/// Writes a file that must not exist yet.
fn write_new(path: &Path, bytes: &[u8]) -> Result<(), String> {
    use std::io::Write;
    let created = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path);
    let mut file = created.map_err(|error| {
        if error.kind() == std::io::ErrorKind::AlreadyExists {
            format!("{} already exists. Choose another name.", path.display())
        } else {
            format!("Failed to save {}: {error}", path.display())
        }
    })?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|error| {
            let _ = std::fs::remove_file(path);
            format!("Failed to save {}: {error}", path.display())
        })
}

/// The file Save as writes and its format. A name typed without `.png`, `.jpg` or `.jpeg` gets
/// `.png`, so the file never carries an extension that does not match its contents.
fn save_as_target(chosen: PathBuf) -> (PathBuf, Format) {
    let extension = chosen
        .extension()
        .map(|extension| extension.to_string_lossy().to_ascii_lowercase());
    match extension.as_deref() {
        Some("jpg" | "jpeg") => (chosen, Format::Jpeg),
        Some("png") => (chosen, Format::Png),
        _ => {
            let mut name = chosen.into_os_string();
            name.push(".png");
            (PathBuf::from(name), Format::Png)
        }
    }
}

const EDITED_SUFFIX: &str = "-edited";

/// `<original>-edited`, with the original cut (at a whole character) so the name stays within
/// what `storage::save` accepts. A name cut this way may match another edit's; saving then picks
/// the next free `_N` name.
fn edited_stem(original: &str) -> String {
    let room = storage::MAX_STEM_CHARS - EDITED_SUFFIX.encode_utf16().count();
    let mut used = 0;
    let kept: String = original
        .chars()
        .take_while(|character| {
            used += character.len_utf16();
            used <= room
        })
        .collect();
    format!("{kept}{EDITED_SUFFIX}")
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
    use super::{edited_stem, save_as_target, save_next_to, ExportAction};
    use crate::config::{Format, OutputConfig};
    use crate::storage::MAX_STEM_CHARS;
    use image::{Rgba, RgbaImage};
    use std::path::PathBuf;

    #[test]
    fn writing_in_place_replaces_the_file_and_leaves_no_temporary_one() {
        let directory = std::env::temp_dir().join(format!("snippy-replace-{}", std::process::id()));
        std::fs::create_dir_all(&directory).expect("temp dir");
        let target = directory.join("picture.png");
        std::fs::write(&target, b"old").expect("old file");
        super::write_replacing(&target, b"new contents").expect("replaced");
        assert_eq!(std::fs::read(&target).expect("read"), b"new contents");
        let names: Vec<_> = std::fs::read_dir(&directory)
            .expect("list")
            .map(|entry| entry.expect("entry").file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from("picture.png")]);
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn a_new_file_is_never_written_over_an_existing_one() {
        let directory = std::env::temp_dir().join(format!("snippy-new-{}", std::process::id()));
        std::fs::create_dir_all(&directory).expect("temp dir");
        let target = directory.join("report.v2.png");
        std::fs::write(&target, b"keep me").expect("existing file");
        assert!(super::write_new(&target, b"other").is_err());
        assert_eq!(std::fs::read(&target).expect("read"), b"keep me");
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn save_as_keeps_a_known_extension_and_adds_png_otherwise() {
        let target = |name: &str| save_as_target(PathBuf::from(name));
        assert_eq!(
            target(r"C:\a\shot.JPG"),
            (PathBuf::from(r"C:\a\shot.JPG"), Format::Jpeg)
        );
        assert_eq!(
            target(r"C:\a\shot.png"),
            (PathBuf::from(r"C:\a\shot.png"), Format::Png)
        );
        assert_eq!(
            target(r"C:\a\shot"),
            (PathBuf::from(r"C:\a\shot.png"), Format::Png)
        );
        assert_eq!(
            target(r"C:\a\v1.2"),
            (PathBuf::from(r"C:\a\v1.2.png"), Format::Png)
        );
    }

    #[test]
    fn long_names_are_cut_so_the_edited_copy_can_be_saved() {
        assert_eq!(edited_stem("shot"), "shot-edited");
        // 192 units, then an emoji (two units) that no longer fits before the suffix.
        let long = format!("{}😀tail%Y", "a".repeat(192));
        let stem = edited_stem(&long);
        assert_eq!(stem, format!("{}-edited", "a".repeat(192)));
        assert!(stem.encode_utf16().count() <= MAX_STEM_CHARS);

        let directory =
            std::env::temp_dir().join(format!("snippy-edit-long-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap_or_else(|error| panic!("{error}"));
        let image = RgbaImage::from_pixel(1, 1, Rgba([1, 2, 3, 255]));
        let percent = edited_stem(&format!("{}%Y{}", "b".repeat(190), "c".repeat(10)));
        let saved = save_next_to(&image, &directory, Some(&percent), &OutputConfig::default());
        let _ = std::fs::remove_dir_all(&directory);
        let saved = saved.unwrap_or_else(|error| panic!("{error}"));
        // The `%` stays literal and the name fits.
        assert_eq!(
            saved.file_stem().and_then(|stem| stem.to_str()),
            Some(format!("{}%Yc-edited", "b".repeat(190)).as_str())
        );
    }

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
