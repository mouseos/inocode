# @ でファイルを指す（中身を渡す、大きいものは頭だけ）

- **対象と範囲：**
  - `turn/start` に `mentions: [path]` を付けられる。どれを指したかは画面が決め、`prompt` の文は打ったまま送る。
    - パスはスレッドのプロジェクトのフォルダーからの相対パス（絶対パスも可）。プロジェクトのフォルダーの外（`..` やリンクで出る先を含む）、無いパスは `invalid_params` で、ターンを始める前に断る。プロジェクトの無いスレッドでは使えない。20 個まで（作業者が決めた値）。
    - ターンを始めるときに読み、人のメッセージの文の部分として、打った文の後に足す。見出しは「The person mentioned <パス>:」。
    - 文のファイル：`read` と同じく行に番号を付け（1 行は 2,000 文字まで）、約 2.5 万トークンまでを渡す。トークンは数えず、圧縮の見積もりと同じ 1 トークン 3 バイト（`FALLBACK_BYTES_PER_TOKEN`）として、75,000 バイトまでにする。超えるファイルは頭だけを渡し、「… cut here at about 25000 tokens: lines 1-K of N are shown. Read the rest with `read` (path <パス>, from K+1).」と書く。0 のバイトがあるファイルは断る。
    - フォルダー：中の名前の一覧（名前の順、フォルダーは `/` で終わる、1,000 件まで）。`.gitignore` は見ない。
    - 画像と PDF：添えるファイル（docs/evidence/input-attachments.md）の `path` と同じ。画像は見せ（画像を読めないモデルなら断る）、PDF は 20 ページ・20,000 文字までの文。
  - 要求 `files/search {thread_id, query, limit?}`：画面の補完のため、プロジェクトのフォルダーのファイルのパスをあいまいに探し、`{files: [{path, score}]}` を良い順に返す。
    - `find` の道具と同じ歩き方（`.gitignore` に従う。隠しファイルは含め、`.git` は除く）。パスは `/` でつなぐ。
    - 合い方：`query` の文字（大文字と小文字は区別しない、空白は無視）がパスに順に現れるものだけ。合った文字ごとに 16 点、直前の文字にも合っていれば 8 点、語の始め（`/`・`_`・`-`・`.`・空白の後、小文字の後の大文字）なら 8 点、ファイル名の中なら 4 点を足し、最初と最後の合った文字の間で飛ばした文字ごとに 1 点を引く。始めの文字の合う場所ごとに数え、一番良いものを取る。同じ点なら短いパス、次に名前の順。fzf などの点の付け方を真似た、作業者の決めた式で、研究の裏付けは無い。
    - 空の `query` は、すべてのファイルを短い順に返す。
    - `limit` は既定 20、1〜100。読む処理は読み取りのループの外で動く。
  - 範囲外：
    - `turn/steer` と `thread/queue/*` の文での指定。
    - Claude Code の `CLAUDE_CODE_DISABLE_FILE_MENTIONS` のような、パスだけを渡す切り替え。
    - 256 KB を超える文のファイルを渡さない、という Claude Code の決まり（ここでは頭だけを渡す）。
- **関連ソースコード：**
  - `core/src/attach.rs`：`Requested::parse`、`gather`（`inside`、`listing`、`from_path`、`head`）。
  - `core/src/find.rs`：`fuzzy_paths`、`fuzzy_score`。
  - `core/src/lib.rs`：`Core::turn_prompt`、`Core::search_files`。
  - `core/src/read.rs`：`cut`（1 行の上限）を使う。
  - `server/src/lib.rs`：`start_turn`、`run_turn`、`search_files`。
- **根拠の状態：** UNSUPPORTED_APPROVED。中身を渡すのと、パスだけを渡すのとを直接比べた研究は見つからなかった。
- **調べた資料（間接の根拠。どれも、この判断に直接は当てはまらない）：**
  - Jimenez ほか, "SWE-bench: Can Language Models Resolve Real-World GitHub Issues?", ICLR 2024。arXiv:2310.06770。直すべきファイルを渡した場合（oracle）は、BM25 で探したファイルを渡した場合より解けた割合が高く、関係の無い文脈を足すと下がった。エージェントが自分で読む場合との比較ではない。
  - Raju ほか, arXiv:2602.16069（査読前）。長い文脈をそのまま入れる方法は、文脈が大きいとき、エージェントが探して読む方法よりずっと悪かった。人が指した 1 つのファイルを入れる場合とは条件が違う。
  - Liu ほか, "Lost in the Middle: How Language Models Use Long Contexts", TACL 2024。DOI 10.1162/tacl_a_00638。長い文脈の中ほどの情報は使われにくい。大きいファイルを全部入れない理由の 1 つ。
  - 費用の考え（推測）：モデルが指されたファイルをどうせ読むなら、中身を先に渡すと、`read` の 1 回の要求（それまでの文脈を、キャッシュから読み直して送る分）を省ける。
- **ユーザーの承認：** 2026-11-27。
  - 足りない機能の洗い出しで「急ぐ群から順に」と答えた。
  - 「効率はどちらが良いか」と聞き、上の調べた結果を受けて、「中身を渡す、大きいものは頭だけ（Claude Code の既定と同じ。約 2.5 万トークンを超えるものは頭だけ渡し、続きは read で読むよう伝える。研究の裏付けは無く、承認として記録する）」を選んだ。
  - 引数の形、上限の値、フォルダーの一覧の形、`files/search` の点の付け方は、作業者（Claude）が決めた。

## ほかのハーネス（2026-11-27、調査用のエージェントがソースと資料を読み、報告。inocode の作業者は直接読んでいない）

| ハーネス | @ で指したファイル |
|---|---|
| Claude Code | Read の道具の上限（既定 25,000 トークン）まで中身を入れる。256 KB を超える文のファイルは入れない。フォルダーは一覧。`CLAUDE_CODE_DISABLE_FILE_MENTIONS` でパスだけにできる |
| Codex（codex-rs/tui/src/bottom_pane/chat_composer.rs の `insert_selected_path`） | `@` をパスの文字に置き換えるだけ。画像は `localImage` として添える |
| opencode（packages/core/src/session/prompt.ts の `readFileAttachment`） | 添えたファイルを 20 MB まで全部入れる |

## 検証

- 部品のテスト：
  - `attach.rs` の `a_long_text_gives_its_head_and_how_to_read_on`：1,000 行のうち 715 行（75,000 バイトまで）を渡し、続きの読み方を書く。`data` なら続きは読めないと書く。
  - `find.rs` の `paths_match_fuzzily_best_first_without_ignored_files`：空白と大文字を無視して合い、同じ点なら短いパスが先、`.gitignore` で除いたファイルは出ず、`limit` で切る。
- 結合テスト（`server/tests/turn_tcp.rs`）：
  - `attachments_and_mentions_join_the_persons_message`：大きいファイルは頭だけと続きの読み方、フォルダーは一覧になって、打った文の後に入る。
  - `files_that_cannot_be_sent_refuse_the_turn_before_it_starts`：プロジェクトの外と無いパスを断る。
  - `files_search_matches_paths_fuzzily_and_skips_ignored_files`：`atfi` で `src/attach_file.rs` が見つかり、`.gitignore` の `target/` の中は出ない。`limit` が効き、101 は断る。
- 確かめていないこと：中身を渡すことで、実際のモデルの作業が速く、安く、正しくなるか（測っていない）。
