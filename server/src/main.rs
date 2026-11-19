//! Runs the server as a standalone process on the loopback interface.
//!
//! The port defaults to 8787 and can be overridden with the `INOCODE_PORT`
//! environment variable. The listening address is always `127.0.0.1`, so the
//! server is not reachable from the network. The provider is configured from
//! `LOGFARE_API_KEY` and, optionally, `INOCODE_LLM_BASE_URL` and
//! `INOCODE_LLM_MODEL`; without a key the server still starts and answers
//! `ping` and `echo`, while `chat` reports that no provider is configured.
//!
//! `login claude|gpt|muse-code|deepai` runs the interactive subscription/OAuth login
//! instead of serving, storing the credential at `~/.inocode/auth.json`.
//! `models` lists the models the configured provider advertises; `usage`
//! prints the account's usage (rate-limit windows or the DeepAI Theos
//! allowance).
//!
//! Shell commands are confined to writing in the project folder and a
//! temporary folder; `INOCODE_SANDBOX=off` turns that off. `sandbox-run` is
//! the confinement runner the server starts itself as, not a user command.

use std::env;
use std::io;
use std::sync::Arc;

use inocode_core::deepai;
use inocode_core::oauth::{self, FlowEndpoints, MuseEndpoints, Store};
use inocode_core::sandbox;
use inocode_core::{Core, Llm, LlmConfig};
use tokio::net::TcpListener;

const DEFAULT_PORT: u16 = 8787;

fn main() -> io::Result<()> {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some(sandbox::RUNNER_ARGUMENT) {
        std::process::exit(sandbox::run(&args[1..]));
    }
    sandbox::configure(env::var("INOCODE_SANDBOX").as_deref() != Ok("off"), None);
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(command(args.into_iter()))
}

async fn command(mut args: impl Iterator<Item = String>) -> io::Result<()> {
    match args.next().as_deref() {
        Some("login") => login_command(args.next().as_deref(), args.next().as_deref()).await,
        Some("models") => models_command().await,
        Some("usage") => usage_command().await,
        _ => serve().await,
    }
}

/// Lists the models the configured provider advertises. Prints model ids
/// only; credentials are never displayed.
async fn models_command() -> io::Result<()> {
    let llm = Llm::new(LlmConfig::from_env().map_err(|error| io::Error::other(error.message))?)
        .map_err(|error| io::Error::other(error.message))?;
    let models = llm
        .models()
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

/// Prints the configured account's usage as JSON. Credentials are never
/// displayed.
async fn usage_command() -> io::Result<()> {
    let llm = Llm::new(LlmConfig::from_env().map_err(|error| io::Error::other(error.message))?)
        .map_err(|error| io::Error::other(error.message))?;
    let usage = llm
        .usage()
        .await
        .map_err(|error| io::Error::other(error.message))?;
    println!(
        "{}",
        serde_json::to_string_pretty(&usage).map_err(io::Error::other)?
    );
    Ok(())
}

async fn serve() -> io::Result<()> {
    let port = match env::var("INOCODE_PORT") {
        Ok(value) => value.parse().map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "INOCODE_PORT must be a port number",
            )
        })?,
        Err(_) => DEFAULT_PORT,
    };
    let listener = TcpListener::bind(("127.0.0.1", port)).await?;
    println!("inocode-server listening on {}", listener.local_addr()?);
    let mut core = Core::from_env();
    match thread_store_path() {
        Ok(path) => match inocode_core::store::Store::open(&path).await {
            Ok(store) => core = core.with_store(store),
            Err(error) => eprintln!("threads unavailable: {}", error.message),
        },
        Err(error) => eprintln!("threads unavailable: {error}"),
    }
    let core = Arc::new(core);
    inocode_server::run(listener, core, async {
        let _ = tokio::signal::ctrl_c().await;
    })
    .await
}

/// `~/.inocode/inocode.db`, beside `auth.json`.
fn thread_store_path() -> Result<std::path::PathBuf, String> {
    let auth = Store::default_path()?;
    let directory = auth.parent().ok_or("no directory for the thread store")?;
    std::fs::create_dir_all(directory).map_err(|error| error.to_string())?;
    Ok(directory.join("inocode.db"))
}

/// Runs one login flow and stores its credential. Tokens and keys are never
/// printed; only provider names and the storage path are.
async fn login_command(provider: Option<&str>, option: Option<&str>) -> io::Result<()> {
    let mut store =
        Store::load(Store::default_path().map_err(io::Error::other)?).map_err(io::Error::other)?;
    let (name, credential) = match provider {
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
                "claude-oauth",
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
                "gpt-oauth",
                oauth::gpt_login_finish(&endpoints, start, None)
                    .await
                    .map_err(login_error)?,
            )
        }
        Some("muse-code" | "muse") => (
            "muse-code",
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
                "deepai",
                deepai::sign_in_finish(&endpoints, start)
                    .await
                    .map_err(login_error)?,
            )
        }
        Some(other) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "unknown login target \"{other}\"; expected claude, gpt, muse-code, or deepai"
                ),
            ));
        }
        None => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "usage: inocode-server login claude|gpt|muse-code|deepai",
            ));
        }
    };
    store.save(name, credential).map_err(io::Error::other)?;
    println!("credential for {name} stored in {:?}", store.path());
    Ok(())
}

fn login_error(error: String) -> io::Error {
    io::Error::other(error)
}
