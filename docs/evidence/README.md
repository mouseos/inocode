# 機能・設計判断の研究根拠の記録方法

新機能や重要な設計判断ごとに `docs/evidence/<feature-slug>.md` を作成・更新する。これは変更履歴ではなく、**現在の設計判断が何に基づき、何が未検証か**を追跡するための記録である。変更履歴はGitで確認する。

## 最小テンプレート

```markdown
# <機能名・設計判断名>

- **対象と範囲：** <依頼された動作と範囲外の事項>
- **関連コード：** <ファイルパス。未実装なら「未実装」>
- **根拠の状態：** RESEARCH_SUPPORTED | UNSUPPORTED_PENDING | UNSUPPORTED_APPROVED | EXISTING_UNSUPPORTED
- **学術的な主張・不足点：** <直接裏付けられる主張。なければ「適用可能な学術研究を確認できていない」>
- **学術資料：** <著者、論文名、年、DOIまたはURL、該当する結果、対象条件、限界。確認できなければその旨>
- **その他の根拠：** <公式規格・仕様、実機試験、テスト結果など。学術研究とは区別する>
- **ユーザーの承認：** <その機能の明示的な承認内容と参照先。未承認・対象外・不明を区別>
- **実装・検証の状況：** <実際に実行したテストと結果。未実施なら「未検証」>
```

## 記録の一覧

実装された機能・設計判断と、その根拠の記録。状態の詳細（部分ごとの区別、承認の証跡、未検証の事項）は各ファイルを参照する。

| 記録 | 対象 | 根拠の状態 | 主なコード |
|---|---|---|---|
| [tcp-frame-protocol.md](tcp-frame-protocol.md) | TCP フレームプロトコル（JSON＋生バイナリ、16 バイトヘッダー） | UNSUPPORTED_APPROVED | `src/` |
| [server-core-requests.md](server-core-requests.md) | Server と Core の分割、要求処理 | UNSUPPORTED_APPROVED | `server/src/lib.rs`、`core/src/lib.rs` |
| [llm-streaming.md](llm-streaming.md) | OpenAI 互換 API 接続と逐次配送 | UNSUPPORTED_APPROVED | `core/src/llm.rs`、`core/src/sse.rs` |
| [anthropic-messages.md](anthropic-messages.md) | Anthropic Messages 互換接続 | UNSUPPORTED_APPROVED | `core/src/llm.rs` |
| [model-discovery.md](model-discovery.md) | モデル一覧の取得 | UNSUPPORTED_APPROVED | `core/src/llm.rs` |
| [oauth-providers.md](oauth-providers.md) | Claude OAuth・GPT OAuth・Muse Code のログインと要求時認証 | UNSUPPORTED_APPROVED | `core/src/oauth.rs` |
| [deepai-code-provider.md](deepai-code-provider.md) | DeepAI Code（チェックイン、待機、Theos） | UNSUPPORTED_APPROVED | `core/src/deepai.rs` |
| [account-usage.md](account-usage.md) | アカウントの使用量取得 | UNSUPPORTED_APPROVED | `core/src/usage.rs` |
| [claude-cli-version.md](claude-cli-version.md) | Claude Code として名乗る版を npm の配布チャンネルから読むこと | UNSUPPORTED_APPROVED | `core/src/claude_cli.rs` |
| [audit-robustness-fixes.md](audit-robustness-fixes.md) | 監査 A 項目（例外処理・正しさ）の是正 | 項目ごと | 複数 |
| [streaming-performance.md](streaming-performance.md) | ストリーミング経路の性能改善（監査 B 項目） | UNSUPPORTED_APPROVED（一部は不実施） | `core/src/sse.rs`、`core/src/llm.rs` |
| [conversation-model.md](conversation-model.md) | 会話の型と 3 形式への変換、自走性の研究の整理 | UNSUPPORTED_APPROVED（判断 6・9 は研究で一部支持） | `core/src/conversation.rs` |
| [tool-calling.md](tool-calling.md) | ツール定義の送信とストリームからの組み立て | UNSUPPORTED_APPROVED | `core/src/conversation.rs`、`core/src/llm.rs` |
| [reasoning-controls.md](reasoning-controls.md) | 思考・推論の指定、モデルの深さ一覧と既定値 | UNSUPPORTED_APPROVED | `core/src/reasoning.rs` |
| [conversation-state.md](conversation-state.md) | 会話の状態をサーバーが持つ判断、Codex の保存の問題 | UNSUPPORTED_APPROVED | 保存層のみ |
| [project-memory.md](project-memory.md) | SQLite の保存層：履歴・大きな中身・教訓・手順・根拠台帳・知識・タグ・リンク・検索・連想 | 部分ごと（RESEARCH_SUPPORTED と UNSUPPORTED_APPROVED） | `core/src/store.rs` |
| [rules-and-local-issues.md](rules-and-local-issues.md) | 守ってほしいこと（規則）とローカル issue | UNSUPPORTED_APPROVED（規則の一部は研究で支持） | `core/src/rules.rs` |
| [turn-protocol.md](turn-protocol.md) | スレッドとターンの要求、サーバー側のループと続けるかの判定 | UNSUPPORTED_APPROVED（継続の規則は実験で決める） | `core/src/turn.rs`、`server/src/lib.rs` |
| [agent-tools.md](agent-tools.md) | 作業一覧・読み直し・教訓のツール、規則の検査と人への確認、繰り返しへの注意、AGENTS.md の読み込み、圧縮後の復元 | 部分ごと（A・B・C は UNSUPPORTED_APPROVED） | `core/src/turn.rs`、`core/src/agent_tools.rs` |
| [web-search.md](web-search.md) | Web などの検索ツール（ZeroSearch_MCP の組み込み）の設計 | UNSUPPORTED_APPROVED（既定は全エンジン） | 実装済み（単体テスト） |
| [web-fetch.md](web-fetch.md) | Web ページ取得ツールの設計 | 本文抽出は RESEARCH_SUPPORTED、Markdown は UNSUPPORTED_APPROVED | 実装済み（単体・TCP テスト） |
| [repo-understanding.md](repo-understanding.md) | リポジトリを広く把握する技術の研究 | RESEARCH_SUPPORTED（outline・名前検索） | outline・名前検索は実装済み |
| [read-function.md](read-function.md) | read で定義を名前で読む | RESEARCH_SUPPORTED（CODESTRUCT、Python のみ） | 実装済み。カーネルの実測（易しい課題、難しい課題と使い方の指示）では差なし |
| [prompt-caching.md](prompt-caching.md) | Anthropic のプロンプトキャッシュの区切り | UNSUPPORTED_APPROVED（公式仕様が根拠） | `core/src/conversation.rs` |
| [find-enclosing.md](find-enclosing.md) | 検索結果に、その行を含む定義の名前を付ける | UNSUPPORTED_APPROVED（独自の方法、対照実験が条件） | `core/src/find.rs`。カーネルの実測で正答は同じ、2 段の呼び出し元の入力が約 5 分の 1（以前の実行との比較） |
| [observation-masking.md](observation-masking.md) | 古いツール出力を隠す | RESEARCH_SUPPORTED（The Complexity Trap、ワークショップ） | 実測を受けて外した（コードなし）。短い課題では差なし。8 問まとめた長いターンでは、キャッシュ外入力が約 6 倍（p=0.031）、得点 0.71 対 0.86（有意でない） |
| [output-limit.md](output-limit.md) | 出力上限にモデルの上限を使う（公式の一覧、なければ models.dev）、切れたら続きを促す | UNSUPPORTED_APPROVED（公式仕様と公開実装） | `core/src/llm.rs`、`core/src/reasoning.rs`、`core/src/turn.rs` |
| [harness-tools.md](harness-tools.md) | ファイル・シェルなどの道具：他のハーネスとの比較と研究 | シェルは UNSUPPORTED_APPROVED、read・find は RESEARCH_SUPPORTED、edit・write は `edit-write.md` | シェル・read・find・edit・write は実装済み |
| [memory-integration.md](memory-integration.md) | 記憶を作業中に使わせる：ツール出力で確かめた発見の自動記録と差し込み、根拠・トークン費用・指示しないと使われない問題 | 自動記録は実測を受けて外した（コードなし）。ほかの案は研究を見て採らない | 外した。実測では効果なし、出力 +2〜3 千トークン・+12 秒／ターン。正しい答えを渡しても 2 問目は減らなかった |
| [approval-modes.md](approval-modes.md) | 承認の 4 つのモード（いちいち承認・モデルに決めさせる・半自走・完全フルアクセス）、サンドボックスの外での実行の依頼、`auto`（モデルに決めさせる）での判定役（DeepAI は公式の審査 API） | UNSUPPORTED_APPROVED | `core/src/review.rs`、`core/src/turn.rs` |
| [method-memory.md](method-memory.md) | 確かめた手法をメモにして、別のプロジェクトで使い回す（対照実験） | 使い回しは RESEARCH_SUPPORTED、識別子の必須・自動で見せる方式は UNSUPPORTED_APPROVED | `method` 道具、`recall` の全文表示を実装。実測（Haiku・Muse、各 8 回）：始めに 1 行で見せると、入力 −80〜84%、時間 −55〜66%。`recall` が抜粋しか返さないと効果なし |
| [compaction-cost-and-overflow.md](compaction-cost-and-overflow.md) | 圧縮の要求でキャッシュを効かせる、送る前の見積もり、窓に入らない履歴を区切って要約する、要約できないときの代替、圧縮モデルの指定（`INOCODE_COMPACT_MODEL`） | 区切った要約は RESEARCH_SUPPORTED（題材は本）、ほかは UNSUPPORTED_APPROVED | `core/src/compaction.rs`、`core/src/turn.rs`。実測（Haiku、4 回ずつ、5abc52f）：要約 1 回のキャッシュ外の入力 −76%、実行全体 −29%（有意ではない） |
| [large-person-messages.md](large-person-messages.md) | 人の大きな発言で圧縮が毎回起きる問題の 4 案（中略して recall、中略してファイル保存、送らない、繰り返さない）の比較 | B は UNSUPPORTED_APPROVED | B（中略してファイル保存）を実装。`core/src/compaction.rs`、`core/src/turn.rs`。実測（Haiku、各 4 回）：ファイル保存の案が正答 6.75/7、費用 −57%。recall で読ませる案と送らない案は 4.5/7 |
| [server-auth.md](server-auth.md) | 操作用のポートの合言葉による認証と、サンドボックスの命令から `~/.inocode` を隠すこと | UNSUPPORTED_APPROVED | `server/src/lib.rs`、`server/src/main.rs`、`core/src/sandbox/` |
| [remote-access.md](remote-access.md) | リモート操作用の WebSocket の受け口（本人がオンにしたときだけ 0.0.0.0、任意のパスワードを PBKDF2 で保存、`Origin` 付きは 403） | UNSUPPORTED_APPROVED | `server/src/lib.rs`、`server/src/main.rs`、`core/src/settings.rs`、`core/src/remote_password.rs` |
| [sandbox.md](sandbox.md) | シェルの命令の書き込みを、プロジェクトと一時フォルダーに限る（DSH からの移植。Linux・macOS・Windows） | UNSUPPORTED_APPROVED | `core/src/sandbox/` |
| [colleague-review.md](colleague-review.md) | 別のモデルの同僚が作業に口を出す仕組み（行き詰まりを規則で見つけたときの助言。同僚の選び方などの調査） | 行き詰まったときの助言：RESEARCH_SUPPORTED（査読前のみ）。選び方などは研究なし | 行き詰まったときの同僚は実装済み（`core/src/colleague.rs`）。選び方・issue の記録は未実装 |
| [explore-subagent.md](explore-subagent.md) | 読むだけの探索役 `explore`（場所と行だけを返す） | RESEARCH_SUPPORTED（査読前のみ） | 実装済み（`core/src/explore.rs`） |
| [model-catalog.md](model-catalog.md) | モデルの一覧の価格・ベンチマーク・実測の速さ、選び方の研究 | 一覧：UNSUPPORTED_APPROVED。自動の割り振り：未実装 | 一覧は実装済み（`core/src/catalog.rs`、`models/catalog`） |
| [usage-log.md](usage-log.md) | 使用量のログとモデルごとの速さ（tok/s、最初の文字までの時間、キャッシュ、時間帯） | UNSUPPORTED_APPROVED | 実装済み（`core/src/usage_log.rs`、`stats/models`） |
| [goal-command.md](goal-command.md) | goal コマンド（目標の達成まで自動で続ける） | UNSUPPORTED_APPROVED | 実装済み（`core/src/goal.rs`、`goal/set`・`goal/read`・`goal/stop`） |
| [plugin-mechanism.md](plugin-mechanism.md) | Claude・ChatGPT のサインインで使うコネクタとプラグイン（仕組み、実装） | UNSUPPORTED_APPROVED | コネクタは実装済み（`core/src/connectors.rs`、`core/src/mcp/mod.rs`）。プラグインのストアと読み込みは作業中 |
| [image-input.md](image-input.md) | 画像をモデルに見せる（read で画像、縮小、切り抜き） | 機能は UNSUPPORTED_APPROVED、既定値は公式文書と研究による | 画像は実装済み（`core/src/picture.rs`）、カメラも実装済み（`core/src/camera.rs`、Windows の実物と Linux の仮想カメラで確認） |
| [lsp-diagnostics.md](lsp-diagnostics.md) | 編集の後の型検査・LSP の診断 | UNSUPPORTED_PENDING | 未実装（適用できる研究なし） |
| [ask-person.md](ask-person.md) | 人に質問するツール `ask`（フォームの形、ターンの終わりに返す） | UNSUPPORTED_APPROVED | 実装済み（`core/src/ask.rs`） |
| [tool-guidance.md](tool-guidance.md) | システムプロンプトのツールの使い分け | 説明を詳しくする：RESEARCH_SUPPORTED。システムプロンプトに置く：UNSUPPORTED_APPROVED | 実装済み（`core/src/turn.rs` の `TOOL_GUIDE`） |
| [reply-to-message.md](reply-to-message.md) | 特定のメッセージへの返信（`reply_to` で番号を指定し、原文を引用して渡す） | UNSUPPORTED_APPROVED | 実装済み（`core/src/turn.rs` の `quote`） |
| [reply-language.md](reply-language.md) | 返答の言語（OS の表示言語か設定ファイルの既定、会話の中の指定で変わる、指示の位置） | UNSUPPORTED_APPROVED | 実装済み（`core/src/language.rs`） |
| [mcp-client.md](mcp-client.md) | MCP の受け口（stdio と HTTP のサーバーの道具を `mcp__<サーバー>__<道具>` としてモデルに渡す） | UNSUPPORTED_APPROVED（仕様は MCP 2026-07-28 と、それより前の版） | `core/src/mcp/`。単体テストと、偽のサーバーを起動する結合テスト |
| [edit-write.md](edit-write.md) | edit・write の道具、空白を無視した照合、編集後の構文検査、効率化の候補 | 置換形式・完全一致・空白を無視した照合・構文検査は RESEARCH_SUPPORTED（査読前を含む）、細部は UNSUPPORTED_APPROVED | `core/src/edit.rs`、構文検査は `core/src/outline.rs` |
| [environment-context.md](environment-context.md) | 日時・OS・シェル・作業フォルダをモデルに伝える | UNSUPPORTED_APPROVED | `core/src/turn.rs`、`core/src/shell.rs` |
| [file-type-tool.md](file-type-tool.md) | ファイルのパスと種類（MIME）を返す道具 | UNSUPPORTED_PENDING | 未実装 |
| [pdf-reading.md](pdf-reading.md) | PDF を読む | UNSUPPORTED_APPROVED | `core/src/pdf.rs`、`core/src/read.rs` |
| [git-context.md](git-context.md) | git の情報をモデルに伝える | UNSUPPORTED_APPROVED | `core/src/git.rs`、`core/src/turn.rs` |
| [instruction-adherence.md](instruction-adherence.md) | 人が決めた約束をエージェントが破る理由と対策案 | UNSUPPORTED_PENDING | 案 1 は promise-list.md で実装。ほかは未実装 |
| [promise-list.md](promise-list.md) | 人が決めた約束の一覧を毎回付ける | RESEARCH_SUPPORTED（手当ての部分は UNSUPPORTED_APPROVED） | `core/src/promise.rs`、`core/src/turn.rs` |
| [audio-recording.md](audio-recording.md) | 機器と形式を指定して音を録る | UNSUPPORTED_APPROVED | `core/src/audio.rs` |
| [transcription.md](transcription.md) | 音のファイルを文字にする（Claude・ChatGPT・Google） | UNSUPPORTED_APPROVED | `core/src/transcribe.rs` |
| [translation.md](translation.md) | 翻訳の共通部品（Google・Edge・Firefox・Chromium）と思考の翻訳 | UNSUPPORTED_APPROVED | `core/src/translate.rs`、`core/src/translate/` |
| [slash-commands.md](slash-commands.md) | スラッシュコマンドの仕組みと組み込みコマンドの一覧（定義の案、確認待ち） | UNSUPPORTED_APPROVED | 未実装 |
| [account-pool.md](account-pool.md) | プロバイダ・モデル・アカウントの登録とアカウントのプール、認証情報の保存、環境変数をやめる | UNSUPPORTED_APPROVED | 設定ファイル・プロバイダの登録・会話ごとの選択は実装済み。プールは未実装 |
| [effort-estimates.md](effort-estimates.md) | 作業の時間とトークンの見積もり（過去の実測から） | RESEARCH_SUPPORTED（作業中の出し直し、査読前）／UNSUPPORTED_APPROVED | 実装済み（`core/src/estimate.rs`、`estimate`、`/estimate`） |
| [company.md](company.md) | 会社の仕組み（役割付きの社員、自動の指名、並行、同僚のレビューと助言、予算） | RESEARCH_SUPPORTED（別会社のレビュー・厳しいレビュー・行き詰まりの助言）／UNSUPPORTED_APPROVED | 実装済み（`core/src/company.rs`、`delegate`・`review`、`members/*`・`allow/*`、`/members`・`/allow`） |
| [thread-title.md](thread-title.md) | スレッドの題名：最初のメッセージからの自動の題名、モデルからの提案と頼まれたときの変更、題名の通知 | UNSUPPORTED_APPROVED | `core/src/title.rs`、`core/src/turn.rs`（`title` 道具、状態の題名）、`title_updated`・`title_proposed` |
| [thread-subscribe.md](thread-subscribe.md) | スレッドの動きの購読（別の接続からターンを見る）と、スレッドの状態（`idle`・`running`・`waiting`）の通知 | UNSUPPORTED_APPROVED | `core/src/lib.rs`（`Activity`・`ThreadStatus`）、`server/src/lib.rs`（`thread/subscribe`・`thread/status/subscribe`） |
| [thread-fork.md](thread-fork.md) | スレッドを、人のメッセージの手前まで、またはそのメッセージの返事までで写す（`thread/fork`） | UNSUPPORTED_APPROVED | `core/src/lib.rs`（`ForkCut`・`fork_thread`）、`core/src/store_threads.rs`（`fork_session_through`） |
| [thread-rewind.md](thread-rewind.md) | 巻き戻し：人のメッセージから後ろを消し、その文を返す（`thread/rewind`） | UNSUPPORTED_APPROVED | `core/src/lib.rs`（`rewind_thread`）、`core/src/store_threads.rs`（`rewind_session`） |
| [file-snapshots.md](file-snapshots.md) | ターンの前にプロジェクトのフォルダの写しをとり、巻き戻しでファイルも戻す（`thread/rewind` の `restore`） | UNSUPPORTED_APPROVED | `core/src/snapshot.rs`、`core/src/lib.rs`（`take_snapshot`・`restore_files`）、`core/src/store.rs`（`snapshots`） |
| [message-queue.md](message-queue.md) | ターンの間に並べた文を、ターンが終わるたびに次のターンとして送る列（`thread/queue/*`、購読の `queue`） | UNSUPPORTED_APPROVED | `core/src/queue.rs`、`core/src/store_threads.rs`（`queue` 表）、`server/src/lib.rs`（`queue_request`） |

## 状態の判定

- `RESEARCH_SUPPORTED`：**直接適用できる**学術的根拠を確認した。類似テーマの引用だけでは不可。
- `UNSUPPORTED_PENDING`：学術的根拠を確認できず、ユーザーの個別承認待ち。**実装禁止**。
- `UNSUPPORTED_APPROVED`：学術的根拠は確認できないが、該当する機能についてユーザーが明示的に実装を承認した。承認の証跡を記録する。
- `EXISTING_UNSUPPORTED`：すでにある実装について適用可能な根拠を確認できない。過去に承認されたと推測しない。関連箇所を扱う際に記録し、勝手に削除・変更しない。

## 注意

論文の結果、実装上の確認、ローカルテスト、個人の判断を区別する。**研究根拠を確認できないことは「その機能に効果がない」という証明ではない。** 設計判断の追跡可能性の参考研究：Jansen, Bosch & Avgeriou (2008), [Documenting after the fact: Recovering architectural design decisions](https://doi.org/10.1016/j.jss.2007.08.025)。この研究は判断の記録を扱うもので、個々の機能の有効性を保証するものではない。

ソースコードには変更履歴を書かず、必要な不変条件や理由だけをコメントする。
