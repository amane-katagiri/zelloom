# 2026-10-31 zelloom の外にいる監督役（司令塔）からワーカーを見守る

zelloom の前に「司令塔」役のエージェントを置き、ワーカー（zelloom のタスクとして動く agent）からの質問・確認・完了報告を受けて、答えが決まるものは司令塔が答え、方向性・本番変更・人の目に触れるものだけを人間に回す使い方を支えるための最小の仕組みを入れた。判断はすべて司令塔側（スキル・ワーカーへの instruction・hook）に置き、zelloom は「タスクを積む・状態と報告を流す・返事を届ける」だけにとどめた。現状の仕様は `docs/architecture/protocol.md`・`scheduler.md`・`runner.md`・`config.md` を参照。

## 決めたこと

- **司令塔は zelloom のタスクにしない**: 司令塔自身の完了とワーカーの完了の意味が混ざらないよう、zelloom の外の別プロセスとして CLI だけで関わる。外部チャネル（通知・遠隔からの返事）も司令塔側の道具で扱い、`reply_to` は引き続き使わない。
- **人間の gate は 2 回**: 着手前は `--source conductor` と `[sources.conductor] auto_queue = false` で `received` に止め、人間が TUI で accept する。人の目に触れる操作の前はワーカーが `report gate` で止まり、司令塔が人間に回す。完了の判断（`loom done`）は gate のあと、司令塔が `ready` を確かめてから行う。
- **attention は core のメモリにだけ持つ**: イベントログは持たない。司令塔はコンテキストを定期的にクリアする前提なので、必要なのは「今なにを待っているか」のスナップショットで、`loom watch` の最初のスナップショットがそれを返す。core を再起動するとタスクは interrupted になって仕切り直しになるので、永続化はしない。
- **attention と watch のイベントは store に集約する**: 状態を変える経路はすべて store を通るので、running を離れたときの attention の消去とイベントの送出を store の更新メソッドで行い、送り漏れを防いだ。スナップショットと購読開始は同じロックの中で行う。
- **watch の遅延は切断で扱う**: 遅れた購読者にはエラー行を送って切る。再接続すれば新しいスナップショットで追いつける。
- **send は question / gate / ready のときだけ受け付ける**: Claude Code に `zellij action write-chars` で打ち込むと、確認ダイアログが出ている間は Enter が選択中の選択肢を決定してしまう（手で確認）。ワーカーが自分でターンを終えて入力欄で待っている状態だけを許し、`blocked` と attention 無しは拒否する。runner の現在のタスクかどうかの確認と attention の消去を core の中で行うのは、タスクが切り替わった直後に次のワーカーへ打ち込む事故を防ぐため。
- **Enter は別の書き込み**: `write-chars` の本文中の LF は入力欄の改行になり送信されない（手で確認）。CR は途中で送信してしまうので LF に正規化し、最後に `write 13` を送る。本文が `-` で始まっても引数と解釈されないよう `--` を付ける。
- **instruction を agent ごとに上書きできるようにした**: ワーカーには「`loom done` を自分で実行しない・`loom report` で報告して待つ」という別の約束が要る。人間が直接積むタスクは従来の instruction のまま使える。
- **並列実行は zelloom に足さない**: 当面は worktree ごとに workspace を登録する運用で、スキル側の手順として書いた。

## 作ったもの

- `loom list --json`、`loom show <id> [--json]`、`loom add --source <type>`、`agents.<name>.instruction`。
- `report` / `watch` / `send` リクエスト、`Task.attention`、runner への `input` イベントと、runner が自分のペインに打ち込む処理（`src/protocol.rs`・`src/store.rs`・`src/core/`・`src/runner.rs`・`src/zellij.rs`・`src/cli.rs`）。
- 司令塔用のスキル `skills/zelloom-conductor/`（ワーカー agent の設定例と、権限確認の通知を `loom report blocked` にする Claude Code の hook の例を含む）。

## 検証

- 手作業（zellij 0.45.1、Claude Code）: フォーカスを移さずに開いたタブの Claude Code へ `write-chars --pane-id` で 1 行・複数行・日本語を打ち込み、別に送った Enter で送信されること、作業中に送ると送信待ちに積まれてターン後に処理されること、入力途中の文字列には連結されること、ダイアログでは Enter が選択肢を決定することを確認した。
- 結合テスト: report / watch のスナップショットと差分 / send の受け付け条件と二重送信の拒否 / 遅延時の切断（`tests/scheduler_flow.rs`）、偽の `zellij` に対する runner の呼び出し引数（`tests/runner_pty.rs`）。
- 通し確認: 別のソケット・設定の core で、ワーカー agent（Claude Code）に `--source conductor` のタスクを積み、accept → ワーカーの `report question` を `loom watch` で受信 → `loom send` で返答 → 2 回目の send が拒否される → ワーカーがファイルを作って `report ready` → `loom done` → runner のタブが閉じる、までを確認した。`blocked` の hook はこの通し確認では発火していない（権限の確認が出なかった）。
- `cargo test` / `cargo clippy --all-targets` / `cargo fmt` がクリーン（`tests/http_adapter.rs` のポート競合による既存の不定な失敗は `docs/todo.md` に記載）。
