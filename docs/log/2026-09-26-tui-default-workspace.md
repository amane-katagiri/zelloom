# TUI の既定 workspace

## 決めたこと

- TUI のタスク追加で `ws:` を省略したときに使う workspace を設定できるようにする。CLI（`loom add`）の自動判定には影響させない。
- 設定は `[tui] default_workspace`。`[workspaces]` に無い ID は `default_agent` と同じく設定エラーにする。
- 優先順位は `ws:` 指定 → `tui.default_workspace` → 選択中タスクの workspace。既定がある場合は選択中タスクから推測しない。
- TUI は設定ファイルを読まない方針を維持し、core の `status` 応答に `tui_default_workspace` を足して受け取る。

## 作ったもの

- `config::TuiConfig` と `Config::validate` の検証。
- `status` 応答の `tui_default_workspace`。
- TUI の `App::default_workspace` と `parse_add_input` の既定解決。`refresh` は `status` を構造体で受け取るようにした。

## 検証

- `cargo clippy --all-targets` 警告なし、`cargo test` 全件成功。
- 追加したテスト: 既定が選択中タスクより優先されること、未登録の `default_workspace` が設定エラーになること。
