//! `settings.json` beside the database: the one place for configuration
//! (docs/evidence/account-pool.md). Each section has its own reader; the
//! file is written whole, through a temporary file renamed over it, so a
//! crash never leaves half a file, and every write bumps [`written`] so a
//! running turn sees the change.
//!
//! Secrets are not kept here: a provider that needs a static API key finds
//! it in the credential store under [`api_key_name`].

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::oauth::AuthKind;
use crate::reasoning::{Effort, Summary};
use crate::ProviderKind;

static WRITTEN: AtomicU64 = AtomicU64::new(0);

/// Held from the read to the rename of every write of this process, so two
/// edits made at once cannot both start from the same file.
static WRITING: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// The settings files this process read or wrote (a server has one), whose
/// times of change [`written`] includes; the oldest go past [`WATCHED_MAX`].
static WATCHED: std::sync::Mutex<Vec<std::path::PathBuf>> = std::sync::Mutex::new(Vec::new());
const WATCHED_MAX: usize = 64;

/// How long a write waits for another process's write of the same file.
const LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// A stamp that changes whenever the settings are written, by this process
/// or by another one (a command line `login` or `key set` beside a running
/// server): compared with an earlier stamp, it tells whether to read them
/// again.
pub(crate) fn written() -> u64 {
    let watched = WATCHED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    let modified = watched.iter().fold(0u64, |stamp, path| {
        let nanos = std::fs::metadata(path)
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |since| since.as_nanos() as u64);
        stamp.rotate_left(13) ^ nanos
    });
    WRITTEN
        .load(Ordering::SeqCst)
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        ^ modified
}

fn watch(path: &Path) {
    let mut watched = WATCHED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if !watched.iter().any(|kept| kept == path) {
        if watched.len() >= WATCHED_MAX {
            watched.remove(0);
        }
        watched.push(path.to_path_buf());
    }
}

/// Holds the lock that orders the writes of `path` across processes.
async fn lock_across_processes(path: &Path) -> Result<std::fs::File, String> {
    let lock = crate::secrets::open_lock_file(&crate::secrets::sibling(path, ".lock"))?;
    let deadline = tokio::time::Instant::now() + LOCK_WAIT;
    loop {
        match lock.try_lock() {
            Ok(()) => return Ok(lock),
            Err(std::fs::TryLockError::WouldBlock) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err(format!(
                    "another program kept {} locked for {} seconds",
                    path.display(),
                    LOCK_WAIT.as_secs()
                ))
            }
            Err(std::fs::TryLockError::Error(error)) => {
                return Err(format!("cannot lock {}: {error}", path.display()))
            }
        }
    }
}

/// The whole file as JSON; an empty object when it is missing.
pub(crate) async fn read(path: &Path) -> Result<Value, String> {
    watch(path);
    match tokio::fs::read_to_string(path).await {
        Ok(text) => parse(&text),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(json!({})),
        Err(error) => Err(error.to_string()),
    }
}

/// [`read`] for callers outside a runtime, such as the server's start.
pub fn read_blocking(path: &Path) -> Result<Value, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => parse(&text),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(json!({})),
        Err(error) => Err(error.to_string()),
    }
}

fn parse(text: &str) -> Result<Value, String> {
    let value: Value = serde_json::from_str(text).map_err(|error| error.to_string())?;
    if !value.is_object() {
        return Err("the settings file is not an object".into());
    }
    Ok(value)
}

/// Applies `edit` to the file at `path` and writes the result, keeping the
/// sections `edit` does not touch as they are. Nothing is written when
/// `edit` fails.
pub(crate) async fn update<T>(
    path: &Path,
    edit: impl FnOnce(&mut Map<String, Value>) -> Result<T, String>,
) -> Result<T, String> {
    let _writing = WRITING.lock().await;
    if let Some(folder) = path.parent() {
        tokio::fs::create_dir_all(folder)
            .await
            .map_err(|error| error.to_string())?;
    }
    let _locked = lock_across_processes(path).await?;
    let mut value = read(path).await?;
    let file = value.as_object_mut().expect("read gives an object");
    let result = edit(file)?;
    let text = serde_json::to_string_pretty(&value).map_err(|error| error.to_string())?;
    if let Some(folder) = path.parent() {
        tokio::fs::create_dir_all(folder)
            .await
            .map_err(|error| error.to_string())?;
    }
    // Named after the process, so a command line tool writing beside a
    // running server does not write into the server's temporary file.
    let temporary = path.with_extension(format!("json.{}.tmp", std::process::id()));
    tokio::fs::write(&temporary, text)
        .await
        .map_err(|error| error.to_string())?;
    tokio::fs::rename(&temporary, path)
        .await
        .map_err(|error| error.to_string())?;
    WRITTEN.fetch_add(1, Ordering::SeqCst);
    Ok(result)
}

/// The kinds of provider: the API format, how requests are authenticated,
/// and the endpoint and model used when the settings name none.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Kind {
    #[serde(rename = "openai")]
    OpenAi,
    #[serde(rename = "anthropic")]
    Anthropic,
    #[serde(rename = "responses")]
    Responses,
    #[serde(rename = "claude-oauth")]
    ClaudeOauth,
    #[serde(rename = "gpt-oauth")]
    GptOauth,
    #[serde(rename = "muse-code")]
    MuseCode,
    #[serde(rename = "deepai")]
    DeepAi,
}

impl Kind {
    pub const ALL: [Kind; 7] = [
        Kind::OpenAi,
        Kind::Anthropic,
        Kind::Responses,
        Kind::ClaudeOauth,
        Kind::GptOauth,
        Kind::MuseCode,
        Kind::DeepAi,
    ];

    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|kind| kind.name().eq_ignore_ascii_case(name.trim()))
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::OpenAi => "openai",
            Self::Anthropic => "anthropic",
            Self::Responses => "responses",
            Self::ClaudeOauth => "claude-oauth",
            Self::GptOauth => "gpt-oauth",
            Self::MuseCode => "muse-code",
            Self::DeepAi => "deepai",
        }
    }

    pub fn auth(self) -> AuthKind {
        match self {
            Self::OpenAi | Self::Anthropic | Self::Responses => AuthKind::ApiKey,
            Self::ClaudeOauth => AuthKind::ClaudeOauth,
            Self::GptOauth => AuthKind::GptOauth,
            Self::MuseCode => AuthKind::MuseSubscription,
            Self::DeepAi => AuthKind::DeepAi,
        }
    }

    /// Whether the provider's key is one the person stores with
    /// `inocode-server key set`, rather than a sign-in.
    pub fn uses_api_key(self) -> bool {
        self.auth() == AuthKind::ApiKey
    }

    /// The API format. DeepAI chooses it per model from the account catalog.
    pub fn format(self) -> ProviderKind {
        match self {
            Self::OpenAi | Self::DeepAi => ProviderKind::OpenAi,
            Self::Anthropic | Self::ClaudeOauth => ProviderKind::Anthropic,
            Self::Responses | Self::GptOauth | Self::MuseCode => ProviderKind::Responses,
        }
    }

    /// Values follow the reference registry
    /// (docs/evidence/oauth-providers.md); the static-key kinds keep the
    /// Logfare endpoint the server always defaulted to.
    pub fn default_base_url(self) -> &'static str {
        match self {
            Self::OpenAi | Self::Anthropic | Self::Responses => crate::DEFAULT_BASE_URL,
            Self::ClaudeOauth => "https://api.anthropic.com",
            Self::GptOauth => "https://chatgpt.com/backend-api/codex",
            Self::MuseCode => "https://api.meta.ai/v1",
            Self::DeepAi => crate::deepai::DEEPAI_BASE,
        }
    }

    pub fn default_model(self) -> &'static str {
        match self {
            Self::OpenAi | Self::Anthropic | Self::Responses => crate::DEFAULT_MODEL,
            Self::ClaudeOauth => "claude-sonnet-5",
            Self::GptOauth => "gpt-5.2-codex",
            Self::MuseCode => "muse-spark-1.3",
            Self::DeepAi => "glm-5.3-flash",
        }
    }
}

/// The credential store entry holding the API key of the provider `id`.
pub fn api_key_name(id: &str) -> String {
    format!("api-key/{id}")
}

/// The thinking depth and summary a provider's models get unless the
/// conversation chooses.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReasoningDefault {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

impl ReasoningDefault {
    fn is_empty(&self) -> bool {
        self.effort.is_none() && self.summary.is_none()
    }
}

/// Translation of what is sent, for models that only speak `model_language`
/// (docs/evidence/translation.md).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutgoingTranslation {
    #[serde(default)]
    pub outgoing: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_language: Option<String>,
}

/// One entry of `providers`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Provider {
    pub kind: Kind,
    /// The kind's endpoint when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// The kind's model when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
    /// The models offered; fetched from the provider when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub models: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "ReasoningDefault::is_empty")]
    pub reasoning: ReasoningDefault,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub translation: Option<OutgoingTranslation>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub disabled: bool,
    /// Left out of the model lists shown for choosing; still usable.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub hidden: bool,
    /// Models of this provider left out of those lists; still usable.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hidden_models: Vec<String>,
    /// The `client_version` the ChatGPT models endpoint is asked as.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_version: Option<String>,
}

impl Provider {
    pub fn new(kind: Kind) -> Self {
        Self {
            kind,
            base_url: None,
            default_model: None,
            models: None,
            reasoning: ReasoningDefault::default(),
            translation: None,
            disabled: false,
            hidden: false,
            hidden_models: Vec::new(),
            client_version: None,
        }
    }

    pub fn base_url(&self) -> &str {
        self.base_url
            .as_deref()
            .unwrap_or_else(|| self.kind.default_base_url())
    }

    pub fn default_model(&self) -> &str {
        self.default_model
            .as_deref()
            .unwrap_or_else(|| self.kind.default_model())
    }

    fn check(&self, id: &str) -> Result<(), String> {
        if let Some(url) = &self.base_url {
            check_url(url).map_err(|error| format!("provider {id}: {error}"))?;
        }
        if let Some(model) = &self.default_model {
            if model.trim().is_empty() {
                return Err(format!("provider {id}: \"default_model\" is empty"));
            }
        }
        if let Some(models) = &self.models {
            if models.iter().any(|model| model.trim().is_empty()) {
                return Err(format!("provider {id}: \"models\" has an empty name"));
            }
        }
        check_effort(self.reasoning.effort.as_deref())
            .and_then(|()| check_summary(self.reasoning.summary.as_deref()))
            .map_err(|error| format!("provider {id}: {error}"))?;
        if let Some(language) = self
            .translation
            .as_ref()
            .and_then(|t| t.model_language.as_ref())
        {
            if language.trim().is_empty() {
                return Err(format!(
                    "provider {id}: \"translation.model_language\" is empty"
                ));
            }
        }
        Ok(())
    }
}

/// A provider, model and reasoning choice: the `default` section, and what
/// a conversation chose. An absent field leaves the choice to the next
/// level (the conversation's to `default`, `default`'s to the provider).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Selection {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

/// The model of one helper role. Without `provider`, the conversation's
/// provider; without `model`, the conversation's model when the provider
/// is the conversation's, else the provider's default.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Role {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
}

/// Helper roles; an absent one uses the conversation's model.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Roles {
    /// Reviews calls in the `auto` approval mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review: Option<Role>,
    /// Writes compaction summaries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compact: Option<Role>,
    /// Answers `explore`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub explore: Option<Role>,
    /// Checks goals; the colleague, then the conversation's model, when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check: Option<Role>,
    /// Advises when the work looks stuck; none when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub colleague: Option<Role>,
}

impl Roles {
    fn each(&self) -> [(&'static str, Option<&Role>); 5] {
        [
            ("review", self.review.as_ref()),
            ("compact", self.compact.as_ref()),
            ("explore", self.explore.as_ref()),
            ("check", self.check.as_ref()),
            ("colleague", self.colleague.as_ref()),
        ]
    }
}

/// The server process's own settings.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Server {
    /// The loopback port; 8787 when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    /// Whether shell commands are confined; on when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<bool>,
    /// Where credentials are kept: `auto` (the default), `keyring`,
    /// `encrypted-file` or `plaintext` (docs/evidence/account-pool.md).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credentials: Option<String>,
    /// Seconds a model's answer may send nothing before the request fails;
    /// [`DEFAULT_STREAM_IDLE_SECONDS`] when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_idle_seconds: Option<u64>,
}

/// The loopback port when the settings and the command line name none.
pub const DEFAULT_PORT: u16 = 8787;

/// How long a model's answer may stay silent by default. Providers send
/// pings or progress much more often while they work, so a longer silence
/// is a stalled connection.
pub const DEFAULT_STREAM_IDLE_SECONDS: u64 = 300;

impl Server {
    pub fn stream_idle(&self) -> std::time::Duration {
        std::time::Duration::from_secs(
            self.stream_idle_seconds
                .unwrap_or(DEFAULT_STREAM_IDLE_SECONDS),
        )
    }

    /// The credential store mode the settings choose.
    pub fn credential_store(&self) -> Result<crate::secrets::StoreMode, String> {
        self.credentials.as_deref().map_or(
            Ok(crate::secrets::StoreMode::Auto),
            crate::secrets::StoreMode::parse,
        )
    }
}

/// The sections of the file this module owns.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Settings {
    /// `None` until the file first gets the section.
    pub providers: Option<BTreeMap<String, Provider>>,
    pub default: Selection,
    pub roles: Roles,
    pub server: Server,
}

fn section<T: for<'de> Deserialize<'de> + Default>(
    file: &Map<String, Value>,
    name: &str,
) -> Result<T, String> {
    match file.get(name) {
        None | Some(Value::Null) => Ok(T::default()),
        Some(value) => {
            serde_json::from_value(value.clone()).map_err(|error| format!("\"{name}\": {error}"))
        }
    }
}

impl Settings {
    pub fn from_value(value: &Value) -> Result<Self, String> {
        let file = value
            .as_object()
            .ok_or("the settings file is not an object")?;
        if file
            .get("colleague")
            .is_some_and(|colleague| !colleague.is_null())
        {
            return Err(
                "\"colleague\" is no longer read: name the colleague's provider and \
                        model in \"roles.colleague\", and remove \"colleague\""
                    .into(),
            );
        }
        let providers = match file.get("providers") {
            None | Some(Value::Null) => None,
            Some(value) => Some(
                serde_json::from_value(value.clone())
                    .map_err(|error| format!("\"providers\": {error}"))?,
            ),
        };
        let settings = Self {
            providers,
            default: section(file, "default")?,
            roles: section(file, "roles")?,
            server: section(file, "server")?,
        };
        settings.check()?;
        Ok(settings)
    }

    pub async fn load(path: &Path) -> Result<Self, String> {
        Self::from_value(&read(path).await?).map_err(|error| format!("{}: {error}", path.display()))
    }

    pub fn provider(&self, id: &str) -> Option<&Provider> {
        self.providers
            .as_ref()
            .and_then(|providers| providers.get(id))
    }

    /// Every field is valid and every provider named is configured.
    fn check(&self) -> Result<(), String> {
        self.server
            .credential_store()
            .map_err(|error| format!("\"server.credentials\": {error}"))?;
        for (id, provider) in self.providers.iter().flatten() {
            check_id(id)?;
            provider.check(id)?;
        }
        let known = |id: &str, place: &str| match self.provider(id) {
            Some(_) => Ok(()),
            None => Err(format!("{place} names the unknown provider {id}")),
        };
        if let Some(id) = &self.default.provider {
            known(id, "\"default\"")?;
        }
        check_effort(self.default.effort.as_deref())
            .and_then(|()| check_summary(self.default.summary.as_deref()))
            .map_err(|error| format!("\"default\": {error}"))?;
        for (name, role) in self.roles.each() {
            let Some(role) = role else { continue };
            if let Some(id) = &role.provider {
                known(id, &format!("\"roles.{name}\""))?;
            }
            check_effort(role.effort.as_deref())
                .map_err(|error| format!("\"roles.{name}\": {error}"))?;
        }
        if self.server.port == Some(0) {
            return Err("\"server.port\" must be from 1 to 65535".into());
        }
        if self
            .server
            .stream_idle_seconds
            .is_some_and(|seconds| !(1..=86_400).contains(&seconds))
        {
            return Err("\"server.stream_idle_seconds\" must be from 1 to 86400".into());
        }
        Ok(())
    }

    /// Where the provider `id` is chosen: `default` and the roles.
    pub fn uses_of(&self, id: &str) -> Vec<String> {
        let mut uses = Vec::new();
        if self.default.provider.as_deref() == Some(id) {
            uses.push("default".to_string());
        }
        for (name, role) in self.roles.each() {
            if role.and_then(|role| role.provider.as_deref()) == Some(id) {
                uses.push(format!("roles.{name}"));
            }
        }
        uses
    }
}

/// Provider ids are short names that are safe in a credential entry name.
fn check_id(id: &str) -> Result<(), String> {
    let usable = !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if usable {
        Ok(())
    } else {
        Err(format!(
            "the provider id {id:?} must be 1 to 64 letters, digits, '-', '_' or '.'"
        ))
    }
}

/// Keys and conversations go only over TLS, or to this machine.
fn check_url(url: &str) -> Result<(), String> {
    let parsed =
        reqwest::Url::parse(url).map_err(|error| format!("\"base_url\" {url:?}: {error}"))?;
    let local = matches!(parsed.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
    match parsed.scheme() {
        "https" => Ok(()),
        "http" if local => Ok(()),
        _ => Err(format!(
            "\"base_url\" {url:?} must be https, or http to localhost"
        )),
    }
}

fn check_effort(effort: Option<&str>) -> Result<(), String> {
    match effort {
        Some(text) if Effort::parse(text).is_none() => Err(format!(
            "the effort {text:?} must be none, minimal, low, medium, high, xhigh, max or ultra"
        )),
        _ => Ok(()),
    }
}

fn check_summary(summary: Option<&str>) -> Result<(), String> {
    match summary {
        Some(text) if Summary::parse(text).is_none() => Err(format!(
            "the summary {text:?} must be auto, concise, detailed or none"
        )),
        _ => Ok(()),
    }
}

/// Writes the checked result of `edit` on the typed settings, leaving the
/// sections other modules own as they are.
async fn edit_settings<T>(
    path: &Path,
    edit: impl FnOnce(&mut Map<String, Value>) -> Result<T, String>,
) -> Result<T, String> {
    update(path, |file| {
        let result = edit(file)?;
        Settings::from_value(&Value::Object(file.clone()))?;
        Ok(result)
    })
    .await
}

fn providers_of(file: &mut Map<String, Value>) -> Result<&mut Map<String, Value>, String> {
    let providers = file.entry("providers").or_insert_with(|| json!({}));
    if providers.is_null() {
        *providers = json!({});
    }
    providers
        .as_object_mut()
        .ok_or_else(|| "\"providers\" is not an object".to_string())
}

/// Adds the provider `id`; an id already in use is refused.
pub async fn add_provider(path: &Path, id: &str, provider: &Provider) -> Result<(), String> {
    check_id(id)?;
    let value = serde_json::to_value(provider).map_err(|error| error.to_string())?;
    edit_settings(path, |file| {
        let providers = providers_of(file)?;
        if providers.contains_key(id) {
            return Err(format!("a provider with the id {id} already exists"));
        }
        providers.insert(id.to_string(), value);
        Ok(())
    })
    .await
}

/// Changes the fields of the provider `id` that `patch` names; a `null`
/// field returns to the kind's default.
pub async fn update_provider(path: &Path, id: &str, patch: &Value) -> Result<Provider, String> {
    let patch = patch.as_object().ok_or("the change is not an object")?;
    edit_settings(path, |file| {
        let entry = providers_of(file)?
            .get_mut(id)
            .ok_or_else(|| format!("no provider has the id {id}"))?
            .as_object_mut()
            .ok_or_else(|| format!("provider {id} is not an object"))?;
        for (name, field) in patch {
            if field.is_null() {
                entry.remove(name);
            } else {
                entry.insert(name.clone(), field.clone());
            }
        }
        serde_json::from_value(Value::Object(entry.clone()))
            .map_err(|error| format!("provider {id}: {error}"))
    })
    .await
}

/// Hides or shows the model `model` of the provider `id` in the lists
/// shown for choosing.
pub async fn set_model_hidden(
    path: &Path,
    id: &str,
    model: &str,
    hidden: bool,
) -> Result<Provider, String> {
    edit_settings(path, |file| {
        let entry = providers_of(file)?
            .get_mut(id)
            .ok_or_else(|| format!("no provider has the id {id}"))?
            .as_object_mut()
            .ok_or_else(|| format!("provider {id} is not an object"))?;
        let mut models: Vec<String> = entry
            .get("hidden_models")
            .and_then(Value::as_array)
            .map(|models| {
                models
                    .iter()
                    .filter_map(|model| model.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        models.retain(|kept| kept != model);
        if hidden {
            models.push(model.to_string());
        }
        if models.is_empty() {
            entry.remove("hidden_models");
        } else {
            entry.insert("hidden_models".into(), Value::from(models));
        }
        serde_json::from_value(Value::Object(entry.clone()))
            .map_err(|error| format!("provider {id}: {error}"))
    })
    .await
}

/// Removes the provider `id`, unless `default` or a role chooses it.
pub async fn remove_provider(path: &Path, id: &str) -> Result<(), RemoveError> {
    let mut used = Vec::new();
    edit_settings(path, |file| {
        let settings = Settings::from_value(&Value::Object(file.clone()))?;
        if settings.provider(id).is_none() {
            return Err(format!("no provider has the id {id}"));
        }
        used = settings.uses_of(id);
        if !used.is_empty() {
            return Err(String::new());
        }
        providers_of(file)?.remove(id);
        Ok(())
    })
    .await
    .map_err(|error| {
        if used.is_empty() {
            RemoveError::Invalid(error)
        } else {
            RemoveError::InUse(used)
        }
    })
}

#[derive(Debug, PartialEq, Eq)]
pub enum RemoveError {
    Invalid(String),
    /// Where the provider is still chosen.
    InUse(Vec<String>),
}

/// The sections `set_sections` may change.
pub const SECTIONS: [&str; 3] = ["default", "roles", "server"];

/// Merges `patch` into the sections it names, one level deep: a field set
/// to `null` is removed. Other sections are refused.
pub async fn set_sections(path: &Path, patch: &Value) -> Result<Settings, String> {
    let patch = patch.as_object().ok_or("the change is not an object")?;
    if let Some(name) = patch.keys().find(|name| !SECTIONS.contains(&name.as_str())) {
        return Err(format!(
            "\"{name}\" cannot be set here; the sections are default, roles and server"
        ));
    }
    edit_settings(path, |file| {
        for (name, fields) in patch {
            let fields = fields
                .as_object()
                .ok_or_else(|| format!("\"{name}\" is not an object"))?;
            let section = file.entry(name.clone()).or_insert_with(|| json!({}));
            if !section.is_object() {
                *section = json!({});
            }
            let section = section.as_object_mut().expect("an object");
            for (field, value) in fields {
                if value.is_null() {
                    section.remove(field);
                } else {
                    section.insert(field.clone(), value.clone());
                }
            }
        }
        Settings::from_value(&Value::Object(file.clone()))
    })
    .await
}

/// Gives the file a `providers` section the first time the server starts:
/// the providers that worked before it had one. That is Logfare's
/// OpenAI-compatible endpoint when its key is stored, and each sign-in kind
/// with a stored credential, under the kind's name; `default` gets the first
/// of them. Returns whether the section was written; a file that already
/// has the section, or no credential, is left as it is.
pub async fn create_defaults(
    path: &Path,
    credentials: &crate::oauth::Store,
) -> Result<bool, String> {
    if Settings::load(path).await?.providers.is_some() {
        return Ok(false);
    }
    let mut found: Vec<(String, Provider)> = Vec::new();
    if credentials.credential(&api_key_name(LOGFARE)).is_some() {
        let mut logfare = Provider::new(Kind::OpenAi);
        logfare.default_model = Some(crate::DEFAULT_MODEL.to_string());
        found.push((LOGFARE.to_string(), logfare));
    }
    for kind in [
        Kind::ClaudeOauth,
        Kind::GptOauth,
        Kind::MuseCode,
        Kind::DeepAi,
    ] {
        if credentials.credential(kind.auth().store_name()).is_some() {
            found.push((kind.name().to_string(), Provider::new(kind)));
        }
    }
    let Some(first) = found.first().map(|(id, _)| id.clone()) else {
        return Ok(false);
    };
    edit_settings(path, |file| {
        if file.get("providers").is_some_and(|value| !value.is_null()) {
            return Ok(false);
        }
        let providers: Map<String, Value> = found
            .iter()
            .map(|(id, provider)| (id.clone(), serde_json::to_value(provider).expect("plain")))
            .collect();
        file.insert("providers".into(), Value::Object(providers));
        let default = file.entry("default").or_insert_with(|| json!({}));
        if let Some(default) = default.as_object_mut() {
            default.entry("provider").or_insert(json!(first));
        }
        Ok(true)
    })
    .await
}

/// Adds the sign-in kind `kind` as a provider under its name, after a
/// login, unless a provider of that kind is already configured; it becomes
/// the default when none is chosen.
pub async fn add_signed_in(path: &Path, kind: Kind) -> Result<bool, String> {
    edit_settings(path, |file| {
        let settings = Settings::from_value(&Value::Object(file.clone()))?;
        if settings
            .providers
            .iter()
            .flatten()
            .any(|(_, provider)| provider.kind == kind)
            || settings.provider(kind.name()).is_some()
        {
            return Ok(false);
        }
        providers_of(file)?.insert(
            kind.name().to_string(),
            serde_json::to_value(Provider::new(kind)).expect("plain"),
        );
        if settings.default.provider.is_none() {
            let default = file.entry("default").or_insert_with(|| json!({}));
            if let Some(default) = default.as_object_mut() {
                default.insert("provider".into(), json!(kind.name()));
            }
        }
        Ok(true)
    })
    .await
}

/// The id the first start gives Logfare's endpoint.
pub const LOGFARE: &str = "logfare";

#[cfg(test)]
mod tests {
    use super::*;

    fn folder(label: &str) -> std::path::PathBuf {
        let folder =
            std::env::temp_dir().join(format!("inocode-settings-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&folder);
        std::fs::create_dir_all(&folder).unwrap();
        folder
    }

    #[tokio::test]
    async fn another_process_writing_is_waited_for_and_its_change_is_noticed() {
        let folder = folder("across");
        let path = folder.join("settings.json");
        std::fs::write(&path, r#"{"language": "Japanese"}"#).unwrap();
        read(&path).await.unwrap();

        // Another program holds the lock while it writes.
        let lock =
            crate::secrets::open_lock_file(&crate::secrets::sibling(&path, ".lock")).unwrap();
        lock.lock().unwrap();
        let writing = tokio::spawn({
            let path = path.clone();
            async move {
                set_sections(&path, &json!({"server": {"port": 9000}}))
                    .await
                    .map(|_| ())
            }
        });
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert!(!writing.is_finished(), "the write waits for the lock");
        let before = written();
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&path, r#"{"language": "English"}"#).unwrap();
        assert_ne!(written(), before, "a change by another process is noticed");
        lock.unlock().unwrap();
        writing.await.unwrap().unwrap();
        let file: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            file["language"], "English",
            "the other program's change is kept"
        );
        assert_eq!(file["server"]["port"], 9000);
    }

    #[tokio::test]
    async fn providers_are_added_changed_and_removed_keeping_other_sections() {
        let folder = folder("crud");
        let path = folder.join("settings.json");
        std::fs::write(
            &path,
            r#"{"language": "Japanese", "translation": {"thinking": true}}"#,
        )
        .unwrap();
        let before = written();
        add_provider(&path, "local", &Provider::new(Kind::OpenAi))
            .await
            .unwrap();
        assert_ne!(written(), before);
        assert!(
            add_provider(&path, "local", &Provider::new(Kind::Anthropic))
                .await
                .unwrap_err()
                .contains("already exists")
        );
        let changed = update_provider(
            &path,
            "local",
            &json!({"base_url": "http://localhost:11434/v1", "default_model": "qwen"}),
        )
        .await
        .unwrap();
        assert_eq!(changed.base_url(), "http://localhost:11434/v1");
        assert!(update_provider(
            &path,
            "local",
            &json!({"base_url": "http://example.com/v1"})
        )
        .await
        .unwrap_err()
        .contains("must be https"));
        assert!(update_provider(&path, "local", &json!({"kind": "ollama"}))
            .await
            .is_err());
        set_sections(&path, &json!({"default": {"provider": "local"}}))
            .await
            .unwrap();
        assert_eq!(
            remove_provider(&path, "local").await,
            Err(RemoveError::InUse(vec!["default".into()]))
        );
        set_sections(&path, &json!({"default": {"provider": null}}))
            .await
            .unwrap();
        remove_provider(&path, "local").await.unwrap();
        let file: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(file["language"], "Japanese");
        assert_eq!(file["translation"]["thinking"], true);
        assert_eq!(file["providers"], json!({}));
        std::fs::remove_dir_all(folder).unwrap();
    }

    #[tokio::test]
    async fn sections_are_checked_before_they_are_written() {
        let folder = folder("check");
        let path = folder.join("settings.json");
        assert!(
            set_sections(&path, &json!({"default": {"provider": "nowhere"}}))
                .await
                .unwrap_err()
                .contains("unknown provider nowhere")
        );
        assert!(
            set_sections(&path, &json!({"roles": {"review": {"effort": "huge"}}}))
                .await
                .is_err()
        );
        assert!(set_sections(&path, &json!({"providers": {}}))
            .await
            .unwrap_err()
            .contains("cannot be set here"));
        assert!(!path.exists(), "nothing was written");
        let settings = set_sections(&path, &json!({"server": {"port": 9000, "sandbox": false}}))
            .await
            .unwrap();
        assert_eq!(settings.server.port, Some(9000));
        assert_eq!(settings.server.sandbox, Some(false));
        std::fs::remove_dir_all(folder).unwrap();
    }

    #[tokio::test]
    async fn the_first_start_adds_the_providers_whose_credentials_are_stored() {
        let folder = folder("defaults");
        let path = folder.join("settings.json");
        let mut store = crate::oauth::Store::load(folder.join("auth.json")).unwrap();
        assert!(!create_defaults(&path, &store).await.unwrap());
        store
            .update(&api_key_name(LOGFARE), |credential| {
                credential.api_key = Some("k".into())
            })
            .unwrap();
        store
            .update("claude-oauth", |credential| credential.access = "a".into())
            .unwrap();
        assert!(create_defaults(&path, &store).await.unwrap());
        let settings = Settings::load(&path).await.unwrap();
        let providers = settings.providers.clone().unwrap();
        assert_eq!(
            providers.keys().collect::<Vec<_>>(),
            ["claude-oauth", "logfare"]
        );
        assert_eq!(providers["logfare"].kind, Kind::OpenAi);
        assert_eq!(providers["logfare"].default_model(), crate::DEFAULT_MODEL);
        assert_eq!(settings.default.provider.as_deref(), Some(LOGFARE));
        // Once the section exists it is the person's.
        remove_provider(&path, "claude-oauth").await.unwrap();
        assert!(!create_defaults(&path, &store).await.unwrap());
        assert!(Settings::load(&path)
            .await
            .unwrap()
            .provider("claude-oauth")
            .is_none());
        assert!(add_signed_in(&path, Kind::ClaudeOauth).await.unwrap());
        assert!(!add_signed_in(&path, Kind::ClaudeOauth).await.unwrap());
        std::fs::remove_dir_all(folder).unwrap();
    }
}
