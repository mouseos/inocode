# OpenAI 互換 API 接続とストリーミング

## 対象と範囲

- 対象: 第2段階の Server / Core に、OpenAI 互換 Chat Completions API への接続と、SSE による逐次受信、既存 TCP プロトコルでの逐次配送、成功・失敗・中断の通知を追加する。
- 実装した接続方式: OpenAI 互換 API のみ。接続方式は `LlmConfig`（Base URL・モデル・API キー）で選ぶ構成にしてあり、Anthropic Messages 互換、Codex OAuth、Claude OAuth は実装していない。
- 範囲外: 会話履歴、セッション管理、ツール実行、RAG、MCP、GUI、製品用 CLI、承認フロー、モデル管理。実装していない。
- 通信仕様（16 バイトヘッダー、Type 1/2/3、Stream ID、ビッグエンディアン、JSON と生バイナリの混在）は第1段階のまま変更していない。

## 関連ソースコード

- `core/src/llm.rs` — `LlmConfig` / `Llm` / `ChatStream`。OpenAI 互換 `/chat/completions` への `stream: true` 要求と、デルタ単位の受信。
- `core/src/sse.rs` — `SseParser`。バイト境界をまたぐ SSE の逐次解析、`[DONE]` と `finish_reason` の解釈、途中終了の検出。
- `core/src/lib.rs` — `Request` / `Response` / `EventPayload` / `Core`。`chat` と `cancel` の受け口。
- `server/src/lib.rs` — 接続ごとの読み取り・書き込みタスク、要求 ID ごとの生成タスク、中断レジストリ、有界の送信キュー。
- `server/tests/chat_tcp.rs` — ローカルの模擬 HTTP API を用いた実 TCP 統合テスト 13 件。
- `server/tests/live_api.rs` — 実 Logfare API を叩くテスト 1 件（通常は `ignored`、CI の `live-api` ジョブが実行）。
- `server/tests/support/mod.rs` — 最小の HTTP/1.1 模擬サーバー（応答本文を任意のチャンクに分割して送る）と TCP クライアント補助。

## 要求・応答

- 既存の `ping` / `echo` は変更なしで動作する。
- `chat`: `{"id": <u64>, "method": "chat", "params": {"prompt": <string>}}`。応答は単一フレームではなく、同じ Stream ID 上のイベント列。
  - `{"id": <要求 ID>, "event": "started"}`
  - `{"id": <要求 ID>, "event": "delta", "text": <string>}`
  - ターン（`turn/start`）では、思考が `{"id": <要求 ID>, "event": "thinking", "text": <string>}` で流れる（`reasoning-controls.md`）。
  - `{"id": <要求 ID>, "event": "completed"}`
  - `{"id": <要求 ID>, "event": "failed", "error": {"code": <string>, "message": <string>}}`
  - `{"id": <要求 ID>, "event": "cancelled"}`
- `cancel`: `{"id": <u64>, "method": "cancel", "params": {"request_id": <u64>}}` → `{"id": <要求 ID>, "result": {"cancelled": <bool>}}`。対象が実行中なら `true`、既に終端状態に入ったか未知なら `false`。
- `cancel` が `true` を返した場合、その対象要求の終端イベントは必ず `cancelled` になる（`completed` / `failed` に化けない）。生成の全待機段階（HTTP 要求の開始、応答ヘッダー待ち、SSE の受信、送信キューの待ち）で中断が有効で、中断すると上流 HTTP 接続を閉じて破棄する。
- 追加したエラーコード: `provider_unconfigured`、`provider_config`、`provider_http`、`provider_protocol`、`provider_stream`、`duplicate_id`。
- 終端イベント（`completed` / `failed` / `cancelled`）は 1 要求につき 1 回だけ送る。`started` はプロバイダーの応答ヘッダーが成功した後に送るため、HTTP エラー時は `started` を送らず `failed` のみになる。
- プロバイダーが `[DONE]` も `finish_reason` も送らずに本文を終えた場合は `completed` ではなく `provider_stream` の `failed` を返す。途中で切れた応答を完了として扱わないため。

## 設計上の判断（監査是正で確定した不変条件）

- 中断は生成タスクが担う。接続のレジストリ（実行中要求 ID → 中断シグナル）は接続ごとに置き、`cancel` メソッドはシグナルを送るだけで ID を解放しない。生成タスクだけが自分のエントリを削除し、終端イベントをキューに入れた後に ID を解放する。これにより「中断応答が `true` なら終端は必ず `cancelled`」「ID は前の要求が終わるまで再利用できない」「旧タスクが新要求の管理情報を壊さない」が保たれる。
- 終端イベントは 1 要求につき 1 回。レジストリのエントリを「確定済み」に切り替えてからシグナルを読むため、`true` を返した中断は必ず `cancelled` 終端に反映される。
- 接続の読み取り側が終わったら、その接続の全生成に中断を通知し、書き込みタスクは次の await で停止する。未送信フレームは接続とともに破棄する（切断後の配送先がないため）。
- 接続ごとの同時生成数に上限はなく、未指示のため上限値と拒否動作は導入していない（承認待ち）。

## 設定

- `LOGFARE_API_KEY`（必須）: Bearer トークン。未設定または空のとき、Server は起動するが `chat` は `provider_unconfigured` を返す。
- `INOCODE_LLM_BASE_URL`（任意、既定 `https://logfare.ai/v1`）: `POST {base_url}/chat/completions` を呼ぶ。末尾スラッシュは除去する。
- `INOCODE_LLM_MODEL`（任意、既定 `logfare/auto`）。
- `INOCODE_PORT`（任意、既定 8787）。待ち受けは常にループバック `127.0.0.1`。

## 根拠の状態

- **UNSUPPORTED_APPROVED** — 学術研究による直接の裏付けは確認できていない。根拠は下記のとおり種別ごとに区別する。
  - ユーザーによる個別承認（証跡あり）: 第3段階の実装指示（OpenAI 互換接続・イベント名・`cancel` の形・逐次配送）。その後の監査是正指示（2026-11-15）で、中断を全待機段階で有効化すること、要求 ID の競合不変条件、`duplicate_id` を混線防止の必要なコードとして維持すること、送信キュー待ちの中断、終端イベントの重複禁止が明示された。
  - 公式仕様: OpenAI 互換 Chat Completions（`stream: true`、`data:` 行、`[DONE]`）と Server-Sent Events のフレーミング（HTML Living Standard）。正しさ・互換性の根拠であり、機能の有効性の研究根拠ではない。
  - 実測: 本リポジトリの模擬 HTTP API による統合テストと実 Logfare API の CI 実行。
- **手順違反の記録（維持）**: AGENTS.md 第3節に従い、実装前に研究を調べる・記録する・停止して明示承認を得ることを行わなかった。学術調査は**実装後**（2026-11-15）に行ったものであり、事前承認の代わりにならない。
- **承認待ち（未実装）**: プロバイダー無応答時のタイムアウト値（応答開始待ち・ストリーム無進捗の区別と値の設定）。現在の回避手段はクライアントによる `cancel` のみ。接続あたりの同時生成数の上限値と拒否動作も承認待ち（下記「不足」）。

## 学術的な主張・不足点

調査方法: `web_search` による検索、Crossref API（DOI・著者・掲載誌・頁）、arXiv API（識別子・版・査読状況）、Semantic Scholar API（会場・抄録）で書誌と主張を確認。**論文の結果・実測・設計判断を区別して記載する。**

### 1. 応答を一括表示せず逐次配送することの知覚的効果 — 部分的に支持

- 支持: 動的（タイピング表示）と静的（全文一括）を同一被験者内で比較した統制実験（N=103）は、動的提示が AI の能力・温かさ・信頼性・エンゲージメント・共感などの知覚を有意に高めたと報告（Bhat, 2025）。一方、知覚された有効性・バイアス・学習支援には有意差なし。
- 支持: 生成速度が知覚された人間性・信頼性・生成テキストの品質と相関する（Zhou et al., 2025, n=297）。ただし抄録では効果の方向（速いほど良いか）は示されない。
- **反証的知見（重要）**: TTFT を 2 秒 / 9 秒 / 20 秒で操作した統制実験では、2 秒条件の参加者が出力を「思慮深くない・有用でない」と評価し、9〜20 秒条件より低かった（Tan et al., CHI 2026）。「速いほど高評価」という単純な前提は支持されない。
- 不足: 上記はいずれも**画面表示**の研究であり、TCP 上のプロトコル配送そのものを評価したものではない。本実装はプロバイダーの生成速度をそのまま配送し、提示速度の調整（ペーシング）は行わない。この点は研究知見（Bhat 2025、Tan 2026、Xiao & Yang 2025）と突き合わせると未検討の設計余地である。

### 2. QoE 指標としての TTFT とトークン配送間隔 — 支持（一部は査読前）

- テキストストリーミングサービスでは初回トークンまでの時間（TTFT）とトークン間隔（TBT）が QoE 要件になり、サーバー型・オンデバイス型のいずれも要求を満たすのが難しいと実測されている（Sun et al., Findings of ACL 2025）。
- 逐次ストリーミングの QoE を「初期待機（TTFT）」と「トークン消費（配送速度）」に分けて定義し、第一トークンを早く、以降を読み手に合わせて滑らかに届けるべきだと論じる研究がある（Liu et al., 2024）。**査読状況不明のプレプリント**であり、根拠としては弱い。
- 不足: 本実装は TTFT を短縮する仕組み（プロバイダー側の待ち時間は制御外）も、配送速度の調整も持たない。計測しているのは実 API の TTFT と総時間のみ。

### 3. 有界キューと逆圧による過負荷制御 — 支持

- ステージ間を明示的な**有界イベントキュー**で結び、キュー満杯時のブロック（逆圧）またはエンキュー失敗時の負荷制御で過負荷を扱う設計が、よく条件付けられたスケーラブルなサービスを実現すると報告されている（Welsh, Culler & Brewer, SOSP 2001）。ステージ分離による性能隔離も同論文の主張。
- 本実装の対応: 接続ごとに書き込みタスクを 1 つ置き、有界キュー（64 フレーム）を介して送る。読み取りの遅いクライアントは自分のキューが満杯になって逆圧がかかり、他クライアントの進行を妨げない。フレーム混在も単一書き込みタスクで防ぐ。研究の主張（有界キュー＋逆圧で過負荷を制御できる）と整合する。
- **研究が保証しないこと**: SEDA は有界キューという設計を支持するが、キュー長 64 の最適性や妥当性を保証しない。64 は実装時に選んだ値であり、根拠は未測定である。
- 不足: 負荷制御（要求の拒否・縮退）は実装していない。キュー満杯時は生成側が待つだけであり、その挙動の性能評価も未実施。

### 4. 生成の中断（キャンセル）— 間接的に支持、直接の研究は未確認

- ストリーミング出力に対するユーザー割り込み（barge-in）は例外的操作ではなく、音声対話では全ユーザー応答の 10〜25 % を占めると報告されている（Witt, 2014）。割り込みへの対応を速くする手法も研究されている（Selfridge et al., SIGDIAL 2013）。
- 逐次配送が読み手の速度を超えると計算資源が無駄になるという分析（Xiao & Yang, UIST 2025）は、生成を止めることの資源的意義を間接的に支える。
- **不足**: LLM の生成キャンセルが課金・GPU 使用量・他要求の遅延をどれだけ減らすかを検証した査読研究は確認できていない。実務文書（NVIDIA Triton の request cancellation など）は実装上の根拠であって学術研究ではない。本実装が確認したのは「中断で上流 HTTP 接続が閉じる」ことまでである。

### 5. 直接の研究が見つからなかった設計判断

- 16 バイト固定ヘッダー、ペイロード上限 16 MiB、チャンク 64 KiB、接続あたりの同時生成数（上限なし）、プロバイダー無応答時のタイムアウト値。いずれも**適用可能な学術研究は未確認**であり、判断根拠は凍結された仕様（ユーザー指示）と実測のみ。
- SSE の 1 イベント上限 16 MiB と `write_json` の送信上限: 無制限のメモリ確保を防ぐために設けた境界であり、輸送層の 1 フレーム上限（16 MiB）と揃えた。境界値自体の妥当性を評価した研究は未確認。
- メッセージのフレーミング方式（長さ前置 vs 区切り文字）について、学術研究は確認できず、実務解説と公式仕様（HTTP/1.1 のメッセージフレーミングなど）のみ。

## 学術資料

1. Bhat, M. (2025). *How Dynamic vs. Static Presentation Shapes User Perception and Emotional Connection to Text-Based AI.* Proceedings of the 30th International Conference on Intelligent User Interfaces (IUI '25), 846–860. https://doi.org/10.1145/3708359.3712131
   - 結果: N=103 の被験者内統制実験。動的（タイピング表示）が静的（一括表示）より能力・温かさ・信頼性・エンゲージメント・共感などの知覚で有意に高評価。有効性・バイアス・学習支援は有意差なし。
   - 適用条件: テキストチャットの表示方式。タイピング**シミュレーション**であり、API からの実トークンストリーミングではない。
   - 限界: 表示レベル（UI）の研究で、プロトコル配送の評価ではない。
2. Zhou, D., Gallagher, J. R., & Sterman, S. (2025). *Thoughtful, Confused, or Untrustworthy: How Text Presentation Influences Perceptions of AI Writing Tools.* Proceedings of the 2025 Conference on Creativity and Cognition (C&C '25), 573–589. https://doi.org/10.1145/3698061.3726907
   - 結果: n=297。生成速度が知覚された人間性・信頼性・生成テキストの品質と相関。
   - 適用条件: ライティング支援ツール。速度条件の詳細な方向性は抄録に現れない。
   - 限界: 抄録のみ確認。全文の条件設計（速度の水準）は未読。
3. Tan, F. F.-Y., Messerschmidt, M. A., Yin, W., & Nov, O. (2026). *The Impact of Response Latency and Task Type on Human-LLM Interaction and Perception.* Proceedings of the 2026 CHI Conference on Human Factors in Computing Systems, 1–17. https://doi.org/10.1145/3772318.3790716
   - 結果: TTFT（2 / 9 / 20 秒）を操作。2 秒条件は 9・20 秒条件より出力を「思慮深くない・有用でない」と評価。操作ログ上の行動は遅延に頑健だった。
   - 適用条件: TTFT の操作であり、逐次配送速度の操作ではない。
   - 限界: 「速ければ良い」という前提への反証であり、本実装の即時配送設計には未解決の論点を残す。
4. Xiao, C., & Yang, Z. (2025). *Streaming, Fast and Slow: Cognitive Load-Aware Streaming for Efficient LLM Serving.* Proceedings of the 38th Annual ACM Symposium on User Interface Software and Technology (UIST '25), 1–13. https://doi.org/10.1145/3746059.3747721
   - 結果: 読み手の速度を超えるストリーミングは計算資源の無駄になるため、認知負荷に応じて配送を遅らせる適応的手法を提案し、資源節約効果を報告。
   - 適用条件: サービング側の速度制御。クラウドソーシング実験＋シミュレーション。
   - 限界: 効果量は抄録抜粋の範囲では不明。本実装は速度制御を持たない。
5. Sun, T., Wang, P., & Lai, F. (2025). *DiSCo: Device-Server Collaborative LLM-based Text Streaming Services.* Findings of the Association for Computational Linguistics: ACL 2025, 14259–14277. https://doi.org/10.18653/v1/2025.findings-acl.734
   - 結果: テキストストリーミングサービスの QoE 要件を TTFT と TBT として定式化し、サーバー型・オンデバイス型いずれも要求を満たすのが難しいと実測。
   - 適用条件: テキストストリーミングのサービング。
   - 限界: 我々の TCP 配送層は対象外。
6. Liu, J., Chung, J.-W., Wu, Z., Lai, F., et al. (2024). *Andes: Defining and Enhancing Quality-of-Experience in LLM-Based Text Streaming Services.* arXiv:2404.16283. https://arxiv.org/abs/2404.16283
   - 結果: QoE を初期待機（TTFT）とトークン消費（配送速度）に分解して定義。
   - **査読状況: 未確認（プレプリント）**。DOI なし。根拠としては弱い。
7. Welsh, M., Culler, D., & Brewer, E. (2001). *SEDA: An Architecture for Well-Conditioned, Scalable Internet Services.* Proceedings of the 18th ACM Symposium on Operating Systems Principles (SOSP '01), 230–243. https://doi.org/10.1145/502034.502057
   - 結果: 明示的な有界イベントキューと逆圧（満杯時ブロック）／負荷制御で過負荷を扱うアーキテクチャ。ステージ分離による性能隔離。
   - 適用条件: サーバーアーキテクチャ全般。
   - 限界: 2001 年の設計研究であり、LLM 固有の評価ではない。
8. Selfridge, E., Arizmendi, I., Heeman, P., & Williams, J. (2013). *Continuously Predicting and Processing Barge-in During a Live Spoken Dialogue Task.* Proceedings of SIGDIAL 2013, 384–393. https://aclanthology.org/W13-4063/
   - 結果: 割り込み（barge-in）を予測・処理する手法で、割り込みへの応答時間を平均 2.61 秒から 1.58 秒へ短縮。
   - 適用条件: 音声対話。テキスト LLM ではない。
   - 限界: 音声出力の割り込みであり、生成中断のコスト削減は扱わない。
9. Witt, S. (2014). *Modeling user response timings in spoken dialog systems.* International Journal of Speech Technology, 18(2), 231–243. https://doi.org/10.1007/s10772-014-9265-1
   - 結果: システム発話中のユーザー応答（barge-in）が全応答の 10〜25 % を占める。
   - 適用条件: 音声対話システム。
   - 限界: 抄録抜粋のみ確認（全文は購読が必要）。テキスト生成の中断頻度を示すものではない。

## その他の根拠（研究とは区別する）

- 公式仕様: OpenAI 互換 Chat Completions の `stream: true` は `text/event-stream` で `data:` 行を逐次返し、終端は `data: [DONE]`。本実装はこの形式のみを解釈する。
- 公式仕様: Server-Sent Events は HTML Living Standard（§9.2 Server-sent events）で定義され、`data:` 行と空行によるイベント区切りが定められている。本実装は改行・CRLF・最終行の欠落改行を扱うが、複数 `data:` 行の連結は未対応。
- 実測（本リポジトリ）: 模擬 HTTP API を用いた統合テスト 13 件、実バイナリ起動スモーク（逐次デルタ、中断、中断後の別要求）、実 Logfare API の CI 実行（下記「実装・検証の状況」）。これらは仕様適合と動作の確認であり、機能の学術的有効性の証明ではない。
- 実務文書: NVIDIA Triton Inference Server の request cancellation、NVIDIA AIPerf の request cancellation testing は、キャンセルが上流の生成を止めるべきという実装上の根拠。学術研究ではない。

## ユーザーの承認

- 参照元: 本タスクの実装指示（第3段階）。OpenAI 互換 API のみを実装し、Anthropic / OAuth は後段、会話履歴・ツール・RAG・MCP・GUI は範囲外と明記されている。
- 承認の性質: **範囲の指示**であり、AGENTS.md 第3節が求める「研究根拠が無い状態で実装してよいという明示的な承認」としては記録が不十分である。研究根拠を欠いたまま実装した事実は上記「根拠の状態」に残す。
- 未承認の項目: 提示速度の調整（ペーシング）、負荷制御、プロバイダー無応答時のタイムアウト値の決定。

## 実装・検証の状況

- 実装: 完了（OpenAI 互換 API 接続、SSE 逐次受信、TCP 逐次配送、成功・失敗・中断の通知）。その後の監査是正で、中断が全待機段階で有効になり、要求 ID の競合、接続切断後のタスク残留、Server 全体の異常終了、SSE 解析、`write_json` のメモリ上限、API キーの Debug 出力が修正された。
- ローカル（Windows 実機）: `cargo test --workspace --release` で 82 件成功・4 件 `ignored`（プロトコル単体 17、プロトコル TCP 統合 14、Core 単体 20、Server チャット TCP 統合 20、Server TCP 統合 11）。`cargo fmt --all -- --check` と `cargo clippy --workspace --all-targets --release -- -D warnings` は警告なし。3 回連続実行して同一結果。
- 模擬 HTTP API による検証項目: 要求内容（`POST /v1/chat/completions`、Bearer、`stream: true`、messages）、SSE の逐次配送（最初のデルタが完了より 300 ms 以上早い）、イベント順序、5 バイト分割（UTF-8 途中を含む）の復元、応答ヘッダー待ちでの中断（上流接続の close を模擬サーバー側の書き込み失敗で確認）、プロバイダー無応答での中断、中断後の終端が `cancelled` で他の終端が出ないこと、中断直後の同 ID 再利用（拒否またはクリーンな新生成のどちらか）、終端後の ID 再利用で旧タスクが干渉しないこと、送信キュー満杯状態での中断（300 デルタ未満で `cancelled` 終端）、切断を 4 回繰り返しても生成タスクが残留しないこと、30 同時生成の混線なし、3 クライアント×2 要求の混在なし、HTTP 429 の `failed`、途中終了の `provider_stream`、生成中の `ping` 同時応答、遅延読み取りでも 300 デルタ欠落なし、`ping`/`echo` 回帰。
- Core / プロトコルの単体検証項目: SSE の複数 `data:` 行の連結、CR 単独・CRLF・BOM、終端子のない最終行、`[DONE]` と `finish_reason`、上限（16 MiB）を超える無終端イベントの拒否、`write_json` の上限一致・超過拒否、`read_json` / `read_binary_to` のフレーム種別不一致、`LlmConfig` の Debug 出力に API キーが現れないこと。
- GitHub Actions（workflow_dispatch、3 OS）: ジョブは `LOGFARE_API_KEY` 未設定時に失敗するよう変更済みで、実 API テストが未実施のまま成功扱いになることはない。
  - 是正前コード: run https://github.com/mouseos/inocode/actions/runs/43714283121 で 6 ジョブ成功。
  - 是正後コード: run https://github.com/mouseos/inocode/actions/runs/43722585178 で 6 ジョブ成功。`protocol test`（ubuntu / windows / macos）は各 OS で fmt・clippy・build・テスト 82 件成功。`live provider test`（3 OS）は実 Logfare API（`logfare/auto`）での逐次生成を確認（デルタ数・文字数・時刻のみ出力）。

| 是正後の live provider test | デルタ数 | 文字数 | 最初のデルタ | 完了まで |
| --- | --- | --- | --- | --- |
| ubuntu-latest | 4 | 22 | 3766 ms | 3766 ms |
| macos-latest | 5 | 33 | 4998 ms | 5096 ms |
| windows-latest | 4 | 23 | 7632 ms | 7636 ms |

| 是正前の live provider test（参考として保持） | デルタ数 | 文字数 | 最初のデルタ | 完了まで |
| --- | --- | --- | --- | --- |
| ubuntu-latest | 16 | 145 | 5868 ms | 6127 ms |
| windows-latest | 8 | 86 | 9721 ms | 9856 ms |
| macos-latest | 10 | 191 | 3243 ms | 3422 ms |

- 未検証: プロバイダーのレート制限下での挙動、数万デルタの長い応答、ネットワーク断、TLS 検証エラー、同時接続数の上限、長時間稼働、中断時のプロバイダー側課金の停止時期（プロバイダー依存）、無応答プロバイダーをクライアントが中断しなかった場合のリソース保持（タイムアウトは承認待ち）。
- 未検証（研究面）: 提示速度の調整効果、中断による資源節約量、有界キュー容量 64 の妥当性、SSE イベント上限 16 MiB の妥当性。
