# runner タブの配置と自動クローズ

## 決めたこと

- 新しい runner タブは末尾ではなく管理タブ（TUI が動いているペインを含むタブ）の右隣に置く。すでに管理タブの右に runner タブが並んでいれば、その並びの直後に置く（作られた順に並ぶ）。
- Zellij の `new-tab` には位置の指定が無いので、作成後に返るタブ ID に対して `move-tab --tab-id <id> left` を必要な回数呼ぶ。
- 管理タブと runner の並びは core 自身のソケットに属するものだけ数える。同じセッションで別ソケットの core を動かしたとき、他方の TUI や runner タブを基準にしてしまうのを実機で確認したため。
- runner が正常終了（終了コード 0）したら自分のペインを閉じる。`new-tab --close-on-exit` はエラー終了でも閉じてしまい、エラー表示が読めなくなるので使わない。
- ペインを閉じるのは core が起動した runner だけ（隠しフラグ `--close-pane-on-exit`）。シェルから手で起動した runner がシェルごとペインを閉じないようにするため。
- タブの移動に失敗してもタスクの起動は止めない（`core.log` に出すだけ）。

## 作ったもの

- `Zellij::new_tab` がタブ ID を返すようにし、`move_tab_left` と `close_pane` を追加。
- `launcher.rs`: runner コマンドの解析（`--close-pane-on-exit` を許容し、ソケットも取り出す）、TUI コマンドの判定、移動回数を求める `left_moves`、`place_next_to_tui`。
- `paths::default_socket_path`（`--socket` の上書きを無視した既定ソケット）。
- `loom runner --close-pane-on-exit` と `runner::close_own_pane`。

## 検証

- `cargo clippy --all-targets` 警告なし、`cargo test` 全件成功。
- 追加したテスト: 管理タブの隣への移動回数、既存の runner タブの並びの後ろに置くこと、別 core の runner タブを並びに数えないこと、`position` 順で判定すること、TUI コマンドの判定とソケットの一致条件、フラグ付き runner コマンドの判定。
- 実機（Zellij 0.45、クライアントがアタッチしたセッション）で、別ソケット・別設定の core と TUI を立てて確認した。2 つの workspace のタスクを追加すると、タブが TUI のタブの右に作られた順に並んだ。`loom stop --force` 後、agent の終了を待って runner タブが閉じた。
- クライアントがアタッチしていないバックグラウンドセッションでは `move-tab` は効くが、`close-pane` ではタブが閉じなかった。
