//! Protobuf message definitions (prost, hand-declared).
//!
//! Schemas recovered from the Bilibili Android app (tv.danmaku.bili 9.18.0)
//! generated classes and cross-checked against the community proto dump
//! (bilibili-API-collect `grpc_api/`). Only the fields this provider reads
//! or writes are declared; unknown fields on the wire are ignored by prost,
//! which keeps the decoders forward-compatible with newer app schemas.
//!
//! Field-number evidence: `SearchAllRequest` constants were verified in the
//! decompiled generated class
//! `com.bapis.bilibili.polymer.app.search.v1.SearchAllRequest`
//! (KEYWORD=1, ORDER=2, TID_LIST=3, DURATION_LIST=4, EXTRA_WORD=5,
//! FROM_SOURCE=6, IS_ORG_QUERY=7, LOCAL_TIME=8, AD_EXTRA=9, PAGINATION=10,
//! PLAYER_ARGS=11; app 9.18 adds FROM_EXTRA=12, IS_REFRESH=14,
//! PUB_TIME_BEGIN_S=17, PUB_TIME_END_S=18 among others).

use prost::Message;

// ---------------------------------------------------------------------------
// gRPC metadata payloads (bilibili.metadata.*), sent as `x-bili-*-bin` headers
// ---------------------------------------------------------------------------

/// `bilibili.metadata.Metadata` -> header `x-bili-metadata-bin`.
#[derive(Clone, PartialEq, Message)]
pub struct Metadata {
    /// Login access_key; empty for guests.
    #[prost(string, tag = "1")]
    pub access_key: String,
    /// e.g. `android`.
    #[prost(string, tag = "2")]
    pub mobi_app: String,
    /// Left empty by the app.
    #[prost(string, tag = "3")]
    pub device: String,
    /// App build number (versionCode), e.g. 9180300 for 9.18.0.
    #[prost(int32, tag = "4")]
    pub build: i32,
    /// Distribution channel, e.g. `master`.
    #[prost(string, tag = "5")]
    pub channel: String,
    /// Device buvid (see `crate::buvid`).
    #[prost(string, tag = "6")]
    pub buvid: String,
    /// e.g. `android`.
    #[prost(string, tag = "7")]
    pub platform: String,
}

/// `bilibili.metadata.device.Device` -> header `x-bili-device-bin`.
#[derive(Clone, PartialEq, Message)]
pub struct Device {
    /// 1 = main (pink) app.
    #[prost(int32, tag = "1")]
    pub app_id: i32,
    #[prost(int32, tag = "2")]
    pub build: i32,
    #[prost(string, tag = "3")]
    pub buvid: String,
    #[prost(string, tag = "4")]
    pub mobi_app: String,
    #[prost(string, tag = "5")]
    pub platform: String,
    #[prost(string, tag = "6")]
    pub device: String,
    #[prost(string, tag = "7")]
    pub channel: String,
    #[prost(string, tag = "8")]
    pub brand: String,
    #[prost(string, tag = "9")]
    pub model: String,
    #[prost(string, tag = "10")]
    pub osver: String,
    #[prost(string, tag = "11")]
    pub fp_local: String,
    #[prost(string, tag = "12")]
    pub fp_remote: String,
    #[prost(string, tag = "13")]
    pub version_name: String,
    #[prost(string, tag = "14")]
    pub fp: String,
    /// First-launch timestamp (ms).
    #[prost(int64, tag = "15")]
    pub fts: i64,
}

/// `bilibili.metadata.network.Network` -> header `x-bili-network-bin`.
#[derive(Clone, PartialEq, Message)]
pub struct Network {
    /// NetworkType: 1 = WIFI.
    #[prost(enumeration = "NetworkType", tag = "1")]
    pub r#type: i32,
    #[prost(enumeration = "TfType", tag = "2")]
    pub tf: i32,
    #[prost(string, tag = "3")]
    pub oid: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, prost::Enumeration)]
#[repr(i32)]
pub enum NetworkType {
    Unknown = 0,
    Wifi = 1,
    Cellular = 2,
    Offline = 3,
    Othernet = 4,
    Ethernet = 5,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, prost::Enumeration)]
#[repr(i32)]
pub enum TfType {
    Unknown = 0,
    UCard = 1,
    UPkg = 2,
    CCard = 3,
    CPkg = 4,
    TCard = 5,
    TPkg = 6,
}

/// `bilibili.metadata.locale.LocaleIds`.
#[derive(Clone, PartialEq, Message)]
pub struct LocaleIds {
    #[prost(string, tag = "1")]
    pub language: String,
    #[prost(string, tag = "2")]
    pub script: String,
    #[prost(string, tag = "3")]
    pub region: String,
}

/// `bilibili.metadata.locale.Locale` -> header `x-bili-locale-bin`.
#[derive(Clone, PartialEq, Message)]
pub struct Locale {
    #[prost(message, optional, tag = "1")]
    pub c_locale: Option<LocaleIds>,
    #[prost(message, optional, tag = "2")]
    pub s_locale: Option<LocaleIds>,
    #[prost(string, tag = "3")]
    pub sim_code: String,
    #[prost(string, tag = "4")]
    pub timezone: String,
}

/// `bilibili.metadata.restriction.Restriction` -> header `x-bili-restriction-bin`.
#[derive(Clone, PartialEq, Message)]
pub struct Restriction {
    #[prost(bool, tag = "1")]
    pub teenagers_mode: bool,
    #[prost(bool, tag = "2")]
    pub lessons_mode: bool,
    /// ModeType: 0 = NORMAL.
    #[prost(enumeration = "ModeType", tag = "3")]
    pub mode: i32,
    #[prost(bool, tag = "4")]
    pub review: bool,
    #[prost(bool, tag = "5")]
    pub disable_rcmd: bool,
    #[prost(bool, tag = "6")]
    pub basic_mode: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, prost::Enumeration)]
#[repr(i32)]
pub enum ModeType {
    Normal = 0,
    Teenagers = 1,
    Lessons = 2,
    Basic = 3,
}

/// `bilibili.metadata.fawkes.FawkesReq` -> header `x-bili-fawkes-req-bin`.
#[derive(Clone, PartialEq, Message)]
pub struct FawkesReq {
    /// Client name in the fawkes system, e.g. `android64`.
    #[prost(string, tag = "1")]
    pub appkey: String,
    /// Environment, e.g. `prod`.
    #[prost(string, tag = "2")]
    pub env: String,
    /// Per-launch session id: 8 chars of [0-9a-z].
    #[prost(string, tag = "3")]
    pub session_id: String,
}

// ---------------------------------------------------------------------------
// Shared pagination / player args
// ---------------------------------------------------------------------------

/// `bilibili.pagination.Pagination`.
#[derive(Clone, PartialEq, Message)]
pub struct Pagination {
    #[prost(int32, tag = "1")]
    pub page_size: i32,
    /// Opaque continuation token; empty for the first page.
    #[prost(string, tag = "2")]
    pub next: String,
}

/// `bilibili.pagination.PaginationReply`.
#[derive(Clone, PartialEq, Message)]
pub struct PaginationReply {
    #[prost(string, tag = "1")]
    pub next: String,
    #[prost(string, tag = "2")]
    pub prev: String,
}

/// `bilibili.app.archive.middleware.v1.PlayerArgs`.
#[derive(Clone, PartialEq, Message)]
pub struct PlayerArgs {
    #[prost(int64, tag = "1")]
    pub qn: i64,
    #[prost(int64, tag = "2")]
    pub fnver: i64,
    #[prost(int64, tag = "3")]
    pub fnval: i64,
    #[prost(int64, tag = "4")]
    pub force_host: i64,
    #[prost(int64, tag = "5")]
    pub voice_balance: i64,
}

// ---------------------------------------------------------------------------
// Search: bilibili.polymer.app.search.v1.Search
// ---------------------------------------------------------------------------

/// `bilibili.polymer.app.search.v1.SearchAllRequest`.
#[derive(Clone, PartialEq, Message)]
pub struct SearchAllRequest {
    #[prost(string, tag = "1")]
    pub keyword: String,
    /// Sort order; 0 = app default (comprehensive).
    #[prost(int32, tag = "2")]
    pub order: i32,
    /// Region (tid) filter list, app format.
    #[prost(string, tag = "3")]
    pub tid_list: String,
    /// Duration filter list, app format.
    #[prost(string, tag = "4")]
    pub duration_list: String,
    #[prost(string, tag = "5")]
    pub extra_word: String,
    #[prost(string, tag = "6")]
    pub from_source: String,
    #[prost(int32, tag = "7")]
    pub is_org_query: i32,
    #[prost(int32, tag = "8")]
    pub local_time: i32,
    #[prost(string, tag = "9")]
    pub ad_extra: String,
    #[prost(message, optional, tag = "10")]
    pub pagination: Option<Pagination>,
    #[prost(message, optional, tag = "11")]
    pub player_args: Option<PlayerArgs>,
    /// App 9.18 extension: publish-time lower bound (unix seconds).
    #[prost(int64, tag = "17")]
    pub pub_time_begin_s: i64,
    /// App 9.18 extension: publish-time upper bound (unix seconds).
    #[prost(int64, tag = "18")]
    pub pub_time_end_s: i64,
}

/// `bilibili.polymer.app.search.v1.SearchAllResponse` (subset).
#[derive(Clone, PartialEq, Message)]
pub struct SearchAllResponse {
    #[prost(string, tag = "1")]
    pub keyword: String,
    #[prost(string, tag = "2")]
    pub trackid: String,
    #[prost(message, repeated, tag = "4")]
    pub item: Vec<SearchItem>,
    #[prost(message, optional, tag = "11")]
    pub pagination: Option<PaginationReply>,
}

/// `bilibili.polymer.app.search.v1.Item` (subset: the video card oneof arm).
#[derive(Clone, PartialEq, Message)]
pub struct SearchItem {
    #[prost(string, tag = "1")]
    pub uri: String,
    /// For video cards this is the aid as a decimal string.
    #[prost(string, tag = "2")]
    pub param: String,
    /// Card kind, e.g. `av`.
    #[prost(string, tag = "3")]
    pub goto: String,
    #[prost(int32, tag = "5")]
    pub position: i32,
    /// oneof card_item: SearchVideoCard at field 37.
    #[prost(message, optional, tag = "37")]
    pub av: Option<SearchVideoCard>,
}

/// `bilibili.polymer.app.search.v1.SearchVideoCard` (subset).
#[derive(Clone, PartialEq, Message)]
pub struct SearchVideoCard {
    #[prost(string, tag = "1")]
    pub title: String,
    #[prost(string, tag = "2")]
    pub cover: String,
    #[prost(int32, tag = "8")]
    pub play: i32,
    #[prost(int32, tag = "9")]
    pub danmaku: i32,
    #[prost(string, tag = "10")]
    pub author: String,
    #[prost(string, tag = "11")]
    pub desc: String,
    /// Duration text, e.g. `12:34`.
    #[prost(string, tag = "12")]
    pub duration: String,
    #[prost(int64, tag = "14")]
    pub mid: i64,
}

// ---------------------------------------------------------------------------
// View: bilibili.app.viewunite.v1.View
// ---------------------------------------------------------------------------

/// `bilibili.app.viewunite.v1.ViewReq` (subset).
#[derive(Clone, PartialEq, Message)]
pub struct ViewReq {
    #[prost(uint64, tag = "1")]
    pub aid: u64,
    #[prost(string, tag = "2")]
    pub bvid: String,
    #[prost(string, tag = "3")]
    pub from: String,
    #[prost(string, tag = "4")]
    pub spmid: String,
    #[prost(string, tag = "5")]
    pub from_spmid: String,
    #[prost(string, tag = "6")]
    pub session_id: String,
    #[prost(message, optional, tag = "7")]
    pub player_args: Option<PlayerArgs>,
    #[prost(string, tag = "8")]
    pub track_id: String,
}

/// `bilibili.app.viewunite.v1.ViewReply` (subset).
#[derive(Clone, PartialEq, Message)]
pub struct ViewReply {
    #[prost(message, optional, tag = "2")]
    pub arc: Option<Arc>,
    #[prost(message, optional, tag = "4")]
    pub owner: Option<Owner>,
}

/// `bilibili.app.viewunite.v1.Arc` (subset per the recovered schema).
#[derive(Clone, PartialEq, Message)]
pub struct Arc {
    #[prost(int64, tag = "1")]
    pub aid: i64,
    #[prost(int64, tag = "2")]
    pub cid: i64,
    #[prost(int64, tag = "3")]
    pub duration: i64,
    #[prost(message, optional, tag = "4")]
    pub stat: Option<Stat>,
    #[prost(string, tag = "5")]
    pub bvid: String,
    #[prost(int32, tag = "6")]
    pub copyright: i32,
    #[prost(string, tag = "8")]
    pub cover: String,
    #[prost(int64, tag = "9")]
    pub type_id: i64,
    #[prost(string, tag = "10")]
    pub title: String,
}

/// `bilibili.app.viewunite.common.Stat` (subset).
#[derive(Clone, PartialEq, Message)]
pub struct Stat {
    #[prost(message, optional, tag = "1")]
    pub vt: Option<StatInfo>,
    #[prost(message, optional, tag = "2")]
    pub danmaku: Option<StatInfo>,
    #[prost(int64, tag = "3")]
    pub reply: i64,
    #[prost(int64, tag = "4")]
    pub fav: i64,
    #[prost(int64, tag = "5")]
    pub coin: i64,
    #[prost(int64, tag = "6")]
    pub share: i64,
    #[prost(int64, tag = "7")]
    pub like: i64,
}

/// `bilibili.app.viewunite.common.StatInfo`.
#[derive(Clone, PartialEq, Message)]
pub struct StatInfo {
    #[prost(int64, tag = "1")]
    pub value: i64,
    #[prost(string, tag = "2")]
    pub text: String,
}

/// `bilibili.app.viewunite.common.Owner` (subset).
#[derive(Clone, PartialEq, Message)]
pub struct Owner {
    #[prost(message, optional, tag = "1")]
    pub avatar: Option<AvatarItem>,
    /// Uploader display name in this surface.
    #[prost(string, tag = "3")]
    pub title: String,
    #[prost(string, tag = "4")]
    pub fans: String,
}

/// `bilibili.dagw.component.avatar.v1.AvatarItem` (subset).
#[derive(Clone, PartialEq, Message)]
pub struct AvatarItem {
    #[prost(int64, tag = "4")]
    pub mid: i64,
}

/// `bilibili.app.viewunite.v1.RelatesFeedReq` (subset).
#[derive(Clone, PartialEq, Message)]
pub struct RelatesFeedReq {
    #[prost(uint64, tag = "1")]
    pub aid: u64,
    #[prost(string, tag = "2")]
    pub bvid: String,
    #[prost(string, tag = "3")]
    pub from: String,
    #[prost(string, tag = "4")]
    pub spmid: String,
    #[prost(message, optional, tag = "7")]
    pub pagination: Option<Pagination>,
}

/// `bilibili.app.viewunite.v1.RelatesFeedReply` (subset).
#[derive(Clone, PartialEq, Message)]
pub struct RelatesFeedReply {
    #[prost(message, repeated, tag = "1")]
    pub relates: Vec<RelateCard>,
    #[prost(message, optional, tag = "2")]
    pub pagination: Option<PaginationReply>,
}

/// `bilibili.app.viewunite.common.RelateCard` (subset).
#[derive(Clone, PartialEq, Message)]
pub struct RelateCard {
    #[prost(message, optional, tag = "2")]
    pub av: Option<RelateAvCard>,
    #[prost(message, optional, tag = "12")]
    pub basic_info: Option<CardBasicInfo>,
}

/// `bilibili.app.viewunite.common.RelateAVCard` (subset).
#[derive(Clone, PartialEq, Message)]
pub struct RelateAvCard {
    #[prost(int64, tag = "1")]
    pub duration: i64,
    #[prost(message, optional, tag = "4")]
    pub stat: Option<Stat>,
}

/// `bilibili.app.viewunite.common.CardBasicInfo` (subset).
#[derive(Clone, PartialEq, Message)]
pub struct CardBasicInfo {
    #[prost(string, tag = "1")]
    pub title: String,
    #[prost(string, tag = "2")]
    pub desc: String,
    #[prost(string, tag = "3")]
    pub cover: String,
    #[prost(string, tag = "4")]
    pub uri: String,
    #[prost(int64, tag = "12")]
    pub id: i64,
}

// ---------------------------------------------------------------------------
// Legacy bilibili.app.view.v1.View (probe surface: the archive payload is
// kept raw until its field mapping is confirmed against a live reply)
// ---------------------------------------------------------------------------

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct LegacyViewReq {
    #[prost(int64, tag = "1")]
    pub aid: i64,
    #[prost(string, tag = "2")]
    pub bvid: String,
    #[prost(string, tag = "3")]
    pub from: String,
    #[prost(message, optional, tag = "14")]
    pub player_args: Option<PlayerArgs>,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct LegacyViewReply {
    #[prost(bytes = "vec", tag = "1")]
    pub arc_raw: Vec<u8>,
    #[prost(string, tag = "14")]
    pub bvid: String,
}
