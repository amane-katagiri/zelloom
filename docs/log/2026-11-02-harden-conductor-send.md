# 2026-11-02 司令塔からの send を安全にする

2026-10-31 に入れた `report` / `watch` / `send` をセキュリティの観点で見直し、`send` がワーカーの意図しない入力にならないよう固めた。現状の仕様は `docs/architecture/protocol.md`・`scheduler.md`・`runner.md`・`config.md`・`zellij.md` を参照。

## 見直しで確かめた問題

- `send` の本文がそのまま Claude Code に打ち込まれる。ESC・Ctrl-C/D・Tab・Shift+Tab（`ESC [ Z`、権限モードの切り替え）・bracketed paste や OSC のシーケンスが通り、先頭の `!` は承認なしのシェルコマンド、先頭の `/` は `/permissions` などのスラッシュコマンドになる。
- 「入力欄で待っている」の判断がワーカー自身の報告だけに頼っている。Notification の `permission_prompt` はダイアログが出てから約 6 秒後にしか来ないので、その間の `send` はダイアログの Enter になる。`question` を報告したあと作業を続けることもできる。
- `send` はタスクしか指定しないので、古い報告への返事が新しい報告に当たる。
- `send` は runner に送った時点で成功を返し、attention も消える。runner が打てなかった（agent が終了して d/r/f のプロンプト中、zellij の失敗、E2BIG、停止中）ときは返事が失われる。
- agent が打ち込み直後に終了すると、読まれなかった入力が runner の d/r/f プロンプト（`tcflush` が無い）や待機中のタスク入力（返事がタスクとして積まれうる）に読まれる。
- runner の待機ループの中で zellij を時間制限なしに同期で呼んでいるので、zellij が止まると stop・SIGTERM・SIGKILL への移行も止まる。

## 決めたこと

- **ターンの状態を agent の hook から受け取る**: 画面を見ずに「入力欄で待っている」を知る手段として、Claude Code の hook（`Stop` → idle、`UserPromptSubmit` と `PreToolUse` → busy）に `loom turn` を呼ばせる。`report` はターンの中から呼ばれるので、どの種類でも busy にする。権限ダイアログは遅れて来る Notification ではなく、ダイアログを出すときに来る `PermissionRequest` で `report blocked` にする。
- **報告に番号を振り、send に `--expect` を必須にする**: core 全体の通し番号を 1 本持ち、報告の `attention.id`、ターンを終えた印、`input_id` をすべてここから取る。`send` は「その報告がまだ最新で、その報告より後にターンを終えた」ときだけ通す。番号を 1 本にしたので、「印 > 報告の id」という比較だけで前後が決まる。
- **本文は書き換えずに拒否する**: 制御文字を取り除いたり先頭の `!` をエスケープしたりすると、司令塔が送ったつもりの文と違うものが届く。LF 以外の C0・DEL・C1 と、先頭の空白でない文字が `!` / `/` のものは理由つきのエラーにして、司令塔に書き直させる。長さは 16 KiB まで（`report` の本文も同じ上限と NUL の拒否だけ。JSON で運ぶだけなので他の制御文字は残す）。
- **配送の結果を待ってから答える**: runner が `input_result` を返すまで core は `send` の応答を保留する（既定 15 秒）。何も打っていない（`not_typed`）ときだけ報告と idle を戻して再送できるようにし、途中まで打ったかもしれない（`partial`）・時間切れ・切断は「未確認」として戻さない。戻すと、半分打たれた入力欄にもう一度打ち込むことになるため。待つ間は scheduler・runners のロックを持たない。
- **打ち込みは別スレッドで時間制限付き**: 2 回の zellij 呼び出しをそれぞれ 5 秒で打ち切り（自分のプロセスグループで起動し、超えたらグループごと kill）、待機ループは stop・SIGTERM・agent の終了を処理し続ける。runner の合計（最大 10 秒）が core の待ち時間（15 秒）より短いので、通常は core の時間切れより先に `partial` が届く。打てない状況の `input`（待機中・プロンプト中・別タスク宛て・停止中・打ち込み中）はすぐ `not_typed` を返す。
- **agent の終了を core に先に知らせる**: agent が自分で終了したら、打ち込み中のスレッドを待ってから `agent_gone` を送り、attention を消して busy にする。そのあと端末の未読入力を捨ててからプロンプトを出す。zellij は `zellij action` が戻ったあとにペインへ書くので、直前に打ち込んでいた場合は 300ms 待ってから捨てる。待機中の入力に戻る前にも捨てる。
- **hook から loom を呼ぶための `ZELLOOM_LOOM_EXE`**: hook は agent の環境で動くので、`PATH` に loom が無くても、別の loom が先にあっても core と同じ実行ファイルを呼べるようにした。値は instruction の `{loom_exe}` と同じ。
- **hook の例は失敗を握りつぶさない**: `[ -z "$ZELLOOM_TASK_ID" ] || ...` で zelloom の外では何もしないようにし、`|| true` は付けない。turn を知らせられなかったことはワーカーの画面に hook のエラーとして出る。`report blocked` の出力は `PermissionRequest` の判定として読まれないよう `/dev/null` に捨てる（終了コードはそのまま）。
- **人の目に触れる操作は Claude Code の権限で必ず止める**: ワーカーの設定例の `permissions.ask` に `git push`・`gh pr create/ready/merge`・`gh issue comment`・`gh pr comment` を入れ、ワーカーが gate を飛ばしても人間の確認ダイアログが出るようにした。ダイアログは `PermissionRequest` の hook で `blocked` になり、司令塔は打ち込まない。
- **タスクごとのトークンは入れない**: `report` / `turn` は同じ uid のどのプロセスからも送れるが、同じ uid ならワーカーの環境変数もソケットも読めるので、トークンを足しても防げない。既知の制限として `docs/todo.md` に書いた。
- **ワーカーには AskUserQuestion とプラン承認を使わせない**: これらのダイアログには誰も答えず、`send` の打ち込みが選択肢を押してしまう。instruction で `loom report question` を使うよう伝える。

## 作ったもの

- `Task.turn`、`Attention.id`、`turn` / `input_result` / `agent_gone` リクエスト、`send` の `expect`、`input` イベントの `input_id`（`src/protocol.rs`）。
- turn の記録と通し番号、`begin_send` / `finish_send` / `abort_send` / `reset_turn`（`src/store.rs`）。
- 本文の検査、`input_result` 待ちと結果ごとの応答、`agent_gone`、`ZELLOOM_LOOM_EXE`、`ZELLOOM_INPUT_TIMEOUT_MS`（`src/core/`）。
- 時間制限付きの `write_chars` / `write_bytes`（`src/zellij.rs`）、打ち込みスレッド・即時の `not_typed`・`agent_gone`・未読入力の破棄（`src/runner.rs`）。
- `loom turn <busy|idle> [--task ID]`、`loom send <id> --expect <attention.id>`、`loom show` の `turn` と `attention_id`（`src/cli.rs`）。
- `skills/zelloom-conductor/` の hook と権限の例、`send` の説明、ワーカーの instruction。

## 検証

- 単体テスト: 本文の規則（正規化、空、各種の制御文字、`!` / `/`、16 KiB の境界）と報告の本文の規則、新しいリクエストの形と CLI の引数。
- 結合テスト（`tests/scheduler_flow.rs`）: turn の遷移と watch への出方、報告 id の増加、ターン中・報告より前に idle になった場合・`expect` の不一致・各本文規則・配送中の二重送信の拒否、偽 runner の `typed` での成功、`not_typed` で報告と idle が戻ること（新しい報告があれば戻らないこと）、`partial`・時間切れ・切断が未確認になり戻らないこと、`agent_gone` で報告が消えて送れなくなること、`restart` で busy から始まること。
- 結合テスト（`tests/runner_pty.rs`）: 偽 zellij の引数と `typed` の応答、止まる偽 zellij（`sleep`）で約 5 秒後に未確認になる間も `loom done` の停止がすぐ効くこと、待機中の runner が偽 core の `input` に `not_typed` を返し何も打たないこと、agent の実行中に端末へ入れた `d` で始まる行が agent の終了後に d/r/f のプロンプトの答えにならないこと（`tcflush` を外すとこのテストが落ちることも確かめた）。
- hook の例: JSON として読めること、各コマンドに見本の hook 入力を流し、偽の `ZELLOOM_LOOM_EXE` で引数・標準入力・終了コードを確かめた（`ZELLOOM_TASK_ID` が無ければ何もしない、loom の失敗は非 0 で返る、dash と bash で同じ）。実際の Claude Code での発火は下の通し確認で確かめた。
- 通し確認（別のソケット・設定の core、ワーカーは Claude Code を権限モード default で起動し、例の `worker-settings.json` を指定）:
  - ワーカーが `loom report question` を実行しようとした Bash 自体が権限ダイアログになり、`PermissionRequest` の hook ですぐ `blocked` になった。この間の `send` は拒否された。画面では Enter が「Yes」を選ぶ状態だったので、ここを防げることが実際の環境で確かめられた。
  - ダイアログを手で承認すると `question` の報告が届き、`Stop` の hook で turn が idle になった。
  - `--expect` の不一致、先頭の `/` と `!`、ESC を含む本文はどれも拒否された。正しい `--expect` の `send` は `typed` の確認を受けて成功し、ワーカーは返事どおりに作業を続けた。
  - ファイルの書き込みと `loom report ready` もそれぞれ `blocked` になり、承認後に `ready` と idle が届いた。`loom done` でタブが閉じた。
- 通し確認で分かったこと: 権限モードが default のワーカーでは `loom report` のたびに権限ダイアログが出るので、`Bash(<loom の絶対パス> report *)` を allow に入れるようスキルの準備の項に書いた。あわせて、誰も答えない AskUserQuestion を例の設定で deny にした。
- `cargo test` / `cargo clippy --all-targets` / `cargo fmt` がクリーン。
