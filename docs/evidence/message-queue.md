# 次のターンとして送る文の列

- **対象と範囲：**
  - ターンが走っている間に、人は文を列に並べられる。列はエンジンが持ち、データベースに保存する（版 17 の `queue` 表）。サーバーを起動し直しても残る。
  - 要求（どれも `thread_id` が要る）：
    - `thread/queue/add {thread_id, prompt}`：列の最後に足す。`{queue_id, queue}` を返す。ターンが走っていなければ、すぐに列の先頭を送る。
    - `thread/queue/list {thread_id}`：`{queue: [{queue_id, prompt, created}]}`。送る順。`created` はミリ秒の時刻。
    - `thread/queue/update {thread_id, queue_id, prompt}`：文を書き直す。`{queue}` を返す。
    - `thread/queue/delete {thread_id, queue_id}`：列から外す。`{queue}` を返す。
    - `thread/queue/reorder {thread_id, queue_ids: [...]}`：並べ替える。`queue_ids` は、今の列の id をちょうど 1 回ずつ並べたものでなければならない。`{queue}` を返す。
    - `thread/queue/start {thread_id}`：ターンが走っていなければ、列の先頭を今送る。`{started}`（送らなかったら `false`）。
    - 空の文、無い `queue_id`、id の足りない・重なる並べ替えは `invalid_params`。無いスレッドは `unknown_thread`。
  - 自動で送る：ターンが `completed` か `length` で終わると、列の先頭を列から外し、新しいターンとして送る。スレッドに保存した承認の方式（無ければ既定）で走る。
    - どのターンの後でも同じ：`turn/start`、`command/run` が始めたターン、列から送ったターン。
    - 列から送ったターンには、頼んだ接続が無い。エンジンの中の別のタスクで走り、そのイベントは `thread/subscribe` で画面に届く。止めるには `turn/interrupt` を使う。
    - 「走っていないか確かめる → ターンを始める（`begin_turn`）→ 先頭を外す」を 1 つのロックの中で行う。列を変える要求も同じロックを取る。ターンを始められなかったときは、文は列に残る。同時に足したりターンが終わったりしても、文が消えることも、ターンが 2 つ始まることもない。
  - 画面への知らせ：`thread/subscribe` に `queue` のイベント（`thread/queue/list` と同じ形の `queue`）を送る。足す・書き直す・外す・並べ替える・送るために先頭を外す、のたびに、列の全体を送る。購読を始めたとき、列が空でなければ、始めの `status` のすぐ後に送る。
  - スレッドを消すと列も消える。フォークでは列を写さない。巻き戻し（`thread/rewind`）では列を残す（列の文は、これから送るために書いたもので、消した発言のためのものではないため）。
  - 範囲外：
    - `turn/steer`（走っているターンに文を差し込む）は今のまま。
    - 画面の操作（どのキーで列を開くかなど）。
    - スラッシュコマンドからの操作。
  - 限界：
    - 列から送ろうとして失敗した場合（プロバイダの設定が無いなど）、文は列に残るが、自動で送ったときには理由がどこにも知らされない。`thread/queue/start` を送ると誤りが返る。`thread/queue/add` は送れなくても成功を返す。
    - 圧縮（`/compact`）の後には列を送らない。圧縮の間に足した文は、次の `thread/queue/start` か `thread/queue/add` を待つ。
    - ターンが終わってすぐに列の先頭が送られるので、その直後の `turn/start` は `turn_running` で断られることがある。
- **関連ソースコード：**
  - `core/src/queue.rs`：`Core::queue`・`add_to_queue`・`update_queued`・`delete_queued`・`reorder_queue`・`start_queue`・`after_turn`、送る処理 `send_queued`・`run_queued`。
  - `core/src/lib.rs`：`Activity::Queue`、`EventPayload::queue`、`Core` の `queue_lock`。
  - `core/src/store.rs`：`SCHEMA_V17`（`queue` 表）。
  - `core/src/store_threads.rs`：`Queued`、`Store::queue`・`add_to_queue`・`update_queued`・`delete_queued`・`reorder_queue`、`delete_session`（列も消す）。
  - `server/src/lib.rs`：`queue_request`、`run_turn`（`end_turn` の後に `after_turn`）、`relay_threads`（`queue` のイベント、購読の始めの列）。
  - テスト：`server/tests/turn_tcp.rs` の `messages_queued_while_a_turn_runs_are_sent_as_the_next_turns_in_their_order`、`the_queue_starts_when_idle_waits_after_an_interrupt_and_is_kept_in_the_store`。`core/src/store.rs` の `a_queue_is_kept_on_rewind_not_forked_and_deleted_with_its_thread`。
- **根拠の状態：** UNSUPPORTED_APPROVED。画面の操作で、効き目を測る学術研究は当てはまらない。調べたのは、ほかのハーネスの実装だけ。
- **ユーザーの承認：** 2026-11-27。
  - 足りない機能の洗い出しで「急ぐ群から順に」と答えた。
  - 「⑥ 動いている間に列に並べた文は、今のターンが終わったらどうしますか？」に「自動で順に送る（ターンが終わるたびに、列の先頭を次のターンとして送る。列はエンジンに保存し、一覧・書き直し・削除・並べ替えができる。Codex・Claude Code・opencode と同じ）」と答えた。
  - 次のことは、作業者（Claude）が決めた：
    - 中断（`cancelled`）や失敗（`failed`）で終わったターンの後は、列を送らず、`thread/queue/start` か次の `thread/queue/add` を待つ。
    - ターンが走っていないときに足すと、すぐに送る。
    - 要求の名前と引数、`thread/queue/start` の結果 `{started}`、書き直し・削除・並べ替えの結果に列を返すこと、イベントの名前 `queue`。
    - 巻き戻しでは列を残すこと。

## ほかのハーネス（2026-11-27、作業を頼んだ側から伝えられた内容。inocode の作業者は直接読んでいない）

| ハーネス | 列 |
|---|---|
| Codex（上流） | `thread/queue/add`・`list`・`update`・`delete`・`reorder`・`start` と、通知 `thread/queue/changed`（実験的） |
| Claude Code | 作業中に打った文を列に並べ、今の道具の呼び出しの後に送る。上矢印で取り戻して書き直せる |
| opencode | 保存される受け箱（inbox）があり、差し込み（steer）か列（queue）で届ける。`GET`・`DELETE`・`PATCH /api/session/:id/inbox` |

## 検証

- 上のテストで確かめたこと：
  - ターンの間に 3 つ足し、並べ替え、1 つを書き直し、1 つを外すと、それぞれの結果と、購読の `queue` のイベントが、その時の列になる。
  - 走っていたターンが終わると、残った 2 つが、並べ替えた順に、1 つずつ別のターンとして送られる。購読には `turn_ended`、`queue`（先頭を外した列）、`turn_started`、`turn_ended` の順に届く。模擬プロバイダは 3 つの要求を受け、2 つ目には 2 つ目の文だけ、3 つ目には書き直した文が入り、外した文はどれにも入らない。
  - 空の文、無いスレッド、`thread_id` が無い、無い `queue_id`、`queue_id` が無い、id の足りない・重なる・違う並べ替え、配列でない `queue_ids` は断られる。
  - ターンが走っていないときに足すと、すぐに送られる。
  - 列から送ったターンを `turn/interrupt` で止めると、残った文は送られず、データベースを開き直しても列に残っている。後から購読した画面には、`status` の後に列が届く。`thread/queue/start` で送られ、列が空なら `{started: false}`。
  - データベースで、巻き戻しでは列が残り、フォークには写らず、スレッドを消すと列も消える。
- 確かめていないこと：
  - `length` で終わったターンの後に送ること（`completed` と同じ道を通るが、試験は `completed` だけ）。
  - 失敗したターンの後に送らないこと（中断と同じ道を通るが、試験は中断だけ）。
  - `command/run` が始めたターンの後に送ること。
  - スレッドに保存した承認の方式で走ること。
  - 足すのとターンの終わりが本当に同時に起きる場面（ロックで守るが、その順を作る試験はない）。
  - サーバーを起動し直した後に、残った列を送ること（起動し直しただけでは送らない。次の `thread/queue/start` か `thread/queue/add` を待つ）。
  - 画面からの使いやすさ。
