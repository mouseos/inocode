# 翻訳（共通部品と、思考の翻訳）

- **対象と範囲：** 文章を別の言語に訳す共通部品。思考に限らず、ほかの場所からも使う。今回つなぐのは、モデルの思考を人の言語に訳して出すところだけ。
  - 計画：Notion「軽量翻訳エンジンと将来の多言語対応」（2026-11-25 版）。オフライン方式（Firefox・Chromium のモデル）とオンライン方式（Google・Edge のページ翻訳）の両方。
- **関連ソースコード：** `core/src/translate.rs`（共通部品）、`core/src/translate/firefox.rs`（Firefox のモデルの取得・保存・訳）、`core/src/translate/marian.rs`（モデルとショートリストの読み込み、推論）、`core/src/translate/sentencepiece.rs`（文の分割と復元）、`core/src/translate/chromium.rs`（Chrome の言語パックの取得・確認・保存・訳）、`core/src/translate/tflite.rs`（TFLite モデルの読み込みと実行）、`core/src/turn.rs`（思考へのつなぎ）、`core/src/lib.rs`（`thinking_translation` イベント、`Core::set_translation`）、`server/src/lib.rs`（`settings/translation`）。
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
  - `service`：`google`（既定）、`edge`、`firefox`、`chromium`。既定を Google にしたのは、コードを含む文の訳が自然だったため（下の実測、2 文での比較で、精度の測定ではない）。
  - `to`：訳す先の言語（例：`ja`）。無ければ `language` の設定が言語の記号（`ja-JP` など）ならその言語、そうでなければシステムの表示言語。
  - `google_key`：Google の鍵の差し替え。
- 設定はターンの始めに読む。プロトコルの `settings/translation`（例：`{"thinking": false}`）で書き換えると、次のターンから効く。
- `settings/translation` で `firefox` か `chromium` の思考の翻訳がオンになったときは、英語から訳す先へのモデルをその場で裏で取りに行く。初回の取得には数秒かかり、最初のターンの訳がそれを待って打ち切られないようにするため。
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

## オフライン方式：Chrome の言語パック（Chromium 方式）

Chrome が組み込みの翻訳 API（Translator API）のために取ってくる言語パック（TranslateKit）を、Chrome と同じ配布元から一度だけ取ってきて手元に置き、その中のモデルを inocode の中で Rust だけで動かす。Google のネイティブライブラリ（libtranslatekit）は取得も読み込みもしない。C/C++ のコードは使わない。設定は `service: "chromium"`。訳す文章は外に出ない。

- **ユーザーの承認：** 「私が責任取るので続行」（2026-11-25）。Google の利用規約は、ソフトウェアを Google のサービスの一部としてだけ使うことを認め、法律が認める場合を除いて複製や解析（リバースエンジニアリング）を禁じている、と伝えたうえでの返答。この方式はその規約に反するおそれがある。学術的な裏付けは調べていない。状態は UNSUPPORTED_APPROVED。
- 参照した Chromium のソース：main ブランチ（2026-11-25 に取得。リビジョンは記録していない＝不明）。
  - `components/on_device_translation/public/language_pack.h`・`language_pack.cc`：https://source.chromium.org/chromium/chromium/src/+/main:components/on_device_translation/public/
  - `components/component_updater/component_updater_url_constants.cc`、`docs/updater/protocol_4.md`：https://source.chromium.org/chromium/chromium/src/+/main:docs/updater/protocol_4.md
  - `components/crx_file/`（CRX3 の形式、`id_util`）：https://source.chromium.org/chromium/chromium/src/+/main:components/crx_file/

### 確かめた事実（2026-11-25）

- 取得先：`POST https://update.googleapis.com/service/update2/json`（`component_updater_url_constants.cc`）。Omaha プロトコル 4.0。見出し `Content-Type: application/json`、`X-Goog-Update-Interactivity: fg`、`X-Goog-Update-Updater: chrome-<版>`、`X-Goog-Update-AppId: <id>`。鍵や署名付きの要求（CUP）は要らなかった。
- 本文の `acceptformat` を `crx3,download` だけにすると、答えは `"updatecheck":{"status":"error-inexpressible"}` になった。Chrome と同じ `crx3,download,puff,run,xz,zucc` なら、取得の手順（pipeline）が返る（curl で確かめた）。`os.version` と `hw` は無くても返った。
- 答えは `)]}'` で始まる JSON。アプリごとに `status`、`updatecheck.nextversion`、`updatecheck.pipelines`。手順のうち「`download`（`size`、`out.sha256`、`urls`）→ `crx3`」の 2 段のものがファイル全体の取得。ほかに差分（`puff`）や `xz` の手順がある。URL は `dl.google.com`、`edgedl.me.gvt1.com`、`www.google.com/dl` の http と https。
- 部品の id：パックの公開鍵の SHA-256 の先頭 16 バイトを 16 進で書き、0〜f を a〜p に置き換えたもの（`crx_file::id_util::GenerateIdFromHash`）。en-ja は `hnccddhebmkigcfihhnmiiihcnaogoel`、en-zh（簡体字）は `boeedobmjjddfgncfnmnehcemjbfpfbk`。
- パックは 38 個。どれも英語ともう 1 言語の組（`language_pack.h` の `LanguagePackKey`、鍵の SHA-256 は `kTranslateKit*Config.public_key_sha`）。表は `chromium.rs` の `PACKS` に写した。英語以外どうしは英語を経由する（`language_pack.cc` の「Currently we always translate via English」）。
- en-ja のパック：版 2024.9.9.1、56,490,050 バイト、CRX3（`Cr24`、版 3、見出しの長さ、見出しの protobuf、その後ろが zip）。見出しには RSA の証明が 2 つ（2048 ビットの鍵と、SHA-256 が上の値に一致する 4096 ビットの鍵）と ECDSA（P-256）の証明が 1 つ、`signed_header_data` の `crx_id` は鍵の SHA-256 の先頭 16 バイト。4096 ビットの鍵の署名（RSA PKCS#1 v1.5、SHA-256、署名する中身は `"CRX3 SignedData\0"`＋見出しの長さ（u32 LE）＋見出し＋zip）が合うことを、手で動かす試験で確かめた。
- zip の中身：`en_ja_nmt/` と `ja_en_nmt/`（両方向）、それぞれ `encoder_0.tflite`（23,278,704 バイト）、`decoder_step_0.tflite`（14,574,968）、`decoder_init_0.tflite`（13,048）、`src_spm_model.model`・`tgt_spm_model.model`（SentencePiece）、同名の `.vocab`（文字の一覧）、`translation_model.pb`（320 バイト）、`processor_spec.pb`。ほかに `en_ja_dictionary/dictionary.bin`（先頭 `BD00`、中身は不明）、`_metadata/verified_contents.json`、`manifest.json`。
- `translation_model.pb`（形式は公開されていない。en-ja での値）：1＝`en`、2＝`ja`、6.3＝{1: 2, 2: {1: 1, 2: 2, 3: 3.0, 5: 5.0, 6: 1, 7: 2, 8: -1, 9: 0.0, 10: 1.0}}、6.4＝{1: 1, 2: 4, 3: 72, 6: 1}、6.5＝モデル名とファイル名の組（`decoder_init_0`、`decoder_step_0`、`encoder_0`）、6.6・6.7＝原文側・訳側の SentencePiece のファイル名。
- `processor_spec.pb`：en→ja は手順 `b2a`、`lc`、`add_period_10` と、その設定 `PreprocAddPeriod/add_period_10:max_sent_len=10:skip_in_training=true:use_wpm_tokens=false:mode=annotate`、`PreprocB2A/b2a:mode=tokenize`、`PreprocLowercaser/lc:activate_languages`。ja→en は `b2a` と `lc` だけ。これらの名前を説明する公開資料は見つからなかった（Web 検索、2026-11-25）。
- SentencePiece：unigram、語彙 8000、`<unk>`=0、`<s>`=1、`</s>`=2、バイトへの分解あり、独自の正規化表あり。英語の原文側の語彙には大文字の片が無い（大文字を含む行はバイトの片 `<0x..>` だけ）。ja→en の訳側（英語）の語彙には `▁I`、`▁The` など大文字の片がある。
- TFLite のファイル：flatbuffer（`TFL3`）、暗号化なし、組み込みの演算だけ。3 つ合わせて使う演算は 33 種（RESHAPE、SHAPE、STRIDED_SLICE、PACK、RANGE、CAST、EXPAND_DIMS、MUL、COS、SIN、CONCATENATION、PAD、TRANSPOSE、GREATER、TILE、GATHER、DEQUANTIZE、ADD、MEAN、SUB、SQUARED_DIFFERENCE、RSQRT、FILL、FULLY_CONNECTED、FLOOR_DIV、BATCH_MATMUL、SELECT、SOFTMAX、SELECT_V2、SPLIT、LOGISTIC、TANH、LOG_SOFTMAX）。形は実行時に決まる（`shape_signature` に -1）。
- 量子化：重みは int8（テンソルごとの倍率 1 つ、ゼロ点 0）。FULLY_CONNECTED はすべて int8 の重みと float の入力の組（hybrid）で、`asymmetric_quantize_inputs` が真。埋め込みは int8 の表を GATHER してから DEQUANTIZE。
- 形と名前：エンコーダーの入力は `src_ids`（int32、[文の数, 長さ]）と `src_paddings`（float、同じ形）、出力は `src_encs`（[文の数, 長さ, 512]）。デコーダーの開始（`beam_search_init`）は `num_hyps`、`src_enc_paddings`、`src_encs` を受け、`atten_probs`、`src_encs`、`beam_state__atten_context` を返す。1 歩（`beam_search_step`）は `step_ids`（[候補の数, 1]）、`atten_init__source_vecs`・`source_contexts`・`source_padding`、`beam_state__rnn_state_{0,1}_{m,c}` と `beam_state__atten_context` を受け、`log_probs`（[候補の数, 8000]、対数確率）と次の状態を返す。名前の `MTDecoderV1` は Lingvo の `lingvo/tasks/mt/decoder.py` と同じ（エンコーダーは 6 層・幅 512 の Transformer、デコーダーは 2 層の LSTM と注意）。探索の繰り返しはモデルの外にある。

### inocode の決め事

- 要求：Chrome の版は `143.0.7499.170` に固定。`@os`・`arch`・`os.platform`・`os.arch` は動いている OS（`std::env::consts`）から決める。`hw` と `os.version` は送らない（無くても答えが返り、送る情報を減らすため）。
- 取得：答えのうち「download → crx3」の手順だけを使い、https の URL だけを順に試す（30 秒・600 秒で打ち切り、答えは 1 MB、パックは答えに載った大きさ（上限 300 MB）まで）。大きさと SHA-256 が答えと一致し、CRX3 の印と版、`crx_id`、鍵の SHA-256 が `PACKS` の値と一致し、その鍵の RSA 署名が合うときだけ使う。署名の確かめには Rust だけの `rsa` クレートを使う。2048 ビットの鍵と ECDSA の証明は確かめない。
- 展開：`<…>_nmt/` の直下にある `.tflite`・`.model`・`.pb` だけを取り出す（`.vocab`、辞書、`_metadata` などは捨てる）。ほかの階層、`..`、先頭が `.` の名前は取り出さない。zip64 と暗号化は断る。CRC-32 と大きさを確かめる。展開は `flate2`（Rust の実装）。各ファイルは一時ファイルに書いて名前を変え、全部書いてから最後に `pack.json`（版、各ファイルの大きさと SHA-256）を書く。
- 置き場：`translation-models/chromium/en-<言語>/`。`pack.json` があり各ファイルの大きさが合えば、ネットにはつながない。パックには両方向が入っているので、en→ja を取れば ja→en も使える。
- 方向の選び方：各 `_nmt` フォルダの `translation_model.pb` の 1・2 番（言語）で選ぶ。フォルダ名には頼らない。
- 計算：TFLite の演算を 1 つずつ、ファイルに書かれた順と配線のとおりに実行する汎用の解釈器を書いた（`tflite.rs`）。各演算は TFLite の参照実装の計算に合わせた。hybrid の FULLY_CONNECTED は TFLite の `EvalHybrid` と同じく、入力を行ごとに int8 に量子化し（`asymmetric_quantize_inputs` が真なら `AsymmetricQuantizeFloats`、偽なら `SymmetricQuantizeFloats` と同じ式）、int8 どうしの積を int32 で足してから倍率を掛ける。ほかは float32 で計算する。知らない演算や型を含むファイルは読み込みの時点で断る。大きな FULLY_CONNECTED（積和が約 100 万回以上）は出力を最大 8 スレッドに分ける。
- 前処理：
  - `lc`：原文を小文字にする（英語の原文側の語彙に大文字が無いため）。訳の中の英字と数字の並びが、原文に大文字を含む形で現れる語と大文字小文字を無視して同じなら、原文の書き方に戻す（例「Open the GitHub settings」→「GitHub 設定を開きます」）。Chrome が大文字をどう戻すかは分からず、これは inocode の判断。
  - `add_period_10`：空白で区切った語が 10 個以下で、終わりに句読点が無い文には、原文の末尾に「.」を足し、訳の末尾の「。」「.」「．」を 1 つ取る。設定の名前と値から推した意味で、Chrome の動きとは確かめていない。
  - `b2a`：意味が分からないので何もしない。SentencePiece の正規化表が同じことをしている可能性はあるが、確かめていない。
- 探索：ビーム幅 4（6.4 の 2 番）、長さの上限は max(72（6.4 の 3 番）, 原文の片の数×2)。72 だけだと長い文が切れるため、2 倍の部分は inocode の判断。点数は対数確率の和で、長さによる補正はしない（6.3.2 の 9 番が 0.0 で、Lingvo の `length_normalization` にあたると見た）。終わりの記号 `</s>` は、その対数確率がその歩で最も高い片より 5.0（6.3.2 の 5 番、Lingvo の `valid_eos_max_logit_delta` にあたると見た）以内のときだけ選べる。`<unk>` と `<s>` は選ばない。終わった候補の最高点が、続いている候補の最高点以上になったら止める（点数は減る一方なので、それ以上良い訳は出ない）。6.3.2 の 3 番（3.0）は Lingvo の `beam_size`（点数の幅による切り捨て）かもしれないが確かめておらず、使わない。最初の歩の入力は `<s>`（6.3.2 の 1 番）、原文の終わりに `</s>` を付ける。
- 1 文ずつ訳す（文の数＝1）。文の区切りと 128 片を超える文の分け方は Firefox 方式と同じ。原文の言語の決め方と、英語以外どうしの英語経由も Firefox 方式と同じ。訳す先の `zh`・`zh-CN` は簡体字、`zh-TW`・`zh-HK` は繁体字のパック、`he` は `iw`、`nb` は `no` のパックを使う。
- コードは Edge・Firefox 方式と同じく、コードの前後の文を別々に訳してコードを挟み直す。
- 読み込んだ方向は `Translator` の中に持ち、計算は `spawn_blocking` で行う。`settings/translation` で `chromium` の思考の翻訳がオンになったときも、Firefox 方式と同じく裏でパックを取りに行く。
- 依存を 2 つ足した：`rsa` 0.9（署名の確認）、`flate2` 1（zip の展開。すでに他のクレート経由で入っていた）。どちらも Rust の実装。

### 本物での確認（2026-11-25、Windows 11、release ビルド）

- `translate::chromium::tests::english_to_japanese_is_fetched_once_and_kept`（手で動かす）：空のフォルダに en-ja を取得し、署名の確認・展開・読み込みまで 2.05 秒。`pack.json` と、`en_ja_nmt/`・`ja_en_nmt/` のそれぞれ 7 ファイル（`.tflite` 3 つ、`.model` 2 つ、`.pb` 2 つ）がそろい、`.tmp` は残らなかった。次に更新サーバーを応答の無い `http://127.0.0.1:9` にした別の `Chromium` で en→ja と ja→en が読み込めること（ネットを使わない）、取っていない en→de は誤りになることを確かめた。
- `translate::chromium::tests::english_and_japanese_are_translated_here`（手で動かす、読み込みと訳は送り先 `http://127.0.0.1:9`、つまりネット無し）：
  - 読み込み：en→ja 19 ms、ja→en 19 ms。プロセスの作業セットは読み込み前 19.5 MB、両方向の読み込み後 101.6 MB、訳した後 103.7 MB。ピーク 152.2 MB は同じプロセスで先に行った取得のとき（CRX 全体と展開したファイルが一時的にメモリにある）。
  - 「I should check the configuration file before running the tests.」→「テストを実行する前に、構成ファイルを確認する必要があります。」（84 ms）
  - 「The user wants a summary of the changes in this pull request.」→「ユーザーは、このプル リクエストの変更点の概要を求めています。」（76 ms）
  - 「Let me look at how the server handles requests that time out.」→「サーバーがそのタイムアウトをどのように処理するかを見てみましょう。」（85 ms）。「requests」が訳から落ちている。
  - 「This function returns an error when the file does not exist. Then the caller retries three times.」→「この関数は、ファイルが存在しない場合にエラーを返します。 その後、発信者は 3 回再試行します。」（185 ms）。「caller」を「発信者」と訳している。
  - 「Open the GitHub settings」→「GitHub 設定を開きます」（56 ms。句点を足して訳し、取った）
  - 「テストを実行する前に設定ファイルを確認します。」→「Check the configuration file before running the test.」（58 ms）
  - 「ユーザーはこのプルリクエストの変更点の要約を求めている。」→「Users are asking for a summary of the changes in this pull request.」（79 ms）
  - 「サーバーがタイムアウトしたリクエストをどう扱うか見てみよう。」→「Let's see how the server handles requests that have timed out.」（86 ms）
- `translate::tests::translates_here`（`INOCODE_TRANSLATE_SERVICE=chromium`）：「I should check `src/main.rs` before running the tests.」→「確認する必要があります `src/main.rs` テストを実行する前に。」、「The user wants a & b <now>.」→「ユーザーは A & B <Now> を求めています。」。コードの前後を別々に訳すため、Edge・Firefox 方式と同じく文が不自然になる。
- 模擬（ネットもモデルも使わない）：`translate::chromium::tests`（部品の id、更新サーバーの答えの読み方、CRX3 の読み方と鍵・id の照合、zip から取り出すファイルの制限と CRC、`translation_model.pb`・`processor_spec.pb` の読み方、大文字の戻し方）、`translate::tflite::tests`（各演算を小さなテンソルで、hybrid の FULLY_CONNECTED を float の結果と比べて、手で組んだ小さな TFLite ファイルの読み込みと実行、知らない演算や壊れたファイルを断ること）。

### Chromium 方式で未検証のこと

- Chrome 自身の訳と同じになるか（Chrome を動かして比べていない）。訳の質の測定はしていない。上の例のとおり誤訳がある。
- `b2a`、`add_period_10`、`lc` の後の大文字の戻し方について、Chrome（libtranslatekit）が実際にすること。`translation_model.pb` の各番号の意味（上は値からの推定）。辞書 `dictionary.bin` の使い道（使っていない）。
- en-ja 以外のパック（zh など）は動かしていない。フォルダ名・言語の書き方・演算の種類が違えば読み込みで誤りになる。英語経由（ja→zh など）も動かしていない。
- 2048 ビットの鍵と ECDSA の証明は確かめていない。
- 更新サーバーが、実在の Chrome でない要求（版を名乗るだけで CUP 署名なし）をこの先も受けるか。手元のパックは新しい版が出ても取り直さない。
- debug ビルドでの速さ（測っていない）。

## 思考の翻訳を Chromium のモデルで（本物での確認、2026-11-25）

- `thinking_is_translated_while_turned_on`（`INOCODE_TRANSLATE_SERVICE=chromium`、Claude OAuth `claude-haiku-5-5`（high）、release ビルド、言語パックは試験の中で初めて取得）：思考 34 件、訳 4 件。オフ後のターンは訳 0 件。全体 8.62 秒。
- 訳の例：「9:47 に 2 時間 38 分が 12:25 になると、日本語で答えます。 算術がチェックアウトされるので、ここにはツールは必要ありません。」（"checks out" を直訳している）。

## 流し方の決め事（inocode の判断）

- 翻訳部品は、方式・Google の鍵・モデルの置き場所が同じなら、プロセスで 1 つを使い回す（`translate::shared`）。読み込んだモデルと開いた接続が、ターンをまたいで残る。
- オンオフはターンの途中でも効く。`settings/translation` で設定を書くたびに数を 1 つ増やし（`settings_written`）、ターンは思考の塊が届くたびにその数を見て、変わっていれば設定を読み直す。オンにすればその塊から訳し、オフにすれば訳しかけの分も捨てる。`settings.json` を手で書き換えた場合は、次のターンから効く（ファイルの変更は見張っていない）。
- 思考を受け取る側は、翻訳を待たない。訳を待つ塊が 32 個を超えたら、古いものから原文のまま `error`（`translation_behind`）を付けて出す。モデルの流れは止めず、溜まる量に上限を置く。
- 答えが終わった後に待つのは 10 秒。それまでに訳せなかった分は、原文のまま `error`（`translation_timeout`）を付けて出す。黙って捨てない。
- 試験：`translate::tests::a_stalled_service_lets_the_text_through_as_written`（答えない相手に 40 文を流し、32 個を超えた分が `translation_behind`、残りが `translation_timeout` で、原文が順番どおり全部出ること）、`translate::tests::the_same_settings_share_one_translator`、`turn::tests::a_change_of_the_settings_is_seen_by_a_running_turn`。

## 訳の正確さの測り方

- 目的：訳を読んで、思考の内容を正しく理解できるか。Bergamot や Chrome の訳との一致ではなく、意味が保たれているかを測る（ユーザーの指示「内容を理解できるか正確性を測れ」、2026-11-25）。
- 方法：GEMBA-DA（Kocmi, Federmann, "Large Language Models Are State-of-the-Art Evaluators of Translation Quality", EAMT 2023, arXiv 2302.14520, https://arxiv.org/abs/2302.14520）。原文と訳文を見せ、0（意味が全く保たれていない）〜100（意味も文法も完全）で点を付けさせる。人の参照訳は使わない形にした。
  - 論文の結果：GPT-4 で、方式どうしの優劣の判定が人の評価と 89.8%（参照訳なしで 87.6%）一致し、当時の他の指標より良かった。一方で 1 文ごとの点は人の評価との一致が弱い（Kendall の τ 0.36〜0.38）と論文自身が書いている。小さなモデル（GPT-2、Ada、Babbage、Curie）は評価役にならなかった。
  - 限界：inocode では採点役を Claude Haiku 5.5 にした（ユーザーが自由に使ってよいと決めたモデル）。GPT-4 と同じ精度で採点できるかは確かめていない。参照訳なしの指示文の正確な原文は、論文の付録からは太字の区別が読めず確認できていないので、参照訳の部分を取り除いた形にした。1 文ごとの点ではなく、方式ごとの平均で比べる。
- 原文：Claude Haiku 5.5（high）に、コーディングの質問 5 問を考えさせたときの実際の思考。流れるときと同じ区切り方（`Chunker`）で文に分け、英字が 3 文字未満の塊は除いた。
- 手で動かす試験：`server/tests/live_tools.rs` の `thinking_translations_are_scored`。

### 結果（2026-11-25、Windows 11、release ビルド、思考 32 塊、採点役 Claude Haiku 5.5（high））

| 方式 | 平均 | 中央値 | 50 未満 | 70 未満 | 32 塊を訳す時間 |
|---|---|---|---|---|---|
| Google | 87.7 | 92.0 | 1 | 2 | 0.31 秒 |
| Edge | 73.2 | 82.0 | 5 | 12 | 0.39 秒 |
| Chromium のモデル | 67.2 | 82.0 | 10 | 12 | 7.74 秒（言語パックの取得を含む） |
| Firefox のモデル | 61.4 | 62.0 | 10 | 17 | 4.00 秒（モデルの取得を含む） |

- 点の低かった例（原文の頭／訳）：
  - 全方式：「The println!」だけの塊（Google 30、Edge 8、Firefox 18）。`println!` の `!` を文の終わりとみなして切ったため。区切り方の問題。
  - Google（70）：「`let t = s;` moves the String…」→「`let `let t = s;` Stringを移動するので…」。コードの一部が重なって出た。原因は調べていない。
  - Edge（35）：`parse_config` の前後を別々に訳したため、「本線や支店で取り消されたり異動になったり」と意味が崩れた。
  - Firefox（12）：「`cfg(test)`古物や文化財がないかどうか…」。コードで切られた短い断片を訳すと崩れる。
  - Chromium（15・25）：長い文の後半が丸ごと抜けた（「残りの原因について説明します。」だけになった）。
- 読み取れること（inocode の判断）：Google は思考の内容をほぼ正しく伝える。Edge・Firefox・Chromium は、コードの前後を別々に訳す作りで文が断片になり、点が下がっている。Chromium は長い文で内容を落とす。点は 1 回の採点で、採点役による誤差を含む。

## 未実装・未検証

- オンライン方式の速さの体系的な測定はしていない。
