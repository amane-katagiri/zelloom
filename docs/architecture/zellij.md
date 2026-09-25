# Zellij 連携

`src/zellij.rs`（`zellij` CLI の薄いラッパ）と `src/launcher.rs`（`TabLauncher` の実装）。対象セッションは環境変数ではなく `--session <name>` で明示する。

## 使うコマンド

| コマンド | 用途 | 呼び出し箇所 |
|---|---|---|
| `zellij --session <name> action list-tabs -a -j` | 既存タブの一覧を JSON で取得（`tab_id` / `name` / `position`） | `Zellij::list_tabs` |
| `zellij --session <name> action list-panes -a -j` | 全ペインの一覧を JSON で取得（`tab_id` / `is_plugin` / `exited` / `exit_status` / `terminal_command` / `pane_command` / `pane_cwd` など） | `Zellij::list_panes` |
| `zellij --session <name> action new-tab --name <name> --cwd <path> --no-focus -- <argv...>` | workspace 用タブを作り、その中で `loom runner ...` を起動する | `Zellij::new_tab` |
| `zellij --session <name> action go-to-tab-name <name>` | 名前でタブへフォーカス移動 | TUI の `Enter`（`src/tui/mod.rs` が直接実行） |
| `zellij --session <name> action close-tab-by-id <id>` | 終了済み runner のタブを閉じる（その後に新しいタブを作る） | `ZellijLauncher::launch` |

すべて同期の `std::process::Command` 呼び出しで、`ZELLOOM_ZELLIJ` で `zellij` バイナリのパスを上書きできる（`Zellij` のメソッドは `zellij_bin()`、TUI は同じ環境変数を自分で読む）。`Zellij` のメソッドは `<name>` に core 起動時の `ZELLIJ_SESSION_NAME` を、TUI は TUI 自身の `ZELLIJ_SESSION_NAME` を使う。

## `--no-focus`

workspace タブは常にフォーカスを奪わずに作成する（`Zellij::new_tab` は常に `--no-focus` を付ける）。

## runner の argv

`zellij action new-tab -- <argv>` で起動したペインのプロセスは、core ではなく Zellij サーバプロセスの環境を継承する。ソケットパスは環境変数ではなく `loom` のグローバル `--socket` フラグとして argv で渡す。`ZellijLauncher::launch` は次の argv を組み立てる。

```text
<loom の実行ファイルの絶対パス> --socket <socket_path> runner <workspace_id>
```

詳細は [config.md](config.md#パスと環境変数)。

## workspace タブの再利用と作り直し

core が `ZELLIJ_SESSION_NAME` なしで起動されていると、`ZellijLauncher::launch` は Zellij を呼ばずにエラーを返す（タスクは `interrupted` になる）。セッション名があれば `list_tabs` と `list_panes` を取得し、`decide()` で分岐する。

runner タブは、名前が workspace ID に一致し、かつ **runner ペイン** を含むタブ。runner ペインは、プラグインでない（`is_plugin` が false）ペインのうち、`terminal_command`（ペインを起動したコマンド。空白区切りの 1 文字列）か `pane_command`（ペインの現在のフォアグラウンドのコマンド）のどちらかが次の形のもの。

```text
<パス>/<core 自身の実行ファイル名> [--socket <path>] runner <workspace_id>
```

- 末尾が ` runner <workspace_id>` であること、その前の（` --socket ` 以降を除いた）部分のファイル名が core の実行ファイル名（`current_exe()` のファイル名。末尾の ` (deleted)` は除く）と一致することで判定する（`is_runner_command`）。
- 同じ名前のタブが複数あってもよい。runner ペインを含まない同名のタブ（ユーザーが自分で作った `loom` タブで TUI を動かしている場合など）は無視する。

分岐:

- 同名のタブのどれかに `exited` でない runner ペインがある → `AlreadyRunning`（何もしない）
- runner ペインを含む同名のタブはあるが、その runner ペインがすべて `exited` → `CloseThenCreate`（それらのタブをすべて閉じてから新しく作る）
- runner ペインを含む同名のタブが無い → `CreateTab`

### 制約: クライアントがアタッチしていないセッションでの挙動

`list-tabs` / `list-panes` は、そのセッションに Zellij クライアント（実際にアタッチしている端末）が1つも無いと、タブはあってもペインを1件も返さない、という実機での挙動が確認されている。この状態では runner ペインが見つからないので、`decide()` は常に `CreateTab` になり、終了済み runner のタブは閉じられずに残る。`AlreadyRunning` / `CloseThenCreate` の分岐と `terminal_command` / `pane_command` の形式は、ユニットテストの合成データでのみ検証している。詳細は [todo.md](../todo.md)。

## 素の `loom` 起動フロー

`loom`（サブコマンド無し）は次を行う（`src/cli.rs` の `run_bare_loom` と `ensure_core`）。

1. ソケットに接続できれば core は動いているとみなし、Zellij の有無に関係なく 3 へ進む。
2. 接続できなければ、`ZELLIJ_SESSION_NAME` が無い場合は「core は Zellij セッションの中から起動する必要がある」というエラーで終了する。ある場合は core をデタッチ起動する（`setsid` した子プロセスとして `loom --socket <path> core` を起動し、stdin は `/dev/null`、stdout/stderr は状態ディレクトリの `core.log` に追記する。TUI の端末は引き継がない）。ソケットができるまで最大 5 秒待つ。
3. 実行した端末（ペイン）で、そのまま管理 TUI（`tui::run`）を動かす。タブの作成・検索・移動はしない。
