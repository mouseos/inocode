use std::sync::Arc;

use codex_core::config::Config;
use codex_extension_api::ExtensionData;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_extension_api::ToolContributor;
use zero_search::Client;

use crate::tool::ToolKind;
use crate::tool::ZeroSearchTool;

struct ZeroSearchExtension {
    client: Arc<Client>,
}

impl ToolContributor for ZeroSearchExtension {
    fn tools_for_step(
        &self,
        _session_store: &ExtensionData,
        _thread_store: &ExtensionData,
        _step_store: &ExtensionData,
    ) -> Vec<
        Arc<dyn for<'call> codex_extension_api::ToolExecutor<codex_extension_api::ToolCall<'call>>>,
    > {
        // Core drops these tools when web_search = "disabled".
        [ToolKind::Web, ToolKind::Shopping, ToolKind::Media]
            .into_iter()
            .map(|kind| {
                Arc::new(ZeroSearchTool::new(kind, Arc::clone(&self.client)))
                    as Arc<
                        dyn for<'call> codex_extension_api::ToolExecutor<
                            codex_extension_api::ToolCall<'call>,
                        >,
                    >
            })
            .collect()
    }
}

pub fn install(registry: &mut ExtensionRegistryBuilder<Config>) {
    // ZeroSearch's Bilibili provider installs a ring-backed process default if none is set.
    // Install the aws-lc-rs default first so every TLS client in the process shares it.
    codex_utils_rustls_provider::ensure_rustls_crypto_provider();
    match Client::new() {
        Ok(client) => registry.tool_contributor(Arc::new(ZeroSearchExtension {
            client: Arc::new(client),
        })),
        Err(err) => tracing::warn!("built-in search is unavailable: {err:#}"),
    }
}

#[cfg(test)]
mod tests {
    use codex_extension_api::ExtensionData;
    use codex_extension_api::ExtensionRegistryBuilder;
    use super::Config;
    use super::install;

    #[test]
    fn install_sets_the_shared_rustls_provider_before_any_search_runs() {
        let mut builder = ExtensionRegistryBuilder::<Config>::new();
        install(&mut builder);
        let provider = rustls::crypto::CryptoProvider::get_default().expect("default provider");
        assert!(
            provider
                .signature_verification_algorithms
                .supported_schemes()
                .contains(&rustls::SignatureScheme::ECDSA_NISTP521_SHA512),
            "the default must be the aws-lc-rs provider, not ring"
        );
    }

    #[test]
    fn extension_contributes_the_three_search_tools() {
        let mut builder = ExtensionRegistryBuilder::<Config>::new();
        install(&mut builder);
        let registry = builder.build();
        let session_store = ExtensionData::new("session");
        let thread_store = ExtensionData::new("11111111-1111-4111-8111-111111111111");
        let step_store = ExtensionData::new("step");
        let names: Vec<String> = registry
            .tool_contributors()
            .iter()
            .flat_map(|contributor| {
                contributor.tools_for_step(&session_store, &thread_store, &step_store)
            })
            .map(|tool| tool.tool_name().name)
            .collect();
        assert_eq!(names, vec!["web_search", "shopping_search", "media_search"]);
    }
}
