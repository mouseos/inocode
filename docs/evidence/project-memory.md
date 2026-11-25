# 会話の保存とプロジェクトの記憶（履歴・教訓・手順・根拠台帳・知識のつながり）

- **対象と範囲：** 会話の状態をサーバーが SQLite に保存し、履歴をトークンを使わずにさかのぼれるようにする。プロジェクトごと・プロジェクト共通の記憶（教訓、手順、根拠、知識）を、タグとリンクでつないで芋づる式に取り出す。検索は索引だけを使い、時間の上限で打ち切る。本段階は core の保存層と検索のみ。範囲外：TCP への公開（段階 3）、エージェントループからの利用と、記憶を文脈へ差し込む処理（段階 4）、モデルによる要約・キーワード抽出、ベクトル検索、作業フォルダの grep ツール、圧縮。
- **関連コード：** `core/src/store.rs`（スキーマ、`Store`、履歴・大きな中身・ノート・タグ・リンク・検索・連想）。
- **根拠の状態：** 部分ごとに下表。全体として UNSUPPORTED_APPROVED（2026-11-16、「SQLiteにする」「この設計すべてを見て改良点がないか学術的根拠もとに調べて。これ以上のものがない場合は実装へ進んでよい」）。研究で直接支持される部分は RESEARCH_SUPPORTED とする。

## 判断と根拠

| # | 判断 | 状態 | 根拠と限界 |
|---|---|---|---|
| 1 | 保存は SQLite（WAL、`synchronous=NORMAL`、1 ターン 1 トランザクション） | UNSUPPORTED_APPROVED | エージェントの会話の保存形式を比べた研究はない。SQLite の性能評価：Gaffney et al., "SQLite: Past, Present, and Future", PVLDB 15(12):3535-3547, 2022（https://vldb.org/pvldb/volumes/15/paper/SQLite:%20Past,%20Present,%20and%20Future）。代替の限界：LSM（RocksDB）は書き込みに強い代わりに読みが弱い（RUM 予想、Athanassoulis et al., EDBT 2016）、mmap 型（LMDB）は正しさ・性能の問題（Crotty, Leis, Pavlo, CIDR 2022）、redb の比較は作者の測定のみ。WAL で読みと書きが互いを止めないのは公式文書（https://www.sqlite.org/wal.html、「ほぼ正しい」と注記）。 |
| 2 | 大きな中身（ツール出力・画像・ログ）は SHA-256 をキーに別表へ 1 回だけ保存し、本文からは参照する | UNSUPPORTED_APPROVED | Codex の再開時のメモリ不足の原因（`codex-rs/rollout/src/recorder.rs:867-905` が全項目を JSON の木にして複製しリストに保持）への対策。研究なし。 |
| 3 | 再開は文脈に入る分だけを読み、履歴は連番でページ単位に読む | UNSUPPORTED_APPROVED | 同上。Liu et al., "Lost in the Middle", TACL 12:157-173, 2024（DOI 10.1162/tacl_a_00638）：長い文脈ほど性能が落ちる（間接）。 |
| 4 | 履歴は 1 メッセージ単位で索引し、時刻を持たせて期間で絞れるようにする | RESEARCH_SUPPORTED（会話の記憶の題材） | Wu et al., "LongMemEval", ICLR 2025（https://proceedings.iclr.cc/paper_files/paper/2025/file/d813d324dbf0598bbdc9c8e79740ed01-Paper-Conference.pdf）：会話全体ではなく発言単位に分けて索引すると検索拡張の性能が上がる。時刻で範囲を絞ると時間に関する想起が上がる（強い LLM で問い合わせを展開した場合 6.8〜11.3%）。数値は二次資料経由で、原表は未確認。コーディングでの検証はない。 |
| 5 | ノートは本文に加えて要約・キーワード・条件を索引に入れる（複数の検索の入口） | RESEARCH_SUPPORTED（同上） | LongMemEval の fact-augmented key expansion（抽出した事実を鍵に加えると想起が上がる。平均 9.4% は二次資料の値）。A-Mem（下記）もノートに要点・キーワード・タグを持たせる。本段階ではモデルによる抽出はせず、書き手（モデルまたは人）が書いた要約・キーワードを索引する。 |
| 6 | 教訓は「条件つき」で持ち、合うものだけを上位数件取り出す | RESEARCH_SUPPORTED（Web 操作などの題材） | Fu et al., "AutoGuide", NeurIPS 2024（https://proceedings.neurips.cc/paper_files/paper/2024/hash/d8efbb5dd415974eb095c3f06bff1f48-Abstract.html）：状況に合う条件つき指針を選んで渡す方式が、全部渡す方式より大きく良い。 |
| 7 | 教訓は候補→有効→退役の状態を持ち、外部の検証か人の承認で有効にする。評価（上げ・下げ）を数える | RESEARCH_SUPPORTED | Xiong et al., "How Memory Management Impacts LLM Agents", ACL 2026（arXiv 2505.16067）：誤った記録は再生産され、何でも加えると悪化、厳しい判定での選択的追加と履歴に基づく削除で改善。Zhao et al., "ExpeL", AAAI 2024：追加・上げ・下げ・編集で教訓を育てる。SWE-Bench-CL（arXiv 2507.00014、査読前）：不正確な取り出しはコーディングでも害。 |
| 8 | 再利用できる手順（ワークフロー）をノートの種類として持つ | RESEARCH_SUPPORTED（Web 操作の題材） | Wang et al., "Agent Workflow Memory", ICML 2025, PMLR 267:63897-63911（https://proceedings.mlr.press/v267/wang25bx.html）：過去の軌跡から再利用可能な手順を作って渡すと、Mind2Web で 24.6%、WebArena で 51.1% の相対改善。 |
| 9 | 知識は上書きせず版を残す（置き換えのリンクと有効期間） | UNSUPPORTED_APPROVED（間接） | LongMemEval は「知識の更新」を独立した能力として測り、現状の手法が弱いことを示した（間接）。ユーザーの運用規則（以前の情報と現在の情報を区別）。 |
| 10 | 知識をタグとリンクでつなぐ | RESEARCH_SUPPORTED（会話の記憶の題材） | Xu et al., "A-Mem", NeurIPS 2025（https://proceedings.neurips.cc/paper_files/paper/2025/hash/19909c36f51abc4856b4560aff3d36d6-Abstract-Conference.html）：ツェッテルカステン式に要点・キーワード・タグ・リンクを持たせ、LoCoMo で既存手法を上回る。タグ単独の効果は分けて確認していない。 |
| 11 | 芋づる式の取り出しは、検索で当たったノートを起点とする Personalized PageRank を局所 push 法で近似する | RESEARCH_SUPPORTED（複数文書の質問応答の題材） | Gutiérrez et al., "HippoRAG", NeurIPS 2024（https://papers.nips.cc/paper_files/paper/2024/hash/6ddc001d07ca4f319af96a3024f6dbd1-Abstract-Conference.html）：概念グラフ上の PPR で、反復検索（IRCoT）と同等以上を 10〜20 倍安く 6〜13 倍速く。HippoRAG 2（ICML 2025）。局所 push 法：Andersen, Chung, Lang, "Local Graph Partitioning using PageRank Vectors", FOCS 2006（計算量はグラフの大きさに依存せず O(1/(εα))）。 |
| 12 | プロジェクト内と共通の 2 つの範囲を持つ | UNSUPPORTED_APPROVED（間接） | AutoGuide は指針が別領域にも効くと報告（間接）。範囲の分け方は Notion の運用（共通知識をプロジェクト配下に閉じ込めない）に倣う。 |
| 13 | 検索は全文検索（FTS5 の BM25）と B 木の索引だけを使い、ベクトル検索は入れない | RESEARCH_SUPPORTED | Thakur et al., "BEIR", NeurIPS 2021 Datasets and Benchmarks（https://neurips.cc/virtual/2021/29903）：未知の領域で BM25 は頑健、密ベクトル検索はしばしば BM25 に劣る。 |
| 14 | 検索は呼び出し側を止めない：SQLite の処理は専用の読み込み接続で別スレッドに逃がし、問い合わせごとに時間の上限を設けて進捗ハンドラで打ち切り、打ち切った印つきで途中結果を返す。件数にも上限 | UNSUPPORTED_APPROVED | 工学上の対策。SQLite の進捗ハンドラ（`sqlite3_progress_handler`）による中断。 |
| 15 | 取り出した記憶は文脈の末尾側（最新の発言の直前）に置く | RESEARCH_SUPPORTED（間接） | Lost in the Middle：中ほどの情報が最も使われにくい。差し込みは段階 4 で実装する。 |
| 16 | 根拠台帳：主張・根拠の種類・参照・状態（検証済み／未検証／仮定／承認済み）をノートとして持つ | UNSUPPORTED_APPROVED | コーディングでの効果を測った査読済み研究はない（LEDGER, arXiv 2608.18398、サーベイ arXiv 2606.04990 はいずれも査読前）。外部の手がかりのない自己修正は効かない（Huang et al., ICLR 2024、間接）。 |

検討して採らなかったもの：Generative Agents（Park et al., UIST 2023）の「関連度・新しさ・重要度」の加重和。各要素の寄与を分けた結果を原典で確認できなかったため、期間による絞り込みだけを入れる。

## 実装上の判断（このプロジェクトの判断）

- 全文検索は FTS5 の trigram 分かち書きを使う。既定の分かち書き（unicode61）は空白で語を切るため、空白のない日本語を検索できない。代わりに検索語は 3 文字以上が必要で、短い語は無視する。複数の語は OR で結び、BM25 で順位付けする。ノートの重みは題名 4、キーワード・条件・タグ 3、要約 2、本文 1。
- 16 KiB を超えるツール結果は `blobs` に移し、読み出しで元に戻す。索引に入れる文字は 1 メッセージ 64 KiB まで。思考は索引しない。
- 教訓・手順はモデル由来なら「候補」、人または検証済みなら「有効」で始まる。主張は検証済み→verified、人→approved、モデル→unverified。文脈に出すのは有効・verified・approved だけ。評価（上げ・下げ）は数えるだけで、自動の退役はしない（ExpeL の重要度の規則は原典の数値を確認していないため）。
- 連想は、リンク（向きを問わない）とタグ（タグを節点とする）のグラフで、α = 0.15、ε = 1e-4、押し出し回数の上限 20,000。節点は残差が ε に達したら列に入れ、取り出したときに次数で判定する（ハブの隣接は質量が届いたときだけ読む）。起点・退役・置き換え済み・範囲外のノートは返さない。
- 読み込みは専用の接続（最大 4 本を再利用）で別スレッドに逃がし、1000 ステップごとに締め切りを確かめて打ち切る。打ち切った問い合わせは、それまでの結果と `truncated` を返す。

## 実装・検証の状況

- 版 10：会話の番号を `session_ids`（`AUTOINCREMENT` の表）の連番から取り、いちばん新しい会話を消しても同じ番号を二度と使わない（前の版のデータベースは、そのときある番号の最大から続ける。それより前に消した番号はわからない）。ターンが残した承認待ちと質問（`waiting`）、MCP のサーバーの質問（`elicitations`）を保存する（`turn-protocol.md`）。会話を消すと、それらも消し、モデルへの要求の記録と課題の出来事は会話との結び付きを外して残す。試験：`thread_ids_are_never_given_twice`、`what_a_turn_left_waiting_and_the_questions_of_mcp_servers_outlive_a_restart`。
- 単体試験 5 件（`core/src/store.rs`）：履歴のページ送りと大きな結果の別保存（行には参照だけが残る）、日本語の全文検索とプロジェクト外の除外、教訓の範囲・状態・版（候補は出ない、置き換えた旧版は残るが出ない、共有にすると他のプロジェクトにも出る）、タグの正規化と別名、連想（1 ホップが 2 ホップより上位、無関係・退役・他プロジェクトは出ない、締め切り切れは truncated）、短い検索語の除外。
- 実測（2026-11-16、Windows 11、release、`store::measure`、手元の 1 回）：1 ターン 50 件×1000 ターンの追記 2.54 秒（1 ターン約 2.5 ms）、5 万件の履歴の全文検索 23 ms、最新 50 件のページ 0.26 ms、ノート 5000 件（全件に共通の「ハブ」タグ）での連想 4.4 ms。
- 未実装：TCP への公開（段階 3）、エージェントループでの利用と文脈への差し込み（段階 4）、モデルによる要約・キーワードの作成、ベクトル検索。
