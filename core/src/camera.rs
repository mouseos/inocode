//! The `camera` tool: lists the cameras (also video capture devices such as
//! a UVC HDMI capture), shows one's formats and controls, takes a photo or
//! records a video, after setting the controls asked for. Photos and frames
//! are shown to the model like images read from files. Design:
//! docs/evidence/image-input.md.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use image::{DynamicImage, ImageFormat, RgbImage};
use nokhwa::pixel_format::{FormatDecoder, RgbFormat};
use nokhwa::utils::{
    ApiBackend, CameraControl, CameraFormat, CameraIndex, ControlValueDescription,
    ControlValueSetter, FrameFormat, KnownCameraControl, KnownCameraControlFlag, RequestedFormat,
    RequestedFormatType,
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
        (Some(_), _) => RequestedFormatType::None,
        (None, Some(fps)) => RequestedFormatType::HighestFrameRate(fps),
        (None, None) => RequestedFormatType::AbsoluteHighestResolution,
    };
    let mut camera = Camera::with_backend(
        index,
        RequestedFormat::new::<RgbFormat>(requested),
        backend(),
    )
    .map_err(|error| format!("cannot open the camera: {error}"))?;
    if let Some(resolution) = arguments.resolution {
        let formats = camera
            .compatible_camera_formats()
            .map_err(|error| format!("cannot read the formats: {error}"))?;
        let format = closest(&formats, resolution, arguments.fps.unwrap_or(30))
            .ok_or("the camera offers no format that can be read")?;
        camera
            .set_camera_requset(RequestedFormat::new::<RgbFormat>(
                RequestedFormatType::Exact(format),
            ))
            .map_err(|error| format!("cannot set the format {format}: {error}"))?;
    }
    Ok(camera)
}

/// The readable format nearest the resolution asked for, then nearest the
/// frame rate, MJPEG first, as it takes the least of the connection.
/// The capture library's own nearest match only looks at one pixel format.
fn closest(formats: &[CameraFormat], [width, height]: [u32; 2], fps: u32) -> Option<CameraFormat> {
    formats
        .iter()
        .filter(|format| RgbFormat::FORMATS.contains(&format.format()))
        .min_by_key(|format| {
            let distance = |have: u32, want: u32| (i64::from(have) - i64::from(want)).pow(2);
            (
                distance(format.width(), width) + distance(format.height(), height),
                format.frame_rate().abs_diff(fps),
                format.format() != FrameFormat::MJPEG,
            )
        })
        .copied()
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
    let controls = controls_of(&camera)?;
    let controls: Vec<String> = controls
        .iter()
        .filter(|control| !holds_a_mode(control))
        .map(|control| describe_control(control, automatic_now(&controls, control)))
        .collect();
    Ok(format!(
        "{}\nFormats (largest first):\n{}\nControls:\n{}",
        camera.info().human_name(),
        formats.join("\n"),
        controls.join("\n")
    ))
}

/// A control's name, values and modes as the model reads them.
fn describe_control(control: &CameraControl, automatic: Option<bool>) -> String {
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
    let mut modes: Vec<String> = control.flag().iter().map(ToString::to_string).collect();
    if let Some(automatic) = automatic {
        let flag = if automatic {
            KnownCameraControlFlag::Automatic
        } else {
            KnownCameraControlFlag::Manual
        };
        modes.insert(0, flag.to_string());
    }
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
    #[cfg(target_os = "linux")]
    if let Some(name) = v4l2::name(control.control()) {
        return name.into();
    }
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

/// The controls of an opened camera. On Linux they are read here: the
/// capture library stops at a control of a type it does not know, such as
/// the region of interest of newer UVC cameras.
#[cfg(target_os = "linux")]
fn controls_of(camera: &Camera) -> Result<Vec<CameraControl>, String> {
    v4l2::controls(device_number(camera)?)
}

#[cfg(not(target_os = "linux"))]
fn controls_of(camera: &Camera) -> Result<Vec<CameraControl>, String> {
    camera
        .camera_controls()
        .map_err(|error| format!("cannot read the controls: {error}"))
}

#[cfg(target_os = "linux")]
fn set_control(
    camera: &mut Camera,
    control: KnownCameraControl,
    setter: ControlValueSetter,
) -> Result<(), String> {
    v4l2::set(device_number(camera)?, control, &setter)
}

#[cfg(not(target_os = "linux"))]
fn set_control(
    camera: &mut Camera,
    control: KnownCameraControl,
    setter: ControlValueSetter,
) -> Result<(), String> {
    camera
        .set_camera_control(control, setter)
        .map_err(|error| error.to_string())
}

/// The number of the camera's `/dev/video` node.
#[cfg(target_os = "linux")]
fn device_number(camera: &Camera) -> Result<usize, String> {
    match camera.info().index() {
        CameraIndex::Index(index) => Ok(*index as usize),
        CameraIndex::String(text) => text
            .trim_start_matches("/dev/video")
            .parse()
            .map_err(|_| format!("cannot tell the device of {text}")),
    }
}

/// Sets the controls asked for; what was set, or the first that failed.
fn set_controls(camera: &mut Camera, wanted: &Map<String, Value>) -> Result<Vec<String>, String> {
    if wanted.is_empty() {
        return Ok(Vec::new());
    }
    let controls = controls_of(camera)?;
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
        set_control(camera, control.control(), setter)
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
/// the manual mode a value needs to take effect where the system keeps the
/// mode apart from the value.
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
            _ if value_needs_manual(&name) => modes.push((name, false)),
            _ => {}
        }
    }
    Ok(modes)
}

#[cfg(windows)]
fn value_needs_manual(name: &str) -> bool {
    crate::camera_mode::has_mode(name)
}

#[cfg(target_os = "linux")]
fn value_needs_manual(name: &str) -> bool {
    v4l2::auto_control(name).is_some()
}

/// On macOS the control is the mode itself, so a value sets the mode.
#[cfg(not(any(windows, target_os = "linux")))]
fn value_needs_manual(_name: &str) -> bool {
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
fn apply_modes(_index: &CameraIndex, _modes: &[(String, bool)]) -> Result<(), String> {
    Ok(())
}

/// Switches the modes asked for on the opened camera, where the system
/// keeps a mode as a control of its own (Linux) or as the control itself
/// (macOS). A mode only implied by a value is skipped when the camera has
/// no such mode.
#[cfg(windows)]
fn switch_modes(
    _camera: &mut Camera,
    _modes: &[(String, bool)],
    _wanted: &Map<String, Value>,
) -> Result<(), String> {
    Ok(())
}

#[cfg(not(windows))]
fn switch_modes(
    camera: &mut Camera,
    modes: &[(String, bool)],
    wanted: &Map<String, Value>,
) -> Result<(), String> {
    for (name, automatic) in modes {
        let asked = wanted
            .iter()
            .any(|(key, value)| key.to_lowercase() == *name && value.is_string());
        let controls = controls_of(camera)?;
        let Some((control, setter)) = mode_setter(&controls, name, *automatic) else {
            if asked {
                return Err(format!("this camera has no automatic mode for {name}"));
            }
            continue;
        };
        let mode = if *automatic { "automatic" } else { "manual" };
        set_control(camera, control, setter)
            .map_err(|error| format!("cannot set {name} to {mode}: {error}"))?;
    }
    Ok(())
}

/// The control holding `name`'s mode and the value that sets it.
#[cfg(target_os = "linux")]
fn mode_setter(
    controls: &[CameraControl],
    name: &str,
    automatic: bool,
) -> Option<(KnownCameraControl, ControlValueSetter)> {
    let id = v4l2::auto_control(name)?;
    let control = controls
        .iter()
        .find(|control| control.control() == KnownCameraControl::Other(id.into()))?;
    let setter = match control.description() {
        ControlValueDescription::Boolean { .. } => ControlValueSetter::Boolean(automatic),
        ControlValueDescription::IntegerRange { max, .. } if id == v4l2::EXPOSURE_AUTO => {
            ControlValueSetter::Integer(v4l2::exposure_mode(automatic, *max))
        }
        _ => ControlValueSetter::Integer(i64::from(automatic)),
    };
    Some((control.control(), setter))
}

/// AVFoundation's modes: 0 locks the current value, 1 adjusts once, 2 keeps
/// adjusting.
#[cfg(target_os = "macos")]
fn mode_setter(
    controls: &[CameraControl],
    name: &str,
    automatic: bool,
) -> Option<(KnownCameraControl, ControlValueSetter)> {
    let known = match name {
        "focus" => KnownCameraControl::Focus,
        "exposure" => KnownCameraControl::Exposure,
        "white_balance" => KnownCameraControl::WhiteBalance,
        _ => return None,
    };
    let control = controls.iter().find(|control| control.control() == known)?;
    let ControlValueDescription::Enum { possible, .. } = control.description() else {
        return None;
    };
    let wanted: &[i64] = if automatic { &[2, 1] } else { &[0] };
    let mode = wanted
        .iter()
        .copied()
        .find(|mode| possible.contains(mode))?;
    Some((known, ControlValueSetter::EnumValue(mode)))
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
fn mode_setter(
    _controls: &[CameraControl],
    _name: &str,
    _automatic: bool,
) -> Option<(KnownCameraControl, ControlValueSetter)> {
    None
}

/// Whether the control's mode is automatic now, where the system keeps the
/// mode as a control of its own; elsewhere the capture library's flags say.
#[cfg(target_os = "linux")]
fn automatic_now(controls: &[CameraControl], control: &CameraControl) -> Option<bool> {
    let id = v4l2::auto_control(&control_name(control))?;
    let mode = controls
        .iter()
        .find(|mode| mode.control() == KnownCameraControl::Other(id.into()))?;
    Some(v4l2::is_automatic(id, &mode.value()))
}

#[cfg(not(target_os = "linux"))]
fn automatic_now(_controls: &[CameraControl], _control: &CameraControl) -> Option<bool> {
    None
}

/// Whether a control only holds another's mode, shown as that one's mode.
#[cfg(target_os = "linux")]
fn holds_a_mode(control: &CameraControl) -> bool {
    matches!(control.control(), KnownCameraControl::Other(id) if v4l2::is_auto_control(id))
}

#[cfg(not(target_os = "linux"))]
fn holds_a_mode(_control: &CameraControl) -> bool {
    false
}

/// V4L2 keeps a control's automatic mode as a control of its own, and names
/// the absolute pan, tilt, zoom, focus, iris and exposure apart from the
/// relative ones (linux/v4l2-controls.h).
#[cfg(target_os = "linux")]
mod v4l2 {
    use nokhwa::utils::{
        CameraControl, ControlValueDescription, ControlValueSetter, KnownCameraControl,
        KnownCameraControlFlag,
    };
    use v4l::control::{Control, Value};
    use v4l::v4l_sys::{v4l2_query_ext_ctrl, V4L2_CTRL_FLAG_NEXT_CTRL};
    use v4l::Device;

    const USER: u32 = 0x0098_0900;
    const CAMERA: u32 = 0x009a_0900;
    pub(super) const EXPOSURE_AUTO: u32 = CAMERA + 1;
    const EXPOSURE_ABSOLUTE: u32 = CAMERA + 2;
    const PAN_ABSOLUTE: u32 = CAMERA + 8;
    const TILT_ABSOLUTE: u32 = CAMERA + 9;
    const FOCUS_ABSOLUTE: u32 = CAMERA + 10;
    const FOCUS_AUTO: u32 = CAMERA + 12;
    const ZOOM_ABSOLUTE: u32 = CAMERA + 13;
    const IRIS_ABSOLUTE: u32 = CAMERA + 17;
    const AUTO_WHITE_BALANCE: u32 = USER + 12;
    const AUTOGAIN: u32 = USER + 18;
    const HUE_AUTO: u32 = USER + 25;
    const AUTOBRIGHTNESS: u32 = USER + 32;
    const AUTO_CONTROLS: [u32; 6] = [
        EXPOSURE_AUTO,
        FOCUS_AUTO,
        AUTO_WHITE_BALANCE,
        AUTOGAIN,
        HUE_AUTO,
        AUTOBRIGHTNESS,
    ];

    /// The controls the capture library names, by their V4L2 ids, as it
    /// maps them.
    const KNOWN: [(u32, KnownCameraControl); 15] = [
        (USER, KnownCameraControl::Brightness),
        (USER + 1, KnownCameraControl::Contrast),
        (USER + 2, KnownCameraControl::Saturation),
        (USER + 3, KnownCameraControl::Hue),
        (USER + 16, KnownCameraControl::Gamma),
        (USER + 17, KnownCameraControl::Exposure),
        (USER + 19, KnownCameraControl::Gain),
        (USER + 26, KnownCameraControl::WhiteBalance),
        (USER + 27, KnownCameraControl::Sharpness),
        (USER + 28, KnownCameraControl::BacklightComp),
        (CAMERA + 4, KnownCameraControl::Pan),
        (CAMERA + 5, KnownCameraControl::Tilt),
        (CAMERA + 11, KnownCameraControl::Focus),
        (CAMERA + 14, KnownCameraControl::Zoom),
        (CAMERA + 18, KnownCameraControl::Iris),
    ];

    fn known(id: u32) -> KnownCameraControl {
        KNOWN
            .iter()
            .find(|(known, _)| *known == id)
            .map_or(KnownCameraControl::Other(id.into()), |(_, control)| {
                *control
            })
    }

    fn id(control: KnownCameraControl) -> Option<u32> {
        match control {
            KnownCameraControl::Other(id) => u32::try_from(id).ok(),
            control => KNOWN
                .iter()
                .find(|(_, known)| *known == control)
                .map(|(id, _)| *id),
        }
    }

    /// The controls of `/dev/video<number>` that hold one number or one
    /// switch; the others (buttons, strings, areas, arrays) are skipped.
    pub(super) fn controls(number: usize) -> Result<Vec<CameraControl>, String> {
        let device =
            Device::new(number).map_err(|error| format!("cannot read the controls: {error}"))?;
        let fd = device.handle().fd();
        let mut controls = Vec::new();
        // SAFETY: an all-zero v4l2_query_ext_ctrl is a valid query, and
        // VIDIOC_QUERY_EXT_CTRL takes a pointer to one, which it fills.
        let mut query: v4l2_query_ext_ctrl = unsafe { std::mem::zeroed() };
        loop {
            query.id |= V4L2_CTRL_FLAG_NEXT_CTRL;
            let found = unsafe {
                v4l::v4l2::ioctl(
                    fd,
                    v4l::v4l2::vidioc::VIDIOC_QUERY_EXT_CTRL,
                    std::ptr::addr_of_mut!(query).cast(),
                )
            };
            if found.is_err() {
                break;
            }
            if let Some(control) = control(&device, &query) {
                controls.push(control);
            }
        }
        Ok(controls)
    }

    /// Integer 1, boolean 2, menu 3 and 64-bit integer 5: the types read
    /// as one value (videodev2.h).
    fn control(device: &Device, query: &v4l2_query_ext_ctrl) -> Option<CameraControl> {
        if !matches!(query.type_, 1 | 2 | 3 | 5) {
            return None;
        }
        let description = match device.control(query.id).ok()?.value {
            Value::Boolean(value) => ControlValueDescription::Boolean {
                value,
                default: query.default_value != 0,
            },
            Value::Integer(value) => ControlValueDescription::IntegerRange {
                min: query.minimum,
                max: query.maximum,
                value,
                step: query.step as i64,
                default: query.default_value,
            },
            _ => return None,
        };
        // V4L2_CTRL_FLAG_*: disabled 0x1, read only 0x4, inactive 0x10,
        // write only 0x40, volatile 0x80.
        let flags = [
            (0x1 | 0x10, KnownCameraControlFlag::Disabled),
            (0x4, KnownCameraControlFlag::ReadOnly),
            (0x40, KnownCameraControlFlag::WriteOnly),
            (0x80, KnownCameraControlFlag::Volatile),
        ]
        .into_iter()
        .filter(|(bits, _)| query.flags & bits != 0)
        .map(|(_, flag)| flag)
        .collect();
        // SAFETY: the driver ends the name with a zero within the array.
        let name = unsafe { std::ffi::CStr::from_ptr(query.name.as_ptr()) }
            .to_string_lossy()
            .into_owned();
        Some(CameraControl::new(
            known(query.id),
            name,
            description,
            flags,
            query.flags & 0x10 == 0,
        ))
    }

    /// Sets a control of `/dev/video<number>` and checks that the camera
    /// kept the value.
    pub(super) fn set(
        number: usize,
        control: KnownCameraControl,
        setter: &ControlValueSetter,
    ) -> Result<(), String> {
        let id = id(control).ok_or("an unknown control")?;
        let value = || match setter {
            ControlValueSetter::Integer(value) => Ok(Value::Integer(*value)),
            ControlValueSetter::Boolean(on) => Ok(Value::Boolean(*on)),
            other => Err(format!("{other} is not a value this camera takes")),
        };
        let device = Device::new(number).map_err(|error| error.to_string())?;
        device
            .set_control(Control {
                id,
                value: value()?,
            })
            .map_err(|error| error.to_string())?;
        let kept = device.control(id).map_err(|error| error.to_string())?.value;
        if kept != value()? {
            return Err(format!("the camera kept {kept:?}"));
        }
        Ok(())
    }

    /// The control holding the automatic mode of the control `name`.
    pub(super) fn auto_control(name: &str) -> Option<u32> {
        Some(match name {
            "exposure" => EXPOSURE_AUTO,
            "focus" => FOCUS_AUTO,
            "white_balance" => AUTO_WHITE_BALANCE,
            "gain" => AUTOGAIN,
            "hue" => HUE_AUTO,
            "brightness" => AUTOBRIGHTNESS,
            _ => return None,
        })
    }

    pub(super) fn is_auto_control(id: u128) -> bool {
        AUTO_CONTROLS
            .iter()
            .any(|&control| u128::from(control) == id)
    }

    /// The name a control is set by, the same as on the other systems: the
    /// absolute ones by the plain name, the relative ones apart from them.
    pub(super) fn name(control: KnownCameraControl) -> Option<&'static str> {
        Some(match control {
            KnownCameraControl::Pan => "pan_relative",
            KnownCameraControl::Tilt => "tilt_relative",
            KnownCameraControl::Zoom => "zoom_relative",
            KnownCameraControl::Focus => "focus_relative",
            KnownCameraControl::Iris => "iris_relative",
            KnownCameraControl::Other(id) => match u32::try_from(id).ok()? {
                EXPOSURE_ABSOLUTE => "exposure",
                PAN_ABSOLUTE => "pan",
                TILT_ABSOLUTE => "tilt",
                FOCUS_ABSOLUTE => "focus",
                ZOOM_ABSOLUTE => "zoom",
                IRIS_ABSOLUTE => "iris",
                _ => return None,
            },
            _ => return None,
        })
    }

    /// The exposure mode: manual is 1; a menu reaching 3 offers aperture
    /// priority, the automatic mode of UVC cameras, and the others the
    /// fully automatic 0.
    pub(super) fn exposure_mode(automatic: bool, max: i64) -> i64 {
        match (automatic, max >= 3) {
            (false, _) => 1,
            (true, true) => 3,
            (true, false) => 0,
        }
    }

    /// Whether the mode control `id` with `value` is automatic.
    pub(super) fn is_automatic(id: u32, value: &ControlValueSetter) -> bool {
        match value {
            ControlValueSetter::Boolean(on) => *on,
            ControlValueSetter::Integer(mode) if id == EXPOSURE_AUTO => *mode != 1,
            ControlValueSetter::Integer(on) => *on != 0,
            _ => false,
        }
    }
}

fn start(arguments: &Arguments) -> Result<(Camera, Vec<String>), String> {
    let index = index_of(arguments.device.as_ref())?;
    let modes = modes(&arguments.controls)?;
    apply_modes(&index, &modes)?;
    let mut camera = open_index(index, arguments)?;
    switch_modes(&mut camera, &modes, &arguments.controls)?;
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

    fn device() -> usize {
        std::env::var("INOCODE_CAMERA_DEVICE").map_or(0, |device| device.parse().expect("a number"))
    }

    /// Records two seconds of video with the camera `INOCODE_CAMERA_DEVICE` (0 when unset) into the system's
    /// temporary folder; run by hand, with the person's approval.
    #[test]
    #[ignore = "records a video with the machine's camera"]
    fn records_a_video_here() {
        let folder = std::env::temp_dir().join("inocode-camera-video");
        std::fs::create_dir_all(&folder).expect("folder");
        let arguments: Arguments =
            serde_json::from_value(json!({"action": "video", "device": device(), "seconds": 2}))
                .expect("arguments");
        let (text, images) = video(&arguments, &folder).expect("video");
        println!("{text}");
        for image in images {
            println!("{}", image.path);
        }
    }

    /// Takes photos with the camera `INOCODE_CAMERA_DEVICE` (0 when unset) after switching exposure to manual with a
    /// value, then back to automatic, printing the controls before and after;
    /// run by hand, with the person's approval. `INOCODE_CAMERA_EXPOSURE`
    /// names the manual value.
    #[test]
    #[ignore = "changes the settings of the machine's camera"]
    fn sets_controls_here() {
        let folder = std::env::temp_dir().join("inocode-camera-controls");
        std::fs::create_dir_all(&folder).expect("folder");
        let show = |label: &str| {
            let arguments: Arguments =
                serde_json::from_value(json!({"action": "controls", "device": device()}))
                    .expect("arguments");
            println!("=== {label}\n{}", describe(&arguments).expect("controls"));
        };
        show("before");
        let Ok(exposure) = std::env::var("INOCODE_CAMERA_EXPOSURE") else {
            return;
        };
        let exposure: f64 = exposure.parse().expect("a number");
        for controls in [json!({"exposure": exposure}), json!({"exposure": "auto"})] {
            let arguments: Arguments = serde_json::from_value(
                json!({"action": "photo", "device": device(), "controls": controls}),
            )
            .expect("arguments");
            let (text, images) = photo(&arguments, &folder).expect("photo");
            println!("--- {controls}\n{text}");
            for image in images {
                println!("{}", image.path);
            }
        }
        show("after");
    }

    /// Prints the controls of each camera and the mean brightness of its
    /// frames for four seconds after the stream opens, to see when the
    /// automatic exposure settles. Keeps no picture; run by hand, with the
    /// person's approval.
    #[test]
    #[ignore = "uses the machine's cameras"]
    fn prints_brightness_as_the_stream_starts() {
        for device in 0..nokhwa::query(backend()).expect("cameras").len() {
            let arguments: Arguments =
                serde_json::from_value(json!({"action": "controls", "device": device}))
                    .expect("arguments");
            println!(
                "=== camera {device}\n{}",
                describe(&arguments).expect("controls")
            );
            brightness(&arguments);
        }
    }

    fn brightness(arguments: &Arguments) {
        let (mut camera, _) = start(arguments).expect("camera");
        let started = Instant::now();
        let mut index = 0;
        while started.elapsed() < Duration::from_secs(4) {
            let frame = grab(&mut camera).expect("frame");
            let pixels = frame.pixels().count().max(1) as u64;
            let sum: u64 = frame
                .pixels()
                .map(|pixel| pixel.0.iter().map(|&value| u64::from(value)).sum::<u64>() / 3)
                .sum();
            println!(
                "frame {index} at {} ms: mean {}",
                started.elapsed().as_millis(),
                sum / pixels
            );
            index += 1;
        }
        let _ = camera.stop_stream();
    }

    #[test]
    fn modes_are_asked_by_auto_and_manual() {
        let wanted: Map<String, Value> =
            serde_json::from_value(json!({"Focus": "auto", "exposure": "manual", "gain": 3}))
                .expect("map");
        let modes = modes(&wanted).expect("modes");
        assert!(modes.contains(&("focus".into(), true)));
        assert!(modes.contains(&("exposure".into(), false)));
        // A value needs the manual mode where the system keeps the mode
        // apart from the value; on macOS the control is the mode.
        assert_eq!(
            modes.contains(&("gain".into(), false)),
            cfg!(any(windows, target_os = "linux"))
        );
        let wrong: Map<String, Value> =
            serde_json::from_value(json!({"focus": "fast"})).expect("map");
        assert!(super::modes(&wrong).unwrap_err().contains("not fast"));
    }

    /// Sets `INOCODE_CAMERA_CONTROL` (gain when unset) of the camera
    /// `INOCODE_CAMERA_DEVICE` to the value `INOCODE_CAMERA_VALUE` (77 when
    /// unset), which turns its mode manual, then back to automatic, checking
    /// the controls after each; run by hand, or in CI with a virtual camera.
    #[test]
    #[ignore = "changes the settings of the machine's camera"]
    fn switches_a_mode_here() {
        let name = std::env::var("INOCODE_CAMERA_CONTROL").unwrap_or_else(|_| "gain".into());
        let value: i64 = std::env::var("INOCODE_CAMERA_VALUE")
            .map_or(77, |value| value.parse().expect("a number"));
        let folder = std::env::temp_dir().join("inocode-camera-mode");
        let line = || {
            let arguments: Arguments =
                serde_json::from_value(json!({"action": "controls", "device": device()}))
                    .expect("arguments");
            let text = describe(&arguments).expect("controls");
            println!("{text}");
            text.lines()
                .find(|line| line.starts_with(&format!("- {name}: ")))
                .unwrap_or_else(|| panic!("no control {name}"))
                .to_string()
        };
        line();
        for controls in [json!({name.clone(): value}), json!({name.clone(): "auto"})] {
            let arguments: Arguments = serde_json::from_value(json!({
                "action": "photo", "device": device(), "resolution": [640, 480],
                "controls": controls
            }))
            .expect("arguments");
            let (text, _) = photo(&arguments, &folder).expect("photo");
            println!("--- {controls}\n{text}");
            let line = line();
            if controls[&name].is_string() {
                assert!(line.contains("Automatic"), "{line}");
            } else {
                assert!(line.starts_with(&format!("- {name}: {value} ")), "{line}");
                assert!(line.contains("Manual"), "{line}");
            }
        }
        let _ = std::fs::remove_dir_all(&folder);
    }

    #[cfg(target_os = "linux")]
    fn control(id: u32, name: &str, description: ControlValueDescription) -> CameraControl {
        CameraControl::new(
            KnownCameraControl::Other(id.into()),
            name.into(),
            description,
            Vec::new(),
            true,
        )
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_modes_are_controls_of_their_own() {
        let menu = ControlValueDescription::IntegerRange {
            min: 0,
            max: 3,
            value: 3,
            step: 1,
            default: 3,
        };
        let switch = ControlValueDescription::Boolean {
            value: true,
            default: true,
        };
        let absolute = ControlValueDescription::IntegerRange {
            min: 1,
            max: 5000,
            value: 156,
            step: 1,
            default: 156,
        };
        let controls = [
            control(0x009a_0901, "Auto Exposure", menu),
            control(0x009a_0902, "Exposure Time, Absolute", absolute),
            control(0x0098_0912, "Gain, Automatic", switch),
        ];
        assert_eq!(control_name(&controls[1]), "exposure");
        assert!(holds_a_mode(&controls[0]) && holds_a_mode(&controls[2]));
        assert!(!holds_a_mode(&controls[1]));
        assert_eq!(automatic_now(&controls, &controls[1]), Some(true));
        let (_, manual) = mode_setter(&controls, "exposure", false).expect("exposure");
        assert_eq!(manual, ControlValueSetter::Integer(1));
        let (_, automatic) = mode_setter(&controls, "exposure", true).expect("exposure");
        assert_eq!(automatic, ControlValueSetter::Integer(3));
        let (id, gain) = mode_setter(&controls, "gain", false).expect("gain");
        assert_eq!(id, KnownCameraControl::Other(0x0098_0912));
        assert_eq!(gain, ControlValueSetter::Boolean(false));
        assert!(mode_setter(&controls, "focus", true).is_none());
        assert_eq!(
            control_name(&CameraControl::new(
                KnownCameraControl::Pan,
                "Pan, Relative".into(),
                ControlValueDescription::None,
                Vec::new(),
                true,
            )),
            "pan_relative"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_modes_are_the_controls_themselves() {
        let controls = [CameraControl::new(
            KnownCameraControl::Exposure,
            "ExposureMode".into(),
            ControlValueDescription::Enum {
                value: 2,
                possible: vec![0, 2],
                default: 2,
            },
            Vec::new(),
            true,
        )];
        assert_eq!(
            mode_setter(&controls, "exposure", true),
            Some((
                KnownCameraControl::Exposure,
                ControlValueSetter::EnumValue(2)
            ))
        );
        assert_eq!(
            mode_setter(&controls, "exposure", false),
            Some((
                KnownCameraControl::Exposure,
                ControlValueSetter::EnumValue(0)
            ))
        );
        assert!(mode_setter(&controls, "focus", true).is_none());
    }

    #[test]
    fn the_nearest_readable_format_is_taken() {
        let format = |width, height, kind, fps| {
            CameraFormat::new(nokhwa::utils::Resolution::new(width, height), kind, fps)
        };
        let formats = [
            format(3840, 2160, FrameFormat::YUYV, 25),
            format(640, 480, FrameFormat::YUYV, 30),
            format(640, 480, FrameFormat::YUYV, 5),
            format(1280, 720, FrameFormat::MJPEG, 30),
        ];
        assert_eq!(closest(&formats, [640, 480], 30), Some(formats[1]));
        assert_eq!(closest(&formats, [1200, 700], 30), Some(formats[3]));
        assert_eq!(closest(&[], [640, 480], 30), None);
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
