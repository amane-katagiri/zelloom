# 2026-09-29 待機中の runner タブからタスクを追加する

workspace のタブで runner が待機しているとき、そのタブに本文を打ち込んでタスクを追加できるようにした。これまではタブを見ていても、タスクを足すには管理画面か別のペインの `loom add` へ移る必要があった。現状の仕様は `docs/architecture/runner.md` を参照。

## 決めたこと

- **1 行 = 1 タスク、canonical モードのまま読む**: 行編集を自前で持たずに端末ドライバへ任せる。複数行の本文は入力できないが、待機中のちょっとした追加には十分と判断した。
- **読み取りはメインスレッドの `poll`**: 端末を読む専用スレッドにすると、agent が端末を持っている間もバックグラウンドから読み続けて入力を奪いうるので、自発終了時のプロンプトと同じく 100ms ごとの `poll` にした。
- **`enqueue` は runner 接続ではなく別接続で送る**: runner 接続に送ると、core が応答より先に同じタスクの `start` を流してくるので、応答との対応付けが複雑になる。
- **source は `runner`**: `sources.runner.auto_queue` で他の経路と別に扱えるようにした。
- **agent 起動前に未読入力を捨てる**: 打ちかけの行が agent の入力として流れ込むのを防ぐ。

## 作ったもの

- `wait_idle` / `read_input_line` / `enqueue_from_runner` と、待機時の案内・プロンプト表示、`start` 受信時の `tcflush`（`src/runner.rs`）。

## 検証

- `tests/runner_pty.rs`: pty 上の実 runner に空白だけの行と日本語を含む行を入力し、後者だけが workspace `a`・source `runner` のタスクとして追加されて agent が起動すること、完了後に再び待機して次の入力も受け付けることを確認。
- `cargo test` / `cargo clippy --all-targets` / `cargo fmt --check` がすべてクリーン。
