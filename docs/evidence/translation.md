# 翻訳（共通部品と、思考の翻訳）

- **対象と範囲：** 文章を別の言語に訳す共通部品。思考に限らず、ほかの場所からも使う。今回つなぐのは、モデルの思考を人の言語に訳して出すところだけ。
  - 計画：Notion「軽量翻訳エンジンと将来の多言語対応」（2026-11-25 版）。オフライン方式（Firefox・Chromium のモデル）とオンライン方式（Google・Edge のページ翻訳）の両方。
- **関連ソースコード：** `core/src/translate.rs`（共通部品）、`core/src/turn.rs`（思考へのつなぎ）、`core/src/lib.rs`（`thinking_translation` イベント、`Core::set_translation`）、`server/src/lib.rs`（`settings/translation`）。
- **根拠の状態：** UNSUPPORTED_APPROVED。ユーザーの指示（2026-11-25）：「推論過程などげんごがゆーざーしようのものとちがうと読みにくい。……推論過程の翻訳を実装して。オンオフはすぐできるように。他の場でもこの機能は使用予定なのでthinking翻訳限定でコードを組まないこと」。訳して読みやすくなるという学術的な裏付けは調べていない。

## オンライン方式（確かめた事実）

どちらも、ブラウザがページ翻訳で使っている送り先。公開された API ではない。訳す文章をその会社に送る。

### Edge

- 送り先：`POST https://edge.microsoft.com/translate/translatetext?from=&to=ja&isEnterpriseClient=false`。`from` が空なら言語を自動で判定する。
- 本文：文字列の JSON 配列。答えは入力と同じ順の配列で、各要素の `translations[0].text` が訳、`detectedLanguage.language` が判定された言語。
- 鍵は要らない。2026-11-25 に curl で 2 文を送り、200 と訳が返った。
- `User-Agent` が無いと 400「Client Browser Version not supported」が返る。値は何でもよい（`inocode/0.1`、curl のもの、Edge のもので確かめた）。inocode は `inocode/<版>` を送る。
- `textType=html` にしても、`class="notranslate"`・`translate="no"`・`<code>` の中身を訳したり、タグを文字として返したりした（2026-11-25、4 通りで確かめた）。コードを守る印として使えない。
- 平文のまま `` ` `` で囲んだコードを送ると、「」に変わったり `&&` が `&` になったりした。

### Google

- 送り先：`POST https://translate-pa.googleapis.com/v1/translateHtml`。`Content-Type: application/json+protobuf`、`X-Goog-API-Key: <鍵>`。
- 本文：`[[[文, 文, …], "auto", "ja"], "te_lib"]`。答え：`[[訳, 訳, …], [判定した言語, …]]`。文は HTML として扱われるので、`&`、`<`、`>` は送る前に実体参照に直し、訳から元に戻す。
- 鍵：`AIzaSyATBXajvzQLTDHEQbcpq0Ihe0vWDHmO520`。Google が公開しているページ翻訳スクリプトの中にある。`https://translate.googleapis.com/translate_a/element.js?client=te_lib` が読み込む `…/translate_http/_/js/k=translate_http.tr.ja…/m=el_main`（2026-11-25 取得）に入っていることを確かめた。秘密の鍵ではない。ユーザーもこの鍵を指定した。`settings.json` の `translation.google_key` で差し替えられる。
- 2026-11-25 に curl で 2 文を送り、200 と訳、判定した言語（`en`）が返った。
- `<span class=notranslate>…</span>` の中は訳さずに返す（中に英文を入れて確かめた）。属性を引用符で囲むと 502 が返る（`class="notranslate"`、2 回）。囲まなければ通る。

## 共通部品の決め事（inocode の判断）

- コードは訳さない。`` ``` `` で囲まれた部分と `` ` `` で囲まれた部分をコードとみなす。
  - Google：文全体を送り、コードは `<span class=notranslate>` で包む。返ってきた印の中身からコードを戻す。印の数が合わないときは、その文だけ下の Edge と同じやり方で訳し直す。
  - Edge：印を守らないので、コードの前後の文をそれぞれ別に送り、コードはそのまま挟み直す。前後の空白も残す。文が途中で切れるため、Google より訳が不自然になる（下の実測）。
- 流れてくる文章は、文の終わり（。！？. ! ? と改行）でまとめてから訳す。区切りがないまま 400 文字を超えたら、空白の位置で切る。コードの囲みが閉じていない間は切らない。
- 訳は 1 つの作業で順に行う。1 回の訳を待つ間に溜まった塊は、次の 1 回にまとめて送る。こうすると順番が保たれ、送る回数も減る。
- 訳せなかったときは、原文をそのまま出し、誤りを添える。
- 原文は変えない。保存されるのは原文の思考で、訳は表示用に流すだけで保存しない。

## 設定とオンオフ

- `settings.json`（データベースの隣）の `translation`：
  - `thinking`：思考を訳すか（既定 false）。
  - `service`：`google`（既定）か `edge`。既定を Google にしたのは、コードを含む文の訳が自然だったため（下の実測、2 文での比較で、精度の測定ではない）。
  - `to`：訳す先の言語（例：`ja`）。無ければ `language` の設定が言語の記号（`ja-JP` など）ならその言語、そうでなければシステムの表示言語。
  - `google_key`：Google の鍵の差し替え。
- 設定はターンの始めに読む。プロトコルの `settings/translation`（例：`{"thinking": false}`）で書き換えると、次のターンから効く。
- 訳はプロトコルのイベント `thinking_translation`（`text`、訳せなかったときは `error`）で届く。原文の `thinking` イベントもそのまま届く。

## テスト

- 模擬：`translate::tests` の `code_is_kept_apart_from_the_text`、`marked_code_comes_back_as_written`、`streamed_text_is_cut_at_sentence_ends_outside_code`、`settings_are_read_and_checked`、`the_target_comes_from_the_setting_then_the_reply_language`、`an_update_keeps_the_rest_of_the_file`、`a_stream_is_translated_in_order_with_code_kept`（手元の代わりのサーバーで、順番・まとめ送り・失敗時の原文）。

## 本物での確認（2026-11-25）

- `translate::tests::translates_here`（手で動かす）：
  - Google：「I should check `src/main.rs` before running the tests.」→「テストを実行する前に、 `src/main.rs`を確認する必要がある。」、「The user wants a & b <now>.」→「ユーザーはaとbを<今すぐ>求めています。」
  - Edge：同じ 2 文 →「確認してみるよ `src/main.rs` テストを実行する前に。」、「ユーザーは a と b を望み<now>ます。」
- `server/tests/live_tools.rs` の `thinking_is_translated_while_turned_on`（手で動かす。本物のサーバーで `settings/translation` でオンにし、Claude OAuth `claude-haiku-5-5`（high）の思考の訳が `thinking_translation` で届くこと、オフにした次のターンでは届かないこと）：
  - Google：思考 14 件、訳 2 件。訳の例「9:47と2:38を足すと12:25になるので、合計は簡単です。」。オフ後のターンは訳 0 件。
  - Edge：思考 47 件、訳 5 件。オフ後のターンは訳 0 件。

## 未実装・未検証

- オフライン方式（Firefox のモデル、Chromium のモデル）は未実装。
- 速さ・メモリ・精度の測定はしていない。
- 動いているターンの途中でのオンオフは効かない（次のターンから）。
