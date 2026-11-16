# 会話の状態をどこで持つか

- **対象と範囲：** 会話の履歴、ツールの実行、エージェントループを、サーバー（ハーネス）側で持つか、クライアント側で持つか。範囲外：TCP の具体的なメソッド（段階 3 で決める）。
- **関連コード：** 保存層は `core/src/store.rs`（`docs/evidence/project-memory.md`）。TCP での公開は未実装。
- **根拠の状態：** UNSUPPORTED_APPROVED。適用可能な学術研究はない。公開実装の比較による判断。

## 判断

サーバーが持つ。クライアントは会話 ID と新しい入力だけを送る。履歴・ツールの実行・ループ・保存はサーバー側で行う。上流の LLM API へは、これまでどおり毎回会話全体を送る（`store: false`。プロバイダ側には状態を残さない）。

## 根拠（公開実装。研究ではない）

- **Codex**（手元の `D:\work\software\OSS\codex` で確認）：
  - クライアントはスレッドを `thread/start` で開始し、`thread/resume` で再開する（`codex-rs/app-server-protocol/src/protocol/common.rs:476, 482`）。
  - 1 ターンは `turn/start` で、スレッド ID と新しい `input` だけを送る（`protocol/v2/turn.rs:67-71`）。ターンごとに `model`・`effort`・`summary`・`cwd`・`approval_policy`・`sandbox_policy` などを上書きできる。
  - 履歴は `codex-rs/core/src/context_manager/history.rs`、保存は `codex-rs/rollout/` が持つ。
- **DeepAI Code（Pi）と DSH：** いずれもハーネス側が会話を持つ（`docs/evidence/conversation-model.md` の調査）。
- **クライアントが持つ方式を採らない理由：** ループと実行がクライアントに寄り、クライアントごとに作り直しになる。保存と再開（段階 6）がクライアント依存になる。

## Codex の保存の問題（保存層の設計に反映）

- 再開時、`load_rollout_items`（`codex-rs/rollout/src/recorder.rs:867-905`）が JSONL を全行読み、各行を JSON の木にして複製（`v.clone()`）してから、全項目をリストに保持する。ツール出力や base64 画像も含むため、長い会話でメモリ不足になる。
- 公開の報告：openai/codex #24948（会話ファイルが 700MB〜2GB）、PR #13096（再開時に 2 回読み込み、ピークのメモリが 2 倍）、#21134（デスクトップ版がスレッドの状態をメモリに抱える）。Claude Code にも同型の報告（102.7MB の会話の再開失敗）。
- 対策は `docs/evidence/project-memory.md` の判断 1〜3（SQLite、大きな中身の分離、文脈の分だけの読み込み）。

## ユーザーの承認

承認済み。2026-11-16 に「サーバーが持つ」案を提示し、ユーザーは Codex も同じかを確かめたうえで、この前提で保存形式（SQLite）の議論に進んだ。後に「会話の状態をサーバーが持つには許可したはずなんだが」と、承認済みであることを明言した。

## 実装・検証の状況

保存層のみ実装（`core/src/store.rs`）。TCP での公開、エージェントループは未実装。
