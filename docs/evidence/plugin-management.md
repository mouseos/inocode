# プラグインの削除・無効化と、画面用の要求

- **対象と範囲：**
  - 入れたプラグイン（`plugin-mechanism.md`）を、消さずに切ること（無効化）、戻すこと（有効化）、消すこと（削除）。プラグインは `名前@カタログ` で名指す。
  - 無効化：
    - 無効にしたものの名前を、プラグインのフォルダの `disabled.json`（名前の一覧）に書く。書くときは、別の名前で書いてから置き換える。
    - 無効のプラグインは、スキル・MCP サーバー・コマンドのどれにも出ない（これらを集める `Installed::all` が飛ばす）。ファイルは残る。
    - 切り替えると MCP の記憶を捨て（`forget_mcp`）、次の要求から反映する。
    - 無効のまま入れ直したプラグインは、無効のまま。
  - 削除：入れたフォルダを消し、無効の一覧からも外す。MCP の記憶を捨てる。
  - 要求（どれも流し続けない普通の要求）：
    - `plugins/list` → `{plugins: [{id, name, catalog, enabled, path, skills, servers, commands}]}`。無効のものも入る。
    - `plugins/search {query}`：カタログを探す（今までの `/plugins search` と同じ結果）。
    - `plugins/install {plugin}`：人が頼んだものとして、モデルの `plugin_install` のような承認は聞かずに入れる（今までの `/plugins install` と同じ）。
    - `plugins/remove {plugin}` → `{removed}`。
    - `plugins/enable {plugin}`・`plugins/disable {plugin}` → `{plugin, enabled, changed}`。`changed` は、もうその状態だったとき `false`。
    - 入っていないプラグイン、`名前@カタログ` の形でない名前は、エラーの番号 `plugins`。引数が無ければ `invalid_params`。
  - コマンド：`/plugins remove`・`/plugins disable`・`/plugins enable`（どれも `名前@カタログ`）を足した。`/plugins list` は無効のものも `enabled: false` で出すようになった。
  - 範囲外・限界：
    - 動いている MCP サーバーの接続を、その場で切るかどうかは確かめていない（次の要求で一覧から消える）。
    - 変えたことを、ほかの画面へは知らせない。
- **関連ソースコード：**
  - `core/src/plugins.rs`：`DISABLED_FILE`、`disabled`、`write_disabled`、`installed_id`、`remove`、`set_enabled`、`listing`、`Installed::all`・`every`・`id`。
  - `core/src/lib.rs`：`Core::plugins`・`search_plugins`・`install_plugin`・`remove_plugin`・`enable_plugin`。
  - `core/src/commands.rs`：`/plugins` の `remove`・`disable`・`enable`。
  - `server/src/lib.rs`：`plugin_request`。
  - テスト：`server/tests/commands_tcp.rs` の `an_installed_plugin_is_turned_off_on_and_deleted_from_a_screen`。
- **根拠の状態：** UNSUPPORTED_APPROVED。管理の操作で、効き目を測る学術研究は当てはまらない。調べたのは、ほかのハーネスの実装だけ。
- **ユーザーの承認：** 2026-11-28。
  - 「次の群（あると便利な機能）に進みますか？」に「次の群へ進む」と答えた。
  - 「プラグインで、何を足しますか？」に「削除・無効化・画面用の要求」（削除と、消さずに切る・戻すを作る。画面から一覧・入れる・削除・切り替えができる専用の要求も作る）と答えた。
  - 無効の一覧をファイルに置くこと、要求とエラーの名前と形は、作業者（Claude）が決めた。

## ほかのハーネス（2026-11-28、作業者が頼んだ調べものの報告）

| ハーネス | プラグインの管理 |
|---|---|
| Claude Code | `/plugin`、`claude plugin install・uninstall・enable・disable・update`（範囲は user・project・local）、`/reload-plugins` |
| Codex（TUI） | `/plugins` の一覧で入れる・Space で切り替える・消す。使えるのは次のセッションから |
| Codex（app-server） | `plugin/list`・`search`・`installed`・`read`・`install`・`uninstall` など（入れる・消すは「作成中」と書かれている） |

## 検証

- テストで確かめたこと：スキルを 1 つ持つプラグインを置くと、`plugins/list` に `enabled: true` で出て、`/skills` に出る。無効にすると `changed: true`、2 度目は `false`、一覧で `enabled: false`、`/skills` から消え、ファイルは残る。`/plugins enable` で戻る。削除するとフォルダが消え、一覧が空になり、2 度目の削除はエラー。引数が無ければ `invalid_params`。
- 確かめていないこと：MCP サーバーを持つプラグインでの切り替え。
