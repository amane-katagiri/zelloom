# 2026-09-26 instruction を任意の引数で渡す（`instruction_args`）

Codex には instruction を渡す専用フラグが無く、`instruction_flag`（フラグ 1 個 + instruction）では表せなかったため、タスク本文に連結して渡していた。instruction を埋め込む引数テンプレートに置き換えた。現状の仕様は `docs/architecture/config.md` を参照。

## 決めたこと

- **`instruction_flag` を廃止して `instruction_args` に置き換える**: 文字列配列で、各要素の `{instruction}` を instruction に置き換えて `command` と task 本文の間に入れる。旧キーを読む互換処置は入れない（旧キーは未知のキーとして無視され、その agent は instruction を本文に連結する挙動になる）。
- **Codex は `-c developer_instructions={instruction}`**: 標準のシステムプロンプトを残したまま developer ロールのメッセージとして足せる。`model_instructions_file` はベースのシステムプロンプトを丸ごと置き換えるので使わない。`-c` の値は TOML として解釈できなければ生の文字列として扱われ、instruction は `You are ...` で始まるので常に生の文字列になる。
- **`{instruction}` を含まない `instruction_args` は設定エラー**: instruction が黙って落ちるのを防ぐ。

## 作ったもの

- `AgentConfig.instruction_args`、`config::INSTRUCTION_PLACEHOLDER`、`Config::validate` での検証（`src/config.rs`）。
- `build_argv` の置換（`src/core/scheduler.rs`）。
- `loom init` のテンプレートで `claude` と `codex` の両方に `instruction_args` を設定。

## 検証

- 単体テスト: テンプレートの `instruction_args`、`{instruction}` を含まない場合の設定エラー、`build_argv` の置換と引数順。
- `tests/runner_pty.rs` の設定を `instruction_args` に更新し、agent に渡る argv が従来と同じであることを確認。
- `codex debug prompt-input -c "developer_instructions=<複数行の instruction>" hello` で、instruction が改行を保ったまま `role: developer` のメッセージとして入ることを確認した。
- `cargo test` / `cargo clippy --all-targets` / `cargo fmt` がすべてクリーン。
