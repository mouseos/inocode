//! Runs the server as a standalone process on the loopback interface.
//!
//! The port defaults to 8787 and can be overridden with the `INOCODE_PORT`
//! environment variable. The listening address is always `127.0.0.1`, so the
//! server is not reachable from the network. The provider is configured from
//! `LOGFARE_API_KEY` and, optionally, `INOCODE_LLM_BASE_URL` and
//! `INOCODE_LLM_MODEL`; without a key the server still starts and answers
//! `ping` and `echo`, while `chat` reports that no provider is configured.

use std::env;
use std::io;
use std::sync::Arc;

use inocode_core::Core;
use tokio::net::TcpListener;

const DEFAULT_PORT: u16 = 8787;

#[tokio::main]
async fn main() -> io::Result<()> {
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
