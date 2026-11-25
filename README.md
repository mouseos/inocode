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

設定は `~/.inocode/settings.json`（`inocode.db` と同じフォルダー）の 1 つのファイルに書く。環境変数では設定しない。API キーとログインの認証情報はこのファイルに書かず、認証情報の保管場所（`~/.inocode/auth.json`）に置く。設計と根拠：[docs/evidence/account-pool.md](docs/evidence/account-pool.md)。

```json
{
  "providers": {
    "logfare": {"kind": "openai", "default_model": "logfare/auto"},
    "claude-oauth": {"kind": "claude-oauth", "reasoning": {"effort": "medium"}},
    "local": {"kind": "openai", "base_url": "http://localhost:11434/v1", "default_model": "qwen3",
              "models": ["qwen3"], "translation": {"outgoing": true, "model_language": "English"}}
  },
  "default": {"provider": "claude-oauth", "model": "claude-sonnet-5", "effort": "high", "summary": "auto"},
  "roles": {
    "compact": {"model": "claude-haiku-5-5"},
    "explore": {"provider": "logfare", "model": "logfare/auto"},
    "colleague": {"provider": "claude-oauth", "model": "claude-opus-5-5", "effort": "high"}
  },
  "server": {"port": 8787, "sandbox": true}
}
```

- `providers`：id ごとのプロバイダー。
  - `kind`：`openai`（`/chat/completions`、Bearer）、`anthropic`（`/messages`、`x-api-key` ＋ `anthropic-version: 2023-06-01`）、`responses`（`/responses`、Bearer）、ログインで使う `claude-oauth` / `gpt-oauth` / `muse-code` / `deepai`（下表）。
  - `base_url`：省くと種類ごとの既定。`https` か、`http://localhost`・`127.0.0.1`・`[::1]` だけ。
  - `default_model`：省くと種類ごとの既定。`models`：使うモデルの一覧。省くと接続先から取る。
  - `reasoning`：このプロバイダーのモデルの思考の深さ（`none` / `minimal` / `low` / `medium` / `high` / `xhigh` / `max`、`ultra` は `max`）と要約（`auto` / `concise` / `detailed` / `none`）の既定。省くと、深さはモデルの一覧の既定、なければ `medium`。
  - `translation`：英語などしか話せないモデル向けに、送る文を訳す設定（`outgoing`、`model_language`）。値は保存するが、送る文の翻訳はまだ実装していない。
  - `disabled`：`true` で使わない。`client_version`：ChatGPT のモデル一覧に名乗る版（既定は全部が見える `99.0.0`）。
- `default`：新しい会話のプロバイダー・モデル・思考の深さ・要約。会話は始めたときの `default` を持ち続け、`thread/settings` で会話ごとに変えられる。
- `roles`：補助役のモデル。`review`（`auto` モードの審査）、`compact`（圧縮の要約）、`explore`（道具 `explore`）、`check`（ゴールの判定。省くと同僚、なければ会話のモデル）、`colleague`（行き詰まったときの同僚。省くと同僚なし）。各 `{provider, model, effort}`。`provider` を省くと会話と同じプロバイダー、`model` を省くと会話と同じモデル（`provider` を書いたときはそのプロバイダーの既定）。役ごと省くと会話のモデルが行う。
- `server`：`port`（既定 `8787`、待ち受けは常に `127.0.0.1`）、`sandbox`（既定 `true`）。起動時の `--port N`・`--sandbox on|off` がこれより優先する。

最初に起動したとき `providers` が無ければ、保存済みの認証情報から作る：`logfare` の API キーがあれば `logfare`（`openai`、`https://logfare.ai/v1`、`logfare/auto`）、ログイン済みの種類ごとにその種類の名前の項目。`default` は最初のもの。

| `kind` | API 形式と既定エンドポイント | 既定モデル | 認証 |
| --- | --- | --- | --- |
| `openai` / `anthropic` / `responses` | 各形式、`https://logfare.ai/v1` | `logfare/auto` | `inocode-server key set <id>` で保存した API キー |
| `claude-oauth` | Anthropic Messages、`https://api.anthropic.com/v1/messages` | `claude-sonnet-5` | OAuth Bearer（失効前に自動更新） |
| `gpt-oauth` | OpenAI Responses、`https://chatgpt.com/backend-api/codex/responses` | `gpt-5.2-codex` | OAuth Bearer ＋ `chatgpt-account-id` |
| `muse-code` | OpenAI Responses、`https://api.meta.ai/v1/responses` | `muse-spark-1.3` | 保存済みサブスクリプションキー（`LLM|`） |
| `deepai` | モデルごとに Chat Completions または Responses、`https://api.deepai.org/deepai_code` | `glm-5.3-flash` | 保存済み `api-key` ＋チェックイン（`docs/evidence/deepai-code-provider.md`） |

Anthropic 方式の `max_tokens` は思考の指定に応じて決まる（思考なしで 8192。思考トークンが `max_tokens` を共有するため。`docs/evidence/anthropic-messages.md`、`docs/evidence/reasoning-controls.md` 参照）。TCP プロトコル、イベント形式、`cancel` の意味は全方式で同じ。

### API キーとログイン

```bash
cargo run --release -p inocode-server -- key set logfare      # キーを入力（画面に出さない）。パイプでも渡せる
cargo run --release -p inocode-server -- login claude       # ブラウザで承認（失敗時は --code でコード貼り付け）
cargo run --release -p inocode-server -- login gpt          # ブラウザで承認
cargo run --release -p inocode-server -- login muse-code    # デバイスコードをブラウザで承認
cargo run --release -p inocode-server -- login deepai
```

- キーは認証情報の保管場所に `api-key/<プロバイダーの id>` という名前で保存する。
- ログインすると、その種類のプロバイダーが `providers` に無ければ足す（`default` が空ならそれにする）。
- 認証情報の値は Debug 出力・ログに出ない。トークンは要求時に失効を確認し、期限切れなら更新トークンで自動更新する。

> `gpt-oauth` のモデル一覧は ChatGPT アカウントごとに異なります（既定の `gpt-5.2-codex` が一覧にないアカウントでは `model_not_found` になります）。`inocode-server models gpt-oauth` で利用可能なモデルを確認し、`default_model` か会話の `model` で選んでください。

### 設定のプロトコル

| メソッド | params | result |
| --- | --- | --- |
| `providers/list` | なし | `{providers: [{id, kind, …設定, effective_base_url, effective_default_model, credential}], default}`。`credential` は認証情報があるか（読めなければ `null`） |
| `providers/add` | `{id, kind, base_url?, default_model?, models?, reasoning?, translation?, disabled?, client_version?}` | `{provider}`。同じ id、知らない種類、`https` でも localhost でもない URL は `invalid_params` |
| `providers/update` | `{id, …変える項目}`。`null` で既定に戻す | `{provider}` |
| `providers/remove` | `{id}` | `{removed: id}`。`default` か役が選んでいれば `provider_in_use`（どこで選んでいるかを書く） |
| `models/list` | `{provider}` | `{models: [...]}`（`models` と同じ形）。`models` を書いたプロバイダーはその一覧 |
| `models` | `{thread_id?}` | 会話（なければ `default`）のプロバイダーのモデル |
| `thread/settings` | `{thread_id, provider?, model?, effort?, summary?, mode?}`。変える項目が無ければ読むだけ。`null` で `default` に戻す | `{thread_id, provider, model, effort, summary, mode}` |
| `settings/get` | なし | `{default, roles, server}` |
| `settings/set` | `{default?, roles?, server?}`。各項目を 1 段だけ上書き、`null` で消す | `{default, roles, server}`。`providers` は `providers/*` で変える |

- 会話のプロバイダーやモデルを変えると、次のモデルへの要求から使う（ターンの途中でも）。プロバイダーだけ変えると、モデルはそのプロバイダーの既定になる。
- `mode` は、`turn/start` が `mode` を書かないときの承認モード。
- 使えない選択（キーが無い、止めたプロバイダー）は保存せずにエラーを返す。

### モデル一覧の取得

`models/list`（プロバイダーを指定）と `models`（会話のプロバイダー）で、接続先が提供するモデル一覧を取得できる。CLI では:

```bash
cargo run --release -p inocode-server -- models            # default のプロバイダー
cargo run --release -p inocode-server -- models logfare
```

応答の `result.models` に `id` / `display_name` / `owned_by` / `reasoning_levels` / `default_reasoning` が入る。エンドポイントは方式ごとに OpenAI 標準の `{base}/models`（Muse・GPT・Chat Completions）または Anthropic の `{base}/v1/models?limit=1000`（Messages）で、認証は chat と同一（Muse は `LLM|` キー、OAuth は保存済みトークン）。取得は常に実通信で行い、応答は 16 MiB で上限を切る。

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

Claude でサインインしていれば claude.ai のコネクタ、ChatGPT でサインインしていれば ChatGPT の apps の道具も使える。本人が公式側で入れたもの（ChatGPT が自分で付ける app は除く）は最初から使い、`mcp.json` の `connectors` で名前か id を指定して切れる:

```json
{"connectors": {"Slack": {"enabled": false}, "Pets": {"enabled": false}}}
```

- MCP の道具が合わせて 100 個以上あるときは最初から渡さず、モデルが `tool_search` で探す。見つかった道具は次の要求から渡す（Codex と同じやり方）。
- 設計と根拠：[docs/evidence/plugin-mechanism.md](docs/evidence/plugin-mechanism.md)。

### プラグインを探して入れる

モデルは `plugin_search` で Claude Code と Codex の公式カタログ（GitHub の `anthropics/claude-plugins-official` と `openai/plugins`）からプラグインを探し、`plugin_install` で入れる。入れる前に必ず本人の承認を待つ（`full` モードを除く）。

- 入れ先は `~/.inocode/plugins/installed/<カタログ>/<名前>/`。
- 入れたプラグインのスキルは、名前と説明と場所を要求ごとにモデルに見せる。MCP サーバーは `plugin_<プラグイン>_<サーバー>` として次の要求から使える。
- コマンド、フック、エージェント定義、アカウント側のストアはまだ使わない。
- 設計と根拠：[docs/evidence/plugin-mechanism.md](docs/evidence/plugin-mechanism.md)。

### 行き詰まったときの同僚

`settings.json` の `roles.colleague` に同僚のモデルを書くと、同じ呼び出しを 3 回したときや、探す・編集・実行のどれかだけを 7 手続けたときに、そのモデルに次の一手を聞き、作業中のモデルに伝える（助言の後 5 手は聞かない）。1 ターンで 2 回助言の後もまだ行き詰まれば、人に質問として回す。着手中のローカル issue（道具 `issue`）が 2 ターン続けて未完了なら、次のターンの始めに同僚に聞き、2 回の助言の後も未完了が続けば人に回す。書かなければ同僚はいない:

```json
{"roles": {"colleague": {"provider": "claude-oauth", "model": "claude-haiku-4-5"}}}
```

- `provider` を省くと、作業中のモデルと同じプロバイダー。`provider` は `providers` の id。
- 前からの書き方 `{"colleague": {"model": …, "provider": …, "base_url": …}}` も、`roles.colleague` が無いときに読む。この `provider` は `providers` の id か、ログインの種類の名前（`claude-oauth` など）。
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

### 作業の見積もり

ターンと目標を作業として記録し（頼んだ文、種類、モデル、トークン、時間、道具の内訳、変えた行とファイル、結果）、似た過去の作業から、これからの作業のトークンと分の幅を出す。`/estimate 〈作業〉` か:

```json
{"id": 1, "method": "estimate", "params": {"thread_id": 3, "text": "README を英語に翻訳する"}}
```

ターンの間は、始めとモデルへの要求のたびに `estimate` の出来事で残りの幅を送る（過去の作業が無ければ送らない）。`/goal status` にも目標の残りの幅を出す。

- 設計と根拠：[docs/evidence/effort-estimates.md](docs/evidence/effort-estimates.md)。

### 社員（任せる・レビューを頼む）

モデルに役割を付けた「社員」を `settings.json` の `members` に登録し、呼んでよい社員とモデルを `allow`（既定と、プロジェクトのフォルダごと。リポジトリの `.inocode/models.json` は承認した中身だけ）に書く。一覧に社員がいると、モデルは `delegate`（社員が同じ道具で別の会話として仕事をし、報告・編集したファイル・トークンを返す。1 段まで、予算付き）と `review`（別の会社の社員が読んで指摘を返す）を使える。`/members`（別名 `/profiles`）、`/allow` か:

```json
{"id": 1, "method": "members/add", "params": {"name": "glm-reviewer", "provider": "zai", "model": "glm-5.3", "role": "常に他社のコードを疑うレビュー役"}}
{"id": 2, "method": "allow/set", "params": {"default": ["glm-reviewer", "claude/claude-haiku-5-5"]}}
```

- 設計と根拠：[docs/evidence/company.md](docs/evidence/company.md)。

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
- 実 API の確認は `server/tests/live_*.rs` と core の一部のテスト（既定で `ignored`）。

### 実 API のテスト（テスト専用の環境変数）

実 API のテストだけは、接続先をテスト専用の環境変数で選ぶ。製品はこれを読まない。テストの補助（`server/tests/support/mod.rs` の `live_core`・`live_llm`、`core/src/llm.rs` の `live_test_llm`）が、一時フォルダーに `settings.json` を書き、キーはそのフォルダーの認証情報の保管場所に入れる（利用者の `~/.inocode` には書かない）。

| 変数 | 既定 | 用途 |
| --- | --- | --- |
| `INOCODE_TEST_PROVIDER` | `openai`（Messages のテストは `anthropic`） | プロバイダーの種類（`kind` と同じ名前） |
| `INOCODE_TEST_BASE_URL` | 種類の既定 | エンドポイント |
| `INOCODE_TEST_MODEL` | 種類の既定 | モデル |
| `INOCODE_TEST_REASONING` | モデルの既定 | 思考の深さ |
| `INOCODE_TEST_API_KEY` | なし（`openai`・`anthropic`・`responses` では必須） | API キー。ログインの種類は、この機械でログイン済みの認証情報を使う |

```bash
INOCODE_TEST_API_KEY=... cargo test --release -p inocode-server --test live_api -- --ignored --nocapture
INOCODE_TEST_PROVIDER=claude-oauth INOCODE_TEST_MODEL=claude-haiku-5-5 cargo test --release -p inocode-server --test live_api logfare_generates_and_streams_text_over_tcp -- --ignored --exact --nocapture
```

- `live_tool_search` だけは、利用者の `~/.inocode/settings.json` の `default` で動く（アカウントのコネクタを使うため）。
- `.github/workflows/protocol-test.yml` は手動実行のみ。Ubuntu / Windows / macOS で fmt・clippy・build・test を回し、`LOGFARE_API_KEY` のシークレットがある場合は `INOCODE_TEST_API_KEY` として渡して実 API の生成とストリーミングも 3 OS で確認する。キー未設定のときは実 API ジョブが失敗する（未実施を成功扱いにしない）。キーはログに出力しない。

## 記録

実装した機能・設計判断ごとの根拠（研究・仕様・参照実装・実測・承認）は `docs/evidence/` にある。一覧は [docs/evidence/README.md](docs/evidence/README.md) の「記録の一覧」。

## 現状の範囲

実装済み: フレームプロトコル、Server / Core、OpenAI 互換 API 接続と逐次配送、Anthropic Messages 互換接続、Responses 接続、Claude OAuth・GPT OAuth・Muse Code・DeepAI Code のログインと要求時認証、使用量の取得、モデル一覧取得（全方式。思考の深さの一覧と既定値を含む）、中断、複数クライアント。core のみ: 会話の型と 3 形式への変換、ツール定義の送信とストリームからのツール呼び出しの組み立て、思考の指定、SQLite の保存層（履歴・記憶・検索・連想）、規則（AGENTS.md からの取り込み・必須の規則の判定・適用の記録）とローカル issue。MCP は stdio のサーバーの道具だけ対応（上の「MCP サーバー」）。未実装: TCP での会話・ツールの公開、エージェントループとツールの実行、MCP の HTTP 接続、GUI、製品用 CLI。
