use super::*;
use crate::AuthCredentialsStoreMode;
use crate::AuthManager;
use crate::CodexAuth;
use pretty_assertions::assert_eq;
use std::sync::atomic::AtomicI64;
use std::sync::atomic::Ordering;

const NOW: i64 = 1_000_000;

struct Fixture {
    home: tempfile::TempDir,
    clock: Arc<AtomicI64>,
    pool: Arc<AccountPool>,
}

fn env(home: &std::path::Path) -> PoolEnv {
    PoolEnv {
        codex_home: home.to_path_buf(),
        mode: AuthCredentialsStoreMode::File,
        keyring: crate::AuthKeyringBackendKind::default(),
        chatgpt_base_url: None,
        auth_route_config: crate::test_support::transport_default_auth_route_config(),
    }
}

fn usage(five: f64, week: f64) -> AccountUsage {
    AccountUsage {
        five_hour: Some(UsageWindow {
            used_percent: five,
            window_minutes: Some(300),
            resets_at: Some(NOW + 3_600),
        }),
        weekly: Some(UsageWindow {
            used_percent: week,
            window_minutes: Some(10_080),
            resets_at: Some(NOW + 86_400),
        }),
        updated_at: Some(NOW),
    }
}

fn meta(id: &str, alias: &str, priority: i32, five: f64) -> AccountMeta {
    AccountMeta {
        id: id.to_string(),
        kind: AccountKind::Chatgpt,
        alias: alias.to_string(),
        plan: Some("plus".to_string()),
        priority,
        enabled: true,
        cooldown_until: None,
        last_used_at: None,
        usage: Some(usage(five, 10.0)),
        added_at: NOW,
        provider: "openai".to_string(),
    }
}

/// "default" (メイン, 10%) plus two fake ChatGPT entries.
async fn fixture() -> Fixture {
    let home = tempfile::tempdir().expect("tempdir");
    let clock = Arc::new(AtomicI64::new(NOW));
    let primary = AuthManager::from_auth_for_testing_with_home(
        CodexAuth::create_dummy_chatgpt_auth_for_testing(),
        home.path().to_path_buf(),
    );
    let ticking = Arc::clone(&clock);
    let pool = AccountPool::open_with_clock(
        env(home.path()),
        primary,
        Arc::new(move || ticking.load(Ordering::SeqCst)),
    )
    .await;
    pool.rename(DEFAULT_ID, "メイン").expect("rename default");
    pool.set_priority(DEFAULT_ID, 10).expect("priority");
    pool.record_usage(DEFAULT_ID, usage(10.0, 10.0), false);
    for (id, alias, five) in [("chatgpt-aaaa", "サブ", 30.0), ("chatgpt-bbbb", "予備", 50.0)] {
        pool.insert_for_test(
            meta(id, alias, 0, five),
            AuthManager::from_auth_for_testing(CodexAuth::create_dummy_chatgpt_auth_for_testing()),
        );
    }
    Fixture { home, clock, pool }
}

fn in_use(pool: &AccountPool) -> String {
    pool.list()
        .into_iter()
        .find(|v| v.in_use)
        .map(|v| v.alias)
        .unwrap_or_default()
}

#[tokio::test]
async fn starts_on_the_least_used_account_and_stays() {
    let f = fixture().await;
    f.pool.active_auth_manager().expect("manager");
    assert_eq!(in_use(&f.pool), "メイン");
    assert_eq!(f.pool.select_for_sampling(Some("t1"), true), None);
    assert_eq!(in_use(&f.pool), "メイン");
}

#[tokio::test]
async fn crossing_the_threshold_switches_before_the_next_sampling() {
    let f = fixture().await;
    let first = f.pool.active_auth_manager().expect("manager");
    let mut events = f.pool.subscribe();
    f.pool
        .record_usage("default", usage(95.0, 10.0), /*observed_while_in_use*/ true);

    let event = f
        .pool
        .select_for_sampling(Some("thread-1"), /*turn_start*/ false)
        .expect("a switch");
    assert_eq!(
        event,
        SwitchEvent {
            from_alias: Some("メイン".to_string()),
            to_alias: "サブ".to_string(),
            reason: SwitchReason::Threshold,
            thread_id: Some("thread-1".to_string()),
        }
    );
    let second = f.pool.active_auth_manager().expect("manager");
    assert!(
        !Arc::ptr_eq(&first, &second),
        "requests now use another manager"
    );
    assert!(event.summary().contains("メイン") && event.summary().contains("サブ"));

    let mut saw_switch = false;
    while let Ok(received) = events.try_recv() {
        saw_switch |= matches!(received, PoolEvent::Switched(_));
    }
    assert!(saw_switch);
}

#[tokio::test]
async fn a_learned_pace_foresees_the_crossing() {
    let f = fixture().await;
    f.pool.active_auth_manager().expect("manager");
    f.pool.record_usage("default", usage(70.0, 10.0), true);
    f.pool.record_usage("default", usage(80.0, 10.0), true);
    // 80% plus several points per sampling reaches 90%.
    let event = f.pool.select_for_sampling(None, false).expect("a switch");
    assert_eq!(event.reason, SwitchReason::Threshold);
}

#[tokio::test]
async fn a_usage_limit_cools_the_account_down_and_moves_on() {
    let f = fixture().await;
    f.pool.active_auth_manager().expect("manager");
    let reset = NOW + 7_200;
    let event = f
        .pool
        .report_limit_reached(Some("thread-9"), Some(reset))
        .expect("another account takes over");
    assert_eq!(event.reason, SwitchReason::LimitReached);
    assert_eq!(event.from_alias.as_deref(), Some("メイン"));
    assert_eq!(event.to_alias, "サブ");

    let list = f.pool.list();
    let cooled = list.iter().find(|v| v.alias == "メイン").expect("default");
    assert_eq!(cooled.state, AccountState::Cooldown);
    assert_eq!(cooled.cooldown_until, Some(reset));

    // After the reset time the account is usable again (it is not forced back, though).
    f.clock.store(reset + 1, Ordering::SeqCst);
    let list = f.pool.list();
    let back = list.iter().find(|v| v.alias == "メイン").expect("default");
    assert_eq!(back.state, AccountState::Active);
    assert_eq!(back.cooldown_until, None);
    assert_eq!(in_use(&f.pool), "サブ");
}

#[tokio::test]
async fn without_a_reset_time_the_policy_default_cooldown_applies() {
    let f = fixture().await;
    f.pool.active_auth_manager().expect("manager");
    f.pool.report_limit_reached(None, None).expect("switch");
    let list = f.pool.list();
    let cooled = list.iter().find(|v| v.alias == "メイン").expect("default");
    assert_eq!(cooled.cooldown_until, Some(NOW + 60));
}

#[tokio::test]
async fn when_every_account_is_cooling_down_there_is_nothing_to_retry_with() {
    let f = fixture().await;
    f.pool.active_auth_manager().expect("manager");
    f.pool
        .report_limit_reached(None, Some(NOW + 100))
        .expect("to サブ");
    f.pool
        .report_limit_reached(None, Some(NOW + 100))
        .expect("to 予備");
    assert_eq!(f.pool.report_limit_reached(None, Some(NOW + 100)), None);
}

#[tokio::test]
async fn auto_switch_off_keeps_the_account_even_on_a_limit() {
    let f = fixture().await;
    f.pool.active_auth_manager().expect("manager");
    let mut policy = f.pool.policy();
    policy.auto_switch = false;
    f.pool.set_policy(policy).expect("policy");
    assert_eq!(f.pool.report_limit_reached(None, Some(NOW + 100)), None);
}

#[tokio::test]
async fn api_keys_are_stored_listed_and_used_after_chatgpt_is_exhausted() {
    let f = fixture().await;
    let view = f
        .pool
        .add_api_key("予備キー", "sk-test-FAKE-pool-0001", Some(1))
        .await
        .expect("add key");
    assert_eq!(view.kind, AccountKind::ApiKey);
    assert!(view.usage.is_none());
    assert!(
        f.pool
            .add_api_key("別名", "sk-test-FAKE-pool-0001", None)
            .await
            .is_err(),
        "the same key is not added twice"
    );

    // Every ChatGPT account over the threshold: the key takes over.
    for id in ["default", "chatgpt-aaaa", "chatgpt-bbbb"] {
        f.pool.record_usage(id, usage(97.0, 50.0), false);
    }
    f.pool.active_auth_manager().expect("manager");
    f.pool.select_for_sampling(None, false);
    assert_eq!(in_use(&f.pool), "予備キー");
    let manager = f.pool.active_auth_manager().expect("manager");
    let auth = manager.auth().await.expect("key auth");
    assert_eq!(auth.api_key(), Some("sk-test-FAKE-pool-0001"));

    // A reset ChatGPT window brings the pool back to ChatGPT.
    f.pool.record_usage("chatgpt-aaaa", usage(5.0, 5.0), false);
    f.pool.select_for_sampling(None, true);
    assert_eq!(in_use(&f.pool), "サブ");
}

#[tokio::test]
async fn entries_survive_a_restart_and_secrets_stay_out_of_views() {
    let f = fixture().await;
    f.pool
        .add_api_key("予備キー", "sk-test-FAKE-pool-0002", Some(7))
        .await
        .expect("add key");
    let id = api_key_account_id("sk-test-FAKE-pool-0002");
    f.pool.rename(&id, "改名").expect("rename");
    f.pool.set_priority(&id, 9).expect("priority");
    f.pool.set_enabled(&id, false).expect("disable");
    let mut policy = f.pool.policy();
    policy.threshold_percent = 80.0;
    f.pool.set_policy(policy.clone()).expect("policy");

    let primary = AuthManager::from_auth_for_testing_with_home(
        CodexAuth::create_dummy_chatgpt_auth_for_testing(),
        f.home.path().to_path_buf(),
    );
    let reopened =
        AccountPool::open_with_clock(env(f.home.path()), primary, Arc::new(|| NOW)).await;
    assert_eq!(reopened.policy(), policy);
    let view = reopened
        .list()
        .into_iter()
        .find(|v| v.id == id)
        .expect("key is back");
    assert_eq!(view.alias, "改名");
    assert_eq!(view.priority, 9);
    assert_eq!(view.state, AccountState::Disabled);

    let shown = format!("{:?}", reopened.list());
    assert!(!shown.contains("sk-test-FAKE"), "{shown}");
    assert!(!shown.contains('@'), "no e-mail address: {shown}");
    let on_disk =
        std::fs::read_to_string(f.home.path().join("accounts").join(&id).join("meta.json"))
            .expect("meta");
    assert!(!on_disk.contains("sk-test-FAKE"));

    reopened.remove(&id).await.expect("remove");
    assert!(reopened.list().iter().all(|v| v.id != id));
    assert!(!f.home.path().join("accounts").join(&id).exists());
}

#[tokio::test]
async fn a_manual_choice_holds_and_is_announced() {
    let f = fixture().await;
    f.pool.active_auth_manager().expect("manager");
    let event = f.pool.use_account("chatgpt-bbbb", Some("t")).expect("use");
    assert_eq!(event.reason, SwitchReason::Manual);
    assert_eq!(event.to_alias, "予備");
    f.pool.select_for_sampling(None, true);
    assert_eq!(in_use(&f.pool), "予備");
    assert!(f.pool.use_account("nope", None).is_err());
    f.pool.set_enabled("chatgpt-bbbb", false).expect("disable");
    assert!(f.pool.use_account("chatgpt-bbbb", None).is_err());
    f.pool.active_auth_manager().expect("manager");
    assert_eq!(in_use(&f.pool), "メイン");
}

#[tokio::test]
async fn removing_the_default_login_drops_it_from_the_pool() {
    let f = fixture().await;
    f.pool.active_auth_manager().expect("manager");
    // The dummy login has no stored file, so logout only clears the cache.
    f.pool.remove(DEFAULT_ID).await.expect("remove default");
    assert!(f.pool.list().iter().all(|v| v.id != DEFAULT_ID));
}

#[tokio::test]
async fn invalid_policy_values_are_rejected() {
    let f = fixture().await;
    let mut policy = f.pool.policy();
    policy.threshold_percent = 120.0;
    assert!(f.pool.set_policy(policy).is_err());
}

#[tokio::test]
async fn a_manual_choice_survives_a_restart() {
    let f = fixture().await;
    f.pool
        .add_api_key("予備キー", "sk-test-FAKE-pool-0003", None)
        .await
        .expect("add key");
    let id = api_key_account_id("sk-test-FAKE-pool-0003");
    f.pool.use_account(&id, None).expect("use");
    let primary = AuthManager::from_auth_for_testing_with_home(
        CodexAuth::create_dummy_chatgpt_auth_for_testing(),
        f.home.path().to_path_buf(),
    );
    let reopened =
        AccountPool::open_with_clock(env(f.home.path()), primary, Arc::new(|| NOW)).await;
    reopened.active_auth_manager().expect("manager");
    assert_eq!(in_use(&reopened), "予備キー");
}

#[tokio::test]
async fn the_claude_group_is_its_own_pool_with_failover_and_survives_a_restart() {
    use crate::claude_oauth::ClaudeCredentials;
    let home = tempfile::tempdir().expect("tempdir");
    let pool = AccountPool::open_group(env(home.path()), "claude", "accounts-claude").await;
    let creds = |n: u32| ClaudeCredentials {
        access: format!("access-FAKE-{n}"),
        refresh: format!("refresh-FAKE-{n}"),
        expires_at_ms: i64::MAX,
        account_uuid: Some(format!("uuid-{n}")),
        plan: Some("max".to_string()),
    };
    let one = pool.add_claude_oauth("クロード1", creds(1)).await.expect("add one");
    let two = pool.add_claude_oauth("クロード2", creds(2)).await.expect("add two");
    assert_eq!(one.kind, AccountKind::ClaudeOauth);
    assert_eq!(one.provider, "claude");
    assert_eq!(pool.group(), "claude");
    assert!(pool.list().iter().all(|v| !v.is_default));

    let first = pool.active_claude_manager().expect("a manager");
    let first_token = first.access_token().await.expect("token");
    let first_alias = in_use(&pool);
    // A usage limit moves to the other sign-in.
    let event = pool
        .report_limit_reached(Some("t"), Some(i64::MAX / 2))
        .expect("another sign-in takes over");
    assert_ne!(event.to_alias, first_alias);
    let second_token = pool
        .active_claude_manager()
        .expect("a manager")
        .access_token()
        .await
        .expect("token");
    assert_ne!(first_token, second_token);

    // Credentials are in claude.json beside the entry, not in the metadata or the views.
    let dir = home.path().join("accounts-claude").join(&one.id);
    assert!(dir.join("claude.json").exists());
    let meta = std::fs::read_to_string(dir.join("meta.json")).expect("meta");
    assert!(!meta.contains("access-FAKE") && !meta.contains("refresh-FAKE"));
    assert!(!format!("{:?}", pool.list()).contains("access-FAKE"));

    let reopened = AccountPool::open_group(env(home.path()), "claude", "accounts-claude").await;
    assert_eq!(reopened.list().len(), 2);
    assert!(reopened.list().iter().any(|v| v.id == two.id));
    let token = reopened
        .active_claude_manager()
        .expect("a manager")
        .access_token()
        .await
        .expect("token");
    assert!(token.starts_with("access-FAKE-"));
}

#[tokio::test]
async fn re_authenticating_requires_the_same_account_and_keeps_its_settings() {
    use crate::claude_oauth::ClaudeCredentials;
    let home = tempfile::tempdir().expect("tempdir");
    let pool = AccountPool::open_group(env(home.path()), "claude", "accounts-claude").await;
    let creds = |n: u32| ClaudeCredentials {
        access: format!("access-FAKE-{n}"),
        refresh: format!("refresh-FAKE-{n}"),
        expires_at_ms: i64::MAX,
        account_uuid: Some(format!("uuid-{n}")),
        plan: None,
    };
    let one = pool.add_claude_oauth("クロード", creds(1)).await.expect("add");
    pool.set_priority(&one.id, 7).expect("priority");

    // Signing in as someone else is refused and changes nothing.
    assert!(
        pool.add_claude_oauth_expecting("別人", creds(2), Some(one.id.as_str()))
            .await
            .is_err()
    );
    assert_eq!(pool.list().len(), 1);

    let renewed = ClaudeCredentials {
        access: "access-FAKE-NEW".to_string(),
        ..creds(1)
    };
    let again = pool
        .add_claude_oauth_expecting("無視される別名", renewed, Some(one.id.as_str()))
        .await
        .expect("the same account");
    assert_eq!(again.id, one.id);
    assert_eq!(again.alias, "クロード");
    assert_eq!(again.priority, 7);
    assert_eq!(pool.list().len(), 1);
    let token = pool
        .active_claude_manager()
        .expect("a manager")
        .access_token()
        .await
        .expect("token");
    assert_eq!(token, "access-FAKE-NEW");
}
