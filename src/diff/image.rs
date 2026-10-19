//! Image diffs (Task 49): which files are images, which blobs to read for
//! each side, the size cap, and the pure layout helpers the viewer draws with.
//!
//! The viewer itself lives in [`crate::ui::widgets::image_diff`]; everything
//! here is UI-independent so it can be tested without a window — including
//! the blob enum the session's LRU cache holds.

use std::sync::Arc;

use crate::github::files::{FileStatus, PrFile};

/// A raster or vector image, by file name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageKind {
    /// A bitmap: `png jpg jpeg gif webp bmp ico`.
    Raster,
    /// A scalable vector: `svg`.
    Svg,
}

/// The image kind implied by a path's extension, case-insensitively.
///
/// `None` for anything else, including a file *named* `png` — a name with no
/// dot has no extension.
pub fn image_kind(path: &str) -> Option<ImageKind> {
    let name = path.rsplit('/').next().unwrap_or(path);
    let (_, extension) = name.rsplit_once('.')?;
    if extension.eq_ignore_ascii_case("svg") {
        Some(ImageKind::Svg)
    } else if matches!(
        extension.to_ascii_lowercase().as_str(),
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "ico"
    ) {
        Some(ImageKind::Raster)
    } else {
        None
    }
}

/// The paths to read a file's base and head image blobs at.
///
/// The base blob is read **at the merge base**, the head blob at the diff's
/// head SHA. Added files have no base side, removed files no head side, and
/// renames and copies read the base at `previous_path`; a rename with no
/// recorded previous path falls back to the head path on both sides.
pub fn image_sources(f: &PrFile) -> (Option<String>, Option<String>) {
    match f.status {
        FileStatus::Added => (None, Some(f.path.clone())),
        FileStatus::Removed => (Some(f.path.clone()), None),
        FileStatus::Renamed | FileStatus::Copied => (
            Some(f.previous_path.clone().unwrap_or_else(|| f.path.clone())),
            Some(f.path.clone()),
        ),
        _ => (Some(f.path.clone()), Some(f.path.clone())),
    }
}

/// The largest image blob this client will fetch; over it the file shows the
/// placeholder with "Open in browser" instead of an image.
pub const MAX_IMAGE_BYTES: u64 = 10 * 1024 * 1024;

/// One side of an image diff, as the session's cache holds it.
#[derive(Clone, Debug)]
pub enum ImageBlob {
    /// This side has no blob (an added file has no base, a removed no head)
    /// or the fetch failed.
    Absent,
    /// The blob is over [`MAX_IMAGE_BYTES`]; it was never buffered.
    TooLarge,
    /// The blob's bytes.
    Bytes(Arc<[u8]>),
}

impl ImageBlob {
    /// The bytes, when this side has them.
    pub fn bytes(&self) -> Option<&Arc<[u8]>> {
        match self {
            ImageBlob::Bytes(bytes) => Some(bytes),
            _ => None,
        }
    }

    /// The blob's size, when it has one.
    pub fn size(&self) -> Option<u64> {
        self.bytes().map(|bytes| bytes.len() as u64)
    }
}

/// How the two sides are drawn against each other.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ImageMode {
    /// Base and head side by side.
    #[default]
    TwoUp,
    /// Both in one rect, the head clipped left of a draggable divider.
    Swipe,
    /// The head drawn over the base at an adjustable opacity.
    Onion,
}

/// The egui texture URI for one side of one file's image diff.
///
/// The URI keys egui's texture cache, so the session's LRU eviction must
/// forget exactly these (with [`egui::Context::forget_image`]). The file's
/// extension rides on the end because egui_extras' loaders pick the decoder
/// from the URI when the bytes carry no usable hint — most importantly the
/// `svg` loader only takes `.svg` URIs.
pub fn image_uri(session: u64, file: u32, side: &str, path: &str) -> String {
    let extension = image_kind(path).map(|kind| match kind {
        ImageKind::Svg => "svg".to_owned(),
        ImageKind::Raster => path
            .rsplit_once('.')
            .map(|(_, ext)| ext.to_ascii_lowercase())
            .unwrap_or_else(|| "png".to_owned()),
    });
    match extension {
        Some(ext) => format!("bytes://{session}/{file}/{side}.{ext}"),
        None => format!("bytes://{session}/{file}/{side}"),
    }
}

/// Scale `img` into `avail`, keeping the aspect ratio and never upscaling.
pub fn fit_size(img: (f32, f32), avail: (f32, f32)) -> (f32, f32) {
    if img.0 <= 0.0 || img.1 <= 0.0 {
        return (0.0, 0.0);
    }
    let scale = (avail.0 / img.0).min(avail.1 / img.1).clamp(0.0, 1.0);
    (img.0 * scale, img.1 * scale)
}

/// The swipe divider's normalized position: anywhere in `0.0..=1.0`.
pub fn swipe_split(t: f32) -> f32 {
    t.clamp(0.0, 1.0)
}

/// The caption's size line: `"1.0 KB → 1.5 KB (+512 B)"`, or a one-sided
/// `"2.0 KB (added)"` / `"2.0 KB (removed)"` when only one side has a blob.
pub fn size_delta_label(base: Option<u64>, head: Option<u64>) -> String {
    match (base, head) {
        (Some(base), Some(head)) => {
            let delta = format!("({})", size_delta(base, head));
            format!("{} → {} {delta}", format_bytes(base), format_bytes(head))
        }
        (None, Some(head)) => format!("{} (added)", format_bytes(head)),
        (Some(base), None) => format!("{} (removed)", format_bytes(base)),
        (None, None) => String::new(),
    }
}

/// The signed change from `base` to `head`, e.g. `+512 B`.
fn size_delta(base: u64, head: u64) -> String {
    if base == head {
        return "unchanged".to_owned();
    }
    if head > base {
        format!("+{}", format_bytes(head - base))
    } else {
        format!("−{}", format_bytes(base - head))
    }
}

/// A byte count as `"512 B"`, `"1.0 KB"`, or `"1.5 MB"`.
fn format_bytes(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    let value = bytes as f64;
    if value < KB {
        format!("{bytes} B")
    } else if value < MB {
        format!("{:.1} KB", value / KB)
    } else {
        format!("{:.1} MB", value / MB)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::files::{FileStatus, PrFile};
    use crate::test_support::file;

    #[test]
    fn detects_images_case_insensitively() {
        assert_eq!(image_kind("assets/Logo.PNG"), Some(ImageKind::Raster));
        assert_eq!(image_kind("icons/x.svg"), Some(ImageKind::Svg));
        assert_eq!(image_kind("data.bin"), None);
        assert_eq!(image_kind("png"), None); // no extension
    }

    #[test]
    fn picks_blobs_per_status() {
        let added = PrFile {
            status: FileStatus::Added,
            ..file("a.png")
        };
        assert_eq!(image_sources(&added), (None, Some("a.png".to_string())));
        let removed = PrFile {
            status: FileStatus::Removed,
            ..file("a.png")
        };
        assert_eq!(image_sources(&removed), (Some("a.png".to_string()), None));
        let renamed = PrFile {
            status: FileStatus::Renamed,
            previous_path: Some("old.png".into()),
            ..file("new.png")
        };
        assert_eq!(
            image_sources(&renamed),
            (Some("old.png".to_string()), Some("new.png".to_string()))
        );
        assert_eq!(
            image_sources(&file("a.png")),
            (Some("a.png".to_string()), Some("a.png".to_string()))
        );
    }

    #[test]
    fn layout_helpers() {
        assert_eq!(fit_size((2000.0, 1000.0), (500.0, 500.0)), (500.0, 250.0));
        assert_eq!(fit_size((100.0, 50.0), (500.0, 500.0)), (100.0, 50.0)); // never upscale
        assert_eq!((swipe_split(1.5), swipe_split(-1.0)), (1.0, 0.0));
        assert_eq!(
            size_delta_label(Some(1024), Some(1536)),
            "1.0 KB → 1.5 KB (+512 B)"
        );
        assert_eq!(size_delta_label(None, Some(2048)), "2.0 KB (added)");
        assert_eq!(size_delta_label(Some(2048), None), "2.0 KB (removed)");
    }

    #[test]
    fn uris_carry_the_side_and_extension() {
        assert_eq!(image_uri(3, 7, "base", "a/b.PNG"), "bytes://3/7/base.png");
        assert_eq!(image_uri(3, 7, "head", "x.svg"), "bytes://3/7/head.svg");
    }
}
