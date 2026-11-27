# スレッドの操作の要求、すべての画面への通知、アーカイブ

- **対象と範囲：**
  - 要求（今までスラッシュコマンドだけだったものを含む）：
    - `thread/rename {thread_id, title}`：題を変える。`{thread_id, title}` を返す。空白だけの題は `invalid_params`。
    - `thread/delete {thread_id}`：`/delete confirm` と同じく、スレッドを消す（コマンドを止め、スレッドのファイルも消す）。`{deleted, files_not_removed}` を返す。ターンが走っている間は `turn_running` で断る。`/delete` も同じ処理を通る。
    - `thread/archive {thread_id}`・`thread/unarchive {thread_id}`：アーカイブする・戻す。`{thread_id, archived}` を返す。アーカイブは、ターンが走っている間は `turn_running` で断る。
    - `thread/export {thread_id, format?}`：`format` は `markdown`（既定）か `json`。`/export` と同じ文字列を `{format, text}` で返す。ファイルには書かない。それ以外の `format` は `invalid_params`。
    - `thread/search {query, limit?}`：`/history` と同じ検索。`limit` は既定 20、1〜100。`{hits, truncated}` を返す。`query` が無いか空なら `invalid_params`。
    - 無いスレッドは、どれも `unknown_thread`。
  - `thread/list` に `archived?: bool` を足した。既定は `false` で、アーカイブしたスレッドは出さない。`true` ならアーカイブしたスレッドだけを出す。各スレッドに `archived` を付ける（`/resume` の一覧にも付くが、`/resume` はアーカイブしたスレッドも今までどおり出す）。
  - アーカイブ（ユーザーの答え「戻すか聞いてから戻せばいいそして継続」）：
    - 実装した解釈：アーカイブしたスレッドで新しいターンを始めようとすると、エンジンが別のエラーコード `thread_archived` で断る。画面はそれを見て、戻すかを人に聞き、戻すなら `thread/unarchive` を送り、同じ要求を送り直す。それで作業が続く。エンジンが勝手に戻すことはしない。
    - 断るもの：`turn/start`、`command/run` でターンを始めるコマンド、`thread/queue/add`、`thread/queue/start`、並べたメッセージの次のターン、`/compact`。どれも `Core::begin_turn`（`thread/queue/add` はその前）で断る。並べたメッセージは消さずに残る。
    - 読むもの（`thread/read`、`thread/fork`、`thread/export`、`thread/diff`、`thread/subscribe`）はそのまま使える。
    - アーカイブの状態は store に残す（`SCHEMA_V18`：`sessions.archived`）。フォークしたスレッドはアーカイブされていない。
    - 限界：`command/run` でターンを始めるコマンドは、コマンドの処理（`/goal` の目標の設定など）を済ませてから、ターンの始めで断られる。その処理は残る。
  - すべての画面への通知：
    - `thread/status/subscribe` は名前を変えず、`status` に加えて、スレッドの一覧の変化も `thread_id` 付きで流す。
      - `thread_created`：`thread/start`、`/new`、`thread/fork`、`/fork`。フォークには `from`（元のスレッド）が付く。
      - `thread_renamed`（`title` 付き）：題が変わるたび。人が変えたときも、最初のメッセージから自動で付いたときも、モデルが付けたときも出る。store が題を付けるところ（`Store::titled`。`thread/titles/subscribe` の知らせと同じ所）から、ほかの変化と同じ流れに送るので、順番が入れ替わらない。
      - `thread_archived`・`thread_unarchived`：状態が変わったときだけ。すでにアーカイブしたスレッドをもう一度アーカイブしても出ない。
      - `thread_deleted`。
    - `thread/subscribe` には、そのスレッドの `thread_renamed`（`title`）、`thread_archived`、`thread_unarchived`、`thread_deleted` を `thread_id` 無しで流す。`thread_deleted` はその購読の最後のイベントで、購読はそこで終わる（それ以後 `cancel` しても `cancelled: false`）。
      - ターンの間に題が変わると、`thread/subscribe` には、ターンのイベントの `title_updated` と `thread_renamed` の両方が届く。
    - `thread/titles/subscribe` は今までどおり。
  - 範囲外：
    - 委任したメンバーのスレッドの作成（今も通知しない）。
    - アーカイブしたスレッドを、時間がたったら消すなどの片付け。
- **関連ソースコード：**
  - `core/src/commands.rs`：`Core::rename_thread`、`Core::delete_thread`、`Core::set_thread_archived`、`Core::export_thread`、`Core::search_threads`（`/history` もこれを使う）、`Core::threads`（`archived`）、`summary_view`、`/fork`・`/delete`。
  - `core/src/lib.rs`：`ThreadChange`、`Activity::Changed`、`EventPayload::thread_change`、`archived_thread`、`Core::begin_turn`、`Core::start_thread`、`Core::fork_thread`。
  - `core/src/queue.rs`：`Core::add_to_queue`。
  - `core/src/store.rs`：`SCHEMA_V18`、`titled`、`activity`（スレッドの動きの流れは store が持ち、`Core::with_store` がそれを使う）。
  - `core/src/store_threads.rs`：`ThreadSummary::archived`、`sessions`（`archived`）、`set_archived`、`is_archived`。
  - `server/src/lib.rs`：`thread_operation`、`list_threads`、`watch_threads`、`relay_threads`。
  - テスト：`server/tests/turn_tcp.rs` の `threads_are_renamed_exported_searched_archived_and_deleted_by_request`、`every_screen_hears_of_threads_created_renamed_archived_and_deleted`。
- **根拠の状態：** UNSUPPORTED_APPROVED。画面の操作と通知で、効き目を測る学術研究は当てはまらない。調べたのは、ほかのハーネスの実装だけ。
- **ユーザーの承認：** 2026-11-27。
  - 足りない機能の洗い出しで「急ぐ群から順に」と答えた。
  - 「⑪ スレッドのアーカイブ：アーカイブしたスレッドはどうなりますか？」に「戻すか聞いてから戻せばいいそして継続」と答えた。上の「実装した解釈」は、作業を頼んだ側の指示による。
  - 要求とイベントの名前と形、`thread/status/subscribe` に一覧の変化を流すこと、ターンの間はアーカイブを断ること、検索の上限 100 は、作業者（Claude）と作業を頼んだ側が決めた。

## ほかのハーネス（2026-11-27、作業を頼んだ側の報告。inocode の作業者は直接読んでいない）

| ハーネス | 操作 | 通知 |
|---|---|---|
| Codex | `thread/archive`、`thread/unarchive`、`thread/delete`、`thread/name/set`。TUI は、アーカイブしたスレッドを戻すかを聞く | `thread/archived`、`thread/unarchived`、`thread/deleted`、`thread/name/updated` |
| opencode | `PATCH` で `archived`、`DELETE` でセッションを消す | `session.archived`、`session.unarchived`、`session.deleted`、`session.renamed` |
| Claude Code | 名前の変更、続きから開くときの一覧 | 不明 |

## 検証

- テストで確かめたこと：
  - `thread/rename` で題が変わり、`thread/list` に出る。空白だけの題は `invalid_params`、無いスレッドは `unknown_thread`。
  - `thread/export` が、既定で Markdown を、`json` でメッセージの配列を返す。違う `format` は `invalid_params`、無いスレッドは `unknown_thread`。
  - `thread/search` が、人のメッセージの語でそのスレッドを見つける。`query` が無いとき、`limit` が 0 のときは `invalid_params`。
  - アーカイブしたスレッドは `thread/list` に出ず、`archived: true` で `archived: true` 付きで出る。`turn/start` は `thread_archived` で失敗し、`thread/queue/add` も `thread_archived`。`thread/read` はできる。戻すと一覧に戻り、`turn/start` が終わりまで走る。
  - ターンが人の答えを待っている間、`thread/delete` と `thread/archive` は `turn_running`。終わった後の `thread/delete` でスレッドが消え、`thread/read` も `thread/delete` も `unknown_thread` になる。
  - `thread/status/subscribe` に `thread_created`（`thread/start` では `from` 無し、フォークでは `from` 付き）、`thread_renamed`、`thread_archived`（2 回のアーカイブで 1 回）、`thread_unarchived`、`thread_deleted` の順に届く。
  - `thread/subscribe` に `thread_renamed`、`thread_archived`、`thread_unarchived`、`thread_deleted` が届き、その後の `cancel` は `cancelled: false`（購読が終わっている）。
- 確かめていないこと：
  - `command/run` でターンを始めるコマンドを、アーカイブしたスレッドで送ったとき。
  - アーカイブしたスレッドの並べたメッセージが、戻した後の `thread/queue/start` で送られること。
  - 自動で付いた題の `thread_renamed`。
