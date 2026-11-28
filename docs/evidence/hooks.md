# フック

- **対象と範囲：** 人が設定したコマンドを、スレッドの仕事の決まった場面で自動で走らせる。設定と入出力の形は Claude Code に合わせた（ユーザーの選択「Claude Code と同じ形」）。
  - 置き場所（ユーザーの選択「全体とプロジェクトごと」）：
    - 全体：inocode の設定ファイル（データベースの横の `settings.json`）の `hooks`。
    - プロジェクト：プロジェクトのフォルダの `.inocode/settings.json` の `hooks`。人が `hooks/trust` で、その時の中身のまま信頼するまで走らせない。中身が変わると信頼は外れ、もう一度信頼が要る。信頼は、データベースの横の `hooks-trusted.json` に、フォルダごとの中身の SHA-256 として記録する。信頼していない、または読めないプロジェクトのフックがあるときは、ターンの注意（`warnings`）にそう書く。
  - 形（Claude Code と同じ）：`{"<場面>": [{"matcher": "...", "hooks": [{"type": "command", "command": "...", "timeout": 秒}]}]}`。
    - `matcher`：空か `*` なら全部。それ以外は `|` で区切った名前のどれかと同じもの。道具の場面では、inocode の道具の名前と、Claude Code の同じ道具の名前（`shell`→`Bash`、`write`→`Write`、`edit`→`Edit`、`read`→`Read`、`find`→`Grep` など、`hooks::CLAUDE_NAMES`）の両方で比べる。**正規表現（`Notebook.*` など）には対応していない**。
    - `type` は `command` だけ走らせる。`http`・`mcp_tool`・`prompt`・`agent` は使わず、注意に書く。
    - `timeout` が無ければ 60 秒。過ぎると止めて、人に知らせる。
  - 走らせ方：サンドボックスの外で、シェルの道具と同じシェル（Windows は PowerShell、ほかは `sh -c`）で、プロジェクトのフォルダで走らせる。環境変数 `CLAUDE_PROJECT_DIR` と `INOCODE_PROJECT_DIR` にフォルダを入れる。標準入力に JSON（`session_id`、`hook_event_name`、`cwd` と場面ごとの欄）を渡す。同じ場面の合うフックは、書いた順に 1 つずつ走らせる（Claude Code は並べて走らせる）。
  - 答えの読み方（Claude Code と同じ）：
    - 終わりの番号 2：止める。理由は標準エラー出力。
    - 0：標準出力が JSON なら読む。読む欄：
      - `continue: false`（`stopReason`）、`systemMessage`、`decision: "block"`（`reason`）。
      - `hookSpecificOutput` の `additionalContext`、`permissionDecision`（`allow`・`deny`・`ask`）と `permissionDecisionReason`、`updatedInput`。
      - `PermissionRequest` の `decision: {behavior, message, updatedInput}`。
    - 0 で JSON でない出力：`UserPromptSubmit` と `SessionStart` ではモデルへの文にし、ほかでは捨てる。
    - それ以外の番号：止めない。人に知らせる。
    - いくつものフックの答えは、最初に止めたものの理由、いちばん厳しい許可（`deny` > `ask` > `allow`）、最後の `updatedInput` をとる。
    - 人への文（`systemMessage` とフックの失敗）は、ターンのイベント `hook`（`result: {event, text}`）で送る。ターンの外の場面では、要求の答えの `hook_messages` に入れる。
    - `continue: false` は、その歩みの結果を保存した後でターンを終え、注意に理由を書く。
  - 場面（ユーザーの選択「Claude Code とほぼ同じ」。作ったもの）：
    - `SessionStart`（`matcher` は `startup`）：スレッドの最初のターンの始め。足した文は、最初の人の発言の前に入る。
    - `UserPromptSubmit`：ターンの人の発言を保存する前。入力に `prompt`。止めるとターンが `hook_blocked` で失敗し、発言は保存しない。足した文は、人の発言の後に入る。
    - `PreToolUse`（道具の名前）：道具の呼び出しの前、規則の確認の前。入力に `tool_name`・`tool_input`・`tool_use_id`。
      - 止める・`deny`：「フックが断った」という結果を返し、走らせない。
      - `updatedInput`：引数を置き換える。
      - `allow`：承認を聞かない。プロジェクトの必ず守る規則と計画モードは、そのまま効く。
      - `ask`：モードによらず人に聞く。
      - 足した文は結果に付ける。
    - `PermissionRequest`（道具の名前）：人に承認を聞く直前。`allow` で走らせ、`deny` で断る。
    - `PostToolUse`・`PostToolUseFailure`（道具の名前）：結果が出た後。失敗（エラーの結果）なら後者で、入力に `error`。止めた理由と足した文は、結果に `Hooks add:` として付ける。
    - `Notification`（`permission_prompt` か `elicitation_dialog`）：人に何かを聞く直前。知らせるだけ。
    - `Stop`：モデルが答え終えて、ターンが終わろうとするとき（目標が無いとき）。入力に `stop_hook_active`（このターンで `Stop` のフックがもう続けさせたか）。止めると、理由を「Stop のフックが続けるよう頼んでいる」という文にしてターンを続ける。何度続けさせるかに上限は無い（Claude Code と同じく、フックが `stop_hook_active` を見て止める）。
    - `SubagentStart`・`SubagentStop`（`explore` か `delegate`）：手伝いや会社のメンバーの仕事の前後。知らせるだけ。
    - `PreCompact`・`PostCompact`（`auto` か `manual`）：要約の前後。`manual` の入力に `custom_instructions`。知らせるだけ。
    - `SessionEnd`（`thread_deleted`）：スレッドの削除。知らせるだけ。
    - `CwdChanged`：`thread/folder`・`/cd`・worktree への移動の後。入力に `old_cwd`・`new_cwd`。
    - `WorktreeCreate`・`WorktreeRemove`：worktree を作った後と、削除で消した後。知らせるだけ（Claude Code のように作り方を置き換えることはしない）。
  - 要求：`hooks/list {thread_id?}` → `{settings, project: {path, hooks, trusted}}`。`hooks/trust {thread_id}` → `{trusted, project}`（フックが無ければ `trusted: false`）。
  - 範囲外・限界：
    - Claude Code のほかの場面は作っていない。例：`Setup`、`UserPromptExpansion`、`PermissionDenied`、`PostToolBatch`、`MessageDisplay`、`TaskCreated`・`TaskCompleted`、`StopFailure`、`TeammateIdle`、`InstructionsLoaded`、`ConfigChange`、`DirectoryAdded`、`FileChanged`、`PreModelSwitch`・`PostModelSwitch`、`Elicitation`・`ElicitationResult`。
    - 会社のメンバーに任せる `delegate` の呼び出しには、`PreToolUse`・`PostToolUse` は走らない（`SubagentStart`・`SubagentStop` だけ）。
    - 道具の引数の欄の名前は inocode のもの（例：`edit` の `path`）で、Claude Code の `file_path` とは違う。Claude Code 用のフックが引数の欄を読むときは、そのままでは動かないことがある。
    - `transcript_path` は渡さない。
    - 全体の設定のフックは信頼を聞かずに走る（人が自分の設定に書いたもの）。
- **関連ソースコード：**
  - `core/src/hooks.rs`：`Hooks`（`load`・`has`・`run`）、`Said`、`parse_hooks`、`run_one`、`read_output`、`listing`、`trust`、`CLAUDE_NAMES`。
  - `core/src/turn.rs`：
    - `Thread::hook`・`hook_note`、`TurnEvent::Hook`。
    - `answer`（`PreToolUse`・`PostToolUse`・`Subagent*`）・`answer_checked`。
    - `ask_person`（`PermissionRequest`）、`put_to_person`（`Notification`）。
    - `steps`（`SessionStart`・`UserPromptSubmit`・`Stop`・`continue: false`）、`delegate_all`、`compact_if_due`。
  - `core/src/lib.rs`：`Core::run_hooks`・`hooks`・`trust_hooks`・`thread_folder`、`move_thread`・`add_worktree`（`CwdChanged`・`WorktreeCreate`）、`EventPayload::hook`。
  - `core/src/commands.rs`：`delete_thread`（`SessionEnd`・`WorktreeRemove`）、`compact`（`PreCompact`・`PostCompact`）。
  - `core/src/shell.rs`：`shell_command`（`pub(crate)` にした）。
  - `server/src/lib.rs`：`hooks/list`・`hooks/trust`、`turn_event`。
  - テスト：`server/tests/turn_tcp.rs` の `hooks_refuse_a_call_add_to_the_prompt_and_results_and_keep_the_turn_going`、`a_projects_hooks_run_only_once_trusted_as_they_are`。
- **根拠の状態：** UNSUPPORTED_APPROVED。人が自分の仕組みを足すための機能で、効き目を測る学術研究は当てはまらない。調べたのは、ほかのハーネスの実装だけ。
- **ユーザーの承認：** 2026-11-28。
  - 「次の群（あると便利な機能）に進みますか？」に「次の群へ進む」と答えた。
  - 「フックは、どの場面で動かせるようにしますか？」に「Claude Code とほぼ同じ」（約 30 種類）と答えた。
  - 「フックのコマンドに、何をさせられるようにしますか？」に「止める・書き換え・足す」と答えた。
  - 「フックの設定はどこに置きますか？」に「全体とプロジェクトごと」（リポジトリの中のものは、人が一度許すまで動かない。Codex と同じ守り方）と答えた。
  - 「フックの設定の書き方はどれに合わせますか？」に「Claude Code と同じ形」と答えた。
  - 作業者（Claude）が決めたこと：
    - 作った場面の選び方（15）。
    - プロジェクトの設定ファイルの場所と、信頼の記録の仕方。
    - 合うフックを順に走らせること、`matcher` の扱い、道具の名前の対応。

## ほかのハーネス（2026-11-28、作業者が頼んだ調べものの報告）

| ハーネス | フック |
|---|---|
| Claude Code | 場面 34。種類は `command`・`http`・`mcp_tool`・`prompt`・`agent`。終わりの番号 2 か `decision: "block"` で止める。`permissionDecision`（allow・deny・ask・defer）、`updatedInput`、`additionalContext`（10,000 字まで）、`continue: false`、`systemMessage`。`/hooks` で見る |
| Codex | 場面は SessionStart・SessionEnd・SubagentStart・SubagentStop・PreToolUse・PermissionRequest・PostToolUse・PreCompact・PostCompact・UserPromptSubmit・Stop・PreModelRequest・PostModelRequest・Interrupt。`hooks.json` か `config.toml` の `[hooks]`。管理されていないフックは信頼が要る。app-server の `hooks/list`、`hook/started`・`hook/completed` |

## 検証

- テストで確かめたこと（Windows、PowerShell）：
  - 全体の設定のフックで：
    - `UserPromptSubmit` の出力が、モデルへの最初の要求に入る。
    - `Write|Edit` に合わせた `PreToolUse` が、`write` を終わりの番号 2 で断り、ファイルが作られない。
    - `read` の `PostToolUse` の `additionalContext` が、結果に付く。
    - `Stop` のフックが 1 度だけ止めて、ターンがもう 1 回モデルに聞き、2 度目で終わる。
    - `http` のフックが使われず、注意に出る。
  - プロジェクトのフックが：
    - 信頼する前は走らず、注意が出て、`hooks/list` で `trusted: false`。
    - `hooks/trust` の後は走る。
    - 中身を変えると、また `trusted: false` になって走らない。
- 確かめていないこと：
  - `sh` で走らせること（macOS と Linux の CI で、上のテストが走る）。
  - テストの無い場面：`PermissionRequest`・`Notification`・`Subagent*`・`Pre/PostCompact`・`SessionEnd`・`CwdChanged`・`Worktree*`。
  - テストの無い答え：`updatedInput`、`allow`・`ask`、`continue: false`、時間切れ。
