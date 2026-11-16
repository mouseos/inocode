# 監査 A 項目（例外処理・正しさ）の是正

- **対象と範囲：** 2026-11-16 の読み取り監査で分類 A とした不具合の修正。新しい公開機能・オプション・CLI は追加しない。性能（分類 B）と冗長（分類 C）の項目は対象外。
- **関連コード：**
  - `core/src/oauth.rs` — `pasted_callback`（A1・A6）、`Store::load` / `Store::save` / `Store::update` / `read_entries` / `write_replacing`（A2・A3・A10）、`await_callback`（A7）、`login_muse` のクライアント（A8）、`read_success_body` と `SUCCESS_BODY_MAX_BYTES`（A9）、`token_client`（A11）
  - `core/src/llm.rs` — `read_capped`・`ERROR_BODY_MAX_BYTES`・`require_success`（A9）、`Llm::credential` の `Store::load` エラー伝播（A2）
  - `core/src/sse.rs` — BOM は先頭行のみ除去、イベント上限は「収集済み data 行＋未終端バッファ」で数える（低優先度 2 件）
  - `server/src/lib.rs` — `models` を `list_models` タスクで実行（A4）、`is_connection_error` と `ACCEPT_ERROR_PAUSE`（A5）
  - `server/src/main.rs` — `Store::load` のエラー伝播（A2）
- **根拠の状態：** 項目ごとに次のとおり。

| 項目 | 内容 | 状態 |
|---|---|---|
| A1 | 貼り付けコードの前後空白・改行を除去 | 不具合修正（参照実装に一致） |
| A6 | `code#state` の state を発行 state と照合 | 不具合修正（参照実装に一致） |
| A2 | 読めない・壊れた `auth.json` を空扱いせずエラーにする | 不具合修正 |
| A3 | 保存直前にファイルを読み直し、自分のエントリだけ置き換える | UNSUPPORTED_APPROVED |
| A10 | 一時ファイル（unix は作成時 0600）へ書いて rename で置換 | UNSUPPORTED_APPROVED |
| A4 | `models` を読み取りループから分離（`chat` と同じ構造） | 不具合修正（既存構造の再利用） |
| A5 | 接続単位の accept エラーは無視、それ以外は 1 秒待って継続 | UNSUPPORTED_APPROVED |
| A7 | コールバック接続を 1 本ずつ別タスクで処理し、終了時に全タスクを停止 | 不具合修正（参照実装に一致） |
| A8 | Muse ログインの各 HTTP 要求に 20 秒の上限 | UNSUPPORTED_APPROVED |
| A9 | エラー本文 16 KiB、ログイン系の成功本文 64 KiB で読み取りを打ち切る | UNSUPPORTED_APPROVED |
| A11 | トークン用クライアントの構築失敗をエラーにする（既定クライアントへの黙った切り替えをやめる） | 不具合修正 |
| 低 | SSE の BOM 除去を先頭のみに、イベントサイズ計数の修正 | 不具合修正（仕様に一致） |

- **学術的な主張・不足点：** 適用可能な学術研究を確認できていない。タイムアウト値・本文上限・accept エラー時の待機・認証ストアの書き込み方式の有効性を直接評価した研究は調べていない（調査未実施）。
- **学術資料：** なし。
- **その他の根拠（研究とは区別する）：**
  - A1・A6：opencodex `src/oauth/callback-server.ts`（`parseCallbackInput` が `input.trim()` する。`#waitForCallback` の手動入力は「A raw paste with an explicit code#state suffix is state-bearing too」として state 不一致を拒否し、state の無い生コードだけ発行 state を使う）。
  - A7：opencodex `src/oauth/callback-server.ts` は Bun のサーバで待ち受け、接続を並行に処理する。
  - A8：opencodex-muse `src/oauth/meta-muse-device.ts` の `REQUEST_TIMEOUT_MS = 20_000`。
  - A9：opencodex `src/oauth/chatgpt.ts` の `CHATGPT_ERROR_BODY_MAX_BYTES = 16 * 1024`、`CHATGPT_SUCCESS_BODY_MAX_BYTES = 64 * 1024`（エラー本文が上限超過なら本文を使わず、成功本文が超過なら失敗）。参照はこの値を ChatGPT のトークン要求に使う。InoCode では Anthropic・Muse のログイン要求とプロバイダのエラー本文にも同じ値を適用しており、その適用範囲の拡大は参照にない判断。
  - A5：axum 0.8.8 `src/serve/listener.rs` の `handle_accept_error` / `is_connection_error`（`ConnectionRefused`・`ConnectionAborted`・`ConnectionReset` は無視、それ以外は 1 秒待機。注記は hyper 0.14 `src/server/tcp.rs` の EMFILE 対策を引用）。
  - SSE：HTML Living Standard の Server-sent events（BOM はストリーム先頭の 1 つだけ無視する）。
  - A3・A10：一時ファイルと rename による置換は参照実装と照合していない。読み直しから rename までの競合は、後述の D2 のロックで防ぐ。
- **ユーザーの承認：** 監査報告で「A4・A7・A8・A9 はタイムアウトや上限を足す新しい動作で承認が必要」「A3 は設計判断」と示したうえで、ユーザーが 2026-11-16 に「A を全部修正」と指示した。UNSUPPORTED_APPROVED の各項目はこの指示を承認として扱う。A7 は最終的にタイムアウトを足さず並行処理で解決したため、新しい上限は導入していない。
- **実装・検証の状況：**
  - 回帰テスト（修正前のコードで失敗、修正後に成功することを確認したもの）：`server/tests/oauth_flows.rs` の `slow_discovery_does_not_block_the_connection`（A4）、`an_idle_connection_does_not_hold_up_the_callback`（A7）、`core/src/sse.rs` の `keeps_a_byte_order_mark_after_the_first_line`・`heartbeat_comments_do_not_count_toward_the_event_limit`。
  - 追加・更新したテスト（修正前コードでの失敗は未確認）：`pasted_code_is_trimmed_and_must_echo_the_login_state`、`claude_code_mode_exchanges_the_pasted_code`（改行付き・発行 state 付きの入力に変更）、`unreadable_store_is_an_error_and_is_never_overwritten`、`saving_keeps_entries_written_after_this_store_was_loaded`、`oversized_bodies_are_not_read_whole`。
  - 未検証：A5（accept エラーを発生させるテストなし）、A8（20 秒待つテストなし）、実プロバイダでの動作。unix の 0600 は 2026-11-16 の CI（run 43789768742）で ubuntu・macOS ともに確認した。

## 監査の残課題 D1〜D3

- **対象と範囲：** A の修正後に残した 3 件。D1 `models` 要求の全体上限、D2 `auth.json` の書き換えの排他、D3 同時リフレッシュの競合。D3 の調査中に見つけた不具合（リフレッシュ直後に返す資格情報に `account_id` などが無く、GPT OAuth の要求が「no account id」で失敗する）も同時に直した。
- **関連コード：** `core/src/llm.rs` の `MODELS_TIMEOUT` と `Llm::models`（D1）、`Llm::credential`（D3）。`core/src/oauth.rs` の `Store::update` と `lock_file` / `open_lock_file` / `sibling`（D2）、`refresh_stored` / `RefreshError` / `is_fresh` / `REFRESH_LOCK_WAIT`（D3）。
- **根拠の状態：** D1・D2・D3 とも UNSUPPORTED_APPROVED。`account_id` の欠落は不具合修正。
- **学術的な主張・不足点：** 適用可能な学術研究を確認できていない（調査未実施）。
- **その他の根拠（研究とは区別する）：**
  - D1：opencodex `src/adapters/qoder/live-models.ts` の `QODER_MODEL_DISCOVERY_TIMEOUT_MS = 30_000`。同じ参照には 8 秒の例（`src/providers/google-vertex-model-discovery.ts`、`src/cli/opencode.ts`）もあり、遅い接続先で誤って失敗しにくい 30 秒を選んだのはこのプロジェクトの判断。reqwest の要求単位の `timeout` で、応答本文の読み取りまで含めて打ち切る。
  - D2：opencodex `src/oauth/store.ts` は store の書き換えを `auth.store.lock` で排他する（`mutateStore`）。参照は排他的作成（O_EXCL）と古いロックの判定で実装しているが、InoCode は OS のファイルロック（Rust 標準の `File::lock`、Windows は `LockFileEx`、unix は `flock`）を `auth.json.lock` に取る。ロックはファイルを閉じるかプロセスが終わると解放されるため、古いロックの判定は持たない。
  - D3：opencodex `src/oauth/store.ts` は provider とアカウントごとのリフレッシュ用ロックを取り、待ち時間の上限は `OAUTH_REFRESH_LOCK_WAIT_MS = 60_000`。InoCode は provider ごとの `auth.json.refresh.<provider>.lock` を `try_lock` と 100 ms 間隔の再試行で最大 60 秒待ち（非同期スレッドを塞がない）、取得後に store を読み直して、別の要求が更新済みならその資格情報を使う。参照のアカウント単位の区別と、リフレッシュ意図の記録（intent ファイル）は持たない（InoCode は provider ごとに 1 アカウント）。
- **ユーザーの承認：** 監査報告で D1・D2 は新しい動作として承認が必要と示し、D3 は調査のみとしていたところ、ユーザーが 2026-11-16 に「許可も出すので実装」と指示した。
- **実装・検証の状況：**
  - `concurrent_saves_from_separate_stores_keep_every_entry`（8 スレッドが別々の `Store` から各 25 回保存し、全エントリが最終値で残る）と `concurrent_refreshes_spend_the_refresh_token_once`（期限切れの資格情報に 4 つの要求が同時に来ても、リフレッシュは 1 回で、全員が新しいトークンと保存済みの `account_id` を受け取る）を追加。2 つのロックを一時的に外すと両テストとも 3 回中 3 回失敗し、戻すと成功することを確認した。
  - D1：上限を一時的に 1 秒にし、3 秒後に応答する模擬プロバイダで `models` が 1.00 秒で失敗することを確認して元に戻した（30 秒を待つテストは追加していない）。
  - 未検証：複数プロセスの間での排他（テストは 1 プロセス内の複数スレッド・タスク）、実プロバイダでの期限切れからのリフレッシュ。
