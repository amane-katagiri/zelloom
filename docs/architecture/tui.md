# TUI

`src/tui/mod.rs`（イベントループ）、`src/tui/app.rs`（状態とキー処理）、`src/tui/ui.rs`（描画）。ratatui + crossterm。素の `loom` が、実行した端末（ペイン）でそのまま起動する。起動前に core が動いていなければ、Zellij セッション内に限り core をデタッチ起動する（[zellij.md](zellij.md#素の-loom-起動フロー)）。

## 画面構成

4 つのリストセクションと、ステータス行・入力行から成る。

- `RUNNING`: `status == running` のタスク（`● workspace text`）
- `QUEUED`: `status == queued`（`  workspace text`）
- `INBOX`: `status == received`（`○ <source の種類>: text`）
- `INTERRUPTED / FAILED`（`done` / `cancelled` が 1 件以上あればタイトルは `INTERRUPTED / FAILED (total: N done / N cancelled)`）: `status == interrupted` と `status == failed` のタスク一覧（`position` 順）。`failed` は行頭に `✗` を付ける（`✗ workspace text`、`interrupted` は `  workspace text`）。カッコ内は現在ストアに残っている `done` / `cancelled` タスクの全期間の総数で、個々のタスクは表示されない
- ステータス行: core に到達できないときは `core not running - retrying...`、直近の操作のエラー/結果メッセージがあればそれ、無ければキー一覧
- 入力行: `> ` に続けてタスク追加/編集の入力バッファ（通常時はプレースホルダ `add task...`）。終了確認モードのときは代わりに確認プロンプト（下記）を表示する

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
| 通常 | `q` | 終了確認モードへ（core に到達できないときは即終了） |
| 通常・入力 | `Ctrl+C` | `q` と同じ（入力モードでは入力を破棄して終了確認モードへ） |
| 入力 | 文字入力 / `Backspace` / `Delete` / `←` / `→` / `Home` / `End` | 文字単位（マルチバイト対応）でバッファを編集。`Ctrl` か `Alt` を伴う文字キー（`Ctrl+C` 以外）は無視し、入力しない。`Shift` はそのまま入力する |
| 入力 | `Enter` | 確定（下記のパースへ）。空/不正な入力なら入力モードのまま `last_message` にエラーを出す |
| 入力 | `Esc` | 入力を破棄して通常モードへ戻る |
| 終了確認 | `y` | core を止めてから終了 |
| 終了確認 | `n` / `Ctrl+C` | core を止めずに終了 |
| 終了確認 | `Esc` | 通常モードへ戻る |
| 終了確認 | その他 | 無視 |

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

## Zellij へのフォーカス移動

`Enter` で `running` タスクを選ぶと `zellij --session $ZELLIJ_SESSION_NAME action go-to-tab-name <workspace>` を直接実行する（`ZELLOOM_ZELLIJ` でバイナリを上書き可能）。`ZELLIJ_SESSION_NAME` が設定されていなければエラーメッセージを `last_message` に出すだけで、TUI 自体は終了しない。
