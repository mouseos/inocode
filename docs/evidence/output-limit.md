# 出力上限：モデルの上限を送る、切れたら続きを促す

- **対象と範囲：**
  - 1 回の応答の出力上限に、モデル自身の上限を使う。値は提供元の公式のモデル一覧から取り、取れないときだけ models.dev を使う。
  - 応答が出力上限で切れたら、同じターンの中で続きを促す。
  - 範囲外：入力が文脈長に近いときに上限を下げること、モデルを切り替えて再試行すること。
- **関連コード：**
  - `core/src/llm.rs`：`ModelInfo::max_output`、`Llm::output_limit`、`models_dev_output`。
  - `core/src/reasoning.rs`：`raise_anthropic_max`。
  - `core/src/turn.rs`：`next_step` の `Step::Resume`、`RESUMES`、`drop_cut_calls`。
- **根拠の状態：** UNSUPPORTED_APPROVED。
  - 学術研究の調査はしていない。
  - 値の出どころは各社の公式仕様。続きの促し方は公開実装（Claude Code）に合わせた。

## 各提供元の上限の出どころ（2026-11-19 の調査）

| 提供元 | 公式の上限の欄 | 上限の指定 | inocode の扱い |
|---|---|---|---|
| Anthropic（API キー） | `GET /v1/models` の `max_tokens`（「この model で `max_tokens` に指定できる最大値」。null もありうる）。https://platform.claude.com/docs/en/api/models/list | 必須（https://platform.claude.com/docs/en/api/messages/create） | 一覧の値を送る |
| Claude OAuth | 同じエンドポイント（OAuth のベータヘッダー付き）。Claude Code 2.1.181 は一覧から `max_tokens` を読むキャッシュを持つが、この版では無効にしてあり、モデルごとの固定表（既定と上限。例：opus-4-6 は 64000／128000）を使う（`refs/claude-2.1.181/bundle-slice.js`） | 必須 | API キーと同じ。OAuth で欄が埋まるかは実 API では未確認 |
| OpenAI Chat Completions・互換サーバー | 公式にはない（`/v1/models` は id・created・owned_by など）。OpenRouter は `top_provider.max_completion_tokens`。Logfare は `max_completion_tokens`（`anthropic-messages.md` の観察） | 任意 | 送らない（従来どおり） |
| OpenAI Responses | 公式にはない | `max_output_tokens` は任意 | 送らない（従来どおり） |
| ChatGPT バックエンド（GPT OAuth） | `/models` の行は `context_window` などで、出力上限の欄はない。Codex は `max_output_tokens` を送らない（`codex-rs/codex-api/src/common.rs`） | 送らないのが Codex の動作 | 送らない（従来どおり） |
| Muse Code・DeepAI | 公式の欄は見つからなかった | Responses／Chat 形式で任意 | 送らない（従来どおり） |

- 上限を送る必要があるのは Anthropic 形式だけ。それ以外は従来から送っておらず、提供元側の上限がそのまま効く。
- models.dev（https://models.dev/api.json、`<提供元>.models.<モデル>.limit.output`）：
  - 有志が保守する一覧（sst/models.dev。MIT とされるが、ライセンス本文は読んでいない）。
  - 2026-11-19 の取得では、claude-haiku-5-5・claude-opus-5-5・claude-sonnet-5-5 はどれも出力 128000 で、Anthropic の文書と一致した。
  - api.anthropic.com に送るときだけ、公式の一覧に値がない場合の代わりに使う。`anthropic` の項目は api.anthropic.com のモデルを表すため。ほかの Anthropic 互換サーバーには当てはめない。
- どちらにもないとき：従来の固定値（思考の深さから決める 8192〜40192）を送る。

## 関係する仕様

- 思考のトークンも `max_tokens` に含まれる。旧方式の思考予算（`budget_tokens`）は `max_tokens` より小さくなければならない。そのため上限が予算以下なら、予算を「上限 − 4096」（最低 1024）に下げる。
- Claude 4.5 以降は、入力＋`max_tokens` が文脈長を超えても受け付ける。生成が文脈長に達すると `model_context_window_exceeded` で止まる。それより前のモデルはベータヘッダーがないと検証エラーになるが、ここでは扱っていない。https://platform.claude.com/docs/en/build-with-claude/context-windows
- `max_tokens` は出力のレート制限（OTPM）の計算に入らない。大きくしても不利はない。https://platform.claude.com/docs/en/api/rate-limits
- 大きい `max_tokens` はストリーミングで送る必要がある。inocode は常にストリーミング。

## 切れたら続きを促す

- 公開実装：Claude Code 2.1.181 は、出力上限で切れると利用者のメッセージとして続きを促す文を入れ、最大 3 回まで繰り返す（`FFp=3`、`max_output_tokens_recovery`）。
- inocode の動作：
  - 切れた応答の後に、ハーネスのメッセージ（種類 `resume`）で続きを促す。文言は独自のもので、謝らず、繰り返さず、続きから書き、残りが長ければ分けるよう求める。
  - 続けて 3 回切れるまで促す。4 回目に切れたら、ターンを `length` で終える。
  - 切れずに次へ進めば、数え直す。
- 切れた応答の中で、引数の JSON が途中までのツール呼び出しは、記録から外す。JSON として読めない引数は、どの提供元にも送り返せないため。
  - 引数が読める呼び出しは、従来どおり実行せず「実行していない」と返す。
- プロジェクト内の観察：
  - 8 問まとめたカーネルの課題（`observation-masking.md`）で、Haiku 5.5 の応答は 16,384 の上限で何度も切れた。
  - 答えの一覧を書く応答が 6 回続けて切れ、0 点になった回もあった。
  - この変更後、同じ課題（composite-03）を Claude OAuth の Haiku 5.5 で 1 回流した（`refs/kernel-bench`、`runs-limit`）。
    - 前回：出力上限で 6 回切れ、得点 0、要求 37 回、812 秒。
    - 今回：出力上限で切れたのは 0 回、得点 0.996（適合率 1.0、再現率 0.993）、要求 20 回、436 秒。
    - 1 回だけの比較で、モデルの答えのばらつきと区別できない。
    - 送った `max_tokens` の値は、記録に残らないので直接は見ていない。切れなかったことからの推測にとどまる。
    - 切れなかったため、続きを促す処理は実 API では動いていない。

## ユーザーの承認

- 2026-11-19：「公式手段を徹底的に調べて、取れないならmodels.devで。切れたら続き促すこと。」

## 実装・検証の状況

- 単体テスト：
  - `llm.rs`：一覧の `max_tokens`・`max_completion_tokens`・`top_provider.max_completion_tokens` の読み取りと、models.dev の項目の読み取り。
  - `reasoning.rs`：上限で置き換えることと、予算を上限より下に保つこと。
  - `turn.rs`：3 回まで促し、4 回目で終えること。
- TCP テスト：
  - `anthropic_tcp.rs`：一覧に 128000 とあれば、毎回の要求の `max_tokens` が 128000 になり、一覧は 1 回だけ読むこと。
  - `turn_tcp.rs`：切れた応答の後に続きを促す文が送られ、途中までのツール呼び出しは送り返されないこと。続けて 4 回切れたら `length` で終わること。
- 未検証：
  - 実 API で一覧の `max_tokens` が返るか（API キー、OAuth とも）。OAuth の 1 回の実行では上限で切れなくなったが、値そのものは見ていない。
  - models.dev の取得（ネットワークを使うため、テストでは呼ばない）。
  - 続きの促しで、答えが正しくつながるか。
