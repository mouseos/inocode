//! Runs the server as a standalone process on the loopback interface.
//!
//! The port defaults to 8787 and can be overridden with the `INOCODE_PORT`
//! environment variable. The listening address is always `127.0.0.1`, so the
//! server is not reachable from the network.

use std::env;
use std::io;

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
    inocode_server::run(listener, async {
        let _ = tokio::signal::ctrl_c().await;
    })
    .await
}
