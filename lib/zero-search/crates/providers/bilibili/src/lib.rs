//! Bilibili provider speaking the Android app's own protocol.
//!
//! The main Bilibili app (tv.danmaku.bili) does not use the public web API
//! for its core surfaces: keyword search is the gRPC service
//! `bilibili.polymer.app.search.v1.Search/SearchAll` and the video page is
//! `bilibili.app.viewunite.v1.View/View`, both on `grpc.biliapi.net`, with
//! protobuf metadata headers (`x-bili-metadata-bin` and friends) carrying
//! a guest identity whose only device value is the buvid the app itself
//! generates (see [`buvid`]). This crate reproduces that protocol in
//! Rust: no login, no access_key, no fabricated values — the buvid is
//! derived by the app's own algorithm from this client's device identity.
//!
//! Live-verified state (2026-11-08, round 2 — supersedes round 1):
//! - Search, video detail (viewunite `View`) and related videos
//!   (`RelatesFeed`) all work end to end as a guest.
//! - The viewunite family demands the device context (the
//!   `x-bili-*-bin` metadata): bare calls get business error -400
//!   (View) or an empty feed (RelatesFeed).
//! - Round 1 concluded the metadata "cannot cross the public edge"
//!   because raw protobuf values were rejected by the front with
//!   HTTP 400. That was a representation error, not an edge law: on
//!   this ingress the `-bin` values travel **base64-encoded** (the
//!   front is a text-oriented bridge and the grpc-go backend
//!   base64-decodes them). With base64 values all three surfaces
//!   pass. See round2-report.md for the forensic trail.
//!
//! Intended tool mapping for the ZeroSearch integration (not wired here):
//! `bilibili_search_videos`, `bilibili_get_video`, `bilibili_get_related`.

pub mod buvid;
pub mod proto;
pub mod transport;

use anyhow::Result;
use prost::Message;
use serde_json::{json, Value};

use buvid::DeviceIdentity;
use proto::*;
use transport::Egress;

pub const PROVIDER: &str = "bilibili";
/// App under analysis: tv.danmaku.bili.
pub const APP_VERSION_NAME: &str = "9.18.0";
pub const APP_BUILD: i32 = 9180300;
pub const GRPC_HOST: &str = "grpc.biliapi.net";

const PATH_SEARCH_ALL: &str = "/bilibili.polymer.app.search.v1.Search/SearchAll";
const PATH_VIEW: &str = "/bilibili.app.viewunite.v1.View/View";
const PATH_RELATES: &str = "/bilibili.app.viewunite.v1.View/RelatesFeed";

pub struct BilibiliClient {
    egress: Egress,
    send_metadata: bool,
    host: &'static str,
    extra_headers: Vec<(String, Vec<u8>)>,
    identity: DeviceIdentity,
    buvid: String,
    session_id: String,
    fts_ms: i64,
}

impl BilibiliClient {
    /// New guest client with a fresh device identity (fresh buvid).
    pub fn new() -> Self {
        Self::with_identity(DeviceIdentity::random())
    }

    /// New guest client around a caller-supplied (persisted) identity, so
    /// the buvid is stable across runs, as on a real device.
    pub fn with_identity(identity: DeviceIdentity) -> Self {
        let buvid = identity.local_buvid();
        Self {
            egress: Egress::Direct,
            send_metadata: true,
            host: GRPC_HOST,
            extra_headers: Vec::new(),
            identity,
            buvid,
            session_id: buvid::fawkes_session_id(),
            fts_ms: current_time_ms(),
        }
    }

    /// Override the gRPC host (round-2 forensics: the app is known to
    /// use more than one front for the same services).
    pub fn with_host(mut self, host: &'static str) -> Self {
        self.host = host;
        self
    }

    /// Append one extra plain header to every call (forensics knob;
    /// e.g. the `user-agent` a grpc-java/OkHttp client always sends).
    pub fn with_extra_header(mut self, name: &str, value: &str) -> Self {
        self.extra_headers
            .push((name.to_string(), value.as_bytes().to_vec()));
        self
    }

    /// Attach the app's `x-bili-*-bin` metadata headers (with buvid) to
    /// every call. Default: on — the viewunite family refuses bare
    /// calls, and search accepts the metadata too. On the wire the
    /// `-bin` values are base64-encoded (see
    /// [`encode_metadata_wire`]); that is the representation this
    /// ingress carries.
    pub fn with_metadata(mut self, send: bool) -> Self {
        self.send_metadata = send;
        self
    }

    /// Route connections through a SOCKS5 proxy (a deployment choice of
    /// the caller — e.g. a region-appropriate egress — never a default).
    pub fn with_socks5(mut self, proxy: impl Into<String>) -> Self {
        self.egress = Egress::Socks5(proxy.into());
        self
    }

    pub fn buvid(&self) -> &str {
        &self.buvid
    }

    pub fn identity(&self) -> &DeviceIdentity {
        &self.identity
    }

    /// The metadata headers the app attaches to every gRPC call
    /// (Moss stack): protobuf payloads under `x-bili-*-bin`, plus `env`.
    fn metadata(&self) -> Vec<(String, Vec<u8>)> {
        let mut map: Vec<(String, Vec<u8>)> = Vec::new();
        let meta = Metadata {
            access_key: String::new(),
            mobi_app: "android".into(),
            device: String::new(),
            build: APP_BUILD,
            channel: "master".into(),
            buvid: self.buvid.clone(),
            platform: "android".into(),
        };
        let device = Device {
            app_id: 1,
            build: APP_BUILD,
            buvid: self.buvid.clone(),
            mobi_app: "android".into(),
            platform: "android".into(),
            device: String::new(),
            channel: "master".into(),
            brand: self.identity.brand.clone(),
            model: self.identity.model.clone(),
            osver: self.identity.osver.clone(),
            fp_local: String::new(),
            fp_remote: String::new(),
            version_name: APP_VERSION_NAME.into(),
            fp: String::new(),
            fts: self.fts_ms,
        };
        let network = Network {
            r#type: NetworkType::Wifi as i32,
            tf: 0,
            oid: String::new(),
        };
        let locale_ids = LocaleIds {
            language: "zh".into(),
            script: "Hans".into(),
            region: "CN".into(),
        };
        let locale = Locale {
            c_locale: Some(locale_ids.clone()),
            s_locale: Some(locale_ids),
            sim_code: String::new(),
            timezone: "Asia/Shanghai".into(),
        };
        let restriction = Restriction {
            teenagers_mode: false,
            lessons_mode: false,
            mode: ModeType::Normal as i32,
            review: false,
            disable_rcmd: false,
            basic_mode: false,
        };
        let fawkes = FawkesReq {
            appkey: "android64".into(),
            env: "prod".into(),
            session_id: self.session_id.clone(),
        };
        map.push(("x-bili-metadata-bin".into(), meta.encode_to_vec()));
        map.push(("x-bili-device-bin".into(), device.encode_to_vec()));
        map.push(("x-bili-network-bin".into(), network.encode_to_vec()));
        map.push(("x-bili-locale-bin".into(), locale.encode_to_vec()));
        map.push(("x-bili-restriction-bin".into(), restriction.encode_to_vec()));
        map.push(("x-bili-fawkes-req-bin".into(), fawkes.encode_to_vec()));
        map.push(("env".into(), b"prod".to_vec()));
        map
    }

    async fn unary<Req, Resp>(&self, path: &'static str, msg: Req) -> Result<Resp>
    where
        Req: Message,
        Resp: Message + Default,
    {
        let mut headers = if self.send_metadata {
            encode_metadata_wire(self.metadata())
        } else {
            vec![("env".to_string(), b"prod".to_vec())]
        };
        headers.extend(self.extra_headers.iter().cloned());
        transport::grpc_unary(&self.egress, self.host, path, &headers, &msg).await
    }

    // ------------------------------------------------------------------
    // Search
    // ------------------------------------------------------------------

    /// Raw SearchAll call (typed).
    pub async fn search_all(&self, req: SearchAllRequest) -> Result<SearchAllResponse> {
        self.unary(PATH_SEARCH_ALL, req).await
    }

    /// Keyword search, shaped like the other ZeroSearch shopping/web
    /// providers' JSON results. `next` is the opaque pagination token from
    /// a previous page (empty for the first page).
    pub async fn search_videos(
        &self,
        keyword: &str,
        page_size: i32,
        next: &str,
        order: i32,
    ) -> Result<Value> {
        let resp = self
            .search_all(SearchAllRequest {
                keyword: keyword.to_string(),
                order,
                from_source: "search".to_string(),
                pagination: Some(Pagination {
                    page_size: if page_size <= 0 { 20 } else { page_size },
                    next: next.to_string(),
                }),
                ..Default::default()
            })
            .await?;
        Ok(search_response_json(keyword, &resp))
    }

    // ------------------------------------------------------------------
    // Video detail / related
    // ------------------------------------------------------------------

    /// Video detail via viewunite `View` (the app's video-page API).
    /// Accepts a BV id (preferred) — pass `aid` instead by using
    /// [`Self::get_video_by_aid`].
    pub async fn get_video(&self, bvid: &str) -> Result<Value> {
        let resp: ViewReply = self
            .unary(
                PATH_VIEW,
                ViewReq {
                    bvid: bvid.to_string(),
                    player_args: Some(default_player_args()),
                    ..Default::default()
                },
            )
            .await?;
        Ok(view_reply_json(bvid, &resp))
    }

    pub async fn get_video_by_aid(&self, aid: u64) -> Result<Value> {
        let resp: ViewReply = self
            .unary(
                PATH_VIEW,
                ViewReq {
                    aid,
                    player_args: Some(default_player_args()),
                    ..Default::default()
                },
            )
            .await?;
        Ok(view_reply_json(&aid.to_string(), &resp))
    }

    /// Related videos under the video page (`RelatesFeed`), by aid.
    pub async fn get_related_by_aid(&self, aid: u64, page_size: i32, next: &str) -> Result<Value> {
        let resp: RelatesFeedReply = self
            .unary(
                PATH_RELATES,
                RelatesFeedReq {
                    aid,
                    pagination: Some(Pagination {
                        page_size: if page_size <= 0 { 20 } else { page_size },
                        next: next.to_string(),
                    }),
                    ..Default::default()
                },
            )
            .await?;
        Ok(relates_reply_json(&resp))
    }

    /// Related videos under the video page (`RelatesFeed`).
    pub async fn get_related(&self, bvid: &str, page_size: i32, next: &str) -> Result<Value> {
        let resp: RelatesFeedReply = self
            .unary(
                PATH_RELATES,
                RelatesFeedReq {
                    bvid: bvid.to_string(),
                    pagination: Some(Pagination {
                        page_size: if page_size <= 0 { 20 } else { page_size },
                        next: next.to_string(),
                    }),
                    ..Default::default()
                },
            )
            .await?;
        let mut out = relates_reply_json(&resp);
        out["bvid"] = json!(bvid);
        Ok(out)
    }
}

/// Shape a RelatesFeedReply into the provider JSON result.
pub fn relates_reply_json(resp: &RelatesFeedReply) -> Value {
    let items: Vec<Value> = resp
        .relates
        .iter()
        .filter_map(|card| {
            let info = card.basic_info.as_ref()?;
            let (duration, stat) = match card.av.as_ref() {
                Some(av) => (av.duration, av.stat.as_ref()),
                None => (0, None),
            };
            let mut bvid = extract_bvid(&info.uri);
            if bvid.is_empty() && info.id > 0 {
                bvid = aid_to_bvid(info.id as u64);
            }
            Some(json!({
                "title": info.title,
                "desc": info.desc,
                "cover": info.cover,
                "uri": info.uri,
                "bvid": bvid,
                "aid": info.id,
                "duration": duration,
                "play": stat.and_then(|s| s.vt.as_ref()).map(|v| v.value),
            }))
        })
        .collect();
    json!({
        "provider": PROVIDER,
        "ok": true,
        "count": items.len(),
        "items": items,
        "next": resp.pagination.as_ref().map(|p| p.next.clone()).unwrap_or_default(),
    })
}

impl Default for BilibiliClient {
    fn default() -> Self {
        Self::new()
    }
}

/// The wire representation of the metadata headers on the public
/// ingress: every `*-bin` value is base64-encoded (standard alphabet,
/// with padding); plain headers such as `env` pass through unchanged.
///
/// Round 2 established this empirically: raw protobuf values are
/// rejected by the front with HTTP 400 (its header validation refuses
/// control bytes), while the grpc-go backend base64-decodes `-bin`
/// values — feeding it printable-but-not-base64 bytes produced its
/// `malformed binary metadata ... illegal base64 data` error. With
/// base64 values, search, `View` and `RelatesFeed` all succeed.
pub fn encode_metadata_wire(headers: Vec<(String, Vec<u8>)>) -> Vec<(String, Vec<u8>)> {
    use base64::Engine;
    headers
        .into_iter()
        .map(|(name, value)| {
            if name.ends_with("-bin") {
                let encoded = base64::engine::general_purpose::STANDARD.encode(&value);
                (name, encoded.into_bytes())
            } else {
                (name, value)
            }
        })
        .collect()
}

/// Player arguments the app attaches to view requests (guest defaults:
/// qn 80 = 1080P tier request, fnval 16 = mp4 preference bitmask).
fn default_player_args() -> PlayerArgs {
    PlayerArgs {
        qn: 80,
        fnver: 0,
        fnval: 16,
        force_host: 0,
        voice_balance: 0,
    }
}

fn current_time_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Shape an error into the shared `{provider, ok:false, error}` JSON,
/// for tool-layer callers that prefer values over errors.
pub fn error_json(err: &anyhow::Error) -> Value {
    json!({
        "provider": PROVIDER,
        "ok": false,
        "error": {
            "kind": "grpc",
            "message": format!("{err:#}"),
        }
    })
}

/// Convert a numeric aid to its BV id, using the same base-58 mapping
/// the app applies locally (BV ids are a pure function of the aid).
pub fn aid_to_bvid(aid: u64) -> String {
    const TABLE: &[u8; 58] = b"fZodR9XQDSUm21yCkr6zBqiveYah8bt4xsWpHnJE7jL5VG3guMTKNPAwcF";
    const POS: [usize; 6] = [11, 10, 3, 8, 4, 6];
    // Template with the fixed digits of the BV format (index 5 = '4',
    // 7 = '1', 9 = '7'); the six variable slots are filled from POS.
    let mut buf = *b"BV1004010700";
    let mut tmp = (aid ^ 177451812u64).wrapping_add(8728348608u64);
    for pos in POS {
        buf[pos] = TABLE[(tmp % 58) as usize];
        tmp /= 58;
    }
    String::from_utf8(buf.to_vec()).expect("ascii")
}

/// Extract a BV id (`BV` + 10 alphanumerics) from a bilibili URI, if present.
pub fn extract_bvid(uri: &str) -> String {
    let bytes = uri.as_bytes();
    for i in 0..bytes.len().saturating_sub(11) {
        if &uri[i..i + 2] == "BV" {
            let cand = &uri[i..i + 12];
            if cand.chars().all(|c| c.is_ascii_alphanumeric()) {
                return cand.to_string();
            }
        }
    }
    String::new()
}

/// Shape a SearchAllResponse into the provider JSON result.
pub fn search_response_json(keyword: &str, resp: &SearchAllResponse) -> Value {
    let items: Vec<Value> = resp
        .item
        .iter()
        .filter_map(|item| {
            let card = item.av.as_ref()?;
            let aid: i64 = item.param.parse().unwrap_or(0);
            let mut bvid = extract_bvid(&item.uri);
            if bvid.is_empty() && aid > 0 {
                bvid = aid_to_bvid(aid as u64);
            }
            Some(json!({
                "title": strip_highlight(&card.title),
                "bvid": bvid,
                "aid": aid,
                "author": card.author,
                "mid": card.mid,
                "cover": card.cover,
                "duration": card.duration,
                "play": card.play,
                "danmaku": card.danmaku,
                "desc": card.desc,
                "goto": item.goto,
                "uri": item.uri,
                "url": if bvid.is_empty() { item.uri.clone() } else { format!("https://www.bilibili.com/video/{bvid}") },
            }))
        })
        .collect();
    json!({
        "provider": PROVIDER,
        "ok": true,
        "query": keyword,
        "trackid": resp.trackid,
        "count": items.len(),
        "items": items,
        "next": resp.pagination.as_ref().map(|p| p.next.clone()).unwrap_or_default(),
    })
}

/// Shape a ViewReply into the provider JSON result.
pub fn view_reply_json(requested: &str, resp: &ViewReply) -> Value {
    let arc = resp.arc.as_ref();
    let stat = arc.and_then(|a| a.stat.as_ref());
    let owner = resp.owner.as_ref();
    json!({
        "provider": PROVIDER,
        "ok": true,
        "item": {
            "aid": arc.map(|a| a.aid).unwrap_or(0),
            "bvid": arc.map(|a| a.bvid.clone()).unwrap_or_default(),
            "cid": arc.map(|a| a.cid).unwrap_or(0),
            "title": arc.map(|a| a.title.clone()).unwrap_or_default(),
            "cover": arc.map(|a| a.cover.clone()).unwrap_or_default(),
            "duration": arc.map(|a| a.duration).unwrap_or(0),
            "type_id": arc.map(|a| a.type_id).unwrap_or(0),
            "copyright": arc.map(|a| a.copyright).unwrap_or(0),
            "requested": requested,
            "owner": {
                "mid": owner.and_then(|o| o.avatar.as_ref()).map(|a| a.mid).unwrap_or(0),
                "name": owner.map(|o| o.title.clone()).unwrap_or_default(),
                "fans": owner.map(|o| o.fans.clone()).unwrap_or_default(),
            },
            "stat": {
                "view": stat.and_then(|s| s.vt.as_ref()).map(|v| v.value).unwrap_or(0),
                "danmaku": stat.and_then(|s| s.danmaku.as_ref()).map(|v| v.value).unwrap_or(0),
                "reply": stat.map(|s| s.reply).unwrap_or(0),
                "fav": stat.map(|s| s.fav).unwrap_or(0),
                "coin": stat.map(|s| s.coin).unwrap_or(0),
                "share": stat.map(|s| s.share).unwrap_or(0),
                "like": stat.map(|s| s.like).unwrap_or(0),
            },
        }
    })
}

/// Search titles come back with `<em class="keyword">…</em>` highlight
/// markup; strip tags for display use.
pub fn strip_highlight(title: &str) -> String {
    let mut out = String::with_capacity(title.len());
    let mut in_tag = false;
    for c in title.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}
