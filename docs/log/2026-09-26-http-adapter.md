# HTTP アダプタの追加

## 決めたこと

- **core プロセスの中で動かすが、Scheduler は直接呼ばない**。HTTP ハンドラは `enqueue` リクエストを組み立てて core 自身の Unix socket にクライアントとして送るだけにした。こうすると Scheduler を呼び出せる経路が「Unix socket 経由」の 1 つに保たれ、HTTP アダプタが特別扱いされない。ハンドラ内では `tokio::task::spawn_blocking` の中で既存の同期 `client::call` を使っている。専用の非同期クライアントを書くよりコード量が少なく、他のクライアント（CLI・TUI）と同じ経路を通ることで、`enqueue` の検証・スケジューリングの挙動が完全に一致することも保証できる。
- **loopback 固定で、トークンなどの認証は入れない**。`http.listen` が `SocketAddr` としてパースでき、かつ `ip().is_loopback()` であることを設定検証で強制した。ホスト名を許さないのは、DNS で loopback 以外に解決される名前を `listen` に書けてしまうと bind 先の意図がわかりにくくなるため。認証を省いたのは、今のところローカルの自作ツールから使う用途しか想定しておらず、同じマシンの他ユーザーからの防御まではスコープ外と判断したため（[todo.md](../todo.md) に非 loopback 公開時の課題として残した）。
- **Host ヘッダのチェックを追加した**。loopback で listen していても、ブラウザから `http://127.0.0.1:7878/tasks` のような URL に対して DNS rebinding 攻撃（悪意あるページが外部ドメインを一時的に 127.0.0.1 に向けて fetch する手口）が成立しうるため、`Host` ヘッダが `localhost` / `127.0.0.1` / `[::1]`（任意で `:port` 付き）のいずれでもなければ `403` にする。
- **Origin ヘッダ + CORS allowlist を追加した**。想定用途が「自作のローカルブラウザツールから叩く」ことなので、`allowed_origins` に明示的に登録した Origin だけを許可する。`tower-http` の `CorsLayer` はブラウザへの CORS ヘッダ付与（プリフライト応答・`Access-Control-Allow-Origin` など）だけを担当させ、許可判定そのもの（403 を返すかどうか）は自前のミドルウェアで行う。CORS はブラウザ側の防御であり、`curl` などブラウザ以外のクライアントは Origin ヘッダを送らないので、Origin チェック単体では防御にならない。Host チェックと組み合わせて初めて意味を持つ。
- **レイヤの順序は Host/Origin チェックを外側、CORS を内側にした**。許可されていない Origin からのプリフライトは CORS 層に渡る前に `403` で弾きたいため。`axum` の `Router::layer` は後から呼んだ方が外側になる仕様なので、`CorsLayer` を先に、独自ミドルウェアを後に `.layer()` した。
- **`source` は常に `{"type":"http"}` 固定、`agent` と `reply_to` は受け付けない**。外部チャンネル（HTTP・将来の Nostr など）からは「どの workspace に入れるか」だけを選んでもらい、どのエージェントを使うかは zelloom 側の設定（`workspaces.<id>.agent` / `default_agent`）に委ねる、という既存の設計方針（CLI の `--agent` は zelloom を操作している本人が使うオプションで、外部ソースには開放していない）にそろえた。`reply_to` は完了通知の宛先だが、HTTP アダプタはリクエストに対して同期的に 201 を返すだけで、後から completion を push する経路を持たないため今回は扱わない。
- **`[http]` は他の設定と違い core 起動時に固定する**。core は通常、操作のたびに設定ファイルを読み直すが、TCP リスナーの bind はプロセスの起動時に一度しかできない。`[http]` の変更を反映するには core の再起動が必要、という制約を README・architecture 双方に明記した。
- **TCP リスナーの bind は既存 core の確認のあと、Unix socket の bind より前に行う**。ポート衝突で黙って HTTP なしのまま動くより、起動失敗にしたほうが気づける。既存 core の確認を先にしないと、二重起動が「already running」ではなくポート衝突として報告されてしまう。
- **core は起動時に設定を読むので、設定が不正なら起動しない**。`[http]` の有無を知るために必要になった。以前は起動に成功し、最初の操作でエラーになっていた。
- **`loom` のデタッチ起動で、core が起動中に死んだことを検出する**。設定エラーやポート衝突で core が落ちても、以前は 5 秒のタイムアウトとしか表示されなかった。子プロセスを `try_wait` で見て、終了していれば `core.log` の場所を示して終了する。
- **`metadata` はオブジェクトだけを受け付ける**。
- **GET エンドポイントは作らなかった**。今回のスコープは「タスクを追加できる」ことだけで、作成後の状態はレスポンスの Task で確認できる。状態を後から追跡する手段は [todo.md](../todo.md) に残した。

## 作ったもの

- `src/config.rs`: `Config.http: Option<HttpConfig>`、`HttpConfig { listen, allowed_origins }`、`Config::validate` での検証（`validate_http_listen` / `validate_http_origin`）、`HttpConfig::listen_addr()`。`CONFIG_TEMPLATE` にコメントアウトした `[http]` の例を追加。
- `src/http.rs`: axum ベースの HTTP サーバ。`POST /tasks` ハンドラ、Host/Origin チェックのミドルウェア、CORS レイヤの組み立て、`core::run` から呼ぶ `serve()`。
- `src/core/mod.rs`: `core::run` の先頭で設定を読み、`[http]` があれば TCP リスナーを Unix socket より先に bind。HTTP サーバを `tokio::spawn` し、Unix socket 側の後始末のあとにそのタスクを `.await` してから戻るようにした。
- 依存関係: `axum`（既定 features）、`tower-http`（`cors` feature のみ）を `cargo add` で追加。
- `tests/http_adapter.rs`: `tests/scheduler_flow.rs` と同じパターンで実 core を一時ソケット・一時 TCP ポートで起動し、生の HTTP/1.1 リクエストを `TcpStream` で送って検証する結合テスト（`reqwest` は追加していない）。
- ドキュメント: `docs/architecture/http.md`（新規）、`docs/architecture.md`・`docs/architecture/config.md`・`docs/architecture/scheduler.md` の該当箇所を更新、README に設定例と説明を追加、`docs/todo.md` から「HTTP adapter」を削除し、認証と GET エンドポイントの未実装事項を追加。

## 確認したこと

- `cargo build -j 2` / `cargo test -j 2` / `cargo clippy -j 2 --all-targets` がいずれも警告無しで通ることを確認した。
- `tests/http_adapter.rs`（11 ケース）で次を確認した: 成功時の `201` と `list` での可視性・`source.type=="http"`、workspace 未登録時の `400` とメッセージ、`Host` ヘッダ不正時の `403`、許可していない `Origin` の `403`（通常リクエスト・プリフライト双方）、許可した `Origin` からのプリフライトで CORS ヘッダが付くこと、許可した `Origin` からの `POST` が `201` かつ `access-control-allow-origin` 付きで返ること、`Content-Type` が JSON でないときの `415`、未知のフィールド（`agent`）を含むボディが `4xx` になること、`[sources.http] auto_queue = false` でタスクが `received` のままになること、core の `shutdown` 後に HTTP の TCP ポートへ接続できなくなること。
- `src/config.rs` に `http.listen` の非 loopback・ホスト名・不正値、`http.allowed_origins` の不正な形式（パス付き・クエリ付き・スキームが `ftp` など）を弾くユニットテストを追加し、通ることを確認した。
