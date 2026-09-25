# zelloom 初期実装計画

本書は 2 部構成。第 1 部は元の設計案（原文のまま）、第 2 部はそれを MVP として実装するための方針。

---

# 第 1 部: 対話型エージェント・タスクキュー zelloom 設計案

## 概要

Claude Code / Codex などの対話型コーディングエージェントを、**1タスク = 1対話セッション**として扱うための軽量なタスクキューを作る。

既存の Zellij セッション内で利用し、各タスクは指定された workspace で新しいエージェントセッションとして実行する。

タスク中は通常の Claude Code / Codex と同様に人間が自由に対話できる。

現在のタスクが終了すると、そのエージェントセッションを破棄し、同じ workspace の次のタスクを新しいセッションとして開始する。

異なる workspace のタスクについては同時実行を許可する。

基本コンセプトは以下。

> One task, one interactive session.

および、

> One interactive session per workspace at a time.

## 目的

一般的なエージェントのプロンプトキューでは、同じエージェントセッションに複数の仕事を順番に投入することが多い。この方式では、前のタスクのコンテキストが後続タスクに残る。

本ツールでは、

```text
Task A1
  ↓
Claude session A1
  ↕
人間と対話
  ↓
終了・セッション破棄

Task A2
  ↓
Claude session A2
```

というモデルを採用する。タスク中は通常の対話型エージェントとして使える一方、タスク間ではコンテキストを完全に分離する。

また、異なるプロジェクトについては、

```text
Workspace A                 Workspace B

Task A1                     Task B1
   ↓                           ↓
Claude A                   Claude B
   ↕                           ↕
Human                      Human
```

のように同時に作業できる。ただし同じ workspace 内では、

```text
Task A1
   ↓
実行中

Task A2
   ↓
待機
```

となり、並列実行しない。

## 基本概念

```text
Task
Workspace
Agent
Queue
Runner
Adapter
```

### Task

Task は実際にエージェントへ渡す1つの仕事。

```json
{
  "id": "01KABC...",
  "text": "READMEの設定例を修正する",
  "status": "queued",
  "workspace": "amanejp",
  "agent": null,
  "source": {
    "type": "cli",
    "id": null
  },
  "reply_to": null,
  "created_at": "2026-09-25T16:00:00+09:00",
  "metadata": {}
}
```

`agent` が未指定の場合は workspace のデフォルトagentを使用する。

### Workspace

Workspace は「エージェントをどこで実行するか」を表す。必ずしも Git repository と同一である必要はない。

```toml
[workspaces.amanejp]
path = "/home/amane/src/amanejp"
agent = "opus"

[workspaces.swing]
path = "/home/amane/src/swing"
agent = "claude"

[workspaces.loom]
path = "/home/amane/src/loom"
agent = "codex"
```

基本情報は `id` / `path` / `default agent` / `max_parallel`。通常は `max_parallel = 1` とし、同じ workspace の複数タスクは必ず直列実行される。

### Agent

Agent は「エージェントをどう起動するか」を表す。Workspace と Agent を分離することで、`workspace = どこで実行するか` / `agent = 何をどう起動するか` という責務分離を行う。

```toml
[agents.claude]
command = ["claude"]

[agents.opus]
command = ["claude", "--model", "opus"]

[agents.codex]
command = ["codex"]

[agents.opencode]
command = ["opencode"]
```

コマンドは shell 文字列ではなく argv 配列として保持する（shell quoting や command injection の問題を避けるため）。

### タスク実行時の解決

Task `{"workspace": "amanejp", "text": "フィード周りを修正する"}`、Workspace `amanejp`（path=`/home/amane/src/amanejp`, agent=`opus`）、Agent `opus`（command=`["claude", "--model", "opus"]`）のとき、Runner は

```text
cwd  = /home/amane/src/amanejp
argv = ["claude", "--model", "opus", "フィード周りを修正する"]
```

として実行する。

### Agent の override

個別タスクで別 agent を指定できるようにしてもよい。

```bash
loom add -w amanejp --agent codex "この問題だけCodexでも確認する"
```

解決優先順位は `Task.agent → Workspace.agent → Global default agent`。MVPでは Task 単位の override は省略してもよい。

### ローカルからの Workspace 登録

```bash
cd ~/src/amanejp
loom workspace add amanejp --agent opus
```

で現在のディレクトリを登録する。

### Workspace の自動判定

ローカルからタスクを追加するときは、現在のディレクトリと登録済み workspace path の最長一致で workspace を自動判定する。Git repository の場合は `git rev-parse --show-toplevel` を補助的に利用してもよい。`loom add -w amanejp "..."` で明示指定も可能。

## アーキテクチャ

```text
                         external world
                               │
              ┌────────────────┼────────────────┐
             CLI              HTTP            Nostr
              └──────────── adapters ───────────┘
                               │
                          Unix socket
                               │
                       ┌───────▼────────┐
                       │   loom core    │
                       │ inbox          │
                       │ global queue   │
                       │ scheduler      │
                       │ task state     │
                       └───────┬────────┘
              ┌────────────────┼────────────────┐
        workspace A      workspace B      workspace C
           runner           runner           runner
           Claude           Claude            Codex
              ↕                ↕                ↕
           Human            Human            Human
```

### Global Queue

タスクキューは workspace ごとに分離せず、1つのグローバルキューとして管理する。Scheduler が workspace ごとに実行可能性を判断する。

### Scheduler

同じ workspace のタスクは最大1つのみ実行する。

```text
for task in queued_tasks:
    if task.workspace is not running:
        start(task)
```

内部的には `active_workspaces = {"amanejp": task_A1, ...}` のような workspace 単位のロックを持つ。必要に応じて全体の並列実行数も `[scheduler] max_parallel = 4` で制限する（同一 workspace 最大1、システム全体最大4）。

## Zellij との統合

既存の Zellij セッションをそのまま利用する。Zellij 自体をネストして起動しない。

### 管理タブ

`loom` 起動時に管理用タブを作成する。

```text
[editor] [shell] [server] [loom] [amanejp ●] [swing ●]
```

```text
┌──────────────── loom ─────────────────┐
│ RUNNING                                 │
│ ● amanejp   ログイン処理を修正          │
│ ● swing     Gateway設定を確認           │
│                                         │
│ QUEUED                                  │
│   amanejp   テスト追加                   │
│   swing     README更新                   │
│   loom    Nostr adapter追加            │
│                                         │
│ INBOX                                   │
│   ○ HTTP: CI失敗を確認                  │
│   ○ Nostr: Issue #32を見る              │
│                                         │
│ > add task...                           │
└─────────────────────────────────────────┘
```

### Workspace タブ

実行中の workspace ごとに Zellij タブを用意し、通常の Claude Code / Codex と同じように操作できる。

### Workspace Runner

Workspace ごとに Runner を1つ持つ。Runner 自体は永続し、エージェントプロセスだけをタスクごとに作り直す。

## タスクのライフサイクル

基本状態: `received → queued → running → done`
補助状態: `failed` / `cancelled` / `rejected` / `interrupted`

### タスク実行フロー

```text
amanejp / Task A1 をclaim
  → cwd=/home/amane/src/amanejp
  → Claude session A1を起動
  → 人間とClaudeが通常どおり対話
  → ユーザーが終了を承認
  → loom done
  → Claude session A1を終了
  → Task A1 → done
  → workspace lock解放
  → amanejp / Task A2 をclaim
  → Claude session A2を新規起動
```

Task A2 には Task A1 の会話履歴を引き継がない。

### タスク終了

Claude 自身に Zellij を直接操作させない（`zellij action close-pane` などを agent に実行させない）。代わりに専用コマンド `loom done` を用意する。

Agent 起動時にタスク ID・workspace・ソケットパスを環境変数で渡し、`loom done` は Unix socket 経由で core へ `{"op": "complete", "task_id": "01KABC..."}` を送信する。Runner は現在の agent process を終了し、次のタスクへ進む。

### Human-in-the-loop

Agent が自分で「仕事が終わった」と判断しただけでは終了しない。ユーザーが明示的に終了を承認した場合のみ `loom done` を実行する。Agent への追加 instruction 例:

```text
You are running inside loom.

Each session corresponds to exactly one task.

Do not end the task merely because you believe the work is complete.

When the user explicitly confirms that the current task is finished,
run:

    loom done
```

管理TUIからも強制終了できるようにする。

## Core

`loom core` がシステム全体の状態を管理する。主要操作は `enqueue` / `claim` / `complete` / `fail` / `cancel` / `list` / `reorder`。Core は Claude / Codex 固有の仕様を知らない。

### IPC

ローカル IPC には Unix domain socket（例 `$XDG_RUNTIME_DIR/loom/default.sock`）を使い、プロトコルは JSON Lines 程度とする。この IPC を安定した内部 API として扱う。

## 外部チャネル

外部サービスへの接続は core に直接実装せず、Adapter（`loom-http`、`loom-nostr` など）として分離し Unix socket 経由で core に接続する。外部側はローカルの絶対パスを知らなくてよい。

### Source / Reply To

タスクの投入元（`{"type": "nostr", "id": "<event-id>", "sender": "<pubkey>"}` など）を保持する。Runner は原則として source を意識しない。将来的に外部チャネルへ結果を返すため `reply_to` を持てるようにする。

### Inbox

外部入力を即座に実行キューへ入れない構成を用意する。source ごとに `[sources.<type>] auto_queue = true|false` を設定し、`false` の場合は人間が承認してから Queue に移す。

## TUI

```text
Enter     タスク追加
↑ / ↓     選択
e         編集
dd        削除
J / K     並べ替え
a         Inboxからaccept
r         reject
f         現在のタスクを完了
Enter     実行中workspaceのタブへ移動
```

## 永続化

状態はローカルに保存する。最終的には SQLite が扱いやすい。Zellij や PC が異常終了した場合、`running` だったタスクを `interrupted` へ移し、`retry` / `mark done` / `cancel` などを人間が選択できるようにする。

## MVP

必須:

- 既存Zellijセッションで動作
- 管理用タブ
- Workspaceごとの実行タブ
- Global Queue
- Workspaceごとの `max_parallel = 1`
- 異なるWorkspaceの並列実行
- 1タスク = 1 fresh agent process
- Claude / Codexとの通常のinteractive session
- `loom done`
- CLIからタスク追加
- Workspace登録
- Agent設定
- Unix socket API
- タスク永続化
- 異常終了時の interrupted 管理

後回し: HTTP adapter / Nostr adapter / 外部への完了通知 / Workspace内並列実行 / Git worktree管理 / GitHub Issues連携 / Slack・Discord / Web UI / 自動タスク分解 / マルチエージェント協調

## 設計上の原則

1. **One task, one interactive session**
2. **One active task per workspace**
3. 異なる workspace は並列実行可能
4. タスク間で会話コンテキストを共有しない
5. Workspace は「どこで実行するか」を表す
6. Agent は「何をどう起動するか」を表す
7. 外部チャネルはローカルの絶対パスやコマンドを知らない
8. タスク中は人間とagentが自由に対話できる
9. タスク終了は原則として人間が承認する
10. TUIはQueueの所有者ではなくClient
11. Coreは外部サービスを知らない
12. Runnerはタスクの入力元を知らない
13. 外部連携はAdapterとして追加する
14. Unix socket APIを内部の安定境界とする
15. 大規模なagent orchestratorではなく、小さな仕事のリレーに留める

---

# 第 2 部: MVP 実装方針

## 命名

- プロジェクト・crate 名は `zelloom`。生成するバイナリ名だけ `loom`（`[[bin]] name = "loom"`）。
- パス・環境変数は `zelloom` を使う。
  - 設定: `$XDG_CONFIG_HOME/zelloom/config.toml`（`ZELLOOM_CONFIG` で上書き）
  - 状態: `$XDG_STATE_HOME/zelloom/state.db`（`ZELLOOM_STATE_DIR` で上書き）。core のログも同ディレクトリ
  - ソケット: `$XDG_RUNTIME_DIR/zelloom/default.sock`（`ZELLOOM_SOCKET` で上書き）
  - agent に渡す環境変数: `ZELLOOM_TASK_ID` / `ZELLOOM_WORKSPACE` / `ZELLOOM_SOCKET`

## 技術選定

- Rust（edition 2024）、単一バイナリ。
- CLI: clap（derive）。
- core: tokio（Unix socket サーバ）。
- 永続化: rusqlite（`bundled`）。
- 設定: toml（読み取り）、toml_edit（`workspace add` での書き込み。既存の書式を保つ）。
- TUI: ratatui + crossterm。TUI と CLI は同期の `std::os::unix::net::UnixStream` クライアントで足りる。
- ID: ULID。
- 端末・プロセス制御: nix。

## サブコマンド

| コマンド | 役割 |
|---|---|
| `loom` | 起動。core が動いていなければデタッチで起動し、Zellij に `loom` 管理タブがなければ作って `loom tui` を開く |
| `loom core` | core デーモン（フォアグラウンド実行） |
| `loom tui` | 管理 TUI |
| `loom runner <workspace>` | workspace タブ内で動く runner |
| `loom add [-w WS] [--agent A] [TEXT]` | タスク追加。TEXT 省略時は標準入力。`-w` 省略時は cwd から最長一致で自動判定（見つからなければ `git rev-parse --show-toplevel` でも試す） |
| `loom done [TASK_ID]` | 完了通知。省略時は `ZELLOOM_TASK_ID` |
| `loom list` | タスク一覧（非 TUI） |
| `loom workspace add <id> [--agent A] [--path P]` | workspace 登録（既定は cwd） |
| `loom workspace list` | workspace 一覧 |

## 設定

```toml
default_agent = "claude"

[scheduler]
max_parallel = 4

[agents.claude]
command = ["claude"]
instruction_flag = "--append-system-prompt"

[agents.codex]
command = ["codex"]

[workspaces.amanejp]
path = "/home/amane/src/amanejp"
agent = "claude"
max_parallel = 1

[sources.cli]
auto_queue = true
```

- `instruction_flag` がある agent には loom 用の instruction を `[flag, instruction]` として argv に足す。ない agent には instruction をタスク本文の前に連結して渡す。
- argv の最終形は `command + [instruction_flag, instruction]? + [text]`。
- instruction 内の `loom done` は runner 自身の実行ファイルの絶対パスで書く（PATH に依存しない）。
- `max_parallel` は workspace では 1 のみ受け付ける（1 以外は設定エラー）。
- `sources.<type>.auto_queue` は未設定なら `true`。`false` の source から来たタスクは `received`（Inbox）に入る。
- core は設定をリクエストごとに読み直す（`workspace add` の結果がすぐ反映されるように）。

## IPC プロトコル

Unix socket 上の JSON Lines。1 行 1 メッセージ。

- 通常のリクエストは 1 行送って 1 行受け取る。応答は `{"ok": true, "data": ...}` か `{"ok": false, "error": "..."}`。
- 操作: `enqueue` / `list` / `complete` / `fail` / `cancel` / `delete` / `edit` / `move`（上下移動）/ `accept` / `reject` / `retry` / `status` / `shutdown` と、runner 用の `runner_attach`。
- `runner_attach` は長寿命接続。runner が `{"op": "runner_attach", "workspace": "..."}` を送ると core は以後この接続にイベントを push する。
  - core → runner: `{"event": "start", "task": {...}, "agent": {...解決済み argv・cwd・env...}}`、`{"event": "stop", "task_id": "..."}`
  - runner → core: `{"op": "agent_exited", "task_id": "...", "outcome": "done" | "failed" | "restart"}` など
- runner の接続が切れたら、その workspace で running だったタスクは `interrupted` になる。

## Scheduler

- キューは `position` 列で順序付けしたグローバル 1 本。
- スケジュール判定のたびに queued を position 順に走査し、workspace が空いていて全体上限未満なら start する。
- start 時、その workspace の runner が接続済みで idle ならイベントを送る。未接続なら Zellij に workspace タブを作って `loom runner <ws>` を起動し、接続を待つ（一定時間接続がなければ失敗扱いで lock を解放）。
- core 起動時に `running` のタスクはすべて `interrupted` にする。

## Runner

- workspace タブの中で常駐し、タスクごとに agent を子プロセスとして起動する。
- agent はシェルのジョブ制御と同じ扱いにする: 子を新しいプロセスグループで起動して `tcsetpgrp` で端末のフォアグラウンドにし、終了後に runner に戻す（runner は SIGTTOU/SIGINT を無視）。
- 起動前に termios を保存し、agent 終了後に復元する。あわせて代替画面・マウス・bracketed paste・キーボード拡張モードなどを解除し、カーソルを表示する。
- `stop` を受けたら agent のプロセスグループに SIGTERM、猶予後に SIGKILL。
- agent が `loom done` なしで終了した場合は runner がその場で `[d]one / [r]estart / [f]ailed` を尋ねて core に返す。
- タスクがないときは待機表示のまま残る。

## TUI

- core を 0.5 秒程度でポーリングして RUNNING / QUEUED / INBOX / INTERRUPTED を表示する。
- キー: `Enter`（入力欄でタスク追加、一覧で running を選んでいれば該当タブへ移動）、`↑/↓`、`e` 編集、`dd` 削除、`J/K` 並べ替え、`a` accept、`r` reject、`f` 実行中タスクを完了、interrupted に対する `R` retry / `D` mark done / `c` cancel、`q` 終了。
- 追加時の workspace は入力欄で `ws: 本文` の形式、または選択中タスクの workspace を既定にする。

## Zellij 連携

- `zellij` CLI の `action` サブコマンドで操作する。対象セッションは `loom` / `loom core` 起動時の `ZELLIJ_SESSION_NAME`。
- 使う操作: タブ作成（名前・cwd・コマンド付き）、タブ名でのフォーカス移動、タブ一覧の取得。

## 実装の段取り

1. core 基盤: 設定・状態 DB・プロトコル・core サーバ・scheduler（タブ起動部は差し替え可能な trait にする）・CLI（add / done / list / workspace）
2. Zellij 調査: 対象バージョンの CLI で上記操作ができるかを検証
3. runner と Zellij 連携、`loom` 起動コマンド
4. TUI
5. 通しの検証（テスト用のバックグラウンド Zellij セッションと偽 agent で）、ドキュメント整備
