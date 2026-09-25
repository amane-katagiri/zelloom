# 2026-09-26 TUI 終了時の core 停止確認

TUI を `q` で閉じても core は動き続け、止めるには別途 `loom stop` が必要だった。TUI を閉じるときに core も止めるかをその場で選べるようにした。現状の仕様は `docs/architecture/tui.md` を参照。

## 決めたこと

- **`q` は確認モードに入る**: 入力行に `Stop core too? ... [y]es / [n]o / [Esc] cancel` を出し、`y` で core を止めて終了、`n` で core を残して終了、`Esc` で通常モードへ戻る。それ以外のキーは無視する（誤って別の操作が走らないように）。
- **core に到達できないときは即終了**: 止める対象が無いので聞く意味が無い。到達可否はポーリングで既に持っている `core_reachable` を使う。
- **`Ctrl+C` も 1 回目は `q` と同じ**: 確認中にもう一度 `Ctrl+C` を押すと core を残して終了する（`n` と同じ）。入力モード中の `Ctrl+C` も入力を破棄して確認モードに入る。どのモードでも `Ctrl+C` 2 回で core に触れずに抜けられる。
- **実行中タスク数はプロンプトに出す**: 止めると `interrupted` になることを知らせるため。数は直前のポーリング結果を使う。
- **`force` はプロンプトが実行中タスクを示したときだけ true**: 利用者はその件数を見て了承しているので、`force: true` で止める。0 件と表示していたのに、その後タスクが始まって core が拒否した場合は、了承していない中断を避けるため `force: false` のまま失敗させ、TUI に留まってステータス行に拒否理由を出す。
- **停止処理は `loom stop` と共通**: `cli::stop_core` をそのまま呼ぶ。停止完了（ソケットが閉じる）まで最大 5 秒ブロックするので、呼ぶ前に `stopping core...` を描画しておく。エラー文は複数行になり得るので、1 行のステータス行向けに改行を空白に置き換える。

## 作ったもの

- `Mode::ConfirmQuit { running }`、`Action::StopCoreAndQuit { force }`、`App::confirm_quit_prompt`（`src/tui/app.rs`）。
- 確認プロンプトの描画（`src/tui/ui.rs`）と、`StopCoreAndQuit` の実行（`src/tui/mod.rs`）。

## 検証

- 単体テスト（`src/tui/app.rs`）: 実行中タスクの有無による確認モードとプロンプト文、`y` の `force`、`n`・`Esc`・その他のキー、core 到達不可時の即終了、`Ctrl+C` の 1 回目/2 回目、入力モード中の `Ctrl+C`。
- 描画テスト（`src/tui/ui.rs`）: 確認プロンプトが最下行に出る。
- 一時的な設定・状態ディレクトリと専用ソケットで core を起動し、擬似端末上の TUI で `q`→その他のキー（終了しない）、`q`→`Esc`（終了しない）、`Ctrl+C`×2（終了し core は残る）、`q`→`y`（終了し core が止まってソケットが消える）、core 停止後の `q`（即終了）を確認した。
- `cargo build` / `cargo test` / `cargo clippy --all-targets` / `cargo fmt` がすべてクリーン。
