# 2026-09-26 素の `loom` をその場で TUI として動かす

素の `loom` は core を起動したうえで、Zellij に `loom` という名前の管理タブを作り（あればそこへ移動し）、その中で `loom tui` を動かしていた。これを、実行した端末（ペイン）でそのまま TUI を動かす形に変えた。現状の仕様は `docs/architecture/zellij.md` の「素の `loom` 起動フロー」と `docs/architecture/tui.md` を参照。

## なぜ

- Zellij のレイアウトのペインから `loom` を起動すると、管理タブが別に作られ、起動元のペインはすぐ終了して exited のまま残った。タブが 1 つ余分に増える。
- レイアウトで `loom` という名前のタブを用意しておくと、そのタブが管理タブとして見つかってしまい、フォーカス移動だけして終わるため TUI が一度も起動しなかった。
- どちらも「管理タブを探す/作る」仕組みそのものが原因なので、仕組みごとやめた。

## 決めたこと

- **素の `loom` は常にその場で TUI を動かす**: タブの作成・検索・移動はしない。どこに置くかは利用者（レイアウトやペイン）が決める。
- **Zellij が必要なのは core を起動するときだけ**: core は workspace タブを作るのにセッション名を使うので、core がいないときに `ZELLIJ_SESSION_NAME` が無ければエラーにする。core が既に動いていれば、Zellij の外からでも TUI を開ける。
- **`loom tui` サブコマンドを廃止**: 素の `loom` と同じ役割になるため。
- **workspace ID `loom` の予約を廃止**: 管理タブと取り違えないための予約だったので不要になった。空・空白/制御文字・`:` の拒否は残す。
- **core のデタッチ起動は変えない**: stdin は `/dev/null`、stdout/stderr は `core.log`、`setsid` 済みで、TUI の端末を引き継がない。

## 作ったもの・消したもの

- `cli::ensure_core(socket)`: core が動いていなければ（Zellij セッション内に限り）デタッチ起動してソケットを待つ。`run_bare_loom` はこれを呼んでから `tui::run` を実行するだけになった。
- 削除: `Command::Tui`、`config::MANAGEMENT_TAB_NAME` と ID 検証の `loom` 予約、`Zellij::go_to_tab_name`、`Zellij::new_tab` の `focus` 引数（呼び出し元が workspace タブだけになり、常に `--no-focus`）。
- README: 起動手順と CLI 表を更新し、Zellij レイアウトから `loom` を起動する例と、TUI 終了時に core を止めるか確認されることを追記。

## 検証

- 単体テスト（`src/config.rs`）: `workspace_id_validation` で `loom` を有効側に移し、`loom_is_a_valid_workspace_id`・`add_workspace_accepts_loom_as_id` を追加。予約 ID のテストは不正 ID（`a:b`）で設定ファイルを作らないことを確かめる `add_workspace_rejects_invalid_id_without_creating_file` に置き換えた。
- 結合テスト（`tests/scheduler_flow.rs`）: `ensure_core_with_running_core_needs_no_zellij`（`ZELLIJ_SESSION_NAME` 無しでも動作中の core があれば成功）、`ensure_core_without_core_outside_zellij_fails`（core も Zellij も無ければ Zellij に触れたエラー）。
- 一時的な設定・状態ディレクトリと専用ソケットで手動確認した（`zellij` は実行せず、バイナリの上書きで存在しないパスを指定）。
  - core も Zellij も無い: エラー終了。
  - 事前に起動した core があり Zellij 無し: 擬似端末上でそのまま TUI が描画され、`q`→`n` で終了し core は残る。
  - core 無しでセッション名だけ設定: core がデタッチ起動され（stdin は `/dev/null`、stdout/stderr は `core.log`、セッションリーダーで制御端末なし）、TUI が描画される。`q`→`y` で core も止まる。
- `cargo build` / `cargo test` / `cargo clippy --all-targets` / `cargo fmt` がすべてクリーン。
