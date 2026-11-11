//! `inocode accounts list|add|remove|use|status`: manage the account pool from the shell.
//!
//! The pool lives in `INOCODE_HOME/accounts/` (see `codex_login::account_pool`). These commands
//! only edit that store; a running app-server picks the changes up when it restarts.

use clap::Args;
use clap::Parser;
use clap::Subcommand;
use codex_core::config::Config;
use codex_login::AuthManager;
use codex_login::CLIENT_ID;
use codex_login::ServerOptions;
use codex_login::account_pool::AccountKind;
use codex_login::account_pool::AccountPool;
use codex_login::account_pool::AccountState;
use codex_login::account_pool::AccountView;
use codex_login::account_pool::SwitchReason;
use codex_login::run_device_code_login;
use codex_login::run_login_server;
use codex_utils_cli::CliConfigOverrides;
use std::sync::Arc;

#[derive(Debug, Parser)]
pub struct AccountsCommand {
    #[clap(skip)]
    pub config_overrides: CliConfigOverrides,

    #[command(subcommand)]
    pub action: AccountsAction,
}

#[derive(Debug, Subcommand)]
pub enum AccountsAction {
    /// List the accounts and API keys in the pool.
    List,

    /// Refresh each account's usage, then show the pool and its policy.
    Status,

    /// Add a ChatGPT account (sign-in in the browser) or an API key (read from stdin).
    Add(AddArgs),

    /// Remove an account or key (by id or alias).
    Remove {
        /// Id or alias.
        account: String,
    },

    /// Switch to an account or key by hand (by id or alias).
    Use {
        /// Id or alias.
        account: String,
    },
}

#[derive(Debug, Args)]
pub struct AddArgs {
    /// Name to show instead of an e-mail address.
    pub alias: String,

    /// Add an API key read from stdin (e.g. `printenv OPENAI_API_KEY | inocode accounts add 予備 --with-api-key`).
    #[arg(long = "with-api-key")]
    pub with_api_key: bool,

    /// Sign in with a device code instead of the browser.
    #[arg(long = "device-auth")]
    pub use_device_code: bool,

    /// Larger is used earlier.
    #[arg(long)]
    pub priority: Option<i32>,

    /// `openai` (default) or `claude`.
    #[arg(long, default_value = "openai")]
    pub provider: String,

    /// Claude sign-in: paste the code Anthropic shows instead of using the local listener.
    #[arg(long = "code")]
    pub paste_code: bool,
}

pub async fn run_accounts(command: AccountsCommand) -> anyhow::Result<()> {
    let cli_overrides = command
        .config_overrides
        .parse_overrides()
        .map_err(|err| anyhow::anyhow!("Error parsing -c overrides: {err}"))?;
    let config = Config::load_with_cli_overrides(cli_overrides).await?;
    let primary = AuthManager::shared_from_config(&config, /*enable_codex_api_key_env*/ false).await?;
    let pool = codex_app_server::open_account_pool(&config, primary).await;
    let claude = codex_app_server::open_claude_account_pool(&config).await;

    match command.action {
        AccountsAction::List => print_list(&[&pool, &claude]),
        AccountsAction::Status => {
            pool.refresh_usage().await;
            print_list(&[&pool, &claude]);
            let policy = pool.policy();
            println!();
            println!(
                "policy: threshold {}%  window {:?}  selection {:?}  api keys {:?}  cooldown {}s  auto-switch {}",
                policy.threshold_percent,
                policy.window,
                policy.selection,
                policy.api_key_order,
                policy.default_cooldown_seconds,
                if policy.auto_switch { "on" } else { "off" },
            );
        }
        AccountsAction::Add(args) => match args.provider.as_str() {
            "openai" => add(&pool, &config, args).await?,
            "claude" => add_claude(&claude, &config, args).await?,
            other => anyhow::bail!("unknown provider `{other}` (openai or claude)"),
        },
        AccountsAction::Remove { account } => {
            let (owner, view) = find(&[&pool, &claude], &account)?;
            owner.remove(&view.id).await?;
            println!("Removed {}", view.alias);
        }
        AccountsAction::Use { account } => {
            let (owner, view) = find(&[&pool, &claude], &account)?;
            let event = owner.use_account(&view.id, /*thread_id*/ None)?;
            debug_assert_eq!(event.reason, SwitchReason::Manual);
            println!("Now using {}", view.alias);
        }
    }
    Ok(())
}

fn find<'a>(
    pools: &[&'a Arc<AccountPool>],
    account: &str,
) -> anyhow::Result<(&'a Arc<AccountPool>, AccountView)> {
    for by_id in [true, false] {
        for pool in pools {
            if let Some(view) = pool
                .list()
                .into_iter()
                .find(|view| if by_id { view.id == account } else { view.alias == account })
            {
                return Ok((*pool, view));
            }
        }
    }
    anyhow::bail!("no account with id or alias `{account}`")
}

/// Adds a Claude sign-in (OAuth) or Claude API key to the `claude` group.
async fn add_claude(pool: &Arc<AccountPool>, config: &Config, args: AddArgs) -> anyhow::Result<()> {
    if args.with_api_key {
        let key = crate::read_api_key_from_stdin();
        let view = pool.add_api_key(&args.alias, &key, args.priority).await?;
        println!("Added Claude API key {} ({})", view.alias, view.id);
        return Ok(());
    }
    let pending = codex_login::claude_oauth::begin(args.paste_code);
    eprintln!("Open this URL in a browser and sign in:\n{}", pending.url);
    let code = if args.paste_code {
        eprintln!("Then paste the code Anthropic shows (code#state) and press Enter:");
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        line
    } else {
        eprintln!("Waiting for the browser to return to localhost:{} ...", codex_login::claude_oauth::CALLBACK_PORT);
        codex_login::claude_oauth::wait_for_callback(
            pending.state.clone(),
            std::time::Duration::from_secs(600),
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
        )
        .await?
    };
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX));
    let credentials = codex_login::claude_oauth::exchange(
        &pending,
        &code,
        &config.auth_route_config(),
        now_ms,
    )
    .await?;
    let view = pool.add_claude_oauth(&args.alias, credentials).await?;
    if let Some(priority) = args.priority {
        pool.set_priority(&view.id, priority)?;
    }
    println!("Added Claude sign-in {} ({})", view.alias, view.id);
    Ok(())
}

async fn add(pool: &Arc<AccountPool>, config: &Config, args: AddArgs) -> anyhow::Result<()> {
    if args.with_api_key {
        let key = crate::read_api_key_from_stdin();
        let view = pool.add_api_key(&args.alias, &key, args.priority).await?;
        println!("Added API key {} ({})", view.alias, view.id);
        return Ok(());
    }
    // The sign-in writes into a pending directory, so the current login is never replaced.
    let login_id = format!(
        "cli-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos())
    );
    let env = pool.env().clone();
    let opts = ServerOptions {
        open_browser: !args.use_device_code,
        ..ServerOptions::new(
            pool.store().pending_dir(&login_id),
            CLIENT_ID.to_string(),
            /*forced_chatgpt_workspace_id*/ None,
            env.mode,
            env.keyring,
            config.auth_route_config(),
        )
    };
    let result = if args.use_device_code {
        run_device_code_login(opts).await
    } else {
        let server = run_login_server(opts)?;
        eprintln!(
            "Open this URL to sign in (it opens automatically when possible):\n{}",
            server.auth_url
        );
        server.block_until_done().await
    };
    if let Err(err) = result {
        pool.discard_pending_login(&login_id);
        anyhow::bail!("sign-in failed: {err}");
    }
    match pool.finish_chatgpt_login(&login_id, &args.alias).await {
        Ok(view) => {
            if let Some(priority) = args.priority {
                pool.set_priority(&view.id, priority)?;
            }
            println!("Added {} ({})", view.alias, view.id);
            Ok(())
        }
        Err(err) => {
            pool.discard_pending_login(&login_id);
            Err(err.into())
        }
    }
}

fn print_list(pools: &[&Arc<AccountPool>]) {
    let list: Vec<AccountView> = pools.iter().flat_map(|pool| pool.list()).collect();
    if list.is_empty() {
        println!("No accounts. Add one with `inocode accounts add <alias>` or sign in with `inocode login`.");
        return;
    }
    for view in list {
        let marker = if view.in_use { "*" } else { " " };
        let kind = match view.kind {
            AccountKind::Chatgpt => view.plan.clone().unwrap_or_else(|| "chatgpt".to_string()),
            AccountKind::ApiKey => "api key".to_string(),
            AccountKind::ClaudeOauth => view.plan.clone().unwrap_or_else(|| "claude".to_string()),
        };
        let usage = view
            .usage
            .as_ref()
            .map(|usage| {
                let window = |w: &Option<codex_login::account_pool::UsageWindow>| {
                    w.as_ref()
                        .map_or("-".to_string(), |w| format!("{:.0}%", w.used_percent))
                };
                format!("5h {}  week {}", window(&usage.five_hour), window(&usage.weekly))
            })
            .unwrap_or_default();
        let state = match view.state {
            AccountState::Active => "active".to_string(),
            AccountState::Disabled => "disabled".to_string(),
            AccountState::Cooldown => view
                .cooldown_until
                .map_or("cooldown".to_string(), |until| format!("cooldown until {until}")),
        };
        let default = if view.is_default { " [default login]" } else { "" };
        println!(
            "{marker} {}  {}  [{}] ({kind})  {usage}  {state}  priority {}{default}",
            view.id, view.alias, view.provider, view.priority
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_subcommands_parse() {
        let list = AccountsCommand::try_parse_from(["accounts", "list"]).expect("list");
        assert!(matches!(list.action, AccountsAction::List));
        let add = AccountsCommand::try_parse_from([
            "accounts",
            "add",
            "予備キー",
            "--with-api-key",
            "--priority",
            "3",
        ])
        .expect("add");
        let AccountsAction::Add(args) = add.action else {
            panic!("expected add");
        };
        assert_eq!(args.alias, "予備キー");
        assert!(args.with_api_key);
        assert_eq!(args.priority, Some(3));
        let used = AccountsCommand::try_parse_from(["accounts", "use", "サブ"]).expect("use");
        assert!(matches!(used.action, AccountsAction::Use { .. }));
        assert!(AccountsCommand::try_parse_from(["accounts", "add"]).is_err());
    }
}
