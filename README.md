# zelloom（`loom`）

zelloom は、Claude Code や Codex のような対話型のコーディングエージェントに、タスクを順番に任せていくためのタスクキューです。Zellij の中で動きます。

いちばんの特徴は、**タスクごとに新しいエージェントのセッションを起動する**ことです。ひとつのタスクが終わるとそのセッションは閉じられ、次のタスクはまっさらな状態のエージェントが担当します。前のタスクの会話を引きずらないので、エージェントが前の作業の文脈に引っ張られて混乱する心配がありません。

- タスクはプロジェクト（zelloom では **workspace** と呼びます）ごとに、1 つずつ順番に実行されます。
- 別々の workspace のタスクは同時に進められます。
- 実行中のエージェントは workspace ごとの Zellij タブに表示されるので、いつでも覗いて話しかけられます。
- zelloom は今使っている Zellij セッションの中で動きます。Zellij をもう 1 つ起動し直すようなことはしません。

## 必要なもの

- Rust のツールチェイン（ソースからビルドします）
- Zellij 0.45 以降
- 使いたいエージェント（`claude` や `codex` など）

## インストール

GitHub のリポジトリから直接インストールできます。

```sh
cargo install --git https://github.com/amane-katagiri/zelloom
```

手元に clone したソースからインストールする場合は、リポジトリのディレクトリで次を実行します。

```sh
git clone https://github.com/amane-katagiri/zelloom
cd zelloom
cargo install --path .
```

どちらの方法でも `loom` というコマンドがインストールされます。

## はじめかた

### 1. 設定ファイルを作る

```sh
loom init
```

`~/.config/zelloom/config.toml` に、Claude Code と Codex を使うためのひな形が作られます。すでにファイルがある場合は何も書き換えず、エラーで止まります。ひな形のままでも使い始められますが、中身は後半の「[設定](#設定)」で詳しく説明しています。

### 2. プロジェクトを workspace として登録する

作業させたいプロジェクトのディレクトリに移動して、次を実行します。

```sh
cd ~/src/myapp
loom workspace add
# registered workspace myapp -> /home/you/src/myapp (agent: default)
```

git リポジトリの中で実行した場合はリポジトリのトップが、そうでなければ今いるディレクトリが登録されます。workspace の名前（ID）はディレクトリ名になります。名前や場所、使うエージェントを自分で決めたいときは次のように指定します。

```sh
loom workspace add myapp --path ~/src/myapp --agent codex
```

同じ名前の workspace がすでにあるとエラーになります。登録内容を変えたいときは設定ファイルを直接編集してください。

### 3. `loom` を起動する

Zellij のペインで `loom` を実行します。

```sh
loom
```

裏でタスクを管理するプロセス（以下 core と呼びます）が起動し、そのペインに管理画面（TUI）が開きます。新しいタブが勝手に開くことはありません。

core の起動は Zellij の中でしかできませんが、一度起動してしまえば、`loom add` などのコマンドは Zellij の外からでも使えます。

Zellij を立ち上げたときに管理画面も開いておきたい場合は、レイアウトファイルに `loom` を実行するペインを入れておくと便利です。

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

`-w` を省略すると、今いるディレクトリがどの workspace に含まれるかを見て自動で選びます。見つからなければ、登録用のコマンドを添えてエラーになります。本文を省略すると標準入力から読むので、長い指示はファイルやヒアドキュメントから流し込めます。

```sh
loom add -w myapp < task.md
```

管理画面で `n` を押して追加することもできます（[管理画面の使い方](#管理画面tuiの使い方)）。

タスクの順番が来ると、その workspace の名前のタブが作られ、中でエージェントが起動します。タブはフォーカスを奪わずに作られるので、作業中の画面が急に切り替わることはありません。管理画面のタブがあれば、その右隣に作られた順に並びます。core を止めるなどしてタブの役目が終わると、タブは自動で閉じます（エラーで終わった場合はエラーを読めるように残ります）。同じ workspace に次のタスクがあっても、今のタスクが終わるまでは待ちます。

### 5. タスクを完了させる

エージェントは、自分で「終わった」と思っただけではタスクを閉じません。あなたが作業を確認して「これで完了」と伝えたときに、エージェントが自分で `loom done` を実行してセッションを終える、という流れになっています。この約束事は起動時にエージェントへ伝えてあります。

```sh
loom done
```

タスクが完了するとエージェントは終了し、同じ workspace に次のタスクがあれば、新しいセッションでそのタスクが始まります。

エージェントに頼まず自分で完了させたいときは、管理画面で実行中のタスクを選んで `f` を押してください。

### エージェントが途中で終了してしまったら

`loom done` を使わずにエージェントが終了した場合（あなたが `/exit` した、エージェントが落ちた、など）は、タブに次の確認が表示されます。

```text
agent exited on its own. [d]one / [r]estart / [f]ailed?
```

- `d`: タスクは完了したものとして扱い、次へ進みます。
- `r`: 同じタスクで新しいセッションを起動し直します。
- `f`: タスクを失敗として記録し、次へ進みます。失敗したタスクは管理画面からやり直せます。

## ふだんの使い方

### タスクの状態

管理画面には、タスクが状態ごとに分かれて表示されます。

| 状態 | 意味 |
|---|---|
| RUNNING | 実行中。workspace のタブでエージェントが動いています |
| QUEUED | 順番待ち |
| INBOX | 受け付けたものの、まだキューに入れていないタスク（[`auto_queue`](#受け取ったタスクを確認してから実行する) を切ったときに使います） |
| INTERRUPTED / FAILED | 中断・失敗したタスク。やり直すか、完了扱いにするか、取り消すかを選べます |

タスクが中断（interrupted）になるのは、core を止めたときに実行中だった場合や、タブを閉じるなどしてエージェントとの接続が切れた場合です。

完了したタスクや取り消したタスクは一覧には出ず、件数だけが見出しに表示されます。

### 管理画面（TUI）の使い方

| キー | できること |
|---|---|
| `↑` `↓`（`k` `j`） | タスクを選ぶ |
| `n` / `o` | タスクを追加する |
| `e` | 選んだタスクの本文を書き直す |
| `dd` | 選んだタスクを削除する（`d` を 2 回） |
| `J` / `K` | 順番待ちのタスクを 1 つ後ろ / 前へ動かす |
| `a` / `r` | INBOX のタスクをキューに入れる / 却下する |
| `f` | 実行中のタスクを完了にする |
| `R` | 中断・失敗したタスクをやり直す（キューの最後に戻ります） |
| `D` | 中断したタスクを完了扱いにする |
| `c` | タスクを取り消す |
| `Enter` | 実行中のタスクのタブへ移動する |
| `q` / `Ctrl+C` | 管理画面を閉じる |

`n` でタスクを追加するときは、`myapp: READMEを直す` のように先頭に workspace 名とコロンを付けると、その workspace に追加されます。付けなかった場合は、設定の [`tui.default_workspace`](#tui) があればそこへ、なければ今選んでいるタスクと同じ workspace へ追加されます。`fix: 〜` のように、先頭の単語が workspace 名でなければ本文の一部としてそのまま扱われます。

`q` で閉じようとすると、core も一緒に止めるかを聞かれます。

- `y`: core も止めて終了します。実行中のタスクがあれば中断扱いになります。
- `n`: core は動かしたまま、管理画面だけを閉じます。もう一度 `loom` を実行すれば戻ってこられます。
- `Esc`: 閉じるのをやめます。

### core を止める

```sh
loom stop
```

実行中のタスクがあるときは、止めずにそのタスクの一覧を表示します。それでも止めたい場合は `--force` を付けてください。実行中だったタスクは中断扱いになりますが、エージェント自体はタブの中で動き続けるので、作業内容が消えることはありません。

## コマンド一覧

| コマンド | 説明 |
|---|---|
| `loom` | core が動いていなければ起動し、管理画面を開きます |
| `loom add [-w WORKSPACE] [--agent AGENT] [本文]` | タスクを追加します。本文を省略すると標準入力から読みます。`--agent` でこのタスクだけ使うエージェントを変えられます |
| `loom done [タスクID]` | タスクを完了にします。エージェントのセッション内では ID を省略できます |
| `loom list` | タスクの一覧を表示します |
| `loom stop [--force]` | core を止めます |
| `loom init` | 設定ファイルのひな形を作ります |
| `loom config edit` | 設定ファイルをエディタ（`$VISUAL` → `$EDITOR` → `vi`）で開き、閉じたあとに内容を検査します |
| `loom workspace add [ID] [--path パス] [--agent AGENT]` | workspace を登録します |
| `loom workspace list` | 登録済みの workspace を表示します |

`loom add`・`loom done`・`loom list` は core が動いていないと使えません。先に Zellij の中で `loom` を起動してください。

どのコマンドにも `--socket <パス>` を付けて、接続する core を指定できます（[環境変数](#環境変数とファイルの場所)も参照）。

## 設定

設定ファイルは `~/.config/zelloom/config.toml` です（`XDG_CONFIG_HOME` を設定していればその下）。別の場所を使いたいときは環境変数 `ZELLOOM_CONFIG` にパスを指定します。

設定ファイルは core が操作のたびに読み直すので、書き換えた内容は core を再起動しなくても次のタスクから反映されます。書き方に誤りがあると、コマンドや管理画面の操作がエラーになり、どこが問題かが表示されます。

### `loom init` で作られる設定

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

ここから先は、この既定の状態から変えたくなったときの設定を順に説明します。

### すべての設定項目

| 項目 | 既定値 | 説明 |
|---|---|---|
| `default_agent` | なし | workspace やタスクで指定がないときに使うエージェント |
| `scheduler.max_parallel` | `4` | 全 workspace を合わせて同時に実行するタスクの上限 |
| `agents.<名前>.command` | （必須） | エージェントを起動するコマンド |
| `agents.<名前>.instruction_args` | なし | zelloom からの指示をエージェントに渡すための引数 |
| `agents.<名前>.shell` | `true` | ログインしたときと同じシェル経由で起動するか |
| `agents.<名前>.oneshot` | `false` | 対話せずに 1 回実行して終わるエージェントとして扱うか |
| `agents.<名前>.env` | なし | エージェントに渡す環境変数 |
| `workspaces.<ID>.path` | （必須） | プロジェクトのディレクトリ |
| `workspaces.<ID>.agent` | なし | この workspace で使うエージェント |
| `workspaces.<ID>.env` | なし | この workspace で起動するエージェントに渡す環境変数 |
| `tui.default_workspace` | なし | 管理画面でタスクを追加するときの既定の workspace |
| `sources.cli.auto_queue` | `true` | 追加したタスクをすぐキューに入れるか |
| `http.listen` | なし | HTTP でタスクを追加できるようにする（[詳細](docs/architecture/http.md)） |
| `http.allowed_origins` | 空 | HTTP アダプタで許可するブラウザの Origin |

知らない項目は無視されます（エラーにはなりません）。綴りを間違えても何も起きないので注意してください。

### エージェントを定義する

`[agents.<名前>]` でエージェントを定義します。`<名前>` は `default_agent` や `loom add --agent` で指定するときに使う名前で、自由に付けられます。

```toml
[agents.claude]
command = ["claude"]
instruction_args = ["--append-system-prompt", "{instruction}"]
```

`command` は起動するコマンドを、引数ごとに分けた配列で書きます。オプションを付けたい場合も配列の要素として足します。

```toml
[agents.claude-opus]
command = ["claude", "--model", "opus"]
instruction_args = ["--append-system-prompt", "{instruction}"]
```

実際には、`command` の後ろに `instruction_args`、最後にタスクの本文を付けたコマンドが実行されます。上の例なら次のようになります。

```text
claude --model opus --append-system-prompt "<zelloom からの指示>" "<タスクの本文>"
```

#### zelloom からの指示の渡し方（`instruction_args`）

zelloom は起動するエージェントに、「1 タスク 1 セッションで動いていること」と「完了を承認されたら `loom done` を実行すること」を伝える短い指示を渡します。`instruction_args` は、この指示をどの引数で渡すかを決める設定で、中の `{instruction}` が指示の本文に置き換わります。

- Claude Code なら `--append-system-prompt` でシステムプロンプトに追加します。
- Codex なら `-c developer_instructions=...` で developer メッセージとして渡します。

`instruction_args` を書く場合は、どこかに必ず `{instruction}` を含めてください（無いと設定エラーになります）。

システムプロンプトを渡す手段を持たないエージェントでは、`instruction_args` を省略します。その場合、指示はタスク本文の前に改行でつないで、ひとつの引数として渡されます。

```toml
[agents.other]
command = ["other-agent"]
```

#### シェルを通さずに起動する（`shell`）

エージェントは既定で、あなたのログインシェル（`$SHELL`）を対話モードで起動し、その中から実行されます。Zellij でタブを手で開いたときと同じく `.bashrc`・`.zshrc`・`config.fish` などが読み込まれるので、そこで設定した `PATH` や環境変数がエージェントにも引き継がれます。

シェルの設定を読み込ませたくない場合や、読み込みに時間がかかる場合は `shell = false` にすると、コマンドを直接起動します。

```toml
[agents.codex]
command = ["codex"]
instruction_args = ["-c", "developer_instructions={instruction}"]
shell = false
```

シェル経由で起動するときの注意点があります。

- rc ファイルで定義したエイリアスやシェル関数は、`command` の先頭に書いても使えません。`PATH` にある実行ファイルを指定してください。
- csh や tcsh をログインシェルにしている場合は、`shell = false` にしてください（bash・zsh・fish・sh などは問題ありません）。
- コマンドが見つからない場合は、シェルがエラーを表示して終了します。タブには「[エージェントが途中で終了してしまったら](#エージェントが途中で終了してしまったら)」と同じ確認が出ます。

#### 対話せずにタスクを順番にこなす（`oneshot`）

`claude -p` や `codex exec` のように、本文を受け取って作業し、終わったら自分で終了するモードのエージェントを使うと、タスクを人の確認なしで次々に実行できます。

```toml
[agents.claude-p]
command = ["claude", "-p"]
oneshot = true
```

```sh
loom add --agent claude-p "READMEの誤字を直す"
```

`oneshot = true` のエージェントは次のように動きます。

- zelloom からの指示は渡さず、`command` の後ろにタスクの本文だけを付けて起動します。`instruction_args` と一緒には書けません（設定エラーになります）。
- エージェントが終了すると、確認を出さずに、終了コードが 0 なら完了、それ以外なら失敗として記録し、同じ workspace の次のタスクへ進みます。失敗したタスクは管理画面からやり直せます。

エージェントの出力はタブに表示されるだけで、保存はされません。

#### 環境変数を渡す（`env`）

エージェントごと、または workspace ごとに環境変数を追加できます。

```toml
[agents.claude]
command = ["claude"]
instruction_args = ["--append-system-prompt", "{instruction}"]
env = { CLAUDE_CODE_MAX_OUTPUT_TOKENS = "32000" }

[workspaces.myapp]
path = "/home/you/src/myapp"
env = { RUST_LOG = "debug" }
```

同じ変数を両方に書いた場合は workspace の値が使われます。値は書いたとおりの文字列で渡され、`$HOME` や `~` は展開されません。

シェル経由で起動する場合（`shell = true`）、rc ファイルの中で同じ変数を `export` していると、そちらの値で上書きされます。

`ZELLOOM_` で始まる名前は zelloom が使うため指定できません。エージェントには次の変数が自動で渡されます。

| 変数 | 中身 |
|---|---|
| `ZELLOOM_TASK_ID` | 実行中のタスクの ID（`loom done` で ID を省略できるのはこのためです） |
| `ZELLOOM_WORKSPACE` | workspace の ID |
| `ZELLOOM_SOCKET` | core の接続先 |

### workspace ごとの設定

`loom workspace add` で登録した workspace は、次のように書き込まれます。手で書き足してもかまいません。

```toml
[workspaces.myapp]
path = "/home/you/src/myapp"
agent = "codex"
env = { RUST_LOG = "debug" }
```

- `path` はエージェントを起動するディレクトリです。存在するかどうかはタスクを実行するまで確認されません。
- `agent` を書くと、この workspace では `default_agent` の代わりにそのエージェントを使います。
- workspace の ID（`[workspaces.<ID>]` の部分）は、Zellij のタブ名にも使われます。空白や `:` は使えません。

使うエージェントは、優先度の高い順に次のように決まります。

1. `loom add --agent` で指定したエージェント
2. workspace の `agent`
3. `default_agent`

どれも指定されていない場合や、指定したエージェントが `[agents]` に無い場合、そのタスクは失敗扱いになります。

### 同時に動かすタスクの数（`scheduler.max_parallel`）

```toml
[scheduler]
max_parallel = 2
```

すべての workspace を合わせて、同時に実行するタスクの上限です。既定は 4 です。上限に達しているあいだ、ほかの workspace のタスクは順番待ちになります。

ひとつの workspace の中では、この値に関係なく常に 1 つずつ実行されます。`workspaces.<ID>.max_parallel` という項目もありますが、今は `1` 以外を書くとエラーになります。

### TUI

```toml
[tui]
default_workspace = "myapp"
```

管理画面で `n` を押してタスクを追加するとき、workspace 名を省略した場合の追加先です。ここに書く名前は `[workspaces]` に登録済みでなければなりません。

設定しない場合は、管理画面でいま選んでいるタスクの workspace に追加されます。タスクがひとつも無いときは workspace 名を付けて入力してください。

### 受け取ったタスクを確認してから実行する

```toml
[sources.cli]
auto_queue = false
```

既定では、追加したタスクはすぐに順番待ち（QUEUED）に入ります。`auto_queue = false` にすると、追加したタスクはいったん INBOX に置かれ、管理画面で `a` を押して受け入れるまで実行されません。あとでまとめて見直してから流したいときに使います。`loom add` と管理画面からの追加は `cli` 扱い、後述の HTTP アダプタ経由の追加は `http` 扱いで、`[sources.<種類>]` として別々に設定できます。

### HTTP でタスクを追加する

```toml
[http]
listen = "127.0.0.1:7878"
allowed_origins = ["http://localhost:5173"]
```

`[http]` を設定すると、core の中に `POST /tasks` だけを受け付ける HTTP サーバが立ち上がります。`listen` は loopback アドレス（`127.0.0.1` や `[::1]`）に限られ、ブラウザから叩く場合は `allowed_origins` にその Origin を登録します。詳しいセキュリティ上の仕組みは [docs/architecture/http.md](docs/architecture/http.md) を参照してください。`[http]` の変更は core の再起動後に反映されます。

### 環境変数とファイルの場所

| 用途 | 既定の場所 | 変更するには |
|---|---|---|
| 設定ファイル | `$XDG_CONFIG_HOME/zelloom/config.toml`（未設定なら `~/.config/zelloom/config.toml`） | `ZELLOOM_CONFIG` |
| タスクの保存先・core のログ | `$XDG_STATE_HOME/zelloom/`（未設定なら `~/.local/state/zelloom/`）の `state.db` と `core.log` | `ZELLOOM_STATE_DIR` |
| core との接続（ソケット） | `$XDG_RUNTIME_DIR/zelloom/default.sock`（未設定なら `/tmp/zelloom-<uid>/default.sock`） | `--socket` または `ZELLOOM_SOCKET` |
| `zellij` コマンド | `PATH` にある `zellij` | `ZELLOOM_ZELLIJ` |
| タブが立ち上がるまでの待ち時間 | 20 秒 | `ZELLOOM_ATTACH_TIMEOUT_MS`（ミリ秒） |

- タブを作ってからエージェントの準備ができるまでに待ち時間を超えると、そのタスクは中断扱いになります。マシンが遅くてよく中断されるようなら、`ZELLOOM_ATTACH_TIMEOUT_MS` を長めにしてください。この値は core の起動時に読まれます。
- core のログは、`loom` が core を裏で起動したときだけ `core.log` に書かれます。タスクが失敗したときの理由などはここで確認できます。

## 困ったときは

- **`loom core is not running` と言われる**: core が動いていません。Zellij のペインで `loom` を実行してください。
- **`loom add` で workspace が見つからないと言われる**: 今いるディレクトリが登録済みのどの workspace にも含まれていません。表示される `loom workspace add` を実行して登録するか、`-w` で workspace を指定してください。
- **タスクがすぐ失敗（FAILED）になる**: workspace やエージェントの指定が設定ファイルと合っていない可能性があります。`core.log` に理由が出ています。
- **タスクがすぐ中断（INTERRUPTED）になる**: タブの作成に失敗したか、エージェントの準備が待ち時間に間に合っていません。`core.log` を確認してください。
- **管理画面でタスクが下のほうに隠れて見えない**: 一覧のスクロールにまだ対応していません。ペインを広げてください。

ほかにもまだ対応していないことは [docs/todo.md](docs/todo.md) にまとめています。

## もっと詳しく

内部の仕組みや細かい仕様は docs にあります。

- 全体の構成: [docs/architecture.md](docs/architecture.md)
- 設定の細かい規則: [docs/architecture/config.md](docs/architecture/config.md)
- スケジューラとタスクの状態遷移: [docs/architecture/scheduler.md](docs/architecture/scheduler.md)
- タブの中で動くプロセス（runner）: [docs/architecture/runner.md](docs/architecture/runner.md)
- Zellij との連携: [docs/architecture/zellij.md](docs/architecture/zellij.md)
- 管理画面: [docs/architecture/tui.md](docs/architecture/tui.md)
- core との通信: [docs/architecture/protocol.md](docs/architecture/protocol.md)
- HTTP アダプタ: [docs/architecture/http.md](docs/architecture/http.md)
- 最初の設計案（今の実装とは違う部分があります）: [docs/plan.md](docs/plan.md)
