# 2026-09-29 TUI と runner で複数行のタスクを入力する

これまで複数行の本文を入れられるのは `loom add`（引数か標準入力）と HTTP アダプタだけだった。TUI の入力欄は 1 行だけで、複数行を貼り付けると改行が `Enter` として届いてしまう。1 行目で確定したあと、残りの文字が通常モードのショートカット（`dd`・`q` など）として解釈される危険があった。runner の待機中の入力は canonical モードの 1 行 = 1 タスク。現状の仕様は `docs/architecture/tui.md` と `docs/architecture/runner.md` を参照。

## 決めたこと

- **空の入力で Enter を 2 回押すとエディタを開く**: TUI と runner で操作をそろえた。1 回目で案内を出してから開くので、うっかり Enter を押してもいきなりエディタに飛ばない。新しいキーを覚える必要もない。改行キー（`Shift+Enter` など）は、Zellij の中では kitty keyboard protocol に頼れないので採用しなかった。
- **runner は canonical モードのまま**: raw モードと自前の行エディタにすると貼り付けも扱えるが、端末ドライバに任せている行編集を全部書き直すことになる。複数行はエディタに任せ、1 行入力は今のままにした。
- **TUI は bracketed paste も有効にする**: 貼り付けの事故をなくすため。通常モードでの貼り付けは無視する。
- **編集ではエディタに元の本文を入れる**: 入力欄を空にしないとエディタ待ちにならないので、長い本文を消しやすいよう `Ctrl+U` を追加した。
- **エディタの解決と起動は `loom config edit` と共通化した**: `src/editor.rs` に移した。
- **runner ではエディタを agent と同じ手順で前面に出す**: runner は `SIGINT` などを無視しているため、同じプロセスグループのまま起動するとエディタにもそれが引き継がれる。別のプロセスグループにして既定の動作に戻し、フォアグラウンドを渡す。

## 作ったもの

- `src/editor.rs`: `name` / `edit` / `compose`（一時ファイルを作ってエディタで開き、前後の空白を除いた本文を返す）。
- `src/tui/app.rs`: `Mode::Input` の `editor_armed`、`Action::OpenEditor`、`handle_paste`、`finish_editor`、`Ctrl+U`、確定処理を `submit_input` にまとめた。
- `src/tui/mod.rs`: bracketed paste の有効化と、`edit_in_terminal`（端末を戻してエディタを開き、TUI を再開する）。
- `src/tui/ui.rs`: 一覧と入力行で改行を `⏎` で表示。
- `src/runner.rs`: 待機中の入力にエディタ待ちを追加。`edit_in_foreground`、agent 起動と共通の `take_foreground_on_exec`。

## 検証

- `src/tui/app.rs` / `src/tui/ui.rs` の単体テスト: エディタ待ちとその解除、編集時の初期本文、エディタの結果の確定（`ws:` の解釈を含む）、空の結果、貼り付け（`\r\n`・`\r` をそろえる、通常モードでは無視）、改行の `⏎` 表示。
- `tests/runner_pty.rs`: pty 上の実 runner で空行を 2 回入力し、1 回目で案内が出ること、2 回目で偽のエディタ（`EDITOR`）が書いた複数行の本文がタスクとして追加され、agent にそのまま渡ることを確認。
- `cargo test` / `cargo clippy --all-targets` / `cargo fmt --check` がすべてクリーン。
- 実際の Zellij 上での TUI からのエディタ起動と貼り付けは未確認。
