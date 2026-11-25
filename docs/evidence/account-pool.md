# プロバイダ・モデル・アカウントの登録と、アカウントのプール

- **対象と範囲：**
  - プロバイダ・モデルの登録・一覧・変更・削除。
  - 会話ごとのプロバイダ・モデル・思考の深さの選択。
  - 1 つのプロバイダに複数のアカウント（OAuth・API キー）を持ち、送るたびに選ぶプール。
  - 認証情報の保存方法。
  - 環境変数での設定をやめる。
- **関連ソースコード：** 設定ファイルは `core/src/settings.rs`、クライアントの作り方と会話ごとの選択は `core/src/lib.rs`（`Core`）、`core/src/llm.rs`（`LlmConfig::for_provider`）、会話の設定の保存は `core/src/store.rs`（版 9）、プロトコルは `server/src/lib.rs`、コマンド（`key set`、`--port`、`--sandbox`）は `server/src/main.rs`、キーの入力は `core/src/secret.rs`。認証情報は `core/src/oauth.rs`（`~/.inocode/auth.json`）。アカウントのプールは未実装。
- **根拠の状態：** UNSUPPORTED_APPROVED。学術的な裏付けは調べていない（使い勝手と互換のための仕組み）。ユーザーの指示（2026-11-25）：
  - 「環境変数での制御はだめ」
  - 「モデル登録やGPTなどoauth系opencodex同様のプールはできるのか？CRUDできるのか？それもかんがえて」
  - 「プールは、opencodex と同じだけ作る方向でよい」
- **注意（ユーザーに伝えた）：** 複数の有料契約を束ねて上限を避ける使い方は、OpenAI・Anthropic の利用規約に触れるおそれがある。

## 手本：opencodex（`refs/opencodex`、v2.100.0、2026-11-15 のコミット 1d10758f を読んだ）

- 登録：`~/.opencodex/config.json` の `providers`。HTTP の管理 API（`/api/providers` などの GET・PUT・POST・PATCH・DELETE）、CLI（`ocx provider …`、`ocx account …`）、GUI から変えられる。
- アカウントの保存：一般の OAuth は `auth.json` にプロバイダごとの `{activeAccountId, accounts[]}`。各アカウントは id・別名・資格情報・再ログイン要否・一時停止など。Codex は別の台帳（`codex-accounts.json`）に優先度・自動切り替えの閾値などを持つ。
- 選び方（`src/oauth/pool-kernel.ts`、`src/codex/routing.ts`、`src/codex/routing/selection.ts`）：
  - 方式は quota（使用量の少ない順、既定）、round-robin、fill-first、reset-first。
  - 会話をアカウントに固定する（キャッシュのため。24 時間使わなければ外す。SQLite に保存）。
  - 優先度の段、自動切り替えの閾値（既定 80%）。
- 失敗の扱い（`src/codex/routing/cooldown-math.ts` など）：
  - 429・402：`Retry-After`（最大 24 時間）か、上限が戻る時刻まで休ませて、別のアカウントへ移る。
  - 401・403：再ログインが要る印を付ける。
  - 5xx・時間切れ：3 回続けば切り替える。回避は 30 秒・2 分・10 分・30 分と段階的に延ばす。60 秒の失敗率（20 件以上で 25%）でも外す。5 分ごとに 1 回だけ試して戻すこともある。
- 使用率：Codex は応答ヘッダー `x-codex-*-used-percent` と使用量 API、Anthropic は `anthropic-ratelimit-unified-*`。
- 量：アカウントの選択と切り替えの部分で約 9,000 行（TypeScript）。組み合わせや経路の決まりまで含めると約 17,000 行。

## 方針（ユーザーと決めたこと、2026-11-25）

- 環境変数での設定をやめ、設定ファイルとプロトコルから変える。
- プールは opencodex と同じ範囲を作る。
- 認証情報は平文にしない（ユーザー：「企業で使うにはそもそも平文が問題だと思う。せめてでも何らかの読めない方式は欲しい」）。
  - 既定は OS の保管庫（Windows の資格情報マネージャー、macOS のキーチェーン、Linux の Secret Service）。Codex は Rust の `keyring` 3.6 で同じことをしている（`codex-rs/config/src/types.rs:89` の `AuthCredentialsStoreMode`：file・keyring・auto・ephemeral）。
  - 保管庫が無いときは暗号化したファイル。Windows は DPAPI、macOS・Linux は起動時のパスフレーズから Argon2id で鍵を作る。鍵をファイルの隣に置く方式は取らない。
  - 平文は、本人が明示して選んだときだけ。
  - 同じユーザーで動くマルウェアはどの方式でも防ぎきれない、とユーザーに伝えた。
- 作る順：スラッシュコマンドの定義を先に決める（ユーザー：「スラッシュコマンドをもっと定義してからやるべき」）。その後、設定の保存場所 → プロバイダ・モデル → スラッシュコマンド → アカウント → プール → 見積もり（`effort-estimates.md`）。

## 実装とテスト

### できたこと

- 設定の保存場所、プロバイダ・モデルの登録・一覧・変更・削除、会話ごとの選択、環境変数での設定の廃止。
- まだ：アカウントのプール（1 つのプロバイダに複数のアカウント）、認証情報を平文にしない保存（別の作業で `core/src/oauth.rs` の中を置き換え中）、スラッシュコマンド、見積もり。

### 設定ファイル

- 場所：データベース（`inocode.db`）と同じフォルダーの `settings.json`（ふつうは `~/.inocode/settings.json`）。前からある `language`・`translation`・`colleague` も同じファイルで、書き方は変えていない。
- 読み書きは `core/src/settings.rs`。書くときは一時ファイル（`settings.json.<プロセス番号>.tmp`）に書いてから置き換えるので、途中で落ちても半端なファイルは残らない。同じプロセスの中では書き込みを 1 つずつにする。書くたびに数を増やし（`settings::written`）、動いているターンは思考の翻訳の設定の変化をそれで知る。
- 書く前に全体を確かめる。おかしな値なら何も書かずにエラーを返す。
- API キーは設定ファイルに書かない。認証情報の保管場所に `api-key/<プロバイダの id>` の名前で置く（`oauth::Store` の公開の関数 `update` を使う。`Credential` の `api_key` 欄）。

```json
{
  "providers": {
    "logfare": {"kind": "openai", "default_model": "logfare/auto"},
    "claude-oauth": {"kind": "claude-oauth", "reasoning": {"effort": "medium", "summary": "auto"}},
    "local": {"kind": "openai", "base_url": "http://localhost:11434/v1", "default_model": "qwen3",
              "models": ["qwen3"], "translation": {"outgoing": true, "model_language": "English"},
              "disabled": false}
  },
  "default": {"provider": "claude-oauth", "model": "claude-sonnet-5", "effort": "high", "summary": "auto"},
  "roles": {
    "review": {"model": "claude-haiku-5-5"},
    "compact": {"model": "claude-haiku-5-5"},
    "explore": {"provider": "logfare"},
    "check": {"effort": "low"},
    "colleague": {"provider": "claude-oauth", "model": "claude-opus-5-5"}
  },
  "server": {"port": 8787, "sandbox": true}
}
```

- `providers`（id ごと）：
  - `kind`：`openai`・`anthropic`・`responses`（API キー）、`claude-oauth`・`gpt-oauth`・`muse-code`・`deepai`（ログイン）。
  - `base_url`：省くと種類の既定。`https` か、`http` なら `localhost`・`127.0.0.1`・`[::1]` だけ受け付ける。
  - `default_model`：省くと種類の既定。`models`：書けばモデルの一覧はこれを返し、接続先に聞かない。
  - `reasoning`：`effort`・`summary` の既定。
  - `translation`：`outgoing`・`model_language`。保存と確かめだけ。送る文を実際に訳す仕組みはまだ無い（`translation.md`）。
  - `disabled`：`true` なら使わない（選ぶと `provider_disabled`）。
  - `client_version`：ChatGPT のモデル一覧に名乗る版。前の `INOCODE_CODEX_CLIENT_VERSION` の代わり。
  - id は 1〜64 文字の英数字・`-`・`_`・`.`。
- `default`：`provider`・`model`・`effort`・`summary`。
- `roles`：`review`・`compact`・`explore`・`check`・`colleague`。それぞれ `{provider, model, effort}`、どれも省ける。
  - `provider` を省くと会話と同じプロバイダ。`model` を省くと会話と同じモデル（`provider` を書いたときはそのプロバイダの既定）。`effort` を省くと会話と同じ（`provider` を書いたときはそのプロバイダの既定）。
  - 役ごと省くと会話のモデルがする（前と同じ）。`check` を省くと同僚、同僚もいなければ会話のモデル（前と同じ）。`colleague` を省くと、前からの `colleague`（`{model, provider?, base_url?}`）を読む。その `provider` はプロバイダの id か、ログインの種類の名前。
- `server`：`port`（既定 8787）、`sandbox`（既定 `true`）。コマンドラインの `--port N`・`--sandbox on|off` が優先する。
- 初めての起動：`providers` が無ければ、保存済みの認証情報から作る。`api-key/logfare` があれば `logfare`（`openai`、`logfare/auto`）、ログイン済みの種類ごとに種類の名前の項目。`default.provider` は最初のもの（`logfare`、`claude-oauth`、`gpt-oauth`、`muse-code`、`deepai` の順）。何も無ければ書かない（次の起動でまた見る）。いったん `providers` ができたら、空になっても作り直さない。
- `login` の後：その種類のプロバイダが無ければ足し、`default.provider` が空ならそれにする。

### 会話ごとの選択

- データベースの版 9 で `thread_settings`（`session_id`、`settings`（JSON）、`updated`）を足した。前の版のデータベースはそのまま上げる（試験 `store::migration`）。
- 中身：`provider`・`model`・`effort`・`summary`・`mode`（`turn/start` が `mode` を書かないときの承認モード）。
- 会話を始めたとき、その時の `default` を写す。後で `default` を変えても、続いている会話のモデルは変わらない（判断：会話の途中でモデルが勝手に変わらないように。ユーザーには未確認）。
- 書いていない項目は `default` に従う。会話がプロバイダだけ変えたら、モデルはそのプロバイダの既定になる。
- 変更はモデルへの次の要求から効く。ターンの途中でも、次の要求からは新しいモデルで送る（試験で確かめた）。補助役のモデルはターンの始めに決める。
- 使えない選択（知らないプロバイダ、キーが無い、止めたプロバイダ）は保存しない。

### 中の作り

- `Core` はプロバイダ・モデル・思考の深さ・要約ごとに `Llm` を 1 つ作って持ち続ける（初めて要るときに作る）。鍵にはプロバイダの設定の中身も含めるので、設定を変えたら新しい `Llm` になる。
- API キーは `Llm` を作るときに読む。サーバーが動いている間に `key set` で変えた鍵は、同じプロバイダ・モデルの `Llm` には効かない（サーバーの再起動か、設定の変更で効く）。
- テストや組み込み用の `Core::with_llm`（決まった 1 つの `Llm`）も残した。プロバイダを選んでいない会話はそれで動く。

### プロトコル

| メソッド | params | result |
|---|---|---|
| `providers/list` | なし | `{providers: [{id, kind, …, effective_base_url, effective_default_model, credential}], default}` |
| `providers/add` | `{id, kind, base_url?, default_model?, models?, reasoning?, translation?, disabled?, client_version?}` | `{provider}` |
| `providers/update` | `{id, …}`（`null` で既定に戻す） | `{provider}` |
| `providers/remove` | `{id}` | `{removed}`。`default` か役が選んでいれば `provider_in_use`（「roles.review」など場所を書く） |
| `models/list` | `{provider}` | `{models}` |
| `models` | `{thread_id?}` | 会話の（無ければ `default` の）プロバイダの `{models}` |
| `thread/settings` | `{thread_id, provider?, model?, effort?, summary?, mode?}` | `{thread_id, provider, model, effort, summary, mode}` |
| `settings/get` | なし | `{default, roles, server}` |
| `settings/set` | `{default?, roles?, server?}` | `{default, roles, server}` |

- 確かめること：同じ id は不可、知らない種類は不可、URL は `https` か localhost の `http` だけ、思考の深さ・要約・モードの名前、`default` と役が名前を出すプロバイダがあること。違えば `invalid_params`。
- `thread/settings` は `thread_id` のほかに項目があれば変更、無ければ読むだけ。

### やめた環境変数と代わり

| 環境変数 | 代わり |
|---|---|
| `INOCODE_LLM_PROVIDER` | `providers.<id>.kind` と `default.provider`、会話の `thread/settings` |
| `INOCODE_LLM_BASE_URL` | `providers.<id>.base_url` |
| `INOCODE_LLM_MODEL` | `providers.<id>.default_model`、`default.model`、会話の `model` |
| `LOGFARE_API_KEY` | `inocode-server key set <id>`（認証情報の保管場所の `api-key/<id>`） |
| `INOCODE_COLLEAGUE_API_KEY` | 同僚のプロバイダの `key set`。同僚は `roles.colleague` |
| `INOCODE_CODEX_CLIENT_VERSION` | `providers.<id>.client_version` |
| `INOCODE_LLM_REASONING`・`INOCODE_LLM_REASONING_SUMMARY` | `providers.<id>.reasoning`、`default.effort`・`default.summary`、会話の `effort`・`summary` |
| `INOCODE_REVIEW_MODEL`・`INOCODE_COMPACT_MODEL`・`INOCODE_EXPLORE_MODEL`・`INOCODE_CHECK_MODEL` | `roles.review`・`roles.compact`・`roles.explore`・`roles.check` |
| `INOCODE_PORT` | `server.port`、`--port N` |
| `INOCODE_SANDBOX` | `server.sandbox`、`--sandbox on|off` |

- 残したもの：`USERPROFILE`・`HOME`（保存場所を決める OS の事実）、`LC_ALL`・`LC_MESSAGES`・`LANG`（OS の言語）。プラグインの設定の `${…}` の展開、サンドボックスの中で渡す `TMP`・`TEMP` は設定ではないので触っていない。

### テスト専用の環境変数

製品は読まない。実 API のテストの接続先を決めるだけ。テストの補助（`server/tests/support/mod.rs` の `live_core`・`live_llm`・`live_config`、`core/src/llm.rs` の `live_test_llm`）が一時フォルダーに `settings.json` を書き、キーはその一時フォルダーの認証情報の保管場所に入れる。

| 変数 | 意味 |
|---|---|
| `INOCODE_TEST_PROVIDER` | 種類（既定 `openai`。Messages のテストは `anthropic`） |
| `INOCODE_TEST_BASE_URL` | エンドポイント |
| `INOCODE_TEST_MODEL` | モデル |
| `INOCODE_TEST_REASONING` | 思考の深さ |
| `INOCODE_TEST_API_KEY` | キー（キーの種類では必須）。ログインの種類はこの機械のログイン済みの認証情報を使う |

- CI（`.github/workflows/protocol-test.yml` の live ジョブ）は、シークレット `LOGFARE_API_KEY` を `INOCODE_TEST_API_KEY` として渡す。公式 Anthropic の手順は `CLAUDE_API_KEY` を `INOCODE_TEST_API_KEY` に、`INOCODE_TEST_BASE_URL=https://api.anthropic.com`、`INOCODE_TEST_MODEL=claude-haiku-5-5`。キーはログに出さない。CI では未実行。
- `live_tool_search` だけは利用者の `~/.inocode/settings.json` のまま動く（アカウントのコネクタを使うため）。

### 試験（すべて模擬の接続先。実 API は下の 1 件だけ）

- `core/src/settings.rs`：プロバイダの追加・変更・削除で他の項目が残る、使われているプロバイダは消せない、書く前に確かめる、初めての起動で認証情報から作る、ログイン後に足す。
- `core/src/llm.rs`：種類ごとの既定、`client_version` の選び方。
- `core/src/store.rs`：版 9 への移行、会話の設定の保存。
- `server/tests/settings_tcp.rs`（実 TCP）：
  - プロバイダの追加・一覧・変更・削除と、拒む場合（同じ id、知らない種類、`http://example.com`、id の形、思考の深さ、`id` 無し）。キーは設定ファイルに入らない。
  - `models/list`：設定の一覧は接続先に聞かない。聞くときは保存したキーで送る。`models` は会話のプロバイダ。
  - 会話の設定：始めの表示、ターンの途中の変更が次の要求から効く（1 回目 `model-a`、2 回目 `model-b`、`reasoning_effort: high`）、次のターンも新しいモデル、拒む場合、`null` で戻す。
  - 会話は始めたときの `default` を持つ。キーの無いプロバイダは選べず、`key set` を案内する。
  - `default`・`roles`・`server` の読み書きと拒む場合。他の項目（`language`）は残る。
  - プロバイダが無いとき、ターンは `provider_unconfigured` で `providers/add` を案内する。
- 既存の試験：`compaction_tcp.rs` は `roles.compact` に、同僚の試験は前からの `colleague` の書き方のまま通る。

### 実 API で確かめたこと（2026-11-25、この機械、1 回）

- `server/tests/live_api.rs` の `a_turn_answers_on_the_provider_the_settings_choose` を、`INOCODE_TEST_PROVIDER=claude-oauth`、`INOCODE_TEST_MODEL=claude-haiku-5-5` で実行（`HTTPS_PROXY`・`HTTP_PROXY` を外して）。テストが一時フォルダーに `settings.json`（プロバイダ `live`、種類 `claude-oauth`、`default` にそのモデル）を書き、この機械の Claude のログインで送った。
- 結果：会話の設定は `provider=live`、`model=claude-haiku-5-5`。「What is 2 + 3? Reply with the number only.」に `5`、ターンは `completed`、約 2.5 秒。

### 確かめていないこと

- CI の live ジョブ（`INOCODE_TEST_API_KEY` での Logfare と公式 Anthropic）は動かしていない。
- `key set` のキー入力を画面に出さない処理（Windows の `SetConsoleMode`、Linux の `termios`、macOS の `stty`）は手で試していない。パイプでの入力も試していない。
- 別のプロセス（`key set`・`login` と動いているサーバー）が同時に設定ファイルを書くと、片方の変更が消えることがある（プロセスをまたぐ鍵はかけていない）。
- `gpt-oauth`・`muse-code`・`deepai` を設定から選んで実際に送ることは試していない（模擬の試験は `openai` の種類だけ）。
- 送る文の翻訳（`translation.outgoing`）は保存だけで、動きは無い。
- `core/src/oauth.rs` の説明文には `LOGFARE_API_KEY`・`INOCODE_LLM_PROVIDER` の名前が残っている（このファイルは別の作業中のため触っていない）。ほかの根拠の記録（`llm-streaming.md`、`sandbox.md` など）にも、前の環境変数の名前が当時の記録として残っている。
