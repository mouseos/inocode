# シェルの命令を OS のサンドボックスに閉じ込める

- **対象と範囲：** `shell` の道具で実行する命令が書き込める場所を、プロジェクトのフォルダーとスレッド専用の一時フォルダー（ログのフォルダーの `tmp`）に限る。Linux・macOS・Windows。範囲外：読み取り・ネットワーク・ほかのプロセスの制限、inocode 自身の edit・write（サーバーの中で動く）、写しと巻き戻し、命令の文字列による判定。
- **関連コード：** `core/src/sandbox/`（`mod.rs` が共通の入口、OS ごとに `linux.rs`・`macos.rs`・`windows.rs`）、`core/src/shell.rs`（命令を `sandbox::confine` に通す）、`server/src/main.rs`（`sandbox-run` で自分を実行役として起動、`INOCODE_SANDBOX=off`）。
- **根拠の状態：** UNSUPPORTED_APPROVED
- **ユーザーの承認：** 2026-11-19、DSH のサンドボックスを MIT の表示つきで移植する案に「承認。違うOSの機能はできれば並列に作って早く仕上げて」。

## 方式

- 移植元：DeepSeek Harness（[deepseek-ai/deepseek-harness](https://github.com/deepseek-ai/deepseek-harness)、コミット 39163dd30f703a79079e2d2446545cff3c453738、MIT）の `packages/sandbox/`。表示は `THIRD_PARTY_NOTICES.md`。
- OS ごと（DSH と同じ）：
  - Linux：bwrap が使えればそれで、なければ Landlock（実行役が自分に Landlock をかけてから命令を起動する）。
  - macOS：Seatbelt（`sandbox-exec` とプロファイル）。
  - Windows：書き込み制限トークン（WRITE_RESTRICTED）と、書き込める場所への ACL の付与。実行役がそのトークンで命令を起動する。
- 閉じ込められないときは命令を実行しない（DSH と同じ）。`INOCODE_SANDBOX=off` で閉じ込めをやめられる。
- 実行役の失敗は `inocode-sandbox:` で始まる文を出し、終了コード 127 で終わる。

## 学術的な主張・不足点

- OS のサンドボックスがエージェントの事故を減らすことを、直接比べた研究は確認できていない。
- 関係する資料（査読前を含む）：
  - Fault-Tolerant Sandboxing for AI Coding Agents（arXiv 2512.12806、査読前）：危険な命令の遮断と、ファイルの写しからの巻き戻し。10 場面だけの小さな評価。
  - AgentSpec（ICSE 2026）：実行時に機械的に強制すると、危険な実行の 90% 超を防いだ（文字列の規則による強制で、OS のサンドボックスではない）。

## その他の根拠（研究ではない）

- 公開されている実装の先例：DSH（Linux・macOS・Windows）。DSH の `SAFETY.md` は「隔離は保証しない」と書いている。
- DSH が認める限界（移植後も同じ）：読み取り・ネットワーク・プロセスの見え方は制限しない。Windows の制限付きトークンは書き込みしか絞れない。

## 実装・検証の状況

- 未検証（実装中）。
