# ターンのプロトコルとサーバー側のループ

- **対象と範囲：** スレッドを作り、読み、ターンを回す要求を TCP プロトコルに加える。`thread/start`・`thread/read`・`turn/start`・`turn/steer` を追加し、中断には既存の `cancel` を使う。ループと「続けるか」の判定はサーバーが持つ。範囲外：ツールの実装（まだ 1 つもない）、継続のための差し込み（todo・再確認・隠し継続）、システムプロンプトと規則の注入、文脈の圧縮、ターン数の上限。
- **関連コード：** `core/src/turn.rs`（ループと判定 `next_step`）、`core/src/lib.rs`（`Core` のスレッド操作、イベントの型）、`core/src/store.rs`（`session_exists`）、`server/src/lib.rs`（要求の振り分けと中継）、`server/src/main.rs`（`~/.inocode/inocode.db` を開く）、`server/tests/turn_tcp.rs`。
- **根拠の状態：** UNSUPPORTED_APPROVED（プロトコルの形と、続けるかの判定。下記）

## 決めたことと根拠

| 判断 | 根拠の種類 | 内容 |
|---|---|---|
| ループと終了判定をサーバーが持つ | 承認済みの設計（研究なし） | 会話の状態をサーバーが持つ判断（`conversation-state.md`）の延長。接続が切れてもターンは続き、作られた発言は保存され、`thread/read` で読める |
| 続けるかの判定を `next_step` の 1 か所に集める | 研究の不足（下記）と承認 | どの継続の規則が最善かは比較実験で決める（2026-11-16 にユーザーが承認）。差し替える場所を 1 つにする |
| 当面の判定：ツールを呼んだか、人の追加入力があれば続け、文章だけで止まれば終える | 公開実装（研究なし） | Codex（`codex-rs/core/src/stream_events_utils.rs` の `needs_follow_up`、`session/turn.rs` の `has_pending_input`）、DSH（`packages/core/agent-loop/README.md`「No built-in turn budget — tool calls or steering continue the current turn」）、Pi が同じ規則。3 つに共通する規則を基準の条件として置く |
| ターン数の上限を置かない | 公開実装と間接の研究 | DSH は上限なし。ターン数の研究（ICSE 2026、arXiv 2510.16786）は固定の上限より必要に応じて延ばすほうが 12〜24% 安く同等以上とする（予算の研究で終了判定の研究ではない）。止めるのは `cancel` |
| 長さ切れ（Length）でターンを終える | 公開実装 | DSH は長さ切れの発言のツール呼び出しを捨てる。続けるかどうかは継続の規則の実験の対象 |
| 呼び出しには必ず結果を付ける（ツールがないので「そのツールはない」というエラー） | 公式仕様 | 答えのない呼び出しは次の送信でエラーになる（`conversation-model.md` 判断 4） |
| 1 スレッドで同時に 1 ターン。走行中の入力は `turn/steer` で次の要求の前に加わる | 公開実装（研究なし） | Codex の `turn/steer`、DSH の inbox（次のステップの前に取り込む） |
| ターンが終わると決めた後に届いた `turn/steer` の入力も捨てない | 不具合の修正（「受け付けた」と返した入力が消えないこと） | 終える直前に入力の受け口を閉じ、それまでに届いた入力があれば、それを次の要求として同じターンで続ける。閉じた後の `turn/steer` は `accepted: false`。中断・失敗で終わるときは、届いた入力を人の発言として保存だけする |
| どの接続からでもスレッドのターンを止められる：`turn/interrupt {thread_id}` と `/stop` | 不具合の修正（接続が切れたターンを止める手段が無かった） | 結果は `{interrupted}`（走っているターンが無ければ `false`）。ターンは、始めた接続の `cancel` と、この止め方の両方で止まる。接続が切れてもターンは続くこと（上）は変えない |
| モデルの答えが一定の時間なにも送ってこなければ、ターンを誤りで終える | 不具合の修正 | 設定 `server.stream_idle_seconds`（既定 300 秒）。誤りの種類は `provider_idle`（`account-pool.md`） |
| ターンの最後に、承認待ちの呼び出しと質問をデータベースに書く | 不具合の修正（再起動で答えられなくなった） | 版 10 の `waiting` 表。`/approve`・`/deny`・`/answer` はそこから読む。MCP のサーバーの質問も `elicitations` 表に書き、サーバーを起動し直すと「期限切れ」にする（待っていたターンがもう無いため）。`/answer` は期限切れの質問を `expired: true` で見せ、それに答えると `expired` の誤りを返してその質問を消す |
| 仕事の外で起きた失敗を、ターンの終わりの `warnings` で知らせる | 不具合の修正（stderr だけだった） | 同僚に聞けなかった、使用量のログに書けなかった、issue に未完了を記録できなかった、承認待ちを保存できなかった、など。`completed` のイベントの `warnings`（文字列の配列。無ければ項目が無い） |

## 研究の状況（継続の規則）

2026-11-16 の調査で、「文章だけで止まったら終える」「完了用のツールを呼ぶまで続ける」「止まったら継続を差し込む」を直接比べた研究は見つからなかった。関連する研究：

- Fan et al., "An Empirical Study of Harness Design for Coding Agents"（arXiv 2609.20804、2026-09、査読前）：計画ツール `update_plan` で弱いモデル（Nemotron-3 30B）が SWE-bench Verified 13.6% → 25.2%（有意）。120B・550B・Mistral-Medium では有意差なし。継続の差し込みや完了用ツールの比較はない（原文の表 1 に完了用ツールはない）。
- Liu, "When May an Agent Stop? Evidence-Carrying Termination for Tool-Using LLMs"（arXiv 2608.23623、2026-08、査読前）：証拠つきでしか完了を認めない方式で、根拠のない早すぎる終了が 40/66 → 0/66、誤って続けた例 0/66。合成の 24 課題、モデル 1 つ（Gemini 2.5 Flash）、第三者の再現なし。
- Shen et al., "Thinking vs. Doing"（arXiv 2506.07976、査読前として扱う）：完了の合図の後の再確認で WebArena の成功率が上がるが、重ねすぎると悪化。
- Bui, "Building Effective AI Coding Agents for the Terminal"（arXiv 2603.05344、作業中の版）：暗黙の終了と完了用ツールを設計として説明。比較実験はない。

実ソースで確認したこと（研究ではない）：Codex は「解決するまで続けよ」という強い指示（`gpt_5_2_prompt.md` 111 行）があるのに、ユーザーの報告では「やります」と言って止まる。DSH は人格の指示が 1 行（"You are a coding agent powered by the {{model}} model."）と todo ツールだけで完走する（ユーザーの報告）。ループの終わり方は同じなので、差はモデル・todo ツール・DeepAI の隠し継続（`conversation-model.md` の自走性の節）のどれかにある。どれが効くかは測っていない。

## 検証方法（予定）

ツールができた後、DeepSeek（DeepAI の無料モデル）で次を比べる：素のループ（この実装）、todo ツールあり、todo が残るときだけ継続を差し込む、DeepAI 方式の継続。測るもの：完走率、根拠のない終了、誤って続けた回数、トークン量。結果で `next_step` を決め、この記録を更新する。

## ユーザーの承認

2026-11-16、「プロトコルを先に作り、継続の規則は実験で決める」案に「Yes, go」。

## 実装・検証の状況

- `server/tests/turn_tcp.rs`（模擬プロバイダ）：文章の応答でターンが終わり、理由と使用量を返し、保存した履歴を次のターンで送る。ツール呼び出しの後は結果を付けて続け、2 回目の要求に `tool` メッセージが入る。同じスレッドの 2 つ目のターンは `turn_running`。`turn/steer` の入力は保存される。`cancel` で `cancelled` になり、途中までの応答は `aborted` として残る。存在しないスレッドや不正な引数はエラー。
- 同じファイル：始めた接続を閉じたターンを、別の接続の `turn/interrupt` で止められる（`any_connection_interrupts_a_turn_whose_connection_closed`）。黙ったプロバイダでターンが `provider_idle` で終わる（`a_provider_that_goes_silent_ends_the_turn_with_an_error`）。承認待ちがサーバーの起動し直しの後も残り、`/approve 1` はその呼び出しだけを通す（`held_calls_outlive_a_restart_and_approve_runs_only_that_call`）。ゴールの判定中に止めたゴールは止まったまま、その間に届いた入力には次の要求で答える（`a_goal_stopped_while_checked_stays_stopped_and_late_steering_is_answered`）。
- `core/src/turn.rs` の単体テスト：判定の表と、呼び出しへの答え。
- 実プロバイダでの確認：未実施。
