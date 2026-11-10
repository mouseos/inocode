//! Offline tests: buvid algorithm vector, protobuf wire shapes, and the
//! JSON shaping of decoded responses. No network.

use bilibili_provider::buvid::{buvid_from, DeviceIdentity};
use bilibili_provider::proto::*;
use bilibili_provider::{
    aid_to_bvid, extract_bvid, search_response_json, strip_highlight, view_reply_json,
};
use prost::Message;

#[test]
fn buvid_matches_app_algorithm() {
    // md5("abc") = 900150983cd24fb0d6963f7d28e17f72
    // app format: prefix + hex[2] + hex[12] + hex[22] + hex
    let b = buvid_from("XX", "abc");
    assert_eq!(b, "XX047900150983cd24fb0d6963f7d28e17f72");
    assert_eq!(b.len(), 37);
}

#[test]
fn identity_buvid_uses_android_id_provider() {
    let id = DeviceIdentity::from_android_id("0123456789abcdef".to_string());
    let b = id.local_buvid();
    assert!(b.starts_with("XX"), "androidId provider prefix: {b}");
    assert_eq!(b.len(), 37);
    // deterministic for the same identity
    assert_eq!(b, id.local_buvid());
}

#[test]
fn search_request_wire_shape() {
    let req = SearchAllRequest {
        keyword: "test".into(),
        from_source: "search".into(),
        pagination: Some(Pagination {
            page_size: 20,
            next: String::new(),
        }),
        ..Default::default()
    };
    let bytes = req.encode_to_vec();
    // field 1 (keyword), wiretype 2 -> 0x0A, len 4, "test"
    assert_eq!(&bytes[0..6], &[0x0A, 0x04, b't', b'e', b's', b't']);
    // field 10 (pagination), wiretype 2 -> 0x52
    assert!(bytes.windows(1).any(|w| w[0] == 0x52));
    // roundtrip
    let back = SearchAllRequest::decode(bytes.as_slice()).unwrap();
    assert_eq!(back, req);
}

#[test]
fn search_response_json_shaping() {
    let resp = SearchAllResponse {
        keyword: "test".into(),
        trackid: "track-1".into(),
        item: vec![
            SearchItem {
                uri: "bilibili://video/BV1GJ411x7h7".into(),
                param: "114514".into(),
                goto: "av".into(),
                av: Some(SearchVideoCard {
                    title: "<em class=\"keyword\">test</em> video".into(),
                    cover: "https://i0.hdslb.com/cover.jpg".into(),
                    play: 12345,
                    danmaku: 67,
                    author: "someone".into(),
                    desc: "d".into(),
                    duration: "03:21".into(),
                    mid: 42,
                }),
                ..Default::default()
            },
            // non-video card: filtered out of items
            SearchItem {
                goto: "bangumi".into(),
                ..Default::default()
            },
        ],
        pagination: Some(PaginationReply {
            next: "tok2".into(),
            prev: String::new(),
        }),
    };
    let v = search_response_json("test", &resp);
    assert_eq!(v["ok"], true);
    assert_eq!(v["count"], 1);
    assert_eq!(v["next"], "tok2");
    let item = &v["items"][0];
    assert_eq!(item["title"], "test video");
    assert_eq!(item["bvid"], "BV1GJ411x7h7");
    assert_eq!(item["aid"], 114514);
    assert_eq!(item["author"], "someone");
    assert_eq!(item["play"], 12345);
    assert_eq!(item["url"], "https://www.bilibili.com/video/BV1GJ411x7h7");
}

#[test]
fn view_reply_json_shaping() {
    let resp = ViewReply {
        arc: Some(Arc {
            aid: 114514,
            cid: 222,
            duration: 201,
            stat: Some(Stat {
                vt: Some(StatInfo {
                    value: 999,
                    text: "999".into(),
                }),
                danmaku: Some(StatInfo {
                    value: 7,
                    text: "7".into(),
                }),
                reply: 3,
                fav: 4,
                coin: 5,
                share: 6,
                like: 8,
            }),
            bvid: "BV1GJ411x7h7".into(),
            cover: "https://i0.hdslb.com/c.jpg".into(),
            title: "title".into(),
            ..Default::default()
        }),
        owner: Some(Owner {
            avatar: Some(AvatarItem { mid: 42 }),
            title: "uploader".into(),
            fans: "1.2万".into(),
        }),
    };
    let v = view_reply_json("BV1GJ411x7h7", &resp);
    assert_eq!(v["item"]["title"], "title");
    assert_eq!(v["item"]["stat"]["view"], 999);
    assert_eq!(v["item"]["stat"]["like"], 8);
    assert_eq!(v["item"]["owner"]["mid"], 42);
    assert_eq!(v["item"]["owner"]["name"], "uploader");
}

#[test]
fn aid_to_bvid_known_vector() {
    // Canonical public test vector for the aid<->bvid mapping.
    assert_eq!(aid_to_bvid(170001), "BV17x411w7KC");
}

#[test]
fn bvid_extraction() {
    assert_eq!(
        extract_bvid("https://www.bilibili.com/video/BV1GJ411x7h7?p=1"),
        "BV1GJ411x7h7"
    );
    assert_eq!(extract_bvid("bilibili://video/114514"), "");
    assert_eq!(strip_highlight("<em class=\"keyword\">a</em>b"), "ab");
}

#[test]
fn metadata_wire_values_are_base64() {
    use base64::Engine;
    use bilibili_provider::encode_metadata_wire;
    // A protobuf-shaped value (tag + length + text, with control bytes).
    let raw = vec![0x12, 0x07, b'a', b'n', b'd', b'r', b'o', b'i', b'd'];
    let wire = encode_metadata_wire(vec![
        ("x-bili-metadata-bin".to_string(), raw.clone()),
        ("env".to_string(), b"prod".to_vec()),
    ]);
    // -bin value: base64 text that decodes back to the exact protobuf.
    let encoded = &wire[0].1;
    assert!(encoded.iter().all(|b| b.is_ascii_graphic() || *b == b'='));
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .expect("wire value must be valid base64");
    assert_eq!(decoded, raw);
    // Plain headers pass through untouched.
    assert_eq!(wire[1].1, b"prod".to_vec());
}
