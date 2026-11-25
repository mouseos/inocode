//! The server process as it starts: what it refuses to start with, the
//! token it writes, and one server per database (docs/evidence/server-auth.md).

mod support;

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use serde_json::{json, Value};
use support::*;

/// A home folder of its own, with `settings` as its `.inocode/settings.json`.
fn home(name: &str, settings: &str) -> PathBuf {
    let home = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("inocode-start-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(home.join(".inocode")).expect("home");
    std::fs::write(home.join(".inocode").join("settings.json"), settings).expect("settings");
    home
}

fn server(home: &Path, port: u16) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_inocode-server"));
    command
        .args(["serve", "--port", &port.to_string()])
        .env("USERPROFILE", home)
        .env("HOME", home)
        .env_remove("HTTPS_PROXY")
        .env_remove("HTTP_PROXY")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind")
        .local_addr()
        .expect("address")
        .port()
}

/// Waits for the line that says the server listens.
fn until_listening(child: &mut Child) {
    let stdout = child.stdout.take().expect("stdout");
    let mut lines = BufReader::new(stdout).lines();
    loop {
        let line = lines
            .next()
            .expect("the server ended before it listened")
            .expect("line");
        if line.contains("listening on") {
            return;
        }
    }
}

struct Killed(Child);

impl Drop for Killed {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn a_broken_settings_file_stops_the_start_with_its_error() {
    let home = home("broken", "{ \"server\": { \"port\": ");
    let output = server(&home, free_port()).output().expect("run");
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("settings.json"), "{error}");
    assert!(!home.join(".inocode").join("server-token").exists());

    let home = home_with_unknown_field();
    let output = server(&home, free_port()).output().expect("run");
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("colleague"), "{error}");
}

fn home_with_unknown_field() -> PathBuf {
    home(
        "legacy",
        &json!({"server": {"credentials": "plaintext"}, "colleague": {"model": "m"}}).to_string(),
    )
}

#[tokio::test]
async fn the_started_server_asks_for_its_token_and_a_second_one_is_refused() {
    let home = home(
        "token",
        &json!({"server": {"credentials": "plaintext"}}).to_string(),
    );
    let port = free_port();
    let mut child = server(&home, port).spawn().expect("start");
    until_listening(&mut child);
    let _first = Killed(child);

    let token =
        std::fs::read_to_string(home.join(".inocode").join("server-token")).expect("token file");
    assert_eq!(token.len(), 64, "32 random bytes in hex");
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));

    let mut stranger = connect_unauthenticated(addr).await;
    let refused = call(&mut stranger, 1, json!({"id": 1, "method": "ping"})).await;
    assert_eq!(refused["error"]["code"], "unauthorized", "{refused}");

    let mut client = connect_unauthenticated(addr).await;
    let accepted = call(
        &mut client,
        1,
        json!({"id": 1, "method": "auth", "params": {"token": token}}),
    )
    .await;
    assert_eq!(accepted["result"]["authenticated"], true, "{accepted}");
    let settings = call(&mut client, 2, json!({"id": 2, "method": "settings/get"})).await;
    let effective: &Value = &settings["result"]["effective"];
    assert_eq!(effective["port"], port, "{settings}");
    assert_eq!(effective["credentials"], "plaintext");
    assert_eq!(
        settings["result"]["restart_required"], true,
        "the file names no port"
    );

    let second = server(&home, free_port()).output().expect("run");
    assert!(!second.status.success());
    let error = String::from_utf8_lossy(&second.stderr);
    assert!(error.contains("another inocode-server"), "{error}");
}
