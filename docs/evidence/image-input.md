# 画像をモデルに見せる（画像を見るツール、カメラ）

- **対象と範囲：** プロジェクトの画像ファイル（スクリーンショット、図、写真）とカメラで撮った写真を、ツールの結果としてモデルに見せる。軽くする方法（縮小、必要な部分の切り抜き）を含む。
- **関連コード：** `core/src/picture.rs`（開く、切り抜く、縮める、保存する、費用の見積もり）、`core/src/read.rs`（`read` で画像を読む、`region`）、`core/src/conversation.rs`（`Image`、`ToolResult` の `images`、3 つの形式への変換）、`core/src/agent_tools.rs`（`Context::shown`）、`core/src/turn.rs`（画像を答えに付ける）、`core/src/compaction.rs`（見積もり）、`core/src/explore.rs`（探索役には見せない）。カメラは `core/src/camera.rs`（nokhwa 0.10、`input-native`）。
- **根拠の状態：** 画像を見せる機能そのものは UNSUPPORTED_APPROVED（カメラのツールの承認、2026-11-21「カメラツール作っていい」。撮った写真をモデルに見せるのに要る）。既定の大きさと切り抜きの方式は下の資料に基づく。
- **ユーザーの判断：** 2026-11-21「画像見るツールだが軽量化など手法はあるか？」「カメラツールもほしい。デバイス指定で写真や動画取るだけ。フォーカスや露出など…全部いじる」「カメラツール作っていい」。

## 提供元の決まり（公式文書、2026-11-21 にサブエージェントが確認）

- Anthropic（vision の文書）：画像の費用は 28×28 画素の区画の数（`⌈幅/28⌉ × ⌈高さ/28⌉`）。高解像度の層（Claude 4.7 以降）は長辺 2,576 画素・4,784 区画まで、それ以外は 1,568 画素・1,568 区画まで。超える画像は縦横比を保って縮小される。形式は JPEG・PNG・GIF・WebP。1 枚 10 MB（base64）まで、8000×8000 まで。ツールの結果（`tool_result`）の中に画像のブロックを入れられる（`{"type": "image", "source": {"type": "base64", ...}}`）。強い JPEG 圧縮は文字を読みにくくすると明記。
- OpenAI（images-vision の文書、openapi.yaml）：パッチ（32×32）方式のモデルは区画の数にモデルごとの係数をかける。`detail` は `low`・`high`・`original`・`auto`。区画が 30,000 を超える画像は断られる（縮小されない）。Responses の `function_call_output.output` は `input_text`・`input_image` の配列にできる。Chat Completions の `tool` メッセージは文字だけ（画像は不可）。
- どちらも、費用は画素の数と `detail` だけで決まり、ファイルの形式や大きさでは変わらない。白黒にして費用が下がる根拠はない。

## 縮小と切り抜きの研究（2026-11-21、サブエージェントが全文を読み、報告）

- 縮小すると、スクリーンショットの正確さは下がる：OSWorld（arXiv 2404.07972）で、画面だけを見せる場合は解像度が高いほど良い（数値は図だけ）。ScreenSpot-Pro（arXiv 2504.07981、査読は未確認）は、目標が画面の 0.07% と小さいと正答が大きく下がる。HR-Bench（arXiv 2408.15556）も、高解像度の画像を固定の大きさに縮めると情報が失われると報告。
- 必要な部分を切り抜いて拡大すると良くなる：ViCrop（ICLR 2025、arXiv 2502.17422）で、GPT-4o の TextVQA（小さい答えの領域）は 65.76 → 75.63（人が切り抜いた場合、表 1）。Zoom-Refine（arXiv 2506.01663、査読前）は、縮小した画像で領域を答えさせ、元の解像度の切り抜きでもう一度答えさせる方式で、API のモデルでも使える。ScreenSpot-Pro の切り抜き方式（ReGround）は 18.9 → 40.2。Anthropic の computer use の道具にも、領域を元の解像度で見る `zoom` がある。
- 画像の代わりに文字（OCR、アクセシビリティの情報）：OSWorld の表 5 で、GPT-4o はアクセシビリティのツリーだけで 11.36%、画面だけで 5.03%、両方で 11.21%。文字は画素の代わりにはならないが補う。
- モデルの中で画像のトークンを減らす方式（FastV、SparseVLM）は、モデルの内部に触れる必要があり、API では使えない。

## 設計（推測による当てはめ）

1. 画像は提供元の上限に収まるよう、手元で縦横比を保って縮小する（既定の長辺 1,568 画素。Anthropic の標準の層の上限）。
2. `region`（元の画像の画素で `[x0, y0, x1, y1]`）で、その部分だけを元の解像度で見られるようにする（ViCrop、Zoom-Refine、Anthropic の `zoom`）。
3. 形式は変えない（PNG はそのまま、圧縮で文字を壊さない）。白黒にはしない。
4. 文字の抽出（OCR）は入れない（依存が増え、根拠も画面の操作に限られる）。

## 実装（2026-11-21）

- `read` の `path` が画像（png・jpg・jpeg・gif・webp）なら、画像として開く（`image` クレート 0.25、png・jpeg・gif・webp だけ）。`region` があれば切り抜き、長辺が 1,568 画素を超えれば縦横比を保って縮める（Lanczos3）。元が JPEG なら JPEG（品質は既定）、それ以外は PNG で、データベースの横の `<名前>.files/images/thread-<スレッド>/` に保存する。答えの文は「Image a.png: 3000×1000 pixels; shown at 1568×523.」の形。
- 画像はツールの結果の `images`（パス、形式、縦横）として履歴に残し、要求を作るときにファイルを読んで base64 にする。ファイルがなくなっていたら、画像の代わりに「(the image … is no longer available)」と書く。
  - Anthropic：`tool_result` の `content` を、文と画像のブロック（base64）の配列にする。
  - Responses：`function_call_output` の `output` を、`input_text` と `input_image`（data URL、`detail` は `auto`）の配列にする。
  - Chat Completions：`tool` メッセージは文字だけなので、続いたツールの結果の画像を、その直後の 1 つの人のメッセージ（`image_url`）にまとめて入れる（公式文書にない、よくあるやり方）。
- 圧縮の見積もり：画像 1 枚を 28×28 画素の区画の数のトークンとし、1 トークンを 4 バイトの文字として数える（推測）。
- 探索役が `read` で画像を読んだときは、画像を探索役にも頼んだモデルにも見せず、「(the image is not shown to the explorer)」と書く。
- 検証：単体テスト `large_images_are_shrunk_and_regions_cut`・`images_are_known_by_their_extension`（`picture.rs`）、`images_of_tool_results_reach_each_format`（`conversation.rs`）。結合テスト `an_image_read_is_shown_to_the_model`（PNG を読むと、次の要求で `tool` メッセージの後に画像が付く。画像の外の `region` は断る）。

## カメラ（2026-11-21）

- 道具 `camera`。`action` は次の 4 つ。
  - `list`：装置の一覧（番号、名前、説明）。UVC の HDMI キャプチャもカメラの 1 つとして見える。
  - `controls`：1 台の形式（解像度・fps・画素の形式。大きい順に 12 件）と、調整できる項目（名前、値、範囲、刻み、既定、自動・手動などの状態）。
  - `photo`：撮る。安定させるために最初の 5 コマを捨て、6 コマ目を使う。元の大きさの PNG を保存し、`picture::prepare` で縮めた（`region` があれば切り抜いた）画像をモデルに見せる。
  - `video`：1〜60 秒（既定 5 秒）を ffmpeg に生の RGB で渡して mp4 にする。時間の 4 分の 1 ごとに 1 コマ、4 コマまでをモデルに見せる。ffmpeg がなければ失敗にする。
- 撮る前に、`controls` に名前と値（数か true・false）を書けば設定する。名前は、決まった項目は固定の名前（`focus`、`exposure`、`gain`、`white_balance`、`brightness`、`contrast`、`hue`、`saturation`、`sharpness`、`gamma`、`backlight_compensation`、`pan`、`tilt`、`zoom`、`iris`）、それ以外は装置が付けた名前（小文字、空白は `_`）。`resolution`（[幅, 高さ]、近い形式を選ぶ）と `fps` も選べる。
- 装置は番号か、名前の一部で選ぶ（省くと 0 番）。
- 承認：`photo` と `video` は、full モード以外では必ず人の承認を待つ（auto モードでも審査役に回さない）。`list` と `controls` は撮らないので待たない。カメラは写る人への影響が大きいため（工学上の選択）。
- MJPEG のコマは `image` クレートで読む（nokhwa の `mjpeg` 機能は mozjpeg を使い、C と nasm のビルドが要るので入れていない）。YUYV・NV12 などは nokhwa が変換する。
- 自動・手動の切り替え：nokhwa は値を設定するときにいまの自動・手動の状態をそのまま使う（nokhwa-bindings-windows 0.4.6 の `set_control` を読んで確認）ので、Windows では `core/src/camera_mode.rs` が、カメラを開く前に Media Foundation で同じ装置を開き、DirectShow の `IAMCameraControl`・`IAMVideoProcAmp` の `Set` で状態だけを切り替える（値はそのまま）。`controls` の値が `"auto"`・`"manual"` ならその状態に、数なら先に手動にしてから値を設定する（自動のままでは値が効かないため）。切り替えられるのは pan・tilt・roll・zoom・exposure・iris・focus と、brightness・contrast・hue・saturation・sharpness・gamma・white_balance・backlight_compensation・gain。Linux（V4L2）と macOS では `"auto"`・`"manual"` を断り、`controls` に出る自動の項目（V4L2 では別の項目）を使うよう返す（Linux で別の項目として見えるかは推測、未確認）。依存に `windows` 0.62（nokhwa と同じ版）を足した。実物のカメラで切り替えるのは、装置の設定を変えるため、人の承認なしには試していない（コンパイルと単体テスト `modes_are_asked_by_auto_and_manual`・`controls_with_a_mode_are_known_by_name` だけ）。
- 依存：nokhwa 0.10（Windows は Media Foundation、Linux は V4L2、macOS は AVFoundation）。Linux ではビルドのときに bindgen で `videodev2.h` を読む（clang と Linux のヘッダーが要る。CI の ubuntu で通るかは未確認）。
- 検証：単体テスト `only_photos_and_videos_wait_for_the_person`・`known_controls_have_fixed_names`、手動のテスト `lists_the_cameras_here`（2026-11-21、このマシンで 2 台を一覧できた）。結合テスト `the_camera_takes_pictures_only_with_the_persons_approval`（on-request と auto のモードで `photo` が人の承認待ちになり、審査役に聞かないこと）。実際の撮影・録画・設定は、人の承認なしに撮らないため、まだ試していない。
- 未検証：実際のモデルが画像を読めるか、各提供元が `tool_result`・`function_call_output` の画像を受け付けるか（実際の API では試していない）。
