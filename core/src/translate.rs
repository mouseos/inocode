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

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::ErrorInfo;

/// The key in Google's published page-translation script.
const GOOGLE_KEY: &str = "AIzaSyATBXajvzQLTDHEQbcpq0Ihe0vWDHmO520";
/// How long one request to an online service may take; a stream's wait at
/// its end is as long, so that a request under way can still answer.
pub(crate) const WAIT: Duration = Duration::from_secs(20);
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
    /// The list of languages Google's page translation offers.
    pub google_languages: String,
    /// Microsoft Translator's list of languages, which Edge's service
    /// translates into.
    pub edge_languages: String,
}

impl Default for Hosts {
    fn default() -> Self {
        Self {
            edge: "https://edge.microsoft.com/translate/translatetext".into(),
            google: "https://translate-pa.googleapis.com/v1/translateHtml".into(),
            firefox: firefox::SETTINGS.into(),
            chromium: chromium::UPDATE.into(),
            google_languages: "https://translate-pa.googleapis.com/v1/supportedLanguages".into(),
            edge_languages: "https://api.cognitive.microsofttranslator.com/languages".into(),
        }
    }
}

/// The license or terms a kept model comes under, as far as they are known.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct License {
    /// The license's SPDX name, or `unknown`.
    pub name: String,
    /// Where the license, or the search for one, was read.
    pub source: String,
    /// Other terms that may apply.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terms: Option<String>,
}

/// A model kept on disk for one of the offline services.
#[derive(Debug, Clone, PartialEq)]
pub struct InstalledModel {
    pub service: Service,
    /// A Chromium pack translates both ways: `from` is always `en` and
    /// `to` the pack's other language.
    pub from: String,
    pub to: String,
    pub version: String,
    /// The size of the model's files.
    pub bytes: u64,
    pub license: License,
    /// The model's folder.
    pub path: PathBuf,
}

/// What a service can translate.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Languages {
    pub from: Vec<String>,
    pub to: Vec<String>,
    /// For the offline services, the pairs with a model of their own;
    /// other pairs go through English, so a pair of a `from` and a `to`
    /// works only when both have one with English. Empty for the online
    /// services, which translate between any of `from` and `to`.
    pub direct: Vec<(String, String)>,
}

/// The whole models kept in `models`, the folder given to
/// [`Translator::new`].
pub async fn installed(models: &Path) -> Vec<InstalledModel> {
    let mut out = firefox::installed(models).await;
    out.extend(chromium::installed(&models.join("chromium")).await);
    out
}

/// Deletes the model of `service` for `from` → `to` kept in `models`;
/// `Ok(false)` when there is none. For Chromium, either language may be
/// `en`, since a pack holds both ways. The translators shared in this
/// process let go of the models they loaded.
pub async fn remove(
    models: &Path,
    service: Service,
    from: &str,
    to: &str,
) -> Result<bool, ErrorInfo> {
    let removed = match service {
        Service::Firefox => firefox::remove(models, from, to).await?,
        Service::Chromium => chromium::remove(&models.join("chromium"), from, to).await?,
        Service::Edge | Service::Google => {
            return Err(ErrorInfo::new(
                "invalid_params",
                format!("the {} service keeps no models", service.name()),
            ))
        }
    };
    forget(service, models).await;
    Ok(removed)
}

/// The key in Google's page-translation script for its list of languages,
/// the default of the `google_languages_key` setting.
const GOOGLE_LANGUAGES_KEY: &str = "AIzaSyBWDj0QJvVIx8XOhRegXX5_SrRWxhT5Hs4";

/// What `service` can translate: Firefox's from its catalog, Chromium's
/// from the packs Chrome lists, Google's and Edge's from their lists of
/// languages, asked with `google_languages_key`.
pub async fn languages(
    service: Service,
    hosts: &Hosts,
    google_languages_key: &str,
) -> Result<Languages, ErrorInfo> {
    let client = reqwest::Client::builder()
        .user_agent(concat!("inocode/", env!("CARGO_PKG_VERSION")))
        .build()
        .unwrap_or_default();
    let get = |url: &str, query: &[(&str, &str)]| client.get(url).query(query).timeout(WAIT).send();
    let codes = |items: &Value, skip: &str| -> Vec<String> {
        let mut out: Vec<String> = items
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|item| item["language"].as_str())
            .filter(|code| *code != skip)
            .map(str::to_string)
            .collect();
        out.sort();
        out
    };
    match service {
        Service::Firefox => firefox::languages(&hosts.firefox, &client).await,
        Service::Chromium => Ok(chromium::languages()),
        Service::Google => {
            let query = [
                ("client", "te_lib"),
                ("display_language", "en"),
                ("key", google_languages_key),
            ];
            let response = get(&hosts.google_languages, &query).await.map_err(failed)?;
            let value = answer(response).await?;
            let from = codes(&value["sourceLanguages"], "auto");
            let to = codes(&value["targetLanguages"], "auto");
            if from.is_empty() || to.is_empty() {
                return Err(unexpected(&value));
            }
            Ok(Languages {
                from,
                to,
                direct: Vec::new(),
            })
        }
        Service::Edge => {
            let query = [("api-version", "3.0"), ("scope", "translation")];
            let response = get(&hosts.edge_languages, &query).await.map_err(failed)?;
            let value = answer(response).await?;
            let mut all: Vec<String> = value["translation"]
                .as_object()
                .map(|languages| languages.keys().cloned().collect())
                .unwrap_or_default();
            if all.is_empty() {
                return Err(unexpected(&value));
            }
            all.sort();
            Ok(Languages {
                from: all.clone(),
                to: all,
                direct: Vec::new(),
            })
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
                chromium::CHROME_VERSION.into(),
                client.clone(),
            ),
            firefox: firefox::Firefox::new(models, hosts.firefox.clone(), client.clone()),
            hosts,
            client,
        }
    }

    /// The same translator asking Chrome's update server as Chrome
    /// `version`.
    pub fn with_chrome_version(mut self, version: String) -> Self {
        self.chromium = self.chromium.with_version(version);
        self
    }

    /// Gets ready to translate from `from` to `to`: Firefox's models and
    /// Chrome's packs are fetched now rather than with the first text; the
    /// services need nothing. Gives back what the person should know about
    /// how the models were checked, such as OneCRL checked against no
    /// revocations known before.
    pub async fn prepare(&self, from: &str, to: &str) -> Result<Vec<String>, ErrorInfo> {
        match self.service {
            Service::Firefox => self.firefox.prepare(from, to).await,
            Service::Chromium => self.chromium.prepare(from, to).await.map(|()| Vec::new()),
            Service::Edge | Service::Google => Ok(Vec::new()),
        }
    }

    /// Each of `texts` in the language `to` (a tag such as `ja`), in the
    /// same order; code in backticks is left as written.
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
                    self.with_code_inline(&sent, to).await?
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

    /// For the services that cannot be told to leave code alone: each text
    /// goes whole, code and all, since cutting a sentence at its code makes
    /// pieces that translate badly (docs/evidence/translation.md); each
    /// piece of code found again in the translation is put back as
    /// written, and a text where one is not found is translated around its
    /// code instead.
    async fn with_code_inline(&self, texts: &[String], to: &str) -> Result<Vec<String>, ErrorInfo> {
        let split: Vec<Vec<Segment>> = texts.iter().map(|text| segments(text)).collect();
        let whole: Vec<String> = texts.iter().map(|text| text.trim().to_string()).collect();
        let answers = self.plain(&whole, to).await?;
        let mut out = Vec::with_capacity(texts.len());
        let mut redo = Vec::new();
        for (index, ((answer, segments), text)) in answers.iter().zip(&split).zip(texts).enumerate()
        {
            match restore_code(answer, segments) {
                Some(restored) => {
                    let lead = &text[..text.len() - text.trim_start().len()];
                    let trail = &text[text.trim_end().len()..];
                    out.push(format!("{lead}{restored}{trail}"));
                }
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

    /// `texts` as plain text through the service, each on its own.
    async fn plain(&self, texts: &[String], to: &str) -> Result<Vec<String>, ErrorInfo> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        match self.service {
            Service::Edge => self.edge(texts, to).await,
            Service::Firefox => self.firefox.translate(texts, to).await,
            Service::Chromium => self.chromium.translate(texts, to).await,
            Service::Google => {
                let escaped: Vec<String> = texts.iter().map(|text| escape(text)).collect();
                let answers = self.google(&escaped, to).await?;
                Ok(answers.iter().map(|answer| unescape(answer)).collect())
            }
        }
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
        let mut translated = self.plain(&sent, to).await?.into_iter();
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
        let (mut out, detected) = self.google_from(html, "auto", to).await?;
        // Google takes a mostly English text with a few Japanese or Chinese
        // characters for that language and gives it back untranslated;
        // such texts go again as English.
        let again: Vec<usize> = (0..html.len())
            .filter(|&index| {
                detected
                    .get(index)
                    .is_some_and(|language| primary(language) == primary(to))
                    && script_language(&html[index]).is_none()
            })
            .collect();
        if !again.is_empty() {
            let resent: Vec<String> = again.iter().map(|&index| html[index].clone()).collect();
            let (translated, _) = self.google_from(&resent, "en", to).await?;
            for (index, text) in again.into_iter().zip(translated) {
                out[index] = text;
            }
        }
        Ok(out)
    }

    /// `html` from the language `from` (or `auto`) to `to`, and the
    /// languages Google took each for.
    async fn google_from(
        &self,
        html: &[String],
        from: &str,
        to: &str,
    ) -> Result<(Vec<String>, Vec<String>), ErrorInfo> {
        let response = self
            .client
            .post(&self.hosts.google)
            .header("Content-Type", "application/json+protobuf")
            .header("X-Goog-API-Key", &self.google_key)
            .body(json!([[html, from, to], "te_lib"]).to_string())
            .timeout(WAIT)
            .send()
            .await
            .map_err(failed)?;
        let value = answer(response).await?;
        let items = value[0].as_array().ok_or_else(|| unexpected(&value))?;
        if items.len() != html.len() {
            return Err(unexpected(&value));
        }
        let texts = items
            .iter()
            .map(|item| {
                item.as_str()
                    .map(str::to_string)
                    .ok_or_else(|| unexpected(item))
            })
            .collect::<Result<_, _>>()?;
        let detected = value[1]
            .as_array()
            .map(|languages| {
                languages
                    .iter()
                    .map(|language| language.as_str().unwrap_or_default().to_string())
                    .collect()
            })
            .unwrap_or_default();
        Ok((texts, detected))
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
        let code = unescape(&inner[..end]);
        out.push_str(&unescape(&rest[..start]));
        drop_copied_start(&mut out, &code);
        out.push_str(&code);
        rest = &inner[end + MARK_CLOSE.len()..];
        found += 1;
    }
    out.push_str(&unescape(rest));
    (found == marks).then_some(out)
}

/// Quotes a translation may put around code in place of its backticks.
const QUOTES: [(&str, &str); 7] = [
    ("`", "`"),
    ("「", "」"),
    ("『", "』"),
    ("'", "'"),
    ("\"", "\""),
    ("\u{201c}", "\u{201d}"),
    ("\u{2018}", "\u{2019}"),
];

/// `translation` with each piece of code of `segments` put back as
/// written where it is found again: its text in any of [`QUOTES`], or, for
/// text of three or more characters, bare between non-word characters.
/// `None` when a piece is not found, or for a fenced block, whose lines a
/// translation does not keep.
fn restore_code(translation: &str, segments: &[Segment]) -> Option<String> {
    let mut found: Vec<(usize, usize, &str)> = Vec::new();
    let free = |found: &[(usize, usize, &str)], start: usize, end: usize| {
        found
            .iter()
            .all(|&(at, until, _)| end <= at || until <= start)
    };
    for segment in segments.iter().filter(|segment| !segment.translate) {
        let code = segment.text.as_str();
        if code.trim().is_empty() {
            continue;
        }
        if code.starts_with("```") {
            return None;
        }
        let inner = code.trim_matches('`');
        if inner.is_empty() {
            return None;
        }
        let quoted = QUOTES.iter().find_map(|(open, close)| {
            let wanted = format!("{open}{inner}{close}");
            translation
                .match_indices(&wanted)
                .map(|(at, _)| (at, at + wanted.len()))
                .find(|&(start, end)| free(&found, start, end))
        });
        let bare = || {
            (inner.chars().count() >= 3)
                .then(|| {
                    translation
                        .match_indices(inner)
                        .map(|(at, _)| (at, at + inner.len()))
                        .find(|&(start, end)| {
                            let before = translation[..start].chars().next_back();
                            let after = translation[end..].chars().next();
                            let word = |c: Option<char>| {
                                c.is_some_and(|c| c.is_alphanumeric() && c.is_ascii())
                            };
                            !word(before) && !word(after) && free(&found, start, end)
                        })
                })
                .flatten()
        };
        let (start, end) = quoted.or_else(bare)?;
        found.push((start, end, code));
    }
    found.sort_by_key(|&(start, ..)| start);
    let mut out = String::with_capacity(translation.len());
    let mut at = 0;
    for (start, end, code) in found {
        out.push_str(&translation[at..start]);
        out.push_str(code);
        at = end;
    }
    out.push_str(&translation[at..]);
    Some(out)
}

/// Google sometimes writes the start of a marked piece of code again just
/// before the mark (`` `let <mark>`let t = s;`</mark> ``); such a copy at the
/// end of `out` is taken off. Code starts with a backtick, which translated
/// text does not end with, so text of its own is not taken.
fn drop_copied_start(out: &mut String, code: &str) {
    let kept = out.trim_end().len();
    let copied = code
        .char_indices()
        .map(|(at, char)| at + char.len_utf8())
        .rfind(|&end| out[..kept].ends_with(&code[..end]));
    if let Some(length) = copied {
        out.truncate(kept - length);
    }
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

    /// Where the ready text ends: after the last sentence end outside
    /// code, else, past the longest chunk, after the last whitespace that
    /// is not inside closed code, so that a backtick that is never closed
    /// does not hold the rest of the stream back.
    fn cut(&self) -> Option<usize> {
        let scan = scan(&self.pending, false);
        scan.ends.last().copied().or_else(|| {
            (self.pending.len() > LONGEST_CHUNK)
                .then(|| scan.space_outside.max(scan.space_in_open_code))
                .flatten()
        })
    }
}

/// What [`scan`] finds in a text.
struct Scan {
    /// Just past each sentence end outside code.
    ends: Vec<usize>,
    /// Just past the last whitespace outside code.
    space_outside: Option<usize>,
    /// Just past the last whitespace inside code that is not closed yet.
    space_in_open_code: Option<usize>,
}

/// The sentence ends of `text`, the one rule for streamed text and for the
/// offline services: after 。！？ and a newline, and after `.`, `!` or `?`
/// when [`starts_sentence`] says so, outside code in ``` fences or
/// backticks. A backtick or fence with no closing one is code to the end
/// of a stream that may still close it; in a `whole` text it is a plain
/// character, and a `.`, `!` or `?` with only whitespace after it ends a
/// sentence.
fn scan(text: &str, whole: bool) -> Scan {
    let mut out = Scan {
        ends: Vec::new(),
        space_outside: None,
        space_in_open_code: None,
    };
    let mut at = 0;
    while let Some(char) = text[at..].chars().next() {
        let end = at + char.len_utf8();
        if char == '`' {
            let fence = if text[at..].starts_with("```") {
                "```"
            } else {
                "`"
            };
            let inside = at + fence.len();
            match text[inside..].find(fence) {
                Some(length) => {
                    at = inside + length + fence.len();
                    continue;
                }
                None if !whole => {
                    out.space_in_open_code = text[inside..]
                        .char_indices()
                        .rfind(|(_, c)| c.is_whitespace())
                        .map(|(offset, c)| inside + offset + c.len_utf8());
                    break;
                }
                None => {}
            }
        }
        match char {
            '\n' | '。' | '！' | '？' => out.ends.push(end),
            '.' | '!' | '?' if starts_sentence(&text[end..], whole) => out.ends.push(end),
            _ if char.is_whitespace() => out.space_outside = Some(end),
            _ => {}
        }
        at = end;
    }
    out
}

/// A whole text cut into sentences and the whitespace around them, by the
/// same rule as streamed text ([`scan`]); `true` marks a sentence.
pub(crate) fn sentences(text: &str) -> Vec<(&str, bool)> {
    let mut out = Vec::new();
    let mut start = 0;
    for end in scan(text, true).ends.into_iter().chain([text.len()]) {
        let piece = &text[start..end];
        start = end;
        let body = piece.trim();
        if body.is_empty() {
            if !piece.is_empty() {
                out.push((piece, false));
            }
            continue;
        }
        let lead = &piece[..piece.len() - piece.trim_start().len()];
        let trail = &piece[piece.trim_end().len()..];
        for (part, sentence) in [(lead, false), (body, true), (trail, false)] {
            if !part.is_empty() {
                out.push((part, sentence));
            }
        }
    }
    out
}

/// Whether `rest`, the text after a `.`, `!` or `?`, starts a new
/// sentence: space, then a capital, a digit, a list mark, a quote or code,
/// or a character of a script without capitals. A lower-case word carries
/// the sentence on, as after `println!` or `e.g.`; in a stream, until the
/// next word has come it is not known, so the cut waits; at the end of a
/// `whole` text the sentence ends.
fn starts_sentence(rest: &str, whole: bool) -> bool {
    let next = rest.trim_start();
    if next.is_empty() {
        return whole;
    }
    if next.len() == rest.len() {
        return false;
    }
    next.chars().next().is_some_and(|first| {
        first.is_uppercase()
            || first.is_ascii_digit()
            || matches!(first, '-' | '*' | '`' | '(' | '"' | '\'' | '#')
            || (!first.is_ascii() && !first.is_lowercase())
    })
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

    /// Ends the text pushed so far: what waits for a sentence end goes to
    /// be translated as a piece of its own, so that it is not joined with
    /// text pushed later.
    pub fn flush(&self) {
        let mut backlog = lock(&self.backlog);
        if let Some(piece) = backlog.chunker.finish() {
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

/// The translators made by [`shared`], by service, the setting that
/// service uses (Google's key, the Chrome version) and models folder.
type Made =
    std::collections::HashMap<(Service, Option<String>, PathBuf), std::sync::Arc<Translator>>;

static MADE: std::sync::LazyLock<std::sync::Mutex<Made>> =
    std::sync::LazyLock::new(Default::default);

fn made() -> std::sync::MutexGuard<'static, Made> {
    MADE.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The translator for the service of `settings` and `models`, made once
/// per process so that loaded models and open connections are kept.
pub(crate) fn shared(settings: &Settings, models: PathBuf) -> std::sync::Arc<Translator> {
    let service = settings.service;
    let setting = match service {
        Service::Google => settings.google_key.clone(),
        Service::Chromium => Some(settings.chrome_version.clone()),
        Service::Edge | Service::Firefox => None,
    };
    made()
        .entry((service, setting, models.clone()))
        .or_insert_with(|| {
            std::sync::Arc::new(
                Translator::new(
                    service,
                    settings.google_key.clone(),
                    Hosts::default(),
                    models,
                )
                .with_chrome_version(settings.chrome_version.clone()),
            )
        })
        .clone()
}

/// Lets go of the shared translators of `service` for `models` and of the
/// models they loaded, so that a model deleted from disk is not used again.
async fn forget(service: Service, models: &Path) {
    let gone: Vec<std::sync::Arc<Translator>> = {
        let mut made = made();
        let keys: Vec<_> = made
            .keys()
            .filter(|(kind, _, folder)| *kind == service && folder == models)
            .cloned()
            .collect();
        keys.into_iter()
            .filter_map(|key| made.remove(&key))
            .collect()
    };
    for translator in gone {
        translator.firefox.forget().await;
        translator.chromium.forget().await;
    }
}

/// The `translation` settings of `settings.json`.
#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    pub thinking: bool,
    pub service: Service,
    pub to: Option<String>,
    pub google_key: Option<String>,
    /// The Chrome version Chrome's update server is asked as.
    pub chrome_version: String,
    /// The key Google's list of languages is asked with.
    pub google_languages_key: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            thinking: false,
            service: Service::Google,
            to: None,
            google_key: None,
            chrome_version: chromium::CHROME_VERSION.into(),
            google_languages_key: GOOGLE_LANGUAGES_KEY.into(),
        }
    }
}

/// The keys of the `translation` settings; `answer` is accepted only as
/// off, since translating answers is not implemented.
const SETTING_KEYS: [&str; 7] = [
    "thinking",
    "service",
    "to",
    "google_key",
    "chrome_version",
    "google_languages_key",
    "answer",
];

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
        if let Some(unknown) = section
            .keys()
            .find(|key| !SETTING_KEYS.contains(&key.as_str()))
        {
            return Err(format!("unknown translation setting {unknown:?}"));
        }
        let text = |name: &str| -> Result<Option<String>, String> {
            match section.get(name) {
                None | Some(Value::Null) => Ok(None),
                Some(Value::String(text)) if text.trim().is_empty() => Ok(None),
                Some(Value::String(text)) => Ok(Some(text.trim().to_string())),
                Some(_) => Err(format!("\"translation.{name}\" is not a string")),
            }
        };
        let switch = |name: &str| -> Result<bool, String> {
            match section.get(name) {
                None | Some(Value::Null) => Ok(false),
                Some(Value::Bool(on)) => Ok(*on),
                Some(_) => Err(format!("\"translation.{name}\" is not true or false")),
            }
        };
        settings.thinking = switch("thinking")?;
        if switch("answer")? {
            return Err("translating answers is not implemented".into());
        }
        if let Some(name) = text("service")? {
            settings.service = Service::parse(&name)
                .ok_or_else(|| format!("unknown translation service {name:?}"))?;
        }
        settings.to = text("to")?;
        if let Some(to) = &settings.to {
            if !language_tag(to) {
                return Err(format!("\"translation.to\" {to:?} is not a language tag"));
            }
        }
        settings.google_key = text("google_key")?;
        if let Some(version) = text("chrome_version")? {
            let parts: Vec<&str> = version.split('.').collect();
            let numbers = parts
                .iter()
                .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()));
            if parts.len() != 4 || !numbers {
                return Err(format!(
                    "\"translation.chrome_version\" {version:?} is not four numbers with dots"
                ));
            }
            settings.chrome_version = version;
        }
        if let Some(key) = text("google_languages_key")? {
            if !key
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
            {
                return Err("\"translation.google_languages_key\" is not a key".into());
            }
            settings.google_languages_key = key;
        }
        Ok(settings)
    }

    /// The settings in the file at `path`; the defaults when it is missing.
    pub(crate) async fn read(path: &Path) -> Result<Self, String> {
        Self::from_value(&crate::settings::read(path).await?)
    }
}

/// Whether `tag` is written as a language tag (BCP 47 in form): a language
/// of two or three letters, then subtags of one to eight letters or
/// digits, such as `ja`, `zh-Hant` or `pt-BR`.
fn language_tag(tag: &str) -> bool {
    let mut parts = tag.split('-');
    let primary = parts.next().unwrap_or_default();
    (2..=3).contains(&primary.len())
        && primary.bytes().all(|b| b.is_ascii_alphabetic())
        && parts.all(|part| {
            (1..=8).contains(&part.len()) && part.bytes().all(|b| b.is_ascii_alphanumeric())
        })
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
    crate::settings::update(path, |file| {
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
        Settings::from_value(&Value::Object(file.clone()))
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_found_in_a_translation_is_put_back_as_written() {
        let parts = segments("Check `parse_config` and `s` in `cfg(test)` now.");
        assert_eq!(
            restore_code("「parse_config」と『s』を 'cfg(test)' で確認する。", &parts).as_deref(),
            Some("`parse_config`と`s`を `cfg(test)` で確認する。")
        );
        assert_eq!(
            restore_code("cfg(test) の中の parse_config と「s」を確認。", &parts).as_deref(),
            Some("`cfg(test)` の中の `parse_config` と`s`を確認。")
        );
        // `s` bare could be any letter s, so it is not taken.
        assert_eq!(
            restore_code("parse_config と s を cfg(test) で。", &parts),
            None
        );
        assert_eq!(restore_code("設定を確認する。", &parts), None);
        let fenced = segments("Run\n```\ncargo test\n```\n");
        assert_eq!(restore_code("実行\n```\ncargo test\n```\n", &fenced), None);
        assert_eq!(
            restore_code("設定を確認。", &segments("Check the settings.")).as_deref(),
            Some("設定を確認。")
        );
    }

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
        let copied = "問題: `let <span class=notranslate>`let t = s;`</span> が <span class=notranslate>`s`</span>使えない";
        assert_eq!(
            unmark(copied, 2).as_deref(),
            Some("問題: `let t = s;` が `s`使えない")
        );
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
        assert_eq!(chunker.push("The println! macro, e.g. this one. "), None);
        assert_eq!(
            chunker.push("Now done! Next"),
            Some("The println! macro, e.g. this one. Now done!".into())
        );
        assert_eq!(chunker.finish(), Some(" Next".into()));
        assert_eq!(chunker.push("最初。"), Some("最初。".into()));
        assert_eq!(chunker.finish(), None);
        let long = "word ".repeat(100);
        assert_eq!(chunker.push(&long).map(|text| text.len()), Some(500));
    }

    #[test]
    fn a_backtick_never_closed_does_not_hold_the_stream_back() {
        let mut chunker = Chunker::default();
        assert_eq!(chunker.push("Use ` here. Then"), None);
        let long = "more words ".repeat(40);
        let cut = chunker.push(&long).unwrap();
        assert!(cut.starts_with("Use ` here. Then"), "{cut}");
        assert!(cut.ends_with(' ') && cut.len() > LONGEST_CHUNK, "{cut}");
        assert_eq!(chunker.push("Next one. And"), Some("Next one.".into()));
    }

    #[test]
    fn whole_texts_are_cut_by_the_same_rule_as_streams() {
        assert_eq!(
            sentences("See e.g. this one. Run `a. B` now! 終わり。x"),
            [
                ("See e.g. this one.", true),
                (" ", false),
                ("Run `a. B` now!", true),
                (" ", false),
                ("終わり。", true),
                ("x", true),
            ]
        );
        // A backtick that is never closed is a plain character in a whole
        // text.
        assert_eq!(
            sentences("Odd ` mark. Next."),
            [("Odd ` mark.", true), (" ", false), ("Next.", true)]
        );
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
                to: Some("ja".into()),
                ..Settings::default()
            }
        );
        assert_eq!(settings.chrome_version, chromium::CHROME_VERSION);
        assert_eq!(settings.google_languages_key, GOOGLE_LANGUAGES_KEY);
        let set = Settings::from_value(&json!({"translation": {
            "chrome_version": "150.0.1.2", "google_languages_key": "AIza-x_1", "to": "zh-Hant",
            "answer": false}}))
        .unwrap();
        assert_eq!(set.chrome_version, "150.0.1.2");
        assert_eq!(set.google_languages_key, "AIza-x_1");
        for wrong in [
            json!({"tihnking": true}),
            json!({"answer": true}),
            json!({"to": "Japanese"}),
            json!({"to": "ja JP"}),
            json!({"chrome_version": "143"}),
            json!({"chrome_version": "143.0.x.1"}),
            json!({"google_languages_key": "a key"}),
        ] {
            assert!(
                Settings::from_value(&json!({ "translation": wrong })).is_err(),
                "{wrong}"
            );
        }
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

    /// A stand-in for Edge's service that answers each text in capitals
    /// outside backticks,
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
                        .map(|text| {
                            let shouted: Vec<String> = text
                                .split('`')
                                .enumerate()
                                .map(|(at, part)| {
                                    if at % 2 == 0 {
                                        part.to_uppercase()
                                    } else {
                                        part.to_string()
                                    }
                                })
                                .collect();
                            json!({"translations": [{"text": shouted.join("`")}]})
                        })
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
                google_languages: String::new(),
                edge_languages: String::new(),
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

    /// What waits for a sentence end when the stream is flushed goes as a
    /// piece of its own, not joined with what is pushed after.
    #[tokio::test]
    async fn a_flushed_stream_ends_its_unfinished_sentence() {
        let (edge, _) = edge_stand_in().await;
        let translator = Translator::new(
            Service::Edge,
            None,
            Hosts {
                edge,
                ..Hosts::default()
            },
            PathBuf::new(),
        );
        let (out, mut received) = mpsc::channel(16);
        let streamer =
            Streamer::start(std::sync::Arc::new(translator), "ja".into(), out, |piece| {
                piece
            });
        streamer.push("first answer without an end");
        streamer.flush();
        streamer.push("second one");
        streamer.finish(Duration::from_secs(10)).await;
        let mut pieces = Vec::new();
        while let Ok(piece) = received.try_recv() {
            pieces.push(piece.text);
        }
        assert_eq!(pieces, ["FIRST ANSWER WITHOUT AN END", "SECOND ONE"]);
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
                ..Hosts::default()
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
        assert_eq!(pieces.len(), 40);
        assert_eq!(count("translation_behind"), 40 - MOST_WAITING);
        assert_eq!(count("translation_timeout"), MOST_WAITING);
        drop(listener);
    }

    #[tokio::test]
    async fn the_same_settings_share_one_translator() {
        let folder = PathBuf::from("shared-test-models");
        let firefox = Settings {
            service: Service::Firefox,
            ..Settings::default()
        };
        let one = shared(&firefox, folder.clone());
        let two = shared(&firefox, folder.clone());
        assert!(std::sync::Arc::ptr_eq(&one, &two));
        let other = shared(&firefox, PathBuf::from("other-test-models"));
        assert!(!std::sync::Arc::ptr_eq(&one, &other));
        // Google's key does not make another offline translator.
        let keyed = Settings {
            google_key: Some("other".into()),
            ..firefox.clone()
        };
        assert!(std::sync::Arc::ptr_eq(
            &one,
            &shared(&keyed, folder.clone())
        ));
        let chromium = Settings {
            service: Service::Chromium,
            ..Settings::default()
        };
        let newer = Settings {
            chrome_version: "150.0.0.0".into(),
            ..chromium.clone()
        };
        assert!(!std::sync::Arc::ptr_eq(
            &shared(&chromium, folder.clone()),
            &shared(&newer, folder.clone())
        ));
    }

    /// A model deleted from disk is not used again by the translators the
    /// process shares.
    #[tokio::test]
    async fn a_removed_model_is_let_go_of() {
        let models = tempfile::tempdir().unwrap();
        let root = models.path().to_path_buf();
        let firefox = Settings {
            service: Service::Firefox,
            ..Settings::default()
        };
        let before = shared(&firefox, root.clone());
        std::fs::create_dir_all(root.join("en-ja")).unwrap();
        assert_eq!(remove(&root, Service::Firefox, "en", "ja").await, Ok(true));
        let after = shared(&firefox, root.clone());
        assert!(!std::sync::Arc::ptr_eq(&before, &after));
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

    #[tokio::test]
    async fn kept_models_are_listed_and_removed_only_inside_the_folder() {
        let models = tempfile::tempdir().unwrap();
        let root = models.path();
        let write = |path: &str, data: &[u8]| {
            let path = root.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, data).unwrap();
        };
        // A Firefox list written before licenses were kept in it.
        write(
            "en-zh-Hans/files.json",
            br#"{"version": "3.0", "architecture": "tiny", "files": [{"name": "model.bin",
                "kind": "model", "location": "x", "size": 1, "hash": "h",
                "decompressed_size": 5, "decompressed_hash": "d"}]}"#,
        );
        write("en-zh-Hans/model.bin", b"12345");
        // Not whole: the list names a file that is missing.
        write(
            "ja-en/files.json",
            br#"{"version": "3.0", "architecture": "base", "files": [{"name": "m",
                "kind": "model", "location": "x", "size": 1, "hash": "h",
                "decompressed_size": 5, "decompressed_hash": "d"}]}"#,
        );
        write("en-ja.update/files.json", b"{}");
        write("notes.txt", b"not a model");
        write(
            "chromium/en-ja/pack.json",
            br#"{"version": "2024.9.9.1", "files": [{"path": "en_ja_nmt/a.pb", "size": 3, "sha256": "x"}]}"#,
        );
        write("chromium/en-ja/en_ja_nmt/a.pb", b"abc");

        let found = installed(root).await;
        let shown: Vec<(Service, &str, &str, &str, u64, &str)> = found
            .iter()
            .map(|model| {
                (
                    model.service,
                    model.from.as_str(),
                    model.to.as_str(),
                    model.version.as_str(),
                    model.bytes,
                    model.license.name.as_str(),
                )
            })
            .collect();
        assert_eq!(
            shown,
            [
                (Service::Firefox, "en", "zh-Hans", "3.0", 5, "MPL-2.0"),
                (Service::Chromium, "en", "ja", "2024.9.9.1", 3, "unknown"),
            ]
        );
        assert_eq!(found[0].path, root.join("en-zh-Hans"));
        assert!(found[1]
            .license
            .terms
            .as_deref()
            .unwrap()
            .contains("Terms of Service"));

        for (service, from, to) in [
            (Service::Firefox, "..", "x"),
            (Service::Firefox, "en/../..", "x"),
            (Service::Firefox, "en", "ja\\..\\.."),
            (Service::Chromium, "ja", "de"),
            (Service::Chromium, "en", ".."),
            (Service::Google, "en", "ja"),
        ] {
            assert!(
                remove(root, service, from, to).await.is_err(),
                "{from} {to}"
            );
        }
        write("en-de", b"a file, not a folder");
        assert!(remove(root, Service::Firefox, "en", "de").await.is_err());
        assert!(root.join("en-de").exists());
        assert_eq!(remove(root, Service::Firefox, "en", "fr").await, Ok(false));
        assert_eq!(remove(root, Service::Firefox, "ja", "en").await, Ok(true));
        assert_eq!(
            remove(root, Service::Firefox, "en", "zh-Hans").await,
            Ok(true)
        );
        assert_eq!(remove(root, Service::Chromium, "ja", "en").await, Ok(true));
        assert!(installed(root).await.is_empty());
        assert!(root.join("notes.txt").exists() && root.join("chromium").exists());
    }

    #[tokio::test]
    async fn the_online_services_languages_are_read_from_their_lists() {
        let google = br#"{"sourceLanguages": [{"language": "auto", "name": "Detect language"},
            {"language": "ja", "name": "Japanese"}, {"language": "en", "name": "English"}],
            "targetLanguages": [{"language": "en", "name": "English"},
            {"language": "zh-TW", "name": "Chinese (Traditional)"}]}"#;
        let edge = br#"{"translation": {"ja": {"name": "Japanese"}, "en": {"name": "English"},
            "tlh-Latn": {"name": "Klingon (Latin)"}}}"#;
        let base = firefox::tests::stand_in(vec![
            ("/google".into(), google.to_vec()),
            ("/edge".into(), edge.to_vec()),
            ("/empty".into(), b"{}".to_vec()),
        ])
        .await;
        let hosts = Hosts {
            google_languages: format!("{base}/google"),
            edge_languages: format!("{base}/edge"),
            ..Hosts::default()
        };
        let found = languages(Service::Google, &hosts, GOOGLE_LANGUAGES_KEY)
            .await
            .unwrap();
        assert_eq!(found.from, ["en", "ja"]);
        assert_eq!(found.to, ["en", "zh-TW"]);
        assert!(found.direct.is_empty());
        let found = languages(Service::Edge, &hosts, GOOGLE_LANGUAGES_KEY)
            .await
            .unwrap();
        assert_eq!(found.from, ["en", "ja", "tlh-Latn"]);
        assert_eq!(found.from, found.to);
        let found = languages(Service::Chromium, &hosts, GOOGLE_LANGUAGES_KEY)
            .await
            .unwrap();
        assert!(found.direct.contains(&("en".into(), "ja".into())));
        let empty = Hosts {
            google_languages: format!("{base}/empty"),
            edge_languages: format!("{base}/missing"),
            ..Hosts::default()
        };
        assert!(languages(Service::Google, &empty, GOOGLE_LANGUAGES_KEY)
            .await
            .is_err());
        assert!(languages(Service::Edge, &empty, GOOGLE_LANGUAGES_KEY)
            .await
            .is_err());
    }

    /// Lists each service's languages from the real servers.
    #[tokio::test]
    #[ignore = "asks Google, Microsoft and Mozilla for their lists of languages; run by hand"]
    async fn languages_are_listed_here() {
        for service in [
            Service::Google,
            Service::Edge,
            Service::Firefox,
            Service::Chromium,
        ] {
            let found = languages(service, &Hosts::default(), GOOGLE_LANGUAGES_KEY)
                .await
                .unwrap();
            println!(
                "{}: {} from, {} to, {} direct pairs",
                service.name(),
                found.from.len(),
                found.to.len(),
                found.direct.len()
            );
            assert!(found.from.contains(&"en".to_string()), "{found:?}");
            assert!(found.to.iter().any(|to| to.starts_with("ja")), "{found:?}");
        }
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
            "Quick arithmetic: 9:47 plus 2 hours 38 minutes comes to 12:25, which is 午後0時25分."
                .to_string(),
        ];
        let translated = translator.translate(&texts, "ja").await.unwrap();
        println!("{translated:?}");
        assert!(translated[0].contains("`src/main.rs`"));
        assert_eq!(translated.len(), 3);
        assert!(!translated[2].starts_with("Quick"), "{}", translated[2]);
    }
}
