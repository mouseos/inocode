# モデルを呼ぶたびの使用量の通知

- **対象と範囲：**
  - ターンの中でモデルへの要求が 1 回終わるたびに、ターンのイベント `usage`（`{event: "usage", tokens: {...}}`）を送る。ターンの本体の要求だけでなく、要約・`explore`・同僚・目標の確認など、ターンの使用量に記録される要求すべてが対象。
  - `tokens` の形：
    - `purpose`（`turn`・`explore`・`colleague` など、要求の記録と同じ）、`provider`、`model`。
    - `input`・`output`・`cached`：その要求のトークン。プロバイダーが知らせなかったときは `null`。`input` は `cached` を含む。
    - `first_output_ms`：送ってから最初の文字までのミリ秒。文字の無い答え（道具の呼び出しだけ）では `null`。`duration_ms`：要求の全体のミリ秒。
    - `output_per_second`：出力トークン ÷ 最初の文字から終わりまでの秒。文字が無ければ要求の全体の秒で割る。時間が 0 か出力が不明なら `null`。
    - `outcome`：`completed`・`failed`・`stopped`。
    - `total`：`{requests, input, output, cached}`。このターンでこの要求までに使った合計。
    - `context_used`：ターン自身のモデル（`purpose` が `turn`）の最新の要求の `input`。会話が文脈をどれだけ使っているかの値として使う。まだ無ければ `null`。
    - `context_window`：ターンのモデルの文脈の大きさ（モデルの一覧から。分からなければ `null`）。
  - 送る時：本体の要求が終わった直後、道具の結果が揃った後（`explore` などの要求の分）、自動の要約の後。前に送ってから増えた記録を、記録の順に送る。
  - ターンの終わりの `completed`・`turn_ended` の `usage` は今までどおり。
  - 範囲外・限界：
    - 会社のメンバーに任せた仕事（`delegate`）の使用量は、合計には入るが記録が無いので、要求ごとの `usage` は出ない。
    - 自動の題名の要求は、ターンの終わりにまとめて記録されるので、`usage` は出ない。
    - `context_used` は、システムの文・道具の説明・状態の文も含めた入力の量で、会話だけの量ではない。
- **関連ソースコード：**
  - `core/src/turn.rs`：`TurnEvent::Usage`、`UsageReport`、`UsageTotal`、`Thread::report_usage`。
  - `core/src/lib.rs`：`EventPayload::usage`（`tokens`）。
  - `server/src/lib.rs`：`turn_event`。
  - テスト：`server/tests/turn_tcp.rs` の `each_model_request_reports_its_usage_the_totals_and_the_context`。
- **根拠の状態：** UNSUPPORTED_APPROVED。画面の表示で、効き目を測る学術研究は当てはまらない。調べたのは、ほかのハーネスの実装だけ。
- **ユーザーの承認：** 2026-11-28。
  - 「次の群（あると便利な機能）に進みますか？」に「次の群へ進む」と答えた。
  - 「使ったトークン量と、文脈がどれだけ埋まったかを、いつ画面に知らせますか？」に「呼ぶたびに総量とかtok/sとか全てを出す dshみたいに」と答えた。
  - 項目の選び方、`context_used` の決め方、イベントの名前と形は、作業者（Claude）が決めた。

## ほかのハーネス（2026-11-28、作業者が頼んだ調べものの報告）

| ハーネス | 使用量の通知 |
|---|---|
| Codex | `thread/tokenUsage/updated`：`total`・`last`（`inputTokens`・`cachedInputTokens`・`outputTokens`・`reasoningOutputTokens` など）と `modelContextWindow` |
| Claude Code | ステータス行の JSON に `context_window.used_percentage`・`context_window_size`・`total_input_tokens`・`total_output_tokens`。`/context`・`/usage` |

## 検証

- テストで確かめたこと：道具を 1 回呼んで答えるターンで、`usage` が 2 回届く。それぞれの `input`・`output`、`total` の合計、2 回目の `context_used`（2 回目の入力）と `context_window`（モデルの一覧の値）、`outcome` が正しい。
- 確かめていないこと：本物のプロバイダーでの値と `output_per_second` の正しさ。
