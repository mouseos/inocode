# ターンごとの変更の差分

- **対象と範囲：**
  - 差分は、ターンの前にとったプロジェクトのフォルダの写し（`file-snapshots.md`）と、今のファイルを比べる。シェルで変えたファイルも入る。`.gitignore` で外したファイルは入らない。
    - 今のファイルの写しを、ターンの前の写しと同じ方法（inocode 専用の git の保管場所で `add -A` と `write-tree`、写しの鍵を持ったまま）でとり、2 つの写しの間を `git diff-tree` で比べる。
    - 形：`{seq, files, text, truncated}`。
      - `seq`：写しを記録した番号（そのターンの最初の人のメッセージの番号）。
      - `files`：ファイルごとに `{path, status, added, removed}`。`status` は `added`・`modified`・`deleted`。名前の変更は、消したファイルと足したファイルとして出す。`added`・`removed` は足した行と消した行の数で、バイナリのファイルでは `null`。
      - `text`：unified diff の文字列。
      - `truncated`：`text` を切ったか。
    - `text` は 256 KiB（262,144 バイト）までにし、超えた分は、その前の行の終わりで切る（`snapshot::DIFF_TEXT_MAX`）。`files` は切らない。
  - 作業中の更新（ユーザーの選択「作業中も更新」）：
    - ファイルを変えたかもしれない道具の結果が保存されるたびに、差分を作り直す。変えないと分かっている道具（`read`・`find`・`explore`・`fetch`・`search`・`tool_search`・`plugin_search`・`plan`・`recall`・`lesson`・`method`・`issue`・`ask`・`title`・`promise`）の後には作らない。それ以外（`edit`・`edits`・`write`・`shell`・`job`・`plugin_install`・MCP の道具・そのほか全部）の後には作る。
    - 前に送ったものと違うときだけ、ターンのイベント `diff`（`{event: "diff", diff: {seq, files, text, truncated}}`）を送る。まだ何も送っていないときは、変わったファイルが無ければ送らない。そのため、1 回の呼び出しで作って消したファイルなど、変化が残らない変更ではイベントが出ない。
    - ターンの呼び出しが全部終わった後にもう一度作り、違えば送る。バックグラウンドのジョブや人が変えた分も、ここで入る。
    - `diff` はターンを始めた接続と、`thread/subscribe` の両方に届く（ほかのターンのイベントと同じ道を通る）。
    - プロジェクトの無いスレッドと、ターンの前の写しがとれなかったターンでは、差分を作らない。
  - 要求 `thread/diff {thread_id, seq?}`：
    - `seq` が無ければ、最新のターンの写しと今のファイルを比べる。
    - `seq` があれば、その番号のメッセージが来たターン（番号がそれ以下で最大の写し）の写しと、次のターンの写しを比べる。次のターンが無ければ、今のファイルと比べる。
    - 応答は上の形に `thread_id` を付けたもの。
    - 写しが無いとき（プロジェクトの無いスレッドなど）は `no_snapshot`、無いスレッドは `unknown_thread`、`seq` が数でなければ `invalid_params`。
  - 費用：
    - 差分を作るたびに、プロジェクトのフォルダ全体に `git add -A` をかけ、`git diff-tree` を最大 3 回走らせる。大きなフォルダでは時間がかかる。
    - 作業中の差分はターンとは別のタスクで作るので、モデルへの次の要求は待たない。作っている間に通知が重なっても、続けて 1 回作り直すだけにまとめる。
    - 最後の差分だけは、ターンの終わりを知らせる前に作るので、そのぶんターンの終わりが遅れる。
    - 写しを作る鍵はすべてのプロジェクトで 1 つなので、ほかのスレッドの写しや差分と順番待ちになる。
  - 範囲外・限界：
    - 道具が走っている途中に差分を作ると、書きかけの状態が入ることがある。次の差分か最後の差分で直る。
    - 作れなかった差分は飛ばし、知らせない。
    - `seq` が人のメッセージかどうかは調べない。写しがとれなかったターンは飛ばして、前後の写しで比べる。
    - 文字コードの判定はしない。`text` は UTF-8 として読めない部分を置き換える。
- **関連ソースコード：**
  - `core/src/snapshot.rs`：`Diff`、`ChangedFile`、`DIFF_TEXT_MAX`、`diff`、`diff_trees`。
  - `core/src/lib.rs`：`Core::take_snapshot`（`TurnCopy` を返す）、`Core::run_claimed`（`send_diffs` を走らせ、道具の結果で知らせる）、`leaves_files`、`diff_value`、`Core::thread_diff`。
  - `core/src/turn.rs`：`TurnEvent::Diff`。
  - `core/src/store.rs`：`add_snapshot`（番号を返す）、`snapshot_span`。
  - `server/src/lib.rs`：`thread_operation`（`thread/diff`）、`turn_event`、`EventPayload::diff`（`core/src/lib.rs`）。
  - テスト：`core/src/snapshot.rs` の `a_diff_lists_each_changed_file_and_cuts_a_long_text`、`server/tests/turn_tcp.rs` の `a_turns_diff_is_sent_while_it_works_and_can_be_asked_for_later`、`a_thread_without_a_project_has_no_diff`。
- **根拠の状態：** UNSUPPORTED_APPROVED。画面の表示で、効き目を測る学術研究は当てはまらない。調べたのは、ほかのハーネスの実装だけ。
- **ユーザーの承認：** 2026-11-27。
  - 足りない機能の洗い出しで「急ぐ群から順に」と答えた。
  - 「⑨ ターンごとの変更の差分…を、いつ画面に出せるようにしますか？」に「作業中も更新（ファイルを変えたであろう道具の後ごとに差分を送り直す。Codex と同じ）」と答えた。
  - 写しを比べる方法、変えないと分かっている道具の一覧、256 KiB の上限、イベントと要求の名前と形は、作業者（Claude）と作業を頼んだ側が決めた。

## ほかのハーネス（2026-11-27、作業を頼んだ側の報告。inocode の作業者は直接読んでいない）

| ハーネス | ターンの差分 |
|---|---|
| Codex | `turn/diff/updated`：ターンの変更をまとめた unified diff を、作業中に送り直す |
| opencode | `GET /api/session/:id/diff`：ターンごと、または範囲の差分 |
| Claude Code | `/diff` |

## 検証

- テストで確かめたこと：
  - 写しの後に変えたファイル、消したファイル、増やしたファイル（バイナリ）が、正しい `status` と行数で出る。長い差分は 256 KiB 以下の行の終わりで切れ、`truncated` が付く。
  - `write` でファイルを変え、シェルで別のファイルを作るターンで、`diff` が届き、続けて同じものは送られない。最後の `diff` に両方のファイルが `modified`・`added` で入り、`seq` が人のメッセージの番号になる。`thread/subscribe` にも同じ `diff` が届く。
  - `thread/diff` で最新のターンを聞くと、最後の `diff` と同じものが返る。
  - シェルの 1 回の呼び出しでファイルを作って消すターンでは、`diff` が出ず、`thread/diff` の `files` が空になる。
  - 前のターンの `seq` を付けると、そのターンの写しから次のターンの写しまでの差分が返る。
  - プロジェクトの無いスレッドでは `diff` が出ず、`thread/diff` は `no_snapshot`。無いスレッドは `unknown_thread`、数でない `seq` は `invalid_params`。
- 確かめていないこと：
  - 大きなリポジトリでかかる時間。
  - MCP の道具、ジョブへの入力、プラグインの導入の後の差分（同じ道を通るが、試験はシェルと `write` だけ）。
  - 差分が作れないときの動き。
