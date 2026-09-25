# 監査で見つかった不具合の修正

コードとドキュメントの監査で見つかった 7 件の不具合を直し、architecture からの「理由」の記述を本ログへ移した。

## 監査の範囲と結果

`src/` 全体・`tests/`・`docs/architecture*`・README を読み合わせ、状態遷移の競合、runner タブの判定、エラー表示、入力処理を中心に確認した。見つかったもの:

1. launcher のタブ名衝突（ユーザーのタブを runner タブと誤認する）
2. `accept` / `reject` / `retry` / `delete` / `edit` の検査と更新の競合
3. `restart` の再解決失敗でタスクが `running` のまま残る
4. core が動いていないときの CLI のエラーが `io error: No such file or directory` だけ
5. `loom add --agent` に未定義の agent を渡しても受け付ける
6. TUI の入力モードで `Ctrl` / `Alt` 付きの文字が入力される
7. `runner_attach` が「最初のメッセージ」でなくても受け付けられる

## 1. タブ名衝突

- 症状: 名前が workspace ID と同じタブがあれば、中身に関係なく runner タブとみなしていた。README のレイアウト例どおり `loom` という名前のタブで TUI を動かし、workspace ID も `loom` だと `AlreadyRunning` になって runner が起動せず、attach タイムアウトでタスクが `interrupted` になる。
- 原因: `decide()` がタブ名と「最初の非プラグインペインが exited か」だけを見ていた。
- 修正: runner ペインをコマンドで判定する。Zellij 0.45 のソースでは、`list-panes -j` の `terminal_command` はペインを起動した `RunCommand`（コマンドと引数を空白で連結）、`pane_command` はペインの現在のフォアグラウンドプロセスの argv（空白で連結）。runner が agent を実行中だと `pane_command` は agent のものになり、終了済みペインでは取れないことがあるので、`terminal_command` と `pane_command` のどちらかが `<…/loom> [--socket <path>] runner <ws>` なら runner ペインとした。実行ファイル名は `current_exe()` のファイル名で比べ、実行中にバイナリを置き換えたときに付く ` (deleted)` を除く。パスに空白があっても判定できるよう、末尾の ` runner <ws>` と ` --socket ` の位置で切り出す。同名のタブは複数ありうるので、生きた runner ペインがどれかにあれば `AlreadyRunning`、runner ペインが終了済みのものだけならそれらをすべて閉じて作り直し、runner ペインを含まない同名タブは無視して新しく作る。
- 影響: クライアントが 1 つもアタッチしていないセッションでは `list-panes` が空になるので、以前の `AlreadyRunning` ではなく `CreateTab` になる（todo に記載）。
- テスト: `launcher::tests` の `no_existing_tab_creates_one` / `live_runner_pane_is_not_duplicated` / `exited_runner_pane_closes_then_creates` / `same_named_tab_without_runner_pane_is_ignored` / `same_named_tab_with_no_panes_listed_is_ignored` / `user_tab_running_the_tui_does_not_count_as_runner` / `runner_for_another_workspace_does_not_count` / `other_program_named_runner_does_not_count` / `live_runner_among_duplicate_named_tabs_wins` / `only_tabs_with_exited_runner_are_closed` / `runner_busy_with_agent_is_found_by_terminal_command` / `runner_started_from_a_shell_is_found_by_pane_command` / `runner_command_matching_accepts_socket_args_and_spaces` / `exe_file_name_strips_deleted_suffix`。`zellij::tests::parses_sample_panes_json` に `terminal_command` の確認を足した。

## 2. 状態検査と更新の競合

- 症状: `running` になったばかりのタスクを `delete` で消す、`cancel` されたタスクを `accept` で `queued` に戻す、などがありえた。
- 原因: `get` で状態を見てから無条件に更新していた。
- 修正: 許可される状態を条件にした 1 つの SQL 文にした。`accept` / `reject` は `update_status_if(received → …)`、`delete` は `delete_unless`（`status != 'running'`）、`edit` は `update_text_unless`、`retry` は `retry_if`（`status IN (interrupted, failed, cancelled)`、`position` は同じ UPDATE 内のサブクエリで最大 + 1）。0 行なら改めて状態を読み、従来と同じ種類のエラー（not found / 状態が違う）を返す。使われなくなった無条件の `update_status` / `update_text` / `delete` / `retry` は削除した。
- テスト: `store::tests` の `delete_unless_skips_task_that_became_running` / `update_text_unless_skips_task_that_became_running` / `accept_via_update_status_if_skips_task_cancelled_in_between` / `retry_if_skips_task_whose_status_changed_in_between` / `retry_moves_task_to_end_of_queue`。

## 3. `restart` の失敗でタスクが残る

- 症状: `agent_exited`（`restart`）で設定から workspace や agent が消えていると、エラーを返すだけでタスクは `running`、runner は待機に戻り、workspace が塞がったままになる。
- 修正: 再解決に失敗したら `running → failed` の条件付き更新、runner の現在のタスクを空にし、`schedule()` を呼び、`cannot restart task '<id>': <理由>; the task was marked failed` を返す。
- テスト: `scheduler_flow::restart_that_cannot_resolve_the_agent_fails_the_task_and_frees_the_workspace`（設定から workspace を消して restart → `failed`、runner の `current_task` が null、設定を戻して次のタスクが同じ runner で始まる）。

## 4. core が動いていないときのエラー

- 症状: `loom add` / `list` / `done` が `io error: No such file or directory (os error 2)` を出すだけだった。
- 修正: `Client::connect` で `NotFound` / `ConnectionRefused` を `ClientError::CoreNotRunning` にし、`loom core is not running (<socket>); start it with `loom` inside a Zellij session` を表示する。`loom stop` は接続失敗を従来どおり `core is not running`（終了コード 0）として扱う。TUI は従来どおり。
- テスト: `client::tests::missing_socket_reports_core_not_running` / `refused_socket_reports_core_not_running`（bind だけして listen しないソケットで ECONNREFUSED を確実に起こす。listener を閉じただけのソケットファイルでは環境によって接続が一度成功してから reset され、テストが不安定だった）。

## 5. 未定義 agent の受け付け

- 修正: `enqueue` がその時点の設定を読み、`agent` が `[agents]` に無ければ `agent '<name>' is not defined in [agents]` を返す。未登録 workspace の拒否は既存どおり。CLI は応答のエラーをそのまま表示する。
- テスト: `scheduler_flow::enqueue_rejects_unknown_agent_and_unknown_workspace`。

## 6. TUI 入力モードの修飾キー

- 修正: 入力モードで `Ctrl` / `Alt` を伴う文字キーは無視する（`Ctrl+C` は従来どおり先に処理）。`Shift` は入力する。
- テスト: `tui::app::tests::ctrl_and_alt_chars_are_not_inserted_in_input_mode`。

## 7. `runner_attach` の位置

- 症状: ドキュメント上は「最初のメッセージ」なのに、通常の接続では何番目でも受け付けていた。runner 接続での 2 回目のエラーメッセージも実際の状況と合っていなかった。
- 修正: `handle_connection` で最初の行かどうかを覚え、2 行目以降の `runner_attach` には `runner_attach must be the first message on a connection` を返して通常の接続のまま続ける。runner 接続の上での `runner_attach` には `connection is already attached as runner for <ws>` を返す。
- テスト: `scheduler_flow::runner_attach_is_only_accepted_as_the_first_message`。

## 手動確認

Zellij 外で、一時的な設定・状態ディレクトリと専用のソケットパスを使い、core の無い状態で `loom list` / `loom done` がメッセージを出して終了コード 1、`loom stop` が `core is not running` で 0 になることを確認した。専用ソケットで起動した core に対して `loom add --agent nope` が `agent 'nope' is not defined in [agents]` で失敗し、定義済みの agent なら追加できることを確認した。

## architecture から移した理由

architecture には結果だけを書く規則に合わせて、次の「なぜ」を削り、ここに残す。

- `--no-focus`: ユーザーが今いるタブから作業を続けられるように、workspace タブはフォーカスを奪わずに作る。
- ソケットパスを argv で渡す: `zellij action new-tab -- <argv>` で起動したペインは core ではなく Zellij サーバの環境を継承するので、core の `ZELLOOM_SOCKET` は runner に届かない。そのため `--socket` を argv に埋め込む。
- 素の `loom` が Zellij 外で core を起動しない: core は workspace タブを作るのにセッション名が要る。
- runner の子が先に端末のフォアグラウンドを取る: 対話シェル（bash / zsh / dash など）は起動時に自分がフォアグラウンドかを確かめ、そうでなければ自分に SIGTTIN を送る。子では SIGTTIN を既定の動作に戻しているので、親の `tcsetpgrp` より先にシェルがこの確認をすると、シェルは停止したまま再開されない。`exec` 前に子の側でフォアグラウンドにすればこの競合は起きない。
- runner が待機中の無関係なメッセージを保留する: core は `stop` の直後に次のタスクの `start` を送ることがある。
- `schedule()` の中から `schedule()` を呼ばない: `tokio::sync::Mutex` は再入できないので、呼ぶと自分自身を待って止まる。
- instruction に `loom` の絶対パスを埋め込む: agent の `PATH` に依存させないため。
- core が操作のたびに設定を読み直す: `loom workspace add` の結果を動いている core に即座に反映させるため。
- `workspace add --agent` で設定ファイルが無いとエラーにする: ファイルが無ければ agent が定義されているはずがない。
- シェルのラッパを runner が組み立てる: シェルは runner の環境（`$SHELL`）で決まる。
