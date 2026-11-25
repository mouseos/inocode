//! Translation with the models Firefox translates pages with, run here
//! offline: the models are fetched from Mozilla's Remote Settings once,
//! checked and kept on disk, then loaded into memory and run on the CPU.
//! Catalog facts and choices: docs/evidence/translation.md.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::marian::{Model, Shortlist};
use super::sentencepiece::Vocabulary;
use crate::ErrorInfo;

/// Mozilla's Remote Settings server.
pub(super) const SETTINGS: &str = "https://firefox.settings.services.mozilla.com/v1";
const COLLECTION: &str = "buckets/main/collections/translations-models-v2/records";
/// Pairs without a model of their own go through English, as in Firefox.
const PIVOT: &str = "en";
/// The model record version Firefox reads (`LANGUAGE_MODEL_MAJOR_VERSION_MIN`
/// and `_MAX` in TranslationsParent.sys.mjs).
const MAJOR_VERSION: u64 = 3;
/// `filter_expression` values a desktop program meets; records with any
/// other are for Android or for other release channels.
const DESKTOP_FILTERS: [&str; 2] = ["", "env.appinfo.OS != 'Android'"];
const CATALOG_WAIT: Duration = Duration::from_secs(30);
const DOWNLOAD_WAIT: Duration = Duration::from_secs(600);
const LARGEST_CATALOG: u64 = 20 << 20;
const LARGEST_FILE: u64 = 300 << 20;
/// Firefox's decoding settings (translations-engine.worker.js):
/// `max-length-break` source pieces and `max-length-factor`.
const LONGEST_SOURCE: usize = 128;
const LENGTH_FACTOR: f32 = 2.0;
const MANIFEST: &str = "files.json";

/// A file of a model as the catalog lists it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct File {
    name: String,
    kind: String,
    location: String,
    size: u64,
    hash: String,
    decompressed_size: u64,
    decompressed_hash: String,
}

/// The files that make up the model of a language pair.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Files {
    version: String,
    architecture: String,
    files: Vec<File>,
}

impl Files {
    fn of(&self, kind: &str) -> Option<&File> {
        self.files.iter().find(|file| file.kind == kind)
    }
}

/// A language pair loaded into memory.
struct Pair {
    model: Model,
    source: Arc<Vocabulary>,
    target: Arc<Vocabulary>,
    shortlist: Option<Shortlist>,
}

impl Pair {
    fn load(folder: &Path, files: &Files) -> Result<Self, String> {
        let read = |file: &File| -> Result<Vec<u8>, String> {
            let data = std::fs::read(folder.join(&file.name))
                .map_err(|error| format!("{}: {error}", file.name))?;
            if data.len() as u64 != file.decompressed_size {
                return Err(format!("{} does not have its listed size", file.name));
            }
            Ok(data)
        };
        let need = |kind: &str| {
            files
                .of(kind)
                .ok_or(format!("the model has no {kind} file"))
        };
        let model = Model::parse(&read(need("model")?)?)?;
        let (source, target) = match files.of("vocab") {
            Some(shared) => {
                let vocabulary = Arc::new(Vocabulary::parse(&read(shared)?)?);
                (vocabulary.clone(), vocabulary)
            }
            None => (
                Arc::new(Vocabulary::parse(&read(need("srcvocab")?)?)?),
                Arc::new(Vocabulary::parse(&read(need("trgvocab")?)?)?),
            ),
        };
        if source.len() != model.source_words() || target.len() != model.target_words() {
            return Err("the vocabularies do not fit the model".into());
        }
        let shortlist = match files.of("lex") {
            Some(file) => Some(Shortlist::parse(&read(file)?, model.target_words())?),
            None => None,
        };
        Ok(Self {
            model,
            source,
            target,
            shortlist,
        })
    }

    /// `text` translated sentence by sentence, the space between them
    /// kept as it is.
    fn translate(&self, text: &str) -> String {
        sentences(text)
            .into_iter()
            .map(|(piece, sentence)| {
                if sentence {
                    self.sentence(piece)
                } else {
                    piece.to_string()
                }
            })
            .collect()
    }

    /// A source longer than Firefox's limit goes in parts, as Bergamot
    /// breaks it.
    fn sentence(&self, sentence: &str) -> String {
        let pieces = self.source.encode(sentence);
        let mut out = Vec::new();
        for part in pieces.chunks(LONGEST_SOURCE) {
            let mut words = part.to_vec();
            words.push(self.source.end());
            let candidates = self
                .shortlist
                .as_ref()
                .map(|shortlist| shortlist.candidates(&words, self.model.target_words()));
            let longest = (LENGTH_FACTOR * words.len() as f32) as usize;
            out.extend(self.model.translate(
                &words,
                candidates.as_deref(),
                self.target.end(),
                self.target.unknown(),
                longest,
            ));
        }
        self.target.decode(&out)
    }
}

/// `text` cut into sentences and the whitespace around them; a sentence
/// ends at 。！？, a newline, or . ! ? before whitespace.
pub(super) fn sentences(text: &str) -> Vec<(&str, bool)> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut chars = text.char_indices().peekable();
    while let Some((at, char)) = chars.next() {
        let next = chars.peek().map(|&(_, next)| next);
        let ends = match char {
            '\n' | '。' | '！' | '？' => true,
            '.' | '!' | '?' => next.is_some_and(char::is_whitespace),
            _ => false,
        };
        if ends {
            let end = at + char.len_utf8();
            push_sentence(&text[start..end], &mut out);
            start = end;
        }
    }
    push_sentence(&text[start..], &mut out);
    out
}

fn push_sentence<'a>(piece: &'a str, out: &mut Vec<(&'a str, bool)>) {
    let body = piece.trim();
    if body.is_empty() {
        if !piece.is_empty() {
            out.push((piece, false));
        }
        return;
    }
    let lead = &piece[..piece.len() - piece.trim_start().len()];
    let trail = &piece[piece.trim_end().len()..];
    for (part, sentence) in [(lead, false), (body, true), (trail, false)] {
        if !part.is_empty() {
            out.push((part, sentence));
        }
    }
}

/// The language of `text` as its script tells it, English otherwise.
fn source_language(text: &str) -> &'static str {
    super::script_language(text).unwrap_or("en")
}

/// `to` as the catalog writes languages: Chinese by script.
fn model_language(to: &str) -> String {
    match to {
        "zh" | "zh-CN" | "zh-SG" | "zh-Hans" => "zh-Hans".into(),
        "zh-TW" | "zh-HK" | "zh-MO" | "zh-Hant" => "zh-Hant".into(),
        other => other.to_string(),
    }
}

/// The catalog's version `3.1` as numbers; versions with letters are
/// test releases and are left out.
fn version(text: &str) -> Option<(u64, u64)> {
    let (major, minor) = text.split_once('.')?;
    Some((major.parse().ok()?, minor.parse().ok()?))
}

/// A file name that is safe to write in the cache folder.
pub(super) fn safe_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('.')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
}

fn file(record: &Value) -> Option<File> {
    let text = |value: &Value| value.as_str().map(str::to_string);
    let attachment = &record["attachment"];
    let file = File {
        name: text(&record["name"])?,
        kind: text(&record["fileType"])?,
        location: text(&attachment["location"])?,
        size: attachment["size"].as_u64()?,
        hash: text(&attachment["hash"])?,
        decompressed_size: record["decompressedSize"].as_u64()?,
        decompressed_hash: text(&record["decompressedHash"])?,
    };
    safe_name(&file.name).then_some(file)
}

/// The files for `from` → `to` in the catalog `records`: of the records a
/// desktop program uses and with a supported version, the highest version
/// that has a model and its vocabularies; on a tie, the larger model.
fn choose(records: &[Value], from: &str, to: &str) -> Option<Files> {
    let mut groups: HashMap<((u64, u64), String), Vec<File>> = HashMap::new();
    for record in records {
        if record["sourceLanguage"] != from || record["targetLanguage"] != to {
            continue;
        }
        let filter = record["filter_expression"].as_str().unwrap_or("").trim();
        if !DESKTOP_FILTERS.contains(&filter) {
            continue;
        }
        let Some(version) = record["version"].as_str().and_then(version) else {
            continue;
        };
        if version.0 != MAJOR_VERSION {
            continue;
        }
        let architecture = record["architecture"].as_str().unwrap_or("").to_string();
        if let Some(file) = file(record) {
            groups
                .entry((version, architecture))
                .or_default()
                .push(file);
        }
    }
    let complete = |files: &[File]| {
        let count = |kind: &str| files.iter().filter(|file| file.kind == kind).count();
        let kinds = ["model", "lex", "vocab", "srcvocab", "trgvocab"];
        kinds.iter().all(|kind| count(kind) <= 1)
            && files.iter().all(|file| kinds.contains(&file.kind.as_str()))
            && count("model") == 1
            && (count("vocab") == 1 || (count("srcvocab") == 1 && count("trgvocab") == 1))
    };
    groups
        .into_iter()
        .filter(|(_, files)| complete(files))
        .max_by_key(|((version, _), files)| {
            let model = files.iter().find(|file| file.kind == "model");
            (*version, model.map_or(0, |file| file.decompressed_size))
        })
        .map(|(((major, minor), architecture), files)| Files {
            version: format!("{major}.{minor}"),
            architecture,
            files,
        })
}

/// The pairs to go through from `from` to `to`: the pair itself, else
/// two through English.
fn plan(records: &[Value], from: &str, to: &str) -> Option<Vec<(String, String, Files)>> {
    if let Some(files) = choose(records, from, to) {
        return Some(vec![(from.to_string(), to.to_string(), files)]);
    }
    if from == PIVOT || to == PIVOT {
        return None;
    }
    let first = choose(records, from, PIVOT)?;
    let second = choose(records, PIVOT, to)?;
    Some(vec![
        (from.to_string(), PIVOT.to_string(), first),
        (PIVOT.to_string(), to.to_string(), second),
    ])
}

pub(super) fn sha256(data: &[u8]) -> String {
    Sha256::digest(data)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// The file's data from its downloaded zstd form, checked against both
/// listed hashes and sizes.
fn unpack(file: &File, compressed: &[u8]) -> Result<Vec<u8>, String> {
    if compressed.len() as u64 != file.size || sha256(compressed) != file.hash {
        return Err(format!("{} does not match its listed hash", file.name));
    }
    let decoder = ruzstd::decoding::StreamingDecoder::new(compressed)
        .map_err(|error| format!("{}: {error}", file.name))?;
    let mut data = Vec::new();
    decoder
        .take(file.decompressed_size + 1)
        .read_to_end(&mut data)
        .map_err(|error| format!("{}: {error}", file.name))?;
    if data.len() as u64 != file.decompressed_size || sha256(&data) != file.decompressed_hash {
        return Err(format!("{} does not unpack to its listed hash", file.name));
    }
    Ok(data)
}

/// Writes `data` to `path` through a temporary file, so the file is
/// either whole or missing.
pub(super) async fn write_whole(path: &Path, data: &[u8]) -> Result<(), String> {
    static COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let count = COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let temporary = path.with_extension(format!("{}-{count}.tmp", std::process::id()));
    tokio::fs::write(&temporary, data)
        .await
        .map_err(|error| format!("{}: {error}", temporary.display()))?;
    if let Err(error) = tokio::fs::rename(&temporary, path).await {
        let _ = tokio::fs::remove_file(&temporary).await;
        return Err(format!("{}: {error}", path.display()));
    }
    Ok(())
}

/// The body of `response`, at most `limit` bytes.
pub(super) async fn body(mut response: reqwest::Response, limit: u64) -> Result<Vec<u8>, String> {
    let status = response.status();
    let url = response.url().clone();
    if !status.is_success() {
        return Err(format!("HTTP {status} from {url}"));
    }
    if response
        .content_length()
        .is_some_and(|length| length > limit)
    {
        return Err(format!("{url} is too large"));
    }
    let mut data = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|error| error.to_string())? {
        if (data.len() + chunk.len()) as u64 > limit {
            return Err(format!("{url} is too large"));
        }
        data.extend_from_slice(&chunk);
    }
    Ok(data)
}

/// Language pairs loaded into memory, by source and target language.
type Loaded = HashMap<(String, String), Arc<Pair>>;

/// The catalog: where attachments are, and the model records.
struct Catalog {
    attachments: reqwest::Url,
    records: Vec<Value>,
}

#[derive(Clone)]
pub(super) struct Firefox {
    folder: PathBuf,
    settings: String,
    client: reqwest::Client,
    loaded: Arc<tokio::sync::Mutex<Loaded>>,
}

impl std::fmt::Debug for Firefox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Firefox")
            .field("folder", &self.folder)
            .field("settings", &self.settings)
            .finish()
    }
}

fn failed(error: impl std::fmt::Display) -> ErrorInfo {
    ErrorInfo::new("translation_failed", error.to_string())
}

impl Firefox {
    /// Models are kept in `folder`; the catalog is at `settings`.
    pub(super) fn new(folder: PathBuf, settings: String, client: reqwest::Client) -> Self {
        Self {
            folder,
            settings,
            client,
            loaded: Arc::default(),
        }
    }

    /// Each of `texts` in the language `to`.
    pub(super) async fn translate(
        &self,
        texts: &[String],
        to: &str,
    ) -> Result<Vec<String>, ErrorInfo> {
        let to = model_language(to);
        let mut routes: HashMap<&str, Vec<Arc<Pair>>> = HashMap::new();
        let mut jobs = Vec::with_capacity(texts.len());
        for text in texts {
            let from = source_language(text);
            if !routes.contains_key(from) {
                routes.insert(from, self.route(from, &to).await?);
            }
            jobs.push((routes[from].clone(), text.clone()));
        }
        tokio::task::spawn_blocking(move || {
            jobs.into_iter()
                .map(|(route, text)| route.iter().fold(text, |text, pair| pair.translate(&text)))
                .collect()
        })
        .await
        .map_err(failed)
    }

    /// Fetches the models that take `from` to `to` when they are not on
    /// disk yet.
    pub(super) async fn prepare(&self, from: &str, to: &str) -> Result<(), ErrorInfo> {
        self.route(from, &model_language(to)).await.map(drop)
    }

    /// The loaded pairs that take `from` to `to`; fetched first when they
    /// are not on disk yet.
    async fn route(&self, from: &str, to: &str) -> Result<Vec<Arc<Pair>>, ErrorInfo> {
        if from == to {
            return Ok(Vec::new());
        }
        let direct = (from.to_string(), to.to_string());
        let pivot = [
            (from.to_string(), PIVOT.to_string()),
            (PIVOT.to_string(), to.to_string()),
        ];
        let through_pivot = from != PIVOT
            && to != PIVOT
            && self.kept(&pivot[0]).await
            && self.kept(&pivot[1]).await;
        let mut pairs = Vec::new();
        if self.kept(&direct).await {
            pairs.push(direct);
        } else if through_pivot {
            pairs.extend(pivot);
        } else {
            let catalog = self.catalog().await.map_err(failed)?;
            let plan = plan(&catalog.records, from, to).ok_or_else(|| {
                failed(format!(
                    "Firefox has no translation model from {from} to {to}"
                ))
            })?;
            for (from, to, files) in plan {
                let pair = (from, to);
                if !self.kept(&pair).await {
                    self.fetch(&catalog, &pair, &files).await.map_err(failed)?;
                }
                pairs.push(pair);
            }
        }
        let mut loaded = Vec::with_capacity(pairs.len());
        for pair in pairs {
            loaded.push(self.load(pair).await?);
        }
        Ok(loaded)
    }

    fn pair_folder(&self, (from, to): &(String, String)) -> PathBuf {
        self.folder.join(format!("{from}-{to}"))
    }

    /// The files of the pair on disk, when all of them were fetched.
    async fn manifest(&self, pair: &(String, String)) -> Option<Files> {
        let folder = self.pair_folder(pair);
        let text = tokio::fs::read_to_string(folder.join(MANIFEST))
            .await
            .ok()?;
        let files: Files = serde_json::from_str(&text).ok()?;
        for file in &files.files {
            let metadata = tokio::fs::metadata(folder.join(&file.name)).await.ok()?;
            if metadata.len() != file.decompressed_size {
                return None;
            }
        }
        Some(files)
    }

    async fn kept(&self, pair: &(String, String)) -> bool {
        self.loaded.lock().await.contains_key(pair) || self.manifest(pair).await.is_some()
    }

    async fn load(&self, pair: (String, String)) -> Result<Arc<Pair>, ErrorInfo> {
        let mut loaded = self.loaded.lock().await;
        if let Some(found) = loaded.get(&pair) {
            return Ok(found.clone());
        }
        let files = self
            .manifest(&pair)
            .await
            .ok_or_else(|| failed(format!("the {}-{} model is missing", pair.0, pair.1)))?;
        let folder = self.pair_folder(&pair);
        let model = tokio::task::spawn_blocking(move || Pair::load(&folder, &files))
            .await
            .map_err(failed)?
            .map_err(|error| failed(format!("the {}-{} model: {error}", pair.0, pair.1)))?;
        let model = Arc::new(model);
        loaded.insert(pair, model.clone());
        Ok(model)
    }

    async fn catalog(&self) -> Result<Catalog, String> {
        let get = |url: String| async move {
            let response = self
                .client
                .get(&url)
                .timeout(CATALOG_WAIT)
                .send()
                .await
                .map_err(|error| error.to_string())?;
            let data = body(response, LARGEST_CATALOG).await?;
            serde_json::from_slice::<Value>(&data).map_err(|error| format!("{url}: {error}"))
        };
        let settings = self.settings.trim_end_matches('/');
        let server = get(format!("{settings}/")).await?;
        let base = server["capabilities"]["attachments"]["base_url"]
            .as_str()
            .ok_or("the settings server names no attachment address")?;
        let attachments = reqwest::Url::parse(base).map_err(|error| error.to_string())?;
        let records = get(format!("{settings}/{COLLECTION}")).await?;
        let records = records["data"]
            .as_array()
            .cloned()
            .ok_or("the model catalog has no records")?;
        Ok(Catalog {
            attachments,
            records,
        })
    }

    /// Downloads, checks and keeps each file of `pair`, then its list.
    async fn fetch(
        &self,
        catalog: &Catalog,
        pair: &(String, String),
        files: &Files,
    ) -> Result<(), String> {
        let folder = self.pair_folder(pair);
        tokio::fs::create_dir_all(&folder)
            .await
            .map_err(|error| format!("{}: {error}", folder.display()))?;
        for file in &files.files {
            let path = folder.join(&file.name);
            if tokio::fs::metadata(&path)
                .await
                .is_ok_and(|metadata| metadata.len() == file.decompressed_size)
            {
                continue;
            }
            let url = attachment(&catalog.attachments, &file.location)?;
            if file.size > LARGEST_FILE || file.decompressed_size > LARGEST_FILE {
                return Err(format!("{} is too large", file.name));
            }
            let response = self
                .client
                .get(url)
                .timeout(DOWNLOAD_WAIT)
                .send()
                .await
                .map_err(|error| error.to_string())?;
            let compressed = body(response, file.size).await?;
            let file_owned = file.clone();
            let data = tokio::task::spawn_blocking(move || unpack(&file_owned, &compressed))
                .await
                .map_err(|error| error.to_string())??;
            write_whole(&path, &data).await?;
        }
        let manifest = serde_json::to_vec_pretty(files).map_err(|error| error.to_string())?;
        write_whole(&folder.join(MANIFEST), &manifest).await
    }
}

/// The address of an attachment at `location`, which must stay on the
/// attachment server.
fn attachment(base: &reqwest::Url, location: &str) -> Result<reqwest::Url, String> {
    let bad = || format!("the attachment location {location:?} leaves the attachment server");
    if location.contains("..") || location.contains("://") || location.starts_with('/') {
        return Err(bad());
    }
    let url = base.join(location).map_err(|_| bad())?;
    if url.scheme() != base.scheme()
        || url.host_str() != base.host_str()
        || url.port() != base.port()
    {
        return Err(bad());
    }
    Ok(url)
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use serde_json::json;

    fn record(
        name: &str,
        kind: &str,
        (from, to): (&str, &str),
        version: &str,
        architecture: &str,
        filter: &str,
        size: u64,
    ) -> Value {
        json!({
            "name": name, "fileType": kind, "sourceLanguage": from, "targetLanguage": to,
            "version": version, "architecture": architecture, "filter_expression": filter,
            "decompressedSize": size, "decompressedHash": "d",
            "attachment": {"location": format!("main/{name}.zst"), "size": 1, "hash": "h"},
        })
    }

    fn set(
        (from, to): (&str, &str),
        version: &str,
        architecture: &str,
        filter: &str,
        size: u64,
        shared: bool,
    ) -> Vec<Value> {
        let pair = format!("{from}{to}");
        let mut out = vec![
            record(
                &format!("model.{pair}.bin"),
                "model",
                (from, to),
                version,
                architecture,
                filter,
                size,
            ),
            record(
                &format!("lex.{pair}.bin"),
                "lex",
                (from, to),
                version,
                architecture,
                filter,
                1,
            ),
        ];
        if shared {
            out.push(record(
                &format!("vocab.{pair}.spm"),
                "vocab",
                (from, to),
                version,
                architecture,
                filter,
                1,
            ));
        } else {
            out.push(record(
                &format!("srcvocab.{pair}.spm"),
                "srcvocab",
                (from, to),
                version,
                architecture,
                filter,
                1,
            ));
            out.push(record(
                &format!("trgvocab.{pair}.spm"),
                "trgvocab",
                (from, to),
                version,
                architecture,
                filter,
                1,
            ));
        }
        out
    }

    #[test]
    fn the_catalog_gives_the_newest_desktop_model_or_a_route_through_english() {
        let mut records = Vec::new();
        records.extend(set(("en", "ja"), "3.0", "base-memory", "", 40, false));
        records.extend(set(("en", "ja"), "2.9", "base", "", 90, false));
        records.extend(set(("en", "ja"), "4.0", "base", "", 90, false));
        records.extend(set(("en", "ja"), "3.5a1", "base", "", 90, false));
        records.extend(set(
            ("ja", "en"),
            "3.1",
            "base-memory",
            "env.appinfo.OS == 'Android'",
            40,
            true,
        ));
        records.extend(set(
            ("ja", "en"),
            "3.0",
            "base",
            "env.appinfo.OS != 'Android'",
            60,
            true,
        ));
        records.extend(set(
            ("de", "en"),
            "3.2",
            "tiny",
            "env.channel == 'default' || env.channel == 'nightly'",
            10,
            true,
        ));
        records.extend(set(("de", "en"), "3.0", "tiny", "", 10, true));
        // Two architectures of one version: the larger model.
        records.extend(set(("en", "de"), "3.0", "tiny", "", 10, true));
        records.extend(set(("en", "de"), "3.0", "base", "", 50, true));
        // A version without its vocabulary is skipped.
        records.push(record(
            "model.enko.bin",
            "model",
            ("en", "ko"),
            "3.1",
            "base",
            "",
            5,
        ));
        records.extend(set(("en", "ko"), "3.0", "base", "", 5, true));

        let direct = choose(&records, "en", "ja").unwrap();
        assert_eq!(
            (direct.version.as_str(), direct.architecture.as_str()),
            ("3.0", "base-memory")
        );
        assert_eq!(direct.files.len(), 4);
        assert!(direct.of("srcvocab").is_some() && direct.of("vocab").is_none());
        let back = choose(&records, "ja", "en").unwrap();
        assert_eq!(
            (back.version.as_str(), back.architecture.as_str()),
            ("3.0", "base")
        );
        assert_eq!(choose(&records, "de", "en").unwrap().version, "3.0");
        assert_eq!(choose(&records, "en", "de").unwrap().architecture, "base");
        assert_eq!(choose(&records, "en", "ko").unwrap().version, "3.0");

        let route: Vec<(String, String)> = plan(&records, "ja", "de")
            .unwrap()
            .into_iter()
            .map(|(from, to, _)| (from, to))
            .collect();
        assert_eq!(
            route,
            [
                ("ja".to_string(), "en".to_string()),
                ("en".to_string(), "de".to_string())
            ]
        );
        assert_eq!(plan(&records, "en", "ja").unwrap().len(), 1);
        assert!(plan(&records, "ja", "fr").is_none());
        assert!(plan(&records, "en", "fr").is_none());
    }

    #[test]
    fn attachments_stay_on_their_server() {
        let base = reqwest::Url::parse("https://cdn.example/").unwrap();
        assert_eq!(
            attachment(&base, "main/a.zst").unwrap().as_str(),
            "https://cdn.example/main/a.zst"
        );
        assert!(attachment(&base, "//evil.example/a").is_err());
        assert!(attachment(&base, "https://evil.example/a").is_err());
        assert!(attachment(&base, "main/../../a").is_err());
        assert!(safe_name("model.enja.intgemm.alphas.bin"));
        assert!(!safe_name("../model"));
        assert!(!safe_name("a/b"));
    }

    #[test]
    fn downloads_are_checked_before_they_are_kept() {
        let data = b"model data ".repeat(100);
        let compressed = ruzstd::encoding::compress_to_vec(
            &data[..],
            ruzstd::encoding::CompressionLevel::Fastest,
        );
        let mut file = File {
            name: "model.bin".into(),
            kind: "model".into(),
            location: "main/x.zst".into(),
            size: compressed.len() as u64,
            hash: sha256(&compressed),
            decompressed_size: data.len() as u64,
            decompressed_hash: sha256(&data),
        };
        assert_eq!(unpack(&file, &compressed).unwrap(), data);
        let mut broken = compressed.clone();
        broken[compressed.len() / 2] ^= 1;
        assert!(unpack(&file, &broken).is_err());
        file.decompressed_hash = sha256(b"other");
        assert!(unpack(&file, &compressed).is_err());
    }

    #[test]
    fn text_is_cut_into_sentences_with_the_space_kept() {
        assert_eq!(
            sentences("  One. Two!\nThree 3.5 four。五"),
            [
                ("  ", false),
                ("One.", true),
                (" ", false),
                ("Two!", true),
                ("\n", false),
                ("Three 3.5 four。", true),
                ("五", true),
            ]
        );
        assert_eq!(source_language("Check `main.rs` now"), "en");
        assert_eq!(source_language("テストを実行する"), "ja");
        assert_eq!(source_language("运行测试"), "zh-Hans");
        assert_eq!(source_language("테스트"), "ko");
        assert_eq!(source_language("Run the 测试 suite"), "en");
        assert_eq!(model_language("zh-TW"), "zh-Hant");
        assert_eq!(model_language("ja"), "ja");
    }

    /// An address nothing answers at, for runs that must not use the
    /// network.
    const NOWHERE: &str = "http://127.0.0.1:9";

    /// The memory the process uses now and at most, in bytes.
    #[cfg(windows)]
    pub(crate) fn memory() -> Option<(usize, usize)> {
        #[repr(C)]
        #[derive(Default)]
        struct Counters {
            size: u32,
            page_faults: u32,
            peak_working_set: usize,
            working_set: usize,
            other: [usize; 6],
        }
        #[link(name = "kernel32")]
        extern "system" {
            fn K32GetProcessMemoryInfo(process: isize, counters: *mut Counters, size: u32) -> i32;
        }
        let mut counters = Counters {
            size: std::mem::size_of::<Counters>() as u32,
            ..Counters::default()
        };
        // -1 is the current process.
        let done = unsafe { K32GetProcessMemoryInfo(-1, &mut counters, counters.size) };
        (done != 0).then_some((counters.working_set, counters.peak_working_set))
    }

    #[cfg(not(windows))]
    pub(crate) fn memory() -> Option<(usize, usize)> {
        None
    }

    #[tokio::test]
    #[ignore = "downloads about 36 MB from Mozilla; run by hand"]
    async fn english_to_japanese_is_fetched_once_and_kept() {
        let folder = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
        let online = Firefox::new(
            folder.path().into(),
            SETTINGS.into(),
            reqwest::Client::new(),
        );
        let started = std::time::Instant::now();
        let route = online.route("en", "ja").await.unwrap();
        println!("fetched and loaded en-ja in {:?}", started.elapsed());
        assert_eq!(route.len(), 1);
        let kept = folder.path().join("en-ja");
        let mut names: Vec<String> = std::fs::read_dir(&kept)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        println!("{names:?}");
        assert_eq!(
            names,
            [
                "files.json",
                "lex.50.50.enja.s2t.bin",
                "model.enja.intgemm.alphas.bin",
                "srcvocab.enja.spm",
                "trgvocab.enja.spm"
            ]
        );
        let offline = Firefox::new(folder.path().into(), NOWHERE.into(), reqwest::Client::new());
        assert_eq!(offline.route("en", "ja").await.unwrap().len(), 1);
        assert!(offline.route("ja", "en").await.is_err());
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "downloads Mozilla's English to Japanese model once; run by hand"]
    async fn english_is_translated_to_japanese_here() {
        let folder = std::env::temp_dir().join("inocode-firefox-models");
        let online = Firefox::new(folder.clone(), SETTINGS.into(), reqwest::Client::new());
        let pair = ("en".to_string(), "ja".to_string());
        if !online.kept(&pair).await {
            let catalog = online.catalog().await.unwrap();
            let files = choose(&catalog.records, "en", "ja").unwrap();
            online.fetch(&catalog, &pair, &files).await.unwrap();
        }
        println!("memory before loading (working set, peak): {:?}", memory());
        let firefox = Firefox::new(folder, NOWHERE.into(), reqwest::Client::new());
        let started = std::time::Instant::now();
        let route = firefox.route("en", "ja").await.unwrap();
        println!(
            "loaded from disk in {:?}; weights in memory: {} bytes",
            started.elapsed(),
            route[0].model.size()
        );
        println!("memory after loading: {:?}", memory());
        let texts = [
            "I should check the configuration file before running the tests.",
            "The user wants a summary of the changes in this pull request.",
            "Let me look at how the server handles requests that time out.",
            "This function returns an error when the file does not exist. Then the caller retries three times.",
        ];
        for text in texts {
            let started = std::time::Instant::now();
            let out = firefox.translate(&[text.to_string()], "ja").await.unwrap();
            println!(
                "{text}
  -> {}  ({:?})",
                out[0],
                started.elapsed()
            );
            assert!(source_language(&out[0]) == "ja", "{out:?}");
        }
        println!("memory after translating: {:?}", memory());
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "downloads Mozilla's Japanese to English and English to Chinese models; run by hand"]
    async fn japanese_goes_to_chinese_through_english_here() {
        let folder = std::env::temp_dir().join("inocode-firefox-models");
        let firefox = Firefox::new(folder, SETTINGS.into(), reqwest::Client::new());
        for (text, to) in [
            ("テストを実行する前に設定ファイルを確認します。", "en"),
            ("テストを実行する前に設定ファイルを確認します。", "zh-Hans"),
        ] {
            let started = std::time::Instant::now();
            let out = firefox.translate(&[text.to_string()], to).await.unwrap();
            println!(
                "{text} ({to})
  -> {}  ({:?})",
                out[0],
                started.elapsed()
            );
            assert_eq!(
                source_language(&out[0]),
                if to == "en" { "en" } else { "zh-Hans" }
            );
        }
    }
}
