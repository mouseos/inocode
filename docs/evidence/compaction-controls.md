# 要約の指示・自動の要約の入り切り・要約の通知

- **対象と範囲：** 要約（`compaction-cost-and-overflow.md` の仕組み）を、Claude Code と同じように人が扱えるようにする。要約の仕組みそのもの（いつ・何を残すか）は変えない。
  - `/compact [instructions]`：人が書いた指示を、要約の依頼の最後に `The person asked this summary to keep, above all: …` として付ける。キャッシュを使う 1 回の依頼、置き換える部分だけの依頼、区切った依頼のどれにも付ける。指示が無ければ今までと同じ。
  - 自動の要約の入り切り：設定ファイルの `compaction.auto`（`settings/set` の `compaction` の部分でも変えられる）。`false` のとき、ターンは自分では要約しない。窓を超えれば、プロバイダーの誤りでターンが失敗する。`/compact` はどちらでも使える。無ければ入り。設定は、要約が要る時にだけ読む。
  - 通知：
    - ターンの中の自動の要約：始めるときにターンのイベント `compaction_started`、終わったら `compaction_ended`（`result: {seq}`）。`seq` は保存した要約のメッセージの番号で、置き換えるものが無かったときと要約できなかったときは `null`（要約できないときは、呼び出しの記録が代わりに保存される。そのときも `null`）。
    - `/compact`：同じ 2 つのイベントを、スレッドの購読（`thread/subscribe`）へ送る。`command/run` の答えは今までどおり `{compacted, seq}`。
  - 範囲外・限界：
    - Claude Code の、自動の要約を始める大きさの設定（`autoCompactWindow`）、`PreCompact`・`PostCompact` のフック（フックは別に作る）、要約の前の警告は、ここでは作っていない。
    - 指示が要約に効くかどうかは、確かめていない。
- **関連ソースコード：**
  - `core/src/compaction.rs`：`focused`、`summary`・`compact_reporting`（`focus`）。
  - `core/src/turn.rs`：`TurnEvent::CompactionStarted`・`CompactionEnded`、`Thread::compact_if_due`・`compact_due`、`compact_now`（`focus`）。
  - `core/src/commands.rs`：`/compact` の `instructions`、`Core::compact`。
  - `core/src/settings.rs`：`Compaction`、`Settings::compaction`、`SECTIONS`。
  - `core/src/lib.rs`：`EventPayload::compaction`。
  - テスト：`server/tests/compaction_tcp.rs` の `the_named_model_summarizes_in_parts_when_the_history_exceeds_its_window`（通知）、`compact_keeps_what_its_instructions_name_above_all`、`with_automatic_compaction_off_a_turn_does_not_compact`。
- **根拠の状態：** UNSUPPORTED_APPROVED。
  - 要約の方式の研究（2026-11-28、作業者が頼んだ調べもの）：コーディングでは、古い道具の結果を隠す方式が、要約と同じくらいの成功率で費用が約半分という結果がある（Lindenbauer ほか、arXiv 2508.21433、DL4C ワークショップ・NeurIPS 2025 併設、査読は本会議ではない）。一方、自分で要約する方式より良いものは無く、多く捨てると最大 20 点下がる、という結果もある（Salle ほか、arXiv 2610.31778、査読前）。手元の実測では、毎回の送信で隠す方式はキャッシュを壊し、キャッシュ外の入力が中央値 142k → 911k に増えた（`observation-masking.md`）。根拠の強さは中くらい以下で、方式は変えていない。
  - 人が書く要約の指示、自動の要約の入り切り、通知の効き目を直接調べた研究は、見つからなかった。
- **ユーザーの承認：** 2026-11-28。
  - 「要約（長くなった会話を縮める処理）で、何を足しますか？」に「根拠付きの最も効率的なのを示して」と答えた。
  - 研究の結果を示したうえでの「要約で作るものを選んでください」に「claudeと同じで」と答えた。
  - Claude Code の機能から、`/compact` の指示・自動の要約の入り切り・始まりと終わりの通知を選んだこと、設定の名前、イベントの名前と形は、作業者（Claude）が決めた。

## ほかのハーネス（2026-11-28、作業者が頼んだ調べものの報告）

| ハーネス | 要約の扱い |
|---|---|
| Claude Code | `/compact [instructions]`、自動の要約は既定で入り（`autoCompactEnabled`・`autoCompactWindow`・`DISABLE_AUTO_COMPACT`）、`PreCompact`・`PostCompact` のフック |
| Codex | TUI の `/compact`、自動の要約の大きさは `model_auto_compact_token_limit`。app-server の `thread/compact/start` と `contextCompaction` の項目 |

## 検証

- テストで確かめたこと：
  - 自動の要約で `compaction_started` の後に要約のメッセージ、その後に `compaction_ended` が届き、`seq` がそのメッセージの番号と同じ。
  - `/compact keep the test results` の要約の依頼に、指示の文が入り、`compacted: true` が返る。
  - `compaction.auto` が `false` のとき、窓に近づいても要約の依頼も通知も出ない。
- 確かめていないこと：`/compact` の通知が購読に届くこと、本物のモデルが指示を守ること。
