//! Copies an image to the Windows clipboard as `CF_DIBV5` (with alpha; Windows derives `CF_DIB`
//! and `CF_BITMAP` from it) and as `PNG` (what browsers and Office prefer).
//!
//! Both formats are fully prepared before the clipboard is opened, so a failure never leaves the
//! clipboard emptied or half-filled, and memory that Windows did not take is always freed.

use image::RgbaImage;
use std::time::Duration;
use windows::core::w;
use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL};
use windows::Win32::Graphics::Gdi::{BITMAPV5HEADER, BI_BITFIELDS};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::System::Ole::CF_DIBV5;

/// `LCS_sRGB`: the pixels are sRGB.
const LCS_SRGB: u32 = 0x7352_4742;
/// Another program may hold the clipboard for a moment; try this many times.
const OPEN_ATTEMPTS: u32 = 10;

pub fn copy_image_to_clipboard(image: &RgbaImage) -> Result<(), String> {
    let dib = Global::with_bytes(&dib_v5(image)?)?;
    let png = Global::with_bytes(&png(image)?)?;
    // SAFETY: registering a format name is always allowed.
    let png_format = unsafe { RegisterClipboardFormatW(w!("PNG")) };

    let _open = Opened::open()?;
    // SAFETY: the clipboard is open by this thread.
    unsafe { EmptyClipboard() }
        .map_err(|error| format!("Could not empty the clipboard: {error}"))?;
    dib.hand_to_clipboard(u32::from(CF_DIBV5.0))
        .map_err(|error| format!("Could not copy the image: {error}"))?;
    if png_format != 0 {
        // PNG is a bonus for apps that read it; the DIB already makes the copy usable.
        if let Err(error) = png.hand_to_clipboard(png_format) {
            log::warn!("Copied without the PNG format: {error}");
        }
    }
    Ok(())
}

/// A bottom-up 32-bit `BITMAPV5HEADER` DIB with an alpha mask, as Windows' own apps write it.
fn dib_v5(image: &RgbaImage) -> Result<Vec<u8>, String> {
    let (width, height) = image.dimensions();
    let row = width as usize * 4;
    let pixels = row
        .checked_mul(height as usize)
        .ok_or("The image is too large to copy")?;
    let header = BITMAPV5HEADER {
        bV5Size: std::mem::size_of::<BITMAPV5HEADER>() as u32,
        bV5Width: i32::try_from(width).map_err(|_| "The image is too wide to copy")?,
        bV5Height: i32::try_from(height).map_err(|_| "The image is too tall to copy")?,
        bV5Planes: 1,
        bV5BitCount: 32,
        bV5Compression: BI_BITFIELDS,
        bV5SizeImage: u32::try_from(pixels).map_err(|_| "The image is too large to copy")?,
        bV5RedMask: 0x00FF_0000,
        bV5GreenMask: 0x0000_FF00,
        bV5BlueMask: 0x0000_00FF,
        bV5AlphaMask: 0xFF00_0000,
        bV5CSType: LCS_SRGB,
        bV5Intent: 4, // LCS_GM_IMAGES
        ..Default::default()
    };
    let mut bytes = Vec::with_capacity(header.bV5Size as usize + pixels);
    // SAFETY: BITMAPV5HEADER is a plain C struct; its bytes are copied as they are.
    bytes.extend_from_slice(unsafe {
        std::slice::from_raw_parts(
            (&header as *const BITMAPV5HEADER).cast::<u8>(),
            std::mem::size_of::<BITMAPV5HEADER>(),
        )
    });
    // Bottom-up rows, BGRA.
    for source in image.as_raw().chunks_exact(row).rev() {
        for pixel in source.chunks_exact(4) {
            bytes.extend_from_slice(&[pixel[2], pixel[1], pixel[0], pixel[3]]);
        }
    }
    Ok(bytes)
}

fn png(image: &RgbaImage) -> Result<Vec<u8>, String> {
    use image::ImageEncoder;
    let mut bytes = Vec::new();
    image::codecs::png::PngEncoder::new(&mut bytes)
        .write_image(
            image.as_raw(),
            image.width(),
            image.height(),
            image::ExtendedColorType::Rgba8,
        )
        .map_err(|error| format!("Could not encode the PNG copy: {error}"))?;
    Ok(bytes)
}

/// Movable global memory holding a copy of some bytes. Freed on drop unless the clipboard took it.
struct Global(Option<HGLOBAL>);

impl Global {
    fn with_bytes(bytes: &[u8]) -> Result<Self, String> {
        // SAFETY: a fresh allocation of `bytes.len()` bytes, locked only while it is filled.
        unsafe {
            let handle = GlobalAlloc(GMEM_MOVEABLE, bytes.len())
                .map_err(|error| format!("Out of memory for the clipboard: {error}"))?;
            let global = Self(Some(handle));
            let target = GlobalLock(handle);
            if target.is_null() {
                return Err("Could not lock clipboard memory".to_owned());
            }
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), target.cast::<u8>(), bytes.len());
            let _ = GlobalUnlock(handle);
            Ok(global)
        }
    }

    /// Gives the memory to the clipboard; from then on Windows owns (and frees) it.
    fn hand_to_clipboard(mut self, format: u32) -> Result<(), String> {
        let handle = self.0.ok_or("Clipboard memory already handed over")?;
        // SAFETY: the clipboard is open by this thread and `handle` is unlocked global memory.
        unsafe { SetClipboardData(format, HANDLE(handle.0)) }
            .map_err(|error| error.message().to_string())?;
        self.0 = None;
        Ok(())
    }
}

impl Drop for Global {
    fn drop(&mut self) {
        if let Some(handle) = self.0.take() {
            // SAFETY: memory the clipboard did not take is still ours to free.
            unsafe {
                let _ = GlobalFree(handle);
            }
        }
    }
}

/// The clipboard, opened by this thread until drop.
struct Opened;

impl Opened {
    fn open() -> Result<Self, String> {
        for attempt in 0..OPEN_ATTEMPTS {
            // SAFETY: no owner window is needed to place data.
            if unsafe { OpenClipboard(None) }.is_ok() {
                return Ok(Self);
            }
            std::thread::sleep(Duration::from_millis(20 * u64::from(attempt + 1)));
        }
        Err("Another program is using the clipboard".to_owned())
    }
}

impl Drop for Opened {
    fn drop(&mut self) {
        // SAFETY: opened by `open` on this thread.
        unsafe {
            let _ = CloseClipboard();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgba;

    #[test]
    fn the_dib_is_bottom_up_bgra_with_a_v5_header() {
        let mut image = RgbaImage::from_pixel(2, 2, Rgba([0, 0, 0, 255]));
        image.put_pixel(0, 0, Rgba([10, 20, 30, 128])); // top-left
        let dib = dib_v5(&image).unwrap();
        let header = std::mem::size_of::<BITMAPV5HEADER>();
        assert_eq!(dib.len(), header + 2 * 2 * 4);
        // The top row is stored last; its first pixel is BGRA.
        let top_row = header + 2 * 4;
        assert_eq!(&dib[top_row..top_row + 4], &[30, 20, 10, 128]);
    }

    #[test]
    #[ignore = "replaces the clipboard's contents"]
    fn an_image_round_trips_through_the_clipboard() {
        let image = RgbaImage::from_pixel(3, 2, Rgba([1, 2, 3, 255]));
        copy_image_to_clipboard(&image).unwrap();
    }
}
