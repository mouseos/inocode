# プロジェクトの階層（グループ）

- **対象と範囲：** プロジェクトを、名前だけのグループに入れて、好きな深さの階層にする。例：`android` → `bootloader` → `z581kl`（フォルダのプロジェクト）→ いくつものチャット。
  - グループ（ユーザーの選択「名前だけのグループ」）：
    - ディスクのフォルダとは関係なく、inocode のデータベースの中だけにある入れ物。
    - グループの中にグループを入れられ、深さに上限は無い。同じ名前のグループがあってもよい（番号で区別する）。
  - プロジェクトは 1 つのグループにだけ入る（ユーザーの選択「1 つだけ」）。グループに入れないプロジェクトは一番上にある。プロジェクトのチャットは、プロジェクトのある所にある。
  - フォルダの無いチャットは、どのグループにも直接入れられる（ユーザーの選択「入れられる」）。フォルダのあるチャットは、自分ではグループに入れず、プロジェクトを動かす。
  - 中身（グループ・プロジェクト・フォルダの無いチャット）のあるグループは消せない（ユーザーの選択「中身があれば断る」、`group_not_empty`）。
  - 保存：スキーマ 19 で、表 `groups(id, name, parent_id, created)` と、`projects.group_id`・`sessions.group_id` を足した。
  - 要求（`group_id`・`parent_id` が `null` のときは一番上）：
    - `groups/list` → `{groups: [{group_id, name, parent_id}], projects: [{project, group_id}]}`。木の形にするのは画面の役目。
    - `groups/create {name, parent_id?}` → `{group_id}`。名前が空なら `invalid_params`。
    - `groups/rename {group_id, name}`。
    - `groups/move {group_id, parent_id}`：自分の中や、自分の下のグループの中には入れられない（`invalid_params`）。
    - `groups/delete {group_id}`。
    - `projects/group {project, group_id}`：`project` はプロジェクトのキー（フォルダのパス）。まだ知らないプロジェクトなら作る。
    - `thread/group {thread_id, group_id}`：フォルダの無いチャットだけ。フォルダのあるチャットは `invalid_params`。
    - `thread/start {group_id}`：フォルダの無いチャットを、グループの中に始める。`project` と一緒には渡せない。無いグループなら、作ったスレッドを消して `unknown_group`。
    - `thread/list` の各スレッドに `group_id`（プロジェクトのグループ、フォルダが無ければ自分のグループ）を足した。
    - 無いグループは `unknown_group`。
  - フォークしたチャットは、元のチャットのグループに入る。
  - 範囲外・限界：
    - グループの変化を、ほかの画面へは知らせない。
    - グループの並べ順は名前の順で、人が並べ替える仕組みは無い。
    - `thread/list` をグループで絞る引数は無い。
- **関連ソースコード：**
  - `core/src/store_groups.rs`：`Group`、`GroupedProject`、`Store::groups`・`create_group`・`rename_group`・`move_group`・`delete_group`・`group_project`・`group_thread`。
  - `core/src/store.rs`：`SCHEMA_V19`。
  - `core/src/store_threads.rs`：`ThreadSummary::group_id`、`SUMMARY_SELECT`、フォーク。
  - `core/src/lib.rs`：`Core::groups` ほか。
  - `core/src/commands.rs`：`summary_view`。
  - `server/src/lib.rs`：`group_request`、`start_thread`（`group_id`）。
  - テスト：`server/tests/commands_tcp.rs` の `projects_and_threads_sit_in_groups_nested_to_any_depth`。
- **根拠の状態：** UNSUPPORTED_APPROVED。整理のための機能で、効き目を測る学術研究は当てはまらない。
- **ユーザーの承認：** 2026-11-28。
  - 「あとはプロジェクト自体を階層化できるようにしてほしいandroid（任意階層）→bootloader（任意階層）→z581kl（フォルダ名）→いろんなチャット」と頼んだ。
  - 「上の段は何にしますか？」に「名前だけのグループ」、「フォルダの無いチャットを、グループに直接入れられるようにしますか？」に「入れられる」、「グループを消したとき、中身はどうしますか？」に「中身があれば断る」、「1 つのプロジェクトを、いくつのグループに入れられるようにしますか？」に「1 つだけ」と答えた。
  - 要求の名前と形、フォークの扱いは、作業者（Claude）が決めた。

## 検証

- テストで確かめたこと：
  - `android` の下に `bootloader` を作り、フォルダ `z581kl` のプロジェクトを `bootloader` に入れると、`groups/list` にその形で出る。
  - そのプロジェクトのチャットは `thread/list` で `bootloader` に、`group_id` を付けて始めたフォルダの無いチャットは `android` に入る。
  - 断られること：`android` を `bootloader` の中へ動かす（`invalid_params`）、中身のある `android` を消す（`group_not_empty`）、フォルダのあるチャットを直接グループへ動かす（`invalid_params`）。
  - 名前を変えられる。空にしたグループは消せる。消したグループに始めようとすると `unknown_group`。
- 確かめていないこと：フォークしたチャットのグループ、古いデータベースからの移行（スキーマの版の単体テストは通る）。
