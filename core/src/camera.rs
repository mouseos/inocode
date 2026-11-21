//! The `camera` tool: lists the cameras (also video capture devices such as
//! a UVC HDMI capture), shows one's formats and controls, takes a photo or
//! records a video, after setting the controls asked for. Photos and frames
//! are shown to the model like images read from files. Design:
//! docs/evidence/image-input.md.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use image::{DynamicImage, ImageFormat, RgbImage};
use nokhwa::pixel_format::RgbFormat;
use nokhwa::utils::{
    ApiBackend, CameraControl, CameraFormat, CameraIndex, ControlValueDescription,
    ControlValueSetter, FrameFormat, KnownCameraControl, RequestedFormat, RequestedFormatType,
    Resolution,
};
use nokhwa::Camera;
use serde::Deserialize;
use serde_json::{json, Map, Value};

use crate::conversation::{Image, ToolSpec};

pub(crate) const CAMERA: &str = "camera";
/// Frames thrown away after the stream opens, so automatic exposure and
/// white balance settle before the photo.
const SETTLE_FRAMES: usize = 5;
const MAX_SECONDS: f64 = 60.0;
/// Frames of a video shown to the model.
const FRAMES_SHOWN: usize = 4;
/// Formats listed per camera at most, the largest first.
const FORMATS_LISTED: usize = 12;

pub(crate) fn spec() -> ToolSpec {
    ToolSpec {
        name: CAMERA.into(),
        description: "Use a camera or video capture device (such as a UVC HDMI capture). \
            list: the devices. controls: one device's formats and controls (focus, \
            exposure, gain, white_balance, zoom and others, with their ranges and current \
            values). photo: take a photo, shown to you; video: record seconds of video to \
            a file, with a few frames shown to you (needs ffmpeg). Both can first set \
            controls by name to a number, true/false, or \"auto\"/\"manual\" for its \
            mode, and pick a resolution [width, height]; photo can show only a region \
            [x0, y0, x1, y1] at full detail. \
            Taking photos and videos waits for the person's approval unless the mode is \
            full."
            .into(),
        parameters: json!({
            "type": "object",
            "required": ["action"],
            "properties": {
                "action": {"type": "string", "enum": ["list", "controls", "photo", "video"]},
                "device": {
                    "description": "The device's index from list, or its name.",
                    "type": ["integer", "string"]
                },
                "controls": {
                    "type": "object",
                    "description": "Control names (from controls) and the values to set.",
                    "additionalProperties": {"type": ["number", "boolean", "string"]}
                },
                "resolution": {
                    "type": "array", "items": {"type": "integer", "minimum": 1},
                    "minItems": 2, "maxItems": 2
                },
                "fps": {"type": "integer", "minimum": 1},
                "seconds": {"type": "number", "minimum": 1, "maximum": MAX_SECONDS},
                "region": {
                    "type": "array", "items": {"type": "integer", "minimum": 0},
                    "minItems": 4, "maxItems": 4
                }
            }
        }),
    }
}

#[derive(Deserialize)]
struct Arguments {
    action: String,
    #[serde(default)]
    device: Option<Value>,
    #[serde(default)]
    controls: Map<String, Value>,
    #[serde(default)]
    resolution: Option<[u32; 2]>,
    #[serde(default)]
    fps: Option<u32>,
    #[serde(default)]
    seconds: Option<f64>,
    #[serde(default)]
    region: Option<[u32; 4]>,
}

/// Whether a call takes pictures, which waits for the person.
pub(crate) fn takes_pictures(arguments: &str) -> bool {
    serde_json::from_str::<Arguments>(arguments)
        .map(|arguments| matches!(arguments.action.as_str(), "photo" | "video"))
        .unwrap_or(true)
}

/// Runs a camera call; images to show go to `shown`, files to `folder`.
pub(crate) async fn run(arguments: &str, folder: &Path) -> Result<(String, Vec<Image>), String> {
    let arguments: Arguments =
        serde_json::from_str(arguments).map_err(|error| format!("invalid arguments: {error}"))?;
    let folder = folder.to_path_buf();
    tokio::task::spawn_blocking(move || match arguments.action.as_str() {
        "list" => list().map(|text| (text, Vec::new())),
        "controls" => describe(&arguments).map(|text| (text, Vec::new())),
        "photo" => photo(&arguments, &folder),
        "video" => video(&arguments, &folder),
        other => Err(format!("unknown action {other}")),
    })
    .await
    .map_err(|error| format!("the camera failed: {error}"))?
}

fn backend() -> ApiBackend {
    nokhwa::native_api_backend().unwrap_or(ApiBackend::Auto)
}

fn list() -> Result<String, String> {
    let devices =
        nokhwa::query(backend()).map_err(|error| format!("cannot list cameras: {error}"))?;
    if devices.is_empty() {
        return Ok("No cameras.".into());
    }
    let lines: Vec<String> = devices
        .iter()
        .map(|device| {
            format!(
                "{}: {} ({})",
                device.index(),
                device.human_name(),
                device.description()
            )
        })
        .collect();
    Ok(lines.join("\n"))
}

/// The device the arguments name: by index, or by (part of) its name.
fn index_of(device: Option<&Value>) -> Result<CameraIndex, String> {
    match device {
        None => Ok(CameraIndex::Index(0)),
        Some(Value::Number(number)) => number
            .as_u64()
            .map(|index| CameraIndex::Index(index as u32))
            .ok_or_else(|| "device must be an index or a name".into()),
        Some(Value::String(name)) => {
            let devices = nokhwa::query(backend())
                .map_err(|error| format!("cannot list cameras: {error}"))?;
            let wanted = name.to_lowercase();
            devices
                .iter()
                .find(|device| device.human_name().to_lowercase().contains(&wanted))
                .map(|device| device.index().clone())
                .ok_or_else(|| format!("no camera named {name}"))
        }
        Some(_) => Err("device must be an index or a name".into()),
    }
}

fn open(arguments: &Arguments) -> Result<Camera, String> {
    open_index(index_of(arguments.device.as_ref())?, arguments)
}

fn open_index(index: CameraIndex, arguments: &Arguments) -> Result<Camera, String> {
    let requested = match (arguments.resolution, arguments.fps) {
        (Some([width, height]), fps) => RequestedFormatType::Closest(CameraFormat::new(
            Resolution::new(width, height),
            FrameFormat::MJPEG,
            fps.unwrap_or(30),
        )),
        (None, Some(fps)) => RequestedFormatType::HighestFrameRate(fps),
        (None, None) => RequestedFormatType::AbsoluteHighestResolution,
    };
    Camera::with_backend(
        index,
        RequestedFormat::new::<RgbFormat>(requested),
        backend(),
    )
    .map_err(|error| format!("cannot open the camera: {error}"))
}

fn describe(arguments: &Arguments) -> Result<String, String> {
    let mut camera = open(arguments)?;
    let mut formats = camera
        .compatible_camera_formats()
        .map_err(|error| format!("cannot read the formats: {error}"))?;
    formats.sort_by_key(|format| {
        std::cmp::Reverse((format.width() * format.height(), format.frame_rate()))
    });
    formats.dedup_by_key(|format| (format.width(), format.height(), format.frame_rate()));
    let formats: Vec<String> = formats
        .iter()
        .take(FORMATS_LISTED)
        .map(|format| {
            format!(
                "{}×{} {} fps {}",
                format.width(),
                format.height(),
                format.frame_rate(),
                format.format()
            )
        })
        .collect();
    let controls = camera
        .camera_controls()
        .map_err(|error| format!("cannot read the controls: {error}"))?;
    let controls: Vec<String> = controls.iter().map(describe_control).collect();
    Ok(format!(
        "{}\nFormats (largest first):\n{}\nControls:\n{}",
        camera.info().human_name(),
        formats.join("\n"),
        controls.join("\n")
    ))
}

/// A control's name, values and modes as the model reads them.
fn describe_control(control: &CameraControl) -> String {
    let values = match control.description() {
        ControlValueDescription::IntegerRange {
            min,
            max,
            value,
            step,
            default,
        } => format!("{value} (from {min} to {max} by {step}, default {default})"),
        ControlValueDescription::Integer { value, default, .. } => {
            format!("{value} (default {default})")
        }
        ControlValueDescription::FloatRange {
            min,
            max,
            value,
            default,
            ..
        } => format!("{value} (from {min} to {max}, default {default})"),
        ControlValueDescription::Boolean { value, default } => {
            format!("{value} (default {default})")
        }
        other => format!("{other}"),
    };
    let modes: Vec<String> = control.flag().iter().map(ToString::to_string).collect();
    format!(
        "- {}: {values}{}",
        control_name(control),
        if modes.is_empty() {
            String::new()
        } else {
            format!(" [{}]", modes.join(", "))
        }
    )
}

/// The name a control is set by: the known ones by a fixed name, the others
/// by the name the device gives.
fn control_name(control: &CameraControl) -> String {
    match control.control() {
        KnownCameraControl::Other(_) => control.name().to_lowercase().replace(' ', "_"),
        known => known_name(known).to_string(),
    }
}

fn known_name(control: KnownCameraControl) -> &'static str {
    match control {
        KnownCameraControl::Brightness => "brightness",
        KnownCameraControl::Contrast => "contrast",
        KnownCameraControl::Hue => "hue",
        KnownCameraControl::Saturation => "saturation",
        KnownCameraControl::Sharpness => "sharpness",
        KnownCameraControl::Gamma => "gamma",
        KnownCameraControl::WhiteBalance => "white_balance",
        KnownCameraControl::BacklightComp => "backlight_compensation",
        KnownCameraControl::Gain => "gain",
        KnownCameraControl::Pan => "pan",
        KnownCameraControl::Tilt => "tilt",
        KnownCameraControl::Zoom => "zoom",
        KnownCameraControl::Exposure => "exposure",
        KnownCameraControl::Iris => "iris",
        KnownCameraControl::Focus => "focus",
        KnownCameraControl::Other(_) => "other",
    }
}

/// Sets the controls asked for; what was set, or the first that failed.
fn set_controls(camera: &mut Camera, wanted: &Map<String, Value>) -> Result<Vec<String>, String> {
    if wanted.is_empty() {
        return Ok(Vec::new());
    }
    let controls = camera
        .camera_controls()
        .map_err(|error| format!("cannot read the controls: {error}"))?;
    let mut set = Vec::new();
    for (name, value) in wanted {
        if value.is_string() {
            // A mode, set before the camera was opened.
            set.push(format!("{name}={value}"));
            continue;
        }
        let wanted_name = name.to_lowercase();
        let control = controls
            .iter()
            .find(|control| control_name(control) == wanted_name)
            .ok_or_else(|| format!("this camera has no control {name}; controls lists them"))?;
        let setter = match (control.description(), value) {
            (ControlValueDescription::Boolean { .. }, Value::Bool(on)) => {
                ControlValueSetter::Boolean(*on)
            }
            (
                ControlValueDescription::FloatRange { .. } | ControlValueDescription::Float { .. },
                Value::Number(number),
            ) => ControlValueSetter::Float(number.as_f64().unwrap_or_default()),
            (_, Value::Number(number)) => ControlValueSetter::Integer(
                number
                    .as_i64()
                    .or_else(|| number.as_f64().map(|value| value.round() as i64))
                    .unwrap_or_default(),
            ),
            (_, Value::Bool(on)) => ControlValueSetter::Integer(i64::from(*on)),
            _ => return Err(format!("{name} takes a number or true/false")),
        };
        camera
            .set_camera_control(control.control(), setter)
            .map_err(|error| format!("cannot set {name} to {value}: {error}"))?;
        set.push(format!("{name}={value}"));
    }
    Ok(set)
}

/// One frame as an image. MJPEG is decoded here, the other formats by the
/// capture library.
fn grab(camera: &mut Camera) -> Result<RgbImage, String> {
    let frame = camera
        .frame()
        .map_err(|error| format!("cannot take a frame: {error}"))?;
    if frame.source_frame_format() == FrameFormat::MJPEG {
        return image::load_from_memory_with_format(frame.buffer(), ImageFormat::Jpeg)
            .map(|picture| picture.to_rgb8())
            .map_err(|error| format!("cannot read the frame: {error}"));
    }
    let decoded = frame
        .decode_image::<RgbFormat>()
        .map_err(|error| format!("cannot read the frame: {error}"))?;
    let (width, height) = (decoded.width(), decoded.height());
    RgbImage::from_raw(width, height, decoded.into_raw())
        .ok_or_else(|| "the frame has the wrong size".into())
}

/// The modes asked for (control, automatic): `"auto"` or `"manual"`, and
/// on Windows the manual mode a value needs to take effect.
fn modes(wanted: &Map<String, Value>) -> Result<Vec<(String, bool)>, String> {
    let mut modes = Vec::new();
    for (name, value) in wanted {
        let name = name.to_lowercase();
        match value {
            Value::String(text) => match text.as_str() {
                "auto" => modes.push((name, true)),
                "manual" => modes.push((name, false)),
                other => {
                    return Err(format!(
                        "{name} takes a number, true/false, auto or manual, not {other}"
                    ))
                }
            },
            _ if cfg!(windows) && has_mode(&name) => modes.push((name, false)),
            _ => {}
        }
    }
    Ok(modes)
}

#[cfg(windows)]
fn has_mode(name: &str) -> bool {
    crate::camera_mode::has_mode(name)
}

#[cfg(not(windows))]
fn has_mode(_name: &str) -> bool {
    false
}

/// Switches the modes asked for, before the camera is opened.
#[cfg(windows)]
fn apply_modes(index: &CameraIndex, modes: &[(String, bool)]) -> Result<(), String> {
    let index = match index {
        CameraIndex::Index(index) => *index,
        CameraIndex::String(_) => return Err("a mode needs the camera's index".into()),
    };
    crate::camera_mode::set_modes(index, modes)
}

#[cfg(not(windows))]
fn apply_modes(_index: &CameraIndex, modes: &[(String, bool)]) -> Result<(), String> {
    match modes.first() {
        None => Ok(()),
        Some((name, _)) => Err(format!(
            "{name}: on this system an automatic mode is a control of its own; set the one \
             controls lists"
        )),
    }
}

fn start(arguments: &Arguments) -> Result<(Camera, Vec<String>), String> {
    let index = index_of(arguments.device.as_ref())?;
    apply_modes(&index, &modes(&arguments.controls)?)?;
    let mut camera = open_index(index, arguments)?;
    let set = set_controls(&mut camera, &arguments.controls)?;
    camera
        .open_stream()
        .map_err(|error| format!("cannot start the camera: {error}"))?;
    Ok((camera, set))
}

fn photo(arguments: &Arguments, folder: &Path) -> Result<(String, Vec<Image>), String> {
    let (mut camera, set) = start(arguments)?;
    for _ in 0..SETTLE_FRAMES {
        grab(&mut camera)?;
    }
    let picture = DynamicImage::ImageRgb8(grab(&mut camera)?);
    let _ = camera.stop_stream();
    let original = save_original(&picture, folder)?;
    let name = camera.info().human_name();
    let (image, line) = crate::picture::prepare(
        &picture,
        ImageFormat::Png,
        arguments.region,
        folder,
        &format!("from {name}"),
    )?;
    Ok((
        format!(
            "{line} Saved at full size as {}.{}",
            original.display(),
            controls_line(&set)
        ),
        vec![image],
    ))
}

fn controls_line(set: &[String]) -> String {
    match set.is_empty() {
        true => String::new(),
        false => format!(" Set: {}.", set.join(", ")),
    }
}

fn save_original(picture: &DynamicImage, folder: &Path) -> Result<PathBuf, String> {
    std::fs::create_dir_all(folder)
        .map_err(|error| format!("cannot create {}: {error}", folder.display()))?;
    let path = folder.join(format!("camera-{}.png", stamp()));
    picture
        .save_with_format(&path, ImageFormat::Png)
        .map_err(|error| format!("cannot save {}: {error}", path.display()))?;
    Ok(path)
}

fn stamp() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |now| now.as_millis())
}

fn video(arguments: &Arguments, folder: &Path) -> Result<(String, Vec<Image>), String> {
    let seconds = arguments.seconds.unwrap_or(5.0).clamp(1.0, MAX_SECONDS);
    let (mut camera, set) = start(arguments)?;
    let first = grab(&mut camera)?;
    let (width, height) = first.dimensions();
    let fps = camera.frame_rate().max(1);
    std::fs::create_dir_all(folder)
        .map_err(|error| format!("cannot create {}: {error}", folder.display()))?;
    let path = folder.join(format!("camera-{}.mp4", stamp()));
    let mut ffmpeg = std::process::Command::new("ffmpeg")
        .args([
            "-loglevel",
            "error",
            "-y",
            "-f",
            "rawvideo",
            "-pix_fmt",
            "rgb24",
            "-s",
        ])
        .arg(format!("{width}x{height}"))
        .args(["-r", &fps.to_string(), "-i", "-", "-pix_fmt", "yuv420p"])
        .arg(&path)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|error| format!("a video needs ffmpeg, which could not be run: {error}"))?;
    let mut input = ffmpeg.stdin.take().expect("piped stdin");
    let started = Instant::now();
    let mut frames = vec![first];
    let mut count = 0usize;
    let mut kept: Vec<RgbImage> = Vec::new();
    while started.elapsed() < Duration::from_secs_f64(seconds) {
        let frame = match frames.pop() {
            Some(frame) => frame,
            None => grab(&mut camera)?,
        };
        if frame.dimensions() != (width, height) {
            continue;
        }
        input
            .write_all(frame.as_raw())
            .map_err(|error| format!("ffmpeg stopped taking frames: {error}"))?;
        // A frame every quarter of the time is kept to be shown.
        let wanted = kept.len() as f64 * seconds / FRAMES_SHOWN as f64;
        if started.elapsed().as_secs_f64() >= wanted && kept.len() < FRAMES_SHOWN {
            kept.push(frame);
        }
        count += 1;
    }
    let _ = camera.stop_stream();
    drop(input);
    let output = ffmpeg
        .wait_with_output()
        .map_err(|error| format!("ffmpeg failed: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "ffmpeg failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let mut images = Vec::new();
    for (number, frame) in kept.into_iter().enumerate() {
        let (image, _) = crate::picture::prepare(
            &DynamicImage::ImageRgb8(frame),
            ImageFormat::Jpeg,
            None,
            folder,
            &format!("frame {}", number + 1),
        )?;
        images.push(image);
    }
    Ok((
        format!(
            "Recorded {count} frames of {width}×{height} at {fps} fps ({seconds} s) from {} \
             to {}; {} of the frames are shown, in order.{}",
            camera.info().human_name(),
            path.display(),
            images.len(),
            controls_line(&set)
        ),
        images,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_photos_and_videos_wait_for_the_person() {
        assert!(!takes_pictures(r#"{"action": "list"}"#));
        assert!(!takes_pictures(r#"{"action": "controls", "device": 0}"#));
        assert!(takes_pictures(r#"{"action": "photo"}"#));
        assert!(takes_pictures(r#"{"action": "video", "seconds": 3}"#));
        assert!(takes_pictures("not json"));
    }

    /// Lists the cameras of the machine; run by hand.
    #[test]
    #[ignore = "uses the machine's cameras"]
    fn lists_the_cameras_here() {
        println!("{}", list().expect("list"));
    }

    /// Takes one photo with camera 0 into the system's temporary folder;
    /// run by hand, with the person's approval.
    #[test]
    #[ignore = "takes a photo with the machine's camera"]
    fn takes_a_photo_here() {
        let folder = std::env::temp_dir().join("inocode-camera-photo");
        std::fs::create_dir_all(&folder).expect("folder");
        let arguments: Arguments =
            serde_json::from_value(json!({"action": "photo", "device": 0})).expect("arguments");
        let (text, images) = photo(&arguments, &folder).expect("photo");
        println!("{text}");
        for image in images {
            println!("{}", image.path);
        }
    }

    #[test]
    fn modes_are_asked_by_auto_and_manual() {
        let wanted: Map<String, Value> =
            serde_json::from_value(json!({"Focus": "auto", "exposure": "manual", "gain": 3}))
                .expect("map");
        let modes = modes(&wanted).expect("modes");
        assert!(modes.contains(&("focus".into(), true)));
        assert!(modes.contains(&("exposure".into(), false)));
        // A value needs the manual mode on Windows, where the library keeps
        // the mode a control has.
        assert_eq!(modes.contains(&("gain".into(), false)), cfg!(windows));
        let wrong: Map<String, Value> =
            serde_json::from_value(json!({"focus": "fast"})).expect("map");
        assert!(super::modes(&wrong).unwrap_err().contains("not fast"));
    }

    #[test]
    fn known_controls_have_fixed_names() {
        assert_eq!(
            known_name(KnownCameraControl::WhiteBalance),
            "white_balance"
        );
        assert_eq!(
            known_name(KnownCameraControl::BacklightComp),
            "backlight_compensation"
        );
        assert_eq!(known_name(KnownCameraControl::Focus), "focus");
    }
}
