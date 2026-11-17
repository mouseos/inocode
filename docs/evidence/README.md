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
| [audit-robustness-fixes.md](audit-robustness-fixes.md) | 監査 A 項目（例外処理・正しさ）の是正 | 項目ごと | 複数 |
| [streaming-performance.md](streaming-performance.md) | ストリーミング経路の性能改善（監査 B 項目） | UNSUPPORTED_APPROVED（一部は不実施） | `core/src/sse.rs`、`core/src/llm.rs` |
| [conversation-model.md](conversation-model.md) | 会話の型と 3 形式への変換、自走性の研究の整理 | UNSUPPORTED_APPROVED（判断 6・9 は研究で一部支持） | `core/src/conversation.rs` |
| [tool-calling.md](tool-calling.md) | ツール定義の送信とストリームからの組み立て | UNSUPPORTED_APPROVED | `core/src/conversation.rs`、`core/src/llm.rs` |
| [reasoning-controls.md](reasoning-controls.md) | 思考・推論の指定、モデルの深さ一覧と既定値 | UNSUPPORTED_APPROVED | `core/src/reasoning.rs` |
| [conversation-state.md](conversation-state.md) | 会話の状態をサーバーが持つ判断、Codex の保存の問題 | UNSUPPORTED_APPROVED | 保存層のみ |
| [project-memory.md](project-memory.md) | SQLite の保存層：履歴・大きな中身・教訓・手順・根拠台帳・知識・タグ・リンク・検索・連想 | 部分ごと（RESEARCH_SUPPORTED と UNSUPPORTED_APPROVED） | `core/src/store.rs` |
| [rules-and-local-issues.md](rules-and-local-issues.md) | 守ってほしいこと（規則）とローカル issue | UNSUPPORTED_APPROVED（規則の一部は研究で支持） | `core/src/rules.rs` |
| [turn-protocol.md](turn-protocol.md) | スレッドとターンの要求、サーバー側のループと続けるかの判定 | UNSUPPORTED_APPROVED（継続の規則は実験で決める） | `core/src/turn.rs`、`server/src/lib.rs` |
| [agent-tools.md](agent-tools.md) | 作業一覧・読み直し・教訓のツール、規則の検査と保留・事前承認、繰り返しへの注意、AGENTS.md の読み込み、圧縮後の復元 | 部分ごと（A・B・C は UNSUPPORTED_APPROVED） | `core/src/turn.rs`、`core/src/agent_tools.rs` |
| [web-search.md](web-search.md) | Web などの検索ツール（ZeroSearch_MCP の組み込み）の設計 | UNSUPPORTED_APPROVED（既定は全エンジン） | 実装済み（単体テスト） |
| [harness-tools.md](harness-tools.md) | ファイル・シェルなどの道具：他のハーネスとの比較と研究 | シェルは UNSUPPORTED_APPROVED、他は UNSUPPORTED_PENDING | シェルは実装済み（TCP テスト） |

## 状態の判定

- `RESEARCH_SUPPORTED`：**直接適用できる**学術的根拠を確認した。類似テーマの引用だけでは不可。
- `UNSUPPORTED_PENDING`：学術的根拠を確認できず、ユーザーの個別承認待ち。**実装禁止**。
- `UNSUPPORTED_APPROVED`：学術的根拠は確認できないが、該当する機能についてユーザーが明示的に実装を承認した。承認の証跡を記録する。
- `EXISTING_UNSUPPORTED`：すでにある実装について適用可能な根拠を確認できない。過去に承認されたと推測しない。関連箇所を扱う際に記録し、勝手に削除・変更しない。

## 注意

論文の結果、実装上の確認、ローカルテスト、個人の判断を区別する。**研究根拠を確認できないことは「その機能に効果がない」という証明ではない。** 設計判断の追跡可能性の参考研究：Jansen, Bosch & Avgeriou (2008), [Documenting after the fact: Recovering architectural design decisions](https://doi.org/10.1016/j.jss.2007.08.025)。この研究は判断の記録を扱うもので、個々の機能の有効性を保証するものではない。

ソースコードには変更履歴を書かず、必要な不変条件や理由だけをコメントする。
