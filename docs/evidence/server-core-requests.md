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
- テスト: 上記 43 件がローカル（Windows）で成功。
- GitHub Actions（`.github/workflows/protocol-test.yml`、workflow_dispatch 手動実行）: run https://github.com/mouseos/inocode/actions/runs/43711649476 で ubuntu-latest / windows-latest / macos-latest の 3 ジョブすべて成功。各 OS の内訳はプロトコル単体 14、プロトコル TCP 統合 14、Core 単体 4、Server TCP 統合 11。fmt / clippy（`-D warnings`）/ build / test を各 OS で実行済み。
- 必要なユーザー承認: 追加なし（範囲は本タスク指示で確定）。LLM 接続は第3段階の指示まで着手しない。
