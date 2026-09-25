# 2026-09-25 agent をユーザーの対話シェル経由で起動する・環境変数の設定

Zellij でタブを手で開いてエージェントを起動したときと、zelloom が起動したときとで、エージェントから見える環境が違っていた。runner は Zellij サーバの環境を継承しているだけで、`.bashrc` などの rc ファイルを通らないため、rc ファイルで足した `PATH` や `export` した変数が agent に届かなかった。そこで agent を既定でユーザーの対話シェル経由で起動するようにし、あわせて設定から agent に環境変数を渡せるようにした。現状の仕様は `docs/architecture/config.md`（設定キー・マージ順・ラッパ）と `docs/architecture/runner.md`（起動手順）を参照。

## 決めたこと

- **既定でシェル経由にする（`agents.<name>.shell`、既定 true）**: 目的は「手でタブを開いたときと同じ状態で始まる」ことなので、既定で有効にした。rc ファイルを通したくない agent 用に `shell = false` を用意した。
- **ラッパは core ではなく runner が組み立てる**: 使うシェルは runner の `$SHELL` でなければならない。runner の環境は Zellij サーバから継承したもので、手で開いたペインと同じだからである。core は別の環境（デタッチ起動したときのもの）で動いているので、core で `$SHELL` を読むと食い違いうる。そこで `ResolvedAgent.argv` は従来どおり agent 自身の argv のままにし、`ResolvedAgent.shell`（bool）を足して、runner が包む。
- **ラッパの形**: POSIX 系（bash / zsh / sh / dash / ksh と不明なもの）は `$SHELL -i -c 'exec "$@"' loom-agent argv...`、fish は `$SHELL -i -c 'exec $argv' argv...`。`-c` のスクリプト文字列は固定で、instruction やタスク本文は位置引数として渡すので、改行・引用符・`$`・バッククォート・非 ASCII を含んでもシェルに解釈されない。`sh -c 'script' name args` では `$0` が `name`、`$@` が `args` になるので、`$0` 用のダミーとして `loom-agent` を挟む。fish は `-c` の後の位置引数をすべて `$argv` に入れる（公式ドキュメントの `fish -c` の説明による）。`$SHELL` が空・未設定なら `/bin/sh` の POSIX ラッパにする。
- **`exec` で置き換える**: シェルが agent に置き換わるので、PID もプロセスグループも変わらず、runner のジョブ制御・`killpg` による停止・`wait` はそのまま使える。副作用として、rc ファイルのエイリアスや関数は agent のコマンドには効かない（`exec` は `PATH` から探す）。コマンドが見つからない場合はシェルが 127 で終わるので、spawn の失敗ではなく自発終了のプロンプトになる。
- **環境変数（`agents.<name>.env` / `workspaces.<id>.env`）**: マージ順は「runner が継承した環境 < agent の env < workspace の env < `ZELLOOM_*`」。`ZELLOOM_TASK_ID` / `ZELLOOM_WORKSPACE` / `ZELLOOM_SOCKET` は常に zelloom の値で、設定で `ZELLOOM_` で始まるキーを書くと設定エラーにした（黙って無視すると設定が効かない理由が分からなくなるため）。キーは空文字列・`=`・NUL を拒否し、値は NUL を拒否する。値は展開せず文字どおりに渡す。
- **rc ファイルとの順序には手を入れない**: `shell = true` では環境変数を設定してからシェルを起動するので、rc ファイルが同じ変数を設定すれば rc の値が勝つ。手で開いたタブで `export` してからシェルを起動したときと同じ挙動であり、それを回避する仕組み（rc の後で再設定するなど）は入れなかった。ドキュメントに明記した。
- **agent の標準入出力は runner のものを継承する**: 最初は `/dev/tty` を複製して明示的に標準入出力に設定したが、そうすると agent の fd 0/1/2 が `/dev/tty` を指し、`tty` コマンドが `/dev/tty` を返す（rc ファイルでよく使う `GPG_TTY=$(tty)` などが壊れる）。runner の標準入出力は Zellij ペインの pty そのものなので、継承する形に戻し、fd 0/1/2 が `/dev/pts/*` を指すことをテストで確かめるようにした。

## SIGTTIN による停止の競合と対策

対話シェル（bash / zsh / dash など）はジョブ制御を初期化するとき、自分のプロセスグループが端末のフォアグラウンドかを確かめ、そうでなければ自分に SIGTTIN を送ってフォアグラウンドになるまで待つ。runner は `pre_exec` で SIGTTIN を既定の動作（停止）に戻しているので、この確認が runner の親側の `tcsetpgrp` より先に起きると、シェルは停止し、そのまま誰にも SIGCONT されない。従来は親側でしか `tcsetpgrp` していなかった。

対策として、シェルのジョブ制御と同じく子の側でもフォアグラウンドを取るようにした。`pre_exec` で `setpgid(0, 0)` → runner が開いた `/dev/tty` の fd に `tcsetpgrp(tty, getpid())`（SIGTTOU がまだ無視なのでバックグラウンドからでも成功する）→ シグナルを既定に戻してマスクを空にする、の順。`/dev/tty` の fd は `O_CLOEXEC` だが `pre_exec` の時点ではまだ開いている。`pre_exec` 内では libc の非同期シグナル安全な関数だけを呼ぶ。親側の `setpgid` / `tcsetpgrp` も残した（冪等）。

通常は `spawn` が exec 完了まで待ってすぐ親が `tcsetpgrp` するので、手元の実行ではこの競合はたまにしか起きない。そこで検証のため、一時的に親側の `tcsetpgrp` の前に 300ms の待ちを入れて試した。子側の `tcsetpgrp` を外した状態では bash と dash が停止状態（`T`）のまま agent が起動しなかった。子側の `tcsetpgrp` がある状態では同じ待ちを入れても全シェルで起動した（この待ちは検証後に取り除いた）。

## 作ったもの

- `src/config.rs`: `AgentConfig.shell`（既定 true）・`AgentConfig.env`・`WorkspaceConfig.env`、`validate_env`、`ConfigError::InvalidEnvKey` / `InvalidEnvValue`。
- `src/core/scheduler.rs`: `build_env`（agent < workspace < `ZELLOOM_*` のマージ）、`ResolvedAgent.shell` の設定。
- `src/protocol.rs`: `ResolvedAgent.shell` を追加。
- `src/runner.rs`: `shell_argv`（シェルのファイル名によるラッパ）、`command_argv`、`pre_exec` での `setpgid` / `tcsetpgrp`。
- `tests/runner_pty.rs`: 新規の結合テスト（下記）。

## 検証

- ユニットテスト:
  - `config::tests::agent_shell_defaults_to_true_and_env_to_empty` / `env_tables_are_parsed_as_literal_strings` / `env_rejects_reserved_and_invalid_keys`（`ZELLOOM_*`・空・`=`・NUL）/ `env_rejects_nul_in_value`
  - `core::scheduler::tests::env_merge_order_is_agent_then_workspace_then_zelloom`（設定に `ZELLOOM_*` が紛れ込んでも zelloom の値が勝つことも含む）
  - `runner::tests::shell_argv_uses_posix_wrapper_for_posix_and_unknown_shells` / `shell_argv_uses_argv_for_fish` / `shell_argv_falls_back_to_bin_sh` / `command_argv_respects_shell_flag`
  - `runner::tests::shell_wrapper_passes_args_through_unchanged`: 手元にあるシェルで実際にラッパを実行し、空白・改行・引用符・バックスラッシュ・`$HOME`・`$(...)`・非 ASCII・空文字列・`-i`・`*`・`exec "$@"` を含む引数が `printf '%s\0'` にそのまま届くことを確かめる（HOME は一時ディレクトリ）。
- 結合テスト `tests/runner_pty.rs`: `openpty` で作った pty を制御端末（`setsid` + `TIOCSCTTY`）にして実際の `loom runner` を起動し、テストプロセス内で起動した core と一時ディレクトリの設定・HOME で次を確かめる。偽の agent（シェルスクリプト）は rc ファイルが `PATH` に足すディレクトリにだけ置く。
  - agent が起動し、停止状態（`T`）でないこと、自分のプロセスグループのリーダーで、端末のフォアグラウンド（`tpgid`）であること、fd 0/1/2 が pty であること
  - rc ファイルで `export` した変数が見えること、設定の env（agent / workspace の優先順、`$HOME` が展開されないこと）と `ZELLOOM_*` が見えること、rc ファイルが設定の変数を上書きすること
  - argv が完全に一致すること（改行・引用符・`$`・非 ASCII を含むタスク本文）、cwd が workspace のパスであること
  - agent の環境の `ZELLOOM_TASK_ID` / `ZELLOOM_SOCKET` だけで `loom done` を実行すると agent が終了し、次のタスクで新しいプロセスが起動すること
  - テスト: `bash_agent_runs_through_interactive_shell` / `sh_agent_runs_through_interactive_shell`（dash、rc は `$ENV`）/ `zsh_agent_runs_through_interactive_shell` / `fish_agent_runs_through_interactive_shell` / `shell_false_runs_agent_directly`（rc が読まれず、agent のコマンドは絶対パス）
- `cargo build` / `cargo test` / `cargo clippy --all-targets` / `cargo fmt` がすべてクリーン。

## 未検証

- zsh と fish は検証環境に入っていなかったため、実際には動かしていない。対応する結合テストは該当シェルが無ければ何もせずに通る。zsh は `-c 'script' name args` で `$0` / `$@` が POSIX と同じになること、fish は `-c` の後の位置引数が `$argv` に入ること（公式ドキュメント）と `exec $argv` で変数をコマンドとして使えること（fish 3.0 以降）に基づいて実装した。
- ksh、および POSIX 以外の構文のシェル（csh 系、nushell など）は試していない。後者は `shell = false` で使う前提としてドキュメントに書いた。
- 実際の Zellij ペインの中での動作（手で開いたタブとの環境の一致）は、この作業では Zellij を起動せずに pty で代替した。
