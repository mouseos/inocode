# プランモード（ファイルを変えずに調べ、計画を承認してから実装する）

- **対象と範囲：**
  - スレッドごとの設定 `plan`（`true`・`false`）。保存は、ほかのスレッドの選択（`mode` など）と同じ `thread_settings` の表。`thread/settings` の読み出しに `plan` が出る（既定は `false`）。
    - 変え方：`thread/settings {thread_id, plan: true|false|null}`（`null` は `false`）。`turn/start` の `plan: true|false`（そのターンが始まるときにスレッドの設定を変え、以後のターンにも残る。ターンが始められなかったときは変えない）。スラッシュコマンド `/plan on`・`/plan off`（`applies: "next_request"`）。
    - `/plan` に何も付けなければ、これまでどおり計画の道具（`plan`）の一覧を出す。`on`・`off` はサブコマンドで、ほかの語は誤り（これまでどおり）。
    - 走っているターンは、モデルへの要求ごとにスレッドの設定を読み直す。途中で変えると、次の要求から効く（`mode` と同じ）。
  - プランモードの間：
    - 要求ごとの状態の文（履歴の後に毎回送り、保存しない）に「プランモードである。読むだけの操作で調べ、ファイルを変えない。計画ができたら exit_plan で示す」を入れる（`plan_mode::STATE`）。
    - 道具 `exit_plan {plan}`（markdown）を、プランモードの間だけ出す。システムプロンプトの道具の案内（`TOOL_GUIDE`）にも 1 行ある（「プランモードの間だけ出る」）。
    - 断る道具（結果は「プランモードなので実行しない。読むだけの操作で調べ、exit_plan で計画を示せ」）：`edit`、`edits`、`write`、`plugin_install`、`delegate`、写真・動画を撮る `camera`、録音する `audio`。
    - いつも人に聞く道具（承認のモードが `on-request`・`auto`・`full` でも。`auto` でも判定役には回さない）：読むだけの命令でない `shell`、`outside_sandbox` を付けた `shell`、動いている命令への入力（`job` の `input`・`keys`）、MCP の道具（`mcp__` で始まるもの）。問いは承認の問い（`mode: "approval"`）で、理由は `plan mode is on, and this may change something`。スレッドやプロジェクトで前に許した同じ呼び出し（`allowances`）は聞かずに実行する。
    - `block` の規則は、断る・聞くより先に止める。
    - ほかの道具（`read`、`find`、`explore`、`recall`、`search`、`fetch`、`transcribe`、読むだけの `shell`、`job` の待つ・止める、計画の `plan`、`promise`、`lesson`、`method`、`issue`、`ask`、`title`、`review`、MCP の資源を読む道具など）は、いつもどおり（承認のモードの決まりもそのまま）。
  - `exit_plan` の流れ：承認の問いと同じ仕組み（`elicitation` のイベント、`turn/answer`）で、`{"mode": "plan", "id", "plan"}` を送る。問いが開いている間、スレッドの状態は `waiting`。片付くと `answered`。
    - `accept`：スレッドのプランモードを切る（保存）。イベント `plan_mode`（`{"event": "plan_mode", "plan": false}`）を、ターンを始めた接続と `thread/subscribe` の画面に送る。道具の結果は「人が計画を承認した。プランモードは切れた。今すぐ計画を実装せよ」。ターンはそのまま、スレッドの承認のモードで実装を続ける。
    - `decline`・`cancel`：プランモードのまま。`content: {"feedback": "..."}` を付けられる。道具の結果は「人が承認しなかった。プランモードのまま。意見：…。計画を直して、もう一度 exit_plan で示せ」（意見がなければ、直して示すか、答えを終えて人に何を変えるか聞け）。
    - 中断：いつもどおりターンが終わる（結果は「人が答える前にターンが止められた」）。
    - 問いを送れない（イベントの受け手がいない）とき：「計画を見せられなかった。プランモードのまま。答えの終わりに計画を書け」と返す。
    - プランモードでないときは道具を出さない。それでも呼ばれたら「exit_plan という道具はない」（ほかの無い道具と同じ）。
  - 承認された計画の文は、`exit_plan` の呼び出しと結果としてスレッドの履歴に残る。別の保存場所は作らない。
  - 範囲外：
    - `thread/settings` や `/plan on|off` での変更を、ほかの画面に知らせる通知（`plan_mode` のイベントは、ターンの中で計画が承認されたときだけ送る。ほかの画面は `thread/settings` を読む）。
    - 計画の承認の後に、計画からスレッドの題名を付けること（Claude Code にはある）。
    - 承認と同時に承認のモードを選ぶこと（承認の後は、スレッドの今の承認のモードで続ける）。
  - 限界：
    - ファイルを変えないことを保証するのは、上の道具の扱いだけ。人が承認した命令や MCP の道具は、ファイルを変えうる。
    - 社員の実行（`delegate`）は、リーダーのスレッドのプランモードを引き継がない（プランモードの間は `delegate` を断るので、起きない）。
    - スレッドを写す（`thread/fork`、`/fork`）と、モデルの選択と一緒にプランモードも写る。
- **関連ソースコード：**
  - `core/src/plan_mode.rs`：`EXIT_PLAN`、`PLAN_MODE`、`STATE`、`REFUSED`、`ASKS`、`spec`、`plan`、`on`、`set`。
  - `core/src/turn.rs`：`plan_refuses`、`plan_asks`、`Heard`、`Thread::put_to_person`（承認の問いと共通）、`Thread::exit_plan`、`Thread::needs`・`Thread::answer`・`Thread::delegate_all` のプランモードの扱い、`steps`（要求ごとの読み直しと道具）、`TurnEvent::PlanMode`、`TOOL_GUIDE`。
  - `core/src/lib.rs`：`ThreadChoice::plan`、`Core::thread_settings`・`Core::set_thread_settings` の `plan`、`Request::plan`、`EventPayload::plan_mode`、`Request::elicitation_answer`（`decline` の `content` を渡す）。
  - `core/src/store.rs`：`answer_elicitation`（`content` を残すのは、計画の問いの `decline` だけ。MCP の質問などの断りには付けない）。
  - `core/src/commands.rs`：`/plan on`・`/plan off`。
  - `server/src/lib.rs`：`turn/start` の `plan`、`turn_event` の `plan_mode`。
  - テスト：`server/tests/turn_tcp.rs` の `in_plan_mode_changes_are_refused_and_commands_wait_for_the_person`、`an_approved_plan_ends_plan_mode_and_the_turn_implements_it`、`plan_mode_is_switched_by_command_and_settings_and_exit_plan_is_offered_only_then`。`core/src/turn.rs` の `plan_mode_refuses_changes_and_asks_before_what_may_change`。`server/tests/settings_tcp.rs` の `thread/settings` の読み出し（`plan: false`）。
- **根拠の状態：** UNSUPPORTED_APPROVED。この機能（人が計画を承認してから実装する、コーディングエージェントの操作の流れ）の効き目を測った研究は確認できていない。
- **ユーザーの承認：** 2026-11-27。
  - 足りない機能の洗い出しで「急ぐ群から順に」と答えた。
  - 「⑫ プランモード（ファイルを変えずに調べて計画を立てる）の終わり方はどうしますか？」に「計画を承認して実装へ（モデルが計画を出したら、人に『この計画で進めるか』を聞く。はいなら、そのまま普通のモードに戻って実装を始める。Claude Code と同じ）」と答えた。
  - 作業者（Claude）が決めたこと：設定の名前 `plan` と、その置き場所（スレッドの設定）。`turn/start` の `plan` が以後のターンにも残ること。`/plan on`・`/plan off` の形（`/plan` だけは今のまま）。道具の名前 `exit_plan` と、問いの形 `{"mode": "plan", "id", "plan"}`。断る道具と、いつも人に聞く道具の一覧（上）。意見の渡し方（`decline` の `content.feedback`）。イベント `plan_mode`。プランモードでないときは道具を出さないこと。承認の後はスレッドの承認のモードで続けること。状態の文と結果の文の言い方。

## 学術的な主張・不足点

- 適用可能な学術研究を確認できていない（人が計画を承認する流れ、プランモードの有無で、コーディングエージェントの結果を比べた研究は見つけていない）。
- 近い研究（この機能の根拠にはしない。書誌は作業者の知識によるもので、今回は本文を読んでいない）：
  - Wang ほか, "Plan-and-Solve Prompting", ACL 2023（arXiv 2305.04091）：先に計画を立ててから解くよう促すと、算数などの推論の正答が上がった。対象は 1 回の答えのプロンプトで、道具を使うエージェントでも、人の承認でもない。
  - Jiang ほか, "Self-planning Code Generation with Large Language Models"（arXiv 2303.06689）：コードを書く前にモデル自身に手順を書かせると、関数単位のコード生成（HumanEval など）の正答が上がった。リポジトリの中で道具を使う作業でも、人の承認でもない。

## ほかのハーネス（2026-11-27、依頼の文にある情報。inocode の作業者は直接読んでいない）

| ハーネス | プランモード |
|---|---|
| Claude Code | 承認のモードの 1 つとして plan がある（Shift+Tab で切り替え、`/plan`）。道具 `ExitPlanMode` が計画を人に見せて承認を求める。承認と同時に、計画から会話の名前を付け直すこともできる |
| Codex | 協働のモード（collaboration mode）の Plan（`/plan`、`turn/start` の `collaborationMode`）。上流には実験的な `thread/prepareApprovedPlan` がある |
| opencode | `plan` のエージェント（プラグイン） |

## 検証

- 上のテストで確かめたこと（Windows、模擬の API）：
  - プランモードで `write` は断られ、ファイルはできない。`mkdir` の `shell` は、`on-request` でも `auto` でも人に聞かれ（`auto` で判定役への要求はない）、断ると実行されない。読むだけの `ls` は聞かずに実行される。
  - プランモードの要求には `exit_plan` の道具と、状態の文の「Plan mode is on」がある。
  - `turn/start` の `plan: true` でプランモードになり、`exit_plan` で `mode` が `plan` の問いが計画の文つきで届く。問いの間、`thread/list` の状態は `waiting`。
  - 意見つきで断ると、プランモードのまま（`thread/settings` の `plan` が `true`）、次の要求に意見が入る。
  - 承認すると `plan_mode`（`plan: false`）のイベントが届き、同じターンの次の `write` が実行される。次の要求には `exit_plan` も状態の文もない。`thread/settings` の `plan` は `false`。
  - プランモードでないとき、要求に `exit_plan` は無く、呼ばれても「no tool named exit_plan」。`/plan on`・`/plan off`・`thread/settings` の `plan`（`true`・`null`）で切り替わり、文字の `plan` は `invalid_params`。`/plan` は計画の一覧を返す。
- 確かめていないこと：
  - 問いを送れないとき（社員の実行など）の結果。
  - 計画の問いの間の中断。
  - 写真・録音・`plugin_install`・`delegate`・MCP の道具・`job` の入力の、プランモードでの扱い（単体テストで判定の関数だけを確かめた。`camera`・`audio` は単体テストもない）。
  - 実際のモデルが、状態の文と道具の説明に従って、読むだけで調べ、`exit_plan` で計画を示すか（ライブの試験はしていない）。
  - 画面からの使いやすさ。
