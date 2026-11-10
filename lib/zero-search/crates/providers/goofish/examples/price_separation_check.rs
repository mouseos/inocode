//! [S1-19] Live separation check for the Goofish 419 question.
//!
//! Rounds 21/22/32 showed sorted search draws HTTP 419
//! (FAIL_SYS_USER_VALIDATE), and round 33 closed the static audit:
//! no implementation-side discrepancy explains it. Price range alone
//! was never sent without sort, so its effect was never separated.
//!
//! This example sends, in ONE client session and in order:
//!
//! 1. a plain search (no sort, no price) — the baseline;
//! 2. a price-only search (startPrice/endPrice, NO sort).
//!
//! It never sends a sorted search. Run manually with the signing
//! runtime configured (GOOFISH_SG_SERVE), never in CI:
//!
//! `cargo run -p shopping-mcp-goofish --example price_separation_check`

use shopping_mcp_goofish::GoofishClient;

#[tokio::main]
async fn main() {
    let client = GoofishClient::new();
    let baseline = client
        .search_products("相机", 1, 10, None, None, None)
        .await;
    println!(
        "BASELINE plain search: ok={} ret={}",
        baseline["ok"], baseline["ret"]
    );
    if baseline["ok"] != true {
        println!("Baseline did not succeed; NOT sending the price-only search.");
        return;
    }
    // Search results live under `data.items` in this envelope.
    let base_count = baseline["data"]["items"]
        .as_array()
        .map(|a| a.len())
        .unwrap_or(0);
    println!("BASELINE items: {base_count}");
    let priced = client
        .search_products("相机", 1, 10, None, Some("1000"), Some("5000"))
        .await;
    println!(
        "PRICE-ONLY search: ok={} ret={} error={}",
        priced["ok"], priced["ret"], priced["error"]
    );
    let count = priced["data"]["items"]
        .as_array()
        .map(|a| a.len())
        .unwrap_or(0);
    println!("PRICE-ONLY items: {count}");
    // Dump the head of the full result so the response shape (and
    // whether the empty list is the server's answer or a parse miss)
    // can be inspected from the log.
    let dump = priced.to_string();
    println!("PRICE-ONLY result head: {}", &dump[..dump.len().min(1500)]);
}
