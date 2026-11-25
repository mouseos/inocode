# プロバイダ・モデル・アカウントの登録と、アカウントのプール

- **対象と範囲：**
  - プロバイダ・モデルの登録・一覧・変更・削除。
  - 会話ごとのプロバイダ・モデル・思考の深さの選択。
  - 1 つのプロバイダに複数のアカウント（OAuth・API キー）を持ち、送るたびに選ぶプール。
  - 認証情報の保存方法。
  - 環境変数での設定をやめる。
- **関連ソースコード：** 設定ファイルは `core/src/settings.rs`、クライアントの作り方と会話ごとの選択は `core/src/lib.rs`（`Core`）、`core/src/llm.rs`（`LlmConfig::for_provider`）、会話の設定の保存は `core/src/store.rs`（版 9）、プロトコルは `server/src/lib.rs`、コマンド（`key set`、`--port`、`--sandbox`）は `server/src/main.rs`、キーの入力は `core/src/secret.rs`。認証情報は `core/src/oauth.rs`（`~/.inocode/auth.json`）。アカウントの登録簿は `core/src/accounts.rs`・`core/src/oauth.rs`（`Store`）・`core/src/secrets.rs`（`AccountInfo`）。アカウントのプール（送るたびに選ぶ仕組み）は `core/src/pool.rs`・`core/src/store_pool.rs`（下の「アカウントのプール」）。
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
- 認証情報を平文にしない保存（下の「認証情報の保存先」）、スラッシュコマンド（`slash-commands.md`）。
- アカウントの登録（1 つのプロバイダの種類に複数のアカウント。下の「アカウントの登録」）。
- アカウントのプール（送るたびにアカウントを選ぶ・切り替える。下の「アカウントのプール」）。
- まだ：見積もり。

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
  - 役ごと省くと会話のモデルがする（前と同じ）。`check` を省くと同僚、同僚もいなければ会話のモデル（前と同じ）。`colleague` を省くと同僚はいない。ファイルの最上位の前からの `colleague` は読まず、残っていれば設定の誤りになる（`colleague-review.md`）。
  - 審査役は、ターンを始めたときのモードによらず用意する。ターンの途中で `auto` に変えても、設定した審査役が審査する。
- `server`：`port`（既定 8787）、`sandbox`（既定 `true`）、`stream_idle_seconds`（既定 300、1〜86400）。コマンドラインの `--port N`・`--sandbox on|off` が優先する。
  - `stream_idle_seconds`：モデルの答えが、始まるのを待つ間も途中も、この秒数なにも送ってこなければ、その要求を `provider_idle` の誤りで終える（ターンは失敗で終わる）。プロバイダは作業中も数秒〜数十秒ごとに何かを送るので、5 分の沈黙は止まった接続とみなす（判断。研究ではない）。変えると、それ以後に作るクライアントから効く。
- 設定ファイルが読めない（JSON の誤り、知らない項目）ときやデータベースが開けないときは、サーバーは既定値で動かず、その誤りを出して起動しない。
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
- API キーは、クライアントを求めるたびに保管場所から読み、その指紋（SHA-256 の先頭 8 バイト）も `Llm` を選ぶ鍵に含める。サーバーが動いている間に `key set` で変えた鍵は、次の要求から使われる。
- 設定ファイルの書き込みは、同じプロセスの中の順番に加え、`settings.json.lock` の OS のロックで、別のプロセス（`login`・`key set` と動いているサーバー）の書き込みとも順番にする（読んでから書き終えるまで持つ。10 秒待って取れなければ誤り）。動いているターンが設定の変化に気づくための目印は、このプロセスの書き込みの回数と、ファイルの更新時刻の両方から作る。
- 同じデータベースを使うサーバーは 1 つだけ：起動時に `inocode.db.lock` を OS のロックで取り、取れなければ起動しない。ターンの持ち主（1 スレッド 1 ターン）はそのプロセスの中で決める。
- テストや組み込み用の `Core::with_llm`（決まった 1 つの `Llm`）も残した。プロバイダを選んでいない会話はそれで動く。

### プロトコル

| メソッド | params | result |
|---|---|---|
| `providers/list` | なし | `{providers: [{id, kind, …, effective_base_url, effective_default_model, credential}], default}` |
| `providers/add` | `{id, kind, base_url?, default_model?, models?, reasoning?, translation?, disabled?, client_version?}` | `{provider}` |
| `providers/update` | `{id, …}`（`null` で既定に戻す） | `{provider}` |
| `providers/remove` | `{id}` | `{removed}`。`default` か役が選んでいれば `provider_in_use`（「roles.review」など場所を書く）。会話が選んでいても `provider_in_use`（会話の番号を書く）。消したら、保管場所のそのプロバイダの API キーも、キーのプールの全部を消す（同じ名前で足し直したとき古い鍵を使わないため） |
| `models/list` | `{provider, include_hidden?}` | `{models}`。隠したモデルは `include_hidden` が `true` のときだけ |
| `models` | `{thread_id?, include_hidden?}` | 会話の（無ければ `default` の）プロバイダの `{models}`。隠したモデルは同じく除く |
| `thread/settings` | `{thread_id, provider?, model?, effort?, summary?, mode?}` | `{thread_id, provider, model, effort, summary, mode}` |
| `settings/get` | なし | `{default, roles, server, effective, restart_required, restart_for}` |
| `settings/set` | `{default?, roles?, server?}` | `settings/get` と同じ |

- `settings/get` の `server` はファイルに書いた値、`effective` はいま効いている値：`port`（待ち受けているポート）、`sandbox`（命令を閉じ込めているか）、`sandbox_from_command_line`（`--sandbox` で決めたか）、`credentials`（認証情報の保存方式）、`stream_idle_seconds`。`restart_required` は、ファイルの値が再起動まで効かないとき `true`、`restart_for` はその項目（`port`・`credentials`）。
- `server.sandbox` の変更は、`--sandbox` で起動していなければ、それ以後に始める命令から効く（再起動は要らない）。`--sandbox` で起動したときは、コマンドラインの値が勝つ。
- 書き込みの前に、いまのファイルが読めることを確かめる。壊れたファイルは、頼んだ変更の誤り（`invalid_params`）ではなく `settings_invalid` で返す。

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
- 保存の単位は（プロバイダ、アカウント）。種類ごとの最初のアカウントの名前は `default`（下の「アカウントの登録」）。
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

## アカウントの登録（実装済み）

- **対象と範囲：** ログインで使う 4 つの種類（`claude-oauth`・`gpt-oauth`・`muse-code`・`deepai`）に、それぞれ複数のアカウントを持たせる。プロトコルとスラッシュコマンドからのログイン、一覧、別名、一時停止、削除、使うアカウントの選択。API キーの種類（`openai`・`anthropic`・`responses`）も、1 プロバイダに複数のキーをアカウントとして持てる（下の「アカウントのプール」の「API キーのプール」）。送るたびにアカウントを選び替えるプールは下の「アカウントのプール」。
- **関連ソースコード：** `core/src/accounts.rs`（プロトコルの処理、ログインの進行、ログインの完了を CLI と共通にした `complete_login`）、`core/src/oauth.rs`（`Store` のアカウントの操作、`same_identity`、`choose_account`、更新が断られたときの印）、`core/src/secrets.rs`（`AccountInfo` と、ファイルへの保存、消したアカウントの OS の保管庫の項目の削除）、`core/src/settings.rs`（`active_account`、`forget_account`）、`core/src/llm.rs`（`with_account`、401 のときの扱い）、`core/src/connectors.rs`、`core/src/commands.rs`、`server/src/lib.rs`、`server/src/main.rs`（`login`）、`core/src/deepai.rs`（`username` を記録）。
- **根拠の状態：** UNSUPPORTED_APPROVED（上の「方針」のユーザー判断。作る順の「アカウント」の段）。学術研究での裏付けは調べていない。

### 保存の形

- アカウントの id：種類ごとの最初のアカウントは `default`（前からある 1 件はそのまま `default` として使える）。2 件目からは乱数の 16 進 8 桁。
- 秘密でない情報（`AccountInfo`）：`alias`（別名）、`paused`（一時停止）、`needs_reauth`（ログインし直しが要る）、`added_ms`（足した時刻、ミリ秒。前からある `default` は不明なので無い）。認証情報の保管場所のファイル（`auth.json`）の最上位の `account_info` に、種類 → アカウント → 情報の形で置く。保存方式（OS の保管庫・DPAPI・パスフレーズ・平文）によらず、ここは暗号化しない。秘密は入れない。
- メールアドレスとアカウント番号（Claude の `account.uuid`、ChatGPT の `chatgpt_account_id`、Muse の `user_id`、DeepAI の `username`）は、前と同じく認証情報の中にあり、一覧のときだけ読み出して見せる。`account_info` には書かない（暗号化したファイルでは、メールアドレスを平文に出さないため）。
- ChatGPT のメールアドレス：トークンの応答に `email` が無ければ、`id_token` の `email` を読む（同じ人かを判定するため）。DeepAI：ログインの応答の `username` を `account_id` に入れる。
- ファイルの形式の版は 2 のまま。`account_info` は省けるので、前のファイルはそのまま読める。前の版の inocode は `account_info` を読まず、書き直すと消す（アカウントの秘密は残る）。
- OS の保管庫のとき、目次のファイルは毎回書き直す（`account_info` の変化を残すため）。アカウントを消したら、目次から外した後で、そのアカウントの保管庫の項目を消す。

### 同じ人の判定

- 新しいログインの認証情報を、同じ種類の各アカウントと比べる。メールアドレス（大文字小文字・前後の空白を無視）とアカウント番号のうち、両方が知っているものが全部一致し、少なくとも 1 つは両方が知っているとき、同じ人とみなす。
- 両方を見る理由：ChatGPT のアカウント番号はワークスペースの番号で、同じチームの別の人と同じになる。逆に 1 人が 1 つのメールアドレスで個人とチームの 2 つのワークスペースに入れる（別の上限）。
- 同じ人なら、そのアカウントの認証情報を新しいものに置き換え、`needs_reauth` を外す。新しい応答に無いメールアドレス・番号は前のものを残す。別名・一時停止・足した時刻は変えない。
- どれとも同じでなければ新しいアカウントとして足す。どちらにも何も分からないとき（前の DeepAI の `default` など）は別人とみなすので、足される。

### 使うアカウントの選び方

- 設定のプロバイダに `active_account`（アカウントの id）を足した。`accounts/use` か `providers/update` で変える。API キーの種類では、キーのプールのアカウントの id を書く。
- 設定のプロバイダから送る要求は、アカウントのプールが選ぶ（下の「アカウントのプール」）。`active_account` はプールが優先する手で選んだアカウントで、それが使えないあいだはプールがほかへ移る。
- 使えるアカウントが 1 つも無いときの誤り（`oauth::choose_account` が作る。どれもターンは `failed`）：
  - アカウントが 1 つも無い：`provider_config`（`/login` を案内）。
  - `account_paused`：`/accounts resume` か `/accounts use` を案内。
  - `account_needs_reauth`：`/login 〈種類〉` か `/accounts use` を案内。
  - `account_unknown`：`active_account` のアカウントが無い。
  - `account_unavailable`：全部が一時停止かログインし直し。
  - `account_ambiguous` は、設定のプロバイダを通らない使い方（`choose_account` を直接呼ぶコネクタ）にだけ残る。
- ログインは `active_account` を変えない（判断：ログインで知らないうちに使うアカウントが替わらないように）。2 つ目のアカウントにログインすると、次の要求からプールが 2 つの中から選ぶ。CLI の `login` はそのとき、プールが選ぶことと `/accounts status`・`/accounts use` を出す。
- `active_account` を変えると、設定の中身が変わるのでクライアントが作り直され、次の要求から効く。
- コネクタ（`/connectors`）は、その種類の止めていないプロバイダのうち、最初に `active_account` を持つものの選択を使う。無ければ使えるアカウントが 1 つのときだけそれで、複数あれば `account_ambiguous`（プールは使わない。理由は下の「アカウントのプール」の「コネクタ」）。

### ログインし直しが要る印

- トークンの更新で、トークンの窓口が HTTP 400・401・403 を返したとき（更新トークンが断られたとき）に付ける。つながらない・429・5xx では付けない（ログインし直しでは直らないため）。
- モデルの API が 401 を返したとき：Claude・ChatGPT は、断られたトークンでの更新を一度試す（それも断られれば上のとおり印が付く。更新できれば印は付けない）。Muse・DeepAI は更新できないので、すぐ付ける。その要求自体は失敗のまま（もう一度送ることはしない）。
- 同じ人でログインし直すと外れる。

### プロトコル

| メソッド | params | result |
|---|---|---|
| `accounts/list` | `{provider?}` | `{accounts: [{id, alias, provider, kind, paused, needs_reauth, added, email, account_id, priority, auto_switch_threshold, active}]}` |
| `accounts/login/start` | `{provider, mode?}` | `{login_id, provider, kind, mode, url, user_code?, status, expires_in}` |
| `accounts/login/code` | `{login_id, code}` | `accounts/login/status` と同じ（交換が終わるまで最大 60 秒待つ） |
| `accounts/login/status` | `{login_id}` | `{login_id, …, status: waiting・done・failed・cancelled, account?, added?, error?}` |
| `accounts/login/cancel` | `{login_id}` | 同じ形（`status: cancelled`） |
| `accounts/rename` | `{provider, account, alias}`（`null` か空で外す） | `{account: {…}}` |
| `accounts/pause` | `{provider, account, paused}` | `{account: {…}}` |
| `accounts/remove` | `{provider, account}` | `{removed, kind}` |
| `accounts/add_key` | `{provider, key, alias?}` | `{account: {…}, added}` |
| `accounts/use` | `{provider, account}`（`null` で選択を外し、プールに任せる） | `{provider, active_account}` |

- `provider`：設定のプロバイダの id。無ければ種類の名前（`claude-oauth` など）、CLI と同じ `claude`・`gpt`・`muse` も受ける。API キーのプロバイダの id なら、そのキーのプール（認証情報の保管場所の `api-key/<id>` のアカウント）。`accounts/use` だけは設定のプロバイダの id が要る。`accounts/login/start` は API キーのプロバイダを `invalid_params` で断り、`accounts/add_key` を案内する。
- `accounts/add_key`：API キーのプロバイダ（設定にあること）に、要求の `key` をアカウントとして入れる。`alias` が今のアカウントの id か別名（大文字小文字を無視）を名指せば、そのキーを置き換える（`added: false`）。名指さなければ、その別名の新しいアカウントを足す（最初の 1 件は `default`）。`alias` を省くと `default` のキー（`inocode-server key set <id>` と同じ）。キーは返さず、ログにも出さない。キーが要求の中を通ってよい理由：この口は起動ごとの合言葉で認証した 127.0.0.1 の接続だけを受け、サンドボックスの命令からは合言葉が読めない（`server-auth.md`）。すでに `accounts/login/code` が同じ口でログインのコードを受けている。サーバーは要求の中身を記録しない（`server/src/lib.rs` の `settings_request`）。スラッシュコマンドは作っていない（会話の記録にキーが残るため）。
- `account`：アカウントの id。合わなければ別名（大文字小文字を無視）。無ければ `account_unknown`。別名は 1〜64 文字、制御文字なし、同じ種類のほかのアカウントの id・別名と重ならないこと（`invalid_params`）。
- `accounts/list`：`provider` を付けなければ、設定のログインのプロバイダごとのアカウントと、プロバイダの無い種類のアカウント（`provider: null`）。同じ種類のプロバイダが 2 つあれば、同じアカウントがそれぞれの行に出る。`active` は、そのプロバイダで新しい会話がいま使うアカウント（プールの選択。プロバイダの無い行は、使えるアカウントが 1 つのときのそれ）。`added` はミリ秒か `null`。`priority` は無ければ 0。秘密は返さない。
- `accounts/remove`：認証情報（OS の保管庫の項目も）を消し、そのアカウントを `active_account` にしていたプロバイダからその選択を外す。プールもそのアカウントについて覚えていること（会話の固定、クールダウン、使用率）を忘れる。
- `accounts/login/start` の `mode`：
  - Claude：`callback`（既定。ブラウザがこの機械の待ち受けポートに戻る）か `code`（Anthropic の画面に出る `code#state` を `accounts/login/code` で渡す）。CLI の `login claude` と `login claude --code` と同じ。
  - ChatGPT：`callback` だけ（CLI と同じ）。
  - DeepAI：`callback` だけ（CLI と同じ）。
  - Muse Code：`device` だけ。`url`（Meta の確認ページ）と `user_code` を返し、裏で承認を待って鍵を作る（CLI と同じ）。
  - ほかの組み合わせは `invalid_params`。
- 終わったログインは `complete_login` を通る：同じ人なら更新、違えば追加し、設定にその種類のプロバイダが無ければ足す（前と同じ）。CLI の `inocode-server login` も同じ関数を通り、足したか更新したかとアカウントの id を出す。
- 進行中のログインはサーバーのメモリーにだけ置く。待つのは最長 15 分（Muse の確認コードの既定の寿命 900 秒。ブラウザの戻りは前からの 300 秒で切れる）、結果は始めてから 25 分まで読める。それを過ぎるか、サーバーを起動し直すと `unknown_login`。ログインは人がその場で承認する短い作業で、待っているもの（この機械のポート、PKCE の値、確認コード）もプロセスと一緒に消えるので、保存しない（判断）。
- 本番の接続先は `LoginEndpoints::default()`。試験は `Core::with_login_endpoints` で模擬の窓口を渡す。トークンの更新の窓口も同じ値を使う（`Llm::with_token_url`）。

### スラッシュコマンド

- `/login`・`/logout`・`/accounts` を使えるようにした（`slash-commands.md` の表）。どれもターンの途中でも使える（`now`）。

### 試験

- 単体：
  - `oauth::tests::identity_needs_one_shared_field_and_no_disagreeing_one`（同じ人の判定。ワークスペース違い・同じワークスペースの別人）。
  - `oauth::tests::the_active_account_or_the_only_usable_one_is_chosen`（選び方と誤りのコード）。
  - `oauth::tests::sign_ins_add_or_update_accounts_and_their_info_persists`（最初は `default`、同じ人は更新で `needs_reauth` が外れる、番号を残す、別名、削除、知らないアカウントは誤り、消したアカウントの情報がファイルから消える）。
  - `oauth::tests::only_a_refusal_of_the_refresh_token_asks_for_a_new_sign_in`。
  - `secrets::tests::account_info_is_kept_beside_the_secrets_and_a_removed_account_leaves_no_entry`（まねの OS の保管庫。情報がファイルに残り、秘密はファイルに出ず、消したアカウントの項目が保管庫から消える）、`secrets::tests::account_info_round_trips_in_the_sealed_file`（パスフレーズのファイル）。
  - `accounts::tests`：id・別名での指定、別名の重なり、名前の解決。
- プロトコル（`server/tests/accounts_tcp.rs`、模擬の Claude の認可・トークン・メッセージの窓口、実 TCP）：
  - コードの方式でログイン → `default` が足される → 一覧（秘密が無い、`active`）→ ターンがそのトークンで送られる。同じ人でもう一度 → 更新（`added: false`、1 件のまま）。別の人 → 2 件目 → 一覧の `active` は 1 件（先に足した `default`）で、会話は使っていたアカウントのまま答える。別名、別名の重なりは誤り、別名で `accounts/use` → そのトークンで送られる。選んだアカウントを一時停止 → もう 1 つで答える。再開 → その会話はもう 1 つのまま、新しい会話は選んだアカウント。両方を一時停止 → `account_paused`。削除 → 秘密がファイルから消え、`active_account` も外れ、残った 1 件で送られる。知らないアカウント、API キーのプロバイダ、合わない方式は誤り。
  - メッセージの窓口が 401 を返し、更新も 400 で断られる → `needs_reauth` が付き、次のターンは `account_needs_reauth`。同じ人でログインし直すと外れる。
  - ブラウザの戻り（待ち受けポートへの GET）でログインが終わる。`callback` の方式にコードは渡せない。途中のログインを止めると `cancelled`、その後のコードも `cancelled` のまま。知らない `login_id` は `unknown_login`。
  - コマンド：`/login claude code`・`/login code`・`/login`（会話のプロバイダ）・`/login status`・`/login cancel`・`/accounts`・`/accounts rename`（空白を含む別名）・`use`・`pause`・`resume`・`list 〈プロバイダ〉`・`/logout`（使っているアカウントを消す）・消えたアカウントの `remove` は `account_unknown`。

### 実際の機械での確認

- 2026-11-25、この機械（Windows 11、認証情報は Windows の資格情報マネージャー）。試験 `accounts_tcp::live_the_machines_accounts_are_listed_and_used`（手で動かす）を `INOCODE_TEST_PROVIDER=claude-oauth`、`INOCODE_TEST_MODEL=claude-haiku-5-5` で、`HTTPS_PROXY`・`HTTP_PROXY` を外して実行。設定は一時フォルダー（プロバイダ `live`、種類 `claude-oauth`）、認証情報はこの機械のもの。
- `accounts/list`：前からある 4 件がそれぞれ `default` として出た。`claude-oauth`（プロバイダ `live`、`active: true`、メールアドレスあり）、`gpt-oauth`・`muse-code`・`deepai`（設定にプロバイダが無いので `provider: null`、どれも `active: true`）。`gpt-oauth` と `deepai` はメールアドレスが記録されていない（前のログインが記録していなかったため）。どれも一時停止・ログインし直しなし、足した時刻は不明（`null`）。
- 同じ会話で 1 ターン（「hi」）：`completed`、日本語のあいさつが返った。全体で約 2.5 秒。
- 実行後の `~/.inocode/auth.json`：形式 `inocode-credentials` 版 2、`backend: keyring`、4 件の目次のまま。`account_info` は書かれていない（何も変えていないため）。本物のアカウントの名前の変更・削除・ログインはしていない。

### 確かめていないこと

- 本物の窓口でのログイン（ブラウザの承認が要るので、プロトコルからは動かしていない）。ChatGPT・Muse・DeepAI のプロトコルからのログインは模擬の試験も無い（ログインの各関数は `oauth_flows.rs` に前からの試験がある）。
- ChatGPT の `id_token` の `email` を読む部分は、本物の `id_token` に `email` があるか確かめていない。DeepAI の `username` も本物の応答では見ていない。
- 401 の扱いは Claude でだけ試した。Muse・DeepAI で 401 のときにすぐ印を付ける動きは試していない。
- 2 つの inocode のプロセスが同時に同じアカウントの情報を書くときは、前からのファイルのロックで順番になるはずだが、試験していない。
- DeepAI の、料金のかかる要求を 1 つずつにする枠は、まだ認証情報の保管場所ごとで、アカウントごとではない。

## アカウントのプール（実装済み）

- **対象と範囲：** 設定のプロバイダ（ログインの種類 `claude-oauth`・`gpt-oauth`・`muse-code`・`deepai`、と API キーの種類 `openai`・`anthropic`・`responses`）が、送る要求ごとにどのアカウント（API キーの種類ではキー）を使うかを選ぶ。複数あるとき、使用率・失敗・上限による断りを見て移り、会話を同じアカウントに留める。opencodex のアカウントの選択と切り替えの部分と同じ範囲。opencodex のモデルの組み合わせ（combos）と、決まりによる経路の選択（policy routing）は範囲外（ユーザーの判断。下の表）。
- **関連ソースコード：** `core/src/pool.rs`（選び方・結果の記録・使用率・断りの分け方・接続先の台帳・状態の表示）、`core/src/store_pool.rs`（データベースへの保存）、`core/src/store.rs`（版 12）、`core/src/store_threads.rs`（分岐した会話の親の記録）、`core/src/llm.rs`（`start_routed`・`pick`・`send`・`answered`・`api_key_of`・`unreachable_code`・`read_usage`。要求ごとにプールに聞き、結果を返す）、`core/src/lib.rs`（`Core::route`、ターンを `pool::in_thread` で包む）、`core/src/accounts.rs`（`pool/*`・`accounts/priority`・`accounts/clear_cooldown`・`accounts/add_key`）、`core/src/settings.rs`（`PoolConfig`、`account_store`）、`core/src/secrets.rs`（`AccountInfo` の `priority`・`auto_switch_threshold`・`credits`）、`core/src/oauth.rs`（`Store::set_key`・`chatgpt_plan`）、`core/src/usage.rs`（`Credits`）、`core/src/commands.rs`（`/accounts` の `priority`・`clear-cooldown`・`strategy`・`status`、`/fork` が親を記録）、`server/src/lib.rs`、`server/src/main.rs`（`key set --account`）。
- **根拠の状態：** UNSUPPORTED_APPROVED。学術研究での裏付けは調べていない。ユーザーの指示（2026-11-25）「プールは、opencodex と同じだけ作る方向でよい」と、残りの仕組みについての指示（2026-11-26）「残りも今作る」による。数値は opencodex のソースの値（下の表）で、効果を測った研究ではない。
- **注意（ユーザーに伝えた）：** 複数の有料契約を束ねて上限を避ける使い方は、OpenAI・Anthropic の利用規約に触れるおそれがある。

### 何をするか

- 要求ごとに、認証情報の保管場所からそのプロバイダの種類のアカウントを読み（一時停止・ログインし直し・優先度・閾値を含む）、プールに選ばせる。プールはプロバイダごと（設定の id ごと）。同じ種類の 2 つのプロバイダは、アカウントの健康状態・使用率・クールダウンを共有し、置き方（順番・現在のアカウント）と会話の固定は別々に持つ。
- 使えるアカウントが 1 つなら、そのアカウント。複数あって `active_account` が無ければ、プールが選ぶ（`account_ambiguous` にならない）。
- **置き方（新しい会話）：** `pool.strategy`
  - `quota`（既定）：そのプロバイダの「現在のアカウント」を使い続け、使用率が閾値に達したら、より使用率の低いアカウントへ移る。現在のアカウントが無ければ、使用率の最も低いもの（同じなら先に足したもの）。
  - `round-robin`：重みの等しい滑らかな重み付き巡回。`sticky_limit` が 2 以上なら、同じアカウントに続けてその数の新しい会話を置く。閾値に達していないアカウントを先に使う（全部達していれば全部から）。
  - `fill-first`：現在のアカウントを閾値に達するまで使い、達したら id の順で次の、閾値に達していないアカウントへ。
  - `reset-first`：閾値に達していないアカウントのうち、5 時間か週の窓が最も早く戻るもの。同じなら使用率の低いもの。
- **優先度の段：** アカウントごとの `priority`（-100〜100、既定 0）。使えるアカウントのうち、閾値に達していないアカウントを含む最も高い段だけを候補にする。全部の段が閾値に達していれば全部が候補。`quota` では、上の段が閾値を下回ると、新しい会話はその段へ戻る。
- **手で選んだアカウント（`active_account`）：** 使えて閾値に達していないあいだは、`quota`・`fill-first` の現在のアカウントになる。段の上限にもなる（それより上の段は使わない）。`round-robin` では次の 1 つをそれにする。変えると、そのプロバイダの会話の固定をすべて外し、そのアカウントの失敗の続きと回避（下）を消す（上限による断りのクールダウンは残す）。使えないあいだはプールがほかへ移る。
- **閾値：** `pool.auto_switch_threshold`（既定 80%、0 は使用率では移らない）。アカウントごとの `auto_switch_threshold` があればそれ。
- **使用率の数え方：** 週と月の窓の高いほうと、まだ戻っていない 5 時間の窓の高いほう。週・月が分からなければ「分からない」（101 として扱い、閾値に達したことにはならず、最も空いているようにも見えない）。ただし 5 時間の窓が 100% で、戻る時刻が先か、戻る時刻が無く 5 分以内に見たものなら 100。
- **会話の固定：** 会話（スレッド）は最初に使ったアカウントに固定する（プロンプトのキャッシュはアカウントごとのため）。24 時間使わなければ外す。データベース（`pool_affinity`）に保存し、再起動の後も続く。同じ会話の補助役（審査・圧縮・探索・判定・同僚）の要求は、同じプロバイダならその固定を使う（ターンを `pool::in_thread` で包み、そのなかのすべての要求を会話のものとして扱う。スラッシュコマンドの実行も同じ）。
  - `quota` のときだけ、固定したアカウントを見直す：`cache_affinity`（既定 `true`）なら使い切った（100%）か使えなくなったときだけ、`false` なら閾値に達したとき。移り先は閾値に達しておらず、使用率がはっきり低いアカウントだけ。見直しは 1 分に 1 回まで（移ってよいときは毎回）。
  - 一時的な不調（下の回避・失敗の続き・失敗率）だけで使えないときは、固定を残したまま、その要求だけをほかへ送る（同じ回り道を覚えておく）。10 分を過ぎても戻らなければ、回り道のアカウントへ固定を移す（それも使えなければ外す）。
  - 上限による断りや再ログインの要る断りでは、そのアカウントへのすべての会話の固定を外す。
- **結果の扱い：**
  - 200 番台：成功。失敗率の窓に成功を足す。失敗が 2 回以上続いていたアカウントは、成功が 2 回続いて初めて失敗の数を 0 に戻す。
  - 429・402（下の「Anthropic の 429 の分け方」「モデルごとの上限」「API キーのプール」の例外を除く）：クールダウン。`Retry-After`（秒か日付、最長 24 時間）、無ければ断りの応答が示す窓の戻る時刻のうち最も早いもの（最長 15 分）、どちらも無ければ 60 秒。そのうえ、示された戻る時刻（最長 6 時間）まではほかのアカウントを選ぶ（回避）。クールダウンと回避はデータベース（`pool_cooldowns`）にも書き、再起動の後も続く。そのアカウントへの会話の固定を外し、現在のアカウントだったら次のものへ移す。答えがまだ始まっていないので、同じ要求を一度だけプールの選ぶ別のアカウントへ送り直す（別のアカウントが無ければ、その断りをそのまま返す）。
  - 401：Claude・ChatGPT は断られたトークンで更新を一度試し、更新も断られたらログインし直しの印を付ける。Muse・DeepAI はすぐ印を付ける。403 で本文が作業領域・権利の拒否（`codex_workspace_access_denied`・`workspace_access_denied`・`codex_entitlement_missing`・`entitlement_missing`）を名乗らないものは、ChatGPT・Muse・DeepAI ならすぐ印を付ける（Claude の 403 は下の「Anthropic の 429 の分け方」）。どちらも、そのアカウントの会話の固定を外す。要求そのものは失敗のまま。
  - 403 で拒否を名乗るもの：失敗の数を 1 足すだけ（印は付けない、固定も外さない）。
  - 500 番台・接続の失敗（下の「接続先の台帳」の、接続先に届かなかったものを除く）・応答の途中の途切れや無音（`provider_idle`）：失敗の数を足す（前の失敗から 5 分を過ぎていれば 1 から）。3 回続けば（`failover_threshold`、0 で切る）、そのアカウントを避け始め、避ける時間は 30 秒、2 分、10 分、30 分と延ばす。現在のアカウントだったら次のものへ移す。60 秒の失敗率の窓でも見る：20 件以上のうち 25% 以上が失敗なら、新しい会話を置かない（手で選んだアカウントは除く）。10% 以下が 30 秒続けば戻す。応答の途中のプロバイダの誤りの出来事（SSE の `error` など）は数えない。
  - そのほかの 400 番台・300 番台：アカウントの健康に数えない。
- **使用率の読み取り：** 応答（成功でも断りでも）の見出しを毎回読む。Codex の `x-codex-{primary,secondary,tertiary}-used-percent`・`-reset-at`・`-window-minutes`（primary の窓が 1 日未満なら 5 時間の窓、28 日以上なら月の窓）、Anthropic の `anthropic-ratelimit-unified-{5h,7d}-utilization`（0〜1 の割合）・`-reset`。加えて、使えるアカウントが 2 つ以上あり、使用率で選ぶとき（`quota`・`reset-first`、または閾値が 0 でないとき）だけ、10 分読んでいないアカウントの使用量 API（ChatGPT の `wham/usage`、Claude の `api/oauth/usage`、Muse の `muse-code/key`。`/usage` と同じもの）を裏で読む。DeepAI は読まない（割合の窓が無い）。
- **クールダウンを早く終える：** `Retry-After` 以外で決まったクールダウン中のアカウントは、5 分ごとに使用量 API を読み、使用率が 100% 未満ならクールダウンと回避を終える。手では `accounts/clear_cooldown`。
- **使えるアカウントが選べないとき：** 全部がクールダウン中なら、現在のアカウント（無ければクールダウンが最も早く終わるもの）へ送る（プロバイダがまた断るか、戻っていれば答える）。全部が一時停止かログインし直しなら、`account_paused`・`account_needs_reauth`・`account_unavailable`。
- **要求以外の読み取り：** モデルの一覧・`/usage`・DeepAI の審査などは、その会話の固定、無ければ新しい会話が使うアカウントで読む。

### opencodex の残りの仕組み

ユーザーの指示（2026-11-26）「残りも今作る」による。モデルの組み合わせ（combos）と、決まりによる経路の選択（policy routing）は、前の判断どおり範囲外。場所は `refs/opencodex/src` からの行番号（v2.100.0、コミット 1d10758f）。どれも学術研究の裏付けは無く、opencodex のソースに合わせた（UNSUPPORTED_APPROVED）。

#### モデルごとの上限

- 要求のモデルから「上限の組（quota group）」を決める（`pool::quota_scope`）：
  - ChatGPT：`gpt-reserve` は `reserve`、ほかのモデルはすべて `shared`。opencodex は「上限が別だと確かめた組だけを分け、ほかは共有」としている（`codex/routing/health-store.ts:103-118` の `NATIVE_MODEL_QUOTA_SCOPES`・`codexQuotaScopeForModel`、`catalog/native-models.ts:2`）。
  - Claude：Fable の家族（`claude-fable-5` と `claude-fable-5-…`）は `Fable`（`oauth/anthropic-model-family.ts:4-7`）。ほかのモデルに組は無い。
  - ほかの種類（Muse・DeepAI・API キー）には組は無い。
- ChatGPT の 429・402 で、`Retry-After` が無く断りの応答の戻る時刻からクールダウンを決めたもの（`reset`）は、その組だけを冷ます（`codex/routing.ts:1452-1494`）。組が `shared` なら、前と同じくそのアカウントの会話の固定を外し、現在のアカウントを移す（`:1480`）。`reserve` なら固定も現在のアカウントも動かさない（`:1477-1479` の説明、`health-store.ts:121` の `isIndependentCodexQuotaScope`）。`Retry-After` か既定の 60 秒のクールダウンは、前と同じくアカウント全体（`:1452-1455` の説明）。
- 組のクールダウンと回避は、そのアカウントの全体のものと合わせて見る：全体のものが続いていれば全体、無ければ要求の組のもの（`health-store.ts:166` の `codexQuotaAvoidUntil`、`:230` の `getCodexQuotaHealthSnapshot`）。
- Anthropic の Fable の家族：断り（下の「Anthropic の 429 の分け方」の家族の上限）は、その家族の要求だけを、窓の戻る時刻か 30 分の早いほうまで、そのアカウントから外す（`oauth/anthropic-model-quota.ts:9`・`:58`・`:99`）。そのアカウントから Fable の答えが返れば外れる（`:75`、`providers/quota/account-cache.ts:320`）。
- Fable の窓の使用率：応答の見出し `anthropic-ratelimit-unified-7d_oi-utilization`（0〜1.01 の割合、100% で頭打ち）、429 で `7d_oi-status: rejected` なら 100%（`providers/quota/anthropic-family-headers.ts:3-14`）。Fable の要求の使用率の数え方には、その窓も入れる（`anthropic-model-quota.ts:13-17` の `anthropicModelPercents`）。窓が 100% のアカウントは、Fable の要求では後回し（ほかに無ければ使う。`oauth/anthropic-routing.ts:805-806`）。
- 会話の固定：Fable の家族の断りだけで使えないときは、固定を残してその要求だけほかへ送る（一時的な不調の回り道と同じ扱い。`anthropic-routing.ts:1316-1324`）。
- 保存：組のクールダウンはデータベースの `pool_scoped_cooldowns`（下の「データベース」）。

#### プランとクレジット

- **プランによる除外：** `pool.excluded_plans`（プランの名前の一覧、大文字小文字を無視）。そのプランのアカウントは、プールが自分では選ばない。`active_account` で手で選んだアカウントは使う（`codex/routing/selection.ts:50` の `excludedCodexPoolPlanKeys`、`:76` の `isCodexAccountPlanExcluded`）。固定した会話もそのアカウントを離れる（`:120` の `isCodexAccountSelectable`、「固定した会話は一覧を通らないので、使えるかの判定にも入れる」という opencodex の説明 `:66-71`）。全部が除外なら送らない（`account_unavailable`。`:110` の `isCodexAccountRotationExcluded`）。
  - プランの名前は、使用量 API の答え（ChatGPT の `plan_type`、Muse の `subs_tier_name`）、無ければ ChatGPT のトークンの `chatgpt_plan_type`（`codex/plan.ts:23-38` の `extractChatgptPlanType`。opencodex も使用量 API の値を優先する `codex/plan-from-token.ts:33`）。Claude はプランを読む口が無いので、除外は効かない。
- **クレジット：** ChatGPT のアカウントで、窓が 100% になっている（5 時間の窓が今も満杯、または週・月の窓が 100% で戻る時刻が先）ときは、プールはそのアカウントを使わない（ChatGPT は満杯でもクレジットを使って答えるため。`codex/account-credit-use.ts:14-28`・`:56-95`・`:97-107`）。アカウントごとの `credits: true`（`accounts/priority` で書く）で、しかも 5 分以内に読んだクレジットが使える（`has_credits` で残高が 0 より多い、または `unlimited`、`overage_limit_reached` でない）ときだけ使い続ける（`codex/quota-types.ts:26`・`:38-49`）。全部がこの状態なら送らない。クレジットは使用量 API の `credits`（`codex/credits.ts:27-52`）と、応答の見出し `x-codex-credits-has-credits`・`-unlimited`・`-balance`（`server/safe-response-headers.ts:108-110` に並ぶ名前）から読む。
  - opencodex の設定の名前は `creditCodexAccountIds`（使ってよいアカウントの一覧）。inocode はアカウントの目次の `credits` に置いた（ほかのアカウントごとの値と同じ場所）。
- **モデルの権利の補正：** ChatGPT が 400 で本文の `detail`（または `error.message`）が「The '〈モデル〉' model is not supported when using Codex with a ChatGPT account.」とちょうど一致するとき（`combos/failover.ts:1389`・`:1403`）、そのアカウントとモデルの組を 6 時間覚え（`codex/observed-model-denials.ts:46`・`:119`）、そのモデルの要求ではほかのアカウントを先に使う。外して空になるときは外さない（`:22-30` の説明「並べ方の好みで、選べる・選べないの判定ではない」）。同じ要求を一度だけ別のアカウントへ送り直す（`server/responses/core-codex-account.ts:1122-1141`）。そのアカウントがそのモデルで答えれば消える（`observed-model-denials.ts:153`）。固定した会話は固定を残し、そのモデルの要求だけほかへ送る。
  - モデルの名簿（`/models` をアカウントごとに取って、権利の有無を知る仕組み、`codex/model-entitlements.ts`）は作っていない。

#### 空いた窓

- `pool.start_idle_windows`（既定 `false`、opencodex の `codexPool.startIdleWindows` も既定で無効）。新しい会話を、5 時間の窓がまだ始まっていないアカウントへ先に置き、その窓を走らせ始める（`codex/routing/idle-window.ts:22-58`）。
- 条件：手で選んだアカウントが無い、組が `reserve` でない（`:30-31`）。使えて閾値に達していないアカウントのうち（`:32-36`）、5 時間の窓が 0%、窓の長さが 5 時間と分かっている、5 分以内に読んだ、戻る時刻が読んだ時刻のちょうど 5 時間後（±1 分）で、まだ先（`:40-47`）。一度置いたら、戻る時刻か今から 5 時間の遅いほう＋1 分までは、そのアカウントの窓は始まったものとみなす（`:45-52`）。
- 置き方の順：会話の固定、分岐した会話の親、空いた窓、方式の順（`codex/routing.ts:1088-1110`）。
- 窓の長さは Codex の見出し（`x-codex-primary-window-minutes`）か使用量 API の `limit_window_seconds` から分かる。Anthropic の見出しには長さが無いので、Claude では使用量 API の答えに長さがあるときだけ効く。

#### 分岐した会話

- `/fork` で作った会話は、親の会話を覚える（データベースの `thread_lineage`）。その会話がそのプロバイダで初めて送るとき、親の会話が今使っているアカウント（回り道の保持中なら回り道の先）へ置く。親が使えなければ、同じ親から分かれたほかの会話（新しいものから 8 件まで）のアカウント（`codex/routing.ts:382-407`・`:415-443`、`codex/lineage.ts:69` の `CODEX_LINEAGE_MAX_SIBLINGS`）。使えるかの判定は固定と同じ（使える、上限の断りが残っていない、失敗が続いていない）。
- 一度でも固定を持った会話には使わない（`routing.ts:1083-1087` の説明）。
- 補助役（審査・圧縮・探索・判定・同僚）は前から同じ会話として送るので、同じアカウント。

#### 固定した会話の上の段への戻し

- `pool.priority_failback`（既定 `false`、opencodex の `codexAccountPriorityFailback` も既定で無効）。`quota` の方式で、固定したアカウントの閾値が 0 でないとき（`codex/account-priority.ts:10-14`）。
- 上の段が新しい会話を取り戻す条件（前からの「上の段が戻ったとき」）が成り立ち、上の段の候補のうち、閾値に達しておらず、上限の断りが残っておらず、失敗が続いておらず、使用率が 5 分以内に読んだもの（5 時間の窓も 5 分以内）で、分かっていて 100% 未満かつ閾値未満のものがあれば、そのうち最も空いたものへ会話の固定を移す（`codex/routing.ts:512`、`account-priority.ts:8` の 5 分）。手で選んだアカウントがあれば移らない（`pickPriorityPreemption` と同じ）。
- 1 分に 1 回の見直しより先に、毎回確かめる（`routing.ts:704-705`）。
- この設定が有効なとき、使用量 API を読む間隔を 10 分から 5 分にする（opencodex は戻しのために 5 分ごとに読み直す。`codex/auth-context.ts:1578-1583`、`codex/auth-api/pool-mode-gate.ts:332-337`）。

#### 接続先の台帳

- 名前が引けない（DNS）・接続を断られた・道が無い（ホスト・ネットワークに届かない）ことがはっきりした失敗は、接続先に要求が届いていないので、アカウント（キー）の失敗に数えない（`lib/upstream-reachability.ts:1-21`・`:29-38`・`:71-87`、`codex/routing/cooldown-math.ts:157`）。代わりに（プロバイダの id、接続先のホスト名）ごとの台帳に数える（`codex/routing.ts:1278-1284`、`codex/upstream-host-health.ts:53`）。
  - inocode の判定：`reqwest` の接続の誤りで、その元の `std::io::Error` の種類が `ConnectionRefused`・`HostUnreachable`・`NetworkUnreachable`・`NetworkDown`、または元の誤りの文が `dns error` で始まるもの（`hyper-util` が名前引きの失敗をこう名乗る。型が公開されていないので文で見る）。リセット・時間切れ・TLS の失敗は、接続先が要求を見た後かもしれないので、前と同じくアカウントの失敗。
- 台帳：続いた失敗の数、最後の時刻、最後の理由。前の失敗から 10 分を過ぎた失敗は 1 から数え直す。答え（どんな HTTP の状態でも）が来れば消える。128 件まで（`upstream-host-health.ts:10-13`・`:253`・`:326`）。
- 回路（circuit）：`pool.host_circuit_threshold`（0〜20、既定 0 で数えるだけ）。その数だけ続けて届かなければ、30 秒はそのプロバイダへ送らず `provider_unreachable` を返す。30 秒の後は 1 件だけ試しに送り（ほかは 1 秒後を案内）、試しも届かなければまた 30 秒（`upstream-host-health.ts:167-216`）。試しが何も知らせずに終わった（送る前に失敗した）ときは、30 秒たてば次の試しを許す（inocode の判断。opencodex は試しの持ち主を厳密に管理している）。
- `pool/status` の `host` に出す。

#### Anthropic の 429 の分け方

- Claude のログインの 429 を、見出しで分ける（`oauth/anthropic-rate-limit-policy.ts:23-32` の `classifyAnthropic429`）：
  - 5 時間か週（`5h`・`7d`）の窓が `rejected`、または全体（`anthropic-ratelimit-unified-status`）が `rejected`：アカウント全体の上限。前と同じクールダウンと送り直し（`oauth/anthropic-routing.ts:1217-1270`）。
  - Fable の窓（`7d_oi`）だけが `rejected`：Fable の家族の上限（上の「モデルごとの上限」）。Fable の要求なら別のアカウントへ一度送り直す（`oauth/anthropic-account-refusal.ts:162-165`）。
  - `Retry-After` があるか、全体が `allowed`：一時的な速度制限。上限の断りではない（opencodex の説明「Retry-After はそのアカウントの一時停止で、上限を使い切った証拠ではない」`anthropic-rate-limit-policy.ts:28`）。そのアカウントを `Retry-After`（無ければ 0.1 秒）の間だけ新しい要求に使わず（`pauseAnthropicRateAdmission`、`anthropic-account-refusal.ts:167-170`）、会話の固定は残す。待ちが 1 秒以内なら同じアカウントで一度待って送り直し、それでも断られたら、または待ちが長ければ、別のアカウントへ一度送り直す（`:171-196`、`ANTHROPIC_SHORT_RETRY_MS` 100・`ANTHROPIC_MAX_INLINE_THROTTLE_MS` 1000 は `anthropic-rate-limit-policy.ts:7-8`）。
  - どれでもない：何の窓かわからない。アカウントの状態は変えず、使えるアカウントが 2 つ以上なら 0.1 秒後に同じアカウントで一度だけ送り直す（`anthropic-account-refusal.ts:166-190`）。
- Claude の 403：本文がアカウント自体を断るもの（`error.type` が `permission_error` で「Your account does not have access to Claude Code」「Your (Claude) subscription has expired / is expired / is inactive」「Your account does not have an active subscription」「(Your|This) organization has disabled Claude subscription access for Claude Code」、`billing_error` か `permission_error` で「Your credit balance is too low to access the Anthropic API」、または `oauth_org_not_allowed`。`error.code` があるなら `subscription_required`・`insufficient_quota`・`permission_denied`・`oauth_org_not_allowed` のどれか。`anthropic-account-refusal.ts:67-90`）は、`Retry-After` が無ければ 10 分のクールダウン（`anthropic-routing.ts:60`・`:1250`）で、ほかのアカウントへ一度送り直す。ログインし直しの印は付けない（`:1213` の説明「プランの更新にログインは要らない」）。そのほかの Claude の 403 はアカウントの健康に数えず、印も付けない（前は印を付けていた）。
  - 文の一致は大文字小文字を無視し、終わりの `.`・`!` を除いて比べる（opencodex の正規表現は終わりの 1 文字だけを許す。inocode のほうが少し緩い）。
- Claude の 401 は前のまま（断られたトークンで更新を一度試す）。opencodex は「OAuth access token has been revoked.」の 401 だけに印を付ける（`anthropic-account-refusal.ts:51-65`・`:135-155`）が、inocode は前からの、更新も断られたら印を付ける扱いを残した。

#### API キーのプール

- 登録：API キーの種類のプロバイダ `<id>` のキーは、認証情報の保管場所の `api-key/<id>` のアカウントとして持つ（ログインのアカウントと同じ形。別名・一時停止・優先度・閾値・削除・`active_account` も同じ）。
  - `inocode-server key set <id> [--account <別名>]`：`--account` が無ければ `default` のキー（前と同じ）。あれば、その id か別名（大文字小文字を無視）のアカウントのキーを置き換え、無ければその別名で新しいアカウントを足す。キーは表示しない（入力も画面に出さない）。
  - プロトコル `accounts/add_key {provider, key, alias?}`（上の「アカウントの登録」の「プロトコル」）。`accounts/list`・`rename`・`pause`・`priority`・`clear_cooldown`・`remove`・`use` は、API キーのプロバイダの id でそのキーを扱う。
  - キーの無いアカウント（前の版でプロバイダを消したときに空にしたもの）はプールに入れない。
- 選び方：ログインのアカウントと同じプール（方式・段・閾値・会話の固定・失敗の続き・回り道・接続先の台帳）。`pool` の設定も書ける。
  - opencodex との違い：opencodex の `apiKeyPoolStrategy`（`round-robin`・`fill-first`・`quota`）は、今使っているキーがクールダウン中のときだけ別のキーを選ぶ（`providers/key-failover.ts:490-548` の `selectProactiveApiKey`。`quota` は使用率の多い残り、無ければ並びの順）。inocode は新しい会話ごとに方式で選ぶ（ユーザーの指示「同じ方式で選ぶ」による）。キーの使用率は読まない（読む口が無い）ので、`quota` の方式では、断られるまで今のキーを使い続ける。これは opencodex の既定の動き（健全なキーを使い続ける）と同じになる。
- 断りの扱い（`providers/key-failover.ts`）：
  - 429・402：`Retry-After`（最長 10 分、`:40`・`:364-378`）、無ければ 60 秒（`:39`）のクールダウン。断りの窓の戻る時刻は使わない（キーには窓が無い）。同じ要求を一度だけ別のキーへ送り直す（`:852-950` の `rotateKeyAfterFailure`）。
  - 401：そのキーを 10 分休ませ（`:930-944`「401 は鍵そのものへの判定なので上限まで休ませる」）、同じ要求を一度だけ別のキーへ送り直す。
  - 403：キーの健康に数えない。
  - opencodex の、429 の本文に書かれた戻る時刻を読む部分（`:107` の `readQuotaResetAt`、`:247` の `parseQuotaResetAt`）と、Anthropic の請求の断りの 1 時間（`:50`）は作っていない。

#### コネクタ

- コネクタ（`/connectors`、claude.ai と ChatGPT の MCP コネクタ）は前のまま、プールを使わない。コネクタの MCP の接続は長く続き、その途中でアカウントが替わると接続先の側の状態が合わなくなること、コネクタはプール（`Core` が持つ）に届かない場所（`core/src/mcp.rs` の接続の管理）でアカウントの見出しを作ることから、簡単ではないと判断した（推測を含む。試していない）。

### opencodex との対応

| opencodex の仕組み | inocode |
|---|---|
| 方式 quota・round-robin（滑らかな重み付き巡回、sticky）・fill-first・reset-first | 作った |
| 優先度の段（`selectPriorityTier`）、手で選んだアカウントが段の上限 | 作った |
| 自動切り替えの閾値（全体 80%、アカウントごとの上書き、0 で切る） | 作った |
| 上の段が戻ったときに新しい会話を戻す（`pickPriorityPreemption`） | 作った |
| 会話の固定（24 時間、SQLite に保存）、quota だけの見直し、`cacheAffinity` | 作った |
| 一時的な不調での回り道（固定を残す、10 分の保持） | 作った（回り道の行き先が無いとき、opencodex は試しの 1 件か断りを返すが、inocode は固定したアカウントへ送る） |
| 429・402 のクールダウン（`Retry-After`、戻る時刻、既定）と回避、同じ要求の送り直し 1 回 | 作った |
| クールダウンを試しの 1 件で早く終える（probe lease） | 使用量 API の読み取りで代えた（本物の要求は送らない） |
| 401・403（資格）で再ログインの印、403 の拒否は印なし | 作った（401 は前からの「更新を一度試す」を残した） |
| 5xx の失敗の続き、3 回で切り替え、30 秒〜30 分の回避、60 秒の失敗率 | 作った |
| 応答の見出しと使用量 API からの使用率、使用率の数え方 | 作った（モデルの家族の窓とクレジット・プランは下の行） |
| 手で選んだときの会話の固定の解除と回避の取り消し | 作った |
| 管理：方式・sticky・閾値・失敗の閾値・優先度・クールダウン解除・状態 | プロトコルとスラッシュコマンドで作った（下） |
| モデルごとの上限（モデル別のクォータ枠） | 作った：ChatGPT の `gpt-reserve` と残り、Anthropic の Fable の家族（下の「モデルごとの上限」）。モデルの権利の名簿（`/models` の取得）は作っていない。権利の補正は断りの記録で作った（次の行） |
| クレジットでの続行、プランによる除外、モデルの権利の補正（断られたモデルの記録） | 作った（下の「プランとクレジット」）。`use-remaining`（手で選んだアカウントを使い切るまで使う一回きりの指定）は作っていない |
| 空いた 5 時間の窓を始めに行く置き方（idle window、opencodex でも既定で無効） | 作った（下の「空いた窓」） |
| 子の会話を親の会話のアカウントへ置く（lineage） | 作った：`/fork` で分けた会話（下の「分岐した会話」）。補助役は同じ会話なので前から同じアカウント |
| 優先度の戻しを固定した会話にも使う（priority failback、opencodex でも既定で無効） | 作った（下の「固定した会話の上の段への戻し」） |
| 接続先の到達できなさの台帳（host ledger）と、任意の回路（circuit） | 作った（下の「接続先の台帳」） |
| Anthropic の 429 の細かい分け方（一時的な速度制限・家族ごとの上限）と 403 の分け方 | 作った（下の「Anthropic の 429 の分け方」） |
| 手で選んだアカウントを、使い切ったときに設定から外す | しない（inocode の `active_account` は設定に残り、また使えるようになれば戻る） |
| モデルの組み合わせ（combos）、決まりによる経路の選択（policy routing） | 範囲外 |
| API キーの複数のキーのプール（`apiKeyPool`・`apiKeyPoolStrategy`） | 作った（下の「API キーのプール」。選び方は opencodex より広い） |
| コネクタのアカウントの選択 | プールを使わない（下の「コネクタ」） |

### 数値と出典（opencodex、`refs/opencodex/src`）

| 値 | inocode | opencodex の場所 |
|---|---|---|
| 方式の既定 `quota`、sticky 既定 1・上限 100 | `pool::Rules::default`、`MAX_STICKY_LIMIT` | `oauth/pool-kernel.ts:26-29` |
| 優先度 0、-100〜100 | `MIN_PRIORITY`・`MAX_PRIORITY` | `oauth/pool-kernel.ts:33-35`、段は `:105`、巡回は `:152`・`:232` |
| 閾値 80% | `DEFAULT_AUTO_SWITCH_THRESHOLD` | `codex/account-auto-switch.ts:6` |
| 失敗の閾値 3（0〜20） | `DEFAULT_FAILOVER_THRESHOLD`・`MAX_FAILOVER_THRESHOLD` | `codex/routing/selection.ts:855`（`?? 3`）、`codex/auth-api/routes.ts:412` |
| 既定のクールダウン 60 秒、`Retry-After` 最長 24 時間 | `DEFAULT_QUOTA_COOLDOWN`・`MAX_QUOTA_COOLDOWN` | `codex/routing/cooldown-math.ts:13-14`、`:184` |
| 戻る時刻からのクールダウン最長 15 分 | `MAX_RESET_COOLDOWN` | `codex/routing/cooldown-math.ts:21` |
| 回避の最長 6 時間 | `MAX_QUOTA_AVOID` | `codex/routing/cooldown-math.ts:27`、`:242` |
| 試しの間隔 5 分 | `PROBE_INTERVAL` | `codex/routing/cooldown-math.ts:29`、`codex/routing/probe-lease.ts:63` |
| 失敗の続きが切れる間隔 5 分 | `STREAK_GAP` | `codex/routing/cooldown-math.ts:30`、`codex/routing/selection.ts:855` |
| 回避 30 秒・2 分・10 分・30 分 | `SOFT_AVOID_STEPS` | `codex/routing/cooldown-math.ts:33`、`codex/routing.ts:1567` |
| 失敗率の窓 60 秒・20 件・25%・10%・30 秒 | `FAILURE_WINDOW*` | `codex/routing/failure-window.ts:14-18` |
| 成功 2 回で失敗を消す | `Inner::record` | `codex/routing/failure-window.ts:174` |
| 会話の固定 24 時間・2048 件、見直し 1 分、回り道の保持 10 分 | `AFFINITY_IDLE`・`MAX_BINDINGS`・`AFFINITY_REEVAL`・`TRANSIENT_HOLD` | `codex/routing/thread-affinity.ts:161-183`、`codex/routing/thread-affinity-store.ts:86` |
| 見直しの決まり | `Inner::reevaluate` | `codex/routing.ts:660`・`:690`、`codex/routing/cache-affinity.ts:57` |
| 使用率の数え方、分からない 101、使い切り 100、5 時間の窓の鮮度 5 分 | `Quota::score`・`UNKNOWN_SCORE`・`EXHAUSTED`・`TERMINAL_SHORT_FRESHNESS` | `codex/routing/cooldown-math.ts:109`、`codex/quota.ts:99`、`codex/quota-types.ts:13`・`:23` |
| 窓の長さの境 1 日・28 日 | `WEEKLY_MIN_SECONDS`・`MONTHLY_MIN_SECONDS` | `codex/quota.ts:55`・`:68`、見出しは `:679` |
| Anthropic の見出し（割合、秒） | `anthropic_headers` | `providers/quota/account-cache.ts:278`・`:296`、断りの窓は `oauth/anthropic-routing.ts:157`・`:164` |
| 使用量 API を読む間隔 10 分 | `USAGE_REFRESH` | `providers/quota-wire.ts:22` |
| 結果の分け方 | `classify` | `codex/routing/cooldown-math.ts:153`、記録は `codex/routing.ts:1272`（拒否 `:1389`、資格 `:1401`、上限 `:1450`、一時的 `:1548`） |
| 拒否の名前 | `names_denial` | `codex/quota-rejection.ts:81`・`:86` |
| 手で選んだとき | `Inner::sync_pin` | `codex/routing/active-account.ts:34` |
| クールダウンの解除 | `Pool::clear_cooldown` | `codex/routing/health-store.ts:279`、`codex/auth-api/routes.ts:282` |
| 上限の組（`gpt-reserve` は `reserve`、ほかは `shared`、Fable の家族） | `quota_scope`・`anthropic_family` | `codex/routing/health-store.ts:108-118`、`oauth/anthropic-model-family.ts:4` |
| 家族の断りの保持 30 分 | `FAMILY_REFUSAL_MAX_AGE` | `oauth/anthropic-model-quota.ts:9`・`:99` |
| Anthropic の短い送り直し 0.1 秒、同じアカウントで待つのは 1 秒まで | `SHORT_RETRY`・`MAX_INLINE_THROTTLE` | `oauth/anthropic-rate-limit-policy.ts:7-8` |
| Anthropic の 403（アカウントの断り）のクールダウン 10 分 | `ACCOUNT_REFUSAL_COOLDOWN` | `oauth/anthropic-routing.ts:60`・`:1250` |
| 断られたモデルの記録 6 時間 | `MODEL_DENIAL` | `codex/observed-model-denials.ts:46` |
| クレジットの鮮度 5 分 | `CREDITS_FRESHNESS` | `codex/quota-types.ts:26` |
| 空いた窓：5 時間・±1 分・5 分以内に読んだもの | `IDLE_WINDOW_SECONDS`・`IDLE_WINDOW_TOLERANCE`・`IDLE_WINDOW_FRESHNESS` | `codex/routing/idle-window.ts:10-12` |
| 兄弟の会話は 8 件まで | `MAX_SIBLINGS` | `codex/lineage.ts:69` |
| 上の段への戻しの鮮度と読み直し 5 分 | `FAILBACK_REFRESH` | `codex/account-priority.ts:8` |
| 接続先の台帳：10 分で数え直し、回路は 30 秒、閾値 0〜20、128 件 | `HOST_FAILURE_WINDOW`・`HOST_CIRCUIT_COOLDOWN`・`MAX_HOST_CIRCUIT_THRESHOLD`・`MAX_HOSTS` | `codex/upstream-host-health.ts:10-13` |
| API キー：`Retry-After` は 10 分まで、無ければ 60 秒、401 は 10 分 | `MAX_KEY_COOLDOWN`・`DEFAULT_QUOTA_COOLDOWN` | `providers/key-failover.ts:39-40`・`:930-944` |

### 設定

```json
{
  "providers": {
    "claude": {
      "kind": "claude-oauth",
      "active_account": "ab12cd34",
      "pool": {"strategy": "quota", "sticky_limit": 1, "auto_switch_threshold": 80,
               "failover_threshold": 3, "cache_affinity": true,
               "excluded_plans": ["free"], "start_idle_windows": false,
               "priority_failback": false, "host_circuit_threshold": 0}
    },
    "keys": {"kind": "anthropic", "active_account": "default", "pool": {"strategy": "round-robin"}}
  }
}
```

- `pool` の各項目は省ける（省くと上の値）。`strategy` は `quota`・`round-robin`・`fill-first`・`reset-first`、`sticky_limit` は 1〜100、`auto_switch_threshold` は 0〜100、`failover_threshold` は 0〜20、`host_circuit_threshold` は 0〜20、`excluded_plans` は空でない名前の一覧。違えば設定の誤り。API キーの種類にも `pool` と `active_account` を書ける（キーのプール）。
- アカウントごとの値は、認証情報の保管場所のファイルの `account_info`（秘密でない目次）の `priority`（0 は書かない）と `auto_switch_threshold` と `credits`（`true` のときだけ書く）。これらを書いたファイルは、これを知らない前の版の inocode では読めない（`account_info` は知らない項目を断る）。

### プロトコル

| メソッド | params | result |
|---|---|---|
| `pool/get` | `{provider}` | `{provider, kind, pool（書いたもの、無ければ null）, effective: {strategy, sticky_limit, auto_switch_threshold, failover_threshold, cache_affinity, excluded_plans, start_idle_windows, priority_failback, host_circuit_threshold}, active_account}` |
| `pool/set` | `{provider, strategy?, sticky_limit?, auto_switch_threshold?, failover_threshold?, cache_affinity?, excluded_plans?, start_idle_windows?, priority_failback?, host_circuit_threshold?}`（`null` で既定に戻す） | `pool/get` と同じ |
| `accounts/priority` | `{provider, account, priority?, auto_switch_threshold?, credits?}`（どれか 1 つは要る。`null` で既定に戻す） | `{account: {…, plan, priority, auto_switch_threshold, credits, active}}` |
| `accounts/clear_cooldown` | `{provider, account}` | `{account, cleared}`（クールダウンか回避が続いていたら `true`。上限の組のものも消す） |
| `pool/reset_affinity` | `{provider?, thread_id?}` | `{cleared}`（外した会話の固定の数） |
| `pool/status` | `{provider?}` | `{providers: [{provider, kind, model, pool（効いている値）, active_account, current, host, accounts: [{id, alias, paused, needs_reauth, priority, auto_switch_threshold, credits, plan, threshold, score, usage, cooldown_until, cooldown_source, quota_avoid_until, soft_avoid_until, rate_pause_until, scoped_cooldowns, denied_models, reason, consecutive_failures, degraded, bound_threads, last_status, last_error, current}]}]}` |

- `provider` は設定のプロバイダの id（ログインの種類も API キーの種類も）。種類の名前は `invalid_params`。`account` は id か別名。`pool/status` は `provider` を省くと設定のすべてのプロバイダ（API キーの種類も）。`kind` は認証情報の保管場所の名前（API キーの種類は `api-key/<id>`）。
- 時刻はミリ秒。`cooldown_until`・`quota_avoid_until`・`soft_avoid_until` は過ぎていれば `null`。`score` は使用率の数え方の値で、分からなければ `null`。`usage` は `{short_percent, short_resets_at, weekly_percent, weekly_resets_at, monthly_percent, monthly_resets_at, families, plan, updated}` か `null`（`families` はモデルの家族の窓、例 `{"Fable": 100}`）。`reason` は使わない理由（`paused`・`needs_reauth`・`plan_excluded`・`usage_limit`（クレジットを守る）・`cooldown`・`quota_avoided`・`rate_paused`・`transient`・`degraded`）か `null`。状態はプロバイダの既定のモデル（`model`）の要求から見たもので、ほかのモデルの上限の組のクールダウンは `reason` に出ず、`scoped_cooldowns`（組の名前 → `{cooldown_until, cooldown_source, quota_avoid_until, status}`）に出る。`denied_models` はそのアカウントが断ったモデル（6 時間）。`host` は接続先の台帳（`{host, consecutive_failures, last_failure, last_code, cooldown_until}`、届かなかったことが無ければ `null`）。`current` は新しい会話がいま使うアカウント。`bound_threads` はそのプロバイダでそのアカウントに固定した会話の数。`last_error` は最後の失敗の文（プロバイダの返した本文の先頭 300 文字まで。認証情報は入らない）。
- `providers/update` の `pool` でも変えられる（同じ確かめ）。

### データベース（版 12）

- `pool_affinity(session_id, provider, kind, account, bound, used)`：会話の固定。キーは（会話、プロバイダ）。読み込みは最初にプールを使うとき 1 回、24 時間以内に使ったものだけ。使った時刻の書き込みは 1 分に 1 回まで。
- `pool_cooldowns(kind, account, until, avoid_until, since, source, status)`：上限による断りのクールダウンと回避。読み込みはまだ続いているものだけ。
- `pool_scoped_cooldowns(kind, account, scope, until, avoid_until, since, source, status)`：上限の組（`reserve`・`shared`・`Fable`）のクールダウンと回避。キーは（種類、アカウント、組）。アカウントを消すかクールダウンを手で解くと、組のものも消える。
- `thread_lineage(session_id, parent)`：`/fork` で作った会話と、その元の会話。会話を消すとその行も消える。読み込みは最初にプールを使うとき 1 回。
- 前の版のデータベースはそのまま上げる（`store::migration` の試験で版 12 まで上がる）。

### スラッシュコマンド

- `/accounts priority 〈プロバイダ〉 〈アカウント〉 〈数〉`、`/accounts clear-cooldown 〈プロバイダ〉 〈アカウント〉`、`/accounts strategy 〈プロバイダ〉 〈方式〉`、`/accounts status [プロバイダ]`（`slash-commands.md`）。

### 試験

- 単体（`core/src/pool.rs`、模擬の時刻）：
  - 方式ごと：`quota` は最も空いたものに置き閾値まで留まる、分からない使用率では移らない・閾値 0 は切る・アカウントごとの閾値、`round-robin` の巡回と `sticky_limit`・閾値を超えたアカウントを飛ばす・のぞき見は何も変えない、`fill-first` の閾値までと id の順、`reset-first` の最も早く戻るもの。
  - 段：最も高い段で空きのあるもの、全段が閾値を超えたら全部、手で選んだアカウントが上限。上の段が戻れば新しい会話が戻る。手で選んだアカウント（使えるあいだ使う、一時停止ならほか、全部使えなければ無し）、手で選んだときの会話の固定の解除と回避の取り消し。
  - クールダウンの計算（`Retry-After` の秒と日付・24 時間の上限・戻る時刻の 15 分の上限・60 秒・回避の 6 時間）、429 で会話の固定が外れ現在のアカウントが移る、全部がクールダウン中なら早く終わるもの。
  - 失敗の続き（2 回では移らない、3 回で 30 秒、2 分、30 分、成功 2 回で消える、5 分で古くなる）、失敗率の窓（19 件では足りない、20 件中 5 件、30 秒続いて戻る）。
  - 会話の固定（閾値を超えても残る・使い切ったら空いたほうへ・行き先が無ければ残る・`cache_affinity: false` は閾値で移る・ほかの方式は見直さない）、24 時間で外れる、一時的な不調での回り道と 10 分での移し替え、401 と 403 の拒否の違い、使用量 API を読む条件と 5 分ごとの確かめ。
  - 見出しの読み取り（Codex の 5 時間・週・月、100 を超える値、Anthropic の割合と断りの窓）、5 時間の窓だけでは数えない、使用量 API の窓の分け方。
  - 残りの仕組み（2026-11-26）：Anthropic の 429 の分け方（5 時間・週・全体・Fable・`Retry-After`・`allowed`・わからない）と送り直しの決め方、速度制限で固定が残り回り道してから戻る、わからない 429 は何も変えない、Claude の 403 の分け方（ふつうの 403 は何もしない、アカウントを断る 403 は 10 分）と本文の判定。Fable の断りは Fable だけ（ほかのモデルは使える、30 分が上限、Fable の答えで外れる）。ChatGPT の戻る時刻からの断りは組だけ（`reserve` は `shared` に影響しない、`shared` は固定を外す、`Retry-After` は全体）。プランの除外（手で選べば使う、使用量 API のプランが勝つ、全部除外なら無し）。クレジット（満杯は使わない、`credits` と 5 分以内のクレジットがあれば使う、古いクレジットは数えない、ほかに無くても送らない、ChatGPT だけ）。断られたモデル（そのモデルだけ後回し、固定は残る、ほかに無ければ使う、答えで消える、6 時間）。空いた窓（空いた窓を先に、一度置いたら始まったとみなす、既定で無効、古い値は使わない、手で選べば使わない）。分岐した会話（親のアカウント、兄弟のアカウント、使えない親は使わない）。上の段への戻し（既定で無効、新しい使用率でだけ、手で選んだら戻らない）。接続先の台帳（10 分で数え直す、回路は閾値で 30 秒、試しは 1 件、試しの失敗でまた閉じる、届けば消える、アカウントの失敗に数えない）。API キーの冷まし方（`Retry-After` は 10 分まで、401 は 10 分、窓の時刻は使わない、使用量は読まない）。
- `core/src/store_pool.rs`：固定とクールダウンの保存・上書き・削除、組のクールダウンが全体と別に残り、組ごと・全部を消せる、分岐の親が残り会話を消すと消える。`core/src/secrets.rs`：`priority`・`auto_switch_threshold`・`credits` が保存の往復で残る。`core/src/oauth.rs`：キーを別名で足す・置き換える・全部消す、トークンのプラン。`core/src/usage.rs`：ChatGPT の使用量 API の `credits` を読み、`/usage` の表示には出さない。`core/src/accounts.rs`：API キーのプロバイダの名前がキーのプールになる。
- プロトコル（`server/tests/pool_tcp.rs`、模擬の Claude、実 TCP）：
  - 2 つのアカウント。1 つ目が 429（`Retry-After: 3600`、5 時間の窓 100%）を返す → 同じ要求が 2 つ目で答え、会話はそこに留まる（模擬が受けた順は `at-alice`、`at-bob`、次のターンは `at-bob` だけ）。`pool/status`：1 つ目は `cooldown_until` が約 1 時間後、`cooldown_source: retry-after`、`reason: cooldown`、`last_status: 429`、`score: 100`。2 つ目は見出しから `score: 40`（週 40%、5 時間 25%）、固定 1 件、`current`。`accounts/list` の `active` も 2 つ目。
  - コアを作り直す（同じデータベース）→ クールダウンの終わりの時刻も会話の固定も残り、その会話と新しい会話は 2 つ目へ送られ、1 つ目には送られない。`accounts/clear_cooldown` で解除（2 回目は `cleared: false`）、`pool/reset_affinity` で固定 2 件を外す。
  - 503 を返すアカウントで 3 回失敗 → `consecutive_failures: 3`、`reason: transient`、新しい会話はもう 1 つで答える。
  - `pool/get`・`pool/set`（書いた値・効いている値・`null` で戻す・設定ファイルに書かれる）と拒む場合（知らない方式、範囲外、知らない項目、項目なし、種類の名前）、`accounts/priority` と拒む場合、`/accounts status`・`strategy`・`priority`・`clear-cooldown`、知らない方式のコマンドは誤り。
  - Fable の断り：2 つのアカウント。1 つ目が Fable の要求にだけ 429（`7d_oi-status: rejected`、`7d_oi-utilization: 1.01`）を返す → Fable の会話は 2 つ目で答える。Sonnet の新しい会話は 1 つ目で答え、もう 1 つの Fable の会話は初めから 2 つ目へ（模擬が受けた順は `at-alice`、`at-bob`、`at-alice`、`at-bob`）。`pool/status`（Sonnet から見る）：1 つ目は `cooldown_until: null`、`scoped_cooldowns.Fable` に終わりの時刻、`usage.families.Fable: 100`。
  - プランの除外（模擬の ChatGPT、トークンの `chatgpt_plan_type` が `free` と `plus`）：`excluded_plans: ["Free"]` で、先に足した `free` のアカウントを飛ばして `plus` で答える。`pool/status` は `reason: plan_excluded`・`plan: free`、効いている値は小文字の `["free"]`。`accounts/use` で `free` を選ぶと、新しい会話はそれで答える。
  - 上の段への戻し：`accounts/use` で 2 つ目（段 0）に置いた会話を残し、選択を外して 1 つ目を段 10 にする。1 つ目は新しい会話を取り 95% と答え、その後 20% と答える。戻しが無効のうちは、2 つ目の会話は 2 つ目のまま。`pool/set` で `priority_failback: true` にすると、次のターンは 1 つ目で答え、`bound_threads` は 1 つ目 2 件・2 つ目 0 件。
  - 分岐した会話：`round-robin` で bob・dave・bob と置いた後、最初の会話を `/fork` した会話は、順番なら dave のところ bob で答える。dave の会話を `/fork` し、コアを作り直す（同じデータベース）と、その会話は新しい順番の最初（bob）ではなく dave で答える（分岐の親がデータベースから読まれる）。
  - 接続先の台帳：閉じたポートを接続先にした API キーのプロバイダ（`host_circuit_threshold: 2`）。2 回のターンは失敗し、`pool/status` の `host` は `consecutive_failures: 2`・`last_code: connection_refused`・`cooldown_until` あり、キーは `consecutive_failures: 0`・`reason: null`。3 回目は送らずに `provider_unreachable`。
  - API キーのプール：`accounts/add_key` で 2 つのキー（`first` は `default`、`second` は新しい id、`added: true`）。空のキー・キー無し・ログインのプロバイダは誤り、`alias` が `default` を名指すと置き換え（`added: false`）、大文字の別名でも同じアカウント。`accounts/list` は 2 件でキーを含まない。1 つ目が 429（`Retry-After: 120`）→ 同じ要求が 2 つ目で答える（模擬が受けた順は `key-one`、`key-two`）。`pool/status` は `kind: api-key/keys`、1 つ目は 2 分以内のクールダウン、キーを含まない。2 つ目を一時停止すると、新しい会話はクールダウン中の 1 つ目へ送られて断られ、失敗する。2 つ目を消すとファイルからそのキーが消える。
  - 前からの試験 `the_pool_is_set_by_method_and_by_command` は、効いている値の新しい項目と、拒む場合（`host_circuit_threshold: 21`、空のプランの名前）を足した。
- 前からの試験 `accounts_tcp.rs` を、プールの動きに合わせて直した（上の「アカウントの登録」の「試験」）。
- 実行したもの（2026-11-25、この機械）：`cargo test -j 1 --workspace`（すべて成功）、`cargo clippy -j 1 --workspace --all-targets -- -D warnings`（警告なし）、`cargo fmt --all`、`git diff --check`。残りの仕組みを足した後（2026-11-26、この機械）にも同じ 4 つを実行し、すべて成功・警告なし。

### 実際の機械での確認（2026-11-25、読むだけ）

- `pool_tcp::live_the_pool_shows_the_machines_accounts_and_answers`（手で動かす）を `INOCODE_TEST_PROVIDER=claude-oauth`、`INOCODE_TEST_MODEL=claude-haiku-5-5` で、`HTTPS_PROXY`・`HTTP_PROXY` を外して実行。設定とデータベースは一時フォルダー（プロバイダ `live`）、認証情報はこの機械のもの（Claude のアカウントは `default` の 1 つ）。
- ターンの前の `pool/status`：方式 `quota` などの既定の値、`current: default`、`score: null`（まだ何も読んでいない）、クールダウン・理由なし、固定 0 件。
- 1 ターン（「hi」）：`completed`、日本語のあいさつ。全体で約 1.9 秒。
- ターンの後の `pool/status`：本物の応答の見出しから `short_percent: 0`（5 時間の窓）、`weekly_percent: 26`、`score: 26`、それぞれの戻る時刻、固定 1 件。本物の Anthropic の応答が `anthropic-ratelimit-unified-5h-*`・`-7d-*` を付けることを確かめた。
- アカウントが 1 つなので、使用量 API の読み取りは起きない。ログイン・上限・アカウントの変更はしていない。

### 実際の機械での確認（2026-11-26、読むだけ）

- 残りの仕組みを足した後に、同じ `pool_tcp::live_the_pool_shows_the_machines_accounts_and_answers` を同じ条件（`INOCODE_TEST_PROVIDER=claude-oauth`、`INOCODE_TEST_MODEL=claude-haiku-5-5`、`HTTPS_PROXY`・`HTTP_PROXY` を外す）で 1 回実行。
- ターンの前の `pool/status`：効いている値に新しい項目（`excluded_plans: []`、`start_idle_windows: false`、`priority_failback: false`、`host_circuit_threshold: 0`）、`current: default`、`score: null`、理由なし、固定 0 件。
- 1 ターン（「hi」）：`completed`、日本語のあいさつ。全体で約 3.1 秒。
- ターンの後：`short_percent: 17`、`weekly_percent: 28`、`score: 28`、`families: {}`（Haiku の応答に `7d_oi` の見出しは無かった）、`plan: null`、固定 1 件。
- ログイン・上限・アカウントの変更はしていない。

### 確かめていないこと

- 残りの仕組み（2026-11-26）について：
  - 本物の Anthropic の 429 が、どの窓に `rejected` を付け、`Retry-After` や `unified-status: allowed` をいつ付けるか（opencodex のソースと模擬だけ）。Fable の `7d_oi` の窓は opencodex の記録（「Fable について試した値で確かめた」`anthropic-family-headers.ts:1`）だけで、本物では見ていない。Claude の 403 の本文の文も本物では見ていない。
  - ChatGPT の `gpt-reserve` の上限が本当に別か（opencodex の判断に従った）、ChatGPT の 400 の「model is not supported」の本文、使用量 API の `credits` の形と `x-codex-credits-*` 見出しが本物の応答に付くか（どれも opencodex のソースだけ）。トークンの `chatgpt_plan_type` も本物では読んでいない。
  - 名前引きの失敗を `hyper-util` の誤りの文（`dns error`）で見分けることは、閉じたポート（接続を断られる）の試験しかしていない。Linux・macOS での `std::io::ErrorKind` の対応も試していない。
  - 空いた窓は、Codex の見出しか使用量 API が窓の長さを言うときだけ効く。本物の ChatGPT の `x-codex-primary-window-minutes` が 300 になるかは見ていない。
  - API キーのプールは模擬の Anthropic の形でだけ試した（OpenAI・Responses の形は 1 キーの前からの試験だけ）。`inocode-server key set --account` は手で試していない（キーの入力を画面に出さない部分は前と同じ）。
  - 2 つのプロセスが同じキーを使うときのクールダウンは共有しない（ログインのアカウントと同じ）。
- 本物の 429・402 の形：Anthropic・ChatGPT・Muse・DeepAI が上限で断るときに `Retry-After` や使用率の見出しをどう付けるかは、本物では見ていない（模擬と opencodex のソースだけ）。本物の上限には当てていない（当てないことにした）。
- Codex の `x-codex-*` 見出しは単体の試験だけで、本物の ChatGPT の応答では見ていない。
- 使用量 API の Claude の `limits[]` の `kind` の名前は分からないので、名前に `five`・`5h`・`session`・`hour`・`window` があれば 5 時間の窓、`month` があれば月、ほかは週とみなした（推測）。
- 2 つの inocode のプロセスが同じアカウントを使うとき、プールの状態（健康・使用率）はプロセスごとで共有しない。データベースは 1 つのサーバーしか使わない（`inocode.db.lock`）。
- DeepAI の、料金のかかる要求を 1 つずつにする枠は、認証情報の保管場所ごとのまま（アカウントごとではない）。DeepAI の状態（チェックイン）はアカウントごとに分けた。
