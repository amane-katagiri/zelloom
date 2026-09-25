# zelloom（`loom`）

## これは何か

zelloom は、Claude Code や Codex のような対話型コーディングエージェントを **「1タスク = 1つの新しい対話セッション」** として扱う、Zellij 上で動く軽量なタスクキュー。

- 1つのタスクは、まっさらな状態から起動したエージェントの1セッションに対応する。前のタスクの会話はセッションと一緒に破棄され、次のタスクへ引き継がれない。
- 同じ workspace（プロジェクト）内のタスクは常に1つずつ直列に実行される。
- 異なる workspace のタスクは並列に実行できる。
- 既存の Zellij セッションの中で動く。Zellij 自体を入れ子で起動することはない。

詳しい設計は [docs/architecture.md](docs/architecture.md) を、初期の設計案は [docs/plan.md](docs/plan.md)（歴史的資料。現状の実装と食い違う箇所がありうる）を参照。

## 動作要件

- Rust ツールチェイン（`cargo install --path .` でビルドする）。
- Zellij 0.45 以降。zelloom は `zellij` CLI の次の機能を使う（一覧は [docs/architecture/zellij.md](docs/architecture/zellij.md#使うコマンド)）。
  - `zellij action new-tab --no-focus -- <cmd>`（フォーカスを奪わずにタブを作成）
  - `zellij action list-tabs -a -j` / `zellij action list-panes -a -j`（JSON 出力）
- **`zellij` CLI と、実際に動いている Zellij セッションのサーバは同じバージョンでなければならない。** バージョンが食い違うと操作に失敗する。

## インストール

```sh
cargo install --path .
```

生成されるバイナリ名は `loom`。

## クイックスタート

### 1. 設定ファイルを作る

```sh
loom init
```

設定ファイル（既定 `$XDG_CONFIG_HOME/zelloom/config.toml`、`ZELLOOM_CONFIG` で上書き可）を次のテンプレートで作る。すでにファイルがあれば何も書き換えずにエラーになる。

```toml
default_agent = "claude"

[scheduler]
max_parallel = 4

[agents.claude]
command = ["claude"]
instruction_args = ["--append-system-prompt", "{instruction}"]

[agents.codex]
command = ["codex"]
instruction_args = ["-c", "developer_instructions={instruction}"]
```

エージェントは既定でユーザーの対話シェル（`$SHELL -i`）経由で起動するので、Zellij でタブを手で開いたときと同じく `.bashrc` / `.zshrc` / `config.fish` の `PATH` や環境変数が効く。直接起動したいエージェントには `shell = false` を書く。エージェントや workspace ごとに環境変数を足すこともできる（workspace の値が優先。rc ファイルが同じ変数を設定すると rc の値になる）。

```toml
[agents.claude]
command = ["claude"]
instruction_args = ["--append-system-prompt", "{instruction}"]
env = { CLAUDE_CODE_MAX_OUTPUT_TOKENS = "32000" }

[workspaces.myapp]
path = "/home/you/src/myapp"
env = { RUST_LOG = "debug" }
```

エージェントには「1タスク=1セッションの仕組みの中で動いていること」と `loom done` の使い方を伝える固定の instruction を渡す。`instruction_args` を持つエージェントには、その中の `{instruction}` を instruction に置き換えた引数を付けて渡す（Claude Code なら `--append-system-prompt`、Codex なら `-c developer_instructions=...`）。`instruction_args` を設定していないエージェントには、instruction をタスク本文の前に連結して渡す。詳細（シェルのラッパ、環境変数のマージ順）は [docs/architecture/config.md](docs/architecture/config.md)。

### 2. workspace を登録する

```sh
cd ~/src/myapp
loom workspace add
# registered workspace myapp -> /home/you/src/myapp (agent: default)
```

引数を省略すると、カレントディレクトリが属する git リポジトリのトップレベル（git 管理外ならカレントディレクトリ）を workspace として登録し、そのディレクトリ名を workspace ID にする。ID・パス・agent は `loom workspace add <ID> --path <P> --agent <A>` で明示できる。登録済みの ID を指定するとエラーになる。規則の詳細は [docs/architecture/config.md](docs/architecture/config.md#workspace-add-の書き込み)。

### 3. Zellij のペインで `loom` を起動する

```sh
loom
```

core（デーモン）がまだ動いていなければバックグラウンドで起動してから、そのペインで管理 TUI を開く。新しいタブは作らない。core の起動には Zellij セッションが必要だが、core がすでに動いていれば `loom` は Zellij の外でも使える。

TUI を `q` で閉じると、core も止めるかを確認する（`y` で停止、`n` で core を残して TUI だけ終了）。

Zellij の起動時に TUI を開いておきたいなら、レイアウトに `loom` を実行するペインを置く。

```kdl
layout {
    tab name="loom" {
        pane command="loom"
    }
}
```

### 4. タスクを追加する

```sh
loom add -w myapp "READMEの設定例を修正する"
```

`-w` を省略すると、カレントディレクトリと登録済み workspace の path を照合して自動判定する（見つからなければ `git rev-parse --show-toplevel` でも試す）。判定できなければ、登録用の `loom workspace add` コマンドを添えてエラーになる。TUI からも `n` キーで `ws: 本文` の形式で追加できる。

タスクがキューの先頭に来ると、その workspace のタブが（無ければ）作られ、そこで新しいエージェントセッションが起動する。同じ workspace の後続タスクは、今のタスクが終わるまで待つ。

### 5. タスクを終える（`loom done`）

エージェントは、**自分で「作業が終わった」と判断しただけではタスクを終了しない**。人間が明示的に完了を承認したときだけ、エージェントに `loom done` を実行させる（instruction にもそう明記されている）。

```sh
loom done
```

タスクIDを省略すると、そのセッションに渡されている `ZELLOOM_TASK_ID` を使う。これで現在のエージェントプロセスが終了し、同じ workspace の次のタスク（あれば）が新しいセッションとして起動する。TUI からは `f`（実行中タスクを完了）でも同じ操作ができる。

## CLI コマンド

| コマンド | 役割 |
|---|---|
| `loom` | core が動いていなければ起動し（Zellij セッション内のみ可）、現在の端末で管理 TUI を開く |
| `loom add [-w/--workspace WS] [--agent A] [TEXT]` | タスク追加。`TEXT` 省略時は標準入力から読む。`A` は `[agents]` に定義済みでなければならない |
| `loom done [TASK_ID]` | タスクを完了にする。省略時は `ZELLOOM_TASK_ID` を使う |
| `loom list` | タスク一覧を表示する（非TUI） |
| `loom stop [--force]` | core を止める。実行中のタスクがあると拒否して一覧を表示する。`--force` なら止め、実行中だったタスクは `interrupted` になる（エージェント自体は終了するまで動き続ける） |
| `loom init` | 設定ファイルをテンプレートから作る（既にあればエラー） |
| `loom workspace add [ID] [--agent A] [--path P]` | workspace を登録する。`P` 省略時は git トップレベル（無ければカレントディレクトリ）、`ID` 省略時はそのディレクトリ名。`A` は `[agents]` に定義済みでなければならない |
| `loom workspace list` | 登録済み workspace の一覧 |

`loom add` / `loom done` / `loom list` は core が動いていないと `loom core is not running (<socket>); start it with `loom` inside a Zellij session` と表示して失敗する（`loom stop` は `core is not running` と表示して正常終了する）。

内部用の `loom core`（core をフォアグラウンドで実行）と `loom runner <workspace>`（core が workspace タブで起動する）は `--help` に表示されない。

すべてのサブコマンドは `--socket <path>` でソケットパスを明示的に指定できる（省略時は `ZELLOOM_SOCKET` または既定値）。

## TUI キー操作

| キー | 動作 |
|---|---|
| `↑`/`k`, `↓`/`j` | 選択移動 |
| `n` / `o` | タスク追加（`ws: 本文` の `ws` が登録済み workspace ならそれを使い、それ以外は設定の `tui.default_workspace`、それも無ければ選択中タスクの workspace を既定に） |
| `e` | 選択中タスクを編集 |
| `dd` | 選択中タスクを削除 |
| `J` / `K` | キュー内で下/上へ移動 |
| `a` / `r` | Inbox のタスクを accept / reject |
| `f` | 実行中タスクを完了にする |
| `R` | interrupted / failed タスクを retry |
| `D` | interrupted タスクを done 扱いにする |
| `c` | queued / received / interrupted タスクを cancel |
| `Enter` | 実行中タスクの workspace タブへフォーカス移動 |
| `q` / `Ctrl+C` | 終了確認を出す。`y` で core も止めて終了（実行中タスクは `interrupted` になる）、`n`（または確認中にもう一度 `Ctrl+C`）で core を残して終了、`Esc` で戻る。core が動いていなければ即終了 |

詳細（画面構成、入力パース、既知の制約など）は [docs/architecture/tui.md](docs/architecture/tui.md)。

## もっと詳しく

- 全体構成・プロセス図・モジュール索引: [docs/architecture.md](docs/architecture.md)
- IPCプロトコル: [docs/architecture/protocol.md](docs/architecture/protocol.md)
- スケジューラとタスクの状態遷移: [docs/architecture/scheduler.md](docs/architecture/scheduler.md)
- runner（ジョブ制御・端末の受け渡し）: [docs/architecture/runner.md](docs/architecture/runner.md)
- Zellij連携: [docs/architecture/zellij.md](docs/architecture/zellij.md)
- 設定・パス・環境変数: [docs/architecture/config.md](docs/architecture/config.md)
- TUI: [docs/architecture/tui.md](docs/architecture/tui.md)
- 残タスク: [docs/todo.md](docs/todo.md)
