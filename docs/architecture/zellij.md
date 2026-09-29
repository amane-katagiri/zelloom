# Zellij 連携

`src/zellij.rs`（`zellij` CLI の薄いラッパ）と `src/launcher.rs`（`TabLauncher` の実装）。対象セッションは環境変数ではなく `--session <name>` で明示する。

## 使うコマンド

| コマンド | 用途 | 呼び出し箇所 |
|---|---|---|
| `zellij --session <name> action list-tabs -a -j` | 既存タブの一覧を JSON で取得（`tab_id` / `name` / `position`） | `Zellij::list_tabs` |
| `zellij --session <name> action list-panes -a -j` | 全ペインの一覧を JSON で取得（`tab_id` / `is_plugin` / `exited` / `exit_status` / `terminal_command` / `pane_command` / `pane_cwd` など） | `Zellij::list_panes` |
| `zellij --session <name> action new-tab --name <name> --cwd <path> --no-focus -- <argv...>` | workspace 用タブを作り、その中で `loom runner ...` を起動する。標準出力に出る新しいタブの ID を返す | `Zellij::new_tab` |
| `zellij --session <name> action move-tab --tab-id <id> left` | 作ったタブを管理タブの隣へ寄せる（1 回で 1 つ左へ） | `Zellij::move_tab_left` |
| `zellij --session <name> action close-pane --pane-id terminal_<id>` | runner が正常終了するとき自分のペインを閉じる | `Zellij::close_pane`（`runner::close_own_pane`） |
| `zellij --session <name> action go-to-tab-name <name>` | 名前でタブへフォーカス移動 | TUI の `Enter`（`Zellij::go_to_tab_name`） |
| `zellij --session <name> action close-tab-by-id <id>` | 終了済み runner のタブを閉じる（その後に新しいタブを作る） | `ZellijLauncher::launch` |

すべて同期の `std::process::Command` 呼び出しで、`ZELLOOM_ZELLIJ` で `zellij` バイナリのパスを上書きできる（`Zellij` のメソッドは `zellij_bin()`、TUI は同じ環境変数を自分で読む）。`Zellij` のメソッドは `<name>` に core 起動時の `ZELLIJ_SESSION_NAME` を、TUI は TUI 自身の `ZELLIJ_SESSION_NAME` を使う。

## `--no-focus`

workspace タブは常にフォーカスを奪わずに作成する（`Zellij::new_tab` は常に `--no-focus` を付ける）。

## runner の argv

`zellij action new-tab -- <argv>` で起動したペインのプロセスは、core ではなく Zellij サーバプロセスの環境を継承する。ソケットパスは環境変数ではなく `loom` のグローバル `--socket` フラグとして argv で渡す。`ZellijLauncher::launch` は次の argv を組み立てる。

```text
<loom の実行ファイルの絶対パス> --socket <socket_path> runner --close-pane-on-exit <workspace_id>
```

`--close-pane-on-exit`（`launcher::CLOSE_PANE_FLAG`。`--help` には出ない runner 用の隠しフラグ）を付けた runner は、正常終了（終了コード 0）するとき `ZELLIJ_SESSION_NAME` と `ZELLIJ_PANE_ID` から自分のペインを特定して閉じる。タブのペインは runner だけなので、タブごと消える。エラーで終了した場合は閉じないので、ペインは終了コードとエラー表示を残したまま exited 状態で残る。フラグの無い runner（シェルから手で起動したものなど）はペインを閉じない。

詳細は [config.md](config.md#パスと環境変数)。

## workspace タブの再利用と作り直し

core が `ZELLIJ_SESSION_NAME` なしで起動されていると、`ZellijLauncher::launch` は Zellij を呼ばずにエラーを返す（タスクは `interrupted` になる）。セッション名があれば `list_tabs` と `list_panes` を取得し、`decide()` で分岐する。

runner タブは、名前が workspace ID に一致し、かつ **runner ペイン** を含むタブ。runner ペインは、プラグインでない（`is_plugin` が false）ペインのうち、`terminal_command`（ペインを起動したコマンド。空白区切りの 1 文字列）か `pane_command`（ペインの現在のフォアグラウンドのコマンド）のどちらかが次の形のもの。

```text
<パス>/<core 自身の実行ファイル名> [--socket <path>] runner [--close-pane-on-exit] <workspace_id>
```

- 末尾が ` runner <workspace_id>` か ` runner --close-pane-on-exit <workspace_id>` であること、その前の（` --socket ` 以降を除いた）部分のファイル名が core の実行ファイル名（`current_exe()` のファイル名。末尾の ` (deleted)` は除く）と一致することで判定する（`is_runner_command`）。
- 同じ名前のタブが複数あってもよい。runner ペインを含まない同名のタブ（ユーザーが自分で作った `loom` タブで TUI を動かしている場合など）は無視する。

分岐:

- 同名のタブのどれかに `exited` でない runner ペインがある → `AlreadyRunning`（何もしない）
- runner ペインを含む同名のタブはあるが、その runner ペインがすべて `exited` → `CloseThenCreate`（それらのタブをすべて閉じてから新しく作る）
- runner ペインを含む同名のタブが無い → `CreateTab`

## 新しいタブの位置

新しく作った runner タブは、`--no-focus` で末尾に作られたあと、管理タブの隣へ移される（`place_next_to_tui`）。

- 管理タブは、プラグインでないペインのうち `terminal_command` か `pane_command` が次の形のものを含むタブ（`is_tui_command`）。複数あれば位置（`position`）が最も左のもの。

  ```text
  <パス>/<core 自身の実行ファイル名> [--socket <path>]
  ```

  `--socket` がある場合はそのパスが core 自身のソケットと一致するもの、無い場合は core 自身のソケットが既定のソケット（`ZELLOOM_SOCKET`、無ければ既定パス。`paths::default_socket_path`）と一致するときだけ数える。別の core の TUI は管理タブとみなさない。
- 移動先は、管理タブの右隣から続く「この core の runner タブ」（ソケットが同じ条件で一致する runner ペインを含むタブ。workspace は問わない）の並びの直後。つまり管理タブの右に、作られた順に runner タブが並ぶ。
- タブを作ったあと `list_tabs` を取り直して `position` 順に並べ、現在の位置と移動先の差の回数だけ `move-tab --tab-id <id> left` を呼ぶ（`left_moves`）。ペインの情報はタブ作成前に取った `list_panes` の結果を使う。
- 管理タブが見つからない、またはすでに移動先にある場合は動かさない。移動に失敗してもタブの作成は成功扱いで、`core.log` にエラーを出すだけ。

### 制約: クライアントがアタッチしていないセッションでの挙動

`list-tabs` / `list-panes` は、そのセッションに Zellij クライアント（実際にアタッチしている端末）が1つも無いと、タブはあってもペインを1件も返さない、という実機での挙動が確認されている。この状態では runner ペインが見つからないので、`decide()` は常に `CreateTab` になり、終了済み runner のタブは閉じられずに残る。管理タブも見つからないので、新しいタブは末尾のままになる。runner の `close-pane` もこの状態ではタブを閉じない。詳細は [todo.md](../todo.md)。

## 素の `loom` 起動フロー

`loom`（サブコマンド無し）は次を行う（`src/cli.rs` の `run_bare_loom` と `ensure_core`）。

1. ソケットに接続できれば core は動いているとみなし、Zellij の有無に関係なく 3 へ進む。
2. 接続できなければ、`ZELLIJ_SESSION_NAME` が無い場合は「core は Zellij セッションの中から起動する必要がある」というエラーで終了する。ある場合は core をデタッチ起動する（`setsid` した子プロセスとして `loom --socket <path> core` を起動し、stdin は `/dev/null`、stdout/stderr は状態ディレクトリの `core.log` に追記する。TUI の端末は引き継がない）。ソケットができるまで最大 5 秒待つ。その間に core が終了したら `loom core exited during startup (<status>); see <core.log>` で終了する。
3. 実行した端末（ペイン）で、そのまま管理 TUI（`tui::run`）を動かす。タブの作成・検索・移動はしない。
