# スレッドの題名（自動の題名、モデルからの提案と変更）

- **対象と範囲：**
  - 自動の題名：スレッドの人の最初のメッセージが記録されるとき、スレッドに題名がなければ、裏で題名を作る要求を 1 回出す（ターンを待たせない）。モデルは、スレッドの提供元に小さいモデル（`luna`・`flash-lite`・`flash`・`haiku` の系統の順に、見つかった系統のうち名前がいちばん後のもの）があればそれ、なければスレッドの今のモデル。小さいモデルが失敗したら、スレッドのモデルで 1 回だけやり直す。それも失敗するか使える答えがなければ、人の最初のメッセージの 1 行目を 80 文字で切ったものにする（`agent_tools::cut_title`）。保存は、そのときまだ題名がない場合だけ（比べてから保存）。後のターンでは作り直さない。
  - モデルの道具 `title`：`action` が `propose`（提案）か `set`（変更）。提案は題名を変えず、人の画面に出す通知を送るだけで、モデルは作業を続ける。同じ題名を 2 度提案すると、通知は送らずにそう返す。今の題名と同じ提案も送らない。変更は、人がこの会話で明示的に題名を変えるよう頼んだときだけ使う（道具の説明に書いた）。
  - 毎回の要求の後ろに付ける状態（`Thread::state`）に、題名があれば `The thread's title: …` の 1 行を入れる。モデルが作業と題名のずれに気づけるようにするため。
  - 通知：題名が変わったとき（自動、モデルの `set`、`/rename`）に `title_updated`、提案のときに `title_proposed` を、走っているターンの通知の流れに出す。ターンの外の変更は `thread/titles/subscribe` で届く。
  - 範囲外：題名の設定項目（題名用のモデルの指定など）は作らない。ターンが走っていないときの題名の変更（例：ターンの後の `/rename`、ターンが終わってから返ってきた自動の題名）は通知されない。inocode にはスレッド全体の通知の流れがなく、通知はターンの要求の ID に付くため。`thread/list` などで読める。
- **関連ソースコード：**
  - `core/src/title.rs`：小さいモデルの選び方、題名の要求と指示文、答えの読み取り、使用量の数え方、道具の定義と引数。
  - `core/src/turn.rs`：`TurnEvent::Title`・`TitleProposed`、`Models::title`、`Thread::start_title`・`Thread::title`（道具の処理）、`Thread::state`（題名の行）、`steps`（道具を渡す。会社のメンバーの実行には渡さない）、`run`（ターンの終わりに使用量を合わせる）。
  - `core/src/store_threads.rs`：`title_untitled`（題名がないときだけ保存）、`session_title`。
  - `core/src/lib.rs`：`EventPayload::title`、`Running::events`、`Core::announce_title`、`Core::run_turn`（設定の `models` の一覧を渡す）。
  - `core/src/commands.rs`：`/rename` が走っているターンに通知する。
  - `server/src/lib.rs`：`title_updated`・`title_proposed` への変換。
- **根拠の状態：** UNSUPPORTED_APPROVED。適用できる学術研究は確認できていない（調べた範囲では、ほかのハーネスの実装だけがある）。
- **ユーザーの承認：**
  - 2026-11-26「実態とあっていないときは作業継続しながら変更提案を出す機能を、明示的に変えてって言われたら変える機能を作ってあとは他のハーネス同様で」
  - それより前に「変更をモデル側からできるように」
  - 題名用のモデルを「ほかのハーネスと同じく」同じ提供元の小さいモデルにすること、設定項目を足さないことは、作業の指示（コーディネーター経由）による。

## ほかのハーネス（2026-11-26、調査用のエージェントがソースと配布物を読み、報告。inocode の作業者は直接読んでいない）

| ハーネス（版） | 自動の題名 | 題名に使うモデル | 指示・長さ | 失敗したとき | 作り直し・人の変更との関係 | モデルの道具・通知 |
|---|---|---|---|---|---|---|
| Codex（openai/codex af71942、`tui/src/app/thread_title.rs`） | 最初のユーザーメッセージが記録され、スレッドに名前がないとき、返答の前に裏で 1 回 | ChatGPT にサインインしていれば専用の軽いモデル（gpt-5.6-luna、低い effort）、そうでなければスレッドの今のモデル | 1 行、36 文字まで、ユーザーの言語、「依頼に答えない」。入力は最初のメッセージ。JSON `{title}` | 最初のメッセージを 60 文字で切って保存 | 自動では作り直さない。人の変更は待っている生成を取り消す。自動の保存は、名前がまだ期待どおり空のときだけ | 道具 `set_thread_title`。通知 `thread/name/updated {threadId, threadName}` |
| Claude Code CLI 2.1.332 | 最初のユーザーメッセージの後、裏で小さく速いモデル（Haiku）に | 小さく速いモデル | 2〜5 語の名詞句、設定の言語かセッションの言語、セッションの文はデータで指示ではない。JSON `{title}` | 後のターンでやり直す | 作り直さない。人の題名があれば省くか捨てる | CLI にモデルの道具はない |
| Gemini CLI 60f8082 | 動いているあいだは題名なし。次の起動時に前のセッションを要約する | — | — | — | 名前の変更はない | — |
| opencode（bcd1457、`packages/core/src/session/context.ts` の `selectTitle`、`model.ts:316` の `model.small`） | あり | 設定の題名用モデル、なければ提供元の小さいモデル（gpt-luna、gemini-flash-lite、gemini-flash、claude-haiku の順に最初にあるもの。minimal の変種）、なければ主のモデル | 不明（報告になし） | 選んだモデルが失敗したら主のモデルで 1 回やり直す | 不明 | 道具 `opencode.session.rename`、イベント `session.renamed` |
| OpenHands（sdk d726333） | 題名が空のあいだだけ | 題名用のモデルの設定、なければエージェントのモデル | 50 文字まで | 50 文字で切った文 | 題名が空のときだけ作る。人は PATCH で変える | モデルの道具はない |
| Cline（2a0f048） | 最初の依頼の 1 行目を 120 文字まで。LLM は使わない | なし | — | — | — | モデルの道具はない |

## 確かめたこと・確かめていないこと

- 確かめた（テスト）：下の「実装とテストの状況」の各テスト。偽の提供元でだけ。
- 確かめていない：
  - 実在の提供元で、小さいモデルが選ばれて題名が返ること、題名の質と長さ、人の言語で書かれること。
  - 「36 文字くらいまで」は指示で頼むだけで、切るのは 80 文字（`cut_title`）。指示どおりの長さになるかは確かめていない。
  - 最初のメッセージのうち題名の要求に渡すのは 2,000 文字まで（大きな貼り付けで費用がかさまないための工学上の選択。研究なし）。
  - 状態に題名を入れることで、モデルが実際にずれに気づいて提案するか。
  - 提案の重なりは、このスレッドの保存された履歴にある `title` の提案と、同じターンの提案だけを見る。
  - 画面で提案を受け入れる操作。受け入れは、既存の `command/run`（`/rename`、`{"thread_id", "name": "rename", "args": "<題名>"}`）で足りるので、新しいプロトコルのメソッドは足していない。

## 通知の形

- `{"id": <ターンの要求の ID>, "event": "title_updated", "title": "<新しい題名>"}`
- `{"id": <ターンの要求の ID>, "event": "title_proposed", "title": "<提案された題名>"}`
- ターンが走っていないときの変更も届くように、`thread/titles/subscribe`（引数なし）がある。`started` の後、どのスレッドの題名が変わっても（自動、モデルの `set`、`/rename`）`{"id": <購読の要求の ID>, "event": "title_updated", "thread_id": <スレッド>, "title": "<新しい題名>"}` を送り、`cancel` か接続が切れるまで続く。題名の変更はデータベースの書き込み（`Store::rename_session`、`Store::title_untitled`）で流すので、変え方を問わない。購読者が遅れて 64 件より多くたまると、古い変更はその購読者に届かない。

## 使用量

- 題名の要求は、記録の目的 `thread_title` として数える。ターンが終わる前に終わった要求は、そのターンの使用量（`completed` の `usage`）に入り、ターンの要求と一緒に記録される（`mcp_sampling` と同じ）。ターンの後に終わった要求は、そのスレッドの要求として単独で記録される。

## 実装とテストの状況

- 単体テスト（`core/src/title.rs`）：小さいモデルの選び方、答えからの題名の読み取り、道具の引数。
- TCP テスト（`server/tests/turn_tcp.rs`、偽の提供元）：
  - `the_first_message_names_the_thread_once_with_a_small_model_of_the_provider`：一覧の小さいモデル（`mini-haiku-1`）で題名が作られ、`title_updated` が出て、`thread/list` の題名になること。要求に道具がなく、人のメッセージが入ること。使用量（要求 2 回、入力 107）がターンに入ること。次のターンでは作り直さず、状態に題名の行があり、`title` 道具が渡されること。
  - `a_thread_is_named_by_its_first_line_when_the_title_requests_fail`：小さいモデル、次にスレッドのモデルの順に 1 回ずつ試し、両方失敗したら最初の空でない行が題名になること。
  - `a_rename_while_the_title_is_made_wins`：生成中の `/rename`（別の接続から）が走っているターンに `title_updated` として届き、後から返った自動の題名で上書きされないこと。
  - `the_model_proposes_a_title_once_without_changing_it`：提案で `title_proposed` が 1 度だけ出て、2 度目は送られず、題名は変わらないこと。題名のあるスレッドでは題名の要求が出ないこと。
  - `the_model_sets_the_title_when_asked`：`set` で題名が変わり、`title_updated` が出て、次の要求の状態に新しい題名が出ること。
- TCP テスト（`server/tests/commands_tcp.rs`）：`title_changes_reach_a_subscriber_while_no_turn_runs`：ターンが走っていないときの `/rename` が、別の接続の購読に `thread_id` 付きの `title_updated` として届き、`cancel` で止まること。
- 既存の TCP テストは、スレッドを作るときに題名を付ける（`support::name_thread`）。題名の要求が偽の提供元への要求の順番や数を変えないようにするため。道具の数を確かめるテストは 1 つ増やした。
- 2026-11-26 に実行：`cargo test -p inocode-core`（414 件成功、34 件は手で実行するもの）、`cargo test -p inocode-server`（178 件成功、失敗なし）、`cargo clippy --workspace --all-targets`（警告なし）、`cargo fmt`。実在の提供元では実行していない。
