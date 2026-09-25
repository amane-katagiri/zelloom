# 2026-09-25 MVP 実装

`docs/plan.md` 第 2 部の方針で MVP を一通り実装し、テストと実 Zellij での通し検証を行った。現状の仕様は `docs/architecture.md` 以下を参照。

## 決めたこと

- **言語・構成**: Rust（edition 2024）の単一バイナリ。crate 名は `zelloom`、生成するバイナリ名だけ `loom`。設定・状態・ソケットのパスと環境変数（`ZELLOOM_*`）は `zelloom` で統一した。
- **永続化**: rusqlite（`bundled`）で SQLite の `tasks` テーブル 1 つ。キュー順は `position` 列で持つ。
- **IPC**: Unix socket 上の JSON Lines。通常のリクエストは 1 行 1 応答、runner だけ `runner_attach` で長寿命接続にして core からイベントを push する。
- **runner の形**: runner を workspace タブ内の常駐プロセスにし、agent をシェルのジョブ制御と同じ形（別プロセスグループ + `tcsetpgrp`）で子として起動する。タブやペインを agent ごとに作り直さずに済み、agent の終了後に端末状態を runner が掃除できる。
- **ソケットは argv で渡す**: `zellij action new-tab` で起動したペインの環境は呼び出し元ではなく Zellij サーバのものになるため、`ZELLOOM_SOCKET` を環境変数で渡しても runner / TUI に届かない。そこでグローバルオプション `--socket` を追加し、タブ起動時の argv に入れた。
- **`stop` のあとは `agent_exited` を送らない**: `complete` / `fail` / `cancel` で `stop` を送る時点で core は状態を確定しているため、runner が結果を送り返す必要がない。`agent_exited` は agent が自分で終了し、人間が runner のプロンプトで結果を選んだときだけ送る。
- **タスク単位の agent 指定**: `loom add --agent` と `enqueue` の `agent` フィールドとして実装した。解決順は タスク → workspace → `default_agent`。
- workspace の `max_parallel` は 1 以外を設定エラーにした。

## 作ったもの

- **core**（`src/core/`、`src/store.rs`）: ソケットサーバ、SQLite ストア、グローバルキューの scheduler（workspace ロックと全体上限）、runner 管理、runner 接続待ちの attach タイムアウト、起動時の `running` → `interrupted`、agent の argv / env の解決と loom 用 instruction の付加。
- **runner**（`src/runner.rs`）: ジョブ制御による agent 起動、termios と端末モードの復元、SIGTERM → SIGKILL の停止シーケンス、自発終了時の `[d]one / [r]estart / [f]ailed` プロンプト、SIGHUP / SIGTERM と core 切断の扱い。
- **Zellij 連携**（`src/zellij.rs`、`src/launcher.rs`）: `zellij action` のラッパ、workspace タブの作成・再利用・作り直し、素の `loom` による core のデタッチ起動と管理タブの作成。
- **TUI**（`src/tui/`）: RUNNING / QUEUED / INBOX / INTERRUPTED の一覧と、追加・編集・削除・並べ替え・accept / reject・完了・retry・cancel、実行中タブへの移動。
- **CLI**（`src/cli.rs`）: `add`（cwd からの workspace 自動判定、標準入力からの本文）、`done`、`list`、`workspace add` / `workspace list`。

## 検証

- 単体テスト 63 件（config / store / protocol / paths / launcher の判定 / zellij の JSON 解析 / TUI のキー処理と描画）と、core を実ソケットで起動して偽 runner をつなぐ結合テスト 4 件（`tests/scheduler_flow.rs`）がすべて通ることを確認した。
- runner を pty 上で動かし、agent の起動・フォアグラウンドの受け渡し・停止・自発終了時のプロンプトを確認した（この検証コードはリポジトリに含めていない）。
- テスト用に分離したバックグラウンドの Zellij セッションで、偽 agent を使って通しで動かした。`loom` による core 起動と管理タブ作成、タスク投入による workspace タブと runner の起動、`loom done` による完了と次タスクの開始を確認した。
  - この検証中に、Zellij クライアント（実際にアタッチしている端末）が1つも無いバックグラウンドセッションでは `list-tabs` はタブを返すが `list-panes` がペインを1件も返さないことに気づいた。そのため `launcher.rs` の「終了済み runner ペインを検出してタブを閉じてから作り直す」分岐は、この検証では実際には踏まれておらず、現状はユニットテストの合成データでのみ検証できている（`docs/todo.md` に記載）。

## レビューで見つけた不具合と修正

1. **attach タイムアウトが終わったタスクを `interrupted` で上書きした**。タイムアウトは「runner の現在のタスクがこのタスクか」だけを見ていたため、attach してすぐ完了したタスクも「未 attach」と区別できなかった。ストアに条件付き更新 `update_status_if`（`running` のときだけ `interrupted` にする）を足し、さらにタスクごとの launch generation を持って、`retry` などで起動し直した後に古いタイマーが発火しても何もしないようにした。あわせて、タブ起動自体が失敗した場合はタイムアウトを待たずにすぐ `interrupted` にするようにした。結合テスト 2 件で再発を防ぐ。
2. **`complete` / `fail` / `cancel` がどの状態のタスクにも効いた**。`queued` のタスクを完了扱いにできてしまったので、`running` と `interrupted` のときだけ受け付けるようにした。
3. **`schedule()` の同時実行で同じタスクを二重に開始しうる**。別々の接続からの `enqueue` と完了が同時に来ると、両方が「このタスクが次で、workspace は空いている」と判断しうる。`schedule()` 全体を `scheduling_lock` で直列化した。

## 実装中に直した runner の不具合

- core は `stop` の直後に次のタスクの `start` を送ることがある。runner の待機ループが自分のタスクの終了だけを待っていたため、その間に届いた `start` を捨ててしまい、次のタスクが始まらなかった。関係ないメッセージを保留キューに戻せる `EventStream` を作り、待機が終わってから処理するようにした。

## レビュー後の追加修正

1. **タブ起動に失敗すると core が止まった**。症状: Zellij セッション外で起動した core に、runner が未接続の workspace のタスクを入れると `enqueue` の応答が返らず、以後のリクエストもすべて止まる。原因: `schedule()` は `scheduling_lock`（`tokio::sync::Mutex`、再入不可）を保持したまま `start_task` を呼び、`start_task` は `TabLauncher::launch` の失敗時にもう一度 `schedule()` を呼んで同じロックを待っていた。修正: 内側の `schedule()` 呼び出しを削除し、外側のループに続きを任せた。ロック保持中に `schedule()` を呼ぶ経路が他に無いことも確認した（attach タイムアウトは別タスク、runner の接続・切断はロック外から呼ぶ）。あわせて `launch` を `spawn_blocking` で実行し、`zellij` の終了待ちで tokio のワーカースレッドを塞がないようにした。`start_task` の `queued` → `running` / `failed` の更新は `queued` からの条件付き更新にし、判定の直後に取り消されたタスクを開始しないようにした。テスト: `failed_tab_launch_interrupts_the_task_and_core_keeps_responding`（常に失敗する `TabLauncher` で 2 件入れて両方 `interrupted` になり、`list` と別 workspace のタスク開始が応答することをタイムアウト付きで確かめる。修正前のコードでは 5 秒のタイムアウトで失敗することを確認した）。
2. **同じ workspace の runner が 2 つ接続すると管理が壊れた**。症状: 後から来た runner の登録で先の登録が上書きされ、先の接続が切れたときに後の runner の登録が消え、その実行中タスクが `interrupted` になる。原因: runner の登録を workspace ID だけで識別していた。修正: 登録に接続ごとの attach ID を持たせ、切断時の後始末は自分の登録にだけ行うようにした。さらに、同じ workspace の runner が接続中のときの `runner_attach` はエラー応答を返して接続を閉じるようにした（runner はエラーを表示して終了する）。2 つ目を受け入れて切り替える方式も考えたが、どちらの端末で agent が動いているかが曖昧になるので、拒否するほうを選んだ。runner 接続の読み取りエラーで切断処理が飛ばされていた（`?` で抜けていた）のも直した。テスト: `second_runner_for_the_same_workspace_is_rejected`。
3. **`agent_exited` がタスクの状態を検査しなかった**。症状: TUI から完了させた直後に runner のプロンプトで `f` を押すと、確定済みの `done` が `failed` で上書きされうる。`restart` は `running` でないタスクにも `start` を送った。原因: `agent_exited` がストアを無条件に更新していた。修正: `agent_exited` は runner 接続からだけ受け付け、そのタスクが送信元 runner の現在のタスクで `running` のときだけ効くようにした（`restart` も同じ）。更新は `running` からの条件付き更新。条件を満たさなければエラーを返し、何も変えない。runner はエラー応答を表示して待機を続ける。`complete` / `fail` / `cancel` も検査した状態からの条件付き更新にした。テスト: `agent_exited_only_applies_to_the_runners_current_running_task`。
4. **`loom workspace add` が不正な設定を書き込めた**。症状: `--agent X` の `X` が `[agents]` に無くても書き込み、以後その設定全体が検証エラーになって core の `enqueue` なども失敗する。workspace ID `loom` は管理タブ名と衝突する。原因: 書き込み時に検証していなかった。修正: 書き込む前に、編集後の内容を読み込み時と同じ `Config::validate` で検証し、失敗したら何も書かない（ファイルや親ディレクトリも作らない）ようにした。workspace ID の検証（空・`loom`・空白・制御文字・`:` を拒否）を `Config::validate` に加えた。`:` を拒否するのは TUI の `ws: 本文` 記法で指定できなくなるため。テスト: `add_workspace_rejects_unknown_agent_without_writing`、`add_workspace_rejects_reserved_id_without_creating_file`、`workspace_id_validation`。
5. **runner がタスク 1 件の失敗で終了した**。症状: `agents.<name>.command` が空、または `/dev/tty` を開けないと runner ごと終了し、タスクは `interrupted` になる。原因: `run_task` 内のエラーを `?` で呼び出し元に返していた。修正: argv が空、`/dev/tty` を開けない、spawn の失敗、終了時プロンプトの失敗のいずれでも、エラーを表示して `agent_exited`（`failed`）を送り、待機に戻るようにした。termios は spawn の前に保存する。あわせて、`command` が空配列または先頭が空文字列の agent を設定の検証で弾くようにした。テスト: `empty_argv_reports_failed_and_keeps_runner_alive`、`unstartable_agent_reports_failed_and_keeps_runner_alive`、`agent_with_empty_command_is_error`。
6. **`loom done` だけソケットの優先順位が逆だった**。症状: `loom --socket S done` としても `ZELLOOM_SOCKET` が優先された。修正: 他のコマンドと同じ `paths::socket_path()`（`--socket` > `ZELLOOM_SOCKET` > 既定）を使うようにした。agent の中では runner が `ZELLOOM_SOCKET` を設定するので、`--socket` を付けなければ従来どおり届く。
7. **TUI の細かい不具合**。
   - `fix: ...` のように「空白を含まない語 + `:`」で始まる本文が workspace 指定と解釈されていた。`ws` が登録済みの workspace ID のときだけ workspace 指定として扱うようにした。TUI のタブは Zellij サーバの環境で起動され `ZELLOOM_CONFIG` を引き継がないため、TUI で設定ファイルを読むと core と異なる設定を見うる。そこで core の `status` 応答に `workspaces`（core が読んだ設定の workspace ID 一覧）を加え、TUI は `list` のたびに `status` も取り直してそれを使うようにした。テスト: `unregistered_prefix_is_part_of_the_task_text`、`add_input_with_workspace_prefix`。
   - `failed` のタスクが TUI に出ず、retry もできなかった。4 つ目の枠を INTERRUPTED / FAILED にして `failed` も（行頭に `✗` を付けて）出し、`R` と `dd` を `failed` にも効くようにした。core は `complete` / `cancel` を `failed` に許していないので、`D` / `c` は `failed` には効かない。見出しの件数は `done` / `cancelled` の全期間の総数で、"recent" ではなく "total" と表示する。テスト: `failed_task_can_be_retried_and_deleted_but_not_cancelled_or_marked_done`、`selectable_tasks_ordered_by_section`、`renders_all_sections_with_sample_tasks`。
   - `queued` / `received` のタスクを取り消す手段が無かった（削除しかできない）。取り消しは正当な操作なので、core の `cancel` を `queued` / `received` からも受け付けるようにし、TUI の `c` もそれらに効くようにした。TUI の各キーが受け付ける状態は、core が許す状態の部分集合にそろえた。テスト: `queued_and_received_tasks_can_be_cancelled`（結合テストと TUI の単体テスト）。
8. **未使用の `Zellij::rename_tab_by_id` を削除した**。
9. **`loom --help` がほぼ空だった**。clap のサブコマンドと引数に説明文（英語）を付けた。

README のクイックスタートは、`loom workspace add --agent` が未定義の agent を拒否するようになったので、agent の設定を workspace の登録より先に書く順に入れ替えた。

検証: 単体テスト 72 件と結合テスト 8 件（`tests/scheduler_flow.rs`）が通ること、`cargo clippy --all-targets` の警告が無いこと、`cargo fmt` で差分が出ないことを確認した。
