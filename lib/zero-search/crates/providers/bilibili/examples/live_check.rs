//! Live verification: search -> detail -> related, 3 sends max, serial.
//! Not part of the provider API; a manual end-to-end check.
//!
//! Env: BILI_QUERY (default 洛天依), BILI_BVID (skip search), BILI_SOCKS5
//! (route through a SOCKS5 proxy, e.g. 127.0.0.1:18080).

use bilibili_provider::BilibiliClient;

#[tokio::main]
async fn main() {
    let query = std::env::var("BILI_QUERY").unwrap_or_else(|_| "洛天依".to_string());
    let mut client = BilibiliClient::new();
    if let Ok(proxy) = std::env::var("BILI_SOCKS5") {
        if !proxy.is_empty() {
            println!("egress: socks5 {proxy}");
            client = client.with_socks5(proxy);
        }
    }
    println!("buvid={} (len {})", client.buvid(), client.buvid().len());

    let mut aid: u64 = 0;
    let mut bvid = std::env::var("BILI_BVID").unwrap_or_default();

    if bvid.is_empty() {
        let search = client
            .search_videos(&query, 20, "", 0)
            .await
            .expect("search failed");
        println!(
            "search ok={} count={} next={}",
            search["ok"], search["count"], search["next"]
        );
        if let Some(items) = search["items"].as_array() {
            for it in items.iter().take(5) {
                println!(
                    " - {} | {} | by {} | play={} danmaku={} dur={}",
                    it["bvid"],
                    it["title"],
                    it["author"],
                    it["play"],
                    it["danmaku"],
                    it["duration"]
                );
            }
            if let Some(first) = items.first() {
                aid = first["aid"].as_u64().unwrap_or(0);
                bvid = first["bvid"].as_str().unwrap_or("").to_string();
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }

    if aid > 0 {
        match client.get_video_by_aid(aid).await {
            Ok(view) => {
                let it = &view["item"];
                println!(
                    "view ok={} bvid={} title={} author={} play={} danmaku={} reply={} like={} dur={}",
                    view["ok"], it["bvid"], it["title"], it["owner"]["name"], it["stat"]["view"],
                    it["stat"]["danmaku"], it["stat"]["reply"], it["stat"]["like"], it["duration"]
                );
            }
            Err(e) => println!("view ERR: {e:#}"),
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        match client.get_related_by_aid(aid, 10, "").await {
            Ok(related) => {
                println!("related ok={} count={}", related["ok"], related["count"]);
                if let Some(items) = related["items"].as_array() {
                    for it in items.iter().take(3) {
                        println!(
                            " - rel {} | {} | play={}",
                            it["bvid"], it["title"], it["play"]
                        );
                    }
                }
            }
            Err(e) => println!("related ERR: {e:#}"),
        }
    } else if !bvid.is_empty() {
        match client.get_video(&bvid).await {
            Ok(view) => println!(
                "view ok={} title={} author={} play={}",
                view["ok"],
                view["item"]["title"],
                view["item"]["owner"]["name"],
                view["item"]["stat"]["view"]
            ),
            Err(e) => println!("view ERR: {e:#}"),
        }
    }
}
