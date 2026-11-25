mod support;

use std::sync::Arc;

use inocode::{FrameHeader, FrameType};
use inocode_core::Core;
use serde_json::{json, Value};
use support::*;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

#[tokio::test]
async fn server_accepts_a_connection_and_returns_the_core_result() {
    with_timeout(async {
        let (addr, _stop, _handle) = start_server(Arc::new(Core::without_llm())).await;
        let mut client = connect(addr).await;
        let response = call(
            &mut client,
            1,
            json!({"id": 1, "method": "ping", "params": {}}),
        )
        .await;
        assert_eq!(response, json!({"id": 1, "result": {"pong": true}}));
    })
    .await;
}

#[tokio::test]
async fn echo_request_reaches_the_core() {
    with_timeout(async {
        let (addr, _stop, _handle) = start_server(Arc::new(Core::without_llm())).await;
        let mut client = connect(addr).await;
        let params = json!({"text": "日本語の双方向通信テスト", "binary": [0, 255, 10]});
        let response = call(
            &mut client,
            3,
            json!({"id": 3, "method": "echo", "params": params}),
        )
        .await;
        assert_eq!(response["id"], 3);
        assert_eq!(response["result"], params);
    })
    .await;
}

#[tokio::test]
async fn response_id_matches_the_request_id() {
    with_timeout(async {
        let (addr, _stop, _handle) = start_server(Arc::new(Core::without_llm())).await;
        let mut client = connect(addr).await;
        for id in [0, 1, 42, u32::MAX as u64, u64::MAX] {
            let response = call(
                &mut client,
                id,
                json!({"id": id, "method": "echo", "params": {"id": id}}),
            )
            .await;
            assert_eq!(response["id"], id);
            assert_eq!(response["result"]["id"], id);
        }
    })
    .await;
}

#[tokio::test]
async fn sequential_requests_on_one_connection_stay_in_order() {
    with_timeout(async {
        let (addr, _stop, _handle) = start_server(Arc::new(Core::without_llm())).await;
        let mut client = connect(addr).await;
        for round in 0..100u64 {
            let response = call(
                &mut client,
                round,
                json!({"id": round, "method": "echo", "params": {"round": round}}),
            )
            .await;
            assert_eq!(response["id"], round);
            assert_eq!(response["result"]["round"], round);

            let ping = call(&mut client, round, json!({"id": round, "method": "ping"})).await;
            assert_eq!(ping["id"], round);
            assert_eq!(ping["result"]["pong"], true);
        }
    })
    .await;
}

#[tokio::test]
async fn concurrent_clients_are_served_at_the_same_time() {
    with_timeout(async {
        let (addr, _stop, _handle) = start_server(Arc::new(Core::without_llm())).await;
        let mut clients = Vec::new();
        for index in 0..8u64 {
            clients.push(tokio::spawn(async move {
                let mut client = connect(addr).await;
                for round in 0..10u64 {
                    let id = index * 1000 + round;
                    let response = call(
                        &mut client,
                        id,
                        json!({"id": id, "method": "echo", "params": {"client": index, "round": round}}),
                    )
                    .await;
                    assert_eq!(response["id"], id);
                    assert_eq!(response["result"]["client"], index);
                    assert_eq!(response["result"]["round"], round);
                }
            }));
        }
        for client in clients {
            client.await.expect("client task");
        }
    })
    .await;
}

#[tokio::test]
async fn responses_are_not_mixed_between_clients() {
    with_timeout(async {
        let (addr, _stop, _handle) = start_server(Arc::new(Core::without_llm())).await;
        let mut clients = Vec::new();
        for index in 0..3u64 {
            clients.push(tokio::spawn(async move {
                let mut client = connect(addr).await;
                for round in 0..50u64 {
                    let response = call(
                        &mut client,
                        index,
                        json!({"id": index, "method": "echo", "params": {"owner": index}}),
                    )
                    .await;
                    assert_eq!(response["id"], index);
                    assert_eq!(
                        response["result"]["owner"], index,
                        "client {index} received another client's payload on round {round}"
                    );
                }
            }));
        }
        for client in clients {
            client.await.expect("client task");
        }
    })
    .await;
}

#[tokio::test]
async fn invalid_json_and_invalid_requests_are_rejected() {
    with_timeout(async {
        let (addr, _stop, _handle) = start_server(Arc::new(Core::without_llm())).await;
        let (mut reader, mut write_half) = connect_raw(addr).await;

        let malformed = b"{ this is not json";
        let header = FrameHeader::new(FrameType::Json, 1, malformed.len())
            .expect("header")
            .encode()
            .expect("encoded header");
        write_half.write_all(&header).await.expect("write header");
        write_half
            .write_all(malformed)
            .await
            .expect("write payload");

        let (stream_id, response): (u64, Value) = reader.read_json().await.expect("read response");
        assert_eq!(stream_id, 1);
        assert_eq!(response["error"]["code"], "invalid_json");
        assert!(response["id"].is_null());

        let mut writer = inocode::FrameWriter::new(write_half);
        writer
            .write_json(2, &json!({"id": 9}))
            .await
            .expect("send incomplete request");
        let (_, response): (u64, Value) = reader.read_json().await.expect("read response");
        assert_eq!(response["error"]["code"], "invalid_request");
        assert!(response["id"].is_null());

        let ping = call(&mut (reader, writer), 3, json!({"id": 9, "method": "ping"})).await;
        assert_eq!(ping["id"], 9);
        assert_eq!(ping["result"]["pong"], true);
    })
    .await;
}

#[tokio::test]
async fn unknown_method_and_missing_params_are_rejected() {
    with_timeout(async {
        let (addr, _stop, _handle) = start_server(Arc::new(Core::without_llm())).await;
        let mut client = connect(addr).await;

        let unknown = call(&mut client, 1, json!({"id": 1, "method": "run_shell"})).await;
        assert_eq!(unknown["id"], 1);
        assert_eq!(unknown["error"]["code"], "unknown_method");
        assert_eq!(unknown["error"]["message"], "Unknown method");
        assert!(unknown.get("result").is_none());

        let missing = call(&mut client, 2, json!({"id": 2, "method": "echo"})).await;
        assert_eq!(missing["id"], 2);
        assert_eq!(missing["error"]["code"], "invalid_params");
    })
    .await;
}

#[tokio::test]
async fn binary_frames_are_rejected_as_requests() {
    with_timeout(async {
        let (addr, _stop, _handle) = start_server(Arc::new(Core::without_llm())).await;
        let mut client = connect(addr).await;
        client
            .1
            .write_binary_chunk(5, b"raw bytes")
            .await
            .expect("send binary frame");
        let (stream_id, response): (u64, Value) =
            client.0.read_json().await.expect("read response");
        assert_eq!(stream_id, 5);
        assert_eq!(response["error"]["code"], "invalid_request");

        let ping = call(&mut client, 6, json!({"id": 6, "method": "ping"})).await;
        assert_eq!(ping["result"]["pong"], true);
    })
    .await;
}

#[tokio::test]
async fn a_client_disconnect_does_not_stop_the_server() {
    with_timeout(async {
        let (addr, _stop, _handle) = start_server(Arc::new(Core::without_llm())).await;

        let stream = TcpStream::connect(addr).await.expect("connect to server");
        let (_read_half, mut write_half) = stream.into_split();
        let header = FrameHeader::new(FrameType::Json, 1, 100)
            .expect("header")
            .encode()
            .expect("encoded header");
        write_half.write_all(&header).await.expect("write header");
        write_half.write_all(b"partial").await.expect("write part");
        drop(write_half);

        let mut client = connect(addr).await;
        let first = call(&mut client, 1, json!({"id": 1, "method": "ping"})).await;
        assert_eq!(first["result"]["pong"], true);
        drop(client);

        let mut second = connect(addr).await;
        let ping = call(&mut second, 2, json!({"id": 2, "method": "ping"})).await;
        assert_eq!(ping["result"]["pong"], true);
    })
    .await;
}

#[tokio::test]
async fn shutdown_stops_accepting_new_connections() {
    with_timeout(async {
        let (addr, stop, handle) = start_server(Arc::new(Core::without_llm())).await;
        let mut client = connect(addr).await;
        let ping = call(&mut client, 1, json!({"id": 1, "method": "ping"})).await;
        assert_eq!(ping["result"]["pong"], true);

        stop.send(()).expect("send shutdown");
        let result = handle.await.expect("server task");
        assert!(result.is_ok(), "server must shut down cleanly");
        assert!(
            TcpStream::connect(addr).await.is_err(),
            "a stopped server must not accept connections"
        );
    })
    .await;
}

/// Reads frames until the server closes the connection; the frames read.
async fn until_closed(client: &mut Client) -> Vec<Value> {
    let mut frames = Vec::new();
    while let Ok((_, frame)) = client.0.read_json::<Value>().await {
        frames.push(frame);
    }
    frames
}

#[tokio::test]
async fn a_connection_without_the_token_is_refused_and_closed() {
    with_timeout(async {
        let (addr, _stop, _handle) = start_server(Arc::new(Core::without_llm())).await;

        let mut no_auth = connect_unauthenticated(addr).await;
        let ping = call(&mut no_auth, 1, json!({"id": 1, "method": "ping"})).await;
        assert_eq!(ping["id"], 1);
        assert_eq!(ping["error"]["code"], "unauthorized", "{ping}");
        assert!(until_closed(&mut no_auth).await.is_empty());

        let mut wrong = connect_unauthenticated(addr).await;
        let refused = call(
            &mut wrong,
            2,
            json!({"id": 2, "method": "auth", "params": {"token": "test-token-0123456789abcdeX"}}),
        )
        .await;
        assert_eq!(refused["error"]["code"], "unauthorized", "{refused}");
        assert!(until_closed(&mut wrong).await.is_empty());

        let mut binary = connect_unauthenticated(addr).await;
        binary
            .1
            .write_binary_chunk(3, b"raw")
            .await
            .expect("send binary frame");
        let (_, answer): (u64, Value) = binary.0.read_json().await.expect("answer");
        assert_eq!(answer["error"]["code"], "unauthorized", "{answer}");

        let mut right = connect(addr).await;
        let ping = call(&mut right, 4, json!({"id": 4, "method": "ping"})).await;
        assert_eq!(ping["result"]["pong"], true);
    })
    .await;
}

#[tokio::test]
async fn an_answer_over_the_frame_limit_is_replaced_by_an_error() {
    with_timeout(async {
        let database = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("inocode-frame-limit-{}", std::process::id()))
            .join("inocode.db");
        let _ = std::fs::remove_dir_all(database.parent().unwrap());
        std::fs::create_dir_all(database.parent().unwrap()).expect("folder");
        let store = inocode_core::store::Store::open(&database)
            .await
            .expect("store");
        let thread = store.start_session(None, "").await.expect("thread");
        // Two messages that together are over the 16 MiB limit of a frame.
        let half = "x".repeat(inocode::DEFAULT_MAX_PAYLOAD_LEN / 2 + 1024);
        let message = |text: &str| inocode_core::conversation::Message {
            id: String::new(),
            body: inocode_core::conversation::Body::User {
                from: inocode_core::conversation::Speaker::Person,
                parts: vec![text.to_string()],
            },
        };
        store
            .append(thread, vec![message(&half), message(&half)])
            .await
            .expect("messages");
        let core = Core::without_llm().with_store(store);
        let (addr, _stop, _handle) = start_server(Arc::new(core)).await;
        let mut client = connect(addr).await;
        let answer = call(
            &mut client,
            1,
            json!({"id": 1, "method": "thread/read", "params": {"thread_id": thread.0}}),
        )
        .await;
        assert_eq!(answer["id"], 1);
        assert_eq!(answer["error"]["code"], "frame_too_large", "{answer}");

        let ping = call(&mut client, 2, json!({"id": 2, "method": "ping"})).await;
        assert_eq!(ping["result"]["pong"], true, "the connection goes on");
    })
    .await;
}

#[tokio::test]
async fn a_request_over_the_frame_limit_is_answered_before_the_close() {
    with_timeout(async {
        let (addr, _stop, _handle) = start_server(Arc::new(Core::without_llm())).await;
        let (mut reader, mut write_half) = connect_raw(addr).await;
        let header = FrameHeader::new(FrameType::Json, 7, inocode::DEFAULT_MAX_PAYLOAD_LEN + 1)
            .expect("header")
            .encode()
            .expect("encoded header");
        write_half.write_all(&header).await.expect("write header");
        let (_, answer): (u64, Value) = reader.read_json().await.expect("answer");
        assert_eq!(answer["error"]["code"], "frame_too_large", "{answer}");
        assert!(reader.read_json::<Value>().await.is_err(), "then closed");
    })
    .await;
}
