//! ステップ 1 の受け入れ条件（DESIGN §12）を実 nvim に対して検証する。
//!
//! カバーするのは項目 1 / 2 / 4 / 5 / 6。項目 3（UI クライアントのアタッチ）と
//! 項目 7（ウィンドウを × で閉じる）は GUI が要るため実機確認に残す。
//!
//! nvim は `ANVI_TEST_NVIM` が指す実行ファイル、無ければ PATH 上の `nvim`。
//! どちらも起動できなければテストは失敗する（スキップしない）。

use std::path::PathBuf;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use anvi_core::clipboard::Memory;
use anvi_core::{Applied, HostEvent, NvimConfig, NvimServer, Phase, Session, SpawnPolicy};
use nvim_rs::rpc::handler::Dummy;
use tokio::io::WriteHalf;
use tokio::net::TcpStream;
use tokio::sync::mpsc::UnboundedReceiver;

/// host とは別チャンネルで nvim に繋ぐ検証用クライアント。
type Client = nvim_rs::Neovim<nvim_rs::compat::tokio::Compat<WriteHalf<TcpStream>>>;

const EVENT_TIMEOUT: Duration = Duration::from_secs(5);
/// 「何も来ないこと」を確かめるための上限。無制限に待たない。
const QUIET_WINDOW: Duration = Duration::from_millis(500);

/// XDG 一式をテンポラリへ切り替える。ユーザーの実設定を絶対に読ませないため
/// （DESIGN §5.2）。テストごとの隔離は `NVIM_APPNAME` が担う。
static XDG_ROOT: LazyLock<PathBuf> = LazyLock::new(|| {
    let root = std::env::temp_dir().join(format!("anvi-test-{}", std::process::id()));
    for key in [
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
        "XDG_STATE_HOME",
        "XDG_CACHE_HOME",
    ] {
        let dir = root.join(key.to_ascii_lowercase());
        std::fs::create_dir_all(&dir).expect("create XDG temp dir");
        // SAFETY: 全テストが最初にこの LazyLock を強制するため、set_var の間に
        // env を読むテストスレッドは存在しない（初期化中は他スレッドがブロックされる）。
        unsafe { std::env::set_var(key, &dir) };
    }
    root
});

fn config_dir(appname: &str) -> PathBuf {
    XDG_ROOT.join("xdg_config_home").join(appname)
}

fn nvim_exe() -> PathBuf {
    match std::env::var_os("ANVI_TEST_NVIM") {
        Some(path) => PathBuf::from(path),
        None => PathBuf::from("nvim"),
    }
}

fn runtime_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("runtime")
}

/// nvim を起こし、host のイベント受信口と検証用 RPC クライアントを返す。
///
/// 起動直後に必ず 1 つだけ届く `ConfigResolved` はここで受け取り、契約
/// （解決先は `NVIM_APPNAME` の設定ディレクトリ、`loaded` は実在と一致）を
/// 全テスト共通で検証する。以降のイベント列は呼び出し側が読む。
async fn start(appname: &str) -> (NvimServer, UnboundedReceiver<HostEvent>, Client) {
    LazyLock::force(&XDG_ROOT);
    let cfg = NvimConfig {
        nvim_exe: nvim_exe(),
        runtime_dir: runtime_dir(),
        appname: appname.to_owned(),
        clipboard: Arc::new(Memory::default()),
    };
    let (server, handles) = NvimServer::spawn(&cfg, &SpawnPolicy::default())
        .await
        .unwrap_or_else(|e| panic!("failed to spawn {}: {e:#}", cfg.nvim_exe.display()));
    let (client, _io) = nvim_rs::create::tokio::new_tcp(("127.0.0.1", server.port()), Dummy::new())
        .await
        .expect("connect the verification client to nvim");
    let mut events = handles.host;
    let expected = config_dir(appname);
    match expect_event(&mut events).await {
        HostEvent::ConfigResolved { dir, loaded } => {
            assert_eq!(PathBuf::from(dir), expected);
            assert_eq!(loaded, expected.join("init.lua").is_file());
        }
        other => panic!("the first host event must resolve the config dir, got {other:?}"),
    }
    (server, events, client)
}

async fn start_session(server: &NvimServer, lines: &[String]) {
    server
        .start_session(lines, None)
        .await
        .expect("start_session");
}

async fn expect_event(events: &mut UnboundedReceiver<HostEvent>) -> HostEvent {
    tokio::time::timeout(EVENT_TIMEOUT, events.recv())
        .await
        .expect("timed out waiting for a host event")
        .expect("the host event channel closed")
}

async fn expect_quiet(events: &mut UnboundedReceiver<HostEvent>) {
    if let Ok(event) = tokio::time::timeout(QUIET_WINDOW, events.recv()).await {
        panic!("unexpected host event: {event:?}");
    }
}

/// キーを 1 バイトも取りこぼさずに流し込む。
async fn feed(client: &Client, keys: &str) {
    let written = client.input(keys).await.expect("nvim_input");
    assert_eq!(
        usize::try_from(written).expect("negative byte count"),
        keys.len(),
        "nvim did not consume all of {keys:?}"
    );
}

async fn lua(client: &Client, code: &str) -> rmpv::Value {
    client.exec_lua(code, vec![]).await.expect("exec_lua")
}

async fn buffer_lines(client: &Client) -> Vec<String> {
    client
        .get_current_buf()
        .await
        .expect("nvim_get_current_buf")
        .get_lines(0, -1, true)
        .await
        .expect("nvim_buf_get_lines")
}

fn owned(lines: &[&str]) -> Vec<String> {
    lines.iter().map(|s| (*s).to_owned()).collect()
}

/// 受け取ったイベントをそのまま状態機械へ流す（host の実装と同じ扱い）。
fn feed_session(session: &mut Session, event: HostEvent) {
    match event {
        HostEvent::SessionWrite(lines) => session.on_write(lines),
        other => panic!("unexpected host event: {other:?}"),
    }
}

/// §12-1 / §12-2: 接続して `start_session` すると、その内容が現在のバッファに載る。
#[tokio::test]
async fn start_session_fills_the_current_buffer() {
    let (mut server, mut events, client) = start("anvi-test-start-session").await;
    let lines = owned(&["hello", "どこでも neovim", ""]);

    server
        .start_session(&lines, Some("markdown"))
        .await
        .expect("start_session");

    assert_eq!(buffer_lines(&client).await, lines);
    let buf = client
        .get_current_buf()
        .await
        .expect("nvim_get_current_buf");
    assert_eq!(
        buf.get_name().await.expect("nvim_buf_get_name"),
        "anvi://edit"
    );
    // 一時ファイルを作らない契約（DESIGN §4.5 / §6.4）
    assert_eq!(
        lua(&client, "return vim.bo.buftype").await.as_str(),
        Some("acwrite")
    );
    assert_eq!(
        lua(&client, "return vim.bo.filetype").await.as_str(),
        Some("markdown")
    );
    assert_eq!(
        lua(&client, "return vim.bo.modified").await.as_bool(),
        Some(false)
    );

    // 2 回目のセッションは前のバッファを wipe して作り直す（DESIGN §6.4）
    let second = owned(&["another session"]);
    start_session(&server, &second).await;
    assert_eq!(buffer_lines(&client).await, second);
    let session_bufs = "local n = 0 \
        for _, b in ipairs(vim.api.nvim_list_bufs()) do \
          if vim.api.nvim_buf_get_name(b) == 'anvi://edit' then n = n + 1 end \
        end \
        return n";
    assert_eq!(
        lua(&client, session_bufs).await.as_i64(),
        Some(1),
        "the previous session buffer should have been wiped"
    );

    expect_quiet(&mut events).await;
    server.shutdown().await.expect("shutdown");
}

/// §12-4: `ZZ` → `session_write` → `session_end`、状態機械は最後の保存を反映する。
#[tokio::test]
async fn zz_writes_then_ends_and_applies_the_edit() {
    let (mut server, mut events, client) = start("anvi-test-zz").await;
    let original = owned(&["original"]);

    let mut session = Session::default();
    assert!(session.begin_capture());
    start_session(&server, &original).await;
    session.begin_edit(original.clone());
    assert_eq!(session.phase(), Phase::Editing);

    feed(&client, "iEDITED <Esc>ZZ").await;

    let edited = owned(&["EDITED original"]);
    feed_session(&mut session, expect_event(&mut events).await);
    assert_eq!(expect_event(&mut events).await, HostEvent::SessionEnd);
    assert_eq!(session.on_end(), Applied::WriteBack(edited));
    assert_eq!(session.phase(), Phase::Idle);

    server.shutdown().await.expect("shutdown");
}

/// §12-4: `:wq` は `cnoreabbrev` 経由で `AnviWriteQuit` に化ける（コマンド名で契約しない）。
#[tokio::test]
async fn wq_abbrev_writes_then_ends() {
    let (mut server, mut events, client) = start("anvi-test-wq").await;
    let original = owned(&["原文"]);

    let mut session = Session::default();
    assert!(session.begin_capture());
    start_session(&server, &original).await;
    session.begin_edit(original.clone());

    feed(&client, "ccrewritten<Esc>:wq<CR>").await;

    feed_session(&mut session, expect_event(&mut events).await);
    assert_eq!(expect_event(&mut events).await, HostEvent::SessionEnd);
    assert_eq!(session.on_end(), Applied::WriteBack(owned(&["rewritten"])));

    // 乗っ取れているなら nvim は生きている
    assert!(
        !client
            .get_api_info()
            .await
            .expect("nvim_get_api_info")
            .is_empty()
    );
    expect_quiet(&mut events).await;
    server.shutdown().await.expect("shutdown");
}

/// §12-4: `:wq!` も乗っ取る。`!` の入力時点で `wq` の abbrev が展開されて
/// `AnviWriteQuit!` になる（コマンド側の bang = true が受ける）。
#[tokio::test]
async fn wq_bang_abbrev_writes_then_ends() {
    let (mut server, mut events, client) = start("anvi-test-wq-bang").await;
    let original = owned(&["原文"]);

    let mut session = Session::default();
    assert!(session.begin_capture());
    start_session(&server, &original).await;
    session.begin_edit(original.clone());

    feed(&client, "ccrewritten<Esc>:wq!<CR>").await;

    feed_session(&mut session, expect_event(&mut events).await);
    assert_eq!(expect_event(&mut events).await, HostEvent::SessionEnd);
    assert_eq!(session.on_end(), Applied::WriteBack(owned(&["rewritten"])));

    // 乗っ取れているなら nvim は生きている
    assert!(
        !client
            .get_api_info()
            .await
            .expect("nvim_get_api_info")
            .is_empty()
    );
    expect_quiet(&mut events).await;
    server.shutdown().await.expect("shutdown");
}

/// 内容が変わらないまま `ZZ` した場合は書き戻さない（DESIGN §9.4）。
#[tokio::test]
async fn unchanged_content_is_not_written_back() {
    let (mut server, mut events, client) = start("anvi-test-unchanged").await;
    let original = owned(&["untouched"]);

    let mut session = Session::default();
    assert!(session.begin_capture());
    start_session(&server, &original).await;
    session.begin_edit(original.clone());

    feed(&client, "ZZ").await;

    feed_session(&mut session, expect_event(&mut events).await);
    assert_eq!(expect_event(&mut events).await, HostEvent::SessionEnd);
    assert_eq!(session.on_end(), Applied::Unchanged);

    server.shutdown().await.expect("shutdown");
}

/// §12-5: `:w` のあと（保存せずに更に編集して）`:q` → 反映されるのは保存済みの内容。
#[tokio::test]
async fn write_then_quit_applies_the_saved_content() {
    let (mut server, mut events, client) = start("anvi-test-write-quit").await;
    let original = owned(&["原文"]);

    let mut session = Session::default();
    assert!(session.begin_capture());
    start_session(&server, &original).await;
    session.begin_edit(original.clone());

    feed(&client, "ccsaved<Esc>:w<CR>").await;
    feed_session(&mut session, expect_event(&mut events).await);
    assert_eq!(
        lua(&client, "return vim.bo.modified").await.as_bool(),
        Some(false),
        "BufWriteCmd must clear 'modified' itself on an acwrite buffer"
    );

    // 保存せずに更に編集してから :q
    feed(&client, "A unsaved<Esc>:q<CR>").await;
    assert_eq!(expect_event(&mut events).await, HostEvent::SessionEnd);

    assert_eq!(session.on_end(), Applied::WriteBack(owned(&["saved"])));
    assert_eq!(
        buffer_lines(&client).await,
        owned(&["saved unsaved"]),
        "the buffer still holds the unsaved edit; only the saved content is applied"
    );

    server.shutdown().await.expect("shutdown");
}

/// §12-6: 一度も保存せず `:q!` → 破棄。かつ nvim プロセスは生きたまま。
#[tokio::test]
async fn quit_bang_discards_and_keeps_nvim_alive() {
    let (mut server, mut events, client) = start("anvi-test-quit-bang").await;
    let original = owned(&["原文"]);

    let mut session = Session::default();
    assert!(session.begin_capture());
    start_session(&server, &original).await;
    session.begin_edit(original.clone());

    feed(&client, "ccthrown away<Esc>:q!<CR>").await;
    assert_eq!(expect_event(&mut events).await, HostEvent::SessionEnd);
    assert_eq!(session.on_end(), Applied::Discarded);
    assert_eq!(session.phase(), Phase::Idle);

    // 切断は来ていない（= nvim は死んでいない）
    expect_quiet(&mut events).await;
    // 同じチャンネルで次のセッションを張れる
    let next = owned(&["next session"]);
    start_session(&server, &next).await;
    assert_eq!(buffer_lines(&client).await, next);
    expect_quiet(&mut events).await;

    server.shutdown().await.expect("shutdown");
}

/// §12-6 の裏: `ZQ` も nvim を殺さずに破棄で終わる。
#[tokio::test]
async fn zq_discards_and_keeps_nvim_alive() {
    let (mut server, mut events, client) = start("anvi-test-zq").await;
    let original = owned(&["原文"]);

    let mut session = Session::default();
    assert!(session.begin_capture());
    start_session(&server, &original).await;
    session.begin_edit(original.clone());

    feed(&client, "ccthrown away<Esc>ZQ").await;
    assert_eq!(expect_event(&mut events).await, HostEvent::SessionEnd);
    assert_eq!(session.on_end(), Applied::Discarded);

    expect_quiet(&mut events).await;
    assert!(
        !client
            .get_api_info()
            .await
            .expect("nvim_get_api_info")
            .is_empty()
    );
    server.shutdown().await.expect("shutdown");
}

/// DESIGN §4.7 / §5.4: 壊れたローカル設定は起動を止めず、`init_error` として届く。
#[tokio::test]
async fn broken_local_config_reports_init_error_and_keeps_working() {
    let appname = "anvi-test-broken-config";
    LazyLock::force(&XDG_ROOT);
    let dir = config_dir(appname);
    std::fs::create_dir_all(&dir).expect("create local config dir");
    std::fs::write(dir.join("init.lua"), "this is not lua(\n").expect("write local config");

    let (mut server, mut events, client) = start(appname).await;

    match expect_event(&mut events).await {
        HostEvent::InitError { kind, message } => {
            assert_eq!(kind, "user_config_error");
            assert!(message.contains("init.lua"), "unhelpful message: {message}");
        }
        other => panic!("expected init_error, got {other:?}"),
    }

    // 同梱コアだけで動作は継続する
    let lines = owned(&["still working"]);
    start_session(&server, &lines).await;
    assert_eq!(buffer_lines(&client).await, lines);

    expect_quiet(&mut events).await;
    server.shutdown().await.expect("shutdown");
}

/// DESIGN §4.7: ローカル設定は runtimepath に載る（append）が、契約は再宣言で戻る。
#[tokio::test]
async fn local_config_cannot_break_the_zz_contract() {
    let appname = "anvi-test-local-config";
    LazyLock::force(&XDG_ROOT);
    let dir = config_dir(appname);
    std::fs::create_dir_all(dir.join("lua")).expect("create local config dir");
    std::fs::write(
        dir.join("lua").join("awtestlocal.lua"),
        "vim.g.aw_test_local_module = true\nreturn {}\n",
    )
    .expect("write local module");
    std::fs::write(
        dir.join("init.lua"),
        "require('awtestlocal')\nvim.keymap.set('n', 'ZZ', '<Nop>')\n",
    )
    .expect("write local config");

    let (mut server, mut events, client) = start(appname).await;
    let original = owned(&["原文"]);

    // append された runtimepath からローカルモジュールを require できている
    assert_eq!(
        lua(&client, "return vim.g.aw_test_local_module")
            .await
            .as_bool(),
        Some(true)
    );
    // 同梱コアの lua/anvi/ はローカル側に隠蔽されていない（prepend）
    assert_eq!(
        lua(&client, "return type(require('anvi').start_session)")
            .await
            .as_str(),
        Some("function")
    );

    let mut session = Session::default();
    assert!(session.begin_capture());
    start_session(&server, &original).await;
    session.begin_edit(original.clone());

    // ローカル設定が潰した ZZ は enforce_contract で復元されている
    feed(&client, "ccrestored<Esc>ZZ").await;
    feed_session(&mut session, expect_event(&mut events).await);
    assert_eq!(expect_event(&mut events).await, HostEvent::SessionEnd);
    assert_eq!(session.on_end(), Applied::WriteBack(owned(&["restored"])));

    server.shutdown().await.expect("shutdown");
}
