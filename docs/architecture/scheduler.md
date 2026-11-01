# Scheduler とタスクの状態

実装は `src/core/scheduler.rs`（各操作とスケジューリング）、`src/core/agent.rs`（agent の argv・環境変数・instruction の組み立てと `send` / `report` の本文の検査）、`src/core/mod.rs`（接続処理と起動時処理）、`src/store.rs`（SQLite）。

## 永続化

- SQLite（`state.db`）の `tasks` テーブル 1 つ。列は `id, text, status, workspace, agent, source(JSON), reply_to(JSON), metadata(JSON), position, created_at, updated_at`。
- `position` はグローバルキューの順序。新規作成と `retry` では「全タスクの最大 `position` + 1」を振る。`move` は隣のタスクと値を入れ替える。
- 接続は 1 本を `std::sync::Mutex` で守って使う。同じ Mutex の中に、タスクごとの `attention`（[下記](#attention)）と turn の記録（[下記](#turn)）を `HashMap` で持ち、`attention.id` などに使う core 全体の通し番号（u64、1 ずつ増える）も持つ。どれも SQLite に書かない。ストアが返す Task にはこれらから `attention` と `turn` を付ける。

## キューの順序と開始条件

キューは workspace で分けないグローバル 1 本。`schedule()` は次を繰り返す。

1. 設定を読み直し、全タスクを `position` 昇順で取得する。
2. `running` の数が `scheduler.max_parallel` 以上なら終わる。
3. `running` のタスクがある workspace を使用中とみなす（workspace ロック）。
4. `queued` のうち、使用中でない workspace の先頭のタスクを 1 つ選んで開始する。無ければ終わる。

- workspace ロックは独立した表を持たず、「その workspace に `running` のタスクがあるか」で判定する。runner の接続待ちのタスクも `running` なので、ロックと全体上限の両方に数えられる。
- `schedule()` 全体は `scheduling_lock`（`tokio::sync::Mutex`）で直列化される。別々の接続から同時に呼ばれても同じタスクを二重に開始しない。
- `schedule()` を呼ぶのは次の後: `enqueue`（`queued` になったとき）、`complete` / `fail` / `cancel`、`accept`、`retry`、`agent_exited`（`done` / `failed`、および再解決に失敗した `restart`）、runner の接続（その workspace に `running` が無いとき）、runner の切断、attach タイムアウトで interrupted にしたとき。
- `schedule()` の中（`scheduling_lock` を保持している間）からは `schedule()` を呼ばない。続きの判定は外側のループが次の周回で行う。

## タスクの開始（`start_task`）

1. workspace が設定に無い、agent が解決できない、解決した agent が `[agents]` に無い、のいずれかならタスクを `failed` にして終わる（理由は core のログにだけ出る）。
2. agent の argv・cwd・env を解決し（[config.md](config.md#agent-の解決と-argv)、[config.md](config.md#agent-の環境変数)）、タスクを `running` にする。
   - 1 と 2 の更新は `queued` からの条件付き更新。判定の後に別の接続から `cancel` などされて `queued` でなくなっていれば何もせず、次の周回に進む。
3. その workspace の runner が接続済みで何も実行していなければ、`start` を送り、runner の現在のタスクとして記録して終わる。
4. そうでなければ、そのタスクの launch generation を 1 増やしてから `TabLauncher::launch` を呼ぶ（Zellij ではタブを作って runner を起動する。[zellij.md](zellij.md)）。
   - 起動に失敗したら、その launch generation の CAS ガード（`interrupt_if_still_waiting`）によりすぐにタスクを `interrupted` にする。`schedule()` は再帰呼び出しせず、外側の `schedule()` のループがそのまま次の候補へ進む。
   - `launch` 自体は `tokio::task::spawn_blocking` で実行されるが、その完了は `.await` で待つため、その間も外側の `scheduling_lock` は保持されている。
   - 起動できたら、attach タイムアウト（既定 20 秒、`ZELLOOM_ATTACH_TIMEOUT_MS` で上書き）のタイマーを張る。
5. runner が接続してくると、その workspace の `running` タスクに対して `start` が送られる（[protocol.md](protocol.md#runner-接続)）。

### attach タイムアウト

タイマーが発火したとき、次をすべて満たす場合に限り `interrupted` にする。

- そのタイマーの generation が、そのタスクの現在の generation と一致する（`retry` などで後から起動し直した場合、古いタイマーは何もしない）。
- その workspace の runner が、このタスクを現在のタスクとして持っていない。
- ストア上の状態がまだ `running` である（`update_status_if(id, running → interrupted)` による条件付き更新。すでに `done` などになっていれば何もしない）。

interrupted にした場合は `schedule()` を呼ぶ。

## runner の管理

- core は workspace ごとに「接続中の runner」と「その runner の現在のタスク」を持つ（メモリ上のみ）。
- 登録には接続ごとの attach ID（core 内の連番）を持たせる。切断時の後始末や `agent_exited` の検証は、この ID が一致する登録に対してだけ行う。
- runner が接続したとき: その workspace に登録済みの runner があれば、新しい接続を拒否する（エラー応答を返して接続を閉じる。先の runner には触れない）。無ければ登録する。その workspace に `running` のタスクがあれば（`position` 順で最初のもの）、workspace と agent が解決できた場合にそれを現在のタスクにして `start` を送る（解決できなければ何もせず、attach タイムアウトで `interrupted` になる）。無ければ `schedule()`。
- runner が切断したとき: 登録がその接続のもの（attach ID が一致）なら外し、その runner の `input_result` を待っている `send` の登録を捨て、現在のタスクがまだ `running` なら `interrupted` にする。そのあと `schedule()`。
- `agent_gone` / `input_result`: `agent_gone` は送信元の接続がその workspace の登録済み runner で、タスクがその runner の現在のタスクのときだけ受け付け、attention を消して turn を `busy` に作り直す。`input_result` は[下記](#turn)。
- `complete` / `fail` / `cancel` で `running` のタスクを終えたとき: そのタスクが runner の現在のタスクなら `stop` を送り、現在のタスクを空にする。
- `agent_exited`: 送信元の接続がその workspace の登録済み runner で、タスクがその runner の現在のタスクで、かつ `running` のときだけ受け付ける。`done` / `failed` は `running` からの条件付き更新で状態を変え、runner の現在のタスクを空にする。`restart` は設定を読み直して agent を再解決し、同じ runner に `start` を再送する。条件を満たさなければエラーを返し、何も変えない。
- `restart` で再解決に失敗した（設定が読めない、workspace が設定に無い、agent が解決できない、解決した agent が `[agents]` に無い）ときは、タスクを `running` からの条件付き更新で `failed` にし、runner の現在のタスクを空にしてから `schedule()` を呼び、`cannot restart task '<id>': <理由>; the task was marked failed` のエラーを返す（条件付き更新が当たらなかった場合は末尾が `the task is no longer 'running'`）。runner はエラーを表示して待機を続ける。
- 状態を変える操作はすべて、許可される状態を条件にした 1 つの SQL 文で更新する（[protocol.md](protocol.md#リクエスト一覧)）。`complete` / `fail` / `cancel` / `accept` / `reject` は `update_status_if`、`delete` は `delete_unless`（`running` 以外なら削除）、`edit` は `update_text_unless`（`running` 以外なら本文を更新）、`retry` は `retry_if`（`interrupted` / `failed` / `cancelled` なら `queued` にし、同じ文の中で最大 `position` + 1 を振る）。

## attention

`running` のタスクが「今なにを待っているか」を 1 件だけ持てる（形は [protocol.md](protocol.md#task)）。メモリ上にだけあり、core を再起動すると消える（そのとき `running` のタスクはどのみち `interrupted` になる）。

- 設定: `report`。ストアのロックの中でタスクが `running` であることを確かめ、通し番号を 1 進めた値を `attention.id` にして置き換え、turn を `busy` にする（`Store::set_attention`）。`running` でなければエラーで、何も変えない。
- 消去:
  - タスクの状態が `running` から変わったとき、経路によらず（`complete` / `fail` / `cancel`、`agent_exited` の `done` / `failed`、runner の切断、attach タイムアウト、タブ起動の失敗、再解決に失敗した `restart`）。状態の条件付き更新（`update_status_if`）が、行き先が `running` でなければ同じロックの中で attention と turn の記録を消す。`retry` と `delete` も消す。core 起動時の `interrupted` 化はすべて消す。
  - `agent_exited` の `restart` を受け付けたとき（`start` を再送する前。再送する Task には `attention` が無く、turn は `busy`）と、`agent_gone` を受け付けたとき（`Store::reset_turn`）。
  - `send` を受け付けたとき（下記）。

## turn

`running` のタスクごとに、turn の状態（`busy` / `idle`）、最後に `idle` になったときの通し番号（ターンを終えた印）、配送中の `send` の `input_id` を持つ。

- タスクが `running` になったとき（`update_status_if` の行き先が `running`）に `busy`・印なし・配送中なしで作り、`running` から離れたときに attention と一緒に消す。`restart` と `agent_gone` でも作り直す。
- `turn idle`: 通し番号を 1 進めた値を印にして `idle` にする。`turn busy` と `report`: `busy` にする（印はそのまま）。
- `send` は、runners のロックの中で runner の現在のタスクを確かめ、そのままストアのロックの中で「`running`・配送中なし・`attention.kind` が `question` / `gate` / `ready`・`attention.id == expect`・`idle` で印が `attention.id` より大きい」を確かめ（`Store::begin_send`）、attention を消して `busy` にし、通し番号から `input_id` を振って配送中にする。続けて同じ runners のロックの中で `input` を送り、ロックを放してから結果を待つ。確認と更新が 1 つのロックの中なので、同時に 2 つの `send` が来ても通るのは 1 つだけ。
- 結果を待つ `send` は scheduler の表（`input_id` → 送信先 runner の attach ID と結果の受け口）に登録する。runner 接続の `input_result` は、attach ID とタスクが一致する登録にだけ結果を渡す。runner が切断すると、その attach ID の登録をすべて捨てる（待っている `send` は切断として終わる）。
- 結果を受けたら配送中を解く（`Store::finish_send`）。`not_typed` のときは、配送中がまだその `input_id` で、かつ attention が空（新しい報告が来ていない）なら、元の attention に戻し、通し番号を 1 進めた値を印にして `idle` にする（`Store::abort_send`）。`agent_gone` や `restart`、`running` から離れたことで記録が作り直されていれば何もしない。

## watch イベント

watch 接続（[protocol.md](protocol.md#watch-接続)）に流す変化は、scheduler の各操作ではなくストアの更新メソッドが出す。ストアは `tokio::sync::broadcast` の送信側（容量 1024）を持ち、行を変えた更新（`insert` / `update_status_if` / `update_text_unless` / `retry_if` / `move_task` / `delete_unless`）と `attention` / `turn` の変更（`set_attention` / `set_turn`（値が変わったときだけ）/ `reset_turn` / `begin_send` / `abort_send`）が、ロックを持ったまま変化後の Task（削除なら `task_deleted`）を送る。条件に合わず何も変えなかった呼び出しは何も送らない。

- 状態を変える経路はすべてストアの更新を通るので、`schedule()` の中での開始・`failed` 化、attach タイムアウト、runner の切断なども漏れなく届く。
- 送信がロックの中なので、イベントは更新が確定した順に並ぶ。`Store::watch` は同じロックの中で購読を始め、スナップショット（`received` / `queued` / `running` / `interrupted`）を読むので、その間の変化を取りこぼさない。
- core 起動時の `mark_all_running_interrupted` は送らない（その時点で購読者はいない）。
- 購読者が 1024 件より遅れると、その接続は `broadcast` の `Lagged` を受け、エラー行を送って閉じる。他の購読者や送信側は待たされない。watch 接続は 1 リクエスト 1 応答の接続のような送信キューを持たず、ソケットに直接書く。書き込みが詰まったまま 10 秒（`ZELLOOM_WATCH_WRITE_TIMEOUT_MS`）を超えた接続は、書き込みを打ち切って閉じる。

## 状態遷移

```text
  enqueue(auto_queue=false)          enqueue(auto_queue=true)
          │                                   │
          ▼             accept                ▼
      received ──────────────────────────▶ queued ◀──────────── retry ─────────┐
          │                                   │                                │
          │ reject                            │ schedule()                     │
          ▼                                   ▼                                │
      rejected                             running ── runner 切断 / attach ──▶ interrupted
  (received / queued から cancel で cancelled へ)
                                              │       タイムアウト / 起動失敗 /    │
                                              │       core 再起動                 │
                  complete, fail, cancel,     │                                   │ complete, fail,
                  agent_exited(done/failed)   │                                   │ cancel
                                              ▼                                   ▼
                                     done / failed / cancelled ◀──────────────────┘
                                             (failed / cancelled からも retry で queued へ)
```

すべての遷移:

| 元の状態 | 契機 | 次の状態 |
|---|---|---|
| （新規） | `enqueue`、source の `auto_queue` が true（既定） | `queued` |
| （新規） | `enqueue`、source の `auto_queue` が false | `received` |
| `received` | `accept` | `queued` |
| `received` | `reject` | `rejected` |
| `received` / `queued` | `cancel` | `cancelled` |
| `queued` | `schedule()` が選び、workspace・agent が解決できた | `running` |
| `queued` | `schedule()` が選んだが workspace / agent が解決できない | `failed` |
| `running` | `complete` / `fail` / `cancel` | `done` / `failed` / `cancelled`（runner に `stop`） |
| `running` | `agent_exited`（`done` / `failed`） | `done` / `failed` |
| `running` | `agent_exited`（`restart`） | `running` のまま（`start` を再送） |
| `running` | `agent_exited`（`restart`）で agent を再解決できない | `failed` |
| `running` | runner の切断（そのタスクを実行中だった） | `interrupted` |
| `running` | runner の接続待ちで attach タイムアウト | `interrupted` |
| `running` | タブ起動（`TabLauncher::launch`）の失敗 | `interrupted` |
| `running` | core の起動（前回の core が残した `running` すべて） | `interrupted` |
| `interrupted` | `complete` / `fail` / `cancel` | `done` / `failed` / `cancelled`（`stop` は送らない） |
| `interrupted` / `failed` / `cancelled` | `retry` | `queued`（キュー末尾） |
| `running` 以外 | `delete` | 行ごと削除 |

- `agent_exited` は、`running` で、かつ送信元 runner の現在のタスクにだけ効く。
- `edit`（`running` 以外）と `move`（`queued` / `received`）は状態を変えない。
- `done` と `rejected` から出る遷移は `delete` だけ。

## core の起動と終了

- 起動時、ソケットファイルが既にあれば接続を試みる。接続できれば「別の core が動いている」としてエラー終了、できなければ古いファイルとして消す。
- 続けて設定ファイルを読み、`[http]` があれば TCP リスナーを bind する（[http.md](http.md#ライフサイクル)）。設定の読み込みか bind に失敗すると core は起動しない。
- DB を開いた直後に `running` のタスクをすべて `interrupted` にする。
- `shutdown` は `force` が false なら `running` のタスクがあるとき拒否する（[protocol.md](protocol.md#リクエスト一覧)）。受け付けると以後 `schedule()` は何もしない（新しいタスクの割り当ても タブの起動もしない）。
- 受け付けたあとは待ち受けを止めてソケットファイルを消し、開いている接続（watch 接続を含む）をすべて閉じ、接続処理の終了を待ってから終了する。runner の接続は切断として扱われるので、実行中だったタスクはその場で `interrupted` になる。runner が未接続のまま `running` だったタスク（attach 待ち）は残り、次の起動時に `interrupted` になる。
- HTTP サーバが起動していれば、同じ shutdown の通知（`subscribe_shutdown`）を使って axum の graceful shutdown を行う。`core::run` はソケットの後始末が終わったあと、HTTP サーバのタスクが終わるのを待ってから戻る（[http.md](http.md#ライフサイクル)）。
- 切断された runner は、待機中ならすぐ終了し、agent 実行中なら agent が終わるのを待ってから終了する（[runner.md](runner.md#core-との切断)）。
