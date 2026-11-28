# スレッドの作業フォルダの変更と、スレッドごとの worktree

- **対象と範囲：**
  - フォルダの変更：`thread/folder {thread_id, project}` とコマンド `/cd <folder>`。
    - スレッドを、絶対パスで名指した別の既存のフォルダのプロジェクトへ移す。会話はそのまま。
    - 次のターンから、コマンド・ファイルの道具・差分の写し・規則（AGENTS.md）・許可・課題は新しいフォルダのプロジェクトのものになる。システムの文がプロジェクトのフォルダを名指すので、モデルには次のターンの最初の要求で伝わる。
    - ターンが動いている間は `turn_running` で断る。相対パスや無いフォルダは `invalid_params`。
  - worktree：`thread/worktree {thread_id, branch?, base?}` とコマンド `/worktree [branch]`。
    - スレッドの今のプロジェクトのフォルダの git リポジトリから、新しいブランチ（既定は `inocode/thread-<番号>`）の worktree を、`base`（既定はリポジトリの `HEAD`）から作り、スレッドをそこへ移す。
    - 置き場所は inocode のデータの横：`<データベース>.files/worktrees/thread-<番号>`（ユーザーの選択）。リポジトリの中には作らない。
    - git は、状態を読むときと同じ守り（フック・fsmonitor・全体と system の設定を読まない）で走らせる。そのため worktree を作るときに post-checkout のフックは走らない。
    - そのスレッドにもう worktree があれば `already_exists`。git が断れば `worktree_failed`（git の文をそのまま返す）。
  - 片付け（ユーザーの選択「削除で消す」）：
    - スレッドを削除したとき、そのスレッドのフォルダが自分の worktree なら、変更もgit が追っていないファイルも無いときだけ `git worktree remove` で消す。ブランチは残す。
    - 残したときは、削除の答えの `worktree` に `{path, removed: false, kept_because}` を入れる。消したときは `{path, removed: true}`。worktree でないスレッドでは `null`。
    - アーカイブでは消さない。
  - 範囲外・限界：
    - サンドボックスの中のコマンドは、worktree のフォルダの外に書けない。worktree の git の情報は元のリポジトリの `.git` にあるので、サンドボックスの中からの `git commit` などは失敗する。サンドボックスの外で走らせる承認が要る。
    - `thread/start` で直接 worktree を作る引数は無い（`thread/start` の後に `thread/worktree`）。
    - 移した前後の会話の中の古いパスを、モデルが使い続けることはある。
    - Claude Code の `.worktreeinclude`（追跡していないファイルを写す）は作っていない。
- **関連ソースコード：**
  - `core/src/worktree.rs`：`place`、`default_branch`、`add`、`remove_if_clean`。
  - `core/src/git.rs`：`git`（`pub(crate)` にした）。
  - `core/src/lib.rs`：`Core::move_thread`・`add_worktree`・`refuse_while_running`。
  - `core/src/store.rs`：`set_session_project`。
  - `core/src/commands.rs`：`/cd`・`/worktree`、`delete_thread`（片付け）。
  - `server/src/lib.rs`：`folder_request`。
  - テスト：`server/tests/commands_tcp.rs` の `a_thread_moves_to_another_folder_and_to_its_own_worktree_removed_with_it`。
- **根拠の状態：** UNSUPPORTED_APPROVED。作業の場所の管理で、効き目を測る学術研究は当てはまらない。調べたのは、ほかのハーネスの実装だけ。
- **ユーザーの承認：** 2026-11-28。
  - 「次の群（あると便利な機能）に進みますか？」に「次の群へ進む」と答えた。
  - 「スレッドの作業フォルダについて、何を作りますか？」に「変更と worktree 両方」と答えた。
  - 「スレッドごとの git の worktree はどこに作り、いつ片付けますか？」に「inocode の保管場所・削除で消す」（スレッドを削除したとき、未保存の変更が無ければ一緒に消す。ブランチは残す）と答えた。
  - ターンの間は断ること、ブランチの既定の名前、要求とコマンドの名前は、作業者（Claude）が決めた。

## ほかのハーネス（2026-11-28、作業者が頼んだ調べものの報告）

| ハーネス | フォルダと worktree |
|---|---|
| Claude Code | `--worktree <name>` で `.claude/worktrees/<name>`（ブランチ `worktree-<name>`）。`/cd` で会話を保ったまま移る。`/add-dir`。`EnterWorktree`・`ExitWorktree` の道具 |
| Codex | TUI の `/worktree`・`/cd`・`/pwd`、`--worktree`。アプリはスレッドごとの worktree を、アーカイブや上限で消す。app-server の `thread/settings/update` の `cwd` |

## 検証

- テストで確かめたこと（Windows、本物の git）：
  - コミットのあるリポジトリのスレッドで `thread/worktree` を呼ぶと、`inocode/thread-<番号>` のブランチと、ファイルのある worktree ができ、`files/search` がそこから探す。2 度目は `already_exists`。
  - `thread/folder` で別のフォルダへ移れ、相対パスは `invalid_params`、`/cd` で戻れる。
  - 削除すると worktree が消え、ブランチは残る。
  - `/worktree feature-x` で名前つきのブランチになり、追跡していないファイルがあると、削除しても worktree が残り `removed: false` が返る。
- 確かめていないこと：ターンの中で移った先のフォルダでコマンドが走ること、macOS と Linux（CI で確かめる）。
