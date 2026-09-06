//! セッション状態機械を持つスレッド（DESIGN 6、11.2、付録 B）。
//!
//! `Session` と nvim の RPC はこのスレッドだけが触る。GUI（winit のループが回る
//! main スレッド）とは `Cmd` で、UIA の MTA スレッドとは `Uia` で、それぞれ
//! チャンネル越しにしか繋がらない。逆向き（コントローラ → GUI）は
//! [`EventLoopProxy`] に載せた [`UserEvent`]。

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::Duration;

use anvi_core::{
    Applied, HostEvent, NvimConfig, NvimServer, Phase, Session, SpawnCancelled, SpawnPolicy,
};
use anyhow::{Context as _, anyhow};
use tokio::runtime::Handle;
use tokio::sync::mpsc::UnboundedReceiver;
use winit::event_loop::EventLoopProxy;

use crate::bundle::{APPNAME, Bundle};
use crate::focus;
use crate::gui::{DEFAULT_GRID, UserEvent};
use crate::uia::Uia;

/// コントローラへの指令。GUI スレッドと RPC 転送タスクから届く。
#[derive(Debug)]
pub enum Cmd {
    /// Sent only after the GUI has installed its session-end HWND subclass.
    Start,
    /// Wake the receiver after an atomic policy change; the policy is truth,
    /// not this command's position in the queue.
    LifecycleChanged,
    Hotkey,
    Exit,
    /// nvim からの通知。`generation` は送信元ペアの世代。キュー滞留中に
    /// ペアが再起動され得るので、受信側でも現世代と照合して旧ペア分を捨てる。
    Host {
        generation: u64,
        event: HostEvent,
    },
    /// GUI からのキー入力（既に nvim 記法）。
    Input(String),
    /// ウィンドウのサイズが変わった。
    Resize {
        cols: u16,
        rows: u16,
    },
    /// ウィンドウの × が押された。`AnviQuit` と同じ扱い。
    CloseRequested,
}

/// nvim との接続一式。RPC は 1 本で、host イベントと UI の `redraw` が相乗りする。
struct Pair {
    nvim: NvimServer,
    host_rx: UnboundedReceiver<HostEvent>,
    redraw_rx: UnboundedReceiver<Vec<rmpv::Value>>,
}

/// コントローラスレッドに渡す一切。
pub struct Boot {
    pub bundle: Bundle,
    pub rt: Handle,
    pub tx: Sender<Cmd>,
    pub rx: Receiver<Cmd>,
    pub uia: Uia,
    /// GUI へ「出せ」「隠せ」「前面へ」を伝える経路。
    pub proxy: EventLoopProxy<UserEvent>,
    pub policy: Arc<SpawnPolicy>,
    /// 現行ペアの世代。main が作り、GUI 側の redraw 判定と共有する。
    pub generation: Arc<AtomicU64>,
}

/// nvim を起こして UI としても繋ぐ。
///
/// `ui_attach` までここでやる。attach していない nvim は 1 バイトも描かないので、
/// 「起動はしたが画面が真っ白」を作らないためにペアの生成と不可分にしておく。
fn spawn_pair(bundle: &Bundle, rt: &Handle, policy: &SpawnPolicy) -> anyhow::Result<Pair> {
    let cfg = NvimConfig {
        nvim_exe: bundle.nvim_exe.clone(),
        runtime_dir: bundle.runtime_dir.clone(),
        appname: APPNAME.to_owned(),
        clipboard: Arc::new(crate::clipboard::WinClipboard),
    };
    let (mut nvim, handles) = rt
        .block_on(NvimServer::spawn(&cfg, policy))
        .context("failed to start the nvim server")?;
    let (cols, rows) = DEFAULT_GRID;
    let attached = rt.block_on(policy.run(async {
        tokio::time::timeout(Duration::from_secs(2), nvim.attach_ui(cols, rows))
            .await
            .context("timed out attaching the ui")?
    }));
    if let Err(err) = attached {
        if let Err(shutdown_err) = rt.block_on(nvim.shutdown()) {
            tracing::warn!(%shutdown_err, "could not stop nvim after interrupted ui attach");
        }
        return Err(err.context("failed to attach the ui"));
    }
    // ポートは実機での調査に要る（`nvim --server 127.0.0.1:PORT --remote-send` で
    // 中の nvim を直接叩ける）。info で出しておく。
    tracing::info!(port = nvim.port(), cols, rows, "nvim is up and attached");
    Ok(Pair {
        nvim,
        host_rx: handles.host,
        redraw_rx: handles.redraw,
    })
}

/// コントローラスレッドを起こす。
pub fn start(boot: Boot) -> anyhow::Result<std::thread::JoinHandle<()>> {
    let Boot {
        bundle,
        rt,
        tx,
        rx,
        uia,
        proxy,
        policy,
        generation,
    } = boot;
    let mut controller = Controller {
        bundle,
        rt,
        tx,
        uia,
        proxy,
        session: Session::default(),
        nvim: None,
        target_hwnd: 0,
        policy,
        generation,
        recovery_pending: false,
        ever_started: false,
    };
    std::thread::Builder::new()
        .name("controller".into())
        .spawn(move || controller.run(rx))
        .context("failed to start the controller thread")
}

struct Controller {
    bundle: Bundle,
    rt: Handle,
    tx: Sender<Cmd>,
    uia: Uia,
    proxy: EventLoopProxy<UserEvent>,
    session: Session,
    nvim: Option<NvimServer>,
    /// 書き戻し先ウィンドウ（DESIGN 6.2 手順 2 / 11）。
    target_hwnd: isize,
    policy: Arc<SpawnPolicy>,
    /// 現行ペアの世代。旧ペアのイベントを捨てるために使う。
    generation: Arc<AtomicU64>,
    /// Retained until a replacement is attached. A cancellation command can be
    /// queued before recovery notices the pause, so a one-shot resume is unsafe.
    recovery_pending: bool,
    ever_started: bool,
}

impl Controller {
    fn run(&mut self, rx: Receiver<Cmd>) {
        loop {
            if self.policy.is_exiting() {
                self.teardown();
                self.notify_gui(UserEvent::Quit);
                return;
            }
            if self.recovery_pending && self.policy.check().is_ok() {
                match self.restart_pair() {
                    Ok(()) => self.recovery_pending = false,
                    Err(err) if err.is::<SpawnCancelled>() => {
                        tracing::debug!("pair restart paused by shutdown");
                    }
                    Err(err) => {
                        self.recovery_pending = false;
                        if !self.ever_started {
                            self.policy.exit();
                            self.notify_gui(UserEvent::Fatal(err));
                            continue;
                        }
                        tracing::error!(%err, "pair restart failed; the host can no longer edit");
                    }
                }
            }
            // SM_SHUTTINGDOWN may precede our query notification, and may clear
            // without a cancellation addressed to this HWND. Poll only while a
            // recovery is pending; the idle resident still sleeps indefinitely.
            let cmd = if self.recovery_pending {
                match rx.recv_timeout(Duration::from_millis(100)) {
                    Ok(cmd) => cmd,
                    Err(RecvTimeoutError::Timeout) => continue,
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            } else {
                match rx.recv() {
                    Ok(cmd) => cmd,
                    Err(_) => break,
                }
            };
            if self.policy.is_exiting() {
                continue;
            }
            match cmd {
                Cmd::Start => self.recover("initial startup"),
                Cmd::LifecycleChanged => {}
                Cmd::Hotkey => self.on_hotkey(),
                Cmd::Host { generation, event } => self.on_host(generation, event),
                Cmd::Input(keys) => self.on_input(&keys),
                Cmd::Resize { cols, rows } => self.on_resize(cols, rows),
                Cmd::CloseRequested => self.on_close_requested(),
                Cmd::Exit => self.policy.exit(),
            }
        }
        tracing::error!("command channel closed; tearing down the pair");
        self.policy.exit();
        self.teardown();
        self.notify_gui(UserEvent::Quit);
    }

    /// DESIGN 付録 B ホットキーハンドラ。
    fn on_hotkey(&mut self) {
        if self.nvim.is_none() || self.recovery_pending || self.policy.is_exiting() {
            tracing::debug!("hotkey ignored while nvim is unavailable");
            return;
        }
        match self.session.phase() {
            // 既存セッションへ戻すだけ（DESIGN 6.1）。
            Phase::Editing => self.notify_gui(UserEvent::Focus),
            Phase::Idle => {
                if let Err(err) = self.begin_session() {
                    tracing::error!(%err, "cannot start a session");
                }
            }
            // Capturing / Applying は一瞬で通過する遷移状態。取りこぼして構わない。
            phase => tracing::debug!(?phase, "hotkey ignored"),
        }
    }

    fn begin_session(&mut self) -> anyhow::Result<()> {
        let foreground = focus::foreground_window();
        if !self.session.begin_capture() {
            tracing::debug!("capture already in flight");
            return Ok(());
        }

        let captured = match self.uia.capture() {
            Ok(Some(captured)) => captured,
            // 編集対象が無ければ何もせず Idle へ戻る。通知も不要（DESIGN 8.3）。
            Ok(None) => {
                tracing::debug!(
                    foreground = format_args!("{foreground:#x}"),
                    "no editable target"
                );
                self.session.abort_capture();
                return Ok(());
            }
            Err(err) => {
                self.session.abort_capture();
                return Err(err.context("capture failed"));
            }
        };
        tracing::debug!(
            foreground = format_args!("{foreground:#x}"),
            target = format_args!("{:#x}", captured.hwnd),
            lines = captured.lines.len(),
            "captured"
        );

        // filetype は指定しない。見た目とオプションはローカル設定の領分（DESIGN 5.4）。
        let started = match self.nvim.as_ref() {
            Some(nvim) => self.rpc(nvim.start_session(&captured.lines, None)),
            None => Err(anyhow!("nvim is unavailable")),
        };
        if let Err(err) = started {
            self.session.abort_capture();
            return Err(err.context("start_session failed"));
        }

        self.target_hwnd = captured.hwnd;
        self.session.begin_edit(captured.lines);

        if let Err(err) = self.proxy.send_event(UserEvent::Show {
            target: self.target_hwnd,
        }) {
            // 画面が出ないなら編集できない。掴んだセッションを畳んで Idle へ戻す。
            self.session.reset();
            return Err(anyhow!("the gui event loop is gone: {err}"));
        }
        Ok(())
    }

    /// DESIGN 付録 B 通知ハンドラ。
    fn on_host(&mut self, generation: u64, event: HostEvent) {
        // enqueue 前のチェック（`forward`）を通過した後に `restart_pair` が世代を
        // 進めていると、旧ペアのイベントがここまで届く。旧 `SessionEnd` が新しい
        // セッションを終了・書き戻しするとデータ破壊なので、ここで捨てる。
        if generation != self.generation.load(Ordering::SeqCst) {
            tracing::debug!(
                ?event,
                generation,
                "host event from a superseded pair dropped"
            );
            return;
        }
        match event {
            // 保持するだけ。書き戻しはセッション終了時（DESIGN 4.4）。
            HostEvent::SessionWrite(lines) => self.session.on_write(lines),
            HostEvent::SessionEnd => self.finish_session(),
            // 「設定が効かない」はここを見れば終わる（DESIGN 5.4）。
            HostEvent::ConfigResolved { dir, loaded } => {
                tracing::info!(dir, loaded, "local config");
            }
            // ローカル設定の読み込み失敗。起動は続行済み（DESIGN 5.4）。
            HostEvent::InitError { kind, message } => {
                tracing::warn!(kind, message, "user config error");
            }
            // 早期ヒント。正はこの下の Disconnected（DESIGN 6.3）。
            HostEvent::NvimDying => self.recover("nvim reported VimLeavePre"),
            HostEvent::Disconnected => self.recover("nvim rpc disconnected"),
        }
    }

    /// GUI から届いたキー入力をそのまま nvim へ。
    fn on_input(&mut self, keys: &str) {
        if self.recovery_pending || self.policy.is_exiting() {
            return;
        }
        if let Some(nvim) = self.nvim.as_ref()
            && let Err(err) = self.rpc(nvim.input(keys))
        {
            tracing::error!(%err, keys, "nvim_input failed");
        }
    }

    fn on_resize(&mut self, cols: u16, rows: u16) {
        if self.recovery_pending || self.policy.is_exiting() {
            return;
        }
        if let Some(nvim) = self.nvim.as_ref()
            && let Err(err) = self.rpc(nvim.try_resize(cols, rows))
        {
            tracing::error!(%err, cols, rows, "nvim_ui_try_resize failed");
        }
    }

    /// ウィンドウの ×。破棄の意味論は `ZQ` と同じで、書き戻しは起きない。
    fn on_close_requested(&mut self) {
        let phase = self.session.phase();
        if phase != Phase::Editing {
            tracing::debug!(?phase, "close requested outside of a session");
            return;
        }
        if let Some(nvim) = self.nvim.as_ref()
            && let Err(err) = self.rpc(nvim.quit_session())
        {
            tracing::error!(%err, "AnviQuit failed");
        }
    }

    fn finish_session(&mut self) {
        let phase = self.session.phase();
        if phase != Phase::Editing {
            tracing::warn!(?phase, "session_end outside of a session");
            return;
        }

        self.notify_gui(UserEvent::Hide);
        let restored = focus::set_foreground(self.target_hwnd);
        let applied = self.session.on_end();
        if let Err(err) = restored {
            // フォーカスが戻らなくても書き戻しは試す。UIA `SetValue` 経路は
            // フォーカスに依存せず（DESIGN 9.1）、諦めると編集内容が消えるだけ。
            tracing::error!(%err, "focus restore failed; attempting write-back anyway");
        }

        match applied {
            Applied::WriteBack(lines) => {
                if let Err(err) = self.uia.write_back(&lines) {
                    tracing::error!(%err, "write-back failed");
                }
            }
            // 相手アプリの undo 履歴を無駄に汚さない（DESIGN 9.4）。
            Applied::Unchanged => tracing::debug!("content unchanged; write-back skipped"),
            Applied::Discarded => tracing::debug!("never written; discarded"),
        }
    }

    /// 安全網（DESIGN 6.3）。ペアを再起動して Idle へ戻す。
    fn recover(&mut self, why: &str) {
        if self.policy.is_exiting() {
            return;
        }
        tracing::warn!(why, "nvim recovery pending");
        self.recovery_pending = true;
    }

    fn restart_pair(&mut self) -> anyhow::Result<()> {
        self.policy.check()?;
        // 世代を先に進める。`NvimServer::shutdown()` は切断監視を止めるが、io ループが
        // それより先に畳まれていれば `Disconnected` は既に積まれている。旧ペアの転送
        // タスクをここで黙らせないと、再起動が無限に連鎖する。
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;

        if let Some(mut nvim) = self.nvim.take()
            && let Err(err) = self.rt.block_on(nvim.shutdown())
        {
            tracing::warn!(%err, "could not shut down the old nvim");
        }
        self.target_hwnd = 0;
        self.session.reset();
        self.notify_gui(UserEvent::Hide);
        // Shutdown awaited above. A query or exit may have arrived meanwhile.
        self.policy.check()?;

        // `spawn_pair` が `ui_attach` までやり直す。GUI は新しいグリッドを
        // 最初の `grid_resize` で受け取る。
        let pair = spawn_pair(&self.bundle, &self.rt, &self.policy)?;
        forward(
            &self.rt,
            pair.host_rx,
            self.tx.clone(),
            generation,
            Arc::clone(&self.generation),
        );
        pump_redraw(
            &self.rt,
            pair.redraw_rx,
            self.proxy.clone(),
            generation,
            Arc::clone(&self.generation),
        );
        self.nvim = Some(pair.nvim);
        self.ever_started = true;
        tracing::info!(generation, "pair restarted");
        Ok(())
    }

    /// Permanent application exit was recorded before this bounded teardown.
    fn teardown(&mut self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
        if let Some(mut nvim) = self.nvim.take()
            && let Err(err) = self.rt.block_on(nvim.shutdown())
        {
            tracing::warn!(%err, "could not shut down nvim during shutdown");
        }
        tracing::info!("pair torn down");
    }

    fn rpc<T>(&self, operation: impl Future<Output = anyhow::Result<T>>) -> anyhow::Result<T> {
        self.rt.block_on(async {
            tokio::select! {
                biased;
                () = self.policy.wait_for_exit() => Err(anyhow!("application is exiting")),
                result = operation => result,
            }
        })
    }

    fn notify_gui(&self, event: UserEvent) {
        if let Err(err) = self.proxy.send_event(event)
            && !self.policy.is_exiting()
        {
            tracing::error!(%err, "the gui event loop is gone; event dropped");
        }
    }
}

/// `NvimServer` のイベント列を `Cmd::Host` へ流す（tokio ランタイム上のタスク）。
///
/// 世代が進んでいたら旧ペアのタスクなので自終する。ただしこのチェックは enqueue 前に
/// しか走らず、通過後に世代が進むレースは防げない。正はメッセージに載せた世代を
/// 受信側（[`Controller::on_host`]）で照合すること。ここでの自終は掃除にすぎない。
fn forward(
    rt: &Handle,
    mut host_rx: UnboundedReceiver<HostEvent>,
    tx: Sender<Cmd>,
    generation: u64,
    current: Arc<AtomicU64>,
) {
    rt.spawn(async move {
        while let Some(event) = host_rx.recv().await {
            if current.load(Ordering::SeqCst) != generation {
                tracing::debug!(?event, generation, "event from a superseded pair dropped");
                return;
            }
            if tx.send(Cmd::Host { generation, event }).is_err() {
                return;
            }
        }
    });
}

/// `redraw` バッチを GUI スレッドへ流す（tokio ランタイム上のタスク）。
///
/// パースはしない。UI 状態への適用は winit のループの中でやる（`redraw` は毎打鍵
/// 飛んでくるので、RPC の io タスクを重くしない）。世代の扱いは [`forward`] と同じ:
/// 自終は enqueue 前チェック、正の判定はメッセージの世代を GUI 側受信点で照合する。
fn pump_redraw(
    rt: &Handle,
    mut redraw_rx: UnboundedReceiver<Vec<rmpv::Value>>,
    proxy: EventLoopProxy<UserEvent>,
    generation: u64,
    current: Arc<AtomicU64>,
) {
    rt.spawn(async move {
        while let Some(batch) = redraw_rx.recv().await {
            if current.load(Ordering::SeqCst) != generation {
                tracing::debug!(generation, "redraw from a superseded pair dropped");
                return;
            }
            if proxy
                .send_event(UserEvent::Redraw { generation, batch })
                .is_err()
            {
                tracing::debug!("the gui event loop is gone; redraw pump stopped");
                return;
            }
        }
    });
}
