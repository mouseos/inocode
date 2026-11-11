use base64::Engine;
use base64::engine::general_purpose::STANDARD;

pub const AUTHORIZE_URL: &str = "https://claude.ai/oauth/authorize";
pub const TOKEN_URL: &str = "https://api.anthropic.com/v1/oauth/token";
pub const PROFILE_URL: &str = "https://api.anthropic.com/api/oauth/profile";
pub const SCOPES: &str = "org:create_api_key user:profile user:inference";
pub const CALLBACK_PORT: u16 = 54545;
pub const CALLBACK_PATH: &str = "/callback";
/// The provider shows `code#state` for the user to paste (no local listener needed).
pub const CODE_REDIRECT_URI: &str = "https://platform.claude.com/oauth/code/callback";
pub const OAUTH_BETA: &str = "oauth-2025-04-20";
/// Refresh this long before the token really expires.
pub const EXPIRY_MARGIN_MS: i64 = 5 * 60 * 1000;

/// Environment override for the OAuth client id (experiments).
pub const CLIENT_ID_ENV: &str = "INOCODE_CLAUDE_OAUTH_CLIENT_ID";
const CLIENT_ID_B64: &str = "OWQxYzI1MGEtZTYxYi00NGQ5LTg4ZWQtNTk0NGQxOTYyZjVl";

pub fn client_id() -> String {
    if let Ok(value) = std::env::var(CLIENT_ID_ENV)
        && !value.trim().is_empty()
    {
        return value.trim().to_string();
    }
    STANDARD
        .decode(CLIENT_ID_B64)
        .ok()
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .unwrap_or_default()
}

pub fn callback_redirect_uri() -> String {
    format!("http://localhost:{CALLBACK_PORT}{CALLBACK_PATH}")
}
