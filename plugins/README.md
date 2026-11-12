# WASM プラグイン

Horizon サーバー用のプラグインシステムです。プラグインは Rust などで書いて WebAssembly（`.wasm`）にコンパイルし、サーバーの `plugins/` フォルダに置くだけで動きます。

- **ホットリロード**: `.wasm` を置く・上書きする・消すと、約1秒以内に読み込み直されます（サーバーの再起動は不要）。コマンド一覧もその場で作り直されます。
- **サンドボックス**: プラグインごとに独立したメモリ（上限 128 MB）。1回の呼び出しが1秒を超えると止められ、5回続けて失敗したプラグインは、ファイルが更新されるまで自動で無効になります。サーバー本体は落ちません。ファイルは自分のフォルダ（`plugins/<名前>/`）にしか触れません。
- **本体のフックがほぼ全部使える**: ゲーム本体のフック一覧から自動生成しているので、475 個のフックと 198 個の helper（ゲーム操作）がそのまま使えます。本体が更新されて増えた分も自動で追従します。
- **もっと深く本体をいじりたいとき**は、本体のソースを書き換える既存の仕組み（[horizon-mixins](../docs/native-mods.md)）を併用します。こちらは再ビルドが必要です。

## サーバーの起動

```sh
cargo build --profile fast -p horizon-wasm-plugins
```

できた `horizon-plugin-server` を、普段の Horizon サーバーと同じように起動します（`--dedicated` など、引数は本体と同じです）。プラグインは作業フォルダの `plugins/` から読み込みます。別の場所を使うときは、環境変数 `HORIZON_PLUGINS` でフォルダを指定します。

使えるフックと helper の一覧は次で出せます。

```sh
horizon-plugin-server --plugin-api
```

## プラグインの書き方

`examples/starter` がそのまま雛形になります（参加メッセージ、`/sethome` `/home`、スポーン保護、定期アナウンス）。

```rust
use horizon_plugin_sdk::prelude::*;

#[derive(Default)]
struct Hello;

impl Plugin for Hello {
    fn info(&self) -> Info {
        Info::new("hello", "1.0")
            .on("player_logged_in")   // 聞きたいフック
            .on("destroy_block")
            .command("hello [name:word]")
    }

    fn on_event(&mut self, event: &Event) -> Reply {
        match event.name.as_str() {
            "player_logged_in" => {
                host::broadcast(&format!("§e{} さんが来ました", event.str("name").unwrap_or("?")));
                Reply::none()
            }
            // キャンセルできるフックは Reply::cancel() で止められる
            "destroy_block" if event.pos("pos").is_some_and(|p| p[1] < 0) => Reply::cancel(),
            "command" => Reply::message("§aこんにちは！"),
            _ => Reply::none(),
        }
    }
}

register!(Hello::default());
```

ビルド:

```sh
rustup target add wasm32-unknown-unknown   # 初回だけ
cargo build --release --target wasm32-unknown-unknown
```

`target/wasm32-unknown-unknown/release/<名前>.wasm` をサーバーの `plugins/` に置けば完了です。

### イベント

- **フック**: `destroy_block`、`player_chat`、`living_incoming_damage_v2` など、本体のフック名そのままです。`Info::on` で聞きたいものを宣言します。中身は `event.data` の JSON で、`event.str("name")` や `event.pos("pos")`、`event.get::<T>("field")` で読みます。
- **返し方**:
  - `Reply::cancel()`: キャンセルできるフックで処理を止める
  - `Reply::none().set("field", 値)`: 書き換えられるフィールドを変更する（例: ダメージ量）
  - `Reply::none().value(値)`: 値を差し替えるタイプのフック
  - `Reply::message("…")`: コマンドの返事（`§` の色コードが使えます）
- **ホスト独自のイベント**: `enable`（読み込み後、最初の tick）、`disable`、`command`、`scheduled`、`plugin_message`

フックの中にはゲーム操作（helper）を呼べないものがあります（`player_chat` など）。その場合は helper がエラーを返すので、返事（`Reply`）で答えるか、`host::schedule` で次の tick に回してください。

### helper（ゲーム操作）

よく使うものには型付きの関数があります。

| 関数 | 内容 |
| --- | --- |
| `host::tell(uuid, text)` / `broadcast(text)` / `action_bar(uuid, text)` | メッセージを送る |
| `host::players()` | オンラインのプレイヤー一覧 |
| `host::teleport(uuid, dimension, pos, yaw, pitch)` | テレポート |
| `host::run_command(line)` | サーバーとしてコマンドを実行 |
| `host::has_permission(uuid, node, level)` | 権限の確認 |
| `host::schedule(delay, token, repeat)` / `cancel_task(token)` | tick 単位のタイマー |
| `host::config()` / `save_default_config(text)` | `plugins/<名前>/config.toml` |
| `host::data_read(path)` / `data_write(path, text)` | `plugins/<名前>/` 内のファイル（書き込みは途中で落ちても壊れない） |
| `host::send_plugin(name, data)` | ほかのプラグインへメッセージ |

それ以外の helper は、すべて `host::call("名前", json!([引数...]))` で呼べます（`set_block`、`kick`、`give`、`play_sound`、`lightning_bolt`…）。UUID は `"xxxxxxxx-xxxx-..."` 形式の文字列、テキストは文字列（`§` コード可）、ブロック状態は数値 ID です。

## コマンド

`Info::command("home [name:word]")` のように、使い方の文字列で登録します。引数は `<名前:種類>`、省略可能なものは `[名前:種類]` です。種類は `word`、`text`（行の残り全部）、`integer`、`number`、`boolean`、`player`、`item`、`block_pos`、`player_name` です。

`Info::command_with(usage, level, Some("ノード"))` で、権限ノードまたは OP レベルで制限できます。登録したコマンドはタブ補完に出ます。

サーバー側の組み込みコマンド（OP レベル 3 以上）:

- `/plugins`: 読み込み中のプラグイン一覧
- `/plugins reload [名前]`: 読み込み直し

## スキンとマント（`plugins/skins.toml`）

オンラインモードのサーバーは、ログイン時に Mojang からスキンとマントを受け取ります。オフラインモードでは受け取れないので、スキンのないプレイヤーが入ると、名前で Mojang に問い合わせて、そのアカウントのスキンとマントを付けます（プレイヤー名を Mojang に送ります）。正規のアカウントがない名前はデフォルトのスキンのままです。止めるときは `enabled = false` にします。

## 権限（`plugins/permissions.toml`）

サーバー全体の権限です。プラグインのコマンドだけでなく、本体の権限チェック（`has_permission`）にも効きます。保存すると自動で読み直されます。

```toml
[groups.default]            # 全員が入るグループ
permissions = ["starter.home"]

[groups.admin]
inherits = ["default"]
permissions = ["*"]

[players.Steve]             # プレイヤー名、または UUID
groups = ["admin"]
permissions = ["-starter.build"]   # 先頭の - は拒否
```

`a.b.*` は `a.b` 以下すべて、`*` は全部に一致します。判定の優先順は、プレイヤー個人の設定、所属グループ（継承元はその後）、`default` の順です。最初に一致したものが使われ、どれにも一致しなければ本体の OP レベルで判定されます。

## 仕組み（ABI）

ホストとプラグインは JSON でやり取りします。プラグインは `memory`、`hz_alloc`、`hz_free`、`hz_init`、`hz_event` をエクスポートし、ホストは `hz.call` と `hz.log` を提供します。詳細は `crates/horizon-wasm-plugins/src/lib.rs` の先頭にあります。SDK を使えば意識する必要はありません。Rust 以外の言語でも、この ABI を満たせば書けます。
