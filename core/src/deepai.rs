//! DeepAI Code accounts: sign-in, the headers every request carries, the
//! `/theos` check-in that must precede model calls, the model catalog, and
//! the `session_busy` admission wait.
//!
//! The wire facts follow the shipped DeepAI Code 0.3.37 client; see
//! docs/evidence/deepai-code-provider.md for sources and what remains
//! unverified.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio::sync::{OnceCell, OwnedSemaphorePermit, Semaphore};

use crate::oauth::Credential;
use crate::ErrorInfo;

/// Model API base of the production service.
pub const DEEPAI_BASE: &str = "https://api.deepai.org/deepai_code";

/// Lane version the server bills under; it answers any other value with 426
/// `lane_version_unsupported`, so this follows the DeepAI Code release.
const LANE_VERSION: &str = "6";
/// The DeepAI Code release whose lane this client speaks, reported in the
/// check-in beacon. Updated together with [`LANE_VERSION`].
const APP_VERSION: &str = "0.3.37";

const LANE_HEADER: &str = "X-DeepAI-Code-Theos";
const INSTANCE_HEADER: &str = "X-DeepAI-Code-Instance";
const MACHINE_HEADER: &str = "X-DeepAI-Code-Machine";
const CHECK_IN_HEADER: &str = "X-DeepAI-Code-Check-In";
const BEACON_HEADER: &str = "X-DeepAI-Code-Beacon";

/// A server that answered without a check-in because it predates check-ins
/// is not asked again for this long.
const LEGACY_SERVER_WINDOW: Duration = Duration::from_secs(30);
/// Pause after a check-in failed on the network or with a 5xx while the last
/// check-in is still valid.
const CHECK_IN_RETRY_PAUSE: Duration = Duration::from_secs(5);
/// Window for the beacon's `pi` flag before any check-in told its interval.
const DEFAULT_RELAY_WINDOW: Duration = Duration::from_secs(600);
/// Bound for each check-in request.
const CHECK_IN_TIMEOUT: Duration = Duration::from_secs(30);
/// Bound for reading the machine identifier from the OS.
const MACHINE_ID_TIMEOUT: Duration = Duration::from_secs(10);
/// Bound for the sign-in code exchange.
const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(20);
/// How long the sign-in waits for the browser.
const SIGN_IN_WAIT: Duration = Duration::from_secs(300);
/// Longest accepted sign-in code.
const MAX_CODE_LEN: usize = 4096;

/// Per-process DeepAI state shared by every request of one [`crate::Llm`].
pub(crate) struct Session {
    instance: String,
    started: Instant,
    machine: OnceCell<Option<String>>,
    /// Held across the `/theos` request, so concurrent requests check in once.
    check_in: tokio::sync::Mutex<CheckIn>,
    catalog: Mutex<Option<Vec<CatalogModel>>>,
    /// Metered models run one request at a time.
    metered_slot: Arc<Semaphore>,
}

#[derive(Default)]
struct CheckIn {
    nonce: Option<String>,
    seconds: u64,
    received_at: Option<Instant>,
    legacy_at: Option<Instant>,
    retry_at: Option<Instant>,
    last_relay_at: Option<Instant>,
}

impl CheckIn {
    fn age(&self, now: Instant) -> Option<Duration> {
        self.received_at.map(|at| now.duration_since(at))
    }

    /// A nonce younger than `fraction` of its interval.
    fn fresh_within(&self, now: Instant, divisor: u32) -> bool {
        self.nonce.is_some()
            && self
                .age(now)
                .is_some_and(|age| age < Duration::from_secs(self.seconds) / divisor)
    }

    fn legacy(&self, now: Instant) -> bool {
        self.legacy_at
            .is_some_and(|at| now.duration_since(at) < LEGACY_SERVER_WINDOW)
    }

    /// Records the check-in a `/theos` answer carries, on a refusal too.
    fn note(&mut self, payload: Option<&Value>, now: Instant) {
        let Some(payload) = payload else { return };
        if let Some((nonce, seconds)) = read_check_in(payload) {
            self.nonce = Some(nonce);
            self.seconds = seconds;
            self.received_at = Some(now);
        } else if is_legacy_answer(payload) {
            self.legacy_at = Some(now);
        }
    }

    fn beacon(&self, now: Instant, started: Instant) -> String {
        let window = if self.nonce.is_some() {
            Duration::from_secs(self.seconds)
        } else {
            DEFAULT_RELAY_WINDOW
        };
        let relayed = self
            .last_relay_at
            .is_some_and(|at| now.duration_since(at) < window);
        // `focus` is left out: this client has no window to report on.
        format!(
            "v={APP_VERSION};up={};pi={}",
            now.duration_since(started).as_secs(),
            u8::from(relayed)
        )
    }
}

/// `check_in: {nonce: <32 lowercase hex>, seconds: <positive number>}`.
fn read_check_in(payload: &Value) -> Option<(String, u64)> {
    let check_in = payload.get("check_in")?;
    let nonce = check_in.get("nonce")?.as_str()?;
    let seconds = check_in.get("seconds")?.as_f64()?;
    let valid_nonce = nonce.len() == 32
        && nonce
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    (valid_nonce && seconds > 0.0).then(|| (nonce.to_string(), seconds.ceil() as u64))
}

/// An allowance answer from a server that predates check-ins.
fn is_legacy_answer(payload: &Value) -> bool {
    let Some(enabled) = payload.get("enabled").and_then(Value::as_bool) else {
        return false;
    };
    let lane: u64 = LANE_VERSION.parse().unwrap_or(0);
    !enabled
        || match payload.get("lane_version") {
            None => true,
            Some(version) => version.as_u64().is_some_and(|version| version < lane),
        }
}

/// One entry of the account's model catalog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CatalogModel {
    pub(crate) id: String,
    pub(crate) name: Option<String>,
    /// Input tokens the model accepts (`contextWindow`).
    pub(crate) context_window: Option<u64>,
    responses: bool,
    pub(crate) theos_per_request: Option<String>,
    theos_per_second: Vec<String>,
    premier: bool,
}

impl CatalogModel {
    /// Whether a request to this model spends Theos (reference
    /// `isDeepAIModelMetered`).
    fn metered(&self) -> bool {
        if let Some(rate) = &self.theos_per_request {
            if is_theos_amount(rate) {
                return rate.bytes().any(|b| (b'1'..=b'9').contains(&b));
            }
        } else if !self.theos_per_second.is_empty()
            && self
                .theos_per_second
                .iter()
                .all(|rate| is_theos_amount(rate))
        {
            return self
                .theos_per_second
                .iter()
                .any(|rate| rate.bytes().any(|b| (b'1'..=b'9').contains(&b)));
        }
        self.premier
    }
}

/// `^\d+(\.\d+)?$`
fn is_theos_amount(value: &str) -> bool {
    let (whole, fraction) = match value.split_once('.') {
        Some((whole, fraction)) => (whole, Some(fraction)),
        None => (value, None),
    };
    !whole.is_empty()
        && whole.bytes().all(|b| b.is_ascii_digit())
        && fraction.is_none_or(|f| !f.is_empty() && f.bytes().all(|b| b.is_ascii_digit()))
}

/// Reads `{models: [...]}`; entries without a usable id are skipped.
pub(crate) fn parse_catalog(value: &Value) -> Result<Vec<CatalogModel>, ErrorInfo> {
    let list = value
        .get("models")
        .and_then(Value::as_array)
        .ok_or_else(|| ErrorInfo::new("provider_protocol", "DeepAI returned no model catalog"))?;
    let models: Vec<CatalogModel> = list
        .iter()
        .filter_map(|item| {
            let id = item.get("id")?.as_str()?;
            let usable = !id.is_empty()
                && id.len() <= 128
                && id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'.' || b == b'-');
            usable.then(|| CatalogModel {
                id: id.to_string(),
                name: item.get("name").and_then(Value::as_str).map(str::to_string),
                context_window: item.get("contextWindow").and_then(Value::as_u64),
                responses: item.get("api").and_then(Value::as_str) == Some("openai-responses"),
                theos_per_request: item
                    .get("theosPerRequest")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                theos_per_second: item
                    .get("theosPerSecond")
                    .and_then(Value::as_object)
                    .map(|rates| {
                        rates
                            .values()
                            .filter_map(|rate| rate.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default(),
                premier: item.get("premier").and_then(Value::as_bool) == Some(true),
            })
        })
        .collect();
    if models.is_empty() {
        return Err(ErrorInfo::new(
            "provider_protocol",
            "DeepAI returned an empty model catalog",
        ));
    }
    Ok(models)
}

/// How a chat request to one model is sent.
pub(crate) struct Route {
    pub(crate) responses: bool,
    pub(crate) metered: bool,
}

/// The daily Theos allowance of a DeepAI account. Amounts keep the decimal
/// strings the server sends.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct TheosAllowance {
    /// Whether the account receives Theos at all.
    pub enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub balance: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub granted: Option<String>,
    /// When the allowance is refilled, as the server writes it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refresh_at: Option<String>,
}

impl TheosAllowance {
    /// Reads `{enabled, balance?, granted?, refresh_at?}`; amounts may come
    /// as numbers or numeric strings (reference `allowanceAmount`).
    fn from_json(value: &Value) -> Option<Self> {
        let amount = |name: &str| match value.get(name)? {
            Value::Number(number) => Some(number.to_string()),
            Value::String(text) => {
                let text = text.trim();
                let digits = text.strip_prefix('-').unwrap_or(text);
                is_theos_amount(digits).then(|| text.to_string())
            }
            _ => None,
        };
        Some(Self {
            enabled: value.get("enabled")?.as_bool()?,
            balance: amount("balance"),
            granted: amount("granted"),
            refresh_at: value
                .get("refresh_at")
                .and_then(Value::as_str)
                .map(str::to_string),
        })
    }
}

/// Outcome of a `/theos` request.
enum TheosAnswer {
    Ok(Value),
    Refused { status: u16, body: Option<Value> },
    Unreachable,
}

impl Session {
    pub(crate) fn new() -> Self {
        let mut instance = [0u8; 16];
        getrandom::getrandom(&mut instance)
            .map_err(|e| e.to_string())
            .expect("OS CSPRNG");
        Self {
            instance: hex(&instance),
            started: Instant::now(),
            machine: OnceCell::new(),
            check_in: tokio::sync::Mutex::new(CheckIn::default()),
            catalog: Mutex::new(None),
            metered_slot: Arc::new(Semaphore::new(1)),
        }
    }

    /// Adds the lane, instance, machine and key headers every DeepAI request
    /// carries.
    pub(crate) async fn headers(
        &self,
        request: reqwest::RequestBuilder,
        api_key: &str,
    ) -> reqwest::RequestBuilder {
        let mut request = request
            .header(LANE_HEADER, LANE_VERSION)
            .header(INSTANCE_HEADER, &self.instance)
            .header("api-key", api_key);
        if let Some(machine) = self.machine.get_or_init(machine_id).await {
            request = request.header(MACHINE_HEADER, machine);
        }
        request
    }

    /// The catalog of the signed-in account. `refresh` fetches it again;
    /// otherwise the session's copy is reused once fetched.
    pub(crate) async fn catalog(
        &self,
        client: &reqwest::Client,
        base: &str,
        api_key: &str,
        refresh: bool,
    ) -> Result<Vec<CatalogModel>, ErrorInfo> {
        if !refresh {
            if let Some(catalog) = self.catalog.lock().expect("catalog lock").clone() {
                return Ok(catalog);
            }
        }
        let request = client
            .get(format!("{}/models", base.trim_end_matches('/')))
            .timeout(Duration::from_secs(30))
            .header("accept", "application/json")
            .header("content-type", "application/json");
        let response = self
            .headers(request, api_key)
            .await
            .send()
            .await
            .map_err(|error| ErrorInfo::new("provider_http", format!("request failed: {error}")))?;
        let response = require_lane(response).await?;
        let body = crate::llm::read_bounded_body(response).await?;
        let body: Value = serde_json::from_str(&body).map_err(|error| {
            ErrorInfo::new(
                "provider_protocol",
                format!("invalid DeepAI model catalog: {error}"),
            )
        })?;
        let catalog = parse_catalog(&body)?;
        *self.catalog.lock().expect("catalog lock") = Some(catalog.clone());
        Ok(catalog)
    }

    /// Format and metering of `model`; a model missing from the catalog is
    /// sent as Chat Completions and counted as metered, like the reference.
    pub(crate) async fn route(
        &self,
        client: &reqwest::Client,
        base: &str,
        api_key: &str,
        model: &str,
    ) -> Result<Route, ErrorInfo> {
        let catalog = self.catalog(client, base, api_key, false).await?;
        Ok(match catalog.iter().find(|entry| entry.id == model) {
            Some(entry) => Route {
                responses: entry.responses,
                metered: entry.metered(),
            },
            None => Route {
                responses: false,
                metered: true,
            },
        })
    }

    /// Waits for the single slot of metered requests.
    pub(crate) async fn metered_slot(&self) -> OwnedSemaphorePermit {
        Arc::clone(&self.metered_slot)
            .acquire_owned()
            .await
            .expect("the slot semaphore is never closed")
    }

    /// Renews the check-in before a model call (reference `ensureCheckedIn`).
    pub(crate) async fn ensure_checked_in(
        &self,
        client: &reqwest::Client,
        base: &str,
        api_key: &str,
    ) -> Result<(), ErrorInfo> {
        let mut state = self.check_in.lock().await;
        let now = Instant::now();
        if state.fresh_within(now, 2) || state.legacy(now) {
            return self.relayed(&mut state);
        }
        if state.fresh_within(now, 1) && state.retry_at.is_some_and(|at| now < at) {
            return self.relayed(&mut state);
        }
        let answer = self.request_theos(&state, client, base, api_key).await;
        let now = Instant::now();
        match answer {
            TheosAnswer::Ok(body) => {
                state.note(Some(&body), now);
                if state.fresh_within(now, 1) || state.legacy(now) {
                    self.relayed(&mut state)
                } else {
                    Err(check_in_unavailable())
                }
            }
            TheosAnswer::Refused { status, body } => {
                state.retry_at = None;
                state.note(body.as_ref(), now);
                let carried = body.as_ref().and_then(read_check_in).is_some();
                if state.fresh_within(now, 1) && (status >= 500 || carried) {
                    if status >= 500 {
                        state.retry_at = Some(now + CHECK_IN_RETRY_PAUSE);
                    }
                    return self.relayed(&mut state);
                }
                Err(refusal(status, body.as_ref()))
            }
            TheosAnswer::Unreachable => {
                if state.fresh_within(now, 1) {
                    state.retry_at = Some(now + CHECK_IN_RETRY_PAUSE);
                    return self.relayed(&mut state);
                }
                Err(check_in_unavailable())
            }
        }
    }

    /// Sends `GET /theos` with the check-in and beacon headers. The caller
    /// holds the check-in lock, so the nonce sent is the latest one.
    async fn request_theos(
        &self,
        state: &CheckIn,
        client: &reqwest::Client,
        base: &str,
        api_key: &str,
    ) -> TheosAnswer {
        let mut request = client
            .get(format!("{}/theos", base.trim_end_matches('/')))
            .timeout(CHECK_IN_TIMEOUT)
            .header("content-type", "application/json")
            .header(BEACON_HEADER, state.beacon(Instant::now(), self.started));
        if let Some(nonce) = &state.nonce {
            request = request.header(CHECK_IN_HEADER, nonce);
        }
        let mut response = match self.headers(request, api_key).await.send().await {
            Ok(response) => response,
            Err(_) => return TheosAnswer::Unreachable,
        };
        let status = response.status();
        let body = crate::llm::read_capped(&mut response, crate::oauth::SUCCESS_BODY_MAX_BYTES)
            .await
            .ok()
            .flatten()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
        match (status.is_success(), body) {
            (true, Some(body)) => TheosAnswer::Ok(body),
            (true, None) => TheosAnswer::Unreachable,
            (false, body) => TheosAnswer::Refused {
                status: status.as_u16(),
                body,
            },
        }
    }

    /// The account's Theos allowance. The answer is also a check-in, so it
    /// goes through the same lock and records the nonce it carries.
    pub(crate) async fn theos(
        &self,
        client: &reqwest::Client,
        base: &str,
        api_key: &str,
    ) -> Result<TheosAllowance, ErrorInfo> {
        let mut state = self.check_in.lock().await;
        let answer = self.request_theos(&state, client, base, api_key).await;
        let now = Instant::now();
        match answer {
            TheosAnswer::Ok(body) => {
                state.note(Some(&body), now);
                TheosAllowance::from_json(&body).ok_or_else(|| {
                    ErrorInfo::new(
                        "provider_protocol",
                        "DeepAI returned an invalid Theos allowance",
                    )
                })
            }
            TheosAnswer::Refused { status, body } => {
                state.retry_at = None;
                state.note(body.as_ref(), now);
                Err(match body.as_ref().and_then(|body| body.get("error")) {
                    Some(error) => ErrorInfo::new(
                        "provider_http",
                        format!(
                            "DeepAI refused the Theos request (HTTP {status}, {}): {}",
                            error
                                .get("code")
                                .and_then(Value::as_str)
                                .unwrap_or("unknown"),
                            error
                                .get("message")
                                .and_then(Value::as_str)
                                .unwrap_or("no message")
                        ),
                    ),
                    None => ErrorInfo::new(
                        "provider_http",
                        format!("DeepAI returned HTTP {status} for the Theos request"),
                    ),
                })
            }
            TheosAnswer::Unreachable => Err(ErrorInfo::new(
                "provider_http",
                "could not read the Theos allowance from DeepAI",
            )),
        }
    }

    /// Marks that a model call is about to be relayed, for the beacon.
    fn relayed(&self, state: &mut CheckIn) -> Result<(), ErrorInfo> {
        state.last_relay_at = Some(Instant::now());
        Ok(())
    }
}

/// A check-in refusal: a 4xx that names its error is reported as the
/// server's error; anything else is the reference's generic retry message.
fn refusal(status: u16, body: Option<&Value>) -> ErrorInfo {
    match body.and_then(|body| body.get("error")) {
        Some(error) if (400..500).contains(&status) => ErrorInfo::new(
            "provider_http",
            format!(
                "DeepAI refused the check-in (HTTP {status}, {}): {}",
                error
                    .get("code")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown"),
                error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("no message")
            ),
        ),
        _ => check_in_unavailable(),
    }
}

fn check_in_unavailable() -> ErrorInfo {
    ErrorInfo::new(
        "check_in_unavailable",
        "DeepAI could not renew this app's connection. Please retry the turn.",
    )
}

/// Turns 426 into an explanation: the server moved to a newer lane than this
/// client speaks, so inocode must follow the DeepAI Code release.
pub(crate) async fn require_lane(
    response: reqwest::Response,
) -> Result<reqwest::Response, ErrorInfo> {
    if response.status().as_u16() == 426 {
        return Err(ErrorInfo::new(
            "provider_http",
            format!(
                "DeepAI no longer accepts lane {LANE_VERSION} (DeepAI Code {APP_VERSION}); \
                 inocode must be updated to the newer DeepAI Code release (HTTP 426)"
            ),
        ));
    }
    crate::llm::require_success(response).await
}

/// What a model-call response means for the admission wait.
pub(crate) enum Admission {
    /// Not a 409: hand the response on.
    Answered(reqwest::Response),
    /// `session_busy`: resend after this delay.
    Busy(Duration),
    /// Any other 409.
    Refused(ErrorInfo),
}

/// Classifies a model-call response; only the explicit `session_busy` 409 is
/// waited out (reference `waitForDeepAICapacity`), with `Retry-After` clamped
/// to 1–30 seconds and 2 seconds when absent.
pub(crate) async fn admission(mut response: reqwest::Response) -> Admission {
    if response.status().as_u16() != 409 {
        return Admission::Answered(response);
    }
    let retry_after = response
        .headers()
        .get("retry-after")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<f64>().ok());
    let bytes = crate::llm::read_capped(&mut response, crate::llm::ERROR_BODY_MAX_BYTES)
        .await
        .ok()
        .flatten()
        .unwrap_or_default();
    let body = serde_json::from_slice::<Value>(&bytes).ok();
    let code = body
        .as_ref()
        .and_then(|body| body.get("error"))
        .and_then(|error| error.get("code"))
        .and_then(Value::as_str);
    if code != Some("session_busy") {
        return Admission::Refused(ErrorInfo::new(
            "provider_http",
            format!(
                "provider returned HTTP 409: {}",
                String::from_utf8_lossy(&bytes).trim()
            ),
        ));
    }
    let seconds = retry_after.filter(|value| value.is_finite()).unwrap_or(2.0);
    Admission::Busy(Duration::from_secs_f64(seconds.clamp(1.0, 30.0)))
}

/// Sign-in endpoints; tests point them at a mock server.
pub struct SignInEndpoints {
    pub base: String,
}

impl Default for SignInEndpoints {
    fn default() -> Self {
        Self {
            base: DEEPAI_BASE.to_string(),
        }
    }
}

/// Everything the browser sign-in needs before the user approves.
pub struct SignInStart {
    /// The URL to open in a browser.
    pub url: String,
    state: String,
    port: u16,
}

/// Builds the sign-in URL with a fresh 32-byte state and a loopback port.
pub fn sign_in_start(endpoints: &SignInEndpoints) -> SignInStart {
    let mut state = [0u8; 32];
    getrandom::getrandom(&mut state)
        .map_err(|e| e.to_string())
        .expect("OS CSPRNG");
    let state = hex(&state);
    let port = crate::oauth::reserve_callback_port(0);
    SignInStart {
        url: format!(
            "{}/desktop_auth/?state={state}&port={port}",
            endpoints.base.trim_end_matches('/')
        ),
        state,
        port,
    }
}

/// Waits for the browser to return to `127.0.0.1:<port>/auth-callback` and
/// exchanges the code for the account's API key.
pub async fn sign_in_finish(
    endpoints: &SignInEndpoints,
    start: SignInStart,
) -> Result<Credential, String> {
    let callback = crate::oauth::await_callback(start.port, "/auth-callback", SIGN_IN_WAIT).await?;
    if !same_secret(&callback.state, &start.state) {
        return Err("state mismatch - possible CSRF attack".to_string());
    }
    if callback.code.is_empty() || callback.code.len() > MAX_CODE_LEN {
        return Err("the sign-in callback carried no usable code".to_string());
    }
    exchange_code(endpoints, &callback.code).await
}

/// `POST {base}/desktop_auth/exchange/ {code}` → `{api_key, username, is_pro}`.
pub async fn exchange_code(endpoints: &SignInEndpoints, code: &str) -> Result<Credential, String> {
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(EXCHANGE_TIMEOUT)
        .build()
        .map_err(|e| format!("cannot build the sign-in HTTP client: {e}"))?;
    let response = client
        .post(format!(
            "{}/desktop_auth/exchange/",
            endpoints.base.trim_end_matches('/')
        ))
        .header("content-type", "application/json")
        .json(&json!({ "code": code }))
        .send()
        .await
        .map_err(|e| format!("could not reach DeepAI to finish signing in: {e}"))?;
    match response.status().as_u16() {
        200..=299 => {}
        400 | 401 => {
            return Err(
                "this sign-in link expired or was already used; start the login again".to_string(),
            )
        }
        429 => {
            return Err(
                "too many sign-in attempts; wait a moment and start the login again".to_string(),
            )
        }
        status => {
            return Err(format!(
                "the DeepAI sign-in exchange returned HTTP {status}"
            ))
        }
    }
    let bytes = crate::oauth::read_success_body(response, "DeepAI sign-in response").await?;
    let payload: Value = serde_json::from_slice(&bytes)
        .map_err(|_| "the DeepAI sign-in response was not valid JSON".to_string())?;
    let api_key = payload
        .get("api_key")
        .and_then(Value::as_str)
        .filter(|key| !key.is_empty() && !key.contains(['\r', '\n']))
        .ok_or("the DeepAI sign-in response did not contain a usable api_key")?;
    Ok(Credential {
        access: String::new(),
        refresh: String::new(),
        expires_at_ms: None,
        account_id: None,
        email: None,
        api_key: Some(api_key.to_string()),
        oauth_access_token: None,
    })
}

/// Constant-time comparison for the sign-in state.
fn same_secret(left: &str, right: &str) -> bool {
    left.len() == right.len()
        && left
            .bytes()
            .zip(right.bytes())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// SHA-256 of the machine identifier, or `None` when none is readable; the
/// raw identifier never leaves the machine.
async fn machine_id() -> Option<String> {
    let raw = tokio::time::timeout(
        MACHINE_ID_TIMEOUT,
        tokio::task::spawn_blocking(read_machine_id),
    )
    .await
    .ok()?
    .ok()??;
    use sha2::{Digest, Sha256};
    Some(hex(&Sha256::digest(raw.as_bytes())))
}

fn command_output(program: &str, args: &[&str]) -> String {
    std::process::Command::new(program)
        .args(args)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
        .unwrap_or_default()
}

fn first_uuid(text: &str) -> Option<String> {
    text.split(|c: char| !(c.is_ascii_hexdigit() || c == '-'))
        .find(|token| is_uuid(token))
        .map(str::to_ascii_lowercase)
}

fn is_uuid(token: &str) -> bool {
    let groups: Vec<&str> = token.split('-').collect();
    groups.len() == 5
        && groups
            .iter()
            .zip([8, 4, 4, 4, 12])
            .all(|(group, len)| group.len() == len && group.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// Placeholder SMBIOS UUIDs that firmware ships instead of a real one.
const SMBIOS_PLACEHOLDERS: [&str; 3] = [
    "00000000-0000-0000-0000-000000000000",
    "ffffffff-ffff-ffff-ffff-ffffffffffff",
    "03000200-0400-0500-0006-000700080009",
];

fn read_machine_id() -> Option<String> {
    if cfg!(target_os = "macos") {
        let ioreg = command_output("ioreg", &["-rd1", "-c", "IOPlatformExpertDevice"]);
        return ioreg
            .lines()
            .find(|line| line.contains("\"IOPlatformUUID\""))
            .and_then(first_uuid);
    }
    if cfg!(windows) {
        let smbios = command_output(
            "powershell",
            &[
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "(Get-CimInstance Win32_ComputerSystemProduct).UUID",
            ],
        );
        if let Some(uuid) =
            first_uuid(&smbios).filter(|uuid| !SMBIOS_PLACEHOLDERS.contains(&uuid.as_str()))
        {
            return Some(uuid);
        }
        let query = command_output(
            "reg",
            &[
                "query",
                r"HKLM\SOFTWARE\Microsoft\Cryptography",
                "/v",
                "MachineGuid",
            ],
        );
        return query
            .lines()
            .find(|line| line.contains("MachineGuid") && line.contains("REG_SZ"))
            .and_then(first_uuid);
    }
    if cfg!(target_os = "linux") {
        for path in ["/etc/machine-id", "/var/lib/dbus/machine-id"] {
            let id = std::fs::read_to_string(path)
                .unwrap_or_default()
                .trim()
                .to_ascii_lowercase();
            if id.len() == 32
                && id.bytes().all(|b| b.is_ascii_hexdigit())
                && id.bytes().any(|b| b != b'0')
            {
                return Some(id);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_in_reads_only_valid_nonces() {
        let nonce = "0123456789abcdef0123456789abcdef";
        assert_eq!(
            read_check_in(&json!({"check_in": {"nonce": nonce, "seconds": 120}})),
            Some((nonce.to_string(), 120))
        );
        assert!(read_check_in(&json!({"check_in": {"nonce": "ABCDEF", "seconds": 120}})).is_none());
        assert!(read_check_in(&json!({"check_in": {"nonce": nonce, "seconds": 0}})).is_none());
        assert!(read_check_in(&json!({"enabled": true})).is_none());
    }

    #[test]
    fn legacy_answers_are_recognised() {
        assert!(is_legacy_answer(
            &json!({"enabled": false, "lane_version": 6})
        ));
        assert!(is_legacy_answer(&json!({"enabled": true})));
        assert!(is_legacy_answer(
            &json!({"enabled": true, "lane_version": 5})
        ));
        assert!(!is_legacy_answer(
            &json!({"enabled": true, "lane_version": 6})
        ));
        assert!(!is_legacy_answer(&json!({"balance": "1"})));
    }

    #[test]
    fn beacon_reports_version_uptime_and_relay() {
        let started = Instant::now();
        let mut state = CheckIn::default();
        let now = started + Duration::from_secs(42);
        assert_eq!(state.beacon(now, started), "v=0.3.37;up=42;pi=0");
        state.last_relay_at = Some(now - Duration::from_secs(10));
        assert_eq!(state.beacon(now, started), "v=0.3.37;up=42;pi=1");
    }

    #[test]
    fn catalog_metering_follows_the_reference_rules() {
        let catalog = parse_catalog(&json!({"models": [
            {"id": "free", "name": "Free", "theosPerRequest": "0", "premier": true},
            {"id": "paid", "name": "Paid", "theosPerRequest": "3"},
            {"id": "per-second", "theosPerSecond": {"low": "0", "high": "0.5"}},
            {"id": "premier-only", "premier": true},
            {"id": "astra", "api": "openai-responses", "theosPerRequest": "50"},
            {"id": "bad id!"}
        ]}))
        .expect("catalog");
        let metered: Vec<(&str, bool)> = catalog
            .iter()
            .map(|m| (m.id.as_str(), m.metered()))
            .collect();
        assert_eq!(
            metered,
            [
                ("free", false),
                ("paid", true),
                ("per-second", true),
                ("premier-only", true),
                ("astra", true)
            ]
        );
        assert!(
            catalog
                .iter()
                .find(|m| m.id == "astra")
                .expect("astra")
                .responses
        );
        assert!(parse_catalog(&json!({"models": []})).is_err());
    }

    #[test]
    fn allowance_reads_numbers_and_numeric_strings() {
        assert_eq!(
            TheosAllowance::from_json(&json!({
                "enabled": true, "balance": "150.5", "granted": 200, "refresh_at": "2026-11-17T00:00:00Z"
            })),
            Some(TheosAllowance {
                enabled: true,
                balance: Some("150.5".to_string()),
                granted: Some("200".to_string()),
                refresh_at: Some("2026-11-17T00:00:00Z".to_string()),
            })
        );
        let disabled = TheosAllowance::from_json(&json!({"enabled": false, "balance": "x"}))
            .expect("disabled");
        assert!(!disabled.enabled);
        assert!(disabled.balance.is_none());
        assert!(TheosAllowance::from_json(&json!({"balance": "1"})).is_none());
    }

    #[test]
    fn uuids_and_secrets_are_parsed_strictly() {
        assert_eq!(
            first_uuid("UUID\r\n4C4C4544-0042-3010-8051-B4C04F4E3732\r\n").as_deref(),
            Some("4c4c4544-0042-3010-8051-b4c04f4e3732")
        );
        assert!(first_uuid("no uuid here").is_none());
        assert!(same_secret("abc", "abc"));
        assert!(!same_secret("abc", "abd"));
        assert!(!same_secret("abc", "abcd"));
    }
}
