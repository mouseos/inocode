mod common;

use std::collections::HashMap;

use common::*;
use inocode::{
    FrameReader, FrameType, FrameWriter, ProtocolError, DEFAULT_MAX_PAYLOAD_LEN, HEADER_LEN,
};
use serde_json::json;
use tokio::io::AsyncWriteExt;

#[tokio::test]
async fn client_sends_json_to_server() {
    with_timeout(async {
        let (client, server) = connect_pair(|mut socket| async move {
            let (stream_id, message): (u64, TestMessage) = socket.0.read_json().await.unwrap();
            assert_eq!(stream_id, 1);
            assert_eq!(message, TestMessage::new(1));
            let ack = Ack {
                ok: true,
                stream_id,
                bytes: 0,
            };
            socket.1.write_json(stream_id, &ack).await.unwrap();
        })
        .await;

        let (mut reader, mut writer) = client;
        writer.write_json(1, &TestMessage::new(1)).await.unwrap();
        let (stream_id, ack): (u64, Ack) = reader.read_json().await.unwrap();
        assert_eq!(stream_id, 1);
        assert!(ack.ok);
        server.await.unwrap();
    })
    .await;
}

#[tokio::test]
async fn server_sends_json_to_client() {
    with_timeout(async {
        let (client, server) = connect_pair(|mut socket| async move {
            socket.1.write_json(2, &TestMessage::new(2)).await.unwrap();
            let (_, ack): (u64, Ack) = socket.0.read_json().await.unwrap();
            assert!(ack.ok);
        })
        .await;

        let (mut reader, mut writer) = client;
        let (stream_id, message): (u64, TestMessage) = reader.read_json().await.unwrap();
        assert_eq!(stream_id, 2);
        assert_eq!(message, TestMessage::new(2));
        let ack = Ack {
            ok: true,
            stream_id,
            bytes: 0,
        };
        writer.write_json(stream_id, &ack).await.unwrap();
        server.await.unwrap();
    })
    .await;
}

#[tokio::test]
async fn client_sends_binary_to_server() {
    with_timeout(async {
        let data = vec![0x00, 0x01, b'\n', 0xff, 0x80, 0x00, 0x7f, b'\r', 0xfe];
        let expected = data.clone();
        let (client, server) = connect_pair(move |mut socket| async move {
            let (stream_id, start): (u64, TestMessage) = socket.0.read_json().await.unwrap();
            assert_eq!(start, start_message(stream_id, expected.len() as u64));

            let mut received = VecWriter::default();
            let bytes = socket
                .0
                .read_binary_to(stream_id, &mut received)
                .await
                .unwrap();
            assert_eq!(received.0, expected);
            assert_eq!(bytes, expected.len() as u64);

            let ack = Ack {
                ok: true,
                stream_id,
                bytes,
            };
            socket.1.write_json(stream_id, &ack).await.unwrap();
        })
        .await;

        let (mut reader, mut writer) = client;
        writer
            .write_json(3, &start_message(3, data.len() as u64))
            .await
            .unwrap();
        writer.write_binary_chunk(3, &data[..4]).await.unwrap();
        writer.write_binary_chunk(3, &data[4..]).await.unwrap();
        writer.write_binary_end(3).await.unwrap();

        let (stream_id, ack): (u64, Ack) = reader.read_json().await.unwrap();
        assert_eq!(stream_id, 3);
        assert!(ack.ok);
        assert_eq!(ack.bytes, data.len() as u64);
        server.await.unwrap();
    })
    .await;
}

#[tokio::test]
async fn server_sends_binary_to_client() {
    with_timeout(async {
        let data: Vec<u8> = (0..=255u8).chain([0u8, 0x00, 0xff, b'\n']).collect();
        let payload = data.clone();
        let (client, server) = connect_pair(move |mut socket| async move {
            socket
                .1
                .write_json(4, &start_message(4, payload.len() as u64))
                .await
                .unwrap();
            socket
                .1
                .write_binary_chunk(4, &payload[..100])
                .await
                .unwrap();
            socket
                .1
                .write_binary_chunk(4, &payload[100..])
                .await
                .unwrap();
            socket.1.write_binary_end(4).await.unwrap();
        })
        .await;

        let (mut reader, _writer) = client;
        let (stream_id, start): (u64, TestMessage) = reader.read_json().await.unwrap();
        assert_eq!(stream_id, 4);
        assert_eq!(start, start_message(4, data.len() as u64));

        let mut received = VecWriter::default();
        let bytes = reader.read_binary_to(4, &mut received).await.unwrap();
        assert_eq!(received.0, data);
        assert_eq!(bytes, data.len() as u64);
        server.await.unwrap();
    })
    .await;
}

#[tokio::test]
async fn json_and_binary_interleave_in_both_directions() {
    with_timeout(async {
        let (client, server) = connect_pair(|mut socket| async move {
            for _ in 0..4 {
                let (stream_id, message): (u64, TestMessage) = socket.0.read_json().await.unwrap();
                let (header, chunk) = read_frame_from(&mut socket.0).await.unwrap();
                assert_eq!(header.frame_type, FrameType::Binary);
                assert_eq!(header.stream_id, stream_id);
                socket
                    .1
                    .write_binary_chunk(stream_id, &chunk)
                    .await
                    .unwrap();
                socket.1.write_json(stream_id, &message).await.unwrap();
            }
        })
        .await;

        let (mut reader, mut writer) = client;
        for round in 0..4u8 {
            let chunk = vec![round; 1000 + round as usize];
            writer
                .write_json(5, &TestMessage::new(round as u64))
                .await
                .unwrap();
            writer.write_binary_chunk(5, &chunk).await.unwrap();

            let (header, echoed) = read_frame_from(&mut reader).await.unwrap();
            assert_eq!(header.frame_type, FrameType::Binary);
            assert_eq!(echoed, chunk);

            let (stream_id, message): (u64, TestMessage) = reader.read_json().await.unwrap();
            assert_eq!(stream_id, 5);
            assert_eq!(message, TestMessage::new(round as u64));
        }
        server.await.unwrap();
    })
    .await;
}

#[tokio::test]
async fn interleaved_frames_are_reassembled_per_stream() {
    with_timeout(async {
        let first = vec![0xaa; 3000];
        let second = vec![0xbb; 5000];
        let expected_first = first.clone();
        let expected_second = second.clone();
        let (client, server) = connect_pair(move |mut socket| async move {
            let mut announced: HashMap<u64, u64> = HashMap::new();
            let mut received: HashMap<u64, Vec<u8>> = HashMap::new();
            let mut ended = 0;
            while ended < 2 {
                let (header, payload) = read_frame_from(&mut socket.0).await.unwrap();
                match header.frame_type {
                    FrameType::Json => {
                        let start: TestMessage = serde_json::from_slice(&payload).unwrap();
                        assert_eq!(start.kind, "start");
                        announced.insert(header.stream_id, start.message.parse().unwrap());
                        received.insert(header.stream_id, Vec::new());
                    }
                    FrameType::Binary => received
                        .get_mut(&header.stream_id)
                        .unwrap()
                        .extend_from_slice(&payload),
                    FrameType::End => ended += 1,
                }
            }
            assert_eq!(received[&11], expected_first);
            assert_eq!(received[&22], expected_second);
            assert_eq!(announced[&11], expected_first.len() as u64);
            assert_eq!(announced[&22], expected_second.len() as u64);

            let ack = Ack {
                ok: true,
                stream_id: 0,
                bytes: (received[&11].len() + received[&22].len()) as u64,
            };
            socket.1.write_json(0, &ack).await.unwrap();
        })
        .await;

        let (mut reader, mut writer) = client;
        writer
            .write_json(11, &start_message(11, first.len() as u64))
            .await
            .unwrap();
        writer
            .write_json(22, &start_message(22, second.len() as u64))
            .await
            .unwrap();
        writer.write_binary_chunk(11, &first[..1000]).await.unwrap();
        writer
            .write_binary_chunk(22, &second[..2000])
            .await
            .unwrap();
        writer.write_binary_chunk(11, &first[1000..]).await.unwrap();
        writer
            .write_binary_chunk(22, &second[2000..])
            .await
            .unwrap();
        writer.write_binary_end(11).await.unwrap();
        writer.write_binary_end(22).await.unwrap();

        let (_, ack): (u64, Ack) = reader.read_json().await.unwrap();
        assert!(ack.ok);
        assert_eq!(ack.bytes, (first.len() + second.len()) as u64);
        server.await.unwrap();
    })
    .await;
}

#[tokio::test]
async fn frames_split_into_single_bytes_are_reassembled() {
    with_timeout(async {
        let data = vec![0x00, b'\n', 0xff, 0x80, 0xc3, 0x28, 0x00, 0x7f];
        let expected = data.clone();
        let (stream, server) = connect_stream(move |stream| async move {
            let (read_half, write_half) = stream.into_split();
            let mut reader = FrameReader::new(read_half);
            let mut writer = FrameWriter::new(OneByteAtATime::new(write_half));

            let (stream_id, message): (u64, TestMessage) = reader.read_json().await.unwrap();
            assert_eq!(message, TestMessage::new(1));
            let mut received = VecWriter::default();
            let bytes = reader
                .read_binary_to(stream_id, &mut received)
                .await
                .unwrap();
            assert_eq!(received.0, expected);

            writer
                .write_json(stream_id, &TestMessage::new(2))
                .await
                .unwrap();
            writer
                .write_binary_chunk(stream_id, &expected)
                .await
                .unwrap();
            writer.write_binary_end(stream_id).await.unwrap();
            bytes
        })
        .await;

        let (read_half, write_half) = stream.into_split();
        let mut reader = FrameReader::new(read_half);
        let mut writer = FrameWriter::new(OneByteAtATime::new(write_half));

        writer.write_json(1, &TestMessage::new(1)).await.unwrap();
        writer.write_binary_chunk(1, &data[..3]).await.unwrap();
        writer.write_binary_chunk(1, &data[3..]).await.unwrap();
        writer.write_binary_end(1).await.unwrap();

        let (stream_id, message): (u64, TestMessage) = reader.read_json().await.unwrap();
        assert_eq!(message, TestMessage::new(2));
        let mut received = VecWriter::default();
        let bytes = reader
            .read_binary_to(stream_id, &mut received)
            .await
            .unwrap();
        assert_eq!(received.0, data);
        assert_eq!(bytes, data.len() as u64);
        assert_eq!(server.await.unwrap(), data.len() as u64);
    })
    .await;
}

#[tokio::test]
async fn concatenated_frames_in_one_write_are_split_apart() {
    with_timeout(async {
        let data = vec![0x00, b'\n', 0xff];
        let payload = data.clone();
        let (mut client, server) = connect_raw(move |mut socket| async move {
            let (json_header, first) = read_frame_from(&mut socket.0).await.unwrap();
            assert_eq!(json_header.frame_type, FrameType::Json);
            assert_eq!(json_header.stream_id, 1);
            let (_, second) = read_frame_from(&mut socket.0).await.unwrap();
            let (binary_header, binary) = read_frame_from(&mut socket.0).await.unwrap();
            assert_eq!(binary_header.frame_type, FrameType::Binary);
            assert_eq!(binary_header.stream_id, 3);
            assert_eq!(binary, payload);
            let (end_header, _) = read_frame_from(&mut socket.0).await.unwrap();
            assert_eq!(end_header.frame_type, FrameType::End);
            assert_eq!(end_header.stream_id, 3);

            let first: TestMessage = serde_json::from_slice(&first).unwrap();
            let second: TestMessage = serde_json::from_slice(&second).unwrap();
            assert_eq!(first, TestMessage::new(1));
            assert_eq!(second, TestMessage::new(2));
        })
        .await;

        let mut frames = Vec::new();
        frames.extend_from_slice(&encode_json_frame(1, &TestMessage::new(1)));
        frames.extend_from_slice(&encode_json_frame(2, &TestMessage::new(2)));
        frames.extend_from_slice(&encode_frame(FrameType::Binary, 3, &data));
        frames.extend_from_slice(&encode_frame(FrameType::End, 3, &[]));
        client.write_all(&frames).await.unwrap();
        server.await.unwrap();
    })
    .await;
}

#[tokio::test]
async fn binary_payloads_match_byte_for_byte_in_both_directions() {
    with_timeout(async {
        let payload: Vec<u8> = vec![0x00, b'\n', 0x0d, 0xff, 0x80, 0xc3, 0x28, 0x00]
            .into_iter()
            .chain("日本語".as_bytes().iter().copied())
            .chain([0x00, 0x00, 0xff])
            .collect();
        let echoed = payload.clone();
        let (client, server) = connect_pair(move |mut socket| async move {
            let (stream_id, _): (u64, TestMessage) = socket.0.read_json().await.unwrap();
            let mut received = VecWriter::default();
            socket
                .0
                .read_binary_to(stream_id, &mut received)
                .await
                .unwrap();
            assert_eq!(received.0, echoed);

            socket
                .1
                .write_json(stream_id, &start_message(stream_id, echoed.len() as u64))
                .await
                .unwrap();
            socket
                .1
                .write_binary_chunk(stream_id, &echoed)
                .await
                .unwrap();
            socket.1.write_binary_end(stream_id).await.unwrap();
        })
        .await;

        let (mut reader, mut writer) = client;
        writer
            .write_json(7, &start_message(7, payload.len() as u64))
            .await
            .unwrap();
        writer.write_binary_chunk(7, &payload).await.unwrap();
        writer.write_binary_end(7).await.unwrap();

        let (_, start): (u64, TestMessage) = reader.read_json().await.unwrap();
        assert_eq!(start, start_message(7, payload.len() as u64));
        let mut received = VecWriter::default();
        reader.read_binary_to(7, &mut received).await.unwrap();
        assert_eq!(received.0, payload);
        server.await.unwrap();
    })
    .await;
}

#[tokio::test]
async fn large_payload_is_streamed_in_chunks_both_directions() {
    with_timeout(async {
        const SIZE: u64 = 8 * 1024 * 1024;
        let (client, server) = connect_pair(|mut socket| async move {
            let (stream_id, start): (u64, TestMessage) = socket.0.read_json().await.unwrap();
            assert_eq!(start, start_message(stream_id, SIZE));

            let mut received = HashSink::new();
            let bytes = socket
                .0
                .read_binary_to(stream_id, &mut received)
                .await
                .unwrap();
            assert_eq!(bytes, SIZE);
            assert_eq!(received.bytes(), SIZE);
            assert_eq!(received.hash(), pattern_hash(SIZE));

            socket
                .1
                .write_json(stream_id, &start_message(stream_id, SIZE))
                .await
                .unwrap();
            let mut source = PatternReader::new(SIZE);
            assert_eq!(
                socket
                    .1
                    .write_binary_stream(stream_id, &mut source)
                    .await
                    .unwrap(),
                SIZE
            );
        })
        .await;

        let (mut reader, mut writer) = client;
        writer.write_json(1, &start_message(1, SIZE)).await.unwrap();
        let mut source = PatternReader::new(SIZE);
        assert_eq!(
            writer.write_binary_stream(1, &mut source).await.unwrap(),
            SIZE
        );

        let (stream_id, start): (u64, TestMessage) = reader.read_json().await.unwrap();
        assert_eq!(start, start_message(stream_id, SIZE));
        let mut received = HashSink::new();
        let bytes = reader
            .read_binary_to(stream_id, &mut received)
            .await
            .unwrap();
        assert_eq!(bytes, SIZE);
        assert_eq!(received.bytes(), SIZE);
        assert_eq!(received.hash(), pattern_hash(SIZE));
        server.await.unwrap();
    })
    .await;
}

async fn header_error(raw: [u8; HEADER_LEN]) -> ProtocolError {
    let (mut client, server) =
        connect_raw(|mut socket| async move { socket.0.read_header().await.err() }).await;
    client.write_all(&raw).await.unwrap();
    server
        .await
        .unwrap()
        .expect("the invalid header must be rejected")
}

#[tokio::test]
async fn invalid_headers_are_rejected() {
    with_timeout(async {
        assert!(matches!(
            header_error(header_bytes(11, 1, 0, 0, 1)).await,
            ProtocolError::InvalidFrameLength(11)
        ));
        assert!(matches!(
            header_error(header_bytes(12, 9, 0, 0, 1)).await,
            ProtocolError::UnknownFrameType(9)
        ));
        assert!(matches!(
            header_error(header_bytes(12, 1, 1, 0, 1)).await,
            ProtocolError::NonZeroFlags(1)
        ));
        assert!(matches!(
            header_error(header_bytes(12, 1, 0, 1, 1)).await,
            ProtocolError::NonZeroReserved(1)
        ));

        let too_large = DEFAULT_MAX_PAYLOAD_LEN as u32 + 12 + 1;
        assert!(matches!(
            header_error(header_bytes(too_large, 2, 0, 0, 1)).await,
            ProtocolError::PayloadTooLarge { .. }
        ));
    })
    .await;
}

#[tokio::test]
async fn disconnects_mid_transfer_are_reported() {
    with_timeout(async {
        // A header announces 1000 payload bytes but the connection closes after 14.
        let (mut client, server) = connect_raw(|mut socket| async move {
            let (stream_id, _): (u64, TestMessage) = socket.0.read_json().await.unwrap();
            let mut received = VecWriter::default();
            socket
                .0
                .read_binary_to(stream_id, &mut received)
                .await
                .err()
        })
        .await;
        let mut partial = encode_frame(FrameType::Binary, 1, &[0xaa; 1000]);
        partial.truncate(30);
        client
            .write_all(&encode_json_frame(1, &start_message(1, 1000)))
            .await
            .unwrap();
        client.write_all(&partial).await.unwrap();
        drop(client);
        assert!(matches!(
            server.await.unwrap(),
            Some(ProtocolError::UnexpectedEof)
        ));

        // The payload arrives but the End frame never does.
        let (mut client, server) = connect_raw(|mut socket| async move {
            let (stream_id, _): (u64, TestMessage) = socket.0.read_json().await.unwrap();
            let mut received = VecWriter::default();
            socket
                .0
                .read_binary_to(stream_id, &mut received)
                .await
                .err()
        })
        .await;
        client
            .write_all(&encode_json_frame(1, &start_message(1, 3)))
            .await
            .unwrap();
        client
            .write_all(&encode_frame(FrameType::Binary, 1, b"abc"))
            .await
            .unwrap();
        drop(client);
        assert!(matches!(
            server.await.unwrap(),
            Some(ProtocolError::UnexpectedEof)
        ));

        // A clean close between frames stays distinguishable from both.
        let (mut client, server) = connect_raw(|mut socket| async move {
            let (stream_id, _): (u64, TestMessage) = socket.0.read_json().await.unwrap();
            let mut received = VecWriter::default();
            let bytes = socket
                .0
                .read_binary_to(stream_id, &mut received)
                .await
                .unwrap();
            let after_end = socket.0.read_header().await.unwrap();
            (bytes, received.0, after_end)
        })
        .await;
        let mut frames = Vec::new();
        frames.extend_from_slice(&encode_json_frame(1, &start_message(1, 3)));
        frames.extend_from_slice(&encode_frame(FrameType::Binary, 1, b"abc"));
        frames.extend_from_slice(&encode_frame(FrameType::End, 1, &[]));
        client.write_all(&frames).await.unwrap();
        drop(client);
        let (bytes, received, after_end) = server.await.unwrap();
        assert_eq!(bytes, 3);
        assert_eq!(received, b"abc");
        assert!(after_end.is_none());
    })
    .await;
}

#[tokio::test]
async fn repeated_round_trips_do_not_mix_or_corrupt_data() {
    with_timeout(async {
        const ROUNDS: u64 = 200;
        let (client, server) = connect_pair(|mut socket| async move {
            for round in 0..ROUNDS {
                let stream_id = round % 3;
                let (received_id, message): (u64, TestMessage) =
                    socket.0.read_json().await.unwrap();
                assert_eq!(received_id, stream_id);
                assert_eq!(message, TestMessage::new(round));

                if round % 25 == 0 {
                    let (header, chunk) = read_frame_from(&mut socket.0).await.unwrap();
                    assert_eq!(header.stream_id, stream_id);
                    socket
                        .1
                        .write_binary_chunk(stream_id, &chunk)
                        .await
                        .unwrap();
                }
                socket.1.write_json(stream_id, &message).await.unwrap();
            }
        })
        .await;

        let (mut reader, mut writer) = client;
        for round in 0..ROUNDS {
            let stream_id = round % 3;
            writer
                .write_json(stream_id, &TestMessage::new(round))
                .await
                .unwrap();
            if round % 25 == 0 {
                let chunk = vec![round as u8; 3000];
                writer.write_binary_chunk(stream_id, &chunk).await.unwrap();
                let (header, echoed) = read_frame_from(&mut reader).await.unwrap();
                assert_eq!(header.stream_id, stream_id);
                assert_eq!(echoed, chunk);
            }
            let (received_id, message): (u64, TestMessage) = reader.read_json().await.unwrap();
            assert_eq!(received_id, stream_id);
            assert_eq!(message, TestMessage::new(round));
        }
        server.await.unwrap();
    })
    .await;
}

#[tokio::test]
async fn json_control_message_keeps_the_specified_shape() {
    with_timeout(async {
        let expected = json!({"id": 1, "type": "test", "message": "日本語の双方向通信テスト"});
        let expected_in_server = expected.clone();
        let (stream, server) = connect_raw(move |mut socket| async move {
            let (header, payload) = read_frame_from(&mut socket.0).await.unwrap();
            assert_eq!(header.frame_type, FrameType::Json);
            assert_eq!(header.stream_id, 1);
            let value: serde_json::Value = serde_json::from_slice(&payload).unwrap();
            assert_eq!(value, expected_in_server);

            socket.1.write_json(1, &expected_in_server).await.unwrap();
        })
        .await;

        let (read_half, mut write_half) = stream.into_split();
        let mut reader = FrameReader::new(read_half);
        write_half
            .write_all(&encode_json_frame(1, &TestMessage::new(1)))
            .await
            .unwrap();
        let (_, reply): (u64, serde_json::Value) = reader.read_json().await.unwrap();
        assert_eq!(reply, expected);
        server.await.unwrap();
    })
    .await;
}
