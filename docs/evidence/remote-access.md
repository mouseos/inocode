# リモート操作用の WebSocket の受け口

- **対象と範囲：** ほかの機械のネイティブアプリから `inocode-server` を操作するための受け口。設定で本人がオンにしたときだけ、すべての住所（0.0.0.0）の別のポートで WebSocket の接続を受け、127.0.0.1 の TCP と同じ要求・イベント・取り消し・大きさの上限で応える。パスワードは任意で、設定すると最初の要求で確かめる。範囲外：TLS（暗号化）、利用者ごとの権限の区別、試行回数の制限、ウェブ版の画面、ブラウザからの接続（`Origin` 付きは断る）。
- **関連コード：** `core/src/settings.rs`（`Server` の `remote`・`remote_port`・`remote_password_hash`、`DEFAULT_REMOTE_PORT`、`remote_address`、`check`、`set_sections`）、`core/src/remote_password.rs`（PBKDF2 の計算・照合）、`core/src/lib.rs`（`Started`、`sections_view`）、`server/src/lib.rs`（`run_remote`、`serve_remote`、`refuse_origin`、`Admission`、`Source`・`Sink`、`serve_connection`、`authenticate`）、`server/src/main.rs`（`serve`）、`server/tests/remote_ws.rs`、`server/tests/server_start.rs`、`server/Cargo.toml`、`core/Cargo.toml`。
- **根拠の状態：** UNSUPPORTED_APPROVED

## ユーザーの承認（2026-11-26、原文のまま）

- 依頼：「あとリモート操作用APIは作れる？パスワード設定もできるように。デフォルトはtailscaleなどでつなぐので不要と考えている。一応リモートはユーザーがオンにしないと使えないように」
- パスワードが無いとき：「認証なしで通す」（起動時に警告を出す）
- 受ける住所：「すべての住所」（0.0.0.0）
- 形式：「形式は要相談。ウェブ版作る予定は今のところないが将来作るのに適した形がいい。リモートはネイティブアプリから操作」、その後「WebSocket にする」

## 学術的な主張・不足点

- 適用可能な学術研究は確認できていない。リモート操作の受け口を持つこと、WebSocket を選ぶこと、パスワードを任意にすることは、ユーザーの判断による。研究で裏付けられた機能ではない。
- パスワードの保存方式（塩を付けた遅いハッシュ）は、公式の規格と指針に従った（下記）。これは正しく作るための根拠で、機能の有効性の研究ではない。

## その他の根拠（公式の規格・指針）

- PBKDF2：RFC 8018（PKCS #5 v2.1）5.2 節。<https://www.rfc-editor.org/rfc/rfc8018#section-5.2>。出力は 32 バイト（SHA-256 の 1 ブロック分）なので、ブロックは 1 つだけ計算する。
- 繰り返しの回数 600,000：OWASP Password Storage Cheat Sheet が PBKDF2-HMAC-SHA256 に示す値。<https://cheatsheetseries.owasp.org/cheatsheets/Password_Storage_Cheat_Sheet.html>
- 計算が正しいことは、公開されている PBKDF2-HMAC-SHA256 の試験値（P="password"、S="salt"、c=1 と c=2）で確かめた（`core/src/remote_password.rs` の単体テスト。値は Python の `hashlib.pbkdf2_hmac` でも同じになることを確認）。
- WebSocket：RFC 6455。`Origin` ヘッダーはブラウザが付けるもので、サーバーはそれでブラウザからの接続を見分けられる（10.2 節）。<https://www.rfc-editor.org/rfc/rfc6455#section-10.2>

## ほかのハーネスのやり方（2026-11-26 に各ソースで確認済みとして依頼者から渡された内容）

| ハーネス | 受け口 | 認証 | パスワード無し・その他 |
|---|---|---|---|
| Codex app-server（ws の受け口） | 自分で WebSocket を受ける | 能力トークン、または署名付き bearer | 認証なしでループバック以外を受けることを拒む。`Origin` 付きの要求は 403 で断る。TLS なし |
| Codex・Claude Code のリモート操作 | 受け口を開けない（外から入るポートなし） | 提供元のアカウントでログインし、提供元のサーバーを中継する | — |
| opencode v1 | HTTP サーバー | `OPENCODE_SERVER_PASSWORD` による Basic 認証 | 未設定なら警告するだけで通す |
| opencode main | HTTP サーバー | 常にパスワードを求める | 未設定なら生成する |
| OpenHands | HTTP サーバー | `SESSION_API_KEY` | ワイルドカードの住所で受けるときだけ警告する |
| Gemini CLI a2a-server | localhost だけ | — | 外からは受けない |

inocode は、パスワード無しで通す点は opencode v1 に、`Origin` を 403 で断る点と TLS を持たない点は Codex app-server に近い。どちらもユーザーの判断（「認証なしで通す」「すべての住所」）と依頼の指示による。

## 設定

`settings.json` の `server` の中。`settings/get` で見え、`settings/set` で変えられる。どれも**サーバーの起動時に効く**（`server.port` と同じ）。起動後に変えた値は、`settings/get` の `restart_required` が `true` になり、`restart_for` に `remote`・`remote_port`・`remote_password` のうち変わったものが入る。いま効いている値は `effective.remote`・`effective.remote_port`・`effective.remote_password_set` に出る。

| 名前 | 既定 | 意味 |
|---|---|---|
| `server.remote` | 無し（オフ） | `true` のときだけリモート操作の受け口を開く |
| `server.remote_port` | `8788`（`DEFAULT_REMOTE_PORT`＝ループバックの既定 8787 の次） | 受け口のポート。1〜65535。ループバックのポート（`server.port`、無ければ 8787）と同じ値は断る。`--port` で同じ値にしたときは起動を止める |
| `server.remote_password` | 無し | **書くだけ**。`settings/set` に平文で渡す。`null` か `""` で消す。1024 バイトまで。ファイルには平文を書かない |
| `server.remote_password_hash` | 無し | ファイルにだけある。`pbkdf2-sha256$<回数>$<塩>$<ハッシュ>`（塩は 16 バイトの乱数、塩とハッシュは base64 のパディングなし）。`settings/set` で直接は書けない（断る）。`settings/get` には出さず、代わりに `server.remote_password_set`（`true`/`false`）を出す |

`remote` が無い・`false` のときは、受け口を開かない（`Server::remote_address` が `None` を返し、`serve` は何も bind しない）。

## 通信の形

- 1 つの WebSocket のテキストメッセージに、TCP のフレーム 1 つ分を入れる：`{"stream_id": n, "message": {...}}`。`stream_id` は TCP のフレームの見出しのストリーム番号、`message` は TCP のフレームの中身の JSON そのもの。サーバーからも同じ形で返す。ほかのキーは付けない。
- TCP と同じ処理（`serve_connection` と `dispatch`）を通すので、使える要求・イベント（`started`、`delta`、`completed` など）・`cancel`・送り待ちの上限は TCP と同じ。
- 形が違うメッセージ（JSON でない、`stream_id` か `message` が無い、余分なキーがある）は、ストリーム番号が分からないのでストリーム 0 に誤り（`invalid_json` か `invalid_request`）を返し、接続は続ける。バイナリのメッセージは、TCP の JSON でないフレームと同じく `invalid_request`（ストリーム 0）。
- 大きさ：`message` の JSON は TCP のフレームの上限（16 MiB）まで。受けるメッセージは、包みの分として 256 バイトを足した大きさまで。超えたら `frame_too_large` を返して閉じる（TCP と同じ）。返す側で上限を超えたら、TCP と同じく代わりの誤りを送る。
- ping/pong は WebSocket の層が応える。

## 認証の流れ

- **パスワードあり：** 最初の要求は `{"id": n, "method": "auth", "params": {"password": "<パスワード>"}}` でなければならない。合えば `{"id": n, "result": {"authenticated": true}}`。違う・無い・別の要求・JSON でない・10 秒以内に来ない場合は、`unauthorized` を返して（返せるとき）接続を閉じる。TCP の合言葉と同じ扱いで、TCP にも試行回数の制限や待ち時間の延長は無いので、ここにも付けていない。照合は PBKDF2 を計算して、どこで食い違ったかで時間が変わらない比較で行う。計算は実行時のスレッドを止めないよう別のスレッドで行う。
- **パスワードなし：** すべての接続を、最初のメッセージから認証済みとして扱う。`auth` を送る必要はなく、送ると、サーバーに無い要求と同じく誤りが返る。起動時に標準エラーへ「パスワード無しでリモート操作がオンで、このポートに届く人は誰でも操作できる」という警告を出す。
- **ローカルの合言葉（`~/.inocode/server-token`）：** リモートの受け口では確かめない。パスワードありのとき `{"token": ...}` だけの `auth` は `unauthorized` で断られる（試験で確認）。パスワードなしのときは、何も確かめないので合言葉も関係しない。
- 起動時に標準出力へ、受け口の住所（`remote access listening on 0.0.0.0:<port> (WebSocket)`）と、パスワードの有無を出す。

## `Origin` ヘッダーを断ること

- WebSocket のアップグレード要求に `Origin` ヘッダーがあれば、HTTP 403 で断る（`refuse_origin`）。
- 理由：0.0.0.0 の受け口はループバック（127.0.0.1）でも応える。ブラウザはウェブページからの WebSocket に必ず `Origin` を付けるので、これを断らないと、本人のブラウザで開いたページが、パスワード無しの受け口からエージェントを操作できてしまう。ネイティブアプリは `Origin` を付けなくてよい。
- 将来ウェブ版を作るときは、許す `Origin` の一覧（allow-list）を設定に持たせる必要がある。いまは一覧を持たず、すべて断る。

## 残る危険と制約（正直に）

- **TLS が無い。** パスワードも会話も平文で流れる。Tailscale や VPN など、経路を暗号化する仕組みの上で使う前提（ユーザーの想定どおり）。インターネットへ直接開けるのは危険。
- **パスワード無しのときは、ポートに届く人をすべて信じる。** 同じ LAN の機械や、ポートを転送した先からも操作できる。起動時の警告だけが知らせる。
- パスワードの試行回数は制限しない。1 回の照合に PBKDF2 の計算がかかるので、総当たりは遅くなるが、大量の試行はサーバーの CPU を使わせる。
- 受け口を開くと、ループバックの TCP と同じく、すべての要求（設定の変更、`full` モードのターンなど）が使える。利用者ごとの権限は無い。
- 設定はサーバーの起動時にだけ読む。パスワードを変えても、再起動までは前のパスワードのまま。
- `hmac` クレート（0.12）を `inocode-core` の直接の依存に加えた。もともと依存の木にあった（`Cargo.lock` に同じ版がある）ので、新しいクレートは増えていない。HMAC を自前で書くより、既存の検証済みの実装を使う方が安全なため。サーバー側では、依存の木にあった `tokio-tungstenite`（`handshake` の機能だけ）と `futures-util` を直接の依存にした。

## 実装・検証の状況

- `core/src/remote_password.rs` の単体テスト：公開の試験値と一致する。ハッシュは自分のパスワードだけに合い、毎回塩が変わる。壊れた値は断られ、どのパスワードにも合わない。
- `core/src/settings.rs` の単体テスト `remote_access_is_off_until_turned_on_and_its_password_is_kept_as_a_hash`：既定でオフ、ポートだけではオフのまま、オンで 0.0.0.0、既定のポート 8788。0・ループバックと同じポート・型の違い・長すぎるパスワード・ハッシュの直接の設定は断られる。ファイルには平文が残らずハッシュだけが残る。`""` と `null` で消える。
- `server/tests/remote_ws.rs`：WebSocket で `ping`・`echo`、形の違うメッセージとバイナリはストリーム 0 の誤りで接続は続く、パスワード無しで `auth` 無しに使える。チャットのイベント（`started`→`delta`→`completed`）と `cancel`。パスワードありで、要求の先送り・パスワード無し・違うパスワード・合言葉だけ（2 通り）は `unauthorized` で閉じられ、正しいパスワードで使える。`Origin` 付きは 403、無ければつながる。`settings/set`・`settings/get` の答えにパスワード・ハッシュ・`pbkdf2`・`remote_password_hash` が出ず、`remote_password_set` だけが出る。
- `server/tests/server_start.rs` の `remote_access_listens_only_when_turned_on_and_warns_without_a_password`：実際のサーバーを起動し、`remote` が無ければ `remote_port` に何も応えず、`true` なら WebSocket で `ping` に応え、標準エラーに警告が出る。
- 実際にほかの機械から Tailscale 越しにつないだ試験はしていない（未検証）。
