//! Runs the server as a standalone process on the loopback interface.
//!
//! Configuration lives in `~/.inocode/settings.json`, beside the thread
//! database (docs/evidence/account-pool.md): the providers, the default
//! model, the helper roles and the server's port and sandbox. `--port N`
//! and `--sandbox on|off` override the `server` section for one run. The
//! listening address is always `127.0.0.1`, so the server is not reachable
//! from the network, unless `server.remote` turns on remote access: then it
//! also takes WebSocket connections on every address at `server.remote_port`
//! (docs/evidence/remote-access.md). Without a usable provider the server
//! still starts and answers `ping` and `echo`, while `chat` reports that
//! no provider is configured. A settings file or database that cannot be
//! read stops the start with its error, and so does a second server on the
//! same database.
//!
//! Each start writes a new random token to `~/.inocode/server-token`; every
//! connection must send it first (docs/evidence/server-auth.md).
//!
//! `login claude|gpt|muse-code|deepai` runs the interactive subscription/OAuth
//! login instead of serving, storing the credential as an account of the
//! kind (a new one, or the one that signs in the same person) and adding
//! the provider to the settings. `key set <provider-id> [--account <alias>]`
//! stores the API key of a static-key provider, read from standard input:
//! its default key, or the key of the account `alias` names (a new one of
//! the provider's key pool when none has that alias). `models
//! [provider-id]` lists a provider's models; `usage` prints the default
//! account's usage (rate-limit windows or the DeepAI Theos allowance).
//!
//! Shell commands are confined to writing in the project folder and a
//! temporary folder unless the settings or `--sandbox off` turn that off.
//! `sandbox-run` is the confinement runner the server starts itself as, not
//! a user command.

use std::env;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;

use inocode_core::deepai;
use inocode_core::oauth::{self, FlowEndpoints, MuseEndpoints, Store};
use inocode_core::sandbox;
use inocode_core::settings::{self, Kind};
use inocode_core::Core;
use tokio::net::TcpListener;

/// What the command line asked for besides the command.
#[derive(Default)]
struct Flags {
    port: Option<u16>,
    sandbox: Option<bool>,
}

fn main() -> io::Result<()> {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some(sandbox::RUNNER_ARGUMENT) {
        std::process::exit(sandbox::run(&args[1..]));
    }
    let (words, flags) = split_flags(args)?;
    // A file that cannot be read is not replaced by the defaults: they could
    // choose another credential store or turn the sandbox on or off.
    let server = server_settings()?;
    sandbox::configure(flags.sandbox.or(server.sandbox).unwrap_or(true), None);
    match server.credential_store() {
        Ok(mode) => inocode_core::secrets::set_mode(mode),
        Err(error) => {
            return Err(invalid(format!(
                "settings.json: server.credentials: {error}"
            )))
        }
    }
    unlock_credentials()?;
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(command(words, flags, server))
}

/// Opens the credential store once, asking for its passphrase at the
/// terminal when it is sealed with one, so that later reads do not fail
/// for want of it. The passphrase is never read from the environment.
fn unlock_credentials() -> io::Result<()> {
    let path = Store::default_path().map_err(io::Error::other)?;
    let Err(error) = Store::load(path.clone()) else {
        return Ok(());
    };
    if !error.contains("locked") {
        return Err(io::Error::other(error));
    }
    for _ in 0..3 {
        eprint!("Passphrase for the credential store: ");
        let passphrase = inocode_core::hidden_input::read_line_hidden()?;
        eprintln!();
        inocode_core::secrets::unlock(&passphrase).map_err(io::Error::other)?;
        match Store::load(path.clone()) {
            Ok(_) => return Ok(()),
            Err(error) => eprintln!("{error}"),
        }
    }
    inocode_core::secrets::lock();
    Err(io::Error::other("the credential store stays locked"))
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

fn split_flags(args: Vec<String>) -> io::Result<(Vec<String>, Flags)> {
    let mut words = Vec::new();
    let mut flags = Flags::default();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--port" => {
                let value = args
                    .next()
                    .ok_or_else(|| invalid("--port needs a number"))?;
                flags.port = Some(
                    value
                        .parse()
                        .ok()
                        .filter(|port| *port != 0)
                        .ok_or_else(|| invalid("--port must be a port number from 1 to 65535"))?,
                );
            }
            "--sandbox" => {
                flags.sandbox = match args.next().as_deref() {
                    Some("on") => Some(true),
                    Some("off") => Some(false),
                    _ => return Err(invalid("--sandbox must be followed by on or off")),
                };
            }
            _ => words.push(arg),
        }
    }
    Ok((words, flags))
}

/// `~/.inocode`, which holds the credential store, the database and the
/// settings.
fn home_folder() -> io::Result<PathBuf> {
    let auth = Store::default_path().map_err(io::Error::other)?;
    Ok(auth
        .parent()
        .ok_or_else(|| io::Error::other("no folder for the credential store"))?
        .to_path_buf())
}

fn settings_path() -> io::Result<PathBuf> {
    Ok(home_folder()?.join("settings.json"))
}

fn server_settings() -> io::Result<settings::Server> {
    let path = settings_path()?;
    let invalid = |error: String| invalid(format!("{}: {error}", path.display()));
    let value = settings::read_blocking(&path).map_err(invalid)?;
    Ok(settings::Settings::from_value(&value)
        .map_err(invalid)?
        .server)
}

async fn command(words: Vec<String>, flags: Flags, server: settings::Server) -> io::Result<()> {
    let mut words = words.into_iter();
    match words.next().as_deref() {
        Some("login") => login_command(words.next().as_deref(), words.next().as_deref()).await,
        Some("key") => {
            let rest: Vec<String> = words.collect();
            match rest
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>()
                .as_slice()
            {
                ["set", provider] => key_command(provider, None).await,
                ["set", provider, "--account", alias] => key_command(provider, Some(alias)).await,
                _ => Err(invalid(
                    "usage: inocode-server key set <provider-id> [--account <alias>]",
                )),
            }
        }
        Some("models") => models_command(words.next().as_deref()).await,
        Some("usage") => usage_command().await,
        None | Some("serve") => {
            let port = flags.port.or(server.port).unwrap_or(settings::DEFAULT_PORT);
            serve(port, flags.sandbox, &server).await
        }
        Some(other) => Err(invalid(format!(
            "unknown command \"{other}\"; expected serve, login, key set, models or usage"
        ))),
    }
}

/// The core the commands share: the thread store and the settings beside
/// it in `~/.inocode`.
async fn open_core() -> io::Result<Core> {
    let folder = home_folder()?;
    std::fs::create_dir_all(&folder)?;
    let store = inocode_core::store::Store::open(folder.join("inocode.db"))
        .await
        .map_err(|error| io::Error::other(error.message))?;
    Ok(Core::new().with_store(store))
}

/// Gives the settings their providers the first time, from the stored
/// credentials.
async fn prepare_settings() -> io::Result<()> {
    let credentials =
        Store::load(Store::default_path().map_err(io::Error::other)?).map_err(io::Error::other)?;
    if settings::create_defaults(&settings_path()?, &credentials)
        .await
        .map_err(io::Error::other)?
    {
        println!("providers added to {:?}", settings_path()?);
    }
    Ok(())
}

/// Lists the models of `provider`, or of the default provider. Prints
/// model ids only; credentials are never displayed.
async fn models_command(provider: Option<&str>) -> io::Result<()> {
    prepare_settings().await?;
    let models = open_core()
        .await?
        .models(provider, None, false)
        .await
        .map_err(|error| io::Error::other(error.message))?;
    for model in &models {
        match &model.display_name {
            Some(name) => println!("{} — {name}", model.id),
            None => println!("{}", model.id),
        }
    }
    println!("{} models", models.len());
    Ok(())
}

/// Prints the default account's usage as JSON. Credentials are never
/// displayed.
async fn usage_command() -> io::Result<()> {
    prepare_settings().await?;
    let usage = open_core()
        .await?
        .usage()
        .await
        .map_err(|error| io::Error::other(error.message))?;
    println!(
        "{}",
        serde_json::to_string_pretty(&usage).map_err(io::Error::other)?
    );
    Ok(())
}

async fn serve(port: u16, sandbox_flag: Option<bool>, server: &settings::Server) -> io::Result<()> {
    let folder = home_folder()?;
    std::fs::create_dir_all(&folder)?;
    // One server per database: turns, held calls and the settings are kept
    // by one process. Held until the process ends.
    let _database_lock = lock_database(&folder.join("inocode.db.lock"))?;
    prepare_settings().await?;
    let core = open_core()
        .await
        .map_err(|error| io::Error::other(format!("the thread database: {error}")))?;
    if let Some(store) = core.thread_store() {
        store
            .expire_elicitations()
            .await
            .map_err(|error| io::Error::other(error.message))?;
    }
    // Commands confined by the sandbox may not read the credentials or the
    // token, where the OS lets that be enforced.
    if let Err(error) = sandbox::hide(&folder) {
        eprintln!(
            "sandbox: {} stays readable by commands: {error}",
            folder.display()
        );
    }
    let listener = TcpListener::bind(("127.0.0.1", port)).await?;
    let address = listener.local_addr()?;
    let remote = match server.remote_address() {
        Some(remote) if remote.port() == address.port() => {
            return Err(invalid(format!(
                "server.remote_port {} is the loopback port; choose another",
                remote.port()
            )))
        }
        Some(remote) => Some(TcpListener::bind(remote).await.map_err(|error| {
            io::Error::new(error.kind(), format!("remote access on {remote}: {error}"))
        })?),
        None => None,
    };
    let token = write_token(&folder.join(TOKEN_FILE))?;
    let remote_port = match &remote {
        Some(listener) => Some(listener.local_addr()?.port()),
        None => None,
    };
    let core = Arc::new(core.with_started(inocode_core::Started {
        port: Some(address.port()),
        sandbox_flag,
        remote_port,
        remote_password_hash: server.remote_password_hash.clone(),
    }));
    println!(
        "inocode-server listening on {address}; clients authenticate with the token in {}",
        folder.join(TOKEN_FILE).display()
    );
    let stopped = || async {
        let _ = tokio::signal::ctrl_c().await;
    };
    let local = inocode_server::run(listener, Arc::clone(&core), token, stopped());
    let Some(remote) = remote else {
        return local.await;
    };
    let remote_address = remote.local_addr()?;
    println!(
        "remote access listening on {remote_address} (WebSocket); {}",
        if server.remote_password_hash.is_some() {
            "clients authenticate with the remote access password"
        } else {
            "no password is set"
        }
    );
    if server.remote_password_hash.is_none() {
        eprintln!(
            "WARNING: remote access is on without a password: anyone who can reach port \
             {} can operate inocode-server. Set server.remote_password, or let only \
             Tailscale or a VPN reach the port.",
            remote_address.port()
        );
    }
    let remote =
        inocode_server::run_remote(remote, core, server.remote_password_hash.clone(), stopped());
    tokio::try_join!(local, remote).map(|_| ())
}

/// The file the token of this start is written to, beside the settings.
const TOKEN_FILE: &str = "server-token";

/// Takes the lock that keeps a second server off the same database.
fn lock_database(path: &std::path::Path) -> io::Result<std::fs::File> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(std::fs::TryLockError::WouldBlock) => Err(io::Error::other(format!(
            "another inocode-server uses this database ({} is locked)",
            path.display()
        ))),
        Err(std::fs::TryLockError::Error(error)) => Err(error),
    }
}

/// Makes a new random token and writes it to `path`, readable only by this
/// user (the user profile's permissions on Windows), replacing the token of
/// an earlier start.
fn write_token(path: &std::path::Path) -> io::Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes).map_err(io::Error::other)?;
    let token: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    let temporary = path.with_extension(format!("{}.tmp", std::process::id()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    {
        use std::io::Write;
        let mut file = options.open(&temporary)?;
        file.write_all(token.as_bytes())?;
        file.sync_all()?;
    }
    std::fs::rename(&temporary, path)?;
    Ok(token)
}

/// Stores the API key of the provider `provider`, typed without echo or
/// piped in: its default key, or with `alias` the key of the account it
/// names by id or alias, a new account of the key pool when none does. The
/// key is never printed.
async fn key_command(provider: &str, alias: Option<&str>) -> io::Result<()> {
    if let Some(alias) = alias {
        let length = alias.chars().count();
        if !(1..=64).contains(&length) || alias.chars().any(char::is_control) {
            return Err(invalid(
                "an account alias has 1 to 64 characters and no control characters",
            ));
        }
    }
    let settings = settings::Settings::load(&settings_path()?)
        .await
        .map_err(io::Error::other)?;
    if let Some(configured) = settings.provider(provider) {
        if !configured.kind.uses_api_key() {
            return Err(invalid(format!(
                "the provider {provider} signs in with `inocode-server login`; it has no API key"
            )));
        }
    }
    println!("API key for {provider}:");
    let key = inocode_core::hidden_input::read_line_hidden()?;
    let key = key.trim();
    if key.is_empty() {
        return Err(invalid("no key was given"));
    }
    let mut store =
        Store::load(Store::default_path().map_err(io::Error::other)?).map_err(io::Error::other)?;
    let stored = store
        .set_key(&settings::api_key_name(provider), alias, key)
        .map_err(io::Error::other)?;
    println!(
        "key for {provider} {} as the account {} in {:?}",
        if stored.added { "stored" } else { "replaced" },
        stored.account,
        store.path()
    );
    if settings.provider(provider).is_none() {
        println!(
            "no provider {provider} is configured yet; add it with providers/add \
             {{\"id\": \"{provider}\", \"kind\": \"openai\" | \"anthropic\" | \"responses\"}}"
        );
    }
    Ok(())
}

/// Runs one login flow and stores its credential. Tokens and keys are never
/// printed; only provider names and the storage path are.
async fn login_command(provider: Option<&str>, option: Option<&str>) -> io::Result<()> {
    // Read first, so a store that cannot be used stops the login before
    // the person approves anything.
    let path = Store::default_path().map_err(io::Error::other)?;
    Store::load(path.clone()).map_err(io::Error::other)?;
    let (kind, credential) = match provider {
        Some("claude") => {
            let code_mode = option == Some("--code");
            let endpoints = FlowEndpoints::anthropic();
            let start = oauth::claude_login_start(&endpoints, code_mode);
            println!("Open this URL and approve access:\n{}", start.url);
            let pasted = if code_mode {
                // Read before awaiting: the stdin lock must not be held across
                // the exchange, and it is not `Send` on Windows.
                println!("Paste the code Anthropic displays (it looks like code#state):");
                let mut line = String::new();
                io::stdin().read_line(&mut line)?;
                Some(line)
            } else {
                None
            };
            (
                Kind::ClaudeOauth,
                oauth::claude_login_finish(&endpoints, start, pasted)
                    .await
                    .map_err(login_error)?,
            )
        }
        Some("gpt") => {
            let endpoints = FlowEndpoints::chatgpt();
            let start = oauth::gpt_login_start(&endpoints);
            println!("Open this URL and approve access:\n{}", start.url);
            (
                Kind::GptOauth,
                oauth::gpt_login_finish(&endpoints, start, None)
                    .await
                    .map_err(login_error)?,
            )
        }
        Some("muse-code" | "muse") => (
            Kind::MuseCode,
            oauth::login_muse(&MuseEndpoints::default())
                .await
                .map_err(login_error)?,
        ),
        Some("deepai") => {
            let endpoints = deepai::SignInEndpoints::default();
            let start = deepai::sign_in_start(&endpoints);
            println!(
                "Open this URL and sign in to DeepAI Code:
{}",
                start.url
            );
            (
                Kind::DeepAi,
                deepai::sign_in_finish(&endpoints, start)
                    .await
                    .map_err(login_error)?,
            )
        }
        Some(other) => {
            return Err(invalid(format!(
                "unknown login target \"{other}\"; expected claude, gpt, muse-code, or deepai"
            )));
        }
        None => {
            return Err(invalid(
                "usage: inocode-server login claude|gpt|muse-code|deepai",
            ));
        }
    };
    let name = kind.auth().store_name();
    let completed =
        inocode_core::accounts::complete_login(&path, Some(&settings_path()?), kind, credential)
            .await
            .map_err(io::Error::other)?;
    let account = &completed.signed_in.account;
    if completed.signed_in.added {
        println!("account {account} of {name} added to {path:?}");
    } else {
        println!("account {account} of {name} signed in again; its credential was replaced");
    }
    if completed.provider_added {
        println!("provider {} added to {:?}", kind.name(), settings_path()?);
    }
    let accounts = Store::load(path).map_err(io::Error::other)?.accounts(name);
    if accounts.len() > 1 {
        println!(
            "{name} has {} accounts; the provider's pool chooses among them for each \
             request (/accounts status shows how), unless /accounts use prefers one",
            accounts.len()
        );
    }
    Ok(())
}

fn login_error(error: String) -> io::Error {
    io::Error::other(error)
}
