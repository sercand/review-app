//! Image encoding (decode → downscale → re-encode) and video blobs.

use std::io::Cursor;

use base64::Engine as _;
use image::{
    DynamicImage, ImageFormat as Codec, ImageReader, codecs::jpeg::JpegEncoder,
    imageops::FilterType,
};
use serde::{Deserialize, Serialize};

use crate::{Error, Image, Result};

/// How [`Image::encode`] sizes and encodes a screenshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageOptions {
    /// Longest edge, in pixels.
    pub max_side: Option<u32>,
    /// Width × height.
    pub max_pixels: Option<u64>,
    pub format: ImageFormat,
}

impl Default for ImageOptions {
    fn default() -> Self {
        Self {
            max_side: None,
            max_pixels: None,
            format: ImageFormat::Jpeg { quality: 80 },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageFormat {
    Jpeg { quality: u8 },
    Png,
}

/// The size that fits both limits, keeping the aspect ratio and never upscaling.
fn fit(w: u32, h: u32, max_side: Option<u32>, max_pixels: Option<u64>) -> (u32, u32) {
    let mut scale = 1.0_f64;
    if let Some(m) = max_side {
        let longest = w.max(h);
        if longest > m {
            scale = scale.min(f64::from(m) / f64::from(longest));
        }
    }
    if let Some(m) = max_pixels {
        let px = u64::from(w) * u64::from(h);
        if px > m {
            #[allow(clippy::cast_precision_loss)]
            let s = (m as f64 / px as f64).sqrt();
            scale = scale.min(s);
        }
    }
    if scale >= 1.0 {
        return (w, h);
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let dim = |d: u32| ((f64::from(d) * scale).floor() as u32).max(1);
    (dim(w), dim(h))
}

fn decode(bytes: &[u8]) -> Result<DynamicImage> {
    ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| Error::InvalidRequest(format!("image: {e}")))?
        .decode()
        .map_err(|e| Error::InvalidRequest(format!("image: {e}")))
}

impl Image {
    /// Decodes PNG or JPEG bytes, scales them down to fit `opts`, and re-encodes them.
    pub fn encode(bytes: &[u8], opts: &ImageOptions) -> Result<Image> {
        Image::encode_decoded(decode(bytes)?, opts)
    }

    /// Scales an image the caller already decoded (e.g. with its own `image::Limits`) to
    /// fit `opts`, and encodes it, as [`Image::encode`] does after decoding.
    pub fn encode_decoded(img: DynamicImage, opts: &ImageOptions) -> Result<Image> {
        let (w, h) = fit(img.width(), img.height(), opts.max_side, opts.max_pixels);
        let img = if (w, h) == (img.width(), img.height()) {
            img
        } else {
            img.resize_exact(w, h, FilterType::Lanczos3)
        };
        let mut out = Vec::new();
        let media_type = match opts.format {
            ImageFormat::Jpeg { quality } => {
                // JPEG has no alpha channel.
                let rgb = DynamicImage::ImageRgb8(img.to_rgb8());
                rgb.write_with_encoder(JpegEncoder::new_with_quality(
                    &mut out,
                    quality.clamp(1, 100),
                ))
                .map_err(|e| Error::InvalidRequest(format!("image encode: {e}")))?;
                "image/jpeg"
            }
            ImageFormat::Png => {
                img.write_to(&mut Cursor::new(&mut out), Codec::Png)
                    .map_err(|e| Error::InvalidRequest(format!("image encode: {e}")))?;
                "image/png"
            }
        };
        Ok(Image::from_bytes(media_type, &out))
    }

    /// Width and height, read from the image header.
    pub fn dimensions(&self) -> Result<(u32, u32)> {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&self.data)
            .map_err(|e| Error::InvalidRequest(format!("image base64: {e}")))?;
        ImageReader::new(Cursor::new(bytes))
            .with_guessed_format()
            .map_err(|e| Error::InvalidRequest(format!("image: {e}")))?
            .into_dimensions()
            .map_err(|e| Error::InvalidRequest(format!("image: {e}")))
    }
}

/// An inline video clip, base64-encoded. A blob only: no decoding or frame extraction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Video {
    pub media_type: String,
    pub data: String,
}

impl Video {
    pub fn from_bytes(media_type: impl Into<String>, bytes: &[u8]) -> Self {
        Self {
            media_type: media_type.into(),
            data: base64::engine::general_purpose::STANDARD.encode(bytes),
        }
    }

    /// `data:<media_type>;base64,<data>`
    pub fn data_url(&self) -> String {
        format!("data:{};base64,{}", self.media_type, self.data)
    }
}

#[cfg(test)]
mod tests {
    use image::{Rgba, RgbaImage};

    use super::*;

    fn png(w: u32, h: u32) -> Vec<u8> {
        let img = RgbaImage::from_pixel(w, h, Rgba([255, 0, 0, 128]));
        let mut out = Vec::new();
        DynamicImage::ImageRgba8(img)
            .write_to(&mut Cursor::new(&mut out), Codec::Png)
            .unwrap();
        out
    }

    #[test]
    fn fit_rules() {
        assert_eq!(fit(2000, 1000, Some(1000), None), (1000, 500));
        assert_eq!(fit(100, 50, Some(1000), None), (100, 50), "never upscales");
        let (w, h) = fit(2000, 1000, None, Some(250_000));
        assert!(u64::from(w) * u64::from(h) <= 250_000);
        assert_eq!(w / h, 2, "keeps the aspect ratio");
        assert_eq!(
            fit(2000, 1000, Some(1500), Some(250_000)),
            fit(2000, 1000, None, Some(250_000))
        );
    }

    #[test]
    fn encode_downscales_to_jpeg() {
        let opts = ImageOptions {
            max_side: Some(1000),
            ..ImageOptions::default()
        };
        let img = Image::encode(&png(2000, 1000), &opts).unwrap();
        assert_eq!(img.media_type, "image/jpeg");
        assert_eq!(img.dimensions().unwrap(), (1000, 500));
    }

    #[test]
    fn encode_png_and_small_images() {
        let opts = ImageOptions {
            max_side: Some(1000),
            max_pixels: None,
            format: ImageFormat::Png,
        };
        let img = Image::encode(&png(40, 30), &opts).unwrap();
        assert_eq!(img.media_type, "image/png");
        assert_eq!(img.dimensions().unwrap(), (40, 30));
    }

    #[test]
    fn invalid_bytes_are_an_error() {
        assert!(matches!(
            Image::encode(b"not an image", &ImageOptions::default()),
            Err(Error::InvalidRequest(_))
        ));
    }

    #[test]
    fn video_data_url() {
        assert_eq!(
            Video::from_bytes("video/mp4", b"hi").data_url(),
            "data:video/mp4;base64,aGk="
        );
    }
}
