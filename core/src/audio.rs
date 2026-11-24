//! The `audio` tool: lists the devices that record sound and records one of
//! them to a WAV or FLAC file in the format asked for. It only records; the
//! sound is not analysed. Design: docs/evidence/audio-recording.md.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, SampleFormat, SizedSample, StreamConfig, SupportedStreamConfig};
use serde::Deserialize;
use serde_json::json;

use crate::conversation::ToolSpec;

pub(crate) const AUDIO: &str = "audio";
const MAX_SECONDS: f64 = 600.0;
/// Supported input formats listed per device at most.
const FORMATS_LISTED: usize = 12;

pub(crate) fn spec() -> ToolSpec {
    ToolSpec {
        name: AUDIO.into(),
        description: "Record sound. list: the devices that record (microphones, line inputs, \
            capture devices), each with its default format and the formats it supports. \
            record: record seconds from one device to a file, wav or flac, at the sample \
            rate, channels and bits asked for (unset ones take the device's default; a \
            rate or channels the device does not deliver are converted to). The sound is saved, not shown to you; \
            transcribe turns speech in it into text. Recording waits for the person's \
            approval."
            .into(),
        parameters: json!({
            "type": "object",
            "required": ["action"],
            "properties": {
                "action": {"type": "string", "enum": ["list", "record"]},
                "device": {
                    "description": "The device's index from list, or its name or id. Unset: the default device.",
                    "type": ["integer", "string"]
                },
                "seconds": {"type": "number", "minimum": 0.1, "maximum": MAX_SECONDS},
                "format": {"type": "string", "enum": ["wav", "flac"]},
                "sample_rate": {"type": "integer", "minimum": 1},
                "channels": {"type": "integer", "minimum": 1},
                "bits": {
                    "type": "integer", "enum": [8, 16, 24, 32],
                    "description": "Bits per sample; flac takes 8, 16 or 24."
                },
                "float": {"type": "boolean", "description": "32-bit float samples (wav only)."},
                "path": {"type": "string", "description": "The file to write, relative to the project folder. Unset: a new file beside the thread's other files."}
            }
        }),
    }
}

#[derive(Deserialize)]
struct Arguments {
    action: String,
    #[serde(default)]
    device: Option<serde_json::Value>,
    #[serde(default)]
    seconds: Option<f64>,
    #[serde(default)]
    format: Option<String>,
    #[serde(default)]
    sample_rate: Option<u32>,
    #[serde(default)]
    channels: Option<u16>,
    #[serde(default)]
    bits: Option<u16>,
    #[serde(default)]
    float: bool,
    #[serde(default)]
    path: Option<String>,
}

/// Whether the call records, which waits for the person.
pub(crate) fn records(arguments: &str) -> bool {
    serde_json::from_str::<Arguments>(arguments)
        .map_or(true, |arguments| arguments.action != "list")
}

/// Runs the tool; `project` is the project folder and `folder` where
/// recordings go when no path is given.
pub(crate) async fn run(
    arguments: &str,
    project: Option<&Path>,
    folder: &Path,
) -> Result<String, String> {
    let arguments: Arguments =
        serde_json::from_str(arguments).map_err(|error| format!("invalid arguments: {error}"))?;
    match arguments.action.as_str() {
        "list" => tokio::task::spawn_blocking(list)
            .await
            .map_err(|error| error.to_string())?,
        "record" => {
            let target = target(&arguments, project, folder)?;
            tokio::task::spawn_blocking(move || record(&arguments, &target))
                .await
                .map_err(|error| error.to_string())?
        }
        other => Err(format!("unknown action {other}; use list or record")),
    }
}

/// The file a recording is written to.
fn target(arguments: &Arguments, project: Option<&Path>, folder: &Path) -> Result<PathBuf, String> {
    let extension = arguments.format.as_deref().unwrap_or("wav");
    match arguments.path.as_deref() {
        Some(path) => {
            let path = Path::new(path);
            if path.is_absolute() {
                Ok(path.to_path_buf())
            } else {
                project
                    .map(|project| project.join(path))
                    .ok_or_else(|| "a relative path needs a project folder".to_string())
            }
        }
        None => {
            let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S%.3f");
            Ok(folder.join(format!("recording-{stamp}.{extension}")))
        }
    }
}

fn input_devices() -> Result<Vec<Device>, String> {
    let host = cpal::default_host();
    let devices = host
        .input_devices()
        .map_err(|error| format!("cannot list the recording devices: {error}"))?;
    Ok(devices.collect())
}

fn label(device: &Device) -> String {
    let name = device
        .description()
        .map(|description| description.name().to_string())
        .unwrap_or_else(|_| "(no name)".to_string());
    match device.id() {
        Ok(id) => format!("{name} (id {id})"),
        Err(_) => name,
    }
}

fn describe(config: &SupportedStreamConfig) -> String {
    format!(
        "{} Hz, {} channels, {}",
        config.sample_rate(),
        config.channels(),
        config.sample_format()
    )
}

fn list() -> Result<String, String> {
    let devices = input_devices()?;
    if devices.is_empty() {
        return Ok("No recording device was found.".to_string());
    }
    let default = cpal::default_host()
        .default_input_device()
        .and_then(|device| device.id().ok());
    let mut text = String::new();
    for (index, device) in devices.iter().enumerate() {
        let mark = if default.is_some() && device.id().ok() == default {
            " [default]"
        } else {
            ""
        };
        text.push_str(&format!("{index}: {}{mark}\n", label(device)));
        if let Ok(config) = device.default_input_config() {
            text.push_str(&format!("   default: {}\n", describe(&config)));
        }
        if let Ok(ranges) = device.supported_input_configs() {
            for range in ranges.take(FORMATS_LISTED) {
                let rates = if range.min_sample_rate() == range.max_sample_rate() {
                    format!("{} Hz", range.min_sample_rate())
                } else {
                    format!("{}-{} Hz", range.min_sample_rate(), range.max_sample_rate())
                };
                text.push_str(&format!(
                    "   supports: {rates}, {} channels, {}\n",
                    range.channels(),
                    range.sample_format()
                ));
            }
        }
    }
    Ok(text.trim_end().to_string())
}

fn device(wanted: Option<&serde_json::Value>) -> Result<Device, String> {
    let Some(wanted) = wanted else {
        return cpal::default_host()
            .default_input_device()
            .ok_or_else(|| "there is no default recording device".to_string());
    };
    let devices = input_devices()?;
    let found = match wanted {
        serde_json::Value::Number(index) => index
            .as_u64()
            .and_then(|index| devices.into_iter().nth(index as usize)),
        serde_json::Value::String(name) => devices.into_iter().find(|device| {
            device.id().is_ok_and(|id| id.to_string() == *name)
                || device
                    .description()
                    .is_ok_and(|description| description.name() == name)
        }),
        _ => None,
    };
    found.ok_or_else(|| format!("no recording device {wanted}; see list"))
}

/// What the file holds: the format asked for, with the device's default
/// for what was not asked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Wanted {
    sample_rate: u32,
    channels: u16,
    bits: u16,
    float: bool,
    flac: bool,
}

fn wanted(arguments: &Arguments, default: &SupportedStreamConfig) -> Result<Wanted, String> {
    let flac = match arguments.format.as_deref().unwrap_or("wav") {
        "wav" => false,
        "flac" => true,
        other => return Err(format!("unknown format {other}; use wav or flac")),
    };
    let float = arguments.float;
    let bits = match (arguments.bits, float) {
        (Some(bits), true) if bits != 32 => {
            return Err("float samples are 32 bits".to_string());
        }
        (_, true) => 32,
        (Some(bits), false) => bits,
        (None, false) => 16,
    };
    if !matches!(bits, 8 | 16 | 24 | 32) {
        return Err(format!("{bits} bits is not one of 8, 16, 24, 32"));
    }
    if flac && (float || bits == 32) {
        return Err("flac holds 8, 16 or 24-bit integer samples".to_string());
    }
    Ok(Wanted {
        sample_rate: arguments.sample_rate.unwrap_or(default.sample_rate()),
        channels: arguments.channels.unwrap_or(default.channels()),
        bits,
        float,
        flac,
    })
}

/// A device format that delivers the rate and channels wanted, preferring
/// the device's default sample format; when the device has none (as a
/// shared device delivers only its mixing format), its default format,
/// converted after recording.
fn stream_config(device: &Device, wanted: &Wanted) -> Result<SupportedStreamConfig, String> {
    let default = device
        .default_input_config()
        .map_err(|error| format!("the device has no input format: {error}"))?;
    let mut ranges: Vec<_> = device
        .supported_input_configs()
        .map_err(|error| format!("the device's formats cannot be read: {error}"))?
        .filter(|range| {
            range.channels() == wanted.channels
                && range.min_sample_rate() <= wanted.sample_rate
                && wanted.sample_rate <= range.max_sample_rate()
        })
        .collect();
    ranges.sort_by_key(|range| range.sample_format() != default.sample_format());
    Ok(ranges
        .into_iter()
        .next()
        .map_or(default, |range| range.with_sample_rate(wanted.sample_rate)))
}

/// Interleaved `samples` with `from` channels as `to` channels: one channel
/// is the mean of all, otherwise channel `n` comes from channel `n % from`.
pub(crate) fn rechannel(samples: &[f32], from: u16, to: u16) -> Vec<f32> {
    let (from, to) = (usize::from(from.max(1)), usize::from(to.max(1)));
    if from == to {
        return samples.to_vec();
    }
    samples
        .chunks(from)
        .flat_map(|frame| {
            (0..to).map(move |channel| {
                if to == 1 {
                    frame.iter().sum::<f32>() / frame.len() as f32
                } else {
                    frame[channel % frame.len()]
                }
            })
        })
        .collect()
}

/// One channel of samples at rate `to`: each output sample averages the
/// input samples it spans when the rate falls, and is interpolated when it
/// rises.
pub(crate) fn resample(samples: &[f32], from: u32, to: u32) -> Vec<f32> {
    if from == to || samples.is_empty() {
        return samples.to_vec();
    }
    let step = f64::from(from) / f64::from(to);
    let count = (samples.len() as f64 / step).floor() as usize;
    (0..count)
        .map(|n| {
            let start = n as f64 * step;
            if step > 1.0 {
                let first = start.floor() as usize;
                let last = ((start + step).ceil() as usize)
                    .min(samples.len())
                    .max(first + 1);
                samples[first..last].iter().sum::<f32>() / (last - first) as f32
            } else {
                let index = start.floor() as usize;
                let next = samples.get(index + 1).copied().unwrap_or(samples[index]);
                let part = (start - index as f64) as f32;
                samples[index] * (1.0 - part) + next * part
            }
        })
        .collect()
}

/// Interleaved samples recorded as `config` in the channels and rate wanted.
fn convert(samples: &[f32], config: &SupportedStreamConfig, wanted: &Wanted) -> Vec<f32> {
    let samples = rechannel(samples, config.channels(), wanted.channels);
    if config.sample_rate() == wanted.sample_rate {
        return samples;
    }
    let channels = usize::from(wanted.channels.max(1));
    let separated: Vec<Vec<f32>> = (0..channels)
        .map(|channel| {
            let one: Vec<f32> = samples
                .iter()
                .skip(channel)
                .step_by(channels)
                .copied()
                .collect();
            resample(&one, config.sample_rate(), wanted.sample_rate)
        })
        .collect();
    let frames = separated.iter().map(Vec::len).min().unwrap_or(0);
    (0..frames)
        .flat_map(|frame| separated.iter().map(move |channel| channel[frame]))
        .collect()
}

fn record(arguments: &Arguments, target: &Path) -> Result<String, String> {
    let seconds = arguments.seconds.unwrap_or(5.0);
    if !(0.1..=MAX_SECONDS).contains(&seconds) {
        return Err(format!("seconds must be from 0.1 to {MAX_SECONDS}"));
    }
    let device = device(arguments.device.as_ref())?;
    let default = device
        .default_input_config()
        .map_err(|error| format!("the device has no input format: {error}"))?;
    let wanted = wanted(arguments, &default)?;
    let config = stream_config(&device, &wanted)?;
    let recorded = capture(&device, &config, seconds)?;
    let samples = convert(&recorded, &config, &wanted);
    let converted =
        if config.sample_rate() == wanted.sample_rate && config.channels() == wanted.channels {
            String::new()
        } else {
            format!(" (the device recorded {}; converted)", describe(&config))
        };
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
    }
    let file = std::fs::File::create(target)
        .map_err(|error| format!("cannot write {}: {error}", target.display()))?;
    let mut file = std::io::BufWriter::new(file);
    if wanted.flac {
        write_flac(&mut file, &samples, &wanted)?;
    } else {
        write_wav(&mut file, &samples, &wanted)?;
    }
    let frames = samples.len() / usize::from(wanted.channels.max(1));
    let kind = if wanted.float { "float" } else { "integer" };
    Ok(format!(
        "Recorded {:.1} s from {} to {}: {}, {} Hz, {} channels, {}-bit {kind}{converted}.",
        frames as f64 / f64::from(wanted.sample_rate),
        label(&device),
        target.display(),
        if wanted.flac { "flac" } else { "wav" },
        wanted.sample_rate,
        wanted.channels,
        wanted.bits
    ))
}

/// Interleaved samples in [-1, 1] recorded for `seconds`.
fn capture(
    device: &Device,
    config: &SupportedStreamConfig,
    seconds: f64,
) -> Result<Vec<f32>, String> {
    let stream_config: StreamConfig = config.config();
    let wanted = (seconds * f64::from(stream_config.sample_rate)).round() as usize
        * usize::from(stream_config.channels);
    let samples = Arc::new(Mutex::new(Vec::with_capacity(wanted)));
    let failure = Arc::new(Mutex::new(None::<String>));
    let stream = match config.sample_format() {
        SampleFormat::I8 => build::<i8>(device, &stream_config, &samples, &failure, wanted),
        SampleFormat::I16 => build::<i16>(device, &stream_config, &samples, &failure, wanted),
        SampleFormat::I32 => build::<i32>(device, &stream_config, &samples, &failure, wanted),
        SampleFormat::U8 => build::<u8>(device, &stream_config, &samples, &failure, wanted),
        SampleFormat::U16 => build::<u16>(device, &stream_config, &samples, &failure, wanted),
        SampleFormat::F32 => build::<f32>(device, &stream_config, &samples, &failure, wanted),
        SampleFormat::F64 => build::<f64>(device, &stream_config, &samples, &failure, wanted),
        other => {
            return Err(format!(
                "the device's sample format {other} is not supported"
            ))
        }
    }?;
    stream
        .play()
        .map_err(|error| format!("cannot start recording: {error}"))?;
    // Waits a little past the length, since the device may start late.
    let deadline = std::time::Instant::now() + Duration::from_secs_f64(seconds + 5.0);
    while std::time::Instant::now() < deadline {
        if samples.lock().expect("samples").len() >= wanted {
            break;
        }
        if let Some(error) = failure.lock().expect("failure").take() {
            return Err(format!("recording failed: {error}"));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    drop(stream);
    let mut samples = std::mem::take(&mut *samples.lock().expect("samples"));
    if samples.is_empty() {
        return Err("the device delivered no sound".to_string());
    }
    samples.truncate(wanted);
    Ok(samples)
}

fn build<T>(
    device: &Device,
    config: &StreamConfig,
    samples: &Arc<Mutex<Vec<f32>>>,
    failure: &Arc<Mutex<Option<String>>>,
    wanted: usize,
) -> Result<cpal::Stream, String>
where
    T: SizedSample,
    f32: cpal::FromSample<T>,
{
    let samples = Arc::clone(samples);
    let failure = Arc::clone(failure);
    device
        .build_input_stream(
            *config,
            move |data: &[T], _: &cpal::InputCallbackInfo| {
                let mut samples = samples.lock().expect("samples");
                let room = wanted.saturating_sub(samples.len());
                samples.extend(
                    data.iter()
                        .take(room)
                        .map(|&sample| sample.to_sample::<f32>()),
                );
            },
            move |error| {
                *failure.lock().expect("failure") = Some(error.to_string());
            },
            None,
        )
        .map_err(|error| format!("cannot open the device: {error}"))
}

/// A sample in [-1, 1] as a signed integer of `bits` bits.
fn integer(sample: f32, bits: u16) -> i32 {
    let peak = ((1_i64 << (bits - 1)) - 1) as f32;
    (sample.clamp(-1.0, 1.0) * peak).round() as i32
}

fn write_wav<W: std::io::Write + std::io::Seek>(
    file: &mut W,
    samples: &[f32],
    wanted: &Wanted,
) -> Result<(), String> {
    let spec = hound::WavSpec {
        channels: wanted.channels,
        sample_rate: wanted.sample_rate,
        bits_per_sample: wanted.bits,
        sample_format: if wanted.float {
            hound::SampleFormat::Float
        } else {
            hound::SampleFormat::Int
        },
    };
    let failed = |error: hound::Error| format!("cannot write the wav file: {error}");
    let mut writer = hound::WavWriter::new(file, spec).map_err(failed)?;
    for &sample in samples {
        if wanted.float {
            writer.write_sample(sample).map_err(failed)?;
        } else if wanted.bits == 8 {
            writer
                .write_sample(integer(sample, 8) as i8)
                .map_err(failed)?;
        } else {
            writer
                .write_sample(integer(sample, wanted.bits))
                .map_err(failed)?;
        }
    }
    writer.finalize().map_err(failed)
}

pub(crate) fn flac_bytes(
    samples: &[i32],
    channels: u16,
    bits: u16,
    sample_rate: u32,
) -> Result<Vec<u8>, String> {
    use flacenc::component::BitRepr;
    use flacenc::error::Verify;
    let config = flacenc::config::Encoder::default()
        .into_verified()
        .map_err(|error| format!("flac encoder: {error:?}"))?;
    let source = flacenc::source::MemSource::from_samples(
        samples,
        usize::from(channels),
        usize::from(bits),
        sample_rate as usize,
    );
    let stream = flacenc::encode_with_fixed_block_size(&config, source, config.block_size)
        .map_err(|error| format!("cannot encode flac: {error:?}"))?;
    let mut sink = flacenc::bitsink::ByteSink::new();
    stream
        .write(&mut sink)
        .map_err(|error| format!("cannot encode flac: {error:?}"))?;
    Ok(sink.as_slice().to_vec())
}

fn write_flac<W: std::io::Write>(
    file: &mut W,
    samples: &[f32],
    wanted: &Wanted,
) -> Result<(), String> {
    let integers: Vec<i32> = samples
        .iter()
        .map(|&sample| integer(sample, wanted.bits))
        .collect();
    let bytes = flac_bytes(&integers, wanted.channels, wanted.bits, wanted.sample_rate)?;
    file.write_all(&bytes)
        .map_err(|error| format!("cannot write the flac file: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(frames: usize, channels: u16) -> Vec<f32> {
        (0..frames * usize::from(channels))
            .map(|n| ((n / usize::from(channels)) as f32 * 0.05).sin() * 0.5)
            .collect()
    }

    fn wanted(bits: u16, float: bool, flac: bool) -> Wanted {
        Wanted {
            sample_rate: 16_000,
            channels: 2,
            bits,
            float,
            flac,
        }
    }

    #[test]
    fn wav_files_hold_the_format_asked_for() {
        let samples = tone(1_600, 2);
        for (bits, float) in [
            (8, false),
            (16, false),
            (24, false),
            (32, false),
            (32, true),
        ] {
            let mut file = std::io::Cursor::new(Vec::new());
            write_wav(&mut file, &samples, &wanted(bits, float, false)).expect("write");
            file.set_position(0);
            let reader = hound::WavReader::new(file).expect("read");
            let spec = reader.spec();
            assert_eq!(
                (spec.sample_rate, spec.channels, spec.bits_per_sample),
                (16_000, 2, bits)
            );
            assert_eq!(spec.sample_format == hound::SampleFormat::Float, float);
            assert_eq!(reader.len() as usize, samples.len());
        }
    }

    #[test]
    fn a_device_format_is_converted_to_the_one_asked_for() {
        let config = SupportedStreamConfig::new(
            2,
            48_000,
            cpal::SupportedBufferSize::Unknown,
            SampleFormat::F32,
        );
        let stereo: Vec<f32> = (0..4_800).flat_map(|_| [0.5, 0.25]).collect();
        let mono = convert(&stereo, &config, &wanted(16, false, false).with(16_000, 1));
        assert_eq!(mono.len(), 1_600);
        assert!(mono.iter().all(|&sample| (sample - 0.375).abs() < 1e-6));
        let kept = convert(&stereo, &config, &wanted(16, false, false).with(16_000, 2));
        assert_eq!(kept.len(), 3_200);
        assert_eq!(&kept[..2], [0.5, 0.25]);
        assert_eq!(rechannel(&[0.1, 0.2], 1, 2).len(), 4);
        assert_eq!(resample(&[0.0, 1.0], 8_000, 16_000), [0.0, 0.5, 1.0, 1.0]);
    }

    impl Wanted {
        fn with(self, sample_rate: u32, channels: u16) -> Self {
            Self {
                sample_rate,
                channels,
                ..self
            }
        }
    }

    #[test]
    fn flac_files_start_with_the_flac_marker() {
        let mut file = Vec::new();
        write_flac(&mut file, &tone(4_096, 2), &wanted(16, false, true)).expect("write");
        assert_eq!(&file[..4], b"fLaC");
        // The STREAMINFO block: sample rate (20 bits), channels - 1 (3 bits),
        // bits - 1 (5 bits).
        let info = &file[8..];
        let packed = u32::from_be_bytes([info[10], info[11], info[12], info[13]]);
        assert_eq!(packed >> 12, 16_000);
        assert_eq!((packed >> 9) & 0b111, 1);
        assert_eq!(((packed >> 4) & 0b1_1111) + 1, 16);
    }

    #[test]
    fn the_format_is_checked_before_recording() {
        let default = SupportedStreamConfig::new(
            2,
            48_000,
            cpal::SupportedBufferSize::Unknown,
            SampleFormat::F32,
        );
        let parse = |text: &str| serde_json::from_str::<Arguments>(text).expect("arguments");
        let unset = super::wanted(&parse(r#"{"action":"record"}"#), &default).expect("default");
        assert_eq!(
            unset,
            Wanted {
                sample_rate: 48_000,
                channels: 2,
                bits: 16,
                float: false,
                flac: false
            }
        );
        let flac_float = parse(r#"{"action":"record","format":"flac","float":true}"#);
        assert!(super::wanted(&flac_float, &default).is_err());
        let odd = parse(r#"{"action":"record","bits":12}"#);
        assert!(super::wanted(&odd, &default).is_err());
        assert!(!records(r#"{"action":"list"}"#));
        assert!(records(r#"{"action":"record"}"#));
    }

    /// Lists the recording devices of this computer.
    #[test]
    #[ignore = "needs this computer's sound devices"]
    fn lists_the_devices_here() {
        println!("{}", list().expect("list"));
    }
}
