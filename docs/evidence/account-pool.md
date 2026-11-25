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
- ほかの根拠の記録（`llm-streaming.md`、`sandbox.md` など）には、前の環境変数の名前が当時の記録として残っている。ほかの根拠の記録（`llm-streaming.md`、`sandbox.md` など）にも、前の環境変数の名前が当時の記録として残っている。

## 認証情報の保存先（実装済み）

- 保存方式は `settings.json` の `server.credentials`（`auto`（既定）・`keyring`・`encrypted-file`・`plaintext`）で選ぶ。サーバーは起動時にこれを読み、保管庫を一度開く。パスフレーズで閉じたファイルなら、端末でパスフレーズを尋ねる（表示しない。3 回まで）。パスフレーズを環境変数からは読まない。
- 試験の中で一時フォルダーに作るログイン情報は、平文の方式で扱う（OS の保管庫に残さず、パスフレーズも要らないように）。
- 本物の移行（2026-11-25、Windows 11）：この機械の平文の `~/.inocode/auth.json`（claude-oauth・deepai・gpt-oauth・muse-code の 4 件）が、最初の読み込みで Windows の資格情報マネージャーへ移った。ファイルには秘密を含まない目次（形式 `inocode-credentials` 版 2、`backend: keyring`、アカウントの一覧）だけが残った。gpt-oauth の値は 2 つに分けて保存された。移した後、`live_api::a_turn_answers_on_the_provider_the_settings_choose` で claude-oauth（claude-haiku-5-5、1.8 秒）と gpt-oauth（gpt-6-luna、2.4 秒）がどちらも答えた。

- **関連ソースコード：** `core/src/secrets.rs`（保存先の選択・暗号化・移行）、`core/src/oauth.rs`（`Store` がこれを使う）。
- **根拠の状態：** UNSUPPORTED_APPROVED（上の「方針」のユーザー判断による。学術研究での裏付けは調べていない）。暗号の使い方は下の公式資料・規格に合わせた。

### 何をするか

- 保存先は設定 `credentials.store` で選ぶ：`auto`（既定）・`keyring`・`encrypted-file`・`plaintext`。
  - 設定ファイルの読み込みは別の人が作っている。つなぐときは `secrets::set_mode(StoreMode::parse(値)?)` の 1 行を起動時に呼ぶ。呼ばなければ `auto`。`Store::open(path, mode)` で個別に指定もできる。
- `auto` の順番：
  1. OS の保管庫（Windows の資格情報マネージャー、macOS のキーチェーン、Linux の Secret Service）。
  2. 使えないときは暗号化したファイル。Windows は DPAPI（そのユーザーだけが開ける）。macOS・Linux はパスフレーズから Argon2id で鍵を作り、XChaCha20-Poly1305 で封じる。
  3. 平文は `plaintext` を選んだときだけ。`auto` のまま平文になることはない。
- `auto` で OS の保管庫を使うのは、既定の場所（`~/.inocode/auth.json`）の保管だけ。保管庫はユーザーに 1 つで、フォルダを消しても項目が残るため、テストなど別の場所の一時的な保管が項目を残さないようにした。別の場所で `keyring` を明示すると、項目名に場所のハッシュ（16 桁）を付けて分ける。
- パスフレーズは環境変数からは読まない。`secrets::unlock(passphrase)` で渡す（メモリーにだけ置く。`secrets::lock()` で消す）。渡していない状態で開くと「パスフレーズで封じられていて、ロックされている」というエラーになる。間違ったパスフレーズや書き換えられたファイルは「パスフレーズが違うか、ファイルが変えられた」というエラーになり、ファイルは上書きしない。
  - サーバーの起動時にパスフレーズを尋ねる部分やプロトコルの呼び出しはまだない（サーバー側は別の人の担当範囲）。
- 鍵は保存しない。ファイルには Argon2id の設定（64 MiB・3 回・4 レーン、RFC 9106 第 4 節の 2 番目の推奨）、16 バイトの乱数ソルト、24 バイトの乱数ノンスだけを書く。これらの見出し部分も暗号の認証対象（AAD）に入れたので、書き換えると復号に失敗する。メモリーやパスの数値が大きすぎる見出しは読む前に断る（1 GiB・64 回・16 レーンまで）。
- 鍵の作成は遅い（わざと）。同じ見出しの鍵はプロセスの中で 1 回だけ作り、メモリーに置いておく。
- 保存の単位は（プロバイダ、アカウント）。今の 1 種類 1 件は、アカウント名 `default`。
  - OS の保管庫：サービス名 `inocode`、項目名 `<プロバイダ>/<アカウント>`（例 `gpt-oauth/default`）。ファイル `auth.json` には、どのアカウントがあるかの一覧（秘密は含まない）だけが残る。
  - 暗号化ファイル・平文：`auth.json` の中にプロバイダ → アカウント → 認証情報の形で入る。
- 大きな値の分割：Windows の資格情報は 1 件 2,560 バイトまでなので、2,048 バイトずつ分けて `<項目名>#<世代>.<番号>` に入れ、本体の項目には世代・個数・SHA-256 を書く。新しい世代を全部書いてから本体を差し替え、古い世代を消すので、読む側が新旧を混ぜることはない（混ざったら SHA-256 が合わず、読み直す）。
- 移行：最初に開いたとき、元の平文 `auth.json`（形式の印がないもの）があれば、選んだ保存先へ全部移し、読み戻して同じか確かめてから、`auth.json` を新しい内容（一覧か暗号化ファイル）で置き換える（名前の付け替えで、途中で落ちても壊れない）。確かめに失敗したら元のファイルをそのまま残し、エラーで知らせる。保存先の設定を変えたときも同じ手順で移り、OS の保管庫から出るときは保管庫の項目を消す。
- ファイルは所有者だけが読める（Unix は 0600）。Windows はユーザープロファイルの ACL と DPAPI に任せる。
- プロセスをまたぐ排他は従来どおりファイルのロック：保存は `auth.json.lock`、トークンの更新は `auth.json.refresh.<プロバイダ>.lock`。保存先によらず同じように効く。
- Linux の Secret Service の部品は、自前の Tokio ランタイムで待つため、Tokio の中から呼ぶと止まる（`zbus` 4.4 `src/utils.rs` の `block_on`）。そのため Linux では保管庫の呼び出しを別スレッドで行う。

### 確かめた事実と出典

- `keyring` 3.6.3 の文書（https://docs.rs/keyring/3.6.3/keyring/ 、2026-11-25 に読んだ）：既定の機能は無く、保管先は機能で選ぶ。`windows-native`・`apple-native`・`linux-native-async-persistent`（keyutils と非同期 Secret Service。非同期ランタイムと暗号の選択が必要）。どれも無い OS では試験用のまね（mock）になる。保管庫によっては同じ項目への同時アクセスが失敗することがある。
- `keyring` 3.6.3 のソース `src/windows.rs`：秘密が `CRED_MAX_CREDENTIAL_BLOB_SIZE`（2,560 バイト）を超えると `TooLong` を返す。文字列で渡すと UTF-16 になるので半分しか入らない。そのため `set_secret`（バイト列）で分割して入れる。保存は `CRED_PERSIST_ENTERPRISE`（ドメイン環境では移動プロファイルと一緒に動く）。
- Microsoft の CryptProtectData の文書（https://learn.microsoft.com/en-us/windows/win32/api/dpapi/nf-dpapi-cryptprotectdata 、2026-11-25 に読んだ）：通常は同じログオン資格情報のユーザーだけが、同じコンピューターで復号できる（移動プロファイルなら別のコンピューターでも）。改ざん検出の MAC が付く。追加エントロピーは復号時にも同じものが要る。出力は `LocalFree` で解放する。`CRYPTPROTECT_UI_FORBIDDEN` は画面を出さない。
- Codex（`D:\work\software\OSS\codex\codex-rs`）：
  - `keyring-store/Cargo.toml`：Linux は `linux-native-async-persistent`、macOS は `apple-native`、Windows は `windows-native`、`crypto-rust`。
  - `keyring-store/src/lib.rs`：`NoEntry` を「無い」として扱い、それ以外はエラー。分割はしていない。
  - `login/src/auth/storage.rs`：項目名は Codex のホームフォルダのハッシュ（`cli|` + 16 桁）。`auto` は保管庫に失敗すると平文ファイルへ戻る（inocode は平文へは戻らない）。保管庫に書けたら平文ファイルを消す。
  - `secrets/src/local.rs`：別方式では age で暗号化したファイルを使い、そのパスフレーズを保管庫に置く。
- 追加した依存（すべて純 Rust か OS の API の呼び出し）：
  - `keyring` 3.6.3：Windows は `windows-sys`（OS API）、macOS は `security-framework`（Security.framework の呼び出し）、Linux は `linux-keyutils`（カーネルのシステムコール）と `secret-service` 4 / `zbus` 4（純 Rust の D-Bus）、暗号は RustCrypto（`crypto-rust`）。
  - `argon2` 0.5.3、`chacha20poly1305` 0.10.1、`zeroize` 1（RustCrypto、純 Rust）。
  - `windows-sys` に機能 `Win32_Security_Cryptography` を追加（DPAPI）。
  - `Cargo.lock` に `dbus`・`libdbus-sys`（C の libdbus）が載るが、`keyring` の `dbus-secret-service?/...` という弱い機能指定を Cargo が記録するためで、ビルドには入らない。`cargo tree -p inocode-core --target all -i libdbus-sys -e normal` が何も出さないことで確かめた。

### プロジェクト内の実測（2026-11-25、Windows 11）

- `cargo test -p inocode-core --lib secrets::tests::round_trips_a_dummy_secret_in_the_os_store_here -- --ignored`：6,000 バイトの仮の値を本物の資格情報マネージャーに 3 つに分けて入れ、読み戻し、消せた。あとで `cmdkey /list` に `inocode` の項目が残っていないことを確かめた。
- 単体テスト：パスフレーズでの往復、間違ったパスフレーズ・ロック中（ファイルは変わらない）、暗号文・ノンス・鍵作成の設定の書き換え、DPAPI の往復と書き換え（Windows）、分割と古い部分の削除・欠けた部分の検出、平文からの移行（保管庫へ・保管庫が使えないときは暗号化ファイルへ）、読み戻しに失敗したときに元ファイルが残ること、`keyring` を明示したときは平文へ戻らないこと、既定の場所以外では `auto` が保管庫に触らないこと、平文から `auto` への移行。テストはすべて一時フォルダとメモリー上のまねの保管庫で行い、ホームフォルダには触れない。

### 限界

- 同じユーザーとして動くマルウェアは、どの方式でも読める（保管庫も DPAPI も、そのユーザーなら開ける。パスフレーズ方式でも、開いている間はメモリーにある）。
- Linux のデスクトップ無しの環境（Secret Service が無い）や macOS・Linux で暗号化ファイルを使うときは、パスフレーズを渡すまで認証情報を使えない。
- 元の平文ファイルは名前の付け替えで置き換えるが、ディスク上の古い領域を消すわけではない。
- 認証情報は使っている間、プロセスのメモリーに平文の文字列としてある（`Credential` のフィールドは消去しない）。
- `core` の単体テストは、Windows 以外では既定を `plaintext` にしている（パスフレーズを待たないため）。`server/tests` などの結合テストは `auto` のままで、Windows では DPAPI のファイルになるが、macOS・Linux ではパスフレーズが無いため保存に失敗する。これらのテストに `secrets::set_mode(StoreMode::Plaintext)` を加える必要がある（その部分は別の人の担当範囲なので変えていない）。

### 未確認

- macOS のキーチェーンと Linux の Secret Service での実際の動作（この機械は Windows だけ）。Linux 用のコードはこの機械ではコンパイルもしていない。
- Linux で Tokio の中から呼んだときに別スレッドにしたことで止まらないこと（ソースを読んだだけ）。
- Argon2id の設定での実際の所要時間。
