# プロバイダ・モデル・アカウントの登録と、アカウントのプール

- **対象と範囲：**
  - プロバイダ・モデルの登録・一覧・変更・削除。
  - 会話ごとのプロバイダ・モデル・思考の深さの選択。
  - 1 つのプロバイダに複数のアカウント（OAuth・API キー）を持ち、送るたびに選ぶプール。
  - 認証情報の保存方法。
  - 環境変数での設定をやめる。
- **関連ソースコード：** 未実装。今の設定は `core/src/llm.rs`（`LlmConfig::from_env`）、認証情報は `core/src/oauth.rs`（`~/.inocode/auth.json`、種類ごとに 1 件）、ログインは `server/src/main.rs`（`login`）。
- **根拠の状態：** UNSUPPORTED_APPROVED。学術的な裏付けは調べていない（使い勝手と互換のための仕組み）。ユーザーの指示（2026-11-25）：
  - 「環境変数での制御はだめ」
  - 「モデル登録やGPTなどoauth系opencodex同様のプールはできるのか？CRUDできるのか？それもかんがえて」
  - 「プールは、opencodex と同じだけ作る方向でよい」
- **注意（ユーザーに伝えた）：** 複数の有料契約を束ねて上限を避ける使い方は、OpenAI・Anthropic の利用規約に触れるおそれがある。

## 手本：opencodex（`refs/opencodex`、v2.100.0、2026-11-15 のコミット 1d10758f を読んだ）

- 登録：`~/.opencodex/config.json` の `providers`。HTTP の管理 API（`/api/providers` などの GET・PUT・POST・PATCH・DELETE）、CLI（`ocx provider …`、`ocx account …`）、GUI から変えられる。
- アカウントの保存：一般の OAuth は `auth.json` にプロバイダごとの `{activeAccountId, accounts[]}`。各アカウントは id・別名・資格情報・再ログイン要否・一時停止など。Codex は別の台帳（`codex-accounts.json`）に優先度・自動切り替えの閾値などを持つ。
- 選び方（`src/oauth/pool-kernel.ts`、`src/codex/routing.ts`、`src/codex/routing/selection.ts`）：
  - 方式は quota（使用量の少ない順、既定）、round-robin、fill-first、reset-first。
  - 会話をアカウントに固定する（キャッシュのため。24 時間使わなければ外す。SQLite に保存）。
  - 優先度の段、自動切り替えの閾値（既定 80%）。
- 失敗の扱い（`src/codex/routing/cooldown-math.ts` など）：
  - 429・402：`Retry-After`（最大 24 時間）か、上限が戻る時刻まで休ませて、別のアカウントへ移る。
  - 401・403：再ログインが要る印を付ける。
  - 5xx・時間切れ：3 回続けば切り替える。回避は 30 秒・2 分・10 分・30 分と段階的に延ばす。60 秒の失敗率（20 件以上で 25%）でも外す。5 分ごとに 1 回だけ試して戻すこともある。
- 使用率：Codex は応答ヘッダー `x-codex-*-used-percent` と使用量 API、Anthropic は `anthropic-ratelimit-unified-*`。
- 量：アカウントの選択と切り替えの部分で約 9,000 行（TypeScript）。組み合わせや経路の決まりまで含めると約 17,000 行。

## 方針（ユーザーと決めたこと、2026-11-25）

- 環境変数での設定をやめ、設定ファイルとプロトコルから変える。
- プールは opencodex と同じ範囲を作る。
- 認証情報は平文にしない（ユーザー：「企業で使うにはそもそも平文が問題だと思う。せめてでも何らかの読めない方式は欲しい」）。
  - 既定は OS の保管庫（Windows の資格情報マネージャー、macOS のキーチェーン、Linux の Secret Service）。Codex は Rust の `keyring` 3.6 で同じことをしている（`codex-rs/config/src/types.rs:89` の `AuthCredentialsStoreMode`：file・keyring・auto・ephemeral）。
  - 保管庫が無いときは暗号化したファイル。Windows は DPAPI、macOS・Linux は起動時のパスフレーズから Argon2id で鍵を作る。鍵をファイルの隣に置く方式は取らない。
  - 平文は、本人が明示して選んだときだけ。
  - 同じユーザーで動くマルウェアはどの方式でも防ぎきれない、とユーザーに伝えた。
- 作る順：スラッシュコマンドの定義を先に決める（ユーザー：「スラッシュコマンドをもっと定義してからやるべき」）。その後、設定の保存場所 → プロバイダ・モデル → スラッシュコマンド → アカウント → プール → 見積もり（`effort-estimates.md`）。

## 実装とテスト

- 未実装。
