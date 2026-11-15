# OpenAI 互換 API 接続とストリーミング

## 対象と範囲

- 対象: 第2段階の Server / Core に、OpenAI 互換 Chat Completions API への接続と、SSE による逐次受信、既存 TCP プロトコルでの逐次配送、成功・失敗・中断の通知を追加する。
- 実装した接続方式: OpenAI 互換 API のみ。接続方式は `LlmConfig`（Base URL・モデル・API キー）で選ぶ構成にしてあり、Anthropic Messages 互換、Codex OAuth、Claude OAuth は実装していない。
- 範囲外: 会話履歴、セッション管理、ツール実行、RAG、MCP、GUI、製品用 CLI、承認フロー、モデル管理。実装していない。
- 通信仕様（16 バイトヘッダー、Type 1/2/3、Stream ID、ビッグエンディアン、JSON と生バイナリの混在）は第1段階のまま変更していない。

## 関連ソースコード

- `core/src/llm.rs` — `LlmConfig` / `Llm` / `ChatStream`。OpenAI 互換 `/chat/completions` への `stream: true` 要求と、デルタ単位の受信。
- `core/src/sse.rs` — `SseParser`。バイト境界をまたぐ SSE の逐次解析、`[DONE]` と `finish_reason` の解釈、途中終了の検出。
- `core/src/lib.rs` — `Request` / `Response` / `EventPayload` / `Core`。`chat` と `cancel` の受け口。
- `server/src/lib.rs` — 接続ごとの読み取り・書き込みタスク、要求 ID ごとの生成タスク、中断レジストリ、有界の送信キュー。
- `server/tests/chat_tcp.rs` — ローカルの模擬 HTTP API を用いた実 TCP 統合テスト 13 件。
- `server/tests/live_api.rs` — 実 Logfare API を叩くテスト 1 件（通常は `ignored`、CI の `live-api` ジョブが実行）。
- `server/tests/support/mod.rs` — 最小の HTTP/1.1 模擬サーバー（応答本文を任意のチャンクに分割して送る）と TCP クライアント補助。

## 要求・応答

- 既存の `ping` / `echo` は変更なしで動作する。
- `chat`: `{"id": <u64>, "method": "chat", "params": {"prompt": <string>}}`。応答は単一フレームではなく、同じ Stream ID 上のイベント列。
  - `{"id": <要求 ID>, "event": "started"}`
  - `{"id": <要求 ID>, "event": "delta", "text": <string>}`
  - `{"id": <要求 ID>, "event": "completed"}`
  - `{"id": <要求 ID>, "event": "failed", "error": {"code": <string>, "message": <string>}}`
  - `{"id": <要求 ID>, "event": "cancelled"}`
- `cancel`: `{"id": <u64>, "method": "cancel", "params": {"request_id": <u64>}}` → `{"id": <要求 ID>, "result": {"cancelled": <bool>}}`。対象が実行中なら `true`、既に終了または未知なら `false`。
- 追加したエラーコード: `provider_unconfigured`、`provider_config`、`provider_http`、`provider_protocol`、`provider_stream`、`duplicate_id`。
- 終端イベント（`completed` / `failed` / `cancelled`）は 1 要求につき 1 回だけ送る。`started` はプロバイダーの応答ヘッダーが成功した後に送るため、HTTP エラー時は `started` を送らず `failed` のみになる。
- プロバイダーが `[DONE]` も `finish_reason` も送らずに本文を終えた場合は `completed` ではなく `provider_stream` の `failed` を返す。途中で切れた応答を完了として扱わないため。

## 設定

- `LOGFARE_API_KEY`（必須）: Bearer トークン。未設定または空のとき、Server は起動するが `chat` は `provider_unconfigured` を返す。
- `INOCODE_LLM_BASE_URL`（任意、既定 `https://logfare.ai/v1`）: `POST {base_url}/chat/completions` を呼ぶ。末尾スラッシュは除去する。
- `INOCODE_LLM_MODEL`（任意、既定 `logfare/auto`）。
- `INOCODE_PORT`（任意、既定 8787）。待ち受けは常にループバック `127.0.0.1`。

## 設計上の判断

- 接続方式は `LlmConfig` の値で選ぶため、Logfare 固有のコードはない。既定値が Logfare なだけであり、Base URL とモデルを差し替えれば任意の OpenAI 互換 API で動く。
- 要求 ID は接続ごとに管理する。同一接続で実行中の ID を再利用した場合は `duplicate_id` で拒否し、応答の取り違えを防ぐ。
- 接続ごとに書き込みタスクを 1 つ置き、有界キュー（64 フレーム）を介して送る。生成タスクが直接ソケットへ書かないため、複数要求のフレームが混ざらず、読み取りの遅いクライアントは自分のキューが詰まるだけで済む（送信キューは無制限に伸びない）。
- 中断は `oneshot` の通知で生成タスクに伝える。`select!` で受信と中断を同時に待つため、プロバイダーの応答が来なくても中断できる。中断すると HTTP 応答は破棄され、接続が閉じる（模擬サーバー側の書き込み失敗で確認）。
- クライアントが切断すると、その接続が所有する生成タスクの中断ハンドルが落ち、生成が止まる。

## 根拠の状態

- **UNSUPPORTED_APPROVED** — 接続方式、イベント名（`started` / `delta` / `completed` / `failed` / `cancelled`）、`chat` / `cancel` の形はユーザー指示で確定しており、有効性を学術研究で主張しない。参照元は本タスクの実装指示（第3段階）。
- 追加した依存: `reqwest` 0.12（`json`、`rustls-tls`、`stream` のみ、既定機能なし）。TLS は rustls を用いるため、Linux / macOS で OpenSSL の開発パッケージを要求しない。

## 検証済みの主張

- `cargo test --workspace --release`（Windows、ローカル実機）: 成功 67 件、失敗 0 件（プロトコル単体 14、プロトコル TCP 統合 14、Core 単体 15、Server チャット TCP 統合 13、Server TCP 統合 11）。計測用 3 件と実 API 1 件は `ignored`。3 回連続実行して同じ結果。
- `cargo fmt --all -- --check` と `cargo clippy --workspace --all-targets --release -- -D warnings` が警告なしで成功。
- 模擬 HTTP API を用いた統合テストが確認する項目:
  1. 要求の送信内容（`POST /v1/chat/completions`、`Authorization: Bearer <key>`、`model`、`stream: true`、`messages[0].role` / `content`）と、応答テキストの取得。
  2. SSE の逐次配送: 模擬が 120 ms 間隔で 4 デルタを送り、最初のデルタが完了時刻より 300 ms 以上早く届くことを確認（一括返却ではない）。
  3. TCP クライアントが受け取るイベント順序（`started` → 複数の `delta` → `completed`）と、連結したテキストの一致。
  4. SSE を 5 バイトずつ（UTF-8 の途中を含む）に分割して送っても復元できること。
  5. 生成途中の `cancel`: `{"cancelled": true}` が返り、`cancelled` イベントで終わり、`completed` が来ないこと。模擬サーバー側の書き込みが失敗することで、プロバイダー接続が実際に閉じたことを確認。
  6. 3 クライアント × 2 要求の同時生成で、各イベントの `id` がストリームと一致し、テキストが混ざらないこと。
  7. HTTP エラー（429）が `started` を伴わず `failed`（`provider_http`、状態コードを含むメッセージ）になること。
  8. 送信途中でのクライアント切断後も Server が他の接続に応答し、切断された生成が止まること。
  9. 既存 TCP プロトコル（`tests/tcp_protocol.rs`）が変更なしで成功。
  10. `ping` / `echo` がプロバイダー設定済みでも未設定でも従来どおり動作すること。
  11. 生成中でも同一接続の別要求と別クライアントの要求に応答すること（`ping` の応答が生成完了前、残りデルタ 10 件以上を残して届く）。
  12. 読み取りを 300 ms 止めたクライアントでも、300 デルタすべてが欠落せず到着すること（有界キューによる逆圧）。
- 独立実行ファイル `inocode-server.exe` を実際に起動し、模擬 SSE プロバイダー（`INOCODE_LLM_BASE_URL` で指定、キーはダミー）と生バイトの TCP クライアントで確認: `ping` / `echo`、逐次デルタ（約 80 ms 間隔で 5 件）、`completed`、生成中の `cancel` と `cancelled`、中断後の別要求の成功。実 API のキーは使っていない。

## 未検証の主張

- 実 Logfare API での生成とストリーミングは、ローカルに `LOGFARE_API_KEY` がないため手元では未実行。GitHub Actions の `live-api` ジョブ（3 OS、リポジトリシークレット使用）で検証する。結果は本ファイル末尾に追記する。
- プロバイダーのレート制限、長い応答（数万デルタ）、ネットワーク断、TLS 検証エラーの網羅的検証は未実施。
- 中断時にプロバイダー側の課金・生成がいつ止まるかは、プロバイダーの実装依存であり検証していない（こちら側は HTTP 応答を破棄して接続を閉じる）。
- 同時接続数の上限、長時間稼働、プロセス再起動時の状態は未検証（Server は状態を持たない）。

## 実装とテストの状況

- 実装: 完了（OpenAI 互換 API 接続、SSE 逐次受信、TCP 逐次配送、成功・失敗・中断の通知）。
- テスト: 上記 67 件がローカル（Windows）で成功。
- GitHub Actions（`.github/workflows/protocol-test.yml`、workflow_dispatch 手動実行）: 通常ジョブは `cargo fmt` / `cargo clippy -D warnings` / build / test を 3 OS で実行。`live-api` ジョブはリポジトリシークレット `LOGFARE_API_KEY` を用いて実 API の生成とストリーミングを 3 OS で実行する。キーは環境変数としてのみ渡し、ソース・コミット・ログ・成果物には書かない（テストが出力するのはデルタ数・文字数・所要時間のみ）。

## 追記: 3 OS の CI 結果

- 実行: https://github.com/mouseos/inocode/actions/runs/43714283121 （workflow_dispatch、コミット `083286c`）。6 ジョブすべて成功。
- `protocol test`（ubuntu-latest / windows-latest / macos-latest）: 各 OS で `cargo fmt --all -- --check`、`cargo clippy --workspace --all-targets --release -- -D warnings`、`cargo build --workspace --release`、`cargo test --workspace --release` が成功。各 OS のテスト内訳はプロトコル単体 14、プロトコル TCP 統合 14、Core 単体 15、Server チャット TCP 統合 13、Server TCP 統合 11（計 67 件成功、失敗 0 件）。実 API テストと計測用テストは `ignored` のまま。
- `live provider test`（ubuntu-latest / windows-latest / macos-latest）: リポジトリシークレット `LOGFARE_API_KEY` を用いて `server/tests/live_api.rs` を 3 OS で実行し、すべて成功。実 Logfare API（`logfare/auto`）からテキストを受信し、複数デルタが完了前に届いたことを確認した。

  | OS | デルタ数 | 文字数 | 最初のデルタ | 完了まで |
  | --- | --- | --- | --- | --- |
  | ubuntu-latest | 16 | 145 | 5868 ms | 6127 ms |
  | windows-latest | 8 | 86 | 9721 ms | 9856 ms |
  | macos-latest | 10 | 191 | 3243 ms | 3422 ms |

  最初のデルタが完了より数百 ms 以上早く届いているため、応答を一括で受け取っていない。生成テキスト自体はログに残していない（テストが出力するのはデルタ数・文字数・時刻のみ）。
- シークレットの扱い: ワークフローはキーを環境変数としてのみ渡す。GitHub Actions のログでは `LOGFARE_API_KEY: ***` とマスクされ、値は表示されないことを確認した。キーはソース、コミット、テストコード、成果物に含まれない。
