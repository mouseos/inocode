//! InnerTube HTTP client: request construction exactly as the YouTube
//! Android app builds it.
//!
//! Grounding in the analysed APK (com.google.android.youtube 21.44.534):
//! * Production InnerTube backend `https://youtubei.googleapis.com` —
//!   `defpackage/akyp.java` (backend enum `PRODUCTION`, classes*.dex).
//! * API key returned by `defpackage/ixl.java::a()` (classes.dex) and sent
//!   both as the `key` query parameter and the `X-Goog-Api-Key` header.
//! * The app interface is `ANDROID` (`defpackage/aqbe.java`), InnerTube
//!   client name id 3, and the client version is the app version itself.
//! * A guest `visitorData` is issued by the server: it arrives in the
//!   `responseContext` of InnerTube responses and is then sent back in the
//!   context and as the `X-Goog-Visitor-Id` header, mirroring the app's
//!   visitor-data provider. Nothing visitor-related is fabricated.

use std::sync::Mutex;

use anyhow::Result;
use serde_json::{json, Value};

use crate::errors::{YoutubeError, KIND_ACCESS_RESTRICTED, KIND_CAPTCHA, KIND_HTTP_ERROR};

/// InnerTube API key embedded in the app (`defpackage/ixl.java::a()`).
pub const INNERTUBE_API_KEY: &str = "AIzaSyA8eiZmM1FaDVjRy-df2KTyQ_vz_yYM39w";
/// Production InnerTube backend (`defpackage/akyp.java::PRODUCTION`).
pub const INNERTUBE_BASE: &str = "https://youtubei.googleapis.com";
/// App version of the analysed APK; the app reports it as clientVersion.
pub const APP_VERSION: &str = "21.44.534";
/// InnerTube client name for the Android app interface.
pub const CLIENT_NAME: &str = "ANDROID";
/// Numeric id of the ANDROID client, sent as `X-YouTube-Client-Name`.
pub const CLIENT_NAME_ID: u32 = 3;

/// Representative Android device profile. The app fills these fields from
/// the device it runs on; a fixed mainstream profile is used here, as in
/// every third-party InnerTube client.
pub struct DeviceProfile {
    pub android_sdk_version: u32,
    pub device_make: &'static str,
    pub device_model: &'static str,
    pub os_version: &'static str,
    pub user_agent: String,
}

pub fn default_profile() -> DeviceProfile {
    DeviceProfile {
        android_sdk_version: 34,
        device_make: "Google",
        device_model: "Pixel 8",
        os_version: "14",
        user_agent: format!(
            "com.google.android.youtube/{APP_VERSION} (Linux; U; Android 14; en_US; Pixel 8 Build/AP1A.240405.002) gzip"
        ),
    }
}

/// Stateful InnerTube client holding the server-issued guest visitorData.
pub struct YoutubeClient {
    http: reqwest::Client,
    profile: DeviceProfile,
    hl: String,
    gl: String,
    visitor_data: Mutex<Option<String>>,
}

impl Default for YoutubeClient {
    fn default() -> Self {
        Self::new()
    }
}

impl YoutubeClient {
    pub fn new() -> Self {
        Self::with_locale("en", "US")
    }

    pub fn with_locale(hl: &str, gl: &str) -> Self {
        let profile = default_profile();
        let http = reqwest::Client::builder()
            .user_agent(profile.user_agent.clone())
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .expect("failed to build HTTP client");
        YoutubeClient {
            http,
            profile,
            hl: hl.to_string(),
            gl: gl.to_string(),
            visitor_data: Mutex::new(None),
        }
    }

    /// The currently held server-issued visitorData, if any.
    pub fn visitor_data(&self) -> Option<String> {
        self.visitor_data.lock().unwrap().clone()
    }

    /// The `context` object the app attaches to every InnerTube request.
    pub fn context_json(&self) -> Value {
        let mut client = json!({
            "clientName": CLIENT_NAME,
            "clientVersion": APP_VERSION,
            "deviceMake": self.profile.device_make,
            "deviceModel": self.profile.device_model,
            "androidSdkVersion": self.profile.android_sdk_version,
            "osName": "Android",
            "osVersion": self.profile.os_version,
            "hl": self.hl,
            "gl": self.gl,
            "originalUrl": "https://www.youtube.com",
            "platform": "MOBILE",
            "clientFormFactor": "UNKNOWN_FORM_FACTOR",
            "browserName": "Android",
            "browserVersion": APP_VERSION,
        });
        if let Some(vd) = self.visitor_data() {
            client["visitorData"] = json!(vd);
        }
        json!({
            "client": client,
            "user": { "lockedSafetyMode": false },
            "request": { "useSsl": true },
        })
    }

    fn endpoint_url(method: &str) -> String {
        format!("{INNERTUBE_BASE}/youtubei/v1/{method}?key={INNERTUBE_API_KEY}&prettyPrint=false")
    }

    /// POST one InnerTube method with the app context merged into `body`.
    /// On success returns the parsed response and records any visitorData
    /// the server issued in `responseContext`.
    pub async fn post(&self, method: &str, mut body: Value) -> Result<Value, YoutubeError> {
        body["context"] = self.context_json();
        let url = Self::endpoint_url(method);
        let mut req = self
            .http
            .post(&url)
            .header("Content-Type", "application/json")
            .header("X-Goog-Api-Key", INNERTUBE_API_KEY)
            .header("X-YouTube-Client-Name", CLIENT_NAME_ID.to_string())
            .header("X-YouTube-Client-Version", APP_VERSION)
            .json(&body);
        if let Some(vd) = self.visitor_data() {
            req = req.header("X-Goog-Visitor-Id", vd);
        }
        let resp = req.send().await?;
        let status = resp.status();
        let text = resp.text().await.map_err(YoutubeError::from)?;
        if !status.is_success() {
            let kind = match status.as_u16() {
                403 => KIND_ACCESS_RESTRICTED,
                429 => KIND_CAPTCHA,
                _ => KIND_HTTP_ERROR,
            };
            return Err(YoutubeError::new(
                kind,
                format!(
                    "innertube {method} returned HTTP {status}: {}",
                    snippet(&text)
                ),
            )
            .with_status(status.as_u16() as i64));
        }
        let value: Value = serde_json::from_str(&text).map_err(|e| {
            YoutubeError::new(
                crate::errors::KIND_PARSE_ERROR,
                format!("innertube {method}: invalid JSON response: {e}"),
            )
        })?;
        if let Some(vd) = value
            .pointer("/responseContext/visitorData")
            .and_then(|v| v.as_str())
        {
            *self.visitor_data.lock().unwrap() = Some(vd.to_string());
        }
        Ok(value)
    }

    /// GET a server-issued URL as plain text (used for caption-track
    /// URLs handed out in player responses). The request goes through
    /// the same HTTP client — and therefore the same user agent — as
    /// the app uses for these fetches.
    pub async fn fetch_text(&self, url: &str) -> Result<String, YoutubeError> {
        let resp = self.http.get(url).send().await?;
        let status = resp.status();
        let text = resp.text().await.map_err(YoutubeError::from)?;
        if !status.is_success() {
            let kind = match status.as_u16() {
                403 => KIND_ACCESS_RESTRICTED,
                429 => KIND_CAPTCHA,
                _ => KIND_HTTP_ERROR,
            };
            return Err(YoutubeError::new(
                kind,
                format!(
                    "GET caption track returned HTTP {status}: {}",
                    snippet(&text)
                ),
            )
            .with_status(status.as_u16() as i64));
        }
        Ok(text)
    }

    /// Bootstrap the guest session the way the app does on first run:
    /// issue a request and keep the visitorData the server returns.
    /// Uses the lightweight `guide` method; the returned value is the
    /// visitorData now held by the client, if the server issued one.
    pub async fn bootstrap_visitor(&self) -> Result<Option<String>, YoutubeError> {
        if let Some(vd) = self.visitor_data() {
            return Ok(Some(vd));
        }
        let _ = self.post("guide", json!({})).await?;
        Ok(self.visitor_data())
    }
}

fn snippet(text: &str) -> String {
    let mut s: String = text.chars().take(200).collect();
    if text.chars().count() > 200 {
        s.push('…');
    }
    s
}
