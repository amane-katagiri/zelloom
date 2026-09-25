# HTTP アダプタ

実装は `src/http.rs`（ルーティング・ハンドラ・セキュリティチェック）、`src/core/mod.rs`（起動・終了）、`src/config.rs`（`[http]` の型と検証）。

## 位置づけ

HTTP サーバは **core プロセスの中で** 動くが、Scheduler を直接呼ぶことはない。core 自身の Unix socket に対する 1 クライアントとして振る舞い、リクエストごとに `enqueue` を送って結果をそのまま HTTP のレスポンスに変換するだけ。この境界により、Scheduler を呼び出せる経路は「Unix socket 経由のリクエスト」1 つに保たれる。

## 設定

```toml
[http]
listen = "127.0.0.1:7878"
allowed_origins = ["http://localhost:5173"]
```

| キー | 型 | 既定 | 検証 |
|---|---|---|---|
| `http.listen` | 文字列 | （`[http]` があれば必須） | `std::net::SocketAddr` としてパースできること（ホスト名不可）。かつ `ip().is_loopback()` であること。満たさなければ `Config::validate` が設定エラーにする |
| `http.allowed_origins` | 文字列配列 | 空 | 各要素が `http://host[:port]` または `https://host[:port]` の形であること（パス・クエリ・フラグメント・末尾スラッシュ不可）。満たさなければ設定エラー |

`[http]` セクションが無ければ HTTP サーバは起動しない。

## ライフサイクル

- `core::run` は既存 core の確認（[scheduler.md](scheduler.md#core-の起動と終了)）のあと設定ファイルを読み、`[http]` があれば Unix socket より先に TCP リスナーを bind する。設定が読めない・検証エラーのとき（`failed to load config <path>: <err>`）と bind に失敗したとき（`failed to bind http listener on <addr>: <err>`）は core の起動自体が失敗する。
- `loom` が core をデタッチ起動した場合、core のエラーは `core.log` に出る。`loom` は core がソケットを開く前に終了したことを検出し、`loom core exited during startup (<status>); see <core.log>` で終了する（[zellij.md](zellij.md)）。
- **`[http]` の変更は core を再起動するまで反映されない**。他の設定項目と違い、core は起動時に読んだ `[http]` の内容（`listen` と `allowed_origins`）だけを使い続ける。Scheduler 側の設定読み直し（`enqueue` の workspace/agent 解決など）には影響しない。
- 起動できたら `[zelloom-core] http listening on <addr>` をログに出す（Unix socket 側の `listening on <path>` と同じ形式）。
- 終了は Scheduler の shutdown watch（`subscribe_shutdown`）を購読し、`shutdown` リクエストが受理されたときに axum の graceful shutdown で止める。`core::run` は Unix socket 側の後始末が終わったあと、HTTP サーバのタスクが終わるのを `.await` してから戻る。

## エンドポイント

`POST /tasks` のみ。

リクエストボディ（`Content-Type: application/json` 必須、未知のフィールドは拒否）:

```json
{"text": "READMEの設定例を修正する", "workspace": "myapp", "metadata": {}}
```

- `agent` / `reply_to` / `source` は受け付けない。agent は workspace の設定（`workspaces.<id>.agent` / `default_agent`）で決まる。
- `metadata` は省略可（既定 `{}`）。指定するならオブジェクトでなければならない。

内部的には次のリクエストを core 自身の Unix socket に送る。

```json
{"op": "enqueue", "text": "...", "workspace": "...", "source": {"type": "http"}, "metadata": {...}}
```

`source.type` は常に `http` 固定。`sources.http.auto_queue` の扱いは他の source と同じで、core 側の既存の仕組みがそのまま効く（[protocol.md](protocol.md#リクエスト一覧)）。

### レスポンス

| 状況 | ステータス | ボディ |
|---|---|---|
| 成功 | `201 Created` | 作成された Task（[protocol.md](protocol.md#task)） |
| core が `ok:false` を返した（workspace 未登録など） | `400 Bad Request` | `{"error": "<core のエラーメッセージ>"}` |
| core の Unix socket に接続できない | `503 Service Unavailable` | `{"error": "..."}` |
| リクエストボディが不正（JSON として読めない、必須フィールド欠落、未知のフィールドを含む） | `400` または `422`（axum の既定の判定に従う） | `{"error": "..."}` |
| `Content-Type` が `application/json` でない | `415 Unsupported Media Type` | `{"error": "..."}` |

## セキュリティチェック

ハンドラの手前で、ルーティングを包むミドルウェアとして次を順に検査する（`src/http.rs` の `security_check`）。

1. **Host ヘッダ**: `localhost` / `127.0.0.1` / `[::1]`（いずれも任意で `:port` 付き）以外は `403 Forbidden`。ヘッダが無い場合も `403`。DNS rebinding 対策。
2. **Origin ヘッダ**: 付いていて、かつ `allowed_origins` に含まれていなければ `403 Forbidden`。付いていない場合は素通りする（ブラウザ以外のクライアントを想定）。

## CORS

`allowed_origins` が空でなければ、`tower-http` の `CorsLayer` を `allowed_origins` の完全一致リストで設定する（メソッドは `POST` のみ、許可ヘッダは `content-type` のみ）。空なら `CorsLayer` 自体を組み込まず、CORS 関連ヘッダは一切付かない。

レイヤの順序は「Host/Origin チェック」が外側、`CorsLayer` が内側になるように組む（`Router::layer` は後から呼んだ方が外側になる）。

- 許可されていない Origin からの `OPTIONS /tasks`（プリフライト）は、`CorsLayer` に渡る前に Host/Origin チェックで `403` になる。
- 許可された Origin からのプリフライトは Host/Origin チェックを通過し、`CorsLayer` がそのまま応答して CORS ヘッダ（`Access-Control-Allow-Origin` など）を返す。
- 許可された Origin からの実際の `POST /tasks` も同様にチェックを通過し、レスポンスに `access-control-allow-origin` が付く。

## 認証

認証は無い。同じマシンのプロセスは誰でも `POST /tasks` できる。
