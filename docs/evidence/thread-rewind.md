# 巻き戻し（前の自分の発言を書き直す）

- **対象と範囲：**
  - 要求 `thread/rewind {thread_id, seq}`：人のメッセージの番号 `seq` を付けると、そのメッセージとその後ろを全部消す。
    - そのメッセージに付いた返信の引用（`reply` の種類の inocode のメッセージ）も一緒に消す。
    - 返す値は `{thread_id, from, removed, text}`。`from` は消した最初の番号、`removed` は消した数、`text` はそのメッセージの文。画面は `text` を入力欄に戻す。
    - 消したものは戻せない（ユーザーの選択）。
    - 最後のターンが残した承認待ちの呼び出しと質問も消す。
    - 約束（standing instructions）のうち、消したメッセージから引いたものは消す。
    - 計画と目標は今のまま。
    - `thread/subscribe` の購読者には、`rewound {from}` と `status idle` を送る。
    - ターンが走っている間は `turn_running` で断る。番号が人のメッセージでなければ `invalid_params`。
  - 範囲外：
    - ファイルの変更を戻すこと（洗い出しの ③）。
    - スラッシュコマンド。
  - 限界：消した後に足すメッセージは、消した番号を使い直す（番号は、スレッドの中のいちばん大きい番号の次）。画面は `rewound` を受けたら、`from` から後ろを捨てる。
- **関連ソースコード：**
  - `core/src/lib.rs`：`Core::rewind_thread`、`Activity::Rewound`、`EventPayload::rewound`。消す位置は、フォークと同じ `fork_end` で決める。
  - `core/src/store_threads.rs`：`rewind_session`。メッセージ、検索の索引、待っているもの、約束を、1 つのトランザクションで直す。
  - `server/src/lib.rs`：`thread/rewind` と `rewound` の変換。
  - テスト：`server/tests/turn_tcp.rs` の `a_rewind_deletes_from_a_message_of_the_person_and_returns_its_text`。
- **根拠の状態：** UNSUPPORTED_APPROVED。画面の操作で、効き目を測る学術研究は当てはまらない。調べたのは、ほかのハーネスの実装だけ。
- **ユーザーの承認：** 2026-11-27。
  - 足りない機能の洗い出しで「急ぐ群から順に」と答えた。
  - 「② 巻き戻し：戻した後ろのメッセージはどうしますか？」に「消す（元に戻せない。Codex と同じ）」と答えた。
  - 要求の名前と返す値、約束の扱いは、作業者（Claude）が決めた。

## ほかのハーネス（2026-11-27、調査用のエージェントがソースと資料を読み、報告。inocode の作業者は直接読んでいない）

| ハーネス | 巻き戻し |
|---|---|
| Codex（openai/codex bee5dc55ac） | `thread/revert {beforeTurnId}` が、そのターンから後ろを消し、`thread/reverted` を送る。前は `thread/rollback`（最後の N ターンを消す）だった。どちらもファイルは戻さない。TUI は Esc を 2 回押して発言を選ぶと、その文が入力欄に戻る |
| opencode（49b9463） | `revert/stage`・`revert/commit`・`DELETE revert` の 3 段階で、確定するまでは「やり直し」で戻せる。ファイルの変更も戻せる |
| Claude Code（公式の資料） | `/rewind`（Esc を 2 回）で、会話だけ、コードだけ、両方のどれを戻すかを選べる。会話を戻すと、選んだ発言が入力欄に戻る |

## 検証

- 上のテストで確かめたこと：
  - 2 つ目の人のメッセージで巻き戻すと、その文が返る。
  - そのメッセージに付いた返信の引用も消え、1 つ目のターンだけが残る。消した数も合う。
  - 購読者に `rewound {from}` と `status idle` が届く。
  - 消したメッセージから引いた約束は消え、残りの約束はそのまま残る。
  - 人のメッセージでない番号と、番号の無い要求は断られる。
- 確かめていないこと：
  - 圧縮の後の巻き戻し。
  - 消した後の番号の使い直しが、画面で問題にならないか。
