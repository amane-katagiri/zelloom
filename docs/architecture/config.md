# 設定とパス

`src/config.rs`（読み込み・検証・書き込み）と `src/paths.rs`（パス解決）。

## パスと環境変数

| 用途 | 既定値 | 上書き | 関数 |
|---|---|---|---|
| 設定ファイル | `$XDG_CONFIG_HOME/zelloom/config.toml`（無ければ `~/.config/zelloom/config.toml`） | `ZELLOOM_CONFIG` | `paths::config_path` |
| 状態ディレクトリ | `$XDG_STATE_HOME/zelloom`（無ければ `~/.local/state/zelloom`） | `ZELLOOM_STATE_DIR` | `paths::state_dir` |
| 状態DB | `<状態ディレクトリ>/state.db` | (`ZELLOOM_STATE_DIR` 経由) | `paths::state_db_path` |
| core のログ | `<状態ディレクトリ>/core.log`（`loom` がデタッチ起動するときのみ書く） | (同上) | `paths::log_dir` |
| ソケット | `$XDG_RUNTIME_DIR/zelloom/default.sock`（無ければ `/tmp/zelloom-<uid>/default.sock`） | 優先順: `--socket`（グローバルCLIフラグ） > `ZELLOOM_SOCKET` > 既定 | `paths::socket_path` |
| `zellij` バイナリ | `zellij`（`PATH` から解決） | `ZELLOOM_ZELLIJ` | `zellij::zellij_bin` |
| attach タイムアウト | 20秒 | `ZELLOOM_ATTACH_TIMEOUT_MS`（ミリ秒） | `core::attach_timeout_from_env` |

- `--socket` はプロセス内の `OnceLock`（`paths::set_socket_override`）に一度だけ保存され、以後そのプロセス内の `socket_path()` 呼び出しすべてに優先する。CLI 起動直後、サブコマンドを実行する前に設定される。
- attach タイムアウトは `loom core` 起動時に一度だけ読まれる。実行中に環境変数を変えても反映されない。
- 設定ファイルが存在しない場合はエラーにせず、空の `Config`（すべて既定値）として扱う。ただし CLI では、設定ファイルが無いことが原因で失敗する操作（`loom add` の workspace 解決、`loom workspace add --agent`）は `loom init` を促すエラーにする。
- core は接続ごとの操作やスケジューリング判定のたびに設定ファイルを読み直す。`loom workspace add` などで設定を変えると、動いている core の次の操作から反映される。

## 設定ファイルの構造

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
shell = false
env = { CODEX_HOME = "/home/you/.codex-zelloom" }

[workspaces.myapp]
path = "/home/you/src/myapp"
agent = "claude"
env = { RUST_LOG = "debug" }
# max_parallel は省略可。指定する場合は 1 以外は設定エラーになる

[sources.nostr]
auto_queue = false

[tui]
default_workspace = "myapp"
```

| キー | 型 | 既定 | 検証 |
|---|---|---|---|
| `default_agent` | 文字列 | 無し | `[agents]` に存在すること |
| `scheduler.max_parallel` | 整数 | 4 | 特に検証なし |
| `agents.<name>.command` | 文字列配列（argv） | 必須 | 空配列、または先頭（プログラム名）が空文字列なら設定エラー |
| `agents.<name>.instruction_args` | 文字列配列 | 無し | instruction を渡す引数。各要素の `{instruction}` を instruction に置き換える。どの要素にも `{instruction}` が無ければ設定エラー。無ければ instruction はタスク本文に連結される |
| `agents.<name>.shell` | 真偽値 | true | true ならユーザーの対話シェル経由で起動する（[下記](#シェル経由の起動)） |
| `agents.<name>.oneshot` | 真偽値 | false | true なら instruction を渡さず、agent の終了コードでタスクの結果を決める（[runner.md](runner.md#終了後の分岐)）。`instruction_args` と同時に指定すると設定エラー |
| `agents.<name>.env` | 文字列→文字列のテーブル | 空 | agent に渡す環境変数。キーは空文字列・`=` や NUL を含むもの・`ZELLOOM_` で始まるものが設定エラー、値は NUL を含むと設定エラー（`config::validate_env`） |
| `workspaces.<id>` | テーブル | — | `<id>`（workspace ID）は空文字列・空白や制御文字や `:` を含むものが設定エラー（`config::validate_workspace_id`） |
| `workspaces.<id>.path` | パス | 必須 | 存在確認はしない |
| `workspaces.<id>.agent` | 文字列 | 無し（`default_agent` にフォールバック） | `[agents]` に存在すること |
| `workspaces.<id>.max_parallel` | 整数 | 無し | 指定するなら 1 のみ許可（それ以外は設定エラー） |
| `workspaces.<id>.env` | 文字列→文字列のテーブル | 空 | `agents.<name>.env` と同じ規則 |
| `sources.<type>.auto_queue` | 真偽値 | true | — |
| `tui.default_workspace` | 文字列 | 無し | `[workspaces]` に存在すること。TUI のタスク追加で `ws:` を省略したときの workspace（[tui.md](tui.md)） |

表に無いキーは無視される（エラーにならない）。検証は設定を読み込むたび（CLI・core とも）に `Config::validate` で行う。

## agent の解決と argv

優先順位: `Task.agent` → `Workspace.agent` → `default_agent`（`Config::resolve_agent`）。

- `enqueue` は、`agent` が指定されていればその時点の設定の `[agents]` にあるかを検証し、無ければ `agent '<name>' is not defined in [agents]` のエラーにする（タスクは作らない）。`loom add --agent` はこのエラーをそのまま表示する。
- 開始時（または `restart` 時）にどれも無い場合や、解決した名前が `[agents]` に無い場合（追加後に設定から agent を消したなど）は、タスクが `failed` になる。

instruction（`loom done` の使い方を伝える固定文。`{loom_exe}` は `std::env::current_exe()` で得た core 自身の実行ファイルの絶対パス）:

```text
You are running inside zelloom.

Each session corresponds to exactly one task.

Do not end the task merely because you believe the work is complete.

When the user explicitly confirms that the current task is finished, run:

    /path/to/loom done
```

argv の組み立て（`build_argv`）:

- `oneshot` の agent: `command + [task.text]`。instruction は渡さない。例: `command = ["claude", "-p"]` なら `["claude", "-p", task.text]`
- `instruction_args` がある agent: `command + instruction_args（各要素の {instruction} を置換）+ [task.text]`。例: `claude` は `["claude", "--append-system-prompt", instruction, task.text]`、`codex` は `["codex", "-c", "developer_instructions=<instruction>", task.text]`（`-c` の値は TOML として解釈できなければ生の文字列として扱われ、developer ロールのメッセージになる）
- 無い agent: `command + ["{instruction}\n{task.text}"]`（instruction と本文を改行区切りで1引数に連結）

cwd は workspace の `path`。argv はシェル経由で起動する場合も agent 自身の argv のままで、シェルのラッパは runner が付ける（[下記](#シェル経由の起動)）。

## agent の環境変数

core が `ResolvedAgent.env` を組み立て（`scheduler::build_env`）、runner が自分の環境に上書きする形で agent の子プロセスにだけ設定する。後に書いたものが勝つ。

1. runner が継承した環境（Zellij サーバの環境。手で開いたペインと同じ）
2. `agents.<name>.env`
3. `workspaces.<id>.env`
4. `ZELLOOM_TASK_ID` / `ZELLOOM_WORKSPACE` / `ZELLOOM_SOCKET`（常に zelloom の値。設定で `ZELLOOM_` で始まるキーは書けない）

値は文字どおりの文字列で、`$HOME` や `~` などは展開しない。

`shell = true` のときは、この環境を設定したうえでシェルを起動するので、シェルの rc ファイルはこれらの変数を読めるし、上書きもできる。rc ファイルが同じ変数を `export` していれば rc の値が agent に渡る（手で開いたタブで `export` してからシェルを起動したときと同じ）。

## シェル経由の起動

`agents.<name>.shell`（既定 true）が true なら、runner は agent を runner 自身の `$SHELL`（空または未設定なら `/bin/sh`）の対話モードで起動する。Zellij でタブを手で開いたときと同じく、rc ファイル（`.bashrc` / `.zshrc` / `config.fish` など）で設定した `PATH` や環境変数が agent に効く。runner の環境は Zellij サーバから継承したものなので、`$SHELL` も手で開いたペインと同じになる。

ラッパはシェルのファイル名（`runner::shell_argv`）で決まる。

| シェル | 実行される argv |
|---|---|
| `fish` | `[$SHELL, "-i", "-c", "exec $argv", argv...]` |
| それ以外（bash / zsh / sh / dash / ksh / 不明なもの） | `[$SHELL, "-i", "-c", "exec \"$@\"", "loom-agent", argv...]` |

- agent の argv（instruction やタスク本文を含む）は位置引数として渡し、スクリプト文字列には埋め込まない。改行・引用符・`$`・非 ASCII を含んでもそのまま agent に届く。
- `exec` でシェル自身が agent に置き換わるので、agent のプロセスはシェルと同じ PID・同じプロセスグループのまま。runner のジョブ制御と停止シーケンスはそのまま効く。
- `exec` はコマンドを `PATH` から探すので、rc ファイルで定義したエイリアスや関数は agent のコマンドとしては使えない。
- コマンドが見つからない場合はシェルがエラーを表示して終了するので、spawn の失敗ではなく「agent が自分で終了した」扱い（runner のプロンプト）になる。
- POSIX の `-c` 構文を受け付けない fish 以外のシェル（csh 系など）を `$SHELL` にしている場合は `shell = false` にする。
- `shell = false` なら argv を直接起動する（rc ファイルは読まれない）。

## workspace の自動判定

`loom add` で `-w` を省略したとき（`config::detect_workspace`）:

1. カレントディレクトリ（`canonicalize` できればそれ、できなければそのまま）と、登録済み workspace の `path` をパスコンポーネント単位で比較し、最も長く前方一致する workspace を選ぶ。
2. 一致が無ければ `git -C <cwd> rev-parse --show-toplevel` の結果で同じ照合をもう一度試す。
3. それでも無ければエラーにする。設定ファイル自体が無ければ `loom init` を促す。あれば、`workspace add` の既定と同じ規則（下記）でカレントディレクトリから求めた ID を使った `loom workspace add <ID>` と、`loom add -w <workspace>` の2通りを示す（ID が不正なら `<ID>` と表示）。

`-w` で指定した workspace が未登録の場合も、設定ファイルが無ければ `loom init` を、あれば `loom workspace add <workspace>` を促す。

## `loom init`

`config::init_config` が `paths::config_path()`（`ZELLOOM_CONFIG` を尊重）に `config::CONFIG_TEMPLATE` を書く。親ディレクトリは作る。ファイルは `create_new` で開くので、既に存在すれば内容に触れずに `ConfigAlreadyExists` エラーで終わる。テンプレートは次の内容（先頭に短いコメント行が付く）で、`Config::validate` を通る。

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

書き込み後、書いたパスと次の手順（`cd <project> && loom workspace add`）を表示する。

## `loom config edit`

`paths::config_path()` のファイルをエディタで開く。ファイルが無ければエディタを起動せず、`loom init` を促すエラーにする。

- エディタは `VISUAL`、`EDITOR` の順で空でない最初のものを使い、どちらも無ければ `vi`。
- 値に引数を含められるよう（`code --wait` など）、`sh -c '<editor> "$@"' <editor> <path>` で起動する。
- エディタが非 0 で終了したらエラーにする。正常終了したら `config::load` で読み直し、エラーがあれば表示して非 0 で終了、無ければ `<path> is valid` を表示する。ファイルは元に戻さない。

## `workspace add` の書き込み

### パスと ID の決定

`loom workspace add [ID] [--path P] [--agent A]`:

- パス（`config::resolve_workspace_path`）: `--path` があればそれ（相対パスはカレントディレクトリ基準）。無ければ `git -C <cwd> rev-parse --show-toplevel` の結果、git 管理外ならカレントディレクトリ。いずれも `canonicalize` できればその結果を使う。
- ID（`config::workspace_id_from_path`）: 省略時は決定したパスの最後のコンポーネント（ディレクトリ名）。`validate_workspace_id` を通らなければ `InvalidDerivedWorkspaceId` エラーにし、ID を明示するよう促す。
- 成功すると `registered workspace <id> -> <path> (agent: <A または default>)` を表示する。

### 書き込み

`config::add_workspace` は既存の `config.toml` を `toml_edit::DocumentMut` として読み込み、コメントや既存キーの並びを保ったまま `[workspaces.<id>]`（`path` と、指定があれば `agent`）を追加する。`[workspaces]` テーブルが無ければ暗黙テーブルとして作るので、空の `[workspaces]` 見出しは出力されない。ファイルが無ければディレクトリごと新規作成するが、`--agent` を指定していてファイルが無い場合は `MissingConfig` エラーにして `loom init` を促す。

次の場合は何も書かずにエラーになる（ファイルもディレクトリも作らない）。

- workspace ID が不正（`validate_workspace_id`）。
- 同じ ID が既に登録されている（`WorkspaceAlreadyRegistered`。既存の path をメッセージに含める）。既存の登録を変えるには設定ファイルを直接編集する。
- 編集後の内容が読み込み時と同じ規則（`Config::validate`）を通らない（未定義の agent を `--agent` に指定した場合など）。
