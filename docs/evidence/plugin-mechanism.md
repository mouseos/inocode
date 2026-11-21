# Claude Code・Codex のプラグインの仕組み（inocode から呼べるか）

- **対象と範囲：** Claude Code と Codex のプラグインとコネクタを、inocode がログイン済みのアカウント（Claude の OAuth、ChatGPT の OAuth）で使えるようにする。Claude でログインしていれば claude.ai のコネクタとプラグイン、ChatGPT でログインしていれば Codex の apps とプラグイン。両方なら両方。
- **関連ソース：** 実装中（下の「実装」）。
- **根拠の状態：** UNSUPPORTED_APPROVED。プラグインやコネクタが結果を良くするという研究は確認していない。ユーザーが機能として個別に承認した。
- **ユーザーの判断：**
  - 2026-11-21「claudeやcodexのプラグイン機能の仕組み調べて。仕組みわかればoauthさえあれば呼び出せるはずだ。」
  - 同日、調査結果（Claude 側は公式文書が第三者による Claude.ai ログインの利用を禁止していること）を伝えたあとで、「だめです。oauthで使える機能はすべて実装対象です。」
  - 同日「codex claudeそれぞれプラグイン配布のストアみたいなのがあるでしょう」「oauthログイン済みならそれも実装して。codexログイン済みならcodexのプラグイン使える。claudeも同様。両方ログイン済みなら共存していい。」
- **規約上の注意（文書の読み取り、法的判断ではない）：** Anthropic の legal-and-compliance は、第三者のアプリが Claude.ai のログインを使うことを禁止している。この制約はユーザーに伝えたうえで、ユーザーが実装を判断した。端末の証明（attestation）やボット検出を回避する処理は作らない。相手がそれを求めるなら、そこで止める。

## 実装

### コネクタ（`core/src/connectors.rs`、`core/src/mcp.rs`）

- サインインの保存場所は、スレッドの保存場所の隣の `auth.json`（既定では `~/.inocode/auth.json`、`~/.inocode/inocode.db` の隣）。ここに Claude のサインインがあれば claude.ai のコネクタを、ChatGPT のサインインがあれば ChatGPT の apps を、MCP のサーバーとして足す。両方あれば両方。`mcp.json` のサーバーと並ぶ。
- claude.ai のコネクタ（接続先は Claude Code 2.1.326 の実行ファイルの文字列から読み取った）：
  - 一覧：`GET https://api.anthropic.com/v1/mcp_servers?limit=1000`。`Authorization: Bearer <サインインのトークン>`、`anthropic-version: 2023-06-01`、`anthropic-beta: mcp-servers-2025-12-04`、`User-Agent: claude-cli/…`。答えは `{"data": [{"id", "display_name", "url", …}], "next_page"}`。`next_page` があれば `&page=` で続きを読む（最大 20 ページ）。
  - 接続：コネクタごとに `https://mcp-proxy.anthropic.com/v1/mcp/{id}` へ Streamable HTTP。`Authorization`、`User-Agent`、`X-Mcp-Client-Session-Id`（接続ごとの乱数）。サーバー名は `claude.ai <display_name>`、道具の名前は `mcp__claude_ai_<名前>__<道具>`（Claude Code と同じ形）。
  - スコープ：一覧には `user:mcp_servers` が要る。inocode の Claude のサインインで求めるスコープを Claude Code と同じ `org:create_api_key user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload user:plugins` にした。前のスコープでサインインしたトークンでは一覧が断られる。
  - 更新でスコープを広げる：Claude Code はトークンを更新するたびに `scope` を送る（2.1.326 の実行ファイルの文字列、`grant_type:"refresh_token",…,scope:M.join(" ")`。既定は `user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload` と、登録されていれば `user:plugins`）。`invalid_scope` で断られたら、保存済みのスコープでもう一度更新する。inocode は以前 `scope` を送っていなかったので、Haiku は動いても（`user:inference` は最初からある）コネクタの一覧は断られていた。これは inocode の実装の誤り。
  - 直したこと：Claude の更新で上のスコープ（`user:plugins` を含む）を送る。断られたら `scope` なしでもう一度更新する。コネクタの一覧が 403 でスコープ不足を返したら、期限前でも 1 回だけ更新して読み直す（`oauth::renew`。別の要求がすでにトークンを替えていれば更新しない）。
  - 検証：単体テスト `a_sign_in_missing_the_connector_scope_is_refreshed_once`（偽の一覧と偽のトークン窓口で、403 のあと 1 回だけ更新し、新しいトークンで読み直して保存すること）、`claude_refresh_exchanges_the_refresh_token`（`scope` に `user:mcp_servers` が入ること）、`claude_refresh_keeps_the_old_scopes_when_new_ones_are_refused`。本物の窓口が更新でスコープを広げるかは未確認。
- ChatGPT の apps（Codex のソース `codex-rs/codex-mcp/src/mcp/mod.rs`、`connection_manager.rs` による）：`https://chatgpt.com/backend-api/wham/apps` へ Streamable HTTP。`Authorization: Bearer <ChatGPT のトークン>` と `ChatGPT-Account-ID`。サーバー名は `codex_apps`。
- トークンは要求ごとに保存場所から読み、期限切れなら更新する（モデルの要求と同じ `oauth::fresh`）。
- 検証：単体テスト `signed_in_accounts_bring_their_connectors`（偽のサーバーで、サインインがないと何も読まないこと、両方のサインインで両方の道具が出ること、各要求に正しいヘッダーが付くこと）。
- 実測（2026-11-21、ユーザーの「1 回ずつ試して」。手動のテスト `lists_the_real_connectors`。道具は呼んでいない）：
  - claude.ai：一覧の要求 1 回。403「OAuth token does not meet scope requirement user:mcp_servers」。前のスコープでサインインしたトークンのため。サインインし直してから、もう一度確かめる必要がある。
  - ChatGPT の apps：初期化、初期化の通知、道具の一覧の 3 回（接続を閉じるときの DELETE がもう 1 回送られた可能性がある）。道具は 415 個で、名前は `github_add_comment_to_issue` など、アプリ名が頭に付く形。
- ChatGPT の apps の中身（2026-11-22、ユーザーの Codex が保存した一覧 `~/.codex/cache/codex_apps_tools/*.json` を数えた。通信はしていない）：道具 532 個、app 19 個。各道具の `_meta._codex_apps.resource_uri` の 2 番目の部分で、出どころが分かれる。
  - ユーザーがつないだ app（`link_…`）：GitHub 91、Google Drive 45、Remote Desktop Commander 32、Gmail 21、Google Calendar 15、Notion 82、ほか 3 つ。
  - OpenAI が最初から入れている app（`implicit_link`）：ChatGPT Space 42、Sites 39、Pets 11、Plugin Management 6、Safety Settings 5 など 8 つ。
  - `synthetic_link`：Linear 93、Atlassian 30。
  - Codex はこのうち設定で切っていない app を全部「使える app」とし、合計が 100 個以上なら最初から渡さず `tool_search` で探させる（`codex-rs/core/src/mcp_tool_exposure.rs`、`DIRECT_MCP_TOOL_EXPOSURE_THRESHOLD = 100`）。
  - ユーザーの指摘（2026-11-22）：「appsってnotionとかでしょ。あれ自分で追加するんであって全部llmに渡すものじゃない。前提からしておかしい。」どの app を渡すかの決め方は未決。
- 道具が多すぎる問題：道具はすべて毎回の要求に入る。OpenAI の Chat Completions は道具を 128 個までしか受けない（OpenAI の API 文書）。415 個を渡すと要求が重く、断られるおそれもある。Codex は apps の道具を最初から渡さず、`tool_search` で必要なものだけ読み込む（`codex-rs/core/src/context/apps_instructions.rs`）。inocode にはこの仕組みがないので、当面、コネクタ 1 つの道具が 100 個（`CONNECTOR_TOOLS`）を超えたら、モデルに渡さず理由を表示する。このため今は ChatGPT の apps は使えない。道具を探して読み込む仕組みは新しい機能で、研究の確認とユーザーの承認が先（UNSUPPORTED_PENDING）。
- **調べ方：** サブエージェントが公式文書、openai/codex のソース、ローカルの claude.exe の文字列、ローカルに入っているプラグインを読んだ報告（以下、報告のまま）。認証付きの通信はしていない。

---


調査日: 2026-11-21。読み取りのみ。認証付き通信は一切していない。

凡例:
- 【確認: 公式文書】= 公式ドキュメントに書いてある
- 【確認: ソース】= openai/codex のソースコードで確認（ローカルクローン `D:\work\software\OSS\codex`、コミット `683bd170dc36812f73867aa6f421a4a8cf4bd133`、2026-06-18）
- 【確認: バイナリ文字列】= ローカルの Claude Code 実行ファイル `C:\Users\yuuma\.local\bin\claude.exe`（v2.1.284）内の文字列から確認。挙動の詳細までは追っていない
- 【確認: 実ファイル】= `C:\Users\yuuma\.claude\plugins\` / `C:\Users\yuuma\.codex\plugins\` の実物
- 【推測】= 上記からの推測。未検証
- 不明 = 根拠が見つからなかった

認証ファイル（`auth.json`、`.credentials.json` など）は開いていない。

---

## 0. 結論（仮説「仕組みが分かれば OAuth だけでプラグインを呼べる」の判定）

**部分的に正しい。ただし「プラグイン」は中身の種類ごとに必要なものがまったく違うので、一括りには言えない。**

1. プラグインの大半（スキル、スラッシュコマンド、エージェント定義、フック、ローカル起動の MCP サーバー、LSP）は、**そもそも OAuth が要らない**。ただのファイル（Markdown / JSON）とローカルプロセスであり、Git リポジトリや公開 URL から取得できる。inocode が同じ形式を読んで同じように動かせば再現できる。
2. 外部サービスにつながる部分（Notion、GitHub など）は2通りに分かれる。
   - (a) プラグインの `.mcp.json` にサービスの URL が直接書いてあるもの（例: Notion プラグイン → `https://mcp.notion.com/mcp`、GitHub プラグイン → `https://api.githubcopilot.com/mcp/`）。これは **そのサービス自身の認証**（Notion の OAuth、GitHub の個人トークン）が必要で、Claude/ChatGPT の OAuth では呼べない。
   - (b) Anthropic / OpenAI のサーバーが中継する「コネクタ」（claude.ai コネクタ、Codex の apps）。これは **Claude/ChatGPT の OAuth トークンだけで呼べる仕組みになっている**。サービスごとの認証（Notion 等の OAuth）は事前に claude.ai / ChatGPT の画面で済ませてあり、その資格情報は Anthropic / OpenAI 側に保管され、中継サーバーが差し込む。
3. ただし (b) と、claude.ai からのプラグイン同期は、**公式に公開された API ではなく、各社の自社クライアント向けの内部エンドポイント**である。
   - Anthropic は利用規約・公式文書で「第三者が Claude.ai ログインを自分のアプリに提供すること」「Free/Pro/Max/Ultra の資格情報を経由させること」「Claude.ai の資格情報・セッショントークンを収集・保存・仲介すること」を明示的に禁止している（後述 4章）。inocode が Claude の OAuth トークンを使ってコネクタ中継を叩くのはこの禁止に当たる可能性が高い。
   - OpenAI 側は、第三者ツールが ChatGPT ログインのトークンを使うことの可否を明記した文書を見つけられなかった（不明）。Codex 本体はオープンソースだが、中継先の `chatgpt.com/backend-api/...` は非公開 API。

まとめ: 技術的には「OAuth トークン＋中継エンドポイントの仕様」で (b) は呼べる作りになっているが、Claude 側は規約上不可、OpenAI 側は規約上の扱いが不明。(1) と (2a) は OAuth と無関係に再現できる。

---

## 1. Claude Code のプラグイン

### 1.1 ディスク上の形式 【確認: 公式文書】

出典: https://code.claude.com/docs/en/plugins-reference （manifest reference）

- マニフェスト: プラグインのルートに `.claude-plugin/plugin.json`。**省略可**。必須項目は `name` のみ。
- 標準の配置（マニフェストで別の場所も指定可）:

| 種類 | 既定の場所 | 中身 |
|---|---|---|
| スキル | `skills/<名前>/SKILL.md` | Markdown（先頭に name/description） |
| コマンド | `commands/*.md` | Markdown（新規はスキル推奨） |
| エージェント | `agents/*.md` | Markdown |
| フック | `hooks/hooks.json` | JSON（イベント→シェルコマンド等） |
| MCP サーバー | `.mcp.json` | JSON（stdio / http / sse / ws、`.mcpb`/`.dxt` バンドルも可） |
| LSP サーバー | `.lsp.json` | JSON（command, extensionToLanguage 必須） |
| 出力スタイル | `output-styles/` | Markdown |
| ワークフロー | `workflows/*.js` | JS |
| テーマ | `themes/` | JSON（experimental） |
| モニター | `monitors/monitors.json` | 常駐シェルコマンド（experimental） |
| 実行ファイル | `bin/` | Bash ツールの PATH に入る |

- その他の項目: `userConfig`（利用者に入力させる値。`sensitive: true` は OS の資格情報ストアへ）、`channels`、`dependencies`、`engines`、`defaultEnabled` など。
- 置換変数: `${CLAUDE_PLUGIN_ROOT}`（インストール先）、`${CLAUDE_PLUGIN_DATA}`（`~/.claude/plugins/data/<id>/`）、`${CLAUDE_PROJECT_DIR}`、`${user_config.KEY}`。フックには `CLAUDE_PLUGIN_OPTION_<KEY>` 環境変数で渡る。
- 名前空間: 部品は `プラグイン名:部品名` になる。MCP ツールは `mcp__plugin_<プラグイン名>_<サーバー名>__<ツール名>`（出典: https://code.claude.com/docs/en/mcp ）。
- プラグイン直下の `CLAUDE.md` は読まれない。

### 1.2 マーケットプレイス 【確認: 公式文書】

出典: https://code.claude.com/docs/en/plugin-marketplaces 、https://code.claude.com/docs/en/plugins/org.md

- マーケットプレイス = `.claude-plugin/marketplace.json` を持つディレクトリ／リポジトリ。必須は `name`、`owner`、`plugins`（各要素に `name` と `source`）。
- プラグインの `source` の種類: 相対パス、`github`、`git-subdir`、`url`（任意ホストの git）、`archive`（HTTPS の zip）、`npm`、`command`（手元でコマンドを実行して生成）。
- 取得方法: **git clone**（利用者の git と既存の git 資格情報を使う）。`url` 型のマーケットプレイスは HTTP で `marketplace.json` を取得（任意で `headers`）。つまり **Claude の OAuth は不要**。
- 公式マーケットプレイス `claude-plugins-official` の実体は GitHub の `anthropics/claude-plugins-official`（公開）。

### 1.3 ローカルのキャッシュ 【確認: 公式文書 + 実ファイル】

出典: https://code.claude.com/docs/en/plugins/loading.md

| パス | 中身 |
|---|---|
| `~/.claude/plugins/known_marketplaces.json` | 登録済みマーケットプレイスと取得元 |
| `~/.claude/plugins/installed_plugins.json` | インストール記録（scope, installPath, version, gitCommitSha） |
| `~/.claude/plugins/marketplaces/<名前>/` | マーケットプレイスの clone |
| `~/.claude/plugins/cache/<マーケット>/<プラグイン>/<版>/` | プラグイン本体のコピー |
| `~/.claude/plugins/data/<id>/` | プラグインの永続データ |
| `~/.claude/plugins/synced/` | claude.ai アカウントから同期したプラグイン |
| `~/.claude/plugins/known_marketplaces_claudeai.json` | claude.ai 上のマーケットプレイス |

実物で確認した内容:
- `known_marketplaces.json`: `claude-plugins-official` → `{"source":"github","repo":"anthropics/claude-plugins-official"}`、`i-have-adhd` → `{"source":"directory", path: C:\Users\yuuma\.codex\plugins\cache\i-have-adhd\i-have-adhd\0.4.1}`（Codex 側のキャッシュをそのまま Claude のマーケットプレイスとして使っている）。
- Notion プラグイン `C:\Users\yuuma\.claude\plugins\cache\claude-plugins-official\notion\0.1.0\`:
  - `.mcp.json` = `{"mcpServers":{"notion":{"type":"http","url":"https://mcp.notion.com/mcp"}}}` → **Notion の公開 MCP サーバーへ直接つなぐ**。
  - `commands/*.md`（search, create-page など）と `skills/notion/*/SKILL.md` は Markdown のみ。
- GitHub プラグイン（`cache\claude-plugins-official\github\d7d32797e7e0\.mcp.json` と、同期版 `synced\f9235308-...\8cc1afaf-...\.mcp.json` は同内容）:
  - `url: https://api.githubcopilot.com/mcp/`、`headers: {"Authorization": "Bearer ${GITHUB_PERSONAL_ACCESS_TOKEN}"}` → **GitHub の個人トークンが必要**。今回のセッションでも `plugin:github:github` が「Authorization header is badly formatted」で接続失敗しており、環境変数未設定と整合する【推測】。
- `synced\...\manifest.json`: `marketplaceName: "anthropic-plugin-directory"`、`pluginId: "plugin_01UZBy..."`。claude.ai 側の ID で管理されている。
- i-have-adhd: `hooks/hooks.json` に `SessionStart` で `node -e ...` を実行するフック。完全にローカル処理。

### 1.4 claude.ai から同期されるプラグイン 【確認: 公式文書 + バイナリ文字列】

- 公式文書（loading.md「Plugins synced from claude.ai」）: claude.ai アカウントで有効にしたプラグイン（組織が有効にしたものを含む）は `<名前>@synced` として読み込まれる。**claude.ai アカウントでサインインしたセッションでのみ同期**。`ANTHROPIC_AUTH_TOKEN`、`CLAUDE_CODE_OAUTH_TOKEN`（`claude setup-token` の長期トークン）、`apiKeyHelper` のセッションでは同期しない。
- バイナリ文字列: OAuth のスコープに `user:plugins`、`user:mcp_servers`、`user:inference`、`user:profile`、`user:sessions:claude_code`、`user:file_upload` がある。同期用と思われる URL 雛形:
  - `/api/oauth/organizations/:orgUUID/marketplaces/${...}/plugins?...`
  - `/api/oauth/organizations/:orgUUID/plugins/${...}/download...`
  - `/api/organizations/:orgUUID/marketplace/plugin-assets/${...}`
- 【推測】これらは claude.ai の OAuth（`user:plugins` スコープ）でプラグインの zip 等を取得する内部 API。ホスト名（api.anthropic.com か claude.ai か）、応答形式は不明。`getSyncedPluginAttestation` という関数名もあり、同期プラグインに何らかの「証明」処理がある可能性があるが内容は不明。
- 同期したプラグインの中身自体は通常のプラグイン（上の GitHub プラグインと同じ形式）。**一度取得すれば中身の実行に Claude の OAuth は不要**。

### 1.5 claude.ai コネクタ（Anthropic 中継の MCP）

【確認: 公式文書】出典: https://code.claude.com/docs/en/mcp 「Use MCP servers from claude.ai」「How connectors reach Claude Code」
- コネクタは claude.ai/customize/connectors で追加し、**サービス側の認証も claude.ai で済ませる**。
- Claude Code（ターミナル、VS Code、JetBrains、Agent SDK）は claude.ai から一覧を取得する。**有効な認証が claude.ai サブスクリプションのログインのときだけ**。API キー、`ANTHROPIC_AUTH_TOKEN`、`CLAUDE_CODE_OAUTH_TOKEN`（setup-token は「モデルへのリクエストしかできない」と明記）などでは読み込まれない。
- `claude.ai rejected the session token` という状態がある＝**コネクタ呼び出しに Claude Code ログインのトークンそのものを使っている**。
- Microsoft 365、Gmail、Google Calendar などは「上流の認証プロバイダが claude.ai のリダイレクト URL しか受け付けない」ため、手元からの直接 OAuth は不可。
- クラウド／セルフホストのセッションでは、コネクタの通信は `api.anthropic.com` 経由で、Anthropic のインフラからツールが呼ばれる。許可リスト用 URL として `https://api.anthropic.com/v2/ccr-sessions/*`、`https://api.anthropic.com/v1/code/sessions/*`、`https://api.anthropic.com/v1/code/mcp/*` が示されている（出典: https://code.claude.com/docs/en/self-hosted-environments-deploy.md 「Connector traffic leaves your network」）。

【確認: バイナリ文字列】（v2.1.284）
- 設定値: `MCP_PROXY_URL:"https://mcp-proxy.anthropic.com"`、`MCP_PROXY_PATH:"/v1/mcp/{server_id}"`。
- 一覧取得: `GET /v1/mcp_servers?limit=1000`、ヘッダ `anthropic-version: 2023-06-01`、`anthropic-beta`（同じバイナリ内に `mcp-servers-2025-12-04` という値あり）、`Authorization: Bearer <OAuth トークン>`。バイナリ内の説明文に「`$CLAUDE_CODE_OAUTH_TOKEN` があるとき curl でこの一覧を取れる」との記述がある。
- 【推測】ターミナル版は `https://mcp-proxy.anthropic.com/v1/mcp/{server_id}` に、claude.ai の OAuth Bearer をつけて Streamable HTTP の MCP として接続し、中継サーバーが Notion 等の資格情報を差し込んで上流へ転送する。正確なヘッダ（anthropic-beta の値など）・応答形式・認可スコープの要否（`user:mcp_servers`）は未検証。

### 1.6 各部品の実行のされ方（Claude Code）

| 部品 | 実行のされ方 | 必要な認証 |
|---|---|---|
| スキル（SKILL.md） | テキスト。名前と説明が文脈に入り、使うときに本文を読み込む 【公式文書: plugins/components】 | なし |
| コマンド（commands/*.md） | テキスト。`$ARGUMENTS` を差し込んだ本文をプロンプトとして送る 【実ファイル: notion/commands/search.md】 | なし |
| エージェント（agents/*.md） | テキスト。サブエージェントのシステムプロンプト等 | なし |
| 出力スタイル | テキスト | なし |
| フック | ローカルのシェルコマンド（または `mcp_tool` 等）をイベント時に実行 | なし（中身次第） |
| stdio MCP | ローカルプロセス | なし（中身次第） |
| http MCP（URL直書き） | プラグイン作者が指定したサービスへ直接接続 | **そのサービスの認証**（Notion OAuth、GitHub PAT 等）。Claude Code が RFC 9728/8414 で OAuth を自分で行う 【公式文書: mcp】 |
| LSP | ローカルプロセス | なし |
| モニター / bin / workflows | ローカル | なし |
| claude.ai コネクタ | Anthropic の中継サーバー経由 | **Claude の OAuth（claude.ai ログイン）**。サービス側の認証は claude.ai 側に保管 |
| 同期プラグインの取得 | claude.ai の内部 API からダウンロード | **Claude の OAuth（`user:plugins`）**【バイナリ文字列 + 推測】 |

---

## 2. Codex のプラグイン

### 2.1 ディスク上の形式 【確認: ソース + 実ファイル】

- マニフェスト: `.codex-plugin/plugin.json`。**`.claude-plugin/plugin.json` も代替として読む**（`codex-rs/core-plugins/src/manifest.rs:478` `ALTERNATE_PLUGIN_MANIFEST_RELATIVE_PATH = ".claude-plugin/plugin.json"`）。
- 部品の種類（`codex-rs/plugin/src/manifest.rs:20-23`）: `skills`、`mcp_servers`（JSON では `mcpServers`）、`apps`、`hooks` の4種類だけ。加えて表示用の `interface`（displayName, category, capabilities, logo など）。
- 既定ファイル（`codex-rs/core-plugins/src/loader.rs:56-59`）: `skills/`、`hooks/hooks.json`、`.mcp.json`、`.app.json`。
- **Claude Code にある commands / agents / output-styles / LSP / monitors / bin に当たるものは Codex のプラグイン形式には見当たらない**（loader.rs に該当なし）。
- 実物:
  - `C:\Users\yuuma\.codex\plugins\cache\chatgpt-global\github\0.1.5\.app.json` = `{"apps":{"github":{"id":"connector_76869538009648d5b282a4bb21c3d157"}}}` → **中身は ChatGPT 側コネクタの ID だけ**。plugin.json は `"skills":"./skills/","apps":"./.app.json"`。
  - `...\openai-bundled\codex-app-tools\26.1111.21308\.mcp.json` = `cmd.exe` で `server.mjs` を起動する stdio MCP（ライセンス Proprietary、デスクトップアプリ用）。
  - `...\openai-bundled\computer-use\26.1111.21308\.codex-plugin\plugin.json` は `hooks` を内包し、種類 `"type":"mcp_tool"`（`node_repl` サーバーの `turn_ended` を呼ぶ）。
  - `...\i-have-adhd\i-have-adhd\0.4.1\.codex-plugin\plugin.json` は `"skills":"./skills/"` のみ。同じリポジトリに `.claude-plugin/` も同居。

### 2.2 マーケットプレイスとカタログ 【確認: ソース + 実ファイル】

- マーケットプレイスのファイル（`codex-rs/core-plugins/src/marketplace.rs:19-22`）: `.agents/plugins/marketplace.json`、`.agents/plugins/api_marketplace.json`、`.claude-plugin/marketplace.json`（**Claude 形式も読む**）。
- `config.toml` での登録例（実物 `C:\Users\yuuma\.codex\config.toml`）: `[marketplaces.i-have-adhd] source_type = "git"`, `source = "https://github.com/ayghri/i-have-adhd.git"`, `ref = "main"`。ローカルのものは `source_type = "local"`。プラグインの有効化は `[plugins."<名前>@<マーケット>"] enabled = true`。
- OpenAI 公式の「curated」カタログ（`codex-rs/core-plugins/src/startup_sync.rs:19-30`）: `https://github.com/openai/plugins.git` の `refs/codex/curated-sync` を git で取得、失敗時の予備として `https://chatgpt.com/backend-api/plugins/export/curated` からアーカイブ取得。保存先 `~/.codex/.tmp/plugins`。
- リモート（ChatGPT アカウント側）のプラグインカタログ（`codex-rs/core-plugins/src/remote.rs`）: `{chatgpt_base_url}/ps/plugins/list`、`/ps/plugins/installed`、`/ps/plugins/{id}`、`/ps/plugins/{id}/install`、`/uninstall`、`/ps/plugins/workspace/shared`、`/ps/plugins/suggested` など。既定のベース URL は `https://chatgpt.com/backend-api/`（`codex-rs/core/src/config/mod.rs:3644`）。**ChatGPT 認証必須で API キー不可**（remote.rs:282-286 のエラーメッセージ「chatgpt authentication required for remote plugin catalog; api key auth is not supported」）。
- キャッシュ: `~/.codex/plugins/cache/<マーケット>/<プラグイン>/<版>/`（実物で確認: `chatgpt-global`、`openai-bundled`、`i-have-adhd`、`created-by-me-remote`）。

### 2.3 Codex の apps（コネクタ）＝ ChatGPT 中継の MCP 【確認: ソース】

- 内部 MCP サーバー名 `codex_apps`（`codex-rs/codex-mcp/src/mcp/mod.rs:48`）。
- 接続先 URL（mod.rs:430-450）: ベースが `https://chatgpt.com` なら `https://chatgpt.com/backend-api/wham/apps`。ベースが別なら `{base}/api/codex/apps`。さらに「ChatGPT ホストのプラグイン実行環境」として `{base}/ps/mcp` もある（mod.rs:464-475）。
- 方式: Streamable HTTP の MCP。任意で `X-OpenAI-Product-Sku` ヘッダ（mod.rs:477-481）。
- 認証（`codex-rs/codex-mcp/src/connection_manager.rs:156-197`、`codex-rs/model-provider/src/auth.rs:113-126`、`codex-rs/model-provider/src/bearer_auth_provider.rs:31-46`）: ChatGPT ログイン系の認証（Chatgpt / ChatgptAuthTokens / AgentIdentity / PersonalAccessToken）のとき、**モデル呼び出しと同じ** `Authorization: Bearer <ChatGPT アクセストークン>` と `ChatGPT-Account-ID: <アカウント ID>`（FedRAMP なら `X-OpenAI-Fedramp: true`）を付ける。環境変数 `CODEX_CONNECTORS_TOKEN` があればそちらを Bearer に使う（mod.rs:51, 421-428）。
- 有効条件（mod.rs:234-236）: `apps_enabled` かつ認証が ChatGPT 系（`uses_codex_backend`）。**API キー認証では apps は無効**（`app-server-protocol/src/protocol/common.rs:61-68`）。
- サービス側の認証: Codex は持たない。未連携なら、ツール結果の `_meta` に `connector_auth_failure`（connector_id、install_url など）が返り、Codex は利用者に install_url を開かせる（`codex-rs/codex-mcp/src/auth_elicitation.rs`）。**サービスの OAuth は ChatGPT 側で行い、資格情報は OpenAI 側に保管される**【ソース + 推測】。
- プラグインの `.app.json` はコネクタ ID を並べるだけで、実際のツールは `codex_apps` MCP から取得し、ID で絞り込む（`core/src/context/apps_instructions.rs:35` の説明文「An app is equivalent to a set of MCP tools within the codex_apps MCP」）。

### 2.4 Codex のスキル 【確認: ソース】

- `codex-rs/core-skills/src/render.rs:25-32`: スキル一覧（名前・説明・場所）を文脈に入れ、使うと決めたらモデルが `SKILL.md` を最後まで読む、という指示文。＝**テキスト注入方式で、実行は不要**。

### 2.5 各部品の実行のされ方（Codex）

| 部品 | 実行のされ方 | 必要な認証 |
|---|---|---|
| skills | テキスト（一覧を注入し、モデルが SKILL.md を読む） | なし |
| mcpServers（stdio） | ローカルプロセス | なし（中身次第） |
| mcpServers（HTTP） | 指定先へ直接 | そのサービスの認証（Codex 内蔵の OAuth または bearer_token_env_var） |
| hooks | ローカルコマンド、または `mcp_tool` 型で MCP ツールを呼ぶ | なし（中身次第） |
| apps（.app.json） | `chatgpt.com/backend-api/wham/apps` の中継 MCP | **ChatGPT の OAuth トークン + アカウント ID**。サービス側の認証は ChatGPT 側 |
| リモートカタログ・インストール | `chatgpt.com/backend-api/ps/plugins/...` | **ChatGPT の OAuth**（API キー不可） |
| curated カタログ | 公開 GitHub `openai/plugins`（予備は chatgpt.com） | なし（予備経路は不明） |

---

## 3. 部品の種類別まとめ（inocode から見て）

| 部品 | Claude Code | Codex | inocode で再現するのに必要なもの |
|---|---|---|---|
| スキル | SKILL.md（テキスト） | SKILL.md（テキスト） | ファイルを読むだけ。OAuth 不要 |
| コマンド | commands/*.md（テキスト） | なし | ファイルを読むだけ |
| エージェント | agents/*.md（テキスト） | なし（プラグイン形式には無い） | ファイルを読むだけ |
| フック | ローカルコマンド | ローカルコマンド / mcp_tool | 同じイベント名・入出力 JSON を実装。OAuth 不要 |
| ローカル MCP | stdio プロセス | stdio プロセス | MCP クライアント実装。OAuth 不要 |
| 直書き HTTP MCP | 例: mcp.notion.com | 同様 | **そのサービスの OAuth / トークン**。Claude/ChatGPT の OAuth では不可 |
| LSP | ローカルプロセス | なし | LSP クライアント実装 |
| Anthropic 中継コネクタ | mcp-proxy.anthropic.com | — | Claude の OAuth（技術的には可能そうだが規約で禁止） |
| OpenAI 中継 apps | — | chatgpt.com/backend-api/wham/apps | ChatGPT の OAuth + アカウント ID（規約上の扱いは不明） |
| プラグイン配布（Git/公開） | GitHub 等を git clone | GitHub 等を git clone | git だけ。OAuth 不要 |
| プラグイン配布（アカウント同期） | claude.ai 内部 API（`user:plugins`） | chatgpt.com `/ps/plugins/...` | 各社 OAuth（Claude 側は規約で禁止） |

---

## 4. 第三者には再現できない／規約上の制約

### Anthropic 【確認: 公式文書】
出典: https://code.claude.com/docs/en/legal-and-compliance 「Authentication and credential use」
- OAuth 認証は「サブスクリプション購入者が Claude Code とその他の Anthropic 純正アプリを普通に使うため」のもの。
- 「Anthropic は第三者開発者が Claude.ai ログインを自分のアプリに提供すること、Free/Pro/Max/Ultra の資格情報を経由してリクエストを流すことを許可しない」「開発者は Claude.ai の資格情報やセッショントークンを収集・保存・仲介してはならない。サインインは Anthropic 自身の手順で完結しなければならない」（要旨。原文は英語）。
- 違反には予告なく対処しうると明記。
- 例外: **改変していない Claude Code 本体**に利用者自身がログインして動かすことは許される。
- 技術的に第三者で再現できないもの:
  - Gmail / Google Calendar / Microsoft 365 等は上流が claude.ai のリダイレクト URL しか受けないため、自前 OAuth では接続できない（公式文書 mcp）。
  - サービス側の資格情報は Anthropic 側に保管されていて取り出せない。中継経由でしか使えない。
- 判断: **inocode が保持している Claude の OAuth トークンでコネクタ中継や同期 API を叩くことは、上記規約に抵触する可能性が高い**。（法的判断ではなく文書の読み取り）

### OpenAI 【確認: 公式文書 + ソース】
- https://learn.chatgpt.com/docs/auth （旧 developers.openai.com/codex/auth）: ログイン情報は `~/.codex/auth.json` か OS の資格情報ストアに保存。「パスワード同様に扱え」。API キーでは「ChatGPT ワークスペースやクラウドに依存する機能は制限・利用不可」「一部プラグインは OAuth の都合で API キーでは使えない」。
- https://learn.chatgpt.com/docs/plugins : 「外部サービスへの接続はそのサービス自身の認証とアクセス制御を使う」。
- 第三者のツールが ChatGPT ログインのトークンを使ってよいかを明記した文書は**見つからなかった（不明）**。OpenAI 全般の利用規約の該当条項は今回確認していない（不明）。
- Codex には `x-oai-attestation` ヘッダ（`codex-rs/core/src/client.rs:558` など）を、デスクトップアプリ等の「アテステーション提供者」があるときだけモデル要求に付ける仕組みがある（`codex-rs/app-server/src/attestation.rs`）。apps の MCP 接続にも付くか、サーバーが必須にしているかは不明（`app-server/src/mcp_refresh.rs:252` では `None` を渡している箇所あり）。
- Codex 本体はオープンソースなので、`wham/apps` への接続方法はソースから正確に再現できる。ただし API 自体は非公開・無保証。

---

## 5. 未解決の点（不明）

1. claude.ai 同期プラグインのダウンロード API の正確なホスト名・認証ヘッダ・応答形式（URL の雛形のみ確認）。
2. `getSyncedPluginAttestation` の中身（同期プラグインに署名や証明が付くのか）。
3. `mcp-proxy.anthropic.com/v1/mcp/{server_id}` 接続時の正確なヘッダ（anthropic-beta の値、必要スコープ）と、第三者クライアントを弾く仕組みの有無。
4. ChatGPT の `wham/apps` / `/ps/mcp` が `x-oai-attestation` や originator 等のクライアント識別を必須にしているか。
5. OpenAI が第三者ツールによる ChatGPT ログイントークンの利用を許可／禁止しているか（明文を発見できず）。
6. Codex の curated カタログ予備経路（`/plugins/export/curated`）の認証要否。
7. ローカルの claude.exe は v2.1.284（2026-09-29 時点）。現在動いている Claude Code の版とは違う可能性があり、エンドポイントが変わっている可能性がある。

---

## 6. 参照した主なファイル・URL

公式文書:
- https://code.claude.com/docs/en/plugins-reference
- https://code.claude.com/docs/en/plugin-marketplaces
- https://code.claude.com/docs/en/plugins/loading.md
- https://code.claude.com/docs/en/plugins/org.md
- https://code.claude.com/docs/en/mcp
- https://code.claude.com/docs/en/self-hosted-environments-deploy.md
- https://code.claude.com/docs/en/legal-and-compliance
- https://learn.chatgpt.com/docs/plugins
- https://learn.chatgpt.com/docs/auth

Codex ソース（`D:\work\software\OSS\codex\codex-rs\`）:
- `codex-mcp/src/mcp/mod.rs`（codex_apps の URL と設定）
- `codex-mcp/src/connection_manager.rs`（ChatGPT 認証の付与）
- `codex-mcp/src/mcp/auth.rs`、`codex-mcp/src/auth_elicitation.rs`
- `model-provider/src/auth.rs`、`model-provider/src/bearer_auth_provider.rs`
- `app-server-protocol/src/protocol/common.rs`（uses_codex_backend）
- `plugin/src/manifest.rs`、`core-plugins/src/manifest.rs`、`core-plugins/src/loader.rs`、`core-plugins/src/marketplace.rs`
- `core-plugins/src/startup_sync.rs`、`core-plugins/src/remote.rs`
- `core-skills/src/render.rs`、`core/src/context/apps_instructions.rs`
- `app-server/src/attestation.rs`、`core/src/client.rs`

ローカル実ファイル:
- `C:\Users\yuuma\.claude\plugins\known_marketplaces.json`、`installed_plugins.json`
- `C:\Users\yuuma\.claude\plugins\cache\claude-plugins-official\notion\0.1.0\`（.mcp.json, commands, skills）
- `C:\Users\yuuma\.claude\plugins\cache\claude-plugins-official\github\d7d32797e7e0\.mcp.json`
- `C:\Users\yuuma\.claude\plugins\synced\f9235308-...\`（manifest.json, 8cc1afaf-...）
- `C:\Users\yuuma\.claude\plugins\cache\i-have-adhd\i-have-adhd\0.4.1\hooks\hooks.json`
- `C:\Users\yuuma\.codex\config.toml`（plugins / marketplaces / apps 節のみ）
- `C:\Users\yuuma\.codex\plugins\cache\chatgpt-global\github\0.1.5\`（.app.json, plugin.json, skills）
- `C:\Users\yuuma\.codex\plugins\cache\openai-bundled\codex-app-tools\26.1111.21308\.mcp.json`
- `C:\Users\yuuma\.codex\plugins\cache\openai-bundled\computer-use\26.1111.21308\.codex-plugin\plugin.json`
- `C:\Users\yuuma\.local\bin\claude.exe`（文字列検索のみ）
