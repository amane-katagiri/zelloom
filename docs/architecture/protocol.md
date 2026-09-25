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
- `shutdown` が成功すると、core は応答を返したあと待ち受けを止めてソケットファイルを消し、runner を含むすべての接続を閉じてから終了する（[scheduler.md](scheduler.md#core-の起動と終了)）。

## Task

`list` などが返すタスクの形。`agent` / `reply_to` は null なら省略、`source.id` / `source.sender` も同様。

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
  "updated_at": "2026-09-25T16:00:00+09:00"
}
```

- `id` は ULID。`position` はグローバルキューの順序（小さいほど先）。
- `status` は `received` / `queued` / `running` / `done` / `failed` / `cancelled` / `rejected` / `interrupted`。
- 時刻はローカルタイムゾーンのオフセット付き RFC 3339（秒精度）。

## リクエスト一覧

「許可される状態」を満たさない場合は `ok: false` とエラーメッセージが返り、何も変わらない。状態の検査と更新は、許可される状態を条件にした 1 つの SQL 文で行う（`complete` / `fail` / `cancel` / `accept` / `reject` / `retry` / `delete` / `edit` と `agent_exited`）。別の操作が先に状態を変えていれば、その時点の状態に対するエラーになり、何も変わらない。

主なエラーメッセージ:

| op | 状態が合わないとき | タスクが無いとき |
|---|---|---|
| `complete` / `fail` / `cancel` | `task '<id>' is '<status>'; only <許可される状態> tasks can be marked <status>`（検査後に変わった場合は `task '<id>' changed state concurrently; try again`） | `task '<id>' not found` |
| `delete` | `cannot delete a running task` | 同上 |
| `edit` | `cannot edit a running task` | 同上 |
| `accept` / `reject` | `task '<id>' is '<status>', not 'received'` | 同上 |
| `retry` | `task '<id>' is '<status>'; only interrupted, failed or cancelled tasks can be retried` | 同上 |

| op | フィールド | 許可される状態 | 成功時の `data` | 備考 |
|---|---|---|---|---|
| `enqueue` | `text`, `workspace`, `agent`?, `source`?（既定 `{"type":"cli"}`）, `reply_to`?, `metadata`?（既定 `{}`） | — | 作成した Task | `workspace` がその時点の設定に登録されていなければ `workspace '<id>' is not registered`、`agent` が指定されていて `[agents]` に無ければ `agent '<name>' is not defined in [agents]`。`sources.<type>.auto_queue` が false なら `received`、それ以外は `queued` |
| `list` | — | — | 全 Task の配列（`position` 昇順、全状態） | |
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

### runner → core

```json
{"op": "agent_exited", "task_id": "01K5Q3...", "outcome": "done"}
```

- agent が `loom done` なしで終了し、runner のプロンプトで人間が選んだ結果を送る。`done` → `done`、`failed` → `failed`、`restart` → 同じタスクの `start` がもう一度来る（状態は `running` のまま）。
- agent を起動できなかったとき（argv が空、`/dev/tty` を開けない、spawn の失敗）や、プロンプトを出せなかったときは `failed` を送る。
- `restart` の再解決に失敗したときは、タスクが `failed` になり、エラー応答が返る（`start` は来ない）。
- core は、そのタスクが `running` で、かつこの接続の runner の現在のタスクである場合だけ受け付ける。`stop` と行き違いになった `agent_exited`（TUI から完了させた直後にプロンプトで `d` を押した場合など）はエラーになり、確定済みの状態は変わらない。`restart` も同じ条件で、満たさなければ `start` を送らない。runner はエラー応答を表示するだけで待機を続ける。

接続が切れると、core はその runner が実行中だったタスクを `interrupted` にする（[scheduler.md](scheduler.md)）。
