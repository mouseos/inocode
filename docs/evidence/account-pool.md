# プロバイダ・モデル・アカウントの登録と、アカウントのプール

- **対象と範囲：**
  - プロバイダ・モデルの登録・一覧・変更・削除。
  - 会話ごとのプロバイダ・モデル・思考の深さの選択。
  - 1 つのプロバイダに複数のアカウント（OAuth・API キー）を持ち、送るたびに選ぶプール。
  - 認証情報の保存方法。
  - 環境変数での設定をやめる。
- **関連ソースコード：** 設定ファイルは `core/src/settings.rs`、クライアントの作り方と会話ごとの選択は `core/src/lib.rs`（`Core`）、`core/src/llm.rs`（`LlmConfig::for_provider`）、会話の設定の保存は `core/src/store.rs`（版 9）、プロトコルは `server/src/lib.rs`、コマンド（`key set`、`--port`、`--sandbox`）は `server/src/main.rs`、キーの入力は `core/src/secret.rs`。認証情報は `core/src/oauth.rs`（`~/.inocode/auth.json`）。アカウントの登録簿は `core/src/accounts.rs`・`core/src/oauth.rs`（`Store`）・`core/src/secrets.rs`（`AccountInfo`）。アカウントのプール（送るたびに選ぶ仕組み）は未実装。
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
- まだ：アカウントのプール（送るたびにアカウントを選ぶ・切り替える）、見積もり。

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
| `providers/remove` | `{id}` | `{removed}`。`default` か役が選んでいれば `provider_in_use`（「roles.review」など場所を書く）。会話が選んでいても `provider_in_use`（会話の番号を書く）。消したら、保管場所のそのプロバイダの API キーも消す（同じ名前で足し直したとき古い鍵を使わないため） |
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

- **対象と範囲：** ログインで使う 4 つの種類（`claude-oauth`・`gpt-oauth`・`muse-code`・`deepai`）に、それぞれ複数のアカウントを持たせる。プロトコルとスラッシュコマンドからのログイン、一覧、別名、一時停止、削除、使うアカウントの選択。API キーの種類（`openai`・`anthropic`・`responses`）は 1 プロバイダ 1 キーのまま。送るたびにアカウントを選び替えるプールは範囲外（まだ）。
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

- 設定のプロバイダに `active_account`（アカウントの id）を足した。`accounts/use` か `providers/update` で変える。API キーの種類には書けない。
- 要求のたびに認証情報の保管場所を読み、次で選ぶ（`oauth::choose_account`）：
  - `active_account` があれば、そのアカウント。無い・一時停止・ログインし直しが要るなら誤り（ほかへは移らない）。
  - 無ければ、一時停止でもログインし直しでもないアカウントが 1 つだけならそれ。
- 誤りのコードと案内（どれもターンは `failed`）：
  - アカウントが 1 つも無い：`provider_config`（`/login` を案内。前と同じコード）。
  - `account_ambiguous`：使えるアカウントが複数あり、どれも選ばれていない。`/accounts use` を案内し、候補の id と別名かメールアドレスを並べる。
  - `account_paused`：`/accounts resume` か `/accounts use` を案内。
  - `account_needs_reauth`：`/login 〈種類〉` か `/accounts use` を案内。
  - `account_unknown`：`active_account` のアカウントが無い。
  - `account_unavailable`：全部が一時停止かログインし直し。
- ログインは `active_account` を変えない（判断：ログインで知らないうちに使うアカウントが替わらないように）。そのため、アカウントが 1 つのプロバイダで 2 つ目にログインすると、選ぶまで `account_ambiguous` になる。CLI の `login` はそのとき「選んでください」と出す。
- `active_account` を変えると、設定の中身が変わるのでクライアントが作り直され、次の要求から効く。
- コネクタ（`/connectors`）は、その種類の止めていないプロバイダのうち、最初に `active_account` を持つものの選択を使う。無ければ同じ規則。

### ログインし直しが要る印

- トークンの更新で、トークンの窓口が HTTP 400・401・403 を返したとき（更新トークンが断られたとき）に付ける。つながらない・429・5xx では付けない（ログインし直しでは直らないため）。
- モデルの API が 401 を返したとき：Claude・ChatGPT は、断られたトークンでの更新を一度試す（それも断られれば上のとおり印が付く。更新できれば印は付けない）。Muse・DeepAI は更新できないので、すぐ付ける。その要求自体は失敗のまま（もう一度送ることはしない）。
- 同じ人でログインし直すと外れる。

### プロトコル

| メソッド | params | result |
|---|---|---|
| `accounts/list` | `{provider?}` | `{accounts: [{id, alias, provider, kind, paused, needs_reauth, added, email, account_id, active}]}` |
| `accounts/login/start` | `{provider, mode?}` | `{login_id, provider, kind, mode, url, user_code?, status, expires_in}` |
| `accounts/login/code` | `{login_id, code}` | `accounts/login/status` と同じ（交換が終わるまで最大 60 秒待つ） |
| `accounts/login/status` | `{login_id}` | `{login_id, …, status: waiting・done・failed・cancelled, account?, added?, error?}` |
| `accounts/login/cancel` | `{login_id}` | 同じ形（`status: cancelled`） |
| `accounts/rename` | `{provider, account, alias}`（`null` か空で外す） | `{account: {…}}` |
| `accounts/pause` | `{provider, account, paused}` | `{account: {…}}` |
| `accounts/remove` | `{provider, account}` | `{removed, kind}` |
| `accounts/use` | `{provider, account}`（`null` で選択を外す） | `{provider, active_account}` |

- `provider`：設定のプロバイダの id。無ければ種類の名前（`claude-oauth` など）、CLI と同じ `claude`・`gpt`・`muse` も受ける。API キーのプロバイダは `invalid_params`。`accounts/use` だけは設定のプロバイダの id が要る。
- `account`：アカウントの id。合わなければ別名（大文字小文字を無視）。無ければ `account_unknown`。別名は 1〜64 文字、制御文字なし、同じ種類のほかのアカウントの id・別名と重ならないこと（`invalid_params`）。
- `accounts/list`：`provider` を付けなければ、設定のログインのプロバイダごとのアカウントと、プロバイダの無い種類のアカウント（`provider: null`）。同じ種類のプロバイダが 2 つあれば、同じアカウントがそれぞれの行に出る。`active` は、そのプロバイダの要求がいま使うアカウント（上の選び方で決まるもの）。`added` はミリ秒か `null`。秘密は返さない。
- `accounts/remove`：認証情報（OS の保管庫の項目も）を消し、そのアカウントを `active_account` にしていたプロバイダからその選択を外す。
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
  - コードの方式でログイン → `default` が足される → 一覧（秘密が無い、`active`）→ ターンがそのトークンで送られる。同じ人でもう一度 → 更新（`added: false`、1 件のまま）。別の人 → 2 件目 → ターンは `account_ambiguous`。別名、別名の重なりは誤り、別名で `accounts/use` → そのトークンで送られる。一時停止 → `account_paused`、再開。削除 → 秘密がファイルから消え、`active_account` も外れ、残った 1 件で送られる。知らないアカウント、API キーのプロバイダ、合わない方式は誤り。
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
