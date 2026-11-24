use crate::config::Format;
use base64::Engine;
use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::{CompressionType, FilterType, PngEncoder};
use image::{ExtendedColorType, ImageEncoder, RgbaImage};

pub fn crop(
    frame: &RgbaImage,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
) -> Result<RgbaImage, String> {
    if width <= 0 || height <= 0 {
        return Err("Selection is empty".to_owned());
    }

    let right = i64::from(x) + i64::from(width);
    let bottom = i64::from(y) + i64::from(height);
    let left = i64::from(x).clamp(0, i64::from(frame.width()));
    let top = i64::from(y).clamp(0, i64::from(frame.height()));
    let right = right.clamp(0, i64::from(frame.width()));
    let bottom = bottom.clamp(0, i64::from(frame.height()));

    if right <= left || bottom <= top {
        return Err("Selection does not intersect the captured screen".to_owned());
    }

    copy_rectangle(
        frame,
        left as u32,
        top as u32,
        (right - left) as u32,
        (bottom - top) as u32,
    )
}

fn copy_rectangle(
    frame: &RgbaImage,
    x: u32,
    y: u32,
    width: u32,
    height: u32,
) -> Result<RgbaImage, String> {
    let row_bytes = usize::try_from(width)
        .ok()
        .and_then(|value| value.checked_mul(4))
        .ok_or_else(|| "Selection is too large".to_owned())?;
    let output_bytes = row_bytes
        .checked_mul(height as usize)
        .ok_or_else(|| "Selection is too large".to_owned())?;
    let source_stride = frame.width() as usize * 4;
    let mut output = vec![0; output_bytes];

    for row in 0..height as usize {
        let source_start = (y as usize + row) * source_stride + x as usize * 4;
        let destination_start = row * row_bytes;
        output[destination_start..destination_start + row_bytes]
            .copy_from_slice(&frame.as_raw()[source_start..source_start + row_bytes]);
    }

    RgbaImage::from_raw(width, height, output)
        .ok_or_else(|| "Failed to allocate the cropped image".to_owned())
}

pub fn crop_freeform(frame: &RgbaImage, points: &[(i32, i32)]) -> Result<RgbaImage, String> {
    if points.len() < 3 {
        return Err("A freeform selection needs at least three points".to_owned());
    }

    let (polygon_left, polygon_top, polygon_right, polygon_bottom) = points.iter().fold(
        (i64::MAX, i64::MAX, i64::MIN, i64::MIN),
        |(left, top, right, bottom), &(x, y)| {
            let (x, y) = (i64::from(x), i64::from(y));
            (left.min(x), top.min(y), right.max(x), bottom.max(y))
        },
    );

    let left = polygon_left.clamp(0, i64::from(frame.width()));
    let top = polygon_top.clamp(0, i64::from(frame.height()));
    let right = polygon_right.clamp(0, i64::from(frame.width()));
    let bottom = polygon_bottom.clamp(0, i64::from(frame.height()));
    if right <= left || bottom <= top {
        return Err("Freeform selection does not intersect the captured screen".to_owned());
    }

    let width = (right - left) as u32;
    let height = (bottom - top) as u32;
    let byte_count = (width as usize)
        .checked_mul(height as usize)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| "Freeform selection is too large".to_owned())?;
    let mut output = vec![0; byte_count];
    let source_stride = frame.width() as usize * 4;
    let output_stride = width as usize * 4;
    let mut intersections = Vec::with_capacity(points.len());

    for output_y in 0..height as usize {
        intersections.clear();
        let scan_y = top as f64 + output_y as f64 + 0.5;
        for edge_index in 0..points.len() {
            let first = points[edge_index];
            let second = points[(edge_index + 1) % points.len()];
            let first_y = f64::from(first.1);
            let second_y = f64::from(second.1);
            if (first_y <= scan_y && second_y > scan_y) || (second_y <= scan_y && first_y > scan_y)
            {
                let ratio = (scan_y - first_y) / (second_y - first_y);
                intersections
                    .push(f64::from(first.0) + ratio * (f64::from(second.0) - f64::from(first.0)));
            }
        }
        intersections.sort_by(f64::total_cmp);

        for pair in intersections.chunks_exact(2) {
            let span_left = (pair[0] - 0.5).ceil() as i64;
            let span_right = (pair[1] - 0.5).ceil() as i64;
            let span_left = span_left.clamp(left, right);
            let span_right = span_right.clamp(left, right);
            if span_right <= span_left {
                continue;
            }

            let length = (span_right - span_left) as usize * 4;
            let source_start = (top as usize + output_y) * source_stride + span_left as usize * 4;
            let destination_start = output_y * output_stride + (span_left - left) as usize * 4;
            output[destination_start..destination_start + length]
                .copy_from_slice(&frame.as_raw()[source_start..source_start + length]);
        }
    }

    RgbaImage::from_raw(width, height, output)
        .ok_or_else(|| "Failed to allocate the freeform image".to_owned())
}

pub fn thumbnail_data_url(image: &RgbaImage) -> String {
    let encoded = if image.width() == 0 || image.height() == 0 {
        Err("Cannot create a thumbnail from an empty image".to_owned())
    } else if image.width() <= 320 && image.height() <= 200 {
        encode_png(image)
    } else {
        let (width, height) = fitted_dimensions(image.width(), image.height(), 320, 200);
        encode_png(&image::imageops::thumbnail(image, width, height))
    };
    match encoded {
        Ok(bytes) => format!(
            "data:image/png;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(bytes)
        ),
        Err(error) => {
            log::warn!("Failed to encode capture thumbnail: {error}");
            "data:image/png;base64,".to_owned()
        }
    }
}

pub fn encode(image: &RgbaImage, format: Format, quality: u8) -> Result<Vec<u8>, String> {
    match format {
        Format::Png => encode_png(image),
        Format::Jpeg => {
            // JPEG has no alpha. Free-form snips leave the area outside the lasso transparent, so
            // composite onto white instead of letting those pixels turn black.
            let mut rgb = Vec::with_capacity(image.width() as usize * image.height() as usize * 3);
            for pixel in image.as_raw().chunks_exact(4) {
                let alpha = u16::from(pixel[3]);
                for &channel in &pixel[..3] {
                    let blended = (u16::from(channel) * alpha + 255 * (255 - alpha) + 127) / 255;
                    rgb.push(blended as u8);
                }
            }
            let mut bytes = Vec::new();
            JpegEncoder::new_with_quality(&mut bytes, quality.clamp(1, 100))
                .encode(&rgb, image.width(), image.height(), ExtendedColorType::Rgb8)
                .map_err(|error| format!("Failed to encode JPEG: {error}"))?;
            Ok(bytes)
        }
    }
}

fn fitted_dimensions(width: u32, height: u32, max_width: u32, max_height: u32) -> (u32, u32) {
    if u64::from(width) * u64::from(max_height) > u64::from(height) * u64::from(max_width) {
        let height = (u64::from(height) * u64::from(max_width) / u64::from(width)).max(1);
        (max_width, height as u32)
    } else {
        let width = (u64::from(width) * u64::from(max_height) / u64::from(height)).max(1);
        (width as u32, max_height)
    }
}

fn encode_png(image: &RgbaImage) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    PngEncoder::new_with_quality(&mut bytes, CompressionType::Fast, FilterType::Adaptive)
        .write_image(
            image.as_raw(),
            image.width(),
            image.height(),
            ExtendedColorType::Rgba8,
        )
        .map_err(|error| format!("Failed to encode PNG: {error}"))?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgba;

    #[test]
    fn crop_clamps_to_frame() {
        let image = RgbaImage::from_pixel(10, 10, Rgba([1, 2, 3, 4]));
        let cropped = crop(&image, -2, -3, 5, 6).unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(cropped.dimensions(), (3, 3));
    }

    #[test]
    fn freeform_outside_frame_is_an_error() {
        let image = RgbaImage::new(10, 10);
        let result = crop_freeform(&image, &[(-10, -10), (-5, -10), (-5, -5)]);
        assert!(result.is_err());
    }

    #[test]
    fn freeform_scanlines_leave_outside_pixels_transparent() {
        let image = RgbaImage::from_pixel(10, 10, Rgba([10, 20, 30, 255]));
        let cropped = crop_freeform(&image, &[(0, 0), (10, 0), (5, 10)])
            .unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(cropped.dimensions(), (10, 10));
        assert_eq!(cropped.get_pixel(5, 1).0, [10, 20, 30, 255]);
        assert_eq!(cropped.get_pixel(0, 9).0, [0, 0, 0, 0]);
    }

    #[test]
    fn jpeg_flattens_transparent_pixels_onto_white() {
        let mut image = RgbaImage::from_pixel(8, 8, Rgba([0, 0, 0, 0]));
        image.put_pixel(0, 0, Rgba([0, 0, 0, 255]));
        let bytes = encode(&image, Format::Jpeg, 100).unwrap_or_else(|error| panic!("{error}"));
        let decoded = image::load_from_memory(&bytes)
            .unwrap_or_else(|error| panic!("{error}"))
            .into_rgb8();
        assert!(decoded
            .get_pixel(7, 7)
            .0
            .iter()
            .all(|&channel| channel > 240));
    }

    #[test]
    fn fitted_thumbnail_dimensions_preserve_aspect_ratio() {
        assert_eq!(fitted_dimensions(640, 200, 320, 200), (320, 100));
        assert_eq!(fitted_dimensions(100, 400, 320, 200), (50, 200));
    }
}
