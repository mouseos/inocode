# スレッドの動きと状態の購読

- **対象と範囲：**
  - `thread/subscribe {thread_id}`：一つのスレッドで起きることを、どの接続がターンを始めたかに関係なく流す。
    - 始めに `started` を送り、続いて今の状態の `status` を送る。走っているターンが人の答えを待つ問い（承認の問い、MCP の質問）があれば、続けてそれぞれを `elicitation` で送る。後から開いた画面にも問いが見える。
    - その後はターンごとに `turn_started`、ターンを始めた接続が受け取るのと同じイベント（`delta`・`thinking`・`message` など）、`turn_ended` の順に流す。
    - 問いが片付く（答え・中断）と `answered`（`elicitation` にその id）を送る。ほかの画面はそれで問いを閉じる。
    - `turn_ended` の `reason` は `completed`・`length`・`cancelled`・`failed`。`usage`・`questions`・`warnings`・`error` も付く。
    - 状態が変わるたびに `status` を送る。
  - `thread/status/subscribe`：すべてのスレッドの `status` を、`thread_id` を付けて流す。
  - 状態は 3 つ：`idle`（何も走らず、人を待つものもない）、`running`（ターンか圧縮が走っている）、`waiting`（走っているターンが人の答えを待っている、または走っていないが最後のターンが質問を残した）。
    - 走っているターンが最初の問いを出すと、その `elicitation` の前に `waiting` を送り、最後の問いが片付くと、その `answered` の後に `running` を送る。この 2 つはターンのイベントを分ける所（`Core::run_claimed`）で送るので、`begin_turn` の `running` と `end_turn` の `idle`・`waiting` の間に、イベントと同じ順で届く。
  - `thread/list` の各スレッドに `status` を付ける。
  - 購読者が遅れて取りこぼしたら `lagged` を送る。画面は `thread/read` で読み直す。どちらの購読も `cancel` か接続が切れるまで続く。
  - 範囲外：
    - 委任したメンバーのスレッドの動き（今も捨てている）。
    - スレッドの作成・削除・名前の変更などの通知（洗い出しの ⑪）。
- **関連ソースコード：**
  - `core/src/lib.rs`：`ThreadStatus`、`Activity`、`TurnEnd`、`Core::activity`、`Core::thread_status`。
    - `Core::begin_turn` と `Core::end_turn` が状態を送る。`turns` の鍵を持ったまま送るので、順番が入れ替わらない。
    - `Core::run_turn` は `turn_started`・`turn_ended` を送る。`Core::run_claimed` は、ターンのイベントを購読者にも分け、問いの開き・片付きで `waiting`・`running` を送る。分けたイベントを全部渡し終えてから、ターンの終わりを知らせる。
    - `Core::elicitations`（開いている問い）、`EventPayload::answered`。
    - `EventPayload::thread_status`、`EventPayload::turn_ended`、`EventPayload::named`。
  - `core/src/commands.rs`：`Core::threads`（`thread/list` の `status`）。
  - `server/src/lib.rs`：`watch_threads`、`relay_threads`、`turn_event`。
  - テスト：`server/tests/turn_tcp.rs` の `another_connection_follows_a_threads_turns_and_its_status`、`another_screen_sees_an_approval_prompt_and_its_answer`、`a_screen_that_subscribes_while_a_prompt_is_open_is_shown_it`。
- **根拠の状態：** UNSUPPORTED_APPROVED。画面と接続の仕組みで、効き目を測る学術研究は当てはまらない。調べたのは、ほかのハーネスの実装だけ。
- **ユーザーの承認：** 2026-11-27。足りない機能の洗い出しで「⑤（別の画面から同じスレッドの動きを見る）と ⑩（スレッドの状態の通知）から始めて、急ぐ群を上から順に進めます」という選択肢に、「急ぐ群から順に」と答えた。
  - 購読を 2 つに分けること、状態を 3 つにすること、イベントの名前は、作業者（Claude）が決めた。
  - 2026-11-27：承認をその場で聞く方式に変えたこと（`approval-modes.md`）に合わせ、走っているターンの問いを `waiting` にすること、`answered`、購読したときに開いている問いを送ることを加えた（作業を頼んだ側の指示。名前 `answered` もその指示による）。

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
  - 承認の問いの間、別の画面に `running`、`turn_started`、`waiting`、`elicitation`、`answered`、`running`、`turn_ended` の順で届き、`answered` の id が問いの id と同じ。その画面から答えられる。
  - 問いが開いている間に購読すると、`started`、`waiting`、その問いの `elicitation` が届き、答えると `answered`、`running` が届く。
- 確かめていないこと：
  - `lagged` が実際に出る場面（4096 件より遅れたとき）。
  - MCP の質問での `waiting`・`answered`（承認の問いと同じ道を通るが、試験は承認の問いだけ）。
  - 購読の始めに送った問いと、同じ問いが購読の流れでもう一度来る場面（id で 1 回だけにするが、その順を作る試験はない）。
  - 画面からの使いやすさ。
