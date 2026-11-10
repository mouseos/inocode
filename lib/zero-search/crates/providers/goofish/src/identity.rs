//! Server-owned Goofish identity: generated locally on first use,
//! persisted in the server's data directory, and reused after that.
//! No environment variable, config file or user action is involved.
//!
//! The identity document is the same shape the live path has always
//! consumed — `{ "utdid": ..., "device_id": ..., "ttid": ... }`:
//!
//! - `utdid` comes from [`generate_utdid`], a faithful port of the
//!   app's own `UTUtdid.generateUtdid` (the type-6 fresh-generation
//!   path a new app install takes). It is *generated*, the same way
//!   the app itself creates one — not copied from any device.
//! - `device_id` is empty. In the app this value is server-issued
//!   after a registration exchange; the anonymous MTOP flow works
//!   with it empty (the pre-ZeroSearch deployment ran exactly so).
//! - `ttid` is a build constant of the app build the signing
//!   runtime emulates: `{channel}@fleamarket_android_{version}`
//!   with the channel of the bundled APK (`231200`, from its
//!   `META-INF/fmchannel` entry) and version `7.28.70`.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use hmac::{Hmac, Mac};
use sha1::Sha1;

type HmacSha1 = Hmac<Sha1>;

/// `ttid` for the emulated app build (channel `231200` of the
/// idlefish 7.28.70 distribution build; see module docs).
pub const DEFAULT_TTID: &str = "231200@fleamarket_android_7.28.70";

/// Serializes tests that mutate process environment variables
/// (`HOME` / `XDG_DATA_HOME`) so they cannot interleave.
#[cfg(test)]
pub(crate) static ENV_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

// ---------------------------------------------------------------------------
// UTDID generation (UTUtdid.generateUtdid, classes9 — faithful port)
// ---------------------------------------------------------------------------

/// The 256-byte RC4 key literal from `com.ta.audid.utils.RC4`.
const RC4_KEY: &str = concat!(
    "QrMgt8GGYI6T52ZY5AnhtxkLzb8egpFn3j5JELI8H6wtACbUnZ5cc3aYTsTRbmkAkRJeYbtx92LPBWm7nBO9UIl7y5i5MQNmUZ",
    "Nf5QENurR5tGyo7yJ2G0MBjWvy6iAtlAbacKP0SwOUeUWx5dsBdyhxa7Id1APtybSdDgicBDuNjI0mlZFUzZSS9dmN8lBD0",
    "WTVOMz0pRZbR3cysomRXOO1ghqjJdTcyDIxzpNAEszN8RMGjrzyU7Hjbmwi6YNK"
);

/// The 44-byte constant `UTUtdid.calcHmac` feeds through RC4 to key
/// its HmacSHA1 (byte at index 32 is 53, per the decompiled source).
const HMAC_RAW: [u8; 44] = [
    69, 114, 116, 223, 125, 202, 225, 86, 245, 11, 178, 160, 239, 157, 64, 23, 161, 130, 174, 192,
    113, 116, 240, 153, 49, 226, 9, 217, 33, 176, 188, 178, 139, 53, 30, 134, 64, 152, 74, 207,
    106, 85, 218, 165,
];

const B64_ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Android `Base64.encodeToString(data, 2)`: standard alphabet,
/// with padding, no wrapping.
fn b64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0] as u32,
            *chunk.get(1).unwrap_or(&0) as u32,
            *chunk.get(2).unwrap_or(&0) as u32,
        ];
        let n = (b[0] << 16) | (b[1] << 8) | b[2];
        out.push(B64_ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(B64_ALPHABET[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            B64_ALPHABET[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            B64_ALPHABET[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

fn b64_decode(text: &str) -> Option<Vec<u8>> {
    let mut table = [255u8; 256];
    for (i, &c) in B64_ALPHABET.iter().enumerate() {
        table[c as usize] = i as u8;
    }
    let bytes: Vec<u8> = text.bytes().filter(|&b| b != b'=').collect();
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    for chunk in bytes.chunks(4) {
        let mut n: u32 = 0;
        for (i, &b) in chunk.iter().enumerate() {
            let v = table[b as usize];
            if v == 255 {
                return None;
            }
            n |= (v as u32) << (18 - 6 * i);
        }
        out.push((n >> 16) as u8);
        if chunk.len() > 2 {
            out.push((n >> 8) as u8);
        }
        if chunk.len() > 3 {
            out.push(n as u8);
        }
    }
    Some(out)
}

/// `IntUtils.getBytes(int)`: big-endian.
fn get_bytes(v: i32) -> [u8; 4] {
    v.to_be_bytes()
}

/// `com.ta.utdid2 StringUtils.hashCode`: 31-accumulation over chars.
fn hash_code(s: &str) -> i32 {
    let mut i: i32 = 0;
    for ch in s.chars() {
        i = i.wrapping_mul(31).wrapping_add(ch as i32);
    }
    i
}

/// Standard RC4, key index cycling `% 256` (com.ta.audid.utils.RC4).
fn rc4(data: &[u8]) -> Vec<u8> {
    let key = RC4_KEY.as_bytes();
    debug_assert_eq!(key.len(), 256);
    let mut state: Vec<u8> = (0..=255).collect();
    let mut j = 0usize;
    let mut i3 = 0usize;
    for i in 0..256 {
        let c = key[i3] as usize;
        j = (c + state[i] as usize + j) % 256;
        state.swap(i, j);
        i3 = (i3 + 1) % 256;
    }
    let mut out = Vec::with_capacity(data.len());
    let (mut x, mut y) = (0usize, 0usize);
    for &b in data {
        x = (x + 1) % 256;
        let sx = state[x];
        y = (y + sx as usize) % 256;
        state.swap(x, y);
        out.push(state[(state[x] as usize + sx as usize) % 256] ^ b);
    }
    out
}

/// `UTUtdid.calcHmac`: HmacSHA1 keyed by RC4(the 44-byte constant),
/// Base64 of the digest.
fn calc_hmac(data14: &[u8]) -> String {
    let key = rc4(&HMAC_RAW);
    let mut mac = HmacSha1::new_from_slice(&key).expect("HMAC accepts any key length");
    mac.update(data14);
    b64_encode(&mac.finalize().into_bytes())
}

/// `PhoneInfoUtils.getUniqueID` — the `getImei` fallback on devices
/// without a YunOS CloudUUID: Base64 of time-sec | nanoTime | rand |
/// rand (four big-endian ints).
fn unique_id() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs() as i64 as i32;
    let nano = now.as_nanos() as i64 as i32;
    let mut raw = Vec::with_capacity(16);
    raw.extend_from_slice(&get_bytes(secs));
    raw.extend_from_slice(&get_bytes(nano));
    raw.extend_from_slice(&get_bytes(rand::random::<u32>() as i32));
    raw.extend_from_slice(&get_bytes(rand::random::<u32>() as i32));
    b64_encode(&raw)
}

/// Generate a fresh UTDID exactly as the app does on first install
/// (`UTUtdid.generateUtdid` with an empty extend factor):
/// time-sec | random | 3 | 0 | hashCode(getImei()) |
/// hashCode(calcHmac(first 14 bytes)), Base64 — 24 characters.
pub fn generate_utdid() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64 as i32)
        .unwrap_or(0);
    let mut payload = Vec::with_capacity(18);
    payload.extend_from_slice(&get_bytes(secs));
    payload.extend_from_slice(&get_bytes(rand::random::<u32>() as i32));
    payload.extend_from_slice(&[3, 0]);
    payload.extend_from_slice(&get_bytes(hash_code(&unique_id())));
    let check = hash_code(&calc_hmac(&payload));
    payload.extend_from_slice(&get_bytes(check));
    debug_assert_eq!(payload.len(), 18);
    b64_encode(&payload)
}

/// `UTUtdid.isValidUtdid`: exactly 24 chars, `[0-9a-zA-Z=/+]` only.
pub fn is_valid_utdid(s: &str) -> bool {
    s.len() == 24
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'=' || b == b'/' || b == b'+')
}

/// Internal consistency: bytes 14–17 equal the HMAC check of 0–13.
pub fn check_bytes_ok(utdid: &str) -> bool {
    let Some(raw) = b64_decode(utdid) else {
        return false;
    };
    if raw.len() != 18 {
        return false;
    }
    get_bytes(hash_code(&calc_hmac(&raw[..14]))) == raw[14..18]
}

// ---------------------------------------------------------------------------
// Persistence (server data directory)
// ---------------------------------------------------------------------------

/// The server's per-user data directory:
/// `$XDG_DATA_HOME/zero-search-mcp`, defaulting to
/// `$HOME/.local/share/zero-search-mcp`.
pub(crate) fn data_dir() -> Option<PathBuf> {
    let base = std::env::var("XDG_DATA_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .filter(|s| !s.is_empty())
                .map(|home| PathBuf::from(home).join(".local").join("share"))
        })?;
    Some(base.join("zero-search-mcp"))
}

/// Path of the auto-managed identity document.
pub(crate) fn identity_file() -> Option<PathBuf> {
    Some(data_dir()?.join("goofish").join("identity.json"))
}

/// The identity document the server generates and keeps: a fresh
/// UTDID, an empty (server-issued-in-app, unneeded here) device id,
/// and the emulated build's `ttid`.
fn identity_document() -> serde_json::Value {
    serde_json::json!({
        "utdid": generate_utdid(),
        "device_id": "",
        "ttid": DEFAULT_TTID,
    })
}

/// Return the path of the server-owned identity file, creating and
/// persisting it on first use. An existing file with a non-empty
/// `utdid` is reused as-is. Returns `None` only when the data
/// directory cannot be determined or written.
pub(crate) fn load_or_create_identity() -> Option<String> {
    let path = identity_file()?;
    if let Ok(text) = std::fs::read_to_string(&path) {
        let usable = serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|doc| {
                doc.get("utdid")
                    .and_then(crate::util::as_str)
                    .map(|u| !u.is_empty())
            })
            .unwrap_or(false);
        if usable {
            return Some(path.to_string_lossy().into_owned());
        }
    }
    let dir = path.parent()?;
    std::fs::create_dir_all(dir).ok()?;
    let text = serde_json::to_string_pretty(&identity_document()).ok()?;
    std::fs::write(&path, text).ok()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Some(path.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_utdid_is_valid_and_consistent() {
        for _ in 0..8 {
            let u = generate_utdid();
            assert!(is_valid_utdid(&u), "invalid utdid: {u}");
            assert!(check_bytes_ok(&u), "check bytes mismatch: {u}");
        }
    }

    #[test]
    fn generated_utdids_differ() {
        assert_ne!(generate_utdid(), generate_utdid());
    }

    #[test]
    fn load_or_create_persists_and_reuses() {
        let _guard = ENV_TEST_LOCK.lock().unwrap();
        let unique = format!("zero-search-identity-test-{}", std::process::id());
        let dir = std::env::temp_dir().join(unique);
        std::env::set_var("XDG_DATA_HOME", &dir);
        let first = load_or_create_identity().expect("identity path");
        let text1 = std::fs::read_to_string(&first).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&text1).unwrap();
        let utdid = doc["utdid"].as_str().unwrap();
        assert!(is_valid_utdid(utdid));
        assert!(check_bytes_ok(utdid));
        assert_eq!(doc["device_id"], serde_json::json!(""));
        assert_eq!(doc["ttid"], serde_json::json!(DEFAULT_TTID));
        // Second call reuses the same document, byte for byte.
        let second = load_or_create_identity().expect("identity path");
        assert_eq!(first, second);
        assert_eq!(std::fs::read_to_string(&second).unwrap(), text1);
        std::env::remove_var("XDG_DATA_HOME");
        std::fs::remove_dir_all(&dir).ok();
    }
}
