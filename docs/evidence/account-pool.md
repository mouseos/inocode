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

- プール・登録・選択は未実装。認証情報の保存方法だけ実装した（次節）。

## 認証情報の保存先（実装済み）

- **関連ソースコード：** `core/src/secrets.rs`（保存先の選択・暗号化・移行）、`core/src/oauth.rs`（`Store` がこれを使う）。
- **根拠の状態：** UNSUPPORTED_APPROVED（上の「方針」のユーザー判断による。学術研究での裏付けは調べていない）。暗号の使い方は下の公式資料・規格に合わせた。

### 何をするか

- 保存先は設定 `credentials.store` で選ぶ：`auto`（既定）・`keyring`・`encrypted-file`・`plaintext`。
  - 設定ファイルの読み込みは別の人が作っている。つなぐときは `secrets::set_mode(StoreMode::parse(値)?)` の 1 行を起動時に呼ぶ。呼ばなければ `auto`。`Store::open(path, mode)` で個別に指定もできる。
- `auto` の順番：
  1. OS の保管庫（Windows の資格情報マネージャー、macOS のキーチェーン、Linux の Secret Service）。
  2. 使えないときは暗号化したファイル。Windows は DPAPI（そのユーザーだけが開ける）。macOS・Linux はパスフレーズから Argon2id で鍵を作り、XChaCha20-Poly1305 で封じる。
  3. 平文は `plaintext` を選んだときだけ。`auto` のまま平文になることはない。
- `auto` で OS の保管庫を使うのは、既定の場所（`~/.inocode/auth.json`）の保管だけ。保管庫はユーザーに 1 つで、フォルダを消しても項目が残るため、テストなど別の場所の一時的な保管が項目を残さないようにした。別の場所で `keyring` を明示すると、項目名に場所のハッシュ（16 桁）を付けて分ける。
- パスフレーズは環境変数からは読まない。`secrets::unlock(passphrase)` で渡す（メモリーにだけ置く。`secrets::lock()` で消す）。渡していない状態で開くと「パスフレーズで封じられていて、ロックされている」というエラーになる。間違ったパスフレーズや書き換えられたファイルは「パスフレーズが違うか、ファイルが変えられた」というエラーになり、ファイルは上書きしない。
  - サーバーの起動時にパスフレーズを尋ねる部分やプロトコルの呼び出しはまだない（サーバー側は別の人の担当範囲）。
- 鍵は保存しない。ファイルには Argon2id の設定（64 MiB・3 回・4 レーン、RFC 9106 第 4 節の 2 番目の推奨）、16 バイトの乱数ソルト、24 バイトの乱数ノンスだけを書く。これらの見出し部分も暗号の認証対象（AAD）に入れたので、書き換えると復号に失敗する。メモリーやパスの数値が大きすぎる見出しは読む前に断る（1 GiB・64 回・16 レーンまで）。
- 鍵の作成は遅い（わざと）。同じ見出しの鍵はプロセスの中で 1 回だけ作り、メモリーに置いておく。
- 保存の単位は（プロバイダ、アカウント）。今の 1 種類 1 件は、アカウント名 `default`。
  - OS の保管庫：サービス名 `inocode`、項目名 `<プロバイダ>/<アカウント>`（例 `gpt-oauth/default`）。ファイル `auth.json` には、どのアカウントがあるかの一覧（秘密は含まない）だけが残る。
  - 暗号化ファイル・平文：`auth.json` の中にプロバイダ → アカウント → 認証情報の形で入る。
- 大きな値の分割：Windows の資格情報は 1 件 2,560 バイトまでなので、2,048 バイトずつ分けて `<項目名>#<世代>.<番号>` に入れ、本体の項目には世代・個数・SHA-256 を書く。新しい世代を全部書いてから本体を差し替え、古い世代を消すので、読む側が新旧を混ぜることはない（混ざったら SHA-256 が合わず、読み直す）。
- 移行：最初に開いたとき、元の平文 `auth.json`（形式の印がないもの）があれば、選んだ保存先へ全部移し、読み戻して同じか確かめてから、`auth.json` を新しい内容（一覧か暗号化ファイル）で置き換える（名前の付け替えで、途中で落ちても壊れない）。確かめに失敗したら元のファイルをそのまま残し、エラーで知らせる。保存先の設定を変えたときも同じ手順で移り、OS の保管庫から出るときは保管庫の項目を消す。
- ファイルは所有者だけが読める（Unix は 0600）。Windows はユーザープロファイルの ACL と DPAPI に任せる。
- プロセスをまたぐ排他は従来どおりファイルのロック：保存は `auth.json.lock`、トークンの更新は `auth.json.refresh.<プロバイダ>.lock`。保存先によらず同じように効く。
- Linux の Secret Service の部品は、自前の Tokio ランタイムで待つため、Tokio の中から呼ぶと止まる（`zbus` 4.4 `src/utils.rs` の `block_on`）。そのため Linux では保管庫の呼び出しを別スレッドで行う。

### 確かめた事実と出典

- `keyring` 3.6.3 の文書（https://docs.rs/keyring/3.6.3/keyring/ 、2026-11-25 に読んだ）：既定の機能は無く、保管先は機能で選ぶ。`windows-native`・`apple-native`・`linux-native-async-persistent`（keyutils と非同期 Secret Service。非同期ランタイムと暗号の選択が必要）。どれも無い OS では試験用のまね（mock）になる。保管庫によっては同じ項目への同時アクセスが失敗することがある。
- `keyring` 3.6.3 のソース `src/windows.rs`：秘密が `CRED_MAX_CREDENTIAL_BLOB_SIZE`（2,560 バイト）を超えると `TooLong` を返す。文字列で渡すと UTF-16 になるので半分しか入らない。そのため `set_secret`（バイト列）で分割して入れる。保存は `CRED_PERSIST_ENTERPRISE`（ドメイン環境では移動プロファイルと一緒に動く）。
- Microsoft の CryptProtectData の文書（https://learn.microsoft.com/en-us/windows/win32/api/dpapi/nf-dpapi-cryptprotectdata 、2026-11-25 に読んだ）：通常は同じログオン資格情報のユーザーだけが、同じコンピューターで復号できる（移動プロファイルなら別のコンピューターでも）。改ざん検出の MAC が付く。追加エントロピーは復号時にも同じものが要る。出力は `LocalFree` で解放する。`CRYPTPROTECT_UI_FORBIDDEN` は画面を出さない。
- Codex（`D:\work\software\OSS\codex\codex-rs`）：
  - `keyring-store/Cargo.toml`：Linux は `linux-native-async-persistent`、macOS は `apple-native`、Windows は `windows-native`、`crypto-rust`。
  - `keyring-store/src/lib.rs`：`NoEntry` を「無い」として扱い、それ以外はエラー。分割はしていない。
  - `login/src/auth/storage.rs`：項目名は Codex のホームフォルダのハッシュ（`cli|` + 16 桁）。`auto` は保管庫に失敗すると平文ファイルへ戻る（inocode は平文へは戻らない）。保管庫に書けたら平文ファイルを消す。
  - `secrets/src/local.rs`：別方式では age で暗号化したファイルを使い、そのパスフレーズを保管庫に置く。
- 追加した依存（すべて純 Rust か OS の API の呼び出し）：
  - `keyring` 3.6.3：Windows は `windows-sys`（OS API）、macOS は `security-framework`（Security.framework の呼び出し）、Linux は `linux-keyutils`（カーネルのシステムコール）と `secret-service` 4 / `zbus` 4（純 Rust の D-Bus）、暗号は RustCrypto（`crypto-rust`）。
  - `argon2` 0.5.3、`chacha20poly1305` 0.10.1、`zeroize` 1（RustCrypto、純 Rust）。
  - `windows-sys` に機能 `Win32_Security_Cryptography` を追加（DPAPI）。
  - `Cargo.lock` に `dbus`・`libdbus-sys`（C の libdbus）が載るが、`keyring` の `dbus-secret-service?/...` という弱い機能指定を Cargo が記録するためで、ビルドには入らない。`cargo tree -p inocode-core --target all -i libdbus-sys -e normal` が何も出さないことで確かめた。

### プロジェクト内の実測（2026-11-25、Windows 11）

- `cargo test -p inocode-core --lib secrets::tests::round_trips_a_dummy_secret_in_the_os_store_here -- --ignored`：6,000 バイトの仮の値を本物の資格情報マネージャーに 3 つに分けて入れ、読み戻し、消せた。あとで `cmdkey /list` に `inocode` の項目が残っていないことを確かめた。
- 単体テスト：パスフレーズでの往復、間違ったパスフレーズ・ロック中（ファイルは変わらない）、暗号文・ノンス・鍵作成の設定の書き換え、DPAPI の往復と書き換え（Windows）、分割と古い部分の削除・欠けた部分の検出、平文からの移行（保管庫へ・保管庫が使えないときは暗号化ファイルへ）、読み戻しに失敗したときに元ファイルが残ること、`keyring` を明示したときは平文へ戻らないこと、既定の場所以外では `auto` が保管庫に触らないこと、平文から `auto` への移行。テストはすべて一時フォルダとメモリー上のまねの保管庫で行い、ホームフォルダには触れない。

### 限界

- 同じユーザーとして動くマルウェアは、どの方式でも読める（保管庫も DPAPI も、そのユーザーなら開ける。パスフレーズ方式でも、開いている間はメモリーにある）。
- Linux のデスクトップ無しの環境（Secret Service が無い）や macOS・Linux で暗号化ファイルを使うときは、パスフレーズを渡すまで認証情報を使えない。
- 元の平文ファイルは名前の付け替えで置き換えるが、ディスク上の古い領域を消すわけではない。
- 認証情報は使っている間、プロセスのメモリーに平文の文字列としてある（`Credential` のフィールドは消去しない）。
- `core` の単体テストは、Windows 以外では既定を `plaintext` にしている（パスフレーズを待たないため）。`server/tests` などの結合テストは `auto` のままで、Windows では DPAPI のファイルになるが、macOS・Linux ではパスフレーズが無いため保存に失敗する。これらのテストに `secrets::set_mode(StoreMode::Plaintext)` を加える必要がある（その部分は別の人の担当範囲なので変えていない）。

### 未確認

- macOS のキーチェーンと Linux の Secret Service での実際の動作（この機械は Windows だけ）。Linux 用のコードはこの機械ではコンパイルもしていない。
- Linux で Tokio の中から呼んだときに別スレッドにしたことで止まらないこと（ソースを読んだだけ）。
- Argon2id の設定での実際の所要時間。
