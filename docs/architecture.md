# アーキテクチャ

zelloom（バイナリ名 `loom`）は、対話型コーディングエージェントを「1 タスク = 1 対話セッション」で回すための小さなタスクキュー。既存の Zellij セッションの中で動き、workspace ごとに 1 つのタブでエージェントを起動する。

本書は現在のコードの構成と索引。詳細は `docs/architecture/` 以下を参照。

## コンポーネント

| コンポーネント | 起動方法 | 役割 |
|---|---|---|
| core | `loom core`（通常は `loom` がデタッチ起動。`loom stop` で停止） | Unix socket サーバ。タスクの永続化（SQLite）、グローバルキュー、scheduler、runner への指示を持つ唯一の状態所有者 |
| runner | `loom runner <workspace>`（core が Zellij タブで起動） | workspace タブに常駐し、タスクごとにエージェントを子プロセス（別プロセスグループ）として起動・停止する |
| TUI | 素の `loom`（実行した端末・ペインでそのまま動く） | core をポーリングして一覧表示し、操作をリクエストとして送るだけのクライアント |
| CLI | `loom add` / `loom done` / `loom list` / `loom stop` / `loom init` / `loom config edit` / `loom workspace ...` | 1 リクエスト 1 応答のクライアント。`init`・`config`・`workspace` は core を介さず設定ファイルを直接読み書きする。内部用の `core` と `runner` は `--help` に出さない |
| Zellij launcher | core 内（`launcher.rs`） | runner が未接続の workspace に対し、`zellij action` でタブを作って runner を起動する |
| HTTP アダプタ | core 内（`http.rs`）。設定に `[http]` があるときだけ起動 | `POST /tasks` を受け付け、core 自身の Unix socket にクライアントとして `enqueue` を送るだけ。Scheduler は直接呼ばない |

## プロセスと IPC

```text
 Zellij セッション
 ┌──────────────────────────────────────────────────────────────────────┐
 │ 任意のペイン   : loom（管理 TUI）                                     │
 │ [ws-a] タブ    : loom --socket S runner ws-a ──fork/exec──▶ agent     │
 │                   (端末のフォアグラウンドを agent に渡す)   (別 pgrp)  │
 │ [ws-b] タブ    : loom --socket S runner ws-b ──▶ agent                │
 │ 任意のペイン   : loom add ... / agent 内から loom done                │
 └───────────┬──────────────────────────────────────────────▲───────────┘
             │ JSON Lines over Unix socket S                │
             ▼                                              │ zellij --session <name>
      ┌──────────────┐        ┌──────────┐                  │   action new-tab ...
      │  loom core   │───────▶│ state.db │                  │
      │ (setsid 済み)│        │ (SQLite) │                  │
      └──────┬───────┘        └──────────┘                  │
             └──────────────────────────────────────────────┘
```

- クライアント（CLI / TUI / `loom done`）は接続ごとに JSON を 1 行送り、1 行受け取る。
- runner は `runner_attach` で長寿命接続を張り、core から `start` / `stop` イベントを受け、`agent_exited` を送る。
- core は runner が未接続の workspace にタスクを割り当てると、Zellij にタブを作って runner を起動し、接続を待つ。
- CLI（`loom add` / `loom done` / `loom list`）は、ソケットが無い・接続を拒否されたとき `loom core is not running (<socket>); start it with `loom` inside a Zellij session` を表示して非 0 で終了する（`client::ClientError::CoreNotRunning`）。
- `loom stop` は `shutdown` を送り、ソケットが接続を受け付けなくなるまで最大 5 秒待つ。core が動いていなければ `core is not running` を表示して正常終了する。`--force` が無ければ `running` のタスクがあるとき拒否され、そのタスク（workspace・本文・ID）を表示して非 0 で終了する。TUI は終了時に core も止めるかを確認する（[tui.md](architecture/tui.md)）。
- 詳細: [protocol.md](architecture/protocol.md) / [scheduler.md](architecture/scheduler.md) / [runner.md](architecture/runner.md) / [zellij.md](architecture/zellij.md) / [http.md](architecture/http.md)

## モジュール索引

| ファイル | 内容 | 詳細 |
|---|---|---|
| `src/main.rs` | clap でパースして `cli::dispatch` を呼ぶだけ | — |
| `src/lib.rs` | モジュール宣言 | — |
| `src/cli.rs` | サブコマンド定義と実装。素の `loom`（必要なら core をデタッチ起動してから、その場で TUI を実行）、`loom stop`、`loom config edit` | [zellij.md](architecture/zellij.md)、[config.md](architecture/config.md) |
| `src/paths.rs` | 設定・状態・ソケットのパス解決と `--socket` の上書き、上書きを無視した既定ソケット | [config.md](architecture/config.md) |
| `src/config.rs` | 設定の読み込み・検証、agent 解決、`init` のテンプレート書き込み、`workspace add` のパス・ID 決定と書き込み、cwd からの workspace 自動判定 | [config.md](architecture/config.md) |
| `src/protocol.rs` | リクエスト・応答・runner イベント・`Task` の型 | [protocol.md](architecture/protocol.md) |
| `src/client.rs` | 同期 Unix socket クライアント | [protocol.md](architecture/protocol.md) |
| `src/store.rs` | SQLite の `tasks` テーブルと操作 | [scheduler.md](architecture/scheduler.md) |
| `src/core/mod.rs` | core サーバ（既存ソケットの確認、ソケット待ち受け、接続ごとの処理、起動時の interrupted 化） | [protocol.md](architecture/protocol.md) |
| `src/core/scheduler.rs` | 各操作の実装、スケジューリング、runner 管理、attach タイムアウト、agent の argv・環境変数の組み立て、`TabLauncher` トレイトとテスト用の `NoopLauncher` | [scheduler.md](architecture/scheduler.md)、[config.md](architecture/config.md) |
| `src/launcher.rs` | `TabLauncher` の Zellij 実装（runner ペイン・管理 TUI ペインの判定、タブの再利用・作り直し判定、新しいタブを管理タブの隣へ移す） | [zellij.md](architecture/zellij.md) |
| `src/http.rs` | HTTP アダプタ（`POST /tasks`、Host/Origin チェック、CORS、core 自身の Unix socket へのクライアント呼び出し） | [http.md](architecture/http.md) |
| `src/zellij.rs` | `zellij --session <name> action ...` の薄いラッパ | [zellij.md](architecture/zellij.md) |
| `src/runner.rs` | runner 本体（待機中の端末からのタスク入力、シェル経由の起動、ジョブ制御、端末復元、停止シーケンス、自発終了時のプロンプト、正常終了時に自分のペインを閉じる） | [runner.md](architecture/runner.md) |
| `src/tui/mod.rs` | TUI のイベントループ、core 呼び出し、タブへのフォーカス | [tui.md](architecture/tui.md) |
| `src/tui/app.rs` | TUI の状態とキー処理 | [tui.md](architecture/tui.md) |
| `src/tui/ui.rs` | TUI の描画 | [tui.md](architecture/tui.md) |
| `tests/scheduler_flow.rs` | core を実ソケットで起動し、偽 runner で scheduler の流れを検証する結合テスト（`loom stop` 相当の停止と `cli::ensure_core` も含む） | — |
| `tests/runner_pty.rs` | pty の上で実際の `loom runner` を動かし、シェル経由の起動・rc ファイル・環境変数・argv・`loom done` による停止・待機中の端末からのタスク入力を検証する結合テスト（手元に無いシェルは飛ばす） | [runner.md](architecture/runner.md) |
| `tests/http_adapter.rs` | core を `[http]` 付きの実ソケットで起動し、生の HTTP/1.1 リクエストで `POST /tasks`・Host/Origin チェック・CORS・core 終了時の停止を検証する結合テスト | [http.md](architecture/http.md) |

## 設定とパスの要約

| 項目 | 既定値 | 上書き |
|---|---|---|
| 設定ファイル | `$XDG_CONFIG_HOME/zelloom/config.toml`（未設定なら `~/.config/...`） | `ZELLOOM_CONFIG` |
| 状態ディレクトリ | `$XDG_STATE_HOME/zelloom`（未設定なら `~/.local/state/...`）。`state.db` と `core.log` を置く | `ZELLOOM_STATE_DIR` |
| ソケット | `$XDG_RUNTIME_DIR/zelloom/default.sock`（未設定なら `/tmp/zelloom-<uid>/default.sock`） | `--socket` > `ZELLOOM_SOCKET` |

- 設定ファイルが無ければ空の設定として扱う（`loom init` でテンプレートから作れる）。core はリクエストやスケジュール判定のたびに設定を読み直す。ただし `[http]`（[http.md](architecture/http.md)）だけは core 起動時に読んだ内容のまま固定され、変更には core の再起動が要る。
- 設定キー、agent 解決、argv の組み立て、agent に渡す環境変数とシェル経由の起動、環境変数の一覧は [config.md](architecture/config.md)。
- TUI の画面とキー操作は [tui.md](architecture/tui.md)。
