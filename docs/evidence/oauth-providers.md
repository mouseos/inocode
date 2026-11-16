# OAuth ログイン対応（Claude OAuth / GPT OAuth / Muse Code）

## 対象と範囲

- 対象: InoCode に 3 つのサブスクリプション/OAuth 認証プロバイダを追加する。
  1. **claude-oauth** — Anthropic Claude Pro/Max の OAuth（Claude Code クライアントと同一の OAuth クライアント）で `api.anthropic.com/v1/messages`（Anthropic Messages 形式）を使う。
  2. **gpt-oauth** — ChatGPT/Codex の OAuth で `https://chatgpt.com/backend-api/codex/responses`（OpenAI Responses 形式）を使う。
  3. **muse-code** — Meta Muse Code サブスクリプション（デバイスログイン → サブスクリプションキーのミント）で `https://api.meta.ai/v1/responses`（OpenAI Responses 形式）を使う。
- 範囲外: エージェントループ、ツール実行、会話履歴、GUI、複数アカウントプール。InoCode の chat プロトコルはツールを一切宣言しないため、opencodex フォーク（ifokeev/opencodex `fix/meta-muse-subscription-tool-alias`）のツール名エイリアスは本実装では未呼用（下記「適用判断」）。
- ログイン（ブラウザ承認・デバイス承認）の実試験はユーザーが翌日実施する。本実装は模擬サーバによる自動試験までを行う。

## 関連ソースコード

- `core/src/oauth.rs` — 認証情報ストア、PKCE、コールバック待ち受け、各ログイン/更新フロー、エンドポイントの注入点。
- `core/src/llm.rs` — `AuthKind` による要求時認証（Bearer 更新を含む）、Responses 形式クライアント。
- `server/src/main.rs` — `login` サブコマンド。
- `server/tests/oauth_flows.rs` — 模擬エンドポイントによるログイン/更新/チャット統合テスト。

## 根拠の状態

- **UNSUPPORTED_APPROVED** — 学術研究による直接の裏付けはなく、互換性実装であるため不要。承認の証跡: 2026-11-15 のユーザー指示「claude oauth, gpt oauth, muse code … opencodex に全て答えがある。フォーク (https://github.com/ifokeev/opencodex/tree/fix/meta-muse-subscription-tool-alias) どおりに実装しないと動かない」。ログイン実試験は翌日にユーザーが実施すると明示されている。

## 根拠（公式仕様と参照実装。研究とは区別する）

一次参照: **lidge-jun/opencodex**（本番互換プロキシ、クローンは gitignore 済み `refs/` 配下。参照コミット: `bb9da3a` Messages 変換、`decb08b` baseUrl 正規化とストリーム異常堅牢化）と、そのフォーク **ifokeev/opencodex** ブランチ `fix/meta-muse-subscription-tool-alias`（参照コミット `1cdae2f` ツール別名、`304f62c`/`29ebee6` クォータ拒否、ドキュメント 2 件）。

- **Claude OAuth**（opencodex `src/oauth/anthropic.ts`）:
  - `client_id = 9d1c250a-e61b-44d9-88ed-5944d1962f5e`（ソース内 base64 定数をデコードした公開値）。
  - 承認 URL `https://claude.ai/oauth/authorize`、パラメータ `code=true, response_type=code, redirect_uri, scope=org:create_api_key user:profile user:inference, code_challenge(S256), code_challenge_method=S256, state`。
  - トークン URL `https://api.anthropic.com/v1/oauth/token`（JSON POST）。`grant_type=authorization_code` では `code`/`state` を `code#state` 形式から分離（`#` 以降を state とする）。`grant_type=refresh_token` で更新。`expires_in` から 5 分の skew を引いた時刻を失効とする。
  - ブラウザモードはローカル `127.0.0.1:54545/callback` で待ち受け。コード表示モードは `redirect_uri=https://platform.claude.com/oauth/code/callback`（ブラウザが `code#state` を表示、利用者が貼る）。
  - 要求時: `Authorization: Bearer <access>`、`anthropic-beta: claude-code-20250219,oauth-2025-04-20`、先頭 system ブロックに Claude Code アイデンティティ（`You are a Claude agent, built on Anthropic's Claude Agent SDK.`）。OAuth トークンは任意ツール名を拒否するため `custom_` 接頭辞を付けるが、InoCode はツールを送らないため未適用。
- **GPT OAuth（ChatGPT/Codex）**（opencodex `src/oauth/chatgpt.ts`、`chatgpt-shared.ts`）:
  - `client_id = app_EMoamEEZ73f0CkXaXp7hrann`、承認 `https://auth.openai.com/oauth/authorize`、トークン `https://auth.openai.com/oauth/token`（**form-encoded**）。
  - パラメータ `response_type=code, redirect_uri, scope=openid profile email offline_access api.connectors.read api.connectors.invoke, code_challenge S256, code_challenge_method S256, state, codex_cli_simplified_flow=true, originator=codex_cli_rs, id_token_add_organizations=true`。コールバックは `127.0.0.1:1455/auth/callback`。
  - `id_token`/`access_token` の JWT ペイロード（base64url デコード）から `https://api.openai.com/auth` 名前空間の `chatgpt_account_id` を取得し、要求ヘッダ `chatgpt-account-id` に使う。
- **Codex バックエンドの要求形状**（opencodex `src/codex/warmup.ts` は実バックエンドで受理される最小要求を記録）:
  - URL `https://chatgpt.com/backend-api/codex/responses`、ヘッダ `Authorization: Bearer <access>`、`ChatGPT-Account-Id: <id>`。
  - 本体: `{"model", "instructions"(省略可), "input":[{"type":"message","role":"user","content":[{"type":"input_text","text":...}]}], "stream":true, "store":false}`。
  - SSE: `response.output_text.delta`（`delta` 文字列＝本文）が本文。`response.completed` で正常終了、`response.failed`（`response.error.message`）と `error` イベントは失敗、`response.incomplete` は終了（不足理由付き）。EOF が终端フレームなしに来るのは打ち切りであり失敗として扱う（opencodex `decb08b` と同一の規則）。
- **Muse Code**（opencodex `src/oauth/meta-muse*.ts`、devlog `_fin/260912_muse_device_oauth`、フォークの修正群）:
  - デバイス許可: `POST https://auth.meta.com/oidc/device/authorization/`、`client_id=1031625952748946`（Meta 自身の Muse Code クライアント。公開値）、ヘッダ `x-api-version: 1.0.0`、form-encoded。応答 `device_code, user_code, verification_uri, verification_uri_complete, interval, expires_in`。
  - ポーリング: `POST https://auth.meta.com/oidc/device/token/`、`{client_id, device_code, grant_type=urn:ietf:params:oauth:grant-type:device_code}`。`authorization_pending` で待機、`slow_down`/429 は `Retry-After` 尊重（最小 1s、+5s、上限 60s）、`access_denied`/`expired_token` は終端。
  - キーミント: `POST https://api.meta.ai/muse-code/key`、`Authorization: Bearer <アカウントアクセストークン>`、本体 `{}`（ログイン時のみ `{"onboard":true}`）、`x-api-version: 1.0.0`。応答の `api_key`（`LLM|` 接頭辞）が Model API のbearer。**アカウントアクセストークン自体は Model API の bearer ではない**（実測で 401、`api_key` が 200 — opencodex devlog 記録）。
  - 画面: `verification_uri` と `user_code` を表示してユーザーに承認させる。
  - 要求: `https://api.meta.ai/v1/responses`（Registry: `baseUrl=https://api.meta.ai/v1`、adapter が `/responses` を付与）、ヘッダ `Authorization: Bearer <LLM|key>`、`User-Agent: muse-build/1.3.0`、`x-api-version: 1.0.0`（Meta 自身のクライアントが毎回送る値、opencodex Registry の staticHeaders）。
  - **フォーク修正の適用判断**: サブスクリプション資格の 403 `client_attestation_denied` は「builtins と同名の平 `function` ツールを宣言した場合」にのみ発火する（フォーク `1cdae2f` の実測記録）。InoCode の chat プロトコルはツールを宣言しないため本ゲートは作動しない。429 クォータ拒否（5 時間窓の `retry-after`）は InoCode では再試行機構を持たないため `provider_http` 失敗イベントとして報告する（フォーク `304f62c` はクライアントの長時間停車を避けるための rewrite であり、InoCode には該当の停車機構がない）。ゆえに両修正は本実装では適用不要と判断し、記録のみ残す。
- **認証情報ストア**（opencodex `src/oauth/store.ts` の構造を単純化）: `~/.inocode/auth.json`（プロバイダ名 → 認証情報）。`access`/`refresh`/`expires_at`/`account_id`/`email`/`api_key`/`oauth_access_token`。Muse は `api_key` が bearer、`oauth_access_token` はミント用のみで要求には送らない。秘密は Debug 出力を赤化する。

## ユーザーの承認

- 参照元: 2026-11-15 深夜タスク指示（claude oauth / gpt oauth / muse code の実装、ログイン試験は翌日ユーザー実施、実装の規範は opencodex と ifokeev フォーク）。

## 実装・検証の状況

- 実装済み（2026-11-15 深夜）:
  - `core/src/oauth.rs` — 認証情報ストア（`~/.inocode/auth.json`、unix は 0600、Debug 赤化）、PKCE（OS CSPRNG、S256）、ローカルコールバック待ち受け、Claude のブラウザ/コード表示 2 モード（`claude_login_start` / `claude_login_finish`）、GPT の form-encoded 交換、Muse のデバイス許可→ポーリング→キーミント、`refresh_claude` / `refresh_gpt`。エンドポイントはすべて注入可能（テスト用）。
  - `core/src/llm.rs` — `AuthKind`（ApiKey 既定）と `ProviderKind::Responses`、方式ごとの既定エンドポイント/モデル（claude-oauth→`claude-sonnet-5`、gpt-oauth→`gpt-5.2-codex`、muse-code→`muse-spark-1.3`。参照元は上記）、要求時の資格読み込みと失効時の自動更新、Claude OAuth 要求の `anthropic-beta` と system アイデンティティブロック、Responses 要求（`store:false`、GPT は `chatgpt-account-id`、Muse は `User-Agent: muse-build/1.3.0` ＋ `x-api-version: 1.0.0`、bearer は Muse の `LLM|` キーでアカウントトークンは送らない）。
  - `server/src/main.rs` — `login claude [--code]|gpt|muse-code` サブコマンド（トークン等を表示しない）。
  - `server/tests/oauth_flows.rs` — 模擬エンドポイント 9 テスト: Claude ブラウザ/コード表示ログイン、GPT ログイン（JWT から account id 抽出、form 本文検証）、Muse デバイスフロー（pending→承認→ミント、`LLM|` キー確認）、`refresh_claude`、OAuth 3 方式での chat over TCP（ヘッダ・本体・逐次配送・完了イベント）、資格未保存時の `provider_config` 失敗。
  - README にログイン手順と方式別既定を追記。
- 検証済み（ローカル、2026-11-15 深夜〜）: `cargo fmt --all --check`、`cargo clippy --workspace --release --all-targets -- -D warnings`、`cargo build --workspace --release`、`cargo test --workspace --release`（111 passed / 0 failed / 6 ignored。うち新規: core 単体 6、統合 9）。
- **実ログイン実測（2026-11-16、Muse Code）**: 実 `login muse-code` でデバイス承認〜キーミントまで完了、`~/.inocode/auth.json` に `api_key`（`LLM|`）と `oauth_access_token` が保存された。実測で判明した Meta 側の線路事実:
  - デバイス許可エンドポイントは `Content-Type: application/x-www-form-urlencoded` が無いと 404、`User-Agent` が空だと 302（任意の UA 文字列で 200）。実装は両ヘッダを送る。
  - Model API `/v1/responses`（muse-spark-1.3、`store:false`）は 200、`response.output_text.delta` で本文、`response.completed`、`response.subscription_usage`、最後に **`data: [DONE]`**（非JSON）で終端。`event:` 行と `data:` 行のペア。
  - これを受け、Responses パーサを参照実装の passthrough アダプタ（`src/adapters/openai-responses/passthrough.ts` の `parseStream`）どおりに修正: **非JSON data 行は無視**（`JSON.parse` 失敗で `continue`、`[DONE]` もここで吸収）、**EOF で正常終了**（`response.completed` を待たない。`response.failed`/`error` のみ中途中断、`response.incomplete` は正常終了）、失敗メッセージは `error`（文字列）→ `error.message` → `incomplete_details.reason` の順。InoCode の旧規則（非JSON＝`provider_protocol` エラー、無终端EOF＝`provider_stream` 失敗）は OpenAI Chat Completions／Anthropic Messages に対しては維持する。
- **Claude/GPT OAuth のフォーク再確認修正（2026-11-16、実ログイン前）**: `src/oauth/anthropic.ts`・`callback-server.ts`・`chatgpt.ts` を精読し、InoCode 実装を参照どおりに修正:
  - CSRF state を OS CSPRNG 16 バイト hex に（参照 `generateState`。旧実装は時刻+PID で予測可能だった）。
  - コールバックで state 一致を検証（参照 `stateMatches`、不一致は「state mismatch - possible CSRF attack」で失敗）。貼り付け入力は前後の空白・改行を除去し、`code#state` の state が発行した state と一致しなければ同じ理由で失敗、state の無い生のコードは同一 PKCE セッションとして発行した state を使う（参照 `callback-server.ts` の `#waitForCallback` 手動入力: 「A raw paste with an explicit code#state suffix is state-bearing too」、`parseCallbackInput` の `input.trim()`）。
  - redirect_uri の広告を `http://localhost:{port}{path}` に（参照注記「OAuth アプリが登録した URI は localhost」。旧実装は 127.0.0.1）。リスナーは `127.0.0.1`＋`::1` の dual bind（Windows は localhost→`::1` 優先。参照 `loopbackBindHostnames`）。
  - ポート使用中はエフェメラルポートへフォールバック（固定 redirect URI でない場合のみ。参照 `#startCallbackServer`）。
  - `expires_in` 欠落・不正時は既定 3600 秒（参照 `credsFrom`）。
  - トークン POST はリダイレクト禁止＋30 秒上限、3xx は失敗（参照 `redirect: "manual"`）。コールバック待ちは 300 秒（コード表示は 600 秒、参照既定）。
  - GPT ログインも同じ構造（start/finish 分割、state 検証、`localhost` 広告）に統一。トークン交換本文に state を含めない点も参照どおり。
  - 適用しない参照機能: アカウントプロフィール補完（`resolveAnthropicAccountIdentity`、表示用メタデータのみ）、ポート 54545 使用中時の貼り付け待ち受付、ローカル Claude Code 認証情報の自動取り込み、複数アカウントの refresh intent/replay guard（単一資格ストアのため）。
- **Claude OAuth 実ログイン実測（2026-11-16）**: フォーク再確認修正後のビルドで実 `login claude`（ブラウザ承認、`localhost:54545` コールバック）が完了し、`~/.inocode/auth.json` に `claude-oauth` の access/refresh/expires（5分 skew 済み）/account_id/email が保存された。dual-bind（`127.0.0.1`＋`::1`）と state 検証付きコールバックが実通信で機能した。コールバック待ち 300 秒のタイムアウトも実測（未承認で切れた場合に正しく終端する）。
  - 実 chat（TCP 経由、claude-oauth）: `started → delta×2 → completed` ✅（OAuth Bearer＋`anthropic-beta`＋system アイデンティティブロック）。
  - 実モデル取得（claude-oauth）: 初回は `anthropic-version` ヘッダ欠落で 400（実測で判明）→ 全 Anthropic 要求で常に送るよう修正 → **17 モデル取得**（`display_name` 付き）✅。修正は commit `5698afe`。
- 未実施・ユーザー実施予定: `gpt-oauth` の実ログイン（実装済み、模擬テスト合格、実ログインのみ未実施）。Muse Code と Claude OAuth のログイン・chat・モデル取得は上記のとおり実測済み。
- **gpt-oauth 実検証（2026-11-16）**:
  - ログイン成功（`login gpt`、ブラウザ承認 → コールバック交換 → ストア保存）。途中の失敗は運用由来: 旧 URL（前回発行の state）での承認は `state mismatch - possible CSRF attack` で拒否（保護は正常動作）。
  - モデル取得成功: `client_version=99.0.0` 付き `GET /backend-api/codex/models` で **9 モデル**（`gpt-6.1-sol`, `gpt-6-astra`, `gpt-6.1-terra`, `gpt-6-sol`, `gpt-6-luna`, `gpt-5.6-sol/terra/luna`, `gpt-daybreak-blue-latest`）。Codex ロスター解析（`supported_in_api`/`visibility` フィルタ、`slug`）は実環境で動作。
  - chat: 既定 `gpt-5.2-codex` は上流 400 `model_not_found`（"not supported when using Codex with a ChatGPT account"）— ロスターがアカウント依存であることの実測確認（フォーク記録と一致）。既定モデルはロスターに存在しない場合があるため、`models` で一覧を取り `INOCODE_LLM_MODEL` で選ぶ運用を README に記載。ロスター内の `gpt-5.6-luna` では上流 429 `usage_limit_reached`（Plus、約28時間でリセット）— 要求経路全体が有効な認証で上流まで届いていることの実測。ストリーミング本体は模擬テストで網羅。
- 適用しなかった参照実装の修正と理由: フォークのツール名エイリアス（`client_` 接頭辞、403 `client_attestation_denied` 対策）はツールを宣言しない InoCode には作動しない。クォータ拒否（429 `retry-after`）の長時間待機回避も、再試行機構を持たない InoCode には該当せず、429 は通常の `provider_http` 失敗として報告される。両方とも将来ツール対応時に再考する。
- **期限切れからのリフレッシュと許可モデルでの chat の実測（2026-11-16 12:10 頃 JST、`bb2f80a` 以降のビルド）**: ユーザーの許可したモデル（Claude は haiku、GPT は luna、Muse は spark contributor の系統）だけを使い、各 1 回ずつ実行した。期限は `auth.json` の該当エントリの `expires_at_ms` を過去にして発生させた（実行前に退避し、確認後に退避ファイルは削除）。
  - Claude（`claude-haiku-5-5`）: 要求の前にリフレッシュが走り、access・refresh とも新しい値に置き換わって期限は約 475 分後になり、`account_id` は保持された。chat は `started → delta → completed`（`inocode ok`）。
  - GPT（`gpt-6-luna`）: リフレッシュで access・refresh とも新しい値になり、`account_id` は保持された。要求は上流に届き、HTTP 429 `usage_limit_reached`（Plus、`resets_in_seconds` 95807）が返った。リフレッシュ直後の要求が `chatgpt-account-id` を付けて送れたことは、`refresh_stored` が保存済みの資格情報を返すようにした修正の実地確認になる。GPT の chat 本文のストリーミングは上限の回復後まで未確認。
  - Muse（`muse-spark-1.3-contributor`）: chat は `started → delta → completed`（`inocode ok`）。Muse のエントリは変化しなかった（静的キーでリフレッシュしない）。
  - 実行後の `auth.json` には 3 プロバイダのエントリがすべて残り、`~/.inocode` には D2・D3 のロックファイル（`auth.json.lock`、`auth.json.refresh.<provider>.lock`、いずれも空）ができる。
