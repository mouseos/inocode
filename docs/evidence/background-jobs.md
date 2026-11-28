# 画面から見る・操作する、スレッドのコマンド（裏で動くシェル）

- **対象と範囲：**
  - モデルの `shell` の呼び出しは、どれも 1 つのジョブとして、スレッドごとの一覧に入る（今までどおり）。`wait_seconds` を過ぎても動いているものが、裏で動くジョブ。この機能は、その一覧を画面から見て操作できるようにする。
  - 要求（どれも `thread_id` を取る。`job_id` はジョブの番号）：
    - `thread/jobs/list` → `{jobs: [...]}`。古い順。各ジョブは `{id, command, running, exit_code, duration_ms, log}`。`exit_code` は終わるまで `null`。`duration_ms` は動いた時間か、今までの時間。`log` はログのファイルの場所。
    - `thread/jobs/read {job_id, offset?, limit?}` → `{job, text, offset, next_offset, size}`。ログ（端末の制御の文字を除いたもの）の `offset` バイト目から、`limit` バイト（既定 64 KiB、1 から 1 MiB まで）を返す。終わりで文字が切れるときは、その文字の前で止め、`next_offset` をそこにする。`size` は読んだときのログの大きさ。
    - `thread/jobs/write {job_id, input?, keys?}` → `{written}`。動いているジョブに文字とキーを送る。キーの名前はモデルの `job` の道具と同じ（`Enter`・`Tab`・`Escape`・`Up`・`Ctrl+C` など）。`written` は送ったバイト数。どちらも無いときは `invalid_params`、終わったジョブは `job_failed`。
    - `thread/jobs/stop {job_id}` → `{stopped}`。動いていれば止めて `true`、もう終わっていれば `false`。
    - `thread/jobs/watch {job_id}`（流し続ける要求）：
      - `started`（`result.job` にジョブ）、`screen`（`text` に、今の画面を描く端末の文字。制御の文字を含む）、その後 `output`（`text` に、端末への出力をそのまま。制御の文字を含む）が来るたびに 1 つ。
      - ジョブが終わると、残りの出力の後に `job_ended`（`result: {exit_code, duration_ms}`）と `completed`。終わったジョブを見ると、`screen` の後すぐ `job_ended` が来る。
      - 出力は、画面を読んで購読するまでの間、画面を書き換える鍵を持ったままにするので、抜けも重なりも出ない。
      - ジョブが 1024 個の出力の塊より先に進んで追いつけないと、`lagged` が来て、その分の出力は届かない。ログには全部残る。
      - 文字の途中で切れた出力は、残りが来るまで送らない。
      - `cancel` で止められる。
    - 無いスレッドは `unknown_thread`、そのスレッドに無いジョブの番号は `unknown_job`。
  - スレッドの購読（`thread/subscribe`）：ジョブが始まったときと終わったときに、`jobs`（`result: {jobs: [...]}`、一覧の全体）を送る。購読を始めたときは、ジョブがあれば、状態の後に一覧を送る。全部のスレッドの状態の購読（`thread/status/subscribe`）には送らない。
  - モデルの側は今までどおり。画面から止めたジョブや入力したジョブも、モデルには次の要求の状態の文で「終わった」と見える。
  - 範囲外・限界：
    - ジョブの一覧はサーバーのプロセスの中にだけあり、サーバーを止めると消える（今までどおり）。ログのファイルは残る。
    - 端末の大きさは 40 行 × 120 桁のままで、画面からは変えられない。
    - Windows で止めるとき：portable-pty 0.9.0 は、止めるのに成功すると失敗と返し、失敗すると成功と返すので、Windows では結果を見ずに `true` を返す。モデルの `job` の道具の `stop` はこの扱いをしていないので、Windows では止まってもエラーが返る（別の不具合として報告。直していない）。
- **関連ソースコード：**
  - `core/src/shell.rs`：`JobInfo`、`JobLog`、`JobWatch`、`list`、`read_log`、`complete_utf8`、`write_to`、`typed`、`stop_job`、`watch_job`、`announce`、`Job::output`。
  - `core/src/lib.rs`：`Activity::Jobs`、`Core::jobs`・`job_log`・`write_job`・`stop_job`・`watch_job`、`EventPayload::jobs`。
  - `core/src/agent_tools.rs`：`place`（`activity`）。
  - `server/src/lib.rs`：`job_request`、`watch_job`、`relay_job`、`relay_threads`。
  - テスト：`server/tests/turn_tcp.rs` の `a_screen_lists_watches_types_into_and_stops_the_threads_jobs`。
- **根拠の状態：** UNSUPPORTED_APPROVED。画面の操作で、効き目を測る学術研究は当てはまらない。調べたのは、ほかのハーネスの実装だけ。
- **ユーザーの承認：** 2026-11-28。
  - 「次の群（あると便利な機能）に進みますか？」に「次の群へ進む」と答えた。
  - 「モデルが裏で走らせたシェル（長く動くコマンド）について、画面から何をできるようにしますか？」に「一覧・出力・停止・入力」（上に加えて、画面から文字やキーを送れる）と答えた。
  - 要求とイベントの名前と形、読む量の上限、追いつけないときの扱いは、作業者（Claude）が決めた。

## ほかのハーネス（2026-11-28、作業者が頼んだ調べものの報告）

| ハーネス | 裏で動くコマンド |
|---|---|
| Codex（app-server、試験的） | `thread/backgroundTerminals/list`・`terminate`・`clean`。出力は `item/commandExecution/outputDelta` で流れる |
| Codex（TUI） | `/ps` で一覧、`/stop` で全部止める |
| Claude Code | `/tasks`（`/bashes`）で一覧し、`x` で止める。出力はファイルにあり、モデルは `Read` で読む |

## 検証

- テストで確かめたこと（Windows）：
  - 入力を待つジョブと眠るジョブを、モデルが始めたターンの後で、一覧に 2 つとも `running: true` で出る。
  - 購読を始めると一覧が届く。
  - `watch` で、問いの出た画面が届き、`write` で `y` と `Enter` を送ると、その答えの出力、`job_ended`（終わりの番号 0）、`completed` が届く。
  - `read` でログに答えが入り、`next_offset` が `size` と同じになる。
  - 終わったとき、止めたときに、購読へ新しい一覧が届く。
  - 止めると `true`、もう一度止めると `false`、無い番号は `unknown_job`。
- 確かめていないこと：macOS と Linux（CI で確かめる）、追いつけないときの `lagged`、長い出力。
