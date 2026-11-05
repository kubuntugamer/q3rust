//! Image decoding for textures.
//!
//! Quake 3's art is overwhelmingly Targa, plus JPEG for the larger textures
//! added by mission packs and mods. The TGA reader lives here because Targa is
//! trivial to read correctly and needs no dependency; it is the format every
//! stock shader names first. JPEG goes through `jpeg-decoder`, because
//! `R_LoadImage` falls back from a missing `.tga` to the `.jpg` beside it and
//! the console's additive stage ships only as `gfx/misc/console02.jpg`.

/// A decoded image, always RGBA8 with the origin at the top left.
#[derive(Debug, PartialEq, Eq)]
pub struct Image {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ImageError {
    Truncated,
    Unsupported(String),
    BadDimensions,
}

impl std::fmt::Display for ImageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ImageError::Truncated => write!(f, "image data ends before the image does"),
            ImageError::Unsupported(what) => write!(f, "unsupported image: {what}"),
            ImageError::BadDimensions => write!(f, "image has zero width or height"),
        }
    }
}

impl std::error::Error for ImageError {}

/// Decodes an uncompressed or RLE Targa file into RGBA8.
pub fn tga(data: &[u8]) -> Result<Image, ImageError> {
    if data.len() < 18 {
        return Err(ImageError::Truncated);
    }

    let id_length = data[0] as usize;
    let color_map_type = data[1];
    let image_type = data[2];
    let width = u16::from_le_bytes([data[12], data[13]]) as u32;
    let height = u16::from_le_bytes([data[14], data[15]]) as u32;
    let depth = data[16];
    let descriptor = data[17];

    if width == 0 || height == 0 {
        return Err(ImageError::BadDimensions);
    }
    if color_map_type != 0 {
        return Err(ImageError::Unsupported(
            "colour-mapped Targa files are not used by the game".into(),
        ));
    }

    // Grayscale types are 8-bit; true-colour types are 24- or 32-bit.
    let rle = match image_type {
        2 => false,
        3 => false,
        10 => true,
        11 => true,
        other => {
            return Err(ImageError::Unsupported(format!(
                "Targa image type {other}"
            )))
        }
    };
    let bytes_per_pixel = match depth {
        8 => 1,
        24 => 3,
        32 => 4,
        other => return Err(ImageError::Unsupported(format!("{other}-bit Targa pixels"))),
    };

    let start = 18 + id_length;
    if start > data.len() {
        return Err(ImageError::Truncated);
    }
    let pixels = &data[start..];
    let pixel_count = (width as usize) * (height as usize);

    // Decode to BGR(A)/gray in source order first; the vertical flip and the
    // channel swap happen once at the end.
    let mut raw = Vec::with_capacity(pixel_count * bytes_per_pixel);
    if rle {
        let mut cursor = 0usize;
        while raw.len() < pixel_count * bytes_per_pixel {
            let packet = *pixels.get(cursor).ok_or(ImageError::Truncated)?;
            cursor += 1;
            let run = (packet & 0x7f) as usize + 1;
            if packet & 0x80 != 0 {
                let pixel = pixels
                    .get(cursor..cursor + bytes_per_pixel)
                    .ok_or(ImageError::Truncated)?;
                cursor += bytes_per_pixel;
                for _ in 0..run {
                    raw.extend_from_slice(pixel);
                }
            } else {
                let span = run * bytes_per_pixel;
                let chunk = pixels
                    .get(cursor..cursor + span)
                    .ok_or(ImageError::Truncated)?;
                raw.extend_from_slice(chunk);
                cursor += span;
            }
        }
        raw.truncate(pixel_count * bytes_per_pixel);
    } else {
        let needed = pixel_count * bytes_per_pixel;
        if pixels.len() < needed {
            return Err(ImageError::Truncated);
        }
        raw.extend_from_slice(&pixels[..needed]);
    }

    let mut rgba = vec![0u8; pixel_count * 4];
    let top_down = descriptor & 0x20 != 0;
    let right_to_left = descriptor & 0x10 != 0;

    for y in 0..height as usize {
        // Targa's default origin is the bottom left.
        let source_row = if top_down { y } else { height as usize - 1 - y };
        for x in 0..width as usize {
            let source_x = if right_to_left { width as usize - 1 - x } else { x };
            let source = (source_row * width as usize + source_x) * bytes_per_pixel;
            let dest = (y * width as usize + x) * 4;
            match bytes_per_pixel {
                1 => {
                    let v = raw[source];
                    rgba[dest] = v;
                    rgba[dest + 1] = v;
                    rgba[dest + 2] = v;
                    rgba[dest + 3] = 255;
                }
                3 => {
                    rgba[dest] = raw[source + 2];
                    rgba[dest + 1] = raw[source + 1];
                    rgba[dest + 2] = raw[source];
                    rgba[dest + 3] = 255;
                }
                _ => {
                    rgba[dest] = raw[source + 2];
                    rgba[dest + 1] = raw[source + 1];
                    rgba[dest + 2] = raw[source];
                    rgba[dest + 3] = raw[source + 3];
                }
            }
        }
    }

    Ok(Image {
        width,
        height,
        rgba,
    })
}

/// Decodes a JPEG file into RGBA8, as `LoadJPG` does.
pub fn jpeg(data: &[u8]) -> Result<Image, ImageError> {
    use jpeg_decoder::PixelFormat;

    let mut decoder = jpeg_decoder::Decoder::new(std::io::Cursor::new(data));
    let pixels = decoder
        .decode()
        .map_err(|e| ImageError::Unsupported(format!("JPEG: {e}")))?;
    let info = decoder.info().ok_or(ImageError::Truncated)?;

    let width = info.width as u32;
    let height = info.height as u32;
    if width == 0 || height == 0 {
        return Err(ImageError::BadDimensions);
    }

    let count = (width as usize) * (height as usize);
    let mut rgba = Vec::with_capacity(count * 4);
    match info.pixel_format {
        PixelFormat::L8 => {
            if pixels.len() < count {
                return Err(ImageError::Truncated);
            }
            for &v in &pixels[..count] {
                rgba.extend_from_slice(&[v, v, v, 255]);
            }
        }
        PixelFormat::RGB24 => {
            if pixels.len() < count * 3 {
                return Err(ImageError::Truncated);
            }
            for pixel in pixels[..count * 3].chunks_exact(3) {
                rgba.extend_from_slice(&[pixel[0], pixel[1], pixel[2], 255]);
            }
        }
        other => {
            return Err(ImageError::Unsupported(format!(
                "JPEG pixel format {other:?}"
            )))
        }
    }

    Ok(Image {
        width,
        height,
        rgba,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds an uncompressed 24-bit Targa with a given pixel layout.
    fn tga_24(width: u16, height: u16, pixels: &[[u8; 3]], top_down: bool) -> Vec<u8> {
        let mut out = vec![0u8; 18];
        out[2] = 2;
        out[12..14].copy_from_slice(&width.to_le_bytes());
        out[14..16].copy_from_slice(&height.to_le_bytes());
        out[16] = 24;
        out[17] = if top_down { 0x20 } else { 0 };
        for p in pixels {
            // Targa stores blue, green, red.
            out.extend_from_slice(&[p[2], p[1], p[0]]);
        }
        out
    }

    #[test]
    fn top_down_pixels_keep_their_order() {
        let data = tga_24(
            2,
            1,
            &[[255, 0, 0], [0, 255, 0]],
            true,
        );
        let image = tga(&data).unwrap();
        assert_eq!((image.width, image.height), (2, 1));
        assert_eq!(&image.rgba[0..4], &[255, 0, 0, 255]);
        assert_eq!(&image.rgba[4..8], &[0, 255, 0, 255]);
    }

    #[test]
    fn bottom_up_rows_are_flipped() {
        // Stored bottom row first: blue, then white on top.
        let data = tga_24(1, 2, &[[0, 0, 255], [255, 255, 255]], false);
        let image = tga(&data).unwrap();
        assert_eq!(&image.rgba[0..4], &[255, 255, 255, 255]);
        assert_eq!(&image.rgba[4..8], &[0, 0, 255, 255]);
    }

    #[test]
    fn rle_packets_expand() {
        let mut data = vec![0u8; 18];
        data[2] = 10;
        data[12..14].copy_from_slice(&4u16.to_le_bytes());
        data[14..16].copy_from_slice(&1u16.to_le_bytes());
        data[16] = 32;
        data[17] = 0x28; // top down, 8 alpha bits
        // A run of four pixels, all red and half transparent.
        data.push(0x83);
        data.extend_from_slice(&[0, 0, 255, 128]);

        let image = tga(&data).unwrap();
        assert_eq!(image.rgba.len(), 16);
        for pixel in image.rgba.chunks(4) {
            assert_eq!(pixel, &[255, 0, 0, 128]);
        }
    }

    #[test]
    fn truncated_file_is_an_error() {
        let data = tga_24(4, 4, &[[0, 0, 0]], true);
        assert_eq!(tga(&data), Err(ImageError::Truncated));
    }

    #[test]
    fn rubbish_is_not_a_jpeg() {
        assert!(matches!(
            jpeg(b"not a jpeg at all"),
            Err(ImageError::Unsupported(_))
        ));
    }
}
