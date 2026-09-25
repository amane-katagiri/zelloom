# 残タスク

着手したら消し、見つけたら足す。完了したものは `docs/log/` へ書く。

## 未実装（`plan.md` の「後回し」のまま）

- Nostr adapter
- 外部チャンネルへの完了通知（`Task.reply_to` はプロトコル・ストアに保存されるだけで、どこからも読まれていない）
- Workspace 内での並列実行（`workspace.max_parallel` は 1 以外を設定するとロードエラーになる）
- Git worktree 管理
- GitHub Issues 連携
- Slack / Discord 連携
- Web UI
- 自動タスク分解
- マルチエージェント協調

## 既知の制限

- タブ起動（`TabLauncher::launch`）は `spawn_blocking` で実行するので tokio のワーカースレッドは塞がないが、`schedule()` はその完了を `scheduling_lock` を保持したまま待つ。`zellij` の応答が遅いと、その間 `schedule()` を呼ぶ操作（`enqueue` など）の応答も遅れる。

## 未実装の機能・仕上げ

- タスク単位の agent override が TUI に無い。CLI の `loom add --agent` は対応済みで、設定・プロトコル・スケジューラ側も `Task.agent` を扱えるが、TUI の追加操作（`Action::Enqueue`）は常に `agent: None` を送っており、入力欄に agent を指定する手段が無い。
- Zellij のタブ名に実行中を示す `●` 等のインジケータを付ける機能が無い。
- クライアント（実際にアタッチしている端末）が1つも無い Zellij セッションでは、`list-tabs` はタブを返すが `list-panes` がペインを1件も返さない、という実機での挙動が確認されている。この状態では `ZellijLauncher::decide` が runner ペインを見つけられず、終了済み runner のタブを閉じずに新しいタブを作るので、同名のタブが溜まりうる。また runner ペインの判定に使う `terminal_command` / `pane_command` の形式（空白区切りの argv）は Zellij 0.45 のソースで確認しただけで、実機の出力では未検証（[architecture/zellij.md](architecture/zellij.md) 参照）。
- TUI の一覧に長いリストのスクロールが無い。`ratatui::widgets::List` を `ListState`/オフセット無しで描画しているため、セクションの高さを超えるタスクは画面から溢れて見えなくなる。
- TUI から `done` / `cancelled` / `rejected` のタスクを見る手段、`cancelled` のタスクを retry する手段が無い（見出しに `done` / `cancelled` の全期間の件数が出るだけ）。
- HTTP アダプタ（[http.md](architecture/http.md)）に認証・トークンが無く、`http.listen` が loopback であることでしか守られていない。非 loopback に公開したい場合に備えて何らかのトークン機構が要る。
- HTTP アダプタにタスクの状態を取得する GET エンドポイントが無い。`POST /tasks` で作成した直後のレスポンスでしか状態がわからない。
