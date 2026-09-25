# 2026-09-25 `loom init` と workspace 登録の既定値

初回セットアップの手数を減らすため、設定ファイルの雛形を作る `loom init` を追加し、`loom workspace add` を引数なしで使えるようにした。あわせて、設定が無い・workspace が見つからないときのエラーに次に打つべきコマンドを示すようにした。現状の仕様は `docs/architecture/config.md` を参照。

## 決めたこと

- **workspace のパスは git のトップレベルを既定にする**: リポジトリのサブディレクトリで `loom workspace add` しても、エージェントを起動したいのは通常リポジトリのルートなので、`--path` が無ければ `git rev-parse --show-toplevel` を使い、git 管理外ならカレントディレクトリにした。`loom add` の自動判定が既に同じ git ヘルパーを使っているので、それを再利用した。
- **ID はディレクトリ名を既定にする**: ほとんどの場合プロジェクト名＝ディレクトリ名で足りる。ディレクトリ名が ID の規則（空・`loom`・空白や制御文字・`:`）に反する場合は勝手に変換せず、エラーにして ID の明示を求める。変換規則を持つと、Zellij のタブ名と設定上の ID の対応が分かりにくくなるため。
- **登録済み ID への `workspace add` はエラーにする**: 以前は path と agent を黙って上書きしていた。ID を省略できるようになると、別のディレクトリで同名のリポジトリを登録したときに既存の登録を気付かず書き換えてしまうので、既存の path を示してエラーにした。変更したい場合は設定ファイルを直接編集する。
- **`loom init` は既存ファイルを上書きしない**: `create_new` で開き、既にあれば中身に触れずにエラーにする。手で編集した設定を消す事故を避けるため、`--force` のような上書き手段も用意していない。テンプレートは README の設定例と同じ内容（`claude` / `codex` と `default_agent = "claude"`）。
- **設定ファイルが無いことが原因のエラーだけ `loom init` を促す**: `loom add` で workspace を解決できないとき、および `loom workspace add --agent` で設定ファイルが無いとき。`--agent` 無しの `workspace add` は従来どおり設定ファイルを新規作成する。core 側（agent 解決失敗でタスクが `failed` になる経路）には手を入れていない。
- **`loom add` の自動判定失敗時は登録コマンドを提示する**: `workspace add` の既定と同じ規則で求めた ID を埋めた `loom workspace add <ID>` と、`loom add -w <workspace>` を示す。
- `[workspaces]` テーブルを新規に作るときは暗黙テーブルにし、空の `[workspaces]` 見出しが出力されないようにした。

## 作ったもの

- `src/config.rs`: `CONFIG_TEMPLATE` / `init_config`、`resolve_workspace_path`（`--path` > git トップレベル > cwd、canonicalize）、`workspace_id_from_path`。`ConfigError` に `InvalidDerivedWorkspaceId` / `WorkspaceAlreadyRegistered` / `MissingConfig` / `ConfigAlreadyExists` を追加。`add_workspace` は既存 ID をエラーにし、`--agent` 指定かつ設定ファイル無しを `MissingConfig` にする。
- `src/cli.rs`: `loom init` サブコマンド、`workspace add` の `ID` を省略可能にして登録内容を表示、`loom add` のエラーメッセージ改善、clap のヘルプ文言。

## 検証

- ユニットテスト（`src/config.rs`）:
  - `resolve_workspace_path_uses_git_toplevel_from_subdir`（git リポジトリのサブディレクトリ → トップレベルとそのディレクトリ名）
  - `resolve_workspace_path_without_git_uses_cwd`
  - `resolve_workspace_path_explicit_path_overrides_git`（絶対パス・相対パスの `--path`）
  - `derived_workspace_id_invalid_is_error`（`/`・`loom`・空白・`:`）
  - `add_workspace_duplicate_id_is_error_without_writing`
  - `add_workspace_with_agent_on_missing_config_points_to_init`
  - `init_config_writes_valid_template`（読み込み・検証が通り、その後 `workspace add --agent codex` もできる）
  - `init_config_refuses_to_overwrite`
- 一時ディレクトリの `ZELLOOM_CONFIG` / `ZELLOOM_STATE_DIR` と存在しないソケットで、`loom init`（2 回目はエラー）、git リポジトリのサブディレクトリからの引数なし `workspace add`、重複登録のエラー、空白を含むディレクトリ名のエラー、設定ファイル無しでの `loom add` / `workspace add --agent` のエラー、自動判定失敗時の提案表示を手で確認した。
- `cargo build` / `cargo test` / `cargo clippy --all-targets` / `cargo fmt` がすべてクリーン。
