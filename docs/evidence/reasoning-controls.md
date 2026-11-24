# 思考・推論の指定（thinking / reasoning）

- **対象と範囲：** リクエストに思考の深さ（effort）と要約の表示（summary）を付ける。3 形式すべて。設定は `INOCODE_LLM_REASONING`（none / minimal / low / medium / high / xhigh / max。ultra は max として扱う）と `INOCODE_LLM_REASONING_SUMMARY`（auto / concise / detailed / none）。深さが未設定なら、モデルのカタログの既定値、それもなければ medium。要約の既定は auto。範囲外：要求ごとの指定（段階 3 の TCP で扱う）、手書きのモデル別段階表（opencodex の `reasoningEfforts` 等）、受け付けない深さの丸め・置き換え、署名拒否時の再送（opencodex `anthropic-thinking-recovery.ts`）、`temperature` の除去（inocode は送らない）、`tool_choice` の書き換え（送らない）、各社固有の `thinking_budget`・`reasoning_split`・`reasoning_details`（対象の接続先に該当しない）。
- **関連コード：** `core/src/reasoning.rs`（`Effort`・`Summary`・`Reasoning`、`ALL_EFFORTS`、`FALLBACK_EFFORT`、`apply`、`anthropic_beta`、Claude のモデル判定）、`core/src/llm.rs`（`LlmConfig::reasoning`、`Llm::effort`（カタログ既定の参照、1 回だけ取得）、`ModelInfo::reasoning_levels`・`default_reasoning`、`request_body`、`destination`、`anthropic_chat` の beta、Responses の再送用 reasoning 項目から `status` を除く）、`server/src/lib.rs`（TCP `models` の `reasoning_levels`・`default_reasoning`）。
- **根拠の状態：** UNSUPPORTED_APPROVED（2026-11-16、「thinkingしなきゃ困る。opencodexに実装あるでしょ。その通りにフル機能使えるようにしなさい。」、カタログについて「「受け付ける深さ」と「既定値」があるならそれを使い、ないならスライダー全部使えるようにしなさい。受け受けない場合はエラーそのまま出す」）。有効性の研究ではなく、参照実装と各社の仕様への追従。
- **参照実装：** opencodex（`D:\work\software\OSS\opencodex\source`、2026-11-16 時点のローカルの写し。git 履歴なし）。
  - 段階と正規化：`src/reasoning-effort.ts:6-13, 48-50, 209-270`（ultra→max、none は送らない、段階表がなければ丸めない）。
  - Anthropic：`src/adapters/anthropic-model-contract.ts:16-21, 43, 66-68, 77-80, 96-100`（モデル判定）、`src/adapters/anthropic.ts:615-642, 1121-1197`（adaptive は `thinking:{type:"adaptive", display:"summarized"}` と `output_config.effort`（minimal→low）、旧方式は `budget_tokens`＝1024/4096/8192/16384/24576/32000、`max_tokens` の式、none のとき disabled / between_tools、api.anthropic.com の Opus 5.5 は display を updates（要約指定が concise/detailed なら summarized））、`:1277`（updates には beta `thinking-display-updates-2026-08-18`）。
  - Responses：`reasoning:{effort, summary}`、summary none は送らない（`src/adapters/openai-responses/reasoning.ts:247-254`）、ChatGPT バックエンドの丸め（`src/codex/catalog/effort.ts:56-96`：スナップショットにないモデルは max→xhigh、5.6 系を除く）、Muse は none を受け付けない（`src/providers/registry/model-seeds.ts:239-258`）、再送する reasoning 項目から `status` を除く（`reasoning.ts:63-173`）。
  - Chat：`reasoning_effort`、none は送らない、DeepSeek の段階 low/high/max（medium・high・xhigh→high、`model-seeds.ts:533-561`）。
- **カタログ（ユーザーの指示による）：**
  - 接続先のモデル一覧が「受け付ける深さ」と「既定値」を出す場合はそれを使う。現在これを出すのは ChatGPT の一覧（`models[].supported_reasoning_levels[].effort`、`default_reasoning_level`。opencodex `src/codex/catalog/discovered-natives.ts:46-75`）。TCP `models` はそれをそのまま返し、出さないモデルには 7 段階すべて（none〜max）を返す（`default_reasoning` は null）。
  - 深さが未設定のとき、ChatGPT ではそのモデルの既定値を使う。一覧はプロセスごとに 1 回だけ取得し、取得に失敗したら「カタログなし」として medium。他の接続先は一覧に既定値がないため medium。
  - 丸めない：指定された深さをそのまま送り、受け付けない接続先のエラー（HTTP の状態と本文）をそのまま返す。opencodex の丸め（ChatGPT の max→xhigh、DeepSeek の medium→high 等）は入れない。none は「送らない」意味として扱う（ChatGPT だけは none という段階として送る）。
- **このプロジェクトの判断（参照実装からの簡略化）：**
  - 手書きのモデル別段階表は持たない。
  - 既定の後退値を medium にする（opencodex のクライアントが何も選ばないときの後退値 `codex/catalog/effort.ts:322-328`）。これにより、以前は思考を指定しなかった要求にも medium が付く。
  - 旧方式の上限は opencodex の「モデルの最大出力と 32000 の大きい方」のうち、モデルの最大出力を知らないので 32000 とする。adaptive の上限は 40192。
- **実装・検証の状況：**
  - 単体試験 `core/src/reasoning.rs` 5 件（モデル判定、Anthropic の形と `max_tokens`・`budget_tokens`、none の扱い、Opus 5.5 の display と beta、Responses と Chat のフィールド（丸めないこと）、段階の解析）、`core/src/llm.rs` の本文試験と一覧の解析（`supported_reasoning_levels`・`default_reasoning_level`）。模擬サーバー：ChatGPT で深さ未設定ならモデルの既定値を送り、一覧は 1 回だけ取得（`gpt_chat_without_a_chosen_effort_uses_the_model_default`）、一覧を出さないモデルは TCP `models` で 7 段階すべて（`muse_models_uses_the_subscription_key_and_muse_headers`）。`cargo test --workspace --release` 全件成功、clippy 警告なし。
  - 実通信（2026-11-16、カタログ対応の前の版で実施。カタログ対応の後は実施していない。`server/tests/live_tools.rs`、ツールを 1 回呼ばせて結果を返す 2 ターン。プロンプトは推論を要するもの）：
    - Claude OAuth `claude-haiku-5-5`：high で署名つき思考 1 件 → 2 ターン目にそのまま再送して完了（署名つき thinking の再送を確認）。medium・none も成功（簡単なプロンプトでは adaptive が思考しなかった）。
    - Muse `muse-spark-1.3-contributor`：high で再送用データつき思考 1 件、完了。
    - DeepAI `glm-5.3-flash`：none・high とも思考 1 件（`reasoning_content`）、完了。`reasoning_effort` を受け付ける。
    - Logfare `logfare/auto`（既定 medium）：Anthropic 形式（旧方式 `budget_tokens`、思考は署名なし）・Responses・Chat すべて完了。
  - 未検証：GPT OAuth（使用量の上限）、Opus 5.5 の updates 表示と beta（許可されたモデルではない）、sonnet 5.5 の between_tools。

## 思考を流れるまま見せる・言語は決めない

- **ユーザーの判断：**「思考言語を強制しない。qwenごかが中国語で考えるし英語最善とも考えない。思考は受信して出すべき。」（2026-11-25）。UNSUPPORTED_APPROVED。
- **思考の言語：** inocode は思考の言語を指示しない。指示するのは返事の言語だけ（`core/src/language.rs`）。
  - 調べた研究：Qi et al. 2025（arXiv 2505.22888、EMNLP 2025 Findings）、EfficientXLang（arXiv 2507.00246）、Yong et al. 2025（arXiv 2505.05408、査読前）。
  - 3 つとも公開モデルを数学・科学の問題で試したもので、Claude や GPT の結果は無い。どの言語が最もよいかの結論は出ていない。
- **思考の表示：** プロバイダーが流す思考は、届いた順にクライアントへ送る。
  - 送り方：`{"id": <要求 ID>, "event": "thinking", "text": <string>}`。
  - 読むもの：
    - Anthropic：`thinking_delta`。
    - Chat Completions：`reasoning_content`／`reasoning`。
    - Responses：`response.reasoning_summary_text.delta`、`response.reasoning_text.delta`。
  - 保存されるのは、答えが終わったときの思考全体（`message` イベント）。流した断片は保存しない。
  - Claude 4.7 以降と 5 系は既定では思考を返さない（display が omitted）。inocode は要約の指定が none でない限り `display` を付けて受け取る（上の Anthropic の項）。
- **テスト：** `llm::tests::thinking_is_streamed_in_every_format`（3 つの形式で、思考と答えが別々に流れる）、`anthropic_events_reduce_to_text_stop_and_ignored`。
- **未検証：** 本物のプロバイダーで、思考が届いた順に画面へ出ること。

