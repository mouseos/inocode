# git の情報をモデルに伝える

- **対象と範囲：** ブランチ、HEAD、変更の状態、最近のコミットなどの git の情報を、モデルへの指示に入れること。日時・OS・シェル・作業フォルダは [environment-context.md](environment-context.md) で扱う。
- **きっかけ：** 2026-11-23 ユーザー「あとgitはcodexなど実装どうなってる」「pdfもgitも効率的な方法調べて」。
- **関連ソースコード：** `core/src/git.rs`、`core/src/turn.rs`（`open` で読み、`Thread::state` で送る）。
- **根拠の状態：** UNSUPPORTED_APPROVED。指示に git の状態や最近のコミットを入れて、成功率やトークンを比べた研究は見つからなかった。そのことを示したうえで、2026-11-23 ユーザーが「git pdf実装」と答えた。

## 資料（論文の結果。すべて査読前）

- **Gloaguen et al., "Coding Agents Don't Know When to Act"**, arXiv 2605.07769。
  - SWE-bench Verified の 200 課題で、修正がすでに入っている（何も変えないのが正解）状態を作った。
  - `.git` を残し、環境も整えた条件では、正しく何もしなかった割合が Sonnet-4.6 で 65.0%、GPT-5.4 mini で 60.5%。`.git` を消し、環境も整えない条件では 50.0%、52.5% に下がった。
  - 正しく何もしなかった回の 63.8% は、git の履歴を読んでいた（失敗した回では 31.4%）。
  - 限界：git の情報を指示に入れたわけではない。リポジトリを消すことと、環境を整えないことを同時に変えている。Python のみ。
- **HAFix**（arXiv 2501.09135）・**HAFixAgent**（arXiv 2511.01047）。
  - 不具合の行の `git blame` からコミットを引いて、直す手がかりにした。履歴で Defects4J は +4.4%、BugsInPy は +38.6%。
  - 必要なときに引く使い方で、指示に状態を入れる話ではない。
- **長さ・無関係な文の害（査読あり、下調べの係の記憶による。読み直していない）**
  - Shi et al., ICML 2023。
  - Levy et al., ACL 2024, DOI 10.18653/v1/2024.acl-long.818。
  - 無関係な文や長い入力で正確さが落ちる。量に上限を置く一般的な支えにとどまる。

## ほかの道具の作り（下調べの係がソースで確認）

| 道具 | 指示に入る git の情報 |
|---|---|
| Claude Code | 会話の最初に一度だけ記録した状態。ブランチ、main ブランチ、`user.name`、`git status --short`（2000 字まで）、直近 5 件のコミット。「途中で更新されない」と書く。 |
| Codex | 指示には入れない。コミット、ブランチ、リモートは、会話の記録の付属情報として保存するだけ。 |
| Aider | 「git のリポジトリの中で作業している」の 1 文だけ。 |
| Gemini CLI | 「コミットの前に `git status && git diff HEAD && git log -n 3` を実行せよ」という決まった文。今の状態は入れない。 |
| Cline | リモート URL（認証情報は消す）、最新のコミットのハッシュ、ブランチ名。状態は入れない。 |
| opencode | 「git のフォルダかどうか（yes/no）」だけ。 |
| pi | 何も入れない。 |

## 注意点（推測を含む）

- **古くなる：** 最初に記録した状態は、モデルが編集やコミットをすると、すぐ古くなる。
- **キャッシュ：** 毎回新しくするなら、キャッシュされる先頭部分（システムプロンプト）に置くと、キャッシュが毎回外れる。Anthropic と OpenAI の公式文書はどちらも、変わらないものを先に、変わるものを後に置くよう書いている。
- **個人情報**
  - `user.name` やメールを入れると、毎回の要求に人の名前が乗る。
  - リモート URL には `https://user:token@...` のように認証情報が入ることがある。
  - コミットの文やブランチ名、未追跡のファイル名から、案件名や私的なファイル名が漏れることがある。
- **量：** 片づいていないリポジトリでは、`git status` が数千行になる。切ると、肝心のファイルが隠れることがある。
- **他人の文：** コミットの文やファイル名は第三者の文なので、指示として扱わないよう、ただのデータだと示す必要がある。

## 効率のよい作りの候補（推測。調べた時点）

1. **会話の最初に一度だけ、短い情報を入れる。** リポジトリかどうか、今のブランチ、HEAD の短いハッシュ、変更・ステージ済み・未追跡のファイルの数を 1 行で。入れるなら最近のコミットの題名 3 件（数百字まで）。
2. **`user.name`、メール、リモート URL は入れない。**
3. **「会話の最初の状態」と明記する。** 今の状態が要るときは、モデル自身に `git status` や `git diff` を実行させる。
4. **後の回では送り直さない。** 送るなら、ブランチか HEAD が変わったときだけ、会話の後ろに差分を付ける（キャッシュされる先頭部分は変えない）。

## 実装とテスト

- 依頼（turn）を始めるたびに、作業フォルダで `git status --porcelain=v2 --branch --ignore-submodules=all` を 1 回だけ実行し（3 秒で打ち切り）、次の 1 行にする。git の起動のしかたは下の「リポジトリの設定で命令を動かさせない」。
  - 例：「Git, when this turn started: branch main at 2530003; files: 3 staged, 2 changed and not staged, 1 untracked. Run git yourself for the current state and the file names.」
- 入れるもの：ブランチ（または detached HEAD）、コミットの短いハッシュ、ステージ済み・未ステージ・衝突・未追跡のファイルの数。
- 入れないもの：ファイル名、コミットの文、`user.name`・メール、リモート URL（上の個人情報と他人の文の注意による）。
- 置き場所：保存しない状態の部分（`Thread::state`、会話の後ろ）に、時刻の次に置く。システムプロンプトには入れないので、キャッシュされる先頭部分は変わらない。毎回の要求に約 40 トークン（推測）が、キャッシュされない分として加わる。
- 候補の 1（会話の最初に一度だけ）との違い：依頼ごとに読み直す。そのため古くなるのは依頼の途中だけで、そのことを文に書いている（「when this turn started」）。
- git の無いフォルダ、git が無い場合、答えが遅い場合、リポジトリの設定に clean フィルタがある場合は、何も入れない。
- テスト：`git::tests::the_line_counts_files_and_names_no_file`、`git::tests::a_new_repository_and_a_detached_head_are_named`、`git::tests::a_real_repository_is_read_and_a_plain_folder_is_not`、`git::tests::reading_the_state_runs_no_program_the_repository_names`。

## リポジトリの設定で命令を動かさせない（このプロジェクトの判断）

この `git status` はサンドボックスの外で動く。一方、サンドボックスの中のシェルの命令はプロジェクトの `.git/config` や `.gitattributes` を書ける。そのため、設定に書いた命令が次のターンの `git status` でサンドボックスの外で動かないようにする。

- `git status` で命令が動きうる設定（git の仕組みから。下の試験で 2 つを実際に確かめた）：
  - `core.fsmonitor`：値が命令のパスなら、状態を読むたびにそれを動かす（フックとして、シェル経由）。→ `-c core.fsmonitor=false` で切る。試験で、そのままの `git status` は動かし、inocode の読み方では動かないことを確かめた。
  - clean フィルタ（`filter.<名前>.clean`・`filter.<名前>.process` と、`.gitattributes` の `filter=<名前>`）：時刻の変わったファイルの中身を確かめるとき、フィルタを通してから比べるので、その命令が動く。フィルタの名前は自由なので `-c` で全部は消せない。→ 先に `git config --get-regexp '^filter\..*\.(clean|process)$'` で調べ、あれば `git status` を動かさず、git の行を入れない。試験で、フィルタの命令が動かないことを確かめた。
  - フック（`core.hooksPath`）：`git status` はフックを動かさない。索引を書き直すと `post-index-change` が動くが、`--no-optional-locks`（と `GIT_OPTIONAL_LOCKS=0`）で索引を書かない。念のため `-c core.hooksPath=<空の装置>` も付ける。
  - ページャ（`core.pager`・`pager.status`）：出力が端末でないので使われない。念のため `--no-pager`。
  - 外部の diff（`diff.external`・`diff.<名前>.command`）と `textconv`：差分の本文を出すときだけ使う。porcelain の状態表示は本文を出さず、名前の変更の判定も本文をそのまま比べる（git の仕組みからの推定。試していない）。
  - サブモジュール：`git status` はサブモジュールの中でも git を動かし、その設定（`.git/modules/...`）は上の clean フィルタの確認の外にある。→ `--ignore-submodules=all` で見ない。サブモジュールの変更は数に入らない。
- システムとユーザー全体の設定は読まない（`GIT_CONFIG_NOSYSTEM=1`、`GIT_CONFIG_GLOBAL=<空の装置>`。Windows は `nul`、ほかは `/dev/null`）。ユーザーの設定のプロジェクト別の許可（`safe.directory`）も読まないので、別のユーザーの持つリポジトリでは git の行が出ない。
- `include.path` で読み込む別のファイルも、`git config --get-regexp` は読み込んだうえで調べる。

## 検証済み・未検証

- **検証済み：** テストで、数え方、ファイル名が入らないこと、本物のリポジトリと普通のフォルダの区別。
- **未検証：** 上のどの部分も、成功率やトークンが良くなるかは確かめていない。確かめるなら、git の操作が要る依頼（変更の確認、コミットの準備、ブランチの判断）を、入れた版と入れない版で走らせ、git コマンドの回数・誤り・トークンを数える。
