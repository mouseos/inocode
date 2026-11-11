//! The model provider hands out the account pool's active credentials, so an account switch is
//! seen by the next request (the client then opens a fresh session with the full history).
//! Own test binary: the pool is process-global.

use codex_login::AuthCredentialsStoreMode;
use codex_login::AuthManager;
use codex_login::CodexAuth;
use codex_login::account_pool;
use codex_login::account_pool::AccountPool;
use codex_login::account_pool::PoolEnv;
use codex_login::account_pool::SwitchReason;
use codex_model_provider::create_model_provider;
use codex_model_provider::request_auth_owner;
use codex_model_provider_info::ModelProviderInfo;
use pretty_assertions::assert_eq;
use std::sync::Arc;

#[tokio::test]
async fn provider_follows_the_pool_and_sees_the_switch() {
    let home = tempfile::tempdir().expect("tempdir");
    let primary = AuthManager::from_auth_for_testing(CodexAuth::from_api_key("sk-test-FAKE-default"));
    let env = PoolEnv {
        codex_home: home.path().to_path_buf(),
        mode: AuthCredentialsStoreMode::File,
        keyring: Default::default(),
        chatgpt_base_url: None,
        auth_route_config: codex_login::test_support::transport_default_auth_route_config(),
    };
    let pool = AccountPool::open(env, Arc::clone(&primary)).await;
    pool.rename(account_pool::DEFAULT_ID, "既定キー").expect("rename");
    pool.set_priority(account_pool::DEFAULT_ID, 5).expect("priority");
    pool.add_api_key("二番目", "sk-test-FAKE-second", Some(1))
        .await
        .expect("add key");
    account_pool::install_global(Some(Arc::clone(&pool)));

    let provider = create_model_provider(
        ModelProviderInfo::create_openai_provider(/*base_url*/ None),
        Some(Arc::clone(&primary)),
    );

    // First use: the highest-priority key.
    let first = provider.auth().await.expect("auth");
    assert_eq!(first.api_key(), Some("sk-test-FAKE-default"));
    let first_owner = request_auth_owner(provider.as_ref());

    // The first account hits its limit: the same provider now serves the other one.
    let event = pool
        .report_limit_reached(Some("thread-1"), Some(i64::MAX / 2))
        .expect("switch");
    assert_eq!(event.reason, SwitchReason::LimitReached);
    let second = provider.auth().await.expect("auth");
    assert_eq!(second.api_key(), Some("sk-test-FAKE-second"));
    assert!(
        request_auth_owner(provider.as_ref()) != first_owner,
        "a different owner makes the client start a fresh session and resend everything"
    );

    // Without a pool the provider is back to its own manager.
    account_pool::install_global(None);
    let alone = provider.auth().await.expect("auth");
    assert_eq!(alone.api_key(), Some("sk-test-FAKE-default"));
}
