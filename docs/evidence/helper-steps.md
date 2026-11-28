# サブエージェントの途中の動きを画面に流す

- **対象と範囲：** ターンの中で別の働き手が動く 2 つの仕組みについて、途中の動きを画面に届ける。
  - 読むだけの手伝い（`explore`、会社のメンバーによるレビュー、目標の確認。`explore::read_only` を使うもの）：
    - 手伝いの 1 歩ごとに、親のターンのイベント `helper`（`result: {call_id, helper, step}`）を送る。`call_id` は、手伝いが答えている親の呼び出し。`helper` は要求の記録の名前（`explore` など）。
    - `step` の形：
      - `{kind: "started", question}`：始めたとき。
      - `{kind: "text", text}`：道具を呼ぶ回の、手伝いの文。
      - `{kind: "call", call_id, name, arguments}`：手伝いの道具の呼び出し。
      - `{kind: "result", call_id, content, is_error, truncated}`：その結果。画面へは 2,000 字まで（`truncated` で切ったかを示す）。手伝い自身には全部を渡す（今までどおり）。
      - `{kind: "ended", answer}` か `{kind: "ended", error}`：終わったとき。最後の答えの文は `answer` に入り、`text` としては送らない。
    - ターンが知らせる先を持たない呼び出し（目標の確認の一部など）では送らない。
  - 会社のメンバーに任せた仕事（`delegate`）：
    - メンバーの子スレッドができたら、親のターンのイベント `delegated`（`result: {call_id, thread_id, member}`）を送る。画面はここから子スレッドへたどれる。
    - 子スレッドのターンのイベントを、子スレッドの購読（`thread/subscribe`）へ、普通のターンと同じに流す：`turn_started`、`status`（`running`）、メッセージ・差分以外の各イベント、`turn_ended`、`status`（`idle`）。全部のスレッドの状態の購読にも `status` が届く。
  - 範囲外・限界：
    - メンバーのターンは `Core` のターンの一覧に入らないので、子スレッドを購読し始めたときの最初の状態や `thread/list` の `status` は、動いていても `idle` になる。
    - `delegated` より前に子スレッドを購読することはできないので、購読が間に合わないと、最初のイベント（`turn_started` など）は届かない。メッセージは `thread/read` で読める。
    - 手伝いの答えの文は、少しずつ（`delta` として）は流さない。1 回の答えが終わってから送る。
- **関連ソースコード：**
  - `core/src/explore.rs`：`read_only`、`read_rounds`、`Steps`、`SHOWN_RESULT_CHARS`。
  - `core/src/agent_tools.rs`：`Context::call_id`。
  - `core/src/turn.rs`：`TurnEvent::Helper`・`Delegated`、`HelperStep`、`Thread::answer`（`call_id`）。
  - `core/src/company.rs`：`Lead::events`、`delegate`・`run_delegated`（子スレッドへの中継）。
  - `core/src/lib.rs`：`TurnEnd::of`、`EventPayload::helper`・`delegated`。
  - `server/src/lib.rs`：`turn_event`。
  - テスト：`server/tests/turn_tcp.rs` の `explore_answers_with_places_found_by_a_helper_that_only_reads`、`server/tests/company_tcp.rs` の `a_members_turn_is_shown_live_in_its_own_thread_linked_from_the_call`。
- **根拠の状態：** UNSUPPORTED_APPROVED。画面の表示で、効き目を測る学術研究は当てはまらない。調べたのは、ほかのハーネスの実装だけ。
- **ユーザーの承認：** 2026-11-28。
  - 「次の群（あると便利な機能）に進みますか？」に「次の群へ進む」と答えた。
  - 「サブエージェント（任せた先の別の働き手）が動いている間、画面に何を見せますか？」に「途中の動きも流す」（親スレッドから子をたどれて、子の発言や道具の呼び出しをその場で見られる。Codex・Claude Code と同じ）と答えた。
  - イベントの名前と形、結果を 2,000 字で切ること、子スレッドの購読へ流す方法は、作業者（Claude）が決めた。

## ほかのハーネス（2026-11-28、作業者が頼んだ調べものの報告）

| ハーネス | サブエージェントの見え方 |
|---|---|
| Claude Code | 呼び出しが `名前(説明)` の行で出る。裏で動くものは入力欄の下に木の形で出る。`/tasks` からサブエージェントの会話を開け、そこに書き込んで向きを変えられる |
| Codex（app-server） | `collabAgentToolCall`（`senderThreadId`・`receiverThreadIds`・`agentsStates`）と `subAgentActivity`（`started`・`interacted`・`completed` など）。サブエージェントは自分のスレッドを持ち、`thread/list` で `ancestorThreadId` で絞れる |

## 検証

- テストで確かめたこと：
  - `explore` が `find`、断られる `write`、断られる外の `read` を順に呼ぶターンで、`helper` が `started`・（`call`・`result`）× 3・`ended` の順に届き、どれも `call_id` が親の呼び出しで、`helper` が `explore`。断られた結果は `is_error: true`、`ended` の `answer` が答え。
  - 会社のメンバーに任せると、親のターンに `delegated`（呼び出し `d1`、メンバー `coder`、子スレッドの番号）が届く。その子スレッドを購読すると、メンバーの道具の結果のメッセージ、答えの文、`turn_ended`（`completed`）が届く。
- 確かめていないこと：レビューと目標の確認での `helper`、同時に動く複数のメンバー。
