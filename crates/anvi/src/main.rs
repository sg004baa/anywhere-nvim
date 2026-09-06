// コンソールを持たない GUI アプリとして起動する。常駐ツールなので、起動のたびに
// コンソールウィンドウが開いては話にならない。ログは stderr ではなくファイルへ出す
// （[`init_tracing`]）。
#![windows_subsystem = "windows"]

//! anvi — どこでも Neovim でテキスト編集するための常駐 host。
//!
//! スレッドモデル（DESIGN 11.2）。COM のアパートメント地雷を踏まないよう最初から分ける。
//!
//! - main スレッド: winit のイベントループ。編集ウィンドウの描画・キーボード・IME に
//!   加え、トレイとグローバルホットキーの隠しウィンドウのメッセージもここで汲まれる
//!   （どちらも「登録したスレッドでループが回っていること」を要求するので、
//!   [`gui::run`] を呼ぶ直前に main スレッドで作る）
//! - `uia` の MTA スレッド: UI Automation の一切
//! - `controller` スレッド: `Session` の所有と RPC 呼び出し
//! - tokio ランタイム: nvim との msgpack-rpc

#[cfg(not(windows))]
compile_error!("anvi targets Windows 11 only");

mod autostart;
mod bundle;
mod clipboard;
mod controller;
mod focus;
mod gui;
mod hotkey;
mod keys;
mod shutdown;
mod tray;
mod uia;

use std::fs::File;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::Duration;

use anvi_core::SpawnPolicy;
use anyhow::{Context as _, anyhow};
use tracing_subscriber::EnvFilter;
use winit::event_loop::EventLoop;

use controller::Cmd;
use gui::UserEvent;

/// `RUST_LOG` が無いときのログレベル。設定の既定値ではなくコード上のリテラル。
const DEFAULT_LOG: &str = "info";

/// ログの置き場所。`%LOCALAPPDATA%\anvi-data` は nvim の data ディレクトリ
/// （`NVIM_APPNAME=anvi`）と同じ名前空間で、消してよいものだけが入る。
const LOG_DIR: &str = "anvi-data\\log";
const LOG_FILE: &str = "anvi.log";
const PREVIOUS_LOG_FILE: &str = "anvi.log.1";

fn main() -> anyhow::Result<()> {
    init_tracing()?;

    let bundle = bundle::resolve()?;
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("failed to build the tokio runtime")?;

    // イベントループはウィンドウより先に要る。proxy はここでしか作れないので、
    // トレイ・ホットキー・コントローラのどれよりも先に用意する。
    let event_loop = EventLoop::<UserEvent>::with_user_event()
        .build()
        .context("failed to create the winit event loop")?;
    let proxy = event_loop.create_proxy();

    let (tx, rx) = std::sync::mpsc::channel::<Cmd>();

    // Child startup waits for GUI::resumed to install the HWND session hook.
    let policy = Arc::new(SpawnPolicy::new(shutdown::system_shutting_down));
    let uia =
        uia::Uia::start(Arc::clone(&policy)).context("failed to start the UI Automation thread")?;

    // ペアの世代。controller が `restart_pair` で進め、転送タスクと GUI の両受信点が
    // 旧ペアのメッセージを捨てる判定に使う。
    let generation = Arc::new(AtomicU64::new(0));
    let tray = tray::Tray::new(gui::ProxyHandle::new(proxy.clone()))?;
    let hotkeys = hotkey::Hotkeys::register(gui::ProxyHandle::new(proxy.clone()))?;
    let controller = controller::start(controller::Boot {
        bundle,
        rt: rt.handle().clone(),
        tx: tx.clone(),
        rx,
        uia,
        proxy: proxy.clone(),
        policy: Arc::clone(&policy),
        generation: Arc::clone(&generation),
    })?;

    tracing::info!("anvi is resident");

    // ここから先は main スレッドを winit が占有する。戻るのは終了時だけ。
    let boot = gui::GuiBoot {
        tx: tx.clone(),
        tray,
        hotkeys,
        generation,
        policy: Arc::clone(&policy),
    };
    let result = gui::run(event_loop, boot);

    // Irreversible even after an event-loop error; a late ENDSESSION(FALSE)
    // must not permit a fresh child while the app is exiting.
    policy.exit();
    // A confirmed Windows session end may already have stopped the controller.
    let _ = tx.send(Cmd::Exit);
    // 転送タスクが持つ複製とは別に、こちらの送信端は手放しておく。
    drop(tx);
    let joined = controller
        .join()
        .map_err(|_| anyhow!("the controller thread panicked"));
    // Native provider callbacks may be blocked in another process. Do not let
    // runtime destruction turn the bounded child teardown into an infinite wait.
    rt.shutdown_timeout(Duration::from_secs(1));
    joined?;
    tracing::info!("anvi stopped");
    result
}

fn init_tracing() -> anyhow::Result<()> {
    let filter = match EnvFilter::try_from_default_env() {
        Ok(filter) => filter,
        // `RUST_LOG` が在るのに解釈できないのは書き間違い。黙って既定値へ落とさない。
        Err(err) if std::env::var_os("RUST_LOG").is_some() => {
            return Err(anyhow!("invalid RUST_LOG: {err}"));
        }
        Err(_) => EnvFilter::new(DEFAULT_LOG),
    };
    let path = log_path()?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("failed to create the log directory: {}", dir.display()))?;
    }
    let file = create_log_file(&path)?;

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(false)
        .with_writer(Arc::new(file))
        .try_init()
        .map_err(|err| anyhow!("failed to install the tracing subscriber: {err}"))
}

/// ログファイルの絶対パス。`%LOCALAPPDATA%` が無い環境は想定しない（既定値へ
/// 落とさず落ちる）。
fn log_path() -> anyhow::Result<PathBuf> {
    let local = std::env::var_os("LOCALAPPDATA").context("LOCALAPPDATA is not set")?;
    Ok(PathBuf::from(local).join(LOG_DIR).join(LOG_FILE))
}

/// 前回の終了時の記録を 1 世代だけ残し、今回のログは空で始める。
fn create_log_file(path: &std::path::Path) -> anyhow::Result<File> {
    let previous = path.with_file_name(PREVIOUS_LOG_FILE);
    // rename は既存の退避先を置き換える。先に削除しないことで、今回のログが
    // 無い場合や移動に失敗した場合も、残っている終了時の記録を失わない。
    match std::fs::rename(path, &previous) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => {
            return Err(err).with_context(|| {
                format!(
                    "failed to rotate the log file from {} to {}",
                    path.display(),
                    previous.display()
                )
            });
        }
    }
    File::create(path).with_context(|| format!("failed to open the log file: {}", path.display()))
}

#[cfg(test)]
mod logging_tests {
    use super::{LOG_FILE, PREVIOUS_LOG_FILE, create_log_file};
    use std::fs;
    use std::io::Write as _;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn isolated_log_dir() -> PathBuf {
        static NEXT_ID: AtomicU64 = AtomicU64::new(0);
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "anvi-log-{}-{timestamp}-{}",
            std::process::id(),
            NEXT_ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&dir).unwrap();
        dir
    }

    #[test]
    fn startup_rotates_only_one_previous_generation() -> anyhow::Result<()> {
        let dir = isolated_log_dir();
        let current = dir.join(LOG_FILE);
        let previous = dir.join(PREVIOUS_LOG_FILE);

        let mut file = create_log_file(&current)?;
        assert_eq!(fs::read(&current)?, b"");
        assert!(!previous.try_exists()?);
        file.write_all(b"shutdown evidence\r\n\x00\xff")?;
        drop(file);

        let mut file = create_log_file(&current)?;
        assert_eq!(fs::read(&current)?, b"");
        assert_eq!(fs::read(&previous)?, b"shutdown evidence\r\n\x00\xff");
        file.write_all(b"next shutdown")?;
        drop(file);

        drop(create_log_file(&current)?);
        assert_eq!(fs::read(&current)?, b"");
        assert_eq!(fs::read(&previous)?, b"next shutdown");
        assert_eq!(fs::read_dir(&dir)?.count(), 2);
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[test]
    fn missing_current_log_preserves_existing_backup() -> anyhow::Result<()> {
        let dir = isolated_log_dir();
        let current = dir.join(LOG_FILE);
        let previous = dir.join(PREVIOUS_LOG_FILE);
        fs::write(&previous, b"last surviving shutdown evidence")?;

        drop(create_log_file(&current)?);

        assert_eq!(fs::read(&current)?, b"");
        assert_eq!(fs::read(&previous)?, b"last surviving shutdown evidence");
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[test]
    fn rotation_error_preserves_current_log_and_reports_both_paths() -> anyhow::Result<()> {
        let dir = isolated_log_dir();
        let current = dir.join(LOG_FILE);
        let previous = dir.join(PREVIOUS_LOG_FILE);
        fs::write(&current, b"shutdown evidence")?;
        fs::create_dir(&previous)?;
        fs::write(previous.join("occupied"), b"do not remove")?;

        let err = create_log_file(&current).unwrap_err();

        assert!(err.downcast_ref::<std::io::Error>().is_some());
        let message = err.to_string();
        assert!(message.contains(&current.display().to_string()));
        assert!(message.contains(&previous.display().to_string()));
        assert_eq!(fs::read(&current)?, b"shutdown evidence");
        assert_eq!(fs::read(previous.join("occupied"))?, b"do not remove");
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[test]
    fn log_creation_error_reports_current_path() -> anyhow::Result<()> {
        let dir = isolated_log_dir();
        let current = dir.join("missing-directory").join(LOG_FILE);

        let err = create_log_file(&current).unwrap_err();

        assert!(err.downcast_ref::<std::io::Error>().is_some());
        assert!(err.to_string().contains(&current.display().to_string()));
        fs::remove_dir_all(dir)?;
        Ok(())
    }
}
