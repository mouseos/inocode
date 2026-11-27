# ファイルの変更を元に戻す（ターンの前の写し）

- **対象と範囲：**
  - ターンの前に、スレッドのプロジェクトのフォルダの写しをとる。写しは store の横の `snapshots/project-<id>` にある、inocode 専用の git の保管場所に置く。
    - プロジェクトの `.gitignore` で外したファイルは写さない。
    - プロジェクト自身の `.git` は使わない。
    - 写しの記録は `snapshots` の表に、そのターンの最初のメッセージの番号と一緒に残す。
    - 写しがとれなかったときは、ターンを止めず、`completed` の `warnings` にそう書く。
    - プロジェクトの無いスレッドでは、写しをとらない。
  - `thread/rewind` に `restore` を足した。値は `conversation`（既定、今までどおり）、`files`、`both`。
    - `files`：会話は残し、ファイルを、そのメッセージが来たターンの前の写しに戻す。
    - `both`：ファイルを戻してから、会話を巻き戻す。
    - 戻すとき、写しの後で変わったファイルと消えたファイルは、写しのとおりに書き直す。写しの後で増えたファイルは消す。これは、シェルや人が変えたファイルも同じ（ユーザーの選択「シェルの変更も含める」）。
    - 返す値に `files_restored: true` と `files_deleted`（消したファイルの数）が付く。写しが無ければ `no_snapshot` で断る。
  - 巻き戻しでメッセージを消すと、そのメッセージから後ろの写しの記録も消す。フォークは、切った位置までの写しの記録を写す。スレッドを消すと、その記録も消す。
  - 範囲外・限界：
    - 写しは git の保管場所に残り続け、古いものを片付ける仕組みはない。
    - プロジェクトの中の別の git の保管場所（サブモジュールなど）の中身は写さない。
    - 大きなフォルダでは、ターンの前の写しに時間がかかる。1 段階 120 秒で打ち切る。
    - `through` などで steer のメッセージを選んだときも、戻す先はそのターンの始めの写しになる。
- **関連ソースコード：**
  - `core/src/snapshot.rs`：`take`、`restore`。git の設定は、プロジェクトの状態を読むとき（`git-context.md`）と同じく、フックとシステム・全体の設定を使わない。改行の変換もしない。
  - `core/src/lib.rs`：`Core::take_snapshot`（`run_claimed` から）、`Core::restore_files`、`Restore`、`Core::rewind_thread`。
  - `core/src/store.rs`：`SCHEMA_V15`、`add_snapshot`、`snapshot_before`。
  - `core/src/store_threads.rs`：`rewind_session`、`fork_session_through`、`delete_session`。
  - `server/src/lib.rs`：`rewind_thread`（`restore` の読み取り）。
  - テスト：`core/src/snapshot.rs` の `a_copy_puts_back_changed_deleted_and_added_files`、`server/tests/turn_tcp.rs` の `a_rewind_can_put_the_projects_files_back_as_they_were_before_the_turn`。
- **根拠の状態：** UNSUPPORTED_APPROVED。画面の操作で、効き目を測る学術研究は当てはまらない。調べたのは、ほかのハーネスの実装だけ。
- **ユーザーの承認：** 2026-11-27。
  - 足りない機能の洗い出しで「急ぐ群から順に」と答えた。
  - 「③ ファイルの変更を元に戻す：どこまで戻せるようにしますか？」に「シェルの変更も含める（ターンの前に作業フォルダの写しを別の git の保管場所にとる。opencode と同じ）」と答えた。
  - 「③ ファイルを戻す操作」に「会話だけ／両方／ファイルだけ（Claude Code と同じ）」と答えた。
  - 写しの置き場所、120 秒の打ち切り、写しがとれないときもターンを止めないことは、作業者（Claude）が決めた。

## ほかのハーネス（2026-11-27、調査用のエージェントがソースと資料を読み、報告。inocode の作業者は直接読んでいない）

| ハーネス | ファイルを戻す |
|---|---|
| opencode（49b9463） | セッションの写しを git でとる。`revert/stage {messageID, files?}` でファイルも戻せる |
| Claude Code（公式の資料 `checkpointing`） | 人の発言ごとにチェックポイントをとる。`/rewind` で会話だけ、コードだけ、両方を選べる。シェルで変えたファイルと、外で変えたファイルは追わない。ファイルの写しは最後の 100 個までを、約 30 日残す |
| Codex（openai/codex bee5dc55ac） | 前はあった（ghost commit の `/undo`）が、外した。巻き戻しはファイルを戻さない |

## 検証

- テストで確かめたこと：
  - 写しの後に変えたファイル、消したファイル、増やしたファイルが、写しのとおりに戻る。CRLF の改行もそのまま戻る。
  - `.gitignore` で外したファイルは触らない。
  - 実際のターン（`write` で既存のファイルを変え、新しいファイルを作る）の後に、`files` で戻すと会話は残る。
  - `both` で戻すと、人が手で変えた分も戻り、会話も巻き戻る。
  - 写しが無いとき（プロジェクトの無いスレッド）は `no_snapshot`、`restore` の値が違うときは `invalid_params` で断る。
- 確かめていないこと：
  - 大きなリポジトリでかかる時間と容量。
  - git の無い環境での警告の出方。
