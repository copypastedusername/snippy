//! Text recognition with the OCR engine built into Windows (`Windows.Media.Ocr`).
//!
//! Windows 10 and 11 ship this engine; it recognises the languages whose OCR component Windows
//! installed with the user's display languages, and downloads nothing. Snipping Tool ships its own
//! private OCR engine instead; Snippy uses only the public Windows API.

use image::RgbaImage;
use serde::Serialize;
use windows::core::HSTRING;
use windows::Globalization::Language;
use windows::Graphics::Imaging::{BitmapPixelFormat, SoftwareBitmap};
use windows::Media::Ocr::OcrEngine;
use windows::Storage::Streams::DataWriter;
use windows::Win32::System::WinRT::{RoInitialize, RO_INIT_MULTITHREADED};

#[derive(Debug, Clone, Serialize)]
pub struct OcrLanguage {
    pub tag: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct OcrWord {
    pub text: String,
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct OcrLine {
    pub text: String,
    pub words: Vec<OcrWord>,
}

#[derive(Debug, Clone, Serialize)]
pub struct OcrText {
    /// Language tag of the engine that read the image.
    pub language: String,
    /// Clockwise angle of the text in degrees, when Windows detected one.
    pub angle: Option<f64>,
    pub lines: Vec<OcrLine>,
}

/// WinRT needs the calling thread in an apartment; workers start without one. Joining the
/// multithreaded apartment is idempotent, and a thread already in another apartment is fine too.
fn ensure_winrt() {
    // SAFETY: RoInitialize has no preconditions; its "already initialised" results are harmless.
    let _ = unsafe { RoInitialize(RO_INIT_MULTITHREADED) };
}

fn error(context: &str, error: windows::core::Error) -> String {
    format!("{context}: {}", error.message())
}

/// Languages Windows can read on this PC, in the order Windows lists them.
pub fn languages() -> Result<Vec<OcrLanguage>, String> {
    ensure_winrt();
    let available = OcrEngine::AvailableRecognizerLanguages()
        .map_err(|e| error("Windows text recognition is unavailable", e))?;
    let mut languages = Vec::new();
    for language in available {
        languages.push(OcrLanguage {
            tag: language
                .LanguageTag()
                .map(|tag| tag.to_string())
                .unwrap_or_default(),
            name: language
                .DisplayName()
                .map(|name| name.to_string())
                .unwrap_or_default(),
        });
    }
    Ok(languages)
}

fn engine(language: Option<&str>) -> Result<OcrEngine, String> {
    match language.filter(|tag| !tag.is_empty()) {
        Some(tag) => {
            let language = Language::CreateLanguage(&HSTRING::from(tag))
                .map_err(|e| error("Unknown language", e))?;
            OcrEngine::TryCreateFromLanguage(&language)
                .map_err(|_| format!("Windows has no text recognition for {tag} on this PC"))
        }
        None => auto_engine(),
    }
}

/// Picks an engine when the user did not choose a language. Windows' own default is the first
/// display language, which on a Russian PC with an English interface is English, and the English
/// engine turns Cyrillic into Latin look-alikes. Engines for other scripts (Russian, Greek, ...)
/// also read Latin text, so an installed non-English engine covers both.
fn auto_engine() -> Result<OcrEngine, String> {
    let installed: Vec<String> = languages()?
        .into_iter()
        .map(|language| language.tag)
        .collect();
    if let Some(tag) = preferred_auto_language(&installed) {
        if let Ok(engine) = Language::CreateLanguage(&HSTRING::from(tag))
            .and_then(|language| OcrEngine::TryCreateFromLanguage(&language))
        {
            return Ok(engine);
        }
    }
    OcrEngine::TryCreateFromUserProfileLanguages().map_err(|_| {
        "Windows has no text recognition for your display languages. Add a language with its \
         optical character recognition component in Settings > Time & language."
            .to_owned()
    })
}

fn preferred_auto_language(installed: &[String]) -> Option<&str> {
    installed
        .iter()
        .map(String::as_str)
        .find(|tag| !tag.eq_ignore_ascii_case("en") && !tag.to_ascii_lowercase().starts_with("en-"))
}

/// Reads the text in `image`. Coordinates are in the image's pixels. `language` is a BCP-47 tag;
/// `None` uses the first of the user's display languages that Windows can read.
pub fn recognize(image: &RgbaImage, language: Option<&str>) -> Result<OcrText, String> {
    ensure_winrt();
    let engine = engine(language)?;
    let limit = OcrEngine::MaxImageDimension().unwrap_or(10_000).max(1);
    let (image, scale) = fit_within(image, limit);

    let writer = DataWriter::new().map_err(|e| error("Could not prepare the image", e))?;
    writer
        .WriteBytes(image.as_raw())
        .map_err(|e| error("Could not prepare the image", e))?;
    let buffer = writer
        .DetachBuffer()
        .map_err(|e| error("Could not prepare the image", e))?;
    let bitmap = SoftwareBitmap::CreateCopyFromBuffer(
        &buffer,
        BitmapPixelFormat::Rgba8,
        image.width() as i32,
        image.height() as i32,
    )
    .map_err(|e| error("Could not prepare the image", e))?;

    let result = engine
        .RecognizeAsync(&bitmap)
        .and_then(|operation| operation.get())
        .map_err(|e| error("Text recognition failed", e))?;

    let mut lines = Vec::new();
    for line in result
        .Lines()
        .map_err(|e| error("Text recognition failed", e))?
    {
        let mut words = Vec::new();
        for word in line
            .Words()
            .map_err(|e| error("Text recognition failed", e))?
        {
            let rect = word
                .BoundingRect()
                .map_err(|e| error("Text recognition failed", e))?;
            words.push(OcrWord {
                text: word.Text().map(|text| text.to_string()).unwrap_or_default(),
                x: f64::from(rect.X) / scale,
                y: f64::from(rect.Y) / scale,
                width: f64::from(rect.Width) / scale,
                height: f64::from(rect.Height) / scale,
            });
        }
        lines.push(OcrLine {
            text: line.Text().map(|text| text.to_string()).unwrap_or_default(),
            words,
        });
    }
    Ok(OcrText {
        language: engine
            .RecognizerLanguage()
            .and_then(|language| language.LanguageTag())
            .map(|tag| tag.to_string())
            .unwrap_or_default(),
        angle: result.TextAngle().ok().and_then(|angle| angle.Value().ok()),
        lines,
    })
}

/// Shrinks images larger than the engine accepts; returns the image and the scale applied.
fn fit_within(image: &RgbaImage, limit: u32) -> (std::borrow::Cow<'_, RgbaImage>, f64) {
    let longest = image.width().max(image.height());
    if longest <= limit {
        return (std::borrow::Cow::Borrowed(image), 1.0);
    }
    let scale = f64::from(limit) / f64::from(longest);
    let width = ((f64::from(image.width()) * scale) as u32).max(1);
    let height = ((f64::from(image.height()) * scale) as u32).max(1);
    let resized =
        image::imageops::resize(image, width, height, image::imageops::FilterType::Triangle);
    (std::borrow::Cow::Owned(resized), scale)
}

#[cfg(test)]
mod tests {
    use super::{fit_within, languages, preferred_auto_language, recognize};

    #[test]
    fn auto_prefers_an_engine_that_also_reads_latin() {
        let tags = |list: &[&str]| list.iter().map(|tag| (*tag).to_owned()).collect::<Vec<_>>();
        assert_eq!(preferred_auto_language(&tags(&["en-US", "ru"])), Some("ru"));
        assert_eq!(preferred_auto_language(&tags(&["en-GB", "en"])), None);
        assert_eq!(
            preferred_auto_language(&tags(&["de-DE", "en-US"])),
            Some("de-DE")
        );
    }
    use image::{Rgba, RgbaImage};

    #[test]
    fn large_images_are_scaled_into_the_engine_limit() {
        let image = RgbaImage::new(400, 100);
        let (fitted, scale) = fit_within(&image, 200);
        assert_eq!(fitted.dimensions(), (200, 50));
        assert!((scale - 0.5).abs() < f64::EPSILON);
        let (same, unit) = fit_within(&image, 1000);
        assert_eq!(same.dimensions(), (400, 100));
        assert!((unit - 1.0).abs() < f64::EPSILON);
    }

    /// Exercises the real Windows engine; skipped where no OCR language is installed.
    #[test]
    fn a_blank_image_has_no_text() {
        if languages().map(|list| list.is_empty()).unwrap_or(true) {
            return;
        }
        let blank = RgbaImage::from_pixel(64, 64, Rgba([255, 255, 255, 255]));
        let text = recognize(&blank, None).unwrap_or_else(|error| panic!("{error}"));
        assert!(text.lines.is_empty());
    }
}
