# ストリーミング経路の性能改善（監査 B 項目）

- **対象と範囲：** 監査で分類 B とした 4 項目。外部から見える動作（SSE の解釈、TCP フレーム、イベントの内容と順序）は変えない。
  - B1：SSE パーサの行の切り出し（`core/src/sse.rs`）
  - B2：server が chat イベントを writer に渡す形（`server/src/lib.rs`）
  - B3：SSE の data を `serde_json::Value` として解釈すること（`core/src/llm.rs` の `openai_delta` / `anthropic_event` / `responses_delta`）
  - B4：OAuth 系の要求ごとの `auth.json` 同期読み込み（`core/src/llm.rs` の `Llm::credential`）
- **関連コード：**
  - `core/src/sse.rs` — `SseParser`（読み取り位置 `start` と探索済み位置 `scanned`、消費済みが未処理以上になったときだけ詰める）、`Event`（data を逐次連結）
  - `server/src/lib.rs` — `Outgoing` / `Payload`（`EventPayload` を型のまま writer へ渡す）
  - 計測：`core/src/llm.rs` の `tests::measure`（`measure_stream_parsing`、`measure_sse_worst_cases`、`measure_credential_store_read`）、`server/tests/measure_server.rs`（`chat_delta_forwarding`）
- **根拠の状態：** B1・B2 は UNSUPPORTED_APPROVED（外部動作を変えない内部の性能改善。学術的根拠は調べていない）。B3・B4 は実装しない（下記の実測による判断）。
- **学術的な主張・不足点：** 適用可能な学術研究を確認できていない（調査未実施）。B1 の手法は、未処理部分だけを再探索し、詰め直しを償却するという一般的なバッファ管理で、特定の論文に基づくものではない。
- **学術資料：** なし。
- **その他の根拠（プロジェクト内の実測。研究とは区別する）：**
  - 環境：Windows 11、AMD Ryzen AI 9 HX 370、rustc 1.93.1、`--release`、`--test-threads=1`。各値は 7 回の中央値。
  - 再現コマンド：`cargo test --release -p inocode-core measure_ -- --ignored --nocapture --test-threads=1`、`cargo test --release -p inocode-server --test measure_server -- --ignored --nocapture --test-threads=1`

| 計測 | 変更前 | 変更後 | 対象 |
|---|---|---|---|
| 4 MiB の 1 行を 4 KiB ずつ受信 | 477〜494 ms（8.1〜8.4 MiB/s） | 3.0〜3.3 ms（1.2〜1.3 GiB/s） | B1 |
| 短いイベント 2 万件を 1 回で受信 | 44〜47 ms | 0.9〜1.0 ms | B1 |
| OpenAI 形式 5 万 delta（SSE のみ、16 KiB 受信） | 15.6〜16.5 ms | 5.6〜6.3 ms | B1 |
| Responses 形式 5 万 delta（SSE のみ） | 20.5〜20.8 ms | 6.6〜6.8 ms | B1 |
| Anthropic 形式 5 万 delta（SSE のみ） | 18.9〜19.3 ms | 4.6〜4.9 ms | B1 |
| server 経由 2 万 delta（模擬プロバイダ → server → TCP クライアント） | 112〜114 ms | B1 のみ 107〜108 ms、B1＋B2 100.1〜100.4 ms | B1・B2 |

  - B3：SSE＋解釈の時間から SSE のみの時間を引くと、`Value` による解釈は 1 イベントあたり約 540〜820 ns（変更後の値）。実際の LLM 出力を毎秒数百イベントとしても CPU は 1 秒あたり 1 ms 未満。型付き構造体に変えると、現在の寛容な解釈（`choices` が配列でない、`error` が任意の型、エスケープを含む文字列など）を一つずつ再現する必要があり、外部動作を変える危険に対して得られる差が小さいため実装しない。
  - B4：5,396 バイトの `auth.json` の `Store::load` は 1 回約 26 µs。要求ごと（delta ごとではない）に 1 回で、HTTP 要求の往復（数百 ms）に対して無視できるため実装しない。
- **ユーザーの承認：** 2026-11-16、段階 2（B の性能改善、実測で差が出ない項目は戻す）の計画に対し「コミット後B」と指示。
- **実装・検証の状況：** `cargo test --workspace --release` 全件成功。B1 で `mixed_terminators_parse_the_same_when_fed_byte_by_byte`（CR・CRLF・LF の混在を 1 バイトずつ与えても一括と同じ結果になる）を追加。既存の SSE テスト（1 バイト分割、上限、BOM、UTF-8 不正）はすべて変更なしで成功。計測値はこのマシンでの値で、他の環境や実プロバイダでは未計測。
