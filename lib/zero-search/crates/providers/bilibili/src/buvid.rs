//! Device identity and local buvid generation.
//!
//! Faithful Rust port of the Android app's biliid local-buvid algorithm,
//! recovered from `com.bilibili.lib.biliid` in tv.danmaku.bili 9.18.0:
//!
//! * Providers are tried in the app's order (SDK 29+):
//!   drmId (prefix `XU`), imei (`XZ`), oaid, mac (`XY`), androidId (`XX`),
//!   uuid (`XW`). The first provider whose content is non-empty and not in
//!   the app's bad-value list wins
//!   (`com.bilibili.lib.biliid.api.internal.{p,r,s,o,k}` +
//!   `S40.d` provider list).
//! * For the winning content string:
//!   `buvid = prefix + md5hex[2] + md5hex[12] + md5hex[22] + md5hex`
//!   where `md5hex` is the lowercase MD5 hex of the content
//!   (`R90.c` in the app). Total length is 37 chars.
//!
//! This client has no access to hardware identifiers (Widevine id, IMEI,
//! MAC), which are empty/unavailable on modern Android anyway. It uses the
//! androidId provider: an Android ID is an OS-generated random 64-bit hex
//! value per device, so this client generates one the same way and derives
//! the buvid with the app's exact algorithm. The uuid provider (`XW`) is
//! kept as the guaranteed fallback, exactly as in the app.
//!
//! In the app, `BuvidHelper.getBuvid()` prefers a *remote* buvid issued by
//! `POST https://app.bilibili.com/x/polymer/buvid/get` and falls back to
//! this local value when the fetch fails or has not completed; the local
//! value is a first-class input to the gRPC metadata.

use md5::{Digest, Md5};
use rand::Rng;

/// The device identity this client presents, mirroring the values the app
/// reads from its own device.
#[derive(Clone, Debug)]
pub struct DeviceIdentity {
    /// Android ID: 16 lowercase hex chars (OS-generated on a real device).
    pub android_id: String,
    /// Random UUID (no dashes) used by the app's last-resort provider.
    pub uuid: String,
    /// Device profile used in the gRPC `Device` metadata.
    pub brand: String,
    pub model: String,
    pub osver: String,
}

impl DeviceIdentity {
    /// Generate a fresh identity the way a new device would appear:
    /// a random Android ID and a random UUID.
    pub fn random() -> Self {
        let mut rng = rand::thread_rng();
        let android_id = format!("{:016x}", rng.gen::<u64>());
        let uuid = format!("{:032x}", rng.gen::<u128>());
        Self {
            android_id,
            uuid,
            brand: "google".to_string(),
            model: "Pixel 8".to_string(),
            osver: "14".to_string(),
        }
    }

    /// Build an identity around a caller-persisted Android ID, so the
    /// derived buvid stays stable across runs (as it does on a device).
    pub fn from_android_id(android_id: impl Into<String>) -> Self {
        let mut id = Self::random();
        id.android_id = android_id.into();
        id
    }

    /// The app's local buvid for this identity: androidId provider (`XX`)
    /// first, uuid provider (`XW`) as the fallback, in app order.
    pub fn local_buvid(&self) -> String {
        if !self.android_id.is_empty() {
            return buvid_from("XX", &self.android_id);
        }
        buvid_from("XW", &self.uuid)
    }
}

/// One provider step of the app algorithm:
/// `prefix + md5[2] + md5[12] + md5[22] + md5` over the content string.
pub fn buvid_from(prefix: &str, content: &str) -> String {
    let mut hasher = Md5::new();
    hasher.update(content.as_bytes());
    let digest = hasher.finalize();
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    let chars: Vec<char> = hex.chars().collect();
    let mut out = String::with_capacity(37);
    out.push_str(prefix);
    out.push(chars[2]);
    out.push(chars[12]);
    out.push(chars[22]);
    out.push_str(&hex);
    out
}

/// Fawkes session id: 8 chars of [0-9a-z], generated per app launch.
pub fn fawkes_session_id() -> String {
    const CHARS: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut rng = rand::thread_rng();
    (0..8)
        .map(|_| CHARS[rng.gen_range(0..CHARS.len())] as char)
        .collect()
}
