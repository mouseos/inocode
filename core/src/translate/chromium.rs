//! Translation with the language packs Chrome downloads for its built-in
//! Translator API, run here offline: a pack is fetched once through
//! Chrome's component updater, its signature and contents are checked,
//! its models are kept on disk, then loaded into memory and run on the CPU
//! with the TFLite interpreter in `tflite.rs`.
//! Wire facts and choices: docs/evidence/translation.md.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use super::firefox::{
    asked_to_wait, body, first_check, language_tag, reading, remove_folder, replace_folder,
    safe_name, sentences, set_wait, sha256, write_whole, writing, Waits, DISK,
};
use super::sentencepiece::{Fields, Value as Field, Vocabulary};
use super::tflite::{Graph, Tensor};
use super::{InstalledModel, Languages, License, Service};
use crate::ErrorInfo;

/// Chrome's component update server
/// (components/component_updater/component_updater_url_constants.cc).
pub(super) const UPDATE: &str = "https://update.googleapis.com/service/update2/json";
/// The Chrome version the update request names.
const CHROME_VERSION: &str = "143.0.7499.170";
/// Chrome translates between two other languages through English
/// (components/on_device_translation/public/language_pack.cc).
const PIVOT: &str = "en";
/// Each pack's language other than English and the SHA-256 of the pack's
/// public key (components/on_device_translation/public/language_pack.h).
const PACKS: [(&str, &str); 38] = [
    (
        "es",
        "63bd10984eaac3be3be087ba035d7d6e447eaa02bb0ccc51b5745db83c04e1bb",
    ),
    (
        "ja",
        "7d2233741ca8625877dc88872d0e6e4badbf372906ffc77be428832fee7dd372",
    ),
    (
        "ar",
        "a1b367f55a8afc1ba3138cabcc040dd857c8bdc61dfea3fa6c5b905779037493",
    ),
    (
        "bn",
        "626f69319c78f39230cd8c6ce8ffd760b935cf69b9c6ae3977a859ab34e9adaf",
    ),
    (
        "de",
        "d75573ae4f4574abed8d31b92ad5b589165d030f182c83ea21bd81719180db13",
    ),
    (
        "fr",
        "e5a255f1200ab30fac842fc40e3bbb091188edd047068053731d96e499141445",
    ),
    (
        "hi",
        "a981d8bc0d1ddf28a47936369bf54e1acda34e8b0aacfe2790b54ac72fc9c2ab",
    ),
    (
        "it",
        "d7c340593455611e899e393704b64975ce86ebd3ffd8d8f99e3755e80ece423e",
    ),
    (
        "ko",
        "e20c8f9e6d33063c6ddc1c9f8e575bc62790a048ea8e7993bdf49b940235cc7a",
    ),
    (
        "nl",
        "7db7146047dad9aa5debbf6d4851542612369e124531f038f07041b090c3896b",
    ),
    (
        "pl",
        "100330ebe48f45f3ac73178c9b2743812616dd2cf37676b0b8f8a16b61b97191",
    ),
    (
        "pt",
        "cd314d326e09e916ca044bc99b7677c2b04aa9a907147930bdd1e3a93873d9ee",
    ),
    (
        "ru",
        "31e6dd3f9d86d13dbea8fb9d9312d5c29c6d4981e74e5642ba2c446258a233c3",
    ),
    (
        "th",
        "73ab922991ea31874e06b565647c9828343853586149a7faa163a58b3c974ee2",
    ),
    (
        "tr",
        "c5751d503e87afad2ba11e41048477aeb15682c261a9a55d603657ffb72bd86e",
    ),
    (
        "vi",
        "b3ff8ea74440cec4a04ccba8c792f2054234f7351815371d66093618e4d43933",
    ),
    (
        "zh-Hans",
        "1e443e1c993356d25dcd4724c915f51aa4fa2bc184b763b51f9c1ee39d7e55c5",
    ),
    (
        "zh-Hant",
        "e850df7d99ef902ef04da56cc8b9404bc31864259520dd4acb707e63cfaa5f3b",
    ),
    (
        "bg",
        "6cdf575f998e24b1999fc899e5d0d9ecbac2529e3302d03a69ea47d1f5c7c3e5",
    ),
    (
        "cs",
        "b1c4ea3fb5cfba7930edac2686b4e4ba7ae64910a4f35c036e839afdeabb8265",
    ),
    (
        "da",
        "03edf84484aa9e13d22fe6fb6d220ba09b6a3e8f2e898807cd7d6b7d3889e77d",
    ),
    (
        "el",
        "29a099ad643f2388900ca97b6c326a7546031ad5b7eb899928f646395f8861ed",
    ),
    (
        "fi",
        "92b7bbb7c7ad825b94fdba43515901cfff8192c84afe17044835c793770a8185",
    ),
    (
        "hr",
        "6a1a7889452b32f1e623269e84705353c051a2a9b24fe5473c5503ed632117db",
    ),
    (
        "hu",
        "f4eef96f3c9624c36ebf68dc6ff10d581af478d40e9e5535757db4559e3ec924",
    ),
    (
        "id",
        "a107f527bd5a871fd34e44359741c658d72925335225723a47858c1d88401d1d",
    ),
    (
        "iw",
        "3128af49e06e38c821f706d9ad5121038cbd0c8f7d51b53b40338e095d786f3c",
    ),
    (
        "lt",
        "e360dd89e43e7a2a63c6dc794d67767f98bcdd6cf430338eef82059712cca5fa",
    ),
    (
        "no",
        "c7ceeb48169e4e88f9fe151f67921d72349c02fafd246c6fa69990f1fa5a99a0",
    ),
    (
        "ro",
        "0a2feb3cacf364bf502f9cffc717810cf847f57aa52bb27a47243715cc7f03a7",
    ),
    (
        "sk",
        "f4e9211145ae0a0cf61f7c6a72e97b6ff59a87da850fea4625a565fadfd6f59d",
    ),
    (
        "sl",
        "e15fa1fb33bb42e50281680cf0c7cdeffa091fd961f34f55506a716be0be65ed",
    ),
    (
        "sv",
        "f7ffca43fb2ecd44dfb7c2a68a66683eb13b10d27ac4c7f9ddc487edf9220a6b",
    ),
    (
        "uk",
        "e08414122bceb0bc6a3cc20760826314a3396987828610d35554509401ecc5e3",
    ),
    (
        "kn",
        "5c9d4231caf49721f8bc7754884365f3a38e847260c612549b49c8efa4ae1679",
    ),
    (
        "ta",
        "abf056fa09fe6f4030e705f1506e3e6cc1dd3cc0414914e4c2d5f6bdcd9cc45d",
    ),
    (
        "te",
        "c71c20312a3fe866b82a7e7735c5f70a0f4f47691a83f7ca730d8b86d812f039",
    ),
    (
        "mr",
        "5e81f0cf35bc9b5807f94c2500248845ff413ad6d3b6be409d2ee3de5e420002",
    ),
];
const UPDATE_WAIT: Duration = Duration::from_secs(30);
const DOWNLOAD_WAIT: Duration = Duration::from_secs(600);
const LARGEST_ANSWER: u64 = 1 << 20;
const LARGEST_PACK: u64 = 300 << 20;
const LARGEST_FILE: u64 = 300 << 20;
const MANIFEST: &str = "pack.json";
/// The file in the packs folder that keeps when each update server asked
/// not to be asked again before.
const BACKOFF_FILE: &str = "update-backoff.json";
/// The longest wait an `X-Retry-After` header is followed for
/// (`kMaxRetryAfterSec` in components/update_client/request_sender.cc).
const LONGEST_RETRY_AFTER: i64 = 24 * 60 * 60;
/// The CRX3 signature covers this, the signed header's length and the
/// signed header, then the archive (components/crx_file/crx_verifier.cc).
const SIGNATURE_CONTEXT: &[u8] = b"CRX3 SignedData\x00";
/// The longest run of source pieces translated at once.
const LONGEST_SOURCE: usize = 128;
/// The pieces a hypothesis never continues with: `<unk>` and `<s>`.
const NEVER: [u32; 2] = [0, 1];

/// A file of a pack on disk, relative to the pack's folder.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Kept {
    path: String,
    size: u64,
    sha256: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Manifest {
    version: String,
    files: Vec<Kept>,
    #[serde(default = "license")]
    license: License,
}

/// No license for the packs was found (see the evidence document).
fn license() -> License {
    License {
        name: "unknown".into(),
        source: "https://www.google.com/chrome/terms/".into(),
        terms: Some(
            "No license for Chrome's language packs was found. They come from Google's \
             component update server; the Google Terms of Service \
             (https://policies.google.com/terms) and the Chrome Additional Terms of Service \
             may apply and may not allow this use."
                .into(),
        ),
    }
}

/// What Chrome's packs translate: English and each pack's language, both
/// ways.
pub(super) fn languages() -> Languages {
    let mut all: Vec<String> = PACKS.iter().map(|(name, _)| name.to_string()).collect();
    all.push(PIVOT.into());
    all.sort();
    let direct = PACKS
        .iter()
        .flat_map(|(name, _)| {
            [
                (PIVOT.to_string(), name.to_string()),
                (name.to_string(), PIVOT.to_string()),
            ]
        })
        .collect();
    Languages {
        from: all.clone(),
        to: all,
        direct,
    }
}

/// The list of files of the pack in `folder`, when all of them are on
/// disk.
async fn read_manifest(folder: &Path) -> Option<Manifest> {
    let _locked = reading(folder).await;
    let _reading = DISK.read().await;
    let text = tokio::fs::read_to_string(folder.join(MANIFEST))
        .await
        .ok()?;
    let manifest: Manifest = serde_json::from_str(&text).ok()?;
    for file in &manifest.files {
        let metadata = tokio::fs::metadata(folder.join(&file.path)).await.ok()?;
        if metadata.len() != file.size {
            return None;
        }
    }
    Some(manifest)
}

/// The whole packs kept in `folder`.
pub(super) async fn installed(folder: &Path) -> Vec<InstalledModel> {
    let mut out = Vec::new();
    let Ok(mut entries) = tokio::fs::read_dir(folder).await else {
        return out;
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name();
        let Some(language) = name
            .to_str()
            .and_then(|name| name.strip_prefix("en-"))
            .filter(|language| language_tag(language))
        else {
            continue;
        };
        if !entry.file_type().await.is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        let path = entry.path();
        if let Some(manifest) = read_manifest(&path).await {
            out.push(InstalledModel {
                service: Service::Chromium,
                from: PIVOT.into(),
                to: language.to_string(),
                bytes: manifest.files.iter().map(|file| file.size).sum(),
                version: manifest.version,
                license: manifest.license,
                path,
            });
        }
    }
    out.sort_by(|a, b| a.to.cmp(&b.to));
    out
}

/// Deletes the kept pack that holds `from` → `to`.
pub(super) async fn remove(folder: &Path, from: &str, to: &str) -> Result<bool, ErrorInfo> {
    let language = match (from, to) {
        (PIVOT, other) | (other, PIVOT) if other != PIVOT && language_tag(other) => other,
        _ => {
            return Err(ErrorInfo::new(
                "invalid_params",
                format!("{from:?} → {to:?} is not a pair of English and a pack's language"),
            ))
        }
    };
    remove_folder(folder, &format!("{PIVOT}-{language}")).await
}

/// The `pack` language as the pack list writes it: Chinese by script,
/// Hebrew as `iw`, Norwegian as `no`.
fn pack_language(tag: &str) -> String {
    match tag {
        "zh" | "zh-CN" | "zh-SG" | "zh-Hans" => "zh-Hans".into(),
        "zh-TW" | "zh-HK" | "zh-MO" | "zh-Hant" => "zh-Hant".into(),
        "he" => "iw".into(),
        "nb" => "no".into(),
        other => other
            .split(['-', '_'])
            .next()
            .unwrap_or(other)
            .to_ascii_lowercase(),
    }
}

/// The SHA-256 of the public key of the pack for `language`.
fn key_hash(language: &str) -> Option<[u8; 32]> {
    let (_, hex) = PACKS.iter().find(|(name, _)| *name == language)?;
    let mut out = [0u8; 32];
    for (slot, pair) in out.iter_mut().zip(hex.as_bytes().chunks(2)) {
        *slot = u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()?;
    }
    Some(out)
}

/// A component's id: the first 16 bytes of its key's SHA-256 written with
/// the letters a–p for 0–f (crx_file::id_util::GenerateIdFromHash).
fn component_id(key_hash: &[u8; 32]) -> String {
    key_hash[..16]
        .iter()
        .flat_map(|byte| [byte >> 4, byte & 15])
        .map(|digit| (b'a' + digit) as char)
        .collect()
}

/// The update request for `app` at `version`, `0.0.0.0` when none is kept
/// (docs/updater/protocol_4.md), naming the system inocode runs on.
fn update_request(app: &str, version: &str) -> Value {
    let (os, platform) = match std::env::consts::OS {
        "windows" => ("win", "Windows"),
        "macos" => ("mac", "Mac OS X"),
        other => (other, "Linux"),
    };
    let (arch, os_arch) = match std::env::consts::ARCH {
        "x86_64" => ("x64", "x86_64"),
        "aarch64" => ("arm64", "arm64"),
        "x86" => ("x86", "x86"),
        other => (other, other),
    };
    json!({"request": {
        "protocol": "4.0",
        "ismachine": false,
        "dedup": "cr",
        "acceptformat": "crx3,download,puff,run,xz,zucc",
        "@updater": "chrome",
        "prodversion": CHROME_VERSION,
        "updaterversion": CHROME_VERSION,
        "@os": os,
        "arch": arch,
        "prodchannel": "stable",
        "updaterchannel": "stable",
        "os": {"platform": platform, "arch": os_arch},
        "apps": [{
            "appid": app,
            "version": version,
            "enabled": true,
            "installsource": "ondemand",
            "updatecheck": {},
        }],
    }})
}

/// Where and what the pack is, as the update server answers.
#[derive(Debug, Clone, PartialEq)]
struct Package {
    version: String,
    size: u64,
    sha256: String,
    urls: Vec<String>,
}

/// The whole-file download of `app` in an update answer: the pipeline
/// that is only a download and a CRX3 install, from https addresses;
/// `None` when the server has nothing newer than the version asked with.
fn package(answer: &str, app: &str) -> Result<Option<Package>, String> {
    let text = answer.trim_start();
    let text = text.strip_prefix(")]}'").unwrap_or(text);
    let value: Value =
        serde_json::from_str(text).map_err(|error| format!("the update answer: {error}"))?;
    let entry = value["response"]["apps"]
        .as_array()
        .and_then(|apps| apps.iter().find(|entry| entry["appid"] == app))
        .ok_or("the update answer does not name the pack")?;
    if entry["status"] != "ok" {
        return Err(format!(
            "the update server answered {} for the pack",
            entry["status"]
        ));
    }
    let check = &entry["updatecheck"];
    if check["status"] == "noupdate" {
        return Ok(None);
    }
    if check["status"] != "ok" {
        return Err(format!(
            "the update server answered {} for the pack",
            check["status"]
        ));
    }
    let version = check["nextversion"]
        .as_str()
        .ok_or("the update answer has no version")?
        .to_string();
    for pipeline in check["pipelines"].as_array().into_iter().flatten() {
        let Some([download, install]) = pipeline["operations"].as_array().map(Vec::as_slice) else {
            continue;
        };
        if download["type"] != "download" || install["type"] != "crx3" {
            continue;
        }
        let (Some(size), Some(sha256)) = (
            download["size"].as_u64(),
            download["out"]["sha256"].as_str(),
        ) else {
            continue;
        };
        if sha256.len() != 64 || !sha256.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            continue;
        }
        let urls: Vec<String> = download["urls"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|url| url["url"].as_str())
            .filter(|url| url.starts_with("https://"))
            .map(str::to_string)
            .collect();
        if !urls.is_empty() {
            return Ok(Some(Package {
                version,
                size,
                sha256: sha256.to_ascii_lowercase(),
                urls,
            }));
        }
    }
    Err("the update answer has no whole download of the pack".into())
}

/// The parts of a CRX3 file (components/crx_file/crx3.proto).
#[derive(Debug)]
struct Crx<'a> {
    /// Each RSA proof: the public key (DER) and its signature.
    rsa: Vec<(&'a [u8], &'a [u8])>,
    signed_data: &'a [u8],
    archive: &'a [u8],
}

fn crx(data: &[u8]) -> Result<Crx<'_>, String> {
    if data.get(..4) != Some(b"Cr24") {
        return Err("the pack is not a CRX file".into());
    }
    let word = |at: usize| -> Result<usize, String> {
        data.get(at..at + 4)
            .map(|bytes| u32::from_le_bytes(bytes.try_into().unwrap()) as usize)
            .ok_or_else(|| "the CRX header is cut short".to_string())
    };
    if word(4)? != 3 {
        return Err("the pack is not a CRX3 file".into());
    }
    let length = word(8)?;
    let header = data
        .get(12..12 + length)
        .ok_or("the CRX header is cut short")?;
    let mut out = Crx {
        rsa: Vec::new(),
        signed_data: &[],
        archive: &data[12 + length..],
    };
    for field in Fields::new(header) {
        match field? {
            (2, Field::Bytes(proof)) => {
                let (mut key, mut signature) = (None, None);
                for field in Fields::new(proof) {
                    match field? {
                        (1, Field::Bytes(bytes)) => key = Some(bytes),
                        (2, Field::Bytes(bytes)) => signature = Some(bytes),
                        _ => {}
                    }
                }
                if let (Some(key), Some(signature)) = (key, signature) {
                    out.rsa.push((key, signature));
                }
            }
            (10000, Field::Bytes(bytes)) => out.signed_data = bytes,
            _ => {}
        }
    }
    Ok(out)
}

/// Checks that the CRX belongs to the pack whose key hashes to `key_hash`
/// and that this key signed it (RSA PKCS#1 v1.5 with SHA-256, as Chrome
/// checks component packs).
fn verify(crx: &Crx, key_hash: &[u8; 32]) -> Result<(), String> {
    let mut id = None;
    for field in Fields::new(crx.signed_data) {
        if let (1, Field::Bytes(bytes)) = field? {
            id = Some(bytes);
        }
    }
    if id != Some(&key_hash[..16]) {
        return Err("the CRX is for another component".into());
    }
    let (key, signature) = crx
        .rsa
        .iter()
        .find(|(key, _)| Sha256::digest(key).as_slice() == key_hash)
        .ok_or("the CRX is not signed with the pack's key")?;
    let mut hasher = Sha256::new();
    hasher.update(SIGNATURE_CONTEXT);
    hasher.update((crx.signed_data.len() as u32).to_le_bytes());
    hasher.update(crx.signed_data);
    hasher.update(crx.archive);
    let digest = hasher.finalize();
    use rsa::pkcs8::DecodePublicKey;
    let key = rsa::RsaPublicKey::from_public_key_der(key)
        .map_err(|error| format!("the pack's key: {error}"))?;
    key.verify(rsa::Pkcs1v15Sign::new::<Sha256>(), &digest, signature)
        .map_err(|_| "the CRX signature does not verify".to_string())
}

/// Whether the zip entry `name` is a file inocode keeps: a model, a
/// SentencePiece model or a settings file directly in a `<from>_<to>_nmt`
/// folder. Anything else, including any other path, is left in the zip.
fn wanted(name: &str) -> bool {
    let mut parts = name.split('/');
    let (Some(folder), Some(file), None) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    safe_name(folder)
        && folder.ends_with("_nmt")
        && safe_name(file)
        && [".tflite", ".model", ".pb"]
            .iter()
            .any(|end| file.ends_with(end))
}

/// The wanted files of a zip archive, checked against their CRC-32 and
/// sizes. Only stored and deflated entries without encryption are read.
fn unzip(archive: &[u8]) -> Result<Vec<(String, Vec<u8>)>, String> {
    let u16_at = |at: usize| -> Result<usize, String> {
        archive
            .get(at..at + 2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]) as usize)
            .ok_or_else(|| "the zip is cut short".to_string())
    };
    let u32_at = |at: usize| -> Result<usize, String> {
        archive
            .get(at..at + 4)
            .map(|b| u32::from_le_bytes(b.try_into().unwrap()) as usize)
            .ok_or_else(|| "the zip is cut short".to_string())
    };
    let search_from = archive.len().saturating_sub(22 + 0xFFFF);
    let end = (search_from..archive.len().saturating_sub(21))
        .rev()
        .find(|&at| archive[at..at + 4] == [0x50, 0x4b, 0x05, 0x06])
        .ok_or("the zip has no directory")?;
    let count = u16_at(end + 10)?;
    let mut at = u32_at(end + 16)?;
    if count == 0xFFFF || at == 0xFFFF_FFFF {
        return Err("zip64 archives are not supported".into());
    }
    let mut out = Vec::new();
    for _ in 0..count {
        if u32_at(at)? != 0x0201_4b50 {
            return Err("the zip directory is broken".into());
        }
        let flags = u16_at(at + 8)?;
        let method = u16_at(at + 10)?;
        let crc = u32_at(at + 16)? as u32;
        let compressed = u32_at(at + 20)?;
        let size = u32_at(at + 24)?;
        let name_length = u16_at(at + 28)?;
        let extra_length = u16_at(at + 30)?;
        let comment_length = u16_at(at + 32)?;
        let local = u32_at(at + 42)?;
        let name = archive
            .get(at + 46..at + 46 + name_length)
            .ok_or("the zip directory is cut short")?;
        let name = String::from_utf8_lossy(name).into_owned();
        at += 46 + name_length + extra_length + comment_length;
        if !wanted(&name) {
            continue;
        }
        if flags & 1 != 0 {
            return Err(format!("{name} is encrypted"));
        }
        if size as u64 > LARGEST_FILE {
            return Err(format!("{name} is too large"));
        }
        if u32_at(local)? != 0x0403_4b50 {
            return Err(format!("{name} has no local header"));
        }
        let start = local + 30 + u16_at(local + 26)? + u16_at(local + 28)?;
        let stored = archive
            .get(start..start + compressed)
            .ok_or_else(|| format!("{name} runs past the end of the zip"))?;
        let data = match method {
            0 => stored.to_vec(),
            8 => {
                let mut data = Vec::with_capacity(size);
                flate2::read::DeflateDecoder::new(stored)
                    .take(size as u64 + 1)
                    .read_to_end(&mut data)
                    .map_err(|error| format!("{name}: {error}"))?;
                data
            }
            other => return Err(format!("{name} uses compression {other}")),
        };
        let mut check = flate2::Crc::new();
        check.update(&data);
        if data.len() != size || check.sum() != crc {
            return Err(format!("{name} does not match its checksum"));
        }
        out.push((name, data));
    }
    Ok(out)
}

/// What `translation_model.pb` says (a protobuf whose schema is not
/// published; the field meanings are read from the en-ja pack, see the
/// evidence document).
#[derive(Debug, Clone, PartialEq)]
struct Settings {
    from: String,
    to: String,
    /// Model name to file name.
    files: Vec<(String, String)>,
    source_vocabulary: String,
    target_vocabulary: String,
    start: u32,
    end: u32,
    end_margin: f32,
    beam: usize,
    longest: usize,
}

fn text(bytes: &[u8]) -> Result<String, String> {
    String::from_utf8(bytes.to_vec()).map_err(|error| error.to_string())
}

fn settings(data: &[u8]) -> Result<Settings, String> {
    let mut out = Settings {
        from: String::new(),
        to: String::new(),
        files: Vec::new(),
        source_vocabulary: String::new(),
        target_vocabulary: String::new(),
        start: 1,
        end: 2,
        end_margin: 5.0,
        beam: 4,
        longest: 72,
    };
    for field in Fields::new(data) {
        match field? {
            (1, Field::Bytes(bytes)) => out.from = text(bytes)?,
            (2, Field::Bytes(bytes)) => out.to = text(bytes)?,
            (6, Field::Bytes(model)) => {
                for field in Fields::new(model) {
                    match field? {
                        (3, Field::Bytes(search)) => {
                            for field in Fields::new(search) {
                                if let (2, Field::Bytes(beam)) = field? {
                                    for field in Fields::new(beam) {
                                        match field? {
                                            (1, Field::Varint(value)) => out.start = value as u32,
                                            (2, Field::Varint(value)) => out.end = value as u32,
                                            (5, Field::Fixed32(bits)) => {
                                                out.end_margin = f32::from_bits(bits)
                                            }
                                            _ => {}
                                        }
                                    }
                                }
                            }
                        }
                        (4, Field::Bytes(limits)) => {
                            for field in Fields::new(limits) {
                                match field? {
                                    (2, Field::Varint(value)) => out.beam = value as usize,
                                    (3, Field::Varint(value)) => out.longest = value as usize,
                                    _ => {}
                                }
                            }
                        }
                        (number @ 5..=7, Field::Bytes(entry)) => {
                            let (mut first, mut second) = (String::new(), String::new());
                            for field in Fields::new(entry) {
                                match field? {
                                    (1, Field::Bytes(bytes)) => first = text(bytes)?,
                                    (2, Field::Bytes(bytes)) => second = text(bytes)?,
                                    _ => {}
                                }
                            }
                            match number {
                                5 => out.files.push((first, second)),
                                6 => out.source_vocabulary = first,
                                _ => out.target_vocabulary = first,
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    if !(1..=16).contains(&out.beam) || out.longest == 0 {
        return Err("the search settings are out of range".into());
    }
    Ok(out)
}

/// The preprocessing steps `processor_spec.pb` names.
fn steps(data: &[u8]) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    for field in Fields::new(data) {
        if let (3, Field::Bytes(spec)) = field? {
            for field in Fields::new(spec) {
                if let (3, Field::Bytes(step)) = field? {
                    out.push(text(step)?);
                }
            }
        }
    }
    Ok(out)
}

/// The value fed to an input of the decoder step.
#[derive(Debug, Clone, PartialEq)]
enum Feed {
    StepIds,
    Encoded,
    Padding,
    /// Beam state carried from one step to the next, by name.
    State(String),
}

/// The part of a tensor name after `beam_state__`, without the
/// `__00:0` ending.
fn state_name(name: &str) -> Option<&str> {
    let (_, rest) = name.split_once("beam_state__")?;
    Some(rest.split("__").next().unwrap_or(rest))
}

/// One direction of a pack, loaded into memory.
struct Direction {
    encoder: Graph,
    init: Graph,
    step: Graph,
    source: Vocabulary,
    target: Vocabulary,
    settings: Settings,
    lowercase: bool,
    /// The longest source in words that gets a period added.
    add_period: Option<usize>,
    feeds: Vec<Feed>,
    log_probs: usize,
    /// For each state, its position among the step's outputs and the
    /// position among the start graph's outputs that starts it.
    states: Vec<(String, usize, Option<usize>)>,
}

#[derive(Debug, Clone)]
struct Hypothesis {
    pieces: Vec<u32>,
    score: f32,
}

impl Direction {
    fn load(folder: &Path) -> Result<Self, String> {
        let read = |name: &str| -> Result<Vec<u8>, String> {
            if !safe_name(name) {
                return Err(format!("the file name {name:?} is not safe"));
            }
            std::fs::read(folder.join(name)).map_err(|error| format!("{name}: {error}"))
        };
        let settings = settings(&read("translation_model.pb")?)?;
        let graph = |prefix: &str| -> Result<Graph, String> {
            let (_, file) = settings
                .files
                .iter()
                .find(|(name, _)| name.starts_with(prefix))
                .ok_or_else(|| format!("the pack names no {prefix} model"))?;
            Graph::parse(&read(file)?).map_err(|error| format!("{file}: {error}"))
        };
        let encoder = graph("encoder")?;
        let init = graph("decoder_init")?;
        let step = graph("decoder_step")?;
        let source = Vocabulary::parse(&read(&settings.source_vocabulary)?)?;
        let target = Vocabulary::parse(&read(&settings.target_vocabulary)?)?;
        let steps = match read("processor_spec.pb") {
            Ok(data) => self::steps(&data)?,
            Err(_) => Vec::new(),
        };
        let lowercase = steps.iter().any(|step| step == "lc");
        let add_period = steps
            .iter()
            .find_map(|step| step.strip_prefix("add_period_")?.parse().ok());
        let feeds = step
            .input_names()
            .iter()
            .map(|name| {
                if name.contains("step_ids") {
                    Ok(Feed::StepIds)
                } else if name.contains("source_vecs") || name.contains("source_contexts") {
                    Ok(Feed::Encoded)
                } else if name.contains("source_padding") {
                    Ok(Feed::Padding)
                } else if let Some(state) = state_name(name) {
                    Ok(Feed::State(state.to_string()))
                } else {
                    Err(format!("the decoder step has an unknown input {name:?}"))
                }
            })
            .collect::<Result<Vec<Feed>, String>>()?;
        let log_probs = step.output("log_probs")?;
        let init_names = init.output_names();
        let step_names = step.output_names();
        let states = feeds
            .iter()
            .filter_map(|feed| match feed {
                Feed::State(name) => Some(name.clone()),
                _ => None,
            })
            .map(|name| {
                let out = step_names
                    .iter()
                    .position(|output| state_name(output) == Some(&name))
                    .ok_or_else(|| format!("the decoder step does not give back {name}"))?;
                let start = init_names
                    .iter()
                    .position(|output| state_name(output) == Some(&name));
                Ok((name, out, start))
            })
            .collect::<Result<Vec<_>, String>>()?;
        if settings.end != target.end() {
            return Err("the end of sentence does not match the target vocabulary".into());
        }
        Ok(Self {
            encoder,
            init,
            step,
            source,
            target,
            settings,
            lowercase,
            add_period,
            feeds,
            log_probs,
            states,
        })
    }

    /// `text` translated sentence by sentence, the space between them
    /// kept as it is.
    fn translate(&self, text: &str) -> Result<String, String> {
        sentences(text)
            .into_iter()
            .map(|(piece, sentence)| {
                if sentence {
                    self.sentence(piece)
                } else {
                    Ok(piece.to_string())
                }
            })
            .collect()
    }

    fn sentence(&self, sentence: &str) -> Result<String, String> {
        let mut source = if self.lowercase {
            sentence.to_lowercase()
        } else {
            sentence.to_string()
        };
        let period = self.add_period.is_some_and(|longest| {
            sentence.split_whitespace().count() <= longest && !ends_in_punctuation(sentence)
        });
        if period {
            source.push('.');
        }
        let pieces = self.source.encode(&source);
        let mut out = Vec::new();
        for part in pieces.chunks(LONGEST_SOURCE) {
            out.extend(self.search(part)?);
        }
        let mut text = self.target.decode(&out);
        if period {
            let trimmed = text.trim_end();
            if let Some(last) = trimmed
                .chars()
                .last()
                .filter(|c| matches!(c, '.' | '。' | '．'))
            {
                text.truncate(trimmed.len() - last.len_utf8());
            }
        }
        if self.lowercase {
            text = recase(&text, sentence);
        }
        Ok(text)
    }

    /// The best translation of the source `pieces` by beam search over
    /// the decoder step (see the evidence document for the rules).
    fn search(&self, pieces: &[u32]) -> Result<Vec<u32>, String> {
        let settings = &self.settings;
        let mut ids: Vec<i32> = pieces.iter().map(|&id| id as i32).collect();
        ids.push(settings.end as i32);
        let length = ids.len();
        let padding = Tensor::f32(vec![1, length], vec![0.0; length]);
        let mut inputs = vec![None, None];
        inputs[self.encoder.input("src_ids")?] = Some(Tensor::i32(vec![1, length], ids));
        inputs[self.encoder.input("src_paddings")?] = Some(padding.clone());
        let encoded = self
            .encoder
            .run(inputs.into_iter().flatten().collect())?
            .swap_remove(self.encoder.output("src_encs")?);
        let mut inputs = vec![None, None, None];
        inputs[self.init.input("num_hyps")?] = Some(Tensor::i32(vec![1], vec![1]));
        inputs[self.init.input("src_enc_paddings")?] = Some(padding.clone());
        inputs[self.init.input("src_encs")?] = Some(encoded.clone());
        if inputs.iter().any(Option::is_none) {
            return Err("the decoder start has other inputs".into());
        }
        let started = self.init.run(inputs.into_iter().flatten().collect())?;
        // Each state as (width, one row per hypothesis).
        let mut states: Vec<(usize, Vec<f32>)> = Vec::new();
        for (position, (name, _, start)) in self.states.iter().enumerate() {
            let feed = self
                .feeds
                .iter()
                .position(|feed| *feed == Feed::State(name.clone()))
                .ok_or("a state has no input")?;
            let declared = self.step.input_shape(feed);
            let state = match start {
                Some(at) => started[*at].floats()?.to_vec(),
                None => {
                    let width = declared.last().copied().unwrap_or(-1);
                    if width <= 0 {
                        return Err(format!("the width of state {position} is unknown"));
                    }
                    vec![0.0; width as usize]
                }
            };
            states.push((state.len(), state));
        }
        let mut hypotheses = vec![Hypothesis {
            pieces: Vec::new(),
            score: 0.0,
        }];
        let mut finished: Option<Hypothesis> = None;
        let longest = settings.longest.max(2 * length);
        for _ in 0..longest {
            let count = hypotheses.len();
            let inputs = self
                .feeds
                .iter()
                .map(|feed| match feed {
                    Feed::StepIds => Ok(Tensor::i32(
                        vec![count, 1],
                        hypotheses
                            .iter()
                            .map(|h| *h.pieces.last().unwrap_or(&settings.start) as i32)
                            .collect(),
                    )),
                    Feed::Encoded => Ok(encoded.clone()),
                    Feed::Padding => Ok(padding.clone()),
                    Feed::State(name) => {
                        let at = self
                            .states
                            .iter()
                            .position(|(state, _, _)| state == name)
                            .ok_or("a state is missing")?;
                        let (width, rows) = &states[at];
                        Ok(Tensor::f32(vec![count, *width], rows.clone()))
                    }
                })
                .collect::<Result<Vec<Tensor>, String>>()?;
            let outputs = self.step.run(inputs)?;
            let log_probs = outputs[self.log_probs].floats()?;
            let words = log_probs.len() / count.max(1);
            if words == 0 || log_probs.len() != words * count {
                return Err("the decoder step gave no scores".into());
            }
            let mut candidates: Vec<(f32, usize, u32)> = Vec::new();
            for (parent, (hypothesis, row)) in
                hypotheses.iter().zip(log_probs.chunks(words)).enumerate()
            {
                let best = row.iter().copied().fold(f32::NEG_INFINITY, f32::max);
                let end = settings.end as usize;
                if let Some(&end_score) = row.get(end) {
                    if end_score >= best - settings.end_margin {
                        let score = hypothesis.score + end_score;
                        if finished.as_ref().is_none_or(|done| score > done.score) {
                            finished = Some(Hypothesis {
                                pieces: hypothesis.pieces.clone(),
                                score,
                            });
                        }
                    }
                }
                let mut top: Vec<(f32, u32)> = Vec::with_capacity(settings.beam + 1);
                for (word, &score) in row.iter().enumerate() {
                    let word = word as u32;
                    if word == settings.end || NEVER.contains(&word) || !score.is_finite() {
                        continue;
                    }
                    if top.len() < settings.beam || score > top[top.len() - 1].0 {
                        let at = top.partition_point(|&(other, _)| other >= score);
                        top.insert(at, (score, word));
                        top.truncate(settings.beam);
                    }
                }
                candidates.extend(
                    top.into_iter()
                        .map(|(score, word)| (hypothesis.score + score, parent, word)),
                );
            }
            candidates.sort_by(|a, b| b.0.total_cmp(&a.0));
            candidates.truncate(settings.beam);
            let Some(&(best, _, _)) = candidates.first() else {
                break;
            };
            if finished.as_ref().is_some_and(|done| done.score >= best) {
                break;
            }
            let mut next_states = Vec::with_capacity(states.len());
            for (at, (_, out, _)) in self.states.iter().enumerate() {
                let rows = outputs[*out].floats()?;
                let width = states[at].0;
                if rows.len() != width * count {
                    return Err("a state changed size".into());
                }
                let gathered: Vec<f32> = candidates
                    .iter()
                    .flat_map(|&(_, parent, _)| {
                        rows[parent * width..(parent + 1) * width].iter().copied()
                    })
                    .collect();
                next_states.push((width, gathered));
            }
            states = next_states;
            hypotheses = candidates
                .iter()
                .map(|&(score, parent, word)| {
                    let mut pieces = hypotheses[parent].pieces.clone();
                    pieces.push(word);
                    Hypothesis { pieces, score }
                })
                .collect();
        }
        Ok(match finished {
            Some(done) => done.pieces,
            None => hypotheses.swap_remove(0).pieces,
        })
    }
}

fn ends_in_punctuation(text: &str) -> bool {
    text.trim_end().chars().last().is_some_and(|c| {
        c.is_ascii_punctuation() || matches!(c, '。' | '！' | '？' | '．' | '」' | '』' | '…')
    })
}

/// `text` with each run of Latin letters and digits that the source has
/// with capitals written as in the source.
fn recase(text: &str, source: &str) -> String {
    let runs = |text: &str| -> Vec<(usize, usize)> {
        let mut out = Vec::new();
        let mut start = None;
        for (at, c) in text.char_indices().chain([(text.len(), ' ')]) {
            match (c.is_ascii_alphanumeric(), start) {
                (true, None) => start = Some(at),
                (false, Some(begin)) => {
                    out.push((begin, at));
                    start = None;
                }
                _ => {}
            }
        }
        out
    };
    let mut written: HashMap<String, &str> = HashMap::new();
    for (start, end) in runs(source) {
        let word = &source[start..end];
        if word.bytes().any(|b| b.is_ascii_uppercase()) {
            written.entry(word.to_ascii_lowercase()).or_insert(word);
        }
    }
    if written.is_empty() {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    for (start, end) in runs(text) {
        if let Some(word) = written.get(&text[start..end].to_ascii_lowercase()) {
            out.push_str(&text[last..start]);
            out.push_str(word);
            last = end;
        }
    }
    out.push_str(&text[last..]);
    out
}

/// When each update server asked not to be asked again before.
static UPDATE_WAITS: Waits = std::sync::LazyLock::new(Default::default);

/// The seconds the `X-Retry-After` header of an answer from the update
/// server `url` asks to wait, followed only over https as Chrome's
/// `RequestSender::OnNetworkFetcherComplete` follows it; `None` without a
/// whole number there. Zero ends an earlier wait.
fn retry_after(url: &str, headers: &reqwest::header::HeaderMap) -> Option<i64> {
    if !url.starts_with("https://") {
        return None;
    }
    headers
        .get("X-Retry-After")?
        .to_str()
        .ok()?
        .trim()
        .parse::<i64>()
        .ok()
        .filter(|&seconds| seconds >= 0)
}

/// Directions loaded into memory, by source and target language.
type Loaded = HashMap<(String, String), Arc<Direction>>;

#[derive(Clone)]
pub(super) struct Chromium {
    folder: PathBuf,
    update: String,
    client: reqwest::Client,
    loaded: Arc<tokio::sync::Mutex<Loaded>>,
}

impl std::fmt::Debug for Chromium {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Chromium")
            .field("folder", &self.folder)
            .field("update", &self.update)
            .finish()
    }
}

fn failed(error: impl std::fmt::Display) -> ErrorInfo {
    ErrorInfo::new("translation_failed", error.to_string())
}

impl Chromium {
    /// Packs are kept in `folder`; `update` is the component update server.
    pub(super) fn new(folder: PathBuf, update: String, client: reqwest::Client) -> Self {
        Self {
            folder,
            update,
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
        let to = pack_language(to);
        let mut routes: HashMap<String, Vec<Arc<Direction>>> = HashMap::new();
        let mut jobs = Vec::with_capacity(texts.len());
        for text in texts {
            let from = pack_language(super::script_language(text).unwrap_or(PIVOT));
            if !routes.contains_key(&from) {
                let route = self.route(&from, &to, false).await?;
                routes.insert(from.clone(), route);
            }
            jobs.push((routes[&from].clone(), text.clone()));
        }
        tokio::task::spawn_blocking(move || {
            jobs.into_iter()
                .map(|(route, text)| {
                    route
                        .iter()
                        .try_fold(text, |text, direction| direction.translate(&text))
                        .map_err(failed)
                })
                .collect()
        })
        .await
        .map_err(failed)?
    }

    /// Fetches the packs that take `from` to `to` when they are not on
    /// disk yet, or a newer version of them when the update server offers
    /// one.
    pub(super) async fn prepare(&self, from: &str, to: &str) -> Result<(), ErrorInfo> {
        self.route(&pack_language(from), &pack_language(to), true)
            .await
            .map(drop)
    }

    /// The loaded directions that take `from` to `to`, through English
    /// between two other languages; packs are fetched first when they are
    /// not on disk yet. A pack already on disk is checked for a newer
    /// version the first time this process uses it: before loading when
    /// `wait`, else alongside, the new version then used from the next
    /// load.
    async fn route(
        &self,
        from: &str,
        to: &str,
        wait: bool,
    ) -> Result<Vec<Arc<Direction>>, ErrorInfo> {
        if from == to {
            return Ok(Vec::new());
        }
        let steps = if from == PIVOT || to == PIVOT {
            vec![(from.to_string(), to.to_string())]
        } else {
            vec![
                (from.to_string(), PIVOT.to_string()),
                (PIVOT.to_string(), to.to_string()),
            ]
        };
        for (from, to) in &steps {
            let language = if from == PIVOT { to } else { from };
            if self.manifest(language).await.is_none() || !first_check(&self.pack_folder(language))
            {
                continue;
            }
            if wait {
                let _ = self.update(language).await;
            } else {
                let this = self.clone();
                let language = language.clone();
                tokio::spawn(async move {
                    let _ = this.update(&language).await;
                });
            }
        }
        let mut out = Vec::with_capacity(steps.len());
        for step in steps {
            out.push(self.load(step).await?);
        }
        Ok(out)
    }

    fn pack_folder(&self, language: &str) -> PathBuf {
        self.folder.join(format!("{PIVOT}-{language}"))
    }

    /// The pack's list of files, when all of them are on disk.
    async fn manifest(&self, language: &str) -> Option<Manifest> {
        read_manifest(&self.pack_folder(language)).await
    }

    async fn load(&self, (from, to): (String, String)) -> Result<Arc<Direction>, ErrorInfo> {
        let mut loaded = self.loaded.lock().await;
        if let Some(found) = loaded.get(&(from.clone(), to.clone())) {
            return Ok(found.clone());
        }
        let language = if from == PIVOT { &to } else { &from };
        if key_hash(language).is_none() {
            return Err(failed(format!(
                "Chrome has no language pack for {language}"
            )));
        }
        let folder = self.pack_folder(language);
        let manifest = match self.manifest(language).await {
            Some(manifest) => manifest,
            None => {
                first_check(&folder);
                self.fetch(language).await.map_err(failed)?
            }
        };
        let (want_from, want_to) = (from.clone(), to.clone());
        let locked = reading(&folder).await;
        let reading = DISK.read().await;
        let direction = tokio::task::spawn_blocking(move || -> Result<Direction, String> {
            let _reading = (locked, reading);
            let mut folders: Vec<&str> = manifest
                .files
                .iter()
                .filter_map(|file| file.path.split_once('/').map(|(folder, _)| folder))
                .collect();
            folders.dedup();
            for name in folders {
                let path = folder.join(name);
                let Ok(data) = std::fs::read(path.join("translation_model.pb")) else {
                    continue;
                };
                let found = settings(&data)?;
                let (source, target) = (pack_language(&found.from), pack_language(&found.to));
                if source == want_from && target == want_to {
                    return Direction::load(&path);
                }
            }
            Err(format!("the pack has no {want_from}-{want_to} model"))
        })
        .await
        .map_err(failed)?
        .map_err(|error| failed(format!("the {from}-{to} model: {error}")))?;
        let direction = Arc::new(direction);
        loaded.insert((from, to), direction.clone());
        Ok(direction)
    }

    /// Asks the update server for the pack, downloads and checks it, and
    /// keeps its models and then its list of files.
    async fn fetch(&self, language: &str) -> Result<Manifest, String> {
        let _writing = writing(&self.folder, &format!("{PIVOT}-{language}")).await?;
        if let Some(manifest) = self.manifest(language).await {
            return Ok(manifest);
        }
        let package = self
            .check(language, "0.0.0.0", false)
            .await?
            .ok_or("the update server offers no pack")?;
        self.install(language, &package, &self.pack_folder(language))
            .await
    }

    /// Fetches a newer version of the kept pack into a folder of its own
    /// and puts it in place of the kept one, when the update server offers
    /// one for the kept version; `Ok(false)` when it does not.
    pub(super) async fn update(&self, language: &str) -> Result<bool, String> {
        if self.manifest(language).await.is_none() {
            return Ok(false);
        }
        let _writing = writing(&self.folder, &format!("{PIVOT}-{language}")).await?;
        let Some(kept) = self.manifest(language).await else {
            return Ok(false);
        };
        let Some(package) = self.check(language, &kept.version, true).await? else {
            return Ok(false);
        };
        if package.version == kept.version {
            return Ok(false);
        }
        let folder = self.pack_folder(language);
        let staging = folder.with_extension("update");
        if let Err(error) = self.install(language, &package, &staging).await {
            let _ = tokio::fs::remove_dir_all(&staging).await;
            return Err(error);
        }
        replace_folder(&staging, &folder).await?;
        let mut loaded = self.loaded.lock().await;
        loaded.remove(&(PIVOT.to_string(), language.to_string()));
        loaded.remove(&(language.to_string(), PIVOT.to_string()));
        Ok(true)
    }

    /// What the update server offers for the pack when `version` is kept.
    /// As in Chrome's update_client, an update check in the `background`
    /// is not sent in the day before the time the server's `X-Retry-After`
    /// asked to wait for (`UpdateEngine::IsThrottled`), while one a user
    /// waits for is.
    async fn check(
        &self,
        language: &str,
        version: &str,
        background: bool,
    ) -> Result<Option<Package>, String> {
        let key = key_hash(language).ok_or("no such pack")?;
        let app = component_id(&key);
        let waits = self.folder.join(BACKOFF_FILE);
        let now = chrono::Utc::now().timestamp();
        if background {
            if let Some(until) = asked_to_wait(&UPDATE_WAITS, Some(&waits), &self.update)
                .await
                .filter(|&until| until - LONGEST_RETRY_AFTER < now && now < until)
            {
                return Err(format!(
                    "Google's update server asked not to be asked again for {} more seconds",
                    until - now
                ));
            }
        }
        let response = self
            .client
            .post(&self.update)
            .header("Content-Type", "application/json")
            .header("X-Goog-Update-Interactivity", "fg")
            .header("X-Goog-Update-Updater", format!("chrome-{CHROME_VERSION}"))
            .header("X-Goog-Update-AppId", &app)
            .body(update_request(&app, version).to_string())
            .timeout(UPDATE_WAIT)
            .send()
            .await
            .map_err(|error| error.to_string())?;
        if let Some(seconds) = retry_after(&self.update, response.headers()) {
            let until = (seconds > 0).then(|| now + seconds.min(LONGEST_RETRY_AFTER));
            let _ = tokio::fs::create_dir_all(&self.folder).await;
            set_wait(&UPDATE_WAITS, Some(&waits), &self.update, until).await;
        }
        let answer = body(response, LARGEST_ANSWER).await?;
        package(&String::from_utf8_lossy(&answer), &app)
    }

    /// Downloads and checks `package`, and keeps its models and then its
    /// list of files in `folder`.
    async fn install(
        &self,
        language: &str,
        package: &Package,
        folder: &Path,
    ) -> Result<Manifest, String> {
        let key = key_hash(language).ok_or("no such pack")?;
        if package.size > LARGEST_PACK {
            return Err("the pack is too large".into());
        }
        let mut last_error = String::from("no address to download from");
        let mut downloaded = None;
        for url in &package.urls {
            let attempt = async {
                let response = self
                    .client
                    .get(url)
                    .timeout(DOWNLOAD_WAIT)
                    .send()
                    .await
                    .map_err(|error| error.to_string())?;
                let data = body(response, package.size).await?;
                if data.len() as u64 != package.size || sha256(&data) != package.sha256 {
                    return Err(format!("{url} does not match its listed hash"));
                }
                Ok::<Vec<u8>, String>(data)
            };
            match attempt.await {
                Ok(data) => {
                    downloaded = Some(data);
                    break;
                }
                Err(error) => last_error = error,
            }
        }
        let data = downloaded.ok_or(last_error)?;
        let files =
            tokio::task::spawn_blocking(move || -> Result<Vec<(String, Vec<u8>)>, String> {
                let crx = crx(&data)?;
                verify(&crx, &key)?;
                unzip(crx.archive)
            })
            .await
            .map_err(|error| error.to_string())??;
        if files.is_empty() {
            return Err("the pack has no models".into());
        }
        let mut kept = Vec::with_capacity(files.len());
        for (path, data) in files {
            let (inner, _) = path.split_once('/').ok_or("a file is outside a folder")?;
            tokio::fs::create_dir_all(folder.join(inner))
                .await
                .map_err(|error| format!("{}: {error}", folder.display()))?;
            write_whole(&folder.join(&path), &data).await?;
            kept.push(Kept {
                size: data.len() as u64,
                sha256: sha256(&data),
                path,
            });
        }
        let manifest = Manifest {
            version: package.version.clone(),
            files: kept,
            license: license(),
        };
        let text = serde_json::to_vec_pretty(&manifest).map_err(|error| error.to_string())?;
        write_whole(&folder.join(MANIFEST), &text).await?;
        Ok(manifest)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pack_is_named_by_its_key() {
        let key = key_hash("ja").unwrap();
        assert_eq!(component_id(&key), "hnccddhebmkigcfihhnmiiihcnaogoel");
        assert_eq!(
            component_id(&key_hash(&pack_language("zh-CN")).unwrap()),
            "boeedobmjjddfgncfnmnehcemjbfpfbk"
        );
        assert_eq!(pack_language("he"), "iw");
        assert_eq!(pack_language("ja-JP"), "ja");
        assert_eq!(pack_language("zh-TW"), "zh-Hant");
        assert!(key_hash("xx").is_none());
        let request = update_request("hnccddhebmkigcfihhnmiiihcnaogoel", "2024.9.9.1");
        assert_eq!(
            request["request"]["apps"][0]["appid"],
            "hnccddhebmkigcfihhnmiiihcnaogoel"
        );
        assert_eq!(request["request"]["apps"][0]["version"], "2024.9.9.1");
        assert_eq!(request["request"]["protocol"], "4.0");
    }

    /// A trimmed copy of the update server's answer for en-ja on
    /// 2026-11-25.
    const ANSWER: &str = r#")]}'
{"response":{"server":"prod","protocol":"4.0","apps":[{"appid":"lbimbicckdokpoicboneldipejkhjgdg","status":"ok","updatecheck":{"status":"noupdate"}},{"appid":"hnccddhebmkigcfihhnmiiihcnaogoel","cohort":"1:2ly3:","status":"ok","cohortname":"Auto","updatecheck":{"nextversion":"2024.9.9.1","status":"ok","pipelines":[{"pipeline_id":"1:xz:x:any","operations":[{"size":5,"type":"download","out":{"sha256":"00"},"urls":[{"url":"https://dl.google.com/x"}]},{"type":"xz"},{"size":9,"type":"crx3","in":{"sha256":"00"}}]},{"pipeline_id":"1:full:dbfc:any","operations":[{"size":56490050,"type":"download","out":{"sha256":"DBFCE5644BF0EB52654B211B2B353101482766EB9295FF2E04E9609C8F000000"},"urls":[{"url":"http://edgedl.me.gvt1.com/edgedl/release2/a.crx3"},{"url":"https://edgedl.me.gvt1.com/edgedl/release2/a.crx3"},{"url":"https://dl.google.com/release2/a.crx3"}]},{"size":56490050,"type":"crx3","in":{"sha256":"dbfce5644bf0eb52654b211b2b353101482766eb9295ff2e04e9609c8f000000"}}]}]}}]}}"#;

    #[test]
    fn the_update_answer_gives_the_whole_download_over_https() {
        let found = package(ANSWER, "hnccddhebmkigcfihhnmiiihcnaogoel").unwrap();
        assert_eq!(
            found.unwrap(),
            Package {
                version: "2024.9.9.1".into(),
                size: 56490050,
                sha256: "dbfce5644bf0eb52654b211b2b353101482766eb9295ff2e04e9609c8f000000".into(),
                urls: vec![
                    "https://edgedl.me.gvt1.com/edgedl/release2/a.crx3".into(),
                    "https://dl.google.com/release2/a.crx3".into(),
                ],
            }
        );
        // Nothing newer than the version asked with.
        assert_eq!(
            package(ANSWER, "lbimbicckdokpoicboneldipejkhjgdg"),
            Ok(None)
        );
        assert!(package(ANSWER, "boeedobmjjddfgncfnmnehcemjbfpfbk").is_err());
        assert!(package("not json", "x").is_err());
    }

    fn varint(mut value: u64, out: &mut Vec<u8>) {
        loop {
            let byte = (value & 0x7F) as u8;
            value >>= 7;
            if value == 0 {
                out.push(byte);
                return;
            }
            out.push(byte | 0x80);
        }
    }

    fn bytes_field(number: u64, bytes: &[u8], out: &mut Vec<u8>) {
        varint(number << 3 | 2, out);
        varint(bytes.len() as u64, out);
        out.extend_from_slice(bytes);
    }

    fn synthetic_crx(key: &[u8], id: &[u8], archive: &[u8]) -> Vec<u8> {
        let mut proof = Vec::new();
        bytes_field(1, key, &mut proof);
        bytes_field(2, &[7; 16], &mut proof);
        let mut signed = Vec::new();
        bytes_field(1, id, &mut signed);
        let mut header = Vec::new();
        bytes_field(2, &proof, &mut header);
        bytes_field(10000, &signed, &mut header);
        let mut out = b"Cr24".to_vec();
        out.extend(3u32.to_le_bytes());
        out.extend((header.len() as u32).to_le_bytes());
        out.extend(header);
        out.extend_from_slice(archive);
        out
    }

    #[test]
    fn a_crx_is_read_and_its_key_and_id_are_checked() {
        let key = b"not really a key";
        let hash: [u8; 32] = Sha256::digest(key).into();
        let data = synthetic_crx(key, &hash[..16], b"PK zip");
        let parsed = crx(&data).unwrap();
        assert_eq!(parsed.rsa, [(&key[..], &[7u8; 16][..])]);
        assert_eq!(parsed.archive, b"PK zip");
        // The key and id match, so only the signature is left to fail.
        assert!(verify(&parsed, &hash)
            .unwrap_err()
            .starts_with("the pack's key"));
        let other = key_hash("ja").unwrap();
        assert_eq!(
            verify(&parsed, &other).unwrap_err(),
            "the CRX is for another component"
        );
        let wrong_id = synthetic_crx(key, &other[..16], b"");
        assert_eq!(
            verify(&crx(&wrong_id).unwrap(), &other).unwrap_err(),
            "the CRX is not signed with the pack's key"
        );
        let mut version_two = data.clone();
        version_two[4] = 2;
        assert!(crx(&version_two).is_err());
        assert!(crx(b"PK\x03\x04").is_err());
    }

    /// A zip of `entries` (name, data, deflated).
    fn zip(entries: &[(&str, &[u8], bool)]) -> Vec<u8> {
        use std::io::Write;
        let mut out = Vec::new();
        let mut directory = Vec::new();
        for (name, data, deflated) in entries {
            let stored = if *deflated {
                let mut encoder =
                    flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
                encoder.write_all(data).unwrap();
                encoder.finish().unwrap()
            } else {
                data.to_vec()
            };
            let mut crc = flate2::Crc::new();
            crc.update(data);
            let method: u16 = if *deflated { 8 } else { 0 };
            let local = out.len() as u32;
            out.extend(0x0403_4b50u32.to_le_bytes());
            out.extend([20, 0, 0, 0]);
            out.extend(method.to_le_bytes());
            out.extend([0; 4]);
            out.extend(crc.sum().to_le_bytes());
            out.extend((stored.len() as u32).to_le_bytes());
            out.extend((data.len() as u32).to_le_bytes());
            out.extend((name.len() as u16).to_le_bytes());
            out.extend([0, 0]);
            out.extend(name.as_bytes());
            out.extend(&stored);
            directory.extend(0x0201_4b50u32.to_le_bytes());
            directory.extend([20, 0, 20, 0, 0, 0]);
            directory.extend(method.to_le_bytes());
            directory.extend([0; 4]);
            directory.extend(crc.sum().to_le_bytes());
            directory.extend((stored.len() as u32).to_le_bytes());
            directory.extend((data.len() as u32).to_le_bytes());
            directory.extend((name.len() as u16).to_le_bytes());
            directory.extend([0; 12]);
            directory.extend(local.to_le_bytes());
            directory.extend(name.as_bytes());
        }
        let start = out.len() as u32;
        let size = directory.len() as u32;
        out.extend(directory);
        out.extend(0x0605_4b50u32.to_le_bytes());
        out.extend([0; 4]);
        out.extend((entries.len() as u16).to_le_bytes());
        out.extend((entries.len() as u16).to_le_bytes());
        out.extend(size.to_le_bytes());
        out.extend(start.to_le_bytes());
        out.extend([0, 0]);
        out
    }

    #[test]
    fn only_model_files_in_a_model_folder_come_out_of_the_zip() {
        let model = b"model data ".repeat(50);
        let archive = zip(&[
            ("manifest.json", b"{}", false),
            ("en_ja_nmt/encoder_0.tflite", &model, true),
            ("en_ja_nmt/translation_model.pb", b"\x0a\x02en", false),
            ("en_ja_nmt/src_spm_model.vocab", b"words", false),
            ("../en_ja_nmt/x.pb", b"no", false),
            ("en_ja_nmt/../../x.pb", b"no", false),
            ("en_ja_nmt/sub/x.pb", b"no", false),
            ("/en_ja_nmt/x.pb", b"no", false),
            ("en_ja_dictionary/dictionary.bin", b"no", false),
            (".hidden_nmt/x.pb", b"no", false),
        ]);
        let files = unzip(&archive).unwrap();
        let names: Vec<&str> = files.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(
            names,
            [
                "en_ja_nmt/encoder_0.tflite",
                "en_ja_nmt/translation_model.pb"
            ]
        );
        assert_eq!(files[0].1, model);
        let mut broken = archive.clone();
        let at = broken.windows(4).position(|w| w == b"\x0a\x02en").unwrap();
        broken[at + 2] = b'E';
        assert!(unzip(&broken).unwrap_err().contains("checksum"));
        assert!(unzip(b"not a zip").is_err());
    }

    #[test]
    fn the_model_settings_and_steps_are_read() {
        // translation_model.pb and processor_spec.pb of the en-ja pack
        // (version 2024.9.9.1), up to the language pair and file names.
        let mut model = Vec::new();
        bytes_field(1, b"en", &mut model);
        bytes_field(2, b"ja", &mut model);
        let mut inner = Vec::new();
        let mut search = Vec::new();
        let mut beam = vec![0x08, 0x01, 0x10, 0x02, 0x1d];
        beam.extend(3.0f32.to_le_bytes());
        beam.push(0x2d);
        beam.extend(5.0f32.to_le_bytes());
        search.extend([0x08, 0x02]);
        bytes_field(2, &beam, &mut search);
        bytes_field(3, &search, &mut inner);
        bytes_field(
            4,
            &[0x08, 0x01, 0x10, 0x04, 0x18, 0x48, 0x30, 0x01],
            &mut inner,
        );
        for (name, file) in [
            ("decoder_init_0", "decoder_init_0.tflite"),
            ("encoder_0", "encoder_0.tflite"),
        ] {
            let mut entry = Vec::new();
            bytes_field(1, name.as_bytes(), &mut entry);
            bytes_field(2, file.as_bytes(), &mut entry);
            bytes_field(5, &entry, &mut inner);
        }
        let mut vocabulary = Vec::new();
        bytes_field(1, b"src_spm_model.model", &mut vocabulary);
        bytes_field(6, &vocabulary, &mut inner);
        bytes_field(6, &inner, &mut model);
        let found = settings(&model).unwrap();
        assert_eq!((found.from.as_str(), found.to.as_str()), ("en", "ja"));
        assert_eq!(
            (found.start, found.end, found.beam, found.longest),
            (1, 2, 4, 72)
        );
        assert_eq!(found.end_margin, 5.0);
        assert_eq!(
            found.files[1],
            ("encoder_0".to_string(), "encoder_0.tflite".to_string())
        );
        assert_eq!(found.source_vocabulary, "src_spm_model.model");

        let mut spec = Vec::new();
        bytes_field(1, b"en", &mut spec);
        let mut list = Vec::new();
        for step in ["b2a", "lc", "add_period_10"] {
            bytes_field(3, step.as_bytes(), &mut list);
        }
        bytes_field(3, &list, &mut spec);
        assert_eq!(steps(&spec).unwrap(), ["b2a", "lc", "add_period_10"]);
        assert_eq!(
            state_name("x/feed_beam_state__rnn_state_0_m__00:0"),
            Some("rnn_state_0_m")
        );
        assert_eq!(state_name("x/fetch_fetches__log_probs__00:0"), None);
    }

    #[test]
    fn capitals_come_back_from_the_source() {
        assert_eq!(
            recase("github の api を使う", "Use the GitHub API"),
            "GitHub の API を使う"
        );
        assert_eq!(recase("the api", "no capitals here"), "the api");
        assert!(ends_in_punctuation("Done."));
        assert!(ends_in_punctuation("終わり。"));
        assert!(!ends_in_punctuation("Settings"));
    }

    /// An address nothing answers at, for runs that must not use the
    /// network.
    const NOWHERE: &str = "http://127.0.0.1:9";

    #[tokio::test]
    async fn the_kept_pack_stays_unless_a_newer_one_installs() {
        let folder = tempfile::tempdir().unwrap();
        let kept_folder = folder.path().join("en-ja");
        std::fs::create_dir_all(kept_folder.join("en_ja_nmt")).unwrap();
        std::fs::write(kept_folder.join("en_ja_nmt/encoder_0.tflite"), b"model").unwrap();
        let kept = Manifest {
            version: "2024.9.9.1".into(),
            files: vec![Kept {
                path: "en_ja_nmt/encoder_0.tflite".into(),
                size: 5,
                sha256: sha256(b"model"),
            }],
            license: license(),
        };
        std::fs::write(
            kept_folder.join(MANIFEST),
            serde_json::to_vec(&kept).unwrap(),
        )
        .unwrap();
        let app = component_id(&key_hash("ja").unwrap());
        let answer = |check: Value| {
            format!(
                ")]}}'\n{}",
                json!({"response": {"protocol": "4.0", "apps": [
                    {"appid": app, "status": "ok", "updatecheck": check}]}})
            )
            .into_bytes()
        };
        let none = answer(json!({"status": "noupdate"}));
        // Offered only over http, which is not used.
        let http_only = answer(json!({"status": "ok", "nextversion": "2025.1.1.1",
            "pipelines": [{"operations": [
                {"type": "download", "size": 5, "out": {"sha256": "0".repeat(64)},
                 "urls": [{"url": "http://127.0.0.1:9/a.crx3"}]},
                {"type": "crx3"}]}]}));
        let base = super::super::firefox::tests::stand_in(vec![
            ("/none".into(), none),
            ("/http".into(), http_only),
        ])
        .await;
        for (update, expected) in [
            (format!("{base}/none"), Ok(false)),
            (format!("{base}/http"), Err(())),
            (NOWHERE.to_string(), Err(())),
        ] {
            let chromium = Chromium::new(folder.path().into(), update, reqwest::Client::new());
            assert_eq!(chromium.update("ja").await.map_err(drop), expected);
            assert_eq!(chromium.manifest("ja").await, Some(kept.clone()));
            assert!(!folder.path().join("en-ja.update").exists());
        }
        let chromium = Chromium::new(folder.path().into(), NOWHERE.into(), reqwest::Client::new());
        assert_eq!(chromium.update("de").await, Ok(false));
    }

    #[test]
    fn x_retry_after_is_read_only_over_https() {
        let headers = |value: &str| {
            let mut headers = reqwest::header::HeaderMap::new();
            headers.insert("X-Retry-After", value.parse().unwrap());
            headers
        };
        let secure = "https://update.example/service";
        assert_eq!(retry_after(secure, &headers("3600")), Some(3600));
        assert_eq!(retry_after(secure, &headers("0")), Some(0));
        assert_eq!(retry_after(secure, &headers("-5")), None);
        assert_eq!(retry_after(secure, &headers("soon")), None);
        assert_eq!(
            retry_after(secure, &reqwest::header::HeaderMap::new()),
            None
        );
        assert_eq!(
            retry_after("http://update.example/service", &headers("3600")),
            None
        );
    }

    #[tokio::test]
    async fn an_update_check_waits_while_the_server_asked_but_a_first_fetch_does_not() {
        let folder = tempfile::tempdir().unwrap();
        let kept_folder = folder.path().join("en-ja");
        std::fs::create_dir_all(kept_folder.join("en_ja_nmt")).unwrap();
        std::fs::write(kept_folder.join("en_ja_nmt/encoder_0.tflite"), b"model").unwrap();
        let kept = Manifest {
            version: "2024.9.9.1".into(),
            files: vec![Kept {
                path: "en_ja_nmt/encoder_0.tflite".into(),
                size: 5,
                sha256: sha256(b"model"),
            }],
            license: license(),
        };
        std::fs::write(
            kept_folder.join(MANIFEST),
            serde_json::to_vec(&kept).unwrap(),
        )
        .unwrap();
        let app = component_id(&key_hash("ja").unwrap());
        let none = format!(
            ")]}}'\n{}",
            json!({"response": {"protocol": "4.0", "apps": [
                {"appid": app, "status": "ok", "updatecheck": {"status": "noupdate"}}]}})
        );
        let (base, asked) = super::super::firefox::tests::stand_in_with(vec![(
            "/none".into(),
            "200 OK".into(),
            none.into_bytes(),
        )])
        .await;
        let update = format!("{base}/none");
        let waits = folder.path().join(BACKOFF_FILE);
        let now = chrono::Utc::now().timestamp();
        set_wait(&UPDATE_WAITS, Some(&waits), &update, Some(now + 3600)).await;
        let chromium = Chromium::new(folder.path().into(), update.clone(), reqwest::Client::new());
        let error = chromium.update("ja").await.unwrap_err();
        assert!(error.contains("asked not to be asked again"), "{error}");
        assert!(asked.lock().unwrap().is_empty());
        assert_eq!(chromium.check("ja", "0.0.0.0", false).await, Ok(None));
        assert_eq!(asked.lock().unwrap().len(), 1);
        // A wait more than a day away is taken for a wrong clock, as Chrome
        // takes it.
        set_wait(&UPDATE_WAITS, Some(&waits), &update, Some(now + 3 * 86_400)).await;
        UPDATE_WAITS.lock().unwrap().remove(&update);
        assert_eq!(chromium.update("ja").await, Ok(false));
        assert_eq!(asked.lock().unwrap().len(), 2);
        assert_eq!(chromium.manifest("ja").await, Some(kept));
    }

    #[test]
    fn the_packs_give_english_and_their_languages_both_ways() {
        let found = languages();
        assert!(found.from.contains(&"en".to_string()) && found.to.contains(&"ja".to_string()));
        assert_eq!(found.from.len(), PACKS.len() + 1);
        assert!(found.direct.contains(&("ja".into(), "en".into())));
        assert!(found.direct.contains(&("en".into(), "zh-Hant".into())));
        assert_eq!(found.direct.len(), PACKS.len() * 2);
        assert_eq!(license().name, "unknown");
    }

    #[tokio::test]
    #[ignore = "asks Google's update server about the English-Japanese pack; run by hand"]
    async fn the_update_server_offers_a_pack_only_newer_than_the_kept_one() {
        let chromium = Chromium::new(
            std::env::temp_dir().join("inocode-chromium-unused"),
            UPDATE.into(),
            reqwest::Client::new(),
        );
        let offered = chromium
            .check("ja", "0.0.0.0", false)
            .await
            .unwrap()
            .unwrap();
        println!("offered {} ({} bytes)", offered.version, offered.size);
        let again = chromium.check("ja", &offered.version, false).await.unwrap();
        println!("asked with {}: {again:?}", offered.version);
        assert_eq!(again, None);
    }

    #[tokio::test]
    #[ignore = "downloads about 56 MB from Google; run by hand"]
    async fn english_to_japanese_is_fetched_once_and_kept() {
        let folder = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
        let online = Chromium::new(folder.path().into(), UPDATE.into(), reqwest::Client::new());
        let started = std::time::Instant::now();
        online.prepare("en", "ja").await.unwrap();
        println!("fetched and loaded en-ja in {:?}", started.elapsed());
        let kept = folder.path().join("en-ja");
        let mut names: Vec<String> = Vec::new();
        for entry in std::fs::read_dir(&kept).unwrap() {
            let entry = entry.unwrap();
            if entry.path().is_dir() {
                for inner in std::fs::read_dir(entry.path()).unwrap() {
                    names.push(format!(
                        "{}/{}",
                        entry.file_name().to_string_lossy(),
                        inner.unwrap().file_name().to_string_lossy()
                    ));
                }
            } else {
                names.push(entry.file_name().to_string_lossy().into_owned());
            }
        }
        names.sort();
        println!("{names:#?}");
        assert!(names.contains(&"pack.json".to_string()));
        assert!(names.contains(&"en_ja_nmt/encoder_0.tflite".to_string()));
        assert!(names.contains(&"ja_en_nmt/decoder_step_0.tflite".to_string()));
        assert!(names.iter().all(|name| !name.ends_with(".tmp")));
        let offline = Chromium::new(folder.path().into(), NOWHERE.into(), reqwest::Client::new());
        offline.prepare("en", "ja").await.unwrap();
        offline.prepare("ja", "en").await.unwrap();
        assert!(offline.prepare("en", "de").await.is_err());
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "downloads Chrome's English-Japanese pack once; run by hand"]
    async fn english_and_japanese_are_translated_here() {
        let folder = std::env::temp_dir().join("inocode-chromium-models");
        let online = Chromium::new(folder.clone(), UPDATE.into(), reqwest::Client::new());
        if online.manifest("ja").await.is_none() {
            online.fetch("ja").await.unwrap();
        }
        println!(
            "memory before loading (working set, peak): {:?}",
            super::super::firefox::tests::memory()
        );
        let chromium = Chromium::new(folder, NOWHERE.into(), reqwest::Client::new());
        for (from, to) in [("en", "ja"), ("ja", "en")] {
            let started = std::time::Instant::now();
            chromium.route(from, to, false).await.unwrap();
            println!("loaded {from}-{to} from disk in {:?}", started.elapsed());
        }
        println!(
            "memory after loading: {:?}",
            super::super::firefox::tests::memory()
        );
        let texts = [
            ("I should check the configuration file before running the tests.", "ja"),
            ("The user wants a summary of the changes in this pull request.", "ja"),
            ("Let me look at how the server handles requests that time out.", "ja"),
            ("This function returns an error when the file does not exist. Then the caller retries three times.", "ja"),
            ("Open the GitHub settings", "ja"),
            ("テストを実行する前に設定ファイルを確認します。", "en"),
            ("ユーザーはこのプルリクエストの変更点の要約を求めている。", "en"),
            ("サーバーがタイムアウトしたリクエストをどう扱うか見てみよう。", "en"),
        ];
        for (text, to) in texts {
            let started = std::time::Instant::now();
            let out = chromium.translate(&[text.to_string()], to).await.unwrap();
            println!("{text}\n  -> {}  ({:?})", out[0], started.elapsed());
            let expected = if to == "ja" { Some("ja") } else { None };
            assert_eq!(super::super::script_language(&out[0]), expected, "{out:?}");
        }
        println!(
            "memory after translating: {:?}",
            super::super::firefox::tests::memory()
        );
    }
}
