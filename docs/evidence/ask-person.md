# 人に質問するツール（`ask`）

- **対象と範囲：** モデルが人に、画面がフォームとして出せる形で質問する。ターンは止めず、質問はターンの終わりに返す。答えは、次のターンで質問への返信（`reply_to`）として送る。
  - 範囲外（未実装）：決めた時間が過ぎたら既定の答えで進むこと（`default` は画面への手がかりとして渡すだけ）。
- **関連コード：** `core/src/ask.rs`（ツールの定義と形の確認）、`core/src/turn.rs`（`Asked`、`Outcome::questions`、`seq_of_result`、`TOOL_GUIDE`）、`core/src/lib.rs`（`EventPayload::questions`）、`server/src/lib.rs`（ターンの終わりの通知）。
- **根拠の状態：** UNSUPPORTED_APPROVED。入力をしやすくするための機能で、効果の研究は調べていない（ユーザーの判断）。
- **ユーザーの判断：** 2026-11-20「あと質問ツールはアイデアほしい。別に入力しやすくするだけなので根拠はそんなにいらない」、アイデアの提示の後に「それでいいと思う。フォーム組めると的確になると思う」「質問は実装許可」。

## 形式（アイデアのうち実装したもの）

- 1 回に 1〜4 問（`questions`）。各問は `question`（文）と `kind`（答え方）。
- `kind`：`choose_one`（1 つ選ぶ）、`choose_many`（複数選ぶ）、`order`（並べ替え）、`text`、`number`、`yes_no`、`date`、`color`、`file`（プロジェクトのファイル）。値の種類だけを決め、表示は画面に任せる（範囲つきの数値はスライダー、色はカラーパレットなど）。種類は MCP の聞き返しの形式（文字・数値・真偽・列挙、日付などの形式）を参考に、並べ替えとファイルを足した（工学上の選択）。
- 選ぶ問いと並べ替えは `options`（2〜10 個、`label` は重ならない）。各選択肢に `description` と `preview`（その案で変わるコードや差分）を付けられる。1 番目を推奨とする。人は選択肢の代わりに、いつでも自分の言葉で答えられる（ツールの説明に書いた。画面がそうできるようにする前提）。
- `number` は `minimum`・`maximum`（逆なら断る）。
- `default`：答えがないときに取る答え。`location`：問いが関わる `path:line`（画面がそのコードの横に出せる）。
- 形が合わないと、どこが合わないかを道具の失敗として返し、ターンの終わりの一覧には入れない。

## 流れ

1. モデルが `ask` を呼ぶ。道具の結果は「Asked the person:」と各問の要約で、「答えは後のメッセージで来る。答えに関係しない作業を続け、なければ答えを終えて、何を待っているかを書く」と伝える。
2. 呼び出しは何も変えないので、承認のモードによらず人の承認を待たない。
3. ターンの終わりの `completed` の通知に `questions` を入れる。各要素は `seq`（`ask` の結果のメッセージの番号）と `questions`（モデルが渡した形そのもの）。`ask` を含むメッセージ自体も、ターンの途中で `message` の通知として届く。
4. 人の答えは、次の `turn/start` で `reply_to` にその `seq` を付けて送る。モデルには「written by the tool ask」として質問の要約が引用され、その後に答えが届く（`reply-to-message.md`）。

## 拡張（2026-11-21）

- **承認：** 2026-11-20 の質問「夜のあいだに inocode へ実装してよいものはどれですか？」で「質問ツールの拡張」（MCP サーバーからの聞き返しを同じフォームで人に出す、答えを lesson の候補にする）を選んだ。UNSUPPORTED_APPROVED、研究なし。
- 答えを lesson の候補に：`ask` の結果か、ハーネスが人に回した質問（`escalation`）への返信では、引用の後に「答えが今後の作業にも当てはまるなら `lesson` で提案して」と添える（`core/src/turn.rs` の `quote`）。提案するかはモデルが決め、提案は既存の `lesson` の流れ（検証か承認で使われる）に入る。
- MCP サーバーからの聞き返し：
  - 道具の呼び出しの途中で MCP サーバーが `elicitation/create`（form）を送ると、ターンの途中の通知 `elicitation` を出す。中身は `id`、`server`（`mcp.json` の名前。仕様は、どのサーバーが聞いているかを画面で示すよう求める）、`message`、`requestedSchema`（元のまま）、`questions`（同じ問いを `ask` の形に直したもの。各問いに元のプロパティ名 `name` と `required`）。
  - 形の対応：文字は `text`（日付の形式なら `date`、ほかの形式は `format` に残す）、`enum`・`oneOf` は `choose_one`（`label` と元の値 `value`）、配列は `choose_many`、真偽は `yes_no`、数値は `number`（`minimum`・`maximum`）。`description`・`default` は引き継ぐ。
  - 人は `turn/answer`（`thread_id`、`elicitation` に通知の `id`、`action` に `accept`・`decline`・`cancel`、`accept` なら `content` にプロパティ名をキーにした答え）で答える。応答は `{"accepted": true|false}`（待っている問いがなければ false）。
  - 呼び出しは答えを待つ（`ask` と違い、ターンは先に進まない。サーバーが答えを待っているため）。10 分過ぎるか中断されたら `cancel`。仕様が求める「断る」「取り消す」の選択と、送る前の見直しは、画面の役目。
  - 詳細は `mcp-client.md` の「聞き返し」。

## 検証

- 単体テスト（`core/src/ask.rs`）：形の要約、問いの数、文のない問い、知らない種類、選択肢の数と重なり、数値の範囲。
- 結合テスト（`server/tests/turn_tcp.rs` の `questions_are_reported_when_the_turn_ends_and_answered_by_a_reply`、模擬 API）：形の合わない呼び出しは失敗になり一覧に入らないこと、合う形は `completed` の `questions` に `seq` つきで入ること、その `seq` への返信で質問が引用されること。
- 単体テスト `the_tool_guide_names_every_tool_of_the_harness`：システムプロンプトの使い分けの節に `ask` があること。
- 拡張：結合テスト `questions_are_reported_when_the_turn_ends_and_answered_by_a_reply`（`ask` への返信に lesson の案内が付くこと）、`a_question_of_an_mcp_server_waits_for_the_persons_answer`、単体テスト `a_requested_schema_becomes_the_questions_of_ask`。
- 未検証：実際のモデルが適切な場面で聞くか、聞きすぎないか、画面での表示（inocode の画面はまだない）。
