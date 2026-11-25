# 翻訳（共通部品と、思考の翻訳）

- **対象と範囲：** 文章を別の言語に訳す共通部品。思考に限らず、ほかの場所からも使う。今回つなぐのは、モデルの思考を人の言語に訳して出すところだけ。
  - 計画：Notion「軽量翻訳エンジンと将来の多言語対応」（2026-11-25 版）。オフライン方式（Firefox・Chromium のモデル）とオンライン方式（Google・Edge のページ翻訳）の両方。
- **関連ソースコード：** `core/src/translate.rs`（共通部品）、`core/src/translate/firefox.rs`（Firefox のモデルの取得・保存・訳）、`core/src/translate/marian.rs`（モデルとショートリストの読み込み、推論）、`core/src/translate/sentencepiece.rs`（文の分割と復元）、`core/src/turn.rs`（思考へのつなぎ）、`core/src/lib.rs`（`thinking_translation` イベント、`Core::set_translation`）、`server/src/lib.rs`（`settings/translation`）。
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

### 訳す先と同じ言語なら送らない（`script_language`）

- ユーザーの承認（2026-11-25）：「訳す先と同じ言語なら訳さない」判別を端末内で作る提案に「はい」。学術的な裏付けは調べていない。
- 決め方：文字の種類だけで決める。かながあれば日本語、ハングルなら韓国語、ほかの漢字なら簡体字中国語。英字がそれらの 3 倍より多いか、それらが無ければ「分からない」。英単語 1 語は数文字の英字になり、日本語や中国語の 1〜2 文字にあたるので 3 倍で比べる（inocode の判断。例「まず src/main.rs を確認する。」は英字 9、日本語 7 文字で日本語になる）。
- 判別した言語と訳す先の言語（`ja-JP` なら `ja` の部分）が同じ塊は、どの方式でも送らずにそのまま返す。分からないときは訳す。ラテン文字の言語どうし（英語とドイツ語など）は区別できないので、いつも訳す。`zh-Hans` と `zh-Hant` も区別しない。
- 判別ライブラリを使わなかった理由（2026-11-25、思考に出そうな 15 文で比べた。手元の試しで、精度の測定ではない）：
  - `whatlang` 0.16：当たり 9 文。英語の短文をフランス語・ハンガリー語・ドイツ語と、「まず src/main.rs を確認する。」をフィンランド語と判定した。
  - `lingua` 1 系（候補を 6 言語に絞る）：当たり 14 文（外れは「了解。」を中国語）。ただし 10 言語分を入れた release の実行ファイルが 34MB になり、1 言語ごとに約 3.4MB 増える。計画の「軽さを優先」に合わない。
- Firefox 方式は原文の言語が要るので、同じ判別を使い、分からないときは英語とみなす。

## 設定とオンオフ

- `settings.json`（データベースの隣）の `translation`：
  - `thinking`：思考を訳すか（既定 false）。
  - `service`：`google`（既定）、`edge`、`firefox`。既定を Google にしたのは、コードを含む文の訳が自然だったため（下の実測、2 文での比較で、精度の測定ではない）。
  - `to`：訳す先の言語（例：`ja`）。無ければ `language` の設定が言語の記号（`ja-JP` など）ならその言語、そうでなければシステムの表示言語。
  - `google_key`：Google の鍵の差し替え。
- 設定はターンの始めに読む。プロトコルの `settings/translation`（例：`{"thinking": false}`）で書き換えると、次のターンから効く。
- `settings/translation` で `firefox` の思考の翻訳がオンになったときは、英語から訳す先へのモデルをその場で裏で取りに行く。初回の取得には数秒かかり、最初のターンの訳がそれを待って打ち切られないようにするため。
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

## オフライン方式：Firefox のモデル

Firefox がページ翻訳に使うモデル（Bergamot / Marian）を Mozilla の配布元から一度だけ取ってきて手元に置き、inocode の中で Rust だけで動かす。C/C++ のコードや外部プログラムは使わない。設定は `service: "firefox"`。訳す文章は外に出ない。

### 確かめた事実（2026-11-25）

- 一覧：`GET https://firefox.settings.services.mozilla.com/v1/buckets/main/collections/translations-models-v2/records` の `data`（404 件）。添付ファイルの置き場は `GET https://firefox.settings.services.mozilla.com/v1/` の `capabilities.attachments.base_url`（`https://firefox-settings-attachments.cdn.mozilla.net/`）。鍵は要らない。
- 1 件ごとに `name`、`version`、`fileType`（`model`・`lex`・`srcvocab`・`trgvocab`、両側共通なら `vocab`）、`architecture`、`sourceLanguage`、`targetLanguage`、`decompressedHash`・`decompressedSize`（展開後の sha256 と大きさ）、`filter_expression`、`attachment`（`hash`＝zstd 圧縮ファイルの sha256、`size`、`location`）。
- 当日の値の数：`filter_expression` は空 358、`env.appinfo.OS == 'Android'` 16、`env.appinfo.OS != 'Android'` 15、`env.channel == 'default' || env.channel == 'nightly'` 15。`version` は 3.0 が 325、3.1 が 64、3.0a1 が 15。`architecture` は base-memory 245、tiny 144、base 15。
- 英語との間の組だけがある（en→ja、ja→en、en→zh-Hans、zh-Hans→en、en→zh-Hant など）。それ以外の組は英語を経由する（Firefox の `PIVOT_LANGUAGE = "en"`）。
- Firefox が使う版は 3 系だけ（`TranslationsParent.sys.mjs` の `LANGUAGE_MODEL_MAJOR_VERSION_MIN = 3`・`_MAX = 3`）。https://github.com/mozilla-firefox/firefox/blob/main/toolkit/components/translations/actors/TranslationsParent.sys.mjs
- Firefox の訳の設定（`translations-engine.worker.js`）：`beam-size: 1`、`normalize: 1.0`、`word-penalty: 0`、`max-length-break: 128`、`max-length-factor: 2.0`、`skip-cost: true`。https://github.com/mozilla-firefox/firefox/blob/main/toolkit/components/translations/content/translations-engine.worker.js 。評価用の https://github.com/mozilla/firefox-translations-models/blob/main/evals/translators/bergamot.config.yml も同じ値。
- モデルファイル（browsermt/marian-dev の `src/common/binary.cpp`）：u64 版（1）、u64 個数、個数分の見出し（名前の長さ・型・形の長さ・データの長さ、各 u64）、名前、形（i32）、u64 の詰め物の長さと詰め物、データ。型は 0x4101（intgemm8）、0x404（float32）、0x101（int8、`special:model.yml` が設定）。
- intgemm8 の行列（`src/tensors/cpu/integer_common.h`）：int8 の値の後ろに f32 の量子化の倍率が 1 つ。値 ÷ 倍率が元の値。埋め込み（名前に `Wemb`）は単語ごとの行のまま、ほかの行列は転置して（出力 1 つにつき 1 行で）入っている。en→ja のファイルで、データの長さが要素数＋256 であることを確かめた。
- en→ja（base-memory 3.0）の設定：transformer、エンコーダー 6 層、デコーダー 4 層、幅 384、ヘッド 8、FFN 1536（relu）、デコーダーの自己回帰は SSRU、後処理 `dan`、埋め込みは目的側と出力層で共有（`tied-embeddings: true`）、語彙は両側 32000 で別々。ja→en（base 3.0、Android 以外向け）：幅 512、デコーダー 2 層、FFN 2048、語彙 64000 を両側で共有し、埋め込み `Wemb` も共有（`tied-embeddings-all: true`）。
- 計算の手順（`src/models/transformer.h`、`src/rnn/cells.h`、`src/graph/node_initializers.cpp`、`src/layers/generic.h`）：埋め込み×√幅＋正弦波の位置信号、各ブロックの後に「足して層正規化」（ε=1e-6）、注意のスコアは 1/√(ヘッドの幅) 倍、SSRU は f=σ(xWf+bf)、c=f·c'+(1−f)·xW、出力 relu(c)。最初の一歩の入力はゼロの埋め込み。出力は埋め込みの転置との積＋`decoder_ff_logit_out_b`。https://github.com/browsermt/marian-dev
- 探索（`src/translator/beam_search.cpp`）：`<unk>` は選ばない（`allow-unk` が偽のとき）。訳の長さが原文の長さ（終わりの記号を含む）×`max-length-factor` に達したら止める。
- ショートリスト（`src/data/shortlist.cpp`・`shortlist.h`）：先頭に u64 の magic（0xF11A48D5013417F5）、検査値、firstNum、bestNum、オフセットの数、単語の数。続いて u64 のオフセット、u32 の目的側の単語。候補は「頻出の firstNum 語＋原文の各単語に載っている語」を 8 の倍数になるまで頻出順に足したもの。検査値は先頭 16 バイトの後ろを u64 ずつ boost の hash_combine（整数の hash はそのまま）で混ぜた値で、en→ja のファイルで一致を確かめた。
- 語彙ファイル（SentencePiece の `sentencepiece_model.proto`）：en→ja・ja→en とも unigram、`byte_fallback` 有効、`</s>`=0、`<unk>`=1、正規化は `nmt_nfkc`（前もって組まれた変換表）、先頭に空白の印を足し、余分な空白を除く。`__source__` などの利用者定義の記号がある。分割は `unigram_model.cc` の Viterbi（利用者定義の記号は 0.1×(長さ−1)、未知の文字は最低点−10）、変換表は darts-clone の二重配列（`normalizer.cc`）。https://github.com/google/sentencepiece

### inocode の決め事

- 一覧から選ぶ組：`filter_expression` が空か `env.appinfo.OS != 'Android'` のものだけ（デスクトップのプログラムなので。Android 向けと nightly 向けは使わない）。版は 3.x の数字だけのもの（3.0a1 のような試験版は使わない）。同じ組で、モデルと語彙がそろっている版のうち最も新しいもの。同じ版に複数の種類（architecture）があれば、モデルの大きいほう。
- 取得：一覧と添付は 30 秒・600 秒で打ち切り、一覧は 20 MB、ファイルは一覧に載った大きさ（上限 300 MB）を超えたら止める。添付の場所は置き場と同じスキーム・ホスト・ポートに限る（`..` や絶対 URL は断る）。圧縮ファイルの sha256 と大きさ、展開後の sha256 と大きさを確かめてから、一時ファイルに書いて名前を変える。組の全ファイルがそろったら最後に `files.json` を書く。展開は Rust だけの `ruzstd`。
- 置き場：`settings.json` の隣の `translation-models/<元>-<先>/`。`files.json` があり各ファイルの大きさが合えば、ネットにはつながない。読み込むときは大きさだけ確かめ、ハッシュは取得時にだけ確かめる。
- 必要な組だけを、初めて要るときに取る。直接の組が無ければ英語を経由する（元→en、en→先）。
- 原文の言語：Firefox は言語判定器を使うが、inocode は文字の種類だけで決める。かながあれば日本語、ハングルなら韓国語、ほかの漢字なら簡体字中国語、ラテン文字のほうが多いか CJK が無ければ英語。元と先が同じなら訳さない。訳す先の `zh`・`zh-CN` は zh-Hans、`zh-TW`・`zh-HK` は zh-Hant として扱う。判定の精度は確かめていない。
- 文に分けて訳す。区切りは 。！？、改行、空白の前の . ! ?（流れてくる文章の区切りと同じ）。文の間の空白はそのまま残す。128 片を超える文は 128 片ずつ訳してつなぐ（`max-length-break`）。
- 探索は Firefox と同じ 1 本（貪欲）。ショートリストの使い方・`<unk>` を選ばないこと・長さの上限は Marian と同じ。
- 計算：重みは int8 のまま持ち、使うときに倍率で戻して f32 で計算する。Bergamot は入力側も int8 にする（`*_QuantMultA` を使う）が、inocode は使わない。結果が Bergamot と一字一句同じになるとは限らない（比べていない）。
- コードは Edge と同じく、コードの前後の文を別々に訳してコードを挟み直す（モデルは印を守れない）。
- 読み込んだモデルは `Translator` の中に持ち、同じ `Translator`（その複製を含む）では読み直さない。思考の訳ではターンごとに `Translator` を作るので、ターンごとに読み直す（下の実測で約 33 ms）。
- 計算は `spawn_blocking` で行う。
- 対応するのは、設定がここに書いた形（transformer、SSRU のデコーダー、`dan`、FFN 2 層、relu か swish、埋め込みの共有）で、行列が intgemm8、語彙が unigram のモデルだけ。違う形のモデルは読み込みの時点で誤りを返す。

### 本物での確認（2026-11-25、Windows 11、release ビルド）

- `translate::firefox::tests::english_to_japanese_is_fetched_once_and_kept`（手で動かす、debug ビルド）：空のフォルダに en→ja を取得（約 36 MB）、取得と読み込みで 3.1 秒。`files.json`、`lex.50.50.enja.s2t.bin`、`model.enja.intgemm.alphas.bin`、`srcvocab.enja.spm`、`trgvocab.enja.spm` がそろった。次に一覧の送り先を応答の無い `http://127.0.0.1:9` にした別の `Firefox` で en→ja が読み込めること（ネットを使わない）、取っていない ja→en は誤りになることを確かめた。
- `translate::firefox::tests::english_is_translated_to_japanese_here`（手で動かす、送り先は `http://127.0.0.1:9` のまま、つまりネット無し）：
  - 読み込み 33 ms。重みがメモリで占める大きさ 43,722,752 バイト。プロセスの作業セットは読み込み前 14.2 MB、読み込み後 72.5 MB、ピーク 102.1 MB（読み込み中はファイルの中身と重みが一時的に両方ある）。訳した後 73.0 MB。
  - 「I should check the configuration file before running the tests.」→「テストを実行する前に、設定ファイルを確認する必要があります。」（27 ms）
  - 「The user wants a summary of the changes in this pull request.」→「ユーザーは、このプルリクエストの変更の概要を要求していました。」（34 ms）
  - 「Let me look at how the server handles requests that time out.」→「サーバーがその時間外請求をどのように処理しているかを見てみましょう。」（32 ms）。「time out」を取り違えている。
  - 「This function returns an error when the file does not exist. Then the caller retries three times.」→「この関数は、ファイルが存在しないときにエラーを返します。 その後、通報者は3回再審を発。」（58 ms）。2 文目は崩れている。
  - debug ビルドでは同じ文に 1.3〜2.8 秒かかった。
- `translate::firefox::tests::japanese_goes_to_chinese_through_english_here`（手で動かす）：ja→en（base、語彙共有）を取得して「テストを実行する前に設定ファイルを確認します。」→「Check the configuration file before running the test.」（読み込み込みで 72 ms）。英語経由で zh-Hans へ「运行测试前请检查配置文件。」（en→zh-Hans の読み込み込みで 1.2 秒）。
- `translate::tests::translates_here`（`INOCODE_TRANSLATE_SERVICE=firefox`）：「I should check `src/main.rs` before running the tests.」→「確認すべきです `src/main.rs` テストを実行する前に。」、「The user wants a & b <now>.」→「ユーザーは 1 & B <now> を欲しがい。」。コードの前後を別々に訳すため、Edge と同じく文が不自然になる。
- 模擬（ネットもモデルも使わない）：`translate::marian::tests`（モデルファイルの読み方と転置した行列の積、ショートリストの候補と検査値）、`translate::sentencepiece::tests`（分割と復元、未知の文字のバイトへの分解、二重配列の最長一致）、`translate::firefox::tests`（一覧からの選び方と英語経由、添付の場所の制限、ハッシュの確認、文の区切りと原文の言語）。

- 思考の翻訳（`thinking_is_translated_while_turned_on`、`INOCODE_TRANSLATE_SERVICE=firefox`、Claude OAuth `claude-haiku-5-5`（high）、release ビルド、モデルは試験の中で初めて取得）：思考 31 件、訳 3 件。オフ後のターンは訳 0 件。全体 6.75 秒。訳の例「9時47分の出発時刻と、同じ日に12時25分に訪れる2時38分の所要時間の到着時刻を随時対応しています。」で、意味の崩れがある。
- 同じ試験を debug ビルドで動かすと訳は 0 件だった。debug では 1 文に 1〜3 秒かかり、初回のモデル取得も重なって、答えの後に待つ 10 秒（`TRANSLATION_WAIT`）を超えたと考えている（原因を個別に測ってはいない）。

### Firefox 方式で未検証のこと

- Bergamot と同じ訳になるか（比べていない）。訳の質の測定はしていない。上の例のとおり誤訳がある。
- tiny の種類と、en→ja・ja→en・en→zh-Hans 以外の組のモデルは動かしていない。BPE の語彙、intgemm16・float32 の行列のモデルには対応していない。
- 原文の言語を文字の種類で決めるやり方の当たり外れ。英語以外のラテン文字の言語（ドイツ語など）は英語として扱われる。
- 一覧の内容が変わったとき（版 4 など）は使えなくなる。手元に置いたモデルは新しい版が出ても取り直さない。

## 未実装・未検証

- Chromium のモデルによるオフライン方式は未実装。調べた事実と、ユーザーの承認「私が責任取るので続行」（2026-11-25。Google の利用規約が Chrome の外での利用や解析を認めていない点を伝えたうえでの返答）は、実装とあわせてこの文書に書く。
- オンライン方式の速さ・精度の測定はしていない。
- 動いているターンの途中でのオンオフは効かない（次のターンから）。
