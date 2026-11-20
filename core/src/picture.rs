//! Images shown to the model: read from a file or taken by the camera, cut
//! to a region when asked, made to fit what the providers take, and kept as
//! files beside the store. Design and sources: docs/evidence/image-input.md.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use image::{DynamicImage, GenericImageView, ImageFormat};

use crate::conversation::Image;

/// The longest edge an image is shown at: Anthropic's standard tier, which
/// the other providers take without rejecting it.
pub(crate) const LONG_EDGE: u32 = 1_568;
/// Extensions read as images.
pub(crate) const EXTENSIONS: [&str; 5] = ["png", "jpg", "jpeg", "gif", "webp"];

/// Whether `path` names an image file by its extension.
pub(crate) fn is_image(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| EXTENSIONS.contains(&extension.to_ascii_lowercase().as_str()))
}

/// Opens the image at `path`.
pub(crate) fn open(path: &Path) -> Result<(DynamicImage, ImageFormat), String> {
    let reader = image::ImageReader::open(path)
        .map_err(|error| format!("cannot open {}: {error}", path.display()))?
        .with_guessed_format()
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    let format = reader
        .format()
        .ok_or_else(|| format!("{} is not an image this tool reads", path.display()))?;
    let picture = reader
        .decode()
        .map_err(|error| format!("cannot read the image {}: {error}", path.display()))?;
    Ok((picture, format))
}

/// Cuts `picture` to `region` (`[x0, y0, x1, y1]` in its pixels) when given,
/// shrinks it to `LONG_EDGE`, and saves it in `folder`; the image to show,
/// and a line that says what is shown.
pub(crate) fn prepare(
    picture: &DynamicImage,
    format: ImageFormat,
    region: Option<[u32; 4]>,
    folder: &Path,
    source: &str,
) -> Result<(Image, String), String> {
    let (width, height) = picture.dimensions();
    let cut = match region {
        Some([x0, y0, x1, y1]) => {
            if x0 >= x1 || y0 >= y1 || x1 > width || y1 > height {
                return Err(format!(
                    "region [{x0}, {y0}, {x1}, {y1}] is not inside the image of \
                     {width}×{height} pixels"
                ));
            }
            picture.crop_imm(x0, y0, x1 - x0, y1 - y0)
        }
        None => picture.clone(),
    };
    let (cut_width, cut_height) = cut.dimensions();
    let shown = if cut_width.max(cut_height) > LONG_EDGE {
        cut.resize(LONG_EDGE, LONG_EDGE, image::imageops::FilterType::Lanczos3)
    } else {
        cut
    };
    let (shown_width, shown_height) = shown.dimensions();
    // Photos stay JPEG; everything else is kept lossless, since lossy
    // compression can make text unreadable.
    let (format, extension, media_type) = match format {
        ImageFormat::Jpeg => (ImageFormat::Jpeg, "jpg", "image/jpeg"),
        _ => (ImageFormat::Png, "png", "image/png"),
    };
    std::fs::create_dir_all(folder)
        .map_err(|error| format!("cannot create {}: {error}", folder.display()))?;
    let path = unique(folder, extension);
    let shown = match format {
        ImageFormat::Jpeg => DynamicImage::ImageRgb8(shown.to_rgb8()),
        _ => shown,
    };
    shown
        .save_with_format(&path, format)
        .map_err(|error| format!("cannot save {}: {error}", path.display()))?;
    let part = match region {
        Some([x0, y0, x1, y1]) => format!(" region [{x0}, {y0}, {x1}, {y1}]"),
        None => String::new(),
    };
    let line = format!(
        "Image {source}: {width}×{height} pixels;{part} shown at {shown_width}×{shown_height}."
    );
    Ok((
        Image {
            path: path.to_string_lossy().into_owned(),
            media_type: media_type.into(),
            width: shown_width,
            height: shown_height,
        },
        line,
    ))
}

/// A new file name in `folder`: the time and a counter, so two images of
/// the same moment do not overwrite each other.
fn unique(folder: &Path, extension: &str) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |now| now.as_millis());
    let count = NEXT.fetch_add(1, Ordering::Relaxed);
    folder.join(format!("{millis}-{count}.{extension}"))
}

/// Tokens an image costs, as Anthropic counts them: one per 28×28 block.
/// Used only to estimate the size of a request.
pub(crate) fn estimated_tokens(image: &Image) -> u64 {
    u64::from(image.width.div_ceil(28)) * u64::from(image.height.div_ceil(28))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn large_images_are_shrunk_and_regions_cut() {
        let dir = tempfile::tempdir().expect("dir");
        let picture = DynamicImage::new_rgb8(3_000, 1_000);
        let (shown, line) =
            prepare(&picture, ImageFormat::Png, None, dir.path(), "a.png").expect("shown");
        assert_eq!((shown.width, shown.height), (1_568, 523));
        assert_eq!(shown.media_type, "image/png");
        assert!(Path::new(&shown.path).exists());
        assert_eq!(line, "Image a.png: 3000×1000 pixels; shown at 1568×523.");

        let (cut, line) = prepare(
            &picture,
            ImageFormat::Jpeg,
            Some([100, 100, 500, 300]),
            dir.path(),
            "b.jpg",
        )
        .expect("cut");
        assert_eq!((cut.width, cut.height), (400, 200));
        assert_eq!(cut.media_type, "image/jpeg");
        assert!(line.contains("region [100, 100, 500, 300] shown at 400×200"));
        assert_ne!(shown.path, cut.path);

        let outside = prepare(
            &picture,
            ImageFormat::Png,
            Some([0, 0, 4_000, 10]),
            dir.path(),
            "c",
        )
        .unwrap_err();
        assert!(
            outside.contains("not inside the image of 3000×1000"),
            "{outside}"
        );
        assert_eq!(estimated_tokens(&shown), 56 * 19);
    }

    #[test]
    fn images_are_known_by_their_extension() {
        assert!(is_image(Path::new("a/b.PNG")));
        assert!(is_image(Path::new("photo.jpeg")));
        assert!(!is_image(Path::new("notes.txt")));
        assert!(!is_image(Path::new("png")));
    }
}
