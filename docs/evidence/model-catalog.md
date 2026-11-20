# モデルの一覧の価格・ベンチマーク・実測の速さ、と仕事に合わせたモデルの選び方

- **対象と範囲：** 接続先が出すモデルの一覧に、価格（models.dev）、ベンチマーク（LLMBenchmark）、ここで測った速さ（使用量のログ）、請求の種類、GPT の速さの区分（fast・flex）とアクセスプログラム（Daybreak など）、DeepAI の Theos、Claude のプランを付けて返す（`models/catalog`）。モデルを選ぶ画面に載せるため。
  - 範囲外（未実装）：これを使った自動の割り振り（仕事の種類・難しさ・時間帯でモデルを選ぶ、同僚を選ぶ）、Claude の CVP（Cyber Verification Program）の状態、利用者が価格を上書きする設定、用途と指標の対応の変更。
- **関連コード：** `core/src/catalog.rs`（取得と保存、照合、順位、組み立て）、`core/src/llm.rs`（`ModelInfo` の `speed_tiers`・`access_programs`・`theos_per_request`、`Llm::billing`・`claude_plan`）、`core/src/deepai.rs`（Theos）、`core/src/lib.rs`（`Core::model_catalog`）、`server/src/lib.rs`（`models/catalog`）。
- **根拠の状態：**
  - 一覧に情報を載せること：UNSUPPORTED_APPROVED（表示の機能）。
  - 自動の割り振り：未実装。研究は下の「選び方の研究」。
- **ユーザーの判断：** 2026-11-21「モデル別価格やベンチマークは旧版のcodexベースinocodeに実装あるので参考にして。性能別で呼ぶモデルを変えるべき。…根拠があった上で同僚呼ぶべき」「選択Uiにも今後モデル別の価格やベンチマーク載せる」「deepaiは消費量独自単位なので取得できるようにし、gptはdaybreakやfastどれで使えるのか,claudeはcvpどれで使えてアクセスはどんな種類化も取得必須」「寝るので自動でできることすべて実装」。

## 参考にした旧版（2026-11-21、サブエージェントが `ino-code` のソースを読み、報告）

- 価格は models.dev（`https://models.dev/api.json`、MIT）、24 時間保存。0/0 の価格は「わからない」とする（契約の提供元は 0 と書くため）。
- ベンチマークは LLMBenchmark（`https://api.llmbenchmark.ai/v1/scores?metric=…`、CC BY 4.0、出典の表示が要る）、12 時間保存。用途と指標の対応は既定の表（coding、reasoning、math、agentic、general、speed）。
- モデルの名前の照合：提供元の名前の対応、そのまま、小文字で `.` と `-` を同じとみなす、日付の後ろ部分を外す。あいまいな照合はしない。
- DeepAI は Theos（1 日の割り当て）で払う。一覧の `theosPerRequest` が `0` なら無料。価格は出さない。
- GPT：Daybreak は OpenAI のサイバーのアクセスプログラム（`standard`・`daybreakBlue`・`daybreakRed`）。ChatGPT の一覧の各モデルの `available_access_programs.cyber[]` に出る（出ていても使えるとは限らず、サーバーが確かめる）。Fast はモデルではなく速さの区分で、`service_tiers` に `priority` があるか `additional_speed_tiers` に `fast` があると使える。Flex もある。
- Claude：プランは `GET https://api.anthropic.com/api/oauth/profile`（`anthropic-beta: oauth-2025-04-20`）の `organization.rate_limit_tier`、なければ `organization_type`。CVP（Cyber Verification Program）は旧版にも取り方がなく、Claude Desktop の配布物に申請の画面があるだけ（取り方は不明）。

## 実装

- `models/catalog`（引数なし）：接続先の `models` の一覧の各モデルに、次を付ける。
  - `billing`：鍵の接続先は `api`、Claude・GPT・Muse のサインインは `plan_included`、DeepAI は `theos`（1 回あたり 0 なら `free`）。
  - `price`：models.dev の価格（100 万トークンあたりの米ドル。入力・出力・キャッシュの読み書き）と、照合した項目（`提供元/モデル`）。DeepAI では出さない。
  - `scores`：用途ごとの指標の値、単位、順位（指標に載っているモデルの中で。時間の指標は小さいほど上）、日付、出典の URL。
  - `measured`：過去 30 日の使用量のログから、件数、tok/s と最初の文字までの時間の中央値、キャッシュの割合（同じ接続先・同じモデル）。
  - `speed_tiers`（`fast`・`flex`）、`access_programs`（Daybreak など）、`theos_per_request`、`context_window`。
  - 全体に `plan`（Claude のサインインのときのプラン）、`sources`（出典と使用許諾）、`stale`（取得に失敗して古い写しを使ったとき）。
- 取得した写しは `<名前>.files/catalog/` に保存し（一時ファイルに書いてから名前を変える）、期限内はそれを使う。取得に失敗したら古い写しを使う。
- 照合は旧版の方式の一部（提供元の対応表、小文字と `.`・`-` の同一視、`provider/` の除去、日付の除去）。

## 選び方の研究（2026-11-21、サブエージェントが全文を読み、報告。自動の割り振りは未実装）

- 難しさで割り振ると費用が下がる：FrugalGPT（arXiv 2305.05176）は、正答を確かめる小さなモデルで段階的に上げ、同じ正答で費用 59〜98% 減（表 3）。RouteLLM（arXiv 2406.18665）は MT Bench で GPT-4 の品質の 95% を 3.66 倍安く（表 6）。ただし学習した振り分けは、学習と違う問いではほぼでたらめに近かった。Hybrid LLM（ICLR 2024）は、大きいモデルへの呼び出し 40% 減で品質の低下ほぼなし（モデルの差が小さいとき）。
- 振り分けの研究の総まとめ：RouterBench・RouterEval（査読前）は、理想の振り分けなら弱いモデルの組でも強いが、実際の振り分けは最良の 1 つのモデルに及ばないことが多いと報告。段階的に上げる方式は、答えの確かめの誤りが 0.1 以下のときだけ効く。
- コーディング：Zhou ほか（arXiv 2611.24324、査読前）は、小さいモデルで始め時間切れなどで大きいモデルに引き継ぐと、SWE-bench Verified で 71.2% → 73.4%、GPU 時間 −16.4%。ただし小さいモデルの答えを隠しテストで確かめており、実際の作業では使えない。SWE-Edit（arXiv 2604.26102、査読前）は、読むだけの閲覧役に GPT-5-mini を使っても GPT-5 と同じ結果（70.3% 対 70.2%）。
- サイバーセキュリティ専用のモデル：Foundation-Sec-8B（Cisco の技術報告、査読なし）は狭い脅威情報の問いで小さい汎用モデルと同程度、70B の汎用モデルには負ける。専用のモデルが最先端の汎用モデルより強いという根拠は見つからなかった。
- ベンチマークで選ぶこと：能力はおおまかには少ない次元で予測できる（Observational Scaling Laws、NeurIPS 2024）が、順位は小さな変更で動き（ACL 2024）、SWE-bench Verified には混入の疑いがある。仕事の種類ごとに順位が成績を予測するという検証はない。
- 当てはめ（推測）：読むだけの調べものに安いモデルを使うのは根拠がある（探索役の `INOCODE_EXPLORE_MODEL`）。「まず安いモデル、失敗したら上げる」は、確かめる手段（テスト）があるときだけ。専用のセキュリティモデルとベンチマークによる自動の選択は根拠が弱い。

## 検証

- 単体テスト（`catalog.rs`）：価格の照合（日付付き、`.` と `-`、0 は価格にしない）、順位（時間は小さいほど上）、DeepAI の請求と価格、名前の正規化。`llm.rs`：ChatGPT の一覧の速さの区分とアクセスプログラム、Claude のプラン。
- 手動のテスト `the_real_catalogs_answer`（2026-11-21、実際の models.dev と LLMBenchmark から取得できた。claude-haiku-4-5 は入力 $1・出力 $5・キャッシュ読み $0.1・書き $1.25）。
- 未検証：実際の ChatGPT の一覧の `service_tiers`・`available_access_programs` の形（旧版のソースの記述に合わせた）、Claude のプランの取得、`models/catalog` の結合テスト（外部に取りに行くため書いていない）。
