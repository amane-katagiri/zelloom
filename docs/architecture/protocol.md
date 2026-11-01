# IPC プロトコル

core と各クライアントは Unix domain socket 上の JSON Lines で通信する。型は `src/protocol.rs`、サーバ側は `src/core/mod.rs`、クライアントは `src/client.rs`。

## 基本

- 1 行 1 メッセージ（UTF-8 の JSON + `\n`）。
- リクエストは `op` フィールドで種類を示す（snake_case）。
- 応答は次のどちらか。`data` / `error` は無ければ省略される。

```json
{"ok":true,"data":{...}}
{"ok":true}
{"ok":false,"error":"task '01K...' not found"}
```

- 1 つの接続で複数のリクエストを順に送ってよい。応答は送った順に 1 行ずつ返る。CLI と TUI は 1 リクエストごとに接続し直している。
- JSON として読めない行には `{"ok":false,"error":"invalid request: ..."}` を返し、接続は維持する。
- 1 行（改行を含む）は 1 MiB（1048576 バイト）まで。超える行を受けた core は `request line exceeds 1048576 bytes; closing the connection` を返して接続を閉じる（通常の接続・runner 接続・最初のメッセージのどれでも同じ）。タスクの本文に他の上限は無いので、`enqueue` の本文は JSON にしたリクエスト全体がこの上限に収まる範囲に限られる。
- 最初のメッセージが `runner_attach` なら runner 接続（[下記](#runner-接続)）、`watch` なら watch 接続（[下記](#watch-接続)）になり、以後は 1 リクエスト 1 応答の接続ではなくなる。
- `shutdown` が成功すると、core は応答を返したあと待ち受けを止めてソケットファイルを消し、runner・watch を含むすべての接続を閉じてから終了する（[scheduler.md](scheduler.md#core-の起動と終了)）。

## Task

`list` などが返すタスクの形。`agent` / `reply_to` / `attention` / `turn` は null なら省略、`source.id` / `source.sender` も同様。

```json
{
  "id": "01K5Q3...",
  "text": "READMEの設定例を修正する",
  "status": "queued",
  "workspace": "myapp",
  "agent": "codex",
  "source": {"type": "cli"},
  "metadata": {},
  "position": 12,
  "created_at": "2026-09-25T16:00:00+09:00",
  "updated_at": "2026-09-25T16:00:00+09:00",
  "attention": {"id": 41, "kind": "question", "message": "SQLite と Postgres のどちらにしますか", "at": "2026-09-25T16:05:00+09:00"},
  "turn": "idle"
}
```

- `id` は ULID。`position` はグローバルキューの順序（小さいほど先）。
- `status` は `received` / `queued` / `running` / `done` / `failed` / `cancelled` / `rejected` / `interrupted`。
- 時刻はローカルタイムゾーンのオフセット付き RFC 3339（秒精度）。
- `attention` は `running` のタスクが `report` で報告した「今なにを待っているか」。`kind` は `question`（質問への答え）/ `gate`（進めてよいかの判断）/ `blocked`（権限ダイアログなど、文字を打ち込んではいけない状態）/ `ready`（ターンを終えて次の指示かレビューを待っている）。`id` は報告ごとに core が振る番号（core 全体で単調増加する u64。`send` の `expect` に使う）、`message` は報告の本文、`at` は報告した時刻。core のメモリにだけあり、SQLite には保存しない（[scheduler.md](scheduler.md#attention)）。
- `turn` は `running` のタスクの agent がターンの途中（`busy`）か、ターンを終えて入力欄で待っている（`idle`）か。`running` のタスクにだけ付く。agent 自身の hook が `turn` で知らせる（[scheduler.md](scheduler.md#turn)）。

## リクエスト一覧

「許可される状態」を満たさない場合は `ok: false` とエラーメッセージが返り、何も変わらない。状態の検査と更新は、許可される状態を条件にした 1 つの SQL 文で行う（`complete` / `fail` / `cancel` / `accept` / `reject` / `retry` / `delete` / `edit` と `agent_exited`）。別の操作が先に状態を変えていれば、その時点の状態に対するエラーになり、何も変わらない。

主なエラーメッセージ:

| op | 状態が合わないとき | タスクが無いとき |
|---|---|---|
| `complete` / `fail` / `cancel` | `task '<id>' is '<status>'; only <許可される状態> tasks can be marked <status>`（検査後に変わった場合は `task '<id>' changed state concurrently; try again`） | `task '<id>' not found` |
| `get` | — | `task '<id>' not found` |
| `delete` | `cannot delete a running task` | 同上 |
| `edit` | `cannot edit a running task` | 同上 |
| `accept` / `reject` | `task '<id>' is '<status>', not 'received'` | 同上 |
| `retry` | `task '<id>' is '<status>'; only interrupted, failed or cancelled tasks can be retried` | 同上 |
| `report` | `task '<id>' is '<status>', not 'running'` | 同上 |
| `turn` | `task '<id>' is '<status>', not 'running'` | 同上 |
| `send` | 本文の規則（[下記](#send-の本文)）に反する: 規則ごとのメッセージ。`running` でない: `task '<id>' is '<status>', not 'running'`。runner が未接続: `no runner is attached for workspace '<ws>'`。runner の現在のタスクでない: `task '<id>' is not the current task of the runner for workspace '<ws>'`。同じタスクへの `send` が配送中: `a reply to task '<id>' is still being delivered`。返信を待っていない: `task '<id>' is not waiting for a reply (attention: blocked)` / `(no attention)`。`expect` が今の報告と違う: `the reply is for report <expect>, but the current report of task '<id>' is <attention.id>; read it before replying`。ターン中: `task '<id>' is still in its turn (turn: busy); wait until its agent ends the turn`。報告のあとにターンを終えていない: `task '<id>' has not ended its turn since report <expect>; wait until it does`。配送の結果は[下記](#send-の配送) | 同上 |

| op | フィールド | 許可される状態 | 成功時の `data` | 備考 |
|---|---|---|---|---|
| `enqueue` | `text`, `workspace`, `agent`?, `source`?（既定 `{"type":"cli"}`。`loom add --source <type>` は `type` を指定する）, `reply_to`?, `metadata`?（既定 `{}`） | — | 作成した Task | `workspace` がその時点の設定に登録されていなければ `workspace '<id>' is not registered`、`agent` が指定されていて `[agents]` に無ければ `agent '<name>' is not defined in [agents]`。`sources.<type>.auto_queue` が false なら `received`、それ以外は `queued` |
| `list` | — | — | 全 Task の配列（`position` 昇順、全状態） | |
| `get` | `task_id` | — | その Task | `loom show` が使う |
| `complete` | `task_id` | `running`, `interrupted` | 更新後の Task（`done`） | `running` だった場合は runner に `stop` を送る |
| `fail` | `task_id` | `running`, `interrupted` | 更新後の Task（`failed`） | 同上 |
| `cancel` | `task_id` | `running`, `interrupted`, `queued`, `received` | 更新後の Task（`cancelled`） | 同上 |
| `delete` | `task_id` | `running` 以外 | なし | 行を削除する |
| `edit` | `task_id`, `text` | `running` 以外 | 更新後の Task | 本文だけを書き換える |
| `move` | `task_id`, `direction`（`up` / `down`） | `queued`, `received` | 更新後の Task | `queued` と `received` をまとめた `position` 順の列で隣と位置を入れ替える。端では何もしない |
| `accept` | `task_id` | `received` | 更新後の Task（`queued`） | |
| `reject` | `task_id` | `received` | 更新後の Task（`rejected`） | |
| `retry` | `task_id` | `interrupted`, `failed`, `cancelled` | 更新後の Task（`queued`） | キューの末尾（最大 `position` + 1）に移す |
| `status` | — | — | 下記 | |
| `shutdown` | `force`?（既定 false） | — | なし | core を終了させる。`force` が false で `running` のタスクがあれば拒否する（下記） |
| `runner_attach` | `workspace` | — | なし | 接続の最初のメッセージでだけ受け付け、以後その接続は runner 接続になる（[runner 接続](#runner-接続)） |
| `report` | `task_id`, `kind`（`question` / `gate` / `blocked` / `ready`）, `message` | `running` | 更新後の Task | 新しい `attention.id` を振ってタスクの `attention` を置き換え（前の報告は消える）、`turn` を `busy` にする。`message` は末尾の空白を除く。空でもよい。末尾の空白を除いて 16 KiB（16384 バイト）を超える、または NUL を含むと拒否する。それ以外の制御文字はそのまま持つ（JSON でエスケープされる）。`running` の確認と更新はストアのロックの中で行う |
| `turn` | `task_id`, `state`（`busy` / `idle`） | `running` | 更新後の Task | タスクの `turn` を変える。`idle` にするたびに、その時点の core の通し番号を「ターンを終えた印」として記録する（[scheduler.md](scheduler.md#turn)）。`turn` の値が変わったときだけ watch に流れる |
| `send` | `task_id`, `expect`（返信する報告の `attention.id`。必須）, `text` | `running`、その workspace の runner の現在のタスク、配送中の `send` が無い、`attention.kind` が `question` / `gate` / `ready`、`attention.id` が `expect`、`turn` が `idle` で、かつその報告のあとに `idle` になっている | 更新後の Task（`attention` なし、`turn` は `busy`） | 本文を[規則](#send-の本文)で検査し、`attention` を消して `turn` を `busy` にしてから runner に `input` を送り、runner の `input_result` を待って応答する（[下記](#send-の配送)）。成功すると `attention` が消えるので、次の `report` までもう一度は送れない |
| `watch` | — | — | — | 接続の最初のメッセージでだけ受け付け、以後その接続は watch 接続になる（[watch 接続](#watch-接続)）。最初でなければ `watch must be the first message on a connection` を返し、接続はそのまま続く |
| `input_result` | `task_id`, `input_id`, `outcome`（`typed` / `not_typed` / `partial`）, `error`? | その `input_id` の `send` が送信元 runner の結果を待っている | なし | runner 接続からのみ受け付ける（それ以外では `input_result is only accepted on a runner connection`）。待っている `send` が無ければ（時間切れのあとなど）`no reply <input_id> to task '<id>' is waiting for a result from this runner` |
| `agent_gone` | `task_id` | `running`、かつ送信元 runner の現在のタスク | 更新後の Task | runner 接続からのみ受け付ける（それ以外では `agent_gone is only accepted on a runner connection`）。agent が自分で終了したことを知らせる。`attention` を消し、`turn` を `busy` にするので、以後 `send` は通らない |
| `agent_exited` | `task_id`, `outcome`（`done` / `failed` / `restart`） | `running`、かつ送信元 runner の現在のタスク | 更新後の Task（`restart` では変更なしの Task） | runner 接続からのみ受け付ける。それ以外の接続では `agent_exited is only accepted on a runner connection`。`restart` で agent を再解決できなければタスクを `failed` にしてエラーを返す（[scheduler.md](scheduler.md#runner-の管理)） |

`status` の `data`:

```json
{
  "tasks_by_status": {"done": 3, "queued": 1, "running": 1},
  "runners": [{"workspace": "myapp", "attached": true, "current_task": "01K5Q3..."}],
  "workspaces": ["myapp", "other"],
  "tui_default_workspace": "myapp",
  "zellij_session_name": "main"
}
```

`runners` は接続中の runner だけを含み（`attached` は常に true）、`current_task` は無ければ null。`workspaces` は core がその時点で読んだ設定の workspace ID 一覧（設定が読めなければ null）。`tui_default_workspace` は設定の `tui.default_workspace`（未設定または設定が読めなければ null）。`zellij_session_name` は core 起動時の `ZELLIJ_SESSION_NAME`（無ければ null）。

`force` なしの `shutdown` が `running` のタスクのために拒否されたときは、`data` に実行中のタスクを入れて返す。確認とシャットダウンの開始は scheduler のロックの中で行うので、確認の直後に別のタスクが始まることはない。

```json
{"ok":false,"error":"1 task(s) are running","data":{"running":[{...Task...}]}}
```

各操作がどの状態遷移を起こすかは [scheduler.md](scheduler.md#状態遷移) にまとめている。

`turn` と `report` は、ソケットに届くプロセスならどのタスクに対しても送れる（タスクごとのトークンは無い）。

### `send` の本文

CRLF と CR を LF にし、末尾の空白を除いてから次を検査する。どれかに当たれば拒否し、本文を書き換えて通すことはしない。

| 規則 | エラー |
|---|---|
| 空 | `reply text is empty` |
| 16 KiB（16384 バイト）を超える | `reply text is <n> bytes; the limit is 16384` |
| LF 以外の C0 制御文字（Tab・ESC・Ctrl-C など）、DEL、C1 制御文字（U+0080〜U+009F）を含む | `reply text contains the control character U+<XXXX>; only line breaks are allowed` |
| 最初の空白でない文字が `!` | `reply text must not start with '!': the agent would run the rest as a shell command` |
| 最初の空白でない文字が `/` | `reply text must not start with '/': the agent would run it as a slash command` |

### `send` の配送

`send` を受け付けると、core は通し番号から `input_id` を振って runner に `input` を送り、同じ runner 接続で `input_result` が返るのを待つ（既定 15 秒、`ZELLOOM_INPUT_TIMEOUT_MS` で上書き。core 起動時に読む）。待つ間は scheduler と runners のロックを持たない。

| runner の結果 | `send` の応答 | `attention` と `turn` |
|---|---|---|
| `typed` | 成功（更新後の Task） | 消えたまま / `busy` |
| `not_typed` | `the reply was not typed into task '<id>'[: <error>]; its report is open again`（新しい報告が来ていた・agent が終了していたときは末尾が `its report was replaced or the agent is gone`） | 待っている間に新しい報告が無ければ元の `attention` に戻し、`turn` を `idle` にする |
| `partial`、時間切れ、runner の切断 | `delivery of the reply to task '<id>' is unconfirmed: <理由>; check its tab before replying again` | 戻さない（打ち込まれたかどうかを人間がタブで確かめる） |

どの結果でも、そのタスクの「配送中」は解ける。

## watch 接続

外部のプロセス（エージェントたちを監督する別のセッションなど）がタスクの変化を追うための push ストリーム。接続の最初のメッセージとして `{"op":"watch"}` を送ると、core は応答行を返さずに次を送る。

1. スナップショット: 状態が `received` / `queued` / `running` / `interrupted` のタスクごとに 1 行（`position` 昇順）。
2. `{"event":"synced"}`。
3. 以後の変化（デルタ）。

```text
client → core  {"op":"watch"}
core → client  {"event":"task","task":{...Task...}}
core → client  {"event":"task","task":{...Task...}}
core → client  {"event":"synced"}
               ... 以後の変化 ...
core → client  {"event":"task","task":{...Task...}}
core → client  {"event":"task_deleted","task_id":"01K5Q3..."}
```

- `task` は、タスクが作られた・状態が変わった・本文が変わった・`position` が変わった・`attention` か `turn` が変わった（`report` / `turn` / `send` / `agent_gone` / 消去）ときに、変化後の Task 全体を送る。デルタは状態を問わない（`done` になったタスクもその変化の 1 行が届く）。`move` では位置を入れ替えた 2 件のタスクがそれぞれ届く。
- `task_deleted` は `delete` で行が消えたとき。
- スナップショットの取得と購読はストアのロックの中で同時に行い、すべての変化はそのロックの中で送られる（[scheduler.md](scheduler.md#watch-イベント)）。スナップショットとデルタの間で変化を取りこぼすことも、二重に受け取ることもなく、デルタは変化が確定した順に届く。
- 遅れて読み切れなくなった（core 側のバッファ 1024 件を超えて溜まった）クライアントには `{"event":"error","error":"watch stream fell behind by <n> event(s); reconnect to get a fresh snapshot"}` を送って接続を閉じる。クライアントは接続し直してスナップショットから取り直す。
- スナップショットを含む watch 接続へのすべての書き込みに時間制限がある（10 秒。`ZELLOOM_WATCH_WRITE_TIMEOUT_MS` で上書き。core 起動時に読む）。超えたら（クライアントが読まずに送信バッファが詰まったなど）core は何も送らずに接続を閉じる。
- watch 接続でクライアントが送った行は読み捨てる。クライアントが書き込み側を閉じる（EOF）と core は接続を閉じる。
- core の `shutdown` でも閉じられる。

## runner 接続

runner は接続直後に `runner_attach` を送る。core は `{"ok":true}` を返し、以後この接続にイベントを push する。

- `runner_attach` は接続の最初のメッセージ（JSON として読めなかった行も 1 つと数える）でなければならない。既に何かを送った通常の接続で送ると `runner_attach must be the first message on a connection` を返し、接続は通常の接続のまま続く。
- runner 接続の上でもう一度 `runner_attach` を送ると `connection is already attached as runner for <workspace>` を返す。登録は変わらない。

1 つの workspace に接続できる runner は 1 つだけ。同じ workspace の runner が接続中のときに `runner_attach` が来ると、core は `{"ok":false,"error":"a runner for workspace '<id>' is already attached"}` を返して接続を閉じる。先に接続している runner の登録と実行中のタスクには影響しない。

同じ接続から通常のリクエスト（`agent_exited` など）も送れる。その応答とイベントは同じストリームに混ざって届き、クライアントは `event` キーの有無で区別する（`ServerMessage` は untagged enum）。

```text
runner → core  {"op":"runner_attach","workspace":"myapp"}
core → runner  {"ok":true}
core → runner  {"event":"start","task":{...Task...},"agent":{"argv":[...],"cwd":"...","env":{...},"shell":true,"oneshot":false}}
               ... agent 実行中 ...
core → runner  {"event":"stop","task_id":"01K5Q3..."}
```

### core → runner イベント

`start`:

```json
{
  "event": "start",
  "task": {"id": "01K5Q3...", "text": "フィード周りを修正する", "status": "running", "workspace": "myapp", "source": {"type": "cli"}, "metadata": {}, "position": 3, "created_at": "...", "updated_at": "..."},
  "agent": {
    "argv": ["claude", "--append-system-prompt", "You are running inside zelloom.\n...", "フィード周りを修正する"],
    "cwd": "/path/to/myapp",
    "env": {
      "RUST_LOG": "debug",
      "ZELLOOM_SOCKET": "$XDG_RUNTIME_DIR/zelloom/default.sock",
      "ZELLOOM_TASK_ID": "01K5Q3...",
      "ZELLOOM_WORKSPACE": "myapp"
    },
    "shell": true,
    "oneshot": false
  }
}
```

`agent` は core 側で解決済み。runner は設定ファイルを読まない。

- `argv`: agent 自身の argv（組み立ては [config.md](config.md#agent-の解決と-argv)）。`shell` が true でもシェルのラッパは含まない。
- `cwd`: workspace の `path`。
- `env`: agent に設定する環境変数。設定の `env` と `ZELLOOM_*` をマージ済み（[config.md](config.md#agent-の環境変数)）。
- `shell`: 設定の `agents.<name>.shell`。true なら runner が自分の `$SHELL` の対話モードで `argv` を包んで起動する（[runner.md](runner.md#タスクの実行)）。ラッパは core ではなく runner が、runner 自身の `$SHELL` で組み立てる。
- `oneshot`: 設定の `agents.<name>.oneshot`。true なら runner は自発終了時にプロンプトを出さず、終了コードで結果を決める（[runner.md](runner.md#終了後の分岐)）。

`start` が送られるのは次のとき。

- scheduler がタスクを開始し、その workspace の runner が接続済みで何も実行していない。
- runner が接続した時点で、その workspace に `running` のタスクがあり（タブを起動して接続を待っていた場合）、設定から workspace と agent が解決できる。
- runner が `agent_exited` の `restart` を送った（同じタスクで、設定を読み直して再解決した `agent` を送る）。この `start` は `restart` への応答より先に届く。

`stop`:

```json
{"event": "stop", "task_id": "01K5Q3..."}
```

`complete` / `fail` / `cancel` で `running` のタスクを終わらせたとき、その runner の現在のタスクであれば送る。状態は `stop` を送る時点で core が確定済みで、runner は `stop` による終了では `agent_exited` を送らない。

`input`:

```json
{"event": "input", "task_id": "01K5Q3...", "input_id": 42, "text": "SQLite でお願いします"}
```

`send` が受け付けられたときに送る。`text` は検査・正規化済み（[`send` の本文](#send-の本文)）。runner は agent のペインに文字を打ち込んで Enter を押し（[runner.md](runner.md#返信の入力)）、`input_result` を返す。

### runner → core

```json
{"op": "agent_exited", "task_id": "01K5Q3...", "outcome": "done"}
```

- agent が `loom done` なしで終了し、runner のプロンプトで人間が選んだ結果を送る。`done` → `done`、`failed` → `failed`、`restart` → 同じタスクの `start` がもう一度来る（状態は `running` のまま）。
- agent を起動できなかったとき（argv が空、`/dev/tty` を開けない、spawn の失敗）や、プロンプトを出せなかったときは `failed` を送る。
- `restart` の再解決に失敗したときは、タスクが `failed` になり、エラー応答が返る（`start` は来ない）。
- core は、そのタスクが `running` で、かつこの接続の runner の現在のタスクである場合だけ受け付ける。`stop` と行き違いになった `agent_exited`（TUI から完了させた直後にプロンプトで `d` を押した場合など）はエラーになり、確定済みの状態は変わらない。`restart` も同じ条件で、満たさなければ `start` を送らない。runner はエラー応答を表示するだけで待機を続ける。

```json
{"op": "input_result", "task_id": "01K5Q3...", "input_id": 42, "outcome": "typed"}
{"op": "input_result", "task_id": "01K5Q3...", "input_id": 42, "outcome": "not_typed", "error": "no agent is running here"}
```

- `input` ごとに 1 回送る。`typed` は本文と Enter の両方を打ち込めた、`not_typed` は何も打ち込んでいない、`partial` は途中まで打ち込んだかどうか分からない（本文の書き込みが時間切れ、または本文のあとの Enter が失敗）。`error` は理由（`typed` では省略）。

```json
{"op": "agent_gone", "task_id": "01K5Q3..."}
```

- agent が自分で終了したとき、終了時のプロンプトや oneshot の結果を送る前に送る。

接続が切れると、core はその runner が実行中だったタスクを `interrupted` にし、その runner の結果を待っている `send` を「配送は未確認」で終わらせる（[scheduler.md](scheduler.md)）。
