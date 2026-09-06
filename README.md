# anvi

Windows のどの入力欄でも、**Neovim で編集して書き戻す**ための常駐ツール。

Slack のメッセージ欄でも、ブラウザのテキストエリアでも、メモ帳でも、`Ctrl+Alt+E` を押せば
中身が Neovim のバッファとして開く。保存して閉じれば元の入力欄へ戻る。

- Neovim も描画フォントも **exe に同梱**。別途インストールするものは無い
- ユーザーの普段の Neovim 設定は読み込まない（`NVIM_APPNAME=anvi` で名前空間ごと隔離）
- 日本語 IME はインライン変換（未確定文字列をその場に描く）
- 対応: Windows 11 (x64) のみ

## インストール

### インストーラ

[Releases](https://github.com/sg004baa/anvi/releases) の
`anvi-vX.Y.Z-windows-x64-setup.exe`。管理者権限は要らない
（`%LOCALAPPDATA%\Programs\anvi` へ入る）。サインイン時の自動起動はウィザードで選べる。

### scoop

```pwsh
scoop bucket add sg004baa https://github.com/sg004baa/scoop-bucket
scoop install sg004baa/anvi
```

### portable

`anvi-vX.Y.Z-windows-x64-portable.zip` を展開して `anvi.exe` を実行するだけ。

## 使い方

1. `anvi.exe` を起動する。ウィンドウは出ず、通知領域に常駐する
2. 編集したい入力欄にフォーカスを当てて **`Ctrl+Alt+E`**
3. 開いた Neovim で編集する
4. **保存して終了（`ZZ` / `:wq` / `:x`）すると元の入力欄へ書き戻す**
5. **一度も保存せずに閉じる（`:q` / `:q!` / `×`）と破棄する**

反映するかどうかは「保存されたかどうか」だけで決まる。コマンド名では決まらない。

| 操作 | 結果 |
|---|---|
| `ZZ` / `:wq` / `:x` | 書き戻して閉じる |
| `:w` | その時点の内容を「保存済み」にする（閉じない） |
| `:q` / `:q!` / ウィンドウの `×` | 閉じる。一度も保存していなければ破棄 |
| トレイ → Exit | アプリ自体を終了する |

nvim はアプリの起動時に一度だけ立ち上がり、セッションを閉じても生き続ける。
2 回目以降のホットキーが速いのはそのため（ヤンクレジスタも引き継がれる）。

## 設定

### 自動起動

トレイメニューの **「サインイン時に起動」** で切り替える
（`HKCU\Software\Microsoft\Windows\CurrentVersion\Run` の `anvi` 値）。
インストーラの自動起動オプションと同じエントリなので、どちらから設定しても状態は一致する。

### Neovim のローカル設定

場所を決めるのは anvi ではなく Neovim である。`NVIM_APPNAME=anvi` で動くので
`stdpath("config")`、つまり **`%XDG_CONFIG_HOME%\anvi\init.lua`**。
`XDG_CONFIG_HOME` を設定していなければ Windows の Neovim はそこを `%LOCALAPPDATA%`
として扱うので **`%LOCALAPPDATA%\anvi\init.lua`** になる（`AppData\Roaming` ではない）。
普段使いの `nvim` の設定とは完全に別。

**どちらを見に行ったかは起動時のログに必ず出る。** 効いていないと思ったらまずこれを見る:

```
INFO anvi::controller: local config dir="C:\Users\you\.config\anvi" loaded=false
```

```lua
-- <上のログが指すディレクトリ>\init.lua
vim.opt.guifont = "Moralerspace Argon HW:h14"
vim.keymap.set("n", "<C-s>", "<Cmd>write<CR>")
```

`guifont` は `ファミリ名:h<サイズ>` の形を解釈する。カンマ区切りの候補列も読み、
**実在する先頭のファミリ**を使う（残りは日本語などのフォールバックに回る）。
サイズをどこにも書かなければ「GUI に任せる」とみなして同梱の
`Moralerspace Argon HW` で描く。`:b` のような解釈できない指定は
**黙って既定へ落とさず**、警告を出して現状維持する。

ローカル設定を読んだあとにアプリ側の契約（保存/破棄の扱い）を再宣言するため、
設定で `ZZ` を潰しても壊れない。

### クリップボード

`"+y` / `"+p`（と `"*`）は Windows のクリップボードに直結している。
win32yank などの外部ツールを入れる必要は無い。

- Windows 側でコピー → anvi の中で `"+p` で貼る
- anvi の中で `"+y` → 他のアプリで `Ctrl+V` で貼る

無名レジスタでも使いたい場合はローカル設定に書く:

```lua
vim.opt.clipboard = "unnamedplus"
```

行指向のヤンク（`yy` など）は、他のアプリへ渡るとき末尾に改行が付く。

### 見た目

タイトルバーは無い（`ZZ` / `ZQ` で閉じる）。背景は不透明度 75% の透過で、
文字とカーソルは不透明のまま描く。周囲に細い枠線と余白がある。

いずれも `crates/anvi/src/gui/render.rs` の先頭の定数を書き換えてビルドする。

| 定数 | 既定 |
|---|---|
| `BACKGROUND_ALPHA` | `0.75`（背景の不透明度） |
| `PADDING` | `8.0`（周囲の余白、論理ピクセル） |
| `BORDER_WIDTH` / `BORDER_ALPHA` | `1.0` / `0.45`（枠線。色は配色の前景色） |

### ホットキー

`Ctrl+Alt+E` 固定。変えたい場合は `crates/anvi/src/hotkey.rs` を書き換えてビルドする。

## ログ

`%LOCALAPPDATA%\anvi-data\log\anvi.log`。起動時に前回分を `anvi.log.1` へ退避し、今回分は空から始める（保持は前回の 1 世代）。
`RUST_LOG` を設定すればレベルを変えられる（既定は `info`）。
不具合報告にはこのファイルを添えてほしい。PC 終了時の問題は次回起動後の `anvi.log.1` も添えてほしい。書き戻しがおかしいときは
`captured route=... framework=...` の行が一次情報になる。

## 既知の限界

- **改行が送信になるアプリ**（設定によっては Slack / Discord）に複数行を書き戻すと、
  意図せず連投になりうる
- **リッチテキスト**はクリップボード経路を通ると平文になる
- 範囲選択して一部だけ編集することはできない。常に入力欄の全内容が対象
- UI Automation から入力欄と判別できない相手（一部の Web アプリ）では何も起きない
- 署名していない exe なので、セキュリティソフトの除外設定が必要になることがある

## アンインストール

- インストーラ版: 「アプリと機能」から削除。既定ではローカル設定
  （`%LOCALAPPDATA%\anvi`）と shada/state（`%LOCALAPPDATA%\anvi-data`）も消える。
  残したい場合は `unins000.exe /KEEPDATA`
- scoop 版: `scoop uninstall anvi`。ユーザーデータは残るので、要らなければ手で消す

## 開発

設計の正は [`docs/DESIGN.md`](docs/DESIGN.md)（日本語）。

```sh
cargo fmt --all -- --check
cargo clippy -p anvi-core --all-targets -- -D warnings
ANVI_TEST_NVIM=$(command -v nvim) cargo test -p anvi-core      # 本物の nvim を起動する
cargo xwin clippy -p anvi --target x86_64-pc-windows-msvc --all-targets -- -D warnings
cargo xwin build --release -p anvi --target x86_64-pc-windows-msvc
scripts/make-bundle.sh <nvim-win64 を展開したディレクトリ> <出力先>
```

`anvi` は Windows 専用なので、Linux では `anvi-core` だけをビルド・テストする
（ワークスペースの `default-members`）。

## ライセンス

MIT または Apache-2.0 のデュアルライセンス（[LICENSE-MIT](LICENSE-MIT) /
[LICENSE-APACHE](LICENSE-APACHE)）。

同梱物: [Neovim](https://github.com/neovim/neovim)（Apache-2.0）、
[Moralerspace Argon HW](https://github.com/yuru7/moralerspace)（SIL Open Font License 1.1）。
