//! PDF files read by the `read` tool: the text of a run of pages, or one
//! page shown as an image with its text. Design and sources:
//! docs/evidence/pdf-reading.md.

use std::path::Path;

use crate::conversation::Image;

/// Pages one call shows at most, as Claude Code's read tool.
const PAGES: usize = 20;
/// Characters of text one call shows at most; a page that alone is longer
/// is cut.
const CHARS: usize = 20_000;
/// Fewer non-blank characters than this, and a page is taken to have no
/// text layer (a scan or a figure), so the model is pointed to its image.
const LITTLE_TEXT: usize = 20;

pub(crate) fn is_pdf(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("pdf"))
}

fn open(path: &Path) -> Result<pdf_extract::Document, String> {
    let document = pdf_extract::Document::load(path)
        .map_err(|error| format!("cannot read the PDF {}: {error}", path.display()))?;
    unlock(document, &path.display().to_string())
}

fn unlock(
    mut document: pdf_extract::Document,
    name: &str,
) -> Result<pdf_extract::Document, String> {
    if document.is_encrypted() {
        document
            .decrypt("")
            .map_err(|_| format!("{name} is protected by a password and cannot be read"))?;
    }
    Ok(document)
}

fn page_text(document: &pdf_extract::Document, number: u32) -> String {
    let mut text = String::new();
    let mut output = pdf_extract::PlainTextOutput::new(&mut text);
    match pdf_extract::output_doc_page(document, &mut output, number) {
        Ok(()) => text,
        Err(error) => format!("(the text of this page cannot be read: {error})"),
    }
}

fn little_text(text: &str) -> bool {
    text.chars().filter(|c| !c.is_whitespace()).count() < LITTLE_TEXT
}

/// The text of the pages from `from` on, as many as fit in one call.
pub(crate) fn text(path: &Path, from: usize) -> Result<String, String> {
    let (mut answer, next) = pages(&open(path)?, &path.display().to_string(), from)?;
    if let Some(next) = next {
        answer.push_str(&format!("… continue with from={next}"));
    }
    Ok(answer.trim_end().to_string())
}

/// The text of the first pages of the PDF in `data`, as many as fit in one
/// call, and the first page not shown, if any.
pub(crate) fn data_text(data: &[u8], name: &str) -> Result<(String, Option<usize>), String> {
    let document = pdf_extract::Document::load_mem(data)
        .map_err(|error| format!("cannot read the PDF {name}: {error}"))?;
    let (answer, next) = pages(&unlock(document, name)?, name, 1)?;
    Ok((answer.trim_end().to_string(), next))
}

/// The text of the pages of `document` (called `name`) from `from` on, as
/// many as fit in one call, and the first page not shown, if any.
fn pages(
    document: &pdf_extract::Document,
    name: &str,
    from: usize,
) -> Result<(String, Option<usize>), String> {
    let total = document.get_pages().len();
    if total == 0 {
        return Ok((format!("{name} has no pages."), None));
    }
    if from > total {
        return Err(format!(
            "{name} has {total} pages; page {from} is past the end"
        ));
    }
    let mut shown = String::new();
    let mut last = from - 1;
    let mut scans = Vec::new();
    for number in from..=total.min(from + PAGES - 1) {
        let text = page_text(document, number as u32);
        let text = text.trim();
        let room = CHARS.saturating_sub(shown.chars().count());
        if number > from && text.chars().count() > room {
            break;
        }
        shown.push_str(&format!("\n--- page {number} ---\n"));
        if little_text(text) {
            scans.push(number.to_string());
            shown.push_str("(almost no text: a scan or a figure)\n");
        } else if text.chars().count() > room {
            let cut: String = text.chars().take(room).collect();
            shown.push_str(&cut);
            shown.push_str("\n… the page is cut here; read it with `page` to see all of it\n");
        } else {
            shown.push_str(text);
            shown.push('\n');
        }
        last = number;
    }
    let mut answer = format!("{name} pages {from}-{last} of {total}:{shown}");
    if !scans.is_empty() {
        answer.push_str(&format!(
            "Pages {} have almost no text; read one with `page` to see it as an image.\n",
            scans.join(", ")
        ));
    }
    Ok((answer, (last < total).then_some(last + 1)))
}

/// Page `number` drawn as an image, saved in `images`, with its text.
pub(crate) fn page(
    path: &Path,
    number: usize,
    region: Option<[u32; 4]>,
    images: &Path,
    source: &str,
) -> Result<(String, Image), String> {
    let data =
        std::fs::read(path).map_err(|error| format!("cannot open {}: {error}", path.display()))?;
    let pdf = hayro::hayro_syntax::Pdf::new(data)
        .map_err(|error| format!("cannot read the PDF {}: {error:?}", path.display()))?;
    let pages = pdf.pages();
    let total = pages.len();
    let page = number
        .checked_sub(1)
        .and_then(|index| pages.get(index))
        .ok_or_else(|| {
            format!(
                "{} has {total} pages; there is no page {number}",
                path.display()
            )
        })?;
    let (width, height) = page.render_dimensions();
    // Drawn so that its long edge is what the model is shown, unless a
    // region is asked for, which is then cut from a sharper drawing.
    let long_edge = if region.is_some() { 2.0 } else { 1.0 } * crate::picture::LONG_EDGE as f32;
    let scale = long_edge / width.max(height).max(1.0);
    let drawn = hayro::render(
        page,
        &hayro::RenderCache::new(),
        &hayro::hayro_interpret::InterpreterSettings::default(),
        &hayro::RenderSettings::default(),
        &hayro::PixmapSettings {
            x_scale: scale,
            y_scale: scale,
            bg_color: hayro::vello_cpu::color::palette::css::WHITE,
        },
    );
    let png = drawn
        .into_png()
        .map_err(|error| format!("cannot draw page {number}: {error}"))?;
    let picture = image::load_from_memory_with_format(&png, image::ImageFormat::Png)
        .map_err(|error| format!("cannot draw page {number}: {error}"))?;
    let (image, line) = crate::picture::prepare(
        &picture,
        image::ImageFormat::Png,
        region,
        images,
        &format!("{source} page {number} of {total}"),
    )?;
    let text = open(path).map_or_else(
        |error| format!("({error})"),
        |document| page_text(&document, number as u32).trim().to_string(),
    );
    let text = if little_text(&text) {
        "The page has almost no text of its own.".to_string()
    } else {
        format!("Its text:\n{text}")
    };
    Ok((format!("{line}\n{text}"), image))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use pdf_extract::{dictionary, Object, Stream};

    /// A PDF with a page of text, a page with only a black square, and a
    /// third page of text.
    pub(crate) fn sample(path: &Path) {
        let mut document = pdf_extract::Document::with_version("1.5");
        let pages_id = document.new_object_id();
        let font_id = document.add_object(dictionary! {
            "Type" => "Font",
            "Subtype" => "Type1",
            "BaseFont" => "Helvetica",
        });
        let resources_id = document.add_object(dictionary! {
            "Font" => dictionary! { "F1" => font_id },
        });
        let contents = [
            "BT /F1 24 Tf 72 700 Td (Hello from the first page) Tj ET".to_string(),
            "0 0 0 rg 100 100 400 400 re f".to_string(),
            "BT /F1 24 Tf 72 700 Td (The third page says goodbye) Tj ET".to_string(),
        ];
        let mut kids = Vec::new();
        for content in contents {
            let content_id = document.add_object(Stream::new(dictionary! {}, content.into_bytes()));
            let page_id = document.add_object(dictionary! {
                "Type" => "Page",
                "Parent" => pages_id,
                "Contents" => content_id,
                "Resources" => resources_id,
                "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
            });
            kids.push(Object::from(page_id));
        }
        document.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Count" => kids.len() as i64,
                "Kids" => kids,
            }),
        );
        let catalog_id = document.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        });
        document.trailer.set("Root", catalog_id);
        document.save(path).expect("save");
    }

    #[test]
    fn the_text_is_read_by_page_and_scans_are_pointed_to() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("a.pdf");
        sample(&path);
        let all = text(&path, 1).expect("read");
        assert!(
            all.starts_with(&format!("{} pages 1-3 of 3:", path.display())),
            "{all}"
        );
        assert!(
            all.contains("--- page 1 ---\nHello from the first page"),
            "{all}"
        );
        assert!(all.contains("--- page 2 ---\n(almost no text"), "{all}");
        assert!(all.contains("Pages 2 have almost no text"), "{all}");
        assert!(!all.contains("continue with"), "{all}");
        let last = text(&path, 3).expect("read");
        assert!(last.contains("pages 3-3 of 3"), "{last}");
        assert!(last.contains("goodbye"), "{last}");
        assert!(text(&path, 4).unwrap_err().contains("past the end"));
    }

    #[test]
    fn a_page_is_drawn_with_its_text() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("a.pdf");
        sample(&path);
        let images = dir.path().join("images");
        let (line, image) = page(&path, 2, None, &images, "a.pdf").expect("draw");
        assert!(line.starts_with("Image a.pdf page 2 of 3:"), "{line}");
        assert!(line.contains("almost no text"), "{line}");
        assert_eq!(image.width.max(image.height), crate::picture::LONG_EDGE);
        // The black square is drawn in the middle of the page.
        let drawn = image::open(&image.path).expect("open").to_rgb8();
        let middle = drawn.get_pixel(image.width / 2, image.height * 6 / 10);
        assert!(middle.0.iter().all(|&value| value < 60), "{middle:?}");
        let corner = drawn.get_pixel(2, 2);
        assert!(corner.0.iter().all(|&value| value > 200), "{corner:?}");

        let (line, _) = page(&path, 1, None, &images, "a.pdf").expect("draw");
        assert!(
            line.contains("Its text:\nHello from the first page"),
            "{line}"
        );
        assert!(page(&path, 9, None, &images, "a.pdf")
            .unwrap_err()
            .contains("there is no page 9"));
    }
}
