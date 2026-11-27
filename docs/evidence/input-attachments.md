# 人のメッセージに添えるファイル（画像、PDF、文字のファイル）

- **対象と範囲：**
  - `turn/start` に `attachments` を付けられる。1 つずつ、次のどちらか。
    - `{name, media_type?, data}`：ファイルの中身を base64 で送る。画面がエンジンと別の機械にあっても送れる。
    - `{path}`：エンジンの機械のファイル。スレッドのプロジェクトのフォルダーからの相対パス（絶対パスも可）。プロジェクトのフォルダーの外（`..` やリンクで出る先を含む）、無いファイル、フォルダーは断る。プロジェクトの無いスレッドでは使えない。
  - 種類の見分け方：名前の拡張子（png・jpg・jpeg・gif・webp は画像、pdf は PDF）、無ければ `media_type`（`image/…`、`application/pdf`）。どちらでもなければ文字のファイルとして読む。`data` は UTF-8 で、0 のバイトが無いこと。`path` は `read` と同じく、0 のバイトが無いこと。
  - 画像：`read` の道具と同じ方法で開き、長辺 1,568 画素まで縮め、データベースの横の `<名前>.files/images/thread-<スレッド>/` に保存する（docs/evidence/image-input.md）。人のメッセージ（`Body::User`）の `images` に、パス・形式・縦横を残す。文の部分に「The person attached an image. Image shot.png: 1×1 pixels; shown at 1×1.」を足す。要求を作るとき、ファイルを読んで base64 にする。
    - Chat Completions：人のメッセージの `content` を、文（部分をつないだもの）と `image_url`（data URL）の配列にする。
    - Anthropic Messages：文のブロックの後に `image` のブロック（base64）を並べる。
    - Responses：`input_text` の後に `input_image`（data URL、`detail` は `auto`）を並べる。
    - 保存した画像が消えていれば、画像の代わりに「(the image … is no longer available)」と書く（道具の結果の画像と同じ）。
  - PDF：`read` と同じく、1 ページ目から 20 ページ・20,000 文字までの文を渡す。`path` なら、続きは `read` の `from` で読めると書かれる（`read` の答えと同じ「… continue with from=N」）。`data` なら、続きは `read` で読めない、と書く。
  - 文のファイル：`read` と同じく行に番号を付け（1 行は 2,000 文字まで）、約 2.5 万トークン（75,000 バイト。圧縮の見積もりと同じ 1 トークン 3 バイト）までを渡す。超えた分は切り、何行目まで渡したかと、`path` なら `read` の `path` と `from` で続きを読めること、`data` なら続きは読めないことを書く。見出しは「The person attached <名前>:」。
  - 画像を読めないモデル：models.dev の一覧の写し（データベースの横の `catalog/models-dev.json`、モデルの一覧の画面が取ったもの）で、スレッドのモデルの `modalities.input` に `image` が無ければ、`invalid_params` で断る。写しが無い、モデルが載っていない、`modalities` が書かれていないときは、断らずに送る（提供元が断れば、そのエラーになる）。
  - 断るときは、すべて `invalid_params` で、ターンを始める前（スレッドを取る前、何も保存する前）に断る。`failed` の通知になる。
  - `thread/read` と `message` の通知には、保存したメッセージがそのまま出る。画像は `images` の `path`・`media_type`・`width`・`height` で、base64 は出ない。
  - 上限（作業者が決めた値）：1 つのメッセージに 10 個まで。1 つのファイルは 10 MiB まで（`data` は元のバイト、`path` はファイルの大きさ）。`data` の合計は、プロトコルの 1 フレームの上限（16 MiB）にも収まる必要がある。Anthropic の画像の上限（1 枚 10 MB）と同じ程度にした。
  - 範囲外：
    - `turn/steer` と `thread/queue/*` の文には、ファイルを添えられない（文だけ）。
    - 画像の `region`、PDF の `page`。モデルが続けて `read` で読める。
    - `/attach` のコマンド。使えないままで、理由を「画面が `turn/start` の `attachments` で添える」に変えた。
  - 限界：
    - 審査役・探索役など、ほかの役に渡す人のメッセージには、画像を入れていない（文だけ）。
    - `data` の PDF でページに文がほとんど無いとき、答えには `read` の `page` で見るよう書かれるが、`data` では読めない。
- **関連ソースコード：**
  - `core/src/attach.rs`：`Requested::parse`（引数を読む）、`gather`（ファイルを読む）、`message`（画像を保存して人のメッセージを作る）。
  - `core/src/conversation.rs`：`Body::User` の `images`、3 つの形式への変換（`anthropic_image`、`responses_image`）。
  - `core/src/lib.rs`：`Request::attached`、`Core::turn_prompt`（画像を読めないモデルを断る）。
  - `core/src/catalog.rs`：`takes_images`。
  - `core/src/pdf.rs`：`data_text`（`data` の PDF）。
  - `core/src/turn.rs`：`Prompt` の `attached`、`steps` で人のメッセージを作る。
  - `core/src/compaction.rs`：`request_bytes` が人のメッセージの画像も数える。
  - `server/src/lib.rs`：`start_turn`、`run_turn`（`turn_prompt` を `begin_turn` の前に呼ぶ）。
- **根拠の状態：** UNSUPPORTED_APPROVED。ファイルを添える手段で、効き目を測る学術研究は当てはまらない。調べたのは、ほかのハーネスの実装だけ。
- **ユーザーの承認：** 2026-11-27。
  - 足りない機能の洗い出しで「急ぐ群から順に」と答えた。
  - 「⑦ 入力に添えられるものはどれにしますか？」に「画像と PDF・文字のファイル（画像はそのままモデルに見せる。PDF と文字のファイルは、今の read 道具と同じように中身を渡す。画像を読めないモデルには、そう伝えて断る）」と答えた。
  - 引数の形、上限の値、`path` をプロジェクトの中に限ること、models.dev の写しで画像を読めるかを決めることは、作業者（Claude）が決めた。

## ほかのハーネス（2026-11-27、調査用のエージェントがソースと資料を読み、報告。inocode の作業者は直接読んでいない）

| ハーネス | 添えるファイル |
|---|---|
| Claude Code | `@` で指したファイルを、Read の道具の上限（既定 25,000 トークン）まで入れる。画像はそのまま見せる |
| Codex | 画像は `localImage` として添える |
| opencode | 添えたファイルを 20 MB まで全部入れる（packages/core/src/session/prompt.ts の `readFileAttachment`） |

## 検証

- 部品のテスト：
  - `conversation.rs` の `images_of_the_persons_message_reach_each_format`：人のメッセージの画像が、Anthropic・Responses・Chat Completions の形になり、消えた画像は文で知らせる。
  - `attach.rs` の `bad_attachments_are_refused`（多すぎ、base64 でない、名前が無い、`path` と `data` の両方、大きすぎ）、`a_pdf_and_a_text_file_become_text_parts`（`data` の PDF と文、`path` の PDF。0 のバイトのある `data` と BMP は断る）、`a_long_text_gives_its_head_and_how_to_read_on`。
  - `catalog.rs` の `image_input_is_read_from_the_modalities`。
- 結合テスト（`server/tests/turn_tcp.rs`）：
  - `attachments_and_mentions_join_the_persons_message`：`data` の画像と文、`path` の文が、模擬の提供元への要求（Chat Completions）の人のメッセージに入る。`message` の通知と `thread/read` に画像の保存先が出て、base64 は出ない。
  - `files_that_cannot_be_sent_refuse_the_turn_before_it_starts`：プロジェクトの外、無いファイル、base64 でない、対応しない種類、大きすぎ、多すぎを `invalid_params` で断り、メッセージは保存されず、提供元への要求も無い。
  - `an_image_is_refused_for_a_model_listed_without_image_input`：一覧で画像を読めないモデルへの画像を断り、文のファイルは送る。
- 確かめていないこと：実際の提供元が、人のメッセージの画像を受け付けるか（実際の API では試していない）。
