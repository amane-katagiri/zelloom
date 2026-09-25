# 2026-09-26 ワンショットの agent（`oneshot`）

`claude -p` のように本文を受け取って作業し、自分で終了する agent でタスクを順番にこなせるようにした。これまでは agent が終了するたびに runner が `[d]one / [r]estart / [f]ailed?` を出して入力を待つので、自動では次へ進まなかった。また、固定の instruction は「ユーザーの確認を待って `loom done` する」という対話前提の内容で、ワンショットには合わなかった。現状の仕様は `docs/architecture/config.md` と `docs/architecture/runner.md` を参照。

## 決めたこと

- **agent ごとの `oneshot` フラグにする**: ワンショットかどうかは起動するコマンド（`-p` の有無など）で決まるので、agent の設定に置く。`ResolvedAgent.oneshot` で runner に伝える。
- **終了コードで結果を決める**: 0 なら `done`、それ以外（シグナルによる終了や `wait` の失敗を含む）は `failed`。`restart` は自動では選ばない。
- **instruction は渡さない**: 対話前提の文なので、ワンショット用の別の文も用意しない。`instruction_args` との同時指定は、指定が黙って無視されるのを防ぐため設定エラーにする。
- **出力の保存はしない**: agent に端末をそのまま渡す今の作りを変える必要があるので、今回は見送った。

## 作ったもの

- `AgentConfig.oneshot` と `Config::validate` での検証（`src/config.rs`）。
- `build_argv` で oneshot のとき `command + [本文]` にする（`src/core/scheduler.rs`）。
- `ResolvedAgent.oneshot`（`src/protocol.rs`）。
- runner の自発終了時に、oneshot なら `oneshot_outcome` で終了コードから結果を決めて送る（`src/runner.rs`）。

## 検証

- 単体テスト: oneshot と `instruction_args` の同時指定が設定エラーになること、oneshot の argv が `command + [本文]` になること。
- `tests/runner_pty.rs`: pty 上の実 runner で、終了コード 0 のタスクが `done`、3 のタスクが `failed` になり、プロンプトが出ないことを、シェル経由と直接起動の両方で確認。
- `cargo test` / `cargo clippy --all-targets` / `cargo fmt --check` がすべてクリーン。
