# Anthropic Messages 互換 API 対応とプロバイダー切り替え

## 対象と範囲

- 対象: 第3段階の OpenAI 互換接続に加え、Anthropic Messages 互換 API（Logfare `/v1/messages`）への接続と、`INOCODE_LLM_PROVIDER` 環境変数による接続先の切り替えを追加する。
- 範囲外: Codex OAuth、Claude OAuth、エージェントループ、ツール実行、会話履歴 DB、GUI。実装しない。プロバイダー切り替えのための汎用設定管理機構・フレームワークも追加しない。
- TCP の 16 バイトヘッダー、JSON/生バイナリのフレーム仕様、`ping` / `echo` / `chat` / `cancel`、イベント形式は変更しない。OpenAI 互換 API の動作と実 API テストは維持する。
- 第3段階で承認保留としたプロバイダー無応答時のタイムアウト値と同時生成数上限は、本段階でも決定・実装しない。

## 関連ソースコード

- `core/src/llm.rs` — `LlmConfig` に接続方式の選択を追加。`Llm::chat` が方式に応じて Chat Completions または Messages 形式の要求を組み立てる。
- `core/src/sse.rs` — Server-Sent Events のフレーミング解析（行端末、複数 `data:` 行の連結、BOM、上限）。API 形式に依存しない部分を担う。
- `server/tests/anthropic_tcp.rs` — 模擬 Messages API と実 TCP による統合テスト（新設）。
- `server/tests/live_api.rs` — 実 Logfare API テスト。OpenAI 形式と Anthropic 形式の 2 件（Anthropic は新設）。
- `.github/workflows/protocol-test.yml` — 両形式の実 API テストを 3 OS で実行（手動実行のまま）。

## 根拠の状態

- **UNSUPPORTED_APPROVED** — 学術研究による直接の裏付けは確認できていない（以下に区別して記載）。承認の証跡は本タスクの実装指示（第4段階、2026-11-15）である。Anthropic 互換 API の追加、`INOCODE_LLM_PROVIDER` による選択、`x-api-key` と `anthropic-version: 2023-06-01` ヘッダー、`max_tokens: 1024`（Logfare 公式サンプルの値）、`message_stop` を正常終了とする扱いはすべて指示で確定している。
- 手順: 今回は AGENTS.md 第3節に従い、実装前に本記録と公式仕様の確認を行った。第3段階で行った「実装後調査」の手順違反は繰り返さない。

## 学術的な主張・不足点

- 本機能は既存 API 形式との互換性の実装であり、新たな有効性を主張する機能ではない。適用可能な学術研究は確認していない。プロトコル互換の実装を研究根拠で裏付ける文献は検索対象外であり、**研究で裏付けられたとは主張しない**。
- ユーザーが指定した表示・知覚に関する研究（Bhat、Zhou、Tan）を本実装の根拠に用いない（第3段階の記録に明記済み）。

## 学術資料

- 該当なし。互換性の根拠は公式 API 仕様（下記）であり、学術研究ではない。

## その他の根拠（研究とは区別する）

- **Logfare 公式ドキュメント**（https://logfare.ai/docs、`https://logfare.ai/docs.md`、2026-11-15 取得）:
  - `/v1/messages` は「Anthropic Messages API」として提供される。
  - 認証は Bearer トークン。`x-api-key` ヘッダーも受け付ける（「which is what Anthropic clients send」）。
  - 全エンドポイントは `https://logfare.ai/v1` の下にある。
  - **ストリーミングの記載は `/v1/chat/completions`（OpenAI 形式）のみ**（2026-11-15 再取得で全文確認）。「Chat completions (streaming supported)」「Set `"stream": true` to stream over SSE」は OpenAI 形式の節だけにあり、`/v1/messages` の節には `stream` パラメータの記載もストリーミングへの言及もない（curl 例も `stream` なし）。
- **Anthropic 公式 Thinking ドキュメント**（https://platform.claude.com/docs/en/build-with-claude/thinking、troubleshooting 同、2026-11-15 取得）:
  - Messages API では thinking は**既定で有効**。「`thinking: {"type": "disabled"}` turns it off on models that accept that value」— 無効化する公式パラメータが存在する。
  - thinking テキストを応答から外したい場合は `display: "omitted"` を使う（thinking ブロックは空で返る）。
- **LiteLLM issue #32357**（https://github.com/BerriAI/litellm/issues/32357、2026-07-07）:
  - OpenAI 互換の推論モデル（`reasoning_content`）を Anthropic `/v1/messages` に写す実装で、`thinking_delta` が本文ブロックに流れ込むと**公式 Anthropic SDK はそれを黙って捨て、Claude Code は「応答なし」としてターンを中断する**という既知の相互運用性問題。公式 SDK の挙動としても `thinking_delta` は回答本文に連結されない。
  - よって「`thinking_delta` を回答に連結する」対応（前節の選択肢 ②）は、Anthropic 仕様・公式 SDK のどちらの根拠とも矛盾する。採らない。
- **実装判断の更新（仕様に基づく。研究ではない）**:
  - 新たな候補（⑤）: Anthropic 要求に公式パラメータ `thinking: {"type": "disabled"}` を加え、推論なしの `text` ブロック（`text_delta`）を得る。Logfare がこのパラメータを尊重するかは未確認で、診断テストで実測してから判断する。
- **Anthropic 公式ストリーミング仕様**（https://platform.claude.com/docs/en/build-with-claude/streaming、2026-11-15 取得）:
  - 正常終了までのイベント列: `message_start` → `content_block_start` → （任意回の `ping` と `content_block_delta`）→ `content_block_stop` → 1 回以上の `message_delta` → 最後の `message_stop`。
  - `content_block_delta` の `delta.type` は `text_delta`（テキスト）、`input_json_delta`（ツール入力の部分 JSON）などがある。テキストとして連結するのは `text_delta` のみ。
  - `message_delta` の usage は累積値であり、本文テキストを運ばない。
  - `ping` イベントは任意回現れる。
  - `error` イベント（例: 非ストリーミング時の HTTP 529 に対応する `overloaded_error`）: `{"type":"error","error":{"type":...,"message":...}}`。**ストリーム内エラーは正常終了ではない**。
  - 「new event types may be added, and your code should handle unknown event types gracefully」— 未知のイベント型は無視するのが仕様上の要求。
  - 例のストリームは `event:` 行と `data:` 行のペアで構成され、`data` の `type` がイベント名と一致する。
  - **thinking 無効化の実測**（run 43727807091、コミット ea3d236、3 OS 同一結果）: 公式パラメータ `thinking: {"type": "disabled"}` を付けても応答形状は無指定時と完全に同一（`message_start`、`thinking` ブロック、`thinking_delta` × 1、`content_block_stop`、`message_delta`、`message_stop`）。Logfare はこのパラメータを無視する。よって選択肢 ⑤（パラメータ追加）は実測で否定された。
  - **代替モデルの部分実測**（run 43729462248 の診断、macOS）: `gemma-4-31b-it` は `text` ブロックを返したが `text_delta` は 1 件のみ（本文全体が 1 イベント）。`qwen-3.8-27b`・`gemma-4-26b` は thinking のみ。追加のモデルプローブはフリーティアのレート制限（HTTP 429）に当たるため診断からは外した（以後は自動実行しない）。
  - **ワークフローの構造修正**: 実測ステップは前段の Logfare ステップが失敗するとスキップされるため、公式 API 実測ステップに `if: always()` を付けて常に実行するようにした（run 43729462248 ではスキップされ結果が観測不能だった）。
- **公式 Anthropic API の実測**（`CLAUDE_API_KEY` 使用、Haiku のみ使用、テキストは出力しない）:
  - 直接プローブ（claude-haiku-5-5、`stream: true`、同じ要求形状）: HTTP 200、`message_start`、`text` ブロック、`ping`、`text_delta` × 25（約 1.2 秒に分散して到着＝本物のストリーミング）、`content_block_stop`、`message_delta`、`message_stop`。公式 API は `text_delta` のストリームを返す。
  - サーバー経由の実測（`INOCODE_LLM_BASE_URL=https://api.anthropic.com`、`claude-haiku-5-5`、実 TCP・実装そのもの）: `deltas=80 chars=372 first_delta=4675ms total=6441ms` で成功。実装は公式 API と正しく動作する。
  - **URL 規約の修正**（公式仕様・SDK・Logfare ドキュメントに基づく）: Anthropic クライアントは Base URL に `/v1` を含めず `/v1/messages` を付けて呼ぶ（公式ドキュメントの curl 例、SDK の既定 `base_url=https://api.anthropic.com`、Logfare ドキュメント「Anthropic SDK の Base URL は `/v1` なし」）。実装は `base_url` の末尾 `/v1` を除去してから `/v1/messages` を付けるため、`https://api.anthropic.com` と `https://logfare.ai/v1`（既定）のどちらでも同じエンドポイントに届く。これを Url 構築の単体テストと模擬統合テスト（`/v1` なし Base）で検証した。
- **実 API 観測**（GitHub Actions、`LOGFARE_API_KEY` 使用、テキストは出力しない）:
  - run 43725234427（コミット 8ee9b80）: 3 OS とも `/v1/messages`（`stream: true`）は本文全体を 1 つの delta で返し `completed` に至った（first_delta と total がほぼ同一）。OpenAI 経路は同一プロンプトで 4〜5 delta。
  - run 43726199798（コミット 872b7af）: 3 OS とも `/v1/messages` は 300 語以上の生成を依頼しても text delta 0 件で `completed` に至った。OpenAI 経路は同一環境で 4〜9 delta。
  - この挙動は、Logfare 公式ドキュメントが `/v1/messages` のストリーミングを記載していないことと整合する（上記）。
  - 診断 run 43726962574（コミット ade1cf5、`logfare_messages_stream_event_kinds`、3 OS で同一結果）: `logfare/auto` で `/v1/messages` ＋ `stream: true` に投げると、HTTP 200・`content-type: text/event-stream` で、イベントは `message_start`、`content_block_start`（content_block.type = **`thinking`**）、`content_block_delta` × 1（delta.type = **`thinking_delta`**）、`content_block_stop`、`message_delta`、`message_stop` がこの順に 1 回ずつ。**`text` ブロックも `text_delta` も 1 件も届かない**。本文相当が `thinking` ブロック内に 1 イベントで届く（Logfare がルーティングした上流モデルの推論出力を Anthropic 形式の最初の content block に写しているとしか説明がつかない）。同一キー・同一環境で `/v1/chat/completions` は 4〜9 delta の本文をストリームする。
- 実装判断（仕様と指示に基づく。研究ではない）:
  - 接続方式は `LlmConfig` の値で選び、既定は `openai`（既存動作を壊さない）。選択のみで設定管理の仕組みは作らない。
  - URL は既存の `INOCODE_LLM_BASE_URL`（既定 `https://logfare.ai/v1`）＋パス連結で組み立て、`/v1` が重複しない。
  - SSE フレーミングの解析（行端末・複数行連結・BOM・上限）は API 形式に依存しないため `sse.rs` に残し、`[DONE]` / Chat Completions 形状の解釈は OpenAI 形式側、イベント `type` の解釈は Anthropic 形式側に分ける。
  - `message_stop` で正常終了、`error` イベントと途中切断（`message_stop` を受けずに本文が終わる）は `provider_stream` の失敗。未知のイベント型は無視する（仕様の要求）。

## ユーザーの承認

- 参照元: 本タスクの実装指示（第4段階 §2〜§5、§7）。実 API で `/v1/messages` の生成・ストリーミングを確認することが必須条件で、模擬 API だけでは完成扱いにしないと明示されている。
- 承認の性質: 機能範囲の確定という形での承認。学術根拠を伴わない（本機能に学術的根拠は不要なため、第3節の停止条件には該当しない設計判断として記録する）。

## 根本原因（2026-11-15 確定・実測と一次資料による）

- **事実**: thinking トークンは `max_tokens` と予算を共有する。出典: Anthropic 公式 Thinking ドキュメント（https://platform.claude.com/docs/en/build-with-claude/thinking、「Thinking tokens count toward your `max_tokens` limit」）。同じ知見が本番プロキシ lidge-jun/opencodex の実装にも記録されている（`src/adapters/anthropic.ts`: 既定 8192、「Omitting `thinking` lets a default-on model think anyway, and thinking shares the caller's `max_tokens` — which truncates a small-budget request before it can emit its stop sequence (#545)」、adaptive モデルでは「a default 8192-token request can spend everything on thought and return empty text」(#5824)。参考コミット `decb08b`（messages baseUrl 正規化とストリーム異常の堅牢化）、`bb9da3a`（Messages 変換の実装））。
- **実測**（ローカル直接 HTTP、同一 300 語プロンプト、テキストは非出力、2026-11-15）:
  - Logfare `/v1/messages` `logfare/auto` `max_tokens=1024`: thinking のみ 2154 字、**`stop_reason=max_tokens`**、output_tokens=1024、本文 0 字 → 予算を思考で消費し切り、本文が出ない。
  - Logfare `/v1/messages` `logfare/auto` `max_tokens=4096`: thinking 4834 字 → **本文 1013 字**、`stop_reason=end_turn`、output_tokens=2912。
  - Logfare `/v1/messages` `gemma-4-31b-it` `max_tokens=1024`: 本文 1724 字、**`text_delta` 1 件のみ**（本文全体が 1 イベント）、`stop_reason=end_turn`。モデル上限は `max_completion_tokens`=16384（`GET /v1/models` で確認）。
  - 公式 `/v1/messages` `claude-haiku-5-5`（モデル ID は公式 Models overview と `/v1/models` の両方で確認）`max_tokens=1024`: text_delta 175 件（増分）、output_tokens=870（1024 に対する余裕が小さく、CI Windows での 0 delta 失敗と整合）。
  - 公式 `/v1/messages` `claude-haiku-5-5` ＋ `thinking:{"type":"disabled"}`: text_delta 232 件、thinking ブロックなし。**公式 API はこのパラメータを尊重する**（Logfare は無視する、上記実測）。
- **結論と修正**: 旧要求の `max_tokens: 1024` は、thinking が有効なモデル（Logfare/auto のルーティング先、公式 haiku-5-5 の adaptive）で本文の余裕がほぼ/全くなく、`stop_reason=max_tokens` による空回答を生む。Anthropic 要求の `max_tokens` を **8192** に引き上げる（opencodex の既定値と同じ。`thinking: {"type": "disabled"}` は受理可否がモデルごとに分かれ、Logfare では無視されるため無条件送信はしない）。実測で 8192 未満の上限が確認されているプロバイダ・モデルはない（Logfare gemma 16384、公式 haiku 128K）。
- **Logfare 側の制約（InoCode 実装不具合とは区別）**: Logfare `/v1/messages` は本文を 1 つの `text_delta` で返す（B1 実測、CI 複数回）。増分ストリーミングは Logfare が提供しない。公式 API は増分で配る。SSE 解析・URL・応答解釈の実装不具合は確認されなかった（直接 HTTP と InoCode 経由の比較、チャンク境界分割再現テストで仮説 3・4・6 を否定）。

## テスト判定と診断の修正（仕様に基づく）

- Anthropic SSE 仕様はテキスト delta の最小件数を保証しない（`content_block_delta` は任意回）。旧実 API テストの `deltas >= 2` は仕様外の要求だったため、Messages 実 API テストの判定を「本文非空 ＋ 正常終了（completed）」に修正し、delta 件数は結果行に出力するのみとした。逐次配送（任意バイト境界での分割復元）は模擬 API テスト 12 件で担保する。OpenAI 経路の既存判定（≥2）は変更しない。
- 診断コードのチャンク境界バグ（行が HTTP チャンク境界で分断され `data:` が誤分割・誤集計される）を修正。CI ubuntu で観測された `<invalid json>=1` 誤集計の原因。
- CI の `cargo test | tee` は `cargo test` の失敗を隠すため、両実 API ステップに `set -o pipefail` を追加した。
- **8192 受理の検証**: 公式 haiku-5-5 と Logfare（gemma-4-31b-it、logfare/auto）の Messages 経路を、`max_tokens=8192` の実装そのものでローカル実 API 実行して確認する（下記「実装・検証の状況」）。

## 残る制約

- Logfare `/v1/messages` の増分ストリーミング非提供（本文 1 `text_delta`）は Logfare 側の制約。対応はプロバイダー側待ち。
- `thinking: {"type": "disabled"}` の送信は公式 API で有効（実測）、Logfare で無視（実測）、受理しないモデルの存在あり（opencodex の記録）。無条件送信は仕様の推測となるため未実装。必要ならユーザー承認の上で対応可能。

## 実装・検証の状況

- 実装: 完了（実装前に本記録を作成した。第3段階のような「実装後調査」ではない）。
  - `core/src/llm.rs` — `ProviderKind`（`openai` / `anthropic`、既定 `openai`）を追加。`LlmConfig::from_env` が `INOCODE_LLM_PROVIDER` を読む（不正値は `provider_config`）。`Llm::chat` が方式に応じて `/chat/completions`（Bearer）または `/messages`（`x-api-key` ＋ `anthropic-version: 2023-06-01` ＋ `max_tokens: 8192` ＋ `stream: true`）へ要求する。Anthropic の URL は設定 Base の末尾 `/v1` を除去してから `/v1/messages` を組み立てる（公式クライアント規約）。
  - `core/src/sse.rs` — API 形式に依存しない SSE 増分パーサー（LF/CRLF/CR、複数 `data:` 行、BOM、16 MiB 上限）。
  - Anthropic 応答の解釈: `content_block_delta` の `text_delta` のみ本文として連結する。`message_stop` で正常終了。`message_start`・`content_block_start`/`_stop`・`message_delta`・`ping`・未知の型は無視（仕様の要求）。`error` イベントと `message_stop` を受けずに本文が終わる場合は `provider_stream` の失敗。
  - `server/tests/anthropic_tcp.rs` — 模擬 Messages API による統合テスト 12 件（要求形状とヘッダー、末尾スラッシュと `/v1` なし Base、分割復元、`message_stop` なしの失敗、ストリーム内 `error`、非テキスト delta の除外、HTTP 429、ヘッダー待ち・ストリーミング中の `cancel`、混線、`ping`/`echo`）。
  - `server/tests/live_api.rs` — 実 API テスト 3 件（OpenAI 形式・Anthropic 形式・Messages 診断）。キーと生成全文は出力しない。
  - `.github/workflows/protocol-test.yml` — Logfare 両経路と公式 Haiku の実 API テストを 3 OS で実行し、結果行が出なければ失敗（未実行を成功とみなさない）。`set -o pipefail` で失敗隠蔽なし。
- 検証済み（ローカル）: `cargo test --workspace --release` 127 件成功・0 失敗（実 API 3 件は ignored）。`cargo fmt --all -- --check`、`cargo clippy --workspace --all-targets --release -- -D warnings`、`cargo build --workspace --release`、actionlint v1.7.12、`git diff --check` すべて成功。独立監査（サブエージェント）で F1 を解消済み、他は OK。
- 検証済み（実 API・ローカル、2026-11-15）:
  - Logfare OpenAI `logfare/auto`（`max_tokens` 送らず、変更なし）: deltas=8、成功。
  - Logfare Messages `gemma-4-31b-it`（`max_tokens=8192` 受理）: deltas=1 chars=1556、成功。
  - Logfare Messages `logfare/auto`（**修正後**: `max_tokens=8192` 受理、thinking＋本文）: deltas=1 chars=877、成功（修正前は thinking のみで本文ゼロ）。
  - 公式 API `claude-haiku-5-5`（`max_tokens=8192` 受理）: deltas=126 chars=711、成功。
- 最終検証: GitHub Actions（3 OS × Logfare 両経路＋公式 Haiku）を別途実行する（コミット・push 後）。
