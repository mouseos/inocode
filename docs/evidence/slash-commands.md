# スラッシュコマンド

- **対象と範囲：** inocode のエンジンが持つスラッシュコマンドの仕組みと、組み込みコマンドの一覧。UI（CLI・GUI）は、この一覧をエンジンから受け取って表示し、入力をエンジンに渡す。
- **関連ソースコード：** `core/src/commands.rs`（コマンドの一覧、名前の決まり、引数の読み取り、各コマンドの実行）、`core/src/store_threads.rs`（会話の一覧・名前・写し・削除、会話の使用量、メモの一覧）、`core/src/turn.rs`（`compact_now`、ターン中のモードの切り替え）、`core/src/lib.rs`（`Core::turn_running`、最後のターンが残した保留と質問、`command` の出来事）、`core/src/mcp/mod.rs`（`listed`、MCP の質問の中身を覚える）、`core/src/plugins.rs`（`commands/*.md` を読む）、`server/src/lib.rs`（`commands/list`・`command/run`・`thread/list`）、`server/tests/commands_tcp.rs`。`/login`・`/logout`・`/accounts` は `core/src/accounts.rs` を呼び、試験は `server/tests/accounts_tcp.rs`。
- **根拠の状態：** UNSUPPORTED_APPROVED。学術的な裏付けは調べていない（操作の入口の設計）。ユーザーの指示（2026-11-25）：
  - 「スラッシュコマンドからゴールを制御しないとだめでしょ」
  - 「スラッシュコマンドをもっと定義してからやるべき」
  - 「根拠だとかいろんな独自ツール向けのコマンド作らなくていいのか？cliだと参照方法がないぞ」（「根拠」は、エージェントが確かめて記録した手法メモのこと）
- **状態：** 実装済み（下の「実装の状態」）。2026-11-25 にユーザーが未決定の 3 点を決めた（下の「決めたこと」）。
- **手本：** Codex の `codex-rs/tui/src/slash_command.rs`（約 50 個のコマンド）。

## 決まり

1. コマンドの一覧はエンジンが返す。プロトコルの `commands/list` で受け取り、各コマンドは次を持つ：
   - 名前と別名
   - 説明（人の言語）
   - 引数の形：サブコマンド、選択肢、自由な文字、ファイルのパス
   - ターンの途中で使えるか
   - どこで定義されたか：組み込み・プラグイン・利用者
2. 入力された `/…` は、エンジンが読んで動かす（プロトコルの `command/run`）。動き方は 3 通り。
   - **設定を変える：** 結果をすぐ返す。例：`/model`、`/translate`。
   - **ターンを始める：** そのターンの出来事が流れてくる。例：`/goal 〈目標〉`、`/review`、`/init`、`/compact`。
   - **表示する：** 一覧や中身を返す。例：`/notes`、`/issues`。
3. 画面だけで済むもの（色、キーの割り当て、コピー、画面の消去）は UI のコマンドにして、エンジンには置かない。
4. プラグインのコマンドは `/プラグイン名:コマンド`。Claude Code の `commands/*.md` の形式で、本文を指示文として使い、`$ARGUMENTS` に引数を入れる。
5. 利用者のコマンドは `~/.inocode/commands/*.md`、プロジェクトのコマンドは `.inocode/commands/*.md`。形式はプラグインと同じ。
   - 組み込みの名前は、プラグインのコマンド・プラグインのスキル・利用者・プロジェクトのどれにも上書きさせない（ユーザー：「上書き禁止」）。他人のリポジトリの `.inocode/commands/goal.md` が `/goal` や `/approve` の中身を替える、といったことを防ぐため。
   - 組み込み以外は、定義元を付けた名前で呼ぶ：`/プラグイン名:名前`（プラグインのコマンドとスキル）、`/user:名前`、`/project:名前`。
   - 定義元を付けない短い名前で呼べるのは、組み込みに無く、組み込み以外でもその名前が 1 つしかないときだけ。ぶつかれば短い名前は使えず、`/help` に呼び分け方を出す。
6. 道具や記録を足すときは、それを見るコマンドも足す。CLI から参照する方法の無い機能を作らない。この文書の表と、エンジンが持つ道具の一覧を試験で照合する。
7. 環境変数で決めていたこと（プロバイダ・モデル・思考の深さ・補助役のモデル）は、すべてコマンドと設定ファイルから変えられるようにする（`account-pool.md`）。

## 組み込みコマンド（案）

### 会話

| コマンド | 動き |
|---|---|
| `/new` | 新しい会話を始める |
| `/resume [会話]` | 前の会話の一覧から選んで続ける |
| `/rename 〈名前〉` | 会話の名前を変える |
| `/fork` | 今の会話を写して分ける |
| `/delete` | 会話を消す（確認を出す） |
| `/compact` | 会話を要約して縮める |
| `/export [形式]` | 会話を書き出す |
| `/history 〈語〉` | 全会話から探し、当たった箇所を出す（`recall`・`search` と同じ記録） |

### 目標・計画・約束

| コマンド | 動き |
|---|---|
| `/goal 〈目標〉` | 目標を決め、達成まで自動で続ける |
| `/goal status` | 今の目標と進み、判定役の最後の判断 |
| `/goal stop` | 目標を止める |
| `/plan` | 今の計画（`plan` の道具の記録）を表示する |
| `/promise` | 約束の一覧を表示する。`/promise remove 〈番号〉`・`/promise clear` |
| `/issues` | 未完了の課題の一覧。`/issues done 〈番号〉`・`/issues drop 〈番号〉` |

### エージェントが記録したこと

| コマンド | 動き |
|---|---|
| `/notes` | 手法メモ（`method`・`lesson`）の一覧 |
| `/notes search 〈語〉` | 手法メモを探す |
| `/notes show 〈番号〉` | 中身、タグ、つながり、役に立ったかの評価 |
| `/notes edit 〈番号〉` | 直す |
| `/notes tag 〈番号〉 〈タグ…〉` | タグを付ける |
| `/notes vote 〈番号〉 up・down` | 役に立ったかを付ける |
| `/notes retire 〈番号〉` | 使わない印を付ける（消さずに残す） |
| `/rules` | プロジェクトのルールの一覧。`/rules import`・`/rules edit 〈番号〉`・`/rules off 〈番号〉` |
| `/colleague` | 同僚の助言のオンオフ、モデル、これまでの助言 |

### 承認・質問

| コマンド | 動き |
|---|---|
| `/mode [モード]` | 承認の 4 つのモード（いちいち承認・モデルに決めさせる・半自走・完全フルアクセス）を見る・変える |
| `/approve [番号]`・`/deny [番号]` | 承認待ちの呼び出しの一覧と、許可・拒否 |
| `/answer [番号]` | 答えを待っている質問（`ask`、MCP の質問）の一覧と回答 |
| `/sandbox` | サンドボックスの状態と、書き込める範囲 |

### モデル・アカウント

| コマンド | 動き |
|---|---|
| `/model [プロバイダ/モデル]` | この会話のモデルを見る・変える。モデルの設定（思考の深さの既定、送る文の翻訳、モデルの言語）もここから |
| `/effort [深さ]` | 思考の深さを変える |
| `/roles` | 補助役（レビュー・圧縮・探索・ゴール判定・同僚）のモデルを見る・変える |
| `/providers` | プロバイダの一覧・追加・変更・削除 |
| `/models` | 使えるモデルの一覧（価格・ベンチマーク・実測の速さ） |
| `/login [プロバイダ]`・`/logout [アカウント]` | ログイン・ログアウト |
| `/accounts` | アカウントの一覧・切り替え・一時停止・別名・削除・優先度、プールの方式 |
| `/usage` | アカウントごとの使用量と上限 |
| `/stats` | モデルごとの実測（tok/s、最初の文字までの時間、キャッシュ） |
| `/status` | 今の会話のモデル・アカウント・モード・使ったトークン |
| `/estimate 〈作業〉` | 作業の時間とトークンの見積もり（`effort-estimates.md`） |

### 翻訳（全体の表示の設定）

| コマンド | 動き |
|---|---|
| `/translate` | 今の設定（チェックの状態、方式、訳す先） |
| `/translate thinking on・off` | 思考を表示で訳す |
| `/translate answer on・off` | 答えを表示で訳す |
| `/translate service 〈google・edge・firefox・chromium〉` | 方式 |
| `/translate to 〈言語〉` | 訳す先 |
| `/translate models` | 取得済みのモデルの一覧と削除 |
| `/translate languages [方式]` | 訳せる言語 |

- 送る文の翻訳（人の発言をモデルの言語に訳して送る）は全体の設定に置かず、`/model` のモデルの設定で決める。英語しか話せない小さなモデル向け。
- 送る文を訳したときは、人の原文と送った訳の両方を記録する。画面には原文を出し、頼まれれば送った訳を見せる（ユーザー：「翻訳後も人間からの要求あれば見れるようにする」）。思考・答えの訳は表示だけで記録しない（「訳すのは見た目上だけ。記録は原文で」）。

### 機器・ファイル

| コマンド | 動き |
|---|---|
| `/attach 〈パス〉` | 次の発言にファイル・画像を添える |
| `/devices` | カメラ・マイクの一覧 |
| `/transcribe 〈ファイル〉 [サービス]` | 音のファイルを文字にする |

### 拡張

| コマンド | 動き |
|---|---|
| `/plugins` | プラグインの一覧・探す・入れる・外す・有効無効 |
| `/skills` | 入っているスキルの一覧 |
| `/mcp` | MCP サーバーと道具の一覧、有効無効 |
| `/connectors` | Claude・ChatGPT のコネクタの一覧 |

### その他

| コマンド | 動き |
|---|---|
| `/help [コマンド]` | 一覧と使い方 |
| `/init` | プロジェクトの AGENTS.md を作る |
| `/review` | 今の変更を見直す |
| `/diff` | git の差分 |

## 決めたこと（2026-11-25、ユーザー）

- 組み込みの名前の上書きは禁止（上の決まり 5）。
- `/goal status` に見積もりを出す（「だす」）。ここまでの時間とトークン、残りの見込みの幅（`effort-estimates.md`）。
- ターンの途中で使える範囲（「これでいい」）：
  - 表示だけのもの（`/notes`、`/plan`、`/status`、`/issues` など）と `/translate` は、途中でもすぐ効く。
  - `/model`・`/effort`・`/mode` は、次のモデルへの要求から効く。
  - 会話を変えるもの（`/new`、`/resume`、`/fork`、`/compact`、`/delete` など）は、ターンが終わってから。

## 実装の状態

### プロトコル

- `commands/list`（`thread_id` は任意）→ `{commands: [...], conflicts: [...]}`。
  - 各コマンド：`name`（組み込み以外は `プラグイン名:名前`・`user:名前`・`project:名前`）、`short`（定義元なしで呼べるときの短い名前、呼べなければ `null`）、`aliases`、`description`、`source`（`builtin`・`plugin`・`user`・`project`）、`plugin`、`kind`（`builtin`・`command`・`skill`）、`forms`、`bare`（何も付けないときに使うサブコマンド）、`available`、`reason`。
  - `forms` は呼び方ごと（`sub` が `null` ならコマンドそのもの、そうでなければサブコマンド）に、`usage`、`args`（`name`、`kind`＝`text`・`path`・`number`・`choice`、`choices`、`required`）、`when`（`now`・`next_request`・`after_turn`）、`runs`（`result`・`turn`）、`available`、`reason`。
  - `conflicts`：短い名前が組み込みとぶつかるか、組み込み以外どうしで重なるものと、代わりに使う名前。
  - `thread_id` を付けると、その会話のプロジェクトの `.inocode/commands/*.md` も入る。
- `command/run`（`thread_id` と、打った文字の `text`（例 `"/goal fix the tests"`）、または `name` と `args`）。
  - 結果を返すコマンド：`{id, result: {command, ...}}`。
  - ターンを始めるコマンド：まず `{id, event: "command", result: {command, ...}}`、続いて `turn/start` と同じ出来事（`started` から `completed`・`failed`・`cancelled` まで）。`cancel` で止められる。
  - 失敗：`{id, error: {code, message, when?}}`。`code` は `unknown_command`・`ambiguous_command`・`invalid_arguments`・`unavailable`・`not_now`（`when` に使えるようになる時：`after_turn`）・`unknown_thread` ほか、元の機能のエラー。
- `thread/list`（`project` は任意でプロジェクトのキー、`limit` は 1〜1000、既定 100）→ `{threads: [{thread_id, project, title, created, last_activity, messages}]}`。最後の発言が新しい順。
- 保存形式の変更は無い。会話の名前は `sessions.title` に元からある列を使う。

### 名前の決まりの実装

- 組み込みの名前と別名は、ほかのどれにも取られない。プロジェクトの `goal.md` は `/project:goal` としてだけ呼べ、`/goal` は組み込みのまま。
- 組み込み以外で短い名前を使えるのは、組み込みに無く、組み込み以外でその名前が 1 つだけのとき。重なれば `ambiguous_command` と呼び分け方を返す。
- 定義元を付けた名前まで同じもの（同じプラグインのコマンドとスキルが同名など）は、どちらも呼べない（`available: false`）。
- `*.md` の形式：先頭の `---` で囲んだ部分から `description`・`argument-hint` を読む。本文の `$ARGUMENTS` を引数に置き換える。本文に `$ARGUMENTS` が無く引数があるときは、Claude Code と同じく本文の後に `ARGUMENTS: 〈引数〉` を足す。サブフォルダは読まない。
- プラグインのスキルは「スキル 〈名前〉 を使う。SKILL.md（パス）を read で読んで従う」という指示文と引数で、ターンを始める。
- 利用者のコマンドの置き場所は、データベースの隣の `commands/`（既定で `~/.inocode/commands/`）。

### ターンの途中

- 決めたとおり：表示と `/translate` は `now`、`/model`・`/effort`・`/mode` は `next_request`、会話を変えるもの（`/new`・`/resume`・`/fork`・`/compact`・`/delete`）とターンを始めるもの（`/goal 〈目標〉`・`/init`・`/review`・`/approve 〈番号〉`・`/deny 〈番号〉`・`ask` への `/answer`・プロンプトのコマンド）は `after_turn`。
- 表に無いものの扱い（実装での判断）：`/rename`・`/notes` の変更・`/issues done`・`/promise remove`・`/providers` の変更・`/plugins install`・`/roles` の変更は `now`（会話の中身を変えない記録・設定のため）。`/roles` の変更は、ターンの補助役がターンの始めに作られるので、次のターンから効く（結果の `applies: "next_turn"`）。`/rules import` はモデルに頼むので `after_turn`。
- `/mode` を途中で変えると、走っているターンの次のモデルへの要求から、新しいモードで承認を判断する。ただし、そのターンで渡した `shell` の説明（サンドボックスの有無）と、`auto` の判定役のモデルは、ターンの始めのまま（判定役が無ければ会話のモデルが判定する）。

### コマンドごと

実装済み（既にある機能を呼ぶ）：

| コマンド | 実装 |
|---|---|
| `/help [コマンド]` | 一覧と呼び分け方、または 1 つの詳細 |
| `/new` | 同じプロジェクトで新しい会話。結果の `thread_id` へ UI が切り替える |
| `/resume [会話]` | 会話の一覧（50 件）、または 1 つの概要。切り替えは UI が行う |
| `/rename 〈名前〉` | `sessions.title` を書き換える |
| `/fork` | メッセージ（保存時刻ごと）、計画、約束、モデルの選択を新しい会話へ写す。目標は写さない |
| `/delete confirm` | 会話・メッセージ（検索の索引からも）・計画・目標・約束・モデルの選択・承認待ちと質問を消し、その会話のシェルの命令を止め、データベースの横のその会話のファイル（命令のログ、画像、音、取得したページ、保存した長い発言）を消す。消せなかったファイルは `files_not_removed` に出す。モデルへの要求の記録（統計用）と課題の出来事は、会話との結び付きを外して残す。共有されうる大きな中身は残す。会話の番号は二度と使わない（データベースの版 10 の `session_ids`） |
| `/compact` | `compaction::compact` を今すぐ呼ぶ。会話がモデルの文脈の窓を満たしているとみなし（実際の見積もりの方が大きければそちら）、古い部分を要約に替え、最近の部分を残す。結果を返し、ターンとしては流さない。要約が書けなかったときは `compaction_failed` の誤りを返し、会話は変えない（ターンの中の自動の圧縮は、要約の代わりに呼び出しの記録を残して続ける）。圧縮の間の `turn/steer` は受け付けない（`accepted: false`） |
| `/export [markdown・json] [パス]` | 文字として返す。パスを付けるとファイルに書く（相対パスはプロジェクトのフォルダから） |
| `/history 〈語〉` | `search_history`（全プロジェクト、20 件） |
| `/goal 〈目標〉`・`status`・`stop` | `goal/set` の後、目標の文でターンを始める。目標の前に `--check "命令"`・`--rounds 回数`・`--tokens 数` を書くと、`goal/set` の `check`・`rounds`・`tokens` になる。何も付けなければ `status` |
| `/stop` | この会話の走っているターンを止める（`turn/interrupt` と同じ。どの接続からでも）。いつでも使える |
| `/plan` | `plan` の道具の記録 |
| `/promise`・`remove 〈番号〉`・`clear` | `promise` の道具の記録 |
| `/issues`・`done 〈番号〉 [理由]`・`drop 〈番号〉 [理由]` | 課題の一覧、done・wontfix で閉じる（記録者は人） |
| `/notes`・`search`・`show`・`edit`・`tag`・`vote`・`retire` | 手法メモ（`lesson`・`method`）。`edit` は新しいメモを作って古いものを `supersede` で置き換える（古いものは履歴として残る） |
| `/rules`・`import`・`off 〈番号〉` | ルールの一覧（強さ付き）、`AGENTS.md` を `import_rules` で取り込む、使わない印 |
| `/colleague` | 同僚の設定（`roles.colleague`）と、この会話の同僚の助言 |
| `/members`（別名 `/profiles`）・`list`・`add 〈名前〉 〈プロバイダ/モデル〉 [役割]`・`set 〈名前〉 〈provider・model・effort・role〉 〈値〉`・`remove 〈名前〉` | `members/*`。何も付けなければ `list`：社員、この会話のプロジェクトで許された一覧、この会話が任せた仕事（任された会話の番号）。`add` でプロバイダを書かなければこの会話のプロバイダ。`set … effort default` で深さの指定を外す（`company.md`） |
| `/allow`・`show`・`default 〈一覧〉`・`project 〈一覧〉`・`approve` | `allow/*`。一覧は社員の名前か `プロバイダ/モデル` を空白かカンマで区切る。`default none` で既定を空に、`project default` でこのプロジェクトの一覧を外す。`approve` はリポジトリの `.inocode/models.json` を今の中身で承認する（`company.md`） |
| `/mode [モード]` | 会話の設定の `mode` |
| `/approve`・`/deny [番号・all]` | 最後のターンが保留した呼び出しの一覧。番号を付けると、承認（`approve` にその呼び出しだけを入れたターン。その呼び出しが当たった規則は許さないので、同じ規則に当たる別の呼び出しはまた保留になる）または拒否を伝えるターンを始める。一覧はデータベースから読み、次のターンが終わるまで残る（ターンが始められなかったときも消えない） |
| `/answer [番号 答え]` | 最後のターンの `ask` の質問と、走っているターンが待つ MCP の質問の一覧（サーバーを起動し直す前の MCP の質問は `expired: true` で出し、答えると `expired` の誤りを返して消す）。`ask` には `reply_to` 付きのターンで答え、MCP には JSON の内容・`decline`・`cancel` で答える |
| `/sandbox` | 有効か、会話のモード、閉じ込めるか、書ける場所 |
| `/model [プロバイダ/モデル・モデル・default]`・`/effort [深さ・default]` | 会話の設定 |
| `/roles [役 モデル・default]` | `settings/set` の `roles` |
| `/providers`・`add 〈id〉 〈種類〉`・`set 〈id〉 〈欄〉 〈値〉`・`remove 〈id〉` | `providers/*` |
| `/models [プロバイダ]` | `models/catalog` |
| `/login [プロバイダ] [callback・code・device]`・`code 〈番号〉 〈コード〉`・`status 〈番号〉`・`cancel 〈番号〉` | `accounts/login/*`。プロバイダを書かなければこの会話のプロバイダ。結果の `url`（と Muse の `user_code`）を人が開く。`code` は Claude でブラウザがこの機械に戻れないとき。〈番号〉は結果の `login_id` |
| `/logout [アカウント]` | この会話のプロバイダのアカウントを `accounts/remove` で消す（秘密も消える）。書かなければ、いまそのプロバイダが使っているアカウント |
| `/accounts`・`list [プロバイダ]`・`use 〈プロバイダ〉 〈アカウント〉`・`pause`・`resume`・`remove 〈プロバイダ〉 〈アカウント〉`・`rename 〈プロバイダ〉 〈アカウント〉 〈別名〉`・`priority 〈プロバイダ〉 〈アカウント〉 〈-100〜100〉`・`clear-cooldown 〈プロバイダ〉 〈アカウント〉`・`strategy 〈プロバイダ〉 〈quota・round-robin・fill-first・reset-first〉`・`status [プロバイダ]` | `accounts/*`・`pool/set`（`strategy`）・`pool/status`（`status`）。アカウントは id か別名で書ける。何も付けなければ `list`。プールの中身は `account-pool.md` |
| `/usage`・`/stats [日数]` | `usage`・`stats/models`。`/stats` の時刻はサーバーの時間帯で数える |
| `/status` | 会話の概要、モデルとモード、走っているか、この会話の要求のトークンの合計、目標 |
| `/translate`・`thinking`・`answer`・`service`・`to`・`models [remove …]`・`languages [方式]` | `settings/translation`・`translation/*` |
| `/devices` | `camera`・`audio` の道具の `list` |
| `/transcribe 〈ファイル〉 〈サービス〉 [言語]` | `transcribe` の道具。サービスの既定は無いので必須にした |
| `/plugins`・`search`・`install` | 入っているプラグイン（スキル・MCP サーバー・コマンド）、カタログの検索、入れる |
| `/skills` | プラグインのスキルと、その呼び名 |
| `/mcp` | `mcp.json` とプラグインの MCP サーバー、起動済みならその道具、コネクタのオンオフ。サーバーは起動しない |
| `/connectors` | サインイン中のアカウントのコネクタ（通信して取る） |
| `/init`・`/review [観点]` | 決まった指示文でターンを始める（プロジェクトのフォルダが要る） |
| `/diff` | `git status --short` と `git diff HEAD` |
| `/estimate 〈作業〉` | プロトコルの `estimate` と同じ（この会話のモデルで）。似た過去の作業からの幅、件数、方法（`effort-estimates.md`） |

まだ無い機能に依存（一覧に `available: false` と `reason` 付きで出し、実行すると `unavailable`。一覧から外すのではなく、こちらに統一した）：

| コマンド | 足りないもの |
|---|---|
| `/attach` | `turn/start` が文字しか運べない |
| `/rules edit` | ルールの文を、強制の設定を保ったまま直す保存の仕組みが無い |

ほかに、決めたことのうち満たしていない点：

- `/goal status` は、ここまでの時間（目標の最初のメッセージから、走っていれば今まで、そうでなければ最後のメッセージまで）とトークン（目標が数えたもの）、回数と上限、判定役の最後の判断、目標が続いている間は残りの見積もりの幅（`estimate`。終わった目標では `null`）を出す。
- `/translate answer` は設定を保存するだけ。答えの翻訳はまだ無いので、結果の `note` にそう書く。
- `/plugins` の外す・有効無効、`/mcp` の有効無効、`/providers` の一覧以外の細かな操作（隠す等）はコマンドに無い。
- `/model` からのモデルの設定（思考の深さの既定、送る文の翻訳、モデルの言語）は無い。
- `/status` にアカウントは出さない（アカウントは `/accounts list` で見る）。
- `/accounts` からはプールの方式・優先度・クールダウンの解除・状態だけを扱う。`sticky_limit`・`auto_switch_threshold`・`failover_threshold`・`cache_affinity`、アカウントごとの閾値、会話の固定の解除は `pool/set`・`accounts/priority`・`pool/reset_affinity` で行う（`account-pool.md`）。`excluded_plans`・`start_idle_windows`・`priority_failback`・`host_circuit_threshold`、アカウントごとの `credits` も同じ。API キーのプロバイダの id を書けば、そのキーのプールのキーを扱う。キーを足すのは `accounts/add_key` と `inocode-server key set` だけで、スラッシュコマンドは無い（会話の記録にキーを残さないため）。
- 保留と質問の一覧は、サーバーが動いている間だけ覚えている（再起動で消える）。

### 道具とコマンドの照合（決まり 6）

- `core/src/commands.rs` の `SHOWN_BY`：記録を残す道具と、それを見るコマンド（`plan`→`/plan`、`promise`→`/promise`、`lesson`・`method`→`/notes`、`issue`→`/issues`、`goal`→`/goal`、`ask`→`/answer`、`plugin_install`→`/plugins`）。
- `RECORDS_NOTHING`：自分の記録を持たない道具（読む・探す・動かす道具と、ファイルを書く道具）。
- 試験 `every_tool_that_keeps_a_record_has_a_command_that_shows_it` が、エンジンの道具の一覧のすべてがどちらか一方に入っていること、見るコマンドが組み込みで使えることを確かめる。道具を足すと、どちらかに入れない限り試験が落ちる。
- `shell` の `job`（裏で動く命令）は、命令のログをファイルに残すだけなので記録を持たない側に入れた。動いている命令の一覧を見るコマンドは無い。

### 試験

- 単体（`core/src/commands.rs`）：入力の分け方、組み込みの上書き禁止、一意な短い名前、同名には定義元が要る、`*.md` と前書きの読み取りと `$ARGUMENTS`、スキルの指示文、引数の結び付け、ターン途中の可否、組み込みの定義の整合、道具とコマンドの照合。
- プロトコル（`server/tests/commands_tcp.rs`、モックのプロバイダ）：一覧と名前の決まり・プロンプトのコマンドのターン、`/goal` と `/goal status`・`stop`、引数の誤り・未対応・未知のコマンド、会話の一覧・名前・写し・書き出し・検索・削除・新規・再開、ターン途中の可否、手法メモ・課題・約束・保留・質問・同僚・翻訳・ヘルプ。

### 未確認

- `/compact`、`/rules import`、`/connectors`、`/devices`、`/transcribe`、`/plugins search・install`、`/diff`、`/init`、`/review`、`/approve 〈番号〉`・`/deny 〈番号〉`、MCP への `/answer` はプロトコルの試験で動かしていない（呼ぶ先の機能はそれぞれの試験がある）。
- `/promise remove` の後、人がその後に書いていれば、モデルは約束の一覧を確かめ直すよう求められ、消した約束を元の発言から戻す可能性がある（一覧の位置は変えずに残しているため）。

### 実際のモデルでの確認（2026-11-25）

- 試験 `live_goal_and_prompt_command`（`server/tests/commands_tcp.rs`、手で動かす）。プロバイダは保存済みの Claude のサインイン（`INOCODE_TEST_PROVIDER=claude-oauth`）、モデルは `claude-haiku-5-5`。
- `/goal Reply with the single word READY.`：`command` の出来事の後にターンが流れ、答えは「READY」、ターンは `completed`（要求 3 回：作業役 1 回と判定役。入力 16,735、出力 547、キャッシュ 7,260 トークン）。
- `/goal status`：状態 `achieved`、判定役が目標を 2 つの点に分け、理由を返した。`elapsed_ms` 7,358、`tokens` 17,282、`rounds` 0。
- 利用者のコマンド `echo.md`（本文「Reply with exactly these words and nothing else: $ARGUMENTS」）を短い名前 `/echo blue whale` で呼び、`user:echo` として解決され、答えは「blue whale」。
- 1 回だけの確認で、ほかのモデル・プロバイダでは動かしていない。

## 審査役への渡し方

- プロジェクトのコマンドとプラグインのコマンド・スキルから作った依頼は、先頭に `[command file] /名前 引数` の 1 行を付けて保存する。`auto` モードの審査役には、その 1 行だけを「Person:」として渡し、ファイルの文は「Command file (from the repository or a plugin, not the person's words)」として別に渡す。他人のリポジトリのコマンドの文で、審査役に強制 push などを通させないため。自分用のコマンド（`~/.inocode/commands`）は本人の言葉として扱う。
- 試験：`review::tests::a_command_files_text_is_not_the_persons_words`。
