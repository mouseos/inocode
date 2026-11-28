# 入力欄から走らせる人のコマンド（`!コマンド`）

- **対象と範囲：**
  - `turn/start {thread_id, shell}`：`shell` の文字列を、スレッドのプロジェクトのフォルダでコマンドとして走らせ、終わったら、そのコマンドと出力を人のメッセージとして会話に入れ、モデルが返事する（普通のターンと同じ流れ）。入力欄の `!` で始まる文をこの要求に変えるのは、画面の役目。
  - `shell` と一緒に `prompt`・`reply_to`・`attachments`・`mentions` は渡せない（`invalid_params`）。承認のモード・`plan` などは普通の `turn/start` と同じに受け取る。
  - コマンドは人が自分で打ったものなので、承認を聞かず、サンドボックスの外で走らせる（Claude Code の `!` と同じ）。モデルの `shell` と同じ端末で走り、スレッドのジョブの一覧に入る（`background-jobs.md`）。
  - コマンドが始まると、ターンのイベント `shell_started`（`result: {job_id}`）を送る。画面は `thread/jobs/watch` でその出力を見られ、`thread/jobs/write` で入力できる。
  - 終わるまで待つ（時間の上限は無い）。人のメッセージは次の形：
    - `I ran this command myself in the project folder, outside the sandbox:` の後に `$ コマンド` の行、その後にモデルの `shell` がジョブの終わりで見るのと同じ文（終わりの番号、かかった秒、ログの場所、出力の最初と最後の 20 行ずつ、1 行 500 字まで）。
  - コマンドの途中でターンを止めると（`turn/interrupt`・`cancel`）、コマンドを止め、終わりを 5 秒まで待ち、その時の文に `I stopped it before it ended.` を付けて人のメッセージとして残し、モデルには聞かずにターンを終える。
  - プロジェクトの無いスレッドでは `shell_failed` で失敗する。
  - 範囲外・限界：
    - 出力は最初と最後の 20 行ずつだけが会話に入る。全部はログにあり、モデルはログを読める。
    - 止めたときに 5 秒で終わらなかったコマンドは、動いている画面の文になる。
- **関連ソースコード：**
  - `core/src/shell.rs`：`PersonJob`（`start`・`id`・`finish`）。
  - `core/src/turn.rs`：`Prompt::shell`、`TurnEvent::ShellStarted`、`Thread::run_for_person`、`steps`。
  - `core/src/lib.rs`：`EventPayload::shell_started`。
  - `server/src/lib.rs`：`person_shell`、`start_turn`、`run_turn`、`turn_event`。
  - テスト：`server/tests/turn_tcp.rs` の `the_persons_command_runs_and_its_output_is_the_prompt_the_model_answers`、`stopping_the_turn_stops_the_persons_command_and_the_model_is_not_asked`。
- **根拠の状態：** UNSUPPORTED_APPROVED。人が自分のコマンドを走らせる操作で、効き目を測る学術研究は当てはまらない。調べたのは、ほかのハーネスの実装だけ。
- **ユーザーの承認：** 2026-11-28。
  - 「次の群（あると便利な機能）に進みますか？」に「次の群へ進む」と答えた。
  - 「入力欄で「!コマンド」と打ってシェルを直接走らせる機能を作るとき、その出力を会話に入れますか？」に「入れて、モデルが返事する」（Claude Code と同じ。出力を見てモデルがすぐ返事する）と答えた。
  - `turn/start` の引数にすること、サンドボックスの外で承認なしに走らせること、時間の上限を置かないこと、止めたときの扱い、文の形は、作業者（Claude）が決めた。

## ほかのハーネス（2026-11-28、作業者が頼んだ調べものの報告）

| ハーネス | 入力欄からのコマンド |
|---|---|
| Claude Code | `!` で始めると、コマンドと出力を会話に入れ、モデルが返事する（`respondToBashCommands: false` で返事を止められる）。サンドボックスの外で走る |
| Codex | 出力を会話に記録する（`core/src/tasks/user_shell.rs`）。app-server の `thread/shellCommand` は、サンドボックスの外・時間の上限 1 時間。モデルが返事するかは確かめられなかった |

## 検証

- テストで確かめたこと（Windows）：
  - `echo` のコマンドで `shell_started`（ジョブ 1）が届き、人のメッセージにコマンドと終わりの番号 0 と出力が入り、モデルへの要求にも出力が入り、モデルの返事でターンが終わる。
  - `shell` と `prompt` を一緒に渡すと `invalid_params`。
  - 眠るコマンドの途中でターンを止めると `cancelled` で終わり、止めたことを書いた人のメッセージが残り、モデルには何も送られず、ジョブは終わっている。
- 確かめていないこと：macOS と Linux（CI で確かめる）、本物のモデルでの返事。
