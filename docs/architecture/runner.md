# Runner

`loom runner <workspace>`（`src/runner.rs`）。workspace タブの唯一のペインで動き、core から受け取ったタスクごとに agent を起動する。シェルのジョブ制御と同じやり方で端末を agent に渡し、終わったら取り戻す。

## 起動と待機

1. SIGTTOU / SIGTTIN / SIGINT / SIGQUIT / SIGTSTP を無視にする。
2. SIGHUP / SIGTERM のハンドラを入れる（self-pipe で内部メッセージに変換する）。
3. ソケット（`--socket` > `ZELLOOM_SOCKET` > 既定。[config.md](config.md#パスと環境変数)）に接続し、最初のメッセージとして `runner_attach` を送る。`ok` 以外が返る（同じ workspace の runner が既に接続中など）か、応答前に切断されたら、エラーを表示して終了する。応答前に SIGHUP / SIGTERM を受けた場合は何も表示せずに終了する。
4. `/dev/tty` を開く。開けたら入力用に保持する。
5. `zelloom runner: <workspace> — waiting for tasks` を表示して待つ。端末を保持している場合は続けて `type a task and press Enter to queue it for <workspace>`、`press Enter on an empty line twice to write a multi-line task in an editor` と入力プロンプト `> ` を表示する。

ソケットの読み取り、agent の `wait`、シグナル受信はそれぞれ別スレッドで行い、1 本の `mpsc` チャネルに流す。待機中に届いた `stop` や応答行は捨てる。ただしエラー応答（`agent_exited` が拒否された場合など）は内容を表示してから捨てる。

## 待機中のタスク入力

待機中、runner はチャネルを確認しつつ、保持した端末を 100ms ごとに `poll` して入力を待つ。端末は canonical モードのまま使うので、行編集（Backspace、Ctrl-U、Ctrl-C で行を破棄など）は端末ドライバに任せ、Enter で確定した 1 行を 1 タスクとして扱う。

- 前後の空白を除いた行が空でなければ、別の接続で `enqueue`（`workspace` はこの runner の workspace、`agent` なし、`source` は `{"type": "runner"}`）を送り、`added <id> (<status>)` を表示する。失敗したらエラーを表示する。どちらの場合も次のプロンプトを出す。
- 前後の空白を除いた行が空なら、1 回目は `press Enter again to write the task in <editor>` を表示して「エディタ待ち」にする。エディタ待ちでもう一度空の行が来たらエディタを開く。空でない行を受けたとき、エディタを開いたとき、`start` を受けたときにエディタ待ちは解除される。
- 複数行を貼り付けると、行ごとに別のタスクになる。1 行はカーネルの canonical モードの上限（4095 バイト）を超えられない。複数行の本文はエディタで書く。

### エディタでの入力

`editor::compose` が一時ディレクトリに空の `loom-task-<pid>-<ulid>.md` を作り、エディタ（[config.md](config.md#loom-config-edit) と同じ解決・`sh -c` 経由の起動）で開く。

- エディタは agent と同じく別のプロセスグループで起動して端末のフォアグラウンドを渡し（シグナルの既定動作への復帰も同じ）、終了後に termios・フォアグラウンド・端末モードを agent 終了時と同じ手順で戻す。
- 正常終了したら内容の前後の空白を除き、空でなければその本文で `enqueue` する（1 行入力と同じ送り方）。空なら `the editor returned empty text; nothing queued` を表示するだけ。エディタが起動できない・非 0 で終了した場合はエラーを表示する。一時ファイルはどの場合も消す。
- エディタを開いている間はチャネルを見ない。その間に届いた `start` などはエディタを閉じてから処理する。
- `sources.runner.auto_queue`（[config.md](config.md)）は他の source と同じく効く。
- `poll` が失敗した、端末が読めなくなった（`POLLHUP` など）、`read` が失敗した場合はエラーを表示し、以後は入力を受け付けずチャネルだけを待つ。
- `start` を受けたら、agent を起動する前に端末の未読入力を `tcflush(TCIFLUSH)` で捨てる。確定前の入力が agent に渡らないようにするため。

## タスクの実行

`start` を受けると:

1. タスク ID と本文を表示する。
2. runner 自身のプロセスグループ ID を覚えておく。`agent.argv` が空でないことを確かめてから `/dev/tty` を開き、現在の termios を保存する。
3. 実行する argv を決める。`agent.shell` が true なら `agent.argv` を runner の `$SHELL` の対話モードで包む（`shell_argv`。ラッパの形は [config.md](config.md#シェル経由の起動)）。false なら `agent.argv` をそのまま使う。
4. その argv を `agent.cwd` で起動する。環境変数は runner の環境に `agent.env` を上書きしたもの（[config.md](config.md#agent-の環境変数)）。標準入出力は runner のもの（ペインの端末）をそのまま継承する。子の `pre_exec` では次の順に処理する（非同期シグナル安全な libc 呼び出しだけを使う）。
   1. `setpgid(0, 0)` で新しいプロセスグループのリーダーになる。
   2. 手順 2 で開いた端末に `tcsetpgrp(tty, getpid())` し、自分のプロセスグループをフォアグラウンドにする。SIGTTOU がまだ無視のままなので、バックグラウンドからでも成功する。
   3. 上記 5 つのシグナルを既定の動作に戻し、シグナルマスクを空にする。
5. 親からも `setpgid` と `tcsetpgrp` で同じことを行う（冪等）。以後 agent は普通の対話プログラムとして端末を使う。
6. agent の終了、`stop`、切断、SIGHUP / SIGTERM を待つ（下記）。
7. 後片付けとして端末を戻す:
   - 保存した termios を `tcsetattr` で復元する。
   - `tcsetpgrp` でフォアグラウンドを runner のプロセスグループに戻す。
   - 次のエスケープシーケンスを書く: 代替画面の解除（`?1049l`）、カーソル表示（`?25h`）、マウス報告の解除（`?1000l` `?1002l` `?1003l` `?1006l`）、bracketed paste の解除（`?2004l`）、フォーカスイベントの解除（`?1004l`）、kitty キーボードプロトコルの pop（`CSI < u`）、SGR リセット（`CSI 0 m`）。
8. 下の表に従って、待機に戻るか runner を終了する。

`agent.argv` が空、`/dev/tty` を開けない、spawn に失敗した、のいずれの場合もエラーを表示し、`agent_exited`（`failed`）を送って待機に戻る。runner は終了しない。シェル経由で起動した場合、コマンドが見つからないなどの失敗はシェルの終了として現れるので、下記の自発終了時のプロンプトになる。

シェル経由でも、シェルは `exec` で agent に置き換わるので、`child` の PID は agent の PID であり、プロセスグループも変わらない。停止シーケンスの `killpg` や終了の待機はシェルの有無に関係なく同じように働く。

## 停止シーケンス

- 自分のタスクの `stop` を受けたら、agent のプロセスグループに SIGTERM を送り、3 秒の猶予を置く。
- 猶予内に終了しなければプロセスグループに SIGKILL を送り、終了を待つ。
- SIGHUP / SIGTERM を runner が受けたときも同じ手順で agent を止める（すでに猶予中なら何もしない）。
- 他のタスク宛ての `start` など、待機中に関係ないメッセージが届いた場合は捨てずに保留し、待機が終わったあとに処理する。

## 終了後の分岐

| 待機中に起きたこと | 動作 |
|---|---|
| SIGHUP / SIGTERM を受けた | agent を止めて runner を終了する |
| `stop` を受けた（core 接続は生きている） | 待機に戻る。`agent_exited` は送らない（core が状態を確定済み） |
| `stop` を受け、かつ core との接続が切れた | runner を終了する |
| agent が自分で終了し、core との接続が切れていた | agent の終了を待ってから runner を終了する（接続が切れても agent は止めない） |
| agent が自分で終了した（上記以外）で `agent.oneshot` が true | 終了コード 0 なら `agent_exited`（`done`）、それ以外（シグナルによる終了や `wait` の失敗を含む）なら `failed` を送って待機に戻る。送れなかったら runner を終了する |
| agent が自分で終了した（上記以外）で `agent.oneshot` が false | プロンプトを出す（下記） |

## 自発終了時のプロンプト

agent が `loom done` なしで終了すると、runner は端末に次を表示し、端末を raw モードにして 1 文字を待つ。

```text
agent exited on its own. [d]one / [r]estart / [f]ailed?
```

- `d` / `D` → `agent_exited`（`done`）、`f` / `F` → `failed`、`r` / `R` → `restart`。それ以外のキーは無視。
- 送ったあとは待機に戻る。`restart` の場合は core から同じタスクの `start` が再送され、新しい agent プロセスが起動する。
- 待っている間に同じタスクの `stop` が来たら（TUI から完了させた場合など）何も送らずに待機に戻る。
- 待っている間に core との接続が切れた、または SIGHUP / SIGTERM を受けたら runner を終了する。
- 入力は 100ms ごとの `poll` で待ち、終わったら termios を元に戻す。
- プロンプト自体が失敗した（端末を raw モードにできないなど）場合はエラーを表示し、`agent_exited`（`failed`）を送って待機に戻る。
- `agent_exited` を送れなかった（core との接続が切れていた）場合は runner を終了する。

## core との切断

- 待機中に切断されたら `connection to loom core lost` を表示して終了する。
- agent 実行中に切断されても agent は止めない。agent が終わった時点で runner も終了する。
- core 側では、切断した runner が実行中だったタスクは `interrupted` になる（[scheduler.md](scheduler.md#runner-の管理)）。
- core が起動した runner（`--close-pane-on-exit` 付き）は、正常終了するとき自分のペインを閉じる（[zellij.md](zellij.md#runner-の-argv)）。エラーで終了した場合や、フラグ無しで起動した runner が終了した場合はペインが exited 状態で残る。次にその workspace でタスクを開始するとき、launcher がタブを閉じて作り直す（[zellij.md](zellij.md#workspace-タブの再利用と作り直し)）。
