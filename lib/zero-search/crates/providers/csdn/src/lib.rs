//! CSDN provider reproducing the Android app's own retrieval routes
//! (net.csdn.csdnplus, analyzed build 6.5.7).
//!
//! Search and article detail both go through the app gateway
//! `https://app-gw.csdn.net` with the app's X-Ca HMAC signature
//! (see `sign.rs`):
//!
//! - Search: `POST /silkroad-api/api/v2/assemble/list/pub/channel/app_search_result_v6`
//!   with the `SearchVipRequestEntity` JSON body the app builds for
//!   its search tabs. The app selects a result category by setting
//!   `SearchVipRequestExt.type` and `.block` to the same value
//!   (`all_v2`, `blog`, `ask`, `download`, `appcourse`, `sku`,
//!   `ebook`, `userinfo`, `vip`, `special_column`). A non-empty
//!   `search_user_name` switches the app to the
//!   `app_personal_home_search` channel.
//! - Detail: `GET /blog/phoenix/app/v1/get-article-basic-info?articleId=..&skin=`
//!   (`BlogPhoenixService`), returning the article including its
//!   `contentHtml` body.
//!
//! Both work as a guest: the app sends `user_name: ""` and no login
//! headers while logged out, and so does this client.

pub mod sign;

use anyhow::{anyhow, Context};
use serde_json::{json, Value};

pub const PROVIDER: &str = "csdn";

const SEARCH_PATH: &str = "/silkroad-api/api/v2/assemble/list/pub/channel/app_search_result_v6";
const SEARCH_CHANNEL: &str = "app_search_result_v6";
const PERSONAL_SEARCH_PATH: &str =
    "/silkroad-api/api/v2/assemble/list/pub/channel/app_personal_home_search";
const PERSONAL_SEARCH_CHANNEL: &str = "app_personal_home_search";
const DETAIL_PATH: &str = "/blog/phoenix/app/v1/get-article-basic-info";

/// A CSDN assembled-search category.
///
/// The Android request classes set both `type` and `block` in
/// `SearchVipRequestExt` to the category's request value:
///
/// - `SearchAllListRequest` (`bi4` in 6.5.7, `xg5` in 6.15.2):
///   `all` (6.5.7) / `all_v2` (current app; the server accepts both
///   and `all_v2` returns the current mix, so that is what we send)
/// - `SearchBlogListRequest` (`di4`): `blog`
/// - `SearchAskListRequest` (`ci4`): `ask`
/// - `SearchDownloadListRequest` (`ni4`): `download`
/// - `SearchCourseListRequest` (`ki4`): `appcourse`
/// - `SearchMallListRequest` (`jj4`): `sku`
/// - `SearchEpubListRequest` (`pi4`): `ebook`
/// - `SearchUserListRequest` (`uj4`): `userinfo`
/// - `SearchVipListRequest` (`ak4`): `vip`
/// - 6.15.2 additionally ships `SearchColumnFragment`, setting the
///   value `special_column` (columns / 专栏).
///
/// There is no forum search in the app (verified in both 6.5.7 and
/// 6.15.2: no search tab, no request class; the `bbs` value the
/// gateway tolerates returns the download-weighted default mix, not
/// forum threads), so no forum category is exposed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SearchCategory {
    All,
    Blog,
    Ask,
    Download,
    Course,
    Mall,
    Ebook,
    User,
    Vip,
    SpecialColumn,
}

impl SearchCategory {
    /// Parse the public category name (or the app's raw request value).
    pub fn parse(input: &str) -> Option<Self> {
        match input.trim().to_ascii_lowercase().as_str() {
            "all" | "all_v2" | "综合" => Some(Self::All),
            "blog" | "博客" => Some(Self::Blog),
            "ask" | "问答" => Some(Self::Ask),
            "download" | "下载" => Some(Self::Download),
            "course" | "appcourse" | "课程" => Some(Self::Course),
            "mall" | "sku" | "商城" => Some(Self::Mall),
            "ebook" | "电子书" => Some(Self::Ebook),
            "user" | "userinfo" | "用户" => Some(Self::User),
            "vip" => Some(Self::Vip),
            "special_column" | "专栏" => Some(Self::SpecialColumn),
            _ => None,
        }
    }

    /// Public, stable category name used by the tool surface.
    pub fn name(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Blog => "blog",
            Self::Ask => "ask",
            Self::Download => "download",
            Self::Course => "course",
            Self::Mall => "mall",
            Self::Ebook => "ebook",
            Self::User => "user",
            Self::Vip => "vip",
            Self::SpecialColumn => "special_column",
        }
    }

    /// The exact value the app writes to both `type` and `block`.
    pub fn request_value(self) -> &'static str {
        match self {
            Self::All => "all_v2",
            Self::Blog => "blog",
            Self::Ask => "ask",
            Self::Download => "download",
            Self::Course => "appcourse",
            Self::Mall => "sku",
            Self::Ebook => "ebook",
            Self::User => "userinfo",
            Self::Vip => "vip",
            Self::SpecialColumn => "special_column",
        }
    }

    /// Categories whose app request class actually switches to the
    /// personal-home channel when `search_user_name` is present.
    /// (`PersonalSearchContentView` exposes all/blog/download/ask;
    /// `SearchVipListRequest` also implements the switch. The mall
    /// request's personal setter ignores the username, and the
    /// course/ebook/user requests have no personal switch.)
    pub fn supports_personal_home(self) -> bool {
        matches!(
            self,
            Self::All | Self::Blog | Self::Ask | Self::Download | Self::Vip
        )
    }

    /// Categories whose app request class has the sort/subfilter
    /// setter pair (`p(String, String)` in the obfuscated classes).
    pub fn supports_sort_filter(self) -> bool {
        matches!(
            self,
            Self::All | Self::Blog | Self::Ask | Self::Download | Self::Course | Self::Mall
        )
    }
}

/// Options for an assembled-search request.
///
/// `sort` accepts the friendly names `relevance`, `latest` and
/// `popular`, the app's raw values (`times_rank`, `hot_rank`), or any
/// other raw `second_rank_name_sort` value for compatibility.
/// `filter` accepts the app's friendly toggle names (`vip_content`,
/// `accepted`); `subfilter` is the raw expression and takes
/// precedence over `filter` when both are set.
#[derive(Clone, Debug)]
pub struct SearchOptions<'a> {
    pub query: &'a str,
    pub page: u32,
    pub limit: u32,
    pub category: SearchCategory,
    pub sort: Option<&'a str>,
    pub filter: Option<&'a str>,
    pub subfilter: Option<&'a str>,
    pub search_user_name: Option<&'a str>,
}

impl<'a> SearchOptions<'a> {
    pub fn new(query: &'a str) -> Self {
        Self {
            query,
            page: 1,
            limit: 20,
            category: SearchCategory::All,
            sort: None,
            filter: None,
            subfilter: None,
            search_user_name: None,
        }
    }

    pub fn personal_home(&self) -> bool {
        self.search_user_name
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false)
    }
}

/// Map a friendly sort name to the app's `second_rank_name_sort`
/// value. The app UI (`SearchContainerFragment.Q`) has exactly three
/// choices: 综合 (no field), 最新 (`times_rank`) and 热门
/// (`hot_rank`). Unknown non-empty values pass through unchanged,
/// preserving the round-1 behaviour.
pub fn normalize_sort(sort: Option<&str>) -> Option<String> {
    let raw = sort?.trim();
    if raw.is_empty() {
        return None;
    }
    match raw.to_ascii_lowercase().as_str() {
        "relevance" | "comprehensive" | "default" | "综合" => None,
        "latest" | "newest" | "time" | "times_rank" | "最新" => Some("times_rank".to_string()),
        "popular" | "hot" | "hot_rank" | "热门" => Some("hot_rank".to_string()),
        _ => Some(raw.to_string()),
    }
}

/// The app's one-button subfilter toggle, by category
/// (`SearchContainerFragment$a.onClick`):
///
/// - blog, toggle labelled “VIP内容”: `vip_view_auth="1"`
/// - mall, same toggle: `ext_1=1`
/// - course, same toggle: `ext_21="2"`
/// - ask, toggle labelled “已采纳” (accepted): `ext_21!=""`
pub fn app_subfilter(category: SearchCategory, filter: Option<&str>) -> Option<String> {
    let raw = filter?.trim();
    if raw.is_empty() {
        return None;
    }
    match (category, raw.to_ascii_lowercase().as_str()) {
        (SearchCategory::Blog, "vip_content" | "vip" | "vip内容") => {
            Some("vip_view_auth=\"1\"".to_string())
        }
        (SearchCategory::Mall, "vip_content" | "vip" | "vip内容") => Some("ext_1=1".to_string()),
        (SearchCategory::Course, "vip_content" | "vip" | "vip内容") => {
            Some("ext_21=\"2\"".to_string())
        }
        (SearchCategory::Ask, "accepted" | "已采纳") => Some("ext_21!=\"\"".to_string()),
        _ => None,
    }
}

/// Effective raw subfilter: an explicit `subfilter` wins over the
/// friendly `filter` toggle.
pub fn effective_subfilter(options: &SearchOptions<'_>) -> Option<String> {
    if let Some(raw) = options.subfilter {
        let raw = raw.trim();
        if !raw.is_empty() {
            return Some(raw.to_string());
        }
    }
    app_subfilter(options.category, options.filter)
}

/// Validate option combinations against what the app implements.
pub fn validate_search_options(options: &SearchOptions<'_>) -> anyhow::Result<()> {
    if options.personal_home() && !options.category.supports_personal_home() {
        return Err(anyhow!(
            "category {} does not implement personal-home search in the app",
            options.category.name()
        ));
    }
    if let Some(filter) = options.filter {
        if !filter.trim().is_empty() && app_subfilter(options.category, Some(filter)).is_none() {
            return Err(anyhow!(
                "filter {filter} is not an app filter for category {}",
                options.category.name()
            ));
        }
    }
    Ok(())
}

/// Build the exact `SearchVipRequestEntity` JSON body for `options`,
/// following the per-category request classes. Fields a request class
/// never sets are omitted (Gson omits null fields); the entity's
/// `use_highlight` defaults to `true` in its constructor.
pub fn build_search_body(options: &SearchOptions<'_>) -> Value {
    let page = options.page.max(1);
    let limit = options.limit.clamp(1, 50);
    let category = options.category.request_value();
    let personal = options.personal_home();
    let mut ext = json!({
        "keywords": options.query,
        "page": page,
        "pageSize": limit,
        "type": category,
        "block": category,
        "use_highlight": true,
        "user_name": "",
        "imei": "",
        "oaid": ""
    });
    if personal {
        ext["search_user_name"] = json!(options.search_user_name.unwrap_or("").trim());
    }
    // Per-class extras: only the "all" request sets insert_card,
    // search_version and version; "all" and "download" set
    // completion_down.
    if options.category == SearchCategory::All {
        ext["insert_card"] = json!("1");
        ext["completion_down"] = json!(1);
        ext["search_version"] = json!("v4");
        ext["version"] = json!(1);
    } else if options.category == SearchCategory::Download {
        ext["completion_down"] = json!(1);
    }
    if let Some(sort) = normalize_sort(options.sort) {
        ext["second_rank_name_sort"] = json!(sort);
    }
    if let Some(subfilter) = effective_subfilter(options) {
        ext["subfilter"] = json!(subfilter);
    }
    let channel = if personal {
        PERSONAL_SEARCH_CHANNEL
    } else {
        SEARCH_CHANNEL
    };
    json!({
        "channel": channel,
        "size": limit,
        "ext": ext
    })
}

fn search_path(options: &SearchOptions<'_>) -> &'static str {
    if options.personal_home() {
        PERSONAL_SEARCH_PATH
    } else {
        SEARCH_PATH
    }
}

pub struct CsdnClient {
    http: reqwest::Client,
    /// The app's persisted device identifier (`X-Device-ID`). A fresh
    /// install generates its own; this client generates one UUID per
    /// instance as the stateless equivalent.
    device_id: String,
}

impl Default for CsdnClient {
    fn default() -> Self {
        Self::new()
    }
}

impl CsdnClient {
    pub fn new() -> Self {
        let http = reqwest::Client::builder()
            .user_agent(format!("CSDNApp/{}(Android)", sign::APP_VERSION))
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .expect("failed to build HTTP client");
        Self {
            http,
            device_id: uuid::Uuid::new_v4().to_string(),
        }
    }

    fn apply_headers(
        &self,
        builder: reqwest::RequestBuilder,
        headers: Vec<(String, String)>,
    ) -> reqwest::RequestBuilder {
        let mut builder = builder;
        for (k, v) in headers {
            builder = builder.header(k, v);
        }
        builder
    }

    /// Raw search call against the app gateway; returns the decoded
    /// `ResponseResult` JSON (`{code, message, data, ...}`).
    /// This is the round-1 signature (category `all`); prefer
    /// [`CsdnClient::search_raw_with_options`] for the full surface.
    pub async fn search_raw(
        &self,
        query: &str,
        page: u32,
        limit: u32,
        sort: Option<&str>,
    ) -> anyhow::Result<Value> {
        let mut options = SearchOptions::new(query);
        options.page = page;
        options.limit = limit;
        options.sort = sort;
        self.search_raw_with_options(&options).await
    }

    /// Raw assembled-search call for the full option surface
    /// (category, sort, filters, personal-home search).
    pub async fn search_raw_with_options(
        &self,
        options: &SearchOptions<'_>,
    ) -> anyhow::Result<Value> {
        validate_search_options(options)?;
        let body = build_search_body(options);
        let path = search_path(options);
        let headers = sign::build_headers("POST", path, &[], &self.device_id);
        let url = format!("{}{}", sign::GATEWAY_HOST, path);
        let resp = self
            .apply_headers(self.http.post(&url), headers)
            .json(&body)
            .send()
            .await
            .context("csdn search request failed")?;
        let status = resp.status();
        let text = resp.text().await.context("csdn search body read failed")?;
        if !status.is_success() {
            return Err(anyhow!(
                "csdn search returned HTTP {status}: {}",
                trim(&text)
            ));
        }
        serde_json::from_str(&text).context("csdn search response was not JSON")
    }

    /// Raw article-detail call; returns the decoded `ResponseResult`.
    pub async fn article_raw(&self, article_id: &str) -> anyhow::Result<Value> {
        let params = vec![
            ("articleId".to_string(), article_id.to_string()),
            ("skin".to_string(), String::new()),
        ];
        let headers = sign::build_headers("GET", DETAIL_PATH, &params, &self.device_id);
        let url = format!("{}{}", sign::GATEWAY_HOST, DETAIL_PATH);
        let resp = self
            .apply_headers(self.http.get(&url), headers)
            .query(&[("articleId", article_id), ("skin", "")])
            .send()
            .await
            .context("csdn article request failed")?;
        let status = resp.status();
        let text = resp.text().await.context("csdn article body read failed")?;
        if !status.is_success() {
            return Err(anyhow!(
                "csdn article returned HTTP {status}: {}",
                trim(&text)
            ));
        }
        serde_json::from_str(&text).context("csdn article response was not JSON")
    }
}

fn trim(s: &str) -> String {
    let t = s.trim();
    if t.chars().count() > 300 {
        t.chars().take(300).collect()
    } else {
        t.to_string()
    }
}

/// The app treats response codes 200 and 2000 as success
/// (`ResponseResult.CODE_V2_SUCCESS` / `CODE_SUCCESS`).
fn check_code(v: &Value) -> anyhow::Result<()> {
    match v.get("code") {
        Some(c) => {
            let code = c.as_i64().or_else(|| c.as_str()?.parse().ok());
            match code {
                Some(200) | Some(2000) => Ok(()),
                _ => {
                    let msg = v
                        .get("message")
                        .or_else(|| v.get("msg"))
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    Err(anyhow!("csdn api code {c}: {msg}"))
                }
            }
        }
        None => Ok(()),
    }
}

fn as_string(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    }
}

fn as_i64(v: Option<&Value>) -> Option<i64> {
    match v {
        Some(Value::Number(n)) => n.as_i64(),
        Some(Value::String(s)) => s.trim().parse().ok(),
        _ => None,
    }
}

fn first_string(item: &Value, keys: &[&str]) -> String {
    for key in keys {
        let s = as_string(item.get(key));
        if !s.is_empty() {
            return s;
        }
    }
    String::new()
}

fn first_i64(item: &Value, keys: &[&str]) -> Option<i64> {
    for key in keys {
        if let Some(n) = as_i64(item.get(key)) {
            return Some(n);
        }
    }
    None
}

/// Map one assembled-search entry to the provider item shape.
///
/// Handles the three response families the app deserializes from the
/// same endpoint:
///
/// - `SearchAll` objects (all/vip/special_column, and inserted cards),
/// - `ElasticBlog.Hits` wrappers (`_source` payloads; blog/ask/
///   download/mall/ebook request classes),
/// - `MemberCourse` (course) and `SearchUser` (user) objects.
pub fn parse_search_item(item: &Value) -> Value {
    // ElasticBlog.Hits: the payload lives under `_source`, the
    // hit's own `_id` is the fallback identifier.
    let (source, hit_id) = match item.get("_source") {
        Some(src) if src.is_object() => (src, as_string(item.get("_id"))),
        _ => (item, String::new()),
    };
    let title = first_string(
        source,
        &["title", "course_name", "filename", "nickName", "name"],
    );
    let author = first_string(
        source,
        &[
            "user_name",
            "author",
            "nickname",
            "course_author",
            "lecturer_csdn_username",
            "userName",
        ],
    );
    let digest = first_string(
        source,
        &[
            "digest",
            "description",
            "body",
            "course_description",
            "note",
        ],
    );
    let url = first_string(source, &["url", "dataUrl", "routeUrl", "course_url"]);
    let view_count = first_i64(source, &["viewcount", "view_num", "view", "views"]);
    let item_type = first_string(source, &["so_type", "type"]);
    let mut id = first_string(source, &["id", "es_id", "blogid", "course_id", "userName"]);
    if id.is_empty() {
        id = hit_id;
    }
    let mut out = json!({
        "id": id,
        "title": title,
        "author": author,
        "digest": digest,
        "url": url,
        "published_at": first_string(source, &["created_at", "create_time", "last_post_time", "edit_time"]),
        "view_count": view_count,
        "type": item_type
    });
    // Category-specific distinguishing fields, kept when present.
    if let Some(obj) = out.as_object_mut() {
        let avatar = first_string(source, &["avatar"]);
        if !avatar.is_empty() {
            obj.insert("avatar".to_string(), json!(avatar));
        }
        for (src_key, out_key) in [
            ("fansCount", "fans_count"),
            ("followCount", "follow_count"),
            ("originalCount", "original_count"),
            ("student_count", "student_count"),
            ("commentcount", "comment_count"),
            ("commentCount", "comment_count"),
        ] {
            if let Some(n) = as_i64(source.get(src_key)) {
                obj.insert(out_key.to_string(), json!(n));
            }
        }
        let price = first_string(source, &["course_final_price", "price", "member_price"]);
        if !price.is_empty() {
            obj.insert("price".to_string(), json!(price));
        }
        let file_type = first_string(source, &["filetype", "file_type"]);
        if !file_type.is_empty() {
            obj.insert("file_type".to_string(), json!(file_type));
        }
    }
    out
}

/// Extract `(items, total)` from a search `data` payload, covering
/// the array form and the `ElasticBlog` object form (`hits`, either a
/// list of hit wrappers or an object holding the list).
fn extract_search_items(data: &Value) -> (Vec<Value>, Option<i64>) {
    match data {
        Value::Array(a) => {
            let total = a.first().and_then(|first| as_i64(first.get("total")));
            (a.iter().map(parse_search_item).collect(), total)
        }
        Value::Object(_) => {
            let total = as_i64(data.get("total"));
            let hits = match data.get("hits") {
                Some(Value::Array(h)) => Some(h.clone()),
                Some(Value::Object(h)) => h.get("hits").and_then(Value::as_array).cloned(),
                _ => None,
            };
            if let Some(h) = hits {
                return (h.iter().map(parse_search_item).collect(), total);
            }
            for key in ["items", "results", "list"] {
                if let Some(a) = data.get(key).and_then(Value::as_array) {
                    return (a.iter().map(parse_search_item).collect(), total);
                }
            }
            (Vec::new(), total)
        }
        _ => (Vec::new(), None),
    }
}

/// Map a search `ResponseResult` to the standard search envelope.
pub fn parse_search_response(v: &Value, query: &str, page: u32, limit: u32) -> Value {
    let mut options = SearchOptions::new(query);
    options.page = page;
    options.limit = limit;
    parse_search_response_with_options(v, &options)
}

/// Map a search `ResponseResult` to the standard search envelope,
/// echoing the effective category/sort/filter in the result.
pub fn parse_search_response_with_options(v: &Value, options: &SearchOptions<'_>) -> Value {
    let data = v.get("data").cloned().unwrap_or(Value::Null);
    let (arr, total) = extract_search_items(&data);
    json!({
        "provider": PROVIDER,
        "ok": true,
        "query": options.query,
        "page": options.page.max(1),
        "limit": options.limit.clamp(1, 50),
        "category": options.category.name(),
        "search_type": options.category.request_value(),
        "sort": normalize_sort(options.sort),
        "subfilter": effective_subfilter(options),
        "search_user_name": options.search_user_name.filter(|s| !s.trim().is_empty()),
        "count": arr.len(),
        "total_available": total,
        "items": arr
    })
}

/// Very small HTML-to-text used for the article body convenience
/// field: block elements become newlines, tags are dropped, the
/// common named entities are decoded.
pub fn html_to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    let mut tag = String::new();
    for c in html.chars() {
        if in_tag {
            if c == '>' {
                in_tag = false;
                let t = tag.trim().to_lowercase();
                if t == "br"
                    || t == "br/"
                    || t.starts_with("/p")
                    || t.starts_with("/div")
                    || t.starts_with("/li")
                    || t.starts_with("/h")
                    || t.starts_with("/tr")
                    || t.starts_with("/pre")
                    || t.starts_with("/blockquote")
                {
                    out.push('\n');
                }
                tag.clear();
            } else {
                tag.push(c);
            }
            continue;
        }
        if c == '<' {
            in_tag = true;
            continue;
        }
        out.push(c);
    }
    let decoded = out
        .replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&");
    // Collapse 3+ newlines to 2 and trim trailing spaces per line.
    let mut cleaned = String::with_capacity(decoded.len());
    let mut newlines = 0;
    for c in decoded.chars() {
        if c == '\n' {
            newlines += 1;
            if newlines <= 2 {
                cleaned.push(c);
            }
        } else {
            newlines = 0;
            cleaned.push(c);
        }
    }
    cleaned.trim().to_string()
}

/// Map a detail `ResponseResult` to the standard item envelope.
pub fn parse_article_response(v: &Value, article_id: &str) -> Value {
    let data = v.get("data").cloned().unwrap_or(Value::Null);
    let article = data.get("article").cloned().unwrap_or(Value::Null);
    let author = data.get("author").cloned().unwrap_or(Value::Null);
    let content_html = as_string(article.get("contentHtml"));
    let tags: Vec<Value> = article
        .get("tags")
        .and_then(Value::as_array)
        .map(|a| a.iter().map(|t| json!(as_string(Some(t)))).collect())
        .unwrap_or_default();
    let item_id = {
        let id = as_string(article.get("articleId"));
        if id.is_empty() {
            article_id.to_string()
        } else {
            id
        }
    };
    let item_author = {
        let u = as_string(article.get("username"));
        if u.is_empty() {
            as_string(author.get("username"))
        } else {
            u
        }
    };
    json!({
        "provider": PROVIDER,
        "ok": true,
        "item": {
            "id": item_id,
            "title": as_string(article.get("title")),
            "author": item_author,
            "author_name": as_string(author.get("nickname")),
            "url": as_string(article.get("url")),
            "description": as_string(article.get("description")),
            "content_html": content_html,
            "content_text": html_to_text(&content_html),
            "tags": tags,
            "view_count": as_i64(article.get("viewCount")),
            "comment_count": as_i64(article.get("commentCount")),
            "digg_count": as_i64(article.get("diggCount")),
            "collect_count": as_i64(article.get("collectCount"))
        }
    })
}

fn error_value(kind: &str, message: &str) -> Value {
    json!({
        "provider": PROVIDER,
        "ok": false,
        "error": { "kind": kind, "message": message }
    })
}

/// Tool-shaped search: `csdn_search_articles` (round-1 signature,
/// category `all`).
pub async fn search_articles(
    client: &CsdnClient,
    query: &str,
    page: u32,
    limit: u32,
    sort: Option<&str>,
) -> Value {
    let mut options = SearchOptions::new(query);
    options.page = page;
    options.limit = limit;
    options.sort = sort;
    search_with_options(client, &options).await
}

/// Tool-shaped search over the full option surface.
pub async fn search_with_options(client: &CsdnClient, options: &SearchOptions<'_>) -> Value {
    if let Err(e) = validate_search_options(options) {
        return error_value("invalid_argument", &e.to_string());
    }
    match client.search_raw_with_options(options).await {
        Ok(v) => match check_code(&v) {
            Ok(()) => parse_search_response_with_options(&v, options),
            Err(e) => error_value("api_error", &e.to_string()),
        },
        Err(e) => error_value("request_error", &e.to_string()),
    }
}

/// Extract the numeric article id from a blog URL of the form
/// `https://blog.csdn.net/{user}/article/details/{id}`, or return
/// the input unchanged when it already looks like an id.
pub fn extract_article_id(input: &str) -> String {
    let t = input.trim();
    if let Some(idx) = t.find("/article/details/") {
        let rest = &t[idx + "/article/details/".len()..];
        let id: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        if !id.is_empty() {
            return id;
        }
    }
    t.to_string()
}

/// Tool-shaped detail: `csdn_get_article`.
pub async fn get_article(client: &CsdnClient, article_id_or_url: &str) -> Value {
    let id = extract_article_id(article_id_or_url);
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_digit()) {
        return error_value(
            "invalid_argument",
            "article_id must be a numeric id or a blog.csdn.net article URL",
        );
    }
    match client.article_raw(&id).await {
        Ok(v) => match check_code(&v) {
            Ok(()) => parse_article_response(&v, &id),
            Err(e) => error_value("api_error", &e.to_string()),
        },
        Err(e) => error_value("request_error", &e.to_string()),
    }
}
