# 日時・OS・シェル・作業フォルダをモデルに伝える

- **対象と範囲：** モデルへの指示に、いまの日時（時差つき）、OS、コマンドを動かすシェル（Windows は PowerShell、ほかは `sh`）、作業フォルダを入れること。git の情報は含めない（承認の範囲外）。
- **きっかけ（ユーザーの判断）：** 2026-11-23「日時、OSなどの基本情報は取れたほうがいいのでは？……powershellでunixコマンド実行する馬鹿が多いし時刻もおかしいの返す奴いる」。
- **関連ソースコード：** `core/src/turn.rs`（`system_prompt`、`Thread::state`）、`core/src/shell.rs`（`shell_command`：Windows は `powershell.exe -NoLogo -NoProfile -Command`、ほかは `sh -c`）。シェルの説明文はどのシェルかを言っていない。
- **根拠の状態：** UNSUPPORTED_APPROVED。2026-11-23、根拠が弱い（日時）か無い（OS・シェル・作業フォルダ）ことを示したうえで「日時・OS・シェルをモデルに伝える機能は、根拠が弱いまま作りますか？」と尋ね、ユーザーが「作る」と答えた。

## 資料（論文の結果）

- Dhingra et al., "Time-Aware Language Models as Temporal Knowledge Bases", TACL 10:257–273, 2022. DOI 10.1162/tacl_a_00459.
  - 学習のときに文章へ時刻を付けると、時期による事実の答えと確信度の合い方がよくなった。
  - 限界：学習のときの話で、できあがったモデルの指示に今日の日付を書くことは試していない。間接的な支えにとどまる。
- Zhao et al., "Set the Clock: Temporal Alignment of Pretrained Language Models", Findings of ACL 2024. arXiv 2402.16797.
  - 知識の締め切りが 2022 年の LLaMA-2 は、おおむね 2019 年ごろの知識で答える。
  - 限界：問題があることは示すが、指示に「今年は X 年」と書くだけの比較は要旨に無い。
- Cheng et al., "Dated Data: Tracing Knowledge Cutoffs in Large Language Models". arXiv 2403.12958（査読前）。
  - 実際の知識の締め切りが、公表された締め切りとずれることが多い。日付を指示に入れることは試していない。
- Sanz-Guerrero et al., "Dating the Model: Hidden Dates in System Prompts Affect LLM Evaluation", AACL-IJCNLP 2026 (short). DOI 10.18653/2026.aacl-short.39.
  - 指示の日付だけを 2024 年の毎日に変えた。9 つのモデル、6 つのデータで試した。
  - 結果：成績が日付だけで変わり、選択式の問答で最大 6%、数学で 14%、コードで 7%、翻訳で BLEU 2.84 動いた。モデルの順位も入れ替わり、考えを書かせると揺れは大きくなった。
  - 指示に日付を入れると答えが揺れることを示す。時期を問う答えがよくなるとは示していない。
- OS・シェル・作業フォルダを指示に入れたときと入れないときを比べた研究は見つからなかった。
  - InterCode（Yang et al., 2023, arXiv 2306.14898）と SWE-agent（Yang et al., 2024, arXiv 2405.15793）は、実行結果を返すことや道具の作りが効くことを示すが、環境の情報は試していない。
  - Vo et al.（arXiv 2405.06807、査読前）は Bash と PowerShell の生成を評価したが、環境の情報の有無は比べていない。
  - PowerShell で Unix のコマンドを打つ失敗の報告は、事例（不具合報告・ブログ）だけ。

## 検証済み・未検証

- 検証済み（コードを読んで）：いまのモデルへの指示には、日時・OS・シェル・作業フォルダのどれも入っていない。
- 未検証：入れたときに、シェルを取り違える失敗や日時の間違いが減るか。日付で答えが揺れる（Sanz-Guerrero et al.）ことが、この道具でも問題になるか。
- 作るなら確かめること：同じ依頼を、入れたときと入れないときで、Windows で走らせる。数えるのは、Unix のコマンドを PowerShell に投げた回数と、日付を間違えた回数。

## 実装とテスト

- OS（`std::env::consts::OS`・`ARCH`）、シェル（`core/src/shell.rs` の `COMMAND_SHELL`。Windows は PowerShell の書き方を使い Unix のコマンドを使わないよう、ほかは POSIX の書き方を使うよう書く）、作業フォルダは変わらないので、システムプロンプト（`system_prompt` の `environment`）に入れる。キャッシュされる先頭部分は変わらない。
- 日時は要求のたびに変わるので、保存しない状態の部分（`Thread::state`、会話の後ろに毎回付く）の先頭に、人のコンピューターの現地時刻で「Now: 2026-11-23 14:05 Monday, UTC+09:00」の形で入れる。分まで。現地時刻と時差は `chrono`（`clock` のみ）で読む。
- テスト：`the_model_is_told_the_system_shell_and_folder`、`the_time_names_the_day_and_the_offset`。
- 未検証：入れたことで、シェルを取り違える失敗や日時の間違いが実際に減るか（本物のモデルでは未試行）。日付で答えが揺れる（Sanz-Guerrero et al.）影響。
