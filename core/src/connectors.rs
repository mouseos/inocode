//! The connectors of the accounts the person signed in with: claude.ai's
//! connectors, reached through Anthropic's MCP proxy with the Claude sign-in,
//! and ChatGPT's apps, reached through its apps MCP server with the ChatGPT
//! sign-in. Both are MCP servers over Streamable HTTP whose bearer is the
//! account's token. Sources: docs/evidence/plugin-mechanism.md.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

use crate::oauth::AuthKind;

/// Where the accounts' services are; the defaults are the real ones.
#[derive(Debug, Clone)]
pub(crate) struct Hosts {
    /// Lists claude.ai's connectors.
    pub claude_api: String,
    /// Anthropic's MCP proxy; a connector is `{proxy}/v1/mcp/{id}`.
    pub claude_proxy: String,
    /// ChatGPT's backend; its apps are at `{chatgpt}/wham/apps`.
    pub chatgpt: String,
    /// Claude's token endpoint when it is not the real one.
    pub claude_token: Option<String>,
}

impl Default for Hosts {
    fn default() -> Self {
        Self {
            claude_api: "https://api.anthropic.com".into(),
            claude_proxy: "https://mcp-proxy.anthropic.com".into(),
            chatgpt: "https://chatgpt.com/backend-api".into(),
            claude_token: None,
        }
    }
}

/// The account a connection signs in with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Account {
    Claude,
    ChatGpt,
}

impl Account {
    fn auth(self) -> AuthKind {
        match self {
            Self::Claude => AuthKind::ClaudeOauth,
            Self::ChatGpt => AuthKind::GptOauth,
        }
    }

    /// The headers that sign a request in with this account, from the
    /// credential at `store`, refreshed when it has expired.
    pub(crate) async fn headers(self, store: &Path) -> Result<reqwest::header::HeaderMap, String> {
        let active = active_account(store, self.auth());
        let (_, credential) =
            crate::oauth::fresh(store.to_path_buf(), self.auth(), active.as_deref(), None)
                .await
                .map_err(|error| error.message)?;
        let user_agent = match self {
            Self::Claude => crate::claude_cli::user_agent(store).await,
            Self::ChatGpt => String::new(),
        };
        self.sign(&credential, &user_agent)
    }

    fn sign(
        self,
        credential: &crate::oauth::Credential,
        user_agent: &str,
    ) -> Result<reqwest::header::HeaderMap, String> {
        let mut headers = reqwest::header::HeaderMap::new();
        let mut add = |name: &'static str, value: &str| -> Result<(), String> {
            let value = reqwest::header::HeaderValue::from_str(value)
                .map_err(|_| format!("the stored credential cannot be sent as {name}"))?;
            headers.insert(name, value);
            Ok(())
        };
        add("authorization", &format!("Bearer {}", credential.access))?;
        match self {
            Self::Claude => add("user-agent", user_agent)?,
            Self::ChatGpt => {
                let account = credential.account_id.as_deref().ok_or(
                    "the stored ChatGPT credential has no account id; run the login again",
                )?;
                add("chatgpt-account-id", account)?;
            }
        }
        Ok(headers)
    }
}

/// One connector server to reach.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Server {
    /// The server's name in tool names.
    pub name: String,
    pub url: String,
    pub account: Account,
    /// Headers sent with every request besides the account's.
    pub headers: Vec<(String, String)>,
}

/// Whether the store at `store` holds a sign-in for `account` that the
/// settings beside it use: once the settings list providers, one of the
/// account's kind must be listed and not turned off.
fn signed_in(store: &Path, account: Account) -> bool {
    let settings = crate::settings::read_blocking(&store.with_file_name("settings.json"))
        .and_then(|value| crate::settings::Settings::from_value(&value));
    let used = match settings {
        Ok(settings) => match &settings.providers {
            Some(providers) => providers
                .values()
                .any(|provider| provider.kind.auth() == account.auth() && !provider.disabled),
            None => true,
        },
        Err(_) => false,
    };
    used && crate::oauth::Store::load(store.to_path_buf())
        .map(|loaded| loaded.credential(account.auth().store_name()).is_some())
        .unwrap_or(false)
}

/// The account connector requests sign in with: the active account of the
/// first provider of its kind the settings use, when one is chosen.
fn active_account(store: &Path, auth: AuthKind) -> Option<String> {
    let value = crate::settings::read_blocking(&store.with_file_name("settings.json")).ok()?;
    let settings = crate::settings::Settings::from_value(&value).ok()?;
    settings
        .providers
        .iter()
        .flatten()
        .filter(|(_, provider)| provider.kind.auth() == auth && !provider.disabled)
        .find_map(|(_, provider)| provider.active_account.clone())
}

/// How long listing claude.ai's connectors may take.
const LIST_TIMEOUT: Duration = Duration::from_secs(20);
/// Pages of connectors read at most.
const LIST_PAGES: usize = 20;
/// The beta the connector list is served under.
const CONNECTORS_BETA: &str = "mcp-servers-2025-12-04";

#[derive(Deserialize)]
struct Page {
    #[serde(default)]
    data: Vec<Connector>,
    #[serde(default)]
    next_page: Option<String>,
}

#[derive(Deserialize)]
struct Connector {
    id: String,
    #[serde(default)]
    display_name: Option<String>,
}

/// The connector servers of the accounts signed in at `store`: claude.ai's
/// connectors with the Claude sign-in, ChatGPT's apps with the ChatGPT
/// sign-in. An account whose connectors cannot be listed is reported and
/// left out.
pub(crate) async fn servers(store: &Path, hosts: &Hosts) -> Vec<Server> {
    let mut servers = Vec::new();
    if signed_in(store, Account::Claude) {
        match claude(store, hosts).await {
            Ok(found) => servers.extend(found),
            Err(error) => eprintln!("MCP: claude.ai connectors are not available: {error}"),
        }
    }
    if signed_in(store, Account::ChatGpt) {
        servers.push(Server {
            name: "codex_apps".into(),
            url: format!("{}/wham/apps", hosts.chatgpt.trim_end_matches('/')),
            account: Account::ChatGpt,
            headers: Vec::new(),
        });
    }
    servers
}

/// claude.ai's connectors, each reached through the proxy.
async fn claude(store: &Path, hosts: &Hosts) -> Result<Vec<Server>, String> {
    let client = reqwest::Client::new();
    let list = format!(
        "{}/v1/mcp_servers?limit=1000",
        hosts.claude_api.trim_end_matches('/')
    );
    let mut connectors = Vec::new();
    let mut page: Option<String> = None;
    let (account, mut credential) = crate::oauth::fresh(
        store.to_path_buf(),
        AuthKind::ClaudeOauth,
        active_account(store, AuthKind::ClaudeOauth).as_deref(),
        hosts.claude_token.as_deref(),
    )
    .await
    .map_err(|error| error.message)?;
    let user_agent = crate::claude_cli::user_agent(store).await;
    let mut renewed = false;
    let mut read_pages = 0;
    while read_pages < LIST_PAGES {
        let url = match &page {
            Some(token) => format!("{list}&page={}", crate::oauth::urlencode(token)),
            None => list.clone(),
        };
        let mut response = client
            .get(url)
            .headers(Account::Claude.sign(&credential, &user_agent)?)
            .header("anthropic-version", "2023-06-01")
            .header("anthropic-beta", CONNECTORS_BETA)
            .header("content-type", "application/json")
            .timeout(LIST_TIMEOUT)
            .send()
            .await
            .map_err(|error| format!("cannot list them: {error}"))?;
        let status = response.status();
        let body = crate::llm::read_capped(&mut response, 4 * 1024 * 1024)
            .await
            .map_err(|error| format!("cannot read the list: {error}"))?
            .ok_or("the list is too large")?;
        if !status.is_success() {
            let text = String::from_utf8_lossy(&body);
            // A sign-in made before the scope was asked for lacks it; a
            // refresh asks for it, as Claude Code does on every refresh.
            if status.as_u16() == 403 && text.contains("scope") && !renewed {
                renewed = true;
                credential = crate::oauth::renew(
                    store.to_path_buf(),
                    AuthKind::ClaudeOauth,
                    Some(&account),
                    &credential.access,
                    hosts.claude_token.as_deref(),
                )
                .await
                .map_err(|error| error.message)?
                .1;
                continue;
            }
            return Err(match status.as_u16() {
                401 | 403 => format!(
                    "the list was refused ({status}); sign in to Claude again: {}",
                    text.trim()
                ),
                _ => format!("the list answered {status}: {}", text.trim()),
            });
        }
        read_pages += 1;
        let read: Page = serde_json::from_slice(&body)
            .map_err(|error| format!("the list is not the expected JSON: {error}"))?;
        connectors.extend(read.data);
        match read.next_page.filter(|token| !token.is_empty()) {
            Some(token) => page = Some(token),
            None => break,
        }
    }
    let proxy = hosts.claude_proxy.trim_end_matches('/');
    Ok(connectors
        .into_iter()
        .map(|connector| Server {
            name: format!(
                "claude.ai {}",
                connector.display_name.as_deref().unwrap_or(&connector.id)
            ),
            url: format!("{proxy}/v1/mcp/{}", connector.id),
            account: Account::Claude,
            headers: vec![("x-mcp-client-session-id".into(), session_id())],
        })
        .collect())
}

/// A random id for one connection's session with the proxy.
fn session_id() -> String {
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes).expect("OS random numbers");
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// The sign-in store beside the thread store, where the server keeps it by
/// default (`~/.inocode/auth.json` beside `~/.inocode/inocode.db`).
pub(crate) fn store_beside(threads: &crate::store::Store) -> PathBuf {
    threads.files_folder().with_file_name("auth.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn connectors_follow_the_providers_the_settings_use() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("auth.json");
        crate::oauth::Store::load(store.clone())
            .unwrap()
            .update(AuthKind::GptOauth.store_name(), |credential| {
                credential.access = "token".into()
            })
            .unwrap();
        let settings = dir.path().join("settings.json");
        let names = || async {
            servers(&store, &Hosts::default())
                .await
                .into_iter()
                .map(|server| server.name)
                .collect::<Vec<_>>()
        };
        // Before the settings list providers, the sign-in is enough.
        assert_eq!(names().await, ["codex_apps"]);
        std::fs::write(
            &settings,
            r#"{"providers": {"gpt": {"kind": "gpt-oauth", "disabled": true}}}"#,
        )
        .unwrap();
        assert!(names().await.is_empty(), "a turned-off provider");
        std::fs::write(&settings, r#"{"providers": {}}"#).unwrap();
        assert!(names().await.is_empty(), "a removed provider");
        std::fs::write(
            &settings,
            r#"{"providers": {"gpt": {"kind": "gpt-oauth"}}}"#,
        )
        .unwrap();
        assert_eq!(names().await, ["codex_apps"]);
    }
}
