//! Remote access over WebSocket: one frame of the TCP protocol per text
//! message, the password, the refused `Origin` header, and the settings
//! that keep the password's hash to themselves
//! (docs/evidence/remote-access.md).

mod support;

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use futures_util::{SinkExt, StreamExt};
use inocode_core::{Core, Llm, LlmConfig, ProviderKind};
use serde_json::{json, Value};
use support::*;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::{Error as WsError, Message};
use tokio_tungstenite::WebSocketStream;

type Remote = WebSocketStream<TcpStream>;

/// Starts remote access on an ephemeral loopback port. Dropping the sender
/// stops it.
async fn start_remote(
    core: Arc<Core>,
    password_hash: Option<String>,
) -> (SocketAddr, oneshot::Sender<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("address");
    let (stop, stopped) = oneshot::channel();
    tokio::spawn(inocode_server::run_remote(
        listener,
        core,
        password_hash,
        async {
            let _ = stopped.await;
        },
    ));
    (addr, stop)
}

async fn open(addr: SocketAddr) -> Remote {
    let stream = TcpStream::connect(addr).await.expect("connect");
    let (socket, _) = tokio_tungstenite::client_async(format!("ws://{addr}/"), stream)
        .await
        .expect("WebSocket handshake");
    socket
}

async fn send(socket: &mut Remote, stream_id: u64, message: Value) {
    let text = json!({"stream_id": stream_id, "message": message}).to_string();
    socket
        .send(Message::text(text))
        .await
        .expect("send message");
}

/// The next frame as `(stream_id, message)`, or `None` once the server has
/// closed the connection.
async fn receive(socket: &mut Remote) -> Option<(u64, Value)> {
    loop {
        match socket.next().await? {
            Ok(Message::Text(text)) => {
                let envelope: Value = serde_json::from_str(text.as_str()).expect("JSON");
                let fields = envelope.as_object().expect("an object");
                assert_eq!(fields.len(), 2, "{envelope}");
                return Some((
                    envelope["stream_id"].as_u64().expect("stream_id"),
                    envelope["message"].clone(),
                ));
            }
            Ok(Message::Close(_)) | Err(_) => return None,
            Ok(_) => {}
        }
    }
}

async fn call(socket: &mut Remote, stream_id: u64, message: Value) -> Value {
    send(socket, stream_id, message).await;
    let (answered_on, answer) = receive(socket).await.expect("an answer");
    assert_eq!(answered_on, stream_id, "{answer}");
    answer
}

async fn ping(socket: &mut Remote, id: u64) {
    let pong = call(socket, id, json!({"id": id, "method": "ping"})).await;
    assert_eq!(pong, json!({"id": id, "result": {"pong": true}}));
}

#[tokio::test]
async fn without_a_password_requests_are_served_as_over_tcp() {
    with_timeout(async {
        let (addr, _stop) = start_remote(Arc::new(Core::new()), None).await;
        let mut socket = open(addr).await;
        ping(&mut socket, 1).await;
        let echoed = call(
            &mut socket,
            5,
            json!({"id": 2, "method": "echo", "params": {"text": "こんにちは"}}),
        )
        .await;
        assert_eq!(echoed["id"], 2, "{echoed}");
        assert_eq!(echoed["result"]["text"], "こんにちは", "{echoed}");

        // What is not one frame is answered on stream 0, and the
        // connection goes on.
        socket.send(Message::text("not json")).await.expect("send");
        let (stream, answer) = receive(&mut socket).await.expect("answer");
        assert_eq!(
            (stream, &answer["error"]["code"]),
            (0, &json!("invalid_json"))
        );
        for shape in [
            json!({"message": {"id": 3, "method": "ping"}}),
            json!({"stream_id": 3}),
            json!({"stream_id": -1, "message": {}}),
            json!({"stream_id": 3, "message": {"id": 3, "method": "ping"}, "more": 1}),
        ] {
            socket
                .send(Message::text(shape.to_string()))
                .await
                .expect("send");
            let (stream, answer) = receive(&mut socket).await.expect("answer");
            assert_eq!(stream, 0, "{shape}");
            assert_eq!(answer["error"]["code"], "invalid_request", "{shape}");
        }
        socket
            .send(Message::binary(b"raw".to_vec()))
            .await
            .expect("send");
        let (stream, answer) = receive(&mut socket).await.expect("answer");
        assert_eq!(
            (stream, &answer["error"]["code"]),
            (0, &json!("invalid_request"))
        );
        let bad = call(&mut socket, 4, json!({"method": "ping"})).await;
        assert_eq!(bad["error"]["code"], "invalid_request", "{bad}");
        ping(&mut socket, 6).await;

        // An `auth` request is not needed, and is answered as any
        // method the server does not have.
        let auth = call(
            &mut socket,
            7,
            json!({"id": 7, "method": "auth", "params": {"password": "x"}}),
        )
        .await;
        assert!(auth.get("error").is_some(), "{auth}");
        ping(&mut socket, 8).await;
    })
    .await;
}

#[tokio::test]
async fn a_chat_streams_its_events_and_can_be_cancelled() {
    with_timeout(async {
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&calls);
        let mock = start_mock(move |request: &MockRequest| {
            if counted.fetch_add(1, Ordering::SeqCst) == 0 {
                MockReply::stream(vec![sse_delta(&reply_for(request)), sse_done()])
            } else {
                let mut chunks: Vec<Vec<u8>> = (0..200).map(|_| sse_delta("x")).collect();
                chunks.push(sse_done());
                MockReply::stream(chunks).with_delay(std::time::Duration::from_millis(50))
            }
        })
        .await;
        let llm = Llm::new(LlmConfig {
            provider: ProviderKind::OpenAi,
            auth: inocode_core::oauth::AuthKind::ApiKey,
            base_url: mock.base_url.clone(),
            model: "test-model".to_string(),
            api_key: "test-key".to_string(),
            auth_store_path: None,
            reasoning: Default::default(),
        })
        .expect("provider client");
        let (addr, _stop) = start_remote(Arc::new(Core::with_llm(llm)), None).await;
        let mut socket = open(addr).await;

        send(
            &mut socket,
            7,
            json!({"id": 7, "method": "chat", "params": {"prompt": "hello"}}),
        )
        .await;
        let mut kinds = Vec::new();
        let mut text = String::new();
        loop {
            let (stream, event) = receive(&mut socket).await.expect("event");
            assert_eq!(stream, 7);
            let kind = event["event"].as_str().unwrap_or_default().to_string();
            if kind == "delta" {
                text.push_str(event["text"].as_str().unwrap_or_default());
            }
            kinds.push(kind.clone());
            if kind == "completed" {
                break;
            }
        }
        assert_eq!(kinds, ["started", "delta", "completed"]);
        assert_eq!(text, "reply[hello]");

        send(
            &mut socket,
            1,
            json!({"id": 1, "method": "chat", "params": {"prompt": "cancel me"}}),
        )
        .await;
        assert_eq!(
            receive(&mut socket).await.expect("event").1["event"],
            "started"
        );
        send(
            &mut socket,
            2,
            json!({"id": 2, "method": "cancel", "params": {"request_id": 1}}),
        )
        .await;
        let (mut answered, mut cancelled) = (false, false);
        while !answered || !cancelled {
            let (stream, message) = receive(&mut socket).await.expect("message");
            match stream {
                1 => {
                    assert_ne!(message["event"], "completed");
                    cancelled |= message["event"] == "cancelled";
                }
                2 => {
                    assert_eq!(message["result"]["cancelled"], true, "{message}");
                    answered = true;
                }
                other => panic!("unexpected stream {other}"),
            }
        }
    })
    .await;
}

#[tokio::test]
async fn with_a_password_only_the_right_one_is_let_in() {
    with_timeout(async {
        let hash = inocode_core::remote_password::hash("open sesame").expect("hash");
        let (addr, _stop) = start_remote(Arc::new(Core::new()), Some(hash)).await;

        let refused = [
            json!({"id": 1, "method": "ping"}),
            json!({"id": 1, "method": "auth", "params": {}}),
            json!({"id": 1, "method": "auth", "params": {"password": "open sesame!"}}),
            json!({"id": 1, "method": "auth", "params": {"token": "open sesame"}}),
            json!({"id": 1, "method": "auth", "params": {"token": TEST_TOKEN}}),
        ];
        for first in refused {
            let mut socket = open(addr).await;
            let answer = call(&mut socket, 3, first.clone()).await;
            assert_eq!(answer["id"], 1, "{first}: {answer}");
            assert_eq!(answer["error"]["code"], "unauthorized", "{first}: {answer}");
            assert!(receive(&mut socket).await.is_none(), "{first}: closed");
        }

        let mut socket = open(addr).await;
        let accepted = call(
            &mut socket,
            0,
            json!({"id": 0, "method": "auth", "params": {"password": "open sesame"}}),
        )
        .await;
        assert_eq!(
            accepted,
            json!({"id": 0, "result": {"authenticated": true}})
        );
        ping(&mut socket, 1).await;
    })
    .await;
}

#[tokio::test]
async fn an_upgrade_with_an_origin_header_is_refused_with_403() {
    with_timeout(async {
        let (addr, _stop) = start_remote(Arc::new(Core::new()), None).await;
        let mut request = format!("ws://{addr}/")
            .into_client_request()
            .expect("request");
        request
            .headers_mut()
            .insert("Origin", "http://localhost".parse().expect("header"));
        let stream = TcpStream::connect(addr).await.expect("connect");
        match tokio_tungstenite::client_async(request, stream).await {
            Err(WsError::Http(response)) => assert_eq!(response.status(), 403),
            Err(other) => panic!("refused otherwise: {other}"),
            Ok(_) => panic!("a request with Origin was upgraded"),
        }
        let mut socket = open(addr).await;
        ping(&mut socket, 1).await;
    })
    .await;
}

/// A folder of its own for each test: tests run in parallel.
fn folder() -> std::path::PathBuf {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let index = NEXT.fetch_add(1, Ordering::SeqCst);
    let folder = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("inocode-remote-{}-{index}", std::process::id()));
    let _ = std::fs::remove_dir_all(&folder);
    std::fs::create_dir_all(&folder).expect("folder");
    folder
}

#[tokio::test]
async fn the_settings_show_whether_a_password_is_set_but_never_it_or_its_hash() {
    with_timeout(async {
        let folder = folder();
        std::fs::write(folder.join("settings.json"), "{}").expect("settings");
        inocode_core::secrets::set_mode(inocode_core::secrets::StoreMode::Plaintext);
        let store = inocode_core::store::Store::open(folder.join("inocode.db"))
            .await
            .expect("store");
        let core = Arc::new(
            Core::new()
                .with_store(store)
                .with_credentials(folder.join("auth.json")),
        );
        let (addr, _stop) = start_remote(core, None).await;
        let mut socket = open(addr).await;

        let set = call(
            &mut socket,
            1,
            json!({"id": 1, "method": "settings/set", "params": {"server": {
                "remote": true, "remote_port": 9123, "remote_password": "open sesame"}}}),
        )
        .await;
        let server = &set["result"]["server"];
        assert_eq!(server["remote"], true, "{set}");
        assert_eq!(server["remote_port"], 9123, "{set}");
        assert_eq!(server["remote_password_set"], true, "{set}");
        let got = call(&mut socket, 2, json!({"id": 2, "method": "settings/get"})).await;
        assert_eq!(
            got["result"]["server"]["remote_password_set"], true,
            "{got}"
        );
        assert_eq!(got["result"]["effective"]["remote"], false, "{got}");
        let file = std::fs::read_to_string(folder.join("settings.json")).expect("file");
        let hash = serde_json::from_str::<Value>(&file).expect("JSON")["server"]
            ["remote_password_hash"]
            .as_str()
            .expect("the hash is kept in the file")
            .to_string();
        assert!(!file.contains("open sesame"), "{file}");
        for answer in [&set, &got] {
            let text = answer.to_string();
            assert!(!text.contains("open sesame"), "{text}");
            assert!(!text.contains(&hash), "{text}");
            assert!(!text.contains("pbkdf2"), "{text}");
            assert!(!text.contains("remote_password_hash"), "{text}");
        }

        let refused = call(
            &mut socket,
            3,
            json!({"id": 3, "method": "settings/set", "params": {"server": {
                "remote_password_hash": hash}}}),
        )
        .await;
        assert_eq!(refused["error"]["code"], "invalid_params", "{refused}");
        let removed = call(
            &mut socket,
            4,
            json!({"id": 4, "method": "settings/set", "params": {"server": {
                "remote_password": null}}}),
        )
        .await;
        assert_eq!(
            removed["result"]["server"]["remote_password_set"], false,
            "{removed}"
        );
        let _ = std::fs::remove_dir_all(folder);
    })
    .await;
}
