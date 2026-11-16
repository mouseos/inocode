//! Runs the server as a standalone process on the loopback interface.
//!
//! The port defaults to 8787 and can be overridden with the `INOCODE_PORT`
//! environment variable. The listening address is always `127.0.0.1`, so the
//! server is not reachable from the network. The provider is configured from
//! `LOGFARE_API_KEY` and, optionally, `INOCODE_LLM_BASE_URL` and
//! `INOCODE_LLM_MODEL`; without a key the server still starts and answers
//! `ping` and `echo`, while `chat` reports that no provider is configured.
//!
//! `login claude|gpt|muse-code` runs the interactive subscription/OAuth login
//! instead of serving, storing the credential at `~/.inocode/auth.json`.
//! `models` lists the models the configured provider advertises.

use std::env;
use std::io;
use std::sync::Arc;

use inocode_core::oauth::{self, FlowEndpoints, MuseEndpoints, Store};
use inocode_core::{Core, Llm, LlmConfig};
use tokio::net::TcpListener;

const DEFAULT_PORT: u16 = 8787;

#[tokio::main]
async fn main() -> io::Result<()> {
    let mut args = env::args().skip(1);
    match args.next().as_deref() {
        Some("login") => login_command(args.next().as_deref(), args.next().as_deref()).await,
        Some("models") => models_command().await,
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
    let core = Arc::new(Core::from_env());
    inocode_server::run(listener, core, async {
        let _ = tokio::signal::ctrl_c().await;
    })
    .await
}

/// Runs one login flow and stores its credential. Tokens and keys are never
/// printed; only provider names and the storage path are.
async fn login_command(provider: Option<&str>, option: Option<&str>) -> io::Result<()> {
    let mut store = Store::load(Store::default_path().map_err(io::Error::other)?);
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
        Some("gpt") => (
            "gpt-oauth",
            oauth::login_gpt(&FlowEndpoints::chatgpt())
                .await
                .map_err(login_error)?,
        ),
        Some("muse-code" | "muse") => (
            "muse-code",
            oauth::login_muse(&MuseEndpoints::default())
                .await
                .map_err(login_error)?,
        ),
        Some(other) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("unknown login target \"{other}\"; expected claude, gpt, or muse-code"),
            ));
        }
        None => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "usage: inocode-server login claude|gpt|muse-code",
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
