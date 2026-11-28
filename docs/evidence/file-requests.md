# 画面向けのファイルの要求（`fs/*`）

- **対象と範囲：**
  - 画面が、ファイルを絶対パスで読む・書く・一覧する・作る・消す・写すための要求。Codex の app-server の `fs/*` と同じ一式で、名前も合わせた。パスは絶対パスだけ受け取り、相対パスは `invalid_params`。
  - どのフォルダでも扱える（スレッドのプロジェクトに限らない）。サーバーにつながる画面は、人のコマンド（`person-shell.md`）でも何でもできるので、ここでも制限を置かない。リモートの接続（パスワードつきの WebSocket）からも同じに使える。
  - 要求：
    - `fs/readFile {path, offset?, limit?, encoding?}` → `{size, offset, length, text}`（`encoding` が `base64` なら `text` の代わりに `data`）。`offset`・`limit` はバイト。無ければファイル全体。1 回で 16 MiB（`files::READ_MAX`）まで、超えれば `too_large`。UTF-8 として読めなければ `not_text`。
    - `fs/writeFile {path, text | data, create_parents?}` → `{written}`。ファイル全体を書く。`data` は base64。`create_parents` で足りないフォルダを作る。
    - `fs/createDirectory {path, recursive?}`：`recursive` は既定で `true`（途中のフォルダも作る）。
    - `fs/getMetadata {path}` → `{kind, size, modified_ms, readonly}`。`kind` は `file`・`directory`・`symlink`・`other`。リンクはたどらない。
    - `fs/readDirectory {path}` → `{entries: [{name, kind, size, modified_ms, readonly}]}`。名前の順。
    - `fs/remove {path, recursive?}`：ファイルかリンクを消す。フォルダは空のものだけ、`recursive` で中身ごと。リンクは、指す先でなくリンクを消す。
    - `fs/copy {from, to, recursive?, overwrite?}`：ファイルを写す。フォルダは `recursive` のときだけ中身ごと写す。写し先があれば `overwrite` が無い限り `already_exists`。写し先が写す元の中なら `invalid_params`。リンクは指す先のファイルとして写す。
  - エラーの番号：`not_found`・`already_exists`・`permission_denied`、そのほかの失敗は `fs_failed`。
  - 変更の監視：
    - `fs/watch {path, recursive?}`（流し続ける要求）：OS の変更の通知を受け取り、`started`（`result: {path}`）の後、変更ごとに `changed`（`result: {kind, paths}`）を送る。`kind` は `created`・`modified`・`renamed`・`removed`・`other`。読み取り（アクセス）の通知は送らない。`recursive` でフォルダの下の全部を見る（既定は見ない）。
    - 止めるのは `cancel`、または `fs/unwatch {watch_id}`（`watch_id` は `fs/watch` の要求の番号。同じ接続だけ）→ `{unwatched}`。止めると `cancelled` で終わる。
    - 画面が読む前に 1,024 個を超えてたまった変更は捨て、次の変更の前に `lagged` を送る。
    - 無いパスは `not_found`。
    - Codex の `fs/changed` は通知の名前だが、ここでは要求の流れの中の `changed` イベントとして届く。
    - 外の部品 `notify`（8 系）を足した（ユーザーの選択「OS の通知を使う」）。Windows では ReadDirectoryChangesW、macOS では FSEvents、Linux では inotify を使う。同じファイルの続けての変更を 1 つにまとめることはしない。
  - 範囲外・限界：
    - 書き込みは、別の名前に書いてから置き換える方法をとらない。書いている途中で失敗すると、ファイルが途中まで書かれた状態になる。
    - 大きなフォルダの写し・削除は、終わるまで応答しない。
- **関連ソースコード：**
  - `core/src/files.rs`：`absolute`、`read_file`、`write_file`、`create_directory`、`metadata`、`read_directory`、`remove`、`copy`、`watch`・`Watch`・`Change`・`CHANGES_KEPT`。
  - `core/Cargo.toml`：`notify = "8"`。
  - `server/src/lib.rs`：`file_request`、`watch_path`、`unwatch`。
  - テスト：`server/tests/commands_tcp.rs` の `a_screen_reads_writes_lists_copies_and_deletes_files_by_absolute_path`、`a_watched_folder_reports_its_changes_until_it_is_unwatched`。
- **根拠の状態：** UNSUPPORTED_APPROVED。画面の操作で、効き目を測る学術研究は当てはまらない。調べたのは、ほかのハーネスの実装だけ。
- **ユーザーの承認：** 2026-11-28。
  - 「次の群（あると便利な機能）に進みますか？」に「次の群へ進む」と答えた。
  - 「画面向けのファイルの API は、どこまで作りますか？」に「Codex と同じ一式」（一覧・読む・書く・フォルダ作成・削除・コピー・情報・変更の監視）と答えた。
  - 「ファイルの変更の監視（画面向け）をどう作りますか？」に「OS の通知を使う」（外の部品 notify を新しく足す）と答えた。
  - 絶対パスだけを受け取り、場所を制限しないこと、上限、引数とエラーの名前は、作業者（Claude）が決めた。

## ほかのハーネス（2026-11-28、作業者が頼んだ調べものの報告）

| ハーネス | ファイルの要求 |
|---|---|
| Codex（app-server） | `fs/readFile`・`fs/writeFile`・`fs/createDirectory`・`fs/getMetadata`・`fs/readDirectory`・`fs/remove`・`fs/copy`・`fs/watch`・`fs/unwatch`、通知 `fs/changed`。絶対パスを取る |

## 検証

- テストで確かめたこと：
  - 足りないフォルダごと文字を書き、全体と一部（base64）を読める。
  - 文字でないファイルは `not_text`。
  - フォルダの一覧が名前の順で、種類と大きさが入る。情報に `modified_ms` が入る。
  - フォルダを `recursive` 無しで写すと `invalid_params`、有りで中身ごと写る。写し先があると `already_exists`。
  - 中身のあるフォルダを `recursive` 無しで消すと失敗し、有りで消える。
  - 無いパスは `not_found`、相対パスは `invalid_params`。
  - フォルダの下を見ていると、中のフォルダにファイルを作った `changed` が届き、`fs/unwatch` で `unwatched: true` が返って `cancelled` で終わる。無いパスは `not_found`。
- 確かめていないこと：リンクの扱い、権限の無いファイル、`lagged`、macOS と Linux（CI で確かめる）。
