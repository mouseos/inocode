# PageIndex（目次の木をたどる検索）

- **対象と範囲：** VectifyAI/PageIndex の方式を inocode に入れるか。調査だけで、実装はしていない。
- **関連コード：** 未実装。
- **根拠の状態：** UNSUPPORTED_PENDING。inocode の使い方（コードの探索、会話、圧縮）に当てはまる研究は見つからなかった。近い研究（RAPTOR、MemTree）はむしろ逆の結果。採用しない。PDF を読む機能を作ることになった場合だけ、小さな比較実験を検討する。
- **ユーザーの判断：** 2026-11-21「これらの最新技術が役立つかの調査して。https://github.com/VectifyAI/PageIndex」
- **調べ方：** サブエージェントがリポジトリ、論文、inocode の記録を読んだ報告（以下、報告のまま）。LLM は動かしていない。

---


- 調査日：2026-11-21
- 対象：https://github.com/VectifyAI/PageIndex
- 範囲：読むだけの調査。コードは変えていない。LLM や有料 API は一度も動かしていない。数値はすべて資料からの転記で、自分では再現していない。
- 区別の書き方：**【論文】** 論文の結果。**【開発元】** VectifyAI の主張。**【推測】** この調査での判断。査読の有無は各論文ごとに書く。

---

## 1. PageIndex とは何か

### 1.1 概要【開発元】

- 説明文：「Document Index for Vectorless, Reasoning-based RAG」。
- 考え方：
  - 文書を細切れにして埋め込みベクトルで似ている断片を探す方式（ふつうの RAG）をやめる。
  - 代わりに、文書ごとに「目次の木」を作る。LLM がその木を見て、どの節を読むかを自分で決める。
  - README の主張：「似ている ≠ 関係がある。関係があるかを決めるには推論が要る」。
- ライセンス：MIT（GitHub API の `license.spdx_id`）。
- その他：
  - リポジトリの作成は 2025-04-01、最終 push は 2026-11-21、スター約 4.06 万。
  - 実装は Python。モデルは LiteLLM 経由で呼ぶ。
- 引用先：論文はない。README が引用先に挙げるのはブログ記事だけ（Zhang, Tang & PageIndex Team, "Next-Generation Vectorless, Reasoning-based RAG", PageIndex Blog, 2025-09）。arXiv にも査読つきの場にも、PageIndex 自体の論文は見つからなかった。

### 1.2 入力

- PDF：
  - ローカル版は、文字を持つ PDF だけを扱う。
  - スキャン画像の PDF や図の理解は、有料のクラウド版だけ（README の表「Handles: Text-based PDFs」）。
- Markdown：`run_pageindex.py --md_path` と `pageindex/page_index_md.py` の `md_to_tree` で扱える。
  - `#` の見出しを機械的に読んで木にする。
  - 要約を付けるかは選べる。
  - いまの README は PDF が中心で、Markdown についてはほとんど書いていない。
- ソースコード：対象外。コードを扱う機能も主張もない。

### 1.3 木の作り方（コードで確認）

方式は 3 つある。

1. **flash（既定。`--mode` の既定値は `flash`）**
   - `pageindex/flash/` 以下で PDF を文字単位で解析し、規則で木を作る。
   - 使う手がかり：字の大きさ・太さ、番号付け、ヘッダー・フッターの除去、段組みの分割、埋め込みの目次（`embedded_toc.py`）。
   - README：「構造は文書のレイアウトから LLM なしで取り出す」。
   - LLM の使い道は次の 2 つ：
     - 各ノードの要約を書く（`if_add_node_summary: "yes"`）。
     - 木を手直しする（`tree_optimize.py`。大きすぎるノードを LLM に分けさせる `propose_children` など）。
2. **classic（`page_index_classic.py`）**
   - 目次のページを LLM に探させる（`toc_detector_single_page`。既定で先頭 20 ページを 1 ページずつ）。
   - 次の作業も LLM が行う：
     - 目次の抜き出しと JSON への変換（途中で切れたら「続けて」と再要求）
     - ページ番号のずれの検出
     - 各見出しが本当にそのページにあるかの確認（`check_title_appearance`、見出しごとに 1 回）
   - 目次がなければ、本文を 2 万トークンずつ LLM に読ませて目次を作らせる（`generate_toc_init`・`generate_toc_continue`）。
   - LLM の呼び出しは、ページ数と見出し数に比例して増える。
3. **Markdown**：見出しから機械的に木を作る。要約を付ける場合は、ノードごとに LLM を 1 回呼ぶ。ただし 200 トークン未満のノードは本文をそのまま使う。

設定の既定値（`pageindex/config.yaml`）：

| 項目 | 既定値 |
|---|---|
| `toc_check_page_num` | 20 |
| `max_page_num_each_node` | 10 |
| `max_token_num_each_node` | 20000 |
| ノード ID・ノード要約 | 付ける |
| 文書の説明・本文 | 付けない |

各ノードが持つもの：`title`、`node_id`、`start_index`・`end_index`（ページ範囲）、`summary`、子の `nodes`。

**作る費用【開発元】**

- 索引用のモデル（`gpt-5.6-luna`）で 1 ページ約 $0.001。1,000 ページの教科書で 1 ドル少し、数分。
- 9〜1,098 ページの PDF で 13 秒〜4.5 分。
- 「索引のモデルは安いもので品質が落ちない」とも主張しているが、比べた数値は出していない。

### 1.4 取り出し方（検索）

- ベクトル検索はしない。
- LLM のエージェントが次のツールを順に呼ぶ（`pageindex/agent_tools.py`。クラウドの MCP と同じ形）。
  - `browse_documents`：文書の一覧
  - `get_document_structure`：本文を含まない木。見出し・ページ範囲・要約
  - `get_page_content`：例「5-7」ページの本文
- ツールの返答は 10 万字まで。20 ページを超える文書は、まず構造を見るよう誘導する（`STRUCTURE_FIRST_PAGE_THRESHOLD = 20`）。
- 引用はページ単位。
- 【推測】やっていることは「目次を見る → 該当ページを読む → 足りなければ戻る」を、モデルにツールでさせることそのもの。inocode の「`find`／見出し名で `read` → 窓で読む」と形はほぼ同じ。違うのは次の 2 点。
  1. PDF のページと見出しを、機械的に木にしておくこと。
  2. 各ノードに LLM の要約が付くこと。

### 1.5 主張している数値と出典

**(a) FinanceBench 98.7%【開発元】**

- 出典：
  - https://pageindex.ai/blog/Mafin2.5 （旧 URL vectify.ai/blog/Mafin2.5）
  - https://github.com/VectifyAI/Mafin2.5-FinanceBench
- 対象は Mafin 2.5（PageIndex の上に作った同社の金融 QA 製品）。PageIndex のオープンソース版そのものではない。
- 条件：
  - FinanceBench の公開セット。問題数はブログに書いていない。
  - 基のモデルは GPT-4o と DeepSeek v3。どちらも 98.7%。
  - あいまいな問題などは「専門家が人手で注釈」した、とある。
  - 採点方法の詳しい説明はない。`eval.py` は公開されている。
- 比較の相手は他社製品（Fintool 98%、Quantly 94%、Perplexity 45% など）。網羅率（66.7% など）が製品ごとに違う。
- README の図にある「ベクトル RAG 約 50%」：
  - 出典の明記はない。
  - 【推測】FinanceBench 論文（2023）の「GPT-4-Turbo＋文書ごとのベクトル索引」50% にあたる。2023 年の古いモデルと素朴な実装の値で、同じ条件の比較ではない。
- 第三者による再現は見つからなかった。

**(b) 自社のベンチマーク「PageIndex Financial QA Benchmark」【開発元】**

- 出典：https://github.com/VectifyAI/pageindex-benchmarks/tree/main/financial-qa
- 条件：
  - 8 社の 10-K（年次報告書）40 件、100 問。種類は、列挙 52、理由 28、会社をまたぐ 20。
  - LLM は 25 種・41 設定。すべて PageIndex で取り出す。各 3 回以上、採点者には設定名を伏せる。
- 結果：
  - 最高は 99.0%。
  - Claude Opus 5.5（max）は 98.7%、$128.49／100 問。
  - Claude Sonnet 5.5 は思考の深さ high で 82.0%、max で 98.0%。
  - Claude Haiku 5.5 は 68.3% から 88.7%（各 p<0.01）。
- **ベクトル RAG や全文入力との比較はこのベンチマークにはない**。ほかの取り出し方式と比べたものではなく、モデルどうしを比べたもの。

**(c) NVIDIA の 10-K で埋め込み検索と比較（cookbook）【開発元】**

- 出典：`cookbook/pageindex-vs-vector-10k/` の 2 つのノートブック。2026-10-16 の実行、`pageindex` 0.4.1。
- 条件：
  - 5 年分の 10-K（93〜169 ページ）、63 問。
  - 生成モデルは GPT-6.0 sol（medium）。
  - 比較相手は 512 トークン区切り（64 重ね）の埋め込み検索。上位 k 件 = 10/30/50/100。
  - 採点者は Claude Opus 5.5。「要点を全部述べ、矛盾なし」なら正解。5 つ（または 25）の答えを順不同・匿名で同時に見せる。
  - 開発元によると、自社の人手採点 315 件と 314 件で一致した。

| 方式 | 正答率 | 1,000 問の費用 |
|---|---|---|
| PageIndex | 100.0% | $110 |
| Gemini Embedding 2、上位 10 | 54.0% | $16 |
| Gemini Embedding 2、上位 100 | 87.3% | $135 |
| Voyage 4、上位 50 | 90.5% | 不明（図のみ） |
| Voyage 4、上位 100 | 92.1% | $134 |
| OpenAI 3-large、上位 50 | 63.5% | 不明（図のみ） |

- 限界【推測】：
  - 開発元が作った問題で、件数も少ない（63 問）。
  - 比較相手は「1 回だけ取り出して答える」素朴な RAG。キーワード検索やページを読むツールを持つエージェント（inocode に近い形）とは比べていない。
  - PageIndex 側だけが複数ターンで、キャッシュも効かせている。

**(d) PDF をまるごと渡す場合との比較【開発元】**

- 同じ答えになった文書で、PDF をそのまま渡すと 52 ページで 2.1 倍、420 ページで 16.6 倍の費用（`gpt-5.6-sol`、キャッシュなし）。805 ページでは窓に入らない。
- 精度の比較ではなく、費用だけの比較。

---

## 2. 学術的な根拠

### 2.1 PageIndex そのものの論文

- **見つからなかった。** 開発元の論文・プレプリントはない。主張は、ブログと自社のベンチマーク・ノートブックだけ。
- 第三者の評価：Hashmi, "Adaptive Query Routing: A Tier-Based Framework for Hybrid Retrieval Across Financial, Legal, and Medical Documents", arXiv 2604.14222（v2 2026-10-28）。**査読前、著者 1 人**。
  - 「木をたどる推論」とベクトル RAG を比べている。PageIndex 型と書くが、PageIndex のコードを使ったかは要旨からは不明。
  - 結果：
    - 総合の品質：木 0.900、提案手法 0.845。
    - 平均の遅さ：ベクトル 1.61 秒、木 3.40 秒。
    - FinanceBench の先頭 50 問（付属の根拠の断片を文書として使用）：木 0.938、ベクトル 0.821。
    - 採点は GPT-4o-mini。
  - 限界（著者自身の記述）：
    - 方式ごとに、文書の見せ方・プロンプト・出力の長さの上限が違う。そのため「差を取り出し方式だけのせいにはできない」。
    - 節単位の評価は、採点の誤りで v2 で取り下げた。
    - 予備的な結果。

### 2.2 同じ考え方の先行研究（LLM が要約の木をたどる）

| 研究 | 査読 | 方式 | 結果 | 条件・限界 |
|---|---|---|---|---|
| MemWalker: Chen, Pasunuru, Weston, Celikyilmaz, "Walking Down the Memory Maze: Beyond Context Limit through Interactive Reading", arXiv 2310.05029 (2023) | 掲載先の記載なし（査読前として扱う） | 長文を区切って要約の木を作り、LLM が根から節を選んで降りる。**PageIndex とほぼ同じ形** | 表 2（Stable Beluga 2 70B、窓 4,096）、Orig. / Long の正答率。QuALITY：MemWalker 67.4 / 73.6、全文（右を残す）70.1 / 72.5、検索 63.1 / 64.8。GovReport：MemWalker 59.4 / 60.4、検索 54.0 / 52.1 | 窓が 4k と短い時代の比較。QuALITY の全体では全文のほうが上。著者の挙げる限界：(1) 長い文書では木を作る負担が大きい。(2) **70B 以上の指示調整済みモデルでないと失敗が積み重なる**（13B では大きく落ちる）。(3) ゼロショットのみ |
| RAPTOR: Sarthi et al., "RAPTOR: Recursive Abstractive Processing for Tree-Organized Retrieval", ICLR 2024, arXiv 2401.18059 | 査読あり | 断片を埋め込みでまとめ、LLM（gpt-3.5-turbo）で要約して木を作る | QuALITY 82.6%（GPT-4）。QASPER F1 55.7（DPR 53.0、BM25 50.2）。**木を上からたどる方式より、全ノードを平らに検索する方式のほうが一貫して良かった**（QASPER 20 件、図 3） | 検索は埋め込みで、LLM がたどる方式ではない。作る費用は長さに比例。要約の約 4% に軽い幻覚。inocode では `repo-understanding.md` に記録済み |
| MemTree: Rezazadeh, Li, Wei, Bao, "From Isolated Conversations to Hierarchical Schemas: Dynamic Tree Memory Representation for LLMs", ICLR 2025, arXiv 2410.14052 | 査読あり | 会話の記憶を木にして逐次足す。追加 1 回あたり LLM 3.27±2.38 回 | MSC（GPT-4o）：全履歴 95.6、MemTree 84.8、MemoryStream 84.4、MemGPT 70.4。MultiHop RAG：RAPTOR 81.0、MemTree 80.5、GraphRAG 78.3。QuALITY（Llama-3.1-70B）：全文 65.1、GraphRAG 62.8、MemTree 59.8 | 検索は埋め込み（LLM はたどらない）。上からたどる方式も試し、k が小さいと精度が落ちた。**全履歴・全文には負ける**。最近の内容に偏る |
| LATTICE: Gupta, Chang, Bui, Hsieh, Dhillon, "LLM-guided Hierarchical Search for End-to-end Reasoning Intensive Retrieval", arXiv 2510.13217（v2 2026-05） | 掲載先の記載なし（査読前） | LLM が要約の階層索引を上から探す。検索時に埋め込みを使わない | BRIGHT nDCG@10 46.7（微調整した最良の組み合わせと同等）。安い検索と組み合わせると 49.1。**少ないトークン予算では、並べ替え方式のほうが良い** | 文書の集合から探す課題。木を作る費用は要旨に記載なし |
| ReadAgent（Lee et al., ICML 2024） | 査読あり | ページごとの要点を作り、必要なページを読み直す | QuALITY 86.16%（全文 85.83%、BM25 84.42%） | `repo-understanding.md` に記録済み。作るのに LLM が要る |
| DeepRead（arXiv 2602.05014） | 査読前 | 見出しの目次と、節を読むツール | 79.5%（比較 69.2%）。ただし正答 1 件あたり 6,770 トークン（比較 4,978） | `repo-understanding.md` に記録済み。目次を外す実験はない |

### 2.3 長い文書の QA と FinanceBench

- 出典：Islam, Kannappan, Kiela, Qian, Scherrer, Vidgen, "FinanceBench: A New Benchmark for Financial Question Answering", arXiv 2311.11944（2023）。**掲載先の記載なし（査読前）**。
- 条件：人手で採点した 150 問、表 2（正答率）。

| 設定 | 正答率 |
|---|---|
| GPT-4-Turbo、全文書で共有のベクトル索引 | 19%（答えられない 68%） |
| GPT-4-Turbo、文書ごとのベクトル索引 | 50% |
| Claude 2、文書を窓に入れる | 76% |
| GPT-4-Turbo、文書を窓に入れる | 79% |
| GPT-4-Turbo、根拠のページを直接渡す | 85% |

- 限界：2023 年のモデル。ベクトル索引は LangChain＋Chroma の素朴な実装。
- 【推測】PageIndex の「ベクトル RAG 約 50%」は、この 50% にあたる。

### 2.4 エージェントのキーワード検索 vs 埋め込み（文書）

| 研究 | 査読 | 結果 | 限界 |
|---|---|---|---|
| "Keyword search is all you need: Achieving RAG-Level Performance without vector databases using agentic tool use", arXiv 2602.23368 | 査読前 | キーワード検索だけのエージェントで、ふつうの RAG の指標の 9 割以上に届く | FinanceBench は 7 問だけ。文脈の細かいところを取りこぼす（著者） |
| Suresh et al., "AgenticRAG: Agentic Retrieval for Enterprise Knowledge Bases", arXiv 2605.05538 | 査読前 | search・find・open・summarize のツールで、FinanceBench 92%（根拠を直接渡した場合との差は 2 ポイント以内）。BRIGHT recall@1 は最良の埋め込みより +21.8 ポイント | 木は使わない。費用の記載なし。モデル名は要旨にない |
| Sen et al., "Is Grep All You Need? How Agent Harnesses Reshape Agentic Search", arXiv 2605.15184 | 査読前 | LongMemEval の 116 問で、grep のほうが埋め込みより概ね高い。結果はハーネス（Claude Code・Codex など）で大きく変わる | 会話の記憶の課題で、文書ではない |
| VecTree-RAG: Zhong et al., arXiv 2607.23006 | 査読前 | 埋め込みで論文と節を絞り、その中で節の木を推論でたどる。QASPER 300 問：正しさ 0.800、根拠ページの精度 0.274（比較 0.046〜0.071） | PageIndex と直接は比べていない。複数ターンは 1 回の検索より高いと著者が認める |

【推測】文書で「LLM が目次の木をたどる」方式と「LLM が grep とページ読みで探す」方式を直接比べた査読つきの研究は、見つからなかった。PageIndex の比較相手は、どれも「1 回だけ取り出す RAG」。

### 2.5 コードでの根拠（場所を探す）

すでに `docs/evidence/repo-understanding.md` と `harness-tools.md` に記録してあるものと、今回足したもの。

- 文字列での検索とファイルの読み：
  - SWE-Explore（arXiv 2606.07297、査読前）：正しいファイルに届いた割合は、BM25 0.079、埋め込み 0.088、Claude Code 0.667。
  - Agentless（FSE 2025、査読あり）：ファイル一覧から選ばせると 78.67%、埋め込みは 70.33%。
  - SWE-agent（NeurIPS 2024、査読あり）：検索結果をまとめて件数に上限を付け、100 行の窓で読む。
- 反対の向きの結果：Reddy et al., "SweRank: Software Issue Localization with Code Ranking", arXiv 2505.07849（コメント欄に ICLR 2026 の camera-ready とある）。
  - SWE-Bench-Lite の関数 Acc@10：専用に学習した埋め込み＋並べ替えで 88.69%、LocAgent（Claude-3.5）77.37%、SWE-agent（Claude-3.5）64.60%、BM25 36.86%。
  - 費用：$0.011〜0.015／件（LocAgent は $0.66）。
  - 限界：専用に学習したモデルが要る。Python のみ。場所の特定だけを測っている。
- LLM の要約の階層でコードを探す方式：
  - Rafiei Oskooei et al., "Natural Language Summarization Enables Multi-Repository Bug Localization by LLMs in Microservice Architectures", arXiv 2512.05908（ICSE 2026 の LLM4Code ワークショップ採択）。
    - ファイル・ディレクトリ・リポジトリの自然言語の要約を作り、上から絞り込む。
    - 結果：Pass@10 0.82、MRR 0.50。
    - 限界：社内の 1 システム（46 リポジトリ、110 万行）だけ。比較相手の数値は要旨にない。要約を作る費用も要旨にない。
  - Dhulshette, Shah, Kulkarni, arXiv 2501.07857（LLM4Code@ICSE 2025）：要約の網羅性を論じたもので、検索の精度の比較はない。
- `repo-understanding.md` の記録：LLM で要約やグラフを作る方式（LingmaAgent・CoSIL・SWE-Search）は、1 件 7 万トークン以上か、数百回の反復が要る。
- CodeRAG-Bench: Wang et al., "CodeRAG-Bench: Can Retrieval Augment Code Generation?", Findings of NAACL 2025, DOI 10.18653/v1/2025.findings-naacl.176（査読あり）。
  - 取り出した文脈は生成を助けることが多いが、検索器が役に立つ文脈を取りこぼす。
  - 200〜800 トークンの区切りが良いことが多い。
  - 木をたどる方式は扱っていない。

### 2.6 GraphRAG・LongRAG（参考）

- GraphRAG：Edge et al., "From Local to Global: A Graph RAG Approach to Query-Focused Summarization", arXiv 2404.16130。**掲載先の記載なし（査読前）**。
  - LLM で実体のグラフと、まとまりごとの要約を作る。
  - 100 万トークン級の「全体を問う」質問で、包括性と多様性が上がる（評価は LLM の判定）。
  - 作る費用が大きい。MemTree 論文では、作り直しに約 3,750〜3,850 回の LLM 呼び出し。
- LongRAG：Jiang, Ma, Chen, arXiv 2406.15319。**技術報告（査読前）**。
  - 取り出す単位を 4k トークンと長くし、長い窓のモデルで読む。
  - NQ 62.7% EM、HotpotQA 64.3% EM。
  - 木ではない。「細切れにしない」という点だけが PageIndex と共通する。

### 2.7 根拠の強さのまとめ

- **PageIndex そのもの**：査読つきの根拠はない。数値は、開発元のブログ・自社ベンチマーク・ノートブックと、査読前の第三者 1 本（著者自身が方式どうしの比較として不十分と書く）だけ。
- **同じ考え方（LLM が要約の木をたどる）**：
  - 査読つきの研究では、木を上からたどるより平らに検索するほうが良い（RAPTOR）。
  - 上からたどると k が小さいときに落ちる（MemTree）。
  - 査読前の MemWalker は「70B 以上の強いモデルが要る」「長い文書では木を作るのが重い」と書く。
  - 全文が窓に入るなら、全文のほうが良いか同等（MemWalker の QuALITY 全体、MemTree の QuALITY・MSC）。
- **inocode に近い比較（キーワード検索とページ読みのエージェント vs 木）**：直接の研究は見つからなかった。
- **コード**：エージェントが grep で探して窓で読む方式は、BM25 や埋め込みで 1 回取り出す方式より強いという査読前の結果が多い。LLM の要約の木で探す方式は、社内 1 件のワークショップ論文だけ。

---

## 3. inocode への当てはめ

### 3.1 いまの inocode にあるもの（コードと記録で確認）

- `find`：文字列・正規表現での検索と名前検索。一致した行に、それを含む定義の名前を付ける（`find-enclosing.md`）。
- `read`：
  - 100 行の窓で読む。
  - `name` で定義を丸ごと読む。対象は関数・型・メソッド、**Markdown の見出し**（`core/src/read.rs` の説明文と `core/src/outline.rs` の「document headings」）。
  - 画像も読める。
  - **PDF は読めない**（`read.rs`・`fetch.rs` に PDF の処理はない）。
- `outline`：一度実装したが、実測で正確さに差がなく、トークンが増えたので外した（`repo-understanding.md`）。
- `explore`：読むだけの探索役。場所と行だけを返す（`core/src/explore.rs`）。
- `fetch`：Web ページの本文を Markdown にする。長いものは先頭 100 行を見せ、残りはファイルに置いて `read` させる（`core/src/fetch.rs`）。
- 過去の会話・記憶：
  - SQLite の FTS5（BM25）で全文検索する（`project-memory.md` 判断 13）。ベクトル検索は入れていない。
  - `recall` は、カーネルの実験 335 ターンで 0 回しか呼ばれていない（`memory-integration.md`）。
- 圧縮：いまの要約の方式で、決めたことは 8/8 保たれた（`compaction-retention.md`）。

### 3.2 使い道ごとの判断

| inocode の使い道 | PageIndex の考え方が足すもの | 根拠 | 判断【推測】 |
|---|---|---|---|
| ソースコードの場所を探す | 要約の木（ディレクトリ → ファイル → 関数）を LLM がたどる | 弱い。コードでは grep と窓での読みが強いという結果が多い（SWE-Explore、Agentless、SWE-agent）。要約の木は社内 1 件のワークショップ論文だけ。`outline`（機械的な骨格）は inocode の実測で効果がなかった。要約を作る LLM の費用がかかり、コードが変わるたびに作り直しが要る | **役に立たない見込み**。`find`・`read`・`explore` で足りている |
| 長い Markdown・テキスト文書を読む | 見出しの木を見せ、節を選んで読む | 見出しの木と節の読み出しは、すでに `read` の `name`（見出しで読む）と `outline.rs` で機械的にできる。LLM の要約を足す効果の根拠は弱い（RAPTOR では木をたどるより平らな検索が上） | **すでにほぼある**。足すとしても見出しの一覧だけで、それは削った `outline` と同じ性質 |
| PDF（論文・データシート・仕様書・年次報告書）を読む | 文字の PDF から、LLM なしで見出しとページの木を作り、ページ範囲で読む | PageIndex の主張は開発元のものだけ。ただし「目次で節を選んでから読む」は DeepRead（査読前）で正答率が上がり、トークンも増えた。PDF を窓に入れきれないときに全文より安いのは理屈どおり【推測】。inocode はいま PDF をまったく読めない | **ここだけは候補になりうる**。ただし PDF を読む機能そのものが新機能で、研究の確認と承認が先（AGENTS.md 第 3 節） |
| 過去の会話・記憶を取り出す | 会話を木にまとめ、上からたどる | MemTree（ICLR 2025）でも、全履歴には負け、ふつうの記憶とほぼ同じ（84.8 対 84.4）。しかも検索は埋め込み。追加 1 回に LLM 約 3 回。inocode では `recall` 自体がほとんど使われていない | **やらない** |
| 長い会話の圧縮 | 要約を階層にする | 圧縮の追加案は、費用だけ増えて効果がなかった（`compaction-retention.md`）。区切った要約（本の題材）はすでに入っている | **やらない** |
| 長い Web ページ・文書サイト | ページの見出しの木 | `fetch` の Markdown を保存すれば、`read` の見出し読みが使える。LLM の要約を足す根拠はない | **新しい仕組みは不要** |

### 3.3 費用と遅さ【開発元の数値と推測】

- 作る費用：
  - flash は構造を作るのに LLM を使わない。ただし要約に 1 ページ約 $0.001（開発元）。
  - classic は目次の検出・変換・各見出しの確認・目次がないときの生成で、LLM 呼び出しが多い。
- 引く費用：1 問ごとに複数ターンのやり取りになる。NVIDIA の例では 1 問約 $0.11（GPT-6.0 sol）。素朴な RAG の上位 10 件（$0.016）の約 7 倍。
- 遅さ：Hashmi の査読前の比較で、木 3.40 秒、ベクトル 1.61 秒。
- inocode の場合【推測】：
  - モデルはもともと自分でツールを呼んで探している。「目次を出す → ページを読む」も、いまのツールの組み合わせでできる。
  - PageIndex の検索エージェントを別に回すと、`explore` と同じ「下請けのモデル呼び出し」が 1 段増える。
  - 要約を作る費用は、文書ごとに前払いになる。

### 3.4 Python で書かれていること

- PageIndex は Python で、LiteLLM と pdfium を使う。
- 【推測】inocode は Rust で、依存を増やさない方針（AGENTS.md 第 1 節）。取り入れるなら、次のどちらかになる。
  - 考え方（レイアウトから見出しの木を作る規則）だけを Rust で作り直す。
  - プラグインか MCP として外に置く。
- PageIndex のクラウドの MCP は有料で、文書を外部に送る。

---

## 4. 結論と勧め

- **根拠の状態：** PageIndex そのものの有効性は、査読つきの根拠がない（開発元の主張だけ）。
  - 同じ考え方の査読つきの研究（RAPTOR、MemTree）は、むしろ次のことを示す。
    - 「木を上からたどるより平らな検索」
    - 「入るなら全文」
  - 査読前の MemWalker は「強いモデルが要る」と書く。
  - inocode の状態の区分では、取り入れる場合は `UNSUPPORTED_PENDING` にあたる。
- **inocode への勧め：今は取り入れない。**
  - コードの探索、過去の会話、圧縮：取り入れない。根拠が弱く、`find`・`read`・`explore`・`recall` と重なる。`outline` の実測とも食い違う。
  - Markdown・テキストの長い文書：`read` の見出し読みで、すでに目的のほとんどを満たしている。
- **唯一の候補：PDF を読むこと。**
  - 小さな実験を考える価値はある。
  - ただし PDF の読み込み自体が新機能で、AGENTS.md 第 3 節の手順（研究の確認 → ユーザーの承認）が先。
  - 実験するなら、次を比べる。
    1. PDF をページ単位のテキストにして、`find`・`read` で探させる
    2. 1 に、LLM なしで作った見出しとページの一覧を足す
    3. 2 に、LLM の要約を足す
  - 測るもの：正答率とトークン。
  - 実行は、ユーザーの許可のあとで、許可されたモデルだけにする。
  - PageIndex の数値は、比較相手が素朴な 1 回取り出しの RAG なので、inocode の比較対象（エージェントの grep と読み）には当てはまらない。

## 5. 主な出典

- PageIndex：
  - https://github.com/VectifyAI/PageIndex （README、`pageindex/config.yaml`、`page_index_classic.py`、`page_index_md.py`、`flash/`、`agent_tools.py`、`cookbook/pageindex-vs-vector-10k/`）。MIT。
- Mafin 2.5（FinanceBench 98.7%）：
  - https://pageindex.ai/blog/Mafin2.5
  - https://github.com/VectifyAI/Mafin2.5-FinanceBench
  - 開発元の主張。
- PageIndex Financial QA Benchmark：https://github.com/VectifyAI/pageindex-benchmarks/tree/main/financial-qa （開発元）
- FinanceBench：Islam et al., arXiv 2311.11944（2023、査読前）。https://arxiv.org/abs/2311.11944
- RAPTOR：Sarthi et al., ICLR 2024（査読あり）。https://arxiv.org/abs/2401.18059
- MemTree：Rezazadeh et al., ICLR 2025（査読あり）。
  - https://arxiv.org/abs/2410.14052
  - https://proceedings.iclr.cc/paper_files/paper/2025/hash/0382cb76309820f71c6eacd47b36ce71-Abstract-Conference.html
- MemWalker：Chen et al., arXiv 2310.05029（掲載先の記載なし、査読前として扱う）。https://arxiv.org/abs/2310.05029
- LATTICE：Gupta et al., arXiv 2510.13217（査読前）。https://arxiv.org/abs/2510.13217
- GraphRAG：Edge et al., arXiv 2404.16130（査読前）。https://arxiv.org/abs/2404.16130
- LongRAG：Jiang et al., arXiv 2406.15319（技術報告、査読前）。https://arxiv.org/abs/2406.15319
- Adaptive Query Routing：Hashmi, arXiv 2604.14222（査読前）。https://arxiv.org/abs/2604.14222
- VecTree-RAG：Zhong et al., arXiv 2607.23006（査読前）。https://arxiv.org/abs/2607.23006
- Keyword search is all you need：arXiv 2602.23368（査読前）。https://arxiv.org/abs/2602.23368
- AgenticRAG：Suresh et al., arXiv 2605.05538（査読前）。https://arxiv.org/abs/2605.05538
- Is Grep All You Need?：Sen et al., arXiv 2605.15184（査読前）。https://arxiv.org/abs/2605.15184
- SweRank：Reddy et al., arXiv 2505.07849（ICLR 2026 の camera-ready とコメント欄にある）。https://arxiv.org/abs/2505.07849
- Rafiei Oskooei et al.：arXiv 2512.05908（ICSE 2026 の LLM4Code ワークショップ）。https://arxiv.org/abs/2512.05908
- CodeRAG-Bench：Wang et al., Findings of NAACL 2025。https://doi.org/10.18653/v1/2025.findings-naacl.176
- inocode の既存の記録：
  - `docs/evidence/repo-understanding.md`（RAPTOR・ReadAgent・DeepRead・Agentless・`outline` の実測）
  - `docs/evidence/harness-tools.md`（SWE-Explore など）
  - `docs/evidence/memory-integration.md`
  - `docs/evidence/compaction-retention.md`
  - `docs/evidence/project-memory.md`

## 6. 確かめていないこと

- PageIndex のコードを実行していない。木の質・費用・速さは、どれも開発元の数値。
- Mafin 2.5 の採点方法（`eval.py` の中身）は読んでいない。
- MemWalker の表は PDF からの文字抜き出しで、列が崩れていた。行の対応は本文の並び順から判断した。
- AgenticRAG・Keyword search・Is Grep・SweRank の一部は要旨か HTML の表だけで確認し、本文の条件は読み切っていない。
- MemWalker・LATTICE・GraphRAG が、その後どこかの査読つきの場に載ったかは確認できなかった（arXiv のページに記載なし）。
