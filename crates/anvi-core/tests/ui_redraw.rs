//! `anvi_core::ui::redraw` を実 nvim の `redraw` 通知に対して検証する
//! （→ v2 計画 §4.1）。
//!
//! 手で組んだバッチだけでは「nvim が本当にそう送ってくるか」は分からない。ここでは
//! 本物の nvim を UI クライアントとして掴み、打ち込んだ文字がグリッドに現れるところ
//! までを通す。特に全角は「本体セル + 空文字列セル」で来るという規約を実物で固定する。
//!
//! nvim は `ANVI_TEST_NVIM` が指す実行ファイル、無ければ PATH 上の `nvim`。
//! どちらも起動できなければテストは失敗する（スキップしない）。

use std::path::PathBuf;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use anvi_core::clipboard::Memory;
use anvi_core::ui::UiState;
use anvi_core::ui::redraw::apply;
use anvi_core::{NvimConfig, NvimHandles, NvimServer, SpawnPolicy};
use rmpv::Value;
use tokio::sync::mpsc::UnboundedReceiver;

const EVENT_TIMEOUT: Duration = Duration::from_secs(5);

/// 要求するグリッド。行が折り返さない程度に狭くしておく。
const COLS: u16 = 20;
const ROWS: u16 = 6;

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

async fn start(appname: &str) -> (NvimServer, NvimHandles) {
    LazyLock::force(&XDG_ROOT);
    let cfg = NvimConfig {
        nvim_exe: nvim_exe(),
        runtime_dir: runtime_dir(),
        appname: appname.to_owned(),
        clipboard: Arc::new(Memory::default()),
    };
    NvimServer::spawn(&cfg, &SpawnPolicy::default())
        .await
        .unwrap_or_else(|e| panic!("failed to spawn {}: {e:#}", cfg.nvim_exe.display()))
}

/// 条件が満たされるまで `redraw` バッチを食い続ける。1 バッチに収まる保証はないので、
/// 期限までは何バッチでも読む。適用が失敗したらそこで落とす（`apply` の契約）。
async fn apply_until(
    state: &mut UiState,
    redraw: &mut UnboundedReceiver<Vec<Value>>,
    what: &str,
    done: impl Fn(&UiState) -> bool,
) {
    let deadline = tokio::time::Instant::now() + EVENT_TIMEOUT;
    while !done(state) {
        let batch = tokio::time::timeout_at(deadline, redraw.recv())
            .await
            .unwrap_or_else(|_| {
                panic!(
                    "{what}: not reached within {EVENT_TIMEOUT:?}; \
                     row 0 = {:?}, mode = {:?}",
                    state.grid.row_text(0),
                    state.mode.name()
                )
            })
            .expect("the redraw channel closed before the expected state");
        apply(state, &batch).unwrap_or_else(|e| panic!("{what}: apply failed: {e:#}"));
    }
}

/// アタッチして `grid_resize` が届くまで進める。空バッファの行には何も描かれない
/// （`grid_clear` 済みの領域に nvim は空白すら送らない）ので、待つのは寸法だけ。
/// ここで寸法が要求どおりでなければ cols/rows を取り違えている。
async fn attach_and_settle(server: &NvimServer, handles: &mut NvimHandles) -> UiState {
    let mut state = UiState::default();
    server
        .attach_ui(COLS, ROWS)
        .await
        .expect("attach as a ui client");
    apply_until(&mut state, &mut handles.redraw, "the initial resize", |s| {
        s.grid.rows() == usize::from(ROWS)
    })
    .await;
    assert_eq!(state.grid.cols(), usize::from(COLS));
    state
}

/// 挿入した ASCII がそのまま 1 行目に載り、`<Esc>` の `mode_change` まで届く。
#[tokio::test]
async fn typed_ascii_shows_up_on_the_first_row() {
    let (mut server, mut handles) = start("anvi-test-ui-redraw-ascii").await;
    let mut state = attach_and_settle(&server, &mut handles).await;

    server.input("ihello<Esc>").await.expect("send keys");
    apply_until(&mut state, &mut handles.redraw, "\"hello\" on row 0", |s| {
        s.grid.row_text(0).trim_end() == "hello" && s.mode.name() == "normal"
    })
    .await;

    // 打ち終わったあとは normal。IME はここで切られる。
    assert!(
        !state.mode.accepts_text_input(),
        "normal mode must not accept text input"
    );

    server.shutdown().await.expect("shutdown");
}

/// 全角は「本体セル + 空文字列セル」の 2 セルで来る。空セルを捨てると列がずれ、
/// 詰めて描くと文字が重なる。
#[tokio::test]
async fn typed_wide_chars_keep_their_continuation_cells() {
    let (mut server, mut handles) = start("anvi-test-ui-redraw-wide").await;
    let mut state = attach_and_settle(&server, &mut handles).await;

    server.input("iあい<Esc>").await.expect("send keys");
    apply_until(
        &mut state,
        &mut handles.redraw,
        "\"あい\" on row 0",
        |s| s.grid.row_text(0).trim_end() == "あい" && s.mode.name() == "normal",
    )
    .await;

    let head: Vec<&str> = state.grid.row(0)[..4]
        .iter()
        .map(|c| c.text.as_str())
        .collect();
    assert_eq!(head, ["あ", "", "い", ""]);

    server.shutdown().await.expect("shutdown");
}
