pub mod cache;
mod catalog;
pub mod collaboration_mode_presets;
pub(crate) mod config;
pub mod manager;
pub mod model_info;
pub mod model_presets;
mod originator;
pub mod test_support;

pub use catalog::CatalogModel;
pub use catalog::ModelOrigin;
pub use codex_protocol::auth::AuthMode;
pub use config::ModelsManagerConfig;
pub use originator::ModelCatalogOriginator;

/// Load the bundled model catalog shipped with `codex-models-manager`.
pub fn bundled_models_response()
-> std::result::Result<codex_protocol::openai_models::ModelsResponse, serde_json::Error> {
    serde_json::from_str(include_str!("../models.json"))
}

/// Convert the client version string to a whole version string (e.g. "1.2.3-alpha.4" -> "1.2.3").
pub fn client_version_to_whole() -> String {
    // `INOCODE_CLIENT_VERSION` (x.y.z) overrides the compiled-in version, so a client can
    // present the version of the official Codex it follows; the backend filters models
    // by `minimal_client_version`.
    if let Ok(value) = std::env::var("INOCODE_CLIENT_VERSION") {
        let whole = value.split(['-', '+']).next().unwrap_or("").trim().to_string();
        let parts: Vec<&str> = whole.split('.').collect();
        if parts.len() == 3 && parts.iter().all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit())) {
            return whole;
        }
    }
    format!(
        "{}.{}.{}",
        env!("CARGO_PKG_VERSION_MAJOR"),
        env!("CARGO_PKG_VERSION_MINOR"),
        env!("CARGO_PKG_VERSION_PATCH")
    )
}
