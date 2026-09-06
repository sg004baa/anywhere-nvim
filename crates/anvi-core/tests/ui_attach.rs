//! `NvimServer::attach_ui` の配線を実 nvim に対して検証する（→ v2 計画 §4.3）。
//!
//! 見るのは「UI クライアントとして登録したら `redraw` 通知が
//! [`NvimHandles::redraw`] まで素通しで届くか」の 1 点だけ。バッチの中身の解釈は
//! `anvi_core::ui::redraw` の担当なので、ここでは触らない。
//!
//! nvim は `ANVI_TEST_NVIM` が指す実行ファイル、無ければ PATH 上の `nvim`。
//! どちらも起動できなければテストは失敗する（スキップしない）。

use std::path::PathBuf;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use anvi_core::clipboard::Memory;
use anvi_core::{HostEvent, NvimConfig, NvimHandles, NvimServer, SpawnPolicy};
use rmpv::Value;
use tokio::sync::mpsc::UnboundedReceiver;

const EVENT_TIMEOUT: Duration = Duration::from_secs(5);

/// 要求するグリッド。`grid_resize` が同じ幅・高さで返ってくることまで見る。
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

/// redraw イベント（`[name, args...]`）の名前。形が違えば契約違反なので落とす。
fn event_name(event: &Value) -> &str {
    event
        .as_array()
        .and_then(|parts| parts.first())
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("a redraw event is not [name, ...]: {event:?}"))
}

/// 最初の `grid_line` が現れるまでのイベントを集める。1 バッチに収まる保証はない
/// ので、期限までは何バッチでも読む。
async fn events_until_grid_line(redraw: &mut UnboundedReceiver<Vec<Value>>) -> Vec<Value> {
    let deadline = tokio::time::Instant::now() + EVENT_TIMEOUT;
    let mut seen = Vec::new();
    loop {
        let batch = tokio::time::timeout_at(deadline, redraw.recv())
            .await
            .unwrap_or_else(|_| {
                let names: Vec<&str> = seen.iter().map(event_name).collect();
                panic!("no grid_line within {EVENT_TIMEOUT:?}; got {names:?}")
            })
            .expect("the redraw channel closed before any grid_line");
        assert!(!batch.is_empty(), "an empty redraw batch reached the host");

        let done = batch.iter().any(|event| event_name(event) == "grid_line");
        seen.extend(batch);
        if done {
            return seen;
        }
    }
}

/// UI としてアタッチしたら `redraw` が届き、その中に `grid_line` が含まれる。
/// 届かなければ Handler → チャンネルの配線か `nvim_ui_attach` の呼び方が壊れている。
#[tokio::test]
async fn attach_ui_streams_redraw_batches_containing_grid_line() {
    let (mut server, mut handles) = start("anvi-test-ui-attach").await;
    server
        .attach_ui(COLS, ROWS)
        .await
        .expect("attach as a ui client");

    let events = events_until_grid_line(&mut handles.redraw).await;

    // 要求した寸法がそのまま返ること。cols/rows を取り違えると
    // `grid_resize` が [1, 6, 20] になる。
    let resize = events
        .iter()
        .find(|event| event_name(event) == "grid_resize")
        .unwrap_or_else(|| panic!("no grid_resize before the first grid_line"));
    let args = resize
        .as_array()
        .and_then(|parts| parts.get(1))
        .and_then(Value::as_array)
        .unwrap_or_else(|| panic!("grid_resize has no argument tuple: {resize:?}"));
    assert_eq!(
        args.iter().map(Value::as_u64).collect::<Vec<_>>(),
        vec![Some(1), Some(u64::from(COLS)), Some(u64::from(ROWS))],
        "grid_resize did not report the requested grid: {resize:?}"
    );

    // host イベント経路は redraw に汚染されない（`redraw` は HostEvent にしない）。
    // 起動時の `ConfigResolved` だけは通る。
    while let Ok(event) = handles.host.try_recv() {
        assert!(
            matches!(event, HostEvent::ConfigResolved { .. }),
            "a redraw notification leaked into the host event channel: {event:?}"
        );
    }

    server.shutdown().await.expect("shutdown");
}
