//! First-use provisioning of the Goofish signing runtime.
//!
//! The release ships a `goofish-payload/` directory next to the
//! server executable: the Rust signing harness (`sgharness`, which
//! runs the app's own SecurityGuard native library under the
//! Unicorn engine — no JVM) and the Android system libraries it
//! loads the library against (see `runtime/goofish/build-payload.sh`).
//! The one piece that cannot be shipped is the Goofish app package
//! itself (~269 MB, Alibaba-proprietary). On the first Goofish
//! call — normally from the server's background pre-warm, before
//! any MTOP request is even built — this module assembles the
//! runtime in the server's data directory:
//!
//! 1. download the pinned app package (idlefish 7.28.70, the build
//!    the harness emulates) from the app-store CDN URL recorded in
//!    `payload.json`, verifying its SHA-256;
//! 2. extract the SecurityGuard native libraries from it;
//! 3. stage the harness's root filesystem (the app package placed
//!    at the path the emulated environment expects it);
//! 4. generate the bundle's `serve.sh` / `serve.cmd`, pointing the
//!    harness at the payload's signer binary and system libraries.
//!
//! The assembled bundle lands in the same data-directory location
//! the resolver has always discovered (`<data dir>/goofish-runtime/
//! serve.sh`), so no discovery behaviour changes. Provisioning is
//! idempotent, runs at most once per process (later calls re-check
//! a marker file), needs no user action, configuration file or
//! environment variable, and only ever runs when a payload is
//! actually present — deployments without one (developer builds,
//! explicit `GOOFISH_SG_SERVE` setups) are untouched.

use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::identity;

pub(crate) const PAYLOAD_DIR_NAME: &str = "goofish-payload";
pub(crate) const RUNTIME_DIR_NAME: &str = "goofish-runtime";
const APK_FILE_NAME: &str = "goofish.apk";
const MARKER_FILE_NAME: &str = "provision.json";

/// Fallback app-package coordinates, used only when a payload's
/// `payload.json` is missing fields. `build-payload.sh` writes the
/// same values into every payload it assembles; the two must move
/// together. The URL is Tencent's app-store CDN, content-addressed
/// by the package's MD5 — it serves exactly this build (verified
/// 2026-11-08: the full download hashes to `apk_sha256`).
const DEFAULT_APK_VERSION: &str = "7.28.70";
const DEFAULT_APK_URL: &str =
    "https://imtt.dd.qq.com/16891/apk/361C15B2C9BC291581DBE4FF8194A899.apk";
const DEFAULT_APK_SHA256: &str = "905718aff71859527c53b80c88a971fc47c7aa5e8c1a6d3d661b9e073b837530";
const DEFAULT_APK_SIZE: u64 = 268_743_306;
const DEFAULT_MAIN_SO: &str = "libsgmainso-6.7.260202.so";
/// APK entries extracted into the runtime's `libs/`: the
/// SecurityGuard library set the harness and the plugin load (the
/// same set `bootstrap.sh` extracted).
const SO_PREFIXES: [&str; 5] = [
    "libsgmainso",
    "libsgmain.so",
    "libsgcore",
    "libsgmisc",
    "libsgnocaptcha",
];

/// Marker value naming the signer generation a runtime was
/// assembled for. Runtimes assembled by the retired JVM payload
/// carry no such field (or a different one) and are re-assembled:
/// their `serve.sh` points into a payload layout that no longer
/// exists. The app package itself is reused — re-assembly only
/// rewrites the scripts and the staged root filesystem.
pub(crate) const SIGNER_KIND: &str = "rust-sgharness";

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PayloadInfo {
    pub dir: PathBuf,
    pub apk_version: String,
    pub apk_url: String,
    pub apk_sha256: String,
    pub apk_size: u64,
    pub main_so: String,
    /// File name of the signer binary inside `signer/`
    /// (`sgharness`, or `sgharness.exe` on Windows).
    pub signer_binary: String,
    /// Android system libraries inside `syslibs/` that the signer
    /// loads the SecurityGuard library against.
    pub syslibs: Vec<String>,
}

fn log(msg: &str) {
    eprintln!("[goofish] {msg}");
}

/// The payload directory shipped next to the server executable, if
/// present and readable.
pub(crate) fn payload_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?.join(PAYLOAD_DIR_NAME);
    if dir.join("payload.json").is_file() {
        Some(dir)
    } else {
        None
    }
}

pub(crate) fn payload_available() -> bool {
    payload_dir().is_some()
}

/// Read and validate `payload.json` in `dir`.
pub(crate) fn load_payload(dir: &Path) -> Result<PayloadInfo, String> {
    let text = std::fs::read_to_string(dir.join("payload.json"))
        .map_err(|e| format!("cannot read {}: {e}", dir.join("payload.json").display()))?;
    let doc: Value = serde_json::from_str(&text)
        .map_err(|e| format!("payload.json in {} is malformed: {e}", dir.display()))?;
    let pick_str = |key: &str| -> Option<String> {
        doc.get(key)
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .filter(|s| !s.is_empty())
    };
    let apk = doc.get("apk");
    let apk_str = |key: &str| -> Option<String> {
        apk.and_then(|a| a.get(key))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .filter(|s| !s.is_empty())
    };
    let signer = doc.get("signer");
    if signer.is_none() {
        // A payload without a signer section is the retired JVM
        // layout (jre/ + harness/); it cannot drive this server.
        return Err(format!(
            "payload in {} is not a Rust-signer payload (no signer section)",
            dir.display()
        ));
    }
    let signer_binary = signer
        .and_then(|s| s.get("binary"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("payload.json in {} names no signer binary", dir.display()))?
        .to_string();
    if !dir.join("signer").join(&signer_binary).is_file() {
        return Err(format!(
            "payload in {} is missing signer/{signer_binary}",
            dir.display()
        ));
    }
    let syslibs: Vec<String> = signer
        .and_then(|s| s.get("syslibs"))
        .and_then(|j| j.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    if syslibs.is_empty() {
        return Err(format!(
            "payload.json in {} lists no signer syslibs",
            dir.display()
        ));
    }
    for lib in &syslibs {
        if !dir.join("syslibs").join(lib).is_file() {
            return Err(format!(
                "payload in {} is missing syslibs/{lib}",
                dir.display()
            ));
        }
    }
    Ok(PayloadInfo {
        dir: dir.to_path_buf(),
        apk_version: apk_str("version").unwrap_or_else(|| DEFAULT_APK_VERSION.to_string()),
        apk_url: apk_str("url").unwrap_or_else(|| DEFAULT_APK_URL.to_string()),
        apk_sha256: apk_str("sha256")
            .unwrap_or_else(|| DEFAULT_APK_SHA256.to_string())
            .to_lowercase(),
        apk_size: apk
            .and_then(|a| a.get("size"))
            .and_then(|v| v.as_u64())
            .unwrap_or(DEFAULT_APK_SIZE),
        main_so: pick_str("main_so").unwrap_or_else(|| DEFAULT_MAIN_SO.to_string()),
        signer_binary,
        syslibs,
    })
}

/// Where the assembled runtime lives (the data-directory bundle
/// location the resolver already discovers).
pub(crate) fn runtime_dir() -> Option<PathBuf> {
    identity::data_dir().map(|d| d.join(RUNTIME_DIR_NAME))
}

fn serve_file_name() -> &'static str {
    if cfg!(windows) {
        "serve.cmd"
    } else {
        "serve.sh"
    }
}

/// Where the staged app package lives inside the harness rootfs:
/// the install path the emulated environment reports for the app,
/// which the SecurityGuard init opens and verifies in place.
fn staged_apk_path(rt: &Path) -> PathBuf {
    rt.join("rootfs")
        .join("data")
        .join("app")
        .join("com.taobao.idlefish-1")
        .join("base.apk")
}

/// Runtime state check: marker written by a previous provisioning
/// run, the serve script, an app package of the expected size, and
/// its staged copy inside the rootfs. The marker must name the
/// current signer generation — a runtime assembled for the retired
/// JVM payload is re-assembled, not reused.
fn runtime_complete(rt: &Path, payload: &PayloadInfo) -> bool {
    if !rt.join(serve_file_name()).is_file() {
        return false;
    }
    let Ok(text) = std::fs::read_to_string(rt.join(MARKER_FILE_NAME)) else {
        return false;
    };
    let Ok(marker) = serde_json::from_str::<Value>(&text) else {
        return false;
    };
    if marker.get("apk_sha256").and_then(|v| v.as_str()) != Some(payload.apk_sha256.as_str()) {
        return false;
    }
    if marker.get("payload_dir").and_then(|v| v.as_str())
        != Some(payload.dir.to_string_lossy().as_ref())
    {
        return false;
    }
    if marker.get("signer").and_then(|v| v.as_str()) != Some(SIGNER_KIND) {
        return false;
    }
    let size_ok = |p: &Path| {
        std::fs::metadata(p)
            .map(|m| m.is_file() && m.len() == payload.apk_size)
            .unwrap_or(false)
    };
    size_ok(&rt.join(APK_FILE_NAME)) && size_ok(&staged_apk_path(rt))
}

static PROVISION_LOCK: Mutex<()> = Mutex::new(());

/// Assemble the runtime in the data directory. Idempotent and
/// serialised: concurrent callers (pre-warm thread, first live
/// call) wait for the one run and then re-check its result.
pub(crate) fn ensure_provisioned() -> Result<PathBuf, String> {
    let _guard = PROVISION_LOCK.lock().unwrap();
    let Some(payload_dir) = payload_dir() else {
        return Err(format!(
            "no {PAYLOAD_DIR_NAME} directory next to the server executable"
        ));
    };
    let payload = load_payload(&payload_dir)?;
    let Some(rt) = runtime_dir() else {
        return Err("no data directory available for the Goofish runtime".to_string());
    };
    if runtime_complete(&rt, &payload) {
        return Ok(rt);
    }
    std::fs::create_dir_all(rt.join("libs"))
        .and_then(|_| std::fs::create_dir_all(rt.join("logs")))
        .map_err(|e| format!("cannot create {}: {e}", rt.display()))?;

    // 1. The app package (downloaded once, SHA-256 verified during
    //    the download; later runs only re-check its size).
    let apk_path = rt.join(APK_FILE_NAME);
    let apk_ok = std::fs::metadata(&apk_path)
        .map(|m| m.is_file() && m.len() == payload.apk_size)
        .unwrap_or(false);
    if !apk_ok {
        download_apk(&payload, &apk_path)?;
    }

    // 2. SecurityGuard libraries out of the package.
    let so_path = rt.join("libs").join(&payload.main_so);
    if !so_path.is_file() {
        extract_libraries(&apk_path, &rt.join("libs"))?;
    }
    if !so_path.is_file() {
        return Err(format!(
            "the downloaded app package did not contain {}",
            payload.main_so
        ));
    }

    // 3. The harness root filesystem, with the verified package
    //    staged at the app's install path inside it.
    stage_rootfs(&rt, &payload)?;

    // 4. The serve script + marker (regenerated whenever any input
    //    above changed, so a moved install re-points itself).
    write_serve_script(&rt, &payload)?;
    let marker = json!({
        "format": 2,
        "signer": SIGNER_KIND,
        "apk_version": payload.apk_version,
        "apk_sha256": payload.apk_sha256,
        "payload_dir": payload.dir.to_string_lossy(),
        "main_so": payload.main_so,
    });
    std::fs::write(
        rt.join(MARKER_FILE_NAME),
        format!("{}\n", serde_json::to_string_pretty(&marker).unwrap()),
    )
    .map_err(|e| format!("cannot write the provisioning marker: {e}"))?;
    log(&format!(
        "signing runtime ready in {} (app package {}, sha256 verified)",
        rt.display(),
        payload.apk_version
    ));
    Ok(rt)
}

fn download_apk(payload: &PayloadInfo, dest: &Path) -> Result<(), String> {
    let part = dest.with_extension("apk.part");
    let _ = std::fs::remove_file(&part);
    log(&format!(
        "first Goofish use: downloading the Goofish app package ({:.0} MB) from the app-store CDN",
        payload.apk_size as f64 / 1_000_000.0
    ));
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(30))
        .timeout(std::time::Duration::from_secs(1800))
        .build()
        .map_err(|e| format!("cannot build the download client: {e}"))?;
    let mut resp = client
        .get(&payload.apk_url)
        .send()
        .map_err(|e| format!("app package download failed: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!(
            "app package download failed: HTTP {}",
            resp.status()
        ));
    }
    let mut file = std::fs::File::create(&part)
        .map_err(|e| format!("cannot create {}: {e}", part.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1024 * 1024];
    let mut written: u64 = 0;
    let mut next_log: u64 = 32 * 1024 * 1024;
    loop {
        let n = resp
            .read(&mut buf)
            .map_err(|e| format!("app package download failed mid-stream: {e}"))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        file.write_all(&buf[..n])
            .map_err(|e| format!("cannot write {}: {e}", part.display()))?;
        written += n as u64;
        if written >= next_log {
            log(&format!(
                "app package download: {written} / {} bytes",
                payload.apk_size
            ));
            next_log += 32 * 1024 * 1024;
        }
    }
    drop(file);
    if written != payload.apk_size {
        let _ = std::fs::remove_file(&part);
        return Err(format!(
            "app package download incomplete: got {written} bytes, expected {}",
            payload.apk_size
        ));
    }
    let digest = format!("{:x}", hasher.finalize());
    if digest != payload.apk_sha256 {
        let _ = std::fs::remove_file(&part);
        return Err(format!(
            "app package failed SHA-256 verification (expected {}, got {digest}); refusing to use it",
            payload.apk_sha256
        ));
    }
    std::fs::rename(&part, dest)
        .map_err(|e| format!("cannot move the app package into place: {e}"))?;
    log("app package downloaded and SHA-256 verified");
    Ok(())
}

// ---------------------------------------------------------------------------
// Minimal ZIP reader (central directory), for pulling the .so files
// out of the app package. Stored and deflated entries only — all an
// APK uses. Sizes come from the central directory, so entries with
// data descriptors are handled.
// ---------------------------------------------------------------------------

struct ZipEntry {
    name: String,
    method: u16,
    comp_size: u64,
    local_offset: u64,
}

fn zip_entries(file: &mut std::fs::File) -> Result<Vec<ZipEntry>, String> {
    let len = file
        .metadata()
        .map_err(|e| format!("cannot stat the app package: {e}"))?
        .len();
    let tail_len = len.min(65_557);
    if tail_len < 22 {
        return Err("the app package is too small to be a ZIP".to_string());
    }
    let mut tail = vec![0u8; tail_len as usize];
    file.seek(SeekFrom::Start(len - tail_len))
        .and_then(|_| file.read_exact(&mut tail))
        .map_err(|e| format!("cannot read the app package: {e}"))?;
    let eocd = (0..tail_len as usize - 21)
        .rev()
        .find(|&i| tail[i..i + 4] == [0x50, 0x4b, 0x05, 0x06])
        .ok_or("the app package has no ZIP end-of-central-directory")?;
    let u32_at = |b: &[u8]| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as u64;
    let cd_size = u32_at(&tail[eocd + 12..eocd + 16]);
    let cd_offset = u32_at(&tail[eocd + 16..eocd + 20]);
    let mut cd = vec![0u8; cd_size as usize];
    file.seek(SeekFrom::Start(cd_offset))
        .and_then(|_| file.read_exact(&mut cd))
        .map_err(|e| format!("cannot read the app package directory: {e}"))?;
    let mut entries = Vec::new();
    let mut i = 0usize;
    while i + 46 <= cd.len() {
        if cd[i..i + 4] != [0x50, 0x4b, 0x01, 0x02] {
            break;
        }
        let u16_at = |b: &[u8]| u16::from_le_bytes([b[0], b[1]]) as usize;
        let method = u16::from_le_bytes([cd[i + 10], cd[i + 11]]);
        let name_len = u16_at(&cd[i + 28..i + 30]);
        let extra_len = u16_at(&cd[i + 30..i + 32]);
        let comment_len = u16_at(&cd[i + 32..i + 34]);
        let name = String::from_utf8_lossy(&cd[i + 46..i + 46 + name_len]).into_owned();
        entries.push(ZipEntry {
            name,
            method,
            comp_size: u32_at(&cd[i + 20..i + 24]),
            local_offset: u32_at(&cd[i + 42..i + 46]),
        });
        i += 46 + name_len + extra_len + comment_len;
    }
    Ok(entries)
}

fn zip_extract_one(file: &mut std::fs::File, entry: &ZipEntry, dest: &Path) -> Result<(), String> {
    let mut lh = [0u8; 30];
    file.seek(SeekFrom::Start(entry.local_offset))
        .and_then(|_| file.read_exact(&mut lh))
        .map_err(|e| format!("cannot read {} in the app package: {e}", entry.name))?;
    if lh[0..4] != [0x50, 0x4b, 0x03, 0x04] {
        return Err(format!("{} has a bad local header", entry.name));
    }
    let name_len = u16::from_le_bytes([lh[26], lh[27]]) as u64;
    let extra_len = u16::from_le_bytes([lh[28], lh[29]]) as u64;
    let data_start = entry.local_offset + 30 + name_len + extra_len;
    file.seek(SeekFrom::Start(data_start))
        .map_err(|e| format!("cannot seek in the app package: {e}"))?;
    let limited = file.take(entry.comp_size);
    let mut out = std::fs::File::create(dest)
        .map_err(|e| format!("cannot create {}: {e}", dest.display()))?;
    match entry.method {
        0 => {
            let mut r = limited;
            std::io::copy(&mut r, &mut out)
                .map_err(|e| format!("cannot extract {}: {e}", entry.name))?;
        }
        8 => {
            let mut decoder = flate2::read::DeflateDecoder::new(limited);
            std::io::copy(&mut decoder, &mut out)
                .map_err(|e| format!("cannot extract {}: {e}", entry.name))?;
        }
        other => {
            return Err(format!(
                "{} uses unsupported ZIP method {other}",
                entry.name
            ))
        }
    }
    Ok(())
}

fn extract_libraries(apk_path: &Path, libs_dir: &Path) -> Result<(), String> {
    let mut file = std::fs::File::open(apk_path)
        .map_err(|e| format!("cannot open {}: {e}", apk_path.display()))?;
    let entries = zip_entries(&mut file)?;
    let mut extracted = 0usize;
    for entry in &entries {
        let Some(base) = entry.name.rsplit('/').next() else {
            continue;
        };
        if !entry.name.contains("/arm64-v8a/") {
            continue;
        }
        if !SO_PREFIXES.iter().any(|p| base.starts_with(p)) {
            continue;
        }
        zip_extract_one(&mut file, entry, &libs_dir.join(base))?;
        extracted += 1;
    }
    if extracted == 0 {
        return Err("the app package contains no SecurityGuard libraries".to_string());
    }
    log(&format!(
        "extracted {extracted} SecurityGuard libraries from the app package"
    ));
    Ok(())
}

/// Stage the harness root filesystem: the directory skeleton the
/// emulated environment expects, with the verified app package
/// placed at the app's install path. The package is hard-linked
/// (it lives in the same data directory, so the link costs no
/// space); a filesystem that refuses the link falls back to a
/// copy. The signer only ever reads the staged package — the
/// SecurityGuard init opens it and verifies its signing block in
/// place — and writes its own state under the app's data dirs.
fn stage_rootfs(rt: &Path, payload: &PayloadInfo) -> Result<(), String> {
    let rootfs = rt.join("rootfs");
    let app_dir = rootfs
        .join("data")
        .join("app")
        .join("com.taobao.idlefish-1");
    let data_dir = rootfs.join("data").join("data").join("com.taobao.idlefish");
    for dir in [
        app_dir.clone(),
        data_dir.join("files"),
        data_dir.join("cache"),
        rootfs.join("tmp"),
        rootfs.join("system"),
    ] {
        std::fs::create_dir_all(&dir)
            .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }
    let staged = app_dir.join("base.apk");
    let staged_ok = std::fs::metadata(&staged)
        .map(|m| m.is_file() && m.len() == payload.apk_size)
        .unwrap_or(false);
    if !staged_ok {
        let _ = std::fs::remove_file(&staged);
        let src = rt.join(APK_FILE_NAME);
        if std::fs::hard_link(&src, &staged).is_err() {
            std::fs::copy(&src, &staged)
                .map_err(|e| format!("cannot stage the app package into the rootfs: {e}"))?;
        }
        let len = std::fs::metadata(&staged).map(|m| m.len()).unwrap_or(0);
        if len != payload.apk_size {
            return Err(format!(
                "staged app package has the wrong size ({len} bytes, expected {})",
                payload.apk_size
            ));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Serve script generation
// ---------------------------------------------------------------------------

fn sh_escape(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('$', "\\$")
        .replace('`', "\\`")
}

pub(crate) fn serve_script_unix(rt: &Path, payload: &PayloadInfo) -> String {
    let payload_str = payload.dir.to_string_lossy();
    let _ = rt;
    format!(
        "#!/bin/sh\n\
         # Assembled by zero-search-mcp on first Goofish use — do not edit;\n\
         # it is regenerated when the bundled signing payload changes.\n\
         set -u\n\
         RT=\"$(cd \"$(dirname \"$0\")\" && pwd)\"\n\
         PAYLOAD=\"{payload_esc}\"\n\
         mkdir -p \"$RT/logs\" \"$RT/rootfs\"\n\
         cd \"$RT\"\n\
         exec \"$PAYLOAD/signer/{bin}\" serve \\\n\
         \x20 --rootfs \"$RT/rootfs\" \\\n\
         \x20 --sg \"$RT/libs/{so}\" \\\n\
         \x20 --syslibs \"$PAYLOAD/syslibs\" \\\n\
         \x20 2>>\"$RT/logs/serve_stderr.log\"\n",
        payload_esc = sh_escape(&payload_str),
        bin = payload.signer_binary,
        so = payload.main_so,
    )
}

pub(crate) fn serve_script_windows(rt: &Path, payload: &PayloadInfo) -> String {
    let payload_str = payload.dir.to_string_lossy().replace('/', "\\");
    let _ = rt;
    format!(
        "@echo off\n\
         rem Assembled by zero-search-mcp on first Goofish use — do not edit;\n\
         rem it is regenerated when the bundled signing payload changes.\n\
         set \"RT=%~dp0\"\n\
         set \"PAYLOAD={payload_str}\"\n\
         if not exist \"%RT%logs\" mkdir \"%RT%logs\"\n\
         if not exist \"%RT%rootfs\" mkdir \"%RT%rootfs\"\n\
         cd /d \"%RT%\"\n\
         \"%PAYLOAD%\\signer\\{bin}\" serve --rootfs \"%RT%rootfs\" --sg \"%RT%libs\\{so}\" --syslibs \"%PAYLOAD%\\syslibs\" 2>>\"%RT%logs\\serve_stderr.log\"\n",
        bin = payload.signer_binary,
        so = payload.main_so,
    )
}

fn write_serve_script(rt: &Path, payload: &PayloadInfo) -> Result<(), String> {
    let path = rt.join(serve_file_name());
    let body = if cfg!(windows) {
        serve_script_windows(rt, payload)
    } else {
        serve_script_unix(rt, payload)
    };
    std::fs::write(&path, body).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&path)
            .map_err(|e| format!("cannot stat {}: {e}", path.display()))?
            .permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms)
            .map_err(|e| format!("cannot mark {} executable: {e}", path.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_payload(dir: &Path) -> PayloadInfo {
        PayloadInfo {
            dir: dir.to_path_buf(),
            apk_version: "7.28.70".to_string(),
            apk_url: DEFAULT_APK_URL.to_string(),
            apk_sha256: DEFAULT_APK_SHA256.to_string(),
            apk_size: DEFAULT_APK_SIZE,
            main_so: DEFAULT_MAIN_SO.to_string(),
            signer_binary: "sgharness".to_string(),
            syslibs: vec!["libc.so".to_string(), "libz.so".to_string()],
        }
    }

    #[test]
    fn payload_json_round_trip() {
        let dir = std::env::temp_dir().join(format!("gf_prov_test_{}", std::process::id()));
        let payload_dir = dir.join("goofish-payload");
        std::fs::create_dir_all(payload_dir.join("signer")).unwrap();
        std::fs::create_dir_all(payload_dir.join("syslibs")).unwrap();
        std::fs::write(payload_dir.join("signer").join("sgharness"), b"x").unwrap();
        std::fs::write(payload_dir.join("syslibs").join("libc.so"), b"x").unwrap();
        std::fs::write(
            payload_dir.join("payload.json"),
            json!({
                "format": 2,
                "platform": "linux-x64",
                "signer": {"kind": SIGNER_KIND, "binary": "sgharness",
                           "syslibs": ["libc.so"]},
                "apk": {"version": "7.28.70", "url": DEFAULT_APK_URL,
                        "sha256": DEFAULT_APK_SHA256, "size": DEFAULT_APK_SIZE},
                "main_so": DEFAULT_MAIN_SO,
            })
            .to_string(),
        )
        .unwrap();
        let info = load_payload(&payload_dir).unwrap();
        assert_eq!(info.apk_sha256, DEFAULT_APK_SHA256);
        assert_eq!(info.apk_size, DEFAULT_APK_SIZE);
        assert_eq!(info.signer_binary, "sgharness");
        assert_eq!(info.syslibs, vec!["libc.so".to_string()]);
        // A payload whose syslib is gone is refused, not half-used.
        std::fs::remove_file(payload_dir.join("syslibs").join("libc.so")).unwrap();
        assert!(load_payload(&payload_dir).is_err());
        // The retired JVM layout (no signer section) is refused too.
        std::fs::write(
            payload_dir.join("payload.json"),
            json!({
                "format": 1,
                "platform": "linux-x64",
                "apk": {"version": "7.28.70", "url": DEFAULT_APK_URL,
                        "sha256": DEFAULT_APK_SHA256, "size": DEFAULT_APK_SIZE},
                "main_so": DEFAULT_MAIN_SO,
                "harness": {"main_class": "local.goofish.SgHarness", "jars": ["a.jar"]},
            })
            .to_string(),
        )
        .unwrap();
        assert!(load_payload(&payload_dir).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn serve_script_unix_shape() {
        let payload = fake_payload(Path::new("/opt/zero search/goofish-payload"));
        let script = serve_script_unix(Path::new("/data/goofish-runtime"), &payload);
        // The signer binary is exec'd with the runtime's rootfs, the
        // extracted SecurityGuard library, and the payload's syslibs.
        assert!(
            script.contains("exec \"$PAYLOAD/signer/sgharness\" serve"),
            "{script}"
        );
        assert!(script.contains("--rootfs \"$RT/rootfs\""), "{script}");
        assert!(
            script.contains("--sg \"$RT/libs/libsgmainso-6.7.260202.so\""),
            "{script}"
        );
        assert!(
            script.contains("--syslibs \"$PAYLOAD/syslibs\""),
            "{script}"
        );
        // The space in the payload path is safe inside the quoting.
        assert!(script.contains("PAYLOAD=\"/opt/zero search/goofish-payload\""));
        // No JVM anywhere in the generated script.
        assert!(!script.contains("java"), "{script}");
    }

    #[test]
    fn serve_script_windows_shape() {
        let payload = fake_payload(Path::new("C:\\zero search\\goofish-payload"));
        let script = serve_script_windows(Path::new("D:\\data\\goofish-runtime"), &payload);
        assert!(script.contains("\\signer\\sgharness"), "{script}");
        assert!(script.contains("--rootfs \"%RT%rootfs\""), "{script}");
        assert!(
            script.contains("libs\\libsgmainso-6.7.260202.so"),
            "{script}"
        );
        assert!(!script.contains("java"), "{script}");
    }

    #[test]
    fn stage_rootfs_places_the_package() {
        let dir = std::env::temp_dir().join(format!("gf_stage_test_{}", std::process::id()));
        let rt = dir.join("goofish-runtime");
        std::fs::create_dir_all(&rt).unwrap();
        let mut payload = fake_payload(Path::new("/payload"));
        payload.apk_size = 11;
        std::fs::write(rt.join(APK_FILE_NAME), b"FAKEAPKDATA").unwrap();
        stage_rootfs(&rt, &payload).unwrap();
        let staged = staged_apk_path(&rt);
        assert_eq!(std::fs::read(&staged).unwrap(), b"FAKEAPKDATA");
        assert!(rt
            .join("rootfs")
            .join("data")
            .join("data")
            .join("com.taobao.idlefish")
            .join("files")
            .is_dir());
        // Idempotent: a second run keeps the staged package.
        stage_rootfs(&rt, &payload).unwrap();
        // A truncated staged copy is replaced. (Remove + rewrite:
        // the staged copy is a hard link, so writing through it
        // would truncate the source package too.)
        std::fs::remove_file(&staged).unwrap();
        std::fs::write(&staged, b"SHORT").unwrap();
        stage_rootfs(&rt, &payload).unwrap();
        assert_eq!(std::fs::read(&staged).unwrap(), b"FAKEAPKDATA");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn zip_extract_stored_and_deflated() {
        // Build a tiny ZIP in memory with one stored and one
        // deflated entry and pull them back out by name.
        use flate2::write::DeflateEncoder;
        use flate2::Compression;
        let dir = std::env::temp_dir().join(format!("gf_zip_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let apk = dir.join("mini.apk");
        let stored_data = b"STORED-DATA".to_vec();
        let deflated_data = b"DEFLATED-DATA-DEFLATED-DATA".to_vec();
        let mut enc = DeflateEncoder::new(Vec::new(), Compression::default());
        enc.write_all(&deflated_data).unwrap();
        let deflated_comp = enc.finish().unwrap();
        // (name, method, data-as-stored)
        let entries: Vec<(&str, u16, Vec<u8>)> = vec![
            ("lib/arm64-v8a/libsgmainso-1.so", 0, stored_data.clone()),
            ("lib/arm64-v8a/libsgcore.so", 8, deflated_comp.clone()),
            ("classes.dex", 0, b"IGNORED".to_vec()),
        ];
        let mut out: Vec<u8> = Vec::new();
        let mut central: Vec<u8> = Vec::new();
        for (name, method, data) in &entries {
            let offset = out.len() as u32;
            let crc = 0u32; // the reader under test never checks CRC
            out.extend_from_slice(&0x04034b50u32.to_le_bytes());
            out.extend_from_slice(&20u16.to_le_bytes()); // version
            out.extend_from_slice(&0u16.to_le_bytes()); // flags
            out.extend_from_slice(&method.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes()); // time
            out.extend_from_slice(&0u16.to_le_bytes()); // date
            out.extend_from_slice(&crc.to_le_bytes());
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
            let plain_len = if *method == 8 {
                deflated_data.len()
            } else {
                data.len()
            };
            out.extend_from_slice(&(plain_len as u32).to_le_bytes());
            out.extend_from_slice(&(name.len() as u16).to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes()); // extra len
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(data);
            // Central directory entry, field offsets per APPNOTE:
            // method @10, comp size @20, size @24, name len @28,
            // extra @30, comment @32, local header offset @42.
            central.extend_from_slice(&0x02014b50u32.to_le_bytes());
            central.extend_from_slice(&20u16.to_le_bytes()); // version made by
            central.extend_from_slice(&20u16.to_le_bytes()); // version needed
            central.extend_from_slice(&0u16.to_le_bytes()); // flags
            central.extend_from_slice(&method.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes()); // time
            central.extend_from_slice(&0u16.to_le_bytes()); // date
            central.extend_from_slice(&crc.to_le_bytes());
            central.extend_from_slice(&(data.len() as u32).to_le_bytes());
            central.extend_from_slice(&(plain_len as u32).to_le_bytes());
            central.extend_from_slice(&(name.len() as u16).to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes()); // extra len
            central.extend_from_slice(&0u16.to_le_bytes()); // comment len
            central.extend_from_slice(&0u16.to_le_bytes()); // disk number
            central.extend_from_slice(&0u16.to_le_bytes()); // internal attrs
            central.extend_from_slice(&0u32.to_le_bytes()); // external attrs
            central.extend_from_slice(&offset.to_le_bytes());
            central.extend_from_slice(name.as_bytes());
        }
        let cd_offset = out.len() as u32;
        out.extend_from_slice(&central);
        out.extend_from_slice(&0x06054b50u32.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        out.extend_from_slice(&(central.len() as u32).to_le_bytes());
        out.extend_from_slice(&cd_offset.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        std::fs::write(&apk, &out).unwrap();

        let libs = dir.join("libs");
        std::fs::create_dir_all(&libs).unwrap();
        extract_libraries(&apk, &libs).unwrap();
        assert_eq!(
            std::fs::read(libs.join("libsgmainso-1.so")).unwrap(),
            stored_data
        );
        assert_eq!(
            std::fs::read(libs.join("libsgcore.so")).unwrap(),
            deflated_data
        );
        assert!(!libs.join("classes.dex").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
