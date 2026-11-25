//! Translation of text into another language, for any part of inocode that
//! shows text in the person's language: the services the browsers' page
//! translation uses or Firefox's and Chrome's models run here offline, code
//! left as written, and text that streams in translated in order as it
//! comes.
//! Wire facts and the choices made: docs/evidence/translation.md.

mod chromium;
mod firefox;
mod marian;
mod sentencepiece;
mod tflite;

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::ErrorInfo;

/// The key in Google's published page-translation script.
const GOOGLE_KEY: &str = "AIzaSyATBXajvzQLTDHEQbcpq0Ihe0vWDHmO520";
const WAIT: Duration = Duration::from_secs(20);
/// The longest run of text without a sentence end that waits for one.
const LONGEST_CHUNK: usize = 400;

/// A translation service.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Service {
    Edge,
    Google,
    Firefox,
    Chromium,
}

impl Service {
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "edge" => Some(Self::Edge),
            "google" => Some(Self::Google),
            "firefox" => Some(Self::Firefox),
            "chromium" => Some(Self::Chromium),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Edge => "edge",
            Self::Google => "google",
            Self::Firefox => "firefox",
            Self::Chromium => "chromium",
        }
    }
}

/// Where the services are; the defaults are the real ones.
#[derive(Debug, Clone)]
pub struct Hosts {
    pub edge: String,
    pub google: String,
    /// The Remote Settings server Firefox's models are listed on.
    pub firefox: String,
    /// The component update server Chrome's language packs come from.
    pub chromium: String,
}

impl Default for Hosts {
    fn default() -> Self {
        Self {
            edge: "https://edge.microsoft.com/translate/translatetext".into(),
            google: "https://translate-pa.googleapis.com/v1/translateHtml".into(),
            firefox: firefox::SETTINGS.into(),
            chromium: chromium::UPDATE.into(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Translator {
    service: Service,
    google_key: String,
    hosts: Hosts,
    client: reqwest::Client,
    /// Firefox's models, kept on disk and, once loaded, in memory.
    firefox: firefox::Firefox,
    /// Chrome's language packs, kept on disk and, once loaded, in memory.
    chromium: chromium::Chromium,
}

impl Translator {
    /// `models` is the folder Firefox's models are kept in; Chrome's packs
    /// go in its `chromium` folder.
    pub fn new(
        service: Service,
        google_key: Option<String>,
        hosts: Hosts,
        models: PathBuf,
    ) -> Self {
        let client = reqwest::Client::builder()
            // Edge's service refuses requests without one.
            .user_agent(concat!("inocode/", env!("CARGO_PKG_VERSION")))
            .build()
            .unwrap_or_default();
        Self {
            service,
            google_key: google_key.unwrap_or_else(|| GOOGLE_KEY.to_string()),
            chromium: chromium::Chromium::new(
                models.join("chromium"),
                hosts.chromium.clone(),
                client.clone(),
            ),
            firefox: firefox::Firefox::new(models, hosts.firefox.clone(), client.clone()),
            hosts,
            client,
        }
    }

    /// Each of `texts` in the language `to` (a tag such as `ja`), in the
    /// same order; code in backticks is left as written.
    /// Gets ready to translate from `from` to `to`: Firefox's models and
    /// Chrome's packs are fetched now rather than with the first text; the
    /// services need nothing.
    pub async fn prepare(&self, from: &str, to: &str) -> Result<(), ErrorInfo> {
        match self.service {
            Service::Firefox => self.firefox.prepare(from, to).await,
            Service::Chromium => self.chromium.prepare(from, to).await,
            Service::Edge | Service::Google => Ok(()),
        }
    }

    /// A text whose script shows it is already in `to` is given back as it
    /// is, without being sent.
    pub async fn translate(&self, texts: &[String], to: &str) -> Result<Vec<String>, ErrorInfo> {
        let target = primary(to);
        let mut out: Vec<Option<String>> = texts
            .iter()
            .map(|text| {
                script_language(text)
                    .is_some_and(|language| primary(language) == target)
                    .then(|| text.clone())
            })
            .collect();
        let sent: Vec<String> = texts
            .iter()
            .zip(&out)
            .filter(|(_, kept)| kept.is_none())
            .map(|(text, _)| text.clone())
            .collect();
        if !sent.is_empty() {
            let translated = match self.service {
                Service::Edge | Service::Firefox | Service::Chromium => {
                    self.around_code(&sent, to).await?
                }
                Service::Google => self.with_code_marked(&sent, to).await?,
            };
            let mut translated = translated.into_iter();
            for slot in out.iter_mut().filter(|slot| slot.is_none()) {
                *slot = translated.next();
            }
        }
        Ok(out.into_iter().map(Option::unwrap_or_default).collect())
    }

    /// Google leaves text in a `notranslate` span as it is, so each text
    /// goes whole with its code marked and the code is put back from the
    /// marks; a text whose marks do not all come back is translated
    /// around its code instead.
    async fn with_code_marked(&self, texts: &[String], to: &str) -> Result<Vec<String>, ErrorInfo> {
        let split: Vec<Vec<Segment>> = texts.iter().map(|text| segments(text)).collect();
        let marked: Vec<String> = split.iter().map(|segments| mark(segments)).collect();
        let answers = self.google(&marked, to).await?;
        let mut out = Vec::with_capacity(texts.len());
        let mut redo = Vec::new();
        for (index, (answer, segments)) in answers.iter().zip(&split).enumerate() {
            match unmark(answer, marks(segments)) {
                Some(text) => out.push(text),
                None => {
                    out.push(String::new());
                    redo.push(index);
                }
            }
        }
        if !redo.is_empty() {
            let again: Vec<String> = redo.iter().map(|&index| texts[index].clone()).collect();
            for (index, text) in redo.into_iter().zip(self.around_code(&again, to).await?) {
                out[index] = text;
            }
        }
        Ok(out)
    }

    /// Translates the text between pieces of code, each run on its own,
    /// and puts the code back between them as written.
    async fn around_code(&self, texts: &[String], to: &str) -> Result<Vec<String>, ErrorInfo> {
        let split: Vec<Vec<Segment>> = texts.iter().map(|text| segments(text)).collect();
        let sent: Vec<String> = split
            .iter()
            .flatten()
            .filter(|segment| segment.translate)
            .map(|segment| segment.text.trim().to_string())
            .collect();
        let mut translated = if sent.is_empty() {
            Vec::new()
        } else {
            match self.service {
                Service::Edge => self.edge(&sent, to).await?,
                Service::Firefox => self.firefox.translate(&sent, to).await?,
                Service::Chromium => self.chromium.translate(&sent, to).await?,
                Service::Google => {
                    let escaped: Vec<String> = sent.iter().map(|text| escape(text)).collect();
                    let answers = self.google(&escaped, to).await?;
                    answers.iter().map(|answer| unescape(answer)).collect()
                }
            }
        }
        .into_iter();
        Ok(split
            .into_iter()
            .map(|segments| {
                segments
                    .into_iter()
                    .map(|segment| {
                        if !segment.translate {
                            return segment.text;
                        }
                        let text = translated.next().unwrap_or_default();
                        let lead =
                            &segment.text[..segment.text.len() - segment.text.trim_start().len()];
                        let trail = &segment.text[segment.text.trim_end().len()..];
                        format!("{lead}{text}{trail}")
                    })
                    .collect()
            })
            .collect())
    }

    async fn edge(&self, texts: &[String], to: &str) -> Result<Vec<String>, ErrorInfo> {
        let to = match to {
            "zh-CN" | "zh-SG" | "zh" => "zh-Hans",
            "zh-TW" | "zh-HK" | "zh-MO" => "zh-Hant",
            other => other,
        };
        let response = self
            .client
            .post(&self.hosts.edge)
            .query(&[("from", ""), ("to", to), ("isEnterpriseClient", "false")])
            .json(texts)
            .timeout(WAIT)
            .send()
            .await
            .map_err(failed)?;
        let value = answer(response).await?;
        let items = value.as_array().ok_or_else(|| unexpected(&value))?;
        if items.len() != texts.len() {
            return Err(unexpected(&value));
        }
        items
            .iter()
            .map(|item| {
                item["translations"][0]["text"]
                    .as_str()
                    .map(str::to_string)
                    .ok_or_else(|| unexpected(item))
            })
            .collect()
    }

    /// `html` translated, as Google answers it: still HTML.
    async fn google(&self, html: &[String], to: &str) -> Result<Vec<String>, ErrorInfo> {
        let to = match to {
            "zh-Hans" | "zh" => "zh-CN",
            "zh-Hant" => "zh-TW",
            other => other,
        };
        let response = self
            .client
            .post(&self.hosts.google)
            .header("Content-Type", "application/json+protobuf")
            .header("X-Goog-API-Key", &self.google_key)
            .body(json!([[html, "auto", to], "te_lib"]).to_string())
            .timeout(WAIT)
            .send()
            .await
            .map_err(failed)?;
        let value = answer(response).await?;
        let items = value[0].as_array().ok_or_else(|| unexpected(&value))?;
        if items.len() != html.len() {
            return Err(unexpected(&value));
        }
        items
            .iter()
            .map(|item| {
                item.as_str()
                    .map(str::to_string)
                    .ok_or_else(|| unexpected(item))
            })
            .collect()
    }
}

/// The language of `text` when its script tells it: Japanese for kana,
/// Korean for Hangul, Simplified Chinese for other Han characters; `None`
/// when there are none or Latin letters outnumber them three to one (a
/// word of a Latin-script language takes several letters where these take
/// one or two characters), since the script does not tell Latin-script
/// languages apart (docs/evidence/translation.md).
pub(crate) fn script_language(text: &str) -> Option<&'static str> {
    let (mut kana, mut hangul, mut han, mut latin) = (0, 0, 0, 0);
    for char in text.chars() {
        match char as u32 {
            0x3040..=0x30FF | 0x31F0..=0x31FF | 0xFF66..=0xFF9F => kana += 1,
            0x1100..=0x11FF | 0x3130..=0x318F | 0xAC00..=0xD7AF => hangul += 1,
            0x3400..=0x4DBF | 0x4E00..=0x9FFF => han += 1,
            _ if char.is_ascii_alphabetic() => latin += 1,
            _ => {}
        }
    }
    let written = kana + hangul + han;
    if written == 0 || written * 3 < latin {
        None
    } else if kana > 0 {
        Some("ja")
    } else if hangul > 0 {
        Some("ko")
    } else {
        Some("zh-Hans")
    }
}

/// The language subtag of `tag`: `zh` for `zh-Hant`.
fn primary(tag: &str) -> &str {
    tag.split(['-', '_']).next().unwrap_or(tag)
}

/// Google answers 502 to a quoted attribute, so the class is unquoted.
const MARK_OPEN: &str = "<span class=notranslate>";
const MARK_CLOSE: &str = "</span>";

/// How many pieces of code `segments` holds.
fn marks(segments: &[Segment]) -> usize {
    segments
        .iter()
        .filter(|segment| !segment.translate && !segment.text.trim().is_empty())
        .count()
}

/// The text as HTML with each piece of code in a mark.
fn mark(segments: &[Segment]) -> String {
    segments
        .iter()
        .map(|segment| {
            if segment.translate || segment.text.trim().is_empty() {
                escape(&segment.text)
            } else {
                format!("{MARK_OPEN}{}{MARK_CLOSE}", escape(&segment.text))
            }
        })
        .collect()
}

/// The translated HTML as text, the code taken from the marks; `None`
/// unless exactly `marks` marks came back.
fn unmark(html: &str, marks: usize) -> Option<String> {
    let mut out = String::new();
    let mut rest = html;
    let mut found = 0;
    while let Some(start) = rest.find(MARK_OPEN) {
        let inner = &rest[start + MARK_OPEN.len()..];
        let end = inner.find(MARK_CLOSE)?;
        out.push_str(&unescape(&rest[..start]));
        out.push_str(&unescape(&inner[..end]));
        rest = &inner[end + MARK_CLOSE.len()..];
        found += 1;
    }
    out.push_str(&unescape(rest));
    (found == marks).then_some(out)
}

fn failed(error: reqwest::Error) -> ErrorInfo {
    ErrorInfo::new("translation_failed", error.to_string())
}

fn unexpected(value: &Value) -> ErrorInfo {
    let mut shown = value.to_string();
    shown.truncate(shown.floor_char_boundary(300));
    ErrorInfo::new("translation_failed", format!("unexpected answer: {shown}"))
}

async fn answer(response: reqwest::Response) -> Result<Value, ErrorInfo> {
    let status = response.status();
    let text = response.text().await.map_err(failed)?;
    if !status.is_success() {
        let mut text = text;
        text.truncate(text.floor_char_boundary(300));
        return Err(ErrorInfo::new(
            "translation_failed",
            format!("HTTP {status}: {text}"),
        ));
    }
    serde_json::from_str(&text)
        .map_err(|error| ErrorInfo::new("translation_failed", error.to_string()))
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn unescape(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
}

/// A run of text, to translate or to keep as written.
#[derive(Debug, PartialEq)]
struct Segment {
    text: String,
    translate: bool,
}

/// `text` cut into code, in ``` fences or backticks, and the text around
/// it; runs of only whitespace are kept as written.
fn segments(text: &str) -> Vec<Segment> {
    let mut out: Vec<Segment> = Vec::new();
    let mut push = |text: &str, code: bool| {
        if text.is_empty() {
            return;
        }
        let translate = !code && !text.trim().is_empty();
        match out.last_mut() {
            Some(last) if last.translate == translate => last.text.push_str(text),
            _ => out.push(Segment {
                text: text.to_string(),
                translate,
            }),
        }
    };
    let mut rest = text;
    while let Some(start) = rest.find('`') {
        let fence = if rest[start..].starts_with("```") {
            "```"
        } else {
            "`"
        };
        let Some(length) = rest[start + fence.len()..].find(fence) else {
            break;
        };
        let end = start + fence.len() + length + fence.len();
        push(&rest[..start], false);
        push(&rest[start..end], true);
        rest = &rest[end..];
    }
    push(rest, false);
    out
}

/// Cuts streamed text into pieces worth translating on their own: up to
/// a sentence end, outside code.
#[derive(Debug, Default)]
pub struct Chunker {
    pending: String,
}

impl Chunker {
    /// Adds `text` and gives back what is ready.
    pub fn push(&mut self, text: &str) -> Option<String> {
        self.pending.push_str(text);
        let end = self.cut()?;
        let rest = self.pending.split_off(end);
        Some(std::mem::replace(&mut self.pending, rest))
    }

    /// What is left at the end of the stream.
    pub fn finish(&mut self) -> Option<String> {
        (!self.pending.is_empty()).then(|| std::mem::take(&mut self.pending))
    }

    /// Where the ready text ends: after the last sentence end that is
    /// outside code, else, past the longest chunk, after the last space.
    fn cut(&self) -> Option<usize> {
        let text = &self.pending;
        let (mut sentence, mut space) = (None, None);
        let mut in_fence = false;
        let mut in_code = false;
        let mut chars = text.char_indices().peekable();
        while let Some((at, char)) = chars.next() {
            if text[at..].starts_with("```") {
                in_fence = !in_fence;
                chars.next();
                chars.next();
                continue;
            }
            if char == '`' && !in_fence {
                in_code = !in_code;
                continue;
            }
            if in_fence || in_code {
                continue;
            }
            let end = at + char.len_utf8();
            let next = chars.peek().map(|&(_, next)| next);
            match char {
                '\n' | '。' | '！' | '？' => sentence = Some(end),
                '.' | '!' | '?' if next.is_some_and(char::is_whitespace) => sentence = Some(end),
                _ if char.is_whitespace() => space = Some(end),
                _ => {}
            }
        }
        sentence.or(space.filter(|_| text.len() > LONGEST_CHUNK))
    }
}

/// A translated piece of a stream.
#[derive(Debug, Clone, PartialEq)]
pub struct Translated {
    pub text: String,
    /// Why the piece could not be translated; `text` is then the source.
    pub error: Option<ErrorInfo>,
}

/// The most ready pieces a stream keeps waiting for a translation; older
/// ones go out as written, so a slow service never holds the whole stream.
const MOST_WAITING: usize = 32;

/// What a stream holds that has not gone out yet.
#[derive(Default)]
struct Backlog {
    chunker: Chunker,
    ready: std::collections::VecDeque<String>,
    /// The pieces being translated, in order, until each goes out.
    in_flight: std::collections::VecDeque<String>,
    closed: bool,
}

type Shared = std::sync::Arc<std::sync::Mutex<Backlog>>;

fn lock(backlog: &Shared) -> std::sync::MutexGuard<'_, Backlog> {
    backlog
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Translates text pushed in as it streams, sending each piece, wrapped
/// by `wrap`, to `out` in the order it came. Pieces that pile up while a
/// translation is under way go together in the next one. Pushing never
/// waits on the translation.
pub struct Streamer<T> {
    backlog: Shared,
    wake: std::sync::Arc<tokio::sync::Notify>,
    task: JoinHandle<()>,
    out: mpsc::Sender<T>,
    wrap: fn(Translated) -> T,
}

impl<T: Send + 'static> Streamer<T> {
    pub fn start(
        translator: std::sync::Arc<Translator>,
        to: String,
        out: mpsc::Sender<T>,
        wrap: fn(Translated) -> T,
    ) -> Self {
        let backlog = Shared::default();
        let wake = std::sync::Arc::new(tokio::sync::Notify::new());
        let task = tokio::spawn(work(
            translator,
            to,
            backlog.clone(),
            wake.clone(),
            out.clone(),
            wrap,
        ));
        Self {
            backlog,
            wake,
            task,
            out,
            wrap,
        }
    }

    pub fn push(&self, text: &str) {
        let mut backlog = lock(&self.backlog);
        if let Some(piece) = backlog.chunker.push(text) {
            backlog.ready.push_back(piece);
        }
        drop(backlog);
        self.wake.notify_one();
    }

    /// Translates what is left and waits for it, at most `wait`; what is
    /// still untranslated then goes out as written.
    pub async fn finish(mut self, wait: Duration) {
        lock(&self.backlog).closed = true;
        self.wake.notify_one();
        if tokio::time::timeout(wait, &mut self.task).await.is_ok() {
            return;
        }
        self.task.abort();
        let left: Vec<String> = {
            let mut backlog = lock(&self.backlog);
            let mut left: Vec<String> = backlog.in_flight.drain(..).collect();
            left.extend(backlog.ready.drain(..));
            left.extend(backlog.chunker.finish());
            left
        };
        let error = ErrorInfo::new(
            "translation_timeout",
            format!("not translated within {} seconds", wait.as_secs()),
        );
        for text in left {
            let piece = Translated {
                text,
                error: Some(error.clone()),
            };
            if self.out.send((self.wrap)(piece)).await.is_err() {
                return;
            }
        }
    }
}

impl<T> Drop for Streamer<T> {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn work<T: Send + 'static>(
    translator: std::sync::Arc<Translator>,
    to: String,
    backlog: Shared,
    wake: std::sync::Arc<tokio::sync::Notify>,
    out: mpsc::Sender<T>,
    wrap: fn(Translated) -> T,
) {
    loop {
        let (behind, batch, closed) = {
            let mut backlog = lock(&backlog);
            if backlog.closed {
                let rest = backlog.chunker.finish();
                backlog.ready.extend(rest);
            }
            let extra = backlog.ready.len().saturating_sub(MOST_WAITING);
            let behind: Vec<String> = backlog.ready.drain(..extra).collect();
            let batch: Vec<String> = backlog.ready.drain(..).collect();
            backlog.in_flight = batch.iter().cloned().collect();
            (behind, batch, backlog.closed)
        };
        let error = ErrorInfo::new(
            "translation_behind",
            "the translation fell too far behind the stream",
        );
        for text in behind {
            let piece = Translated {
                text,
                error: Some(error.clone()),
            };
            if out.send(wrap(piece)).await.is_err() {
                return;
            }
        }
        if batch.is_empty() {
            if closed {
                return;
            }
            wake.notified().await;
            continue;
        }
        let translated: Vec<Translated> = match translator.translate(&batch, &to).await {
            Ok(texts) => texts
                .into_iter()
                .map(|text| Translated { text, error: None })
                .collect(),
            Err(error) => batch
                .into_iter()
                .map(|text| Translated {
                    text,
                    error: Some(error.clone()),
                })
                .collect(),
        };
        for piece in translated {
            if out.send(wrap(piece)).await.is_err() {
                return;
            }
            lock(&backlog).in_flight.pop_front();
        }
    }
}

/// Bumped whenever the settings are written, so that a running turn sees
/// a change at once.
static SETTINGS_WRITTEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// How many times the settings were written by this process.
pub(crate) fn settings_written() -> u64 {
    SETTINGS_WRITTEN.load(std::sync::atomic::Ordering::SeqCst)
}

/// The translator for `service`, `google_key` and `models`, made once per
/// process so that loaded models and open connections are kept.
pub(crate) fn shared(
    service: Service,
    google_key: Option<String>,
    models: PathBuf,
) -> std::sync::Arc<Translator> {
    type Key = (Service, Option<String>, PathBuf);
    type Made = std::sync::Mutex<std::collections::HashMap<Key, std::sync::Arc<Translator>>>;
    static MADE: std::sync::OnceLock<Made> = std::sync::OnceLock::new();
    let mut made = MADE
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    made.entry((service, google_key.clone(), models.clone()))
        .or_insert_with(|| {
            std::sync::Arc::new(Translator::new(
                service,
                google_key,
                Hosts::default(),
                models,
            ))
        })
        .clone()
}

/// The `translation` settings of `settings.json`.
#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    pub thinking: bool,
    pub service: Service,
    pub to: Option<String>,
    pub google_key: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            thinking: false,
            service: Service::Google,
            to: None,
            google_key: None,
        }
    }
}

impl Settings {
    pub(crate) fn from_value(value: &Value) -> Result<Self, String> {
        let mut settings = Self::default();
        let Some(section) = value.get("translation") else {
            return Ok(settings);
        };
        if section.is_null() {
            return Ok(settings);
        }
        let section = section
            .as_object()
            .ok_or("\"translation\" is not an object")?;
        let text = |name: &str| -> Result<Option<String>, String> {
            match section.get(name) {
                None | Some(Value::Null) => Ok(None),
                Some(Value::String(text)) if text.trim().is_empty() => Ok(None),
                Some(Value::String(text)) => Ok(Some(text.trim().to_string())),
                Some(_) => Err(format!("\"translation.{name}\" is not a string")),
            }
        };
        match section.get("thinking") {
            None | Some(Value::Null) => {}
            Some(Value::Bool(on)) => settings.thinking = *on,
            Some(_) => return Err("\"translation.thinking\" is not true or false".into()),
        }
        if let Some(name) = text("service")? {
            settings.service = Service::parse(&name)
                .ok_or_else(|| format!("unknown translation service {name:?}"))?;
        }
        settings.to = text("to")?;
        settings.google_key = text("google_key")?;
        Ok(settings)
    }

    /// The settings in the file at `path`; the defaults when it is missing.
    pub(crate) async fn read(path: &Path) -> Result<Self, String> {
        match tokio::fs::read_to_string(path).await {
            Ok(text) => {
                let value: Value =
                    serde_json::from_str(&text).map_err(|error| error.to_string())?;
                Self::from_value(&value)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error.to_string()),
        }
    }
}

/// The language to translate into: the setting, else the reply language
/// when it is written as a language tag, else the system's language.
pub(crate) fn target(settings: &Settings, language: Option<&str>) -> Option<String> {
    settings
        .to
        .clone()
        .or_else(|| language.and_then(tag))
        .or_else(|| crate::language::system().as_deref().and_then(tag))
}

/// `name` as a translation language tag when it is one, such as `ja` for
/// `ja-JP`; Chinese keeps its script or region.
fn tag(name: &str) -> Option<String> {
    let parts: Vec<&str> = name.split(['-', '_']).collect();
    let primary = parts[0];
    if !(2..=3).contains(&primary.len()) || !primary.chars().all(|c| c.is_ascii_alphabetic()) {
        return None;
    }
    let primary = primary.to_ascii_lowercase();
    if primary == "zh" {
        if let Some(rest) = parts.get(1) {
            return Some(format!("zh-{rest}"));
        }
    }
    Some(primary)
}

/// Writes `patch` into the `translation` settings of the file at `path`,
/// leaving the rest of the file as it is, and gives back the result.
pub(crate) async fn update(path: &Path, patch: &Value) -> Result<Settings, String> {
    let patch = patch.as_object().ok_or("the settings are not an object")?;
    let mut value = match tokio::fs::read_to_string(path).await {
        Ok(text) => serde_json::from_str(&text).map_err(|error| error.to_string())?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => json!({}),
        Err(error) => return Err(error.to_string()),
    };
    let file = value
        .as_object_mut()
        .ok_or("the settings file is not an object")?;
    let section = file.entry("translation").or_insert_with(|| json!({}));
    if section.is_null() {
        *section = json!({});
    }
    let section = section
        .as_object_mut()
        .ok_or("\"translation\" is not an object")?;
    for (name, field) in patch {
        section.insert(name.clone(), field.clone());
    }
    let settings = Settings::from_value(&value)?;
    let text = serde_json::to_string_pretty(&value).map_err(|error| error.to_string())?;
    let temporary = path.with_extension("json.tmp");
    tokio::fs::write(&temporary, text)
        .await
        .map_err(|error| error.to_string())?;
    tokio::fs::rename(&temporary, path)
        .await
        .map_err(|error| error.to_string())?;
    SETTINGS_WRITTEN.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    Ok(settings)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marked_code_comes_back_as_written() {
        let parts = segments("Use `a && b <x>` and `c`.");
        let html = mark(&parts);
        assert_eq!(
            html,
            "Use <span class=notranslate>`a &amp;&amp; b &lt;x&gt;`</span> and \
             <span class=notranslate>`c`</span>."
        );
        let answer = "<span class=notranslate>`c`</span> と \
                      <span class=notranslate>`a &amp;&amp; b &lt;x&gt;`</span> を使う &amp; 終わり";
        assert_eq!(
            unmark(answer, marks(&parts)).as_deref(),
            Some("`c` と `a && b <x>` を使う & 終わり")
        );
        assert_eq!(unmark("<span class=notranslate>`c`</span> だけ", 2), None);
        assert_eq!(unmark("切れた <span class=notranslate>`c`", 1), None);
    }

    #[test]
    fn code_is_kept_apart_from_the_text() {
        let parts = segments("Run `cargo test` now.\n```rust\nfn a() {}\n```\nDone ` open");
        let shown: Vec<(&str, bool)> = parts
            .iter()
            .map(|part| (part.text.as_str(), part.translate))
            .collect();
        assert_eq!(
            shown,
            [
                ("Run ", true),
                ("`cargo test`", false),
                (" now.\n", true),
                ("```rust\nfn a() {}\n```", false),
                ("\nDone ` open", true),
            ]
        );
        assert_eq!(
            segments("  \n"),
            [Segment {
                text: "  \n".into(),
                translate: false
            }]
        );
    }

    #[test]
    fn streamed_text_is_cut_at_sentence_ends_outside_code() {
        let mut chunker = Chunker::default();
        assert_eq!(chunker.push("I need to che"), None);
        assert_eq!(
            chunker.push("ck a.rs. Then"),
            Some("I need to check a.rs.".into())
        );
        assert_eq!(chunker.push(" run `x. y` ok"), None);
        assert_eq!(
            chunker.push("\n```\nfoo. bar\n"),
            Some(" Then run `x. y` ok\n".into())
        );
        assert_eq!(chunker.push("```\n"), Some("```\nfoo. bar\n```\n".into()));
        assert_eq!(chunker.push("考える。次"), Some("考える。".into()));
        assert_eq!(chunker.finish(), Some("次".into()));
        assert_eq!(chunker.finish(), None);
        let long = "word ".repeat(100);
        assert_eq!(chunker.push(&long).map(|text| text.len()), Some(500));
    }

    #[test]
    fn settings_are_read_and_checked() {
        assert_eq!(Settings::from_value(&json!({})), Ok(Settings::default()));
        let settings = Settings::from_value(&json!({"translation": {
            "thinking": true, "service": "google", "to": " ja ", "google_key": ""}}))
        .unwrap();
        assert_eq!(
            settings,
            Settings {
                thinking: true,
                service: Service::Google,
                to: Some("ja".into()),
                google_key: None,
            }
        );
        assert!(Settings::from_value(&json!({"translation": {"service": "deepl"}})).is_err());
        let firefox = Settings::from_value(&json!({"translation": {"service": "firefox"}}));
        assert_eq!(firefox.unwrap().service, Service::Firefox);
        let chromium = Settings::from_value(&json!({"translation": {"service": "chromium"}}));
        assert_eq!(chromium.unwrap().service, Service::Chromium);
        assert!(Settings::from_value(&json!({"translation": {"thinking": "yes"}})).is_err());
        assert!(Settings::from_value(&json!({"translation": 3})).is_err());
    }

    #[test]
    fn the_script_tells_only_the_languages_it_can() {
        assert_eq!(script_language("まず src/main.rs を確認する。"), Some("ja"));
        assert_eq!(script_language("設定を確認する。"), Some("ja"));
        assert_eq!(script_language("需要检查代码。"), Some("zh-Hans"));
        assert_eq!(script_language("사용자가 코드를 고친다."), Some("ko"));
        assert_eq!(script_language("Ich muss die Datei prüfen."), None);
        assert_eq!(script_language("Run the 测试 suite now"), None);
        assert_eq!(script_language("12:25"), None);
        assert_eq!(primary("zh-Hant"), "zh");
    }

    #[test]
    fn the_target_comes_from_the_setting_then_the_reply_language() {
        let mut settings = Settings::default();
        assert_eq!(target(&settings, Some("ja-JP")).as_deref(), Some("ja"));
        assert_eq!(target(&settings, Some("zh_TW")).as_deref(), Some("zh-TW"));
        settings.to = Some("de".into());
        assert_eq!(target(&settings, Some("ja-JP")).as_deref(), Some("de"));
        assert_eq!(tag("Japanese"), None);
        assert_eq!(tag("ja"), Some("ja".into()));
    }

    #[tokio::test]
    async fn an_update_keeps_the_rest_of_the_file() {
        let folder = std::env::temp_dir().join(format!("inocode-translate-{}", std::process::id()));
        std::fs::create_dir_all(&folder).unwrap();
        let path = folder.join("settings.json");
        std::fs::write(
            &path,
            r#"{"language": "ja-JP", "translation": {"service": "google"}}"#,
        )
        .unwrap();
        let settings = update(&path, &json!({"thinking": true})).await.unwrap();
        assert!(settings.thinking);
        assert_eq!(settings.service, Service::Google);
        let value: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(value["language"], "ja-JP");
        assert!(update(&path, &json!({"service": "nope"})).await.is_err());
        let value: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(value["translation"]["service"], "google");
        std::fs::remove_dir_all(&folder).unwrap();
    }

    /// A stand-in for Edge's service that answers each text in capitals,
    /// or fails with 500 when a text says "fail"; gives back its address
    /// and the number of requests it had.
    async fn edge_stand_in() -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = format!("http://{}/translate", listener.local_addr().unwrap());
        let count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = count.clone();
        tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let mut request = Vec::new();
                let body = loop {
                    let mut buffer = [0u8; 4096];
                    let read = socket.read(&mut buffer).await.unwrap();
                    assert!(read > 0, "the request ended early");
                    request.extend_from_slice(&buffer[..read]);
                    let text = String::from_utf8_lossy(&request).to_string();
                    if let Some((head, body)) = text.split_once("\r\n\r\n") {
                        let length: usize = head
                            .lines()
                            .find_map(|line| {
                                line.to_ascii_lowercase()
                                    .strip_prefix("content-length: ")
                                    .map(|n| n.parse().unwrap())
                            })
                            .unwrap_or(0);
                        if body.len() >= length {
                            break body.to_string();
                        }
                    }
                };
                let texts: Vec<String> = serde_json::from_str(&body).unwrap();
                let (status, answer) = if texts.iter().any(|text| text.contains("fail")) {
                    ("500 Internal Server Error", "no".to_string())
                } else {
                    let items: Vec<Value> = texts
                        .iter()
                        .map(|text| json!({"translations": [{"text": text.to_uppercase()}]}))
                        .collect();
                    ("200 OK", Value::from(items).to_string())
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{answer}",
                    answer.len()
                );
                socket.write_all(response.as_bytes()).await.unwrap();
            }
        });
        (address, count)
    }

    async fn stream(edge: &str, pieces: &[&str]) -> Vec<Translated> {
        let translator = Translator::new(
            Service::Edge,
            None,
            Hosts {
                edge: edge.to_string(),
                google: String::new(),
                firefox: String::new(),
                chromium: String::new(),
            },
            PathBuf::new(),
        );
        let (out, mut received) = mpsc::channel(16);
        let streamer =
            Streamer::start(std::sync::Arc::new(translator), "ja".into(), out, |piece| {
                piece
            });
        for piece in pieces {
            streamer.push(piece);
        }
        streamer.finish(Duration::from_secs(10)).await;
        let mut translated = Vec::new();
        while let Ok(piece) = received.try_recv() {
            translated.push(piece);
        }
        translated
    }

    /// Text left untranslated goes out as written: the oldest at once when
    /// too much waits, the rest when the wait at the end runs out.
    #[tokio::test]
    async fn a_stalled_service_lets_the_text_through_as_written() {
        // Takes connections and never answers.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let edge = format!("http://{}/translate", listener.local_addr().unwrap());
        let translator = Translator::new(
            Service::Edge,
            None,
            Hosts {
                edge,
                google: String::new(),
                firefox: String::new(),
                chromium: String::new(),
            },
            PathBuf::new(),
        );
        let (out, mut received) = mpsc::channel(64);
        let streamer =
            Streamer::start(std::sync::Arc::new(translator), "ja".into(), out, |piece| {
                piece
            });
        let sentences: Vec<String> = (0..40).map(|n| format!("Sentence {n}. ")).collect();
        for sentence in &sentences {
            streamer.push(sentence);
        }
        streamer.finish(Duration::from_millis(300)).await;
        let mut pieces = Vec::new();
        while let Ok(piece) = received.try_recv() {
            pieces.push(piece);
        }
        let joined: String = pieces.iter().map(|piece| piece.text.as_str()).collect();
        assert_eq!(joined, sentences.concat());
        let codes: Vec<&str> = pieces
            .iter()
            .map(|piece| piece.error.as_ref().map_or("", |error| error.code))
            .collect();
        let count = |wanted: &str| codes.iter().filter(|&&code| code == wanted).count();
        // The space after the last sentence is a piece of its own.
        assert_eq!(pieces.len(), 41);
        assert_eq!(count("translation_behind"), 41 - MOST_WAITING);
        assert_eq!(count("translation_timeout"), MOST_WAITING);
        drop(listener);
    }

    #[tokio::test]
    async fn the_same_settings_share_one_translator() {
        let folder = PathBuf::from("shared-test-models");
        let one = shared(Service::Firefox, None, folder.clone());
        let two = shared(Service::Firefox, None, folder.clone());
        assert!(std::sync::Arc::ptr_eq(&one, &two));
        let other = shared(Service::Firefox, None, PathBuf::from("other-test-models"));
        assert!(!std::sync::Arc::ptr_eq(&one, &other));
    }

    #[tokio::test]
    async fn a_stream_is_translated_in_order_with_code_kept() {
        let (edge, count) = edge_stand_in().await;
        let pieces = stream(
            &edge,
            &["first one", ". Run `cargo t", "est` now.\n", "end"],
        )
        .await;
        let joined: String = pieces.iter().map(|piece| piece.text.as_str()).collect();
        assert_eq!(joined, "FIRST ONE. RUN `cargo test` NOW.\nEND");
        assert!(pieces.iter().all(|piece| piece.error.is_none()));
        // Pushed before the first translation started, so sent together.
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 1);

        let pieces = stream(&edge, &["もう日本語です。", "And English."]).await;
        let joined: String = pieces.iter().map(|piece| piece.text.as_str()).collect();
        assert_eq!(joined, "もう日本語です。AND ENGLISH.");
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 2);
        let pieces = stream(&edge, &["日本語だけ。"]).await;
        assert_eq!(pieces[0].text, "日本語だけ。");
        assert_eq!(
            count.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "sent text already in Japanese"
        );

        let pieces = stream(&edge, &["This will fail. ", "So `this`."]).await;
        let joined: String = pieces.iter().map(|piece| piece.text.as_str()).collect();
        assert_eq!(joined, "This will fail. So `this`.");
        assert!(pieces.iter().all(|piece| piece.error.is_some()));
    }

    /// Translates through the real services: `INOCODE_TRANSLATE_SERVICE`
    /// (`edge`, `google`, or `firefox` or `chromium`, which keep their
    /// models in the temporary folder).
    #[tokio::test]
    #[ignore = "sends text to a translation service; run by hand"]
    async fn translates_here() {
        let service = std::env::var("INOCODE_TRANSLATE_SERVICE").unwrap_or_else(|_| "edge".into());
        let translator = Translator::new(
            Service::parse(&service).unwrap(),
            None,
            Hosts::default(),
            std::env::temp_dir().join("inocode-firefox-models"),
        );
        let texts = vec![
            "I should check `src/main.rs` before running the tests.".to_string(),
            "The user wants a & b <now>.".to_string(),
        ];
        let translated = translator.translate(&texts, "ja").await.unwrap();
        println!("{translated:?}");
        assert!(translated[0].contains("`src/main.rs`"));
        assert_eq!(translated.len(), 2);
    }
}
