# 独立ServerとCore要求処理

## 対象と範囲

- 対象: 第1段階の TCP フレームプロトコル上に、独立実行できる Server と、接続状態を持たない Core を追加する。JSON 制御フレームの Payload に要求・応答構造を定義する。
- 範囲外: LLM 接続、モデル管理、エージェント、ツール実行、会話履歴、SQLite、セッション管理、RAG、MCP、GUI、製品用 CLI、HTTP/WebSocket。実装していない。
- 通信仕様（16 バイトヘッダー、Type 1/2/3、Stream ID、ビッグエンディアン）は第1段階のまま変更していない。

## 関連ソースコード

- `core/src/lib.rs` — `Request` / `Response` / `ResponseBody` / `ErrorInfo` / `Core::handle`。TCP 依存なし。
- `server/src/lib.rs` — `run(listener, shutdown)`: 接続受付、要求のデコード、Core への委譲、応答送信。
- `server/src/main.rs` — 独立プロセスの起動（ループバック固定、ポートは `INOCODE_PORT`、既定 8787）。
- `server/tests/server_tcp.rs` — 実 TCP 接続による統合テスト 11 件。
- `.github/workflows/protocol-test.yml` — fmt / clippy / build / test を 3 OS で手動実行。

## 要求・応答（JSON Payload）

- 要求: `{"id": <u64>, "method": <string>, "params": <任意>}`。`id` と `method` は必須、`params` は任意。
- 成功応答: `{"id": <要求と同じ>, "result": <任意>}`。
- エラー応答: `{"id": <要求と同じ or null>, "error": {"code": <string>, "message": <string>}}`。
- 実装したエラーコード: `invalid_json`、`invalid_request`、`invalid_params`、`unknown_method`。JSON として解釈できない、または要求構造を満たさない場合は `id` を `null` にして返す（対応する要求 ID が存在しないため）。
- 実装した method: `ping`（`{"pong": true}`）、`echo`（`params` をそのまま返す）。通信と要求処理の検証用であり、LLM 機能ではない。
- Binary / End フレームは要求プロトコルの対象外。Payload を読み捨てたうえで `invalid_request` を返し、同一接続を継続する。
- 要求と応答はフレームの Stream ID をそのまま用いるため、同一接続の複数要求と複数クライアントの応答は接続・Stream ID の組で区別される。

## 根拠の状態

- **UNSUPPORTED_APPROVED** — Server / Core の分割、要求・応答の形、method（`ping` / `echo`）はユーザー指示で確定しており、有効性を学術研究で主張しない。参照元は本タスクの実装指示（第2段階 §3）。
- 設計判断: Core は接続状態を持たず、Server が 1 インスタンスを `Arc` で共有する。接続ごとに `tokio::spawn` するのは、1 クライアントの切断や不正フレームで他のクライアントを止めないため。追加の依存クレートは導入していない（tokio / serde / serde_json のみ）。

## 学術的な主張・不足点

調査方法: `web_search` による検索（2026-11-15）。**適用可能な学術研究は確認できなかった。**

- 要求・応答の形（`id` / `method` / `params`、`result` / `error`）と、Stream ID を要求・応答の対応付けに使う方式は、ユーザー指示で確定した仕様である。方式の妥当性を評価した研究は確認していない。
- Core を接続状態から切り離し、Server が 1 インスタンスを共有する設計は、状態を持たない処理の共有という一般的な構成であり、研究根拠ではなく設計判断である。
- 不足: 要求・応答方式の比較評価、共有 Core のスケーラビリティ評価は未実施。負荷制御やタイムアウトの設計も未検討。

## 学術資料

- 該当なし（適用可能な査読研究・一次研究を確認できなかった）。研究で裏付けられたとは主張しない。

## その他の根拠（研究とは区別する）

- ユーザー指示による仕様: 要求・応答の形、エラーコード、`ping` / `echo` の意味（第2段階の指示）。
- 実測（本リポジトリ）: 下記のとおり実 TCP 接続による統合テストで確認。仕様適合と動作の確認であり、学術的有効性の証明ではない。
- 実装上の判断: 1 クライアントの切断や不正フレームで他クライアントを止めないため、接続ごとにタスクを分ける。ソケットオプション（`TCP_NODELAY`）の設定失敗はその接続だけを破棄し、Server 全体は受け付けを続ける。有界キューによる逆圧の考え方は `llm-streaming.md`（SEDA, Welsh et al. 2001）に記録する。

## ユーザーの承認

- 参照元: 本タスクの実装指示（第2段階 §3）。
- 承認の性質: 仕様の確定という形での承認であり、研究根拠を伴わない。第3節が求める明示承認として記録できるかは**不明**。
- 第3段階で `chat` / `cancel` を追加した（詳細は `llm-streaming.md`）。本ファイルの記述は第2段階時点の要求処理を対象とする。

## 検証済みの主張

- `cargo test --workspace --release` が成功: プロトコル単体 14・プロトコル TCP 統合 14・Core 単体 4・Server TCP 統合 11（計 43）。
- `cargo fmt --all -- --check`、`cargo clippy --workspace --all-targets --release -- -D warnings` が警告なしで成功。
- 独立実行ファイル `inocode-server` を実際に起動し、生バイトでフレームを書き込む外部クライアントから確認: 起動ログ、`ping`、`echo`（日本語バイト列が `e6 97 a5 e6 9c ac e8 aa 9e` と一致）、`unknown_method`、不正 JSON、エラー後の継続、2 クライアント目の `echo`。
- Server 統合テストが確認する項目: 接続受付、Core への到達（echo）、Core 結果の TCP 返却、Request/Response ID 一致（0・42・u32::MAX・u64::MAX を含む）、同一接続での連続 100 往復、8 クライアント同時接続、3 クライアント間の混線なし、不正 JSON と構造不正、未知 method と params 欠落、Binary フレーム拒否、途中切断後も稼働、正常終了（`run` が `Ok` を返し、以後の接続を拒否）。
- 既存の JSON・生バイナリ通信テスト（`tests/tcp_protocol.rs`）は変更なしで成功。

## 未検証の主張

- Linux / macOS は GitHub Actions の `ubuntu-latest` / `macos-latest` 上でのみ確認しており、手元の実機検証はしていない（Windows のみローカル実機）。
- 多数同時接続時のスケーラビリティ、長時間稼働、ループバック以外のネットワーク経路、悪意ある入力の網羅的検証は未実施。
- 要求ペイロードは `FrameReader` の上限（16 MiB）まで受け付ける。上限超過時の挙動は第1段階のテストで確認済みだが、Server 経由の大きすぎる要求は未検証。

## 実装とテストの状況

- 実装: 完了（Server / Core / 要求・応答）。
- テスト: 第2段階の時点で 43 件がローカル（Windows）で成功。第3段階で `chat` / `cancel` を追加し、その後の監査是正で回帰テストを追加した結果、現在は 82 件（内訳と CI 結果は `llm-streaming.md`）。
- GitHub Actions（`.github/workflows/protocol-test.yml`、workflow_dispatch 手動実行）: run https://github.com/mouseos/inocode/actions/runs/43711649476 で ubuntu-latest / windows-latest / macos-latest の 3 ジョブすべて成功。各 OS の内訳はプロトコル単体 14、プロトコル TCP 統合 14、Core 単体 4、Server TCP 統合 11。fmt / clippy（`-D warnings`）/ build / test を各 OS で実行済み。
- 追加の承認: 第2段階では追加承認なし（範囲は指示で確定）。LLM 接続は第3段階の指示で着手し、根拠記録は `llm-streaming.md` に分離した。
