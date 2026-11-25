//! The `transcribe` tool: speech in an audio file as text, from one of
//! three services: Claude's speech-to-text with the person's Claude
//! sign-in (as Claude Code's voice input), ChatGPT's with the ChatGPT
//! sign-in (as Codex's dictation), or the Google speech service Chrome's
//! speech recognition uses, with a key the person sets. None of the three
//! is a published API. Wire facts and sources:
//! docs/evidence/transcription.md.

use std::path::Path;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::json;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

use crate::conversation::ToolSpec;

pub(crate) const TRANSCRIBE: &str = "transcribe";
/// The rate and format Claude's and Google's services are sent: 16 kHz
/// mono 16-bit, as Claude Code and Chrome send.
const RATE: u32 = 16_000;
/// Bytes of audio per WebSocket message: 100 ms.
const CHUNK: usize = RATE as usize / 10 * 2;
/// The longest audio sent.
const MAX_SECONDS: f64 = 600.0;
const KEEP_ALIVE: Duration = Duration::from_secs(8);
/// How long the services may take after the audio is sent.
const WAIT: Duration = Duration::from_secs(30);
/// The largest file sent to ChatGPT.
const MAX_BYTES: u64 = 25 * 1024 * 1024;

/// Where the services are; the defaults are the real ones.
#[derive(Debug, Clone)]
pub(crate) struct Hosts {
    pub claude: String,
    pub chatgpt: String,
    pub google: String,
}

impl Default for Hosts {
    fn default() -> Self {
        Self {
            claude: "wss://api.anthropic.com".into(),
            chatgpt: "https://chatgpt.com/backend-api".into(),
            google: "https://www.google.com/speech-api/full-duplex/v1".into(),
        }
    }
}

pub(crate) fn spec() -> ToolSpec {
    ToolSpec {
        name: TRANSCRIBE.into(),
        description: "Turn speech in an audio file into text. service claude uses the \
            person's Claude sign-in and chatgpt their ChatGPT sign-in; google uses the \
            speech service of Chrome's speech recognition. claude and google read wav files; chatgpt also takes mp3, \
            m4a, webm and ogg. language is a code such as ja or ja-JP (chatgpt detects \
            it). The audio is sent to that service."
            .into(),
        parameters: json!({
            "type": "object",
            "required": ["path", "service"],
            "properties": {
                "path": {"type": "string", "description": "The audio file, relative to the project folder."},
                "service": {"type": "string", "enum": ["claude", "chatgpt", "google"]},
                "language": {"type": "string"}
            }
        }),
    }
}

#[derive(Deserialize)]
struct Arguments {
    path: String,
    service: String,
    #[serde(default)]
    language: Option<String>,
}

/// Runs the tool. `accounts` is the sign-in store, `settings` the settings
/// file that holds the Google key.
pub(crate) async fn run(
    arguments: &str,
    project: Option<&Path>,
    accounts: &Path,
    settings: &Path,
    hosts: &Hosts,
) -> Result<String, String> {
    let arguments: Arguments =
        serde_json::from_str(arguments).map_err(|error| format!("invalid arguments: {error}"))?;
    let path = Path::new(&arguments.path);
    let path = match project {
        Some(project) if path.is_relative() => project.join(path),
        _ => path.to_path_buf(),
    };
    let language = match arguments.language.as_deref().map(str::trim) {
        Some(language) if !language.is_empty() => Some(language.to_string()),
        _ => default_language(settings).await,
    };
    let text = match arguments.service.as_str() {
        "claude" => {
            let audio = pcm(&path).await?;
            let headers = crate::connectors::Account::Claude.headers(accounts).await?;
            // Claude's service takes the language alone, without a region.
            let language = language
                .as_deref()
                .and_then(|tag| tag.split('-').next())
                .unwrap_or("en")
                .to_ascii_lowercase();
            claude(&hosts.claude, headers, &language, &audio).await?
        }
        "chatgpt" => {
            let headers = crate::connectors::Account::ChatGpt
                .headers(accounts)
                .await?;
            chatgpt(&hosts.chatgpt, headers, &path).await?
        }
        "google" => {
            let key = google_key(settings).await?;
            let audio = pcm(&path).await?;
            google(
                &hosts.google,
                &key,
                language.as_deref().unwrap_or("en-US"),
                &audio,
            )
            .await?
        }
        other => {
            return Err(format!(
                "unknown service {other}; use claude, chatgpt or google"
            ))
        }
    };
    Ok(if text.trim().is_empty() {
        "No speech was recognized.".to_string()
    } else {
        text
    })
}

/// The language spoken when the call names none: the reply language of the
/// settings when it is written as a language tag (such as `ja` or
/// `ja-JP`), else the system's display language; `None` when neither gives
/// a tag, and the services' own default (English) is used.
async fn default_language(settings: &Path) -> Option<String> {
    let configured = crate::settings::read(settings)
        .await
        .ok()
        .and_then(|value| {
            value
                .get("language")?
                .as_str()
                .map(str::trim)
                .map(str::to_string)
        });
    configured
        .filter(|language| is_tag(language))
        .or_else(|| crate::language::system().filter(|language| is_tag(language)))
}

/// Whether `text` is a language tag: a primary language of two or three
/// letters, then subtags of letters and digits.
fn is_tag(text: &str) -> bool {
    let mut parts = text.split(['-', '_']);
    parts.next().is_some_and(|primary| {
        (2..=3).contains(&primary.len()) && primary.chars().all(|c| c.is_ascii_alphabetic())
    }) && parts.all(|part| {
        (1..=8).contains(&part.len()) && part.chars().all(|c| c.is_ascii_alphanumeric())
    })
}

/// A wav file as 16 kHz mono 16-bit little-endian samples.
async fn pcm(path: &Path) -> Result<Vec<u8>, String> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let reader = hound::WavReader::open(&path)
            .map_err(|error| format!("cannot read {} as wav: {error}", path.display()))?;
        let spec = reader.spec();
        let failed = |error: hound::Error| format!("cannot read {}: {error}", path.display());
        let samples: Vec<f32> = match spec.sample_format {
            hound::SampleFormat::Float => reader
                .into_samples::<f32>()
                .collect::<Result<_, _>>()
                .map_err(failed)?,
            hound::SampleFormat::Int => {
                let peak = (1_i64 << (spec.bits_per_sample - 1)) as f32;
                reader
                    .into_samples::<i32>()
                    .map(|sample| sample.map(|sample| sample as f32 / peak))
                    .collect::<Result<_, _>>()
                    .map_err(failed)?
            }
        };
        let mono = crate::audio::rechannel(&samples, spec.channels, 1);
        if mono.len() as f64 / f64::from(spec.sample_rate) > MAX_SECONDS {
            return Err(format!("the audio is longer than {MAX_SECONDS} seconds"));
        }
        let resampled = crate::audio::resample(&mono, spec.sample_rate, RATE);
        Ok(resampled
            .iter()
            .flat_map(|&sample| {
                ((sample.clamp(-1.0, 1.0) * f32::from(i16::MAX)).round() as i16).to_le_bytes()
            })
            .collect())
    })
    .await
    .map_err(|error| error.to_string())?
}

/// Claude's speech-to-text over a WebSocket, as Claude Code's voice input.
async fn claude(
    host: &str,
    headers: reqwest::header::HeaderMap,
    language: &str,
    audio: &[u8],
) -> Result<String, String> {
    let query = [
        ("encoding", "linear16"),
        ("sample_rate", "16000"),
        ("channels", "1"),
        ("endpointing_ms", "300"),
        ("utterance_end_ms", "1000"),
        ("language", language),
        ("use_conversation_engine", "true"),
    ]
    .iter()
    .map(|(name, value)| format!("{name}={}", encode(value)))
    .collect::<Vec<_>>()
    .join("&");
    let url = format!("{host}/api/ws/speech_to_text/voice_stream?{query}");
    let mut request = url
        .into_client_request()
        .map_err(|error| format!("bad speech-to-text address: {error}"))?;
    for (name, value) in &headers {
        request.headers_mut().insert(name.clone(), value.clone());
    }
    request
        .headers_mut()
        .insert("x-app", reqwest::header::HeaderValue::from_static("cli"));
    let (socket, _) = tokio_tungstenite::connect_async(request)
        .await
        .map_err(|error| format!("cannot reach Claude's speech-to-text: {error}"))?;
    let (mut sender, mut receiver) = socket.split();
    let send = async move {
        let failed = |error: tokio_tungstenite::tungstenite::Error| error.to_string();
        sender
            .send(Message::text(r#"{"type":"KeepAlive"}"#))
            .await
            .map_err(failed)?;
        let mut last = tokio::time::Instant::now();
        for chunk in audio.chunks(CHUNK) {
            sender
                .send(Message::binary(chunk.to_vec()))
                .await
                .map_err(failed)?;
            if last.elapsed() >= KEEP_ALIVE {
                sender
                    .send(Message::text(r#"{"type":"KeepAlive"}"#))
                    .await
                    .map_err(failed)?;
                last = tokio::time::Instant::now();
            }
        }
        sender
            .send(Message::text(r#"{"type":"CloseStream"}"#))
            .await
            .map_err(failed)?;
        Ok::<_, String>(sender)
    };
    let receive = heard_to_the_end(&mut receiver, WAIT);
    let (sent, heard) = tokio::join!(send, receive);
    let heard = heard?;
    if let Ok(mut sender) = sent {
        let _ = sender.close().await;
    } else if let Err(error) = sent {
        return Err(format!("cannot send the audio: {error}"));
    }
    Ok(heard)
}

/// The text Claude's speech-to-text sends until it closes. One that stops
/// answering for `wait` has not finished, so what it sent is an error, not
/// the transcript.
async fn heard_to_the_end<S>(receiver: &mut S, wait: Duration) -> Result<String, String>
where
    S: futures_util::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    let mut heard = Heard::default();
    loop {
        match tokio::time::timeout(wait, receiver.next()).await {
            Err(_) => {
                return Err(format!(
                    "speech-to-text stopped answering for {} s before it finished; what it \
                     had heard so far, which may be incomplete: {}",
                    wait.as_secs_f64(),
                    heard.text()
                ))
            }
            Ok(None) | Ok(Some(Ok(Message::Close(_)))) => return Ok(heard.text()),
            Ok(Some(Err(error))) => return Err(format!("speech-to-text failed: {error}")),
            Ok(Some(Ok(Message::Text(text)))) => heard.take(text.as_str())?,
            Ok(Some(Ok(_))) => {}
        }
    }
}

/// What Claude's speech-to-text has said: the finished utterances and the
/// one still being heard.
#[derive(Default)]
struct Heard {
    done: Vec<String>,
    pending: String,
}

impl Heard {
    fn take(&mut self, message: &str) -> Result<(), String> {
        let Ok(message) = serde_json::from_str::<serde_json::Value>(message) else {
            return Ok(());
        };
        match message["type"].as_str() {
            Some("TranscriptInterim" | "TranscriptText") => {
                if let Some(text) = message["data"].as_str().filter(|text| !text.is_empty()) {
                    self.pending = text.to_string();
                }
            }
            Some("TranscriptEndpoint") => {
                let text = std::mem::take(&mut self.pending);
                if !text.is_empty() {
                    self.done.push(text);
                }
            }
            Some("TranscriptError") => {
                let why = message["description"]
                    .as_str()
                    .or(message["error_code"].as_str())
                    .unwrap_or("unknown transcription error");
                return Err(format!("speech-to-text failed: {why}"));
            }
            Some("error") => {
                let why = message["message"].as_str().unwrap_or("unknown error");
                return Err(format!("speech-to-text failed: {why}"));
            }
            _ => {}
        }
        Ok(())
    }

    fn text(mut self) -> String {
        if !self.pending.is_empty() {
            self.done.push(std::mem::take(&mut self.pending));
        }
        self.done.join(" ")
    }
}

/// ChatGPT's transcription, as Codex's dictation: the file as the
/// multipart `file` field, answered with `{"text": …}`.
async fn chatgpt(
    host: &str,
    headers: reqwest::header::HeaderMap,
    path: &Path,
) -> Result<String, String> {
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    let mime = match extension.as_str() {
        "wav" => "audio/wav",
        "mp3" => "audio/mpeg",
        "m4a" => "audio/mp4",
        "webm" => "audio/webm",
        "ogg" => "audio/ogg",
        _ => return Err("chatgpt takes wav, mp3, m4a, webm or ogg files".to_string()),
    };
    let size = tokio::fs::metadata(path)
        .await
        .map_err(|error| format!("cannot open {}: {error}", path.display()))?
        .len();
    if size > MAX_BYTES {
        return Err(format!("the file is larger than {MAX_BYTES} bytes"));
    }
    let bytes = tokio::fs::read(path)
        .await
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    let boundary = format!("inocode{:016x}", random());
    let body = multipart(&boundary, &extension, mime, &bytes);
    let response = reqwest::Client::new()
        .post(format!("{host}/transcribe"))
        .headers(headers)
        .header(
            "content-type",
            format!("multipart/form-data; boundary={boundary}"),
        )
        .timeout(Duration::from_secs(120))
        .body(body)
        .send()
        .await
        .map_err(|error| format!("cannot reach ChatGPT's transcription: {error}"))?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!("ChatGPT's transcription answered {status}"));
    }
    #[derive(Deserialize)]
    struct Transcript {
        text: String,
    }
    let transcript: Transcript = response
        .json()
        .await
        .map_err(|error| format!("unreadable transcription: {error}"))?;
    Ok(transcript.text)
}

fn multipart(boundary: &str, extension: &str, mime: &str, bytes: &[u8]) -> Vec<u8> {
    let mut body = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; \
         filename=\"audio.{extension}\"\r\nContent-Type: {mime}\r\n\r\n"
    )
    .into_bytes();
    body.extend_from_slice(bytes);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    body
}

/// The key Chrome's speech recognition is known to send, used by other
/// open-source speech libraries as their default.
const GOOGLE_KEY: &str = "AIzaSyBOti4mM-6x9WDnZIjIeyEU21OpBXqWBgw";

/// The key for Google's speech service: `google_speech_key` in the
/// settings file, else Chrome's.
async fn google_key(settings: &Path) -> Result<String, String> {
    let text = match tokio::fs::read_to_string(settings).await {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(GOOGLE_KEY.to_string());
        }
        Err(error) => return Err(format!("{}: {error}", settings.display())),
    };
    let value: serde_json::Value =
        serde_json::from_str(&text).map_err(|error| format!("{}: {error}", settings.display()))?;
    Ok(value["google_speech_key"]
        .as_str()
        .map(str::trim)
        .filter(|key| !key.is_empty())
        .unwrap_or(GOOGLE_KEY)
        .to_string())
}

/// Google's full-duplex speech service, as Chrome's network speech
/// recognition: the results are read from `down` while the audio, as
/// FLAC, is posted to `up`, the two paired by one id.
async fn google(host: &str, key: &str, language: &str, audio: &[u8]) -> Result<String, String> {
    let samples: Vec<i32> = audio
        .as_chunks::<2>()
        .0
        .iter()
        .map(|&pair| i32::from(i16::from_le_bytes(pair)))
        .collect();
    let flac = crate::audio::flac_bytes(&samples, 1, 16, RATE)?;
    let pair = format!("{:016x}", random());
    let client = reqwest::Client::new();
    let down = client
        .get(format!(
            "{host}/down?key={}&pair={pair}&output=pb",
            encode(key)
        ))
        .timeout(WAIT + Duration::from_secs(60))
        .send();
    let up = client
        .post(format!(
            "{host}/up?app=chromium&key={}&lang={}&output=pb&pFilter=0&pair={pair}&continuous",
            encode(key),
            encode(language)
        ))
        .header("content-type", format!("audio/x-flac; rate={RATE}"))
        .timeout(Duration::from_secs(120))
        .body(flac)
        .send();
    let (down, up) = tokio::join!(down, up);
    let up = up.map_err(|error| format!("cannot reach Google's speech service: {error}"))?;
    if !up.status().is_success() {
        return Err(format!("Google's speech service answered {}", up.status()));
    }
    let down = down.map_err(|error| format!("cannot reach Google's speech service: {error}"))?;
    if !down.status().is_success() {
        return Err(format!(
            "Google's speech service answered {}",
            down.status()
        ));
    }
    let body = down
        .bytes()
        .await
        .map_err(|error| format!("Google's answer was cut: {error}"))?;
    google_text(&body)
}

/// The final transcripts in Google's answer: `SpeechRecognitionEvent`
/// messages, each after its length as 4 big-endian bytes.
fn google_text(body: &[u8]) -> Result<String, String> {
    let mut finals = Vec::new();
    let mut rest = body;
    while rest.len() >= 4 {
        let length = u32::from_be_bytes([rest[0], rest[1], rest[2], rest[3]]) as usize;
        let Some(event) = rest.get(4..4 + length) else {
            return Err("Google's answer was cut".to_string());
        };
        rest = &rest[4 + length..];
        for (field, value) in fields(event)? {
            match (field, value) {
                (1, Field::Number(status)) if status != 0 => {
                    return Err(format!(
                        "Google's speech service refused: {}",
                        status_name(status)
                    ));
                }
                (2, Field::Bytes(result)) => {
                    let mut transcript = None;
                    let mut last = false;
                    for (field, value) in fields(result)? {
                        match (field, value) {
                            (1, Field::Bytes(alternative)) if transcript.is_none() => {
                                for (field, value) in fields(alternative)? {
                                    if let (1, Field::Bytes(text)) = (field, value) {
                                        transcript =
                                            Some(String::from_utf8_lossy(text).to_string());
                                    }
                                }
                            }
                            (2, Field::Number(flag)) => last = flag != 0,
                            _ => {}
                        }
                    }
                    if let Some(transcript) = transcript.filter(|_| last) {
                        finals.push(transcript);
                    }
                }
                _ => {}
            }
        }
    }
    Ok(finals.join(" "))
}

fn status_name(status: u64) -> &'static str {
    match status {
        1 => "no speech",
        2 => "aborted",
        3 => "audio capture",
        4 => "network",
        5 => "not allowed",
        6 => "service not allowed (the key may be wrong)",
        7 => "bad grammar",
        8 => "language not supported",
        _ => "unknown status",
    }
}

enum Field<'a> {
    Number(u64),
    Bytes(&'a [u8]),
}

/// The fields of a protocol buffer message: varints, length-delimited
/// values, and fixed 32 and 64-bit values (whose numbers are not needed
/// here and are skipped).
fn fields(mut message: &[u8]) -> Result<Vec<(u64, Field<'_>)>, String> {
    let broken = || "Google's answer is not readable".to_string();
    let mut fields = Vec::new();
    while !message.is_empty() {
        let (key, rest) = varint(message).ok_or_else(broken)?;
        message = rest;
        let field = key >> 3;
        match key & 7 {
            0 => {
                let (value, rest) = varint(message).ok_or_else(broken)?;
                message = rest;
                fields.push((field, Field::Number(value)));
            }
            2 => {
                let (length, rest) = varint(message).ok_or_else(broken)?;
                let length = usize::try_from(length).map_err(|_| broken())?;
                let value = rest.get(..length).ok_or_else(broken)?;
                message = &rest[length..];
                fields.push((field, Field::Bytes(value)));
            }
            1 => message = message.get(8..).ok_or_else(broken)?,
            5 => message = message.get(4..).ok_or_else(broken)?,
            _ => return Err(broken()),
        }
    }
    Ok(fields)
}

fn varint(bytes: &[u8]) -> Option<(u64, &[u8])> {
    let mut value = 0_u64;
    for (index, &byte) in bytes.iter().enumerate().take(10) {
        value |= u64::from(byte & 0x7f) << (7 * index);
        if byte & 0x80 == 0 {
            return Some((value, &bytes[index + 1..]));
        }
    }
    None
}

fn encode(value: &str) -> String {
    value
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (byte as char).to_string()
            }
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

fn random() -> u64 {
    let mut bytes = [0_u8; 8];
    getrandom::getrandom(&mut bytes).expect("random bytes");
    u64::from_le_bytes(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(fields: &[(u64, Vec<u8>)]) -> Vec<u8> {
        let mut bytes = Vec::new();
        for (field, value) in fields {
            bytes.push((*field << 3 | 2) as u8);
            bytes.push(value.len() as u8);
            bytes.extend_from_slice(value);
        }
        bytes
    }

    fn result(text: &str, last: bool) -> Vec<u8> {
        let alternative = message(&[(1, text.as_bytes().to_vec())]);
        let mut result = message(&[(1, alternative)]);
        result.extend_from_slice(&[2 << 3, u8::from(last)]);
        // A stability, fixed 32 bits, skipped.
        result.extend_from_slice(&[3 << 3 | 5, 0, 0, 0x80, 0x3f]);
        result
    }

    fn framed(events: &[Vec<u8>]) -> Vec<u8> {
        events
            .iter()
            .flat_map(|event| {
                let mut bytes = (event.len() as u32).to_be_bytes().to_vec();
                bytes.extend_from_slice(event);
                bytes
            })
            .collect()
    }

    #[test]
    fn googles_final_results_are_read_and_its_refusals_named() {
        let body = framed(&[
            Vec::new(),
            message(&[(2, result("hello wor", false))]),
            message(&[(2, result("hello world", true))]),
            message(&[(2, result("again", true))]),
        ]);
        assert_eq!(google_text(&body).expect("text"), "hello world again");
        let refused = framed(&[vec![1 << 3, 6]]);
        assert!(google_text(&refused)
            .unwrap_err()
            .contains("service not allowed"));
        assert!(google_text(&[0, 0, 0, 9, 1]).is_err());
    }

    #[test]
    fn claudes_messages_make_the_text() {
        let mut heard = Heard::default();
        for message in [
            r#"{"type":"TranscriptInterim","data":"hel"}"#,
            r#"{"type":"TranscriptText","data":"hello"}"#,
            r#"{"type":"TranscriptEndpoint"}"#,
            r#"{"type":"TranscriptInterim","data":"and more"}"#,
        ] {
            heard.take(message).expect("take");
        }
        assert_eq!(heard.text(), "hello and more");
        let mut failed = Heard::default();
        let error = failed
            .take(r#"{"type":"TranscriptError","description":"bad audio"}"#)
            .unwrap_err();
        assert!(error.contains("bad audio"));
    }

    #[tokio::test]
    async fn a_service_that_stops_answering_gives_no_transcript() {
        let said = || {
            futures_util::stream::iter([Ok(Message::text(
                r#"{"type":"TranscriptText","data":"half a"}"#,
            ))])
        };
        let mut silent = said().chain(futures_util::stream::pending());
        let error = heard_to_the_end(&mut silent, Duration::from_millis(50))
            .await
            .expect_err("it never closed");
        assert!(error.contains("may be incomplete: half a"), "{error}");
        let mut closed = said().chain(futures_util::stream::iter([Ok(Message::Close(None))]));
        assert_eq!(
            heard_to_the_end(&mut closed, Duration::from_millis(50)).await,
            Ok("half a".to_string())
        );
    }

    #[test]
    fn only_language_tags_are_taken_as_the_spoken_language() {
        for tag in ["ja", "ja-JP", "en_US", "zh-Hant-TW"] {
            assert!(is_tag(tag), "{tag}");
        }
        for name in ["Japanese", "日本語", "", "j", "ja-"] {
            assert!(!is_tag(name), "{name}");
        }
    }

    #[tokio::test]
    async fn the_settings_language_is_the_default_when_it_is_a_tag() {
        let dir = tempfile::tempdir().expect("dir");
        let settings = dir.path().join("settings.json");
        std::fs::write(&settings, r#"{"language": "ja-JP"}"#).expect("write");
        assert_eq!(default_language(&settings).await.as_deref(), Some("ja-JP"));
        std::fs::write(&settings, r#"{"language": "Japanese"}"#).expect("write");
        assert_eq!(
            default_language(&settings).await,
            crate::language::system().filter(|language| is_tag(language))
        );
    }

    #[tokio::test]
    async fn a_wav_file_is_read_as_sixteen_bit_samples() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("a.wav");
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: 32_000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(&path, spec).expect("create");
        for _ in 0..3_200 {
            writer.write_sample(i16::MAX / 2).expect("write");
            writer.write_sample(i16::MAX / 2).expect("write");
        }
        writer.finalize().expect("finalize");
        let bytes = pcm(&path).await.expect("pcm");
        assert_eq!(bytes.len(), 1_600 * 2);
        let first = i16::from_le_bytes([bytes[0], bytes[1]]);
        assert!(
            (i32::from(first) - i32::from(i16::MAX / 2)).abs() <= 1,
            "{first}"
        );
    }

    #[test]
    fn chatgpt_gets_the_file_as_a_form_field() {
        let body = multipart("B", "wav", "audio/wav", b"RIFF");
        assert_eq!(
            String::from_utf8(body).expect("text"),
            "--B\r\nContent-Disposition: form-data; name=\"file\"; filename=\"audio.wav\"\r\n\
             Content-Type: audio/wav\r\n\r\nRIFF\r\n--B--\r\n"
        );
        assert_eq!(encode("ja-JP & x"), "ja-JP%20%26%20x");
    }

    #[tokio::test]
    async fn the_settings_key_comes_before_chromes() {
        let dir = tempfile::tempdir().expect("dir");
        let settings = dir.path().join("settings.json");
        assert_eq!(google_key(&settings).await.expect("key"), GOOGLE_KEY);
        std::fs::write(&settings, r#"{"language": "ja"}"#).expect("write");
        assert_eq!(google_key(&settings).await.expect("key"), GOOGLE_KEY);
        std::fs::write(&settings, r#"{"google_speech_key": " k1 "}"#).expect("write");
        assert_eq!(google_key(&settings).await.expect("key"), "k1");
    }

    /// Sends the call in INOCODE_TRANSCRIBE_CALL (the tool's arguments) to
    /// the real service, with the sign-ins and settings in ~/.inocode.
    #[tokio::test]
    #[ignore = "sends audio to a real service with the person's sign-in"]
    async fn transcribes_here() {
        let call = std::env::var("INOCODE_TRANSCRIBE_CALL").expect("INOCODE_TRANSCRIBE_CALL");
        let accounts = crate::oauth::Store::default_path().expect("home");
        let settings = accounts.with_file_name("settings.json");
        let text = run(&call, None, &accounts, &settings, &Hosts::default()).await;
        println!("{text:?}");
    }
}
