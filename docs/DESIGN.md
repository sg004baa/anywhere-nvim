# anvi — 設計ドキュメント

**ステータス:** v2 実装済み（UI 内製。Windows 実機で起動〜取得〜編集〜書き戻しまで確認。IME の実機確認は継続中）
**対象プラットフォーム:** Windows 11 (x64) 専用
**実装言語:** Rust (ネイティブアプリ)

---

## 0. アウトライン

- 1〜2章: 何を作るのか
- 3〜4章: 全体構成と、なぜその構成なのか（却下案含む）
- 5〜9章: 各コンポーネントの詳細仕様
- 10章: 既知の限界（実装前に必ず読むこと）
- 11〜12章: 実装順序と受け入れ条件
- 13章: 配布（インストーラ / scoop）
- 付録: 動くコードの骨組み

**実装は 12 章の順序で進めること。** ステップ 1 が通るまで他に手を出さない。

---

## 1. 目的

どこでも Neovim でテキスト編集を行うための、個人用ツール。

バックグラウンドに常駐し、グローバルホットキーで現在フォーカス中の入力欄の内容を Neovim に持ち込み、編集結果を元のアプリへ書き戻す。

### 指針

> このアプリは **Neovim を使うこと** が目的ではない。
> **「どこでも快適にテキスト編集できること」** が目的であり、その手段として Neovim を利用する。

### 設計原則

- 個人利用を前提とする
- シンプルさを最優先する
- 必要になってから機能を追加する
- 汎用性・拡張性を目的に設計しない
- ユーザーが一時ファイルや内部実装を意識しない体験を目指す

---

## 2. スコープ

### v1 でやること

- トレイ常駐 + グローバルホットキー
- フォーカス中の入力欄からのテキスト取得（UIA 優先 / クリップボード fallback）
- 常駐 Neovim での編集
- 元の入力欄への書き戻し（UIA 優先 / クリップボード貼り付け fallback）
- 元アプリへのフォーカス復帰
- 同時 1 セッションのみ。編集中にホットキーが押されたら既存ウィンドウへフォーカス
- ローカルの `init.lua` による設定の上書き（→ 5.4）

### v1 でやらないこと

- 複数セッション
- エディタの差し替え（UI も nvim も自前の組み合わせに固定する）
- 設定 GUI（設定はローカルの `init.lua` を直接書く）
- 幅広いアプリケーションへの対応保証
- クロスプラットフォーム対応
- プラグインシステム

### 将来検討

AI 補完 / スニペット / テンプレート / 編集履歴 / アプリごとの設定 / 編集対象フィルタ / Markdown プレビュー

---

## 3. アーキテクチャ

### 3.1 プロセス構成

```
┌─────────────────────────────────────────────────┐
│ anvi.exe  (host)                       │
│                                                 │
│  - トレイアイコン                               │
│  - グローバルホットキー                         │
│  - UI Automation / クリップボード               │
│  - 対象ウィンドウの HWND 保持とフォーカス復帰   │
│  - 編集ウィンドウ（winit + Direct2D）と IME     │
│  - セッション状態機械                           │
└───────┬─────────────────────────────────────────┘
        │ msgpack-rpc。API クライアントと UI クライアントを 1 本で兼ねる
        │ 127.0.0.1:PORT
        ↓
┌──────────────────────────────────────────────────┐
│ nvim.exe --headless --listen 127.0.0.1:PORT      │
│                                                  │
│  - バッファの実体（常駐。セッション毎に死なない）│
│  - 同梱 init.lua + ローカル設定をロード（→ 5.4） │
└──────────────────────────────────────────────────┘
```

2 プロセスともアプリ起動時に立ち上がり、終了まで生き続ける。

### 3.2 各プロセスの責務

| プロセス | 責務 | 責務外 |
|---|---|---|
| host | OS との対話全般、セッション管理、描画・入力・IME | テキストの編集そのもの |
| nvim | テキストの保持と編集 | 画面表示、OS 操作 |

**nvim を起動するのは host の責務。** host は同じ RPC 接続で `nvim_ui_attach` を呼び、API クライアントと UI クライアントを兼ねる。接続を 2 本張らないので、切断・再起動の面倒を見る先も 1 つで済む。

### 3.3 通信経路

| 経路 | 手段 |
|---|---|
| host → nvim | msgpack-rpc over TCP。`nvim_buf_set_lines` 等の API 呼び出しと、`nvim_input` / `nvim_ui_try_resize` |
| nvim → host | `vim.rpcnotify(host_chan, "...")` による通知（init.lua から）と、`ui_attach` 済みチャンネルへ流れる `redraw` |

`redraw` は RPC のハンドラでは解釈せず、そのまま core の `ui` へ渡して状態を組み立て、GUI が自前で描く（→ 7）。

---

## 4. 設計判断ログ

実装中に「なぜこうなってるんだ」と思ったら、まずここを読むこと。

### 4.1 nvim を常駐させる（毎回起動しない）

**判断:** nvim は起動時に一度だけ立ち上げ、セッションが終わっても殺さない。

**理由:** 「編集開始までを高速にする」が要件。Windows での nvim コールドスタートは 200〜400ms かかる。加えてセキュリティソフトの新規プロセススキャンが乗ると体感が破綻する。

**トレードオフ:** register / undo 履歴 / jumplist がセッション間で引き継がれる。これは個人ツールでは概ね利点（前のセッションでヤンクしたものを次で貼れる）。バッファの汚れが気になる場合はセッション開始時に旧バッファを `bwipeout` する。

### 4.2 UI を自前で持つ（Neovide を同梱しない）

**判断:** host 自身が `nvim_ui_attach` して描画する。winit がウィンドウ・キーボード・IME を、Direct2D/DirectWrite が描画を担う。

**却下案:** Neovide の同梱（v1 はこれだった）。

**捨てた理由は IME。** Neovide を選んだ最大の動機が日本語入力だったにもかかわらず、実機で **変換中の未確定文字列がまったく表示されなかった**。Neovide は winit から受け取った preedit を自分で描かず、lua の `neovide.preedit_handler()` に投げる設計で、既定では誰も描かない（neovide#1931、FAQ）。描かせるには nvim nightly の API とサードパーティのプラグインが要る。「どこでも快適にテキスト編集」が目的のアプリで、日本語入力が他人の実装待ちになるのは本末転倒。

**winit 自体は問題なかった。** `src/platform_impl/windows/ime.rs` は `GCS_COMPSTR` に加えて `GCS_COMPATTR` を読み、**変換対象クラスタのバイト範囲つき**で `Ime::Preedit` を配る。`WM_IME_SETCONTEXT` では `ISC_SHOWUICOMPOSITIONWINDOW` を落とし、`WM_IME_COMPOSITION` では `DefWindowProc` を呼ばない ── つまり既にインライン IME として振る舞う。足りなかったのは「preedit を描く UI」だけだった。

**代償:** 端末エミュレータ相当を自前で持つことになる。`grid_line` / `grid_scroll` / `hl_attr_define` / `mode_change` の適用（`anvi-core` の `ui`）、等幅グリッドの描画（`gui/render.rs`）、キーの nvim 記法への変換（`ui::input`）。ただしそのぶん UI プロセスが消え、v1 で泥仕事だったウィンドウ探索・ツールウィンドウの回避・UI プロセスの生存監視がまるごと不要になった。

**描画に Direct2D/DirectWrite を選んだ理由:** Windows 専用アプリであり、`windows` crate は UIA で既に依存にある。日本語のフォントフォールバックとグリフ品質はシステムの実装がいちばん確実で、GPU スタックを 1 つ増やさずに済む。

### 4.3 セッション終了で nvim を殺さない

**判断:** `:q` 系コマンドを乗っ取り、nvim を生かしたままウィンドウを隠す。

**理由:** 素の `:q!` は UI ではなく **nvim 本体を殺す**。常駐サーバーが消滅し、編集ウィンドウは何も映さなくなる。

**却下案:** 毎回死なせて、即座に新しいペアを裏で spawn しておく。

一見きれい（`:q!` も `ZZ` もネイティブに動き、状態も毎回まっさら）だが、nvim の起動は一瞬では終わらない。ホットキーを押した瞬間に前の nvim がまだ畳まれていなければ待たされるし、4.1 で挙げた「前のセッションのヤンクが次でも使える」も失われる。常駐しているものを毎回作り直す理由がない。

ただし **この案は異常系のリカバリとして採用する**（→ 6.3）。

### 4.4 保存/破棄をコマンド名ではなく「状態」で契約する

**判断:** 「`:wq` で反映、`:q!` で破棄」という仕様は採らない。「**一度でも保存されたら、最後に保存された内容を反映。一度も保存されずに終了したら破棄**」とする。

**理由:** コマンド名で契約すると `:x` / `ZZ` / `:wqa` が全部落ちる。合言葉を正確に言えた人だけ入れる店になる。

**含意:** この契約は通常のファイル編集と同型になる。`:w` してから `:q!` しても、書き込み済みの内容が巻き戻らないのはファイルと同じ。したがって `session_end` は「反映せよ/破棄せよ」の情報を運ばない。host が「このセッションで `session_write` を受信したか」だけで反映可否を決める。

**注:** `:w` 時点では host は内容を保持するだけで、書き戻しはセッション終了時にまとめて行う（**ライブ反映はしない**。クリップボード経路の書き戻しは対象へのフォーカス移動を伴うため、編集中の反映は原理的に成立しない）。旧 AutoHotkey 版で使っていた SHA256 による変更検知は機構として不要になる（内容無変化時に書き戻しをスキップする最適化としてのみ残す。相手アプリの undo 履歴を無駄に汚さないため）。

### 4.5 一時ファイルを作らない

**判断:** ディスクに一切書かない。`buftype=acwrite` + `BufWriteCmd` で `:w` を乗っ取る。

**理由:** 要件「ユーザーが一時ファイルを意識しない」。そもそも作らなければ意識しようがない。

### 4.6 TCP を使う（名前付きパイプではない）

**判断:** `--listen 127.0.0.1:PORT`。

**理由:** 名前付きパイプのほうが Windows では筋が良いが、個人ツールでローカルポートを開くのが許容できるなら TCP のほうが実装が単純で、nvim 側の指定も 1 行で済む。

**注意:**

- ポートは固定値ではなく、起動時に空きポートを取得して使う（多重起動時の衝突回避）。取得から nvim の bind までの間に他プロセスに奪われる可能性（TOCTOU）があるため、nvim の起動失敗時はポートを取り直して再試行する
- spawn 直後は nvim がまだ listen していない。host からの接続はリトライループで行う
- 127.0.0.1 とはいえ、同一マシンの任意プロセスが nvim RPC（= 任意コード実行）に接続できる。個人ツールとして許容する

### 4.7 ローカル設定は「同梱コアの後」に読む

**判断:** 同梱コア → ローカル設定 → **契約の再宣言**、の順で読む。

**却下案:** ローカル設定 → 同梱コア（後勝ちで同梱を守る）。

一見すると「同梱を後に読めば壊れないので安全」に見えるが、三点で成立しない。

1. **ローカル設定がほぼ無意味になる。** ローカル設定に書きたいものは実際には見た目とオプション（`wrap` / `number` / `colorscheme` / `listchars`）とキーマップである。同梱が後勝ちする以上、同梱が触れる項目は全て潰される。これを回避するには「同梱が何も設定しない」ようにするしかなく、そうなると守っているのは読み込み順ではなく同梱の空虚さである。

2. **読み込み順は強制力ではない。** ローカル設定は `VimEnter` / `vim.schedule` / `defer_fn` で自分の処理を後ろへ回せる。順序は減速帯であって壁ではない。

3. **そもそも守るべきものの大半は順序に依存しない。** 契約の中核である `buftype=acwrite`、`BufWriteCmd`、バッファローカルオプションは `start_session()` の中で **セッション毎に** 張られる。これはどちらの設定ファイルよりも遥かに後に実行されるため、読み込み順で壊されることが原理的にない。

順序に対して脆いのは以下の 3 つだけである。

- `ZZ` / `ZQ` のグローバルキーマップ
- `:q` 系の `cnoreabbrev`
- `VimLeavePre` の autocmd

**そしてこの 3 つは、壊れても致命傷にならない。** 6.3 の安全網（RPC 切断検知）が既にこのケースを設計に織り込んでいる。`ZZ` が潰されて素の `:q!` が nvim に届いても、host は切断を検知して nvim を再起動し Idle に戻る。データ破損ではなく機能低下として着地する。

したがって **同梱コアを先に読み、ローカル設定に自由を与え、最後に上記 3 つだけを再宣言する** のが最善。ローカル設定は見た目とオプションを自由に上書きでき、契約は再宣言で復元され、再宣言すら遅延実行で掻い潜られた場合は安全網が受け止める。却下案が欲しかった保護を、却下案のコストを払わずに得られる。

**注:** より重要なリスクは上書きではなく **ローカル設定の破損** である。文法エラーや存在しないモジュールの `require` 一つでアプリ全体が起動しなくなり、しかも原因が分かりにくい。**`pcall` で囲み、失敗しても同梱コアのみで続行すること**（→ 5.4）。

---

## 5. Neovim サーバー

### 5.1 起動コマンド

```
set NVIM_APPNAME=anvi
nvim.exe --headless --listen 127.0.0.1:%PORT% -u <bundled>\init.lua --noplugin
```

`-u` で指定しただけでは bundle ディレクトリは runtimepath に乗らない。**bundled init.lua の先頭で bundle ディレクトリを runtimepath に追加してから `require('anvi')` すること**（→ 付録 A-1）。

同梱 init.lua はエントリポイントであり、ローカル設定の読み込み順もここで決まる（→ 5.4）。

### 5.2 既存環境からの隔離（最重要）

**ユーザーの通常の Neovim 設定を絶対に読み込ませないこと。**

lazy.nvim / Mason / nvim-treesitter / LSP がロードされると、常駐化で稼いだ起動速度が全て無意味になる。

`-u` の指定だけでは不十分で、shada / state / site ディレクトリは既存のものを参照しに行く。**`NVIM_APPNAME` で名前空間ごと隔離すること。** `--noplugin` も併せて指定する。

`NVIM_APPNAME` を分けておけば shada も分離されるため、「前のセッションのヤンクが次でも使える」という 4.1 の利点を安全に享受できる。

### 5.3 同梱物

- `nvim.exe` および runtime 一式
- 専用 `init.lua`
- 描画フォント `Moralerspace Argon HW`（**exe に焼き込む**。ファイルとしては配らない）
- ライセンス（`LICENSE-MIT` / `LICENSE-APACHE` / `LICENSE-Moralerspace.txt` / `nvim/LICENSE.txt`）

UI は host 自身が持つため、同梱する exe は nvim だけ。システムにインストールされた Neovim には一切依存しない。

フォントは `include_bytes!` で exe に入れ、DirectWrite のカスタムフォントコレクションとして使う（`gui/fontset.rs`）。**利用者の環境に何が入っていようと等幅 + 日本語が出ることを保証する**ためで、これが `guifont` 未指定時の既定であり、フォールバック鎖の最後尾でもある。`guifont` でシステムフォントを指定すればそちらが primary になり、そのフォントに無い文字だけ同梱フォントが拾う。

`guifont` の読み方は 3 通りしかない（`gui/font.rs` の `GuiFont`）。

| 値 | 扱い |
|---|---|
| `A:h12` / `A:h12,B:h12`（サイズあり） | その指定。候補列は **実在する先頭のファミリ**が primary、残りはフォールバック鎖 |
| 空文字列、`A,B`（サイズがどこにも無い） | **GUI に任せる** = 同梱フォント |
| `A:h12:b` / `A:habc` など | 解けない。現状維持 + 警告 |

**サイズの無い候補列を警告にしてはいけない。** nvim 0.12 の `guifont` の組み込み既定値が
`"Cascadia Code,Cascadia Mono,Consolas,Courier New,monospace"` であり、これは
`ui_attach` 直後に必ず `option_set` で飛んでくる。利用者が何も選んでいないことの
表明なので、起動のたびに警告を出すのは誤報にしかならない。

アイコンも exe のリソースとして埋め込む（`build.rs` が名前 ID `1` で入れる）。トレイ・タスクバー・インストーラ・アンインストーラが同じ実体を指す。絵の出典は `scripts/make-icon.py` で、生成物 `assets/anvi.ico` をコミットしている。

**セキュリティソフトの除外設定に同梱 exe を追加すること。** ESET を含む多くの製品は、署名のない新規 exe に対してスキャンやプロセス保護を行い、起動遅延やファイルアクセス拒否の原因になる。

### 5.4 init.lua の構成とローカル設定

#### 同梱コアが持つもの

初期状態では極力シンプルに保つ。以下のみを持つ。

- host との RPC チャンネル登録
- セッション用バッファの管理
- `:w` の乗っ取り
- `:q` 系の乗っ取り
- 異常終了時の通知
- `vim.g.clipboard` provider の登録（→ 5.6）

見た目やキーマップは同梱コアには入れない。ローカル設定側の領分とする。

#### ローカル設定の場所

**専用のディレクトリを新設しない。** `NVIM_APPNAME=anvi` を指定している時点で、Neovim の設定ディレクトリは既にこのアプリ専用のものへ切り替わっている。

```lua
vim.fn.stdpath("config")  -- → $XDG_CONFIG_HOME/anvi
vim.fn.stdpath("data")    -- → $XDG_DATA_HOME/anvi-data
```

**パスをハードコードしないこと。** `stdpath()` から導出すれば、APPNAME を変えたときも追従する。

読み込む対象は `$XDG_CONFIG_HOME/anvi/init.lua` の 1 ファイル。存在しなければ何もしない（ローカル設定は完全に任意）。

> `XDG_CONFIG_HOME` 未設定時、Windows の Neovim はこれを `%LOCALAPPDATA%` として扱う（`AppData\Roaming` ではない）。したがって既定では `%LOCALAPPDATA%\anvi\init.lua`。

**解決先は必ず host へ報告する（`config_resolved`）。** 探索パスが 1 つしかなくても、
その 1 つがどこかは環境変数次第で動く。「設定が効かない」の原因が
`XDG_CONFIG_HOME` の有無だったとき、ログに出ていなければ利用者は当てずっぽうで
`AppData\Roaming` にファイルを置くことになる（実際に起きた）。読み込みの成否
（`loaded`）も一緒に運び、host は起動時に 1 行出す:

```
INFO anvi::controller: local config dir="C:\Users\you\.config\anvi" loaded=false
```

#### 読み込み順

```
1. 同梱コア（契約の確立）
2. ローカル設定（pcall で保護）
3. 契約の再宣言（キーマップ / abbrev / VimLeavePre）
```

判断の根拠と却下案は 4.7 を参照。実装は付録 A。

#### 必須の防御

- **`pcall` で囲む。** ローカル設定が例外を投げてもアプリは起動し続け、同梱コアのみで動作する。エラーは host へ通知してログに残す。囲まないと、typo 一つでアプリが無言で使えなくなる
- **契約を再宣言する。** `ZZ` / `ZQ` / `:q` 系 abbrev / `VimLeavePre` を、ローカル設定の読み込み後にもう一度張り直す。`VimLeavePre` は augroup を `clear = true` で作り直して二重登録を防ぐ

#### runtimepath

ローカル設定から自前モジュールを `require` できるよう、`stdpath("config")` を runtimepath に加える。

**同梱側を prepend、ローカル側を append すること。** 逆にするとローカル側の `lua/anvi/` が同梱コアを丸ごと隠蔽してしまう。

#### 起動コストについて

**ローカル設定は nvim サーバーの起動時に一度だけ読まれる。セッション毎ではない。** セッション開始時にやっているのは `nvim_buf_set_lines` だけなので、**ローカル設定が重くてもホットキーからの体感速度には影響しない。** アプリ起動が遅くなるだけであり、常駐アプリではほぼ問題にならない。

したがって「ローカル設定に何を書くと遅くなるか」の線引きは起動時コストではなく、**バッファ毎に走る処理があるかどうか**である。

| 影響 | 例 |
|---|---|
| 起動時のみ。許容 | colorscheme、オプション、キーマップ、プラグイン導入そのもの |
| **セッション毎に効く。要注意** | LSP の自動アタッチ、treesitter パース、`BufEnter` / `BufNewFile` 系 autocmd、重い statusline |

#### `--noplugin` との関係

`--noplugin` は元々ユーザーの通常設定を締め出すために入れたが、その役割は既に `NVIM_APPNAME` が担っている。現状の `--noplugin` が実際に止めているのは **このアプリ用ローカル設定配下の `plugin/` ディレクトリの自動読み込み** だけである。

暗黙のロードを避けたいので v1 では維持する。ローカル設定からは明示的に `require` すること。プラグインマネージャを入れたくなったら、その時点で外すか判断する。

### 5.5 host のチャンネル ID の受け渡し

init.lua は起動時点では host のチャンネル ID を知らない。以下の手順で登録する。

1. host が TCP で接続する
2. host が `nvim_get_api_info()` を呼ぶ → 戻り値の第 1 要素が **host 自身のチャンネル ID**
3. host が `nvim_exec_lua("require('anvi').set_host(...)", { chan })` を呼んで登録する

以降 init.lua 側は `vim.rpcnotify(host_chan, ...)` で host に通知できる。

### 5.6 クリップボード連携（nvim の `"+` / `"*` レジスタ）

同梱 nvim は Windows ではクリップボード provider を持たない。nvim は win32yank などの
外部ツールに依存するが、同梱物に exe を増やしたくない。**host 自身を provider の実体にする。**

同梱コアが `vim.g.clipboard` を登録し、`copy` / `paste` の中身は host への
`vim.rpcrequest` にする（実装は付録 A）。host は Win32 のクリップボードを直に叩く
（8.2 の fallback で使っているものと同じコード）。

| 方向 | request | 引数 | 戻り値 |
|---|---|---|---|
| 貼り付け（`"+p`） | `clipboard_get` | なし | `[lines, regtype]` |
| ヤンク（`"+y`） | `clipboard_set` | `[lines]` | なし |

`lines` は行配列、`regtype` は `setreg()` の規約（`v` = 文字指向 / `V` = 行指向）。
**regtype が載るのは取得方向だけである。**

#### コピー方向は regtype を見ない

nvim が `copy` に渡してくる `lines` は、行指向・矩形指向のとき**末尾に空文字列の要素が
付いている**。つまり CRLF で結合するだけで末尾改行が自然に生える。host が regtype を見て
`\r\n` を足すと二重になる（実際に一度やって `"日本語\r\n\r\n"` になった）。
**`clipboard_set` は regtype を受け取らないし、見ない。**

実測（2026-08-06、同梱 nvim）:

| ヤンク | `lines` | `regtype` |
|---|---|---|
| `"+yy`（1 行・行指向） | `{"abc", ""}` | `"V"` |
| `"+yj`（2 行・行指向） | `{"abc", "def", ""}` | `"V"` |
| `"+yw`（文字指向） | `{"abc"}` | `"v"` |
| `<C-v>` で 2 行（矩形指向） | `{"ab", "ef", ""}` | `"b"` |

矩形指向の regtype は `"\x16{幅}"` ではなく **`"b"`** で来る。`setreg()` の規約とは違う。
`"\x16{幅}"` を前提にパースすると矩形ヤンクだけが nvim 側でエラーになる。コピー方向で
regtype を見ない以上どうでもよい事実だが、見ようとすると必ず踏む。

#### 末尾改行が行指向の印（取得方向）

OS のクリップボードには regtype が載らない。**取得方向だけが判定を持つ。**
生テキストが改行で終わっていれば `V`、そうでなければ `v` を返す。返す `lines` に
末尾の空要素は要らない（`{{"L1"}, "V"}` を返せば行指向で貼れることを実測で確認済み）。

**8.4 の「書き戻しでは末尾改行を付けない」とはここだけ意図的に違う。** 8.4 は入力欄との
往復の話で、末尾改行は入力欄が付けてきた終端子でしかない。こちらは他アプリの
クリップボードとの往復の話で、末尾改行は行指向を伝える唯一の手がかりである。だから
コピー方向では nvim が末尾の空要素として付けて渡してくるし、取得方向ではそれを読む。

矩形指向は幅を OS のクリップボードから復元できない。貼り付け時は行指向になる。

#### `cache_enabled = 0`

nvim 側のキャッシュを切る。Windows 側で後からコピーされた内容を必ず読むため。

#### デッドロックの注意

`vim.rpcrequest` は応答が返るまで nvim を待たせる。**host のリクエストハンドラから nvim を
呼び返してはいけない。** Win32 の呼び出しもブロックしうるので `spawn_blocking` に逃がす。

#### `clipboard` オプションは設定しない

`unnamedplus` などは同梱コアで設定しない。オプションはローカル設定の領分（→ 5.4）。
同梱コアは provider を用意するところまでを担う。

---

## 6. セッションのライフサイクル

### 6.1 状態機械（host 側）

```
Idle ──[ホットキー]──> Capturing ──[取得成功]──> Editing
 ↑                          │                      │
 │                          └─[取得失敗]──> Idle    │
 │                                                  │
 └──[フォーカス復帰完了]── Applying <──[session_end]┘
```

- **Idle:** 編集ウィンドウは非表示。ホットキー待ち
- **Capturing:** 対象 HWND を保存し、テキストを取得
- **Editing:** 編集ウィンドウ表示中。この状態でホットキーが押されたら、そのウィンドウにフォーカスを移すだけ
- **Applying:** 書き戻しとフォーカス復帰。一瞬で通過する遷移状態であり、実装上は `session_end` ハンドラ内で同期的に完結してよい（付録 B はそうしている）

対象が取得できなかった場合は何もせず Idle に戻る（要件通り）。

### 6.2 正常フロー

1. ホットキー押下
2. `GetForegroundWindow()` で対象 HWND を保存
3. テキスト取得（→ 8 章）
4. host が `nvim_buf_set_lines` でバッファへ流し込む
5. host が編集ウィンドウを表示、フォーカスを移す
6. ユーザーが編集
7. `:w` 系 → `BufWriteCmd` が発火 → init.lua が host に `session_write`(内容) を通知。host は内容を保持するだけで、この時点では書き戻さない
8. `ZZ` / `:wq` / `:x` → 書き込み後に `session_end`
9. `ZQ` / `:q` / `:q!` → 書き込まずに `session_end`
10. host が編集ウィンドウを隠す
11. host が対象 HWND へフォーカスを復帰
12. このセッションで `session_write` を一度でも受信しており、かつ最後に保存された内容が取得時と異なれば書き戻す（→ 9 章）

### 6.3 quit の乗っ取りと安全網

`:q` 系を nvim に届かせず、host への通知に差し替える（実装は付録 A）。

**これは網羅的ではない。** `:quit` / `:qa!` / `:xa` などは抜ける。個人ツールなので、抜けたら都度追加すればよい。完全性より「明日から使える」ことを優先する。

抜けた場合、および nvim がクラッシュした場合に備え、**必ず安全網を持つこと。** 検知は二本立てにする。

- host 側: RPC チャンネルの切断を検知する（**これを正とする**）
- init.lua 側: `VimLeavePre` で host に `nvim_dying` を通知する。ただし終了処理中の `rpcnotify` はフラッシュされない可能性があるため、**早期ヒント以上の役割を与えないこと**

いずれかを検知したら、**host は nvim を再起動して `ui_attach` をやり直し、状態を Idle にリセットする。** 4.3 で却下した「毎回再起動」案を、通常経路ではなく異常系のリカバリとして使う形になる。これにより「host がセッション中の状態で固まる」ことを防げる。編集ウィンドウは host 自身のものなので作り直さない ── ただし古いグリッドが残らないよう、再起動のついでに隠す。

**誤発火に注意:** トレイの Exit と Windows のセッション終了は、通常のクラッシュ復旧と分離する。共有 `SpawnPolicy` は可逆な終了照会と不可逆なアプリ終了を別ビットで持つ。

- 編集 HWND のサブクラスで `WM_QUERYENDSESSION` を直接受け、起動を一時抑止して即座に TRUE を返す。既存の子の終了や RPC 待ちは行わない。同期通知は winit のキューメッセージフックを通らないため、`with_msg_hook` では代用しない。
- `WM_ENDSESSION(FALSE)` は照会だけを解除する。抑止中に子が失われていれば保持した復旧要求を再開する。`TRUE` とトレイの Exit は不可逆な終了とし、後続の FALSE でも解除しない。
- 初回起動も HWND のフック設置後に行う。旧子の終了待ち後、各プロセス生成の直前・直後、接続・握手・UI attach の待機中にも抑止を確認する。`SM_SHUTTINGDOWN` は親への通知より先に子が終了した場合の補助判定とする。
- 終了照会では既存の編集 RPC / UIA 応答待ちは中断しない。終了確定時はそれらの待機を中断し、子の終了待ちは 1 秒、tokio ランタイムの終了待ちは 1 秒を上限にする。OS 内で停止した Win32 呼び出しまで時間保証するものではない。
- 状態確認と `CreateProcess` は OS の終了開始に対して原子的ではない。生成と競合した場合は直後の確認で子を破棄する。実際のシャットダウン時のエラーダイアログ消失は、疑似通知だけでは検証できない。

### 6.4 バッファ

- セッション毎に新規バッファを作る（`nvim_create_buf(false, true)`）
- `buftype=acwrite` を設定し、`BufWriteCmd` を張る
- 古いバッファは wipe する（API では `nvim_buf_delete`。`:bwipeout` 相当）
- `BufWriteCmd` の中では `vim.bo.modified = false` を自分で設定すること（`acwrite` では自動で降りない）

---

## 7. 編集ウィンドウ

ウィンドウは host 自身のものになった。探索も生存監視も要らず、Win32 を直に叩くのは前面化（7.3）だけ。

### 7.1 生成

winit で作る。`visible=false` / `active=false` / `skip_taskbar=true` で、**作った時点では画面に出ないしフォーカスも奪わない**。v1 の「画面外へ飛ばしてから隠す」待避は不要になった。

**タイトルバーは出さない**（`decorations=false`）。編集対象に重ねて出し `ZZ` / `ZQ` で閉じる 1 枚窓であり、閉じる・最大化のボタンにも枠にも役割が無い。

#### 透過（背景のみ）・枠線・余白

見た目の定数は 3 つとも `render.rs` の先頭にある。

| 定数 | 値 | 何 |
|---|---|---|
| `BACKGROUND_ALPHA` | 0.75 | 背景の不透明度 |
| `PADDING` | 8（論理 px） | グリッドの周囲の余白 |
| `BORDER_WIDTH` / `BORDER_ALPHA` | 1（論理 px） / 0.45 | 枠線の太さと不透明度。色は既定の前景色 |

**透かすのは背景だけで、文字・カーソル・preedit は不透明のまま**にする。下に何が来るか分からない以上、文字まで透かすと読めなくなる。枠線を引くのも同じ理由で、縁が無いと透けた背景が下のアプリと地続きに見える。

余白は変換行列 1 枚（`SetTransform`）でずらして作る。セル座標の計算に余白を混ぜると全ての描画関数が余白を知る羽目になるため。`Clear` は変換の影響を受けないので余白も背景色で埋まり、枠線だけ恒等変換に戻してから引く。余白のぶんウィンドウは広がる（行列数は減らさない → `window::grid_for` / `resize_to_grid` が `pad` を受け取る）。**IME の候補ウィンドウ位置と preedit の右端打ち切りも余白のぶんずれる**ので、両方に同じ `pad` を渡すこと。

そのために出力経路が HWND 直付けではない。`ID2D1HwndRenderTarget` は `D2D1_ALPHA_MODE_IGNORE` しか取れず、**アルファを持てない**。

```
D3D11 デバイス → D2D デバイスコンテキスト → 合成用スワップチェーン
  → IDCompositionVisual → IDCompositionTarget(HWND)
```

付随する掟が 3 つある。

- ウィンドウは `WS_EX_NOREDIRECTIONBITMAP`（winit の `with_no_redirection_bitmap`）。リダイレクションサーフェスが残っていると、そこが不透明に塗られて背後が透けない
- `IDCompositionTarget` を**手放さない**。落とすと合成が外れ、描いたものが 1 ドットも出なくなる
- 背景の塗りだけ `D2D1_PRIMITIVE_BLEND_COPY`。既定の source-over だと `Clear` の 75% の上に 75% を重ねて 94% になり、セルごとに透け方が変わる

セル寸法は DirectWrite に実測させる（`IDWriteFontFace::GetMetrics` と `'M'` の `GetDesignGlyphMetrics`）。ウィンドウはレンダーターゲットより先に要るので、暫定サイズで作ってから実測値で `request_inner_size` し直す。

#### セルの空文字列は「全角の続き」専用

`grid_line` は全角文字を「本体セル + 空文字列セル」で送る。描画側はこの空文字列を
目印に、カーソルが続きセルへ乗ったとき本体セルへ寄せる。

したがって **未描画・消去済みのセルを空文字列にしてはならない**（`Cell::BLANK` =
空白 1 文字）。nvim は `grid_clear` / `grid_resize` のあと空白セルを送り直さないので、
既定値が空文字列だと行末の未描画セルが「全角の続き」に化け、**入力中のカーソルが
1 セル左へずれる**（v0.2.0 で実際に出た）。

### 7.2 表示 / 非表示

| タイミング | 操作 |
|---|---|
| 起動時 | 生成時点で非表示。何も見えない |
| 表示時 | 対象ウィンドウに重ねて移動（→ 位置決め）→ `set_visible(true)` → フォーカス（→ 7.3） |
| 終了時 | `set_visible(false)` → 対象アプリへフォーカス復帰 |

ウィンドウの × はウィンドウを壊さず、`AnviQuit`（= `ZQ`、破棄）として nvim へ流す。セッション外の × は無視する。

#### 位置決め

編集ウィンドウは**編集対象と同じモニタで、対象ウィンドウの中心に重ねて**出す。目線とマウスがある場所へ出すのが編集の始まりであり、別のモニタや対象から離れた場所に出てはならない。

- 対象は UIA が返す入力欄の HWND だが、矩形もモニタも `GA_ROOT` を解決したトップレベルウィンドウで見る（7.3 と同じ理由。UIA が返すのは子コントロール）
- 載っているモニタは `MonitorFromWindow(MONITOR_DEFAULTTONEAREST)`、置ける範囲は `GetMonitorInfoW` の **`rcWork`**（タスクバーの下に潜らない）
- 対象矩形の中心へ編集ウィンドウの中心を合わせ、`rcWork` からはみ出す分は中へ押し戻す。`rcWork` より大きい辺は左上端へ寄せる（はみ出しは右下へ出す）
- 幾何計算は `window::place_over` の純関数で、Win32 の問い合わせと分離してテストしてある。座標はセカンダリモニタで負になり、仮想スクリーンの端で i32 を溢れうるので加減算は `saturating_*`
- 位置決めに失敗したらログ（`error`）を残して**表示は続ける**。決め打ちの位置へ落とす代替経路は持たない

既存セッションへ戻る（Idle でないときのホットキー、7.3 のフォーカスだけの経路）ではウィンドウを動かさない。編集中に位置が跳ねるほうが害が大きい。

### 7.3 フォーカス復帰

Windows には `SetForegroundWindow` の呼び出し制限があるため、単純に呼んでも効かないことがある。winit の `Window::focus_window()` は素で `SetForegroundWindow` を呼ぶだけなので使わない。

- **現在の前面ウィンドウのスレッド**へ `AttachThreadInput` してから `SetForegroundWindow` を呼ぶ（対象スレッドへアタッチする版は実機で拒否された）
- `AllowSetForegroundWindow` を併用し、最後に `GetForegroundWindow` で結果を検証する
- UIA が返すのは子コントロールの HWND なので、必ず `GA_ROOT` を解決してから扱う

### 7.4 IME（このアプリの主目的）

winit は既にインライン IME として振る舞う（→ 4.2）。host がやるのは次の 4 つ。

- **有効・無効の切り替え:** `mode_change` を見て、挿入 / 置換 / 端末 / 選択モードのときだけ `set_ime_allowed(true)`。ノーマル / コマンドラインモードでは OS IME を無効にし、nvim 本来の入力挙動に合わせる
- **未確定文字列を描く:** `Ime::Preedit` は nvim へ送らず、カーソル位置へ自前で重ねて描く。全体に細い下線、`target`（変換対象クラスタ）は反転 + 太い下線
- **候補ウィンドウの位置:** カーソルセルの矩形を `set_ime_cursor_area` で渡す
- **確定文字列だけを流す:** `Ime::Commit` を `nvim_input` へ（`<` は `<lt>` へ逃がす）。`nvim_paste` は使わない ── モードに依らず一貫させたいのと、`vim.paste` と acwrite バッファの契約を干渉させないため

変換中（composition 中）のキーイベントは nvim へ流さない。IME が食ったキーが二重に入るのを防ぐ。

---

## 8. テキストの取得

### 8.1 経路

`IUIAutomation::GetFocusedElement()` でフォーカス中の要素を取得し、UIA は次の適格性だけを
判定する。

| UIA で確認するもの | 条件 |
|---|---|
| ControlType | `Edit`、`Document`、または下記の ValuePattern 条件も満たす `ComboBox` |
| キーボード入力 | `IsKeyboardFocusable == true` |
| ValuePattern（`ComboBox` のみ） | pattern が存在し、`CurrentIsReadOnly == false` |
| 復帰先 | 対象自身または前面 root window の HWND を特定できる |

書き込み可能な ValuePattern を持つだけの任意の ControlType は対象にしない。条件を満たした対象は、
アプリや `FrameworkId` にかかわらず `Ctrl+A` → `Ctrl+C` → `CF_UNICODETEXT` 読み取りの
1 経路だけで取得する。`ValuePattern.CurrentValue` と `TextPattern.DocumentRange` は読まない。
UIA provider が返す文字列が nvim バッファへ入る経路は存在しない。

Zen Browser / Gecko のアドレス・検索欄は、キーボードフォーカス可能で書き込み可能な
ValuePattern を持つ `ComboBox` としてこの適格性を満たす。通常の Edit、Chromium / Electron の
`contenteditable`、Slack のリッチテキスト入力欄と同じクリップボード経路で扱う。扱う内容は
常にプレーンテキストであり、書式は保持しない（→ 10.3）。

### 8.2 クリップボード取得の注意

- **送信前に物理修飾キーの解放を待つこと。** ホットキーの修飾キー（Ctrl / Shift / Alt / Win）はこの時点でまだ押されている。そこに `Ctrl+A` を注入すると対象には `Ctrl+Shift+A` 等が届く。`GetAsyncKeyState` で全修飾キーが離れるまで待ってから注入する
- 送信前に既存のクリップボード内容を退避する
- `GetClipboardSequenceNumber()` の変化を監視して、コピー完了を待つ（固定 sleep は避ける）
- 空の入力欄では `Ctrl+C` してもクリップボードが更新されず、sequence number は変わらない。タイムアウトの扱いは 8.3 参照
- 読み取り後、退避した内容を復元する
- **復元は best effort。** 画像 + HTML + text の複数フォーマットを持つデータや、遅延レンダリング形式のデータは原理的に完全復元できない
- `Ctrl+A` が別の動作に割り当てられているアプリでは事故る

### 8.3 取得できなかった場合

フォーカス要素、HWND、または編集可能性を UIA で確認できなかった場合は、キーを一切注入せず
Idle に戻る。エラー通知も不要（要件「編集対象が存在しない場合は何もしない」）。

編集対象と確認した後のコピータイムアウトは「取得失敗」と「空の入力欄」を区別できないため、
**空文字として続行**する。空欄から書き始める用途（チャットの新規作文など）を殺さないためである。
sequence number が変化したのに `CF_UNICODETEXT` が無い場合は、壊れた内容で編集を始めず
取得エラーにする。

### 8.4 改行コードの正規化

Windows アプリからクリップボードで取得するテキストは `\r\n` / `\n` / `\r` が混在しうる。

- 取得時: 3 種類のいずれも行区切りとして分割し、内部表現は常に行配列（nvim バッファの行）に正規化する
- **末尾の改行は「終端子」として扱い、空行にしない。** `"abc\r\n"` は `["abc"]` とする。落とすのは末尾 1 つだけなので、意図的な末尾の空行（`"a\n\n"` → `["a", ""]`）は保たれる
- 書き戻し時: `\r\n` で結合する。クリップボード（`CF_UNICODETEXT`）の慣習は `\r\n` であり、旧来の EDIT コントロールは裸の `\n` を正しく表示できない。**末尾には改行を付けない**（取得時に終端子として落としているため、往復は末尾改行 1 つ分だけ非対称になる。これは意図的）

---

## 9. テキストの書き戻し

### 9.1 経路

| 取得 | 書き戻し |
|---|---|
| `Ctrl+A` → `Ctrl+C` → `CF_UNICODETEXT` | `Ctrl+A` → `Ctrl+V` |

適格性を確認済みのすべての対象でこの 1 経路だけを使う。ValuePattern は `ComboBox` の
適格性判定にだけ使い、取得・書き戻しではアプリ、`FrameworkId`、UIA pattern による分岐をせず、
`SetValue` も使わない。UIA の役割は編集可能性、復帰先 HWND、取得時と書き戻し時の対象
identity の安全確認に限る。

取得時には `IUIAutomationElement`、RuntimeId（provider が返す場合）、HWND を UIA スレッド内に
保持する。プレーンな行配列と HWND だけがスレッド外へ出る。

### 9.2 書き戻し先の安全確認

controller が編集ウィンドウを隠して取得時の HWND を前面へ戻した後、UIA で
`GetFocusedElement()` を取り直す。次のすべてを満たさない限り、クリップボード変更も
キー注入も行わない。

1. フォーカス要素が 8.1 と同じ適格性（ControlType、キーボードフォーカス、`ComboBox` の ValuePattern）を満たす
2. 取得時に RuntimeId が得られた場合は同じ RuntimeId。RuntimeId は COM wrapper ではなく
   論理要素の identity なので、正規な provider が同じ論理要素の wrapper を再作成しても一致する
3. RuntimeId を公開しない provider では `IUIAutomation::CompareElements` が真
4. キー注入の直前に、取得時 HWND と現在の前面ウィンドウの root HWND が一致する

対象アプリが入力欄そのものを破棄・再作成して RuntimeId が変わった場合は、見た目が同じでも
別要素として書き戻しを中止する。別の入力欄へ全文貼り付けするより、編集結果を適用できない方を
安全側とする。

### 9.3 クリップボード貼り付け

1. 編集ウィンドウを隠し、取得時の対象ウィンドウへフォーカスを戻す
2. 9.2 の UIA identity と root HWND を確認する
3. 既存のクリップボード内容を退避する
4. 編集後テキストを `\r\n` で結合し、`CF_UNICODETEXT` としてクリップボードへ置く（→ 8.4）
5. **物理修飾キーの解放を待つ**（`ZZ` の Shift が押されたままだと `Ctrl+V` が `Ctrl+Shift+V` に化ける。8.2 と同じ処理）
6. `Ctrl+A`（フォーカス復帰で取得時の選択範囲は失われているため、必ず全体を選択し直す）
7. `Ctrl+V`
8. 貼り付けの取り込みを短時間待ってからクリップボードを復元する（復元は best effort）

### 9.4 変更がない場合

編集前後で内容が一致する場合は書き戻しをスキップする。相手アプリの undo 履歴を無駄に汚さないため。

### 9.5 UIA provider のテキストを使わない理由

UIA provider の文字列は対象によって空行や placeholder を含む表現が安定しないため、
`ValuePattern.CurrentValue` と `TextPattern.DocumentRange` は対象アプリにかかわらず使わない。
provider ごとの例外や fallback を作らず、コピー結果だけを正とする。

Chromium の `contenteditable` は 1 行につき 1 ブロック（`<div>` / `<p>`）で構成されることが
多く、空行はテキストを持たないブロック（`<div><br></div>`）になる。Chrome で
`<div>dvhoge</div><div><br></div><div>dvmoge</div>` を取得した実測では次の差が出る。

| 取得手段 | 得られたテキスト |
|---|---|
| `ValuePattern.CurrentValue` | `"dvhoge\ndvmoge"` ❌ 空行が消える |
| `TextPattern.DocumentRange` | `"dvhoge\ndvmoge"` ❌ 空行が消える |
| `Ctrl+A` → `Ctrl+C` | `"dvhoge\r\n\r\ndvmoge"` ✅ |

Slack の `contenteditable` は書き込み可能な `ValuePattern` を持つが、`SetValue` は改行ごとに
段落を割り、`"AAA\r\nBBB"` を `AAA\n\nBBB` に変える。一方、`Ctrl+V` は `AAA\nBBB` を保つ。
この差は改行コードではなく経路の問題なので、結合は常に `\r\n` のままにする。

コピーと貼り付けなら Chromium / Electron / Slack の空行を含む内容を往復できる。
ただし扱うのはプレーンテキストだけで、元の太字、リンク、色などの書式は平文化する（→ 10.3）。

---

## 10. 既知の限界

**実装前に必ず目を通すこと。** ここに書いてあるものは「バグ」ではなく仕様上の制約。

### 10.1 Electron 系アプリ（Slack / Discord など）

UIA がフォーカス要素を 8.1 の適格な編集対象として公開する `contenteditable` は、Slack を
含めて内容を編集できる。取得も書き戻しも通常の入力欄と同じコピー / 貼り付け経路であり、
アプリや `FrameworkId` による特別扱いはない。

サポートするのはプレーンテキストとしての内容編集である。元のリッチテキスト書式は保持しない
（→ 10.3）。またクリップボードを一時的に占有する（→ 10.4）。

### 10.2 改行が送信になるアプリ

貼り付けた改行を本文ではなく送信操作として解釈するアプリでは、複数行テキストが分割送信される
可能性がある。これは対象アプリ固有の貼り付け意味論であり、anvi は送信操作を判別できない。

### 10.3 リッチテキストの平文化

リッチテキスト入力欄も内容編集の対象である。ただし取得時に `CF_UNICODETEXT` だけを読み、
書き戻し時も `CF_UNICODETEXT` だけを設定するため、太字、斜体、リンク、色、埋め込みなどの
書式は失われ、入力欄全体がプレーンテキストになる。書式保持はサポート範囲外。

### 10.4 クリップボードの一時的な占有

すべての取得と書き戻しでクリップボードを一時的に占有する。元の内容の復元は best effort
（→ 8.2）であり、複数フォーマットや遅延レンダリングを完全には戻せない場合がある。

### 10.5 範囲選択して一部だけ編集

v1 では非対応。常に入力欄の全内容が対象。

### 10.6 セキュリティソフト

同梱 exe を除外設定に入れないと、起動遅延やファイルアクセス拒否が発生しうる。

### 10.7 ローカル設定は契約を壊しうる

`pcall` と契約の再宣言で大半は防げるが、ローカル設定が `vim.schedule` / `VimEnter` 等で遅延実行すれば `ZZ` や abbrev を潰せる（→ 4.7）。

その場合の着地点は「素の `:q!` が nvim に届き、host が切断を検知してペアを再起動する」であり、編集内容が失われる以上の被害はない。**設定を書くのは自分自身なので、これ以上の防御はしない。**

### 10.8 自前 UI が持たないもの

- **マウス操作。** クリックもホイールも nvim へ送っていない（`nvim_input_mouse` を呼ばない）。キーボードだけで完結するツールなので必要になるまで入れない
- **`ext_multigrid` / `ext_cmdline` / `ext_popupmenu` / `ext_messages`。** cmdline も補完メニューも浮動ウィンドウも、nvim にグリッドへ描かせる。GUI 側で作り直す価値がない
- **フォントの太字・斜体はファミリ内の合成のみ。** `guifont` の `:b` / `:i` のような装飾オプションは受け付けない（解けない指定は現状維持 + 警告）
- **`undercurl` は点線で描く。** 波線は諦めた
- **合字・絵文字は等幅グリッドから外れうる。** 等幅前提でセルへ割り付けているので、送り幅が違うグリフは桁がずれる

---

## 11. 実装スタック

### 11.1 クレート

| 用途 | クレート |
|---|---|
| グローバルホットキー | `global-hotkey` |
| トレイアイコン | `tray-icon` |
| nvim RPC | `nvim-rs` |
| ウィンドウ / キーボード / IME | `winit`（+ `raw-window-handle` で HWND を取り出す） |
| 描画 / Win32 / UI Automation | `windows`（Direct2D・DirectWrite・D3D11・DXGI・DirectComposition。IMM32 は winit 側） |
| クリップボード | Win32 直叩き（8.2 の fallback と 5.6 の nvim レジスタ連携の両方で使う） |
| exe リソース（アイコン / バージョン情報） | `winresource`（build-dependencies） |

### 11.2 スレッドモデル（重要）

COM のアパートメント地雷を踏まないこと。

- **UI Automation は MTA で回す**
- **main スレッドは winit のイベントループが占有する。** 描画・キーボード・IME に加え、トレイとグローバルホットキーの隠しウィンドウのメッセージもここで汲まれる（どちらも「登録したスレッドでループが回っていること」を要求するので、イベントループを回すスレッドで作る）
- **`Session` と nvim の RPC は controller スレッドだけが触る。** GUI からは `Cmd`、GUI へは `EventLoopProxy` の `UserEvent`

これを混ぜると「特定のアプリでのみ、なぜかハングする」という原因究明が極めて困難な不具合が発生する。最初から分けること。

### 11.3 コンソールとログ

host は **GUI サブシステム**（`#![windows_subsystem = "windows"]`）。常駐ツールが起動の
たびにコンソールウィンドウを開いては使い物にならない。結果として次の 2 つが要る。

- ログは stderr ではなく `%LOCALAPPDATA%\anvi-data\log\anvi.log` へ書く。起動時に
  前回分を `anvi.log.1` へ置き換え退避し、今回分を空で作る。前回の 1 世代だけを保持し、
  PC 終了時の記録を次回起動で失わないようにする
- nvim の子プロセスは `CREATE_NO_WINDOW` で起動する。コンソールを持たない親から
  コンソールアプリを起動すると、**子が自分でコンソールウィンドウを開く**

### 11.4 トレイメニュー

`サインイン時に起動`（チェック）と `Exit` の 2 項目。チェックの実体は
`HKCU\...\CurrentVersion\Run` の `anvi` 値で、**インストーラの `startup` タスクと同じ
エントリ**（→ 13.1）。portable / scoop にはインストーラが無いので、ここが唯一の入口になる。

`muda` のイベントハンドラは `Fn + Send` を要求するためメニュー項目そのものを掴めない。
ハンドラは `UserEvent::ToggleAutostart` を投げるだけにして、レジストリ操作と
（失敗時の）チェック戻しはイベントループのスレッドで `Tray` が行う。

---

## 12. 実装順序

**上から順に進める。前のステップが通るまで次に進まない。**

### ステップ 1 — 技術的不確実性の解消（最優先）

UI もホットキーも UIA も一切実装しない。host はコンソールアプリで良い。

**受け入れ条件:**

1. host が nvim を `--headless --listen` で起動し、RPC 接続できる
2. `nvim_buf_set_lines` で任意の文字列をバッファへ流し込める
3. UI クライアントを手動でアタッチし、表示して編集できる
4. `:wq` → host のコンソールに編集後テキストが出力される
5. `:w` のあと `:q` → 保存済みの内容が host に反映される（状態契約の確認）
6. 一度も保存せず `:q!` → host が破棄し、**nvim プロセスが生きたまま**である

**6 が最も怪しい。ここを最初に通すこと。** これが通れば、このプロジェクトの技術的リスクはほぼ消滅する。

### ステップ 2 — 常駐化と表示制御

- トレイアイコン
- グローバルホットキー
- 編集ウィンドウの生成（非表示のまま）
- ホットキーでの表示 / セッション終了時の非表示
- 編集中にホットキーが押されたら編集ウィンドウにフォーカス

### ステップ 3 — UIA 経路

- `GetFocusedElement` → `ValuePattern` で取得
- `SetValue` で書き戻し
- メモ帳、Windows のネイティブ入力欄、ブラウザの `<input>` などで動作確認

### ステップ 4 — クリップボード fallback

- UIA が空振りした場合の `Ctrl+A` / `Ctrl+C` 経路
- クリップボード退避と復元
- `GetClipboardSequenceNumber` による完了待ち
- 物理修飾キーの解放待ち（→ 8.2 / 9.3）
- 改行コードの正規化（→ 8.4）
- 空欄タイムアウトの扱い（→ 8.3）

### ステップ 5 — フォーカス復帰の泥仕事

- `AttachThreadInput` / `AllowSetForegroundWindow`
- 実アプリでの検証。ここは試行錯誤になる

### ステップ 6 — ローカル設定の読み込み

同梱コアが安定してから入れる。実装量は小さい（→ 付録 A-1）。

- `$XDG_CONFIG_HOME/anvi/init.lua` があれば `pcall` で読む
- 読み込み後に `enforce_contract()` を呼ぶ
- 検証: ローカル設定で `ZZ` を潰す → 再宣言で戻ることを確認
- 検証: ローカル設定に文法エラーを仕込む → アプリが起動し、host に `init_error` が届くことを確認

### ステップ 7 — 実際に使い、不便を潰す

見た目やオプションはローカル設定へ。同梱コアに追加するのは、契約に関わるものだけに留める。

### ステップ 8 — 自前 UI（実施済み）

Neovide の IME が使い物にならなかったため実施（→ 4.2）。順序は core の状態モデル（`ui`）→ キー記法（`ui::input`）→ Direct2D レンダラ → winit シェルと host 統合。**IME は最後の仕上げではなく、設計の出発点に置くこと。**

---

## 13. 配布

配布物は 2 つ。中身（`stage/anvi`）は完全に同一で、包み方だけが違う。

| 形式 | 資産名 | 用途 |
|---|---|---|
| portable zip | `anvi-vX.Y.Z-windows-x64-portable.zip` | 展開して置くだけ。scoop もこれを使う |
| インストーラ | `anvi-vX.Y.Z-windows-x64-setup.exe` | 自動起動・スタートメニュー・アンインストーラが要る場合 |

どちらもタグ push（`v[0-9]+.[0-9]+.[0-9]+*`）で `release.yml` が作る。ワークスペースの
version とタグが食い違っていれば `verify-version` がその場で落とす。

### 13.1 インストーラ（`installer/anvi.iss`, Inno Setup 6）

- インストール先は `%LOCALAPPDATA%\Programs\anvi`。**管理者権限を要求しない**
  （`PrivilegesRequired=lowest`）。個人ツールに UAC を挟む理由が無い
- タスク `startup`（既定 ON）が `HKCU\...\CurrentVersion\Run` に登録する。常駐しない
  ホットキーツールは使い物にならないので既定は ON、ウィザードで外せる
- **上書きインストール前に `taskkill /F /T /IM anvi.exe` で常駐を落とす。**
  `×` はセッションの破棄であってアプリの終了ではない（→ 7.2）ため、再起動マネージャに
  任せると閉じられずに止まる。`CloseApplications=no` はそのための明示指定
- アンインストールは既定でユーザーデータ（`%LOCALAPPDATA%\anvi` のローカル設定と
  `%LOCALAPPDATA%\anvi-data` の shada/state）も消す。`/KEEPDATA` を付けると残す。
  対話時は MsgBox で確認する（`/SUPPRESSMSGBOXES` は `MsgBox` を抑止しないので、
  MsgBox は必ず `UninstallSilent` が偽の枝にだけ置くこと）
- インストール先（`Programs\anvi`）とローカル設定（`anvi`）は別物。
  前者にはアンインストーラ以外触らない
- ウィザードのアイコンは `assets/anvi.ico`、ライセンスページは `LICENSE-MIT`
  （Apache-2.0 はインストール先の `LICENSE-APACHE`。→ 13.3）

release.yml は毎回サイレントで「インストール → 配置と Run 値の確認 → `/KEEPDATA` で
アンインストールしてデータが残ることの確認 → 再インストール → 既定でアンインストールして
全部消えることの確認」まで回す。`unins000.exe` は `%TEMP%` へ自分を複製して即座に戻るので、
削除完了はポーリングで待つ。

### 13.2 scoop（個人 bucket）

`sg004baa/scoop-bucket` の `bucket/anvi.json`。portable zip を `extract_dir`
`anvi` で展開し、`bin` と `shortcuts` に `anvi.exe` を出す。
`checkver: github` + `autoupdate`（hash は `$url.sha256`）で追従する。

scoop 経路では自動起動もアンインストール時のデータ削除も行われない。ユーザーデータは
アプリディレクトリの外（`%LOCALAPPDATA%\anvi*`）にあるので、更新でも
`scoop uninstall` でも消えない。消したければ手で消す。

### 13.3 ライセンス

本体は `MIT OR Apache-2.0`（`LICENSE-MIT` / `LICENSE-APACHE`）。再配布物のライセンスは
配布物と一緒に置く（→ 5.3）。

| 再配布するもの | ライセンス | 置き場所 |
|---|---|---|
| anvi 本体 | MIT OR Apache-2.0 | `LICENSE-MIT` / `LICENSE-APACHE` |
| Moralerspace Argon HW（exe に埋め込み） | SIL Open Font License 1.1 | `LICENSE-Moralerspace.txt` |
| Neovim | Apache-2.0 | `nvim/LICENSE.txt`（release.yml が同じタグから取る） |

---

## 付録 A: init.lua の骨組み

### A-1. 同梱 init.lua（エントリポイント）

`-u` で指定されるファイル。ここが読み込み順の全てを決める（→ 4.7 / 5.4）。

```lua
-- <bundled>/init.lua

-- 同梱ディレクトリは prepend（ローカル側に lua/anvi/ があっても隠蔽されないよう先に置く）
local bundle = vim.fs.dirname(debug.getinfo(1, "S").source:sub(2))
vim.opt.runtimepath:prepend(bundle)

local aw = require("anvi")

-- 1. 同梱コア。契約を確立する
aw.setup()

-- 2. ローカル設定（任意）。壊れていても起動を止めない。
--    場所を決めるのは nvim であってこのアプリではない
local cfg_dir  = vim.fn.stdpath("config")            -- = $XDG_CONFIG_HOME/anvi
local cfg_file = vim.fs.joinpath(cfg_dir, "init.lua")
local found    = vim.uv.fs_stat(cfg_file) ~= nil

aw.report_config(cfg_dir, found)                     -- 効かないときの一次情報

if found then
  vim.opt.runtimepath:append(cfg_dir)                -- ローカル側は append
  local ok, err = pcall(dofile, cfg_file)
  if not ok then
    -- 起動は続行する。原因が分かるよう host には必ず伝える
    aw.report_error("user_config_error", tostring(err))
  end
end

-- 3. 契約の再宣言。ローカル設定に潰されていても戻す
aw.enforce_contract()
```

> `vim.uv` / `vim.fs.joinpath` は Neovim 0.10+。同梱する nvim のバージョンを下げる場合は
> `vim.loop` / 手動のパス結合に読み替えること。

### A-2. コアモジュール

```lua
-- <bundled>/lua/anvi/init.lua

local M = {
  host = nil,  -- host の RPC チャンネル ID
  buf  = nil,  -- 現在のセッションバッファ
}

local function notify(event, payload)
  if M.host then
    vim.rpcnotify(M.host, event, payload)
  end
end

--- 起動時の報告。この時点ではまだ host が接続していないので溜めておく。
--- `{ event, payload }` の並びで、届いた順にそのまま流す
M.pending = {}

function M.report_error(kind, msg)
  table.insert(M.pending, { event = "init_error", payload = { kind = kind, message = msg } })
end

--- ローカル設定をどこに探しに行ったか
function M.report_config(dir, loaded)
  table.insert(M.pending, { event = "config_resolved", payload = { dir = dir, loaded = loaded } })
end

--- host が接続時に呼ぶ
function M.set_host(chan)
  M.host = chan
  for _, e in ipairs(M.pending) do
    notify(e.event, e.payload)  -- host 側でログに出す
  end
  M.pending = {}
end

--- host がセッション開始時に呼ぶ
function M.start_session(lines, filetype)
  -- 古いバッファを破棄
  if M.buf and vim.api.nvim_buf_is_valid(M.buf) then
    vim.api.nvim_buf_delete(M.buf, { force = true })
  end

  local buf = vim.api.nvim_create_buf(false, true)
  vim.api.nvim_buf_set_lines(buf, 0, -1, false, lines)
  vim.bo[buf].buftype  = "acwrite"
  vim.bo[buf].filetype = filetype or ""
  vim.bo[buf].modified = false
  vim.api.nvim_buf_set_name(buf, "anvi://edit")

  -- :w を乗っ取る。ディスクには書かない
  vim.api.nvim_create_autocmd("BufWriteCmd", {
    buffer = buf,
    callback = function()
      notify("session_write", vim.api.nvim_buf_get_lines(buf, 0, -1, false))
      vim.bo[buf].modified = false  -- acwrite では自分で降ろす必要がある
    end,
  })

  vim.api.nvim_set_current_buf(buf)
  M.buf = buf
  return buf
end

--- セッション終了（nvim は死なない）。反映するかは host が「保存を受信したか」で決める
local function finish()
  notify("session_end", nil)
end

--- 同梱コア。ローカル設定より前に一度だけ呼ぶ
function M.setup()
  -- bang = true は必須。`:q!` は abbrev 展開で「AnviQuit!」の形になるため
  vim.api.nvim_create_user_command("AnviWriteQuit", function()
    vim.cmd("write")   -- BufWriteCmd 経由で内容が host に渡る
    finish()
  end, { bang = true })

  vim.api.nvim_create_user_command("AnviQuit", function()
    finish()
  end, { bang = true })

  -- OS クリップボード（同梱 nvim は provider を持たないので host に代行させる）
  local function host_channel()
    if not M.host then error("anvi: host チャンネルが未登録のためクリップボードを使えない") end
    return M.host
  end
  local function paste() return vim.rpcrequest(host_channel(), "clipboard_get") end
  -- regtype は使わない。行指向なら lines の末尾に空文字列が入って渡ってくる（→ 5.6）
  local function copy(lines, _regtype) vim.rpcrequest(host_channel(), "clipboard_set", lines) end
  vim.g.clipboard = {
    name = "anvi",
    copy = { ["+"] = copy, ["*"] = copy },
    paste = { ["+"] = paste, ["*"] = paste },
    cache_enabled = 0,
  }

  -- 見た目やオプションはここに入れない。ローカル設定の領分（→ 5.4）
end

--- 契約の再宣言。ローカル設定を読み込んだ後に呼ぶ（→ 4.7）
--- 順序に対して脆いのはここに集約されている 3 つだけ
function M.enforce_contract()
  -- 1. キーマップ
  vim.keymap.set("n", "ZZ", "<Cmd>AnviWriteQuit<CR>")
  vim.keymap.set("n", "ZQ", "<Cmd>AnviQuit<CR>")

  -- 2. :q 系の乗っ取り（網羅的ではない。抜けたら追加する）
  local function abbr(lhs, rhs)
    vim.cmd(([[cnoreabbrev <expr> %s (getcmdtype()==#':' && getcmdline()==#%q) ? %q : %q]])
      :format(lhs, lhs, rhs, lhs))
  end

  abbr("q",  "AnviQuit")
  abbr("q!", "AnviQuit")
  abbr("wq", "AnviWriteQuit")
  abbr("x",  "AnviWriteQuit")

  -- 3. 安全網: 乗っ取りを抜けて本当に終了しようとした場合
  --    clear = true で二重登録を防ぐ（再宣言されうるため）
  local grp = vim.api.nvim_create_augroup("AnviContract", { clear = true })
  vim.api.nvim_create_autocmd("VimLeavePre", {
    group = grp,
    callback = function()
      notify("nvim_dying", nil)
    end,
  })
end

return M
```

> 再宣言でも防げないケース（ローカル設定が `vim.schedule` 等で後から潰す）は残るが、
> その場合も 6.3 の安全網が受け止める。データ破損ではなく機能低下として着地する。

---

## 付録 B: host 側の骨組み（擬似コード）

```rust
// 起動シーケンス
let port = pick_free_port();  // TOCTOU: nvim の bind 失敗時は取り直して再試行（→ 4.6）

let nvim_child = Command::new(bundled("nvim.exe"))
    .env("NVIM_APPNAME", "anvi")
    .args(["--headless", "--listen", &format!("127.0.0.1:{port}")])
    .args(["-u", &bundled("init.lua"), "--noplugin"])
    .spawn()?;

let nvim = connect_tcp_with_retry(port).await?;  // nvim-rs。nvim が listen するまでラグがあるためリトライ（→ 4.6）

// 自分のチャンネル ID を取得して init.lua に登録させる
let (chan, _api) = nvim.get_api_info().await?;
nvim.exec_lua("require('anvi').set_host(...)", vec![chan.into()]).await?;

// UI クライアントとしても同じ接続を使う（→ 3.3）
nvim.ui_attach(cols, rows, &opts /* rgb + ext_linegrid */).await?;

// ウィンドウは winit のイベントループ側で作る。作った時点では見えない
let window = create_window(visible: false, active: false, skip_taskbar: true);
let renderer = Renderer::new(window.hwnd(), &font, window.scale_factor())?;
```

```rust
// nvim からの通知ハンドラ
match event.as_str() {
    "session_write" => {
        state.written = Some(lines_from(args));  // 保持するだけ。書き戻しはセッション終了時
    }
    "session_end" => {
        hide(editor_window);
        restore_focus(state.target_hwnd);
        // 一度でも保存されていたら、最後に保存された内容を反映（無変化ならスキップ）
        if let Some(written) = &state.written {
            if *written != state.original {
                write_back(&state.target, written);
            }
        }
        state.phase = Phase::Idle;
    }
    "config_resolved" => {
        // どこを見に行ったか。「設定が効かない」の一次情報（→ 5.4）
        log::info!("local config dir={:?} loaded={:?}", args.dir, args.loaded);
    }
    "init_error" => {
        // ローカル設定の読み込み失敗。起動は続行済み。ログに残すだけ（→ 5.4）
        log::warn!("user config error: {:?}", args);
    }
    "nvim_dying" => {
        if !policy.is_exiting() {
            recovery_pending = true; // RPC 切断も同じ経路。照会中は要求を保持する
            // controller が起動可能になってから再起動・ui_attach・Idle へのリセットを行う
        }
    }
    _ => {}
}
```

```rust
// ホットキーハンドラ
match state.phase {
    Phase::Editing => {
        focus(editor_window);    // 既存セッションへ戻すだけ
    }
    Phase::Idle => {
        let target = GetForegroundWindow();
        let Some(text) = capture_text(target) else { return };  // 取れなければ何もしない
        state.target   = target;
        state.original = text.clone();
        state.written  = None;   // セッション毎に必ずリセット
        nvim.exec_lua("require('anvi').start_session(...)", ...).await?;
        show_and_focus(editor_window);
        state.phase = Phase::Editing;
    }
    _ => {}
}
```

---

## 付録 C: 用語

| 語 | 意味 |
|---|---|
| host | `anvi.exe`。本アプリ本体 |
| セッション | ホットキー押下から書き戻し完了までの一連の流れ |
| ペア | 常駐 nvim と、その RPC 接続（host イベントと `redraw` が相乗りする）の組 |
| 対象 (target) | 編集元となる、フォーカス中の入力欄とそのウィンドウ |
| UIA | UI Automation。Windows のアクセシビリティ API |
