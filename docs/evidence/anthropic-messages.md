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

## 未決事項（ユーザー判断待ち）

- Logfare `/v1/messages` の `logfare/auto` は本文相当を `thinking_delta` 1 件として届ける（上記の実測）。Anthropic 公式仕様では `thinking_delta` は推論テキストであり回答本文ではないため、現実装（`text_delta` のみ連結）は仕様どおりで、このストリームでは回答テキストが 0 件になる。実装を変えないでよい根拠が 2 つある: Anthropic 仕様（`text_delta` のみが本文）、および公式 SDK の挙動（`thinking_delta` を回答に連結しない、LiteLLM #32357 で裏付け）。
- 判明した代替手段はすべて実測または根拠で否定済み: `thinking: {"type": "disabled"}` は無視される（⑤ 否定）。
- 残る選択肢（いずれもユーザー判断待ち）: ①現状維持（`logfare/auto` では回答が空になる）、③ Anthropic 経路で `logfare/auto` 以外のモデルを使う、④ Anthropic 経路で非ストリーミング要求にする。決定まで現実装を変更しない。

## 実装・検証の状況

- 実装: 完了（実装前に本記録を作成した。第3段階のような「実装後調査」ではない）。
  - `core/src/llm.rs` — `ProviderKind`（`openai` / `anthropic`、既定 `openai`）を追加。`LlmConfig::from_env` が `INOCODE_LLM_PROVIDER` を読む（不正値は `provider_config`）。`Llm::chat` が方式に応じて `/chat/completions`（Bearer）または `/messages`（`x-api-key` ＋ `anthropic-version: 2023-06-01` ＋ `max_tokens: 1024` ＋ `stream: true`）へ要求する。Base URL は末尾の `/` を除去してからパスを連結するため `/v1` が重複しない。
  - `core/src/sse.rs` — API 形式に依存しない SSE 増分パーサー（LF/CRLF/CR、複数 `data:` 行、BOM、16 MiB 上限）。
  - Anthropic 応答の解釈: `content_block_delta` の `text_delta` のみ本文として連結する。`message_stop` で正常終了。`message_start`・`content_block_start`/`_stop`・`message_delta`・`ping`・未知の型は無視（仕様の要求）。`error` イベントと `message_stop` を受けずに本文が終わる場合は `provider_stream` の失敗。
  - `server/tests/anthropic_tcp.rs` — 模擬 Messages API による統合テスト 11 件（要求形状とヘッダー、末尾スラッシュ、分割復元、`message_stop` なしの失敗、ストリーム内 `error`、非テキスト delta の除外、HTTP 429、ヘッダー待ち・ストリーミング中の `cancel`、混線、`ping`/`echo`）。
  - `server/tests/live_api.rs` — 実 API テスト 2 件（OpenAI 形式・Anthropic 形式）。キーと生成全文は出力しない。
  - `.github/workflows/protocol-test.yml` — 実 API ジョブで両形式を実行し、どちらかの結果行が出力されなければ失敗（未実行を成功とみなさない）。
- 検証済み（ローカル）: `cargo test --workspace --release` 95 件成功（うち Anthropic 新設 11 件）＋実 API 2 件は ignored。`cargo fmt --all -- --check`、`cargo clippy --workspace --all-targets --release -- -D warnings`、actionlint v1.7.12、`git diff --check` すべて成功。
- 検証済み（実 API）: 未実施。GitHub Actions（3 OS × 両形式、`LOGFARE_API_KEY`）での確認が必須条件。模擬 API だけでは完成扱いにしない。
