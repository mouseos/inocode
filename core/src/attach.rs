//! What the person sends with a message besides its text: attached files
//! (images, PDFs and text files, as data or as paths on the engine's
//! machine) and project files mentioned with `@`. Design and sources:
//! docs/evidence/input-attachments.md, docs/evidence/file-mentions.md.

use std::io::{BufRead, BufReader, Cursor};
use std::path::{Path, PathBuf};

use base64::Engine;
use image::{DynamicImage, ImageFormat};
use serde_json::Value;

use crate::conversation::{Body, Speaker};
use crate::ErrorInfo;

/// Files one message may carry.
const MAX_ATTACHMENTS: usize = 10;
/// Bytes of one attached file: its data, or the file at its path.
const MAX_ATTACHMENT_BYTES: u64 = 10 * 1024 * 1024;
const MAX_MENTIONS: usize = 20;
/// Tokens of a text file put in the message at most, as Claude Code's read
/// limit; a longer file gives only its head.
const TEXT_TOKENS: u64 = 25_000;
/// Entries of a mentioned folder listed at most.
const LISTED: usize = 1_000;

/// One file sent with a message: its data, or its path on the engine's
/// machine (relative to the thread's project folder, and inside it).
#[derive(Debug, Clone, PartialEq)]
pub enum Attachment {
    Data {
        name: String,
        media_type: Option<String>,
        data: Vec<u8>,
    },
    Path(String),
}

/// The `attachments` and `mentions` of a `turn/start`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Requested {
    pub attachments: Vec<Attachment>,
    pub mentions: Vec<String>,
}

impl Requested {
    /// Reads `attachments` (`[{name, media_type?, data}` in base64 `|
    /// {path}]`) and `mentions` (`[path]`) from `params`; both may be absent.
    pub fn parse(params: Option<&Value>) -> Result<Self, ErrorInfo> {
        let field = |name: &str| params.and_then(|params| params.get(name));
        let list = |name: &str, most: usize| -> Result<Vec<Value>, ErrorInfo> {
            match field(name) {
                None | Some(Value::Null) => Ok(Vec::new()),
                Some(Value::Array(items)) if items.len() <= most => Ok(items.clone()),
                Some(Value::Array(_)) => {
                    Err(invalid(format!("\"{name}\" may hold at most {most} items")))
                }
                Some(_) => Err(invalid(format!("\"{name}\" must be an array"))),
            }
        };
        let mut attachments = Vec::new();
        for (index, item) in list("attachments", MAX_ATTACHMENTS)?.iter().enumerate() {
            attachments.push(attachment(index, item)?);
        }
        let mentions = list("mentions", MAX_MENTIONS)?
            .iter()
            .map(|item| match item.as_str() {
                Some(path) if !path.trim().is_empty() => Ok(path.to_string()),
                _ => Err(invalid(
                    "each of \"mentions\" must be a non-empty path".to_string(),
                )),
            })
            .collect::<Result<_, _>>()?;
        Ok(Self {
            attachments,
            mentions,
        })
    }

    pub fn is_empty(&self) -> bool {
        self.attachments.is_empty() && self.mentions.is_empty()
    }
}

fn attachment(index: usize, item: &Value) -> Result<Attachment, ErrorInfo> {
    let at = format!("attachments[{index}]");
    let text = |name: &str| match item.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) if !text.trim().is_empty() => Ok(Some(text.clone())),
        Some(_) => Err(invalid(format!(
            "\"{name}\" of {at} must be a non-empty string"
        ))),
    };
    if !item.is_object() {
        return Err(invalid(format!("{at} must be an object")));
    }
    match (text("path")?, text("data")?) {
        (Some(path), None) => Ok(Attachment::Path(path)),
        (None, Some(data)) => {
            let name = text("name")?
                .ok_or_else(|| invalid(format!("{at} sent as data needs a \"name\"")))?;
            // Base64 is four characters for three bytes.
            if data.len() as u64 / 4 * 3 > MAX_ATTACHMENT_BYTES + 2 {
                return Err(too_large(&name));
            }
            let data = base64::engine::general_purpose::STANDARD
                .decode(data.trim())
                .map_err(|_| invalid(format!("the data of {name} is not valid base64")))?;
            if data.len() as u64 > MAX_ATTACHMENT_BYTES {
                return Err(too_large(&name));
            }
            Ok(Attachment::Data {
                name,
                media_type: text("media_type")?,
                data,
            })
        }
        _ => Err(invalid(format!(
            "{at} must have either \"path\" or \"data\""
        ))),
    }
}

/// What the files of a message become: text parts, and images to show,
/// which are saved when the turn records the message.
#[derive(Debug, Clone, Default)]
pub struct Attached {
    parts: Vec<String>,
    pictures: Vec<Picture>,
}

#[derive(Debug, Clone)]
struct Picture {
    picture: DynamicImage,
    format: ImageFormat,
    name: String,
    intro: &'static str,
}

impl Attached {
    /// The name of the first image, if any.
    pub(crate) fn first_image(&self) -> Option<&str> {
        self.pictures.first().map(|picture| picture.name.as_str())
    }
}

/// Reads the files of `requested`: mentions first, then attachments, paths
/// against the project `folder`. Every refusal is an `invalid_params`
/// error, made before anything is stored.
pub(crate) async fn gather(
    requested: Requested,
    folder: Option<PathBuf>,
) -> Result<Attached, ErrorInfo> {
    tokio::task::spawn_blocking(move || {
        let mut attached = Attached::default();
        for given in &requested.mentions {
            let path = inside(folder.as_deref(), given)?;
            if path.is_dir() {
                attached.parts.push(listing(given, &path)?);
            } else {
                from_path(given, &path, Intro::Mentioned, &mut attached)?;
            }
        }
        for attachment in requested.attachments {
            match attachment {
                Attachment::Path(given) => {
                    let path = inside(folder.as_deref(), &given)?;
                    let size = std::fs::metadata(&path)
                        .map_err(|error| invalid(format!("cannot read {given}: {error}")))?
                        .len();
                    if path.is_dir() {
                        return Err(invalid(format!(
                            "{given} is a folder; attach its files, or mention it"
                        )));
                    }
                    if size > MAX_ATTACHMENT_BYTES {
                        return Err(too_large(&given));
                    }
                    from_path(&given, &path, Intro::Attached, &mut attached)?;
                }
                Attachment::Data {
                    name,
                    media_type,
                    data,
                } => from_data(name, media_type.as_deref(), &data, &mut attached)?,
            }
        }
        Ok(attached)
    })
    .await
    .map_err(|error| ErrorInfo::new("internal", error.to_string()))?
}

/// The person's message: `text`, then the parts of `attached`, with its
/// images saved in `folder` (the thread's image folder beside the store).
pub(crate) async fn message(
    text: String,
    attached: Attached,
    folder: PathBuf,
) -> Result<Body, ErrorInfo> {
    let Attached {
        parts: added,
        pictures,
    } = attached;
    let mut parts = vec![text];
    parts.extend(added);
    if pictures.is_empty() {
        return Ok(Body::User {
            from: Speaker::Person,
            parts,
            images: Vec::new(),
        });
    }
    let saved = tokio::task::spawn_blocking(move || {
        pictures
            .iter()
            .map(|picture| {
                crate::picture::prepare(
                    &picture.picture,
                    picture.format,
                    None,
                    &folder,
                    &picture.name,
                )
                .map(|(image, line)| (image, format!("{} {line}", picture.intro)))
            })
            .collect::<Result<Vec<_>, String>>()
    })
    .await
    .map_err(|error| ErrorInfo::new("internal", error.to_string()))?
    .map_err(|error| ErrorInfo::new("store", error))?;
    let mut images = Vec::new();
    for (image, line) in saved {
        parts.push(line);
        images.push(image);
    }
    Ok(Body::User {
        from: Speaker::Person,
        parts,
        images,
    })
}

#[derive(Clone, Copy)]
enum Intro {
    Attached,
    Mentioned,
}

impl Intro {
    fn file(self, name: &str) -> String {
        match self {
            Intro::Attached => format!("The person attached {name}:"),
            Intro::Mentioned => format!("The person mentioned {name}:"),
        }
    }

    fn image(self) -> &'static str {
        match self {
            Intro::Attached => "The person attached an image.",
            Intro::Mentioned => "The person mentioned an image.",
        }
    }
}

/// `given` (relative to `folder`, or absolute) as a path that exists inside
/// the project folder.
fn inside(folder: Option<&Path>, given: &str) -> Result<PathBuf, ErrorInfo> {
    let folder = folder.ok_or_else(|| {
        invalid(format!(
            "{given} cannot be read: this thread has no project folder"
        ))
    })?;
    let root = folder.canonicalize().map_err(|error| {
        invalid(format!(
            "the project folder {} cannot be read: {error}",
            folder.display()
        ))
    })?;
    let path = folder
        .join(given)
        .canonicalize()
        .map_err(|_| invalid(format!("{given} is not a file or folder of the project")))?;
    if !path.starts_with(&root) {
        return Err(invalid(format!("{given} is outside the project folder")));
    }
    Ok(path)
}

/// The entries of the folder `path`, as `given`, folders ending in `/`.
fn listing(given: &str, path: &Path) -> Result<String, ErrorInfo> {
    let mut entries: Vec<String> = std::fs::read_dir(path)
        .map_err(|error| invalid(format!("cannot read the folder {given}: {error}")))?
        .flatten()
        .map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            match entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                true => format!("{name}/"),
                false => name,
            }
        })
        .collect();
    entries.sort();
    let total = entries.len();
    entries.truncate(LISTED);
    let mut text = format!(
        "The person mentioned the folder {given}. Its entries ({total}):\n{}",
        entries.join("\n")
    );
    if total > LISTED {
        text.push_str(&format!("\n… and {} more", total - LISTED));
    }
    Ok(text)
}

/// Reads the file at `path`, as the `read` tool does: an image to show, the
/// text of a PDF's first pages, or the head of a text file.
fn from_path(
    given: &str,
    path: &Path,
    intro: Intro,
    attached: &mut Attached,
) -> Result<(), ErrorInfo> {
    if crate::picture::is_image(path) {
        let (picture, format) = crate::picture::open(path).map_err(invalid)?;
        attached.pictures.push(Picture {
            picture,
            format,
            name: given.to_string(),
            intro: intro.image(),
        });
        return Ok(());
    }
    let body = if crate::pdf::is_pdf(path) {
        crate::pdf::text(path, 1).map_err(invalid)?
    } else {
        let file = std::fs::File::open(path)
            .map_err(|error| invalid(format!("cannot open {given}: {error}")))?;
        head(BufReader::new(file), given, Some(given))?
    };
    attached
        .parts
        .push(format!("{}\n{body}", intro.file(given)));
    Ok(())
}

/// Reads a file sent as data: by the extension of its `name`, else by
/// `media_type`, an image, a PDF, or else UTF-8 text.
fn from_data(
    name: String,
    media_type: Option<&str>,
    data: &[u8],
    attached: &mut Attached,
) -> Result<(), ErrorInfo> {
    let path = Path::new(&name);
    let media_type = media_type.unwrap_or_default().to_ascii_lowercase();
    let image = crate::picture::is_image(path)
        || (!crate::pdf::is_pdf(path) && media_type.starts_with("image/"));
    if image {
        let (picture, format) = decode(&name, data)?;
        attached.pictures.push(Picture {
            picture,
            format,
            name,
            intro: Intro::Attached.image(),
        });
        return Ok(());
    }
    let body = if crate::pdf::is_pdf(path) || media_type == "application/pdf" {
        let (mut text, next) = crate::pdf::data_text(data, &name).map_err(invalid)?;
        if let Some(next) = next {
            text.push_str(&format!(
                "\n… the pages from {next} on are not shown: the PDF was sent as data, so \
                 they cannot be read with `read`."
            ));
        }
        text
    } else {
        if data.contains(&0) || std::str::from_utf8(data).is_err() {
            return Err(unsupported(&name));
        }
        head(Cursor::new(data), &name, None)?
    };
    attached
        .parts
        .push(format!("{}\n{body}", Intro::Attached.file(&name)));
    Ok(())
}

/// The image in `data`, if it is one the `read` tool shows.
fn decode(name: &str, data: &[u8]) -> Result<(DynamicImage, ImageFormat), ErrorInfo> {
    let reader = image::ImageReader::new(Cursor::new(data))
        .with_guessed_format()
        .map_err(|error| invalid(format!("cannot read the image {name}: {error}")))?;
    match reader.format() {
        Some(ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::Gif | ImageFormat::WebP) => {}
        _ => return Err(unsupported(name)),
    }
    let format = reader.format().unwrap_or(ImageFormat::Png);
    let picture = reader
        .decode()
        .map_err(|error| invalid(format!("cannot read the image {name}: {error}")))?;
    Ok((picture, format))
}

/// The lines of a text file numbered as `read` numbers them, up to about
/// [`TEXT_TOKENS`] tokens; a longer file is cut with a note saying how to
/// read the rest with `read` when it has a `path`, or that it was cut.
fn head(mut reader: impl BufRead, name: &str, path: Option<&str>) -> Result<String, ErrorInfo> {
    let budget = (TEXT_TOKENS as f64 * crate::compaction::FALLBACK_BYTES_PER_TOKEN) as usize;
    let mut shown = String::new();
    let mut shown_lines = 0;
    let mut total = 0;
    let mut line = Vec::new();
    loop {
        line.clear();
        let read = reader
            .read_until(b'\n', &mut line)
            .map_err(|error| invalid(format!("cannot read {name}: {error}")))?;
        if read == 0 {
            break;
        }
        if line.contains(&0) {
            return Err(unsupported(name));
        }
        total += 1;
        if shown_lines + 1 == total {
            let text = String::from_utf8_lossy(&line);
            let numbered = format!(
                "{total}: {}\n",
                crate::read::cut(text.trim_end_matches(['\n', '\r']))
            );
            if shown.len() + numbered.len() <= budget {
                shown.push_str(&numbered);
                shown_lines = total;
            }
        }
    }
    if total == 0 {
        return Ok(format!("{name} is empty."));
    }
    if shown_lines == total {
        return Ok(shown.trim_end().to_string());
    }
    let rest = match path {
        Some(path) => format!(
            "Read the rest with `read` (path {path}, from {}).",
            shown_lines + 1
        ),
        None => "The file was sent as data, so the rest cannot be read.".to_string(),
    };
    Ok(format!(
        "{shown}… cut here at about {TEXT_TOKENS} tokens: lines 1-{shown_lines} of {total} \
         are shown. {rest}"
    ))
}

fn invalid(message: String) -> ErrorInfo {
    ErrorInfo::new("invalid_params", message)
}

fn too_large(name: &str) -> ErrorInfo {
    invalid(format!(
        "{name} is larger than {} MiB, the most one attachment may be",
        MAX_ATTACHMENT_BYTES / 1024 / 1024
    ))
}

fn unsupported(name: &str) -> ErrorInfo {
    invalid(format!(
        "{name} is not an image (png, jpeg, gif, webp), a PDF or a UTF-8 text file"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parse(params: Value) -> Result<Requested, ErrorInfo> {
        Requested::parse(Some(&params))
    }

    fn data(name: &str, bytes: &[u8]) -> Value {
        json!({"name": name, "data": base64::engine::general_purpose::STANDARD.encode(bytes)})
    }

    #[test]
    fn bad_attachments_are_refused() {
        let refused = |params: Value| parse(params).expect_err("refused").message;
        let many: Vec<Value> = (0..=MAX_ATTACHMENTS).map(|_| data("a.txt", b"a")).collect();
        assert!(refused(json!({"attachments": many})).contains("at most 10"));
        assert!(
            refused(json!({"attachments": [{"name": "a.png", "data": "not base64!"}]}))
                .contains("not valid base64")
        );
        assert!(refused(json!({"attachments": [{"data": "YQ=="}]})).contains("needs a \"name\""));
        assert!(
            refused(json!({"attachments": [{"path": "a", "data": "YQ=="}]}))
                .contains("either \"path\" or \"data\"")
        );
        let big = vec![b'a'; MAX_ATTACHMENT_BYTES as usize + 1];
        assert!(refused(json!({"attachments": [data("big.txt", &big)]})).contains("larger than"));
        assert!(refused(json!({"mentions": [""]})).contains("non-empty path"));
        assert!(parse(json!({})).expect("none").is_empty());
    }

    #[tokio::test]
    async fn a_pdf_and_a_text_file_become_text_parts() {
        let dir = tempfile::tempdir().expect("dir");
        let pdf = dir.path().join("a.pdf");
        crate::pdf::tests::sample(&pdf);
        let bytes = std::fs::read(&pdf).expect("pdf");
        let requested = Requested {
            attachments: vec![
                Attachment::Data {
                    name: "report.pdf".into(),
                    media_type: None,
                    data: bytes,
                },
                Attachment::Data {
                    name: "notes".into(),
                    media_type: Some("text/plain".into()),
                    data: b"first\nsecond\n".to_vec(),
                },
                Attachment::Path("a.pdf".into()),
            ],
            mentions: Vec::new(),
        };
        let attached = gather(requested, Some(dir.path().to_path_buf()))
            .await
            .expect("read");
        assert!(attached.pictures.is_empty());
        assert!(
            attached.parts[0]
                .starts_with("The person attached report.pdf:\nreport.pdf pages 1-3 of 3:"),
            "{}",
            attached.parts[0]
        );
        assert!(attached.parts[0].contains("Hello from the first page"));
        assert_eq!(
            attached.parts[1],
            "The person attached notes:\n1: first\n2: second"
        );
        assert!(
            attached.parts[2].contains("goodbye"),
            "{}",
            attached.parts[2]
        );

        let binary = Requested {
            attachments: vec![Attachment::Data {
                name: "a.bin".into(),
                media_type: None,
                data: vec![0, 1, 2],
            }],
            mentions: Vec::new(),
        };
        let error = gather(binary, None).await.expect_err("not text");
        assert!(
            error.message.contains("is not an image"),
            "{}",
            error.message
        );
        let bmp = Requested {
            attachments: vec![Attachment::Data {
                name: "a.bmp".into(),
                media_type: Some("image/bmp".into()),
                data: b"BM\0\0".to_vec(),
            }],
            mentions: Vec::new(),
        };
        let error = gather(bmp, None).await.expect_err("not shown");
        assert!(
            error.message.contains("is not an image"),
            "{}",
            error.message
        );
    }

    #[test]
    fn a_long_text_gives_its_head_and_how_to_read_on() {
        let line = "x".repeat(99);
        let text: String = (0..1_000).map(|_| format!("{line}\n")).collect();
        let cut = head(Cursor::new(text.as_bytes()), "big.txt", Some("big.txt")).expect("text");
        assert!(cut.starts_with("1: xxx"));
        // 75,000 bytes: 99 lines of up to 104 bytes, then 616 of 105.
        assert!(cut.contains("\n715: x") && !cut.contains("\n716: "));
        assert!(cut.ends_with(
            "… cut here at about 25000 tokens: lines 1-715 of 1000 are shown. Read the rest \
             with `read` (path big.txt, from 716)."
        ));
        let data = head(Cursor::new(text.as_bytes()), "big.txt", None).expect("text");
        assert!(data.ends_with("The file was sent as data, so the rest cannot be read."));
    }
}
