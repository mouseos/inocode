# Claude Code として名乗る版

- **対象と範囲：** Claude のアカウントで使用量（`/api/oauth/usage`）とコネクタ（`/v1/mcp_servers`、MCP の中継）を読むときに送る `User-Agent: claude-cli/<版> (external, cli)` の版を、npm で今配られている Claude Code の版にする。
  - 範囲外：ほかの提供元の名乗り。Claude Code のほかの見出し（`anthropic-beta` など）。
- **関連コード：** `core/src/claude_cli.rs`（読み取りと保持）、`core/src/llm.rs`（使用量の読み取り）、`core/src/connectors.rs`（コネクタ）、`core/src/settings.rs`（`server.claude_cli_channel`）。
- **根拠の状態：** UNSUPPORTED_APPROVED。
  - 本家と同じ版を名乗ると受け付けが変わらない、という研究や公式の説明はない（調べていない）。古い版を名乗り続けると、いつか受け付けられなくなるかもしれない、という推測による。
- **ユーザーの承認：** 2026-11-26、「claudeに名乗るのはnpmから最新バージョン番号取れたりしないか？もちろんこのためだけにnpm依存関係に増やさないでほしい」、続けて「デフォルトlatest選択stable」。

## 仕組み

- 読む先：`https://registry.npmjs.org/-/package/@anthropic-ai/claude-code/dist-tags`。認証なしで、チャンネル名と版の小さな JSON が返る。npm の道具や依存は使わず、HTTP で 1 回読むだけ。
  - 2026-11-26 の実際の答え：`{"stable":"2.1.326","latest":"2.1.332","next":"2.1.332"}`。
- チャンネル：設定の `server.claude_cli_channel` で `latest`（書かなければこれ）か `stable`。ほかの値は設定を保存するときに断る。`settings/get` の `effective.claude_cli_channel` に今の値が出る。
  - `next` もあるが、2026-11-26 の時点では `latest` と同じ版で、選べるようにはしていない。
- 読み直し：うまく読めたら 24 時間使い、それから読み直す。失敗したら前に読めた版を使い続け、1 時間後に試し直す。プロセスの中でまだ一度も読めていなければ `2.1.321` を使う。待つのは最大 5 秒。
- 版は数字と点だけのときに使う（見出しに入れるため）。それ以外の値なら `2.1.321` を使う。

## 検証

- `claude_cli::tests`：チャンネルごとの版、ないチャンネル、数字と点以外を含む値、設定からのチャンネルの読み取り。
- `server/tests/oauth_flows.rs` の `claude_usage_sends_the_reference_headers`：送った見出しが `claude-cli/<数字と点> (external, cli)` の形であること。手元では npm を実際に読めた（失敗の表示が出なかった）。
- 未確認：本家と違う版を名乗ったときに Anthropic が実際に断るかどうか。
