# 2026-09-25 `loom stop` と内部サブコマンドの非表示

core を止める手段が `shutdown` リクエストを直接送るしかなかったので、`loom stop [--force]` を追加した。あわせて、利用者が直接打つことのない `loom core` / `loom runner` を `--help` から隠した。現状の仕様は `docs/architecture.md`、`docs/architecture/protocol.md`、`docs/architecture/scheduler.md` を参照。

## 決めたこと

- **`core` と `runner` は隠すだけで残す**: `loom` が core をデタッチ起動し、core が Zellij タブで runner を起動するときに使うので、サブコマンドとしては必要。clap の `hide = true` で `--help` の一覧からだけ外した。`tui` は管理タブを手で開き直す用途があるので表示したままにした。
- **実行中タスクの確認は core 側で行う**: `shutdown` に `force`（省略時 false）を足し、false なら core が `running` のタスクを調べて、あれば拒否する。クライアント側で `list` してから `shutdown` を送ると、その間に次のタスクが始まり得る。確認とシャットダウン開始を `scheduling_lock` の中で行い、以後 `schedule()` は何もしないようにしたので、確認後に新しいタスクが割り当てられることはない。
- **拒否時は `ok:false` の応答の `data` に実行中タスクを入れる**: エラー文字列だけではクライアントが一覧を表示できないため。`loom stop` は workspace・本文・ID を並べ、`--force` を案内して非 0 で終了する。
- **core が動いていないときの `loom stop` は成功扱い**: 目的の状態（core が止まっている）はすでに満たされているので、`core is not running` を表示して 0 で終了する。ソケットファイルが残っていても接続できなければ同じ扱い。
- **停止の完了はソケットが接続を受け付けなくなることで判断する**: `shutdown` の応答は停止開始の時点で返るので、`loom stop` はそのあと最大 5 秒、接続できなくなるまで待つ。間に合わなければエラーにする。
- **shutdown 時に全接続を閉じる**: これまでは待ち受けループを抜けて `run` から戻るだけで、開いている接続（特に runner の長寿命接続）はプロセス終了（ランタイムの破棄）まで閉じられる保証が無かった。`Notify` を `watch` に置き換え、接続ごとの読み込みを shutdown と `select!` させて抜けるようにし、`run` は接続タスクを `JoinSet` で持って、ソケットファイルを消したあと全部の終了を待つ。runner の接続は通常の切断として後始末されるので、実行中だったタスクはその場で `interrupted` になる。`Notify::notify_waiters` は待っている future が無いと通知が消えるが、`watch` は状態として残るのでこの取りこぼしも無くなった。
- **runner 側は変更しない**: 切断時の既存の動き（待機中なら終了、agent 実行中なら agent の終了を待ってから終了、agent は止めない）がそのまま `--force` 後の期待する動きなので。

## 作ったもの

- `Request::Shutdown { force }` と `Scheduler::shutdown`（`src/protocol.rs`、`src/core/scheduler.rs`）。
- core の接続処理の shutdown 対応と接続の待ち合わせ（`src/core/mod.rs`）。
- `loom stop [--force]` と `cli::stop_core`（`src/cli.rs`）。
- 既存テストの後片付けの `shutdown` は `force: true` にした（実行中タスクを残したまま止めるものがあるため）。

## 検証

- 結合テスト（`tests/scheduler_flow.rs`）
  - `stop_is_refused_while_a_task_is_running_unless_forced`: 実行中タスクがあると `--force` 無しの停止が拒否され、メッセージにタスク ID・本文・`--force` が含まれ、core は応答を続ける。`--force` で停止すると、実行中・待機中どちらの偽 runner の接続も閉じられ、core のスレッドが終わり、ソケットファイルが消える。再起動後、タスクは `interrupted`。何も実行していなければ `--force` 無しで止まる。
  - `stop_without_a_core_reports_not_running`
- 一時的な設定・状態ディレクトリと専用ソケットで core を起動し、Python の偽 runner を attach させて、`--help` の表示、core 無しの `loom stop`、実行中タスクがあるときの拒否、`--force` での停止（偽 runner 側で切断を確認、ソケットファイルの削除）、再起動後の `interrupted` を手で確認した。
- `cargo build` / `cargo test` / `cargo clippy --all-targets` / `cargo fmt` がすべてクリーン。
