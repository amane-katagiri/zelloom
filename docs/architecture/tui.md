# TUI

`src/tui/mod.rs`（イベントループ）、`src/tui/app.rs`（状態とキー処理）、`src/tui/ui.rs`（描画）。ratatui + crossterm。素の `loom` が、実行した端末（ペイン）でそのまま起動する。起動前に core が動いていなければ、Zellij セッション内に限り core をデタッチ起動する（[zellij.md](zellij.md#素の-loom-起動フロー)）。

## 画面構成

4 つのリストセクションと、ステータス行・入力行から成る。

- `RUNNING`: `status == running` のタスク（`● workspace text`）
- `QUEUED`: `status == queued`（`  workspace text`）
- `INBOX`: `status == received`（`○ <source の種類>: text`）
- `INTERRUPTED / FAILED`（`done` / `cancelled` が 1 件以上あればタイトルは `INTERRUPTED / FAILED (total: N done / N cancelled)`）: `status == interrupted` と `status == failed` のタスク一覧（`position` 順）。`failed` は行頭に `✗` を付ける（`✗ workspace text`、`interrupted` は `  workspace text`）。カッコ内は現在ストアに残っている `done` / `cancelled` タスクの全期間の総数で、個々のタスクは表示されない
- ステータス行: core に到達できないときは `core not running - retrying...`、直近の操作のエラー/結果メッセージがあればそれ、無ければキー一覧。メッセージは通常モードで `?` / `h` を押すと消え、キー一覧に戻る
- 入力行: `> ` に続けてタスク追加/編集の入力バッファ（通常時はプレースホルダ `add task...`）。終了確認モードのときは代わりに確認プロンプト（下記）を表示する

一覧の各行と入力行では、本文中の改行を `⏎` に置き換えて 1 行で表示する（`ui::single_line`）。

選択可能なタスク一覧（`selectable_tasks`）は `RUNNING → QUEUED → INBOX(received) → INTERRUPTED / FAILED` の順に連結した1本のリスト（セクションとその状態の対応は `app::SECTION_STATUSES`）で、`↑/↓`（`k`/`j`）はこの結合リストの中を動く。選択はタスクIDで追跡するので、一覧が更新されて並びが変わっても同じタスクを選び続ける（該当タスクが消えたら選択位置を新しいリストの範囲内にクランプする）。

**既知の制約**: リストは `ratatui::widgets::List` を `ListState`/スクロールオフセット無しでそのまま描画しているため、セクションの高さを超える件数のタスクがあると、はみ出した分は画面に表示されない（選択自体はリストの末尾まで動くが、画面をスクロールする手段が無い）。

## ポーリング

500ms 間隔で `List` リクエストを送って `app.set_tasks` に反映し、成功したら続けて `Status` リクエストを送って応答の `workspaces`（core が読んだ設定の workspace ID 一覧）を `app.workspace_ids` に、`tui_default_workspace` を `app.default_workspace` に入れる（`refresh`）。`workspaces` が取れなければ前回の一覧を使い続ける。TUI 自身は設定ファイルを読まない。キー入力待ちは `crossterm::event::poll` に残りの待ち時間を渡すので、ポーリングとキー入力待ちが同じループの中で両立する。操作（`Action`）を core に送って成功した直後にも即座にもう一度 `refresh` する（失敗時はエラーを `last_message` に出す）。

## キー操作

| モード | キー | 動作 |
|---|---|---|
| 通常 | `↑` / `k`, `↓` / `j` | 選択移動 |
| 通常 | `n` / `o` | 入力行にフォーカスし、タスク追加モードへ |
| 通常 | `Enter`（`running` を選択中） | その workspace の Zellij タブへフォーカス移動 |
| 通常 | `e`（`running` 以外を選択中） | 選択中タスクの本文を編集モードで開く（本文をあらかじめ入力欄に入れる） |
| 通常 | `dd`（`queued`/`received`/`interrupted`/`failed` を選択中） | 削除（1回目の `d` は保留、2回目で確定。`d` 以外のキーを押すと保留は解除される） |
| 通常 | `J` / `K`（`queued`/`received` を選択中） | キュー内で下/上に1つ移動 |
| 通常 | `a`（`received` を選択中） | Inbox から accept（`queued` へ） |
| 通常 | `r`（`received` を選択中） | reject |
| 通常 | `f`（`running` を選択中） | 完了にする（`loom done` と同じ効果） |
| 通常 | `R`（`interrupted`/`failed` を選択中） | retry（キュー末尾へ戻す） |
| 通常 | `D`（`interrupted` を選択中） | 完了扱いにする（`done`） |
| 通常 | `c`（`queued`/`received`/`interrupted` を選択中） | cancel |
| 通常 | `?` / `h` | ステータス行のメッセージを消してキー一覧に戻す |
| 通常 | `q` | 終了確認モードへ（core に到達できないときは即終了） |
| 通常・入力 | `Ctrl+C` | `q` と同じ（入力モードでは入力を破棄して終了確認モードへ） |
| 入力 | 文字入力 / `Backspace` / `Delete` / `←` / `→` / `Home` / `End` | 文字単位（マルチバイト対応）でバッファを編集。`Ctrl` か `Alt` を伴う文字キー（`Ctrl+C`・`Ctrl+U` 以外）は無視し、入力しない。`Shift` はそのまま入力する |
| 入力 | `Ctrl+U` | カーソルより前を消す |
| 入力 | 貼り付け（bracketed paste） | 貼り付けたテキストを改行ごとカーソル位置に挿入する（`\r\n` と `\r` は `\n` にそろえる） |
| 入力 | `Enter`（バッファが空白だけ） | 1 回目は「エディタ待ち」にし、その間はステータス行に `press Enter again to write the task in <editor>` を出す。エディタ待ちでもう一度押すとエディタを開く（下記） |
| 入力 | `Enter`（それ以外） | 確定（下記のパースへ）。不正な入力なら入力モードのまま `last_message` にエラーを出す |
| 入力 | `Esc` | 入力を破棄して通常モードへ戻る |
| 終了確認 | `y` | core を止めてから終了 |
| 終了確認 | `n` / `Ctrl+C` | core を止めずに終了 |
| 終了確認 | `Esc` | 通常モードへ戻る |
| 終了確認 | その他 | 無視 |

エディタ待ち（`Mode::Input` の `editor_armed`）は、入力モードで `Enter` 以外のキー（`Esc` を含む）を押すか貼り付けると解除され、ステータス行も元の表示に戻る。通常モードでの貼り付けは無視し、`last_message` に `press n to add a task before pasting` を出す（貼り付けた文字をショートカットキーとして解釈しないため、端末の bracketed paste を有効にしている）。

条件を満たさないキー（例: 選択中タスクの状態がその操作に合わない）は何もせず、`last_message` に理由を表示するだけで終わる（`Enter` だけは何も表示しない）。各キーが受け付ける状態は、core がその操作を許す状態（[protocol.md](protocol.md#リクエスト一覧)）の部分集合にしている。

## 終了確認

`q`（または `Ctrl+C`）で入る終了確認モード（`Mode::ConfirmQuit { running }`）では、入力行に次のプロンプトを出す。`running` は直前のポーリング結果の `running` タスク数。

- `running == 0`: `Stop core too? [y]es / [n]o / [Esc] cancel`
- `running > 0`: `Stop core too? N task(s) running will become interrupted. [y]es / [n]o / [Esc] cancel`

core に到達できない（`core_reachable == false`）ときは確認を出さずに即終了する。

`y` は `Action::StopCoreAndQuit { force: running > 0 }` を返し、通常モードに戻る。イベントループはステータス行に `stopping core...` を描画してから `loom stop` と同じ `cli::stop_core`（最大 5 秒、ソケットが閉じるまで待つ）を呼ぶ。成功すれば終了し、失敗すれば TUI に留まってエラー（改行は空白に置き換える）をステータス行に出す。プロンプトに実行中タスクが無かったのに、その後タスクが始まって core が停止を拒否した場合もこの失敗として扱う。

## 追加/編集の入力パース

- タスク追加: 入力が `ws: 本文` の形で、コロンより前（前後の空白を除く）が登録済みの workspace ID（`app.workspace_ids`）に一致するときだけ、それを workspace、コロンより後を本文にする。そうでなければ入力全体を本文とし（`fix: ...` のような本文はそのまま残る）、`app.default_workspace`（設定の `tui.default_workspace`）があればそれ、無ければ現在選択中のタスクの workspace を既定にする（どちらも無ければエラー）。
- 編集: 選択中タスクの本文をあらかじめバッファに入れておき、`Enter` で本文だけを置き換える（workspace や状態は変えない）。

## エディタでの入力

エディタ待ちで `Enter` を押すと `Action::OpenEditor { initial }` を返す。`initial` は追加なら空、編集なら元のタスクの本文。イベントループは端末を TUI 終了時と同じ状態に戻してから `editor::compose`（[runner.md](runner.md#エディタでの入力) と同じ。一時ファイルに `initial` を書いてエディタで開き、前後の空白を除いた内容を返す）を呼び、終わったら raw モード・代替画面・bracketed paste を戻して画面を描き直す。

結果は `App::finish_editor` に渡す。

- 空でない本文: 入力行で `Enter` を押したときと同じパース・確定をする（追加なら先頭の `ws:` も解釈する）。
- 空: キャンセル扱い。何も出さずに入力モードを抜ける（`Esc` と同じ）。
- エディタが起動できない・非 0 で終了した: 入力モードのまま `error: ...` を出す。

## Zellij へのフォーカス移動

`Enter` で `running` タスクを選ぶと `Zellij::go_to_tab_name` で `zellij --session $ZELLIJ_SESSION_NAME action go-to-tab-name <workspace>` を実行する（zellij の出力は捕捉し、TUI の画面には流さない）。成功したら `last_message` を消す。`ZELLIJ_SESSION_NAME` が設定されていないときや zellij が失敗したときはエラーメッセージを `last_message` に出すだけで、TUI 自体は終了しない。
