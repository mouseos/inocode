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
| `LOGFARE_API_KEY` | なし（必須） | プロバイダーの API キー |
| `INOCODE_LLM_PROVIDER` | `openai` | 接続方式: `openai`（`/chat/completions`、Bearer 認証）, `anthropic`（`/messages`、`x-api-key` ＋ `anthropic-version: 2023-06-01`）, `responses`（`/responses`、Bearer 認証）またはサブスクリプション/OAuth 方式の `claude-oauth` / `gpt-oauth` / `muse-code` / `deepai`（下記） |
| `INOCODE_LLM_BASE_URL` | `https://logfare.ai/v1`（方式ごとの既定は下表） | API Base URL |
| `INOCODE_LLM_MODEL` | `logfare/auto`（方式ごとの既定は下表） | モデル名 |
| `INOCODE_COMPACT_MODEL` | 作業中のモデル | 圧縮の要約を書くモデル（同じ接続方式・同じ認証）。作業中のモデルと同じなら、要約の要求でキャッシュが効く |
| `INOCODE_EXPLORE_MODEL` | 作業中のモデル | 道具 `explore` の探索役のモデル（同じ接続方式・同じ認証）。探索役は `find`・`read` だけを使い、場所と行の範囲を返す（[docs/evidence/explore-subagent.md](docs/evidence/explore-subagent.md)） |
| `INOCODE_PORT` | `8787` | 待ち受けポート（待ち受けは常に `127.0.0.1`） |
| `INOCODE_LLM_REASONING` | モデルの既定値（一覧にあれば）、なければ `medium` | 思考の深さ: `none` / `minimal` / `low` / `medium` / `high` / `xhigh` / `max`（`ultra` は `max`）。受け付けない値は接続先のエラーをそのまま返す |
| `INOCODE_LLM_REASONING_SUMMARY` | `auto` | 思考の要約: `auto` / `concise` / `detailed` / `none` |

Anthropic 方式の `max_tokens` は思考の指定に応じて決まる（思考なしで 8192。思考トークンが `max_tokens` を共有するため。`docs/evidence/anthropic-messages.md`、`docs/evidence/reasoning-controls.md` 参照）。Logfare の `/v1/messages` は Anthropic Messages API 互換なので、`INOCODE_LLM_PROVIDER=anthropic` を設定するだけで切り替わる。TCP プロトコル、イベント形式、`cancel` の意味は全方式で同じ。

### サブスクリプション / OAuth ログイン

`claude-oauth`（Claude Pro/Max）、`gpt-oauth`（ChatGPT/Codex）、`muse-code`（Meta Muse Code）は認証情報ストア `~/.inocode/auth.json` を使う。最初に一度だけログインする:

```bash
cargo run --release -p inocode-server -- login claude       # ブラウザで承認（失敗時は --code でコード貼り付け）
cargo run --release -p inocode-server -- login gpt          # ブラウザで承認
cargo run --release -p inocode-server -- login muse-code    # デバイスコードをブラウザで承認
```

ログイン後は `INOCODE_LLM_PROVIDER` を切り替えるだけで使える（API キー変数は不要）:

| `INOCODE_LLM_PROVIDER` | API 形式と既定エンドポイント | 既定モデル | 認証 |
| --- | --- | --- | --- |
| `claude-oauth` | Anthropic Messages、`https://api.anthropic.com/v1/messages` | `claude-sonnet-5` | OAuth Bearer（失効前に自動更新） |
| `gpt-oauth` | OpenAI Responses、`https://chatgpt.com/backend-api/codex/responses` | `gpt-5.2-codex` | OAuth Bearer ＋ `chatgpt-account-id` |
| `muse-code` | OpenAI Responses、`https://api.meta.ai/v1/responses` | `muse-spark-1.3` | 保存済みサブスクリプションキー（`LLM|`） |
| `deepai` | モデルごとに Chat Completions または Responses、`https://api.deepai.org/deepai_code` | `glm-5.3-flash` | 保存済み `api-key` ＋チェックイン（`docs/evidence/deepai-code-provider.md`） |

> `gpt-oauth` のモデル一覧は ChatGPT アカウントごとに異なります（既定の `gpt-5.2-codex` が一覧にないアカウントでは `model_not_found` になります）。`inocode-server models` で利用可能なモデルを確認し、`INOCODE_LLM_MODEL` で選んでください。

認証情報はファイルに保存される（秘密値は Debug 出力・ログに出ない）。トークンは要求時に失効を確認し、期限切れなら更新トークンで自動更新する。

### モデル一覧の取得

`models` で接続先が提供するモデル一覧を取得できる。CLI では:

```bash
cargo run --release -p inocode-server -- models
```

TCP プロトコルでは `models` メソッド（即時応答、`{"id": n, "method": "models"}`）で、応答の `result.models` に `id` / `display_name` / `owned_by` が入る。エンドポイントは方式ごとに OpenAI 標準の `{base}/models`（Muse・GPT・Chat Completions）または Anthropic の `{base}/v1/models?limit=1000`（Messages）で、認証は chat と同一（Muse は `LLM|` キー、OAuth は保存済みトークン）。取得は常に実通信で行い、応答は 16 MiB で上限を切る。

```bash
export LOGFARE_API_KEY=...            # 環境変数からのみ読む。リポジトリに書かない
cargo run --release -p inocode-server
```

### MCP サーバー

`~/.inocode/mcp.json`（スレッドの保存先 `inocode.db` と同じフォルダー）に書いた MCP サーバーの道具を、モデルが使える。形式は Claude Desktop などと同じ `mcpServers`:

```json
{
  "mcpServers": {
    "github": {"command": "github-mcp-server", "args": ["stdio"], "env": {"GITHUB_TOKEN": "..."}}
  }
}
```

- リモートのサーバーは Streamable HTTP でつなぐ。`command` の代わりに `url` と、必要なら `headers` を書く（OAuth は未対応なので、鍵はヘッダーで渡す）:

  ```json
  {"mcpServers": {"remote": {"url": "https://example.com/mcp", "headers": {"Authorization": "Bearer ..."}}}}
  ```

- 使うのは道具（tools）だけで、リソース・プロンプトなどは使わない。
- サーバーは最初のターンで起動し、inocode-server が終わるまで使い続ける。設定を変えたら inocode-server を起動し直す。起動に 30 秒以上かかったり失敗したりしたサーバーは飛ばし、標準エラーに理由を出す。
- 道具の名前は `mcp__<サーバー名>__<道具名>`（英数字・`_`・`-` 以外は `_` にし、64 文字で切る）。
- 承認：MCP の道具の呼び出しは変更として扱う（`ask` では人の承認を待ち、`auto` では審査される）。
- Windows では `npx` などは `npx.cmd` と書く（拡張子を補わないため）。
- 設計と根拠：[docs/evidence/mcp-client.md](docs/evidence/mcp-client.md)。

### アカウントのコネクタと apps

Claude でサインインしていれば claude.ai のコネクタ、ChatGPT でサインインしていれば ChatGPT の apps の道具も使える。公式側で入れてあるものは最初から使い、`mcp.json` の `connectors` で名前か id を指定して切れる:

```json
{"connectors": {"Slack": {"enabled": false}, "Pets": {"enabled": false}}}
```

- 道具が 100 個を超えるコネクタや app は、毎回の要求に入れきれないので使わない（標準エラーに理由を出す）。
- 設計と根拠：[docs/evidence/plugin-mechanism.md](docs/evidence/plugin-mechanism.md)。

### 行き詰まったときの同僚

`settings.json` に同僚のモデルを書くと、同じ呼び出しを 3 回したときや、探す・編集・実行のどれかだけを 7 手続けたときに、そのモデルに次の一手を聞き、作業中のモデルに伝える（助言の後 5 手は聞かない）。1 ターンで 2 回助言の後もまだ行き詰まれば、人に質問として回す。着手中のローカル issue（道具 `issue`）が 2 ターン続けて未完了なら、次のターンの始めに同僚に聞き、2 回の助言の後も未完了が続けば人に回す。書かなければ同僚はいない:

```json
{"colleague": {"model": "claude-haiku-4-5", "provider": "claude-oauth"}}
```

- `provider` を省くと、作業中のモデルと同じ提供元・同じ認証。`openai`・`anthropic`・`responses` では鍵を環境変数 `INOCODE_COLLEAGUE_API_KEY` に置き、必要なら `base_url` を書く。
- 設計と根拠：[docs/evidence/colleague-review.md](docs/evidence/colleague-review.md)。

### 画像とカメラ

- `read` で png・jpg・gif・webp を読むと、画像としてモデルに見せる（長辺 1,568 画素に縮める。`region` で一部を元の解像度で見る）。
- 道具 `camera` で、カメラや UVC の HDMI キャプチャの一覧、調整できる項目、写真、動画（ffmpeg が要る）を扱う。撮影は full モード以外では必ず人の承認を待つ。調整の値に `"auto"`・`"manual"` を書くと、自動・手動を切り替える（Windows。撮影と同じく未試行）。
- 設計と根拠：[docs/evidence/image-input.md](docs/evidence/image-input.md)。

### モデルの一覧の価格とベンチマーク

`models/catalog` で、接続先のモデルの一覧に、価格（models.dev）、ベンチマークの順位（LLMBenchmark、CC BY 4.0）、ここで測った速さ、請求の種類、GPT の fast・flex と Daybreak、DeepAI の Theos、Claude のプランを付けて返す。取得した写しは 12〜24 時間使う。

- 設計と根拠：[docs/evidence/model-catalog.md](docs/evidence/model-catalog.md)。

### 使用量のログ

モデルへの要求を 1 件ずつ記録する（ターン、探索役、同僚）。`stats/models` で、モデルごとの件数・トークン・キャッシュの割合・最初の文字までの時間・tok/s（中央値）を、時間帯ごとにも返す:

```json
{"id": 1, "method": "stats/models", "params": {"days": 30, "utc_offset_minutes": 540}}
```

- 設計と根拠：[docs/evidence/usage-log.md](docs/evidence/usage-log.md)。

### 返答の言語

モデルは、OS の表示言語で返答する。話しかけた文から言語を推定はしない。会話の中で別の言語を頼むと、最後に頼んだ言語に変わる。OS と違う言語を既定にするには、`~/.inocode/settings.json`（`inocode.db` と同じフォルダー）に書く:

```json
{"language": "Japanese"}
```

- OS の表示言語は、Windows では表示言語の設定の 1 番目、それ以外では `LC_ALL`・`LC_MESSAGES`・`LANG` の順で最初に値のあるもの（`C`・`POSIX` は言語なしとみなす）。どちらからも決まらなければ、言語の指示を入れない。
- 設定は毎ターンの始めに読む。
- 設計と根拠：[docs/evidence/reply-language.md](docs/evidence/reply-language.md)。

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

実装した機能・設計判断ごとの根拠（研究・仕様・参照実装・実測・承認）は `docs/evidence/` にある。一覧は [docs/evidence/README.md](docs/evidence/README.md) の「記録の一覧」。

## 現状の範囲

実装済み: フレームプロトコル、Server / Core、OpenAI 互換 API 接続と逐次配送、Anthropic Messages 互換接続、Responses 接続、Claude OAuth・GPT OAuth・Muse Code・DeepAI Code のログインと要求時認証、使用量の取得、モデル一覧取得（全方式。思考の深さの一覧と既定値を含む）、中断、複数クライアント。core のみ: 会話の型と 3 形式への変換、ツール定義の送信とストリームからのツール呼び出しの組み立て、思考の指定、SQLite の保存層（履歴・記憶・検索・連想）、規則（AGENTS.md からの取り込み・必須の規則の判定・適用の記録）とローカル issue。MCP は stdio のサーバーの道具だけ対応（上の「MCP サーバー」）。未実装: TCP での会話・ツールの公開、エージェントループとツールの実行、MCP の HTTP 接続、GUI、製品用 CLI。
