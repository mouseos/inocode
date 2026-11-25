//! Translation with the models Firefox translates pages with, run here
//! offline: the models are fetched from Mozilla's Remote Settings once,
//! checked and kept on disk, then loaded into memory and run on the CPU.
//! The catalog's content signature is checked as Firefox checks it.
//! Catalog facts and choices: docs/evidence/translation.md.

use std::cmp::Ordering;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt::Write as _;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256, Sha384};
use x509_cert::der::{Decode, Encode};
use x509_cert::ext::pkix::name::GeneralName;
use x509_cert::ext::pkix::{BasicConstraints, ExtendedKeyUsage, KeyUsage, SubjectAltName};
use x509_cert::spki::ObjectIdentifier;
use x509_cert::Certificate;

use super::marian::{Model, Shortlist};
use super::sentencepiece::Vocabulary;
use super::{InstalledModel, Languages, License, Service};
use crate::ErrorInfo;

/// Mozilla's Remote Settings server.
pub(super) const SETTINGS: &str = "https://firefox.settings.services.mozilla.com/v1";
const COLLECTION: &str = "buckets/main/collections/translations-models-v2";
/// The SHA-256 of Mozilla's content signature root, the one Firefox
/// trusts (security/manager/ssl/content-signature-prod.pem).
const ROOT_SHA256: &str = "c8a80e9afaef4e219b6fb5d7a71d0f101223bac5001ac28f9b0d43dc59a106db";
/// The name the signing certificate must carry for the main bucket
/// (`DEFAULT_SIGNER` in services/settings/remote-settings.sys.mjs).
const SIGNER: &str = "remote-settings.content-signature.mozilla.org";
/// OneCRL, the certificates Mozilla revokes, and the name its signing
/// certificate carries (`SECURITY_STATE_SIGNER` in
/// security/manager/ssl/RemoteSecuritySettings.sys.mjs).
const ONECRL: &str = "buckets/security-state/collections/onecrl";
const ONECRL_SIGNER: &str = "onecrl.content-signature.mozilla.org";
/// Files in the models folder: OneCRL's revocations as last verified, and
/// when each settings server asked not to be asked again before.
const ONECRL_FILE: &str = "onecrl.json";
const BACKOFF_FILE: &str = "settings-backoff.json";
/// The folder in a models folder that holds the lock files.
const LOCKS: &str = ".locks";
/// What the signature covers before the catalog
/// (security/manager/ssl/ContentSignatureVerifier.cpp).
const SIGNATURE_PREFIX: &str = "Content-Signature:\x00";
const LARGEST_CHAIN: u64 = 64 << 10;
/// Where Mozilla states the models' license.
const MODELS_LICENSE: &str = "https://github.com/mozilla/translations/blob/main/README.md";
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
    #[serde(default = "license")]
    license: License,
}

fn license() -> License {
    License {
        name: "MPL-2.0".into(),
        source: MODELS_LICENSE.into(),
        terms: None,
    }
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
            license: license(),
        })
}

/// The catalog's files for `from` → `to` when they are a newer version
/// than `kept`.
fn newer(records: &[Value], from: &str, to: &str, kept: &Files) -> Option<Files> {
    let found = choose(records, from, to)?;
    (version(&found.version)? > version(&kept.version).unwrap_or((0, 0))).then_some(found)
}

/// What the catalog's models translate.
fn catalog_languages(records: &[Value]) -> Languages {
    let pairs: BTreeSet<(String, String)> = records
        .iter()
        .filter_map(|record| {
            Some((
                record["sourceLanguage"].as_str()?.to_string(),
                record["targetLanguage"].as_str()?.to_string(),
            ))
        })
        .collect();
    let direct: Vec<(String, String)> = pairs
        .into_iter()
        .filter(|(from, to)| choose(records, from, to).is_some())
        .collect();
    let collect = |side: fn(&(String, String)) -> &String| -> Vec<String> {
        let all: BTreeSet<&String> = direct.iter().map(side).collect();
        all.into_iter().cloned().collect()
    };
    Languages {
        from: collect(|pair| &pair.0),
        to: collect(|pair| &pair.1),
        direct,
    }
}

/// `from` and `to` of a pair folder named `<from>-<to>`; every pair in the
/// catalog has English on one side, which tells where the name splits.
fn pair_of(name: &str) -> Option<(String, String)> {
    let (from, to) = if let Some(to) = name.strip_prefix("en-") {
        ("en", to)
    } else if let Some(from) = name.strip_suffix("-en") {
        (from, "en")
    } else {
        name.split_once('-')?
    };
    (language_tag(from) && language_tag(to)).then(|| (from.to_string(), to.to_string()))
}

/// A language tag such as `ja` or `zh-Hans`, which is safe in a folder
/// name.
pub(super) fn language_tag(tag: &str) -> bool {
    (1..=16).contains(&tag.len())
        && tag
            .split('-')
            .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_alphanumeric()))
}

/// The bytes Remote Settings signs for a collection: the prefix, then the
/// records and timestamp as canonical JSON (`canonicalStringify` in
/// services/settings/RemoteSettings.worker.mjs). Records are sorted by id,
/// deleted ones dropped and, of records with the same id, the last kept.
fn signed_message(records: &[Value], timestamp: u64) -> String {
    let mut sorted: Vec<&Value> = records
        .iter()
        .filter(|record| record["deleted"] != true)
        .collect();
    let id = |record: &Value| record["id"].as_str().unwrap_or("").to_string();
    sorted.sort_by(|a, b| utf16_order(&id(a), &id(b)));
    let kept: Vec<&Value> = sorted
        .iter()
        .enumerate()
        .filter(|(at, record)| sorted.get(at + 1).is_none_or(|next| id(next) != id(record)))
        .map(|(_, record)| *record)
        .collect();
    let mut out = String::from(SIGNATURE_PREFIX);
    canonical(
        &json!({"data": kept, "last_modified": timestamp.to_string()}),
        &mut out,
    );
    out
}

/// JavaScript's string order: by UTF-16 code unit.
fn utf16_order(a: &str, b: &str) -> Ordering {
    a.encode_utf16().cmp(b.encode_utf16())
}

/// `value` written as Firefox's CanonicalJSON.sys.mjs writes it with
/// jsesc: no spaces, object keys in JavaScript's order, numbers as
/// JavaScript writes them, and in strings only printable ASCII left as it
/// is.
fn canonical(value: &Value, out: &mut String) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Number(number) => out.push_str(&js_number(number)),
        Value::String(text) => quote(text, out),
        Value::Array(items) => {
            out.push('[');
            for (at, item) in items.iter().enumerate() {
                if at > 0 {
                    out.push(',');
                }
                canonical(item, out);
            }
            out.push(']');
        }
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort_by(|a, b| utf16_order(a, b));
            out.push('{');
            for (at, key) in keys.into_iter().enumerate() {
                if at > 0 {
                    out.push(',');
                }
                quote(key, out);
                out.push(':');
                canonical(&map[key], out);
            }
            out.push('}');
        }
    }
}

/// A string as jsesc writes it with `json` and `lowercaseHex`: each UTF-16
/// code unit outside printable ASCII as `\uxxxx`.
fn quote(text: &str, out: &mut String) {
    out.push('"');
    for unit in text.encode_utf16() {
        match unit {
            0x22 => out.push_str("\\\""),
            0x5C => out.push_str("\\\\"),
            0x08 => out.push_str("\\b"),
            0x0C => out.push_str("\\f"),
            0x0A => out.push_str("\\n"),
            0x0D => out.push_str("\\r"),
            0x09 => out.push_str("\\t"),
            0x20..=0x7E => out.push(unit as u8 as char),
            _ => {
                let _ = write!(out, "\\u{unit:04x}");
            }
        }
    }
    out.push('"');
}

/// A number as JavaScript's `JSON.stringify` writes it; CanonicalJSON
/// writes -0 as `-0`.
fn js_number(number: &serde_json::Number) -> String {
    const EXACT: u64 = 1 << 53;
    if let Some(whole) = number.as_u64().filter(|&n| n <= EXACT) {
        return whole.to_string();
    }
    if let Some(whole) = number.as_i64().filter(|n| n.unsigned_abs() <= EXACT) {
        return whole.to_string();
    }
    let value = number.as_f64().unwrap_or(f64::NAN);
    if !value.is_finite() {
        return "null".into();
    }
    if value == 0.0 {
        return if value.is_sign_negative() { "-0" } else { "0" }.into();
    }
    // The shortest digits that give the value back, as JavaScript picks
    // them; `n` is where the decimal point goes (ECMA-262 Number::toString).
    let shortest = format!("{:e}", value.abs());
    let (mantissa, exponent) = shortest.split_once('e').unwrap_or((&shortest, "0"));
    let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
    let k = digits.len() as i32;
    let n = exponent.parse::<i32>().unwrap_or(0) + 1;
    let body = if k <= n && n <= 21 {
        format!("{digits}{}", "0".repeat((n - k) as usize))
    } else if 0 < n && n <= 21 {
        format!("{}.{}", &digits[..n as usize], &digits[n as usize..])
    } else if -6 < n && n <= 0 {
        format!("0.{}{digits}", "0".repeat((-n) as usize))
    } else {
        let sign = if n - 1 < 0 { '-' } else { '+' };
        let rest = if k == 1 {
            String::new()
        } else {
            format!(".{}", &digits[1..])
        };
        format!("{}{rest}e{sign}{}", &digits[..1], (n - 1).abs())
    };
    if value < 0.0 {
        format!("-{body}")
    } else {
        body
    }
}

/// The certificates of a PEM chain, in the order given.
fn pem_certificates(pem: &[u8]) -> Result<Vec<Vec<u8>>, String> {
    let text = std::str::from_utf8(pem).map_err(|_| "the certificate chain is not text")?;
    let mut out = Vec::new();
    let mut block: Option<String> = None;
    for line in text.lines().map(str::trim) {
        if line == "-----BEGIN CERTIFICATE-----" {
            block = Some(String::new());
        } else if line == "-----END CERTIFICATE-----" {
            let data = block.take().ok_or("the certificate chain is broken")?;
            out.push(
                base64::engine::general_purpose::STANDARD
                    .decode(data)
                    .map_err(|error| format!("the certificate chain: {error}"))?,
            );
        } else if let Some(data) = block.as_mut() {
            data.push_str(line);
        }
    }
    if block.is_some() || out.is_empty() {
        return Err("the certificate chain is broken".into());
    }
    Ok(out)
}

const ECDSA_WITH_SHA384: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.2.840.10045.4.3.3");
const RSA_WITH_SHA384: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.2.840.113549.1.1.12");
const EC_PUBLIC_KEY: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.2.840.10045.2.1");
const SECP384R1: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.3.132.0.34");
const CODE_SIGNING: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.3.6.1.5.5.7.3.3");
const BASIC_CONSTRAINTS: ObjectIdentifier = ObjectIdentifier::new_unwrap("2.5.29.19");
const KEY_USAGE: ObjectIdentifier = ObjectIdentifier::new_unwrap("2.5.29.15");
const EXTENDED_KEY_USAGE: ObjectIdentifier = ObjectIdentifier::new_unwrap("2.5.29.37");
const SUBJECT_ALT_NAME: ObjectIdentifier = ObjectIdentifier::new_unwrap("2.5.29.17");
/// Extensions a critical one may be; any other critical one is refused,
/// as mozilla::pkix refuses it. Name constraints are not checked: the
/// signer name is fixed and inside the ones Mozilla's intermediate sets.
const KNOWN_EXTENSIONS: [&str; 7] = [
    "2.5.29.19",
    "2.5.29.15",
    "2.5.29.37",
    "2.5.29.17",
    "2.5.29.30",
    "2.5.29.14",
    "2.5.29.35",
];

/// The extension `oid` of `certificate` decoded as `T`.
fn extension<'a, T: Decode<'a>>(
    certificate: &'a Certificate,
    oid: ObjectIdentifier,
) -> Result<Option<T>, String> {
    let Some(found) = certificate
        .tbs_certificate
        .extensions
        .iter()
        .flatten()
        .find(|extension| extension.extn_id == oid)
    else {
        return Ok(None);
    };
    T::from_der(found.extn_value.as_bytes())
        .map(Some)
        .map_err(|error| format!("the certificate extension {oid}: {error}"))
}

fn p384_key(certificate: &Certificate) -> Result<p384::ecdsa::VerifyingKey, String> {
    let info = &certificate.tbs_certificate.subject_public_key_info;
    let curve = info
        .algorithm
        .parameters
        .as_ref()
        .and_then(|parameters| parameters.decode_as::<ObjectIdentifier>().ok());
    if info.algorithm.oid != EC_PUBLIC_KEY || curve != Some(SECP384R1) {
        return Err("the certificate's key is not a P-384 key".into());
    }
    p384::ecdsa::VerifyingKey::from_sec1_bytes(info.subject_public_key.raw_bytes())
        .map_err(|_| "the certificate's key is broken".to_string())
}

/// Checks that `issuer` signed `child`, with ECDSA P-384 or RSA and
/// SHA-384, the two kinds Mozilla's chain uses.
fn issued_by(child: &Certificate, issuer: &Certificate) -> Result<(), String> {
    let names = |certificate: &Certificate, of: bool| {
        let name = if of {
            &certificate.tbs_certificate.subject
        } else {
            &certificate.tbs_certificate.issuer
        };
        name.to_der().map_err(|error| error.to_string())
    };
    if names(child, false)? != names(issuer, true)? {
        return Err("a certificate's issuer is not the next one in the chain".into());
    }
    let signed = child
        .tbs_certificate
        .to_der()
        .map_err(|error| error.to_string())?;
    let signature = child
        .signature
        .as_bytes()
        .ok_or("a certificate's signature is broken")?;
    let bad = || "a certificate's signature does not verify".to_string();
    match child.signature_algorithm.oid {
        ECDSA_WITH_SHA384 => {
            use p384::ecdsa::signature::Verifier;
            let signature = p384::ecdsa::Signature::from_der(signature).map_err(|_| bad())?;
            p384_key(issuer)?
                .verify(&signed, &signature)
                .map_err(|_| bad())
        }
        RSA_WITH_SHA384 => {
            use rsa::pkcs8::DecodePublicKey;
            let info = issuer
                .tbs_certificate
                .subject_public_key_info
                .to_der()
                .map_err(|error| error.to_string())?;
            let key = rsa::RsaPublicKey::from_public_key_der(&info)
                .map_err(|_| "the issuer's key is not an RSA key".to_string())?;
            key.verify(
                rsa::Pkcs1v15Sign::new::<Sha384>(),
                &Sha384::digest(&signed),
                signature,
            )
            .map_err(|_| bad())
        }
        other => Err(format!("a certificate is signed with {other}")),
    }
}

/// Certificates OneCRL revokes, by issuer and serial number and by subject
/// and the SHA-256 of the public key, each part in standard base64.
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
struct Revoked {
    by_issuer: BTreeSet<(String, String)>,
    by_key: BTreeSet<(String, String)>,
}

impl Revoked {
    /// The revocations in OneCRL's `records`, read as Firefox's
    /// `updateCertBlocklist` reads them: `issuerName` and `serialNumber`,
    /// else `subject` and `pubKeyHash`; parts that are not base64 are
    /// left out, as cert_storage leaves them out.
    fn from_records(records: &[Value]) -> Self {
        let part = |value: &Value| {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(value.as_str()?)
                .ok()?;
            Some(base64::engine::general_purpose::STANDARD.encode(bytes))
        };
        let mut out = Self::default();
        for record in records {
            if let (Some(issuer), Some(serial)) =
                (part(&record["issuerName"]), part(&record["serialNumber"]))
            {
                out.by_issuer.insert((issuer, serial));
            } else if let (Some(subject), Some(key)) =
                (part(&record["subject"]), part(&record["pubKeyHash"]))
            {
                out.by_key.insert((subject, key));
            }
        }
        out
    }

    /// Whether `certificate` is revoked, looked up as cert_storage's
    /// `get_revocation_state` looks it up.
    fn revokes(&self, certificate: &Certificate) -> Result<bool, String> {
        let encode = |bytes: &[u8]| base64::engine::general_purpose::STANDARD.encode(bytes);
        fn der(value: &impl Encode) -> Result<Vec<u8>, String> {
            value.to_der().map_err(|error| error.to_string())
        }
        let tbs = &certificate.tbs_certificate;
        let by_issuer = (
            encode(&der(&tbs.issuer)?),
            encode(tbs.serial_number.as_bytes()),
        );
        let by_key = (
            encode(&der(&tbs.subject)?),
            encode(&Sha256::digest(der(&tbs.subject_public_key_info)?)),
        );
        Ok(self.by_issuer.contains(&by_issuer) || self.by_key.contains(&by_key))
    }
}

/// The key that signs a collection, from the PEM chain at the signature's
/// `x5u`, checked as Firefox checks it: the chain runs from the signing
/// certificate to Mozilla's root, each certificate signed by the next, all
/// valid at `now` (seconds since 1970) and none in `revoked`, the ones
/// above the signer allowed to issue certificates, code signing allowed,
/// and the signer named `signer`.
fn signing_key(
    pem: &[u8],
    now: i64,
    signer: &str,
    revoked: &Revoked,
) -> Result<p384::ecdsa::VerifyingKey, String> {
    let ders = pem_certificates(pem)?;
    let chain: Vec<Certificate> = ders
        .iter()
        .map(|der| Certificate::from_der(der))
        .collect::<Result<_, _>>()
        .map_err(|error| format!("a certificate is broken: {error}"))?;
    if chain.len() < 2 || sha256(ders.last().map_or(&[][..], Vec::as_slice)) != ROOT_SHA256 {
        return Err("the certificate chain does not end at Mozilla's root".into());
    }
    let now = u64::try_from(now).unwrap_or(0);
    for (at, certificate) in chain.iter().enumerate() {
        let validity = &certificate.tbs_certificate.validity;
        if now < validity.not_before.to_unix_duration().as_secs()
            || now > validity.not_after.to_unix_duration().as_secs()
        {
            return Err("a certificate of the chain is not valid now".into());
        }
        if revoked.revokes(certificate)? {
            return Err("a certificate of the chain is revoked in OneCRL".into());
        }
        for extension in certificate.tbs_certificate.extensions.iter().flatten() {
            if extension.critical && !KNOWN_EXTENSIONS.contains(&&*extension.extn_id.to_string()) {
                return Err(format!(
                    "a certificate has the unknown critical extension {}",
                    extension.extn_id
                ));
            }
        }
        let purposes: Option<ExtendedKeyUsage> = extension(certificate, EXTENDED_KEY_USAGE)?;
        let code_signing = purposes
            .as_ref()
            .map(|usage| usage.0.contains(&CODE_SIGNING));
        if at == 0 {
            if code_signing != Some(true) {
                return Err("the signing certificate is not for code signing".into());
            }
            let names: Option<SubjectAltName> = extension(certificate, SUBJECT_ALT_NAME)?;
            let named = names
                .iter()
                .flat_map(|names| &names.0)
                .any(|name| matches!(name, GeneralName::DnsName(dns) if dns.to_string() == signer));
            if !named {
                return Err(format!("the signing certificate is not for {signer}"));
            }
        } else {
            if code_signing == Some(false) {
                return Err("a certificate of the chain is not for code signing".into());
            }
            let constraints: Option<BasicConstraints> = extension(certificate, BASIC_CONSTRAINTS)?;
            let usage: Option<KeyUsage> = extension(certificate, KEY_USAGE)?;
            if !constraints.is_some_and(|constraints| constraints.ca)
                || usage.is_some_and(|usage| !usage.key_cert_sign())
            {
                return Err("a certificate of the chain may not issue certificates".into());
            }
        }
    }
    for pair in chain.windows(2) {
        issued_by(&pair[0], &pair[1])?;
    }
    p384_key(&chain[0])
}

/// Checks the base64url ECDSA P-384 SHA-384 `signature` (r and s, 48 bytes
/// each) of `message` with the chain `pem` (see [`signing_key`]).
fn check_signature(
    message: &str,
    signature: &str,
    pem: &[u8],
    now: i64,
    signer: &str,
    revoked: &Revoked,
) -> Result<(), String> {
    use p384::ecdsa::signature::Verifier;
    let key = signing_key(pem, now, signer, revoked)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(signature.trim_end_matches('='))
        .map_err(|_| "the signature is not base64url")?;
    let signature =
        p384::ecdsa::Signature::from_slice(&bytes).map_err(|_| "the signature is broken")?;
    key.verify(message.as_bytes(), &signature)
        .map_err(|_| "the collection does not match its signature".to_string())
}

/// The `(x5u, signature)` of each P-384 signature in a collection's
/// metadata, the `signatures` list when there is one.
fn signatures(metadata: &Value) -> Vec<(&str, &str)> {
    let list = match metadata["signatures"].as_array() {
        Some(list) => list.iter().collect(),
        None => vec![&metadata["signature"]],
    };
    list.into_iter()
        .filter(|signature| signature["mode"] == "p384ecdsa")
        .filter_map(|signature| {
            Some((signature["x5u"].as_str()?, signature["signature"].as_str()?))
        })
        .collect()
}

/// The records of a changeset when one of its signatures verifies with
/// its chain in `chains` (by `x5u`) at `now`, signed by `signer` and with
/// no certificate in `revoked`; deleted records left out.
fn verified(
    changeset: &Value,
    chains: &[(String, Vec<u8>)],
    now: i64,
    signer: &str,
    revoked: &Revoked,
) -> Result<Vec<Value>, String> {
    let records = changeset["changes"]
        .as_array()
        .ok_or("the collection has no records")?;
    let timestamp = changeset["timestamp"]
        .as_u64()
        .ok_or("the collection has no timestamp")?;
    let message = signed_message(records, timestamp);
    let mut last = String::from("it has no P-384 signature");
    for (x5u, signature) in signatures(&changeset["metadata"]) {
        let Some((_, pem)) = chains.iter().find(|(url, _)| url == x5u) else {
            last = format!("its certificate chain {x5u} was not fetched");
            continue;
        };
        match check_signature(&message, signature, pem, now, signer, revoked) {
            Ok(()) => {
                return Ok(records
                    .iter()
                    .filter(|record| record["deleted"] != true)
                    .cloned()
                    .collect())
            }
            Err(error) => last = error,
        }
    }
    Err(format!(
        "the collection's signature does not verify: {last}"
    ))
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

/// Held to read a kept model's files and to replace or delete a model's
/// folder, so that no one in this process reads a folder half replaced.
/// Taken after a folder's lock files, never before.
pub(super) static DISK: tokio::sync::RwLock<()> = tokio::sync::RwLock::const_new(());

/// A lock on a file in a models folder's [`LOCKS`] folder, which other
/// processes see too. The system lets it go when the file is closed, which
/// happens when this is dropped or the process ends, so a process that
/// stops leaves no lock behind.
pub(super) struct FileLock {
    _file: std::fs::File,
}

/// Each model folder has two locks. `Write` is held, exclusive, for the
/// whole of fetching, updating or deleting the model, so that only one
/// process at a time writes it. `Read` is held shared while the model's
/// files are read, and exclusive while its folder is swapped or deleted.
/// A process takes `Write` before `Read`, and both before [`DISK`].
#[derive(Clone, Copy)]
pub(super) enum Lock {
    Write,
    Read { exclusive: bool },
}

/// Takes the lock `kind` of the model folder `name` in `base`, waiting for
/// other holders.
pub(super) async fn lock(base: &Path, name: &str, kind: Lock) -> Result<FileLock, String> {
    let locks = base.join(LOCKS);
    let (suffix, exclusive) = match kind {
        Lock::Write => ("write", true),
        Lock::Read { exclusive } => ("read", exclusive),
    };
    let path = locks.join(format!("{name}.{suffix}"));
    let shown = path.display().to_string();
    tokio::task::spawn_blocking(move || {
        std::fs::create_dir_all(&locks)?;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)?;
        if exclusive {
            file.lock()?;
        } else {
            file.lock_shared()?;
        }
        Ok::<_, std::io::Error>(FileLock { _file: file })
    })
    .await
    .map_err(|error| error.to_string())?
    .map_err(|error| format!("{shown}: {error}"))
}

/// `folder` as the models folder it is in and its name there.
fn split(folder: &Path) -> (&Path, &str) {
    (
        folder.parent().unwrap_or(Path::new("")),
        folder
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(""),
    )
}

/// The shared read lock of the model folder `folder`; `None` when its lock
/// file cannot be made, as in a models folder this user may not write to,
/// where nothing can change the model either.
pub(super) async fn reading(folder: &Path) -> Option<FileLock> {
    let (base, name) = split(folder);
    lock(base, name, Lock::Read { exclusive: false }).await.ok()
}

/// Takes the write lock of the model folder `name` in `base`, then puts
/// right what a process that stopped while updating it left behind: the
/// kept folder is put back from its `.old` copy when only that is there,
/// and the `.update` folder and any other `.old` copy are deleted.
pub(super) async fn writing(base: &Path, name: &str) -> Result<FileLock, String> {
    let held = lock(base, name, Lock::Write).await?;
    let folder = base.join(name);
    let old = folder.with_extension("old");
    if !tokio::fs::try_exists(&folder).await.unwrap_or(true)
        && tokio::fs::try_exists(&old).await.unwrap_or(false)
    {
        let _swapping = lock(base, name, Lock::Read { exclusive: true }).await?;
        let _writing = DISK.write().await;
        tokio::fs::rename(&old, &folder)
            .await
            .map_err(|error| format!("{}: {error}", old.display()))?;
    } else {
        let _ = tokio::fs::remove_dir_all(&old).await;
    }
    let _ = tokio::fs::remove_dir_all(folder.with_extension("update")).await;
    Ok(held)
}

/// The model folders already checked for a newer version in this process.
static CHECKED: LazyLock<std::sync::Mutex<HashSet<PathBuf>>> = LazyLock::new(Default::default);

/// Whether the model in `folder` is still to be checked for a newer
/// version in this process; marks it checked.
pub(super) fn first_check(folder: &Path) -> bool {
    CHECKED
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(folder.to_path_buf())
}

/// Puts the folder `new` in the place of `folder` and deletes the old one;
/// the caller holds the write lock of `folder`.
pub(super) async fn replace_folder(new: &Path, folder: &Path) -> Result<(), String> {
    let old = folder.with_extension("old");
    let (base, name) = split(folder);
    let _swapping = lock(base, name, Lock::Read { exclusive: true }).await?;
    let _writing = DISK.write().await;
    let _ = tokio::fs::remove_dir_all(&old).await;
    tokio::fs::rename(folder, &old)
        .await
        .map_err(|error| format!("{}: {error}", folder.display()))?;
    if let Err(error) = tokio::fs::rename(new, folder).await {
        let _ = tokio::fs::rename(&old, folder).await;
        return Err(format!("{}: {error}", folder.display()));
    }
    let _ = tokio::fs::remove_dir_all(&old).await;
    Ok(())
}

/// Deletes the folder `name` in `base` when it is a real folder directly
/// inside it; `Ok(false)` when there is none.
pub(super) async fn remove_folder(base: &Path, name: &str) -> Result<bool, ErrorInfo> {
    let invalid = |message: String| ErrorInfo::new("invalid_params", message);
    if !tokio::fs::try_exists(base).await.unwrap_or(false) {
        return Ok(false);
    }
    let _writing = writing(base, name).await.map_err(failed)?;
    let path = base.join(name);
    let metadata = match tokio::fs::symlink_metadata(&path).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(failed(format!("{}: {error}", path.display()))),
    };
    if !metadata.is_dir() {
        return Err(invalid(format!("{} is not a model folder", path.display())));
    }
    let real = tokio::fs::canonicalize(&path).await.map_err(failed)?;
    let real_base = tokio::fs::canonicalize(base).await.map_err(failed)?;
    if real.parent() != Some(real_base.as_path()) {
        return Err(invalid(format!(
            "{} is outside the models folder",
            path.display()
        )));
    }
    let _swapping = lock(base, name, Lock::Read { exclusive: true })
        .await
        .map_err(failed)?;
    let _disk = DISK.write().await;
    tokio::fs::remove_dir_all(&real)
        .await
        .map_err(|error| failed(format!("{}: {error}", path.display())))?;
    Ok(true)
}

/// The files of the pair in `folder`, when all of them were fetched; the
/// caller holds the folder's read lock and [`DISK`].
async fn manifest_in(folder: &Path) -> Option<Files> {
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

/// The files of the pair in `folder`, when all of them were fetched.
async fn read_manifest(folder: &Path) -> Option<Files> {
    let _locked = reading(folder).await;
    let _reading = DISK.read().await;
    manifest_in(folder).await
}

/// The time each server asked not to be asked again before, in seconds
/// since 1970, as this process knows it.
pub(super) type Waits = LazyLock<std::sync::Mutex<HashMap<String, i64>>>;

/// When `server` asked not to be asked again before, from `memory` and the
/// file `kept` that other processes write too.
pub(super) async fn asked_to_wait(
    memory: &Waits,
    kept: Option<&Path>,
    server: &str,
) -> Option<i64> {
    let mut until = memory
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(server)
        .copied();
    if let Some(path) = kept {
        let on_disk = tokio::fs::read(path)
            .await
            .ok()
            .and_then(|data| serde_json::from_slice::<HashMap<String, i64>>(&data).ok())
            .and_then(|waits| waits.get(server).copied());
        until = until.max(on_disk);
    }
    until
}

/// Notes that `server` asked not to be asked again before `until`, or no
/// longer asks when `None`, in `memory` and in the file `kept`.
pub(super) async fn set_wait(
    memory: &Waits,
    kept: Option<&Path>,
    server: &str,
    until: Option<i64>,
) {
    let change = |waits: &mut HashMap<String, i64>| match until {
        Some(until) => {
            waits.insert(server.to_string(), until);
        }
        None => {
            waits.remove(server);
        }
    };
    change(
        &mut memory
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()),
    );
    let Some(path) = kept else {
        return;
    };
    let mut waits: HashMap<String, i64> = tokio::fs::read(path)
        .await
        .ok()
        .and_then(|data| serde_json::from_slice(&data).ok())
        .unwrap_or_default();
    change(&mut waits);
    if let Some(parent) = path.parent() {
        let _ = tokio::fs::create_dir_all(parent).await;
    }
    if let Ok(data) = serde_json::to_vec_pretty(&waits) {
        let _ = write_whole(path, &data).await;
    }
}

/// The whole models kept in `models`.
pub(super) async fn installed(models: &Path) -> Vec<InstalledModel> {
    let mut out = Vec::new();
    let Ok(mut entries) = tokio::fs::read_dir(models).await else {
        return out;
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let Some((from, to)) = entry.file_name().to_str().and_then(pair_of) else {
            continue;
        };
        if !entry.file_type().await.is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        let path = entry.path();
        if let Some(files) = read_manifest(&path).await {
            out.push(InstalledModel {
                service: Service::Firefox,
                from,
                to,
                bytes: files.files.iter().map(|file| file.decompressed_size).sum(),
                version: files.version,
                license: files.license,
                path,
            });
        }
    }
    out.sort_by(|a, b| (&a.from, &a.to).cmp(&(&b.from, &b.to)));
    out
}

/// Deletes the kept model for `from` → `to`.
pub(super) async fn remove(models: &Path, from: &str, to: &str) -> Result<bool, ErrorInfo> {
    if !language_tag(from) || !language_tag(to) {
        return Err(ErrorInfo::new(
            "invalid_params",
            format!("{from:?} → {to:?} is not a language pair"),
        ));
    }
    remove_folder(models, &format!("{from}-{to}")).await
}

/// What Firefox's models translate, from the catalog at `settings`.
pub(super) async fn languages(
    settings: &str,
    client: &reqwest::Client,
) -> Result<Languages, ErrorInfo> {
    let catalog = catalog(client, settings, None).await.map_err(failed)?;
    Ok(catalog_languages(&catalog.records))
}

/// Language pairs loaded into memory, by source and target language.
type Loaded = HashMap<(String, String), Arc<Pair>>;

/// The catalog: where attachments are, and the model records.
struct Catalog {
    attachments: reqwest::Url,
    records: Vec<Value>,
}

/// The catalogs fetched in this process, by settings server: the catalog
/// is fetched at most once per process.
static CATALOGS: LazyLock<std::sync::Mutex<HashMap<String, Arc<Catalog>>>> =
    LazyLock::new(Default::default);

/// When each settings server asked not to be asked again before.
static SETTINGS_WAITS: Waits = LazyLock::new(Default::default);

/// The seconds an answer of a settings server asks to wait before the next
/// request: the larger of its `Backoff` and `Retry-After` headers read as
/// whole seconds, a header that is not one left out, as application-services'
/// remote_settings client reads them.
fn backoff_seconds(headers: &reqwest::header::HeaderMap) -> u64 {
    let seconds = |name: &str| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.trim().parse::<u64>().ok())
            .unwrap_or(0)
    };
    seconds("Backoff").max(seconds("Retry-After"))
}

/// Requests to a settings server, which are not sent while the server has
/// asked to wait.
struct Settings<'a> {
    client: &'a reqwest::Client,
    server: &'a str,
    /// The file the waits are kept in, when there is a models folder.
    waits: Option<PathBuf>,
}

impl Settings<'_> {
    /// The body at `path` on the server, at most `limit` bytes.
    async fn get(&self, path: &str, limit: u64) -> Result<Vec<u8>, String> {
        let now = chrono::Utc::now().timestamp();
        if let Some(until) = asked_to_wait(&SETTINGS_WAITS, self.waits.as_deref(), self.server)
            .await
            .filter(|&until| until > now)
        {
            return Err(format!(
                "Mozilla's settings server {} asked not to be asked again for {} more seconds",
                self.server,
                until - now
            ));
        }
        let response = self
            .client
            .get(format!("{}/{path}", self.server))
            .timeout(CATALOG_WAIT)
            .send()
            .await
            .map_err(|error| error.to_string())?;
        let wait = backoff_seconds(response.headers());
        if wait > 0 {
            let until = now.saturating_add(i64::try_from(wait).unwrap_or(i64::MAX));
            set_wait(
                &SETTINGS_WAITS,
                self.waits.as_deref(),
                self.server,
                Some(until),
            )
            .await;
        }
        body(response, limit).await
    }

    async fn json(&self, path: &str) -> Result<Value, String> {
        let data = self.get(path, LARGEST_CATALOG).await?;
        serde_json::from_slice(&data).map_err(|error| format!("{}/{path}: {error}", self.server))
    }

    /// The verified records of `collection`, signed by `signer` with no
    /// certificate in `revoked`.
    async fn collection(
        &self,
        collection: &str,
        signer: &str,
        revoked: &Revoked,
    ) -> Result<Vec<Value>, String> {
        let changeset = self
            .json(&format!("{collection}/changeset?_expected=0"))
            .await?;
        let mut chains = Vec::new();
        for (x5u, _) in signatures(&changeset["metadata"]) {
            if !x5u.starts_with("https://") {
                continue;
            }
            let fetched = async {
                let response = self
                    .client
                    .get(x5u)
                    .timeout(CATALOG_WAIT)
                    .send()
                    .await
                    .map_err(|error| error.to_string())?;
                body(response, LARGEST_CHAIN).await
            };
            if let Ok(pem) = fetched.await {
                chains.push((x5u.to_string(), pem));
            }
        }
        verified(
            &changeset,
            &chains,
            chrono::Utc::now().timestamp(),
            signer,
            revoked,
        )
        .map_err(|error| format!("{collection}: {error}"))
    }

    /// OneCRL's revocations, its own signature checked against those known
    /// before (kept in `kept`, none without it), as Firefox checks OneCRL
    /// against its certificate store; those known before when it cannot
    /// be fetched or does not verify.
    async fn revoked(&self, kept: Option<&Path>) -> Revoked {
        let before: Revoked = match kept {
            Some(path) => tokio::fs::read(path)
                .await
                .ok()
                .and_then(|data| serde_json::from_slice(&data).ok())
                .unwrap_or_default(),
            None => Revoked::default(),
        };
        let Ok(records) = self.collection(ONECRL, ONECRL_SIGNER, &before).await else {
            return before;
        };
        let now = Revoked::from_records(&records);
        if let (Some(path), Ok(data)) = (kept, serde_json::to_vec(&now)) {
            if now != before {
                let _ = write_whole(path, &data).await;
            }
        }
        now
    }
}

/// The verified catalog at `settings`, fetched when this process has not
/// fetched it yet; `models` is the models folder, where OneCRL and the
/// server's request to wait are kept.
async fn catalog(
    client: &reqwest::Client,
    settings: &str,
    models: Option<&Path>,
) -> Result<Arc<Catalog>, String> {
    let settings = settings.trim_end_matches('/');
    let kept = CATALOGS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(settings)
        .cloned();
    if let Some(kept) = kept {
        return Ok(kept);
    }
    if let Some(models) = models {
        let _ = tokio::fs::create_dir_all(models).await;
    }
    let server = Settings {
        client,
        server: settings,
        waits: models.map(|models| models.join(BACKOFF_FILE)),
    };
    let info = server.json("").await?;
    let base = info["capabilities"]["attachments"]["base_url"]
        .as_str()
        .ok_or("the settings server names no attachment address")?;
    let attachments = reqwest::Url::parse(base).map_err(|error| error.to_string())?;
    let revoked = server
        .revoked(models.map(|models| models.join(ONECRL_FILE)).as_deref())
        .await;
    let records = server
        .collection(COLLECTION, SIGNER, &revoked)
        .await
        .map_err(|error| format!("the model catalog: {error}"))?;
    let fetched = Arc::new(Catalog {
        attachments,
        records,
    });
    CATALOGS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(settings.to_string(), fetched.clone());
    Ok(fetched)
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
                routes.insert(from, self.route(from, &to, false).await?);
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
    /// disk yet, or a newer version of them when the catalog lists one.
    pub(super) async fn prepare(&self, from: &str, to: &str) -> Result<(), ErrorInfo> {
        self.route(from, &model_language(to), true).await.map(drop)
    }

    /// The loaded pairs that take `from` to `to`; fetched first when they
    /// are not on disk yet. A pair already on disk is checked for a newer
    /// version the first time this process uses it: before loading when
    /// `wait`, else alongside, the new version then used from the next
    /// load.
    async fn route(&self, from: &str, to: &str, wait: bool) -> Result<Vec<Arc<Pair>>, ErrorInfo> {
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
        let mut on_disk = Vec::new();
        if self.kept(&direct).await {
            pairs.push(direct);
            on_disk.clone_from(&pairs);
        } else if through_pivot {
            pairs.extend(pivot);
            on_disk.clone_from(&pairs);
        } else {
            let catalog = self.catalog().await.map_err(failed)?;
            let plan = plan(&catalog.records, from, to).ok_or_else(|| {
                failed(format!(
                    "Firefox has no translation model from {from} to {to}"
                ))
            })?;
            for (from, to, files) in plan {
                let pair = (from, to);
                if self.kept(&pair).await {
                    on_disk.push(pair.clone());
                } else {
                    first_check(&self.pair_folder(&pair));
                    self.fetch_pair(&catalog, &pair, &files)
                        .await
                        .map_err(failed)?;
                }
                pairs.push(pair);
            }
        }
        for pair in on_disk {
            if !first_check(&self.pair_folder(&pair)) {
                continue;
            }
            if wait {
                let _ = self.update(&pair).await;
            } else {
                let this = self.clone();
                tokio::spawn(async move {
                    let _ = this.update(&pair).await;
                });
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
        read_manifest(&self.pair_folder(pair)).await
    }

    async fn kept(&self, pair: &(String, String)) -> bool {
        self.loaded.lock().await.contains_key(pair) || self.manifest(pair).await.is_some()
    }

    async fn load(&self, pair: (String, String)) -> Result<Arc<Pair>, ErrorInfo> {
        let mut loaded = self.loaded.lock().await;
        if let Some(found) = loaded.get(&pair) {
            return Ok(found.clone());
        }
        let folder = self.pair_folder(&pair);
        let locked = reading(&folder).await;
        let reading = DISK.read().await;
        let files = manifest_in(&folder)
            .await
            .ok_or_else(|| failed(format!("the {}-{} model is missing", pair.0, pair.1)))?;
        let model = tokio::task::spawn_blocking(move || {
            let _reading = (locked, reading);
            Pair::load(&folder, &files)
        })
        .await
        .map_err(failed)?
        .map_err(|error| failed(format!("the {}-{} model: {error}", pair.0, pair.1)))?;
        let model = Arc::new(model);
        loaded.insert(pair, model.clone());
        Ok(model)
    }

    async fn catalog(&self) -> Result<Arc<Catalog>, String> {
        catalog(&self.client, &self.settings, Some(&self.folder)).await
    }

    /// Fetches the pair's `files` into its folder, unless another process
    /// fetched it while this one waited for the write lock.
    async fn fetch_pair(
        &self,
        catalog: &Catalog,
        pair: &(String, String),
        files: &Files,
    ) -> Result<(), String> {
        let folder = self.pair_folder(pair);
        let (base, name) = split(&folder);
        let _writing = writing(base, name).await?;
        if self.manifest(pair).await.is_some() {
            return Ok(());
        }
        self.fetch(catalog, &folder, files).await
    }

    /// Fetches the newest version of the kept pair into a folder of its
    /// own and puts it in place of the kept one, when the catalog lists a
    /// newer version; `Ok(false)` when it does not.
    pub(super) async fn update(&self, pair: &(String, String)) -> Result<bool, String> {
        let folder = self.pair_folder(pair);
        let (base, name) = split(&folder);
        if self.manifest(pair).await.is_none() {
            return Ok(false);
        }
        let _writing = writing(base, name).await?;
        let Some(kept) = self.manifest(pair).await else {
            return Ok(false);
        };
        let catalog = self.catalog().await?;
        let Some(files) = newer(&catalog.records, &pair.0, &pair.1, &kept) else {
            return Ok(false);
        };
        let staging = folder.with_extension("update");
        if let Err(error) = self.fetch(&catalog, &staging, &files).await {
            let _ = tokio::fs::remove_dir_all(&staging).await;
            return Err(error);
        }
        replace_folder(&staging, &folder).await?;
        self.loaded.lock().await.remove(pair);
        Ok(true)
    }

    /// Downloads, checks and keeps each file of `files` in `folder`, then
    /// their list.
    async fn fetch(&self, catalog: &Catalog, folder: &Path, files: &Files) -> Result<(), String> {
        tokio::fs::create_dir_all(folder)
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

    fn canonical_text(value: &Value) -> String {
        let mut out = String::new();
        canonical(value, &mut out);
        out
    }

    #[test]
    fn canonical_json_follows_firefox() {
        // Keys in UTF-16 order: U+E000 is one unit, U+10000 two starting
        // with 0xD800, so U+10000 comes first, unlike in code point order.
        let value =
            json!({"b": 1, "a": [true, false, null], "\u{e000}": 0, "\u{10000}": {}, "A": []});
        assert_eq!(
            canonical_text(&value),
            r#"{"A":[],"a":[true,false,null],"b":1,"\ud800\udc00":{},"\ue000":0}"#
        );
        assert_eq!(
            canonical_text(&json!(
                "q\" b\\ /'\u{8}\u{c}\n\r\t\u{1}\u{7f}\u{e9}\u{1f600}"
            )),
            r#""q\" b\\ /'\b\f\n\r\t\u0001\u007f\u00e9\ud83d\ude00""#
        );
        let numbers = json!([
            0,
            -5,
            1.5,
            -0.0,
            1e21,
            1e20,
            1e-7,
            0.000001,
            123.456,
            1.0,
            2.5e-10,
            9007199254740993u64,
            1152921504606846976u64,
            -1e300
        ]);
        assert_eq!(
            canonical_text(&numbers),
            "[0,-5,1.5,-0,1e+21,100000000000000000000,1e-7,0.000001,123.456,1,2.5e-10,\
             9007199254740992,1152921504606847000,-1e+300]"
        );
        let records = vec![
            json!({"id": "b", "last_modified": 2}),
            json!({"id": "a", "x": "old"}),
            json!({"id": "c", "deleted": true}),
            json!({"id": "a", "x": "new"}),
        ];
        assert_eq!(
            signed_message(&records, 1337),
            "Content-Signature:\u{0}{\"data\":[{\"id\":\"a\",\"x\":\"new\"},\
             {\"id\":\"b\",\"last_modified\":2}],\"last_modified\":\"1337\"}"
        );
    }

    /// A changeset of Remote Settings' small `main/message-groups`
    /// collection and the certificate chain its signature names, as served
    /// on 2026-11-25.
    const CHANGESET: &str = include_str!("fixtures/message-groups-changeset.json");
    const CHAIN: &str = include_str!("fixtures/remote-settings-chain.pem");
    /// 2026-11-25 00:00 UTC, while the chain is valid.
    const SIGNED_AT: i64 = 1_795_564_800;

    fn fixture() -> (Value, Vec<(String, Vec<u8>)>) {
        let changeset: Value = serde_json::from_str(CHANGESET).unwrap();
        let x5u = changeset["metadata"]["signature"]["x5u"]
            .as_str()
            .unwrap()
            .to_string();
        (changeset, vec![(x5u, CHAIN.as_bytes().to_vec())])
    }

    #[test]
    fn a_signed_catalog_verifies_and_a_changed_one_does_not() {
        let (changeset, chains) = fixture();
        let records = verified(&changeset, &chains, SIGNED_AT, SIGNER, &none()).unwrap();
        assert_eq!(records.len(), 15);

        let mut changed = changeset.clone();
        changed["changes"][0]["id"] = json!("something-else");
        assert!(verified(&changed, &chains, SIGNED_AT, SIGNER, &none())
            .unwrap_err()
            .contains("does not match its signature"));
        let mut changed = changeset.clone();
        changed["timestamp"] = json!(changeset["timestamp"].as_u64().unwrap() + 1);
        assert!(verified(&changed, &chains, SIGNED_AT, SIGNER, &none()).is_err());
        let mut changed = changeset.clone();
        let original = changed["metadata"]["signature"]["signature"]
            .as_str()
            .unwrap()
            .to_string();
        let first = if original.starts_with('A') { "B" } else { "A" };
        let signature = format!("{first}{}", &original[1..]);
        changed["metadata"]["signature"]["signature"] = json!(signature);
        changed["metadata"]["signatures"][0]["signature"] = json!(signature);
        assert!(verified(&changed, &chains, SIGNED_AT, SIGNER, &none()).is_err());

        // After the signing certificate expires (2027-01-03).
        assert!(
            verified(&changeset, &chains, 1_799_000_000, SIGNER, &none())
                .unwrap_err()
                .contains("not valid now")
        );
        assert!(verified(&changeset, &[], SIGNED_AT, SIGNER, &none())
            .unwrap_err()
            .contains("was not fetched"));
        let certificates = pem_certificates(CHAIN.as_bytes()).unwrap();
        assert_eq!(certificates.len(), 3);
        let pem = |ders: &[&Vec<u8>]| -> Vec<u8> {
            ders.iter()
                .map(|der| {
                    format!(
                        "-----BEGIN CERTIFICATE-----\r\n{}\r\n-----END CERTIFICATE-----\r\n",
                        base64::engine::general_purpose::STANDARD.encode(der)
                    )
                })
                .collect::<String>()
                .into_bytes()
        };
        let with = |chain: Vec<u8>| vec![(chains[0].0.clone(), chain)];
        // The same chain with Windows line ends still verifies.
        let all: Vec<&Vec<u8>> = certificates.iter().collect();
        assert!(verified(&changeset, &with(pem(&all)), SIGNED_AT, SIGNER, &none()).is_ok());
        let no_root = pem(&[&certificates[0], &certificates[1]]);
        assert!(
            verified(&changeset, &with(no_root), SIGNED_AT, SIGNER, &none())
                .unwrap_err()
                .contains("Mozilla's root")
        );
        let skipped = pem(&[&certificates[0], &certificates[2]]);
        assert!(
            verified(&changeset, &with(skipped), SIGNED_AT, SIGNER, &none())
                .unwrap_err()
                .contains("issuer")
        );
        let reversed = pem(&[&certificates[1], &certificates[0], &certificates[2]]);
        assert!(verified(&changeset, &with(reversed), SIGNED_AT, SIGNER, &none()).is_err());
        assert!(pem_certificates(b"not a chain").is_err());
    }

    #[test]
    fn newer_versions_languages_and_folder_names_come_from_the_catalog() {
        let mut records = Vec::new();
        records.extend(set(("en", "ja"), "3.0", "base", "", 40, false));
        records.extend(set(("en", "ja"), "3.1", "base", "", 40, false));
        records.extend(set(("ja", "en"), "3.0", "base", "", 40, true));
        records.extend(set(("en", "zh-Hans"), "3.0", "tiny", "", 40, true));
        records.extend(set(
            ("de", "en"),
            "3.0",
            "tiny",
            "env.appinfo.OS == 'Android'",
            4,
            true,
        ));
        let mut kept = choose(&records, "en", "ja").unwrap();
        assert_eq!(kept.license.name, "MPL-2.0");
        kept.version = "3.0".into();
        assert_eq!(newer(&records, "en", "ja", &kept).unwrap().version, "3.1");
        kept.version = "3.1".into();
        assert!(newer(&records, "en", "ja", &kept).is_none());
        kept.version = "3.2".into();
        assert!(newer(&records, "en", "ja", &kept).is_none());

        let languages = catalog_languages(&records);
        assert_eq!(languages.from, ["en", "ja"]);
        assert_eq!(languages.to, ["en", "ja", "zh-Hans"]);
        assert_eq!(languages.direct.len(), 3);

        assert_eq!(pair_of("en-zh-Hans"), Some(("en".into(), "zh-Hans".into())));
        assert_eq!(pair_of("zh-Hant-en"), Some(("zh-Hant".into(), "en".into())));
        assert_eq!(pair_of("ja-de"), Some(("ja".into(), "de".into())));
        assert_eq!(pair_of("en-ja.update"), None);
        assert_eq!(pair_of("chromium"), None);
        assert!(language_tag("zh-Hans"));
        assert!(!language_tag(".."));
        assert!(!language_tag("a/b"));
        assert!(!language_tag("en-"));

        let folder = Path::new("checked-once-test-folder");
        assert!(first_check(folder));
        assert!(!first_check(folder));
    }

    /// A stand-in HTTP server that answers each path in `routes` with its
    /// body and anything else with 404; gives back its address.
    pub(crate) async fn stand_in(routes: Vec<(String, Vec<u8>)>) -> String {
        let routes = routes
            .into_iter()
            .map(|(path, body)| (path, "200 OK".to_string(), body))
            .collect();
        stand_in_with(routes).await.0
    }

    /// The paths a stand-in server was asked for, in order.
    pub(crate) type Asked = Arc<std::sync::Mutex<Vec<String>>>;

    /// A stand-in HTTP server that answers each path in `routes` with its
    /// status line, which may go on with header lines, and its body, and
    /// anything else with 404; gives back its address and what it was
    /// asked for.
    pub(crate) async fn stand_in_with(routes: Vec<(String, String, Vec<u8>)>) -> (String, Asked) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let asked = Asked::default();
        let log = asked.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = format!("http://{}", listener.local_addr().unwrap());
        let routes = Arc::new(routes);
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let routes = routes.clone();
                let log = log.clone();
                tokio::spawn(async move {
                    let mut request = Vec::new();
                    let mut buffer = [0u8; 4096];
                    let head = loop {
                        let read = socket.read(&mut buffer).await.unwrap_or(0);
                        if read == 0 {
                            return;
                        }
                        request.extend_from_slice(&buffer[..read]);
                        let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") else {
                            continue;
                        };
                        let head = String::from_utf8_lossy(&request[..end]).to_string();
                        let length: usize = head
                            .lines()
                            .find_map(|line| {
                                line.to_ascii_lowercase()
                                    .strip_prefix("content-length: ")
                                    .and_then(|n| n.parse().ok())
                            })
                            .unwrap_or(0);
                        if request.len() >= end + 4 + length {
                            break head;
                        }
                    };
                    let path = head
                        .split_whitespace()
                        .nth(1)
                        .unwrap_or("")
                        .split('?')
                        .next()
                        .unwrap_or("")
                        .to_string();
                    log.lock().unwrap().push(path.clone());
                    let (status, body) = match routes.iter().find(|(route, _, _)| *route == path) {
                        Some((_, status, body)) => (status.as_str(), body.clone()),
                        None => ("404 Not Found", Vec::new()),
                    };
                    let head = format!(
                        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = socket.write_all(head.as_bytes()).await;
                    let _ = socket.write_all(&body).await;
                });
            }
        });
        (address, asked)
    }

    /// A catalog record whose attachment is `data`, zstd-compressed, at
    /// `main/<name>.zst`.
    fn served(name: &str, kind: &str, version: &str, data: &[u8]) -> (Value, (String, Vec<u8>)) {
        let compressed =
            ruzstd::encoding::compress_to_vec(data, ruzstd::encoding::CompressionLevel::Fastest);
        let record = json!({
            "name": name, "fileType": kind, "sourceLanguage": "en", "targetLanguage": "ja",
            "version": version, "architecture": "base", "filter_expression": "",
            "decompressedSize": data.len(), "decompressedHash": sha256(data),
            "attachment": {
                "location": format!("main/{name}.zst"),
                "size": compressed.len(),
                "hash": sha256(&compressed),
            },
        });
        (record, (format!("/main/{name}.zst"), compressed))
    }

    #[tokio::test]
    async fn a_newer_version_replaces_the_kept_one_and_the_kept_one_stays_otherwise() {
        let models = tempfile::tempdir().unwrap();
        let pair = ("en".to_string(), "ja".to_string());
        let kept_folder = models.path().join("en-ja");
        std::fs::create_dir_all(&kept_folder).unwrap();
        let old = b"old model".to_vec();
        std::fs::write(kept_folder.join("model.enja.bin"), &old).unwrap();
        let kept = Files {
            version: "3.0".into(),
            architecture: "base".into(),
            files: vec![File {
                name: "model.enja.bin".into(),
                kind: "model".into(),
                location: "main/old.zst".into(),
                size: 1,
                hash: "h".into(),
                decompressed_size: old.len() as u64,
                decompressed_hash: sha256(&old),
            }],
            license: license(),
        };
        std::fs::write(
            kept_folder.join(MANIFEST),
            serde_json::to_vec(&kept).unwrap(),
        )
        .unwrap();

        let model = b"new model ".repeat(30);
        let (model_record, model_route) = served("model.enja.bin", "model", "3.1", &model);
        let (vocab_record, vocab_route) = served("vocab.enja.spm", "vocab", "3.1", b"words");
        let base = stand_in(vec![model_route, vocab_route]).await;
        let catalog = |records: Vec<Value>| {
            Arc::new(Catalog {
                attachments: reqwest::Url::parse(&format!("{base}/")).unwrap(),
                records,
            })
        };
        let settings = format!("{base}/settings-update-test");
        CATALOGS.lock().unwrap().insert(
            settings.clone(),
            catalog(vec![model_record.clone(), vocab_record.clone()]),
        );
        let firefox = Firefox::new(models.path().into(), settings, reqwest::Client::new());
        assert_eq!(firefox.update(&pair).await, Ok(true));
        let now = firefox.manifest(&pair).await.unwrap();
        assert_eq!(now.version, "3.1");
        assert_eq!(now.license, license());
        assert_eq!(
            std::fs::read(kept_folder.join("model.enja.bin")).unwrap(),
            model
        );
        assert_eq!(
            std::fs::read(kept_folder.join("vocab.enja.spm")).unwrap(),
            b"words"
        );
        let mut names: Vec<String> = std::fs::read_dir(models.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, [LOCKS, "en-ja"]);
        assert_eq!(firefox.update(&pair).await, Ok(false));

        // A newer version whose download does not match the catalog leaves
        // the kept one.
        let mut broken = model_record.clone();
        broken["version"] = json!("3.2");
        broken["decompressedHash"] = json!(sha256(b"other"));
        let mut vocab = vocab_record.clone();
        vocab["version"] = json!("3.2");
        let settings = format!("{base}/settings-broken-test");
        CATALOGS
            .lock()
            .unwrap()
            .insert(settings.clone(), catalog(vec![broken, vocab]));
        let firefox = Firefox::new(models.path().into(), settings, reqwest::Client::new());
        assert!(firefox.update(&pair).await.is_err());
        assert_eq!(firefox.manifest(&pair).await.unwrap().version, "3.1");
        assert!(!models.path().join("en-ja.update").exists());

        // Without the network the kept version stays.
        let offline = Firefox::new(models.path().into(), NOWHERE.into(), reqwest::Client::new());
        assert!(offline.update(&pair).await.is_err());
        assert_eq!(offline.manifest(&pair).await.unwrap().version, "3.1");
        // A pair that is not kept is not fetched by an update.
        assert_eq!(offline.update(&("en".into(), "de".into())).await, Ok(false));
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

    fn none() -> Revoked {
        Revoked::default()
    }

    fn base64(bytes: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    #[test]
    fn a_chain_with_a_certificate_in_onecrl_is_refused() {
        let (changeset, chains) = fixture();
        let ders = pem_certificates(CHAIN.as_bytes()).unwrap();
        let signer = Certificate::from_der(&ders[0]).unwrap();
        let middle = Certificate::from_der(&ders[1]).unwrap();
        let by_issuer = json!({
            "issuerName": base64(&signer.tbs_certificate.issuer.to_der().unwrap()),
            "serialNumber": base64(signer.tbs_certificate.serial_number.as_bytes()),
        });
        let spki = middle
            .tbs_certificate
            .subject_public_key_info
            .to_der()
            .unwrap();
        let by_key = json!({
            "subject": base64(&middle.tbs_certificate.subject.to_der().unwrap()),
            "pubKeyHash": base64(&Sha256::digest(&spki)),
        });
        let other = json!({"issuerName": base64(b"someone else"), "serialNumber": "AQ=="});
        let broken = json!({"issuerName": "not base64!", "serialNumber": "AQ=="});
        for revoking in [by_issuer, by_key] {
            let revoked = Revoked::from_records(&[revoking, other.clone()]);
            assert!(verified(&changeset, &chains, SIGNED_AT, SIGNER, &revoked)
                .unwrap_err()
                .contains("revoked in OneCRL"));
        }
        let revoked = Revoked::from_records(&[other, broken]);
        assert_eq!(revoked.by_issuer.len(), 1);
        assert!(verified(&changeset, &chains, SIGNED_AT, SIGNER, &revoked).is_ok());
        // OneCRL is signed by a certificate of its own name.
        assert!(
            verified(&changeset, &chains, SIGNED_AT, ONECRL_SIGNER, &none())
                .unwrap_err()
                .contains(ONECRL_SIGNER)
        );
    }

    #[tokio::test]
    async fn a_settings_server_that_asks_to_wait_is_not_asked_again_until_then() {
        let models = tempfile::tempdir().unwrap();
        let info = br#"{"capabilities": {"attachments": {"base_url": "https://cdn.example/"}}}"#;
        let (base, asked) = stand_in_with(vec![
            (
                "/wait-a/".into(),
                "200 OK\r\nBackoff: 600".into(),
                info.to_vec(),
            ),
            (
                "/wait-b/".into(),
                "503 Service Unavailable\r\nRetry-After: 120".into(),
                Vec::new(),
            ),
            ("/wait-c/".into(), "200 OK".into(), info.to_vec()),
        ])
        .await;
        let client = reqwest::Client::new();
        let folder = Some(models.path());

        let settings = format!("{base}/wait-a");
        let error = catalog(&client, &settings, folder).await.err().unwrap();
        assert!(error.contains("asked not to be asked again"), "{error}");
        assert_eq!(*asked.lock().unwrap(), ["/wait-a/"]);
        // A process that starts later reads the wait from the models folder.
        SETTINGS_WAITS.lock().unwrap().remove(&settings);
        assert!(catalog(&client, &settings, folder).await.is_err());
        assert_eq!(asked.lock().unwrap().len(), 1);
        let firefox = Firefox::new(models.path().into(), settings, client.clone());
        let error = firefox.prepare("en", "ja").await.unwrap_err();
        assert!(error.message.contains("asked not to be asked again"));
        assert_eq!(asked.lock().unwrap().len(), 1);

        let settings = format!("{base}/wait-b");
        assert!(catalog(&client, &settings, folder)
            .await
            .err()
            .unwrap()
            .contains("503"));
        assert!(catalog(&client, &settings, folder)
            .await
            .err()
            .unwrap()
            .contains("asked not to be asked again"));
        assert_eq!(asked.lock().unwrap().len(), 2);

        // A wait that has passed stops nothing.
        let settings = format!("{base}/wait-c");
        let past = chrono::Utc::now().timestamp() - 1;
        set_wait(
            &SETTINGS_WAITS,
            Some(&models.path().join(BACKOFF_FILE)),
            &settings,
            Some(past),
        )
        .await;
        let error = catalog(&client, &settings, folder).await.err().unwrap();
        assert!(error.contains("404"), "{error}");
        let asked = asked.lock().unwrap();
        assert_eq!(
            asked[2..],
            [
                "/wait-c/".to_string(),
                format!("/wait-c/{ONECRL}/changeset"),
                format!("/wait-c/{COLLECTION}/changeset"),
            ]
        );
    }

    /// Writes a whole kept model of one file in `folder`.
    fn keep(folder: &Path, version: &str) -> Files {
        std::fs::create_dir_all(folder).unwrap();
        let data = b"kept model".to_vec();
        std::fs::write(folder.join("model.enja.bin"), &data).unwrap();
        let files = Files {
            version: version.into(),
            architecture: "base".into(),
            files: vec![File {
                name: "model.enja.bin".into(),
                kind: "model".into(),
                location: "main/kept.zst".into(),
                size: 1,
                hash: "h".into(),
                decompressed_size: data.len() as u64,
                decompressed_hash: sha256(&data),
            }],
            license: license(),
        };
        std::fs::write(folder.join(MANIFEST), serde_json::to_vec(&files).unwrap()).unwrap();
        files
    }

    const SHORT: Duration = Duration::from_millis(300);

    #[tokio::test(flavor = "multi_thread")]
    async fn a_folder_being_written_or_swapped_is_waited_for() {
        let models = tempfile::tempdir().unwrap();
        let pair = ("en".to_string(), "ja".to_string());
        keep(&models.path().join("en-ja"), "3.0");
        let one = Firefox::new(models.path().into(), NOWHERE.into(), reqwest::Client::new());
        let two = Firefox::new(models.path().into(), NOWHERE.into(), reqwest::Client::new());

        // Another holder writing the model: a second writer waits, a
        // reader does not.
        let held = writing(models.path(), "en-ja").await.unwrap();
        assert!(tokio::time::timeout(SHORT, two.update(&pair))
            .await
            .is_err());
        assert!(
            tokio::time::timeout(SHORT, remove(models.path(), "en", "ja"))
                .await
                .is_err()
        );
        assert!(one.manifest(&pair).await.is_some());
        drop(held);

        // Another holder swapping the folder: readers wait.
        let swapping = lock(models.path(), "en-ja", Lock::Read { exclusive: true })
            .await
            .unwrap();
        assert!(tokio::time::timeout(SHORT, one.manifest(&pair))
            .await
            .is_err());
        drop(swapping);
        assert!(one.manifest(&pair).await.is_some());

        // Readers share.
        let first = reading(&models.path().join("en-ja")).await.unwrap();
        assert!(two.manifest(&pair).await.is_some());
        assert!(tokio::time::timeout(
            SHORT,
            lock(models.path(), "en-ja", Lock::Read { exclusive: true })
        )
        .await
        .is_err());
        drop(first);

        assert_eq!(remove(models.path(), "en", "ja").await, Ok(true));
        assert!(one.manifest(&pair).await.is_none());
    }

    #[tokio::test]
    async fn what_a_stopped_update_left_is_put_right() {
        let models = tempfile::tempdir().unwrap();
        let folder = models.path().join("en-ja");
        let pair = ("en".to_string(), "ja".to_string());
        // Stopped between moving the kept folder aside and moving the new
        // one in.
        keep(&folder.with_extension("old"), "3.0");
        keep(&folder.with_extension("update"), "3.1");
        drop(writing(models.path(), "en-ja").await.unwrap());
        let firefox = Firefox::new(models.path().into(), NOWHERE.into(), reqwest::Client::new());
        assert_eq!(firefox.manifest(&pair).await.unwrap().version, "3.0");
        assert!(!folder.with_extension("old").exists());
        assert!(!folder.with_extension("update").exists());

        // Stopped while downloading, or before deleting the old copy.
        keep(&folder.with_extension("old"), "2.9");
        keep(&folder.with_extension("update"), "3.1");
        assert!(firefox.update(&pair).await.is_err());
        assert_eq!(firefox.manifest(&pair).await.unwrap().version, "3.0");
        assert!(!folder.with_extension("old").exists());
        assert!(!folder.with_extension("update").exists());

        // A deleted model does not come back from its old copy.
        keep(&folder.with_extension("old"), "2.9");
        assert_eq!(remove(models.path(), "en", "ja").await, Ok(true));
        assert!(!folder.with_extension("old").exists());
        assert!(firefox.manifest(&pair).await.is_none());
        assert_eq!(remove(models.path(), "en", "ja").await, Ok(false));
    }

    /// Set to a models folder, [`lock_holder_for_another_process`] holds
    /// the write lock of its `en-ja` folder until it is killed.
    const HOLDER: &str = "INOCODE_TEST_LOCK_HOLDER";

    #[tokio::test]
    #[ignore = "run by a_lock_held_by_another_process_is_waited_for_and_let_go_when_it_stops"]
    async fn lock_holder_for_another_process() {
        let Ok(models) = std::env::var(HOLDER) else {
            return;
        };
        let models = PathBuf::from(models);
        let _held = writing(&models, "en-ja").await.unwrap();
        std::fs::write(models.join("held"), b"").unwrap();
        tokio::time::sleep(Duration::from_secs(120)).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_lock_held_by_another_process_is_waited_for_and_let_go_when_it_stops() {
        let models = tempfile::tempdir().unwrap();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "translate::firefox::tests::lock_holder_for_another_process",
                "--ignored",
                "--test-threads=1",
            ])
            .env(HOLDER, models.path())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let held = models.path().join("held");
        let started = std::time::Instant::now();
        while !held.exists() {
            assert!(
                started.elapsed() < Duration::from_secs(60),
                "the other process did not take the lock"
            );
            assert!(
                child.try_wait().unwrap().is_none(),
                "the other process ended"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(
            tokio::time::timeout(SHORT, lock(models.path(), "en-ja", Lock::Write))
                .await
                .is_err()
        );
        // Killed, as a crash would end it: its lock goes with it.
        child.kill().unwrap();
        child.wait().unwrap();
        tokio::time::timeout(
            Duration::from_secs(10),
            lock(models.path(), "en-ja", Lock::Write),
        )
        .await
        .unwrap()
        .unwrap();
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
    #[ignore = "fetches Mozilla's model catalog and its certificate chain; run by hand"]
    async fn the_real_catalog_is_signed_and_lists_newer_versions() {
        let client = reqwest::Client::new();
        let catalog = catalog(&client, SETTINGS, None).await.unwrap();
        println!("{} records, signature verified", catalog.records.len());
        assert!(catalog.records.len() > 100);
        let newest = choose(&catalog.records, "en", "ja").unwrap();
        println!(
            "en-ja: version {} ({})",
            newest.version, newest.architecture
        );
        let mut kept = newest.clone();
        assert!(newer(&catalog.records, "en", "ja", &kept).is_none());
        kept.version = "2.9".into();
        assert_eq!(
            newer(&catalog.records, "en", "ja", &kept).unwrap().version,
            newest.version
        );
        let languages = languages(SETTINGS, &client).await.unwrap();
        println!(
            "{} pairs; from {:?}; to {:?}",
            languages.direct.len(),
            languages.from,
            languages.to
        );
        assert!(languages.direct.contains(&("en".into(), "ja".into())));
    }

    #[tokio::test]
    #[ignore = "fetches Mozilla's OneCRL, model catalog and their certificate chains; run by hand"]
    async fn the_real_onecrl_is_signed_and_revokes_nothing_in_the_catalog_chain() {
        let client = reqwest::Client::new();
        let server = Settings {
            client: &client,
            server: SETTINGS,
            waits: None,
        };
        let records = server
            .collection(ONECRL, ONECRL_SIGNER, &none())
            .await
            .unwrap();
        let revoked = Revoked::from_records(&records);
        println!(
            "OneCRL: {} records; {} by issuer and serial, {} by subject and key",
            records.len(),
            revoked.by_issuer.len(),
            revoked.by_key.len()
        );
        assert!(revoked.by_issuer.len() > 100);
        let catalog = server
            .collection(COLLECTION, SIGNER, &revoked)
            .await
            .unwrap();
        println!(
            "model catalog: {} records, signature verified",
            catalog.len()
        );
        let models = tempfile::tempdir().unwrap();
        let kept = models.path().join(ONECRL_FILE);
        assert_eq!(server.revoked(Some(&kept)).await, revoked);
        assert_eq!(server.revoked(Some(&kept)).await, revoked);
        println!(
            "kept OneCRL: {} bytes",
            std::fs::metadata(&kept).unwrap().len()
        );
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
        let route = online.route("en", "ja", false).await.unwrap();
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
        assert_eq!(offline.route("en", "ja", false).await.unwrap().len(), 1);
        assert!(offline.route("ja", "en", false).await.is_err());
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
            online
                .fetch(&catalog, &online.pair_folder(&pair), &files)
                .await
                .unwrap();
        }
        println!("memory before loading (working set, peak): {:?}", memory());
        let firefox = Firefox::new(folder, NOWHERE.into(), reqwest::Client::new());
        let started = std::time::Instant::now();
        let route = firefox.route("en", "ja", false).await.unwrap();
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
