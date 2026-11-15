# inocode

TCP 上で動く JSON / 生バイナリ混在のフレームプロトコルライブラリと、その上に載る要求処理（Core）と独立 Server。OpenAI 互換 API への接続と、応答テキストの逐次配送に対応する。

## 構成

- ルートクレート `inocode` — 16 バイト固定ヘッダーのフレーム入出力。TCP のみを扱い、上位機能を持たない。
- `core` (`inocode-core`) — 要求・応答・イベントの型、SSE 解析、OpenAI 互換 API クライアント。TCP 依存なし。
- `server` (`inocode-server`) — 接続受付と要求の振り分け、生成タスク、独立実行ファイル。

## プロトコル（第1段階から変更なし）

```text
0       4   5     6        8                        16
+-------+---+-----+--------+-------------------------+
| len   | T | F   | rsvd   | stream id               |
+-------+---+-----+--------+-------------------------+
```

- `len` は u32 ビッグエンディアンで、自身の後ろ 12 バイトとペイロードの合計。
- `T` は 1 = JSON、2 = 生バイナリ、3 = バイナリ終端。`F`（Flags）と rsvd は 0 のみ許可し、受信時に 0 以外は拒否する。
- `stream id` は u64。JSON と生バイナリは同一接続で混在できる。Base64 は使わない。
- 1 フレームのペイロード上限は既定 16 MiB。大きな転送は複数フレームに分割する。

## 要求と応答

要求は JSON フレームのペイロード。

```json
{"id": 1, "method": "echo", "params": {"text": "hello"}}
```

| method | params | 応答 |
| --- | --- | --- |
| `ping` | なし | `{"id": 1, "result": {"pong": true}}` |
| `echo` | 任意 | `{"id": 1, "result": <params>}` |
| `chat` | `{"prompt": "..."}` | 同一 Stream ID 上のイベント列（下記） |
| `cancel` | `{"request_id": <u64>}` | `{"id": ..., "result": {"cancelled": <bool>}}` |

エラーは `{"id": <id or null>, "error": {"code": "...", "message": "..."}}`。コードは `invalid_json`、`invalid_request`、`invalid_params`、`unknown_method`、`duplicate_id`、`provider_unconfigured`、`provider_config`、`provider_http`、`provider_protocol`、`provider_stream`。

### 生成イベント

`chat` は 1 つの応答ではなく、要求 ID を保ったイベントを順に返す。

```json
{"id": 1, "event": "started"}
{"id": 1, "event": "delta", "text": "こん"}
{"id": 1, "event": "delta", "text": "にちは"}
{"id": 1, "event": "completed"}
```

失敗時は `{"id": 1, "event": "failed", "error": {"code": "...", "message": "..."}}`、中断時は `{"id": 1, "event": "cancelled"}`。終端イベントは 1 要求につき 1 回。`cancel` が `{"cancelled": true}` を返した場合、その要求の終端は必ず `cancelled` になり、生成の全段階（HTTP 要求開始、応答ヘッダー待ち、SSE 受信、送信キュー待ち）で中断できる。

## 設定

| 環境変数 | 既定 | 用途 |
| --- | --- | --- |
| `LOGFARE_API_KEY` | なし（必須） | プロバイダーの Bearer トークン |
| `INOCODE_LLM_BASE_URL` | `https://logfare.ai/v1` | OpenAI 互換 API の Base URL |
| `INOCODE_LLM_MODEL` | `logfare/auto` | モデル名 |
| `INOCODE_PORT` | `8787` | 待ち受けポート（待ち受けは常に `127.0.0.1`） |

```bash
export LOGFARE_API_KEY=...            # 環境変数からのみ読む。リポジトリに書かない
cargo run --release -p inocode-server
```

## ビルドとテスト

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --release -- -D warnings
cargo build --workspace --release
cargo test --workspace --release
```

- 統合テストは実際の TCP 接続とループバックの模擬 HTTP API を使う。実 API は叩かない。
- 実 Logfare API の確認は `server/tests/live_api.rs`（既定で `ignored`）。

```bash
LOGFARE_API_KEY=... cargo test --release -p inocode-server --test live_api -- --ignored --nocapture
```

- `.github/workflows/protocol-test.yml` は手動実行のみ。Ubuntu / Windows / macOS で fmt・clippy・build・test を回し、`LOGFARE_API_KEY` がある場合は実 API の生成とストリーミングも 3 OS で確認する。キー未設定のときは実 API ジョブが失敗する（未実施を成功扱いにしない）。キーはログに出力しない。

## 記録

- `docs/evidence/tcp-frame-protocol.md` — フレームプロトコル。
- `docs/evidence/server-core-requests.md` — Server / Core と要求処理。
- `docs/evidence/llm-streaming.md` — OpenAI 互換 API 接続とストリーミング。

## 現状の範囲

実装済み: フレームプロトコル、Server / Core、OpenAI 互換 API 接続と逐次配送、中断、複数クライアント。未実装: Anthropic Messages 互換、Codex OAuth、Claude OAuth、会話履歴、ツール実行、RAG、MCP、GUI、製品用 CLI。
