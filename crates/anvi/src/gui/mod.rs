//! 自前の nvim UI（DESIGN v2）。
//!
//! winit がウィンドウ・キーボード・IME を、Direct2D/DirectWrite が描画を担う。
//! nvim を触るのは controller スレッドだけなので、ここからは [`crate::controller::Cmd`]
//! を送るだけで RPC は持たない。
//!
//! このモジュールが main スレッドを占有する。トレイとグローバルホットキーの隠し
//! ウィンドウも同じスレッドに属し、winit のループがそのメッセージを汲む。

pub mod font;
pub mod fontset;
pub mod ime;
pub mod keys;
pub mod render;
pub mod window;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use anvi_core::SpawnPolicy;
use anvi_core::ui::input::{Mods, encode_key, encode_text};
use anvi_core::ui::{UiState, redraw};
use anyhow::Context as _;
use winit::application::ApplicationHandler;
use winit::dpi::{PhysicalPosition, PhysicalSize};
use winit::event::{ElementState, Ime, KeyEvent, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::window::{Window, WindowId};

use crate::controller::Cmd;
use crate::gui::font::{FontSpec, GuiFont};
use crate::gui::ime::ImeState;
use crate::gui::render::Renderer;
use crate::hotkey::Hotkeys;
use crate::shutdown::SessionEndHook;
use crate::tray::Tray;

/// 既定のグリッド。ウィンドウの初期サイズも `nvim_ui_attach` もこれで揃える。
pub const DEFAULT_GRID: (u16, u16) = (90, 28);

/// レンダーターゲットを作り直しても描画が通らないときに諦める回数。
///
/// `D2DERR_RECREATE_TARGET`（ドライバのリセットなど）は作り直せば直るが、直らない
/// 状態で作り直しと再描画を往復すると CPU を焼き続ける。数回で見切る。
const MAX_DRAW_FAILURES: u32 = 3;

/// 未確定文字列（IME の変換中の文字列）。
///
/// この UI を自前で持つ理由そのもの。ここを描かないと日本語が打てない。
/// `target` は変換対象クラスタのバイト範囲で、winit が `GCS_COMPATTR` から解いてくれる。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Preedit {
    pub text: String,
    pub target: Option<(usize, usize)>,
}

impl Preedit {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }
}

/// winit のユーザーイベント。controller / トレイ / ホットキー / RPC 転送から届く。
#[derive(Debug)]
pub enum UserEvent {
    /// nvim からの `redraw` バッチ（params をそのまま）。`generation` は送信元
    /// ペアの世代。キュー滞留中にペアが再起動され得るので、受信側でも現世代と
    /// 照合して旧ペア分を捨てる（旧 redraw が新セッションの画面を汚さないように）。
    Redraw {
        generation: u64,
        batch: Vec<rmpv::Value>,
    },
    Hotkey,
    /// トレイの自動起動チェックが切り替わった。実際のレジストリ操作は
    /// メニュー項目を持っている [`Tray`] 側で行う（ハンドラは `Send` 制約で
    /// 項目を掴めない）。
    ToggleAutostart,
    /// 編集ウィンドウを出す。`target` は編集対象の入力欄の HWND で、ウィンドウは
    /// それと同じモニタの、対象ウィンドウに重なる位置へ出す（DESIGN 7.2）。
    /// HWND は `!Send` なのでスレッドを跨ぐ経路では `isize` で運ぶ。
    Show {
        target: isize,
    },
    Hide,
    Focus,
    Quit,
    /// Initial child startup failed; preserve the error as the process result.
    Fatal(anyhow::Error),
}

/// 他スレッド・他クレートのコールバックから [`UserEvent`] を投げるための握り。
///
/// `EventLoopProxy` は `Send` だが `!Sync`（内部に HWND を持つ）。`tray-icon` と
/// `global-hotkey` のイベントハンドラは `Fn + Send + Sync` を要求するので、
/// `Mutex` を噛ませる。ハンドラはどれもイベントループのスレッドから呼ばれるため
/// 実際には競合しない。
pub struct ProxyHandle(Mutex<EventLoopProxy<UserEvent>>);

impl ProxyHandle {
    #[must_use]
    pub fn new(proxy: EventLoopProxy<UserEvent>) -> Self {
        Self(Mutex::new(proxy))
    }

    /// 投げる。ループが畳まれていれば記録して捨てる（終了処理の最中に起きうる）。
    pub fn send(&self, event: UserEvent) {
        let Ok(proxy) = self.0.lock() else {
            tracing::error!("the event loop proxy is poisoned");
            return;
        };
        if let Err(err) = proxy.send_event(event) {
            tracing::error!(%err, "the gui event loop is gone; event dropped");
        }
    }
}

/// GUI に渡す一切。トレイとホットキーは登録したスレッドでループが回っている必要が
/// あるので、main スレッドで作ってからここへ預ける。
pub struct GuiBoot {
    pub tx: Sender<Cmd>,
    pub tray: Tray,
    pub hotkeys: Hotkeys,
    pub policy: Arc<SpawnPolicy>,
    /// 現行ペアの世代。controller の `restart_pair` が進める。旧ペアの `Redraw` を
    /// 捨てる判定に使う。
    pub generation: Arc<AtomicU64>,
}

/// main スレッドを占有する。ループが終わったら戻る。
pub fn run(event_loop: EventLoop<UserEvent>, boot: GuiBoot) -> anyhow::Result<()> {
    // 常駐アプリなので待機中は 1 フレームも回さない。描画は `flush` と
    // `RedrawRequested` が来たときだけ。
    event_loop.set_control_flow(ControlFlow::Wait);

    let mut app = App {
        tx: boot.tx,
        tray: boot.tray,
        _hotkeys: boot.hotkeys,
        surface: None,
        ui: UiState::default(),
        font: FontSpec::default(),
        ime: ImeState::default(),
        mods: Mods::default(),
        generation: boot.generation,
        policy: boot.policy,
        grid: DEFAULT_GRID,
        draw_failures: 0,
        fatal: None,
    };
    let result = event_loop.run_app(&mut app);
    app.policy.exit();
    app.send(Cmd::Exit);

    // ハンドラは `Result` を返せないので、続行できない失敗は `fatal` に積んで
    // `exit()` している。ループ自体のエラーより、そちらが本当の原因。
    if let Some(err) = app.fatal.take() {
        return Err(err);
    }
    result.context("the winit event loop failed")
}

/// ウィンドウと、その上のレンダーターゲット。生成は `resumed` まで待つ。
struct Surface {
    // Fields drop in declaration order: detach the subclass before HWND teardown.
    _session_end: SessionEndHook,
    window: Window,
    renderer: Renderer,
    /// `focus::set_foreground` に渡す生の HWND。
    hwnd: isize,
}

struct App {
    tx: Sender<Cmd>,
    /// 生かしておく必要がある（drop するとアイコンが消える）。自動起動の
    /// チェック状態を持っているので、`ToggleAutostart` でも触る。
    tray: Tray,
    _hotkeys: Hotkeys,
    surface: Option<Surface>,
    ui: UiState,
    font: FontSpec,
    ime: ImeState,
    /// 直近の `ModifiersChanged` が伝えてきた修飾キー。
    mods: Mods,
    /// 直近に nvim へ伝えたグリッド。同じ値を送り返さないため。
    grid: (u16, u16),
    /// 現行ペアの世代。[`UserEvent::Redraw`] の世代照合に使う。
    generation: Arc<AtomicU64>,
    policy: Arc<SpawnPolicy>,
    draw_failures: u32,
    /// ループを畳む原因になった致命的エラー。[`run`] の戻り値になる。
    fatal: Option<anyhow::Error>,
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        // Windows では起動時に 1 度だけ届く。再入は無視する（多重生成しない）。
        if self.surface.is_some() {
            return;
        }
        match self.create_surface(event_loop) {
            Ok(surface) => {
                self.surface = Some(surface);
                // No initial CreateProcess before synchronous notifications can
                // reach the real HWND procedure.
                self.send(Cmd::Start);
            }
            Err(err) => self.die(event_loop, err),
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::Redraw { generation, batch } => {
                // 転送タスク側のチェックは enqueue 前にしか走らない。ここが正の判定。
                if generation != self.generation.load(Ordering::SeqCst) {
                    tracing::debug!(generation, "redraw from a superseded pair dropped");
                    return;
                }
                self.on_redraw(&batch);
            }
            UserEvent::Show { target } => self.on_show(target),
            UserEvent::Hide => match self.surface.as_ref() {
                Some(surface) => window::hide(&surface.window),
                None => tracing::debug!("hide requested before the window exists"),
            },
            UserEvent::Focus => self.on_focus_request(),
            UserEvent::Hotkey => self.send(Cmd::Hotkey),
            UserEvent::ToggleAutostart => self.tray.apply_autostart(),
            UserEvent::Quit => {
                tracing::info!("exit requested");
                self.policy.exit();
                self.send(Cmd::Exit);
                event_loop.exit();
            }
            UserEvent::Fatal(err) => self.die(event_loop, err),
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_id: WindowId,
        event: WindowEvent,
    ) {
        match self.surface.as_ref() {
            Some(surface) if surface.window.id() == window_id => {}
            Some(_) => {
                tracing::debug!(?window_id, "event for an unknown window dropped");
                return;
            }
            None => return,
        }

        match event {
            // 閉じるのは nvim の仕事（`AnviQuit` = `ZQ`）。ここで exit しない。
            WindowEvent::CloseRequested => self.send(Cmd::CloseRequested),
            WindowEvent::Resized(size) => self.on_resized(event_loop, size),
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                if let Err(err) = self.rescale(scale_factor) {
                    self.die(event_loop, err);
                }
            }
            WindowEvent::RedrawRequested => self.on_draw(event_loop),
            WindowEvent::Ime(ime) => self.on_ime(ime),
            WindowEvent::ModifiersChanged(modifiers) => self.mods = keys::mods(modifiers.state()),
            WindowEvent::KeyboardInput {
                event,
                is_synthetic,
                ..
            } => self.on_key(&event, is_synthetic),
            WindowEvent::Focused(false) => self.on_focus_lost(),
            _ => {}
        }
    }
}

impl App {
    /// 続行できない失敗。原因を抱えたままループを畳む。
    fn die(&mut self, event_loop: &ActiveEventLoop, err: anyhow::Error) {
        tracing::error!(%err, "the gui cannot continue");
        self.fatal = Some(err);
        self.policy.exit();
        self.send(Cmd::Exit);
        event_loop.exit();
    }

    fn send(&self, cmd: Cmd) {
        if let Err(err) = self.tx.send(cmd)
            && !self.policy.is_exiting()
        {
            tracing::error!(cmd = ?err.0, "the controller is gone; command dropped");
        }
    }

    fn request_redraw(&self) {
        if let Some(surface) = self.surface.as_ref() {
            surface.window.request_redraw();
        }
    }

    fn create_surface(&mut self, event_loop: &ActiveEventLoop) -> anyhow::Result<Surface> {
        let window = window::create(event_loop, self.grid)?;
        let hwnd = window::hwnd_of(&window)?;
        let session_end = SessionEndHook::install(hwnd, Arc::clone(&self.policy), self.tx.clone())?;
        let renderer = Renderer::new(hwnd, &self.font, window.scale_factor())
            .context("failed to create the Direct2D renderer")?;
        // 暫定サイズで作ったウィンドウを、実測したセル寸法へ合わせ直す。
        let metrics = renderer.metrics();
        window::resize_to_grid(
            &window,
            (metrics.width, metrics.height),
            self.grid,
            renderer.padding(),
        );
        window.set_ime_allowed(self.ui.mode.accepts_text_input());
        Ok(Surface {
            _session_end: session_end,
            window,
            renderer,
            hwnd,
        })
    }

    /// nvim の `redraw` バッチ 1 回分。
    fn on_redraw(&mut self, batch: &[rmpv::Value]) {
        let outcome = match redraw::apply(&mut self.ui, batch) {
            Ok(outcome) => outcome,
            // 既知イベントの引数が壊れている。UI 状態は中途半端になるが、書き戻し
            // 経路は nvim 側のイベントで動くので host ごと落とすほどではない。
            Err(err) => {
                tracing::error!(%err, "redraw batch rejected");
                return;
            }
        };

        if outcome.font_changed {
            self.apply_guifont();
        }
        let Some(surface) = self.surface.as_ref() else {
            tracing::debug!("redraw arrived before the window exists");
            return;
        };
        if outcome.mode_changed {
            surface
                .window
                .set_ime_allowed(self.ui.mode.accepts_text_input());
        }
        if outcome.title_changed {
            surface.window.set_title(&self.ui.title);
        }
        if outcome.flushed {
            surface.window.request_redraw();
        }
    }

    /// `option_set guifont` の反映。
    ///
    /// **解けない指定を既定へ黙って戻さない。** 一方「GUI に任せる」指定
    /// （空文字列、およびサイズを含まない候補列 = nvim の組み込み既定値）は
    /// こちらの既定フォントを使う。→ [`FontSpec::parse`]
    fn apply_guifont(&mut self) {
        let font = match self.ui.guifont.as_deref().map(FontSpec::parse) {
            None | Some(GuiFont::Unspecified) => FontSpec::default(),
            Some(GuiFont::Spec(font)) => font,
            Some(GuiFont::Invalid) => {
                tracing::warn!(
                    guifont = self.ui.guifont.as_deref(),
                    "unparsable guifont; keeping the current font"
                );
                return;
            }
        };
        if font == self.font {
            return;
        }

        let grid = {
            let Some(surface) = self.surface.as_mut() else {
                // ウィンドウより先に来ることはないが、来たら次の生成で拾われる。
                self.font = font;
                return;
            };
            let scale = surface.window.scale_factor();
            if let Err(err) = surface.renderer.set_font(&font, scale) {
                tracing::error!(%err, families = ?font.families, "cannot switch the font");
                return;
            }
            let metrics = surface.renderer.metrics();
            let pad = surface.renderer.padding();
            surface.window.request_redraw();
            window::grid_for(
                surface.window.inner_size(),
                (metrics.width, metrics.height),
                pad,
            )
        };
        tracing::info!(families = ?font.families, size_pt = font.size_pt, "font changed");
        self.font = font;
        self.set_grid(grid);
    }

    /// nvim に伝えるグリッドを更新する。変わっていなければ何もしない。
    fn set_grid(&mut self, grid: (u16, u16)) {
        if grid == self.grid {
            return;
        }
        self.grid = grid;
        self.send(Cmd::Resize {
            cols: grid.0,
            rows: grid.1,
        });
    }

    fn on_show(&mut self, target: isize) {
        let Some(surface) = self.surface.as_ref() else {
            tracing::error!("show requested before the window exists");
            return;
        };
        if let Err(err) = window::show(&surface.window, surface.hwnd, target) {
            tracing::error!(%err, "cannot bring the editor window to the front");
        }
    }

    fn on_focus_request(&mut self) {
        let Some(surface) = self.surface.as_ref() else {
            tracing::error!("focus requested before the window exists");
            return;
        };
        if let Err(err) = crate::focus::set_foreground(surface.hwnd) {
            tracing::error!(%err, "cannot focus the editor window");
        }
    }

    fn on_resized(&mut self, event_loop: &ActiveEventLoop, size: PhysicalSize<u32>) {
        // 最小化は 0x0 で届く。0 でレンダーターゲットを張り直すと壊れる。
        if size.width == 0 || size.height == 0 {
            tracing::debug!(
                width = size.width,
                height = size.height,
                "empty resize ignored"
            );
            return;
        }
        match self.resize_surface(size) {
            Ok(Some(grid)) => self.set_grid(grid),
            Ok(None) => {}
            Err(err) => self.die(event_loop, err),
        }
    }

    fn resize_surface(&mut self, size: PhysicalSize<u32>) -> anyhow::Result<Option<(u16, u16)>> {
        let Some(surface) = self.surface.as_mut() else {
            return Ok(None);
        };
        surface
            .renderer
            .resize(size.width, size.height)
            .context("cannot resize the render target")?;
        let metrics = surface.renderer.metrics();
        Ok(Some(window::grid_for(
            size,
            (metrics.width, metrics.height),
            surface.renderer.padding(),
        )))
    }

    /// DPI が変わった。セル寸法が変わるのでフォントを作り直して行列数を数え直す。
    fn rescale(&mut self, scale: f64) -> anyhow::Result<()> {
        let grid = {
            let Some(surface) = self.surface.as_mut() else {
                return Ok(());
            };
            surface
                .renderer
                .set_font(&self.font, scale)
                .context("cannot rebuild the font for the new dpi")?;
            let metrics = surface.renderer.metrics();
            let pad = surface.renderer.padding();
            surface.window.request_redraw();
            window::grid_for(
                surface.window.inner_size(),
                (metrics.width, metrics.height),
                pad,
            )
        };
        self.set_grid(grid);
        Ok(())
    }

    fn on_draw(&mut self, event_loop: &ActiveEventLoop) {
        let result = {
            let Some(surface) = self.surface.as_mut() else {
                return;
            };
            surface.renderer.draw(&self.ui, self.ime.preedit())
        };
        let Err(err) = result else {
            self.draw_failures = 0;
            return;
        };

        self.draw_failures += 1;
        tracing::error!(%err, attempt = self.draw_failures, "draw failed");
        if self.draw_failures >= MAX_DRAW_FAILURES {
            self.die(event_loop, err.context("the renderer keeps failing"));
            return;
        }
        // `D2DERR_RECREATE_TARGET` はドライバのリセット等で普通に起きる。
        // 作り直して次のフレームで描き直す。
        if let Err(err) = self.rebuild_renderer() {
            self.die(event_loop, err);
        }
    }

    /// レンダーターゲットを作り直す。
    ///
    /// 渡すのは **いま保持しているフォントと現在の `scale_factor`**。既定値を渡すと
    /// `option_set guifont` で反映済みのフォントが作り直しのたびに黙って戻る。
    /// `Renderer::new` は自分で `GetClientRect` するので `resize` は要らないが、
    /// DPI が変わったあとに作り直すと新しいセル寸法になるので、行列数は数え直す。
    fn rebuild_renderer(&mut self) -> anyhow::Result<()> {
        let grid = {
            let Some(surface) = self.surface.as_mut() else {
                return Ok(());
            };
            let scale = surface.window.scale_factor();
            surface.renderer = Renderer::new(surface.hwnd, &self.font, scale)
                .context("cannot recreate the render target")?;
            let metrics = surface.renderer.metrics();
            let pad = surface.renderer.padding();
            surface.window.request_redraw();
            window::grid_for(
                surface.window.inner_size(),
                (metrics.width, metrics.height),
                pad,
            )
        };
        self.set_grid(grid);
        Ok(())
    }

    fn on_ime(&mut self, event: Ime) {
        match event {
            // Windows では composition の開始（`WM_IME_STARTCOMPOSITION`）。
            Ime::Enabled => self.ime.begin(),
            Ime::Preedit(text, target) => {
                self.ime.set_preedit(text, target);
                self.place_ime_cursor();
                self.request_redraw();
            }
            Ime::Commit(text) => {
                self.ime.commit();
                // `nvim_paste` は使わない。モードに依らず一貫させるため（→ 計画 §4.2）。
                self.send(Cmd::Input(encode_text(&text)));
                self.request_redraw();
            }
            // `WM_IME_ENDCOMPOSITION`。未確定はもう無い。
            Ime::Disabled => {
                self.ime.clear();
                self.request_redraw();
            }
        }
    }

    /// 候補ウィンドウをカーソルの下へ寄せる。IME 自身の未確定描画は winit が
    /// 抑止済み（`ISC_SHOWUICOMPOSITIONWINDOW` を落としている）なので、ここで
    /// 伝えるのは候補一覧の位置だけ。
    fn place_ime_cursor(&self) {
        let Some(surface) = self.surface.as_ref() else {
            return;
        };
        let metrics = surface.renderer.metrics();
        let width = f64::from(metrics.width);
        let height = f64::from(metrics.height);
        // グリッドは余白のぶんずらして描いている。候補ウィンドウも同じだけずらす。
        let pad = f64::from(surface.renderer.padding());
        let x = self.ui.cursor.col as f64 * width + pad;
        let y = self.ui.cursor.row as f64 * height + pad;
        surface.window.set_ime_cursor_area(
            PhysicalPosition::new(x, y),
            PhysicalSize::new(width, height),
        );
    }

    fn on_key(&mut self, event: &KeyEvent, is_synthetic: bool) {
        if event.state != ElementState::Pressed {
            return;
        }
        // フォーカスの出入りで winit が合成するイベント。実際の打鍵ではない。
        if is_synthetic {
            return;
        }
        // IME が食っているキーを二重に流さない。
        if self.ime.composing() {
            tracing::trace!("key dropped while composing");
            return;
        }
        let Some(key) = keys::convert(&event.logical_key) else {
            tracing::debug!(key = ?event.logical_key, "unmapped key dropped");
            return;
        };
        let Some(notation) = encode_key(key, self.mods) else {
            tracing::debug!(?key, "key has no nvim notation");
            return;
        };
        self.send(Cmd::Input(notation));
    }

    fn on_focus_lost(&mut self) {
        // 宙に浮いた composition を残さない。
        self.ime.clear();
        // 修飾キーの離鍵はフォーカスの外で起きる。持ち越すと次の打鍵が化ける。
        self.mods = Mods::default();
        self.request_redraw();
    }
}
