# スレッドの動きと状態の購読

- **対象と範囲：**
  - `thread/subscribe {thread_id}`：一つのスレッドで起きることを、どの接続がターンを始めたかに関係なく流す。
    - 始めに `started` を送り、続いて今の状態の `status` を送る。
    - その後はターンごとに `turn_started`、ターンを始めた接続が受け取るのと同じイベント（`delta`・`thinking`・`message` など）、`turn_ended` の順に流す。
    - `turn_ended` の `reason` は `completed`・`length`・`cancelled`・`failed`。`usage`・`held`・`questions`・`warnings`・`error` も付く。
    - 状態が変わるたびに `status` を送る。
  - `thread/status/subscribe`：すべてのスレッドの `status` を、`thread_id` を付けて流す。
  - 状態は 3 つ：`idle`（何も走らず、人を待つものもない）、`running`（ターンか圧縮が走っている）、`waiting`（走っていないが、最後のターンが承認待ちの呼び出しか質問を残した）。
  - `thread/list` の各スレッドに `status` を付ける。
  - 購読者が遅れて取りこぼしたら `lagged` を送る。画面は `thread/read` で読み直す。どちらの購読も `cancel` か接続が切れるまで続く。
  - 範囲外：
    - 委任したメンバーのスレッドの動き（今も捨てている）。
    - スレッドの作成・削除・名前の変更などの通知（洗い出しの ⑪）。
    - 実行中の MCP の質問を `waiting` として出すこと。MCP の質問は、`thread/subscribe` の `elicitation` イベントで届く。
- **関連ソースコード：**
  - `core/src/lib.rs`：`ThreadStatus`、`Activity`、`TurnEnd`、`Core::activity`、`Core::thread_status`。
    - `Core::begin_turn` と `Core::end_turn` が状態を送る。`turns` の鍵を持ったまま送るので、順番が入れ替わらない。
    - `Core::run_turn` は `turn_started`・`turn_ended` を送る。`Core::run_claimed` は、ターンのイベントを購読者にも分ける。分けたイベントを全部渡し終えてから、ターンの終わりを知らせる。
    - `EventPayload::thread_status`、`EventPayload::turn_ended`、`EventPayload::named`。
  - `core/src/commands.rs`：`Core::threads`（`thread/list` の `status`）。
  - `server/src/lib.rs`：`watch_threads`、`relay_threads`、`turn_event`。
  - テスト：`server/tests/turn_tcp.rs` の `another_connection_follows_a_threads_turns_and_its_status`。
- **根拠の状態：** UNSUPPORTED_APPROVED。画面と接続の仕組みで、効き目を測る学術研究は当てはまらない。調べたのは、ほかのハーネスの実装だけ。
- **ユーザーの承認：** 2026-11-27。足りない機能の洗い出しで「⑤（別の画面から同じスレッドの動きを見る）と ⑩（スレッドの状態の通知）から始めて、急ぐ群を上から順に進めます」という選択肢に、「急ぐ群から順に」と答えた。
  - 購読を 2 つに分けること、状態を 3 つにすること、イベントの名前は、作業者（Claude）が決めた。

## ほかのハーネス（2026-11-27、調査用のエージェントがソースを読み、報告。inocode の作業者は直接読んでいない）

| ハーネス | 同じスレッドを別の画面から見る | 状態の通知 |
|---|---|---|
| Codex（openai/codex bee5dc55ac、`codex-rs/app-server-protocol/src/protocol/common.rs`） | `thread/start`・`thread/resume` で接続がスレッドを購読し、`thread/unsubscribe` でやめる。購読者が 30 分いないと読み込みを外し `thread/closed` を送る | `thread/status/changed`：`notLoaded`・`idle`・`systemError`・`active`（`waitingOnApproval`・`waitingOnUserInput` 付き） |
| opencode（49b9463、`packages/protocol/src/groups/`） | `GET /api/event` の SSE で、すべてのセッションのイベントを流す | セッションのイベント（実行の開始・成功・失敗・中断、`viewed` による既読） |
| Claude Code（公式の資料） | Remote Control で別の機器から同じセッションを操作できる | SDK のイベント `session_state_changed` |

## 検証

- 実装したテスト（上）で確かめたこと：
  - 別の接続が、ターンを始めた接続と同じイベントを、同じ順で受け取る。
  - `turn_ended` の `usage` が、ターンの `completed` と一致する。
  - 状態が `idle` → `running` → `idle`、質問を残したターンの後は `waiting` になる。
  - 全スレッドの購読にも `thread_id` 付きで届く。
  - `thread/list` の `status` が `waiting` になる。
  - 無いスレッドの購読は `unknown_thread` で失敗する。
- 確かめていないこと：
  - `lagged` が実際に出る場面（4096 件より遅れたとき）。
  - 画面からの使いやすさ。
