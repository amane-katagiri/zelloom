# 2026-09-29 TUI のステータス行に古い表示が残る・画面がずれる問題を直す

TUI のステータス行（`last_message`）に、状態が変わったあとも古い表示が残る箇所があった。また `Enter` で running タスクのタブへ移動すると画面が上にずれることがあった。現状の仕様は `docs/architecture/tui.md` を参照。

## 決めたこと

- **エディタ待ちの案内は状態から描画する**: `last_message` に入れると、`Esc` などで解除されても残ってしまう。`Mode::Input` の `editor_armed` を見て描画時に出すことにし、解除と同時に消えるようにした。
- **タブ移動は zellij の出力を捕捉する**: `.status()` で起動していたため、zellij の出力が代替画面にそのまま書き込まれて画面がずれていた。ほかの zellij 呼び出しと同じ `Zellij` ラッパー（`.output()`）を使う。
- **タブ移動の成功メッセージは出さない**: タブが切り替わること自体が結果なので、`focused workspace ...` は出さずに `last_message` を消す。
- **エディタが空で戻ったらキャンセル扱い**: 案内を出す代わりに、TUI は `Esc` と同じく入力モードを抜け、runner は何も表示しない。
- **エラーメッセージは今までどおり**: 次の操作が成功するまで残す。

## 作ったもの

- `src/tui/app.rs`: `App::editor_armed`。エディタ待ちで `last_message` を使わないようにした。`finish_editor` の空の結果で通常モードに戻す。
- `src/tui/ui.rs`: エディタ待ちの間はステータス行に `editor::open_hint` を出す。
- `src/zellij.rs`: `Zellij::go_to_tab_name`。
- `src/tui/mod.rs`: `focus_zellij_tab` を `Zellij::go_to_tab_name` に置き換えた。
- `src/runner.rs`: エディタが空で戻ったときの表示をなくした。

## 検証

- `src/tui/app.rs` の単体テスト: エディタ待ちの状態、空の結果で通常モードに戻りメッセージが出ないこと。
- `cargo test` / `cargo clippy --all-targets` / `cargo fmt --check` がすべてクリーン。
- 実際の Zellij 上でのタブ移動時の表示は未確認。
