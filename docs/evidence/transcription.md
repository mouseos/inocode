# 文字起こしの道具（`transcribe`）

- **対象と範囲：** 音のファイルの話し声を文字にする道具。送り先は次の 3 つ。どれも公開された API ではなく、各社のアプリが中で使っている送り先。
  - Claude：Claude のサインイン
  - ChatGPT：ChatGPT のサインイン
  - Google：Chrome の音声認識が使う送り先
- **関連ソースコード：** `core/src/transcribe.rs`、`core/src/audio.rs`（`rechannel`、`resample`、`flac_bytes`）、`core/src/connectors.rs`（`Account::headers`：サインインのヘッダー）。
- **根拠の状態：** UNSUPPORTED_APPROVED。ユーザーの承認：「文字お越しはclaude chatgpt oauth/自己責任でgoogleのchrome api流用」（2026-11-24）。

## 各社の送り方（確かめた事実）

### Claude（Claude Code 2.1.284 の実行ファイルの中の JavaScript を読んだ）

- 送り先：`wss://api.anthropic.com/api/ws/speech_to_text/voice_stream`（WebSocket）。
- 問い合わせの項目：`encoding=linear16`、`sample_rate=16000`、`channels=1`、`endpointing_ms=300`、`utterance_end_ms=1000`、`language`（既定 `en`）、`use_conversation_engine=true`。
- ヘッダー：`Authorization: Bearer <Claude の OAuth トークン>`、`User-Agent`、`x-app: cli`。Claude Code は `anthropic-client-platform` も送るが、値を確かめていないので inocode は送らない。
- 送るもの：
  - 最初に `{"type":"KeepAlive"}` を送り、8 秒ごとにくり返す。
  - 音は 16kHz・1 チャンネル・16 ビットの PCM をバイナリで送る。
  - 最後に `{"type":"CloseStream"}` を送る。
- 受けるもの：
  - `TranscriptInterim`／`TranscriptText`：途中の文で、`data` に入る。
  - `TranscriptEndpoint`：その時点の文が確定した印。
  - 誤り：`TranscriptError`（`description`／`error_code`）と `error`（`message`）。
- Claude Code は音を話す速さで送る。inocode はファイルを一度に送る。2026-11-24 の実測では、サーバーはこれを受け付けた（下の「本物での確認」）。

### ChatGPT（openai/codex のソース `codex-rs/backend-client/src/client.rs` の `transcribe_audio`）

- 送り先：`POST https://chatgpt.com/backend-api/transcribe`。
- 本文：multipart で、項目名 `file`、ファイル名 `audio.<拡張子>`。
- ヘッダー：ChatGPT のサインインのもの（`Authorization`、`chatgpt-account-id`）。
- 答え：`{"text": …}`。
- 形式：wav、mp3、m4a、webm、ogg。
- 25MB の上限は inocode の決め事で、サーバーの上限は確かめていない。

### Google（Chromium のソースを読んだ）

- 読んだファイル：
  - `content/browser/speech/network_speech_recognition_engine_impl.cc`
  - `components/speech/audio_encoder.cc`、`components/speech/chunked_byte_buffer.h`
  - `content/browser/speech/speech_recognizer_impl.h`
  - `content/public/browser/google_streaming_api.proto`
- 送り先：`https://www.google.com/speech-api/full-duplex/v1`。
  - 結果は `/down?key=…&pair=…&output=pb` を GET して読む。
  - 音は `/up?…` に POST する。同じ `pair` の値で 2 つを結び付ける。
- `up` の項目：`key`、`pair`、`output=pb`、`lang`、`pFilter=0`、`app=chromium`、`continuous`。
- 音の形式：FLAC、1 チャンネル、16 ビット、16kHz。`Content-Type: audio/x-flac; rate=16000`。Chrome の録音側の値（`kAudioSampleRate = 16000`）による。
- 答え：4 バイトのビッグエンディアンの長さ＋`SpeechRecognitionEvent`（proto2）のくり返し。
  - `status`（1）：0 以外なら誤り。
  - `result`（2）：`alternative`（1）→ `transcript`（1）、`final`（2）。
  - inocode は、確定（final）した結果の最初の候補だけを使う。
- **キー：**
  - 既定は Chrome が送るものとして知られているキー（`transcribe.rs` の `GOOGLE_KEY`）。オープンソースの Python ライブラリ SpeechRecognition も、同じ値を既定にしている（`speech_recognition/recognizers/google.py`）。
  - `~/.inocode/settings.json` に `google_speech_key` があれば、そちらを使う。
  - 既定のキーを入れることはユーザーの指示による：「デフォルトで入れなさい。既知だ。」（2026-11-24）。
- **人が負う危険（ユーザーは「自己責任」と承知）：**
  - Chromium の文書は、Chrome が使う Google の API の多くを Google 専用とし、派生した製品向けではないと書いている。キーは配ったり共有したりしないよう求めている（https://chromium.org/developers/how-tos/api-keys）。
  - Chromium の開発者は、この音声 API を Chrome の開発用と答えている（https://groups.google.com/a/chromium.org/g/chromium-dev/c/pdDDdYH2Hg4）。
  - 予告なく使えなくなることがある。

## 待ち切りと言語の既定（このプロジェクトの判断）

- Claude の文字起こしは、音を送り終えた後、サービスが接続を閉じるまで待つ。30 秒何も届かなければ、終わっていないとみなし、それまでに聞き取れた文を「不完全かもしれない」と添えた誤りとして返す（成功としては返さない）。
- `language` を指定しないときの言語：`settings.json` の `language` が言語の印（`ja`、`ja-JP` のような、2〜3 文字の言語と、英数字の続き）ならそれ、そうでなければ OS の表示言語。どちらも無ければ Claude は `en`、Google は `en-US`。`language` に「Japanese」のような言語の名前が書いてあるときは、印ではないので使わない（名前から印への変換はしない）。Claude には地域を除いた言語（`ja-JP` なら `ja`）を送る。
- テスト：`a_service_that_stops_answering_gives_no_transcript`、`only_language_tags_are_taken_as_the_spoken_language`、`the_settings_language_is_the_default_when_it_is_a_tag`。

## 音の変換

Claude と Google には WAV を読み、全チャンネルの平均で 1 チャンネルにして、16kHz に変換して送る（変換は `audio.rs` と同じ簡単なもの）。長さは 600 秒まで。

## テスト

- `transcribe::tests`
  - Google の答えから確定した文だけを読み、断られた理由を名前で返す。壊れた答えは誤りにする。
  - Claude のメッセージの並びから文を作る。誤りを返す。
  - WAV を 16kHz・16 ビットにする。
  - ChatGPT へ送る multipart の形。
  - 設定に Google のキーがあればそれを使い、無ければ既定のキーを使う。
- 手で動かす試験：`transcribe::tests::transcribes_here`。`INOCODE_TRANSCRIBE_CALL` に道具の引数を入れる。`~/.inocode` のサインインと設定を使う。

## 本物での確認（2026-11-24、ユーザーの承認「試す」）

- 入力：Windows の読み上げ（Microsoft Haruka、ja-JP）で作った WAV。文は「今日はいい天気です。録音と文字起こしの試験をしています。」。
- chatgpt：「今日はいい天気です録音と文字起こしの試験をしています。」（句点が 1 つ抜けた）。
- claude（language=ja）：「今日はいい天気です。録音と文字起こしの試験をしています。」（元の文と同じ）。
- 各送り先へ 1 回ずつ送った。認識の正確さを比べたものではない。
- google（language=ja-JP、既定のキー）：「今日はいい天気です  録音と文字起こしの試験をしています」。確定した 2 つの結果を空白でつないでいる。句点は付かなかった。
