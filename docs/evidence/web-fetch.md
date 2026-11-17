# Web ページ取得ツール：設計（承認待ち）

- **対象と範囲：** モデルが URL のページを読むツール。無駄な HTML を読ませずに圧縮する。うまくいかないときや HTML の構造を見たいときは、全体も取れるようにする（2026-11-17 のユーザー指示）。
- **関連コード：** `core/src/fetch.rs`（`fetch`）。`core/src/agent_tools.rs` で登録する。保存先はデータベースの横の `<データベース名>.files/fetch/thread-<スレッド>/`。
- **根拠の状態：** 本文抽出は RESEARCH_SUPPORTED。出力の形式（Markdown）は UNSUPPORTED_APPROVED。抽出器はプロジェクト内のベンチマークで選んだ（研究ではない）。

## 研究（調査日 2026-11-17）

| 主張 | 査読 | 出典と数値 | 限界 |
|---|---|---|---|
| 本文抽出器は、HTML を単に文字にするより大幅に正確。平均 F1：Trafilatura 0.883、Readability 0.861、単純な文字化 0.664〜0.683。Readability はどの複雑さでも最も安定 | 査読あり | Bevendorff, Gupta, Kiesel, Stein, "An Empirical Comparison of Web Content Extraction Algorithms", SIGIR 2023, DOI 10.1145/3539618.3591920。8 つのデータセット、3,985 ページ | 古い英語のページが中心。正解は平文で、構造は評価していない |
| 生の HTML は 1 ページ平均 8 万トークン超。90% 超が CSS・JS・コメント | 査読あり | Tan et al., "HtmlRAG", WWW 2025, arXiv 2411.02959 | 質問応答のみ。モデルは Llama のみ |
| 生の HTML は正答率が明らかに低い。例：70B で NQ が 46.75（整えた HTML は 58.00）、HotpotQA が 28.00（同 42.25） | 査読あり | 同上 | 同上 |
| 整えた HTML と平文はほぼ同等。Markdown はわずかに低い。70B の NQ Hit@1：整えた HTML 58.00、平文 59.75、Markdown 57.00。HotpotQA EM：42.25 / 41.00 / 39.00 | 査読あり | 同上 | 差は数ポイントで、有意差の検定はない |
| 小さな生成モデルで Markdown に書き直すと（Jina Reader 1.5B）、BM25 並みに落ちる。NQ 48.25、HotpotQA 34.25 | 査読あり | 同上 | 4K トークンの予算の場合 |
| 新しい抽出器の比較：rs-trafilatura 0.903、Trafilatura 0.841、dom_smoothie 0.817、Readability 0.736（テスト集合の F1） | 査読前 | Foley, "WCXB", arXiv 2605.21097 | rs-trafilatura の作者による評価 |
| 抽出器はコードに弱い。Trafilatura の編集類似度は 0.13 | 査読前 | Ma et al., "AICC", arXiv 2511.16397 | 著者が自分のツールを評価 |
| 隠し要素を消毒すると、隠しテキストによる注入の成功率が下がる。平均 24.9% → 消毒で 13.1% → 対策をすべて組み合わせて 4.7%。ゼロ幅文字は Unicode 正規化が要る | 査読不明 | Guo, Wei, "Hidden-in-Plain-Text", arXiv 2601.10923 | 判定は自動 |
| タグを外しただけの平文は、隠れた指示を普通の文にしてしまう。注入に従った割合：平文 3.9%、HTML 1.1% | 査読前 | Khodayari et al., "Indirect Prompt Injection in the Wild", arXiv 2604.27202 | 抽出器や Markdown 変換は試していない |

根拠が見つからなかったもの：
- 質問を添えて小さなモデルにページを要約させる方式（Claude Code の WebFetch）の正確さ。幻覚や細部の欠落を測った研究がない。
- Rust の HTML→Markdown 変換の評価。

## 設計案

1. `fetch`：`url`、`mode`（`main`／`text`／`html`）。
   - `main`（既定）：隠し要素を消してから本文を抽出し、見出し・リンク・コードを残した短い文章にする。
   - `text`：ページ全体を文章にする。本文の抽出に失敗したときに使う。
   - `html`：生の HTML を返す。
2. どのモードでも、取得したページの全体をファイルに保存する。
   - 長い結果は冒頭だけ返し、続きはファイルから読む（シェルのログと同じ考え方）。
   - 一度に大量の HTML がモデルに入らない。
3. 結果は区切り記号で囲み、外部のデータであると示す（`web-search.md` と同じ）。
4. 小さなモデルによる要約は入れない（根拠なし）。

## 未決（承認が要る）

1. **`main` の出力形式**（研究は「整えた HTML ≈ 平文 > Markdown」で、差は数ポイント）：
   - Markdown：コードやリンクが読みやすい。研究ではわずかに低い。
   - 平文：研究では最も良いか同等。見出しやコードの区切りが失われる。
2. **抽出器**：
   - `dom_smoothie`（Readability.js の移植、MIT、2026-10 更新）：Readability は査読ありの研究で最も安定。
   - `rs-trafilatura`（MIT/Apache-2.0、2026-04 更新）：数値は高いが、評価が作者自身のもの。
3. **長い結果の上限**（研究なし）：冒頭何字までを返すか。

## 抽出器のベンチマーク（プロジェクト内の実測、2026-11-17）

- 手順とコード：`refs/extract-bench`（git 管理外）。
- 評価方法：
  - 単語の袋の適合率・再現率・F1。小文字にし、`\w+` で区切る。WCXB の `evaluate.py` と同じ方法。
  - 出力が空、エラー、panic のページは F1 を 0 とする。
  - 正解が空の 16 ページは除く。
- 主な結果は、どの抽出器の作者とも無関係なデータで測った。
  - Bevendorff et al. の統合データ（SIGIR 2023）、3,975 ページ。
  - 各抽出器は Markdown を出力する設定で動かした（llm_readability は Markdown の設定がないため文章で出力）。

| 抽出器 | 平均 F1 | 中央値 F1 | F1=0 | ms/ページ |
|---|---|---|---|---|
| html2text（抽出なし） | .694 | .737 | panic 4 | 3.4 |
| **dom_smoothie 0.19.0（MIT）** | **.879** | **.964** | 0 | 2.6 |
| rs-trafilatura 0.2.2 | .768 | .928 | 空 153 | 12.1 |
| trafilatura 0.6.0（nchapman） | .847 | .944 | 0 | 5.9 |
| llm_readability 0.0.17 | .730 | .946 | 空 504 | 3.3 |

- 補助：WCXB のテスト集合 505 ページ。rs-trafilatura の作者が作り、開発集合で調整したデータ。
  - 平均 F1：rs-trafilatura .860、dom_smoothie .831、trafilatura .830。
  - 文書（documentation）の 42 ページ：dom_smoothie .874、rs-trafilatura .951、trafilatura .950。
- コードの残り方：WCXB の文書 20 ページの `<pre>` を調べた。
  - 残ったコードのトークンの割合：dom_smoothie .890、trafilatura .897、rs-trafilatura .919、llm_readability .368。
- 選んだもの：`dom_smoothie`。
  - 作者と無関係なデータで最も正確。
  - 失敗が 0 件で、最も速い。
- 限界：
  - 文書ページでは trafilatura 系が上だった（作者が作った補助データでの結果）。
  - 時間は 1 回だけの計測。
  - 言語の内訳は測っていない。空白で区切らない CJK では、この評価方法は不正確になる。

## 実装（工学上の選択で研究なし）

- `fetch`：`url`、`mode`（`main`／`text`／`html`、既定は `main`）。
- 取得：http と https だけ。30 秒で打ち切り、上限は 10MB。
- 取得した本体は、加工せずに必ずファイルに保存する。`main` と `text` では、変換した結果も `.md` に保存する。
- モデルに返すもの：URL、状態コード、種類、題名、保存先、結果の冒頭 100 行（`read` と同じ窓）。続きは `read` で読む。
- `main`：
  - 隠し要素を消してから、`dom_smoothie` で本文を Markdown にする。消すのは `hidden`、`aria-hidden=true`、`template`、インラインの `display:none`・`visibility:hidden`。
  - 本文が見つからないときは、ページ全体を文章にして、そうしたと書き添える。
- `text`：隠し要素を消してから、`html2text` でページ全体を文章にする。
  - `html2text` は一部の表で panic するため、その場合は HTML をそのまま返して、そうしたと書き添える。
- ゼロ幅文字（U+200B〜U+200F、U+2060〜U+2064、U+FEFF）を消す（Guo, Wei の対策の一部）。
- 結果は区切り記号で囲む。
- HTML でない文書（JSON・テキストなど）はそのまま返す。画像などの文書でないものは、保存先と大きさだけを返す。
- テスト：
  - `fetch.rs` の単体テスト：リンク・コードが残り、隠しテキスト・ナビゲーション・フッターが消えること。冒頭 100 行の表示。
  - `turn_tcp.rs` の `a_fetched_page_comes_as_its_main_content_and_is_saved_whole`：ローカルのサーバーからサーバー経由で取得する。
- 未対応：
  - 外部の CSS で隠した要素は見えない（CSS を計算しないため）。
  - JavaScript で描くページは中身が取れない。

## ユーザーの承認

- 2026-11-17：設計案と未決 3 点への回答。
  1. `main` の出力形式は Markdown（UNSUPPORTED_APPROVED）。
     - 理由（ユーザー）：文字だけではリンクや画像が欠けるため、結局 HTML 全体を読みに行って無駄になる。
     - 研究では平文が数ポイント上だが、有意差の検定はない。研究で測ったのは質問応答だけで、リンクをたどる作業は測っていない。
  2. 抽出器は Rust のクレートをベンチマークして決める（`refs/extract-bench`、結果はこの文書に記録）。
  3. 長い結果はシェルのログと同じ扱いにする。全体をファイルに保存し、モデルには冒頭とファイルの場所だけを返して、続きは読むツールで読ませる。
