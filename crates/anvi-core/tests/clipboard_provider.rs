//! `g:clipboard` provider（同梱 init.lua）が host への RPC 経由で実際に往復する
//! ことを実 nvim に対して検証する（issue #11）。
//!
//! 見るのは 2 方向とも。Windows 側でコピーされた内容が `"+p` で貼れること、
//! anvi 内で `"+y` した内容がクリップボードに載ること。手で組んだ RPC を叩くだけでは
//! 「nvim が本当に provider を呼ぶか」は分からないので、本物の nvim にキーを打つ。
//! OS クリップボードの実体は Win32（`anvi` crate）だが、ここで見たいのは配線なので
//! [`anvi_core::clipboard::Memory`] を挿す。
//!
//! nvim は `ANVI_TEST_NVIM` が指す実行ファイル、無ければ PATH 上の `nvim`。
//! どちらも起動できなければテストは失敗する（スキップしない）。

use std::path::PathBuf;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use anvi_core::clipboard::{Clipboard, Memory};
use anvi_core::{HostEvent, NvimConfig, NvimHandles, NvimServer, SpawnPolicy};
use tokio::sync::mpsc::UnboundedReceiver;

const EVENT_TIMEOUT: Duration = Duration::from_secs(5);

/// `nvim_input` は nvim が打鍵を処理し終える前に返る。副作用の確認はこの間隔で
/// ポーリングする（固定 sleep にすると遅いマシンで落ちる）。
const POLL_INTERVAL: Duration = Duration::from_millis(20);
const POLL_TIMEOUT: Duration = Duration::from_secs(5);

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

fn nvim_exe() -> PathBuf {
    match std::env::var_os("ANVI_TEST_NVIM") {
        Some(path) => PathBuf::from(path),
        None => PathBuf::from("nvim"),
    }
}

fn runtime_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("runtime")
}

/// 渡したクリップボードを載せて nvim を起こす。
async fn start(appname: &str, clip: Arc<Memory>) -> (NvimServer, NvimHandles) {
    LazyLock::force(&XDG_ROOT);
    let cfg = NvimConfig {
        nvim_exe: nvim_exe(),
        runtime_dir: runtime_dir(),
        appname: appname.to_owned(),
        clipboard: clip as Arc<dyn Clipboard>,
    };
    NvimServer::spawn(&cfg, &SpawnPolicy::default())
        .await
        .unwrap_or_else(|e| panic!("failed to spawn {}: {e:#}", cfg.nvim_exe.display()))
}

/// `SessionWrite` が来るまでイベントを読み進める。`config_resolved` など先行する
/// イベントが挟まるので、名指しのものが出るまで捨てる。
async fn expect_session_write(events: &mut UnboundedReceiver<HostEvent>) -> Vec<String> {
    let deadline = tokio::time::Instant::now() + EVENT_TIMEOUT;
    loop {
        let event = tokio::time::timeout_at(deadline, events.recv())
            .await
            .unwrap_or_else(|_| panic!("no session_write within {EVENT_TIMEOUT:?}"))
            .expect("the host event channel closed before session_write");
        match event {
            HostEvent::SessionWrite(lines) => return lines,
            HostEvent::ConfigResolved { .. } => {}
            other => panic!("unexpected host event before session_write: {other:?}"),
        }
    }
}

/// クリップボードが期待の内容になるまで待つ。
async fn wait_for_clipboard(clip: &Memory, expected: &str) {
    let deadline = tokio::time::Instant::now() + POLL_TIMEOUT;
    loop {
        let got = clip.contents();
        if got == expected {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the clipboard did not become {expected:?} within {POLL_TIMEOUT:?}; got {got:?}"
        );
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// Windows 側でコピーされた内容が `"+p` で貼れる。末尾改行が無いので文字指向、
/// つまり空行に対する貼り付けはその行に載る。
#[tokio::test]
async fn pastes_what_windows_put_on_the_clipboard() {
    let clip = Arc::new(Memory::default());
    clip.put("hello");
    let (mut server, mut handles) = start("anvi-test-clipboard-paste", Arc::clone(&clip)).await;

    server
        .start_session(&[String::new()], None)
        .await
        .expect("start_session");
    server.input("\"+p").await.expect("send keys");
    server.input(":w<CR>").await.expect("send :w");

    assert_eq!(
        expect_session_write(&mut handles.host).await,
        vec!["hello".to_owned()],
        "the clipboard contents did not reach the buffer via the provider"
    );

    server.shutdown().await.expect("shutdown");
}

/// `"+yy` した行はクリップボードに載る。行指向のヤンクでは nvim が `lines` の末尾に
/// 空行を入れて渡してくるので、結合した結果は末尾 CRLF で終わる（二重にならない）。
#[tokio::test]
async fn copies_a_yanked_line_to_the_clipboard() {
    let clip = Arc::new(Memory::default());
    let (mut server, _handles) = start("anvi-test-clipboard-copy", Arc::clone(&clip)).await;

    server
        .start_session(&["日本語".to_owned(), "x".to_owned()], None)
        .await
        .expect("start_session");
    server.input("gg\"+yy").await.expect("send keys");

    wait_for_clipboard(&clip, "日本語\r\n").await;

    server.shutdown().await.expect("shutdown");
}

/// 末尾改行のあるクリップボードは行指向で貼れる。`"+p` はカーソル行の**下**に
/// 行として入るので、空バッファに貼ると先頭の空行は残る。
#[tokio::test]
async fn pastes_a_linewise_clipboard_as_whole_lines() {
    let clip = Arc::new(Memory::default());
    clip.put("L1\r\nL2\r\n");
    let (mut server, mut handles) =
        start("anvi-test-clipboard-paste-linewise", Arc::clone(&clip)).await;

    server
        .start_session(&[String::new()], None)
        .await
        .expect("start_session");
    server.input("\"+p").await.expect("send keys");
    server.input(":w<CR>").await.expect("send :w");

    assert_eq!(
        expect_session_write(&mut handles.host).await,
        vec![String::new(), "L1".to_owned(), "L2".to_owned()],
        "the linewise clipboard was not pasted as whole lines"
    );

    server.shutdown().await.expect("shutdown");
}

/// 矩形ヤンクの regtype は `"\x16{幅}"` ではなく `"b"` で来る。host はそれを解釈
/// しないので、矩形でもエラーにならず素直に行として載る。
#[tokio::test]
async fn copies_a_blockwise_yank_without_erroring() {
    let clip = Arc::new(Memory::default());
    let (mut server, _handles) =
        start("anvi-test-clipboard-copy-blockwise", Arc::clone(&clip)).await;

    server
        .start_session(&["abcd".to_owned(), "efgh".to_owned()], None)
        .await
        .expect("start_session");
    server.input("gg0<C-v>jl\"+y").await.expect("send keys");

    wait_for_clipboard(&clip, "ab\r\nef\r\n").await;

    server.shutdown().await.expect("shutdown");
}
