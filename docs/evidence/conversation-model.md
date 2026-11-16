# 会話の型（プロバイダ中立のメッセージ列）

- **対象と範囲：** 1 往復の `prompt` しか持てない現状を、複数ターンとツール呼び出し・結果・思考を含む会話として表せるようにする型と、それを 3 形式（OpenAI Chat Completions / Anthropic Messages / OpenAI Responses）へ変換する処理。範囲外（後続段階）：ツール定義の送信とストリームからのツール呼び出しの組み立て（段階 2）、TCP プロトコルへの公開（段階 3）、エージェントループ（段階 4）、観測結果の圧縮・マスク（後続）、保存と再開（段階 6）、画像。
- **関連コード：** `core/src/conversation.rs`（型、`to_chat_completions`・`to_anthropic`・`to_responses`、送信前の検査 `prepare`）。既存の 1 往復の `chat` はまだこの型を通らない（段階 2・3 で接続）。
- **根拠の状態：** 設計判断ごとに下表のとおり。全体としては UNSUPPORTED_APPROVED（2026-11-16 承認。研究で直接裏付けられるのは判断 6・9 の一部のみで、それ以外は公式仕様・公開実装・このプロジェクトの判断）。
- **調査日：** 2026-11-16。各調査の原典 URL・ファイル位置は下記。

## 設計案

```text
Conversation { system: Option<String>, messages: Vec<Message> }

Message = { id, body }                                                 # id は会話内で一意
body =
  | User      { from: Person | Harness(kind), parts: [Text] }            # ハーネスが差し込んだ発言を区別
  | Assistant { origin: { format, model }, stop: StopReason, parts: [AssistantPart] }   # parts は届いた順のまま
  | ToolResult{ call_id, name, content: Text, is_error: bool }           # 結果 1 件 = 1 メッセージ

StopReason = Completed | Length | ToolUse | Error | Aborted             # Error / Aborted の発言は送信時に除外

AssistantPart =
  | Text      { text }
  | Reasoning { text, replay: Option<Replay> }    # 読める思考（空でもよい）と再送用データ
  | ToolCall  { call_id, name, arguments: String, replay: Option<Replay> }   # 引数はモデルが出した JSON 文字列のまま

Replay = 開けない値（Anthropic の signature / redacted_thinking の data、Responses の reasoning 項目、
         Chat Completions の reasoning フィールド名など）。手を加えず、作った origin と同じ形式・同じモデルへだけ返す。
```

| # | 判断 | 根拠の種類 | 根拠 |
|---|---|---|---|
| 1 | 特定の通信形式を正本にせず、独自の中立な型を持つ | 公開実装（研究なし） | 中立型：Vercel AI SDK `ModelMessage`、opencode `SessionMessage`、opencodex `OcxMessage`、DSH `Message`、Pi `Message`。形式を正本にする例：Codex（Responses 項目）、SWE-agent・Aider（Chat 形式）。inocode は 3 形式を同格に話すため前者 |
| 2 | 役割は system（会話の外の 1 フィールド）/ user / assistant / tool 結果 | 公式仕様・公開実装（研究なし） | system の置き場が 3 形式で異なる（Chat は message、Responses は `instructions`、Anthropic は top-level `system`）。tool 結果を独立した役割にする例：Chat、Pi（`toolResult`）、DSH（`tool`） |
| 3 | ツール呼び出しと結果はプロバイダが振った呼び出し ID の文字列で結ぶ | 公式仕様・公開実装（研究なし） | 3 形式とも ID で対応づけ（`tool_call_id` / `call_id` / `tool_use_id`）。全調査対象の実装が同じ |
| 4 | 送信前に対応を検査する：答えのない呼び出し・呼び出しのない結果・ID の重複・呼び出しの直後以外にある結果はエラー | 公式仕様・公開実装（研究なし） | Anthropic は結果を直後の user メッセージの先頭にまとめることを要求（違反は 400）。Chat も同様のエラーが実在（公式文書には明記なし）。DSH の serializer が同じ検査でエラーにする。Codex と Pi は欠けた結果を補って送る（下記「未決の判断」） |
| 5 | 引数はモデルが出した JSON 文字列のまま保持し、Anthropic へ送るときだけ解析する | 公式仕様・公開実装（研究なし） | Chat と Responses は JSON 文字列、Anthropic は JSON オブジェクト。OpenAI は「常に正しい JSON とは限らない」と明記。Codex・DSH・opencode は文字列保持、Pi は解析済みと `argumentsError` を持つ |
| 6 | 思考（Reasoning）は会話に残し、同じ形式・同じモデルのときは再送用データごと返す | **研究（査読前・条件付き）**＋公式仕様 | 研究：思考を次ターンへ引き継ぐと BFCL multi-turn で約 2〜5% 向上（Liu et al., arXiv 2606.00135、査読前、効果は種のばらつきと同程度）、DeepTool（arXiv 2605.29568、査読前、数学・科学）、ReAct（ICLR 2023、間接）。仕様：Anthropic は tool 使用時に thinking を無改変で返すことを要求、Responses は reasoning 項目を元の順序で返すことを要求、DeepSeek はツール使用時に全ターンの reasoning を返さないと 400 |
| 7 | 別の形式・別のモデルへ送るときは思考を落とす（文章としても送らない） | 公開実装（研究なし） | Pi `transform-messages.js` L91-96：署名は作ったモデルにしか有効でなく、読める思考を文章で渡すと新しいモデルが自分の返答の一部と扱うため落とす（原文を確認）。DSH は署名だけ外して文章を残し、opencode は読める文章を送る（下記「未決の判断」） |
| 8 | assistant の部品は届いた順に保持する | 公式仕様（研究なし） | Anthropic は最新 assistant メッセージ内の thinking の並べ替え・部分削除を 400 にする。Responses は reasoning 前の履歴を変えると reasoning を捨てることがあると記載 |
| 9 | ツール結果は 1 件ずつ独立したメッセージにする（後で古い結果だけを畳めるようにする） | **研究（査読あり＋ワークショップ）** | SWE-agent（NeurIPS 2024）Table 3：SWE-bench Lite・GPT-4 Turbo で直近 5 件以外の観測を 1 行に畳むと 18.0%、全履歴は 15.0%（1 回の実行、検定なし。原文確認済み）。The Complexity Trap（arXiv 2508.21433、DL4C@NeurIPS 2025 ワークショップ）：古い観測を置き換えると要約と同程度の解決率で費用約半分、ただし思考モデル 1 構成では低下。保存は生のまま行い、畳むのは送信時の後続段階（本段階では畳まない） |
| 10 | ツール名は `^[A-Za-z0-9_-]{1,64}$` に制限 | 公式仕様 | Chat は最大 64、Anthropic は 1〜128、Responses は通常の関数に記載なし。3 形式すべてに通る範囲 |
| 11 | エラー結果の印（`is_error`）は Anthropic にだけ送り、Chat と Responses には本文だけを送る | 公式仕様 | `is_error` は Anthropic にのみ存在 |

## 自走性（長い作業を途中で止めずに続ける）の研究と、会話の型への影響

DeepAI Code と DSH の自走の仕組み（Notion「Ino Code（Rust新規実装）」の 2026-11-16 のメモ）について研究を調べた。仕組みそのものは段階 4（エージェントループ）以降で扱うが、会話の型はそれを載せられる必要がある。

| 仕組み | 研究上の状態 | 主な根拠と限界 |
|---|---|---|
| 「確認するまで続けよ」の指示 | 間接 | MAST（Cemri et al., arXiv 2503.13657、査読状態は未確認）：1,642 件のマルチエージェント実行で、検証の欠落・誤り（8.2%・9.1%）、早すぎる終了（6.2%）、手順の繰り返し（15.7%）を計測。目標の検証手順を足すと ChatDev で +15.6%（1 系統）。指示文そのものの比較はない |
| 止まった直後に「もう一度確かめよ」と差し込む（DeepAI の隠し継続） | 間接（類推） | Shen et al. "Thinking vs. Doing"（arXiv 2506.07976、掲載先は原文で未確認のため査読前として扱う）：完了の合図の後に再確認を促すと WebArena で 23.81% から上昇（表で確認できた最大値は 27.36%）、約 25% で行動が変わる。ただし再確認を重ねすぎると正答を誤答に変えて悪化（原文で確認）。Huang et al.（ICLR 2024）：外部の手がかりなしの自己修正は推論を悪化させうる → ツールの成功を条件にする DeepAI の制限と整合するが、コーディングエージェントでの検証はない。回数上限 3 の根拠はない |
| 同じ失敗の繰り返しの検出 | 間接 | 繰り返しは失敗と強く関係する（MAST、Bouzenia & Pradel ASE 2025、Mehtiyev & Assunção arXiv 2604.02547）。対策の効果を測った研究はない。Sinha et al.（ICLR 2026、arXiv 2509.09677）：自分の過去の誤りが文脈にあると誤りが増える |
| 文脈の圧縮 | 直接（査読前） | Fan et al., "An Empirical Study of Harness Design for Coding Agents"（arXiv 2609.20804、2026-09、査読前、原文で確認）：古いツール出力の省略・要約などの管理で、管理なしより SWE-bench Verified で平均 +35.7 ポイント（32K 窓）、+2.7 ポイント（128K 窓）（4 モデル×4 段階の平均。段階ごとの差は 128K で −1.2〜+7.6）。省略してから要約するのが安価で同程度。ReSum（arXiv 2509.13313、査読前）。窓の 80% で発動するといった閾値の根拠はない |
| 計画（todo）ツール | 直接だがモデル次第（査読前） | Fan et al.（原文で確認）：弱いモデル（Nemotron-3 30B）で 25.2% 対 13.6%、計画なしでは早々に止まる（中央値 40 ターン対 5、編集なしで終わる実行 27.8% 対 68.6%。この 2 つは要約経由の確認）。強いモデルでは差がないかわずかに不利（65.8 対 67.8、68.6 対 69.0、有意差なし） |
| 一時的な失敗の再試行 | なし | 工学上の信頼性の対策 |
| DSH のゴール周回（待機中に再指示、上限 256） | 直接の根拠なし | ターン数の研究（ICSE 2026、arXiv 2510.16786）：固定の上限より必要に応じて延ばす方式が 12〜24% 安く同等以上。追加のステップは失敗する実行に費やされやすい。再確認の重ねすぎは悪化（Shen et al.） |

会話の型に足すもの（このための判断。研究は上表の間接的な支持にとどまる）：

| # | 追加 | 理由 |
|---|---|---|
| 12 | user の発言に「出どころ」を持たせる：人（Person）か、ハーネスが差し込んだもの（Harness、種類つき）か | 継続の差し込み・再確認・ループの注意・todo の再提示は、回数の上限や条件（Shen et al. の「重ねすぎると悪化」、DeepAI の上限 3 回）を判定するために、人の発言と区別して数えられる必要がある。Pi は `custom_message`（表示フラグつき）、DSH は会話中の developer/system メッセージで区別している |
| 13 | assistant の発言に止まった理由（完了 / 長さ切れ / ツール使用 / エラー / 中断）を持たせる | 続けるかどうかの判断材料（DeepAI は「完了」で止まったときだけ継続を差し込む。DSH は長さ切れのときツール呼び出しを捨てる） |
| 14 | エラーまたは中断で終わった assistant の発言は、送信時に丸ごと除外する（その中のツール呼び出しも） | Pi と同じ。自分の過去の誤りが文脈に残ると誤りが増える（Sinha et al.、間接）。これで「中断したターンの答えのない呼び出し」は除外され、未決の判断 1 は「正常に終わった発言に答えのない呼び出しがあればエラー」に絞られる |
| 15 | 各メッセージに会話内で一意な ID を持たせる | 後の段階の圧縮（どこからどこまでを要約で置き換えたか。Pi の `firstKeptEntryId`、DSH の compaction イベント）と、保存・再開で範囲を指すため |

## 研究で裏付けられないこと（正直な整理）

- 型付きイベントの列にするか、普通のチャットメッセージにするか：OpenHands は event stream を採るが、その選択の比較実験はない。
- 呼び出し ID による結びつけ、役割の分け方：研究はなく、公式仕様の要請。
- 別モデルへの思考の扱い（判断 7）：研究はなく、実装ごとに対応が分かれる。

## 未決の判断（ユーザーの判断が必要）

1. 正常に終わった assistant の発言に答えのない呼び出しがあるとき、送信をエラーにするか（DSH 方式・案）、「結果なし」の結果を補って送るか（Codex・Pi 方式）。エラーや中断で終わった発言は判断 14 で除外するため、補う必要が生じるのは呼び出し側の不具合のときに限られる。
2. 別モデルへの思考：落とす（Pi・案）か、文章だけ残す（DSH・opencode）か。
3. TCP の `chat` に会話（`messages`）を受け付けさせるのを本段階で行うか、段階 3 のプロトコル設計でまとめて行うか（案：段階 3。本段階は core の型と変換のみ）。

## 調査した原典

- 公開ハーネス：Codex `codex-rs/protocol/src/models.rs`（`ResponseItem`、https://github.com/openai/codex/blob/f1ebc7b372bc837b7c6386113e867256d7cc7036/codex-rs/protocol/src/models.rs#L1133）と `core/src/context_manager/normalize.rs`；opencode `packages/schema/src/session-message.ts`（https://github.com/anomalyco/opencode/blob/367d24b70aec58e7827f04c47dadf2a9f6802053/packages/schema/src/session-message.ts#L41）；Vercel AI SDK `packages/provider-utils/src/types/model-message.ts`（https://github.com/vercel/ai/blob/8ba3940d3dd1f3cd3c4365bbaec505c6ef0eb1e3/packages/provider-utils/src/types/model-message.ts#L10）；OpenHands SDK `openhands/sdk/llm/message.py`（https://github.com/OpenHands/software-agent-sdk/blob/1d4c0249668c86d4327af51af1659330f8f0dbfc/openhands-sdk/openhands/sdk/llm/message.py#L126）；SWE-agent `sweagent/types.py`（https://github.com/SWE-agent/SWE-agent/blob/3ea751c087f32b16e039a2233dd6eefecef325d5/sweagent/types.py#L56）；Aider `aider/coders/chat_chunks.py`；opencodex `src/types/request.ts`（`OcxMessage`、L240-L398）。
- DSH（DeepSeek Harness、MIT、commit `39163dd30f703a79079e2d2446545cff3c453738`）：`packages/llm/llm/src/message.ts`、`types.ts`、`packages/llm/llm-deepseek/src/serialize.ts`（呼び出しと結果の検査）、`replay.ts`（同一モデルのときだけ署名を付ける）。https://github.com/deepseek-ai/deepseek-harness
- DeepAI Code 0.3.37 同梱の Pi 1.3.0（`@earendil-works/pi-ai`、MIT、https://github.com/earendil-works/pi。同梱の `types.d.ts`・`transform-messages.js` は npm 1.3.0 と SHA-256 一致）。
- 公式仕様（2026-11-16 取得）：OpenAI Chat Completions（https://developers.openai.com/api/reference/resources/chat/subresources/completions/methods/create）、Responses（https://developers.openai.com/api/reference/resources/responses/methods/create、https://developers.openai.com/api/docs/guides/reasoning）、Anthropic（https://platform.claude.com/docs/en/agents-and-tools/tool-use/handle-tool-calls、https://platform.claude.com/docs/en/build-with-claude/thinking）、DeepSeek（https://api-docs.deepseek.com/guides/thinking_mode）。
- 研究：Yang et al., "SWE-agent", NeurIPS 2024, DOI 10.52202/079017-1601（arXiv 2405.15793v3 Table 3 を確認）；Lindenbauer et al., "The Complexity Trap", arXiv 2508.21433（DL4C@NeurIPS 2025 ワークショップ）；Yao et al., "ReAct", ICLR 2023, arXiv 2210.03629；Liu et al., arXiv 2606.00135（査読前）；He et al., "DeepTool", arXiv 2605.29568（査読前）；Wang et al., "OpenHands", ICLR 2025, arXiv 2407.16741（設計の記述のみ、比較実験なし）。

## ユーザーの承認

2026-11-16、「案のとおりで実装」。設計案と未決の 3 点をいずれも案のとおりとする：(1) 正常に終わった発言に答えのない呼び出しがあれば送信をエラーにする、(2) 別の形式・別のモデルへは思考を落とす、(3) TCP への公開は段階 3。

## 実装時の細部（このプロジェクトの判断。研究・仕様による裏付けはない）

- エラー・中断で除外した発言の呼び出しに答える結果も一緒に除外する（判断 14 の「呼び出しも除外」の帰結。残すと呼び出しのない結果になる）。
- 送るものが何も残らない assistant の発言（他モデルの思考だけ、空の文章だけ）は送らない。空の文章部品は送らない（Anthropic は空の text ブロックを拒否する）。
- Anthropic：同じ役割が続くメッセージは 1 つにまとめる（ツール結果と直後の user の文章は、結果を先にして 1 つの user メッセージになる）。引数が空文字列なら `{}`、JSON オブジェクトでなければエラー（コード `invalid_conversation`）。
- Chat Completions：複数の文章部品・user の部品は空行（`\n\n`）でつないで 1 つの文字列にする。system は `role: "system"`。同じモデルの思考は `Replay` のオブジェクトの各フィールドを assistant メッセージへ足す。
- Responses：system は `instructions`。assistant の文章部品は 1 つずつ `message` 項目にし、届いた順を保つ。
- 再送用データ（`Replay`）は、作った形式とモデルが送信先と一致するときだけ手を加えずに送る。ツール呼び出しの `Replay` も同じで、一致しなければ ID・名前・引数から組み立てる（Anthropic の `tool_use` は常に組み立てる）。

## 実装・検証の状況

- 単体試験 6 件（`core/src/conversation.rs`）：3 形式それぞれの出力の形（system、思考の再送、文章、2 件の呼び出しと順不同の結果、エラー結果、続く user の発言）、別モデルでの思考の除外、エラー・中断の発言と結果の除外、対応の検査（結果なし・user の後の結果・呼び出しのない結果・ID の重複・不正なツール名）、Anthropic の引数がオブジェクトでないときのエラー。`cargo test -p inocode-core --release` 56 件成功、`cargo clippy --workspace --all-targets --release -- -D warnings` 警告なし。
- 未検証：実際のプロバイダへの送信（段階 2 でツール定義と合わせて行う）。各社が変換結果を受け付けるかは、公式仕様との照合のみで実通信では確かめていない。
