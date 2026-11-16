# モデル取得（discovery）機能

- **対象と範囲:** すべての接続方式（`openai` / `anthropic`（API キー）、`claude-oauth`、`gpt-oauth`、`muse-code`）で、接続先が提供するモデル一覧を取得する機能。`inocode-server models` CLI サブコマンドと、TCP プロトコルの `models` メソッド（即時応答）を追加する。既存メソッド（`ping`/`echo`/`chat`/`cancel`）とフレーム形式は変更しない。
- **関連コード:** `core/src/llm.rs`（`Llm::models`、`ModelInfo`、方式別 URL/認証）、`core/src/lib.rs`（`Core::models`）、`server/src/lib.rs`（`models` メソッドの非同期ディスパッチ）、`server/src/main.rs`（`models` サブコマンド）。
- **根拠の状態:** **UNSUPPORTED_APPROVED** — 学術研究による有効性検証はこの種の API 準拠機能に適用されない。承認の証跡: 2026-11-16 のユーザー指示「モデル取得機能 muse, oauth, api すべてで実装しなさい。モデル取得なしで会話とか無理でしょ」。
- **学術的な主張・不足点:** 適用可能な学術研究を確認できていない（効果検証を要する主張もない）。
- **学術資料:** なし（不明）。

## その他の根拠（公式仕様と参照実装。研究とは区別する）

- **OpenAI（公式 API リファレンス、developers.openai.com/api/reference/models）:** `GET {base}/models`、`Authorization: Bearer`、応答は `{object: "list", data: [{id, object: "model", owned_by, created, ...}]}`。
- **Anthropic（公式 API リファレンス、platform.claude.com/docs/en/api/models/list）:** `GET {base}/v1/models`、`x-api-key` ＋ `anthropic-version`。`limit` は既定 20・範囲 1–1000、応答は `{data: [{type: "model", id, display_name, ...}], has_more, ...}`。
- **参照実装 lidge-jun/opencodex ＋フォーク**（`src/oauth/index.ts` の `buildModelsRequest`、`src/providers/model-discovery.ts` の `providerModelsUrl`）:
  - 汎用（OpenAI 形式・Responses 形式）: `providerModelsUrl(baseUrl)` = 末尾スラッシュ除去 → 末尾 `/models` 除去 → `{base}/models`、`Authorization: Bearer`。応答は `{data: [{id, owned_by?}]}`。
  - Anthropic 特例: base の末尾 `/v1` を除去して `{base}/v1/models?limit=1000`、`anthropic-version: 2023-06-01`。API キーは `x-api-key`、OAuth は `Authorization: Bearer` ＋ `anthropic-beta`（OAuth ヘッダ）。
  - discovery は推論と同一の静的ヘッダを運ぶ（`User-Agent: muse-build/1.3.0`、`x-api-version: 1.0.0` など。レート制限側が匿名クライアントと区別しないため）。
  - **Muse の認証素材**: meta-muse エントリの判定ログに「OAuth アクセストークンは `/v1/models` で 401、姉妹の `api_key` は 200」とある。したがって discovery の bearer は推論と同じ `LLM|` キー（アカウントトークンではない）。
  - 応答読み取りは上限付きバイト数で JSON を解析（`readBoundedDiscoveryJson`）。
- **gpt-oauth（ChatGPT/Codex backend）の `/models` は未検証の点に注意**: フォークの openai 経路は自サーバ経由の forward で discovery トークンを送らず、ChatGPT backend の `/models` 実挙動は記録にない。InoCode は OpenAI 標準形状（`{base}/models` ＋ Bearer ＋ `chatgpt-account-id`）を実装し、実応答は明日のログイン試験で確認する。

## ユーザーの承認

- 参照元: 2026-11-16「モデル取得機能 muse, oauth, api すべてで実装しなさい」。全方式対象は指示文に明示。

## 実装・検証の状況

- 実装済み（2026-11-16）:
  - `core/src/llm.rs` — `Llm::models()`（`ModelInfo {id, display_name, owned_by}`）、方式別 URL/認証（上記の参照実装どおり）、16 MiB 上限付き本文読み取り、OpenAI 形状・Anthropic 形状の両応答解析（`id` の無い項目はスキップ）。
  - `core/src/lib.rs` — `Core::models()`。`server/src/lib.rs` — `models` メソッドの非同期ディスパッチ（即時応答、既存メソッドとフレーム形式は変更なし）。`server/src/main.rs` — `models` サブコマンド（モデル ID のみ表示）。
  - `server/tests/oauth_flows.rs` — 模擬 discovery 5 テスト（muse / gpt / claude-oauth / openai キー / anthropic キー）でヘッダ・パス・応答解析を検証。
- 検証済み（ローカル、2026-11-16）: `cargo fmt --all --check`、`cargo clippy --workspace --release --all-targets -- -D warnings`、`cargo test --workspace --release`（119 passed / 0 failed）。
- 実通信確認: muse の `/v1/models` を実キーで 1 回実行（下記）。claude/gpt の実 discovery は翌日のログイン試験時に確認（未実施）。
