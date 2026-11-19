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

## OS ごとの実装上の決定（このプロジェクトの判断）

- Linux（`linux.rs`）：
  - bwrap が PATH にあり、試しの起動（5 秒まで、プロセスごとに 1 回）が通れば bwrap。`/` を読み取り専用、`/tmp` は使い捨て、2 つのフォルダーだけ書き込み可。
  - なければ実行役が no-new-privs と Landlock を自分にかけてから命令を exec する。DSH の `landlock-run`（C）を `libc` で移植した。
  - DSH との違い：Landlock では実の `/tmp` への書き込みを許さない（約束の 2 つのフォルダーに合わせた）。古い Landlock の ABI では一部だけの強制になり、そのことを `inocode-sandbox:` の 1 行で出す（DSH と同じく実行はする）。
- macOS（`macos.rs`）：`/usr/bin/sandbox-exec -p <プロファイル>`。`(allow default)(deny file-write*)` に、`/dev/null` と 2 つのフォルダー（実体のパス）の書き込みを足す。DSH が許す `/tmp` は許さない。
- Windows（`windows.rs`）：
  - 実行役が、ログオン SID・Everyone・プロジェクトのフォルダーの SID・一時フォルダーの SID を制限 SID とする WRITE_RESTRICTED・Low 整合性のトークンを作り、`CreateProcessAsUserW` で命令を起動する。kill-on-close の Job に入れる。
  - 各フォルダーに、その SID への書き込みの許可、サブフォルダーでの Everyone の `FILE_DELETE_CHILD` の拒否、Low の no-write-up のラベルを付ける。すでにあれば付け直さない。
  - **DSH にない追加：** ラベルを付けるには `WRITE_OWNER` が要る。D: ドライブなどユーザーが「変更」権限しか持たないフォルダーでは DSH の方式だと失敗し、すべての命令が拒否された。そこで、フォルダーの所有者であるユーザー自身に、そのフォルダーだけの `WRITE_OWNER` を付けてからラベルを付ける。閉じ込めた命令にはこの権限はない（手で確認）。
  - **ユーザーのデータへの影響：** 付けた許可とラベルは、フォルダーに残り続ける（DSH と同じ。次から付け直しを省くため）。最初の 1 回は、継承のためにフォルダーの木全体の ACL が書き換わる（大きなプロジェクトでは数十秒、DSH の説明）。ラベルが残るので、同じユーザーのほかの Low 整合性のプロセスも、そのフォルダーに書ける。
  - DSH から移さなかったもの：一時フォルダーの許可の取り消し（inocode の一時フォルダーはスレッドの複数の命令で共有するため）、許可の付与のロックファイル、読み取り専用モードなど。

## 実装・検証の状況

- Windows（このマシン）：`cargo fmt`・`clippy -D warnings`・`cargo test --workspace` が通過（Windows の担当による。統合後に再実行）。`server/tests/sandbox_windows.rs` で、プロジェクトと `$env:TEMP` には書ける、外のフォルダーには書けない、終了コード 7 がそのまま返ることを確認。既存の端末のテスト（`turn_tcp`）も実行役を通って通過。
- Linux（WSL2、カーネル 6.6、bwrap なし）：Landlock の経路で、2 つのフォルダーには書ける、`$HOME` と `/tmp` には書けない、終了コードがそのまま返る、実行役の失敗は 127。`turn_tcp` 14 件が通過。bwrap の経路と、Landlock のないカーネルでの拒否は未検証。
- macOS：GitHub Actions の macos-latest（[実行 44499742250](https://github.com/mouseos/inocode/actions/runs/44499742250)）で `macos.rs` の 3 件が通過。2 つのフォルダーには書ける、別のフォルダーには書けない、`/dev/null` には書ける、終了コード 7 がそのまま返る。
- 同じ実行で、ubuntu-latest の Linux の 3 件（Landlock の経路を含む）と、windows-latest の `sandbox_windows` と端末のテストも通過。3 つの OS の protocol test がすべて成功。
- 同じ実行の windows-latest のライブ試験で、Logfare の生成のテスト 1 件が時間切れで失敗した。シェルを使わないテストで、サンドボックスとは関係しない。同じジョブの公式 Anthropic のライブ試験は通過。
